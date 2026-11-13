//! Remote shells on a Windows pseudoconsole (ConPTY, Windows 10 1809+):
//! [`sard_core::pty::Pty`] for the host's terminal sessions.
//!
//! ```text
//!  write() ──▶ input pipe ──▶ conhost (pseudoconsole) ◀──▶ shell (pwsh / powershell / cmd)
//!  read()  ◀── output pipe ◀──┘   VT sequences out, VT input in
//! ```
//!
//! # How the session ends
//!
//! `read` must return `Ok(0)` once the shell has exited and its output is
//! drained. The output pipe only reaches end-of-file when conhost closes its
//! end, and conhost keeps running after the shell exits for as long as the
//! pseudoconsole is open. So a waiter thread waits for the shell process and
//! then calls `ClosePseudoConsole`: conhost paints a final frame (the shell's
//! last output) into the pipe, ends any other program still attached to the
//! console (jobs the shell left behind) and exits; the reader drains what is
//! left and then sees end-of-file (`ERROR_BROKEN_PIPE`).
//!
//! Before Windows 11 24H2, `ClosePseudoConsole` waits for conhost to exit,
//! and conhost can't exit while it is blocked writing into a full output
//! pipe. That is why closing happens on the waiter thread and never on the
//! thread that reads: the host's reader thread keeps calling `read` until it
//! returns 0, so the pipe keeps draining and the close completes. When the
//! [`ConPty`] is dropped instead (nobody reads any more), our end of the
//! output pipe is closed with it, so conhost's writes fail rather than block
//! and the waiter's close still completes.
//!
//! `hangup` terminates the shell process; the waiter then closes the
//! pseudoconsole as above, which ends everything else attached to it. It
//! doesn't call `ClosePseudoConsole` itself, so it never blocks the caller.

use crate::sys::{self, HANDLE, HPCON, HRESULT};
use sard_core::pty::{Pty, clamp_size};
use sard_core::{debug, warn};
use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

crate::dylib! {
    /// The pseudoconsole API, looked up at run time: sard still starts on
    /// Windows versions without it (only shells are unavailable there).
    struct ConPtyApi ["kernel32.dll"] {
        fn CreatePseudoConsole(sys::COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;
        fn ResizePseudoConsole(HPCON, sys::COORD) -> HRESULT;
        fn ClosePseudoConsole(HPCON);
    }
}

fn api() -> io::Result<&'static ConPtyApi> {
    static API: OnceLock<Result<ConPtyApi, String>> = OnceLock::new();
    API.get_or_init(|| ConPtyApi::load().map_err(|e| format!("{e}: terminals need Windows 10 version 1809 or later")))
        .as_ref()
        .map_err(|e| io::Error::other(e.clone()))
}

fn coord(cols: u16, rows: u16) -> sys::COORD {
    let (cols, rows) = clamp_size(cols, rows);
    sys::COORD { X: cols as i16, Y: rows as i16 }
}

fn hr_io(what: &str, hr: HRESULT) -> io::Error {
    io::Error::other(sys::hr_error(what, hr))
}

/// What the waiter thread and the [`ConPty`] share.
struct Shared {
    api: &'static ConPtyApi,
    /// The pseudoconsole until it is closed (taken by whoever closes it).
    /// Held locked while resizing, so it can't be closed in the middle.
    hpcon: Mutex<Option<HPCON>>,
    /// The shell process (owned; closed when the last user drops).
    process: HANDLE,
}

// SAFETY: a process handle and a pseudoconsole handle may be used from any
// thread; the pseudoconsole is only touched under its mutex.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    /// Close the pseudoconsole (once). May block until conhost has written
    /// its last output (see the module comment): not for the reader thread.
    fn close_console(&self) {
        let h = self.hpcon.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(h) = h {
            // SAFETY: a live pseudoconsole, closed exactly once (taken above).
            unsafe { (self.api.ClosePseudoConsole)(h) };
        }
    }

    fn exit_code(&self) -> Option<u32> {
        let mut code = 0u32;
        // SAFETY: valid process handle and out pointer.
        let ok = unsafe { sys::GetExitCodeProcess(self.process, &mut code) } != 0;
        (ok && code != sys::STILL_ACTIVE).then_some(code)
    }

    fn running(&self) -> bool {
        // SAFETY: valid process handle.
        (unsafe { sys::WaitForSingleObject(self.process, 0) }) == sys::WAIT_TIMEOUT
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Only reached with the console still open if the waiter never ran
        // (spawn failed half-way); our pipe ends are closed by then.
        self.close_console();
        // SAFETY: we own the process handle; nothing uses it any more.
        unsafe { sys::CloseHandle(self.process) };
    }
}

/// A shell on a pseudoconsole.
pub struct ConPty {
    shared: Arc<Shared>,
    /// Our end of the input pipe (keyboard bytes for conhost).
    input: File,
    /// Our end of the output pipe (VT output from conhost).
    output: File,
    name: String,
    hung_up: AtomicBool,
}

/// The shell to start: `pwsh.exe` on PATH, Windows PowerShell, `%COMSPEC%`.
fn find_shell() -> (PathBuf, &'static str) {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path).filter(|d| d.is_absolute()) {
            let p = dir.join("pwsh.exe");
            // `symlink_metadata`: the Store's pwsh is an app execution alias,
            // a reparse point `is_file` can't follow but CreateProcess runs.
            if p.is_file() || std::fs::symlink_metadata(&p).is_ok() {
                return (p, "pwsh");
            }
        }
    }
    let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    let ps = root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    if ps.is_file() {
        return (ps, "powershell");
    }
    let cmd = std::env::var_os("COMSPEC").map(PathBuf::from).filter(|p| p.is_file());
    (cmd.unwrap_or_else(|| root.join(r"System32\cmd.exe")), "cmd")
}

/// A Unicode environment block: ours, minus sard's own settings, plus
/// terminal hints. Sorted case-insensitively, as Windows expects.
fn environment() -> Vec<u16> {
    let ours = ["SARD_CONFIG_DIR", "COLORTERM", "TERM_PROGRAM"];
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| !ours.iter().any(|o| k.to_string_lossy().eq_ignore_ascii_case(o)))
        .collect();
    vars.push(("COLORTERM".into(), "truecolor".into()));
    vars.push(("TERM_PROGRAM".into(), "Sard".into()));
    vars.sort_by_cached_key(|(k, _)| k.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (k, v) in &vars {
        block.extend(k.encode_wide());
        block.push(u16::from(b'='));
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// `"C:\path\shell.exe" args`, NUL-terminated (CreateProcessW may write to it).
fn command_line(exe: &Path, args: &str) -> Vec<u16> {
    let mut s: Vec<u16> = OsStr::new("\"").encode_wide().collect();
    s.extend(exe.as_os_str().encode_wide());
    s.push(u16::from(b'"'));
    if !args.is_empty() {
        s.push(u16::from(b' '));
        s.extend(args.encode_utf16());
    }
    s.push(0);
    s
}

/// An anonymous pipe: (read end, write end).
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let (mut r, mut w) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: valid out pointers; default security (handles not inheritable).
    if unsafe { sys::CreatePipe(&mut r, &mut w, std::ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreatePipe returned two fresh handles we now own.
    Ok(unsafe { (OwnedHandle::from_raw_handle(r), OwnedHandle::from_raw_handle(w)) })
}

/// A `PROC_THREAD_ATTRIBUTE_LIST` with one attribute: the pseudoconsole.
struct AttributeList {
    buf: Vec<usize>,
}

impl AttributeList {
    fn pseudoconsole(hpcon: HPCON) -> io::Result<AttributeList> {
        let mut size = 0usize;
        // SAFETY: size query (fails with ERROR_INSUFFICIENT_BUFFER by design).
        unsafe { sys::InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut list = AttributeList { buf: vec![0usize; size.div_ceil(std::mem::size_of::<usize>())] };
        // SAFETY: buffer of at least `size` bytes, pointer-aligned.
        if unsafe { sys::InitializeProcThreadAttributeList(list.ptr(), 1, 0, &mut size) } == 0 {
            list.buf = Vec::new(); // not initialized: nothing to delete
            return Err(io::Error::last_os_error());
        }
        // SAFETY: initialized list with room for one attribute. The value of
        // this attribute is the HPCON itself (not a pointer to it), as in the
        // ConPTY documentation; the list must outlive CreateProcessW only.
        let ok = unsafe {
            sys::UpdateProcThreadAttribute(
                list.ptr(),
                0,
                sys::PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                hpcon,
                std::mem::size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(list)
    }

    fn ptr(&mut self) -> *mut c_void {
        self.buf.as_mut_ptr().cast()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            // SAFETY: a list initialized by InitializeProcThreadAttributeList.
            unsafe { sys::DeleteProcThreadAttributeList(self.ptr()) };
        }
    }
}

/// Start the user's shell in their profile folder on a new pseudoconsole of
/// `cols` x `rows` cells.
pub fn spawn(cols: u16, rows: u16) -> io::Result<Box<dyn Pty>> {
    let api = api()?;
    let (exe, name) = find_shell();
    // Ours: write keyboard input / read output. Conhost's: the other ends.
    let (pty_in, input) = pipe()?;
    let (output, pty_out) = pipe()?;
    let mut hpcon: HPCON = std::ptr::null_mut();
    // SAFETY: valid pipe handles (conhost duplicates them) and out pointer.
    let hr = unsafe {
        use std::os::windows::io::AsRawHandle;
        (api.CreatePseudoConsole)(coord(cols, rows), pty_in.as_raw_handle(), pty_out.as_raw_handle(), 0, &mut hpcon)
    };
    if hr < 0 {
        return Err(hr_io("CreatePseudoConsole", hr));
    }
    // Conhost has its own copies now. Ours must go, or the output pipe
    // would never reach end-of-file.
    drop((pty_in, pty_out));
    let input = File::from(input);
    let output = File::from(output);

    let mut attrs = match AttributeList::pseudoconsole(hpcon) {
        Ok(a) => a,
        Err(e) => {
            drop(output);
            // SAFETY: the pseudoconsole created above, no client attached;
            // our output end is closed, so conhost's writes can't block it.
            unsafe { (api.ClosePseudoConsole)(hpcon) };
            return Err(e);
        }
    };
    // SAFETY: zeroed POD structs; all-zero is valid for them.
    let mut si: sys::STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<sys::STARTUPINFOEXW>() as u32;
    // Without explicit (invalid) standard handles the shell can inherit
    // sard's own redirected ones (stderr goes to a log file in the tray app)
    // instead of using the pseudoconsole.
    si.StartupInfo.dwFlags = sys::STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = sys::INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdOutput = sys::INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdError = sys::INVALID_HANDLE_VALUE;
    si.lpAttributeList = attrs.ptr();
    let mut cmdline = command_line(&exe, if name == "cmd" { "" } else { "-NoLogo" });
    let env = environment();
    let home = std::env::var_os("USERPROFILE").filter(|h| Path::new(h).is_dir());
    let cwd: Option<Vec<u16>> = home.map(|h| h.encode_wide().chain(std::iter::once(0)).collect());
    // SAFETY: zeroed POD out-struct.
    let mut pi: sys::PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: valid, NUL-terminated command line (writable), environment
    // block and directory; the attribute list lives until after the call.
    let ok = unsafe {
        sys::CreateProcessW(
            std::ptr::null(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            sys::EXTENDED_STARTUPINFO_PRESENT | sys::CREATE_UNICODE_ENVIRONMENT,
            env.as_ptr().cast(),
            cwd.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            &si.StartupInfo,
            &mut pi,
        )
    };
    let err = io::Error::last_os_error();
    drop(attrs);
    if ok == 0 {
        drop(output);
        // SAFETY: as above (no client attached; our output end is closed).
        unsafe { (api.ClosePseudoConsole)(hpcon) };
        return Err(io::Error::new(err.kind(), format!("cannot start {}: {err}", exe.display())));
    }
    // SAFETY: we own the thread handle and don't need it.
    unsafe { sys::CloseHandle(pi.hThread) };

    let shared = Arc::new(Shared { api, hpcon: Mutex::new(Some(hpcon)), process: pi.hProcess });
    let waiter = Arc::clone(&shared);
    let r = std::thread::Builder::new().name("sard-conpty-wait".into()).spawn(move || {
        // SAFETY: valid process handle (kept open by `Shared`).
        unsafe { sys::WaitForSingleObject(waiter.process, sys::INFINITE) };
        // The shell is gone: close the console so the output pipe ends.
        waiter.close_console();
    });
    if let Err(e) = r {
        // Without the waiter, `read` would never end: don't start at all.
        // SAFETY: valid process handle.
        unsafe { sys::TerminateProcess(shared.process, 1) };
        drop(output);
        shared.close_console();
        return Err(e);
    }
    debug!("terminal: {} (pid {}) on a pseudoconsole", exe.display(), pi.dwProcessId);
    Ok(Box::new(ConPty { shared, input, output, name: name.into(), hung_up: AtomicBool::new(false) }))
}

impl Pty for ConPty {
    fn write(&self, data: &[u8]) -> io::Result<()> {
        (&self.input).write_all(data)
    }

    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match (&self.output).read(buf) {
                // Conhost closed its end: the console is gone, output drained.
                Err(e) if matches!(e.raw_os_error(), Some(sys::ERROR_BROKEN_PIPE | sys::ERROR_HANDLE_EOF)) => {
                    return Ok(0);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => return r,
            }
        }
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let h = self.shared.hpcon.lock().unwrap_or_else(|e| e.into_inner());
        let Some(h) = *h else { return Ok(()) }; // already closed: the shell has exited
        // SAFETY: live pseudoconsole; the lock keeps it from being closed meanwhile.
        let hr = unsafe { (self.shared.api.ResizePseudoConsole)(h, coord(cols, rows)) };
        if hr < 0 {
            return Err(hr_io("ResizePseudoConsole", hr));
        }
        Ok(())
    }

    fn hangup(&self) {
        if self.shared.running() {
            self.hung_up.store(true, Ordering::Relaxed);
            // SAFETY: valid process handle. The waiter wakes up and closes
            // the pseudoconsole, which ends the rest of the shell's programs.
            if unsafe { sys::TerminateProcess(self.shared.process, 1) } == 0 {
                warn!("cannot end the terminal's shell: {}", io::Error::last_os_error());
            }
        }
    }

    fn exit_message(&self) -> String {
        if self.hung_up.load(Ordering::Relaxed) {
            return "ended".into();
        }
        // The shell normally exited when the output ended; don't hang if not.
        for _ in 0..20 {
            if let Some(code) = self.shared.exit_code() {
                // NTSTATUS-style codes (0xC000013A: closed with Ctrl+C) read better in hex.
                return if code >= 0x8000_0000 {
                    format!("exited with status {code:#010X}")
                } else {
                    format!("exited with status {code}")
                };
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        "ended".into()
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

impl Drop for ConPty {
    fn drop(&mut self) {
        // Don't leave the shell running. The waiter then closes the
        // pseudoconsole; our pipe ends (closed right after this) make sure
        // conhost can't block it on a full output pipe.
        self.hangup();
    }
}
