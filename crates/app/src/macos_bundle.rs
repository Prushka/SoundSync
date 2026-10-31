//! macOS bundle metadata and the login LaunchAgent (pure text builders, testable anywhere).

use std::path::{Path, PathBuf};

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const BUNDLE_ID: &str = "com.soundsync.app";
const INFO_TEMPLATE: &str = include_str!("../../../assets/Info.plist.in");

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Info.plist for SoundSync.app.
pub fn info_plist(version: &str) -> String {
    INFO_TEMPLATE.replace("{version}", &xml_escape(version))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// LaunchAgent that starts the app (through LaunchServices, so macOS ties Local Network and
/// microphone permissions to the app) hidden in the menu bar at login.
pub fn launch_agent_plist(app_bundle: &Path) -> String {
    let app = xml_escape(&app_bundle.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{BUNDLE_ID}</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/bin/open</string>
        <string>-a</string>
        <string>{app}</string>
        <string>--args</string>
        <string>--background</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#
    )
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// `/X/SoundSync.app` for an executable at `/X/SoundSync.app/Contents/MacOS/soundsync`.
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn launch_agent_plist_uses_open_a() {
        let p = launch_agent_plist(Path::new("/Applications/SoundSync.app"));
        assert!(p.contains("<string>com.soundsync.app</string>"));
        assert!(p.contains("<string>/usr/bin/open</string>"));
        assert!(p.contains("<string>-a</string>"));
        assert!(p.contains("<string>/Applications/SoundSync.app</string>"));
        assert!(p.contains("<string>--background</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(!p.contains("MacOS/soundsync"), "the agent must launch the app bundle, not the binary");
    }

    #[test]
    fn launch_agent_plist_escapes_paths() {
        let p = launch_agent_plist(Path::new("/Users/a&b/Apps/<SoundSync>.app"));
        assert!(p.contains("/Users/a&amp;b/Apps/&lt;SoundSync&gt;.app"));
    }

    #[test]
    fn info_plist_contains_required_keys() {
        let p = info_plist("1.2.3");
        for key in [
            "<key>CFBundleIdentifier</key>\n    <string>com.soundsync.app</string>",
            "<key>CFBundleName</key>\n    <string>SoundSync</string>",
            "<key>CFBundleExecutable</key>\n    <string>soundsync</string>",
            "<key>CFBundleShortVersionString</key>\n    <string>1.2.3</string>",
            "<key>LSUIElement</key>\n    <true/>",
            "<key>LSMinimumSystemVersion</key>\n    <string>12.0</string>",
            "<key>NSMicrophoneUsageDescription</key>",
            "<key>NSLocalNetworkUsageDescription</key>",
            "<string>_soundsync._udp</string>",
            "<key>CFBundleIconFile</key>\n    <string>SoundSync</string>",
        ] {
            assert!(p.contains(key), "missing {key}");
        }
        assert!(!p.contains("{version}"));
    }

    #[test]
    fn bundle_path_from_exe() {
        assert_eq!(
            bundle_of(Path::new("/Applications/SoundSync.app/Contents/MacOS/soundsync")).as_deref(),
            Some(Path::new("/Applications/SoundSync.app"))
        );
        assert_eq!(bundle_of(Path::new("/Users/me/dev/target/release/soundsync")), None);
    }
}
