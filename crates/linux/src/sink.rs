//! Client-side video output on Linux: NVDEC → CUDA/GL interop → OpenGL.
//!
//! Decoded NV12 (or YUV444) surfaces are copied GPU-to-GPU into GL textures
//! registered with CUDA, converted to RGB in a fragment shader (BT.709,
//! limited or full range) and presented with swap interval 0.

use crate::gl::egl::{EglContext, Target};
use crate::gl::{self, FULLSCREEN_VS, Gl};
use crate::nv::cuda::{CU_GRAPHICS_REGISTER_FLAGS_WRITE_DISCARD, CUarray, CUgraphicsResource, Cuda, Mem};
use crate::nv::nvdec::{MappedFrame, NvDecoder};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use sard_core::media::{self, PresentStats, VideoSink, letterbox};
use sard_core::proto::Codec;
use std::sync::Arc;
use std::time::Instant;

const YUV_FS: &str = r#"#version 330 core
in vec2 uv;
out vec4 color;
uniform sampler2D t0;
uniform sampler2D t1;
uniform sampler2D t2;
uniform int planar444;
uniform int full_range;
void main() {
    float y = texture(t0, uv).r;
    vec2 c = planar444 != 0 ? vec2(texture(t1, uv).r, texture(t2, uv).r) : texture(t1, uv).rg;
    if (full_range == 0) {
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        c = (c - 128.0 / 255.0) * (255.0 / 224.0);
    } else {
        c -= 0.5;
    }
    color = vec4(y + 1.5748 * c.y, y - 0.1873 * c.x - 0.4681 * c.y, y + 1.8556 * c.x, 1.0);
}
"#;

struct GlState {
    gl: Gl,
    program: u32,
    vao: u32,
    u_planar: i32,
    u_full: i32,
}

struct Textures {
    w: u32,
    h: u32,
    yuv444: bool,
    tex: Vec<u32>,
    res: Vec<CUgraphicsResource>,
}

pub struct GlVideoSink {
    egl: EglContext,
    cuda: Arc<Cuda>,
    gls: Option<GlState>,
    decoder: Option<(Codec, NvDecoder)>,
    textures: Option<Textures>,
    win: (u32, u32),
    full_range: bool,
    has_frame: bool,
    error: Option<String>,
}

// SAFETY: moved to the render thread once and used only there.
unsafe impl Send for GlVideoSink {}

impl GlVideoSink {
    pub fn new(display: RawDisplayHandle, window: RawWindowHandle, width: u32, height: u32) -> media::Result<Self> {
        let target = match (display, window) {
            (RawDisplayHandle::Wayland(d), RawWindowHandle::Wayland(w)) => {
                Target::Wayland { display: d.display.as_ptr(), surface: w.surface.as_ptr() }
            }
            (RawDisplayHandle::Xlib(d), RawWindowHandle::Xlib(w)) => {
                Target::Xlib { display: d.display.map_or(std::ptr::null_mut(), |p| p.as_ptr()), window: w.window }
            }
            (d, w) => {
                return Err(media::err(format!("unsupported window system: {d:?} / {w:?}")));
            }
        };
        let egl = EglContext::new(target, width, height)?;
        let cuda = Cuda::new()?;
        Ok(GlVideoSink {
            egl,
            cuda,
            gls: None,
            decoder: None,
            textures: None,
            win: (width, height),
            full_range: false,
            has_frame: false,
            error: None,
        })
    }

    /// First use on the render thread: make the context current and build GL objects.
    fn ensure_gl(&mut self) -> media::Result<()> {
        if self.gls.is_some() {
            return Ok(());
        }
        self.egl.make_current()?;
        self.egl.set_swap_interval(0);
        let gl = Gl::load(&self.egl)?;
        let program = gl.program(FULLSCREEN_VS, YUV_FS)?;
        let mut vao = 0;
        // SAFETY: context current.
        unsafe {
            (gl.glGenVertexArrays)(1, &mut vao);
            (gl.glUseProgram)(program);
            (gl.glUniform1i)(gl.uniform(program, "t0"), 0);
            (gl.glUniform1i)(gl.uniform(program, "t1"), 1);
            (gl.glUniform1i)(gl.uniform(program, "t2"), 2);
        }
        sard_core::info!("OpenGL: {} / {}", gl.string(gl::RENDERER), gl.string(gl::VERSION));
        let u_planar = gl.uniform(program, "planar444");
        let u_full = gl.uniform(program, "full_range");
        self.gls = Some(GlState { gl, program, vao, u_planar, u_full });
        Ok(())
    }

    fn release_textures(&mut self) {
        if let (Some(t), Some(g)) = (self.textures.take(), &self.gls) {
            let _c = self.cuda.push();
            for r in &t.res {
                // SAFETY: registered by us.
                unsafe { (self.cuda.api.cuGraphicsUnregisterResource)(*r) };
            }
            // SAFETY: textures created by us; context current on this thread.
            unsafe { (g.gl.glDeleteTextures)(t.tex.len() as i32, t.tex.as_ptr()) };
        }
    }

    fn ensure_textures(&mut self, w: u32, h: u32, yuv444: bool) -> media::Result<()> {
        if let Some(t) = &self.textures
            && t.w == w
            && t.h == h
            && t.yuv444 == yuv444
        {
            return Ok(());
        }
        self.release_textures();
        let g = self.gls.as_ref().unwrap();
        let tex: Vec<u32> = if yuv444 {
            (0..3).map(|_| g.gl.texture(gl::R8, w, h, gl::RED, gl::LINEAR)).collect()
        } else {
            vec![
                g.gl.texture(gl::R8, w, h, gl::RED, gl::LINEAR),
                g.gl.texture(gl::RG8, w / 2, h / 2, gl::RG, gl::LINEAR),
            ]
        };
        let _c = self.cuda.push();
        let mut res = Vec::new();
        for &t in &tex {
            let mut r = std::ptr::null_mut();
            // SAFETY: valid GL texture in the current context.
            let rc = unsafe {
                (self.cuda.api.cuGraphicsGLRegisterImage)(
                    &mut r,
                    t,
                    gl::TEXTURE_2D,
                    CU_GRAPHICS_REGISTER_FLAGS_WRITE_DISCARD,
                )
            };
            self.cuda.check(rc, "cuGraphicsGLRegisterImage")?;
            res.push(r);
        }
        self.textures = Some(Textures { w, h, yuv444, tex, res });
        Ok(())
    }

    /// Copy a mapped NVDEC surface into the GL textures (GPU to GPU).
    fn upload(&mut self, f: &MappedFrame) -> media::Result<()> {
        self.ensure_textures(f.width, f.height, f.yuv444)?;
        self.full_range = f.full_range;
        let t = self.textures.as_mut().unwrap();
        let cuda = &self.cuda;
        // SAFETY: resources registered in this context; arrays valid while mapped.
        unsafe {
            let n = t.res.len() as u32;
            cuda.check((cuda.api.cuGraphicsMapResources)(n, t.res.as_mut_ptr(), std::ptr::null_mut()), "map")?;
            let mut arrays: Vec<CUarray> = Vec::new();
            for r in &t.res {
                let mut a = std::ptr::null_mut();
                cuda.check((cuda.api.cuGraphicsSubResourceGetMappedArray)(&mut a, *r, 0, 0), "mapped array")?;
                arrays.push(a);
            }
            let plane = f.pitch as u64 * ((f.surface_height as u64 + 1) & !1);
            let (w, h) = (f.width as usize, f.height as usize);
            let res = (|| -> Result<(), String> {
                cuda.copy_2d(Mem::Device(f.dptr), f.pitch, Mem::Array(arrays[0]), 0, w, h, std::ptr::null_mut())?;
                if t.yuv444 {
                    cuda.copy_2d(
                        Mem::Device(f.dptr + plane),
                        f.pitch,
                        Mem::Array(arrays[1]),
                        0,
                        w,
                        h,
                        std::ptr::null_mut(),
                    )?;
                    cuda.copy_2d(
                        Mem::Device(f.dptr + 2 * plane),
                        f.pitch,
                        Mem::Array(arrays[2]),
                        0,
                        w,
                        h,
                        std::ptr::null_mut(),
                    )?;
                } else {
                    cuda.copy_2d(
                        Mem::Device(f.dptr + plane),
                        f.pitch,
                        Mem::Array(arrays[1]),
                        0,
                        w / 2 * 2,
                        h / 2,
                        std::ptr::null_mut(),
                    )?;
                }
                Ok(())
            })();
            (cuda.api.cuGraphicsUnmapResources)(n, t.res.as_mut_ptr(), std::ptr::null_mut());
            res?;
        }
        Ok(())
    }

    fn draw(&mut self) -> media::Result<()> {
        let Some(g) = &self.gls else { return Ok(()) };
        let gl = &g.gl;
        let (ww, wh) = self.win;
        // SAFETY: context current; objects valid.
        unsafe {
            (gl.glViewport)(0, 0, ww as i32, wh as i32);
            (gl.glClearColor)(0.0, 0.0, 0.0, 1.0);
            (gl.glClear)(gl::COLOR_BUFFER_BIT);
            if let Some(t) = &self.textures {
                let (x, y, w, h) = letterbox(ww, wh, t.w, t.h);
                // GL's origin is bottom-left.
                (gl.glViewport)(
                    x.round() as i32,
                    (wh as f32 - y - h).round() as i32,
                    w.round() as i32,
                    h.round() as i32,
                );
                (gl.glUseProgram)(g.program);
                (gl.glUniform1i)(g.u_planar, t.yuv444 as i32);
                (gl.glUniform1i)(g.u_full, self.full_range as i32);
                for (i, &tex) in t.tex.iter().enumerate() {
                    (gl.glActiveTexture)(gl::TEXTURE0 + i as u32);
                    (gl.glBindTexture)(gl::TEXTURE_2D, tex);
                }
                (gl.glBindVertexArray)(g.vao);
                (gl.glDrawArrays)(gl::TRIANGLE_STRIP, 0, 4);
            }
        }
        self.egl.swap()?;
        Ok(())
    }
}

impl VideoSink for GlVideoSink {
    fn present(&mut self, codec: Codec, data: &[u8], _keyframe: bool) -> media::Result<PresentStats> {
        self.ensure_gl()?;
        if self.decoder.as_ref().is_none_or(|(c, _)| *c != codec) {
            self.decoder = Some((codec, NvDecoder::new(Arc::clone(&self.cuda), codec)?));
        }
        let t0 = Instant::now();
        let mut dec = self.decoder.take().unwrap();
        let mut got = false;
        let r = dec.1.decode(data, 0, |f| {
            got = true;
            if let Err(e) = self.upload(f) {
                self.error = Some(e.to_string());
            }
        });
        self.decoder = Some(dec);
        r?;
        if let Some(e) = self.error.take() {
            return Err(media::err(e));
        }
        let decode_us = t0.elapsed().as_micros() as u32;
        if !got {
            return Ok(PresentStats { decode_us, present_us: 0 });
        }
        self.has_frame = true;
        let t1 = Instant::now();
        self.draw()?;
        Ok(PresentStats { decode_us, present_us: t1.elapsed().as_micros() as u32 })
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.win = (width.max(1), height.max(1));
        self.egl.resize(self.win.0, self.win.1);
    }

    fn redraw(&mut self) -> media::Result<()> {
        if self.has_frame {
            self.ensure_gl()?;
            self.draw()?;
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("NVDEC on {} → OpenGL", self.cuda.device_name)
    }
}

impl Drop for GlVideoSink {
    fn drop(&mut self) {
        self.decoder = None;
        self.release_textures();
    }
}
