//! Windows session end (shutdown, restart, sign-out). winit does not handle WM_ENDSESSION, so a
//! hidden top-level window on its own thread hears it and runs the handlers before Windows ends
//! the process — used to put the default output devices back.

use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, RegisterClassW, TranslateMessage, WINDOW_EX_STYLE,
    WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::w;

type Handler = Arc<dyn Fn() + Send + Sync>;

static HANDLERS: Mutex<Vec<Handler>> = Mutex::new(Vec::new());

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                let handlers: Vec<Handler> = HANDLERS.lock().map(|h| h.clone()).unwrap_or_default();
                for h in handlers {
                    h();
                }
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Runs `on_end` when the Windows session ends. Returns the (hidden) window handle.
pub fn watch(on_end: impl Fn() + Send + Sync + 'static) -> std::io::Result<isize> {
    HANDLERS.lock().map_err(|_| std::io::Error::other("session-end handlers poisoned"))?.push(Arc::new(on_end));
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new().name("soundsync-session".into()).spawn(move || unsafe {
        let instance = match GetModuleHandleW(None) {
            Ok(m) => m.into(),
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        let class = w!("SoundSyncSessionEnd");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: instance, lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc); // fails harmlessly if a previous watcher registered it
        // A top-level window that is never shown: unlike message-only windows it gets
        // WM_QUERYENDSESSION / WM_ENDSESSION.
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("SoundSync session"), WS_OVERLAPPED, 0, 0, 0, 0, None, None, Some(instance), None);
        let ok = hwnd.is_ok();
        let _ = tx.send(hwnd.map(|h| h.0 as isize));
        if !ok {
            return;
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    })?;
    rx.recv().map_err(|_| std::io::Error::other("session watcher thread died"))?.map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_ENDSESSION};

    #[test]
    fn session_end_runs_the_handler() {
        let ended = Arc::new(AtomicUsize::new(0));
        let e = ended.clone();
        let hwnd = watch(move || {
            e.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        let hwnd = HWND(hwnd as *mut _);
        unsafe { SendMessageW(hwnd, WM_ENDSESSION, Some(WPARAM(0)), Some(LPARAM(0))) };
        assert_eq!(ended.load(Ordering::SeqCst), 0, "a cancelled shutdown is not an end");
        unsafe { SendMessageW(hwnd, WM_ENDSESSION, Some(WPARAM(1)), Some(LPARAM(0))) };
        assert_eq!(ended.load(Ordering::SeqCst), 1);
    }
}
