//! macOS platform backends for sard: ScreenCaptureKit capture (video and
//! system audio), VideoToolbox encode/decode, Metal presentation, CGEvent
//! input injection, and the app shell (menu bar item, dialogs, login item).
//!
//! All framework bindings are hand-written (`cf`, `ffi`); Objective-C APIs
//! are called through `objc2::msg_send!`.

#![cfg(target_os = "macos")]

pub mod annexb;
pub mod audio;
pub mod audio_capture;
pub mod capture;
pub mod cf;
pub mod clipboard;
pub mod ffi;
pub mod input;
pub mod metal;
pub mod objc;
pub mod pattern;
pub mod platform;
pub mod shell;
pub mod sink;
pub mod test_source;
pub mod tray;
pub mod vt;
