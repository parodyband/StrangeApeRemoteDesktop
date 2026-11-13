//! Platform-neutral interfaces between the `sard` app and the platform
//! backends (`sard-linux`, `sard-macos`).
//!
//! Host side: a [`VideoSource`] (capture + encode, fused because the GPU
//! surfaces never leave the platform layer) and an [`InputSink`].
//! Client side: a [`VideoSink`] (decode + present into a window).

use crate::proto::Codec;
pub use crate::proto::DisplayInfo;
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
    /// Displays this source can switch between (empty: not switchable).
    fn displays(&self) -> Vec<DisplayInfo> {
        Vec::new()
    }
    /// `DisplayInfo::id` currently being streamed.
    fn current_display(&self) -> Option<u32> {
        None
    }
    /// Switch to another display. The next frame must be a keyframe (its size
    /// may differ). The input sink returned with this source must follow the
    /// switch (absolute coordinates map to the new display).
    fn select_display(&mut self, _id: u32) -> Result<()> {
        Err(err("this source cannot switch displays"))
    }
    /// "Fit to the viewer's screen": switch the streamed display to the mode
    /// closest to `size` (desktop size in logical pixels, see [`pick_mode`]),
    /// or back to the mode it had (`None`). Returns the new mode for the
    /// viewer to show. The source must keep streaming afterwards (restart
    /// capture at the new size if needed) and must restore the original mode
    /// when it is dropped, or when another display is selected.
    fn fit_display(&mut self, _size: Option<(u32, u32)>) -> Result<String> {
        Err(err("changing the screen resolution isn't supported on this computer"))
    }
}

/// A display mode, as offered by the OS.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayMode {
    /// Desktop size in logical pixels (what windows and text are laid out in).
    pub width: u32,
    pub height: u32,
    /// Pixels actually scanned out (2x the logical size for HiDPI modes).
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// Hz (0 = unknown).
    pub refresh: f64,
}

impl DisplayMode {
    pub fn hidpi(&self) -> bool {
        self.pixel_width >= self.width * 3 / 2
    }
    pub fn describe(&self) -> String {
        if self.hidpi() {
            format!("{}×{} (HiDPI)", self.width, self.height)
        } else {
            format!("{}×{}", self.width, self.height)
        }
    }
}

/// Pick the mode that best fits a viewer wanting a desktop of about
/// `target` logical pixels: closest shape first, then closest size, keeping
/// the current mode's sharpness (HiDPI) and refresh rate where possible.
/// Never picks a desktop larger than the current one (fitting a phone means
/// bigger text, not more of it). Returns an index into `modes`, or None if
/// nothing smaller fits or `current` is already the best.
pub fn pick_mode(modes: &[DisplayMode], current: &DisplayMode, target: (u32, u32)) -> Option<usize> {
    let (tw, th) = (target.0.max(1) as f64, target.1.max(1) as f64);
    // A phone in landscape is wider than any monitor mode; portrait (taller
    // than wide) can't be matched by a landscape monitor: fit the width.
    let target_aspect = (tw / th).max(1.0);
    let score = |m: &DisplayMode| {
        let aspect = m.width as f64 / m.height as f64;
        let shape = (aspect / target_aspect).ln().abs();
        let size = (m.width as f64 / tw).ln().abs();
        let sharp = if m.hidpi() == current.hidpi() { 0.0 } else { 0.4 };
        let rate = if current.refresh > 0.0 && (m.refresh - current.refresh).abs() > 1.0 { 0.05 } else { 0.0 };
        shape * 2.0 + size + sharp + rate
    };
    let best = modes
        .iter()
        .enumerate()
        .filter(|(_, m)| m.width >= 640 && m.height >= 400)
        .filter(|(_, m)| m.width <= current.width && m.height <= current.height)
        .min_by(|a, b| score(a.1).total_cmp(&score(b.1)))?;
    let same =
        best.1.width == current.width && best.1.height == current.height && best.1.pixel_width == current.pixel_width;
    (!same && score(best.1) < score(current)).then_some(best.0)
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

// ---------------------------------------------------------------------------
// Tray / menu-bar UI (platform-neutral description; see `platform::create_tray`).
// ---------------------------------------------------------------------------

/// One entry of the tray / menu-bar menu. `id`s are chosen by the app and
/// reported back through the tray callback when an item is clicked.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuEntry {
    Item { id: u32, label: String, enabled: bool, checked: Option<bool> },
    Submenu { label: String, enabled: bool, items: Vec<MenuEntry> },
    Separator,
}

/// Straight-alpha RGBA8 image.
#[derive(Clone, Debug, PartialEq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Tray icon artwork. `mono` is black-with-alpha (macOS template image, KDE
/// symbolic); `color` is the full-color variant. `*_active` are shown while a
/// session is connected / the screen is being shared.
#[derive(Clone, Debug)]
pub struct TrayIcons {
    pub mono: Rgba,
    pub mono_active: Rgba,
    pub color: Rgba,
    pub color_active: Rgba,
}

/// Called (from any thread) with the `id` of the clicked menu item.
pub type TrayCallback = Box<dyn Fn(u32) + Send + Sync>;

/// A live tray / menu-bar item. All methods may be called from any thread.
pub trait Tray: Send + Sync {
    /// Replace the whole menu.
    fn set_menu(&self, menu: Vec<MenuEntry>);
    fn set_tooltip(&self, text: &str);
    /// Show the "active" icon (connected or being viewed).
    fn set_active(&self, active: bool);
}

/// System clipboard: text, and images as PNG.
pub trait Clipboard: Send {
    /// The clipboard's text, if it has any.
    fn get(&mut self) -> Option<String>;
    fn set(&mut self, text: &str);
    /// The clipboard's image as PNG, when it holds an image and no text
    /// (text wins: spreadsheets and word processors put a picture of the
    /// selection next to its text). Polled often, so implementations cache
    /// by the system's change counter instead of re-reading.
    fn get_image(&mut self) -> Option<std::sync::Arc<[u8]>> {
        None
    }
    /// Put a PNG image on the clipboard; false if this platform can't.
    fn set_image(&mut self, _png: &[u8]) -> bool {
        false
    }
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
    use super::{AUDIO_PACKET_FRAMES, DisplayMode, JitterBuffer, letterbox, pick_mode};

    fn m(width: u32, height: u32, scale: u32, refresh: f64) -> DisplayMode {
        DisplayMode { width, height, pixel_width: width * scale, pixel_height: height * scale, refresh }
    }

    #[test]
    fn pick_mode_for_a_phone() {
        // A 1440p monitor: a landscape phone asking for ~1280 wide gets 1280×720.
        let modes = [
            m(2560, 1440, 1, 144.0),
            m(1920, 1080, 1, 144.0),
            m(1280, 720, 1, 144.0),
            m(1280, 720, 1, 60.0),
            m(1280, 1024, 1, 60.0),
            m(800, 600, 1, 60.0),
        ];
        assert_eq!(pick_mode(&modes, &modes[0], (1280, 590)), Some(2), "keeps 144 Hz");
        // Portrait phone: still a landscape mode, by width.
        assert_eq!(pick_mode(&modes, &modes[0], (1024, 2200)).map(|i| modes[i].width), Some(1280));
        // Already at the best mode: nothing to do.
        assert_eq!(pick_mode(&modes, &modes[2], (1280, 590)), None);
        // Never larger than the current desktop.
        assert_eq!(pick_mode(&modes[..3], &modes[2], (2560, 1200)), None);
        // A Retina Mac keeps HiDPI.
        let mac = [m(1728, 1117, 2, 120.0), m(1280, 800, 2, 120.0), m(1280, 800, 1, 120.0), m(1024, 640, 2, 120.0)];
        assert_eq!(pick_mode(&mac, &mac[0], (1280, 590)), Some(1));
    }

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
