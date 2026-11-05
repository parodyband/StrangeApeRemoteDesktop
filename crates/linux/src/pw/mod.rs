//! PipeWire video capture stream (libpipewire-0.3 via dlopen).
//!
//! Runs a `pw_thread_loop`; the `process` callback keeps only the newest
//! frame (older ones go straight back to the compositor) so the encoder always
//! works on the freshest image. Cursor metadata is extracted from every
//! buffer, including cursor-only updates that carry no new pixels.

pub mod layout;
pub mod pod;

use crate::dylib;
use layout as L;
use pod::{Pod, choice, frac, i32s, id, int, object, pair, prop, u32s};
use sard_core::{debug, warn};
use std::ffi::{CStr, CString, c_char, c_void};
use std::os::fd::{IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, String>;

dylib! {
    pub struct PwApi ["libpipewire-0.3.so.0", "libpipewire-0.3.so"] {
        fn pw_init(*mut i32, *mut *mut *mut c_char);
        fn pw_get_library_version() -> *const c_char;
        fn pw_thread_loop_new(*const c_char, *const c_void) -> *mut c_void;
        fn pw_thread_loop_get_loop(*mut c_void) -> *mut c_void;
        fn pw_thread_loop_start(*mut c_void) -> i32;
        fn pw_thread_loop_stop(*mut c_void);
        fn pw_thread_loop_destroy(*mut c_void);
        fn pw_thread_loop_lock(*mut c_void);
        fn pw_thread_loop_unlock(*mut c_void);
        fn pw_context_new(*mut c_void, *mut c_void, usize) -> *mut c_void;
        fn pw_context_destroy(*mut c_void);
        fn pw_context_connect_fd(*mut c_void, i32, *mut c_void, usize) -> *mut c_void;
        fn pw_core_disconnect(*mut c_void) -> i32;
        fn pw_properties_new_string(*const c_char) -> *mut c_void;
        fn pw_stream_new(*mut c_void, *const c_char, *mut c_void) -> *mut c_void;
        fn pw_stream_add_listener(*mut c_void, *mut c_void, *const c_void, *mut c_void);
        fn pw_stream_connect(*mut c_void, u32, u32, u32, *mut *const c_void, u32) -> i32;
        fn pw_stream_update_params(*mut c_void, *mut *const c_void, u32) -> i32;
        fn pw_stream_dequeue_buffer(*mut c_void) -> *mut c_void;
        fn pw_stream_queue_buffer(*mut c_void, *mut c_void) -> i32;
        fn pw_stream_disconnect(*mut c_void) -> i32;
        fn pw_stream_destroy(*mut c_void);
    }
}

fn api() -> Result<&'static PwApi> {
    static API: OnceLock<std::result::Result<PwApi, String>> = OnceLock::new();
    API.get_or_init(|| {
        let a = PwApi::load()?;
        // SAFETY: pw_init accepts NULL argc/argv.
        unsafe { (a.pw_init)(std::ptr::null_mut(), std::ptr::null_mut()) };
        Ok(a)
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// Negotiated video format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoFormat {
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub modifier: Option<u64>,
}

impl VideoFormat {
    /// True if the first byte in memory is blue (BGRx/BGRA).
    pub fn is_bgr(&self) -> bool {
        matches!(self.format, L::VIDEO_FORMAT_BGRX | L::VIDEO_FORMAT_BGRA)
    }
}

/// Cursor state accumulated from buffer metadata.
#[derive(Clone, Debug, Default)]
pub struct Cursor {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    /// (serial, hot_x, hot_y, width, height, RGBA pixels)
    pub shape: Option<(u32, u32, u32, u32, u32, Vec<u8>)>,
    pub dirty: bool,
    serial: u32,
    shape_hash: u64,
}

pub enum FrameData {
    Mem { ptr: *const u8, stride: usize, len: usize },
    DmaBuf { fd: i32, offset: u32, stride: u32 },
}

/// A dequeued buffer; give it back with [`Capture::release`].
pub struct Frame {
    buf: *mut c_void,
    pub data: FrameData,
    pub pts_ns: Option<i64>,
    pub format: VideoFormat,
}

struct Inner {
    api: &'static PwApi,
    stream: AtomicPtr<c_void>,
    fps: u32,
    modifiers: Vec<u64>,
    format: Mutex<Option<VideoFormat>>,
    slot: Mutex<Option<(usize, VideoFormat)>>,
    ready: Condvar,
    cursor: Mutex<Cursor>,
    state: AtomicI32,
    error: Mutex<Option<String>>,
}

pub struct Capture {
    inner: Box<Inner>,
    thread_loop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
    _events: Box<[u64; L::STREAM_EVENTS_SIZE / 8]>,
    _hook: Box<[u64; L::SPA_HOOK_SIZE / 8]>,
}

// SAFETY: all PipeWire calls from other threads take the thread-loop lock.
unsafe impl Send for Capture {}

unsafe fn rd_u32(p: *const u8, off: usize) -> u32 {
    unsafe { (p.add(off) as *const u32).read_unaligned() }
}
unsafe fn rd_i32(p: *const u8, off: usize) -> i32 {
    unsafe { (p.add(off) as *const i32).read_unaligned() }
}
unsafe fn rd_ptr(p: *const u8, off: usize) -> *mut u8 {
    unsafe { (p.add(off) as *const *mut u8).read_unaligned() }
}

fn enum_formats(fps: u32, modifiers: &[u64]) -> Vec<Vec<u8>> {
    let shm_formats = [L::VIDEO_FORMAT_BGRX, L::VIDEO_FORMAT_BGRA, L::VIDEO_FORMAT_RGBX, L::VIDEO_FORMAT_RGBA];
    // DMA-BUF: only the formats whose modifiers the caller queried (XR24/AR24).
    let dma_formats = [L::VIDEO_FORMAT_BGRX, L::VIDEO_FORMAT_BGRA];
    let fmt_choice = |list: &[u32]| {
        let mut e = vec![u32s(list[0])];
        e.extend(list.iter().map(|f| u32s(*f)));
        choice(L::CHOICE_ENUM, L::T_ID, &e)
    };
    let common = |formats: &[u32], extra: Vec<Vec<u8>>| {
        let mut p = vec![
            prop(L::FORMAT_MEDIA_TYPE, 0, id(L::MEDIA_TYPE_VIDEO)),
            prop(L::FORMAT_MEDIA_SUBTYPE, 0, id(L::MEDIA_SUBTYPE_RAW)),
            prop(L::FORMAT_VIDEO_FORMAT, 0, fmt_choice(formats)),
            prop(
                L::FORMAT_VIDEO_SIZE,
                0,
                choice(L::CHOICE_RANGE, L::T_RECTANGLE, &[pair(1920, 1080), pair(1, 1), pair(16384, 16384)]),
            ),
            prop(L::FORMAT_VIDEO_FRAMERATE, 0, frac(0, 1)),
            prop(
                L::FORMAT_VIDEO_MAX_FRAMERATE,
                0,
                choice(L::CHOICE_RANGE, L::T_FRACTION, &[pair(fps, 1), pair(1, 1), pair(fps.max(1), 1)]),
            ),
        ];
        p.extend(extra);
        object(L::OBJ_FORMAT, L::PARAM_ENUM_FORMAT, &p)
    };
    let mut out = Vec::new();
    if !modifiers.is_empty() {
        let mut e = vec![pod::i64s(modifiers[0] as i64)];
        e.extend(modifiers.iter().map(|m| pod::i64s(*m as i64)));
        out.push(common(
            &dma_formats,
            vec![prop(
                L::FORMAT_VIDEO_MODIFIER,
                L::PROP_FLAG_MANDATORY | L::PROP_FLAG_DONT_FIXATE,
                choice(L::CHOICE_ENUM, L::T_LONG, &e),
            )],
        ));
    }
    out.push(common(&shm_formats, vec![]));
    out
}

fn buffer_params(dmabuf: bool) -> Vec<Vec<u8>> {
    let types = if dmabuf { 1 << L::DATA_DMABUF } else { (1 << L::DATA_MEMFD) | (1 << L::DATA_MEMPTR) };
    let cursor_size = |n: i32| (L::META_CURSOR_SIZE + L::META_BITMAP_SIZE) as i32 + n * n * 4;
    vec![
        object(
            L::OBJ_PARAM_BUFFERS,
            L::PARAM_BUFFERS,
            &[
                prop(L::BUFFERS_BUFFERS, 0, choice(L::CHOICE_RANGE, L::T_INT, &[i32s(4), i32s(2), i32s(16)])),
                prop(L::BUFFERS_BLOCKS, 0, int(1)),
                prop(L::BUFFERS_DATA_TYPE, 0, choice(L::CHOICE_FLAGS, L::T_INT, &[i32s(types as i32)])),
            ],
        ),
        object(
            L::OBJ_PARAM_META,
            L::PARAM_META,
            &[
                prop(L::META_PARAM_TYPE, 0, id(L::META_HEADER)),
                prop(L::META_PARAM_SIZE, 0, int(L::META_HEADER_SIZE as i32)),
            ],
        ),
        object(
            L::OBJ_PARAM_META,
            L::PARAM_META,
            &[
                prop(L::META_PARAM_TYPE, 0, id(L::META_CURSOR)),
                prop(
                    L::META_PARAM_SIZE,
                    0,
                    choice(
                        L::CHOICE_RANGE,
                        L::T_INT,
                        &[i32s(cursor_size(64)), i32s(cursor_size(1)), i32s(cursor_size(512))],
                    ),
                ),
            ],
        ),
    ]
}

unsafe extern "C" fn on_state_changed(data: *mut c_void, _old: i32, state: i32, error: *const c_char) {
    let inner = unsafe { &*(data as *const Inner) };
    inner.state.store(state, Ordering::Relaxed);
    if !error.is_null() {
        let e = unsafe { CStr::from_ptr(error) }.to_string_lossy().into_owned();
        warn!("pipewire stream error: {e}");
        *inner.error.lock().unwrap() = Some(e);
    }
    debug!("pipewire stream state -> {state}");
    inner.ready.notify_all();
}

unsafe extern "C" fn on_param_changed(data: *mut c_void, pid: u32, param: *const c_void) {
    let inner = unsafe { &*(data as *const Inner) };
    if pid != L::PARAM_FORMAT || param.is_null() {
        return;
    }
    let Some((_, _, props)) = (unsafe { pod::parse_object(param) }) else { return };
    let mut f = VideoFormat { format: 0, width: 0, height: 0, modifier: None };
    for (k, v) in &props {
        match *k {
            L::FORMAT_VIDEO_FORMAT => f.format = v.as_u32().unwrap_or(0),
            L::FORMAT_VIDEO_SIZE => {
                if let Some((w, h)) = v.as_pair() {
                    f.width = w;
                    f.height = h;
                }
            }
            L::FORMAT_VIDEO_MODIFIER => f.modifier = v.as_i64().map(|m| m as u64),
            _ => {}
        }
    }
    debug!("pipewire format: {f:?}");
    *inner.format.lock().unwrap() = Some(f);
    let params: Vec<Pod> = buffer_params(f.modifier.is_some()).iter().map(|b| Pod::from_bytes(b)).collect();
    let mut ptrs: Vec<*const c_void> = params.iter().map(Pod::as_ptr).collect();
    let stream = inner.stream.load(Ordering::Relaxed);
    // SAFETY: called on the loop thread with valid pods.
    unsafe { (inner.api.pw_stream_update_params)(stream, ptrs.as_mut_ptr(), ptrs.len() as u32) };
    inner.ready.notify_all();
}

/// Read cursor metadata from a spa_buffer into `c`.
unsafe fn read_cursor(spa: *const u8, c: &mut Cursor) {
    let n_metas = unsafe { rd_u32(spa, L::BUF_N_METAS) } as usize;
    let metas = unsafe { rd_ptr(spa, L::BUF_METAS) };
    for i in 0..n_metas {
        let m = unsafe { metas.add(i * L::META_SIZE) };
        if unsafe { rd_u32(m, L::META_TYPE) } != L::META_CURSOR {
            continue;
        }
        let msize = unsafe { rd_u32(m, L::META_SIZE_FIELD) } as usize;
        let d = unsafe { rd_ptr(m, L::META_DATA) };
        if d.is_null() || msize < L::META_CURSOR_SIZE {
            return;
        }
        let cid = unsafe { rd_u32(d, L::MC_ID) };
        if cid == 0 {
            if c.visible {
                c.visible = false;
                c.dirty = true;
            }
            return;
        }
        let (x, y) = unsafe { (rd_i32(d, L::MC_POSITION), rd_i32(d, L::MC_POSITION + 4)) };
        if (x, y) != (c.x, c.y) || !c.visible {
            c.x = x;
            c.y = y;
            c.visible = true;
            c.dirty = true;
        }
        let boff = unsafe { rd_u32(d, L::MC_BITMAP_OFFSET) } as usize;
        if boff >= L::META_CURSOR_SIZE && boff + L::META_BITMAP_SIZE <= msize {
            let b = unsafe { d.add(boff) };
            let (fmt, w, h, stride, off) = unsafe {
                (
                    rd_u32(b, L::MB_FORMAT),
                    rd_u32(b, L::MB_SIZE),
                    rd_u32(b, L::MB_SIZE + 4),
                    rd_i32(b, L::MB_STRIDE),
                    rd_u32(b, L::MB_OFFSET) as usize,
                )
            };
            if w > 0
                && h > 0
                && w <= 512
                && h <= 512
                && stride >= (w * 4) as i32
                && boff + off + stride as usize * h as usize <= msize
            {
                let px = unsafe { b.add(off) };
                let mut rgba = Vec::with_capacity((w * h * 4) as usize);
                for row in 0..h as usize {
                    let r = unsafe { std::slice::from_raw_parts(px.add(row * stride as usize), w as usize * 4) };
                    for p in r.chunks_exact(4) {
                        match fmt {
                            L::VIDEO_FORMAT_BGRA | L::VIDEO_FORMAT_BGRX => {
                                rgba.extend_from_slice(&[p[2], p[1], p[0], p[3]])
                            }
                            L::VIDEO_FORMAT_ARGB => rgba.extend_from_slice(&[p[1], p[2], p[3], p[0]]),
                            L::VIDEO_FORMAT_ABGR => rgba.extend_from_slice(&[p[3], p[2], p[1], p[0]]),
                            _ => rgba.extend_from_slice(p),
                        }
                    }
                }
                let hot =
                    unsafe { (rd_i32(d, L::MC_HOTSPOT).max(0) as u32, rd_i32(d, L::MC_HOTSPOT + 4).max(0) as u32) };
                // FNV-1a so identical shapes don't get re-sent.
                let mut hash =
                    0xcbf29ce484222325u64 ^ ((w as u64) << 32 | h as u64) ^ ((hot.0 as u64) << 16 | hot.1 as u64);
                for &byte in &rgba {
                    hash = (hash ^ byte as u64).wrapping_mul(0x100000001b3);
                }
                if hash != c.shape_hash {
                    c.shape_hash = hash;
                    c.serial = c.serial.wrapping_add(1);
                    c.shape = Some((c.serial, hot.0, hot.1, w, h, rgba));
                    c.dirty = true;
                }
            }
        }
        return;
    }
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    let inner = unsafe { &*(data as *const Inner) };
    let api = inner.api;
    let stream = inner.stream.load(Ordering::Relaxed);
    let Some(format) = *inner.format.lock().unwrap() else { return };
    let mut slot = inner.slot.lock().unwrap();
    let mut got = false;
    loop {
        // SAFETY: on the loop thread; stream valid.
        let b = unsafe { (api.pw_stream_dequeue_buffer)(stream) };
        if b.is_null() {
            break;
        }
        let spa = unsafe { rd_ptr(b as *const u8, L::PWBUF_BUFFER) };
        unsafe { read_cursor(spa, &mut inner.cursor.lock().unwrap()) };
        let n_datas = unsafe { rd_u32(spa, L::BUF_N_DATAS) };
        let datas = unsafe { rd_ptr(spa, L::BUF_DATAS) };
        let valid = n_datas > 0 && {
            let chunk = unsafe { rd_ptr(datas, L::DATA_CHUNK) };
            let size = unsafe { rd_u32(chunk, L::CHUNK_SIZE) };
            let flags = unsafe { rd_i32(chunk, L::CHUNK_FLAGS) } as u32;
            size > 0 && flags & L::CHUNK_FLAG_CORRUPTED == 0
        };
        if valid {
            if let Some((old, _)) = slot.replace((b as usize, format)) {
                unsafe { (api.pw_stream_queue_buffer)(stream, old as *mut c_void) };
            }
            got = true;
        } else {
            unsafe { (api.pw_stream_queue_buffer)(stream, b) };
        }
    }
    drop(slot);
    if got || inner.cursor.lock().unwrap().dirty {
        inner.ready.notify_all();
    }
}

impl Capture {
    /// Connect to `node_id` on the PipeWire remote `fd` (from the portal).
    /// `modifiers`: DMA-BUF modifiers we can import (empty = shared memory only).
    pub fn connect(fd: OwnedFd, node_id: u32, fps: u32, modifiers: Vec<u64>) -> Result<Capture> {
        let api = api()?;
        // SAFETY: standard pw_thread_loop setup; all objects are torn down in Drop.
        unsafe {
            let name = CString::new("sard-capture").unwrap();
            let tl = (api.pw_thread_loop_new)(name.as_ptr(), std::ptr::null());
            if tl.is_null() {
                return Err("pw_thread_loop_new failed".into());
            }
            let context = (api.pw_context_new)((api.pw_thread_loop_get_loop)(tl), std::ptr::null_mut(), 0);
            if context.is_null() {
                (api.pw_thread_loop_destroy)(tl);
                return Err("pw_context_new failed".into());
            }
            if (api.pw_thread_loop_start)(tl) < 0 {
                return Err("pw_thread_loop_start failed".into());
            }
            (api.pw_thread_loop_lock)(tl);
            let mut cap = Capture {
                inner: Box::new(Inner {
                    api,
                    stream: AtomicPtr::new(std::ptr::null_mut()),
                    fps,
                    modifiers,
                    format: Mutex::new(None),
                    slot: Mutex::new(None),
                    ready: Condvar::new(),
                    cursor: Mutex::new(Cursor::default()),
                    state: AtomicI32::new(0),
                    error: Mutex::new(None),
                }),
                thread_loop: tl,
                context,
                core: std::ptr::null_mut(),
                _events: Box::new([0; L::STREAM_EVENTS_SIZE / 8]),
                _hook: Box::new([0; L::SPA_HOOK_SIZE / 8]),
            };
            let result = cap.setup(fd, node_id);
            (api.pw_thread_loop_unlock)(tl);
            result?;
            Ok(cap)
        }
    }

    /// Must be called with the loop locked.
    unsafe fn setup(&mut self, fd: OwnedFd, node_id: u32) -> Result<()> {
        let api = self.inner.api;
        unsafe {
            self.core = (api.pw_context_connect_fd)(self.context, fd.into_raw_fd(), std::ptr::null_mut(), 0);
            if self.core.is_null() {
                return Err("pw_context_connect_fd failed".into());
            }
            let props = CString::new("media.type=Video media.category=Capture media.role=Screen").unwrap();
            let name = CString::new("sard").unwrap();
            let stream = (api.pw_stream_new)(self.core, name.as_ptr(), (api.pw_properties_new_string)(props.as_ptr()));
            if stream.is_null() {
                return Err("pw_stream_new failed".into());
            }
            self.inner.stream.store(stream, Ordering::Relaxed);
            let ev = self._events.as_mut_ptr() as *mut u8;
            (ev.add(L::EV_VERSION) as *mut u32).write(L::VERSION_STREAM_EVENTS);
            (ev.add(L::EV_STATE_CHANGED) as *mut usize).write(on_state_changed as *const () as usize);
            (ev.add(L::EV_PARAM_CHANGED) as *mut usize).write(on_param_changed as *const () as usize);
            (ev.add(L::EV_PROCESS) as *mut usize).write(on_process as *const () as usize);
            let data = &*self.inner as *const Inner as *mut c_void;
            (api.pw_stream_add_listener)(stream, self._hook.as_mut_ptr().cast(), self._events.as_ptr().cast(), data);
            let params: Vec<Pod> =
                enum_formats(self.inner.fps, &self.inner.modifiers).iter().map(|b| Pod::from_bytes(b)).collect();
            let mut ptrs: Vec<*const c_void> = params.iter().map(Pod::as_ptr).collect();
            let r = (api.pw_stream_connect)(
                stream,
                L::DIRECTION_INPUT,
                node_id,
                L::FLAG_AUTOCONNECT | L::FLAG_MAP_BUFFERS,
                ptrs.as_mut_ptr(),
                ptrs.len() as u32,
            );
            if r < 0 {
                return Err(format!("pw_stream_connect failed ({r})"));
            }
        }
        Ok(())
    }

    /// Wait until the format is negotiated.
    pub fn wait_format(&self, timeout: Duration) -> Result<VideoFormat> {
        let deadline = Instant::now() + timeout;
        let mut slot = self.inner.slot.lock().unwrap();
        loop {
            if let Some(f) = *self.inner.format.lock().unwrap() {
                return Ok(f);
            }
            if let Some(e) = self.inner.error.lock().unwrap().clone() {
                return Err(format!("pipewire: {e}"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("pipewire: no video format negotiated (is the stream running?)".into());
            }
            slot = self.inner.ready.wait_timeout(slot, left).unwrap().0;
        }
    }

    pub fn format(&self) -> Option<VideoFormat> {
        *self.inner.format.lock().unwrap()
    }

    pub fn error(&self) -> Option<String> {
        self.inner.error.lock().unwrap().clone()
    }

    /// Take the newest frame, waiting up to `timeout`.
    pub fn next_frame(&self, timeout: Duration) -> Option<Frame> {
        let deadline = Instant::now() + timeout;
        let mut slot = self.inner.slot.lock().unwrap();
        loop {
            if let Some((b, format)) = slot.take() {
                return Some(self.describe(b as *mut c_void, format));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || self.inner.cursor.lock().unwrap().dirty {
                return None;
            }
            slot = self.inner.ready.wait_timeout(slot, left).unwrap().0;
        }
    }

    fn describe(&self, b: *mut c_void, format: VideoFormat) -> Frame {
        // SAFETY: b is a dequeued pw_buffer we own until release().
        unsafe {
            let spa = rd_ptr(b as *const u8, L::PWBUF_BUFFER);
            let d0 = rd_ptr(spa, L::BUF_DATAS);
            let ty = rd_u32(d0, L::DATA_TYPE);
            let chunk = rd_ptr(d0, L::DATA_CHUNK);
            let offset = rd_u32(chunk, L::CHUNK_OFFSET);
            let size = rd_u32(chunk, L::CHUNK_SIZE);
            let stride = rd_i32(chunk, L::CHUNK_STRIDE);
            let data = if ty == L::DATA_DMABUF {
                FrameData::DmaBuf {
                    fd: (d0.add(L::DATA_FD) as *const i64).read_unaligned() as i32,
                    offset,
                    stride: stride as u32,
                }
            } else {
                let base = rd_ptr(d0, L::DATA_DATA);
                FrameData::Mem {
                    ptr: base.add(offset as usize),
                    stride: stride.unsigned_abs() as usize,
                    len: size as usize,
                }
            };
            let mut pts_ns = None;
            let n_metas = rd_u32(spa, L::BUF_N_METAS) as usize;
            let metas = rd_ptr(spa, L::BUF_METAS);
            for i in 0..n_metas {
                let m = metas.add(i * L::META_SIZE);
                if rd_u32(m, L::META_TYPE) == L::META_HEADER {
                    let d = rd_ptr(m, L::META_DATA);
                    if !d.is_null() {
                        pts_ns = Some((d.add(L::MH_PTS) as *const i64).read_unaligned());
                    }
                }
            }
            Frame { buf: b, data, pts_ns, format }
        }
    }

    /// Return a frame's buffer to the compositor.
    pub fn release(&self, f: Frame) {
        let api = self.inner.api;
        // SAFETY: queue_buffer from a foreign thread requires the loop lock.
        unsafe {
            (api.pw_thread_loop_lock)(self.thread_loop);
            (api.pw_stream_queue_buffer)(self.inner.stream.load(Ordering::Relaxed), f.buf);
            (api.pw_thread_loop_unlock)(self.thread_loop);
        }
    }

    /// Take a pending cursor change.
    pub fn take_cursor(&self) -> Option<Cursor> {
        let mut c = self.inner.cursor.lock().unwrap();
        if !c.dirty {
            return None;
        }
        c.dirty = false;
        let out = c.clone();
        c.shape = None;
        Some(out)
    }

    /// Drop DMA-BUF from the offered formats and renegotiate (import failed).
    pub fn fall_back_to_shm(&self) {
        let params: Vec<Pod> = enum_formats(self.inner.fps, &[]).iter().map(|b| Pod::from_bytes(b)).collect();
        let mut ptrs: Vec<*const c_void> = params.iter().map(Pod::as_ptr).collect();
        let api = self.inner.api;
        *self.inner.format.lock().unwrap() = None;
        // SAFETY: update_params from a foreign thread with the loop locked.
        unsafe {
            (api.pw_thread_loop_lock)(self.thread_loop);
            if let Some((b, _)) = self.inner.slot.lock().unwrap().take() {
                (api.pw_stream_queue_buffer)(self.inner.stream.load(Ordering::Relaxed), b as *mut c_void);
            }
            (api.pw_stream_update_params)(
                self.inner.stream.load(Ordering::Relaxed),
                ptrs.as_mut_ptr(),
                ptrs.len() as u32,
            );
            (api.pw_thread_loop_unlock)(self.thread_loop);
        }
    }

    pub fn is_streaming(&self) -> bool {
        self.inner.state.load(Ordering::Relaxed) == L::STATE_STREAMING
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let api = self.inner.api;
        // SAFETY: tear down in reverse order with the loop stopped.
        unsafe {
            (api.pw_thread_loop_lock)(self.thread_loop);
            if let Some((b, _)) = self.inner.slot.lock().unwrap().take() {
                (api.pw_stream_queue_buffer)(self.inner.stream.load(Ordering::Relaxed), b as *mut c_void);
            }
            let stream = self.inner.stream.swap(std::ptr::null_mut(), Ordering::Relaxed);
            if !stream.is_null() {
                (api.pw_stream_disconnect)(stream);
                (api.pw_stream_destroy)(stream);
            }
            if !self.core.is_null() {
                (api.pw_core_disconnect)(self.core);
            }
            (api.pw_thread_loop_unlock)(self.thread_loop);
            (api.pw_thread_loop_stop)(self.thread_loop);
            (api.pw_context_destroy)(self.context);
            (api.pw_thread_loop_destroy)(self.thread_loop);
        }
    }
}

pub fn library_version() -> Option<String> {
    let a = api().ok()?;
    // SAFETY: static string.
    let p = unsafe { (a.pw_get_library_version)() };
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}
