//! One viewer session: a window showing a remote host, its connection, the
//! render thread (decode + present) and input forwarding. Used by both
//! `sard connect` (one session, exit when it ends) and the tray app (sessions
//! opened and closed from the menu).

use crate::keymap;
use crate::{Result, platform};
use sard_core::clock;
use sard_core::media::{DisplayInfo, VideoSink, letterbox};
use sard_core::net::{Connection, Event};
use sard_core::proto::{AudioPacket, ClientHello, Codec, Control, Platform, StreamConfig, button, client_flags};
use sard_core::stats::Samples;
use sard_core::video::{CompleteFrame, FrameMeta, frame_flags};
use sard_core::{info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{DeviceEvent, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{CursorGrabMode, CustomCursor, Fullscreen, Window, WindowId};

const STREAM_CURSOR_METADATA: u32 = crate::host::STREAM_CURSOR_METADATA;

/// Events from a session's background threads to the UI thread.
#[derive(Debug)]
pub enum SessionEvent {
    Control(Control),
    CursorPos {
        visible: bool,
    },
    Disconnected(String),
    Stats(String),
    /// Smooth on average but with regular long stalls: the pattern of a Mac's
    /// Wi-Fi pausing for AWDL (AirDrop/Handoff). Sent at most once per session.
    WifiStalls,
}

/// Everything the winit event loop can be woken with.
pub enum AppEvent {
    /// From session `id`'s threads.
    Session(u64, SessionEvent),
    /// Tray / menu-bar item clicked.
    Menu(u32),
    /// A background connect attempt finished.
    Connected { id: u64, label: String, result: std::result::Result<Connection, String> },
    /// The Connect Anywhere token was set or cleared.
    AnywhereChanged,
    /// The user confirmed forgetting a paired host (by key).
    ForgetHost([u8; 32]),
    /// The user confirmed revoking a device allowed to view this computer.
    RemoveDevice([u8; 32]),
    /// "Pair with a new computer" finished: the address to connect to (None = cancelled).
    Paired(std::result::Result<Option<String>, String>),
    /// Host service state changed (viewer connected, sharing toggled, ...).
    HostChanged,
    /// Re-render the tray menu.
    Refresh,
    /// An update check finished (`manual`: the user asked, so report "up to date" too).
    UpdateChecked { manual: bool, result: std::result::Result<Option<crate::update::UpdateInfo>, String> },
    /// The update dialog was answered: 0 install, 1 later, 2 skip (None: closed).
    UpdateChoice { version: String, choice: Option<usize> },
    /// Downloading and installing an update finished.
    UpdateInstalled(std::result::Result<(), String>),
}

#[derive(Clone, Debug, Default)]
pub struct SessionOpts {
    pub fps: Option<u16>,
    pub bitrate_kbps: Option<u32>,
    pub codec: Option<Codec>,
    /// 1-based display index to start on (0 = host default).
    pub display: u8,
    pub fullscreen: bool,
    pub view_only: bool,
    pub audio: bool,
    pub swap_mods: bool,
    pub auto_relative: bool,
}

impl SessionOpts {
    pub fn defaults() -> Self {
        Self { audio: true, swap_mods: true, auto_relative: true, ..Default::default() }
    }
}

/// What the owner should do after handling an event.
pub enum Outcome {
    Continue,
    /// The session ended (window closed, disconnect hotkey, peer gone).
    Closed(String),
}

pub struct Session {
    pub id: u64,
    pub label: String,
    /// What the user picked to connect to (alias or address), for de-duplication.
    pub target: String,
    conn: Connection,
    opts: SessionOpts,
    window: Arc<Window>,
    render: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    win_size: Arc<AtomicU64>,
    stream: Option<StreamConfig>,
    displays: Vec<DisplayInfo>,
    current_display: Option<u32>,
    cursor_pos: Option<PhysicalPosition<f64>>,
    modifiers: ModifiersState,
    swap_mods: bool,
    relative: bool,
    /// Relative mode was switched on automatically because the host hid its cursor.
    auto_relative: bool,
    focused: bool,
    cursors: HashMap<u32, CustomCursor>,
    remote_cursor_visible: bool,
    /// The viewer saw the AWDL stall pattern (see [`SessionEvent::WifiStalls`]).
    pub wifi_stalls: bool,
}

fn pack(w: u32, h: u32) -> u64 {
    ((w as u64) << 32) | h as u64
}

impl Session {
    /// Open the window, start decoding and say hello to the host.
    pub fn open(
        el: &ActiveEventLoop,
        conn: Connection,
        label: String,
        opts: SessionOpts,
        proxy: EventLoopProxy<AppEvent>,
        id: u64,
    ) -> Result<Session> {
        let events = conn.take_events().ok_or("events already taken")?;
        let frames = conn.take_video().ok_or("video already taken")?;
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes()
            .with_title(format!("Sard — connecting to {label}…"))
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        // Windows shows a generic icon on the title bar and taskbar otherwise.
        #[cfg(windows)]
        {
            let rgba = include_bytes!("../assets/tray/color.rgba").to_vec();
            attrs = attrs.with_window_icon(winit::window::Icon::from_rgba(rgba, 64, 64).ok());
        }
        let window = Arc::new(el.create_window(attrs)?);
        if opts.fullscreen {
            window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        window.focus_window();
        let size = window.inner_size();
        let win_size = Arc::new(AtomicU64::new(pack(size.width, size.height)));
        let sink = platform::create_video_sink(
            window.display_handle()?.as_raw(),
            window.window_handle()?.as_raw(),
            size.width,
            size.height,
        )?;
        info!("video output: {}", sink.describe());

        let monitor = window.current_monitor().or_else(|| el.primary_monitor());
        let (mw, mh, hz) = monitor
            .map(|m| (m.size().width, m.size().height, m.refresh_rate_millihertz().unwrap_or(60_000)))
            .unwrap_or((3840, 2160, 60_000));
        let max_fps = opts.fps.unwrap_or(((hz + 500) / 1000) as u16).clamp(10, 240);
        let hello = ClientHello {
            platform: Platform::current(),
            decoders: platform::decoders(),
            codec_pref: opts.codec.map_or(0, |c| c as u8),
            max_width: mw.max(1280) as u16,
            max_height: mh.max(720) as u16,
            max_fps,
            bitrate_kbps: opts.bitrate_kbps.unwrap_or(0),
            flags: client_flags::LOCAL_CURSOR | if opts.audio { client_flags::AUDIO } else { 0 },
            display: opts.display,
            fit_width: 0,
            fit_height: 0,
        };
        info!(
            "requesting up to {}x{} @ {} fps, decoders {:#x}",
            hello.max_width, hello.max_height, max_fps, hello.decoders
        );
        conn.send_control(&Control::ClientHello(hello));

        let stop = Arc::new(AtomicBool::new(false));
        {
            let (proxy, conn, audio) = (proxy.clone(), conn.clone(), opts.audio);
            std::thread::Builder::new()
                .name("sard-events".into())
                .spawn(move || forward_events(events, proxy, id, audio, conn))?;
        }
        let render = {
            let (conn, stop, win_size, label) = (conn.clone(), Arc::clone(&stop), Arc::clone(&win_size), label.clone());
            std::thread::Builder::new()
                .name("sard-render".into())
                .spawn(move || render_loop(sink, frames, conn, proxy, id, stop, win_size, label))?
        };
        Ok(Session {
            id,
            target: label.clone(),
            label,
            conn,
            auto_relative: false,
            swap_mods: false,
            opts,
            window,
            render: Some(render),
            stop,
            win_size,
            stream: None,
            displays: Vec::new(),
            current_display: None,
            cursor_pos: None,
            modifiers: ModifiersState::empty(),
            relative: false,
            focused: true,
            cursors: HashMap::new(),
            remote_cursor_visible: true,
            wifi_stalls: false,
        })
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    /// The host's public key (identifies it in `known_hosts.txt`).
    pub fn host_key(&self) -> [u8; 32] {
        self.conn.peer_key()
    }

    pub fn focus(&self) {
        self.window.focus_window();
    }

    pub fn toggle_fullscreen(&self) {
        let fs = self.window.fullscreen().is_some();
        self.window.set_fullscreen(if fs { None } else { Some(Fullscreen::Borderless(None)) });
    }

    pub fn displays(&self) -> (&[DisplayInfo], Option<u32>) {
        (&self.displays, self.current_display)
    }

    pub fn select_display(&self, id: u32) {
        if self.current_display != Some(id) {
            self.conn.send_control(&Control::SelectDisplay { id });
        }
    }

    /// Stop threads and tell the host we're gone.
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.conn.close();
        if let Some(t) = self.render.take() {
            let _ = t.join();
        }
    }

    fn video_size(&self) -> (u32, u32) {
        self.stream.as_ref().map_or((0, 0), |s| (s.width as u32, s.height as u32))
    }

    /// Window pixel position → normalized video coordinates.
    fn normalize(&self, p: PhysicalPosition<f64>) -> Option<(f32, f32)> {
        let s = self.window.inner_size();
        let (vw, vh) = self.video_size();
        if vw == 0 {
            return None;
        }
        let (rx, ry, rw, rh) = letterbox(s.width, s.height, vw, vh);
        Some((((p.x as f32 - rx) / rw).clamp(0.0, 1.0), ((p.y as f32 - ry) / rh).clamp(0.0, 1.0)))
    }

    fn hotkey(&self) -> bool {
        self.modifiers.control_key() && self.modifiers.alt_key() && self.modifiers.shift_key()
    }

    /// Ctrl+Alt+Shift + key. Returns None if the key isn't a hotkey.
    fn handle_hotkey(&mut self, code: KeyCode) -> Option<Outcome> {
        let digit = match code {
            KeyCode::Digit1 => Some(0),
            KeyCode::Digit2 => Some(1),
            KeyCode::Digit3 => Some(2),
            KeyCode::Digit4 => Some(3),
            KeyCode::Digit5 => Some(4),
            KeyCode::Digit6 => Some(5),
            KeyCode::Digit7 => Some(6),
            KeyCode::Digit8 => Some(7),
            KeyCode::Digit9 => Some(8),
            _ => None,
        };
        let outcome = match code {
            KeyCode::KeyQ => Outcome::Closed("Disconnected.".into()),
            KeyCode::KeyF => {
                self.toggle_fullscreen();
                Outcome::Continue
            }
            KeyCode::KeyM => {
                self.auto_relative = false;
                self.set_relative(!self.relative);
                Outcome::Continue
            }
            KeyCode::KeyK => {
                self.conn.request_keyframe();
                Outcome::Continue
            }
            KeyCode::KeyD => {
                // Next display.
                if let Some(pos) = self.displays.iter().position(|d| Some(d.id) == self.current_display)
                    && self.displays.len() > 1
                {
                    self.select_display(self.displays[(pos + 1) % self.displays.len()].id);
                }
                Outcome::Continue
            }
            _ if digit.is_some() => {
                if let Some(d) = self.displays.get(digit.unwrap()) {
                    self.select_display(d.id);
                }
                Outcome::Continue
            }
            _ => return None,
        };
        // The hotkey chord's modifiers are down on the host; release them.
        self.conn.send_control(&Control::ReleaseAll);
        Some(outcome)
    }

    fn set_relative(&mut self, on: bool) {
        let w = &self.window;
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
            Control::StreamConfig(mut s) => {
                s.host_name = sard_core::config::clean_display(&s.host_name);
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
                // Cmd on a Mac is Ctrl on Linux and Windows: swap only across that divide.
                self.swap_mods = self.opts.swap_mods
                    && s.host_platform != Platform::Unknown
                    && (s.host_platform == Platform::MacOs) != (Platform::current() == Platform::MacOs);
                let resized = self.stream.as_ref().is_none_or(|o| (o.width, o.height) != (s.width, s.height));
                if resized && self.window.fullscreen().is_none() {
                    fit_window(&self.window, s.width as u32, s.height as u32);
                }
                if s.flags & STREAM_CURSOR_METADATA == 0 {
                    self.window.set_cursor_visible(false); // cursor is in the video
                }
                self.stream = Some(s);
            }
            Control::Displays { current, list } => {
                let names: Vec<String> = list.iter().map(|d| sard_core::config::clean_display(&d.name)).collect();
                info!("host displays: {} (streaming {current})", names.join(", "));
                self.displays = list.into_iter().zip(names).map(|(d, name)| DisplayInfo { name, ..d }).collect();
                self.current_display = Some(current);
            }
            Control::CursorShape { serial, hot_x, hot_y, width, height, rgba } => {
                let cursor = if let Some(c) = self.cursors.get(&serial) {
                    c.clone()
                } else {
                    let src = CustomCursor::from_rgba(
                        rgba,
                        width,
                        height,
                        hot_x.min(width.saturating_sub(1)),
                        hot_y.min(height.saturating_sub(1)),
                    );
                    match src {
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
                self.window.set_cursor(cursor);
            }
            Control::Status { text } => {
                let text = sard_core::config::clean_display(&text);
                info!("host: {text}");
                if self.stream.is_none() {
                    self.window.set_title(&format!("Sard — {} — {text}", self.label));
                }
            }
            other => sard_core::debug!("unhandled control {other:?}"),
        }
    }

    pub fn on_event(&mut self, el: &ActiveEventLoop, ev: SessionEvent) -> Outcome {
        match ev {
            SessionEvent::Control(c) => self.on_control(el, c),
            SessionEvent::CursorPos { visible } => {
                if visible != self.remote_cursor_visible {
                    self.remote_cursor_visible = visible;
                    let metadata = self.stream.as_ref().is_some_and(|s| s.flags & STREAM_CURSOR_METADATA != 0);
                    // 3D viewports / games hide the cursor and read relative
                    // motion: follow the host automatically.
                    if metadata && self.opts.auto_relative && !self.opts.view_only {
                        if !visible && !self.relative && self.focused {
                            self.set_relative(true);
                            self.auto_relative = self.relative;
                        } else if visible && self.auto_relative {
                            self.auto_relative = false;
                            self.set_relative(false);
                        }
                    }
                    if !self.relative {
                        self.window.set_cursor_visible(visible);
                    }
                }
            }
            SessionEvent::Disconnected(r) => return Outcome::Closed(format!("Disconnected: {r}")),
            SessionEvent::Stats(s) => self.window.set_title(&s),
            SessionEvent::WifiStalls => self.wifi_stalls = true,
        }
        Outcome::Continue
    }

    pub fn window_event(&mut self, event: WindowEvent) -> Outcome {
        match event {
            WindowEvent::CloseRequested => return Outcome::Closed("Disconnected.".into()),
            WindowEvent::Resized(s) => self.win_size.store(pack(s.width, s.height), Ordering::Relaxed),
            WindowEvent::Focused(false) => {
                self.focused = false;
                self.conn.send_control(&Control::ReleaseAll);
                if self.relative {
                    self.set_relative(false);
                }
                self.auto_relative = false;
            }
            WindowEvent::Focused(true) => self.focused = true,
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                if is_synthetic || event.repeat {
                    return Outcome::Continue; // host compositor does its own autorepeat
                }
                let PhysicalKey::Code(code) = event.physical_key else { return Outcome::Continue };
                let down = event.state == ElementState::Pressed;
                if down
                    && self.hotkey()
                    && let Some(o) = self.handle_hotkey(code)
                {
                    return o;
                }
                if !self.opts.view_only
                    && let Some(mut k) = keymap::to_evdev(code)
                {
                    if self.swap_mods {
                        k = keymap::swap_ctrl_meta(k);
                    }
                    self.conn.send_control(&Control::Key { code: k, down });
                }
            }
            WindowEvent::CursorMoved { .. } | WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. }
                if self.opts.view_only => {}
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
                    MouseButton::Other(_) => return Outcome::Continue,
                };
                let (x, y) = self.cursor_pos.and_then(|p| self.normalize(p)).unwrap_or((0.5, 0.5));
                self.conn.send_control(&Control::MouseButton { button, down: state == ElementState::Pressed, x, y });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let c = match delta {
                    MouseScrollDelta::LineDelta(x, y) => Control::Scroll { dx: x, dy: y, discrete: true },
                    MouseScrollDelta::PixelDelta(p) => {
                        let scale = self.window.scale_factor();
                        Control::Scroll { dx: (p.x / scale) as f32, dy: (p.y / scale) as f32, discrete: false }
                    }
                };
                self.conn.send_control(&c);
            }
            _ => {}
        }
        Outcome::Continue
    }

    pub fn device_event(&mut self, event: DeviceEvent) {
        if self.relative
            && let DeviceEvent::MouseMotion { delta: (dx, dy) } = event
        {
            self.conn.send_control(&Control::MouseRel { dx: dx as f32, dy: dy as f32 });
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

fn fit_window(w: &Window, vw: u32, vh: u32) {
    let Some(m) = w.current_monitor() else { return };
    let ms = m.size();
    let scale = ((ms.width as f64 * 0.9) / vw as f64).min((ms.height as f64 * 0.85) / vh as f64).min(1.0);
    let _ = w.request_inner_size(PhysicalSize::new((vw as f64 * scale) as u32, (vh as f64 * scale) as u32));
}

/// Spots the pattern of a Mac's Wi-Fi pausing for AWDL (AirDrop/Handoff):
/// seconds whose median latency is fine but whose p95 is long, in at least 4
/// of the last 10 seconds.
#[derive(Default)]
struct StallDetector {
    history: u16,
}

impl StallDetector {
    /// Feed one second's latency percentiles (µs); true once the pattern holds.
    fn second(&mut self, p50_us: u32, p95_us: u32) -> bool {
        let stalled = p95_us > 40_000 && p95_us > 3 * p50_us.max(1) && p50_us < 25_000;
        self.history = (self.history << 1 | stalled as u16) & 0x3ff;
        self.history.count_ones() >= 4
    }
}

/// Network events → UI thread; audio and clipboard are handled right here.
fn forward_events(
    events: Receiver<Event>,
    proxy: EventLoopProxy<AppEvent>,
    id: u64,
    audio_enabled: bool,
    conn: Connection,
) {
    let clip = crate::clipsync::ClipSync::start(conn);
    let mut audio: Option<Box<dyn sard_core::media::AudioSink>> = None;
    let send = |ev: SessionEvent| proxy.send_event(AppEvent::Session(id, ev)).is_ok();
    for ev in events {
        match ev {
            Event::Control(Control::AudioConfig { sample_rate, channels }) => {
                if audio_enabled && audio.is_none() {
                    match platform::open_audio_sink() {
                        Ok(s) => {
                            info!("audio: {sample_rate} Hz x{channels} → {}", s.describe());
                            audio = Some(s);
                        }
                        Err(e) => warn!("audio playback unavailable: {e}"),
                    }
                }
            }
            Event::Control(Control::Clipboard { text }) => {
                if let Some(c) = &clip {
                    c.on_remote(text);
                }
            }
            Event::Control(Control::ClipboardImage { serial, total, offset, data }) => {
                if let Some(c) = &clip {
                    c.on_remote_image(serial, total, offset, data);
                }
            }
            Event::Audio(bytes) => {
                if let (Some(a), Ok(p)) = (&audio, AudioPacket::decode(&bytes)) {
                    a.push(p.seq, &p.samples);
                }
            }
            Event::Control(c) => {
                if !send(SessionEvent::Control(c)) {
                    return;
                }
            }
            Event::CursorPos { visible, .. } => {
                if !send(SessionEvent::CursorPos { visible }) {
                    return;
                }
            }
            Event::Disconnected(r) => {
                send(SessionEvent::Disconnected(r));
                return;
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_loop(
    mut sink: Box<dyn VideoSink>,
    frames: Receiver<CompleteFrame>,
    conn: Connection,
    proxy: EventLoopProxy<AppEvent>,
    id: u64,
    stop: Arc<AtomicBool>,
    win_size: Arc<AtomicU64>,
    label: String,
) {
    let mut size = 0u64;
    let mut e2e = Samples::default();
    let mut decode = Samples::default();
    let (mut n, mut bytes, mut errors, mut log_n) = (0u32, 0usize, 0u32, 0u32);
    let mut last = Instant::now();
    let mut meta_last: Option<FrameMeta> = None;
    let (mut stalls, mut stalls_reported) = (StallDetector::default(), false);
    while !stop.load(Ordering::Relaxed) {
        let s = win_size.load(Ordering::Relaxed);
        if s != size {
            size = s;
            sink.resize((s >> 32) as u32, s as u32);
            let _ = sink.redraw();
        }
        match frames.recv_timeout(Duration::from_millis(16)) {
            Ok(f) => {
                let Ok((meta, bits)) = FrameMeta::read(&f.data) else { continue };
                match sink.present(meta.codec, bits, meta.flags & frame_flags::KEYFRAME != 0) {
                    Ok(ps) => {
                        if let Some(cap) = conn.remote_to_local_us(meta.capture_us) {
                            e2e.add(clock::now_us().saturating_sub(cap) as u32);
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
            let (l, d, st) = (e2e.take(), decode.take(), conn.stats());
            // AWDL signature: a normal median but a long tail, again and again.
            if cfg!(target_os = "macos") && !stalls_reported && n >= 20 && stalls.second(l.p50, l.p95) {
                stalls_reported = true;
                info!(
                    "Wi-Fi stalls detected (latency p50 {:.1} ms, p95 {:.1} ms)",
                    l.p50 as f64 / 1e3,
                    l.p95 as f64 / 1e3
                );
                let _ = proxy.send_event(AppEvent::Session(id, SessionEvent::WifiStalls));
            }
            let title = match meta_last {
                Some(m) => format!(
                    "Sard — {label} — {}x{} {} — {:.0} fps — latency {:.1} ms (p95 {:.1}) — decode {:.1} ms — {:.1} Mbps — rtt {:.1} ms — lost {}",
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
                None => format!("Sard — {label} — waiting for video…"),
            };
            if log_n.is_multiple_of(5) {
                info!("{title}");
            }
            log_n += 1;
            if proxy.send_event(AppEvent::Session(id, SessionEvent::Stats(title))).is_err() {
                break;
            }
            n = 0;
            bytes = 0;
            last = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StallDetector;

    #[test]
    fn stall_pattern_needs_repeats_and_a_good_median() {
        let mut d = StallDetector::default();
        // Smooth seconds and one-off hiccups don't count.
        for _ in 0..10 {
            assert!(!d.second(6_000, 9_000));
        }
        for _ in 0..3 {
            assert!(!d.second(6_000, 120_000));
        }
        // A congested link (everything slow) isn't the AWDL pattern.
        let mut c = StallDetector::default();
        for _ in 0..10 {
            assert!(!c.second(60_000, 120_000));
        }
        // Repeated long tails over a good median: yes.
        assert!(d.second(7_000, 90_000));
    }
}
