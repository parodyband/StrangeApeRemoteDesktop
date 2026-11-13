//! Monotonic clocks and cross-machine clock offset estimation.
//!
//! All latency timestamps on the wire are in microseconds of the *sender's*
//! monotonic clock. Linux uses `CLOCK_MONOTONIC` (same clock PipeWire stamps
//! buffers with); macOS uses `CLOCK_UPTIME_RAW` (== `mach_absolute_time`,
//! the CoreMedia host clock); Windows uses `QueryPerformanceCounter` (the
//! clock DXGI and WASAPI stamp frames with). [`ClockSync`] estimates the offset between the
//! two machines from ping/pong exchanges so end-to-end latency can be shown.

use std::collections::VecDeque;

#[cfg(target_os = "linux")]
const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
#[cfg(target_vendor = "apple")]
const CLOCK: libc::clockid_t = libc::CLOCK_UPTIME_RAW;

/// Monotonic time in microseconds.
#[inline]
pub fn now_us() -> u64 {
    now_ns() / 1000
}

/// Monotonic time in nanoseconds.
#[cfg(unix)]
#[inline]
pub fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec; CLOCK is a valid clock id.
    unsafe { libc::clock_gettime(CLOCK, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryPerformanceCounter(count: *mut i64) -> i32;
    fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
}

/// `QueryPerformanceCounter` ticks per second (fixed at boot).
#[cfg(windows)]
pub fn qpc_frequency() -> u64 {
    static FREQ: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: valid out pointer; cannot fail on Windows XP and later.
        unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1) as u64
    })
}

/// Convert a `QueryPerformanceCounter` value (as found in DXGI / WASAPI
/// timestamps) to this module's nanoseconds.
#[cfg(windows)]
pub fn qpc_to_ns(ticks: u64) -> u64 {
    (ticks as u128 * 1_000_000_000 / qpc_frequency() as u128) as u64
}

/// Monotonic time in nanoseconds.
#[cfg(windows)]
#[inline]
pub fn now_ns() -> u64 {
    let mut c = 0i64;
    // SAFETY: valid out pointer.
    unsafe { QueryPerformanceCounter(&mut c) };
    qpc_to_ns(c as u64)
}

/// Wall-clock nanoseconds since the Unix epoch (used only for handshake
/// replay protection, never for latency).
pub fn unix_ns() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    rtt_us: u64,
    offset_us: i64,
}

/// NTP-style offset estimator using the minimum-RTT sample of a sliding window.
#[derive(Default)]
pub struct ClockSync {
    samples: VecDeque<Sample>,
    smoothed_rtt_us: Option<f64>,
}

impl ClockSync {
    /// ~8 s of pings (one per 50 ms).
    const WINDOW: usize = 160;

    pub fn new() -> Self {
        Self::default()
    }

    /// `t0` local send, `t1` remote receive, `t2` remote send, `t3` local receive.
    pub fn add(&mut self, t0: u64, t1: u64, t2: u64, t3: u64) {
        // i128: timestamps come from the peer and must not overflow.
        let (t0, t1, t2, t3) = (t0 as i128, t1 as i128, t2 as i128, t3 as i128);
        let rtt = (t3 - t0) - (t2 - t1);
        if !(0..=60_000_000).contains(&rtt) {
            return;
        }
        let Ok(offset) = i64::try_from(((t1 - t0) + (t2 - t3)) / 2) else { return };
        if self.samples.len() == Self::WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample { rtt_us: rtt as u64, offset_us: offset });
        let r = rtt as f64;
        self.smoothed_rtt_us = Some(match self.smoothed_rtt_us {
            None => r,
            Some(s) => s * 0.875 + r * 0.125,
        });
    }

    /// Remote clock minus local clock, in microseconds.
    pub fn offset_us(&self) -> Option<i64> {
        self.samples.iter().min_by_key(|s| s.rtt_us).map(|s| s.offset_us)
    }

    /// Convert a remote timestamp to local clock.
    pub fn remote_to_local(&self, remote_us: u64) -> Option<u64> {
        self.offset_us().map(|o| (remote_us as i64 - o).max(0) as u64)
    }

    pub fn min_rtt_us(&self) -> Option<u64> {
        self.samples.iter().map(|s| s.rtt_us).min()
    }

    pub fn smoothed_rtt_us(&self) -> Option<f64> {
        self.smoothed_rtt_us
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic() {
        let a = now_us();
        let b = now_us();
        assert!(b >= a);
    }

    #[test]
    fn offset_estimate() {
        let mut c = ClockSync::new();
        // remote is 1_000_000 us ahead; one-way delay 300us each way.
        for i in 0..10u64 {
            let t0 = 10_000 + i * 1000;
            let t1 = t0 + 300 + 1_000_000;
            let t2 = t1 + 50;
            let t3 = t2 - 1_000_000 + 300 + i; // jitter on return path
            c.add(t0, t1, t2, t3);
        }
        let o = c.offset_us().unwrap();
        assert!((o - 1_000_000).abs() <= 1, "offset {o}");
        assert_eq!(c.min_rtt_us(), Some(600));
    }
}
