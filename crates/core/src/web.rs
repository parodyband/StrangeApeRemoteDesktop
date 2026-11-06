//! Tiny HTTPS / WebSocket client for the Connect Anywhere service.
//!
//! TLS comes from the system's libcurl, loaded at runtime (`dlopen`) like the
//! GPU libraries: curl opens the TCP + TLS connection (`CURLOPT_CONNECT_ONLY`,
//! certificates verified against the system store) and hands us a byte
//! stream, on which this module speaks just enough HTTP/1.1 and WebSocket
//! (RFC 6455) for short requests and one long-lived signalling socket. No TLS
//! crates, no async runtime.

use std::ffi::{CString, c_char, c_int, c_long, c_void};
use std::io;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

type EasyInit = unsafe extern "C" fn() -> *mut c_void;
type EasySetopt = unsafe extern "C" fn(*mut c_void, c_int, ...) -> c_int;
type EasyPerform = unsafe extern "C" fn(*mut c_void) -> c_int;
type EasyIo = unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut usize) -> c_int;
type EasyGetinfo = unsafe extern "C" fn(*mut c_void, c_int, ...) -> c_int;
type EasyCleanup = unsafe extern "C" fn(*mut c_void);
type EasyStrerror = unsafe extern "C" fn(c_int) -> *const c_char;
type GlobalInit = unsafe extern "C" fn(c_long) -> c_int;

const CURLOPT_URL: c_int = 10002;
const CURLOPT_NOSIGNAL: c_int = 99;
const CURLOPT_HTTP_VERSION: c_int = 84;
const CURLOPT_CONNECT_ONLY: c_int = 141;
const CURLOPT_CONNECTTIMEOUT_MS: c_int = 156;
const CURL_HTTP_VERSION_1_1: c_long = 2;
const CURLINFO_ACTIVESOCKET: c_int = 0x50_0000 + 44;
const CURLE_AGAIN: c_int = 81;
const CURL_GLOBAL_DEFAULT: c_long = 3;

struct Curl {
    init: EasyInit,
    setopt: EasySetopt,
    perform: EasyPerform,
    send: EasyIo,
    recv: EasyIo,
    getinfo: EasyGetinfo,
    cleanup: EasyCleanup,
    strerror: EasyStrerror,
}

fn curl() -> io::Result<&'static Curl> {
    static LIB: OnceLock<Result<Curl, String>> = OnceLock::new();
    LIB.get_or_init(load).as_ref().map_err(|e| io::Error::other(e.clone()))
}

fn load() -> Result<Curl, String> {
    let names: &[&str] = if cfg!(target_os = "macos") {
        &["/usr/lib/libcurl.4.dylib"]
    } else {
        &["libcurl.so.4", "libcurl-gnutls.so.4", "libcurl.so"]
    };
    let mut handle = std::ptr::null_mut();
    for n in names {
        let c = CString::new(*n).unwrap();
        // SAFETY: plain dlopen of a system library.
        handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if !handle.is_null() {
            break;
        }
    }
    if handle.is_null() {
        return Err("libcurl not found (install curl)".into());
    }
    let sym = |name: &str| -> Result<*mut c_void, String> {
        let c = CString::new(name).unwrap();
        // SAFETY: looking up a symbol in a library we keep loaded forever.
        let p = unsafe { libc::dlsym(handle, c.as_ptr()) };
        if p.is_null() { Err(format!("libcurl lacks {name}")) } else { Ok(p) }
    };
    // SAFETY: the symbols have exactly these C signatures (curl/easy.h).
    unsafe {
        let global: GlobalInit = std::mem::transmute(sym("curl_global_init")?);
        global(CURL_GLOBAL_DEFAULT);
        Ok(Curl {
            init: std::mem::transmute::<*mut c_void, EasyInit>(sym("curl_easy_init")?),
            setopt: std::mem::transmute::<*mut c_void, EasySetopt>(sym("curl_easy_setopt")?),
            perform: std::mem::transmute::<*mut c_void, EasyPerform>(sym("curl_easy_perform")?),
            send: std::mem::transmute::<*mut c_void, EasyIo>(sym("curl_easy_send")?),
            recv: std::mem::transmute::<*mut c_void, EasyIo>(sym("curl_easy_recv")?),
            getinfo: std::mem::transmute::<*mut c_void, EasyGetinfo>(sym("curl_easy_getinfo")?),
            cleanup: std::mem::transmute::<*mut c_void, EasyCleanup>(sym("curl_easy_cleanup")?),
            strerror: std::mem::transmute::<*mut c_void, EasyStrerror>(sym("curl_easy_strerror")?),
        })
    }
}

fn curl_err(c: &Curl, code: c_int, what: &str) -> io::Error {
    // SAFETY: curl_easy_strerror returns a static string.
    let msg = unsafe { std::ffi::CStr::from_ptr((c.strerror)(code)) }.to_string_lossy().into_owned();
    io::Error::other(format!("{what}: {msg}"))
}

/// A parsed `http(s)://` / `ws(s)://` URL.
#[derive(Clone, Debug, PartialEq)]
pub struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// Path and query, starting with '/'.
    pub path: String,
}

impl Url {
    pub fn parse(s: &str) -> io::Result<Url> {
        let bad = || io::Error::new(io::ErrorKind::InvalidInput, format!("bad URL {s}"));
        let (scheme, rest) = s.split_once("://").ok_or_else(bad)?;
        let tls = match scheme {
            "https" | "wss" => true,
            "http" | "ws" => false,
            _ => return Err(bad()),
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let default = if tls { 443 } else { 80 };
        let (host, port) = if authority.starts_with('[') {
            // [v6addr] or [v6addr]:port
            let end = authority.find(']').ok_or_else(bad)?;
            let port = match &authority[end + 1..] {
                "" => default,
                p => p.strip_prefix(':').and_then(|p| p.parse().ok()).ok_or_else(bad)?,
            };
            (authority[..=end].to_string(), port)
        } else {
            match authority.split_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().map_err(|_| bad())?),
                None => (authority.to_string(), default),
            }
        };
        if host.is_empty() || host.contains(|c: char| c.is_whitespace() || c == '@') {
            return Err(bad());
        }
        Ok(Url { tls, host, port, path: path.to_string() })
    }

    fn host_header(&self) -> String {
        if self.port == if self.tls { 443 } else { 80 } {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn join(&self, path: &str) -> Url {
        Url { path: path.to_string(), ..self.clone() }
    }
}

/// A TCP (+TLS) byte stream opened by libcurl.
pub struct Stream {
    lib: &'static Curl,
    easy: *mut c_void,
    fd: c_int,
}

// SAFETY: a curl easy handle may be used from any thread, one at a time;
// `Stream` is not `Sync`, so &mut access is exclusive.
unsafe impl Send for Stream {}

impl Stream {
    pub fn connect(url: &Url, timeout: Duration) -> io::Result<Stream> {
        let lib = curl()?;
        // SAFETY: standard easy-handle lifecycle; options take the documented types.
        unsafe {
            let easy = (lib.init)();
            if easy.is_null() {
                return Err(io::Error::other("curl_easy_init failed"));
            }
            // Owns the handle from here on: early returns clean it up.
            let mut s = Stream { lib, easy, fd: -1 };
            let target =
                CString::new(format!("{}://{}:{}/", if url.tls { "https" } else { "http" }, url.host, url.port))
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad host"))?;
            (lib.setopt)(easy, CURLOPT_URL, target.as_ptr());
            (lib.setopt)(easy, CURLOPT_NOSIGNAL, 1 as c_long);
            (lib.setopt)(easy, CURLOPT_CONNECT_ONLY, 1 as c_long);
            // We speak HTTP/1.1 ourselves: don't let ALPN pick h2.
            (lib.setopt)(easy, CURLOPT_HTTP_VERSION, CURL_HTTP_VERSION_1_1);
            (lib.setopt)(easy, CURLOPT_CONNECTTIMEOUT_MS, timeout.as_millis().min(c_long::MAX as u128) as c_long);
            let rc = (lib.perform)(easy);
            if rc != 0 {
                return Err(curl_err(lib, rc, &format!("connect {}", url.host)));
            }
            let mut fd: c_int = -1;
            let rc = (lib.getinfo)(easy, CURLINFO_ACTIVESOCKET, &mut fd as *mut c_int);
            if rc != 0 || fd < 0 {
                return Err(io::Error::other("no socket from curl"));
            }
            s.fd = fd;
            Ok(s)
        }
    }

    fn wait(&self, events: i16, until: Option<Instant>) -> io::Result<()> {
        let ms = match until {
            Some(t) => {
                let left = t.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
                }
                left.as_millis().min(i32::MAX as u128) as i32
            }
            None => -1,
        };
        let mut p = libc::pollfd { fd: self.fd, events, revents: 0 };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut p, 1, ms) };
        match n {
            0 => Err(io::Error::new(io::ErrorKind::TimedOut, "timed out")),
            n if n < 0 => {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted { Ok(()) } else { Err(e) }
            }
            _ => Ok(()),
        }
    }

    pub fn write_all(&mut self, mut data: &[u8], timeout: Duration) -> io::Result<()> {
        let until = Instant::now() + timeout;
        while !data.is_empty() {
            let mut n = 0usize;
            // SAFETY: valid handle and buffer.
            let rc = unsafe { (self.lib.send)(self.easy, data.as_ptr() as *mut c_void, data.len(), &mut n) };
            match rc {
                0 => data = &data[n..],
                CURLE_AGAIN => self.wait(libc::POLLOUT, Some(until))?,
                _ => return Err(curl_err(self.lib, rc, "send")),
            }
        }
        Ok(())
    }

    /// Read some bytes; `Ok(0)` at end of stream; `TimedOut` if nothing
    /// arrived before `until` (None = wait forever).
    pub fn read(&mut self, buf: &mut [u8], until: Option<Instant>) -> io::Result<usize> {
        loop {
            let mut n = 0usize;
            // SAFETY: valid handle and buffer.
            let rc = unsafe { (self.lib.recv)(self.easy, buf.as_mut_ptr().cast(), buf.len(), &mut n) };
            match rc {
                0 => return Ok(n),
                // TLS may hold decrypted bytes the socket no longer shows:
                // always try recv before polling.
                CURLE_AGAIN => self.wait(libc::POLLIN, until)?,
                _ => return Err(curl_err(self.lib, rc, "recv")),
            }
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the handle came from curl_easy_init and is not used again.
        unsafe { (self.lib.cleanup)(self.easy) };
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn request_head(method: &str, url: &Url, headers: &[(&str, &str)], extra: &str) -> String {
    let mut h = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: sard/{}\r\n",
        url.path,
        url.host_header(),
        env!("CARGO_PKG_VERSION")
    );
    for (k, v) in headers {
        h += &format!("{k}: {v}\r\n");
    }
    h += extra;
    h += "\r\n";
    h
}

/// Read until the end of the HTTP response head; returns (status, head, leftover body bytes).
fn read_head(s: &mut Stream, until: Instant) -> io::Result<(u16, String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).into_owned();
            let rest = buf[i + 4..].to_vec();
            let status = head
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse().ok())
                .ok_or_else(|| io::Error::other("bad HTTP response"))?;
            return Ok((status, head, rest));
        }
        if buf.len() > 64 << 10 {
            return Err(io::Error::other("HTTP header too large"));
        }
        let n = s.read(&mut chunk, Some(until))?;
        if n == 0 {
            return Err(io::Error::other("connection closed before the response"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// One HTTP request (`Connection: close`). Returns status and body.
pub fn request(
    method: &str,
    url: &Url,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> io::Result<(u16, Vec<u8>)> {
    let until = Instant::now() + timeout;
    let mut s = Stream::connect(url, timeout)?;
    let extra = format!("Content-Length: {}\r\nConnection: close\r\n", body.len());
    let head = request_head(method, url, headers, &extra);
    s.write_all(head.as_bytes(), timeout)?;
    s.write_all(body, timeout)?;
    let (status, head, mut data) = read_head(&mut s, until)?;
    let mut chunk = [0u8; 8192];
    loop {
        let n = s.read(&mut chunk, Some(until))?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..n]);
        if data.len() > 1 << 20 {
            return Err(io::Error::other("response too large"));
        }
        if let Some(len) = header(&head, "content-length").and_then(|v| v.parse::<usize>().ok())
            && data.len() >= len
        {
            data.truncate(len);
            break;
        }
    }
    if header(&head, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        data = dechunk(&data).ok_or_else(|| io::Error::other("bad chunked body"))?;
    }
    Ok((status, data))
}

fn dechunk(mut d: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let i = d.windows(2).position(|w| w == b"\r\n")?;
        let size = usize::from_str_radix(std::str::from_utf8(&d[..i]).ok()?.split(';').next()?.trim(), 16).ok()?;
        d = &d[i + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(d.get(..size)?);
        d = d.get(size + 2..)?;
    }
}

/// A client WebSocket carrying text messages.
pub struct WebSocket {
    s: Stream,
    buf: Vec<u8>,
}

impl WebSocket {
    pub fn connect(url: &Url, headers: &[(&str, &str)], timeout: Duration) -> io::Result<WebSocket> {
        let until = Instant::now() + timeout;
        let mut s = Stream::connect(url, timeout)?;
        let key = base64(&crate::rand::bytes::<16>());
        let extra = format!(
            "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
        );
        s.write_all(request_head("GET", url, headers, &extra).as_bytes(), timeout)?;
        let (status, head, rest) = read_head(&mut s, until)?;
        if status != 101 {
            let reason = head.lines().next().unwrap_or("").to_string();
            return Err(io::Error::new(
                if status == 401 || status == 403 { io::ErrorKind::PermissionDenied } else { io::ErrorKind::Other },
                format!("websocket refused: {reason}"),
            ));
        }
        Ok(WebSocket { s, buf: rest })
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut f = Vec::with_capacity(payload.len() + 14);
        f.push(0x80 | opcode);
        match payload.len() {
            n if n < 126 => f.push(0x80 | n as u8),
            n if n <= 0xFFFF => {
                f.push(0x80 | 126);
                f.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                f.push(0x80 | 127);
                f.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        // Client frames are masked (RFC 6455 §5.3).
        let mask: [u8; 4] = crate::rand::bytes();
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.s.write_all(&f, Duration::from_secs(10))
    }

    pub fn send_text(&mut self, text: &str) -> io::Result<()> {
        self.send_frame(0x1, text.as_bytes())
    }

    /// Next text message; `Ok(None)` if none arrived within `timeout`.
    /// Answers pings; a close frame or EOF is an error.
    pub fn recv(&mut self, timeout: Duration) -> io::Result<Option<String>> {
        let until = Instant::now() + timeout;
        loop {
            if let Some((opcode, payload, used)) = parse_frame(&self.buf)? {
                self.buf.drain(..used);
                match opcode {
                    0x1 | 0x2 => return Ok(Some(String::from_utf8_lossy(&payload).into_owned())),
                    0x8 => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "websocket closed by server")),
                    0x9 => self.send_frame(0xA, &payload)?,
                    _ => {}
                }
                continue;
            }
            let mut chunk = [0u8; 4096];
            let n = match self.s.read(&mut chunk, Some(until)) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::TimedOut => return Ok(None),
                Err(e) => return Err(e),
            };
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "websocket connection closed"));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    pub fn close(mut self) {
        let _ = self.send_frame(0x8, &1000u16.to_be_bytes());
    }
}

/// (opcode, payload, bytes used) of one complete unmasked server frame.
fn parse_frame(b: &[u8]) -> io::Result<Option<(u8, Vec<u8>, usize)>> {
    if b.len() < 2 {
        return Ok(None);
    }
    let opcode = b[0] & 0x0F;
    let masked = b[1] & 0x80 != 0;
    let (len, mut i) = match b[1] & 0x7F {
        126 if b.len() >= 4 => (u16::from_be_bytes([b[2], b[3]]) as usize, 4),
        127 if b.len() >= 10 => (u64::from_be_bytes(b[2..10].try_into().unwrap()) as usize, 10),
        126 | 127 => return Ok(None),
        n => (n as usize, 2),
    };
    if len > 1 << 20 {
        return Err(io::Error::other("websocket frame too large"));
    }
    let mask = if masked {
        let Some(m) = b.get(i..i + 4) else { return Ok(None) };
        i += 4;
        Some([m[0], m[1], m[2], m[3]])
    } else {
        None
    };
    let Some(p) = b.get(i..i + len) else { return Ok(None) };
    let payload = match mask {
        Some(m) => p.iter().enumerate().map(|(j, x)| x ^ m[j % 4]).collect(),
        None => p.to_vec(),
    };
    Ok(Some((opcode, payload, i + len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let u = Url::parse("wss://sard.example.workers.dev/v1/host?key=ab").unwrap();
        assert_eq!(
            (u.tls, u.host.as_str(), u.port, u.path.as_str()),
            (true, "sard.example.workers.dev", 443, "/v1/host?key=ab")
        );
        let u = Url::parse("http://127.0.0.1:8787").unwrap();
        assert_eq!((u.tls, u.host.as_str(), u.port, u.path.as_str()), (false, "127.0.0.1", 8787, "/"));
        assert_eq!(u.host_header(), "127.0.0.1:8787");
        assert!(Url::parse("ftp://x").is_err());
        assert!(Url::parse("https://user@x/").is_err());
    }

    #[test]
    fn base64_rfc4648() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn ipv6_url() {
        let u = Url::parse("ws://[::1]:8787/x").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("[::1]", 8787));
        assert_eq!(Url::parse("https://[::1]/").unwrap().port, 443);
    }

    #[test]
    fn frames() {
        assert_eq!(parse_frame(&[0x81, 0x02, b'h', b'i']).unwrap(), Some((1, b"hi".to_vec(), 4)));
        assert_eq!(parse_frame(&[0x81, 0x02, b'h']).unwrap(), None);
        let mut long = vec![0x82, 126, 0x01, 0x00];
        long.extend(std::iter::repeat_n(7u8, 256));
        assert_eq!(parse_frame(&long).unwrap().unwrap().2, 260);
        assert_eq!(dechunk(b"3\r\nabc\r\n1\r\nd\r\n0\r\n\r\n").unwrap(), b"abcd");
    }

    /// `cargo test -p sard-core web -- --ignored`: real TLS through libcurl.
    #[test]
    #[ignore]
    fn live_https() {
        let u = Url::parse("https://www.cloudflare.com/cdn-cgi/trace").unwrap();
        let (status, body) = request("GET", &u, &[], &[], Duration::from_secs(10)).unwrap();
        let text = String::from_utf8_lossy(&body);
        println!("{status}\n{text}");
        assert_eq!(status, 200);
        assert!(text.contains("tls="));
    }
}
