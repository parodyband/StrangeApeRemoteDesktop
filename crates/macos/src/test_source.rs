//! Synthetic video source: animated NV12 pattern in IOSurface pixel buffers,
//! encoded with VideoToolbox. Needs no macOS permissions.

use crate::pattern::Pattern;
use crate::vt::{self, EncoderConfig, RateControl, VtEncoder};
use sard_core::clock;
use sard_core::media::{self, CursorMode, EncodedFrame, SourceOptions, VideoSource};
use sard_core::proto::Codec;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct TestPatternSource {
    enc: VtEncoder,
    pattern: Pattern,
    interval: Duration,
    next_due: Instant,
    t: u32,
    force_key: bool,
    /// Last pointer position reported by the input sink (drawn as a marker).
    pub pointer: Arc<Mutex<(f32, f32)>>,
}

impl TestPatternSource {
    pub fn new(opts: &SourceOptions) -> media::Result<Self> {
        let (w, h) = ((opts.test_size.0 & !1).max(64), (opts.test_size.1 & !1).max(64));
        let enc = VtEncoder::new(EncoderConfig {
            codec: opts.codec,
            width: w,
            height: h,
            fps: opts.fps.max(1),
            bitrate_kbps: opts.bitrate_kbps.max(1000),
            rate_control: RateControl::for_preset(opts.codec, opts.preset),
        })
        .map_err(media::err)?;
        Ok(TestPatternSource {
            enc,
            pattern: Pattern::new(w as usize, h as usize),
            interval: Duration::from_secs_f64(1.0 / opts.fps.max(1) as f64),
            next_due: Instant::now(),
            t: 0,
            force_key: true,
            pointer: Arc::new(Mutex::new((0.5, 0.5))),
        })
    }
}

impl VideoSource for TestPatternSource {
    fn next_frame(&mut self, timeout: Duration) -> media::Result<Option<EncodedFrame>> {
        let now = Instant::now();
        if self.next_due > now {
            let wait = self.next_due - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
        }
        self.next_due = if Instant::now() > self.next_due + self.interval {
            Instant::now() + self.interval
        } else {
            self.next_due + self.interval
        };
        let pointer = *self.pointer.lock().unwrap();
        self.pattern.render(self.t, pointer);
        let capture_us = clock::now_us();
        let pb = self.enc.pixel_buffer().map_err(media::err)?;
        vt::write_nv12(&pb, &self.pattern.frame, self.pattern.w, self.pattern.h).map_err(media::err)?;
        vt::tag_bt709(&pb);
        let key = std::mem::take(&mut self.force_key);
        let out = self.enc.encode(&pb, capture_us, key).map_err(media::err)?;
        self.t = self.t.wrapping_add(1);
        let Some(out) = out else {
            self.force_key |= key;
            return Ok(None);
        };
        let c = self.enc.config();
        Ok(Some(EncodedFrame { data: out.data, keyframe: out.keyframe, capture_us, width: c.width, height: c.height }))
    }

    fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.enc.set_bitrate(kbps);
    }

    fn codec(&self) -> Codec {
        self.enc.config().codec
    }

    fn size(&self) -> (u32, u32) {
        (self.enc.config().width, self.enc.config().height)
    }

    /// No cursor in the pattern: let the client keep its local pointer visible.
    fn cursor_mode(&self) -> CursorMode {
        CursorMode::Metadata
    }

    fn describe(&self) -> String {
        let c = self.enc.config();
        format!(
            "test pattern {}x{} → VideoToolbox {}{}{}",
            c.width,
            c.height,
            c.codec.name(),
            if self.enc.hardware { " (hardware" } else { " (software" },
            if self.enc.low_latency { ", low-latency RC)" } else { ")" }
        )
    }
}
