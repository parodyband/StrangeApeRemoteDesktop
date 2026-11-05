//! Host input injection with CGEvent, plus the evdev → macOS virtual key
//! code table.

use crate::ffi::*;
use objc2::{class, msg_send};
use sard_core::input::key as K;
use sard_core::media::{self, InputEvent, InputSink};
use sard_core::proto::button;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Linux evdev key code → macOS virtual key code (`kVK_*`, HIToolbox Events.h).
pub fn evdev_to_mac(code: u16) -> Option<u16> {
    Some(match code {
        K::ESC => 0x35,
        K::N1 => 0x12,
        K::N2 => 0x13,
        K::N3 => 0x14,
        K::N4 => 0x15,
        K::N5 => 0x17,
        K::N6 => 0x16,
        K::N7 => 0x1A,
        K::N8 => 0x1C,
        K::N9 => 0x19,
        K::N0 => 0x1D,
        K::MINUS => 0x1B,
        K::EQUAL => 0x18,
        K::BACKSPACE => 0x33,
        K::TAB => 0x30,
        K::Q => 0x0C,
        K::W => 0x0D,
        K::E => 0x0E,
        K::R => 0x0F,
        K::T => 0x11,
        K::Y => 0x10,
        K::U => 0x20,
        K::I => 0x22,
        K::O => 0x1F,
        K::P => 0x23,
        K::LEFTBRACE => 0x21,
        K::RIGHTBRACE => 0x1E,
        K::ENTER => 0x24,
        K::LEFTCTRL => 0x3B,
        K::A => 0x00,
        K::S => 0x01,
        K::D => 0x02,
        K::F => 0x03,
        K::G => 0x05,
        K::H => 0x04,
        K::J => 0x26,
        K::K => 0x28,
        K::L => 0x25,
        K::SEMICOLON => 0x29,
        K::APOSTROPHE => 0x27,
        K::GRAVE => 0x32,
        K::LEFTSHIFT => 0x38,
        K::BACKSLASH => 0x2A,
        K::Z => 0x06,
        K::X => 0x07,
        K::C => 0x08,
        K::V => 0x09,
        K::B => 0x0B,
        K::N => 0x2D,
        K::M => 0x2E,
        K::COMMA => 0x2B,
        K::DOT => 0x2F,
        K::SLASH => 0x2C,
        K::RIGHTSHIFT => 0x3C,
        K::KPASTERISK => 0x43,
        K::LEFTALT => 0x3A,
        K::SPACE => 0x31,
        K::CAPSLOCK => 0x39,
        K::F1 => 0x7A,
        K::F2 => 0x78,
        K::F3 => 0x63,
        K::F4 => 0x76,
        K::F5 => 0x60,
        K::F6 => 0x61,
        K::F7 => 0x62,
        K::F8 => 0x64,
        K::F9 => 0x65,
        K::F10 => 0x6D,
        K::NUMLOCK => 0x47, // keypad Clear sits where Num Lock is
        K::KP7 => 0x59,
        K::KP8 => 0x5B,
        K::KP9 => 0x5C,
        K::KPMINUS => 0x4E,
        K::KP4 => 0x56,
        K::KP5 => 0x57,
        K::KP6 => 0x58,
        K::KPPLUS => 0x45,
        K::KP1 => 0x53,
        K::KP2 => 0x54,
        K::KP3 => 0x55,
        K::KP0 => 0x52,
        K::KPDOT => 0x41,
        K::KEY_102ND => 0x0A, // ISO section key
        K::F11 => 0x67,
        K::F12 => 0x6F,
        K::RO => 0x5E,       // JIS underscore
        K::MUHENKAN => 0x66, // JIS Eisu
        K::HENKAN => 0x68,   // JIS Kana
        K::KATAKANAHIRAGANA => 0x68,
        K::KPJPCOMMA => 0x5F,
        K::KPENTER => 0x4C,
        K::RIGHTCTRL => 0x3E,
        K::KPSLASH => 0x4B,
        K::SYSRQ => 0x69, // Print Screen → F13, as on Apple keyboards
        K::RIGHTALT => 0x3D,
        K::HOME => 0x73,
        K::UP => 0x7E,
        K::PAGEUP => 0x74,
        K::LEFT => 0x7B,
        K::RIGHT => 0x7C,
        K::END => 0x77,
        K::DOWN => 0x7D,
        K::PAGEDOWN => 0x79,
        K::INSERT => 0x72, // Help
        K::DELETE => 0x75, // forward delete
        K::MUTE => 0x4A,
        K::VOLUMEDOWN => 0x49,
        K::VOLUMEUP => 0x48,
        K::KPEQUAL => 0x51,
        K::PAUSE => 0x71, // F15
        K::KPCOMMA => 0x5F,
        K::YEN => 0x5D,
        K::LEFTMETA => 0x37,
        K::RIGHTMETA => 0x36,
        K::COMPOSE => 0x6E, // contextual menu
        K::F13 => 0x69,
        K::F14 => 0x6B,
        K::F15 => 0x71,
        K::F16 => 0x6A,
        K::F17 => 0x40,
        K::F18 => 0x4F,
        K::F19 => 0x50,
        K::F20 => 0x5A,
        _ => return None,
    })
}

/// Modifier bit (device-independent | device-dependent) for a modifier key.
fn modifier_mask(code: u16) -> Option<u64> {
    Some(match code {
        K::LEFTSHIFT => kCGEventFlagMaskShift | NX_DEVICELSHIFTKEYMASK,
        K::RIGHTSHIFT => kCGEventFlagMaskShift | NX_DEVICERSHIFTKEYMASK,
        K::LEFTCTRL => kCGEventFlagMaskControl | NX_DEVICELCTLKEYMASK,
        K::RIGHTCTRL => kCGEventFlagMaskControl | NX_DEVICERCTLKEYMASK,
        K::LEFTALT => kCGEventFlagMaskAlternate | NX_DEVICELALTKEYMASK,
        K::RIGHTALT => kCGEventFlagMaskAlternate | NX_DEVICERALTKEYMASK,
        K::LEFTMETA => kCGEventFlagMaskCommand | NX_DEVICELCMDKEYMASK,
        K::RIGHTMETA => kCGEventFlagMaskCommand | NX_DEVICERCMDKEYMASK,
        _ => return None,
    })
}

/// Extra flags real hardware sets on navigation / keypad keys.
fn key_flags(vk: u16) -> u64 {
    match vk {
        0x7B..=0x7E => kCGEventFlagMaskNumericPad | kCGEventFlagMaskSecondaryFn, // arrows
        0x72..=0x77 | 0x79 => kCGEventFlagMaskSecondaryFn,                       // help/home/pgup/del/end/pgdn
        0x41 | 0x43 | 0x45 | 0x47 | 0x4B | 0x4C | 0x4E | 0x51..=0x59 | 0x5B | 0x5C => kCGEventFlagMaskNumericPad,
        0x60..=0x6F | 0x7A | 0x78 | 0x71 | 0x40 | 0x4F | 0x50 | 0x5A => kCGEventFlagMaskSecondaryFn, // F-keys
        _ => 0,
    }
}

/// Where the streamed frame sits in global display coordinates (points).
pub trait Bounds: Send {
    fn rect(&self) -> CGRect;
}

/// The display being streamed, shared between a video source and its input
/// sink so absolute pointer positions follow display switches.
#[derive(Clone)]
pub struct DisplayBounds(pub Arc<AtomicU32>);

impl DisplayBounds {
    pub fn new(id: CGDirectDisplayID) -> Self {
        DisplayBounds(Arc::new(AtomicU32::new(id)))
    }
    pub fn id(&self) -> CGDirectDisplayID {
        self.0.load(Ordering::Relaxed)
    }
    pub fn set(&self, id: CGDirectDisplayID) {
        self.0.store(id, Ordering::Relaxed);
    }
}

impl Bounds for DisplayBounds {
    fn rect(&self) -> CGRect {
        // SAFETY: plain query.
        unsafe { CGDisplayBounds(self.id()) }
    }
}

#[derive(Clone, Copy)]
struct Click {
    button: u8,
    at: Instant,
    pos: CGPoint,
    count: i64,
}

/// CGEvent-based input sink for real screen sharing.
pub struct CgInput {
    source: CGEventSourceRef,
    bounds: Box<dyn Bounds>,
    pos: CGPoint,
    mods: u64,
    buttons: u8,
    last_click: Option<Click>,
    down_count: [i64; 8],
    double_click: f64,
    scroll_acc: (f32, f32),
    trusted: bool,
    warned: bool,
}

// SAFETY: the event source is only used from the input thread that owns us.
unsafe impl Send for CgInput {}

pub const ACCESSIBILITY_HINT: &str = "macOS Accessibility permission is missing, so remote input is ignored: \
     open System Settings → Privacy & Security → Accessibility, enable Sard (or the terminal app running sard), \
     then reconnect.";

pub fn accessibility_trusted() -> bool {
    // SAFETY: plain query.
    unsafe { AXIsProcessTrusted() != 0 }
}

impl CgInput {
    pub fn new(bounds: Box<dyn Bounds>) -> media::Result<Self> {
        // SAFETY: plain CG calls; the source is released in Drop.
        let (source, pos) = unsafe {
            let source = CGEventSourceCreate(kCGEventSourceStateHIDSystemState);
            if source.is_null() {
                return Err(media::err("CGEventSourceCreate failed"));
            }
            // Don't suppress the host's own mouse/keyboard after we inject.
            CGEventSourceSetLocalEventsSuppressionInterval(source, 0.0);
            let ev = CGEventCreate(std::ptr::null_mut());
            let pos = if ev.is_null() { CGPoint::default() } else { CGEventGetLocation(ev) };
            if !ev.is_null() {
                crate::cf::CFRelease(ev);
            }
            (source, pos)
        };
        let double_click = objc2::rc::autoreleasepool(|_| {
            // SAFETY: class method returning NSTimeInterval.
            let v: f64 = unsafe { msg_send![class!(NSEvent), doubleClickInterval] };
            if v > 0.0 && v < 5.0 { v } else { 0.5 }
        });
        let trusted = accessibility_trusted();
        if !trusted {
            sard_core::warn!("{ACCESSIBILITY_HINT}");
            // Adds sard to the Accessibility list / shows the system prompt once.
            // SAFETY: plain call.
            unsafe { CGRequestPostEventAccess() };
        }
        Ok(CgInput {
            source,
            bounds,
            pos,
            mods: 0,
            buttons: 0,
            last_click: None,
            down_count: [1; 8],
            double_click,
            scroll_acc: (0.0, 0.0),
            trusted,
            warned: !trusted,
        })
    }

    fn post(&mut self, ev: CGEventRef) {
        if ev.is_null() {
            return;
        }
        // SAFETY: valid event we own.
        unsafe {
            CGEventPost(kCGHIDEventTap, ev);
            crate::cf::CFRelease(ev as *const c_void);
        }
    }

    fn flags(&self) -> u64 {
        self.mods | kCGEventFlagMaskNonCoalesced
    }

    fn clamp(&self, p: CGPoint) -> CGPoint {
        let b = self.bounds.rect();
        CGPoint {
            x: p.x.clamp(b.origin.x, b.origin.x + (b.size.width - 1.0).max(0.0)),
            y: p.y.clamp(b.origin.y, b.origin.y + (b.size.height - 1.0).max(0.0)),
        }
    }

    fn mouse_move(&mut self, to: CGPoint, delta: (f64, f64)) {
        let to = self.clamp(to);
        let (kind, btn) = if self.buttons & (1 << button::LEFT) != 0 {
            (kCGEventLeftMouseDragged, 0)
        } else if self.buttons & (1 << button::RIGHT) != 0 {
            (kCGEventRightMouseDragged, 1)
        } else if let Some(b) =
            [button::MIDDLE, button::BACK, button::FORWARD].iter().find(|&&b| self.buttons & (1 << b) != 0)
        {
            (kCGEventOtherMouseDragged, cg_button(*b).2)
        } else {
            (kCGEventMouseMoved, 0)
        };
        // SAFETY: valid source; event released by `post`.
        let ev = unsafe {
            let ev = CGEventCreateMouseEvent(self.source, kind, to, btn);
            if !ev.is_null() {
                CGEventSetIntegerValueField(ev, kCGMouseEventDeltaX, delta.0.round() as i64);
                CGEventSetIntegerValueField(ev, kCGMouseEventDeltaY, delta.1.round() as i64);
                CGEventSetFlags(ev, self.flags());
            }
            ev
        };
        self.pos = to;
        self.post(ev);
    }

    fn mouse_button(&mut self, b: u8, down: bool) {
        let (down_kind, up_kind, num) = cg_button(b);
        if num == u32::MAX {
            return;
        }
        let idx = b as usize & 7;
        let count = if down {
            let now = Instant::now();
            let count = match self.last_click {
                Some(c)
                    if c.button == b
                        && now.duration_since(c.at).as_secs_f64() <= self.double_click
                        && (c.pos.x - self.pos.x).abs() <= 4.0
                        && (c.pos.y - self.pos.y).abs() <= 4.0 =>
                {
                    c.count + 1
                }
                _ => 1,
            };
            self.last_click = Some(Click { button: b, at: now, pos: self.pos, count });
            self.down_count[idx] = count;
            self.buttons |= 1 << b;
            count
        } else {
            self.buttons &= !(1 << b);
            self.down_count[idx]
        };
        // SAFETY: valid source; event released by `post`.
        let ev = unsafe {
            let ev = CGEventCreateMouseEvent(self.source, if down { down_kind } else { up_kind }, self.pos, num);
            if !ev.is_null() {
                CGEventSetIntegerValueField(ev, kCGMouseEventClickState, count);
                CGEventSetIntegerValueField(ev, kCGMouseEventButtonNumber, num as i64);
                CGEventSetFlags(ev, self.flags());
            }
            ev
        };
        self.post(ev);
    }

    fn key(&mut self, code: u16, down: bool) {
        if let Some(m) = modifier_mask(code) {
            // Device-independent bit stays set while either side is held.
            let dev = m & 0xffff;
            if down {
                self.mods |= m;
            } else {
                self.mods &= !dev;
                let side_bits = [
                    (kCGEventFlagMaskShift, NX_DEVICELSHIFTKEYMASK | NX_DEVICERSHIFTKEYMASK),
                    (kCGEventFlagMaskControl, NX_DEVICELCTLKEYMASK | NX_DEVICERCTLKEYMASK),
                    (kCGEventFlagMaskAlternate, NX_DEVICELALTKEYMASK | NX_DEVICERALTKEYMASK),
                    (kCGEventFlagMaskCommand, NX_DEVICELCMDKEYMASK | NX_DEVICERCMDKEYMASK),
                ];
                for (indep, both) in side_bits {
                    if m & indep != 0 && self.mods & both == 0 {
                        self.mods &= !indep;
                    }
                }
            }
        }
        let Some(vk) = evdev_to_mac(code) else {
            sard_core::debug!("no macOS key for evdev code {code}");
            return;
        };
        // SAFETY: valid source; event released by `post`. Modifier key codes
        // produce kCGEventFlagsChanged events automatically.
        let ev = unsafe {
            let ev = CGEventCreateKeyboardEvent(self.source, vk, down);
            if !ev.is_null() {
                CGEventSetFlags(ev, self.flags() | key_flags(vk));
            }
            ev
        };
        self.post(ev);
    }

    fn scroll(&mut self, dx: f32, dy: f32, discrete: bool) {
        // Accumulate fractional notches / pixels so slow scrolling isn't lost.
        self.scroll_acc.0 += dx;
        self.scroll_acc.1 += dy;
        let (sx, sy) = (self.scroll_acc.0.trunc(), self.scroll_acc.1.trunc());
        if sx == 0.0 && sy == 0.0 {
            return;
        }
        self.scroll_acc.0 -= sx;
        self.scroll_acc.1 -= sy;
        let units = if discrete { kCGScrollEventUnitLine } else { kCGScrollEventUnitPixel };
        // SAFETY: valid source; event released by `post`.
        let ev = unsafe {
            let ev = CGEventCreateScrollWheelEvent2(self.source, units, 2, sy as i32, sx as i32, 0);
            if !ev.is_null() {
                if !discrete {
                    CGEventSetIntegerValueField(ev, kCGScrollWheelEventIsContinuous, 1);
                }
                CGEventSetFlags(ev, self.flags());
            }
            ev
        };
        self.post(ev);
    }
}

/// (down type, up type, CG button number) for a wire button.
fn cg_button(b: u8) -> (u32, u32, u32) {
    match b {
        button::LEFT => (kCGEventLeftMouseDown, kCGEventLeftMouseUp, 0),
        button::RIGHT => (kCGEventRightMouseDown, kCGEventRightMouseUp, 1),
        button::MIDDLE => (kCGEventOtherMouseDown, kCGEventOtherMouseUp, 2),
        button::BACK => (kCGEventOtherMouseDown, kCGEventOtherMouseUp, 3),
        button::FORWARD => (kCGEventOtherMouseDown, kCGEventOtherMouseUp, 4),
        _ => (0, 0, u32::MAX),
    }
}

impl InputSink for CgInput {
    fn inject(&mut self, ev: &InputEvent) -> media::Result<()> {
        if !self.trusted && accessibility_trusted() {
            self.trusted = true;
            sard_core::info!("Accessibility permission granted; injecting input");
        }
        if !self.trusted && !self.warned {
            sard_core::warn!("{ACCESSIBILITY_HINT}");
            self.warned = true;
        }
        match *ev {
            InputEvent::Key { code, down } => self.key(code, down),
            InputEvent::Button { button, down } => self.mouse_button(button, down),
            InputEvent::MoveAbs { x, y } => {
                let b = self.bounds.rect();
                let to = CGPoint {
                    x: b.origin.x + x.clamp(0.0, 1.0) as f64 * b.size.width,
                    y: b.origin.y + y.clamp(0.0, 1.0) as f64 * b.size.height,
                };
                let delta = (to.x - self.pos.x, to.y - self.pos.y);
                self.mouse_move(to, delta);
            }
            InputEvent::MoveRel { dx, dy } => {
                let to = CGPoint { x: self.pos.x + dx as f64, y: self.pos.y + dy as f64 };
                self.mouse_move(to, (dx as f64, dy as f64));
            }
            InputEvent::Scroll { dx, dy, discrete } => self.scroll(dx, dy, discrete),
        }
        Ok(())
    }
}

impl Drop for CgInput {
    fn drop(&mut self) {
        // SAFETY: we own the source.
        unsafe { crate::cf::CFRelease(self.source as *const c_void) };
    }
}

/// Input sink for the test pattern: logs events and moves the marker.
pub struct LogInput {
    pub pointer: Arc<Mutex<(f32, f32)>>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycode_table_sanity() {
        // Spot checks against HIToolbox kVK_* values.
        assert_eq!(evdev_to_mac(K::A), Some(0x00));
        assert_eq!(evdev_to_mac(K::Z), Some(0x06));
        assert_eq!(evdev_to_mac(K::ESC), Some(0x35));
        assert_eq!(evdev_to_mac(K::ENTER), Some(0x24));
        assert_eq!(evdev_to_mac(K::SPACE), Some(0x31));
        assert_eq!(evdev_to_mac(K::LEFTMETA), Some(0x37));
        assert_eq!(evdev_to_mac(K::RIGHTMETA), Some(0x36));
        assert_eq!(evdev_to_mac(K::LEFTALT), Some(0x3A));
        assert_eq!(evdev_to_mac(K::F1), Some(0x7A));
        assert_eq!(evdev_to_mac(K::UP), Some(0x7E));
        assert_eq!(evdev_to_mac(K::KPENTER), Some(0x4C));
        assert_eq!(evdev_to_mac(0), None);

        // Every main-block key (letters, digits, punctuation, modifiers,
        // F1-F12, navigation, keypad) is mapped, and no two of them collide.
        let mut seen = std::collections::HashMap::new();
        let required = (K::ESC..=K::KPDOT)
            .chain([K::F11, K::F12, K::KPENTER, K::RIGHTCTRL, K::KPSLASH, K::RIGHTALT])
            .chain(K::HOME..=K::DELETE)
            .chain([K::LEFTMETA, K::RIGHTMETA])
            .filter(|&k| k != K::SCROLLLOCK);
        for k in required {
            let vk = evdev_to_mac(k).unwrap_or_else(|| panic!("evdev {k} unmapped"));
            assert!(vk < 0x80, "evdev {k} -> {vk:#x} out of range");
            if let Some(prev) = seen.insert(vk, k) {
                panic!("evdev {prev} and {k} both map to {vk:#x}");
            }
        }
        // Letters map to distinct ANSI codes.
        let letters = [K::Q, K::W, K::E, K::R, K::T, K::Y, K::U, K::I, K::O, K::P];
        let mut v: Vec<u16> = letters.iter().map(|&k| evdev_to_mac(k).unwrap()).collect();
        v.dedup();
        assert_eq!(v.len(), letters.len());
    }

    #[test]
    fn modifier_flags_track_both_sides() {
        assert_eq!(modifier_mask(K::LEFTMETA).unwrap() & kCGEventFlagMaskCommand, kCGEventFlagMaskCommand);
        assert_eq!(key_flags(0x7E), kCGEventFlagMaskNumericPad | kCGEventFlagMaskSecondaryFn);
        assert_eq!(key_flags(0x00), 0);
    }
}
