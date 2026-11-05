//! Linux platform backends for sard: NVIDIA encode/decode, PipeWire capture
//! through xdg-desktop-portal, input injection, EGL/OpenGL presentation.

#![cfg(target_os = "linux")]

pub mod dl;
pub mod nv;
pub mod yuv;
