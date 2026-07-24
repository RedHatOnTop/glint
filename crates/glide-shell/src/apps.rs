//! Installed-program inventory + uninstall, read from the Uninstall registry
//! hives — the same source appwiz.cpl uses — surfaced inline in the 앱 pane so
//! the settings app owns app removal instead of bouncing to the classic applet.

use winreg::RegKey;
use winreg::enums::{
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY,
};

const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";

pub struct App {
    pub name: String,
    pub publisher: String,
    pub version: String,
    /// The raw UninstallString — a command line handed to the vendor uninstaller.
    pub uninstall: String,
}

/// Every user-visible installed program, deduped and sorted by name.
pub fn installed() -> Vec<App> {
    let mut out: Vec<App> = Vec::new();
    // The three places appwiz looks: HKLM 64-bit, HKLM 32-bit (WOW), HKCU.
    let hives = [
        (HKEY_LOCAL_MACHINE, KEY_READ | KEY_WOW64_64KEY),
        (HKEY_LOCAL_MACHINE, KEY_READ | KEY_WOW64_32KEY),
        (HKEY_CURRENT_USER, KEY_READ),
    ];
    for (root, flags) in hives {
        let Ok(base) = RegKey::predef(root).open_subkey_with_flags(UNINSTALL, flags) else {
            continue;
        };
        for sub in base.enum_keys().flatten() {
            let Ok(k) = base.open_subkey_with_flags(&sub, KEY_READ) else { continue };
            let name: String = k.get_value("DisplayName").unwrap_or_default();
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            // Hide OS components and patch/update entries, like appwiz does.
            let sys: u32 = k.get_value("SystemComponent").unwrap_or(0);
            if sys == 1 || k.get_value::<String, _>("ParentKeyName").is_ok() {
                continue;
            }
            let uninstall: String = k.get_value("UninstallString").unwrap_or_default();
            if uninstall.trim().is_empty() {
                continue;
            }
            if out.iter().any(|a| a.name.eq_ignore_ascii_case(name)) {
                continue;
            }
            out.push(App {
                name: name.to_string(),
                publisher: k.get_value("Publisher").unwrap_or_default(),
                version: k.get_value("DisplayVersion").unwrap_or_default(),
                uninstall,
            });
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// Run an UninstallString. It is a full command line (quoted exe + switches, or
/// `MsiExec.exe /X{guid}`), so cmd.exe parses it and the vendor uninstaller —
/// with its own UAC prompt — takes over.
pub fn uninstall(cmd: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};
    let arg = format!("/C {cmd}");
    let wide: Vec<u16> = arg.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(None, w!("open"), w!("cmd.exe"), PCWSTR(wide.as_ptr()), None, SW_SHOWNORMAL);
    }
}
