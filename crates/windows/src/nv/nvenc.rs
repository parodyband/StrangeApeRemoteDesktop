//! NVENC hardware encoder (nvEncodeAPI64.dll) with Direct3D 11 input.
//!
//! Same configuration as `sard-linux`'s CUDA-input encoder (ultra-low-latency
//! tuning, CBR with a single-frame VBV, no B-frames, infinite GOP, headers
//! with every IDR, BT.709 VUI); the input is an NV12 `ID3D11Texture2D` written
//! by our colour conversion shader, registered with NVENC once per texture.

use super::blob::{Blob, F, Guid};
use super::layout as L;
use crate::dylib;
use sard_core::proto::Codec;
use std::ffi::{CStr, c_char, c_void};

pub type Result<T> = std::result::Result<T, String>;

/// `NV_ENC_DEVICE_TYPE_DIRECTX` / `NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX` (nvEncodeAPI.h).
const DEVICE_TYPE_DIRECTX: u32 = 0;
const RESOURCE_TYPE_DIRECTX: u32 = 0;

dylib! {
    struct NvEncLib ["nvEncodeAPI64.dll"] {
        fn NvEncodeAPIGetMaxSupportedVersion(*mut u32) -> u32;
        fn NvEncodeAPICreateInstance(*mut c_void) -> u32;
    }
}

type Enc = *mut c_void;
type FnOpen = unsafe extern "system" fn(*mut c_void, *mut Enc) -> u32;
type FnPresetEx = unsafe extern "system" fn(Enc, Guid, Guid, u32, *mut c_void) -> u32;
type FnEncPtr = unsafe extern "system" fn(Enc, *mut c_void) -> u32;
type FnEnc = unsafe extern "system" fn(Enc) -> u32;
type FnCaps = unsafe extern "system" fn(Enc, Guid, *mut c_void, *mut i32) -> u32;
type FnErr = unsafe extern "system" fn(Enc) -> *const c_char;
type FnInvalidate = unsafe extern "system" fn(Enc, u64) -> u32;

struct Api {
    _lib: NvEncLib,
    open: FnOpen,
    preset_ex: FnPresetEx,
    caps: FnCaps,
    initialize: FnEncPtr,
    create_bitstream: FnEncPtr,
    destroy_bitstream: FnEncPtr,
    encode: FnEncPtr,
    lock: FnEncPtr,
    unlock: FnEncPtr,
    map: FnEncPtr,
    unmap: FnEncPtr,
    destroy: FnEnc,
    invalidate: FnInvalidate,
    register: FnEncPtr,
    unregister: FnEncPtr,
    reconfigure: FnEncPtr,
    last_error: FnErr,
}

impl Api {
    fn load() -> Result<Self> {
        let lib = NvEncLib::load().map_err(|e| format!("{e} (NVIDIA driver without NVENC?)"))?;
        let mut max = 0u32;
        // SAFETY: valid out pointer.
        unsafe { (lib.NvEncodeAPIGetMaxSupportedVersion)(&mut max) };
        let ours = ((L::NVENCAPI_VERSION & 0xff) << 4) | ((L::NVENCAPI_VERSION >> 24) & 0xf);
        if max < ours {
            return Err(format!(
                "NVIDIA driver supports NVENC API {}.{}, need {}.{}; update the driver",
                max >> 4,
                max & 0xf,
                ours >> 4,
                ours & 0xf
            ));
        }
        let mut fl = Blob::new(L::FUNCTION_LIST_SIZE);
        fl.set(L::FL_VERSION, L::FUNCTION_LIST_VER as u64);
        // SAFETY: fl is a correctly sized, versioned function list.
        let r = unsafe { (lib.NvEncodeAPICreateInstance)(fl.ptr()) };
        if r != 0 {
            return Err(format!("NvEncodeAPICreateInstance failed ({r})"));
        }
        // SAFETY: the driver filled the list; each slot holds the matching fn.
        unsafe fn get<T: Copy>(fl: &Blob, f: F) -> Result<T> {
            let p = fl.get_ptr(f);
            if p.is_null() {
                return Err(format!("NVENC function at offset {} missing", f.0));
            }
            Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
        }
        unsafe {
            Ok(Api {
                open: get(&fl, L::FL_OPEN_ENCODE_SESSION_EX)?,
                preset_ex: get(&fl, L::FL_GET_ENCODE_PRESET_CONFIG_EX)?,
                caps: get(&fl, L::FL_GET_ENCODE_CAPS)?,
                initialize: get(&fl, L::FL_INITIALIZE_ENCODER)?,
                create_bitstream: get(&fl, L::FL_CREATE_BITSTREAM_BUFFER)?,
                destroy_bitstream: get(&fl, L::FL_DESTROY_BITSTREAM_BUFFER)?,
                encode: get(&fl, L::FL_ENCODE_PICTURE)?,
                lock: get(&fl, L::FL_LOCK_BITSTREAM)?,
                unlock: get(&fl, L::FL_UNLOCK_BITSTREAM)?,
                map: get(&fl, L::FL_MAP_INPUT_RESOURCE)?,
                unmap: get(&fl, L::FL_UNMAP_INPUT_RESOURCE)?,
                destroy: get(&fl, L::FL_DESTROY_ENCODER)?,
                invalidate: get(&fl, L::FL_INVALIDATE_REF_FRAMES)?,
                register: get(&fl, L::FL_REGISTER_RESOURCE)?,
                unregister: get(&fl, L::FL_UNREGISTER_RESOURCE)?,
                reconfigure: get(&fl, L::FL_RECONFIGURE_ENCODER)?,
                last_error: get(&fl, L::FL_GET_LAST_ERROR_STRING)?,
                _lib: lib,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EncoderParams {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Full 4:4:4 chroma (not used on Windows yet: input is NV12).
    pub yuv444: bool,
    /// NVENC preset P1 (fastest) ..= P7 (best quality).
    pub preset: u8,
    /// VBV size in frames (1.0 = strictest latency).
    pub vbv_frames: f32,
}

impl Default for EncoderParams {
    fn default() -> Self {
        Self {
            codec: Codec::Hevc,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 30_000,
            yuv444: false,
            preset: 1,
            vbv_frames: 1.0,
        }
    }
}

pub struct NvEncoder {
    api: Api,
    enc: Enc,
    init: Blob,
    cfg: Blob,
    /// Registered input textures: (ID3D11Texture2D pointer, registration).
    inputs: Vec<(*mut c_void, *mut c_void)>,
    output: *mut c_void,
    params: EncoderParams,
    frame_idx: u32,
    buffer_format: u32,
    pub supports_rfi: bool,
}

// SAFETY: the encoder is used from one thread at a time.
unsafe impl Send for NvEncoder {}

fn codec_guid(c: Codec) -> Guid {
    match c {
        Codec::H264 => L::CODEC_H264,
        Codec::Hevc => L::CODEC_HEVC,
        Codec::Av1 => L::CODEC_AV1,
    }
}

fn preset_guid(p: u8) -> Guid {
    match p {
        0 | 1 => L::PRESET_P1,
        2 => L::PRESET_P2,
        3 => L::PRESET_P3,
        4 => L::PRESET_P4,
        5 => L::PRESET_P5,
        6 => L::PRESET_P6,
        _ => L::PRESET_P7,
    }
}

/// Offset a field of NV_ENC_CONFIG to its position inside another struct.
fn at(base: F, f: F) -> F {
    F(base.0 + f.0, f.1)
}

impl NvEncoder {
    /// `device`: the `ID3D11Device` the input textures belong to.
    pub fn new(device: *mut c_void, params: EncoderParams) -> Result<Self> {
        let api = Api::load()?;
        let mut enc: Enc = std::ptr::null_mut();
        let mut os = Blob::new(L::OPEN_SESSION_SIZE);
        os.set(L::OS_VERSION, L::OPEN_SESSION_VER as u64);
        os.set(L::OS_DEVICE_TYPE, DEVICE_TYPE_DIRECTX as u64);
        os.set_ptr(L::OS_DEVICE, device);
        os.set(L::OS_API_VERSION, L::NVENCAPI_VERSION as u64);
        // SAFETY: blob is a valid versioned struct.
        let r = unsafe { (api.open)(os.ptr(), &mut enc) };
        if r != 0 || enc.is_null() {
            return Err(format!(
                "nvEncOpenEncodeSessionEx failed ({r}); not an NVENC-capable NVIDIA GPU, or too many sessions"
            ));
        }
        let mut me = NvEncoder {
            api,
            enc,
            init: Blob::new(L::INIT_SIZE),
            cfg: Blob::new(L::CONFIG_SIZE),
            inputs: Vec::new(),
            output: std::ptr::null_mut(),
            params: EncoderParams { yuv444: false, ..params },
            frame_idx: 0,
            buffer_format: L::BUFFER_FORMAT_NV12,
            supports_rfi: false,
        };
        let guid = codec_guid(params.codec);
        let maxw = me.cap(guid, L::CAPS_WIDTH_MAX);
        let maxh = me.cap(guid, L::CAPS_HEIGHT_MAX);
        if maxw == 0 {
            return Err(format!("this GPU cannot encode {}", params.codec.name()));
        }
        if params.width as i32 > maxw || params.height as i32 > maxh {
            return Err(format!("{}x{} exceeds encoder max {maxw}x{maxh}", params.width, params.height));
        }
        me.supports_rfi = me.cap(guid, L::CAPS_SUPPORT_REF_PIC_INVALIDATION) != 0;
        me.configure()?;
        let mut bb = Blob::new(L::BITSTREAM_BUF_SIZE);
        bb.set(L::BB_VERSION, L::BITSTREAM_BUF_VER as u64);
        // SAFETY: valid versioned struct.
        let r = unsafe { (me.api.create_bitstream)(me.enc, bb.ptr()) };
        if r != 0 {
            return Err(me.err(r, "nvEncCreateBitstreamBuffer"));
        }
        me.output = bb.get_ptr(L::BB_BITSTREAM_BUFFER);
        Ok(me)
    }

    fn cap(&self, guid: Guid, which: u32) -> i32 {
        let mut c = Blob::new(L::CAPS_PARAM_SIZE);
        c.set(L::CAPS_VERSION, L::CAPS_PARAM_VER as u64);
        c.set(L::CAPS_TO_QUERY, which as u64);
        let mut v = 0i32;
        // SAFETY: valid encoder + versioned caps struct.
        let r = unsafe { (self.api.caps)(self.enc, guid, c.ptr(), &mut v) };
        if r != 0 { 0 } else { v }
    }

    fn err(&self, r: u32, what: &str) -> String {
        // SAFETY: returns a static/encoder-owned C string.
        let s = unsafe { (self.api.last_error)(self.enc) };
        let detail =
            if s.is_null() { String::new() } else { unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned() };
        format!("{what} failed ({r}): {detail}")
    }

    fn configure(&mut self) -> Result<()> {
        let p = self.params;
        let guid = codec_guid(p.codec);
        let preset = preset_guid(p.preset);
        let tuning = L::TUNING_ULTRA_LOW_LATENCY;

        let mut pc = Blob::new(L::PRESET_CONFIG_SIZE);
        pc.set(L::PC_VERSION, L::PRESET_CONFIG_VER as u64);
        pc.set(at(L::PC_PRESET_CFG, L::CFG_VERSION), L::CONFIG_VER as u64);
        // SAFETY: versioned preset config struct.
        let r = unsafe { (self.api.preset_ex)(self.enc, guid, preset, tuning, pc.ptr()) };
        if r != 0 {
            return Err(self.err(r, "nvEncGetEncodePresetConfigEx"));
        }
        let start = L::PC_PRESET_CFG.0;
        let preset_bytes = pc.bytes()[start..start + L::CONFIG_SIZE].to_vec();
        self.cfg.copy_in(0, &preset_bytes);
        let c = &mut self.cfg;
        c.set(L::CFG_VERSION, L::CONFIG_VER as u64);
        c.set(L::CFG_GOP_LENGTH, L::INFINITE_GOPLENGTH as u64);
        c.set(L::CFG_FRAME_INTERVAL_P, 1);

        let bps = p.bitrate_kbps as u64 * 1000;
        c.set(L::RC_MODE, L::RC_CBR as u64);
        c.set(L::RC_AVERAGE_BIT_RATE, bps);
        c.set(L::RC_MAX_BIT_RATE, bps);
        let vbv = (bps as f64 * p.vbv_frames as f64 / p.fps.max(1) as f64) as u64;
        c.set(L::RC_VBV_BUFFER_SIZE, vbv);
        c.set(L::RC_VBV_INITIAL_DELAY, vbv);
        c.set(L::RC_MULTI_PASS, L::MULTI_PASS_DISABLED as u64);
        c.set_bits(L::RC_ENABLE_LOOKAHEAD, 0);
        c.set_bits(L::RC_ZERO_REORDER_DELAY, 1);
        c.set_bits(L::RC_ENABLE_AQ, 0);
        c.set_bits(L::RC_ENABLE_TEMPORAL_AQ, 0);

        let chroma = if p.yuv444 { 3 } else { 1 };
        match p.codec {
            Codec::Hevc => {
                c.set_guid(L::CFG_PROFILE_GUID, if p.yuv444 { &L::HEVC_PROFILE_FREXT } else { &L::HEVC_PROFILE_MAIN });
                c.set(L::HEVC_IDR_PERIOD, L::INFINITE_GOPLENGTH as u64);
                c.set_bits(L::HEVC_REPEAT_SPSPPS, 1);
                c.set_bits(L::HEVC_CHROMA_FORMAT_IDC, chroma);
                c.set_bits(L::HEVC_ENABLE_INTRA_REFRESH, 0);
                c.set(L::HEVC_SLICE_MODE, 0);
                c.set(L::HEVC_SLICE_MODE_DATA, 0);
                c.set(L::HEVC_INPUT_BIT_DEPTH, L::BIT_DEPTH_8 as u64);
                c.set(L::HEVC_OUTPUT_BIT_DEPTH, L::BIT_DEPTH_8 as u64);
                c.set(L::HEVC_VUI_VIDEO_SIGNAL_TYPE_PRESENT, 1);
                c.set(L::HEVC_VUI_VIDEO_FORMAT, L::VUI_VIDEO_FORMAT_UNSPECIFIED as u64);
                c.set(L::HEVC_VUI_FULL_RANGE, 0);
                c.set(L::HEVC_VUI_COLOUR_DESCRIPTION_PRESENT, 1);
                c.set(L::HEVC_VUI_COLOUR_PRIMARIES, L::VUI_PRIMARIES_BT709 as u64);
                c.set(L::HEVC_VUI_TRANSFER, L::VUI_TRANSFER_BT709 as u64);
                c.set(L::HEVC_VUI_MATRIX, L::VUI_MATRIX_BT709 as u64);
                if self.supports_rfi {
                    c.set(L::HEVC_MAX_NUM_REF_FRAMES_IN_DPB, 5);
                }
            }
            Codec::H264 => {
                c.set_guid(
                    L::CFG_PROFILE_GUID,
                    if p.yuv444 { &L::H264_PROFILE_HIGH_444 } else { &L::H264_PROFILE_HIGH },
                );
                c.set(L::H264_IDR_PERIOD, L::INFINITE_GOPLENGTH as u64);
                c.set_bits(L::H264_REPEAT_SPSPPS, 1);
                c.set(L::H264_CHROMA_FORMAT_IDC, chroma as u64);
                c.set_bits(L::H264_ENABLE_INTRA_REFRESH, 0);
                c.set(L::H264_SLICE_MODE, 0);
                c.set(L::H264_SLICE_MODE_DATA, 0);
                c.set(L::H264_VUI_VIDEO_SIGNAL_TYPE_PRESENT, 1);
                c.set(L::H264_VUI_VIDEO_FORMAT, L::VUI_VIDEO_FORMAT_UNSPECIFIED as u64);
                c.set(L::H264_VUI_FULL_RANGE, 0);
                c.set(L::H264_VUI_COLOUR_DESCRIPTION_PRESENT, 1);
                c.set(L::H264_VUI_COLOUR_PRIMARIES, L::VUI_PRIMARIES_BT709 as u64);
                c.set(L::H264_VUI_TRANSFER, L::VUI_TRANSFER_BT709 as u64);
                c.set(L::H264_VUI_MATRIX, L::VUI_MATRIX_BT709 as u64);
                if self.supports_rfi {
                    c.set(L::H264_MAX_NUM_REF_FRAMES, 5);
                }
            }
            Codec::Av1 => {
                c.set_guid(L::CFG_PROFILE_GUID, &L::AV1_PROFILE_MAIN);
                c.set(L::AV1_IDR_PERIOD, L::INFINITE_GOPLENGTH as u64);
                c.set_bits(L::AV1_REPEAT_SEQ_HDR, 1);
                c.set_bits(L::AV1_CHROMA_FORMAT_IDC, 1);
                c.set_bits(L::AV1_ENABLE_INTRA_REFRESH, 0);
                c.set(L::AV1_INPUT_BIT_DEPTH, L::BIT_DEPTH_8 as u64);
                c.set(L::AV1_OUTPUT_BIT_DEPTH, L::BIT_DEPTH_8 as u64);
                c.set(L::AV1_COLOR_PRIMARIES, L::VUI_PRIMARIES_BT709 as u64);
                c.set(L::AV1_TRANSFER, L::VUI_TRANSFER_BT709 as u64);
                c.set(L::AV1_MATRIX, L::VUI_MATRIX_BT709 as u64);
                c.set(L::AV1_COLOR_RANGE, 0);
            }
        }

        let cfg_ptr = self.cfg.ptr();
        let i = &mut self.init;
        i.set(L::INIT_VERSION, L::INIT_VER as u64);
        i.set_guid(L::INIT_ENCODE_GUID, &guid);
        i.set_guid(L::INIT_PRESET_GUID, &preset);
        i.set(L::INIT_ENCODE_WIDTH, p.width as u64);
        i.set(L::INIT_ENCODE_HEIGHT, p.height as u64);
        i.set(L::INIT_DAR_WIDTH, p.width as u64);
        i.set(L::INIT_DAR_HEIGHT, p.height as u64);
        i.set(L::INIT_FRAME_RATE_NUM, p.fps as u64);
        i.set(L::INIT_FRAME_RATE_DEN, 1);
        i.set(L::INIT_ENABLE_ENCODE_ASYNC, 0);
        i.set(L::INIT_ENABLE_PTD, 1);
        i.set_ptr(L::INIT_ENCODE_CONFIG, cfg_ptr);
        i.set(L::INIT_MAX_ENCODE_WIDTH, p.width as u64);
        i.set(L::INIT_MAX_ENCODE_HEIGHT, p.height as u64);
        i.set(L::INIT_TUNING_INFO, tuning as u64);
        i.set(L::INIT_BUFFER_FORMAT, self.buffer_format as u64);
        // SAFETY: init blob is valid and points at cfg, which outlives the call.
        let r = unsafe { (self.api.initialize)(self.enc, self.init.ptr()) };
        if r != 0 {
            return Err(self.err(r, "nvEncInitializeEncoder"));
        }
        Ok(())
    }

    pub fn params(&self) -> EncoderParams {
        self.params
    }

    fn registration(&mut self, texture: *mut c_void) -> Result<*mut c_void> {
        if let Some(&(_, reg)) = self.inputs.iter().find(|(t, _)| *t == texture) {
            return Ok(reg);
        }
        if self.inputs.len() >= 8 {
            // The caller's texture ring changed (resize): forget old registrations.
            for (_, reg) in self.inputs.drain(..) {
                // SAFETY: registered by us, not mapped.
                unsafe { (self.api.unregister)(self.enc, reg) };
            }
        }
        let p = self.params;
        let mut rr = Blob::new(L::REGISTER_SIZE);
        rr.set(L::REG_VERSION, L::REGISTER_VER as u64);
        rr.set(L::REG_RESOURCE_TYPE, RESOURCE_TYPE_DIRECTX as u64);
        rr.set(L::REG_WIDTH, p.width as u64);
        rr.set(L::REG_HEIGHT, p.height as u64);
        rr.set(L::REG_PITCH, 0);
        rr.set_ptr(L::REG_RESOURCE, texture);
        rr.set(L::REG_BUFFER_FORMAT, self.buffer_format as u64);
        rr.set(L::REG_BUFFER_USAGE, L::BUFFER_USAGE_INPUT_IMAGE as u64);
        // SAFETY: valid versioned struct describing a live texture.
        let r = unsafe { (self.api.register)(self.enc, rr.ptr()) };
        if r != 0 {
            return Err(self.err(r, "nvEncRegisterResource"));
        }
        let reg = rr.get_ptr(L::REG_REGISTERED);
        self.inputs.push((texture, reg));
        Ok(reg)
    }

    /// Encode an NV12 `ID3D11Texture2D` of the configured size; appends the
    /// bitstream to `out`. Returns true if the output is a keyframe (IDR).
    pub fn encode(&mut self, texture: *mut c_void, timestamp: u64, force_idr: bool, out: &mut Vec<u8>) -> Result<bool> {
        let reg = self.registration(texture)?;
        let mut m = Blob::new(L::MAP_SIZE);
        m.set(L::MAP_VERSION, L::MAP_VER as u64);
        m.set_ptr(L::MAP_REGISTERED, reg);
        // SAFETY: registered resource is valid.
        let r = unsafe { (self.api.map)(self.enc, m.ptr()) };
        if r != 0 {
            return Err(self.err(r, "nvEncMapInputResource"));
        }
        let mapped = m.get_ptr(L::MAP_MAPPED);

        let p = self.params;
        let mut pic = Blob::new(L::PIC_SIZE);
        pic.set(L::PIC_VERSION, L::PIC_VER as u64);
        pic.set(L::PIC_INPUT_WIDTH, p.width as u64);
        pic.set(L::PIC_INPUT_HEIGHT, p.height as u64);
        pic.set(L::PIC_INPUT_PITCH, 0);
        let flags = if force_idr || self.frame_idx == 0 { L::PIC_FLAG_FORCEIDR | L::PIC_FLAG_OUTPUT_SPSPPS } else { 0 };
        pic.set(L::PIC_FLAGS, flags as u64);
        pic.set(L::PIC_FRAME_IDX, self.frame_idx as u64);
        pic.set(L::PIC_INPUT_TIMESTAMP, timestamp);
        pic.set_ptr(L::PIC_INPUT_BUFFER, mapped);
        pic.set_ptr(L::PIC_OUTPUT_BITSTREAM, self.output);
        pic.set(L::PIC_BUFFER_FMT, self.buffer_format as u64);
        pic.set(L::PIC_PICTURE_STRUCT, L::PIC_STRUCT_FRAME as u64);
        self.frame_idx = self.frame_idx.wrapping_add(1);
        // SAFETY: all referenced resources are alive.
        let r = unsafe { (self.api.encode)(self.enc, pic.ptr()) };
        let result = if r != 0 && r != L::NV_ENC_ERR_NEED_MORE_INPUT {
            Err(self.err(r, "nvEncEncodePicture"))
        } else {
            let mut lk = Blob::new(L::LOCK_SIZE);
            lk.set(L::LOCK_VERSION, L::LOCK_VER as u64);
            lk.set_ptr(L::LOCK_OUTPUT_BITSTREAM, self.output);
            // SAFETY: output buffer is valid; lock waits for completion.
            let r = unsafe { (self.api.lock)(self.enc, lk.ptr()) };
            if r != 0 {
                Err(self.err(r, "nvEncLockBitstream"))
            } else {
                let size = lk.get(L::LOCK_BITSTREAM_SIZE) as usize;
                let ptr = lk.get_ptr(L::LOCK_BITSTREAM_PTR) as *const u8;
                // SAFETY: driver guarantees `size` readable bytes while locked.
                out.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, size) });
                let pt = lk.get(L::LOCK_PICTURE_TYPE) as u32;
                // SAFETY: matching unlock.
                unsafe { (self.api.unlock)(self.enc, self.output) };
                Ok(pt == L::PIC_TYPE_IDR || pt == L::PIC_TYPE_I)
            }
        };
        // SAFETY: unmap what we mapped.
        unsafe { (self.api.unmap)(self.enc, mapped) };
        result
    }

    /// Change the target bitrate without an IDR.
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        if kbps == self.params.bitrate_kbps {
            return Ok(());
        }
        let bps = kbps as u64 * 1000;
        let vbv = (bps as f64 * self.params.vbv_frames as f64 / self.params.fps.max(1) as f64) as u64;
        self.cfg.set(L::RC_AVERAGE_BIT_RATE, bps);
        self.cfg.set(L::RC_MAX_BIT_RATE, bps);
        self.cfg.set(L::RC_VBV_BUFFER_SIZE, vbv);
        self.cfg.set(L::RC_VBV_INITIAL_DELAY, vbv);
        let mut rc = Blob::new(L::RECONF_SIZE);
        rc.set(L::RECONF_VERSION, L::RECONF_VER as u64);
        let init = self.init.bytes().to_vec();
        rc.copy_in(L::RECONF_REINIT_PARAMS.0, &init);
        rc.set_bits(L::RECONF_RESET_ENCODER, 0);
        rc.set_bits(L::RECONF_FORCE_IDR, 0);
        // SAFETY: reinit params point at our live cfg blob.
        let r = unsafe { (self.api.reconfigure)(self.enc, rc.ptr()) };
        if r != 0 {
            return Err(self.err(r, "nvEncReconfigureEncoder"));
        }
        self.params.bitrate_kbps = kbps;
        Ok(())
    }

    /// Invalidate reference frames encoded at/after `timestamp`.
    pub fn invalidate_refs(&mut self, timestamp: u64) -> Result<()> {
        // SAFETY: valid encoder.
        let r = unsafe { (self.api.invalidate)(self.enc, timestamp) };
        if r != 0 { Err(self.err(r, "nvEncInvalidateRefFrames")) } else { Ok(()) }
    }
}

impl Drop for NvEncoder {
    fn drop(&mut self) {
        // SAFETY: tearing down resources we created, in reverse order.
        unsafe {
            if !self.output.is_null() {
                (self.api.destroy_bitstream)(self.enc, self.output);
            }
            for (_, reg) in &self.inputs {
                (self.api.unregister)(self.enc, *reg);
            }
            (self.api.destroy)(self.enc);
        }
    }
}
