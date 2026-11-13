//! The local console `sard shell` runs in: raw mode with virtual-terminal
//! input and output, window size, resize polling, keyboard input as terminal
//! bytes. Used by `sard/src/console.rs`.
//!
//! Raw mode clears line input, echo and Ctrl+C processing (Ctrl+C arrives as
//! byte 3, for the remote shell) and turns on `ENABLE_VIRTUAL_TERMINAL_INPUT`,
//! so arrows and function keys come in as escape sequences (`ESC [ A`), the
//! way a Unix terminal sends them. Output gets VT processing, so the remote
//! shell's escape sequences are interpreted. Both code pages become UTF-8.

use crate::sys::{self, HANDLE};
use std::io;
use std::sync::Mutex;
use std::time::Duration;

/// Console modes and code pages before [`enable_raw`], for [`restore`].
pub struct Saved {
    input_mode: u32,
    output_mode: Option<u32>,
    input_cp: u32,
    output_cp: u32,
}

fn std_handle(which: u32) -> Option<HANDLE> {
    // SAFETY: plain query.
    let h = unsafe { sys::GetStdHandle(which) };
    (!h.is_null() && h != sys::INVALID_HANDLE_VALUE).then_some(h)
}

fn console_mode(h: HANDLE) -> Option<u32> {
    let mut mode = 0u32;
    // SAFETY: valid out pointer; fails (returns 0) when `h` isn't a console.
    (unsafe { sys::GetConsoleMode(h, &mut mode) } != 0).then_some(mode)
}

fn set_console_mode(h: HANDLE, mode: u32) -> io::Result<()> {
    // SAFETY: plain call on a console handle.
    if unsafe { sys::SetConsoleMode(h, mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Switch the console to raw VT mode. Fails if stdin isn't a console, or the
/// console doesn't do virtual-terminal input (before Windows 10).
pub fn enable_raw() -> io::Result<Saved> {
    let input = std_handle(sys::STD_INPUT_HANDLE).ok_or_else(|| io::Error::other("no console input"))?;
    let input_mode = console_mode(input).ok_or_else(|| io::Error::other("standard input is not a console"))?;
    // SAFETY: plain queries.
    let (input_cp, output_cp) = unsafe { (sys::GetConsoleCP(), sys::GetConsoleOutputCP()) };
    let output = std_handle(sys::STD_OUTPUT_HANDLE);
    let output_mode = output.and_then(console_mode);

    // ENABLE_WINDOW_INPUT only queues resize records, which reads skip.
    set_console_mode(input, sys::ENABLE_VIRTUAL_TERMINAL_INPUT | sys::ENABLE_WINDOW_INPUT).map_err(|e| {
        io::Error::other(format!("this console has no virtual-terminal input (Windows 10 or later needed): {e}"))
    })?;
    if let (Some(h), Some(mode)) = (output, output_mode) {
        let raw = mode | sys::ENABLE_PROCESSED_OUTPUT | sys::ENABLE_VIRTUAL_TERMINAL_PROCESSING;
        // DISABLE_NEWLINE_AUTO_RETURN: "\n" moves down only, as on a Unix
        // terminal (the remote side sends "\r\n" itself). Some older consoles
        // reject the flag; VT processing alone is still usable.
        if set_console_mode(h, raw | sys::DISABLE_NEWLINE_AUTO_RETURN).is_err()
            && let Err(e) = set_console_mode(h, raw)
        {
            let _ = set_console_mode(input, input_mode);
            return Err(io::Error::other(format!("this console has no virtual-terminal output: {e}")));
        }
    }
    // SAFETY: plain calls; restored by `restore`.
    unsafe {
        sys::SetConsoleCP(sys::CP_UTF8);
        sys::SetConsoleOutputCP(sys::CP_UTF8);
    }
    Ok(Saved { input_mode, output_mode, input_cp, output_cp })
}

/// Put back the modes and code pages saved by [`enable_raw`].
pub fn restore(saved: &Saved) {
    if let Some(h) = std_handle(sys::STD_INPUT_HANDLE) {
        let _ = set_console_mode(h, saved.input_mode);
    }
    if let (Some(h), Some(mode)) = (std_handle(sys::STD_OUTPUT_HANDLE), saved.output_mode) {
        let _ = set_console_mode(h, mode);
    }
    // SAFETY: plain calls with code pages the console had before.
    unsafe {
        sys::SetConsoleCP(saved.input_cp);
        sys::SetConsoleOutputCP(saved.output_cp);
    }
}

/// Visible window size in cells (cols, rows), if stdout is a console.
pub fn size() -> Option<(u16, u16)> {
    let h = std_handle(sys::STD_OUTPUT_HANDLE)?;
    let mut info = sys::CONSOLE_SCREEN_BUFFER_INFO::default();
    // SAFETY: valid out pointer; fails when `h` isn't a console.
    if unsafe { sys::GetConsoleScreenBufferInfo(h, &mut info) } == 0 {
        return None;
    }
    let w = info.srWindow;
    let (cols, rows) = (i32::from(w.Right) - i32::from(w.Left) + 1, i32::from(w.Bottom) - i32::from(w.Top) + 1);
    (cols > 0 && rows > 0).then_some((cols as u16, rows as u16))
}

/// Call `f` (on a background thread) whenever the window size changes. The
/// console has no resize signal outside its input queue (which the reader
/// owns), so this polls.
pub fn on_resize(f: Box<dyn Fn(u16, u16) + Send>) {
    let _ = std::thread::Builder::new().name("sard-console-size".into()).spawn(move || {
        let mut last = size();
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let now = size();
            if now != last
                && let Some((c, r)) = now
            {
                f(c, r);
            }
            last = now;
        }
    });
}

/// Keyboard input converted to UTF-8 but not yet returned (the caller's
/// buffer was full), and a UTF-16 high surrogate waiting for its other half.
struct Pending {
    bytes: Vec<u8>,
    high: Option<u16>,
}

static PENDING: Mutex<Pending> = Mutex::new(Pending { bytes: Vec::new(), high: None });

/// Blocking read of raw keyboard input as terminal bytes (UTF-8 text and
/// escape sequences); `Ok(0)` at end of input.
///
/// Reads UTF-16 with `ReadConsoleW` and converts it here: `ReadFile` with the
/// UTF-8 input code page returns NUL bytes for non-ASCII keys on Windows 10's
/// console host. Not a console (input redirected): plain `ReadFile` bytes.
pub fn read_input(buf: &mut [u8]) -> io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    let h = std_handle(sys::STD_INPUT_HANDLE).ok_or_else(|| io::Error::other("no console input"))?;
    // One reader at a time (the client has one input thread); the lock also
    // keeps the leftovers in order.
    let mut p = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if !p.bytes.is_empty() {
            let n = p.bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&p.bytes[..n]);
            p.bytes.drain(..n);
            return Ok(n);
        }
        if console_mode(h).is_none() {
            use std::io::Read;
            return io::stdin().lock().read(buf);
        }
        let mut wide = [0u16; 512];
        let mut n = 0u32;
        // SAFETY: valid buffer of the stated length (in UTF-16 units) and out pointer.
        let ok =
            unsafe { sys::ReadConsoleW(h, wide.as_mut_ptr().cast(), wide.len() as u32, &mut n, std::ptr::null_mut()) };
        if ok == 0 {
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(sys::ERROR_HANDLE_EOF | sys::ERROR_BROKEN_PIPE) => Ok(0),
                _ => Err(e),
            };
        }
        let mut units: Vec<u16> = p.high.take().into_iter().chain(wide[..n as usize].iter().copied()).collect();
        if let Some(&last) = units.last()
            && (0xD800..0xDC00).contains(&last)
        {
            p.high = units.pop();
        }
        let mut text = String::with_capacity(units.len());
        for c in char::decode_utf16(units) {
            text.push(c.unwrap_or(char::REPLACEMENT_CHARACTER));
        }
        p.bytes.extend_from_slice(text.as_bytes());
        // Nothing usable yet (only half a surrogate pair): read again.
    }
}
