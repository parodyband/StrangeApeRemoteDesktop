//! Connect Anywhere: reach a paired host across the internet without port
//! forwarding or a VPN.
//!
//! A small rendezvous service (a Cloudflare Worker, see the sard-anywhere
//! repo) introduces the two sides; video still flows directly between them:
//!
//! 1. The host keeps a WebSocket open to the service and publishes its
//!    addresses: LAN ones plus its public address, learned from a STUN server
//!    on the very socket it accepts connections on.
//! 2. A viewer learns its own public address the same way, then asks the
//!    service to introduce it. The service tells the host "this viewer is at
//!    a.b.c.d:p" and returns the host's addresses.
//! 3. The host sends a few packets toward the viewer (opening its NAT) while
//!    the viewer sends its handshake to every host address; the first one
//!    that answers is used.
//!
//! The service only ever sees public keys and addresses; the connection is
//! authenticated and encrypted end to end by the usual Noise handshake.

use crate::web::{self, Url, WebSocket};
use crate::{info, warn};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How the service is reached: `anywhere.txt` in the config directory,
/// lines `url <https://...>` and `token <account token>`.
#[derive(Clone, Debug)]
pub struct Service {
    pub url: Url,
    pub token: String,
}

impl Service {
    pub fn load(dir: &Path) -> Option<Service> {
        let text = std::fs::read_to_string(dir.join("anywhere.txt")).ok()?;
        let (mut url, mut token) = (None, None);
        for line in text.lines().map(str::trim).filter(|l| !l.starts_with('#')) {
            match line.split_once(char::is_whitespace) {
                Some(("url", v)) => url = Url::parse(v.trim()).ok(),
                Some(("token", v)) => token = Some(v.trim().to_string()),
                _ => {}
            }
        }
        let token = token.filter(|t| !t.is_empty())?;
        Some(Service { url: url?, token })
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// Ask the service to introduce `viewer` (at `viewer_addrs`) to `host`.
    /// Returns the host's addresses, or `Ok(None)` if the host isn't online.
    pub fn introduce(
        &self,
        host: &[u8; 32],
        viewer: &[u8; 32],
        viewer_addrs: &[SocketAddr],
    ) -> std::io::Result<Option<Vec<SocketAddr>>> {
        let body = format!(
            "host {}\nviewer {}\naddrs {}\n",
            crate::wire::hex(host),
            crate::wire::hex(viewer),
            join(viewer_addrs)
        );
        let auth = self.auth();
        let (status, reply) = web::request(
            "POST",
            &self.url.join("/v1/connect"),
            &[("Authorization", &auth), ("Content-Type", "text/plain")],
            body.as_bytes(),
            Duration::from_secs(8),
        )?;
        match status {
            200 => {}
            404 => return Ok(None),
            401 => return Err(std::io::Error::other("Connect Anywhere: this device's account token was refused")),
            s => return Err(std::io::Error::other(format!("Connect Anywhere: service error {s}"))),
        }
        let text = String::from_utf8_lossy(&reply);
        Ok(Some(parse_addrs(text.lines().find_map(|l| l.strip_prefix("addrs")).unwrap_or(""))))
    }
}

fn join(addrs: &[SocketAddr]) -> String {
    addrs.iter().map(SocketAddr::to_string).collect::<Vec<_>>().join(" ")
}

fn parse_addrs(s: &str) -> Vec<SocketAddr> {
    s.split_whitespace().filter_map(|a| a.parse().ok()).take(8).collect()
}

/// What the host agent is doing, for the menu.
#[derive(Clone, Debug, Default)]
pub struct AgentStatus {
    pub online: bool,
    pub public: Option<SocketAddr>,
    pub error: Option<String>,
}

/// Host side: stays registered with the service and punches toward viewers
/// it is introduced to. Stops when dropped.
pub struct HostAgent {
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<AgentStatus>>,
    thread: Option<JoinHandle<()>>,
}

/// Callbacks into the host: current addresses (does a STUN query; also keeps
/// the NAT mapping alive) and "punch toward these addresses".
pub struct HostHooks {
    pub addrs: Box<dyn Fn() -> (Option<SocketAddr>, Vec<SocketAddr>) + Send>,
    pub punch: Box<dyn Fn(Vec<SocketAddr>) + Send>,
    /// The agent's status changed (online / offline).
    pub changed: Box<dyn Fn() + Send>,
}

/// Refresh addresses (and the NAT mapping) this often; also the WebSocket keepalive.
const REFRESH: Duration = Duration::from_secs(25);

impl HostAgent {
    pub fn start(service: Service, host_key: [u8; 32], hooks: HostHooks) -> HostAgent {
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(AgentStatus::default()));
        let (s, st) = (Arc::clone(&stop), Arc::clone(&status));
        let thread = std::thread::Builder::new()
            .name("sard-anywhere".into())
            .spawn(move || run_agent(&service, &host_key, &hooks, &s, &st))
            .ok();
        HostAgent { stop, status, thread }
    }

    pub fn status(&self) -> AgentStatus {
        self.status.lock().unwrap().clone()
    }
}

impl Drop for HostAgent {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run_agent(service: &Service, key: &[u8; 32], hooks: &HostHooks, stop: &AtomicBool, status: &Mutex<AgentStatus>) {
    let url = service.url.join(&format!("/v1/host?key={}", crate::wire::hex(key)));
    let auth = service.auth();
    let mut backoff = Duration::from_secs(1);
    let mut last_error = String::new();
    while !stop.load(Ordering::Relaxed) {
        let err = match WebSocket::connect(&url, &[("Authorization", &auth)], Duration::from_secs(10)) {
            Ok(ws) => {
                backoff = Duration::from_secs(1);
                last_error.clear();
                info!("Connect Anywhere: registered with {}", service.url.host);
                let e = serve(ws, hooks, stop, status);
                status.lock().unwrap().online = false;
                e
            }
            Err(e) => e,
        };
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let msg = err.to_string();
        if msg != last_error {
            warn!("Connect Anywhere: {msg}; retrying");
            last_error = msg.clone();
        }
        let notify = {
            let mut st = status.lock().unwrap();
            let changed = st.error.as_deref() != Some(msg.as_str());
            st.online = false;
            st.error = Some(msg);
            changed
        };
        if notify {
            (hooks.changed)();
        }
        // Sleep in small steps so stopping is quick.
        let until = Instant::now() + backoff;
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

fn serve(mut ws: WebSocket, hooks: &HostHooks, stop: &AtomicBool, status: &Mutex<AgentStatus>) -> std::io::Error {
    let mut published = String::new();
    let mut next_refresh = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            ws.close();
            return std::io::Error::other("stopped");
        }
        if Instant::now() >= next_refresh {
            next_refresh = Instant::now() + REFRESH;
            let (public, mut addrs) = (hooks.addrs)();
            if let Some(p) = public {
                addrs.insert(0, p);
            }
            let line = format!("addrs {}", join(&addrs));
            if line != published {
                if let Err(e) = ws.send_text(&line) {
                    return e;
                }
                info!("Connect Anywhere: reachable at {}", join(&addrs));
                published = line;
            }
            if let Err(e) = ws.send_text("ping") {
                return e;
            }
            let flipped = {
                let mut st = status.lock().unwrap();
                let flipped = !st.online || st.public != public;
                st.online = true;
                st.public = public;
                st.error = None;
                flipped
            };
            if flipped {
                (hooks.changed)();
            }
        }
        match ws.recv(Duration::from_millis(500)) {
            Ok(Some(msg)) => {
                let mut words = msg.split_whitespace();
                if words.next() == Some("connect") {
                    let viewer = words.next().unwrap_or("").to_string();
                    let addrs = parse_addrs(&words.collect::<Vec<_>>().join(" "));
                    info!(
                        "Connect Anywhere: viewer {}… at {} is connecting",
                        viewer.get(..8).unwrap_or(&viewer),
                        join(&addrs)
                    );
                    if !addrs.is_empty() {
                        (hooks.punch)(addrs);
                    }
                }
            }
            Ok(None) => {}
            Err(e) => return e,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_and_addrs() {
        let dir = std::env::temp_dir().join(format!("sard-anywhere-{}", crate::rand::u32()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Service::load(&dir).is_none());
        std::fs::write(dir.join("anywhere.txt"), "# test\nurl http://127.0.0.1:8787\ntoken abc\n").unwrap();
        let s = Service::load(&dir).unwrap();
        assert_eq!((s.url.port, s.token.as_str()), (8787, "abc"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            parse_addrs(" 1.2.3.4:5 junk [::1]:9"),
            vec!["1.2.3.4:5".parse().unwrap(), "[::1]:9".parse().unwrap()]
        );
    }
}
