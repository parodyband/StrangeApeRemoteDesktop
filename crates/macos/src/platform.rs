//! macOS implementation of the sard platform API (see `sard/src/platform.rs`).

use crate::input::LogInput;
use crate::sink::{HeadlessSink, MetalVideoSink};
use crate::test_source::TestPatternSource;
use crate::vt::{self, EncoderConfig, RateControl, VtEncoder};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use sard_core::media::{self, InputSink, SourceOptions, VideoSink, VideoSource};
use sard_core::proto::Codec;
use std::sync::{Arc, OnceLock};

const ALL: [Codec; 3] = [Codec::Hevc, Codec::H264, Codec::Av1];

pub fn open_host(opts: &SourceOptions) -> media::Result<(Box<dyn VideoSource>, Box<dyn InputSink>)> {
    if opts.test_pattern {
        let src = TestPatternSource::new(opts)?;
        let input = LogInput { pointer: Arc::clone(&src.pointer) };
        return Ok((Box::new(src), Box::new(input)));
    }
    crate::capture::open(opts)
}

/// Called on the main thread right after the window is created.
pub fn create_video_sink(
    display: RawDisplayHandle,
    window: RawWindowHandle,
    width: u32,
    height: u32,
) -> media::Result<Box<dyn VideoSink>> {
    Ok(Box::new(MetalVideoSink::new(display, window, width, height)?))
}

/// Decode-only client sink (`sard connect --headless`).
pub fn create_headless_sink() -> media::Result<Box<dyn VideoSink>> {
    Ok(Box::new(HeadlessSink::default()))
}

struct Caps {
    gpu: String,
    os: String,
    enc: u8,
    dec: u8,
    enc_notes: Vec<String>,
    hw_dec: u8,
}

fn sysctl_string(name: &std::ffi::CStr) -> Option<String> {
    let mut buf = [0u8; 256];
    let mut len = buf.len();
    // SAFETY: valid buffer and length pointers.
    let rc = unsafe { libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr() as _, &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 || len == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..len]).trim_end_matches('\0').to_string())
}

fn caps() -> &'static Caps {
    static C: OnceLock<Caps> = OnceLock::new();
    C.get_or_init(|| {
        let gpu = objc2::rc::autoreleasepool(|_| crate::metal::Renderer::new().map(|r| r.device_name))
            .or_else(|_| sysctl_string(c"machdep.cpu.brand_string").ok_or(()))
            .unwrap_or_else(|_| "unknown GPU".into());
        let os = sysctl_string(c"kern.osproductversion").unwrap_or_default();
        let mut enc = 0;
        let mut enc_notes = Vec::new();
        for c in [Codec::Hevc, Codec::H264] {
            let cfg = EncoderConfig {
                codec: c,
                width: 640,
                height: 360,
                fps: 60,
                bitrate_kbps: 4000,
                rate_control: RateControl::LowLatency,
            };
            match VtEncoder::new(cfg) {
                Ok(e) => {
                    enc |= c.bit();
                    enc_notes.push(format!(
                        "{} ({}{})",
                        c.name(),
                        if e.hardware { "hardware" } else { "software" },
                        if e.low_latency { ", low-latency RC available" } else { "" }
                    ));
                }
                Err(e) => sard_core::debug!("no {} encoder: {e}", c.name()),
            }
        }
        // VideoToolbox always has H.264/HEVC decoders (software fallback);
        // AV1 is not wired up in this backend.
        let dec = Codec::Hevc.bit() | Codec::H264.bit();
        let hw_dec = ALL.iter().filter(|&&c| vt::hardware_decode_supported(c)).fold(0, |m, c| m | c.bit());
        Caps { gpu, os, enc, dec, enc_notes, hw_dec }
    })
}

pub fn encoders() -> u8 {
    caps().enc
}

pub fn decoders() -> u8 {
    caps().dec
}

fn names(mask: u8) -> String {
    let v: Vec<&str> = ALL.iter().filter(|c| mask & c.bit() != 0).map(|c| c.name()).collect();
    if v.is_empty() { "none".into() } else { v.join(", ") }
}

pub fn describe() -> String {
    let c = caps();
    format!("macOS {}, {}", c.os, c.gpu)
}

pub fn capabilities() -> Vec<String> {
    let c = caps();
    let input = if crate::input::accessibility_trusted() { "granted" } else { "not granted" };
    // SAFETY: plain query.
    let screen = if unsafe { crate::ffi::CGPreflightScreenCaptureAccess() } { "granted" } else { "not granted" };
    vec![
        format!("GPU:     {}", c.gpu),
        format!(
            "encode:  {} (VideoToolbox)",
            if c.enc_notes.is_empty() { "none".to_string() } else { c.enc_notes.join(", ") }
        ),
        format!("decode:  {} (VideoToolbox; hardware: {})", names(c.dec), names(c.hw_dec)),
        format!("screen recording permission: {screen}"),
        format!("accessibility permission:    {input}"),
    ]
}
