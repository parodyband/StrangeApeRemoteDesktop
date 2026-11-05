//! Tiny Objective-C helpers on top of `objc2` (NSString, NSError, main thread).

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

/// Run `f` on the main thread: right away when already there, otherwise
/// asynchronously through the main dispatch queue (serviced by the main run
/// loop, i.e. NSApplication / winit).
pub fn run_on_main(f: impl FnOnce() + Send + 'static) {
    if objc2::MainThreadMarker::new().is_some() {
        f();
    } else {
        main_async(f);
    }
}

/// Queue `f` on the main dispatch queue (also when called on the main thread:
/// runs after the current event has been handled).
pub fn main_async(f: impl FnOnce() + Send + 'static) {
    unsafe extern "C" fn trampoline(ctx: *mut c_void) {
        // SAFETY: `ctx` is the box leaked below, consumed exactly once.
        let f = unsafe { Box::from_raw(ctx as *mut Box<dyn FnOnce() + Send>) };
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
            sard_core::warn!("panic in main-thread task");
        }
    }
    let f: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(f));
    // SAFETY: the main queue is immortal; the trampoline takes ownership of the box.
    unsafe {
        crate::ffi::dispatch_async_f(crate::ffi::dispatch_get_main_queue(), Box::into_raw(f) as *mut c_void, trampoline)
    };
}
