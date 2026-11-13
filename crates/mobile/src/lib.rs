//! C API of the Sard core for the iPhone app. See `include/sard.h` for the
//! contract as Swift sees it; this file is its implementation.
//!
//! Everything protocol-shaped stays in Rust: pairing, Connect Anywhere,
//! the encrypted transport, VideoToolbox decode and Metal presentation
//! (shared with the macOS viewer), audio jitter buffering, key mapping. The
//! app is UI: lists, gestures, the terminal emulator, audio output.
//!
//! Threads: each session runs a network-event thread and, for screen
//! sessions, a render thread. Callbacks come from those threads; the app
//! must hop to the main queue asynchronously (never `sync`: closing a
//! session from the main thread waits for these threads).

#![cfg(target_vendor = "apple")]
#![allow(clippy::missing_safety_doc)] // documented in include/sard.h

mod json;
pub mod keys;

use json::Json;
use sard_core::config::{self, KnownHosts};
use sard_core::crypto::fingerprint;
use sard_core::media::JitterBuffer;
use sard_core::net::{Client, Connection, Event, NetConfig};
use sard_core::proto::{AudioPacket, ClientHello, Codec, Control, Platform, button, client_flags, lane};
use sard_core::video::{FrameMeta, frame_flags};
use sard_core::{info, warn};
use sard_macos::metal::Presenter;
use sard_macos::vt::{self, VtDecoder};
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ setup

struct Globals {
    dir: PathBuf,
    name: Mutex<String>,
}

static GLOBALS: OnceLock<Globals> = OnceLock::new();

fn globals() -> &'static Globals {
    GLOBALS.get().expect("sard_init not called")
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run `f`, turning a panic into `fallback` (never unwind into Swift).
fn guard<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        warn!("panic in the Sard core");
        fallback
    })
}

unsafe fn str_arg<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "";
    }
    // SAFETY: caller passes a NUL-terminated UTF-8 string.
    unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("")
}

fn out_string(s: String) -> *mut c_char {
    CString::new(s.replace('\0', "")).unwrap_or_default().into_raw()
}

fn parse_key(hex: &str) -> Option<[u8; 32]> {
    sard_core::wire::unhex(hex).and_then(|b| b.try_into().ok())
}

fn net_config(lane: u8) -> NetConfig {
    NetConfig { name: lock(&globals().name).clone(), lane, ..Default::default() }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_init(config_dir: *const c_char, device_name: *const c_char) {
    // SAFETY: caller contract (C strings).
    let (dir, name) = unsafe { (str_arg(config_dir).to_string(), str_arg(device_name).to_string()) };
    guard((), || {
        sard_core::log::init();
        let dir = PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        if GLOBALS.set(Globals { dir: dir.clone(), name: Mutex::new(name.clone()) }).is_err() {
            *lock(&globals().name) = name;
            return;
        }
        match config::load_or_create_identity(&dir) {
            Ok(id) => info!("sard {} mobile core: {}", env!("CARGO_PKG_VERSION"), fingerprint(&id.public())),
            Err(e) => warn!("identity: {e}"),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_set_device_name(name: *const c_char) {
    // SAFETY: caller contract.
    let name = unsafe { str_arg(name) }.to_string();
    guard((), || *lock(&globals().name) = name)
}

/// Install the app's TCP + TLS transport (Connect Anywhere's HTTPS).
#[cfg(target_os = "ios")]
#[unsafe(no_mangle)]
pub extern "C" fn sard_set_transport(t: sard_core::web::Transport) {
    sard_core::web::set_transport(t);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_free(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: allocated by `out_string`.
        drop(unsafe { CString::from_raw(s) });
    }
}

/// `{"fingerprint": "...", "version": "..."}`
#[unsafe(no_mangle)]
pub extern "C" fn sard_identity() -> *mut c_char {
    out_string(guard(String::from("{}"), || {
        let fp = config::load_or_create_identity(&globals().dir).map(|k| fingerprint(&k.public())).unwrap_or_default();
        Json::object().str("fingerprint", &fp).str("version", env!("CARGO_PKG_VERSION")).build()
    }))
}

/// Paired computers: `[{"key": hex, "name": ..., "addr": ...}]`.
#[unsafe(no_mangle)]
pub extern "C" fn sard_hosts() -> *mut c_char {
    out_string(guard(String::from("[]"), || {
        let hosts = KnownHosts::load(&globals().dir).map(|k| k.hosts).unwrap_or_default();
        let items: Vec<String> = hosts
            .iter()
            .map(|h| {
                Json::object()
                    .str("key", &sard_core::wire::hex(&h.key))
                    .str("name", &config::clean_display(&h.name))
                    .str("addr", &h.addr)
                    .str("fingerprint", &fingerprint(&h.key))
                    .build()
            })
            .collect();
        format!("[{}]", items.join(","))
    }))
}

/// Pair with the computer at `addr` using the 6-digit code it shows.
/// Blocking (up to ~10 s). `{"ok": true, "key", "name"}` or `{"ok": false, "error"}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_pair(addr: *const c_char, code: *const c_char) -> *mut c_char {
    // SAFETY: caller contract.
    let (addr, code) = unsafe { (str_arg(addr).trim().to_string(), str_arg(code).to_string()) };
    out_string(guard(Json::error("internal error"), || {
        let g = globals();
        let r = (|| -> Result<([u8; 32], String), String> {
            let identity = config::load_or_create_identity(&g.dir).map_err(|e| e.to_string())?;
            let (key, name, at) =
                Client::pair(&addr, &code, &identity, &net_config(lane::SCREEN), Duration::from_secs(10))
                    .map_err(|e| e.to_string())?;
            let name = config::clean_display(&name);
            let stored = if at.port() == sard_core::DEFAULT_PORT { at.ip().to_string() } else { at.to_string() };
            KnownHosts::load(&g.dir).and_then(|mut k| k.add(key, &stored, &name)).map_err(|e| e.to_string())?;
            Ok((key, name))
        })();
        match r {
            Ok((key, name)) => {
                Json::object().bool("ok", true).str("key", &sard_core::wire::hex(&key)).str("name", &name).build()
            }
            Err(e) => Json::error(&e),
        }
    }))
}

/// Forget a paired computer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_forget(key: *const c_char) -> bool {
    // SAFETY: caller contract.
    let key = unsafe { str_arg(key) }.to_string();
    guard(false, || {
        let Some(k) = parse_key(&key) else { return false };
        KnownHosts::load(&globals().dir).and_then(|mut h| h.remove(&k)).is_ok_and(|r| r.is_some())
    })
}

/// Change the address a paired computer is reached at.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_set_address(key: *const c_char, addr: *const c_char) -> bool {
    // SAFETY: caller contract.
    let (key, addr) = unsafe { (str_arg(key).to_string(), str_arg(addr).trim().to_string()) };
    guard(false, || {
        let (Some(k), false) = (parse_key(&key), addr.is_empty() || addr.contains(' ')) else { return false };
        let Ok(mut hosts) = KnownHosts::load(&globals().dir) else { return false };
        let Some(h) = hosts.hosts.iter().find(|h| h.key == k) else { return false };
        let name = h.name.clone();
        hosts.add(k, &addr, &name).is_ok()
    })
}

/// Connect Anywhere: `{"enabled": bool}`.
#[unsafe(no_mangle)]
pub extern "C" fn sard_anywhere() -> *mut c_char {
    out_string(guard(String::from("{}"), || {
        Json::object().bool("enabled", sard_core::anywhere::Service::load(&globals().dir).is_some()).build()
    }))
}

/// Set the Connect Anywhere invite code (empty: turn it off). `{"ok": ...}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_set_anywhere(token: *const c_char) -> *mut c_char {
    // SAFETY: caller contract.
    let token = unsafe { str_arg(token) }.trim().to_string();
    out_string(guard(Json::error("internal error"), || {
        let dir = &globals().dir;
        let r = if token.is_empty() {
            match std::fs::remove_file(dir.join("anywhere.txt")) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        } else {
            sard_core::anywhere::Service::save_token(dir, &token)
        };
        match r {
            Ok(()) => Json::object().bool("ok", true).build(),
            Err(e) => Json::error(&e.to_string()),
        }
    }))
}

/// evdev key code for a USB HID usage (`UIKey.keyCode`), 0 if none.
#[unsafe(no_mangle)]
pub extern "C" fn sard_hid_to_evdev(usage: u32) -> u16 {
    keys::hid_to_evdev(usage)
}

// --------------------------------------------------------------- sessions

/// Callbacks into the app (any thread; see the module docs).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SardCallbacks {
    pub ctx: *mut c_void,
    /// A JSON event (see sard.h for the kinds).
    pub event: extern "C" fn(ctx: *mut c_void, json: *const c_char),
    /// Terminal output; acknowledge with `sard_shell_ack` once shown.
    pub shell_data: extern "C" fn(ctx: *mut c_void, id: u32, data: *const u8, len: usize),
    /// Host cursor image, RGBA8 (straight alpha), `width * height * 4` bytes.
    pub cursor_shape:
        extern "C" fn(ctx: *mut c_void, serial: u32, width: u16, height: u16, hot_x: u16, hot_y: u16, rgba: *const u8),
    /// Host cursor position (0..1 of the video) and visibility.
    pub cursor_pos: extern "C" fn(ctx: *mut c_void, x: f32, y: f32, visible: bool),
}

// SAFETY: the app promises the callbacks and ctx may be used from any thread.
unsafe impl Send for SardCallbacks {}
unsafe impl Sync for SardCallbacks {}

impl SardCallbacks {
    fn emit(&self, json: String) {
        if let Ok(c) = CString::new(json) {
            (self.event)(self.ctx, c.as_ptr());
        }
    }
}

/// What a session shows.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SardScreenOptions {
    /// Largest video the phone wants (pixels); 0 = 2560x1600.
    pub max_width: u16,
    pub max_height: u16,
    pub max_fps: u16,
    /// Fit the host's screen to this desktop size at the start (0 = no).
    pub fit_width: u16,
    pub fit_height: u16,
    pub audio: bool,
}

pub const SARD_MODE_SCREEN: u32 = 0;
pub const SARD_MODE_TERMINAL: u32 = 1;

struct SendPresenter(Presenter);
// SAFETY: created on the main thread, then used only by the render thread
// (Metal drawables and drawableSize may be used from any thread).
unsafe impl Send for SendPresenter {}

struct Inner {
    cb: SardCallbacks,
    mode: u32,
    conn: Mutex<Option<Connection>>,
    stop: AtomicBool,
    /// A new layer (or None: detach) for the render thread to pick up.
    layer: Mutex<Option<Option<SendPresenter>>>,
    audio: Mutex<JitterBuffer>,
    audio_on: AtomicBool,
}

impl Inner {
    fn conn(&self) -> Option<Connection> {
        lock(&self.conn).clone()
    }
    fn send(&self, c: &Control) {
        if let Some(conn) = self.conn() {
            conn.send_control(c);
        }
    }
}

pub struct SardSession {
    inner: Arc<Inner>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

/// Open a session with a paired computer (`key`: hex from `sard_hosts`).
/// Returns at once; progress arrives as events (`connected` or `failed`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_connect(
    key: *const c_char,
    mode: u32,
    opts: SardScreenOptions,
    cb: SardCallbacks,
) -> *mut SardSession {
    // SAFETY: caller contract.
    let key = unsafe { str_arg(key) }.to_string();
    guard(std::ptr::null_mut(), || {
        let inner = Arc::new(Inner {
            cb,
            mode,
            conn: Mutex::new(None),
            stop: AtomicBool::new(false),
            layer: Mutex::new(None),
            audio: Mutex::new(JitterBuffer::default()),
            audio_on: AtomicBool::new(false),
        });
        let session = Box::new(SardSession { inner: Arc::clone(&inner), threads: Mutex::new(Vec::new()) });
        // The session thread connects, then runs the events and joins its
        // render thread: joining it (close) waits for everything.
        match std::thread::Builder::new().name("sard-session".into()).spawn(move || run_session(inner, key, opts)) {
            Ok(t) => lock(&session.threads).push(t),
            Err(e) => warn!("cannot start session: {e}"),
        }
        Box::into_raw(session)
    })
}

fn decoders() -> u8 {
    let mut d = Codec::H264.bit();
    for c in [Codec::Hevc, Codec::Av1] {
        if vt::hardware_decode_supported(c) {
            d |= c.bit();
        }
    }
    d
}

fn run_session(inner: Arc<Inner>, key: String, opts: SardScreenOptions) {
    let fail = |e: &str| inner.cb.emit(Json::object().str("type", "failed").str("error", e).build());
    let g = globals();
    let Some(k) = parse_key(&key) else { return fail("unknown computer") };
    let host = match KnownHosts::load(&g.dir).ok().and_then(|h| h.hosts.into_iter().find(|h| h.key == k)) {
        Some(h) => h,
        None => return fail("this computer isn't paired any more"),
    };
    let identity = match config::load_or_create_identity(&g.dir) {
        Ok(i) => i,
        Err(e) => return fail(&e.to_string()),
    };
    let terminal = inner.mode == SARD_MODE_TERMINAL;
    let cfg = net_config(if terminal { lane::TERMINAL } else { lane::SCREEN });
    let conn = match sard_core::anywhere::connect(&g.dir, &host, &host.addr, identity, cfg) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    if inner.stop.load(Ordering::Relaxed) {
        conn.close();
        return;
    }
    let (Some(events), frames) = (conn.take_events(), (!terminal).then(|| conn.take_video()).flatten()) else {
        return fail("internal error");
    };
    let hello = if terminal {
        ClientHello {
            platform: Platform::current(),
            decoders: 0,
            codec_pref: 0,
            max_width: 0,
            max_height: 0,
            max_fps: 0,
            bitrate_kbps: 0,
            flags: client_flags::TERMINAL,
            display: 0,
            fit_width: 0,
            fit_height: 0,
        }
    } else {
        ClientHello {
            platform: Platform::current(),
            decoders: decoders(),
            codec_pref: 0,
            max_width: if opts.max_width == 0 { 2560 } else { opts.max_width },
            max_height: if opts.max_height == 0 { 1600 } else { opts.max_height },
            max_fps: if opts.max_fps == 0 { 60 } else { opts.max_fps },
            bitrate_kbps: 0,
            flags: client_flags::LOCAL_CURSOR | if opts.audio { client_flags::AUDIO } else { 0 },
            display: 0,
            fit_width: opts.fit_width,
            fit_height: opts.fit_height,
        }
    };
    conn.send_control(&Control::ClientHello(hello));
    *lock(&inner.conn) = Some(conn.clone());
    let via = if sard_core::net::is_lan(conn.peer_addr().ip()) { "local network" } else { "internet" };
    inner.cb.emit(
        Json::object()
            .str("type", "connected")
            .str("name", &config::clean_display(conn.peer_name()))
            .str("via", via)
            .build(),
    );
    let mut children = Vec::new();
    if let Some(frames) = frames {
        let i = Arc::clone(&inner);
        let c = conn.clone();
        match std::thread::Builder::new().name("sard-render".into()).spawn(move || render_loop(i, c, frames)) {
            Ok(t) => children.push(t),
            Err(e) => warn!("cannot start render thread: {e}"),
        }
    }
    event_loop(&inner, &conn, events);
    inner.stop.store(true, Ordering::Relaxed);
    for t in children {
        let _ = t.join();
    }
}

fn event_loop(inner: &Inner, conn: &Connection, events: std::sync::mpsc::Receiver<Event>) {
    let cb = &inner.cb;
    let reason = loop {
        if inner.stop.load(Ordering::Relaxed) {
            break None;
        }
        let ev = match events.recv_timeout(Duration::from_millis(100)) {
            Ok(ev) => ev,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break Some("connection closed".to_string()),
        };
        match ev {
            Event::Control(c) => match c {
                Control::ShellData { id, data } => (cb.shell_data)(cb.ctx, id, data.as_ptr(), data.len()),
                Control::CursorShape { serial, hot_x, hot_y, width, height, rgba } => {
                    if rgba.len() == width as usize * height as usize * 4 {
                        (cb.cursor_shape)(cb.ctx, serial, width, height, hot_x, hot_y, rgba.as_ptr());
                    }
                }
                Control::AudioConfig { .. } => {
                    *lock(&inner.audio) = JitterBuffer::default();
                    inner.audio_on.store(true, Ordering::Relaxed);
                    cb.emit(Json::object().str("type", "audio").build());
                }
                other => {
                    if let Some(j) = json::control(&other) {
                        cb.emit(j);
                    }
                }
            },
            Event::CursorPos { x, y, visible } => (cb.cursor_pos)(cb.ctx, x, y, visible),
            Event::Audio(bytes) => {
                if let Ok(p) = AudioPacket::decode(&bytes) {
                    lock(&inner.audio).push(p.seq, &p.samples);
                }
            }
            Event::Disconnected(r) => break Some(r),
            Event::MouseAbs { .. } | Event::KeyframeRequest | Event::Report(_) => {}
        }
    };
    conn.close();
    if let Some(r) = reason {
        cb.emit(Json::object().str("type", "disconnected").str("reason", &r).build());
    }
}

fn render_loop(
    inner: Arc<Inner>,
    conn: Connection,
    frames: std::sync::mpsc::Receiver<sard_core::video::CompleteFrame>,
) {
    let mut presenter: Option<Presenter> = None;
    let mut drawable = (0u32, 0u32);
    let mut decoder: Option<VtDecoder> = None;
    let mut last_kf_req = Instant::now() - Duration::from_secs(1);
    let (mut frames_n, mut bytes, mut latency_sum, mut latency_n, mut decode_sum) = (0u32, 0usize, 0u64, 0u32, 0u64);
    let mut last_stats = Instant::now();
    let mut announced = (0u16, 0u16);
    while !inner.stop.load(Ordering::Relaxed) && !conn.is_closed() {
        if let Some(new) = lock(&inner.layer).take() {
            presenter = new.map(|p| p.0);
            drawable = (0, 0);
            // A new view shows nothing until the next frame: ask for one now.
            conn.request_keyframe();
        }
        let f = match frames.recv_timeout(Duration::from_millis(30)) {
            Ok(f) => f,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(p) = &mut presenter
                    && p.has_frame()
                    && drawable != (0, 0)
                {
                    // Keep the picture after a layer reattach / rotation.
                    let _ = p.draw();
                }
                continue;
            }
            Err(_) => break,
        };
        let Ok((meta, bits)) = FrameMeta::read(&f.data) else { continue };
        if decoder.as_ref().is_none_or(|d| d.codec() != meta.codec) {
            match VtDecoder::new(meta.codec) {
                Ok(d) => decoder = Some(d),
                Err(e) => {
                    warn!("decoder: {e}");
                    continue;
                }
            }
        }
        let t0 = Instant::now();
        let image = match decoder.as_mut().unwrap().decode(bits) {
            Ok(Some(img)) => img,
            Ok(None) => continue,
            Err(e) => {
                if last_kf_req.elapsed() > Duration::from_millis(300) {
                    warn!("decode failed: {e}; requesting keyframe");
                    conn.request_keyframe();
                    last_kf_req = Instant::now();
                }
                if meta.flags & frame_flags::KEYFRAME != 0 {
                    decoder = None;
                }
                continue;
            }
        };
        decode_sum += t0.elapsed().as_micros() as u64;
        let (w, h) = vt::dimensions(&image);
        if (w as u16, h as u16) != announced {
            announced = (w as u16, h as u16);
            inner.cb.emit(Json::object().str("type", "video").num("width", w as f64).num("height", h as f64).build());
        }
        if let Some(p) = &mut presenter {
            if drawable != (w, h) {
                p.resize(w, h);
                drawable = (w, h);
            }
            if p.set_frame(&image).is_ok() {
                let _ = p.draw();
            }
        }
        frames_n += 1;
        bytes += bits.len();
        if let Some(cap) = conn.remote_to_local_us(meta.capture_us) {
            latency_sum += sard_core::clock::now_us().saturating_sub(cap);
            latency_n += 1;
        }
        if last_stats.elapsed() >= Duration::from_secs(1) {
            let dt = last_stats.elapsed().as_secs_f64();
            let st = conn.stats();
            inner.cb.emit(
                Json::object()
                    .str("type", "stats")
                    .num("fps", (frames_n as f64 / dt).round())
                    .num("mbps", (bytes as f64 * 8.0 / dt / 1e6 * 10.0).round() / 10.0)
                    .num(
                        "latency_ms",
                        if latency_n > 0 { (latency_sum / latency_n as u64) as f64 / 1000.0 } else { -1.0 },
                    )
                    .num("decode_ms", if frames_n > 0 { decode_sum as f64 / frames_n as f64 / 1000.0 } else { 0.0 })
                    .num("rtt_ms", st.rtt_us as f64 / 1000.0)
                    .build(),
            );
            (frames_n, bytes, latency_sum, latency_n, decode_sum) = (0, 0, 0, 0, 0);
            last_stats = Instant::now();
        }
    }
}

unsafe fn session<'a>(s: *mut SardSession) -> Option<&'a SardSession> {
    // SAFETY: caller passes a live session from sard_connect (or null).
    unsafe { s.as_ref() }
}

/// End the session: no callbacks run after this returns; frees `s`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_session_close(s: *mut SardSession) {
    if s.is_null() {
        return;
    }
    // SAFETY: from Box::into_raw in sard_connect; not used again by the app.
    let s = unsafe { Box::from_raw(s) };
    guard((), || {
        s.inner.stop.store(true, Ordering::Relaxed);
        if let Some(c) = s.inner.conn() {
            c.close();
        }
        for t in lock(&s.threads).drain(..) {
            let _ = t.join();
        }
    })
}

/// Show the video in this `CAMetalLayer` (main thread), or stop drawing (null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_session_set_layer(s: *mut SardSession, layer: *mut c_void) {
    // SAFETY: caller contract.
    let Some(s) = (unsafe { session(s) }) else { return };
    guard((), || {
        let p = if layer.is_null() {
            None
        } else {
            // SAFETY: the app passes a CAMetalLayer, on the main thread.
            match unsafe { Presenter::with_layer(layer.cast(), 16, 16) } {
                Ok(p) => Some(SendPresenter(p)),
                Err(e) => {
                    warn!("cannot draw into the layer: {e}");
                    None
                }
            }
        };
        *lock(&s.inner.layer) = Some(p);
    })
}

macro_rules! with_session {
    ($s:expr, |$inner:ident| $body:expr) => {{
        // SAFETY: caller contract.
        if let Some(s) = unsafe { session($s) } {
            let $inner = &s.inner;
            guard((), || {
                $body;
            })
        }
    }};
}

/// Move the host pointer to (x, y), normalized 0..1 of the video.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_mouse_move(s: *mut SardSession, x: f32, y: f32) {
    with_session!(s, |i| if let Some(c) = i.conn() {
        c.send_mouse_abs(x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
    })
}

/// Press/release a mouse button (1 left, 2 right, 3 middle) at (x, y).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_mouse_button(s: *mut SardSession, button: u8, down: bool, x: f32, y: f32) {
    let b = if (button::LEFT..=button::FORWARD).contains(&button) { button } else { button::LEFT };
    with_session!(s, |i| i.send(&Control::MouseButton { button: b, down, x: x.clamp(0.0, 1.0), y: y.clamp(0.0, 1.0) }))
}

/// Scroll by (dx, dy) logical pixels (trackpad style).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_scroll(s: *mut SardSession, dx: f32, dy: f32) {
    with_session!(s, |i| i.send(&Control::Scroll { dx, dy, discrete: false }))
}

/// Press/release an evdev key.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_key(s: *mut SardSession, code: u16, down: bool) {
    with_session!(s, |i| i.send(&Control::Key { code, down }))
}

/// Type text (US layout key presses; unmappable characters are skipped).
/// `modifiers`: evdev codes held around each key (Ctrl, Alt, Meta), may be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_type_text(s: *mut SardSession, text: *const c_char, modifiers: *const u16, n_mod: usize) {
    // SAFETY: caller contract.
    let text = unsafe { str_arg(text) }.to_string();
    let mods: Vec<u16> = if modifiers.is_null() {
        Vec::new()
    } else {
        // SAFETY: caller passes n_mod codes.
        unsafe { std::slice::from_raw_parts(modifiers, n_mod) }.to_vec()
    };
    with_session!(s, |i| {
        let key = |code, down| i.send(&Control::Key { code, down });
        for ch in text.chars() {
            let Some((code, shift)) = keys::char_to_key(ch) else { continue };
            for &m in &mods {
                key(m, true);
            }
            if shift {
                key(sard_core::input::key::LEFTSHIFT, true);
            }
            key(code, true);
            key(code, false);
            if shift {
                key(sard_core::input::key::LEFTSHIFT, false);
            }
            for &m in mods.iter().rev() {
                key(m, false);
            }
        }
    })
}

/// Release every key and button held on the host.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_release_all(s: *mut SardSession) {
    with_session!(s, |i| i.send(&Control::ReleaseAll))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_select_display(s: *mut SardSession, id: u32) {
    with_session!(s, |i| i.send(&Control::SelectDisplay { id }))
}

/// Fit the host's screen to a desktop of about width x height (0x0: restore).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_fit_display(s: *mut SardSession, width: u16, height: u16) {
    with_session!(s, |i| i.send(&Control::FitDisplay { width, height }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_request_keyframe(s: *mut SardSession) {
    with_session!(s, |i| if let Some(c) = i.conn() {
        c.request_keyframe()
    })
}

/// Put text on the host's clipboard.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_set_clipboard(s: *mut SardSession, text: *const c_char) {
    // SAFETY: caller contract.
    let text = unsafe { str_arg(text) }.to_string();
    with_session!(s, |i| i.send(&Control::Clipboard { text }))
}

/// Fill `out` with `frames` frames of interleaved stereo 48 kHz audio
/// (silence when there's none). Real-time safe enough for an audio thread:
/// one short lock, no allocation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_audio_pull(s: *mut SardSession, out: *mut i16, frames: usize) {
    if out.is_null() {
        return;
    }
    // SAFETY: caller passes room for frames * 2 samples.
    let buf = unsafe { std::slice::from_raw_parts_mut(out, frames * 2) };
    // SAFETY: caller contract.
    match unsafe { session(s) } {
        Some(s) if s.inner.audio_on.load(Ordering::Relaxed) => lock(&s.inner.audio).pull(buf),
        _ => buf.fill(0),
    }
}

// --------------------------------------------------------------- terminal

/// Open a new shell (`id` 0) or reattach to one (`id` from a `shells` event).
/// The answer is a `shell_opened` or `shell_refused` event with `token`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_shell_open(s: *mut SardSession, token: u32, id: u32, cols: u16, rows: u16) {
    with_session!(s, |i| i.send(&Control::ShellOpen { token, id, cols, rows }))
}

/// Keyboard input for shell `id` (UTF-8 / escape sequences).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_shell_write(s: *mut SardSession, id: u32, data: *const u8, len: usize) {
    if data.is_null() || len == 0 {
        return;
    }
    // SAFETY: caller passes len bytes.
    let data = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    with_session!(s, |i| i.send(&Control::ShellData { id, data }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_shell_resize(s: *mut SardSession, id: u32, cols: u16, rows: u16) {
    with_session!(s, |i| i.send(&Control::ShellResize { id, cols, rows }))
}

/// `count` more `shell_data` callbacks of shell `id` have been shown.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_shell_ack(s: *mut SardSession, id: u32, count: u32) {
    with_session!(s, |i| i.send(&Control::ShellAck { id, count }))
}

/// End shell `id` on the host.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_shell_close(s: *mut SardSession, id: u32) {
    with_session!(s, |i| i.send(&Control::ShellClose { id }))
}

/// Ask for the list of shells (answered with a `shells` event).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sard_shell_list(s: *mut SardSession) {
    with_session!(s, |i| i.send(&Control::ShellList))
}
