// Release builds are GUI apps (no console window); subcommands attach to the parent console.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod autostart;
mod cli;
mod headless;
mod icon;
mod logging;
mod macos_bundle;
#[cfg(windows)]
mod session_end;
mod single_instance;
mod ui;
mod update;

use std::sync::Arc;

use clap::Parser;
use soundsync_core::config::{Config, Paths};
use soundsync_engine::{Engine, EngineOptions};

use cli::{Cli, Cmd};
use single_instance::Instance;

/// The OS default-device switcher (Windows only).
pub fn default_devices_api() -> Option<Arc<dyn soundsync_engine::defaults::DefaultDeviceApi>> {
    #[cfg(windows)]
    {
        Some(Arc::new(soundsync_engine::defaults::WindowsDefaults))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Windows may end the session (shutdown, restart, sign-out) while the default output is
/// switched to VB-CABLE: put the saved defaults back before the process is ended.
#[cfg(windows)]
fn restore_defaults_at_session_end(state_file: std::path::PathBuf) {
    let Some(api) = default_devices_api() else { return };
    let watched = session_end::watch(move || {
        let mut guard = soundsync_engine::defaults::DefaultSwitchGuard::new(api.clone(), state_file.clone());
        if let Some(msg) = guard.recover_on_start() {
            tracing::info!("session ending: {msg}");
        }
    });
    if let Err(e) = watched {
        tracing::warn!("cannot watch for the end of the Windows session: {e}");
    }
}

/// `soundsync restore-defaults`: restores the default output devices saved in state.toml.
fn restore_defaults_now(paths: &Paths) -> anyhow::Result<()> {
    let restored = default_devices_api().and_then(|api| soundsync_engine::defaults::DefaultSwitchGuard::new(api, paths.state_file.clone()).recover_on_start());
    println!("{}", restored.unwrap_or_else(|| "nothing to restore".into()));
    Ok(())
}

/// Writes the app icon (1024x1024 PNG) for the macOS bundle script.
fn write_icon_png(path: &std::path::Path) -> anyhow::Result<()> {
    let size = 1024;
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, size, size);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&icon::icon_rgba(size))?;
    println!("wrote {}", path.display());
    Ok(())
}

/// Lets console output of subcommands reach the terminal they were started from.
fn attach_console() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn main() -> anyhow::Result<()> {
    if std::env::args().len() > 1 {
        attach_console();
    }
    let args = Cli::parse();
    match args.cmd {
        Some(Cmd::Devices) => {
            let host = soundsync_audio::default_host()?;
            cli::devices(host.as_ref())
        }
        Some(Cmd::Tone { device, freq, seconds, pulses }) => {
            let host = soundsync_audio::default_host()?;
            cli::tone(host.as_ref(), &device, freq, seconds, pulses)
        }
        Some(Cmd::Measure { device, a, b, seconds }) => {
            let host = soundsync_audio::default_host()?;
            cli::measure(host.as_ref(), device, a, b, seconds)
        }
        Some(Cmd::Run { broadcast, mode, capture, monitor, output, port, bind, peers, no_mdns, seconds }) => headless::run(
            headless::RunArgs { broadcast, mode, capture, monitor, output, port, bind, peers, no_mdns, seconds },
            args.config,
            args.instance.as_deref(),
        ),
        Some(Cmd::RestoreDefaults) => restore_defaults_now(&Paths::for_instance(args.instance.as_deref())),
        Some(Cmd::GenIcon { png, plist }) => {
            if let Some(p) = png {
                write_icon_png(&p)?;
            }
            if let Some(p) = plist {
                std::fs::write(&p, macos_bundle::info_plist(soundsync_core::version::PACKAGE_VERSION, soundsync_core::version::build_number()))?;
                println!("wrote {}", p.display());
            }
            Ok(())
        }
        None => {
            let paths = Paths::for_instance(args.instance.as_deref());
            let config_path = args.config.clone().unwrap_or_else(|| paths.config_file.clone());
            if args.quit {
                if !single_instance::signal_quit(&config_path) {
                    eprintln!("SoundSync is not running");
                }
                return Ok(());
            }
            match single_instance::acquire(&config_path)? {
                Instance::Secondary => {
                    single_instance::signal_show(&config_path);
                    Ok(())
                }
                Instance::Primary(guard) => {
                    let _log = logging::init_logging(&paths.log_dir)?;
                    let loaded = Config::load_or_create(&config_path)?;
                    for w in &loaded.warnings {
                        tracing::warn!("config: {w}");
                    }
                    let host = soundsync_audio::default_host()?;
                    let engine = Engine::start(EngineOptions {
                        config: loaded.config.clone(),
                        config_path: Some(config_path.clone()),
                        state_path: Some(paths.state_file.clone()),
                        host: Arc::clone(&host),
                        enable_mdns: !args.gui_no_mdns,
                        bind_ip: args.gui_bind,
                        impair: None,
                        default_devices: default_devices_api(),
                    })?;
                    #[cfg(windows)]
                    restore_defaults_at_session_end(paths.state_file.clone());
                    // Windows starts hidden from the Startup shortcut; on macOS a Finder launch
                    // (no --background) opens the window since there is no taskbar entry.
                    let opened_by_user = cfg!(target_os = "macos") && !args.background;
                    ui::run_gui(ui::AppContext {
                        start_visible: args.show || loaded.created || opened_by_user,
                        config: loaded.config,
                        load_warnings: loaded.warnings,
                        paths,
                        config_path,
                        host,
                        engine,
                        instance: guard,
                        bind_ip: args.gui_bind,
                        enable_mdns: !args.gui_no_mdns,
                    })
                }
            }
        }
    }
}

#[cfg(all(test, windows))]
mod com_tests {
    use std::net::Ipv4Addr;

    use soundsync_core::config::Config;
    use soundsync_engine::{Engine, EngineOptions};
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};

    /// winit initializes OLE (single-threaded COM) on the UI thread after the engine started, so
    /// starting the engine must not put the calling thread into the multithreaded apartment.
    #[test]
    fn engine_start_leaves_the_ui_thread_free_for_ole() {
        let Ok(host) = soundsync_audio::default_host() else { return }; // no audio stack (CI)
        let mut config = Config::default();
        config.network.port = 0;
        config.playback.output_device = "null".into();
        let engine = Engine::start(EngineOptions {
            config,
            config_path: None,
            state_path: None,
            host,
            enable_mdns: false,
            bind_ip: Some(Ipv4Addr::LOCALHOST),
            impair: None,
            default_devices: None,
        })
        .unwrap();
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        assert_ne!(hr, RPC_E_CHANGED_MODE, "Engine::start put the caller's thread into the MTA");
        if hr.is_ok() {
            unsafe { CoUninitialize() };
        }
        engine.shutdown();
    }

    /// After a crash or a Windows shutdown while switched, the next start restores the saved
    /// defaults; that recovery must not touch the UI thread's COM apartment either. The saved ids
    /// do not exist, so nothing on this PC is changed.
    #[test]
    fn crash_recovery_leaves_the_ui_thread_free_for_ole() {
        let Ok(host) = soundsync_audio::default_host() else { return };
        let dir = std::env::temp_dir().join(format!("soundsync-com-recovery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("state.toml");
        std::fs::write(&state, "[saved_defaults]\nconsole = \"{bogus-1}\"\nmultimedia = \"{bogus-1}\"\ncommunications = \"{bogus-2}\"\n").unwrap();
        let mut config = Config::default();
        config.network.port = 0;
        config.playback.output_device = "null".into();
        let engine = Engine::start(EngineOptions {
            config,
            config_path: None,
            state_path: Some(state),
            host,
            enable_mdns: false,
            bind_ip: Some(Ipv4Addr::LOCALHOST),
            impair: None,
            default_devices: Some(std::sync::Arc::new(soundsync_engine::defaults::WindowsDefaults)),
        })
        .unwrap();
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        assert_ne!(hr, RPC_E_CHANGED_MODE, "crash recovery put the caller's thread into the MTA");
        if hr.is_ok() {
            unsafe { CoUninitialize() };
        }
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
