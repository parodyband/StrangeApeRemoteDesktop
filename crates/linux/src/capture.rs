//! Screen capture via xdg-desktop-portal + PipeWire (in progress).

use sard_core::media::{self, InputSink, SourceOptions, VideoSource};

pub fn open(_opts: &SourceOptions) -> media::Result<(Box<dyn VideoSource>, Box<dyn InputSink>)> {
    Err(media::err("screen capture is not implemented yet; use --test-pattern"))
}
