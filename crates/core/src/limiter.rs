//! Output-stage soft limiter: an exact no-op below 0 dBFS, instant gain reduction above it,
//! and a 50 ms release back to unity.

const RELEASE_S: f32 = 0.050;
const SNAP: f32 = 1e-6;

/// Gain reduction is tracked as `1 − gain` so the release keeps full f32 precision near unity
/// (tracking `gain` itself stalls a few ulps below 1.0 and never becomes bit-exact again).
#[derive(Clone, Debug)]
pub struct SoftLimiter {
    reduction: f32,
    release_coeff: f32,
}

impl SoftLimiter {
    pub fn new(sample_rate: u32) -> Self {
        Self { reduction: 0.0, release_coeff: (-1.0 / (RELEASE_S * sample_rate as f32)).exp() }
    }

    pub fn process(&mut self, buf: &mut [f32], channels: usize) {
        for frame in buf.chunks_exact_mut(channels.max(1)) {
            let peak = frame.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            let needed = if peak > 1.0 { 1.0 - 1.0 / peak } else { 0.0 };
            self.reduction = needed.max(self.reduction * self.release_coeff);
            if self.reduction < SNAP {
                self.reduction = 0.0;
                continue;
            }
            let gain = 1.0 - self.reduction;
            for x in frame.iter_mut() {
                *x = (*x * gain).clamp(-1.0, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn limiter_is_bit_exact_below_full_scale() {
        let mut lim = SoftLimiter::new(48_000);
        let input = noise(5, 48_000 * 2, 1.0);
        let mut buf = input.clone();
        lim.process(&mut buf, 2);
        assert_eq!(buf.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), input.iter().map(|x| x.to_bits()).collect::<Vec<_>>());
    }

    #[test]
    fn limiter_caps_peaks() {
        let mut lim = SoftLimiter::new(48_000);
        let mut buf: Vec<f32> = (0..48_000 * 2)
            .map(|i| {
                let t = (i / 2) as f32 / 48_000.0;
                1.8 * (t * 440.0 * std::f32::consts::TAU).sin() + 0.9 * (t * 97.0 * std::f32::consts::TAU).sin()
            })
            .collect();
        lim.process(&mut buf, 2);
        let peak = buf.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak <= 1.0, "peak {peak}");
        assert!(peak > 0.9, "limiter should not crush the signal: peak {peak}");
    }

    #[test]
    fn limiter_releases_to_unity() {
        let mut lim = SoftLimiter::new(48_000);
        let mut loud = vec![2.0f32, -2.0];
        lim.process(&mut loud, 2);
        assert!(loud[0] <= 1.0);
        let quiet = vec![0.5f32; 48_000 * 2];
        let mut buf = quiet.clone();
        lim.process(&mut buf, 2);
        let tail = 4_800 * 2;
        assert!(buf[..2].iter().all(|&x| x < 0.5), "gain still reduced right after the peak");
        assert_eq!(&buf[buf.len() - tail..], &quiet[quiet.len() - tail..]);
    }
}
