//! Screen capture with DXGI Desktop Duplication, GPU colour conversion and
//! hardware encode.
//!
//! ```text
//! IDXGIOutputDuplication frame (BGRA) ─▶ copy ─▶ RGB→NV12 shader (BT.709, scale)
//!   ─▶ NV12 texture ─▶ NVENC (D3D11 input) or Media Foundation encoder ─▶ bitstream
//! ```
//!
//! The cursor is not part of the duplicated image: its shape and position come
//! from the duplication's pointer metadata and go to the viewer separately
//! (`CursorMode::Metadata`). Every output of every GPU is offered as a
//! display; the D3D11 device (and encoder) live on the GPU driving the chosen
//! one, as Desktop Duplication requires.
//!
//! Not captured (Windows doesn't allow it for a normal user process): the
//! secure desktop (UAC prompts, Ctrl+Alt+Del, the lock screen); the stream
//! pauses there and resumes when the normal desktop returns.

#![allow(non_snake_case, non_camel_case_types, clippy::upper_case_acronyms)]

use crate::com::{self, Interface};
use crate::d3d::{
    self, Adapter, Device, ID3D11ShaderResourceView, ID3D11Texture2D, IDXGIOutput1, IDXGIOutputDuplication,
};
use crate::encode::{Encoder, Settings};
use crate::gpu::{Nv12Target, Passes};
use crate::input::{InputTarget, SendInputSink};
use crate::sys::{self, RECT};
use sard_core::clock;
use sard_core::media::{
    self, CursorMode, CursorShape, CursorUpdate, DisplayInfo, EncodedFrame, InputSink, SourceOptions, VideoSource,
};
use sard_core::proto::Codec;
use sard_core::{debug, info, warn};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One monitor as DXGI sees it.
#[derive(Clone)]
pub struct Screen {
    pub id: u32,
    pub adapter: Adapter,
    pub output: IDXGIOutput1,
    pub name: String,
    /// `\\.\DISPLAY1`
    pub device_name: String,
    /// Desktop coordinates (physical pixels).
    pub rect: RECT,
    pub rotation: u32,
}

impl Screen {
    pub fn width(&self) -> u32 {
        (self.rect.right - self.rect.left).max(0) as u32
    }
    pub fn height(&self) -> u32 {
        (self.rect.bottom - self.rect.top).max(0) as u32
    }
    pub fn primary(&self) -> bool {
        self.rect.left == 0 && self.rect.top == 0
    }
}

/// Every desktop-attached output of every hardware adapter.
pub fn screens() -> Result<Vec<Screen>, String> {
    d3d::set_dpi_aware();
    let f = d3d::factory()?;
    let names = friendly_names();
    let mut v = Vec::new();
    for a in d3d::adapters(&f) {
        for i in 0.. {
            let o: IDXGIOutput1 =
                match com::out::<d3d::IDXGIOutput>(|p| unsafe { a.adapter.EnumOutputs(i, p) }, "EnumOutputs") {
                    Ok(c) => match c.cast() {
                        Ok(o) => o,
                        Err(_) => continue,
                    },
                    Err(_) => break,
                };
            // SAFETY: zeroed POD out-struct.
            let mut d: d3d::DXGI_OUTPUT_DESC = unsafe { std::mem::zeroed() };
            if unsafe { o.GetDesc(&mut d) } < 0 || d.AttachedToDesktop == 0 {
                continue;
            }
            let device_name = sys::from_wide(&d.DeviceName);
            let id = v.len() as u32 + 1;
            let name = names
                .iter()
                .find(|(dev, _)| *dev == device_name)
                .map(|(_, n)| n.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("Display {id}"));
            v.push(Screen {
                id,
                adapter: a.clone(),
                output: o,
                name,
                device_name,
                rect: d.DesktopCoordinates,
                rotation: d.Rotation,
            });
        }
    }
    // Same model twice: tell them apart.
    let dup: Vec<String> =
        v.iter().filter(|s| v.iter().filter(|t| t.name == s.name).count() > 1).map(|s| s.name.clone()).collect();
    for s in v.iter_mut().filter(|s| dup.contains(&s.name)) {
        s.name = format!("{} ({})", s.name, s.device_name.trim_start_matches(r"\\.\"));
    }
    Ok(v)
}

fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let s = (max_w as f64 / w as f64).min(max_h as f64 / h as f64).min(1.0);
    ((((w as f64 * s) as u32) & !1).max(2), (((h as f64 * s) as u32) & !1).max(2))
}

/// The union of all screens (the virtual desktop `SendInput` normalizes to).
pub fn virtual_desktop(screens: &[Screen]) -> RECT {
    let mut r = RECT { left: i32::MAX, top: i32::MAX, right: i32::MIN, bottom: i32::MIN };
    for s in screens {
        r.left = r.left.min(s.rect.left);
        r.top = r.top.min(s.rect.top);
        r.right = r.right.max(s.rect.right);
        r.bottom = r.bottom.max(s.rect.bottom);
    }
    if r.left > r.right { RECT::default() } else { r }
}

// ---------------------------------------------------------------------------
// Monitor names (QueryDisplayConfig: EDID names like "DELL U2720Q")

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DISPLAYCONFIG_RATIONAL {
    Numerator: u32,
    Denominator: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DISPLAYCONFIG_PATH_SOURCE_INFO {
    adapterId: d3d::LUID,
    id: u32,
    modeInfoIdx: u32,
    statusFlags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DISPLAYCONFIG_PATH_TARGET_INFO {
    adapterId: d3d::LUID,
    id: u32,
    modeInfoIdx: u32,
    outputTechnology: u32,
    rotation: u32,
    scaling: u32,
    refreshRate: DISPLAYCONFIG_RATIONAL,
    scanLineOrdering: u32,
    targetAvailable: i32,
    statusFlags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DISPLAYCONFIG_PATH_INFO {
    sourceInfo: DISPLAYCONFIG_PATH_SOURCE_INFO,
    targetInfo: DISPLAYCONFIG_PATH_TARGET_INFO,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DISPLAYCONFIG_MODE_INFO {
    infoType: u32,
    id: u32,
    adapterId: d3d::LUID,
    u: [u64; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DISPLAYCONFIG_DEVICE_INFO_HEADER {
    r#type: u32,
    size: u32,
    adapterId: d3d::LUID,
    id: u32,
}

#[repr(C)]
struct DISPLAYCONFIG_SOURCE_DEVICE_NAME {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    viewGdiDeviceName: [u16; 32],
}

#[repr(C)]
struct DISPLAYCONFIG_TARGET_DEVICE_NAME {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    flags: u32,
    outputTechnology: u32,
    edidManufactureId: u16,
    edidProductCodeId: u16,
    connectorInstance: u32,
    monitorFriendlyDeviceName: [u16; 64],
    monitorDevicePath: [u16; 128],
}

const _: () = {
    use std::mem::size_of;
    assert!(size_of::<DISPLAYCONFIG_PATH_INFO>() == 72);
    assert!(size_of::<DISPLAYCONFIG_MODE_INFO>() == 64);
    assert!(size_of::<DISPLAYCONFIG_DEVICE_INFO_HEADER>() == 20);
    assert!(size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() == 84);
    assert!(size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() == 420);
};

const QDC_ONLY_ACTIVE_PATHS: u32 = 2;
const DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME: u32 = 1;
const DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME: u32 = 2;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetDisplayConfigBufferSizes(flags: u32, paths: *mut u32, modes: *mut u32) -> i32;
    fn QueryDisplayConfig(
        flags: u32,
        npaths: *mut u32,
        paths: *mut DISPLAYCONFIG_PATH_INFO,
        nmodes: *mut u32,
        modes: *mut DISPLAYCONFIG_MODE_INFO,
        topology: *mut u32,
    ) -> i32;
    fn DisplayConfigGetDeviceInfo(packet: *mut DISPLAYCONFIG_DEVICE_INFO_HEADER) -> i32;
}

/// (`\\.\DISPLAYn`, monitor name) for the active displays.
fn friendly_names() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let (mut np, mut nm) = (0u32, 0u32);
    // SAFETY: valid out pointers; buffers sized from the first call.
    unsafe {
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != 0 {
            return out;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        let r = QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            std::ptr::null_mut(),
        );
        if r != 0 {
            return out;
        }
        for p in &paths[..np as usize] {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: p.sourceInfo.adapterId,
                    id: p.sourceInfo.id,
                },
                viewGdiDeviceName: [0; 32],
            };
            if DisplayConfigGetDeviceInfo(&mut src.header) != 0 {
                continue;
            }
            let mut tgt: DISPLAYCONFIG_TARGET_DEVICE_NAME = std::mem::zeroed();
            tgt.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: p.targetInfo.adapterId,
                id: p.targetInfo.id,
            };
            let name = if DisplayConfigGetDeviceInfo(&mut tgt.header) == 0 {
                sys::from_wide(&tgt.monitorFriendlyDeviceName)
            } else {
                String::new()
            };
            out.push((sys::from_wide(&src.viewGdiDeviceName), name));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Cursor

/// Convert a DXGI pointer shape to straight-alpha RGBA. XOR ("invert the
/// screen") pixels, which RGBA can't express, become black with a white edge.
fn pointer_rgba(info: &d3d::DXGI_OUTDUPL_POINTER_SHAPE_INFO, buf: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let (w, pitch) = (info.Width as usize, info.Pitch as usize);
    let mono = info.Type == d3d::DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME;
    let h = if mono { info.Height as usize / 2 } else { info.Height as usize };
    if w == 0 || h == 0 || w > 512 || h > 512 {
        return None;
    }
    let mut rgba = vec![0u8; w * h * 4];
    let mut invert = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let px = &mut rgba[(y * w + x) * 4..][..4];
            match info.Type {
                d3d::DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME => {
                    let bit = |row: usize| buf.get(row * pitch + x / 8).map_or(0, |b| (b >> (7 - x % 8)) & 1);
                    match (bit(y), bit(y + h)) {
                        (0, 0) => px.copy_from_slice(&[0, 0, 0, 255]),
                        (0, _) => px.copy_from_slice(&[255, 255, 255, 255]),
                        (_, 0) => {}
                        _ => invert[y * w + x] = true,
                    }
                }
                d3d::DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR => {
                    let s = buf.get(y * pitch + x * 4..y * pitch + x * 4 + 4)?;
                    px.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
                }
                d3d::DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR => {
                    let s = buf.get(y * pitch + x * 4..y * pitch + x * 4 + 4)?;
                    if s[3] == 0 {
                        px.copy_from_slice(&[s[2], s[1], s[0], 255]);
                    } else if s[0] | s[1] | s[2] != 0 {
                        invert[y * w + x] = true;
                    }
                }
                _ => return None,
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            if !invert[y * w + x] {
                continue;
            }
            rgba[(y * w + x) * 4..][..4].copy_from_slice(&[0, 0, 0, 255]);
            for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1), (-1, -1), (1, 1), (-1, 1), (1, -1)] {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                    continue;
                }
                let i = ny as usize * w + nx as usize;
                if !invert[i] && rgba[i * 4 + 3] == 0 {
                    rgba[i * 4..][..4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
        }
    }
    Some((w as u32, h as u32, rgba))
}

#[derive(Default)]
struct CursorTrack {
    serial: u32,
    shape: Option<CursorShape>,
    shape_dirty: bool,
    /// Top-left of the pointer image in screen pixels (DXGI reports that, not the hotspot).
    pos: (i32, i32),
    visible: bool,
    dirty: bool,
    buf: Vec<u8>,
}

// ---------------------------------------------------------------------------
// The source

struct Gpu {
    dev: Device,
    passes: Passes,
    vendor: u32,
    adapter_name: String,
    luid: (u32, i32),
}

impl Gpu {
    fn new(a: &Adapter) -> Result<Gpu, String> {
        let dev = Device::new(Some(&a.adapter))?;
        let passes = Passes::new(&dev)?;
        Ok(Gpu { dev, passes, vendor: a.vendor, adapter_name: a.name.clone(), luid: (a.luid.LowPart, a.luid.HighPart) })
    }
}

#[derive(Default)]
struct Timing {
    n: u32,
    wait_us: u64,
    convert_us: u64,
    encode_us: u64,
}

pub struct DuplSource {
    screens: Vec<Screen>,
    current: usize,
    gpu: Gpu,
    dupl: Option<IDXGIOutputDuplication>,
    next_reopen: Instant,
    reopen_failures: u32,
    desktop: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, u32, u32, u32)>,
    targets: Vec<Nv12Target>,
    next_target: usize,
    last_target: Option<usize>,
    enc: Encoder,
    opts: SourceOptions,
    bitrate_kbps: u32,
    desktop_size: (u32, u32),
    out_size: (u32, u32),
    force_key: bool,
    queue: VecDeque<EncodedFrame>,
    cursor: CursorTrack,
    target: Arc<Mutex<InputTarget>>,
    timing: Timing,
    warned_rotation: bool,
}

// SAFETY: created on the host thread, then used only by the video thread.
unsafe impl Send for DuplSource {}

const RING: usize = 4;

pub fn open(opts: &SourceOptions) -> media::Result<(Box<dyn VideoSource>, Box<dyn InputSink>)> {
    let screens = screens()?;
    if screens.is_empty() {
        return Err(media::err(
            "no displays to capture: Desktop Duplication needs a monitor (or a dummy plug / virtual display) \
             on a GPU, in the console session (not over Remote Desktop, not from a service)",
        ));
    }
    let current = if opts.display >= 1 && (opts.display as usize) <= screens.len() {
        opts.display as usize - 1
    } else {
        screens.iter().position(Screen::primary).unwrap_or(0)
    };
    for s in &screens {
        info!(
            "display {}: {} {}x{} at {},{} on {} ({})",
            s.id,
            s.name,
            s.width(),
            s.height(),
            s.rect.left,
            s.rect.top,
            s.adapter.name,
            s.adapter.vendor_name()
        );
    }
    let s = &screens[current];
    let gpu = Gpu::new(&s.adapter)?;
    let target = Arc::new(Mutex::new(InputTarget { screen: s.rect, desktop: virtual_desktop(&screens) }));
    let dupl = duplicate(&gpu.dev, &s.output).map_err(media::err)?;
    let desktop_size = dupl_size(&dupl);
    let out_size = fit(desktop_size.0, desktop_size.1, opts.max_width, opts.max_height);
    let enc = make_encoder(&gpu, opts, out_size, opts.bitrate_kbps)?;
    let targets = make_targets(&gpu.dev, out_size)?;
    info!(
        "capture pipeline: Desktop Duplication {}x{} → {}x{} NV12 → {} on {} ({})",
        desktop_size.0,
        desktop_size.1,
        out_size.0,
        out_size.1,
        enc.name,
        gpu.adapter_name,
        d3d::vendor_name(gpu.vendor)
    );
    let src = DuplSource {
        screens,
        current,
        gpu,
        dupl: Some(dupl),
        next_reopen: Instant::now(),
        reopen_failures: 0,
        desktop: None,
        targets,
        next_target: 0,
        last_target: None,
        enc,
        opts: opts.clone(),
        bitrate_kbps: opts.bitrate_kbps.max(1000),
        desktop_size,
        out_size,
        force_key: true,
        queue: VecDeque::new(),
        cursor: CursorTrack::default(),
        target: Arc::clone(&target),
        timing: Timing::default(),
        warned_rotation: false,
    };
    Ok((Box::new(src), Box::new(SendInputSink::new(target))))
}

fn make_encoder(gpu: &Gpu, opts: &SourceOptions, size: (u32, u32), kbps: u32) -> media::Result<Encoder> {
    let s = Settings {
        codec: opts.codec,
        width: size.0,
        height: size.1,
        fps: opts.fps.max(1),
        bitrate_kbps: kbps.max(1000),
        preset: opts.preset,
    };
    Ok(Encoder::new(&gpu.dev, gpu.vendor, s)?)
}

fn make_targets(dev: &Device, size: (u32, u32)) -> media::Result<Vec<Nv12Target>> {
    (0..RING).map(|_| Nv12Target::new(dev, size.0, size.1).map_err(media::err)).collect()
}

fn dupl_size(d: &IDXGIOutputDuplication) -> (u32, u32) {
    let mut desc = d3d::DXGI_OUTDUPL_DESC::default();
    // SAFETY: valid out pointer.
    unsafe { d.GetDesc(&mut desc) };
    (desc.ModeDesc.Width, desc.ModeDesc.Height)
}

/// Start duplicating `output` on `dev` (which must be on the output's GPU).
/// Asks for 8-bit BGRA, so HDR desktops are tone-mapped by Windows.
pub fn duplicate(dev: &Device, output: &IDXGIOutput1) -> Result<IDXGIOutputDuplication, String> {
    if let Ok(o5) = output.cast::<d3d::IDXGIOutput5>() {
        let formats = [d3d::DXGI_FORMAT_B8G8R8A8_UNORM];
        let r = com::out(
            |p| unsafe { o5.DuplicateOutput1(dev.device.as_raw(), 0, formats.len() as u32, formats.as_ptr(), p) },
            "DuplicateOutput1",
        );
        match r {
            Ok(d) => return Ok(d),
            Err(e) => debug!("{e}; trying DuplicateOutput"),
        }
    }
    let mut p = std::ptr::null_mut();
    // SAFETY: valid device and out pointer.
    let hr = unsafe { output.DuplicateOutput(dev.device.as_raw(), &mut p) };
    if hr < 0 {
        let hint = match hr {
            d3d::DXGI_ERROR_NOT_CURRENTLY_AVAILABLE => {
                " (too many apps are duplicating this screen; close other capture/streaming tools)"
            }
            d3d::DXGI_ERROR_UNSUPPORTED => {
                " (unsupported: Remote Desktop session, or a GPU without duplication support)"
            }
            d3d::E_ACCESSDENIED | d3d::DXGI_ERROR_ACCESS_DENIED => {
                " (access denied: the secure desktop is showing — UAC prompt, lock screen — or sard runs as a service)"
            }
            d3d::DXGI_ERROR_SESSION_DISCONNECTED => " (the session is disconnected)",
            _ => "",
        };
        return Err(format!("{}{hint}", sys::hr_error("DuplicateOutput", hr)));
    }
    // SAFETY: owned reference.
    unsafe { IDXGIOutputDuplication::from_raw(p) }.ok_or_else(|| "DuplicateOutput returned nothing".into())
}

enum Acquired {
    Timeout,
    Lost(String),
    Frame { image: bool, capture_us: u64 },
}

impl DuplSource {
    fn reopen(&mut self) -> bool {
        if Instant::now() < self.next_reopen {
            return false;
        }
        let s = &self.screens[self.current];
        match duplicate(&self.gpu.dev, &s.output) {
            Ok(d) => {
                if self.reopen_failures > 0 {
                    info!("screen capture resumed");
                }
                self.reopen_failures = 0;
                let size = dupl_size(&d);
                self.dupl = Some(d);
                if size != self.desktop_size {
                    info!("desktop size changed to {}x{}", size.0, size.1);
                    self.desktop_size = size;
                    self.desktop = None;
                    let out = fit(size.0, size.1, self.opts.max_width, self.opts.max_height);
                    if out != self.out_size
                        && let Err(e) = self.resize_output(out)
                    {
                        warn!("cannot follow the new desktop size: {e}");
                    }
                }
                self.force_key = true;
                true
            }
            Err(e) => {
                if self.reopen_failures == 0 {
                    warn!("screen capture paused: {e}");
                }
                self.reopen_failures += 1;
                self.next_reopen = Instant::now() + Duration::from_millis(250);
                false
            }
        }
    }

    fn resize_output(&mut self, out: (u32, u32)) -> media::Result<()> {
        self.enc = make_encoder(&self.gpu, &self.opts, out, self.bitrate_kbps)?;
        self.targets = make_targets(&self.gpu.dev, out)?;
        self.last_target = None;
        self.out_size = out;
        Ok(())
    }

    fn acquire(&mut self, timeout_ms: u32) -> Acquired {
        let Some(d) = self.dupl.clone() else { return Acquired::Lost("no duplication".into()) };
        let mut info = d3d::DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res = std::ptr::null_mut();
        // SAFETY: valid out pointers.
        let hr = unsafe { d.AcquireNextFrame(timeout_ms, &mut info, &mut res) };
        if hr == d3d::DXGI_ERROR_WAIT_TIMEOUT {
            return Acquired::Timeout;
        }
        if hr < 0 {
            return Acquired::Lost(sys::hr_error("AcquireNextFrame", hr));
        }
        // SAFETY: owned reference to the desktop image (released with the frame).
        let resource = unsafe { com::Com::from_raw(res) };
        self.update_cursor(&d, &info);
        let image = info.LastPresentTime != 0 || info.AccumulatedFrames > 0;
        let now = clock::now_us();
        let capture_us = if info.LastPresentTime > 0 {
            let t = clock::qpc_to_ns(info.LastPresentTime as u64) / 1000;
            if t <= now && now - t < 200_000 { t } else { now }
        } else {
            now
        };
        let mut r = Acquired::Frame { image, capture_us };
        if image
            && let Some(tex) = resource.as_ref().and_then(|c| c.query::<ID3D11Texture2D>().ok())
            && let Err(e) = self.copy_desktop(&tex)
        {
            r = Acquired::Lost(e);
        }
        drop(resource);
        // SAFETY: matching release of the acquired frame.
        unsafe { d.ReleaseFrame() };
        r
    }

    fn copy_desktop(&mut self, tex: &ID3D11Texture2D) -> Result<(), String> {
        let mut desc = d3d::D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid out pointer.
        unsafe { tex.GetDesc(&mut desc) };
        if self.desktop.as_ref().is_none_or(|d| (d.2, d.3, d.4) != (desc.Width, desc.Height, desc.Format)) {
            let copy = self.gpu.dev.texture(
                &d3d::D3D11_TEXTURE2D_DESC::new(
                    desc.Width,
                    desc.Height,
                    desc.Format,
                    d3d::D3D11_USAGE_DEFAULT,
                    d3d::D3D11_BIND_SHADER_RESOURCE,
                    0,
                ),
                None,
            )?;
            let srv = self.gpu.dev.srv(&copy, desc.Format)?;
            debug!("desktop image {}x{} format {}", desc.Width, desc.Height, desc.Format);
            self.desktop = Some((copy, srv, desc.Width, desc.Height, desc.Format));
        }
        let (copy, ..) = self.desktop.as_ref().unwrap();
        // SAFETY: same size and format.
        unsafe { self.gpu.dev.context.CopyResource(copy.as_raw(), tex.as_raw()) };
        Ok(())
    }

    fn update_cursor(&mut self, d: &IDXGIOutputDuplication, info: &d3d::DXGI_OUTDUPL_FRAME_INFO) {
        let c = &mut self.cursor;
        if info.PointerShapeBufferSize > 0 {
            c.buf.resize(info.PointerShapeBufferSize as usize, 0);
            let mut need = 0u32;
            let mut si = d3d::DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            // SAFETY: buffer of the stated size.
            let hr =
                unsafe { d.GetFramePointerShape(c.buf.len() as u32, c.buf.as_mut_ptr().cast(), &mut need, &mut si) };
            if hr >= 0
                && let Some((w, h, rgba)) = pointer_rgba(&si, &c.buf[..need as usize])
            {
                c.serial = c.serial.wrapping_add(1);
                c.shape = Some(CursorShape {
                    serial: c.serial,
                    hot_x: si.HotSpot.x.clamp(0, w as i32 - 1) as u16,
                    hot_y: si.HotSpot.y.clamp(0, h as i32 - 1) as u16,
                    width: w as u16,
                    height: h as u16,
                    rgba,
                });
                c.shape_dirty = true;
                c.dirty = true;
            }
        }
        if info.LastMouseUpdateTime != 0 {
            let p = info.PointerPosition;
            let visible = p.Visible != 0;
            if visible != c.visible || (visible && (p.Position.x, p.Position.y) != c.pos) {
                c.visible = visible;
                if visible {
                    c.pos = (p.Position.x, p.Position.y);
                }
                c.dirty = true;
            }
        }
    }

    fn encode(&mut self, capture_us: u64, reuse: bool) -> media::Result<()> {
        let t0 = Instant::now();
        let idx = if reuse {
            match self.last_target {
                Some(i) => i,
                None => return Ok(()),
            }
        } else {
            let Some((_, srv, ..)) = &self.desktop else { return Ok(()) };
            let i = self.next_target;
            self.next_target = (self.next_target + 1) % self.targets.len();
            self.gpu.passes.rgb_to_nv12(&self.gpu.dev, srv, &self.targets[i]);
            i
        };
        let t1 = Instant::now();
        let key = std::mem::take(&mut self.force_key);
        let packets = self.enc.encode(&self.targets[idx].texture, capture_us, key)?;
        self.last_target = Some(idx);
        for p in packets {
            self.queue.push_back(EncodedFrame {
                data: p.data,
                keyframe: p.keyframe,
                capture_us: if p.timestamp_us != 0 { p.timestamp_us } else { capture_us },
                width: self.out_size.0,
                height: self.out_size.1,
            });
        }
        let t = &mut self.timing;
        t.n += 1;
        t.wait_us += clock::now_us().saturating_sub(capture_us);
        t.convert_us += (t1 - t0).as_micros() as u64;
        t.encode_us += t1.elapsed().as_micros() as u64;
        if t.n == 300 {
            let n = t.n as f64 * 1000.0;
            debug!(
                "capture stages (avg ms): present→done {:.2} | convert {:.2} | encode {:.2}",
                t.wait_us as f64 / n,
                t.convert_us as f64 / n,
                t.encode_us as f64 / n
            );
            *t = Timing::default();
        }
        Ok(())
    }
}

impl VideoSource for DuplSource {
    fn next_frame(&mut self, timeout: Duration) -> media::Result<Option<EncodedFrame>> {
        if let Some(f) = self.queue.pop_front() {
            return Ok(Some(f));
        }
        let deadline = Instant::now() + timeout;
        loop {
            if self.dupl.is_none() && !self.reopen() {
                if self.force_key && self.last_target.is_some() {
                    // Paused (secure desktop): the viewer still gets a decodable picture.
                    self.encode(clock::now_us(), true)?;
                    return Ok(self.queue.pop_front());
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(None);
                }
                std::thread::sleep(left.min(Duration::from_millis(20)));
                continue;
            }
            let left = deadline.saturating_duration_since(Instant::now()).as_millis() as u32;
            match self.acquire(left) {
                Acquired::Timeout => {
                    if self.force_key && self.last_target.is_some() {
                        // Static screen: re-encode the last picture as a keyframe.
                        self.encode(clock::now_us(), true)?;
                        return Ok(self.queue.pop_front());
                    }
                    return Ok(None);
                }
                Acquired::Lost(e) => {
                    debug!("duplication lost: {e}");
                    self.dupl = None;
                    self.desktop = None;
                    self.force_key = true;
                }
                Acquired::Frame { image, capture_us } => {
                    if self.screens[self.current].rotation > d3d::DXGI_MODE_ROTATION_IDENTITY && !self.warned_rotation {
                        warn!("this display is rotated; the stream shows it unrotated (not supported yet)");
                        self.warned_rotation = true;
                    }
                    if image {
                        self.encode(capture_us, false)?;
                        return Ok(self.queue.pop_front());
                    }
                    // Pointer-only update: let the caller send the cursor now.
                    return Ok(None);
                }
            }
        }
    }

    fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.bitrate_kbps = kbps;
        self.enc.set_bitrate(kbps);
    }

    fn codec(&self) -> Codec {
        self.opts.codec
    }

    fn size(&self) -> (u32, u32) {
        self.out_size
    }

    fn cursor_mode(&self) -> CursorMode {
        CursorMode::Metadata
    }

    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        let c = &mut self.cursor;
        if !c.dirty {
            return None;
        }
        c.dirty = false;
        let (hx, hy) = c.shape.as_ref().map_or((0, 0), |s| (s.hot_x as i32, s.hot_y as i32));
        let (w, h) = self.desktop_size;
        Some(CursorUpdate {
            x: (c.pos.0 + hx) as f32 / w.max(1) as f32,
            y: (c.pos.1 + hy) as f32 / h.max(1) as f32,
            visible: c.visible,
            shape: if std::mem::take(&mut c.shape_dirty) { c.shape.clone() } else { None },
        })
    }

    fn describe(&self) -> String {
        format!(
            "screen {}x{} → {}x{} {} {} on {}",
            self.desktop_size.0,
            self.desktop_size.1,
            self.out_size.0,
            self.out_size.1,
            self.enc.name,
            self.opts.codec.name(),
            self.gpu.adapter_name
        )
    }

    fn displays(&self) -> Vec<DisplayInfo> {
        self.screens
            .iter()
            .map(|s| DisplayInfo {
                id: s.id,
                name: s.name.clone(),
                width: s.width(),
                height: s.height(),
                x: s.rect.left,
                y: s.rect.top,
            })
            .collect()
    }

    fn current_display(&self) -> Option<u32> {
        Some(self.screens[self.current].id)
    }

    fn select_display(&mut self, id: u32) -> media::Result<()> {
        let idx = self
            .screens
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| media::err(format!("no display with id {id}")))?;
        if idx == self.current {
            self.force_key = true;
            return Ok(());
        }
        let s = self.screens[idx].clone();
        let same_gpu = (s.adapter.luid.LowPart, s.adapter.luid.HighPart) == self.gpu.luid;
        // Set up the new screen first so a failure leaves the old one running.
        let gpu = if same_gpu { None } else { Some(Gpu::new(&s.adapter)?) };
        let dev = gpu.as_ref().map_or(&self.gpu.dev, |g| &g.dev);
        let dupl = duplicate(dev, &s.output)?;
        let size = dupl_size(&dupl);
        let out = fit(size.0, size.1, self.opts.max_width, self.opts.max_height);
        self.dupl = None;
        self.desktop = None;
        if let Some(g) = gpu {
            self.gpu = g;
            self.resize_output(out)?;
        } else if out != self.out_size {
            self.resize_output(out)?;
        }
        self.dupl = Some(dupl);
        self.desktop_size = size;
        self.current = idx;
        self.target.lock().unwrap().screen = s.rect;
        self.cursor.dirty = true;
        self.cursor.shape_dirty = self.cursor.shape.is_some();
        self.force_key = true;
        info!("switched to display {} ({}x{} → {}x{}, {})", s.name, size.0, size.1, out.0, out.1, self.enc.name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monochrome_pointer() {
        // 2x1 pointer: pixel 0 black (AND 0, XOR 0), pixel 1 transparent (AND 1, XOR 0).
        let info = d3d::DXGI_OUTDUPL_POINTER_SHAPE_INFO {
            Type: d3d::DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
            Width: 2,
            Height: 2,
            Pitch: 1,
            HotSpot: sys::POINT { x: 0, y: 0 },
        };
        let (w, h, rgba) = pointer_rgba(&info, &[0b0100_0000, 0b0000_0000]).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(&rgba[..4], &[0, 0, 0, 255]);
        assert_eq!(rgba[7], 0);
    }

    #[test]
    fn fit_keeps_even_sizes() {
        assert_eq!(fit(2560, 1440, 7680, 4320), (2560, 1440));
        assert_eq!(fit(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(fit(1366, 768, 7680, 4320), (1366, 768));
    }
}
