//! Keyboard input from the phone, as the evdev key codes the protocol uses.
//!
//! * Typed text (the on-screen keyboard) becomes key presses on a US layout:
//!   the host turns them back into characters with its own layout, which
//!   matches for the usual US/ANSI setups. Characters without a key (emoji,
//!   accents) can't be typed this way.
//! * Hardware keyboards report USB HID usages (`UIKey.keyCode`), mapped here.

use sard_core::input::key;

/// Key and whether Shift is needed for `c` on a US layout.
pub fn char_to_key(c: char) -> Option<(u16, bool)> {
    let lower = |k| Some((k, false));
    let upper = |k| Some((k, true));
    match c {
        'a'..='z' => lower(letter(c)),
        'A'..='Z' => upper(letter(c.to_ascii_lowercase())),
        '1'..='9' => lower(key::N1 + (c as u16 - '1' as u16)),
        '0' => lower(key::N0),
        ' ' => lower(key::SPACE),
        '\n' | '\r' => lower(key::ENTER),
        '\t' => lower(key::TAB),
        '\u{8}' | '\u{7f}' => lower(key::BACKSPACE),
        '\u{1b}' => lower(key::ESC),
        '-' => lower(key::MINUS),
        '_' => upper(key::MINUS),
        '=' => lower(key::EQUAL),
        '+' => upper(key::EQUAL),
        '[' => lower(key::LEFTBRACE),
        '{' => upper(key::LEFTBRACE),
        ']' => lower(key::RIGHTBRACE),
        '}' => upper(key::RIGHTBRACE),
        '\\' => lower(key::BACKSLASH),
        '|' => upper(key::BACKSLASH),
        ';' => lower(key::SEMICOLON),
        ':' => upper(key::SEMICOLON),
        '\'' => lower(key::APOSTROPHE),
        '"' => upper(key::APOSTROPHE),
        '`' => lower(key::GRAVE),
        '~' => upper(key::GRAVE),
        ',' => lower(key::COMMA),
        '<' => upper(key::COMMA),
        '.' => lower(key::DOT),
        '>' => upper(key::DOT),
        '/' => lower(key::SLASH),
        '?' => upper(key::SLASH),
        '!' => upper(key::N1),
        '@' => upper(key::N2),
        '#' => upper(key::N3),
        '$' => upper(key::N4),
        '%' => upper(key::N5),
        '^' => upper(key::N6),
        '&' => upper(key::N7),
        '*' => upper(key::N8),
        '(' => upper(key::N9),
        ')' => upper(key::N0),
        // iOS "smart punctuation" replacements: type what was meant.
        '‘' | '’' => lower(key::APOSTROPHE),
        '“' | '”' => upper(key::APOSTROPHE),
        '–' | '—' => lower(key::MINUS),
        _ => None,
    }
}

fn letter(c: char) -> u16 {
    use key::*;
    const ROW: [u16; 26] = [A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z];
    ROW[(c as u8 - b'a') as usize]
}

/// USB HID keyboard usage (page 7) → evdev key code (0 = unmapped).
pub fn hid_to_evdev(usage: u32) -> u16 {
    const T: [u16; 0x68] = [
        0, 0, 0, 0, // 0x00-0x03: none, error codes
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21,
        44, // 0x04-0x1d: A-Z
        2, 3, 4, 5, 6, 7, 8, 9, 10, 11, // 0x1e-0x27: 1-0
        28, 1, 14, 15, 57, 12, 13, 26, 27, 43, // 0x28-0x31: Enter Esc Backspace Tab Space - = [ ] \
        43, 39, 40, 41, 51, 52, 53, 58, // 0x32-0x39: Non-US # ; ' ` , . / CapsLock
        59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88, // 0x3a-0x45: F1-F12
        99, 70, 119, 110, 102, 104, 111, 107, 109, // 0x46-0x4e: PrtSc ScrLk Pause Ins Home PgUp Del End PgDn
        106, 105, 108, 103, // 0x4f-0x52: Right Left Down Up
        69, 98, 55, 74, 78, 96, // 0x53-0x58: NumLock KP/ KP* KP- KP+ KPEnter
        79, 80, 81, 75, 76, 77, 71, 72, 73, 82, 83, // 0x59-0x63: KP1-KP9 KP0 KP.
        86, 127, 116, 117, // 0x64-0x67: Non-US \ Menu Power KP=
    ];
    match usage {
        0xe0 => 29,                                 // Left Control
        0xe1 => 42,                                 // Left Shift
        0xe2 => 56,                                 // Left Alt (Option)
        0xe3 => 125,                                // Left GUI (Command)
        0xe4 => 97,                                 // Right Control
        0xe5 => 54,                                 // Right Shift
        0xe6 => 100,                                // Right Alt
        0xe7 => 126,                                // Right GUI
        0x68..=0x73 => 183 + (usage - 0x68) as u16, // F13-F24
        u if (u as usize) < T.len() => T[u as usize],
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_maps_to_us_keys() {
        assert_eq!(char_to_key('a'), Some((key::A, false)));
        assert_eq!(char_to_key('Q'), Some((key::Q, true)));
        assert_eq!(char_to_key('7'), Some((key::N7, false)));
        assert_eq!(char_to_key('&'), Some((key::N7, true)));
        assert_eq!(char_to_key('?'), Some((key::SLASH, true)));
        assert_eq!(char_to_key('’'), Some((key::APOSTROPHE, false)));
        assert_eq!(char_to_key('é'), None);
        for c in (32u8..127).map(char::from) {
            assert!(char_to_key(c).is_some(), "printable ASCII {c:?} unmapped");
        }
    }

    #[test]
    fn hid_usages() {
        assert_eq!(hid_to_evdev(0x04), key::A);
        assert_eq!(hid_to_evdev(0x1d), key::Z);
        assert_eq!(hid_to_evdev(0x27), key::N0);
        assert_eq!(hid_to_evdev(0x28), key::ENTER);
        assert_eq!(hid_to_evdev(0x35), key::GRAVE);
        assert_eq!(hid_to_evdev(0x45), 88); // F12
        assert_eq!(hid_to_evdev(0x52), 103); // Up
        assert_eq!(hid_to_evdev(0xe3), 125); // Command → Meta
        assert_eq!(hid_to_evdev(0x200), 0);
        // Letters agree with the text table.
        for (i, c) in ('a'..='z').enumerate() {
            assert_eq!(hid_to_evdev(0x04 + i as u32), char_to_key(c).unwrap().0, "{c}");
        }
    }
}
