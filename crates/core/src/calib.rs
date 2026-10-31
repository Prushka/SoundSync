//! Calibration signals and detection.
//!
//! Each node plays a short exponential sweep at a scheduled time; the microphone recording is
//! cross-correlated with the sweep to find when it actually arrived. Only differences between
//! nodes matter, so the microphone's own input latency cancels out.

use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

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

/// Finds `reference` inside `recording` by FFT cross-correlation (either polarity) with parabolic
/// peak interpolation. `None` when the peak is not at least [`MIN_SNR_DB`] above the rest.
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
    let mut prod: Vec<Complex<f64>> = fa.iter().zip(&fb).map(|(x, y)| x * y.conj()).collect();
    let mut corr = inv.make_output_vec();
    inv.process(&mut prod, &mut corr).ok()?;

    let lags = recording.len() - reference.len() + 1;
    let mag: Vec<f64> = corr[..lags].iter().map(|c| c.abs()).collect();
    let (peak_k, &peak) = mag.iter().enumerate().max_by(|x, y| x.1.total_cmp(y.1))?;
    if peak <= 0.0 {
        return None;
    }
    let guard = (reference.len() / 10).max(1);
    let (sum_sq, count) = mag
        .iter()
        .enumerate()
        .filter(|(k, _)| k.abs_diff(peak_k) > guard)
        .fold((0.0, 0usize), |(s, c), (_, &m)| (s + m * m, c + 1));
    let snr_db = if count == 0 || sum_sq == 0.0 {
        f32::INFINITY
    } else {
        (20.0 * (peak / (sum_sq / count as f64).sqrt()).log10()) as f32
    };
    if snr_db < MIN_SNR_DB {
        return None;
    }
    let mut offset = peak_k as f64;
    if peak_k > 0 && peak_k + 1 < lags {
        let (ym, y0, yp) = (mag[peak_k - 1], mag[peak_k], mag[peak_k + 1]);
        let denom = ym - 2.0 * y0 + yp;
        if denom != 0.0 {
            offset += (0.5 * (ym - yp) / denom).clamp(-0.5, 0.5);
        }
    }
    Some(Detection { offset_frames: offset, snr_db })
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
