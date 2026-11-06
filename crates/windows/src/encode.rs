//! Hardware video encoding of NV12 Direct3D 11 textures.
//!
//! * NVIDIA: NVENC with D3D11 input (`nv/nvenc.rs`), configured exactly like
//!   the Linux host.
//! * AMD, Intel, others: the GPU vendor's Media Foundation encoder MFT (AMF,
//!   Quick Sync), driven asynchronously, in low-latency CBR mode without
//!   B-frames.
//!
//! `SARD_ENCODER=mf` forces Media Foundation on NVIDIA too (testing).

use crate::d3d::{self, Device};
use crate::mf::{self, Mft};
use crate::nv::nvenc::{EncoderParams, NvEncoder};
use crate::sys::GUID;
use sard_core::proto::Codec;
use sard_core::{debug, info, warn};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub preset: u8,
}

/// One encoded access unit.
pub struct Packet {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub timestamp_us: u64,
}

enum Backend {
    Nvenc(Box<NvEncoder>),
    Mf(Box<MfEncoder>),
}

pub struct Encoder {
    backend: Backend,
    pub settings: Settings,
    pub name: String,
    /// Why NVENC wasn't used (for diagnostics), if it was skipped or failed.
    pub fallback_reason: Option<String>,
}

// SAFETY: used by one thread at a time (the host's video thread).
unsafe impl Send for Encoder {}

pub fn force_mf() -> bool {
    std::env::var("SARD_ENCODER").is_ok_and(|v| v.eq_ignore_ascii_case("mf"))
}

impl Encoder {
    /// Pick the encoder for `dev`'s GPU (`vendor`): NVENC on NVIDIA, else Media Foundation.
    pub fn new(dev: &Device, vendor: u32, s: Settings) -> Result<Encoder, String> {
        let fallback_reason = if vendor == d3d::VENDOR_NVIDIA && !force_mf() {
            let p = EncoderParams {
                codec: s.codec,
                width: s.width,
                height: s.height,
                fps: s.fps,
                bitrate_kbps: s.bitrate_kbps.max(1000),
                yuv444: false,
                preset: s.preset,
                vbv_frames: 1.0,
            };
            match NvEncoder::new(crate::com::Interface::as_raw(&dev.device), p) {
                Ok(e) => {
                    return Ok(Encoder {
                        backend: Backend::Nvenc(Box::new(e)),
                        settings: s,
                        name: "NVENC".into(),
                        fallback_reason: None,
                    });
                }
                Err(e) => {
                    warn!("NVENC {}: {e}; trying Media Foundation", s.codec.name());
                    format!("NVENC failed: {e}")
                }
            }
        } else if vendor == d3d::VENDOR_NVIDIA {
            "SARD_ENCODER=mf".into()
        } else {
            format!("{} GPU (NVENC needs NVIDIA)", d3d::vendor_name(vendor))
        };
        let fallback_reason = Some(fallback_reason);
        let m = MfEncoder::new(dev, vendor, s)?;
        let name = format!("Media Foundation: {}", m.mft.name);
        Ok(Encoder { backend: Backend::Mf(Box::new(m)), settings: s, name, fallback_reason })
    }

    /// Encode an NV12 texture of the configured size. Returns finished access
    /// units (usually exactly one; asynchronous encoders may lag by one).
    pub fn encode(
        &mut self,
        tex: &d3d::ID3D11Texture2D,
        timestamp_us: u64,
        force_key: bool,
    ) -> Result<Vec<Packet>, String> {
        match &mut self.backend {
            Backend::Nvenc(e) => {
                let mut data = Vec::with_capacity(256 << 10);
                let keyframe = e.encode(crate::com::Interface::as_raw(tex), timestamp_us, force_key, &mut data)?;
                Ok(vec![Packet { data, keyframe, timestamp_us }])
            }
            Backend::Mf(e) => e.encode(tex, timestamp_us, force_key),
        }
    }

    pub fn set_bitrate(&mut self, kbps: u32) {
        let r = match &mut self.backend {
            Backend::Nvenc(e) => e.set_bitrate(kbps),
            Backend::Mf(e) => e.set_bitrate(kbps),
        };
        match r {
            Ok(()) => self.settings.bitrate_kbps = kbps,
            Err(e) => warn!("set_bitrate: {e}"),
        }
    }

    pub fn is_nvenc(&self) -> bool {
        matches!(self.backend, Backend::Nvenc(_))
    }
}

// ---------------------------------------------------------------------------
// Media Foundation

fn subtype(c: Codec) -> GUID {
    crate::decode::mf_subtype(c)
}

pub struct MfEncoder {
    pub mft: Mft,
    _manager: mf::IMFDXGIDeviceManager,
    api: Option<mf::ICodecAPI>,
    codec: Codec,
    fps: u32,
    /// Codec configuration from the output type (prepended to keyframes that lack it).
    sequence_header: Option<Vec<u8>>,
    frames: u64,
}

fn set_codec_value(api: &mf::ICodecAPI, key: &GUID, mut v: mf::VARIANT, what: &str) {
    // SAFETY: valid GUID and VARIANT for the property.
    let hr = unsafe { api.SetValue(key, &mut v) };
    if hr < 0 {
        debug!("encoder ignores {what} ({:#010X})", hr as u32);
    }
}

/// Hardware encoder MFTs for `codec`, the GPU vendor's first.
pub fn mf_encoders(codec: Codec, vendor: u32) -> Result<Vec<mf::MftInfo>, String> {
    let flags = mf::MFT_ENUM_FLAG_HARDWARE | mf::MFT_ENUM_FLAG_ASYNCMFT | mf::MFT_ENUM_FLAG_SORTANDFILTER;
    let mut list =
        mf::enumerate(mf::MFT_CATEGORY_VIDEO_ENCODER, flags, Some(mf::MFVideoFormat_NV12), Some(subtype(codec)))?;
    list.sort_by_key(|m| m.vendor != Some(vendor));
    Ok(list)
}

impl MfEncoder {
    pub fn new(dev: &Device, vendor: u32, s: Settings) -> Result<MfEncoder, String> {
        let list = mf_encoders(s.codec, vendor)?;
        if list.is_empty() {
            return Err(format!(
                "no hardware {} encoder: this GPU/driver has no Media Foundation encoder for it",
                s.codec.name()
            ));
        }
        let manager = mf::device_manager(&dev.device)?;
        let mut errors = Vec::new();
        for m in &list {
            if m.vendor.is_some_and(|v| v != vendor) {
                // Another GPU's encoder can't read this device's textures.
                errors.push(format!("{}: belongs to another GPU", m.name));
                continue;
            }
            match Self::setup(m, &manager, s) {
                Ok(e) => {
                    info!("encoder: {} ({}, {}x{} @ {} fps)", m.name, s.codec.name(), s.width, s.height, s.fps);
                    return Ok(e);
                }
                Err(e) => {
                    debug!("encoder {} unusable: {e}", m.name);
                    errors.push(format!("{}: {e}", m.name));
                }
            }
        }
        Err(format!("no usable {} encoder ({})", s.codec.name(), errors.join("; ")))
    }

    fn setup(info: &mf::MftInfo, manager: &mf::IMFDXGIDeviceManager, s: Settings) -> Result<MfEncoder, String> {
        let mut mft = Mft::activate(info)?;
        mft.set_d3d_manager(manager)?;
        let api = mft.codec_api();
        let bps = s.bitrate_kbps.max(1000) * 1000;
        if let Some(a) = &api {
            set_codec_value(a, &mf::CODECAPI_AVLowLatencyMode, mf::VARIANT::bool(true), "low-latency mode");
            set_codec_value(
                a,
                &mf::CODECAPI_AVEncCommonRateControlMode,
                mf::VARIANT::u32(mf::eAVEncCommonRateControlMode_CBR),
                "CBR",
            );
            set_codec_value(a, &mf::CODECAPI_AVEncCommonMeanBitRate, mf::VARIANT::u32(bps), "bitrate");
            // One frame of VBV, as on Linux: the smallest latency CBR allows.
            set_codec_value(a, &mf::CODECAPI_AVEncCommonBufferSize, mf::VARIANT::u32(bps / s.fps.max(1)), "VBV size");
            set_codec_value(a, &mf::CODECAPI_AVEncMPVDefaultBPictureCount, mf::VARIANT::u32(0), "no B-frames");
            // Long GOP: keyframes come on request (loss recovery), not on a timer.
            set_codec_value(a, &mf::CODECAPI_AVEncMPVGOPSize, mf::VARIANT::u32(s.fps.max(1) * 600), "GOP size");
            let speed = 20 + (s.preset.clamp(1, 7) as u32 - 1) * 80 / 6;
            set_codec_value(a, &mf::CODECAPI_AVEncCommonQualityVsSpeed, mf::VARIANT::u32(speed), "quality/speed");
        }
        // Encoders want the output (compressed) type first.
        let out = mf::media_type()?;
        let inp = mf::media_type()?;
        // SAFETY: plain attribute setters on our own media types.
        unsafe {
            out.SetGUID(&mf::MF_MT_MAJOR_TYPE, &mf::MFMediaType_Video);
            out.SetGUID(&mf::MF_MT_SUBTYPE, &subtype(s.codec));
            out.SetUINT32(&mf::MF_MT_AVG_BITRATE, bps);
            out.SetUINT64(&mf::MF_MT_FRAME_SIZE, mf::pack(s.width, s.height));
            out.SetUINT64(&mf::MF_MT_FRAME_RATE, mf::pack(s.fps.max(1), 1));
            out.SetUINT64(&mf::MF_MT_PIXEL_ASPECT_RATIO, mf::pack(1, 1));
            out.SetUINT32(&mf::MF_MT_INTERLACE_MODE, mf::MFVideoInterlace_Progressive);
            match s.codec {
                Codec::H264 => {
                    out.SetUINT32(&mf::MF_MT_MPEG2_PROFILE, mf::eAVEncH264VProfile_High);
                }
                Codec::Hevc => {
                    out.SetUINT32(&mf::MF_MT_MPEG2_PROFILE, mf::eAVEncH265VProfile_Main_420_8);
                }
                Codec::Av1 => {}
            }
            for t in [&out, &inp] {
                t.SetUINT32(&mf::MF_MT_VIDEO_NOMINAL_RANGE, mf::MFNominalRange_16_235);
                t.SetUINT32(&mf::MF_MT_YUV_MATRIX, mf::MFVideoTransferMatrix_BT709);
                t.SetUINT32(&mf::MF_MT_VIDEO_PRIMARIES, mf::MFVideoPrimaries_BT709);
                t.SetUINT32(&mf::MF_MT_TRANSFER_FUNCTION, mf::MFVideoTransFunc_709);
            }
            inp.SetGUID(&mf::MF_MT_MAJOR_TYPE, &mf::MFMediaType_Video);
            inp.SetGUID(&mf::MF_MT_SUBTYPE, &mf::MFVideoFormat_NV12);
            inp.SetUINT64(&mf::MF_MT_FRAME_SIZE, mf::pack(s.width, s.height));
            inp.SetUINT64(&mf::MF_MT_FRAME_RATE, mf::pack(s.fps.max(1), 1));
            inp.SetUINT64(&mf::MF_MT_PIXEL_ASPECT_RATIO, mf::pack(1, 1));
            inp.SetUINT32(&mf::MF_MT_INTERLACE_MODE, mf::MFVideoInterlace_Progressive);
        }
        mft.set_output_type(&out)?;
        mft.set_input_type(&inp)?;
        let sequence_header = mft.current_output().ok().and_then(|t| mf::blob(&t, &mf::MF_MT_MPEG_SEQUENCE_HEADER));
        mft.start()?;
        Ok(MfEncoder {
            mft,
            _manager: manager.clone(),
            api,
            codec: s.codec,
            fps: s.fps.max(1),
            sequence_header,
            frames: 0,
        })
    }

    fn collect(&mut self, wait: Duration, out: &mut Vec<Packet>) -> Result<(), String> {
        let mut wait = wait;
        loop {
            match self.mft.output(wait)? {
                mf::Output::Sample(s) => {
                    let mut data = Vec::new();
                    mf::sample_bytes(&s, &mut data)?;
                    let mut clean = 0u32;
                    let mut t = 0i64;
                    // SAFETY: valid out pointers.
                    unsafe {
                        s.GetUINT32(&mf::MFSampleExtension_CleanPoint, &mut clean);
                        s.GetSampleTime(&mut t);
                    }
                    let keyframe = clean != 0;
                    if keyframe
                        && !has_parameter_sets(self.codec, &data)
                        && let Some(h) = &self.sequence_header
                    {
                        data.splice(0..0, h.iter().copied());
                    }
                    out.push(Packet { data, keyframe, timestamp_us: (t.max(0) / 10) as u64 });
                    wait = Duration::ZERO;
                }
                mf::Output::StreamChange => {
                    // Encoders renegotiate rarely; keep our output type.
                    debug!("encoder stream change");
                }
                mf::Output::NeedInput => return Ok(()),
            }
        }
    }

    pub fn encode(
        &mut self,
        tex: &d3d::ID3D11Texture2D,
        timestamp_us: u64,
        force_key: bool,
    ) -> Result<Vec<Packet>, String> {
        let mut out = Vec::new();
        // Anything that finished since last time first.
        self.collect(Duration::ZERO, &mut out)?;
        if (force_key || self.frames == 0)
            && let Some(a) = &self.api
        {
            set_codec_value(a, &mf::CODECAPI_AVEncVideoForceKeyFrame, mf::VARIANT::u32(1), "force keyframe");
        }
        let duration = 10_000_000 / self.fps as i64;
        let sample = mf::texture_sample(tex, timestamp_us as i64 * 10, duration)?;
        self.mft.input(&sample, Duration::from_millis(200))?;
        self.frames += 1;
        // Low-latency hardware encoders finish a frame in a few ms.
        self.collect(Duration::from_millis(if out.is_empty() { 40 } else { 0 }), &mut out)?;
        Ok(out)
    }

    pub fn set_bitrate(&mut self, kbps: u32) -> Result<(), String> {
        let Some(a) = &self.api else { return Err("encoder has no ICodecAPI".into()) };
        let bps = kbps.max(1000) * 1000;
        let mut v = mf::VARIANT::u32(bps);
        // SAFETY: valid GUID and VARIANT.
        crate::sys::check(unsafe { a.SetValue(&mf::CODECAPI_AVEncCommonMeanBitRate, &mut v) }, "set bitrate")
    }
}

/// Whether an access unit carries its codec configuration (SPS / VPS / sequence header).
pub fn has_parameter_sets(codec: Codec, data: &[u8]) -> bool {
    match codec {
        Codec::H264 | Codec::Hevc => {
            let mut i = 0;
            while i + 3 < data.len() {
                if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                    let h = data[i + 3];
                    let ok = match codec {
                        Codec::H264 => h & 0x1f == 7,
                        _ => (h >> 1) & 0x3f == 32,
                    };
                    if ok {
                        return true;
                    }
                    i += 3;
                } else {
                    i += 1;
                }
            }
            false
        }
        // OBU_SEQUENCE_HEADER (type 1) as the first OBU after any temporal delimiter.
        Codec::Av1 => {
            let mut i = 0;
            while i < data.len() {
                let h = data[i];
                let ty = (h >> 3) & 0xf;
                if ty == 1 {
                    return true;
                }
                if ty != 2 {
                    return false;
                }
                // Temporal delimiter: header (+ extension) + leb128 size (0).
                i += 1 + ((h >> 2) & 1) as usize;
                let mut size = 0usize;
                let mut shift = 0;
                while i < data.len() {
                    let b = data[i];
                    i += 1;
                    size |= ((b & 0x7f) as usize) << shift;
                    shift += 7;
                    if b & 0x80 == 0 {
                        break;
                    }
                }
                i += size;
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_set_detection() {
        assert!(has_parameter_sets(Codec::H264, &[0, 0, 0, 1, 0x67, 1, 2]));
        assert!(!has_parameter_sets(Codec::H264, &[0, 0, 0, 1, 0x65, 1, 2]));
        assert!(has_parameter_sets(Codec::Hevc, &[0, 0, 1, 0x40, 1]));
        assert!(!has_parameter_sets(Codec::Hevc, &[0, 0, 1, 0x26, 1]));
        // AV1: temporal delimiter (0x12 0x00) then sequence header (0x0a ...).
        assert!(has_parameter_sets(Codec::Av1, &[0x12, 0x00, 0x0a, 0x0b]));
        assert!(!has_parameter_sets(Codec::Av1, &[0x12, 0x00, 0x32, 0x0b]));
    }
}
