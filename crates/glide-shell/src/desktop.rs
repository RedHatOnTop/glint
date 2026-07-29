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

use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, GENERIC_READ, HGLOBAL, HWND,
    LPARAM, LRESULT, POINTL, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_PROPERTIES1, D2D1_DRAW_TEXT_OPTIONS_CLIP,
    D2D1_INTERPOLATION_MODE_LINEAR, D2D1_ROUNDED_RECT, ID2D1Bitmap1,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_METRICS,
    IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    CreateCompatibleDC, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, EnumDisplayMonitors, FF_DONTCARE, FW_NORMAL, GetDIBits, GetMonitorInfoW, HDC,
    HFONT, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow,
    OUT_DEFAULT_PRECIS, ValidateRect,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, DVASPECT_CONTENT, FORMATETC, IDataObject,
    STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::SystemServices::{
    MK_CONTROL, MK_LBUTTON, MK_RBUTTON, MK_SHIFT, MODIFIERKEYS_FLAGS,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE, DoDragDrop,
    IDropSource, IDropSource_Impl, IDropTarget, OleFlushClipboard, OleGetClipboard, OleInitialize,
    OleSetClipboard, RegisterDragDrop, ReleaseStgMedium,
};
use windows::Win32::Graphics::Imaging::{
    GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VIRTUAL_KEY, VK_CONTROL, VK_DELETE, VK_DOWN, VK_ESCAPE, VK_F2, VK_F5, VK_LEFT, VK_RETURN,
    VK_RIGHT, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, BHID_SFUIObject, DefSubclassProc, DesktopWallpaper, FO_DELETE, FOF_ALLOWUNDO,
    IDesktopWallpaper, IShellItem, IShellItemImageFactory, SHCreateShellItemArrayFromIDLists,
    RemoveWindowSubclass, SHCNE_ALLEVENTS, SHCNRF_InterruptLevel, SHCNRF_NewDelivery,
    SHCNRF_ShellLevel, SHChangeNotification_Lock, SHChangeNotification_Unlock, SHChangeNotifyEntry,
    SHChangeNotifyRegister, SHCreateItemFromParsingName, SHFILEOPSTRUCTW, SHFileOperationW,
    SHParseDisplayName, SIGDN_NORMALDISPLAY, SIIGBF_BIGGERSIZEOK, SIIGBF_RESIZETOFIT,
    SetWindowSubclass, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HRESULT, PCWSTR, implement, w};
use windows_numerics::Vector2;

use crate::render::{Renderer, ellipsize, fill_round, rect};
use crate::theme;

// Both live behind the windows crate's "Win32_UI_Controls" feature, which
// this crate does not take — it would pull in the whole common-control
// surface for two integers.
const WM_MOUSELEAVE: u32 = 0x02A3;
const EM_SETSEL: u32 = 0x00B1;

// The clipboard's cut-or-copy flag is one DWORD in an HGLOBAL, and the global
// heap sits behind "Win32_System_Memory". Declared here for the same reason as
// the two constants above: a whole feature for three calls is not a trade.
const GMEM_MOVEABLE: u32 = 0x0002;
unsafe extern "system" {
    fn GlobalAlloc(uflags: u32, dwbytes: usize) -> HGLOBAL;
    fn GlobalLock(hmem: HGLOBAL) -> *mut core::ffi::c_void;
    fn GlobalUnlock(hmem: HGLOBAL) -> BOOL;
}
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
const ID_AUTO_ARRANGE: u32 = 5;
const ID_SHOW_ICONS: u32 = 6;
/// Icon edge in logical px, in the order explorer lists the sizes. The cell
/// and the label box follow from it, so this one number is the whole setting.
const ICON_SIZES: [(u32, &str, f32); 3] =
    [(10, "큰 아이콘", 96.0), (11, "보통 아이콘", 48.0), (12, "작은 아이콘", 32.0)];
const SORTS: [(u32, &str, Sort); 4] = [
    (20, "이름", Sort::Name),
    (21, "크기", Sort::Size),
    (22, "항목 유형", Sort::Kind),
    (23, "수정한 날짜", Sort::Modified),
];

const CELL_GAP: f32 = 6.0;
/// Cell padding around the icon: sides, and below it for two lines of label.
const CELL_PAD_X: f32 = 36.0;
const CELL_PAD_Y: f32 = 54.0;
const MARGIN: f32 = 18.0;
/// Type-ahead window. Explorer uses a second; a shell that redraws the whole
/// desktop per keystroke is better off forgiving a slower typist.
const TYPE_AHEAD_MS: u32 = 1200;

/// Icon cell design (SHELL_DESIGN §5). Explorer rings its labels in a black
/// halo and washes the whole cell in accent when selected; both are its look,
/// not ours. A name here rides on a small rounded slab cut from the same
/// surface as the bar and the menus, and a selected cell is that surface too,
/// with the accent hairline every other panel wears along its top edge.
const CARD_RADIUS: f32 = 10.0;
const CHIP_RADIUS: f32 = 7.0;
const CHIP_PAD_X: f32 = 7.0;
const CHIP_PAD_Y: f32 = 3.0;
/// Icon bottom to the top of the chip. The cell padding below the icon has to
/// leave this plus two lines of text plus the chip's own padding, or a name
/// that wrapped before is trimmed to one line and an ellipsis.
const LABEL_GAP: f32 = 5.0;
/// Chip surface: darker and denser than BAR_BG, because it sits directly on a
/// photograph rather than on the acrylic the bar has under it.
const CHIP_BG: D2D1_COLOR_F = theme::rgba(16, 17, 21, 0.52);
const CHIP_BG_HOVER: D2D1_COLOR_F = theme::rgba(16, 17, 21, 0.66);
const CARD_BG: D2D1_COLOR_F = theme::rgba(26, 27, 32, 0.72);
/// Ink on the accent chip. The accents are bright enough that white text on
/// them is the unreadable combination, not the safe one.
const CHIP_INK_SELECTED: D2D1_COLOR_F = theme::rgba(10, 14, 16, 1.0);

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
    /// Laid-out label, kept because the chip behind it is sized from the text
    /// and measuring on every frame would mean a layout per icon per paint.
    /// Rebuilt when the cell width it was measured against changes.
    text: Option<Label>,
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

/// A label measured once: the layout to draw and the box it actually fills.
struct Label {
    layout: IDWriteTextLayout,
    /// Cell width this was measured against; a different one invalidates it.
    max_w: f32,
    w: f32,
    h: f32,
}

impl Label {
    fn measure(
        dwrite: &IDWriteFactory,
        fmt: &IDWriteTextFormat,
        text: &[u16],
        max_w: f32,
        max_h: f32,
    ) -> Option<Label> {
        unsafe {
            let layout = dwrite.CreateTextLayout(text, fmt, max_w, max_h.max(1.0)).ok()?;
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m).ok()?;
            Some(Label {
                layout,
                max_w,
                // Trailing whitespace is inside the line but not under the
                // glyphs, and a chip sized to include it looks off-centre.
                w: m.width.min(max_w),
                h: m.height.min(max_h.max(1.0)),
            })
        }
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Name,
    Size,
    Kind,
    Modified,
}

/// What the 보기 and 정렬 기준 menus set. Explorer keeps the equivalent in its
/// own shell bag; ours is a text file beside the icon positions.
#[derive(Clone, Copy, PartialEq)]
struct View {
    icon: f32,
    sort: Sort,
    /// Icons pack in sort order and stay packed — a dragged icon comes back.
    auto_arrange: bool,
    show_icons: bool,
}

impl Default for View {
    fn default() -> Self {
        Self { icon: 48.0, sort: Sort::Name, auto_arrange: false, show_icons: true }
    }
}

impl View {
    fn load() -> Self {
        let mut v = Self::default();
        let Ok(txt) = std::fs::read_to_string(view_path()) else { return v };
        for line in txt.lines() {
            let Some((k, val)) = line.split_once('=') else { continue };
            match k.trim() {
                "icon" => {
                    // Only the three the menu offers; anything else is a hand
                    // edit and the default is safer than an unreachable size.
                    if let Ok(px) = val.trim().parse::<f32>()
                        && ICON_SIZES.iter().any(|(_, _, s)| *s == px)
                    {
                        v.icon = px;
                    }
                }
                "sort" => {
                    v.sort = match val.trim() {
                        "size" => Sort::Size,
                        "kind" => Sort::Kind,
                        "modified" => Sort::Modified,
                        _ => Sort::Name,
                    }
                }
                "auto_arrange" => v.auto_arrange = val.trim() == "1",
                "show_icons" => v.show_icons = val.trim() == "1",
                _ => {}
            }
        }
        v
    }

    fn save(&self) {
        let p = view_path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let sort = match self.sort {
            Sort::Name => "name",
            Sort::Size => "size",
            Sort::Kind => "kind",
            Sort::Modified => "modified",
        };
        let b = |v: bool| if v { "1" } else { "0" };
        let _ = std::fs::write(
            p,
            format!(
                "icon={}\nsort={}\nauto_arrange={}\nshow_icons={}\n",
                self.icon,
                sort,
                b(self.auto_arrange),
                b(self.show_icons)
            ),
        );
    }
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
    /// The far end of a Shift+arrow range. Every plain move and every click
    /// re-drops it where the cursor lands.
    anchor: usize,
    /// Type-ahead: the prefix typed so far, and when the last key landed. A
    /// pause longer than TYPE_AHEAD_MS starts a new word.
    typed: String,
    typed_at: std::time::Instant,
    /// Parsing names put on the clipboard by Ctrl+X. They stay on the desktop
    /// until something pastes them, drawn faded — the only sign a cut is
    /// pending, since the clipboard itself says nothing on screen.
    cut: Vec<String>,
    rename: Option<Rename>,
    view: View,
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
            anchor: 0,
            typed: String::new(),
            typed_at: std::time::Instant::now(),
            cut: Vec::new(),
            rename: None,
            view: View::load(),
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
    /// Cell size, which is the icon plus room for two lines of label. Every
    /// layout and hit test goes through these rather than a constant, since
    /// the icon size is a menu option.
    fn cell_w(&self) -> f32 {
        self.view.icon + CELL_PAD_X
    }

    fn cell_h(&self) -> f32 {
        self.view.icon + CELL_PAD_Y
    }

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
        if !self.primary || !self.view.show_icons {
            self.items.clear();
            self.items_sig = (Vec::new(), (0, 0, 0, 0));
            return;
        }
        let mut found: Vec<(String, Option<PathBuf>)> = NAMESPACE_ITEMS
            .iter()
            .filter(|(clsid, default_on)| namespace_visible(clsid, *default_on))
            .map(|(clsid, _)| (format!("::{clsid}"), None))
            .collect();

        struct Entry {
            path: PathBuf,
            dir: bool,
            size: u64,
            modified: std::time::SystemTime,
        }
        let mut files: Vec<Entry> = Vec::new();
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
                files.push(Entry {
                    path,
                    dir: meta.is_dir(),
                    size: meta.len(),
                    modified: meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                });
            }
        }
        let name_of = |e: &Entry| {
            e.path.file_name().unwrap_or_default().to_string_lossy().to_lowercase()
        };
        let sort = self.view.sort;
        // Folders lead in every order, the way explorer groups them, and the
        // name breaks every tie so the layout is stable between reads.
        files.sort_by(|a, b| {
            b.dir.cmp(&a.dir).then_with(|| {
                let by_key = match sort {
                    Sort::Name => std::cmp::Ordering::Equal,
                    Sort::Size => a.size.cmp(&b.size),
                    Sort::Kind => {
                        let ext = |e: &Entry| {
                            e.path
                                .extension()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_lowercase()
                        };
                        ext(a).cmp(&ext(b))
                    }
                    // Newest first: on a desktop the recent file is the one
                    // being looked for.
                    Sort::Modified => b.modified.cmp(&a.modified),
                };
                by_key.then_with(|| name_of(a).cmp(&name_of(b)))
            })
        });
        found.extend(
            files
                .into_iter()
                .map(|e| (e.path.as_os_str().to_string_lossy().into_owned(), Some(e.path))),
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
        self.rows = (((bottom - top) / (self.cell_h() + CELL_GAP)).floor() as usize).max(1);
        self.origin_x = (work.left - self.mon.left) as f32 / self.scale + MARGIN;
        self.origin_y = top;

        // Downscale-only quality.
        let px = (self.view.icon * self.scale * 2.0) as i32;
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
                    text: None,
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
    /// file next to the settings. Auto-arrange throws the saved ones away and
    /// packs the sorted order instead.
    fn place_items(&mut self) {
        let saved = if self.view.auto_arrange { Default::default() } else { load_positions() };
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
        let (step_x, step_y) = (self.cell_w() + CELL_GAP, self.cell_h() + CELL_GAP);
        for it in &mut self.items {
            it.x = origin_x + it.col as f32 * step_x;
            it.y = origin_y + it.row as f32 * step_y;
        }
    }

    /// The cell a point falls in, rounded to whichever cell the icon's
    /// top-left is nearest — dragging aims with the icon, not the cursor.
    fn cell_at(&self, x: f32, y: f32) -> (u32, u32) {
        let col = ((x - self.origin_x) / (self.cell_w() + CELL_GAP)).round().max(0.0) as u32;
        let row = ((y - self.origin_y) / (self.cell_h() + CELL_GAP))
            .round()
            .clamp(0.0, self.rows.saturating_sub(1) as f32) as u32;
        (col, row)
    }

    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        let (cw, ch) = (self.cell_w(), self.cell_h());
        self.items.iter().position(|i| x >= i.x && x < i.x + cw && y >= i.y && y < i.y + ch)
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
            let (cw, ch, icon) = (self.cell_w(), self.cell_h(), self.view.icon);
            let hair = 1.0 / self.scale;
            let label_box = cw - 2.0 * CHIP_PAD_X - 4.0;
            let label_h = ch - 8.0 - icon - LABEL_GAP - 2.0 * CHIP_PAD_Y;
            // Measuring is a pre-pass: the chip is sized from the laid-out
            // text, and the draw loop only borrows the items.
            let (dwrite, fmt) = (self.renderer.dwrite.clone(), self.fmt_label.clone());
            for item in &mut self.items {
                if item.text.as_ref().is_some_and(|t| t.max_w == label_box) {
                    continue;
                }
                item.text = Label::measure(&dwrite, &fmt, &item.label, label_box, label_h);
            }
            for (i, item) in self.items.iter().enumerate() {
                let (lx, ly) = if item.selected { lift } else { (0.0, 0.0) };
                let mut alpha = if (lx, ly) == (0.0, 0.0) { 1.0 } else { 0.72 };
                if self.cut.contains(&item.parsing) {
                    alpha *= 0.45;
                }
                let (itx, ity) = (item.x + lx, item.y + ly);
                let cell = rect(itx, ity, itx + cw, ity + ch);
                let hovered = self.hover == Some(i);
                if item.selected {
                    fill_round(r, cell, CARD_RADIUS, theme::with_alpha(CARD_BG, CARD_BG.a * alpha));
                    // Inset by the corner radius so the line ends where the
                    // curve starts instead of overhanging it.
                    fill_round(
                        r,
                        rect(
                            cell.left + CARD_RADIUS,
                            cell.top,
                            cell.right - CARD_RADIUS,
                            cell.top + hair,
                        ),
                        0.0,
                        theme::with_alpha(theme::accent(), alpha),
                    );
                } else if hovered {
                    fill_round(r, cell, CARD_RADIUS, theme::HOVER_FILL);
                }

                let ix = itx + (cw - icon) / 2.0;
                let iy = ity + 8.0;
                if let Some(bmp) = &item.bitmap {
                    r.dc.DrawBitmap(
                        bmp,
                        Some(&rect(ix, iy, ix + icon, iy + icon)),
                        alpha,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        None,
                        None,
                    );
                } else if let Ok(b) = r.brush(theme::rgba(255, 255, 255, 0.12)) {
                    r.dc.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: rect(ix, iy, ix + icon, iy + icon),
                            radiusX: 8.0,
                            radiusY: 8.0,
                        },
                        &b,
                    );
                }

                // Label: a chip, not a halo. Ringing the glyphs in black is how
                // explorer survives a bright wallpaper; a slab does it in one
                // draw, holds any accent behind selected text, and belongs to
                // the same family as the bar and the menus.
                let Some(t) = &item.text else { continue };
                let chip_w = (t.w + 2.0 * CHIP_PAD_X).min(cw - 4.0);
                let chip_x = itx + (cw - chip_w) / 2.0;
                let chip_y = iy + icon + LABEL_GAP;
                let chip_h = t.h + 2.0 * CHIP_PAD_Y;
                let chip = rect(chip_x, chip_y, chip_x + chip_w, chip_y + chip_h);
                let (fill, ink) = if item.selected {
                    (theme::with_alpha(theme::accent(), 0.92), CHIP_INK_SELECTED)
                } else if hovered {
                    (CHIP_BG_HOVER, theme::TEXT)
                } else {
                    (CHIP_BG, theme::TEXT)
                };
                fill_round(
                    r,
                    chip,
                    CHIP_RADIUS.min(chip_h / 2.0),
                    theme::with_alpha(fill, fill.a * alpha),
                );
                if let Ok(b) = r.brush(theme::with_alpha(ink, alpha)) {
                    r.dc.DrawTextLayout(
                        Vector2 {
                            X: itx + (cw - t.max_w) / 2.0,
                            Y: chip_y + CHIP_PAD_Y,
                        },
                        &t.layout,
                        &b,
                        D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    );
                }
            }

            if let Some(m) = &self.marquee {
                let sel = rect(m.x0.min(m.x1), m.y0.min(m.y1), m.x0.max(m.x1), m.y0.max(m.y1));
                fill_round(r, sel, 4.0, theme::with_alpha(theme::accent(), 0.14));
                if let Ok(b) = r.brush(theme::with_alpha(theme::accent(), 0.65)) {
                    r.dc.DrawRoundedRectangle(
                        &D2D1_ROUNDED_RECT { rect: sel, radiusX: 4.0, radiusY: 4.0 },
                        &b,
                        hair,
                        None,
                    );
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

    /// Our half of the background menu: what explorer puts above the shell's
    /// own entries, with the current view reflected in the checks.
    fn bg_menu(&self) -> Vec<crate::shellmenu::CustomItem> {
        use crate::shellmenu::CustomItem as Item;
        let mut view: Vec<Item> = ICON_SIZES
            .iter()
            .map(|(id, label, px)| Item::radio(*id, label, self.view.icon == *px))
            .collect();
        view.push(Item::sep());
        view.push(Item::check(ID_AUTO_ARRANGE, "아이콘 자동 정렬", self.view.auto_arrange));
        view.push(Item::check(ID_SHOW_ICONS, "바탕 화면 아이콘 표시", self.view.show_icons));

        let sort: Vec<Item> = SORTS
            .iter()
            .map(|(id, label, s)| Item::radio(*id, label, self.view.sort == *s))
            .collect();

        vec![
            Item::submenu("보기", view),
            Item::submenu("정렬 기준", sort),
            Item::sep(),
            Item::new(ID_REFRESH, "새로 고침"),
            Item::new(ID_GLIDE, "glide로 열기"),
            Item::sep(),
            Item::new(ID_DISPLAY, "디스플레이 설정"),
            Item::new(ID_PERSONAL, "개인 설정"),
        ]
    }

    /// A view option changed: the icons have to be extracted again at the new
    /// size and the grid rebuilt, which is what clearing the signature buys.
    fn apply_view(&mut self, repack: bool) {
        self.view.save();
        self.items_sig.0.clear();
        self.load_items();
        if repack {
            // Sorting, or switching auto-arrange on, overrides where things
            // were dragged to — the same thing explorer does.
            self.pack_all();
        }
        self.paint();
    }

    /// Lay every item out in sort order from the first cell, and make that the
    /// saved arrangement.
    fn pack_all(&mut self) {
        let rows = self.rows.max(1) as u32;
        for (i, it) in self.items.iter_mut().enumerate() {
            let i = i as u32;
            it.col = i / rows;
            it.row = i % rows;
        }
        self.sync_cells();
        if !self.view.auto_arrange {
            save_positions(&self.items);
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
    ///
    /// `extend` is Shift held: the selection becomes every item inside the
    /// block of cells the anchor and the cursor span. A list would extend
    /// along its one order; this is a grid, and the rectangle is the only
    /// reading of "everything between here and there" that survives a drag.
    fn move_cursor(&mut self, dx: isize, dy: isize, extend: bool) {
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
                if extend {
                    self.select_block();
                } else {
                    self.anchor = i;
                    for (j, it) in self.items.iter_mut().enumerate() {
                        it.selected = j == i;
                    }
                }
                return;
            }
        }
    }

    /// Select every item in the block of cells the anchor and cursor corner.
    fn select_block(&mut self) {
        let (Some(a), Some(c)) = (self.items.get(self.anchor), self.items.get(self.cursor)) else {
            return;
        };
        let (c0, c1) = (a.col.min(c.col), a.col.max(c.col));
        let (r0, r1) = (a.row.min(c.row), a.row.max(c.row));
        for it in &mut self.items {
            it.selected = (c0..=c1).contains(&it.col) && (r0..=r1).contains(&it.row);
        }
    }

    /// Type-ahead: jump to the first item whose name starts with what has been
    /// typed. Matching is on the label rather than the filename, because that
    /// is what is on screen — and it is the localized one for 휴지통.
    fn type_ahead(&mut self, ch: char) {
        if self.typed_at.elapsed().as_millis() as u32 > TYPE_AHEAD_MS {
            self.typed.clear();
        }
        self.typed_at = std::time::Instant::now();
        // The same letter again means "next item starting with it", which is
        // only distinguishable from a prefix while the prefix is one letter.
        let repeat = self.typed.chars().next() == Some(ch) && self.typed.chars().count() == 1;
        if !repeat {
            self.typed.push(ch);
        }
        let needle = self.typed.to_lowercase();
        let start = if repeat { self.cursor + 1 } else { 0 };
        let n = self.items.len();
        for k in 0..n {
            let i = (start + k) % n;
            let label = String::from_utf16_lossy(&self.items[i].label).to_lowercase();
            if label.starts_with(&needle) {
                self.cursor = i;
                self.anchor = i;
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
        // Auto-arrange owns the layout: the icon snaps back to the cell the
        // packing gave it, which is where the cells still say it is.
        if !d.moved || self.view.auto_arrange {
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

    /// The selection as the shell's own data object — CF_HDROP and every other
    /// format an app might ask for. Same principle as the drop target: the
    /// shell already builds one, and anything written here would be a worse
    /// version of it.
    fn selection_data(&self) -> Option<IDataObject> {
        let mut pidls: Vec<*const ITEMIDLIST> = Vec::new();
        for it in self.items.iter().filter(|it| it.selected) {
            let w: Vec<u16> = it.parsing.encode_utf16().chain(std::iter::once(0)).collect();
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            unsafe {
                if SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None).is_ok() {
                    pidls.push(pidl as *const _);
                }
            }
        }
        if pidls.is_empty() {
            return None;
        }
        let data = unsafe {
            SHCreateShellItemArrayFromIDLists(&pidls)
                .ok()
                .and_then(|arr| arr.BindToHandler(None, &BHID_DataObject).ok())
        };
        for p in pidls {
            unsafe { CoTaskMemFree(Some(p as *const _)) };
        }
        data
    }

    /// Ctrl+C and Ctrl+X. Which of the two it was rides on the clipboard as
    /// the shell's "Preferred DropEffect" format, which is where every file
    /// manager on the machine looks for it — including the one pasting into a
    /// folder window that has nothing to do with us.
    fn clipboard_put(&mut self, cut: bool) {
        let Some(data) = self.selection_data() else { return };
        let effect = if cut { DROPEFFECT_MOVE } else { DROPEFFECT_COPY };
        unsafe {
            set_preferred_effect(&data, effect);
            if OleSetClipboard(&data).is_err() {
                return;
            }
            // Rendered now rather than on demand: a shell that dies holding a
            // delayed render takes the clipboard down with it.
            let _ = OleFlushClipboard();
        }
        self.cut = if cut {
            self.items.iter().filter(|it| it.selected).map(|it| it.parsing.clone()).collect()
        } else {
            Vec::new()
        };
    }

    /// Ctrl+V. The paste is a drop: the Desktop folder's own drop target is
    /// handed the clipboard's data object, so collisions, .lnk files and
    /// cross-volume moves behave exactly as they do for a real drag. The
    /// modifier is stated rather than left to the target's default, which
    /// would turn a copy into a move whenever the source is on this volume.
    fn clipboard_paste(&mut self) {
        unsafe {
            let Ok(data) = OleGetClipboard() else { return };
            let move_it = preferred_effect(&data) == DROPEFFECT_MOVE;
            let Some(dir) = desktop_dir() else { return };
            let w: Vec<u16> =
                dir.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
            let target = (|| -> windows::core::Result<IDropTarget> {
                let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None)?;
                item.BindToHandler(None, &BHID_SFUIObject)
            })();
            let Ok(target) = target else { return };

            let mut pt = windows::Win32::Foundation::POINT::default();
            let _ = GetCursorPos(&mut pt);
            let at = POINTL { x: pt.x, y: pt.y };
            // MK_LBUTTON is not decoration: a drop with no button in the key
            // state is a right-drag as far as the shell target is concerned,
            // and it answers with the 여기에 복사 / 취소 menu instead of pasting.
            let keys = MODIFIERKEYS_FLAGS(
                MK_LBUTTON.0 | if move_it { MK_SHIFT.0 } else { MK_CONTROL.0 },
            );
            let mut effect = if move_it { DROPEFFECT_MOVE } else { DROPEFFECT_COPY };
            if target.DragEnter(&data, keys, at, &mut effect).is_err() {
                return;
            }
            let _ = target.DragOver(keys, at, &mut effect);
            if effect == DROPEFFECT(0) {
                let _ = target.DragLeave();
                return;
            }
            if let Err(e) = target.Drop(&data, keys, at, &mut effect) {
                crate::safety::note(&format!("desktop: paste failed ({e})"));
                return;
            }
            // A cut is spent once it lands, the same as everywhere else.
            if move_it {
                let _ = OleSetClipboard(None);
                self.cut.clear();
            }
        }
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
        let (x, y) = (item.x, item.y + self.view.icon + 14.0);
        self.end_rename(false);

        let name: Vec<u16> = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let (px, py) = ((x * self.scale) as i32, (y * self.scale) as i32);
        let (pw, ph) = ((self.cell_w() * self.scale) as i32, (22.0 * self.scale) as i32);

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
        let (cw, ch) = (self.cell_w(), self.cell_h());
        for item in &mut self.items {
            item.selected = item.x < r && item.x + cw > l && item.y < b && item.y + ch > t;
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

fn view_path() -> PathBuf {
    positions_path().with_file_name("desktop-view.txt")
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

/// The user's own Desktop folder — where a paste lands and what the drop
/// target is bound to. The Public one is read as well but never written.
fn desktop_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Desktop"))
}

/// "Preferred DropEffect": one DWORD saying whether the files on the clipboard
/// were cut or copied. Registered by name because it has no fixed CF_ number.
unsafe fn preferred_effect_format() -> Option<FORMATETC> {
    let cf = unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) };
    (cf != 0).then(|| FORMATETC {
        cfFormat: cf as u16,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    })
}

unsafe fn set_preferred_effect(data: &IDataObject, effect: DROPEFFECT) {
    unsafe {
        let Some(fmt) = preferred_effect_format() else { return };
        let hg = GlobalAlloc(GMEM_MOVEABLE, 4);
        if hg.is_invalid() {
            return;
        }
        let p = GlobalLock(hg) as *mut u32;
        if p.is_null() {
            return;
        }
        *p = effect.0;
        let _ = GlobalUnlock(hg);
        let medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: hg },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        };
        // fRelease: the data object owns the block from here, including when
        // SetData itself fails.
        let _ = data.SetData(&fmt, &medium, true);
    }
}

unsafe fn preferred_effect(data: &IDataObject) -> DROPEFFECT {
    unsafe {
        let Some(fmt) = preferred_effect_format() else { return DROPEFFECT_COPY };
        let Ok(mut medium) = data.GetData(&fmt) else { return DROPEFFECT_COPY };
        let mut effect = DROPEFFECT_COPY;
        let p = GlobalLock(medium.u.hGlobal) as *const u32;
        if !p.is_null() {
            effect = DROPEFFECT(*p);
            let _ = GlobalUnlock(medium.u.hGlobal);
        }
        ReleaseStgMedium(&mut medium);
        effect
    }
}

/// The drag source side of the same trade. IDropSource is the one interface
/// the shell cannot supply for us — it is the *source's* judgement of when the
/// drag ends — but it is also two methods of pure policy, and both are the
/// standard answer: cancel on Escape or the right button, drop when the button
/// that started it comes up, and let OLE draw its own cursors.
#[implement(IDropSource)]
struct DragSource;

impl IDropSource_Impl for DragSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() || keys.0 & MK_RBUTTON.0 != 0 {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            windows::Win32::Foundation::S_OK
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Is the pointer over a window that is not one of ours? Capture does not
/// change what is under the cursor, so this stays true while we hold it. The
/// secondary desktops count as ours: dragging an icon across a monitor edge is
/// a move within one folder, not an export.
unsafe fn pointer_left_us() -> bool {
    unsafe {
        let mut pt = windows::Win32::Foundation::POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return false;
        }
        let h = WindowFromPoint(pt);
        if h.is_invalid() {
            return false;
        }
        let root = GetAncestor(h, GA_ROOT);
        DESKTOPS.with(|d| !d.borrow().contains(&root))
    }
}

/// Runs the drag. Modal: OLE pumps this thread's messages until the button
/// comes up, so no borrow of the Desktop may be alive across it. Answers
/// whether the files left the desktop.
unsafe fn drag_out(data: &IDataObject) -> bool {
    unsafe {
        let source: IDropSource = DragSource.into();
        let mut effect = DROPEFFECT_NONE;
        let hr = DoDragDrop(
            data,
            &source,
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
            &mut effect,
        );
        hr == DRAGDROP_S_DROP && effect == DROPEFFECT_MOVE
    }
}

/// Make the desktop accept drops, by handing the job to the shell rather than
/// implementing IDropTarget here. The Desktop folder's own drop target already
/// knows copy against move against link, what the modifier keys mean, what to
/// do with a .lnk and what to do with a URL; anything written here would be a
/// worse version of it.
unsafe fn register_drop(hwnd: HWND) {
    unsafe {
        let Some(dir) = desktop_dir() else {
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
                    // Off our windows and past the threshold, the drag stops
                    // being a rearrangement and becomes an export: the icons
                    // go back where they were and OLE takes over the pointer.
                    if desk.drag.as_ref().is_some_and(|d| d.moved) && pointer_left_us() {
                        desk.drag = None;
                        desk.hover = None;
                        let data = desk.selection_data();
                        desk.paint();
                        let _ = ReleaseCapture();
                        // DoDragDrop pumps this wndproc reentrantly — the desk
                        // borrow must not live across it.
                        let moved = data.as_ref().is_some_and(|d| drag_out(d));
                        let desk =
                            &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Desktop);
                        if moved {
                            desk.refresh_all();
                        }
                        return LRESULT(0);
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
                        desk.anchor = i;
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
                    let custom = desk.bg_menu();
                    match std::env::var("USERPROFILE") {
                        Ok(p) => crate::shellmenu::show_background_menu(
                            hwnd,
                            &PathBuf::from(p).join("Desktop"),
                            &custom,
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
                    MenuOutcome::Custom(ID_AUTO_ARRANGE) => {
                        desk.view.auto_arrange = !desk.view.auto_arrange;
                        let repack = desk.view.auto_arrange;
                        desk.apply_view(repack);
                    }
                    MenuOutcome::Custom(ID_SHOW_ICONS) => {
                        desk.view.show_icons = !desk.view.show_icons;
                        desk.apply_view(false);
                    }
                    MenuOutcome::Custom(id) => {
                        if let Some((_, _, px)) = ICON_SIZES.iter().find(|(i, _, _)| *i == id) {
                            desk.view.icon = *px;
                            // The grid changes shape under them, but the cells
                            // an icon was dragged to still mean the same thing,
                            // so a size change is not a rearrangement.
                            desk.apply_view(false);
                        } else if let Some((_, _, s)) = SORTS.iter().find(|(i, _, _)| *i == id) {
                            desk.view.sort = *s;
                            desk.apply_view(true);
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
                let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
                let vk = VIRTUAL_KEY(wparam.0 as u16);
                match vk {
                    VK_LEFT => desk.move_cursor(-1, 0, shift),
                    VK_RIGHT => desk.move_cursor(1, 0, shift),
                    VK_UP => desk.move_cursor(0, -1, shift),
                    VK_DOWN => desk.move_cursor(0, 1, shift),
                    VK_RETURN => desk.open_selected(),
                    VK_F5 => {
                        desk.refresh_all();
                        return LRESULT(0);
                    }
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
                        // Escape also calls off a pending cut, which is the
                        // only way to take one back short of pasting it.
                        desk.cut.clear();
                        desk.typed.clear();
                    }
                    // There are no VK constants for the letter keys.
                    VIRTUAL_KEY(0x41) if ctrl => desk.select_all(),
                    VIRTUAL_KEY(0x43) if ctrl => desk.clipboard_put(false),
                    VIRTUAL_KEY(0x58) if ctrl => desk.clipboard_put(true),
                    VIRTUAL_KEY(0x56) if ctrl => desk.clipboard_paste(),
                    _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
                }
                desk.paint();
                LRESULT(0)
            }
            // Type-ahead. WM_KEYDOWN hands the letters on to DefWindowProc,
            // which is what turns them into characters — and the character is
            // what a Korean name is matched on, not the virtual key.
            WM_CHAR => {
                match char::from_u32(wparam.0 as u32) {
                    Some(c) if !c.is_control() && (c != ' ' || !desk.typed.is_empty()) => {
                        desk.type_ahead(c);
                        desk.paint();
                    }
                    _ => {}
                }
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
