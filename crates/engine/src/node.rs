//! The engine thread: handshakes, clock pings, peer liveness, commands and status snapshots.

use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, select, tick};
use soundsync_audio::{AudioHost, DeviceEvent};
use soundsync_core::clock::ClockSample;
use soundsync_core::config::Config;
use soundsync_core::protocol::{Control, Hello};
use soundsync_core::{Nanos, NodeId, Os, Role};

use crate::calibration::CalibState;
use crate::command::Command;
use crate::synctest::SyncTestState;
use crate::discovery::{Discovery, inbound_suspect};
use crate::net::{Inbound, RxCmd, RxCounters, Transport};
use crate::peers::PeerTable;
use crate::power::{GapDetector, wall_ns};
use crate::status::{PeerStatus, Status};
use crate::streams::Media;

const HELLO_RETRY_NS: Nanos = 1_000_000_000;
const STATUS_EVERY_NS: Nanos = 100_000_000;
pub(crate) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) struct Node {
    pub config: Config,
    pub config_path: Option<PathBuf>,
    /// Fingerprint of the config file as last loaded or written by this engine.
    pub config_fingerprint: Arc<AtomicU64>,
    pub host: Arc<dyn AudioHost>,
    pub transport: Arc<Transport>,
    pub node_id: Arc<AtomicU64>,
    pub session: u64,
    pub peers: PeerTable,
    pub status: Arc<ArcSwap<Status>>,
    pub routes: Sender<RxCmd>,
    pub media: Media,
    pub calib: CalibState,
    pub sync_test: Option<SyncTestState>,
    pub defaults: Option<crate::defaults::DefaultSwitchGuard>,
    pub watchdog: crate::defaults::SilenceWatchdog,
    pub silence_suspect: bool,
    pub counters: Arc<RxCounters>,
    pub discovery: Option<Discovery>,
    /// When we first contacted an mDNS-discovered (so: running) peer without any peer answering.
    first_unanswered: Option<Nanos>,
    inbound_ok: bool,
    pub warnings: Vec<String>,
    /// Addresses we keep saying HELLO to until they answer (static peers, discovery).
    pub hello_targets: Vec<SocketAddr>,
    /// LANs this node uses; peers must be on one of them (or loopback, or static).
    pub local_nets: Vec<crate::discovery::LocalNet>,
    static_ips: Vec<std::net::IpAddr>,
    last_hello: HashMap<SocketAddr, Nanos>,
    last_status: Nanos,
    /// Device added/removed/default-changed notifications from the audio host.
    pub device_events: Option<Receiver<DeviceEvent>>,
    _device_watch: Option<Box<dyn Send>>,
    gap_host: GapDetector,
    gap_wall: GapDetector,
}

fn resolve_static(peers: &[String]) -> Vec<SocketAddr> {
    peers
        .iter()
        .filter_map(|p| match p.to_socket_addrs() {
            Ok(mut addrs) => addrs.find(|a| a.is_ipv4()),
            Err(e) => {
                tracing::warn!("static peer {p:?}: {e}");
                None
            }
        })
        .collect()
}

impl Node {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config: Config,
        config_path: Option<PathBuf>,
        host: Arc<dyn AudioHost>,
        transport: Arc<Transport>,
        node_id: Arc<AtomicU64>,
        status: Arc<ArcSwap<Status>>,
        routes: Sender<RxCmd>,
        counters: Arc<RxCounters>,
    ) -> Node {
        let hello_targets = resolve_static(&config.network.static_peers);
        let static_ips = hello_targets.iter().map(|a| a.ip()).collect();
        let media = Media::new(&config);
        Node {
            media,
            calib: CalibState::default(),
            sync_test: None,
            defaults: None,
            watchdog: crate::defaults::SilenceWatchdog::default(),
            silence_suspect: false,
            config,
            config_path,
            config_fingerprint: Arc::default(),
            host,
            transport,
            node_id,
            session: rand::random(),
            peers: PeerTable::default(),
            status,
            routes,
            counters,
            discovery: None,
            first_unanswered: None,
            inbound_ok: true,
            warnings: Vec::new(),
            hello_targets,
            local_nets: Vec::new(),
            static_ips,
            last_hello: HashMap::new(),
            last_status: 0,
            device_events: None,
            _device_watch: None,
            gap_host: GapDetector::default(),
            gap_wall: GapDetector::default(),
        }
    }

    /// Subscribes to device notifications. Runs on the engine thread: on Windows this joins the
    /// thread to the multithreaded COM apartment, which the UI thread must stay out of (OLE).
    fn watch_devices(&mut self) {
        let (tx, rx) = crossbeam_channel::unbounded();
        match self.host.watch(Box::new(move |e| {
            let _ = tx.send(e);
        })) {
            Ok(w) => {
                self._device_watch = Some(w);
                self.device_events = Some(rx);
            }
            Err(e) => tracing::warn!("device change notifications unavailable: {e}"),
        }
    }

    /// Restores OS defaults left switched by a crash or an unclean Windows shutdown. Runs on the
    /// engine thread: it enumerates devices through COM, which must stay off the UI thread.
    fn recover_defaults(&mut self) {
        if let Some(msg) = self.defaults.as_mut().and_then(|g| g.recover_on_start()) {
            self.warn(msg);
        }
    }

    pub(crate) fn id(&self) -> NodeId {
        self.node_id.load(Ordering::Relaxed)
    }

    pub(crate) fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !self.warnings.contains(&msg) {
            tracing::warn!("{msg}");
            self.warnings.push(msg);
        }
    }

    pub(crate) fn save_config(&mut self) {
        let Some(path) = self.config_path.clone() else { return };
        let saved = self.config.to_text().and_then(|text| {
            // Published before the write so the UI never mistakes our own save for an edit.
            let previous = self.config_fingerprint.swap(Config::fingerprint(text.as_bytes()), Ordering::AcqRel);
            Config::save_text(&path, &text).inspect_err(|_| self.config_fingerprint.store(previous, Ordering::Release))
        });
        if let Err(e) = saved {
            self.warn(format!("cannot save {}: {e}", path.display()));
        }
    }

    fn hello(&self, reply: bool) -> Control {
        Control::Hello(Hello {
            reply,
            name: self.config.node.name.clone(),
            os: Os::current(),
            app_version: APP_VERSION.into(),
            group: self.config.network.group.clone(),
            session: self.session,
        })
    }

    pub(crate) fn send(&self, to: SocketAddr, c: &Control) {
        self.transport.send_control(to, c);
    }

    fn say_hello(&mut self, to: SocketAddr, now: Nanos) {
        let due = self.last_hello.get(&to).is_none_or(|t| now - t >= HELLO_RETRY_NS);
        if due {
            self.last_hello.insert(to, now);
            let h = self.hello(false);
            self.send(to, &h);
        }
    }

    fn on_hello(&mut self, from: SocketAddr, sender: NodeId, hello: Hello, now: Nanos) {
        if hello.group != self.config.network.group {
            return;
        }
        if !crate::discovery::hello_allowed(from.ip(), &self.local_nets, &self.static_ips) {
            tracing::debug!("ignoring HELLO from {from}: not on a local network");
            return;
        }
        if sender == self.id() {
            if hello.session == self.session {
                return; // our own HELLO came back (e.g. discovered ourselves)
            }
            // Two nodes share an id (a copied config). The one with the lower session renames.
            if self.session < hello.session {
                let old = self.id();
                self.config.regenerate_id();
                self.node_id.store(self.config.node.id, Ordering::Relaxed);
                self.save_config();
                if let Some(d) = &mut self.discovery {
                    d.update_id(self.config.node.id);
                }
                self.warn("duplicate node id detected; regenerated");
                tracing::info!("node id {old:016x} -> {:016x}", self.config.node.id);
            }
            // Either way answer, so the other side learns about the collision too.
            let h = self.hello(true);
            self.send(from, &h);
            return;
        }
        if self.peers.upsert_hello(sender, from, &hello, now) {
            tracing::info!("peer {} ({sender:016x}) at {from}", hello.name);
        }
        if !hello.reply {
            let h = self.hello(true);
            self.send(from, &h);
        }
    }

    pub(crate) fn handle_inbound(&mut self, m: Inbound) {
        let Inbound { from, sender, arrival_ns, control } = m;
        if let Control::Hello(h) = control {
            self.on_hello(from, sender, h, arrival_ns);
            return;
        }
        if !self.peers.touch(sender, arrival_ns) {
            // Talk to us after a proper handshake.
            self.say_hello(from, arrival_ns);
            return;
        }
        match control {
            Control::Ping { t1 } => {
                let pong = Control::Pong { t1, t2: arrival_ns, t3: self.host.now_ns() };
                self.send(from, &pong);
            }
            Control::Pong { t1, t2, t3 } => {
                if let Some(p) = self.peers.get_mut(sender)
                    && p.clock.add(ClockSample { t1, t2, t3, t4: arrival_ns })
                {
                    tracing::info!("clock of {} stepped (slept?); re-synchronising", p.name);
                }
            }
            Control::State(state) => {
                if let Some(p) = self.peers.get_mut(sender) {
                    p.state = Some(state);
                }
            }
            other => self.handle_control(from, sender, other, arrival_ns),
        }
    }

    /// Control messages beyond the handshake/clock layer (streams, calibration, ...).
    fn handle_control(&mut self, from: SocketAddr, sender: NodeId, control: Control, arrival_ns: Nanos) {
        if self.handle_media_control(from, sender, &control, arrival_ns) {
            return;
        }
        match control {
            Control::Calib(msg) => self.handle_calib(sender, msg, arrival_ns),
            Control::SyncTest { active, epoch_ns } => self.on_sync_test(sender, active, epoch_ns),
            other => tracing::debug!("unhandled control message {other:?}"),
        }
    }

    pub(crate) fn handle_command(&mut self, c: Command) {
        tracing::info!("command {c:?}");
        if self.handle_media_command(&c) {
            return;
        }
        match c {
            Command::SyncTest(on) => self.sync_test_command(on),
            Command::StartCalibration { mic_node, mic_device, check } => {
                if mic_node == self.id() {
                    self.start_calibration(&mic_device, check);
                } else if let Some(p) = self.peers.get(mic_node) {
                    self.send(p.addr, &Control::Calib(soundsync_core::protocol::CalibMsg::Request { mic_device, check }));
                    self.calib.status = crate::status::CalibStatus::Running { step: 0, total: self.peers.len() + 1 };
                } else {
                    self.warn("calibration: the chosen microphone node is not connected");
                }
            }
            Command::ApplyCalibration => self.apply_calibration(),
            Command::CancelCalibration => self.cancel_calibration(),
            other => tracing::info!("command {other:?}: not handled"),
        }
    }

    fn poll_discovery(&mut self, now: Nanos) {
        let found: Vec<SocketAddr> = self.discovery.as_ref().map_or_else(Vec::new, |d| d.found().try_iter().collect());
        for addr in found {
            if !self.hello_targets.contains(&addr) {
                tracing::info!("discovered SoundSync node at {addr}");
                self.hello_targets.push(addr);
            }
            if self.peers.is_empty() {
                self.first_unanswered.get_or_insert(now);
            }
        }
        if !self.peers.is_empty() {
            self.first_unanswered = None;
        }
        let last = self.counters.last_authenticated_ns.load(Ordering::Relaxed);
        self.inbound_ok = !inbound_suspect(now, self.first_unanswered, (last != 0).then_some(last));
    }

    /// Warnings that reflect a current condition (recomputed for every status).
    fn condition_warnings(&self) -> Vec<String> {
        let mut w = self.layout_warnings();
        if self.silence_suspect {
            let device = self.capture_device_name().unwrap_or_default();
            w.push(format!(
                "Apps are playing to {device} but SoundSync hears silence — an app may be using exclusive mode or protected (DRM) audio"
            ));
        }
        if !self.inbound_ok {
            w.push(if cfg!(target_os = "macos") {
                "No replies from other SoundSync devices — allow SoundSync in System Settings ▸ Privacy & Security ▸ Local Network".to_string()
            } else {
                "No replies from other SoundSync devices — Windows Firewall may be blocking SoundSync (run install-windows.ps1 without -NoFirewall)"
                    .to_string()
            });
        }
        w
    }

    /// After sleep (or a frozen process) peer clocks and stream timing are stale and devices
    /// may have been reset: start clock sync over and reopen every stream.
    fn check_resume(&mut self, now: Nanos) {
        let host_gap = self.gap_host.tick(now);
        let wall_gap = self.gap_wall.tick(wall_ns());
        let Some(gap) = host_gap.max(wall_gap) else { return };
        tracing::info!("resumed after {} s", gap / 1_000_000_000);
        for p in self.peers.iter_mut() {
            p.clock.reset();
            p.first_seen = now;
            p.last_rx = now;
            p.next_ping = now;
        }
        self.last_hello.clear();
        if self.media.capture.is_some() {
            self.restart_capture(); // reopens the output too
        } else {
            self.open_output();
        }
    }

    pub(crate) fn tick(&mut self) {
        let now = self.host.now_ns();
        self.check_resume(now);
        self.poll_discovery(now);
        for target in self.hello_targets.clone() {
            if !self.peers.has_addr(target) {
                self.say_hello(target, now);
            }
        }
        let mut pings = Vec::new();
        for p in self.peers.iter_mut() {
            if now >= p.next_ping {
                p.next_ping = now + p.ping_interval(now);
                pings.push(p.addr);
            }
        }
        for addr in pings {
            self.send(addr, &Control::Ping { t1: self.host.now_ns() });
        }
        for gone in self.peers.expire(now) {
            tracing::info!("peer {} ({:016x}) timed out", gone.name, gone.id);
        }
        self.devices_tick(now);
        self.media_tick(now);
        self.calibration_tick(now);
        self.sync_test_tick(now);
        if now.saturating_sub(self.last_status) >= STATUS_EVERY_NS {
            self.last_status = now;
            self.publish_status(now);
        }
    }

    pub(crate) fn base_status(&self, now: Nanos) -> Status {
        Status {
            node_id: self.id(),
            name: self.config.node.name.clone(),
            group: self.config.network.group.clone(),
            os: Os::current(),
            master_mode: self.config.master.mode,
            peers: self
                .peers
                .iter()
                .map(|p| PeerStatus {
                    id: p.id,
                    name: p.name.clone(),
                    os: p.os,
                    addr: p.addr,
                    role: p.state.as_ref().map_or(Role::Receiver, |s| s.role),
                    rtt_ns: p.clock.rtt_ns(),
                    jitter_ns: p.clock.jitter_ns(),
                    clock_offset_ns: p.clock_offset_ns(now),
                    extra_latency_ns: p.state.as_ref().map_or(0, |s| s.extra_latency_ns),
                    output: p.state.as_ref().and_then(|s| s.output.clone()),
                    rx: p.state.as_ref().map_or_else(Vec::new, |s| s.rx.clone()),
                })
                .collect(),
            warnings: self.warnings.iter().cloned().chain(self.condition_warnings()).collect(),
            inbound_ok: self.inbound_ok,
            volume: self.config.playback.volume,
            muted: self.config.playback.muted,
            extra_latency_ns: self.config.extra_latency_ns(),
            ..Status::default()
        }
    }

    fn publish_status(&mut self, now: Nanos) {
        let mut s = self.base_status(now);
        self.fill_media_status(&mut s);
        s.calibration = self.calib.status.clone();
        s.sync_test = self.sync_test.is_some();
        s.streams_paused = self.streams_paused();
        self.status.store(Arc::new(s));
    }
}

pub(crate) fn run(mut node: Node, inbound: Receiver<Inbound>, commands: Receiver<Command>, stop: Arc<AtomicBool>) {
    node.watch_devices();
    node.recover_defaults();
    node.open_output();
    if node.config.master.enabled {
        node.start_broadcast();
    }
    let ticker = tick(Duration::from_millis(10));
    while !stop.load(Ordering::Acquire) {
        select! {
            recv(inbound) -> m => match m {
                Ok(m) => node.handle_inbound(m),
                Err(_) => break,
            },
            recv(commands) -> c => match c {
                Ok(c) => node.handle_command(c),
                Err(_) => break,
            },
            recv(ticker) -> _ => node.tick(),
        }
    }
    node.restore_defaults();
    let now = node.host.now_ns();
    node.publish_status(now);
}
