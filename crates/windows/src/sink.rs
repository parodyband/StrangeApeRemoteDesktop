//! Client-side video output on Windows: hardware decode (see `decode.rs`) →
//! YUV→RGB shader → flip-model swap chain, presented immediately (tearing
//! allowed when the system supports it, like swap interval 0 on Linux).

use crate::com::{self, Interface};
use crate::d3d::{self, Device, ID3D11RenderTargetView, ID3D11Texture2D, IDXGIFactory2, IDXGISwapChain1};
use crate::decode::Decoder;
use crate::gpu::Passes;
use crate::nv::cuda::Cuda;
use crate::sys;
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use sard_core::media::{self, PresentStats, VideoSink, letterbox};
use sard_core::proto::Codec;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// The GPU to decode and present on, and NVIDIA's CUDA context if it is one.
pub struct Gpu {
    pub dev: Device,
    pub name: String,
    pub cuda: Option<Arc<Cuda>>,
    pub cuda_error: Option<String>,
}

impl Gpu {
    /// The preferred adapter (NVIDIA first). `SARD_DECODER=mf` skips NVDEC.
    pub fn open() -> Result<Gpu, String> {
        let f = d3d::factory()?;
        let list = d3d::adapters(&f);
        let a = d3d::preferred_adapter(&list);
        let dev = Device::new(a.map(|a| &a.adapter))?;
        let name = a.map_or_else(|| "default GPU".to_string(), |a| a.name.clone());
        let force_mf = crate::decode::force_mf();
        let (cuda, cuda_error) = match a {
            Some(a) if a.vendor == d3d::VENDOR_NVIDIA && !force_mf => match Cuda::new(a.adapter.as_raw()) {
                Ok(c) => (Some(c), None),
                Err(e) => (None, Some(e)),
            },
            _ => (None, None),
        };
        if let Some(e) = &cuda_error {
            sard_core::warn!("NVIDIA GPU but no CUDA ({e}); decoding with Media Foundation");
        }
        Ok(Gpu { dev, name, cuda, cuda_error })
    }

    fn decoder(&self, codec: Codec) -> Result<Decoder, String> {
        match Decoder::new(&self.dev, codec, self.cuda.as_ref()) {
            Ok(d) => Ok(d),
            Err(e) if self.cuda.is_some() => {
                sard_core::warn!("NVDEC {}: {e}; trying Media Foundation", codec.name());
                Decoder::new(&self.dev, codec, None)
            }
            Err(e) => Err(e),
        }
    }
}

pub struct D3dVideoSink {
    gpu: Gpu,
    passes: Passes,
    swapchain: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    tearing: bool,
    win: (u32, u32),
    decoder: Option<Decoder>,
    snapshot: Option<PathBuf>,
}

// SAFETY: created on the UI thread, then used only by the render thread; the
// D3D11 device is free-threaded and the context is only touched there.
unsafe impl Send for D3dVideoSink {}

const BUFFERS: u32 = 2;

impl D3dVideoSink {
    pub fn new(_display: RawDisplayHandle, window: RawWindowHandle, width: u32, height: u32) -> media::Result<Self> {
        let RawWindowHandle::Win32(h) = window else {
            return Err(media::err(format!("unsupported window handle {window:?}")));
        };
        let hwnd = h.hwnd.get() as sys::HWND;
        let gpu = Gpu::open()?;
        let passes = Passes::new(&gpu.dev)?;
        let adapter = gpu.dev.adapter()?;
        let factory: IDXGIFactory2 =
            com::out(|p| unsafe { adapter.GetParent(&IDXGIFactory2::IID, p) }, "IDXGIAdapter::GetParent")?;
        let tearing = factory.cast::<d3d::IDXGIFactory5>().is_ok_and(|f5| {
            let mut allow: i32 = 0;
            // SAFETY: BOOL out-parameter of the stated size.
            unsafe {
                f5.CheckFeatureSupport(d3d::DXGI_FEATURE_PRESENT_ALLOW_TEARING, (&mut allow as *mut i32).cast(), 4) >= 0
                    && allow != 0
            }
        });
        let desc = d3d::DXGI_SWAP_CHAIN_DESC1 {
            Width: width.max(1),
            Height: height.max(1),
            Format: d3d::DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: 0,
            SampleDesc: d3d::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: d3d::DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: BUFFERS,
            Scaling: d3d::DXGI_SCALING_STRETCH,
            SwapEffect: d3d::DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: d3d::DXGI_ALPHA_MODE_IGNORE,
            Flags: if tearing { d3d::DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING } else { 0 },
        };
        let swapchain: IDXGISwapChain1 = com::out(
            |p| unsafe {
                factory.CreateSwapChainForHwnd(
                    gpu.dev.device.as_raw(),
                    hwnd,
                    &desc,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    p,
                )
            },
            "CreateSwapChainForHwnd",
        )?;
        // SAFETY: plain calls on live objects.
        unsafe {
            // Alt+Enter belongs to the remote machine; fullscreen is winit's borderless window.
            factory.MakeWindowAssociation(hwnd, d3d::DXGI_MWA_NO_ALT_ENTER);
            if let Ok(d1) = gpu.dev.device.cast::<d3d::IDXGIDevice1>() {
                d1.SetMaximumFrameLatency(1);
            }
        }
        sard_core::info!(
            "Direct3D 11 on {} (feature level {:x}), flip model{}",
            gpu.name,
            gpu.dev.feature_level,
            if tearing { ", tearing allowed" } else { "" }
        );
        let mut s = D3dVideoSink {
            gpu,
            passes,
            swapchain,
            rtv: None,
            tearing,
            win: (width.max(1), height.max(1)),
            decoder: None,
            snapshot: None,
        };
        s.make_rtv()?;
        Ok(s)
    }

    fn make_rtv(&mut self) -> Result<(), String> {
        let back: ID3D11Texture2D =
            com::out(|p| unsafe { self.swapchain.GetBuffer(0, &ID3D11Texture2D::IID, p) }, "GetBuffer")?;
        self.rtv = Some(com::out(
            |p| unsafe { self.gpu.dev.device.CreateRenderTargetView(back.as_raw(), std::ptr::null(), p) },
            "CreateRenderTargetView",
        )?);
        Ok(())
    }

    fn draw(&mut self) -> Result<(), String> {
        let Some(rtv) = &self.rtv else { return Ok(()) };
        let dev = &self.gpu.dev;
        let (ww, wh) = self.win;
        let black = [0.0f32, 0.0, 0.0, 1.0];
        // SAFETY: live render target view.
        unsafe { dev.context.ClearRenderTargetView(rtv.as_raw(), black.as_ptr()) };
        if let Some(p) = self.decoder.as_ref().and_then(|d| d.planes()) {
            let (x, y, w, h) = letterbox(ww, wh, p.width, p.height);
            let vp = d3d::viewport(x.round(), y.round(), w.round(), h.round());
            self.passes.present(dev, rtv, &vp, &p.srvs, p.planar444, p.full_range)?;
        }
        let flags = if self.tearing { d3d::DXGI_PRESENT_ALLOW_TEARING } else { 0 };
        // SAFETY: live swap chain.
        let hr = unsafe { self.swapchain.Present(0, flags) };
        if hr == d3d::DXGI_ERROR_DEVICE_REMOVED || hr == d3d::DXGI_ERROR_DEVICE_RESET {
            // SAFETY: plain query.
            let why = unsafe { dev.device.GetDeviceRemovedReason() };
            return Err(sys::hr_error("GPU device lost (driver update or reset?)", why));
        }
        sys::check(hr, "Present")
    }
}

impl VideoSink for D3dVideoSink {
    fn present(&mut self, codec: Codec, data: &[u8], _keyframe: bool) -> media::Result<PresentStats> {
        if self.decoder.as_ref().is_none_or(|d| d.codec != codec) {
            self.decoder = None;
            let d = self.gpu.decoder(codec)?;
            sard_core::info!("decoding {} with {}", codec.name(), d.describe());
            self.decoder = Some(d);
        }
        let t0 = Instant::now();
        let got = self.decoder.as_mut().unwrap().decode(&self.gpu.dev, data)?;
        let decode_us = t0.elapsed().as_micros() as u32;
        if !got {
            return Ok(PresentStats { decode_us, present_us: 0 });
        }
        if let Some(path) = self.snapshot.take() {
            save_snapshot(&self.gpu.dev, self.decoder.as_ref().unwrap(), &path);
        }
        let t1 = Instant::now();
        self.draw()?;
        Ok(PresentStats { decode_us, present_us: t1.elapsed().as_micros() as u32 })
    }

    fn resize(&mut self, width: u32, height: u32) {
        let (w, h) = (width.max(1), height.max(1));
        if (w, h) == self.win {
            return;
        }
        self.win = (w, h);
        self.rtv = None;
        // SAFETY: every reference to the back buffers was dropped above.
        let flags = if self.tearing { d3d::DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING } else { 0 };
        let hr = unsafe { self.swapchain.ResizeBuffers(0, w, h, d3d::DXGI_FORMAT_UNKNOWN, flags) };
        if hr < 0 {
            sard_core::warn!("{}", sys::hr_error("ResizeBuffers", hr));
        }
        if let Err(e) = self.make_rtv() {
            sard_core::warn!("{e}");
        }
    }

    fn redraw(&mut self) -> media::Result<()> {
        if self.decoder.as_ref().is_some_and(|d| d.has_frame) {
            self.draw()?;
        }
        Ok(())
    }

    fn snapshot_next(&mut self, path: &std::path::Path) -> media::Result<()> {
        self.snapshot = Some(path.to_path_buf());
        Ok(())
    }

    fn describe(&self) -> String {
        match &self.decoder {
            Some(d) => format!("{} → Direct3D 11", d.describe()),
            None => format!("Direct3D 11 on {}", self.gpu.name),
        }
    }
}

fn save_snapshot(dev: &Device, d: &Decoder, path: &std::path::Path) {
    match d.snapshot_rgb(dev).and_then(|(w, h, rgb)| {
        sard_core::png::write_rgb(path, w, h, &rgb).map_err(|e| format!("{}: {e}", path.display()))
    }) {
        Ok(()) => sard_core::info!("saved snapshot {}", path.display()),
        Err(e) => sard_core::warn!("snapshot failed: {e}"),
    }
}

/// Decode-only client output (`sard connect --headless`): no window.
pub struct HeadlessSink {
    gpu: Gpu,
    decoder: Option<Decoder>,
    snapshot: Option<PathBuf>,
}

// SAFETY: as for D3dVideoSink.
unsafe impl Send for HeadlessSink {}

impl HeadlessSink {
    pub fn new() -> media::Result<Self> {
        Ok(HeadlessSink { gpu: Gpu::open()?, decoder: None, snapshot: None })
    }
}

impl VideoSink for HeadlessSink {
    fn present(&mut self, codec: Codec, data: &[u8], _keyframe: bool) -> media::Result<PresentStats> {
        if self.decoder.as_ref().is_none_or(|d| d.codec != codec) {
            self.decoder = None;
            let d = self.gpu.decoder(codec)?;
            sard_core::info!("decoding {} with {}", codec.name(), d.describe());
            self.decoder = Some(d);
        }
        let t0 = Instant::now();
        let got = self.decoder.as_mut().unwrap().decode(&self.gpu.dev, data)?;
        if got && let Some(path) = self.snapshot.take() {
            save_snapshot(&self.gpu.dev, self.decoder.as_ref().unwrap(), &path);
        }
        Ok(PresentStats { decode_us: t0.elapsed().as_micros() as u32, present_us: 0 })
    }

    fn resize(&mut self, _width: u32, _height: u32) {}

    fn snapshot_next(&mut self, path: &std::path::Path) -> media::Result<()> {
        self.snapshot = Some(path.to_path_buf());
        Ok(())
    }

    fn describe(&self) -> String {
        match &self.decoder {
            Some(d) => format!("{} (headless)", d.describe()),
            None => format!("{} (headless)", self.gpu.name),
        }
    }
}
