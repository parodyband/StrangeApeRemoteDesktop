//! Video frame packetization, FEC and reassembly.
//!
//! A frame (a [`FrameMeta`] header followed by the encoded bitstream) is cut
//! into equal-size shards, grouped into FEC blocks of at most
//! `max_block_data` data shards, and each block gets `m` Cauchy RS parity
//! shards. Shards of multi-block frames are interleaved so a burst loss is
//! spread across blocks. The receiver reassembles blocks, repairs with FEC,
//! NACKs what FEC could not repair, and delivers frames strictly in order so
//! the decoder never sees a frame whose reference is missing. When a frame is
//! given up on, everything up to the next keyframe/recovery frame is dropped
//! and a keyframe is requested.

use crate::fec;
use crate::proto::{Codec, msg};
use crate::wire::{Put, Reader, Result, WireError};
use std::collections::{BTreeMap, VecDeque};

pub const SHARD_HEADER_LEN: usize = 16;
pub const FRAME_META_LEN: usize = 24;

pub mod frame_flags {
    /// IDR / key frame: decodable on its own.
    pub const KEYFRAME: u8 = 1 << 0;
    /// Recovery frame: references only frames the receiver acknowledged.
    pub const RECOVERY: u8 = 1 << 1;
    /// (shard flag) This shard is a retransmission.
    pub const RETRANSMIT: u8 = 1 << 7;
}

/// Per-frame header that travels inside the frame payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameMeta {
    pub codec: Codec,
    pub flags: u8,
    pub width: u16,
    pub height: u16,
    /// Host monotonic µs when the frame was captured.
    pub capture_us: u64,
    /// Host monotonic µs when encoding finished and sending began.
    pub send_us: u64,
}

impl FrameMeta {
    pub fn write(&self, b: &mut Vec<u8>) {
        b.put_u8(1); // version
        b.put_u8(self.codec as u8);
        b.put_u8(self.flags);
        b.put_u8(0);
        b.put_u16(self.width);
        b.put_u16(self.height);
        b.put_u64(self.capture_us);
        b.put_u64(self.send_us);
    }

    pub fn read(buf: &[u8]) -> Result<(Self, &[u8])> {
        let mut r = Reader::new(buf);
        if r.u8()? != 1 {
            return Err(WireError);
        }
        let codec = Codec::from_u8(r.u8()?).ok_or(WireError)?;
        let flags = r.u8()?;
        r.u8()?;
        let meta =
            FrameMeta { codec, flags, width: r.u16()?, height: r.u16()?, capture_us: r.u64()?, send_us: r.u64()? };
        Ok((meta, r.rest()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShardHeader {
    pub frame_id: u32,
    pub block: u8,
    pub block_count: u8,
    pub index: u8,
    pub data_shards: u8,
    pub parity_shards: u8,
    pub flags: u8,
    pub shard_size: u16,
    pub frame_len: u32,
}

impl ShardHeader {
    pub fn write(&self, b: &mut Vec<u8>) {
        b.put_u32(self.frame_id);
        b.put_u8(self.block);
        b.put_u8(self.block_count);
        b.put_u8(self.index);
        b.put_u8(self.data_shards);
        b.put_u8(self.parity_shards);
        b.put_u8(self.flags);
        b.put_u16(self.shard_size);
        b.put_u32(self.frame_len);
    }

    pub fn read(r: &mut Reader) -> Result<Self> {
        let h = ShardHeader {
            frame_id: r.u32()?,
            block: r.u8()?,
            block_count: r.u8()?,
            index: r.u8()?,
            data_shards: r.u8()?,
            parity_shards: r.u8()?,
            flags: r.u8()?,
            shard_size: r.u16()?,
            frame_len: r.u32()?,
        };
        if h.block_count == 0
            || h.block >= h.block_count
            || h.data_shards == 0
            || h.shard_size == 0
            || (h.index as usize) >= h.data_shards as usize + h.parity_shards as usize
        {
            return Err(WireError);
        }
        Ok(h)
    }
}

/// FEC parameters chosen by the sender.
#[derive(Clone, Copy, Debug)]
pub struct FecPolicy {
    /// Parity shards as a fraction of data shards (0.0 disables FEC).
    pub ratio: f32,
    /// Minimum parity shards per block when `ratio > 0`.
    pub min_parity: usize,
    /// Max data shards per block.
    pub max_block_data: usize,
}

impl Default for FecPolicy {
    fn default() -> Self {
        Self { ratio: 0.10, min_parity: 1, max_block_data: 64 }
    }
}

impl FecPolicy {
    fn parity_for(&self, k: usize) -> usize {
        if self.ratio <= 0.0 {
            return 0;
        }
        let m = ((k as f32) * self.ratio).ceil() as usize;
        m.max(self.min_parity).min(255 - k)
    }
}

struct SentBlock {
    k: usize,
    m: usize,
    shards: Vec<u8>,
}

struct SentFrame {
    frame_id: u32,
    flags: u8,
    frame_len: u32,
    shard_size: usize,
    blocks: Vec<SentBlock>,
    bytes: usize,
}

/// Sender side: packetizes frames and keeps recent ones for retransmission.
pub struct VideoSender {
    next_frame_id: u32,
    /// Max payload bytes per shard.
    max_shard: usize,
    pub fec: FecPolicy,
    history: VecDeque<SentFrame>,
    history_bytes: usize,
    pub max_history_bytes: usize,
    pub stats_shards_sent: u64,
    pub stats_retransmitted: u64,
}

impl VideoSender {
    /// `max_payload` is the largest decrypted payload a packet may carry.
    pub fn new(max_payload: usize) -> Self {
        let max_shard = max_payload - 1 - SHARD_HEADER_LEN;
        Self {
            next_frame_id: 0,
            max_shard,
            fec: FecPolicy::default(),
            history: VecDeque::new(),
            history_bytes: 0,
            max_history_bytes: 32 << 20,
            stats_shards_sent: 0,
            stats_retransmitted: 0,
        }
    }

    pub fn next_frame_id(&self) -> u32 {
        self.next_frame_id
    }

    /// Packetize `frame` (meta + bitstream). `emit` is called once per packet
    /// with the plaintext message payload, in send order.
    pub fn send_frame(&mut self, frame: &[u8], flags: u8, mut emit: impl FnMut(&[u8])) -> u32 {
        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);

        let len = frame.len().max(1);
        let n_total = len.div_ceil(self.max_shard);
        // Even shard size minimizes padding for small frames.
        let shard_size = len.div_ceil(n_total);
        let n_blocks = n_total.div_ceil(self.fec.max_block_data).min(255);
        let base = n_total / n_blocks;
        let extra = n_total % n_blocks;

        let mut blocks = Vec::with_capacity(n_blocks);
        let mut shard_off = 0usize;
        for b in 0..n_blocks {
            let k = base + usize::from(b < extra);
            let m = self.fec.parity_for(k);
            let mut shards = vec![0u8; (k + m) * shard_size];
            let start = shard_off * shard_size;
            let end = ((shard_off + k) * shard_size).min(frame.len());
            if start < end {
                shards[..end - start].copy_from_slice(&frame[start..end]);
            }
            if m > 0 {
                fec::encode(k, m, shard_size, &mut shards);
            }
            blocks.push(SentBlock { k, m, shards });
            shard_off += k;
        }

        let sent = SentFrame {
            frame_id,
            flags,
            frame_len: frame.len() as u32,
            shard_size,
            bytes: blocks.iter().map(|b| b.shards.len()).sum(),
            blocks,
        };

        // Interleave: round-robin over blocks by shard index.
        let max_shards = sent.blocks.iter().map(|b| b.k + b.m).max().unwrap_or(0);
        let mut buf = Vec::with_capacity(1 + SHARD_HEADER_LEN + shard_size);
        for idx in 0..max_shards {
            for b in 0..sent.blocks.len() {
                if idx < sent.blocks[b].k + sent.blocks[b].m {
                    Self::build(&sent, b, idx, 0, &mut buf);
                    emit(&buf);
                    self.stats_shards_sent += 1;
                }
            }
        }

        self.history_bytes += sent.bytes;
        self.history.push_back(sent);
        while self.history_bytes > self.max_history_bytes || self.history.len() > 256 {
            if let Some(f) = self.history.pop_front() {
                self.history_bytes -= f.bytes;
            }
        }
        frame_id
    }

    fn build(f: &SentFrame, block: usize, idx: usize, extra_flags: u8, buf: &mut Vec<u8>) {
        let b = &f.blocks[block];
        buf.clear();
        buf.put_u8(msg::VIDEO);
        ShardHeader {
            frame_id: f.frame_id,
            block: block as u8,
            block_count: f.blocks.len() as u8,
            index: idx as u8,
            data_shards: b.k as u8,
            parity_shards: b.m as u8,
            flags: f.flags | extra_flags,
            shard_size: f.shard_size as u16,
            frame_len: f.frame_len,
        }
        .write(buf);
        buf.extend_from_slice(&b.shards[idx * f.shard_size..(idx + 1) * f.shard_size]);
    }

    /// Handle a NACK payload (after the msg type byte). Emits retransmissions.
    pub fn on_nack(&mut self, payload: &[u8], mut emit: impl FnMut(&[u8])) -> Result<()> {
        let mut r = Reader::new(payload);
        let frame_id = r.u32()?;
        let n = r.u8()?;
        let Some(f) = self.history.iter().find(|f| f.frame_id == frame_id) else {
            return Ok(()); // too old; receiver will give up and request a keyframe
        };
        let mut buf = Vec::new();
        for _ in 0..n {
            let block = r.u8()?;
            let count = r.u8()? as usize;
            let indices = r.bytes(count)?;
            let blocks: Vec<usize> = if block == 0xFF { (0..f.blocks.len()).collect() } else { vec![block as usize] };
            for b in blocks {
                if b >= f.blocks.len() {
                    continue;
                }
                if count == 0 {
                    for idx in 0..f.blocks[b].k {
                        Self::build(f, b, idx, frame_flags::RETRANSMIT, &mut buf);
                        emit(&buf);
                        self.stats_retransmitted += 1;
                    }
                } else {
                    for &idx in indices {
                        if (idx as usize) < f.blocks[b].k + f.blocks[b].m {
                            Self::build(f, b, idx as usize, frame_flags::RETRANSMIT, &mut buf);
                            emit(&buf);
                            self.stats_retransmitted += 1;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Receiver
// ---------------------------------------------------------------------------

struct PartialBlock {
    k: usize,
    m: usize,
    buf: Vec<u8>,
    present: Vec<bool>,
    received: usize,
    done: bool,
}

struct Partial {
    /// Known once any shard arrives.
    frame_len: usize,
    shard_size: usize,
    flags: u8,
    blocks: Vec<Option<PartialBlock>>,
    blocks_done: usize,
    data: Vec<u8>,
    known: bool,
    complete: bool,
    first_us: u64,
    last_us: u64,
    nacks: u32,
    last_nack_us: u64,
    shards_received: u32,
    used_fec: bool,
    used_rtx: bool,
}

impl Partial {
    fn unknown(now: u64) -> Self {
        Partial {
            frame_len: 0,
            shard_size: 0,
            flags: 0,
            blocks: Vec::new(),
            blocks_done: 0,
            data: Vec::new(),
            known: false,
            complete: false,
            first_us: now,
            last_us: now,
            nacks: 0,
            last_nack_us: 0,
            shards_received: 0,
            used_fec: false,
            used_rtx: false,
        }
    }
}

/// A fully reassembled frame (meta + bitstream).
#[derive(Debug)]
pub struct CompleteFrame {
    pub frame_id: u32,
    pub flags: u8,
    pub data: Vec<u8>,
    /// Local µs when the first / last needed shard arrived.
    pub first_us: u64,
    pub complete_us: u64,
}

/// Receiver-side counters (reset by `take_report`).
#[derive(Clone, Copy, Debug, Default)]
pub struct RecvCounters {
    pub packets: u32,
    pub lost_packets: u32,
    pub fec_recovered: u32,
    pub rtx_recovered: u32,
    pub frames_lost: u32,
    pub frames_complete: u32,
    pub assembly_us_total: u64,
}

/// Tunables for loss handling.
#[derive(Clone, Copy, Debug)]
pub struct RecvConfig {
    /// After a newer frame's shards arrive, wait this long before NACKing.
    pub reorder_grace_us: u64,
    /// With no newer frames, NACK an incomplete frame after this much silence.
    pub idle_nack_us: u64,
    pub max_nacks: u32,
    /// Give up on a frame this long after it was first seen.
    pub max_wait_us: u64,
}

impl Default for RecvConfig {
    fn default() -> Self {
        Self { reorder_grace_us: 500, idle_nack_us: 3_000, max_nacks: 4, max_wait_us: 120_000 }
    }
}

pub enum RecvEvent {
    /// Send this NACK payload (full message incl. type byte).
    Nack(Vec<u8>),
    /// Frames were given up on; request a keyframe.
    NeedKeyframe { last_delivered: Option<u32> },
}

pub struct VideoReceiver {
    frames: BTreeMap<u32, Partial>,
    /// Next frame id to deliver; `None` until the first keyframe.
    next_id: Option<u32>,
    newest_seen: Option<u32>,
    /// Set after a loss: only a keyframe/recovery frame can resume decoding.
    awaiting_resync: bool,
    ready: VecDeque<CompleteFrame>,
    pub cfg: RecvConfig,
    pub counters: RecvCounters,
    last_delivered: Option<u32>,
}

impl Default for VideoReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoReceiver {
    pub fn new() -> Self {
        Self {
            frames: BTreeMap::new(),
            next_id: None,
            newest_seen: None,
            awaiting_resync: true,
            ready: VecDeque::new(),
            cfg: RecvConfig::default(),
            counters: RecvCounters::default(),
            last_delivered: None,
        }
    }

    /// Frames ready for the decoder, in order.
    pub fn pop_ready(&mut self) -> Option<CompleteFrame> {
        self.ready.pop_front()
    }

    pub fn is_awaiting_resync(&self) -> bool {
        self.awaiting_resync
    }

    pub fn take_counters(&mut self) -> RecvCounters {
        std::mem::take(&mut self.counters)
    }

    /// Handle one VIDEO message payload (after the type byte).
    pub fn on_shard(&mut self, payload: &[u8], now: u64) -> Result<()> {
        let mut r = Reader::new(payload);
        let h = ShardHeader::read(&mut r)?;
        let data = r.rest();
        if data.len() != h.shard_size as usize {
            return Err(WireError);
        }
        self.counters.packets += 1;
        let id = h.frame_id;
        if let Some(next) = self.next_id
            && id < next
        {
            return Ok(()); // already delivered or dropped
        }
        let resync_shard = h.flags & (frame_flags::KEYFRAME | frame_flags::RECOVERY) != 0;
        // While waiting for a keyframe/recovery frame, frames that follow a
        // resync frame still being assembled must be kept: they become
        // decodable as soon as it completes. Anything else is undecodable.
        let follows_resync =
            self.awaiting_resync && self.frames.range(..id).any(|(_, p)| p.known && Self::is_resync(p));
        if self.awaiting_resync && !resync_shard && !follows_resync {
            self.newest_seen = Some(self.newest_seen.map_or(id, |n| n.max(id)));
            return Ok(());
        }

        // Track gaps: frames between newest_seen and this one that we have
        // never heard of get placeholder entries so they can be NACKed whole.
        match self.newest_seen {
            Some(n) if id > n => {
                if !self.awaiting_resync || follows_resync {
                    let from = n + 1;
                    let lo = self.next_id.map_or(from, |x| x.max(from));
                    for missing in lo..id {
                        if self.frames.len() > 512 {
                            break;
                        }
                        self.frames.entry(missing).or_insert_with(|| Partial::unknown(now));
                    }
                }
                self.newest_seen = Some(id);
            }
            None => self.newest_seen = Some(id),
            _ => {}
        }

        let p = self.frames.entry(id).or_insert_with(|| Partial::unknown(now));
        if p.complete {
            return Ok(());
        }
        if !p.known {
            p.known = true;
            p.frame_len = h.frame_len as usize;
            p.shard_size = h.shard_size as usize;
            p.flags = h.flags & !frame_flags::RETRANSMIT;
            p.blocks = (0..h.block_count).map(|_| None).collect();
            let total_data_bytes = (h.frame_len as usize).div_ceil(h.shard_size as usize) * h.shard_size as usize;
            p.data = vec![0u8; total_data_bytes.max(h.shard_size as usize)];
        } else if p.shard_size != h.shard_size as usize || p.blocks.len() != h.block_count as usize {
            return Err(WireError);
        }
        p.last_us = now;
        p.shards_received += 1;
        if h.flags & frame_flags::RETRANSMIT != 0 {
            p.used_rtx = true;
        }

        let s = p.shard_size;
        let blk = p.blocks[h.block as usize].get_or_insert_with(|| {
            let k = h.data_shards as usize;
            let m = h.parity_shards as usize;
            PartialBlock { k, m, buf: vec![0u8; (k + m) * s], present: vec![false; k + m], received: 0, done: false }
        });
        if blk.done || blk.k != h.data_shards as usize || blk.m != h.parity_shards as usize {
            return Ok(());
        }
        let idx = h.index as usize;
        if blk.present[idx] {
            return Ok(());
        }
        blk.present[idx] = true;
        blk.received += 1;
        blk.buf[idx * s..(idx + 1) * s].copy_from_slice(data);

        if blk.received >= blk.k {
            // Data shards that had not arrived when the block became decodable
            // are the real losses (parity still in flight is not a loss).
            let missing_data = (0..blk.k).filter(|&j| !blk.present[j]).count() as u32;
            let needed_fec = missing_data > 0;
            if needed_fec && !fec::reconstruct(blk.k, blk.m, s, &mut blk.buf, &mut blk.present) {
                return Ok(());
            }
            if needed_fec {
                p.used_fec = true;
                self.counters.lost_packets += missing_data;
            }
            blk.done = true;
            let k = blk.k;
            // Block b's data follows all data shards of earlier blocks under the
            // sender's even split (computable even if earlier blocks are unseen).
            let block_start = split_offset(p.frame_len, s, p.blocks.len(), h.block as usize);
            let blk = p.blocks[h.block as usize].as_ref().unwrap();
            p.data[block_start * s..(block_start + k) * s].copy_from_slice(&blk.buf[..k * s]);
            p.blocks_done += 1;
            if p.blocks_done == p.blocks.len() {
                p.complete = true;
                p.data.truncate(p.frame_len);
                self.counters.frames_complete += 1;
                if p.used_fec {
                    self.counters.fec_recovered += 1;
                }
                if p.used_rtx {
                    self.counters.rtx_recovered += 1;
                }
                self.counters.assembly_us_total += now - p.first_us;
                // Free FEC buffers early.
                for b in p.blocks.iter_mut().flatten() {
                    b.buf = Vec::new();
                }
                self.try_deliver(now);
            }
        }
        Ok(())
    }

    fn is_resync(p: &Partial) -> bool {
        p.flags & (frame_flags::KEYFRAME | frame_flags::RECOVERY) != 0
    }

    fn try_deliver(&mut self, now: u64) {
        loop {
            // A complete keyframe/recovery frame ahead of a stuck position (or
            // while awaiting resync) lets us jump forward.
            let jump = self
                .frames
                .iter()
                .find(|(id, p)| p.complete && Self::is_resync(p) && self.next_id.is_none_or(|n| **id >= n))
                .map(|(id, _)| *id);
            let next = match self.next_id {
                Some(n) if !self.awaiting_resync => n,
                _ => match jump {
                    Some(k) => {
                        self.drop_before(k);
                        self.awaiting_resync = false;
                        k
                    }
                    None => return,
                },
            };
            let next_complete = self.frames.get(&next).is_some_and(|p| p.complete);
            if next_complete {
                let p = self.frames.remove(&next).unwrap();
                self.ready.push_back(CompleteFrame {
                    frame_id: next,
                    flags: p.flags,
                    data: p.data,
                    first_us: p.first_us,
                    complete_us: p.last_us,
                });
                self.last_delivered = Some(next);
                self.next_id = Some(next + 1);
                continue;
            }
            self.next_id = Some(next);
            // Stuck on `next`; jump if a later resync frame is already complete.
            if let Some(k) = jump
                && k > next
            {
                let lost = self.frames.range(next..k).filter(|(_, p)| !p.complete).count() as u32;
                self.counters.frames_lost += lost.max(1);
                self.drop_before(k);
                self.next_id = Some(k);
                continue;
            }
            // Make sure the frame we are waiting for is tracked, so it gets
            // NACKed or eventually given up on (it may have been entirely lost
            // or discarded while we were waiting for a keyframe).
            if self.newest_seen.is_some_and(|n| n >= next) {
                self.frames.entry(next).or_insert_with(|| Partial::unknown(now));
            }
            return;
        }
    }

    fn drop_before(&mut self, id: u32) {
        let keep = self.frames.split_off(&id);
        self.frames = keep;
    }

    /// Periodic work: NACK generation and give-up decisions. Call every ~1 ms.
    pub fn poll(&mut self, now: u64, rtt_us: u64) -> Vec<RecvEvent> {
        let mut out = Vec::new();
        let Some(newest) = self.newest_seen else {
            return out;
        };
        let nack_interval = (rtt_us * 3 / 2).max(2_000);
        let mut give_up = false;
        if self.awaiting_resync {
            // Keep (partial) resync frames and whatever follows the first one;
            // everything older is undecodable.
            let max_wait = self.cfg.max_wait_us;
            let first_resync = self.frames.iter().find(|(_, p)| p.known && Self::is_resync(p)).map(|(id, _)| *id);
            self.frames.retain(|id, p| {
                (p.known && Self::is_resync(p) && now.saturating_sub(p.first_us) < max_wait * 4)
                    || first_resync.is_some_and(|r| *id > r)
            });
        }

        for (&id, p) in self.frames.iter_mut() {
            if p.complete {
                continue;
            }
            if self.next_id.is_some_and(|n| id < n) {
                continue;
            }
            let age = now.saturating_sub(p.first_us);
            let silent = now.saturating_sub(p.last_us);
            if age > self.cfg.max_wait_us || (p.nacks >= self.cfg.max_nacks && now - p.last_nack_us > nack_interval * 2)
            {
                if !self.awaiting_resync && self.next_id.is_none_or(|n| id >= n) {
                    give_up = true;
                }
                continue;
            }
            let newer_seen = newest > id;
            let due = (newer_seen && silent > self.cfg.reorder_grace_us) || silent > self.cfg.idle_nack_us;
            if !due || p.nacks >= self.cfg.max_nacks || (p.nacks > 0 && now - p.last_nack_us < nack_interval) {
                continue;
            }
            let mut m = Vec::with_capacity(32);
            m.put_u8(msg::NACK);
            m.put_u32(id);
            if !p.known {
                m.put_u8(1);
                m.put_u8(0xFF);
                m.put_u8(0);
            } else {
                let count_pos = m.len();
                m.put_u8(0);
                let mut entries = 0u8;
                for (bi, b) in p.blocks.iter().enumerate() {
                    match b {
                        None => {
                            m.put_u8(bi as u8);
                            m.put_u8(0);
                            entries += 1;
                        }
                        Some(b) if !b.done => {
                            let need = b.k.saturating_sub(b.received);
                            let missing: Vec<u8> =
                                (0..b.k).filter(|&j| !b.present[j]).take(need).map(|j| j as u8).collect();
                            if missing.is_empty() {
                                continue;
                            }
                            m.put_u8(bi as u8);
                            m.put_u8(missing.len() as u8);
                            m.put_bytes(&missing);
                            entries += 1;
                        }
                        _ => {}
                    }
                    if m.len() > 1000 || entries == 255 {
                        break;
                    }
                }
                m[count_pos] = entries;
                if entries == 0 {
                    continue;
                }
            }
            p.nacks += 1;
            p.last_nack_us = now;
            out.push(RecvEvent::Nack(m));
            if out.len() >= 8 {
                break;
            }
        }

        if give_up {
            // Drop everything except (partial or complete) resync frames and
            // wait for one to complete. The caller requests a keyframe.
            let lost = self.frames.values().filter(|p| !p.complete).count() as u32;
            self.counters.frames_lost += lost.max(1);
            for p in self.frames.values().filter(|p| !p.complete) {
                for b in p.blocks.iter().flatten().filter(|b| !b.done) {
                    self.counters.lost_packets += (0..b.k).filter(|&j| !b.present[j]).count() as u32;
                }
            }
            self.frames.retain(|_, p| p.known && Self::is_resync(p));
            self.awaiting_resync = true;
            self.next_id = Some(self.frames.keys().next().copied().unwrap_or(newest + 1));
            out.push(RecvEvent::NeedKeyframe { last_delivered: self.last_delivered });
            self.try_deliver(now);
        }
        out
    }
}

/// Data-shard offset of block `b` under the sender's even split.
fn split_offset(frame_len: usize, shard_size: usize, n_blocks: usize, b: usize) -> usize {
    let n_total = frame_len.max(1).div_ceil(shard_size);
    let base = n_total / n_blocks;
    let extra = n_total % n_blocks;
    b * base + b.min(extra)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rand::XorShift;

    fn make_frame(id: u32, len: usize, key: bool) -> Vec<u8> {
        let mut f = Vec::new();
        FrameMeta {
            codec: Codec::Hevc,
            flags: if key { frame_flags::KEYFRAME } else { 0 },
            width: 1920,
            height: 1080,
            capture_us: id as u64,
            send_us: 0,
        }
        .write(&mut f);
        f.extend((0..len).map(|i| (i as u32 ^ id.wrapping_mul(2654435761)) as u8));
        f
    }

    /// Simulated channel: sender → (loss) → receiver, NACKs flow back losslessly.
    fn run(loss: f64, frames: u32, frame_len: impl Fn(u32) -> usize, seed: u64) -> (u32, RecvCounters) {
        let mut rng = XorShift(seed);
        let mut tx = VideoSender::new(1400 - crate::proto::packet::DATA_OVERHEAD);
        tx.fec = FecPolicy { ratio: 0.15, min_parity: 1, max_block_data: 64 };
        let mut rx = VideoReceiver::new();
        let mut now = 1_000u64;
        let mut delivered = 0u32;
        let mut expected: Vec<Vec<u8>> = Vec::new();
        let mut counters = RecvCounters::default();
        for i in 0..frames {
            let key = i == 0 || i % 60 == 0;
            let f = make_frame(i, frame_len(i), key);
            expected.push(f.clone());
            let mut pkts = Vec::new();
            tx.send_frame(&f, if key { frame_flags::KEYFRAME } else { 0 }, |p| pkts.push(p.to_vec()));
            for p in pkts {
                if !rng.chance(loss) {
                    rx.on_shard(&p[1..], now).unwrap();
                }
                now += 10;
            }
            // Let NACK/retransmit cycles run for one frame interval (8 ms).
            for _ in 0..8 {
                now += 1000;
                for ev in rx.poll(now, 1000) {
                    match ev {
                        RecvEvent::Nack(n) => {
                            let mut rtx = Vec::new();
                            tx.on_nack(&n[1..], |p| rtx.push(p.to_vec())).unwrap();
                            for p in rtx {
                                if !rng.chance(loss) {
                                    rx.on_shard(&p[1..], now).unwrap();
                                }
                            }
                        }
                        RecvEvent::NeedKeyframe { .. } => {}
                    }
                }
                while let Some(cf) = rx.pop_ready() {
                    assert_eq!(cf.data, expected[cf.frame_id as usize], "frame {} corrupted", cf.frame_id);
                    delivered += 1;
                }
            }
            let c = rx.take_counters();
            counters.packets += c.packets;
            counters.lost_packets += c.lost_packets;
            counters.fec_recovered += c.fec_recovered;
            counters.rtx_recovered += c.rtx_recovered;
            counters.frames_lost += c.frames_lost;
        }
        (delivered, counters)
    }

    #[test]
    fn lossless_delivers_all_in_order() {
        let (d, c) = run(
            0.0,
            120,
            |i| {
                if i % 60 == 0 { 200_000 } else { 1000 + (i as usize * 997) % 60_000 }
            },
            1,
        );
        assert_eq!(d, 120);
        assert_eq!(c.frames_lost, 0);
        assert_eq!(c.fec_recovered, 0);
        // Parity shards still in flight when a frame completes are not losses.
        assert_eq!(c.lost_packets, 0);
    }

    #[test]
    fn tiny_frames() {
        let (d, _) = run(0.0, 30, |_| 1, 2);
        assert_eq!(d, 30);
    }

    #[test]
    fn moderate_loss_is_fully_repaired() {
        let (d, c) = run(0.03, 240, |i| if i % 60 == 0 { 150_000 } else { 30_000 }, 3);
        assert_eq!(d, 240, "counters {c:?}");
        assert!(c.fec_recovered > 0);
    }

    #[test]
    fn heavy_loss_recovers_via_keyframes() {
        // 20% loss: some frames will be given up; decoding must resume at keyframes
        // and every delivered frame must be intact (checked in run()).
        let (d, c) = run(0.20, 240, |i| if i % 60 == 0 { 100_000 } else { 20_000 }, 4);
        assert!(d > 100, "delivered {d}, {c:?}");
    }

    /// A keyframe that needs a retransmission while the following P-frames
    /// already arrived must not stall the stream (regression).
    #[test]
    fn delayed_keyframe_with_following_frames() {
        let mut tx = VideoSender::new(1371);
        tx.fec = FecPolicy { ratio: 0.0, min_parity: 0, max_block_data: 64 };
        let mut rx = VideoReceiver::new();
        let mut now = 1_000u64;
        let mut sent = Vec::new();
        for i in 0..5u32 {
            let key = i == 0;
            let f = make_frame(i, if key { 20_000 } else { 3_000 }, key);
            let mut pkts = Vec::new();
            tx.send_frame(&f, if key { frame_flags::KEYFRAME } else { 0 }, |p| pkts.push(p.to_vec()));
            for (j, p) in pkts.iter().enumerate() {
                if key && j == 3 {
                    continue; // lose one keyframe shard (no FEC)
                }
                rx.on_shard(&p[1..], now).unwrap();
                now += 10;
            }
            sent.push(f);
        }
        assert!(rx.pop_ready().is_none());
        // NACK -> retransmit completes the keyframe; all frames must follow.
        let mut delivered = Vec::new();
        for _ in 0..20 {
            now += 1_000;
            for ev in rx.poll(now, 500) {
                if let RecvEvent::Nack(n) = ev {
                    let mut rtx = Vec::new();
                    tx.on_nack(&n[1..], |p| rtx.push(p.to_vec())).unwrap();
                    for p in rtx {
                        rx.on_shard(&p[1..], now).unwrap();
                    }
                }
            }
            while let Some(f) = rx.pop_ready() {
                delivered.push(f.frame_id);
            }
        }
        assert_eq!(delivered, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn split_offset_matches_sender() {
        for (len, s, nb) in [(1000usize, 10usize, 3usize), (64 * 1354 * 3 + 5, 1354, 4), (17, 17, 1)] {
            let n_total = len.div_ceil(s);
            let base = n_total / nb;
            let extra = n_total % nb;
            let mut off = 0;
            for b in 0..nb {
                assert_eq!(split_offset(len, s, nb, b), off);
                off += base + usize::from(b < extra);
            }
        }
    }
}
