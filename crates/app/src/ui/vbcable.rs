//! VB-CABLE helper in the "This PC" panel: detection, guided install that protects the user's
//! default speakers, speaker-layout check, and the opt-in default-output switch.

use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};
use soundsync_audio::DeviceInfo;
use soundsync_core::MasterMode;
use soundsync_core::config::Config;
use soundsync_core::layout::describe;
use soundsync_engine::Command;
use soundsync_engine::defaults::DefaultSwitchGuard;

use super::app::SoundSyncApp;

const VB_URL: &str = "https://vb-audio.com/Cable/";

/// The connected VB-CABLE playback endpoint, if installed.
pub fn detect_cable(devices: &[DeviceInfo]) -> Option<&DeviceInfo> {
    devices.iter().find(|d| d.active && d.name.contains("CABLE Input"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupState {
    Installed { channels: u16 },
    NotInstalled,
    WaitingForInstall,
}

/// Helper state kept by the app.
pub struct CableHelper {
    pub waiting: bool,
    last_poll: Instant,
}

impl Default for CableHelper {
    fn default() -> Self {
        CableHelper { waiting: false, last_poll: Instant::now() }
    }
}

fn open_url(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
}

/// Puts back the default speakers saved before the VB-CABLE install; returns the notice.
fn finish_setup(guard: Option<&DefaultSwitchGuard>) -> String {
    match guard.map(|g| g.restore_after_setup()) {
        Some(Ok(true)) => "VB-CABLE installed; your default speakers were restored.".into(),
        Some(Err(e)) => format!("VB-CABLE installed, but restoring your default speakers failed: {e}"),
        _ => "VB-CABLE installed.".into(),
    }
}

/// At start-up, resumes a setup begun before SoundSync restarted (VB-Audio's installer asks for a
/// reboot): finished at once if the cable is there, waited for otherwise. Returns (waiting, notice).
pub fn resume_setup(guard: Option<&DefaultSwitchGuard>, devices: &[DeviceInfo]) -> (bool, Option<String>) {
    match guard.filter(|g| g.setup_pending()) {
        None => (false, None),
        Some(g) if detect_cable(devices).is_some() => (false, Some(finish_setup(Some(g)))),
        Some(_) => (true, None),
    }
}

fn state(app: &SoundSyncApp) -> SetupState {
    match detect_cable(&app.render_devices) {
        Some(c) => SetupState::Installed { channels: c.mix_format.channels },
        None if app.cable.waiting => SetupState::WaitingForInstall,
        None => SetupState::NotInstalled,
    }
}

pub fn panel(app: &mut SoundSyncApp, ui: &mut egui::Ui, config: &Config, mode: MasterMode) {
    if mode != MasterMode::Virtual {
        return;
    }
    match state(app) {
        SetupState::NotInstalled => {
            ui.label("VB-CABLE is not installed. SoundSync captures it to keep every speaker, including this PC's, in sync.");
            ui.horizontal(|ui| {
                if ui.button("Set up VB-CABLE…").clicked() {
                    let saved = app.defaults_guard.as_ref().map(|g| g.save_for_setup());
                    open_url(VB_URL);
                    app.cable.waiting = true;
                    app.notice = Some(match saved {
                        Some(Err(e)) => format!("Could not save your current default speakers ({e}); install VB-CABLE, then check Sound settings."),
                        _ => "VB-CABLE is a free product by VB-Audio Software; install it, then return here.".into(),
                    });
                }
                ui.label(RichText::new("or choose Direct capture above").weak());
            });
        }
        SetupState::WaitingForInstall => {
            ui.label("Waiting for VB-CABLE… SoundSync puts your default speakers back once it appears.");
            if app.cable.last_poll.elapsed() >= Duration::from_secs(2) {
                app.cable.last_poll = Instant::now();
                app.refresh_devices();
                if detect_cable(&app.render_devices).is_some() {
                    app.cable.waiting = false;
                    app.notice = Some(finish_setup(app.defaults_guard.as_ref()));
                }
            }
        }
        SetupState::Installed { channels } => {
            let cable = detect_cable(&app.render_devices).cloned();
            let monitor = if config.master.monitor_device.trim().is_empty() {
                app.render_devices.iter().find(|d| d.active && d.is_default).cloned()
            } else {
                app.render_devices.iter().find(|d| d.active && d.name == config.master.monitor_device).cloned()
            };
            if let (Some(c), Some(m)) = (&cable, &monitor)
                && channels < m.mix_format.channels
            {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!(
                        "⚠ CABLE Input is {}, your monitor is {} — set CABLE Input to {} in Configure speakers",
                        describe(c.mix_format),
                        describe(m.mix_format),
                        describe(m.mix_format)
                    ))
                    .color(egui::Color32::from_rgb(0xB2, 0x6A, 0x00)));
                    if ui.small_button("Configure speakers…").clicked() {
                        super::panels::open_speaker_setup();
                    }
                });
            }
            let mut switch = config.master.switch_default;
            if ui
                .checkbox(&mut switch, "Switch Windows default output to VB-CABLE while broadcasting")
                .on_hover_text("Apps play to VB-CABLE so SoundSync can capture them. Your previous default is restored when broadcasting stops, on exit, and after a crash.")
                .changed()
            {
                app.command(Command::SetSwitchDefault(switch));
            }
            if !switch {
                ui.label(
                    RichText::new("Set \"CABLE Input\" as the output of the apps to share (Sound settings ▸ Volume mixer), or make it the default output and pick your speakers as Monitor output.")
                        .weak(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundsync_audio::{DeviceInfo, Direction, StreamFormat};
    use soundsync_engine::defaults::{DefaultDeviceApi, DefaultDevices, DefaultRole, DefaultSwitchGuard};

    fn dev(name: &str, active: bool) -> DeviceInfo {
        DeviceInfo {
            id: format!("id-{name}"),
            name: name.into(),
            direction: Direction::Render,
            is_default: false,
            active,
            mix_format: StreamFormat { sample_rate: 48_000, channels: 8, channel_mask: 0x63F },
            default_period_ns: 10_000_000,
            min_period_ns: 3_000_000,
        }
    }

    #[test]
    fn detect_cable_matches_vb_names() {
        let devices = vec![dev("Speakers (Realtek)", true), dev("CABLE Input (VB-Audio Virtual Cable)", true)];
        assert_eq!(detect_cable(&devices).map(|d| d.id.as_str()), Some("id-CABLE Input (VB-Audio Virtual Cable)"));
        assert!(detect_cable(&[dev("CABLE Input (VB-Audio Virtual Cable)", false)]).is_none(), "a disconnected cable does not count");
        assert!(detect_cable(&[dev("Speakers", true)]).is_none());
    }

    struct Api(std::sync::Mutex<DefaultDevices>);

    impl DefaultDeviceApi for Api {
        fn read(&self) -> Result<DefaultDevices, String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn set(&self, id: &str, role: DefaultRole) -> Result<(), String> {
            let mut d = self.0.lock().unwrap();
            match role {
                DefaultRole::Console => d.console = id.into(),
                DefaultRole::Multimedia => d.multimedia = id.into(),
                DefaultRole::Communications => d.communications = id.into(),
            }
            Ok(())
        }
        fn exists(&self, _id: &str) -> bool {
            true
        }
    }

    #[test]
    fn setup_started_before_a_restart_is_resumed() {
        // VB-Audio's installer asks for a reboot: the snapshot of the user's speakers must survive it.
        let tv = DefaultDevices { console: "{tv}".into(), multimedia: "{tv}".into(), communications: "{tv}".into() };
        let api = std::sync::Arc::new(Api(std::sync::Mutex::new(tv.clone())));
        let dir = std::env::temp_dir().join(format!("soundsync-setup-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("state.toml");
        DefaultSwitchGuard::new(api.clone(), state.clone()).save_for_setup().unwrap();
        api.set("{cable}", DefaultRole::Console).unwrap(); // the installer made itself the default
        let g = DefaultSwitchGuard::new(api.clone(), state.clone()); // SoundSync restarted
        assert_eq!(resume_setup(Some(&g), &[dev("Speakers", true)]), (true, None), "still waiting for the cable");
        let (waiting, notice) = resume_setup(Some(&g), &[dev("Speakers", true), dev("CABLE Input (VB-Audio Virtual Cable)", true)]);
        assert!(!waiting);
        assert!(notice.is_some_and(|n| n.contains("restored")));
        assert_eq!(*api.0.lock().unwrap(), tv);
        assert_eq!(resume_setup(Some(&g), &[dev("CABLE Input (VB-Audio Virtual Cable)", true)]), (false, None), "nothing left to resume");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
