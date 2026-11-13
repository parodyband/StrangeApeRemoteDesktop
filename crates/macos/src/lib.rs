//! macOS platform backends for sard: ScreenCaptureKit capture (video and
//! system audio), VideoToolbox encode/decode, Metal presentation, CGEvent
//! input injection, and the app shell (menu bar item, dialogs, login item).
//!
//! All framework bindings are hand-written (`cf`, `ffi`); Objective-C APIs
//! are called through `objc2::msg_send!`.

//!
//! On iOS only the client half builds (VideoToolbox decode, Metal
//! presentation): the iPhone app (`sard-mobile`) reuses it.

#![cfg(target_vendor = "apple")]

pub mod annexb;
#[cfg(target_os = "macos")]
pub mod audio;
#[cfg(target_os = "macos")]
pub mod audio_capture;
#[cfg(target_os = "macos")]
pub mod capture;
pub mod cf;
#[cfg(target_os = "macos")]
pub mod clipboard;
#[cfg(target_os = "macos")]
pub mod cursor;
#[cfg(target_os = "macos")]
pub mod display_mode;
pub mod ffi;
#[cfg(target_os = "macos")]
pub mod input;
pub mod metal;
pub mod objc;
#[cfg(target_os = "macos")]
pub mod pattern;
#[cfg(target_os = "macos")]
pub mod platform;
#[cfg(target_os = "macos")]
pub mod shell;
#[cfg(target_os = "macos")]
pub mod sink;
#[cfg(target_os = "macos")]
pub mod test_source;
#[cfg(target_os = "macos")]
pub mod tray;
pub mod vt;
