//! Windows implementation of the sard platform API (see `sard/src/platform.rs`).

use crate::d3d::{self, Device};
use crate::encode::{Encoder, Settings};
use crate::input::LogInput;
use crate::sys;
use crate::test_source::TestPatternSource;
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use sard_core::media::{
    self, AudioCallback, AudioSink, AudioSource, Clipboard, InputSink, SourceOptions, Tray, TrayCallback, TrayIcons,
    VideoSink, VideoSource,
};
use sard_core::proto::Codec;
use std::sync::{Arc, OnceLock};

const ALL: [Codec; 3] = [Codec::Hevc, Codec::H264, Codec::Av1];

pub fn open_host(opts: &SourceOptions) -> media::Result<(Box<dyn VideoSource>, Box<dyn InputSink>)> {
    if opts.test_pattern {
        let src = TestPatternSource::new(opts)?;
        let input = LogInput { pointer: Arc::clone(&src.pointer) };
        return Ok((Box::new(src), Box::new(input)));
    }
    crate::capture::open(opts)
}

/// Called on the main thread right after the window is created.
pub fn create_video_sink(
    display: RawDisplayHandle,
    window: RawWindowHandle,
    width: u32,
    height: u32,
) -> media::Result<Box<dyn VideoSink>> {
    Ok(Box::new(crate::sink::D3dVideoSink::new(display, window, width, height)?))
}

/// Decode-only client output (`sard connect --headless`).
pub fn create_headless_sink() -> media::Result<Box<dyn VideoSink>> {
    Ok(Box::new(crate::sink::HeadlessSink::new()?))
}

/// Nothing to pre-authorize on Windows. (Windows Firewall asks whether sard may
/// receive connections the first time it listens.)
pub fn request_host_permissions() -> Vec<String> {
    Vec::new()
}

pub fn missing_host_permissions() -> Vec<&'static str> {
    Vec::new()
}

pub fn open_permission_settings(_name: &str) {}

/// Capture what this machine plays (WASAPI loopback of the default output).
pub fn open_audio_source(cb: AudioCallback) -> media::Result<Box<dyn AudioSource>> {
    Ok(Box::new(crate::audio::LoopbackSource::new(cb)?))
}

/// Play remote audio through the default output device.
pub fn open_audio_sink() -> media::Result<Box<dyn AudioSink>> {
    Ok(Box::new(crate::audio::WasapiSink::new()?))
}

pub fn open_clipboard() -> Option<Box<dyn Clipboard>> {
    Some(Box::new(crate::clipboard::WinClipboard::open()?))
}

// ---- app shell: `tray.rs` / `shell.rs` ----

/// Notification-area icon. Call on winit's main thread (inside `resumed`);
/// the callback runs on that thread.
pub fn create_tray(icons: TrayIcons, on_click: TrayCallback) -> media::Result<Box<dyn Tray>> {
    Ok(Box::new(crate::tray::WinTray::new(icons, on_click)?))
}

pub fn prompt_text(title: &str, message: &str) -> Option<String> {
    crate::shell::prompt_text(title, message)
}

pub fn show_message(title: &str, message: &str) {
    crate::shell::show_message(title, message)
}

pub fn confirm(title: &str, message: &str, action: &str) -> bool {
    crate::shell::confirm(title, message, action)
}

pub fn choose(title: &str, message: &str, buttons: &[&str]) -> Option<usize> {
    crate::shell::choose(title, message, buttons)
}

pub fn set_launch_at_login(enabled: bool) -> media::Result<()> {
    crate::shell::set_launch_at_login(enabled)
}

pub fn launch_at_login() -> bool {
    crate::shell::launch_at_login()
}

/// No Dock on Windows; viewer windows have their own taskbar buttons.
pub fn set_dock_visible(_visible: bool) {}

// ---- capabilities ----

struct Caps {
    os: String,
    gpu: String,
    vendor: u32,
    adapters: Vec<String>,
    enc: u8,
    enc_path: String,
    enc_notes: Vec<String>,
    dec: u8,
    dec_path: &'static str,
    dec_notes: Vec<String>,
    checks: Vec<String>,
}

#[repr(C)]
#[allow(non_snake_case, clippy::upper_case_acronyms)]
struct OSVERSIONINFOW {
    dwOSVersionInfoSize: u32,
    dwMajorVersion: u32,
    dwMinorVersion: u32,
    dwBuildNumber: u32,
    dwPlatformId: u32,
    szCSDVersion: [u16; 128],
}

/// "Windows 11 (10.0.22631)" — RtlGetVersion isn't subject to manifest-based version lies.
fn os_version() -> String {
    type FnRtlGetVersion = unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32;
    let ntdll = sys::wide("ntdll.dll");
    // SAFETY: ntdll is loaded in every process; RtlGetVersion has this signature.
    unsafe {
        let m = sys::GetModuleHandleW(ntdll.as_ptr());
        let p = sys::GetProcAddress(m, c"RtlGetVersion".as_ptr().cast());
        if p.is_null() {
            return "Windows".into();
        }
        let f: FnRtlGetVersion = std::mem::transmute::<*mut std::ffi::c_void, FnRtlGetVersion>(p);
        let mut v: OSVERSIONINFOW = std::mem::zeroed();
        v.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
        f(&mut v);
        let name = if v.dwMajorVersion == 10 && v.dwBuildNumber >= 22000 {
            "11".to_string()
        } else {
            v.dwMajorVersion.to_string()
        };
        format!("Windows {name} ({}.{}.{})", v.dwMajorVersion, v.dwMinorVersion, v.dwBuildNumber)
    }
}

fn names(mask: u8) -> String {
    let v: Vec<&str> = ALL.iter().filter(|c| mask & c.bit() != 0).map(|c| c.name()).collect();
    if v.is_empty() { "none".into() } else { v.join(", ") }
}

fn caps() -> &'static Caps {
    static C: OnceLock<Caps> = OnceLock::new();
    C.get_or_init(|| {
        let os = os_version();
        let mut c = Caps {
            os,
            gpu: "no GPU".into(),
            vendor: 0,
            adapters: Vec::new(),
            enc: 0,
            enc_path: "none".into(),
            enc_notes: Vec::new(),
            dec: 0,
            dec_path: "none",
            dec_notes: Vec::new(),
            checks: Vec::new(),
        };
        let f = match d3d::factory() {
            Ok(f) => f,
            Err(e) => {
                c.checks.push(format!("DXGI unavailable: {e}"));
                return c;
            }
        };
        let list = d3d::adapters(&f);
        for a in &list {
            c.adapters.push(format!(
                "{} ({}, {} MB{})",
                a.name,
                a.vendor_name(),
                a.dedicated_mb,
                if a.software { ", software" } else { "" }
            ));
        }
        let Some(a) = d3d::preferred_adapter(&list) else {
            c.checks.push("no hardware GPU (only the Microsoft Basic Render Driver): sard needs a GPU".into());
            return c;
        };
        c.gpu = a.name.clone();
        c.vendor = a.vendor;
        let dev = match Device::new(Some(&a.adapter)) {
            Ok(d) => d,
            Err(e) => {
                c.checks.push(format!("Direct3D 11 device on {}: {e}", a.name));
                return c;
            }
        };
        c.checks.push(format!("Direct3D 11 feature level {:x}", dev.feature_level));
        c.checks.push(format!(
            "NV12 render targets (GPU colour conversion): {}",
            if dev.can_render_to(d3d::DXGI_FORMAT_NV12) { "yes" } else { "NO (capture won't work)" }
        ));
        match crate::gpu::Passes::new(&dev) {
            Ok(_) => c.checks.push("HLSL shaders (d3dcompiler_47.dll): ok".into()),
            Err(e) => c.checks.push(format!("HLSL shaders: {e}")),
        }
        match crate::mf::api() {
            Ok(_) => c.checks.push("Media Foundation: available".into()),
            Err(e) => c.checks.push(format!("Media Foundation: {e}")),
        }
        // Encoders: what the host would use, probed at a small size.
        let mut paths = Vec::new();
        for codec in [Codec::Hevc, Codec::H264, Codec::Av1] {
            let s = Settings { codec, width: 640, height: 360, fps: 60, bitrate_kbps: 4000, preset: 1 };
            match Encoder::new(&dev, a.vendor, s) {
                Ok(e) => {
                    c.enc |= codec.bit();
                    if !paths.contains(&e.name) {
                        paths.push(e.name.clone());
                    }
                    if let Some(r) = &e.fallback_reason
                        && !e.is_nvenc()
                        && a.vendor == d3d::VENDOR_NVIDIA
                    {
                        c.enc_notes.push(format!("{}: {r}", codec.name()));
                    }
                }
                Err(e) => c.enc_notes.push(format!("{}: {e}", codec.name())),
            }
        }
        if !paths.is_empty() {
            c.enc_path = paths.join(" / ");
        }
        let d = crate::decode::hardware_decoders(
            &dev,
            a.vendor == d3d::VENDOR_NVIDIA,
            crate::com::Interface::as_raw(&a.adapter),
        );
        c.dec = d.mask;
        c.dec_path = d.path;
        c.dec_notes = d.notes;
        c
    })
}

pub fn encoders() -> u8 {
    caps().enc
}

pub fn decoders() -> u8 {
    caps().dec
}

pub fn describe() -> String {
    let c = caps();
    format!("{}, {}", c.os, c.gpu)
}

pub fn capabilities() -> Vec<String> {
    let c = caps();
    let mut v = vec![format!("OS:      {}", c.os)];
    for (i, a) in c.adapters.iter().enumerate() {
        v.push(format!("{}{a}", if i == 0 { "GPUs:    " } else { "         " }));
    }
    v.push(format!("using:   {} ({})", c.gpu, d3d::vendor_name(c.vendor)));
    v.push(format!("encode:  {} ({})", names(c.enc), c.enc_path));
    for n in &c.enc_notes {
        v.push(format!("         {n}"));
    }
    v.push(format!("decode:  {} ({}, hardware only)", names(c.dec), c.dec_path));
    for n in &c.dec_notes {
        v.push(format!("         {n}"));
    }
    for ch in &c.checks {
        v.push(format!("check:   {ch}"));
    }
    match crate::capture::screens() {
        Ok(s) if s.is_empty() => v.push("screens: none (Remote Desktop session or no monitor?)".into()),
        Ok(s) => {
            for (i, s) in s.iter().enumerate() {
                v.push(format!(
                    "{}{}: {} {}x{} at {},{} on {}",
                    if i == 0 { "screens: " } else { "         " },
                    s.id,
                    s.name,
                    s.width(),
                    s.height(),
                    s.rect.left,
                    s.rect.top,
                    s.adapter.name
                ));
            }
        }
        Err(e) => v.push(format!("screens: {e}")),
    }
    v
}
