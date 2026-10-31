//! Per-thread COM initialization and QPC time.

use std::cell::Cell;
use std::sync::OnceLock;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::{AudioError, Nanos};

thread_local! {
    static COM_READY: Cell<bool> = const { Cell::new(false) };
}

/// Initializes COM (multithreaded apartment) on the calling thread once. A thread that already
/// joined another apartment (e.g. the UI thread) keeps it; WASAPI works from either.
pub(crate) fn ensure_com() {
    COM_READY.with(|ready| {
        if !ready.get() {
            let _ = wasapi::initialize_mta();
            ready.set(true);
        }
    });
}

fn qpc_frequency() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        unsafe { QueryPerformanceFrequency(&mut f) }.expect("QueryPerformanceFrequency");
        f
    })
}

/// QPC time in nanoseconds. WASAPI timestamps (100 ns units) are on the same clock.
pub fn now_ns() -> Nanos {
    let mut c = 0i64;
    unsafe { QueryPerformanceCounter(&mut c) }.expect("QueryPerformanceCounter");
    (c as i128 * 1_000_000_000 / qpc_frequency() as i128) as Nanos
}

pub(crate) fn os_err(context: &str, e: impl std::fmt::Display) -> AudioError {
    AudioError::Os(format!("{context}: {e}"))
}
