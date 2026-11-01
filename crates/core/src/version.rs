//! The version shown to people: the crate version plus, for builds made by CI, the build number
//! and commit (`SOUNDSYNC_BUILD` and `SOUNDSYNC_COMMIT` at compile time), e.g.
//! "0.1.0 (build 42, fcaec3d)". Local builds say "(dev)".

use std::sync::LazyLock;

/// The release version (`version` in the workspace's Cargo.toml).
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

fn set(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|v| !v.is_empty())
}

/// The CI build number, if CI built this binary.
pub fn build_number_of(env: Option<&str>) -> Option<&str> {
    set(env)
}

/// "0.1.0 (build 42, fcaec3d)", "0.1.0 (build 42)" or "0.1.0 (dev)".
pub fn version_label(version: &str, build: Option<&str>, commit: Option<&str>) -> String {
    match (set(build), set(commit)) {
        (Some(b), Some(c)) => format!("{version} (build {b}, {c})"),
        (Some(b), None) => format!("{version} (build {b})"),
        (None, _) => format!("{version} (dev)"),
    }
}

/// This binary's build number, if CI built it.
pub fn build_number() -> Option<&'static str> {
    build_number_of(option_env!("SOUNDSYNC_BUILD"))
}

/// This binary's version label.
pub fn version() -> &'static str {
    static LABEL: LazyLock<String> = LazyLock::new(|| version_label(PACKAGE_VERSION, option_env!("SOUNDSYNC_BUILD"), option_env!("SOUNDSYNC_COMMIT")));
    &LABEL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_label_names_the_ci_build_or_says_dev() {
        assert_eq!(version_label("0.1.0", Some("42"), Some("fcaec3d")), "0.1.0 (build 42, fcaec3d)");
        assert_eq!(version_label("0.1.0", Some("42"), None), "0.1.0 (build 42)");
        assert_eq!(version_label("0.1.0", None, Some("fcaec3d")), "0.1.0 (dev)");
        assert_eq!(version_label("0.1.0", Some(""), Some("")), "0.1.0 (dev)", "set but empty counts as unset");
        assert_eq!(build_number_of(Some(" 42 ")), Some("42"));
        assert_eq!(build_number_of(Some("")), None);
    }
}
