//! Battery readout + power-scheme switching for the settings 전원 pane.
//! GetSystemPowerStatus (kernel32) plus the powrprof scheme APIs. Switching
//! among *existing* schemes needs no elevation, so this is all HKCU-safe.

use windows::Win32::Foundation::{HLOCAL, LocalFree, WIN32_ERROR};
use windows::Win32::System::Power::{
    ACCESS_SCHEME, GetSystemPowerStatus, PowerEnumerate, PowerGetActiveScheme, PowerReadFriendlyName,
    PowerSetActiveScheme,
};
use windows::core::GUID;

pub struct Battery {
    /// False = desktop / no battery in the system.
    pub present: bool,
    pub percent: u8,
    pub charging: bool,
    /// On wall power.
    pub ac: bool,
}

pub fn battery() -> Battery {
    unsafe {
        let mut sps = Default::default();
        if GetSystemPowerStatus(&mut sps).is_err() {
            return Battery { present: false, percent: 0, charging: false, ac: true };
        }
        // BatteryFlag 128 = no system battery, 255 = unknown; flag 8 = charging.
        let present = sps.BatteryFlag & 128 == 0 && sps.BatteryLifePercent != 255;
        Battery {
            present,
            percent: sps.BatteryLifePercent.min(100),
            charging: sps.BatteryFlag & 8 != 0,
            ac: sps.ACLineStatus == 1,
        }
    }
}

pub struct Plan {
    pub guid: GUID,
    pub name: String,
    pub active: bool,
}

pub fn plans() -> Vec<Plan> {
    unsafe {
        let active = active_scheme();
        let mut out = Vec::new();
        let mut i = 0u32;
        loop {
            let mut guid = GUID::from_u128(0);
            let mut sz = std::mem::size_of::<GUID>() as u32;
            let e = PowerEnumerate(
                None,
                None,
                None,
                ACCESS_SCHEME,
                i,
                Some(&mut guid as *mut _ as *mut u8),
                &mut sz,
            );
            if e != WIN32_ERROR(0) {
                break;
            }
            let name = friendly_name(&guid);
            out.push(Plan { guid, name, active: active == Some(guid) });
            i += 1;
            if i > 32 {
                break; // paranoia against a misbehaving enumerator
            }
        }
        out
    }
}

pub fn set_plan(guid: &GUID) {
    unsafe {
        let _ = PowerSetActiveScheme(None, Some(guid as *const GUID));
    }
}

unsafe fn active_scheme() -> Option<GUID> {
    unsafe {
        let mut p: *mut GUID = std::ptr::null_mut();
        if PowerGetActiveScheme(None, &mut p) != WIN32_ERROR(0) || p.is_null() {
            return None;
        }
        let g = *p;
        let _ = LocalFree(Some(HLOCAL(p as *mut core::ffi::c_void)));
        Some(g)
    }
}

unsafe fn friendly_name(guid: &GUID) -> String {
    unsafe {
        let g = guid as *const GUID;
        let mut sz = 0u32;
        let _ = PowerReadFriendlyName(None, Some(g), None, None, None, &mut sz);
        if sz == 0 {
            return String::new();
        }
        let mut buf = vec![0u8; sz as usize];
        if PowerReadFriendlyName(None, Some(g), None, None, Some(buf.as_mut_ptr()), &mut sz)
            != WIN32_ERROR(0)
        {
            return String::new();
        }
        let u16s: Vec<u16> = buf.chunks_exact(2).map(|c| u16::from_ne_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&u16s).trim_end_matches('\0').to_string()
    }
}
