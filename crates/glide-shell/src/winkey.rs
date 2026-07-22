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

// PowerToys' masking key: reserved VK, no app reacts to it, but its presence
// between Win-down and Win-up stops explorer treating the Win press as bare.
const VK_DUMMY: u16 = 0xFF;
const VK_S: u32 = 0x53;

thread_local! {
    static BAR: Cell<isize> = const { Cell::new(0) };
    static WIN_DOWN: Cell<bool> = const { Cell::new(false) };
    static OTHER_KEY: Cell<bool> = const { Cell::new(false) };
    static SWALLOW_S: Cell<bool> = const { Cell::new(false) };
}

/// Install the hook on the current (taskbar) thread; `bar` receives
/// WM_WINKEY. Failure is non-fatal — the shell just loses the shortcut.
pub fn install(bar: HWND) {
    BAR.set(bar.0 as isize);
    unsafe {
        if let Err(e) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), None, 0) {
            eprintln!("glide-shell: winkey hook failed: {e}");
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
                        // Win+S: ours (glint search), never the stock search
                        // pane. Swallow every S repeat while Win is held and
                        // mask once with the dummy so the Win release that
                        // follows doesn't read as bare. Only the plain chord:
                        // Win+Shift+S is the OS snipping shortcut (and any
                        // other modifier isn't ours either).
                        if WIN_DOWN.get() && kb.vkCode == VK_S && !modifier_down() {
                            OTHER_KEY.set(true);
                            if !SWALLOW_S.get() {
                                SWALLOW_S.set(true);
                                send_keys(&[(VK_DUMMY, false), (VK_DUMMY, true)]);
                                let _ = PostMessageW(
                                    Some(HWND(BAR.get() as *mut _)),
                                    WM_WINKEY_S,
                                    WPARAM(0),
                                    LPARAM(0),
                                );
                            }
                            return LRESULT(1);
                        }
                        OTHER_KEY.set(true);
                    }
                    WM_KEYUP | WM_SYSKEYUP if kb.vkCode == VK_S && SWALLOW_S.get() => {
                        // The matching release of a swallowed S press.
                        SWALLOW_S.set(false);
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
                    _ => eprintln!("glide-shell: glint.exe not found next to shell"),
                }
            }
        }
    }
}
