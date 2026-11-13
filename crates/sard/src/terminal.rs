//! Host side of remote shells (`sard shell`, the iPhone app).
//!
//! A terminal session is a connection in the terminal lane whose hello has
//! [`client_flags::TERMINAL`]: no capture, no input injection, just shells.
//! Shells belong to the device that opened them and outlive the session: a
//! phone's connection drops whenever the app is in the background, and the
//! shell (an SSH session, a build, an editor) must still be there when it
//! comes back. Reattaching replays the recent output and makes full-screen
//! programs redraw. Shells nobody reattaches to end after [`KEEP_DETACHED`].
//!
//! Terminal access is off until the host's owner turns it on ("Allow
//! Terminal Access" in the app, `sard host --terminal`): pairing a device
//! for screen sharing shouldn't silently hand out a shell too.

use crate::Result;
use sard_core::net::{Connection, Event};
use sard_core::proto::{Control, ShellInfo};
use sard_core::pty::{Pty, clamp_size};
use sard_core::{info, warn};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// How long a shell keeps running with no session showing it.
const KEEP_DETACHED: Duration = Duration::from_secs(12 * 3600);
/// Shells per device (a runaway client must not fork the host to death).
const MAX_PER_DEVICE: usize = 12;
/// Output kept for replay when a session reattaches.
const SCROLLBACK: usize = 256 * 1024;
/// Output is held back while this many reliable segments (~1.3 KB each) are
/// unacknowledged, so `cat bigfile` streams at the path's pace instead of
/// queueing without bound. The shell blocks on its full terminal meanwhile.
const MAX_BACKLOG: usize = 384;
/// Largest output chunk per message.
const CHUNK: usize = 16 * 1024;
/// Output messages a client may have unacknowledged (`ShellAck`): at most
/// WINDOW x CHUNK bytes wait in its queues. The network layer acknowledges
/// on receipt, so without this a client slower than the network (a busy
/// phone UI) would see its event queue overflow and lose output.
const WINDOW: u32 = 128;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Output {
    /// Recent output, for replay on reattach.
    scrollback: VecDeque<u8>,
    /// Scrollback dropped its oldest bytes (replay starts mid-stream).
    trimmed: bool,
    /// The session currently showing this shell.
    conn: Option<Connection>,
    detached_since: Option<Instant>,
    size: (u16, u16),
    /// Output messages sent to `conn` and not yet acknowledged.
    in_flight: u32,
}

struct Shell {
    id: u32,
    owner: [u8; 32],
    name: String,
    started: Instant,
    pty: Box<dyn Pty>,
    out: Mutex<Output>,
}

impl Shell {
    fn info(&self) -> ShellInfo {
        let out = lock(&self.out);
        ShellInfo {
            id: self.id,
            name: self.name.clone(),
            attached: out.conn.as_ref().is_some_and(|c| !c.is_closed()),
            age_s: self.started.elapsed().as_secs().min(u32::MAX as u64) as u32,
        }
    }

    /// Show this shell in `conn`'s session (taking it from any other).
    fn attach(&self, conn: &Connection, token: u32, cols: u16, rows: u16, resumed: bool) {
        let size = clamp_size(cols, rows);
        let mut out = lock(&self.out);
        if let Some(old) = out.conn.take()
            && !old.same_session(conn)
            && !old.is_closed()
        {
            old.send_control(&Control::ShellExit { id: self.id, message: "opened in another session".into() });
        }
        conn.send_control(&Control::ShellOpened { token, id: self.id, name: self.name.clone(), resumed });
        out.in_flight = 0;
        if resumed {
            for chunk in replay_chunks(&out) {
                conn.send_control(&Control::ShellData { id: self.id, data: chunk });
                out.in_flight += 1;
            }
        }
        out.conn = Some(conn.clone());
        out.detached_since = None;
        let old_size = std::mem::replace(&mut out.size, size);
        drop(out);
        if resumed && old_size == size {
            // Same size: nudge it anyway so full-screen programs (editors,
            // top) get SIGWINCH and repaint the whole screen.
            let _ = self.pty.resize(size.0, size.1.saturating_sub(1).max(1));
        }
        let _ = self.pty.resize(size.0, size.1);
    }

    fn detach(&self, conn: &Connection) {
        let mut out = lock(&self.out);
        if out.conn.as_ref().is_some_and(|c| c.same_session(conn)) {
            out.conn = None;
            out.detached_since = Some(Instant::now());
        }
    }

    fn ack(&self, conn: &Connection, count: u32) {
        let mut out = lock(&self.out);
        if out.conn.as_ref().is_some_and(|c| c.same_session(conn)) {
            out.in_flight = out.in_flight.saturating_sub(count);
        }
    }

    fn attached_to(&self, conn: &Connection) -> bool {
        lock(&self.out).conn.as_ref().is_some_and(|c| c.same_session(conn))
    }
}

/// All shells this host runs, across sessions.
pub struct Terminals {
    shells: Mutex<HashMap<u32, Arc<Shell>>>,
    next_id: AtomicU32,
    allowed: AtomicBool,
}

impl Terminals {
    pub fn new(allowed: bool) -> Arc<Terminals> {
        let t = Arc::new(Terminals {
            shells: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            allowed: AtomicBool::new(allowed),
        });
        let weak = Arc::downgrade(&t);
        let _ = std::thread::Builder::new().name("sard-term-reaper".into()).spawn(move || reaper(weak));
        t
    }

    pub fn allowed(&self) -> bool {
        self.allowed.load(Ordering::Relaxed)
    }

    /// Turning access off also ends every running shell.
    pub fn set_allowed(&self, on: bool) {
        self.allowed.store(on, Ordering::Relaxed);
        if !on {
            self.end_where(|_| true, "terminal access was turned off on this computer");
        }
        info!("terminal access {}", if on { "on" } else { "off" });
    }

    /// A device lost its access: end its shells.
    pub fn remove_device(&self, key: &[u8; 32]) {
        self.end_where(|s| &s.owner == key, "this device's access was removed");
    }

    /// End everything (the host stops sharing).
    pub fn shutdown(&self) {
        self.end_where(|_| true, "the computer stopped sharing");
    }

    fn end_where(&self, pred: impl Fn(&Shell) -> bool, why: &str) {
        let ended: Vec<Arc<Shell>> = {
            let mut shells = lock(&self.shells);
            let ids: Vec<u32> = shells.values().filter(|s| pred(s)).map(|s| s.id).collect();
            ids.iter().filter_map(|id| shells.remove(id)).collect()
        };
        for s in ended {
            if let Some(c) = lock(&s.out).conn.take() {
                c.send_control(&Control::ShellExit { id: s.id, message: why.into() });
            }
            s.pty.hangup();
        }
    }

    fn list(&self, owner: &[u8; 32]) -> Vec<ShellInfo> {
        let mut list: Vec<ShellInfo> =
            lock(&self.shells).values().filter(|s| &s.owner == owner).map(|s| s.info()).collect();
        list.sort_by_key(|i| i.id);
        list
    }

    fn get(&self, owner: &[u8; 32], id: u32) -> Option<Arc<Shell>> {
        lock(&self.shells).get(&id).filter(|s| &s.owner == owner).cloned()
    }

    fn open(self: &Arc<Self>, conn: &Connection, token: u32, id: u32, cols: u16, rows: u16) {
        let owner = conn.peer_key();
        let refuse = |reason: &str| conn.send_control(&Control::ShellRefused { token, reason: reason.into() });
        if !self.allowed() {
            return refuse(
                "Terminal access is off on this computer. Turn on “Allow Terminal Access” in its Sard menu \
                 (Settings), then try again.",
            );
        }
        if id != 0 {
            match self.get(&owner, id) {
                Some(s) => s.attach(conn, token, cols, rows, true),
                None => refuse("that terminal has ended"),
            }
            return;
        }
        if lock(&self.shells).values().filter(|s| s.owner == owner).count() >= MAX_PER_DEVICE {
            return refuse("too many terminals are open; close one first");
        }
        let (cols, rows) = clamp_size(cols, rows);
        let pty = match spawn_shell(cols, rows) {
            Ok(p) => p,
            Err(e) => return refuse(&format!("cannot start a shell: {e}")),
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let shell = Arc::new(Shell {
            id,
            owner,
            name: pty.name(),
            started: Instant::now(),
            pty,
            out: Mutex::new(Output {
                scrollback: VecDeque::new(),
                trimmed: false,
                conn: None,
                detached_since: None,
                size: (cols, rows),
                in_flight: 0,
            }),
        });
        info!("terminal {id} ({}) started for {}", shell.name, conn.peer_name());
        lock(&self.shells).insert(id, Arc::clone(&shell));
        shell.attach(conn, token, cols, rows, false);
        let weak = Arc::downgrade(self);
        let r = std::thread::Builder::new().name(format!("sard-term-{id}")).spawn(move || pump(shell, weak));
        if let Err(e) = r {
            warn!("cannot start terminal reader: {e}");
        }
    }

    /// The session ended: its shells keep running, detached.
    fn detach_session(&self, conn: &Connection) {
        for s in lock(&self.shells).values() {
            s.detach(conn);
        }
    }
}

#[cfg(unix)]
fn spawn_shell(cols: u16, rows: u16) -> std::io::Result<Box<dyn Pty>> {
    sard_core::pty::spawn(cols, rows)
}

#[cfg(windows)]
fn spawn_shell(cols: u16, rows: u16) -> std::io::Result<Box<dyn Pty>> {
    crate::platform::spawn_shell(cols, rows)
}

/// Move a shell's output to whichever session shows it, until it exits.
fn pump(shell: Arc<Shell>, terminals: Weak<Terminals>) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        // Back-pressure: wait while the attached client is behind (its
        // window is full) or the path is (unacknowledged segments). A
        // detached shell keeps running into its scrollback.
        loop {
            let (conn, in_flight) = {
                let out = lock(&shell.out);
                (out.conn.clone(), out.in_flight)
            };
            match conn {
                Some(c) if !c.is_closed() && (in_flight >= WINDOW || c.reliable_backlog() > MAX_BACKLOG) => {
                    std::thread::sleep(Duration::from_millis(3));
                }
                _ => break,
            }
        }
        let n = match shell.pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                warn!("terminal {}: {e}", shell.id);
                break;
            }
        };
        let data = &buf[..n];
        let mut out = lock(&shell.out);
        out.scrollback.extend(data);
        if out.scrollback.len() > SCROLLBACK {
            let excess = out.scrollback.len() - SCROLLBACK;
            out.scrollback.drain(..excess);
            out.trimmed = true;
        }
        // Sent under the lock: an attach in between replays the scrollback
        // and must not see this chunk twice.
        if let Some(c) = out.conn.clone() {
            c.send_control(&Control::ShellData { id: shell.id, data: data.to_vec() });
            out.in_flight += 1;
        }
    }
    let message = shell.pty.exit_message();
    info!("terminal {} {message}", shell.id);
    if let Some(t) = terminals.upgrade() {
        lock(&t.shells).remove(&shell.id);
    }
    if let Some(c) = lock(&shell.out).conn.take() {
        c.send_control(&Control::ShellExit { id: shell.id, message });
    }
}

/// The scrollback as replay messages, starting at a line boundary if its
/// beginning was trimmed (never inside an escape sequence or a UTF-8
/// character).
fn replay_chunks(out: &Output) -> Vec<Vec<u8>> {
    let (a, b) = out.scrollback.as_slices();
    let mut replay = [a, b].concat();
    if out.trimmed
        && let Some(nl) = replay.iter().position(|&c| c == b'\n')
    {
        replay.drain(..=nl);
    }
    replay.chunks(CHUNK).map(<[u8]>::to_vec).collect()
}

fn reaper(terminals: Weak<Terminals>) {
    loop {
        std::thread::sleep(Duration::from_secs(60));
        let Some(t) = terminals.upgrade() else { return };
        t.end_where(
            |s| lock(&s.out).detached_since.is_some_and(|d| d.elapsed() > KEEP_DETACHED),
            "closed after being unused for a long time",
        );
    }
}

/// Serve one terminal session until the client leaves.
pub fn serve(conn: &Connection, events: Receiver<Event>, terminals: &Arc<Terminals>) -> Result<()> {
    let owner = conn.peer_key();
    // Also tells the client this host speaks the terminal protocol.
    conn.send_control(&Control::Shells { list: terminals.list(&owner) });
    let result = loop {
        let ev = match events.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => ev,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if conn.is_closed() {
                    break Ok(());
                }
                continue;
            }
            Err(_) => break Ok(()),
        };
        match ev {
            Event::Control(Control::ShellOpen { token, id, cols, rows }) => terminals.open(conn, token, id, cols, rows),
            Event::Control(Control::ShellData { id, data }) => {
                if let Some(s) = terminals.get(&owner, id).filter(|s| s.attached_to(conn))
                    && let Err(e) = s.pty.write(&data)
                {
                    warn!("terminal {id}: write failed: {e}");
                }
            }
            Event::Control(Control::ShellAck { id, count }) => {
                if let Some(s) = terminals.get(&owner, id) {
                    s.ack(conn, count);
                }
            }
            Event::Control(Control::ShellResize { id, cols, rows }) => {
                if let Some(s) = terminals.get(&owner, id).filter(|s| s.attached_to(conn)) {
                    let size = clamp_size(cols, rows);
                    lock(&s.out).size = size;
                    let _ = s.pty.resize(size.0, size.1);
                }
            }
            Event::Control(Control::ShellClose { id }) => {
                if let Some(s) = terminals.get(&owner, id) {
                    info!("terminal {id} closed by {}", conn.peer_name());
                    s.pty.hangup();
                }
            }
            Event::Control(Control::ShellList) => conn.send_control(&Control::Shells { list: terminals.list(&owner) }),
            Event::Control(other) => sard_core::debug!("terminal session: ignoring {other:?}"),
            Event::Disconnected(reason) => {
                info!("terminal session with {} ended: {reason}", conn.peer_name());
                break Ok(());
            }
            _ => {}
        }
    };
    terminals.detach_session(conn);
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use sard_core::crypto::Keypair;
    use sard_core::net::{Client, Host, HostOptions, NetConfig};
    use sard_core::proto::{ClientHello, Platform, client_flags, lane};

    fn hello() -> Control {
        Control::ClientHello(ClientHello {
            platform: Platform::current(),
            decoders: 0,
            codec_pref: 0,
            max_width: 0,
            max_height: 0,
            max_fps: 0,
            bitrate_kbps: 0,
            flags: client_flags::TERMINAL,
            display: 0,
            fit_width: 0,
            fit_height: 0,
        })
    }

    /// Wait for a control message matching `f`, collecting shell output.
    fn wait_for<T>(events: &Receiver<Event>, out: &mut Vec<u8>, mut f: impl FnMut(&Control, &[u8]) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(Event::Control(c)) = events.recv_timeout(Duration::from_millis(100)) {
                if let Control::ShellData { id, data } = &c {
                    out.extend_from_slice(data);
                    if let Some(conn) = ACK_TO.with(|a| a.borrow().clone()) {
                        conn.send_control(&Control::ShellAck { id: *id, count: 1 });
                    }
                }
                if let Some(v) = f(&c, out) {
                    return v;
                }
            }
        }
        panic!("timed out; output so far: {:?}", String::from_utf8_lossy(out));
    }

    fn output_contains(events: &Receiver<Event>, out: &mut Vec<u8>, needle: &str) {
        // Only the tail can hold a new match: stays linear for bulk output.
        wait_for(events, out, |_, out| {
            let tail = &out[out.len().saturating_sub(needle.len() + CHUNK)..];
            String::from_utf8_lossy(tail).contains(needle).then_some(())
        });
    }

    thread_local! {
        /// The client connection test helpers acknowledge output on.
        static ACK_TO: std::cell::RefCell<Option<Connection>> = const { std::cell::RefCell::new(None) };
    }

    fn ack_on(c: &Connection) {
        ACK_TO.with(|a| *a.borrow_mut() = Some(c.clone()));
    }

    /// End to end over real UDP: open a shell, run a command, drop the
    /// session, reconnect and find the shell (and its output) still there;
    /// a screen session from the same device doesn't replace the terminal.
    #[test]
    fn shells_survive_reconnects() {
        let host_id = Keypair::generate();
        let client_id = Keypair::generate();
        let host_key = host_id.public();
        let mut trusted = HashMap::new();
        trusted.insert(client_id.public(), "phone".to_string());
        let host = Host::bind(
            "127.0.0.1:0".parse().unwrap(),
            host_id,
            NetConfig { name: "test-host".into(), ..Default::default() },
            HostOptions { trusted, pairing_code: None, on_paired: None },
        )
        .unwrap();
        let addr = host.local_addr().unwrap().to_string();
        let terminals = Terminals::new(true);
        let t2 = Arc::clone(&terminals);
        let host = Arc::new(host);
        let h2 = Arc::clone(&host);
        std::thread::spawn(move || {
            while let Some(conn) = h2.accept(Duration::from_secs(30)) {
                let t = Arc::clone(&t2);
                std::thread::spawn(move || {
                    let events = conn.take_events().unwrap();
                    match events.recv_timeout(Duration::from_secs(5)) {
                        Ok(Event::Control(Control::ClientHello(h))) if h.flags & client_flags::TERMINAL != 0 => {
                            serve(&conn, events, &t).unwrap()
                        }
                        _ => {
                            // A screen session: just hold it open.
                            while !conn.is_closed() {
                                std::thread::sleep(Duration::from_millis(50));
                            }
                        }
                    }
                });
            }
        });
        let cfg = NetConfig { name: "phone".into(), lane: lane::TERMINAL, ..Default::default() };
        let connect = || {
            let c = Client::connect(&addr, host_key, client_id.clone(), cfg.clone(), Duration::from_secs(5)).unwrap();
            ack_on(&c);
            let ev = c.take_events().unwrap();
            c.send_control(&hello());
            (c, ev)
        };
        let mut out = Vec::new();

        let (c, ev) = connect();
        wait_for(&ev, &mut out, |m, _| matches!(m, Control::Shells { list } if list.is_empty()).then_some(()));
        c.send_control(&Control::ShellOpen { token: 1, id: 0, cols: 90, rows: 20 });
        let id = wait_for(&ev, &mut out, |m, _| match m {
            Control::ShellOpened { token: 1, id, resumed: false, .. } => Some(*id),
            _ => None,
        });
        c.send_control(&Control::ShellData { id, data: b"echo first-$((6*7)); stty size\r".to_vec() });
        output_contains(&ev, &mut out, "20 90");
        assert!(String::from_utf8_lossy(&out).contains("first-42"));

        // A screen session from the same device (other lane) leaves it alone.
        let screen = Client::connect(
            &addr,
            host_key,
            client_id.clone(),
            NetConfig { name: "phone".into(), ..Default::default() },
            Duration::from_secs(5),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!c.is_closed(), "screen session replaced the terminal session");
        drop(screen);

        // Drop the session; the shell keeps running and produces output.
        c.send_control(&Control::ShellData { id, data: b"sleep 1; echo while-away\r".to_vec() });
        std::thread::sleep(Duration::from_millis(100));
        c.close();
        drop((c, ev));
        std::thread::sleep(Duration::from_millis(1500));

        let (c, ev) = connect();
        let mut out = Vec::new();
        let listed = wait_for(&ev, &mut out, |m, _| match m {
            Control::Shells { list } => Some(list.clone()),
            _ => None,
        });
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        c.send_control(&Control::ShellOpen { token: 2, id, cols: 100, rows: 30 });
        wait_for(&ev, &mut out, |m, _| matches!(m, Control::ShellOpened { token: 2, resumed: true, .. }).then_some(()));
        output_contains(&ev, &mut out, "while-away");
        assert!(String::from_utf8_lossy(&out).contains("first-42"), "scrollback replayed");
        c.send_control(&Control::ShellData { id, data: b"exit 5\r".to_vec() });
        let msg = wait_for(&ev, &mut out, |m, _| match m {
            Control::ShellExit { id: i, message } if *i == id => Some(message.clone()),
            _ => None,
        });
        assert_eq!(msg, "exited with status 5");

        // Off: refused.
        terminals.set_allowed(false);
        c.send_control(&Control::ShellOpen { token: 3, id: 0, cols: 80, rows: 24 });
        wait_for(&ev, &mut out, |m, _| matches!(m, Control::ShellRefused { token: 3, .. }).then_some(()));
    }

    /// Lots of output arrives complete and in order (back-pressure, no drops).
    #[test]
    fn bulk_output_is_complete() {
        let host_id = Keypair::generate();
        let client_id = Keypair::generate();
        let host_key = host_id.public();
        let mut trusted = HashMap::new();
        trusted.insert(client_id.public(), "phone".to_string());
        let host = Host::bind(
            "127.0.0.1:0".parse().unwrap(),
            host_id,
            NetConfig::default(),
            HostOptions { trusted, pairing_code: None, on_paired: None },
        )
        .unwrap();
        let addr = host.local_addr().unwrap().to_string();
        let terminals = Terminals::new(true);
        std::thread::spawn(move || {
            let conn = host.accept(Duration::from_secs(10)).unwrap();
            let events = conn.take_events().unwrap();
            let _ = events.recv_timeout(Duration::from_secs(5));
            serve(&conn, events, &terminals).unwrap();
        });
        let cfg = NetConfig { lane: lane::TERMINAL, ..Default::default() };
        let c = Client::connect(&addr, host_key, client_id, cfg, Duration::from_secs(5)).unwrap();
        ack_on(&c);
        let ev = c.take_events().unwrap();
        c.send_control(&hello());
        let mut out = Vec::new();
        c.send_control(&Control::ShellOpen { token: 1, id: 0, cols: 200, rows: 50 });
        let id = wait_for(&ev, &mut out, |m, _| match m {
            Control::ShellOpened { id, .. } => Some(*id),
            _ => None,
        });
        // ~2.4 MB of numbered lines, then a marker.
        c.send_control(&Control::ShellData { id, data: b"seq 1 300000; echo END-OF-$((1+1))\r".to_vec() });
        output_contains(&ev, &mut out, "END-OF-2");
        let text = String::from_utf8_lossy(&out);
        let nums: Vec<u32> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
        let start = nums.iter().position(|&n| n == 1).expect("seq output");
        let run = &nums[start..];
        assert_eq!(run.len(), 300000, "lines lost or duplicated");
        assert!(run.iter().enumerate().all(|(i, &n)| n as usize == i + 1));
    }
}
