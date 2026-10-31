//! Startup-folder shortcut via IShellLinkW / IPersistFile.

use std::path::{Path, PathBuf};

use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile, STGM_READ};
use windows::Win32::UI::Shell::{FOLDERID_Startup, IShellLinkW, KNOWN_FOLDER_FLAG, SHGetKnownFolderPath, ShellLink};
use windows::core::{HSTRING, Interface, PCWSTR};

use super::AutostartError;

pub(crate) const SHORTCUT_NAME: &str = "SoundSync.lnk";

fn os(e: windows::core::Error) -> AutostartError {
    AutostartError::Os(e.to_string())
}

fn com() {
    // Shell links work in either apartment; keep whatever this thread already joined.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
}

pub(crate) fn startup_dir() -> Result<PathBuf, AutostartError> {
    let p = unsafe { SHGetKnownFolderPath(&FOLDERID_Startup, KNOWN_FOLDER_FLAG(0), None) }.map_err(os)?;
    let path = unsafe { p.to_string() }.map_err(|e| AutostartError::Os(e.to_string()));
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _)) };
    Ok(PathBuf::from(path?))
}

pub(crate) fn read_target(lnk: &Path) -> Result<String, AutostartError> {
    com();
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }.map_err(os)?;
    let file: IPersistFile = link.cast().map_err(os)?;
    unsafe { file.Load(&HSTRING::from(lnk.as_os_str()), STGM_READ) }.map_err(os)?;
    let mut buf = [0u16; 1024];
    unsafe { link.GetPath(&mut buf, std::ptr::null_mut(), 0) }.map_err(os)?;
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..len]))
}

/// Writes a shortcut to `target` with no arguments (SoundSync then starts hidden in the tray).
pub(crate) fn write_shortcut(lnk: &Path, target: &Path) -> Result<(), AutostartError> {
    com();
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }.map_err(os)?;
    unsafe {
        link.SetPath(&HSTRING::from(target.as_os_str())).map_err(os)?;
        if let Some(dir) = target.parent() {
            link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str())).map_err(os)?;
        }
        link.SetDescription(&HSTRING::from("SoundSync: synchronized multi-room audio")).map_err(os)?;
        link.SetIconLocation(&HSTRING::from(target.as_os_str()), 0).map_err(os)?;
    }
    let file: IPersistFile = link.cast().map_err(os)?;
    unsafe { file.Save(PCWSTR(HSTRING::from(lnk.as_os_str()).as_ptr()), true) }.map_err(os)?;
    Ok(())
}

fn same_path(a: &str, b: &Path) -> bool {
    a.eq_ignore_ascii_case(&b.to_string_lossy())
}

pub(crate) fn is_enabled_in(startup_dir: &Path, exe: &Path) -> Result<bool, AutostartError> {
    let lnk = startup_dir.join(SHORTCUT_NAME);
    if !lnk.exists() {
        return Ok(false);
    }
    Ok(same_path(&read_target(&lnk)?, exe))
}

pub(crate) fn set_enabled_in(startup_dir: &Path, exe: &Path, enabled: bool) -> Result<(), AutostartError> {
    let lnk = startup_dir.join(SHORTCUT_NAME);
    if lnk.exists() {
        let target = read_target(&lnk)?;
        let ours = same_path(&target, exe);
        let stale = !Path::new(&target).exists();
        if !ours && !stale {
            // Someone else's shortcut: never replace it, and never delete it.
            return if enabled { Err(AutostartError::ForeignShortcut(lnk)) } else { Ok(()) };
        }
        if !enabled {
            std::fs::remove_file(&lnk)?;
            return Ok(());
        }
    }
    if enabled {
        std::fs::create_dir_all(startup_dir)?;
        write_shortcut(&lnk, exe)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("soundsync-autostart-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn exe() -> PathBuf {
        std::env::current_exe().unwrap()
    }

    fn cmd_exe() -> PathBuf {
        PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join("System32").join("cmd.exe")
    }

    #[test]
    fn create_detect_and_remove_shortcut_in_temp_dir() {
        let d = temp_dir("roundtrip");
        assert!(!is_enabled_in(&d, &exe()).unwrap());
        set_enabled_in(&d, &exe(), true).unwrap();
        assert!(d.join(SHORTCUT_NAME).exists());
        assert!(is_enabled_in(&d, &exe()).unwrap());
        assert_eq!(read_target(&d.join(SHORTCUT_NAME)).unwrap().to_lowercase(), exe().to_string_lossy().to_lowercase());
        set_enabled_in(&d, &exe(), false).unwrap();
        assert!(!d.join(SHORTCUT_NAME).exists());
        assert!(!is_enabled_in(&d, &exe()).unwrap());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn refuses_foreign_shortcut() {
        let d = temp_dir("foreign");
        write_shortcut(&d.join(SHORTCUT_NAME), &cmd_exe()).unwrap();
        match set_enabled_in(&d, &exe(), true) {
            Err(AutostartError::ForeignShortcut(p)) => assert!(p.ends_with(SHORTCUT_NAME)),
            other => panic!("expected ForeignShortcut, got {other:?}"),
        }
        assert!(!is_enabled_in(&d, &exe()).unwrap(), "a shortcut to another program doesn't count");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn disable_leaves_foreign_shortcut() {
        let d = temp_dir("foreign-keep");
        write_shortcut(&d.join(SHORTCUT_NAME), &cmd_exe()).unwrap();
        set_enabled_in(&d, &exe(), false).unwrap();
        assert!(d.join(SHORTCUT_NAME).exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn overwrites_stale_shortcut() {
        let d = temp_dir("stale");
        write_shortcut(&d.join(SHORTCUT_NAME), &d.join("gone").join("soundsync.exe")).unwrap();
        set_enabled_in(&d, &exe(), true).unwrap();
        assert!(is_enabled_in(&d, &exe()).unwrap());
        let _ = std::fs::remove_dir_all(d);
    }
}
