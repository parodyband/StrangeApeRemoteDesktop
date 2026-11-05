//! Tiny Objective-C helpers on top of `objc2` (NSString, NSError).

use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use std::ffi::{CStr, c_char, c_void};

/// Owned NSString from UTF-8.
pub fn ns_string(s: &str) -> Retained<AnyObject> {
    // SAFETY: NSString alloc/init with valid UTF-8 bytes (NSUTF8StringEncoding = 4).
    unsafe {
        let a: Allocated<AnyObject> = msg_send![class!(NSString), alloc];
        let r: Option<Retained<AnyObject>> =
            msg_send![a, initWithBytes: s.as_ptr() as *const c_void, length: s.len(), encoding: 4usize];
        r.expect("NSString initWithBytes")
    }
}

/// Rust copy of an NSString (empty for nil).
///
/// # Safety
/// `s` must be nil or an NSString.
pub unsafe fn to_string(s: *const AnyObject) -> String {
    if s.is_null() {
        return String::new();
    }
    // SAFETY: caller contract; UTF8String is valid while `s` lives.
    unsafe {
        let p: *const c_char = msg_send![s, UTF8String];
        if p.is_null() { String::new() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
    }
}

/// "description (domain code)" for an NSError (or "unknown error" for nil).
///
/// # Safety
/// `e` must be nil or an NSError.
pub unsafe fn error_string(e: *const AnyObject) -> String {
    if e.is_null() {
        return "unknown error".into();
    }
    // SAFETY: caller contract; standard NSError accessors.
    unsafe {
        let desc: *const AnyObject = msg_send![e, localizedDescription];
        let domain: *const AnyObject = msg_send![e, domain];
        let code: isize = msg_send![e, code];
        format!("{} ({} {code})", to_string(desc), to_string(domain))
    }
}
