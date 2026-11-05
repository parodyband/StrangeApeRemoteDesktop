//! EGL (libEGL.so.1) for window and headless OpenGL contexts.

use crate::dylib;
use std::ffi::{CStr, c_char, c_void};

pub type EGLDisplay = *mut c_void;
pub type EGLConfig = *mut c_void;
pub type EGLContext = *mut c_void;
pub type EGLSurface = *mut c_void;
pub type EGLImage = *mut c_void;
pub type EGLDeviceEXT = *mut c_void;
pub type EGLint = i32;
pub type EGLenum = u32;
pub type EGLAttrib = isize;

pub const EGL_NONE: EGLint = 0x3038;
pub const EGL_TRUE: u32 = 1;
pub const EGL_SURFACE_TYPE: EGLint = 0x3033;
pub const EGL_WINDOW_BIT: EGLint = 0x0004;
pub const EGL_PBUFFER_BIT: EGLint = 0x0001;
pub const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
pub const EGL_OPENGL_BIT: EGLint = 0x0008;
pub const EGL_RED_SIZE: EGLint = 0x3024;
pub const EGL_GREEN_SIZE: EGLint = 0x3023;
pub const EGL_BLUE_SIZE: EGLint = 0x3022;
pub const EGL_ALPHA_SIZE: EGLint = 0x3021;
pub const EGL_OPENGL_API: EGLenum = 0x30A2;
pub const EGL_CONTEXT_MAJOR_VERSION: EGLint = 0x3098;
pub const EGL_CONTEXT_MINOR_VERSION: EGLint = 0x30FB;
pub const EGL_CONTEXT_OPENGL_PROFILE_MASK: EGLint = 0x30FD;
pub const EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT: EGLint = 0x0001;
pub const EGL_PLATFORM_WAYLAND_KHR: EGLenum = 0x31D8;
pub const EGL_PLATFORM_X11_KHR: EGLenum = 0x31D5;
pub const EGL_PLATFORM_DEVICE_EXT: EGLenum = 0x313F;
pub const EGL_EXTENSIONS: EGLint = 0x3055;
pub const EGL_VENDOR: EGLint = 0x3053;
pub const EGL_LINUX_DMA_BUF_EXT: EGLenum = 0x3270;
pub const EGL_WIDTH: EGLint = 0x3057;
pub const EGL_HEIGHT: EGLint = 0x3056;
pub const EGL_LINUX_DRM_FOURCC_EXT: EGLint = 0x3271;
pub const EGL_DMA_BUF_PLANE0_FD_EXT: EGLint = 0x3272;
pub const EGL_DMA_BUF_PLANE0_OFFSET_EXT: EGLint = 0x3273;
pub const EGL_DMA_BUF_PLANE0_PITCH_EXT: EGLint = 0x3274;
pub const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: EGLint = 0x3443;
pub const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: EGLint = 0x3444;
pub const EGL_NO_CONTEXT: EGLContext = std::ptr::null_mut();
pub const EGL_NO_SURFACE: EGLSurface = std::ptr::null_mut();
pub const EGL_NO_DISPLAY: EGLDisplay = std::ptr::null_mut();

dylib! {
    pub struct EglApi ["libEGL.so.1", "libEGL.so"] {
        fn eglGetProcAddress(*const c_char) -> *mut c_void;
        fn eglGetError() -> EGLint;
        fn eglInitialize(EGLDisplay, *mut EGLint, *mut EGLint) -> u32;
        fn eglTerminate(EGLDisplay) -> u32;
        fn eglBindAPI(EGLenum) -> u32;
        fn eglChooseConfig(EGLDisplay, *const EGLint, *mut EGLConfig, EGLint, *mut EGLint) -> u32;
        fn eglCreateContext(EGLDisplay, EGLConfig, EGLContext, *const EGLint) -> EGLContext;
        fn eglDestroyContext(EGLDisplay, EGLContext) -> u32;
        fn eglCreateWindowSurface(EGLDisplay, EGLConfig, usize, *const EGLint) -> EGLSurface;
        fn eglDestroySurface(EGLDisplay, EGLSurface) -> u32;
        fn eglMakeCurrent(EGLDisplay, EGLSurface, EGLSurface, EGLContext) -> u32;
        fn eglSwapBuffers(EGLDisplay, EGLSurface) -> u32;
        fn eglSwapInterval(EGLDisplay, EGLint) -> u32;
        fn eglQueryString(EGLDisplay, EGLint) -> *const c_char;
    }
}

dylib! {
    pub struct WaylandEgl ["libwayland-egl.so.1", "libwayland-egl.so"] {
        fn wl_egl_window_create(*mut c_void, i32, i32) -> *mut c_void;
        fn wl_egl_window_resize(*mut c_void, i32, i32, i32, i32);
        fn wl_egl_window_destroy(*mut c_void);
    }
}

type GetPlatformDisplay = unsafe extern "C" fn(EGLenum, *mut c_void, *const EGLint) -> EGLDisplay;
type QueryDevices = unsafe extern "C" fn(EGLint, *mut EGLDeviceEXT, *mut EGLint) -> u32;
type QueryDeviceString = unsafe extern "C" fn(EGLDeviceEXT, EGLint) -> *const c_char;
pub type CreateImage = unsafe extern "C" fn(EGLDisplay, EGLContext, EGLenum, *mut c_void, *const EGLAttrib) -> EGLImage;
pub type DestroyImage = unsafe extern "C" fn(EGLDisplay, EGLImage) -> u32;
pub type QueryDmaBufModifiers =
    unsafe extern "C" fn(EGLDisplay, EGLint, EGLint, *mut u64, *mut u32, *mut EGLint) -> u32;

pub type Result<T> = std::result::Result<T, String>;

impl EglApi {
    /// # Safety
    /// `T` must be the matching function pointer type.
    pub unsafe fn proc<T: Copy>(&self, name: &str) -> Option<T> {
        let c = std::ffi::CString::new(name).unwrap();
        // SAFETY: valid C string.
        let p = unsafe { (self.eglGetProcAddress)(c.as_ptr()) };
        if p.is_null() { None } else { Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) }) }
    }

    pub fn error(&self, what: &str) -> String {
        // SAFETY: no arguments.
        format!("{what} failed (EGL error {:#x})", unsafe { (self.eglGetError)() })
    }

    pub fn query_string(&self, dpy: EGLDisplay, name: EGLint) -> String {
        // SAFETY: returns a static string or NULL.
        let p = unsafe { (self.eglQueryString)(dpy, name) };
        if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
    }
}

/// Where to put the context.
pub enum Target {
    Wayland {
        display: *mut c_void,
        surface: *mut c_void,
    },
    Xlib {
        display: *mut c_void,
        window: u64,
    },
    /// Headless, on the first EGL device (the NVIDIA GPU).
    Headless,
}

/// An EGL display + OpenGL 3.3 core context (+ window surface).
pub struct EglContext {
    pub api: EglApi,
    pub display: EGLDisplay,
    pub context: EGLContext,
    pub surface: EGLSurface,
    wl: Option<(WaylandEgl, *mut c_void)>,
    pub create_image: Option<CreateImage>,
    pub destroy_image: Option<DestroyImage>,
    pub query_modifiers: Option<QueryDmaBufModifiers>,
}

// SAFETY: EGL contexts may migrate between threads (one current thread at a time).
unsafe impl Send for EglContext {}

impl EglContext {
    pub fn new(target: Target, width: u32, height: u32) -> Result<Self> {
        let api = EglApi::load()?;
        // SAFETY: standard EGL initialization sequence with valid handles.
        unsafe {
            let gpd: GetPlatformDisplay = api
                .proc("eglGetPlatformDisplay")
                .or_else(|| api.proc("eglGetPlatformDisplayEXT"))
                .ok_or("eglGetPlatformDisplay unavailable")?;
            let display = match &target {
                Target::Wayland { display, .. } => gpd(EGL_PLATFORM_WAYLAND_KHR, *display, std::ptr::null()),
                Target::Xlib { display, .. } => gpd(EGL_PLATFORM_X11_KHR, *display, std::ptr::null()),
                Target::Headless => {
                    let qd: QueryDevices = api.proc("eglQueryDevicesEXT").ok_or("eglQueryDevicesEXT unavailable")?;
                    let qs: Option<QueryDeviceString> = api.proc("eglQueryDeviceStringEXT");
                    let mut devs = [std::ptr::null_mut(); 8];
                    let mut n = 0;
                    qd(8, devs.as_mut_ptr(), &mut n);
                    // Prefer the NVIDIA device (it must match the CUDA device for interop).
                    let mut pick = None;
                    for &d in devs.iter().take(n as usize) {
                        let ext = qs.map(|q| q(d, EGL_EXTENSIONS)).filter(|p| !p.is_null());
                        let ext = ext.map(|p| CStr::from_ptr(p).to_string_lossy().into_owned()).unwrap_or_default();
                        if ext.contains("EGL_NV_device_cuda") {
                            pick = Some(d);
                            break;
                        }
                        pick.get_or_insert(d);
                    }
                    let dev = pick.ok_or("no EGL devices")?;
                    gpd(EGL_PLATFORM_DEVICE_EXT, dev, std::ptr::null())
                }
            };
            if display.is_null() {
                return Err(api.error("eglGetPlatformDisplay"));
            }
            let (mut maj, mut min) = (0, 0);
            if (api.eglInitialize)(display, &mut maj, &mut min) != EGL_TRUE {
                return Err(api.error("eglInitialize"));
            }
            if (api.eglBindAPI)(EGL_OPENGL_API) != EGL_TRUE {
                return Err(api.error("eglBindAPI(OpenGL)"));
            }
            let windowed = !matches!(target, Target::Headless);
            let attrs = [
                EGL_SURFACE_TYPE,
                if windowed { EGL_WINDOW_BIT } else { EGL_PBUFFER_BIT },
                EGL_RENDERABLE_TYPE,
                EGL_OPENGL_BIT,
                EGL_RED_SIZE,
                8,
                EGL_GREEN_SIZE,
                8,
                EGL_BLUE_SIZE,
                8,
                EGL_ALPHA_SIZE,
                0,
                EGL_NONE,
            ];
            let mut config = std::ptr::null_mut();
            let mut n = 0;
            if (api.eglChooseConfig)(display, attrs.as_ptr(), &mut config, 1, &mut n) != EGL_TRUE || n == 0 {
                // Headless devices may expose no pbuffer configs; any GL config works surfaceless.
                let attrs = [EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE];
                if (api.eglChooseConfig)(display, attrs.as_ptr(), &mut config, 1, &mut n) != EGL_TRUE || n == 0 {
                    return Err(api.error("eglChooseConfig"));
                }
            }
            let ctx_attrs = [
                EGL_CONTEXT_MAJOR_VERSION,
                3,
                EGL_CONTEXT_MINOR_VERSION,
                3,
                EGL_CONTEXT_OPENGL_PROFILE_MASK,
                EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
                EGL_NONE,
            ];
            let context = (api.eglCreateContext)(display, config, EGL_NO_CONTEXT, ctx_attrs.as_ptr());
            if context.is_null() {
                return Err(api.error("eglCreateContext"));
            }
            let mut wl = None;
            let surface = match target {
                Target::Wayland { surface, .. } => {
                    let w = WaylandEgl::load()?;
                    let win = (w.wl_egl_window_create)(surface, width.max(1) as i32, height.max(1) as i32);
                    if win.is_null() {
                        return Err("wl_egl_window_create failed".into());
                    }
                    let s = (api.eglCreateWindowSurface)(display, config, win as usize, std::ptr::null());
                    wl = Some((w, win));
                    s
                }
                Target::Xlib { window, .. } => {
                    (api.eglCreateWindowSurface)(display, config, window as usize, std::ptr::null())
                }
                Target::Headless => EGL_NO_SURFACE,
            };
            if windowed && surface.is_null() {
                return Err(api.error("eglCreateWindowSurface"));
            }
            let create_image = api.proc("eglCreateImage");
            let destroy_image = api.proc("eglDestroyImage");
            let query_modifiers = api.proc("eglQueryDmaBufModifiersEXT");
            Ok(EglContext { api, display, context, surface, wl, create_image, destroy_image, query_modifiers })
        }
    }

    pub fn make_current(&self) -> Result<()> {
        // SAFETY: handles are valid.
        let ok = unsafe { (self.api.eglMakeCurrent)(self.display, self.surface, self.surface, self.context) };
        if ok != EGL_TRUE { Err(self.api.error("eglMakeCurrent")) } else { Ok(()) }
    }

    pub fn release_current(&self) {
        // SAFETY: releasing is always valid.
        unsafe { (self.api.eglMakeCurrent)(self.display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT) };
    }

    /// Non-blocking presents (no vsync wait) for lowest latency.
    pub fn set_swap_interval(&self, i: i32) {
        // SAFETY: context is current.
        unsafe { (self.api.eglSwapInterval)(self.display, i) };
    }

    pub fn swap(&self) -> Result<()> {
        // SAFETY: valid display/surface.
        let ok = unsafe { (self.api.eglSwapBuffers)(self.display, self.surface) };
        if ok != EGL_TRUE { Err(self.api.error("eglSwapBuffers")) } else { Ok(()) }
    }

    pub fn resize(&self, w: u32, h: u32) {
        if let Some((api, win)) = &self.wl {
            // SAFETY: win is our wl_egl_window.
            unsafe { (api.wl_egl_window_resize)(*win, w.max(1) as i32, h.max(1) as i32, 0, 0) };
        }
    }

    /// Load an OpenGL function.
    ///
    /// # Safety
    /// `T` must be the matching function pointer type.
    pub unsafe fn gl_proc<T: Copy>(&self, name: &str) -> Option<T> {
        // SAFETY: forwarded.
        unsafe { self.api.proc(name) }
    }
}

impl Drop for EglContext {
    fn drop(&mut self) {
        // SAFETY: destroying what we created.
        unsafe {
            (self.api.eglMakeCurrent)(self.display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
            if !self.surface.is_null() {
                (self.api.eglDestroySurface)(self.display, self.surface);
            }
            if let Some((api, win)) = &self.wl {
                (api.wl_egl_window_destroy)(*win);
            }
            (self.api.eglDestroyContext)(self.display, self.context);
        }
    }
}
