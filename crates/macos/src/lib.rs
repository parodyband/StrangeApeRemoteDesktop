//! macOS platform backends for sard: ScreenCaptureKit capture, VideoToolbox
//! encode/decode, Metal presentation, CGEvent input injection.

#![cfg(target_os = "macos")]

pub mod platform;
