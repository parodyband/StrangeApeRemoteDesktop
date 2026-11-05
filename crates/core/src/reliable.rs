//! Reliable, ordered message channel over the unreliable datagram transport.
//!
//! Used for input events (key/button/scroll must never be lost or reordered),
//! stream configuration, cursor shapes and clipboard. Messages larger than
//! one segment are fragmented. The receiver acks with a cumulative sequence
//! plus a 64-bit selective-ack bitmap; the sender retransmits on timeout.
//! Traffic on this channel is tiny compared to video, so simplicity wins.

use crate::proto::msg;
use crate::wire::{Put, Reader, Result};
use std::collections::{BTreeMap, VecDeque};

const FIRST: u8 = 1;
const LAST: u8 = 2;

/// Largest message accepted for reassembly (cursor images, clipboard).
pub const MAX_MESSAGE: usize = 16 << 20;

struct Segment {
    seq: u32,
    payload: Vec<u8>,
    sent_us: u64,
    retries: u32,
}

pub struct ReliableChannel {
    next_seq: u32,
    unacked: VecDeque<Segment>,
    next_expected: u32,
    out_of_order: BTreeMap<u32, (u8, Vec<u8>)>,
    partial: Vec<u8>,
    in_message: bool,
    max_segment: usize,
    ack_pending: bool,
    pub retransmits: u64,
    /// Malformed segments received (dropped).
    pub violations: u64,
}

impl ReliableChannel {
    /// `max_payload`: largest plaintext payload per packet.
    pub fn new(max_payload: usize) -> Self {
        Self {
            next_seq: 0,
            unacked: VecDeque::new(),
            next_expected: 0,
            out_of_order: BTreeMap::new(),
            partial: Vec::new(),
            in_message: false,
            max_segment: max_payload - 1 - 4 - 1,
            ack_pending: false,
            retransmits: 0,
            violations: 0,
        }
    }

    /// Queue a message; returns the payloads to transmit now.
    pub fn send(&mut self, message: &[u8], now: u64) -> Vec<Vec<u8>> {
        let chunks: Vec<&[u8]> =
            if message.is_empty() { vec![&[][..]] } else { message.chunks(self.max_segment).collect() };
        let n = chunks.len();
        let mut out = Vec::with_capacity(n);
        for (i, c) in chunks.into_iter().enumerate() {
            let mut flags = 0;
            if i == 0 {
                flags |= FIRST;
            }
            if i == n - 1 {
                flags |= LAST;
            }
            let mut p = Vec::with_capacity(6 + c.len());
            p.put_u8(msg::RELIABLE);
            p.put_u32(self.next_seq);
            p.put_u8(flags);
            p.put_bytes(c);
            self.unacked.push_back(Segment { seq: self.next_seq, payload: p.clone(), sent_us: now, retries: 0 });
            self.next_seq = self.next_seq.wrapping_add(1);
            out.push(p);
        }
        out
    }

    pub fn unacked_len(&self) -> usize {
        self.unacked.len()
    }

    /// Handle a RELIABLE payload (after type byte). Returns complete messages.
    pub fn on_segment(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut r = Reader::new(payload);
        let seq = r.u32()?;
        let flags = r.u8()?;
        let data = r.rest();
        self.ack_pending = true;
        let diff = seq.wrapping_sub(self.next_expected) as i32;
        if diff < 0 {
            return Ok(Vec::new()); // duplicate
        }
        if diff > 0 {
            if diff < 4_096 {
                self.out_of_order.entry(seq).or_insert_with(|| (flags, data.to_vec()));
            }
            return Ok(Vec::new());
        }
        // A malformed segment only discards the message it belongs to; the
        // drain continues so the channel never stalls on buffered segments.
        let mut done = Vec::new();
        let mut ok = self.accept(flags, data, &mut done);
        while let Some((f, d)) = self.out_of_order.remove(&self.next_expected) {
            ok &= self.accept(f, &d, &mut done);
        }
        if !ok {
            self.violations += 1;
        }
        Ok(done)
    }

    /// Consume the segment at `next_expected`. Returns false if it was malformed.
    fn accept(&mut self, flags: u8, data: &[u8], done: &mut Vec<Vec<u8>>) -> bool {
        self.next_expected = self.next_expected.wrapping_add(1);
        if flags & FIRST != 0 {
            self.partial.clear();
            self.in_message = true;
        }
        if !self.in_message {
            return false; // continuation without a start: drop
        }
        if self.partial.len() + data.len() > MAX_MESSAGE {
            self.in_message = false;
            self.partial.clear();
            return false;
        }
        self.partial.extend_from_slice(data);
        if flags & LAST != 0 {
            self.in_message = false;
            done.push(std::mem::take(&mut self.partial));
        }
        true
    }

    /// Build an ACK payload if anything was received since the last one.
    pub fn take_ack(&mut self) -> Option<Vec<u8>> {
        if !self.ack_pending {
            return None;
        }
        self.ack_pending = false;
        let mut sack = 0u64;
        for i in 0..64u32 {
            if self.out_of_order.contains_key(&self.next_expected.wrapping_add(1 + i)) {
                sack |= 1 << i;
            }
        }
        let mut p = Vec::with_capacity(13);
        p.put_u8(msg::ACK);
        p.put_u32(self.next_expected);
        p.put_u64(sack);
        Some(p)
    }

    /// Handle an ACK payload (after type byte). Returns an RTT sample (µs) if
    /// a never-retransmitted segment was newly acknowledged.
    pub fn on_ack(&mut self, payload: &[u8], now: u64) -> Result<Option<u64>> {
        let mut r = Reader::new(payload);
        let cum = r.u32()?;
        let sack = r.u64()?;
        let mut rtt = None;
        self.unacked.retain(|s| {
            let d = cum.wrapping_sub(s.seq) as i32;
            let acked = d > 0 || {
                let off = s.seq.wrapping_sub(cum) as i64 - 1;
                (0..64).contains(&off) && sack & (1 << off) != 0
            };
            if acked && s.retries == 0 {
                rtt = Some(now.saturating_sub(s.sent_us));
            }
            !acked
        });
        Ok(rtt)
    }

    /// Retransmit segments older than `rto_us`.
    pub fn poll(&mut self, now: u64, rto_us: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for s in self.unacked.iter_mut() {
            let backoff = rto_us << s.retries.min(5);
            if now.saturating_sub(s.sent_us) >= backoff {
                s.sent_us = now;
                s.retries += 1;
                self.retransmits += 1;
                out.push(s.payload.clone());
                if out.len() >= 64 {
                    break;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rand::XorShift;

    #[test]
    fn ordered_delivery_with_loss_and_reorder() {
        let mut a = ReliableChannel::new(100);
        let mut b = ReliableChannel::new(100);
        let mut rng = XorShift(99);
        let msgs: Vec<Vec<u8>> = (0..200).map(|i| vec![i as u8; (i * 7) % 300]).collect();
        let mut in_flight: Vec<Vec<u8>> = Vec::new();
        let mut got = Vec::new();
        let mut now = 0u64;
        for m in &msgs {
            in_flight.extend(a.send(m, now));
        }
        for _ in 0..2000 {
            now += 1000;
            // deliver a random subset in random order, drop 20%
            let mut keep = Vec::new();
            while let Some(p) = in_flight.pop() {
                if rng.chance(0.2) {
                    continue;
                }
                if rng.chance(0.3) {
                    keep.push(p);
                    continue;
                }
                assert_eq!(p[0], msg::RELIABLE);
                got.extend(b.on_segment(&p[1..]).unwrap());
            }
            in_flight = keep;
            if let Some(ack) = b.take_ack()
                && !rng.chance(0.2)
            {
                a.on_ack(&ack[1..], now).unwrap();
            }
            in_flight.extend(a.poll(now, 5000));
            if got.len() == msgs.len() && a.unacked_len() == 0 {
                break;
            }
        }
        assert_eq!(got, msgs);
        assert_eq!(a.unacked_len(), 0);
    }

    /// A bad segment must not stall later messages (review finding #8).
    #[test]
    fn bad_segment_does_not_stall() {
        let mut b = ReliableChannel::new(1200);
        let seg = |seq: u32, flags: u8, data: &[u8]| {
            let mut p = Vec::new();
            p.put_u32(seq);
            p.put_u8(flags);
            p.put_bytes(data);
            p
        };
        // seq 1 (complete message) arrives early and is buffered.
        assert!(b.on_segment(&seg(1, FIRST | LAST, b"good")).unwrap().is_empty());
        // seq 0 is a continuation without a start: malformed.
        let got = b.on_segment(&seg(0, 0, b"junk")).unwrap();
        assert_eq!(got, vec![b"good".to_vec()]);
        assert_eq!(b.violations, 1);
        assert_eq!(b.on_segment(&seg(2, FIRST | LAST, b"next")).unwrap(), vec![b"next".to_vec()]);
    }

    #[test]
    fn empty_and_large_messages() {
        let mut a = ReliableChannel::new(1200);
        let mut b = ReliableChannel::new(1200);
        let big: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        let mut got = Vec::new();
        for p in a.send(&[], 0).into_iter().chain(a.send(&big, 0)) {
            got.extend(b.on_segment(&p[1..]).unwrap());
        }
        assert_eq!(got, vec![Vec::new(), big]);
    }
}
