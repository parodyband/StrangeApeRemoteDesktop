//! Synthetic video source: an animated NV12 pattern encoded with NVENC.
//! Used for testing the full pipeline without screen-capture permission.

use crate::nv::cuda::{Cuda, Mem};
use crate::nv::nvenc::{EncoderParams, NvEncoder};
use crate::yuv::{rgb_to_yuv709, test_pattern_nv12};
use sard_core::clock;
use sard_core::media::{self, EncodedFrame, InputEvent, InputSink, SourceOptions, VideoSource};
use sard_core::proto::Codec;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct TestPatternSource {
    enc: NvEncoder,
    cuda: Arc<Cuda>,
    background: Vec<u8>,
    frame: Vec<u8>,
    w: usize,
    h: usize,
    interval: Duration,
    next_due: Instant,
    t: u32,
    force_key: bool,
    codec: Codec,
    /// Last pointer position reported by the input sink (drawn as a marker).
    pub pointer: Arc<std::sync::Mutex<(f32, f32)>>,
}

impl TestPatternSource {
    pub fn new(opts: &SourceOptions) -> media::Result<Self> {
        let cuda = Cuda::new()?;
        let (w, h) = (opts.test_size.0 & !1, opts.test_size.1 & !1);
        let enc = NvEncoder::new(
            Arc::clone(&cuda),
            EncoderParams {
                codec: opts.codec,
                width: w,
                height: h,
                fps: opts.fps,
                bitrate_kbps: opts.bitrate_kbps.max(1000),
                yuv444: false,
                preset: opts.preset,
                vbv_frames: 1.0,
            },
        )?;
        let (w, h) = (w as usize, h as usize);
        let mut background = vec![0u8; w * h * 3 / 2];
        test_pattern_nv12(w, h, w, 0, &mut background);
        Ok(TestPatternSource {
            enc,
            cuda,
            frame: background.clone(),
            background,
            w,
            h,
            interval: Duration::from_secs_f64(1.0 / opts.fps.max(1) as f64),
            next_due: Instant::now(),
            t: 0,
            force_key: true,
            codec: opts.codec,
            pointer: Arc::new(std::sync::Mutex::new((0.5, 0.5))),
        })
    }

    fn fill_rect(&mut self, x0: usize, y0: usize, rw: usize, rh: usize, rgb: (u8, u8, u8)) {
        let (w, h) = (self.w, self.h);
        let (yv, u, v) = rgb_to_yuv709(rgb.0, rgb.1, rgb.2);
        let (x1, y1) = ((x0 + rw).min(w), (y0 + rh).min(h));
        for y in y0..y1 {
            self.frame[y * w + x0..y * w + x1].fill(yv);
        }
        let uv = &mut self.frame[w * h..];
        for y in y0 / 2..y1 / 2 {
            for x in x0 / 2..x1 / 2 {
                uv[y * w + x * 2] = u;
                uv[y * w + x * 2 + 1] = v;
            }
        }
    }

    fn render(&mut self) {
        let (w, h) = (self.w, self.h);
        self.frame.copy_from_slice(&self.background);
        // Scroll the "text" band horizontally.
        let band = h * 3 / 4 + 2;
        let off = (self.t as usize * 2) % w;
        for y in band..h {
            let row = &self.background[y * w..(y + 1) * w];
            let dst = &mut self.frame[y * w..(y + 1) * w];
            dst[..w - off].copy_from_slice(&row[off..]);
            dst[w - off..].copy_from_slice(&row[..off]);
        }
        // Moving bar.
        let bx = (self.t as usize * 8) % w.saturating_sub(32).max(1);
        self.fill_rect(bx, 0, 32, h * 3 / 4, (255, 60, 30));
        // Frame counter in binary (16 blocks) so freshness is visible.
        for bit in 0..16 {
            let on = self.t >> (15 - bit) & 1 != 0;
            self.fill_rect(16 + bit * 24, 16, 20, 20, if on { (255, 255, 255) } else { (0, 0, 0) });
        }
        // Remote pointer marker (shows input round trip).
        let (px, py) = *self.pointer.lock().unwrap();
        let (cx, cy) = ((px * w as f32) as usize, (py * h as f32) as usize);
        self.fill_rect(cx.saturating_sub(6), cy.saturating_sub(6), 12, 12, (0, 255, 120));
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
        self.render();
        let capture_us = clock::now_us();
        let (idx, dptr, pitch) = self.enc.next_input();
        {
            let _g = self.cuda.push();
            self.cuda.copy_2d(
                Mem::Host(self.frame.as_ptr()),
                self.w,
                Mem::Device(dptr),
                pitch,
                self.w,
                self.h * 3 / 2,
                std::ptr::null_mut(),
            )?;
        }
        let mut data = Vec::with_capacity(256 * 1024);
        let key = std::mem::take(&mut self.force_key);
        let keyframe = self.enc.encode(idx, self.t as u64, key, &mut data)?;
        self.t = self.t.wrapping_add(1);
        Ok(Some(EncodedFrame { data, keyframe, capture_us, width: self.w as u32, height: self.h as u32 }))
    }

    fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    fn set_bitrate(&mut self, kbps: u32) {
        if let Err(e) = self.enc.set_bitrate(kbps) {
            sard_core::warn!("set_bitrate: {e}");
        }
    }

    fn codec(&self) -> Codec {
        self.codec
    }

    fn size(&self) -> (u32, u32) {
        (self.w as u32, self.h as u32)
    }

    /// No cursor in the pattern: let the client keep its local pointer visible.
    fn cursor_mode(&self) -> sard_core::media::CursorMode {
        sard_core::media::CursorMode::Metadata
    }

    fn describe(&self) -> String {
        format!("test pattern {}x{} → NVENC {} on {}", self.w, self.h, self.codec.name(), self.cuda.device_name)
    }
}

/// Input sink for the test pattern: logs events and moves the marker.
pub struct LogInput {
    pub pointer: Arc<std::sync::Mutex<(f32, f32)>>,
}

impl InputSink for LogInput {
    fn inject(&mut self, ev: &InputEvent) -> media::Result<()> {
        match ev {
            InputEvent::MoveAbs { x, y } => *self.pointer.lock().unwrap() = (*x, *y),
            other => sard_core::info!("input: {other:?}"),
        }
        Ok(())
    }
}
