//! Reading and setting the Windows default output device.
//!
//! Windows has no documented API for changing the default endpoint; every audio switcher
//! (EarTrumpet, SoundSwitch, AudioDeviceCmdlets) uses the undocumented `IPolicyConfig`, whose
//! vtable has been stable since Windows 7. Only `SetDefaultEndpoint` is called; the slots before
//! it are placeholders that must never be invoked.

#![allow(non_snake_case)]

use windows::Win32::Media::Audio::{ERole, eCommunications, eConsole, eMultimedia, eRender};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::{GUID, HRESULT, HSTRING, IUnknown, IUnknown_Vtbl, PCWSTR, interface};

use super::com::{ensure_com, os_err};
use super::enumerate::enumerator;
use crate::{AudioError, DefaultDevices, DefaultRole};

#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: IUnknown {
    fn GetMixFormat(&self) -> HRESULT;
    fn GetDeviceFormat(&self) -> HRESULT;
    fn ResetDeviceFormat(&self) -> HRESULT;
    fn SetDeviceFormat(&self) -> HRESULT;
    fn GetProcessingPeriod(&self) -> HRESULT;
    fn SetProcessingPeriod(&self) -> HRESULT;
    fn GetShareMode(&self) -> HRESULT;
    fn SetShareMode(&self) -> HRESULT;
    fn GetPropertyValue(&self) -> HRESULT;
    fn SetPropertyValue(&self) -> HRESULT;
    fn SetDefaultEndpoint(&self, device_id: PCWSTR, role: ERole) -> HRESULT;
}

const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

fn erole(r: DefaultRole) -> ERole {
    match r {
        DefaultRole::Console => eConsole,
        DefaultRole::Multimedia => eMultimedia,
        DefaultRole::Communications => eCommunications,
    }
}

fn default_id(role: ERole) -> Result<String, AudioError> {
    let en = enumerator()?;
    let device = unsafe { en.GetDefaultAudioEndpoint(eRender, role) }.map_err(|e| os_err("get default endpoint", e))?;
    let id = unsafe { device.GetId() }.map_err(|e| os_err("endpoint id", e))?;
    let s = unsafe { id.to_string() }.map_err(|e| os_err("endpoint id", e));
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(id.0 as *const _)) };
    s
}

/// Current default render endpoint ids for each role.
pub fn read_defaults() -> Result<DefaultDevices, AudioError> {
    ensure_com();
    Ok(DefaultDevices { console: default_id(eConsole)?, multimedia: default_id(eMultimedia)?, communications: default_id(eCommunications)? })
}

/// Makes `id` the default render endpoint for `role`. Changes a user-visible Windows setting.
pub fn set_default_render(id: &str, role: DefaultRole) -> Result<(), AudioError> {
    ensure_com();
    let policy: IPolicyConfig = unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) }.map_err(|e| os_err("policy config", e))?;
    let id = HSTRING::from(id);
    unsafe { policy.SetDefaultEndpoint(PCWSTR(id.as_ptr()), erole(role)) }.ok().map_err(|e| os_err("set default endpoint", e))
}

/// True if a render endpoint with this id exists and is active.
pub fn endpoint_active(id: &str) -> bool {
    super::enumerate::list(crate::Direction::Render).map(|l| l.iter().any(|(d, _)| d.id == id && d.active)).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "reads (never writes) this PC's default devices"]
    fn read_defaults_returns_ids() {
        let d = read_defaults().unwrap();
        for id in [&d.console, &d.multimedia, &d.communications] {
            assert!(id.starts_with("{0.0.0."), "render endpoint id: {id}");
            assert!(endpoint_active(id));
        }
    }
}
