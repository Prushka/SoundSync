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

/// Peak of [`wake_noise`] (about -30 dBFS): a soft "shh".
pub const WAKE_LEVEL: f32 = 0.03;

/// One second of soft noise, faded in and out. Played before calibration sweeps: TVs and
/// soundbars mute after a while of silence and lose the start of the next sound.
pub fn wake_noise(sample_rate: u32) -> Vec<f32> {
    let n = sample_rate as usize;
    let fade = n / 10;
    let mut s: u64 = 0x2545_F491_4F6C_DD1D;
    (0..n)
        .map(|i| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let white = (s >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
            let edge = i.min(n - 1 - i);
            let env = if edge < fade { 0.5 - 0.5 * (std::f64::consts::PI * edge as f64 / fade as f64).cos() } else { 1.0 };
            (WAKE_LEVEL as f64 * env * white) as f32
        })
        .collect()
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
    /// How loud the arrival is: its mean square over the reference's length (its echoes for a
    /// fifth of that length included), with the noise before it taken out; dB re full scale.
    pub level_db: f32,
    /// Loudest sample of the arrival, dBFS.
    pub peak_dbfs: f32,
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
    let (level_db, peak_dbfs) = arrival_level(recording, k, reference.len());
    Some(Detection { offset_frames: offset, snr_db, level_db, peak_dbfs })
}

/// Level and peak of an arrival of `len` samples at `start` (see [`Detection::level_db`]). A log
/// sweep has the same energy in every octave, so this level weighs its band the way music sounds.
fn arrival_level(recording: &[f32], start: usize, len: usize) -> (f32, f32) {
    let power = |s: &[f32]| s.iter().map(|&x| (x as f64).powi(2)).sum::<f64>();
    let end = (start + len + len / 5).min(recording.len());
    let span = &recording[start.min(end)..end];
    let before = &recording[..start.saturating_sub(len / 20)];
    let noise = if before.is_empty() { 0.0 } else { power(before) / before.len() as f64 };
    let energy = (power(span) - noise * span.len() as f64).max(0.0);
    let level_db = 10.0 * (energy / len.max(1) as f64).max(1e-12).log10();
    let peak = span.iter().fold(0f32, |m, x| m.max(x.abs()));
    (level_db as f32, 20.0 * peak.max(1e-6).log10())
}

/// Below this a detection counts as weak (the run still uses it; detection needs [`MIN_SNR_DB`]).
pub const WEAK_SNR_DB: f32 = 30.0;
/// A device's sweeps further apart than this make its correction doubtful.
pub const MAX_SPREAD_NS: Nanos = 1_000_000;
/// One speaker's sweeps varying in level by more than this point to a microphone that adjusts
/// its own gain (webcams and cameras do), which squeezes level differences.
pub const MAX_LEVEL_SPREAD_DB: f32 = 2.0;
/// Sweeps peaking above this may have hit the microphone's limit (some limit well below full
/// scale), which makes a loud speaker measure quieter than it is.
pub const OVERLOAD_DBFS: f32 = -6.0;

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
    /// How much louder than the quietest speaker this one arrived (dB, 0 for the quietest).
    pub level_db: f32,
    /// Loudest sample of this speaker's sweeps at the microphone, dBFS.
    pub peak_dbfs: f32,
    /// Largest level difference between this speaker's sweeps (0 with one sweep).
    pub level_spread_db: f32,
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
        if self.level_spread_db > MAX_LEVEL_SPREAD_DB {
            p.push(format!("level varied by {:.1} dB between sweeps (the microphone may adjust its own gain)", self.level_spread_db));
        }
        if self.peak_dbfs > OVERLOAD_DBFS {
            p.push(format!("microphone overloaded (peaks at {:.0} dBFS)", self.peak_dbfs));
        }
        p
    }
}

/// One sweep as the microphone heard it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Heard {
    /// Arrival − scheduled emission.
    pub residual_ns: Nanos,
    pub snr_db: f32,
    pub level_db: f32,
    pub peak_dbfs: f32,
}

/// One device's sweeps, `None` for a sweep not heard.
pub type Sweeps = Vec<Option<Heard>>;

/// Turns each device's sweeps into corrections: each device's median residual against the
/// reference, and its median level against the quietest device. `Err(node)` names a device none
/// of whose sweeps was heard.
pub fn calibration_results(measured: &[(NodeId, Sweeps)], reference: CalibReference) -> Result<Vec<CalibResult>, NodeId> {
    fn median<T: Copy + PartialOrd + std::ops::Add<Output = T>>(mut v: Vec<T>, half: impl Fn(T) -> T) -> T {
        v.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let m = v.len() / 2;
        if v.len() % 2 == 1 { v[m] } else { half(v[m - 1] + v[m]) }
    }
    let mut medians = Vec::with_capacity(measured.len());
    let mut results = Vec::with_capacity(measured.len());
    for (node, sweeps) in measured {
        let heard: Vec<Heard> = sweeps.iter().flatten().copied().collect();
        if heard.is_empty() {
            return Err(*node);
        }
        let residuals: Vec<Nanos> = heard.iter().map(|h| h.residual_ns).collect();
        let levels = heard.iter().map(|h| h.level_db);
        let level_spread_db = levels.clone().fold(f32::NEG_INFINITY, f32::max) - levels.fold(f32::INFINITY, f32::min);
        let (first, last) = (residuals.iter().min().copied().unwrap_or(0), residuals.iter().max().copied().unwrap_or(0));
        medians.push((*node, median(residuals, |x| x / 2)));
        results.push(CalibResult {
            node: *node,
            delta_ns: 0,
            snr_db: heard.iter().map(|h| h.snr_db).fold(f32::INFINITY, f32::min),
            spread_ns: last - first,
            heard: heard.len() as u8,
            sweeps: sweeps.len() as u8,
            level_db: median(heard.iter().map(|h| h.level_db).collect(), |x| x / 2.0),
            peak_dbfs: heard.iter().map(|h| h.peak_dbfs).fold(f32::NEG_INFINITY, f32::max),
            level_spread_db,
        });
    }
    for (r, (_, delta)) in results.iter_mut().zip(solve_offsets(&medians, reference)) {
        r.delta_ns = delta;
    }
    let quietest = results.iter().map(|r| r.level_db).fold(f32::INFINITY, f32::min);
    for r in &mut results {
        r.level_db -= quietest;
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

    fn heard(residual_ns: Nanos, snr_db: f32) -> Option<Heard> {
        Some(Heard { residual_ns, snr_db, level_db: -30.0, peak_dbfs: -20.0 })
    }

    #[test]
    fn results_take_the_median_of_each_devices_sweeps_and_flag_doubtful_ones() {
        const MS: Nanos = 1_000_000;
        let measured = [
            (1, vec![heard(10 * MS, 42.0), heard(10 * MS + 200_000, 40.0), heard(13 * MS, 36.0)]), // one sweep off
            (2, vec![heard(5 * MS, 44.0), None, heard(5 * MS + 100_000, 45.0)]),                  // one not heard
            (3, vec![heard(20 * MS, 25.0), heard(20 * MS, 26.0), heard(20 * MS + 50_000, 25.5)]), // weak
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
        let missing = [(1, vec![heard(MS, 40.0)]), (7, vec![None, None, None])];
        assert_eq!(calibration_results(&missing, CalibReference::Median), Err(7), "a device never heard fails the run");
    }

    #[test]
    fn levels_are_relative_to_the_quietest_speaker() {
        let sweep = |level_db: f32, peak_dbfs: f32| Some(Heard { residual_ns: 0, snr_db: 40.0, level_db, peak_dbfs });
        let measured = [
            (1, vec![sweep(-30.0, -20.0), sweep(-31.0, -21.0), sweep(-29.5, -19.0)]), // median -30
            (2, vec![sweep(-24.0, -12.0), None, sweep(-25.0, -12.5)]),                // median of two: -24.5
            (3, vec![sweep(-22.0, -3.3), sweep(-22.2, -3.4), sweep(-21.9, -3.3)]),    // median -22, overloaded
        ];
        let results = calibration_results(&measured, CalibReference::Median).unwrap();
        let by = |n: NodeId| results.iter().find(|r| r.node == n).unwrap();
        assert_eq!(by(1).level_db, 0.0, "the quietest speaker is the reference");
        assert_eq!(by(2).level_db, 5.5);
        assert_eq!(by(3).level_db, 8.0);
        assert_eq!(by(3).peak_dbfs, -3.3, "the loudest sample of the speaker's sweeps");
        assert!(by(2).problems().iter().all(|p| !p.contains("overloaded")));
        assert_eq!(by(3).problems(), vec!["microphone overloaded (peaks at -3 dBFS)".to_string()]);
    }

    #[test]
    fn a_microphone_that_adjusts_its_own_gain_is_flagged() {
        // A camera microphone measured on real hardware: the same speaker's sweeps came in at
        // -31.0, -28.8 and -27.2 dBFS as its automatic gain crept up. Such a microphone squeezes
        // level differences, so matching needs another Check.
        let sweep = |level_db: f32| Some(Heard { residual_ns: 0, snr_db: 40.0, level_db, peak_dbfs: -15.0 });
        let measured = [(1, vec![sweep(-31.0), sweep(-28.8), sweep(-27.2)]), (2, vec![sweep(-20.0), sweep(-20.3), sweep(-19.9)])];
        let results = calibration_results(&measured, CalibReference::Median).unwrap();
        assert_eq!(results[0].problems(), vec!["level varied by 3.8 dB between sweeps (the microphone may adjust its own gain)".to_string()]);
        assert!(results[1].problems().is_empty(), "{:?}", results[1]);
    }

    #[test]
    fn level_is_measured_without_the_room_noise() {
        // The same sweep at two gains 6 dB apart, in noise that is not far below it: the level
        // must follow the gain, not the noise.
        let reference = calibration_sweep(RATE);
        let delay = 9_876.0;
        let level = |gain: f64| {
            let mut rec = noise(21, 36_000, 0.02); // −38.8 dBFS, just 4 dB under the quieter sweep
            for (i, x) in rec.iter_mut().enumerate() {
                *x += (gain * sweep_value((i as f64 - delay) / RATE as f64, 0.1, 200.0, 8000.0)) as f32;
            }
            find_signal(&rec, &reference).expect("detected")
        };
        let (loud, quiet) = (level(0.1), level(0.05));
        assert!(((loud.level_db - quiet.level_db) - 6.02).abs() < 0.3, "{} vs {}", loud.level_db, quiet.level_db);
        // A 0.5-amplitude sweep has a mean square of about 0.125 (-9 dB): at gain 0.1, -29 dBFS.
        assert!((loud.level_db - (-29.0)).abs() < 0.5, "level {}", loud.level_db);
        // Peak: the sweep's 0.05 plus at most 0.02 of noise.
        assert!((-26.1..=-23.0).contains(&loud.peak_dbfs), "peak {}", loud.peak_dbfs);
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
    fn wake_noise_is_a_soft_second_of_sound() {
        let w = wake_noise(RATE);
        assert_eq!(w.len(), RATE as usize, "one second");
        let peak = w.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.01 && peak <= WAKE_LEVEL, "peak {peak}");
        assert!(w[0].abs() < 1e-4 && w[w.len() - 1].abs() < 1e-3, "faded in and out");
        let middle = &w[RATE as usize / 10..RATE as usize * 9 / 10];
        let rms = (middle.iter().map(|x| x * x).sum::<f32>() / middle.len() as f32).sqrt();
        assert!(rms > WAKE_LEVEL / 4.0, "sound throughout, rms {rms}");
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
