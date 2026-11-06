//! Minimal COM support: an owning interface pointer and a macro that
//! declares typed wrappers for the vtable slots this crate calls.
//!
//! Every interface is declared with the absolute vtable index of each method
//! (IUnknown's three methods are slots 0-2), checked against the SDK headers
//! by `tools/winabi/check.py`:
//!
//! ```ignore
//! com_interface! {
//!     pub struct ID3D11Device(0xdb6f6ddb_ac77_4e88_8253_819df9bbf140);
//!     5 fn CreateTexture2D(desc: *const D3D11_TEXTURE2D_DESC, init: *const c_void, out: *mut *mut c_void) -> HRESULT;
//! }
//! ```

use crate::sys::{GUID, HRESULT};
use std::ffi::c_void;
use std::ptr::NonNull;

/// An owned reference to a COM object (Release on drop, AddRef on clone).
#[repr(transparent)]
pub struct Com(NonNull<c_void>);

// SAFETY: every interface used here is free-threaded or only used from one
// thread at a time by its owner (D3D11 device and context access is serialized
// by the code that owns them).
unsafe impl Send for Com {}
unsafe impl Sync for Com {}

type FnAddRef = unsafe extern "system" fn(*mut c_void) -> u32;
type FnQi = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;

impl Com {
    /// Take ownership of one reference. `None` for a null pointer.
    ///
    /// # Safety
    /// `p` must be null or a valid COM interface pointer whose reference the caller owns.
    pub unsafe fn from_raw(p: *mut c_void) -> Option<Com> {
        NonNull::new(p).map(Com)
    }

    pub fn as_raw(&self) -> *mut c_void {
        self.0.as_ptr()
    }

    /// Give up ownership without releasing.
    pub fn into_raw(self) -> *mut c_void {
        let p = self.0.as_ptr();
        std::mem::forget(self);
        p
    }

    /// The function in vtable slot `index`.
    ///
    /// # Safety
    /// `F` must be the slot's exact function pointer type and the slot must exist.
    #[inline]
    pub unsafe fn vfn<F: Copy>(&self, index: usize) -> F {
        debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<usize>());
        // SAFETY: a COM object starts with a pointer to its vtable of function pointers.
        unsafe {
            let vtbl = *(self.0.as_ptr() as *const *const usize);
            std::mem::transmute_copy::<usize, F>(&*vtbl.add(index))
        }
    }

    /// `QueryInterface` for another interface of the same object.
    pub fn query<T: Interface>(&self) -> Result<T, HRESULT> {
        let mut out = std::ptr::null_mut();
        // SAFETY: slot 0 of every COM vtable is QueryInterface.
        let hr = unsafe { self.vfn::<FnQi>(0)(self.as_raw(), &T::IID, &mut out) };
        if hr < 0 {
            return Err(hr);
        }
        // SAFETY: QueryInterface returned an owned reference.
        unsafe { Com::from_raw(out) }.map(T::from_com).ok_or(crate::sys::E_FAIL)
    }
}

impl Clone for Com {
    fn clone(&self) -> Self {
        // SAFETY: slot 1 is AddRef.
        unsafe { self.vfn::<FnAddRef>(1)(self.as_raw()) };
        Com(self.0)
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: slot 2 is Release; we own one reference.
        unsafe { self.vfn::<FnAddRef>(2)(self.as_raw()) };
    }
}

impl std::fmt::Debug for Com {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Com({:p})", self.0)
    }
}

/// A typed COM interface wrapper.
pub trait Interface: Sized {
    const IID: GUID;
    fn from_com(c: Com) -> Self;
    fn com(&self) -> &Com;
    fn as_raw(&self) -> *mut c_void {
        self.com().as_raw()
    }
    /// QueryInterface to another interface.
    fn cast<T: Interface>(&self) -> Result<T, HRESULT> {
        self.com().query::<T>()
    }
    /// Take ownership of a raw out-pointer filled by a COM call.
    ///
    /// # Safety
    /// As for [`Com::from_raw`], and `p` must implement this interface.
    unsafe fn from_raw(p: *mut c_void) -> Option<Self> {
        // SAFETY: forwarded.
        unsafe { Com::from_raw(p) }.map(Self::from_com)
    }
}

/// Call a COM creation function that fills `*mut *mut c_void`, check the
/// HRESULT and wrap the result: `out(|p| unsafe { dev.CreateTexture2D(&desc, null(), p) }, "CreateTexture2D")`.
pub fn out<T: Interface>(f: impl FnOnce(*mut *mut c_void) -> HRESULT, what: &str) -> Result<T, String> {
    let mut p = std::ptr::null_mut();
    let hr = f(&mut p);
    crate::sys::check(hr, what)?;
    // SAFETY: the call succeeded and returned an owned reference of the requested interface.
    unsafe { T::from_raw(p) }.ok_or_else(|| format!("{what} returned no object"))
}

#[macro_export]
macro_rules! com_interface {
    (
        $(#[$m:meta])*
        $vis:vis struct $name:ident($iid:expr);
        $( $idx:literal fn $method:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?; )*
    ) => {
        $(#[$m])*
        #[derive(Clone, Debug)]
        #[repr(transparent)]
        $vis struct $name(pub $crate::com::Com);

        impl $crate::com::Interface for $name {
            const IID: $crate::sys::GUID = $crate::sys::GUID::from_u128($iid);
            fn from_com(c: $crate::com::Com) -> Self {
                $name(c)
            }
            fn com(&self) -> &$crate::com::Com {
                &self.0
            }
        }

        #[allow(
            non_snake_case,
            dead_code,
            clippy::too_many_arguments,
            clippy::missing_safety_doc,
            clippy::macro_metavars_in_unsafe
        )]
        impl $name {
            $(
                #[inline]
                pub unsafe fn $method(&self, $($arg: $ty),*) $(-> $ret)? {
                    // SAFETY: slot index and signature are checked against the SDK headers;
                    // the caller upholds the method's own contract.
                    unsafe {
                        let f: unsafe extern "system" fn(*mut ::std::ffi::c_void, $($ty),*) $(-> $ret)? =
                            self.0.vfn($idx);
                        f(self.0.as_raw(), $($arg),*)
                    }
                }
            )*
        }
    };
}
