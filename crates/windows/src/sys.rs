//! Hand-written Win32 declarations (kernel32, user32, gdi32, shell32,
//! advapi32, ole32): only what this crate calls. Struct layouts are checked
//! against the Windows SDK headers by `tools/winabi/check.py`.

#![allow(non_snake_case, non_camel_case_types, clippy::upper_case_acronyms, dead_code)]

use std::ffi::c_void;

pub type BOOL = i32;
pub type HRESULT = i32;
pub type HANDLE = *mut c_void;
pub type HWND = *mut c_void;
pub type HMENU = *mut c_void;
pub type HICON = *mut c_void;
pub type HCURSOR = *mut c_void;
pub type HBITMAP = *mut c_void;
pub type HDC = *mut c_void;
pub type HMODULE = *mut c_void;
pub type HGLOBAL = *mut c_void;
pub type HKEY = *mut c_void;
pub type HMONITOR = *mut c_void;
/// A pseudoconsole (ConPTY).
pub type HPCON = *mut c_void;
pub type WPARAM = usize;
pub type LPARAM = isize;
pub type LRESULT = isize;
pub type WNDPROC = Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>;
pub type DLGPROC = Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> isize>;

/// A COM / Windows GUID.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct GUID {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl GUID {
    /// `0x6bc82762_4e63_4ca4_aa85_1e50f321f6bf` style: the GUID's text form as one number.
    pub const fn from_u128(v: u128) -> GUID {
        GUID {
            data1: (v >> 96) as u32,
            data2: (v >> 80) as u16,
            data3: (v >> 64) as u16,
            data4: (v as u64).to_be_bytes(),
        }
    }
}

impl std::fmt::Display for GUID {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let d = &self.data4;
        write!(
            f,
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            self.data1, self.data2, self.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }
}

pub const S_OK: HRESULT = 0;
pub const S_FALSE: HRESULT = 1;
pub const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
pub const E_FAIL: HRESULT = 0x8000_4005_u32 as i32;
pub const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
pub const RPC_E_CHANGED_MODE: HRESULT = 0x8001_0106_u32 as i32;

pub const INFINITE: u32 = 0xFFFF_FFFF;
pub const WAIT_OBJECT_0: u32 = 0;
pub const WAIT_TIMEOUT: u32 = 258;
pub const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;
pub const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
pub const STD_ERROR_HANDLE: u32 = -12i32 as u32;
pub const GMEM_MOVEABLE: u32 = 0x0002;
pub const CF_UNICODETEXT: u32 = 13;
pub const CF_DIB: u32 = 8;
pub const CF_DIBV5: u32 = 17;
pub const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x0000_1000;
pub const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x0000_0200;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct POINT {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RECT {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
pub struct MSG {
    pub hwnd: HWND,
    pub message: u32,
    pub wParam: WPARAM,
    pub lParam: LPARAM,
    pub time: u32,
    pub pt: POINT,
    pub lPrivate: u32,
}

#[repr(C)]
pub struct WNDCLASSEXW {
    pub cbSize: u32,
    pub style: u32,
    pub lpfnWndProc: WNDPROC,
    pub cbClsExtra: i32,
    pub cbWndExtra: i32,
    pub hInstance: HMODULE,
    pub hIcon: HICON,
    pub hCursor: HCURSOR,
    pub hbrBackground: *mut c_void,
    pub lpszMenuName: *const u16,
    pub lpszClassName: *const u16,
    pub hIconSm: HICON,
}

#[repr(C)]
pub struct ICONINFO {
    pub fIcon: BOOL,
    pub xHotspot: u32,
    pub yHotspot: u32,
    pub hbmMask: HBITMAP,
    pub hbmColor: HBITMAP,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CIEXYZTRIPLE {
    pub v: [i32; 9],
}

#[repr(C)]
pub struct BITMAPV5HEADER {
    pub bV5Size: u32,
    pub bV5Width: i32,
    pub bV5Height: i32,
    pub bV5Planes: u16,
    pub bV5BitCount: u16,
    pub bV5Compression: u32,
    pub bV5SizeImage: u32,
    pub bV5XPelsPerMeter: i32,
    pub bV5YPelsPerMeter: i32,
    pub bV5ClrUsed: u32,
    pub bV5ClrImportant: u32,
    pub bV5RedMask: u32,
    pub bV5GreenMask: u32,
    pub bV5BlueMask: u32,
    pub bV5AlphaMask: u32,
    pub bV5CSType: u32,
    pub bV5Endpoints: CIEXYZTRIPLE,
    pub bV5GammaRed: u32,
    pub bV5GammaGreen: u32,
    pub bV5GammaBlue: u32,
    pub bV5Intent: u32,
    pub bV5ProfileData: u32,
    pub bV5ProfileSize: u32,
    pub bV5Reserved: u32,
}

#[repr(C)]
pub struct NOTIFYICONDATAW {
    pub cbSize: u32,
    pub hWnd: HWND,
    pub uID: u32,
    pub uFlags: u32,
    pub uCallbackMessage: u32,
    pub hIcon: HICON,
    pub szTip: [u16; 128],
    pub dwState: u32,
    pub dwStateMask: u32,
    pub szInfo: [u16; 256],
    pub uVersion: u32,
    pub szInfoTitle: [u16; 64],
    pub dwInfoFlags: u32,
    pub guidItem: GUID,
    pub hBalloonIcon: HICON,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MOUSEINPUT {
    pub dx: i32,
    pub dy: i32,
    pub mouseData: u32,
    pub dwFlags: u32,
    pub time: u32,
    pub dwExtraInfo: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KEYBDINPUT {
    pub wVk: u16,
    pub wScan: u16,
    pub dwFlags: u32,
    pub time: u32,
    pub dwExtraInfo: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union INPUT_u {
    pub mi: MOUSEINPUT,
    pub ki: KEYBDINPUT,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct INPUT {
    pub r#type: u32,
    pub u: INPUT_u,
}

#[repr(C)]
pub struct MONITORINFOEXW {
    pub cbSize: u32,
    pub rcMonitor: RECT,
    pub rcWork: RECT,
    pub dwFlags: u32,
    pub szDevice: [u16; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct COORD {
    pub X: i16,
    pub Y: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SMALL_RECT {
    pub Left: i16,
    pub Top: i16,
    pub Right: i16,
    pub Bottom: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CONSOLE_SCREEN_BUFFER_INFO {
    pub dwSize: COORD,
    pub dwCursorPosition: COORD,
    pub wAttributes: u16,
    pub srWindow: SMALL_RECT,
    pub dwMaximumWindowSize: COORD,
}

#[repr(C)]
pub struct STARTUPINFOW {
    pub cb: u32,
    pub lpReserved: *mut u16,
    pub lpDesktop: *mut u16,
    pub lpTitle: *mut u16,
    pub dwX: u32,
    pub dwY: u32,
    pub dwXSize: u32,
    pub dwYSize: u32,
    pub dwXCountChars: u32,
    pub dwYCountChars: u32,
    pub dwFillAttribute: u32,
    pub dwFlags: u32,
    pub wShowWindow: u16,
    pub cbReserved2: u16,
    pub lpReserved2: *mut u8,
    pub hStdInput: HANDLE,
    pub hStdOutput: HANDLE,
    pub hStdError: HANDLE,
}

#[repr(C)]
pub struct STARTUPINFOEXW {
    pub StartupInfo: STARTUPINFOW,
    pub lpAttributeList: *mut c_void,
}

#[repr(C)]
pub struct PROCESS_INFORMATION {
    pub hProcess: HANDLE,
    pub hThread: HANDLE,
    pub dwProcessId: u32,
    pub dwThreadId: u32,
}

/// `DEVMODEW` with the display variant of its unions (`dmPosition`,
/// `dmDisplayOrientation`, `dmDisplayFixedOutput`; `dmDisplayFlags`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DEVMODEW {
    pub dmDeviceName: [u16; 32],
    pub dmSpecVersion: u16,
    pub dmDriverVersion: u16,
    pub dmSize: u16,
    pub dmDriverExtra: u16,
    pub dmFields: u32,
    pub dmPosition: POINT,
    pub dmDisplayOrientation: u32,
    pub dmDisplayFixedOutput: u32,
    pub dmColor: i16,
    pub dmDuplex: i16,
    pub dmYResolution: i16,
    pub dmTTOption: i16,
    pub dmCollate: i16,
    pub dmFormName: [u16; 32],
    pub dmLogPixels: u16,
    pub dmBitsPerPel: u32,
    pub dmPelsWidth: u32,
    pub dmPelsHeight: u32,
    pub dmDisplayFlags: u32,
    pub dmDisplayFrequency: u32,
    pub dmICMMethod: u32,
    pub dmICMIntent: u32,
    pub dmMediaType: u32,
    pub dmDitherType: u32,
    pub dmReserved1: u32,
    pub dmReserved2: u32,
    pub dmPanningWidth: u32,
    pub dmPanningHeight: u32,
}

impl DEVMODEW {
    /// Zeroed, with `dmSize` set (as `EnumDisplaySettingsExW` requires).
    pub fn new() -> DEVMODEW {
        // SAFETY: plain integers and arrays; all-zero is a valid value.
        let mut dm: DEVMODEW = unsafe { std::mem::zeroed() };
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        dm
    }
}

impl Default for DEVMODEW {
    fn default() -> DEVMODEW {
        DEVMODEW::new()
    }
}

const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<GUID>() == 16);
    assert!(size_of::<POINT>() == 8);
    assert!(size_of::<RECT>() == 16);
    assert!(size_of::<MSG>() == 48);
    assert!(size_of::<WNDCLASSEXW>() == 80);
    assert!(size_of::<ICONINFO>() == 32);
    assert!(size_of::<BITMAPV5HEADER>() == 124);
    assert!(size_of::<NOTIFYICONDATAW>() == 976);
    assert!(offset_of!(NOTIFYICONDATAW, szTip) == 40);
    assert!(offset_of!(NOTIFYICONDATAW, uVersion) == 816);
    assert!(offset_of!(NOTIFYICONDATAW, guidItem) == 952);
    assert!(size_of::<MOUSEINPUT>() == 32);
    assert!(size_of::<KEYBDINPUT>() == 24);
    assert!(size_of::<INPUT>() == 40);
    assert!(size_of::<MONITORINFOEXW>() == 104);
    assert!(size_of::<COORD>() == 4);
    assert!(size_of::<SMALL_RECT>() == 8);
    assert!(size_of::<CONSOLE_SCREEN_BUFFER_INFO>() == 22);
    assert!(offset_of!(CONSOLE_SCREEN_BUFFER_INFO, srWindow) == 10);
    assert!(size_of::<STARTUPINFOW>() == 104);
    assert!(offset_of!(STARTUPINFOW, dwFlags) == 60);
    assert!(offset_of!(STARTUPINFOW, hStdInput) == 80);
    assert!(size_of::<STARTUPINFOEXW>() == 112);
    assert!(size_of::<PROCESS_INFORMATION>() == 24);
    assert!(size_of::<DEVMODEW>() == 220);
    assert!(offset_of!(DEVMODEW, dmFields) == 72);
    assert!(offset_of!(DEVMODEW, dmPosition) == 76);
    assert!(offset_of!(DEVMODEW, dmDisplayOrientation) == 84);
    assert!(offset_of!(DEVMODEW, dmLogPixels) == 166);
    assert!(offset_of!(DEVMODEW, dmBitsPerPel) == 168);
    assert!(offset_of!(DEVMODEW, dmDisplayFlags) == 180);
    assert!(offset_of!(DEVMODEW, dmDisplayFrequency) == 184);
    assert!(offset_of!(DEVMODEW, dmPanningHeight) == 216);
};

// ---- window messages / styles ----
pub const WM_NULL: u32 = 0x0000;
pub const WM_CREATE: u32 = 0x0001;
pub const WM_DESTROY: u32 = 0x0002;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_QUIT: u32 = 0x0012;
pub const WM_SETFONT: u32 = 0x0030;
pub const WM_COMMAND: u32 = 0x0111;
pub const WM_INITDIALOG: u32 = 0x0110;
pub const WM_CLIPBOARDUPDATE: u32 = 0x031D;
pub const WM_CONTEXTMENU: u32 = 0x007B;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_RBUTTONUP: u32 = 0x0205;
pub const WM_USER: u32 = 0x0400;
pub const WM_APP: u32 = 0x8000;
pub const NIN_SELECT: u32 = WM_USER;
pub const NIN_KEYSELECT: u32 = WM_USER + 1;

pub const WS_OVERLAPPED: u32 = 0x0000_0000;
pub const WS_POPUP: u32 = 0x8000_0000;
pub const WS_CHILD: u32 = 0x4000_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;
pub const WS_CAPTION: u32 = 0x00C0_0000;
pub const WS_SYSMENU: u32 = 0x0008_0000;
pub const WS_BORDER: u32 = 0x0080_0000;
pub const WS_TABSTOP: u32 = 0x0001_0000;
pub const WS_GROUP: u32 = 0x0002_0000;
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
pub const WS_EX_TOPMOST: u32 = 0x0000_0008;
pub const DS_MODALFRAME: u32 = 0x80;
pub const DS_SETFONT: u32 = 0x40;
pub const DS_CENTER: u32 = 0x0800;
pub const DS_SETFOREGROUND: u32 = 0x200;
pub const ES_AUTOHSCROLL: u32 = 0x0080;
pub const BS_PUSHBUTTON: u32 = 0x0000;
pub const BS_DEFPUSHBUTTON: u32 = 0x0001;
pub const SS_LEFT: u32 = 0x0000;
pub const IDOK: i32 = 1;
pub const IDCANCEL: i32 = 2;
pub const IDYES: i32 = 6;
pub const IDNO: i32 = 7;
pub const GWLP_USERDATA: i32 = -21;
pub const HWND_MESSAGE: HWND = -3isize as HWND;
pub const IDC_ARROW: usize = 32512;

// ---- menus ----
pub const MF_STRING: u32 = 0x0000;
pub const MF_GRAYED: u32 = 0x0001;
pub const MF_CHECKED: u32 = 0x0008;
pub const MF_POPUP: u32 = 0x0010;
pub const MF_SEPARATOR: u32 = 0x0800;
pub const TPM_RIGHTBUTTON: u32 = 0x0002;
pub const TPM_RETURNCMD: u32 = 0x0100;
pub const TPM_NONOTIFY: u32 = 0x0080;
pub const TPM_BOTTOMALIGN: u32 = 0x0020;

// ---- message boxes ----
pub const MB_OK: u32 = 0x0000;
pub const MB_OKCANCEL: u32 = 0x0001;
pub const MB_YESNOCANCEL: u32 = 0x0003;
pub const MB_YESNO: u32 = 0x0004;
pub const MB_ICONWARNING: u32 = 0x0030;
pub const MB_ICONINFORMATION: u32 = 0x0040;
pub const MB_ICONQUESTION: u32 = 0x0020;
pub const MB_SETFOREGROUND: u32 = 0x0001_0000;
pub const MB_TOPMOST: u32 = 0x0004_0000;

// ---- notify icon ----
pub const NIM_ADD: u32 = 0;
pub const NIM_MODIFY: u32 = 1;
pub const NIM_DELETE: u32 = 2;
pub const NIM_SETVERSION: u32 = 4;
pub const NIF_MESSAGE: u32 = 0x01;
pub const NIF_ICON: u32 = 0x02;
pub const NIF_TIP: u32 = 0x04;
pub const NIF_SHOWTIP: u32 = 0x80;
pub const NOTIFYICON_VERSION_4: u32 = 4;

// ---- input ----
pub const INPUT_MOUSE: u32 = 0;
pub const INPUT_KEYBOARD: u32 = 1;
pub const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
pub const KEYEVENTF_KEYUP: u32 = 0x0002;
pub const KEYEVENTF_UNICODE: u32 = 0x0004;
pub const KEYEVENTF_SCANCODE: u32 = 0x0008;
pub const MOUSEEVENTF_MOVE: u32 = 0x0001;
pub const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
pub const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
pub const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
pub const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
pub const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
pub const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
pub const MOUSEEVENTF_XDOWN: u32 = 0x0080;
pub const MOUSEEVENTF_XUP: u32 = 0x0100;
pub const MOUSEEVENTF_WHEEL: u32 = 0x0800;
pub const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
pub const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
pub const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
pub const XBUTTON1: u32 = 0x0001;
pub const XBUTTON2: u32 = 0x0002;
pub const WHEEL_DELTA: i32 = 120;
pub const SM_XVIRTUALSCREEN: i32 = 76;
pub const SM_YVIRTUALSCREEN: i32 = 77;
pub const SM_CXVIRTUALSCREEN: i32 = 78;
pub const SM_CYVIRTUALSCREEN: i32 = 79;

// ---- GDI ----
pub const BI_BITFIELDS: u32 = 3;
pub const DIB_RGB_COLORS: u32 = 0;

// ---- registry ----
pub const HKEY_CURRENT_USER: HKEY = 0x8000_0001_usize as HKEY;
pub const KEY_READ: u32 = 0x2_0019;
pub const KEY_WRITE: u32 = 0x2_0006;
pub const REG_SZ: u32 = 1;
pub const RRF_RT_REG_SZ: u32 = 0x0000_0002;
pub const ERROR_SUCCESS: i32 = 0;
pub const ERROR_FILE_NOT_FOUND: i32 = 2;

// ---- console ----
pub const STD_INPUT_HANDLE: u32 = -10i32 as u32;
pub const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
pub const ENABLE_LINE_INPUT: u32 = 0x0002;
pub const ENABLE_ECHO_INPUT: u32 = 0x0004;
pub const ENABLE_WINDOW_INPUT: u32 = 0x0008;
pub const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
pub const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
pub const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
pub const DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x0008;
pub const CP_UTF8: u32 = 65001;

// ---- processes ----
pub const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
pub const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
pub const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
pub const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: u32 = 0x0002_0016;
pub const STILL_ACTIVE: u32 = 259;
pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;
pub const ERROR_HANDLE_EOF: i32 = 38;
pub const ERROR_BROKEN_PIPE: i32 = 109;
pub const ERROR_NO_DATA: i32 = 232;

// ---- display modes ----
pub const ENUM_CURRENT_SETTINGS: u32 = -1i32 as u32;
pub const DM_DISPLAYORIENTATION: u32 = 0x0000_0080;
pub const DM_BITSPERPEL: u32 = 0x0004_0000;
pub const DM_PELSWIDTH: u32 = 0x0008_0000;
pub const DM_PELSHEIGHT: u32 = 0x0010_0000;
pub const DM_DISPLAYFLAGS: u32 = 0x0020_0000;
pub const DM_DISPLAYFREQUENCY: u32 = 0x0040_0000;
pub const DM_INTERLACED: u32 = 0x0000_0002;
pub const CDS_FULLSCREEN: u32 = 0x0000_0004;
pub const DISP_CHANGE_SUCCESSFUL: i32 = 0;
pub const DISP_CHANGE_RESTART: i32 = 1;
pub const DISP_CHANGE_BADMODE: i32 = -2;
pub const MDT_EFFECTIVE_DPI: u32 = 0;
pub const USER_DEFAULT_SCREEN_DPI: u32 = 96;

// ---- COM ----
pub const COINIT_MULTITHREADED: u32 = 0x0;
pub const COINIT_APARTMENTTHREADED: u32 = 0x2;
pub const CLSCTX_ALL: u32 = 0x17;

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn GetLastError() -> u32;
    pub fn CloseHandle(h: HANDLE) -> BOOL;
    pub fn CreateEventW(attrs: *const c_void, manual_reset: BOOL, initial: BOOL, name: *const u16) -> HANDLE;
    pub fn SetEvent(h: HANDLE) -> BOOL;
    pub fn WaitForSingleObject(h: HANDLE, ms: u32) -> u32;
    pub fn GetModuleHandleW(name: *const u16) -> HMODULE;
    pub fn LoadLibraryExW(name: *const u16, file: HANDLE, flags: u32) -> HMODULE;
    pub fn GetProcAddress(module: HMODULE, name: *const u8) -> *mut c_void;
    pub fn GlobalAlloc(flags: u32, bytes: usize) -> HGLOBAL;
    pub fn GlobalLock(h: HGLOBAL) -> *mut c_void;
    pub fn GlobalUnlock(h: HGLOBAL) -> BOOL;
    pub fn GlobalFree(h: HGLOBAL) -> HGLOBAL;
    pub fn GlobalSize(h: HGLOBAL) -> usize;
    pub fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
    pub fn FreeConsole() -> BOOL;
    pub fn GetConsoleWindow() -> HWND;
    pub fn GetConsoleMode(h: HANDLE, mode: *mut u32) -> BOOL;
    pub fn GetStdHandle(which: u32) -> HANDLE;
    pub fn SetStdHandle(which: u32, h: HANDLE) -> BOOL;
    pub fn GetCurrentThreadId() -> u32;
    pub fn SetConsoleMode(h: HANDLE, mode: u32) -> BOOL;
    pub fn GetConsoleCP() -> u32;
    pub fn GetConsoleOutputCP() -> u32;
    pub fn SetConsoleCP(cp: u32) -> BOOL;
    pub fn SetConsoleOutputCP(cp: u32) -> BOOL;
    pub fn GetConsoleScreenBufferInfo(h: HANDLE, info: *mut CONSOLE_SCREEN_BUFFER_INFO) -> BOOL;
    pub fn ReadConsoleW(h: HANDLE, buf: *mut c_void, chars: u32, read: *mut u32, control: *mut c_void) -> BOOL;
    pub fn CreatePipe(read: *mut HANDLE, write: *mut HANDLE, attrs: *const c_void, size: u32) -> BOOL;
    pub fn InitializeProcThreadAttributeList(list: *mut c_void, count: u32, flags: u32, size: *mut usize) -> BOOL;
    pub fn UpdateProcThreadAttribute(
        list: *mut c_void,
        flags: u32,
        attribute: usize,
        value: *const c_void,
        size: usize,
        previous: *mut c_void,
        returned: *mut usize,
    ) -> BOOL;
    pub fn DeleteProcThreadAttributeList(list: *mut c_void);
    pub fn CreateProcessW(
        app: *const u16,
        cmdline: *mut u16,
        process_attrs: *const c_void,
        thread_attrs: *const c_void,
        inherit: BOOL,
        flags: u32,
        env: *const c_void,
        cwd: *const u16,
        startup: *const STARTUPINFOW,
        info: *mut PROCESS_INFORMATION,
    ) -> BOOL;
    pub fn GetExitCodeProcess(process: HANDLE, code: *mut u32) -> BOOL;
    pub fn TerminateProcess(process: HANDLE, code: u32) -> BOOL;
    pub fn FormatMessageW(
        flags: u32,
        source: *const c_void,
        id: u32,
        lang: u32,
        buf: *mut u16,
        size: u32,
        args: *mut c_void,
    ) -> u32;
}

#[link(name = "user32")]
unsafe extern "system" {
    pub fn RegisterClassExW(class: *const WNDCLASSEXW) -> u16;
    pub fn CreateWindowExW(
        ex_style: u32,
        class: *const u16,
        name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: HWND,
        menu: HMENU,
        instance: HMODULE,
        param: *mut c_void,
    ) -> HWND;
    pub fn DefWindowProcW(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
    pub fn DestroyWindow(hwnd: HWND) -> BOOL;
    pub fn PostMessageW(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> BOOL;
    pub fn SendMessageW(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
    pub fn GetMessageW(msg: *mut MSG, hwnd: HWND, min: u32, max: u32) -> BOOL;
    pub fn TranslateMessage(msg: *const MSG) -> BOOL;
    pub fn DispatchMessageW(msg: *const MSG) -> LRESULT;
    pub fn PostQuitMessage(code: i32);
    pub fn SetWindowLongPtrW(hwnd: HWND, index: i32, value: isize) -> isize;
    pub fn GetWindowLongPtrW(hwnd: HWND, index: i32) -> isize;
    pub fn RegisterWindowMessageW(name: *const u16) -> u32;
    pub fn CreatePopupMenu() -> HMENU;
    pub fn AppendMenuW(menu: HMENU, flags: u32, id: usize, text: *const u16) -> BOOL;
    pub fn DestroyMenu(menu: HMENU) -> BOOL;
    pub fn TrackPopupMenuEx(menu: HMENU, flags: u32, x: i32, y: i32, hwnd: HWND, params: *const c_void) -> BOOL;
    pub fn SetForegroundWindow(hwnd: HWND) -> BOOL;
    pub fn GetCursorPos(p: *mut POINT) -> BOOL;
    pub fn CreateIconIndirect(info: *const ICONINFO) -> HICON;
    pub fn DestroyIcon(icon: HICON) -> BOOL;
    pub fn LoadCursorW(instance: HMODULE, name: *const u16) -> HCURSOR;
    pub fn MessageBoxW(hwnd: HWND, text: *const u16, caption: *const u16, kind: u32) -> i32;
    pub fn SendInput(count: u32, inputs: *const INPUT, size: i32) -> u32;
    pub fn GetSystemMetrics(index: i32) -> i32;
    pub fn OpenClipboard(owner: HWND) -> BOOL;
    pub fn CloseClipboard() -> BOOL;
    pub fn EmptyClipboard() -> BOOL;
    pub fn GetClipboardData(format: u32) -> HANDLE;
    pub fn SetClipboardData(format: u32, data: HANDLE) -> HANDLE;
    pub fn GetClipboardSequenceNumber() -> u32;
    pub fn RegisterClipboardFormatW(name: *const u16) -> u32;
    pub fn IsClipboardFormatAvailable(format: u32) -> BOOL;
    pub fn GetDC(hwnd: HWND) -> HDC;
    pub fn ReleaseDC(hwnd: HWND, dc: HDC) -> i32;
    pub fn GetMonitorInfoW(monitor: HMONITOR, info: *mut MONITORINFOEXW) -> BOOL;
    pub fn DialogBoxIndirectParamW(
        instance: HMODULE,
        template: *const c_void,
        parent: HWND,
        proc_: DLGPROC,
        param: LPARAM,
    ) -> isize;
    pub fn EndDialog(hwnd: HWND, result: isize) -> BOOL;
    pub fn GetDlgItem(hwnd: HWND, id: i32) -> HWND;
    pub fn GetDlgItemTextW(hwnd: HWND, id: i32, buf: *mut u16, max: i32) -> u32;
    pub fn GetWindowTextLengthW(hwnd: HWND) -> i32;
    pub fn SetFocus(hwnd: HWND) -> HWND;
    pub fn EnumDisplaySettingsExW(device: *const u16, mode: u32, devmode: *mut DEVMODEW, flags: u32) -> BOOL;
    pub fn ChangeDisplaySettingsExW(
        device: *const u16,
        devmode: *const DEVMODEW,
        hwnd: HWND,
        flags: u32,
        param: *const c_void,
    ) -> i32;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    pub fn CreateBitmap(w: i32, h: i32, planes: u32, bpp: u32, bits: *const c_void) -> HBITMAP;
    pub fn CreateDIBSection(
        dc: HDC,
        info: *const c_void,
        usage: u32,
        bits: *mut *mut c_void,
        section: HANDLE,
        offset: u32,
    ) -> HBITMAP;
    pub fn DeleteObject(obj: *mut c_void) -> BOOL;
}

#[link(name = "shell32")]
unsafe extern "system" {
    pub fn Shell_NotifyIconW(msg: u32, data: *const NOTIFYICONDATAW) -> BOOL;
    pub fn ShellExecuteW(
        hwnd: HWND,
        op: *const u16,
        file: *const u16,
        params: *const u16,
        dir: *const u16,
        show: i32,
    ) -> HMODULE;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    pub fn RegCreateKeyExW(
        key: HKEY,
        subkey: *const u16,
        reserved: u32,
        class: *const u16,
        options: u32,
        sam: u32,
        security: *const c_void,
        result: *mut HKEY,
        disposition: *mut u32,
    ) -> i32;
    pub fn RegSetValueExW(key: HKEY, name: *const u16, reserved: u32, kind: u32, data: *const u8, len: u32) -> i32;
    pub fn RegDeleteValueW(key: HKEY, name: *const u16) -> i32;
    pub fn RegGetValueW(
        key: HKEY,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        kind: *mut u32,
        data: *mut c_void,
        len: *mut u32,
    ) -> i32;
    pub fn RegCloseKey(key: HKEY) -> i32;
}

#[link(name = "ole32")]
unsafe extern "system" {
    pub fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> HRESULT;
    pub fn CoCreateInstance(
        clsid: *const GUID,
        outer: *mut c_void,
        ctx: u32,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT;
    pub fn CoTaskMemFree(p: *mut c_void);
    pub fn PropVariantClear(p: *mut c_void) -> HRESULT;
}

/// NUL-terminated UTF-16 for Win32 `W` functions.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copy `s` into a fixed UTF-16 buffer (truncated, always NUL-terminated).
pub fn copy_wide(dst: &mut [u16], s: &str) {
    let mut n = 0;
    for (d, c) in dst.iter_mut().zip(s.encode_utf16()) {
        *d = c;
        n += 1;
    }
    let end = n.min(dst.len().saturating_sub(1));
    dst[end] = 0;
}

/// A UTF-16 buffer up to its first NUL.
pub fn from_wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

/// System message for a Win32 error code or HRESULT (empty if none).
pub fn error_message(code: u32) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: valid buffer of the stated size; no inserts.
    let n = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            std::ptr::null(),
            code,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            std::ptr::null_mut(),
        )
    };
    String::from_utf16_lossy(&buf[..n as usize]).trim().to_string()
}

/// "what failed: 0x887A0004 (The specified device interface ... is not supported)".
pub fn hr_error(what: &str, hr: HRESULT) -> String {
    let msg = error_message(hr as u32);
    if msg.is_empty() {
        format!("{what} failed: {:#010X}", hr as u32)
    } else {
        format!("{what} failed: {:#010X} ({msg})", hr as u32)
    }
}

/// `Ok(())` for success HRESULTs (>= 0), else a readable error.
pub fn check(hr: HRESULT, what: &str) -> Result<(), String> {
    if hr >= 0 { Ok(()) } else { Err(hr_error(what, hr)) }
}

/// This module's instance handle (for window classes and dialogs).
pub fn instance() -> HMODULE {
    // SAFETY: NULL asks for the executable's own module.
    unsafe { GetModuleHandleW(std::ptr::null()) }
}

/// Initialize COM on this thread (multithreaded apartment). Harmless if the
/// thread already joined an apartment.
pub fn com_init() {
    // SAFETY: plain COM initialization; we never uninitialize (threads end with the process or shortly after).
    let hr = unsafe { CoInitializeEx(std::ptr::null_mut(), COINIT_MULTITHREADED) };
    if hr < 0 && hr != RPC_E_CHANGED_MODE {
        sard_core::warn!("{}", hr_error("CoInitializeEx", hr));
    }
}
