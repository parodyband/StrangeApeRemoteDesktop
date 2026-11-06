//! Screen capture with ScreenCaptureKit, feeding the captured IOSurface
//! `CVPixelBuffer`s straight into the VideoToolbox encoder (zero copy).
//!
//! SCStream delivers frames only when the screen changes; the latest one
//! wins (no queueing). On a keyframe request with a static screen the last
//! buffer is re-encoded as a keyframe.
//!
//! Display switching starts a new SCStream on the other display (recreating
//! the encoder if the size changes) and then stops the old one; the input
//! sink shares the current display id, so pointer mapping follows.

use crate::cf::{self, CfRef};
use crate::ffi::*;
use crate::input::{Bounds, CgInput, DisplayBounds};
use crate::objc::{error_string, ns_string, to_string};
use crate::vt::{EncoderConfig, RateControl, VtEncoder};
use block2::RcBlock;
use objc2::rc::{Allocated, Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, ClassBuilder, NSObject, Sel};
use objc2::{ClassType, class, msg_send, sel};
use sard_core::clock;
use sard_core::media::{
    self, CursorMode, CursorUpdate, DisplayInfo, EncodedFrame, InputSink, SourceOptions, VideoSource,
};
use sard_core::proto::Codec;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const SCREEN_RECORDING_HINT: &str = "macOS Screen Recording permission is required: open System Settings → \
     Privacy & Security → Screen & System Audio Recording, enable Sard (or the terminal app running sard), \
     then restart sard.";

#[derive(Default)]
struct Latest {
    frame: Option<(CfRef, u64)>,
    seq: u64,
    stopped: Option<String>,
}

#[derive(Default)]
struct Shared {
    m: Mutex<Latest>,
    cv: Condvar,
}

/// Stream-output objects → their shared state. Callbacks look themselves up
/// here, so a late callback after teardown simply finds nothing.
fn registry() -> &'static Mutex<HashMap<usize, Arc<Shared>>> {
    static R: OnceLock<Mutex<HashMap<usize, Arc<Shared>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

fn lookup(this: *mut AnyObject) -> Option<Arc<Shared>> {
    registry().lock().ok()?.get(&(this as usize)).cloned()
}

extern "C-unwind" fn did_output(
    this: *mut AnyObject,
    _sel: Sel,
    _stream: *mut AnyObject,
    sbuf: *mut c_void,
    kind: isize,
) {
    if kind != 0 || sbuf.is_null() {
        return; // 0 = SCStreamOutputTypeScreen
    }
    let Some(shared) = lookup(this) else { return };
    // SAFETY: valid sample buffer for the duration of the callback; all
    // getters return +0 references.
    unsafe {
        let arr = CMSampleBufferGetSampleAttachmentsArray(sbuf, 0);
        if arr.is_null() || cf::CFArrayGetCount(arr) == 0 {
            return;
        }
        let info = cf::CFArrayGetValueAtIndex(arr, 0);
        let status = cf::number_value(cf::CFDictionaryGetValue(info, SCStreamFrameInfoStatus));
        // SCFrameStatusComplete = 0, SCFrameStatusStarted = 4; idle/blank/... carry no new image.
        if !matches!(status, Some(0) | Some(4)) {
            return;
        }
        let Some(image) = CfRef::retain(CMSampleBufferGetImageBuffer(sbuf)) else { return };
        // The presentation time is on the host clock (mach_absolute_time,
        // the same clock as CLOCK_UPTIME_RAW / clock::now_us).
        let now = clock::now_us();
        let pts = CMSampleBufferGetPresentationTimeStamp(sbuf)
            .as_us()
            .filter(|&t| t <= now && now - t < 1_000_000)
            .unwrap_or(now);
        if let Ok(mut g) = shared.m.lock() {
            g.frame = Some((image, pts));
            g.seq += 1;
        }
        shared.cv.notify_all();
    }
}

extern "C-unwind" fn did_stop(this: *mut AnyObject, _sel: Sel, _stream: *mut AnyObject, error: *mut AnyObject) {
    let Some(shared) = lookup(this) else { return };
    // SAFETY: nil or NSError.
    let msg = unsafe { error_string(error) };
    sard_core::warn!("screen capture stopped: {msg}");
    if let Ok(mut g) = shared.m.lock() {
        g.stopped = Some(msg);
    }
    shared.cv.notify_all();
}

fn output_class() -> &'static AnyClass {
    static CLS: OnceLock<&'static AnyClass> = OnceLock::new();
    CLS.get_or_init(|| {
        let mut b =
            ClassBuilder::new(c"SardStreamOutput", NSObject::class()).expect("SardStreamOutput already defined");
        for p in [c"SCStreamOutput", c"SCStreamDelegate"] {
            if let Some(p) = AnyProtocol::get(p) {
                b.add_protocol(p);
            }
        }
        // SAFETY: signatures match -stream:didOutputSampleBuffer:ofType: and
        // -stream:didStopWithError:.
        unsafe {
            b.add_method(
                sel!(stream:didOutputSampleBuffer:ofType:),
                did_output as extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, *mut c_void, isize),
            );
            b.add_method(
                sel!(stream:didStopWithError:),
                did_stop as extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject),
            );
        }
        b.register()
    })
}

struct SendObj(Retained<AnyObject>);
// SAFETY: used only to hand an object from a completion queue to the waiting thread.
unsafe impl Send for SendObj {}

/// Run an async ObjC call taking a `^(id result, NSError *error)` block and wait.
pub(crate) fn wait_object(
    call: impl FnOnce(&block2::Block<dyn Fn(*mut AnyObject, *mut AnyObject)>),
) -> Result<Retained<AnyObject>, String> {
    let (tx, rx) = mpsc::channel::<Result<SendObj, String>>();
    let tx = Mutex::new(Some(tx));
    let block = RcBlock::new(move |obj: *mut AnyObject, err: *mut AnyObject| {
        // SAFETY: obj is nil or a valid object; err nil or NSError.
        let r = unsafe { Retained::retain(obj) }.map(SendObj).ok_or_else(|| unsafe { error_string(err) });
        if let Some(tx) = tx.lock().ok().and_then(|mut t| t.take()) {
            let _ = tx.send(r);
        }
    });
    call(&block);
    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(r) => r.map(|o| o.0),
        Err(_) => Err("timed out waiting for ScreenCaptureKit".into()),
    }
}

/// Run an async ObjC call taking a `^(NSError *error)` block and wait.
pub(crate) fn wait_done(
    call: impl FnOnce(&block2::Block<dyn Fn(*mut AnyObject)>),
    timeout: Duration,
) -> Result<(), String> {
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let tx = Mutex::new(Some(tx));
    let block = RcBlock::new(move |err: *mut AnyObject| {
        // SAFETY: err is nil or an NSError.
        let r = if err.is_null() { Ok(()) } else { Err(unsafe { error_string(err) }) };
        if let Some(tx) = tx.lock().ok().and_then(|mut t| t.take()) {
            let _ = tx.send(r);
        }
    });
    call(&block);
    rx.recv_timeout(timeout).unwrap_or_else(|_| Err("timed out".into()))
}

/// SCStreamConfiguration for low-latency NV12 capture. Needs no permission.
pub fn stream_config(width: u32, height: u32, fps: u32) -> Retained<AnyObject> {
    // SAFETY: plain property setters on a fresh configuration object.
    unsafe {
        let cfg: Retained<AnyObject> = msg_send![class!(SCStreamConfiguration), new];
        let _: () = msg_send![&*cfg, setWidth: width as usize];
        let _: () = msg_send![&*cfg, setHeight: height as usize];
        let _: () = msg_send![&*cfg, setMinimumFrameInterval: CMTime::new(1, fps.max(1) as i32)];
        let _: () = msg_send![&*cfg, setPixelFormat: kCVPixelFormatType_420v];
        let _: () = msg_send![&*cfg, setShowsCursor: true];
        let _: () = msg_send![&*cfg, setQueueDepth: 4isize];
        let _: () = msg_send![&*cfg, setColorMatrix: kCGDisplayStreamYCbCrMatrix_ITU_R_709_2];
        let _: () = msg_send![&*cfg, setColorSpaceName: kCGColorSpaceSRGB];
        let _: () = msg_send![&*cfg, setCapturesAudio: false];
        cfg
    }
}

/// Pixel size of a display (Retina: physical pixels, not points).
pub fn display_pixel_size(id: CGDirectDisplayID) -> (u32, u32) {
    // SAFETY: plain CG queries; mode released.
    unsafe {
        let mode = CGDisplayCopyDisplayMode(id);
        if !mode.is_null() {
            let r = (CGDisplayModeGetPixelWidth(mode) as u32, CGDisplayModeGetPixelHeight(mode) as u32);
            CGDisplayModeRelease(mode);
            if r.0 > 0 && r.1 > 0 {
                return r;
            }
        }
        let b = CGDisplayBounds(id);
        (b.size.width as u32, b.size.height as u32)
    }
}

/// Fit (w, h) into (max_w, max_h) keeping aspect; even dimensions.
pub fn fit_size(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let s = (max_w as f64 / w as f64).min(max_h as f64 / h as f64).min(1.0);
    let (fw, fh) = ((w as f64 * s) as u32 & !1, (h as f64 * s) as u32 & !1);
    (fw.max(2), fh.max(2))
}

/// The current `SCShareableContent` (needs Screen Recording permission).
pub(crate) fn shareable_content() -> Result<Retained<AnyObject>, String> {
    wait_object(|block| unsafe {
        let _: () = msg_send![
            class!(SCShareableContent),
            getShareableContentExcludingDesktopWindows: true,
            onScreenWindowsOnly: true,
            completionHandler: block
        ];
    })
    .map_err(|e| format!("cannot list displays: {e}. {SCREEN_RECORDING_HINT}"))
}

/// The `SCDisplay` for a CoreGraphics display id.
///
/// # Safety
/// `content` must be an `SCShareableContent`.
pub(crate) unsafe fn find_display(content: &AnyObject, id: CGDirectDisplayID) -> Option<Retained<AnyObject>> {
    // SAFETY: SCShareableContent / SCDisplay accessors.
    unsafe {
        let displays: *mut AnyObject = msg_send![content, displays];
        let n: usize = if displays.is_null() { 0 } else { msg_send![displays, count] };
        (0..n).find_map(|i| {
            let d: *mut AnyObject = msg_send![displays, objectAtIndex: i];
            let did: u32 = msg_send![d, displayID];
            if did == id { Retained::retain(d) } else { None }
        })
    }
}

/// Active displays, main display first (the order of `--display <n>`).
/// Ids are CoreGraphics display ids; positions are global points
/// (`CGDisplayBounds`). Needs no permission.
pub fn display_list() -> Vec<DisplayInfo> {
    let mut buf = [0u32; 32];
    let mut n = 0u32;
    // SAFETY: valid buffer and count pointers.
    let ok = unsafe { CGGetActiveDisplayList(buf.len() as u32, buf.as_mut_ptr(), &mut n) } == 0;
    // SAFETY: plain query.
    let main = unsafe { CGMainDisplayID() };
    let mut ids: Vec<u32> = if ok { buf[..(n as usize).min(buf.len())].to_vec() } else { Vec::new() };
    if ids.is_empty() {
        ids.push(main);
    }
    ids.sort_by_key(|&id| id != main); // stable: main first, then CoreGraphics order
    let names = screen_names();
    let mut list: Vec<DisplayInfo> = ids
        .into_iter()
        .map(|id| {
            let (width, height) = display_pixel_size(id);
            // SAFETY: plain queries.
            let (b, builtin) = unsafe { (CGDisplayBounds(id), CGDisplayIsBuiltin(id) != 0) };
            let name = names.iter().find(|(i, n)| *i == id && !n.is_empty()).map(|(_, n)| n.clone());
            let name =
                name.unwrap_or_else(|| if builtin { "Built-in Display".into() } else { format!("Display {id}") });
            DisplayInfo { id, name, width, height, x: b.origin.x.round() as i32, y: b.origin.y.round() as i32 }
        })
        .collect();
    dedupe_names(&mut list);
    list
}

/// `NSScreen.localizedName` by display id (empty list if AppKit has none).
fn screen_names() -> Vec<(u32, String)> {
    autoreleasepool(|_| {
        let key = ns_string("NSScreenNumber");
        // SAFETY: NSScreen / NSDictionary / NSNumber accessors; all +0 within the pool.
        unsafe {
            let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
            let n: usize = if screens.is_null() { 0 } else { msg_send![screens, count] };
            (0..n)
                .filter_map(|i| {
                    let screen: *mut AnyObject = msg_send![screens, objectAtIndex: i];
                    let desc: *mut AnyObject = msg_send![screen, deviceDescription];
                    if desc.is_null() {
                        return None;
                    }
                    let num: *mut AnyObject = msg_send![desc, objectForKey: &*key];
                    if num.is_null() {
                        return None;
                    }
                    let id: u32 = msg_send![num, unsignedIntValue];
                    let name: *mut AnyObject = msg_send![screen, localizedName];
                    Some((id, to_string(name)))
                })
                .collect()
        }
    })
}

/// Make identical display names distinguishable ("DELL U2720Q (1)", "… (2)").
fn dedupe_names(list: &mut [DisplayInfo]) {
    let names: Vec<String> = list.iter().map(|d| d.name.clone()).collect();
    let mut seen: HashMap<String, u32> = HashMap::new();
    for d in list.iter_mut() {
        if names.iter().filter(|n| **n == d.name).count() > 1 {
            let k = seen.entry(d.name.clone()).or_insert(0);
            *k += 1;
            d.name = format!("{} ({k})", d.name);
        }
    }
}

/// One running SCStream and the state its output object delivers into.
struct Capture {
    stream: Retained<AnyObject>,
    output: Retained<AnyObject>,
    queue: *mut c_void,
    shared: Arc<Shared>,
}

// SAFETY: SCStream's methods are thread-safe; the callbacks only touch `Shared`.
unsafe impl Send for Capture {}

impl Capture {
    /// Start capturing `display` (an `SCDisplay`) at `w`x`h` NV12; the cursor
    /// is drawn into the video only if `shows_cursor`.
    fn start(display: &AnyObject, w: u32, h: u32, fps: u32, shows_cursor: bool) -> Result<Capture, String> {
        let shared = Arc::new(Shared::default());
        // SAFETY: standard SCK object setup; `output` stays registered until Drop.
        unsafe {
            let output: Retained<AnyObject> = msg_send![output_class(), new];
            registry().lock().unwrap().insert(Retained::as_ptr(&output) as usize, Arc::clone(&shared));

            let empty: Retained<AnyObject> = msg_send![class!(NSArray), array];
            let f: Allocated<AnyObject> = msg_send![class!(SCContentFilter), alloc];
            let filter: Retained<AnyObject> = msg_send![f, initWithDisplay: display, excludingWindows: &*empty];
            let cfg = stream_config(w, h, fps);
            if !shows_cursor {
                let _: () = msg_send![&*cfg, setShowsCursor: false];
            }
            let s: Allocated<AnyObject> = msg_send![class!(SCStream), alloc];
            let stream: Retained<AnyObject> =
                msg_send![s, initWithFilter: &*filter, configuration: &*cfg, delegate: &*output];

            let attr = dispatch_queue_attr_make_with_qos_class(std::ptr::null(), QOS_CLASS_USER_INTERACTIVE, 0);
            let queue = dispatch_queue_create(c"com.strangeape.sard.capture".as_ptr(), attr);
            let cap = Capture { stream, output, queue, shared };
            let mut err: *mut AnyObject = null_mut();
            let ok: bool = msg_send![
                &*cap.stream,
                addStreamOutput: &*cap.output,
                type: 0isize,
                sampleHandlerQueue: queue as *mut AnyObject,
                error: &mut err
            ];
            if !ok {
                return Err(format!("addStreamOutput failed: {}", error_string(err)));
            }
            wait_done(
                |block| {
                    let _: () = msg_send![&*cap.stream, startCaptureWithCompletionHandler: block];
                },
                Duration::from_secs(15),
            )
            .map_err(|e| format!("cannot start screen capture: {e}. {SCREEN_RECORDING_HINT}"))?;
            Ok(cap)
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        autoreleasepool(|_| {
            // SAFETY: valid stream/output.
            unsafe {
                let r = wait_done(
                    |block| {
                        let _: () = msg_send![&*self.stream, stopCaptureWithCompletionHandler: block];
                    },
                    Duration::from_secs(3),
                );
                if let Err(e) = r {
                    sard_core::debug!("stopCapture: {e}");
                }
                let mut err: *mut AnyObject = null_mut();
                let _: bool =
                    msg_send![&*self.stream, removeStreamOutput: &*self.output, type: 0isize, error: &mut err];
            }
        });
        if let Ok(mut r) = registry().lock() {
            r.remove(&(Retained::as_ptr(&self.output) as usize));
        }
        // SAFETY: our queue; SCK has dropped its use of it after removal.
        unsafe { dispatch_release(self.queue) };
    }
}

pub struct ScreenSource {
    cap: Capture,
    enc: VtEncoder,
    seen: u64,
    last: Option<CfRef>,
    force_key: bool,
    /// Display being captured; shared with the input sink.
    display: DisplayBounds,
    max_size: (u32, u32),
    fps: u32,
    /// Viewer-drawn cursor: tracked here instead of drawn into the video.
    cursor: Option<crate::cursor::CursorTracker>,
}

pub fn open(opts: &SourceOptions) -> media::Result<(Box<dyn VideoSource>, Box<dyn InputSink>)> {
    let src = autoreleasepool(|_| ScreenSource::new(opts)).map_err(media::err)?;
    let input = CgInput::new(Box::new(src.display.clone()))?;
    Ok((Box::new(src), Box::new(input)))
}

impl ScreenSource {
    fn new(opts: &SourceOptions) -> Result<Self, String> {
        // SAFETY: plain query; the request shows the system prompt (once).
        if unsafe { !CGPreflightScreenCaptureAccess() } {
            sard_core::warn!("{SCREEN_RECORDING_HINT}");
            unsafe { CGRequestScreenCaptureAccess() };
        }
        let content = shareable_content()?;
        let list = display_list();
        let idx = opts.display as usize;
        let Some(info) = list.get(idx) else {
            return Err(format!("display {idx} not found ({} display(s); 0 = main)", list.len()));
        };
        // SAFETY: `content` is an SCShareableContent.
        let display = unsafe { find_display(&content, info.id) }
            .ok_or_else(|| format!("display {} ({}) is not available for capture", info.id, info.name))?;
        let max_size = (opts.max_width.max(320), opts.max_height.max(240));
        let (w, h) = fit_size(info.width, info.height, max_size.0, max_size.1);
        let fps = opts.fps.max(1);

        let enc = VtEncoder::new(EncoderConfig {
            codec: opts.codec,
            width: w,
            height: h,
            fps,
            bitrate_kbps: opts.bitrate_kbps.max(1000),
            rate_control: RateControl::for_preset(opts.codec, opts.preset),
        })?;
        let metadata_cursor = opts.cursor == CursorMode::Metadata;
        let cap = Capture::start(&display, w, h, fps, !metadata_cursor)?;
        sard_core::info!(
            "ScreenCaptureKit: display {} \"{}\" ({}x{} px) captured at {w}x{h} @ {fps} fps",
            info.id,
            info.name,
            info.width,
            info.height
        );
        Ok(ScreenSource {
            cap,
            enc,
            seen: 0,
            last: None,
            force_key: true,
            display: DisplayBounds::new(info.id),
            max_size,
            fps,
            cursor: metadata_cursor.then(crate::cursor::CursorTracker::default),
        })
    }

    fn switch_to(&mut self, id: CGDirectDisplayID) -> Result<(), String> {
        let content = shareable_content()?;
        // SAFETY: `content` is an SCShareableContent.
        let display = unsafe { find_display(&content, id) }.ok_or_else(|| format!("display {id} is not available"))?;
        let (pw, ph) = display_pixel_size(id);
        let (w, h) = fit_size(pw, ph, self.max_size.0, self.max_size.1);
        let cur = *self.enc.config();
        // Everything that can fail happens before the old stream is touched.
        let enc = if (w, h) != (cur.width, cur.height) {
            Some(VtEncoder::new(EncoderConfig { width: w, height: h, ..cur })?)
        } else {
            None
        };
        let cap = Capture::start(&display, w, h, self.fps, self.cursor.is_none())?;
        drop(std::mem::replace(&mut self.cap, cap)); // stops the old stream
        if let Some(enc) = enc {
            self.enc = enc;
        }
        self.display.set(id);
        if let Some(c) = &mut self.cursor {
            c.reset();
        }
        self.seen = 0;
        self.last = None;
        self.force_key = true;
        sard_core::info!("ScreenCaptureKit: switched to display {id} ({pw}x{ph} px) captured at {w}x{h}");
        Ok(())
    }

    fn encode(&mut self, image: CfRef, capture_us: u64) -> media::Result<Option<EncodedFrame>> {
        let key = std::mem::take(&mut self.force_key);
        let out = self.enc.encode(&image, capture_us, key).map_err(media::err)?;
        self.last = Some(image);
        let Some(out) = out else {
            self.force_key |= key;
            return Ok(None);
        };
        let c = self.enc.config();
        Ok(Some(EncodedFrame { data: out.data, keyframe: out.keyframe, capture_us, width: c.width, height: c.height }))
    }
}

impl VideoSource for ScreenSource {
    fn next_frame(&mut self, timeout: Duration) -> media::Result<Option<EncodedFrame>> {
        let deadline = Instant::now() + timeout;
        let mut g = self.cap.shared.m.lock().unwrap();
        loop {
            if let Some(e) = &g.stopped {
                return Err(media::err(format!("screen capture stopped: {e}")));
            }
            if g.seq != self.seen
                && let Some((image, at)) = g.frame.take()
            {
                self.seen = g.seq;
                drop(g);
                return self.encode(image, at);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || (self.force_key && self.last.is_some()) {
                break;
            }
            // Nothing new on screen: still look at the cursor every 8 ms, so
            // its movements go out promptly (the caller then takes them).
            let slice = if self.cursor.is_some() { left.min(Duration::from_millis(8)) } else { left };
            g = self.cap.shared.cv.wait_timeout(g, slice).unwrap().0;
            if let Some(c) = &self.cursor
                && g.seq == self.seen
                && c.moved(self.display.rect())
            {
                return Ok(None);
            }
        }
        drop(g);
        // Static screen but the client needs a keyframe: re-encode the last image.
        if self.force_key
            && let Some(last) = self.last.clone()
        {
            return self.encode(last, clock::now_us());
        }
        Ok(None)
    }

    fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.enc.set_bitrate(kbps);
    }

    fn codec(&self) -> Codec {
        self.enc.config().codec
    }

    fn size(&self) -> (u32, u32) {
        (self.enc.config().width, self.enc.config().height)
    }

    fn cursor_mode(&self) -> CursorMode {
        if self.cursor.is_some() { CursorMode::Metadata } else { CursorMode::Embedded }
    }

    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        let rect = self.display.rect();
        // Stream pixels per point: the cursor is drawn at the video's scale.
        let scale = self.enc.config().width as f64 / rect.size.width.max(1.0);
        self.cursor.as_mut()?.poll(rect, scale)
    }

    fn describe(&self) -> String {
        let c = self.enc.config();
        format!(
            "ScreenCaptureKit display {} {}x{} → VideoToolbox {}{}{}",
            self.display.id(),
            c.width,
            c.height,
            c.codec.name(),
            if self.enc.hardware { " (hardware" } else { " (software" },
            if self.enc.low_latency { ", low-latency RC)" } else { ")" }
        )
    }

    fn displays(&self) -> Vec<DisplayInfo> {
        display_list()
    }

    fn current_display(&self) -> Option<u32> {
        Some(self.display.id())
    }

    fn select_display(&mut self, id: u32) -> media::Result<()> {
        if id == self.display.id() {
            self.force_key = true;
            return Ok(());
        }
        autoreleasepool(|_| self.switch_to(id)).map_err(media::err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_size_keeps_aspect_and_even() {
        assert_eq!(fit_size(3456, 2234, 7680, 4320), (3456, 2234));
        let (w, h) = fit_size(3456, 2234, 1920, 1080);
        assert!(w <= 1920 && h <= 1080 && w % 2 == 0 && h % 2 == 0);
        assert!(((w as f64 / h as f64) - 3456.0 / 2234.0).abs() < 0.01);
    }

    #[test]
    fn stream_configuration_without_permission() {
        // Exercises every SCStreamConfiguration setter (and its ObjC type
        // encodings) without needing Screen Recording permission.
        autoreleasepool(|_| {
            let cfg = stream_config(1920, 1080, 120);
            unsafe {
                let w: usize = msg_send![&*cfg, width];
                let fmt: u32 = msg_send![&*cfg, pixelFormat];
                let interval: CMTime = msg_send![&*cfg, minimumFrameInterval];
                let cursor: bool = msg_send![&*cfg, showsCursor];
                assert_eq!((w, fmt, cursor), (1920, kCVPixelFormatType_420v, true));
                assert_eq!((interval.value, interval.timescale), (1, 120));
            }
        });
        let _ = output_class();
    }

    #[test]
    fn display_list_without_permission() {
        let list = display_list();
        assert!(!list.is_empty());
        assert_eq!(list[0].id, unsafe { CGMainDisplayID() }, "main display first");
        for d in &list {
            assert!(d.width > 0 && d.height > 0 && !d.name.is_empty(), "{d:?}");
            eprintln!("display: {d:?}");
        }
        let mut ids: Vec<u32> = list.iter().map(|d| d.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), list.len());
    }

    #[test]
    fn duplicate_display_names_are_numbered() {
        let d = |id, name: &str| DisplayInfo { id, name: name.into(), width: 1, height: 1, x: 0, y: 0 };
        let mut list = vec![d(1, "Built-in"), d(2, "DELL"), d(3, "DELL")];
        dedupe_names(&mut list);
        let names: Vec<&str> = list.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Built-in", "DELL (1)", "DELL (2)"]);
    }

    #[test]
    fn shared_display_bounds_follow_switch() {
        use crate::input::Bounds;
        let main = unsafe { CGMainDisplayID() };
        let b = DisplayBounds::new(0);
        let sink_side = b.clone();
        b.set(main);
        assert_eq!(sink_side.id(), main);
        assert_eq!(sink_side.rect(), unsafe { CGDisplayBounds(main) });
    }
}
