//! VideoToolbox H.264 / HEVC encode and decode, speaking Annex-B on the
//! outside (see `annexb`), plus CVPixelBuffer helpers.

use crate::annexb::{self, NalCodec};
use crate::cf::{self, CfRef, Dict, kCFAllocatorDefault};
use crate::ffi::*;
use sard_core::proto::Codec;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, String>;

pub fn codec_type(c: Codec) -> u32 {
    match c {
        Codec::H264 => kCMVideoCodecType_H264,
        Codec::Hevc => kCMVideoCodecType_HEVC,
        Codec::Av1 => kCMVideoCodecType_AV1,
    }
}

fn nal_codec(c: Codec) -> Result<NalCodec> {
    match c {
        Codec::H264 => Ok(NalCodec::H264),
        Codec::Hevc => Ok(NalCodec::Hevc),
        Codec::Av1 => Err("AV1 is not supported by the VideoToolbox backend".into()),
    }
}

/// A VT session that is invalidated before its last reference goes away.
struct Session {
    r: CfRef,
    compression: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: valid session; invalidation stops further callbacks.
        unsafe {
            if self.compression {
                VTCompressionSessionInvalidate(self.r.as_ptr());
            } else {
                VTDecompressionSessionInvalidate(self.r.as_ptr());
            }
        }
    }
}

fn set_prop(session: *mut c_void, key: cf::CFStringRef, value: *const c_void, name: &str) -> bool {
    // SAFETY: valid session, key and value.
    let st = unsafe { VTSessionSetProperty(session, key, value) };
    if st != 0 {
        sard_core::debug!("VideoToolbox: {name} not applied: {}", cf::os_status(st));
    }
    st == 0
}

fn get_bool_prop(session: *mut c_void, key: cf::CFStringRef) -> Option<bool> {
    let mut v: *const c_void = null();
    // SAFETY: valid session; the returned value is +1.
    unsafe {
        if VTSessionCopyProperty(session, key, kCFAllocatorDefault, &mut v as *mut _ as *mut c_void) != 0 {
            return None;
        }
        let v = CfRef::from_owned(v)?;
        cf::number_value(v.as_ptr()).map(|n| n != 0)
    }
}

/// Attributes for IOSurface-backed, Metal-compatible NV12 (video range) buffers.
fn nv12_attrs(size: Option<(u32, u32)>) -> Dict {
    // SAFETY: reading immutable framework constants.
    unsafe {
        let mut d = Dict::new()
            .set_ref(kCVPixelBufferPixelFormatTypeKey, &cf::i32(kCVPixelFormatType_420v as i32))
            .set(kCVPixelBufferIOSurfacePropertiesKey, Dict::new().as_ptr())
            .set(kCVPixelBufferMetalCompatibilityKey, cf::boolean(true));
        if let Some((w, h)) = size {
            d = d
                .set_ref(kCVPixelBufferWidthKey, &cf::i32(w as i32))
                .set_ref(kCVPixelBufferHeightKey, &cf::i32(h as i32));
        }
        d
    }
}

// ------------------------------------------------------------------ encoder

/// How VideoToolbox should trade quality for latency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateControl {
    /// `EnableLowLatencyRateControl`: steady frame sizes and the best quality
    /// per bit, but `PrioritizeEncodingSpeedOverQuality` is not available.
    LowLatency,
    /// Normal rate control with `PrioritizeEncodingSpeedOverQuality`. On
    /// Apple Silicon HEVC this encodes ~40% faster (M5 Pro, 1440p: 4.1 vs
    /// 7.1 ms; 4K: 8.3 vs 14.7 ms) at a lower quality per bit.
    Speed,
}

impl RateControl {
    /// Policy for the host's `--preset` (1 = fastest, the default). H.264
    /// encodes equally fast either way, so it always uses low-latency RC.
    pub fn for_preset(codec: Codec, preset: u8) -> Self {
        if codec == Codec::Hevc && preset <= 1 { RateControl::Speed } else { RateControl::LowLatency }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub rate_control: RateControl,
}

/// One encoded access unit in Annex-B form.
#[derive(Debug)]
pub struct Encoded {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

type EncResult = Result<Option<Encoded>>;

struct EncSlot {
    codec: NalCodec,
    out: Mutex<Option<EncResult>>,
    cv: Condvar,
}

pub struct VtEncoder {
    // Field order matters: the session must be invalidated before `slot` is freed.
    session: Session,
    slot: Box<EncSlot>,
    cfg: EncoderConfig,
    force_key: CfRef,
    last_pts: i64,
    pub low_latency: bool,
    pub hardware: bool,
}

// SAFETY: the session and its callback slot are only driven from the thread
// that owns the encoder; the callback synchronizes through the slot mutex.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        let nal = nal_codec(cfg.codec)?;
        let slot = Box::new(EncSlot { codec: nal, out: Mutex::new(None), cv: Condvar::new() });
        let source = nv12_attrs(Some((cfg.width, cfg.height)));
        // (low-latency RC, require hardware), best first; always fall back to
        // whatever encoder exists.
        let attempts: &[(bool, bool)] = match cfg.rate_control {
            RateControl::LowLatency => &[(true, true), (false, true), (false, false)],
            RateControl::Speed => &[(false, true), (true, true), (false, false)],
        };
        let mut last_err = String::new();
        let mut created = None;
        for &(low_latency, require_hw) in attempts {
            // SAFETY: framework constants.
            let spec = unsafe {
                let mut d = Dict::new();
                if low_latency {
                    d = d.set(kVTVideoEncoderSpecification_EnableLowLatencyRateControl, cf::boolean(true));
                }
                if require_hw {
                    d = d.set(kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, cf::boolean(true));
                }
                d
            };
            let mut s: VTCompressionSessionRef = null_mut();
            // SAFETY: valid arguments; `slot` outlives the session (see field order).
            let st = unsafe {
                VTCompressionSessionCreate(
                    kCFAllocatorDefault,
                    cfg.width as i32,
                    cfg.height as i32,
                    codec_type(cfg.codec),
                    spec.as_ptr(),
                    source.as_ptr(),
                    null(),
                    Some(encode_callback),
                    &*slot as *const EncSlot as *mut c_void,
                    &mut s,
                )
            };
            if st == 0 && !s.is_null() {
                // SAFETY: +1 session from Create.
                let r = unsafe { CfRef::from_owned(s) }.unwrap();
                created = Some((Session { r, compression: true }, low_latency, require_hw));
                break;
            }
            last_err = format!(
                "VTCompressionSessionCreate({} {}x{}, low_latency={low_latency}, hw={require_hw}): {}",
                cfg.codec.name(),
                cfg.width,
                cfg.height,
                cf::os_status(st)
            );
            sard_core::debug!("{last_err}");
        }
        let Some((session, low_latency, required_hw)) = created else { return Err(last_err) };
        let s = session.r.as_ptr();
        // SAFETY: framework constants; values are valid CF objects.
        unsafe {
            set_prop(s, kVTCompressionPropertyKey_RealTime, cf::boolean(true), "RealTime");
            set_prop(s, kVTCompressionPropertyKey_AllowFrameReordering, cf::boolean(false), "AllowFrameReordering");
            set_prop(
                s,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                cf::i32(i32::MAX).as_ptr(),
                "MaxKeyFrameInterval",
            );
            set_prop(s, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, cf::f64(0.0).as_ptr(), "MaxKFDuration");
            set_prop(s, kVTCompressionPropertyKey_ExpectedFrameRate, cf::i32(cfg.fps as i32).as_ptr(), "FrameRate");
            if !low_latency {
                // (Not supported with low-latency RC, which never delays frames anyway.)
                set_prop(
                    s,
                    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                    cf::boolean(true),
                    "SpeedOverQ",
                );
                set_prop(s, kVTCompressionPropertyKey_MaxFrameDelayCount, cf::i32(0).as_ptr(), "MaxFrameDelayCount");
                let profile = match cfg.codec {
                    Codec::H264 => kVTProfileLevel_H264_High_AutoLevel,
                    _ => kVTProfileLevel_HEVC_Main_AutoLevel,
                };
                set_prop(s, kVTCompressionPropertyKey_ProfileLevel, profile, "ProfileLevel");
            }
            set_prop(
                s,
                kVTCompressionPropertyKey_ColorPrimaries,
                kCVImageBufferColorPrimaries_ITU_R_709_2,
                "primaries",
            );
            set_prop(
                s,
                kVTCompressionPropertyKey_TransferFunction,
                kCVImageBufferTransferFunction_ITU_R_709_2,
                "transfer",
            );
            set_prop(s, kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2, "matrix");
        }
        // (The query is unsupported in low-latency mode; then `required_hw` tells.)
        let hardware = required_hw
            || get_bool_prop(s, unsafe { kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder })
                .unwrap_or(false);
        // SAFETY: framework constant.
        let force_key = unsafe { Dict::new().set(kVTEncodeFrameOptionKey_ForceKeyFrame, cf::boolean(true)).into_ref() };
        let mut enc = VtEncoder { session, slot, cfg, force_key, last_pts: -1, low_latency, hardware };
        enc.set_bitrate(cfg.bitrate_kbps);
        // SAFETY: valid session.
        cf::check(unsafe { VTCompressionSessionPrepareToEncodeFrames(s) }, "PrepareToEncodeFrames")?;
        Ok(enc)
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.cfg
    }

    pub fn set_bitrate(&mut self, kbps: u32) {
        let kbps = kbps.max(500);
        self.cfg.bitrate_kbps = kbps;
        let s = self.session.r.as_ptr();
        let bps = kbps as i64 * 1000;
        // Hard cap: 1.5x the average over any 500 ms window (keeps bursts,
        // e.g. keyframes, from building a queue in the network path).
        let window = 0.5;
        let limit = cf::array(&[cf::i64((bps as f64 / 8.0 * 1.5 * window) as i64).as_ptr(), cf::f64(window).as_ptr()]);
        // SAFETY: framework constants, valid values.
        unsafe {
            set_prop(s, kVTCompressionPropertyKey_AverageBitRate, cf::i64(bps).as_ptr(), "AverageBitRate");
            set_prop(s, kVTCompressionPropertyKey_DataRateLimits, limit.as_ptr(), "DataRateLimits");
        }
    }

    /// New pixel buffer from the encoder's own pool (IOSurface-backed NV12).
    pub fn pixel_buffer(&self) -> Result<CfRef> {
        // SAFETY: valid session; the pool is owned by the session (+0).
        unsafe {
            let pool = VTCompressionSessionGetPixelBufferPool(self.session.r.as_ptr());
            if pool.is_null() {
                return new_pixel_buffer(self.cfg.width, self.cfg.height);
            }
            let mut pb: CVPixelBufferRef = null_mut();
            let st = CVPixelBufferPoolCreatePixelBuffer(kCFAllocatorDefault, pool, &mut pb);
            cf::check(st, "CVPixelBufferPoolCreatePixelBuffer")?;
            CfRef::from_owned(pb).ok_or_else(|| "null pixel buffer".into())
        }
    }

    /// Encode one picture and wait for its bitstream (no queueing).
    /// `time_us` is the capture time; it drives rate control.
    pub fn encode(&mut self, image: &CfRef, time_us: u64, force_key: bool) -> EncResult {
        *self.slot.out.lock().unwrap() = None;
        let pts = (time_us as i64).max(self.last_pts + 1);
        self.last_pts = pts;
        let props = if force_key { self.force_key.as_ptr() as *const c_void } else { null() };
        let mut info = 0u32;
        // SAFETY: valid session and image buffer.
        let st = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session.r.as_ptr(),
                image.as_ptr(),
                CMTime::new(pts, 1_000_000),
                CMTime::INVALID,
                props,
                null_mut(),
                &mut info,
            )
        };
        cf::check(st, "VTCompressionSessionEncodeFrame")?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut g = self.slot.out.lock().unwrap();
        loop {
            if let Some(r) = g.take() {
                return r;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(g);
                // Should not happen in real-time mode; flush so the frame comes out.
                // SAFETY: valid session.
                unsafe { VTCompressionSessionCompleteFrames(self.session.r.as_ptr(), CMTime::INVALID) };
                return self.slot.out.lock().unwrap().take().unwrap_or(Err("encoder produced no output".into()));
            }
            g = self.slot.cv.wait_timeout(g, left).unwrap().0;
        }
    }
}

unsafe extern "C" fn encode_callback(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: cf::OSStatus,
    info: u32,
    sbuf: CMSampleBufferRef,
) {
    // SAFETY: refcon is the encoder's boxed slot, alive while the session is.
    let slot = unsafe { &*(refcon as *const EncSlot) };
    let r = if status != 0 {
        Err(format!("encode failed: {}", cf::os_status(status)))
    } else if sbuf.is_null() || info & kVTEncodeInfo_FrameDropped != 0 {
        Ok(None)
    } else {
        // SAFETY: valid sample buffer for the duration of the callback.
        unsafe { sample_to_annexb(slot.codec, sbuf) }.map(Some)
    };
    if let Ok(mut g) = slot.out.lock() {
        *g = Some(r);
    }
    slot.cv.notify_all();
}

/// Is this VT sample a sync (key) frame?
///
/// # Safety
/// `sbuf` must be a valid sample buffer.
unsafe fn sample_is_sync(sbuf: CMSampleBufferRef) -> bool {
    // SAFETY: caller contract; attachments are +0.
    unsafe {
        let arr = CMSampleBufferGetSampleAttachmentsArray(sbuf, 0);
        if arr.is_null() || cf::CFArrayGetCount(arr) == 0 {
            return true;
        }
        let d = cf::CFArrayGetValueAtIndex(arr, 0);
        let not_sync = cf::CFDictionaryGetValue(d, kCMSampleAttachmentKey_NotSync);
        cf::number_value(not_sync).unwrap_or(0) == 0
    }
}

/// Parameter sets (VPS/SPS/PPS) and NAL length size of a format description.
///
/// # Safety
/// `fd` must be a valid H.264/HEVC format description; the returned slices
/// borrow from it.
pub unsafe fn parameter_sets<'a>(codec: NalCodec, fd: CMFormatDescriptionRef) -> Result<(Vec<&'a [u8]>, usize)> {
    let get = match codec {
        NalCodec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
        NalCodec::Hevc => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
    };
    let mut count = 0usize;
    let mut nal_len = 4i32;
    // SAFETY: caller contract; out-pointers valid.
    unsafe {
        cf::check(get(fd, 0, null_mut(), null_mut(), &mut count, &mut nal_len), "get parameter set count")?;
        let mut sets = Vec::with_capacity(count);
        for i in 0..count {
            let (mut p, mut n) = (null(), 0usize);
            cf::check(get(fd, i, &mut p, &mut n, null_mut(), null_mut()), "get parameter set")?;
            sets.push(std::slice::from_raw_parts(p, n));
        }
        Ok((sets, nal_len as usize))
    }
}

/// VT output (length-prefixed, parameter sets out of band) → Annex-B with
/// parameter sets in front of keyframes.
///
/// # Safety
/// `sbuf` must be a valid encoded sample buffer.
unsafe fn sample_to_annexb(codec: NalCodec, sbuf: CMSampleBufferRef) -> Result<Encoded> {
    // SAFETY: caller contract; all objects are +0 and live as long as sbuf.
    unsafe {
        let keyframe = sample_is_sync(sbuf);
        let fd = CMSampleBufferGetFormatDescription(sbuf);
        if fd.is_null() {
            return Err("encoded sample without format description".into());
        }
        let (sets, nal_len) = parameter_sets(codec, fd)?;
        let bb = CMSampleBufferGetDataBuffer(sbuf);
        if bb.is_null() {
            return Err("encoded sample without data".into());
        }
        let total = CMBlockBufferGetDataLength(bb);
        let mut out = Vec::with_capacity(total + 64 + if keyframe { 256 } else { 0 });
        if keyframe {
            for s in &sets {
                annexb::push_annexb(&mut out, s);
            }
        }
        let (mut at, mut len, mut p) = (0usize, 0usize, null_mut());
        let st = CMBlockBufferGetDataPointer(bb, 0, &mut at, &mut len, &mut p);
        if st == 0 && at == total && !p.is_null() {
            annexb::length_prefixed_to_annexb(std::slice::from_raw_parts(p as *const u8, total), nal_len, &mut out)?;
        } else {
            let mut tmp = vec![0u8; total];
            cf::check(CMBlockBufferCopyDataBytes(bb, 0, total, tmp.as_mut_ptr() as _), "CMBlockBufferCopyDataBytes")?;
            annexb::length_prefixed_to_annexb(&tmp, nal_len, &mut out)?;
        }
        Ok(Encoded { data: out, keyframe })
    }
}

// ------------------------------------------------------------------ decoder

struct DecSlot {
    out: Mutex<Option<Result<Option<CfRef>>>>,
}

pub struct VtDecoder {
    session: Option<Session>,
    format: Option<CfRef>,
    params: Vec<Vec<u8>>,
    slot: Box<DecSlot>,
    codec: Codec,
    nal: NalCodec,
    pub hardware: bool,
}

// SAFETY: as for the encoder, single owner thread; callback uses the mutex.
unsafe impl Send for VtDecoder {}

impl VtDecoder {
    pub fn new(codec: Codec) -> Result<Self> {
        Ok(VtDecoder {
            session: None,
            format: None,
            params: Vec::new(),
            slot: Box::new(DecSlot { out: Mutex::new(None) }),
            codec,
            nal: nal_codec(codec)?,
            hardware: false,
        })
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Decode one Annex-B access unit synchronously. Returns the decoded
    /// IOSurface-backed NV12 `CVPixelBuffer`, or `None` if the decoder
    /// produced no picture.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<CfRef>> {
        let au = annexb::split_access_unit(self.nal, data);
        if !au.params.is_empty() {
            let same =
                au.params.len() == self.params.len() && au.params.iter().zip(&self.params).all(|(a, b)| *a == &b[..]);
            if !same || self.session.is_none() {
                self.configure(&au.params)?;
            }
        }
        let (Some(session), Some(format)) = (&self.session, &self.format) else {
            return Err("waiting for a keyframe with parameter sets".into());
        };
        if au.sample.is_empty() {
            return Ok(None);
        }
        let len = au.sample.len();
        // SAFETY: CoreMedia allocates and owns the block memory; we copy into it.
        let sample = unsafe {
            let mut bb: CMBlockBufferRef = null_mut();
            cf::check(
                CMBlockBufferCreateWithMemoryBlock(
                    kCFAllocatorDefault,
                    null_mut(),
                    len,
                    kCFAllocatorDefault,
                    null(),
                    0,
                    len,
                    kCMBlockBufferAssureMemoryNowFlag,
                    &mut bb,
                ),
                "CMBlockBufferCreateWithMemoryBlock",
            )?;
            let bb = CfRef::from_owned(bb).ok_or("null block buffer")?;
            cf::check(
                CMBlockBufferReplaceDataBytes(au.sample.as_ptr() as _, bb.as_ptr(), 0, len),
                "CMBlockBufferReplaceDataBytes",
            )?;
            let mut sb: CMSampleBufferRef = null_mut();
            let sizes = [len];
            cf::check(
                CMSampleBufferCreateReady(
                    kCFAllocatorDefault,
                    bb.as_ptr(),
                    format.as_ptr(),
                    1,
                    0,
                    null(),
                    1,
                    sizes.as_ptr(),
                    &mut sb,
                ),
                "CMSampleBufferCreateReady",
            )?;
            CfRef::from_owned(sb).ok_or("null sample buffer")?
        };
        *self.slot.out.lock().unwrap() = None;
        let mut info = 0u32;
        // SAFETY: valid session and sample; flags 0 = synchronous decode.
        let st =
            unsafe { VTDecompressionSessionDecodeFrame(session.r.as_ptr(), sample.as_ptr(), 0, null_mut(), &mut info) };
        if st != 0 {
            // kVTInvalidSessionErr (e.g. after sleep/GPU reset): rebuild on the next keyframe.
            if st == -12903 {
                self.session = None;
            }
            return Err(format!("VTDecompressionSessionDecodeFrame: {}", cf::os_status(st)));
        }
        if self.slot.out.lock().unwrap().is_none() {
            // SAFETY: valid session.
            unsafe { VTDecompressionSessionWaitForAsynchronousFrames(session.r.as_ptr()) };
        }
        self.slot.out.lock().unwrap().take().unwrap_or(Ok(None))
    }

    fn configure(&mut self, params: &[&[u8]]) -> Result<()> {
        let ptrs: Vec<*const u8> = params.iter().map(|p| p.as_ptr()).collect();
        let sizes: Vec<usize> = params.iter().map(|p| p.len()).collect();
        let mut fd: CMFormatDescriptionRef = null_mut();
        // SAFETY: arrays of `params.len()` valid parameter sets.
        let st = unsafe {
            match self.nal {
                NalCodec::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    kCFAllocatorDefault,
                    params.len(),
                    ptrs.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    &mut fd,
                ),
                NalCodec::Hevc => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    kCFAllocatorDefault,
                    params.len(),
                    ptrs.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    null(),
                    &mut fd,
                ),
            }
        };
        cf::check(st, "create format description from parameter sets")?;
        // SAFETY: +1 from Create.
        let fd = unsafe { CfRef::from_owned(fd) }.ok_or("null format description")?;
        self.params = params.iter().map(|p| p.to_vec()).collect();
        // SAFETY: valid session and format.
        let reuse = self.session.as_ref().is_some_and(
            |s| unsafe { VTDecompressionSessionCanAcceptFormatDescription(s.r.as_ptr(), fd.as_ptr()) } != 0,
        );
        if reuse {
            self.format = Some(fd);
            return Ok(());
        }
        self.session = None;
        self.format = None;
        let dest = nv12_attrs(None);
        let record = VTDecompressionOutputCallbackRecord {
            callback: Some(decode_callback),
            refcon: &*self.slot as *const _ as _,
        };
        let mut s: VTDecompressionSessionRef = null_mut();
        // SAFETY: valid format and attributes; `slot` outlives the session.
        let st = unsafe {
            VTDecompressionSessionCreate(kCFAllocatorDefault, fd.as_ptr(), null(), dest.as_ptr(), &record, &mut s)
        };
        cf::check(st, "VTDecompressionSessionCreate")?;
        // SAFETY: +1 from Create.
        let session = Session { r: unsafe { CfRef::from_owned(s) }.ok_or("null session")?, compression: false };
        // SAFETY: framework constants.
        unsafe {
            set_prop(session.r.as_ptr(), kVTDecompressionPropertyKey_RealTime, cf::boolean(true), "decoder RealTime");
            self.hardware =
                get_bool_prop(session.r.as_ptr(), kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder)
                    .unwrap_or(false);
            let dim = CMVideoFormatDescriptionGetDimensions(fd.as_ptr());
            sard_core::info!(
                "VideoToolbox decoder: {} {}x{} ({})",
                self.codec.name(),
                dim.width,
                dim.height,
                if self.hardware { "hardware" } else { "software" }
            );
        }
        self.session = Some(session);
        self.format = Some(fd);
        Ok(())
    }
}

unsafe extern "C" fn decode_callback(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: cf::OSStatus,
    info: u32,
    image: CVImageBufferRef,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: refcon is the decoder's boxed slot, alive while the session is.
    let slot = unsafe { &*(refcon as *const DecSlot) };
    let r = if status != 0 {
        Err(format!("decode failed: {}", cf::os_status(status)))
    } else if image.is_null() || info & kVTDecodeInfo_FrameDropped != 0 {
        Ok(None)
    } else {
        // SAFETY: valid image buffer; retained for the caller.
        Ok(unsafe { CfRef::retain(image) })
    };
    if let Ok(mut g) = slot.out.lock() {
        *g = Some(r);
    }
}

// ------------------------------------------------------------ pixel buffers

/// New IOSurface-backed NV12 (video range) pixel buffer.
pub fn new_pixel_buffer(w: u32, h: u32) -> Result<CfRef> {
    let attrs = nv12_attrs(None);
    let mut pb: CVPixelBufferRef = null_mut();
    // SAFETY: valid attributes and out-pointer.
    unsafe {
        let st = CVPixelBufferCreate(
            kCFAllocatorDefault,
            w as usize,
            h as usize,
            kCVPixelFormatType_420v,
            attrs.as_ptr(),
            &mut pb,
        );
        cf::check(st, "CVPixelBufferCreate")?;
        CfRef::from_owned(pb).ok_or_else(|| "null pixel buffer".into())
    }
}

/// Tag a buffer as BT.709 (primaries, transfer, matrix).
pub fn tag_bt709(pb: &CfRef) {
    // SAFETY: valid buffer; framework constants.
    unsafe {
        let b = pb.as_ptr();
        CVBufferSetAttachment(
            b,
            kCVImageBufferColorPrimariesKey,
            kCVImageBufferColorPrimaries_ITU_R_709_2,
            kCVAttachmentMode_ShouldPropagate,
        );
        CVBufferSetAttachment(
            b,
            kCVImageBufferTransferFunctionKey,
            kCVImageBufferTransferFunction_ITU_R_709_2,
            kCVAttachmentMode_ShouldPropagate,
        );
        CVBufferSetAttachment(
            b,
            kCVImageBufferYCbCrMatrixKey,
            kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            kCVAttachmentMode_ShouldPropagate,
        );
    }
}

pub fn dimensions(pb: &CfRef) -> (u32, u32) {
    // SAFETY: valid buffer.
    unsafe { (CVPixelBufferGetWidth(pb.as_ptr()) as u32, CVPixelBufferGetHeight(pb.as_ptr()) as u32) }
}

/// Copy a tightly packed NV12 frame into a (locked) pixel buffer.
pub fn write_nv12(pb: &CfRef, src: &[u8], w: usize, h: usize) -> Result<()> {
    let p = pb.as_ptr();
    // SAFETY: buffer locked for writing while we copy within plane bounds.
    unsafe {
        if CVPixelBufferGetPixelFormatType(p) != kCVPixelFormatType_420v {
            return Err("pixel buffer is not 420v".into());
        }
        if CVPixelBufferGetWidth(p) < w || CVPixelBufferGetHeight(p) < h {
            return Err("pixel buffer too small".into());
        }
        cf::check(CVPixelBufferLockBaseAddress(p, 0), "CVPixelBufferLockBaseAddress")?;
        for (plane, rows, off) in [(0usize, h, 0usize), (1, h / 2, w * h)] {
            let dst = CVPixelBufferGetBaseAddressOfPlane(p, plane) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(p, plane);
            for y in 0..rows {
                std::ptr::copy_nonoverlapping(src.as_ptr().add(off + y * w), dst.add(y * stride), w);
            }
        }
        CVPixelBufferUnlockBaseAddress(p, 0);
    }
    Ok(())
}

/// Read one plane back into a tightly packed Vec (tests / diagnostics).
/// Returns (bytes, width in bytes, rows).
pub fn read_plane(pb: &CfRef, plane: usize) -> Result<(Vec<u8>, usize, usize)> {
    let p = pb.as_ptr();
    // SAFETY: buffer locked read-only while we copy within plane bounds.
    unsafe {
        cf::check(CVPixelBufferLockBaseAddress(p, kCVPixelBufferLock_ReadOnly), "lock")?;
        let rows = CVPixelBufferGetHeightOfPlane(p, plane);
        let width = CVPixelBufferGetWidthOfPlane(p, plane) * if plane == 0 { 1 } else { 2 };
        let stride = CVPixelBufferGetBytesPerRowOfPlane(p, plane);
        let src = CVPixelBufferGetBaseAddressOfPlane(p, plane) as *const u8;
        let mut out = vec![0u8; width * rows];
        for y in 0..rows {
            std::ptr::copy_nonoverlapping(src.add(y * stride), out.as_mut_ptr().add(y * width), width);
        }
        CVPixelBufferUnlockBaseAddress(p, kCVPixelBufferLock_ReadOnly);
        Ok((out, width, rows))
    }
}

pub fn hardware_decode_supported(c: Codec) -> bool {
    // SAFETY: plain query.
    unsafe { VTIsHardwareDecodeSupported(codec_type(c)) != 0 }
}
