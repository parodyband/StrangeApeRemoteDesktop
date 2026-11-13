//! The Windows clipboard: text (`CF_UNICODETEXT`) and images. Images travel
//! as PNG: read from the "PNG" format apps like browsers add, else converted
//! from the bitmap (`CF_DIBV5` / `CF_DIB`, e.g. PrintScreen); written as both
//! PNG and a bitmap so every app can paste.
//!
//! Setting the clipboard needs an owner window, and an owner window's thread
//! must keep processing messages (other programs' `EmptyClipboard` sends it
//! `WM_DESTROYCLIPBOARD` and waits). So the clipboard lives on its own thread
//! with a message-only window; `get` / `set` are marshalled there with
//! `SendMessageW`.

use crate::sys::{self, HWND, LPARAM, LRESULT, WPARAM};
use sard_core::media::Clipboard;
use std::sync::{Arc, OnceLock, mpsc};

const WM_CLIP_GET: u32 = sys::WM_APP + 1;
const WM_CLIP_SET: u32 = sys::WM_APP + 2;
const WM_CLIP_GET_IMAGE: u32 = sys::WM_APP + 3;
const WM_CLIP_SET_IMAGE: u32 = sys::WM_APP + 4;

pub struct WinClipboard {
    hwnd: usize,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Clipboard sequence number the caches below belong to.
    seq: u32,
    /// None: not read since the clipboard changed.
    text: Option<Option<String>>,
    image: Option<Option<Arc<[u8]>>>,
}

/// The registered "PNG" clipboard format.
fn png_format() -> u32 {
    static F: OnceLock<u32> = OnceLock::new();
    // SAFETY: NUL-terminated name.
    *F.get_or_init(|| unsafe { sys::RegisterClipboardFormatW(sys::wide("PNG").as_ptr()) })
}

fn open(hwnd: HWND) -> bool {
    // Another program may hold the clipboard for a moment.
    for _ in 0..10 {
        // SAFETY: plain call with our window.
        if unsafe { sys::OpenClipboard(hwnd) } != 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

fn read_text(hwnd: HWND) -> Option<String> {
    if !open(hwnd) {
        return None;
    }
    // SAFETY: clipboard open; the handle stays valid until CloseClipboard; we read within GlobalSize.
    let text = unsafe {
        let h = sys::GetClipboardData(sys::CF_UNICODETEXT);
        let mut s = None;
        if !h.is_null() {
            let p = sys::GlobalLock(h) as *const u16;
            if !p.is_null() {
                let max = sys::GlobalSize(h) / 2;
                let units = std::slice::from_raw_parts(p, max);
                s = Some(sys::from_wide(units));
                sys::GlobalUnlock(h);
            }
        }
        sys::CloseClipboard();
        s
    };
    text.map(|t| t.replace("\r\n", "\n"))
}

/// Copy a clipboard format's bytes (clipboard open).
///
/// # Safety
/// The clipboard is open on this thread.
unsafe fn read_bytes(format: u32) -> Option<Vec<u8>> {
    // SAFETY: the handle stays valid until CloseClipboard; we read within GlobalSize.
    unsafe {
        let h = sys::GetClipboardData(format);
        if h.is_null() {
            return None;
        }
        let p = sys::GlobalLock(h) as *const u8;
        if p.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(p, sys::GlobalSize(h)).to_vec();
        sys::GlobalUnlock(h);
        Some(v)
    }
}

/// Put bytes on the clipboard in `format` (clipboard open and emptied by us).
///
/// # Safety
/// The clipboard is open on this thread, owned by our window.
unsafe fn write_bytes(format: u32, data: &[u8]) -> bool {
    // SAFETY: on success the system owns the memory; otherwise we free it.
    unsafe {
        let h = sys::GlobalAlloc(sys::GMEM_MOVEABLE, data.len().max(1));
        if h.is_null() {
            return false;
        }
        let p = sys::GlobalLock(h) as *mut u8;
        if p.is_null() {
            sys::GlobalFree(h);
            return false;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        sys::GlobalUnlock(h);
        if sys::SetClipboardData(format, h).is_null() {
            sys::GlobalFree(h);
            return false;
        }
        true
    }
}

/// The clipboard's image as PNG, unless it also has text.
fn read_image(hwnd: HWND) -> Option<Vec<u8>> {
    // SAFETY: plain queries.
    let has = |f: u32| unsafe { sys::IsClipboardFormatAvailable(f) } != 0;
    let png = png_format();
    if has(sys::CF_UNICODETEXT) || !(has(png) || has(sys::CF_DIB) || has(sys::CF_DIBV5)) || !open(hwnd) {
        return None;
    }
    // SAFETY: clipboard open by us; closed below.
    let (png, dib) = unsafe {
        let p = if png != 0 { read_bytes(png) } else { None };
        let p = p.and_then(|p| sard_core::png::byte_len(&p).map(|n| p[..n].to_vec()));
        let d = if p.is_none() { read_bytes(sys::CF_DIBV5).or_else(|| read_bytes(sys::CF_DIB)) } else { None };
        sys::CloseClipboard();
        (p, d)
    };
    png.or_else(|| {
        let (w, h, rgba) = sard_core::dib::to_rgba(&dib?)?;
        Some(sard_core::png::encode_rgba(w, h, &rgba))
    })
}

fn write_image(hwnd: HWND, png: &[u8], dib: Option<&[u8]>) -> bool {
    if !open(hwnd) {
        sard_core::debug!("clipboard busy; image not set");
        return false;
    }
    // SAFETY: clipboard open by our window.
    unsafe {
        sys::EmptyClipboard();
        let mut ok = png_format() != 0 && write_bytes(png_format(), png);
        if let Some(d) = dib {
            ok |= write_bytes(sys::CF_DIB, d);
        }
        sys::CloseClipboard();
        ok
    }
}

fn write_text(hwnd: HWND, text: &str) {
    let crlf = text.replace("\r\n", "\n").replace('\n', "\r\n");
    let w: Vec<u16> = crlf.encode_utf16().chain(std::iter::once(0)).collect();
    if !open(hwnd) {
        sard_core::debug!("clipboard busy; not set");
        return;
    }
    // SAFETY: clipboard open by our window; on success the system owns the memory.
    unsafe {
        sys::EmptyClipboard();
        let h = sys::GlobalAlloc(sys::GMEM_MOVEABLE, w.len() * 2);
        if !h.is_null() {
            let p = sys::GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                sys::GlobalUnlock(h);
                if sys::SetClipboardData(sys::CF_UNICODETEXT, h).is_null() {
                    sys::GlobalFree(h);
                }
            } else {
                sys::GlobalFree(h);
            }
        }
        sys::CloseClipboard();
    }
}

struct SetImage<'a> {
    png: &'a [u8],
    dib: Option<Vec<u8>>,
    ok: bool,
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CLIP_GET => {
            // SAFETY: the sender passes a live &mut Option<String> and waits for us.
            let out = unsafe { &mut *(lp as *mut Option<String>) };
            *out = read_text(hwnd);
            0
        }
        WM_CLIP_SET => {
            // SAFETY: the sender passes a live &str and waits for us.
            let text = unsafe { &*(lp as *const &str) };
            write_text(hwnd, text);
            0
        }
        WM_CLIP_GET_IMAGE => {
            // SAFETY: the sender passes a live &mut Option<Vec<u8>> and waits for us.
            let out = unsafe { &mut *(lp as *mut Option<Vec<u8>>) };
            *out = read_image(hwnd);
            0
        }
        WM_CLIP_SET_IMAGE => {
            // SAFETY: the sender passes a live SetImage and waits for us.
            let req = unsafe { &mut *(lp as *mut SetImage) };
            req.ok = write_image(hwnd, req.png, req.dib.as_deref());
            0
        }
        sys::WM_CLOSE => {
            // SAFETY: our own window, destroyed on its thread.
            unsafe { sys::DestroyWindow(hwnd) };
            0
        }
        sys::WM_DESTROY => {
            // SAFETY: plain call.
            unsafe { sys::PostQuitMessage(0) };
            0
        }
        // SAFETY: default handling.
        _ => unsafe { sys::DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Register a window class once per process (ignores "already registered").
pub fn register_class(name: &[u16], proc_: sys::WNDPROC) {
    let wc = sys::WNDCLASSEXW {
        cbSize: std::mem::size_of::<sys::WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: proc_,
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: sys::instance(),
        hIcon: std::ptr::null_mut(),
        hCursor: std::ptr::null_mut(),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: name.as_ptr(),
        hIconSm: std::ptr::null_mut(),
    };
    // SAFETY: valid class description; the name outlives the call (static in callers).
    unsafe { sys::RegisterClassExW(&wc) };
}

impl WinClipboard {
    pub fn open() -> Option<WinClipboard> {
        let (tx, rx) = mpsc::channel::<usize>();
        let thread = std::thread::Builder::new()
            .name("sard-clipboard-win".into())
            .spawn(move || {
                let class = sys::wide("SardClipboard");
                register_class(&class, Some(wndproc));
                // SAFETY: registered class; message-only window (HWND_MESSAGE parent).
                let hwnd = unsafe {
                    sys::CreateWindowExW(
                        0,
                        class.as_ptr(),
                        class.as_ptr(),
                        0,
                        0,
                        0,
                        0,
                        0,
                        sys::HWND_MESSAGE,
                        std::ptr::null_mut(),
                        sys::instance(),
                        std::ptr::null_mut(),
                    )
                };
                let _ = tx.send(hwnd as usize);
                if hwnd.is_null() {
                    return;
                }
                // SAFETY: standard message loop for this thread's window.
                unsafe {
                    let mut msg: sys::MSG = std::mem::zeroed();
                    while sys::GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                        sys::TranslateMessage(&msg);
                        sys::DispatchMessageW(&msg);
                    }
                }
            })
            .ok()?;
        let hwnd = rx.recv().ok()?;
        if hwnd == 0 {
            return None;
        }
        Some(WinClipboard { hwnd, thread: Some(thread), seq: 0, text: None, image: None })
    }

    /// Forget cached contents if the clipboard changed since they were read.
    /// Unchanged: don't open the clipboard (other programs may be using it).
    fn check_seq(&mut self) {
        // SAFETY: plain query.
        let seq = unsafe { sys::GetClipboardSequenceNumber() };
        if seq == 0 || seq != self.seq {
            self.seq = seq;
            self.text = None;
            self.image = None;
        }
    }

    /// After we wrote the clipboard: our own write isn't a change to read back.
    fn wrote(&mut self, text: Option<String>, image: Option<Arc<[u8]>>) {
        // SAFETY: plain query.
        self.seq = unsafe { sys::GetClipboardSequenceNumber() };
        self.text = Some(text);
        self.image = Some(image);
    }
}

impl Clipboard for WinClipboard {
    fn get(&mut self) -> Option<String> {
        self.check_seq();
        let hwnd = self.hwnd as HWND;
        self.text
            .get_or_insert_with(|| {
                let mut out: Option<String> = None;
                // SAFETY: synchronous; `out` outlives the call.
                unsafe { sys::SendMessageW(hwnd, WM_CLIP_GET, 0, &mut out as *mut _ as LPARAM) };
                out
            })
            .clone()
    }

    fn set(&mut self, text: &str) {
        // SAFETY: synchronous; `text` outlives the call.
        unsafe { sys::SendMessageW(self.hwnd as HWND, WM_CLIP_SET, 0, &text as *const &str as LPARAM) };
        self.wrote(Some(text.to_string()), None);
    }

    fn get_image(&mut self) -> Option<Arc<[u8]>> {
        self.check_seq();
        let hwnd = self.hwnd as HWND;
        self.image
            .get_or_insert_with(|| {
                let mut out: Option<Vec<u8>> = None;
                // SAFETY: synchronous; `out` outlives the call.
                unsafe { sys::SendMessageW(hwnd, WM_CLIP_GET_IMAGE, 0, &mut out as *mut _ as LPARAM) };
                out.map(Into::into)
            })
            .clone()
    }

    fn set_image(&mut self, png: &[u8]) -> bool {
        // A bitmap too, for apps that don't read PNG (decoded here, off the window thread).
        let dib = sard_core::png::decode(png).map(|i| sard_core::dib::from_rgba(i.width, i.height, &i.rgba));
        let mut req = SetImage { png, dib, ok: false };
        // SAFETY: synchronous; `req` outlives the call.
        unsafe { sys::SendMessageW(self.hwnd as HWND, WM_CLIP_SET_IMAGE, 0, &mut req as *mut _ as LPARAM) };
        self.wrote(None, Some(png.into()));
        req.ok
    }
}

impl Drop for WinClipboard {
    fn drop(&mut self) {
        // SAFETY: posting to our window; its thread destroys it.
        unsafe { sys::PostMessageW(self.hwnd as HWND, sys::WM_CLOSE, 0, 0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
