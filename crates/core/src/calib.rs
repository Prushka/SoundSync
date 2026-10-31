//! Calibration signals and detection.
//!
//! Each node plays a short exponential sweep at a scheduled time; the microphone recording is
//! cross-correlated with the sweep to find when it actually arrived. Only differences between
//! nodes matter, so the microphone's own input latency cancels out.

use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

use serde::{Deserialize, Serialize};

use crate::types::{Nanos, NodeId};

const FADE_S: f64 = 0.005;
/// Detection threshold: peak correlation over the RMS of the rest. Noise alone reaches ~13 dB
/// across a second of lags, so 20 dB keeps false detections out.
pub const MIN_SNR_DB: f32 = 20.0;

/// Exponential (log) sine sweep from `f0_hz` to `f1_hz`, amplitude 0.5, 5 ms raised-cosine fades.
pub fn log_sweep(sample_rate: u32, duration_s: f64, f0_hz: f64, f1_hz: f64) -> Vec<f32> {
    let n = (duration_s * sample_rate as f64).round() as usize;
    let k = (f1_hz / f0_hz).ln();
    (0..n)
        .map(|i| {
            let t = i as f64 / sample_rate as f64;
            let phase = std::f64::consts::TAU * f0_hz * duration_s / k * ((t / duration_s * k).exp() - 1.0);
            let env = if t < FADE_S {
                0.5 - 0.5 * (std::f64::consts::PI * t / FADE_S).cos()
            } else if t > duration_s - FADE_S {
                0.5 - 0.5 * (std::f64::consts::PI * (duration_s - t) / FADE_S).cos()
            } else {
                1.0
            };
            (0.5 * env * phase.sin()) as f32
        })
        .collect()
}

/// The standard calibration sweep: 100 ms, 200 Hz → 8 kHz.
pub fn calibration_sweep(sample_rate: u32) -> Vec<f32> {
    log_sweep(sample_rate, 0.1, 200.0, 8000.0)
}

/// 10 ms Hann-windowed sine burst, amplitude 0.5 (sync-test click).
pub fn click(sample_rate: u32, freq_hz: f64) -> Vec<f32> {
    let n = (sample_rate / 100) as usize;
    (0..n)
        .map(|i| {
            let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (n - 1) as f64).cos();
            (0.5 * w * (std::f64::consts::TAU * freq_hz * i as f64 / sample_rate as f64).sin()) as f32
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Detection {
    /// Position of the reference's first sample within the recording (fractional frames).
    pub offset_frames: f64,
    pub snr_db: f32,
}

/// The earliest arrival at least this strong relative to the strongest (-10 dB) is taken as the
/// direct sound: a reflection off a wall can be louder, but it always comes later.
const DIRECT_FRACTION: f64 = 0.316;

/// Finds `reference` inside `recording` (either polarity). The recording is deconvolved by the
/// reference over the reference's own frequency band with a smooth (Hann) taper, which turns
/// every arrival into a short pulse without side ripples; its envelope then shows each arrival
/// as one hump. Returns the earliest strong arrival, refined by parabolic interpolation, or
/// `None` when the strongest one is not at least [`MIN_SNR_DB`] above the rest.
pub fn find_signal(recording: &[f32], reference: &[f32]) -> Option<Detection> {
    if reference.is_empty() || recording.len() < reference.len() {
        return None;
    }
    let n = (recording.len() + reference.len()).next_power_of_two();
    let mut planner = RealFftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    let mut a = fwd.make_input_vec();
    a[..recording.len()].iter_mut().zip(recording).for_each(|(d, &s)| *d = s as f64);
    let mut b = fwd.make_input_vec();
    b[..reference.len()].iter_mut().zip(reference).for_each(|(d, &s)| *d = s as f64);
    let mut fa = fwd.make_output_vec();
    let mut fb = fwd.make_output_vec();
    fwd.process(&mut a, &mut fa).ok()?;
    fwd.process(&mut b, &mut fb).ok()?;
    // Deconvolve over the reference's band (within 20 dB of its strongest bin), Hann-tapered.
    let power: Vec<f64> = fb.iter().map(|c| c.norm_sqr()).collect();
    let max_power = power.iter().copied().fold(0.0, f64::max);
    if max_power <= 0.0 {
        return None;
    }
    let lo = power.iter().position(|&p| p >= max_power * 0.01)?;
    let hi = power.iter().rposition(|&p| p >= max_power * 0.01)?;
    let width = (hi - lo).max(1) as f64;
    let prod: Vec<Complex<f64>> = fa
        .iter()
        .zip(&fb)
        .enumerate()
        .map(|(k, (x, y))| {
            if k < lo || k > hi {
                return Complex::new(0.0, 0.0);
            }
            let taper = 0.5 - 0.5 * (std::f64::consts::TAU * (k - lo) as f64 / width).cos();
            x * y.conj() * (taper / (power[k] + max_power * 1e-3))
        })
        .collect();
    // The result and its Hilbert transform (spectrum turned by -90°) give the envelope.
    let last = prod.len() - 1;
    let mut prod = prod;
    prod[0] = Complex::new(0.0, 0.0);
    prod[last] = Complex::new(0.0, 0.0);
    let mut quad: Vec<Complex<f64>> = prod.iter().map(|c| Complex::new(c.im, -c.re)).collect();
    let mut corr = inv.make_output_vec();
    inv.process(&mut prod.clone(), &mut corr).ok()?;
    let mut hilbert = inv.make_output_vec();
    inv.process(&mut quad, &mut hilbert).ok()?;
    let lags = recording.len() - reference.len() + 1;
    let env: Vec<f64> = (0..lags).map(|k| corr[k].hypot(hilbert[k])).collect();
    let (peak_k, &peak) = env.iter().enumerate().max_by(|x, y| x.1.total_cmp(y.1))?;
    if peak <= 0.0 {
        return None;
    }
    let guard = (reference.len() / 10).max(1);
    let (sum_sq, count) = env
        .iter()
        .enumerate()
        .filter(|(k, _)| k.abs_diff(peak_k) > guard)
        .fold((0.0, 0usize), |(s, c), (_, &m)| (s + m * m, c + 1));
    let noise = if count == 0 { 0.0 } else { (sum_sq / count as f64).sqrt() };
    let snr_db = if noise == 0.0 { f32::INFINITY } else { (20.0 * (peak / noise).log10()) as f32 };
    if snr_db < MIN_SNR_DB {
        return None;
    }
    // Earliest local maximum that is strong (and well above the noise): the direct sound.
    let threshold = (peak * DIRECT_FRACTION).max(noise * 10f64.powf(MIN_SNR_DB as f64 / 20.0));
    let k = (1..peak_k).find(|&k| env[k] >= threshold && env[k] >= env[k - 1] && env[k] >= env[k + 1]).unwrap_or(peak_k);
    let mut offset = k as f64;
    if k > 0 && k + 1 < lags {
        let (ym, y0, yp) = (env[k - 1], env[k], env[k + 1]);
        let denom = ym - 2.0 * y0 + yp;
        if denom != 0.0 {
            offset += (0.5 * (ym - yp) / denom).clamp(-0.5, 0.5);
        }
    }
    Some(Detection { offset_frames: offset, snr_db })
}

/// Below this a detection counts as weak (the run still uses it; detection needs [`MIN_SNR_DB`]).
pub const WEAK_SNR_DB: f32 = 30.0;
/// A device's sweeps further apart than this make its correction doubtful.
pub const MAX_SPREAD_NS: Nanos = 1_000_000;

/// One device's outcome of a calibration (or check) run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibResult {
    pub node: NodeId,
    /// Add this to the device's extra latency (relative to the reference device).
    pub delta_ns: Nanos,
    /// Signal-to-noise of the weakest sweep heard, in dB.
    pub snr_db: f32,
    /// Largest difference between this device's sweeps (0 with one sweep).
    pub spread_ns: Nanos,
    pub heard: u8,
    pub sweeps: u8,
}

impl CalibResult {
    /// Reasons to doubt this measurement; empty when it looks good.
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if self.heard < self.sweeps {
            p.push(format!("heard {} of {} sweeps", self.heard, self.sweeps));
        }
        if self.spread_ns > MAX_SPREAD_NS {
            p.push(format!("sweeps disagree by {:.1} ms", self.spread_ns as f64 / 1e6));
        }
        if self.snr_db < WEAK_SNR_DB {
            p.push(format!("weak signal ({:.0} dB)", self.snr_db));
        }
        p
    }
}

/// One device's sweeps: (arrival − scheduled, SNR in dB), or `None` for a sweep not heard.
pub type Sweeps = Vec<Option<(Nanos, f32)>>;

/// Turns each device's sweeps — arrival residual (arrival − scheduled) and SNR, or `None` when
/// a sweep was not heard — into corrections: each device's median residual against the
/// reference. `Err(node)` names a device none of whose sweeps was heard.
pub fn calibration_results(measured: &[(NodeId, Sweeps)], reference: CalibReference) -> Result<Vec<CalibResult>, NodeId> {
    let mut medians = Vec::with_capacity(measured.len());
    let mut results = Vec::with_capacity(measured.len());
    for (node, sweeps) in measured {
        let mut heard: Vec<Nanos> = sweeps.iter().flatten().map(|&(r, _)| r).collect();
        if heard.is_empty() {
            return Err(*node);
        }
        heard.sort_unstable();
        let m = heard.len() / 2;
        let median = if heard.len() % 2 == 1 { heard[m] } else { (heard[m - 1] + heard[m]) / 2 };
        medians.push((*node, median));
        results.push(CalibResult {
            node: *node,
            delta_ns: 0,
            snr_db: sweeps.iter().flatten().map(|&(_, s)| s).fold(f32::INFINITY, f32::min),
            spread_ns: heard[heard.len() - 1] - heard[0],
            heard: heard.len() as u8,
            sweeps: sweeps.len() as u8,
        });
    }
    for (r, (_, delta)) in results.iter_mut().zip(solve_offsets(&medians, reference)) {
        r.delta_ns = delta;
    }
    Ok(results)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalibReference {
    Median,
    Node(NodeId),
}

/// Given each node's arrival residual (arrival − scheduled), returns the delta to ADD to each
/// node's extra latency so all arrive together with the reference.
pub fn solve_offsets(residuals: &[(NodeId, Nanos)], reference: CalibReference) -> Vec<(NodeId, Nanos)> {
    if residuals.is_empty() {
        return Vec::new();
    }
    let median = || {
        let mut v: Vec<Nanos> = residuals.iter().map(|&(_, r)| r).collect();
        v.sort_unstable();
        let m = v.len() / 2;
        if v.len() % 2 == 1 { v[m] } else { (v[m - 1] + v[m]) / 2 }
    };
    let reference = match reference {
        CalibReference::Node(id) => residuals.iter().find(|&&(n, _)| n == id).map_or_else(median, |&(_, r)| r),
        CalibReference::Median => median(),
    };
    residuals.iter().map(|&(n, r)| (n, r - reference)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// Exponential sweep evaluated at an arbitrary (fractional) time, independent of `log_sweep`.
    fn sweep_value(t: f64, dur: f64, f0: f64, f1: f64) -> f64 {
        if !(0.0..dur).contains(&t) {
            return 0.0;
        }
        let k = (f1 / f0).ln();
        let phase = std::f64::consts::TAU * f0 * dur / k * ((t / dur * k).exp() - 1.0);
        let fade = 0.005;
        let env = if t < fade {
            0.5 - 0.5 * (std::f64::consts::PI * t / fade).cos()
        } else if t > dur - fade {
            0.5 - 0.5 * (std::f64::consts::PI * (dur - t) / fade).cos()
        } else {
            1.0
        };
        0.5 * env * phase.sin()
    }

    fn noise(seed: u64, n: usize, amp: f32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32 * amp
            })
            .collect()
    }

    #[test]
    fn sweep_has_expected_length_and_level() {
        let s = log_sweep(RATE, 0.1, 200.0, 8000.0);
        assert_eq!(s.len(), 4800);
        let peak = s.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak <= 0.5 + 1e-6 && peak > 0.49, "peak {peak}");
        assert!(s[0].abs() < 1e-3 && s[4799].abs() < 1e-2, "faded edges");
        for (i, &x) in s.iter().enumerate().step_by(97) {
            let want = sweep_value(i as f64 / RATE as f64, 0.1, 200.0, 8000.0);
            assert!((x as f64 - want).abs() < 1e-4, "sample {i}: {x} vs {want}");
        }
    }

    #[test]
    fn finds_sweep_delay_within_half_ms_in_noise() {
        let reference = log_sweep(RATE, 0.1, 200.0, 8000.0);
        let delay = 12_345.6;
        let mut rec = noise(11, 48_000, 0.01);
        for (i, x) in rec.iter_mut().enumerate() {
            *x += 0.1 * sweep_value((i as f64 - delay) / RATE as f64, 0.1, 200.0, 8000.0) as f32;
        }
        let d = find_signal(&rec, &reference).expect("detected");
        assert!((d.offset_frames - delay).abs() < 24.0, "offset {}", d.offset_frames);
        assert!((d.offset_frames - delay).abs() < 1.0, "sub-sample accuracy expected, got {}", d.offset_frames);
        assert!(d.snr_db > 12.0, "snr {}", d.snr_db);
    }

    #[test]
    fn earliest_strong_arrival_beats_a_louder_reflection() {
        // A speaker facing a wall: its direct sound reaches the microphone first but quieter
        // than the reflection 4 ms later. The direct arrival is the one that tells the latency.
        let reference = log_sweep(RATE, 0.1, 200.0, 8000.0);
        let direct = 12_345.6;
        let reflection = direct + 0.004 * RATE as f64;
        let mut rec = noise(5, 48_000, 0.01);
        for (i, x) in rec.iter_mut().enumerate() {
            let t = |at: f64| sweep_value((i as f64 - at) / RATE as f64, 0.1, 200.0, 8000.0);
            *x += (0.4 * t(direct) - 0.8 * t(reflection)) as f32; // reflections can invert, too
        }
        let d = find_signal(&rec, &reference).expect("detected");
        assert!((d.offset_frames - direct).abs() < 2.0, "found {:.1}, direct sound at {direct}", d.offset_frames);
    }

    #[test]
    fn results_take_the_median_of_each_devices_sweeps_and_flag_doubtful_ones() {
        const MS: Nanos = 1_000_000;
        let measured = [
            (1, vec![Some((10 * MS, 42.0)), Some((10 * MS + 200_000, 40.0)), Some((13 * MS, 36.0))]), // one sweep off
            (2, vec![Some((5 * MS, 44.0)), None, Some((5 * MS + 100_000, 45.0))]),                  // one not heard
            (3, vec![Some((20 * MS, 25.0)), Some((20 * MS, 26.0)), Some((20 * MS + 50_000, 25.5))]), // weak
        ];
        let results = calibration_results(&measured, CalibReference::Node(2)).unwrap();
        let by = |n: NodeId| results.iter().find(|r| r.node == n).unwrap();
        // Corrections are relative to device 2 (median of its two: 5.05 ms); later = more extra latency.
        assert_eq!(by(1).delta_ns, 10_200_000 - 5_050_000);
        assert_eq!(by(2).delta_ns, 0);
        assert_eq!(by(3).delta_ns, 20_000_000 - 5_050_000);
        assert_eq!((by(1).heard, by(1).sweeps, by(1).spread_ns), (3, 3, 3 * MS));
        assert_eq!(by(1).snr_db, 36.0, "the weakest sweep counts");
        assert_eq!(by(1).problems(), vec!["sweeps disagree by 3.0 ms".to_string()]);
        assert_eq!(by(2).problems(), vec!["heard 2 of 3 sweeps".to_string()]);
        assert_eq!(by(3).problems(), vec!["weak signal (25 dB)".to_string()]);
        let missing = [(1, vec![Some((MS, 40.0))]), (7, vec![None, None, None])];
        assert_eq!(calibration_results(&missing, CalibReference::Median), Err(7), "a device never heard fails the run");
    }

    #[test]
    fn earliest_arrival_never_fires_early_on_a_single_sweep() {
        // The earliest-arrival rule must not mistake ripple before the pulse for an arrival.
        for rate in [44_100u32, 48_000, 96_000] {
            let reference = calibration_sweep(rate);
            for (seed, delay_s) in [(1u64, 0.1234), (2, 0.2501), (3, 0.31717)] {
                let delay = delay_s * rate as f64;
                let mut rec = noise(seed, rate as usize / 2, 0.005);
                for (i, x) in rec.iter_mut().enumerate() {
                    *x += 0.3 * sweep_value((i as f64 - delay) / rate as f64, 0.1, 200.0, 8000.0) as f32;
                }
                let d = find_signal(&rec, &reference).expect("detected");
                assert!((d.offset_frames - delay).abs() < 1.0, "{rate} Hz: found {:.2}, sweep at {delay:.2}", d.offset_frames);
            }
        }
    }

    #[test]
    fn finds_inverted_polarity() {
        let reference = log_sweep(RATE, 0.1, 200.0, 8000.0);
        let mut rec = vec![0.0f32; 20_000];
        for (i, &x) in reference.iter().enumerate() {
            rec[5_000 + i] = -0.2 * x;
        }
        let d = find_signal(&rec, &reference).expect("detected");
        assert!((d.offset_frames - 5_000.0).abs() < 0.5, "offset {}", d.offset_frames);
    }

    #[test]
    fn returns_none_when_absent() {
        let reference = log_sweep(RATE, 0.1, 200.0, 8000.0);
        assert!(find_signal(&noise(3, 48_000, 0.1), &reference).is_none());
        assert!(find_signal(&[0.0; 100], &reference).is_none(), "recording shorter than reference");
    }

    #[test]
    fn solve_offsets_median_reference() {
        let ms = 1_000_000;
        let deltas = solve_offsets(&[(1, 10 * ms), (2, 45 * ms), (3, 190 * ms)], CalibReference::Median);
        assert_eq!(deltas, vec![(1, -35 * ms), (2, 0), (3, 145 * ms)]);
        let even = solve_offsets(&[(1, 10 * ms), (2, 20 * ms), (3, 40 * ms), (4, 50 * ms)], CalibReference::Median);
        assert_eq!(even, vec![(1, -20 * ms), (2, -10 * ms), (3, 10 * ms), (4, 20 * ms)]);
    }

    #[test]
    fn solve_offsets_node_reference() {
        let ms = 1_000_000;
        let deltas = solve_offsets(&[(1, 10 * ms), (2, 45 * ms), (3, 190 * ms)], CalibReference::Node(3));
        assert_eq!(deltas, vec![(1, -180 * ms), (2, -145 * ms), (3, 0)]);
        // Unknown reference node falls back to the median.
        let fallback = solve_offsets(&[(1, 10 * ms), (2, 45 * ms), (3, 190 * ms)], CalibReference::Node(9));
        assert_eq!(fallback[1], (2, 0));
    }

    #[test]
    fn click_is_short_and_bounded() {
        let c = click(RATE, 1000.0);
        assert_eq!(c.len(), 480);
        let peak = c.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak <= 0.5 && peak > 0.45, "peak {peak}");
        assert!(c[0].abs() < 1e-6 && c[479].abs() < 1e-2);
    }
}
