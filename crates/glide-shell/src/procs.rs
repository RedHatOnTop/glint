//! Process inventory, live CPU share, and control for the Task Manager's
//! 프로세스 tab — a Process-Hacker-flavoured view: parent/child tree, owner,
//! thread count, image path, and terminate / kill-tree / suspend-resume /
//! priority. Toolhelp gives names/PIDs/parents/threads for free; psapi the
//! working set; a token+SID lookup the owner; GetProcessTimes vs GetSystemTimes
//! the CPU percentage of total machine capacity.

use core::ffi::c_void;
use std::collections::{HashMap, HashSet};

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, TRUE};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Security::{
    GetTokenInformation, LookupAccountSidW, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetPriorityClass, GetProcessTimes,
    GetSystemTimes, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, OpenProcess,
    OpenProcessToken, PROCESS_CREATION_FLAGS, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION, PROCESS_SUSPEND_RESUME, PROCESS_TERMINATE, QueryFullProcessImageNameW,
    SetPriorityClass, TerminateProcess,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsWindowVisible, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, PCSTR, PWSTR, w};

pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    /// Working-set bytes.
    pub mem: u64,
    /// Share of total machine CPU over the last interval, 0–100.
    pub cpu: f32,
    pub threads: u32,
    /// Short account name that owns the process ("" when unknowable).
    pub user: String,
    /// Priority class as a Korean label.
    pub prio: &'static str,
    /// Full image path ("" when unknowable — System, protected processes).
    pub path: String,
    /// FileDescription from the image's version resource ("Zen Browser",
    /// "Claude Code") — "" when absent; the UI falls back to the exe name.
    pub descr: String,
}

/// The five user-settable priority levels the UI cycles through (realtime is
/// deliberately excluded — it can wedge the machine).
pub const PRIORITIES: [(&str, PROCESS_CREATION_FLAGS); 5] = [
    ("낮음", IDLE_PRIORITY_CLASS),
    ("보통 이하", BELOW_NORMAL_PRIORITY_CLASS),
    ("보통", NORMAL_PRIORITY_CLASS),
    ("보통 이상", ABOVE_NORMAL_PRIORITY_CLASS),
    ("높음", HIGH_PRIORITY_CLASS),
];

/// Holds the previous CPU snapshot and per-PID owner/path caches so a 1.5 s
/// refresh doesn't repeat SID lookups or path queries for existing processes.
pub struct Sampler {
    prev: HashMap<u32, u64>,
    prev_sys: u64,
    users: HashMap<u32, String>,
    paths: HashMap<u32, String>,
    descrs: HashMap<u32, String>,
}

impl Sampler {
    pub fn new() -> Self {
        Sampler {
            prev: HashMap::new(),
            prev_sys: 0,
            users: HashMap::new(),
            paths: HashMap::new(),
            descrs: HashMap::new(),
        }
    }

    pub fn sample(&mut self) -> Vec<Proc> {
        unsafe {
            let (mut idle, mut kern, mut user) =
                (FILETIME::default(), FILETIME::default(), FILETIME::default());
            let _ = GetSystemTimes(Some(&mut idle), Some(&mut kern), Some(&mut user));
            let sys_now = ft(kern) + ft(user);
            let sys_delta = sys_now.saturating_sub(self.prev_sys);
            let first = self.prev_sys == 0;
            self.prev_sys = sys_now;

            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return Vec::new();
            };
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut out = Vec::new();
            let mut cur = HashMap::new();
            let mut alive: Vec<u32> = Vec::new();
            if Process32FirstW(snap, &mut entry).is_ok() {
                loop {
                    let pid = entry.th32ProcessID;
                    if pid != 0 {
                        alive.push(pid);
                        let (mem, cpu_time, prio) = proc_stats(pid);
                        let cpu = if !first && sys_delta > 0 {
                            self.prev
                                .get(&pid)
                                .map(|&pv| cpu_time.saturating_sub(pv) as f64 / sys_delta as f64 * 100.0)
                                .unwrap_or(0.0) as f32
                        } else {
                            0.0
                        };
                        cur.insert(pid, cpu_time);
                        let user = self.users.entry(pid).or_insert_with(|| proc_user(pid)).clone();
                        let path = self.paths.entry(pid).or_insert_with(|| proc_path(pid)).clone();
                        let descr = self
                            .descrs
                            .entry(pid)
                            .or_insert_with(|| if path.is_empty() { String::new() } else { file_description(&path) })
                            .clone();
                        out.push(Proc {
                            pid,
                            ppid: entry.th32ParentProcessID,
                            name: wide_to_string(&entry.szExeFile),
                            mem,
                            cpu,
                            threads: entry.cntThreads,
                            user,
                            prio,
                            path,
                            descr,
                        });
                    }
                    if Process32NextW(snap, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
            // Evict caches for dead PIDs so they can't collide with a reused PID.
            self.users.retain(|k, _| alive.contains(k));
            self.paths.retain(|k, _| alive.contains(k));
            self.descrs.retain(|k, _| alive.contains(k));
            self.prev = cur;
            out.sort_by(|a, b| b.mem.cmp(&a.mem));
            out
        }
    }
}

/// PIDs that own a visible, titled, non-cloaked top-level window — the same
/// heuristic Windows Task Manager uses to lift "apps" above background
/// processes. Cheap enough to re-run each 1.5 s refresh.
pub fn app_pids() -> HashSet<u32> {
    let mut set: HashSet<u32> = HashSet::new();
    unsafe {
        let _ = EnumWindows(Some(app_enum), LPARAM(&mut set as *mut _ as isize));
    }
    set
}

unsafe extern "system" fn app_enum(hwnd: HWND, lp: LPARAM) -> BOOL {
    unsafe {
        let set = &mut *(lp.0 as *mut HashSet<u32>);
        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }
        // Owned windows (dialogs, tool palettes) belong to their owner's app.
        if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
            if !owner.0.is_null() {
                return TRUE;
            }
        }
        if (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW.0 != 0 {
            return TRUE;
        }
        if GetWindowTextLengthW(hwnd) == 0 {
            return TRUE;
        }
        // Skip cloaked windows — background UWP hosts keep an invisible frame.
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut c_void, 4);
        if cloaked != 0 {
            return TRUE;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != 0 {
            set.insert(pid);
        }
        TRUE
    }
}

pub fn terminate(pid: u32) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) else {
            return false;
        };
        let ok = TerminateProcess(h, 1).is_ok();
        let _ = CloseHandle(h);
        ok
    }
}

/// Every descendant PID of `pid` within `all` (depth-first), the parent last —
/// terminating in that order kills children before the root can respawn them.
pub fn descendants(pid: u32, all: &[Proc]) -> Vec<u32> {
    let mut out = Vec::new();
    for c in all.iter().filter(|p| p.ppid == pid && p.pid != pid) {
        out.extend(descendants(c.pid, all));
    }
    out.push(pid);
    out
}

pub fn set_suspended(pid: u32, suspend: bool) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SUSPEND_RESUME, false, pid) else {
            return false;
        };
        let name: &[u8] = if suspend { b"NtSuspendProcess\0" } else { b"NtResumeProcess\0" };
        let ok = nt_proc(name).map(|f| f(h) >= 0).unwrap_or(false);
        let _ = CloseHandle(h);
        ok
    }
}

pub fn set_priority(pid: u32, class: PROCESS_CREATION_FLAGS) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SET_INFORMATION, false, pid) else {
            return false;
        };
        let ok = SetPriorityClass(h, class).is_ok();
        let _ = CloseHandle(h);
        ok
    }
}

/// (working-set bytes, kernel+user CPU time in 100 ns units, priority label).
unsafe fn proc_stats(pid: u32) -> (u64, u64, &'static str) {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return (0, 0, "");
        };
        let mut pmc = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        let mem = if GetProcessMemoryInfo(h, &mut pmc, pmc.cb).is_ok() {
            pmc.WorkingSetSize as u64
        } else {
            0
        };
        let (mut c, mut e, mut k, mut u) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let cpu = if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u).is_ok() {
            ft(k) + ft(u)
        } else {
            0
        };
        let prio = PRIORITIES
            .iter()
            .find(|(_, cls)| cls.0 == GetPriorityClass(h))
            .map(|(l, _)| *l)
            .unwrap_or("");
        let _ = CloseHandle(h);
        (mem, cpu, prio)
    }
}

unsafe fn proc_path(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let path = if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            String::new()
        };
        let _ = CloseHandle(h);
        path
    }
}

/// FileDescription string from the image's version resource — the friendly
/// name Windows Task Manager shows ("Zen Browser", "Claude Code"). Empty when
/// the file has no version info or the query fails.
fn file_description(path: &str) -> String {
    unsafe {
        let wpath: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let size = GetFileVersionInfoSizeW(windows::core::PCWSTR(wpath.as_ptr()), None);
        if size == 0 {
            return String::new();
        }
        let mut buf = vec![0u8; size as usize];
        if GetFileVersionInfoW(
            windows::core::PCWSTR(wpath.as_ptr()),
            None,
            size,
            buf.as_mut_ptr() as *mut c_void,
        )
        .is_err()
        {
            return String::new();
        }
        // Pick the first (lang, codepage) translation, then read its description.
        let mut tr: *mut c_void = std::ptr::null_mut();
        let mut tr_len = 0u32;
        let key_tr: Vec<u16> = "\\VarFileInfo\\Translation"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        if !VerQueryValueW(
            buf.as_ptr() as *const c_void,
            windows::core::PCWSTR(key_tr.as_ptr()),
            &mut tr,
            &mut tr_len,
        )
        .as_bool()
            || tr_len < 4
        {
            return String::new();
        }
        let lang = *(tr as *const u16);
        let cp = *(tr as *const u16).add(1);
        let sub = format!("\\StringFileInfo\\{lang:04x}{cp:04x}\\FileDescription");
        let key: Vec<u16> = sub.encode_utf16().chain(std::iter::once(0)).collect();
        let mut val: *mut c_void = std::ptr::null_mut();
        let mut val_len = 0u32;
        if VerQueryValueW(
            buf.as_ptr() as *const c_void,
            windows::core::PCWSTR(key.as_ptr()),
            &mut val,
            &mut val_len,
        )
        .as_bool()
            && val_len > 0
        {
            let s = std::slice::from_raw_parts(val as *const u16, val_len as usize);
            wide_slice(s).trim().to_string()
        } else {
            String::new()
        }
    }
}

unsafe fn proc_user(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut token = HANDLE::default();
        let opened = OpenProcessToken(h, TOKEN_QUERY, &mut token).is_ok();
        let _ = CloseHandle(h);
        if !opened {
            return String::new();
        }
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        let name = if len > 0 {
            let mut buf = vec![0u8; len as usize];
            if GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr() as *mut c_void), len, &mut len)
                .is_ok()
            {
                let tu = &*(buf.as_ptr() as *const TOKEN_USER);
                sid_name(tu.User.Sid)
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        let _ = CloseHandle(token);
        name
    }
}

unsafe fn sid_name(sid: windows::Win32::Security::PSID) -> String {
    unsafe {
        let mut name = [0u16; 256];
        let mut nlen = name.len() as u32;
        let mut dom = [0u16; 256];
        let mut dlen = dom.len() as u32;
        let mut use_ = SID_NAME_USE::default();
        if LookupAccountSidW(
            windows::core::PCWSTR::null(),
            sid,
            Some(PWSTR(name.as_mut_ptr())),
            &mut nlen,
            Some(PWSTR(dom.as_mut_ptr())),
            &mut dlen,
            &mut use_,
        )
        .is_ok()
        {
            wide_slice(&name)
        } else {
            String::new()
        }
    }
}

type NtProc = unsafe extern "system" fn(HANDLE) -> i32;

unsafe fn nt_proc(name: &[u8]) -> Option<NtProc> {
    unsafe {
        let m = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        GetProcAddress(m, PCSTR(name.as_ptr())).map(|p| std::mem::transmute::<_, NtProc>(p))
    }
}

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

fn wide_to_string(w: &[u16]) -> String {
    wide_slice(w)
}

fn wide_slice(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}
