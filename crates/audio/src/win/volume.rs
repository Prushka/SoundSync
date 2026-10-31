//! Endpoint volume and audio-session queries on render endpoints.

use wasapi::SessionState;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::System::Com::CLSCTX_ALL;

use super::com::os_err;
use super::enumerate::resolve;
use crate::{AudioError, DeviceSelector, Direction};

pub(crate) fn endpoint_volume(sel: &DeviceSelector) -> Result<(f32, bool), AudioError> {
    let (_, device) = resolve(Direction::Render, sel)?;
    let vol: IAudioEndpointVolume =
        unsafe { device.as_immdevice().Activate(CLSCTX_ALL, None) }.map_err(|e| os_err("activate endpoint volume", e))?;
    let level = unsafe { vol.GetMasterVolumeLevelScalar() }.map_err(|e| os_err("get volume", e))?;
    let muted = unsafe { vol.GetMute() }.map_err(|e| os_err("get mute", e))?.as_bool();
    Ok((level, muted))
}

/// Active render sessions on the endpoint that belong to other processes.
pub(crate) fn active_sessions(sel: &DeviceSelector) -> Result<usize, AudioError> {
    let (_, device) = resolve(Direction::Render, sel)?;
    let manager = device.get_iaudiosessionmanager().map_err(|e| os_err("session manager", e))?;
    let sessions = manager.get_audiosessionenumerator().map_err(|e| os_err("session enumerator", e))?;
    let own = std::process::id();
    let count = sessions.get_count().map_err(|e| os_err("session count", e))?;
    Ok((0..count)
        .filter_map(|i| sessions.get_session(i).ok())
        .filter(|s| matches!(s.get_state(), Ok(SessionState::Active)))
        .filter(|s| s.get_process_id().is_ok_and(|pid| pid != own && pid != 0))
        .count())
}
