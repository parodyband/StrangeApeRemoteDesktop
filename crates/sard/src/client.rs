//! `sard connect`: one viewer session in its own window (see `session.rs`),
//! or a windowless decoder for tests (`--headless`).

use crate::args::Args;
use crate::session::{AppEvent, Outcome, Session, SessionOpts};
use crate::{Result, platform};
use sard_core::clock;
use sard_core::config::{self, KnownHost, KnownHosts};
use sard_core::net::{Client, Connection, Event};
use sard_core::proto::{AudioPacket, ClientHello, Codec, Control, Platform, client_flags};
use sard_core::stats::Samples;
use sard_core::video::{CompleteFrame, FrameMeta, frame_flags};
use sard_core::{anywhere, stun, turn};
use sard_core::{error, info, warn};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::WindowId;

/// Resolve `arg` (alias, name or address) to a paired host and the address to dial.
pub fn resolve(arg: &str) -> Result<(KnownHost, String)> {
    let dir = config::config_dir();
    let aliases = config::Aliases::load(&dir)?;
    let target = aliases.resolve(arg);
    if target != arg {
        info!("{arg} → {target}");
    }
    let known = KnownHosts::load(&dir)?;
    let host = known
        .find(target)
        .ok_or_else(|| {
            format!("not paired with {target}; run `sard host --pair` there, then `sard pair {arg} <code>` here")
        })?
        .clone();
    // Prefer an explicit address typed by the user (may have changed since pairing).
    let addr = if target.parse::<std::net::IpAddr>().is_ok() || target.contains(':') || target.contains('.') {
        target.to_string()
    } else {
        host.addr.clone()
    };
    Ok((host, addr))
}

/// Connect to a paired host: directly at `addr`, and with Connect Anywhere
/// configured also through the rendezvous service (public addresses, hole
/// punching). Whichever path answers first is used.
pub fn dial(host: &KnownHost, addr: &str, args: &Args) -> Result<Connection> {
    let dir = config::config_dir();
    let identity = config::load_or_create_identity(&dir)?;
    let cfg = crate::net_config(args)?;
    let Some(service) = anywhere::Service::load(&dir) else {
        return Ok(Client::connect(addr, host.key, identity, cfg, Duration::from_secs(8))?);
    };
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
    let lan: Vec<std::net::SocketAddr> = sard_core::net::resolve(addr).into_iter().filter(|a| a.is_ipv4()).collect();
    // Our public address, learned on the socket the connection will use.
    let public = stun::resolve(stun::DEFAULT_SERVER, false)
        .and_then(|server| stun::query(&socket, server, Duration::from_millis(1500)));
    let (tx, rx) = std::sync::mpsc::channel();
    let host_addrs: Arc<Mutex<Option<Vec<std::net::SocketAddr>>>> = Arc::default();
    {
        let (service, found) = (service.clone(), Arc::clone(&host_addrs));
        let (host_key, me, name) = (host.key, identity.public(), host.name.clone());
        std::thread::spawn(move || match service.introduce(&host_key, &me, public.as_slice()) {
            Ok(Some(addrs)) => {
                info!("Connect Anywhere: {name} is at {addrs:?}");
                *found.lock().unwrap() = Some(addrs.clone());
                let _ = tx.send(addrs);
            }
            Ok(None) => info!("Connect Anywhere: {name} is not online"),
            Err(e) => warn!("{e}"),
        });
    }
    // 1. Directly: LAN address, or the host's public address with hole punching.
    //    (SARD_FORCE_RELAY=1 skips this, to test the relay.)
    let force_relay = std::env::var_os("SARD_FORCE_RELAY").is_some();
    if force_relay {
        for _ in 0..50 {
            if host_addrs.lock().unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let direct = if force_relay {
        Err(sard_core::net::NetError::Timeout)
    } else {
        Client::connect_candidates(
            socket,
            lan,
            Some(rx),
            host.key,
            identity.clone(),
            cfg.clone(),
            Duration::from_secs(3),
        )
    };
    match direct {
        Ok(c) => return Ok(c),
        Err(sard_core::net::NetError::Timeout) => {}
        Err(e) => return Err(e.into()),
    }
    // 2. Through the relay, for networks that can't be punched through.
    let not_reached = || format!("{} didn't answer on this network or through Connect Anywhere", host.name);
    let Some(addrs) = host_addrs.lock().unwrap().clone() else { return Err(not_reached().into()) };
    let Some(host_public) = addrs.iter().copied().find(|a| !sard_core::net::is_lan(a.ip())) else {
        return Err(not_reached().into());
    };
    let Some(creds) = service.relay_credentials()? else { return Err(not_reached().into()) };
    info!("Connect Anywhere: no direct path to {}; using the relay", host.name);
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
    let relay = turn::allocate(&socket, &creds, host_public, Duration::from_secs(4))?;
    info!("Connect Anywhere: relay address {}", relay.relayed);
    // The host punches toward the relay so its router lets the relay's packets in.
    service.introduce(&host.key, &identity.public(), &[relay.relayed])?;
    Client::connect_relay(socket, relay, host.key, identity, cfg, Duration::from_secs(6)).map_err(|e| match e {
        sard_core::net::NetError::Timeout => not_reached().into(),
        e => e.into(),
    })
}

pub fn opts_from_args(args: &Args) -> Result<SessionOpts> {
    Ok(SessionOpts {
        fps: args.parse_value("fps")?,
        bitrate_kbps: args.parse_value::<f32>("bitrate")?.map(|m| (m * 1000.0) as u32),
        codec: args.value("codec").and_then(Codec::parse),
        display: args.parse_value("display")?.unwrap_or(0),
        fullscreen: args.flag("fullscreen"),
        view_only: args.flag("view-only"),
        audio: !args.flag("no-audio"),
        swap_mods: !args.flag("no-swap-modifiers"),
        auto_relative: !args.flag("no-auto-relative"),
    })
}

pub fn run(args: &Args) -> Result<()> {
    let Some(arg) = args.positional(0) else {
        return Err("usage: sard connect <host | alias>".into());
    };
    let (host, addr) = resolve(arg)?;
    println!("Connecting to {} ({addr}) ...", host.name);
    let conn = dial(&host, &addr, args)?;
    if args.flag("headless") {
        let events = conn.take_events().ok_or("events taken")?;
        let frames = conn.take_video().ok_or("video taken")?;
        return run_headless(conn, events, frames, args, &host.name);
    }

    let event_loop = crate::event_loop()?;
    let mut app = Viewer {
        pending: Some((conn, host.name, opts_from_args(args)?)),
        proxy: event_loop.create_proxy(),
        session: None,
        exit_reason: None,
    };
    event_loop.run_app(&mut app)?;
    drop(app.session.take());
    if let Some(r) = app.exit_reason {
        println!("{r}");
    }
    Ok(())
}

/// `sard connect`: exactly one session; the process ends with it.
struct Viewer {
    pending: Option<(Connection, String, SessionOpts)>,
    proxy: EventLoopProxy<AppEvent>,
    session: Option<Session>,
    exit_reason: Option<String>,
}

impl Viewer {
    fn finish(&mut self, el: &ActiveEventLoop, outcome: Outcome) {
        if let Outcome::Closed(reason) = outcome {
            self.exit_reason = Some(reason);
            self.session = None;
            el.exit();
        }
    }
}

impl ApplicationHandler<AppEvent> for Viewer {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let Some((conn, label, opts)) = self.pending.take() else { return };
        match Session::open(el, conn, label, opts, self.proxy.clone(), 1) {
            Ok(s) => self.session = Some(s),
            Err(e) => {
                error!("cannot start: {e}");
                self.exit_reason = Some(format!("error: {e}"));
                el.exit();
            }
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: AppEvent) {
        if let AppEvent::Session(_, ev) = ev
            && let Some(s) = &mut self.session
        {
            let o = s.on_event(el, ev);
            self.finish(el, o);
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if let Some(s) = &mut self.session {
            let o = s.window_event(event);
            self.finish(el, o);
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let Some(s) = &mut self.session {
            s.device_event(event);
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
        flags: client_flags::LOCAL_CURSOR | if args.flag("audio") { client_flags::AUDIO } else { 0 },
        display: args.parse_value("display")?.unwrap_or(0),
    };
    conn.send_control(&Control::ClientHello(hello));
    let stop = Arc::new(AtomicBool::new(false));
    let dry = std::env::var_os("SARD_AUDIO_DRY").is_some();
    // Diagnostic: switch to the next remote display every N seconds.
    let cycle = args.parse_value::<f32>("cycle-displays")?.map(Duration::from_secs_f32);
    let displays = Arc::new(std::sync::Mutex::new((Vec::<u32>::new(), 0u32)));
    {
        let stop = Arc::clone(&stop);
        let displays = Arc::clone(&displays);
        std::thread::Builder::new().name("sard-events".into()).spawn(move || {
            let mut audio: Option<Box<dyn sard_core::media::AudioSink>> = None;
            let (mut packets, mut gaps, mut last_seq) = (0u64, 0u64, None::<u32>);
            for ev in events {
                match ev {
                    Event::Control(Control::AudioConfig { sample_rate, channels }) if !dry => {
                        match platform::open_audio_sink() {
                            Ok(s) => {
                                info!("audio: {sample_rate} Hz x{channels} → {}", s.describe());
                                audio = Some(s);
                            }
                            Err(e) => warn!("audio playback unavailable: {e}"),
                        }
                    }
                    Event::Audio(bytes) => {
                        if let Ok(p) = AudioPacket::decode(&bytes) {
                            packets += 1;
                            if last_seq.is_some_and(|l| p.seq != l.wrapping_add(1)) {
                                gaps += 1;
                            }
                            last_seq = Some(p.seq);
                            if let Some(a) = &audio {
                                a.push(p.seq, &p.samples);
                            }
                        }
                    }
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
                    Event::Control(Control::Status { text }) => {
                        info!("host: {}", sard_core::config::clean_display(&text))
                    }
                    Event::Control(Control::Displays { current, list }) => {
                        let names: Vec<String> = list
                            .iter()
                            .map(|d| {
                                let star = if d.id == current { "*" } else { "" };
                                format!("{star}{} [{}] {}x{}", config::clean_display(&d.name), d.id, d.width, d.height)
                            })
                            .collect();
                        info!("host displays: {}", names.join(", "));
                        *displays.lock().unwrap() = (list.iter().map(|d| d.id).collect(), current);
                    }
                    Event::Disconnected(r) => {
                        info!("disconnected: {r}");
                        break;
                    }
                    _ => {}
                }
            }
            if packets > 0 {
                info!("audio: {packets} packets received, {gaps} gaps");
            }
            stop.store(true, Ordering::Relaxed);
        })?;
    }
    let start = Instant::now();
    let mut last = Instant::now();
    let (mut n, mut bytes, mut total) = (0u32, 0usize, 0u64);
    let (mut e2e, mut dec) = (Samples::default(), Samples::default());
    let mut snapshot_armed = false;
    // Diagnostic: hold the host pointer at a normalized position (input check).
    let move_to: Option<(f32, f32)> = args.value("move-to").and_then(|v| {
        let (x, y) = v.split_once(',')?;
        Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
    });
    let mut last_move = Instant::now() - Duration::from_secs(1);
    let mut last_cycle = Instant::now();
    while !stop.load(Ordering::Relaxed) && duration.is_none_or(|d| start.elapsed() < d) {
        if let Some((x, y)) = move_to
            && start.elapsed() > Duration::from_millis(500)
            && last_move.elapsed() > Duration::from_millis(250)
        {
            conn.send_mouse_abs(x, y);
            last_move = Instant::now();
        }
        if let Some(every) = cycle
            && last_cycle.elapsed() >= every
        {
            last_cycle = Instant::now();
            let (ids, current) = displays.lock().unwrap().clone();
            if ids.len() > 1 {
                let next = ids[(ids.iter().position(|&i| i == current).unwrap_or(0) + 1) % ids.len()];
                info!("switching to display {next}");
                conn.send_control(&Control::SelectDisplay { id: next });
            }
        }
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
