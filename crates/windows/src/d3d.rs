//! DXGI / Direct3D 11 declarations (the parts this crate calls), plus device
//! creation, adapter enumeration and HLSL compilation helpers.

#![allow(non_snake_case, non_camel_case_types, clippy::upper_case_acronyms, dead_code)]

use crate::com::{self, Interface};
use crate::com_interface;
use crate::sys::{self, BOOL, GUID, HMONITOR, HRESULT, HWND, POINT, RECT};
use std::ffi::c_void;

// ---------------------------------------------------------------------------
// Constants

pub const DXGI_FORMAT_UNKNOWN: u32 = 0;
pub const DXGI_FORMAT_R16G16B16A16_FLOAT: u32 = 10;
pub const DXGI_FORMAT_R10G10B10A2_UNORM: u32 = 24;
pub const DXGI_FORMAT_R8G8B8A8_UNORM: u32 = 28;
pub const DXGI_FORMAT_R8G8_UNORM: u32 = 49;
pub const DXGI_FORMAT_R8_UNORM: u32 = 61;
pub const DXGI_FORMAT_B8G8R8A8_UNORM: u32 = 87;
pub const DXGI_FORMAT_B8G8R8A8_UNORM_SRGB: u32 = 91;
pub const DXGI_FORMAT_NV12: u32 = 103;

pub const D3D11_USAGE_DEFAULT: u32 = 0;
pub const D3D11_USAGE_DYNAMIC: u32 = 2;
pub const D3D11_USAGE_STAGING: u32 = 3;
pub const D3D11_BIND_VERTEX_BUFFER: u32 = 0x1;
pub const D3D11_BIND_CONSTANT_BUFFER: u32 = 0x4;
pub const D3D11_BIND_SHADER_RESOURCE: u32 = 0x8;
pub const D3D11_BIND_RENDER_TARGET: u32 = 0x20;
pub const D3D11_BIND_DECODER: u32 = 0x200;
pub const D3D11_BIND_VIDEO_ENCODER: u32 = 0x400;
pub const D3D11_CPU_ACCESS_WRITE: u32 = 0x1_0000;
pub const D3D11_CPU_ACCESS_READ: u32 = 0x2_0000;
pub const D3D11_RESOURCE_MISC_SHARED: u32 = 0x2;
pub const D3D11_MAP_READ: u32 = 1;
pub const D3D11_MAP_WRITE_DISCARD: u32 = 4;
pub const D3D11_FILTER_MIN_MAG_MIP_POINT: u32 = 0;
pub const D3D11_FILTER_MIN_MAG_MIP_LINEAR: u32 = 0x15;
pub const D3D11_TEXTURE_ADDRESS_CLAMP: u32 = 3;
pub const D3D11_COMPARISON_NEVER: u32 = 1;
pub const D3D11_FLOAT32_MAX: f32 = 3.402_823_5e38;
pub const D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP: u32 = 5;
pub const D3D11_RTV_DIMENSION_TEXTURE2D: u32 = 4;
pub const D3D11_SRV_DIMENSION_TEXTURE2D: u32 = 4;
pub const D3D11_CREATE_DEVICE_BGRA_SUPPORT: u32 = 0x20;
pub const D3D11_CREATE_DEVICE_VIDEO_SUPPORT: u32 = 0x800;
pub const D3D11_SDK_VERSION: u32 = 7;
pub const D3D11_FORMAT_SUPPORT_RENDER_TARGET: u32 = 0x4000;
pub const D3D_DRIVER_TYPE_UNKNOWN: u32 = 0;
pub const D3D_DRIVER_TYPE_HARDWARE: u32 = 1;
pub const D3D_FEATURE_LEVEL_10_0: u32 = 0xa000;
pub const D3D_FEATURE_LEVEL_10_1: u32 = 0xa100;
pub const D3D_FEATURE_LEVEL_11_0: u32 = 0xb000;
pub const D3D_FEATURE_LEVEL_11_1: u32 = 0xb100;

pub const DXGI_USAGE_RENDER_TARGET_OUTPUT: u32 = 0x20;
pub const DXGI_SCALING_STRETCH: u32 = 0;
pub const DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL: u32 = 3;
pub const DXGI_SWAP_EFFECT_FLIP_DISCARD: u32 = 4;
pub const DXGI_ALPHA_MODE_IGNORE: u32 = 3;
pub const DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING: u32 = 0x800;
pub const DXGI_PRESENT_ALLOW_TEARING: u32 = 0x200;
pub const DXGI_FEATURE_PRESENT_ALLOW_TEARING: u32 = 0;
pub const DXGI_MWA_NO_ALT_ENTER: u32 = 0x2;
pub const DXGI_MODE_ROTATION_IDENTITY: u32 = 1;
pub const DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME: u32 = 1;
pub const DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR: u32 = 2;
pub const DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR: u32 = 4;

pub const DXGI_ERROR_INVALID_CALL: HRESULT = 0x887A_0001_u32 as i32;
pub const DXGI_ERROR_NOT_FOUND: HRESULT = 0x887A_0002_u32 as i32;
pub const DXGI_ERROR_UNSUPPORTED: HRESULT = 0x887A_0004_u32 as i32;
pub const DXGI_ERROR_DEVICE_REMOVED: HRESULT = 0x887A_0005_u32 as i32;
pub const DXGI_ERROR_DEVICE_RESET: HRESULT = 0x887A_0007_u32 as i32;
pub const DXGI_ERROR_NOT_CURRENTLY_AVAILABLE: HRESULT = 0x887A_0022_u32 as i32;
pub const DXGI_ERROR_ACCESS_LOST: HRESULT = 0x887A_0026_u32 as i32;
pub const DXGI_ERROR_WAIT_TIMEOUT: HRESULT = 0x887A_0027_u32 as i32;
pub const DXGI_ERROR_SESSION_DISCONNECTED: HRESULT = 0x887A_0028_u32 as i32;
pub const DXGI_ERROR_ACCESS_DENIED: HRESULT = 0x887A_002B_u32 as i32;
pub const E_ACCESSDENIED: HRESULT = 0x8007_0005_u32 as i32;

pub const VENDOR_NVIDIA: u32 = 0x10DE;
pub const VENDOR_AMD: u32 = 0x1002;
pub const VENDOR_INTEL: u32 = 0x8086;
pub const VENDOR_MICROSOFT: u32 = 0x1414;

// ---------------------------------------------------------------------------
// Structs

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DXGI_RATIONAL {
    pub Numerator: u32,
    pub Denominator: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DXGI_SAMPLE_DESC {
    pub Count: u32,
    pub Quality: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DXGI_MODE_DESC {
    pub Width: u32,
    pub Height: u32,
    pub RefreshRate: DXGI_RATIONAL,
    pub Format: u32,
    pub ScanlineOrdering: u32,
    pub Scaling: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LUID {
    pub LowPart: u32,
    pub HighPart: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DXGI_ADAPTER_DESC1 {
    pub Description: [u16; 128],
    pub VendorId: u32,
    pub DeviceId: u32,
    pub SubSysId: u32,
    pub Revision: u32,
    pub DedicatedVideoMemory: usize,
    pub DedicatedSystemMemory: usize,
    pub SharedSystemMemory: usize,
    pub AdapterLuid: LUID,
    pub Flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DXGI_OUTPUT_DESC {
    pub DeviceName: [u16; 32],
    pub DesktopCoordinates: RECT,
    pub AttachedToDesktop: BOOL,
    pub Rotation: u32,
    pub Monitor: HMONITOR,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DXGI_OUTDUPL_DESC {
    pub ModeDesc: DXGI_MODE_DESC,
    pub Rotation: u32,
    pub DesktopImageInSystemMemory: BOOL,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DXGI_OUTDUPL_POINTER_POSITION {
    pub Position: POINT,
    pub Visible: BOOL,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DXGI_OUTDUPL_FRAME_INFO {
    pub LastPresentTime: i64,
    pub LastMouseUpdateTime: i64,
    pub AccumulatedFrames: u32,
    pub RectsCoalesced: BOOL,
    pub ProtectedContentMaskedOut: BOOL,
    pub PointerPosition: DXGI_OUTDUPL_POINTER_POSITION,
    pub TotalMetadataBufferSize: u32,
    pub PointerShapeBufferSize: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DXGI_OUTDUPL_POINTER_SHAPE_INFO {
    pub Type: u32,
    pub Width: u32,
    pub Height: u32,
    pub Pitch: u32,
    pub HotSpot: POINT,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DXGI_SWAP_CHAIN_DESC1 {
    pub Width: u32,
    pub Height: u32,
    pub Format: u32,
    pub Stereo: BOOL,
    pub SampleDesc: DXGI_SAMPLE_DESC,
    pub BufferUsage: u32,
    pub BufferCount: u32,
    pub Scaling: u32,
    pub SwapEffect: u32,
    pub AlphaMode: u32,
    pub Flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct D3D11_TEXTURE2D_DESC {
    pub Width: u32,
    pub Height: u32,
    pub MipLevels: u32,
    pub ArraySize: u32,
    pub Format: u32,
    pub SampleDesc: DXGI_SAMPLE_DESC,
    pub Usage: u32,
    pub BindFlags: u32,
    pub CPUAccessFlags: u32,
    pub MiscFlags: u32,
}

impl D3D11_TEXTURE2D_DESC {
    /// A plain 2D texture: one mip, one slice, no MSAA.
    pub fn new(w: u32, h: u32, format: u32, usage: u32, bind: u32, cpu: u32) -> Self {
        Self {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: usage,
            BindFlags: bind,
            CPUAccessFlags: cpu,
            MiscFlags: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct D3D11_SUBRESOURCE_DATA {
    pub pSysMem: *const c_void,
    pub SysMemPitch: u32,
    pub SysMemSlicePitch: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct D3D11_MAPPED_SUBRESOURCE {
    pub pData: *mut c_void,
    pub RowPitch: u32,
    pub DepthPitch: u32,
}

impl Default for D3D11_MAPPED_SUBRESOURCE {
    fn default() -> Self {
        Self { pData: std::ptr::null_mut(), RowPitch: 0, DepthPitch: 0 }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct D3D11_VIEWPORT {
    pub TopLeftX: f32,
    pub TopLeftY: f32,
    pub Width: f32,
    pub Height: f32,
    pub MinDepth: f32,
    pub MaxDepth: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct D3D11_SAMPLER_DESC {
    pub Filter: u32,
    pub AddressU: u32,
    pub AddressV: u32,
    pub AddressW: u32,
    pub MipLODBias: f32,
    pub MaxAnisotropy: u32,
    pub ComparisonFunc: u32,
    pub BorderColor: [f32; 4],
    pub MinLOD: f32,
    pub MaxLOD: f32,
}

/// `D3D11_RENDER_TARGET_VIEW_DESC` for a 2D texture (the union is 12 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct D3D11_RENDER_TARGET_VIEW_DESC {
    pub Format: u32,
    pub ViewDimension: u32,
    /// `Texture2D.MipSlice`, then padding of the union.
    pub u: [u32; 3],
}

/// `D3D11_SHADER_RESOURCE_VIEW_DESC` for a 2D texture (the union is 16 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct D3D11_SHADER_RESOURCE_VIEW_DESC {
    pub Format: u32,
    pub ViewDimension: u32,
    /// `Texture2D.MostDetailedMip`, `Texture2D.MipLevels`, then padding.
    pub u: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct D3D11_BUFFER_DESC {
    pub ByteWidth: u32,
    pub Usage: u32,
    pub BindFlags: u32,
    pub CPUAccessFlags: u32,
    pub MiscFlags: u32,
    pub StructureByteStride: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct D3D11_BOX {
    pub left: u32,
    pub top: u32,
    pub front: u32,
    pub right: u32,
    pub bottom: u32,
    pub back: u32,
}

const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<DXGI_ADAPTER_DESC1>() == 312);
    assert!(offset_of!(DXGI_ADAPTER_DESC1, AdapterLuid) == 296);
    assert!(size_of::<DXGI_OUTPUT_DESC>() == 96);
    assert!(size_of::<DXGI_OUTDUPL_DESC>() == 36);
    assert!(size_of::<DXGI_OUTDUPL_FRAME_INFO>() == 48);
    assert!(offset_of!(DXGI_OUTDUPL_FRAME_INFO, PointerShapeBufferSize) == 44);
    assert!(size_of::<DXGI_OUTDUPL_POINTER_SHAPE_INFO>() == 24);
    assert!(size_of::<DXGI_SWAP_CHAIN_DESC1>() == 48);
    assert!(size_of::<D3D11_TEXTURE2D_DESC>() == 44);
    assert!(size_of::<D3D11_SUBRESOURCE_DATA>() == 16);
    assert!(size_of::<D3D11_MAPPED_SUBRESOURCE>() == 16);
    assert!(size_of::<D3D11_VIEWPORT>() == 24);
    assert!(size_of::<D3D11_SAMPLER_DESC>() == 52);
    assert!(size_of::<D3D11_RENDER_TARGET_VIEW_DESC>() == 20);
    assert!(size_of::<D3D11_SHADER_RESOURCE_VIEW_DESC>() == 24);
    assert!(size_of::<D3D11_BUFFER_DESC>() == 24);
    assert!(size_of::<D3D11_BOX>() == 24);
};

// ---------------------------------------------------------------------------
// Interfaces (absolute vtable slots)

com_interface! {
    pub struct IDXGIFactory1(0x770aae78_f26f_4dba_a829_253c83d1b387);
    8 fn MakeWindowAssociation(hwnd: HWND, flags: u32) -> HRESULT;
    12 fn EnumAdapters1(index: u32, out: *mut *mut c_void) -> HRESULT;
}

com_interface! {
    pub struct IDXGIFactory2(0x50c83a1c_e072_4c48_87b0_3630fa36a6d0);
    8 fn MakeWindowAssociation(hwnd: HWND, flags: u32) -> HRESULT;
    15 fn CreateSwapChainForHwnd(
        device: *mut c_void,
        hwnd: HWND,
        desc: *const DXGI_SWAP_CHAIN_DESC1,
        fullscreen: *const c_void,
        restrict_output: *mut c_void,
        out: *mut *mut c_void,
    ) -> HRESULT;
}

com_interface! {
    pub struct IDXGIFactory5(0x7632e1f5_ee65_4dca_87fd_84cd75f8838d);
    28 fn CheckFeatureSupport(feature: u32, data: *mut c_void, size: u32) -> HRESULT;
}

com_interface! {
    pub struct IDXGIAdapter1(0x29038f61_3839_4626_91fd_086879011a05);
    6 fn GetParent(iid: *const GUID, out: *mut *mut c_void) -> HRESULT;
    7 fn EnumOutputs(index: u32, out: *mut *mut c_void) -> HRESULT;
    10 fn GetDesc1(desc: *mut DXGI_ADAPTER_DESC1) -> HRESULT;
}

com_interface! {
    pub struct IDXGIOutput(0xae02eedb_c735_4690_8d52_5a8dc20213aa);
    7 fn GetDesc(desc: *mut DXGI_OUTPUT_DESC) -> HRESULT;
}

com_interface! {
    pub struct IDXGIOutput1(0x00cddea8_939b_4b83_a340_a685226666cc);
    7 fn GetDesc(desc: *mut DXGI_OUTPUT_DESC) -> HRESULT;
    22 fn DuplicateOutput(device: *mut c_void, out: *mut *mut c_void) -> HRESULT;
}

com_interface! {
    pub struct IDXGIOutput5(0x80a07424_ab52_42eb_833c_0c42fd282d98);
    26 fn DuplicateOutput1(
        device: *mut c_void,
        flags: u32,
        num_formats: u32,
        formats: *const u32,
        out: *mut *mut c_void,
    ) -> HRESULT;
}

com_interface! {
    pub struct IDXGIOutputDuplication(0x191cfac3_a341_470d_b26e_a864f428319c);
    7 fn GetDesc(desc: *mut DXGI_OUTDUPL_DESC);
    8 fn AcquireNextFrame(timeout_ms: u32, info: *mut DXGI_OUTDUPL_FRAME_INFO, resource: *mut *mut c_void) -> HRESULT;
    11 fn GetFramePointerShape(
        size: u32,
        buf: *mut c_void,
        required: *mut u32,
        info: *mut DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    ) -> HRESULT;
    14 fn ReleaseFrame() -> HRESULT;
}

com_interface! {
    pub struct IDXGIDevice(0x54ec77fa_1377_44e6_8c32_88fd5f44c84c);
    7 fn GetAdapter(out: *mut *mut c_void) -> HRESULT;
}

com_interface! {
    pub struct IDXGIDevice1(0x77db970f_6276_48ba_ba28_070143b4392c);
    12 fn SetMaximumFrameLatency(frames: u32) -> HRESULT;
}

com_interface! {
    pub struct IDXGISwapChain1(0x790a45f7_0d42_4876_983a_0a55cfe6f4aa);
    8 fn Present(sync_interval: u32, flags: u32) -> HRESULT;
    9 fn GetBuffer(index: u32, iid: *const GUID, out: *mut *mut c_void) -> HRESULT;
    13 fn ResizeBuffers(count: u32, w: u32, h: u32, format: u32, flags: u32) -> HRESULT;
}

com_interface! {
    pub struct ID3D11Device(0xdb6f6ddb_ac77_4e88_8253_819df9bbf140);
    3 fn CreateBuffer(desc: *const D3D11_BUFFER_DESC, init: *const D3D11_SUBRESOURCE_DATA, out: *mut *mut c_void) -> HRESULT;
    5 fn CreateTexture2D(desc: *const D3D11_TEXTURE2D_DESC, init: *const D3D11_SUBRESOURCE_DATA, out: *mut *mut c_void) -> HRESULT;
    7 fn CreateShaderResourceView(
        resource: *mut c_void,
        desc: *const D3D11_SHADER_RESOURCE_VIEW_DESC,
        out: *mut *mut c_void,
    ) -> HRESULT;
    9 fn CreateRenderTargetView(
        resource: *mut c_void,
        desc: *const D3D11_RENDER_TARGET_VIEW_DESC,
        out: *mut *mut c_void,
    ) -> HRESULT;
    12 fn CreateVertexShader(code: *const c_void, len: usize, linkage: *mut c_void, out: *mut *mut c_void) -> HRESULT;
    15 fn CreatePixelShader(code: *const c_void, len: usize, linkage: *mut c_void, out: *mut *mut c_void) -> HRESULT;
    23 fn CreateSamplerState(desc: *const D3D11_SAMPLER_DESC, out: *mut *mut c_void) -> HRESULT;
    29 fn CheckFormatSupport(format: u32, support: *mut u32) -> HRESULT;
    37 fn GetFeatureLevel() -> u32;
    39 fn GetDeviceRemovedReason() -> HRESULT;
    40 fn GetImmediateContext(out: *mut *mut c_void);
}

com_interface! {
    pub struct ID3D11DeviceContext(0xc0bfa96c_e089_44fb_8eaf_26f8796190da);
    8 fn PSSetShaderResources(start: u32, n: u32, views: *const *mut c_void);
    9 fn PSSetShader(shader: *mut c_void, classes: *const *mut c_void, n: u32);
    10 fn PSSetSamplers(start: u32, n: u32, samplers: *const *mut c_void);
    11 fn VSSetShader(shader: *mut c_void, classes: *const *mut c_void, n: u32);
    13 fn Draw(vertices: u32, start: u32);
    14 fn Map(resource: *mut c_void, sub: u32, kind: u32, flags: u32, out: *mut D3D11_MAPPED_SUBRESOURCE) -> HRESULT;
    15 fn Unmap(resource: *mut c_void, sub: u32);
    16 fn PSSetConstantBuffers(start: u32, n: u32, buffers: *const *mut c_void);
    17 fn IASetInputLayout(layout: *mut c_void);
    24 fn IASetPrimitiveTopology(topology: u32);
    33 fn OMSetRenderTargets(n: u32, views: *const *mut c_void, depth: *mut c_void);
    35 fn OMSetBlendState(state: *mut c_void, factor: *const f32, mask: u32);
    43 fn RSSetState(state: *mut c_void);
    44 fn RSSetViewports(n: u32, viewports: *const D3D11_VIEWPORT);
    46 fn CopySubresourceRegion(
        dst: *mut c_void,
        dst_sub: u32,
        x: u32,
        y: u32,
        z: u32,
        src: *mut c_void,
        src_sub: u32,
        src_box: *const D3D11_BOX,
    );
    47 fn CopyResource(dst: *mut c_void, src: *mut c_void);
    48 fn UpdateSubresource(
        dst: *mut c_void,
        sub: u32,
        dst_box: *const D3D11_BOX,
        data: *const c_void,
        row_pitch: u32,
        depth_pitch: u32,
    );
    50 fn ClearRenderTargetView(view: *mut c_void, color: *const f32);
    110 fn ClearState();
    111 fn Flush();
}

com_interface! {
    pub struct ID3D11Texture2D(0x6f15aaf2_d208_4e89_9ab4_489535d34f9c);
    10 fn GetDesc(desc: *mut D3D11_TEXTURE2D_DESC);
}

com_interface! {
    pub struct ID3D11Resource(0xdc8e63f3_d12b_4952_b47b_5e45026a862d);
}

com_interface! {
    pub struct ID3D11RenderTargetView(0xdfdba067_0b8d_4865_875b_d7b4516cc164);
}

com_interface! {
    pub struct ID3D11ShaderResourceView(0xb0e06fe0_8192_4e1a_b1ca_36d7414710b2);
}

com_interface! {
    pub struct ID3D11VertexShader(0x3b301d64_d678_4289_8897_22f8928b72f3);
}

com_interface! {
    pub struct ID3D11PixelShader(0xea82e40d_51dc_4f33_93d4_db7c9125ae8c);
}

com_interface! {
    pub struct ID3D11SamplerState(0xda6fea51_564c_4487_9810_f0d0f9b4e3a5);
}

com_interface! {
    pub struct ID3D11Buffer(0x48570b85_d1ee_4fcd_a250_eb350722b037);
}

com_interface! {
    /// Lets Media Foundation share the immediate context with our own calls.
    pub struct ID3D10Multithread(0x9b7e4e00_342c_4106_a19f_4f2704f689f0);
    3 fn Enter();
    4 fn Leave();
    5 fn SetMultithreadProtected(on: BOOL) -> BOOL;
}

com_interface! {
    pub struct ID3DBlob(0x8ba5fb08_5195_40e2_ac58_0d989c3a0102);
    3 fn GetBufferPointer() -> *mut c_void;
    4 fn GetBufferSize() -> usize;
}

#[link(name = "d3d11")]
unsafe extern "system" {
    pub fn D3D11CreateDevice(
        adapter: *mut c_void,
        driver_type: u32,
        software: sys::HMODULE,
        flags: u32,
        levels: *const u32,
        num_levels: u32,
        sdk_version: u32,
        device: *mut *mut c_void,
        level: *mut u32,
        context: *mut *mut c_void,
    ) -> HRESULT;
}

#[link(name = "dxgi")]
unsafe extern "system" {
    pub fn CreateDXGIFactory1(iid: *const GUID, out: *mut *mut c_void) -> HRESULT;
}

// ---------------------------------------------------------------------------
// Helpers

pub fn factory() -> Result<IDXGIFactory1, String> {
    com::out(|p| unsafe { CreateDXGIFactory1(&IDXGIFactory1::IID, p) }, "CreateDXGIFactory1")
}

/// One GPU as DXGI sees it.
#[derive(Clone)]
pub struct Adapter {
    pub adapter: IDXGIAdapter1,
    pub index: u32,
    pub name: String,
    pub vendor: u32,
    pub device_id: u32,
    pub luid: LUID,
    pub dedicated_mb: u64,
    pub software: bool,
}

impl Adapter {
    pub fn vendor_name(&self) -> &'static str {
        vendor_name(self.vendor)
    }
}

pub fn vendor_name(vendor: u32) -> &'static str {
    match vendor {
        VENDOR_NVIDIA => "NVIDIA",
        VENDOR_AMD => "AMD",
        VENDOR_INTEL => "Intel",
        VENDOR_MICROSOFT => "Microsoft",
        _ => "unknown vendor",
    }
}

/// All adapters, in DXGI order (the first is the one driving the primary display).
pub fn adapters(f: &IDXGIFactory1) -> Vec<Adapter> {
    let mut v = Vec::new();
    for i in 0.. {
        let Ok(a) = com::out::<IDXGIAdapter1>(|p| unsafe { f.EnumAdapters1(i, p) }, "EnumAdapters1") else { break };
        // SAFETY: zeroed POD out-struct.
        let mut d: DXGI_ADAPTER_DESC1 = unsafe { std::mem::zeroed() };
        if unsafe { a.GetDesc1(&mut d) } < 0 {
            continue;
        }
        v.push(Adapter {
            adapter: a,
            index: i,
            name: sys::from_wide(&d.Description),
            vendor: d.VendorId,
            device_id: d.DeviceId,
            luid: d.AdapterLuid,
            dedicated_mb: d.DedicatedVideoMemory as u64 >> 20,
            // DXGI_ADAPTER_FLAG_SOFTWARE (Microsoft Basic Render Driver).
            software: d.Flags & 2 != 0 || (d.VendorId == VENDOR_MICROSOFT && d.DeviceId == 0x8c),
        });
    }
    v
}

/// The first hardware adapter, preferring NVIDIA (the fast path).
pub fn preferred_adapter(list: &[Adapter]) -> Option<&Adapter> {
    list.iter().find(|a| !a.software && a.vendor == VENDOR_NVIDIA).or_else(|| list.iter().find(|a| !a.software))
}

/// A D3D11 device + immediate context on `adapter` (None = default adapter).
pub struct Device {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub feature_level: u32,
}

impl Device {
    pub fn new(adapter: Option<&IDXGIAdapter1>) -> Result<Device, String> {
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        let try_create = |flags: u32, levels: &[u32]| {
            let (mut dev, mut ctx, mut level) = (std::ptr::null_mut(), std::ptr::null_mut(), 0u32);
            let ad = adapter.map_or(std::ptr::null_mut(), |a| a.as_raw());
            let driver = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
            // SAFETY: valid out pointers and feature level array.
            let hr = unsafe {
                D3D11CreateDevice(
                    ad,
                    driver,
                    std::ptr::null_mut(),
                    flags,
                    levels.as_ptr(),
                    levels.len() as u32,
                    D3D11_SDK_VERSION,
                    &mut dev,
                    &mut level,
                    &mut ctx,
                )
            };
            (hr, dev, ctx, level)
        };
        let (mut hr, mut dev, mut ctx, mut level) = try_create(flags, &levels);
        if hr == sys::E_INVALIDARG {
            // Windows 7 without the platform update rejects 11_1 in the list.
            (hr, dev, ctx, level) = try_create(flags, &levels[1..]);
        }
        if hr < 0 {
            // Some drivers refuse VIDEO_SUPPORT; presentation still works without it.
            (hr, dev, ctx, level) = try_create(D3D11_CREATE_DEVICE_BGRA_SUPPORT, &levels[1..]);
        }
        sys::check(hr, "D3D11CreateDevice")?;
        // SAFETY: the call succeeded and returned owned references.
        let device = unsafe { ID3D11Device::from_raw(dev) }.ok_or("D3D11CreateDevice returned no device")?;
        let context = unsafe { ID3D11DeviceContext::from_raw(ctx) }.ok_or("D3D11CreateDevice returned no context")?;
        // Media Foundation and our threads share the immediate context.
        if let Ok(mt) = device.cast::<ID3D10Multithread>() {
            // SAFETY: plain setter.
            unsafe { mt.SetMultithreadProtected(1) };
        }
        Ok(Device { device, context, feature_level: level })
    }

    pub fn texture(
        &self,
        desc: &D3D11_TEXTURE2D_DESC,
        init: Option<&D3D11_SUBRESOURCE_DATA>,
    ) -> Result<ID3D11Texture2D, String> {
        let init = init.map_or(std::ptr::null(), |i| i as *const _);
        com::out(|p| unsafe { self.device.CreateTexture2D(desc, init, p) }, "CreateTexture2D")
    }

    /// Render target view of `tex` (for NV12: `R8_UNORM` = luma plane, `R8G8_UNORM` = chroma plane).
    pub fn rtv(&self, tex: &ID3D11Texture2D, format: u32) -> Result<ID3D11RenderTargetView, String> {
        let d =
            D3D11_RENDER_TARGET_VIEW_DESC { Format: format, ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D, u: [0; 3] };
        com::out(|p| unsafe { self.device.CreateRenderTargetView(tex.as_raw(), &d, p) }, "CreateRenderTargetView")
    }

    /// Shader resource view of `tex` (for NV12: `R8_UNORM` = luma plane, `R8G8_UNORM` = chroma plane).
    pub fn srv(&self, tex: &ID3D11Texture2D, format: u32) -> Result<ID3D11ShaderResourceView, String> {
        let d = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            u: [0, 1, 0, 0],
        };
        com::out(|p| unsafe { self.device.CreateShaderResourceView(tex.as_raw(), &d, p) }, "CreateShaderResourceView")
    }

    pub fn sampler(&self, filter: u32) -> Result<ID3D11SamplerState, String> {
        let d = D3D11_SAMPLER_DESC {
            Filter: filter,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MipLODBias: 0.0,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            BorderColor: [0.0; 4],
            MinLOD: 0.0,
            MaxLOD: D3D11_FLOAT32_MAX,
        };
        com::out(|p| unsafe { self.device.CreateSamplerState(&d, p) }, "CreateSamplerState")
    }

    /// A dynamic constant buffer of `size` bytes (multiple of 16).
    pub fn constant_buffer(&self, size: u32) -> Result<ID3D11Buffer, String> {
        let d = D3D11_BUFFER_DESC {
            ByteWidth: size.div_ceil(16) * 16,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        com::out(|p| unsafe { self.device.CreateBuffer(&d, std::ptr::null(), p) }, "CreateBuffer")
    }

    /// Overwrite a dynamic buffer.
    pub fn write_buffer(&self, buf: &ID3D11Buffer, data: &[u8]) -> Result<(), String> {
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: dynamic buffer created with CPU write access; we copy at most its size.
        unsafe {
            sys::check(self.context.Map(buf.as_raw(), 0, D3D11_MAP_WRITE_DISCARD, 0, &mut m), "Map")?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), m.pData as *mut u8, data.len());
            self.context.Unmap(buf.as_raw(), 0);
        }
        Ok(())
    }

    pub fn vertex_shader(&self, code: &[u8]) -> Result<ID3D11VertexShader, String> {
        com::out(
            |p| unsafe { self.device.CreateVertexShader(code.as_ptr().cast(), code.len(), std::ptr::null_mut(), p) },
            "CreateVertexShader",
        )
    }

    pub fn pixel_shader(&self, code: &[u8]) -> Result<ID3D11PixelShader, String> {
        com::out(
            |p| unsafe { self.device.CreatePixelShader(code.as_ptr().cast(), code.len(), std::ptr::null_mut(), p) },
            "CreatePixelShader",
        )
    }

    /// Whether `format` can be a render target on this device (NV12 needs Windows 8 + driver support).
    pub fn can_render_to(&self, format: u32) -> bool {
        let mut s = 0u32;
        // SAFETY: valid out pointer.
        unsafe { self.device.CheckFormatSupport(format, &mut s) >= 0 && s & D3D11_FORMAT_SUPPORT_RENDER_TARGET != 0 }
    }

    /// The DXGI adapter this device runs on.
    pub fn adapter(&self) -> Result<IDXGIAdapter1, String> {
        let dxgi: IDXGIDevice = self.device.cast().map_err(|hr| sys::hr_error("IDXGIDevice", hr))?;
        let a: com::Com = {
            let mut p = std::ptr::null_mut();
            // SAFETY: valid out pointer.
            sys::check(unsafe { dxgi.GetAdapter(&mut p) }, "GetAdapter")?;
            unsafe { com::Com::from_raw(p) }.ok_or("no adapter")?
        };
        a.query::<IDXGIAdapter1>().map_err(|hr| sys::hr_error("IDXGIAdapter1", hr))
    }
}

/// A viewport covering `(x, y, w, h)`.
pub fn viewport(x: f32, y: f32, w: f32, h: f32) -> D3D11_VIEWPORT {
    D3D11_VIEWPORT { TopLeftX: x, TopLeftY: y, Width: w, Height: h, MinDepth: 0.0, MaxDepth: 1.0 }
}

// ---------------------------------------------------------------------------
// HLSL compilation (d3dcompiler_47.dll ships with Windows 10 and later)

type FnD3DCompile = unsafe extern "system" fn(
    src: *const c_void,
    len: usize,
    name: *const u8,
    defines: *const c_void,
    include: *mut c_void,
    entry: *const u8,
    target: *const u8,
    flags1: u32,
    flags2: u32,
    code: *mut *mut c_void,
    errors: *mut *mut c_void,
) -> HRESULT;

const D3DCOMPILE_OPTIMIZATION_LEVEL3: u32 = 1 << 15;

/// Compile HLSL source for `target` ("vs_4_0", "ps_4_0") with entry point `entry`.
pub fn compile(src: &str, entry: &str, target: &str) -> Result<Vec<u8>, String> {
    static LIB: std::sync::OnceLock<Result<FnD3DCompile, String>> = std::sync::OnceLock::new();
    let compile = *LIB
        .get_or_init(|| {
            let lib = crate::dl::Library::open(&["d3dcompiler_47.dll"])?;
            // SAFETY: D3DCompile has exactly this signature (d3dcompiler.h).
            let f = unsafe { lib.sym::<FnD3DCompile>("D3DCompile\0") };
            std::mem::forget(lib); // keep loaded for the process lifetime
            f
        })
        .as_ref()
        .map_err(|e| e.clone())?;
    let entry = format!("{entry}\0");
    let target_z = format!("{target}\0");
    let (mut code, mut errors) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: valid source buffer and NUL-terminated strings; out pointers receive owned blobs.
    let hr = unsafe {
        compile(
            src.as_ptr().cast(),
            src.len(),
            c"sard.hlsl".as_ptr().cast(),
            std::ptr::null(),
            std::ptr::null_mut(),
            entry.as_ptr(),
            target_z.as_ptr(),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            &mut errors,
        )
    };
    let blob_bytes = |p: *mut c_void| -> Option<Vec<u8>> {
        // SAFETY: an owned ID3DBlob from D3DCompile.
        let b = unsafe { ID3DBlob::from_raw(p) }?;
        // SAFETY: the blob owns this buffer while alive.
        Some(unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }.to_vec())
    };
    let errs = blob_bytes(errors).map(|e| String::from_utf8_lossy(&e).trim_end_matches('\0').trim().to_string());
    if hr < 0 {
        return Err(format!(
            "{}: {}",
            sys::hr_error(&format!("D3DCompile {entry} ({target})"), hr),
            errs.unwrap_or_default()
        ));
    }
    blob_bytes(code).ok_or_else(|| "D3DCompile returned no code".into())
}

/// Opt in to per-monitor DPI awareness (v2), so desktop duplication, cursor
/// positions and `SendInput` all use physical pixels. Harmless if already set
/// (winit does it for windowed processes).
pub fn set_dpi_aware() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        type FnSetCtx = unsafe extern "system" fn(isize) -> BOOL;
        let user32 = sys::wide("user32.dll");
        // SAFETY: user32 is always loaded in a process that links it; the
        // symbol has this signature (Windows 10 1703+).
        unsafe {
            let m = sys::GetModuleHandleW(user32.as_ptr());
            let p = sys::GetProcAddress(m, c"SetProcessDpiAwarenessContext".as_ptr().cast());
            if !p.is_null() {
                let f: FnSetCtx = std::mem::transmute::<*mut c_void, FnSetCtx>(p);
                // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
                f(-4);
            }
        }
    });
}
