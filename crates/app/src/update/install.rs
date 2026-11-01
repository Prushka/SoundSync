//! Puts a downloaded build in place of the running copy. The new copy is unpacked and started once
//! (`--version`) in `.soundsync-update/new/` next to it, then the two trade places by renaming:
//! a running exe can't be overwritten but can be moved. The old copy waits in
//! `.soundsync-update/old/` until the new one deletes the folder at its start.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use soundsync_core::Os;

use crate::macos_bundle::bundle_of;

/// Next to the running copy (same volume, so moving is a rename).
pub const STAGING: &str = ".soundsync-update";

pub const DEV_BUILD: &str = "This is a development build; it doesn't update itself.";
const NOT_A_BUNDLE: &str = "Only SoundSync.app updates itself.";
const TRANSLOCATED: &str = "Move SoundSync.app to Applications and open it from there to update it.";
/// Windows Defender may scan a new exe before it first runs.
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);

/// What gets replaced: the exe (Windows) or the app bundle (macOS).
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Exe(PathBuf),
    Bundle(PathBuf),
}

impl Target {
    pub fn path(&self) -> &Path {
        match self {
            Target::Exe(p) | Target::Bundle(p) => p,
        }
    }

    fn folder(&self) -> &Path {
        self.path().parent().unwrap_or(Path::new("."))
    }

    fn file_name(&self) -> &OsStr {
        self.path().file_name().unwrap_or(OsStr::new("SoundSync"))
    }

    pub fn staging(&self) -> PathBuf {
        self.folder().join(STAGING)
    }

    /// The program to start for `--version` inside a copy at `copy`.
    fn program_in(&self, copy: &Path) -> PathBuf {
        match self {
            Target::Exe(_) => copy.to_path_buf(),
            Target::Bundle(_) => copy.join("Contents").join("MacOS").join("soundsync"),
        }
    }
}

/// A new build unpacked and checked, ready to trade places with the running copy.
#[derive(Clone, Debug, PartialEq)]
pub struct Staged {
    pub target: Target,
    pub new: PathBuf,
}

/// The new build is in place; the old copy waits at `old`.
#[derive(Debug)]
pub struct Swapped {
    pub target: Target,
    pub old: PathBuf,
}

/// Which copy an executable at `exe` would update, or why it can't.
pub fn target_for(exe: &Path, build: Option<&str>, os: Os) -> Result<Target, String> {
    if build.is_none_or(|b| b.trim().is_empty()) {
        return Err(DEV_BUILD.into());
    }
    match os {
        Os::Windows => Ok(Target::Exe(exe.to_path_buf())),
        // A quarantined app opened from Downloads runs from a read-only copy elsewhere.
        Os::MacOs if exe.to_string_lossy().contains("/AppTranslocation/") => Err(TRANSLOCATED.into()),
        Os::MacOs => bundle_of(exe).map(Target::Bundle).ok_or_else(|| NOT_A_BUNDLE.into()),
        Os::Other => Err("SoundSync updates itself only on Windows and macOS.".into()),
    }
}

/// The running copy, or why it can't update itself.
pub fn target() -> Result<Target, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot tell where SoundSync runs from: {e}"))?;
    target_for(&exe, soundsync_core::version::build_number(), Os::current())
}

/// Whether `soundsync --version` printed this build ("… (build 9, abc1234)" or "… (build 9)").
pub fn reports_build(version_output: &str, build: u32) -> bool {
    let tag = format!("(build {build}");
    version_output.match_indices(&tag).any(|(i, _)| matches!(version_output[i + tag.len()..].chars().next(), Some(',' | ')')))
}

/// Writes the release zip's `soundsync.exe` to `dest`.
pub fn extract_exe(zip: &[u8], dest: &Path) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).map_err(|e| format!("the download is not a readable zip: {e}"))?;
    let mut exe = archive.by_name("soundsync.exe").map_err(|_| "the release zip has no soundsync.exe".to_string())?;
    let mut file = std::fs::File::create(dest).map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
    std::io::copy(&mut exe, &mut file).map_err(|e| format!("cannot unpack soundsync.exe: {e}"))?;
    Ok(())
}

/// Runs `program` with `args`, quietly, and returns its output; gives up after `timeout`.
fn run(program: &Path, args: &[&str], timeout: Duration) -> Result<std::process::Output, String> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flashing up
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot start {}: {e}", program.display()))?;
    let started = Instant::now();
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if started.elapsed() > timeout {
            let _ = child.kill();
            return Err(format!("{} did not finish within {} s", program.display(), timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().map_err(|e| e.to_string())
}

/// Unpacks the release zip next to the running copy and checks that the new build starts and is
/// build `build`. Leaves nothing behind when it fails.
pub fn stage(target: &Target, zip: &[u8], build: u32) -> Result<Staged, String> {
    let staging = target.staging();
    let new_dir = staging.join("new");
    let _ = std::fs::remove_dir_all(&new_dir);
    std::fs::create_dir_all(&new_dir)
        .map_err(|e| format!("SoundSync can't write to {} ({e}). Download build {build} from the release page instead.", target.folder().display()))?;
    let staged = unpack(target, zip, &staging, &new_dir).and_then(|new| {
        let out = run(&target.program_in(&new), &["--version"], VERSION_TIMEOUT)?;
        let printed = String::from_utf8_lossy(&out.stdout);
        if !reports_build(&printed, build) {
            return Err(format!("the downloaded program does not start as build {build} (it said {:?})", printed.trim()));
        }
        Ok(Staged { target: target.clone(), new })
    });
    if staged.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    staged
}

/// The new copy, unpacked into `new_dir`.
fn unpack(target: &Target, zip: &[u8], staging: &Path, new_dir: &Path) -> Result<PathBuf, String> {
    match target {
        Target::Exe(_) => {
            let new = new_dir.join(target.file_name());
            extract_exe(zip, &new)?;
            Ok(new)
        }
        Target::Bundle(_) => {
            // ditto keeps the bundle's symlinks, permissions and signature (the zip was made by it).
            let file = staging.join("update.zip");
            std::fs::write(&file, zip).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
            let out = run(Path::new("/usr/bin/ditto"), &["-x", "-k", &file.to_string_lossy(), &new_dir.to_string_lossy()], VERSION_TIMEOUT)?;
            if !out.status.success() {
                return Err(format!("cannot unpack the update: {}", String::from_utf8_lossy(&out.stderr).trim()));
            }
            let new = new_dir.join("SoundSync.app");
            let out = run(Path::new("/usr/bin/codesign"), &["--verify", "--deep", "--strict", &new.to_string_lossy()], VERSION_TIMEOUT)?;
            if !out.status.success() {
                return Err(format!("the downloaded SoundSync.app is damaged: {}", String::from_utf8_lossy(&out.stderr).trim()));
            }
            Ok(new)
        }
    }
}

/// Trades places: the running copy moves to `.soundsync-update/old/`, the staged one into its
/// place. If the second step fails the first is undone, so the running copy stays usable.
pub fn swap(staged: &Staged) -> Result<Swapped, String> {
    let target = staged.target.path();
    let old_dir = staged.target.staging().join("old");
    let _ = std::fs::remove_dir_all(&old_dir);
    std::fs::create_dir_all(&old_dir).map_err(|e| format!("cannot write {}: {e}", old_dir.display()))?;
    let old = old_dir.join(staged.target.file_name());
    std::fs::rename(target, &old).map_err(|e| format!("cannot move {} aside: {e}", target.display()))?;
    if let Err(e) = std::fs::rename(&staged.new, target) {
        let _ = std::fs::rename(&old, target);
        return Err(format!("cannot put the new build in place of {}: {e}", target.display()));
    }
    Ok(Swapped { target: staged.target.clone(), old })
}

impl Swapped {
    /// Puts the old copy back (the new one goes back to the staging folder).
    pub fn undo(&self) -> Result<(), String> {
        let target = self.target.path();
        let back = self.target.staging().join("new").join(self.target.file_name());
        let _ = std::fs::create_dir_all(back.parent().unwrap_or(Path::new(".")));
        let _ = std::fs::remove_dir_all(&back);
        std::fs::rename(target, &back).map_err(|e| format!("cannot move the new build away from {}: {e}", target.display()))?;
        std::fs::rename(&self.old, target).map_err(|e| format!("cannot put the old build back at {}: {e}", target.display()))
    }
}

/// The arguments to start the new copy with: this run's, without `--show`/`--background`, plus
/// whatever brings the window back the way it was (Windows starts hidden unless `--show`; macOS
/// opens the window unless `--background`).
pub fn relaunch_args(args: &[OsString], visible: bool, os: Os) -> Vec<OsString> {
    let mut out: Vec<OsString> = args.iter().filter(|a| *a != "--show" && *a != "--background").cloned().collect();
    match (os, visible) {
        (Os::MacOs, false) => out.push("--background".into()),
        (Os::MacOs, true) => {}
        (_, true) => out.push("--show".into()),
        (_, false) => {}
    }
    out
}

/// Starts the copy at `target` with `args`. On macOS through LaunchServices (`open -n`), as the
/// login item does, so its permissions stay tied to the app.
pub fn launch(target: &Target, args: &[OsString]) -> std::io::Result<()> {
    match target {
        Target::Exe(exe) => {
            let mut cmd = Command::new(exe);
            cmd.args(args).current_dir(target.folder()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            cmd.spawn().map(drop)
        }
        Target::Bundle(bundle) => {
            let status = Command::new("/usr/bin/open").arg("-n").arg(bundle).arg("--args").args(args).status()?;
            if status.success() { Ok(()) } else { Err(std::io::Error::other(format!("open exited with {status}"))) }
        }
    }
}

/// Whether the previous copy was just replaced (it waits in `.soundsync-update/old/`; a swap that
/// was rolled back leaves that folder empty).
pub fn was_just_updated(target: &Target) -> bool {
    target.staging().join("old").join(target.file_name()).exists()
}

/// Deletes `.soundsync-update`, retrying while the old copy is still exiting (its exe stays
/// locked until then). False if it is still there after `give_up_after`.
pub fn clean_up(target: &Target, give_up_after: Duration) -> bool {
    let staging = target.staging();
    let started = Instant::now();
    loop {
        match std::fs::remove_dir_all(&staging) {
            Ok(()) => return true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) if started.elapsed() >= give_up_after => return false,
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A fresh, empty folder for one test.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("soundsync-install-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, bytes) in files {
            w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(bytes).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn dev_builds_and_loose_mac_binaries_do_not_update() {
        let win = Path::new(r"C:\x\soundsync.exe");
        assert_eq!(target_for(win, None, Os::Windows).unwrap_err(), "This is a development build; it doesn't update itself.");
        assert_eq!(target_for(win, Some(""), Os::Windows).unwrap_err(), "This is a development build; it doesn't update itself.");
        assert_eq!(target_for(win, Some("8"), Os::Windows), Ok(Target::Exe(win.to_path_buf())));
        let loose = Path::new("/Users/x/bin/soundsync");
        assert_eq!(target_for(loose, Some("8"), Os::MacOs).unwrap_err(), "Only SoundSync.app updates itself.");
        let translocated = Path::new("/private/var/folders/a/AppTranslocation/B/d/SoundSync.app/Contents/MacOS/soundsync");
        assert_eq!(target_for(translocated, Some("8"), Os::MacOs).unwrap_err(), "Move SoundSync.app to Applications and open it from there to update it.");
        let installed = Path::new("/Applications/SoundSync.app/Contents/MacOS/soundsync");
        assert_eq!(target_for(installed, Some("8"), Os::MacOs), Ok(Target::Bundle(PathBuf::from("/Applications/SoundSync.app"))));
    }

    #[test]
    fn the_new_binary_must_report_the_build() {
        assert!(reports_build("soundsync 0.1.0 (build 9, abc1234)\n", 9));
        assert!(reports_build("soundsync 0.1.0 (build 9)\n", 9));
        assert!(!reports_build("soundsync 0.1.0 (build 90, abc1234)\n", 9));
        assert!(!reports_build("soundsync 0.1.0 (build 8, abc1234)\n", 9));
        assert!(!reports_build("soundsync 0.1.0 (dev)\n", 9));
    }

    #[test]
    fn the_exe_is_taken_from_the_release_zip() {
        let dir = temp("extract");
        let zip = zip_of(&[("README.md", b"# SoundSync"), ("soundsync.exe", b"MZ new build")]);
        extract_exe(&zip, &dir.join("soundsync.exe")).unwrap();
        assert_eq!(std::fs::read(dir.join("soundsync.exe")).unwrap(), b"MZ new build");
        let e = extract_exe(&zip_of(&[("README.md", b"# SoundSync")]), &dir.join("other.exe")).unwrap_err();
        assert!(e.contains("soundsync.exe"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn swapping_moves_the_old_copy_aside_and_undo_puts_it_back() {
        let dir = temp("swap");
        let exe = dir.join("soundsync.exe");
        std::fs::write(&exe, "old").unwrap();
        let target = Target::Exe(exe.clone());
        let new = target.staging().join("new").join("soundsync.exe");
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::write(&new, "new").unwrap();
        let swapped = swap(&Staged { target: target.clone(), new: new.clone() }).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(swapped.old, target.staging().join("old").join("soundsync.exe"));
        assert_eq!(std::fs::read_to_string(&swapped.old).unwrap(), "old");
        swapped.undo().unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old", "undo brings the old copy back");
        // The new copy went missing: the swap fails and the old copy stays where it was.
        let _ = std::fs::remove_dir_all(target.staging());
        let e = swap(&Staged { target, new }).unwrap_err();
        assert!(!e.is_empty());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_swap_that_was_undone_or_failed_is_not_reported_as_an_update() {
        let dir = temp("undone");
        let exe = dir.join("soundsync.exe");
        std::fs::write(&exe, "old").unwrap();
        let target = Target::Exe(exe.clone());
        let new = target.staging().join("new").join("soundsync.exe");
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::write(&new, "new").unwrap();
        let swapped = swap(&Staged { target: target.clone(), new: new.clone() }).unwrap();
        assert!(was_just_updated(&target), "the old copy waits in old/");
        swapped.undo().unwrap(); // the new build could not be started
        assert!(!was_just_updated(&target), "the old copy is back: nothing was updated");
        std::fs::remove_file(&new).unwrap();
        assert!(swap(&Staged { target: target.clone(), new }).is_err());
        assert!(!was_just_updated(&target), "a swap that was rolled back is not an update");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relaunch_keeps_the_window_hidden_when_it_was() {
        let started = args(&["--instance", "b", "--show", "--bind", "127.0.0.1"]);
        assert_eq!(relaunch_args(&started, false, Os::Windows), args(&["--instance", "b", "--bind", "127.0.0.1"]));
        assert_eq!(relaunch_args(&started, true, Os::Windows), args(&["--instance", "b", "--bind", "127.0.0.1", "--show"]));
        assert_eq!(relaunch_args(&started, false, Os::MacOs), args(&["--instance", "b", "--bind", "127.0.0.1", "--background"]));
        assert_eq!(relaunch_args(&started, true, Os::MacOs), args(&["--instance", "b", "--bind", "127.0.0.1"]));
        assert_eq!(relaunch_args(&args(&["--background"]), true, Os::MacOs), args(&[]), "the login item's --background goes once the window is open");
    }

    #[test]
    fn leftovers_without_an_old_copy_are_cleaned_quietly() {
        let dir = temp("leftovers");
        let target = Target::Exe(dir.join("soundsync.exe"));
        std::fs::create_dir_all(target.staging().join("new")).unwrap();
        std::fs::write(target.staging().join("new").join("x"), "half a download").unwrap();
        assert!(!was_just_updated(&target));
        assert!(clean_up(&target, Duration::from_secs(1)));
        assert!(!target.staging().exists());
        std::fs::create_dir_all(target.staging().join("old")).unwrap();
        std::fs::write(target.staging().join("old").join("soundsync.exe"), "the copy that was replaced").unwrap();
        assert!(was_just_updated(&target));
        assert!(clean_up(&target, Duration::from_secs(1)));
        assert!(clean_up(&target, Duration::from_secs(1)), "nothing to clean is fine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn staging_where_nothing_can_be_written_says_so() {
        let dir = temp("readonly");
        let not_a_folder = dir.join("file");
        std::fs::write(&not_a_folder, "").unwrap();
        let target = Target::Exe(not_a_folder.join("soundsync.exe"));
        let e = stage(&target, &zip_of(&[("soundsync.exe", b"MZ")]), 9).unwrap_err();
        assert!(e.starts_with("SoundSync can't write to"), "{e}");
        assert!(e.contains("Download build 9 from the release page instead."), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_staging_leaves_nothing_behind() {
        let dir = temp("failed");
        let target = if cfg!(target_os = "macos") { Target::Bundle(dir.join("SoundSync.app")) } else { Target::Exe(dir.join("soundsync.exe")) };
        assert!(stage(&target, &zip_of(&[("README.md", b"no program in here")]), 9).is_err());
        assert!(!target.staging().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
