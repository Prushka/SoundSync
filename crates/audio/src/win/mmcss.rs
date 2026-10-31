//! MMCSS "Pro Audio" scheduling for audio threads.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW};
use windows::core::w;

/// Registers the current thread with MMCSS for the lifetime of the guard (best effort).
pub(crate) struct ProAudioGuard(Option<HANDLE>);

impl ProAudioGuard {
    pub(crate) fn new() -> Self {
        let mut index = 0u32;
        ProAudioGuard(unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index) }.ok())
    }
}

impl Drop for ProAudioGuard {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
        }
    }
}
