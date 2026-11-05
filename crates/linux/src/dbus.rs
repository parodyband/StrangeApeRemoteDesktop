//! Minimal synchronous D-Bus client (session bus), enough for
//! xdg-desktop-portal: method calls, signals, the full type system for
//! decoding replies, and Unix fd passing (`OpenPipeWireRemote`).
//!
//! Hand-written instead of zbus to keep the dependency tree (and build time)
//! tiny. Little-endian only, which is what every Linux bus speaks in practice.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, String>;

pub const METHOD_CALL: u8 = 1;
pub const METHOD_RETURN: u8 = 2;
pub const ERROR: u8 = 3;
pub const SIGNAL: u8 = 4;
pub const FLAG_NO_REPLY_EXPECTED: u8 = 0x1;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Byte(u8),
    Bool(bool),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F64(f64),
    Str(String),
    Path(String),
    Sig(String),
    /// Index into the message's fd list.
    Fd(u32),
    /// Element signature + items.
    Array(String, Vec<Value>),
    Struct(Vec<Value>),
    DictEntry(Box<Value>, Box<Value>),
    Variant(Box<Value>),
}

impl Value {
    pub fn signature(&self) -> String {
        match self {
            Value::Byte(_) => "y".into(),
            Value::Bool(_) => "b".into(),
            Value::I16(_) => "n".into(),
            Value::U16(_) => "q".into(),
            Value::I32(_) => "i".into(),
            Value::U32(_) => "u".into(),
            Value::I64(_) => "x".into(),
            Value::U64(_) => "t".into(),
            Value::F64(_) => "d".into(),
            Value::Str(_) => "s".into(),
            Value::Path(_) => "o".into(),
            Value::Sig(_) => "g".into(),
            Value::Fd(_) => "h".into(),
            Value::Array(e, _) => format!("a{e}"),
            Value::Struct(f) => format!("({})", f.iter().map(Value::signature).collect::<String>()),
            Value::DictEntry(k, v) => format!("{{{}{}}}", k.signature(), v.signature()),
            Value::Variant(_) => "v".into(),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self.unvariant() {
            Value::Str(s) | Value::Path(s) | Value::Sig(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_u32(&self) -> Option<u32> {
        match self.unvariant() {
            Value::U32(v) => Some(*v),
            Value::I32(v) => Some(*v as u32),
            Value::Byte(v) => Some(*v as u32),
            _ => None,
        }
    }

    pub fn as_i32(&self) -> Option<i32> {
        match self.unvariant() {
            Value::I32(v) => Some(*v),
            Value::U32(v) => Some(*v as i32),
            _ => None,
        }
    }

    pub fn unvariant(&self) -> &Value {
        match self {
            Value::Variant(v) => v.unvariant(),
            v => v,
        }
    }

    /// Look up `key` in an `a{sv}` / `a{s*}` dictionary.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self.unvariant() {
            Value::Array(_, items) => items.iter().find_map(|i| match i {
                Value::DictEntry(k, v) if k.as_str() == Some(key) => Some(v.unvariant()),
                _ => None,
            }),
            _ => None,
        }
    }

    pub fn items(&self) -> &[Value] {
        match self.unvariant() {
            Value::Array(_, items) | Value::Struct(items) => items,
            _ => &[],
        }
    }
}

/// Build an `a{sv}` dictionary.
pub fn dict(entries: Vec<(&str, Value)>) -> Value {
    Value::Array(
        "{sv}".into(),
        entries
            .into_iter()
            .map(|(k, v)| Value::DictEntry(Box::new(Value::Str(k.into())), Box::new(Value::Variant(Box::new(v)))))
            .collect(),
    )
}

fn alignment(sig: u8) -> usize {
    match sig {
        b'y' | b'g' | b'v' => 1,
        b'n' | b'q' => 2,
        b'x' | b't' | b'd' | b'(' | b'{' => 8,
        _ => 4,
    }
}

// ---------------------------------------------------------------------------
// Marshalling
// ---------------------------------------------------------------------------

struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn align(&mut self, n: usize) {
        while !self.buf.len().is_multiple_of(n) {
            self.buf.push(0);
        }
    }

    fn write(&mut self, v: &Value) {
        match v {
            Value::Byte(b) => self.buf.push(*b),
            Value::Bool(b) => self.u32(*b as u32),
            Value::I16(x) => {
                self.align(2);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Value::U16(x) => {
                self.align(2);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Value::I32(x) => {
                self.align(4);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Value::U32(x) | Value::Fd(x) => self.u32(*x),
            Value::I64(x) => {
                self.align(8);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Value::U64(x) => {
                self.align(8);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Value::F64(x) => {
                self.align(8);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Value::Str(s) | Value::Path(s) => {
                self.u32(s.len() as u32);
                self.buf.extend_from_slice(s.as_bytes());
                self.buf.push(0);
            }
            Value::Sig(s) => {
                self.buf.push(s.len() as u8);
                self.buf.extend_from_slice(s.as_bytes());
                self.buf.push(0);
            }
            Value::Array(esig, items) => {
                self.align(4);
                let len_pos = self.buf.len();
                self.buf.extend_from_slice(&[0; 4]);
                self.align(alignment(esig.as_bytes()[0]));
                let start = self.buf.len();
                for i in items {
                    self.write(i);
                }
                let len = (self.buf.len() - start) as u32;
                self.buf[len_pos..len_pos + 4].copy_from_slice(&len.to_le_bytes());
            }
            Value::Struct(fields) => {
                self.align(8);
                for f in fields {
                    self.write(f);
                }
            }
            Value::DictEntry(k, v) => {
                self.align(8);
                self.write(k);
                self.write(v);
            }
            Value::Variant(inner) => {
                self.write(&Value::Sig(inner.signature()));
                self.write(inner);
            }
        }
    }

    fn u32(&mut self, x: u32) {
        self.align(4);
        self.buf.extend_from_slice(&x.to_le_bytes());
    }
}

// ---------------------------------------------------------------------------
// Unmarshalling
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Ty {
    Basic(u8),
    Array(Box<Ty>),
    Struct(Vec<Ty>),
    Dict(Box<Ty>, Box<Ty>),
    Variant,
}

impl Ty {
    fn sig(&self) -> String {
        match self {
            Ty::Basic(c) => (*c as char).to_string(),
            Ty::Array(e) => format!("a{}", e.sig()),
            Ty::Struct(f) => format!("({})", f.iter().map(Ty::sig).collect::<String>()),
            Ty::Dict(k, v) => format!("{{{}{}}}", k.sig(), v.sig()),
            Ty::Variant => "v".into(),
        }
    }
    fn align(&self) -> usize {
        alignment(self.sig().as_bytes()[0])
    }
}

fn parse_one(s: &[u8], pos: &mut usize) -> Result<Ty> {
    let c = *s.get(*pos).ok_or("truncated signature")?;
    *pos += 1;
    Ok(match c {
        b'y' | b'b' | b'n' | b'q' | b'i' | b'u' | b'x' | b't' | b'd' | b's' | b'o' | b'g' | b'h' => Ty::Basic(c),
        b'v' => Ty::Variant,
        b'a' => {
            if s.get(*pos) == Some(&b'{') {
                *pos += 1;
                let k = parse_one(s, pos)?;
                let v = parse_one(s, pos)?;
                if s.get(*pos) != Some(&b'}') {
                    return Err("bad dict signature".into());
                }
                *pos += 1;
                Ty::Array(Box::new(Ty::Dict(Box::new(k), Box::new(v))))
            } else {
                Ty::Array(Box::new(parse_one(s, pos)?))
            }
        }
        b'(' => {
            let mut f = Vec::new();
            while s.get(*pos) != Some(&b')') {
                f.push(parse_one(s, pos)?);
            }
            *pos += 1;
            Ty::Struct(f)
        }
        _ => return Err(format!("unsupported signature char {}", c as char)),
    })
}

fn parse_sig(s: &str) -> Result<Vec<Ty>> {
    let b = s.as_bytes();
    let mut pos = 0;
    let mut v = Vec::new();
    while pos < b.len() {
        v.push(parse_one(b, &mut pos)?);
    }
    Ok(v)
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn align(&mut self, n: usize) {
        self.pos = self.pos.div_ceil(n) * n;
    }

    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.pos + n > self.buf.len() {
            return Err("truncated message".into());
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32> {
        self.align(4);
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn read(&mut self, t: &Ty) -> Result<Value> {
        Ok(match t {
            Ty::Basic(c) => match c {
                b'y' => Value::Byte(self.take(1)?[0]),
                b'b' => Value::Bool(self.u32()? != 0),
                b'n' => {
                    self.align(2);
                    Value::I16(i16::from_le_bytes(self.take(2)?.try_into().unwrap()))
                }
                b'q' => {
                    self.align(2);
                    Value::U16(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
                }
                b'i' => Value::I32(self.u32()? as i32),
                b'u' => Value::U32(self.u32()?),
                b'h' => Value::Fd(self.u32()?),
                b'x' | b't' | b'd' => {
                    self.align(8);
                    let b: [u8; 8] = self.take(8)?.try_into().unwrap();
                    match c {
                        b'x' => Value::I64(i64::from_le_bytes(b)),
                        b't' => Value::U64(u64::from_le_bytes(b)),
                        _ => Value::F64(f64::from_le_bytes(b)),
                    }
                }
                b's' | b'o' => {
                    let n = self.u32()? as usize;
                    let s = String::from_utf8_lossy(self.take(n)?).into_owned();
                    self.take(1)?;
                    if *c == b's' { Value::Str(s) } else { Value::Path(s) }
                }
                b'g' => {
                    let n = self.take(1)?[0] as usize;
                    let s = String::from_utf8_lossy(self.take(n)?).into_owned();
                    self.take(1)?;
                    Value::Sig(s)
                }
                _ => unreachable!(),
            },
            Ty::Array(e) => {
                let len = self.u32()? as usize;
                self.align(e.align());
                let end = self.pos + len;
                if end > self.buf.len() {
                    return Err("array overruns message".into());
                }
                let mut items = Vec::new();
                while self.pos < end {
                    items.push(self.read(e)?);
                }
                Value::Array(e.sig(), items)
            }
            Ty::Struct(f) => {
                self.align(8);
                Value::Struct(f.iter().map(|t| self.read(t)).collect::<Result<_>>()?)
            }
            Ty::Dict(k, v) => {
                self.align(8);
                Value::DictEntry(Box::new(self.read(k)?), Box::new(self.read(v)?))
            }
            Ty::Variant => {
                let sig = match self.read(&Ty::Basic(b'g'))? {
                    Value::Sig(s) => s,
                    _ => unreachable!(),
                };
                let tys = parse_sig(&sig)?;
                if tys.len() != 1 {
                    return Err("variant must hold one type".into());
                }
                Value::Variant(Box::new(self.read(&tys[0])?))
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct Message {
    pub msg_type: u8,
    pub flags: u8,
    pub serial: u32,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub reply_serial: Option<u32>,
    pub destination: Option<String>,
    pub sender: Option<String>,
    pub body: Vec<Value>,
    pub fds: Vec<OwnedFd>,
}

impl Message {
    fn encode(&self, args: &[Value]) -> Vec<u8> {
        let mut body = Writer { buf: Vec::new() };
        for a in args {
            body.write(a);
        }
        let sig: String = args.iter().map(Value::signature).collect();
        let mut fields = Vec::new();
        let field = |code: u8, v: Value| Value::Struct(vec![Value::Byte(code), Value::Variant(Box::new(v))]);
        if let Some(p) = &self.path {
            fields.push(field(1, Value::Path(p.clone())));
        }
        if let Some(i) = &self.interface {
            fields.push(field(2, Value::Str(i.clone())));
        }
        if let Some(m) = &self.member {
            fields.push(field(3, Value::Str(m.clone())));
        }
        if let Some(d) = &self.destination {
            fields.push(field(6, Value::Str(d.clone())));
        }
        if !sig.is_empty() {
            fields.push(field(8, Value::Sig(sig)));
        }
        let mut w = Writer { buf: Vec::with_capacity(128 + body.buf.len()) };
        w.buf.extend_from_slice(&[b'l', self.msg_type, self.flags, 1]);
        w.buf.extend_from_slice(&(body.buf.len() as u32).to_le_bytes());
        w.buf.extend_from_slice(&self.serial.to_le_bytes());
        w.write(&Value::Array("(yv)".into(), fields));
        w.align(8);
        w.buf.extend_from_slice(&body.buf);
        w.buf
    }

    /// Total length of the message starting at `buf`, if the fixed header is available.
    fn total_len(buf: &[u8]) -> Result<Option<usize>> {
        if buf.len() < 16 {
            return Ok(None);
        }
        if buf[0] != b'l' {
            return Err("big-endian D-Bus messages are not supported".into());
        }
        let body = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        let fields = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
        Ok(Some((16 + fields).div_ceil(8) * 8 + body))
    }

    fn decode(buf: &[u8], fds: &mut VecDeque<OwnedFd>) -> Result<Message> {
        let mut r = Reader { buf, pos: 12 };
        let fields = r.read(&Ty::Array(Box::new(Ty::Struct(vec![Ty::Basic(b'y'), Ty::Variant]))))?;
        r.align(8);
        let mut m = Message {
            msg_type: buf[1],
            flags: buf[2],
            serial: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            ..Default::default()
        };
        let mut sig = String::new();
        let mut n_fds = 0;
        for f in fields.items() {
            let it = f.items();
            let (Some(Value::Byte(code)), Some(v)) = (it.first(), it.get(1)) else { continue };
            let v = v.unvariant();
            match code {
                1 => m.path = v.as_str().map(String::from),
                2 => m.interface = v.as_str().map(String::from),
                3 => m.member = v.as_str().map(String::from),
                4 => m.error_name = v.as_str().map(String::from),
                5 => m.reply_serial = v.as_u32(),
                6 => m.destination = v.as_str().map(String::from),
                7 => m.sender = v.as_str().map(String::from),
                8 => sig = v.as_str().unwrap_or("").to_string(),
                9 => n_fds = v.as_u32().unwrap_or(0),
                _ => {}
            }
        }
        for _ in 0..n_fds {
            if let Some(fd) = fds.pop_front() {
                m.fds.push(fd);
            }
        }
        let start = r.pos;
        let mut br = Reader { buf: &buf[start..], pos: 0 };
        for t in parse_sig(&sig)? {
            m.body.push(br.read(&t)?);
        }
        Ok(m)
    }

    pub fn error_text(&self) -> String {
        let detail = self.body.first().and_then(Value::as_str).unwrap_or("");
        format!("{}: {detail}", self.error_name.as_deref().unwrap_or("error"))
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

struct ReadState {
    buf: Vec<u8>,
    fds: VecDeque<OwnedFd>,
    inbox: VecDeque<Message>,
}

pub struct Connection {
    sock: UnixStream,
    serial: Mutex<u32>,
    read: Mutex<ReadState>,
    write: Mutex<()>,
    pub unique_name: String,
}

fn session_socket() -> Result<UnixStream> {
    let addr = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().unwrap_or_else(|| {
        // SAFETY: getuid never fails.
        format!("unix:path=/run/user/{}/bus", unsafe { libc::getuid() })
    });
    for part in addr.split(';') {
        let Some(rest) = part.strip_prefix("unix:") else { continue };
        for kv in rest.split(',') {
            if let Some(p) = kv.strip_prefix("path=") {
                return UnixStream::connect(p).map_err(|e| format!("connect {p}: {e}"));
            }
            if let Some(name) = kv.strip_prefix("abstract=") {
                use std::os::linux::net::SocketAddrExt;
                let a =
                    std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).map_err(|e| e.to_string())?;
                return UnixStream::connect_addr(&a).map_err(|e| format!("connect @{name}: {e}"));
            }
        }
    }
    Err(format!("unsupported DBUS_SESSION_BUS_ADDRESS {addr}"))
}

impl Connection {
    /// Connect and authenticate to the session bus.
    pub fn session() -> Result<Self> {
        let mut sock = session_socket()?;
        sock.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
        // SASL EXTERNAL with our uid.
        // SAFETY: getuid never fails.
        let uid = unsafe { libc::getuid() }.to_string();
        let hex: String = uid.bytes().map(|b| format!("{b:02x}")).collect();
        let line = |s: &mut UnixStream| -> Result<String> {
            let mut out = Vec::new();
            let mut b = [0u8; 1];
            while !out.ends_with(b"\r\n") {
                s.read_exact(&mut b).map_err(|e| format!("dbus auth: {e}"))?;
                out.push(b[0]);
            }
            Ok(String::from_utf8_lossy(&out).trim_end().to_string())
        };
        sock.write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes()).map_err(|e| e.to_string())?;
        let ok = line(&mut sock)?;
        if !ok.starts_with("OK") {
            return Err(format!("dbus auth rejected: {ok}"));
        }
        sock.write_all(b"NEGOTIATE_UNIX_FD\r\n").map_err(|e| e.to_string())?;
        let agree = line(&mut sock)?;
        if !agree.starts_with("AGREE_UNIX_FD") {
            return Err(format!("dbus: no fd passing ({agree})"));
        }
        sock.write_all(b"BEGIN\r\n").map_err(|e| e.to_string())?;
        let mut c = Connection {
            sock,
            serial: Mutex::new(0),
            read: Mutex::new(ReadState { buf: Vec::new(), fds: VecDeque::new(), inbox: VecDeque::new() }),
            write: Mutex::new(()),
            unique_name: String::new(),
        };
        let reply = c.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello", &[])?;
        c.unique_name = reply.body.first().and_then(Value::as_str).unwrap_or_default().to_string();
        Ok(c)
    }

    fn next_serial(&self) -> u32 {
        let mut s = self.serial.lock().unwrap();
        *s += 1;
        *s
    }

    fn send(&self, msg: &Message, args: &[Value]) -> Result<u32> {
        let mut m = Message { serial: self.next_serial(), ..Default::default() };
        m.msg_type = msg.msg_type;
        m.flags = msg.flags;
        m.path = msg.path.clone();
        m.interface = msg.interface.clone();
        m.member = msg.member.clone();
        m.destination = msg.destination.clone();
        let bytes = m.encode(args);
        let _g = self.write.lock().unwrap();
        (&self.sock).write_all(&bytes).map_err(|e| format!("dbus write: {e}"))?;
        Ok(m.serial)
    }

    fn method(dest: &str, path: &str, iface: &str, member: &str, flags: u8) -> Message {
        Message {
            msg_type: METHOD_CALL,
            flags,
            path: Some(path.into()),
            interface: Some(iface.into()),
            member: Some(member.into()),
            destination: Some(dest.into()),
            ..Default::default()
        }
    }

    /// Call a method and wait for its reply (signals received meanwhile are queued).
    pub fn call(&self, dest: &str, path: &str, iface: &str, member: &str, args: &[Value]) -> Result<Message> {
        let serial = self.send(&Self::method(dest, path, iface, member, 0), args)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let reply = self.wait(
            |m| m.reply_serial == Some(serial) && (m.msg_type == METHOD_RETURN || m.msg_type == ERROR),
            deadline,
        )?;
        if reply.msg_type == ERROR {
            return Err(format!("{iface}.{member}: {}", reply.error_text()));
        }
        Ok(reply)
    }

    /// Fire-and-forget method call (used for high-rate input events).
    pub fn call_noreply(&self, dest: &str, path: &str, iface: &str, member: &str, args: &[Value]) -> Result<()> {
        self.send(&Self::method(dest, path, iface, member, FLAG_NO_REPLY_EXPECTED), args).map(|_| ())
    }

    /// Wait for a message matching `pred` (checking queued ones first).
    pub fn wait(&self, pred: impl Fn(&Message) -> bool, deadline: Instant) -> Result<Message> {
        let mut st = self.read.lock().unwrap();
        loop {
            if let Some(i) = st.inbox.iter().position(&pred) {
                return Ok(st.inbox.remove(i).unwrap());
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for D-Bus message".into());
            }
            self.read_some(&mut st, deadline)?;
        }
    }

    fn read_some(&self, st: &mut ReadState, deadline: Instant) -> Result<()> {
        let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        self.sock.set_read_timeout(Some(left.min(Duration::from_millis(500)))).map_err(|e| e.to_string())?;
        let mut data = [0u8; 16384];
        let mut cmsg = [0u8; 256];
        let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: data.len() };
        // SAFETY: zeroed msghdr filled with valid buffers.
        let mut mh: libc::msghdr = unsafe { std::mem::zeroed() };
        mh.msg_iov = &mut iov;
        mh.msg_iovlen = 1;
        mh.msg_control = cmsg.as_mut_ptr().cast();
        mh.msg_controllen = cmsg.len();
        // SAFETY: valid socket and msghdr.
        let n = unsafe { libc::recvmsg(self.sock.as_raw_fd(), &mut mh, libc::MSG_CMSG_CLOEXEC) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) {
                return Ok(());
            }
            return Err(format!("dbus read: {e}"));
        }
        if n == 0 {
            return Err("D-Bus connection closed".into());
        }
        // Collect passed fds.
        // SAFETY: iterating control messages of the msghdr we just filled.
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&mh);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(c) as *const i32;
                    let count = ((*c).cmsg_len - libc::CMSG_LEN(0) as usize) / 4;
                    for i in 0..count {
                        st.fds.push_back(OwnedFd::from_raw_fd(*data.add(i)));
                    }
                }
                c = libc::CMSG_NXTHDR(&mh, c);
            }
        }
        st.buf.extend_from_slice(&data[..n as usize]);
        while let Some(len) = Message::total_len(&st.buf)? {
            if st.buf.len() < len {
                break;
            }
            let raw: Vec<u8> = st.buf.drain(..len).collect();
            let msg = Message::decode(&raw, &mut st.fds)?;
            st.inbox.push_back(msg);
        }
        // Don't let unrelated signals pile up forever.
        while st.inbox.len() > 256 {
            st.inbox.pop_front();
        }
        Ok(())
    }

    pub fn add_match(&self, rule: &str) -> Result<()> {
        self.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "AddMatch",
            &[Value::Str(rule.into())],
        )
        .map(|_| ())
    }

    pub fn remove_match(&self, rule: &str) {
        let _ = self.call_noreply(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RemoveMatch",
            &[Value::Str(rule.into())],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marshal_roundtrip() {
        let v = vec![
            Value::Path("/a/b".into()),
            dict(vec![
                ("handle_token", Value::Str("t1".into())),
                ("types", Value::U32(3)),
                ("multiple", Value::Bool(false)),
                ("x", Value::F64(1.5)),
            ]),
            Value::Array(
                "(ua{sv})".into(),
                vec![Value::Struct(vec![
                    Value::U32(42),
                    dict(vec![("size", Value::Struct(vec![Value::I32(2560), Value::I32(1440)]))]),
                ])],
            ),
            Value::Byte(7),
            Value::U64(u64::MAX),
        ];
        let msg = Message { msg_type: METHOD_RETURN, serial: 9, ..Default::default() };
        let bytes = msg.encode(&v);
        assert_eq!(Message::total_len(&bytes).unwrap(), Some(bytes.len()));
        let mut fds = VecDeque::new();
        let d = Message::decode(&bytes, &mut fds).unwrap();
        assert_eq!(d.body, v);
        let streams = &d.body[2];
        let first = &streams.items()[0];
        assert_eq!(first.items()[0].as_u32(), Some(42));
        let size = first.items()[1].get("size").unwrap();
        assert_eq!(size.items()[0].as_i32(), Some(2560));
        assert_eq!(d.body[1].get("types").unwrap().as_u32(), Some(3));
    }

    #[test]
    fn signature_parse() {
        let t = parse_sig("a{sv}(ii)ua(ua{sv})").unwrap();
        assert_eq!(t.iter().map(Ty::sig).collect::<String>(), "a{sv}(ii)ua(ua{sv})");
    }

    #[test]
    fn talks_to_session_bus() {
        // Skips when there is no session bus (CI).
        let Ok(c) = Connection::session() else { return };
        assert!(c.unique_name.starts_with(':'));
        let r = c.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "GetId", &[]).unwrap();
        assert_eq!(r.body[0].as_str().unwrap().len(), 32);
    }
}
