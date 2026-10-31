//! Loopback measurements.
//!
//! * [`probe_loopback`]: how loopback timestamps relate to DAC time on a device, and whether
//!   loopback is bit-exact, by rendering a known burst through our own render path and finding
//!   it in the loopback capture.
//! * [`measure_pair`]: relative delay between what two endpoints play.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use soundsync_core::calib::find_signal;

use crate::{
    AudioError, AudioHost, CaptureInfo, CaptureSink, DeviceSelector, LoopbackOptions, Nanos, OutputOptions, RenderCallback, RenderTiming,
    StreamFormat,
};

const BURST_FRAMES: usize = 4096;
const BURST_AMPLITUDE: f32 = 0.25;
/// Several bursts; the median offset rejects a one-off render start-up glitch.
const BURSTS: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProbeResult {
    /// DAC time − loopback capture time of the same sample.
    pub offset_ns: Nanos,
    /// Captured samples equal the rendered ones exactly.
    pub bit_exact: bool,
    /// RMS(captured) / RMS(rendered): ≠ 1 means endpoint volume is applied before loopback.
    pub gain: f32,
}

fn burst(seed: u64) -> Vec<f32> {
    let mut s: u64 = 0x5EED_1234_ABCD_9876 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (0..BURST_FRAMES)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            ((s >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32 * BURST_AMPLITUDE
        })
        .collect()
}

/// Plays each signal (mono, copied to every channel) once at its start frame and records the
/// DAC time of each start frame.
struct BurstPlayer {
    bursts: Vec<(u64, Vec<f32>)>,
    frame: u64,
    dac_of_start: Arc<Mutex<Vec<Option<Nanos>>>>,
}

impl RenderCallback for BurstPlayer {
    fn render(&mut self, out: &mut [f32], format: StreamFormat, t: RenderTiming) {
        let ch = format.channels as usize;
        let frames = (out.len() / ch) as u64;
        for (b, (start, signal)) in self.bursts.iter().enumerate() {
            if *start >= self.frame && *start < self.frame + frames {
                let dac = t.dac_time_ns + ((start - self.frame) as i128 * 1_000_000_000 / format.sample_rate as i128) as Nanos;
                self.dac_of_start.lock().unwrap()[b] = Some(dac);
            }
            for (i, frame) in out.chunks_exact_mut(ch).enumerate() {
                let k = self.frame + i as u64;
                if k >= *start && k < start + signal.len() as u64 {
                    frame.fill(signal[(k - start) as usize]);
                }
            }
        }
        self.frame += frames;
    }
}

#[derive(Default)]
struct Recording {
    format: Option<StreamFormat>,
    /// (capture time of first frame, interleaved samples)
    chunks: Vec<(Nanos, Vec<f32>)>,
}

/// Capture sink that keeps everything it receives.
#[derive(Clone, Default)]
pub struct Recorder(Arc<Mutex<Recording>>);

impl CaptureSink for Recorder {
    fn on_capture(&mut self, data: &[f32], format: StreamFormat, info: CaptureInfo) {
        let mut r = self.0.lock().unwrap();
        r.format = Some(format);
        r.chunks.push((info.time_ns, data.to_vec()));
    }
}

impl Recorder {
    pub fn new() -> Recorder {
        Recorder(Arc::default())
    }

    /// Channel-0 samples laid out contiguously, plus the capture time of each sample index
    /// (from the chunk it came from).
    pub fn channel0(&self) -> (Vec<f32>, Vec<Nanos>, Option<StreamFormat>) {
        let r = self.0.lock().unwrap();
        let Some(fmt) = r.format else { return (Vec::new(), Vec::new(), None) };
        let ch = fmt.channels as usize;
        let mut samples = Vec::new();
        let mut times = Vec::new();
        for (t0, data) in &r.chunks {
            for (i, frame) in data.chunks_exact(ch).enumerate() {
                samples.push(frame[0]);
                times.push(t0 + (i as i128 * 1_000_000_000 / fmt.sample_rate as i128) as Nanos);
            }
        }
        (samples, times, Some(fmt))
    }

    pub fn all_channels(&self) -> Vec<Vec<f32>> {
        let r = self.0.lock().unwrap();
        let Some(fmt) = r.format else { return Vec::new() };
        let ch = fmt.channels as usize;
        let mut out = vec![Vec::new(); ch];
        for (_, data) in &r.chunks {
            for frame in data.chunks_exact(ch) {
                for (c, &x) in frame.iter().enumerate() {
                    out[c].push(x);
                }
            }
        }
        out
    }
}

pub fn probe_loopback(host: &dyn AudioHost, sel: &DeviceSelector) -> Result<ProbeResult, AudioError> {
    // Our render stream starts first and doubles as the keep-alive: a second render client
    // joining the endpoint later can glitch it (seen as an 8 ms discontinuity on Windows), which
    // would shift the render clock relative to the loopback timestamps for the whole session.
    let rate = host.devices(crate::Direction::Render)?.iter().find(|d| crate::select_device(std::slice::from_ref(d), sel).is_ok()).map_or(48_000, |d| d.mix_format.sample_rate) as u64;
    let bursts: Vec<(u64, Vec<f32>)> = (0..BURSTS).map(|k| (rate * 6 / 10 + k as u64 * rate * 15 / 100, burst(k as u64))).collect();
    let dac_of_start = Arc::new(Mutex::new(vec![None; BURSTS]));
    let player = BurstPlayer { bursts: bursts.clone(), frame: 0, dac_of_start: dac_of_start.clone() };
    let render = host.open_output(sel, OutputOptions::default(), Box::new(player))?;
    std::thread::sleep(Duration::from_millis(300));
    let rec = Recorder::new();
    let capture = host.open_loopback(sel, LoopbackOptions { no_keepalive: true, ..Default::default() }, Box::new(rec.clone()))?;
    std::thread::sleep(Duration::from_millis(1300));
    drop(render);
    std::thread::sleep(Duration::from_millis(100));
    drop(capture);

    let dacs = dac_of_start.lock().unwrap().clone();
    let (ch0, times, _) = rec.channel0();
    let channels = rec.all_channels();
    let rms = |v: &[f32]| (v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
    let mut offsets = Vec::new();
    let mut bit_exact = true;
    let mut gain = None;
    for ((_, signal), dac) in bursts.iter().zip(dacs) {
        let (Some(dac), Some(det)) = (dac, find_signal(&ch0, signal)) else { continue };
        let start = det.offset_frames.round() as usize;
        if start + signal.len() > ch0.len() {
            continue;
        }
        bit_exact &= channels.iter().all(|c| c[start..start + signal.len()].iter().zip(signal).all(|(a, b)| a.to_bits() == b.to_bits()));
        gain.get_or_insert((rms(&ch0[start..start + signal.len()]) / rms(signal)) as f32);
        offsets.push(dac - times[start]);
    }
    if offsets.len() < BURSTS / 2 + 1 {
        return Err(AudioError::Os(format!("probe bursts found in loopback capture: {} of {BURSTS}", offsets.len())));
    }
    offsets.sort_unstable();
    Ok(ProbeResult { offset_ns: offsets[offsets.len() / 2], bit_exact, gain: gain.unwrap_or(0.0) })
}

/// Index of the 200 ms window with the most energy.
fn loudest_window(samples: &[f32], len: usize) -> Option<usize> {
    if samples.len() < len || len == 0 {
        return None;
    }
    let mut energy: f64 = samples[..len].iter().map(|x| (*x as f64).powi(2)).sum();
    let (mut best, mut best_e) = (0, energy);
    for i in len..samples.len() {
        energy += (samples[i] as f64).powi(2) - (samples[i - len] as f64).powi(2);
        if energy > best_e {
            best_e = energy;
            best = i + 1 - len;
        }
    }
    (best_e > 1e-9).then_some(best)
}

/// Loopback-captures both endpoints for `seconds` and returns how much later B plays the loudest
/// 200 ms of A (negative: B is earlier). `None` when no common signal was found.
pub fn measure_pair(host: &dyn AudioHost, a: &DeviceSelector, b: &DeviceSelector, seconds: f64) -> Result<Option<Nanos>, AudioError> {
    let (ra, rb) = (Recorder::new(), Recorder::new());
    let sa = host.open_loopback(a, LoopbackOptions::default(), Box::new(ra.clone()))?;
    let sb = host.open_loopback(b, LoopbackOptions::default(), Box::new(rb.clone()))?;
    std::thread::sleep(Duration::from_secs_f64(seconds));
    drop((sa, sb));
    let (xa, ta, fa) = ra.channel0();
    let (xb, tb, _) = rb.channel0();
    let Some(fa) = fa else { return Ok(None) };
    let len = fa.sample_rate as usize / 5;
    let Some(start) = loudest_window(&xa, len) else { return Ok(None) };
    Ok(find_signal(&xb, &xa[start..start + len]).map(|d| tb[d.offset_frames.round() as usize] - ta[start]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loudest_window_finds_burst() {
        let mut x = vec![0.01f32; 10_000];
        for v in &mut x[6_000..6_500] {
            *v = 0.5;
        }
        let i = loudest_window(&x, 400).unwrap();
        assert!((6_000..=6_100).contains(&i), "{i}");
        assert_eq!(loudest_window(&[0.0; 1000], 100), None, "silence has no loudest window");
        assert_eq!(loudest_window(&x[..10], 100), None);
    }
}
