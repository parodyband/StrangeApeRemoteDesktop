//! Windows implementation of the sard platform API (see `sard/src/platform.rs`).

use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use sard_core::media::{
    self, AudioCallback, AudioSink, AudioSource, Clipboard, InputSink, SourceOptions, Tray, TrayCallback, TrayIcons,
    VideoSink, VideoSource,
};

pub fn open_host(_opts: &SourceOptions) -> media::Result<(Box<dyn VideoSource>, Box<dyn InputSink>)> {
    Err(media::err("not yet"))
}

pub fn create_video_sink(
    _display: RawDisplayHandle,
    _window: RawWindowHandle,
    _width: u32,
    _height: u32,
) -> media::Result<Box<dyn VideoSink>> {
    Err(media::err("not yet"))
}

pub fn create_headless_sink() -> media::Result<Box<dyn VideoSink>> {
    Err(media::err("not yet"))
}

pub fn encoders() -> u8 {
    0
}

pub fn decoders() -> u8 {
    0
}

pub fn describe() -> String {
    "Windows".into()
}

pub fn capabilities() -> Vec<String> {
    Vec::new()
}

pub fn request_host_permissions() -> Vec<String> {
    Vec::new()
}

pub fn missing_host_permissions() -> Vec<&'static str> {
    Vec::new()
}

pub fn open_permission_settings(_name: &str) {}

pub fn open_audio_source(_cb: AudioCallback) -> media::Result<Box<dyn AudioSource>> {
    Err(media::err("not yet"))
}

pub fn open_audio_sink() -> media::Result<Box<dyn AudioSink>> {
    Err(media::err("not yet"))
}

pub fn open_clipboard() -> Option<Box<dyn Clipboard>> {
    None
}

pub fn create_tray(_icons: TrayIcons, _on_click: TrayCallback) -> media::Result<Box<dyn Tray>> {
    Err(media::err("not yet"))
}

pub fn prompt_text(_title: &str, _message: &str) -> Option<String> {
    None
}

pub fn show_message(_title: &str, _message: &str) {}

pub fn confirm(_title: &str, _message: &str, _action: &str) -> bool {
    false
}

pub fn choose(_title: &str, _message: &str, _buttons: &[&str]) -> Option<usize> {
    None
}

pub fn set_launch_at_login(_enabled: bool) -> media::Result<()> {
    Err(media::err("not yet"))
}

pub fn launch_at_login() -> bool {
    false
}

pub fn set_dock_visible(_visible: bool) {}
