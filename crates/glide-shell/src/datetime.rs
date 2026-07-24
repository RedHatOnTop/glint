//! Date/time readout + time-zone switching for the 날짜·시간 pane. Changing the
//! zone needs only SE_TIME_ZONE_NAME — a privilege standard users already hold —
//! so we enable it in-process and call SetDynamicTimeZoneInformation. Setting
//! the clock itself or toggling NTP sync needs admin and stays in timedate.cpl.

use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::Time::{
    DYNAMIC_TIME_ZONE_INFORMATION, EnumDynamicTimeZoneInformation, GetDynamicTimeZoneInformation,
    SetDynamicTimeZoneInformation,
};
use windows::core::w;
use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};

const TZ_ROOT: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Time Zones";

pub struct Zone {
    pub key: String,
    /// Friendly "(UTC+09:00) 서울" label from the zone's registry Display value.
    pub display: String,
    pub current: bool,
    info: DYNAMIC_TIME_ZONE_INFORMATION,
}

/// (date, time) as display strings for the current local clock.
pub fn now() -> (String, String) {
    let st = unsafe { GetLocalTime() };
    let wd = ["일", "월", "화", "수", "목", "금", "토"]
        .get(st.wDayOfWeek as usize)
        .copied()
        .unwrap_or("");
    (
        format!("{:04}-{:02}-{:02} ({wd})", st.wYear, st.wMonth, st.wDay),
        format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond),
    )
}

/// Every installed time zone, current one flagged, ordered UTC-12 → UTC+14.
pub fn zones() -> Vec<Zone> {
    let cur = current_key();
    let mut out: Vec<Zone> = Vec::new();
    let mut i = 0u32;
    loop {
        let mut dtzi = DYNAMIC_TIME_ZONE_INFORMATION::default();
        // Returns ERROR_SUCCESS (0) while zones remain, ERROR_NO_MORE_ITEMS at end.
        if unsafe { EnumDynamicTimeZoneInformation(i, &mut dtzi) } != 0 {
            break;
        }
        i += 1;
        if i > 400 {
            break; // guard against a runaway enumerator
        }
        let key = wide_to_string(&dtzi.TimeZoneKeyName);
        if key.is_empty() {
            continue;
        }
        let display = zone_display(&key).unwrap_or_else(|| wide_to_string(&dtzi.StandardName));
        let current = key.eq_ignore_ascii_case(&cur);
        out.push(Zone { key, display, current, info: dtzi });
    }
    // Bias = UTC − local (minutes); descending walks the globe west → east.
    out.sort_by(|a, b| b.info.Bias.cmp(&a.info.Bias).then(a.display.cmp(&b.display)));
    out
}

/// Switch to `z`. Returns false if the change was rejected (e.g. privilege
/// unexpectedly withheld by policy).
pub fn set_zone(z: &Zone) -> bool {
    unsafe {
        enable_time_zone_privilege();
        SetDynamicTimeZoneInformation(&z.info).is_ok()
    }
}

fn current_key() -> String {
    let mut dtzi = DYNAMIC_TIME_ZONE_INFORMATION::default();
    unsafe {
        let _ = GetDynamicTimeZoneInformation(&mut dtzi);
    }
    wide_to_string(&dtzi.TimeZoneKeyName)
}

fn zone_display(key: &str) -> Option<String> {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(format!(r"{TZ_ROOT}\{key}"), KEY_READ)
        .ok()?
        .get_value("Display")
        .ok()
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

unsafe fn enable_time_zone_privilege() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token)
            .is_err()
        {
            return false;
        }
        let mut luid = LUID::default();
        let ok = if LookupPrivilegeValueW(None, w!("SeTimeZonePrivilege"), &mut luid).is_ok() {
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None).is_ok()
        } else {
            false
        };
        let _ = CloseHandle(token);
        ok
    }
}
