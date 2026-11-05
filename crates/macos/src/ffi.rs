//! Hand-written bindings for the C frameworks we use: CoreMedia, CoreVideo,
//! VideoToolbox, CoreGraphics, ApplicationServices, Metal, libdispatch.
//! Only what sard needs; signatures follow the macOS SDK headers.

#![allow(non_upper_case_globals, non_snake_case, non_camel_case_types, dead_code)]

use crate::cf::{Boolean, CFAllocatorRef, CFArrayRef, CFDictionaryRef, CFStringRef, CFTypeRef, OSStatus};
use objc2::encode::{Encode, Encoding};
use std::ffi::{c_char, c_void};

// ---------------------------------------------------------------- CoreMedia

pub type CMSampleBufferRef = *mut c_void;
pub type CMBlockBufferRef = *mut c_void;
pub type CMFormatDescriptionRef = *mut c_void;
pub type CVImageBufferRef = *mut c_void;
pub type CVPixelBufferRef = *mut c_void;
pub type CVPixelBufferPoolRef = *mut c_void;
pub type CVMetalTextureCacheRef = *mut c_void;
pub type CVMetalTextureRef = *mut c_void;
pub type VTCompressionSessionRef = *mut c_void;
pub type VTDecompressionSessionRef = *mut c_void;
pub type CVReturn = i32;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CMTime {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}

pub const kCMTimeFlags_Valid: u32 = 1;

impl CMTime {
    pub const INVALID: CMTime = CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 };
    pub fn new(value: i64, timescale: i32) -> Self {
        CMTime { value, timescale, flags: kCMTimeFlags_Valid, epoch: 0 }
    }
    pub fn is_valid(&self) -> bool {
        self.flags & kCMTimeFlags_Valid != 0 && self.timescale > 0
    }
    /// Value in microseconds (None if invalid).
    pub fn as_us(&self) -> Option<u64> {
        if !self.is_valid() || self.value < 0 {
            return None;
        }
        Some((self.value as i128 * 1_000_000 / self.timescale as i128) as u64)
    }
}

// SAFETY: matches `typedef struct { int64; int32; uint32; int64 } CMTime` ("{?=qiIq}").
unsafe impl Encode for CMTime {
    const ENCODING: Encoding = Encoding::Struct("?", &[i64::ENCODING, i32::ENCODING, u32::ENCODING, i64::ENCODING]);
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CMVideoDimensions {
    pub width: i32,
    pub height: i32,
}

pub const kCMBlockBufferAssureMemoryNowFlag: u32 = 1;

pub const kCMVideoCodecType_H264: u32 = u32::from_be_bytes(*b"avc1");
pub const kCMVideoCodecType_HEVC: u32 = u32::from_be_bytes(*b"hvc1");
pub const kCMVideoCodecType_AV1: u32 = u32::from_be_bytes(*b"av01");
/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` ('420v', NV12 limited range).
pub const kCVPixelFormatType_420v: u32 = u32::from_be_bytes(*b"420v");

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    pub static kCMSampleAttachmentKey_NotSync: CFStringRef;
    pub static kCMSampleAttachmentKey_DependsOnOthers: CFStringRef;

    pub fn CMSampleBufferGetDataBuffer(sbuf: CMSampleBufferRef) -> CMBlockBufferRef;
    pub fn CMSampleBufferGetFormatDescription(sbuf: CMSampleBufferRef) -> CMFormatDescriptionRef;
    pub fn CMSampleBufferGetSampleAttachmentsArray(sbuf: CMSampleBufferRef, create: Boolean) -> CFArrayRef;
    pub fn CMSampleBufferGetImageBuffer(sbuf: CMSampleBufferRef) -> CVImageBufferRef;
    pub fn CMSampleBufferGetPresentationTimeStamp(sbuf: CMSampleBufferRef) -> CMTime;
    pub fn CMSampleBufferCreateReady(
        alloc: CFAllocatorRef,
        data: CMBlockBufferRef,
        format: CMFormatDescriptionRef,
        num_samples: isize,
        num_timing: isize,
        timing: *const c_void,
        num_sizes: isize,
        sizes: *const usize,
        out: *mut CMSampleBufferRef,
    ) -> OSStatus;

    pub fn CMBlockBufferGetDataLength(b: CMBlockBufferRef) -> usize;
    pub fn CMBlockBufferCopyDataBytes(b: CMBlockBufferRef, offset: usize, len: usize, dst: *mut c_void) -> OSStatus;
    pub fn CMBlockBufferGetDataPointer(
        b: CMBlockBufferRef,
        offset: usize,
        len_at_offset: *mut usize,
        total_len: *mut usize,
        ptr: *mut *mut c_char,
    ) -> OSStatus;
    pub fn CMBlockBufferCreateWithMemoryBlock(
        structure_alloc: CFAllocatorRef,
        memory: *mut c_void,
        block_len: usize,
        block_alloc: CFAllocatorRef,
        custom_source: *const c_void,
        offset: usize,
        data_len: usize,
        flags: u32,
        out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    pub fn CMBlockBufferReplaceDataBytes(
        src: *const c_void,
        dst: CMBlockBufferRef,
        offset: usize,
        len: usize,
    ) -> OSStatus;

    pub fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        alloc: CFAllocatorRef,
        count: usize,
        ptrs: *const *const u8,
        sizes: *const usize,
        nal_header_len: i32,
        out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    pub fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        alloc: CFAllocatorRef,
        count: usize,
        ptrs: *const *const u8,
        sizes: *const usize,
        nal_header_len: i32,
        extensions: CFDictionaryRef,
        out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    pub fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        desc: CMFormatDescriptionRef,
        index: usize,
        ptr: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        nal_header_len: *mut i32,
    ) -> OSStatus;
    pub fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
        desc: CMFormatDescriptionRef,
        index: usize,
        ptr: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        nal_header_len: *mut i32,
    ) -> OSStatus;
    pub fn CMVideoFormatDescriptionGetDimensions(desc: CMFormatDescriptionRef) -> CMVideoDimensions;
}

// ---------------------------------------------------------------- CoreVideo

pub const kCVPixelBufferLock_ReadOnly: u64 = 1;
pub const kCVAttachmentMode_ShouldPropagate: u32 = 1;

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    pub static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    pub static kCVPixelBufferWidthKey: CFStringRef;
    pub static kCVPixelBufferHeightKey: CFStringRef;
    pub static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
    pub static kCVPixelBufferMetalCompatibilityKey: CFStringRef;
    pub static kCVImageBufferColorPrimariesKey: CFStringRef;
    pub static kCVImageBufferColorPrimaries_ITU_R_709_2: CFStringRef;
    pub static kCVImageBufferTransferFunctionKey: CFStringRef;
    pub static kCVImageBufferTransferFunction_ITU_R_709_2: CFStringRef;
    pub static kCVImageBufferYCbCrMatrixKey: CFStringRef;
    pub static kCVImageBufferYCbCrMatrix_ITU_R_709_2: CFStringRef;

    pub fn CVPixelBufferCreate(
        alloc: CFAllocatorRef,
        width: usize,
        height: usize,
        format: u32,
        attrs: CFDictionaryRef,
        out: *mut CVPixelBufferRef,
    ) -> CVReturn;
    pub fn CVPixelBufferPoolCreatePixelBuffer(
        alloc: CFAllocatorRef,
        pool: CVPixelBufferPoolRef,
        out: *mut CVPixelBufferRef,
    ) -> CVReturn;
    pub fn CVPixelBufferLockBaseAddress(pb: CVPixelBufferRef, flags: u64) -> CVReturn;
    pub fn CVPixelBufferUnlockBaseAddress(pb: CVPixelBufferRef, flags: u64) -> CVReturn;
    pub fn CVPixelBufferGetBaseAddressOfPlane(pb: CVPixelBufferRef, plane: usize) -> *mut c_void;
    pub fn CVPixelBufferGetBytesPerRowOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
    pub fn CVPixelBufferGetWidth(pb: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetHeight(pb: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetWidthOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
    pub fn CVPixelBufferGetHeightOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
    pub fn CVPixelBufferGetPixelFormatType(pb: CVPixelBufferRef) -> u32;
    pub fn CVPixelBufferGetIOSurface(pb: CVPixelBufferRef) -> *mut c_void;
    pub fn CVBufferSetAttachment(buf: CVImageBufferRef, key: CFStringRef, value: CFTypeRef, mode: u32);

    pub fn CVMetalTextureCacheCreate(
        alloc: CFAllocatorRef,
        cache_attrs: CFDictionaryRef,
        device: *mut c_void,
        texture_attrs: CFDictionaryRef,
        out: *mut CVMetalTextureCacheRef,
    ) -> CVReturn;
    pub fn CVMetalTextureCacheCreateTextureFromImage(
        alloc: CFAllocatorRef,
        cache: CVMetalTextureCacheRef,
        image: CVImageBufferRef,
        attrs: CFDictionaryRef,
        pixel_format: usize,
        width: usize,
        height: usize,
        plane: usize,
        out: *mut CVMetalTextureRef,
    ) -> CVReturn;
    pub fn CVMetalTextureGetTexture(t: CVMetalTextureRef) -> *mut c_void;
    pub fn CVMetalTextureCacheFlush(cache: CVMetalTextureCacheRef, options: u64);
}

// ------------------------------------------------------------- VideoToolbox

pub type VTCompressionOutputCallback = unsafe extern "C" fn(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: OSStatus,
    info: u32,
    sbuf: CMSampleBufferRef,
);

pub type VTDecompressionOutputCallback = unsafe extern "C" fn(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: OSStatus,
    info: u32,
    image: CVImageBufferRef,
    pts: CMTime,
    duration: CMTime,
);

#[repr(C)]
pub struct VTDecompressionOutputCallbackRecord {
    pub callback: Option<VTDecompressionOutputCallback>,
    pub refcon: *mut c_void,
}

pub const kVTEncodeInfo_Asynchronous: u32 = 1;
pub const kVTEncodeInfo_FrameDropped: u32 = 2;
pub const kVTDecodeInfo_FrameDropped: u32 = 2;

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    pub static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFStringRef;
    pub static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;
    pub static kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: CFStringRef;
    pub static kVTCompressionPropertyKey_RealTime: CFStringRef;
    pub static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    pub static kVTCompressionPropertyKey_AllowTemporalCompression: CFStringRef;
    pub static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    pub static kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: CFStringRef;
    pub static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    pub static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    pub static kVTCompressionPropertyKey_DataRateLimits: CFStringRef;
    pub static kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality: CFStringRef;
    pub static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    pub static kVTCompressionPropertyKey_ColorPrimaries: CFStringRef;
    pub static kVTCompressionPropertyKey_TransferFunction: CFStringRef;
    pub static kVTCompressionPropertyKey_YCbCrMatrix: CFStringRef;
    pub static kVTCompressionPropertyKey_MaxFrameDelayCount: CFStringRef;
    pub static kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder: CFStringRef;
    pub static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
    pub static kVTProfileLevel_H264_High_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_HEVC_Main_AutoLevel: CFStringRef;
    pub static kVTDecompressionPropertyKey_RealTime: CFStringRef;
    pub static kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder: CFStringRef;

    pub fn VTCompressionSessionCreate(
        alloc: CFAllocatorRef,
        width: i32,
        height: i32,
        codec: u32,
        encoder_spec: CFDictionaryRef,
        source_attrs: CFDictionaryRef,
        compressed_alloc: CFAllocatorRef,
        callback: Option<VTCompressionOutputCallback>,
        refcon: *mut c_void,
        out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    pub fn VTCompressionSessionPrepareToEncodeFrames(s: VTCompressionSessionRef) -> OSStatus;
    pub fn VTCompressionSessionEncodeFrame(
        s: VTCompressionSessionRef,
        image: CVImageBufferRef,
        pts: CMTime,
        duration: CMTime,
        frame_props: CFDictionaryRef,
        frame_refcon: *mut c_void,
        info_out: *mut u32,
    ) -> OSStatus;
    pub fn VTCompressionSessionCompleteFrames(s: VTCompressionSessionRef, until: CMTime) -> OSStatus;
    pub fn VTCompressionSessionInvalidate(s: VTCompressionSessionRef);
    pub fn VTCompressionSessionGetPixelBufferPool(s: VTCompressionSessionRef) -> CVPixelBufferPoolRef;

    pub fn VTDecompressionSessionCreate(
        alloc: CFAllocatorRef,
        format: CMFormatDescriptionRef,
        decoder_spec: CFDictionaryRef,
        dest_attrs: CFDictionaryRef,
        callback: *const VTDecompressionOutputCallbackRecord,
        out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    pub fn VTDecompressionSessionDecodeFrame(
        s: VTDecompressionSessionRef,
        sbuf: CMSampleBufferRef,
        flags: u32,
        frame_refcon: *mut c_void,
        info_out: *mut u32,
    ) -> OSStatus;
    pub fn VTDecompressionSessionWaitForAsynchronousFrames(s: VTDecompressionSessionRef) -> OSStatus;
    pub fn VTDecompressionSessionCanAcceptFormatDescription(
        s: VTDecompressionSessionRef,
        format: CMFormatDescriptionRef,
    ) -> Boolean;
    pub fn VTDecompressionSessionInvalidate(s: VTDecompressionSessionRef);

    pub fn VTSessionSetProperty(s: *mut c_void, key: CFStringRef, value: CFTypeRef) -> OSStatus;
    pub fn VTSessionCopyProperty(s: *mut c_void, key: CFStringRef, alloc: CFAllocatorRef, out: *mut c_void)
    -> OSStatus;
    pub fn VTIsHardwareDecodeSupported(codec: u32) -> Boolean;
}

// ------------------------------------------------------------- CoreGraphics

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

// SAFETY: layouts match the CoreGraphics structs and their ObjC encodings.
unsafe impl Encode for CGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}
unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}
unsafe impl Encode for CGRect {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
}

pub type CGEventRef = *mut c_void;
pub type CGEventSourceRef = *mut c_void;
pub type CGDirectDisplayID = u32;

pub const kCGEventSourceStateHIDSystemState: i32 = 1;
pub const kCGHIDEventTap: u32 = 0;

pub const kCGEventLeftMouseDown: u32 = 1;
pub const kCGEventLeftMouseUp: u32 = 2;
pub const kCGEventRightMouseDown: u32 = 3;
pub const kCGEventRightMouseUp: u32 = 4;
pub const kCGEventMouseMoved: u32 = 5;
pub const kCGEventLeftMouseDragged: u32 = 6;
pub const kCGEventRightMouseDragged: u32 = 7;
pub const kCGEventOtherMouseDown: u32 = 25;
pub const kCGEventOtherMouseUp: u32 = 26;
pub const kCGEventOtherMouseDragged: u32 = 27;

pub const kCGMouseEventClickState: u32 = 1;
pub const kCGMouseEventButtonNumber: u32 = 3;
pub const kCGMouseEventDeltaX: u32 = 4;
pub const kCGMouseEventDeltaY: u32 = 5;
pub const kCGScrollWheelEventIsContinuous: u32 = 88;

pub const kCGScrollEventUnitPixel: u32 = 0;
pub const kCGScrollEventUnitLine: u32 = 1;

pub const kCGEventFlagMaskAlphaShift: u64 = 0x0001_0000;
pub const kCGEventFlagMaskShift: u64 = 0x0002_0000;
pub const kCGEventFlagMaskControl: u64 = 0x0004_0000;
pub const kCGEventFlagMaskAlternate: u64 = 0x0008_0000;
pub const kCGEventFlagMaskCommand: u64 = 0x0010_0000;
pub const kCGEventFlagMaskNumericPad: u64 = 0x0020_0000;
pub const kCGEventFlagMaskSecondaryFn: u64 = 0x0080_0000;
pub const kCGEventFlagMaskNonCoalesced: u64 = 0x0000_0100;
// Device-dependent (left/right) modifier bits from IOLLEvent.h.
pub const NX_DEVICELCTLKEYMASK: u64 = 0x0000_0001;
pub const NX_DEVICELSHIFTKEYMASK: u64 = 0x0000_0002;
pub const NX_DEVICERSHIFTKEYMASK: u64 = 0x0000_0004;
pub const NX_DEVICELCMDKEYMASK: u64 = 0x0000_0008;
pub const NX_DEVICERCMDKEYMASK: u64 = 0x0000_0010;
pub const NX_DEVICELALTKEYMASK: u64 = 0x0000_0020;
pub const NX_DEVICERALTKEYMASK: u64 = 0x0000_0040;
pub const NX_DEVICERCTLKEYMASK: u64 = 0x0000_2000;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    pub static kCGColorSpaceSRGB: CFStringRef;
    pub static kCGDisplayStreamYCbCrMatrix_ITU_R_709_2: CFStringRef;

    pub fn CGMainDisplayID() -> CGDirectDisplayID;
    pub fn CGDisplayBounds(d: CGDirectDisplayID) -> CGRect;
    pub fn CGDisplayCopyDisplayMode(d: CGDirectDisplayID) -> *mut c_void;
    pub fn CGDisplayModeGetPixelWidth(mode: *mut c_void) -> usize;
    pub fn CGDisplayModeGetPixelHeight(mode: *mut c_void) -> usize;
    pub fn CGDisplayModeRelease(mode: *mut c_void);
    pub fn CGColorSpaceCreateWithName(name: CFStringRef) -> *mut c_void;
    pub fn CGColorSpaceRelease(cs: *mut c_void);

    pub fn CGEventSourceCreate(state: i32) -> CGEventSourceRef;
    pub fn CGEventSourceSetLocalEventsSuppressionInterval(src: CGEventSourceRef, seconds: f64);
    pub fn CGEventCreate(src: CGEventSourceRef) -> CGEventRef;
    pub fn CGEventGetLocation(ev: CGEventRef) -> CGPoint;
    pub fn CGEventCreateKeyboardEvent(src: CGEventSourceRef, key: u16, down: bool) -> CGEventRef;
    pub fn CGEventCreateMouseEvent(src: CGEventSourceRef, kind: u32, pos: CGPoint, button: u32) -> CGEventRef;
    pub fn CGEventCreateScrollWheelEvent2(
        src: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    pub fn CGEventSetFlags(ev: CGEventRef, flags: u64);
    pub fn CGEventSetIntegerValueField(ev: CGEventRef, field: u32, value: i64);
    pub fn CGEventPost(tap: u32, ev: CGEventRef);

    pub fn CGPreflightScreenCaptureAccess() -> bool;
    pub fn CGRequestScreenCaptureAccess() -> bool;
    pub fn CGPreflightPostEventAccess() -> bool;
    pub fn CGRequestPostEventAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    pub fn AXIsProcessTrusted() -> Boolean;
    /// With `{kAXTrustedCheckOptionPrompt: true}` shows the system prompt.
    pub fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> Boolean;
    pub static kAXTrustedCheckOptionPrompt: *const std::ffi::c_void;
}

// ------------------------------------------------- Metal, QuartzCore, SCKit

#[link(name = "Metal", kind = "framework")]
unsafe extern "C" {
    /// Returns a +1 `id<MTLDevice>`.
    pub fn MTLCreateSystemDefaultDevice() -> *mut objc2::runtime::AnyObject;
}

#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {
    pub fn CACurrentMediaTime() -> f64;
}

#[link(name = "ScreenCaptureKit", kind = "framework")]
unsafe extern "C" {
    /// `NSString *` key into the per-frame attachment dictionary.
    pub static SCStreamFrameInfoStatus: *const c_void;
}

// ------------------------------------------------------------ libdispatch

pub const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

unsafe extern "C" {
    pub fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    pub fn dispatch_queue_attr_make_with_qos_class(attr: *const c_void, qos: u32, prio: i32) -> *const c_void;
    pub fn dispatch_release(obj: *mut c_void);
}
