//! Endpoint enumeration (all states, so disconnected devices can still be matched by name).

use wasapi::{Device, DeviceState};
use windows::Win32::Media::Audio::{
    DEVICE_STATE, DEVICE_STATEMASK_ALL, EDataFlow, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};

use super::com::{ensure_com, os_err};
use crate::{AudioError, DeviceInfo, DeviceSelector, Direction, StreamFormat, select_device};

const FALLBACK_FORMAT: StreamFormat = StreamFormat { sample_rate: 48_000, channels: 2, channel_mask: 0x3 };

fn flow(dir: Direction) -> EDataFlow {
    match dir {
        Direction::Render => eRender,
        Direction::Capture => eCapture,
    }
}

pub(crate) fn enumerator() -> Result<IMMDeviceEnumerator, AudioError> {
    ensure_com();
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| os_err("create device enumerator", e))
}

fn to_format(w: &wasapi::WaveFormat) -> StreamFormat {
    StreamFormat { sample_rate: w.get_samplespersec(), channels: w.get_nchannels(), channel_mask: w.get_dwchannelmask() }
}

/// Mix format and (default, minimum) period of an active endpoint.
fn probe(device: &Device) -> Option<(StreamFormat, i64, i64)> {
    let client = device.get_iaudioclient().ok()?;
    let mix = client.get_mixformat().ok()?;
    let (default_hns, min_hns) = client.get_device_period().ok()?;
    Some((to_format(&mix), default_hns * 100, min_hns * 100))
}

pub(crate) fn list(dir: Direction) -> Result<Vec<(DeviceInfo, Device)>, AudioError> {
    let en = enumerator()?;
    let default_id = unsafe { en.GetDefaultAudioEndpoint(flow(dir), eConsole) }
        .ok()
        .and_then(|d| Device::from_immdevice(d).ok())
        .and_then(|d| d.get_id().ok());
    let collection = unsafe { en.EnumAudioEndpoints(flow(dir), DEVICE_STATE(DEVICE_STATEMASK_ALL)) }.map_err(|e| os_err("enumerate endpoints", e))?;
    let count = unsafe { collection.GetCount() }.map_err(|e| os_err("count endpoints", e))?;
    let mut out = Vec::new();
    for i in 0..count {
        let Ok(raw) = (unsafe { collection.Item(i) }) else { continue };
        let Ok(device) = Device::from_immdevice(raw) else { continue };
        let (Ok(id), Ok(name)) = (device.get_id(), device.get_friendlyname()) else { continue };
        let active = matches!(device.get_state(), Ok(DeviceState::Active));
        let (mix_format, default_period_ns, min_period_ns) = if active {
            probe(&device).unwrap_or((FALLBACK_FORMAT, 10_000_000, 10_000_000))
        } else {
            let fmt = device.get_device_format().map(|w| to_format(&w)).unwrap_or(FALLBACK_FORMAT);
            (fmt, 10_000_000, 10_000_000)
        };
        let is_default = default_id.as_deref() == Some(id.as_str());
        out.push((DeviceInfo { id, name, direction: dir, is_default, active, mix_format, default_period_ns, min_period_ns }, device));
    }
    Ok(out)
}

/// Resolves a selector to an active endpoint.
pub(crate) fn resolve(dir: Direction, sel: &DeviceSelector) -> Result<(DeviceInfo, Device), AudioError> {
    let mut all = list(dir)?;
    let infos: Vec<DeviceInfo> = all.iter().map(|(i, _)| i.clone()).collect();
    let chosen = select_device(&infos, sel)?;
    if !chosen.active {
        return Err(AudioError::NotFound(format!("{} (not connected)", chosen.name)));
    }
    let idx = infos.iter().position(|i| i.id == chosen.id).expect("chosen from list");
    Ok(all.swap_remove(idx))
}
