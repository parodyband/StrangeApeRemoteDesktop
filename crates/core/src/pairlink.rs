//! Pairing links: what a host's pairing QR code holds, so a phone can pair
//! by scanning instead of typing.
//!
//! `sard://pair?a=192.168.1.20,10.0.0.5:47801&c=123456&k=<host key hex>&n=<name>`
//!
//! * `a`: addresses to try, in order (port only if not the default).
//! * `c`: the one-time pairing code (the PAKE still does the authenticating).
//! * `k`: the host's public key: the phone checks that the computer that
//!   answered is this one before saving it.
//! * `n`: the host's name, for display while pairing.

use crate::wire::{hex, unhex};

#[derive(Clone, Debug, PartialEq)]
pub struct PairLink {
    pub addrs: Vec<String>,
    pub code: String,
    pub key: Option<[u8; 32]>,
    pub name: String,
}

fn escape(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => o.push(b as char),
            _ => o += &format!("%{b:02X}"),
        }
    }
    o
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(if b[i] == b'+' { b' ' } else { b[i] });
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl PairLink {
    pub fn to_url(&self) -> String {
        let mut s = format!(
            "sard://pair?a={}&c={}",
            self.addrs.iter().map(|a| escape(a)).collect::<Vec<_>>().join(","),
            escape(&self.code)
        );
        if let Some(k) = &self.key {
            s += &format!("&k={}", hex(k));
        }
        if !self.name.is_empty() {
            s += &format!("&n={}", escape(&self.name));
        }
        s
    }

    /// Parse a scanned link; None if it isn't a Sard pairing link.
    pub fn parse(url: &str) -> Option<PairLink> {
        let rest = url.trim().strip_prefix("sard://pair")?;
        let query = rest.strip_prefix('/').unwrap_or(rest).strip_prefix('?')?;
        let mut link = PairLink { addrs: Vec::new(), code: String::new(), key: None, name: String::new() };
        for part in query.split('&') {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            match k {
                "a" => {
                    link.addrs = v
                        .split(',')
                        .map(unescape)
                        .map(|a| a.trim().to_string())
                        .filter(|a| !a.is_empty() && !a.contains(char::is_whitespace))
                        .collect()
                }
                "c" => link.code = unescape(v).chars().filter(char::is_ascii_digit).collect(),
                "k" => link.key = unhex(v).and_then(|b| b.try_into().ok()),
                "n" => link.name = crate::config::clean_display(&unescape(v)),
                _ => {}
            }
        }
        (!link.addrs.is_empty() && link.code.len() == 6).then_some(link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let l = PairLink {
            addrs: vec!["192.168.1.131".into(), "10.0.0.5:47801".into(), "fe80::1".into()],
            code: "104387".into(),
            key: Some([0xab; 32]),
            name: "Austin's Mac & PC".into(),
        };
        let url = l.to_url();
        assert!(url.starts_with("sard://pair?a=192.168.1.131,10.0.0.5%3A47801,"));
        assert_eq!(PairLink::parse(&url), Some(l));
    }

    #[test]
    fn rejects_other_links() {
        assert_eq!(PairLink::parse("https://example.com/?a=1&c=123456"), None);
        assert_eq!(PairLink::parse("sard://pair?a=1.2.3.4&c=12"), None, "code must be 6 digits");
        assert_eq!(PairLink::parse("sard://pair?c=123456"), None, "needs an address");
        let l = PairLink::parse("sard://pair?a=1.2.3.4&c=123%20456&k=zz").unwrap();
        assert_eq!((l.code.as_str(), l.key), ("123456", None));
    }
}
