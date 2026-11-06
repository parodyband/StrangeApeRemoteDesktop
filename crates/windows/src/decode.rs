//! Video decoding into Direct3D 11 textures the present shader reads.
//!
//! * NVIDIA: NVDEC (CUDA), the decoded surface copied GPU-to-GPU into `R8` /
//!   `R8G8` textures registered with CUDA (D3D11 interop).
//! * Others (AMD, Intel, ...): a Media Foundation decoder MFT given our D3D11
//!   device (DXVA hardware decoding); its NV12 output is copied into a texture
//!   of our own that has shader views.
//!
//! Only codecs the GPU decodes in hardware are advertised ([`hardware_decoders`]).

#![allow(non_upper_case_globals)]

use crate::com::{self, Interface};
use crate::com_interface;
use crate::d3d::{self, Device, ID3D11ShaderResourceView, ID3D11Texture2D};
use crate::mf::{self, Mft};
use crate::nv::cuda::{CUarray, CUgraphicsResource, Cuda, Mem};
use crate::nv::nvdec::{MappedFrame, NvDecoder};
use crate::sys::{self, BOOL, GUID, HRESULT};
use sard_core::proto::Codec;
use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;

com_interface! {
    pub struct ID3D11VideoDevice(0x10ec4d5b_975a_4689_b9e4_d0aac30fe333);
    11 fn GetVideoDecoderProfileCount() -> u32;
    12 fn GetVideoDecoderProfile(index: u32, profile: *mut GUID) -> HRESULT;
    13 fn CheckVideoDecoderFormat(profile: *const GUID, format: u32, supported: *mut BOOL) -> HRESULT;
}

pub const D3D11_DECODER_PROFILE_H264_VLD_NOFGT: GUID = GUID::from_u128(0x1b81be68_a0c7_11d3_b984_00c04f2e73c5);
pub const D3D11_DECODER_PROFILE_HEVC_VLD_MAIN: GUID = GUID::from_u128(0x5b11d51b_2f4c_4452_bcc3_09f2a1160cc0);
pub const D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0: GUID = GUID::from_u128(0xb8be4ccb_cf53_46ba_8d59_d6b8a6da5d2a);

pub fn mf_subtype(c: Codec) -> GUID {
    match c {
        Codec::H264 => mf::MFVideoFormat_H264,
        Codec::Hevc => mf::MFVideoFormat_HEVC,
        Codec::Av1 => mf::MFVideoFormat_AV1,
    }
}

fn decoder_profile(c: Codec) -> GUID {
    match c {
        Codec::H264 => D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
        Codec::Hevc => D3D11_DECODER_PROFILE_HEVC_VLD_MAIN,
        Codec::Av1 => D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0,
    }
}

/// Whether the GPU behind `dev` has a DXVA decoder for `c` with NV12 output.
pub fn dxva_supports(dev: &Device, c: Codec) -> bool {
    let Ok(vd) = dev.device.cast::<ID3D11VideoDevice>() else { return false };
    let want = decoder_profile(c);
    // SAFETY: plain queries with valid out pointers.
    unsafe {
        let n = vd.GetVideoDecoderProfileCount();
        for i in 0..n {
            let mut g = GUID::default();
            if vd.GetVideoDecoderProfile(i, &mut g) >= 0 && g == want {
                let mut ok: BOOL = 0;
                return vd.CheckVideoDecoderFormat(&want, d3d::DXGI_FORMAT_NV12, &mut ok) >= 0 && ok != 0;
            }
        }
    }
    false
}

/// Which decode path a device would use, and the codecs it decodes in hardware.
#[derive(Clone, Debug)]
pub struct DecodeCaps {
    pub mask: u8,
    pub path: &'static str,
    pub notes: Vec<String>,
}

/// Codecs decodable in hardware on the preferred adapter (NVDEC on NVIDIA,
/// else DXVA through Media Foundation).
pub fn hardware_decoders(dev: &Device, nvidia: bool, adapter: *mut c_void) -> DecodeCaps {
    let mut notes = Vec::new();
    if nvidia {
        match Cuda::new(adapter) {
            Ok(cuda) => {
                let mut mask = 0;
                for c in [Codec::Hevc, Codec::H264, Codec::Av1] {
                    match NvDecoder::new(Arc::clone(&cuda), c) {
                        Ok(_) => mask |= c.bit(),
                        Err(e) => notes.push(format!("NVDEC {}: {e}", c.name())),
                    }
                }
                if mask != 0 {
                    return DecodeCaps { mask, path: "NVDEC", notes };
                }
            }
            Err(e) => notes.push(format!("NVDEC unavailable: {e}")),
        }
    }
    let mut mask = 0;
    for c in [Codec::Hevc, Codec::H264, Codec::Av1] {
        if !dxva_supports(dev, c) {
            notes.push(format!("no DXVA {} decoder on this GPU", c.name()));
            continue;
        }
        let flags = mf::MFT_ENUM_FLAG_SYNCMFT
            | mf::MFT_ENUM_FLAG_ASYNCMFT
            | mf::MFT_ENUM_FLAG_HARDWARE
            | mf::MFT_ENUM_FLAG_SORTANDFILTER;
        match mf::enumerate(mf::MFT_CATEGORY_VIDEO_DECODER, flags, Some(mf_subtype(c)), Some(mf::MFVideoFormat_NV12)) {
            Ok(v) if !v.is_empty() => mask |= c.bit(),
            Ok(_) => notes.push(format!(
                "no Media Foundation {} decoder installed{}",
                c.name(),
                match c {
                    Codec::Hevc => " (install \"HEVC Video Extensions\" from the Microsoft Store)",
                    Codec::Av1 => " (install \"AV1 Video Extension\" from the Microsoft Store)",
                    Codec::H264 => "",
                }
            )),
            Err(e) => notes.push(e),
        }
    }
    DecodeCaps { mask, path: "Media Foundation (DXVA)", notes }
}

/// Planes of the latest decoded frame, ready for [`crate::gpu::Passes::present`].
pub struct Planes<'a> {
    pub srvs: Vec<*mut c_void>,
    pub width: u32,
    pub height: u32,
    pub planar444: bool,
    pub full_range: bool,
    _p: std::marker::PhantomData<&'a ()>,
}

/// Textures written by CUDA from NVDEC surfaces.
struct CudaTextures {
    w: u32,
    h: u32,
    yuv444: bool,
    tex: Vec<ID3D11Texture2D>,
    srv: Vec<ID3D11ShaderResourceView>,
    res: Vec<CUgraphicsResource>,
}

struct NvPath {
    cuda: Arc<Cuda>,
    dec: NvDecoder,
    tex: Option<CudaTextures>,
    full_range: bool,
}

struct MfPath {
    mft: Mft,
    _manager: mf::IMFDXGIDeviceManager,
    /// Our copy of the latest frame (NV12 with shader views).
    frame: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, ID3D11ShaderResourceView, u32, u32)>,
    full_range: bool,
    /// Visible size from the current output type.
    visible: (u32, u32),
    frames: i64,
}

enum Path {
    Nv(NvPath),
    Mf(MfPath),
}

pub struct Decoder {
    pub codec: Codec,
    path: Path,
    pub has_frame: bool,
}

// SAFETY: used from the render thread only after creation; COM objects are free-threaded.
unsafe impl Send for Decoder {}

impl Decoder {
    /// NVDEC when `cuda` is given, else a Media Foundation hardware decoder on `dev`.
    pub fn new(dev: &Device, codec: Codec, cuda: Option<&Arc<Cuda>>) -> Result<Decoder, String> {
        let path = match cuda {
            Some(cuda) => Path::Nv(NvPath {
                cuda: Arc::clone(cuda),
                dec: NvDecoder::new(Arc::clone(cuda), codec)?,
                tex: None,
                full_range: false,
            }),
            None => Path::Mf(open_mf(dev, codec)?),
        };
        Ok(Decoder { codec, path, has_frame: false })
    }

    pub fn describe(&self) -> String {
        match &self.path {
            Path::Nv(p) => format!("NVDEC on {}", p.cuda.device_name),
            Path::Mf(p) => {
                format!("Media Foundation {} ({})", p.mft.name, if p.mft.hardware { "hardware" } else { "DXVA" })
            }
        }
    }

    /// Decode one access unit. Returns true if a new frame is ready.
    pub fn decode(&mut self, dev: &Device, data: &[u8]) -> Result<bool, String> {
        let got = match &mut self.path {
            Path::Nv(p) => {
                let mut err = None;
                let mut got = false;
                let (cuda, tex, full) = (&p.cuda, &mut p.tex, &mut p.full_range);
                p.dec.decode(data, 0, |f| {
                    got = true;
                    *full = f.full_range;
                    if let Err(e) = upload_cuda(dev, cuda, tex, f) {
                        err = Some(e);
                    }
                })?;
                if let Some(e) = err {
                    return Err(e);
                }
                got
            }
            Path::Mf(p) => decode_mf(dev, p, data)?,
        };
        self.has_frame |= got;
        Ok(got)
    }

    pub fn planes(&self) -> Option<Planes<'_>> {
        if !self.has_frame {
            return None;
        }
        match &self.path {
            Path::Nv(p) => {
                let t = p.tex.as_ref()?;
                Some(Planes {
                    srvs: t.srv.iter().map(|s| s.as_raw()).collect(),
                    width: t.w,
                    height: t.h,
                    planar444: t.yuv444,
                    full_range: p.full_range,
                    _p: Default::default(),
                })
            }
            Path::Mf(p) => {
                let (_, y, uv, w, h) = p.frame.as_ref()?;
                Some(Planes {
                    srvs: vec![y.as_raw(), uv.as_raw()],
                    width: *w,
                    height: *h,
                    planar444: false,
                    full_range: p.full_range,
                    _p: Default::default(),
                })
            }
        }
    }

    /// The latest frame as packed RGB (snapshots).
    pub fn snapshot_rgb(&self, dev: &Device) -> Result<(u32, u32, Vec<u8>), String> {
        match &self.path {
            Path::Nv(p) => {
                let t = p.tex.as_ref().ok_or("no frame decoded yet")?;
                if t.yuv444 {
                    return Err("snapshots of 4:4:4 video are not supported".into());
                }
                let (w, h) = (t.w as usize, t.h as usize);
                let y = read_texture(dev, &t.tex[0], w, h, 1)?;
                let uv = read_texture(dev, &t.tex[1], w / 2, h / 2, 2)?;
                let mut rgb = vec![0u8; w * h * 3];
                crate::gpu::nv12_to_rgb(&y, &uv, w, h, w, p.full_range, &mut rgb);
                Ok((t.w, t.h, rgb))
            }
            Path::Mf(p) => {
                let (tex, _, _, w, h) = p.frame.as_ref().ok_or("no frame decoded yet")?;
                let (w, h) = (*w as usize, *h as usize);
                let (y, uv, pitch) = read_nv12(dev, tex, w, h)?;
                let mut rgb = vec![0u8; w * h * 3];
                crate::gpu::nv12_to_rgb(&y, &uv, w, h, pitch, p.full_range, &mut rgb);
                Ok((w as u32, h as u32, rgb))
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Path::Nv(p) = &mut self.path
            && let Some(t) = p.tex.take()
        {
            let _g = p.cuda.push();
            for r in t.res {
                // SAFETY: registered by us.
                unsafe { (p.cuda.api.cuGraphicsUnregisterResource)(r) };
            }
        }
    }
}

fn upload_cuda(dev: &Device, cuda: &Cuda, tex: &mut Option<CudaTextures>, f: &MappedFrame) -> Result<(), String> {
    let (w, h) = (f.width, f.height);
    if tex.as_ref().is_none_or(|t| t.w != w || t.h != h || t.yuv444 != f.yuv444) {
        if let Some(old) = tex.take() {
            for r in old.res {
                // SAFETY: registered by us; context is current (NvDecoder::decode pushed it).
                unsafe { (cuda.api.cuGraphicsUnregisterResource)(r) };
            }
        }
        let mk = |w: u32, h: u32, fmt: u32| {
            dev.texture(
                &d3d::D3D11_TEXTURE2D_DESC::new(
                    w,
                    h,
                    fmt,
                    d3d::D3D11_USAGE_DEFAULT,
                    d3d::D3D11_BIND_SHADER_RESOURCE,
                    0,
                ),
                None,
            )
        };
        let specs: Vec<(u32, u32, u32)> = if f.yuv444 {
            vec![(w, h, d3d::DXGI_FORMAT_R8_UNORM); 3]
        } else {
            vec![(w, h, d3d::DXGI_FORMAT_R8_UNORM), (w / 2, h / 2, d3d::DXGI_FORMAT_R8G8_UNORM)]
        };
        let mut t = CudaTextures { w, h, yuv444: f.yuv444, tex: Vec::new(), srv: Vec::new(), res: Vec::new() };
        for (tw, th, fmt) in specs {
            let x = mk(tw, th, fmt)?;
            t.srv.push(dev.srv(&x, fmt)?);
            t.res.push(cuda.register_d3d11(x.as_raw())?);
            t.tex.push(x);
        }
        *tex = Some(t);
    }
    let t = tex.as_mut().unwrap();
    // SAFETY: resources registered in this context; arrays valid while mapped.
    unsafe {
        let n = t.res.len() as u32;
        cuda.check((cuda.api.cuGraphicsMapResources)(n, t.res.as_mut_ptr(), std::ptr::null_mut()), "map")?;
        let mut arrays: Vec<CUarray> = Vec::new();
        let mut res = Ok(());
        for r in &t.res {
            let mut a = std::ptr::null_mut();
            res = res.and(cuda.check((cuda.api.cuGraphicsSubResourceGetMappedArray)(&mut a, *r, 0, 0), "mapped array"));
            arrays.push(a);
        }
        let plane = f.pitch as u64 * ((f.surface_height as u64 + 1) & !1);
        let (w, h) = (f.width as usize, f.height as usize);
        let null = std::ptr::null_mut();
        if res.is_ok() {
            res = cuda.copy_2d(Mem::Device(f.dptr), f.pitch, Mem::Array(arrays[0]), 0, w, h, null);
        }
        if res.is_ok() {
            res = if t.yuv444 {
                cuda.copy_2d(Mem::Device(f.dptr + plane), f.pitch, Mem::Array(arrays[1]), 0, w, h, null).and_then(
                    |_| cuda.copy_2d(Mem::Device(f.dptr + 2 * plane), f.pitch, Mem::Array(arrays[2]), 0, w, h, null),
                )
            } else {
                cuda.copy_2d(Mem::Device(f.dptr + plane), f.pitch, Mem::Array(arrays[1]), 0, w / 2 * 2, h / 2, null)
            };
        }
        (cuda.api.cuGraphicsUnmapResources)(n, t.res.as_mut_ptr(), std::ptr::null_mut());
        res
    }
}

fn open_mf(dev: &Device, codec: Codec) -> Result<MfPath, String> {
    let manager = mf::device_manager(&dev.device)?;
    let flags = mf::MFT_ENUM_FLAG_SYNCMFT
        | mf::MFT_ENUM_FLAG_ASYNCMFT
        | mf::MFT_ENUM_FLAG_HARDWARE
        | mf::MFT_ENUM_FLAG_SORTANDFILTER;
    let list =
        mf::enumerate(mf::MFT_CATEGORY_VIDEO_DECODER, flags, Some(mf_subtype(codec)), Some(mf::MFVideoFormat_NV12))?;
    if list.is_empty() {
        return Err(format!("no Media Foundation {} decoder is installed", codec.name()));
    }
    let mut errors = Vec::new();
    for info in &list {
        match setup_mf(&manager, info, codec) {
            Ok(mft) => {
                sard_core::info!("decoder: {} ({})", info.name, if mft.is_async() { "async" } else { "sync" });
                return Ok(MfPath {
                    mft,
                    _manager: manager,
                    frame: None,
                    full_range: false,
                    visible: (0, 0),
                    frames: 0,
                });
            }
            Err(e) => {
                sard_core::debug!("decoder {} unusable: {e}", info.name);
                errors.push(e);
            }
        }
    }
    Err(format!("no usable {} decoder: {}", codec.name(), errors.join("; ")))
}

fn setup_mf(manager: &mf::IMFDXGIDeviceManager, info: &mf::MftInfo, codec: Codec) -> Result<Mft, String> {
    let mut mft = Mft::activate(info)?;
    mft.set_d3d_manager(manager)?;
    if let Some(api) = mft.codec_api() {
        let mut v = mf::VARIANT::u32(1);
        // SAFETY: valid GUID and VARIANT; unsupported properties are ignored.
        unsafe { api.SetValue(&mf::CODECAPI_AVLowLatencyMode, &mut v) };
    }
    let t = mf::media_type()?;
    // SAFETY: plain attribute setters on our media type.
    unsafe {
        t.SetGUID(&mf::MF_MT_MAJOR_TYPE, &mf::MFMediaType_Video);
        t.SetGUID(&mf::MF_MT_SUBTYPE, &mf_subtype(codec));
        t.SetUINT64(&mf::MF_MT_FRAME_SIZE, mf::pack(1920, 1080));
        t.SetUINT32(&mf::MF_MT_INTERLACE_MODE, mf::MFVideoInterlace_Progressive);
    }
    mft.set_input_type(&t)?;
    mft.select_output(&mf::MFVideoFormat_NV12)?;
    mft.start()?;
    Ok(mft)
}

fn output_geometry(p: &mut MfPath) {
    let Ok(t) = p.mft.current_output() else { return };
    let mut size = 0u64;
    let mut range = 0u32;
    // SAFETY: valid out pointers.
    unsafe {
        t.GetUINT64(&mf::MF_MT_FRAME_SIZE, &mut size);
        t.GetUINT32(&mf::MF_MT_VIDEO_NOMINAL_RANGE, &mut range);
    }
    let (mut w, mut h) = ((size >> 32) as u32, size as u32);
    if let Some(b) = mf::blob(&t, &mf::MF_MT_MINIMUM_DISPLAY_APERTURE)
        && b.len() >= 16
    {
        let cx = i32::from_le_bytes(b[8..12].try_into().unwrap());
        let cy = i32::from_le_bytes(b[12..16].try_into().unwrap());
        if cx > 0 && cy > 0 {
            (w, h) = (cx as u32, cy as u32);
        }
    }
    p.visible = (w & !1, h & !1);
    p.full_range = range == mf::MFNominalRange_0_255;
}

fn decode_mf(dev: &Device, p: &mut MfPath, data: &[u8]) -> Result<bool, String> {
    // 100 ns units; only the order matters (no B-frames).
    p.frames += 1;
    let sample = mf::memory_sample(data, p.frames * 166_667)?;
    if let Err(e) = p.mft.input(&sample, Duration::from_millis(500)) {
        // A sync decoder with output still pending: collect it, then retry once.
        if !e.contains("0xC00D36B5") {
            return Err(e);
        }
        drain_mf(dev, p)?;
        p.mft.input(&sample, Duration::from_millis(500))?;
    }
    drain_mf(dev, p)
}

fn drain_mf(dev: &Device, p: &mut MfPath) -> Result<bool, String> {
    let mut got = false;
    // Hardware decoders produce the frame right away in low-latency mode; give
    // an async one a few ms to announce it.
    let wait = if p.mft.is_async() { Duration::from_millis(20) } else { Duration::ZERO };
    for _ in 0..8 {
        match p.mft.output(if got { Duration::ZERO } else { wait })? {
            mf::Output::Sample(s) => {
                if p.visible == (0, 0) {
                    output_geometry(p);
                }
                copy_mf_frame(dev, p, &s)?;
                got = true;
            }
            mf::Output::StreamChange => {
                p.mft.select_output(&mf::MFVideoFormat_NV12)?;
                output_geometry(p);
                p.frame = None;
            }
            mf::Output::NeedInput => break,
        }
    }
    Ok(got)
}

fn copy_mf_frame(dev: &Device, p: &mut MfPath, s: &mf::IMFSample) -> Result<(), String> {
    let buf: mf::IMFMediaBuffer = com::out(|q| unsafe { s.GetBufferByIndex(0, q) }, "GetBufferByIndex")?;
    let dxgi: mf::IMFDXGIBuffer =
        buf.cast().map_err(|_| "decoder output is not a Direct3D surface (software decode?)")?;
    let src: ID3D11Texture2D = com::out(|q| unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, q) }, "GetResource")?;
    let mut sub = 0u32;
    // SAFETY: valid out pointer.
    unsafe { dxgi.GetSubresourceIndex(&mut sub) };
    let (w, h) = p.visible;
    if w == 0 || h == 0 {
        return Err("decoder reported no frame size".into());
    }
    if p.frame.as_ref().is_none_or(|f| (f.3, f.4) != (w, h)) {
        let desc = d3d::D3D11_TEXTURE2D_DESC::new(
            w,
            h,
            d3d::DXGI_FORMAT_NV12,
            d3d::D3D11_USAGE_DEFAULT,
            d3d::D3D11_BIND_SHADER_RESOURCE,
            0,
        );
        let tex = dev.texture(&desc, None)?;
        let y = dev.srv(&tex, d3d::DXGI_FORMAT_R8_UNORM)?;
        let uv = dev.srv(&tex, d3d::DXGI_FORMAT_R8G8_UNORM)?;
        p.frame = Some((tex, y, uv, w, h));
    }
    let (dst, ..) = p.frame.as_ref().unwrap();
    let bx = d3d::D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
    // SAFETY: both textures are NV12 on this device; the box is inside both.
    unsafe { dev.context.CopySubresourceRegion(dst.as_raw(), 0, 0, 0, 0, src.as_raw(), sub, &bx) };
    Ok(())
}

/// Read a single-plane texture (R8 / R8G8) back to the CPU, tightly packed.
fn read_texture(dev: &Device, tex: &ID3D11Texture2D, w: usize, h: usize, bpp: usize) -> Result<Vec<u8>, String> {
    let mut desc = d3d::D3D11_TEXTURE2D_DESC::default();
    // SAFETY: valid out pointer.
    unsafe { tex.GetDesc(&mut desc) };
    let st = dev.texture(
        &d3d::D3D11_TEXTURE2D_DESC::new(
            desc.Width,
            desc.Height,
            desc.Format,
            d3d::D3D11_USAGE_STAGING,
            0,
            d3d::D3D11_CPU_ACCESS_READ,
        ),
        None,
    )?;
    let mut out = vec![0u8; w * h * bpp];
    // SAFETY: staging copy of a same-sized texture; mapped while read.
    unsafe {
        dev.context.CopyResource(st.as_raw(), tex.as_raw());
        let mut m = d3d::D3D11_MAPPED_SUBRESOURCE::default();
        sys::check(dev.context.Map(st.as_raw(), 0, d3d::D3D11_MAP_READ, 0, &mut m), "Map staging")?;
        for row in 0..h {
            let src = (m.pData as *const u8).add(row * m.RowPitch as usize);
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(row * w * bpp), w * bpp);
        }
        dev.context.Unmap(st.as_raw(), 0);
    }
    Ok(out)
}

/// Read an NV12 texture back: (luma, chroma, pitch) with both planes at `pitch`.
pub fn read_nv12(dev: &Device, tex: &ID3D11Texture2D, w: usize, h: usize) -> Result<(Vec<u8>, Vec<u8>, usize), String> {
    let st = dev.texture(
        &d3d::D3D11_TEXTURE2D_DESC::new(
            w as u32,
            h as u32,
            d3d::DXGI_FORMAT_NV12,
            d3d::D3D11_USAGE_STAGING,
            0,
            d3d::D3D11_CPU_ACCESS_READ,
        ),
        None,
    )?;
    // SAFETY: staging copy; mapped while read (NV12 staging: chroma follows luma at RowPitch * height).
    unsafe {
        let bx = d3d::D3D11_BOX { left: 0, top: 0, front: 0, right: w as u32, bottom: h as u32, back: 1 };
        dev.context.CopySubresourceRegion(st.as_raw(), 0, 0, 0, 0, tex.as_raw(), 0, &bx);
        let mut m = d3d::D3D11_MAPPED_SUBRESOURCE::default();
        sys::check(dev.context.Map(st.as_raw(), 0, d3d::D3D11_MAP_READ, 0, &mut m), "Map staging")?;
        let pitch = m.RowPitch as usize;
        let base = m.pData as *const u8;
        let y = std::slice::from_raw_parts(base, pitch * h).to_vec();
        let uv = std::slice::from_raw_parts(base.add(pitch * h), pitch * h / 2).to_vec();
        dev.context.Unmap(st.as_raw(), 0);
        Ok((y, uv, pitch))
    }
}
