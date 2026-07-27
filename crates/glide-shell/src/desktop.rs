//! M3 desktop — wallpaper + icon grid at the bottom of the z-order
//! (SHELL_DESIGN §6.3). Full-monitor window pinned under every normal window
//! via WM_WINDOWPOSCHANGING; alongside explorer it visually replaces the
//! stock desktop (same items, our rendering), after the swap it IS the
//! desktop.
//!
//! v1 scope: select (click / Ctrl / marquee), double-click open, right-click
//! shell context menu (shellmenu.rs), wallpaper reload on WM_SETTINGCHANGE.
//! Not yet: explorer's saved icon positions (undocumented ItemPos blobs — we
//! auto-arrange), drag, keyboard. Recorded in §6.3.

use std::path::PathBuf;

use windows::Win32::Foundation::{GENERIC_READ, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_PROPERTIES1, D2D1_DRAW_TEXT_OPTIONS_CLIP,
    D2D1_INTERPOLATION_MODE_LINEAR, D2D1_ROUNDED_RECT, ID2D1Bitmap1,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_TEXT_ALIGNMENT_CENTER, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, ValidateRect,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::Graphics::Imaging::{
    GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_CONTROL,
    GetKeyState,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IShellItem, IShellItemImageFactory, SHCNE_ALLEVENTS, SHCNRF_InterruptLevel,
    SHCNRF_NewDelivery, SHCNRF_ShellLevel, SHChangeNotification_Lock, SHChangeNotification_Unlock,
    SHChangeNotifyEntry, SHChangeNotifyRegister, SHCreateItemFromParsingName, SHParseDisplayName,
    SIGDN_NORMALDISPLAY, SIIGBF_BIGGERSIZEOK, SIIGBF_RESIZETOFIT, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::render::{Renderer, ellipsize, fill_round, rect};
use crate::theme;

const WM_MOUSELEAVE: u32 = 0x02A3;
/// SHChangeNotify delivery — something under the desktop moved.
const WM_SHELLCHANGE: u32 = WM_APP + 1;
/// Reload debounce: one user action arrives as a burst of notifications.
const TIMER_RELOAD: usize = 1;

// Background context menu, our own entries (SHELL_DESIGN §6.3).
const ID_REFRESH: u32 = 1;
const ID_GLIDE: u32 = 2;
const ID_DISPLAY: u32 = 3;
const ID_PERSONAL: u32 = 4;
const BG_CUSTOM: [crate::shellmenu::CustomItem; 5] = [
    (ID_REFRESH, "새로 고침", true),
    (ID_GLIDE, "glide로 열기", true),
    (0, "", true),
    (ID_DISPLAY, "디스플레이 설정", true),
    (ID_PERSONAL, "개인 설정", true),
];

const CELL_W: f32 = 84.0;
const CELL_H: f32 = 98.0;
const CELL_GAP: f32 = 6.0;
const ICON: f32 = 48.0;
const MARGIN: f32 = 18.0;

/// The desktop's namespace roots, in the order explorer lists them, with the
/// visibility each has on a fresh profile. They are not files — they live in
/// the shell namespace directly under the desktop, so everything about them
/// (label, icon, menu, opening) goes through their `::{CLSID}` parsing name.
const NAMESPACE_ITEMS: [(&str, bool); 5] = [
    ("{20D04FE0-3AEA-1069-A2D8-08002B30309D}", false), // 내 PC
    ("{59031A47-3F72-44A7-89C5-5595FE6B30EE}", false), // 사용자 파일
    ("{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}", false), // 네트워크
    ("{645FF040-5081-101B-9F08-00AA002F954E}", true),  // 휴지통
    ("{5399E694-6CE5-4D6C-8FCE-1D8870FDCBA0}", false), // 제어판
];

/// Which of the above the user has turned on (개인 설정 → 테마 → 바탕 화면 아이콘
/// 설정 writes here). A DWORD of 1 hides; absent means "leave the default".
const HIDE_ICONS: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel";

struct Item {
    /// Shell parsing name: a filesystem path, or `::{CLSID}`. This is the
    /// identity the shell answers to — menus and icons are built from it.
    parsing: String,
    /// Filesystem path, for the items that have one.
    path: Option<PathBuf>,
    label: Vec<u16>,
    bitmap: Option<ID2D1Bitmap1>,
    /// Cell top-left, logical.
    x: f32,
    y: f32,
    selected: bool,
}

impl Item {
    /// Items with equal keys share one IShellFolder, which is what a
    /// multi-selection context menu is built against. Namespace items all
    /// answer to the desktop root, so they group together as `None`.
    fn menu_group(&self) -> Option<PathBuf> {
        self.path.as_ref().and_then(|p| p.parent()).map(|p| p.to_path_buf())
    }
}

struct Marquee {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

pub struct Desktop {
    renderer: Renderer,
    fmt_label: IDWriteTextFormat,
    wallpaper: Option<ID2D1Bitmap1>,
    wall_path: String,
    wall_mtime: Option<std::time::SystemTime>,
    items: Vec<Item>,
    /// What the items were built from; WM_SETTINGCHANGE is broadcast for
    /// unrelated settings (ScreenXpert is chatty), so skip the shell-icon
    /// re-extraction unless the folder contents or work area changed.
    items_sig: (Vec<String>, (i32, i32, i32, i32)),
    hover: Option<usize>,
    marquee: Option<Marquee>,
    tracking: bool,
    w: f32,
    h: f32,
    scale: f32,
}

/// Create the desktop window; it lives on this thread's message loop for the
/// life of the process (leaked box, same as the shell itself).
pub fn spawn(dpi: f32) -> anyhow::Result<()> {
    if std::env::var_os("GLIDE_DESK_OFF").is_some() {
        return Ok(());
    }
    unsafe {
        let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
        let class = w!("glide_shell_desktop");
        let wc = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(desktop_wndproc),
            hInstance: hinstance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
            class,
            w!("glide-shell desktop"),
            WS_POPUP,
            0,
            0,
            sw,
            sh,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?;

        let scale = dpi / 96.0;
        let renderer = Renderer::new(hwnd, sw as u32, sh as u32, dpi)?;

        let fmt_label = renderer.dwrite.CreateTextFormat(
            w!("Segoe UI Variable"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            11.5,
            w!("ko-kr"),
        )?;
        fmt_label.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        // Two wrapped lines, then a character-ellipsis — explorer's look.
        ellipsize(&renderer.dwrite, &fmt_label);

        let mut desk = Box::new(Desktop {
            renderer,
            fmt_label,
            wallpaper: None,
            wall_path: String::new(),
            wall_mtime: None,
            items: Vec::new(),
            items_sig: (Vec::new(), (0, 0, 0, 0)),
            hover: None,
            marquee: None,
            tracking: false,
            w: sw as f32 / scale,
            h: sh as f32 / scale,
            scale,
        });
        desk.load_wallpaper();
        desk.load_items();
        crate::shellmenu::enable_dark_menus();
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::leak(desk) as *mut Desktop as isize);
        // Only after the pointer is installed: the shell can deliver the first
        // notification before this function returns, and a WM_SHELLCHANGE that
        // lands on a null userdata is a leaked delivery handle.
        watch(hwnd);

        let _ = SetWindowPos(
            hwnd,
            Some(HWND_BOTTOM),
            0,
            0,
            sw,
            sh,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let desk = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Desktop);
        eprintln!(
            "desktop: {} items, wallpaper={} ({})",
            desk.items.len(),
            desk.wallpaper.is_some(),
            desk.wall_path
        );
        desk.paint();
        Ok(())
    }
}

impl Desktop {
    fn load_wallpaper(&mut self) {
        if std::env::var_os("GLIDE_DESK_BARE").is_some() {
            return;
        }
        unsafe {
            let mut buf = [0u16; 512];
            let _ = SystemParametersInfoW(
                SPI_GETDESKWALLPAPER,
                buf.len() as u32,
                Some(buf.as_mut_ptr() as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
            let path = String::from_utf16_lossy(&buf[..len]);
            let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            if path == self.wall_path && mtime == self.wall_mtime && self.wallpaper.is_some() {
                return;
            }
            self.wall_path = path.clone();
            self.wall_mtime = mtime;
            self.wallpaper = None;
            if path.is_empty() {
                return;
            }
            self.wallpaper = (|| -> windows::core::Result<ID2D1Bitmap1> {
                let gpu = crate::render::gpu()?;
                let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
                let decoder = gpu.wic.CreateDecoderFromFilename(
                    PCWSTR(wide.as_ptr()),
                    None,
                    GENERIC_READ,
                    WICDecodeMetadataCacheOnDemand,
                )?;
                let frame = decoder.GetFrame(0)?;
                // Downscale to cover size before the GPU upload — Duo
                // wallpaper assets are 3600×3600; full-res cost 120+MB.
                let (mut sw, mut sh) = (0u32, 0u32);
                frame.GetSize(&mut sw, &mut sh)?;
                let (mw, mh) = (self.w * self.scale, self.h * self.scale);
                let s = (mw / sw.max(1) as f32).max(mh / sh.max(1) as f32).min(1.0);
                let (tw, th) = (
                    ((sw as f32 * s).ceil() as u32).max(1),
                    ((sh as f32 * s).ceil() as u32).max(1),
                );
                let scaler = gpu.wic.CreateBitmapScaler()?;
                scaler.Initialize(&frame, tw, th, WICBitmapInterpolationModeFant)?;
                let converter = gpu.wic.CreateFormatConverter()?;
                converter.Initialize(
                    &scaler,
                    &GUID_WICPixelFormat32bppPBGRA,
                    WICBitmapDitherTypeNone,
                    None,
                    0.0,
                    WICBitmapPaletteTypeCustom,
                )?;
                self.renderer.dc.CreateBitmapFromWicBitmap(&converter, None)
            })()
            .ok();
        }
    }

    /// The enabled namespace roots, then the user and public Desktop folders
    /// merged with dirs first and then by name, laid out in explorer-style
    /// columns inside the work area.
    fn load_items(&mut self) {
        let mut found: Vec<(String, Option<PathBuf>)> = NAMESPACE_ITEMS
            .iter()
            .filter(|(clsid, default_on)| namespace_visible(clsid, *default_on))
            .map(|(clsid, _)| (format!("::{clsid}"), None))
            .collect();

        let mut files: Vec<(PathBuf, bool)> = Vec::new();
        let roots = [
            std::env::var("USERPROFILE").ok().map(|p| PathBuf::from(p).join("Desktop")),
            std::env::var("PUBLIC").ok().map(|p| PathBuf::from(p).join("Desktop")),
        ];
        for root in roots.into_iter().flatten() {
            let Ok(rd) = std::fs::read_dir(root) else { continue };
            for f in rd.flatten() {
                let path = f.path();
                let name = f.file_name().to_string_lossy().to_ascii_lowercase();
                if name == "desktop.ini" {
                    continue;
                }
                let Ok(meta) = f.metadata() else { continue };
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x2 != 0 {
                    continue; // FILE_ATTRIBUTE_HIDDEN
                }
                files.push((path, meta.is_dir()));
            }
        }
        files.sort_by(|a, b| {
            b.1.cmp(&a.1).then_with(|| {
                let an = a.0.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
                let bn = b.0.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
                an.cmp(&bn)
            })
        });
        found.extend(
            files
                .into_iter()
                .map(|(p, _)| (p.as_os_str().to_string_lossy().into_owned(), Some(p))),
        );

        // Work area (explorer's bar + ours both reserve; icons stay above).
        let mut work = windows::Win32::Foundation::RECT::default();
        unsafe {
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut work as *mut _ as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
        }
        let sig = (
            found.iter().map(|(p, _)| p.clone()).collect::<Vec<String>>(),
            (work.left, work.top, work.right, work.bottom),
        );
        if sig == self.items_sig && !self.items.is_empty() {
            return;
        }
        self.items_sig = sig;

        let top = work.top as f32 / self.scale + MARGIN;
        let bottom = work.bottom as f32 / self.scale - MARGIN;
        let rows = (((bottom - top) / (CELL_H + CELL_GAP)).floor() as usize).max(1);

        let px = (ICON * self.scale * 2.0) as i32; // downscale-only quality
        self.items = found
            .into_iter()
            .enumerate()
            .map(|(i, (parsing, path))| {
                let (col, row) = (i / rows, i % rows);
                let label: String = match &path {
                    Some(p) => {
                        let stem_only = matches!(
                            p.extension().and_then(|e| e.to_str()),
                            Some("lnk") | Some("url")
                        );
                        if stem_only {
                            p.file_stem().unwrap_or_default().to_string_lossy().into_owned()
                        } else {
                            p.file_name().unwrap_or_default().to_string_lossy().into_owned()
                        }
                    }
                    // The shell owns the name, and it is localized.
                    None => display_name(&parsing).unwrap_or_else(|| parsing.clone()),
                };
                Item {
                    bitmap: shell_image(&self.renderer, &parsing, px),
                    label: label.encode_utf16().collect(),
                    parsing,
                    path,
                    x: MARGIN + col as f32 * (CELL_W + CELL_GAP),
                    y: top + row as f32 * (CELL_H + CELL_GAP),
                    selected: false,
                }
            })
            .collect();
    }

    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        self.items
            .iter()
            .position(|i| x >= i.x && x < i.x + CELL_W && y >= i.y && y < i.y + CELL_H)
    }

    fn open(&self, idx: usize) {
        let Some(item) = self.items.get(idx) else { return };
        match &item.path {
            Some(p) => open_uri(&p.as_os_str().to_string_lossy()),
            // A parsing name is not something ShellExecute takes; the shell:
            // scheme is how it reaches the namespace.
            None => open_uri(&format!("shell:{}", item.parsing)),
        }
    }

    fn paint(&mut self) {
        unsafe {
            let r = &self.renderer;
            r.dc.BeginDraw();
            r.dc.Clear(Some(&theme::rgba(18, 20, 24, 1.0)));

            if let Some(wp) = &self.wallpaper {
                let sz = wp.GetSize();
                if sz.width > 0.0 && sz.height > 0.0 {
                    // Cover-crop: fill the monitor, keep aspect, center.
                    let s = (self.w / sz.width).max(self.h / sz.height);
                    let (cw, ch) = (self.w / s, self.h / s);
                    let (sx, sy) = ((sz.width - cw) / 2.0, (sz.height - ch) / 2.0);
                    r.dc.DrawBitmap(
                        wp,
                        Some(&rect(0.0, 0.0, self.w, self.h)),
                        1.0,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        Some(&rect(sx, sy, sx + cw, sy + ch)),
                        None,
                    );
                }
            }

            for (i, item) in self.items.iter().enumerate() {
                let cell = rect(item.x, item.y, item.x + CELL_W, item.y + CELL_H);
                if item.selected {
                    fill_round(r, cell, 6.0, theme::with_alpha(theme::accent(), 0.22));
                    if let Ok(b) = r.brush(theme::with_alpha(theme::accent(), 0.7)) {
                        r.dc.DrawRoundedRectangle(
                            &D2D1_ROUNDED_RECT { rect: cell, radiusX: 6.0, radiusY: 6.0 },
                            &b,
                            1.0,
                            None,
                        );
                    }
                } else if self.hover == Some(i) {
                    fill_round(r, cell, 6.0, theme::rgba(255, 255, 255, 0.10));
                }

                let ix = item.x + (CELL_W - ICON) / 2.0;
                let iy = item.y + 8.0;
                if let Some(bmp) = &item.bitmap {
                    r.dc.DrawBitmap(
                        bmp,
                        Some(&rect(ix, iy, ix + ICON, iy + ICON)),
                        1.0,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        None,
                        None,
                    );
                } else if let Ok(b) = r.brush(theme::rgba(255, 255, 255, 0.12)) {
                    r.dc.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: rect(ix, iy, ix + ICON, iy + ICON),
                            radiusX: 8.0,
                            radiusY: 8.0,
                        },
                        &b,
                    );
                }

                // Label: a single drop shadow only darkens one side, and a
                // bright busy wallpaper eats white text on the other three.
                // Ring the glyphs instead — eight offsets at low alpha build a
                // halo that reads as a soft shadow but works against any
                // background, without a scrim box behind every icon.
                let lr = rect(item.x + 2.0, iy + ICON + 4.0, item.x + CELL_W - 2.0, item.y + CELL_H - 2.0);
                const HALO: [(f32, f32); 8] = [
                    (-1.0, -1.0), (0.0, -1.0), (1.0, -1.0),
                    (-1.0, 0.0), (1.0, 0.0),
                    (-1.0, 1.0), (0.0, 1.0), (1.0, 1.0),
                ];
                for (dx, dy) in HALO {
                    let o = rect(lr.left + dx, lr.top + dy, lr.right + dx, lr.bottom + dy);
                    self.label(&item.label, o, theme::rgba(0, 0, 0, 0.34));
                }
                // Weight under the text so it sits on the wallpaper rather than
                // floating in a uniform outline.
                let drop = rect(lr.left, lr.top + 2.0, lr.right, lr.bottom + 2.0);
                self.label(&item.label, drop, theme::rgba(0, 0, 0, 0.35));
                self.label(&item.label, lr, theme::rgba(244, 246, 250, 1.0));
            }

            if let Some(m) = &self.marquee {
                let sel = rect(m.x0.min(m.x1), m.y0.min(m.y1), m.x0.max(m.x1), m.y0.max(m.y1));
                if let Ok(b) = r.brush(theme::with_alpha(theme::accent(), 0.12)) {
                    r.dc.FillRectangle(&sel, &b);
                }
                if let Ok(b) = r.brush(theme::with_alpha(theme::accent(), 0.6)) {
                    r.dc.DrawRectangle(&sel, &b, 1.0, None);
                }
            }

            if let Err(e) = r.dc.EndDraw(None, None) {
                eprintln!("desktop EndDraw: {e}");
            }
            if let Err(e) = r.present() {
                eprintln!("desktop present: {e}");
            }
        }
    }

    fn label(&self, text: &[u16], r: D2D_RECT_F, c: D2D1_COLOR_F) {
        unsafe {
            if let Ok(b) = self.renderer.brush(c) {
                self.renderer.dc.DrawText(
                    text,
                    &self.fmt_label,
                    &r,
                    &b,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
    }

    /// Folder contents changed under us (shell verb, user request).
    fn refresh_all(&mut self) {
        self.items_sig.0.clear();
        self.load_items();
        self.load_wallpaper();
        self.paint();
    }

    fn marquee_apply(&mut self) {
        let Some(m) = &self.marquee else { return };
        let (l, t) = (m.x0.min(m.x1), m.y0.min(m.y1));
        let (r, b) = (m.x0.max(m.x1), m.y0.max(m.y1));
        for item in &mut self.items {
            item.selected =
                item.x < r && item.x + CELL_W > l && item.y < b && item.y + CELL_H > t;
        }
    }
}

fn open_uri(uri: &str) {
    unsafe {
        let wide: Vec<u16> = uri.encode_utf16().chain(std::iter::once(0)).collect();
        ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), None, None, SW_SHOWNORMAL);
    }
}

/// Sibling glide.exe on the Desktop folder; its single-instance pipe folds
/// repeat opens into tabs.
fn open_glide() {
    let Some(dir) = std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Desktop"))
    else {
        return;
    };
    let Some(exe) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("glide.exe")))
        .filter(|p| p.exists())
    else {
        crate::safety::note("glide.exe not found next to shell");
        return;
    };
    unsafe {
        let exe_w: Vec<u16> = exe
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let arg_w: Vec<u16> = format!("\"{}\"", dir.display())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(exe_w.as_ptr()),
            PCWSTR(arg_w.as_ptr()),
            None,
            SW_SHOWNORMAL,
        );
    }
}

/// Ask the shell to report changes to everything the desktop shows.
///
/// This is explorer's own mechanism rather than a directory watch, because
/// half of what the desktop shows is not a directory: a file going into the
/// recycle bin changes that icon, and no filesystem event says so.
unsafe fn watch(hwnd: HWND) {
    unsafe {
        let mut names: Vec<String> =
            NAMESPACE_ITEMS.iter().map(|(clsid, _)| format!("::{clsid}")).collect();
        for var in ["USERPROFILE", "PUBLIC"] {
            if let Some(p) = std::env::var_os(var) {
                let dir = PathBuf::from(p).join("Desktop");
                names.push(dir.as_os_str().to_string_lossy().into_owned());
            }
        }

        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
        let mut entries: Vec<SHChangeNotifyEntry> = Vec::new();
        for name in &names {
            let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            if SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None).is_err() {
                continue;
            }
            pidls.push(pidl);
            entries.push(SHChangeNotifyEntry { pidl, fRecursive: false.into() });
        }

        let id = SHChangeNotifyRegister(
            hwnd,
            SHCNRF_ShellLevel | SHCNRF_InterruptLevel | SHCNRF_NewDelivery,
            SHCNE_ALLEVENTS.0 as i32,
            WM_SHELLCHANGE,
            entries.len() as i32,
            entries.as_ptr(),
        );
        if id == 0 {
            crate::safety::note("desktop: SHChangeNotifyRegister failed — no live refresh");
        }
        // The register copies the entries; these were ours.
        for pidl in pidls {
            CoTaskMemFree(Some(pidl as *const core::ffi::c_void));
        }
    }
}

/// Is this namespace root on the desktop? `default_on` stands in for the
/// value explorer has never written.
fn namespace_visible(clsid: &str, default_on: bool) -> bool {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(HIDE_ICONS, KEY_READ)
        .and_then(|k| k.get_value::<u32, _>(clsid))
        .map_or(default_on, |hidden| hidden == 0)
}

/// The shell's own localized name for a parsing name ("휴지통").
fn display_name(parsing: &str) -> Option<String> {
    unsafe {
        let wide: Vec<u16> = parsing.encode_utf16().chain(std::iter::once(0)).collect();
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let name = item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?;
        let s = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const core::ffi::c_void));
        s
    }
}

/// Shell-quality image for a parsing name: real thumbnails for pictures,
/// themed icons for everything else, via IShellItemImageFactory.
fn shell_image(renderer: &Renderer, parsing: &str, px: i32) -> Option<ID2D1Bitmap1> {
    if std::env::var_os("GLIDE_DESK_BARE").is_some() {
        return None;
    }
    unsafe {
        let wide: Vec<u16> = parsing.encode_utf16().chain(std::iter::once(0)).collect();
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let hbmp = factory
            .GetImage(
                windows::Win32::Foundation::SIZE { cx: px, cy: px },
                SIIGBF_RESIZETOFIT | SIIGBF_BIGGERSIZEOK,
            )
            .ok()?;

        let result = (|| {
            let hdc = CreateCompatibleDC(None);
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: px,
                    biHeight: -px, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = vec![0u8; (px * px * 4) as usize];
            let got = GetDIBits(
                hdc,
                hbmp,
                0,
                px as u32,
                Some(pixels.as_mut_ptr() as *mut _),
                &mut bi,
                DIB_RGB_COLORS,
            );
            let _ = DeleteDC(hdc);
            if got == 0 {
                return None;
            }
            // Thumbnails come back with a dead alpha channel — force opaque.
            // Icon alpha is straight; premultiply for the swapchain format.
            if pixels.chunks_exact(4).all(|p| p[3] == 0) {
                for p in pixels.chunks_exact_mut(4) {
                    p[3] = 255;
                }
            }
            for p in pixels.chunks_exact_mut(4) {
                let a = p[3] as u32;
                p[0] = ((p[0] as u32 * a) / 255) as u8;
                p[1] = ((p[1] as u32 * a) / 255) as u8;
                p[2] = ((p[2] as u32 * a) / 255) as u8;
            }
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
                ..Default::default()
            };
            renderer
                .dc
                .CreateBitmap(
                    D2D_SIZE_U { width: px as u32, height: px as u32 },
                    Some(pixels.as_ptr() as *const _),
                    (px * 4) as u32,
                    &props,
                )
                .ok()
        })();
        let _ = DeleteObject(hbmp.into());
        result
    }
}

extern "system" fn desktop_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        // Keep the window glued to the bottom of the z-order no matter who
        // tries to raise it — this is what makes it "the desktop".
        if msg == WM_WINDOWPOSCHANGING {
            let wp = lparam.0 as *mut WINDOWPOS;
            if !wp.is_null() {
                (*wp).hwndInsertAfter = HWND_BOTTOM;
                (*wp).flags &= !SWP_NOZORDER;
            }
            return LRESULT(0);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Desktop;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let desk = &mut *ptr;
        let lx = |d: &Desktop| (lparam.0 & 0xFFFF) as i16 as f32 / d.scale;
        let ly = |d: &Desktop| ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 / d.scale;
        match msg {
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                desk.paint();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_MOUSEMOVE => {
                let (x, y) = (lx(desk), ly(desk));
                if desk.marquee.is_some() {
                    if let Some(m) = &mut desk.marquee {
                        m.x1 = x;
                        m.y1 = y;
                    }
                    desk.marquee_apply();
                    desk.paint();
                } else {
                    let h = desk.hit(x, y);
                    if h != desk.hover {
                        desk.hover = h;
                        desk.paint();
                    }
                }
                if !desk.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    if TrackMouseEvent(&mut tme).is_ok() {
                        desk.tracking = true;
                    }
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                desk.tracking = false;
                if desk.hover.take().is_some() {
                    desk.paint();
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                let (x, y) = (lx(desk), ly(desk));
                let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
                match desk.hit(x, y) {
                    Some(i) => {
                        if ctrl {
                            desk.items[i].selected = !desk.items[i].selected;
                        } else if !desk.items[i].selected {
                            for it in &mut desk.items {
                                it.selected = false;
                            }
                            desk.items[i].selected = true;
                        }
                        if msg == WM_LBUTTONDBLCLK {
                            desk.open(i);
                        }
                    }
                    None => {
                        if !ctrl {
                            for it in &mut desk.items {
                                it.selected = false;
                            }
                        }
                        desk.marquee = Some(Marquee { x0: x, y0: y, x1: x, y1: y });
                        SetCapture(hwnd);
                    }
                }
                desk.paint();
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                // Take state BEFORE ReleaseCapture (synchronous CAPTURECHANGED).
                let had = desk.marquee.take().is_some();
                let _ = ReleaseCapture();
                if had {
                    desk.paint();
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                if desk.marquee.take().is_some() {
                    desk.paint();
                }
                LRESULT(0)
            }
            WM_RBUTTONDOWN => {
                // Explorer semantics: right-press retargets the selection.
                let (x, y) = (lx(desk), ly(desk));
                match desk.hit(x, y) {
                    Some(i) if !desk.items[i].selected => {
                        for it in &mut desk.items {
                            it.selected = false;
                        }
                        desk.items[i].selected = true;
                        desk.paint();
                    }
                    None => {
                        let any = desk.items.iter().any(|it| it.selected);
                        for it in &mut desk.items {
                            it.selected = false;
                        }
                        if any {
                            desk.paint();
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let (x, y) = (lx(desk), ly(desk));
                let hit = desk.hit(x, y);
                let paths: Vec<String> = match hit {
                    Some(i) => {
                        // Whole selection, but only siblings of the clicked
                        // item — one IShellFolder serves the menu.
                        let group = desk.items[i].menu_group();
                        let mut sel: Vec<String> = desk
                            .items
                            .iter()
                            .filter(|it| it.selected && it.menu_group() == group)
                            .map(|it| it.parsing.clone())
                            .collect();
                        if sel.is_empty() {
                            sel.push(desk.items[i].parsing.clone());
                        }
                        sel
                    }
                    None => Vec::new(),
                };
                // Menus on a NOACTIVATE window only dismiss properly with
                // foreground; the user's click grants us the SFW right.
                let _ = SetForegroundWindow(hwnd);
                // TrackPopupMenuEx pumps this wndproc reentrantly — the desk
                // borrow must not live across it (last use was `paths`).
                let outcome = if hit.is_some() {
                    crate::shellmenu::show_item_menu(hwnd, &paths)
                } else {
                    match std::env::var("USERPROFILE") {
                        Ok(p) => crate::shellmenu::show_background_menu(
                            hwnd,
                            &PathBuf::from(p).join("Desktop"),
                            &BG_CUSTOM,
                        ),
                        Err(_) => return LRESULT(0),
                    }
                };
                let desk = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Desktop);
                use crate::shellmenu::MenuOutcome;
                match outcome {
                    MenuOutcome::Invoked | MenuOutcome::Custom(ID_REFRESH) => desk.refresh_all(),
                    MenuOutcome::Custom(ID_GLIDE) => open_glide(),
                    MenuOutcome::Custom(ID_DISPLAY) => open_uri("ms-settings:display"),
                    MenuOutcome::Custom(ID_PERSONAL) => open_uri("ms-settings:personalization"),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_SHELLCHANGE => {
                // The delivery is shared memory the shell allocated for us;
                // it has to be released whether or not we read it. What
                // changed does not matter — one user action arrives as a
                // burst, so coalesce and re-read the folder once.
                let mut pidls: *mut *mut ITEMIDLIST = std::ptr::null_mut();
                let mut event = 0i32;
                let lock = SHChangeNotification_Lock(
                    windows::Win32::Foundation::HANDLE(wparam.0 as *mut _),
                    lparam.0 as u32,
                    Some(&mut pidls),
                    Some(&mut event),
                );
                if !lock.is_invalid() {
                    let _ = SHChangeNotification_Unlock(lock);
                }
                let _ = SetTimer(Some(hwnd), TIMER_RELOAD, 250, None);
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_RELOAD => {
                let _ = KillTimer(Some(hwnd), TIMER_RELOAD);
                desk.refresh_all();
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                // Wallpaper or work-area changes both land here.
                desk.load_wallpaper();
                desk.load_items();
                desk.paint();
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_BOTTOM),
                    0,
                    0,
                    sw,
                    sh,
                    SWP_NOACTIVATE,
                );
                desk.w = sw as f32 / desk.scale;
                desk.h = sh as f32 / desk.scale;
                let dpi = desk.scale * 96.0;
                let _ = desk.renderer.resize(sw as u32, sh as u32, dpi);
                desk.items_sig.0.clear();
                desk.load_items();
                // Wallpaper was scaled for the old monitor size.
                desk.wall_path.clear();
                desk.load_wallpaper();
                desk.paint();
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
