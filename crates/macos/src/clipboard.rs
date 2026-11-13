//! Clipboard via NSPasteboard: text, and images as PNG. `changeCount` makes
//! polling cheap: contents are only read when the pasteboard changed.

use crate::objc::{ns_string, to_string};
use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use sard_core::media::Clipboard;
use std::ffi::c_void;
use std::sync::Arc;

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

const UTF8_TEXT: &str = "public.utf8-plain-text";
const PNG: &str = "public.png";
const TIFF: &str = "public.tiff";
/// NSBitmapImageFileTypePNG.
const FILE_TYPE_PNG: usize = 4;

pub struct MacClipboard {
    last_count: isize,
    text: Option<String>,
    /// None: not read since the last change.
    image: Option<Option<Arc<[u8]>>>,
}

fn pasteboard() -> *const AnyObject {
    // SAFETY: class method returning the shared pasteboard (never nil).
    unsafe { msg_send![class!(NSPasteboard), generalPasteboard] }
}

/// Copy an NSData's bytes.
///
/// # Safety
/// `data` is nil or a live NSData.
unsafe fn data_bytes(data: *const AnyObject) -> Option<Vec<u8>> {
    if data.is_null() {
        return None;
    }
    // SAFETY: NSData accessors; `bytes` is valid for `length` bytes while `data` lives.
    unsafe {
        let len: usize = msg_send![data, length];
        let p: *const c_void = msg_send![data, bytes];
        (len > 0 && !p.is_null()).then(|| std::slice::from_raw_parts(p.cast::<u8>(), len).to_vec())
    }
}

/// The pasteboard's image as PNG: PNG as is, anything else AppKit can read
/// as a bitmap (TIFF from most apps) converted.
fn read_png(pb: *const AnyObject) -> Option<Vec<u8>> {
    // SAFETY: NSPasteboard / NSBitmapImageRep methods with valid arguments;
    // returned objects are autoreleased and used within the caller's pool.
    unsafe {
        let png: *const AnyObject = msg_send![pb, dataForType: &*ns_string(PNG)];
        if let Some(b) = data_bytes(png) {
            return Some(b);
        }
        let tiff: *const AnyObject = msg_send![pb, dataForType: &*ns_string(TIFF)];
        if tiff.is_null() {
            return None;
        }
        let rep: *const AnyObject = msg_send![class!(NSBitmapImageRep), imageRepWithData: tiff];
        if rep.is_null() {
            return None;
        }
        let props: *const AnyObject = msg_send![class!(NSDictionary), dictionary];
        let png: *const AnyObject = msg_send![rep, representationUsingType: FILE_TYPE_PNG, properties: props];
        data_bytes(png)
    }
}

impl MacClipboard {
    pub fn new() -> Self {
        Self { last_count: -1, text: None, image: None }
    }

    /// Re-read the text if the pasteboard changed (the image is read lazily).
    fn refresh(&mut self) {
        let pb = pasteboard();
        // SAFETY: NSPasteboard instance methods.
        let count: isize = unsafe { msg_send![pb, changeCount] };
        if count != self.last_count {
            self.last_count = count;
            let ty = ns_string(UTF8_TEXT);
            let s: *const AnyObject = unsafe { msg_send![pb, stringForType: &*ty] };
            self.text = (!s.is_null()).then(|| unsafe { to_string(s) });
            self.image = None;
        }
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
            self.refresh();
            self.text.clone()
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
            self.text = Some(text.to_string());
            self.image = Some(None);
        })
    }

    fn get_image(&mut self) -> Option<Arc<[u8]>> {
        autoreleasepool(|_| {
            self.refresh();
            if self.text.as_deref().is_some_and(|t| !t.is_empty()) {
                return None;
            }
            self.image.get_or_insert_with(|| read_png(pasteboard()).map(Into::into)).clone()
        })
    }

    fn set_image(&mut self, png: &[u8]) -> bool {
        autoreleasepool(|_| {
            let pb = pasteboard();
            // SAFETY: NSPasteboard / NSData methods with valid arguments.
            let ok = unsafe {
                let data: *const AnyObject =
                    msg_send![class!(NSData), dataWithBytes: png.as_ptr().cast::<c_void>(), length: png.len()];
                let _: isize = msg_send![pb, clearContents];
                let ok: bool = msg_send![pb, setData: data, forType: &*ns_string(PNG)];
                self.last_count = msg_send![pb, changeCount];
                ok
            };
            self.text = None;
            self.image = Some(Some(png.into()));
            ok
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips text and an image through the real pasteboard and
    /// restores the previous text afterwards.
    #[test]
    fn pasteboard_roundtrip() {
        let mut c = MacClipboard::new();
        let original = c.get();
        c.set("sard clipboard test ✂");
        let mut fresh = MacClipboard::new();
        assert_eq!(fresh.get().as_deref(), Some("sard clipboard test ✂"));
        assert_eq!(fresh.get_image(), None, "text wins over images");

        let png = sard_core::png::encode_rgb(2, 1, &[255, 0, 0, 0, 0, 255]);
        assert!(c.set_image(&png));
        let mut fresh = MacClipboard::new();
        assert_eq!(fresh.get(), None);
        assert_eq!(fresh.get_image().as_deref(), Some(&png[..]));

        c.set(original.as_deref().unwrap_or(""));
    }
}
