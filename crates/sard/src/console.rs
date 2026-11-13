//! The local terminal `sard shell` runs in: raw mode, window size and resize
//! notifications. Unix uses termios + SIGWINCH; Windows the console API with
//! virtual-terminal input/output.

use std::io;

/// Keeps the terminal in raw mode; restores the previous mode when dropped.
pub struct RawMode {
    #[cfg(unix)]
    saved: libc::termios,
    #[cfg(windows)]
    saved: windows::Saved,
}

/// Current window size in cells (cols, rows), if stdout is a terminal.
pub fn size() -> Option<(u16, u16)> {
    #[cfg(unix)]
    {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: valid fd and out-pointer.
        let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
        (ok && ws.ws_col > 0 && ws.ws_row > 0).then_some((ws.ws_col, ws.ws_row))
    }
    #[cfg(windows)]
    {
        windows::size()
    }
}

/// Is stdin a terminal (not a pipe or file)?
pub fn is_terminal() -> bool {
    use std::io::IsTerminal;
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

impl RawMode {
    /// Switch stdin to raw mode: every key goes to the remote shell as typed
    /// (including ^C, ^Z, arrows), nothing is echoed locally.
    pub fn enable() -> io::Result<RawMode> {
        #[cfg(unix)]
        {
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            // SAFETY: valid fd and termios.
            if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut t) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let saved = t;
            // SAFETY: modifies our local copy only.
            unsafe { libc::cfmakeraw(&mut t) };
            // SAFETY: valid fd and termios.
            if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(RawMode { saved })
        }
        #[cfg(windows)]
        {
            Ok(RawMode { saved: windows::enable_raw()? })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: restores the attributes read in `enable`.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
        #[cfg(windows)]
        windows::restore(&self.saved);
    }
}

/// Blocking read of raw keyboard input (bytes as a terminal sends them,
/// UTF-8 and escape sequences); `Ok(0)` at end of input.
pub fn read_input(buf: &mut [u8]) -> io::Result<usize> {
    #[cfg(unix)]
    loop {
        // SAFETY: valid buffer.
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    #[cfg(windows)]
    windows::read_input(buf)
}

/// Call `f` (on a background thread) whenever the window size changes.
pub fn on_resize(f: impl Fn(u16, u16) + Send + 'static) {
    #[cfg(unix)]
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        static CHANGED: AtomicBool = AtomicBool::new(false);
        extern "C" fn on_winch(_: libc::c_int) {
            CHANGED.store(true, Ordering::Relaxed);
        }
        // SAFETY: installing an async-signal-safe handler (one atomic store).
        unsafe {
            libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t);
        }
        let _ = std::thread::Builder::new().name("sard-winch".into()).spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if CHANGED.swap(false, Ordering::Relaxed)
                    && let Some((c, r)) = size()
                {
                    f(c, r);
                }
            }
        });
    }
    #[cfg(windows)]
    windows::on_resize(Box::new(f));
}

#[cfg(windows)]
mod windows {
    //! Implemented by the Windows backend (console modes with
    //! ENABLE_VIRTUAL_TERMINAL_INPUT / _PROCESSING).
    pub use sard_windows::console::{Saved, enable_raw, on_resize, read_input, restore, size};
}
