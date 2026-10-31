//! Hand edits of config.toml while SoundSync runs: the engine owns the file and fingerprints what
//! it writes; anything else on disk is an edit, applied by restarting the engine (or reported if
//! it does not parse). Reading never renames or rewrites the file.

use soundsync_core::config::Config;

#[derive(Debug)]
pub enum ConfigChange {
    /// The file is what the engine wrote (or an edit already handled).
    Unchanged,
    /// Someone else changed the file and it parses.
    Edited { config: Box<Config>, warnings: Vec<String>, fingerprint: u64 },
    /// Someone else changed the file and it does not parse.
    Invalid { error: String, fingerprint: u64 },
}

/// Classifies the config file's current `bytes` against the engine's fingerprint and the last
/// edit already handled.
pub fn config_change(bytes: &[u8], engine_fingerprint: u64, handled: Option<u64>) -> ConfigChange {
    let fingerprint = Config::fingerprint(bytes);
    if fingerprint == engine_fingerprint || handled == Some(fingerprint) {
        return ConfigChange::Unchanged;
    }
    match Config::parse(bytes) {
        Ok((config, warnings)) => ConfigChange::Edited { config: Box::new(config), warnings, fingerprint },
        Err(error) => ConfigChange::Invalid { error, fingerprint },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_changes_are_classified() {
        let ours = b"[playback]\nvolume = 0.5\n";
        let engine_fp = Config::fingerprint(ours);
        assert!(matches!(config_change(ours, engine_fp, None), ConfigChange::Unchanged), "the engine's own save");
        let edited = b"[playback]\nvolume = 0.2\n";
        match config_change(edited, engine_fp, None) {
            ConfigChange::Edited { config, fingerprint, .. } => {
                assert_eq!(config.playback.volume, 0.2);
                assert_eq!(fingerprint, Config::fingerprint(edited));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(config_change(edited, engine_fp, Some(Config::fingerprint(edited))), ConfigChange::Unchanged), "handled once");
        assert!(matches!(config_change(b"[playback\n", engine_fp, None), ConfigChange::Invalid { .. }), "a half-typed edit is reported, not replaced");
    }
}
