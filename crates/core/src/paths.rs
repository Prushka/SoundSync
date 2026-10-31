//! Per-user file locations, optionally suffixed by an instance name so several nodes can run on
//! one machine for testing.
//!
//! | file   | Windows                              | macOS                                            |
//! |--------|--------------------------------------|--------------------------------------------------|
//! | config | `%APPDATA%\SoundSync\config.toml`    | `~/Library/Application Support/SoundSync/config.toml` |
//! | logs   | `%LOCALAPPDATA%\SoundSync\logs\`     | `~/Library/Logs/SoundSync/`                      |
//! | state  | `%LOCALAPPDATA%\SoundSync\state.toml`| `~/Library/Application Support/SoundSync/state.toml` |

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub config_file: PathBuf,
    pub log_dir: PathBuf,
    pub state_file: PathBuf,
}

fn dir_name(instance: Option<&str>) -> String {
    match instance.map(|i| i.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect::<String>()) {
        Some(i) if !i.is_empty() => format!("SoundSync-{i}"),
        _ => "SoundSync".to_string(),
    }
}

impl Paths {
    pub fn for_instance(instance: Option<&str>) -> Paths {
        let name = dir_name(instance);
        let base = directories::BaseDirs::new();
        let fallback = || std::env::temp_dir();
        if cfg!(target_os = "macos") {
            let support = base.as_ref().map_or_else(fallback, |b| b.data_dir().to_path_buf()).join(&name);
            let logs = base.as_ref().map_or_else(fallback, |b| b.home_dir().join("Library").join("Logs")).join(&name);
            Paths { config_file: support.join("config.toml"), log_dir: logs, state_file: support.join("state.toml") }
        } else {
            let roaming = base.as_ref().map_or_else(fallback, |b| b.config_dir().to_path_buf()).join(&name);
            let local = base.as_ref().map_or_else(fallback, |b| b.data_local_dir().to_path_buf()).join(&name);
            Paths { config_file: roaming.join("config.toml"), log_dir: local.join("logs"), state_file: local.join("state.toml") }
        }
    }
}
