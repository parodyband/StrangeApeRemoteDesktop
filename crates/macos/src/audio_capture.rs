//! System audio capture ("what you hear") with ScreenCaptureKit.
//!
//! A separate SCStream on the main display with a tiny 1 fps video
//! configuration and `capturesAudio`; its audio output delivers 48 kHz
//! float32 buffers (usually non-interleaved), converted here to interleaved
//! stereo s16 for the host's [`AudioCallback`]. Sard's own audio is excluded.
//! Needs the Screen (& System Audio) Recording permission.

use crate::capture::{SCREEN_RECORDING_HINT, find_display, shareable_content, stream_config, wait_done};
use crate::cf::{self, CfRef};
use crate::ffi::*;
use crate::objc::error_string;
use objc2::rc::{Allocated, Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, ClassBuilder, NSObject, Sel};
use objc2::{ClassType, class, msg_send, sel};
use sard_core::media::{AUDIO_CHANNELS, AUDIO_RATE, AudioCallback, AudioSource};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// `SCStreamOutputType`.
const OUTPUT_SCREEN: isize = 0;
const OUTPUT_AUDIO: isize = 1;

/// Sample encoding of delivered PCM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleKind {
    F32,
    I16,
    I32,
}

impl SampleKind {
    fn bytes(self) -> usize {
        match self {
            SampleKind::I16 => 2,
            SampleKind::F32 | SampleKind::I32 => 4,
        }
    }

    #[inline]
    fn to_i16(self, b: &[u8]) -> i16 {
        match self {
            // Symmetric full scale; NaN stays NaN through clamp and `as` maps it to 0.
            SampleKind::F32 => (f32::from_ne_bytes([b[0], b[1], b[2], b[3]]).clamp(-1.0, 1.0) * 32767.0).round() as i16,
            SampleKind::I16 => i16::from_ne_bytes([b[0], b[1]]),
            SampleKind::I32 => (i32::from_ne_bytes([b[0], b[1], b[2], b[3]]) >> 16) as i16,
        }
    }
}

/// Layout of the PCM in a sample buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    pub sample: SampleKind,
    pub channels: usize,
    /// One buffer with all channels interleaved (else one buffer per channel).
    pub interleaved: bool,
}

impl PcmFormat {
    /// Native-endian linear PCM we can convert, from a stream description.
    pub fn from_asbd(a: &AudioStreamBasicDescription) -> Option<PcmFormat> {
        let f = a.format_flags;
        if a.format_id != kAudioFormatLinearPCM || f & kAudioFormatFlagIsBigEndian != 0 || a.channels_per_frame == 0 {
            return None;
        }
        let sample = match (f & kAudioFormatFlagIsFloat != 0, a.bits_per_channel) {
            (true, 32) => SampleKind::F32,
            (false, 16) if f & kAudioFormatFlagIsSignedInteger != 0 => SampleKind::I16,
            (false, 32) if f & kAudioFormatFlagIsSignedInteger != 0 => SampleKind::I32,
            _ => return None,
        };
        Some(PcmFormat {
            sample,
            channels: a.channels_per_frame as usize,
            interleaved: f & kAudioFormatFlagIsNonInterleaved == 0,
        })
    }
}

/// Append `frames` frames of `planes` (one interleaved buffer, or one buffer
/// per channel) to `out` as interleaved stereo s16. Mono is duplicated to
/// both sides, channels beyond the first two are dropped, and short buffers
/// limit the frame count instead of being over-read.
pub fn to_stereo_s16(fmt: &PcmFormat, planes: &[&[u8]], frames: usize, out: &mut Vec<i16>) {
    let bytes = fmt.sample.bytes();
    let sample = |p: &[u8], i: usize| fmt.sample.to_i16(&p[i * bytes..(i + 1) * bytes]);
    if fmt.interleaved {
        let (Some(p), ch) = (planes.first(), fmt.channels.max(1)) else { return };
        let r = if ch >= 2 { 1 } else { 0 };
        let n = frames.min(p.len() / (bytes * ch));
        out.reserve(n * AUDIO_CHANNELS);
        for i in 0..n {
            out.push(sample(p, i * ch));
            out.push(sample(p, i * ch + r));
        }
    } else {
        let ch = fmt.channels.min(planes.len());
        if ch == 0 {
            return;
        }
        let (l, r) = (planes[0], planes[if ch >= 2 { 1 } else { 0 }]);
        let n = frames.min(l.len() / bytes).min(r.len() / bytes);
        out.reserve(n * AUDIO_CHANNELS);
        for i in 0..n {
            out.push(sample(l, i));
            out.push(sample(r, i));
        }
    }
}

struct State {
    cb: AudioCallback,
    out: Vec<i16>,
    warned: bool,
}

struct Shared {
    state: Mutex<State>,
}

/// Output objects → their state (late callbacks after teardown find nothing).
fn registry() -> &'static Mutex<HashMap<usize, Arc<Shared>>> {
    static R: OnceLock<Mutex<HashMap<usize, Arc<Shared>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

fn lookup(this: *mut AnyObject) -> Option<Arc<Shared>> {
    registry().lock().ok()?.get(&(this as usize)).cloned()
}

/// Most buffers a sample buffer may split its audio into (one per channel).
const MAX_BUFFERS: usize = 8;

/// Convert one audio sample buffer and hand it to the callback.
///
/// # Safety
/// `sbuf` must be a valid `CMSampleBuffer`.
unsafe fn deliver(sbuf: CMSampleBufferRef, st: &mut State) {
    // SAFETY: valid sample buffer (caller); getters return +0 references, the
    // block buffer is +1 and keeps the data alive while we read it.
    unsafe {
        let frames = CMSampleBufferGetNumSamples(sbuf);
        let desc = CMSampleBufferGetFormatDescription(sbuf);
        if frames <= 0 || desc.is_null() {
            return;
        }
        let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(desc);
        let fmt = if asbd.is_null() { None } else { PcmFormat::from_asbd(&*asbd) };
        let Some(fmt) = fmt else {
            if !std::mem::replace(&mut st.warned, true) {
                sard_core::warn!("audio capture: unsupported sample format {:?}", asbd.as_ref());
            }
            return;
        };
        if (*asbd).sample_rate as u32 != AUDIO_RATE && !std::mem::replace(&mut st.warned, true) {
            sard_core::warn!("audio capture: got {} Hz instead of {AUDIO_RATE} Hz", (*asbd).sample_rate);
        }
        // AudioBufferList: u32 count (padded to 8 bytes) + AudioBuffer[count].
        // CoreMedia wants exactly the size it needs, so ask first.
        let mut abl = [0u64; 1 + MAX_BUFFERS * 2];
        let mut need = 0usize;
        let mut block: CMBlockBufferRef = null_mut();
        let mut status = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sbuf,
            &mut need,
            null_mut(),
            0,
            std::ptr::null(),
            std::ptr::null(),
            0,
            null_mut(),
        );
        if status == 0 && (need < 8 || need > std::mem::size_of_val(&abl)) {
            status = -12737; // kCMSampleBufferError_ArrayTooSmall: more buffers than we handle
        }
        if status == 0 {
            status = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
                sbuf,
                null_mut(),
                abl.as_mut_ptr().cast(),
                need,
                std::ptr::null(),
                std::ptr::null(),
                kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
                &mut block,
            );
        }
        if status != 0 {
            if !std::mem::replace(&mut st.warned, true) {
                sard_core::warn!("audio capture: cannot read sample buffer ({})", cf::os_status(status));
            }
            return;
        }
        let _block = CfRef::from_owned(block);
        let count = (abl.as_ptr() as *const u32).read() as usize;
        let buffers = abl.as_ptr().add(1) as *const AudioBuffer;
        let mut planes: [&[u8]; MAX_BUFFERS] = [&[]; MAX_BUFFERS];
        for (i, plane) in planes.iter_mut().enumerate().take(count.min(MAX_BUFFERS)) {
            let b = buffers.add(i).read();
            if !b.data.is_null() {
                *plane = std::slice::from_raw_parts(b.data as *const u8, b.data_byte_size as usize);
            }
        }
        st.out.clear();
        to_stereo_s16(&fmt, &planes[..count.min(MAX_BUFFERS)], frames as usize, &mut st.out);
        if !st.out.is_empty() {
            (st.cb)(&st.out);
        }
    }
}

extern "C-unwind" fn did_output(
    this: *mut AnyObject,
    _sel: Sel,
    _stream: *mut AnyObject,
    sbuf: *mut c_void,
    kind: isize,
) {
    if kind != OUTPUT_AUDIO || sbuf.is_null() {
        return; // video frames of the audio-only stream are ignored
    }
    let Some(shared) = lookup(this) else { return };
    let Ok(mut st) = shared.state.lock() else { return };
    // SAFETY: SCK passes a valid sample buffer for the duration of the callback.
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { deliver(sbuf, &mut st) }));
    if r.is_err() {
        sard_core::error!("audio capture: panic in audio callback");
    }
}

extern "C-unwind" fn did_stop(this: *mut AnyObject, _sel: Sel, _stream: *mut AnyObject, error: *mut AnyObject) {
    if lookup(this).is_some() {
        // SAFETY: nil or NSError.
        sard_core::warn!("audio capture stopped: {}", unsafe { error_string(error) });
    }
}

fn output_class() -> &'static AnyClass {
    static CLS: OnceLock<&'static AnyClass> = OnceLock::new();
    CLS.get_or_init(|| {
        let mut b = ClassBuilder::new(c"SardAudioOutput", NSObject::class()).expect("SardAudioOutput already defined");
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

/// SCStreamConfiguration for audio-only capture: 48 kHz stereo, own process
/// excluded, minimal video (64x36 at 1 fps). Needs no permission.
pub fn audio_stream_config() -> Retained<AnyObject> {
    let cfg = stream_config(64, 36, 1);
    // SAFETY: plain property setters (macOS 13+).
    unsafe {
        let _: () = msg_send![&*cfg, setShowsCursor: false];
        let _: () = msg_send![&*cfg, setQueueDepth: 3isize];
        let _: () = msg_send![&*cfg, setCapturesAudio: true];
        let _: () = msg_send![&*cfg, setSampleRate: AUDIO_RATE as isize];
        let _: () = msg_send![&*cfg, setChannelCount: AUDIO_CHANNELS as isize];
        let _: () = msg_send![&*cfg, setExcludesCurrentProcessAudio: true];
    }
    cfg
}

pub struct ScreenAudioSource {
    stream: Retained<AnyObject>,
    output: Retained<AnyObject>,
    queue: *mut c_void,
}

// SAFETY: SCStream's methods are thread-safe; callbacks only touch `Shared`.
unsafe impl Send for ScreenAudioSource {}

impl ScreenAudioSource {
    pub fn new(cb: AudioCallback) -> Result<Self, String> {
        let content = shareable_content().map_err(|e| format!("{e} (needed for system audio)"))?;
        // SAFETY: `content` is an SCShareableContent; plain CG query.
        let display = unsafe { find_display(&content, CGMainDisplayID()) }.ok_or("no display to attach audio to")?;
        let shared = Arc::new(Shared { state: Mutex::new(State { cb, out: Vec::with_capacity(4096), warned: false }) });
        // SAFETY: standard SCK object setup; `output` stays registered until Drop.
        unsafe {
            let output: Retained<AnyObject> = msg_send![output_class(), new];
            registry().lock().unwrap().insert(Retained::as_ptr(&output) as usize, shared);

            let empty: Retained<AnyObject> = msg_send![class!(NSArray), array];
            let f: Allocated<AnyObject> = msg_send![class!(SCContentFilter), alloc];
            let filter: Retained<AnyObject> = msg_send![f, initWithDisplay: &*display, excludingWindows: &*empty];
            let cfg = audio_stream_config();
            let s: Allocated<AnyObject> = msg_send![class!(SCStream), alloc];
            let stream: Retained<AnyObject> =
                msg_send![s, initWithFilter: &*filter, configuration: &*cfg, delegate: &*output];
            let attr = dispatch_queue_attr_make_with_qos_class(std::ptr::null(), QOS_CLASS_USER_INTERACTIVE, 0);
            let queue = dispatch_queue_create(c"com.strangeape.sard.audio".as_ptr(), attr);
            let src = ScreenAudioSource { stream, output, queue };
            // The (ignored) screen output keeps SCK from logging dropped frames.
            for kind in [OUTPUT_AUDIO, OUTPUT_SCREEN] {
                let mut err: *mut AnyObject = null_mut();
                let ok: bool = msg_send![
                    &*src.stream,
                    addStreamOutput: &*src.output,
                    type: kind,
                    sampleHandlerQueue: queue as *mut AnyObject,
                    error: &mut err
                ];
                if !ok {
                    return Err(format!("addStreamOutput({kind}) failed: {}", error_string(err)));
                }
            }
            wait_done(
                |block| {
                    let _: () = msg_send![&*src.stream, startCaptureWithCompletionHandler: block];
                },
                Duration::from_secs(15),
            )
            .map_err(|e| format!("cannot start audio capture: {e}. {SCREEN_RECORDING_HINT}"))?;
            Ok(src)
        }
    }
}

impl AudioSource for ScreenAudioSource {
    fn describe(&self) -> String {
        format!("ScreenCaptureKit system audio ({} kHz stereo)", AUDIO_RATE / 1000)
    }
}

impl Drop for ScreenAudioSource {
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
                    sard_core::debug!("audio stopCapture: {e}");
                }
                for kind in [OUTPUT_AUDIO, OUTPUT_SCREEN] {
                    let mut err: *mut AnyObject = null_mut();
                    let _: bool =
                        msg_send![&*self.stream, removeStreamOutput: &*self.output, type: kind, error: &mut err];
                }
            }
        });
        if let Ok(mut r) = registry().lock() {
            r.remove(&(Retained::as_ptr(&self.output) as usize));
        }
        // SAFETY: our queue; SCK has dropped its use of it after removal.
        unsafe { dispatch_release(self.queue) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32_bytes(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_ne_bytes()).collect()
    }

    const F32_PLANAR: PcmFormat = PcmFormat { sample: SampleKind::F32, channels: 2, interleaved: false };

    #[test]
    fn planar_float_to_interleaved_s16() {
        let l = f32_bytes(&[0.0, 0.5, 1.0, -1.0, 2.0]);
        let r = f32_bytes(&[-0.5, 0.25, -2.0, f32::NAN, 1e-6]);
        let mut out = vec![7i16]; // appends
        to_stereo_s16(&F32_PLANAR, &[&l, &r], 5, &mut out);
        assert_eq!(out, [7, 0, -16384, 16384, 8192, 32767, -32767, -32767, 0, 32767, 0]);
    }

    #[test]
    fn interleaved_mono_multichannel_and_short_buffers() {
        // Interleaved float stereo.
        let fmt = PcmFormat { sample: SampleKind::F32, channels: 2, interleaved: true };
        let mut out = Vec::new();
        to_stereo_s16(&fmt, &[&f32_bytes(&[0.5, -0.5, 0.25, -0.25])], 2, &mut out);
        assert_eq!(out, [16384, -16384, 8192, -8192]);
        // Mono planar is duplicated.
        let fmt = PcmFormat { sample: SampleKind::F32, channels: 1, interleaved: false };
        out.clear();
        to_stereo_s16(&fmt, &[&f32_bytes(&[0.5, -1.0])], 2, &mut out);
        assert_eq!(out, [16384, 16384, -32767, -32767]);
        // 4-channel interleaved s16 keeps the first two channels.
        let fmt = PcmFormat { sample: SampleKind::I16, channels: 4, interleaved: true };
        let data: Vec<u8> = [1i16, 2, 3, 4, 5, 6, 7, 8].iter().flat_map(|x| x.to_ne_bytes()).collect();
        out.clear();
        to_stereo_s16(&fmt, &[&data], 2, &mut out);
        assert_eq!(out, [1, 2, 5, 6]);
        // Frame count larger than the data: no over-read.
        out.clear();
        to_stereo_s16(&F32_PLANAR, &[&f32_bytes(&[0.0; 3]), &f32_bytes(&[0.0; 2])], 100, &mut out);
        assert_eq!(out.len(), 4);
        // Missing right plane: left is duplicated; no planes: nothing.
        out.clear();
        to_stereo_s16(&F32_PLANAR, &[&f32_bytes(&[1.0])], 1, &mut out);
        assert_eq!(out, [32767, 32767]);
        out.clear();
        to_stereo_s16(&F32_PLANAR, &[], 10, &mut out);
        assert!(out.is_empty());
        // s32 keeps the high 16 bits.
        let fmt = PcmFormat { sample: SampleKind::I32, channels: 2, interleaved: true };
        let data: Vec<u8> = [0x4000_0000i32, i32::MIN].iter().flat_map(|x| x.to_ne_bytes()).collect();
        out.clear();
        to_stereo_s16(&fmt, &[&data], 1, &mut out);
        assert_eq!(out, [0x4000, -0x8000]);
    }

    fn asbd(flags: u32, bits: u32, channels: u32) -> AudioStreamBasicDescription {
        let bpf = if flags & kAudioFormatFlagIsNonInterleaved != 0 { bits / 8 } else { bits / 8 * channels };
        AudioStreamBasicDescription {
            sample_rate: 48_000.0,
            format_id: kAudioFormatLinearPCM,
            format_flags: flags,
            bytes_per_packet: bpf,
            frames_per_packet: 1,
            bytes_per_frame: bpf,
            channels_per_frame: channels,
            bits_per_channel: bits,
            reserved: 0,
        }
    }

    const PACKED: u32 = 1 << 3;

    #[test]
    fn format_from_stream_description() {
        let planar = kAudioFormatFlagIsFloat | PACKED | kAudioFormatFlagIsNonInterleaved;
        assert_eq!(PcmFormat::from_asbd(&asbd(planar, 32, 2)), Some(F32_PLANAR));
        let s16 = PcmFormat::from_asbd(&asbd(kAudioFormatFlagIsSignedInteger | PACKED, 16, 2)).unwrap();
        assert_eq!((s16.sample, s16.interleaved), (SampleKind::I16, true));
        assert_eq!(PcmFormat::from_asbd(&asbd(kAudioFormatFlagIsFloat | PACKED, 64, 2)), None);
        assert_eq!(PcmFormat::from_asbd(&asbd(PACKED, 16, 2)), None, "unsigned");
        assert_eq!(PcmFormat::from_asbd(&asbd(planar | kAudioFormatFlagIsBigEndian, 32, 2)), None);
        let mut aac = asbd(planar, 32, 2);
        aac.format_id = u32::from_be_bytes(*b"aac ");
        assert_eq!(PcmFormat::from_asbd(&aac), None);
    }

    #[test]
    fn audio_configuration_without_permission() {
        autoreleasepool(|_| {
            let cfg = audio_stream_config();
            // SAFETY: SCStreamConfiguration getters.
            unsafe {
                let audio: bool = msg_send![&*cfg, capturesAudio];
                let rate: isize = msg_send![&*cfg, sampleRate];
                let ch: isize = msg_send![&*cfg, channelCount];
                let excl: bool = msg_send![&*cfg, excludesCurrentProcessAudio];
                assert_eq!((audio, rate, ch, excl), (true, 48_000, 2, true));
            }
        });
    }

    // ---- a real CMSampleBuffer through the ObjC output callback ----

    #[link(name = "CoreMedia", kind = "framework")]
    unsafe extern "C" {
        fn CMAudioFormatDescriptionCreate(
            alloc: cf::CFAllocatorRef,
            asbd: *const AudioStreamBasicDescription,
            layout_size: usize,
            layout: *const c_void,
            cookie_size: usize,
            cookie: *const c_void,
            extensions: cf::CFDictionaryRef,
            out: *mut CMFormatDescriptionRef,
        ) -> cf::OSStatus;
        fn CMAudioSampleBufferCreateWithPacketDescriptions(
            alloc: cf::CFAllocatorRef,
            data: CMBlockBufferRef,
            data_ready: cf::Boolean,
            make_ready: *const c_void,
            refcon: *const c_void,
            format: CMFormatDescriptionRef,
            num_samples: isize,
            pts: CMTime,
            packet_descs: *const c_void,
            out: *mut CMSampleBufferRef,
        ) -> cf::OSStatus;
        fn CMSampleBufferSetDataBufferFromAudioBufferList(
            sbuf: CMSampleBufferRef,
            block_alloc: cf::CFAllocatorRef,
            memory_alloc: cf::CFAllocatorRef,
            flags: u32,
            abl: *const c_void,
        ) -> cf::OSStatus;
    }

    /// A planar float32 stereo sample buffer like the ones SCK delivers.
    fn planar_sample_buffer(left: &[f32], right: &[f32]) -> CfRef {
        let frames = left.len();
        let desc = asbd(kAudioFormatFlagIsFloat | PACKED | kAudioFormatFlagIsNonInterleaved, 32, 2);
        // SAFETY: CoreMedia constructors with valid inputs; the buffer list
        // points at `left`/`right`, which CoreMedia copies.
        unsafe {
            let mut fmt: CMFormatDescriptionRef = null_mut();
            let st = CMAudioFormatDescriptionCreate(
                std::ptr::null(),
                &desc,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null(),
                &mut fmt,
            );
            assert_eq!(st, 0, "CMAudioFormatDescriptionCreate");
            let fmt = CfRef::from_owned(fmt).unwrap();
            let mut sbuf: CMSampleBufferRef = null_mut();
            let st = CMAudioSampleBufferCreateWithPacketDescriptions(
                std::ptr::null(),
                null_mut(),
                0,
                std::ptr::null(),
                std::ptr::null(),
                fmt.as_ptr(),
                frames as isize,
                CMTime::new(0, 48_000),
                std::ptr::null(),
                &mut sbuf,
            );
            assert_eq!(st, 0, "CMAudioSampleBufferCreateWithPacketDescriptions");
            let sbuf = CfRef::from_owned(sbuf).unwrap();
            #[repr(C)]
            struct Abl2 {
                count: u32,
                buffers: [AudioBuffer; 2],
            }
            let ab = |s: &[f32]| AudioBuffer {
                number_channels: 1,
                data_byte_size: (s.len() * 4) as u32,
                data: s.as_ptr() as *mut c_void,
            };
            let abl = Abl2 { count: 2, buffers: [ab(left), ab(right)] };
            let st = CMSampleBufferSetDataBufferFromAudioBufferList(
                sbuf.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                (&abl as *const Abl2).cast(),
            );
            assert_eq!(st, 0, "CMSampleBufferSetDataBufferFromAudioBufferList");
            sbuf
        }
    }

    #[test]
    fn sample_buffer_through_output_callback() {
        let got: Arc<Mutex<Vec<i16>>> = Arc::default();
        let sink = Arc::clone(&got);
        let cb: AudioCallback = Box::new(move |s: &[i16]| sink.lock().unwrap().extend_from_slice(s));
        let shared = Arc::new(Shared { state: Mutex::new(State { cb, out: Vec::new(), warned: false }) });
        let left: Vec<f32> = (0..480).map(|i| i as f32 / 480.0).collect();
        let right: Vec<f32> = left.iter().map(|v| -v).collect();
        let sbuf = planar_sample_buffer(&left, &right);
        autoreleasepool(|_| {
            // SAFETY: our registered class; the method is called the way SCK calls it.
            unsafe {
                let output: Retained<AnyObject> = msg_send![output_class(), new];
                let key = Retained::as_ptr(&output) as usize;
                registry().lock().unwrap().insert(key, shared);
                let nil: *mut AnyObject = null_mut();
                let _: () =
                    msg_send![&*output, stream: nil, didOutputSampleBuffer: sbuf.as_ptr(), ofType: OUTPUT_SCREEN];
                assert!(got.lock().unwrap().is_empty(), "screen buffers are ignored");
                let _: () =
                    msg_send![&*output, stream: nil, didOutputSampleBuffer: sbuf.as_ptr(), ofType: OUTPUT_AUDIO];
                registry().lock().unwrap().remove(&key);
            }
        });
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 480 * 2);
        for i in [0usize, 1, 240, 479] {
            let v = (i as f32 / 480.0 * 32767.0).round() as i16;
            assert_eq!((got[2 * i], got[2 * i + 1]), (v, -v), "frame {i}");
        }
    }
}
