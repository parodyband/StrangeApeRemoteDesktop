//! Text clipboard via NSPasteboard. `changeCount` makes polling cheap: the
//! string is only read when the pasteboard actually changed.

use crate::objc::{ns_string, to_string};
use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use sard_core::media::Clipboard;

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

const UTF8_TEXT: &str = "public.utf8-plain-text";

pub struct MacClipboard {
    last_count: isize,
    cached: Option<String>,
}

fn pasteboard() -> *const AnyObject {
    // SAFETY: class method returning the shared pasteboard (never nil).
    unsafe { msg_send![class!(NSPasteboard), generalPasteboard] }
}

impl MacClipboard {
    pub fn new() -> Self {
        Self { last_count: -1, cached: None }
    }
}

impl Default for MacClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Clipboard for MacClipboard {
    fn get(&mut self) -> Option<String> {
        autoreleasepool(|_| {
            let pb = pasteboard();
            // SAFETY: NSPasteboard instance methods.
            let count: isize = unsafe { msg_send![pb, changeCount] };
            if count != self.last_count {
                self.last_count = count;
                let ty = ns_string(UTF8_TEXT);
                let s: *const AnyObject = unsafe { msg_send![pb, stringForType: &*ty] };
                self.cached = (!s.is_null()).then(|| unsafe { to_string(s) });
            }
            self.cached.clone()
        })
    }

    fn set(&mut self, text: &str) {
        autoreleasepool(|_| {
            let pb = pasteboard();
            let (s, ty) = (ns_string(text), ns_string(UTF8_TEXT));
            // SAFETY: NSPasteboard instance methods with valid NSStrings.
            unsafe {
                let _: isize = msg_send![pb, clearContents];
                let _: bool = msg_send![pb, setString: &*s, forType: &*ty];
                self.last_count = msg_send![pb, changeCount];
            }
            self.cached = Some(text.to_string());
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips text through the real pasteboard and restores the previous
    /// contents afterwards.
    #[test]
    fn pasteboard_roundtrip() {
        let mut c = MacClipboard::new();
        let original = c.get();
        c.set("sard clipboard test ✂");
        let mut fresh = MacClipboard::new();
        assert_eq!(fresh.get().as_deref(), Some("sard clipboard test ✂"));
        c.set(original.as_deref().unwrap_or(""));
    }
}
