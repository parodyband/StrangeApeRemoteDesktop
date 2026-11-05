//! Minimal `dlopen` wrapper and a macro for declaring function tables.
//!
//! All GPU / desktop libraries (CUDA, NVENC, NVDEC, EGL, GL, PipeWire,
//! libwayland-egl) are loaded at runtime. That keeps the build free of
//! `-sys` crates, pkg-config and bindgen, and lets the binary start (and give
//! a clear error) on machines where a library is missing.

use std::ffi::{CStr, CString, c_void};

pub struct Library {
    handle: *mut c_void,
    name: String,
}

// SAFETY: dl handles may be used from any thread.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}

impl Library {
    /// Open the first library in `names` that loads.
    pub fn open(names: &[&str]) -> Result<Self, String> {
        let mut last = String::new();
        for n in names {
            let c = CString::new(*n).unwrap();
            // SAFETY: valid C string; flags are standard.
            let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if !h.is_null() {
                return Ok(Library { handle: h, name: n.to_string() });
            }
            last = dlerror();
        }
        Err(format!("cannot load {}: {last}", names.join(" / ")))
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
        // SAFETY: handle is valid; name is NUL-terminated.
        let p = unsafe { libc::dlsym(self.handle, name.as_ptr().cast()) };
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

fn dlerror() -> String {
    // SAFETY: dlerror returns a thread-local string or NULL.
    let e = unsafe { libc::dlerror() };
    if e.is_null() { "unknown error".into() } else { unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned() }
}

/// Declare a struct of C function pointers loaded from a shared library.
///
/// ```ignore
/// dylib! { pub struct Cuda ["libcuda.so.1"] {
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
            $( pub $f: unsafe extern "C" fn($($arg),*) $(-> $ret)?, )*
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
