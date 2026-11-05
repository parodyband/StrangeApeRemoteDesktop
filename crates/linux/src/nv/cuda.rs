//! CUDA driver API subset (loaded from libcuda.so.1).

use super::blob::Blob;
use super::layout as L;
use crate::dylib;
use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;

pub type CUresult = i32;
pub type CUdevice = i32;
pub type CUcontext = *mut c_void;
pub type CUstream = *mut c_void;
pub type CUdeviceptr = u64;
pub type CUarray = *mut c_void;
pub type CUgraphicsResource = *mut c_void;

pub const CU_GRAPHICS_REGISTER_FLAGS_NONE: u32 = 0;
pub const CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY: u32 = 1;
pub const CU_GRAPHICS_REGISTER_FLAGS_WRITE_DISCARD: u32 = 2;

dylib! {
    pub struct CudaApi ["libcuda.so.1", "libcuda.so"] {
        fn cuInit(u32) -> CUresult;
        fn cuDeviceGetCount(*mut i32) -> CUresult;
        fn cuDeviceGet(*mut CUdevice, i32) -> CUresult;
        fn cuDeviceGetName(*mut c_char, i32, CUdevice) -> CUresult;
        fn cuCtxCreate_v2(*mut CUcontext, u32, CUdevice) -> CUresult;
        fn cuCtxDestroy_v2(CUcontext) -> CUresult;
        fn cuCtxPushCurrent_v2(CUcontext) -> CUresult;
        fn cuCtxPopCurrent_v2(*mut CUcontext) -> CUresult;
        fn cuCtxSynchronize() -> CUresult;
        fn cuMemAlloc_v2(*mut CUdeviceptr, usize) -> CUresult;
        fn cuMemAllocPitch_v2(*mut CUdeviceptr, *mut usize, usize, usize, u32) -> CUresult;
        fn cuMemFree_v2(CUdeviceptr) -> CUresult;
        fn cuMemcpy2D_v2(*const c_void) -> CUresult;
        fn cuMemcpy2DAsync_v2(*const c_void, CUstream) -> CUresult;
        fn cuMemHostAlloc(*mut *mut c_void, usize, u32) -> CUresult;
        fn cuMemFreeHost(*mut c_void) -> CUresult;
        fn cuStreamCreate(*mut CUstream, u32) -> CUresult;
        fn cuStreamDestroy_v2(CUstream) -> CUresult;
        fn cuStreamSynchronize(CUstream) -> CUresult;
        fn cuGetErrorName(CUresult, *mut *const c_char) -> CUresult;
        fn cuGraphicsGLRegisterImage(*mut CUgraphicsResource, u32, u32, u32) -> CUresult;
        fn cuGraphicsUnregisterResource(CUgraphicsResource) -> CUresult;
        fn cuGraphicsMapResources(u32, *mut CUgraphicsResource, CUstream) -> CUresult;
        fn cuGraphicsUnmapResources(u32, *mut CUgraphicsResource, CUstream) -> CUresult;
        fn cuGraphicsSubResourceGetMappedArray(*mut CUarray, CUgraphicsResource, u32, u32) -> CUresult;
    }
}

/// A CUDA context on device 0 shared by the encoder/decoder/interop code.
pub struct Cuda {
    pub api: CudaApi,
    ctx: CUcontext,
    pub device_name: String,
}

// SAFETY: CUDA contexts can be pushed on any thread; we always push/pop.
unsafe impl Send for Cuda {}
unsafe impl Sync for Cuda {}

/// Pops the context when dropped.
pub struct CtxGuard<'a>(&'a Cuda);

impl Drop for CtxGuard<'_> {
    fn drop(&mut self) {
        let mut c = std::ptr::null_mut();
        // SAFETY: we pushed this context in `Cuda::push`.
        unsafe { (self.0.api.cuCtxPopCurrent_v2)(&mut c) };
    }
}

pub type Result<T> = std::result::Result<T, String>;

/// Where a 2D copy reads from / writes to.
#[derive(Clone, Copy)]
pub enum Mem {
    Host(*const u8),
    HostMut(*mut u8),
    Device(CUdeviceptr),
    Array(CUarray),
}

impl Cuda {
    pub fn new() -> Result<Arc<Self>> {
        let api = CudaApi::load()?;
        // SAFETY: plain driver API calls with valid out-pointers.
        unsafe {
            let r = (api.cuInit)(0);
            if r != 0 {
                return Err(format!("cuInit failed ({r}); is the NVIDIA driver loaded?"));
            }
            let mut n = 0;
            (api.cuDeviceGetCount)(&mut n);
            if n == 0 {
                return Err("no CUDA devices".into());
            }
            let mut dev = 0;
            (api.cuDeviceGet)(&mut dev, 0);
            let mut name = [0 as c_char; 128];
            (api.cuDeviceGetName)(name.as_mut_ptr(), 128, dev);
            let device_name = CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned();
            let mut ctx = std::ptr::null_mut();
            let r = (api.cuCtxCreate_v2)(&mut ctx, 0, dev);
            if r != 0 {
                return Err(format!("cuCtxCreate failed ({r})"));
            }
            let mut popped = std::ptr::null_mut();
            (api.cuCtxPopCurrent_v2)(&mut popped);
            Ok(Arc::new(Cuda { api, ctx, device_name }))
        }
    }

    pub fn ctx(&self) -> CUcontext {
        self.ctx
    }

    /// Make the context current on this thread for the guard's lifetime.
    pub fn push(&self) -> CtxGuard<'_> {
        // SAFETY: ctx is valid for our lifetime.
        unsafe { (self.api.cuCtxPushCurrent_v2)(self.ctx) };
        CtxGuard(self)
    }

    pub fn check(&self, r: CUresult, what: &str) -> Result<()> {
        if r == 0 {
            return Ok(());
        }
        let mut name: *const c_char = std::ptr::null();
        // SAFETY: valid out pointer; returned string is static.
        unsafe { (self.api.cuGetErrorName)(r, &mut name) };
        let n = if name.is_null() { "?".into() } else { unsafe { CStr::from_ptr(name) }.to_string_lossy() };
        Err(format!("{what} failed: {n} ({r})"))
    }

    /// Allocate pitched device memory. Context must be current.
    pub fn alloc_pitch(&self, width_bytes: usize, height: usize) -> Result<(CUdeviceptr, usize)> {
        let mut p = 0;
        let mut pitch = 0;
        // SAFETY: valid out pointers.
        let r = unsafe { (self.api.cuMemAllocPitch_v2)(&mut p, &mut pitch, width_bytes, height, 16) };
        self.check(r, "cuMemAllocPitch")?;
        Ok((p, pitch))
    }

    pub fn free(&self, p: CUdeviceptr) {
        if p != 0 {
            // SAFETY: p came from cuMemAlloc*.
            unsafe { (self.api.cuMemFree_v2)(p) };
        }
    }

    /// 2D copy (synchronous if `stream` is null). Context must be current.
    #[allow(clippy::too_many_arguments)]
    pub fn copy_2d(
        &self,
        src: Mem,
        src_pitch: usize,
        dst: Mem,
        dst_pitch: usize,
        width_bytes: usize,
        height: usize,
        stream: CUstream,
    ) -> Result<()> {
        let mut b = Blob::new(L::MEMCPY2D_SIZE);
        let put = |b: &mut Blob, m: Mem, ty, host, dev, arr| match m {
            Mem::Host(p) => {
                b.set(ty, L::CU_MEMORYTYPE_HOST as u64);
                b.set_ptr(host, p);
            }
            Mem::HostMut(p) => {
                b.set(ty, L::CU_MEMORYTYPE_HOST as u64);
                b.set_ptr(host, p);
            }
            Mem::Device(d) => {
                b.set(ty, L::CU_MEMORYTYPE_DEVICE as u64);
                b.set(dev, d);
            }
            Mem::Array(a) => {
                b.set(ty, L::CU_MEMORYTYPE_ARRAY as u64);
                b.set_ptr(arr, a);
            }
        };
        put(&mut b, src, L::MC_SRC_MEMORY_TYPE, L::MC_SRC_HOST, L::MC_SRC_DEVICE, L::MC_SRC_ARRAY);
        put(&mut b, dst, L::MC_DST_MEMORY_TYPE, L::MC_DST_HOST, L::MC_DST_DEVICE, L::MC_DST_ARRAY);
        b.set(L::MC_SRC_PITCH, src_pitch as u64);
        b.set(L::MC_DST_PITCH, dst_pitch as u64);
        b.set(L::MC_WIDTH_IN_BYTES, width_bytes as u64);
        b.set(L::MC_HEIGHT, height as u64);
        // SAFETY: descriptor is fully initialized; memory validity is the caller's contract.
        let r = unsafe {
            if stream.is_null() {
                (self.api.cuMemcpy2D_v2)(b.ptr())
            } else {
                (self.api.cuMemcpy2DAsync_v2)(b.ptr(), stream)
            }
        };
        self.check(r, "cuMemcpy2D")
    }
}

impl Drop for Cuda {
    fn drop(&mut self) {
        // SAFETY: we created the context.
        unsafe { (self.api.cuCtxDestroy_v2)(self.ctx) };
    }
}
