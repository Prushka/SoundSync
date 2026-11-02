//! Virtual audio devices for tests.
//!
//! Every [`FakeHost`] has its own clock (offset + skew from the process-wide [`true_now`]) and
//! its own devices. Outputs consume frames at their nominal rate × (1 + ppm) in true time and
//! record the true emission time of every rising edge above 0.25 on channel 0. Sources stand in
//! for loopback-capturable render endpoints: they generate a test signal at their own drifting
//! rate, record the true capture time of each pulse, and (standing in for "Windows plays it
//! directly") record its emission `probe_offset + hidden` later.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::null::{ThreadStream, WakeTracker, open_null_output};
use crate::select::{DeviceSelector, select_device};
use crate::{
    AudioError, AudioHost, CaptureInfo, CaptureSink, DeviceEvent, DeviceInfo, Direction, EndpointVolume, LoopbackOptions, Nanos, OutputOptions,
    RenderCallback, RenderTiming, Stream, StreamFormat,
};

const ONSET_THRESHOLD: f32 = 0.25;

/// Process-wide reference ("true") time in ns; starts at 10^12 so offsets never go negative.
pub fn true_now() -> Nanos {
    static BASE: OnceLock<Instant> = OnceLock::new();
    BASE.get_or_init(Instant::now).elapsed().as_nanos() as Nanos + 1_000_000_000_000
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FakeClock {
    pub offset_ns: Nanos,
    pub skew_ppm: f64,
}

#[derive(Clone, Debug)]
pub struct FakeOutputSpec {
    pub name: String,
    pub format: StreamFormat,
    pub ppm: f64,
    /// Latency included in the dac times the device reports.
    pub reported_latency_ns: Nanos,
    /// Latency after the DAC that nobody reports (TV processing, Bluetooth, acoustics).
    pub hidden_latency_ns: Nanos,
    pub period_ns: Nanos,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FakeSignal {
    Silence,
    Pulses { every_ms: u32, amplitude: f32 },
    Sine { hz: f32, amplitude: f32 },
}

#[derive(Clone, Debug)]
pub struct FakeSourceSpec {
    pub name: String,
    pub format: StreamFormat,
    pub ppm: f64,
    pub signal: FakeSignal,
    /// Loopback timestamp → DAC for audio Windows plays directly on this device.
    pub probe_offset_ns: Nanos,
    pub hidden_latency_ns: Nanos,
}

/// Shared acoustic space: outputs of every host attached to it add their (true-time) emissions,
/// microphones hear the sum.
pub struct FakeRoom {
    rate: u32,
    t0: Nanos,
    buf: Mutex<Vec<f32>>,
}

const ROOM_MAX_SECONDS: usize = 600;

impl FakeRoom {
    pub fn new(sample_rate: u32) -> Arc<FakeRoom> {
        Arc::new(FakeRoom { rate: sample_rate, t0: true_now(), buf: Mutex::new(Vec::new()) })
    }

    fn frame_at(&self, t: Nanos) -> f64 {
        (t - self.t0) as f64 * self.rate as f64 / 1e9
    }

    fn index(&self, frame: f64) -> Option<usize> {
        (frame >= 0.0 && frame < (ROOM_MAX_SECONDS * self.rate as usize) as f64).then_some(frame as usize)
    }

    // Keep the stream's origin and frame count separate until indexing the room. At 48 kHz a
    // frame lasts 20,833 1/3 ns: truncating each timestamp to ns first can merge adjacent samples
    // and leave holes, changing a speaker's measured level even when no device underran.
    fn add(&self, start: Nanos, first: i64, rate: f64, samples: &[f32], channels: usize) {
        let origin = self.frame_at(start);
        let step = self.rate as f64 / rate;
        let mut buf = self.buf.lock().unwrap();
        for (k, frame) in samples.chunks_exact(channels).enumerate() {
            if let Some(i) = self.index(origin + (first + k as i64) as f64 * step) {
                if i >= buf.len() {
                    buf.resize(i + self.rate as usize, 0.0);
                }
                buf[i] += frame[0];
            }
        }
    }

    fn read(&self, start: Nanos, first: u64, out: &mut [f32]) {
        let origin = self.frame_at(start);
        let buf = self.buf.lock().unwrap();
        for (k, x) in out.iter_mut().enumerate() {
            *x = self.index(origin + (first + k as u64) as f64).and_then(|i| buf.get(i).copied()).unwrap_or(0.0);
        }
    }
}

#[derive(Clone, Debug)]
struct TimeMap {
    clock: FakeClock,
    t0: Nanos,
    /// Sudden steps of the host clock so far (see [`FakeHost::jump_clock`]).
    jump: Arc<AtomicI64>,
}

impl TimeMap {
    fn host(&self, true_t: Nanos) -> Nanos {
        true_t + self.clock.offset_ns + self.jump.load(Ordering::Relaxed) + (self.clock.skew_ppm * 1e-6 * (true_t - self.t0) as f64).round() as Nanos
    }
}

type Watcher = Arc<dyn Fn(DeviceEvent) + Send + Sync>;

#[derive(Default)]
struct Inner {
    outputs: Vec<FakeOutputSpec>,
    sources: Vec<FakeSourceSpec>,
    onsets: HashMap<String, Vec<Nanos>>,
    pulses: HashMap<String, Vec<Nanos>>,
    underruns: HashMap<String, Vec<Nanos>>,
    output_gaps: HashMap<String, Vec<std::ops::Range<Nanos>>>,
    output_stalls: HashMap<String, Duration>,
    sessions: HashMap<String, usize>,
    room: Option<Arc<FakeRoom>>,
    /// (name, hidden input latency)
    mics: Vec<(String, Nanos)>,
    /// Unplugged devices: not listed, cannot be opened, their streams end.
    removed: HashSet<String>,
    /// The default render device (None: the first one).
    default_render: Option<String>,
    /// Devices' own volume (slider 0..=1, mute); unset means 100% and unmuted.
    volumes: HashMap<String, (f32, bool)>,
    /// Outputs that mute after silence: (silence before muting, time to unmute), ns.
    sleepy: HashMap<String, (Nanos, Nanos)>,
    watchers: Vec<(u64, Watcher)>,
    next_watcher: u64,
}

/// Unregisters a device watcher when dropped.
struct WatchGuard {
    inner: Arc<Mutex<Inner>>,
    id: u64,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        self.inner.lock().unwrap().watchers.retain(|(id, _)| *id != self.id);
    }
}

/// Sleep of the simulated device threads. `SS_FAKE_MIN_SLEEP_MS` makes every sleep at least that
/// long, reproducing machines whose timers fire late (GitHub's macOS runners sleep ~20 ms).
fn device_sleep(d: Duration) {
    static MIN: OnceLock<Duration> = OnceLock::new();
    let min = *MIN.get_or_init(|| {
        std::env::var("SS_FAKE_MIN_SLEEP_MS").ok().and_then(|v| v.parse().ok()).map_or(Duration::ZERO, Duration::from_millis)
    });
    std::thread::sleep(d.max(min));
}

/// The fake devices' volume curve, like Windows' (50% is about -10.5 dB): code must use dB.
fn fake_db(scalar: f32) -> f32 {
    if scalar <= 0.0 { -96.0 } else { (35.0 * scalar.log10()).max(-96.0) }
}

/// What a device's own volume does to the sound it plays (1.0 at 100%, 0 when muted).
fn device_gain(inner: &Mutex<Inner>, name: &str) -> f32 {
    match inner.lock().unwrap().volumes.get(name) {
        Some(&(scalar, muted)) => EndpointVolume { scalar, db: fake_db(scalar), muted }.gain(),
        None => 1.0,
    }
}

/// A sink that mutes after a stretch of digital silence and takes a while to unmute once sound
/// comes back, losing that sound (TVs and soundbars do this): see [`FakeHost::make_sleepy`].
struct SleepySink {
    sleep_after: f64,
    wake: f64,
    silent_for: f64,
    asleep: bool,
    /// Time left until a waking sink plays again.
    waking: Option<f64>,
}

impl SleepySink {
    fn new((sleep_after, wake): (Nanos, Nanos)) -> Self {
        SleepySink { sleep_after: sleep_after as f64, wake: wake as f64, silent_for: 0.0, asleep: false, waking: None }
    }

    /// Plays or mutes one frame lasting `dt` ns.
    fn frame(&mut self, frame: &mut [f32], dt: f64) {
        let sound = frame.iter().any(|x| *x != 0.0);
        if self.asleep {
            match &mut self.waking {
                None if sound => self.waking = Some(self.wake - dt),
                None => {}
                Some(left) => *left -= dt,
            }
            if self.waking.is_none_or(|left| left > 0.0) {
                frame.fill(0.0);
                return;
            }
            (self.asleep, self.waking, self.silent_for) = (false, None, 0.0);
        }
        self.silent_for = if sound { 0.0 } else { self.silent_for + dt };
        self.asleep = self.silent_for >= self.sleep_after;
    }
}

fn device_removed(inner: &Mutex<Inner>, name: &str) -> bool {
    inner.lock().unwrap().removed.contains(name)
}

pub struct FakeHost {
    time: TimeMap,
    inner: Arc<Mutex<Inner>>,
}

fn device_info(name: &str, format: StreamFormat, direction: Direction, is_default: bool) -> DeviceInfo {
    DeviceInfo {
        id: format!("fake:{name}"),
        name: name.to_string(),
        direction,
        is_default,
        active: true,
        mix_format: format,
        default_period_ns: 10_000_000,
        min_period_ns: 10_000_000,
    }
}

impl FakeHost {
    pub fn new(clock: FakeClock) -> Arc<FakeHost> {
        Arc::new(FakeHost { time: TimeMap { clock, t0: true_now(), jump: Arc::default() }, inner: Arc::default() })
    }

    /// Unplugs a device: streams on it end (`is_alive()` turns false) and watchers hear `Removed`.
    pub fn remove_device(&self, name: &str) {
        self.inner.lock().unwrap().removed.insert(name.to_string());
        self.notify(DeviceEvent::Removed(format!("fake:{name}")));
    }

    /// Plugs a removed device back in; watchers hear `Added`.
    pub fn readd_device(&self, name: &str) {
        self.inner.lock().unwrap().removed.remove(name);
        self.notify(DeviceEvent::Added(format!("fake:{name}")));
    }

    /// Changes a source's format (like "Configure speakers"); pair with remove/readd to end the
    /// streams opened with the old format, as Windows does.
    pub fn set_source_format(&self, name: &str, format: StreamFormat) {
        for s in self.inner.lock().unwrap().sources.iter_mut().filter(|s| s.name == name) {
            s.format = format;
        }
    }

    /// Makes `name` the default render device (what "Default" resolves to).
    pub fn set_default_render(&self, name: &str) {
        self.inner.lock().unwrap().default_render = Some(name.to_string());
        self.notify(DeviceEvent::DefaultChanged(Direction::Render));
    }

    /// Steps this host's clock by `ns` at once (what a sleeping machine looks like to its peers).
    pub fn jump_clock(&self, ns: Nanos) {
        self.time.jump.fetch_add(ns, Ordering::Relaxed);
    }

    fn notify(&self, e: DeviceEvent) {
        let watchers: Vec<Watcher> = self.inner.lock().unwrap().watchers.iter().map(|(_, w)| w.clone()).collect();
        for w in watchers {
            w(e.clone());
        }
    }

    pub fn add_output(&self, s: FakeOutputSpec) {
        self.inner.lock().unwrap().outputs.push(s);
    }

    pub fn add_source(&self, s: FakeSourceSpec) {
        self.inner.lock().unwrap().sources.push(s);
    }

    /// Outputs of this host add their emissions to `room`; its mics listen to it.
    pub fn attach_room(&self, room: Arc<FakeRoom>) {
        self.inner.lock().unwrap().room = Some(room);
    }

    /// A microphone hearing the attached room; its timestamps run `hidden_input_latency_ns` late.
    pub fn add_mic(&self, name: &str, hidden_input_latency_ns: Nanos) {
        self.inner.lock().unwrap().mics.push((name.to_string(), hidden_input_latency_ns));
    }

    /// Makes `device` mute after `after_silence_ns` of digital silence and lose the first
    /// `wake_ns` of sound that follows (like a TV or soundbar).
    pub fn make_sleepy(&self, device: &str, after_silence_ns: Nanos, wake_ns: Nanos) {
        self.inner.lock().unwrap().sleepy.insert(device.to_string(), (after_silence_ns, wake_ns));
    }

    pub fn set_active_sessions(&self, device: &str, n: usize) {
        self.inner.lock().unwrap().sessions.insert(device.to_string(), n);
    }

    /// True emission times recorded on `device` so far.
    pub fn onsets(&self, device: &str) -> Vec<Nanos> {
        self.inner.lock().unwrap().onsets.get(device).cloned().unwrap_or_default()
    }

    /// True times at which `device` underran so far: its thread woke too late to keep the buffer
    /// filled (a slow machine stalled it), so the audio meant for that moment never played.
    pub fn underruns(&self, device: &str) -> Vec<Nanos> {
        self.inner.lock().unwrap().underruns.get(device).cloned().unwrap_or_default()
    }

    /// Exact true emission intervals skipped when the simulated output thread missed its
    /// buffer deadline. Includes the device's reported and hidden latency, unlike wake times.
    pub fn output_gaps(&self, device: &str) -> Vec<std::ops::Range<Nanos>> {
        self.inner.lock().unwrap().output_gaps.get(device).cloned().unwrap_or_default()
    }

    /// Stalls this output's thread once, to exercise recovery from a missed device callback.
    pub fn stall_output(&self, device: &str, duration: Duration) {
        self.inner.lock().unwrap().output_stalls.insert(device.to_string(), duration);
    }

    /// True capture times of pulses generated by `source` so far.
    pub fn source_pulses(&self, source: &str) -> Vec<Nanos> {
        self.inner.lock().unwrap().pulses.get(source).cloned().unwrap_or_default()
    }

    fn render_devices(&self) -> Vec<DeviceInfo> {
        let g = self.inner.lock().unwrap();
        let outs = g.outputs.iter().map(|o| (o.name.as_str(), o.format));
        let srcs = g.sources.iter().map(|s| (s.name.as_str(), s.format));
        let listed: Vec<(&str, StreamFormat)> = outs.chain(srcs).filter(|(n, _)| !g.removed.contains(*n)).collect();
        let default = g.default_render.as_deref().filter(|d| listed.iter().any(|(n, _)| n == d)).or(listed.first().map(|(n, _)| *n));
        listed.iter().map(|(n, f)| device_info(n, *f, Direction::Render, Some(*n) == default)).collect()
    }

    fn resolve(&self, sel: &DeviceSelector) -> Result<String, AudioError> {
        let devices = self.render_devices();
        Ok(select_device(&devices, sel)?.name.clone())
    }

    fn output_spec(&self, name: &str) -> Option<FakeOutputSpec> {
        let g = self.inner.lock().unwrap();
        g.outputs.iter().find(|o| o.name == name).cloned().or_else(|| {
            // Sources can be rendered to as well (e.g. a keep-alive or a direct-mode output).
            g.sources.iter().find(|s| s.name == name).map(|s| FakeOutputSpec {
                name: s.name.clone(),
                format: s.format,
                ppm: s.ppm,
                reported_latency_ns: s.probe_offset_ns,
                hidden_latency_ns: s.hidden_latency_ns,
                period_ns: 10_000_000,
            })
        })
    }

    fn source_spec(&self, name: &str) -> Option<FakeSourceSpec> {
        self.inner.lock().unwrap().sources.iter().find(|s| s.name == name).cloned()
    }
}

/// Rising edges above the onset threshold, interpolated to sub-frame time.
struct OnsetDetector {
    prev: f32,
}

impl OnsetDetector {
    fn feed(&mut self, x: f32, t_prev: f64, t_now: f64) -> Option<Nanos> {
        let hit = self.prev <= ONSET_THRESHOLD && x > ONSET_THRESHOLD;
        let at = hit.then(|| (t_prev + (ONSET_THRESHOLD - self.prev) as f64 / (x - self.prev) as f64 * (t_now - t_prev)) as Nanos);
        self.prev = x;
        at
    }
}

fn run_output(spec: FakeOutputSpec, time: TimeMap, inner: Arc<Mutex<Inner>>, mut cb: Box<dyn RenderCallback>, stop: Arc<AtomicBool>) {
    let fmt = spec.format;
    let ch = fmt.channels as usize;
    let dev_rate = fmt.sample_rate as f64 * (1.0 + spec.ppm * 1e-6);
    let period = ((spec.period_ns as f64 * dev_rate / 1e9).round() as i64).max(1);
    let mut buf = vec![0.0f32; (period as usize * 16) * ch];
    let t_start = true_now() + spec.period_ns;
    let nap = Duration::from_nanos((spec.period_ns / 4).max(500_000) as u64);
    let mut wake = WakeTracker::new(nap);
    let frame_true = |w: i64| t_start as f64 + w as f64 * 1e9 / dev_rate;
    let mut written: i64 = 0;
    let mut det = OnsetDetector { prev: 0.0 };
    let mut found = Vec::new();
    let room = inner.lock().unwrap().room.clone();
    let mut sleepy = inner.lock().unwrap().sleepy.get(&spec.name).copied().map(SleepySink::new);
    while !stop.load(Ordering::Acquire) && !device_removed(&inner, &spec.name) {
        let stall = inner.lock().unwrap().output_stalls.remove(&spec.name);
        if let Some(duration) = stall {
            std::thread::sleep(duration);
        }
        let now = true_now();
        let played = ((now - t_start) as f64 * dev_rate / 1e9).floor() as i64;
        if played > written {
            let shift = (spec.reported_latency_ns + spec.hidden_latency_ns) as f64;
            let gap = (frame_true(written) + shift).round() as Nanos..(frame_true(played) + shift).round() as Nanos;
            let mut g = inner.lock().unwrap();
            g.output_gaps.entry(spec.name.clone()).or_default().push(gap);
            g.underruns.entry(spec.name.clone()).or_default().push(now);
            written = played; // underrun: the device moved on without us
        }
        // At least two periods buffered, more if this thread wakes up late (it is a simulation:
        // a real device's buffer is serviced by its own callback, not by our sleeps).
        let target = played + (2 * period).max((wake.lead_ns() * dev_rate / 1e9) as i64);
        if target > written {
            let n = ((target - written) as usize).min(buf.len() / ch);
            let out = &mut buf[..n * ch];
            out.fill(0.0);
            let dac = time.host(frame_true(written).round() as Nanos) + spec.reported_latency_ns;
            cb.render(out, fmt, RenderTiming { now_ns: time.host(now), dac_time_ns: dac });
            let gain = device_gain(&inner, &spec.name); // the device's own volume, as Windows applies it
            if gain != 1.0 {
                out.iter_mut().for_each(|x| *x *= gain);
            }
            if let Some(sink) = &mut sleepy {
                out.chunks_exact_mut(ch).for_each(|f| sink.frame(f, 1e9 / dev_rate));
            }
            let shift = (spec.reported_latency_ns + spec.hidden_latency_ns) as f64;
            for j in 0..n {
                let w = written + j as i64;
                if let Some(t) = det.feed(out[j * ch], frame_true(w - 1) + shift, frame_true(w) + shift) {
                    found.push(t);
                }
            }
            if let Some(r) = &room {
                r.add(t_start + spec.reported_latency_ns + spec.hidden_latency_ns, written, dev_rate, out, ch);
            }
            if !found.is_empty() {
                inner.lock().unwrap().onsets.entry(spec.name.clone()).or_default().append(&mut found);
            }
            written += n as i64;
        }
        wake.sleep(nap, device_sleep);
    }
}

fn run_source(spec: FakeSourceSpec, time: TimeMap, inner: Arc<Mutex<Inner>>, mut sink: Box<dyn CaptureSink>, stop: Arc<AtomicBool>) {
    let fmt = spec.format;
    let ch = fmt.channels as usize;
    let rate = fmt.sample_rate as f64 * (1.0 + spec.ppm * 1e-6);
    let chunk = (fmt.sample_rate / 100) as u64;
    let pulse_every = match spec.signal {
        FakeSignal::Pulses { every_ms, .. } => (every_ms as u64 * fmt.sample_rate as u64 / 1000).max(1),
        _ => 0,
    };
    let mut buf = vec![0.0f32; chunk as usize * ch];
    let t_start = true_now();
    let frame_true = |k: u64| t_start as f64 + k as f64 * 1e9 / rate;
    let mut produced: u64 = 0;
    while !stop.load(Ordering::Acquire) && !device_removed(&inner, &spec.name) {
        let now = true_now() as f64;
        while frame_true(produced + chunk) <= now && !stop.load(Ordering::Acquire) {
            let mut pulses = Vec::new();
            for i in 0..chunk {
                let k = produced + i;
                let x = match spec.signal {
                    FakeSignal::Silence => 0.0,
                    FakeSignal::Pulses { amplitude, .. } => {
                        if k > 0 && k.is_multiple_of(pulse_every) {
                            pulses.push(frame_true(k).round() as Nanos);
                            amplitude
                        } else {
                            0.0
                        }
                    }
                    FakeSignal::Sine { hz, amplitude } => amplitude * (std::f64::consts::TAU * hz as f64 * k as f64 / fmt.sample_rate as f64).sin() as f32,
                };
                buf[i as usize * ch..(i as usize + 1) * ch].fill(x);
            }
            if !pulses.is_empty() {
                let mut g = inner.lock().unwrap();
                let shift = spec.probe_offset_ns + spec.hidden_latency_ns;
                g.onsets.entry(spec.name.clone()).or_default().extend(pulses.iter().map(|p| p + shift));
                g.pulses.entry(spec.name.clone()).or_default().extend(pulses);
            }
            let t = time.host(frame_true(produced).round() as Nanos);
            let info = CaptureInfo {
                time_ns: t,
                raw_time_ns: t,
                frames: chunk as usize,
                silent: spec.signal == FakeSignal::Silence,
                discontinuity: false,
            };
            sink.on_capture(&buf, fmt, info);
            produced += chunk;
        }
        device_sleep(Duration::from_millis(1));
    }
}

fn run_mic(name: String, hidden: Nanos, room: Arc<FakeRoom>, time: TimeMap, inner: Arc<Mutex<Inner>>, mut sink: Box<dyn CaptureSink>, stop: Arc<AtomicBool>) {
    let fmt = StreamFormat { sample_rate: room.rate, channels: 1, channel_mask: 0x4 };
    let chunk = (fmt.sample_rate / 100) as u64;
    let mut buf = vec![0.0f32; chunk as usize];
    let t_start = true_now();
    let frame_true = |k: u64| t_start + (k as i128 * 1_000_000_000 / fmt.sample_rate as i128) as Nanos;
    let mut produced = 0u64;
    while !stop.load(Ordering::Acquire) && !device_removed(&inner, &name) {
        // Outputs write ahead of real time, so the room is complete up to "now".
        while frame_true(produced + chunk) <= true_now() - 5_000_000 && !stop.load(Ordering::Acquire) {
            room.read(t_start - hidden, produced, &mut buf);
            let t = time.host(frame_true(produced));
            sink.on_capture(&buf, fmt, CaptureInfo { time_ns: t, raw_time_ns: t, frames: chunk as usize, silent: false, discontinuity: false });
            produced += chunk;
        }
        device_sleep(Duration::from_millis(1));
    }
}

impl AudioHost for FakeHost {
    fn now_ns(&self) -> Nanos {
        self.time.host(true_now())
    }

    fn devices(&self, dir: Direction) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(match dir {
            Direction::Render => self.render_devices(),
            Direction::Capture => {
                let g = self.inner.lock().unwrap();
                let rate = g.room.as_ref().map_or(48_000, |r| r.rate);
                let fmt = StreamFormat { sample_rate: rate, channels: 1, channel_mask: 0x4 };
                g.mics.iter().filter(|(n, _)| !g.removed.contains(n)).enumerate().map(|(i, (n, _))| device_info(n, fmt, Direction::Capture, i == 0)).collect()
            }
        })
    }

    fn open_output(&self, sel: &DeviceSelector, _opts: OutputOptions, cb: Box<dyn RenderCallback>) -> Result<Stream, AudioError> {
        if *sel == DeviceSelector::Null {
            let time = self.time.clone();
            let fmt = StreamFormat { sample_rate: 48_000, channels: 2, channel_mask: 0x3 };
            return Ok(open_null_output(Arc::new(move || time.host(true_now())), fmt, cb));
        }
        let name = self.resolve(sel)?;
        let spec = self.output_spec(&name).ok_or(AudioError::NotFound(name))?;
        let (time, inner) = (self.time.clone(), self.inner.clone());
        let (fmt, latency) = (spec.format, spec.reported_latency_ns);
        Ok(Box::new(ThreadStream::spawn("fake-output", fmt, latency, move |stop, _| run_output(spec, time, inner, cb, stop))))
    }

    fn open_loopback(&self, sel: &DeviceSelector, _opts: LoopbackOptions, sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError> {
        let name = self.resolve(sel)?;
        let spec = self.source_spec(&name).ok_or(AudioError::Unsupported("fake loopback needs a source device"))?;
        let (time, inner) = (self.time.clone(), self.inner.clone());
        let fmt = spec.format;
        Ok(Box::new(ThreadStream::spawn("fake-loopback", fmt, 0, move |stop, _| run_source(spec, time, inner, sink, stop))))
    }

    fn open_input(&self, sel: &DeviceSelector, sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError> {
        let devices = self.devices(Direction::Capture)?;
        let name = select_device(&devices, sel)?.name.clone();
        let (hidden, room) = {
            let g = self.inner.lock().unwrap();
            let hidden = g.mics.iter().find(|(n, _)| *n == name).map(|(_, h)| *h).unwrap_or(0);
            (hidden, g.room.clone().ok_or(AudioError::Unsupported("fake mic needs a room"))?)
        };
        let (time, inner) = (self.time.clone(), self.inner.clone());
        let fmt = StreamFormat { sample_rate: room.rate, channels: 1, channel_mask: 0x4 };
        Ok(Box::new(ThreadStream::spawn("fake-mic", fmt, 0, move |stop, _| run_mic(name, hidden, room, time, inner, sink, stop))))
    }

    fn probe_loopback_latency(&self, sel: &DeviceSelector) -> Result<Nanos, AudioError> {
        let name = self.resolve(sel)?;
        self.source_spec(&name).map(|s| s.probe_offset_ns).ok_or(AudioError::Unsupported("not a loopback source"))
    }

    fn endpoint_volume(&self, sel: &DeviceSelector) -> Result<EndpointVolume, AudioError> {
        let name = self.resolve(sel)?;
        let (scalar, muted) = self.inner.lock().unwrap().volumes.get(&name).copied().unwrap_or((1.0, false));
        Ok(EndpointVolume { scalar, db: fake_db(scalar), muted })
    }

    fn set_endpoint_volume(&self, sel: &DeviceSelector, scalar: f32) -> Result<(), AudioError> {
        let name = self.resolve(sel)?;
        self.inner.lock().unwrap().volumes.entry(name).or_insert((1.0, false)).0 = scalar.clamp(0.0, 1.0);
        Ok(())
    }

    fn set_endpoint_volume_db(&self, sel: &DeviceSelector, db: f32) -> Result<(), AudioError> {
        self.set_endpoint_volume(sel, 10f32.powf(db / 35.0)) // inverse of fake_db
    }

    fn set_endpoint_mute(&self, sel: &DeviceSelector, muted: bool) -> Result<(), AudioError> {
        let name = self.resolve(sel)?;
        self.inner.lock().unwrap().volumes.entry(name).or_insert((1.0, false)).1 = muted;
        Ok(())
    }

    fn active_sessions(&self, sel: &DeviceSelector) -> Result<usize, AudioError> {
        let name = self.resolve(sel)?;
        Ok(self.inner.lock().unwrap().sessions.get(&name).copied().unwrap_or(0))
    }

    fn watch(&self, on_event: Box<dyn Fn(DeviceEvent) + Send + Sync>) -> Result<Box<dyn Send>, AudioError> {
        let mut g = self.inner.lock().unwrap();
        let id = g.next_watcher;
        g.next_watcher += 1;
        g.watchers.push((id, Arc::from(on_event)));
        Ok(Box::new(WatchGuard { inner: self.inner.clone(), id }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    fn stereo() -> StreamFormat {
        StreamFormat { sample_rate: 48_000, channels: 2, channel_mask: 3 }
    }

    #[test]
    fn room_preserves_a_speakers_level_on_its_sample_boundaries() {
        // A 48 kHz frame lasts 20,833 1/3 ns. Rounding its timestamp to whole ns must
        // not put two speaker samples into one room sample and leave the next empty.
        for rate in [44_100, 48_000] {
            for offset in [0, 1, 10_417, 20_833, 20_834, 180_000_000] {
                let room = FakeRoom { rate, t0: 1_000_000_000_000, buf: Mutex::new(Vec::new()) };
                let start = room.t0 + offset;
                let samples: Vec<f32> = (0..480).map(|i| i as f32 / 1000.0).collect();
                // Different callback sizes must keep the same sample positions, too.
                for (k, chunk) in samples.chunks(17).enumerate() {
                    room.add(start, (k * 17) as i64, rate as f64, chunk, 1);
                }
                let mut heard = vec![0.0; samples.len()];
                for (k, chunk) in heard.chunks_mut(13).enumerate() {
                    room.read(start, (k * 13) as u64, chunk);
                }
                assert_eq!(heard, samples, "{rate} Hz, offset {offset} ns");
            }
        }
    }

    #[test]
    fn room_mixes_speakers_at_their_emission_times() {
        let room = FakeRoom { rate: 48_000, t0: 1_000_000_000_000, buf: Mutex::new(Vec::new()) };
        // Channel 0 of two stereo speakers, starting 1 ms apart; the mic starts at room time 0.
        let a = [0.25, -0.25].repeat(96);
        let b = [0.5, -0.5].repeat(96);
        room.add(room.t0 + 1_000_000, 0, 48_000.0, &a, 2);
        room.add(room.t0 + 2_000_000, 0, 48_000.0, &b, 2);
        let mut heard = vec![0.0; 240];
        room.read(room.t0, 0, &mut heard);
        assert_eq!(heard, [vec![0.0; 48], vec![0.25; 48], vec![0.75; 48], vec![0.5; 48], vec![0.0; 48]].concat());
        let mut before = [1.0; 48];
        room.read(room.t0 - 1_000_000, 0, &mut before);
        assert_eq!(before, [0.0; 48], "before the room's start is silence");
    }

    /// Records timings; blocks 50 ms on call number `.1` (a scheduling stall).
    struct Rec(Arc<Mutex<Vec<(RenderTiming, usize)>>>, Option<usize>);
    impl RenderCallback for Rec {
        fn render(&mut self, out: &mut [f32], format: StreamFormat, t: RenderTiming) {
            let calls = {
                let mut g = self.0.lock().unwrap();
                g.push((t, out.len() / format.channels as usize));
                g.len()
            };
            if Some(calls) == self.1 {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    #[test]
    fn sleepy_sink_mutes_after_silence_and_loses_the_start_of_sound() {
        let ms = 1e6;
        let mut sink = SleepySink::new((1_000_000_000, 300_000_000));
        let mut play = |x: f32, dur_ms: f64| (0..(dur_ms as usize)).map(|_| {
            let mut f = [x];
            sink.frame(&mut f, ms);
            f[0]
        }).collect::<Vec<f32>>();
        assert!(play(0.5, 100.0).iter().all(|x| *x == 0.5), "awake at first");
        play(0.0, 1_000.0);
        let after_sleep = play(0.5, 500.0);
        assert!(after_sleep[..299].iter().all(|x| *x == 0.0), "the first 300 ms are lost");
        assert!(after_sleep[300..].iter().all(|x| *x == 0.5), "then it plays");
        play(0.0, 500.0);
        assert!(play(0.5, 10.0).iter().all(|x| *x == 0.5), "a short pause does not put it to sleep");
    }

    #[test]
    fn device_volume_can_be_set_in_decibels() {
        let host = FakeHost::new(FakeClock::default());
        host.add_output(FakeOutputSpec { name: "TV".into(), format: stereo(), ppm: 0.0, reported_latency_ns: 0, hidden_latency_ns: 0, period_ns: 10_000_000 });
        let tv = DeviceSelector::Name("TV".into());
        host.set_endpoint_volume_db(&tv, -10.5).unwrap();
        let v = host.endpoint_volume(&tv).unwrap();
        assert!((v.db - -10.5).abs() < 0.01, "{v:?}");
        assert!((v.scalar - 0.5).abs() < 0.01, "the fake curve puts -10.5 dB at about 50%: {v:?}");
        host.set_endpoint_volume_db(&tv, 6.0).unwrap();
        assert_eq!(host.endpoint_volume(&tv).unwrap().scalar, 1.0, "clamped to the device's range");
    }

    #[test]
    fn a_stalled_output_reports_its_underrun() {
        // The audio meant for the stall never plays: tests that need every sample (calibration
        // sweeps) must be able to tell that the machine, not the code, lost it.
        let host = FakeHost::new(FakeClock::default());
        host.add_output(FakeOutputSpec { name: "TV".into(), format: stereo(), ppm: 0.0, reported_latency_ns: 0, hidden_latency_ns: 0, period_ns: 10_000_000 });
        let t0 = true_now();
        let stream = host.open_output(&DeviceSelector::Name("TV".into()), OutputOptions::default(), Box::new(Rec(Arc::default(), Some(20)))).unwrap();
        std::thread::sleep(Duration::from_millis(1000));
        drop(stream);
        let underruns = host.underruns("TV");
        assert!(!underruns.is_empty(), "the stall must show as an underrun");
        assert!(underruns.iter().all(|t| (t0..true_now()).contains(t)), "true times of the underruns: {underruns:?}");
    }

    #[test]
    fn fake_output_dac_times_follow_device_rate() {
        // The callback blocks 50 ms once (what an oversleeping CI machine does): the device
        // underruns and skips ahead, and between contiguous callbacks the DAC times still
        // advance at the device's own rate (100 ppm fast), not the nominal one.
        let host = FakeHost::new(FakeClock::default());
        host.add_output(FakeOutputSpec {
            name: "TV".into(),
            format: stereo(),
            ppm: 100.0,
            reported_latency_ns: 15_000_000,
            hidden_latency_ns: 0,
            period_ns: 10_000_000,
        });
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stream = host
            .open_output(&DeviceSelector::Name("TV".into()), OutputOptions::default(), Box::new(Rec(calls.clone(), Some(20))))
            .unwrap();
        std::thread::sleep(Duration::from_millis(1300));
        drop(stream);
        let calls = calls.lock().unwrap();
        let dev_rate = 48_000.0 * (1.0 + 100e-6);
        let (mut span, mut frames, mut skips) = (0.0f64, 0usize, 0);
        for w in calls.windows(2) {
            let (dt, n) = ((w[1].0.dac_time_ns - w[0].0.dac_time_ns) as f64, w[0].1);
            let residual = dt - n as f64 * 1e9 / dev_rate;
            assert!(residual > -1_000.0, "DAC time went backwards relative to the frames written: {residual} ns");
            if residual > 1_000.0 {
                skips += 1; // an underrun: the device moved on without us
            } else {
                span += dt;
                frames += n;
            }
        }
        assert!(skips >= 1, "the stall must have caused an underrun");
        assert!(frames >= 30_000, "frames {frames}");
        let at_device_rate = frames as f64 * 1e9 / dev_rate;
        let at_nominal_rate = frames as f64 * 1e9 / 48_000.0;
        assert!((span - at_device_rate).abs() < 2_000.0, "span {span} vs {at_device_rate} at the device rate");
        assert!((span - at_nominal_rate).abs() > 40_000.0, "the 100 ppm must show: span {span} vs {at_nominal_rate}");
        let (first, _) = calls[0];
        let lead = first.dac_time_ns - first.now_ns;
        assert!((15_000_000..=40_000_000).contains(&lead), "lead {lead} includes reported latency");
    }

    type Captured = Arc<Mutex<Vec<(CaptureInfo, Vec<f32>)>>>;
    struct Cap(Captured);
    impl CaptureSink for Cap {
        fn on_capture(&mut self, data: &[f32], _format: StreamFormat, info: CaptureInfo) {
            self.0.lock().unwrap().push((info, data.to_vec()));
        }
    }

    #[test]
    fn fake_source_pulses_at_known_capture_times() {
        let host = FakeHost::new(FakeClock::default());
        host.add_source(FakeSourceSpec {
            name: "CABLE Input".into(),
            format: stereo(),
            ppm: -50.0,
            signal: FakeSignal::Pulses { every_ms: 200, amplitude: 0.8 },
            probe_offset_ns: 20_000_000,
            hidden_latency_ns: 0,
        });
        let got = Arc::new(Mutex::new(Vec::new()));
        let stream = host
            .open_loopback(&DeviceSelector::Name("CABLE".into()), LoopbackOptions::default(), Box::new(Cap(got.clone())))
            .unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        drop(stream);
        let pulses = host.source_pulses("CABLE Input");
        assert!(pulses.len() >= 4, "pulses {}", pulses.len());
        let rate = 48_000.0 * (1.0 - 50e-6);
        let mut found = Vec::new();
        for (info, data) in got.lock().unwrap().iter() {
            for (i, frame) in data.as_chunks::<2>().0.iter().enumerate() {
                if frame[0] == 0.8 {
                    found.push(info.time_ns + (i as f64 * 1e9 / rate).round() as Nanos);
                }
            }
        }
        assert_eq!(found.len(), pulses.len());
        for (f, p) in found.iter().zip(&pulses) {
            assert!((f - p).abs() < 1_000, "captured pulse at {f}, generated at {p}");
        }
        // Direct-mode stand-in: the source "plays" each pulse probe_offset later.
        let emitted = host.onsets("CABLE Input");
        assert_eq!(emitted.len(), pulses.len());
        assert!((emitted[0] - pulses[0] - 20_000_000).abs() < 1_000);
        assert_eq!(host.probe_loopback_latency(&DeviceSelector::Name("CABLE".into())).unwrap(), 20_000_000);
    }

    #[test]
    fn removed_device_stops_its_streams_and_comes_back() {
        let host = FakeHost::new(FakeClock::default());
        host.add_output(FakeOutputSpec { name: "TV".into(), format: stereo(), ppm: 0.0, reported_latency_ns: 0, hidden_latency_ns: 0, period_ns: 10_000_000 });
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        let watch = host.watch(Box::new(move |e| seen.lock().unwrap().push(e))).unwrap();
        let tv = DeviceSelector::Name("TV".into());
        let stream = host.open_output(&tv, OutputOptions::default(), Box::new(Rec(Arc::default(), None))).unwrap();
        assert!(stream.is_alive());
        host.remove_device("TV");
        let deadline = Instant::now() + Duration::from_secs(1);
        while stream.is_alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!stream.is_alive(), "a stream on a removed device dies");
        assert!(host.devices(Direction::Render).unwrap().is_empty());
        assert!(matches!(host.open_output(&tv, OutputOptions::default(), Box::new(Rec(Arc::default(), None))), Err(AudioError::NotFound(_))));
        host.readd_device("TV");
        assert_eq!(host.devices(Direction::Render).unwrap().len(), 1);
        assert!(host.open_output(&tv, OutputOptions::default(), Box::new(Rec(Arc::default(), None))).unwrap().is_alive());
        assert_eq!(*events.lock().unwrap(), vec![DeviceEvent::Removed("fake:TV".into()), DeviceEvent::Added("fake:TV".into())]);
        drop(watch);
        host.remove_device("TV");
        assert_eq!(events.lock().unwrap().len(), 2, "no events after the watch is dropped");
    }

    #[test]
    fn jump_clock_moves_host_time() {
        let host = FakeHost::new(FakeClock::default());
        let before = host.now_ns() - true_now();
        host.jump_clock(30_000_000_000);
        let after = host.now_ns() - true_now();
        assert!((after - before - 30_000_000_000).abs() < 1_000_000, "jumped by {} ns", after - before);
    }

    #[test]
    fn fake_clock_offset_applies() {
        let host = FakeHost::new(FakeClock { offset_ns: 7_000_000, skew_ppm: 0.0 });
        let diff = host.now_ns() - true_now();
        assert!((diff - 7_000_000).abs() < 100_000, "diff {diff}");
        let skewed = FakeHost::new(FakeClock { offset_ns: 0, skew_ppm: 1000.0 });
        let a = (skewed.now_ns(), true_now());
        std::thread::sleep(Duration::from_millis(200));
        let b = (skewed.now_ns(), true_now());
        let ratio = (b.0 - a.0) as f64 / (b.1 - a.1) as f64;
        assert!((ratio - 1.001).abs() < 1e-4, "ratio {ratio}");
    }

    #[test]
    fn devices_lists_outputs_and_sources() {
        let host = FakeHost::new(FakeClock::default());
        host.add_output(FakeOutputSpec { name: "TV".into(), format: stereo(), ppm: 0.0, reported_latency_ns: 0, hidden_latency_ns: 0, period_ns: 10_000_000 });
        host.add_source(FakeSourceSpec {
            name: "CABLE Input".into(),
            format: stereo(),
            ppm: 0.0,
            signal: FakeSignal::Silence,
            probe_offset_ns: 0,
            hidden_latency_ns: 0,
        });
        let names: Vec<String> = host.devices(Direction::Render).unwrap().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["TV".to_string(), "CABLE Input".to_string()]);
        assert!(host.devices(Direction::Capture).unwrap().is_empty());
        host.set_active_sessions("CABLE Input", 2);
        assert_eq!(host.active_sessions(&DeviceSelector::Name("CABLE Input".into())).unwrap(), 2);
        assert!(matches!(host.open_output(&DeviceSelector::Name("nope".into()), OutputOptions::default(), Box::new(Rec(Arc::default(), None))), Err(AudioError::NotFound(_))));
    }
}
