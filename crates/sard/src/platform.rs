#![allow(dead_code)] // app-shell functions are wired up incrementally
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
//! fn request_host_permissions() -> Vec<String>;   // prompts; returns how-to-fix text
//! fn missing_host_permissions() -> Vec<&'static str>;  // check only: short names
//! fn open_permission_settings(name: &str);           // macOS: open that privacy pane
//! fn open_audio_source(AudioCallback) -> Result<Box<dyn AudioSource>>;
//! fn open_audio_sink() -> Result<Box<dyn AudioSink>>;
//! fn open_clipboard() -> Option<Box<dyn Clipboard>>;
//! fn create_headless_sink() -> Result<Box<dyn VideoSink>>;
//!
//! // App shell (tray / menu bar):
//! fn create_tray(TrayIcons, TrayCallback) -> Result<Box<dyn Tray>>;
//!     // Must be called on the main thread after the winit event loop started
//!     // (inside `resumed`). The callback may fire on any thread.
//! fn prompt_text(title: &str, message: &str) -> Option<String>;  // modal text entry; None = cancelled
//! fn show_message(title: &str, message: &str);                   // modal notice
//! fn set_launch_at_login(enabled: bool) -> Result<()>;
//! fn launch_at_login() -> bool;
//! fn set_dock_visible(visible: bool);  // macOS: Dock icon while a viewer window is open (no-op on Linux)
//! ```

#[cfg(target_os = "linux")]
pub use sard_linux::platform::*;
#[cfg(target_os = "macos")]
pub use sard_macos::platform::*;
