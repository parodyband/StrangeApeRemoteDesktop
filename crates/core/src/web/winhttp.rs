//! Windows implementation of [`super::request`] and [`super::WebSocket`] on
//! WinHTTP (synchronous mode). WinHTTP brings the system TLS stack,
//! certificate store and proxy settings, and a WebSocket client that answers
//! pings itself. A reader thread turns its blocking receive into
//! `recv(timeout)`.

use super::Url;
use std::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type HInternet = *mut c_void;

const WINHTTP_ACCESS_TYPE_DEFAULT_PROXY: u32 = 0;
const WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
const WINHTTP_ADDREQ_FLAG_ADD: u32 = 0x2000_0000;
const WINHTTP_ADDREQ_FLAG_REPLACE: u32 = 0x8000_0000;
const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
const WINHTTP_OPTION_UPGRADE_TO_WEB_SOCKET: u32 = 114;
const WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE: u32 = 0;
const WINHTTP_WEB_SOCKET_BINARY_FRAGMENT_BUFFER_TYPE: u32 = 1;
const WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE: u32 = 2;
const WINHTTP_WEB_SOCKET_UTF8_FRAGMENT_BUFFER_TYPE: u32 = 3;
const WINHTTP_WEB_SOCKET_CLOSE_BUFFER_TYPE: u32 = 4;
const WINHTTP_WEB_SOCKET_SUCCESS_CLOSE_STATUS: u16 = 1000;
const ERROR_WINHTTP_TIMEOUT: i32 = 12002;

#[link(name = "winhttp")]
unsafe extern "system" {
    fn WinHttpOpen(agent: *const u16, access: u32, proxy: *const u16, bypass: *const u16, flags: u32) -> HInternet;
    fn WinHttpSetTimeouts(h: HInternet, resolve: i32, connect: i32, send: i32, receive: i32) -> i32;
    fn WinHttpConnect(session: HInternet, server: *const u16, port: u16, reserved: u32) -> HInternet;
    fn WinHttpOpenRequest(
        connect: HInternet,
        verb: *const u16,
        object: *const u16,
        version: *const u16,
        referrer: *const u16,
        accept_types: *const *const u16,
        flags: u32,
    ) -> HInternet;
    fn WinHttpAddRequestHeaders(request: HInternet, headers: *const u16, len: u32, modifiers: u32) -> i32;
    fn WinHttpSendRequest(
        request: HInternet,
        headers: *const u16,
        headers_len: u32,
        optional: *const c_void,
        optional_len: u32,
        total_len: u32,
        context: usize,
    ) -> i32;
    fn WinHttpReceiveResponse(request: HInternet, reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(
        request: HInternet,
        level: u32,
        name: *const u16,
        buf: *mut c_void,
        len: *mut u32,
        index: *mut u32,
    ) -> i32;
    fn WinHttpReadData(request: HInternet, buf: *mut c_void, len: u32, read: *mut u32) -> i32;
    fn WinHttpSetOption(h: HInternet, option: u32, buf: *const c_void, len: u32) -> i32;
    fn WinHttpCloseHandle(h: HInternet) -> i32;
    fn WinHttpWebSocketCompleteUpgrade(request: HInternet, context: usize) -> HInternet;
    fn WinHttpWebSocketSend(ws: HInternet, kind: u32, buf: *const c_void, len: u32) -> u32;
    fn WinHttpWebSocketReceive(ws: HInternet, buf: *mut c_void, len: u32, read: *mut u32, kind: *mut u32) -> u32;
    fn WinHttpWebSocketShutdown(ws: HInternet, status: u16, reason: *const c_void, len: u32) -> u32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    let kind = if e.raw_os_error() == Some(ERROR_WINHTTP_TIMEOUT) { io::ErrorKind::TimedOut } else { e.kind() };
    io::Error::new(kind, format!("{what}: {e}"))
}

fn check(ok: i32, what: &str) -> io::Result<()> {
    if ok != 0 { Ok(()) } else { Err(last_error(what)) }
}

/// An owned WinHTTP handle.
struct Handle(HInternet);

// SAFETY: WinHTTP handles may be used from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    fn new(h: HInternet, what: &str) -> io::Result<Handle> {
        if h.is_null() { Err(last_error(what)) } else { Ok(Handle(h)) }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle we opened and close exactly once.
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

/// Session, connection and an opened (not yet sent) request.
struct Request {
    req: Handle,
    _conn: Handle,
    session: Handle,
}

fn open(method: &str, url: &Url, headers: &[(&str, &str)], timeout: Duration) -> io::Result<Request> {
    let agent = wide(&format!("sard/{}", env!("CARGO_PKG_VERSION")));
    // SAFETY: valid NUL-terminated strings; null proxy arguments mean "none".
    let session = unsafe {
        let h = WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, std::ptr::null(), std::ptr::null(), 0);
        if h.is_null() {
            // Before Windows 8.1: no automatic proxy mode.
            WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, std::ptr::null(), std::ptr::null(), 0)
        } else {
            h
        }
    };
    let session = Handle::new(session, "WinHttpOpen")?;
    let ms = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
    // SAFETY: valid session handle.
    check(unsafe { WinHttpSetTimeouts(session.0, ms, ms, ms, ms) }, "WinHttpSetTimeouts")?;
    let host = wide(url.host.trim_start_matches('[').trim_end_matches(']'));
    // SAFETY: valid handle and host string.
    let conn = Handle::new(unsafe { WinHttpConnect(session.0, host.as_ptr(), url.port, 0) }, "connect")?;
    let verb = wide(method);
    let path = wide(&url.path);
    let flags = if url.tls { WINHTTP_FLAG_SECURE } else { 0 };
    // SAFETY: valid strings; null version/referrer/accept types use the defaults.
    let req = unsafe {
        WinHttpOpenRequest(
            conn.0,
            verb.as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            flags,
        )
    };
    let req = Handle::new(req, "WinHttpOpenRequest")?;
    for (k, v) in headers {
        let line = wide(&format!("{k}: {v}"));
        // SAFETY: valid request handle; NUL-terminated header (length -1 = up to the NUL).
        let ok = unsafe {
            WinHttpAddRequestHeaders(
                req.0,
                line.as_ptr(),
                u32::MAX,
                WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE,
            )
        };
        check(ok, "WinHttpAddRequestHeaders")?;
    }
    Ok(Request { req, _conn: conn, session })
}

fn send(r: &Request, body: &[u8], host: &str) -> io::Result<u16> {
    let len = u32::try_from(body.len()).map_err(|_| io::Error::other("request body too large"))?;
    let ptr = if body.is_empty() { std::ptr::null() } else { body.as_ptr().cast() };
    // SAFETY: valid handle; body outlives the synchronous call.
    let ok = unsafe { WinHttpSendRequest(r.req.0, std::ptr::null(), 0, ptr, len, len, 0) };
    check(ok, &format!("connect {host}"))?;
    // SAFETY: valid handle.
    check(unsafe { WinHttpReceiveResponse(r.req.0, std::ptr::null_mut()) }, &format!("response from {host}"))?;
    let mut status = 0u32;
    let mut size = 4u32;
    // SAFETY: a DWORD out buffer of the stated size.
    let ok = unsafe {
        WinHttpQueryHeaders(
            r.req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            (&mut status as *mut u32).cast(),
            &mut size,
            std::ptr::null_mut(),
        )
    };
    check(ok, "status code")?;
    Ok(status as u16)
}

/// One HTTP(S) request. Returns status and body (WinHTTP undoes chunking).
pub fn request(
    method: &str,
    url: &Url,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> io::Result<(u16, Vec<u8>)> {
    let r = open(method, url, headers, timeout)?;
    let status = send(&r, body, &url.host)?;
    let mut data = Vec::new();
    let mut chunk = vec![0u8; 16 << 10];
    loop {
        let mut n = 0u32;
        // SAFETY: valid handle and buffer.
        let ok = unsafe { WinHttpReadData(r.req.0, chunk.as_mut_ptr().cast(), chunk.len() as u32, &mut n) };
        check(ok, "read response")?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..n as usize]);
        if data.len() > 1 << 20 {
            return Err(io::Error::other("response too large"));
        }
    }
    Ok((status, data))
}

struct WsShared {
    /// The socket handle; swapped to null when closed (closing cancels a pending receive).
    ws: AtomicPtr<c_void>,
    // Keeps the session alive as long as the socket.
    _session: Handle,
}

impl WsShared {
    fn handle(&self) -> HInternet {
        self.ws.load(Ordering::Acquire)
    }

    fn close(&self) {
        let h = self.ws.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !h.is_null() {
            // SAFETY: the socket handle, closed exactly once (the swap above).
            unsafe { WinHttpCloseHandle(h) };
        }
    }
}

impl Drop for WsShared {
    fn drop(&mut self) {
        self.close();
    }
}

/// A client WebSocket carrying text messages.
pub struct WebSocket {
    shared: Arc<WsShared>,
    rx: Receiver<io::Result<String>>,
    reader: Option<std::thread::JoinHandle<()>>,
    send_lock: Mutex<()>,
}

impl WebSocket {
    pub fn connect(url: &Url, headers: &[(&str, &str)], timeout: Duration) -> io::Result<WebSocket> {
        let r = open("GET", url, headers, timeout)?;
        // SAFETY: valid request handle; this option takes no buffer.
        let ok = unsafe { WinHttpSetOption(r.req.0, WINHTTP_OPTION_UPGRADE_TO_WEB_SOCKET, std::ptr::null(), 0) };
        check(ok, "WinHttpSetOption(UPGRADE_TO_WEB_SOCKET)")?;
        let status = send(&r, &[], &url.host)?;
        if status != 101 {
            return Err(io::Error::new(
                if status == 401 || status == 403 { io::ErrorKind::PermissionDenied } else { io::ErrorKind::Other },
                format!("websocket refused: HTTP {status}"),
            ));
        }
        // SAFETY: the request completed the 101 upgrade.
        let ws = unsafe { WinHttpWebSocketCompleteUpgrade(r.req.0, 0) };
        if ws.is_null() {
            return Err(last_error("websocket upgrade"));
        }
        // Long-lived socket: the receive timeout only bounds a silent server (the
        // service pings well within it); `recv(timeout)` is done with a channel.
        // SAFETY: valid session handle.
        unsafe { WinHttpSetTimeouts(r.session.0, 10_000, 10_000, 10_000, 0) };
        let Request { session, .. } = r;
        let shared = Arc::new(WsShared { ws: AtomicPtr::new(ws), _session: session });
        let (tx, rx) = mpsc::channel();
        let sh = Arc::clone(&shared);
        let reader = std::thread::Builder::new().name("sard-websocket".into()).spawn(move || {
            let mut msg = Vec::new();
            let mut buf = vec![0u8; 64 << 10];
            loop {
                let h = sh.handle();
                if h.is_null() {
                    return;
                }
                let (mut n, mut kind) = (0u32, 0u32);
                // SAFETY: socket handle (WinHTTP validates it if it was closed meanwhile)
                // and buffer; one receive at a time (this thread).
                let rc =
                    unsafe { WinHttpWebSocketReceive(h, buf.as_mut_ptr().cast(), buf.len() as u32, &mut n, &mut kind) };
                if rc != 0 {
                    let e = io::Error::from_raw_os_error(rc as i32);
                    let _ = tx.send(Err(io::Error::new(io::ErrorKind::ConnectionAborted, format!("websocket: {e}"))));
                    return;
                }
                match kind {
                    WINHTTP_WEB_SOCKET_CLOSE_BUFFER_TYPE => {
                        let _ = tx
                            .send(Err(io::Error::new(io::ErrorKind::ConnectionAborted, "websocket closed by server")));
                        return;
                    }
                    WINHTTP_WEB_SOCKET_UTF8_FRAGMENT_BUFFER_TYPE | WINHTTP_WEB_SOCKET_BINARY_FRAGMENT_BUFFER_TYPE => {
                        msg.extend_from_slice(&buf[..n as usize]);
                    }
                    WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE | WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE => {
                        msg.extend_from_slice(&buf[..n as usize]);
                        let text = String::from_utf8_lossy(&msg).into_owned();
                        msg.clear();
                        if tx.send(Ok(text)).is_err() {
                            return;
                        }
                    }
                    _ => {}
                }
                if msg.len() > 1 << 20 {
                    let _ = tx.send(Err(io::Error::other("websocket message too large")));
                    return;
                }
            }
        })?;
        Ok(WebSocket { shared, rx, reader: Some(reader), send_lock: Mutex::new(()) })
    }

    pub fn send_text(&mut self, text: &str) -> io::Result<()> {
        let _g = self.send_lock.lock().unwrap_or_else(|e| e.into_inner());
        let len = u32::try_from(text.len()).map_err(|_| io::Error::other("message too large"))?;
        // SAFETY: valid socket; WinHTTP allows one send concurrent with the reader's receive.
        let h = self.shared.handle();
        if h.is_null() {
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "websocket closed"));
        }
        let rc =
            unsafe { WinHttpWebSocketSend(h, WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE, text.as_ptr().cast(), len) };
        if rc != 0 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("websocket send: {}", io::Error::from_raw_os_error(rc as i32)),
            ));
        }
        Ok(())
    }

    /// Next text message; `Ok(None)` if none arrived within `timeout`.
    /// A close frame or a broken connection is an error.
    pub fn recv(&mut self, timeout: Duration) -> io::Result<Option<String>> {
        match self.rx.recv_timeout(timeout) {
            Ok(m) => m.map(Some),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                Err(io::Error::new(io::ErrorKind::ConnectionAborted, "websocket connection closed"))
            }
        }
    }

    pub fn close(self) {
        // Drop does the work.
    }
}

impl Drop for WebSocket {
    fn drop(&mut self) {
        // Send our close frame; the server's reply ends the reader's receive.
        let h = self.shared.handle();
        if !h.is_null() {
            // SAFETY: valid socket handle.
            unsafe { WinHttpWebSocketShutdown(h, WINHTTP_WEB_SOCKET_SUCCESS_CLOSE_STATUS, std::ptr::null(), 0) };
        }
        let wait = |r: &std::thread::JoinHandle<()>, d: Duration| {
            let deadline = std::time::Instant::now() + d;
            while !r.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        if let Some(r) = self.reader.take() {
            wait(&r, Duration::from_secs(1));
            // A server that doesn't answer: closing the handle cancels the receive.
            self.shared.close();
            wait(&r, Duration::from_secs(1));
        }
    }
}
