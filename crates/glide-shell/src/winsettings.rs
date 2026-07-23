//! The Windows-side reads and writes glide's control center exposes, so the
//! settings app is a real system surface — not just glide's own toggles.
//!
//! Two jobs the stock UI does badly:
//!   * Personalization (dark mode / transparency / accented title bars) lives
//!     in registry DWORDs under Themes\Personalize + DWM; we read and write
//!     them and broadcast WM_SETTINGCHANGE so running apps re-theme live.
//!   * Startup apps — the "billboard" that installers keep adding to — are
//!     HKCU\Run values plus Startup-folder shortcuts, with an enable/disable
//!     bit stored the exact way Task Manager stores it (StartupApproved). We
//!     enumerate and flip that bit, all under HKCU so no elevation is needed.

use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, REG_BINARY};

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
};
use windows::core::{PCWSTR, w};

const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
const DWM: &str = r"Software\Microsoft\Windows\DWM";
const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_RUN: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const APPROVED_FOLDER: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder";

// ---- personalization -------------------------------------------------------

fn read_dword(path: &str, name: &str) -> Option<u32> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(path, KEY_READ)
        .ok()?
        .get_value::<u32, _>(name)
        .ok()
}

fn write_dword(path: &str, name: &str, val: u32) {
    if let Ok((k, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(path) {
        let _ = k.set_value(name, &val);
    }
}

/// Nudge every running app to re-read the theme (what the Settings app does
/// when you flip dark mode) so the change is visible without a re-login.
fn broadcast_theme() {
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("ImmersiveColorSet").as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            200,
            None,
        );
    }
}

pub fn dark_mode() -> bool {
    // Default (key absent) on Windows is light.
    read_dword(PERSONALIZE, "AppsUseLightTheme").map(|v| v == 0).unwrap_or(false)
}

pub fn set_dark_mode(dark: bool) {
    let v = u32::from(!dark); // 1 = light, 0 = dark
    write_dword(PERSONALIZE, "AppsUseLightTheme", v);
    write_dword(PERSONALIZE, "SystemUsesLightTheme", v);
    broadcast_theme();
}

pub fn transparency() -> bool {
    read_dword(PERSONALIZE, "EnableTransparency").map(|v| v == 1).unwrap_or(true)
}

pub fn set_transparency(on: bool) {
    write_dword(PERSONALIZE, "EnableTransparency", u32::from(on));
    broadcast_theme();
}

pub fn title_accent() -> bool {
    read_dword(DWM, "ColorPrevalence").map(|v| v == 1).unwrap_or(false)
}

pub fn set_title_accent(on: bool) {
    write_dword(DWM, "ColorPrevalence", u32::from(on));
    broadcast_theme();
}

// ---- startup apps ----------------------------------------------------------

pub struct StartupItem {
    pub name: String,
    /// Command line (Run) or shortcut path (folder) — shown as the subtitle.
    pub detail: String,
    /// Startup-folder shortcut vs HKCU\Run value; picks the approval subkey.
    pub folder: bool,
    pub enabled: bool,
}

/// The approval bit Task Manager writes: 12 bytes, byte 0 even = enabled, odd
/// = disabled. Absent value = enabled (never toggled).
fn approved_enabled(subkey: &str, name: &str) -> bool {
    let Ok(k) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(subkey, KEY_READ) else {
        return true;
    };
    match k.get_raw_value(name) {
        Ok(v) => v.bytes.first().map(|b| b & 1 == 0).unwrap_or(true),
        Err(_) => true,
    }
}

fn set_approved(subkey: &str, name: &str, enabled: bool) {
    let Ok((k, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(subkey) else {
        return;
    };
    let mut bytes = vec![0u8; 12];
    bytes[0] = if enabled { 0x02 } else { 0x03 };
    let _ = k.set_raw_value(
        name,
        &winreg::RegValue { bytes: bytes.into(), vtype: REG_BINARY },
    );
}

fn startup_folder() -> Option<std::path::PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(
        std::path::PathBuf::from(appdata)
            .join(r"Microsoft\Windows\Start Menu\Programs\Startup"),
    )
}

/// Everything HKCU launches at logon: Run values + Startup-folder shortcuts,
/// each tagged with whether it is currently approved to run.
pub fn list_startup() -> Vec<StartupItem> {
    let mut out = Vec::new();
    if let Ok(run) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN, KEY_READ) {
        for name in run.enum_values().flatten().map(|(n, _)| n) {
            let detail = run.get_value::<String, _>(&name).unwrap_or_default();
            let enabled = approved_enabled(APPROVED_RUN, &name);
            out.push(StartupItem { name, detail, folder: false, enabled });
        }
    }
    if let Some(dir) = startup_folder() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for p in rd.flatten().map(|e| e.path()) {
                let is_lnk = p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk"));
                if !is_lnk {
                    continue;
                }
                let file = p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                let name = p.file_stem().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                let enabled = approved_enabled(APPROVED_FOLDER, &file);
                out.push(StartupItem {
                    name,
                    detail: "시작 폴더 바로가기".to_string(),
                    folder: true,
                    enabled,
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

pub fn set_startup_enabled(item: &StartupItem, enabled: bool) {
    // The approval value is keyed by the Run value name, or by the shortcut's
    // full filename for folder items.
    let (subkey, name) = if item.folder {
        (APPROVED_FOLDER, format!("{}.lnk", item.name))
    } else {
        (APPROVED_RUN, item.name.clone())
    };
    set_approved(subkey, &name, enabled);
}

// ---- glide's own logon autostart -------------------------------------------

/// HKCU\Run value name for glide's bar. Distinct from the shell registration
/// (safety::register writes Winlogon Shell=); this just launches the bar at
/// logon alongside explorer, for daily-driving before a full swap.
const GLIDE_RUN_NAME: &str = "glide-shell";

pub fn glide_autostart() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN, KEY_READ)
        .ok()
        .and_then(|k| k.get_value::<String, _>(GLIDE_RUN_NAME).ok())
        .is_some()
}

pub fn set_glide_autostart(on: bool) {
    let Ok((k, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN) else {
        return;
    };
    if on {
        if let Ok(exe) = std::env::current_exe() {
            let _ = k.set_value(GLIDE_RUN_NAME, &exe.to_string_lossy().to_string());
        }
    } else {
        let _ = k.delete_value(GLIDE_RUN_NAME);
    }
}

// ---- system info (About) ---------------------------------------------------

/// (label, value) rows for the About page — the System control-panel applet,
/// consolidated.
pub fn system_info() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let hklm = RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE);

    if let Ok(cv) = hklm.open_subkey_with_flags(
        r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        KEY_READ,
    ) {
        let product: String = cv.get_value("ProductName").unwrap_or_default();
        let display: String = cv.get_value("DisplayVersion").unwrap_or_default();
        let build: String = cv.get_value("CurrentBuild").unwrap_or_default();
        let ubr: u32 = cv.get_value("UBR").unwrap_or(0);
        out.push((
            "운영 체제".to_string(),
            format!("{product} {display} (빌드 {build}.{ubr})").trim().to_string(),
        ));
    }

    if let Ok(cpu) = hklm.open_subkey_with_flags(
        r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
        KEY_READ,
    ) {
        let name: String = cpu.get_value("ProcessorNameString").unwrap_or_default();
        if !name.is_empty() {
            out.push(("프로세서".to_string(), name.trim().to_string()));
        }
    }

    out.push(("메모리".to_string(), ram_string()));

    let host = std::env::var("COMPUTERNAME").unwrap_or_default();
    let user = std::env::var("USERNAME").unwrap_or_default();
    if !host.is_empty() {
        out.push(("장치 이름".to_string(), host));
    }
    if !user.is_empty() {
        out.push(("사용자".to_string(), user));
    }
    out
}

fn ram_string() -> String {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe {
        if GlobalMemoryStatusEx(&mut m).is_ok() {
            let gb = |b: u64| (b as f64) / 1024.0 / 1024.0 / 1024.0;
            format!("{:.1} GB (사용 가능 {:.1} GB)", gb(m.ullTotalPhys), gb(m.ullAvailPhys))
        } else {
            "알 수 없음".to_string()
        }
    }
}

// ---- Windows tweaks (the registry substance behind Settings/Control Panel) --
//
// Settings and Control Panel are, at bottom, GUIs that poke files and the
// registry. These are the tweaks people actually open regedit / dig through
// Settings for — exposed as direct HKCU reads/writes (no elevation), each
// reversible, applied live where Explorer honours a change notification.

/// How a tweak's on/off state maps to the registry.
pub enum TweakKind {
    /// A DWORD flag: `on` when enabled, `off` when disabled. `default_on` is
    /// the effective state when the value is absent.
    Flag { on: u32, off: u32, default_on: bool },
    /// The Win11→Win10 context-menu tweak: enabled = the CLSID key exists with
    /// an empty default value; disabled = key absent. Explorer reads it only at
    /// startup, so we write it and leave the relaunch to the user (killing
    /// explorer under an active session is not ours to do).
    ClassicMenu,
}

pub struct Tweak {
    pub title: &'static str,
    pub sub: &'static str,
    path: &'static str,
    name: &'static str,
    pub kind: TweakKind,
}

const ADV: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced";
const CABINET: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\CabinetState";
const SEARCH: &str = r"Software\Microsoft\Windows\CurrentVersion\Search";
/// The shell extension the Win11 short context menu hangs off; an empty
/// InprocServer32 default disables it and restores the full Win10 menu.
const CLASSIC_MENU_CLSID: &str = r"Software\Classes\CLSID\{86ca1aa0-34aa-4e8b-a509-50c905bae2a2}";
const CLASSIC_MENU_KEY: &str =
    r"Software\Classes\CLSID\{86ca1aa0-34aa-4e8b-a509-50c905bae2a2}\InprocServer32";

pub const TWEAKS: &[Tweak] = &[
    Tweak {
        title: "파일 확장명 표시",
        sub: "모든 파일의 .exe · .txt 확장명 표시",
        path: ADV,
        name: "HideFileExt",
        kind: TweakKind::Flag { on: 0, off: 1, default_on: false },
    },
    Tweak {
        title: "숨김 파일 표시",
        sub: "숨김 속성 파일 · 폴더 표시",
        path: ADV,
        name: "Hidden",
        kind: TweakKind::Flag { on: 1, off: 2, default_on: false },
    },
    Tweak {
        title: "보호된 운영체제 파일 표시",
        sub: "시스템 파일까지 표시 — 주의",
        path: ADV,
        name: "ShowSuperHidden",
        kind: TweakKind::Flag { on: 1, off: 0, default_on: false },
    },
    Tweak {
        title: "탐색기 시작 위치 = 내 PC",
        sub: "새 탐색기 창을 '내 PC'로 (기본: 즐겨찾기)",
        path: ADV,
        name: "LaunchTo",
        kind: TweakKind::Flag { on: 1, off: 2, default_on: false },
    },
    Tweak {
        title: "제목 표시줄에 전체 경로",
        sub: "탐색기 제목에 전체 폴더 경로 표시",
        path: CABINET,
        name: "FullPath",
        kind: TweakKind::Flag { on: 1, off: 0, default_on: false },
    },
    Tweak {
        title: "검색 웹 결과(Bing) 끄기",
        sub: "시작 · 검색에서 Bing 웹 결과 제거",
        path: SEARCH,
        name: "BingSearchEnabled",
        kind: TweakKind::Flag { on: 0, off: 1, default_on: false },
    },
    Tweak {
        title: "마우스 우클릭 = Win10 전체 메뉴",
        sub: "Win11 축약 메뉴 대신 전체 메뉴 — 탐색기 재시작 후 적용",
        path: CLASSIC_MENU_KEY,
        name: "",
        kind: TweakKind::ClassicMenu,
    },
];

pub fn tweak_enabled(t: &Tweak) -> bool {
    match &t.kind {
        TweakKind::Flag { on, default_on, .. } => match read_dword(t.path, t.name) {
            Some(v) => v == *on,
            None => *default_on,
        },
        TweakKind::ClassicMenu => RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(t.path, KEY_READ)
            .is_ok(),
    }
}

pub fn set_tweak(t: &Tweak, enabled: bool) {
    match &t.kind {
        TweakKind::Flag { on, off, .. } => {
            write_dword(t.path, t.name, if enabled { *on } else { *off });
            notify_explorer();
        }
        TweakKind::ClassicMenu => {
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            if enabled {
                if let Ok((k, _)) = hkcu.create_subkey(t.path) {
                    let _ = k.set_value("", &"");
                }
            } else {
                // Restore the Win11 menu = drop the whole CLSID override.
                let _ = hkcu.delete_subkey_all(CLASSIC_MENU_CLSID);
            }
        }
    }
}

/// Tell Explorer its file-view settings changed so open windows refresh without
/// a logoff (what the Folder Options dialog does under the hood).
fn notify_explorer() {
    use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
    }
}

// ---- launch (deep-links + classic tools) -----------------------------------

/// Open a still-stock panel or tool so the settings app stays the single entry
/// point: ms-settings: URIs, control.exe applets, .msc consoles, or a folder
/// path (env vars expanded, so `%APPDATA%\glide-shell` opens the config dir).
pub fn launch(target: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let expanded = expand_env(target);
    let wide: Vec<u16> = expanded.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

fn expand_env(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
    let src: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let n = ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), None);
        if n == 0 {
            return s.to_string();
        }
        let mut buf = vec![0u16; n as usize];
        let n2 = ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf));
        if n2 == 0 {
            return s.to_string();
        }
        String::from_utf16_lossy(&buf[..(n2 as usize).saturating_sub(1)])
    }
}
