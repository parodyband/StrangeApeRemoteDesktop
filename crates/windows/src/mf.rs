//! Media Foundation: declarations, MFT enumeration and a driver for
//! synchronous and asynchronous (hardware) transforms with Direct3D 11
//! surfaces.
//!
//! mfplat.dll is loaded at run time: Windows "N" editions ship without Media
//! Foundation until the Media Feature Pack is installed, and sard should still
//! start there (NVENC/NVDEC don't need it).

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals, clippy::upper_case_acronyms, dead_code)]

use crate::com::{self, Com, Interface};
use crate::com_interface;
use crate::sys::{self, BOOL, GUID, HRESULT};
use std::ffi::c_void;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// GUIDs

const fn fourcc_guid(fcc: &[u8; 4]) -> GUID {
    GUID {
        data1: u32::from_le_bytes(*fcc),
        data2: 0x0000,
        data3: 0x0010,
        data4: [0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71],
    }
}

pub const MFMediaType_Video: GUID = GUID::from_u128(0x73646976_0000_0010_8000_00aa00389b71);
pub const MFVideoFormat_H264: GUID = fourcc_guid(b"H264");
pub const MFVideoFormat_HEVC: GUID = fourcc_guid(b"HEVC");
pub const MFVideoFormat_AV1: GUID = fourcc_guid(b"AV01");
pub const MFVideoFormat_NV12: GUID = fourcc_guid(b"NV12");

pub const MFT_CATEGORY_VIDEO_DECODER: GUID = GUID::from_u128(0xd6c02d4b_6833_45b4_971a_05a4b04bab91);
pub const MFT_CATEGORY_VIDEO_ENCODER: GUID = GUID::from_u128(0xf79eac7d_e545_4387_bdee_d647d7bde42a);

pub const MF_MT_MAJOR_TYPE: GUID = GUID::from_u128(0x48eba18e_f8c9_4687_bf11_0a74c9f96a8f);
pub const MF_MT_SUBTYPE: GUID = GUID::from_u128(0xf7e34c9a_42e8_4714_b74b_cb29d72c35e5);
pub const MF_MT_FRAME_SIZE: GUID = GUID::from_u128(0x1652c33d_d6b2_4012_b834_72030849a37d);
pub const MF_MT_FRAME_RATE: GUID = GUID::from_u128(0xc459a2e8_3d2c_4e44_b132_fee5156c7bb0);
pub const MF_MT_AVG_BITRATE: GUID = GUID::from_u128(0x20332624_fb0d_4d9e_bd0d_cbf6786c102e);
pub const MF_MT_INTERLACE_MODE: GUID = GUID::from_u128(0xe2724bb8_e676_4806_b4b2_a8d6efb44ccd);
pub const MF_MT_PIXEL_ASPECT_RATIO: GUID = GUID::from_u128(0xc6376a1e_8d0a_4027_be45_6d9a0ad39bb6);
pub const MF_MT_MPEG2_PROFILE: GUID = GUID::from_u128(0xad76a80b_2d5c_4e0b_b375_64e520137036);
pub const MF_MT_MPEG_SEQUENCE_HEADER: GUID = GUID::from_u128(0x3c036de7_3ad0_4c9e_9216_ee6d6ac21cb3);
pub const MF_MT_MINIMUM_DISPLAY_APERTURE: GUID = GUID::from_u128(0xd7388766_18fe_48c6_a177_ee894867c8c4);
pub const MF_MT_VIDEO_NOMINAL_RANGE: GUID = GUID::from_u128(0xc21b8ee5_b956_4071_8daf_325edf5cab11);
pub const MF_MT_YUV_MATRIX: GUID = GUID::from_u128(0x3e23d450_2c75_4d25_a00e_b91670d12327);
pub const MF_MT_VIDEO_PRIMARIES: GUID = GUID::from_u128(0xdbfbe4d7_0740_4ee0_8192_850ab0e21935);
pub const MF_MT_TRANSFER_FUNCTION: GUID = GUID::from_u128(0x5fb0fce9_be5c_4935_a811_ec838f8eed93);
pub const MF_TRANSFORM_ASYNC: GUID = GUID::from_u128(0xf81a699a_649a_497d_8c73_29f8fed6ad7a);
pub const MF_TRANSFORM_ASYNC_UNLOCK: GUID = GUID::from_u128(0xe5666d6b_3422_4eb6_a421_da7db1f8e207);
pub const MF_LOW_LATENCY: GUID = GUID::from_u128(0x9c27891a_ed7a_40e1_88e8_b22727a024ee);
pub const MF_SA_D3D11_AWARE: GUID = GUID::from_u128(0x206b4fc8_fcf9_4c51_afe3_9764369e33a0);
pub const MFT_ENUM_HARDWARE_VENDOR_ID_Attribute: GUID = GUID::from_u128(0x3aecb0cc_035b_4bcc_8185_2b8d551ef3af);
pub const MFT_FRIENDLY_NAME_Attribute: GUID = GUID::from_u128(0x314ffbae_5b41_4c95_9c19_4e7d586face3);
pub const MFSampleExtension_CleanPoint: GUID = GUID::from_u128(0x9cdf01d8_a0f0_43ba_b077_eaa06cbd728a);

// ICodecAPI properties (codecapi.h)
pub const CODECAPI_AVLowLatencyMode: GUID = GUID::from_u128(0x9c27891a_ed7a_40e1_88e8_b22727a024ee);
pub const CODECAPI_AVEncCommonRateControlMode: GUID = GUID::from_u128(0x1c0608e9_370c_4710_8a58_cb6181c42423);
pub const CODECAPI_AVEncCommonMeanBitRate: GUID = GUID::from_u128(0xf7222374_2144_4815_b550_a37f8e12ee52);
pub const CODECAPI_AVEncCommonMaxBitRate: GUID = GUID::from_u128(0x9651eae4_39b9_4ebf_85ef_d7f444ec7465);
pub const CODECAPI_AVEncCommonBufferSize: GUID = GUID::from_u128(0x0db96574_b6a4_4c8b_8106_3773de0310cd);
pub const CODECAPI_AVEncCommonQualityVsSpeed: GUID = GUID::from_u128(0x98332df8_03cd_476b_89fa_3f9e442dec9f);
pub const CODECAPI_AVEncMPVGOPSize: GUID = GUID::from_u128(0x95f31b26_95a4_41aa_9303_246a7fc6eef1);
pub const CODECAPI_AVEncMPVDefaultBPictureCount: GUID = GUID::from_u128(0x8d390aac_dc5c_4200_b57f_814d04babab2);
pub const CODECAPI_AVEncVideoForceKeyFrame: GUID = GUID::from_u128(0x398c1b98_8353_475a_9ef2_8f265d260345);

// ---------------------------------------------------------------------------
// Constants

pub const MF_VERSION: u32 = 0x0002_0070;
pub const MFSTARTUP_LITE: u32 = 1;
pub const MFT_ENUM_FLAG_SYNCMFT: u32 = 0x1;
pub const MFT_ENUM_FLAG_ASYNCMFT: u32 = 0x2;
pub const MFT_ENUM_FLAG_HARDWARE: u32 = 0x4;
pub const MFT_ENUM_FLAG_SORTANDFILTER: u32 = 0x40;
pub const MFT_MESSAGE_COMMAND_FLUSH: u32 = 0;
pub const MFT_MESSAGE_SET_D3D_MANAGER: u32 = 2;
pub const MFT_MESSAGE_NOTIFY_BEGIN_STREAMING: u32 = 0x1000_0000;
pub const MFT_MESSAGE_NOTIFY_END_STREAMING: u32 = 0x1000_0001;
pub const MFT_MESSAGE_NOTIFY_START_OF_STREAM: u32 = 0x1000_0003;
pub const MFT_OUTPUT_STREAM_PROVIDES_SAMPLES: u32 = 0x100;
pub const MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES: u32 = 0x200;
pub const MF_EVENT_FLAG_NO_WAIT: u32 = 1;
pub const METransformNeedInput: u32 = 601;
pub const METransformHaveOutput: u32 = 602;
pub const MFVideoInterlace_Progressive: u32 = 2;
pub const MFNominalRange_0_255: u32 = 1;
pub const MFNominalRange_16_235: u32 = 2;
pub const MFVideoTransferMatrix_BT709: u32 = 1;
pub const MFVideoPrimaries_BT709: u32 = 2;
pub const MFVideoTransFunc_709: u32 = 5;
pub const eAVEncCommonRateControlMode_CBR: u32 = 0;
pub const eAVEncH264VProfile_High: u32 = 100;
pub const eAVEncH265VProfile_Main_420_8: u32 = 1;

pub const MF_E_TRANSFORM_TYPE_NOT_SET: HRESULT = 0xC00D_6D60_u32 as i32;
pub const MF_E_TRANSFORM_STREAM_CHANGE: HRESULT = 0xC00D_6D61_u32 as i32;
pub const MF_E_TRANSFORM_NEED_MORE_INPUT: HRESULT = 0xC00D_6D72_u32 as i32;
pub const MF_E_NOTACCEPTING: HRESULT = 0xC00D_36B5_u32 as i32;
pub const MF_E_NO_EVENTS_AVAILABLE: HRESULT = 0xC00D_3E80_u32 as i32;
pub const MF_E_NO_MORE_TYPES: HRESULT = 0xC00D_36B9_u32 as i32;

pub const VT_UI4: u16 = 19;
pub const VT_UI8: u16 = 21;
pub const VT_BOOL: u16 = 11;

// ---------------------------------------------------------------------------
// Structs

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MFT_REGISTER_TYPE_INFO {
    pub guidMajorType: GUID,
    pub guidSubtype: GUID,
}

#[repr(C)]
pub struct MFT_OUTPUT_DATA_BUFFER {
    pub dwStreamID: u32,
    pub pSample: *mut c_void,
    pub dwStatus: u32,
    pub pEvents: *mut c_void,
}

#[repr(C)]
#[derive(Default)]
pub struct MFT_OUTPUT_STREAM_INFO {
    pub dwFlags: u32,
    pub cbSize: u32,
    pub cbAlignment: u32,
}

/// `VARIANT` / `PROPVARIANT` with an integer payload (24 bytes on x64).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VARIANT {
    pub vt: u16,
    pub r1: u16,
    pub r2: u16,
    pub r3: u16,
    pub val: u64,
    pub pad: u64,
}

impl VARIANT {
    pub fn u32(v: u32) -> Self {
        VARIANT { vt: VT_UI4, r1: 0, r2: 0, r3: 0, val: v as u64, pad: 0 }
    }
    pub fn bool(v: bool) -> Self {
        // VARIANT_TRUE is -1 (all bits).
        VARIANT { vt: VT_BOOL, r1: 0, r2: 0, r3: 0, val: if v { 0xFFFF } else { 0 }, pad: 0 }
    }
}

/// `MFVideoArea` (MF_MT_MINIMUM_DISPLAY_APERTURE).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct MFVideoArea {
    pub OffsetX: u32,
    pub OffsetY: u32,
    pub cx: i32,
    pub cy: i32,
}

const _: () = {
    use std::mem::size_of;
    assert!(size_of::<MFT_REGISTER_TYPE_INFO>() == 32);
    assert!(size_of::<MFT_OUTPUT_DATA_BUFFER>() == 32);
    assert!(size_of::<MFT_OUTPUT_STREAM_INFO>() == 12);
    assert!(size_of::<VARIANT>() == 24);
    assert!(size_of::<MFVideoArea>() == 16);
};

// ---------------------------------------------------------------------------
// Interfaces

com_interface! {
    pub struct IMFAttributes(0x2cd2d921_c447_44a7_a13c_4adabfc247e3);
    7 fn GetUINT32(key: *const GUID, out: *mut u32) -> HRESULT;
    8 fn GetUINT64(key: *const GUID, out: *mut u64) -> HRESULT;
    10 fn GetGUID(key: *const GUID, out: *mut GUID) -> HRESULT;
    11 fn GetStringLength(key: *const GUID, len: *mut u32) -> HRESULT;
    12 fn GetString(key: *const GUID, buf: *mut u16, size: u32, len: *mut u32) -> HRESULT;
    14 fn GetBlobSize(key: *const GUID, size: *mut u32) -> HRESULT;
    15 fn GetBlob(key: *const GUID, buf: *mut u8, size: u32, written: *mut u32) -> HRESULT;
    21 fn SetUINT32(key: *const GUID, v: u32) -> HRESULT;
    22 fn SetUINT64(key: *const GUID, v: u64) -> HRESULT;
    24 fn SetGUID(key: *const GUID, v: *const GUID) -> HRESULT;
}

com_interface! {
    pub struct IMFMediaType(0x44ae0fa8_ea31_4109_8d2e_4cae4997c555);
    7 fn GetUINT32(key: *const GUID, out: *mut u32) -> HRESULT;
    8 fn GetUINT64(key: *const GUID, out: *mut u64) -> HRESULT;
    10 fn GetGUID(key: *const GUID, out: *mut GUID) -> HRESULT;
    14 fn GetBlobSize(key: *const GUID, size: *mut u32) -> HRESULT;
    15 fn GetBlob(key: *const GUID, buf: *mut u8, size: u32, written: *mut u32) -> HRESULT;
    21 fn SetUINT32(key: *const GUID, v: u32) -> HRESULT;
    22 fn SetUINT64(key: *const GUID, v: u64) -> HRESULT;
    24 fn SetGUID(key: *const GUID, v: *const GUID) -> HRESULT;
}

com_interface! {
    pub struct IMFSample(0xc40a00f2_b93a_4d80_ae8c_5a1c634f58e4);
    7 fn GetUINT32(key: *const GUID, out: *mut u32) -> HRESULT;
    21 fn SetUINT32(key: *const GUID, v: u32) -> HRESULT;
    35 fn GetSampleTime(t: *mut i64) -> HRESULT;
    36 fn SetSampleTime(t: i64) -> HRESULT;
    38 fn SetSampleDuration(d: i64) -> HRESULT;
    39 fn GetBufferCount(n: *mut u32) -> HRESULT;
    40 fn GetBufferByIndex(i: u32, out: *mut *mut c_void) -> HRESULT;
    41 fn ConvertToContiguousBuffer(out: *mut *mut c_void) -> HRESULT;
    42 fn AddBuffer(buf: *mut c_void) -> HRESULT;
}

com_interface! {
    pub struct IMFMediaBuffer(0x045fa593_8799_42b8_bc8d_8968c6453507);
    3 fn Lock(data: *mut *mut u8, max: *mut u32, cur: *mut u32) -> HRESULT;
    4 fn Unlock() -> HRESULT;
    5 fn GetCurrentLength(len: *mut u32) -> HRESULT;
    6 fn SetCurrentLength(len: u32) -> HRESULT;
}

com_interface! {
    pub struct IMFDXGIBuffer(0xe7174cfa_1c9e_48b1_8866_626226bfc258);
    3 fn GetResource(iid: *const GUID, out: *mut *mut c_void) -> HRESULT;
    4 fn GetSubresourceIndex(index: *mut u32) -> HRESULT;
}

com_interface! {
    pub struct IMFActivate(0x7fee9e9a_4a89_47a6_899c_b6a53a70fb67);
    7 fn GetUINT32(key: *const GUID, out: *mut u32) -> HRESULT;
    11 fn GetStringLength(key: *const GUID, len: *mut u32) -> HRESULT;
    12 fn GetString(key: *const GUID, buf: *mut u16, size: u32, len: *mut u32) -> HRESULT;
    15 fn GetBlob(key: *const GUID, buf: *mut u8, size: u32, written: *mut u32) -> HRESULT;
    33 fn ActivateObject(iid: *const GUID, out: *mut *mut c_void) -> HRESULT;
    34 fn ShutdownObject() -> HRESULT;
}

com_interface! {
    pub struct IMFTransform(0xbf94c121_5b05_4e6f_8000_ba598961414d);
    7 fn GetOutputStreamInfo(id: u32, info: *mut MFT_OUTPUT_STREAM_INFO) -> HRESULT;
    8 fn GetAttributes(out: *mut *mut c_void) -> HRESULT;
    13 fn GetInputAvailableType(id: u32, index: u32, out: *mut *mut c_void) -> HRESULT;
    14 fn GetOutputAvailableType(id: u32, index: u32, out: *mut *mut c_void) -> HRESULT;
    15 fn SetInputType(id: u32, t: *mut c_void, flags: u32) -> HRESULT;
    16 fn SetOutputType(id: u32, t: *mut c_void, flags: u32) -> HRESULT;
    18 fn GetOutputCurrentType(id: u32, out: *mut *mut c_void) -> HRESULT;
    23 fn ProcessMessage(msg: u32, param: usize) -> HRESULT;
    24 fn ProcessInput(id: u32, sample: *mut c_void, flags: u32) -> HRESULT;
    25 fn ProcessOutput(flags: u32, count: u32, buffers: *mut MFT_OUTPUT_DATA_BUFFER, status: *mut u32) -> HRESULT;
}

com_interface! {
    pub struct IMFMediaEventGenerator(0x2cd0bd52_bcd5_4b89_b62c_eadc0c031e7d);
    3 fn GetEvent(flags: u32, out: *mut *mut c_void) -> HRESULT;
}

com_interface! {
    pub struct IMFMediaEvent(0xdf598932_f10c_4e39_bba2_c308f101daa3);
    33 fn GetType(t: *mut u32) -> HRESULT;
    35 fn GetStatus(hr: *mut HRESULT) -> HRESULT;
}

com_interface! {
    pub struct IMFDXGIDeviceManager(0xeb533d5d_2db6_40f8_97a9_494692014f07);
    7 fn ResetDevice(device: *mut c_void, token: u32) -> HRESULT;
}

com_interface! {
    pub struct ICodecAPI(0x901db4c7_31ce_41a2_85dc_8fa0bf41b8da);
    3 fn IsSupported(api: *const GUID) -> HRESULT;
    9 fn SetValue(api: *const GUID, value: *mut VARIANT) -> HRESULT;
}

// ---------------------------------------------------------------------------
// mfplat.dll

crate::dylib! {
    pub struct MfPlat ["mfplat.dll"] {
        fn MFStartup(u32, u32) -> HRESULT;
        fn MFCreateMediaType(*mut *mut c_void) -> HRESULT;
        fn MFCreateSample(*mut *mut c_void) -> HRESULT;
        fn MFCreateMemoryBuffer(u32, *mut *mut c_void) -> HRESULT;
        fn MFCreateDXGISurfaceBuffer(*const GUID, *mut c_void, u32, BOOL, *mut *mut c_void) -> HRESULT;
        fn MFCreateDXGIDeviceManager(*mut u32, *mut *mut c_void) -> HRESULT;
        fn MFTEnumEx(
            GUID,
            u32,
            *const MFT_REGISTER_TYPE_INFO,
            *const MFT_REGISTER_TYPE_INFO,
            *mut *mut *mut c_void,
            *mut u32,
        ) -> HRESULT;
    }
}

/// mfplat.dll, started once per process.
pub fn api() -> Result<&'static MfPlat, String> {
    static API: std::sync::OnceLock<Result<MfPlat, String>> = std::sync::OnceLock::new();
    API.get_or_init(|| {
        let mf = MfPlat::load().map_err(|e| {
            format!("{e}: Media Foundation is missing (Windows N edition? install the Media Feature Pack)")
        })?;
        // SAFETY: plain initialization call; never shut down (lives as long as the process).
        sys::check(unsafe { (mf.MFStartup)(MF_VERSION, MFSTARTUP_LITE) }, "MFStartup")?;
        Ok(mf)
    })
    .as_ref()
    .map_err(|e| e.clone())
}

pub fn media_type() -> Result<IMFMediaType, String> {
    let mf = api()?;
    com::out(|p| unsafe { (mf.MFCreateMediaType)(p) }, "MFCreateMediaType")
}

pub fn pack(hi: u32, lo: u32) -> u64 {
    (hi as u64) << 32 | lo as u64
}

/// One registered transform, as listed by `MFTEnumEx`.
pub struct MftInfo {
    pub activate: IMFActivate,
    pub name: String,
    /// "VEN_10DE" style vendor id for hardware MFTs.
    pub vendor: Option<u32>,
    pub hardware: bool,
}

fn string_attr(a: &IMFActivate, key: &GUID) -> Option<String> {
    let mut len = 0u32;
    // SAFETY: valid out pointers / buffer sizes.
    unsafe {
        if a.GetStringLength(key, &mut len) < 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize + 1];
        if a.GetString(key, buf.as_mut_ptr(), buf.len() as u32, &mut len) < 0 {
            return None;
        }
        Some(sys::from_wide(&buf))
    }
}

/// Transforms of `category` converting `input` → `output`, best first
/// (hardware before software, as MFTEnumEx sorts them).
pub fn enumerate(
    category: GUID,
    flags: u32,
    input: Option<GUID>,
    output: Option<GUID>,
) -> Result<Vec<MftInfo>, String> {
    let mf = api()?;
    let info = |g: GUID| MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: g };
    let (i, o) = (input.map(info), output.map(info));
    let mut list: *mut *mut c_void = std::ptr::null_mut();
    let mut n = 0u32;
    // SAFETY: valid in/out pointers; the returned array is freed below.
    let hr = unsafe {
        (mf.MFTEnumEx)(
            category,
            flags,
            i.as_ref().map_or(std::ptr::null(), |x| x as *const _),
            o.as_ref().map_or(std::ptr::null(), |x| x as *const _),
            &mut list,
            &mut n,
        )
    };
    sys::check(hr, "MFTEnumEx")?;
    let mut out = Vec::new();
    for k in 0..n as usize {
        // SAFETY: MFTEnumEx returned `n` owned IMFActivate pointers.
        let Some(a) = (unsafe { IMFActivate::from_raw(*list.add(k)) }) else { continue };
        let name = string_attr(&a, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_else(|| "unnamed MFT".into());
        let vendor = string_attr(&a, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute)
            .and_then(|v| u32::from_str_radix(v.trim_start_matches("VEN_"), 16).ok());
        let hardware = vendor.is_some();
        out.push(MftInfo { activate: a, name, vendor, hardware });
    }
    if !list.is_null() {
        // SAFETY: the array itself is CoTaskMemAlloc'ed.
        unsafe { sys::CoTaskMemFree(list.cast()) };
    }
    Ok(out)
}

/// A D3D11 device manager for `device` (shared with an MFT).
pub fn device_manager(device: &crate::d3d::ID3D11Device) -> Result<IMFDXGIDeviceManager, String> {
    let mf = api()?;
    let mut token = 0u32;
    let m: IMFDXGIDeviceManager =
        com::out(|p| unsafe { (mf.MFCreateDXGIDeviceManager)(&mut token, p) }, "MFCreateDXGIDeviceManager")?;
    // SAFETY: valid device and the token from creation.
    sys::check(unsafe { m.ResetDevice(device.as_raw(), token) }, "IMFDXGIDeviceManager::ResetDevice")?;
    Ok(m)
}

/// A media sample holding one D3D11 texture (subresource 0).
pub fn texture_sample(
    texture: &crate::d3d::ID3D11Texture2D,
    time_100ns: i64,
    duration: i64,
) -> Result<IMFSample, String> {
    let mf = api()?;
    let buf: IMFMediaBuffer = com::out(
        |p| unsafe { (mf.MFCreateDXGISurfaceBuffer)(&crate::d3d::ID3D11Texture2D::IID, texture.as_raw(), 0, 0, p) },
        "MFCreateDXGISurfaceBuffer",
    )?;
    let sample: IMFSample = com::out(|p| unsafe { (mf.MFCreateSample)(p) }, "MFCreateSample")?;
    // SAFETY: valid objects.
    unsafe {
        sys::check(sample.AddBuffer(buf.as_raw()), "IMFSample::AddBuffer")?;
        sample.SetSampleTime(time_100ns);
        sample.SetSampleDuration(duration);
    }
    Ok(sample)
}

/// A media sample with a copy of `data` in system memory.
pub fn memory_sample(data: &[u8], time_100ns: i64) -> Result<IMFSample, String> {
    let mf = api()?;
    let buf: IMFMediaBuffer =
        com::out(|p| unsafe { (mf.MFCreateMemoryBuffer)(data.len() as u32, p) }, "MFCreateMemoryBuffer")?;
    // SAFETY: the buffer has room for `data`; locked while we copy.
    unsafe {
        let (mut ptr, mut max, mut cur) = (std::ptr::null_mut(), 0u32, 0u32);
        sys::check(buf.Lock(&mut ptr, &mut max, &mut cur), "IMFMediaBuffer::Lock")?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buf.Unlock();
        buf.SetCurrentLength(data.len() as u32);
    }
    let sample: IMFSample = com::out(|p| unsafe { (mf.MFCreateSample)(p) }, "MFCreateSample")?;
    // SAFETY: valid objects.
    unsafe {
        sys::check(sample.AddBuffer(buf.as_raw()), "IMFSample::AddBuffer")?;
        sample.SetSampleTime(time_100ns);
    }
    Ok(sample)
}

/// Copy a sample's bytes out (system-memory samples: encoder output).
pub fn sample_bytes(sample: &IMFSample, out: &mut Vec<u8>) -> Result<(), String> {
    let buf: IMFMediaBuffer =
        com::out(|p| unsafe { sample.ConvertToContiguousBuffer(p) }, "ConvertToContiguousBuffer")?;
    // SAFETY: lock/unlock pair; `cur` bytes are readable while locked.
    unsafe {
        let (mut ptr, mut max, mut cur) = (std::ptr::null_mut(), 0u32, 0u32);
        sys::check(buf.Lock(&mut ptr, &mut max, &mut cur), "IMFMediaBuffer::Lock")?;
        out.extend_from_slice(std::slice::from_raw_parts(ptr, cur as usize));
        buf.Unlock();
    }
    Ok(())
}

/// Blob attribute of a media type.
pub fn blob(t: &IMFMediaType, key: &GUID) -> Option<Vec<u8>> {
    let mut size = 0u32;
    // SAFETY: valid out pointers / buffer.
    unsafe {
        if t.GetBlobSize(key, &mut size) < 0 || size == 0 {
            return None;
        }
        let mut v = vec![0u8; size as usize];
        if t.GetBlob(key, v.as_mut_ptr(), size, &mut size) < 0 {
            return None;
        }
        v.truncate(size as usize);
        Some(v)
    }
}

/// A transform being driven: synchronous MFTs directly, asynchronous
/// (hardware) MFTs through their event queue.
pub struct Mft {
    pub transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    pub name: String,
    pub hardware: bool,
    /// Async: inputs the MFT has asked for and we haven't given yet.
    need_input: u32,
    /// Async: outputs announced and not yet collected.
    have_output: u32,
    provides_samples: bool,
    activate: IMFActivate,
}

impl Mft {
    pub fn activate(info: &MftInfo) -> Result<Mft, String> {
        let transform: IMFTransform =
            com::out(|p| unsafe { info.activate.ActivateObject(&IMFTransform::IID, p) }, "ActivateObject")?;
        let attrs: Option<IMFAttributes> = com::out(|p| unsafe { transform.GetAttributes(p) }, "GetAttributes").ok();
        let mut is_async = 0u32;
        if let Some(a) = &attrs {
            // SAFETY: valid out pointer.
            unsafe { a.GetUINT32(&MF_TRANSFORM_ASYNC, &mut is_async) };
        }
        let events = if is_async != 0 {
            let a = attrs.as_ref().ok_or("async MFT without attributes")?;
            // SAFETY: plain attribute setter (required before using an async MFT).
            sys::check(unsafe { a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }, "MF_TRANSFORM_ASYNC_UNLOCK")?;
            Some(transform.cast::<IMFMediaEventGenerator>().map_err(|hr| sys::hr_error("IMFMediaEventGenerator", hr))?)
        } else {
            None
        };
        if let Some(a) = &attrs {
            // SAFETY: plain attribute setter; ignored by MFTs that don't know it.
            unsafe { a.SetUINT32(&MF_LOW_LATENCY, 1) };
        }
        Ok(Mft {
            transform,
            events,
            name: info.name.clone(),
            hardware: info.hardware,
            need_input: 0,
            have_output: 0,
            provides_samples: false,
            activate: info.activate.clone(),
        })
    }

    pub fn is_async(&self) -> bool {
        self.events.is_some()
    }

    pub fn attributes(&self) -> Option<IMFAttributes> {
        com::out(|p| unsafe { self.transform.GetAttributes(p) }, "GetAttributes").ok()
    }

    pub fn codec_api(&self) -> Option<ICodecAPI> {
        self.transform.cast::<ICodecAPI>().ok()
    }

    /// Give the transform our D3D11 device. Err if it isn't D3D11-aware.
    pub fn set_d3d_manager(&self, manager: &IMFDXGIDeviceManager) -> Result<(), String> {
        let mut aware = 0u32;
        if let Some(a) = self.attributes() {
            // SAFETY: valid out pointer.
            unsafe { a.GetUINT32(&MF_SA_D3D11_AWARE, &mut aware) };
        }
        if aware == 0 && !self.is_async() {
            return Err(format!("{} is not Direct3D 11 aware", self.name));
        }
        // SAFETY: the manager outlives the transform (both owned by the caller).
        let hr = unsafe { self.transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) };
        sys::check(hr, &format!("{}: MFT_MESSAGE_SET_D3D_MANAGER", self.name))
    }

    pub fn set_input_type(&self, t: &IMFMediaType) -> Result<(), String> {
        // SAFETY: valid media type.
        sys::check(unsafe { self.transform.SetInputType(0, t.as_raw(), 0) }, &format!("{}: SetInputType", self.name))
    }

    pub fn set_output_type(&self, t: &IMFMediaType) -> Result<(), String> {
        // SAFETY: valid media type.
        sys::check(unsafe { self.transform.SetOutputType(0, t.as_raw(), 0) }, &format!("{}: SetOutputType", self.name))
    }

    /// Choose the first available output type with `subtype` (decoders: NV12).
    pub fn select_output(&self, subtype: &GUID) -> Result<IMFMediaType, String> {
        for i in 0.. {
            let t: IMFMediaType = match com::out(|p| unsafe { self.transform.GetOutputAvailableType(0, i, p) }, "") {
                Ok(t) => t,
                Err(_) => break,
            };
            let mut g = GUID::default();
            // SAFETY: valid out pointer.
            if unsafe { t.GetGUID(&MF_MT_SUBTYPE, &mut g) } >= 0 && g == *subtype {
                self.set_output_type(&t)?;
                return Ok(t);
            }
        }
        Err(format!("{} offers no NV12 output", self.name))
    }

    pub fn current_output(&self) -> Result<IMFMediaType, String> {
        com::out(|p| unsafe { self.transform.GetOutputCurrentType(0, p) }, "GetOutputCurrentType")
    }

    /// Begin streaming (after the types are set).
    pub fn start(&mut self) -> Result<(), String> {
        let mut info = MFT_OUTPUT_STREAM_INFO::default();
        // SAFETY: valid out pointer.
        if unsafe { self.transform.GetOutputStreamInfo(0, &mut info) } >= 0 {
            self.provides_samples =
                info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES) != 0;
        }
        // SAFETY: plain messages.
        unsafe {
            sys::check(self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0), "NOTIFY_BEGIN_STREAMING")?;
            sys::check(self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0), "NOTIFY_START_OF_STREAM")?;
        }
        Ok(())
    }

    /// Async: collect queued events without blocking; with `wait`, block up to that long for one.
    fn pump(&mut self, wait: Option<Duration>) -> Result<(), String> {
        let Some(ev) = &self.events else { return Ok(()) };
        let deadline = wait.map(|w| Instant::now() + w);
        loop {
            let mut p = std::ptr::null_mut();
            // SAFETY: valid out pointer; NO_WAIT never blocks.
            let hr = unsafe { ev.GetEvent(MF_EVENT_FLAG_NO_WAIT, &mut p) };
            if hr == MF_E_NO_EVENTS_AVAILABLE {
                match deadline {
                    Some(d) if Instant::now() < d => {
                        std::thread::sleep(Duration::from_micros(200));
                        continue;
                    }
                    _ => return Ok(()),
                }
            }
            sys::check(hr, &format!("{}: GetEvent", self.name))?;
            // SAFETY: an owned event.
            let Some(e) = (unsafe { IMFMediaEvent::from_raw(p) }) else { continue };
            let (mut kind, mut status) = (0u32, 0i32);
            // SAFETY: valid out pointers.
            unsafe {
                e.GetType(&mut kind);
                e.GetStatus(&mut status);
            }
            sys::check(status, &format!("{}: event {kind}", self.name))?;
            match kind {
                METransformNeedInput => self.need_input += 1,
                METransformHaveOutput => self.have_output += 1,
                _ => {}
            }
            // One event per call when waiting: let the caller react.
            if wait.is_some() {
                return Ok(());
            }
        }
    }

    /// Feed one sample. Async MFTs: waits (up to `timeout`) until the MFT asks for input.
    pub fn input(&mut self, sample: &IMFSample, timeout: Duration) -> Result<(), String> {
        if self.is_async() {
            let deadline = Instant::now() + timeout;
            self.pump(None)?;
            while self.need_input == 0 {
                if Instant::now() >= deadline {
                    return Err(format!("{}: timed out waiting for the encoder to accept input", self.name));
                }
                self.pump(Some(deadline - Instant::now()))?;
            }
            self.need_input -= 1;
        }
        // SAFETY: valid sample.
        let hr = unsafe { self.transform.ProcessInput(0, sample.as_raw(), 0) };
        sys::check(hr, &format!("{}: ProcessInput", self.name))
    }

    /// Collect one output sample if available. Async: waits up to `wait` for
    /// the MFT to announce one. `Ok(None)`: needs more input.
    /// `Err(MF_E_TRANSFORM_STREAM_CHANGE)` is reported as `Output::StreamChange`.
    pub fn output(&mut self, wait: Duration) -> Result<Output, String> {
        if self.is_async() {
            let deadline = Instant::now() + wait;
            self.pump(None)?;
            while self.have_output == 0 {
                if Instant::now() >= deadline {
                    return Ok(Output::NeedInput);
                }
                self.pump(Some(deadline - Instant::now()))?;
            }
            self.have_output -= 1;
        }
        let sample = if self.provides_samples || self.is_async() {
            None
        } else {
            // Software path (system memory output): allocate a buffer.
            let mut info = MFT_OUTPUT_STREAM_INFO::default();
            // SAFETY: valid out pointer.
            unsafe { self.transform.GetOutputStreamInfo(0, &mut info) };
            Some(memory_sample(&vec![0u8; info.cbSize.max(1 << 20) as usize], 0)?)
        };
        let mut db = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: sample.as_ref().map_or(std::ptr::null_mut(), |s| s.as_raw()),
            dwStatus: 0,
            pEvents: std::ptr::null_mut(),
        };
        let mut status = 0u32;
        // SAFETY: one output buffer descriptor; ownership of a returned sample/events passes to us.
        let hr = unsafe { self.transform.ProcessOutput(0, 1, &mut db, &mut status) };
        if !db.pEvents.is_null() {
            // SAFETY: owned IMFCollection we don't use.
            drop(unsafe { Com::from_raw(db.pEvents) });
        }
        let got = if sample.is_some() {
            // We passed our own sample; it is not AddRef'ed for us.
            sample
        } else {
            // SAFETY: the MFT allocated this sample and handed us its reference.
            unsafe { IMFSample::from_raw(db.pSample) }
        };
        match hr {
            MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Output::NeedInput),
            MF_E_TRANSFORM_STREAM_CHANGE => Ok(Output::StreamChange),
            hr if hr < 0 => Err(sys::hr_error(&format!("{}: ProcessOutput", self.name), hr)),
            _ => Ok(got.map_or(Output::NeedInput, Output::Sample)),
        }
    }

    pub fn flush(&mut self) {
        // SAFETY: plain message.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) };
        self.have_output = 0;
    }
}

impl Drop for Mft {
    fn drop(&mut self) {
        // SAFETY: plain messages; then release the MFT's resources.
        unsafe {
            self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            self.activate.ShutdownObject();
        }
    }
}

pub enum Output {
    Sample(IMFSample),
    NeedInput,
    StreamChange,
}
