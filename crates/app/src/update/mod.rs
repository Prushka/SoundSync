//! Self-update from GitHub releases: finds a newer build, downloads and checks it, and replaces
//! the copy that is running (the exe on Windows, SoundSync.app on macOS).

pub mod fetch;
pub mod release;
