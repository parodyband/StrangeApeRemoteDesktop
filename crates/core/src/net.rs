//! UDP transport: endpoint, handshakes, sessions.
//!
//! One UDP socket per endpoint and one receive thread that also runs all
//! timers (~1 ms tick). Senders (encoder thread, input thread, ...) send
//! directly on the shared socket; there is no async runtime.
//!
//! Sessions are keyed by the receiver-chosen 32-bit index in every DATA
//! packet, so routing never depends on the source address: peers may roam,
//! and a relay could forward by index (room for future routing/NAT traversal).

use crate::clock::{self, ClockSync};
use crate::crypto::{
    CryptoError, IkInitiator, IkResponder, Keypair, PairInitiator, PairResponder, ReplayWindow, TransportCipher,
};
use crate::proto::{Control, HelloPayload, PairPayload, Report, msg, packet};
use crate::reliable::ReliableChannel;
use crate::video::{CompleteFrame, RecvEvent, VideoReceiver, VideoSender};
use crate::wire::{Put, Reader};
use crate::{PROTOCOL_VERSION, debug, info, warn};
use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const IK_PROLOGUE: &[u8] = b"sard/1 connect";
const PAIR_PROLOGUE: &[u8] = b"sard/1 pair";

const PING_INTERVAL_US: u64 = 250_000;
const REPORT_INTERVAL_US: u64 = 250_000;
const KEEPALIVE_US: u64 = 1_000_000;
const TIMEOUT_US: u64 = 10_000_000;

#[derive(Debug)]
pub enum NetError {
    Io(io::Error),
    Crypto(CryptoError),
    Timeout,
    Rejected(String),
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetError::Io(e) => write!(f, "network error: {e}"),
            NetError::Crypto(e) => write!(f, "handshake failed: {e}"),
            NetError::Timeout => f.write_str("timed out (is the host running, reachable, and are we paired?)"),
            NetError::Rejected(s) => write!(f, "rejected: {s}"),
        }
    }
}

impl std::error::Error for NetError {}

impl From<io::Error> for NetError {
    fn from(e: io::Error) -> Self {
        NetError::Io(e)
    }
}

impl From<CryptoError> for NetError {
    fn from(e: CryptoError) -> Self {
        NetError::Crypto(e)
    }
}

#[derive(Clone, Debug)]
pub struct NetConfig {
    /// Largest UDP payload we send (bytes). 1400 is safe on Ethernet/Wi-Fi;
    /// use ~1200 through VPNs/tunnels.
    pub max_datagram: usize,
    /// Display name sent to peers.
    pub name: String,
    /// Pace video at this rate (Mbit/s); 0 = send at line rate.
    pub pace_mbps: u32,
    /// Socket buffer size request (bytes).
    pub socket_buffer: usize,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self { max_datagram: 1400, name: crate::config::hostname(), pace_mbps: 0, socket_buffer: 8 << 20 }
    }
}

impl NetConfig {
    pub fn max_payload(&self) -> usize {
        self.max_datagram - packet::DATA_OVERHEAD
    }
}

/// Things the application needs to react to.
#[derive(Debug)]
pub enum Event {
    Control(Control),
    /// Absolute pointer position (normalized 0..1), latest wins.
    MouseAbs {
        x: f32,
        y: f32,
    },
    /// Host cursor position (normalized) and visibility.
    CursorPos {
        x: f32,
        y: f32,
        visible: bool,
    },
    /// Remote decoder needs a keyframe.
    KeyframeRequest,
    /// Receiver statistics from the peer.
    Report(Report),
    Audio(Vec<u8>),
    Disconnected(String),
}

/// Snapshot of connection statistics.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConnStats {
    pub rtt_us: u64,
    /// Minimum RTT over the recent ping window (baseline without queueing).
    pub min_rtt_us: u64,
    pub clock_offset_us: Option<i64>,
    pub tx_bps: f64,
    pub rx_bps: f64,
    pub tx_packets: u64,
    pub rx_packets: u64,
    pub fec_ratio: f32,
    pub video_lost_packets: u64,
    pub video_fec_recovered: u64,
    pub video_rtx_recovered: u64,
    pub video_frames_lost: u64,
    pub reliable_retransmits: u64,
    pub video_retransmits: u64,
}

struct ConnState {
    reliable: ReliableChannel,
    video_rx: VideoReceiver,
    clock: ClockSync,
    srtt_us: f64,
    last_ping_us: u64,
    last_report_us: u64,
    last_kf_req_us: u64,
    kf_req_id: u32,
    last_kf_req_rx: Option<u32>,
    mouse_seq_rx: Option<u32>,
    cursor_seq_rx: Option<u32>,
    seen_video: bool,
    totals: ConnStats,
    rate_since_us: u64,
    tx_bytes_window: u64,
    rx_bytes_window: u64,
}

/// Where a session's packets go. On macOS each host session gets its own
/// `connect()`ed socket: Darwin runs a costly per-datagram policy check
/// (Local Network privacy / NECP) on *unconnected* UDP sockets, which caps
/// throughput at a few thousand packets/s and builds 100+ ms queues.
struct ConnIo {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    /// `socket` is connected to `peer`: use send(), not send_to().
    connected: bool,
}

struct Conn {
    io: Mutex<ConnIo>,
    local_index: u32,
    remote_index: u32,
    remote_static: [u8; 32],
    remote_name: String,
    send_cipher: TransportCipher,
    recv_cipher: TransportCipher,
    send_counter: AtomicU64,
    replay: Mutex<ReplayWindow>,
    state: Mutex<ConnState>,
    video_tx: Mutex<VideoSender>,
    events_tx: SyncSender<Event>,
    events_rx: Mutex<Option<Receiver<Event>>>,
    frames_tx: SyncSender<CompleteFrame>,
    /// The application consumes video (`take_video`); otherwise VIDEO is ignored.
    video_taken: AtomicBool,
    frames_rx: Mutex<Option<Receiver<CompleteFrame>>>,
    closed: AtomicBool,
    last_recv_us: AtomicU64,
    last_send_us: AtomicU64,
    tx_bytes: AtomicU64,
    tx_packets: AtomicU64,
    mouse_seq_tx: AtomicU32,
    cursor_seq_tx: AtomicU32,
    max_payload: usize,
    pace_mbps: u32,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Conn {
    #[allow(clippy::too_many_arguments)]
    fn new(
        io: ConnIo,
        local_index: u32,
        remote_index: u32,
        remote_static: [u8; 32],
        remote_name: String,
        send: TransportCipher,
        recv: TransportCipher,
        cfg: &NetConfig,
    ) -> Arc<Self> {
        // Bounded: a peer must not be able to grow our memory without limit.
        let (events_tx, events_rx) = mpsc::sync_channel(4096);
        let (frames_tx, frames_rx) = mpsc::sync_channel(64);
        let now = clock::now_us();
        let mp = cfg.max_payload();
        Arc::new(Conn {
            io: Mutex::new(io),
            local_index,
            remote_index,
            remote_static,
            remote_name,
            send_cipher: send,
            recv_cipher: recv,
            send_counter: AtomicU64::new(0),
            replay: Mutex::new(ReplayWindow::default()),
            state: Mutex::new(ConnState {
                reliable: ReliableChannel::new(mp),
                video_rx: VideoReceiver::new(),
                clock: ClockSync::new(),
                srtt_us: 5_000.0,
                last_ping_us: 0,
                last_report_us: now,
                last_kf_req_us: 0,
                kf_req_id: 0,
                last_kf_req_rx: None,
                mouse_seq_rx: None,
                cursor_seq_rx: None,
                seen_video: false,
                totals: ConnStats { fec_ratio: 0.10, ..Default::default() },
                rate_since_us: now,
                tx_bytes_window: 0,
                rx_bytes_window: 0,
            }),
            video_tx: Mutex::new(VideoSender::new(mp)),
            events_tx,
            events_rx: Mutex::new(Some(events_rx)),
            frames_tx,
            frames_rx: Mutex::new(Some(frames_rx)),
            video_taken: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            last_recv_us: AtomicU64::new(now),
            last_send_us: AtomicU64::new(now),
            tx_bytes: AtomicU64::new(0),
            tx_packets: AtomicU64::new(0),
            mouse_seq_tx: AtomicU32::new(0),
            cursor_seq_tx: AtomicU32::new(0),
            max_payload: mp,
            pace_mbps: cfg.pace_mbps,
        })
    }

    /// Encrypt and send one payload.
    fn send_payload(&self, payload: &[u8]) {
        debug_assert!(payload.len() <= self.max_payload, "payload {} > {}", payload.len(), self.max_payload);
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        let counter = self.send_counter.fetch_add(1, Ordering::Relaxed);
        let mut pkt = [0u8; 2048];
        let n = payload.len();
        pkt[0] = packet::DATA;
        pkt[1..5].copy_from_slice(&self.remote_index.to_le_bytes());
        pkt[5..13].copy_from_slice(&counter.to_le_bytes());
        pkt[13..13 + n].copy_from_slice(payload);
        let (hdr, body) = pkt.split_at_mut(packet::DATA_HEADER_LEN);
        let tag = self.send_cipher.seal(counter, hdr, &mut body[..n]);
        body[n..n + 16].copy_from_slice(&tag);
        let total = packet::DATA_HEADER_LEN + n + 16;
        {
            let io = lock(&self.io);
            let r =
                if io.connected { io.socket.send(&pkt[..total]) } else { io.socket.send_to(&pkt[..total], io.peer) };
            if let Err(e) = r {
                // ENOBUFS / EAGAIN under bursts: drop, FEC/NACK will cover it.
                debug!("send to {}: {e}", io.peer);
            }
        }
        self.tx_bytes.fetch_add(total as u64, Ordering::Relaxed);
        self.tx_packets.fetch_add(1, Ordering::Relaxed);
        self.last_send_us.store(clock::now_us(), Ordering::Relaxed);
    }

    fn handle_payload(&self, p: &[u8], now: u64) {
        if p.is_empty() {
            return;
        }
        let body = &p[1..];
        match p[0] {
            msg::VIDEO => {
                if !self.video_taken.load(Ordering::Relaxed) {
                    return; // this side doesn't receive video
                }
                let mut st = lock(&self.state);
                st.seen_video = true;
                if let Err(e) = st.video_rx.on_shard(body, now) {
                    debug!("bad video shard: {e}");
                }
                while let Some(f) = st.video_rx.pop_ready() {
                    if self.frames_tx.try_send(f).is_err() {
                        // Decoder is not keeping up: later frames would reference
                        // the dropped one, so resynchronize on a keyframe.
                        warn!("video queue full; dropping frames until the next keyframe");
                        st.video_rx.resync();
                        st.last_kf_req_us = 0;
                        break;
                    }
                }
            }
            msg::RELIABLE => {
                let (msgs, ack) = {
                    let mut st = lock(&self.state);
                    let msgs = st.reliable.on_segment(body);
                    (msgs, st.reliable.take_ack())
                };
                if let Some(a) = ack {
                    self.send_payload(&a);
                }
                match msgs {
                    Ok(msgs) => {
                        for m in msgs {
                            match Control::decode(&m) {
                                Ok(c) => {
                                    let _ = self.events_tx.try_send(Event::Control(c));
                                }
                                Err(_) => debug!("unknown control message type {:?}", m.first()),
                            }
                        }
                    }
                    Err(e) => warn!("reliable channel error: {e}"),
                }
            }
            msg::ACK => {
                let mut st = lock(&self.state);
                if let Ok(Some(rtt)) = st.reliable.on_ack(body, now) {
                    st.srtt_us = st.srtt_us * 0.875 + rtt as f64 * 0.125;
                }
            }
            msg::NACK => {
                let mut tx = lock(&self.video_tx);
                if let Err(e) = tx.on_nack(body, |pl| self.send_payload(pl)) {
                    debug!("bad nack: {e}");
                }
            }
            msg::PING => {
                let mut r = Reader::new(body);
                if let Ok(t0) = r.u64() {
                    let mut out = Vec::with_capacity(25);
                    out.put_u8(msg::PONG);
                    out.put_u64(t0);
                    out.put_u64(now);
                    out.put_u64(clock::now_us());
                    self.send_payload(&out);
                }
            }
            msg::PONG => {
                let mut r = Reader::new(body);
                if let (Ok(t0), Ok(t1), Ok(t2)) = (r.u64(), r.u64(), r.u64()) {
                    let mut st = lock(&self.state);
                    // Only answers to the ping we actually sent count.
                    if t0 != st.last_ping_us || now < t0 || t2 < t1 {
                        return;
                    }
                    st.clock.add(t0, t1, t2, now);
                    let rtt = (now.saturating_sub(t0)).saturating_sub(t2.saturating_sub(t1));
                    st.srtt_us = st.srtt_us * 0.875 + rtt as f64 * 0.125;
                }
            }
            msg::MOUSE_ABS => {
                let mut r = Reader::new(body);
                if let (Ok(seq), Ok(x), Ok(y)) = (r.u32(), r.f32(), r.f32()) {
                    let mut st = lock(&self.state);
                    if newer(seq, &mut st.mouse_seq_rx) {
                        let _ = self.events_tx.try_send(Event::MouseAbs { x, y });
                    }
                }
            }
            msg::CURSOR_POS => {
                let mut r = Reader::new(body);
                if let (Ok(seq), Ok(x), Ok(y), Ok(v)) = (r.u32(), r.f32(), r.f32(), r.u8()) {
                    let mut st = lock(&self.state);
                    if newer(seq, &mut st.cursor_seq_rx) {
                        let _ = self.events_tx.try_send(Event::CursorPos { x, y, visible: v != 0 });
                    }
                }
            }
            msg::KEYFRAME_REQ => {
                let mut r = Reader::new(body);
                if let Ok(id) = r.u32() {
                    let mut st = lock(&self.state);
                    if st.last_kf_req_rx != Some(id) {
                        st.last_kf_req_rx = Some(id);
                        let _ = self.events_tx.try_send(Event::KeyframeRequest);
                    }
                }
            }
            msg::REPORT => {
                let mut r = Reader::new(body);
                if let Ok(rep) = Report::decode(&mut r) {
                    self.adapt_fec(&rep);
                    let _ = self.events_tx.try_send(Event::Report(rep));
                }
            }
            msg::AUDIO => {
                let _ = self.events_tx.try_send(Event::Audio(body.to_vec()));
            }
            msg::KEEPALIVE => {}
            msg::DISCONNECT => {
                if !self.closed.swap(true, Ordering::Relaxed) {
                    let _ = self.events_tx.try_send(Event::Disconnected("peer disconnected".into()));
                }
            }
            t => debug!("unknown message type {t}"),
        }
    }

    /// Adapt FEC overhead to the loss the receiver reports.
    fn adapt_fec(&self, rep: &Report) {
        let total = rep.packets_received + rep.packets_lost;
        if total < 50 {
            return;
        }
        let loss = rep.packets_lost as f32 / total as f32;
        // Enough parity to absorb ~3x the observed loss, at least 5%, at most 50%.
        let target = (loss * 3.0 + 0.05).clamp(0.05, 0.5);
        // Lock order: never hold `video_tx` and `state` at the same time.
        let ratio = {
            let mut tx = lock(&self.video_tx);
            let cur = tx.fec.ratio;
            tx.fec.ratio = if target > cur { target } else { cur * 0.9 + target * 0.1 };
            tx.fec.ratio
        };
        lock(&self.state).totals.fec_ratio = ratio;
    }

    fn tick(&self, now: u64) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        let mut sends: Vec<Vec<u8>> = Vec::new();
        {
            let mut st = lock(&self.state);
            let rtt = st.srtt_us as u64;
            let rto = (rtt * 2).max(15_000);
            sends.extend(st.reliable.poll(now, rto));

            for ev in st.video_rx.poll(now, rtt) {
                match ev {
                    RecvEvent::Nack(n) => sends.push(n),
                    RecvEvent::NeedKeyframe { .. } => {
                        st.last_kf_req_us = 0; // force immediate request below
                    }
                }
            }
            if st.seen_video
                && st.video_rx.is_awaiting_resync()
                && now.saturating_sub(st.last_kf_req_us) > (rtt * 3).max(100_000)
            {
                st.kf_req_id = st.kf_req_id.wrapping_add(1);
                st.last_kf_req_us = now;
                let mut m = Vec::with_capacity(5);
                m.put_u8(msg::KEYFRAME_REQ);
                m.put_u32(st.kf_req_id);
                sends.push(m);
                debug!("requesting keyframe #{}", st.kf_req_id);
            }

            if now.saturating_sub(st.last_ping_us) >= PING_INTERVAL_US {
                st.last_ping_us = now;
                let mut m = Vec::with_capacity(9);
                m.put_u8(msg::PING);
                m.put_u64(now);
                sends.push(m);
            }

            if st.seen_video && now.saturating_sub(st.last_report_us) >= REPORT_INTERVAL_US {
                st.last_report_us = now;
                let c = st.video_rx.take_counters();
                st.totals.video_lost_packets += c.lost_packets as u64;
                st.totals.video_fec_recovered += c.fec_recovered as u64;
                st.totals.video_rtx_recovered += c.rtx_recovered as u64;
                st.totals.video_frames_lost += c.frames_lost as u64;
                let rep = Report {
                    packets_received: c.packets,
                    packets_lost: c.lost_packets,
                    fec_recovered: c.fec_recovered,
                    retransmit_recovered: c.rtx_recovered,
                    frames_lost: c.frames_lost,
                    frames_decoded: c.frames_complete,
                    decode_us: 0,
                    jitter_us: if c.frames_complete > 0 {
                        (c.assembly_us_total / c.frames_complete as u64) as u32
                    } else {
                        0
                    },
                };
                let mut m = Vec::with_capacity(40);
                m.put_u8(msg::REPORT);
                rep.encode_into(&mut m);
                sends.push(m);
            }
        }
        for s in &sends {
            self.send_payload(s);
        }

        let last_send = self.last_send_us.load(Ordering::Relaxed);
        if now.saturating_sub(last_send) > KEEPALIVE_US {
            self.send_payload(&[msg::KEEPALIVE]);
        }
        let last_recv = self.last_recv_us.load(Ordering::Relaxed);
        if now.saturating_sub(last_recv) > TIMEOUT_US && !self.closed.swap(true, Ordering::Relaxed) {
            let _ = self.events_tx.try_send(Event::Disconnected("connection timed out".into()));
        }
    }

    fn stats(&self) -> ConnStats {
        let now = clock::now_us();
        // Lock order: copy what we need from `video_tx` first, then `state`.
        let (video_retransmits, fec_ratio) = {
            let tx = lock(&self.video_tx);
            (tx.stats_retransmitted, tx.fec.ratio)
        };
        let mut st = lock(&self.state);
        let mut s = st.totals;
        s.rtt_us = st.srtt_us as u64;
        s.min_rtt_us = st.clock.min_rtt_us().unwrap_or(0);
        s.clock_offset_us = st.clock.offset_us();
        s.tx_packets = self.tx_packets.load(Ordering::Relaxed);
        s.reliable_retransmits = st.reliable.retransmits;
        s.video_retransmits = video_retransmits;
        s.fec_ratio = fec_ratio;
        let dt = now.saturating_sub(st.rate_since_us);
        if dt >= 500_000 {
            let txb = self.tx_bytes.load(Ordering::Relaxed);
            st.totals.tx_bps = (txb - st.tx_bytes_window) as f64 * 8e6 / dt as f64;
            st.totals.rx_bps = st.rx_bytes_window as f64 * 8e6 / dt as f64;
            st.tx_bytes_window = txb;
            st.rx_bytes_window = 0;
            st.rate_since_us = now;
            s.tx_bps = st.totals.tx_bps;
            s.rx_bps = st.totals.rx_bps;
        }
        s
    }
}

fn newer(seq: u32, last: &mut Option<u32>) -> bool {
    match *last {
        Some(l) if (seq.wrapping_sub(l) as i32) <= 0 => false,
        _ => {
            *last = Some(seq);
            true
        }
    }
}

/// Cloneable handle to an established, encrypted session.
#[derive(Clone)]
pub struct Connection {
    conn: Arc<Conn>,
    endpoint: Arc<Shared>,
    /// Client connections own their endpoint: dropping the last handle stops
    /// the network thread. Host connections share the host's endpoint.
    _guard: Option<Arc<StopGuard>>,
}

struct StopGuard(Arc<Shared>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::Relaxed);
    }
}

impl Connection {
    pub fn peer_name(&self) -> &str {
        &self.conn.remote_name
    }

    pub fn peer_key(&self) -> [u8; 32] {
        self.conn.remote_static
    }

    pub fn peer_addr(&self) -> SocketAddr {
        lock(&self.conn.io).peer
    }

    pub fn is_closed(&self) -> bool {
        self.conn.closed.load(Ordering::Relaxed)
    }

    /// Take the event receiver (once).
    pub fn take_events(&self) -> Option<Receiver<Event>> {
        lock(&self.conn.events_rx).take()
    }

    /// Take the receiver of reassembled video frames (once).
    pub fn take_video(&self) -> Option<Receiver<CompleteFrame>> {
        self.conn.video_taken.store(true, Ordering::Relaxed);
        lock(&self.conn.frames_rx).take()
    }

    pub fn send_control(&self, c: &Control) {
        let now = clock::now_us();
        let pkts = lock(&self.conn.state).reliable.send(&c.encode(), now);
        for p in pkts {
            self.conn.send_payload(&p);
        }
    }

    /// Packetize, protect and send an encoded frame (meta + bitstream).
    pub fn send_video_frame(&self, frame: &[u8], flags: u8) -> u32 {
        let pace = self.conn.pace_mbps;
        let mut tx = lock(&self.conn.video_tx);
        if pace == 0 {
            tx.send_frame(frame, flags, |p| self.conn.send_payload(p))
        } else {
            let ns_per_byte = 8_000.0 / pace as f64;
            let start = Instant::now();
            let mut budget_ns = 0.0f64;
            tx.send_frame(frame, flags, |p| {
                self.conn.send_payload(p);
                budget_ns += (p.len() + packet::DATA_OVERHEAD + 28) as f64 * ns_per_byte;
                let ahead = budget_ns - start.elapsed().as_nanos() as f64;
                if ahead > 200_000.0 {
                    std::thread::sleep(Duration::from_nanos(ahead as u64 - 100_000));
                }
            })
        }
    }

    pub fn set_fec_ratio(&self, ratio: f32) {
        lock(&self.conn.video_tx).fec.ratio = ratio;
    }

    pub fn send_mouse_abs(&self, x: f32, y: f32) {
        let seq = self.conn.mouse_seq_tx.fetch_add(1, Ordering::Relaxed);
        let mut m = Vec::with_capacity(13);
        m.put_u8(msg::MOUSE_ABS);
        m.put_u32(seq);
        m.put_f32(x);
        m.put_f32(y);
        self.conn.send_payload(&m);
    }

    pub fn send_cursor_pos(&self, x: f32, y: f32, visible: bool) {
        let seq = self.conn.cursor_seq_tx.fetch_add(1, Ordering::Relaxed);
        let mut m = Vec::with_capacity(14);
        m.put_u8(msg::CURSOR_POS);
        m.put_u32(seq);
        m.put_f32(x);
        m.put_f32(y);
        m.put_u8(visible as u8);
        self.conn.send_payload(&m);
    }

    /// Unreliable audio packet (payload format defined by the audio module).
    pub fn send_audio(&self, payload: &[u8]) {
        let mut m = Vec::with_capacity(payload.len() + 1);
        m.put_u8(msg::AUDIO);
        m.put_bytes(payload);
        self.conn.send_payload(&m);
    }

    /// Ask the sender for a keyframe right away.
    pub fn request_keyframe(&self) {
        let mut st = lock(&self.conn.state);
        st.kf_req_id = st.kf_req_id.wrapping_add(1);
        st.last_kf_req_us = clock::now_us();
        let mut m = Vec::with_capacity(5);
        m.put_u8(msg::KEYFRAME_REQ);
        m.put_u32(st.kf_req_id);
        drop(st);
        self.conn.send_payload(&m);
    }

    pub fn max_payload(&self) -> usize {
        self.conn.max_payload
    }

    pub fn stats(&self) -> ConnStats {
        self.conn.stats()
    }

    /// Convert a peer monotonic timestamp (µs) to our clock, once clock sync
    /// has samples.
    pub fn remote_to_local_us(&self, remote: u64) -> Option<u64> {
        lock(&self.conn.state).clock.remote_to_local(remote)
    }

    pub fn rtt_us(&self) -> u64 {
        lock(&self.conn.state).srtt_us as u64
    }

    /// Close: tell the peer and stop processing.
    pub fn close(&self) {
        if !self.conn.closed.load(Ordering::Relaxed) {
            for _ in 0..3 {
                self.conn.send_payload(&[msg::DISCONNECT]);
            }
            self.conn.closed.store(true, Ordering::Relaxed);
        }
        lock(&self.endpoint.sessions).remove(&self.conn.local_index);
    }
}

// ---------------------------------------------------------------------------
// Endpoint
// ---------------------------------------------------------------------------

type PairedFn = Box<dyn Fn([u8; 32], &str) + Send + Sync>;

struct PendingConnect {
    local_index: u32,
    initiator: Option<IkInitiator>,
    result: Sender<Result<Arc<Conn>, NetError>>,
}

struct HandshakeCache {
    timestamp: u64,
    msg1_hash: [u8; 32],
    response: Vec<u8>,
}

/// How long a pairing code stays valid.
pub const PAIRING_TTL: Duration = Duration::from_secs(600);

#[derive(Clone, Copy)]
struct PairingWindow {
    psk: [u8; 32],
    expires_us: u64,
}

struct TokenBucket {
    tokens: f64,
    last_us: u64,
    rate: f64,
    burst: f64,
}

impl TokenBucket {
    fn new(rate: f64, burst: f64) -> Self {
        Self { tokens: burst, last_us: clock::now_us(), rate, burst }
    }
    fn take(&mut self, now: u64) -> bool {
        self.tokens = (self.tokens + now.saturating_sub(self.last_us) as f64 * self.rate / 1e6).min(self.burst);
        self.last_us = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

struct Shared {
    socket: Arc<UdpSocket>,
    /// Client role: the socket is `connect()`ed to the host.
    connected: bool,
    identity: Keypair,
    cfg: NetConfig,
    sessions: Mutex<HashMap<u32, Arc<Conn>>>,
    stop: AtomicBool,
    // Host role
    trusted: Mutex<HashMap<[u8; 32], String>>,
    /// Active pairing window (single use, expires).
    pairing: Mutex<Option<PairingWindow>>,
    /// Last pairing reply, for retransmitted PAIR_INITs after the code was used.
    pair_cache: Mutex<Option<([u8; 32], Vec<u8>)>>,
    /// Our mac1 key (hash of our public key): only paired clients can compute it.
    mac1_key: [u8; 32],
    /// Recent handshake responses by request hash (retransmits skip the DH).
    recent_handshakes: Mutex<std::collections::VecDeque<([u8; 32], Vec<u8>)>>,
    /// Rate limit for expensive handshake/pairing work.
    handshake_bucket: Mutex<TokenBucket>,
    last_pair_warn: AtomicU64,
    on_paired: Mutex<Option<PairedFn>>,
    accept_tx: Mutex<Option<Sender<Connection>>>,
    hs_cache: Mutex<HashMap<[u8; 32], HandshakeCache>>,
    last_untrusted_warn: AtomicU64,
    /// Testing aid: drop this fraction of incoming DATA packets (`SARD_SIM_LOSS`).
    sim_loss: f64,
    sim_rng: Mutex<crate::rand::XorShift>,
    // Client role
    pending: Mutex<Option<PendingConnect>>,
}

fn configure_socket(sock: &UdpSocket, buf: usize) {
    use std::os::fd::AsRawFd;
    let fd = sock.as_raw_fd();
    let set = |level: libc::c_int, opt: libc::c_int, v: libc::c_int| {
        // SAFETY: fd is a valid socket; value pointer/len are correct.
        unsafe {
            libc::setsockopt(
                fd,
                level,
                opt,
                (&v as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        }
    };
    set(libc::SOL_SOCKET, libc::SO_RCVBUF, buf as libc::c_int);
    set(libc::SOL_SOCKET, libc::SO_SNDBUF, buf as libc::c_int);
    // DSCP EF-ish (0xB8): Wi-Fi WMM maps it to the video/voice access category.
    set(libc::IPPROTO_IP, libc::IP_TOS, 0xB8);
    let get = |opt: libc::c_int| -> i32 {
        let mut v: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: as above.
        unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, opt, (&mut v as *mut libc::c_int).cast(), &mut len) };
        v
    };
    let rcv = get(libc::SO_RCVBUF);
    debug!("socket buffers: rcv={} snd={}", rcv, get(libc::SO_SNDBUF));
    if (rcv as usize) < buf / 2 {
        warn!(
            "UDP receive buffer is only {} KiB; bursts may drop packets. Raise it with: \
             sudo sysctl -w net.core.rmem_max={buf} (Linux)",
            rcv / 1024
        );
    }
}

impl Shared {
    fn new(socket: UdpSocket, identity: Keypair, cfg: NetConfig, connected: bool) -> io::Result<Arc<Self>> {
        let identity_public = identity.public();
        configure_socket(&socket, cfg.socket_buffer);
        socket.set_read_timeout(Some(Duration::from_millis(1)))?;
        Ok(Arc::new(Shared {
            socket: Arc::new(socket),
            connected,
            identity,
            cfg,
            sessions: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
            trusted: Mutex::new(HashMap::new()),
            pairing: Mutex::new(None),
            pair_cache: Mutex::new(None),
            mac1_key: crate::crypto::mac1_key(&identity_public),
            recent_handshakes: Mutex::new(std::collections::VecDeque::new()),
            handshake_bucket: Mutex::new(TokenBucket::new(50.0, 20.0)),
            last_pair_warn: AtomicU64::new(0),
            on_paired: Mutex::new(None),
            accept_tx: Mutex::new(None),
            hs_cache: Mutex::new(HashMap::new()),
            last_untrusted_warn: AtomicU64::new(0),
            sim_loss: std::env::var("SARD_SIM_LOSS").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0),
            sim_rng: Mutex::new(crate::rand::XorShift(crate::rand::u64() | 1)),
            pending: Mutex::new(None),
        }))
    }

    /// macOS: a socket sharing the listening port, connected to `peer`.
    #[cfg(target_os = "macos")]
    fn session_socket(&self, peer: SocketAddr) -> Option<Arc<UdpSocket>> {
        let local = self.socket.local_addr().ok()?;
        let r = (|| -> io::Result<UdpSocket> {
            let s = bind_reusable(SocketAddr::new(local.ip(), local.port()))?;
            s.connect(peer)?;
            configure_socket(&s, self.cfg.socket_buffer);
            s.set_read_timeout(Some(Duration::from_millis(50)))?;
            Ok(s)
        })();
        match r {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                warn!("per-session socket for {peer} failed ({e}); using the shared socket");
                None
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn session_socket(&self, _peer: SocketAddr) -> Option<Arc<UdpSocket>> {
        None
    }

    /// Receive loop for a per-session socket (ends when the session closes
    /// or the peer roams to another address).
    fn spawn_session_reader(self: &Arc<Self>, sock: Arc<UdpSocket>, conn: Arc<Conn>) {
        let me = Arc::clone(self);
        let r = std::thread::Builder::new().name("sard-net-session".into()).spawn(move || {
            raise_thread_priority();
            let mut buf = [0u8; 2048];
            while !me.stop.load(Ordering::Relaxed) && !conn.closed.load(Ordering::Relaxed) {
                match sock.recv_from(&mut buf) {
                    Ok((n, from)) => me.on_packet(&buf[..n], from),
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                    Err(e) => {
                        debug!("session recv: {e}");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                if !Arc::ptr_eq(&lock(&conn.io).socket, &sock) {
                    break;
                }
            }
        });
        if let Err(e) = r {
            warn!("cannot spawn session reader: {e}");
        }
    }

    fn spawn(self: &Arc<Self>) {
        let me = Arc::clone(self);
        std::thread::Builder::new().name("sard-net".into()).spawn(move || me.run()).expect("spawn net thread");
    }

    fn run(self: Arc<Self>) {
        raise_thread_priority();
        let mut buf = [0u8; 2048];
        let mut last_tick = 0u64;
        while !self.stop.load(Ordering::Relaxed) {
            match self.socket.recv_from(&mut buf) {
                Ok((n, from)) => self.on_packet(&buf[..n], from),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                Err(e) => {
                    debug!("recv: {e}");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            let now = clock::now_us();
            if now - last_tick >= 1_000 {
                last_tick = now;
                let sessions: Vec<Arc<Conn>> = lock(&self.sessions).values().cloned().collect();
                for c in sessions {
                    c.tick(now);
                    if c.closed.load(Ordering::Relaxed) {
                        lock(&self.sessions).remove(&c.local_index);
                    }
                }
            }
        }
    }

    fn on_packet(self: &Arc<Self>, pkt: &[u8], from: SocketAddr) {
        if pkt.is_empty() {
            return;
        }
        if self.sim_loss > 0.0 && pkt[0] == packet::DATA && lock(&self.sim_rng).chance(self.sim_loss) {
            return;
        }
        match pkt[0] {
            packet::DATA => self.on_data(pkt, from),
            packet::HANDSHAKE_INIT => {
                if let Err(e) = self.on_handshake_init(pkt, from) {
                    debug!("handshake from {from} failed: {e}");
                }
            }
            packet::HANDSHAKE_RESP => self.on_handshake_resp(pkt, from),
            packet::PAIR_INIT => {
                if let Err(e) = self.on_pair_init(pkt, from) {
                    debug!("pairing attempt from {from} failed: {e}");
                }
            }
            _ => {}
        }
    }

    fn on_data(&self, pkt: &[u8], from: SocketAddr) {
        if pkt.len() < packet::DATA_OVERHEAD + 1 {
            return;
        }
        let idx = u32::from_le_bytes(pkt[1..5].try_into().unwrap());
        let counter = u64::from_le_bytes(pkt[5..13].try_into().unwrap());
        let Some(conn) = lock(&self.sessions).get(&idx).cloned() else {
            return;
        };
        if !lock(&conn.replay).check(counter) {
            return;
        }
        let body_len = pkt.len() - packet::DATA_OVERHEAD;
        let mut body = [0u8; 2048];
        body[..body_len].copy_from_slice(&pkt[packet::DATA_HEADER_LEN..packet::DATA_HEADER_LEN + body_len]);
        let tag = &pkt[pkt.len() - 16..];
        if conn.recv_cipher.open(counter, &pkt[..packet::DATA_HEADER_LEN], &mut body[..body_len], tag).is_err() {
            return;
        }
        lock(&conn.replay).update(counter);
        let now = clock::now_us();
        conn.last_recv_us.store(now, Ordering::Relaxed);
        {
            let mut io = lock(&conn.io);
            if io.peer != from {
                info!("peer {} moved {} -> {}", conn.remote_name, io.peer, from);
                io.peer = from;
                if io.connected && !self.connected {
                    // The per-session socket is tied to the old address.
                    io.socket = Arc::clone(&self.socket);
                    io.connected = false;
                }
            }
        }
        {
            let mut st = lock(&conn.state);
            st.rx_bytes_window += pkt.len() as u64;
            st.totals.rx_packets += 1;
        }
        conn.handle_payload(&body[..body_len], now);
    }

    fn new_index(&self) -> u32 {
        let sessions = lock(&self.sessions);
        loop {
            let i = crate::rand::u32();
            if i != 0 && !sessions.contains_key(&i) {
                return i;
            }
        }
    }

    fn on_handshake_init(self: &Arc<Self>, pkt: &[u8], from: SocketAddr) -> Result<(), NetError> {
        if lock(&self.accept_tx).is_none() || pkt.len() < 5 + crate::crypto::MAC1_LEN {
            return Ok(()); // not a host
        }
        // Cheap checks first: mac1 (only paired clients know our public key),
        // retransmit cache, then a global rate limit before any X25519 work.
        let (body, mac) = pkt.split_at(pkt.len() - crate::crypto::MAC1_LEN);
        if !crate::crypto::ct_eq(&crate::crypto::mac1(&self.mac1_key, body), mac) {
            return Ok(());
        }
        let msg1_hash = crate::crypto::sha256(&[body]);
        if let Some((_, resp)) = lock(&self.recent_handshakes).iter().find(|(h, _)| *h == msg1_hash) {
            let _ = self.socket.send_to(resp, from);
            return Ok(());
        }
        if !lock(&self.handshake_bucket).take(clock::now_us()) {
            debug!("handshake rate limit hit ({from})");
            return Ok(());
        }
        let sender_index = u32::from_le_bytes(body[1..5].try_into().unwrap());
        let msg1 = &body[5..];
        let (resp, rs, payload) = IkResponder::read_message1(&self.identity, IK_PROLOGUE, msg1)?;

        let Some(peer_name) = lock(&self.trusted).get(&rs).cloned() else {
            let now = clock::now_us();
            if now - self.last_untrusted_warn.load(Ordering::Relaxed) > 5_000_000 {
                self.last_untrusted_warn.store(now, Ordering::Relaxed);
                warn!(
                    "connection attempt from unpaired device {} at {from}; pair it first (sard host --pair)",
                    crate::crypto::fingerprint(&rs)
                );
            }
            return Ok(());
        };
        let mut hello = HelloPayload::decode(&payload).map_err(|_| NetError::Rejected("bad hello".into()))?;
        hello.name = crate::config::clean_display(&hello.name);
        if hello.version != PROTOCOL_VERSION {
            warn!("{peer_name} speaks protocol v{} (we speak v{PROTOCOL_VERSION})", hello.version);
            return Ok(());
        }

        // Replay protection + idempotent retransmits.
        {
            let cache = lock(&self.hs_cache);
            if let Some(c) = cache.get(&rs) {
                if c.msg1_hash == msg1_hash {
                    let _ = self.socket.send_to(&c.response, from);
                    return Ok(());
                }
                if hello.timestamp <= c.timestamp {
                    debug!("stale handshake from {peer_name}");
                    return Ok(());
                }
            }
        }

        let local_index = self.new_index();
        let reply =
            HelloPayload { version: PROTOCOL_VERSION, timestamp: clock::unix_ns(), name: self.cfg.name.clone() };
        let (msg2, out) = resp.write_message2(&reply.encode())?;
        let mut response = Vec::with_capacity(9 + msg2.len());
        response.put_u8(packet::HANDSHAKE_RESP);
        response.put_u32(local_index);
        response.put_u32(sender_index);
        response.put_bytes(&msg2);

        // Replace any previous session with the same peer.
        {
            let mut sessions = lock(&self.sessions);
            let old: Vec<u32> = sessions.iter().filter(|(_, c)| c.remote_static == rs).map(|(k, _)| *k).collect();
            for k in old {
                if let Some(c) = sessions.remove(&k) {
                    // Tell the old endpoint (if it is still alive) before dropping it.
                    c.send_payload(&[msg::DISCONNECT]);
                    if !c.closed.swap(true, Ordering::Relaxed) {
                        let _ = c.events_tx.try_send(Event::Disconnected("replaced by new connection".into()));
                    }
                }
            }
        }
        let display = if hello.name.is_empty() { peer_name } else { hello.name.clone() };
        let session_socket = self.session_socket(from);
        let io = match &session_socket {
            Some(s) => ConnIo { socket: Arc::clone(s), peer: from, connected: true },
            None => ConnIo { socket: Arc::clone(&self.socket), peer: from, connected: false },
        };
        let conn = Conn::new(io, local_index, sender_index, rs, display, out.send, out.recv, &self.cfg);
        lock(&self.sessions).insert(local_index, Arc::clone(&conn));
        if let Some(s) = session_socket {
            self.spawn_session_reader(s, Arc::clone(&conn));
        }
        let _ = self.socket.send_to(&response, from);
        {
            let mut recent = lock(&self.recent_handshakes);
            recent.push_back((msg1_hash, response.clone()));
            if recent.len() > 32 {
                recent.pop_front();
            }
        }
        lock(&self.hs_cache).insert(rs, HandshakeCache { timestamp: hello.timestamp, msg1_hash, response });
        info!("{} connected from {from}", conn.remote_name);
        if let Some(tx) = lock(&self.accept_tx).as_ref() {
            let _ = tx.send(Connection { conn, endpoint: Arc::clone(self), _guard: None });
        }
        Ok(())
    }

    fn on_handshake_resp(&self, pkt: &[u8], from: SocketAddr) {
        if pkt.len() < 9 {
            return;
        }
        let sender_index = u32::from_le_bytes(pkt[1..5].try_into().unwrap());
        let receiver_index = u32::from_le_bytes(pkt[5..9].try_into().unwrap());
        let mut pending = lock(&self.pending);
        let Some(p) = pending.as_mut() else { return };
        if p.local_index != receiver_index {
            return;
        }
        let Some(ini) = p.initiator.as_ref() else {
            return;
        };
        // Authenticate on a copy: a forged/garbled response (the receiver index
        // is visible on the wire) must not consume the handshake state.
        let (out, payload) = match ini.clone().read_message2(&pkt[9..]) {
            Ok(v) => v,
            Err(e) => {
                debug!("ignoring invalid handshake response from {from}: {e}");
                return;
            }
        };
        p.initiator = None;
        let mut hello =
            HelloPayload::decode(&payload).unwrap_or(HelloPayload { version: 0, timestamp: 0, name: String::new() });
        hello.name = crate::config::clean_display(&hello.name);
        let result = if hello.version != PROTOCOL_VERSION {
            Err(NetError::Rejected(format!("host speaks protocol v{}", hello.version)))
        } else {
            let conn = Conn::new(
                ConnIo { socket: Arc::clone(&self.socket), peer: from, connected: self.connected },
                receiver_index,
                sender_index,
                out.remote_static,
                hello.name,
                out.send,
                out.recv,
                &self.cfg,
            );
            lock(&self.sessions).insert(receiver_index, Arc::clone(&conn));
            Ok(conn)
        };
        let _ = p.result.send(result);
        *pending = None;
    }

    fn on_pair_init(&self, pkt: &[u8], from: SocketAddr) -> Result<(), NetError> {
        if pkt.len() < 5 {
            return Ok(());
        }
        let hash = crate::crypto::sha256(&[pkt]);
        if let Some((h, resp)) = lock(&self.pair_cache).as_ref()
            && *h == hash
        {
            let _ = self.socket.send_to(resp, from); // retransmit of the request we answered
            return Ok(());
        }
        let now = clock::now_us();
        let Some(window) = *lock(&self.pairing) else {
            debug!("pairing request from {from} but pairing is not enabled");
            return Ok(());
        };
        if now > window.expires_us {
            *lock(&self.pairing) = None;
            info!("pairing code expired; restart with `sard host --pair` for a new one");
            return Ok(());
        }
        if !lock(&self.handshake_bucket).take(now) {
            return Ok(());
        }
        let sender_index = u32::from_le_bytes(pkt[1..5].try_into().unwrap());
        let (resp, payload) = match PairResponder::read_message1(&window.psk, PAIR_PROLOGUE, &pkt[5..]) {
            Ok(v) => v,
            Err(e) => {
                if now.saturating_sub(self.last_pair_warn.load(Ordering::Relaxed)) > 5_000_000 {
                    self.last_pair_warn.store(now, Ordering::Relaxed);
                    warn!("pairing attempt from {from} failed ({e}): wrong code?");
                }
                return Ok(());
            }
        };
        let mut pp = PairPayload::decode(&payload).map_err(|_| NetError::Rejected("bad pairing payload".into()))?;
        pp.name = crate::config::clean_display(&pp.name);
        let reply =
            PairPayload { version: PROTOCOL_VERSION, static_key: self.identity.public(), name: self.cfg.name.clone() };
        let msg2 = resp.write_message2(&reply.encode())?;
        let mut out = Vec::new();
        out.put_u8(packet::PAIR_RESP);
        out.put_u32(0);
        out.put_u32(sender_index);
        out.put_bytes(&msg2);
        // Single use: the code is spent once a device has paired with it.
        *lock(&self.pairing) = None;
        *lock(&self.pair_cache) = Some((hash, out.clone()));
        let newly = lock(&self.trusted).insert(pp.static_key, pp.name.clone()).is_none();
        if newly {
            info!(
                "paired with {} ({}) at {from}; the pairing code is now used up",
                pp.name,
                crate::crypto::fingerprint(&pp.static_key)
            );
            if let Some(f) = lock(&self.on_paired).as_ref() {
                f(pp.static_key, &pp.name);
            }
        }
        let _ = self.socket.send_to(&out, from);
        Ok(())
    }
}

fn raise_thread_priority() {
    #[cfg(target_os = "macos")]
    // SAFETY: plain libc call on the current thread.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
    #[cfg(target_os = "linux")]
    // SAFETY: plain libc call; failure (no permission) is harmless.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, -10);
    }
}

/// Host side: listens for paired clients.
pub struct Host {
    shared: Arc<Shared>,
    accept_rx: Receiver<Connection>,
}

pub struct HostOptions {
    /// Trusted clients (static key → name).
    pub trusted: HashMap<[u8; 32], String>,
    /// Pairing code to accept (normalized), if pairing is enabled.
    pub pairing_code: Option<String>,
    /// Called when a new client pairs (persist it).
    pub on_paired: Option<PairedFn>,
}

impl Host {
    pub fn bind(addr: SocketAddr, identity: Keypair, cfg: NetConfig, opts: HostOptions) -> io::Result<Host> {
        #[cfg(target_os = "macos")]
        let socket = bind_reusable(addr)?;
        #[cfg(not(target_os = "macos"))]
        let socket = UdpSocket::bind(addr)?;
        let shared = Shared::new(socket, identity, cfg, false)?;
        *lock(&shared.trusted) = opts.trusted;
        *lock(&shared.pairing) = opts.pairing_code.as_deref().map(|c| PairingWindow {
            psk: crate::crypto::pairing_psk(c),
            expires_us: clock::now_us() + PAIRING_TTL.as_micros() as u64,
        });
        *lock(&shared.on_paired) = opts.on_paired;
        let (tx, rx) = mpsc::channel::<Connection>();
        *lock(&shared.accept_tx) = Some(tx);
        shared.spawn();
        Ok(Host { shared, accept_rx: rx })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.shared.socket.local_addr()
    }

    pub fn set_pairing_code(&self, code: Option<&str>) {
        *lock(&self.shared.pairing) = code.map(|c| PairingWindow {
            psk: crate::crypto::pairing_psk(c),
            expires_us: clock::now_us() + PAIRING_TTL.as_micros() as u64,
        });
    }

    /// Wait for the next client connection.
    pub fn accept(&self, timeout: Duration) -> Option<Connection> {
        self.accept_rx.recv_timeout(timeout).ok()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// Client side: connect to (or pair with) a host.
pub struct Client;

fn resolve(addr: &str) -> io::Result<SocketAddr> {
    let with_port = if addr.parse::<SocketAddr>().is_ok()
        || addr.rsplit_once(':').is_some_and(|(h, p)| !h.contains(':') && p.parse::<u16>().is_ok())
        || addr.starts_with('[')
    {
        addr.to_string()
    } else {
        format!("{addr}:{}", crate::DEFAULT_PORT)
    };
    with_port
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("cannot resolve {addr}")))
}

/// Bind with SO_REUSEADDR + SO_REUSEPORT so per-session connected sockets can
/// share the listening port (incoming datagrams prefer the connected socket).
#[cfg(target_os = "macos")]
fn bind_reusable(addr: SocketAddr) -> io::Result<UdpSocket> {
    use std::os::fd::FromRawFd;
    let domain = if addr.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
    // SAFETY: plain socket syscalls; the fd is owned by the UdpSocket immediately.
    unsafe {
        let fd = libc::socket(domain, libc::SOCK_DGRAM, libc::IPPROTO_UDP);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let sock = UdpSocket::from_raw_fd(fd);
        let one: libc::c_int = 1;
        for opt in [libc::SO_REUSEADDR, libc::SO_REUSEPORT] {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                (&one as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        let mut ss: libc::sockaddr_storage = std::mem::zeroed();
        let len = match addr {
            SocketAddr::V4(a) => {
                let sin = &mut *(&mut ss as *mut libc::sockaddr_storage as *mut libc::sockaddr_in);
                sin.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
                sin.sin_family = libc::AF_INET as libc::sa_family_t;
                sin.sin_port = a.port().to_be();
                sin.sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());
                std::mem::size_of::<libc::sockaddr_in>()
            }
            SocketAddr::V6(a) => {
                let sin6 = &mut *(&mut ss as *mut libc::sockaddr_storage as *mut libc::sockaddr_in6);
                sin6.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
                sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                sin6.sin6_port = a.port().to_be();
                sin6.sin6_addr.s6_addr = a.ip().octets();
                sin6.sin6_flowinfo = a.flowinfo();
                sin6.sin6_scope_id = a.scope_id();
                std::mem::size_of::<libc::sockaddr_in6>()
            }
        };
        if libc::bind(fd, (&ss as *const libc::sockaddr_storage).cast(), len as libc::socklen_t) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(sock)
    }
}

fn bind_for(peer: &SocketAddr) -> io::Result<UdpSocket> {
    if peer.is_ipv6() { UdpSocket::bind("[::]:0") } else { UdpSocket::bind("0.0.0.0:0") }
}

impl Client {
    /// Connect to a paired host. Returns an established connection.
    pub fn connect(
        addr: &str,
        host_key: [u8; 32],
        identity: Keypair,
        cfg: NetConfig,
        timeout: Duration,
    ) -> Result<Connection, NetError> {
        let peer = resolve(addr)?;
        let socket = bind_for(&peer)?;
        // A connected socket: cheaper per packet everywhere, and on macOS it
        // avoids a per-datagram policy check that otherwise throttles UDP.
        socket.connect(peer)?;
        let shared = Shared::new(socket, identity.clone(), cfg.clone(), true)?;
        let local_index = shared.new_index();
        let mut ini = IkInitiator::new(&identity, host_key, IK_PROLOGUE);
        let hello = HelloPayload { version: PROTOCOL_VERSION, timestamp: clock::unix_ns(), name: cfg.name.clone() };
        let msg1 = ini.write_message1(&hello.encode())?;
        let mut pkt = Vec::with_capacity(5 + msg1.len());
        pkt.put_u8(packet::HANDSHAKE_INIT);
        pkt.put_u32(local_index);
        pkt.put_bytes(&msg1);
        let mac = crate::crypto::mac1(&crate::crypto::mac1_key(&host_key), &pkt);
        pkt.put_bytes(&mac);
        let (tx, rx) = mpsc::channel();
        *lock(&shared.pending) = Some(PendingConnect { local_index, initiator: Some(ini), result: tx });
        shared.spawn();

        let deadline = Instant::now() + timeout;
        loop {
            if let Err(e) = shared.socket.send(&pkt) {
                shared.stop.store(true, Ordering::Relaxed);
                return Err(e.into());
            }
            match rx.recv_timeout(Duration::from_millis(300)) {
                Ok(Ok(conn)) => {
                    info!("connected to {} at {peer}", conn.remote_name);
                    let guard = Some(Arc::new(StopGuard(Arc::clone(&shared))));
                    return Ok(Connection { conn, endpoint: shared, _guard: guard });
                }
                Ok(Err(e)) => {
                    shared.stop.store(true, Ordering::Relaxed);
                    return Err(e);
                }
                Err(_) if Instant::now() < deadline => continue,
                Err(_) => {
                    shared.stop.store(true, Ordering::Relaxed);
                    return Err(NetError::Timeout);
                }
            }
        }
    }

    /// Pair with a host using the code it displays. Returns the host's static
    /// key and name; persist them for future `connect` calls.
    pub fn pair(
        addr: &str,
        code: &str,
        identity: &Keypair,
        cfg: &NetConfig,
        timeout: Duration,
    ) -> Result<([u8; 32], String, SocketAddr), NetError> {
        let normalized = crate::crypto::normalize_pairing_code(code)
            .ok_or_else(|| NetError::Rejected("invalid pairing code format".into()))?;
        let psk = crate::crypto::pairing_psk(&normalized);
        let peer = resolve(addr)?;
        let socket = bind_for(&peer)?;
        socket.connect(peer)?;
        socket.set_read_timeout(Some(Duration::from_millis(300)))?;
        let local_index = crate::rand::u32();
        let mut ini = PairInitiator::new(&psk, PAIR_PROLOGUE);
        let payload = PairPayload { version: PROTOCOL_VERSION, static_key: identity.public(), name: cfg.name.clone() };
        let msg1 = ini.write_message1(&payload.encode());
        let mut pkt = Vec::new();
        pkt.put_u8(packet::PAIR_INIT);
        pkt.put_u32(local_index);
        pkt.put_bytes(&msg1);
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 2048];
        while Instant::now() < deadline {
            socket.send(&pkt)?;
            let until = Instant::now() + Duration::from_millis(300);
            while Instant::now() < until {
                match socket.recv_from(&mut buf) {
                    Ok((n, from)) if n > 9 && buf[0] == packet::PAIR_RESP => {
                        let ri = u32::from_le_bytes(buf[5..9].try_into().unwrap());
                        if ri != local_index {
                            continue;
                        }
                        // Authenticate on a copy so a forged reply can't end pairing.
                        let Ok(reply) = ini.clone().read_message2(&buf[9..n]) else { continue };
                        let mut hp = PairPayload::decode(&reply).map_err(|_| NetError::Rejected("bad reply".into()))?;
                        hp.name = crate::config::clean_display(&hp.name);
                        return Ok((hp.static_key, hp.name, from));
                    }
                    Ok(_) => {}
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                    Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Err(NetError::Rejected(
            "no valid reply: check the code and that the host runs `sard host --pair` \
             (codes are single-use and expire after 10 minutes)"
                .into(),
        ))
    }
}
