//! Bare-Win-key hook → glint toggle (SHELL_DESIGN §6.4). A WH_KEYBOARD_LL
//! hook watches for the Win key going down and back up with no other key in
//! between; that release is swallowed and replaced with Win+dummy so the
//! stock Start menu never fires, then the taskbar thread pokes glint's
//! toggle pipe (spawning glint if it isn't running).
//!
//! Win+<anything> combos pass through untouched. The callback does no I/O —
//! it must stay under a millisecond or the OS drops the hook.

use std::cell::Cell;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, OPEN_EXISTING, WriteFile,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, KBDLLHOOKSTRUCT, LLKHF_INJECTED, PostMessageW, SW_SHOWNORMAL,
    SetWindowsHookExW, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};
use windows::core::PCWSTR;

/// Posted to the bar window when a bare Win release was captured.
pub const WM_WINKEY: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 2;
/// Posted when Win+S was captured (search — glint).
pub const WM_WINKEY_S: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 3;
/// Posted for every other Win chord we claim; `wparam` is the VK.
pub const WM_WINKEY_COMBO: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 8;

// PowerToys' masking key: reserved VK, no app reacts to it, but its presence
// between Win-down and Win-up stops explorer treating the Win press as bare.
const VK_DUMMY: u16 = 0xFF;
const VK_S: u32 = 0x53;

/// The Win chords explorer used to answer and nobody does once it is gone —
/// with the shell replaced they reach no window at all, and the bare letter
/// lands in whatever has focus. Win+L (winlogon) and Win+Shift+S (the OS
/// snipper) are not on the list because they never belonged to the shell.
const CLAIMED: &[u32] = &[
    0x45, // E  file manager
    0x52, // R  run
    0x44, // D  show desktop (toggle)
    0x4D, // M  minimize all
    0x49, // I  settings
    0x41, // A  action centre
    0x58, // X  power-user menu
    0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, // 1-9  bar slots
];

thread_local! {
    static BAR: Cell<isize> = const { Cell::new(0) };
    static WIN_DOWN: Cell<bool> = const { Cell::new(false) };
    static OTHER_KEY: Cell<bool> = const { Cell::new(false) };
    /// VK of a chord press we swallowed, so its release goes too and an
    /// auto-repeat does not fire the action again. 0 when nothing is held.
    static SWALLOW_VK: Cell<u32> = const { Cell::new(0) };
}

/// Install the hook on the current (taskbar) thread; `bar` receives
/// WM_WINKEY. Failure is non-fatal — the shell just loses the shortcut.
pub fn install(bar: HWND) {
    BAR.set(bar.0 as isize);
    unsafe {
        if let Err(e) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), None, 0) {
            crate::safety::note(&format!("winkey hook failed: {e}"));
        }
    }
}

unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            // Our own SendInput events come back through here — let them
            // pass, and keep them out of the bare-press bookkeeping.
            if kb.flags.0 & LLKHF_INJECTED.0 == 0 {
                let win = kb.vkCode == VK_LWIN.0 as u32 || kb.vkCode == VK_RWIN.0 as u32;
                match wparam.0 as u32 {
                    WM_KEYDOWN | WM_SYSKEYDOWN if win => {
                        if !WIN_DOWN.get() {
                            WIN_DOWN.set(true);
                            OTHER_KEY.set(false);
                        }
                    }
                    WM_KEYDOWN | WM_SYSKEYDOWN => {
                        // A Win release we never saw — the guest lab produced
                        // one by injecting a synthetic press — would otherwise
                        // leave WIN_DOWN set for the rest of the session, and
                        // every later S would disappear into the search chord.
                        // The flag is a latch, so confirm it against the key.
                        if WIN_DOWN.get() && !win_physically_down() {
                            WIN_DOWN.set(false);
                        }
                        // A chord we answer: swallow every repeat while Win is
                        // held and mask once with the dummy so the Win release
                        // that follows doesn't read as bare. Only the plain
                        // chord — Win+Shift+S is the OS snipping shortcut, and
                        // any other modifier isn't ours either.
                        let ours = kb.vkCode == VK_S || CLAIMED.contains(&kb.vkCode);
                        if WIN_DOWN.get() && ours && !modifier_down() {
                            OTHER_KEY.set(true);
                            if SWALLOW_VK.get() != kb.vkCode {
                                SWALLOW_VK.set(kb.vkCode);
                                send_keys(&[(VK_DUMMY, false), (VK_DUMMY, true)]);
                                let (msg, w) = if kb.vkCode == VK_S {
                                    (WM_WINKEY_S, 0)
                                } else {
                                    (WM_WINKEY_COMBO, kb.vkCode as usize)
                                };
                                let _ = PostMessageW(
                                    Some(HWND(BAR.get() as *mut _)),
                                    msg,
                                    WPARAM(w),
                                    LPARAM(0),
                                );
                            }
                            return LRESULT(1);
                        }
                        OTHER_KEY.set(true);
                    }
                    WM_KEYUP | WM_SYSKEYUP if kb.vkCode == SWALLOW_VK.get() => {
                        // The matching release of a swallowed chord press.
                        SWALLOW_VK.set(0);
                        return LRESULT(1);
                    }
                    WM_KEYUP | WM_SYSKEYUP if win => {
                        let bare = WIN_DOWN.get() && !OTHER_KEY.get();
                        WIN_DOWN.set(false);
                        if bare {
                            // Swallow the physical release and resynthesize it
                            // behind a dummy key, so the system sees Win+dummy
                            // instead of a bare Win press (no Start menu), and
                            // the Win key still ends up released.
                            send_keys(&[
                                (VK_DUMMY, false),
                                (VK_DUMMY, true),
                                (kb.vkCode as u16, true),
                            ]);
                            let _ = PostMessageW(
                                Some(HWND(BAR.get() as *mut _)),
                                WM_WINKEY,
                                WPARAM(0),
                                LPARAM(0),
                            );
                            return LRESULT(1);
                        }
                    }
                    _ => {}
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

fn win_physically_down() -> bool {
    [VK_LWIN, VK_RWIN]
        .iter()
        .any(|&vk| unsafe { GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 })
}

fn modifier_down() -> bool {
    [VK_SHIFT, VK_CONTROL, VK_MENU]
        .iter()
        .any(|&vk| unsafe { GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 })
}

fn send_keys(keys: &[(u16, bool)]) {
    let inputs: Vec<INPUT> = keys
        .iter()
        .map(|&(vk, up)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        })
        .collect();
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Win+E. glide is this desktop's file manager; explorer would open the stock
/// one and put a second shell's window on screen.
pub fn open_file_manager() {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("glide.exe")));
    match exe {
        Some(path) if path.exists() => launch(&path.to_string_lossy(), None),
        // No sibling build: better the stock window than nothing at all.
        _ => launch("explorer.exe", None),
    }
}

/// Win+R. shell32's own run dialog, by ordinal, in a process of its own — it
/// is modal, and hosting it here would freeze the bar for as long as it is up.
pub fn run_dialog() {
    launch("rundll32.exe", Some("shell32.dll,#61"));
}

fn launch(exe: &str, args: Option<&str>) {
    let exe: Vec<u16> = exe.encode_utf16().chain(std::iter::once(0)).collect();
    let args: Option<Vec<u16>> =
        args.map(|a| a.encode_utf16().chain(std::iter::once(0)).collect());
    unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            windows::core::w!("open"),
            PCWSTR(exe.as_ptr()),
            args.as_ref().map_or(PCWSTR::null(), |a| PCWSTR(a.as_ptr())),
            None,
            SW_SHOWNORMAL,
        );
    }
}

/// UI-thread handler for WM_WINKEY: signal glint's toggle pipe, or start
/// glint (sibling binary) when no instance owns the pipe yet.
pub fn toggle_glint() {
    // Must match glint_core::toggle_pipe::PIPE — glide-shell deliberately
    // doesn't depend on the egui workspace half.
    const PIPE: &str = r"\\.\pipe\glint-toggle";
    let name: Vec<u16> = PIPE.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        match CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        ) {
            Ok(h) => {
                let mut written = 0u32;
                let _ = WriteFile(h, Some(b"toggle"), Some(&mut written), None);
                let _ = CloseHandle(h);
            }
            Err(_) => {
                // No pipe → glint isn't running; first bare-Win launches it
                // (it starts visible, so this already reads as "menu opened").
                let exe = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join("glint.exe")));
                match exe {
                    Some(path) if path.exists() => {
                        let wide: Vec<u16> = path
                            .as_os_str()
                            .to_string_lossy()
                            .encode_utf16()
                            .chain(std::iter::once(0))
                            .collect();
                        windows::Win32::UI::Shell::ShellExecuteW(
                            None,
                            windows::core::w!("open"),
                            PCWSTR(wide.as_ptr()),
                            None,
                            None,
                            SW_SHOWNORMAL,
                        );
                    }
                    _ => crate::safety::note("glint.exe not found next to shell"),
                }
            }
        }
    }
}
