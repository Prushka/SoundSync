//! Playout timing controller: turns the measured playout error into a resample-ratio correction.
//!
//! The plant is `de/dt = (ratio − 1) + drift`: running the resampler slower than nominal
//! (`ratio < 1`, fewer output frames per input frame) consumes the source faster and pulls a
//! late stream back. A PI loop on the low-passed error with a soft deadband settles the error
//! inside the deadband while the integrator carries the steady clock-drift compensation.

use crate::types::Nanos;

#[derive(Clone, Copy, Debug)]
pub struct SyncParams {
    /// Proportional gain: ppm of ratio correction per ms of (deadbanded) error.
    pub kp_ppm_per_ms: f64,
    /// Integral gain: ppm per ms·s of accumulated (deadbanded) error.
    pub ki_ppm_per_ms_s: f64,
    pub max_ppm: f64,
    pub deadband_ns: Nanos,
    pub resync_ns: Nanos,
    /// Per-update exponential smoothing factor for the error.
    pub lowpass_alpha: f64,
}

impl Default for SyncParams {
    /// kp 2000 ppm/ms and ki 1000 ppm/(ms·s) make the loop critically damped at ~1 rad/s
    /// (Kp = 2 /s, Ki = 1 /s²): a 200 ppm drift step peaks at ~75 µs of error. The 0.1 ms
    /// deadband (on the 0.2 s low-passed error) keeps jitter from moving the ratio.
    fn default() -> Self {
        Self {
            kp_ppm_per_ms: 2000.0,
            ki_ppm_per_ms_s: 1000.0,
            max_ppm: 300.0,
            deadband_ns: 100_000,
            resync_ns: 15_000_000,
            lowpass_alpha: 0.05,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SyncDecision {
    /// Error too large to slew; drop or pad audio and start over.
    Realign,
    /// Relative resample ratio to apply (1.0 = nominal).
    Ratio(f64),
}

#[derive(Clone, Debug)]
pub struct SyncController {
    p: SyncParams,
    filtered_ms: f64,
    integral_ms_s: f64,
}

impl SyncController {
    pub fn new(p: SyncParams) -> Self {
        Self { p, filtered_ms: 0.0, integral_ms_s: 0.0 }
    }

    /// `error_ns` = scheduled DAC time − target time of the frame at the output head
    /// (positive = late). `dt_ns` = time since the previous update.
    pub fn update(&mut self, error_ns: Nanos, dt_ns: Nanos) -> SyncDecision {
        if error_ns.abs() > self.p.resync_ns {
            return SyncDecision::Realign;
        }
        let e_ms = error_ns as f64 / 1e6;
        self.filtered_ms += self.p.lowpass_alpha * (e_ms - self.filtered_ms);
        let db_ms = self.p.deadband_ns as f64 / 1e6;
        let excess = if self.filtered_ms.abs() > db_ms { self.filtered_ms - db_ms.copysign(self.filtered_ms) } else { 0.0 };
        let dt_s = dt_ns.max(0) as f64 / 1e9;
        self.integral_ms_s += excess * dt_s;
        // Anti-windup: the integral term alone may never exceed the ratio cap.
        let i_cap = self.p.max_ppm / self.p.ki_ppm_per_ms_s;
        self.integral_ms_s = self.integral_ms_s.clamp(-i_cap, i_cap);
        let ppm = -(self.p.kp_ppm_per_ms * excess + self.p.ki_ppm_per_ms_s * self.integral_ms_s);
        let ppm = ppm.clamp(-self.p.max_ppm, self.p.max_ppm);
        SyncDecision::Ratio(1.0 + ppm * 1e-6)
    }

    pub fn reset(&mut self) {
        self.filtered_ms = 0.0;
        self.integral_ms_s = 0.0;
    }

    /// Forgets the error history but keeps the integrator, which holds the output's clock drift
    /// compensation: a realign within a stream does not change how fast the DAC runs.
    pub fn reset_error(&mut self) {
        self.filtered_ms = 0.0;
    }

    pub fn filtered_error_ns(&self) -> f64 {
        self.filtered_ms * 1e6
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: Nanos = 10_000_000;

    fn ratio(d: SyncDecision) -> f64 {
        match d {
            SyncDecision::Ratio(r) => r,
            SyncDecision::Realign => panic!("unexpected realign"),
        }
    }

    #[test]
    fn late_error_speeds_up() {
        let mut c = SyncController::new(SyncParams::default());
        let mut r = 1.0;
        for _ in 0..50 {
            r = ratio(c.update(2_000_000, DT));
        }
        assert!(r < 1.0, "late playback must consume input faster: ratio {r}");
    }

    #[test]
    fn early_error_slows_down() {
        let mut c = SyncController::new(SyncParams::default());
        let mut r = 1.0;
        for _ in 0..50 {
            r = ratio(c.update(-2_000_000, DT));
        }
        assert!(r > 1.0, "early playback must consume input slower: ratio {r}");
    }

    #[test]
    fn ratio_is_capped_at_300ppm() {
        let mut c = SyncController::new(SyncParams::default());
        for _ in 0..1000 {
            let r = ratio(c.update(10_000_000, DT));
            assert!((r - 1.0).abs() <= 300e-6 + 1e-12, "ratio {r}");
        }
    }

    #[test]
    fn deadband_keeps_ratio_steady() {
        let mut c = SyncController::new(SyncParams::default());
        let first = ratio(c.update(300_000, DT));
        for i in 0..100 {
            let e = if i % 2 == 0 { -300_000 } else { 300_000 };
            let r = ratio(c.update(e, DT));
            assert!((r - first).abs() < 1e-7, "ratio moved to {r} on jitter");
        }
    }

    #[test]
    fn large_error_requests_realign() {
        let mut c = SyncController::new(SyncParams::default());
        assert_eq!(c.update(16_000_000, DT), SyncDecision::Realign);
        assert_eq!(c.update(-16_000_000, DT), SyncDecision::Realign);
    }

    #[test]
    fn integrator_holds_drift_compensation() {
        // A constant drift is only cancelled once the integrator supplies it; after the error has
        // been driven back inside the deadband the ratio must stay off-nominal.
        let mut c = SyncController::new(SyncParams::default());
        let mut e = 0.0f64; // ns
        let drift = 150e-6; // the device runs fast: without correction the error grows
        let mut r = 1.0;
        for _ in 0..3000 {
            r = ratio(c.update(e as Nanos, DT));
            e += (drift - (1.0 - r)) * DT as f64;
        }
        assert!(e.abs() < 150_000.0, "residual error {e} ns");
        assert!(((1.0 - r) - drift).abs() < 20e-6, "ratio {r}");
    }

    #[test]
    fn reset_returns_to_nominal() {
        let mut c = SyncController::new(SyncParams::default());
        for _ in 0..100 {
            c.update(5_000_000, DT);
        }
        c.reset();
        assert_eq!(ratio(c.update(0, DT)), 1.0);
    }

    #[test]
    fn reset_error_keeps_drift_compensation() {
        let mut c = SyncController::new(SyncParams::default());
        for _ in 0..100 {
            c.update(5_000_000, DT);
        }
        let held = ratio(c.update(0, DT));
        c.reset_error();
        let r = ratio(c.update(0, DT));
        assert!(r < 1.0, "the integrator's correction must survive: {r}");
        assert!((r - held).abs() < 1e-9 + (1.0 - held).abs() * 0.01, "{r} vs {held}");
    }
}
