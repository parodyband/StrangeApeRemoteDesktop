//! Drive the C API against a running host, as the app would:
//! `cargo run -p sard-mobile --example smoke -- <config dir> <addr> <code>`
//! (start `sard host --pair --terminal --test-pattern` there first).

#[cfg(target_os = "macos")]
fn main() {
    use sard_mobile::*;
    use std::ffi::{CStr, CString, c_char, c_void};
    use std::sync::Mutex;
    use std::time::Duration;

    static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static OUTPUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    extern "C" fn event(_: *mut c_void, j: *const c_char) {
        let s = unsafe { CStr::from_ptr(j) }.to_string_lossy().into_owned();
        println!("event: {s}");
        EVENTS.lock().unwrap().push(s);
    }
    extern "C" fn shell_data(_: *mut c_void, _id: u32, d: *const u8, n: usize) {
        OUTPUT.lock().unwrap().extend_from_slice(unsafe { std::slice::from_raw_parts(d, n) });
    }
    extern "C" fn cursor_shape(_: *mut c_void, s: u32, w: u16, h: u16, _: u16, _: u16, _: *const u8) {
        println!("cursor shape {s}: {w}x{h}");
    }
    extern "C" fn cursor_pos(_: *mut c_void, _: f32, _: f32, _: bool) {}
    let take = |s: *mut c_char| {
        let r = unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned();
        unsafe { sard_free(s) };
        r
    };
    let a: Vec<String> = std::env::args().collect();
    let c = |s: &str| CString::new(s).unwrap();
    unsafe { sard_init(c(&a[1]).as_ptr(), c("smoke-phone").as_ptr()) };
    let r = take(unsafe { sard_pair(c(&a[2]).as_ptr(), c(&a[3]).as_ptr()) });
    println!("pair: {r}");
    let hosts = take(sard_hosts());
    println!("hosts: {hosts}");
    let key = hosts.split("\"key\":\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    let cb = SardCallbacks { ctx: std::ptr::null_mut(), event, shell_data, cursor_shape, cursor_pos };
    let opts =
        SardScreenOptions { max_width: 1280, max_height: 720, max_fps: 60, fit_width: 0, fit_height: 0, audio: false };

    // Screen session (no layer: decode only) and terminal session at once.
    let screen = unsafe { sard_connect(c(&key).as_ptr(), SARD_MODE_SCREEN, opts, cb) };
    let term = unsafe { sard_connect(c(&key).as_ptr(), SARD_MODE_TERMINAL, opts, cb) };
    std::thread::sleep(Duration::from_secs(2));
    unsafe { sard_shell_open(term, 1, 0, 80, 24) };
    std::thread::sleep(Duration::from_millis(800));
    let id = EVENTS.lock().unwrap().iter().find_map(|e| {
        e.contains("shell_opened")
            .then(|| e.split("\"id\":").nth(1).unwrap().split(',').next().unwrap().parse::<u32>().unwrap())
    });
    let id = id.expect("shell opened");
    let cmd = b"echo smoke-$((6*7))\r";
    unsafe { sard_shell_write(term, id, cmd.as_ptr(), cmd.len()) };
    unsafe { sard_mouse_move(screen, 0.5, 0.5) };
    std::thread::sleep(Duration::from_secs(3));
    let n = OUTPUT.lock().unwrap().len() as u32;
    unsafe { sard_shell_ack(term, id, n) };
    let out = String::from_utf8_lossy(&OUTPUT.lock().unwrap()).into_owned();
    println!("terminal output contains smoke-42: {}", out.contains("smoke-42"));
    unsafe { sard_shell_close(term, id) };
    std::thread::sleep(Duration::from_millis(300));
    unsafe { sard_session_close(screen) };
    unsafe { sard_session_close(term) };
    let ev = EVENTS.lock().unwrap();
    let stats = ev.iter().filter(|e| e.contains("\"stats\"")).count();
    println!("stats events: {stats}; stream: {}", ev.iter().any(|e| e.contains("\"stream\"")));
}

#[cfg(not(target_os = "macos"))]
fn main() {}
