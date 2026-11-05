//! Screen capture via xdg-desktop-portal + PipeWire, GPU colour conversion
//! and NVENC encode.
//!
//! ```text
//! PipeWire buffer (SHM / DMA-BUF) ─▶ GL texture ─▶ CSC shader (BT.709, scale)
//!   ─▶ R8 NV12 texture ─▶ CUDA (GL interop, GPU copy) ─▶ NVENC input ─▶ bitstream
//! ```

use crate::gl::egl::{EglContext, Target};
use crate::gl::{self, FULLSCREEN_VS, Gl};
use crate::nv::cuda::{CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY, CUgraphicsResource, Cuda, Mem};
use crate::nv::nvenc::{EncoderParams, NvEncoder};
use crate::portal::{self, PortalInput, Session};
use crate::pw::{self, Capture, FrameData, VideoFormat};
use sard_core::clock;
use sard_core::config;
use sard_core::input::wire_button_to_evdev;
use sard_core::media::{
    self, CursorMode, CursorShape, CursorUpdate, EncodedFrame, InputEvent, InputSink, SourceOptions, VideoSource,
};
use sard_core::proto::Codec;
use sard_core::{debug, info, warn};
use std::sync::Arc;
use std::time::Duration;

/// RGB → NV12 (BT.709 limited range) into an R8 target of size W x H*3/2.
const CSC_FS: &str = r#"#version 330 core
out float o;
uniform sampler2D src;
uniform vec2 out_size;
const vec3 KY = vec3(0.2126, 0.7152, 0.0722);
void main() {
    vec2 p = floor(gl_FragCoord.xy);
    if (p.y < out_size.y) {
        vec3 c = texture(src, (p + 0.5) / out_size).rgb;
        o = (16.0 + 219.0 * dot(c, KY)) / 255.0;
    } else {
        float row = p.y - out_size.y;
        float pair = floor(p.x * 0.5);
        vec3 c = texture(src, vec2(pair * 2.0 + 1.0, row * 2.0 + 1.0) / out_size).rgb;
        float y = dot(c, KY);
        float v = mod(p.x, 2.0) < 1.0 ? (c.b - y) / 1.8556 : (c.r - y) / 1.5748;
        o = (128.0 + 224.0 * v) / 255.0;
    }
}
"#;

const TOKEN_FILE: &str = "portal_restore_token";

struct Csc {
    gl: Gl,
    program: u32,
    vao: u32,
    u_size: i32,
    src: u32,
    src_size: (u32, u32),
    dst: u32,
    fbo: u32,
    dst_res: CUgraphicsResource,
}

pub struct PortalSource {
    // Field order = drop order: stream before session.
    capture: Capture,
    session: Session,
    egl: EglContext,
    cuda: Arc<Cuda>,
    csc: Option<Csc>,
    enc: NvEncoder,
    codec: Codec,
    stream_size: (u32, u32),
    out_size: (u32, u32),
    last_input: Option<usize>,
    force_key: bool,
    gl_current: bool,
    frames: u64,
}

// SAFETY: the source is created on the host thread and then used (and the GL
// context made current) only by the video thread.
unsafe impl Send for PortalSource {}

fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let s = (max_w as f64 / w as f64).min(max_h as f64 / h as f64).min(1.0);
    (((w as f64 * s) as u32) & !1, ((h as f64 * s) as u32) & !1)
}

pub fn open(opts: &SourceOptions) -> media::Result<(Box<dyn VideoSource>, Box<dyn InputSink>)> {
    let dir = config::config_dir();
    let token =
        std::fs::read_to_string(dir.join(TOKEN_FILE)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let cursor = match opts.cursor {
        CursorMode::Metadata => portal::cursor_mode::METADATA,
        CursorMode::Embedded => portal::cursor_mode::EMBEDDED,
    };
    let session = match Session::start(cursor, token.as_deref(), true) {
        Ok(s) => s,
        Err(e) if token.is_some() => {
            warn!("portal session with saved token failed ({e}); asking again");
            let _ = std::fs::remove_file(dir.join(TOKEN_FILE));
            Session::start(cursor, None, true)?
        }
        Err(e) => return Err(e.into()),
    };
    if let Some(t) = &session.restore_token {
        let _ = std::fs::create_dir_all(&dir);
        if let Err(e) = std::fs::write(dir.join(TOKEN_FILE), t) {
            warn!("could not save portal restore token: {e}");
        }
    }
    let input: Box<dyn InputSink> =
        if session.remote_input { Box::new(PortalSink { input: session.input() }) } else { Box::new(NullSink) };
    let fd = session.open_pipewire_remote()?;
    let node = session.streams[0].node_id;
    let capture = Capture::connect(fd, node, opts.fps, Vec::new())?;
    let format = capture.wait_format(Duration::from_secs(10))?;
    info!(
        "pipewire {}: {}x{} format {} ({})",
        pw::library_version().unwrap_or_default(),
        format.width,
        format.height,
        format.format,
        if format.modifier.is_some() { "dma-buf" } else { "shared memory" }
    );
    let stream_size = (format.width, format.height);
    let out_size = fit(format.width, format.height, opts.max_width, opts.max_height);
    let egl = EglContext::new(Target::Headless, 0, 0)?;
    let cuda = Cuda::new()?;
    let enc = NvEncoder::new(
        Arc::clone(&cuda),
        EncoderParams {
            codec: opts.codec,
            width: out_size.0,
            height: out_size.1,
            fps: opts.fps,
            bitrate_kbps: opts.bitrate_kbps.max(1000),
            yuv444: false,
            preset: opts.preset,
            vbv_frames: 1.0,
        },
    )?;
    let src = PortalSource {
        capture,
        session,
        egl,
        cuda,
        csc: None,
        enc,
        codec: opts.codec,
        stream_size,
        out_size,
        last_input: None,
        force_key: true,
        gl_current: false,
        frames: 0,
    };
    Ok((Box::new(src), input))
}

impl PortalSource {
    fn ensure_gl(&mut self) -> media::Result<()> {
        if !self.gl_current {
            // The video thread owns the context from here on.
            self.egl.make_current()?;
            self.gl_current = true;
        }
        if self.csc.is_some() {
            return Ok(());
        }
        let gl = Gl::load(&self.egl)?;
        let program = gl.program(FULLSCREEN_VS, CSC_FS)?;
        let (w, h) = self.out_size;
        let dst = gl.texture(gl::R8, w, h * 3 / 2, gl::RED, gl::NEAREST);
        let mut fbo = 0;
        let mut vao = 0;
        // SAFETY: context current on this thread.
        unsafe {
            (gl.glGenVertexArrays)(1, &mut vao);
            (gl.glGenFramebuffers)(1, &mut fbo);
            (gl.glBindFramebuffer)(gl::FRAMEBUFFER, fbo);
            (gl.glFramebufferTexture2D)(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, dst, 0);
            if (gl.glCheckFramebufferStatus)(gl::FRAMEBUFFER) != gl::FRAMEBUFFER_COMPLETE {
                return Err(media::err("CSC framebuffer incomplete"));
            }
            (gl.glUseProgram)(program);
            (gl.glUniform1i)(gl.uniform(program, "src"), 0);
        }
        let u_size = gl.uniform(program, "out_size");
        let _c = self.cuda.push();
        let mut dst_res = std::ptr::null_mut();
        // SAFETY: valid texture in the current GL context.
        let rc = unsafe {
            (self.cuda.api.cuGraphicsGLRegisterImage)(
                &mut dst_res,
                dst,
                gl::TEXTURE_2D,
                CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY,
            )
        };
        self.cuda.check(rc, "cuGraphicsGLRegisterImage")?;
        info!("capture pipeline: {} → {}x{} NV12 on {}", gl.string(gl::RENDERER), w, h, self.cuda.device_name);
        self.csc = Some(Csc { gl, program, vao, u_size, src: 0, src_size: (0, 0), dst, fbo, dst_res });
        Ok(())
    }

    /// Upload a shared-memory frame into the source texture.
    fn upload(&mut self, ptr: *const u8, stride: usize, len: usize, f: VideoFormat) -> media::Result<()> {
        let c = self.csc.as_mut().unwrap();
        let (w, h) = (f.width, f.height);
        if stride < w as usize * 4 || len < stride * (h as usize - 1) + w as usize * 4 {
            return Err(media::err(format!("short frame: stride {stride} len {len} for {w}x{h}")));
        }
        let gl = &c.gl;
        // SAFETY: context current; ptr valid for len bytes until the buffer is released.
        unsafe {
            if c.src_size != (w, h) {
                if c.src != 0 {
                    (gl.glDeleteTextures)(1, &c.src);
                }
                c.src = gl.texture(gl::RGBA8, w, h, gl::RGBA, gl::LINEAR);
                c.src_size = (w, h);
            }
            (gl.glBindTexture)(gl::TEXTURE_2D, c.src);
            (gl.glPixelStorei)(gl::UNPACK_ROW_LENGTH, (stride / 4) as i32);
            (gl.glPixelStorei)(gl::UNPACK_ALIGNMENT, 4);
            let fmt = if f.is_bgr() { gl::BGRA } else { gl::RGBA };
            (gl.glTexSubImage2D)(gl::TEXTURE_2D, 0, 0, 0, w as i32, h as i32, fmt, gl::UNSIGNED_BYTE, ptr.cast());
            (gl.glPixelStorei)(gl::UNPACK_ROW_LENGTH, 0);
        }
        Ok(())
    }

    /// Run the CSC pass and copy the NV12 result into an NVENC input buffer.
    fn convert(&mut self) -> media::Result<usize> {
        let (w, h) = self.out_size;
        let c = self.csc.as_mut().unwrap();
        let gl = &c.gl;
        // SAFETY: context current; objects valid.
        unsafe {
            (gl.glBindFramebuffer)(gl::FRAMEBUFFER, c.fbo);
            (gl.glViewport)(0, 0, w as i32, (h * 3 / 2) as i32);
            (gl.glUseProgram)(c.program);
            (gl.glUniform2f)(c.u_size, w as f32, h as f32);
            (gl.glActiveTexture)(gl::TEXTURE0);
            (gl.glBindTexture)(gl::TEXTURE_2D, c.src);
            (gl.glBindVertexArray)(c.vao);
            (gl.glDrawArrays)(gl::TRIANGLE_STRIP, 0, 4);
            (gl.glFlush)();
        }
        let (idx, dptr, pitch) = self.enc.next_input();
        let cuda = &self.cuda;
        let _g = cuda.push();
        // SAFETY: registered resource; mapping synchronizes with GL.
        unsafe {
            let mut res = c.dst_res;
            cuda.check((cuda.api.cuGraphicsMapResources)(1, &mut res, std::ptr::null_mut()), "map")?;
            let mut arr = std::ptr::null_mut();
            let r = cuda
                .check((cuda.api.cuGraphicsSubResourceGetMappedArray)(&mut arr, res, 0, 0), "mapped array")
                .and_then(|_| {
                    cuda.copy_2d(
                        Mem::Array(arr),
                        0,
                        Mem::Device(dptr),
                        pitch,
                        w as usize,
                        (h * 3 / 2) as usize,
                        std::ptr::null_mut(),
                    )
                });
            (cuda.api.cuGraphicsUnmapResources)(1, &mut res, std::ptr::null_mut());
            r?;
        }
        Ok(idx)
    }

    fn encode(&mut self, idx: usize, capture_us: u64) -> media::Result<EncodedFrame> {
        let mut data = Vec::with_capacity(256 * 1024);
        let key = std::mem::take(&mut self.force_key);
        let keyframe = self.enc.encode(idx, self.frames, key, &mut data)?;
        self.frames += 1;
        self.last_input = Some(idx);
        Ok(EncodedFrame { data, keyframe, capture_us, width: self.out_size.0, height: self.out_size.1 })
    }
}

impl VideoSource for PortalSource {
    fn next_frame(&mut self, timeout: Duration) -> media::Result<Option<EncodedFrame>> {
        self.ensure_gl()?;
        let Some(frame) = self.capture.next_frame(timeout) else {
            if let Some(e) = self.capture.error() {
                return Err(media::err(format!("screen capture stopped: {e}")));
            }
            // Static screen: honour keyframe requests by re-encoding the last image.
            if self.force_key
                && let Some(idx) = self.last_input
            {
                return self.encode(idx, clock::now_us()).map(Some);
            }
            return Ok(None);
        };
        let now = clock::now_us();
        // Use the compositor's timestamp when it is plausible (CLOCK_MONOTONIC ns).
        let capture_us =
            frame.pts_ns.map(|p| (p / 1000) as u64).filter(|&p| p <= now && now - p < 200_000).unwrap_or(now);
        if (frame.format.width, frame.format.height) != self.stream_size {
            debug!("stream resized to {}x{}", frame.format.width, frame.format.height);
            self.stream_size = (frame.format.width, frame.format.height);
        }
        let r = match frame.data {
            FrameData::Mem { ptr, stride, len } => self.upload(ptr, stride, len, frame.format),
            FrameData::DmaBuf { .. } => Err(media::err("DMA-BUF frames are not supported yet")),
        };
        self.capture.release(frame);
        r?;
        let idx = self.convert()?;
        self.encode(idx, capture_us).map(Some)
    }

    fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    fn set_bitrate(&mut self, kbps: u32) {
        if let Err(e) = self.enc.set_bitrate(kbps) {
            warn!("set_bitrate: {e}");
        }
    }

    fn codec(&self) -> Codec {
        self.codec
    }

    fn size(&self) -> (u32, u32) {
        self.out_size
    }

    fn cursor_mode(&self) -> CursorMode {
        if self.session.cursor_mode == portal::cursor_mode::METADATA {
            CursorMode::Metadata
        } else {
            CursorMode::Embedded
        }
    }

    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        let c = self.capture.take_cursor()?;
        let (sw, sh) = self.stream_size;
        Some(CursorUpdate {
            x: c.x as f32 / sw.max(1) as f32,
            y: c.y as f32 / sh.max(1) as f32,
            visible: c.visible,
            shape: c.shape.map(|(serial, hx, hy, w, h, rgba)| CursorShape {
                serial,
                hot_x: hx as u16,
                hot_y: hy as u16,
                width: w as u16,
                height: h as u16,
                rgba,
            }),
        })
    }

    fn describe(&self) -> String {
        format!(
            "screen {}x{} → {}x{} NVENC {} on {}",
            self.stream_size.0,
            self.stream_size.1,
            self.out_size.0,
            self.out_size.1,
            self.codec.name(),
            self.cuda.device_name
        )
    }
}

impl Drop for PortalSource {
    fn drop(&mut self) {
        if let Some(c) = self.csc.take() {
            let _g = self.cuda.push();
            // SAFETY: resources created by us; context current on this thread.
            unsafe {
                (self.cuda.api.cuGraphicsUnregisterResource)(c.dst_res);
                (c.gl.glDeleteFramebuffers)(1, &c.fbo);
                (c.gl.glDeleteTextures)(1, &c.dst);
                if c.src != 0 {
                    (c.gl.glDeleteTextures)(1, &c.src);
                }
                (c.gl.glDeleteVertexArrays)(1, &c.vao);
                (c.gl.glDeleteProgram)(c.program);
            }
        }
    }
}

struct PortalSink {
    input: PortalInput,
}

impl InputSink for PortalSink {
    fn inject(&mut self, ev: &InputEvent) -> media::Result<()> {
        let r = match *ev {
            InputEvent::Key { code, down } => self.input.key(code, down),
            InputEvent::Button { button, down } => match wire_button_to_evdev(button) {
                Some(b) => self.input.button(b, down),
                None => Ok(()),
            },
            InputEvent::MoveAbs { x, y } => self.input.move_abs(x, y),
            InputEvent::MoveRel { dx, dy } => self.input.move_rel(dx, dy),
            InputEvent::Scroll { dx, dy, discrete } => self.input.scroll(dx, dy, discrete),
        };
        r.map_err(media::err)
    }
}

struct NullSink;

impl InputSink for NullSink {
    fn inject(&mut self, _ev: &InputEvent) -> media::Result<()> {
        Ok(())
    }
}
