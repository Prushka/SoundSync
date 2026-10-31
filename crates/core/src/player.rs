//! Per-stream receive pipeline: jitter buffer → timing controller → resampler → layout map.
//!
//! Every source frame `f` has a target local DAC time
//! `remote_to_local(capture_time(f)) + presentation_offset − extra_latency + behind`.
//! Each render compares when the frame at the output head will actually reach the DAC with its
//! target, and the [`SyncController`] steers the resample ratio (which also converts the source
//! rate to the device rate) to drive that error to zero.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType, WindowFunction};

use crate::clock::ClockMap;
use crate::jitter::{Insert, JitterBuffer};
use crate::layout::{ChannelMap, MapKind};
use crate::protocol::AudioHeader;
use crate::sync::{SyncController, SyncDecision, SyncParams};
use crate::types::{Nanos, StreamFormat};

/// Largest number of output frames rendered per internal step; longer device periods are split.
const MAX_SEGMENT: usize = 2048;
/// Extra delay added on top of the measured deficit when a stream turns out to be infeasible.
const BEHIND_MARGIN_NS: Nanos = 2_000_000;
const SLACK_BUCKET_NS: Nanos = 5_000_000_000;
/// A presentation-offset change larger than this realigns at once instead of slewing.
const OFFSET_REALIGN_NS: Nanos = 1_000_000;
/// A DAC time this far from where the previous render left off means the device glitched (an
/// underrun after a stall): realign at once instead of slewing for tens of seconds.
const DAC_JUMP_NS: Nanos = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderTiming {
    /// Local time of this render call.
    pub now_ns: Nanos,
    /// Local time at which the first frame written now will reach the DAC.
    pub dac_time_ns: Nanos,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlayState {
    #[default]
    Waiting,
    Playing,
    /// Playing, but later than intended because data arrives after its deadline.
    Behind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderReport {
    pub state: PlayState,
    pub error_ns: Nanos,
    pub missing_frames: u32,
    pub realigned: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayerStats {
    /// Smallest (target − arrival − device lead) over the last ~10 s: how much earlier than
    /// strictly necessary packets arrive. Negative means they arrive too late.
    pub min_slack_ns: Option<Nanos>,
    /// Largest capture→emission delay this node needed recently for this stream (arrival delay +
    /// device lead + one period + resampler lookahead + extra latency). Unlike the slack it does
    /// not depend on the presentation offset in use, so the group delay can be sized from it.
    pub required_delay_ns: Option<Nanos>,
    pub behind_ns: Nanos,
    pub lost_frames: u64,
    pub realigns: u64,
    /// Relative resample ratio currently applied (1.0 = nominal).
    pub ratio: f64,
    pub presentation_offset_ns: Nanos,
}

#[derive(Clone, Copy, Debug)]
pub struct PlayerConfig {
    pub out: StreamFormat,
    pub chunk_frames: usize,
    /// Jitter-buffer capacity in source frames.
    pub jitter_capacity_frames: usize,
    pub sync: SyncParams,
    pub fade_frames: usize,
}

impl PlayerConfig {
    /// 64-frame resampler chunks, 3 s of buffering (covers the 2 s maximum group delay), 5 ms fades.
    pub fn for_output(out: StreamFormat) -> Self {
        Self {
            out,
            chunk_frames: 64,
            jitter_capacity_frames: 3 * out.sample_rate as usize,
            sync: SyncParams::default(),
            fade_frames: out.sample_rate as usize / 200,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Timeline {
    frame_index: u64,
    capture_time_ns: Nanos,
    presentation_offset_ns: Nanos,
}

/// Minimum slack and maximum required delay over two rolling 5 s buckets.
#[derive(Clone, Copy, Debug, Default)]
struct SlackWindow {
    bucket_start: Option<Nanos>,
    current: Option<(Nanos, Nanos)>,
    previous: Option<(Nanos, Nanos)>,
}

impl SlackWindow {
    fn add(&mut self, at: Nanos, slack: Nanos, need: Nanos) {
        match self.bucket_start {
            Some(start) if at - start < SLACK_BUCKET_NS => {}
            _ => {
                self.previous = self.current;
                self.current = None;
                self.bucket_start = Some(at);
            }
        }
        self.current = Some(self.current.map_or((slack, need), |(s, n)| (s.min(slack), n.max(need))));
    }

    fn combined(&self) -> Option<(Nanos, Nanos)> {
        match (self.current, self.previous) {
            (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.max(b.1))),
            (a, b) => a.or(b),
        }
    }

    fn min(&self) -> Option<Nanos> {
        self.combined().map(|c| c.0).filter(|&m| m != Nanos::MAX)
    }

    fn max_need(&self) -> Option<Nanos> {
        self.combined().map(|c| c.1)
    }

    fn full(&self) -> bool {
        self.previous.is_some()
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    /// Forgets slack (it is relative to the presentation offset) but keeps the required delay
    /// (which is not).
    fn forget_slack(&mut self) {
        for b in [&mut self.current, &mut self.previous].into_iter().flatten() {
            b.0 = Nanos::MAX;
        }
    }
}

pub struct StreamPlayer {
    src: StreamFormat,
    cfg: PlayerConfig,
    pub(crate) jitter: JitterBuffer,
    map: ChannelMap,
    resampler: Async<f32>,
    base_ratio: f64,
    ratio_rel: f64,
    controller: SyncController,
    in_buf: Vec<f32>,
    chunk_buf: Vec<f32>,
    /// Resampled frames (source channel count, output rate) not yet written to the device.
    fifo: Vec<f32>,
    fifo_start: usize,
    fifo_frames: usize,
    seg_buf: Vec<f32>,
    timeline: Option<Timeline>,
    stream_id: Option<u32>,
    first_frame: Option<u64>,
    playing: bool,
    /// Next source frame to feed to the resampler; negative before the stream's first frame.
    read_pos: i64,
    fade_in_left: usize,
    behind_ns: Nanos,
    wait_anchor: Option<u64>,
    /// The stream's presentation offset changed enough to realign on the next render.
    offset_changed: bool,
    lead_ns: Option<Nanos>,
    /// Duration of the last device period; a frame must be buffered this much earlier too.
    period_ns: Nanos,
    last_dac: Option<Nanos>,
    /// Where the device's timeline should be at the next render if nothing glitched.
    next_dac: Option<Nanos>,
    slack: SlackWindow,
    lost_frames: u64,
    realigns: u64,
    state: PlayState,
    volume: f32,
    muted: bool,
}

impl StreamPlayer {
    pub fn new(src: StreamFormat, cfg: PlayerConfig) -> Self {
        let sc = src.channels as usize;
        let base_ratio = cfg.out.sample_rate as f64 / src.sample_rate as f64;
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: None,
            oversampling_factor: 256,
            interpolation: SincInterpolationType::Linear,
            window: WindowFunction::BlackmanHarris2,
        };
        let resampler = Async::<f32>::new_sinc(base_ratio, 1.01, &params, cfg.chunk_frames, sc, FixedAsync::Output)
            .expect("valid resampler parameters");
        let fifo_capacity = MAX_SEGMENT + 2 * resampler.output_frames_max();
        Self {
            src,
            jitter: JitterBuffer::new(sc, cfg.jitter_capacity_frames),
            map: ChannelMap::new(src, cfg.out),
            in_buf: vec![0.0; resampler.input_frames_max() * sc],
            chunk_buf: vec![0.0; resampler.output_frames_max() * sc],
            fifo: vec![0.0; fifo_capacity * sc],
            fifo_start: 0,
            fifo_frames: 0,
            seg_buf: vec![0.0; MAX_SEGMENT * sc],
            resampler,
            base_ratio,
            ratio_rel: 1.0,
            controller: SyncController::new(cfg.sync),
            cfg,
            timeline: None,
            stream_id: None,
            first_frame: None,
            playing: false,
            read_pos: 0,
            fade_in_left: 0,
            behind_ns: 0,
            wait_anchor: None,
            offset_changed: false,
            lead_ns: None,
            period_ns: 0,
            last_dac: None,
            next_dac: None,
            slack: SlackWindow::default(),
            lost_frames: 0,
            realigns: 0,
            state: PlayState::Waiting,
            volume: 1.0,
            muted: false,
        }
    }

    pub fn src_format(&self) -> StreamFormat {
        self.src
    }

    pub fn layout_kind(&self) -> MapKind {
        self.map.kind()
    }

    /// Master endpoint volume and mute from the newest header.
    pub fn master_volume(&self) -> (f32, bool) {
        (self.volume, self.muted)
    }

    pub fn stats(&self) -> PlayerStats {
        PlayerStats {
            min_slack_ns: self.slack.min(),
            required_delay_ns: self.slack.max_need(),
            behind_ns: self.behind_ns,
            lost_frames: self.lost_frames,
            realigns: self.realigns,
            ratio: self.ratio_rel,
            presentation_offset_ns: self.timeline.map_or(0, |t| t.presentation_offset_ns),
        }
    }

    fn reset_stream(&mut self) {
        self.jitter.clear();
        self.timeline = None;
        self.first_frame = None;
        self.playing = false;
        self.behind_ns = 0;
        self.wait_anchor = None;
        self.slack.clear();
        self.state = PlayState::Waiting;
        self.controller.reset();
    }

    pub fn push(&mut self, h: &AudioHeader, samples: &[f32], arrival_ns: Nanos, clock: &ClockMap, extra_latency_ns: Nanos) {
        if h.format != self.src {
            return;
        }
        if self.stream_id != Some(h.stream_id) {
            self.reset_stream();
            self.stream_id = Some(h.stream_id);
        }
        let inserted = if samples.is_empty() {
            self.jitter.insert_silence(h.frame_index, h.frames as usize)
        } else {
            self.jitter.insert(h.frame_index, samples)
        };
        let newer = self.timeline.is_none_or(|t| h.frame_index >= t.frame_index);
        if inserted == Insert::Reset {
            // A jump far beyond the buffered data: the stream restarted or the master slept.
            self.playing = false;
            self.first_frame = None;
            self.wait_anchor = None;
        }
        if newer || inserted == Insert::Reset {
            if let Some(t) = self.timeline
                && (h.presentation_offset_ns - t.presentation_offset_ns).abs() > OFFSET_REALIGN_NS
            {
                self.offset_changed = self.playing;
                // Lateness was measured against the old offset; re-check feasibility from scratch.
                self.behind_ns = 0;
                self.wait_anchor = None;
                self.slack.forget_slack();
            }
            self.timeline = Some(Timeline {
                frame_index: h.frame_index,
                capture_time_ns: h.capture_time_ns,
                presentation_offset_ns: h.presentation_offset_ns,
            });
            self.volume = h.volume;
            self.muted = h.muted;
        }
        if inserted == Insert::TooOld {
            return;
        }
        self.first_frame = Some(self.first_frame.map_or(h.frame_index, |f| f.min(h.frame_index)));
        if let Some(lead) = self.lead_ns {
            // Worst case the frame is the last one of a period, which is rendered one lead plus one
            // period before its DAC time, and the resampler reads `delay` frames ahead of the head.
            let target = self.target_ns(h.frame_index as f64, clock, extra_latency_ns, 0) as Nanos;
            let deadline = target - lead - self.period_ns - self.base_delay_src_ns();
            let slack = deadline - arrival_ns;
            self.slack.add(arrival_ns, slack, h.presentation_offset_ns - slack);
        }
    }

    fn src_ns_per_frame(&self) -> f64 {
        1e9 / self.src.sample_rate as f64
    }

    /// Target local DAC time of (fractional) source position `pos`.
    fn target_ns(&self, pos: f64, clock: &ClockMap, extra_latency_ns: Nanos, behind_ns: Nanos) -> f64 {
        let tl = self.timeline.expect("timeline present");
        let cap = tl.capture_time_ns as f64 + (pos - tl.frame_index as f64) * self.src_ns_per_frame();
        let cap_i = cap.floor();
        clock.remote_to_local(cap_i as Nanos) as f64 + (cap - cap_i) + (tl.presentation_offset_ns - extra_latency_ns + behind_ns) as f64
    }

    /// Source position whose target DAC time is `t_local`.
    fn pos_for_target(&self, t_local: Nanos, clock: &ClockMap, extra_latency_ns: Nanos) -> f64 {
        let tl = self.timeline.expect("timeline present");
        let cap = clock.local_to_remote(t_local - tl.presentation_offset_ns + extra_latency_ns - self.behind_ns);
        tl.frame_index as f64 + (cap - tl.capture_time_ns) as f64 / self.src_ns_per_frame()
    }

    fn total_ratio(&self) -> f64 {
        self.base_ratio * self.ratio_rel
    }

    /// Source position of the next frame that will be written to the device.
    fn head_pos(&self) -> f64 {
        self.read_pos as f64 - (self.resampler.output_delay() + self.fifo_frames) as f64 / self.total_ratio()
    }

    pub fn render_add(&mut self, out: &mut [f32], t: RenderTiming, clock: &ClockMap, extra_latency_ns: Nanos, gain: f32) -> RenderReport {
        self.lead_ns = Some(t.dac_time_ns - t.now_ns);
        self.period_ns = (out.len() / self.cfg.out.channels as usize) as Nanos * 1_000_000_000 / self.cfg.out.sample_rate as Nanos;
        let dt = self.last_dac.map_or(0, |d| t.dac_time_ns - d);
        self.last_dac = Some(t.dac_time_ns);
        let device_glitched = self.next_dac.is_some_and(|e| (t.dac_time_ns - e).abs() > DAC_JUMP_NS);
        self.next_dac = Some(t.dac_time_ns + self.period_ns);
        let mut report = RenderReport::default();
        if self.timeline.is_none() {
            self.state = PlayState::Waiting;
            report.state = self.state;
            return report;
        }
        let out_frames = out.len() / self.cfg.out.channels as usize;

        if self.playing && self.stream_stalled() {
            self.playing = false;
        }
        if self.playing {
            self.relax_behind();
        }
        if !self.playing {
            self.try_start(t.dac_time_ns, out_frames, clock, extra_latency_ns);
            if !self.playing {
                report.state = self.state;
                return report;
            }
        }

        let error = t.dac_time_ns - self.target_ns(self.head_pos(), clock, extra_latency_ns, self.behind_ns).round() as Nanos;
        report.error_ns = error;
        let mut fade_out = false;
        let decision = if std::mem::take(&mut self.offset_changed) || device_glitched { SyncDecision::Realign } else { self.controller.update(error, dt) };
        match decision {
            SyncDecision::Realign => {
                fade_out = true;
                report.realigned = true;
                self.realigns += 1;
            }
            SyncDecision::Ratio(r) => {
                if r != self.ratio_rel && self.resampler.set_resample_ratio_relative(r, true).is_ok() {
                    self.ratio_rel = r;
                }
            }
        }

        let oc = self.cfg.out.channels as usize;
        let mut done = 0;
        while done < out_frames {
            let n = (out_frames - done).min(MAX_SEGMENT);
            report.missing_frames += self.render_segment(&mut out[done * oc..(done + n) * oc], n, gain, fade_out && done == 0);
            done += n;
        }
        if fade_out {
            self.playing = false;
        }
        self.state = if self.behind_ns > 0 { PlayState::Behind } else { PlayState::Playing };
        report.state = self.state;
        report
    }

    /// True once the read position is half a second past the newest data (master stopped).
    fn stream_stalled(&self) -> bool {
        let end = self.jitter.end_frame().unwrap_or(0) as i64;
        self.read_pos > end + self.src.sample_rate as i64 / 2
    }

    /// Lower `behind_ns` once packets have arrived comfortably early for a full slack window.
    fn relax_behind(&mut self) {
        if self.behind_ns == 0 || !self.slack.full() {
            return;
        }
        let Some(min_slack) = self.slack.min() else { return };
        let needed = (-min_slack + BEHIND_MARGIN_NS).max(0);
        if needed + 5_000_000 < self.behind_ns {
            self.behind_ns = if needed <= BEHIND_MARGIN_NS { 0 } else { needed };
            self.playing = false;
            self.realigns += 1;
        }
    }

    fn base_delay_src_ns(&self) -> Nanos {
        (self.base_delay_src() * self.src_ns_per_frame()) as Nanos
    }

    fn try_start(&mut self, dac_ns: Nanos, out_frames: usize, clock: &ClockMap, extra_latency_ns: Nanos) {
        for _ in 0..2 {
            let p0 = self.pos_for_target(dac_ns, clock, extra_latency_ns);
            let start = (p0 + self.base_delay_src()).ceil();
            let end = self.jitter.end_frame().unwrap_or(0) as f64;
            let first = self.first_frame.unwrap_or(0) as f64;
            let period_src = out_frames as f64 / self.base_ratio;
            if start + period_src < first {
                self.state = PlayState::Waiting;
                return; // the stream's first frame plays after this period
            }
            // This render consumes a whole period plus the resampler's lookahead.
            let need = period_src + self.resampler.input_frames_next() as f64;
            if start + need > end {
                let end_frame = end as u64;
                match self.wait_anchor {
                    None => self.wait_anchor = Some(end_frame),
                    Some(anchor) if end_frame >= anchor + self.src.sample_rate as u64 / 10 => {
                        // Data keeps arriving but never in time: play later by the deficit.
                        let deficit = (start + need - end) * self.src_ns_per_frame();
                        self.behind_ns += deficit.ceil() as Nanos + BEHIND_MARGIN_NS;
                        self.wait_anchor = None;
                        continue;
                    }
                    Some(_) => {}
                }
                self.state = if self.behind_ns > 0 { PlayState::Behind } else { PlayState::Waiting };
                return;
            }
            self.resampler.reset();
            self.ratio_rel = 1.0;
            self.controller.reset_error();
            self.read_pos = start as i64;
            self.fifo_start = 0;
            self.fifo_frames = 0;
            self.fade_in_left = self.cfg.fade_frames;
            self.wait_anchor = None;
            self.playing = true;
            return;
        }
    }

    /// Resampler delay in source frames at the nominal ratio (used when (re)starting).
    fn base_delay_src(&self) -> f64 {
        self.resampler.output_delay() as f64 / self.base_ratio
    }

    /// Renders `n` frames into `out` (interleaved, output channels). Returns lost frames.
    fn render_segment(&mut self, out: &mut [f32], n: usize, gain: f32, fade_out: bool) -> u32 {
        let sc = self.src.channels as usize;
        let mut lost = 0;
        while self.fifo_frames < n {
            lost += self.produce_chunk();
        }
        let start = self.fifo_start * sc;
        self.seg_buf[..n * sc].copy_from_slice(&self.fifo[start..start + n * sc]);
        self.fifo_start += n;
        self.fifo_frames -= n;

        let fade = self.cfg.fade_frames.max(1);
        for (i, frame) in self.seg_buf[..n * sc].chunks_exact_mut(sc).enumerate() {
            let mut g = 1.0;
            if self.fade_in_left > 0 {
                g *= 1.0 - self.fade_in_left as f32 / fade as f32;
                self.fade_in_left -= 1;
            }
            if fade_out {
                g *= (1.0 - i as f32 / fade as f32).max(0.0);
            }
            if g != 1.0 {
                frame.iter_mut().for_each(|x| *x *= g);
            }
        }
        self.map.apply_add(&self.seg_buf[..n * sc], out, gain);
        lost
    }

    /// Resamples one chunk into the FIFO. Returns frames that were holes in the stream.
    fn produce_chunk(&mut self) -> u32 {
        let sc = self.src.channels as usize;
        if self.fifo_start > 0 {
            let (s, len) = (self.fifo_start * sc, self.fifo_frames * sc);
            self.fifo.copy_within(s..s + len, 0);
            self.fifo_start = 0;
        }
        let need = self.resampler.input_frames_next();
        // Positions before frame 0 are pre-stream silence.
        let pre = (-self.read_pos).clamp(0, need as i64) as usize;
        self.in_buf[..pre * sc].fill(0.0);
        let from = self.read_pos.max(0) as u64;
        let missing = self.jitter.read(from, &mut self.in_buf[pre * sc..need * sc]);
        // Only count holes inside the received range; frames before the stream start or past the
        // newest packet are expected silence.
        let first = self.first_frame.unwrap_or(0);
        let end = self.jitter.end_frame().unwrap_or(0);
        let span_end = from + (need - pre) as u64;
        let (lo, hi) = (from.max(first), span_end.min(end));
        let inside = hi.saturating_sub(lo) as usize;
        let outside = (need - pre) - inside;
        let holes = missing.saturating_sub(outside);
        self.lost_frames += holes as u64;

        let input = InterleavedSlice::new(&self.in_buf[..need * sc], sc, need).expect("sized input");
        let out_frames = self.resampler.output_frames_next();
        let mut output = InterleavedSlice::new_mut(&mut self.chunk_buf[..out_frames * sc], sc, out_frames).expect("sized output");
        let (consumed, produced) = self.resampler.process_into_buffer(&input, &mut output, None).expect("buffers sized by resampler");
        self.read_pos += consumed as i64;
        let dst = self.fifo_frames * sc;
        self.fifo[dst..dst + produced * sc].copy_from_slice(&self.chunk_buf[..produced * sc]);
        self.fifo_frames += produced;
        holes as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AudioHeader, frames_per_packet};

    const T0: Nanos = 10_000_000_000;
    const PERIOD: usize = 480;

    #[derive(Clone, Copy, PartialEq)]
    enum Signal {
        Pulses,
        Sine,
    }

    #[derive(Clone, Copy)]
    struct SimCfg {
        src_rate: u32,
        out_rate: u32,
        dev_ppm: f64,
        secs: f64,
        offset_ns: Nanos,
        net_delay_ns: Nanos,
        lead_ns: Nanos,
        /// (at seconds, new presentation offset)
        offset_change: Option<(f64, Nanos)>,
        /// (start seconds, length seconds): master silent, then restarts with a new stream id
        gap: Option<(f64, f64)>,
        signal: Signal,
        /// Receiver's extra output latency (plays this much earlier).
        extra_ns: Nanos,
        /// Master clock = true time + this offset; the receiver gets the matching ClockMap.
        master_clock_offset_ns: Nanos,
        /// (at seconds, gap): the output device underruns once and moves on by `gap`.
        underrun: Option<(f64, Nanos)>,
    }

    impl Default for SimCfg {
        fn default() -> Self {
            Self {
                src_rate: 48_000,
                out_rate: 48_000,
                dev_ppm: 0.0,
                secs: 5.0,
                offset_ns: 50_000_000,
                net_delay_ns: 1_000_000,
                lead_ns: 10_000_000,
                offset_change: None,
                gap: None,
                signal: Signal::Pulses,
                extra_ns: 0,
                master_clock_offset_ns: 0,
                underrun: None,
            }
        }
    }

    struct SimPacket {
        arrival: Nanos,
        header: AudioHeader,
        samples: Vec<f32>,
    }

    struct SimResult {
        /// (expected emission, measured emission) for each detected pulse
        pulses: Vec<(Nanos, Nanos)>,
        expected: Vec<Nanos>,
        reports: Vec<(Nanos, RenderReport)>,
        out_ch0: Vec<f32>,
        player: StreamPlayer,
    }

    impl SimResult {
        fn pulses_after(&self, secs: f64) -> Vec<(Nanos, Nanos)> {
            let t = T0 + (secs * 1e9) as Nanos;
            self.pulses.iter().copied().filter(|&(e, _)| e >= t).collect()
        }
        fn expected_after(&self, secs: f64) -> usize {
            let t = T0 + (secs * 1e9) as Nanos;
            self.expected.iter().filter(|&&e| e >= t).count()
        }
    }

    fn stereo(rate: u32) -> StreamFormat {
        StreamFormat { sample_rate: rate, channels: 2, channel_mask: 0x3 }
    }

    /// Master side: stereo stream, one or two segments (second after `gap`), 175-frame packets.
    fn master_packets(cfg: &SimCfg, expected: &mut Vec<Nanos>) -> Vec<SimPacket> {
        let fmt = stereo(cfg.src_rate);
        let fpp = frames_per_packet(2) as u64;
        let mut segments = vec![(T0, cfg.secs, 0xA11CE)];
        if let Some((start, len)) = cfg.gap {
            segments = vec![(T0, start, 0xA11CE), (T0 + ((start + len) * 1e9) as Nanos, cfg.secs - start - len, 0xB0B)];
        }
        let pulse_every = cfg.src_rate as u64 / 10;
        let mut out = Vec::new();
        for (seg_t0, seg_secs, stream_id) in segments {
            let total = (seg_secs * cfg.src_rate as f64) as u64;
            let cap = |f: u64| seg_t0 + (f as f64 * 1e9 / cfg.src_rate as f64).round() as Nanos;
            let offset_at = |t: Nanos| match cfg.offset_change {
                Some((at, new)) if t >= T0 + (at * 1e9) as Nanos => new,
                _ => cfg.offset_ns,
            };
            let mut f = 0u64;
            let mut seq = 0u32;
            while f + fpp <= total {
                let mut samples = Vec::with_capacity(fpp as usize * 2);
                for i in f..f + fpp {
                    let x = match cfg.signal {
                        Signal::Pulses => {
                            if i % pulse_every == pulse_every / 2 {
                                expected.push(cap(i) + offset_at(cap(i)) - cfg.extra_ns);
                                0.8
                            } else {
                                0.0
                            }
                        }
                        Signal::Sine => (0.5 * (i as f64 / cfg.src_rate as f64 * 440.0 * std::f64::consts::TAU).sin()) as f32,
                    };
                    samples.extend_from_slice(&[x, x]);
                }
                out.push(SimPacket {
                    arrival: cap(f + fpp - 1) + cfg.net_delay_ns,
                    header: AudioHeader {
                        stream_id,
                        seq,
                        frame_index: f,
                        frames: fpp as u16,
                        capture_time_ns: cap(f) + cfg.master_clock_offset_ns,
                        presentation_offset_ns: offset_at(cap(f)),
                        format: fmt,
                        volume: 1.0,
                        muted: false,
                    },
                    samples,
                });
                f += fpp;
                seq += 1;
            }
        }
        out
    }

    fn sim(cfg: SimCfg) -> SimResult {
        let mut expected = Vec::new();
        let packets = master_packets(&cfg, &mut expected);
        let mut player = StreamPlayer::new(stereo(cfg.src_rate), PlayerConfig::for_output(stereo(cfg.out_rate)));
        let clock = ClockMap { ref_local: T0, offset_ns: cfg.master_clock_offset_ns as f64, skew: 0.0 };
        let dev_rate = cfg.out_rate as f64 * (1.0 + cfg.dev_ppm * 1e-6);
        let dac = |j: u64| T0 + (j as f64 * 1e9 / dev_rate).round() as Nanos;
        let mut next_packet = 0;
        let mut out = vec![0.0f32; PERIOD * 2];
        let mut prev = 0.0f32;
        let mut onsets = Vec::new();
        let mut reports = Vec::new();
        let mut out_ch0 = Vec::new();
        let renders = (cfg.secs * cfg.out_rate as f64 / PERIOD as f64) as u64;
        let mut skipped = 0u64;
        for k in 0..renders {
            if let Some((at, gap)) = cfg.underrun
                && skipped == 0
                && dac(k * PERIOD as u64) >= T0 + (at * 1e9) as Nanos
            {
                skipped = (gap as f64 * dev_rate / 1e9).round() as u64; // frames the device played without us
            }
            let j0 = k * PERIOD as u64 + skipped;
            let dac_time = dac(j0);
            let now = dac_time - cfg.lead_ns;
            while next_packet < packets.len() && packets[next_packet].arrival <= now {
                let p = &packets[next_packet];
                player.push(&p.header, &p.samples, p.arrival, &clock, cfg.extra_ns);
                next_packet += 1;
            }
            out.fill(0.0);
            let report = player.render_add(&mut out, RenderTiming { now_ns: now, dac_time_ns: dac_time }, &clock, cfg.extra_ns, 1.0);
            reports.push((dac_time, report));
            for j in 0..PERIOD {
                let x = out[j * 2];
                out_ch0.push(x);
                if prev <= 0.25 && x > 0.25 {
                    let (t_a, t_b) = (dac(j0 + j as u64 - 1) as f64, dac(j0 + j as u64) as f64);
                    onsets.push((t_a + (0.25 - prev) as f64 / (x - prev) as f64 * (t_b - t_a)) as Nanos);
                }
                prev = x;
            }
        }
        let pulses = onsets
            .iter()
            .filter_map(|&m| {
                let e = *expected.iter().min_by_key(|&&e| (e - m).abs())?;
                ((e - m).abs() < 30_000_000).then_some((e, m))
            })
            .collect();
        SimResult { pulses, expected, reports, out_ch0, player }
    }

    fn assert_pulses_within(r: &SimResult, after_secs: f64, tol_ns: Nanos) {
        let got = r.pulses_after(after_secs);
        let want = r.expected_after(after_secs);
        assert!(want > 0);
        assert!(got.len() + 2 >= want, "only {} of {} pulses detected", got.len(), want);
        for (e, m) in got {
            assert!((m - e).abs() < tol_ns, "pulse at {} ms off by {} us", (e - T0) / 1_000_000, (m - e) / 1000);
        }
    }

    #[test]
    fn tracks_200ppm_drift_within_200us() {
        for ppm in [-200.0, 200.0] {
            let r = sim(SimCfg { dev_ppm: ppm, secs: 40.0, ..Default::default() });
            assert_pulses_within(&r, 20.0, 200_000);
            assert!((r.player.stats().ratio - 1.0).abs() > 100e-6, "ppm {ppm}: ratio {}", r.player.stats().ratio);
        }
    }

    #[test]
    fn extra_latency_plays_earlier() {
        let r = sim(SimCfg { extra_ns: 20_000_000, ..Default::default() });
        assert_pulses_within(&r, 1.0, 200_000);
    }

    #[test]
    fn remote_clock_offset_is_applied() {
        let r = sim(SimCfg { master_clock_offset_ns: -7_300_000, ..Default::default() });
        assert_pulses_within(&r, 1.0, 200_000);
    }

    #[test]
    fn prealigns_on_start() {
        let r = sim(SimCfg { secs: 3.0, ..Default::default() });
        let &(e, m) = r.pulses.first().expect("a pulse");
        assert_eq!(e, r.expected[0], "the very first pulse must play");
        assert!((m - e).abs() < 200_000, "first pulse off by {} us", (m - e) / 1000);
    }

    #[test]
    fn small_offset_change_realigns_immediately() {
        // Auto group delay grows in 5 ms steps; slewing 5 ms at 300 ppm would take ~17 s.
        let r = sim(SimCfg { secs: 12.0, offset_change: Some((10.0, 55_000_000)), ..Default::default() });
        let realigns = r.reports.iter().filter(|(_, rep)| rep.realigned).count();
        assert_eq!(realigns, 1);
        assert_pulses_within(&r, 10.3, 200_000);
    }

    #[test]
    fn device_underrun_realigns_at_once() {
        // The output glitched (CPU stall): the device played 10 ms we never wrote, so its DAC time
        // jumps ahead of the frames written. The stream must snap back instead of slewing ~30 s.
        let r = sim(SimCfg { secs: 8.0, underrun: Some((5.0, 10_000_000)), ..Default::default() });
        assert_pulses_within(&r, 5.2, 200_000);
    }

    #[test]
    fn realign_keeps_drift_compensation() {
        // A group delay step realigns the stream, but the device's clock drift is unchanged: the
        // rate correction learned so far must carry over instead of re-converging from zero.
        let r = sim(SimCfg { dev_ppm: 200.0, secs: 30.0, offset_change: Some((25.0, 55_000_000)), ..Default::default() });
        let worst = |from: f64, to: f64| {
            let (a, b) = (T0 + (from * 1e9) as Nanos, T0 + (to * 1e9) as Nanos);
            r.pulses.iter().filter(|&&(e, _)| (a..b).contains(&e)).map(|&(e, m)| (m - e).abs()).max().unwrap()
        };
        let (before, after) = (worst(20.0, 25.0), worst(25.1, 30.0));
        assert!(after <= before + 30_000, "after the realign {} us vs {} us before", after / 1000, before / 1000);
    }

    #[test]
    fn realigns_after_50ms_jump() {
        let r = sim(SimCfg { secs: 15.0, offset_change: Some((10.0, 100_000_000)), ..Default::default() });
        let realigns = r.reports.iter().filter(|(_, rep)| rep.realigned).count();
        assert_eq!(realigns, 1, "exactly one realign");
        assert_pulses_within(&r, 11.2, 200_000);
    }

    fn header(fmt: StreamFormat, frame_index: u64, frames: u16, capture: Nanos, offset: Nanos) -> AudioHeader {
        AudioHeader {
            stream_id: 1,
            seq: 0,
            frame_index,
            frames,
            capture_time_ns: capture,
            presentation_offset_ns: offset,
            format: fmt,
            volume: 1.0,
            muted: false,
        }
    }

    #[test]
    fn waits_with_silence_until_data_due() {
        let fmt = stereo(48_000);
        let mut player = StreamPlayer::new(fmt, PlayerConfig::for_output(fmt));
        player.push(&header(fmt, 0, 175, T0, 50_000_000), &[0.5; 350], T0 + 1_000_000, &ClockMap::IDENTITY, 0);
        let mut out = vec![0.0f32; PERIOD * 2];
        let rep = player.render_add(&mut out, RenderTiming { now_ns: T0 + 2_000_000, dac_time_ns: T0 + 12_000_000 }, &ClockMap::IDENTITY, 0, 1.0);
        assert_eq!(rep.state, PlayState::Waiting);
        assert!(out.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn reports_behind_when_data_late() {
        // Data arrives 20 ms after capture but should play 5 ms after capture. A frame must be in
        // the buffer one device lead (10 ms) plus one period (10 ms) plus the resampler lookahead
        // (~1.3 ms) before its DAC time, so the node can only play about
        // 20 + 10 + 10 + 1.3 + 2 (margin) - 5 = 38 ms late (plus up to one 3.6 ms packet).
        let r = sim(SimCfg { offset_ns: 5_000_000, net_delay_ns: 20_000_000, signal: Signal::Sine, ..Default::default() });
        let behind = r.player.stats().behind_ns;
        assert!((33_000_000..=45_000_000).contains(&behind), "behind {} ms", behind as f64 / 1e6);
        assert_eq!(r.reports.last().unwrap().1.state, PlayState::Behind);
        let start = r.out_ch0.iter().position(|&x| x != 0.0).expect("audio played");
        let longest_zero_run = r.out_ch0[start..].split(|&x| x != 0.0).map(|run| run.len()).max().unwrap_or(0);
        assert!(longest_zero_run <= PERIOD, "gap of {longest_zero_run} frames after playback started");
    }

    #[test]
    fn behind_clears_when_offset_grows() {
        // Infeasible at 5 ms, comfortably feasible once the group delay grows to 80 ms.
        let r = sim(SimCfg { secs: 9.0, offset_ns: 5_000_000, net_delay_ns: 20_000_000, offset_change: Some((4.0, 80_000_000)), ..Default::default() });
        assert_eq!(r.player.stats().behind_ns, 0);
        assert_eq!(r.reports.last().unwrap().1.state, PlayState::Playing);
        assert_pulses_within(&r, 5.0, 200_000);
    }

    #[test]
    fn converts_44k1_to_48k_keeping_timing() {
        let r = sim(SimCfg { src_rate: 44_100, out_rate: 48_000, secs: 20.0, ..Default::default() });
        assert_pulses_within(&r, 5.0, 200_000);
    }

    #[test]
    fn push_computes_slack() {
        let fmt = stereo(48_000);
        let mut player = StreamPlayer::new(fmt, PlayerConfig::for_output(fmt));
        let mut out = vec![0.0f32; PERIOD * 2];
        // One render establishes a 10 ms device lead.
        player.render_add(&mut out, RenderTiming { now_ns: T0 - 10_000_000, dac_time_ns: T0 }, &ClockMap::IDENTITY, 0, 1.0);
        // Target DAC time of frame 0 = T0 + 50 ms; it arrives 30 ms before that. It must be buffered
        // a lead (10 ms) + a period (480 frames = 10 ms) + the resampler lookahead (64 frames =
        // 1.33 ms) ahead of its DAC time, leaving 8.67 ms to spare.
        player.push(&header(fmt, 0, 175, T0, 50_000_000), &[0.0; 350], T0 + 20_000_000, &ClockMap::IDENTITY, 0);
        let slack = player.stats().min_slack_ns.expect("slack recorded");
        assert!((slack - 8_666_667).abs() < 300_000, "slack {slack}");
    }

    #[test]
    fn required_delay_is_independent_of_offset_changes() {
        // Needed delay = network (1 ms) + packet (3.6 ms) + lead (10 ms) + period (10 ms) +
        // resampler lookahead (1.3 ms) ≈ 26 ms, whatever offset the stream currently uses.
        let r = sim(SimCfg { secs: 12.0, offset_change: Some((4.0, 120_000_000)), ..Default::default() });
        let need = r.player.stats().required_delay_ns.expect("measured");
        assert!((24_000_000..=28_000_000).contains(&need), "required {} ms", need as f64 / 1e6);
    }

    #[test]
    fn long_gap_realigns_without_growth() {
        let r = sim(SimCfg { secs: 620.0, gap: Some((5.0, 600.0)), ..Default::default() });
        assert_pulses_within(&r, 606.0, 200_000);
        assert_pulses_within(&r, 0.0, 30_000_000); // nothing wildly off anywhere
        assert_eq!(r.player.jitter.data.len(), 2 * 3 * 48_000, "jitter buffer must not grow");
    }

    #[test]
    fn layout_mapping_applied() {
        let src = StreamFormat { sample_rate: 48_000, channels: 6, channel_mask: 0x3F };
        let mut player = StreamPlayer::new(src, PlayerConfig::for_output(stereo(48_000)));
        assert_eq!(player.layout_kind(), MapKind::Downmix);
        let fpp = frames_per_packet(6) as u64;
        let mut f = 0u64;
        let mut out = vec![0.0f32; PERIOD * 2];
        let mut last = (0.0, 0.0);
        let cap = |f: u64| T0 + f as Nanos * 1_000_000_000 / 48_000;
        for k in 0..200 {
            let dac_time = T0 + k * 10_000_000;
            let now = dac_time - 10_000_000;
            while cap(f + fpp) + 1_000_000 <= now {
                let mut samples = vec![0.0f32; fpp as usize * 6];
                for frame in samples.as_chunks_mut::<6>().0 {
                    frame[2] = 0.5; // FC
                }
                player.push(&header(src, f, fpp as u16, cap(f), 30_000_000), &samples, now, &ClockMap::IDENTITY, 0);
                f += fpp;
            }
            out.fill(0.0);
            player.render_add(&mut out, RenderTiming { now_ns: now, dac_time_ns: dac_time }, &ClockMap::IDENTITY, 0, 1.0);
            last = (out[PERIOD], out[PERIOD + 1]);
        }
        let want = 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        assert!((last.0 - want).abs() < 0.01 && (last.1 - want).abs() < 0.01, "{last:?}");
    }
}
