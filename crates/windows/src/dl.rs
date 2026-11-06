//! `LoadLibraryExW` / `GetProcAddress` wrapper and a macro for function
//! tables, mirroring `sard-linux`'s `dl` module.
//!
//! GPU and media libraries (nvcuda, nvEncodeAPI64, nvcuvid, mfplat,
//! d3dcompiler_47) are loaded at run time, so the program starts (and says
//! clearly what is missing) on machines without them: no NVIDIA driver, or a
//! Windows "N" edition without the Media Feature Pack. Only System32 is
//! searched, so a DLL planted next to the executable is never picked up.

use crate::sys;
use std::ffi::c_void;

pub struct Library {
    handle: sys::HMODULE,
    name: String,
}

// SAFETY: module handles may be used from any thread.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}

impl Library {
    /// Load the first library in `names` that loads (from System32).
    pub fn open(names: &[&str]) -> Result<Self, String> {
        let mut last = 0;
        for n in names {
            let w = sys::wide(n);
            // SAFETY: valid NUL-terminated name.
            let h = unsafe { sys::LoadLibraryExW(w.as_ptr(), std::ptr::null_mut(), sys::LOAD_LIBRARY_SEARCH_SYSTEM32) };
            if !h.is_null() {
                return Ok(Library { handle: h, name: n.to_string() });
            }
            // SAFETY: plain thread-local error query.
            last = unsafe { sys::GetLastError() };
        }
        Err(format!("cannot load {}: {} (error {last})", names.join(" / "), sys::error_message(last)))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Look up a symbol. `name` must be NUL-terminated.
    ///
    /// # Safety
    /// `T` must be a function pointer type matching the symbol's real signature.
    pub unsafe fn sym<T: Copy>(&self, name: &str) -> Result<T, String> {
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
        assert!(name.ends_with('\0'));
        // SAFETY: valid module handle; NUL-terminated name.
        let p = unsafe { sys::GetProcAddress(self.handle, name.as_ptr()) };
        if p.is_null() {
            return Err(format!("{}: missing symbol {}", self.name, name.trim_end_matches('\0')));
        }
        // SAFETY: caller guarantees T is the right fn pointer type.
        Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
    }

    /// Optional symbol lookup.
    ///
    /// # Safety
    /// As for [`Library::sym`].
    pub unsafe fn sym_opt<T: Copy>(&self, name: &str) -> Option<T> {
        // SAFETY: forwarded.
        unsafe { self.sym(name).ok() }
    }
}

/// Declare a struct of `extern "system"` function pointers loaded from a DLL.
///
/// ```ignore
/// dylib! { pub struct Cuda ["nvcuda.dll"] {
///     fn cuInit(u32) -> i32;
/// } }
/// ```
#[macro_export]
macro_rules! dylib {
    ($(#[$m:meta])* $vis:vis struct $name:ident [$($libname:expr),+ $(,)?] {
        $( fn $f:ident($($arg:ty),* $(,)?) $(-> $ret:ty)?; )*
    }) => {
        $(#[$m])*
        #[allow(non_snake_case, dead_code)]
        $vis struct $name {
            pub lib: $crate::dl::Library,
            $( pub $f: unsafe extern "system" fn($($arg),*) $(-> $ret)?, )*
        }
        impl $name {
            pub fn load() -> ::std::result::Result<Self, String> {
                let lib = $crate::dl::Library::open(&[$($libname),+])?;
                // SAFETY: signatures are declared to match the C headers.
                unsafe {
                    Ok(Self {
                        $( $f: lib.sym(concat!(stringify!($f), "\0"))?, )*
                        lib,
                    })
                }
            }
        }
    };
}
