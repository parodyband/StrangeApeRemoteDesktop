//! Persistent identity and trust state.
//!
//! Files (all plain text, in the config directory):
//! * `identity.key`  – this machine's X25519 secret key (hex), mode 0600.
//! * `trusted.txt`   – (host role) clients allowed to connect: `<pubkey hex> <name>`.
//! * `known_hosts.txt` – (client role) paired hosts: `<pubkey hex> <addr> <name>`.
//!
//! Config directory: `$SARD_CONFIG_DIR`, else `$XDG_CONFIG_HOME/sard` /
//! `~/.config/sard` on Linux and `~/Library/Application Support/sard` on macOS.

use crate::crypto::Keypair;
use crate::wire::{hex, unhex};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("SARD_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/sard")
    } else if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(x).join("sard")
    } else {
        home.join(".config/sard")
    }
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buffer is valid for buf.len() bytes.
    let r = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if r != 0 {
        return "unknown".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let s = String::from_utf8_lossy(&buf[..end]).to_string();
    s.trim_end_matches(".local").to_string()
}

fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

/// Load this machine's identity key, creating one on first run.
pub fn load_or_create_identity(dir: &Path) -> io::Result<Keypair> {
    let path = dir.join("identity.key");
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let bytes = unhex(&s)
                .filter(|b| b.len() == 32)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{} is corrupt", path.display())))?;
            Ok(Keypair::from_secret(bytes.try_into().unwrap()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let kp = Keypair::generate();
            write_private(&path, &(hex(&kp.secret_bytes()) + "\n"))?;
            Ok(kp)
        }
        Err(e) => Err(e),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub key: [u8; 32],
    pub name: String,
}

/// Host-side list of clients allowed to connect.
pub struct TrustStore {
    path: PathBuf,
    pub peers: Vec<Peer>,
}

impl TrustStore {
    pub fn load(dir: &Path) -> io::Result<Self> {
        let path = dir.join("trusted.txt");
        let mut peers = Vec::new();
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                for line in s.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                    let (k, name) = line.split_once(' ').unwrap_or((line, ""));
                    if let Some(key) = unhex(k).and_then(|b| <[u8; 32]>::try_from(b).ok()) {
                        peers.push(Peer { key, name: name.trim().to_string() });
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(Self { path, peers })
    }

    pub fn find(&self, key: &[u8; 32]) -> Option<&Peer> {
        self.peers.iter().find(|p| &p.key == key)
    }

    pub fn add(&mut self, key: [u8; 32], name: &str) -> io::Result<()> {
        self.peers.retain(|p| p.key != key);
        self.peers.push(Peer { key, name: sanitize(name) });
        self.save()
    }

    pub fn remove_by_name(&mut self, name: &str) -> io::Result<bool> {
        let before = self.peers.len();
        self.peers.retain(|p| p.name != name);
        let changed = self.peers.len() != before;
        if changed {
            self.save()?;
        }
        Ok(changed)
    }

    fn save(&self) -> io::Result<()> {
        let mut s = String::from("# sard trusted clients: <public key> <name>\n");
        for p in &self.peers {
            s += &format!("{} {}\n", hex(&p.key), p.name);
        }
        write_private(&self.path, &s)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct KnownHost {
    pub key: [u8; 32],
    pub addr: String,
    pub name: String,
}

/// Client-side list of paired hosts.
pub struct KnownHosts {
    path: PathBuf,
    pub hosts: Vec<KnownHost>,
}

impl KnownHosts {
    pub fn load(dir: &Path) -> io::Result<Self> {
        let path = dir.join("known_hosts.txt");
        let mut hosts = Vec::new();
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                for line in s.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                    let mut it = line.splitn(3, ' ');
                    let (Some(k), Some(addr)) = (it.next(), it.next()) else {
                        continue;
                    };
                    let name = it.next().unwrap_or("").trim().to_string();
                    if let Some(key) = unhex(k).and_then(|b| <[u8; 32]>::try_from(b).ok()) {
                        hosts.push(KnownHost { key, addr: addr.to_string(), name });
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(Self { path, hosts })
    }

    /// Look up by address (`host` or `host:port`) or by name.
    pub fn find(&self, query: &str) -> Option<&KnownHost> {
        let q = query.trim();
        self.hosts
            .iter()
            .find(|h| h.addr == q)
            .or_else(|| self.hosts.iter().find(|h| strip_port(&h.addr) == strip_port(q)))
            .or_else(|| self.hosts.iter().find(|h| h.name.eq_ignore_ascii_case(q)))
    }

    pub fn add(&mut self, key: [u8; 32], addr: &str, name: &str) -> io::Result<()> {
        self.hosts.retain(|h| h.key != key && h.addr != addr);
        self.hosts.push(KnownHost { key, addr: addr.to_string(), name: sanitize(name) });
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        let mut s = String::from("# sard known hosts: <public key> <address> <name>\n");
        for h in &self.hosts {
            s += &format!("{} {} {}\n", hex(&h.key), h.addr, h.name);
        }
        write_private(&self.path, &s)
    }
}

fn strip_port(a: &str) -> &str {
    if a.starts_with('[') {
        return a.split(']').next().unwrap_or(a).trim_start_matches('[');
    }
    match a.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && p.parse::<u16>().is_ok() => h,
        _ => a,
    }
}

fn sanitize(name: &str) -> String {
    let s: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
    let s = s.trim().replace(' ', "_");
    if s.is_empty() { "unnamed".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sard-test-{tag}-{}", crate::rand::u32()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn identity_persists() {
        let d = tmpdir("id");
        let a = load_or_create_identity(&d).unwrap();
        let b = load_or_create_identity(&d).unwrap();
        assert_eq!(a.public(), b.public());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn trust_and_known_hosts() {
        let d = tmpdir("trust");
        let mut t = TrustStore::load(&d).unwrap();
        t.add([1; 32], "mac book").unwrap();
        let t2 = TrustStore::load(&d).unwrap();
        assert_eq!(t2.find(&[1; 32]).unwrap().name, "mac_book");

        let mut k = KnownHosts::load(&d).unwrap();
        k.add([2; 32], "192.168.1.77:47800", "ape-linux").unwrap();
        let k2 = KnownHosts::load(&d).unwrap();
        assert_eq!(k2.find("192.168.1.77").unwrap().key, [2; 32]);
        assert_eq!(k2.find("ape-linux").unwrap().key, [2; 32]);
        assert!(k2.find("10.0.0.1").is_none());
        std::fs::remove_dir_all(d).unwrap();
    }
}
