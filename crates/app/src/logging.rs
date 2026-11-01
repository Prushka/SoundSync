//! Per-session log files: `soundsync.log` is the current session; at start-up the previous one
//! is archived as `soundsync-<UTC yyyymmdd-hhmmss>.log` (`-recovered` if that session crashed,
//! detected by a leftover `.running` marker), keeping the newest 5 archives.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing_subscriber::EnvFilter;

const CURRENT: &str = "soundsync.log";
const MARKER: &str = ".running";
const KEEP: usize = 5;

#[derive(Debug)]
pub struct RotateOutcome {
    pub previous_crashed: bool,
    pub archived: Option<PathBuf>,
}

/// `yyyymmdd-hhmmss` in UTC.
pub fn utc_stamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

pub fn rotate_logs(dir: &Path, now: SystemTime) -> io::Result<RotateOutcome> {
    std::fs::create_dir_all(dir)?;
    let marker = dir.join(MARKER);
    let previous_crashed = marker.exists();
    let current = dir.join(CURRENT);
    let mut archived = None;
    if current.exists() {
        let name = format!("soundsync-{}{}.log", utc_stamp(now), if previous_crashed { "-recovered" } else { "" });
        let target = dir.join(name);
        std::fs::rename(&current, &target)?;
        archived = Some(target);
    }
    if previous_crashed {
        let _ = std::fs::remove_file(&marker);
    }
    let mut archives: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("soundsync-") && n.ends_with(".log")))
        .collect();
    archives.sort(); // timestamps sort chronologically
    let excess = archives.len().saturating_sub(KEEP);
    for old in &archives[..excess] {
        let _ = std::fs::remove_file(old);
    }
    Ok(RotateOutcome { previous_crashed, archived })
}

/// The `.running` file: present while a session runs, removed only on a clean exit (a drop
/// during a panic leaves it, so the next start archives that log as `-recovered`).
pub struct RunMarker(PathBuf);

impl RunMarker {
    pub fn create(dir: &Path) -> io::Result<RunMarker> {
        let path = dir.join(MARKER);
        std::fs::write(&path, b"")?;
        Ok(RunMarker(path))
    }
}

impl Drop for RunMarker {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

/// Keeps logging alive; on drop it marks the session as cleanly ended.
pub struct LogGuard {
    marker: Option<RunMarker>,
    _worker: tracing_appender::non_blocking::WorkerGuard,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            tracing::error!("exiting after a panic");
        } else {
            tracing::info!("clean exit");
        }
        drop(self.marker.take());
    }
}

/// Rotates the previous session's log and routes `tracing` to `dir/soundsync.log`.
pub fn init_logging(dir: &Path) -> anyhow::Result<LogGuard> {
    let outcome = rotate_logs(dir, SystemTime::now())?;
    let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(CURRENT))?;
    let marker = RunMarker::create(dir)?;
    let (writer, worker) = tracing_appender::non_blocking(file);
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,wgpu_hal=warn,wgpu_core=warn,egui_wgpu=warn,naga=warn".into()))
        .with_ansi(false)
        .with_writer(writer)
        .try_init();
    tracing::info!("SoundSync {} starting", soundsync_core::version::version());
    if outcome.previous_crashed {
        tracing::warn!("the previous session did not exit cleanly; its log was kept as {:?}", outcome.archived);
    }
    // Panics in any thread go to the log too.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default_hook(info);
    }));
    Ok(LogGuard { marker: Some(marker), _worker: worker })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("soundsync-logtest-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn utc_stamp_formats_civil_time() {
        assert_eq!(utc_stamp(at(0)), "19700101-000000");
        assert_eq!(utc_stamp(at(1_793_428_592)), "20261031-063632");
        assert_eq!(utc_stamp(at(951_782_400)), "20000229-000000", "leap day");
    }

    #[test]
    fn clean_session_plain_archive() {
        let d = dir("clean");
        std::fs::write(d.join("soundsync.log"), "old session").unwrap();
        let out = rotate_logs(&d, at(1_793_428_592)).unwrap();
        assert!(!out.previous_crashed);
        let archived = out.archived.unwrap();
        assert_eq!(archived.file_name().unwrap(), "soundsync-20261031-063632.log");
        assert_eq!(std::fs::read_to_string(&archived).unwrap(), "old session");
        assert!(!d.join("soundsync.log").exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn crashed_session_gets_recovered_suffix() {
        let d = dir("crash");
        std::fs::write(d.join("soundsync.log"), "crashed").unwrap();
        std::fs::write(d.join(".running"), "").unwrap();
        let out = rotate_logs(&d, at(1_793_428_592)).unwrap();
        assert!(out.previous_crashed);
        assert_eq!(out.archived.unwrap().file_name().unwrap(), "soundsync-20261031-063632-recovered.log");
        assert!(!d.join(".running").exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn rotation_keeps_five_archives() {
        let d = dir("keep");
        for i in 0..7u64 {
            std::fs::write(d.join("soundsync.log"), format!("session {i}")).unwrap();
            rotate_logs(&d, at(1_700_000_000 + i * 60)).unwrap();
        }
        let mut names: Vec<String> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names.len(), 5, "{names:?}");
        assert_eq!(std::fs::read_to_string(d.join(&names[0])).unwrap(), "session 2", "oldest two deleted");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn run_marker_kept_when_dropped_during_panic() {
        let d = dir("panic");
        let marker_dir = d.clone();
        let result = std::panic::catch_unwind(move || {
            let _m = RunMarker::create(&marker_dir).unwrap();
            panic!("simulated crash");
        });
        assert!(result.is_err());
        assert!(d.join(".running").exists(), "a panic is a crash, not a clean exit");
        {
            let _m = RunMarker::create(&d).unwrap();
        }
        assert!(!d.join(".running").exists(), "normal drop marks a clean exit");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn no_previous_log_is_fine() {
        let d = dir("empty");
        let out = rotate_logs(&d, at(1)).unwrap();
        assert!(out.archived.is_none() && !out.previous_crashed);
        let _ = std::fs::remove_dir_all(d);
    }
}
