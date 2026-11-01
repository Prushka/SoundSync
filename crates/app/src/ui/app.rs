//! The eframe application: owns the engine, the tray and the UI state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui;
use soundsync_audio::{AudioHost, DeviceInfo, Direction};
use soundsync_core::config::{Config, Paths};
use soundsync_core::delay::DelaySetting;
use soundsync_core::{NodeId, Os};
use soundsync_engine::{Command, Engine, EngineOptions, Status};

use super::AppContext;
use super::tray::{Action, Tray};
use crate::single_instance::InstanceGuard;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Sync,
    Settings,
}

/// Editable copies of settings that only apply when saved.
pub struct Drafts {
    pub name: String,
    pub group: String,
    pub key: String,
    pub port: String,
    pub interface: String,
    pub delay_auto: bool,
    pub delay_ms: f64,
    pub mic_node: Option<NodeId>,
    pub mic_device: String,
    /// Apply also turns louder speakers down to the quietest one.
    pub match_volumes: bool,
    /// Per-node extra latency being edited (ms).
    pub extra_ms: HashMap<NodeId, f64>,
    /// Per-node Volume slider being moved (percent, when last touched): shown instead of the
    /// reported value until that catches up.
    pub volume: HashMap<NodeId, (f32, Instant)>,
}

impl Drafts {
    fn from_config(c: &Config) -> Drafts {
        let (delay_auto, delay_ms) = match c.sync.delay_ms {
            DelaySetting::Auto => (true, 80.0),
            DelaySetting::Manual(ms) => (false, ms),
        };
        Drafts {
            name: c.node.name.clone(),
            group: c.network.group.clone(),
            key: c.network.key.clone(),
            port: c.network.port.to_string(),
            interface: c.network.interface.clone(),
            delay_auto,
            delay_ms,
            mic_node: None,
            mic_device: String::new(),
            match_volumes: true,
            extra_ms: HashMap::new(),
            volume: HashMap::new(),
        }
    }
}

pub struct SoundSyncApp {
    pub engine: Option<Engine>,
    pub host: Arc<dyn AudioHost>,
    pub paths: Paths,
    pub config_path: PathBuf,
    /// The configuration on disk (re-read every second without side effects; the engine owns
    /// the file and writes it on commands).
    pub config: Config,
    /// Fingerprint of the last hand edit already applied or reported.
    handled_config: Option<u64>,
    /// An edit seen once; acted on when the next check still sees it (editors save in steps, and
    /// a read can land between the engine announcing a save and writing it).
    pending_edit: Option<u64>,
    bind_ip: Option<std::net::Ipv4Addr>,
    enable_mdns: bool,
    pub status: Arc<Status>,
    pub tab: Tab,
    pub drafts: Drafts,
    pub render_devices: Vec<DeviceInfo>,
    pub capture_devices: Vec<DeviceInfo>,
    pub autostart: Result<bool, String>,
    pub notice: Option<String>,
    pub load_warnings: Vec<String>,
    pub cable: super::vbcable::CableHelper,
    /// Saves/restores default speakers around the VB-CABLE install (Windows).
    pub defaults_guard: Option<soundsync_engine::defaults::DefaultSwitchGuard>,
    tray: Option<Tray>,
    actions: Receiver<Action>,
    hwnd: Arc<AtomicIsize>,
    quitting: Arc<AtomicBool>,
    visible: bool,
    pub started: Instant,
    last_status: Instant,
    last_config: Instant,
    last_devices: Instant,
    /// The engine's device-change count when the device lists were last refreshed.
    seen_device_events: u64,
    /// The remote microphone node last asked for its list, and when.
    inputs_asked: Option<(NodeId, Instant)>,
    _instance: InstanceGuard,
}

#[cfg(windows)]
fn window_handle(cc: &eframe::CreationContext<'_>) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}

#[cfg(not(windows))]
fn window_handle(_cc: &eframe::CreationContext<'_>) -> isize {
    0
}

/// Shows the window even if eframe is idle (belt and braces for the hidden-window case).
fn show_native(hwnd: isize) {
    #[cfg(windows)]
    if hwnd != 0 {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{SW_SHOW, SetForegroundWindow, ShowWindow};
        unsafe {
            let h = HWND(hwnd as *mut _);
            let _ = ShowWindow(h, SW_SHOW);
            let _ = SetForegroundWindow(h);
        }
    }
    #[cfg(not(windows))]
    let _ = hwnd;
}

impl SoundSyncApp {
    pub fn new(cc: &eframe::CreationContext<'_>, app: AppContext) -> SoundSyncApp {
        let ctx = cc.egui_ctx.clone();
        let (tx, actions): (Sender<Action>, Receiver<Action>) = unbounded();
        let can_broadcast = Os::current() == Os::Windows;
        let tray = match Tray::create(ctx.clone(), tx.clone(), can_broadcast) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!("tray icon unavailable: {e}");
                None
            }
        };
        let hwnd = Arc::new(AtomicIsize::new(window_handle(cc)));
        let (show_tx, show_ctx) = (tx.clone(), ctx.clone());
        app.instance.on_show(move || {
            let _ = show_tx.send(Action::Show);
            show_ctx.request_repaint();
        });
        let (quit_tx, quit_ctx) = (tx, ctx.clone());
        app.instance.on_quit(move || {
            let _ = quit_tx.send(Action::Quit);
            quit_ctx.request_repaint();
        });
        // Keeps `logic` (status, tray) running while the window is hidden.
        let tick_ctx = ctx;
        std::thread::Builder::new()
            .name("soundsync-ui-tick".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    tick_ctx.request_repaint();
                }
            })
            .ok();
        let status = app.engine.status();
        let defaults_guard = crate::default_devices_api().map(|api| soundsync_engine::defaults::DefaultSwitchGuard::new(api, app.paths.state_file.clone()));
        let mut s = SoundSyncApp {
            drafts: Drafts::from_config(&app.config),
            config: app.config.clone(),
            handled_config: None,
            pending_edit: None,
            bind_ip: app.bind_ip,
            enable_mdns: app.enable_mdns,
            engine: Some(app.engine),
            host: app.host,
            paths: app.paths,
            config_path: app.config_path,
            status,
            tab: Tab::Overview,
            render_devices: Vec::new(),
            capture_devices: Vec::new(),
            autostart: crate::autostart::is_enabled().map_err(|e| e.to_string()),
            notice: None,
            load_warnings: app.load_warnings,
            cable: super::vbcable::CableHelper::default(),
            defaults_guard,
            tray,
            actions,
            hwnd,
            quitting: Arc::new(AtomicBool::new(false)),
            visible: app.start_visible,
            started: Instant::now(),
            last_status: Instant::now(),
            last_config: Instant::now(),
            last_devices: Instant::now() - Duration::from_secs(60),
            seen_device_events: 0,
            inputs_asked: None,
            _instance: app.instance,
        };
        s.refresh_devices();
        let (waiting, notice) = super::vbcable::resume_setup(s.defaults_guard.as_ref(), &s.render_devices);
        s.cable.waiting = waiting;
        if notice.is_some() {
            s.notice = notice;
        }
        s
    }

    pub fn command(&self, c: Command) {
        if let Some(e) = &self.engine {
            e.command(c);
        }
    }

    pub fn refresh_devices(&mut self) {
        self.render_devices = self.host.devices(Direction::Render).unwrap_or_default();
        self.capture_devices = self.host.devices(Direction::Capture).unwrap_or_default();
        self.last_devices = Instant::now();
    }

    fn show(&mut self, ctx: &egui::Context) {
        self.visible = true;
        show_native(self.hwnd.load(Ordering::Relaxed));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.refresh_devices();
        if let Some(e) = &self.engine {
            self.status = e.status();
        }
    }

    fn handle(&mut self, a: Action, ctx: &egui::Context) {
        let st = self.status.clone();
        match a {
            Action::Show => self.show(ctx),
            Action::ToggleBroadcast => self.command(Command::SetBroadcast(!st.broadcasting)),
            Action::ToggleMute => self.command(Command::SetMuted(!st.muted)),
            Action::ToggleSyncTest => self.command(Command::SyncTest(!st.sync_test)),
            Action::Quit => {
                self.quitting.store(true, Ordering::Release);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// Keeps the microphone list of the chosen remote microphone node fresh while it is shown.
    fn ask_for_remote_inputs(&mut self) {
        let Some(node) = self.drafts.mic_node.filter(|n| *n != self.status.node_id) else { return };
        let due = self.inputs_asked.is_none_or(|(asked, at)| asked != node || at.elapsed() >= Duration::from_secs(5));
        if self.visible && self.tab == Tab::Sync && due {
            self.inputs_asked = Some((node, Instant::now()));
            self.command(Command::ListInputs(node));
        }
    }

    /// Saves network/name settings and restarts the engine with them.
    pub fn save_and_restart(&mut self) {
        let mut config = self.config.clone();
        config.node.name = self.drafts.name.trim().to_string();
        config.network.group = self.drafts.group.trim().to_string();
        config.network.key = self.drafts.key.clone();
        config.network.interface = self.drafts.interface.trim().to_string();
        match self.drafts.port.trim().parse::<u16>() {
            Ok(p) if p > 0 => config.network.port = p,
            _ => {
                self.notice = Some("port must be a number between 1 and 65535".into());
                return;
            }
        }
        drop(self.engine.take()); // stops the engine's own saves before ours
        if let Err(e) = config.save(&self.config_path) {
            self.notice = Some(format!("cannot save config: {e}"));
        }
        self.restart_engine(config, "Network settings saved; SoundSync restarted.");
    }

    /// (Re)starts the engine with `config`, keeping the command-line engine options.
    fn restart_engine(&mut self, config: Config, done: &str) {
        drop(self.engine.take()); // releases the port and audio devices first
        match Engine::start(EngineOptions {
            config: config.clone(),
            config_path: Some(self.config_path.clone()),
            state_path: Some(self.paths.state_file.clone()),
            host: self.host.clone(),
            enable_mdns: self.enable_mdns,
            bind_ip: self.bind_ip,
            impair: None,
            default_devices: crate::default_devices_api(),
        }) {
            Ok(e) => {
                self.status = e.status();
                self.engine = Some(e);
                self.config = config;
                self.drafts = Drafts::from_config(&self.config);
                self.notice = Some(done.into());
            }
            Err(e) => self.notice = Some(format!("cannot restart: {e:#}")),
        }
    }

    /// Picks up hand edits of config.toml (see `config_watch`).
    fn check_config_file(&mut self) {
        let Ok(bytes) = std::fs::read(&self.config_path) else { return };
        let engine_fp = self.engine.as_ref().map_or(0, |e| e.config_fingerprint());
        let change = super::config_watch::config_change(&bytes, engine_fp, self.handled_config);
        let seen = match &change {
            super::config_watch::ConfigChange::Unchanged => None,
            super::config_watch::ConfigChange::Edited { fingerprint, .. } | super::config_watch::ConfigChange::Invalid { fingerprint, .. } => Some(*fingerprint),
        };
        if seen.is_some() && std::mem::replace(&mut self.pending_edit, seen) != seen {
            return; // first sighting: confirm on the next check
        }
        self.pending_edit = None;
        match change {
            super::config_watch::ConfigChange::Unchanged => {
                if let Ok((config, _)) = Config::parse(&bytes) {
                    self.config = config;
                }
            }
            super::config_watch::ConfigChange::Edited { config, warnings, fingerprint } => {
                self.handled_config = Some(fingerprint);
                tracing::info!("config.toml was edited; restarting the engine with it");
                self.load_warnings = warnings;
                self.restart_engine(*config, "config.toml was edited; SoundSync applied it.");
            }
            super::config_watch::ConfigChange::Invalid { error, fingerprint } => {
                self.handled_config = Some(fingerprint);
                tracing::warn!("config.toml is not valid TOML: {error}");
                self.notice = Some(format!("config.toml was not applied: {error}. SoundSync keeps the current settings until the file is fixed."));
            }
        }
    }
}

impl eframe::App for SoundSyncApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(a) = self.actions.try_recv() {
            self.handle(a, ctx);
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting.load(Ordering::Acquire) {
            // Closing the window only hides it; Quit is in the tray menu.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.visible = false;
        }
        if self.last_status.elapsed() >= Duration::from_millis(200) {
            self.last_status = Instant::now();
            if let Some(e) = &self.engine {
                self.status = e.status();
            }
            if let Some(t) = &mut self.tray {
                t.update(&self.status);
            }
            if self.status.device_events != self.seen_device_events {
                self.seen_device_events = self.status.device_events;
                self.refresh_devices(); // a device came, went or changed: show it at once
            }
            self.ask_for_remote_inputs();
        }
        if self.last_config.elapsed() >= Duration::from_secs(1) {
            self.last_config = Instant::now();
            self.check_config_file();
        }
        if self.visible && self.last_devices.elapsed() >= Duration::from_secs(5) {
            self.refresh_devices();
        }
        if self.visible {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        super::panels::draw(self, ui);
    }

    fn on_exit(&mut self) {
        tracing::info!("quitting");
        drop(self.engine.take());
    }
}
