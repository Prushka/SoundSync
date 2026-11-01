//! Endpoint volume and audio-session queries on render endpoints.

use wasapi::SessionState;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::System::Com::CLSCTX_ALL;

use super::com::os_err;
use super::enumerate::resolve;
use crate::{AudioError, DeviceSelector, Direction, EndpointVolume};

fn endpoint(sel: &DeviceSelector) -> Result<IAudioEndpointVolume, AudioError> {
    let (_, device) = resolve(Direction::Render, sel)?;
    unsafe { device.as_immdevice().Activate(CLSCTX_ALL, None) }.map_err(|e| os_err("activate endpoint volume", e))
}

pub(crate) fn endpoint_volume(sel: &DeviceSelector) -> Result<EndpointVolume, AudioError> {
    let vol = endpoint(sel)?;
    let scalar = unsafe { vol.GetMasterVolumeLevelScalar() }.map_err(|e| os_err("get volume", e))?;
    let db = unsafe { vol.GetMasterVolumeLevel() }.map_err(|e| os_err("get volume level", e))?;
    let muted = unsafe { vol.GetMute() }.map_err(|e| os_err("get mute", e))?.as_bool();
    Ok(EndpointVolume { scalar, db, muted })
}

pub(crate) fn set_endpoint_volume(sel: &DeviceSelector, scalar: f32) -> Result<(), AudioError> {
    let vol = endpoint(sel)?;
    unsafe { vol.SetMasterVolumeLevelScalar(scalar.clamp(0.0, 1.0), std::ptr::null()) }.map_err(|e| os_err("set volume", e))
}

pub(crate) fn set_endpoint_volume_db(sel: &DeviceSelector, db: f32) -> Result<(), AudioError> {
    let vol = endpoint(sel)?;
    let (mut min, mut max, mut step) = (0f32, 0f32, 0f32);
    unsafe { vol.GetVolumeRange(&mut min, &mut max, &mut step) }.map_err(|e| os_err("get volume range", e))?;
    unsafe { vol.SetMasterVolumeLevel(db.clamp(min, max), std::ptr::null()) }.map_err(|e| os_err("set volume level", e))
}

pub(crate) fn set_endpoint_mute(sel: &DeviceSelector, muted: bool) -> Result<(), AudioError> {
    let vol = endpoint(sel)?;
    unsafe { vol.SetMute(muted, std::ptr::null()) }.map_err(|e| os_err("set mute", e))
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
