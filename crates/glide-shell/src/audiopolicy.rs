//! Default-endpoint switching via the undocumented IPolicyConfig COM
//! interface — the route every audio switcher (EarTrumpet, SoundSwitch,
//! nircmd) takes, because there is no documented API for it. Only
//! SetDefaultEndpoint is ever called; the earlier vtable slots are declared
//! purely to keep its offset right, with pointer-sized placeholders where
//! the real argument types don't matter to us.

#![allow(non_snake_case)] // COM vtable slot names keep the header's casing

use core::ffi::c_void;

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, ERole, IMMDeviceEnumerator, MMDeviceEnumerator, eCommunications, eConsole,
    eMultimedia, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::core::{GUID, HRESULT, PCWSTR, PWSTR, interface};

const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: windows::core::IUnknown {
    unsafe fn GetMixFormat(&self, device: PCWSTR, fmt: *mut *mut c_void) -> HRESULT;
    unsafe fn GetDeviceFormat(&self, device: PCWSTR, default: i32, fmt: *mut *mut c_void)
    -> HRESULT;
    unsafe fn ResetDeviceFormat(&self, device: PCWSTR) -> HRESULT;
    unsafe fn SetDeviceFormat(
        &self,
        device: PCWSTR,
        endpoint: *mut c_void,
        mix: *mut c_void,
    ) -> HRESULT;
    unsafe fn GetProcessingPeriod(
        &self,
        device: PCWSTR,
        default: i32,
        def_period: *mut i64,
        min_period: *mut i64,
    ) -> HRESULT;
    unsafe fn SetProcessingPeriod(&self, device: PCWSTR, period: *mut i64) -> HRESULT;
    unsafe fn GetShareMode(&self, device: PCWSTR, mode: *mut c_void) -> HRESULT;
    unsafe fn SetShareMode(&self, device: PCWSTR, mode: *mut c_void) -> HRESULT;
    unsafe fn GetPropertyValue(
        &self,
        device: PCWSTR,
        fx: i32,
        key: *const c_void,
        value: *mut c_void,
    ) -> HRESULT;
    unsafe fn SetPropertyValue(
        &self,
        device: PCWSTR,
        fx: i32,
        key: *const c_void,
        value: *const c_void,
    ) -> HRESULT;
    unsafe fn SetDefaultEndpoint(&self, device: PCWSTR, role: ERole) -> HRESULT;
    unsafe fn SetEndpointVisibility(&self, device: PCWSTR, visible: i32) -> HRESULT;
}

/// Make `id` (a null-terminated MMDevice endpoint ID) the default for all
/// three roles, like the stock sound flyout does.
pub fn set_default_endpoint(id: &[u16]) -> windows::core::Result<()> {
    unsafe {
        let pc: IPolicyConfig = CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL)?;
        for role in [eConsole, eMultimedia, eCommunications] {
            pc.SetDefaultEndpoint(PCWSTR(id.as_ptr()), role).ok()?;
        }
    }
    Ok(())
}

/// A rendering endpoint the sound pane lists.
pub struct Endpoint {
    /// Null-terminated MMDevice ID, ready to feed [`set_default_endpoint`].
    pub id: Vec<u16>,
    pub name: String,
    pub default: bool,
}

/// Every active render endpoint, with the current default flagged.
pub fn list_render() -> Vec<Endpoint> {
    unsafe {
        let Ok(en): windows::core::Result<IMMDeviceEnumerator> =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return Vec::new();
        };
        let default_id = en
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .ok()
            .and_then(|d| d.GetId().ok())
            .map(|p| pwstr_owned(p))
            .unwrap_or_default();
        let Ok(coll) = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
            return Vec::new();
        };
        let n = coll.GetCount().unwrap_or(0);
        let mut out = Vec::new();
        for i in 0..n {
            let Ok(dev) = coll.Item(i) else { continue };
            let Ok(idp) = dev.GetId() else { continue };
            let id = pwstr_owned(idp);
            // PKEY_Device_FriendlyName is reliably VT_LPWSTR; the PROPVARIANT
            // frees its own string on drop, so borrow pwszVal before it goes.
            let name = dev
                .OpenPropertyStore(STGM_READ)
                .ok()
                .and_then(|st| st.GetValue(&PKEY_Device_FriendlyName).ok())
                .map(|pv| {
                    let p = pv.Anonymous.Anonymous.Anonymous.pwszVal;
                    if p.is_null() { String::new() } else { p.to_string().unwrap_or_default() }
                })
                .unwrap_or_default();
            let default = !default_id.is_empty() && id == default_id;
            out.push(Endpoint { id, name, default });
        }
        out
    }
}

/// Master volume (0.0–1.0) and mute state of the default render endpoint.
pub fn volume() -> Option<(f32, bool)> {
    unsafe {
        let ep = default_volume_iface()?;
        match (ep.GetMasterVolumeLevelScalar(), ep.GetMute()) {
            (Ok(v), Ok(m)) => Some((v, m.as_bool())),
            _ => None,
        }
    }
}

pub fn set_volume(v: f32) {
    unsafe {
        if let Some(ep) = default_volume_iface() {
            let _ = ep.SetMasterVolumeLevelScalar(v.clamp(0.0, 1.0), std::ptr::null());
        }
    }
}

pub fn set_mute(m: bool) {
    unsafe {
        if let Some(ep) = default_volume_iface() {
            let _ = ep.SetMute(m, std::ptr::null());
        }
    }
}

unsafe fn default_volume_iface() -> Option<IAudioEndpointVolume> {
    unsafe {
        let en: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        dev.Activate(CLSCTX_ALL, None).ok()
    }
}

/// Copy a COM-allocated wide string into an owned, null-terminated buffer and
/// free the original.
unsafe fn pwstr_owned(p: PWSTR) -> Vec<u16> {
    unsafe {
        if p.is_null() {
            return Vec::new();
        }
        let mut len = 0usize;
        while *p.0.add(len) != 0 {
            len += 1;
        }
        let v = std::slice::from_raw_parts(p.0, len + 1).to_vec();
        CoTaskMemFree(Some(p.0 as *const c_void));
        v
    }
}
