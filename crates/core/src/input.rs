//! Input event codes. Keys travel on the wire as Linux evdev key codes
//! (`linux/input-event-codes.h`), a stable, layout-independent physical-key
//! namespace. Clients translate their native scancodes to these; the macOS
//! host translates them to virtual key codes.

#![allow(dead_code)]

pub mod key {
    pub const ESC: u16 = 1;
    pub const N1: u16 = 2;
    pub const N2: u16 = 3;
    pub const N3: u16 = 4;
    pub const N4: u16 = 5;
    pub const N5: u16 = 6;
    pub const N6: u16 = 7;
    pub const N7: u16 = 8;
    pub const N8: u16 = 9;
    pub const N9: u16 = 10;
    pub const N0: u16 = 11;
    pub const MINUS: u16 = 12;
    pub const EQUAL: u16 = 13;
    pub const BACKSPACE: u16 = 14;
    pub const TAB: u16 = 15;
    pub const Q: u16 = 16;
    pub const W: u16 = 17;
    pub const E: u16 = 18;
    pub const R: u16 = 19;
    pub const T: u16 = 20;
    pub const Y: u16 = 21;
    pub const U: u16 = 22;
    pub const I: u16 = 23;
    pub const O: u16 = 24;
    pub const P: u16 = 25;
    pub const LEFTBRACE: u16 = 26;
    pub const RIGHTBRACE: u16 = 27;
    pub const ENTER: u16 = 28;
    pub const LEFTCTRL: u16 = 29;
    pub const A: u16 = 30;
    pub const S: u16 = 31;
    pub const D: u16 = 32;
    pub const F: u16 = 33;
    pub const G: u16 = 34;
    pub const H: u16 = 35;
    pub const J: u16 = 36;
    pub const K: u16 = 37;
    pub const L: u16 = 38;
    pub const SEMICOLON: u16 = 39;
    pub const APOSTROPHE: u16 = 40;
    pub const GRAVE: u16 = 41;
    pub const LEFTSHIFT: u16 = 42;
    pub const BACKSLASH: u16 = 43;
    pub const Z: u16 = 44;
    pub const X: u16 = 45;
    pub const C: u16 = 46;
    pub const V: u16 = 47;
    pub const B: u16 = 48;
    pub const N: u16 = 49;
    pub const M: u16 = 50;
    pub const COMMA: u16 = 51;
    pub const DOT: u16 = 52;
    pub const SLASH: u16 = 53;
    pub const RIGHTSHIFT: u16 = 54;
    pub const KPASTERISK: u16 = 55;
    pub const LEFTALT: u16 = 56;
    pub const SPACE: u16 = 57;
    pub const CAPSLOCK: u16 = 58;
    pub const F1: u16 = 59;
    pub const F2: u16 = 60;
    pub const F3: u16 = 61;
    pub const F4: u16 = 62;
    pub const F5: u16 = 63;
    pub const F6: u16 = 64;
    pub const F7: u16 = 65;
    pub const F8: u16 = 66;
    pub const F9: u16 = 67;
    pub const F10: u16 = 68;
    pub const NUMLOCK: u16 = 69;
    pub const SCROLLLOCK: u16 = 70;
    pub const KP7: u16 = 71;
    pub const KP8: u16 = 72;
    pub const KP9: u16 = 73;
    pub const KPMINUS: u16 = 74;
    pub const KP4: u16 = 75;
    pub const KP5: u16 = 76;
    pub const KP6: u16 = 77;
    pub const KPPLUS: u16 = 78;
    pub const KP1: u16 = 79;
    pub const KP2: u16 = 80;
    pub const KP3: u16 = 81;
    pub const KP0: u16 = 82;
    pub const KPDOT: u16 = 83;
    pub const ZENKAKUHANKAKU: u16 = 85;
    pub const KEY_102ND: u16 = 86;
    pub const F11: u16 = 87;
    pub const F12: u16 = 88;
    pub const RO: u16 = 89;
    pub const KATAKANA: u16 = 90;
    pub const HIRAGANA: u16 = 91;
    pub const HENKAN: u16 = 92;
    pub const KATAKANAHIRAGANA: u16 = 93;
    pub const MUHENKAN: u16 = 94;
    pub const KPJPCOMMA: u16 = 95;
    pub const KPENTER: u16 = 96;
    pub const RIGHTCTRL: u16 = 97;
    pub const KPSLASH: u16 = 98;
    pub const SYSRQ: u16 = 99;
    pub const RIGHTALT: u16 = 100;
    pub const HOME: u16 = 102;
    pub const UP: u16 = 103;
    pub const PAGEUP: u16 = 104;
    pub const LEFT: u16 = 105;
    pub const RIGHT: u16 = 106;
    pub const END: u16 = 107;
    pub const DOWN: u16 = 108;
    pub const PAGEDOWN: u16 = 109;
    pub const INSERT: u16 = 110;
    pub const DELETE: u16 = 111;
    pub const MUTE: u16 = 113;
    pub const VOLUMEDOWN: u16 = 114;
    pub const VOLUMEUP: u16 = 115;
    pub const POWER: u16 = 116;
    pub const KPEQUAL: u16 = 117;
    pub const PAUSE: u16 = 119;
    pub const KPCOMMA: u16 = 121;
    pub const HANGEUL: u16 = 122;
    pub const HANJA: u16 = 123;
    pub const YEN: u16 = 124;
    pub const LEFTMETA: u16 = 125;
    pub const RIGHTMETA: u16 = 126;
    pub const COMPOSE: u16 = 127;
    pub const NEXTSONG: u16 = 163;
    pub const PLAYPAUSE: u16 = 164;
    pub const PREVIOUSSONG: u16 = 165;
    pub const STOPCD: u16 = 166;
    pub const F13: u16 = 183;
    pub const F14: u16 = 184;
    pub const F15: u16 = 185;
    pub const F16: u16 = 186;
    pub const F17: u16 = 187;
    pub const F18: u16 = 188;
    pub const F19: u16 = 189;
    pub const F20: u16 = 190;
    pub const F21: u16 = 191;
    pub const F22: u16 = 192;
    pub const F23: u16 = 193;
    pub const F24: u16 = 194;
    /// Highest key code we forward.
    pub const MAX: u16 = 0x2ff;
}

/// evdev mouse button codes.
pub mod btn {
    pub const LEFT: u16 = 0x110;
    pub const RIGHT: u16 = 0x111;
    pub const MIDDLE: u16 = 0x112;
    pub const SIDE: u16 = 0x113;
    pub const EXTRA: u16 = 0x114;
}

/// Map a wire mouse button (`proto::button`) to an evdev button code.
pub fn wire_button_to_evdev(b: u8) -> Option<u16> {
    use crate::proto::button;
    Some(match b {
        button::LEFT => btn::LEFT,
        button::RIGHT => btn::RIGHT,
        button::MIDDLE => btn::MIDDLE,
        button::BACK => btn::SIDE,
        button::FORWARD => btn::EXTRA,
        _ => return None,
    })
}

/// Tracks pressed keys/buttons on the host so everything can be released when
/// a client disconnects or loses focus (no stuck modifiers).
#[derive(Default)]
pub struct PressedSet {
    keys: Vec<u16>,
    buttons: Vec<u8>,
}

impl PressedSet {
    pub fn key(&mut self, code: u16, down: bool) -> bool {
        let pos = self.keys.iter().position(|&k| k == code);
        match (down, pos) {
            (true, None) => {
                self.keys.push(code);
                true
            }
            (false, Some(i)) => {
                self.keys.swap_remove(i);
                true
            }
            // Repeat press: forward (autorepeat on host); stray release: drop.
            (true, Some(_)) => true,
            (false, None) => false,
        }
    }

    pub fn button(&mut self, b: u8, down: bool) -> bool {
        let pos = self.buttons.iter().position(|&x| x == b);
        match (down, pos) {
            (true, None) => {
                self.buttons.push(b);
                true
            }
            (false, Some(i)) => {
                self.buttons.swap_remove(i);
                true
            }
            (true, Some(_)) => false,
            (false, None) => false,
        }
    }

    /// Drain everything currently held.
    pub fn take_all(&mut self) -> (Vec<u16>, Vec<u8>) {
        (std::mem::take(&mut self.keys), std::mem::take(&mut self.buttons))
    }
}
