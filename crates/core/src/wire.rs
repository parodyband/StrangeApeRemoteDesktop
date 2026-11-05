//! Little-endian byte encoding helpers. No serde: every message is hand-encoded
//! so the wire format is explicit and stable.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireError;

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("malformed message")
    }
}

impl std::error::Error for WireError {}

pub type Result<T> = std::result::Result<T, WireError>;

/// Append-only encoder over a `Vec<u8>`.
pub trait Put {
    fn put_u8(&mut self, v: u8);
    fn put_u16(&mut self, v: u16);
    fn put_u32(&mut self, v: u32);
    fn put_u64(&mut self, v: u64);
    fn put_i16(&mut self, v: i16);
    fn put_i32(&mut self, v: i32);
    fn put_f32(&mut self, v: f32);
    fn put_bytes(&mut self, v: &[u8]);
    /// u16 length prefix + bytes (truncates at 65535).
    fn put_blob16(&mut self, v: &[u8]);
    /// u32 length prefix + bytes.
    fn put_blob32(&mut self, v: &[u8]);
    fn put_str(&mut self, s: &str);
}

impl Put for Vec<u8> {
    #[inline]
    fn put_u8(&mut self, v: u8) {
        self.push(v);
    }
    #[inline]
    fn put_u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn put_u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn put_u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn put_i16(&mut self, v: i16) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn put_i32(&mut self, v: i32) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn put_f32(&mut self, v: f32) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn put_bytes(&mut self, v: &[u8]) {
        self.extend_from_slice(v);
    }
    fn put_blob16(&mut self, v: &[u8]) {
        let n = v.len().min(u16::MAX as usize);
        self.put_u16(n as u16);
        self.extend_from_slice(&v[..n]);
    }
    fn put_blob32(&mut self, v: &[u8]) {
        self.put_u32(v.len() as u32);
        self.extend_from_slice(v);
    }
    fn put_str(&mut self, s: &str) {
        // Truncate on a char boundary so the result stays valid UTF-8.
        let mut n = s.len().min(u16::MAX as usize);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        self.put_blob16(&s.as_bytes()[..n]);
    }
}

/// Bounds-checked decoder.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    #[inline]
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(WireError);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    #[inline]
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.bytes(N)?.try_into().unwrap())
    }

    /// Everything not yet consumed.
    pub fn rest(&mut self) -> &'a [u8] {
        let s = &self.buf[self.pos..];
        self.pos = self.buf.len();
        s
    }

    #[inline]
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    #[inline]
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    #[inline]
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    #[inline]
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    #[inline]
    pub fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.array()?))
    }
    #[inline]
    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.array()?))
    }
    #[inline]
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.array()?))
    }
    pub fn blob16(&mut self) -> Result<&'a [u8]> {
        let n = self.u16()? as usize;
        self.bytes(n)
    }
    pub fn blob32(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.bytes(n)
    }
    pub fn str(&mut self) -> Result<String> {
        let b = self.blob16()?;
        String::from_utf8(b.to_vec()).map_err(|_| WireError)
    }
}

/// Hex encode (lowercase).
pub fn hex(bytes: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(D[(b >> 4) as usize] as char);
        s.push(D[(b & 15) as usize] as char);
    }
    s
}

/// Hex decode; `None` on any invalid character or odd length.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let val = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    s.as_bytes().chunks(2).map(|p| Some(val(p[0])? << 4 | val(p[1])?)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut v = Vec::new();
        v.put_u8(7);
        v.put_u16(0xBEEF);
        v.put_u32(0xDEAD_BEEF);
        v.put_u64(u64::MAX - 3);
        v.put_i16(-5);
        v.put_f32(1.5);
        v.put_str("héllo");
        v.put_blob32(&[1, 2, 3]);
        let mut r = Reader::new(&v);
        assert_eq!(r.u8().unwrap(), 7);
        assert_eq!(r.u16().unwrap(), 0xBEEF);
        assert_eq!(r.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.u64().unwrap(), u64::MAX - 3);
        assert_eq!(r.i16().unwrap(), -5);
        assert_eq!(r.f32().unwrap(), 1.5);
        assert_eq!(r.str().unwrap(), "héllo");
        assert_eq!(r.blob32().unwrap(), &[1, 2, 3]);
        assert_eq!(r.remaining(), 0);
        assert!(r.u8().is_err());
    }

    #[test]
    fn hex_roundtrip() {
        let b = [0u8, 1, 0xab, 0xff];
        assert_eq!(hex(&b), "0001abff");
        assert_eq!(unhex("0001ABff").unwrap(), b);
        assert!(unhex("0g").is_none());
    }
}
