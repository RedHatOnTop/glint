//! M2 tray — the Shell_TrayWnd protocol (SHELL_DESIGN §6.2).
//!
//! Shell_NotifyIcon marshals across processes as WM_COPYDATA to whatever
//! window has the exact class name "Shell_TrayWnd". Claiming that class while
//! explorer lives races its tray, so claiming sits behind `--tray-claim` and
//! is verified in explorer-kill sessions.
//!
//! Wire format (dwData = 1): SHELLTRAYDATA { magic, NIM_* message, then a
//! NOTIFYICONDATA in the **32-bit layout regardless of sender bitness** —
//! hWnd/hIcon travel as u32. HICONs are USER objects, valid cross-process.

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use crate::taskbar::Bar;

/// NIN_SELECT — v4 icons act on this, not the raw button pair.
pub const NIN_SELECT: u32 = 0x0400;

/// Deliver one Shell_NotifyIcon callback to the icon's owner. The bar strip
/// and the overflow flyout both route clicks through here so a demoted icon
/// behaves identically to a promoted one. v4 packs coords in wParam and
/// (uid, event) in lParam; v0–v3 use (uid, event) directly. We are
/// WS_EX_NOACTIVATE, so hand the owner our foreground right or its own
/// SetForegroundWindow (menus, restore) gets denied.
pub fn forward(owner: HWND, uid: u32, callback: u32, version: u32, event: u32) {
    if callback == 0 {
        return;
    }
    unsafe {
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(owner, Some(&mut pid));
        if pid != 0 {
            let _ = AllowSetForegroundWindow(pid);
        }
        if event == WM_RBUTTONDOWN {
            let _ = SetForegroundWindow(owner);
        }
        let (wparam, lparam) = if version >= 4 {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            (
                WPARAM((((pt.y as u32 as usize) & 0xFFFF) << 16) | (pt.x as u32 as usize & 0xFFFF)),
                LPARAM((((uid as isize) & 0xFFFF) << 16) | (event as isize & 0xFFFF)),
            )
        } else {
            (WPARAM(uid as usize), LPARAM(event as isize))
        };
        let _ = SendNotifyMessageW(owner, callback, wparam, lparam);
    }
}

pub const NIM_ADD: u32 = 0;
pub const NIM_MODIFY: u32 = 1;
pub const NIM_DELETE: u32 = 2;
pub const NIM_SETVERSION: u32 = 4;
pub const NIF_MESSAGE: u32 = 0x01;
pub const NIF_ICON: u32 = 0x02;
pub const NIF_TIP: u32 = 0x04;
pub const NIF_STATE: u32 = 0x08;
pub const NIS_HIDDEN: u32 = 0x01;

// Wire NOTIFYICONDATA (32-bit layout) is parsed by offset in
// parse_tray_data: cbSize@0 hWnd@4 uID@8 uFlags@12 uCallback@16 hIcon@20
// szTip[128]@24 dwState@280 dwStateMask@284 szInfo[256]@288
// uTimeoutOrVersion@800 szInfoTitle[64]@804 dwInfoFlags@932 guid@936.

#[derive(Debug, Clone)]
pub struct TrayEvent {
    pub message: u32,
    pub owner: HWND,
    pub uid: u32,
    pub flags: u32,
    pub callback: u32,
    pub hicon: isize,
    pub state: u32,
    pub state_mask: u32,
    pub version: u32,
    pub tip: String,
}

/// Register + create our Shell_TrayWnd (plus the TrayNotifyWnd child some
/// apps look for), broadcast TaskbarCreated so running apps re-register.
/// `bar_ptr` is stored in GWLP_USERDATA — both windows share the Bar.
pub unsafe fn claim(bar_ptr: *mut Bar) -> anyhow::Result<HWND> {
    unsafe {
        let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
        let class = w!("Shell_TrayWnd");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(tray_wndproc),
            hInstance: hinstance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            anyhow::bail!("RegisterClassW(Shell_TrayWnd) failed — already claimed by us?");
        }
        let tray = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class,
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?;
        SetWindowLongPtrW(tray, GWLP_USERDATA, bar_ptr as isize);

        let notify_class = w!("TrayNotifyWnd");
        let wc2 = WNDCLASSW {
            lpfnWndProc: Some(passthrough_wndproc),
            hInstance: hinstance.into(),
            lpszClassName: notify_class,
            ..Default::default()
        };
        if RegisterClassW(&wc2) != 0 {
            let _ = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                notify_class,
                w!(""),
                WS_CHILD,
                0,
                0,
                0,
                0,
                Some(tray),
                None,
                Some(hinstance.into()),
                None,
            );
        }

        broadcast_taskbar_created();
        Ok(tray)
    }
}

pub fn broadcast_taskbar_created() {
    unsafe {
        let msg = RegisterWindowMessageW(w!("TaskbarCreated"));
        let _ = SendNotifyMessageW(HWND_BROADCAST, msg, WPARAM(0), LPARAM(0));
    }
}

extern "system" fn passthrough_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

extern "system" fn tray_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_COPYDATA {
            let bar_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Bar;
            if !bar_ptr.is_null() && lparam.0 != 0 {
                let cds = &*(lparam.0 as *const COPYDATASTRUCT);
                if cds.dwData == 1 {
                    if let Some(ev) = parse_tray_data(cds) {
                        (*bar_ptr).on_tray_event(ev);
                        return LRESULT(1);
                    }
                }
                // dwData 0 = appbar service, 3 = icon rect query — not yet.
                return LRESULT(0);
            }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

fn parse_tray_data(cds: &COPYDATASTRUCT) -> Option<TrayEvent> {
    // Layout: u32 magic, u32 message, Nid32. Senders with older cbSize send
    // a shorter tail; require at least through `hicon`.
    const HEAD: usize = 8;
    const MIN_NID: usize = 4 * 6; // cb_size..=hicon
    if (cds.cbData as usize) < HEAD + MIN_NID {
        return None;
    }
    unsafe {
        let base = cds.lpData as *const u8;
        let message = std::ptr::read_unaligned(base.add(4) as *const u32);
        let nid = base.add(HEAD);
        let avail = cds.cbData as usize - HEAD;
        let rd_u32 = |off: usize| -> u32 {
            if off + 4 <= avail {
                std::ptr::read_unaligned(nid.add(off) as *const u32)
            } else {
                0
            }
        };
        let hwnd_raw = rd_u32(4);
        let uid = rd_u32(8);
        let flags = rd_u32(12);
        let callback = rd_u32(16);
        let hicon = rd_u32(20);
        let tip_off = 24;
        let mut tip = String::new();
        if flags & NIF_TIP != 0 && tip_off + 128 * 2 <= avail {
            let mut buf = [0u16; 128];
            std::ptr::copy_nonoverlapping(nid.add(tip_off), buf.as_mut_ptr() as *mut u8, 256);
            let len = buf.iter().position(|c| *c == 0).unwrap_or(128);
            tip = String::from_utf16_lossy(&buf[..len]);
        }
        let state_off = tip_off + 128 * 2;
        let state = rd_u32(state_off);
        let state_mask = rd_u32(state_off + 4);
        // timeout/version sits after szInfo[256].
        let version_off = state_off + 8 + 256 * 2;
        let version = rd_u32(version_off);
        Some(TrayEvent {
            message,
            owner: HWND(hwnd_raw as usize as *mut _),
            uid,
            flags,
            callback,
            hicon: hicon as usize as isize,
            state,
            state_mask,
            version,
            tip,
        })
    }
}
