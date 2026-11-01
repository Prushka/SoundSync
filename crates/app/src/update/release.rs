//! What GitHub says about the latest release: its build number (from the tag
//! `v<version>-build.<n>`), the page with its notes, and this OS's download with its checksum.

use serde::Deserialize;
use soundsync_core::Os;

/// One download of a release.
#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

/// The latest release, as far as this OS is concerned.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub tag: String,
    /// "SoundSync 0.1.0 (build 8)".
    pub title: String,
    pub build: u32,
    /// The release's page (notes and downloads).
    pub page_url: String,
    pub asset: Asset,
}

impl Release {
    /// Builds are numbered by CI run, so a higher number is a later build, whatever the version.
    pub fn newer_than(&self, build: u32) -> bool {
        self.build > build
    }
}

/// The release download for `os` (see `scripts/publish-release.sh`).
pub fn asset_name(os: Os) -> Option<&'static str> {
    match os {
        Os::Windows => Some("SoundSync-windows-x64.zip"),
        Os::MacOs => Some("SoundSync-macos-universal.zip"),
        Os::Other => None,
    }
}

/// 8 for "v0.1.0-build.8".
pub fn build_of_tag(tag: &str) -> Option<u32> {
    let (version, build) = tag.strip_prefix('v')?.rsplit_once("-build.")?;
    if version.is_empty() || build.is_empty() || !build.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    build.parse().ok()
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    name: Option<String>,
    html_url: String,
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    size: u64,
    digest: Option<String>,
    browser_download_url: String,
}

/// "sha256:<64 hex digits>" (GitHub's asset digest).
fn parse_digest(digest: &str) -> Option<[u8; 32]> {
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Reads GitHub's `releases/latest` reply. A release without `asset`, or whose asset has no
/// SHA-256 digest, is refused: an update must be checked before it replaces anything.
pub fn parse_latest(json: &str, asset: &str) -> Result<Release, String> {
    let r: ApiRelease = serde_json::from_str(json).map_err(|e| format!("unexpected reply from GitHub: {e}"))?;
    let build = build_of_tag(&r.tag_name).ok_or_else(|| format!("the latest release ({}) is not a SoundSync build", r.tag_name))?;
    let a = r.assets.into_iter().find(|a| a.name == asset).ok_or_else(|| format!("build {build} has no {asset}"))?;
    let sha256 = a.digest.as_deref().and_then(parse_digest).ok_or_else(|| format!("build {build}'s {asset} has no SHA-256 checksum"))?;
    Ok(Release {
        title: r.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| r.tag_name.clone()),
        tag: r.tag_name,
        build,
        page_url: r.html_url,
        asset: Asset { name: a.name, url: a.browser_download_url, size: a.size, sha256 },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LATEST: &str = include_str!("testdata/latest-build-8.json");

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn build_numbers_come_from_release_tags() {
        assert_eq!(build_of_tag("v0.1.0-build.8"), Some(8));
        assert_eq!(build_of_tag("v2.3.0-build.120"), Some(120));
        for junk in ["v0.1.0", "build.8", "v0.1.0-build.x", "v0.1.0-build.", ""] {
            assert_eq!(build_of_tag(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn the_latest_release_names_its_asset_and_checksum() {
        let r = parse_latest(LATEST, "SoundSync-windows-x64.zip").unwrap();
        assert_eq!(r.build, 8);
        assert_eq!(r.tag, "v0.1.0-build.8");
        assert_eq!(r.title, "SoundSync 0.1.0 (build 8)");
        assert_eq!(r.page_url, "https://github.com/Prushka/SoundSync/releases/tag/v0.1.0-build.8");
        assert_eq!(r.asset.name, "SoundSync-windows-x64.zip");
        assert_eq!(r.asset.size, 7_782_375);
        assert_eq!(hex(&r.asset.sha256), "6a98aa5e00336c52df74af59a8e3627fe87e6626634e906a644741a2508a04f4");
        assert!(r.asset.url.starts_with("https://github.com/") && r.asset.url.ends_with("/SoundSync-windows-x64.zip"), "{}", r.asset.url);
        assert_eq!(parse_latest(LATEST, "SoundSync-macos-universal.zip").unwrap().asset.size, 13_522_724);
    }

    #[test]
    fn releases_without_our_asset_or_checksum_are_refused() {
        assert!(parse_latest(LATEST, "SoundSync-linux.zip").is_err());
        let no_digest = LATEST.replace("\"digest\": \"sha256:6a98aa5e00336c52df74af59a8e3627fe87e6626634e906a644741a2508a04f4\",", "");
        assert!(parse_latest(&no_digest, "SoundSync-windows-x64.zip").is_err(), "an asset without a checksum can't be verified");
        let short = LATEST.replace("sha256:6a98aa5e00336c52df74af59a8e3627fe87e6626634e906a644741a2508a04f4", "sha256:6a98aa5e");
        assert!(parse_latest(&short, "SoundSync-windows-x64.zip").is_err());
        let md5 = LATEST.replace("sha256:6a98aa5e00336c52df74af59a8e3627fe87e6626634e906a644741a2508a04f4", "md5:6a98aa5e00336c52df74af59a8e3627f");
        assert!(parse_latest(&md5, "SoundSync-windows-x64.zip").is_err());
        let untagged = LATEST.replace("\"tag_name\": \"v0.1.0-build.8\"", "\"tag_name\": \"nightly\"");
        assert!(parse_latest(&untagged, "SoundSync-windows-x64.zip").unwrap_err().contains("not a SoundSync build"));
        assert!(parse_latest("not json", "SoundSync-windows-x64.zip").is_err());
    }

    #[test]
    fn only_higher_builds_are_newer() {
        let r = parse_latest(LATEST, "SoundSync-windows-x64.zip").unwrap();
        assert!(r.newer_than(7));
        assert!(!r.newer_than(8));
        assert!(!r.newer_than(9));
    }

    #[test]
    fn each_os_has_its_asset() {
        assert_eq!(asset_name(Os::Windows), Some("SoundSync-windows-x64.zip"));
        assert_eq!(asset_name(Os::MacOs), Some("SoundSync-macos-universal.zip"));
        assert_eq!(asset_name(Os::Other), None);
    }
}
