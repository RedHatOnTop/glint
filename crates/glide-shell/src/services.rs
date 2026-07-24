//! Service Control Manager list + start/stop for the Task Manager 서비스 tab.
//! Enumeration needs only read rights (standard user); starting/stopping a
//! system service usually needs admin, so control returns a bool the UI surfaces
//! instead of pretending it always worked.

use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, ENUM_SERVICE_STATUS_PROCESSW, EnumServicesStatusExW,
    OpenSCManagerW, OpenServiceW, SC_ENUM_PROCESS_INFO, SC_MANAGER_CONNECT,
    SC_MANAGER_ENUMERATE_SERVICE, SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
    SERVICE_START, SERVICE_STATE_ALL, SERVICE_STATUS, SERVICE_STOP, SERVICE_WIN32, StartServiceW,
};
use windows::core::PCWSTR;

pub struct Svc {
    /// Service key name — the handle used for control.
    pub name: String,
    pub display: String,
    pub running: bool,
}

pub fn list() -> Vec<Svc> {
    unsafe {
        let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)
        else {
            return Vec::new();
        };
        let mut needed = 0u32;
        let mut count = 0u32;
        let mut resume = 0u32;
        // First call sizes the buffer (fails with ERROR_MORE_DATA).
        let _ = EnumServicesStatusExW(
            scm,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_STATE_ALL,
            None,
            &mut needed,
            &mut count,
            Some(&mut resume),
            PCWSTR::null(),
        );
        let mut out = Vec::new();
        if needed > 0 {
            let mut buf = vec![0u8; needed as usize];
            resume = 0;
            if EnumServicesStatusExW(
                scm,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                Some(&mut buf),
                &mut needed,
                &mut count,
                Some(&mut resume),
                PCWSTR::null(),
            )
            .is_ok()
            {
                let arr = buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW;
                for i in 0..count as usize {
                    let e = &*arr.add(i);
                    out.push(Svc {
                        name: e.lpServiceName.to_string().unwrap_or_default(),
                        display: e.lpDisplayName.to_string().unwrap_or_default(),
                        running: e.ServiceStatusProcess.dwCurrentState == SERVICE_RUNNING,
                    });
                }
            }
        }
        let _ = CloseServiceHandle(scm);
        out.sort_by(|a, b| a.display.to_lowercase().cmp(&b.display.to_lowercase()));
        out
    }
}

/// Start or stop `name`. Returns false on failure (commonly access-denied
/// without elevation), which the UI reports rather than swallowing.
pub fn set_running(name: &str, run: bool) -> bool {
    unsafe {
        let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_CONNECT) else {
            return false;
        };
        let wname: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let access = (if run { SERVICE_START } else { SERVICE_STOP }) | SERVICE_QUERY_STATUS;
        let ok = match OpenServiceW(scm, PCWSTR(wname.as_ptr()), access) {
            Ok(svc) => {
                let r = if run {
                    StartServiceW(svc, None).is_ok()
                } else {
                    let mut st = SERVICE_STATUS::default();
                    ControlService(svc, SERVICE_CONTROL_STOP, &mut st).is_ok()
                };
                let _ = CloseServiceHandle(svc);
                r
            }
            Err(_) => false,
        };
        let _ = CloseServiceHandle(scm);
        ok
    }
}
