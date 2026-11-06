//! Input injection with `SendInput`.
//!
//! Keys arrive as Linux evdev codes, a physical-key namespace; they are sent
//! as PC scan codes (`KEYEVENTF_SCANCODE`), so Windows applies the host's own
//! keyboard layout, as for a real keyboard. Absolute pointer positions are
//! normalized to the streamed screen and mapped into the virtual desktop.
//!
//! Windows refuses input into windows of elevated (administrator) apps and
//! onto the secure desktop (UAC, lock screen) unless sard itself runs
//! elevated; `SendInput` then reports fewer events inserted, logged once.

use crate::sys::{self, INPUT, INPUT_u, KEYBDINPUT, MOUSEINPUT, RECT};
use sard_core::input::key;
use sard_core::media::{self, InputEvent, InputSink};
use sard_core::proto::button;
use std::sync::{Arc, Mutex};

/// Where absolute coordinates land: the streamed screen inside the virtual desktop.
#[derive(Clone, Copy, Debug, Default)]
pub struct InputTarget {
    pub screen: RECT,
    pub desktop: RECT,
}

/// (scan code, extended) for an evdev key code.
pub fn scancode(code: u16) -> Option<(u16, bool)> {
    use key::*;
    let e = |s: u16| Some((s, true));
    match code {
        // Set 1 scan codes equal the evdev codes for the main block.
        1..=68 | 71..=83 => Some((code, false)),
        NUMLOCK => e(0x45), // NumLock needs the extended flag with SendInput (else it is Pause)
        SCROLLLOCK => Some((0x46, false)),
        KEY_102ND => Some((0x56, false)),
        F11 => Some((0x57, false)),
        F12 => Some((0x58, false)),
        RO => Some((0x73, false)),
        KATAKANAHIRAGANA => Some((0x70, false)),
        HENKAN => Some((0x79, false)),
        MUHENKAN => Some((0x7B, false)),
        YEN => Some((0x7D, false)),
        HANGEUL => Some((0x72, false)),
        HANJA => Some((0x71, false)),
        KPEQUAL => Some((0x59, false)),
        KPCOMMA => Some((0x7E, false)),
        KPENTER => e(0x1C),
        RIGHTCTRL => e(0x1D),
        KPSLASH => e(0x35),
        SYSRQ => e(0x37),
        RIGHTALT => e(0x38),
        HOME => e(0x47),
        UP => e(0x48),
        PAGEUP => e(0x49),
        LEFT => e(0x4B),
        RIGHT => e(0x4D),
        END => e(0x4F),
        DOWN => e(0x50),
        PAGEDOWN => e(0x51),
        INSERT => e(0x52),
        DELETE => e(0x53),
        LEFTMETA => e(0x5B),
        RIGHTMETA => e(0x5C),
        COMPOSE => e(0x5D),
        MUTE => e(0x20),
        VOLUMEDOWN => e(0x2E),
        VOLUMEUP => e(0x30),
        NEXTSONG => e(0x19),
        PLAYPAUSE => e(0x22),
        PREVIOUSSONG => e(0x10),
        STOPCD => e(0x24),
        F13..=F23 => Some((0x64 + (code - F13), false)),
        F24 => Some((0x76, false)),
        _ => None,
    }
}

const VK_PAUSE: u16 = 0x13;

pub struct SendInputSink {
    target: Arc<Mutex<InputTarget>>,
    rel_acc: (f32, f32),
    wheel_acc: (f32, f32),
    warned: bool,
}

impl SendInputSink {
    pub fn new(target: Arc<Mutex<InputTarget>>) -> Self {
        SendInputSink { target, rel_acc: (0.0, 0.0), wheel_acc: (0.0, 0.0), warned: false }
    }

    fn send(&mut self, inputs: &[INPUT]) {
        // SAFETY: valid array of INPUT structs of the stated size.
        let n = unsafe { sys::SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
        if (n as usize) < inputs.len() && !self.warned {
            self.warned = true;
            // SAFETY: plain thread-local error query.
            let err = unsafe { sys::GetLastError() };
            sard_core::warn!(
                "SendInput blocked ({}): the focused window may belong to an elevated app, or the secure desktop \
                 (UAC / lock screen) is showing; run sard as administrator to control those",
                sys::error_message(err)
            );
        }
    }

    fn mouse(&mut self, dx: i32, dy: i32, data: i32, flags: u32) {
        let mi = MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 };
        self.send(&[INPUT { r#type: sys::INPUT_MOUSE, u: INPUT_u { mi } }]);
    }

    fn key(&mut self, code: u16, down: bool) {
        let up = if down { 0 } else { sys::KEYEVENTF_KEYUP };
        let ki = if code == key::PAUSE {
            KEYBDINPUT { wVk: VK_PAUSE, wScan: 0, dwFlags: up, time: 0, dwExtraInfo: 0 }
        } else {
            let Some((scan, ext)) = scancode(code) else {
                sard_core::debug!("no Windows scan code for evdev key {code}");
                return;
            };
            let flags = sys::KEYEVENTF_SCANCODE | up | if ext { sys::KEYEVENTF_EXTENDEDKEY } else { 0 };
            KEYBDINPUT { wVk: 0, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 }
        };
        self.send(&[INPUT { r#type: sys::INPUT_KEYBOARD, u: INPUT_u { ki } }]);
    }

    fn move_abs(&mut self, x: f32, y: f32) {
        let t = *self.target.lock().unwrap();
        let (s, d) = (t.screen, t.desktop);
        let px = s.left as f32 + x.clamp(0.0, 1.0) * ((s.right - s.left).max(1) - 1) as f32;
        let py = s.top as f32 + y.clamp(0.0, 1.0) * ((s.bottom - s.top).max(1) - 1) as f32;
        let (dw, dh) = (((d.right - d.left).max(2) - 1) as f32, ((d.bottom - d.top).max(2) - 1) as f32);
        // 0..65535 across the whole virtual desktop.
        let nx = ((px - d.left as f32) * 65535.0 / dw).round() as i32;
        let ny = ((py - d.top as f32) * 65535.0 / dh).round() as i32;
        self.mouse(nx, ny, 0, sys::MOUSEEVENTF_MOVE | sys::MOUSEEVENTF_ABSOLUTE | sys::MOUSEEVENTF_VIRTUALDESK);
    }

    fn move_rel(&mut self, dx: f32, dy: f32) {
        self.rel_acc.0 += dx;
        self.rel_acc.1 += dy;
        let (ix, iy) = (self.rel_acc.0.trunc(), self.rel_acc.1.trunc());
        if ix == 0.0 && iy == 0.0 {
            return;
        }
        self.rel_acc.0 -= ix;
        self.rel_acc.1 -= iy;
        self.mouse(ix as i32, iy as i32, 0, sys::MOUSEEVENTF_MOVE);
    }

    fn button(&mut self, b: u8, down: bool) {
        let (flags, data) = match (b, down) {
            (button::LEFT, true) => (sys::MOUSEEVENTF_LEFTDOWN, 0),
            (button::LEFT, false) => (sys::MOUSEEVENTF_LEFTUP, 0),
            (button::RIGHT, true) => (sys::MOUSEEVENTF_RIGHTDOWN, 0),
            (button::RIGHT, false) => (sys::MOUSEEVENTF_RIGHTUP, 0),
            (button::MIDDLE, true) => (sys::MOUSEEVENTF_MIDDLEDOWN, 0),
            (button::MIDDLE, false) => (sys::MOUSEEVENTF_MIDDLEUP, 0),
            (button::BACK, true) => (sys::MOUSEEVENTF_XDOWN, sys::XBUTTON1),
            (button::BACK, false) => (sys::MOUSEEVENTF_XUP, sys::XBUTTON1),
            (button::FORWARD, true) => (sys::MOUSEEVENTF_XDOWN, sys::XBUTTON2),
            (button::FORWARD, false) => (sys::MOUSEEVENTF_XUP, sys::XBUTTON2),
            _ => return,
        };
        self.mouse(0, 0, data as i32, flags);
    }

    /// Wire convention: dy > 0 scrolls up, dx > 0 scrolls left; notches or
    /// logical pixels. Windows: WHEEL > 0 is up, HWHEEL > 0 is right, 120 per notch.
    fn scroll(&mut self, dx: f32, dy: f32, discrete: bool) {
        let per = if discrete { sys::WHEEL_DELTA as f32 } else { 3.0 };
        self.wheel_acc.0 += -dx * per;
        self.wheel_acc.1 += dy * per;
        let (h, v) = (self.wheel_acc.0.trunc(), self.wheel_acc.1.trunc());
        self.wheel_acc.0 -= h;
        self.wheel_acc.1 -= v;
        if v != 0.0 {
            self.mouse(0, 0, v as i32, sys::MOUSEEVENTF_WHEEL);
        }
        if h != 0.0 {
            self.mouse(0, 0, h as i32, sys::MOUSEEVENTF_HWHEEL);
        }
    }
}

impl InputSink for SendInputSink {
    fn inject(&mut self, ev: &InputEvent) -> media::Result<()> {
        match *ev {
            InputEvent::Key { code, down } => self.key(code, down),
            InputEvent::Button { button, down } => self.button(button, down),
            InputEvent::MoveAbs { x, y } => self.move_abs(x, y),
            InputEvent::MoveRel { dx, dy } => self.move_rel(dx, dy),
            InputEvent::Scroll { dx, dy, discrete } => self.scroll(dx, dy, discrete),
        }
        Ok(())
    }
}

/// Input sink for the test pattern: logs events and moves the marker.
pub struct LogInput {
    pub pointer: Arc<Mutex<(f32, f32)>>,
}

impl InputSink for LogInput {
    fn inject(&mut self, ev: &InputEvent) -> media::Result<()> {
        sard_core::debug!("input: {ev:?}");
        let mut p = self.pointer.lock().unwrap();
        match *ev {
            InputEvent::MoveAbs { x, y } => *p = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)),
            InputEvent::MoveRel { dx, dy } => {
                *p = ((p.0 + dx / 1000.0).clamp(0.0, 1.0), (p.1 + dy / 1000.0).clamp(0.0, 1.0))
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scancodes() {
        assert_eq!(scancode(key::A), Some((0x1E, false)));
        assert_eq!(scancode(key::ESC), Some((0x01, false)));
        assert_eq!(scancode(key::KP0), Some((0x52, false)));
        assert_eq!(scancode(key::INSERT), Some((0x52, true)));
        assert_eq!(scancode(key::RIGHTCTRL), Some((0x1D, true)));
        assert_eq!(scancode(key::LEFTMETA), Some((0x5B, true)));
        assert_eq!(scancode(key::F12), Some((0x58, false)));
        assert_eq!(scancode(key::F13), Some((0x64, false)));
        assert_eq!(scancode(key::F23), Some((0x6E, false)));
        assert_eq!(scancode(key::NUMLOCK), Some((0x45, true)));
        assert_eq!(scancode(0x2ff), None);
    }
}
