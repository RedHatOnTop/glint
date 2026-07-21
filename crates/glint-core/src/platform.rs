// Win32 chrome: real Win11 acrylic backdrop, rounded corners, dark-mode aware.
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DWM_SYSTEMBACKDROP_TYPE,
    DWM_WINDOW_CORNER_PREFERENCE,
};

/// Win11 window backdrop material.
#[derive(Clone, Copy, PartialEq)]
pub enum Backdrop {
    /// DWMSBT_MAINWINDOW = 2 — Mica, for long-lived app windows (glide).
    Mica,
    /// DWMSBT_TRANSIENTWINDOW = 3 — Acrylic, for flyouts/palettes (glint).
    Acrylic,
}

impl Backdrop {
    fn dwm(self) -> DWM_SYSTEMBACKDROP_TYPE {
        match self {
            Backdrop::Mica => DWM_SYSTEMBACKDROP_TYPE(2),
            Backdrop::Acrylic => DWM_SYSTEMBACKDROP_TYPE(3),
        }
    }
}

pub fn is_dark_mode() -> bool {
    // HKCU\...\Themes\Personalize\AppsUseLightTheme == 0 → dark
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize")
        .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
        .map(|v| v == 0)
        .unwrap_or(true)
}

/// Windows accent color (DWM ColorizationColor, ARGB) → egui color.
pub fn accent_color() -> egui::Color32 {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\DWM")
        .and_then(|k| k.get_value::<u32, _>("ColorizationColor"))
        .map(|argb| {
            egui::Color32::from_rgb(
                ((argb >> 16) & 0xff) as u8,
                ((argb >> 8) & 0xff) as u8,
                (argb & 0xff) as u8,
            )
        })
        .unwrap_or(egui::Color32::from_rgb(0x00, 0x78, 0xD4)) // Win11 default blue
}

fn hwnd_of(cc: &eframe::CreationContext<'_>) -> Option<HWND> {
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut core::ffi::c_void)),
        _ => None,
    }
}

pub fn hwnd_isize(cc: &eframe::CreationContext<'_>) -> isize {
    hwnd_of(cc).map(|h| h.0 as isize).unwrap_or(0)
}

/// Show/hide from any thread (the hotkey handler thread). RegisterHotKey grants
/// foreground rights at hotkey time, so SetForegroundWindow succeeds here.
pub fn set_window_visible(hwnd: isize, show: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetForegroundWindow, ShowWindow, SW_HIDE, SW_SHOW,
    };
    if hwnd == 0 {
        return;
    }
    unsafe {
        let h = HWND(hwnd as *mut core::ffi::c_void);
        if show {
            let _ = ShowWindow(h, SW_SHOW);
            let _ = SetForegroundWindow(h);
        } else {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
}

/// Segoe UI Variable + Malgun (한글) + Segoe Fluent Icons, loaded from system fonts.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut add = |name: &str, path: &str| -> bool {
        match std::fs::read(path) {
            Ok(bytes) => {
                fonts
                    .font_data
                    .insert(name.to_string(), egui::FontData::from_owned(bytes).into());
                true
            }
            Err(_) => false,
        }
    };
    let seg = add("segoe_var", "C:\\Windows\\Fonts\\SegUIVar.ttf");
    let mal = add("malgun", "C:\\Windows\\Fonts\\malgun.ttf");
    let ico = add("fluent_icons", "C:\\Windows\\Fonts\\SegoeIcons.ttf")
        || add("fluent_icons", "C:\\Windows\\Fonts\\segmdl2.ttf");

    let prop = fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .unwrap();
    // Priority: Segoe UI Variable → Malgun (한글) → Fluent glyphs → egui defaults
    let mut i = 0;
    if seg {
        prop.insert(i, "segoe_var".into());
        i += 1;
    }
    if mal {
        prop.insert(i, "malgun".into());
        i += 1;
    }
    if ico {
        prop.insert(i, "fluent_icons".into());
    }
    // Monospace (file-preview snippets) keeps its ASCII face but gains a Malgun
    // fallback so Korean text files don't render as tofu.
    if mal {
        if let Some(mono) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            mono.push("malgun".into());
        }
    }
    ctx.set_fonts(fonts);
}

pub fn apply_win11_chrome(cc: &eframe::CreationContext<'_>, backdrop: Backdrop) {
    let Some(hwnd) = hwnd_of(cc) else { return };
    unsafe {
        // Rounded corners (Win11 default 8px radius)
        let corner: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as _,
            std::mem::size_of_val(&corner) as u32,
        );
        // Backdrop material behind the (transparent) framebuffer
        let bd = backdrop.dwm();
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &bd as *const _ as _,
            std::mem::size_of_val(&bd) as u32,
        );
        // Dark title-bar assets (no titlebar, but keeps DWM consistent)
        let dark: i32 = if is_dark_mode() { 1 } else { 0 };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as _,
            4,
        );
    }
}
