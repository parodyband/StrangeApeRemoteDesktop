//! App-shell helpers: message boxes, button choices (TaskDialog, falling back
//! to MessageBox without Common Controls 6), a text-entry dialog built from
//! an in-memory template, and start-at-login via the per-user Run key.

#![allow(non_snake_case, non_camel_case_types, clippy::upper_case_acronyms)]

use crate::sys::{self, HWND, LPARAM, WPARAM};
use sard_core::media;
use std::ffi::c_void;

const APP_TITLE: &str = "Sard";

fn crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn message_box(title: &str, message: &str, kind: u32) -> i32 {
    let (t, m) = (sys::wide(title), sys::wide(&crlf(message)));
    // SAFETY: valid strings; no owner window.
    unsafe {
        sys::MessageBoxW(std::ptr::null_mut(), m.as_ptr(), t.as_ptr(), kind | sys::MB_SETFOREGROUND | sys::MB_TOPMOST)
    }
}

pub fn show_message(title: &str, message: &str) {
    message_box(title, message, sys::MB_OK | sys::MB_ICONINFORMATION);
}

// ---------------------------------------------------------------------------
// TaskDialogIndirect (comctl32 v6; the executable's manifest asks for it)

#[repr(C, packed(1))]
struct TASKDIALOG_BUTTON {
    nButtonID: i32,
    pszButtonText: *const u16,
}

#[repr(C, packed(1))]
struct TASKDIALOGCONFIG {
    cbSize: u32,
    hwndParent: HWND,
    hInstance: sys::HMODULE,
    dwFlags: u32,
    dwCommonButtons: u32,
    pszWindowTitle: *const u16,
    pszMainIcon: *const u16,
    pszMainInstruction: *const u16,
    pszContent: *const u16,
    cButtons: u32,
    pButtons: *const TASKDIALOG_BUTTON,
    nDefaultButton: i32,
    cRadioButtons: u32,
    pRadioButtons: *const TASKDIALOG_BUTTON,
    nDefaultRadioButton: i32,
    pszVerificationText: *const u16,
    pszExpandedInformation: *const u16,
    pszExpandedControlText: *const u16,
    pszCollapsedControlText: *const u16,
    pszFooterIcon: *const u16,
    pszFooter: *const u16,
    pfCallback: *const c_void,
    lpCallbackData: isize,
    cxWidth: u32,
}

const _: () = {
    use std::mem::size_of;
    assert!(size_of::<TASKDIALOG_BUTTON>() == 12);
    assert!(size_of::<TASKDIALOGCONFIG>() == 160);
};

const TDF_ALLOW_DIALOG_CANCELLATION: u32 = 0x0008;
const TDF_POSITION_RELATIVE_TO_WINDOW: u32 = 0x1000;
/// `TD_INFORMATION_ICON` = MAKEINTRESOURCEW(-3).
const TD_INFORMATION_ICON: usize = 0xFFFD;

type FnTaskDialogIndirect =
    unsafe extern "system" fn(*const TASKDIALOGCONFIG, *mut i32, *mut i32, *mut i32) -> sys::HRESULT;

fn task_dialog_fn() -> Option<FnTaskDialogIndirect> {
    static F: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    let p = *F.get_or_init(|| {
        let lib = crate::dl::Library::open(&["comctl32.dll"]).ok()?;
        // SAFETY: TaskDialogIndirect has this signature (commctrl.h); only v6 exports it.
        let f = unsafe { lib.sym_opt::<FnTaskDialogIndirect>("TaskDialogIndirect\0") };
        std::mem::forget(lib);
        f.map(|f| f as usize)
    });
    // SAFETY: the address of the function looked up above.
    p.map(|p| unsafe { std::mem::transmute::<usize, FnTaskDialogIndirect>(p) })
}

/// A dialog with 2-3 buttons: `Some(index)`; `buttons[0]` is the default,
/// Escape / close picks `buttons[1]`.
pub fn choose(title: &str, message: &str, buttons: &[&str]) -> Option<usize> {
    if buttons.is_empty() {
        show_message(title, message);
        return Some(0);
    }
    if let Some(td) = task_dialog_fn() {
        let wtitle = sys::wide(APP_TITLE);
        let main = sys::wide(title);
        let content = sys::wide(&crlf(message));
        let labels: Vec<Vec<u16>> = buttons.iter().map(|b| sys::wide(b)).collect();
        let btns: Vec<TASKDIALOG_BUTTON> = labels
            .iter()
            .enumerate()
            .map(|(i, l)| TASKDIALOG_BUTTON { nButtonID: 100 + i as i32, pszButtonText: l.as_ptr() })
            .collect();
        // SAFETY: zeroed POD, then the fields we use; all strings outlive the call.
        let mut cfg: TASKDIALOGCONFIG = unsafe { std::mem::zeroed() };
        cfg.cbSize = std::mem::size_of::<TASKDIALOGCONFIG>() as u32;
        cfg.dwFlags = TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW;
        cfg.pszWindowTitle = wtitle.as_ptr();
        cfg.pszMainIcon = TD_INFORMATION_ICON as *const u16;
        cfg.pszMainInstruction = main.as_ptr();
        cfg.pszContent = content.as_ptr();
        cfg.cButtons = btns.len() as u32;
        cfg.pButtons = btns.as_ptr();
        cfg.nDefaultButton = 100;
        let mut pressed = 0i32;
        // SAFETY: valid config.
        let hr = unsafe { td(&cfg, &mut pressed, std::ptr::null_mut(), std::ptr::null_mut()) };
        if hr >= 0 {
            return match pressed {
                p if (100..100 + buttons.len() as i32).contains(&p) => Some((p - 100) as usize),
                // Escape / the close box.
                sys::IDCANCEL => Some(1.min(buttons.len() - 1)),
                _ => None,
            };
        }
        sard_core::debug!("{}", sys::hr_error("TaskDialogIndirect", hr));
    }
    // Without TaskDialog: map onto MessageBox's fixed buttons, and say which is which.
    match buttons.len() {
        1 => {
            show_message(title, message);
            Some(0)
        }
        2 => {
            let text = format!("{message}\n\nOK: {}\nCancel: {}", buttons[0], buttons[1]);
            let r = message_box(title, &text, sys::MB_OKCANCEL | sys::MB_ICONQUESTION);
            Some(if r == sys::IDOK { 0 } else { 1 })
        }
        _ => {
            let text = format!("{message}\n\nYes: {}\nNo: {}\nCancel: {}", buttons[0], buttons[2], buttons[1]);
            match message_box(title, &text, sys::MB_YESNOCANCEL | sys::MB_ICONQUESTION) {
                sys::IDYES => Some(0),
                sys::IDNO => Some(2),
                _ => Some(1),
            }
        }
    }
}

pub fn confirm(title: &str, message: &str, action: &str) -> bool {
    choose(title, message, &[action, "Cancel"]) == Some(0)
}

// ---------------------------------------------------------------------------
// Text entry: a dialog from an in-memory DLGTEMPLATE

const ID_EDIT: i32 = 100;

/// Little builder for DLGTEMPLATE / DLGITEMTEMPLATE (WORD stream, items DWORD-aligned).
struct Template(Vec<u16>);

impl Template {
    fn word(&mut self, w: u16) {
        self.0.push(w);
    }
    fn dword(&mut self, d: u32) {
        self.0.push(d as u16);
        self.0.push((d >> 16) as u16);
    }
    fn text(&mut self, s: &str) {
        self.0.extend(s.encode_utf16());
        self.0.push(0);
    }
    fn align(&mut self) {
        if self.0.len() % 2 == 1 {
            self.0.push(0);
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn item(&mut self, style: u32, x: i16, y: i16, cx: i16, cy: i16, id: u16, class: u16, title: &str) {
        self.align();
        self.dword(style | sys::WS_CHILD | sys::WS_VISIBLE);
        self.dword(0);
        for v in [x, y, cx, cy] {
            self.word(v as u16);
        }
        self.word(id);
        self.word(0xFFFF);
        self.word(class);
        self.text(title);
        self.word(0); // no creation data
    }
}

struct PromptState {
    result: Option<String>,
}

unsafe extern "system" fn prompt_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    match msg {
        sys::WM_INITDIALOG => {
            // SAFETY: lParam is the &mut PromptState passed to DialogBoxIndirectParamW.
            unsafe {
                sys::SetWindowLongPtrW(hwnd, sys::GWLP_USERDATA, lp);
                sys::SetForegroundWindow(hwnd);
                sys::SetFocus(sys::GetDlgItem(hwnd, ID_EDIT));
            }
            0 // focus set ourselves
        }
        sys::WM_COMMAND => {
            let id = (wp & 0xffff) as i32;
            if id == sys::IDOK || id == sys::IDCANCEL {
                // SAFETY: the pointer stored at WM_INITDIALOG; the dialog is modal so it is alive.
                unsafe {
                    let st = &mut *(sys::GetWindowLongPtrW(hwnd, sys::GWLP_USERDATA) as *mut PromptState);
                    if id == sys::IDOK {
                        let edit = sys::GetDlgItem(hwnd, ID_EDIT);
                        let n = sys::GetWindowTextLengthW(edit).max(0) as usize;
                        let mut buf = vec![0u16; n + 1];
                        sys::GetDlgItemTextW(hwnd, ID_EDIT, buf.as_mut_ptr(), buf.len() as i32);
                        st.result = Some(sys::from_wide(&buf));
                    }
                    sys::EndDialog(hwnd, id as isize);
                }
                return 1;
            }
            0
        }
        sys::WM_CLOSE => {
            // SAFETY: our dialog.
            unsafe { sys::EndDialog(hwnd, sys::IDCANCEL as isize) };
            1
        }
        _ => 0,
    }
}

pub fn prompt_text(title: &str, message: &str) -> Option<String> {
    const BUTTON: u16 = 0x0080;
    const EDIT: u16 = 0x0081;
    const STATIC: u16 = 0x0082;
    let lines = message.lines().count().max(1) as i16;
    let text_h = 10 * lines + 4;
    let (w, edit_y) = (260i16, 10 + text_h);
    let button_y = edit_y + 22;
    let mut t = Template(Vec::new());
    t.dword(
        sys::DS_MODALFRAME
            | sys::DS_SETFONT
            | sys::DS_CENTER
            | sys::DS_SETFOREGROUND
            | sys::WS_POPUP
            | sys::WS_CAPTION
            | sys::WS_SYSMENU,
    );
    t.dword(sys::WS_EX_TOPMOST);
    t.word(4); // items
    for v in [0i16, 0, w, button_y + 22] {
        t.word(v as u16);
    }
    t.word(0); // no menu
    t.word(0); // default class
    t.text(title);
    t.word(9); // point size
    t.text("Segoe UI");
    t.item(sys::SS_LEFT, 7, 7, w - 14, text_h, 0xFFFF, STATIC, &crlf(message));
    t.item(sys::ES_AUTOHSCROLL | sys::WS_BORDER | sys::WS_TABSTOP, 7, edit_y, w - 14, 14, ID_EDIT as u16, EDIT, "");
    t.item(sys::BS_DEFPUSHBUTTON | sys::WS_TABSTOP, w - 118, button_y, 52, 14, sys::IDOK as u16, BUTTON, "OK");
    t.item(sys::BS_PUSHBUTTON | sys::WS_TABSTOP, w - 60, button_y, 52, 14, sys::IDCANCEL as u16, BUTTON, "Cancel");
    // DLGTEMPLATE must be DWORD aligned: the Vec<u16> is 2-byte aligned, so copy into u32 storage.
    let mut aligned = vec![0u32; t.0.len().div_ceil(2)];
    // SAFETY: the destination has room for every u16 of the template.
    unsafe { std::ptr::copy_nonoverlapping(t.0.as_ptr(), aligned.as_mut_ptr() as *mut u16, t.0.len()) };
    let mut st = PromptState { result: None };
    // SAFETY: valid template and dialog procedure; `st` outlives the modal dialog.
    let r = unsafe {
        sys::DialogBoxIndirectParamW(
            sys::instance(),
            aligned.as_ptr().cast(),
            std::ptr::null_mut(),
            Some(prompt_proc),
            &mut st as *mut PromptState as LPARAM,
        )
    };
    if r == -1 {
        sard_core::warn!("text dialog failed: {}", sys::error_message(unsafe { sys::GetLastError() }));
        return None;
    }
    st.result.filter(|_| r == sys::IDOK as isize)
}

// ---------------------------------------------------------------------------
// Start at login: HKCU\Software\Microsoft\Windows\CurrentVersion\Run

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Sard";

/// `sardw.exe` (the windowless launcher next to sard.exe) if it is there, else `sard.exe app`
/// (which briefly shows a console window at login).
fn run_command() -> media::Result<String> {
    let exe = std::env::current_exe()?;
    let launcher = exe.with_file_name("sardw.exe");
    if launcher.is_file() {
        return Ok(format!("\"{}\"", launcher.display()));
    }
    Ok(format!("\"{}\" app", exe.display()))
}

pub fn set_launch_at_login(enabled: bool) -> media::Result<()> {
    let key = sys::wide(RUN_KEY);
    let name = sys::wide(RUN_VALUE);
    let mut hk: sys::HKEY = std::ptr::null_mut();
    // SAFETY: valid strings and out pointer; the key is closed below.
    unsafe {
        let r = sys::RegCreateKeyExW(
            sys::HKEY_CURRENT_USER,
            key.as_ptr(),
            0,
            std::ptr::null(),
            0,
            sys::KEY_WRITE,
            std::ptr::null(),
            &mut hk,
            std::ptr::null_mut(),
        );
        if r != sys::ERROR_SUCCESS {
            return Err(media::err(format!("cannot open the Run key: {}", sys::error_message(r as u32))));
        }
        let r = if enabled {
            let cmd = sys::wide(&run_command()?);
            sys::RegSetValueExW(hk, name.as_ptr(), 0, sys::REG_SZ, cmd.as_ptr().cast(), (cmd.len() * 2) as u32)
        } else {
            match sys::RegDeleteValueW(hk, name.as_ptr()) {
                sys::ERROR_FILE_NOT_FOUND => sys::ERROR_SUCCESS,
                r => r,
            }
        };
        sys::RegCloseKey(hk);
        if r != sys::ERROR_SUCCESS {
            return Err(media::err(format!("cannot change the Run key: {}", sys::error_message(r as u32))));
        }
    }
    Ok(())
}

pub fn launch_at_login() -> bool {
    let key = sys::wide(RUN_KEY);
    let name = sys::wide(RUN_VALUE);
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: valid strings, buffer and length.
    let r = unsafe {
        sys::RegGetValueW(
            sys::HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            sys::RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    r == sys::ERROR_SUCCESS && !sys::from_wide(&buf).is_empty()
}
