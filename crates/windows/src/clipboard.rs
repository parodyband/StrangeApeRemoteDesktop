//! The Windows text clipboard (`CF_UNICODETEXT`).
//!
//! Setting the clipboard needs an owner window, and an owner window's thread
//! must keep processing messages (other programs' `EmptyClipboard` sends it
//! `WM_DESTROYCLIPBOARD` and waits). So the clipboard lives on its own thread
//! with a message-only window; `get` / `set` are marshalled there with
//! `SendMessageW`.

use crate::sys::{self, HWND, LPARAM, LRESULT, WPARAM};
use sard_core::media::Clipboard;
use std::sync::mpsc;

const WM_CLIP_GET: u32 = sys::WM_APP + 1;
const WM_CLIP_SET: u32 = sys::WM_APP + 2;

pub struct WinClipboard {
    hwnd: usize,
    thread: Option<std::thread::JoinHandle<()>>,
    seq: u32,
    cached: Option<String>,
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
        Some(WinClipboard { hwnd, thread: Some(thread), seq: 0, cached: None })
    }
}

impl Clipboard for WinClipboard {
    fn get(&mut self) -> Option<String> {
        // Unchanged since the last read: don't open the clipboard (other programs may be using it).
        // SAFETY: plain query.
        let seq = unsafe { sys::GetClipboardSequenceNumber() };
        if seq != 0 && seq == self.seq {
            return self.cached.clone();
        }
        let mut out: Option<String> = None;
        // SAFETY: synchronous; `out` outlives the call.
        unsafe { sys::SendMessageW(self.hwnd as HWND, WM_CLIP_GET, 0, &mut out as *mut _ as LPARAM) };
        self.seq = seq;
        self.cached = out.clone();
        out
    }

    fn set(&mut self, text: &str) {
        // SAFETY: synchronous; `text` outlives the call.
        unsafe { sys::SendMessageW(self.hwnd as HWND, WM_CLIP_SET, 0, &text as *const &str as LPARAM) };
        // SAFETY: plain query.
        self.seq = unsafe { sys::GetClipboardSequenceNumber() };
        self.cached = Some(text.to_string());
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
