//! Linux platform backends for sard: NVIDIA encode/decode, PipeWire capture
//! through xdg-desktop-portal, input injection, EGL/OpenGL presentation.

#![cfg(target_os = "linux")]
// FFI wrappers take raw GPU/window handles by design; callers are this app.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub mod capture;
pub mod dl;
pub mod gl;
pub mod nv;
pub mod platform;
pub mod sink;
pub mod test_source;
pub mod yuv;
