//! Pseudo-terminals for remote shells (`sard shell`, the iPhone app).
//!
//! [`Pty`] is a running shell attached to a terminal: write keyboard input,
//! read output, resize, hang up. Linux and macOS share the implementation
//! here (`openpty` + `fork`/`exec` through `std::process`); Windows
//! implements the trait on ConPTY in `sard-windows`.

use std::io;

/// A shell running on a pseudo-terminal. All methods may be called from any
/// thread; `read` is meant for one dedicated reader thread.
pub trait Pty: Send + Sync {
    /// Keyboard input (terminal bytes, UTF-8 and escape sequences).
    fn write(&self, data: &[u8]) -> io::Result<()>;
    /// Blocking read of output; `Ok(0)` once the shell has exited and its
    /// output is drained.
    fn read(&self, buf: &mut [u8]) -> io::Result<usize>;
    /// New window size in character cells (the shell gets SIGWINCH).
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;
    /// End the shell and everything it started in the terminal.
    fn hangup(&self);
    /// After `read` returned 0: how the shell ended ("exited with status 0").
    fn exit_message(&self) -> String;
    /// Short name of the program ("zsh").
    fn name(&self) -> String;
}

/// Window size bounds: a bogus size must not make the shell misbehave.
pub fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(2, 1000), rows.clamp(1, 500))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use unix::spawn;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix {
    use super::{Pty, clamp_size};
    use std::ffi::CStr;
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex;

    struct UnixPty {
        master: File,
        child: Mutex<Child>,
        pid: i32,
        name: String,
    }

    /// The user's login shell (from the password database, then `$SHELL`).
    fn login_shell() -> String {
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0u8; 4096];
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: valid buffers; `out` points into `pw` on success.
        let rc = unsafe { libc::getpwuid_r(libc::getuid(), &mut pw, buf.as_mut_ptr().cast(), buf.len(), &mut out) };
        if rc == 0 && !out.is_null() && !pw.pw_shell.is_null() {
            // SAFETY: NUL-terminated string inside `buf`.
            let s = unsafe { CStr::from_ptr(pw.pw_shell) }.to_string_lossy().into_owned();
            if !s.is_empty() && std::path::Path::new(&s).exists() {
                return s;
            }
        }
        std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into())
    }

    fn set_size(fd: i32, cols: u16, rows: u16) -> io::Result<()> {
        let (cols, rows) = clamp_size(cols, rows);
        let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: valid fd and winsize.
        if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Start the user's login shell in their home directory on a new
    /// pseudo-terminal of `cols` x `rows` cells.
    pub fn spawn(cols: u16, rows: u16) -> io::Result<Box<dyn Pty>> {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: out-pointers to two ints; no name buffer, default termios.
        if unsafe {
            libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty returned two fresh descriptors we now own.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        // SAFETY: valid fd; our copy of the master must not leak into the shell.
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        set_size(master.as_raw_fd(), cols, rows)?;

        let shell = login_shell();
        let name = std::path::Path::new(&shell).file_name().map_or("sh".into(), |n| n.to_string_lossy().into_owned());
        let home = std::env::var_os("HOME").filter(|h| std::path::Path::new(h).is_dir());
        let mut cmd = Command::new(&shell);
        // A leading '-' makes it a login shell: profile files run, so PATH and
        // friends match a local Terminal window.
        cmd.arg0(format!("-{name}"))
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave))
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "Sard")
            .env_remove("SARD_CONFIG_DIR");
        if let Some(h) = &home {
            cmd.current_dir(h);
        }
        // GUI apps (the menu bar app) start without a locale; shells and
        // editors then fall back to ASCII.
        if std::env::var_os("LANG").is_none() && std::env::var_os("LC_ALL").is_none() {
            cmd.env("LANG", "en_US.UTF-8");
        }
        // SAFETY: runs in the forked child before exec; setsid and ioctl are
        // async-signal-safe. The terminal (stdin) becomes the controlling
        // terminal of the new session, so job control and ^C work.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        let pid = child.id() as i32;
        Ok(Box::new(UnixPty { master: File::from(master), child: Mutex::new(child), pid, name }))
    }

    impl Pty for UnixPty {
        fn write(&self, data: &[u8]) -> io::Result<()> {
            (&self.master).write_all(data)
        }

        fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            loop {
                match (&self.master).read(buf) {
                    // Linux reports EIO once the last slave descriptor is closed.
                    Err(e) if e.raw_os_error() == Some(libc::EIO) => return Ok(0),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    r => return r,
                }
            }
        }

        fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
            set_size(self.master.as_raw_fd(), cols, rows)
        }

        fn hangup(&self) {
            // SAFETY: plain signals to the shell's process group (it leads its
            // own session, so its pgid is its pid).
            unsafe {
                libc::kill(-self.pid, libc::SIGHUP);
                libc::kill(-self.pid, libc::SIGCONT);
            }
        }

        fn exit_message(&self) -> String {
            let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
            // The shell normally exited when its output ended; don't hang on
            // a background job that kept the terminal open.
            for _ in 0..20 {
                match child.try_wait() {
                    Ok(Some(st)) => {
                        return match (st.code(), st.signal()) {
                            (Some(c), _) => format!("exited with status {c}"),
                            (None, Some(s)) => format!("ended by signal {s}"),
                            _ => "ended".into(),
                        };
                    }
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
            "ended".into()
        }

        fn name(&self) -> String {
            self.name.clone()
        }
    }

    impl Drop for UnixPty {
        fn drop(&mut self) {
            let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(child.try_wait(), Ok(None)) {
                self.hangup();
                // Reap in the background so no zombie is left behind.
                let pid = self.pid;
                let _ = std::thread::Builder::new().name("sard-pty-reap".into()).spawn(move || {
                    let mut status = 0;
                    // SAFETY: waiting for our own (not yet reaped) child.
                    unsafe { libc::waitpid(pid, &mut status, 0) };
                });
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::{Duration, Instant};

        fn read_until(p: &dyn Pty, needle: &str, timeout: Duration) -> String {
            let deadline = Instant::now() + timeout;
            let mut all = Vec::new();
            let mut buf = [0u8; 4096];
            while Instant::now() < deadline {
                let n = p.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                all.extend_from_slice(&buf[..n]);
                if String::from_utf8_lossy(&all).contains(needle) {
                    break;
                }
            }
            String::from_utf8_lossy(&all).into_owned()
        }

        #[test]
        fn shell_runs_commands_and_resizes() {
            let p = spawn(100, 30).unwrap();
            p.write(b"stty size; echo sard-$((40+2))\r").unwrap();
            let out = read_until(p.as_ref(), "sard-42", Duration::from_secs(20));
            assert!(out.contains("30 100"), "terminal size not applied: {out:?}");
            assert!(out.contains("sard-42"), "{out:?}");
            p.resize(120, 40).unwrap();
            p.write(b"stty size\r").unwrap();
            let out = read_until(p.as_ref(), "40 120", Duration::from_secs(10));
            assert!(out.contains("40 120"), "{out:?}");
            p.write(b"exit 3\r").unwrap();
            let _ = read_until(p.as_ref(), "\u{0}", Duration::from_secs(10));
            assert_eq!(p.exit_message(), "exited with status 3");
        }

        #[test]
        fn hangup_ends_the_shell() {
            let p = spawn(80, 24).unwrap();
            p.write(b"echo ready\r").unwrap();
            read_until(p.as_ref(), "ready", Duration::from_secs(20));
            p.hangup();
            let mut buf = [0u8; 4096];
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && p.read(&mut buf).unwrap() > 0 {}
            assert!(Instant::now() < deadline, "shell kept running after hangup");
        }
    }
}
