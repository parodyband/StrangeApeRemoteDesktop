//! Cryptography: Noise handshakes + per-packet AEAD.
//!
//! * Connection: `Noise_IK_25519_ChaChaPoly_SHA256`. The client already knows
//!   the host's static key (learned during pairing); the host checks the
//!   client's static key against its trust store. One round trip.
//! * Pairing: `Noise_NNpsk0_25519_ChaChaPoly_SHA256`, PSK derived from a
//!   100-bit one-time pairing code shown by the host. Both sides exchange
//!   static public keys inside the encrypted payloads and persist them.
//! * Transport: ChaCha20-Poly1305 with an explicit 64-bit packet counter as
//!   nonce (carried in the clear packet header, authenticated as AAD) and a
//!   sliding replay window, WireGuard style, so UDP loss/reordering is fine.
//!
//! Primitives come from audited RustCrypto / dalek crates; only the Noise
//! state machine (a few dozen lines, per the spec) is implemented here.

use chacha20poly1305::{AeadInOut, ChaCha20Poly1305, Key, KeyInit, Nonce, Tag};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

pub const KEY_LEN: usize = 32;
pub const TAG_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    /// Authentication failed (wrong key, tampering, or wrong pairing code).
    Decrypt,
    /// Message too short / malformed.
    BadLength,
    /// Low-order public key (DH output all zero).
    WeakKey,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CryptoError::Decrypt => "authentication failed",
            CryptoError::BadLength => "malformed handshake",
            CryptoError::WeakKey => "invalid public key",
        })
    }
}

impl std::error::Error for CryptoError {}

type Result<T> = std::result::Result<T, CryptoError>;

/// X25519 static or ephemeral keypair.
#[derive(Clone)]
pub struct Keypair {
    secret: StaticSecret,
    public: [u8; 32],
}

impl Keypair {
    pub fn generate() -> Self {
        Self::from_secret(crate::rand::bytes())
    }

    pub fn from_secret(bytes: [u8; 32]) -> Self {
        let secret = StaticSecret::from(bytes);
        let public = PublicKey::from(&secret).to_bytes();
        Self { secret, public }
    }

    pub fn public(&self) -> [u8; 32] {
        self.public
    }

    pub fn secret_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }

    fn dh(&self, their_public: &[u8; 32]) -> Result<[u8; 32]> {
        let shared = self.secret.diffie_hellman(&PublicKey::from(*their_public));
        if !shared.was_contributory() {
            return Err(CryptoError::WeakKey);
        }
        Ok(shared.to_bytes())
    }
}

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    let out = h.finalize();
    let mut r = [0u8; 32];
    r.copy_from_slice(&out);
    r
}

pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(&[key]));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    for p in parts {
        inner.update(p);
    }
    let ih = inner.finalize();
    sha256(&[&opad, &ih])
}

/// Noise HKDF producing three outputs.
fn hkdf(ck: &[u8; 32], ikm: &[u8]) -> ([u8; 32], [u8; 32], [u8; 32]) {
    let temp = hmac_sha256(ck, &[ikm]);
    let o1 = hmac_sha256(&temp, &[&[1]]);
    let o2 = hmac_sha256(&temp, &[&o1, &[2]]);
    let o3 = hmac_sha256(&temp, &[&o2, &[3]]);
    (o1, o2, o3)
}

#[inline]
fn nonce(n: u64) -> Nonce {
    let mut b = [0u8; 12];
    b[4..].copy_from_slice(&n.to_le_bytes());
    Nonce::from(b)
}

struct CipherState {
    k: Option<ChaCha20Poly1305>,
    n: u64,
}

impl CipherState {
    fn empty() -> Self {
        Self { k: None, n: 0 }
    }

    fn with_key(k: [u8; 32]) -> Self {
        Self { k: Some(ChaCha20Poly1305::new(&Key::from(k))), n: 0 }
    }

    fn encrypt_with_ad(&mut self, ad: &[u8], pt: &[u8]) -> Vec<u8> {
        let mut buf = pt.to_vec();
        if let Some(c) = &self.k {
            let tag = c
                .encrypt_inout_detached(&nonce(self.n), ad, buf.as_mut_slice().into())
                .expect("chacha20poly1305 encrypt cannot fail for small buffers");
            buf.extend_from_slice(&tag);
            self.n += 1;
        }
        buf
    }

    fn decrypt_with_ad(&mut self, ad: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
        let Some(c) = &self.k else {
            return Ok(ct.to_vec());
        };
        if ct.len() < TAG_LEN {
            return Err(CryptoError::BadLength);
        }
        let (body, tag) = ct.split_at(ct.len() - TAG_LEN);
        let mut buf = body.to_vec();
        let tag = Tag::try_from(tag).map_err(|_| CryptoError::BadLength)?;
        c.decrypt_inout_detached(&nonce(self.n), ad, buf.as_mut_slice().into(), &tag)
            .map_err(|_| CryptoError::Decrypt)?;
        self.n += 1;
        Ok(buf)
    }
}

struct Symmetric {
    ck: [u8; 32],
    h: [u8; 32],
    cs: CipherState,
}

impl Symmetric {
    fn new(protocol: &str) -> Self {
        let p = protocol.as_bytes();
        let h = if p.len() <= 32 {
            let mut h = [0u8; 32];
            h[..p.len()].copy_from_slice(p);
            h
        } else {
            sha256(&[p])
        };
        Self { ck: h, h, cs: CipherState::empty() }
    }

    fn mix_hash(&mut self, data: &[u8]) {
        self.h = sha256(&[&self.h, data]);
    }

    fn mix_key(&mut self, ikm: &[u8]) {
        let (ck, k, _) = hkdf(&self.ck, ikm);
        self.ck = ck;
        self.cs = CipherState::with_key(k);
    }

    fn mix_key_and_hash(&mut self, ikm: &[u8]) {
        let (ck, th, k) = hkdf(&self.ck, ikm);
        self.ck = ck;
        self.mix_hash(&th);
        self.cs = CipherState::with_key(k);
    }

    fn encrypt_and_hash(&mut self, pt: &[u8]) -> Vec<u8> {
        let h = self.h;
        let ct = self.cs.encrypt_with_ad(&h, pt);
        self.mix_hash(&ct);
        ct
    }

    fn decrypt_and_hash(&mut self, ct: &[u8]) -> Result<Vec<u8>> {
        let h = self.h;
        let pt = self.cs.decrypt_with_ad(&h, ct)?;
        self.mix_hash(ct);
        Ok(pt)
    }

    fn split(&self) -> ([u8; 32], [u8; 32]) {
        let (a, b, _) = hkdf(&self.ck, &[]);
        (a, b)
    }
}

/// Result of a completed handshake.
pub struct HandshakeOutput {
    pub send: TransportCipher,
    pub recv: TransportCipher,
    /// Remote static key (for IK; for pairing it comes from the payload).
    pub remote_static: [u8; 32],
    /// Channel binding hash.
    pub handshake_hash: [u8; 32],
}

const IK_NAME: &str = "Noise_IK_25519_ChaChaPoly_SHA256";
const NNPSK0_NAME: &str = "Noise_NNpsk0_25519_ChaChaPoly_SHA256";

fn take32(b: &[u8]) -> [u8; 32] {
    b[..32].try_into().unwrap()
}

/// IK initiator (client side of a connection).
pub struct IkInitiator {
    sym: Symmetric,
    s: Keypair,
    e: Keypair,
    rs: [u8; 32],
}

impl IkInitiator {
    pub fn new(local: &Keypair, remote_static: [u8; 32], prologue: &[u8]) -> Self {
        let mut sym = Symmetric::new(IK_NAME);
        sym.mix_hash(prologue);
        sym.mix_hash(&remote_static);
        Self { sym, s: local.clone(), e: Keypair::generate(), rs: remote_static }
    }

    /// `-> e, es, s, ss` + payload.
    pub fn write_message1(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(32 + 48 + payload.len() + 16);
        out.extend_from_slice(&self.e.public);
        self.sym.mix_hash(&self.e.public.clone());
        let es = self.e.dh(&self.rs)?;
        self.sym.mix_key(&es);
        let s_pub = self.s.public;
        out.extend(self.sym.encrypt_and_hash(&s_pub));
        let ss = self.s.dh(&self.rs)?;
        self.sym.mix_key(&ss);
        out.extend(self.sym.encrypt_and_hash(payload));
        Ok(out)
    }

    /// `<- e, ee, se` + payload.
    pub fn read_message2(mut self, msg: &[u8]) -> Result<(HandshakeOutput, Vec<u8>)> {
        if msg.len() < 32 + TAG_LEN {
            return Err(CryptoError::BadLength);
        }
        let re = take32(msg);
        self.sym.mix_hash(&re);
        let ee = self.e.dh(&re)?;
        self.sym.mix_key(&ee);
        let se = self.s.dh(&re)?;
        self.sym.mix_key(&se);
        let payload = self.sym.decrypt_and_hash(&msg[32..])?;
        let (k1, k2) = self.sym.split();
        Ok((
            HandshakeOutput {
                send: TransportCipher::new(k1),
                recv: TransportCipher::new(k2),
                remote_static: self.rs,
                handshake_hash: self.sym.h,
            },
            payload,
        ))
    }
}

/// IK responder (host side), after reading message 1.
pub struct IkResponder {
    sym: Symmetric,
    re: [u8; 32],
    rs: [u8; 32],
}

impl IkResponder {
    /// Process message 1. Returns the responder state, the initiator's static
    /// key (caller must check it against the trust store) and the payload.
    pub fn read_message1(local: &Keypair, prologue: &[u8], msg: &[u8]) -> Result<(Self, [u8; 32], Vec<u8>)> {
        if msg.len() < 32 + 48 + TAG_LEN {
            return Err(CryptoError::BadLength);
        }
        let mut sym = Symmetric::new(IK_NAME);
        sym.mix_hash(prologue);
        sym.mix_hash(&local.public);
        let re = take32(msg);
        sym.mix_hash(&re);
        let es = local.dh(&re)?;
        sym.mix_key(&es);
        let rs_vec = sym.decrypt_and_hash(&msg[32..80])?;
        let rs = take32(&rs_vec);
        let ss = local.dh(&rs)?;
        sym.mix_key(&ss);
        let payload = sym.decrypt_and_hash(&msg[80..])?;
        Ok((Self { sym, re, rs }, rs, payload))
    }

    pub fn write_message2(mut self, payload: &[u8]) -> Result<(Vec<u8>, HandshakeOutput)> {
        let e = Keypair::generate();
        let mut out = Vec::with_capacity(32 + payload.len() + 16);
        out.extend_from_slice(&e.public);
        self.sym.mix_hash(&e.public);
        let ee = e.dh(&self.re)?;
        self.sym.mix_key(&ee);
        let se = e.dh(&self.rs)?;
        self.sym.mix_key(&se);
        out.extend(self.sym.encrypt_and_hash(payload));
        let (k1, k2) = self.sym.split();
        Ok((
            out,
            HandshakeOutput {
                send: TransportCipher::new(k2),
                recv: TransportCipher::new(k1),
                remote_static: self.rs,
                handshake_hash: self.sym.h,
            },
        ))
    }
}

/// NNpsk0 initiator (client side of pairing).
pub struct PairInitiator {
    sym: Symmetric,
    e: Keypair,
}

impl PairInitiator {
    pub fn new(psk: &[u8; 32], prologue: &[u8]) -> Self {
        let mut sym = Symmetric::new(NNPSK0_NAME);
        sym.mix_hash(prologue);
        sym.mix_key_and_hash(psk);
        Self { sym, e: Keypair::generate() }
    }

    pub fn write_message1(&mut self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + payload.len() + 16);
        let e_pub = self.e.public;
        out.extend_from_slice(&e_pub);
        self.sym.mix_hash(&e_pub);
        self.sym.mix_key(&e_pub);
        out.extend(self.sym.encrypt_and_hash(payload));
        out
    }

    pub fn read_message2(mut self, msg: &[u8]) -> Result<Vec<u8>> {
        if msg.len() < 32 + TAG_LEN {
            return Err(CryptoError::BadLength);
        }
        let re = take32(msg);
        self.sym.mix_hash(&re);
        self.sym.mix_key(&re);
        let ee = self.e.dh(&re)?;
        self.sym.mix_key(&ee);
        self.sym.decrypt_and_hash(&msg[32..])
    }
}

/// NNpsk0 responder (host side of pairing).
pub struct PairResponder {
    sym: Symmetric,
    re: [u8; 32],
}

impl PairResponder {
    pub fn read_message1(psk: &[u8; 32], prologue: &[u8], msg: &[u8]) -> Result<(Self, Vec<u8>)> {
        if msg.len() < 32 + TAG_LEN {
            return Err(CryptoError::BadLength);
        }
        let mut sym = Symmetric::new(NNPSK0_NAME);
        sym.mix_hash(prologue);
        sym.mix_key_and_hash(psk);
        let re = take32(msg);
        sym.mix_hash(&re);
        sym.mix_key(&re);
        let payload = sym.decrypt_and_hash(&msg[32..])?;
        Ok((Self { sym, re }, payload))
    }

    pub fn write_message2(mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let e = Keypair::generate();
        let mut out = Vec::with_capacity(32 + payload.len() + 16);
        out.extend_from_slice(&e.public);
        self.sym.mix_hash(&e.public);
        self.sym.mix_key(&e.public);
        let ee = e.dh(&self.re)?;
        self.sym.mix_key(&ee);
        out.extend(self.sym.encrypt_and_hash(payload));
        Ok(out)
    }
}

/// Per-direction transport AEAD with explicit counters.
pub struct TransportCipher(ChaCha20Poly1305);

impl TransportCipher {
    pub fn new(key: [u8; 32]) -> Self {
        Self(ChaCha20Poly1305::new(&Key::from(key)))
    }

    /// Encrypt `buf` in place; returns the tag to append.
    #[inline]
    pub fn seal(&self, counter: u64, ad: &[u8], buf: &mut [u8]) -> [u8; TAG_LEN] {
        let tag = self.0.encrypt_inout_detached(&nonce(counter), ad, buf.into()).expect("encrypt cannot fail");
        let mut t = [0u8; TAG_LEN];
        t.copy_from_slice(&tag);
        t
    }

    /// Decrypt `buf` in place, verifying `tag`.
    #[inline]
    pub fn open(&self, counter: u64, ad: &[u8], buf: &mut [u8], tag: &[u8]) -> Result<()> {
        let tag = Tag::try_from(tag).map_err(|_| CryptoError::BadLength)?;
        self.0.decrypt_inout_detached(&nonce(counter), ad, buf.into(), &tag).map_err(|_| CryptoError::Decrypt)
    }
}

/// Sliding anti-replay window (4096 packets).
pub struct ReplayWindow {
    max: u64,
    init: bool,
    bits: [u64; Self::WORDS],
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self { max: 0, init: false, bits: [0; Self::WORDS] }
    }
}

impl ReplayWindow {
    const WORDS: usize = 64;
    const SIZE: u64 = (Self::WORDS * 64) as u64;

    #[inline]
    fn bit(c: u64) -> (usize, u64) {
        let i = (c % Self::SIZE) as usize;
        (i / 64, 1u64 << (i % 64))
    }

    /// Would `counter` be accepted? (Does not modify state.)
    pub fn check(&self, counter: u64) -> bool {
        if !self.init || counter > self.max {
            return true;
        }
        if self.max - counter >= Self::SIZE {
            return false;
        }
        let (w, m) = Self::bit(counter);
        self.bits[w] & m == 0
    }

    /// Record `counter` as seen. Call only after successful authentication.
    pub fn update(&mut self, counter: u64) {
        if !self.init {
            self.init = true;
            self.max = counter;
            self.bits = [0; Self::WORDS];
        } else if counter > self.max {
            let diff = counter - self.max;
            if diff >= Self::SIZE {
                self.bits = [0; Self::WORDS];
            } else {
                for c in self.max + 1..counter {
                    let (w, m) = Self::bit(c);
                    self.bits[w] &= !m;
                }
            }
            self.max = counter;
        }
        let (w, m) = Self::bit(counter);
        self.bits[w] |= m;
    }
}

// ---------------------------------------------------------------------------
// Pairing codes
// ---------------------------------------------------------------------------

const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Generate a 20-symbol (100-bit) pairing code, formatted `XXXXX-XXXXX-XXXXX-XXXXX`.
pub fn generate_pairing_code() -> String {
    let raw: [u8; 20] = crate::rand::bytes();
    let mut s = String::with_capacity(23);
    for (i, b) in raw.iter().enumerate() {
        if i > 0 && i % 5 == 0 {
            s.push('-');
        }
        s.push(CODE_ALPHABET[(b & 31) as usize] as char);
    }
    s
}

/// Normalize user input (case, dashes, spaces). `None` if invalid.
pub fn normalize_pairing_code(input: &str) -> Option<String> {
    let s: String = input.chars().filter(|c| !c.is_whitespace() && *c != '-').map(|c| c.to_ascii_uppercase()).collect();
    if s.len() != 20 || !s.bytes().all(|b| CODE_ALPHABET.contains(&b)) {
        return None;
    }
    Some(s)
}

/// Derive the 32-byte PSK from a (normalized) pairing code.
pub fn pairing_psk(normalized_code: &str) -> [u8; 32] {
    sha256(&[b"sard-pair-v1", normalized_code.as_bytes()])
}

/// Short human-readable fingerprint of a public key, e.g. `3f2a-91bc-0d44-7e10`.
pub fn fingerprint(public: &[u8; 32]) -> String {
    let h = sha256(&[b"sard-fp", public]);
    let x = crate::wire::hex(&h[..8]);
    format!("{}-{}-{}-{}", &x[0..4], &x[4..8], &x[8..12], &x[12..16])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{hex, unhex};

    #[test]
    fn hmac_rfc4231_case2() {
        let mac = hmac_sha256(b"Jefe", &[b"what do ya want ", b"for nothing?"]);
        assert_eq!(hex(&mac), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    #[test]
    fn hmac_rfc4231_case6_long_key() {
        let key = [0xaau8; 131];
        let mac = hmac_sha256(&key, &[b"Test Using Larger Than Block-Size Key - Hash Key First"]);
        assert_eq!(hex(&mac), "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
    }

    #[test]
    fn x25519_rfc7748_vector() {
        // RFC 7748 section 6.1
        let a = Keypair::from_secret(
            unhex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a").unwrap().try_into().unwrap(),
        );
        assert_eq!(hex(&a.public()), "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
        let b_pub: [u8; 32] =
            unhex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f").unwrap().try_into().unwrap();
        assert_eq!(hex(&a.dh(&b_pub).unwrap()), "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
    }

    #[test]
    fn ik_handshake_and_transport() {
        let host = Keypair::generate();
        let client = Keypair::generate();
        let mut ini = IkInitiator::new(&client, host.public(), b"sard-test");
        let m1 = ini.write_message1(b"hello host").unwrap();
        let (resp, rs, p1) = IkResponder::read_message1(&host, b"sard-test", &m1).unwrap();
        assert_eq!(rs, client.public());
        assert_eq!(p1, b"hello host");
        let (m2, host_out) = resp.write_message2(b"hello client").unwrap();
        let (client_out, p2) = ini.read_message2(&m2).unwrap();
        assert_eq!(p2, b"hello client");
        assert_eq!(client_out.handshake_hash, host_out.handshake_hash);

        let mut data = *b"frame data 123";
        let tag = client_out.send.seal(7, b"hdr", &mut data);
        assert_ne!(&data, b"frame data 123");
        host_out.recv.open(7, b"hdr", &mut data, &tag).unwrap();
        assert_eq!(&data, b"frame data 123");

        let mut d2 = *b"reply";
        let tag2 = host_out.send.seal(0, b"", &mut d2);
        let mut bad = d2;
        assert!(client_out.recv.open(1, b"", &mut bad, &tag2).is_err(), "wrong nonce must fail");
        client_out.recv.open(0, b"", &mut d2, &tag2).unwrap();
        assert_eq!(&d2, b"reply");
    }

    #[test]
    fn ik_rejects_wrong_host_key() {
        let host = Keypair::generate();
        let impostor = Keypair::generate();
        let client = Keypair::generate();
        let mut ini = IkInitiator::new(&client, host.public(), b"p");
        let m1 = ini.write_message1(b"x").unwrap();
        assert!(IkResponder::read_message1(&impostor, b"p", &m1).is_err());
    }

    #[test]
    fn ik_rejects_tampering() {
        let host = Keypair::generate();
        let client = Keypair::generate();
        let mut ini = IkInitiator::new(&client, host.public(), b"p");
        let mut m1 = ini.write_message1(b"x").unwrap();
        let n = m1.len();
        m1[n - 1] ^= 1;
        assert!(IkResponder::read_message1(&host, b"p", &m1).is_err());
    }

    #[test]
    fn pairing_roundtrip_and_wrong_code() {
        let code = normalize_pairing_code(&generate_pairing_code()).unwrap();
        let psk = pairing_psk(&code);
        let mut ini = PairInitiator::new(&psk, b"pair");
        let m1 = ini.write_message1(b"client-static");
        let (resp, p1) = PairResponder::read_message1(&psk, b"pair", &m1).unwrap();
        assert_eq!(p1, b"client-static");
        let m2 = resp.write_message2(b"host-static").unwrap();
        assert_eq!(ini.read_message2(&m2).unwrap(), b"host-static");

        let wrong = pairing_psk("AAAAAAAAAAAAAAAAAAAA");
        let mut ini2 = PairInitiator::new(&psk, b"pair");
        let m1 = ini2.write_message1(b"x");
        assert!(PairResponder::read_message1(&wrong, b"pair", &m1).is_err());
    }

    #[test]
    fn pairing_code_format() {
        let c = generate_pairing_code();
        assert_eq!(c.len(), 23);
        let n = normalize_pairing_code(&c.to_lowercase().replace('-', " ")).unwrap();
        assert_eq!(n, c.replace('-', ""));
        assert!(normalize_pairing_code("ABC").is_none());
        assert!(normalize_pairing_code("IIIIIOOOOOIIIIIOOOOO").is_none());
    }

    #[test]
    fn replay_window() {
        let mut w = ReplayWindow::default();
        for c in [5u64, 3, 9, 4] {
            assert!(w.check(c));
            w.update(c);
            assert!(!w.check(c));
        }
        assert!(w.check(6));
        w.update(10_000);
        assert!(!w.check(5), "too old");
        assert!(w.check(9_999));
        assert!(!w.check(10_000));
        w.update(9_999);
        assert!(!w.check(9_999));
        // Bits for skipped counters must be cleared when the window advances.
        w.update(10_000 + 4096 + 1);
        assert!(w.check(10_000 + 4096));
    }
}
