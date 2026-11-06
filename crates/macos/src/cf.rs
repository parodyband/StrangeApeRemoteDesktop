//! Hand-written CoreFoundation bindings plus a small owned-reference wrapper.

#![allow(non_upper_case_globals, non_snake_case, dead_code)]

use std::ffi::{c_char, c_void};
use std::ptr::NonNull;

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFMutableDictionaryRef = *mut c_void;
pub type CFArrayRef = *const c_void;
pub type CFNumberRef = *const c_void;
pub type CFBooleanRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFIndex = isize;
pub type CFTypeID = usize;
pub type Boolean = u8;
pub type OSStatus = i32;

pub const kCFNumberSInt32Type: CFIndex = 3;
pub const kCFNumberSInt64Type: CFIndex = 4;
pub const kCFNumberFloat64Type: CFIndex = 6;
const kCFStringEncodingUTF8: u32 = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub static kCFAllocatorDefault: CFAllocatorRef;
    pub static kCFAllocatorNull: CFAllocatorRef;
    pub static kCFBooleanTrue: CFBooleanRef;
    pub static kCFBooleanFalse: CFBooleanRef;
    // Callback structs; only their addresses are used.
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
    static kCFTypeArrayCallBacks: u8;

    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    pub fn CFBooleanGetTypeID() -> CFTypeID;
    pub fn CFNumberGetTypeID() -> CFTypeID;
    pub fn CFStringGetTypeID() -> CFTypeID;
    pub fn CFBooleanGetValue(b: CFBooleanRef) -> Boolean;
    pub fn CFNumberCreate(alloc: CFAllocatorRef, the_type: CFIndex, value: *const c_void) -> CFNumberRef;
    pub fn CFNumberGetValue(n: CFNumberRef, the_type: CFIndex, value: *mut c_void) -> Boolean;
    pub fn CFDictionaryCreateMutable(
        alloc: CFAllocatorRef,
        capacity: CFIndex,
        key_cb: *const c_void,
        value_cb: *const c_void,
    ) -> CFMutableDictionaryRef;
    pub fn CFDictionarySetValue(d: CFMutableDictionaryRef, key: *const c_void, value: *const c_void);
    pub fn CFDictionaryGetValue(d: CFDictionaryRef, key: *const c_void) -> *const c_void;
    pub fn CFArrayCreate(
        alloc: CFAllocatorRef,
        values: *const *const c_void,
        count: CFIndex,
        cb: *const c_void,
    ) -> CFArrayRef;
    pub fn CFArrayGetCount(a: CFArrayRef) -> CFIndex;
    pub fn CFArrayGetValueAtIndex(a: CFArrayRef, i: CFIndex) -> *const c_void;
    pub fn CFStringCreateWithBytes(
        alloc: CFAllocatorRef,
        bytes: *const u8,
        len: CFIndex,
        encoding: u32,
        external: Boolean,
    ) -> CFStringRef;
    pub fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: CFIndex, encoding: u32) -> Boolean;
    pub fn CFCopyDescription(cf: CFTypeRef) -> CFStringRef;
    pub fn CFDataCreate(alloc: CFAllocatorRef, bytes: *const u8, len: CFIndex) -> CFTypeRef;
}

/// An owned (+1) CoreFoundation reference, released on drop.
pub struct CfRef(NonNull<c_void>);

// SAFETY: the CF objects we hold (immutable dictionaries/numbers/strings,
// CVPixelBuffers, VT sessions, format descriptions) are thread-safe to
// retain/release and are only mutated through thread-safe APIs.
unsafe impl Send for CfRef {}
unsafe impl Sync for CfRef {}

impl CfRef {
    /// Take ownership of a +1 reference (result of a Create/Copy function).
    ///
    /// # Safety
    /// `p` must be a valid CF object the caller owns, or null.
    pub unsafe fn from_owned(p: *const c_void) -> Option<Self> {
        NonNull::new(p as *mut c_void).map(CfRef)
    }

    /// Retain a +0 reference.
    ///
    /// # Safety
    /// `p` must be a valid CF object, or null.
    pub unsafe fn retain(p: *const c_void) -> Option<Self> {
        let r = NonNull::new(p as *mut c_void)?;
        // SAFETY: valid object per contract.
        unsafe { CFRetain(r.as_ptr()) };
        Some(CfRef(r))
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Clone for CfRef {
    fn clone(&self) -> Self {
        // SAFETY: we hold a valid reference.
        unsafe { CFRetain(self.0.as_ptr()) };
        CfRef(self.0)
    }
}

impl Drop for CfRef {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { CFRelease(self.0.as_ptr()) };
    }
}

impl std::fmt::Debug for CfRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CfRef({:p})", self.0)
    }
}

pub fn string(s: &str) -> CfRef {
    // SAFETY: valid UTF-8 bytes; CF copies them.
    unsafe {
        CfRef::from_owned(CFStringCreateWithBytes(
            kCFAllocatorDefault,
            s.as_ptr(),
            s.len() as CFIndex,
            kCFStringEncodingUTF8,
            0,
        ))
        .expect("CFStringCreateWithBytes")
    }
}

/// Rust copy of a CFString (lossy; empty for null / non-strings).
///
/// # Safety
/// `s` must be null or a valid CF object.
pub unsafe fn to_string(s: CFStringRef) -> String {
    // SAFETY: caller contract.
    unsafe {
        if s.is_null() || CFGetTypeID(s) != CFStringGetTypeID() {
            return String::new();
        }
        let mut buf = vec![0 as c_char; 1024];
        if CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as CFIndex, kCFStringEncodingUTF8) == 0 {
            return String::new();
        }
        std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

/// An immutable CFData copy of `bytes`.
pub fn data(bytes: &[u8]) -> CfRef {
    // SAFETY: copies `bytes`; +1 result.
    unsafe { CfRef::from_owned(CFDataCreate(kCFAllocatorDefault, bytes.as_ptr(), bytes.len() as CFIndex)) }
        .expect("CFDataCreate")
}

pub fn i32(v: i32) -> CfRef {
    // SAFETY: valid pointer to an i32 for the duration of the call.
    unsafe {
        CfRef::from_owned(CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &v as *const i32 as _)).unwrap()
    }
}

pub fn i64(v: i64) -> CfRef {
    // SAFETY: as above.
    unsafe {
        CfRef::from_owned(CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt64Type, &v as *const i64 as _)).unwrap()
    }
}

pub fn f64(v: f64) -> CfRef {
    // SAFETY: as above.
    unsafe {
        CfRef::from_owned(CFNumberCreate(kCFAllocatorDefault, kCFNumberFloat64Type, &v as *const f64 as _)).unwrap()
    }
}

pub fn boolean(v: bool) -> CFBooleanRef {
    // SAFETY: immortal constants.
    unsafe { if v { kCFBooleanTrue } else { kCFBooleanFalse } }
}

/// Read a CFNumber / CFBoolean as i64.
///
/// # Safety
/// `v` must be null or a valid CF object.
pub unsafe fn number_value(v: *const c_void) -> Option<i64> {
    // SAFETY: caller contract; type checked before use.
    unsafe {
        if v.is_null() {
            return None;
        }
        let t = CFGetTypeID(v);
        if t == CFNumberGetTypeID() {
            let mut out = 0i64;
            CFNumberGetValue(v, kCFNumberSInt64Type, &mut out as *mut i64 as _);
            Some(out)
        } else if t == CFBooleanGetTypeID() {
            Some(CFBooleanGetValue(v) as i64)
        } else {
            None
        }
    }
}

/// Mutable CFDictionary with CFType callbacks.
pub struct Dict(CfRef);

impl Default for Dict {
    fn default() -> Self {
        Self::new()
    }
}

impl Dict {
    pub fn new() -> Self {
        // SAFETY: standard CFType callbacks.
        unsafe {
            Dict(
                CfRef::from_owned(CFDictionaryCreateMutable(
                    kCFAllocatorDefault,
                    0,
                    &raw const kCFTypeDictionaryKeyCallBacks as _,
                    &raw const kCFTypeDictionaryValueCallBacks as _,
                ))
                .expect("CFDictionaryCreateMutable"),
            )
        }
    }

    /// Set `key` (a CFString constant) to any CF value (retained by the dict).
    ///
    /// # Safety
    /// `key` and `value` must be valid CF objects.
    pub unsafe fn set(self, key: CFStringRef, value: *const c_void) -> Self {
        // SAFETY: caller contract; the dictionary retains both.
        unsafe { CFDictionarySetValue(self.0.as_ptr(), key, value) };
        self
    }

    /// # Safety
    /// `key` must be a valid CFString.
    pub unsafe fn set_ref(self, key: CFStringRef, value: &CfRef) -> Self {
        // SAFETY: caller contract; `value` is valid.
        unsafe { self.set(key, value.as_ptr()) }
    }

    pub fn as_ptr(&self) -> CFDictionaryRef {
        self.0.as_ptr()
    }

    pub fn into_ref(self) -> CfRef {
        self.0
    }
}

pub fn array(values: &[*const c_void]) -> CfRef {
    // SAFETY: values are valid CF objects; the array retains them.
    unsafe {
        CfRef::from_owned(CFArrayCreate(
            kCFAllocatorDefault,
            values.as_ptr(),
            values.len() as CFIndex,
            &raw const kCFTypeArrayCallBacks as _,
        ))
        .expect("CFArrayCreate")
    }
}

/// Human-readable OSStatus (four-char code when printable).
pub fn os_status(code: OSStatus) -> String {
    let b = (code as u32).to_be_bytes();
    if b.iter().all(|c| c.is_ascii_graphic()) {
        format!("{code} ('{}')", String::from_utf8_lossy(&b))
    } else {
        format!("{code}")
    }
}

pub fn check(code: OSStatus, what: &str) -> Result<(), String> {
    if code == 0 { Ok(()) } else { Err(format!("{what} failed: {}", os_status(code))) }
}
