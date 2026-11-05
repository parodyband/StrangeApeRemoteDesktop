//! `sard connect`: window, input capture, decode + present.
//!
//! Threads: the winit event loop (main thread, input), a render thread that
//! owns the platform `VideoSink` and decodes/presents frames as soon as they
//! are reassembled, a forwarder for network events, and the network thread
//! inside `sard_core::net`.

use crate::args::Args;
use crate::keymap;
use crate::{Result, platform};
use sard_core::clock;
use sard_core::config::{self, KnownHosts};
use sard_core::media::{VideoSink, letterbox};
use sard_core::net::{Client, Connection, Event};
use sard_core::proto::{AudioPacket, ClientHello, Codec, Control, Platform, StreamConfig, button, client_flags};
use sard_core::stats::Samples;
use sard_core::video::{CompleteFrame, FrameMeta, frame_flags};
use sard_core::{error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{CursorGrabMode, CustomCursor, Fullscreen, Window, WindowId};

const STREAM_CURSOR_METADATA: u32 = crate::host::STREAM_CURSOR_METADATA;

#[derive(Debug)]
enum UserEvent {
    Control(Control),
    CursorPos { visible: bool },
    Disconnected(String),
    Stats(String),
}

pub fn run(args: &Args) -> Result<()> {
    let Some(target) = args.positional(0) else {
        return Err("usage: sard connect <host>".into());
    };
    let dir = config::config_dir();
    let identity = config::load_or_create_identity(&dir)?;
    let known = KnownHosts::load(&dir)?;
    let host = known
        .find(target)
        .ok_or_else(|| {
            format!("not paired with {target}; run `sard host --pair` there, then `sard pair {target} <code>` here")
        })?
        .clone();
    // Prefer an explicit address typed by the user (may have changed since pairing).
    let addr = if target.parse::<std::net::IpAddr>().is_ok() || target.contains(':') || target.contains('.') {
        target.to_string()
    } else {
        host.addr.clone()
    };
    let cfg = crate::net_config(args)?;
    println!("Connecting to {} ({addr}) ...", host.name);
    let conn = Client::connect(&addr, host.key, identity, cfg, Duration::from_secs(8))?;
    let events = conn.take_events().ok_or("events taken")?;
    let frames = conn.take_video().ok_or("video taken")?;
    if args.flag("headless") {
        return run_headless(conn, events, frames, args, &host.name);
    }

    let mut builder = EventLoop::<UserEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::EventLoopBuilderExtMacOS;
        // No default menu: Cmd+Q etc. must reach the remote machine.
        builder.with_default_menu(false);
    }
    let event_loop = builder.build()?;
    let proxy = event_loop.create_proxy();

    // Forward network events to the UI thread.
    {
        let proxy = proxy.clone();
        let audio = !args.flag("no-audio");
        let c = conn.clone();
        std::thread::Builder::new()
            .name("sard-events".into())
            .spawn(move || forward_events(events, proxy, audio, c))?;
    }

    let mut app = App {
        conn,
        args_fps: args.parse_value("fps")?,
        args_bitrate: args.parse_value::<f32>("bitrate")?.map(|m| (m * 1000.0) as u32),
        args_codec: args.value("codec").and_then(Codec::parse),
        args_display: args.parse_value("display")?.unwrap_or(0),
        fullscreen: args.flag("fullscreen"),
        view_only: args.flag("view-only"),
        audio: !args.flag("no-audio"),
        swap_mods_allowed: !args.flag("no-swap-modifiers"),
        host_name: host.name.clone(),
        frames: Some(frames),
        proxy,
        window: None,
        render: None,
        stop: Arc::new(AtomicBool::new(false)),
        win_size: Arc::new(AtomicU64::new(0)),
        stream: None,
        cursor_pos: None,
        modifiers: ModifiersState::empty(),
        swap_mods: false,
        relative: false,
        cursors: HashMap::new(),
        remote_cursor_visible: true,
        exit_reason: None,
    };
    event_loop.run_app(&mut app)?;
    app.shutdown();
    if let Some(r) = app.exit_reason {
        println!("{r}");
    }
    Ok(())
}

fn forward_events(events: Receiver<Event>, proxy: EventLoopProxy<UserEvent>, audio_enabled: bool, conn: Connection) {
    let clip = crate::clipsync::ClipSync::start(conn);
    let mut audio: Option<Box<dyn sard_core::media::AudioSink>> = None;
    // Diagnostics: count audio packets without playing them (avoids feedback
    // when host and client share a machine).
    let dry = std::env::var_os("SARD_AUDIO_DRY").is_some();
    let (mut dry_n, mut dry_gaps, mut dry_last, mut dry_t) = (0u64, 0u64, None::<u32>, Instant::now());
    for ev in events {
        let ue = match ev {
            Event::Control(Control::AudioConfig { sample_rate, channels }) => {
                if audio_enabled && audio.is_none() && !dry {
                    match platform::open_audio_sink() {
                        Ok(s) => {
                            info!("audio: {sample_rate} Hz x{channels} → {}", s.describe());
                            audio = Some(s);
                        }
                        Err(e) => warn!("audio playback unavailable: {e}"),
                    }
                }
                continue;
            }
            Event::Control(Control::Clipboard { text }) => {
                if let Some(c) = &clip {
                    c.on_remote(text);
                }
                continue;
            }
            Event::Audio(bytes) => {
                if dry {
                    if let Ok(p) = AudioPacket::decode(&bytes) {
                        dry_n += 1;
                        if let Some(l) = dry_last
                            && p.seq != l.wrapping_add(1)
                        {
                            dry_gaps += 1;
                        }
                        dry_last = Some(p.seq);
                        if dry_t.elapsed() >= Duration::from_secs(2) {
                            info!(
                                "audio (dry): {:.0} packets/s ({} frames each), {dry_gaps} gaps",
                                dry_n as f64 / dry_t.elapsed().as_secs_f64(),
                                p.samples.len() / 2
                            );
                            dry_n = 0;
                            dry_t = Instant::now();
                        }
                    }
                } else if let (Some(a), Ok(p)) = (&audio, AudioPacket::decode(&bytes)) {
                    a.push(p.seq, &p.samples);
                }
                continue;
            }
            Event::Control(c) => UserEvent::Control(c),
            Event::CursorPos { visible, .. } => UserEvent::CursorPos { visible },
            Event::Disconnected(r) => {
                let _ = proxy.send_event(UserEvent::Disconnected(r));
                return;
            }
            _ => continue,
        };
        if proxy.send_event(ue).is_err() {
            return;
        }
    }
}

struct App {
    conn: Connection,
    args_fps: Option<u16>,
    args_bitrate: Option<u32>,
    args_codec: Option<Codec>,
    args_display: u8,
    fullscreen: bool,
    view_only: bool,
    audio: bool,
    swap_mods_allowed: bool,
    host_name: String,
    frames: Option<Receiver<CompleteFrame>>,
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Arc<Window>>,
    render: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    win_size: Arc<AtomicU64>,
    stream: Option<StreamConfig>,
    cursor_pos: Option<PhysicalPosition<f64>>,
    modifiers: ModifiersState,
    swap_mods: bool,
    relative: bool,
    cursors: HashMap<u32, CustomCursor>,
    remote_cursor_visible: bool,
    exit_reason: Option<String>,
}

fn pack(w: u32, h: u32) -> u64 {
    ((w as u64) << 32) | h as u64
}

impl App {
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.conn.close();
        if let Some(t) = self.render.take() {
            let _ = t.join();
        }
    }

    fn start(&mut self, el: &ActiveEventLoop) -> Result<()> {
        let attrs = Window::default_attributes()
            .with_title(format!("sard — connecting to {}…", self.host_name))
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(el.create_window(attrs)?);
        if self.fullscreen {
            window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let size = window.inner_size();
        self.win_size.store(pack(size.width, size.height), Ordering::Relaxed);
        let display = window.display_handle()?.as_raw();
        let handle = window.window_handle()?.as_raw();
        let sink = platform::create_video_sink(display, handle, size.width, size.height)?;
        info!("video output: {}", sink.describe());

        let monitor = window.current_monitor().or_else(|| el.primary_monitor());
        let (mw, mh, hz) = monitor
            .map(|m| (m.size().width, m.size().height, m.refresh_rate_millihertz().unwrap_or(60_000)))
            .unwrap_or((3840, 2160, 60_000));
        let max_fps = self.args_fps.unwrap_or(((hz + 500) / 1000) as u16).clamp(10, 240);
        let hello = ClientHello {
            platform: Platform::current(),
            decoders: platform::decoders(),
            codec_pref: self.args_codec.map_or(0, |c| c as u8),
            max_width: mw.max(1280) as u16,
            max_height: mh.max(720) as u16,
            max_fps,
            bitrate_kbps: self.args_bitrate.unwrap_or(0),
            flags: client_flags::LOCAL_CURSOR | if self.audio { client_flags::AUDIO } else { 0 },
            display: self.args_display,
        };
        info!(
            "requesting up to {}x{} @ {} fps, decoders {:#x}",
            hello.max_width, hello.max_height, max_fps, hello.decoders
        );
        self.conn.send_control(&Control::ClientHello(hello));

        let frames = self.frames.take().ok_or("render already started")?;
        let (conn, proxy, stop, win_size) =
            (self.conn.clone(), self.proxy.clone(), Arc::clone(&self.stop), Arc::clone(&self.win_size));
        let host_name = self.host_name.clone();
        self.render = Some(
            std::thread::Builder::new()
                .name("sard-render".into())
                .spawn(move || render_loop(sink, frames, conn, proxy, stop, win_size, host_name))?,
        );
        self.window = Some(window);
        Ok(())
    }

    fn video_size(&self) -> (u32, u32) {
        self.stream.as_ref().map_or((0, 0), |s| (s.width as u32, s.height as u32))
    }

    /// Window pixel position → normalized video coordinates.
    fn normalize(&self, p: PhysicalPosition<f64>) -> Option<(f32, f32)> {
        let w = self.window.as_ref()?;
        let s = w.inner_size();
        let (vw, vh) = self.video_size();
        if vw == 0 {
            return None;
        }
        let (rx, ry, rw, rh) = letterbox(s.width, s.height, vw, vh);
        let x = ((p.x as f32 - rx) / rw).clamp(0.0, 1.0);
        let y = ((p.y as f32 - ry) / rh).clamp(0.0, 1.0);
        Some((x, y))
    }

    fn hotkey(&self) -> bool {
        self.modifiers.control_key() && self.modifiers.alt_key() && self.modifiers.shift_key()
    }

    fn handle_hotkey(&mut self, el: &ActiveEventLoop, code: KeyCode) -> bool {
        let Some(w) = self.window.clone() else {
            return false;
        };
        match code {
            KeyCode::KeyQ => {
                self.exit_reason = Some("Disconnected.".into());
                el.exit();
            }
            KeyCode::KeyF => {
                let fs = w.fullscreen().is_some();
                w.set_fullscreen(if fs { None } else { Some(Fullscreen::Borderless(None)) });
            }
            KeyCode::KeyM => self.set_relative(!self.relative),
            KeyCode::KeyK => self.conn.request_keyframe(),
            _ => return false,
        }
        // The hotkey chord's modifiers are down on the host; release them.
        self.conn.send_control(&Control::ReleaseAll);
        true
    }

    fn set_relative(&mut self, on: bool) {
        let Some(w) = &self.window else { return };
        if on {
            let ok = w.set_cursor_grab(CursorGrabMode::Locked).or_else(|_| w.set_cursor_grab(CursorGrabMode::Confined));
            if let Err(e) = ok {
                warn!("pointer lock unavailable: {e}");
                return;
            }
            w.set_cursor_visible(false);
        } else {
            let _ = w.set_cursor_grab(CursorGrabMode::None);
            w.set_cursor_visible(self.remote_cursor_visible);
        }
        self.relative = on;
        info!("relative mouse mode {}", if on { "on" } else { "off" });
    }

    fn on_control(&mut self, el: &ActiveEventLoop, c: Control) {
        match c {
            Control::StreamConfig(s) => {
                info!(
                    "stream: {} {}x{} @ {} fps, {:.0} Mbps from {} ({:?})",
                    s.codec.name(),
                    s.width,
                    s.height,
                    s.fps,
                    s.bitrate_kbps as f32 / 1000.0,
                    s.host_name,
                    s.host_platform
                );
                self.swap_mods = self.swap_mods_allowed
                    && s.host_platform != Platform::Unknown
                    && s.host_platform != Platform::current();
                if let Some(w) = &self.window {
                    if self.stream.is_none() && w.fullscreen().is_none() {
                        fit_window(w, s.width as u32, s.height as u32);
                    }
                    if s.flags & STREAM_CURSOR_METADATA == 0 {
                        w.set_cursor_visible(false); // cursor is in the video
                    }
                }
                self.stream = Some(s);
            }
            Control::CursorShape { serial, hot_x, hot_y, width, height, rgba } => {
                let cursor = if let Some(c) = self.cursors.get(&serial) {
                    c.clone()
                } else {
                    match CustomCursor::from_rgba(
                        rgba,
                        width,
                        height,
                        hot_x.min(width.saturating_sub(1)),
                        hot_y.min(height.saturating_sub(1)),
                    ) {
                        Ok(src) => {
                            let c = el.create_custom_cursor(src);
                            if self.cursors.len() > 64 {
                                self.cursors.clear();
                            }
                            self.cursors.insert(serial, c.clone());
                            c
                        }
                        Err(e) => {
                            warn!("bad cursor image: {e}");
                            return;
                        }
                    }
                };
                if let Some(w) = &self.window {
                    w.set_cursor(cursor);
                }
            }
            Control::Status { text } => {
                info!("host: {text}");
                if let Some(w) = &self.window
                    && self.stream.is_none()
                {
                    w.set_title(&format!("sard — {} — {text}", self.host_name));
                }
            }
            other => sard_core::debug!("unhandled control {other:?}"),
        }
    }
}

fn fit_window(w: &Window, vw: u32, vh: u32) {
    let Some(m) = w.current_monitor() else { return };
    let ms = m.size();
    let scale = ((ms.width as f64 * 0.9) / vw as f64).min((ms.height as f64 * 0.85) / vh as f64).min(1.0);
    let _ = w.request_inner_size(PhysicalSize::new((vw as f64 * scale) as u32, (vh as f64 * scale) as u32));
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_none()
            && let Err(e) = self.start(el)
        {
            error!("cannot start: {e}");
            self.exit_reason = Some(format!("error: {e}"));
            el.exit();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.exit_reason = Some("Disconnected.".into());
                el.exit();
            }
            WindowEvent::Resized(s) => {
                self.win_size.store(pack(s.width, s.height), Ordering::Relaxed);
            }
            WindowEvent::Focused(false) => {
                self.conn.send_control(&Control::ReleaseAll);
                if self.relative {
                    self.set_relative(false);
                }
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                if is_synthetic || event.repeat {
                    return; // host compositor does its own autorepeat
                }
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let down = event.state == ElementState::Pressed;
                if down && self.hotkey() && self.handle_hotkey(el, code) {
                    return;
                }
                if self.view_only {
                    return;
                }
                if let Some(mut k) = keymap::to_evdev(code) {
                    if self.swap_mods {
                        k = keymap::swap_ctrl_meta(k);
                    }
                    self.conn.send_control(&Control::Key { code: k, down });
                }
            }
            WindowEvent::CursorMoved { .. } | WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. }
                if self.view_only => {}
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = Some(position);
                if !self.relative
                    && let Some((x, y)) = self.normalize(position)
                {
                    self.conn.send_mouse_abs(x, y);
                }
            }
            WindowEvent::MouseInput { state, button: b, .. } => {
                let button = match b {
                    MouseButton::Left => button::LEFT,
                    MouseButton::Right => button::RIGHT,
                    MouseButton::Middle => button::MIDDLE,
                    MouseButton::Back => button::BACK,
                    MouseButton::Forward => button::FORWARD,
                    MouseButton::Other(_) => return,
                };
                let (x, y) = self.cursor_pos.and_then(|p| self.normalize(p)).unwrap_or((0.5, 0.5));
                self.conn.send_control(&Control::MouseButton { button, down: state == ElementState::Pressed, x, y });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let c = match delta {
                    MouseScrollDelta::LineDelta(x, y) => Control::Scroll { dx: x, dy: y, discrete: true },
                    MouseScrollDelta::PixelDelta(p) => {
                        let scale = self.window.as_ref().map_or(1.0, |w| w.scale_factor());
                        Control::Scroll { dx: (p.x / scale) as f32, dy: (p.y / scale) as f32, discrete: false }
                    }
                };
                self.conn.send_control(&c);
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if self.relative
            && let DeviceEvent::MouseMotion { delta: (dx, dy) } = event
        {
            self.conn.send_control(&Control::MouseRel { dx: dx as f32, dy: dy as f32 });
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Control(c) => self.on_control(el, c),
            UserEvent::CursorPos { visible } => {
                if visible != self.remote_cursor_visible {
                    self.remote_cursor_visible = visible;
                    if let Some(w) = &self.window
                        && !self.relative
                    {
                        w.set_cursor_visible(visible);
                    }
                }
            }
            UserEvent::Disconnected(r) => {
                self.exit_reason = Some(format!("Disconnected: {r}"));
                el.exit();
            }
            UserEvent::Stats(s) => {
                if let Some(w) = &self.window {
                    w.set_title(&s);
                }
            }
        }
    }
}

fn render_loop(
    mut sink: Box<dyn VideoSink>,
    frames: Receiver<CompleteFrame>,
    conn: Connection,
    proxy: EventLoopProxy<UserEvent>,
    stop: Arc<AtomicBool>,
    win_size: Arc<AtomicU64>,
    host_name: String,
) {
    let mut size = 0u64;
    let mut e2e = Samples::default();
    let mut decode = Samples::default();
    let mut n = 0u32;
    let mut bytes = 0usize;
    let mut last = Instant::now();
    let mut meta_last: Option<FrameMeta> = None;
    let mut errors = 0u32;
    let mut log_n = 0u32;
    while !stop.load(Ordering::Relaxed) {
        let s = win_size.load(Ordering::Relaxed);
        if s != size {
            size = s;
            sink.resize((s >> 32) as u32, s as u32);
            let _ = sink.redraw();
        }
        match frames.recv_timeout(Duration::from_millis(16)) {
            Ok(f) => {
                let Ok((meta, bits)) = FrameMeta::read(&f.data) else {
                    continue;
                };
                match sink.present(meta.codec, bits, meta.flags & frame_flags::KEYFRAME != 0) {
                    Ok(ps) => {
                        let now = clock::now_us();
                        if let Some(cap) = conn.remote_to_local_us(meta.capture_us) {
                            e2e.add(now.saturating_sub(cap) as u32);
                        }
                        decode.add(ps.decode_us + ps.present_us);
                        n += 1;
                        bytes += bits.len();
                        meta_last = Some(meta);
                    }
                    Err(e) => {
                        errors += 1;
                        if errors < 5 || errors.is_multiple_of(100) {
                            warn!("decode/present failed: {e}; requesting keyframe");
                        }
                        conn.request_keyframe();
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
        let dt = last.elapsed();
        if dt >= Duration::from_secs(1) {
            let l = e2e.take();
            let d = decode.take();
            let st = conn.stats();
            let title = match meta_last {
                Some(m) => format!(
                    "sard — {host_name} — {}x{} {} — {:.0} fps — latency {:.1} ms (p95 {:.1}) — decode {:.1} ms — {:.1} Mbps — rtt {:.1} ms — lost {}",
                    m.width,
                    m.height,
                    m.codec.name(),
                    n as f64 / dt.as_secs_f64(),
                    l.p50 as f64 / 1000.0,
                    l.p95 as f64 / 1000.0,
                    d.p50 as f64 / 1000.0,
                    bytes as f64 * 8.0 / dt.as_secs_f64() / 1e6,
                    st.rtt_us as f64 / 1000.0,
                    st.video_frames_lost
                ),
                None => format!("sard — {host_name} — waiting for video…"),
            };
            if log_n.is_multiple_of(5) {
                info!("{title}");
            }
            log_n += 1;
            let _ = proxy.send_event(UserEvent::Stats(title));
            n = 0;
            bytes = 0;
            last = Instant::now();
        }
    }
}

/// Windowless client for tests: decode with the platform decoder, log stats,
/// optionally save one decoded frame. Never opens a window.
fn run_headless(
    conn: Connection,
    events: Receiver<Event>,
    frames: Receiver<CompleteFrame>,
    args: &Args,
    host_name: &str,
) -> Result<()> {
    let duration = args.parse_value::<u64>("duration")?.map(Duration::from_secs);
    let snapshot: Option<std::path::PathBuf> = args.value("snapshot").map(Into::into);
    let mut sink = platform::create_headless_sink()?;
    info!("headless client: {}", sink.describe());
    let hello = ClientHello {
        platform: Platform::current(),
        decoders: platform::decoders(),
        codec_pref: args.value("codec").and_then(Codec::parse).map_or(0, |c| c as u8),
        max_width: args.parse_value("max-width")?.unwrap_or(3840),
        max_height: args.parse_value("max-height")?.unwrap_or(2160),
        max_fps: args.parse_value("fps")?.unwrap_or(60),
        bitrate_kbps: args.parse_value::<f32>("bitrate")?.map_or(0, |m| (m * 1000.0) as u32),
        flags: client_flags::LOCAL_CURSOR,
        display: args.parse_value("display")?.unwrap_or(0),
    };
    conn.send_control(&Control::ClientHello(hello));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        std::thread::Builder::new().name("sard-events".into()).spawn(move || {
            for ev in events {
                match ev {
                    Event::Control(Control::StreamConfig(s)) => {
                        info!(
                            "stream: {} {}x{} @ {} fps from {}",
                            s.codec.name(),
                            s.width,
                            s.height,
                            s.fps,
                            s.host_name
                        )
                    }
                    Event::Control(Control::Status { text }) => info!("host: {text}"),
                    Event::Disconnected(r) => {
                        info!("disconnected: {r}");
                        break;
                    }
                    _ => {}
                }
            }
            stop.store(true, Ordering::Relaxed);
        })?;
    }
    let start = Instant::now();
    let mut last = Instant::now();
    let (mut n, mut bytes, mut total) = (0u32, 0usize, 0u64);
    let (mut e2e, mut dec) = (Samples::default(), Samples::default());
    let mut snapshot_armed = false;
    while !stop.load(Ordering::Relaxed) && duration.is_none_or(|d| start.elapsed() < d) {
        if !snapshot_armed
            && start.elapsed() > Duration::from_secs(2)
            && let Some(p) = &snapshot
        {
            sink.snapshot_next(p)?;
            snapshot_armed = true;
        }
        match frames.recv_timeout(Duration::from_millis(50)) {
            Ok(f) => {
                let Ok((meta, bits)) = FrameMeta::read(&f.data) else { continue };
                match sink.present(meta.codec, bits, meta.flags & frame_flags::KEYFRAME != 0) {
                    Ok(ps) => {
                        if let Some(cap) = conn.remote_to_local_us(meta.capture_us) {
                            e2e.add(clock::now_us().saturating_sub(cap) as u32);
                        }
                        dec.add(ps.decode_us);
                        n += 1;
                        total += 1;
                        bytes += bits.len();
                    }
                    Err(e) => {
                        warn!("decode failed: {e}; requesting keyframe");
                        conn.request_keyframe();
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
        if last.elapsed() >= Duration::from_secs(5) {
            let (l, d, st) = (e2e.take(), dec.take(), conn.stats());
            let dt = last.elapsed().as_secs_f64();
            info!(
                "{host_name}: {:.0} fps | capture→decoded p50 {:.1} ms p95 {:.1} ms | decode {:.1} ms | {:.1} Mbps | rtt {:.2} ms | frames lost {} | fec-repaired {} | rtx-repaired {}",
                n as f64 / dt,
                l.p50 as f64 / 1000.0,
                l.p95 as f64 / 1000.0,
                d.p50 as f64 / 1000.0,
                bytes as f64 * 8.0 / dt / 1e6,
                st.rtt_us as f64 / 1000.0,
                st.video_frames_lost,
                st.video_fec_recovered,
                st.video_rtx_recovered
            );
            n = 0;
            bytes = 0;
            last = Instant::now();
        }
    }
    info!("headless client done: {total} frames in {:.1}s", start.elapsed().as_secs_f64());
    conn.close();
    Ok(())
}
