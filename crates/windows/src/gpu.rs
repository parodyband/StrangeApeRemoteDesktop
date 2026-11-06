//! Direct3D 11 shader passes shared by capture and presentation:
//!
//! * colour conversion RGB → NV12 (BT.709 limited range, with scaling) into
//!   the luma (`R8`) and chroma (`R8G8`) planes of an NV12 render target, the
//!   same maths as `sard-linux`'s GL conversion shader;
//! * NV12 / planar 4:4:4 → RGB for the viewer window (limited or full range).
//!
//! Shaders are compiled at run time with `d3dcompiler_47.dll` (part of
//! Windows 10 and later), once per process.

use crate::com::Interface;
use crate::d3d::{self, Device, ID3D11Buffer, ID3D11PixelShader, ID3D11SamplerState, ID3D11VertexShader};
use std::sync::OnceLock;

const COMMON: &str = r#"
struct VsOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

// One triangle covering the viewport: (0,0) (2,0) (0,2) in uv.
VsOut vs_main(uint id : SV_VertexID) {
    VsOut o;
    float2 p = float2((id << 1) & 2, id & 2);
    o.uv = p;
    o.pos = float4(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
    return o;
}

SamplerState smp : register(s0);
"#;

/// RGB -> NV12, BT.709 limited range.
const CSC: &str = r#"
Texture2D src : register(t0);
static const float3 KY = float3(0.2126, 0.7152, 0.0722);

float ps_luma(VsOut i) : SV_Target {
    float3 c = src.Sample(smp, i.uv).rgb;
    return (16.0 + 219.0 * dot(c, KY)) / 255.0;
}

// Rendered at half size: each output pixel's centre is the centre of a 2x2
// source block, so the bilinear sample averages it.
float2 ps_chroma(VsOut i) : SV_Target {
    float3 c = src.Sample(smp, i.uv).rgb;
    float y = dot(c, KY);
    return float2((128.0 + 224.0 * (c.b - y) / 1.8556) / 255.0, (128.0 + 224.0 * (c.r - y) / 1.5748) / 255.0);
}
"#;

/// YUV -> RGB for display.
const PRESENT: &str = r#"
Texture2D<float> t_y : register(t0);
Texture2D<float2> t_c : register(t1);
Texture2D<float> t_v : register(t2);
cbuffer Params : register(b0) { uint planar444; uint full_range; uint2 pad; };

float4 ps_present(VsOut i) : SV_Target {
    float y = t_y.Sample(smp, i.uv);
    float2 c = planar444 != 0 ? float2(t_c.Sample(smp, i.uv).r, t_v.Sample(smp, i.uv)) : t_c.Sample(smp, i.uv);
    if (full_range == 0) {
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        c = (c - 128.0 / 255.0) * (255.0 / 224.0);
    } else {
        c -= 0.5;
    }
    float3 rgb = float3(y + 1.5748 * c.y, y - 0.1873 * c.x - 0.4681 * c.y, y + 1.8556 * c.x);
    return float4(saturate(rgb), 1.0);
}
"#;

struct Bytecode {
    vs: Vec<u8>,
    luma: Vec<u8>,
    chroma: Vec<u8>,
    present: Vec<u8>,
}

fn bytecode() -> Result<&'static Bytecode, String> {
    static B: OnceLock<Result<Bytecode, String>> = OnceLock::new();
    B.get_or_init(|| {
        let csc = format!("{COMMON}{CSC}");
        let present = format!("{COMMON}{PRESENT}");
        Ok(Bytecode {
            vs: d3d::compile(COMMON, "vs_main", "vs_4_0")?,
            luma: d3d::compile(&csc, "ps_luma", "ps_4_0")?,
            chroma: d3d::compile(&csc, "ps_chroma", "ps_4_0")?,
            present: d3d::compile(&present, "ps_present", "ps_4_0")?,
        })
    })
    .as_ref()
    .map_err(|e| e.clone())
}

const D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST: u32 = 4;

/// Compiled pipeline objects for one device.
pub struct Passes {
    vs: ID3D11VertexShader,
    luma: ID3D11PixelShader,
    chroma: ID3D11PixelShader,
    present: ID3D11PixelShader,
    linear: ID3D11SamplerState,
    params: ID3D11Buffer,
}

impl Passes {
    pub fn new(dev: &Device) -> Result<Passes, String> {
        let b = bytecode()?;
        Ok(Passes {
            vs: dev.vertex_shader(&b.vs)?,
            luma: dev.pixel_shader(&b.luma)?,
            chroma: dev.pixel_shader(&b.chroma)?,
            present: dev.pixel_shader(&b.present)?,
            linear: dev.sampler(d3d::D3D11_FILTER_MIN_MAG_MIP_LINEAR)?,
            params: dev.constant_buffer(16)?,
        })
    }

    fn draw(
        &self,
        dev: &Device,
        ps: &ID3D11PixelShader,
        rtv: *mut std::ffi::c_void,
        vp: &d3d::D3D11_VIEWPORT,
        srvs: &[*mut std::ffi::c_void],
    ) {
        let ctx = &dev.context;
        // SAFETY: all objects belong to `dev`; arrays outlive the calls.
        unsafe {
            ctx.OMSetRenderTargets(1, &rtv, std::ptr::null_mut());
            ctx.RSSetViewports(1, vp);
            ctx.IASetInputLayout(std::ptr::null_mut());
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(self.vs.as_raw(), std::ptr::null(), 0);
            ctx.PSSetShader(ps.as_raw(), std::ptr::null(), 0);
            let s = self.linear.as_raw();
            ctx.PSSetSamplers(0, 1, &s);
            ctx.PSSetShaderResources(0, srvs.len() as u32, srvs.as_ptr());
            let cb = self.params.as_raw();
            ctx.PSSetConstantBuffers(0, 1, &cb);
            ctx.Draw(3, 0);
            // Unbind inputs so they can be written (render target, CUDA, MF) next time.
            let none = [std::ptr::null_mut(); 3];
            ctx.PSSetShaderResources(0, srvs.len() as u32, none.as_ptr());
            let no_rtv = std::ptr::null_mut();
            ctx.OMSetRenderTargets(1, &no_rtv, std::ptr::null_mut());
        }
    }

    /// Convert `src` (any RGB texture, scaled to fit) into an NV12 target of `w`x`h`.
    pub fn rgb_to_nv12(&self, dev: &Device, src: &d3d::ID3D11ShaderResourceView, dst: &Nv12Target) {
        let (w, h) = (dst.width as f32, dst.height as f32);
        let s = [src.as_raw()];
        self.draw(dev, &self.luma, dst.luma.as_raw(), &d3d::viewport(0.0, 0.0, w, h), &s);
        self.draw(dev, &self.chroma, dst.chroma.as_raw(), &d3d::viewport(0.0, 0.0, w / 2.0, h / 2.0), &s);
    }

    /// Draw YUV planes (luma, chroma[, V for planar 4:4:4]) into `rtv` within `vp`.
    pub fn present(
        &self,
        dev: &Device,
        rtv: &d3d::ID3D11RenderTargetView,
        vp: &d3d::D3D11_VIEWPORT,
        planes: &[*mut std::ffi::c_void],
        planar444: bool,
        full_range: bool,
    ) -> Result<(), String> {
        let mut params = [0u8; 16];
        params[0..4].copy_from_slice(&(planar444 as u32).to_le_bytes());
        params[4..8].copy_from_slice(&(full_range as u32).to_le_bytes());
        dev.write_buffer(&self.params, &params)?;
        let mut srvs = [std::ptr::null_mut(); 3];
        for (d, s) in srvs.iter_mut().zip(planes) {
            *d = *s;
        }
        self.draw(dev, &self.present, rtv.as_raw(), vp, &srvs);
        Ok(())
    }
}

/// An NV12 texture the encoders read, with render target views of its planes.
pub struct Nv12Target {
    pub texture: d3d::ID3D11Texture2D,
    luma: d3d::ID3D11RenderTargetView,
    chroma: d3d::ID3D11RenderTargetView,
    pub width: u32,
    pub height: u32,
}

impl Nv12Target {
    pub fn new(dev: &Device, width: u32, height: u32) -> Result<Nv12Target, String> {
        let desc = d3d::D3D11_TEXTURE2D_DESC::new(
            width,
            height,
            d3d::DXGI_FORMAT_NV12,
            d3d::D3D11_USAGE_DEFAULT,
            d3d::D3D11_BIND_RENDER_TARGET | d3d::D3D11_BIND_SHADER_RESOURCE,
            0,
        );
        let texture = dev.texture(&desc, None).map_err(|e| {
            format!("{e}: this GPU/driver cannot render to NV12 textures (needs a Direct3D 11.1 driver)")
        })?;
        let luma = dev.rtv(&texture, d3d::DXGI_FORMAT_R8_UNORM)?;
        let chroma = dev.rtv(&texture, d3d::DXGI_FORMAT_R8G8_UNORM)?;
        Ok(Nv12Target { texture, luma, chroma, width, height })
    }
}

/// BT.709 limited-range NV12 → packed RGB on the CPU (snapshots only).
pub fn nv12_to_rgb(y: &[u8], uv: &[u8], w: usize, h: usize, pitch: usize, full_range: bool, out: &mut [u8]) {
    for row in 0..h {
        for col in 0..w {
            let yy = y[row * pitch + col] as f32 / 255.0;
            let ci = (row / 2) * pitch + (col / 2) * 2;
            let (u, v) = (uv[ci] as f32 / 255.0, uv[ci + 1] as f32 / 255.0);
            let (yy, u, v) = if full_range {
                (yy, u - 0.5, v - 0.5)
            } else {
                (
                    (yy - 16.0 / 255.0) * (255.0 / 219.0),
                    (u - 128.0 / 255.0) * (255.0 / 224.0),
                    (v - 128.0 / 255.0) * (255.0 / 224.0),
                )
            };
            let px = &mut out[(row * w + col) * 3..][..3];
            px[0] = ((yy + 1.5748 * v).clamp(0.0, 1.0) * 255.0).round() as u8;
            px[1] = ((yy - 0.1873 * u - 0.4681 * v).clamp(0.0, 1.0) * 255.0).round() as u8;
            px[2] = ((yy + 1.8556 * u).clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
}
