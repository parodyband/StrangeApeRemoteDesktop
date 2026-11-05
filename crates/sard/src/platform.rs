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

// Audio on macOS is not implemented yet; these stubs keep the build working.
#[cfg(target_os = "macos")]
pub fn open_audio_source(
    _cb: sard_core::media::AudioCallback,
) -> sard_core::media::Result<Box<dyn sard_core::media::AudioSource>> {
    Err(sard_core::media::err("audio capture is not implemented on macOS yet"))
}

#[cfg(target_os = "macos")]
pub fn create_headless_sink() -> sard_core::media::Result<Box<dyn sard_core::media::VideoSink>> {
    Err(sard_core::media::err("--headless is not implemented on macOS yet"))
}

#[cfg(target_os = "macos")]
pub fn open_clipboard() -> Option<Box<dyn sard_core::media::Clipboard>> {
    None
}

#[cfg(target_os = "macos")]
pub fn open_audio_sink() -> sard_core::media::Result<Box<dyn sard_core::media::AudioSink>> {
    Err(sard_core::media::err("audio playback is not implemented on macOS yet"))
}
