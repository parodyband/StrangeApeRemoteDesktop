//! Minimal STUN (RFC 8489) Binding client: ask a public STUN server which
//! address and port our UDP socket appears as on the internet. That public
//! address is what the other side punches toward for a direct connection
//! across NATs ("Connect Anywhere").

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

/// Cloudflare's public STUN server.
pub const DEFAULT_SERVER: &str = "stun.cloudflare.com:3478";

const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// A Binding request with a fresh transaction id.
pub fn request() -> ([u8; 12], [u8; 20]) {
    let txid: [u8; 12] = crate::rand::bytes();
    let mut m = [0u8; 20];
    m[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // length 0: no attributes
    m[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    m[8..20].copy_from_slice(&txid);
    (txid, m)
}

/// Does `pkt` look like a STUN message (any kind)? Cheap pre-check so the
/// transport can tell STUN replies from its own packets on a shared socket.
pub fn is_stun(pkt: &[u8]) -> bool {
    pkt.len() >= 20
        && pkt[0] & 0xC0 == 0
        && u32::from_be_bytes(pkt[4..8].try_into().unwrap()) == MAGIC
        && u16::from_be_bytes(pkt[2..4].try_into().unwrap()) as usize == pkt.len() - 20
}

/// Transaction id of a STUN message (call after [`is_stun`]).
pub fn txid(pkt: &[u8]) -> [u8; 12] {
    pkt[8..20].try_into().unwrap()
}

/// The mapped address in a Binding success response for `txid`.
pub fn parse_response(pkt: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if !is_stun(pkt) || u16::from_be_bytes([pkt[0], pkt[1]]) != BINDING_SUCCESS || &pkt[8..20] != txid {
        return None;
    }
    let mut plain = None;
    let mut i = 20;
    while i + 4 <= pkt.len() {
        let ty = u16::from_be_bytes([pkt[i], pkt[i + 1]]);
        let len = u16::from_be_bytes([pkt[i + 2], pkt[i + 3]]) as usize;
        let v = pkt.get(i + 4..i + 4 + len)?;
        match ty {
            ATTR_XOR_MAPPED_ADDRESS => return decode_addr(v, Some(&pkt[4..20])),
            ATTR_MAPPED_ADDRESS => plain = decode_addr(v, None),
            _ => {}
        }
        i += 4 + len.div_ceil(4) * 4;
    }
    plain
}

/// MAPPED-ADDRESS / XOR-MAPPED-ADDRESS value. `xor` is cookie + txid.
fn decode_addr(v: &[u8], xor: Option<&[u8]>) -> Option<SocketAddr> {
    if v.len() < 4 {
        return None;
    }
    let family = v[1];
    let mut port = u16::from_be_bytes([v[2], v[3]]);
    if xor.is_some() {
        port ^= (MAGIC >> 16) as u16;
    }
    let ip = match (family, v.len()) {
        (0x01, 8) => {
            let mut b: [u8; 4] = v[4..8].try_into().unwrap();
            if let Some(x) = xor {
                for (o, k) in b.iter_mut().zip(x) {
                    *o ^= k;
                }
            }
            IpAddr::V4(Ipv4Addr::from(b))
        }
        (0x02, 20) => {
            let mut b: [u8; 16] = v[4..20].try_into().unwrap();
            if let Some(x) = xor {
                for (o, k) in b.iter_mut().zip(x) {
                    *o ^= k;
                }
            }
            IpAddr::V6(Ipv6Addr::from(b))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// Resolve a STUN server name to an address of the given family.
pub fn resolve(server: &str, ipv6: bool) -> Option<SocketAddr> {
    server.to_socket_addrs().ok()?.find(|a| a.is_ipv6() == ipv6)
}

/// Blocking query on a socket nobody else is reading (e.g. a client socket
/// before its connection starts). Retries a few times within `timeout`.
pub fn query(socket: &UdpSocket, server: SocketAddr, timeout: Duration) -> Option<SocketAddr> {
    let (txid, req) = request();
    let deadline = Instant::now() + timeout;
    let old = socket.read_timeout().ok().flatten();
    let mut buf = [0u8; 576];
    let mut result = None;
    'outer: while Instant::now() < deadline {
        if socket.send_to(&req, server).is_err() {
            break;
        }
        let until = (Instant::now() + Duration::from_millis(250)).min(deadline);
        while let Some(left) = until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
            let _ = socket.set_read_timeout(Some(left));
            match socket.recv_from(&mut buf) {
                Ok((n, from)) if from == server => {
                    if let Some(a) = parse_response(&buf[..n], &txid) {
                        result = Some(a);
                        break 'outer;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
    let _ = socket.set_read_timeout(old);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc5769_ipv4_response() {
        // RFC 5769 §2.2 sample IPv4 response (XOR-MAPPED-ADDRESS 192.0.2.1:32853).
        let hexs = concat!(
            "0101003c2112a442b7e7a701bc34d686fa87dfae",
            "8022000b7465737420766563746f7220",
            "002000080001a147e112a643",
            "000800142b91f599fd9e90c38c7489f92af9ba53f06be7d7",
            "80280004c07d4c96"
        );
        let pkt = crate::wire::unhex(hexs).unwrap();
        assert!(is_stun(&pkt));
        let tx: [u8; 12] = pkt[8..20].try_into().unwrap();
        assert_eq!(parse_response(&pkt, &tx), Some("192.0.2.1:32853".parse().unwrap()));
        assert_eq!(parse_response(&pkt, &[0u8; 12]), None);
    }

    #[test]
    fn request_shape() {
        let (tx, r) = request();
        assert!(is_stun(&r));
        assert_eq!(txid(&r), tx);
        // Our own transport packets are never mistaken for STUN.
        assert!(!is_stun(&[1, 2, 3]));
    }
}

#[cfg(test)]
mod live {
    /// `cargo test -p sard-core stun -- --ignored`: needs internet.
    #[test]
    #[ignore]
    fn cloudflare_stun() {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let server = super::resolve(super::DEFAULT_SERVER, false).expect("resolve");
        let public = super::query(&s, server, std::time::Duration::from_secs(3)).expect("no STUN answer");
        println!("public address: {public} (local port {})", s.local_addr().unwrap().port());
    }
}
