//! M3 desktop — wallpaper + icon grid at the bottom of the z-order
//! (SHELL_DESIGN §6.3). One window per monitor, each covering its own screen
//! and pinned under every normal window via WM_WINDOWPOSCHANGING; alongside
//! explorer it visually replaces the stock desktop (same items, our
//! rendering), after the swap it IS the desktop.
//!
//! Scope: select (click / Ctrl / marquee), double-click open, right-click
//! shell context menu (shellmenu.rs), drag with saved positions, keyboard,
//! drop target, live folder watch. Icons live on the primary monitor, which
//! is where explorer keeps them; the others carry their own wallpaper.

use std::path::PathBuf;

use windows::Win32::Foundation::{GENERIC_READ, HWND, LPARAM, LRESULT, RECT, WPARAM};
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
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    CreateCompatibleDC, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, EnumDisplayMonitors, FF_DONTCARE, FW_NORMAL, GetDIBits, GetMonitorInfoW, HDC,
    HFONT, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow,
    OUT_DEFAULT_PRECIS, ValidateRect,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::System::Ole::{IDropTarget, OleInitialize, RegisterDragDrop};
use windows::Win32::Graphics::Imaging::{
    GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VIRTUAL_KEY, VK_CONTROL, VK_DELETE, VK_DOWN, VK_ESCAPE, VK_F2, VK_LEFT, VK_RETURN, VK_RIGHT,
    VK_SHIFT, VK_UP,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_SFUIObject, DefSubclassProc, DesktopWallpaper, FO_DELETE, FOF_ALLOWUNDO, IDesktopWallpaper,
    IShellItem, IShellItemImageFactory,
    RemoveWindowSubclass, SHCNE_ALLEVENTS, SHCNRF_InterruptLevel, SHCNRF_NewDelivery,
    SHCNRF_ShellLevel, SHChangeNotification_Lock, SHChangeNotification_Unlock, SHChangeNotifyEntry,
    SHChangeNotifyRegister, SHCreateItemFromParsingName, SHFILEOPSTRUCTW, SHFileOperationW,
    SHParseDisplayName, SIGDN_NORMALDISPLAY, SIIGBF_BIGGERSIZEOK, SIIGBF_RESIZETOFIT,
    SetWindowSubclass, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCWSTR, w};

use crate::render::{Renderer, ellipsize, fill_round, rect};
use crate::theme;

// Both live behind the windows crate's "Win32_UI_Controls" feature, which
// this crate does not take — it would pull in the whole common-control
// surface for two integers.
const WM_MOUSELEAVE: u32 = 0x02A3;
const EM_SETSEL: u32 = 0x00B1;
/// SHChangeNotify delivery — something under the desktop moved.
const WM_SHELLCHANGE: u32 = WM_APP + 1;
/// The rename box finished; wparam is 1 to keep what was typed.
const WM_RENAME_DONE: u32 = WM_APP + 2;
/// Reload debounce: one user action arrives as a burst of notifications.
const TIMER_RELOAD: usize = 1;
/// Display-change debounce — a monitor arriving is several messages, and the
/// rescan has to run off the message that reported it (see `rescan`).
const TIMER_RESCAN: usize = 2;

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
    /// Grid cell. The authority for where the item is; x/y follow from it.
    col: u32,
    row: u32,
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

/// In-place rename. A real EDIT window rather than something drawn here,
/// because a self-drawn box would have to reimplement IME composition and
/// these are Korean filenames. It is a popup and not a child: the desktop is
/// WS_EX_NOREDIRECTIONBITMAP, which has no surface for a child HWND to
/// compose into, and a child would simply not appear.
struct Rename {
    idx: usize,
    edit: HWND,
}

/// A selection being dragged across the grid.
struct Drag {
    /// Where the button went down, logical.
    ox: f32,
    oy: f32,
    /// Where the cursor is now, logical.
    x: f32,
    y: f32,
    /// Past the system drag threshold. Below it this is still a plain click,
    /// and letting go must not move anything.
    moved: bool,
}

/// A monitor, as the desktop needs it. The device name is the identity that
/// survives a display change — HMONITOR handles do not, so a window cannot be
/// matched back to its screen by handle after monitors come and go.
#[derive(Clone)]
struct Screen {
    device: String,
    rect: RECT,
    dpi: f32,
    primary: bool,
}

// Every desktop window, in creation order. One per monitor; the message loop
// they all live on is the taskbar thread, so this never leaves it.
thread_local! {
    static DESKTOPS: std::cell::RefCell<Vec<HWND>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn screens() -> Vec<Screen> {
    unsafe extern "system" fn cb(hmon: HMONITOR, _hdc: HDC, _rc: *mut RECT, l: LPARAM) -> BOOL {
        unsafe {
            let out = &mut *(l.0 as *mut Vec<Screen>);
            let mut mi = MONITORINFOEXW::default();
            mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(hmon, &mut mi as *mut _ as *mut _).as_bool() {
                let (mut dx, mut dy) = (96u32, 96u32);
                let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
                let len = mi.szDevice.iter().position(|c| *c == 0).unwrap_or(mi.szDevice.len());
                out.push(Screen {
                    device: String::from_utf16_lossy(&mi.szDevice[..len]),
                    rect: mi.monitorInfo.rcMonitor,
                    dpi: dx as f32,
                    // MONITORINFOF_PRIMARY
                    primary: mi.monitorInfo.dwFlags & 1 != 0,
                });
            }
        }
        true.into()
    }
    let mut out: Vec<Screen> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut out as *mut _ as isize));
    }
    // Lab seam: this box has one screen, and the two-window path is the whole
    // point of the change. Splitting the one it has down the middle exercises
    // it — two windows, two renderers, icons on the left half only.
    if std::env::var_os("GLIDE_DESK_SPLIT").is_some() && out.len() == 1 {
        let s = out.remove(0);
        let mid = (s.rect.left + s.rect.right) / 2;
        out.push(Screen {
            device: format!("{}-L", s.device),
            rect: RECT { right: mid, ..s.rect },
            ..s.clone()
        });
        out.push(Screen {
            device: format!("{}-R", s.device),
            rect: RECT { left: mid, ..s.rect },
            primary: false,
            ..s
        });
    }
    out
}

pub struct Desktop {
    hwnd: HWND,
    /// Which monitor this window covers, and where that monitor is — icon
    /// coordinates are window-relative, the monitor rect is not.
    device: String,
    mon: RECT,
    /// Icons belong to the primary monitor, the way explorer keeps them. The
    /// rest of the windows are wallpaper and a background menu.
    primary: bool,
    /// Whether this window holds the shell change notification. Only the
    /// primary needs one, and the primary can move between monitors.
    watching: bool,
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
    drag: Option<Drag>,
    tracking: bool,
    /// Rows per column in the current layout.
    rows: usize,
    /// Cell (0,0), logical and window-relative — inside the work area, which
    /// on this monitor need not start at the monitor's own top-left.
    origin_x: f32,
    origin_y: f32,
    /// Where the keyboard is. Clicks move it too, so F2 after a click means
    /// what the user expects.
    cursor: usize,
    rename: Option<Rename>,
    w: f32,
    h: f32,
    scale: f32,
}

/// One desktop window per monitor; they live on this thread's message loop for
/// the life of the process (leaked boxes, same as the shell itself).
pub fn spawn() -> anyhow::Result<()> {
    if std::env::var_os("GLIDE_DESK_OFF").is_some() {
        return Ok(());
    }
    unsafe {
        let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(desktop_wndproc),
            hInstance: hinstance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);
        crate::shellmenu::enable_dark_menus();

        let screens = screens();
        let mut first: anyhow::Result<()> = Ok(());
        for s in &screens {
            if let Err(e) = create(s) {
                crate::safety::note(&format!("desktop: no window on {} ({e})", s.device));
                if first.is_ok() {
                    first = Err(e);
                }
            }
        }
        // Every monitor failing is a real failure; one of several is noted and
        // the rest of the desktop still comes up.
        if DESKTOPS.with(|d| d.borrow().is_empty()) { first } else { Ok(()) }
    }
}

const CLASS: PCWSTR = w!("glide_shell_desktop");

/// Build the window for one monitor and register it.
unsafe fn create(s: &Screen) -> anyhow::Result<()> {
    unsafe {
        let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
        let (sw, sh) = (s.rect.right - s.rect.left, s.rect.bottom - s.rect.top);
        let hwnd = CreateWindowExW(
            // Activatable, unlike every other window this shell owns: keys
            // only arrive at the focus, and the desktop is a keyboard surface.
            // WM_WINDOWPOSCHANGING keeps it at the bottom regardless, which is
            // exactly what explorer's desktop is — focusable and behind
            // everything. TOOLWINDOW keeps it out of Alt+Tab.
            WS_EX_TOOLWINDOW | WS_EX_NOREDIRECTIONBITMAP,
            CLASS,
            w!("glide-shell desktop"),
            WS_POPUP,
            s.rect.left,
            s.rect.top,
            sw,
            sh,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?;

        let scale = s.dpi / 96.0;
        let renderer = Renderer::new(hwnd, sw as u32, sh as u32, s.dpi)?;

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
            hwnd,
            device: s.device.clone(),
            mon: s.rect,
            primary: s.primary,
            watching: false,
            renderer,
            fmt_label,
            wallpaper: None,
            wall_path: String::new(),
            wall_mtime: None,
            items: Vec::new(),
            items_sig: (Vec::new(), (0, 0, 0, 0)),
            hover: None,
            marquee: None,
            drag: None,
            tracking: false,
            rows: 1,
            origin_x: MARGIN,
            origin_y: 0.0,
            cursor: 0,
            rename: None,
            w: sw as f32 / scale,
            h: sh as f32 / scale,
            scale,
        });
        desk.load_wallpaper();
        desk.load_items();
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::leak(desk) as *mut Desktop as isize);
        DESKTOPS.with(|d| d.borrow_mut().push(hwnd));
        // Only after the pointer is installed: the shell can deliver the first
        // notification before this function returns, and a WM_SHELLCHANGE that
        // lands on a null userdata is a leaked delivery handle.
        let desk = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Desktop);
        if s.primary {
            watch(hwnd);
            desk.watching = true;
        }
        register_drop(hwnd);

        let _ = SetWindowPos(
            hwnd,
            Some(HWND_BOTTOM),
            s.rect.left,
            s.rect.top,
            sw,
            sh,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        eprintln!(
            "desktop: {} on {} ({}×{} @{}), {} items, wallpaper={} ({})",
            if s.primary { "primary" } else { "secondary" },
            s.device,
            sw,
            sh,
            s.dpi as u32,
            desk.items.len(),
            desk.wallpaper.is_some(),
            desk.wall_path
        );
        desk.paint();
        Ok(())
    }
}

/// Re-fit the windows to the monitors after a display change: the ones whose
/// monitor stayed are moved and rescaled, a monitor that arrived gets a new
/// window, and one that left has its window closed.
///
/// Closing goes through WM_CLOSE rather than DestroyWindow: this runs off a
/// message, and the window being retired can be the one whose wndproc is on
/// the stack — destroying it there would free the Desktop underneath the
/// frame that called us.
fn rescan() {
    let screens = screens();
    let existing = DESKTOPS.with(|d| d.borrow().clone());
    let mut fitted: Vec<&str> = Vec::new();
    for hwnd in existing {
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Desktop;
        if ptr.is_null() {
            continue;
        }
        let desk = unsafe { &mut *ptr };
        match screens.iter().find(|s| s.device == desk.device) {
            Some(s) => {
                desk.refit(s);
                fitted.push(&s.device);
            }
            None => unsafe {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            },
        }
    }
    for s in screens.iter().filter(|s| !fitted.contains(&s.device.as_str())) {
        if let Err(e) = unsafe { create(s) } {
            crate::safety::note(&format!("desktop: no window on {} ({e})", s.device));
        }
    }
}

impl Desktop {
    /// Take the geometry of the monitor this window is on, and answer with its
    /// work area. Both move under us — a bar docking, a resolution change, the
    /// monitor itself being rearranged — so nothing about them is cached past
    /// the call that needs it.
    fn work_area(&mut self) -> RECT {
        unsafe {
            let mut mi = MONITORINFOEXW::default();
            mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            let hmon = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
            if !GetMonitorInfoW(hmon, &mut mi as *mut _ as *mut _).as_bool() {
                return self.mon;
            }
            self.mon = mi.monitorInfo.rcMonitor;
            mi.monitorInfo.rcWork
        }
    }

    /// The monitor moved, resized or changed scaling: follow it, and rebuild
    /// everything that was sized against the old one.
    fn refit(&mut self, s: &Screen) {
        let (w, h) = (s.rect.right - s.rect.left, s.rect.bottom - s.rect.top);
        let same_geometry = s.rect == self.mon && s.dpi == self.scale * 96.0;
        let same_role = s.primary == self.primary;
        self.mon = s.rect;
        self.scale = s.dpi / 96.0;
        self.w = w as f32 / self.scale;
        self.h = h as f32 / self.scale;
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_BOTTOM),
                s.rect.left,
                s.rect.top,
                w,
                h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        // The icons follow the primary monitor, so a monitor that gained or
        // lost the role has to re-read the folder even if it did not move.
        self.primary = s.primary;
        if s.primary && !self.watching {
            unsafe { watch(self.hwnd) };
            self.watching = true;
        }
        if same_geometry && same_role {
            return;
        }
        if !same_geometry {
            let _ = self.renderer.resize(w as u32, h as u32, s.dpi);
        }
        self.items_sig.0.clear();
        self.load_items();
        // The wallpaper was decoded at the old monitor's size, and a monitor
        // can carry its own image.
        self.wall_path.clear();
        self.load_wallpaper();
        self.paint();
    }

    fn load_wallpaper(&mut self) {
        if std::env::var_os("GLIDE_DESK_BARE").is_some() {
            return;
        }
        unsafe {
            let path = wallpaper_path(self.mon);
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
    /// columns inside the work area. Only the primary monitor has them.
    fn load_items(&mut self) {
        if !self.primary {
            self.items.clear();
            self.items_sig = (Vec::new(), (0, 0, 0, 0));
            return;
        }
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

        // Work area of this window's monitor (explorer's bar + ours both
        // reserve; icons stay above). SPI_GETWORKAREA only knows the primary
        // one, which is no help once there are several.
        let work = self.work_area();
        let sig = (
            found.iter().map(|(p, _)| p.clone()).collect::<Vec<String>>(),
            (work.left, work.top, work.right, work.bottom),
        );
        if sig == self.items_sig && !self.items.is_empty() {
            return;
        }
        self.items_sig = sig;

        // Window-relative: the work area is in screen coordinates and this
        // window starts at its monitor's top-left, not the desktop's.
        let top = (work.top - self.mon.top) as f32 / self.scale + MARGIN;
        let bottom = (work.bottom - self.mon.top) as f32 / self.scale - MARGIN;
        self.rows = (((bottom - top) / (CELL_H + CELL_GAP)).floor() as usize).max(1);
        self.origin_x = (work.left - self.mon.left) as f32 / self.scale + MARGIN;
        self.origin_y = top;

        let px = (ICON * self.scale * 2.0) as i32; // downscale-only quality
        self.items = found
            .into_iter()
            .map(|(parsing, path)| {
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
                    col: 0,
                    row: 0,
                    x: 0.0,
                    y: 0.0,
                    selected: false,
                }
            })
            .collect();
        self.place_items();
    }

    /// Give every item a cell: the one the user dragged it to if there is a
    /// saved position for it, otherwise the first free cell in reading order.
    /// Explorer keeps this in an undocumented ItemPos blob; ours is a text
    /// file next to the settings.
    fn place_items(&mut self) {
        let saved = load_positions();
        let rows = self.rows as u32;
        let mut taken: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
        let mut homeless: Vec<usize> = Vec::new();

        for i in 0..self.items.len() {
            // A cell off the bottom means the work area shrank since it was
            // saved; that item goes back in the flow rather than off-screen.
            let cell = saved.get(&self.items[i].parsing).copied().filter(|&(_, r)| r < rows);
            match cell {
                Some(c) if taken.insert(c) => {
                    self.items[i].col = c.0;
                    self.items[i].row = c.1;
                }
                _ => homeless.push(i),
            }
        }

        let mut free = (0u32, 0u32);
        for i in homeless {
            while !taken.insert(free) {
                free = next_cell(free, rows);
            }
            self.items[i].col = free.0;
            self.items[i].row = free.1;
        }
        self.sync_cells();
    }

    /// Pixel positions follow from the grid cells, never the other way round.
    fn sync_cells(&mut self) {
        let (origin_x, origin_y) = (self.origin_x, self.origin_y);
        for it in &mut self.items {
            it.x = origin_x + it.col as f32 * (CELL_W + CELL_GAP);
            it.y = origin_y + it.row as f32 * (CELL_H + CELL_GAP);
        }
    }

    /// The cell a point falls in, rounded to whichever cell the icon's
    /// top-left is nearest — dragging aims with the icon, not the cursor.
    fn cell_at(&self, x: f32, y: f32) -> (u32, u32) {
        let col = ((x - self.origin_x) / (CELL_W + CELL_GAP)).round().max(0.0) as u32;
        let row = ((y - self.origin_y) / (CELL_H + CELL_GAP))
            .round()
            .clamp(0.0, self.rows.saturating_sub(1) as f32) as u32;
        (col, row)
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

            // A drag in flight: the selection rides with the cursor, faded, so
            // the cells it came from stay readable underneath.
            let lift = match &self.drag {
                Some(d) if d.moved => (d.x - d.ox, d.y - d.oy),
                _ => (0.0, 0.0),
            };
            for (i, item) in self.items.iter().enumerate() {
                let (lx, ly) = if item.selected { lift } else { (0.0, 0.0) };
                let alpha = if (lx, ly) == (0.0, 0.0) { 1.0 } else { 0.72 };
                let (itx, ity) = (item.x + lx, item.y + ly);
                let cell = rect(itx, ity, itx + CELL_W, ity + CELL_H);
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

                let ix = itx + (CELL_W - ICON) / 2.0;
                let iy = ity + 8.0;
                if let Some(bmp) = &item.bitmap {
                    r.dc.DrawBitmap(
                        bmp,
                        Some(&rect(ix, iy, ix + ICON, iy + ICON)),
                        alpha,
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
                let lr = rect(itx + 2.0, iy + ICON + 4.0, itx + CELL_W - 2.0, ity + CELL_H - 2.0);
                const HALO: [(f32, f32); 8] = [
                    (-1.0, -1.0), (0.0, -1.0), (1.0, -1.0),
                    (-1.0, 0.0), (1.0, 0.0),
                    (-1.0, 1.0), (0.0, 1.0), (1.0, 1.0),
                ];
                for (dx, dy) in HALO {
                    let o = rect(lr.left + dx, lr.top + dy, lr.right + dx, lr.bottom + dy);
                    self.label(&item.label, o, theme::rgba(0, 0, 0, 0.34 * alpha));
                }
                // Weight under the text so it sits on the wallpaper rather than
                // floating in a uniform outline.
                let drop = rect(lr.left, lr.top + 2.0, lr.right, lr.bottom + 2.0);
                self.label(&item.label, drop, theme::rgba(0, 0, 0, 0.35 * alpha));
                self.label(&item.label, lr, theme::rgba(244, 246, 250, alpha));
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

    /// Grid navigation over the cells themselves, not the item order — after
    /// a drag the two have nothing to do with each other. Empty cells are
    /// stepped over, so a gap left by a drag does not stop the cursor.
    fn move_cursor(&mut self, dx: isize, dy: isize) {
        if self.items.is_empty() {
            return;
        }
        let rows = self.rows as isize;
        let last_col = self.items.iter().map(|it| it.col as isize).max().unwrap_or(0);
        let cur = self.cursor.min(self.items.len() - 1);
        let (mut c, mut r) = (self.items[cur].col as isize, self.items[cur].row as isize);

        for _ in 0..=(rows * (last_col + 1)) {
            c += dx;
            r += dy;
            // Vertical movement wraps into the neighbouring column, the way a
            // column of icons reads. Horizontal movement just stops.
            if r < 0 {
                c -= 1;
                r = rows - 1;
            } else if r >= rows {
                c += 1;
                r = 0;
            }
            if c < 0 || c > last_col {
                return;
            }
            if let Some(i) =
                self.items.iter().position(|it| it.col as isize == c && it.row as isize == r)
            {
                self.cursor = i;
                for (j, it) in self.items.iter_mut().enumerate() {
                    it.selected = j == i;
                }
                return;
            }
        }
    }

    /// Commit a drag: every selected icon moves by the same delta, snapped to
    /// the grid, and anything landing on an occupied cell slides on to the
    /// next free one rather than stacking.
    fn drop_icons(&mut self) {
        let Some(d) = self.drag.take() else { return };
        if !d.moved {
            return;
        }
        let (dx, dy) = (d.x - d.ox, d.y - d.oy);
        let rows = self.rows as u32;
        let mut taken: std::collections::HashSet<(u32, u32)> =
            self.items.iter().filter(|it| !it.selected).map(|it| (it.col, it.row)).collect();

        let moving: Vec<usize> =
            (0..self.items.len()).filter(|&i| self.items[i].selected).collect();
        for i in moving {
            let mut cell = self.cell_at(self.items[i].x + dx, self.items[i].y + dy);
            while !taken.insert(cell) {
                cell = next_cell(cell, rows);
            }
            self.items[i].col = cell.0;
            self.items[i].row = cell.1;
        }
        self.sync_cells();
        save_positions(&self.items);
    }

    fn select_all(&mut self) {
        for it in &mut self.items {
            it.selected = true;
        }
    }

    fn open_selected(&self) {
        for i in 0..self.items.len() {
            if self.items[i].selected {
                self.open(i);
            }
        }
    }

    /// Recycle (or, with shift, erase) every selected item that is a file.
    /// Namespace items are not deletable and are simply skipped.
    fn delete_selected(&self, hwnd: HWND, permanent: bool) {
        // SHFileOperation takes the whole batch as one double-null-terminated
        // block, which is also why this is one call and not one per item: the
        // user gets a single undo entry, as they would from explorer.
        let mut from: Vec<u16> = Vec::new();
        for it in self.items.iter().filter(|it| it.selected) {
            let Some(p) = &it.path else { continue };
            from.extend(p.as_os_str().to_string_lossy().encode_utf16());
            from.push(0);
        }
        if from.is_empty() {
            return;
        }
        from.push(0);

        let mut op = SHFILEOPSTRUCTW {
            hwnd,
            wFunc: FO_DELETE,
            pFrom: PCWSTR(from.as_ptr()),
            fFlags: if permanent { 0 } else { FOF_ALLOWUNDO.0 as u16 },
            ..Default::default()
        };
        unsafe {
            SHFileOperationW(&mut op);
        }
        // No refresh here — the operation raises the shell notification we are
        // already listening for.
    }

    fn begin_rename(&mut self, hwnd: HWND, idx: usize) {
        let Some(item) = self.items.get(idx) else { return };
        // Namespace items rename through the shell, not the filesystem; not
        // worth the machinery for "휴지통" → something else.
        let Some(path) = item.path.clone() else { return };
        let (x, y) = (item.x, item.y + ICON + 14.0);
        self.end_rename(false);

        let name: Vec<u16> = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let (px, py) = ((x * self.scale) as i32, (y * self.scale) as i32);
        let (pw, ph) = ((CELL_W * self.scale) as i32, (22.0 * self.scale) as i32);

        let edit = unsafe {
            let Ok(edit) = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                w!("EDIT"),
                PCWSTR(name.as_ptr()),
                WS_POPUP | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                px,
                py,
                pw,
                ph,
                Some(hwnd),
                None,
                None,
                None,
            ) else {
                return;
            };
            SendMessageW(edit, WM_SETFONT, Some(WPARAM(ui_font(self.scale).0 as usize)), None);
            // Explorer selects the stem and leaves the extension alone, which
            // is the whole point of renaming in place.
            let stem = path.file_stem().map_or(0, |s| s.to_string_lossy().encode_utf16().count());
            let _ = SetWindowSubclass(edit, Some(rename_proc), 0x0D17, hwnd.0 as usize);
            let _ = SetForegroundWindow(edit);
            let _ = SetFocus(Some(edit));
            SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(stem as isize)));
            edit
        };
        self.rename = Some(Rename { idx, edit });
    }

    fn end_rename(&mut self, commit: bool) {
        let Some(r) = self.rename.take() else { return };
        let mut buf = [0u16; 512];
        let len = unsafe { GetWindowTextW(r.edit, &mut buf) } as usize;
        unsafe {
            let _ = RemoveWindowSubclass(r.edit, Some(rename_proc), 0x0D17);
            let _ = DestroyWindow(r.edit);
        }
        if !commit || len == 0 {
            return;
        }
        let typed = String::from_utf16_lossy(&buf[..len]);
        let Some(path) = self.items.get(r.idx).and_then(|it| it.path.clone()) else { return };
        if path.file_name().is_some_and(|n| n.to_string_lossy() == typed) {
            return;
        }
        let target = path.with_file_name(&typed);
        if let Err(e) = std::fs::rename(&path, &target) {
            crate::safety::note(&format!("desktop: rename to {typed} failed: {e}"));
        }
        // The rename raises a shell notification; the refresh rides on that.
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

/// The rename box, told about the two keys an EDIT does not handle. Both
/// answers are posted rather than acted on here, because acting on them
/// destroys this very window from inside its own message handler.
unsafe extern "system" fn rename_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    owner: usize,
) -> LRESULT {
    unsafe {
        let desk = HWND(owner as *mut core::ffi::c_void);
        match msg {
            WM_KEYDOWN if wparam.0 as u16 == VK_RETURN.0 || wparam.0 as u16 == VK_ESCAPE.0 => {
                let commit = wparam.0 as u16 == VK_RETURN.0;
                let _ = PostMessageW(Some(desk), WM_RENAME_DONE, WPARAM(commit as usize), LPARAM(0));
                LRESULT(0)
            }
            // A single-line EDIT beeps at these; it was never asked to be a
            // dialog and there is no default button to press.
            WM_CHAR if wparam.0 as u16 == 0x0D || wparam.0 as u16 == 0x1B => LRESULT(0),
            WM_KILLFOCUS => {
                // Clicking away keeps the edit, same as explorer.
                let _ = PostMessageW(Some(desk), WM_RENAME_DONE, WPARAM(1), LPARAM(0));
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

/// One shared UI font for the rename box, made on first use and kept — it
/// outlives every rename and there is only ever one desktop.
fn ui_font(scale: f32) -> HFONT {
    use std::sync::OnceLock;
    static FONT: OnceLock<isize> = OnceLock::new();
    HFONT(*FONT.get_or_init(|| unsafe {
        CreateFontW(
            -((12.0 * scale) as i32),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            w!("Segoe UI"),
        )
        .0 as isize
    }) as *mut core::ffi::c_void)
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

/// Reading order down a column, then on to the next.
fn next_cell((col, row): (u32, u32), rows: u32) -> (u32, u32) {
    if row + 1 < rows { (col, row + 1) } else { (col + 1, 0) }
}

fn positions_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("glide-shell").join("desktop-icons.txt")
}

/// `col,row=parsing name`, one per line. The name goes last because it is the
/// only field that can contain anything.
fn load_positions() -> std::collections::HashMap<String, (u32, u32)> {
    let mut out = std::collections::HashMap::new();
    let Ok(txt) = std::fs::read_to_string(positions_path()) else { return out };
    for line in txt.lines() {
        let Some((cell, name)) = line.split_once('=') else { continue };
        let Some((c, r)) = cell.split_once(',') else { continue };
        let (Ok(c), Ok(r)) = (c.trim().parse(), r.trim().parse()) else { continue };
        out.insert(name.to_string(), (c, r));
    }
    out
}

fn save_positions(items: &[Item]) {
    let p = positions_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut txt = String::new();
    for it in items {
        txt.push_str(&format!("{},{}={}\n", it.col, it.row, it.parsing));
    }
    let _ = std::fs::write(p, txt);
}

/// Make the desktop accept drops, by handing the job to the shell rather than
/// implementing IDropTarget here. The Desktop folder's own drop target already
/// knows copy against move against link, what the modifier keys mean, what to
/// do with a .lnk and what to do with a URL; anything written here would be a
/// worse version of it.
unsafe fn register_drop(hwnd: HWND) {
    unsafe {
        let Some(dir) = std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Desktop"))
        else {
            return;
        };
        let w: Vec<u16> = dir
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let result = (|| -> windows::core::Result<()> {
            // RegisterDragDrop wants OLE, and the thread has only had
            // CoInitializeEx. Already-STA makes this the cheap half of
            // OleInitialize rather than a second apartment.
            OleInitialize(None)?;
            let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None)?;
            let target: IDropTarget = item.BindToHandler(None, &BHID_SFUIObject)?;
            RegisterDragDrop(hwnd, &target)
        })();
        if let Err(e) = result {
            crate::safety::note(&format!("desktop: no drop target ({e})"));
        }
    }
}

/// Is this namespace root on the desktop? `default_on` stands in for the
/// value explorer has never written.
/// The wallpaper image for one monitor. Windows holds a separate one per
/// monitor and SPI_GETDESKWALLPAPER answers with only one of them, so ask the
/// wallpaper service; its monitor IDs come in an order of their own, so the
/// one we want is found by rect. Falls back to SPI when the service says
/// nothing useful — a spanned image, or no per-monitor entry at all.
fn wallpaper_path(mon: RECT) -> String {
    unsafe {
        if let Ok(dw) = CoCreateInstance::<_, IDesktopWallpaper>(&DesktopWallpaper, None, CLSCTX_ALL)
        {
            for i in 0..dw.GetMonitorDevicePathCount().unwrap_or(0) {
                let Ok(id) = dw.GetMonitorDevicePathAt(i) else { continue };
                let mine = dw.GetMonitorRECT(PCWSTR(id.0)).is_ok_and(|r| r == mon);
                let found = mine.then(|| dw.GetWallpaper(PCWSTR(id.0)).ok()).flatten();
                CoTaskMemFree(Some(id.0 as *const _));
                if let Some(p) = found {
                    let path = p.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(p.0 as *const _));
                    if !path.is_empty() {
                        return path;
                    }
                }
            }
        }
        let mut buf = [0u16; 512];
        let _ = SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buf.len() as u32,
            Some(buf.as_mut_ptr() as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }
}

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
        // Before the Desktop is borrowed: the rescan reaches into every one of
        // them, including this window's, and may retire this very window.
        if msg == WM_TIMER && wparam.0 == TIMER_RESCAN {
            let _ = KillTimer(Some(hwnd), TIMER_RESCAN);
            rescan();
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
            WM_MOUSEMOVE => {
                let (x, y) = (lx(desk), ly(desk));
                if desk.drag.is_some() {
                    // The threshold is the system's, so a click with a shaky
                    // hand stays a click.
                    let (tx, ty) = (
                        GetSystemMetrics(SM_CXDRAG) as f32 / desk.scale,
                        GetSystemMetrics(SM_CYDRAG) as f32 / desk.scale,
                    );
                    if let Some(d) = &mut desk.drag {
                        d.x = x;
                        d.y = y;
                        d.moved |= (x - d.ox).abs() > tx || (y - d.oy).abs() > ty;
                    }
                    desk.paint();
                } else if desk.marquee.is_some() {
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
                        desk.cursor = i;
                        if msg == WM_LBUTTONDBLCLK {
                            desk.open(i);
                        } else {
                            desk.drag =
                                Some(Drag { ox: x, oy: y, x, y, moved: false });
                            SetCapture(hwnd);
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
                let had = desk.marquee.take().is_some() || desk.drag.is_some();
                desk.drop_icons();
                let _ = ReleaseCapture();
                if had {
                    desk.paint();
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                // Capture lost rather than released — abandon the drag where
                // it started instead of dropping icons somewhere arbitrary.
                if desk.marquee.take().is_some() || desk.drag.take().is_some() {
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
            WM_KEYDOWN => {
                let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
                let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
                let vk = VIRTUAL_KEY(wparam.0 as u16);
                match vk {
                    VK_LEFT => desk.move_cursor(-1, 0),
                    VK_RIGHT => desk.move_cursor(1, 0),
                    VK_UP => desk.move_cursor(0, -1),
                    VK_DOWN => desk.move_cursor(0, 1),
                    VK_RETURN => desk.open_selected(),
                    VK_F2 => {
                        let idx = desk.cursor;
                        desk.begin_rename(hwnd, idx);
                        return LRESULT(0);
                    }
                    VK_DELETE => desk.delete_selected(hwnd, shift),
                    VK_ESCAPE => {
                        for it in &mut desk.items {
                            it.selected = false;
                        }
                    }
                    // 'A'. There is no VK constant for the letter keys.
                    VIRTUAL_KEY(0x41) if ctrl => desk.select_all(),
                    _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
                }
                desk.paint();
                LRESULT(0)
            }
            WM_RENAME_DONE => {
                desk.end_rename(wparam.0 != 0);
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
                // A monitor arriving or leaving is a burst of these, one to
                // every window; coalesce and let a single rescan sort out
                // which windows the new arrangement wants.
                let _ = SetTimer(Some(hwnd), TIMER_RESCAN, 400, None);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                // The suggested rect is for a window being dragged between
                // monitors; this one covers its monitor and stays there, so
                // only the scaling is news.
                let dpi = (wparam.0 & 0xFFFF) as f32;
                let s = Screen {
                    device: desk.device.clone(),
                    rect: desk.mon,
                    dpi,
                    primary: desk.primary,
                };
                desk.refit(&s);
                LRESULT(0)
            }
            WM_DESTROY => {
                DESKTOPS.with(|d| d.borrow_mut().retain(|&h| h != hwnd));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
