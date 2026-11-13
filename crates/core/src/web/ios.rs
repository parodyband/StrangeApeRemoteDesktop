//! iOS byte stream for the HTTP / WebSocket client in `web.rs`.
//!
//! iOS has no public libcurl, and TLS belongs to the system anyway, so the
//! app hands us four functions backed by Network.framework (`NWConnection`
//! with TLS, certificates checked by the system) through [`set_transport`].
//! Calls block the calling thread, which is always one of our background
//! threads (Connect Anywhere signalling, relay credentials).

use std::ffi::{CString, c_char, c_void};
use std::io;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Functions the app provides. Timeouts are milliseconds; `read` takes -1 for
/// "wait forever".
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Transport {
    /// Open a TCP (+ TLS when `tls`) connection; null on failure.
    pub open: extern "C" fn(host: *const c_char, port: u16, tls: bool, timeout_ms: i32) -> *mut c_void,
    /// Send all of `data`: `len` on success, -1 on error, -2 on timeout.
    pub write: extern "C" fn(conn: *mut c_void, data: *const u8, len: usize, timeout_ms: i32) -> isize,
    /// Receive up to `len` bytes: the count, 0 at end of stream, -1 on error,
    /// -2 on timeout.
    pub read: extern "C" fn(conn: *mut c_void, buf: *mut u8, len: usize, timeout_ms: i32) -> isize,
    /// Close and free the connection.
    pub close: extern "C" fn(conn: *mut c_void),
}

static TRANSPORT: OnceLock<Transport> = OnceLock::new();

/// Install the app's transport (once, at startup).
pub fn set_transport(t: Transport) {
    let _ = TRANSPORT.set(t);
}

fn transport() -> io::Result<&'static Transport> {
    TRANSPORT.get().ok_or_else(|| io::Error::other("no network transport installed"))
}

fn ms(d: Duration) -> i32 {
    d.as_millis().clamp(1, i32::MAX as u128) as i32
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "timed out")
}

/// A TCP (+TLS) byte stream opened by the app's transport.
pub struct Stream {
    t: &'static Transport,
    conn: *mut c_void,
}

// SAFETY: the app's transport serializes access per connection; `Stream` is
// not `Sync`, so &mut access is exclusive.
unsafe impl Send for Stream {}

impl Stream {
    pub fn connect(url: &super::Url, timeout: Duration) -> io::Result<Stream> {
        let t = transport()?;
        let host = CString::new(url.host.trim_start_matches('[').trim_end_matches(']'))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad host"))?;
        let conn = (t.open)(host.as_ptr(), url.port, url.tls, ms(timeout));
        if conn.is_null() {
            return Err(io::Error::other(format!("cannot connect to {}", url.host)));
        }
        Ok(Stream { t, conn })
    }

    pub fn write_all(&mut self, data: &[u8], timeout: Duration) -> io::Result<()> {
        match (self.t.write)(self.conn, data.as_ptr(), data.len(), ms(timeout)) {
            n if n >= 0 => Ok(()),
            -2 => Err(timed_out()),
            _ => Err(io::Error::other("send failed")),
        }
    }

    /// Read some bytes; `Ok(0)` at end of stream; `TimedOut` if nothing
    /// arrived before `until` (None = wait forever).
    pub fn read(&mut self, buf: &mut [u8], until: Option<Instant>) -> io::Result<usize> {
        let timeout = match until {
            Some(t) => {
                let left = t.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(timed_out());
                }
                ms(left)
            }
            None => -1,
        };
        match (self.t.read)(self.conn, buf.as_mut_ptr(), buf.len(), timeout) {
            n if n >= 0 => Ok(n as usize),
            -2 => Err(timed_out()),
            _ => Err(io::Error::other("receive failed")),
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        (self.t.close)(self.conn);
    }
}
