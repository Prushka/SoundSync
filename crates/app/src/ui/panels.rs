//! Window layout: header with warnings, then Overview / Sync / Settings tabs.

use eframe::egui::{self, Color32, RichText};
use soundsync_audio::DeviceInfo;
use soundsync_core::calib::CalibKind;
use soundsync_core::layout::describe;
use soundsync_core::protocol::VolumeChange;
use soundsync_core::{MasterMode, Nanos, NodeId, Os};
use soundsync_engine::{CalibStatus, Command, Status, UpdatePeerStatus, VolumeStatus};
use soundsync_core::remote_update::{UpdateAction, UpdateState as GroupUpdateState};

use super::app::{SoundSyncApp, Tab};
use super::format::{fmt_level, fmt_ms, fmt_opt_ms, role_label, sync_label, tray_status_line, update_line, update_status};
use crate::update::UpdateState;

const WARN: Color32 = Color32::from_rgb(0xB2, 0x6A, 0x00);
const INFO: Color32 = Color32::from_rgb(0x1E, 0x88, 0xE5);

pub fn draw(app: &mut SoundSyncApp, ui: &mut egui::Ui) {
    egui::Panel::top("header").show(ui, |ui| header(app, ui));
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| match app.tab {
            Tab::Overview => overview(app, ui),
            Tab::Sync => sync(app, ui),
            Tab::Settings => settings(app, ui),
        });
    });
}

fn header(app: &mut SoundSyncApp, ui: &mut egui::Ui) {
    let st = app.status.clone();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.heading("SoundSync");
        ui.label(RichText::new(format!("{} · group \"{}\"", st.name, st.group)).weak());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(tray_status_line(&st));
        });
    });
    ui.horizontal(|ui| {
        ui.selectable_value(&mut app.tab, Tab::Overview, "Overview");
        ui.selectable_value(&mut app.tab, Tab::Sync, "Sync");
        ui.selectable_value(&mut app.tab, Tab::Settings, "Settings");
    });
    let mut warnings: Vec<String> = app.load_warnings.iter().map(|w| format!("config: {w}")).collect();
    if st.os == Os::MacOs && st.peers.is_empty() && !st.updates.iter().any(|p| p.online) && st.incompatible_peers.is_empty() && app.started.elapsed() > std::time::Duration::from_secs(10) {
        warnings.push("No devices found — allow SoundSync in System Settings ▸ Privacy & Security ▸ Local Network".into());
    }
    warnings.extend(st.warnings.iter().cloned());
    if let Some(n) = app.notice.clone() {
        ui.horizontal(|ui| {
            ui.label(RichText::new(n).color(INFO));
            if ui.small_button("✕").clicked() {
                app.notice = None;
            }
        });
    }
    update_header(app, ui);
    for w in warnings {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("⚠ {w}")).color(WARN));
            if w.contains(" → ") && ui.small_button("Configure speakers…").clicked() {
                open_speaker_setup();
            }
        });
    }
    ui.add_space(4.0);
}

/// The update line: what is available, the download's progress, or why it failed.
fn update_header(app: &mut SoundSyncApp, ui: &mut egui::Ui) {
    let state = app.updater.state();
    let Some(line) = update_line(&state) else { return };
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(line).color(INFO));
        match &state {
            UpdateState::Available(r) => {
                if ui.small_button("Update and restart").clicked() {
                    app.updater.install();
                }
                if ui.small_button("What's new").clicked() {
                    open_url(&r.page_url);
                }
            }
            UpdateState::Downloading { received, total, .. } => {
                ui.add(egui::ProgressBar::new(*received as f32 / (*total).max(1) as f32).desired_width(200.0));
            }
            UpdateState::Failed { release: Some(r), .. } => {
                if ui.small_button("Retry").clicked() {
                    app.updater.install();
                }
                if ui.small_button("Release page").clicked() {
                    open_url(&r.page_url);
                }
            }
            _ => {}
        }
    });
}

pub fn open_url(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
}

pub fn open_speaker_setup() {
    #[cfg(windows)]
    let _ = std::process::Command::new("control").args(["mmsys.cpl", "sounds"]).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").args(["-a", "Audio MIDI Setup"]).spawn();
}

pub fn open_folder(path: &std::path::Path) {
    let _ = std::fs::create_dir_all(path);
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
}

/// A device picker with a refresh button; returns the new configured value when the user picks
/// one, and sets `refresh` when they ask for fresh device lists.
fn device_combo(ui: &mut egui::Ui, id: &str, current: &str, devices: &[DeviceInfo], extra: &[(&str, &str)], refresh: &mut bool) -> Option<String> {
    let shown = if current.trim().is_empty() { "Default".to_string() } else { current.to_string() };
    let mut picked = None;
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt(id).width(360.0).selected_text(shown).show_ui(ui, |ui| {
            for (value, label) in extra {
                if ui.selectable_label(current == *value, *label).clicked() {
                    picked = Some(value.to_string());
                }
            }
            for d in devices.iter().filter(|d| d.active) {
                let label = format!("{}  ({} {} kHz)", d.name, describe(d.mix_format), d.mix_format.sample_rate / 1000);
                if ui.selectable_label(current == d.name, label).clicked() {
                    picked = Some(d.name.clone());
                }
            }
        });
        *refresh |= ui.small_button("🔄").on_hover_text("Refresh the device lists (they also refresh by themselves when devices change)").clicked();
    });
    picked
}

fn overview(app: &mut SoundSyncApp, ui: &mut egui::Ui) {
    let st = app.status.clone();
    if st.os == Os::Windows {
        ui.group(|ui| this_pc(app, ui, &st));
        ui.add_space(8.0);
    } else {
        ui.label("This Mac plays audio from Windows masters.");
        ui.add_space(8.0);
    }
    ui.group(|ui| playback(app, ui, &st));
    ui.add_space(8.0);
    ui.group(|ui| group_table(app, ui, &st));
}

fn this_pc(app: &mut SoundSyncApp, ui: &mut egui::Ui, st: &Status) {
    ui.heading("This PC");
    let mut on = st.broadcasting;
    if ui.checkbox(&mut on, "Broadcast this PC's audio").changed() {
        app.command(Command::SetBroadcast(on));
    }
    let config = app.config.clone();
    let mut mode = config.master.mode;
    ui.horizontal(|ui| {
        ui.label("Mode:");
        let a = ui.radio_value(&mut mode, MasterMode::Virtual, "VB-CABLE (full sync)");
        let b = ui.radio_value(&mut mode, MasterMode::Direct, "Direct capture (lowest latency, partial sync)");
        if a.changed() || b.changed() {
            app.command(Command::SetMasterMode(mode));
        }
    });
    let mut refresh = false;
    egui::Grid::new("this_pc").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Capture");
        let current = if config.master.capture_device.is_empty() && mode == MasterMode::Virtual { "CABLE Input".to_string() } else { config.master.capture_device.clone() };
        if let Some(d) = device_combo(ui, "capture", &current, &app.render_devices, &[], &mut refresh) {
            app.command(Command::SetCaptureDevice(d));
        }
        ui.end_row();
        if mode == MasterMode::Virtual {
            ui.label("Monitor output");
            if let Some(d) = device_combo(ui, "monitor", &config.master.monitor_device, &app.render_devices, &[("", "Default"), ("null", "None (don't play here)")], &mut refresh) {
                app.command(Command::SetMonitorDevice(d));
            }
            ui.end_row();
        }
    });
    if refresh {
        app.refresh_devices();
    }
    super::vbcable::panel(app, ui, &config, mode);
    if let Some(c) = &st.capture {
        ui.label(RichText::new(format!("Capturing {}: {} {} Hz · delay {}", c.name, describe(c.format), c.format.sample_rate, fmt_opt_ms(st.delay_ns))).weak());
    }
    for note in &st.notes {
        ui.label(RichText::new(format!("ℹ {note}")).weak());
    }
}

fn playback(app: &mut SoundSyncApp, ui: &mut egui::Ui, st: &Status) {
    ui.heading("Playback");
    let config = app.config.clone();
    let mut refresh = false;
    egui::Grid::new("playback").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Output");
        if let Some(d) = device_combo(ui, "output", &config.playback.output_device, &app.render_devices, &[("", "Default")], &mut refresh) {
            app.command(Command::SetOutputDevice(d));
        }
        ui.end_row();
        ui.label("Volume");
        let mut percent = (st.volume * 100.0).round();
        let what = match &st.volume_device {
            Some(device) => format!("The volume of {device} itself: the same slider as in the system's sound settings"),
            None => "SoundSync's own level (this output has no volume control of its own)".to_string(),
        };
        if ui.add(egui::Slider::new(&mut percent, 0.0..=100.0).step_by(1.0).fixed_decimals(0).suffix("%")).on_hover_text(what).changed() {
            app.command(Command::SetVolume(percent / 100.0));
        }
        ui.end_row();
    });
    if refresh {
        app.refresh_devices();
    }
    ui.horizontal(|ui| {
        let mut muted = st.muted;
        let mute_hint = st.volume_device.as_ref().map_or_else(|| "Mutes SoundSync on this PC".to_string(), |d| format!("Mutes {d}"));
        if ui.checkbox(&mut muted, "Mute").on_hover_text(mute_hint).changed() {
            app.command(Command::SetMuted(muted));
        }
        let mut follow = config.playback.follow_master_volume;
        if ui
            .checkbox(&mut follow, "Follow master volume")
            .on_hover_text("Streams also get quieter when the volume is turned down on the PC that broadcasts them")
            .changed()
        {
            app.command(Command::SetFollowMasterVolume(follow));
        }
        let mut remote = config.playback.play_remote;
        if ui.checkbox(&mut remote, "Play other masters").changed() {
            app.command(Command::SetPlayRemote(remote));
        }
    });
    if let Some(o) = &st.output {
        ui.label(RichText::new(format!("{}: {} {} Hz, reported latency {}", o.name, describe(o.format), o.format.sample_rate, fmt_ms(o.latency_ns))).weak());
    }
    for s in &st.streams {
        let from = if s.master == st.node_id { "This PC".to_string() } else { st.peers.iter().find(|p| p.id == s.master).map_or("?".into(), |p| p.name.clone()) };
        let mut line = format!("{from}: {} · {}", describe(s.format), sync_label(s));
        if s.lost_frames > 0 {
            line += &format!(" · {} frames lost", s.lost_frames);
        }
        ui.label(line);
    }
}

fn extra_editor(app: &mut SoundSyncApp, ui: &mut egui::Ui, node: NodeId, current_ns: Nanos) {
    let value = app.drafts.extra_ms.entry(node).or_insert(current_ns as f64 / 1e6);
    let resp = ui.add(egui::DragValue::new(value).range(-1000.0..=2000.0).speed(0.5).suffix(" ms"));
    if resp.drag_stopped() || resp.lost_focus() {
        let ns = (*value * 1e6).round() as Nanos;
        app.command(Command::SetExtraLatency { node, ns });
    }
    if !resp.dragged() && !resp.has_focus() {
        app.drafts.extra_ms.insert(node, current_ns as f64 / 1e6);
    }
}

/// A node's Volume slider and mute, editable from here (any node's, like its extra latency).
fn volume_editor(app: &mut SoundSyncApp, ui: &mut egui::Ui, node: NodeId, name: &str, reported: Option<VolumeStatus>) {
    let Some(v) = reported else {
        ui.label("—");
        return;
    };
    // While the slider moves, and briefly after, show the value being set rather than the last
    // reported one (which catches up within a moment).
    let held = app.drafts.volume.get(&node).filter(|(_, at)| at.elapsed() < std::time::Duration::from_millis(1500)).map(|(p, _)| *p);
    let mut percent = held.unwrap_or((v.volume * 100.0).round());
    let what = if v.device { format!("{name}'s output volume (the system's slider)") } else { format!("SoundSync's level on {name} (its output has no volume control)") };
    ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = 90.0;
        let resp = ui.add(egui::Slider::new(&mut percent, 0.0..=100.0).step_by(1.0).fixed_decimals(0).suffix("%")).on_hover_text(what);
        if resp.changed() || resp.dragged() {
            app.drafts.volume.insert(node, (percent, std::time::Instant::now()));
        }
        if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
            app.command(Command::ChangeVolume { node, change: VolumeChange::Set(percent / 100.0) });
        }
        let mut muted = v.muted;
        if ui.toggle_value(&mut muted, "🔇").on_hover_text(format!("Mute {name}")).changed() {
            app.command(Command::ChangeVolume { node, change: VolumeChange::Muted(muted) });
        }
    });
}

fn group_table(app: &mut SoundSyncApp, ui: &mut egui::Ui, st: &Status) {
    ui.horizontal_wrapped(|ui| {
        ui.heading("Group");
        let can_check = st.updates.iter().any(|p| p.online && !p.pending && p.requested.is_none() && !p.state.busy() && !matches!(p.state, GroupUpdateState::Unavailable(_)));
        if ui.add_enabled(can_check, egui::Button::new("Check devices for updates")).clicked() {
            app.check_group_updates();
        }
        let count = st.updates.iter().filter(|p| p.installable().is_some()).count();
        if ui.add_enabled(count > 0, egui::Button::new(format!("Update all outdated devices ({count})"))).clicked() {
            app.update_outdated_devices();
        }
    });
    ui.label(RichText::new("Updates install the latest release and restart SoundSync on the other devices. Their audio will pause.").weak());
    egui::Grid::new("group").striped(true).num_columns(10).spacing([14.0, 6.0]).show(ui, |ui| {
        for h in ["Name", "OS", "Build", "Role", "RTT", "Output latency", "Volume", "Extra latency", "Sync", "Update"] {
            ui.label(RichText::new(h).strong());
        }
        ui.end_row();
        ui.label(format!("{} (this PC)", st.name));
        ui.label(format!("{:?}", st.os));
        ui.label(soundsync_core::version::build_number().unwrap_or("dev"));
        ui.label(role_label(if st.broadcasting { soundsync_core::Role::Master(st.master_mode) } else { soundsync_core::Role::Receiver }));
        ui.label("—");
        ui.label(st.output.as_ref().map_or("—".into(), |o| fmt_ms(o.latency_ns)));
        let own = VolumeStatus { volume: st.volume, muted: st.muted, device: st.volume_device.is_some() };
        volume_editor(app, ui, st.node_id, "this PC", Some(own));
        extra_editor(app, ui, st.node_id, st.extra_latency_ns);
        ui.label(st.streams.iter().find(|s| s.master != st.node_id).map_or("—".into(), sync_label));
        if app.updater.state().installable().is_some() {
            if ui.button("Update and restart").clicked() {
                app.updater.install();
            }
        } else {
            ui.vertical(|ui| {
                ui.set_max_width(240.0);
                ui.label(super::format::group_update_state(&app.updater.state().for_group()));
            });
        }
        ui.end_row();
        for p in &st.peers {
            let update = st.updates.iter().find(|u| u.id == p.id);
            ui.label(&p.name).on_hover_text(format!("SoundSync {}", p.app_version));
            ui.label(format!("{:?}", p.os));
            ui.label(update.and_then(|u| u.build).map_or_else(|| super::format::build_label(&p.app_version), |b| b.to_string()));
            ui.label(role_label(p.role));
            ui.label(fmt_opt_ms(p.rtt_ns));
            match &p.output {
                Some(o) => ui.label(fmt_ms(o.device_latency_ns)).on_hover_text(&o.device),
                None => ui.label("—"),
            };
            volume_editor(app, ui, p.id, &p.name, p.volume);
            extra_editor(app, ui, p.id, p.extra_latency_ns);
            let rx = p.rx.iter().find(|r| r.master != p.id);
            ui.label(rx.map_or("—".into(), |r| if r.behind_ns > 0 { format!("Behind {:.0} ms", r.behind_ns as f64 / 1e6) } else { format!("±{:.1} ms", r.sync_error_ns.abs() as f64 / 1e6) }));
            group_update_cell(app, ui, update);
            ui.end_row();
        }
        // Update discovery still works when the node cannot join this version's audio group.
        for p in st.updates.iter().filter(|u| !st.peers.iter().any(|p| p.id == u.id)) {
            ui.label(&p.name).on_hover_text(format!("{} · SoundSync {}", p.addr, p.app_version));
            ui.label(format!("{:?}", p.os));
            ui.label(p.build.map_or_else(|| "dev".into(), |b| b.to_string()));
            ui.label(if !p.online { "Offline" } else if p.protocol != soundsync_core::protocol::PROTOCOL_VERSION { "Update required" } else { "Connecting…" });
            for _ in 0..5 { ui.label("—"); }
            group_update_cell(app, ui, Some(p));
            ui.end_row();
        }
        for p in st.incompatible_peers.iter().filter(|o| !st.peers.iter().any(|p| p.addr == o.from) && !st.updates.iter().any(|p| p.addr == o.from)) {
            ui.label(p.from.ip().to_string());
            ui.label("—");
            ui.label(if p.version < soundsync_core::protocol::PROTOCOL_VERSION { "Older" } else { "Newer" });
            ui.label("Update required");
            for _ in 0..5 { ui.label("—"); }
            group_update_cell(app, ui, None);
            ui.end_row();
        }
    });
    if st.peers.is_empty() && st.updates.is_empty() && st.incompatible_peers.is_empty() {
        ui.label(RichText::new("No other SoundSync devices found yet. They must use the same group name (and key).").weak());
    }
}

fn group_update_cell(app: &SoundSyncApp, ui: &mut egui::Ui, peer: Option<&UpdatePeerStatus>) {
    ui.vertical(|ui| {
        ui.set_max_width(240.0);
        let Some(p) = peer else {
            ui.label(if app.started.elapsed() < std::time::Duration::from_secs(8) { "Checking update support…" } else { "Update on this device once to enable remote updates." });
            return;
        };
        if let Some(notice) = &p.notice {
            ui.label(notice);
        }
        if let Some(build) = p.installable() {
            if ui.button(format!("Update to build {build}")).on_hover_text("Download the latest release and restart SoundSync on this device").clicked() {
                app.update_device(p.id, build);
            }
            if let GroupUpdateState::Failed { error, .. } = &p.state {
                ui.label(RichText::new(error).color(WARN));
            }
        } else if p.pending {
            ui.label("Sending request…");
        } else if !p.online {
            ui.label(if matches!(p.requested, Some(UpdateAction::Install { .. })) { "Waiting for the device to restart…" } else { "Offline" });
        } else if p.requested.is_some() && !p.state.busy() && !matches!(p.state, GroupUpdateState::Failed { .. }) {
            ui.label("Waiting for the device…");
        } else {
            ui.label(super::format::group_update_state(&p.state));
        }
    });
}

fn sync(app: &mut SoundSyncApp, ui: &mut egui::Ui) {
    let st = app.status.clone();
    ui.group(|ui| {
        ui.heading("Group delay");
        ui.label("Time from capture to sound on every speaker. Auto picks the smallest delay every speaker can meet.");
        ui.horizontal(|ui| {
            let mut changed = ui.checkbox(&mut app.drafts.delay_auto, "Auto").changed();
            if !app.drafts.delay_auto {
                changed |= ui.add(egui::DragValue::new(&mut app.drafts.delay_ms).range(20.0..=2000.0).suffix(" ms")).lost_focus();
            }
            if changed {
                let setting = if app.drafts.delay_auto { soundsync_core::delay::DelaySetting::Auto } else { soundsync_core::delay::DelaySetting::Manual(app.drafts.delay_ms) };
                app.command(Command::SetDelay(setting));
            }
            ui.label(format!("current: {}", fmt_opt_ms(st.delay_ns)));
        });
    });
    ui.add_space(8.0);
    ui.group(|ui| {
        ui.heading("Sync test");
        ui.label("Every speaker clicks once per second at the same moment (each at its own pitch). Adjust the extra-latency values until the clicks sound like one.");
        let mut on = st.sync_test;
        if ui.checkbox(&mut on, "Play sync clicks").changed() {
            app.command(Command::SyncTest(on));
        }
    });
    ui.add_space(8.0);
    ui.group(|ui| calibration(app, ui, &st));
}

fn calibration(app: &mut SoundSyncApp, ui: &mut egui::Ui, st: &Status) {
    ui.heading("Auto-calibrate with a microphone");
    ui.label("Place a microphone where you listen, then calibrate latency or volume separately. Each calibration plays three short sweeps per speaker; Check plays one. Avoid Bluetooth headset microphones.");
    let mic_node = app.drafts.mic_node.unwrap_or(st.node_id);
    egui::Grid::new("calib").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Microphone on");
        let name = if mic_node == st.node_id { format!("{} (this PC)", st.name) } else { st.peers.iter().find(|p| p.id == mic_node).map_or("?".into(), |p| p.name.clone()) };
        egui::ComboBox::from_id_salt("mic_node").selected_text(name).show_ui(ui, |ui| {
            let mut choose = |node: NodeId| {
                if node != mic_node {
                    app.drafts.mic_device = String::new(); // each node has its own microphones
                }
                app.drafts.mic_node = Some(node);
            };
            if ui.selectable_label(mic_node == st.node_id, format!("{} (this PC)", st.name)).clicked() {
                choose(st.node_id);
            }
            for p in &st.peers {
                if ui.selectable_label(mic_node == p.id, &p.name).clicked() {
                    choose(p.id);
                }
            }
        });
        ui.end_row();
        ui.label("Microphone");
        if mic_node == st.node_id {
            let current = app.drafts.mic_device.clone();
            let mut refresh = false;
            if let Some(d) = device_combo(ui, "mic", &current, &app.capture_devices, &[("", "Default")], &mut refresh) {
                app.drafts.mic_device = d;
            }
            if refresh {
                app.refresh_devices();
            }
        } else {
            let peer = st.peers.iter().find(|p| p.id == mic_node);
            ui.horizontal(|ui| {
                let list = peer.and_then(|p| p.inputs.as_ref());
                if let Some(list) = list {
                    let default = list.default.as_deref().map_or("Default".to_string(), |d| format!("Default ({d})"));
                    let current = app.drafts.mic_device.clone();
                    let shown = if current.is_empty() { default.clone() } else { current.clone() };
                    egui::ComboBox::from_id_salt("remote_mic").width(360.0).selected_text(shown).show_ui(ui, |ui| {
                        if ui.selectable_label(current.is_empty(), default).clicked() {
                            app.drafts.mic_device = String::new();
                        }
                        for d in &list.devices {
                            if ui.selectable_label(current == *d, d).clicked() {
                                app.drafts.mic_device = d.clone();
                            }
                        }
                    });
                    if list.devices.is_empty() {
                        ui.label(RichText::new("No microphones connected").weak());
                    }
                } else {
                    ui.label(RichText::new(if peer.is_some() { "Loading microphones…" } else { "Device disconnected" }).weak());
                }
                if ui.add_enabled(peer.is_some(), egui::Button::new("🔄")).on_hover_text("Refresh microphones").clicked() {
                    app.command(Command::ListInputs(mic_node));
                }
            });
        }
        ui.end_row();
    });
    // Read again after the selector, so a click always uses the newly selected device.
    let mic_node = app.drafts.mic_node.unwrap_or(st.node_id);
    for kind in [CalibKind::Latency, CalibKind::Volume] {
        ui.add_space(8.0);
        let title = match kind { CalibKind::Latency => "Latency", CalibKind::Volume => "Volume" };
        ui.push_id(title, |ui| {
            ui.group(|ui| calibration_kind(app, ui, st, mic_node, kind));
        });
    }
}

fn calibration_kind(app: &mut SoundSyncApp, ui: &mut egui::Ui, st: &Status, mic_node: NodeId, kind: CalibKind) {
    let (title, name, description) = match kind {
        CalibKind::Latency => ("Latency", "latency", "Align when the speakers are heard by correcting their extra latencies."),
        CalibKind::Volume => ("Volume", "volume", "Match loudness at your listening position by turning louder speakers down to the quietest one."),
    };
    ui.heading(title);
    ui.label(description);
    let running = matches!(st.calibration, CalibStatus::Running { .. });
    let ours_running = matches!(st.calibration, CalibStatus::Running { kind: k, .. } if k == kind);
    let mic_available = if mic_node == st.node_id {
        app.capture_devices.iter().any(|d| d.active)
    } else {
        st.peers.iter().find(|p| p.id == mic_node).and_then(|p| p.inputs.as_ref()).is_some_and(|list| !list.devices.is_empty())
    };
    ui.horizontal(|ui| {
        let mic_device = app.drafts.mic_device.clone();
        if ui
            .add_enabled(!running && mic_available, egui::Button::new(format!("Calibrate {name}")))
            .on_hover_text("Three sweeps per speaker; corrections are shown before you apply them")
            .clicked()
        {
            app.command(Command::StartCalibration { mic_node, mic_device: mic_device.clone(), kind, check: false });
        }
        if ui
            .add_enabled(!running && mic_available, egui::Button::new(format!("Check {name}")))
            .on_hover_text("One quick sweep per speaker: measures how far each one is still off")
            .clicked()
        {
            app.command(Command::StartCalibration { mic_node, mic_device, kind, check: true });
        }
        if ours_running && ui.button("Cancel").clicked() {
            app.command(Command::CancelCalibration);
        }
        if let CalibStatus::Done { session, kind: k, .. } = &st.calibration
            && *k == kind
        {
            let label = format!("Apply {name}");
            if ui.button(label).on_hover_text(format!("Apply this result, then run Check {name} to measure what is left")).clicked() {
                app.command(Command::ApplyCalibration { session: *session, kind });
            }
        }
    });
    match &st.calibration {
        CalibStatus::Running { kind: k, step, total } if *k == kind => {
            ui.add(egui::ProgressBar::new(*step as f32 / (*total).max(1) as f32).text(format!("sweep {} of {total}…", (*step + 1).min(*total))));
        }
        CalibStatus::Failed { kind: k, message } if *k == kind => {
            ui.label(RichText::new(format!("{title} calibration failed: {message}")).color(WARN));
        }
        CalibStatus::Done { results, check, kind: k, .. } if *k == kind => {
            ui.label(match (kind, check) {
                (CalibKind::Latency, true) => "Remaining timing differences (within 1 ms is in sync):",
                (CalibKind::Latency, false) => "Measured corrections to add to each speaker's extra latency:",
                (CalibKind::Volume, true) => "Remaining loudness differences (within 1 dB is matched):",
                (CalibKind::Volume, false) => "Measured levels above the quietest speaker (Apply volume turns these down):",
            });
            egui::Grid::new("calib_result").striped(true).num_columns(4).spacing([16.0, 4.0]).show(ui, |ui| {
                ui.label(RichText::new("Speaker").strong());
                ui.label(RichText::new(match kind {
                    CalibKind::Latency => if *check { "Off by" } else { "Correction" },
                    CalibKind::Volume => "Level above quietest",
                }).strong());
                ui.label(RichText::new("Signal").strong());
                ui.label(RichText::new("").strong());
                ui.end_row();
                for r in results {
                    let name = if r.node == st.node_id { st.name.clone() } else { st.peers.iter().find(|p| p.id == r.node).map_or("?".into(), |p| p.name.clone()) };
                    ui.label(name);
                    ui.label(match kind {
                        CalibKind::Latency => format!("{:+.1} ms", r.delta_ns as f64 / 1e6),
                        CalibKind::Volume => fmt_level(r.level_db),
                    });
                    ui.label(super::format::fmt_snr(r.snr_db));
                    let (note, ok) = super::format::calib_note(r, kind, *check);
                    ui.label(if ok { RichText::new(note) } else { RichText::new(format!("⚠ {note}")).color(WARN) });
                    ui.end_row();
                }
            });
        }
        _ => {}
    }
}

fn settings(app: &mut SoundSyncApp, ui: &mut egui::Ui) {
    ui.group(|ui| {
        ui.heading("Startup");
        match app.autostart.clone() {
            Ok(enabled) => {
                let mut on = enabled;
                let label = if cfg!(windows) { "Start with Windows" } else { "Start at login" };
                if ui.checkbox(&mut on, label).changed() {
                    app.autostart = crate::autostart::set_enabled(on).and_then(|_| crate::autostart::is_enabled()).map_err(|e| e.to_string());
                }
            }
            Err(e) => {
                ui.label(RichText::new(format!("Autostart unavailable: {e}")).color(WARN));
            }
        }
    });
    ui.add_space(8.0);
    ui.group(|ui| {
        ui.heading("Network");
        egui::Grid::new("net").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("This PC's name");
            ui.text_edit_singleline(&mut app.drafts.name);
            ui.end_row();
            ui.label("Group");
            ui.text_edit_singleline(&mut app.drafts.group);
            ui.end_row();
            ui.label("Group key (optional)");
            ui.add(egui::TextEdit::singleline(&mut app.drafts.key).password(true));
            ui.end_row();
            ui.label("UDP port");
            ui.text_edit_singleline(&mut app.drafts.port);
            ui.end_row();
            ui.label("Interface");
            ui.add(egui::TextEdit::singleline(&mut app.drafts.interface).hint_text("auto"));
            ui.end_row();
        });
        ui.label(RichText::new("Every PC in a group needs the same group name and key. Do not copy node.id between PCs.").weak());
        if ui.button("Save & restart network").clicked() {
            app.save_and_restart();
        }
    });
    ui.add_space(8.0);
    ui.group(|ui| updates(app, ui));
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Open config folder").clicked()
            && let Some(dir) = app.config_path.parent()
        {
            open_folder(dir);
        }
        if ui.button("Open logs").clicked() {
            open_folder(&app.paths.log_dir);
        }
    });
}

fn updates(app: &mut SoundSyncApp, ui: &mut egui::Ui) {
    ui.heading("Updates");
    let state = app.updater.state();
    ui.label(format!("SoundSync {}", soundsync_core::version::version()));
    let status = RichText::new(update_status(&state, std::time::SystemTime::now()));
    ui.label(if matches!(state, UpdateState::Failed { .. }) { status.color(WARN) } else { status.weak() });
    ui.horizontal(|ui| {
        let idle = !matches!(
            state,
            UpdateState::Unavailable(_) | UpdateState::Checking | UpdateState::Downloading { .. } | UpdateState::Ready { .. } | UpdateState::Restarting(_)
        );
        if ui.add_enabled(idle, egui::Button::new("Check for updates")).clicked() {
            app.updater.check_now();
        }
        if matches!(state, UpdateState::Available(_)) && ui.button("Update and restart").clicked() {
            app.updater.install();
        }
    });
}
