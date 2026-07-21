//! Register glide as the Windows folder handler (double-click a folder / drive →
//! glide instead of Explorer).
//!
//! Everything is written under HKCU (no admin needed) and every key we create is
//! removed again on unregister, so Explorer is fully restored. This is a
//! reversible opt-in the user toggles — never a silent hijack.
//!
//! Win+E redirect was tried via the `opennewwindow` verb of CLSID
//! {52205fd8-…} and does NOT work on Windows 11 26200 (Microsoft dropped that
//! path); unregister still sweeps the key in case an older build set it.
use std::io;
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

// Legacy Win+E redirect CLSID — only ever deleted now, never written.
const WINE_ROOT: &str = r"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}";

// Folder + drive open verbs. Overriding these at the user level makes a
// double-clicked folder open glide instead of Explorer.
const FOLDER_CLASSES: [&str; 2] = ["Directory", "Drive"];

fn exe_path() -> io::Result<String> {
    Ok(std::env::current_exe()?
        .to_string_lossy()
        .replace('/', "\\"))
}

fn hkcu() -> RegKey {
    RegKey::predef(HKEY_CURRENT_USER)
}

fn default_value(subkey: &str) -> Option<String> {
    hkcu()
        .open_subkey(subkey)
        .ok()
        .and_then(|k| k.get_value::<String, _>("").ok())
}

/// True when the folder-open verb currently points at this exe.
pub fn folder_handler_enabled() -> bool {
    let Ok(exe) = exe_path() else { return false };
    default_value(r"Software\Classes\Directory\shell\open\command")
        .map(|v| v.to_lowercase().contains(&exe.to_lowercase()))
        .unwrap_or(false)
}

pub fn set_folder_handler(on: bool) -> io::Result<()> {
    if on {
        let exe = exe_path()?;
        let cmd = format!("\"{exe}\" \"%1\"");
        for class in FOLDER_CLASSES {
            let (k, _) =
                hkcu().create_subkey(format!(r"Software\Classes\{class}\shell\open\command"))?;
            k.set_value("", &cmd)?;
            // An empty ddeexec at the user level shadows Explorer's DDE handshake
            // so our command actually runs (otherwise DDE steals the open).
            let (dde, _) =
                hkcu().create_subkey(format!(r"Software\Classes\{class}\shell\open\ddeexec"))?;
            dde.set_value("", &"")?;
        }
    } else {
        for class in FOLDER_CLASSES {
            // Drop the whole `open` override we created; the machine default under
            // HKCR takes back over.
            let _ = hkcu().delete_subkey_all(format!(r"Software\Classes\{class}\shell\open"));
        }
        // Sweep the dead Win+E CLSID redirect if a prior build left one behind.
        let _ = hkcu().delete_subkey_all(WINE_ROOT);
    }
    Ok(())
}

// Right-click "glide에서 열기" verb: (class, arg placeholder). %1 = the clicked
// item, %V = the folder whose background was clicked. Shows in Win11's legacy
// menu ("더 많은 옵션 표시" / Shift+F10) — the modern top level needs a packaged
// IExplorerCommand, out of scope here.
const VERB_CLASSES: [(&str, &str); 3] = [
    ("Directory", "%1"),
    (r"Directory\Background", "%V"),
    ("Drive", "%1"),
];

/// True when the context-menu verb points at this exe.
pub fn context_menu_enabled() -> bool {
    let Ok(exe) = exe_path() else { return false };
    default_value(r"Software\Classes\Directory\shell\glide\command")
        .map(|v| v.to_lowercase().contains(&exe.to_lowercase()))
        .unwrap_or(false)
}

pub fn set_context_menu(on: bool) -> io::Result<()> {
    if on {
        let exe = exe_path()?;
        for (class, arg) in VERB_CLASSES {
            let (k, _) = hkcu().create_subkey(format!(r"Software\Classes\{class}\shell\glide"))?;
            k.set_value("", &"glide에서 열기")?;
            k.set_value("Icon", &exe)?;
            let (c, _) =
                hkcu().create_subkey(format!(r"Software\Classes\{class}\shell\glide\command"))?;
            c.set_value("", &format!("\"{exe}\" \"{arg}\""))?;
        }
    } else {
        for (class, _) in VERB_CLASSES {
            let _ = hkcu().delete_subkey_all(format!(r"Software\Classes\{class}\shell\glide"));
        }
    }
    Ok(())
}
