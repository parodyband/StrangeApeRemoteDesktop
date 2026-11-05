//! Platform-neutral interfaces between the `sard` app and the platform
//! backends (`sard-linux`, `sard-macos`).
//!
//! Host side: a [`VideoSource`] (capture + encode, fused because the GPU
//! surfaces never leave the platform layer) and an [`InputSink`].
//! Client side: a [`VideoSink`] (decode + present into a window).

use crate::proto::Codec;
use std::time::Duration;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// Convenience: turn any displayable error into [`Error`].
pub fn err(msg: impl std::fmt::Display) -> Error {
    msg.to_string().into()
}

/// One encoded access unit (Annex-B for H.264/HEVC, OBU stream for AV1).
#[derive(Debug, Default)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Host monotonic µs (see `clock::now_us`) when the frame was captured.
    pub capture_us: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorMode {
    /// Cursor is drawn into the video.
    Embedded,
    /// Cursor is sent separately (shape + position) and drawn by the client.
    Metadata,
}

/// What the host should capture and how to encode it.
#[derive(Clone, Debug)]
pub struct SourceOptions {
    pub codec: Codec,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Display index (0 = primary / let the user pick).
    pub display: u32,
    /// Client limits; the source downscales if the display is larger.
    pub max_width: u32,
    pub max_height: u32,
    pub yuv444: bool,
    pub cursor: CursorMode,
    /// Synthetic animated test pattern instead of screen capture.
    pub test_pattern: bool,
    /// Encoder speed/quality preset, 1 (fastest) ..= 7.
    pub preset: u8,
    /// Size for the test pattern (ignored for real capture).
    pub test_size: (u32, u32),
}

impl Default for SourceOptions {
    fn default() -> Self {
        Self {
            codec: Codec::Hevc,
            fps: 60,
            bitrate_kbps: 30_000,
            display: 0,
            max_width: 7680,
            max_height: 4320,
            yuv444: false,
            cursor: CursorMode::Metadata,
            test_pattern: false,
            preset: 1,
            test_size: (1920, 1080),
        }
    }
}

/// Cursor image, RGBA8 straight alpha.
#[derive(Clone, Debug, PartialEq)]
pub struct CursorShape {
    pub serial: u32,
    pub hot_x: u16,
    pub hot_y: u16,
    pub width: u16,
    pub height: u16,
    pub rgba: Vec<u8>,
}

/// Cursor state change (position normalized 0..1 to the video frame).
#[derive(Clone, Debug, PartialEq)]
pub struct CursorUpdate {
    pub x: f32,
    pub y: f32,
    pub visible: bool,
    pub shape: Option<CursorShape>,
}

/// Host-side capture + encode.
pub trait VideoSource: Send {
    /// Block until the next encoded frame, or `Ok(None)` after `timeout`
    /// (e.g. static screen with damage-based capture).
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<EncodedFrame>>;
    /// Make the next frame a keyframe; if no new content arrives, re-encode
    /// the last captured image so the client can recover anyway.
    fn request_keyframe(&mut self);
    fn set_bitrate(&mut self, kbps: u32);
    fn codec(&self) -> Codec;
    /// Current encoded size.
    fn size(&self) -> (u32, u32);
    /// How the cursor is actually delivered (may differ from what was asked,
    /// e.g. a capture API that can only embed the cursor).
    fn cursor_mode(&self) -> CursorMode {
        CursorMode::Embedded
    }
    /// Pending cursor change, if the source tracks the cursor separately.
    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        None
    }
    /// Human-readable description for logs ("NVENC hevc 2560x1440 via portal").
    fn describe(&self) -> String {
        String::new()
    }
}

/// Input events delivered to the host. Positions are normalized to the
/// streamed frame (0..1); the sink maps them to its screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputEvent {
    /// `code` is a Linux evdev key code (see `input::key`).
    Key {
        code: u16,
        down: bool,
    },
    /// `button` uses `proto::button` values.
    Button {
        button: u8,
        down: bool,
    },
    MoveAbs {
        x: f32,
        y: f32,
    },
    MoveRel {
        dx: f32,
        dy: f32,
    },
    /// Discrete: wheel notches (fractional allowed). Otherwise logical pixels.
    Scroll {
        dx: f32,
        dy: f32,
        discrete: bool,
    },
}

/// Host-side input injection.
pub trait InputSink: Send {
    fn inject(&mut self, ev: &InputEvent) -> Result<()>;
}

/// Timing of one decoded+presented frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct PresentStats {
    pub decode_us: u32,
    pub present_us: u32,
}

/// Client-side decode + present into a window. Created on the UI thread from
/// raw window handles, then used from a dedicated render thread.
pub trait VideoSink: Send {
    fn present(&mut self, codec: Codec, data: &[u8], keyframe: bool) -> Result<PresentStats>;
    /// Window drawable size changed (physical pixels).
    fn resize(&mut self, width: u32, height: u32);
    /// Re-present the last frame (after a resize/expose).
    fn redraw(&mut self) -> Result<()> {
        Ok(())
    }
    /// Save the next decoded frame as a PNG (diagnostics; optional).
    fn snapshot_next(&mut self, _path: &std::path::Path) -> Result<()> {
        Err(err("snapshots are not supported by this video output"))
    }
    fn describe(&self) -> String {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Audio: uncompressed 48 kHz stereo s16 (1.5 Mbit/s, negligible on a LAN and
// zero codec latency). Packets carry 5 ms.
// ---------------------------------------------------------------------------

pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: usize = 2;
pub const AUDIO_PACKET_FRAMES: usize = 240;

/// Receives captured interleaved stereo samples (any number of frames).
pub type AudioCallback = Box<dyn FnMut(&[i16]) + Send>;

/// A running host audio capture; dropping it stops capture.
pub trait AudioSource: Send {
    fn describe(&self) -> String {
        String::new()
    }
}

/// Client audio output fed from the network.
pub trait AudioSink: Send {
    fn push(&self, seq: u32, samples: &[i16]);
    fn describe(&self) -> String {
        String::new()
    }
}

/// System clipboard (plain text).
pub trait Clipboard: Send {
    fn get(&mut self) -> Option<String>;
    fn set(&mut self, text: &str);
}

/// Small adaptive jitter buffer for interleaved stereo s16.
pub struct JitterBuffer {
    buf: std::collections::VecDeque<i16>,
    expected: Option<u32>,
    /// Frames to accumulate before (re)starting playback.
    pub target_frames: usize,
    /// Above this many buffered frames, drop the oldest down to target.
    pub max_frames: usize,
    primed: bool,
    pub underruns: u64,
    pub dropped_frames: u64,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(48_000),
            expected: None,
            target_frames: AUDIO_RATE as usize * 15 / 1000,
            max_frames: AUDIO_RATE as usize * 60 / 1000,
            primed: false,
            underruns: 0,
            dropped_frames: 0,
        }
    }
}

impl JitterBuffer {
    pub fn frames(&self) -> usize {
        self.buf.len() / AUDIO_CHANNELS
    }

    pub fn push(&mut self, seq: u32, samples: &[i16]) {
        if let Some(e) = self.expected {
            let d = seq.wrapping_sub(e) as i32;
            if d < 0 {
                return; // late duplicate / reordered: already played past it
            }
            // Fill short gaps (lost packets) with silence to keep timing.
            for _ in 0..(d as usize).min(4) {
                self.buf.extend(std::iter::repeat_n(0, AUDIO_PACKET_FRAMES * AUDIO_CHANNELS));
            }
        }
        self.expected = Some(seq.wrapping_add(1));
        self.buf.extend(samples.iter().copied());
        if self.frames() > self.max_frames {
            let drop = (self.frames() - self.target_frames) * AUDIO_CHANNELS;
            self.buf.drain(..drop);
            self.dropped_frames += (drop / AUDIO_CHANNELS) as u64;
        }
    }

    /// Fill `out` (interleaved) for playback; silence when not enough data.
    pub fn pull(&mut self, out: &mut [i16]) {
        if !self.primed && self.frames() < self.target_frames {
            out.fill(0);
            return;
        }
        self.primed = true;
        let n = out.len().min(self.buf.len());
        for (o, s) in out[..n].iter_mut().zip(self.buf.drain(..n)) {
            *o = s;
        }
        if n < out.len() {
            out[n..].fill(0);
            self.primed = false;
            self.underruns += 1;
        }
    }
}

/// Where the video rectangle sits inside a window, preserving aspect ratio.
/// Returns (x, y, w, h) in window pixels.
pub fn letterbox(win_w: u32, win_h: u32, vid_w: u32, vid_h: u32) -> (f32, f32, f32, f32) {
    if vid_w == 0 || vid_h == 0 || win_w == 0 || win_h == 0 {
        return (0.0, 0.0, win_w as f32, win_h as f32);
    }
    let (ww, wh, vw, vh) = (win_w as f32, win_h as f32, vid_w as f32, vid_h as f32);
    let scale = (ww / vw).min(wh / vh);
    let (w, h) = (vw * scale, vh * scale);
    ((ww - w) / 2.0, (wh - h) / 2.0, w, h)
}

#[cfg(test)]
mod tests {
    use super::{AUDIO_PACKET_FRAMES, JitterBuffer, letterbox};

    #[test]
    fn jitter_buffer_primes_fills_gaps_and_caps() {
        let mut j = JitterBuffer::default();
        let pkt = vec![1i16; AUDIO_PACKET_FRAMES * 2];
        let mut out = vec![9i16; 480];
        j.push(0, &pkt);
        j.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0), "not primed yet");
        j.push(1, &pkt);
        j.push(3, &pkt); // seq 2 lost -> silence inserted
        assert_eq!(j.frames(), AUDIO_PACKET_FRAMES * 4);
        j.pull(&mut out);
        assert!(out.iter().all(|&s| s == 1));
        j.push(2, &pkt); // late: ignored
        assert_eq!(j.frames(), AUDIO_PACKET_FRAMES * 4 - 240);
        for s in 4..40 {
            j.push(s, &pkt);
        }
        assert!(j.frames() <= j.max_frames);
    }

    #[test]
    fn letterbox_fits() {
        assert_eq!(letterbox(1920, 1080, 1920, 1080), (0.0, 0.0, 1920.0, 1080.0));
        let (x, y, w, h) = letterbox(2000, 1000, 1920, 1080);
        assert!((h - 1000.0).abs() < 0.01 && y == 0.0 && x > 0.0 && (w / h - 16.0 / 9.0).abs() < 0.01);
    }
}
