//! Small statistics helpers for latency/throughput readouts.

/// Exponential moving average.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ema {
    value: Option<f64>,
    alpha: f64,
}

impl Ema {
    pub fn new(alpha: f64) -> Self {
        Self { value: None, alpha }
    }
    pub fn add(&mut self, x: f64) {
        self.value = Some(match self.value {
            None => x,
            Some(v) => v + self.alpha * (x - v),
        });
    }
    pub fn get(&self) -> f64 {
        self.value.unwrap_or(0.0)
    }
}

/// Windowed counter: call `add` per event, `rate_per_sec` to read and reset.
#[derive(Clone, Copy, Debug)]
pub struct Rate {
    count: f64,
    since_us: u64,
}

impl Rate {
    pub fn new(now_us: u64) -> Self {
        Self { count: 0.0, since_us: now_us }
    }
    pub fn add(&mut self, n: f64) {
        self.count += n;
    }
    /// Returns `Some(rate)` if at least `window_us` elapsed (and resets).
    pub fn poll(&mut self, now_us: u64, window_us: u64) -> Option<f64> {
        let dt = now_us.saturating_sub(self.since_us);
        if dt < window_us {
            return None;
        }
        let r = self.count * 1e6 / dt as f64;
        self.count = 0.0;
        self.since_us = now_us;
        Some(r)
    }
}

/// Fixed-size latency histogram summary (mean / p50 / p95 / max) over the
/// samples since the last `take`.
#[derive(Clone, Debug, Default)]
pub struct Samples {
    v: Vec<u32>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Summary {
    pub n: usize,
    pub mean: f64,
    pub p50: u32,
    pub p95: u32,
    pub max: u32,
}

impl Samples {
    pub fn add(&mut self, x: u32) {
        if self.v.len() < 100_000 {
            self.v.push(x);
        }
    }
    pub fn take(&mut self) -> Summary {
        let mut v = std::mem::take(&mut self.v);
        if v.is_empty() {
            return Summary::default();
        }
        v.sort_unstable();
        let n = v.len();
        Summary {
            n,
            mean: v.iter().map(|&x| x as f64).sum::<f64>() / n as f64,
            p50: v[n / 2],
            p95: v[(n * 95 / 100).min(n - 1)],
            max: v[n - 1],
        }
    }
}
