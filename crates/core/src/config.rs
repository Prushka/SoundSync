//! Per-node configuration file (`config.toml`).
//!
//! Loading never fails because of content: a BOM is stripped, unknown keys and out-of-range
//! values produce warnings and defaults, a section with wrongly typed values falls back to its
//! defaults on its own, and an unparseable file is backed up and replaced by defaults.

pub use crate::paths::Paths;

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub use crate::delay::DelaySetting;
use crate::protocol::{DEFAULT_PORT, GroupKey};
pub use crate::types::MasterMode;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub node: NodeSection,
    pub network: NetworkSection,
    pub master: MasterSection,
    pub playback: PlaybackSection,
    pub sync: SyncSection,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeSection {
    /// Random identity; 0 means "generate on next load".
    pub id: u64,
    /// Display name; empty means the computer's hostname.
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkSection {
    pub group: String,
    /// Optional shared secret; nodes only talk to nodes with the same group and key.
    pub key: String,
    pub port: u16,
    /// "auto", an interface name (substring) or an IPv4 address.
    pub interface: String,
    /// `host:port` entries contacted directly (for networks that block mDNS).
    pub static_peers: Vec<String>,
}

impl Default for NetworkSection {
    fn default() -> Self {
        Self { group: "default".into(), key: String::new(), port: DEFAULT_PORT, interface: "auto".into(), static_peers: Vec::new() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MasterSection {
    pub enabled: bool,
    pub mode: MasterMode,
    /// Empty: "CABLE Input" in virtual mode, the default output in direct mode.
    pub capture_device: String,
    /// Real output that plays this PC's own stream in virtual mode; empty = default output.
    pub monitor_device: String,
    /// Switch the Windows default output to VB-CABLE while broadcasting (restored afterwards).
    pub switch_default: bool,
    pub keepalive_low_latency: bool,
}

impl Default for MasterSection {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: MasterMode::Virtual,
            capture_device: String::new(),
            monitor_device: String::new(),
            switch_default: false,
            keepalive_low_latency: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackSection {
    /// Empty = default output; "null" = discard (headless/testing).
    pub output_device: String,
    pub volume: f32,
    pub muted: bool,
    pub follow_master_volume: bool,
    pub play_remote: bool,
    /// Latency after the DAC that the OS doesn't report (TV processing, Bluetooth, ...).
    pub extra_latency_ms: f64,
    pub low_latency: bool,
}

impl Default for PlaybackSection {
    fn default() -> Self {
        Self {
            output_device: String::new(),
            volume: 1.0,
            muted: false,
            follow_master_volume: true,
            play_remote: true,
            extra_latency_ms: 0.0,
            low_latency: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncSection {
    pub delay_ms: DelaySetting,
    pub margin_ms: f64,
}

impl Default for SyncSection {
    fn default() -> Self {
        Self { delay_ms: DelaySetting::Auto, margin_ms: 5.0 }
    }
}

#[derive(Clone, Debug)]
pub struct LoadOutcome {
    pub config: Config,
    /// The file was missing or unreadable and has been (re)written with defaults.
    pub created: bool,
    pub warnings: Vec<String>,
}

fn section<T>(table: &toml::Table, name: &str, warnings: &mut Vec<String>) -> T
where
    T: Default + Serialize + for<'de> Deserialize<'de>,
{
    let Some(value) = table.get(name) else { return T::default() };
    let Some(t) = value.as_table() else {
        warnings.push(format!("[{name}] is not a table; using defaults"));
        return T::default();
    };
    if let Ok(known) = toml::Table::try_from(T::default()) {
        for key in t.keys().filter(|k| !known.contains_key(*k)) {
            warnings.push(format!("unknown key {name}.{key} ignored"));
        }
    }
    match T::deserialize(toml::Value::Table(t.clone())) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("[{name}] is invalid ({}); using defaults for this section", e.message()));
            T::default()
        }
    }
}

fn default_name() -> String {
    let host = gethostname::gethostname().to_string_lossy().trim().to_string();
    if host.is_empty() { "SoundSync node".into() } else { host }
}

/// Random non-zero id that fits a TOML (signed 64-bit) integer.
fn random_id() -> u64 {
    loop {
        let id: u64 = rand::random::<u64>() & i64::MAX as u64;
        if id != 0 {
            return id;
        }
    }
}

impl Config {
    /// Parses config file bytes (UTF-8 BOM tolerated, values clamped) without touching any file
    /// or inventing an id. Err(message) if it is not valid TOML.
    pub fn parse(bytes: &[u8]) -> Result<(Config, Vec<String>), String> {
        let text = String::from_utf8_lossy(bytes);
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
        let table = text.parse::<toml::Table>().map_err(|e| e.message().to_string())?;
        let mut warnings = Vec::new();
        let mut config = Self::from_table(&table, &mut warnings);
        warnings.extend(config.validate());
        Ok((config, warnings))
    }

    /// Identifies a config file's exact content (FNV-1a 64), to tell our own saves from edits.
    pub fn fingerprint(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3))
    }

    pub fn load_or_create(path: &Path) -> io::Result<LoadOutcome> {
        let mut warnings = Vec::new();
        let mut created = false;
        let mut config = match std::fs::read(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                created = true;
                Config::default()
            }
            Err(e) => return Err(e),
            Ok(bytes) => {
                match Self::parse(&bytes) {
                    Ok((config, w)) => {
                        warnings.extend(w);
                        config
                    }
                    Err(e) => {
                        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                        let backup = path.with_file_name(format!(
                            "{}.bad-{secs}",
                            path.file_name().map_or("config.toml".into(), |n| n.to_string_lossy().into_owned())
                        ));
                        std::fs::rename(path, &backup)?;
                        warnings.push(format!(
                            "{} is not valid TOML ({}); saved it as {} and started from defaults",
                            path.display(),
                            e,
                            backup.display()
                        ));
                        created = true;
                        Config::default()
                    }
                }
            }
        };
        warnings.extend(config.validate());
        let mut changed = created;
        if config.node.id == 0 {
            config.node.id = random_id();
            changed = true;
        }
        if config.node.name.trim().is_empty() {
            config.node.name = default_name();
            changed = true;
        }
        if changed {
            config.save(path)?;
        }
        Ok(LoadOutcome { config, created, warnings })
    }

    fn from_table(table: &toml::Table, warnings: &mut Vec<String>) -> Config {
        const SECTIONS: [&str; 5] = ["node", "network", "master", "playback", "sync"];
        for key in table.keys().filter(|k| !SECTIONS.contains(&k.as_str())) {
            warnings.push(format!("unknown section [{key}] ignored"));
        }
        Config {
            node: section(table, "node", warnings),
            network: section(table, "network", warnings),
            master: section(table, "master", warnings),
            playback: section(table, "playback", warnings),
            sync: section(table, "sync", warnings),
        }
    }

    /// Writes atomically (temp file + rename).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        Self::save_text(path, &self.to_text()?)
    }

    /// The file content [`Self::save`] writes.
    pub fn to_text(&self) -> io::Result<String> {
        toml::to_string_pretty(self).map_err(io::Error::other)
    }

    /// Writes `text` through a temp file of its own, so several writers (engine and UI) never
    /// trip over each other's temp file.
    pub fn save_text(path: &Path, text: &str) -> io::Result<()> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("toml.{}-{n}.tmp", std::process::id()));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    /// Clamps out-of-range values in place; returns one warning per corrected field.
    pub fn validate(&mut self) -> Vec<String> {
        let mut w = Vec::new();
        if self.network.port == 0 {
            w.push(format!("network.port 0 is invalid; using {DEFAULT_PORT}"));
            self.network.port = DEFAULT_PORT;
        }
        if self.network.group.trim().is_empty() {
            w.push("network.group is empty; using \"default\"".into());
            self.network.group = "default".into();
        }
        let v = self.playback.volume;
        if !v.is_finite() || !(0.0..=1.0).contains(&v) {
            let fixed = if v.is_finite() { v.clamp(0.0, 1.0) } else { 1.0 };
            w.push(format!("playback.volume {v} is outside 0..1; using {fixed}"));
            self.playback.volume = fixed;
        }
        let e = self.playback.extra_latency_ms;
        if !e.is_finite() || !(-1000.0..=2000.0).contains(&e) {
            let fixed = if e.is_finite() { e.clamp(-1000.0, 2000.0) } else { 0.0 };
            w.push(format!("playback.extra_latency_ms {e} is outside -1000..2000; using {fixed}"));
            self.playback.extra_latency_ms = fixed;
        }
        let m = self.sync.margin_ms;
        if !m.is_finite() || !(0.0..=100.0).contains(&m) {
            let fixed = if m.is_finite() { m.clamp(0.0, 100.0) } else { 5.0 };
            w.push(format!("sync.margin_ms {m} is outside 0..100; using {fixed}"));
            self.sync.margin_ms = fixed;
        }
        if let DelaySetting::Manual(ms) = self.sync.delay_ms
            && (!ms.is_finite() || !(20.0..=2000.0).contains(&ms))
        {
            let fixed = if ms.is_finite() { ms.clamp(20.0, 2000.0) } else { 80.0 };
            w.push(format!("sync.delay_ms {ms} is outside 20..2000; using {fixed}"));
            self.sync.delay_ms = DelaySetting::Manual(fixed);
        }
        w
    }

    pub fn regenerate_id(&mut self) {
        let old = self.node.id;
        while self.node.id == old {
            self.node.id = random_id();
        }
    }

    pub fn group_key(&self) -> GroupKey {
        GroupKey::derive(&self.network.group, &self.network.key)
    }

    pub fn extra_latency_ns(&self) -> crate::types::Nanos {
        (self.playback.extra_latency_ms * 1e6).round() as crate::types::Nanos
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Unique scratch directory, removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("soundsync-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    #[test]
    fn parse_is_side_effect_free_and_reports_errors() {
        // The UI re-reads the file every second: a half-typed edit must not be renamed away or
        // replaced by defaults.
        assert!(Config::parse(b"[network\ngroup = ").is_err());
        let (c, warnings) = Config::parse("\u{FEFF}[playback]\nvolume = 3.0\n".as_bytes()).unwrap();
        assert_eq!(c.playback.volume, 1.0);
        assert_eq!(warnings.len(), 1);
        assert_eq!(c.node.id, 0, "parse never invents an id");
        assert_ne!(Config::fingerprint(b"a"), Config::fingerprint(b"b"));
    }

    #[test]
    fn concurrent_saves_do_not_collide() {
        // The engine (on commands) and the UI (network settings) both save the file.
        let dir = temp_dir("concurrent");
        let path = dir.join("config.toml");
        let threads: Vec<_> = (0..2)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut c = Config::default();
                    for i in 0..200 {
                        c.playback.volume = (t * 200 + i) as f32 / 400.0;
                        c.save(&path).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(Config::parse(&std::fs::read(&path).unwrap()).is_ok());
        let leftovers: Vec<_> = std::fs::read_dir(&dir.0).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn missing_file_creates_defaults_with_random_id() {
        let dir = temp_dir("create");
        let path = dir.join("config.toml");
        let out = Config::load_or_create(&path).unwrap();
        assert!(out.created);
        assert_ne!(out.config.node.id, 0);
        assert!(!out.config.node.name.is_empty());
        assert_eq!(out.config.network.port, 47810);
        assert_eq!(out.config.master.mode, MasterMode::Virtual);
        assert!(path.exists());
        // Loading again keeps the same identity.
        let again = Config::load_or_create(&path).unwrap();
        assert!(!again.created);
        assert_eq!(again.config.node.id, out.config.node.id);
        assert!(again.warnings.is_empty(), "{:?}", again.warnings);
    }

    #[test]
    fn roundtrip_preserves_values() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("config.toml");
        let mut c = Config::default();
        c.node.id = 0xABCD;
        c.node.name = "Living Room".into();
        c.network.group = "home".into();
        c.network.key = "s3cret".into();
        c.network.static_peers = vec!["192.168.1.20:47810".into()];
        c.master.enabled = true;
        c.master.mode = MasterMode::Direct;
        c.master.capture_device = "Speakers".into();
        c.playback.volume = 0.25;
        c.playback.extra_latency_ms = 42.5;
        c.sync.delay_ms = DelaySetting::Manual(120.0);
        c.save(&path).unwrap();
        let out = Config::load_or_create(&path).unwrap();
        assert_eq!(out.config, c);
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    }

    #[test]
    fn delay_auto_and_number_parse() {
        let dir = temp_dir("delay");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[node]\nid = 5\nname = \"x\"\n[sync]\ndelay_ms = \"auto\"\n").unwrap();
        assert_eq!(Config::load_or_create(&path).unwrap().config.sync.delay_ms, DelaySetting::Auto);
        std::fs::write(&path, "[node]\nid = 5\nname = \"x\"\n[sync]\ndelay_ms = 80\n").unwrap();
        assert_eq!(Config::load_or_create(&path).unwrap().config.sync.delay_ms, DelaySetting::Manual(80.0));
    }

    #[test]
    fn bom_unknown_keys_and_bad_values_are_tolerated() {
        let dir = temp_dir("bom");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "\u{FEFF}[node]\nid = 7\nname = \"PC\"\n[network]\nport = 0\nfoo = 1\n[playback]\nvolume = 5.0\nextra_latency_ms = -50000\n",
        )
        .unwrap();
        let out = Config::load_or_create(&path).unwrap();
        assert!(!out.created);
        assert_eq!(out.config.node.id, 7);
        assert_eq!(out.config.network.port, 47810);
        assert_eq!(out.config.playback.volume, 1.0);
        assert_eq!(out.config.playback.extra_latency_ms, -1000.0);
        let all = out.warnings.join("\n");
        for key in ["network.port", "playback.volume", "playback.extra_latency_ms", "network.foo"] {
            assert!(all.contains(key), "no warning about {key}: {all}");
        }
    }

    #[test]
    fn wrongly_typed_section_falls_back_alone() {
        let dir = temp_dir("types");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[node]\nid = 9\nname = \"PC\"\n[network]\nport = \"abc\"\n[playback]\nvolume = 0.5\n").unwrap();
        let out = Config::load_or_create(&path).unwrap();
        assert_eq!(out.config.node.id, 9, "valid sections survive");
        assert_eq!(out.config.playback.volume, 0.5);
        assert_eq!(out.config.network.port, 47810);
        assert!(out.warnings.join("\n").contains("[network]"), "{:?}", out.warnings);
    }

    #[test]
    fn invalid_toml_falls_back_and_backs_up() {
        let dir = temp_dir("invalid");
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is [not toml").unwrap();
        let out = Config::load_or_create(&path).unwrap();
        assert!(out.created);
        assert_ne!(out.config.node.id, 0);
        assert!(!out.warnings.is_empty());
        let backups: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("config.toml.bad-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read_to_string(backups[0].path()).unwrap(), "this is [not toml");
    }

    #[test]
    fn ids_fit_in_toml_integers() {
        // TOML integers are signed 64-bit; an id above i64::MAX would make the file unreadable.
        let mut c = Config::default();
        for _ in 0..2000 {
            c.regenerate_id();
            assert!(c.node.id > 0 && c.node.id <= i64::MAX as u64, "id {} does not fit", c.node.id);
        }
    }

    #[test]
    fn regenerate_id_changes_and_persists() {
        let dir = temp_dir("regen");
        let path = dir.join("config.toml");
        let mut c = Config::load_or_create(&path).unwrap().config;
        let old = c.node.id;
        c.regenerate_id();
        assert_ne!(c.node.id, old);
        assert_ne!(c.node.id, 0);
        c.save(&path).unwrap();
        assert_eq!(Config::load_or_create(&path).unwrap().config.node.id, c.node.id);
    }

    #[test]
    fn paths_with_instance_suffix() {
        let p = Paths::for_instance(Some("b"));
        assert_eq!(p.config_file.file_name().unwrap(), "config.toml");
        assert_eq!(p.config_file.parent().unwrap().file_name().unwrap(), "SoundSync-b");
        assert!(p.log_dir.components().any(|c| c.as_os_str() == "SoundSync-b"));
        assert!(p.state_file.components().any(|c| c.as_os_str() == "SoundSync-b"));
        let default = Paths::for_instance(None);
        assert_eq!(default.config_file.parent().unwrap().file_name().unwrap(), "SoundSync");
    }

    #[test]
    fn group_key_depends_on_group_and_secret() {
        let mut a = Config::default();
        a.network.group = "home".into();
        let mut b = a.clone();
        assert_eq!(a.group_key(), b.group_key());
        b.network.key = "x".into();
        assert_ne!(a.group_key(), b.group_key());
        b.network.key.clear();
        b.network.group = "office".into();
        assert_ne!(a.group_key(), b.group_key());
    }
}
