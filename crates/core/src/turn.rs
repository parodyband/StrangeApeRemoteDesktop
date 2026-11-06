//! TURN (RFC 8656) client, just enough to relay one UDP flow: when two
//! networks can't be connected directly (e.g. a phone hotspot whose NAT
//! picks a new public port per destination), the viewer allocates a relay
//! address on a TURN server (Cloudflare's) and talks to the host through it.
//!
//! Only the viewer speaks TURN. The host just sees packets from the relay
//! address, after punching toward it like toward any other viewer address.
//! Data goes in ChannelData framing (4-byte header) on one channel bound to
//! the host's public address. Our own end-to-end encryption is unchanged:
//! the relay sees only ciphertext.

use md5::Md5;
use sha1::{Digest, Sha1};
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

const MAGIC: u32 = 0x2112_A442;
const ALLOCATE: u16 = 0x0003;
const REFRESH: u16 = 0x0004;
const CHANNEL_BIND: u16 = 0x0009;
const SUCCESS: u16 = 0x0100;
const ERROR: u16 = 0x0110;

const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_ERROR_CODE: u16 = 0x0009;
const ATTR_CHANNEL_NUMBER: u16 = 0x000C;
const ATTR_LIFETIME: u16 = 0x000D;
const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
const ATTR_REALM: u16 = 0x0014;
const ATTR_NONCE: u16 = 0x0015;
const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// The one channel we use (0x4000..=0x4FFF are valid).
pub const CHANNEL: u16 = 0x4000;
/// Allocation, permission and channel all live 5–10 minutes: refresh well before.
pub const REFRESH_EVERY: Duration = Duration::from_secs(150);

/// Short-lived credentials for a TURN server (minted by the service).
#[derive(Clone, Debug)]
pub struct Credentials {
    pub server: SocketAddr,
    pub username: String,
    pub password: String,
}

fn sha1(parts: &[&[u8]]) -> [u8; 20] {
    let mut h = Sha1::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..20].copy_from_slice(&sha1(&[key]));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    sha1(&[&opad, &sha1(&[&ipad, msg])])
}

/// Long-term credential key: MD5(username ":" realm ":" password).
fn long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(format!("{username}:{realm}:{password}").as_bytes());
    h.finalize().into()
}

fn xor_addr(a: SocketAddr, txid: &[u8; 12]) -> Vec<u8> {
    let port = a.port() ^ (MAGIC >> 16) as u16;
    let mut v = vec![0, 0];
    v.extend_from_slice(&port.to_be_bytes());
    match a.ip() {
        IpAddr::V4(ip) => {
            v[1] = 0x01;
            v.extend(ip.octets().iter().zip(MAGIC.to_be_bytes()).map(|(b, k)| b ^ k));
        }
        IpAddr::V6(ip) => {
            v[1] = 0x02;
            let mut key = MAGIC.to_be_bytes().to_vec();
            key.extend_from_slice(txid);
            v.extend(ip.octets().iter().zip(key).map(|(b, k)| b ^ k));
        }
    }
    v
}

/// Encode a STUN message; with `key`, append MESSAGE-INTEGRITY.
fn encode(ty: u16, txid: &[u8; 12], attrs: &[(u16, Vec<u8>)], key: Option<&[u8; 16]>) -> Vec<u8> {
    let mut m = Vec::with_capacity(128);
    m.extend_from_slice(&ty.to_be_bytes());
    m.extend_from_slice(&[0, 0]);
    m.extend_from_slice(&MAGIC.to_be_bytes());
    m.extend_from_slice(txid);
    for (t, v) in attrs {
        m.extend_from_slice(&t.to_be_bytes());
        m.extend_from_slice(&(v.len() as u16).to_be_bytes());
        m.extend_from_slice(v);
        while m.len() % 4 != 0 {
            m.push(0);
        }
    }
    let set_len = |m: &mut Vec<u8>, extra: usize| {
        let len = (m.len() - 20 + extra) as u16;
        m[2..4].copy_from_slice(&len.to_be_bytes());
    };
    if let Some(k) = key {
        // The length covers the MESSAGE-INTEGRITY attribute itself (RFC 8489 §14.5).
        set_len(&mut m, 24);
        let mac = hmac_sha1(k, &m);
        m.extend_from_slice(&ATTR_MESSAGE_INTEGRITY.to_be_bytes());
        m.extend_from_slice(&20u16.to_be_bytes());
        m.extend_from_slice(&mac);
    } else {
        set_len(&mut m, 0);
    }
    m
}

struct Parsed<'a> {
    ty: u16,
    txid: [u8; 12],
    attrs: Vec<(u16, &'a [u8])>,
}

fn parse(pkt: &[u8]) -> Option<Parsed<'_>> {
    if !crate::stun::is_stun(pkt) {
        return None;
    }
    let mut attrs = Vec::new();
    let mut i = 20;
    while i + 4 <= pkt.len() {
        let t = u16::from_be_bytes([pkt[i], pkt[i + 1]]);
        let l = u16::from_be_bytes([pkt[i + 2], pkt[i + 3]]) as usize;
        attrs.push((t, pkt.get(i + 4..i + 4 + l)?));
        i += 4 + l.div_ceil(4) * 4;
    }
    Some(Parsed { ty: u16::from_be_bytes([pkt[0], pkt[1]]), txid: crate::stun::txid(pkt), attrs })
}

impl Parsed<'_> {
    fn attr(&self, t: u16) -> Option<&[u8]> {
        self.attrs.iter().find(|(a, _)| *a == t).map(|(_, v)| *v)
    }
    fn text(&self, t: u16) -> Option<String> {
        self.attr(t).map(|v| String::from_utf8_lossy(v).into_owned())
    }
    fn error(&self) -> Option<(u16, String)> {
        let v = self.attr(ATTR_ERROR_CODE)?;
        if v.len() < 4 {
            return None;
        }
        Some(((v[2] & 7) as u16 * 100 + v[3] as u16, String::from_utf8_lossy(&v[4..]).into_owned()))
    }
    fn xor_address(&self, t: u16) -> Option<SocketAddr> {
        // Reuse the STUN decoder: wrap the attribute in a fake success response.
        let v = self.attr(t)?;
        let mut fake = Vec::with_capacity(24 + v.len());
        fake.extend_from_slice(&0x0101u16.to_be_bytes());
        fake.extend_from_slice(&((4 + v.len().div_ceil(4) * 4) as u16).to_be_bytes());
        fake.extend_from_slice(&MAGIC.to_be_bytes());
        fake.extend_from_slice(&self.txid);
        fake.extend_from_slice(&0x0020u16.to_be_bytes());
        fake.extend_from_slice(&(v.len() as u16).to_be_bytes());
        fake.extend_from_slice(v);
        while fake.len() % 4 != 0 {
            fake.push(0);
        }
        crate::stun::parse_response(&fake, &self.txid)
    }
}

/// A relay allocation with one channel bound to `peer`.
#[derive(Clone, Debug)]
pub struct Allocation {
    pub server: SocketAddr,
    /// Our address on the relay: the host sends here.
    pub relayed: SocketAddr,
    /// Our public address as the TURN server sees it.
    pub mapped: Option<SocketAddr>,
    /// The host's public address the channel is bound to.
    pub peer: SocketAddr,
    username: String,
    realm: String,
    nonce: String,
    key: [u8; 16],
    pending: Vec<([u8; 12], u16)>,
}

impl Allocation {
    fn auth_attrs(&self) -> Vec<(u16, Vec<u8>)> {
        vec![
            (ATTR_USERNAME, self.username.as_bytes().to_vec()),
            (ATTR_REALM, self.realm.as_bytes().to_vec()),
            (ATTR_NONCE, self.nonce.as_bytes().to_vec()),
        ]
    }

    fn request(&mut self, ty: u16) -> Vec<u8> {
        let txid: [u8; 12] = crate::rand::bytes();
        let mut attrs = Vec::new();
        match ty {
            REFRESH => attrs.push((ATTR_LIFETIME, 600u32.to_be_bytes().to_vec())),
            CHANNEL_BIND => {
                attrs.push((ATTR_CHANNEL_NUMBER, vec![(CHANNEL >> 8) as u8, CHANNEL as u8, 0, 0]));
                attrs.push((ATTR_XOR_PEER_ADDRESS, xor_addr(self.peer, &txid)));
            }
            _ => {}
        }
        attrs.extend(self.auth_attrs());
        self.pending.retain(|(_, t)| *t != ty);
        self.pending.push((txid, ty));
        encode(ty, &txid, &attrs, Some(&self.key))
    }

    /// Requests that keep the allocation and channel alive; send every
    /// [`REFRESH_EVERY`] to the TURN server.
    pub fn refresh(&mut self) -> Vec<Vec<u8>> {
        vec![self.request(REFRESH), self.request(CHANNEL_BIND)]
    }

    /// Handle a STUN message from the TURN server. Returns requests to resend
    /// (after a stale nonce).
    pub fn on_response(&mut self, pkt: &[u8]) -> Vec<Vec<u8>> {
        let Some(p) = parse(pkt) else { return Vec::new() };
        let Some(i) = self.pending.iter().position(|(t, _)| *t == p.txid) else { return Vec::new() };
        let (_, ty) = self.pending.remove(i);
        if p.ty & 0x0110 == ERROR {
            match p.error() {
                Some((438, _)) => {
                    if let Some(n) = p.text(ATTR_NONCE) {
                        self.nonce = n;
                    }
                    return vec![self.request(ty)];
                }
                Some((code, reason)) => crate::warn!("TURN refresh failed: {code} {reason}"),
                None => {}
            }
        }
        Vec::new()
    }

    /// Frame `data` for the bound channel.
    pub fn wrap(&self, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(4 + data.len());
        v.extend_from_slice(&CHANNEL.to_be_bytes());
        v.extend_from_slice(&(data.len() as u16).to_be_bytes());
        v.extend_from_slice(data);
        v
    }

    /// Payload of a ChannelData packet on our channel.
    pub fn unwrap<'a>(&self, pkt: &'a [u8]) -> Option<&'a [u8]> {
        channel_payload(pkt)
    }
}

/// Payload of a ChannelData packet on [`CHANNEL`].
pub fn channel_payload(pkt: &[u8]) -> Option<&[u8]> {
    if pkt.len() < 4 || u16::from_be_bytes([pkt[0], pkt[1]]) != CHANNEL {
        return None;
    }
    let len = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
    pkt.get(4..4 + len)
}

/// Send `req` until a response with its transaction id arrives (blocking;
/// nobody else may be reading `socket`).
fn transact(socket: &UdpSocket, server: SocketAddr, req: &[u8], until: Instant) -> io::Result<Vec<u8>> {
    let txid = crate::stun::txid(req);
    let mut buf = [0u8; 1500];
    while Instant::now() < until {
        socket.send_to(req, server)?;
        let resend = (Instant::now() + Duration::from_millis(250)).min(until);
        while let Some(left) = resend.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
            socket.set_read_timeout(Some(left))?;
            match socket.recv_from(&mut buf) {
                Ok((n, from))
                    if from == server && crate::stun::is_stun(&buf[..n]) && crate::stun::txid(&buf[..n]) == txid =>
                {
                    return Ok(buf[..n].to_vec());
                }
                Ok(_) => {}
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => break,
                Err(e) => return Err(e),
            }
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "no answer from the relay server"))
}

fn fail(p: &Parsed, what: &str) -> io::Error {
    let (code, reason) = p.error().unwrap_or((0, String::new()));
    io::Error::other(format!("relay {what} refused: {code} {reason}"))
}

/// Allocate a relay address and bind a channel to `peer` (blocking; run on a
/// socket nobody else reads yet).
pub fn allocate(
    socket: &UdpSocket,
    creds: &Credentials,
    peer: SocketAddr,
    timeout: Duration,
) -> io::Result<Allocation> {
    let until = Instant::now() + timeout;
    let old = socket.read_timeout().ok().flatten();
    let result = (|| {
        let transport = (ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0]);
        // 1. Unauthenticated: the server answers 401 with its realm and a nonce.
        let txid: [u8; 12] = crate::rand::bytes();
        let resp =
            transact(socket, creds.server, &encode(ALLOCATE, &txid, std::slice::from_ref(&transport), None), until)?;
        let p = parse(&resp).ok_or_else(|| io::Error::other("bad relay response"))?;
        let (realm, mut nonce) = match (p.text(ATTR_REALM), p.text(ATTR_NONCE)) {
            (Some(r), Some(n)) => (r, n),
            _ => return Err(fail(&p, "allocation")),
        };
        let key = long_term_key(&creds.username, &realm, &creds.password);
        // 2. Authenticated allocation (retry once on a stale nonce).
        for _ in 0..2 {
            let txid: [u8; 12] = crate::rand::bytes();
            let attrs = vec![
                transport.clone(),
                (ATTR_USERNAME, creds.username.as_bytes().to_vec()),
                (ATTR_REALM, realm.as_bytes().to_vec()),
                (ATTR_NONCE, nonce.as_bytes().to_vec()),
            ];
            let resp = transact(socket, creds.server, &encode(ALLOCATE, &txid, &attrs, Some(&key)), until)?;
            let p = parse(&resp).ok_or_else(|| io::Error::other("bad relay response"))?;
            if p.ty == ALLOCATE | SUCCESS {
                let relayed =
                    p.xor_address(ATTR_XOR_RELAYED_ADDRESS).ok_or_else(|| io::Error::other("relay gave no address"))?;
                let mut a = Allocation {
                    server: creds.server,
                    relayed,
                    mapped: p.xor_address(ATTR_XOR_MAPPED_ADDRESS),
                    peer,
                    username: creds.username.clone(),
                    realm: realm.clone(),
                    nonce: nonce.clone(),
                    key,
                    pending: Vec::new(),
                };
                // 3. Bind our channel to the host (this also permits its packets).
                for _ in 0..2 {
                    let req = a.request(CHANNEL_BIND);
                    let resp = transact(socket, a.server, &req, until)?;
                    let p = parse(&resp).ok_or_else(|| io::Error::other("bad relay response"))?;
                    a.pending.clear();
                    if p.ty == CHANNEL_BIND | SUCCESS {
                        return Ok(a);
                    }
                    match (p.error(), p.text(ATTR_NONCE)) {
                        (Some((438, _)), Some(n)) => a.nonce = n,
                        _ => return Err(fail(&p, "channel")),
                    }
                }
                return Err(io::Error::other("relay channel: nonce kept going stale"));
            }
            match (p.error(), p.text(ATTR_NONCE)) {
                (Some((438, _)), Some(n)) => nonce = n,
                _ => return Err(fail(&p, "allocation")),
            }
        }
        Err(io::Error::other("relay allocation: nonce kept going stale"))
    })();
    let _ = socket.set_read_timeout(old);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_and_md5_vectors() {
        // RFC 2202 test case 2.
        assert_eq!(
            crate::wire::hex(&hmac_sha1(b"Jefe", b"what do ya want for nothing?")),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
        // RFC 5769 §2.4: long-term key for user "<U+30DE><U+30C8><U+30EA><U+30C3><U+30AF><U+30B9>" is
        // computed after SASLprep; check the plain-ASCII formula instead.
        let k = long_term_key("user", "realm", "pass");
        let mut h = Md5::new();
        h.update(b"user:realm:pass");
        assert_eq!(k, <[u8; 16]>::from(h.finalize()));
    }

    #[test]
    fn integrity_and_channel_data() {
        let key = long_term_key("u", "r", "p");
        let txid = [9u8; 12];
        let peer: SocketAddr = "203.0.113.7:47800".parse().unwrap();
        let m = encode(CHANNEL_BIND, &txid, &[(ATTR_XOR_PEER_ADDRESS, xor_addr(peer, &txid))], Some(&key));
        let p = parse(&m).unwrap();
        assert_eq!(p.ty, CHANNEL_BIND);
        assert_eq!(p.xor_address(ATTR_XOR_PEER_ADDRESS), Some(peer));
        // MESSAGE-INTEGRITY covers everything before it, with the final length.
        let mi_at = m.len() - 24;
        assert_eq!(&m[mi_at + 4..], &hmac_sha1(&key, &m[..mi_at]));
        let a = Allocation {
            server: "198.51.100.1:3478".parse().unwrap(),
            relayed: "198.51.100.1:50000".parse().unwrap(),
            mapped: None,
            peer,
            username: "u".into(),
            realm: "r".into(),
            nonce: "n".into(),
            key,
            pending: Vec::new(),
        };
        let w = a.wrap(b"hello");
        assert_eq!(&w[..4], &[0x40, 0x00, 0x00, 0x05]);
        assert_eq!(a.unwrap(&w), Some(&b"hello"[..]));
        assert_eq!(a.unwrap(&[0x40, 0x01, 0, 0]), None);
    }
}

#[cfg(test)]
mod relay_tests {
    use super::*;
    use crate::crypto::Keypair;
    use crate::net::{Client, Event, Host, HostOptions, NetConfig};
    use crate::proto::Control;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// A toy TURN server: 401 then success for Allocate, accepts ChannelBind
    /// and Refresh, and relays ChannelData between the client and its peer.
    fn fake_turn() -> SocketAddr {
        let ctl = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let relay = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let ctl_addr = ctl.local_addr().unwrap();
        let relay_addr = relay.local_addr().unwrap();
        let state: Arc<Mutex<(Option<SocketAddr>, Option<SocketAddr>)>> = Arc::default(); // (client, peer)
        {
            let (ctl, relay, state) = (Arc::clone(&ctl), Arc::clone(&relay), Arc::clone(&state));
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                loop {
                    let Ok((n, from)) = ctl.recv_from(&mut buf) else { return };
                    let pkt = &buf[..n];
                    if let Some(p) = parse(pkt) {
                        let reply = match p.ty {
                            ALLOCATE if p.attr(ATTR_USERNAME).is_none() => encode(
                                ALLOCATE | ERROR,
                                &p.txid,
                                &[
                                    (ATTR_ERROR_CODE, vec![0, 0, 4, 1]),
                                    (ATTR_REALM, b"test".to_vec()),
                                    (ATTR_NONCE, b"n1".to_vec()),
                                ],
                                None,
                            ),
                            ALLOCATE => {
                                assert!(p.attr(ATTR_MESSAGE_INTEGRITY).is_some());
                                state.lock().unwrap().0 = Some(from);
                                encode(
                                    ALLOCATE | SUCCESS,
                                    &p.txid,
                                    &[
                                        (ATTR_XOR_RELAYED_ADDRESS, xor_addr(relay_addr, &p.txid)),
                                        (ATTR_XOR_MAPPED_ADDRESS, xor_addr(from, &p.txid)),
                                    ],
                                    None,
                                )
                            }
                            CHANNEL_BIND => {
                                state.lock().unwrap().1 = p.xor_address(ATTR_XOR_PEER_ADDRESS);
                                encode(CHANNEL_BIND | SUCCESS, &p.txid, &[], None)
                            }
                            t => encode(t | SUCCESS, &p.txid, &[], None),
                        };
                        let _ = ctl.send_to(&reply, from);
                    } else if let Some(data) = channel_payload(pkt)
                        && let Some(peer) = state.lock().unwrap().1
                    {
                        let _ = relay.send_to(data, peer);
                    }
                }
            });
        }
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                let Ok((n, from)) = relay.recv_from(&mut buf) else { return };
                let (client, peer) = *state.lock().unwrap();
                if let Some(c) = client
                    && Some(from) == peer
                {
                    let mut v = CHANNEL.to_be_bytes().to_vec();
                    v.extend_from_slice(&(n as u16).to_be_bytes());
                    v.extend_from_slice(&buf[..n]);
                    let _ = ctl.send_to(&v, c);
                }
            }
        });
        ctl_addr
    }

    #[test]
    fn connect_through_relay() {
        let server = fake_turn();
        let host_id = Keypair::generate();
        let client_id = Keypair::generate();
        let mut trusted = HashMap::new();
        trusted.insert(client_id.public(), "viewer".to_string());
        let host = Host::bind(
            "127.0.0.1:0".parse().unwrap(),
            host_id.clone(),
            NetConfig { name: "host".into(), ..Default::default() },
            HostOptions { trusted, pairing_code: None, on_paired: None },
        )
        .unwrap();
        let host_addr = host.local_addr().unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let creds = Credentials { server, username: "u".into(), password: "p".into() };
        let alloc = allocate(&socket, &creds, host_addr, Duration::from_secs(3)).unwrap();
        assert_ne!(alloc.relayed, server);
        let client = Client::connect_relay(
            socket,
            alloc,
            host_id.public(),
            client_id,
            NetConfig { name: "viewer".into(), ..Default::default() },
            Duration::from_secs(3),
        )
        .unwrap();
        let server_conn = host.accept(Duration::from_secs(3)).unwrap();
        // The host sees the relay address; the viewer sees the host.
        assert_ne!(server_conn.peer_addr(), host_addr);
        assert_eq!(client.peer_addr(), host_addr);
        // Relayed sessions use the smaller WAN datagram both ways.
        assert!(client.max_payload() <= crate::net::WAN_MAX_DATAGRAM);
        client.send_control(&Control::Status { text: "via relay".into() });
        server_conn.send_control(&Control::Status { text: "back".into() });
        let ev = server_conn.take_events().unwrap();
        assert!(matches!(ev.recv_timeout(Duration::from_secs(3)).unwrap(), Event::Control(Control::Status { .. })));
        let cev = client.take_events().unwrap();
        assert!(matches!(cev.recv_timeout(Duration::from_secs(3)).unwrap(), Event::Control(Control::Status { .. })));
    }
}
