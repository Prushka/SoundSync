//! One SoundSync per config file. On Windows a named mutex marks the running instance and named
//! events let a second launch bring up its window (`.Show`) or quit it (`.Quit`). Elsewhere a
//! lock file prevents a second copy (macOS LaunchServices reactivates the running app itself).

use std::path::Path;
use std::sync::{Arc, Mutex};

type Callback = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

pub enum Instance {
    Primary(InstanceGuard),
    Secondary,
}

/// FNV-1a 64 of the lowercased path, so one config file = one instance.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn instance_name(config_path: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in config_path.to_string_lossy().to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("Local\\SoundSync.{h:016x}")
}

pub struct InstanceGuard {
    on_show: Callback,
    on_quit: Callback,
    #[cfg(windows)]
    _win: win::Handles,
    #[cfg(not(windows))]
    _lock: std::fs::File,
}

impl InstanceGuard {
    pub fn on_show(&self, f: impl Fn() + Send + 'static) {
        *self.on_show.lock().unwrap() = Some(Box::new(f));
    }

    pub fn on_quit(&self, f: impl Fn() + Send + 'static) {
        *self.on_quit.lock().unwrap() = Some(Box::new(f));
    }
}

#[cfg(windows)]
mod win {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForMultipleObjects,
    };
    use windows::core::HSTRING;

    use super::Callback;

    pub(super) struct Handles {
        mutex: HANDLE,
        show: HANDLE,
        quit: HANDLE,
        stop: HANDLE,
        waiter: Option<std::thread::JoinHandle<()>>,
    }

    // SAFETY: kernel handles may be used from any thread.
    unsafe impl Send for Handles {}

    struct SendHandles([HANDLE; 3]);
    unsafe impl Send for SendHandles {}

    impl Drop for Handles {
        fn drop(&mut self) {
            unsafe {
                let _ = SetEvent(self.stop);
            }
            if let Some(t) = self.waiter.take() {
                let _ = t.join();
            }
            unsafe {
                for h in [self.show, self.quit, self.stop, self.mutex] {
                    let _ = CloseHandle(h);
                }
            }
        }
    }

    /// Returns None if another instance owns the mutex.
    pub(super) fn acquire(name: &str, on_show: Callback, on_quit: Callback) -> anyhow::Result<Option<Handles>> {
        let mutex = unsafe { CreateMutexW(None, true, &HSTRING::from(name))? };
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                let _ = CloseHandle(mutex);
            }
            return Ok(None);
        }
        let show = unsafe { CreateEventW(None, false, false, &HSTRING::from(format!("{name}.Show")))? };
        let quit = unsafe { CreateEventW(None, false, false, &HSTRING::from(format!("{name}.Quit")))? };
        let stop = unsafe { CreateEventW(None, true, false, None)? };
        let handles = SendHandles([show, quit, stop]);
        let waiter = std::thread::Builder::new().name("soundsync-instance".into()).spawn(move || {
            let handles = handles;
            loop {
                let r = unsafe { WaitForMultipleObjects(&handles.0, false, INFINITE) };
                let callback = match r.0.wrapping_sub(WAIT_OBJECT_0.0) {
                    0 => &on_show,
                    1 => &on_quit,
                    _ => return,
                };
                if let Some(f) = callback.lock().unwrap().as_ref() {
                    f();
                }
            }
        })?;
        Ok(Some(Handles { mutex, show, quit, stop, waiter: Some(waiter) }))
    }

    pub(super) fn signal(name: &str) -> bool {
        match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(name)) } {
            Ok(h) => unsafe {
                let ok = SetEvent(h).is_ok();
                let _ = CloseHandle(h);
                ok
            },
            Err(_) => false,
        }
    }
}

pub fn acquire(config_path: &Path) -> anyhow::Result<Instance> {
    let on_show: Callback = Arc::default();
    let on_quit: Callback = Arc::default();
    #[cfg(windows)]
    {
        match win::acquire(&instance_name(config_path), on_show.clone(), on_quit.clone())? {
            Some(h) => Ok(Instance::Primary(InstanceGuard { on_show, on_quit, _win: h })),
            None => Ok(Instance::Secondary),
        }
    }
    #[cfg(not(windows))]
    {
        let lock_path = config_path.with_file_name("soundsync.lock");
        if let Some(dir) = lock_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path)?;
        match file.try_lock() {
            Ok(()) => Ok(Instance::Primary(InstanceGuard { on_show, on_quit, _lock: file })),
            Err(_) => Ok(Instance::Secondary),
        }
    }
}

/// Asks the running instance to show its window. False if none is running.
pub fn signal_show(config_path: &Path) -> bool {
    #[cfg(windows)]
    {
        win::signal(&format!("{}.Show", instance_name(config_path)))
    }
    #[cfg(not(windows))]
    {
        let _ = config_path;
        false
    }
}

/// Asks the running instance to quit. False if none is running.
pub fn signal_quit(config_path: &Path) -> bool {
    #[cfg(windows)]
    {
        win::signal(&format!("{}.Quit", instance_name(config_path)))
    }
    #[cfg(not(windows))]
    {
        let _ = config_path;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use std::sync::Arc;
    #[cfg(windows)]
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn instance_name_hashes_lowercased_path() {
        let a = instance_name(Path::new(r"C:\Users\Dan\AppData\Roaming\SoundSync\config.toml"));
        let b = instance_name(Path::new(r"c:\users\dan\appdata\roaming\soundsync\config.toml"));
        assert_eq!(a, b);
        assert!(a.starts_with(r"Local\SoundSync."), "{a}");
        assert_eq!(a.len(), r"Local\SoundSync.".len() + 16);
        assert_ne!(a, instance_name(Path::new(r"C:\other\config.toml")));
    }

    #[cfg(windows)]
    #[test]
    fn second_acquire_is_secondary_and_signals_show() {
        let path = std::env::temp_dir().join(format!("soundsync-si-{}.toml", std::process::id()));
        let Instance::Primary(guard) = acquire(&path).unwrap() else { panic!("first acquire must be primary") };
        let shown = Arc::new(AtomicUsize::new(0));
        let s = shown.clone();
        guard.on_show(move || {
            s.fetch_add(1, Ordering::SeqCst);
        });
        assert!(matches!(acquire(&path).unwrap(), Instance::Secondary));
        assert!(signal_show(&path));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while shown.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(shown.load(Ordering::SeqCst), 1);
        drop(guard);
        assert!(!signal_show(&path), "no running instance after the guard is dropped");
    }
}
