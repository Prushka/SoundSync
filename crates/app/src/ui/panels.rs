//! Window layout: header with warnings, then Overview / Sync / Settings tabs.

use eframe::egui::{self, Color32, RichText};
use soundsync_audio::DeviceInfo;
use soundsync_core::layout::describe;
use soundsync_core::{MasterMode, Nanos, NodeId, Os};
use soundsync_engine::{CalibStatus, Command, Status};

use super::app::{SoundSyncApp, Tab};
use super::format::{fmt_ms, fmt_opt_ms, role_label, sync_label, tray_status_line};

const WARN: Color32 = Color32::from_rgb(0xB2, 0x6A, 0x00);

pub fn draw(app: &mut SoundSyncApp, ui: &mut egui::Ui) {
    egui::Panel::top("header").show(ui, |ui| header(app, ui));
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match app.tab {
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
    if st.os == Os::MacOs && st.peers.is_empty() && app.started.elapsed() > std::time::Duration::from_secs(10) {
        warnings.push("No devices found — allow SoundSync in System Settings ▸ Privacy & Security ▸ Local Network".into());
    }
    warnings.extend(st.warnings.iter().cloned());
    if let Some(n) = app.notice.clone() {
        ui.horizontal(|ui| {
            ui.label(RichText::new(n).color(Color32::from_rgb(0x1E, 0x88, 0xE5)));
            if ui.small_button("✕").clicked() {
                app.notice = None;
            }
        });
    }
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

/// A device picker; returns the new configured value when the user picks one.
fn device_combo(ui: &mut egui::Ui, id: &str, current: &str, devices: &[DeviceInfo], extra: &[(&str, &str)]) -> Option<String> {
    let shown = if current.trim().is_empty() { "Default".to_string() } else { current.to_string() };
    let mut picked = None;
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
    egui::Grid::new("this_pc").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Capture");
        let current = if config.master.capture_device.is_empty() && mode == MasterMode::Virtual { "CABLE Input".to_string() } else { config.master.capture_device.clone() };
        if let Some(d) = device_combo(ui, "capture", &current, &app.render_devices, &[]) {
            app.command(Command::SetCaptureDevice(d));
        }
        ui.end_row();
        if mode == MasterMode::Virtual {
            ui.label("Monitor output");
            if let Some(d) = device_combo(ui, "monitor", &config.master.monitor_device, &app.render_devices, &[("", "Default"), ("null", "None (don't play here)")]) {
                app.command(Command::SetMonitorDevice(d));
            }
            ui.end_row();
        }
    });
    super::vbcable::panel(app, ui, &config, mode);
    if let Some(c) = &st.capture {
        ui.label(RichText::new(format!("Capturing {}: {} {} Hz · delay {}", c.name, describe(c.format), c.format.sample_rate, fmt_opt_ms(st.delay_ns))).weak());
    }
}

fn playback(app: &mut SoundSyncApp, ui: &mut egui::Ui, st: &Status) {
    ui.heading("Playback");
    let config = app.config.clone();
    egui::Grid::new("playback").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Output");
        if let Some(d) = device_combo(ui, "output", &config.playback.output_device, &app.render_devices, &[("", "Default")]) {
            app.command(Command::SetOutputDevice(d));
        }
        ui.end_row();
        ui.label("Volume");
        let mut percent = (st.volume * 100.0).round();
        let what = match &st.volume_device {
            Some(device) => format!("The volume of {device} itself: the same slider as in the system's sound settings"),
            None => "SoundSync's own level (this output has no volume control of its own)".to_string(),
        };
        if ui.add(egui::Slider::new(&mut percent, 0.0..=100.0).step_by(1.0).suffix("%")).on_hover_text(what).changed() {
            app.command(Command::SetVolume(percent / 100.0));
        }
        ui.end_row();
    });
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

fn group_table(app: &mut SoundSyncApp, ui: &mut egui::Ui, st: &Status) {
    ui.heading("Group");
    egui::Grid::new("group").striped(true).num_columns(7).spacing([14.0, 6.0]).show(ui, |ui| {
        for h in ["Name", "OS", "Role", "RTT", "Output latency", "Extra latency", "Sync"] {
            ui.label(RichText::new(h).strong());
        }
        ui.end_row();
        ui.label(format!("{} (this PC)", st.name));
        ui.label(format!("{:?}", st.os));
        ui.label(role_label(if st.broadcasting { soundsync_core::Role::Master(st.master_mode) } else { soundsync_core::Role::Receiver }));
        ui.label("—");
        ui.label(st.output.as_ref().map_or("—".into(), |o| fmt_ms(o.latency_ns)));
        extra_editor(app, ui, st.node_id, st.extra_latency_ns);
        ui.label(st.streams.iter().find(|s| s.master != st.node_id).map_or("—".into(), sync_label));
        ui.end_row();
        for p in &st.peers {
            ui.label(&p.name);
            ui.label(format!("{:?}", p.os));
            ui.label(role_label(p.role));
            ui.label(fmt_opt_ms(p.rtt_ns));
            ui.label(p.output.as_ref().map_or("—".into(), |o| format!("{} ({})", fmt_ms(o.device_latency_ns), o.device)));
            extra_editor(app, ui, p.id, p.extra_latency_ns);
            let rx = p.rx.iter().find(|r| r.master != p.id);
            ui.label(rx.map_or("—".into(), |r| if r.behind_ns > 0 { format!("Behind {:.0} ms", r.behind_ns as f64 / 1e6) } else { format!("±{:.1} ms", r.sync_error_ns.abs() as f64 / 1e6) }));
            ui.end_row();
        }
    });
    if st.peers.is_empty() {
        ui.label(RichText::new("No other SoundSync devices found yet. They must use the same group name (and key).").weak());
    }
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
    ui.label("Place a microphone where you listen. The speakers take turns playing a short sweep, three times each; SoundSync measures when each one arrives and corrects the extra latencies. Check measures what is left afterwards. Avoid Bluetooth headset microphones.");
    let mic_node = app.drafts.mic_node.unwrap_or(st.node_id);
    egui::Grid::new("calib").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Microphone on");
        let name = if mic_node == st.node_id { format!("{} (this PC)", st.name) } else { st.peers.iter().find(|p| p.id == mic_node).map_or("?".into(), |p| p.name.clone()) };
        egui::ComboBox::from_id_salt("mic_node").selected_text(name).show_ui(ui, |ui| {
            if ui.selectable_label(mic_node == st.node_id, format!("{} (this PC)", st.name)).clicked() {
                app.drafts.mic_node = Some(st.node_id);
            }
            for p in &st.peers {
                if ui.selectable_label(mic_node == p.id, &p.name).clicked() {
                    app.drafts.mic_node = Some(p.id);
                }
            }
        });
        ui.end_row();
        ui.label("Microphone");
        if mic_node == st.node_id {
            let current = app.drafts.mic_device.clone();
            if let Some(d) = device_combo(ui, "mic", &current, &app.capture_devices, &[("", "Default")]) {
                app.drafts.mic_device = d;
            }
        } else {
            ui.add(egui::TextEdit::singleline(&mut app.drafts.mic_device).hint_text("default microphone on that PC"));
        }
        ui.end_row();
    });
    ui.horizontal(|ui| {
        let running = matches!(st.calibration, CalibStatus::Running { .. });
        let mic_device = app.drafts.mic_device.clone();
        if ui
            .add_enabled(!running, egui::Button::new("Start"))
            .on_hover_text("Three sweeps per speaker; corrections are shown before you apply them")
            .clicked()
        {
            app.command(Command::StartCalibration { mic_node, mic_device: mic_device.clone(), check: false });
        }
        if ui
            .add_enabled(!running, egui::Button::new("Check"))
            .on_hover_text("One quick sweep per speaker: measures how far each one is still off")
            .clicked()
        {
            app.command(Command::StartCalibration { mic_node, mic_device, check: true });
        }
        if ui.add_enabled(running, egui::Button::new("Cancel")).clicked() {
            app.command(Command::CancelCalibration);
        }
        if let CalibStatus::Done { check, .. } = &st.calibration {
            let label = if *check { "Correct these" } else { "Apply" };
            if ui.button(label).clicked() {
                app.command(Command::ApplyCalibration);
                app.notice = Some("Corrections applied. Run Check to measure what is left.".into());
            }
        }
    });
    match &st.calibration {
        CalibStatus::Idle => {}
        CalibStatus::Running { step, total } => {
            ui.add(egui::ProgressBar::new(*step as f32 / (*total).max(1) as f32).text(format!("sweep {} of {total}…", (*step + 1).min(*total))));
        }
        CalibStatus::Failed(msg) => {
            ui.label(RichText::new(format!("Calibration failed: {msg}")).color(WARN));
        }
        CalibStatus::Done { results, check, .. } => {
            ui.label(if *check {
                "Left over (one sweep each; within 1 ms is in sync):"
            } else {
                "Measured corrections (Apply adds them to the extra latencies):"
            });
            egui::Grid::new("calib_result").striped(true).num_columns(4).spacing([16.0, 4.0]).show(ui, |ui| {
                ui.label(RichText::new("Speaker").strong());
                ui.label(RichText::new(if *check { "Off by" } else { "Correction" }).strong());
                ui.label(RichText::new("Signal").strong());
                ui.label(RichText::new("").strong());
                ui.end_row();
                for r in results {
                    let name = if r.node == st.node_id { st.name.clone() } else { st.peers.iter().find(|p| p.id == r.node).map_or("?".into(), |p| p.name.clone()) };
                    ui.label(name);
                    ui.label(format!("{:+.1} ms", r.delta_ns as f64 / 1e6));
                    ui.label(super::format::fmt_snr(r.snr_db));
                    let (note, ok) = super::format::calib_note(r, *check);
                    ui.label(if ok { RichText::new(note) } else { RichText::new(format!("⚠ {note}")).color(WARN) });
                    ui.end_row();
                }
            });
        }
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
    ui.horizontal(|ui| {
        if ui.button("Open config folder").clicked()
            && let Some(dir) = app.config_path.parent()
        {
            open_folder(dir);
        }
        if ui.button("Open logs").clicked() {
            open_folder(&app.paths.log_dir);
        }
        ui.label(RichText::new(format!("SoundSync {}", env!("CARGO_PKG_VERSION"))).weak());
    });
}
