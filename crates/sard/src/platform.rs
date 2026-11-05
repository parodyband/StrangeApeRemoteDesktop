//! Compile-time dispatch to the platform backend. Each backend crate exposes
//! the same `platform` module API:
//!
//! ```ignore
//! fn open_host(&SourceOptions) -> Result<(Box<dyn VideoSource>, Box<dyn InputSink>)>;
//! fn create_video_sink(RawDisplayHandle, RawWindowHandle, w, h) -> Result<Box<dyn VideoSink>>;
//! fn encoders() -> u8;   // Codec::bit() mask
//! fn decoders() -> u8;
//! fn describe() -> String;
//! fn capabilities() -> Vec<String>;
//! ```

#[cfg(target_os = "linux")]
pub use sard_linux::platform::*;
#[cfg(target_os = "macos")]
pub use sard_macos::platform::*;
