//! Start SoundSync when the user signs in.
//!
//! * Windows: a `SoundSync.lnk` in the user's Startup folder (no admin), matching the install
//!   script. A shortcut that points at another program is never replaced or removed.
//! * macOS: a LaunchAgent that runs `open -a SoundSync.app` (Task 26).

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
mod windows;

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum AutostartError {
    #[error("{0} belongs to another program; not replacing it")]
    #[cfg_attr(not(windows), allow(dead_code))]
    ForeignShortcut(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Os(String),
}

pub fn is_enabled() -> Result<bool, AutostartError> {
    #[cfg(windows)]
    {
        windows::is_enabled_in(&windows::startup_dir()?, &std::env::current_exe()?)
    }
    #[cfg(target_os = "macos")]
    {
        macos::is_enabled()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Ok(false)
    }
}

pub fn set_enabled(enabled: bool) -> Result<(), AutostartError> {
    #[cfg(windows)]
    {
        windows::set_enabled_in(&windows::startup_dir()?, &std::env::current_exe()?, enabled)
    }
    #[cfg(target_os = "macos")]
    {
        macos::set_enabled(enabled)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = enabled;
        Err(AutostartError::Os("autostart is not supported on this platform".into()))
    }
}
