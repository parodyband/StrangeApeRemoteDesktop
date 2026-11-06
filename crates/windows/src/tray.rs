//! Notification-area ("system tray") icon with a popup menu.
//!
//! A hidden window on winit's main thread receives the icon's callbacks
//! (winit's message loop dispatches them); the popup menu is built from the
//! app's `MenuEntry` model each time it opens. The window is a normal hidden
//! top-level window, not message-only, so it hears `TaskbarCreated` and puts
//! the icon back when Explorer restarts.

use crate::clipboard::register_class;
use crate::sys::{self, HICON, HWND, LPARAM, LRESULT, WPARAM};
use sard_core::media::{self, MenuEntry, Rgba, Tray, TrayCallback, TrayIcons};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const WM_TRAY: u32 = sys::WM_APP + 10;
const ICON_ID: u32 = 1;
const SM_CXSMICON: i32 = 49;

struct Shared {
    hwnd: usize,
    menu: Mutex<Vec<MenuEntry>>,
    tooltip: Mutex<String>,
    active: AtomicBool,
    icon: usize,
    icon_active: usize,
    on_click: TrayCallback,
    taskbar_created: u32,
}

impl Shared {
    fn data(&self, flags: u32) -> sys::NOTIFYICONDATAW {
        // SAFETY: zeroed POD struct, then filled in.
        let mut d: sys::NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        d.cbSize = std::mem::size_of::<sys::NOTIFYICONDATAW>() as u32;
        d.hWnd = self.hwnd as HWND;
        d.uID = ICON_ID;
        d.uFlags = flags;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = (if self.active.load(Ordering::Relaxed) { self.icon_active } else { self.icon }) as HICON;
        sys::copy_wide(&mut d.szTip, &self.tooltip.lock().unwrap());
        d
    }

    fn add(&self) -> bool {
        let mut d = self.data(sys::NIF_MESSAGE | sys::NIF_ICON | sys::NIF_TIP | sys::NIF_SHOWTIP);
        // SAFETY: valid struct.
        let ok = unsafe { sys::Shell_NotifyIconW(sys::NIM_ADD, &d) } != 0;
        d.uVersion = sys::NOTIFYICON_VERSION_4;
        // SAFETY: valid struct.
        unsafe { sys::Shell_NotifyIconW(sys::NIM_SETVERSION, &d) };
        ok
    }

    fn modify(&self) {
        let d = self.data(sys::NIF_ICON | sys::NIF_TIP | sys::NIF_SHOWTIP);
        // SAFETY: valid struct.
        unsafe { sys::Shell_NotifyIconW(sys::NIM_MODIFY, &d) };
    }

    fn show_menu(&self, x: i32, y: i32) {
        let model = self.menu.lock().unwrap().clone();
        let mut ids = Vec::new();
        // SAFETY: menus built and destroyed here; the window is ours.
        unsafe {
            let root = sys::CreatePopupMenu();
            build(root, &model, &mut ids);
            let hwnd = self.hwnd as HWND;
            // Required so the menu closes when clicking elsewhere.
            sys::SetForegroundWindow(hwnd);
            let flags = sys::TPM_RETURNCMD | sys::TPM_RIGHTBUTTON | sys::TPM_NONOTIFY | sys::TPM_BOTTOMALIGN;
            let cmd = sys::TrackPopupMenuEx(root, flags, x, y, hwnd, std::ptr::null());
            sys::PostMessageW(hwnd, sys::WM_NULL, 0, 0);
            sys::DestroyMenu(root);
            if cmd > 0
                && let Some(&id) = ids.get(cmd as usize - 1)
            {
                (self.on_click)(id);
            }
        }
    }
}

/// Menu labels: `&` marks a mnemonic in Win32 menus.
fn label(s: &str) -> Vec<u16> {
    sys::wide(&s.replace('&', "&&"))
}

/// Append `entries` to `menu`; local command ids are 1-based indexes into `ids`
/// (the app's ids can exceed the 16 bits menus carry).
///
/// # Safety
/// `menu` must be a valid menu handle.
unsafe fn build(menu: sys::HMENU, entries: &[MenuEntry], ids: &mut Vec<u32>) {
    for e in entries {
        // SAFETY: valid menu; strings live through each call.
        unsafe {
            match e {
                MenuEntry::Item { id, label: l, enabled, checked } => {
                    ids.push(*id);
                    let mut f = sys::MF_STRING;
                    if !enabled {
                        f |= sys::MF_GRAYED;
                    }
                    if *checked == Some(true) {
                        f |= sys::MF_CHECKED;
                    }
                    sys::AppendMenuW(menu, f, ids.len(), label(l).as_ptr());
                }
                MenuEntry::Submenu { label: l, enabled, items } => {
                    let sub = sys::CreatePopupMenu();
                    build(sub, items, ids);
                    let f = sys::MF_POPUP | if *enabled { 0 } else { sys::MF_GRAYED };
                    sys::AppendMenuW(menu, f, sub as usize, label(l).as_ptr());
                }
                MenuEntry::Separator => {
                    sys::AppendMenuW(menu, sys::MF_SEPARATOR, 0, std::ptr::null());
                }
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds the Arc<Shared> pointer set at creation (or 0).
    let shared = unsafe { sys::GetWindowLongPtrW(hwnd, sys::GWLP_USERDATA) } as *const Shared;
    let sh = if shared.is_null() { None } else { Some(unsafe { &*shared }) };
    match (msg, sh) {
        (WM_TRAY, Some(sh)) => {
            // NOTIFYICON_VERSION_4: event in LOWORD(lParam), anchor point in wParam.
            let ev = (lp as u32) & 0xffff;
            if matches!(ev, sys::WM_CONTEXTMENU | sys::NIN_SELECT | sys::NIN_KEYSELECT) {
                let mut p = sys::POINT::default();
                // SAFETY: valid out pointer.
                unsafe { sys::GetCursorPos(&mut p) };
                if ev == sys::NIN_KEYSELECT {
                    p = sys::POINT { x: (wp & 0xffff) as i16 as i32, y: ((wp >> 16) & 0xffff) as i16 as i32 };
                }
                sh.show_menu(p.x, p.y);
            }
            0
        }
        (m, Some(sh)) if m == sh.taskbar_created && m != 0 => {
            sh.add();
            0
        }
        (sys::WM_CLOSE, _) => {
            // SAFETY: our window, on its own thread.
            unsafe { sys::DestroyWindow(hwnd) };
            0
        }
        (sys::WM_DESTROY, Some(_)) => {
            // SAFETY: release the reference the window held.
            unsafe {
                sys::SetWindowLongPtrW(hwnd, sys::GWLP_USERDATA, 0);
                drop(Arc::from_raw(shared));
            }
            0
        }
        // SAFETY: default handling.
        _ => unsafe { sys::DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Area-average resize of straight-alpha RGBA.
fn resize(img: &Rgba, size: u32) -> Vec<u8> {
    let (sw, sh) = (img.width as usize, img.height as usize);
    let n = size as usize;
    let mut out = vec![0u8; n * n * 4];
    for y in 0..n {
        let (y0, y1) = (y * sh / n, ((y + 1) * sh / n).max(y * sh / n + 1));
        for x in 0..n {
            let (x0, x1) = (x * sw / n, ((x + 1) * sw / n).max(x * sw / n + 1));
            let mut acc = [0u32; 4];
            let mut cnt = 0;
            for sy in y0..y1.min(sh) {
                for sx in x0..x1.min(sw) {
                    let p = &img.pixels[(sy * sw + sx) * 4..][..4];
                    let a = p[3] as u32;
                    // Weight colour by alpha so transparent edges don't darken.
                    acc[0] += p[0] as u32 * a;
                    acc[1] += p[1] as u32 * a;
                    acc[2] += p[2] as u32 * a;
                    acc[3] += a;
                    cnt += 1;
                }
            }
            let o = &mut out[(y * n + x) * 4..][..4];
            for c in 0..3 {
                o[c] = acc[c].checked_div(acc[3]).unwrap_or(0) as u8;
            }
            o[3] = (acc[3] / cnt.max(1)) as u8;
        }
    }
    out
}

/// An HICON from straight-alpha RGBA.
pub fn icon_from_rgba(rgba: &[u8], size: u32) -> Result<HICON, String> {
    // SAFETY: GDI objects created, used for CreateIconIndirect and deleted.
    unsafe {
        let mut bi: sys::BITMAPV5HEADER = std::mem::zeroed();
        bi.bV5Size = std::mem::size_of::<sys::BITMAPV5HEADER>() as u32;
        bi.bV5Width = size as i32;
        bi.bV5Height = -(size as i32); // top-down
        bi.bV5Planes = 1;
        bi.bV5BitCount = 32;
        bi.bV5Compression = sys::BI_BITFIELDS;
        bi.bV5RedMask = 0x00FF_0000;
        bi.bV5GreenMask = 0x0000_FF00;
        bi.bV5BlueMask = 0x0000_00FF;
        bi.bV5AlphaMask = 0xFF00_0000;
        let dc = sys::GetDC(std::ptr::null_mut());
        let mut bits = std::ptr::null_mut();
        let color = sys::CreateDIBSection(
            dc,
            (&bi as *const sys::BITMAPV5HEADER).cast(),
            sys::DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        sys::ReleaseDC(std::ptr::null_mut(), dc);
        if color.is_null() || bits.is_null() {
            return Err("CreateDIBSection failed".into());
        }
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (size * size * 4) as usize);
        for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(rgba.as_chunks::<4>().0) {
            *d = [s[2], s[1], s[0], s[3]];
        }
        let mask = sys::CreateBitmap(size as i32, size as i32, 1, 1, std::ptr::null());
        let info = sys::ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = sys::CreateIconIndirect(&info);
        sys::DeleteObject(mask);
        sys::DeleteObject(color);
        if icon.is_null() { Err("CreateIconIndirect failed".into()) } else { Ok(icon) }
    }
}

pub struct WinTray {
    shared: Arc<Shared>,
}

impl WinTray {
    /// Call on the thread that runs the message loop (winit's main thread).
    pub fn new(icons: TrayIcons, on_click: TrayCallback) -> media::Result<WinTray> {
        // SAFETY: plain query (DPI-scaled for the process's awareness).
        let size = (unsafe { sys::GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64)) as u32;
        let icon = icon_from_rgba(&resize(&icons.color, size), size)?;
        let icon_active = icon_from_rgba(&resize(&icons.color_active, size), size)?;
        let class = sys::wide("SardTray");
        register_class(&class, Some(wndproc));
        let title = sys::wide("Sard");
        // SAFETY: registered class; hidden tool window (never shown).
        let hwnd = unsafe {
            sys::CreateWindowExW(
                sys::WS_EX_TOOLWINDOW,
                class.as_ptr(),
                title.as_ptr(),
                sys::WS_POPUP,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                sys::instance(),
                std::ptr::null_mut(),
            )
        };
        if hwnd.is_null() {
            return Err(media::err(format!(
                "CreateWindowEx (tray) failed: {}",
                sys::error_message(unsafe { sys::GetLastError() })
            )));
        }
        let tc = sys::wide("TaskbarCreated");
        let shared = Arc::new(Shared {
            hwnd: hwnd as usize,
            menu: Mutex::new(Vec::new()),
            tooltip: Mutex::new("Sard".into()),
            active: AtomicBool::new(false),
            icon: icon as usize,
            icon_active: icon_active as usize,
            on_click,
            // SAFETY: valid string.
            taskbar_created: unsafe { sys::RegisterWindowMessageW(tc.as_ptr()) },
        });
        // SAFETY: the window keeps one reference, released on WM_DESTROY.
        unsafe { sys::SetWindowLongPtrW(hwnd, sys::GWLP_USERDATA, Arc::into_raw(Arc::clone(&shared)) as isize) };
        if !shared.add() {
            sard_core::warn!("Shell_NotifyIcon(NIM_ADD) failed; the icon appears when Explorer is ready");
        }
        Ok(WinTray { shared })
    }
}

impl Tray for WinTray {
    fn set_menu(&self, menu: Vec<MenuEntry>) {
        *self.shared.menu.lock().unwrap() = menu;
    }

    fn set_tooltip(&self, text: &str) {
        *self.shared.tooltip.lock().unwrap() = text.to_string();
        self.shared.modify();
    }

    fn set_active(&self, active: bool) {
        if self.shared.active.swap(active, Ordering::Relaxed) != active {
            self.shared.modify();
        }
    }
}

impl Drop for WinTray {
    fn drop(&mut self) {
        let d = self.shared.data(0);
        // SAFETY: valid struct; posting to our window (destroyed on its thread).
        unsafe {
            sys::Shell_NotifyIconW(sys::NIM_DELETE, &d);
            sys::PostMessageW(self.shared.hwnd as HWND, sys::WM_CLOSE, 0, 0);
            sys::DestroyIcon(self.shared.icon as HICON);
            sys::DestroyIcon(self.shared.icon_active as HICON);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_halves() {
        let img = Rgba { width: 2, height: 2, pixels: vec![255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0] };
        assert_eq!(resize(&img, 1), vec![255, 0, 0, 127]);
    }
}
