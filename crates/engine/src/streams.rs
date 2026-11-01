//! Audio orchestration on the engine thread: the output (mixer), the capture (master), the
//! per-stream receive slots, subscriptions, STATE gossip and the group delay.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use arc_swap::ArcSwap;
use soundsync_audio::{AudioError, DeviceSelector, Direction, EndpointVolume, LoopbackOptions, OutputOptions, Stream, select_device};
use soundsync_core::clock::ClockMap;
use soundsync_core::delay::{DelayPolicy, DelaySetting};
use soundsync_core::layout::{ChannelMap, MapKind, describe};
use soundsync_core::player::{PlayerConfig, StreamPlayer};
use soundsync_core::protocol::{Control, NodeState, OutputInfo, RxStat, StreamInfo, VolumeChange};
use soundsync_core::{MasterMode, Nanos, NodeId, Os, Role, StreamFormat};

use crate::chunk::AudioChunk;
use crate::command::Command;
use crate::devices::{FALLBACK_RETRY_NS, OUTPUT_CHANGED, Reopener};
use crate::leases::{LEASE_TTL_NS, Leases};
use crate::net::RxCmd;
use crate::node::Node;
use crate::receiver::{Mixer, MixerCmd, MixerGarbage, MixerShared, SlotStats, StreamSlot};
use crate::sender::{CaptureSender, SenderShared};
use crate::status::{DeviceStatus, Status, StreamStatus};

const STREAM_RING: usize = 512;
const SLOW_TICK_NS: Nanos = 1_000_000_000;
const FAST_TICK_NS: Nanos = 250_000_000;
const REQUIRED_GUARD_NS: Nanos = 2_000_000;
/// Before a receiver has measured anything, its estimate adds this to its device lead and period:
/// the master's capture period (~10 ms), one packet (~4 ms), the network, the resampler lookahead
/// and the guard.
const FALLBACK_HEADROOM_NS: Nanos = 20_000_000;
const DEFAULT_FORMAT: StreamFormat = StreamFormat { sample_rate: 48_000, channels: 2, channel_mask: 0x3 };
const ECHO_GUARD: &str = "Remote streams muted here: this PC is capturing its own output device (echo guard)";
const VBCABLE_MISSING: &str = "VB-CABLE (CABLE Input) not found — install it or switch to Direct mode";
const FALLBACK_SUFFIX: &str = " not found — using default output";
/// Error tag: the output would be the very device a virtual master captures (a feedback loop).
const CAPTURE_LOOP: &str = "the output is the captured device";
pub(crate) const MONITOR_IS_CAPTURE: &str =
    "The monitor output would be the VB-CABLE being broadcast (it is the Windows default) — choose your speakers as Monitor output in Overview ▸ This PC";
const OPEN_FAILED: &str = "cannot open audio output";

pub(crate) struct OutputState {
    pub(crate) stream: Stream,
    pub(crate) id: String,
    pub(crate) device: String,
    format: StreamFormat,
    cmds: rtrb::Producer<MixerCmd>,
    garbage: rtrb::Consumer<MixerGarbage>,
}

pub(crate) struct CaptureState {
    pub(crate) stream: Stream,
    pub(crate) selector: DeviceSelector,
    pub(crate) id: String,
    pub(crate) device: String,
    mode: MasterMode,
    shared: Arc<SenderShared>,
    leases: Leases,
    probe_ns: Nanos,
    /// Hands new local-stream producers to the sender (virtual mode only).
    local_updates: Option<rtrb::Producer<rtrb::Producer<AudioChunk>>>,
}

pub(crate) struct SlotHandle {
    master: NodeId,
    stream_id: u32,
    format: StreamFormat,
    stats: Arc<SlotStats>,
    clock: Arc<ArcSwap<ClockMap>>,
    local: bool,
    layout: MapKind,
}

pub(crate) struct Media {
    pub mixer: Arc<MixerShared>,
    pub output: Option<OutputState>,
    pub capture: Option<CaptureState>,
    pub delay: DelayPolicy,
    pub slots: Vec<SlotHandle>,
    pub echo_guard: bool,
    /// Some while the output is gone and being reopened.
    pub output_reopen: Option<Reopener>,
    /// The output that went away (for the "disconnected" warning and the recalibration hint).
    pub output_lost_device: Option<String>,
    /// The lost output was unplugged, not just interrupted.
    pub output_removed: bool,
    /// Playing on the default because the configured output is missing: when to look again.
    pub output_fallback_retry: Option<Nanos>,
    /// Some while the capture device is gone and broadcasting should resume once it is back.
    pub capture_reopen: Option<Reopener>,
    pub capture_lost_device: Option<String>,
    /// Stream id last announced to peers (a new one is gossiped at once, not on the next round).
    announced_stream: u32,
    /// The output device's own (OS) volume, when it has one: the Volume slider controls it.
    /// None (a null output, or no volume control): SoundSync's own software level is used.
    pub output_volume: Option<EndpointVolume>,
    /// The last requirement measured on this output, reported between streams so a restarted
    /// broadcast starts with a delay this node can make (forgotten when the output reopens).
    last_required: Option<Nanos>,
    last_slow: Nanos,
    last_fast: Nanos,
}

impl Media {
    pub(crate) fn new(config: &soundsync_core::config::Config) -> Media {
        Media {
            mixer: Arc::new(MixerShared::new(
                config.playback.volume,
                config.playback.muted,
                config.extra_latency_ns(),
                config.playback.follow_master_volume,
            )),
            output: None,
            capture: None,
            delay: DelayPolicy::new(config.sync.delay_ms, (config.sync.margin_ms * 1e6) as Nanos),
            slots: Vec::new(),
            echo_guard: false,
            output_reopen: None,
            output_lost_device: None,
            output_removed: false,
            output_fallback_retry: None,
            capture_reopen: None,
            capture_lost_device: None,
            announced_stream: 0,
            last_required: None,
            output_volume: None,
            last_slow: 0,
            last_fast: 0,
        }
    }
}

fn mismatch(kind: MapKind) -> bool {
    matches!(kind, MapKind::Downmix | MapKind::Upmix)
}

impl Node {
    fn output_selector(&self) -> DeviceSelector {
        let virtual_master = self.media.capture.as_ref().is_some_and(|c| c.mode == MasterMode::Virtual);
        DeviceSelector::parse(if virtual_master { &self.config.master.monitor_device } else { &self.config.playback.output_device })
    }

    /// (id, name, mix format) of the device `sel` picks.
    fn device_format(&self, dir: Direction, sel: &DeviceSelector) -> Result<(String, String, StreamFormat), AudioError> {
        if *sel == DeviceSelector::Null {
            return Ok(("null".into(), "null".into(), DEFAULT_FORMAT));
        }
        let devices = self.host.devices(dir)?;
        let d = select_device(&devices, sel)?;
        Ok((d.id.clone(), d.name.clone(), d.mix_format))
    }

    /// The multimedia default saved before we switched the OS default to the cable, if any.
    fn pre_switch_default(&self) -> Option<String> {
        let capture = self.media.capture.as_ref().map(|c| c.id.as_str());
        self.defaults.as_ref().and_then(|g| g.saved()).map(|d| d.multimedia).filter(|id| !id.is_empty() && Some(id.as_str()) != capture)
    }

    /// The output follows the OS default (none configured, or the configured one is missing).
    pub(crate) fn output_on_default(&self) -> bool {
        self.output_selector() == DeviceSelector::Default || self.media.output_fallback_retry.is_some()
    }

    pub(crate) fn configured_output_present(&self) -> bool {
        self.device_format(Direction::Render, &self.output_selector()).is_ok()
    }

    fn try_open_output(&self, sel: &DeviceSelector) -> Result<OutputState, AudioError> {
        let (id, device, format) = self.device_format(Direction::Render, sel)?;
        if self.media.capture.as_ref().is_some_and(|c| c.mode == MasterMode::Virtual && c.id == id) {
            return Err(AudioError::Unsupported(CAPTURE_LOOP));
        }
        let (cmds, cmd_rx) = rtrb::RingBuffer::new(64);
        let (garbage_tx, garbage) = rtrb::RingBuffer::new(64);
        let mixer = Mixer::new(format, self.media.mixer.clone(), cmd_rx, garbage_tx);
        let opts = OutputOptions { low_latency: self.config.playback.low_latency };
        let stream = self.host.open_output(sel, opts, Box::new(mixer))?;
        if stream.format() != format {
            return Err(AudioError::Os(format!("{device} opened as {:?}, expected {format:?}", stream.format())));
        }
        Ok(OutputState { stream, id, device, format, cmds, garbage })
    }

    /// (Re)opens the output for the current mode. All receive slots are rebuilt afterwards.
    /// A missing configured device falls back to the default output (looked for again every
    /// 5 s); if nothing can be opened, reopening is retried with backoff.
    pub(crate) fn open_output(&mut self) {
        self.drop_all_slots();
        self.media.output = None;
        self.media.last_required = None;
        let now = self.host.now_ns();
        let sel = self.output_selector();
        let mut fallback = false;
        let opened = match self.try_open_output(&sel) {
            Ok(o) => Ok(o),
            Err(AudioError::NotFound(_) | AudioError::Ambiguous(_)) if sel != DeviceSelector::Default => {
                self.warn(format!("{sel}{FALLBACK_SUFFIX}"));
                fallback = true;
                self.try_open_output(&DeviceSelector::Default)
            }
            Err(e) => Err(e),
        };
        // "Default" resolved to the captured cable (made default by us or by the user): play
        // where the default was before our switch, or not at all.
        let opened = match opened {
            Err(AudioError::Unsupported(CAPTURE_LOOP)) => match self.pre_switch_default() {
                Some(id) => self.try_open_output(&DeviceSelector::Id(id)),
                None => Err(AudioError::Unsupported(CAPTURE_LOOP)),
            },
            other => other,
        };
        match opened {
            Ok(o) => {
                tracing::info!("output: {} ({} {} Hz)", o.device, describe(o.format), o.format.sample_rate);
                self.media.output = Some(o);
                self.poll_output_volume();
                self.warnings.retain(|w| !w.starts_with(OPEN_FAILED) && w != MONITOR_IS_CAPTURE);
                if fallback {
                    self.media.output_fallback_retry.get_or_insert(now + FALLBACK_RETRY_NS);
                } else {
                    self.media.output_fallback_retry = None;
                    self.warnings.retain(|w| !w.ends_with(FALLBACK_SUFFIX));
                }
                if let Some(c) = &self.media.capture
                    && c.mode == MasterMode::Virtual
                {
                    self.add_local_slot();
                }
            }
            Err(AudioError::Unsupported(CAPTURE_LOOP)) => {
                // Retrying cannot help; the user picks a monitor (or the default changes back).
                self.warn(MONITOR_IS_CAPTURE);
                self.media.output_fallback_retry = None;
            }
            Err(e) => {
                self.warn(format!("{OPEN_FAILED} {sel}: {e}"));
                self.media.output_fallback_retry = None;
                if self.media.output_reopen.is_none() {
                    let mut r = Reopener::new();
                    r.failed(now);
                    self.media.output_reopen = Some(r);
                }
            }
        }
        self.update_echo_guard();
    }

    fn update_echo_guard(&mut self) {
        let guard = match (&self.media.capture, &self.media.output) {
            (Some(c), Some(o)) => c.mode == MasterMode::Direct && c.device == o.device,
            _ => false,
        };
        self.media.echo_guard = guard;
        if guard {
            self.warn(ECHO_GUARD);
            self.drop_remote_slots();
        } else {
            self.warnings.retain(|w| w != ECHO_GUARD);
        }
    }

    fn add_slot(&mut self, master: NodeId, stream_id: u32, format: StreamFormat, clock: ClockMap, local: bool, rx: rtrb::Consumer<AudioChunk>) {
        let Some(out) = &mut self.media.output else { return };
        let out_format = out.format;
        let layout = ChannelMap::new(format, out_format).kind();
        let player = StreamPlayer::new(format, PlayerConfig::for_output(out_format));
        let stats = Arc::new(SlotStats::default());
        let clock = Arc::new(ArcSwap::from_pointee(clock));
        let slot = StreamSlot { master, stream_id, player, rx, clock: clock.clone(), stats: stats.clone(), gain: 1.0 };
        if out.cmds.push(MixerCmd::AddStream(Box::new(slot))).is_ok() {
            self.media.slots.push(SlotHandle { master, stream_id, format, stats, clock, local, layout });
            if mismatch(layout) {
                // Shown while it lasts (layout_warnings); logged once here.
                tracing::warn!("{}", self.layout_warning(master, local, format, out_format));
            }
        }
    }

    fn layout_warning(&self, master: NodeId, local: bool, format: StreamFormat, out_format: StreamFormat) -> String {
        let name = if local { "This PC".to_string() } else { self.peers.get(master).map_or_else(|| format!("{master:016x}"), |p| p.name.clone()) };
        format!("{name}: {} → {} (channels remapped; match the speaker layouts in Sound settings)", describe(format), describe(out_format))
    }

    /// One warning per stream currently remapped to a different speaker layout (recomputed for
    /// every status, so it clears as soon as the layouts match).
    pub(crate) fn layout_warnings(&self) -> Vec<String> {
        let Some(out) = &self.media.output else { return Vec::new() };
        let mut w: Vec<String> = self.media.slots.iter().filter(|s| mismatch(s.layout)).map(|s| self.layout_warning(s.master, s.local, s.format, out.format)).collect();
        w.dedup();
        w
    }

    /// (Re)creates the slot that plays this master's own stream on the monitor output.
    fn add_local_slot(&mut self) {
        let Some(c) = &mut self.media.capture else { return };
        let Some(updates) = &mut c.local_updates else { return };
        let format = c.stream.format();
        let (tx, rx) = rtrb::RingBuffer::new(STREAM_RING);
        if updates.push(tx).is_err() {
            return;
        }
        let id = self.id();
        self.add_slot(id, 0, format, ClockMap::IDENTITY, true, rx);
    }

    fn remove_slot(&mut self, i: usize) {
        let h = self.media.slots.swap_remove(i);
        if let Some(out) = &mut self.media.output {
            let _ = out.cmds.push(MixerCmd::RemoveStream { master: h.master, stream_id: h.stream_id });
        }
        if !h.local {
            let _ = self.routes.send(RxCmd::RemoveRoute { master: h.master, stream_id: h.stream_id });
        }
    }

    fn drop_remote_slots(&mut self) {
        let mut i = 0;
        while i < self.media.slots.len() {
            if self.media.slots[i].local {
                i += 1;
            } else {
                self.remove_slot(i);
            }
        }
    }

    pub(crate) fn drop_all_slots(&mut self) {
        while !self.media.slots.is_empty() {
            self.remove_slot(0);
        }
    }

    /// Requirement this node reports: how much delay it needs between capture and emission.
    pub(crate) fn required_delay(&self) -> Nanos {
        let measured = self.media.slots.iter().filter_map(|s| s.stats.required()).max().or(self.media.last_required);
        match measured {
            Some(v) => v + REQUIRED_GUARD_NS,
            None => {
                // Not measured yet: the same parts a measurement has (the device's worst lead and
                // one period; the headroom stands in for capture, packet, network and lookahead).
                let lead = self.media.mixer.lead_ns.load(Ordering::Relaxed).max(0);
                let period = self.media.mixer.period_ns.load(Ordering::Relaxed).max(0);
                lead + period + self.config.extra_latency_ns() + FALLBACK_HEADROOM_NS
            }
        }
    }

    fn capture_selector(&self, mode: MasterMode) -> DeviceSelector {
        match (mode, self.config.master.capture_device.trim()) {
            (MasterMode::Virtual, "") => DeviceSelector::Name("CABLE Input".into()),
            (MasterMode::Direct, "") => DeviceSelector::Default,
            (_, name) => DeviceSelector::parse(name),
        }
    }

    pub(crate) fn start_broadcast(&mut self) {
        if self.media.capture.is_some() {
            return;
        }
        let mode = self.config.master.mode;
        let sel = self.capture_selector(mode);
        let (id, device) = match self.device_format(Direction::Render, &sel) {
            Ok((id, name, _)) => (id, name),
            Err(e) => {
                if mode == MasterMode::Virtual && matches!(e, AudioError::NotFound(_)) {
                    self.warn(VBCABLE_MISSING);
                } else {
                    self.warn(format!("cannot capture {sel}: {e}"));
                }
                return;
            }
        };
        let shared = Arc::new(SenderShared::new());
        let host = self.host.clone();
        let mut sender = CaptureSender::new(
            self.node_id.clone(),
            self.transport.key().clone(),
            self.transport.clone(),
            shared.clone(),
            None,
            Arc::new(move || host.now_ns()),
        );
        let mut local_updates = None;
        if mode == MasterMode::Virtual {
            let (tx, rx) = rtrb::RingBuffer::new(4);
            sender = sender.with_local_updates(rx);
            local_updates = Some(tx);
        }
        let opts = LoopbackOptions { keepalive_low_latency: self.config.master.keepalive_low_latency, no_keepalive: false };
        let stream = match self.host.open_loopback(&sel, opts, Box::new(sender)) {
            Ok(s) => s,
            Err(AudioError::Unsupported(_)) => {
                self.warn("This device can only play audio from other masters (receive-only)");
                return;
            }
            Err(e) => {
                self.warn(format!("cannot capture {device}: {e}"));
                return;
            }
        };
        let probe_ns = if mode == MasterMode::Direct { self.host.probe_loopback_latency(&sel).unwrap_or(0) } else { 0 };
        let format = stream.format();
        tracing::info!("broadcasting {device} ({mode:?}, {} {} Hz)", describe(format), format.sample_rate);
        self.warnings.retain(|w| w != VBCABLE_MISSING);
        self.media.capture = Some(CaptureState { stream, selector: sel, id, device, mode, shared, leases: Leases::new(LEASE_TTL_NS), probe_ns, local_updates });
        self.update_presentation_offset(true);
        if let Some(d) = &mut self.discovery {
            d.update_role(Role::Master(mode));
        }
        // Virtual mode plays our own stream on the monitor output (open_output adds that slot).
        self.open_output();
        self.switch_default_if_wanted();
    }

    pub(crate) fn capture_device_name(&self) -> Option<String> {
        self.media.capture.as_ref().map(|c| c.device.clone())
    }

    /// Makes the captured VB-CABLE the OS default output if the user asked for that.
    fn switch_default_if_wanted(&mut self) {
        let Some(c) = &self.media.capture else { return };
        if c.mode != MasterMode::Virtual || !self.config.master.switch_default {
            return;
        }
        let id = self.host.devices(Direction::Render).ok().and_then(|d| d.into_iter().find(|d| d.name == c.device).map(|d| d.id));
        if let (Some(id), Some(g)) = (id, &mut self.defaults)
            && let Err(e) = g.switch_to(&id)
        {
            self.warn(format!("cannot switch the default output: {e}"));
        }
    }

    pub(crate) fn restore_defaults(&mut self) {
        if let Some(g) = &mut self.defaults {
            match g.restore() {
                Ok(None) => {}
                Ok(Some(w)) => self.warn(w),
                Err(e) => self.warn(format!("cannot restore the default output: {e}")),
            }
        }
    }

    pub(crate) fn stop_broadcast(&mut self) {
        self.clear_capture_recovery();
        let was_capturing = self.media.capture.take().is_some();
        // Also while the capture device is gone: the default-output switch outlives the outage.
        self.restore_defaults();
        if !was_capturing {
            return;
        }
        self.silence_suspect = false;
        tracing::info!("broadcast stopped");
        if let Some(d) = &mut self.discovery {
            d.update_role(Role::Receiver);
        }
        self.open_output();
    }

    /// Delay requirements of this node and of every peer that plays (or will play) our stream.
    /// Peers that play remote streams count before they subscribe: they only subscribe after
    /// hearing of a new stream, and its first group delay must already cover them.
    fn peer_requirements(&mut self, now: Nanos) -> Vec<Nanos> {
        let own = self.required_delay();
        let subscribers: Vec<SocketAddr> = self.media.capture.as_mut().map_or_else(Vec::new, |c| c.leases.active(now));
        let mut reqs = vec![own];
        reqs.extend(
            self.peers
                .iter()
                .filter_map(|p| p.state.as_ref().filter(|s| s.plays_remote || subscribers.contains(&p.addr)).map(|s| s.required_delay_ns)),
        );
        reqs
    }

    /// Sets the offset the sender stamps on packets: the group delay (virtual) or the master's
    /// own output path (direct).
    fn update_presentation_offset(&mut self, stream_start: bool) {
        let now = self.host.now_ns();
        let Some(mode) = self.media.capture.as_ref().map(|c| c.mode) else { return };
        let value = match mode {
            MasterMode::Direct => self.media.capture.as_ref().map(|c| c.probe_ns).unwrap_or(0) + self.config.extra_latency_ns(),
            MasterMode::Virtual => {
                let reqs = self.peer_requirements(now);
                if stream_start {
                    self.media.delay.on_stream_start(&reqs)
                } else {
                    match self.media.delay.update(&reqs) {
                        Some(d) => {
                            tracing::info!("group delay → {} ms", d / 1_000_000);
                            d
                        }
                        None => return,
                    }
                }
            }
        };
        if let Some(c) = &self.media.capture {
            c.shared.presentation_offset_ns.store(value, Ordering::Relaxed);
        }
    }

    fn presentation_offset(&self) -> Option<Nanos> {
        self.media.capture.as_ref().map(|c| c.shared.presentation_offset_ns.load(Ordering::Relaxed))
    }

    fn node_state(&self) -> NodeState {
        let stream = self.media.capture.as_ref().and_then(|c| {
            let id = c.shared.stream_id.load(Ordering::Acquire);
            let format = (**c.shared.format.load())?;
            (id != 0).then_some(StreamInfo { stream_id: id, format, mode: c.mode, presentation_offset_ns: c.shared.presentation_offset_ns.load(Ordering::Relaxed) })
        });
        let volume = self.own_volume();
        NodeState {
            name: self.config.node.name.clone(),
            os: Os::current(),
            session: self.session,
            role: self.media.capture.as_ref().map_or(Role::Receiver, |c| Role::Master(c.mode)),
            stream,
            output: self.media.output.as_ref().map(|o| OutputInfo { device: o.device.clone(), format: o.format, device_latency_ns: o.stream.latency_ns() }),
            extra_latency_ns: self.config.extra_latency_ns(),
            required_delay_ns: self.required_delay(),
            plays_remote: self.config.playback.play_remote && !self.media.echo_guard,
            volume: volume.volume,
            muted: volume.muted,
            device_volume: volume.device,
            rx: self
                .media
                .slots
                .iter()
                .filter(|s| !s.local)
                .map(|s| RxStat {
                    master: s.master,
                    stream_id: s.stream_id,
                    sync_error_ns: s.stats.error_ns.load(Ordering::Relaxed),
                    behind_ns: s.stats.behind_ns.load(Ordering::Relaxed),
                    lost_frames: s.stats.lost_frames.load(Ordering::Relaxed),
                })
                .collect(),
        }
    }

    /// Remote streams this node should be playing: (master, stream info).
    fn wanted_streams(&self) -> Vec<(NodeId, StreamInfo)> {
        if !self.config.playback.play_remote || self.media.echo_guard {
            return Vec::new();
        }
        self.peers
            .iter()
            .filter(|p| p.clock_map().is_some())
            .filter_map(|p| p.state.as_ref().and_then(|s| s.stream.clone()).map(|s| (p.id, s)))
            .collect()
    }

    fn sync_slots(&mut self) {
        let wanted = self.wanted_streams();
        let mut i = 0;
        while i < self.media.slots.len() {
            let s = &self.media.slots[i];
            let keep = s.local || (!s.stats.format_changed.load(Ordering::Relaxed) && wanted.iter().any(|(m, info)| *m == s.master && info.stream_id == s.stream_id && info.format == s.format));
            if keep {
                i += 1;
            } else {
                self.remove_slot(i);
            }
        }
        for (master, info) in wanted {
            if self.media.slots.iter().any(|s| s.master == master && s.stream_id == info.stream_id) {
                continue;
            }
            let Some(map) = self.peers.get(master).and_then(|p| p.clock_map()) else { continue };
            let (tx, rx) = rtrb::RingBuffer::new(STREAM_RING);
            let _ = self.routes.send(RxCmd::AddRoute { master, stream_id: info.stream_id, tx });
            self.add_slot(master, info.stream_id, info.format, map, false, rx);
            // Subscribe now rather than at the next lease renewal, so playback starts promptly.
            if let Some(p) = self.peers.get(master) {
                self.send(p.addr, &Control::Subscribe { stream_id: info.stream_id });
            }
        }
    }

    pub(crate) fn send_state(&self) {
        let state = Control::State(self.node_state());
        for p in self.peers.iter() {
            self.send(p.addr, &state);
        }
    }

    pub(crate) fn media_tick(&mut self, now: Nanos) {
        if let Some(out) = &mut self.media.output {
            while let Ok(g) = out.garbage.pop() {
                drop(g);
            }
        }
        self.sync_slots();
        if let Some(v) = self.media.slots.iter().filter_map(|s| s.stats.required()).max() {
            self.media.last_required = Some(v);
        }
        let stream = self.media.capture.as_ref().map_or(0, |c| c.shared.stream_id.load(Ordering::Acquire));
        if stream != self.media.announced_stream && self.node_state().stream.is_some() {
            self.media.announced_stream = stream;
            self.send_state();
        }
        if now - self.media.last_fast >= FAST_TICK_NS {
            self.media.last_fast = now;
            self.poll_output_volume();
            for s in &self.media.slots {
                if !s.local
                    && let Some(m) = self.peers.get(s.master).and_then(|p| p.clock_map())
                {
                    s.clock.store(Arc::new(m));
                }
            }
            if let Some(c) = &mut self.media.capture {
                let subs = c.leases.active(now);
                c.shared.subscribers.store(Arc::new(subs));
                // Receivers follow the master's volume as the OS applies it (its dB level, not the
                // slider position); loopback capture comes before that volume.
                if let Ok(v) = self.host.endpoint_volume(&c.selector) {
                    c.shared.volume_bits.store(v.gain().to_bits(), Ordering::Relaxed);
                    c.shared.muted.store(v.muted, Ordering::Relaxed);
                }
            }
        }
        if now - self.media.last_slow >= SLOW_TICK_NS {
            self.media.last_slow = now;
            self.send_state();
            for (master, info) in self.wanted_streams() {
                if let Some(p) = self.peers.get(master) {
                    self.send(p.addr, &Control::Subscribe { stream_id: info.stream_id });
                }
            }
            self.update_presentation_offset(false);
            if let Some(c) = &self.media.capture {
                let sessions = self.host.active_sessions(&c.selector).unwrap_or(0);
                let silent = c.shared.silent.load(Ordering::Relaxed);
                self.silence_suspect = self.watchdog.update(now, silent, sessions);
            }
        }
    }

    /// Handles stream-related control messages; returns false if not one of them.
    pub(crate) fn handle_media_control(&mut self, from: SocketAddr, sender: NodeId, control: &Control, now: Nanos) -> bool {
        // Audio only ever goes to the address the peer handshook from: a SUBSCRIBE naming a
        // known id from elsewhere must not redirect a stream there.
        let peer_addr = self.peers.get(sender).map(|p| p.addr).filter(|a| *a == from);
        match control {
            Control::Subscribe { .. } => {
                if let (Some(c), Some(addr)) = (&mut self.media.capture, peer_addr) {
                    c.leases.renew(addr, now);
                }
            }
            Control::Unsubscribe { .. } => {
                if let (Some(c), Some(addr)) = (&mut self.media.capture, peer_addr) {
                    c.leases.remove(addr);
                }
            }
            Control::SetOffset { target, extra_latency_ns } => {
                if *target == self.id() {
                    self.set_own_extra_latency(*extra_latency_ns);
                }
            }
            Control::SetVolume { target, change } => {
                if *target == self.id() {
                    self.change_own_volume(*change);
                }
            }
            _ => return false,
        }
        true
    }

    pub(crate) fn set_own_extra_latency(&mut self, ns: Nanos) {
        // A new (calibrated or hand-tuned) value accounts for the changed output.
        self.warnings.retain(|w| w != OUTPUT_CHANGED);
        let ms = (ns as f64 / 1e6).clamp(-1000.0, 2000.0);
        self.config.playback.extra_latency_ms = ms;
        self.media.mixer.extra_latency_ns.store(self.config.extra_latency_ns(), Ordering::Relaxed);
        self.save_config();
        if self.media.capture.as_ref().is_some_and(|c| c.mode == MasterMode::Direct) {
            self.update_presentation_offset(false);
        }
    }

    fn restart_broadcast(&mut self) {
        if self.media.capture.is_some() {
            self.media.capture = None;
            self.restore_defaults();
            self.start_broadcast();
        }
    }

    /// Reopens the capture as it is configured, keeping a default-output switch in place.
    pub(crate) fn restart_capture(&mut self) {
        if self.media.capture.take().is_some() {
            self.start_broadcast();
        }
    }

    /// Applies an audio command; returns false for commands handled elsewhere.
    pub(crate) fn handle_media_command(&mut self, c: &Command) -> bool {
        match c {
            Command::SetBroadcast(on) => {
                self.config.master.enabled = *on;
                self.save_config();
                if *on { self.start_broadcast() } else { self.stop_broadcast() }
            }
            Command::SetMasterMode(m) => {
                self.config.master.mode = *m;
                self.save_config();
                self.restart_broadcast();
            }
            Command::SetCaptureDevice(d) => {
                self.config.master.capture_device = d.clone();
                self.save_config();
                self.restart_broadcast();
            }
            Command::SetMonitorDevice(d) => {
                self.config.master.monitor_device = d.clone();
                self.save_config();
                self.open_output();
            }
            Command::SetOutputDevice(d) => {
                self.config.playback.output_device = d.clone();
                self.save_config();
                self.open_output();
            }
            Command::SetVolume(v) => self.change_own_volume(VolumeChange::Set(*v)),
            Command::SetMuted(m) => self.change_own_volume(VolumeChange::Muted(*m)),
            Command::ChangeVolume { node, change } => self.change_volume(*node, *change),
            Command::SetFollowMasterVolume(f) => {
                self.config.playback.follow_master_volume = *f;
                self.media.mixer.follow_master_volume.store(*f, Ordering::Relaxed);
                self.save_config();
            }
            Command::SetPlayRemote(p) => {
                self.config.playback.play_remote = *p;
                self.save_config();
                if !*p {
                    let subscribed: Vec<(NodeId, u32)> = self.media.slots.iter().filter(|s| !s.local).map(|s| (s.master, s.stream_id)).collect();
                    for (master, stream_id) in subscribed {
                        if let Some(peer) = self.peers.get(master) {
                            self.send(peer.addr, &Control::Unsubscribe { stream_id });
                        }
                    }
                    self.drop_remote_slots();
                }
            }
            Command::SetExtraLatency { node, ns } => {
                if *node == self.id() {
                    self.set_own_extra_latency(*ns);
                } else if let Some(p) = self.peers.get(*node) {
                    self.send(p.addr, &Control::SetOffset { target: *node, extra_latency_ns: *ns });
                }
            }
            Command::SetDelay(setting) => {
                self.config.sync.delay_ms = *setting;
                self.save_config();
                let d = self.media.delay.set_setting(*setting);
                if let Some(c) = &self.media.capture
                    && c.mode == MasterMode::Virtual
                {
                    let d = if matches!(setting, DelaySetting::Auto) { self.media.delay.current().unwrap_or(d) } else { d };
                    c.shared.presentation_offset_ns.store(d, Ordering::Relaxed);
                }
            }
            Command::SetSwitchDefault(b) => {
                self.config.master.switch_default = *b;
                self.save_config();
                if *b { self.switch_default_if_wanted() } else { self.restore_defaults() }
            }
            _ => return false,
        }
        true
    }

    /// Sample rate of the open output, if any.
    pub(crate) fn output_rate(&self) -> Option<u32> {
        self.media.output.as_ref().map(|o| o.format.sample_rate)
    }

    /// Schedules a mono signal to be heard at `emit_at_ns` (local clock).
    pub(crate) fn play_signal(&mut self, samples: Vec<f32>, emit_at_ns: Nanos) -> bool {
        match &mut self.media.output {
            Some(o) => o.cmds.push(MixerCmd::Play(Box::new(crate::receiver::ScheduledSignal { samples, emit_at_ns }))).is_ok(),
            None => false,
        }
    }

    /// Current capture mode when broadcasting.
    pub(crate) fn capture_mode(&self) -> Option<MasterMode> {
        self.media.capture.as_ref().map(|c| c.mode)
    }

    pub(crate) fn fill_media_status(&self, st: &mut Status) {
        st.broadcasting = self.media.capture.is_some();
        st.capture = self.media.capture.as_ref().map(|c| DeviceStatus { name: c.device.clone(), format: c.stream.format(), latency_ns: c.probe_ns });
        st.output = self.media.output.as_ref().map(|o| DeviceStatus { name: o.device.clone(), format: o.format, latency_ns: o.stream.latency_ns() });
        st.delay_ns = self.presentation_offset();
        let v = self.own_volume();
        (st.volume, st.muted) = (v.volume, v.muted);
        st.volume_device = self.media.output.as_ref().filter(|_| v.device).map(|o| o.device.clone());
        let out_format = self.media.output.as_ref().map(|o| o.format);
        st.streams = self
            .media
            .slots
            .iter()
            .map(|s| StreamStatus {
                master: s.master,
                stream_id: s.stream_id,
                format: s.format,
                state: s.stats.state(),
                error_ns: s.stats.error_ns.load(Ordering::Relaxed),
                behind_ns: s.stats.behind_ns.load(Ordering::Relaxed),
                lost_frames: s.stats.lost_frames.load(Ordering::Relaxed),
                layout_warning: out_format.filter(|_| mismatch(s.layout)).map(|o| format!("{} → {}", describe(s.format), describe(o))),
                master_volume: f32::from_bits(s.stats.master_volume_bits.load(Ordering::Relaxed)),
            })
            .collect();
    }
}
