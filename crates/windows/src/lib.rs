//! Windows platform backends for sard: DXGI Desktop Duplication capture,
//! NVENC / NVDEC (NVIDIA) or Media Foundation hardware codecs (AMD, Intel,
//! others), Direct3D 11 presentation, SendInput injection, WASAPI audio and
//! the app shell (notification-area icon, dialogs, start at login).
//!
//! All bindings are hand-written: Win32 in `sys`, COM interfaces declared by
//! vtable slot with `com_interface!`, GPU/media DLLs loaded at run time
//! (`dl`). `tools/winabi/check.py` checks slots, IIDs, struct sizes and
//! constants against the Windows SDK headers.

#![cfg(target_os = "windows")]
// FFI wrappers take raw GPU/window handles by design; callers are this app.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub mod audio;
pub mod capture;
pub mod clipboard;
pub mod com;
pub mod d3d;
pub mod decode;
pub mod dl;
pub mod encode;
pub mod gpu;
pub mod input;
pub mod mf;
pub mod nv;
pub mod os;
pub mod platform;
pub mod shell;
pub mod sink;
pub mod sys;
pub mod test_source;
pub mod tray;
