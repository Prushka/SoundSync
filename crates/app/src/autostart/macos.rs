//! "Start at login" on macOS: a per-user LaunchAgent that opens SoundSync.app (hidden).

use std::path::PathBuf;

use super::AutostartError;
use crate::macos_bundle::{BUNDLE_ID, bundle_of, launch_agent_plist};

fn agent_path() -> Result<PathBuf, AutostartError> {
    let home = std::env::var_os("HOME").ok_or_else(|| AutostartError::Os("HOME is not set".into()))?;
    Ok(PathBuf::from(home).join("Library").join("LaunchAgents").join(format!("{BUNDLE_ID}.plist")))
}

pub fn is_enabled() -> Result<bool, AutostartError> {
    Ok(agent_path()?.exists())
}

pub fn set_enabled(enabled: bool) -> Result<(), AutostartError> {
    let path = agent_path()?;
    if enabled {
        let exe = std::env::current_exe()?;
        let bundle = bundle_of(&exe).ok_or_else(|| AutostartError::Os("Start at login needs SoundSync.app (this copy is not inside an app bundle)".into()))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, launch_agent_plist(&bundle))?;
    } else if path.exists() {
        let uid = unsafe { libc::getuid() };
        let _ = std::process::Command::new("launchctl").args(["bootout", &format!("gui/{uid}/{BUNDLE_ID}")]).output();
        std::fs::remove_file(&path)?;
    }
    Ok(())
}
