//! Peer clock estimation from NTP-style exchanges.
//!
//! For each exchange `t1` (local send), `t2` (remote receive), `t3` (remote send), `t4` (local
//! receive): offset θ = ((t2−t1)+(t3−t4))/2 and round-trip delay δ = (t4−t1)−(t3−t2). Only the
//! lowest-delay quarter of a sliding window is trusted (queueing only ever adds delay), and a
//! least-squares fit of θ against local time gives offset and skew.

use crate::types::Nanos;

pub const CLOCK_MIN_SAMPLES: usize = 8;
pub const CLOCK_WINDOW: usize = 128;
const MAX_SKEW: f64 = 500e-6;
const MIN_SKEW_SPAN_NS: Nanos = 2_000_000_000;
/// A sample's offset can be wrong by at most half its round trip; anything beyond that plus this
/// margin means one of the clocks stepped (a Mac's clock stops while it sleeps).
const STEP_MARGIN_NS: f64 = 50_000_000.0;

#[derive(Clone, Copy, Debug)]
pub struct ClockSample {
    pub t1: Nanos,
    pub t2: Nanos,
    pub t3: Nanos,
    pub t4: Nanos,
}

/// Linear map between a local clock and a remote clock:
/// `remote = local + offset + skew·(local − ref_local)`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClockMap {
    pub ref_local: Nanos,
    pub offset_ns: f64,
    pub skew: f64,
}

impl ClockMap {
    pub const IDENTITY: ClockMap = ClockMap { ref_local: 0, offset_ns: 0.0, skew: 0.0 };

    pub fn local_to_remote(&self, local: Nanos) -> Nanos {
        let x = (local - self.ref_local) as f64;
        local + (self.offset_ns + self.skew * x).round() as Nanos
    }

    pub fn remote_to_local(&self, remote: Nanos) -> Nanos {
        // remote − ref = x·(1 + skew) + offset, solved for x = local − ref.
        let x = ((remote - self.ref_local) as f64 - self.offset_ns) / (1.0 + self.skew);
        self.ref_local + x.round() as Nanos
    }
}

#[derive(Clone, Copy, Debug)]
struct Point {
    mid: Nanos,
    theta: f64,
    delay: Nanos,
}

#[derive(Clone, Debug, Default)]
pub struct ClockEstimator {
    window: std::collections::VecDeque<Point>,
}

impl ClockEstimator {
    pub fn new() -> Self {
        Self { window: std::collections::VecDeque::with_capacity(CLOCK_WINDOW) }
    }

    /// Adds one exchange. Returns true if it showed a clock step and restarted the estimate.
    pub fn add(&mut self, s: ClockSample) -> bool {
        let theta = ((s.t2 - s.t1) as f64 + (s.t3 - s.t4) as f64) / 2.0;
        let delay = (s.t4 - s.t1) - (s.t3 - s.t2);
        if delay < 0 {
            return false; // impossible exchange (clock stepped or corrupted)
        }
        let mid = s.t1 + (s.t4 - s.t1) / 2;
        let stepped = self.map().is_some_and(|m| {
            let predicted = m.offset_ns + m.skew * (mid - m.ref_local) as f64;
            (theta - predicted).abs() > delay as f64 / 2.0 + STEP_MARGIN_NS
        });
        if stepped {
            self.window.clear();
        }
        if self.window.len() == CLOCK_WINDOW {
            self.window.pop_front();
        }
        self.window.push_back(Point { mid, theta, delay });
        stepped
    }

    pub fn map(&self) -> Option<ClockMap> {
        if self.window.len() < CLOCK_MIN_SAMPLES {
            return None;
        }
        let mut best: Vec<Point> = self.window.iter().copied().collect();
        best.sort_by_key(|p| p.delay);
        best.truncate((self.window.len() / 4).max(4));

        let newest = self.window.back()?.mid;
        let oldest = self.window.front()?.mid;
        if newest - oldest < MIN_SKEW_SPAN_NS {
            let mut thetas: Vec<f64> = best.iter().map(|p| p.theta).collect();
            thetas.sort_by(f64::total_cmp);
            return Some(ClockMap { ref_local: newest, offset_ns: thetas[thetas.len() / 2], skew: 0.0 });
        }

        let n = best.len() as f64;
        let mx = best.iter().map(|p| (p.mid - newest) as f64).sum::<f64>() / n;
        let my = best.iter().map(|p| p.theta).sum::<f64>() / n;
        let (mut sxy, mut sxx) = (0.0, 0.0);
        for p in &best {
            let dx = (p.mid - newest) as f64 - mx;
            sxy += dx * (p.theta - my);
            sxx += dx * dx;
        }
        let skew = if sxx > 0.0 { (sxy / sxx).clamp(-MAX_SKEW, MAX_SKEW) } else { 0.0 };
        // Evaluate the fit at ref_local = newest.
        let offset_ns = my - skew * mx;
        Some(ClockMap { ref_local: newest, offset_ns, skew })
    }

    pub fn rtt_ns(&self) -> Option<Nanos> {
        self.window.iter().map(|p| p.delay).min()
    }

    pub fn jitter_ns(&self) -> Option<Nanos> {
        let min = self.rtt_ns()?;
        let mut devs: Vec<Nanos> = self.window.iter().map(|p| p.delay - min).collect();
        devs.sort_unstable();
        Some(devs[devs.len() / 2])
    }

    pub fn reset(&mut self) {
        self.window.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift for test jitter.
    struct Rng(u64);
    impl Rng {
        fn next_f64(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    const START: Nanos = 1_000_000_000_000;
    const OFFSET: f64 = 3_700_000.0;
    const SKEW: f64 = 80e-6;

    fn remote_of(local: Nanos) -> Nanos {
        local + (OFFSET + SKEW * (local - START) as f64).round() as Nanos
    }

    /// One NTP-style exchange starting at local time `t1`.
    fn exchange(rng: &mut Rng, t1: Nanos) -> ClockSample {
        let mut one_way = || {
            let mut d = 100_000.0 + rng.next_f64() * 300_000.0;
            if rng.next_f64() < 0.1 {
                d += 2_000_000.0;
            }
            d as Nanos
        };
        let d1 = one_way();
        let d2 = one_way();
        let t2 = remote_of(t1 + d1);
        let t3 = t2 + 20_000;
        let t4 = t1 + d1 + 20_000 + d2;
        ClockSample { t1, t2, t3, t4 }
    }

    #[test]
    fn estimates_offset_within_50us_with_jitter() {
        let mut rng = Rng(7);
        let mut est = ClockEstimator::new();
        let mut t = START;
        let mut last_t4 = t;
        for _ in 0..(60 * 4) {
            let s = exchange(&mut rng, t);
            last_t4 = s.t4;
            est.add(s);
            t += 250_000_000;
        }
        let map = est.map().expect("synced");
        let local_now = last_t4;
        let err = map.remote_to_local(remote_of(local_now)) - local_now;
        assert!(err.abs() < 50_000, "offset error {err} ns");
        assert!((map.skew - SKEW).abs() < 5e-6, "skew {}", map.skew);
        assert!(est.rtt_ns().unwrap() >= 200_000 && est.rtt_ns().unwrap() < 300_000);
        assert!(est.jitter_ns().unwrap() > 0);
    }

    #[test]
    fn not_synced_before_min_samples() {
        let mut rng = Rng(1);
        let mut est = ClockEstimator::new();
        for i in 0..7 {
            est.add(exchange(&mut rng, START + i * 50_000_000));
        }
        assert!(est.map().is_none());
        est.add(exchange(&mut rng, START + 7 * 50_000_000));
        let map = est.map().expect("8 samples are enough");
        // Short window → no skew estimate, offset only.
        assert_eq!(map.skew, 0.0);
        let err = map.remote_to_local(remote_of(START)) - START;
        assert!(err.abs() < 200_000, "offset error {err}");
    }

    #[test]
    fn reset_clears_state() {
        let mut rng = Rng(2);
        let mut est = ClockEstimator::new();
        for i in 0..20 {
            est.add(exchange(&mut rng, START + i * 50_000_000));
        }
        assert!(est.map().is_some());
        est.reset();
        assert!(est.map().is_none());
        assert!(est.rtt_ns().is_none());
    }

    #[test]
    fn remote_clock_step_restarts_estimate() {
        // A Mac's clock stops while it sleeps: after waking, its offset differs by the sleep time.
        // Jittery but consistent samples never restart the estimate; a step does, at once.
        let mut rng = Rng(5);
        let mut est = ClockEstimator::new();
        let mut t = START;
        for _ in 0..60 {
            est.add(exchange(&mut rng, t));
            t += 250_000_000;
        }
        assert_eq!(est.window.len(), 60, "consistent samples must not restart the estimate");
        let step: Nanos = -30_000_000_000;
        let stepped = |s: ClockSample| ClockSample { t2: s.t2 + step, t3: s.t3 + step, ..s };
        est.add(stepped(exchange(&mut rng, t)));
        t += 250_000_000;
        assert!(est.map().is_none(), "a stepped clock must not be mapped with stale samples");
        for _ in 0..CLOCK_MIN_SAMPLES {
            est.add(stepped(exchange(&mut rng, t)));
            t += 50_000_000;
        }
        let map = est.map().expect("re-synced");
        let err = map.remote_to_local(remote_of(t) + step) - t;
        assert!(err.abs() < 500_000, "offset error after the step {err} ns");
    }

    #[test]
    fn identity_map_roundtrips() {
        assert_eq!(ClockMap::IDENTITY.remote_to_local(123_456), 123_456);
        assert_eq!(ClockMap::IDENTITY.local_to_remote(-42), -42);
        let map = ClockMap { ref_local: START, offset_ns: -1_234_567.8, skew: 120e-6 };
        for x in [START - 5_000_000_000, START, START + 77_000_000_000] {
            let back = map.local_to_remote(map.remote_to_local(x));
            assert!((back - x).abs() <= 1, "{x} -> {back}");
        }
        // local_to_remote applies offset and skew relative to ref_local.
        assert_eq!(map.local_to_remote(START + 1_000_000_000), START + 1_000_000_000 - 1_234_568 + 120_000);
    }
}
