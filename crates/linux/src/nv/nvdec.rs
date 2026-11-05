//! NVDEC hardware decoder (libnvcuvid.so.1) with the NVIDIA bitstream parser.
//!
//! Each `decode` call feeds exactly one access unit with
//! `CUVID_PKT_ENDOFPICTURE` and `ulMaxDisplayDelay = 0`, so the display
//! callback fires inside the same call and the decoded surface is handed to
//! the caller immediately (zero frames of decoder latency).

use super::blob::{Blob, F};
use super::cuda::{CUdeviceptr, Cuda};
use super::layout as L;
use crate::dylib;
use sard_core::proto::Codec;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Arc;

pub type Result<T> = std::result::Result<T, String>;

dylib! {
    struct NvDecLib ["libnvcuvid.so.1", "libnvcuvid.so"] {
        fn cuvidGetDecoderCaps(*mut c_void) -> i32;
        fn cuvidCreateDecoder(*mut *mut c_void, *mut c_void) -> i32;
        fn cuvidDestroyDecoder(*mut c_void) -> i32;
        fn cuvidDecodePicture(*mut c_void, *mut c_void) -> i32;
        fn cuvidMapVideoFrame64(*mut c_void, i32, *mut CUdeviceptr, *mut u32, *mut c_void) -> i32;
        fn cuvidUnmapVideoFrame64(*mut c_void, CUdeviceptr) -> i32;
        fn cuvidCreateVideoParser(*mut *mut c_void, *mut c_void) -> i32;
        fn cuvidParseVideoData(*mut c_void, *mut c_void) -> i32;
        fn cuvidDestroyVideoParser(*mut c_void) -> i32;
    }
}

/// Read a field from a C struct we only have a pointer to.
///
/// # Safety
/// `p` must point to a live struct containing `f`.
unsafe fn rd(p: *const c_void, f: F) -> u64 {
    let b = unsafe { (p as *const u8).add(f.0) };
    unsafe {
        match f.1 {
            1 => *b as u64,
            2 => (b as *const u16).read_unaligned() as u64,
            4 => (b as *const u32).read_unaligned() as u64,
            8 => (b as *const u64).read_unaligned(),
            _ => unreachable!(),
        }
    }
}

/// A decoded frame, valid only inside the `decode` callback.
pub struct MappedFrame {
    pub dptr: CUdeviceptr,
    pub pitch: usize,
    pub width: u32,
    pub height: u32,
    /// Rows of the luma plane in the surface (chroma starts at `pitch * surface_height`).
    pub surface_height: u32,
    pub yuv444: bool,
    pub full_range: bool,
    pub timestamp: i64,
}

struct State {
    lib: NvDecLib,
    decoder: *mut c_void,
    codec: u32,
    coded_w: u32,
    coded_h: u32,
    width: u32,
    height: u32,
    surface_h: u32,
    chroma444: bool,
    full_range: bool,
    surfaces: u32,
    ready: VecDeque<(i32, i64)>,
    error: Option<String>,
}

pub struct NvDecoder {
    cuda: Arc<Cuda>,
    parser: *mut c_void,
    state: Box<State>,
}

// SAFETY: used from one thread at a time; CUDA context pushed per call.
unsafe impl Send for NvDecoder {}

fn cuda_codec(c: Codec) -> u32 {
    match c {
        Codec::H264 => L::CUDA_CODEC_H264,
        Codec::Hevc => L::CUDA_CODEC_HEVC,
        Codec::Av1 => L::CUDA_CODEC_AV1,
    }
}

unsafe extern "C" fn on_sequence(user: *mut c_void, fmt: *mut c_void) -> i32 {
    let st = unsafe { &mut *(user as *mut State) };
    let f = fmt as *const c_void;
    let (codec, coded_w, coded_h, chroma, depth, min_surf, left, top, right, bottom, full) = unsafe {
        (
            rd(f, L::VF_CODEC) as u32,
            rd(f, L::VF_CODED_WIDTH) as u32,
            rd(f, L::VF_CODED_HEIGHT) as u32,
            rd(f, L::VF_CHROMA_FORMAT) as u32,
            rd(f, L::VF_BIT_DEPTH_LUMA_MINUS8) as u32,
            rd(f, L::VF_MIN_NUM_DECODE_SURFACES) as u32,
            rd(f, L::VF_DISPLAY_LEFT) as i32,
            rd(f, L::VF_DISPLAY_TOP) as i32,
            rd(f, L::VF_DISPLAY_RIGHT) as i32,
            rd(f, L::VF_DISPLAY_BOTTOM) as i32,
            {
                let w = rd(f, F(L::VF_FULL_RANGE.0, 4)) as u32;
                (w >> L::VF_FULL_RANGE.1) & 1
            },
        )
    };
    let width = (right - left).max(0) as u32;
    let height = (bottom - top).max(0) as u32;
    let surfaces = (min_surf + 2).max(4);
    let chroma444 = chroma == L::CUDA_CHROMA_444;
    st.full_range = full != 0;
    if !st.decoder.is_null()
        && st.codec == codec
        && st.coded_w == coded_w
        && st.coded_h == coded_h
        && st.width == width
        && st.height == height
        && st.chroma444 == chroma444
    {
        return st.surfaces as i32;
    }
    if !st.decoder.is_null() {
        unsafe { (st.lib.cuvidDestroyDecoder)(st.decoder) };
        st.decoder = std::ptr::null_mut();
    }
    if depth != 0 {
        st.error = Some(format!("{}-bit video is not supported yet", depth + 8));
        return 0;
    }
    let mut dc = Blob::new(L::DECODE_CREATE_SIZE);
    dc.set(L::DC_WIDTH, coded_w as u64);
    dc.set(L::DC_HEIGHT, coded_h as u64);
    dc.set(L::DC_NUM_DECODE_SURFACES, surfaces as u64);
    dc.set(L::DC_CODEC_TYPE, codec as u64);
    dc.set(L::DC_CHROMA_FORMAT, chroma as u64);
    dc.set(L::DC_CREATION_FLAGS, L::CUDA_CREATE_PREFER_CUVID as u64);
    dc.set(L::DC_BIT_DEPTH_MINUS8, 0);
    dc.set(L::DC_MAX_WIDTH, coded_w as u64);
    dc.set(L::DC_MAX_HEIGHT, coded_h as u64);
    dc.set(L::DC_DISPLAY_LEFT, left as u64);
    dc.set(L::DC_DISPLAY_TOP, top as u64);
    dc.set(L::DC_DISPLAY_RIGHT, right as u64);
    dc.set(L::DC_DISPLAY_BOTTOM, bottom as u64);
    dc.set(L::DC_OUTPUT_FORMAT, if chroma444 { L::CUDA_SURFACE_YUV444 } else { L::CUDA_SURFACE_NV12 } as u64);
    dc.set(L::DC_DEINTERLACE_MODE, L::CUDA_DEINTERLACE_WEAVE as u64);
    dc.set(L::DC_TARGET_WIDTH, width as u64);
    dc.set(L::DC_TARGET_HEIGHT, height as u64);
    dc.set(L::DC_NUM_OUTPUT_SURFACES, 2);
    let mut dec = std::ptr::null_mut();
    let r = unsafe { (st.lib.cuvidCreateDecoder)(&mut dec, dc.ptr()) };
    if r != 0 {
        st.error = Some(format!("cuvidCreateDecoder failed ({r}) for {width}x{height}"));
        return 0;
    }
    st.decoder = dec;
    st.codec = codec;
    st.coded_w = coded_w;
    st.coded_h = coded_h;
    st.width = width;
    st.height = height;
    // Output surfaces are allocated at target size.
    st.surface_h = height;
    st.chroma444 = chroma444;
    st.surfaces = surfaces;
    surfaces as i32
}

unsafe extern "C" fn on_decode(user: *mut c_void, pic: *mut c_void) -> i32 {
    let st = unsafe { &mut *(user as *mut State) };
    if st.decoder.is_null() {
        return 0;
    }
    let r = unsafe { (st.lib.cuvidDecodePicture)(st.decoder, pic) };
    if r != 0 {
        st.error = Some(format!("cuvidDecodePicture failed ({r})"));
        return 0;
    }
    1
}

unsafe extern "C" fn on_display(user: *mut c_void, info: *mut c_void) -> i32 {
    let st = unsafe { &mut *(user as *mut State) };
    if info.is_null() {
        return 1; // end of stream
    }
    let (idx, ts) = unsafe { (rd(info, L::DISP_PICTURE_INDEX) as i32, rd(info, L::DISP_TIMESTAMP) as i64) };
    st.ready.push_back((idx, ts));
    1
}

impl NvDecoder {
    pub fn new(cuda: Arc<Cuda>, codec: Codec) -> Result<Self> {
        let lib = NvDecLib::load()?;
        let _g = cuda.push();
        // Capability check for a clear error.
        let mut caps = Blob::new(L::DECODE_CAPS_SIZE);
        caps.set(L::CAPS_CODEC_TYPE, cuda_codec(codec) as u64);
        caps.set(L::CAPS_CHROMA_FORMAT, L::CUDA_CHROMA_420 as u64);
        caps.set(L::CAPS_BIT_DEPTH_MINUS8, 0);
        // SAFETY: valid caps struct.
        let r = unsafe { (lib.cuvidGetDecoderCaps)(caps.ptr()) };
        if r != 0 || caps.get(L::CAPS_IS_SUPPORTED) == 0 {
            return Err(format!("this GPU cannot decode {}", codec.name()));
        }

        let mut state = Box::new(State {
            lib,
            decoder: std::ptr::null_mut(),
            codec: 0,
            coded_w: 0,
            coded_h: 0,
            width: 0,
            height: 0,
            surface_h: 0,
            chroma444: false,
            full_range: false,
            surfaces: 0,
            ready: VecDeque::new(),
            error: None,
        });
        let mut pp = Blob::new(L::PARSER_PARAMS_SIZE);
        pp.set(L::PP_CODEC_TYPE, cuda_codec(codec) as u64);
        pp.set(L::PP_MAX_NUM_DECODE_SURFACES, 1);
        pp.set(L::PP_CLOCK_RATE, 1_000_000);
        pp.set(L::PP_ERROR_THRESHOLD, 100);
        pp.set(L::PP_MAX_DISPLAY_DELAY, 0);
        pp.set_ptr(L::PP_USER_DATA, &mut *state as *mut State);
        pp.set(L::PP_SEQUENCE_CALLBACK, on_sequence as *const () as usize as u64);
        pp.set(L::PP_DECODE_PICTURE, on_decode as *const () as usize as u64);
        pp.set(L::PP_DISPLAY_PICTURE, on_display as *const () as usize as u64);
        let mut parser = std::ptr::null_mut();
        // SAFETY: valid parser params; state outlives the parser (both owned by self).
        let r = unsafe { (state.lib.cuvidCreateVideoParser)(&mut parser, pp.ptr()) };
        if r != 0 {
            return Err(format!("cuvidCreateVideoParser failed ({r})"));
        }
        drop(_g);
        Ok(NvDecoder { cuda, parser, state })
    }

    pub fn cuda(&self) -> &Arc<Cuda> {
        &self.cuda
    }

    /// Decode one access unit. `on_frame` is called for each output picture
    /// while it is mapped (normally exactly once).
    pub fn decode(&mut self, data: &[u8], timestamp: i64, mut on_frame: impl FnMut(&MappedFrame)) -> Result<usize> {
        let _g = self.cuda.push();
        let mut pkt = Blob::new(L::PACKET_SIZE);
        pkt.set(L::PKT_FLAGS, (L::CUVID_PKT_ENDOFPICTURE | L::CUVID_PKT_TIMESTAMP) as u64);
        pkt.set(L::PKT_PAYLOAD_SIZE, data.len() as u64);
        pkt.set_ptr(L::PKT_PAYLOAD, data.as_ptr());
        pkt.set(L::PKT_TIMESTAMP, timestamp as u64);
        // SAFETY: parser is valid; data outlives the call.
        let r = unsafe { (self.state.lib.cuvidParseVideoData)(self.parser, pkt.ptr()) };
        if let Some(e) = self.state.error.take() {
            return Err(e);
        }
        if r != 0 {
            return Err(format!("cuvidParseVideoData failed ({r})"));
        }
        let mut n = 0;
        while let Some((idx, ts)) = self.state.ready.pop_front() {
            let st = &mut *self.state;
            let mut proc = Blob::new(L::PROC_PARAMS_SIZE);
            proc.set(L::PROC_PROGRESSIVE_FRAME, 1);
            let mut dptr: CUdeviceptr = 0;
            let mut pitch: u32 = 0;
            // SAFETY: idx came from the display callback of this decoder.
            let r = unsafe { (st.lib.cuvidMapVideoFrame64)(st.decoder, idx, &mut dptr, &mut pitch, proc.ptr()) };
            if r != 0 {
                return Err(format!("cuvidMapVideoFrame failed ({r})"));
            }
            on_frame(&MappedFrame {
                dptr,
                pitch: pitch as usize,
                width: st.width,
                height: st.height,
                surface_height: st.surface_h,
                yuv444: st.chroma444,
                full_range: st.full_range,
                timestamp: ts,
            });
            // SAFETY: unmapping what we mapped.
            unsafe { (st.lib.cuvidUnmapVideoFrame64)(st.decoder, dptr) };
            n += 1;
        }
        Ok(n)
    }

    pub fn size(&self) -> (u32, u32) {
        (self.state.width, self.state.height)
    }
}

impl Drop for NvDecoder {
    fn drop(&mut self) {
        let _g = self.cuda.push();
        // SAFETY: destroying objects we created.
        unsafe {
            (self.state.lib.cuvidDestroyVideoParser)(self.parser);
            if !self.state.decoder.is_null() {
                (self.state.lib.cuvidDestroyDecoder)(self.state.decoder);
            }
        }
    }
}
