//! Default-endpoint switching via the undocumented IPolicyConfig COM
//! interface — the route every audio switcher (EarTrumpet, SoundSwitch,
//! nircmd) takes, because there is no documented API for it. Only
//! SetDefaultEndpoint is ever called; the earlier vtable slots are declared
//! purely to keep its offset right, with pointer-sized placeholders where
//! the real argument types don't matter to us.

#![allow(non_snake_case)] // COM vtable slot names keep the header's casing

use core::ffi::c_void;

use windows::Win32::Media::Audio::{ERole, eCommunications, eConsole, eMultimedia};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::{GUID, HRESULT, PCWSTR, interface};

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
