//! Host time: `mach_absolute_time` in nanoseconds (the clock CoreAudio's `mHostTime` uses).

use std::sync::OnceLock;

use crate::Nanos;

// libc marks the mach bindings deprecated in favour of the mach2 crate; they are still the
// documented system calls.
#[allow(deprecated)]
fn timebase() -> (u64, u64) {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        unsafe {
            libc::mach_timebase_info(&mut info);
        }
        (info.numer.max(1) as u64, info.denom.max(1) as u64)
    })
}

/// Converts a CoreAudio host time (mach ticks) to nanoseconds.
pub fn host_ns(ticks: u64) -> Nanos {
    let (numer, denom) = timebase();
    (ticks as u128 * numer as u128 / denom as u128) as Nanos
}

pub fn now_ns() -> Nanos {
    #[allow(deprecated)]
    let t = unsafe { libc::mach_absolute_time() };
    host_ns(t)
}
