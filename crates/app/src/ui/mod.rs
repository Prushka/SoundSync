//! The SoundSync window and tray.

pub mod app;
pub mod config_watch;
pub mod format;
pub mod panels;
pub mod tray;
pub mod vbcable;

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use soundsync_audio::AudioHost;
use soundsync_core::config::{Config, Paths};
use soundsync_engine::Engine;

use crate::icon::icon_rgba;
use crate::single_instance::InstanceGuard;

pub struct AppContext {
    pub paths: Paths,
    pub config_path: PathBuf,
    pub config: Config,
    pub load_warnings: Vec<String>,
    pub host: Arc<dyn AudioHost>,
    pub engine: Engine,
    pub instance: InstanceGuard,
    /// Show the window at start (`--show`, or first run); otherwise start in the tray.
    pub start_visible: bool,
    /// Engine options to reuse when the engine restarts (`--bind`, `--no-mdns`).
    pub bind_ip: Option<std::net::Ipv4Addr>,
    pub enable_mdns: bool,
}

pub fn run_gui(ctx: AppContext) -> anyhow::Result<()> {
    let icon = egui::IconData { rgba: icon_rgba(64), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SoundSync")
            .with_inner_size([980.0, 680.0])
            .with_min_inner_size([640.0, 420.0])
            .with_icon(Arc::new(icon))
            .with_visible(ctx.start_visible),
        ..Default::default()
    };
    eframe::run_native("SoundSync", options, Box::new(move |cc| Ok(Box::new(app::SoundSyncApp::new(cc, ctx)))))
        .map_err(|e| anyhow::anyhow!("cannot open the SoundSync window: {e}"))
}
