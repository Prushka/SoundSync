//! Smooths jittery device timestamps into a clean frame-index → time line.

use std::collections::VecDeque;

use crate::types::Nanos;

const WINDOW: usize = 512;
const MIN_FIT_POINTS: usize = 16;

/// Least-squares fit of device timestamps against frame index over the last 512 points.
/// With fewer than 16 points it extrapolates from the newest point at the nominal rate.
#[derive(Clone, Debug)]
pub struct FrameClock {
    nominal_rate: u32,
    points: VecDeque<(u64, Nanos)>,
    /// (time at newest frame, ns per frame), cached after each add.
    fit: Option<(f64, f64)>,
}

impl FrameClock {
    pub fn new(nominal_rate: u32) -> Self {
        Self { nominal_rate, points: VecDeque::with_capacity(WINDOW), fit: None }
    }

    pub fn add(&mut self, frame: u64, time_ns: Nanos) {
        if self.points.len() == WINDOW {
            self.points.pop_front();
        }
        self.points.push_back((frame, time_ns));
        self.fit = self.compute_fit();
    }

    fn nominal_ns_per_frame(&self) -> f64 {
        1e9 / self.nominal_rate as f64
    }

    fn compute_fit(&self) -> Option<(f64, f64)> {
        if self.points.len() < MIN_FIT_POINTS {
            return None;
        }
        let &(f_ref, t_ref) = self.points.back()?;
        let n = self.points.len() as f64;
        let xs = self.points.iter().map(|&(f, _)| f as f64 - f_ref as f64);
        let ys = self.points.iter().map(|&(_, t)| (t - t_ref) as f64);
        let mx = xs.clone().sum::<f64>() / n;
        let my = ys.clone().sum::<f64>() / n;
        let (mut sxy, mut sxx) = (0.0, 0.0);
        for (x, y) in xs.zip(ys) {
            sxy += (x - mx) * (y - my);
            sxx += (x - mx) * (x - mx);
        }
        if sxx <= 0.0 {
            return None;
        }
        let slope = sxy / sxx;
        // Reject absurd slopes (more than 1% off nominal) rather than trusting a broken stream.
        let nominal = self.nominal_ns_per_frame();
        if (slope / nominal - 1.0).abs() > 0.01 {
            return None;
        }
        Some((t_ref as f64 + my - slope * mx, slope))
    }

    pub fn time_of(&self, frame: u64) -> Nanos {
        let Some(&(f_ref, t_ref)) = self.points.back() else { return 0 };
        let dx = frame as f64 - f_ref as f64;
        match self.fit {
            Some((t_at_ref, slope)) => (t_at_ref + slope * dx).round() as Nanos,
            None => t_ref + (dx * self.nominal_ns_per_frame()).round() as Nanos,
        }
    }

    /// Actual frame rate divided by nominal (1.0 until enough points).
    pub fn rate_ratio(&self) -> f64 {
        match self.fit {
            Some((_, slope)) => self.nominal_ns_per_frame() / slope,
            None => 1.0,
        }
    }

    pub fn reset(&mut self) {
        self.points.clear();
        self.fit = None;
    }

    /// True until the first timestamp is added (or after a reset).
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Nanos = 5_000_000_000;
    fn true_time(frame: u64) -> Nanos {
        T0 + (frame as f64 * 1e9 / (48_000.0 * (1.0 + 50e-6))).round() as Nanos
    }

    #[test]
    fn frame_clock_smooths_jittery_timestamps() {
        let mut state: u64 = 3;
        let mut noise = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) * 200_000.0
        };
        let mut fc = FrameClock::new(48_000);
        let mut f = 0u64;
        while f < 48_000 * 5 {
            fc.add(f, true_time(f) + noise() as Nanos);
            f += 480;
        }
        for probe in [f - 480, f - 48_000, f] {
            let err = fc.time_of(probe) - true_time(probe);
            assert!(err.abs() < 20_000, "frame {probe}: error {err} ns");
        }
        assert!((fc.rate_ratio() - (1.0 + 50e-6)).abs() < 5e-6, "ratio {}", fc.rate_ratio());
    }

    #[test]
    fn few_points_use_nominal_rate() {
        let mut fc = FrameClock::new(48_000);
        fc.add(1000, 2_000_000_000);
        assert_eq!(fc.time_of(1480), 2_010_000_000);
        assert_eq!(fc.rate_ratio(), 1.0);
    }

    #[test]
    fn frame_clock_reset_on_discontinuity() {
        let mut fc = FrameClock::new(48_000);
        for i in 0..100u64 {
            fc.add(i * 480, 1_000_000_000 + i as Nanos * 10_000_000);
        }
        fc.reset();
        assert!(fc.is_empty());
        fc.add(0, 9_000_000_000);
        assert!(!fc.is_empty());
        let t = fc.time_of(480);
        assert!((t - 9_010_000_000).abs() <= 1_000, "{t}");
    }
}
