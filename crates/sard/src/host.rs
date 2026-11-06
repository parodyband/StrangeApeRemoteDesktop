//! `sard host`: accept paired clients and stream this machine to them.

use crate::args::Args;
use crate::{Result, platform};
use sard_core::clock;
use sard_core::config::{self, TrustStore};
use sard_core::crypto::{fingerprint, generate_pairing_code, normalize_pairing_code};
use sard_core::input::PressedSet;
use sard_core::media::{
    AUDIO_PACKET_FRAMES, AUDIO_RATE, CursorMode, InputEvent, InputSink, SourceOptions, VideoSource,
};
use sard_core::net::{Connection, Event, Host, HostOptions};
use sard_core::proto::{AudioPacket, ClientHello, Codec, Control, Platform, StreamConfig, client_flags};
use sard_core::stats::{Ema, Samples};
use sard_core::video::{FRAME_META_LEN, FrameMeta, frame_flags};
use sard_core::{error, info, warn};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// StreamConfig flag: cursor is sent as metadata (client draws it).
pub const STREAM_CURSOR_METADATA: u32 = 1;

/// `sard host`: run the host service in the foreground.
pub fn run(args: &Args) -> Result<()> {
    if args.flag("reselect") {
        // Forget the saved screen-share approval so the picker is shown again.
        let _ = std::fs::remove_file(config::config_dir().join("portal_restore_token"));
    }
    let svc = HostService::start(args, args.flag("pair"), Box::new(|| {}))?;
    let st = svc.status();
    println!("sard host \"{}\" listening on port {} ({})", st.name, st.port, st.fingerprint);
    if !st.ips.is_empty() {
        println!("  addresses: {}", st.ips.join(", "));
    }
    if let Some((code, _)) = &st.pairing {
        println!();
        println!("  Pairing code:  {code}   (single use, valid for 10 minutes)");
        let code = code.replace(' ', "");
        println!(
            "  On the other machine run:  sard pair {} {code}",
            st.ips.first().map_or("<this-ip>", |s| s.as_str())
        );
        println!();
    }
    println!("{}", platform::describe());
    for m in &st.missing_permissions {
        println!("  ! permission needed — {m}");
    }
    println!("Waiting for a client... (Ctrl+C to stop)");
    loop {
        std::thread::park();
    }
}

/// What the tray shows about the host side.
#[derive(Clone, Debug, Default)]
pub struct HostStatus {
    pub name: String,
    pub port: u16,
    pub ips: Vec<String>,
    pub fingerprint: String,
    /// Name of the machine currently viewing this one.
    pub viewer: Option<String>,
    /// Active single-use pairing code and when it stops working.
    pub pairing: Option<(String, Instant)>,
    /// Permissions the OS still needs granted before capture works.
    pub missing_permissions: Vec<String>,
    /// Connect Anywhere (None: not set up on this device).
    pub anywhere: Option<sard_core::anywhere::AgentStatus>,
}

type ChangeFn = Box<dyn Fn() + Send + Sync>;

struct ServiceState {
    status: Mutex<HostStatus>,
    stop: AtomicBool,
    /// Current session (connection) so it can be kicked.
    current: Mutex<Option<Connection>>,
    on_change: ChangeFn,
}

/// The host side running inside a process: listens, pairs, and serves one
/// viewer at a time (a new viewer takes over). Stops when dropped.
pub struct HostService {
    /// Held while sharing: one host per user (two would split the port).
    _lock: std::fs::File,
    /// Registration with the Connect Anywhere service, if set up.
    anywhere: Option<sard_core::anywhere::HostAgent>,
    host: Arc<Host>,
    trust: Arc<Mutex<TrustStore>>,
    state: Arc<ServiceState>,
    accept: Option<std::thread::JoinHandle<()>>,
}

const PAIRING_TTL: Duration = Duration::from_secs(600);

impl HostService {
    pub fn start(args: &Args, pair: bool, on_change: ChangeFn) -> Result<HostService> {
        let lock = crate::lock_file("host.lock")?
            .ok_or("Sard is already sharing this computer (the Sard app or a `sard host` in a terminal)")?;
        let dir = config::config_dir();
        let identity = config::load_or_create_identity(&dir)?;
        let identity_public = identity.public();
        let trust = TrustStore::load(&dir)?;
        let trusted: HashMap<[u8; 32], String> = trust.peers.iter().map(|p| (p.key, p.name.clone())).collect();
        let pair = pair || trust.peers.is_empty();
        let trust = Arc::new(Mutex::new(trust));
        let trust_paired = Arc::clone(&trust);
        let port: u16 = args.parse_value("port")?.unwrap_or(sard_core::DEFAULT_PORT);
        let bind: SocketAddr = format!("{}:{port}", args.value("bind").unwrap_or("0.0.0.0")).parse()?;
        let cfg = crate::net_config(args)?;
        let state = Arc::new(ServiceState {
            status: Mutex::new(HostStatus {
                name: cfg.name.clone(),
                port,
                ips: local_ips(),
                fingerprint: fingerprint(&identity.public()),
                ..Default::default()
            }),
            stop: AtomicBool::new(false),
            current: Mutex::new(None),
            on_change,
        });
        let paired_state = Arc::clone(&state);
        let host = Host::bind(
            bind,
            identity,
            cfg,
            HostOptions {
                trusted,
                pairing_code: None,
                on_paired: Some(Box::new(move |key, name| {
                    info!("paired with {name}");
                    if let Err(e) = trust_paired.lock().unwrap().add(key, name) {
                        error!("could not save trusted client: {e}");
                    }
                    paired_state.status.lock().unwrap().pairing = None; // the code is spent
                    let _ = std::fs::remove_file(config::config_dir().join("pairing-code.txt"));
                    (paired_state.on_change)();
                })),
            },
        )?;
        let host = Arc::new(host);
        let anywhere = sard_core::anywhere::Service::load(&dir).map(|service| {
            let (h1, h2, st) = (Arc::clone(&host), Arc::clone(&host), Arc::clone(&state));
            sard_core::anywhere::HostAgent::start(
                service,
                identity_public,
                sard_core::anywhere::HostHooks {
                    addrs: Box::new(move || {
                        let public = sard_core::stun::resolve(sard_core::stun::DEFAULT_SERVER, false)
                            .and_then(|s| h1.stun(s, Duration::from_secs(2)));
                        let lan = local_ips().iter().filter_map(|ip| format!("{ip}:{port}").parse().ok()).collect();
                        (public, lan)
                    }),
                    punch: Box::new(move |to| h2.punch(to)),
                    changed: Box::new(move || (st.on_change)()),
                },
            )
        });
        let mut svc = HostService {
            _lock: lock,
            anywhere,
            host: Arc::clone(&host),
            trust,
            state: Arc::clone(&state),
            accept: None,
        };
        let _ = std::fs::remove_file(dir.join("pairing-code.txt"));
        if pair {
            svc.new_pairing_code();
        }
        let test_pattern = args.flag("test-pattern");
        if !test_pattern {
            state.status.lock().unwrap().missing_permissions = platform::request_host_permissions();
        }
        let args = args.clone();
        svc.accept = Some(
            std::thread::Builder::new().name("sard-accept".into()).spawn(move || accept_loop(&host, state, &args))?,
        );
        Ok(svc)
    }

    pub fn status(&self) -> HostStatus {
        let anywhere = self.anywhere.as_ref().map(sard_core::anywhere::HostAgent::status);
        let mut s = self.state.status.lock().unwrap();
        s.anywhere = anywhere;
        if s.pairing.as_ref().is_some_and(|(_, until)| Instant::now() >= *until) {
            s.pairing = None;
        }
        s.clone()
    }

    /// Open a fresh 10-minute pairing window and return its code.
    pub fn new_pairing_code(&self) -> String {
        let code = generate_pairing_code();
        self.host.set_pairing_code(normalize_pairing_code(&code).as_deref());
        // Also leave it in a private file: without a terminal (app bundle,
        // systemd), that's where to find it.
        let _ = config::write_private_file(
            &config::config_dir(),
            "pairing-code.txt",
            &format!("{}\n# single use, valid for 10 minutes\n", code.replace(' ', "")),
        );
        self.state.status.lock().unwrap().pairing = Some((code.clone(), Instant::now() + PAIRING_TTL));
        (self.state.on_change)();
        code
    }

    /// Revoke a paired device: it can't connect any more (and is disconnected
    /// if it is viewing right now).
    pub fn remove_device(&self, key: &[u8; 32]) -> Result<()> {
        self.host.untrust(key);
        self.trust.lock().unwrap().remove(key)?;
        let current = self.state.current.lock().unwrap().clone();
        if let Some(c) = current
            && &c.peer_key() == key
        {
            c.send_control(&Control::Status { text: "this device's access was removed".into() });
            std::thread::sleep(Duration::from_millis(30));
            c.close();
        }
        (self.state.on_change)();
        Ok(())
    }

    /// End the current viewer's session.
    pub fn disconnect_viewer(&self) {
        if let Some(c) = self.state.current.lock().unwrap().as_ref() {
            c.send_control(&Control::Status { text: "the host ended the session".into() });
            std::thread::sleep(Duration::from_millis(30));
            c.close();
        }
    }
}

impl Drop for HostService {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
        self.disconnect_viewer();
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(config::config_dir().join("pairing-code.txt"));
    }
}

fn accept_loop(host: &Host, state: Arc<ServiceState>, args: &Args) {
    // One session at a time; a new client takes over from the current one
    // (personal use: reconnecting from another machine should just work).
    let mut current: Option<std::thread::JoinHandle<()>> = None;
    while !state.stop.load(Ordering::Relaxed) {
        let Some(conn) = host.accept(Duration::from_millis(250)) else {
            continue;
        };
        if let Some(handle) = current.take() {
            let old = state.current.lock().unwrap().take();
            if let Some(old) = old
                && !old.is_closed()
            {
                info!("{} connected; ending the session with {}", conn.peer_name(), old.peer_name());
                old.send_control(&Control::Status { text: format!("{} took over this host", conn.peer_name()) });
                std::thread::sleep(Duration::from_millis(50));
                old.close();
            }
            let _ = handle.join();
        }
        info!("session start: {} from {}", conn.peer_name(), conn.peer_addr());
        *state.current.lock().unwrap() = Some(conn.clone());
        state.status.lock().unwrap().viewer = Some(config::clean_display(conn.peer_name()));
        (state.on_change)();
        let (a, state) = (args.clone(), Arc::clone(&state));
        let r = std::thread::Builder::new().name("sard-session".into()).spawn({
            move || {
                match serve(&conn, &a) {
                    Ok(()) => info!("session with {} ended", conn.peer_name()),
                    Err(e) => {
                        error!("session with {} failed: {e}", conn.peer_name());
                        conn.send_control(&Control::Status { text: format!("host error: {e}") });
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
                conn.close();
                // Unless a newer viewer already took over (it holds `current`).
                if state.current.lock().unwrap().take().is_some() {
                    state.status.lock().unwrap().viewer = None;
                    (state.on_change)();
                }
            }
        });
        match r {
            Ok(h) => current = Some(h),
            Err(e) => error!("cannot start session thread: {e}"),
        }
    }
    if let Some(c) = state.current.lock().unwrap().take() {
        c.close();
    }
    if let Some(h) = current {
        let _ = h.join();
    }
}
fn negotiate(hello: &ClientHello, args: &Args) -> Result<SourceOptions> {
    let host_enc = platform::encoders();
    let both = host_enc & hello.decoders;
    if both == 0 {
        return Err("no video codec supported by both host and client".into());
    }
    let pick = |c: Codec| both & c.bit() != 0;
    let codec = if let Some(c) = args.value("codec") {
        let c = Codec::parse(c).ok_or("unknown --codec")?;
        if !pick(c) {
            return Err(format!("{} not supported by both ends", c.name()).into());
        }
        c
    } else if let Some(c) = Codec::from_u8(hello.codec_pref).filter(|c| pick(*c)) {
        c
    } else if pick(Codec::Hevc) {
        Codec::Hevc
    } else if pick(Codec::H264) {
        Codec::H264
    } else {
        Codec::Av1
    };
    let host_fps: Option<u32> = args.parse_value("fps")?;
    let client_fps = if hello.max_fps == 0 { 60 } else { hello.max_fps as u32 };
    let fps = host_fps.map_or(client_fps, |f| f.min(client_fps)).clamp(10, 240);
    let bitrate_kbps = if hello.bitrate_kbps > 0 {
        hello.bitrate_kbps
    } else {
        args.parse_value::<f32>("bitrate")?.map_or(0, |m| (m * 1000.0) as u32)
    };
    let test_size = match args.value("test-pattern") {
        Some(s) => {
            let (w, h) = s.split_once('x').ok_or("--test-pattern expects WxH")?;
            (w.parse()?, h.parse()?)
        }
        None => (2560, 1440),
    };
    let cursor = match args.value("cursor") {
        Some("embedded") => CursorMode::Embedded,
        Some("metadata") | None => {
            if hello.flags & client_flags::LOCAL_CURSOR != 0 {
                CursorMode::Metadata
            } else {
                CursorMode::Embedded
            }
        }
        Some(o) => return Err(format!("unknown --cursor {o}").into()),
    };
    Ok(SourceOptions {
        codec,
        fps,
        bitrate_kbps,
        display: args.parse_value("display")?.unwrap_or(hello.display as u32),
        max_width: if hello.max_width == 0 { 7680 } else { hello.max_width as u32 },
        max_height: if hello.max_height == 0 { 4320 } else { hello.max_height as u32 },
        yuv444: args.flag("yuv444") && hello.flags & client_flags::YUV444 != 0,
        cursor,
        test_pattern: args.flag("test-pattern"),
        preset: args.parse_value("preset")?.unwrap_or(1),
        test_size,
    })
}

/// Default bitrate from pixel rate: ~0.12 bits/pixel, clamped.
pub fn auto_bitrate_kbps(w: u32, h: u32, fps: u32, codec: Codec) -> u32 {
    let px = w as f64 * h as f64 * fps as f64;
    let bpp = match codec {
        Codec::H264 => 0.16,
        Codec::Hevc => 0.12,
        Codec::Av1 => 0.10,
    };
    ((px * bpp / 1000.0) as u32).clamp(8_000, 150_000)
}

fn wait_hello(events: &Receiver<Event>) -> Result<ClientHello> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(Event::Control(Control::ClientHello(h))) => return Ok(h),
            Ok(Event::Disconnected(r)) => return Err(r.into()),
            Ok(_) => {}
            Err(_) => return Err("client did not say hello".into()),
        }
    }
}

struct Shared {
    stop: AtomicBool,
    keyframe: AtomicBool,
    bitrate_kbps: AtomicU32,
    /// Display the client asked for (applied by the video thread).
    select_display: Mutex<Option<u32>>,
}

fn displays_msg(source: &dyn VideoSource) -> Option<Control> {
    let list = source.displays();
    let current = source.current_display()?;
    (!list.is_empty()).then_some(Control::Displays { current, list })
}

fn serve(conn: &Connection, args: &Args) -> Result<()> {
    let events = conn.take_events().ok_or("events already taken")?;
    let hello = wait_hello(&events)?;
    info!(
        "client {} ({:?}): decoders {:#x}, max {}x{}@{}",
        conn.peer_name(),
        hello.platform,
        hello.decoders,
        hello.max_width,
        hello.max_height,
        hello.max_fps
    );
    let mut opts = negotiate(&hello, args)?;
    if !opts.test_pattern {
        conn.send_control(&Control::Status {
            text: "starting screen capture (approve the share dialog on the host if one appears)...".into(),
        });
    }
    let (mut source, input) = platform::open_host(&opts)?;
    let (w, h) = source.size();
    let ceiling = if opts.bitrate_kbps > 0 { opts.bitrate_kbps } else { auto_bitrate_kbps(w, h, opts.fps, opts.codec) };
    opts.bitrate_kbps = ceiling;
    source.set_bitrate(ceiling);
    info!("streaming: {} ({} fps, {} Mbps cap)", source.describe(), opts.fps, ceiling / 1000);

    let cursor_meta = source.cursor_mode() == CursorMode::Metadata;
    let stream_cfg = |w: u32, h: u32, kbps: u32| {
        Control::StreamConfig(StreamConfig {
            codec: opts.codec,
            width: w as u16,
            height: h as u16,
            fps: opts.fps as u16,
            bitrate_kbps: kbps,
            flags: if cursor_meta { STREAM_CURSOR_METADATA } else { 0 },
            host_platform: Platform::current(),
            host_name: config::hostname(),
        })
    };
    conn.send_control(&stream_cfg(w, h, ceiling));
    if let Some(d) = displays_msg(source.as_ref()) {
        conn.send_control(&d);
    }

    // Audio: uncompressed PCM in 5 ms packets, straight from the capture callback.
    // A viewer on this same machine would play our captured audio back into
    // the output we capture: a feedback loop that gets very loud, very fast.
    let peer_ip = conn.peer_addr().ip();
    let same_machine = peer_ip.is_loopback() || local_ips().contains(&peer_ip.to_string());
    if same_machine && hello.flags & client_flags::AUDIO != 0 {
        info!("viewer is on this machine: not sending audio (it would feed back)");
    }
    let _audio = if hello.flags & client_flags::AUDIO != 0 && !args.flag("no-audio") && !same_machine {
        let c = conn.clone();
        let mut seq = 0u32;
        let mut stage: Vec<i16> = Vec::with_capacity(AUDIO_PACKET_FRAMES * 8);
        let per_packet = AUDIO_PACKET_FRAMES * 2;
        match platform::open_audio_source(Box::new(move |s: &[i16]| {
            stage.extend_from_slice(s);
            let mut off = 0;
            while stage.len() - off >= per_packet {
                c.send_audio(&AudioPacket::encode(seq, clock::now_us(), &stage[off..off + per_packet]));
                seq = seq.wrapping_add(1);
                off += per_packet;
            }
            stage.drain(..off);
        })) {
            Ok(a) => {
                info!("audio: {}", a.describe());
                // Only now: the client opens an output stream when told to expect audio.
                conn.send_control(&Control::AudioConfig { sample_rate: AUDIO_RATE, channels: 2 });
                Some(a)
            }
            Err(e) => {
                warn!("audio capture unavailable: {e}");
                None
            }
        }
    } else {
        None
    };

    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        keyframe: AtomicBool::new(true),
        bitrate_kbps: AtomicU32::new(ceiling),
        select_display: Mutex::new(None),
    });
    let input_thread = {
        let shared = Arc::clone(&shared);
        let conn = conn.clone();
        std::thread::Builder::new()
            .name("sard-input".into())
            .spawn(move || input_loop(events, input, &shared, &conn, ceiling))?
    };

    let result = video_loop(conn, source.as_mut(), &shared, &opts, &stream_cfg);
    shared.stop.store(true, Ordering::Relaxed);
    let _ = input_thread.join();
    result
}

fn video_loop(
    conn: &Connection,
    source: &mut dyn VideoSource,
    shared: &Shared,
    opts: &SourceOptions,
    stream_cfg: &dyn Fn(u32, u32, u32) -> Control,
) -> Result<()> {
    let mut size = source.size();
    let mut current_kbps = shared.bitrate_kbps.load(Ordering::Relaxed);
    let mut buf = Vec::with_capacity(1 << 20);
    let mut host_lat = Samples::default();
    let mut frame_bytes = Ema::new(0.05);
    let mut frames = 0u32;
    let mut last_log = Instant::now();
    while !shared.stop.load(Ordering::Relaxed) && !conn.is_closed() {
        if let Some(id) = shared.select_display.lock().unwrap().take() {
            match source.select_display(id) {
                Ok(()) => {
                    // A new display may have a new size; the client resets on StreamConfig.
                    let (w, h) = source.size();
                    size = (w, h);
                    conn.send_control(&stream_cfg(w, h, current_kbps));
                    if let Some(d) = displays_msg(source) {
                        conn.send_control(&d);
                    }
                }
                Err(e) => conn.send_control(&Control::Status { text: format!("cannot switch display: {e}") }),
            }
        }
        if shared.keyframe.swap(false, Ordering::Relaxed) {
            source.request_keyframe();
        }
        let kbps = shared.bitrate_kbps.load(Ordering::Relaxed);
        if kbps != current_kbps {
            source.set_bitrate(kbps);
            current_kbps = kbps;
        }
        if let Some(f) = source.next_frame(Duration::from_millis(50))? {
            if (f.width, f.height) != size {
                size = (f.width, f.height);
                info!("stream size changed to {}x{}", size.0, size.1);
                conn.send_control(&stream_cfg(size.0, size.1, current_kbps));
            }
            let flags = if f.keyframe { frame_flags::KEYFRAME } else { 0 };
            buf.clear();
            let send_us = clock::now_us();
            FrameMeta {
                codec: opts.codec,
                flags,
                width: f.width as u16,
                height: f.height as u16,
                capture_us: f.capture_us,
                send_us,
            }
            .write(&mut buf);
            buf.extend_from_slice(&f.data);
            conn.send_video_frame(&buf, flags);
            host_lat.add(send_us.saturating_sub(f.capture_us) as u32);
            frame_bytes.add((buf.len() - FRAME_META_LEN) as f64);
            frames += 1;
        }
        if let Some(c) = source.take_cursor() {
            if let Some(s) = c.shape {
                conn.send_control(&Control::CursorShape {
                    serial: s.serial,
                    hot_x: s.hot_x,
                    hot_y: s.hot_y,
                    width: s.width,
                    height: s.height,
                    rgba: s.rgba,
                });
            }
            conn.send_cursor_pos(c.x, c.y, c.visible);
        }
        if last_log.elapsed() >= Duration::from_secs(5) {
            let s = host_lat.take();
            let st = conn.stats();
            info!(
                "{:.0} fps | capture→send p50 {:.1} ms p95 {:.1} ms | {:.0} KB/frame | {:.1} Mbps (cap {}) | rtt {:.2} ms | fec {:.0}% | rtx {}",
                frames as f64 / last_log.elapsed().as_secs_f64(),
                s.p50 as f64 / 1000.0,
                s.p95 as f64 / 1000.0,
                frame_bytes.get() / 1024.0,
                st.tx_bps / 1e6,
                current_kbps / 1000,
                st.rtt_us as f64 / 1000.0,
                st.fec_ratio * 100.0,
                st.video_retransmits
            );
            frames = 0;
            last_log = Instant::now();
        }
    }
    Ok(())
}

/// Congestion-aware bitrate controller.
///
/// Random packet loss (typical on Wi-Fi) is handled by FEC and retransmission,
/// and lowering the bitrate would not reduce it. Only signals that indicate
/// an overloaded path reduce the bitrate:
/// * frames lost despite FEC + retransmission,
/// * queueing delay (smoothed RTT well above the minimum RTT),
/// * extreme raw loss (> 25%).
///
/// It probes back up slowly after a period of clean reports.
struct RateControl {
    ceiling: u32,
    current: u32,
    last_change: Instant,
    clean_reports: u32,
}

impl RateControl {
    fn on_report(&mut self, received: u32, lost: u32, frames_lost: u32, rtt_us: u64, min_rtt_us: u64) -> Option<u32> {
        let total = received + lost;
        if total < 20 && frames_lost == 0 {
            return None;
        }
        let loss = lost as f32 / total.max(1) as f32;
        let queueing = min_rtt_us > 0 && rtt_us > min_rtt_us + (2 * min_rtt_us).max(4_000);
        let before = self.current;
        if frames_lost > 0 || loss > 0.25 {
            self.current = (self.current as f32 * 0.75) as u32;
            self.clean_reports = 0;
        } else if queueing {
            self.current = (self.current as f32 * 0.9) as u32;
            self.clean_reports = 0;
        } else {
            self.clean_reports += 1;
            if self.clean_reports >= 8 && self.last_change.elapsed() > Duration::from_secs(2) {
                self.current = ((self.current as f32 * 1.10) as u32).min(self.ceiling);
                self.clean_reports = 0;
            }
        }
        self.current = self.current.clamp(2_000.min(self.ceiling), self.ceiling);
        if self.current != before {
            self.last_change = Instant::now();
            Some(self.current)
        } else {
            None
        }
    }
}

fn input_loop(
    events: Receiver<Event>,
    mut input: Box<dyn InputSink>,
    shared: &Shared,
    conn: &Connection,
    ceiling: u32,
) {
    let clip = crate::clipsync::ClipSync::start(conn.clone());
    let mut pressed = PressedSet::default();
    let mut pending: Option<Event> = None;
    let mut rc = RateControl { ceiling, current: ceiling, last_change: Instant::now(), clean_reports: 0 };
    let inject = |input: &mut Box<dyn InputSink>, ev: InputEvent| {
        if let Err(e) = input.inject(&ev) {
            warn!("input injection failed: {e}");
        }
    };
    while !shared.stop.load(Ordering::Relaxed) {
        let mut ev = match pending.take() {
            Some(ev) => ev,
            None => match events.recv_timeout(Duration::from_millis(100)) {
                Ok(ev) => ev,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            },
        };
        // Coalesce bursts of absolute moves: only the newest position matters,
        // and each injection is a round trip through the compositor.
        if matches!(ev, Event::MouseAbs { .. }) {
            while let Ok(next) = events.try_recv() {
                if matches!(next, Event::MouseAbs { .. }) {
                    ev = next;
                } else {
                    pending = Some(next);
                    break;
                }
            }
        }
        match ev {
            Event::MouseAbs { x, y } => inject(&mut input, InputEvent::MoveAbs { x, y }),
            Event::KeyframeRequest => shared.keyframe.store(true, Ordering::Relaxed),
            Event::Report(r) => {
                let st = conn.stats();
                if let Some(kbps) =
                    rc.on_report(r.packets_received, r.packets_lost, r.frames_lost, st.rtt_us, st.min_rtt_us)
                {
                    info!(
                        "bitrate -> {:.1} Mbps (loss {}/{}, frames lost {}, rtt {:.1}/{:.1} ms)",
                        kbps as f32 / 1000.0,
                        r.packets_lost,
                        r.packets_received,
                        r.frames_lost,
                        st.rtt_us as f64 / 1000.0,
                        st.min_rtt_us as f64 / 1000.0
                    );
                    shared.bitrate_kbps.store(kbps, Ordering::Relaxed);
                }
            }
            Event::Disconnected(reason) => {
                info!("client disconnected: {reason}");
                break;
            }
            Event::Control(c) => match c {
                Control::Key { code, down } => {
                    if pressed.key(code, down) {
                        inject(&mut input, InputEvent::Key { code, down });
                    }
                }
                Control::MouseButton { button, down, x, y } => {
                    inject(&mut input, InputEvent::MoveAbs { x, y });
                    if pressed.button(button, down) {
                        inject(&mut input, InputEvent::Button { button, down });
                    }
                }
                Control::Scroll { dx, dy, discrete } => inject(&mut input, InputEvent::Scroll { dx, dy, discrete }),
                Control::MouseRel { dx, dy } => inject(&mut input, InputEvent::MoveRel { dx, dy }),
                Control::ReleaseAll => release_all(&mut pressed, &mut input),
                Control::SelectDisplay { id } => {
                    *shared.select_display.lock().unwrap() = Some(id);
                }
                Control::SetBitrate { kbps } => {
                    rc.ceiling = if kbps == 0 { ceiling } else { kbps };
                    rc.current = rc.ceiling;
                    shared.bitrate_kbps.store(rc.ceiling, Ordering::Relaxed);
                }
                Control::Clipboard { text } => {
                    if let Some(c) = &clip {
                        c.on_remote(text);
                    }
                }
                other => sard_core::debug!("ignoring control message {other:?}"),
            },
            Event::CursorPos { .. } | Event::Audio(_) => {}
        }
    }
    release_all(&mut pressed, &mut input);
    shared.stop.store(true, Ordering::Relaxed);
    let _ = conn;
}

fn release_all(pressed: &mut PressedSet, input: &mut Box<dyn InputSink>) {
    let (keys, buttons) = pressed.take_all();
    for code in keys {
        let _ = input.inject(&InputEvent::Key { code, down: false });
    }
    for button in buttons {
        let _ = input.inject(&InputEvent::Button { button, down: false });
    }
}

/// Non-loopback IPv4 addresses of this machine (for the pairing hint).
fn local_ips() -> Vec<String> {
    let mut out = Vec::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs/freeifaddrs pair; we only read the returned list.
    unsafe {
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut p = ifap;
        while !p.is_null() {
            let a = (*p).ifa_addr;
            if !a.is_null() && (*a).sa_family as i32 == libc::AF_INET {
                let sin = &*(a as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !ip.is_loopback() && !ip.is_link_local() {
                    out.push(ip.to_string());
                }
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_control_backs_off_and_recovers() {
        let mut rc = RateControl { ceiling: 50_000, current: 50_000, last_change: Instant::now(), clean_reports: 0 };
        // Random loss repaired by FEC: no change.
        assert_eq!(rc.on_report(1000, 100, 0, 400, 300), None);
        // Unrecoverable frames: back off.
        assert_eq!(rc.on_report(1000, 100, 2, 400, 300), Some(37_500));
        // Queueing delay: back off a little.
        assert_eq!(rc.on_report(1000, 0, 0, 9_000, 300), Some(33_750));
        rc.last_change = Instant::now() - Duration::from_secs(5);
        let mut up = None;
        for _ in 0..8 {
            up = rc.on_report(1000, 0, 0, 400, 300).or(up);
        }
        assert_eq!(up, Some(37_125));
    }

    #[test]
    fn auto_bitrate_reasonable() {
        let b = auto_bitrate_kbps(2560, 1440, 60, Codec::Hevc);
        assert!((20_000..=40_000).contains(&b), "{b}");
        assert_eq!(auto_bitrate_kbps(7680, 4320, 240, Codec::H264), 150_000);
    }
}
