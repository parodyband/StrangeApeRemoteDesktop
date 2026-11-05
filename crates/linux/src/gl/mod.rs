//! OpenGL 3.3 core functions loaded through EGL, plus small helpers.

pub mod egl;

use egl::EglContext;
use std::ffi::{CString, c_char, c_void};

pub const TEXTURE_2D: u32 = 0x0DE1;
pub const R8: i32 = 0x8229;
pub const RG8: i32 = 0x822B;
pub const RGBA8: i32 = 0x8058;
pub const RED: u32 = 0x1903;
pub const RG: u32 = 0x8227;
pub const RGBA: u32 = 0x1908;
pub const BGRA: u32 = 0x80E1;
pub const UNSIGNED_BYTE: u32 = 0x1401;
pub const TEXTURE_MIN_FILTER: u32 = 0x2801;
pub const TEXTURE_MAG_FILTER: u32 = 0x2800;
pub const TEXTURE_WRAP_S: u32 = 0x2802;
pub const TEXTURE_WRAP_T: u32 = 0x2803;
pub const LINEAR: i32 = 0x2601;
pub const NEAREST: i32 = 0x2600;
pub const CLAMP_TO_EDGE: i32 = 0x812F;
pub const TEXTURE0: u32 = 0x84C0;
pub const VERTEX_SHADER: u32 = 0x8B31;
pub const FRAGMENT_SHADER: u32 = 0x8B30;
pub const COMPILE_STATUS: u32 = 0x8B81;
pub const LINK_STATUS: u32 = 0x8B82;
pub const TRIANGLE_STRIP: u32 = 0x0005;
pub const COLOR_BUFFER_BIT: u32 = 0x4000;
pub const FRAMEBUFFER: u32 = 0x8D40;
pub const COLOR_ATTACHMENT0: u32 = 0x8CE0;
pub const FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
pub const UNPACK_ROW_LENGTH: u32 = 0x0CF2;
pub const UNPACK_ALIGNMENT: u32 = 0x0CF5;
pub const PACK_ALIGNMENT: u32 = 0x0D05;
pub const VENDOR: u32 = 0x1F00;
pub const RENDERER: u32 = 0x1F01;
pub const VERSION: u32 = 0x1F02;
pub const SYNC_GPU_COMMANDS_COMPLETE: u32 = 0x9117;
pub const SYNC_FLUSH_COMMANDS_BIT: u32 = 1;

macro_rules! gl_api {
    (pub struct $name:ident { $( fn $f:ident($($arg:ty),* $(,)?) $(-> $ret:ty)?; )* }) => {
        #[allow(non_snake_case)]
        pub struct $name { $( pub $f: unsafe extern "C" fn($($arg),*) $(-> $ret)?, )* }
        impl $name {
            pub fn load(egl: &EglContext) -> Result<Self, String> {
                // SAFETY: names and signatures follow the OpenGL 3.3 core spec.
                unsafe {
                    Ok(Self { $( $f: egl.gl_proc(stringify!($f)).ok_or(concat!("missing GL function ", stringify!($f)))?, )* })
                }
            }
        }
    };
}

gl_api! {
    pub struct Gl {
        fn glGetString(u32) -> *const u8;
        fn glGetError() -> u32;
        fn glViewport(i32, i32, i32, i32);
        fn glClearColor(f32, f32, f32, f32);
        fn glClear(u32);
        fn glGenTextures(i32, *mut u32);
        fn glDeleteTextures(i32, *const u32);
        fn glBindTexture(u32, u32);
        fn glTexImage2D(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
        fn glTexSubImage2D(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
        fn glTexParameteri(u32, u32, i32);
        fn glActiveTexture(u32);
        fn glPixelStorei(u32, i32);
        fn glCreateShader(u32) -> u32;
        fn glShaderSource(u32, i32, *const *const c_char, *const i32);
        fn glCompileShader(u32);
        fn glGetShaderiv(u32, u32, *mut i32);
        fn glGetShaderInfoLog(u32, i32, *mut i32, *mut c_char);
        fn glDeleteShader(u32);
        fn glCreateProgram() -> u32;
        fn glAttachShader(u32, u32);
        fn glLinkProgram(u32);
        fn glGetProgramiv(u32, u32, *mut i32);
        fn glGetProgramInfoLog(u32, i32, *mut i32, *mut c_char);
        fn glUseProgram(u32);
        fn glDeleteProgram(u32);
        fn glGetUniformLocation(u32, *const c_char) -> i32;
        fn glUniform1i(i32, i32);
        fn glUniform1f(i32, f32);
        fn glUniform2f(i32, f32, f32);
        fn glUniform4f(i32, f32, f32, f32, f32);
        fn glGenVertexArrays(i32, *mut u32);
        fn glBindVertexArray(u32);
        fn glDeleteVertexArrays(i32, *const u32);
        fn glDrawArrays(u32, i32, i32);
        fn glGenFramebuffers(i32, *mut u32);
        fn glBindFramebuffer(u32, u32);
        fn glFramebufferTexture2D(u32, u32, u32, u32, i32);
        fn glCheckFramebufferStatus(u32) -> u32;
        fn glDeleteFramebuffers(i32, *const u32);
        fn glFinish();
        fn glFlush();
        fn glReadPixels(i32, i32, i32, i32, u32, u32, *mut c_void);
        fn glEGLImageTargetTexture2DOES(u32, *mut c_void);
        fn glFenceSync(u32, u32) -> *mut c_void;
        fn glClientWaitSync(*mut c_void, u32, u64) -> u32;
        fn glDeleteSync(*mut c_void);
    }
}

impl Gl {
    pub fn string(&self, name: u32) -> String {
        // SAFETY: returns a static NUL-terminated string or NULL.
        let p = unsafe { (self.glGetString)(name) };
        if p.is_null() {
            String::new()
        } else {
            unsafe { std::ffi::CStr::from_ptr(p.cast()) }.to_string_lossy().into_owned()
        }
    }

    fn shader(&self, kind: u32, src: &str) -> Result<u32, String> {
        let c = CString::new(src).unwrap();
        // SAFETY: valid GL context is current; pointers valid for the call.
        unsafe {
            let s = (self.glCreateShader)(kind);
            let p = c.as_ptr();
            (self.glShaderSource)(s, 1, &p, std::ptr::null());
            (self.glCompileShader)(s);
            let mut ok = 0;
            (self.glGetShaderiv)(s, COMPILE_STATUS, &mut ok);
            if ok == 0 {
                let mut buf = vec![0 as c_char; 4096];
                let mut n = 0;
                (self.glGetShaderInfoLog)(s, 4096, &mut n, buf.as_mut_ptr());
                let msg = std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
                (self.glDeleteShader)(s);
                return Err(format!("shader compile error: {msg}"));
            }
            Ok(s)
        }
    }

    /// Compile and link a vertex + fragment program.
    pub fn program(&self, vs: &str, fs: &str) -> Result<u32, String> {
        let v = self.shader(VERTEX_SHADER, vs)?;
        let f = self.shader(FRAGMENT_SHADER, fs)?;
        // SAFETY: as above.
        unsafe {
            let p = (self.glCreateProgram)();
            (self.glAttachShader)(p, v);
            (self.glAttachShader)(p, f);
            (self.glLinkProgram)(p);
            (self.glDeleteShader)(v);
            (self.glDeleteShader)(f);
            let mut ok = 0;
            (self.glGetProgramiv)(p, LINK_STATUS, &mut ok);
            if ok == 0 {
                let mut buf = vec![0 as c_char; 4096];
                let mut n = 0;
                (self.glGetProgramInfoLog)(p, 4096, &mut n, buf.as_mut_ptr());
                let msg = std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
                return Err(format!("program link error: {msg}"));
            }
            Ok(p)
        }
    }

    pub fn uniform(&self, prog: u32, name: &str) -> i32 {
        let c = CString::new(name).unwrap();
        // SAFETY: valid program and C string.
        unsafe { (self.glGetUniformLocation)(prog, c.as_ptr()) }
    }

    /// Create a 2D texture with the given internal format and size.
    pub fn texture(&self, internal: i32, w: u32, h: u32, format: u32, filter: i32) -> u32 {
        let mut t = 0;
        // SAFETY: GL context current.
        unsafe {
            (self.glGenTextures)(1, &mut t);
            (self.glBindTexture)(TEXTURE_2D, t);
            (self.glTexImage2D)(
                TEXTURE_2D,
                0,
                internal,
                w as i32,
                h as i32,
                0,
                format,
                UNSIGNED_BYTE,
                std::ptr::null(),
            );
            (self.glTexParameteri)(TEXTURE_2D, TEXTURE_MIN_FILTER, filter);
            (self.glTexParameteri)(TEXTURE_2D, TEXTURE_MAG_FILTER, filter);
            (self.glTexParameteri)(TEXTURE_2D, TEXTURE_WRAP_S, CLAMP_TO_EDGE);
            (self.glTexParameteri)(TEXTURE_2D, TEXTURE_WRAP_T, CLAMP_TO_EDGE);
        }
        t
    }
}

/// Full-screen triangle strip vertex shader (no vertex buffers).
pub const FULLSCREEN_VS: &str = r#"#version 330 core
out vec2 uv;
void main() {
    vec2 p = vec2(float(gl_VertexID & 1) * 2.0 - 1.0, float(gl_VertexID >> 1) * 2.0 - 1.0);
    uv = vec2((p.x + 1.0) * 0.5, 1.0 - (p.y + 1.0) * 0.5);
    gl_Position = vec4(p, 0.0, 1.0);
}
"#;
