//! Thin wlanapi wrapper for the network flyout: available-network list,
//! software radio toggle, connect-by-profile. First WLAN interface only —
//! this box has one, and multi-adapter UI is not a v1 concern.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::WiFi::*;
use windows::core::{GUID, PCWSTR};

pub struct Wifi {
    handle: HANDLE,
    iface: GUID,
}

#[derive(Clone, PartialEq)]
pub struct Net {
    pub ssid: String,
    /// wlanSignalQuality, 0-100.
    pub signal: u32,
    pub secured: bool,
    pub connected: bool,
    /// Saved profile name — present means we can connect without a password UI.
    pub profile: Option<String>,
}

impl Wifi {
    pub fn open() -> Option<Wifi> {
        unsafe {
            let mut ver = 0u32;
            let mut handle = HANDLE::default();
            if WlanOpenHandle(2, None, &mut ver, &mut handle) != 0 {
                return None;
            }
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            if WlanEnumInterfaces(handle, None, &mut list) != 0 || list.is_null() {
                let _ = WlanCloseHandle(handle, None);
                return None;
            }
            let n = (*list).dwNumberOfItems;
            let iface = (n > 0).then(|| (*list).InterfaceInfo[0].InterfaceGuid);
            WlanFreeMemory(list as _);
            match iface {
                Some(iface) => Some(Wifi { handle, iface }),
                None => {
                    let _ = WlanCloseHandle(handle, None);
                    None
                }
            }
        }
    }

    /// Fire an async scan; results land in the cache `networks()` reads.
    pub fn scan(&self) {
        unsafe {
            let _ = WlanScan(self.handle, &self.iface, None, None, None);
        }
    }

    /// Cached available networks, deduped by SSID (an SSID appears once per
    /// BSS type / profile in the raw list), connected-first then by signal.
    pub fn networks(&self) -> Vec<Net> {
        unsafe {
            let mut list: *mut WLAN_AVAILABLE_NETWORK_LIST = std::ptr::null_mut();
            if WlanGetAvailableNetworkList(self.handle, &self.iface, 0, None, &mut list) != 0
                || list.is_null()
            {
                return Vec::new();
            }
            let items = std::slice::from_raw_parts(
                (*list).Network.as_ptr(),
                (*list).dwNumberOfItems as usize,
            );
            let mut nets: Vec<Net> = Vec::new();
            for n in items {
                let ssid_bytes = &n.dot11Ssid.ucSSID[..(n.dot11Ssid.uSSIDLength as usize).min(32)];
                let ssid = String::from_utf8_lossy(ssid_bytes).into_owned();
                if ssid.trim_matches('\0').is_empty() {
                    continue;
                }
                let profile = if n.dwFlags & WLAN_AVAILABLE_NETWORK_HAS_PROFILE != 0 {
                    let name = String::from_utf16_lossy(&n.strProfileName);
                    let name = name.trim_end_matches('\0');
                    (!name.is_empty()).then(|| name.to_string())
                } else {
                    None
                };
                let connected = n.dwFlags & WLAN_AVAILABLE_NETWORK_CONNECTED != 0;
                match nets.iter_mut().find(|m| m.ssid == ssid) {
                    Some(m) => {
                        m.signal = m.signal.max(n.wlanSignalQuality);
                        m.connected |= connected;
                        if m.profile.is_none() {
                            m.profile = profile;
                        }
                    }
                    None => nets.push(Net {
                        ssid,
                        signal: n.wlanSignalQuality,
                        secured: n.bSecurityEnabled.as_bool(),
                        connected,
                        profile,
                    }),
                }
            }
            WlanFreeMemory(list as _);
            nets.sort_by(|a, b| b.connected.cmp(&a.connected).then(b.signal.cmp(&a.signal)));
            nets.truncate(8);
            nets
        }
    }

    /// Software radio state — the thing the flyout toggle flips. Hardware
    /// kill switches show up as "off" here too, which is the honest display.
    pub fn radio_on(&self) -> bool {
        unsafe {
            let mut size = 0u32;
            let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
            if WlanQueryInterface(
                self.handle,
                &self.iface,
                wlan_intf_opcode_radio_state,
                None,
                &mut size,
                &mut data,
                None,
            ) != 0
                || data.is_null()
            {
                return true;
            }
            let rs = &*(data as *const WLAN_RADIO_STATE);
            let phys = (rs.dwNumberOfPhys as usize).min(rs.PhyRadioState.len());
            let on = rs.PhyRadioState[..phys].iter().any(|p| {
                p.dot11SoftwareRadioState == dot11_radio_state_on
                    && p.dot11HardwareRadioState == dot11_radio_state_on
            });
            WlanFreeMemory(data);
            on
        }
    }

    pub fn set_radio(&self, on: bool) {
        unsafe {
            let mut size = 0u32;
            let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
            if WlanQueryInterface(
                self.handle,
                &self.iface,
                wlan_intf_opcode_radio_state,
                None,
                &mut size,
                &mut data,
                None,
            ) != 0
                || data.is_null()
            {
                return;
            }
            let rs = *(data as *const WLAN_RADIO_STATE);
            WlanFreeMemory(data);
            let state = if on { dot11_radio_state_on } else { dot11_radio_state_off };
            let phys = (rs.dwNumberOfPhys as usize).min(rs.PhyRadioState.len());
            for i in 0..phys {
                let set = WLAN_PHY_RADIO_STATE {
                    dwPhyIndex: rs.PhyRadioState[i].dwPhyIndex,
                    dot11SoftwareRadioState: state,
                    dot11HardwareRadioState: rs.PhyRadioState[i].dot11HardwareRadioState,
                };
                let _ = WlanSetInterface(
                    self.handle,
                    &self.iface,
                    wlan_intf_opcode_radio_state,
                    std::mem::size_of::<WLAN_PHY_RADIO_STATE>() as u32,
                    &set as *const _ as _,
                    None,
                );
            }
        }
    }

    /// Connect to a saved profile. Unknown networks go to ms-settings —
    /// a password UI is not v1 scope.
    pub fn connect(&self, profile: &str) {
        unsafe {
            let wide: Vec<u16> = profile.encode_utf16().chain(std::iter::once(0)).collect();
            let params = WLAN_CONNECTION_PARAMETERS {
                wlanConnectionMode: wlan_connection_mode_profile,
                strProfile: PCWSTR(wide.as_ptr()),
                pDot11Ssid: std::ptr::null_mut(),
                pDesiredBssidList: std::ptr::null_mut(),
                dot11BssType: dot11_BSS_type_infrastructure,
                dwFlags: 0,
            };
            let _ = WlanConnect(self.handle, &self.iface, &params, None);
        }
    }
}

impl Drop for Wifi {
    fn drop(&mut self) {
        unsafe {
            let _ = WlanCloseHandle(self.handle, None);
        }
    }
}
