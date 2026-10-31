//! Optional switching of the Windows default output to VB-CABLE while broadcasting, with the
//! previous defaults saved to `state.toml` first so they are restored on stop, on exit, or on
//! the next start after a crash. Also the "apps play but we hear silence" watchdog.

use std::path::PathBuf;
use std::sync::Arc;

pub use soundsync_audio::{DefaultDevices, DefaultRole};
use soundsync_core::Nanos;

const SAVED: &str = "saved_defaults";
const SETUP: &str = "vbcable_setup";
/// Unix time until which roles whose device was missing are restored when it reappears.
const PENDING_UNTIL: &str = "pending_restore_until";
const PENDING_FOR_SECS: i64 = 600;
pub const PENDING_PREFIX: &str = "could not restore the default output yet";
const SILENCE_WARN_NS: Nanos = 5_000_000_000;

/// Access to the OS default-device settings (real one on Windows, mock in tests).
pub trait DefaultDeviceApi: Send + Sync {
    fn read(&self) -> Result<DefaultDevices, String>;
    fn set(&self, id: &str, role: DefaultRole) -> Result<(), String>;
    fn exists(&self, id: &str) -> bool;
}

pub struct DefaultSwitchGuard {
    api: Arc<dyn DefaultDeviceApi>,
    state_path: PathBuf,
    /// This guard switched the defaults and has not restored them yet.
    switched: bool,
}

const ROLES: [DefaultRole; 3] = [DefaultRole::Console, DefaultRole::Multimedia, DefaultRole::Communications];

fn id_for(d: &DefaultDevices, role: DefaultRole) -> &str {
    match role {
        DefaultRole::Console => &d.console,
        DefaultRole::Multimedia => &d.multimedia,
        DefaultRole::Communications => &d.communications,
    }
}

fn id_mut(d: &mut DefaultDevices, role: DefaultRole) -> &mut String {
    match role {
        DefaultRole::Console => &mut d.console,
        DefaultRole::Multimedia => &mut d.multimedia,
        DefaultRole::Communications => &mut d.communications,
    }
}

fn is_empty(d: &DefaultDevices) -> bool {
    ROLES.iter().all(|r| id_for(d, *r).is_empty())
}

fn ids(d: &DefaultDevices) -> String {
    let mut v: Vec<&str> = ROLES.iter().map(|r| id_for(d, *r)).filter(|i| !i.is_empty()).collect();
    v.dedup();
    v.join(", ")
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl DefaultSwitchGuard {
    pub fn new(api: Arc<dyn DefaultDeviceApi>, state_path: PathBuf) -> Self {
        DefaultSwitchGuard { api, state_path, switched: false }
    }

    fn load_state(&self) -> toml::Table {
        std::fs::read_to_string(&self.state_path).ok().and_then(|s| s.parse::<toml::Table>().ok()).unwrap_or_default()
    }

    fn store_state(&self, t: &toml::Table) -> Result<(), String> {
        if let Some(dir) = self.state_path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = toml::to_string_pretty(t).map_err(|e| e.to_string())?;
        std::fs::write(&self.state_path, text).map_err(|e| format!("cannot write {}: {e}", self.state_path.display()))
    }

    fn get(&self, key: &str) -> Option<DefaultDevices> {
        self.load_state().get(key).cloned().and_then(|v| v.try_into().ok())
    }

    fn put(&self, key: &str, d: Option<&DefaultDevices>) -> Result<(), String> {
        let mut t = self.load_state();
        match d {
            Some(d) => {
                t.insert(key.into(), toml::Value::try_from(d).map_err(|e| e.to_string())?);
            }
            None => {
                t.remove(key);
            }
        }
        self.store_state(&t)
    }

    /// Sets each role back; returns the roles that could not be (their device is missing).
    fn apply(&self, d: &DefaultDevices) -> Result<DefaultDevices, String> {
        let mut remaining = DefaultDevices { console: String::new(), multimedia: String::new(), communications: String::new() };
        for role in ROLES {
            let id = id_for(d, role);
            if id.is_empty() {
                continue;
            }
            if self.api.exists(id) {
                self.api.set(id, role)?;
            } else {
                *id_mut(&mut remaining, role) = id.to_string();
            }
        }
        Ok(remaining)
    }

    fn pending_until(&self) -> Option<i64> {
        self.load_state().get(PENDING_UNTIL).and_then(|v| v.as_integer())
    }

    fn set_pending_until(&self, until: Option<i64>) -> Result<(), String> {
        let mut t = self.load_state();
        match until {
            Some(u) => t.insert(PENDING_UNTIL.into(), toml::Value::Integer(u)),
            None => t.remove(PENDING_UNTIL),
        };
        self.store_state(&t)
    }

    /// Records what is left after a restore: nothing (done), or roles whose device is missing,
    /// kept for [`Self::retry_pending`] until a deadline.
    fn finish(&self, remaining: DefaultDevices) -> Result<Option<String>, String> {
        if is_empty(&remaining) {
            self.put(SAVED, None)?;
            self.set_pending_until(None)?;
            return Ok(None);
        }
        self.put(SAVED, Some(&remaining))?;
        if self.pending_until().is_none() {
            self.set_pending_until(Some(unix_now() + PENDING_FOR_SECS))?;
        }
        Ok(Some(format!("{PENDING_PREFIX}: {} is not available; SoundSync restores it when it reappears", ids(&remaining))))
    }

    /// Drops a pending restore whose device never came back. Returns the message if it did.
    fn expire_pending(&self) -> Option<String> {
        let until = self.pending_until()?;
        if unix_now() <= until {
            return None;
        }
        let gone = self.get(SAVED).map(|d| ids(&d)).unwrap_or_default();
        let _ = self.put(SAVED, None);
        let _ = self.set_pending_until(None);
        Some(format!("gave up restoring the default output {gone}: it did not reappear"))
    }

    /// Restores defaults left switched by a crash or an unclean Windows shutdown (also used at
    /// session end). Returns a message when it did something.
    pub fn recover_on_start(&mut self) -> Option<String> {
        let saved = self.get(SAVED)?;
        if let Some(msg) = self.expire_pending() {
            return Some(msg);
        }
        let outcome = self.apply(&saved).and_then(|remaining| self.finish(remaining));
        Some(match outcome {
            Ok(None) => "restored the Windows default output devices saved before an unexpected exit".into(),
            Ok(Some(w)) => w,
            Err(e) => format!("cannot restore the default output: {e}"),
        })
    }

    /// Restores roles left pending because their device was missing, once it is back (call on
    /// device arrival). Returns a message when something was restored or given up.
    pub fn retry_pending(&mut self) -> Option<String> {
        if self.switched {
            return None;
        }
        let saved = self.get(SAVED)?;
        if let Some(msg) = self.expire_pending() {
            return Some(msg);
        }
        let remaining = self.apply(&saved).ok()?;
        if remaining == saved {
            return None;
        }
        let restored = ids(&saved);
        match self.finish(remaining) {
            Ok(None) => Some(format!("restored the default output {restored}")),
            _ => None,
        }
    }

    /// The defaults saved before switching, while switched (or pending restore).
    pub fn saved(&self) -> Option<DefaultDevices> {
        self.get(SAVED)
    }

    /// Saves the current defaults (keeping originals already saved, e.g. still pending) and
    /// makes `target_id` the default.
    pub fn switch_to(&mut self, target_id: &str) -> Result<(), String> {
        let current = self.api.read()?;
        let mut saved = self.get(SAVED).unwrap_or(current.clone());
        for role in ROLES {
            if id_for(&saved, role).is_empty() {
                *id_mut(&mut saved, role) = id_for(&current, role).to_string();
            }
        }
        self.put(SAVED, Some(&saved))?;
        self.set_pending_until(None)?;
        self.switched = true;
        for role in ROLES {
            self.api.set(target_id, role)?;
        }
        Ok(())
    }

    /// Undoes this guard's switch. Ok(Some(warning)) if a device was missing (kept pending).
    pub fn restore(&mut self) -> Result<Option<String>, String> {
        if !self.switched {
            return Ok(None);
        }
        let Some(saved) = self.get(SAVED) else {
            self.switched = false;
            return Ok(None);
        };
        self.switched = false;
        let remaining = self.apply(&saved)?;
        self.finish(remaining)
    }

    /// Before installing VB-CABLE (whose installer may make itself the default).
    pub fn save_for_setup(&self) -> Result<(), String> {
        let current = self.api.read()?;
        self.put(SETUP, Some(&current))
    }

    /// A VB-CABLE setup was started and its snapshot is still waiting to be restored.
    pub fn setup_pending(&self) -> bool {
        self.get(SETUP).is_some()
    }

    /// After the cable appeared: restore what was default before. False if nothing was saved.
    pub fn restore_after_setup(&self) -> Result<bool, String> {
        let Some(saved) = self.get(SETUP) else { return Ok(false) };
        self.apply(&saved)?;
        self.put(SETUP, None)?;
        Ok(true)
    }
}

/// The real Windows default-device settings.
#[cfg(windows)]
pub struct WindowsDefaults;

#[cfg(windows)]
impl DefaultDeviceApi for WindowsDefaults {
    fn read(&self) -> Result<DefaultDevices, String> {
        soundsync_audio::win::policy::read_defaults().map_err(|e| e.to_string())
    }
    fn set(&self, id: &str, role: DefaultRole) -> Result<(), String> {
        soundsync_audio::win::policy::set_default_render(id, role).map_err(|e| e.to_string())
    }
    fn exists(&self, id: &str) -> bool {
        soundsync_audio::win::policy::endpoint_active(id)
    }
}

/// Warns when apps are playing to the captured device but SoundSync only hears silence
/// (exclusive-mode app or protected content).
#[derive(Default)]
pub struct SilenceWatchdog {
    silent_since: Option<Nanos>,
}

impl SilenceWatchdog {
    pub fn update(&mut self, now: Nanos, capture_silent: bool, active_sessions: usize) -> bool {
        if capture_silent && active_sessions > 0 {
            let since = *self.silent_since.get_or_insert(now);
            now - since >= SILENCE_WARN_NS
        } else {
            self.silent_since = None;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockApi {
        current: Mutex<Option<DefaultDevices>>,
        missing: Mutex<Vec<String>>,
        calls: Mutex<Vec<(String, DefaultRole)>>,
    }

    impl MockApi {
        fn with(console: &str, multimedia: &str, communications: &str) -> Arc<MockApi> {
            let m = MockApi::default();
            *m.current.lock().unwrap() = Some(DefaultDevices { console: console.into(), multimedia: multimedia.into(), communications: communications.into() });
            Arc::new(m)
        }
        fn current(&self) -> DefaultDevices {
            self.current.lock().unwrap().clone().unwrap()
        }
    }

    impl DefaultDeviceApi for MockApi {
        fn read(&self) -> Result<DefaultDevices, String> {
            self.current.lock().unwrap().clone().ok_or_else(|| "none".into())
        }
        fn set(&self, id: &str, role: DefaultRole) -> Result<(), String> {
            self.calls.lock().unwrap().push((id.to_string(), role));
            let mut c = self.current.lock().unwrap();
            let d = c.as_mut().unwrap();
            match role {
                DefaultRole::Console => d.console = id.into(),
                DefaultRole::Multimedia => d.multimedia = id.into(),
                DefaultRole::Communications => d.communications = id.into(),
            }
            Ok(())
        }
        fn exists(&self, id: &str) -> bool {
            !self.missing.lock().unwrap().iter().any(|m| m == id)
        }
    }

    fn state_path(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("soundsync-defaults-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("state.toml")
    }

    #[test]
    fn saves_then_switches_then_restores() {
        let api = MockApi::with("{tv}", "{tv}", "{headset}");
        let path = state_path("roundtrip");
        let mut g = DefaultSwitchGuard::new(api.clone(), path.clone());
        g.switch_to("{cable}").unwrap();
        assert_eq!(api.current(), DefaultDevices { console: "{cable}".into(), multimedia: "{cable}".into(), communications: "{cable}".into() });
        assert!(std::fs::read_to_string(&path).unwrap().contains("{headset}"), "previous defaults are on disk before switching");
        // Switching again must not overwrite the saved originals with the cable.
        g.switch_to("{cable}").unwrap();
        assert_eq!(g.restore().unwrap(), None);
        assert_eq!(api.current(), DefaultDevices { console: "{tv}".into(), multimedia: "{tv}".into(), communications: "{headset}".into() });
        assert!(!std::fs::read_to_string(&path).unwrap_or_default().contains("saved_defaults"));
    }

    #[test]
    fn crash_recovery_restores_on_start() {
        let api = MockApi::with("{tv}", "{tv}", "{tv}");
        let path = state_path("crash");
        {
            let mut g = DefaultSwitchGuard::new(api.clone(), path.clone());
            g.switch_to("{cable}").unwrap();
            std::mem::forget(g); // the process crashes while switched
        }
        let mut g = DefaultSwitchGuard::new(api.clone(), path.clone());
        let msg = g.recover_on_start().expect("recovery happened");
        assert!(msg.contains("restored"), "{msg}");
        assert_eq!(api.current().console, "{tv}");
        assert!(g.recover_on_start().is_none(), "state cleared after recovery");
    }

    #[test]
    fn restore_skips_missing_device() {
        let api = MockApi::with("{tv}", "{usb}", "{tv}");
        let path = state_path("missing");
        let mut g = DefaultSwitchGuard::new(api.clone(), path);
        g.switch_to("{cable}").unwrap();
        api.missing.lock().unwrap().push("{usb}".into());
        let warning = g.restore().unwrap().expect("warns about the missing device");
        assert!(warning.contains("{usb}"), "{warning}");
        assert_eq!(api.current().console, "{tv}");
        assert_eq!(api.current().multimedia, "{cable}", "a missing device is not forced back");
    }

    #[test]
    fn missing_device_stays_pending_until_it_returns() {
        // At early login a USB/Bluetooth default may not exist yet: forgetting it would leave the
        // user's speakers on whatever Windows picked.
        let api = MockApi::with("{tv}", "{usb}", "{tv}");
        let path = state_path("pending");
        let mut g = DefaultSwitchGuard::new(api.clone(), path.clone());
        g.switch_to("{cable}").unwrap();
        api.missing.lock().unwrap().push("{usb}".into());
        assert!(g.restore().unwrap().is_some_and(|w| w.contains("{usb}")));
        assert_eq!(g.saved().map(|d| d.multimedia), Some("{usb}".into()), "kept for later");
        assert_eq!(g.retry_pending(), None, "still missing");
        api.missing.lock().unwrap().clear();
        assert!(g.retry_pending().is_some_and(|m| m.contains("restored")));
        assert_eq!(api.current().multimedia, "{usb}");
        assert!(g.saved().is_none());
        assert_eq!(g.retry_pending(), None);
    }

    #[test]
    fn switch_after_partial_recovery_keeps_the_pending_original() {
        let api = MockApi::with("{tv}", "{usb}", "{tv}");
        let path = state_path("pending-switch");
        let mut g = DefaultSwitchGuard::new(api.clone(), path.clone());
        g.switch_to("{cable}").unwrap();
        std::mem::forget(g); // crash while switched
        api.missing.lock().unwrap().push("{usb}".into());
        let mut g = DefaultSwitchGuard::new(api.clone(), path.clone());
        assert!(g.recover_on_start().is_some_and(|m| m.contains("{usb}")));
        // Windows put multimedia on the TV meanwhile; broadcasting switches to the cable again.
        api.set("{tv}", DefaultRole::Multimedia).unwrap();
        g.switch_to("{cable}").unwrap();
        api.missing.lock().unwrap().clear();
        g.restore().unwrap();
        assert_eq!(api.current(), DefaultDevices { console: "{tv}".into(), multimedia: "{usb}".into(), communications: "{tv}".into() });
    }

    #[test]
    fn pending_restore_gives_up_after_its_deadline() {
        let api = MockApi::with("{tv}", "{tv}", "{tv}");
        let path = state_path("pending-expired");
        std::fs::write(&path, "pending_restore_until = 1\n[saved_defaults]\nconsole = \"\"\nmultimedia = \"{usb}\"\ncommunications = \"\"\n").unwrap();
        let mut g = DefaultSwitchGuard::new(api.clone(), path.clone());
        assert!(g.recover_on_start().is_some_and(|m| m.contains("gave up")));
        assert!(g.saved().is_none());
        assert!(api.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn restore_without_a_switch_changes_nothing() {
        let api = MockApi::with("{tv}", "{tv}", "{tv}");
        let path = state_path("not-switched");
        std::fs::write(&path, "[saved_defaults]\nconsole = \"{old}\"\nmultimedia = \"{old}\"\ncommunications = \"{old}\"\n").unwrap();
        let mut g = DefaultSwitchGuard::new(api.clone(), path);
        assert_eq!(g.restore().unwrap(), None, "only a switch made by this guard is undone");
        assert!(api.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn no_state_means_no_action() {
        let api = MockApi::with("{tv}", "{tv}", "{tv}");
        let mut g = DefaultSwitchGuard::new(api.clone(), state_path("none"));
        assert!(g.recover_on_start().is_none());
        assert_eq!(g.restore().unwrap(), None);
        assert!(api.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn setup_snapshot_restores_after_install() {
        let api = MockApi::with("{tv}", "{tv}", "{tv}");
        let g = DefaultSwitchGuard::new(api.clone(), state_path("setup"));
        g.save_for_setup().unwrap();
        // The VB-CABLE installer makes itself the default.
        api.set("{cable}", DefaultRole::Console).unwrap();
        assert!(g.restore_after_setup().unwrap());
        assert_eq!(api.current().console, "{tv}");
        assert!(!g.restore_after_setup().unwrap(), "nothing left to restore");
    }

    #[test]
    fn silence_watchdog_trips_after_5s_with_sessions() {
        let s = 1_000_000_000;
        let mut w = SilenceWatchdog::default();
        assert!(!w.update(0, true, 1));
        assert!(!w.update(4 * s + 900_000_000, true, 1));
        assert!(w.update(5 * s, true, 1));
        assert!(!w.update(6 * s, false, 1), "audio returning resets it");
        assert!(!w.update(7 * s, true, 1));
        let mut quiet = SilenceWatchdog::default();
        for t in 0..20 {
            assert!(!quiet.update(t * s, true, 0), "no app playing: silence is expected");
        }
    }
}
