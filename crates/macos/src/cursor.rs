//! The host's mouse cursor, for viewer-drawn cursors: the viewer draws it
//! locally (instant response) instead of seeing it baked into the video one
//! round trip late.
//!
//! Position: `CGEventGetLocation` of a fresh event (global points, top-left
//! origin), polled cheaply. Shape: `NSCursor.currentSystemCursor` (whichever
//! app set it), rendered to RGBA at the stream's scale and only resent when
//! its pixels change.

use crate::ffi::*;
use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use sard_core::media::{CursorShape, CursorUpdate};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGCursorIsVisible() -> u32;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: *mut c_void,
        info: u32,
    ) -> *mut c_void;
    fn CGContextDrawImage(ctx: *mut c_void, rect: CGRect, image: *mut c_void);
    fn CGContextRelease(ctx: *mut c_void);
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

/// RGBA, premultiplied, byte order R G B A.
const BITMAP_INFO: u32 = 1 /* kCGImageAlphaPremultipliedLast */ | (4 << 12) /* kCGBitmapByteOrder32Big */;
/// Re-check the cursor image this often (position is checked on every poll).
const SHAPE_EVERY: Duration = Duration::from_millis(40);

/// Global cursor position in points.
pub fn position() -> CGPoint {
    // SAFETY: a null-source event just reports the current location; released.
    unsafe {
        let ev = CGEventCreate(null_mut());
        if ev.is_null() {
            return CGPoint::default();
        }
        let p = CGEventGetLocation(ev);
        crate::cf::CFRelease(ev as _);
        p
    }
}

/// `CGCursorIsVisible` (deprecated): answers false outside the GUI session
/// and isn't trusted for hiding the viewer's cursor; diagnostic only.
pub fn visible() -> bool {
    // SAFETY: plain query.
    unsafe { CGCursorIsVisible() != 0 }
}

/// The current system cursor drawn at `scale` pixels per point:
/// (rgba straight alpha, width, height, hot_x, hot_y).
pub fn shape(scale: f64) -> Option<(Vec<u8>, u32, u32, u32, u32)> {
    autoreleasepool(|_| {
        // SAFETY: AppKit getters on objects kept alive by the pool; Core
        // Graphics context created and released here.
        unsafe {
            let cur: *mut AnyObject = msg_send![class!(NSCursor), currentSystemCursor];
            if cur.is_null() {
                return None;
            }
            let image: *mut AnyObject = msg_send![cur, image];
            if image.is_null() {
                return None;
            }
            let hot: CGPoint = msg_send![cur, hotSpot];
            let size: CGSize = msg_send![image, size];
            let w = ((size.width * scale).round() as usize).clamp(1, 256);
            let h = ((size.height * scale).round() as usize).clamp(1, 256);
            let cg: *mut c_void = msg_send![image, CGImageForProposedRect: null_mut::<c_void>(), context: null::<AnyObject>(), hints: null::<AnyObject>()];
            if cg.is_null() {
                return None;
            }
            let mut px = vec![0u8; w * h * 4];
            let cs = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
            let ctx = CGBitmapContextCreate(px.as_mut_ptr().cast(), w, h, 8, w * 4, cs, BITMAP_INFO);
            CGColorSpaceRelease(cs);
            if ctx.is_null() {
                return None;
            }
            let rect =
                CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: w as f64, height: h as f64 } };
            CGContextDrawImage(ctx, rect, cg);
            CGContextRelease(ctx);
            // Viewers expect straight alpha.
            for p in px.as_chunks_mut::<4>().0 {
                let a = p[3] as u32;
                if a > 0 && a < 255 {
                    for c in &mut p[..3] {
                        *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                    }
                }
            }
            let hx = ((hot.x * scale).round().max(0.0) as u32).min(w as u32 - 1);
            let hy = ((hot.y * scale).round().max(0.0) as u32).min(h as u32 - 1);
            Some((px, w as u32, h as u32, hx, hy))
        }
    })
}

fn hash(data: &[u8]) -> u64 {
    // FNV-1a: only to notice changes.
    data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100_0000_01b3))
}

/// Tracks what the viewer last got, so updates are sent only on change.
pub struct CursorTracker {
    last_pos: Option<(f32, f32, bool)>,
    last_shape_hash: u64,
    serial: u32,
    next_shape_check: Instant,
}

impl Default for CursorTracker {
    fn default() -> Self {
        CursorTracker { last_pos: None, last_shape_hash: 0, serial: 0, next_shape_check: Instant::now() }
    }
}

impl CursorTracker {
    /// Has the cursor moved since the last update? (cheap; for waking the
    /// capture loop on a static screen)
    pub fn moved(&self, display: CGRect) -> bool {
        let (x, y, v) = normalized(position(), display);
        self.last_pos.is_none_or(|(lx, ly, lv)| (lx, ly, lv) != (x, y, v))
    }

    /// Changes since the last call, in coordinates relative to `display`
    /// (global points); `scale` is stream pixels per point.
    pub fn poll(&mut self, display: CGRect, scale: f64) -> Option<CursorUpdate> {
        // Visible = over the captured display. (Detecting a cursor hidden by
        // a game, for automatic relative mouse, isn't reliable on macOS yet.)
        let (x, y, visible) = normalized(position(), display);
        let mut shape = None;
        if Instant::now() >= self.next_shape_check {
            self.next_shape_check = Instant::now() + SHAPE_EVERY;
            if let Some((rgba, w, h, hx, hy)) = self::shape(scale) {
                let hsh = hash(&rgba) ^ ((w as u64) << 48 | (h as u64) << 32 | (hx as u64) << 16 | hy as u64);
                if hsh != self.last_shape_hash {
                    self.last_shape_hash = hsh;
                    self.serial = self.serial.wrapping_add(1);
                    shape = Some(CursorShape {
                        serial: self.serial,
                        hot_x: hx as u16,
                        hot_y: hy as u16,
                        width: w as u16,
                        height: h as u16,
                        rgba,
                    });
                }
            }
        }
        let pos = (x, y, visible);
        if shape.is_none() && self.last_pos == Some(pos) {
            return None;
        }
        self.last_pos = Some(pos);
        Some(CursorUpdate { x, y, visible, shape })
    }

    /// Resend everything (e.g. after switching displays).
    pub fn reset(&mut self) {
        self.last_pos = None;
        self.last_shape_hash = 0;
        self.next_shape_check = Instant::now();
    }
}

/// Position relative to `display` (0..1) and whether it is on that display.
fn normalized(p: CGPoint, d: CGRect) -> (f32, f32, bool) {
    let (w, h) = (d.size.width.max(1.0), d.size.height.max(1.0));
    let (x, y) = ((p.x - d.origin.x) / w, (p.y - d.origin.y) / h);
    let on = (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y);
    (x.clamp(0.0, 1.0) as f32, y.clamp(0.0, 1.0) as f32, on)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_against_display_bounds() {
        let d = CGRect { origin: CGPoint { x: 1728.0, y: 0.0 }, size: CGSize { width: 2560.0, height: 1440.0 } };
        assert_eq!(normalized(CGPoint { x: 1728.0 + 1280.0, y: 720.0 }, d), (0.5, 0.5, true));
        let (_, _, on) = normalized(CGPoint { x: 100.0, y: 100.0 }, d);
        assert!(!on, "on another display");
    }

    /// Needs a logged-in GUI session (run from a terminal on the Mac).
    #[test]
    #[ignore]
    fn reads_the_live_cursor() {
        let p = position();
        let s = shape(2.0).expect("a cursor image");
        println!("cursor at {p:?}, {}x{} hot ({}, {}), visible {}", s.1, s.2, s.3, s.4, visible());
        assert!(s.1 >= 8 && s.2 >= 8);
    }
}
