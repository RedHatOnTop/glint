//! Dismiss-on-click-away for the bar popups (start menu, status flyouts).
//!
//! WA_INACTIVE dismissal only works when the popup actually became the
//! active window. A popup opened from the Win-key hook can't take
//! activation (this process never received the input, so the foreground
//! lock rejects SetForegroundWindow), and then nothing ever tells it to
//! go away. A WH_MOUSE_LL hook closes the gap: any button-down anywhere
//! on screen while a popup is up gets posted to the bar, which decides
//! whether the click was "away" (see `Bar::click_away`).
//!
//! Same rule as the keyboard hook: the callback does no work beyond one
//! PostMessage, and only while a popup is actually open.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, MSLLHOOKSTRUCT, PostMessageW, SetWindowsHookExW, WH_MOUSE_LL, WM_APP,
    WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_RBUTTONDOWN, WM_XBUTTONDOWN,
};

/// Posted to the bar on any button-down while a popup is open.
/// wparam = screen x, lparam = screen y (both signed, multi-monitor).
pub const WM_CLICKAWAY: u32 = WM_APP + 15;

static BAR: AtomicIsize = AtomicIsize::new(0);
static START_OPEN: AtomicBool = AtomicBool::new(false);
static FLYOUT_OPEN: AtomicBool = AtomicBool::new(false);
static AC_OPEN: AtomicBool = AtomicBool::new(false);
static OVERFLOW_OPEN: AtomicBool = AtomicBool::new(false);

pub fn set_start_open(v: bool) {
    START_OPEN.store(v, Ordering::Relaxed);
}

pub fn set_flyout_open(v: bool) {
    FLYOUT_OPEN.store(v, Ordering::Relaxed);
}

pub fn set_ac_open(v: bool) {
    AC_OPEN.store(v, Ordering::Relaxed);
}

pub fn set_overflow_open(v: bool) {
    OVERFLOW_OPEN.store(v, Ordering::Relaxed);
}

/// Install on the taskbar thread; failure is non-fatal (popups then only
/// close via their buttons/Esc/WA_INACTIVE).
pub fn install(bar: HWND) {
    BAR.store(bar.0 as isize, Ordering::Relaxed);
    unsafe {
        if let Err(e) = SetWindowsHookExW(WH_MOUSE_LL, Some(hook), None, 0) {
            eprintln!("glide-shell: clickaway hook failed: {e}");
        }
    }
}

unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0
            && (START_OPEN.load(Ordering::Relaxed)
                || FLYOUT_OPEN.load(Ordering::Relaxed)
                || AC_OPEN.load(Ordering::Relaxed)
                || OVERFLOW_OPEN.load(Ordering::Relaxed))
        {
            let msg = wparam.0 as u32;
            if msg == WM_LBUTTONDOWN
                || msg == WM_RBUTTONDOWN
                || msg == WM_MBUTTONDOWN
                || msg == WM_XBUTTONDOWN
            {
                let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
                let _ = PostMessageW(
                    Some(HWND(BAR.load(Ordering::Relaxed) as *mut _)),
                    WM_CLICKAWAY,
                    WPARAM(info.pt.x as isize as usize),
                    LPARAM(info.pt.y as isize),
                );
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}
