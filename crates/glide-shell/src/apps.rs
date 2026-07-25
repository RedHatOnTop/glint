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

/// Run an UninstallString. It is already a full command line (quoted exe +
/// switches, or `MsiExec.exe /X{guid}`), so it goes to `CreateProcessW` as-is —
/// the same thing appwiz.cpl does. The vendor uninstaller, with its own UAC
/// prompt, takes over.
///
/// It must NOT be routed through `cmd.exe /C`: cmd strips the outer quotes of a
/// `/C` argument and then re-parses the result, so a quoted image path holding
/// `&` (e.g. `"…\Intel(R) Graphics Software & Drivers\Uninstaller.exe"
/// --uninstaller`, present on real machines) splits into two broken commands.
/// `%` in a path would likewise be eaten by variable expansion.
pub fn uninstall(cmd: &str) {
    use windows::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
    };
    use windows::core::PWSTR;
    // CreateProcessW may write to the command-line buffer, so it must be owned
    // and writable — hence a Vec, not a literal.
    let mut line: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        let started = CreateProcessW(
            None,
            Some(PWSTR(line.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT,
            None,
            None,
            &si,
            &mut pi,
        )
        .is_ok();
        if started {
            let _ = windows::Win32::Foundation::CloseHandle(pi.hProcess);
            let _ = windows::Win32::Foundation::CloseHandle(pi.hThread);
        } else {
            // An uninstaller that needs elevation fails CreateProcess with
            // ERROR_ELEVATION_REQUIRED; ShellExecute's "runas" path handles the
            // UAC prompt. Quoting is safe here — no shell re-parse involved.
            elevated_fallback(cmd);
        }
    }
}

/// Split a command line into image path + arguments so ShellExecuteW can run it
/// with elevation. Honours a leading quoted path; otherwise splits at the first
/// space, which is what the unquoted UninstallString convention implies.
fn elevated_fallback(cmd: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;
    let trimmed = cmd.trim();
    let (exe, args) = if let Some(rest) = trimmed.strip_prefix('"') {
        match rest.split_once('"') {
            Some((exe, args)) => (exe, args.trim()),
            None => (rest, ""),
        }
    } else {
        match trimmed.split_once(' ') {
            Some((exe, args)) => (exe, args.trim()),
            None => (trimmed, ""),
        }
    };
    let wexe: Vec<u16> = exe.encode_utf16().chain(std::iter::once(0)).collect();
    let wargs: Vec<u16> = args.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("runas"),
            PCWSTR(wexe.as_ptr()),
            if args.is_empty() { PCWSTR::null() } else { PCWSTR(wargs.as_ptr()) },
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}
