//! macOS implementation of the sard platform API (see `sard/src/platform.rs`).

use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use sard_core::media::{self, InputSink, SourceOptions, VideoSink, VideoSource};
use sard_core::proto::Codec;

pub fn open_host(_opts: &SourceOptions) -> media::Result<(Box<dyn VideoSource>, Box<dyn InputSink>)> {
    Err(media::err("macOS host not implemented yet"))
}

pub fn create_video_sink(
    _display: RawDisplayHandle,
    _window: RawWindowHandle,
    _width: u32,
    _height: u32,
) -> media::Result<Box<dyn VideoSink>> {
    Err(media::err("macOS client not implemented yet"))
}

pub fn encoders() -> u8 {
    Codec::Hevc.bit() | Codec::H264.bit()
}

pub fn decoders() -> u8 {
    Codec::Hevc.bit() | Codec::H264.bit()
}

pub fn describe() -> String {
    "macOS (VideoToolbox)".into()
}

pub fn capabilities() -> Vec<String> {
    vec!["VideoToolbox: hevc, h264".into()]
}
