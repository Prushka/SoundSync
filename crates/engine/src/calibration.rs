//! Microphone calibration.
//!
//! The node with the microphone coordinates: it pauses stream playback everywhere, asks each
//! node in turn to play a short sweep at a scheduled time (in the coordinator's clock), records,
//! and finds when each sweep actually arrived. Differences between arrival residuals are the
//! differences in unreported output latency; the microphone's own latency cancels out.

use std::sync::{Arc, Mutex};

use soundsync_audio::{CaptureInfo, CaptureSink, DeviceSelector, Stream, StreamFormat};
use soundsync_core::calib::{CalibReference, calibration_sweep, find_signal, solve_offsets};
use soundsync_core::protocol::{CalibMsg, CalibSignal, Control};
use soundsync_core::{MasterMode, Nanos, NodeId, Role};

use crate::node::Node;
use crate::status::CalibStatus;

const FIRST_SLOT_NS: Nanos = 1_500_000_000;
const SLOT_SPACING_NS: Nanos = 1_000_000_000;
const TAIL_NS: Nanos = 1_000_000_000;
const WINDOW_BEFORE_NS: Nanos = 50_000_000;
const WINDOW_AFTER_NS: Nanos = 700_000_000;
/// A participant unpauses on its own if the coordinator's End never arrives.
const PARTICIPANT_GRACE_NS: Nanos = 2_000_000_000;

#[derive(Default)]
struct Recording {
    rate: u32,
    /// (time of first frame, channel-0 samples)
    chunks: Vec<(Nanos, Vec<f32>)>,
}

struct MicSink(Arc<Mutex<Recording>>);

impl CaptureSink for MicSink {
    fn on_capture(&mut self, data: &[f32], format: StreamFormat, info: CaptureInfo) {
        let ch = format.channels.max(1) as usize;
        let mut r = self.0.lock().unwrap();
        r.rate = format.sample_rate;
        r.chunks.push((info.time_ns, data.iter().step_by(ch).copied().collect()));
    }
}

pub(crate) struct CalibRun {
    session: u32,
    _mic: Stream,
    rec: Arc<Mutex<Recording>>,
    /// (node, name, scheduled emission in our clock)
    slots: Vec<(NodeId, String, Nanos)>,
    until: Nanos,
    reference: CalibReference,
}

pub(crate) struct Participant {
    coordinator: NodeId,
    session: u32,
    until_local: Nanos,
}

#[derive(Default)]
pub(crate) struct CalibState {
    pub run: Option<CalibRun>,
    pub participant: Option<Participant>,
    pub status: CalibStatus,
}

impl Node {
    pub(crate) fn update_paused(&self) {
        let paused = self.calib.run.is_some() || self.calib.participant.is_some() || self.sync_test.is_some();
        self.media.mixer.paused.store(paused, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn streams_paused(&self) -> bool {
        self.media.mixer.paused.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn calibration_reference(&self) -> CalibReference {
        if self.capture_mode() == Some(MasterMode::Direct) {
            return CalibReference::Node(self.id());
        }
        self.peers
            .iter()
            .find(|p| p.state.as_ref().is_some_and(|s| s.role == Role::Master(MasterMode::Direct)))
            .map_or(CalibReference::Median, |p| CalibReference::Node(p.id))
    }

    pub(crate) fn start_calibration(&mut self, mic_device: &str) {
        if self.calib.run.is_some() {
            return;
        }
        let lower = mic_device.to_lowercase();
        if lower.contains("hands-free") || lower.contains("headset") {
            self.warn("A Bluetooth headset microphone switches the headset to call mode, which changes its latency; use another microphone for calibration");
        }
        let rec = Arc::new(Mutex::new(Recording::default()));
        let mic = match self.host.open_input(&DeviceSelector::parse(mic_device), Box::new(MicSink(rec.clone()))) {
            Ok(m) => m,
            Err(e) => {
                self.calib.status = CalibStatus::Failed(format!("cannot open microphone {mic_device}: {e}"));
                return;
            }
        };
        let session: u32 = rand::random::<u32>().max(1);
        let now = self.host.now_ns();
        let mut nodes: Vec<(NodeId, String)> = self.peers.iter().map(|p| (p.id, p.name.clone())).collect();
        nodes.push((self.id(), self.config.node.name.clone()));
        nodes.sort_by_key(|(id, _)| *id);
        let slots: Vec<(NodeId, String, Nanos)> =
            nodes.into_iter().enumerate().map(|(k, (id, name))| (id, name, now + FIRST_SLOT_NS + k as Nanos * SLOT_SPACING_NS)).collect();
        let until = slots.last().map_or(now, |s| s.2) + TAIL_NS;
        let begin = Control::Calib(CalibMsg::Begin { session, until_ns: until });
        for p in self.peers.iter() {
            self.send(p.addr, &begin);
        }
        for (id, _, at) in &slots {
            if *id == self.id() {
                if let Some(rate) = self.output_rate() {
                    self.play_signal(calibration_sweep(rate), *at);
                }
            } else if let Some(p) = self.peers.get(*id) {
                self.send(p.addr, &Control::Calib(CalibMsg::Play { session, at_ns: *at, signal: CalibSignal::Sweep }));
            }
        }
        tracing::info!("calibration {session:08x}: {} speakers, results in {:.1} s", slots.len(), (until - now) as f64 / 1e9);
        self.calib.status = CalibStatus::Running { step: 0, total: slots.len() };
        let reference = self.calibration_reference();
        self.calib.run = Some(CalibRun { session, _mic: mic, rec, slots, until, reference });
        self.update_paused();
    }

    pub(crate) fn calibration_tick(&mut self, now: Nanos) {
        if let Some(run) = &self.calib.run {
            let done = run.slots.iter().filter(|s| s.2 + WINDOW_AFTER_NS < now).count();
            self.calib.status = CalibStatus::Running { step: done, total: run.slots.len() };
            if now >= run.until {
                self.finish_calibration();
            }
        }
        if let Some(p) = &self.calib.participant
            && now > p.until_local + PARTICIPANT_GRACE_NS
        {
            self.calib.participant = None;
            self.update_paused();
        }
    }

    fn analyze(run: &CalibRun) -> Result<Vec<(NodeId, Nanos)>, String> {
        let rec = run.rec.lock().unwrap();
        let rate = rec.rate.max(1);
        let mut samples = Vec::new();
        let mut times = Vec::new();
        for (t0, chunk) in &rec.chunks {
            for (i, x) in chunk.iter().enumerate() {
                samples.push(*x);
                times.push(t0 + (i as i128 * 1_000_000_000 / rate as i128) as Nanos);
            }
        }
        let sweep = calibration_sweep(rate);
        let mut residuals = Vec::new();
        for (id, name, at) in &run.slots {
            let lo = times.partition_point(|t| *t < at - WINDOW_BEFORE_NS);
            let hi = times.partition_point(|t| *t < at + WINDOW_AFTER_NS);
            let det = (hi > lo).then(|| find_signal(&samples[lo..hi], &sweep)).flatten().ok_or_else(|| format!("no signal from {name}"))?;
            let arrival = times[lo] + (det.offset_frames * 1e9 / rate as f64).round() as Nanos;
            residuals.push((*id, arrival - at));
        }
        Ok(solve_offsets(&residuals, run.reference))
    }

    fn finish_calibration(&mut self) {
        let Some(run) = self.calib.run.take() else { return };
        let result = Self::analyze(&run);
        let (deltas, error) = match &result {
            Ok(d) => (d.clone(), None),
            Err(e) => (Vec::new(), Some(e.clone())),
        };
        for (id, delta) in &deltas {
            tracing::info!("calibration: {id:016x} needs {:+.2} ms", *delta as f64 / 1e6);
        }
        let msg = Control::Calib(CalibMsg::Result { session: run.session, deltas: deltas.clone(), error: error.clone() });
        let end = Control::Calib(CalibMsg::End { session: run.session });
        for p in self.peers.iter() {
            self.send(p.addr, &msg);
            self.send(p.addr, &end);
        }
        self.calib.status = match error {
            Some(e) => CalibStatus::Failed(e),
            None => CalibStatus::Done { session: run.session, deltas },
        };
        self.update_paused();
    }

    pub(crate) fn cancel_calibration(&mut self) {
        if let Some(run) = self.calib.run.take() {
            let end = Control::Calib(CalibMsg::End { session: run.session });
            for p in self.peers.iter() {
                self.send(p.addr, &end);
            }
        }
        self.calib.participant = None;
        self.calib.status = CalibStatus::Idle;
        self.update_paused();
    }

    /// Adds each measured delta to the node's current extra latency.
    pub(crate) fn apply_calibration(&mut self) {
        let CalibStatus::Done { deltas, .. } = self.calib.status.clone() else { return };
        for (node, delta) in deltas {
            if node == self.id() {
                let cur = self.config.extra_latency_ns();
                self.set_own_extra_latency(cur + delta);
            } else if let Some(p) = self.peers.get(node) {
                let cur = p.state.as_ref().map_or(0, |s| s.extra_latency_ns);
                self.send(p.addr, &Control::SetOffset { target: node, extra_latency_ns: cur + delta });
            }
        }
        self.calib.status = CalibStatus::Idle;
    }

    pub(crate) fn handle_calib(&mut self, sender: NodeId, msg: CalibMsg, now: Nanos) {
        match msg {
            CalibMsg::Request { mic_device } => self.start_calibration(&mic_device),
            CalibMsg::Begin { session, until_ns } => {
                let until_local = self.peers.get(sender).and_then(|p| p.clock_map()).map_or(now + 10_000_000_000, |m| m.remote_to_local(until_ns));
                self.calib.participant = Some(Participant { coordinator: sender, session, until_local });
                self.calib.status = CalibStatus::Running { step: 0, total: self.peers.len() + 1 };
                self.update_paused();
            }
            CalibMsg::Play { session, at_ns, signal: CalibSignal::Sweep } => {
                let ours = self.calib.participant.as_ref().is_some_and(|p| p.coordinator == sender && p.session == session);
                let map = self.peers.get(sender).and_then(|p| p.clock_map());
                if let (true, Some(map), Some(rate)) = (ours, map, self.output_rate()) {
                    self.play_signal(calibration_sweep(rate), map.remote_to_local(at_ns));
                }
            }
            CalibMsg::Play { .. } => {}
            CalibMsg::End { session } => {
                if self.calib.participant.as_ref().is_some_and(|p| p.session == session) {
                    self.calib.participant = None;
                    self.update_paused();
                }
            }
            CalibMsg::Result { session, deltas, error } => {
                self.calib.status = match error {
                    Some(e) => CalibStatus::Failed(e),
                    None => CalibStatus::Done { session, deltas },
                };
            }
        }
    }
}
