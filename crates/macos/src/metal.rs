//! Metal presentation of decoded NV12 frames.
//!
//! Decoded IOSurface-backed `CVPixelBuffer`s are wrapped as Metal textures
//! through a `CVMetalTextureCache` (zero copy), converted BT.709 limited range
//! → RGB in a fragment shader, letterboxed, and presented on a `CAMetalLayer`
//! with display sync off.

use crate::cf::{self, CfRef, kCFAllocatorDefault};
use crate::ffi::{self, CGSize, CVMetalTextureRef};
use crate::objc::{error_string, ns_string, to_string};
use block2::RcBlock;
use objc2::encode::{Encode, Encoding};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use sard_core::media::letterbox;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::Mutex;

pub type Result<T> = std::result::Result<T, String>;

const MTL_PIXEL_FORMAT_R8_UNORM: usize = 10;
const MTL_PIXEL_FORMAT_RG8_UNORM: usize = 30;
pub const MTL_PIXEL_FORMAT_BGRA8_UNORM: usize = 80;
const MTL_LOAD_ACTION_CLEAR: usize = 2;
const MTL_STORE_ACTION_STORE: usize = 1;
const MTL_PRIMITIVE_TRIANGLE_STRIP: usize = 4;

const SHADER: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VOut { float4 pos [[position]]; float2 uv; };

// rect = (left, top, right, bottom) in NDC.
vertex VOut vs(uint vid [[vertex_id]], constant float4 &rect [[buffer(0)]]) {
    float2 uv = float2(float(vid & 1), float(vid >> 1));
    VOut o;
    o.pos = float4(mix(rect.x, rect.z, uv.x), mix(rect.y, rect.w, uv.y), 0.0, 1.0);
    o.uv = uv;
    return o;
}

// BT.709, limited (video) range Y'CbCr 4:2:0 -> R'G'B'.
fragment float4 fs(VOut in [[stage_in]],
                   texture2d<float> luma [[texture(0)]],
                   texture2d<float> chroma [[texture(1)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float y = (luma.sample(s, in.uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    float2 c = (chroma.sample(s, in.uv).rg - 128.0 / 255.0) * (255.0 / 224.0);
    float3 rgb = float3(y + 1.5748 * c.y, y - 0.1873 * c.x - 0.4681 * c.y, y + 1.8556 * c.x);
    return float4(saturate(rgb), 1.0);
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct MtlOrigin {
    x: usize,
    y: usize,
    z: usize,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct MtlSize {
    w: usize,
    h: usize,
    d: usize,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct MtlRegion {
    origin: MtlOrigin,
    size: MtlSize,
}
// SAFETY: layouts match MTLOrigin / MTLSize / MTLRegion (anonymous typedef structs).
unsafe impl Encode for MtlOrigin {
    const ENCODING: Encoding = Encoding::Struct("?", &[usize::ENCODING, usize::ENCODING, usize::ENCODING]);
}
unsafe impl Encode for MtlSize {
    const ENCODING: Encoding = Encoding::Struct("?", &[usize::ENCODING, usize::ENCODING, usize::ENCODING]);
}
unsafe impl Encode for MtlRegion {
    const ENCODING: Encoding = Encoding::Struct("?", &[MtlOrigin::ENCODING, MtlSize::ENCODING]);
}

/// Metal textures (Y and CbCr planes) aliasing one decoded pixel buffer.
#[derive(Clone)]
pub struct FrameTextures {
    y: CfRef,
    uv: CfRef,
    _buffer: CfRef,
    pub width: u32,
    pub height: u32,
}

impl FrameTextures {
    fn mtl(t: &CfRef) -> *mut AnyObject {
        // SAFETY: valid CVMetalTexture; texture is +0, alive as long as it.
        unsafe { ffi::CVMetalTextureGetTexture(t.as_ptr()) as *mut AnyObject }
    }
}

/// Device, pipeline and texture cache; independent of any window.
pub struct Renderer {
    pub device: Retained<AnyObject>,
    queue: Retained<AnyObject>,
    pipeline: Retained<AnyObject>,
    cache: CfRef,
    pub device_name: String,
}

impl Renderer {
    pub fn new() -> Result<Self> {
        // SAFETY: standard Metal object creation; all calls on valid objects.
        unsafe {
            let device = Retained::from_raw(ffi::MTLCreateSystemDefaultDevice()).ok_or("no Metal device")?;
            let name_obj: *const AnyObject = msg_send![&*device, name];
            let device_name = to_string(name_obj);

            let src = ns_string(SHADER);
            let mut err: *mut AnyObject = null_mut();
            let lib: Option<Retained<AnyObject>> =
                msg_send![&*device, newLibraryWithSource: &*src, options: null_mut::<AnyObject>(), error: &mut err];
            let lib = lib.ok_or_else(|| format!("shader compile failed: {}", error_string(err)))?;
            let vs: Option<Retained<AnyObject>> = msg_send![&*lib, newFunctionWithName: &*ns_string("vs")];
            let fs: Option<Retained<AnyObject>> = msg_send![&*lib, newFunctionWithName: &*ns_string("fs")];
            let (vs, fs) = (vs.ok_or("no vs")?, fs.ok_or("no fs")?);

            let desc: Retained<AnyObject> = msg_send![class!(MTLRenderPipelineDescriptor), new];
            let _: () = msg_send![&*desc, setVertexFunction: &*vs];
            let _: () = msg_send![&*desc, setFragmentFunction: &*fs];
            let atts: *mut AnyObject = msg_send![&*desc, colorAttachments];
            let att0: *mut AnyObject = msg_send![atts, objectAtIndexedSubscript: 0usize];
            let _: () = msg_send![att0, setPixelFormat: MTL_PIXEL_FORMAT_BGRA8_UNORM];
            let mut err: *mut AnyObject = null_mut();
            let pipeline: Option<Retained<AnyObject>> =
                msg_send![&*device, newRenderPipelineStateWithDescriptor: &*desc, error: &mut err];
            let pipeline = pipeline.ok_or_else(|| format!("pipeline creation failed: {}", error_string(err)))?;

            let queue: Option<Retained<AnyObject>> = msg_send![&*device, newCommandQueue];
            let queue = queue.ok_or("newCommandQueue failed")?;

            let mut cache: ffi::CVMetalTextureCacheRef = null_mut();
            let st = ffi::CVMetalTextureCacheCreate(
                kCFAllocatorDefault,
                std::ptr::null(),
                Retained::as_ptr(&device) as *mut c_void,
                std::ptr::null(),
                &mut cache,
            );
            cf::check(st, "CVMetalTextureCacheCreate")?;
            let cache = CfRef::from_owned(cache).ok_or("null texture cache")?;
            Ok(Renderer { device, queue, pipeline, cache, device_name })
        }
    }

    /// Wrap a decoded NV12 pixel buffer as Metal textures (no copy).
    pub fn textures(&self, pb: &CfRef) -> Result<FrameTextures> {
        let p = pb.as_ptr();
        // SAFETY: valid pixel buffer and cache; outputs are +1.
        unsafe {
            if ffi::CVPixelBufferGetPixelFormatType(p) != ffi::kCVPixelFormatType_420v {
                return Err("decoded buffer is not NV12 video range".into());
            }
            let plane = |i: usize, fmt: usize| -> Result<CfRef> {
                let (w, h) = (ffi::CVPixelBufferGetWidthOfPlane(p, i), ffi::CVPixelBufferGetHeightOfPlane(p, i));
                let mut t: CVMetalTextureRef = null_mut();
                let st = ffi::CVMetalTextureCacheCreateTextureFromImage(
                    kCFAllocatorDefault,
                    self.cache.as_ptr(),
                    p,
                    std::ptr::null(),
                    fmt,
                    w,
                    h,
                    i,
                    &mut t,
                );
                cf::check(st, "CVMetalTextureCacheCreateTextureFromImage")?;
                CfRef::from_owned(t).ok_or_else(|| "null metal texture".into())
            };
            Ok(FrameTextures {
                y: plane(0, MTL_PIXEL_FORMAT_R8_UNORM)?,
                uv: plane(1, MTL_PIXEL_FORMAT_RG8_UNORM)?,
                _buffer: pb.clone(),
                width: ffi::CVPixelBufferGetWidth(p) as u32,
                height: ffi::CVPixelBufferGetHeight(p) as u32,
            })
        }
    }

    /// Encode clear + letterboxed draw of `frame` into `target`
    /// (`id<MTLTexture>`, BGRA8). Returns the uncommitted command buffer;
    /// frame textures stay alive until the GPU has finished with them.
    ///
    /// # Safety
    /// `target` must be a valid BGRA8 render-target `id<MTLTexture>`; call
    /// inside an autorelease pool.
    pub unsafe fn encode_draw(
        &self,
        target: *mut AnyObject,
        frame: Option<&FrameTextures>,
    ) -> Result<Retained<AnyObject>> {
        // SAFETY: valid Metal objects; standard render pass encoding.
        unsafe {
            let tw: usize = msg_send![target, width];
            let th: usize = msg_send![target, height];
            let pass: Retained<AnyObject> = msg_send![class!(MTLRenderPassDescriptor), renderPassDescriptor];
            let atts: *mut AnyObject = msg_send![&*pass, colorAttachments];
            let att0: *mut AnyObject = msg_send![atts, objectAtIndexedSubscript: 0usize];
            let _: () = msg_send![att0, setTexture: target];
            let _: () = msg_send![att0, setLoadAction: MTL_LOAD_ACTION_CLEAR];
            let _: () = msg_send![att0, setStoreAction: MTL_STORE_ACTION_STORE];
            let cb: Option<Retained<AnyObject>> = msg_send![&*self.queue, commandBuffer];
            let cb = cb.ok_or("commandBuffer failed")?;
            let enc: Option<Retained<AnyObject>> = msg_send![&*cb, renderCommandEncoderWithDescriptor: &*pass];
            let enc = enc.ok_or("renderCommandEncoder failed")?;
            if let Some(f) = frame {
                let (x, y, w, h) = letterbox(tw as u32, th as u32, f.width, f.height);
                let (tw, th) = (tw as f32, th as f32);
                let rect: [f32; 4] =
                    [x / tw * 2.0 - 1.0, 1.0 - y / th * 2.0, (x + w) / tw * 2.0 - 1.0, 1.0 - (y + h) / th * 2.0];
                let _: () = msg_send![&*enc, setRenderPipelineState: &*self.pipeline];
                let _: () =
                    msg_send![&*enc, setVertexBytes: rect.as_ptr() as *const c_void, length: 16usize, atIndex: 0usize];
                let _: () = msg_send![&*enc, setFragmentTexture: FrameTextures::mtl(&f.y), atIndex: 0usize];
                let _: () = msg_send![&*enc, setFragmentTexture: FrameTextures::mtl(&f.uv), atIndex: 1usize];
                let _: () = msg_send![&*enc, drawPrimitives: MTL_PRIMITIVE_TRIANGLE_STRIP, vertexStart: 0usize, vertexCount: 4usize];
                let hold = Mutex::new(Some(f.clone()));
                let done = RcBlock::new(move |_cb: *mut AnyObject| {
                    if let Ok(mut h) = hold.lock() {
                        h.take();
                    }
                });
                let _: () = msg_send![&*cb, addCompletedHandler: &*done];
            }
            let _: () = msg_send![&*enc, endEncoding];
            Ok(cb)
        }
    }

    /// Render `frame` into an offscreen BGRA texture and read it back
    /// (tests: verifies the zero-copy path and the colour conversion).
    pub fn render_offscreen(&self, frame: &FrameTextures, w: u32, h: u32) -> Result<Vec<u8>> {
        objc2::rc::autoreleasepool(|_| {
            // SAFETY: standard texture creation/readback on valid objects.
            unsafe {
                let td: Retained<AnyObject> = msg_send![
                    class!(MTLTextureDescriptor),
                    texture2DDescriptorWithPixelFormat: MTL_PIXEL_FORMAT_BGRA8_UNORM,
                    width: w as usize,
                    height: h as usize,
                    mipmapped: false
                ];
                let _: () = msg_send![&*td, setUsage: 5usize]; // ShaderRead | RenderTarget
                let _: () = msg_send![&*td, setStorageMode: 0usize]; // Shared
                let tex: Option<Retained<AnyObject>> = msg_send![&*self.device, newTextureWithDescriptor: &*td];
                let tex = tex.ok_or("newTextureWithDescriptor failed")?;
                let cb = self.encode_draw(Retained::as_ptr(&tex) as *mut AnyObject, Some(frame))?;
                let _: () = msg_send![&*cb, commit];
                let _: () = msg_send![&*cb, waitUntilCompleted];
                let mut out = vec![0u8; w as usize * h as usize * 4];
                let region = MtlRegion {
                    origin: MtlOrigin { x: 0, y: 0, z: 0 },
                    size: MtlSize { w: w as usize, h: h as usize, d: 1 },
                };
                let _: () = msg_send![
                    &*tex,
                    getBytes: out.as_mut_ptr() as *mut c_void,
                    bytesPerRow: w as usize * 4,
                    fromRegion: region,
                    mipmapLevel: 0usize
                ];
                Ok(out)
            }
        })
    }
}

/// Renderer bound to a window's `CAMetalLayer`.
pub struct Presenter {
    pub renderer: Renderer,
    layer: Retained<AnyObject>,
    current: Option<FrameTextures>,
}

impl Presenter {
    /// Attach a new `CAMetalLayer` to `view` (an `NSView`).
    ///
    /// # Safety
    /// Main thread only; `view` must be a valid `NSView`.
    pub unsafe fn new(view: *mut AnyObject, width: u32, height: u32) -> Result<Self> {
        let renderer = Renderer::new()?;
        // SAFETY: main thread (caller contract); standard layer setup.
        unsafe {
            let layer: Retained<AnyObject> = msg_send![class!(CAMetalLayer), new];
            let _: () = msg_send![&*layer, setDevice: &*renderer.device];
            let _: () = msg_send![&*layer, setPixelFormat: MTL_PIXEL_FORMAT_BGRA8_UNORM];
            let _: () = msg_send![&*layer, setFramebufferOnly: true];
            let _: () = msg_send![&*layer, setDisplaySyncEnabled: false];
            let _: () = msg_send![&*layer, setOpaque: true];
            // Decoded R'G'B' is sRGB-encoded desktop content; let the window
            // server colour-match it to the display.
            let cs = ffi::CGColorSpaceCreateWithName(ffi::kCGColorSpaceSRGB);
            if !cs.is_null() {
                let _: () = msg_send![&*layer, setColorspace: cs];
                ffi::CGColorSpaceRelease(cs);
            }
            let window: *mut AnyObject = msg_send![view, window];
            let scale: f64 = if window.is_null() { 1.0 } else { msg_send![window, backingScaleFactor] };
            let _: () = msg_send![&*layer, setContentsScale: scale];
            let size = CGSize { width: width.max(1) as f64, height: height.max(1) as f64 };
            let _: () = msg_send![&*layer, setDrawableSize: size];
            // Layer-hosting view: our layer becomes the view's backing layer.
            let _: () = msg_send![view, setLayer: &*layer];
            let _: () = msg_send![view, setWantsLayer: true];
            Ok(Presenter { renderer, layer, current: None })
        }
    }

    pub fn set_frame(&mut self, pb: &CfRef) -> Result<()> {
        self.current = Some(self.renderer.textures(pb)?);
        Ok(())
    }

    pub fn has_frame(&self) -> bool {
        self.current.is_some()
    }

    pub fn video_size(&self) -> Option<(u32, u32)> {
        self.current.as_ref().map(|f| (f.width, f.height))
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let size = CGSize { width: width.max(1) as f64, height: height.max(1) as f64 };
        // SAFETY: drawableSize may be set from any thread.
        unsafe {
            let _: () = msg_send![&*self.layer, setDrawableSize: size];
        }
        if sard_core::log::enabled(sard_core::log::DEBUG) {
            // SAFETY: plain property read.
            let b: ffi::CGRect = unsafe { msg_send![&*self.layer, bounds] };
            sard_core::debug!(
                "metal layer: drawable {width}x{height}, bounds {:.0}x{:.0} pt",
                b.size.width,
                b.size.height
            );
        }
    }

    /// Draw the current frame (or black) and present immediately.
    pub fn draw(&mut self) -> Result<()> {
        objc2::rc::autoreleasepool(|_| {
            // SAFETY: valid layer; drawable used within this pool.
            unsafe {
                let drawable: Option<Retained<AnyObject>> = msg_send![&*self.layer, nextDrawable];
                let Some(drawable) = drawable else {
                    return Err("no drawable available".to_string());
                };
                let target: *mut AnyObject = msg_send![&*drawable, texture];
                let cb = self.renderer.encode_draw(target, self.current.as_ref())?;
                let _: () = msg_send![&*cb, presentDrawable: &*drawable];
                let _: () = msg_send![&*cb, commit];
                Ok(())
            }
        })
    }
}
