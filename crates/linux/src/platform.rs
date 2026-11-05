//! Linux implementation of the sard platform API (see `sard/src/platform.rs`).

use crate::nv::cuda::Cuda;
use crate::nv::nvdec::NvDecoder;
use crate::nv::nvenc::{EncoderParams, NvEncoder};
use crate::sink::GlVideoSink;
use crate::test_source::{LogInput, TestPatternSource};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use sard_core::media::{self, AudioCallback, AudioSink, AudioSource, InputSink, SourceOptions, VideoSink, VideoSource};
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

pub fn create_video_sink(
    display: RawDisplayHandle,
    window: RawWindowHandle,
    width: u32,
    height: u32,
) -> media::Result<Box<dyn VideoSink>> {
    Ok(Box::new(GlVideoSink::new(display, window, width, height)?))
}

/// Capture what this machine is playing (host side).
pub fn open_audio_source(cb: AudioCallback) -> media::Result<Box<dyn AudioSource>> {
    crate::pw::audio::open_source(cb)
}

/// Play remote audio (client side).
pub fn open_audio_sink() -> media::Result<Box<dyn AudioSink>> {
    crate::pw::audio::open_sink()
}

/// The desktop's text clipboard, if reachable.
pub fn open_clipboard() -> Option<Box<dyn sard_core::media::Clipboard>> {
    let c = crate::clipboard::LinuxClipboard::open()?;
    sard_core::debug!("clipboard via {}", c.backend_name());
    Some(Box::new(c))
}

struct Caps {
    gpu: String,
    enc: u8,
    dec: u8,
    note: Option<String>,
}

fn caps() -> &'static Caps {
    static C: OnceLock<Caps> = OnceLock::new();
    C.get_or_init(|| {
        let cuda = match Cuda::new() {
            Ok(c) => c,
            Err(e) => {
                return Caps { gpu: "no NVIDIA GPU".into(), enc: 0, dec: 0, note: Some(e) };
            }
        };
        let mut enc = 0;
        let mut dec = 0;
        let mut note = None;
        for c in ALL {
            let p =
                EncoderParams { codec: c, width: 256, height: 256, fps: 30, bitrate_kbps: 1000, ..Default::default() };
            match NvEncoder::new(Arc::clone(&cuda), p) {
                Ok(_) => enc |= c.bit(),
                Err(e) => {
                    note.get_or_insert(e);
                }
            }
            if NvDecoder::new(Arc::clone(&cuda), c).is_ok() {
                dec |= c.bit();
            }
        }
        Caps { gpu: cuda.device_name.clone(), enc, dec, note }
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
    format!("Linux, {}", caps().gpu)
}

pub fn capabilities() -> Vec<String> {
    let c = caps();
    let mut v = vec![
        format!("GPU:     {}", c.gpu),
        format!("encode:  {} (NVENC)", names(c.enc)),
        format!("decode:  {} (NVDEC)", names(c.dec)),
    ];
    if let Some(n) = &c.note
        && c.enc == 0
    {
        v.push(format!("note:    {n}"));
    }
    v
}
