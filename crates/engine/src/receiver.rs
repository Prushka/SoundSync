//! The render callback: every received stream (and this node's own stream in virtual mode)
//! timed, resampled and layout-mapped by its own [`StreamPlayer`], summed, scaled by the node
//! volume and passed through the soft limiter. Scheduled one-shot signals (calibration sweeps,
//! sync-test clicks) are mixed in at exact DAC times.
//!
//! Realtime-safe: slots and signals are built on the engine thread and handed over through
//! rings; finished ones go back through the garbage ring to be freed there.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};

use arc_swap::ArcSwap;
use soundsync_audio::{RenderCallback, RenderTiming};
use soundsync_core::clock::ClockMap;
use soundsync_core::layout::MapKind;
use soundsync_core::limiter::SoftLimiter;
use soundsync_core::player::{PlayState, StreamPlayer};
use soundsync_core::{Nanos, NodeId, StreamFormat};

use crate::chunk::AudioChunk;

/// Settings the engine thread changes while the mixer runs.
pub struct MixerShared {
    pub volume_bits: AtomicU32,
    pub muted: AtomicBool,
    pub extra_latency_ns: AtomicI64,
    pub follow_master_volume: AtomicBool,
    /// Streams render silently (calibration / sync test) but keep their timing.
    pub paused: AtomicBool,
    /// Last observed device lead (dac time − now).
    pub lead_ns: AtomicI64,
    /// Length of one output render (the device period), for estimating requirements.
    pub period_ns: AtomicI64,
}

impl MixerShared {
    pub fn new(volume: f32, muted: bool, extra_latency_ns: Nanos, follow_master_volume: bool) -> Self {
        MixerShared {
            volume_bits: AtomicU32::new(volume.to_bits()),
            muted: AtomicBool::new(muted),
            extra_latency_ns: AtomicI64::new(extra_latency_ns),
            follow_master_volume: AtomicBool::new(follow_master_volume),
            paused: AtomicBool::new(false),
            lead_ns: AtomicI64::new(0),
            period_ns: AtomicI64::new(0),
        }
    }
}

/// Per-stream numbers the mixer publishes for the engine thread.
pub struct SlotStats {
    pub error_ns: AtomicI64,
    pub behind_ns: AtomicI64,
    pub lost_frames: AtomicU64,
    /// `i64::MIN` until known.
    pub min_slack_ns: AtomicI64,
    /// Capture→emission delay this node needs for the stream; `i64::MIN` until known.
    pub required_ns: AtomicI64,
    pub state: AtomicU8,
    pub format_changed: AtomicBool,
    pub layout_kind: AtomicU8,
    pub presentation_offset_ns: AtomicI64,
    /// Gain this stream follows from its master's volume (f32 bits; 1.0 when not following).
    pub master_volume_bits: AtomicU32,
}

impl Default for SlotStats {
    fn default() -> Self {
        SlotStats {
            error_ns: AtomicI64::new(0),
            behind_ns: AtomicI64::new(0),
            lost_frames: AtomicU64::new(0),
            min_slack_ns: AtomicI64::new(i64::MIN),
            required_ns: AtomicI64::new(i64::MIN),
            state: AtomicU8::new(0),
            format_changed: AtomicBool::new(false),
            layout_kind: AtomicU8::new(0),
            presentation_offset_ns: AtomicI64::new(0),
            master_volume_bits: AtomicU32::new(1.0f32.to_bits()),
        }
    }
}

impl SlotStats {
    pub fn state(&self) -> PlayState {
        match self.state.load(Ordering::Relaxed) {
            1 => PlayState::Playing,
            2 => PlayState::Behind,
            _ => PlayState::Waiting,
        }
    }

    pub fn layout(&self) -> MapKind {
        match self.layout_kind.load(Ordering::Relaxed) {
            1 => MapKind::Substituted,
            2 => MapKind::Downmix,
            3 => MapKind::Upmix,
            _ => MapKind::Identity,
        }
    }

    pub fn min_slack(&self) -> Option<Nanos> {
        let v = self.min_slack_ns.load(Ordering::Relaxed);
        (v != i64::MIN).then_some(v)
    }

    pub fn required(&self) -> Option<Nanos> {
        let v = self.required_ns.load(Ordering::Relaxed);
        (v != i64::MIN).then_some(v)
    }
}

pub struct StreamSlot {
    pub master: NodeId,
    pub stream_id: u32,
    pub player: StreamPlayer,
    pub rx: rtrb::Consumer<AudioChunk>,
    /// Master clock → local clock (identity for this node's own stream).
    pub clock: Arc<ArcSwap<ClockMap>>,
    pub stats: Arc<SlotStats>,
    pub gain: f32,
}

/// Mono samples played on every channel so they reach the listener at `emit_at_ns` (local
/// clock), i.e. written for DAC time `emit_at_ns − extra latency`.
pub struct ScheduledSignal {
    pub samples: Vec<f32>,
    pub emit_at_ns: Nanos,
}

pub enum MixerCmd {
    /// Adds a stream (replacing one with the same master and id).
    AddStream(Box<StreamSlot>),
    RemoveStream { master: NodeId, stream_id: u32 },
    Play(Box<ScheduledSignal>),
}

/// Objects returned to the engine thread to be freed off the realtime thread.
pub enum MixerGarbage {
    Slot(Box<StreamSlot>),
    Signal(Box<ScheduledSignal>),
}

struct Playing {
    signal: Box<ScheduledSignal>,
    /// Next sample to play; None until its start time is reached.
    pos: Option<usize>,
}

pub struct Mixer {
    out: StreamFormat,
    shared: Arc<MixerShared>,
    cmds: rtrb::Consumer<MixerCmd>,
    garbage: rtrb::Producer<MixerGarbage>,
    // Boxed on purpose: slots arrive and leave through rings as Boxes, so the realtime thread
    // never moves large structs or frees memory.
    #[allow(clippy::vec_box)]
    slots: Vec<Box<StreamSlot>>,
    signals: Vec<Playing>,
    limiter: SoftLimiter,
    /// Largest device lead in the current and previous ~1 s window (the lead swings by up to a
    /// period as the device buffer drains and refills; requirements must cover the worst).
    lead_window: (Nanos, Nanos, Nanos), // (window start, max this window, max previous window)
    timing_transition_frames: usize,
}

const MAX_SLOTS: usize = 32;
const MAX_SIGNALS: usize = 32;

fn state_code(s: PlayState) -> u8 {
    match s {
        PlayState::Waiting => 0,
        PlayState::Playing => 1,
        PlayState::Behind => 2,
    }
}

fn layout_code(k: MapKind) -> u8 {
    match k {
        MapKind::Identity => 0,
        MapKind::Substituted => 1,
        MapKind::Downmix => 2,
        MapKind::Upmix => 3,
    }
}

impl Mixer {
    pub fn new(out: StreamFormat, shared: Arc<MixerShared>, cmds: rtrb::Consumer<MixerCmd>, garbage: rtrb::Producer<MixerGarbage>) -> Self {
        Mixer {
            out,
            shared,
            cmds,
            garbage,
            slots: Vec::with_capacity(MAX_SLOTS),
            signals: Vec::with_capacity(MAX_SIGNALS),
            limiter: SoftLimiter::new(out.sample_rate),
            lead_window: (0, 0, 0),
            timing_transition_frames: 0,
        }
    }

    fn discard(&mut self, g: MixerGarbage) {
        if let Err(rtrb::PushError::Full(g)) = self.garbage.push(g) {
            drop(g); // ring full: free here rather than leak
        }
    }

    fn apply_commands(&mut self) {
        while let Ok(cmd) = self.cmds.pop() {
            match cmd {
                MixerCmd::AddStream(slot) => {
                    if let Some(i) = self.slots.iter().position(|s| s.master == slot.master && s.stream_id == slot.stream_id) {
                        let old = std::mem::replace(&mut self.slots[i], slot);
                        self.discard(MixerGarbage::Slot(old));
                    } else if self.slots.len() < MAX_SLOTS {
                        self.slots.push(slot);
                    } else {
                        self.discard(MixerGarbage::Slot(slot));
                    }
                }
                MixerCmd::RemoveStream { master, stream_id } => {
                    if let Some(i) = self.slots.iter().position(|s| s.master == master && s.stream_id == stream_id) {
                        let old = self.slots.swap_remove(i);
                        self.discard(MixerGarbage::Slot(old));
                    }
                }
                MixerCmd::Play(signal) => {
                    if self.signals.len() < MAX_SIGNALS {
                        self.signals.push(Playing { signal, pos: None });
                    } else {
                        self.discard(MixerGarbage::Signal(signal));
                    }
                }
            }
        }
    }

    fn render_streams(&mut self, out: &mut [f32], t: RenderTiming, extra: Nanos) {
        let paused = self.shared.paused.load(Ordering::Relaxed);
        let follow = self.shared.follow_master_volume.load(Ordering::Relaxed);
        for slot in &mut self.slots {
            let clock = **slot.clock.load();
            while let Ok(chunk) = slot.rx.pop() {
                if chunk.header.format != slot.player.src_format() {
                    slot.stats.format_changed.store(true, Ordering::Relaxed);
                    continue;
                }
                slot.player.push(&chunk.header, chunk.samples(), chunk.arrival_ns, &clock, extra);
            }
            if slot.stats.format_changed.load(Ordering::Relaxed) {
                continue;
            }
            let master_gain = if follow {
                let (v, muted) = slot.player.master_volume();
                if muted { 0.0 } else { v }
            } else {
                1.0
            };
            slot.stats.master_volume_bits.store(master_gain.to_bits(), Ordering::Relaxed);
            let gain = if paused { 0.0 } else { slot.gain * master_gain };
            let previous_realigns = slot.player.stats().realigns;
            let report = slot.player.render_add(out, t, &clock, extra, gain);
            let stats = slot.player.stats();
            if stats.realigns != previous_realigns {
                // A hard realign fades this buffer out and the next aligned buffer in; reducing
                // Behind only fades in. This reports the existing transition without changing it.
                let fade = soundsync_core::player::PlayerConfig::for_output(self.out).fade_frames;
                let affected = if report.realigned { out.len() / self.out.channels as usize + fade } else { fade };
                self.timing_transition_frames = self.timing_transition_frames.max(affected);
            }
            let s = &slot.stats;
            s.error_ns.store(report.error_ns, Ordering::Relaxed);
            s.behind_ns.store(stats.behind_ns, Ordering::Relaxed);
            s.lost_frames.store(stats.lost_frames, Ordering::Relaxed);
            s.min_slack_ns.store(stats.min_slack_ns.unwrap_or(i64::MIN), Ordering::Relaxed);
            s.required_ns.store(stats.required_delay_ns.unwrap_or(i64::MIN), Ordering::Relaxed);
            s.state.store(state_code(report.state), Ordering::Relaxed);
            s.layout_kind.store(layout_code(slot.player.layout_kind()), Ordering::Relaxed);
            s.presentation_offset_ns.store(stats.presentation_offset_ns, Ordering::Relaxed);
        }
    }

    fn render_signals(&mut self, out: &mut [f32], t: RenderTiming, extra: Nanos) {
        let ch = self.out.channels as usize;
        let frames = out.len() / ch;
        let rate = self.out.sample_rate as i128;
        let mut i = 0;
        while i < self.signals.len() {
            let p = &mut self.signals[i];
            let (start_frame, mut pos) = match p.pos {
                Some(pos) => (0usize, pos),
                None => {
                    let target = p.signal.emit_at_ns - extra;
                    let k0 = (((target - t.dac_time_ns) as i128 * rate + 500_000_000).div_euclid(1_000_000_000)) as i64;
                    if k0 >= frames as i64 {
                        i += 1;
                        continue; // starts in a later period
                    }
                    // Started in the past (late command): skip ahead to stay on time.
                    if k0 >= 0 { (k0 as usize, 0) } else { (0, (-k0) as usize) }
                }
            };
            for frame in out[start_frame * ch..].chunks_exact_mut(ch) {
                if pos >= p.signal.samples.len() {
                    break;
                }
                let x = p.signal.samples[pos];
                frame.iter_mut().for_each(|s| *s += x);
                pos += 1;
            }
            p.pos = Some(pos);
            if pos >= p.signal.samples.len() {
                let done = self.signals.swap_remove(i);
                self.discard(MixerGarbage::Signal(done.signal));
            } else {
                i += 1;
            }
        }
    }
}

impl RenderCallback for Mixer {
    fn render(&mut self, out: &mut [f32], _format: StreamFormat, t: RenderTiming) {
        self.timing_transition_frames = 0;
        self.apply_commands();
        let lead = t.dac_time_ns - t.now_ns;
        let (start, cur, prev) = &mut self.lead_window;
        if t.now_ns - *start >= 1_000_000_000 {
            (*start, *prev, *cur) = (t.now_ns, *cur, lead);
        } else {
            *cur = (*cur).max(lead);
        }
        self.shared.lead_ns.store((*cur).max(*prev), Ordering::Relaxed);
        let frames = out.len() / self.out.channels.max(1) as usize;
        self.shared.period_ns.store((frames as i64 * 1_000_000_000) / self.out.sample_rate.max(1) as i64, Ordering::Relaxed);
        out.fill(0.0);
        let extra = self.shared.extra_latency_ns.load(Ordering::Relaxed);
        self.render_streams(out, t, extra);
        self.render_signals(out, t, extra);
        if self.shared.muted.load(Ordering::Relaxed) {
            out.fill(0.0);
        } else {
            let volume = f32::from_bits(self.shared.volume_bits.load(Ordering::Relaxed));
            if volume != 1.0 {
                out.iter_mut().for_each(|x| *x *= volume);
            }
        }
        self.limiter.process(out, self.out.channels as usize);
    }

    fn timing_transition_frames(&self) -> usize { self.timing_transition_frames }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundsync_core::player::PlayerConfig;
    use soundsync_core::protocol::{AudioHeader, frames_per_packet};

    const T0: Nanos = 20_000_000_000;
    const PERIOD: usize = 480;
    const OFFSET: Nanos = 40_000_000;

    fn stereo() -> StreamFormat {
        StreamFormat { sample_rate: 48_000, channels: 2, channel_mask: 0x3 }
    }

    /// A master stream: value(frame) -> sample, 175-frame packets arriving 1 ms after capture.
    struct Source {
        stream_id: u32,
        tx: rtrb::Producer<AudioChunk>,
        next_frame: u64,
        value: Box<dyn Fn(u64) -> f32>,
        volume: f32,
        muted: bool,
        format: StreamFormat,
    }

    impl Source {
        fn cap(f: u64) -> Nanos {
            T0 + (f as i128 * 1_000_000_000 / 48_000) as Nanos
        }
        fn pump(&mut self, now: Nanos) {
            let fpp = frames_per_packet(self.format.channels) as u64;
            while Self::cap(self.next_frame + fpp) + 1_000_000 <= now {
                let samples: Vec<f32> = (self.next_frame..self.next_frame + fpp)
                    .flat_map(|f| std::iter::repeat_n((self.value)(f), self.format.channels as usize))
                    .collect();
                let header = AudioHeader {
                    stream_id: self.stream_id,
                    seq: 0,
                    frame_index: self.next_frame,
                    frames: fpp as u16,
                    capture_time_ns: Self::cap(self.next_frame),
                    presentation_offset_ns: OFFSET,
                    format: self.format,
                    volume: self.volume,
                    muted: self.muted,
                };
                let _ = self.tx.push(AudioChunk::from_samples(header, &samples, now));
                self.next_frame += fpp;
            }
        }
    }

    struct Harness {
        mixer: Mixer,
        cmds: rtrb::Producer<MixerCmd>,
        garbage: rtrb::Consumer<MixerGarbage>,
        shared: Arc<MixerShared>,
        sources: Vec<Source>,
        k: u64,
        out: Vec<f32>,
    }

    impl Harness {
        fn new() -> Harness {
            let shared = Arc::new(MixerShared::new(1.0, false, 0, true));
            let (cmds, cmd_rx) = rtrb::RingBuffer::new(64);
            let (garbage_tx, garbage) = rtrb::RingBuffer::new(64);
            let mixer = Mixer::new(stereo(), shared.clone(), cmd_rx, garbage_tx);
            Harness { mixer, cmds, garbage, shared, sources: Vec::new(), k: 0, out: vec![0.0; PERIOD * 2] }
        }

        fn slot(&self, stream_id: u32, rx: rtrb::Consumer<AudioChunk>, src: StreamFormat) -> Box<StreamSlot> {
            Box::new(StreamSlot {
                master: 1,
                stream_id,
                player: StreamPlayer::new(src, PlayerConfig::for_output(stereo())),
                rx,
                clock: Arc::new(ArcSwap::from_pointee(ClockMap::IDENTITY)),
                stats: Arc::new(SlotStats::default()),
                gain: 1.0,
            })
        }

        fn add(&mut self, stream_id: u32, value: impl Fn(u64) -> f32 + 'static) -> Arc<SlotStats> {
            let (tx, rx) = rtrb::RingBuffer::new(1024);
            let slot = self.slot(stream_id, rx, stereo());
            let stats = slot.stats.clone();
            assert!(self.cmds.push(MixerCmd::AddStream(slot)).is_ok());
            self.sources.push(Source { stream_id, tx, next_frame: 0, value: Box::new(value), volume: 1.0, muted: false, format: stereo() });
            stats
        }

        fn dac(&self, frame: u64) -> Nanos {
            T0 + (frame as i128 * 1_000_000_000 / 48_000) as Nanos
        }

        /// Renders one period; returns channel 0 and the dac time of its first frame.
        fn render(&mut self) -> (Vec<f32>, Nanos) {
            let dac = self.dac(self.k * PERIOD as u64);
            let now = dac - 10_000_000;
            for s in &mut self.sources {
                s.pump(now);
            }
            self.out.fill(0.0);
            self.mixer.render(&mut self.out, stereo(), RenderTiming { now_ns: now, dac_time_ns: dac });
            self.k += 1;
            (self.out.iter().step_by(2).copied().collect(), dac)
        }

        fn render_secs(&mut self, secs: f64) -> Vec<(Vec<f32>, Nanos)> {
            (0..(secs * 100.0) as usize).map(|_| self.render()).collect()
        }
    }

    #[test]
    fn timing_transition_diagnostics_clear_after_the_realign_buffer() {
        let mut h = Harness::new();
        h.add(7, |_| 0.3);
        h.render_secs(0.5);
        assert_eq!(h.mixer.timing_transition_frames(), 0);
        h.shared.extra_latency_ns.store(-50_000_000, Ordering::Relaxed);
        h.render();
        assert_eq!(h.mixer.timing_transition_frames(), PERIOD + PlayerConfig::for_output(stereo()).fade_frames);
        h.render();
        assert_eq!(h.mixer.timing_transition_frames(), 0, "ordinary buffers must not be marked as transitions");
    }

    fn last_value(r: &[(Vec<f32>, Nanos)]) -> f32 {
        *r.last().unwrap().0.last().unwrap()
    }

    #[test]
    fn mixes_two_streams_sum() {
        let mut h = Harness::new();
        h.add(1, |_| 0.25);
        h.add(2, |_| 0.5);
        let r = h.render_secs(1.0);
        assert!((last_value(&r) - 0.75).abs() < 1e-3, "{}", last_value(&r));
    }

    #[test]
    fn limiter_engages_on_overload() {
        let mut h = Harness::new();
        h.add(1, |_| 0.8);
        h.add(2, |_| 0.8);
        let r = h.render_secs(1.0);
        let tail: Vec<f32> = r[50..].iter().flat_map(|(v, _)| v.clone()).collect();
        assert!(tail.iter().all(|x| *x <= 1.0), "clipped above full scale");
        assert!(tail.iter().all(|x| *x > 0.9), "limiter must not crush the signal");
    }

    #[test]
    fn follow_master_volume() {
        let mut h = Harness::new();
        h.add(1, |_| 0.5);
        h.sources[0].volume = 0.5;
        let r = h.render_secs(1.0);
        assert!((last_value(&r) - 0.25).abs() < 1e-3, "following: {}", last_value(&r));
        h.shared.follow_master_volume.store(false, Ordering::Relaxed);
        let r = h.render_secs(0.2);
        assert!((last_value(&r) - 0.5).abs() < 1e-3, "not following: {}", last_value(&r));
        h.shared.follow_master_volume.store(true, Ordering::Relaxed);
        h.sources[0].muted = true;
        let r = h.render_secs(0.3);
        assert_eq!(last_value(&r), 0.0, "master muted");
    }

    #[test]
    fn node_volume_and_mute_apply() {
        let mut h = Harness::new();
        h.add(1, |_| 0.5);
        h.shared.volume_bits.store(0.5f32.to_bits(), Ordering::Relaxed);
        let r = h.render_secs(1.0);
        assert!((last_value(&r) - 0.25).abs() < 1e-3, "{}", last_value(&r));
        h.shared.muted.store(true, Ordering::Relaxed);
        let r = h.render_secs(0.1);
        assert!(r.last().unwrap().0.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn format_change_flags_slot() {
        let mut h = Harness::new();
        let stats = h.add(1, |_| 0.5);
        h.render_secs(0.5);
        assert!(!stats.format_changed.load(Ordering::Relaxed));
        h.sources[0].format = StreamFormat { sample_rate: 48_000, channels: 8, channel_mask: 0x63F };
        let r = h.render_secs(0.5);
        assert!(stats.format_changed.load(Ordering::Relaxed));
        assert!(r.last().unwrap().0.iter().all(|x| *x == 0.0), "a slot with the wrong format plays nothing");
    }

    fn onsets(r: &[(Vec<f32>, Nanos)]) -> Vec<Nanos> {
        let mut prev = 0.0;
        let mut out = Vec::new();
        for (v, dac) in r {
            for (j, &x) in v.iter().enumerate() {
                if prev <= 0.25 && x > 0.25 {
                    out.push(dac + (j as i128 * 1_000_000_000 / 48_000) as Nanos);
                }
                prev = x;
            }
        }
        out
    }

    #[test]
    fn paused_outputs_silence_but_keeps_timing() {
        let mut h = Harness::new();
        let pulse = |f: u64| if f % 4800 == 2400 { 0.8 } else { 0.0 };
        h.add(1, pulse);
        h.render_secs(1.0);
        h.shared.paused.store(true, Ordering::Relaxed);
        let paused = h.render_secs(1.0);
        assert!(paused.iter().all(|(v, _)| v.iter().all(|x| *x == 0.0)));
        h.shared.paused.store(false, Ordering::Relaxed);
        let after = h.render_secs(0.5);
        let o = onsets(&after);
        assert!(!o.is_empty());
        for t in o {
            // Nearest generated pulse frame (k * 4800 + 2400).
            let f = ((t - OFFSET - T0) as f64 * 48_000.0 / 1e9 - 2400.0) / 4800.0;
            let f = f.round() as u64 * 4800 + 2400;
            let want = Source::cap(f) + OFFSET;
            assert!((t - want).abs() < 200_000, "pulse at {t} is {} us from frame {f}", (t - want) / 1000);
        }
    }

    #[test]
    fn scheduled_signal_plays_at_target() {
        let mut h = Harness::new();
        h.render_secs(0.1);
        let emit_at = h.dac(48_000) + 123_456; // somewhere inside a period
        let signal: Vec<f32> = (0..1000).map(|i| if i == 0 { 1.0 } else { 0.1 }).collect();
        assert!(h.cmds.push(MixerCmd::Play(Box::new(ScheduledSignal { samples: signal, emit_at_ns: emit_at }))).is_ok());
        let r = h.render_secs(1.2);
        let (first_frame_time, _) = r
            .iter()
            .flat_map(|(v, dac)| v.iter().enumerate().map(move |(j, x)| (dac + (j as i128 * 1_000_000_000 / 48_000) as Nanos, *x)))
            .find(|(_, x)| *x != 0.0)
            .expect("signal played");
        assert!((first_frame_time - emit_at).abs() <= 21_000, "off by {} ns", first_frame_time - emit_at);
        // It ends, and its memory goes back to the engine thread.
        assert!(r.last().unwrap().0.iter().all(|x| *x == 0.0));
        let mut got_signal = false;
        while let Ok(g) = h.garbage.pop() {
            got_signal |= matches!(g, MixerGarbage::Signal(_));
        }
        assert!(got_signal);
    }

    #[test]
    fn scheduled_signal_respects_extra_latency() {
        let mut h = Harness::new();
        h.shared.extra_latency_ns.store(30_000_000, Ordering::Relaxed);
        h.render_secs(0.1);
        let emit_at = h.dac(48_000);
        assert!(h.cmds.push(MixerCmd::Play(Box::new(ScheduledSignal { samples: vec![0.5; 100], emit_at_ns: emit_at }))).is_ok());
        let r = h.render_secs(1.1);
        let t = onsets(&r)[0];
        assert!((t - (emit_at - 30_000_000)).abs() <= 21_000, "a 30 ms-late speaker must be fed 30 ms early");
    }

    #[test]
    fn replaced_slot_goes_to_garbage() {
        let mut h = Harness::new();
        h.add(7, |_| 0.1);
        h.render();
        let (_, rx) = rtrb::RingBuffer::new(8);
        let again = h.slot(7, rx, stereo());
        assert!(h.cmds.push(MixerCmd::AddStream(again)).is_ok());
        h.render();
        assert!(matches!(h.garbage.pop(), Ok(MixerGarbage::Slot(_))));
        assert!(h.cmds.push(MixerCmd::RemoveStream { master: 1, stream_id: 7 }).is_ok());
        h.render();
        assert!(matches!(h.garbage.pop(), Ok(MixerGarbage::Slot(_))));
    }
}
