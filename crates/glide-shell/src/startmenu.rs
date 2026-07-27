//! The real start menu (SHELL_DESIGN §6.4 final goal). Win10 two-pane
//! layout: app list (자주 사용 on top, 초성/A–Z sections below) on the left,
//! pinned Metro tile grid on the right, independently scrolled.
//! All shell work — the `shell:AppsFolder` enumeration (win32 Start
//! Menu shortcuts and UWP packages in one pass) and every icon extraction —
//! runs on a dedicated MTA COM worker thread: IShellItemImageFactory calls
//! cost 5–50 ms each, and doing them on the UI thread froze hover repaints.
//! The UI thread only turns finished pixel buffers into D2D bitmaps.
//! Input-wise a Flyout sibling (activatable; dies on WA_INACTIVE, Esc,
//! start-button re-click, 1s foreground-check fallback).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_PROPERTIES1,
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_ROUNDED_RECT, ID2D1Bitmap1,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_FAR,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_WORD_WRAPPING_NO_WRAP, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dwm::{
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, ScreenToClient, ValidateRect,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Power::SetSuspendState;
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VIRTUAL_KEY, VK_DOWN, VK_ESCAPE, VK_RETURN, VK_UP,
};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, IEnumShellItems, IShellItem, IShellItemImageFactory,
    SHCreateItemFromParsingName, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
    SIIGBF_RESIZETOFIT, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCWSTR, w};

use crate::render::{Renderer, ellipsize, fill_round, rect};
use crate::theme;

const WM_MOUSELEAVE: u32 = 0x02A3;
/// Worker → UI: replies are waiting on the channel.
const WM_APP_REPLY: u32 = WM_APP + 10;
const TIMER_REFRESH: usize = 1;

// Win10 two-pane layout: app list left, tile grid right.
const W: f32 = 604.0;
const HEADER_H: f32 = 46.0;
const FOOTER_H: f32 = 56.0;
const LIST_X0: f32 = 12.0;
const LIST_W: f32 = 240.0;
/// Divider between the list pane and the tile pane.
const SPLIT_X: f32 = LIST_X0 + LIST_W + 6.0;
const TILES_X0: f32 = SPLIT_X + 6.0;
const ROW_H: f32 = 36.0;
const SECTION_H: f32 = 28.0;
const LIST_ICON: f32 = 24.0;
/// Metro tile grid: 3 columns of square tiles, wide tiles span 2.
const TILE_COLS: usize = 3;
const TILE: f32 = 104.0;
const TILE_GUT: f32 = 8.0;
const TILE_ICON: f32 = 40.0;
/// Device pixels requested from the shell; actual size read back via
/// GetObjectW — assuming the request is honored corrupted every icon whose
/// bitmap came back a different size.
const ICON_PX: i32 = 64;
/// App list re-enumeration threshold; installs are rare, opens are not.
const STALE_SECS: u64 = 300;
const MENU_TOGGLE_PIN: usize = 1;
const MENU_TOGGLE_WIDE: usize = 2;
const MENU_DISSOLVE: usize = 3;
const MENU_UNGROUP: usize = 4;
const MENU_NEW_GROUP: usize = 90;
/// + group index for the "그룹에 추가" submenu entries.
const MENU_GROUP_BASE: usize = 100;
/// Folder-view header row (back chip + group name) height.
const FOLDER_HEAD: f32 = 44.0;
/// Cursor travel (logical px) before a pressed tile becomes a drag; below
/// this the press stays a click.
const DRAG_SLOP: f32 = 4.0;
/// Footer user chip width; folder shortcuts sit to its right.
const USER_W: f32 = 108.0;

struct Entry {
    name: String,
    wname: Vec<u16>,
    /// Parsing name relative to AppsFolder — the icon-cache key and pin key.
    parsing: String,
    /// "shell:AppsFolder\{parsing}", NUL-terminated.
    launch: Vec<u16>,
    /// Lowercased executable/package stem behind the display name, so a search
    /// for what the user types at a prompt finds the app. Not persisted.
    target: String,
    /// 2×1 Metro tile instead of 1×1; only pins persist this.
    wide: bool,
    /// Tile group (Win10 tile folder); pins sharing a name collapse into one
    /// folder tile. Only pins persist this.
    folder: Option<String>,
}

impl Entry {
    fn new(name: String, parsing: String) -> Self {
        let launch = parse_target(&parsing)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let wname = name.encode_utf16().collect();
        let target = search_target(&parsing);
        Entry { name, wname, parsing, launch, target, wide: false, folder: None }
    }
}

/// The name the user knows the app by when it is not the display name: what
/// they would type at a prompt. On a Korean install "명령 프롬프트" parses to
/// `{GUID}\cmd.exe`, so searching `cmd` — the obvious thing to type — matched
/// nothing at all, and with no explorer there is no other way to launch it.
fn search_target(parsing: &str) -> String {
    let tail = parsing.rsplit(['\\', '/']).next().unwrap_or(parsing);
    // Packaged apps arrive as Family_publisherhash!AppId. Strip the hash only
    // there: a bare filename may legitimately contain '_' (PowerShell_ISE.exe).
    let stem = match tail.split_once('!') {
        Some((family, _)) => family.rsplit_once('_').map_or(family, |(f, _)| f),
        None => std::path::Path::new(tail)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(tail),
    };
    stem.to_lowercase()
}

/// What `SHCreateItemFromParsingName` (and ShellExecute) should be handed for a
/// stored parsing name. AppsFolder IDs are relative — `Microsoft.WindowsStore…`
/// or `{GUID}\path\app.exe` — while the Start Menu fallback stores absolute
/// `.lnk` paths, which parse on their own and must not be prefixed.
fn parse_target(parsing: &str) -> String {
    if std::path::Path::new(parsing).is_absolute() {
        parsing.to_string()
    } else {
        format!(r"shell:AppsFolder\{parsing}")
    }
}

/// One right-pane tile: a lone pin or a folder of them (indices into `pins`).
enum TileItem {
    Single(usize),
    Folder(String, Vec<usize>),
}

enum Row {
    Section(Vec<u16>),
    App(usize),
    /// Index into `freq` — 자주 사용 rows at the top of the list pane.
    Freq(usize),
}

#[derive(Clone, Copy, PartialEq)]
enum Act {
    App(usize),
    /// Index into `tile_items()` — a pin tile or a folder tile.
    Tile(usize),
    /// Index into the open folder's member list (pins indices via members()).
    FolderItem(usize),
    /// "‹ 뒤로" header row while a folder is open.
    FolderBack,
    /// Index into the "자주 사용" list (Win10-style frequent apps).
    Freq(usize),
    /// Index into the type-to-search results.
    Result(usize),
    User,
    Docs,
    Downloads,
    Lock,
    Sleep,
    Restart,
    Shutdown,
}

/// Drag payload, identity-based — tile/member indices go stale across the
/// pin mutations a drop performs.
enum DragSrc {
    /// Loose pin tile, by parsing name.
    Pin(String),
    /// Folder tile, by group name.
    Folder(String),
    /// Pin inside the open folder.
    Member(String),
    /// Left-pane app row (parsing, display name); dropping it pins the app.
    App(String, String),
}

/// Where the payload would land if released now.
#[derive(Clone, Copy, PartialEq)]
enum DropSpot {
    /// Insert as items[i] in the main grid (i == len ⇒ append).
    Before(usize),
    /// Fold into items[i]: onto a single = new group, onto a folder = join.
    Into(usize),
    /// Insert as members[j] of the open folder.
    MemberBefore(usize),
    /// Folder-view header: pull the member out of the group.
    OutOfFolder,
}

/// In-flight drag. Armed on LBUTTONDOWN over anything draggable; only
/// becomes live past DRAG_SLOP, so plain clicks never notice it.
struct Drag {
    src: DragSrc,
    /// Down point, for the slop check.
    x0: f32,
    y0: f32,
    /// Cursor now.
    x: f32,
    y: f32,
    /// Grab offset inside the tile so the ghost doesn't snap to the cursor.
    gx: f32,
    gy: f32,
    live: bool,
    spot: Option<DropSpot>,
}

enum Job {
    Apps { epoch: u32 },
    Icon { parsing: String },
}

enum Reply {
    Apps { epoch: u32, list: Vec<(String, String)> },
    Icon { parsing: String, w: i32, h: i32, pixels: Vec<u8> },
}

pub struct StartMenu {
    hwnd: HWND,
    renderer: Renderer,
    fmt_head: IDWriteTextFormat,
    fmt_item: IDWriteTextFormat,
    fmt_grid: IDWriteTextFormat,
    fmt_center: IDWriteTextFormat,
    fmt_tile: IDWriteTextFormat,
    fmt_section: IDWriteTextFormat,
    fmt_glyph: IDWriteTextFormat,
    pub open: bool,
    /// See dismiss(): keeps a bar-button UP from reopening the menu its own
    /// DOWN closed via WA_INACTIVE/click-away.
    dismissed_at: Option<std::time::Instant>,
    scale: f32,
    w: f32,
    h: f32,
    fg_at_open: HWND,
    apps: Vec<Entry>,
    pins: Vec<Entry>,
    rows: Vec<Row>,
    /// rows[i] top offset inside the list content (mixed row heights).
    row_pos: Vec<f32>,
    loading: bool,
    epoch: u32,
    loaded_at: Option<std::time::Instant>,
    /// Type-to-search: any printable key while the menu is open filters the
    /// app list (Launchpad/GNOME behavior), with 초성 matching (ㅋㄹ → 크롬).
    query: String,
    /// Tile folder currently expanded in the right pane.
    open_folder: Option<String>,
    /// apps indices matching `query`.
    results: Vec<usize>,
    selected: usize,
    /// parsing → (launch count, display name); persisted, drives 자주 사용.
    counts: HashMap<String, (u32, String)>,
    freq: Vec<Entry>,
    /// parsing name → decoded bitmap (None = extraction failed, draw fallback).
    icons: HashMap<String, Option<ID2D1Bitmap1>>,
    /// parsing name → tile background from the icon's dominant color.
    tints: HashMap<String, D2D1_COLOR_F>,
    requested: HashSet<String>,
    jobs: Sender<Job>,
    replies: Receiver<Reply>,
    scroll_list: f32,
    scroll_tiles: f32,
    user: Vec<u16>,
    hover: Option<Act>,
    tracking: bool,
    drag: Option<Drag>,
}

impl StartMenu {
    pub fn new(dpi: f32) -> anyhow::Result<Self> {
        unsafe {
            let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            let class = w!("glide_shell_startmenu");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(start_wndproc),
                hInstance: hinstance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            RegisterClassW(&wc); // 0 on re-register is fine
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP,
                class,
                w!(""),
                WS_POPUP,
                0,
                0,
                64,
                64,
                None,
                None,
                Some(hinstance.into()),
                None,
            )?;
            let dark: i32 = 1;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &dark as *const _ as _,
                4,
            );
            let backdrop: i32 = 3; // DWMSBT_TRANSIENTWINDOW
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                &backdrop as *const _ as _,
                4,
            );
            let corner = DWMWCP_ROUND.0;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const _ as _,
                4,
            );
            let renderer = Renderer::new(hwnd, 64, 64, dpi)?;

            let mk = |family: PCWSTR, size: f32, weight: DWRITE_FONT_WEIGHT| {
                renderer.dwrite.CreateTextFormat(
                    family,
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("ko-kr"),
                )
            };
            let family = w!("Segoe UI Variable");
            let fmt_head = mk(family, 13.5, DWRITE_FONT_WEIGHT_SEMI_BOLD)
                .or_else(|_| mk(w!("Segoe UI"), 13.5, DWRITE_FONT_WEIGHT_SEMI_BOLD))?;
            fmt_head.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            fmt_head.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_item = mk(family, 12.5, DWRITE_FONT_WEIGHT_NORMAL)
                .or_else(|_| mk(w!("Segoe UI"), 12.5, DWRITE_FONT_WEIGHT_NORMAL))?;
            fmt_item.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            fmt_item.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            // App display names run long; the row does not.
            ellipsize(&renderer.dwrite, &fmt_item);
            let fmt_grid = mk(family, 11.0, DWRITE_FONT_WEIGHT_NORMAL)
                .or_else(|_| mk(w!("Segoe UI"), 11.0, DWRITE_FONT_WEIGHT_NORMAL))?;
            fmt_grid.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            fmt_grid.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_grid.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            ellipsize(&renderer.dwrite, &fmt_grid);
            let fmt_center = mk(family, 12.5, DWRITE_FONT_WEIGHT_NORMAL)
                .or_else(|_| mk(w!("Segoe UI"), 12.5, DWRITE_FONT_WEIGHT_NORMAL))?;
            fmt_center.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_center.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            // Metro tile label: bottom-left inside the tile.
            let fmt_tile = mk(family, 11.0, DWRITE_FONT_WEIGHT_NORMAL)
                .or_else(|_| mk(w!("Segoe UI"), 11.0, DWRITE_FONT_WEIGHT_NORMAL))?;
            fmt_tile.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            fmt_tile.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_FAR)?;
            ellipsize(&renderer.dwrite, &fmt_tile);
            let fmt_section = mk(family, 11.5, DWRITE_FONT_WEIGHT_SEMI_BOLD)
                .or_else(|_| mk(w!("Segoe UI"), 11.5, DWRITE_FONT_WEIGHT_SEMI_BOLD))?;
            fmt_section.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            fmt_section.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_glyph = mk(w!("Segoe Fluent Icons"), 15.0, DWRITE_FONT_WEIGHT_NORMAL)
                .or_else(|_| mk(w!("Segoe MDL2 Assets"), 15.0, DWRITE_FONT_WEIGHT_NORMAL))?;
            fmt_glyph.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_glyph.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            let user: Vec<u16> = std::env::var("USERNAME")
                .unwrap_or_else(|_| "사용자".into())
                .encode_utf16()
                .collect();

            let (jobs, job_rx) = channel::<Job>();
            let (reply_tx, replies) = channel::<Reply>();
            let hwnd_raw = hwnd.0 as isize;
            std::thread::spawn(move || worker(job_rx, reply_tx, hwnd_raw));

            // Pre-warm during startup idle: the cold shell:AppsFolder
            // enumeration costs several seconds, and it sits at the head of the
            // single worker queue, so a first open with no warm-up shows a
            // multi-second "loading" state with blank tiles. Kick the app enum
            // and the pinned-tile icons now; the replies land via WM_APP_REPLY
            // on the (hidden) menu window that the bar's loop already pumps, so
            // the first Win-key open is already populated.
            let pins = load_start_pins();
            let _ = jobs.send(Job::Apps { epoch: 1 });
            for p in &pins {
                let _ = jobs.send(Job::Icon { parsing: p.parsing.clone() });
            }

            Ok(StartMenu {
                hwnd,
                renderer,
                fmt_head,
                fmt_item,
                fmt_grid,
                fmt_center,
                fmt_tile,
                fmt_section,
                fmt_glyph,
                open: false,
                dismissed_at: None,
                scale: dpi / 96.0,
                w: 0.0,
                h: 0.0,
                fg_at_open: HWND::default(),
                apps: Vec::new(),
                pins,
                rows: Vec::new(),
                row_pos: Vec::new(),
                loading: true,
                epoch: 1,
                loaded_at: None,
                query: String::new(),
                open_folder: None,
                results: Vec::new(),
                selected: 0,
                counts: load_counts(),
                freq: Vec::new(),
                icons: HashMap::new(),
                tints: HashMap::new(),
                requested: HashSet::new(),
                jobs,
                replies,
                scroll_list: 0.0,
                scroll_tiles: 0.0,
                user,
                hover: None,
                tracking: false,
                drag: None,
            })
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn show(&mut self, bar_rect: RECT) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, self as *mut StartMenu as isize);
        }
        crate::clickaway::set_start_open(true);
        // The pointer above is the wndproc's only way back to us, and it cannot
        // be installed in new() — StartMenu is returned by value, so its address
        // is not final until it has been stored. Every WM_APP_REPLY posted
        // before this line therefore hit a null userdata and was dropped, the
        // prewarm reply included: the list sat in the channel with nothing left
        // to wake the drain, and the menu stayed on "loading" for the life of
        // the session. Drain here so the first open collects it.
        self.on_replies();
        if !self.loading
            && self
                .loaded_at
                .is_none_or(|t| t.elapsed().as_secs() > STALE_SECS)
        {
            self.epoch += 1;
            self.loading = true;
            let _ = self.jobs.send(Job::Apps { epoch: self.epoch });
        }
        self.open = true;
        self.hover = None;
        self.scroll_list = 0.0;
        self.scroll_tiles = 0.0;
        self.query.clear();
        self.open_folder = None;
        self.results.clear();
        self.rebuild_freq();

        self.w = W;
        // As tall as fits above the bar, Win11-proportioned cap.
        let avail = bar_rect.top as f32 / self.scale - 24.0;
        self.h = avail.min(640.0);
        let wd = (self.w * self.scale).round() as i32;
        let hd = (self.h * self.scale).round() as i32;
        unsafe {
            self.fg_at_open = GetForegroundWindow();
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                bar_rect.left + (12.0 * self.scale) as i32,
                bar_rect.top - (10.0 * self.scale) as i32 - hd,
                wd,
                hd,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        let _ = self
            .renderer
            .resize(wd as u32, hd as u32, self.scale * 96.0);
        self.paint();
        unsafe {
            // Same rule as the flyouts: the start-button click grants us the
            // foreground right; take it so WA_INACTIVE dismissal works.
            let _ = SetForegroundWindow(self.hwnd);
            SetTimer(Some(self.hwnd), TIMER_REFRESH, 1000, None);
        }
    }

    pub fn hide(&mut self) {
        crate::clickaway::set_start_open(false);
        if self.open {
            self.open = false;
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_REFRESH);
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        self.hover = None;
        self.drag = None;
    }

    /// Hide caused by the user clicking elsewhere (WA_INACTIVE, click-away):
    /// stamps the moment so the same click's UP on the start button doesn't
    /// reopen the menu its own DOWN just closed.
    pub fn dismiss(&mut self) {
        if self.open {
            self.dismissed_at = Some(std::time::Instant::now());
        }
        self.hide();
    }

    pub fn just_dismissed(&self) -> bool {
        self.dismissed_at
            .is_some_and(|t| t.elapsed().as_millis() < 400)
    }

    // ---- data --------------------------------------------------------------

    /// Worker finished something; drain and integrate.
    fn on_replies(&mut self) {
        let mut dirty = false;
        while let Ok(rep) = self.replies.try_recv() {
            match rep {
                Reply::Apps { epoch, list } => {
                    if epoch != self.epoch {
                        continue;
                    }
                    self.apps = list
                        .into_iter()
                        .map(|(name, parsing)| Entry::new(name, parsing))
                        .collect();
                    self.rebuild_rows();
                    self.loading = false;
                    self.loaded_at = Some(std::time::Instant::now());
                    dirty = true;
                }
                Reply::Icon { parsing, w, h, pixels } => {
                    if let Some((r, g, b)) = tint_of(&pixels) {
                        self.tints.insert(
                            parsing.clone(),
                            D2D1_COLOR_F { r, g, b, a: 1.0 },
                        );
                    }
                    let bmp = self.make_bitmap(w, h, &pixels);
                    self.icons.insert(parsing, bmp);
                    dirty = true;
                }
            }
        }
        if dirty && self.open {
            self.paint();
        }
    }

    fn make_bitmap(&self, w: i32, h: i32, pixels: &[u8]) -> Option<ID2D1Bitmap1> {
        if w <= 0 || h <= 0 || pixels.len() != (w * h * 4) as usize {
            return None;
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
        unsafe {
            self.renderer
                .dc
                .CreateBitmap(
                    D2D_SIZE_U { width: w as u32, height: h as u32 },
                    Some(pixels.as_ptr() as *const _),
                    (w * 4) as u32,
                    &props,
                )
                .ok()
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        self.row_pos.clear();
        let mut y = 0.0f32;
        // Win10 keeps 자주 사용 at the top of the list pane.
        if !self.freq.is_empty() {
            self.row_pos.push(y);
            self.rows.push(Row::Section("자주 사용".encode_utf16().collect()));
            y += SECTION_H;
            for i in 0..self.freq.len() {
                self.row_pos.push(y);
                self.rows.push(Row::Freq(i));
                y += ROW_H;
            }
        }
        let mut last: Option<(u8, char)> = None;
        for (i, a) in self.apps.iter().enumerate() {
            let sec = section_of(&a.name);
            if last != Some(sec) {
                last = Some(sec);
                self.row_pos.push(y);
                let mut buf = [0u16; 2];
                self.rows.push(Row::Section(sec.1.encode_utf16(&mut buf).to_vec()));
                y += SECTION_H;
            }
            self.row_pos.push(y);
            self.rows.push(Row::App(i));
            y += ROW_H;
        }
    }

    fn request_icon(&mut self, parsing: &str) {
        if self.icons.contains_key(parsing) || self.requested.contains(parsing) {
            return;
        }
        self.requested.insert(parsing.to_string());
        let _ = self.jobs.send(Job::Icon { parsing: parsing.to_string() });
    }

    fn toggle_pin(&mut self, parsing: &str, name: &str) {
        if let Some(i) = self.pins.iter().position(|p| p.parsing == parsing) {
            self.pins.remove(i);
        } else {
            self.pins.push(Entry::new(name.to_string(), parsing.to_string()));
        }
        save_start_pins(&self.pins);
        self.rebuild_freq();
        self.paint();
    }

    fn toggle_wide(&mut self, parsing: &str) {
        if let Some(p) = self.pins.iter_mut().find(|p| p.parsing == parsing) {
            p.wide = !p.wide;
            save_start_pins(&self.pins);
            self.paint();
        }
    }

    /// Move a pin into a group (or out with `None`).
    fn set_folder(&mut self, parsing: &str, folder: Option<String>) {
        if let Some(p) = self.pins.iter_mut().find(|p| p.parsing == parsing) {
            p.folder = folder;
            save_start_pins(&self.pins);
            // Open folder may have just emptied.
            if let Some(f) = &self.open_folder {
                if self.members(f).is_empty() {
                    self.open_folder = None;
                    self.scroll_tiles = 0.0;
                }
            }
            self.paint();
        }
    }

    /// Ungroup every member; the folder tile dissolves back into singles.
    fn dissolve(&mut self, folder: &str) {
        for p in &mut self.pins {
            if p.folder.as_deref() == Some(folder) {
                p.folder = None;
            }
        }
        save_start_pins(&self.pins);
        if self.open_folder.as_deref() == Some(folder) {
            self.open_folder = None;
            self.scroll_tiles = 0.0;
        }
        self.paint();
    }

    /// "그룹 1", "그룹 2", … first unused.
    fn next_group_name(&self) -> String {
        let groups = self.groups();
        (1..)
            .map(|n| format!("그룹 {n}"))
            .find(|c| !groups.contains(c))
            .unwrap()
    }

    fn bump_count(&mut self, parsing: &str, name: &str) {
        let e = self
            .counts
            .entry(parsing.to_string())
            .or_insert((0, name.to_string()));
        e.0 += 1;
        e.1 = name.to_string();
        save_counts(&self.counts);
    }

    /// Top launched apps that aren't already pinned, most-used first.
    fn rebuild_freq(&mut self) {
        let mut v: Vec<(&String, &(u32, String))> = self
            .counts
            .iter()
            .filter(|(p, _)| !self.pins.iter().any(|pin| &pin.parsing == *p))
            .collect();
        v.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.1.1.cmp(&b.1.1)));
        self.freq = v
            .into_iter()
            .take(6)
            .map(|(p, (_, n))| Entry::new(n.clone(), p.clone()))
            .collect();
        self.rebuild_rows();
    }

    fn update_results(&mut self) {
        self.results.clear();
        self.selected = 0;
        self.scroll_list = 0.0;
        if self.query.is_empty() {
            return;
        }
        let q = self.query.to_lowercase();
        let jamo_mode = q.chars().all(|c| ('ㄱ'..='ㅎ').contains(&c));
        // (tier, position, frequency, index): a display-name hit always
        // outranks an executable-name hit, so typing 메모 keeps 메모장 on top
        // even though several packages carry "notepad" in their id.
        let mut scored: Vec<(u8, usize, u32, usize)> = Vec::new();
        for (i, a) in self.apps.iter().enumerate() {
            let hay = if jamo_mode { name_cho(&a.name) } else { a.name.to_lowercase() };
            let hit = match hay.find(&q) {
                Some(pos) => Some((0u8, pos)),
                // Jamo queries are Hangul initials; targets are ASCII paths.
                None if jamo_mode => None,
                None => a.target.find(&q).map(|pos| (1u8, pos)),
            };
            if let Some((tier, pos)) = hit {
                let count = self.counts.get(&a.parsing).map(|c| c.0).unwrap_or(0);
                scored.push((tier, pos, u32::MAX - count, i));
            }
        }
        scored.sort();
        self.results = scored.into_iter().map(|(_, _, _, i)| i).collect();
    }

    /// Keep the keyboard selection inside the search viewport.
    fn ensure_selected_visible(&mut self) {
        let view_h = self.list_bottom() - self.list_top();
        let top = self.selected as f32 * ROW_H;
        if top < self.scroll_list {
            self.scroll_list = top;
        } else if top + ROW_H > self.scroll_list + view_h {
            self.scroll_list = top + ROW_H - view_h;
        }
    }

    // ---- layout ------------------------------------------------------------

    fn list_top(&self) -> f32 {
        HEADER_H
    }

    fn list_bottom(&self) -> f32 {
        self.h - FOOTER_H
    }

    /// Right-pane tile items: pins fold into folder tiles at the position of
    /// their first member; loose pins stay singles, in pin order.
    fn tile_items(&self) -> Vec<TileItem> {
        let mut out: Vec<TileItem> = Vec::new();
        for (i, pin) in self.pins.iter().enumerate() {
            match &pin.folder {
                Some(f) => {
                    if let Some(TileItem::Folder(_, members)) = out
                        .iter_mut()
                        .find(|it| matches!(it, TileItem::Folder(n, _) if n == f))
                    {
                        members.push(i);
                    } else {
                        out.push(TileItem::Folder(f.clone(), vec![i]));
                    }
                }
                None => out.push(TileItem::Single(i)),
            }
        }
        out
    }

    /// Pin indices inside a folder, in pin order.
    fn members(&self, folder: &str) -> Vec<usize> {
        self.pins
            .iter()
            .enumerate()
            .filter(|(_, p)| p.folder.as_deref() == Some(folder))
            .map(|(i, _)| i)
            .collect()
    }

    /// Existing group names, in first-appearance order.
    fn groups(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for p in &self.pins {
            if let Some(f) = &p.folder {
                if !out.contains(f) {
                    out.push(f.clone());
                }
            }
        }
        out
    }

    /// First-fit tile packing: (col, row, span) per input span. Wide tiles
    /// take two adjacent cells; squares backfill earlier holes so the grid
    /// stays tight.
    fn pack(spans: &[usize]) -> Vec<(usize, usize, usize)> {
        let mut used: Vec<[bool; TILE_COLS]> = Vec::new();
        let mut out = Vec::with_capacity(spans.len());
        for &span in spans {
            let mut row = 0usize;
            let (row, col) = loop {
                if row == used.len() {
                    used.push([false; TILE_COLS]);
                }
                if let Some(c) =
                    (0..=(TILE_COLS - span)).find(|&c| used[row][c..c + span].iter().all(|u| !u))
                {
                    break (row, c);
                }
                row += 1;
            };
            for u in &mut used[row][col..col + span] {
                *u = true;
            }
            out.push((col, row, span));
        }
        out
    }

    /// Packed slots for the main grid (folders collapse to 1×1).
    fn item_slots(&self, items: &[TileItem]) -> Vec<(usize, usize, usize)> {
        let spans: Vec<usize> = items
            .iter()
            .map(|it| match it {
                TileItem::Single(i) if self.pins[*i].wide => 2,
                _ => 1,
            })
            .collect();
        Self::pack(&spans)
    }

    /// Packed slots for the open folder's members.
    fn member_slots(&self, members: &[usize]) -> Vec<(usize, usize, usize)> {
        let spans: Vec<usize> = members
            .iter()
            .map(|&i| if self.pins[i].wide { 2 } else { 1 })
            .collect();
        Self::pack(&spans)
    }

    fn tile_rect(&self, col: usize, row: usize, span: usize, y_off: f32) -> D2D_RECT_F {
        let x = TILES_X0 + col as f32 * (TILE + TILE_GUT);
        let y = self.list_top() + y_off + row as f32 * (TILE + TILE_GUT) - self.scroll_tiles;
        rect(x, y, x + span as f32 * TILE + (span - 1) as f32 * TILE_GUT, y + TILE)
    }

    fn grid_h(slots: &[(usize, usize, usize)]) -> f32 {
        let rows = slots.iter().map(|&(_, r, _)| r + 1).max().unwrap_or(0);
        rows as f32 * (TILE + TILE_GUT)
    }

    fn tiles_content_h(&self) -> f32 {
        match &self.open_folder {
            Some(f) => FOLDER_HEAD + Self::grid_h(&self.member_slots(&self.members(f))),
            None => Self::grid_h(&self.item_slots(&self.tile_items())),
        }
    }

    fn list_content_h(&self) -> f32 {
        if !self.query.is_empty() {
            return self.results.len() as f32 * ROW_H;
        }
        self.row_pos
            .last()
            .map(|p| {
                p + match self.rows.last() {
                    Some(Row::Section(_)) => SECTION_H,
                    _ => ROW_H,
                }
            })
            .unwrap_or(0.0)
    }

    fn max_scroll_list(&self) -> f32 {
        (self.list_content_h() - (self.list_bottom() - self.list_top())).max(0.0)
    }

    fn max_scroll_tiles(&self) -> f32 {
        (self.tiles_content_h() - (self.list_bottom() - self.list_top())).max(0.0)
    }

    /// Wheel scrolls the pane under the cursor.
    fn wheel(&mut self, x: f32, notches: f32) {
        let d = notches * ROW_H * 3.0;
        let changed = if x >= SPLIT_X {
            let max = self.max_scroll_tiles();
            let before = self.scroll_tiles;
            self.scroll_tiles = (self.scroll_tiles - d).clamp(0.0, max);
            self.scroll_tiles != before
        } else {
            let max = self.max_scroll_list();
            let before = self.scroll_list;
            self.scroll_list = (self.scroll_list - d).clamp(0.0, max);
            self.scroll_list != before
        };
        if changed {
            self.paint();
        }
    }

    /// Footer control rects, right-aligned: lock, sleep, restart, shutdown.
    fn power_rects(&self) -> [(D2D_RECT_F, Act); 4] {
        let acts = [Act::Lock, Act::Sleep, Act::Restart, Act::Shutdown];
        let size = 34.0;
        let gap = 6.0;
        let cy = self.h - FOOTER_H / 2.0;
        let mut right = self.w - 14.0;
        let mut out = [(rect(0.0, 0.0, 0.0, 0.0), Act::Lock); 4];
        for (slot, act) in out.iter_mut().zip(acts).rev() {
            *slot = (
                rect(right - size, cy - size / 2.0, right, cy + size / 2.0),
                act,
            );
            right -= size + gap;
        }
        out
    }

    /// Footer folder shortcuts (Win7-style): documents, downloads.
    fn folder_rects(&self) -> [(D2D_RECT_F, Act); 2] {
        let size = 34.0;
        let cy = self.h - FOOTER_H / 2.0;
        let x0 = 10.0 + USER_W + 8.0;
        [
            (rect(x0, cy - size / 2.0, x0 + size, cy + size / 2.0), Act::Docs),
            (
                rect(x0 + size + 6.0, cy - size / 2.0, x0 + size * 2.0 + 6.0, cy + size / 2.0),
                Act::Downloads,
            ),
        ]
    }

    fn hit(&self, x: f32, y: f32) -> Option<Act> {
        if y >= self.list_bottom() {
            for (rc, act) in self.power_rects().into_iter().chain(self.folder_rects()) {
                if x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom {
                    return Some(act);
                }
            }
            if (10.0..10.0 + USER_W).contains(&x) {
                return Some(Act::User);
            }
            return None;
        }
        if y < self.list_top() {
            return None;
        }
        if x >= SPLIT_X {
            match &self.open_folder {
                Some(f) => {
                    let br = self.back_rect();
                    if x >= br.left && x < br.right && y >= br.top && y < br.bottom {
                        return Some(Act::FolderBack);
                    }
                    let members = self.members(f);
                    for (j, &(c, r, s)) in self.member_slots(&members).iter().enumerate() {
                        let rc = self.tile_rect(c, r, s, FOLDER_HEAD);
                        if x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom {
                            return Some(Act::FolderItem(j));
                        }
                    }
                }
                None => {
                    let items = self.tile_items();
                    for (i, &(c, r, s)) in self.item_slots(&items).iter().enumerate() {
                        let rc = self.tile_rect(c, r, s, 0.0);
                        if x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom {
                            return Some(Act::Tile(i));
                        }
                    }
                }
            }
            return None;
        }
        if !self.query.is_empty() {
            let idx = ((y - self.list_top() + self.scroll_list) / ROW_H) as usize;
            return (idx < self.results.len()).then_some(Act::Result(idx));
        }
        let cy = y - self.list_top() + self.scroll_list;
        for (i, row) in self.rows.iter().enumerate() {
            let act = match row {
                Row::App(a) => Act::App(*a),
                Row::Freq(f) => Act::Freq(*f),
                Row::Section(_) => continue,
            };
            let top = self.row_pos[i];
            if cy >= top && cy < top + ROW_H {
                return Some(act);
            }
        }
        None
    }

    // ---- drag & drop --------------------------------------------------------

    /// LBUTTONDOWN: remember what a drag from here would carry.
    fn arm_drag(&mut self, x: f32, y: f32) {
        let src = match self.hit(x, y) {
            Some(Act::Tile(i)) => {
                let items = self.tile_items();
                let &(c, r, s) = &self.item_slots(&items)[i];
                let rc = self.tile_rect(c, r, s, 0.0);
                let src = match &items[i] {
                    TileItem::Single(pi) => DragSrc::Pin(self.pins[*pi].parsing.clone()),
                    TileItem::Folder(n, _) => DragSrc::Folder(n.clone()),
                };
                Some((src, x - rc.left, y - rc.top))
            }
            Some(Act::FolderItem(j)) => self.open_folder.clone().and_then(|f| {
                let members = self.members(&f);
                let &pi = members.get(j)?;
                let &(c, r, s) = self.member_slots(&members).get(j)?;
                let rc = self.tile_rect(c, r, s, FOLDER_HEAD);
                Some((DragSrc::Member(self.pins[pi].parsing.clone()), x - rc.left, y - rc.top))
            }),
            Some(Act::App(i)) => self
                .apps
                .get(i)
                .map(|a| (DragSrc::App(a.parsing.clone(), a.name.clone()), TILE / 2.0, TILE / 2.0)),
            Some(Act::Freq(i)) => self
                .freq
                .get(i)
                .map(|e| (DragSrc::App(e.parsing.clone(), e.name.clone()), TILE / 2.0, TILE / 2.0)),
            Some(Act::Result(i)) => self
                .results
                .get(i)
                .and_then(|&a| self.apps.get(a))
                .map(|a| (DragSrc::App(a.parsing.clone(), a.name.clone()), TILE / 2.0, TILE / 2.0)),
            _ => None,
        };
        self.drag = src.map(|(src, gx, gy)| Drag {
            src,
            x0: x,
            y0: y,
            x,
            y,
            gx,
            gy,
            live: false,
            spot: None,
        });
    }

    fn drag_move(&mut self, x: f32, y: f32) {
        {
            let Some(d) = &mut self.drag else { return };
            d.x = x;
            d.y = y;
            if !d.live {
                if (x - d.x0).abs() < DRAG_SLOP && (y - d.y0).abs() < DRAG_SLOP {
                    return;
                }
                d.live = true;
                self.hover = None;
            }
        }
        let spot = self.drop_spot(x, y);
        if let Some(d) = &mut self.drag {
            d.spot = spot;
        }
        self.paint();
    }

    /// The dragged payload's own tile in the main grid, if it has one.
    fn src_item_idx(&self, items: &[TileItem]) -> Option<usize> {
        let d = self.drag.as_ref()?;
        items.iter().position(|it| match (&d.src, it) {
            (DragSrc::Pin(p) | DragSrc::App(p, _), TileItem::Single(pi)) => {
                self.pins[*pi].parsing == *p
            }
            (DragSrc::Folder(f), TileItem::Folder(n, _)) => n == f,
            _ => false,
        })
    }

    fn drop_spot(&self, x: f32, y: f32) -> Option<DropSpot> {
        let d = self.drag.as_ref()?;
        if x < SPLIT_X || y < self.list_top() || y >= self.list_bottom() {
            return None;
        }
        if let Some(f) = &self.open_folder {
            // Folder view: members reorder; the header band pulls one out.
            if !matches!(d.src, DragSrc::Member(_)) {
                return None;
            }
            if y < self.list_top() + FOLDER_HEAD {
                return Some(DropSpot::OutOfFolder);
            }
            let members = self.members(f);
            for (j, &(c, r, s)) in self.member_slots(&members).iter().enumerate() {
                let rc = self.tile_rect(c, r, s, FOLDER_HEAD);
                if x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom {
                    let before = x < (rc.left + rc.right) / 2.0;
                    return Some(DropSpot::MemberBefore(if before { j } else { j + 1 }));
                }
            }
            return Some(DropSpot::MemberBefore(members.len()));
        }
        let items = self.tile_items();
        let src_item = self.src_item_idx(&items);
        for (i, &(c, r, s)) in self.item_slots(&items).iter().enumerate() {
            let rc = self.tile_rect(c, r, s, 0.0);
            if !(x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom) {
                continue;
            }
            if Some(i) == src_item {
                return None;
            }
            // Win10 gestures: tile onto tile makes a group, tile or app onto
            // a folder joins it; folders never merge.
            let combinable = matches!(
                (&d.src, &items[i]),
                (DragSrc::Pin(_), _) | (DragSrc::App(..), TileItem::Folder(..))
            );
            let fx = (x - rc.left) / (rc.right - rc.left);
            return Some(if combinable && (0.25..0.75).contains(&fx) {
                DropSpot::Into(i)
            } else if fx < 0.5 {
                DropSpot::Before(i)
            } else {
                DropSpot::Before(i + 1)
            });
        }
        Some(DropSpot::Before(items.len()))
    }

    fn commit_drop(&mut self, d: Drag) {
        match d.spot {
            Some(DropSpot::Before(i)) => self.drop_before(&d.src, i),
            Some(DropSpot::Into(i)) => self.drop_into(&d.src, i),
            Some(DropSpot::MemberBefore(j)) => self.drop_member_before(&d.src, j),
            Some(DropSpot::OutOfFolder) => {
                if let DragSrc::Member(p) = &d.src {
                    let p = p.clone();
                    self.set_folder(&p, None);
                }
            }
            None => self.paint(),
        }
    }

    /// Reorder the main grid: pull the payload out of the item list, reinsert
    /// before item `i`, flatten back to pin order.
    fn drop_before(&mut self, src: &DragSrc, mut i: usize) {
        // Dragging a pinned app row is moving its tile; if the pin lives in a
        // folder, the drag pulls it out first.
        if let DragSrc::Pin(p) | DragSrc::App(p, _) = src {
            if let Some(e) = self.pins.iter_mut().find(|e| e.parsing == *p) {
                e.folder = None;
            }
        }
        let mut items = self.tile_items();
        let si = match src {
            DragSrc::Pin(p) | DragSrc::App(p, _) => items.iter().position(
                |it| matches!(it, TileItem::Single(pi) if self.pins[*pi].parsing == *p),
            ),
            DragSrc::Folder(f) => items
                .iter()
                .position(|it| matches!(it, TileItem::Folder(n, _) if n == f)),
            DragSrc::Member(_) => return,
        };
        let moved = match si {
            Some(si) => {
                if si < i {
                    i -= 1;
                }
                items.remove(si)
            }
            None => match src {
                // Unpinned app row: dropping it mints the pin right there.
                DragSrc::App(p, n) => {
                    self.pins.push(Entry::new(n.clone(), p.clone()));
                    TileItem::Single(self.pins.len() - 1)
                }
                _ => return,
            },
        };
        items.insert(i.min(items.len()), moved);
        self.reflow(items);
    }

    /// Persist an item ordering: flatten back into pin order (folder members
    /// come out contiguous, so the file stays tidy).
    fn reflow(&mut self, items: Vec<TileItem>) {
        let mut order: Vec<usize> = Vec::with_capacity(self.pins.len());
        for it in &items {
            match it {
                TileItem::Single(pi) => order.push(*pi),
                TileItem::Folder(_, ms) => order.extend(ms),
            }
        }
        for pi in 0..self.pins.len() {
            if !order.contains(&pi) {
                order.push(pi);
            }
        }
        self.pins = permute(std::mem::take(&mut self.pins), &order);
        save_start_pins(&self.pins);
        self.rebuild_freq();
        self.paint();
    }

    /// Fold the payload into items[i]: onto a single tile = fresh group at
    /// the target's slot, onto a folder tile = join it.
    fn drop_into(&mut self, src: &DragSrc, i: usize) {
        let (parsing, name) = match src {
            DragSrc::Pin(p) => (p.clone(), String::new()),
            DragSrc::App(p, n) => (p.clone(), n.clone()),
            _ => return,
        };
        let items = self.tile_items();
        let group = match items.get(i) {
            Some(TileItem::Folder(g, _)) => g.clone(),
            Some(TileItem::Single(ti)) => {
                let g = self.next_group_name();
                self.pins[*ti].folder = Some(g.clone());
                g
            }
            None => return,
        };
        if !self.pins.iter().any(|p| p.parsing == parsing) {
            self.pins.push(Entry::new(name, parsing.clone()));
        }
        if let Some(p) = self.pins.iter_mut().find(|p| p.parsing == parsing) {
            p.folder = Some(group.clone());
        }
        // The folder tile sits at its first member's pin slot; parking the
        // dragged entry after the last existing member keeps that slot put.
        if let Some(s) = self.pins.iter().position(|p| p.parsing == parsing) {
            let entry = self.pins.remove(s);
            match self.pins.iter().rposition(|p| p.folder.as_deref() == Some(group.as_str())) {
                Some(m) => self.pins.insert(m + 1, entry),
                None => self.pins.push(entry),
            }
        }
        save_start_pins(&self.pins);
        self.rebuild_freq();
        self.paint();
    }

    /// Reorder within the open folder: same entries, new order, same slots.
    fn drop_member_before(&mut self, src: &DragSrc, j: usize) {
        let DragSrc::Member(parsing) = src else { return };
        let Some(f) = self.open_folder.clone() else { return };
        let slots = self.members(&f);
        let Some(mj) = slots.iter().position(|&pi| self.pins[pi].parsing == *parsing) else {
            return;
        };
        let mut order = slots.clone();
        let moved = order.remove(mj);
        let j = if mj < j { j - 1 } else { j };
        order.insert(j.min(order.len()), moved);
        self.pins = permute_slots(std::mem::take(&mut self.pins), &slots, &order);
        save_start_pins(&self.pins);
        self.paint();
    }

    /// Launch an entry, feed the 자주 사용 counter, close.
    fn launch_entry(&mut self, parsing: String, name: String, cmd: Vec<u16>) {
        launch(&cmd);
        self.bump_count(&parsing, &name);
        self.hide();
    }

    fn act(&mut self, a: Act) {
        match a {
            Act::App(i) => {
                if let Some(app) = self.apps.get(i) {
                    let (p, n, c) = (app.parsing.clone(), app.name.clone(), app.launch.clone());
                    self.launch_entry(p, n, c);
                }
            }
            Act::Tile(i) => match self.tile_items().get(i) {
                Some(TileItem::Single(pi)) => {
                    if let Some(pin) = self.pins.get(*pi) {
                        let (p, n, c) =
                            (pin.parsing.clone(), pin.name.clone(), pin.launch.clone());
                        self.launch_entry(p, n, c);
                    }
                }
                Some(TileItem::Folder(name, _)) => {
                    self.open_folder = Some(name.clone());
                    self.scroll_tiles = 0.0;
                    self.hover = None;
                    self.paint();
                }
                None => {}
            },
            Act::FolderItem(j) => {
                if let Some(f) = self.open_folder.clone() {
                    if let Some(pin) = self.members(&f).get(j).and_then(|&pi| self.pins.get(pi)) {
                        let (p, n, c) =
                            (pin.parsing.clone(), pin.name.clone(), pin.launch.clone());
                        self.launch_entry(p, n, c);
                    }
                }
            }
            Act::FolderBack => {
                self.open_folder = None;
                self.scroll_tiles = 0.0;
                self.hover = None;
                self.paint();
            }
            Act::Freq(i) => {
                if let Some(f) = self.freq.get(i) {
                    let (p, n, c) = (f.parsing.clone(), f.name.clone(), f.launch.clone());
                    self.launch_entry(p, n, c);
                }
            }
            Act::Result(i) => {
                if let Some(app) = self.results.get(i).and_then(|&a| self.apps.get(a)) {
                    let (p, n, c) = (app.parsing.clone(), app.name.clone(), app.launch.clone());
                    self.launch_entry(p, n, c);
                }
            }
            Act::Docs => {
                open_profile_dir("Documents");
                self.hide();
            }
            Act::Downloads => {
                open_profile_dir("Downloads");
                self.hide();
            }
            Act::User => {
                open_home();
                self.hide();
            }
            Act::Lock => {
                self.hide();
                unsafe {
                    let _ = LockWorkStation();
                }
            }
            Act::Sleep => {
                self.hide();
                unsafe {
                    let _ = SetSuspendState(false, false, false);
                }
            }
            Act::Restart => {
                self.hide();
                run_shutdown(w!("/r /t 0"));
            }
            Act::Shutdown => {
                self.hide();
                run_shutdown(w!("/s /t 0"));
            }
        }
    }

    fn refresh(&mut self) {
        unsafe {
            // SetForegroundWindow can be denied; if we never became foreground
            // and the user moved on, WA_INACTIVE never comes — close here.
            let fg = GetForegroundWindow();
            if fg != self.hwnd && fg != self.fg_at_open {
                self.dismiss();
            }
        }
    }

    // ---- paint -------------------------------------------------------------

    fn paint(&mut self) {
        // Queue icon fetches for what is (or will be) visible; the worker
        // streams results back via WM_APP_REPLY.
        let (top, bottom) = (self.list_top(), self.list_bottom());
        let mut want = Vec::new();
        if !self.query.is_empty() {
            for (i, &a) in self.results.iter().enumerate() {
                let y = top + i as f32 * ROW_H - self.scroll_list;
                if y + ROW_H >= top && y <= bottom {
                    want.push(self.apps[a].parsing.clone());
                }
            }
        } else {
            for (i, row) in self.rows.iter().enumerate() {
                let y = top + self.row_pos[i] - self.scroll_list;
                if y + ROW_H < top || y > bottom {
                    continue;
                }
                match row {
                    Row::App(a) => want.push(self.apps[*a].parsing.clone()),
                    Row::Freq(f) => {
                        if let Some(e) = self.freq.get(*f) {
                            want.push(e.parsing.clone());
                        }
                    }
                    Row::Section(_) => {}
                }
            }
        }
        match &self.open_folder {
            Some(f) => {
                let members = self.members(f);
                for (&pi, &(c, r, s)) in members.iter().zip(&self.member_slots(&members)) {
                    let rc = self.tile_rect(c, r, s, FOLDER_HEAD);
                    if rc.bottom >= top && rc.top <= bottom {
                        want.push(self.pins[pi].parsing.clone());
                    }
                }
            }
            None => {
                let items = self.tile_items();
                for (item, &(c, r, s)) in items.iter().zip(&self.item_slots(&items)) {
                    let rc = self.tile_rect(c, r, s, 0.0);
                    if rc.bottom < top || rc.top > bottom {
                        continue;
                    }
                    match item {
                        TileItem::Single(pi) => want.push(self.pins[*pi].parsing.clone()),
                        // Folder tiles preview their first four members.
                        TileItem::Folder(_, ms) => {
                            for &pi in ms.iter().take(4) {
                                want.push(self.pins[pi].parsing.clone());
                            }
                        }
                    }
                }
            }
        }
        for p in want {
            self.request_icon(&p);
        }

        unsafe {
            let r = &self.renderer;
            r.dc.BeginDraw();
            r.dc.Clear(Some(&theme::rgba(26, 27, 32, 0.92)));

            // Header: search echo while typing, else title + hint.
            if !self.query.is_empty() {
                self.text(&[0xE721], &self.fmt_glyph, rect(16.0, 0.0, 40.0, HEADER_H), theme::accent());
                let q: Vec<u16> = self.query.encode_utf16().collect();
                self.text(&q, &self.fmt_head, rect(44.0, 0.0, self.w - 60.0, HEADER_H), theme::TEXT);
                let n: Vec<u16> = format!("{}", self.results.len()).encode_utf16().collect();
                self.text(&n, &self.fmt_section, rect(self.w - 56.0, 0.0, self.w - 18.0, HEADER_H), theme::TEXT_DIM);
            } else {
                let title: Vec<u16> = "시작".encode_utf16().collect();
                self.text(&title, &self.fmt_head, rect(20.0, 0.0, 200.0, HEADER_H), theme::TEXT);
                let hint: Vec<u16> = "입력하면 검색".encode_utf16().collect();
                self.text(
                    &hint,
                    &self.fmt_grid,
                    rect(self.w - 130.0, 0.0, self.w - 18.0, HEADER_H),
                    theme::TEXT_DIM,
                );
            }

            // Left pane: search results or the freq + app list.
            r.dc.PushAxisAlignedClip(
                &rect(0.0, top, SPLIT_X - 3.0, bottom),
                D2D1_ANTIALIAS_MODE_ALIASED,
            );
            if !self.query.is_empty() {
                self.paint_search(top, bottom);
            } else {
                self.paint_apps(top, bottom);
            }
            r.dc.PopAxisAlignedClip();

            // Pane divider
            if let Ok(b) = r.brush(theme::with_alpha(theme::TEXT_DIM, 0.18)) {
                r.dc.FillRectangle(&rect(SPLIT_X - 1.0, top + 4.0, SPLIT_X, bottom - 4.0), &b);
            }

            // Right pane: pinned tile grid.
            r.dc.PushAxisAlignedClip(
                &rect(SPLIT_X, top, self.w, bottom),
                D2D1_ANTIALIAS_MODE_ALIASED,
            );
            self.paint_tiles(top, bottom);
            r.dc.PopAxisAlignedClip();

            // Per-pane scrollbar thumbs (only when the content overflows)
            let view_h = bottom - top;
            let list_h = self.list_content_h();
            if list_h > view_h {
                let th = (view_h * view_h / list_h).max(24.0);
                let ty = top + (view_h - th) * (self.scroll_list / self.max_scroll_list());
                self.fill_round(
                    rect(SPLIT_X - 9.0, ty, SPLIT_X - 6.0, ty + th),
                    1.5,
                    theme::with_alpha(theme::TEXT_DIM, 0.5),
                );
            }
            let tiles_h = self.tiles_content_h();
            if tiles_h > view_h {
                let th = (view_h * view_h / tiles_h).max(24.0);
                let ty = top + (view_h - th) * (self.scroll_tiles / self.max_scroll_tiles());
                self.fill_round(
                    rect(self.w - 6.0, ty, self.w - 3.0, ty + th),
                    1.5,
                    theme::with_alpha(theme::TEXT_DIM, 0.5),
                );
            }

            // Footer
            if let Ok(b) = r.brush(theme::with_alpha(theme::TEXT_DIM, 0.25)) {
                r.dc.FillRectangle(&rect(12.0, bottom, self.w - 12.0, bottom + 1.0), &b);
            }
            if self.hover == Some(Act::User) {
                self.fill_round(
                    rect(
                        10.0,
                        bottom + (FOOTER_H - 34.0) / 2.0,
                        10.0 + USER_W,
                        bottom + (FOOTER_H + 34.0) / 2.0,
                    ),
                    6.0,
                    theme::HOVER_FILL,
                );
            }
            self.text(&[0xE77B], &self.fmt_glyph, rect(16.0, bottom, 44.0, self.h), theme::TEXT_DIM);
            self.text(
                &self.user,
                &self.fmt_item,
                rect(46.0, bottom, 10.0 + USER_W, self.h),
                theme::TEXT,
            );
            for (rc, act) in self.folder_rects() {
                if self.hover == Some(act) {
                    self.fill_round(rc, 6.0, theme::HOVER_FILL);
                }
                let glyph: u16 = match act {
                    Act::Docs => 0xE8A5,  // Document
                    _ => 0xE896,          // Download
                };
                self.text(&[glyph], &self.fmt_glyph, rc, theme::TEXT_DIM);
            }
            for (rc, act) in self.power_rects() {
                if self.hover == Some(act) {
                    self.fill_round(rc, 6.0, theme::HOVER_FILL);
                }
                let glyph: u16 = match act {
                    Act::Lock => 0xE72E,    // Lock
                    Act::Sleep => 0xE708,   // QuietHours (moon)
                    Act::Restart => 0xE72C, // Refresh
                    _ => 0xE7E8,            // PowerButton
                };
                self.text(&[glyph], &self.fmt_glyph, rc, theme::TEXT);
            }

            if let Some(d) = &self.drag {
                if d.live {
                    self.paint_drag(d);
                }
            }

            let _ = r.dc.EndDraw(None, None);
            let _ = self.renderer.present();
        }
    }

    /// Drop indicator + ghost tile at the cursor; painted over everything.
    fn paint_drag(&self, d: &Drag) {
        match d.spot {
            Some(DropSpot::Before(i)) => {
                let items = self.tile_items();
                let slots = self.item_slots(&items);
                let rc = if let Some(&(c, r, s)) = slots.get(i) {
                    let t = self.tile_rect(c, r, s, 0.0);
                    rect(t.left - 6.0, t.top, t.left - 2.0, t.bottom)
                } else if let Some(&(c, r, s)) = slots.last() {
                    let t = self.tile_rect(c, r, s, 0.0);
                    rect(t.right + 2.0, t.top, t.right + 6.0, t.bottom)
                } else {
                    rect(TILES_X0 - 6.0, self.list_top(), TILES_X0 - 2.0, self.list_top() + TILE)
                };
                self.fill_round(rc, 2.0, theme::accent());
            }
            Some(DropSpot::Into(i)) => {
                let items = self.tile_items();
                if let Some(&(c, r, s)) = self.item_slots(&items).get(i) {
                    self.accent_outline(self.tile_rect(c, r, s, 0.0));
                }
            }
            Some(DropSpot::MemberBefore(j)) => {
                if let Some(f) = &self.open_folder {
                    let slots = self.member_slots(&self.members(f));
                    let rc = if let Some(&(c, r, s)) = slots.get(j) {
                        let t = self.tile_rect(c, r, s, FOLDER_HEAD);
                        rect(t.left - 6.0, t.top, t.left - 2.0, t.bottom)
                    } else if let Some(&(c, r, s)) = slots.last() {
                        let t = self.tile_rect(c, r, s, FOLDER_HEAD);
                        rect(t.right + 2.0, t.top, t.right + 6.0, t.bottom)
                    } else {
                        return;
                    };
                    self.fill_round(rc, 2.0, theme::accent());
                }
            }
            Some(DropSpot::OutOfFolder) => {
                let top = self.list_top();
                self.accent_outline(rect(
                    TILES_X0,
                    top + 4.0,
                    self.w - 12.0,
                    top + FOLDER_HEAD - 4.0,
                ));
            }
            None => {}
        }
        let span = match &d.src {
            DragSrc::Pin(p) | DragSrc::Member(p) | DragSrc::App(p, _) => self
                .pins
                .iter()
                .find(|e| e.parsing == *p)
                .map_or(1, |e| if e.wide { 2 } else { 1 }),
            DragSrc::Folder(_) => 1,
        };
        let w = span as f32 * TILE + (span - 1) as f32 * TILE_GUT;
        let rc = rect(d.x - d.gx, d.y - d.gy, d.x - d.gx + w, d.y - d.gy + TILE);
        match &d.src {
            DragSrc::Folder(f) => self.paint_folder_tile(rc, f, &self.members(f), false),
            DragSrc::Pin(p) | DragSrc::Member(p) | DragSrc::App(p, _) => {
                match self.pins.iter().find(|e| e.parsing == *p) {
                    Some(e) => self.paint_tile(rc, e, false),
                    None => {
                        // Unpinned app row: synthesize the tile it would become.
                        if let DragSrc::App(p, n) = &d.src {
                            self.paint_tile(rc, &Entry::new(n.clone(), p.clone()), false);
                        }
                    }
                }
            }
        }
        self.accent_outline(rc);
    }

    fn accent_outline(&self, rc: D2D_RECT_F) {
        unsafe {
            if let Ok(b) = self.renderer.brush(theme::accent()) {
                self.renderer.dc.DrawRoundedRectangle(
                    &D2D1_ROUNDED_RECT { rect: rc, radiusX: 3.0, radiusY: 3.0 },
                    &b,
                    2.0,
                    None,
                );
            }
        }
    }

    /// Right pane: the pinned tile grid, or the open folder's contents under
    /// a fixed back-chip header.
    fn paint_tiles(&self, top: f32, bottom: f32) {
        if let Some(f) = &self.open_folder {
            let members = self.members(f);
            for (j, (&pi, &(c, r, s))) in
                members.iter().zip(&self.member_slots(&members)).enumerate()
            {
                self.paint_tile(
                    self.tile_rect(c, r, s, FOLDER_HEAD),
                    &self.pins[pi],
                    self.hover == Some(Act::FolderItem(j)),
                );
            }
            // Header painted after the grid so tiles scroll under it.
            unsafe {
                if let Ok(b) = self.renderer.brush(theme::rgba(26, 27, 32, 0.92)) {
                    self.renderer
                        .dc
                        .FillRectangle(&rect(SPLIT_X, top, self.w, top + FOLDER_HEAD), &b);
                }
            }
            let br = self.back_rect();
            if self.hover == Some(Act::FolderBack) {
                self.fill_round(br, 5.0, theme::HOVER_FILL);
            }
            let back: Vec<u16> = "‹ 뒤로".encode_utf16().collect();
            self.text(&back, &self.fmt_center, br, theme::TEXT);
            let name: Vec<u16> = f.encode_utf16().collect();
            self.text(
                &name,
                &self.fmt_head,
                rect(br.right + 10.0, top, self.w - 12.0, top + FOLDER_HEAD),
                theme::TEXT,
            );
            return;
        }
        if self.pins.is_empty() {
            let hint: Vec<u16> = "고정된 앱이 없습니다.\r왼쪽 목록에서 우클릭 → 고정"
                .encode_utf16()
                .collect();
            self.text(
                &hint,
                &self.fmt_center,
                rect(SPLIT_X, top, self.w, bottom),
                theme::TEXT_DIM,
            );
            return;
        }
        let items = self.tile_items();
        for (i, (item, &(c, r, s))) in items.iter().zip(&self.item_slots(&items)).enumerate() {
            let rc = self.tile_rect(c, r, s, 0.0);
            let hovered = self.hover == Some(Act::Tile(i));
            match item {
                TileItem::Single(pi) => self.paint_tile(rc, &self.pins[*pi], hovered),
                TileItem::Folder(name, ms) => self.paint_folder_tile(rc, name, ms, hovered),
            }
        }
    }

    /// "‹ 뒤로" chip in the folder-view header.
    fn back_rect(&self) -> D2D_RECT_F {
        let y = self.list_top();
        rect(TILES_X0, y + 6.0, TILES_X0 + 88.0, y + FOLDER_HEAD - 6.0)
    }

    /// Metro tile: dominant-color background, centered icon, label inside
    /// bottom-left. Hover = light wash + hairline border (the Win10 look).
    fn paint_tile(&self, rc: D2D_RECT_F, e: &Entry, hovered: bool) {
        if rc.bottom < self.list_top() || rc.top > self.list_bottom() {
            return;
        }
        let bg = self
            .tints
            .get(&e.parsing)
            .copied()
            .unwrap_or(theme::rgba(255, 255, 255, 0.07));
        self.fill_round(rc, 3.0, bg);
        if hovered {
            self.hover_deco(rc);
        }
        let icx = (rc.left + rc.right) / 2.0;
        let icy = rc.top + (TILE - TILE_ICON) / 2.0 - 8.0;
        self.draw_icon(
            &e.parsing,
            rect(icx - TILE_ICON / 2.0, icy, icx + TILE_ICON / 2.0, icy + TILE_ICON),
        );
        self.text(
            &e.wname,
            &self.fmt_tile,
            rect(rc.left + 9.0, rc.top, rc.right - 9.0, rc.bottom - 7.0),
            theme::TEXT,
        );
    }

    /// Hover wash + white hairline shared by tile kinds.
    fn hover_deco(&self, rc: D2D_RECT_F) {
        self.fill_round(rc, 3.0, theme::HOVER_FILL);
        unsafe {
            if let Ok(b) = self.renderer.brush(theme::rgba(255, 255, 255, 0.45)) {
                self.renderer.dc.DrawRoundedRectangle(
                    &D2D1_ROUNDED_RECT { rect: rc, radiusX: 3.0, radiusY: 3.0 },
                    &b,
                    1.5,
                    None,
                );
            }
        }
    }

    /// Folder tile: neutral slab, up to four member icons in a 2×2 preview,
    /// group name bottom-left like a normal tile.
    fn paint_folder_tile(&self, rc: D2D_RECT_F, name: &str, members: &[usize], hovered: bool) {
        if rc.bottom < self.list_top() || rc.top > self.list_bottom() {
            return;
        }
        self.fill_round(rc, 3.0, theme::rgba(255, 255, 255, 0.07));
        if hovered {
            self.hover_deco(rc);
        }
        for (k, &pi) in members.iter().take(4).enumerate() {
            let x = rc.left + 14.0 + (k % 2) as f32 * 40.0;
            let y = rc.top + 10.0 + (k / 2) as f32 * 34.0;
            self.draw_icon(&self.pins[pi].parsing, rect(x, y, x + 30.0, y + 30.0));
        }
        let label: Vec<u16> = name.encode_utf16().collect();
        self.text(
            &label,
            &self.fmt_tile,
            rect(rc.left + 9.0, rc.top, rc.right - 9.0, rc.bottom - 7.0),
            theme::TEXT,
        );
    }

    /// Type-to-search result rows (left pane); `selected` gets the accent.
    fn paint_search(&self, top: f32, bottom: f32) {
        if self.results.is_empty() {
            let msg: Vec<u16> = "일치하는 앱 없음".encode_utf16().collect();
            self.text(&msg, &self.fmt_center, rect(0.0, top, SPLIT_X, bottom), theme::TEXT_DIM);
            return;
        }
        for (i, &a) in self.results.iter().enumerate() {
            let y = top + i as f32 * ROW_H - self.scroll_list;
            if y + ROW_H < top || y > bottom {
                continue;
            }
            let app = &self.apps[a];
            if i == self.selected {
                self.fill_round(
                    rect(8.0, y + 1.0, SPLIT_X - 12.0, y + ROW_H - 1.0),
                    6.0,
                    theme::with_alpha(theme::accent(), 0.22),
                );
            } else if self.hover == Some(Act::Result(i)) {
                self.fill_round(
                    rect(8.0, y + 1.0, SPLIT_X - 12.0, y + ROW_H - 1.0),
                    6.0,
                    theme::HOVER_FILL,
                );
            }
            self.paint_row_entry(app, y);
        }
    }

    /// Icon + name for one list row, shared by search/apps/freq painters.
    fn paint_row_entry(&self, e: &Entry, y: f32) {
        let islot = rect(
            16.0,
            y + (ROW_H - LIST_ICON) / 2.0,
            16.0 + LIST_ICON,
            y + (ROW_H + LIST_ICON) / 2.0,
        );
        self.draw_icon(&e.parsing, islot);
        self.text(
            &e.wname,
            &self.fmt_item,
            rect(16.0 + LIST_ICON + 10.0, y, SPLIT_X - 14.0, y + ROW_H),
            theme::TEXT,
        );
    }

    /// Left pane: 자주 사용 rows on top, then the sectioned app list.
    fn paint_apps(&self, top: f32, bottom: f32) {
        if self.loading && self.apps.is_empty() {
            let msg: Vec<u16> = "앱 목록 불러오는 중…".encode_utf16().collect();
            self.text(&msg, &self.fmt_center, rect(0.0, top, SPLIT_X, bottom), theme::TEXT_DIM);
            return;
        }
        for (i, row) in self.rows.iter().enumerate() {
            let y = top + self.row_pos[i] - self.scroll_list;
            match row {
                Row::Section(label) => {
                    if y + SECTION_H < top || y > bottom {
                        continue;
                    }
                    self.text(
                        label,
                        &self.fmt_section,
                        rect(18.0, y, SPLIT_X - 12.0, y + SECTION_H),
                        theme::accent(),
                    );
                }
                Row::App(_) | Row::Freq(_) => {
                    if y + ROW_H < top || y > bottom {
                        continue;
                    }
                    let (entry, act) = match row {
                        Row::App(a) => (self.apps.get(*a), Act::App(*a)),
                        Row::Freq(f) => (self.freq.get(*f), Act::Freq(*f)),
                        Row::Section(_) => unreachable!(),
                    };
                    let Some(entry) = entry else { continue };
                    if self.hover == Some(act) {
                        self.fill_round(
                            rect(8.0, y + 1.0, SPLIT_X - 12.0, y + ROW_H - 1.0),
                            6.0,
                            theme::HOVER_FILL,
                        );
                    }
                    self.paint_row_entry(entry, y);
                }
            }
        }
    }

    /// Aspect-fit the cached icon into `slot`; dim placeholder until (or if)
    /// the worker delivers.
    fn draw_icon(&self, parsing: &str, slot: D2D_RECT_F) {
        match self.icons.get(parsing) {
            Some(Some(bmp)) => unsafe {
                let sz = bmp.GetSize();
                if sz.width <= 0.0 || sz.height <= 0.0 {
                    return;
                }
                let sw = slot.right - slot.left;
                let sh = slot.bottom - slot.top;
                let s = (sw / sz.width).min(sh / sz.height);
                let (dw, dh) = (sz.width * s, sz.height * s);
                let dst = rect(
                    slot.left + (sw - dw) / 2.0,
                    slot.top + (sh - dh) / 2.0,
                    slot.left + (sw - dw) / 2.0 + dw,
                    slot.top + (sh - dh) / 2.0 + dh,
                );
                self.renderer.dc.DrawBitmap(
                    bmp,
                    Some(&dst),
                    1.0,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    None,
                    None,
                );
            },
            _ => {
                self.fill_round(slot, 5.0, theme::with_alpha(theme::TEXT_DIM, 0.3));
            }
        }
    }

    fn text(&self, s: &[u16], fmt: &IDWriteTextFormat, rc: D2D_RECT_F, color: D2D1_COLOR_F) {
        unsafe {
            if let Ok(b) = self.renderer.brush(color) {
                self.renderer.dc.DrawText(
                    s,
                    fmt,
                    &rc,
                    &b,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
    }

    fn fill_round(&self, rc: D2D_RECT_F, radius: f32, color: D2D1_COLOR_F) {
        fill_round(&self.renderer, rc, radius, color);
    }
}

fn launch(cmd: &[u16]) {
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(cmd.as_ptr()), None, None, SW_SHOWNORMAL);
    }
}

// ---- pins persistence ------------------------------------------------------

fn start_pins_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("glide-shell").join("start_pins.txt")
}

/// One pin per line: `{parsing}\t{display name}\t{wide 0|1}\t{folder}`
/// (trailing fields optional for files written by older builds).
fn load_start_pins() -> Vec<Entry> {
    std::fs::read_to_string(start_pins_path())
        .map(|s| {
            s.lines()
                .filter_map(|l| {
                    let mut it = l.splitn(4, '\t');
                    let parsing = it.next()?;
                    let name = it.next()?;
                    if parsing.is_empty() {
                        return None;
                    }
                    let mut e = Entry::new(name.to_string(), parsing.to_string());
                    e.wide = it.next() == Some("1");
                    e.folder = it.next().filter(|f| !f.is_empty()).map(String::from);
                    Some(e)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_start_pins(pins: &[Entry]) {
    let p = start_pins_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body: String = pins
        .iter()
        .map(|e| {
            format!(
                "{}\t{}\t{}\t{}\n",
                e.parsing,
                e.name,
                e.wide as u8,
                e.folder.as_deref().unwrap_or("")
            )
        })
        .collect();
    let _ = std::fs::write(p, body);
}

// ---- launch counts (자주 사용) ----------------------------------------------

fn counts_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("glide-shell").join("start_counts.txt")
}

/// One app per line: `{count}\t{parsing}\t{display name}`.
fn load_counts() -> HashMap<String, (u32, String)> {
    std::fs::read_to_string(counts_path())
        .map(|s| {
            s.lines()
                .filter_map(|l| {
                    let mut it = l.splitn(3, '\t');
                    let count: u32 = it.next()?.parse().ok()?;
                    let parsing = it.next()?.to_string();
                    let name = it.next()?.to_string();
                    Some((parsing, (count, name)))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_counts(counts: &HashMap<String, (u32, String)>) {
    let p = counts_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body: String = counts
        .iter()
        .map(|(parsing, (count, name))| format!("{count}\t{parsing}\t{name}\n"))
        .collect();
    let _ = std::fs::write(p, body);
}

// ---- pin reordering ---------------------------------------------------------
//
// Drag-and-drop reorders pins by index, and the indices come from tile geometry
// that is rebuilt on every layout pass. A stale or repeated index there used to
// panic — and a panic on the shell's UI thread takes the desktop with it — so
// both permutations below are total: unusable indices are skipped, and no pin is
// ever dropped on the floor.

/// Reorder `pins` to follow `order`, appending anything `order` left out.
fn permute(pins: Vec<Entry>, order: &[usize]) -> Vec<Entry> {
    let mut old: Vec<Option<Entry>> = pins.into_iter().map(Some).collect();
    let mut out: Vec<Entry> = order
        .iter()
        .filter_map(|&i| old.get_mut(i).and_then(Option::take))
        .collect();
    out.extend(old.into_iter().flatten());
    out
}

/// Rewrite only the entries occupying `slots`, filling those slots with the
/// entries named by `order` — a folder's members shuffle among themselves while
/// the folder tile keeps its position in the grid.
fn permute_slots(pins: Vec<Entry>, slots: &[usize], order: &[usize]) -> Vec<Entry> {
    let mut old: Vec<Option<Entry>> = pins.into_iter().map(Some).collect();
    let moved: Vec<Entry> = order
        .iter()
        .filter_map(|&i| old.get_mut(i).and_then(Option::take))
        .collect();
    // `moved` holds exactly what was taken, so zip places every one of them
    // even when `order` came out shorter than `slots`.
    for (slot, e) in slots.iter().zip(moved) {
        if let Some(s) = old.get_mut(*slot) {
            *s = Some(e);
        }
    }
    old.into_iter().flatten().collect()
}

// ---- sorting ----------------------------------------------------------------

/// Sort/section class: 0 = digits & symbols ("#"), 1 = 한글 초성, 2 = A–Z.
fn section_of(name: &str) -> (u8, char) {
    const CHO: [char; 19] = [
        'ㄱ', 'ㄱ', 'ㄴ', 'ㄷ', 'ㄷ', 'ㄹ', 'ㅁ', 'ㅂ', 'ㅂ', 'ㅅ', 'ㅅ', 'ㅇ', 'ㅈ', 'ㅈ',
        'ㅊ', 'ㅋ', 'ㅌ', 'ㅍ', 'ㅎ',
    ];
    let c = name.chars().next().unwrap_or('#');
    if ('가'..='힣').contains(&c) {
        (1, CHO[(c as usize - 0xAC00) / 588])
    } else if c.is_ascii_alphabetic() {
        (2, c.to_ascii_uppercase())
    } else {
        (0, '#')
    }
}

/// Search haystack for 초성 queries: 한글 syllables collapse to their initial
/// consonant (full 19-jamo table — ㄲㄸㅃㅆㅉ stay distinct), everything else
/// lowercases, so "ㅋㄹ" finds 크롬 and "ㄱㅁ" finds 게임.
fn name_cho(name: &str) -> String {
    const CHO_FULL: [char; 19] = [
        'ㄱ', 'ㄲ', 'ㄴ', 'ㄷ', 'ㄸ', 'ㄹ', 'ㅁ', 'ㅂ', 'ㅃ', 'ㅅ', 'ㅆ', 'ㅇ', 'ㅈ', 'ㅉ',
        'ㅊ', 'ㅋ', 'ㅌ', 'ㅍ', 'ㅎ',
    ];
    name.chars()
        .flat_map(|c| {
            if ('가'..='힣').contains(&c) {
                CHO_FULL[(c as usize - 0xAC00) / 588].to_lowercase()
            } else {
                c.to_lowercase()
            }
        })
        .collect()
}

// ---- worker thread -----------------------------------------------------------

/// MTA COM worker: AppsFolder enumeration and icon extraction. Sends replies
/// and pokes the UI window with WM_APP_REPLY; exits when the channel closes.
fn worker(jobs: Receiver<Job>, replies: Sender<Reply>, hwnd_raw: isize) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    crate::safety::note("start menu: worker up");
    let hwnd = HWND(hwnd_raw as *mut _);
    while let Ok(job) = jobs.recv() {
        let rep = match job {
            Job::Apps { epoch } => Reply::Apps { epoch, list: apps_or_fallback() },
            Job::Icon { parsing } => {
                let (w, h, pixels) = extract_icon(&parsing).unwrap_or((0, 0, Vec::new()));
                Reply::Icon { parsing, w, h, pixels }
            }
        };
        if replies.send(rep).is_err() {
            break;
        }
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_REPLY, WPARAM(0), LPARAM(0));
        }
    }
}

/// How long AppsFolder gets before the Start menu stops waiting for it.
const APPS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// AppsFolder, or the Start Menu shortcut trees when it does not answer.
///
/// On a Winlogon-started shell with no explorer, the AppsFolder enumeration
/// does not fail — it never returns, and because it sits at the head of the one
/// COM worker queue it takes every icon job down with it, so the menu stays on
/// "loading" forever. The enumeration therefore runs on a thread of its own
/// that we are willing to abandon: a hung COM call cannot be cancelled, so the
/// thread is left blocked and the queue moves on.
fn apps_or_fallback() -> Vec<(String, String)> {
    let t0 = std::time::Instant::now();
    crate::safety::note("start menu: enumerating apps");
    let (tx, rx) = channel::<Vec<(String, String)>>();
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let _ = tx.send(enum_apps());
    });
    match rx.recv_timeout(APPS_TIMEOUT) {
        Ok(list) if !list.is_empty() => {
            crate::safety::note(&format!(
                "start menu: {} apps from AppsFolder in {}ms",
                list.len(),
                t0.elapsed().as_millis()
            ));
            list
        }
        Ok(_) => {
            let list = start_menu_apps();
            crate::safety::note(&format!(
                "start menu: AppsFolder empty, {} apps from the Start Menu trees",
                list.len()
            ));
            list
        }
        Err(_) => {
            let list = start_menu_apps();
            crate::safety::note(&format!(
                "start menu: AppsFolder did not answer in {}s, {} apps from the Start Menu trees",
                APPS_TIMEOUT.as_secs(),
                list.len()
            ));
            list
        }
    }
}

/// Every `.lnk` under the common and per-user Start Menu trees, which is where
/// installers put shortcuts and what Start showed for the fifteen years before
/// AppsFolder existed. Display name is the shortcut's own; the parsing name is
/// its absolute path, which `parse_target` passes through untouched.
fn start_menu_apps() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>, depth: u32) {
        if depth > 6 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out, depth + 1);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")) {
                let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else { continue };
                out.push((stem.to_string(), p.to_string_lossy().into_owned()));
            }
        }
    }

    let mut out: Vec<(String, String)> = Vec::new();
    for var in ["ProgramData", "APPDATA"] {
        let Ok(base) = std::env::var(var) else { continue };
        walk(
            &std::path::Path::new(&base).join(r"Microsoft\Windows\Start Menu\Programs"),
            &mut out,
            0,
        );
    }
    // Same program installed for the machine and the user is one entry, as
    // Start shows it.
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    out.dedup_by(|a, b| a.0.eq_ignore_ascii_case(&b.0));
    out.sort_by_cached_key(|(name, _)| {
        let (class, ch) = section_of(name);
        (class, ch, name.to_lowercase())
    });
    out
}

/// (display name, parsing name) for everything the stock Start shows,
/// pre-sorted #→한글→ABC.
fn enum_apps() -> Vec<(String, String)> {
    let mut out = Vec::new();
    unsafe {
        let folder: IShellItem =
            match SHCreateItemFromParsingName(w!("shell:AppsFolder"), None) {
                Ok(f) => f,
                Err(e) => {
                    crate::safety::note(&format!("start menu: no shell:AppsFolder: {e:?}"));
                    return out;
                }
            };
        let en: IEnumShellItems = match folder.BindToHandler(None, &BHID_EnumItems) {
            Ok(e) => e,
            Err(e) => {
                crate::safety::note(&format!("start menu: AppsFolder BindToHandler: {e:?}"));
                return out;
            }
        };
        loop {
            let mut batch: [Option<IShellItem>; 1] = [None];
            let mut got = 0u32;
            if let Err(e) = en.Next(&mut batch, Some(&mut got)) {
                crate::safety::note(&format!(
                    "start menu: AppsFolder enumeration stopped after {} items: {e:?}",
                    out.len()
                ));
                break;
            }
            if got == 0 {
                break;
            }
            let Some(item) = batch[0].take() else { break };
            let Some(name) = take_pwstr(item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()) else {
                continue;
            };
            let Some(rel) = take_pwstr(item.GetDisplayName(SIGDN_PARENTRELATIVEPARSING).ok())
            else {
                continue;
            };
            out.push((name, rel));
        }
    }
    out.sort_by_cached_key(|(name, _)| {
        let (class, ch) = section_of(name);
        (class, ch, name.to_lowercase())
    });
    out
}

/// Premultiplied BGRA pixels at the bitmap's REAL size (GetObjectW) — the
/// shell does not always honor the requested size, and reading a mismatched
/// buffer is what mangled the first cut of these icons.
fn extract_icon(parsing: &str) -> Option<(i32, i32, Vec<u8>)> {
    unsafe {
        let path: Vec<u16> = parse_target(parsing)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None).ok()?;
        let factory: IShellItemImageFactory = item.cast().ok()?;
        let hbmp = factory
            .GetImage(
                windows::Win32::Foundation::SIZE { cx: ICON_PX, cy: ICON_PX },
                SIIGBF_RESIZETOFIT,
            )
            .ok()?;
        let result = (|| {
            let mut bm = BITMAP::default();
            if GetObjectW(
                hbmp.into(),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bm as *mut _ as *mut _),
            ) == 0
                || bm.bmWidth <= 0
                || bm.bmHeight <= 0
            {
                return None;
            }
            let (w, h) = (bm.bmWidth, bm.bmHeight);
            let hdc = CreateCompatibleDC(None);
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            let got = GetDIBits(
                hdc,
                hbmp,
                0,
                h as u32,
                Some(pixels.as_mut_ptr() as *mut _),
                &mut bi,
                DIB_RGB_COLORS,
            );
            let _ = DeleteDC(hdc);
            if got == 0 {
                return None;
            }
            // Dead alpha channel (24bpp sources) — force opaque. Live alpha
            // is straight; premultiply for the swapchain format.
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
            Some((w, h, pixels))
        })();
        let _ = DeleteObject(hbmp.into());
        result
    }
}

/// Dominant color of an icon (premultiplied BGRA), normalized to a dark-theme
/// tile background. Colored pixels vote; near-grayscale ones don't, so white
/// glyph icons fall back to the neutral tile instead of washing out gray.
fn tint_of(pixels: &[u8]) -> Option<(f32, f32, f32)> {
    let (mut rs, mut gs, mut bs, mut n) = (0u64, 0u64, 0u64, 0u64);
    for p in pixels.chunks_exact(4) {
        let a = p[3] as u32;
        if a < 200 {
            continue;
        }
        let b = (p[0] as u32 * 255 / a).min(255);
        let g = (p[1] as u32 * 255 / a).min(255);
        let r = (p[2] as u32 * 255 / a).min(255);
        let mx = r.max(g).max(b);
        let mn = r.min(g).min(b);
        if mx - mn < 24 {
            continue;
        }
        rs += r as u64;
        gs += g as u64;
        bs += b as u64;
        n += 1;
    }
    if n < 32 {
        return None;
    }
    let (r, g, b) = ((rs / n) as f32, (gs / n) as f32, (bs / n) as f32);
    let mx = r.max(g).max(b).max(1.0);
    // Darken to tile depth (max channel ≈ 0.46) with a touch of gray so
    // saturated brand colors don't go neon against the dark panel.
    let k = 118.0 / mx;
    let mix = |c: f32| (c * k * 0.88 + 14.0) / 255.0;
    Some((mix(r), mix(g), mix(b)))
}

/// Read and free a shell-allocated display-name string.
fn take_pwstr(p: Option<windows::core::PWSTR>) -> Option<String> {
    let p = p?;
    unsafe {
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn open_home() {
    open_dir(std::env::var_os("USERPROFILE").map(PathBuf::from));
}

fn open_profile_dir(sub: &str) {
    open_dir(
        std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(sub)),
    );
}

fn open_dir(dir: Option<PathBuf>) {
    let Some(dir) = dir else { return };
    unsafe {
        let wide: Vec<u16> = dir
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), None, None, SW_SHOWNORMAL);
    }
}

fn run_shutdown(args: PCWSTR) {
    unsafe {
        ShellExecuteW(None, None, w!("shutdown.exe"), args, None, SW_HIDE);
    }
}

/// Owned right-click target — survives TrackPopupMenu's reentrant pump.
enum RTarget {
    /// App list / search / 자주 사용 rows: pin/unpin only.
    AppLike { parsing: String, name: String, pinned: bool },
    /// Loose pin tile: unpin, resize, add to a group.
    Tile { parsing: String, name: String, wide: bool },
    /// Folder tile: dissolve.
    FolderTile { name: String },
    /// Tile inside the open folder: pull out, unpin, resize.
    Member { parsing: String, name: String, wide: bool },
}

extern "system" fn start_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut StartMenu;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let sm = &mut *ptr;
        let lx = |s: &StartMenu| (lparam.0 & 0xFFFF) as i16 as f32 / s.scale;
        let ly = |s: &StartMenu| ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 / s.scale;
        match msg {
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                sm.paint();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_APP_REPLY => {
                sm.on_replies();
                LRESULT(0)
            }
            WM_ACTIVATE => {
                if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    sm.dismiss();
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = (lx(sm), ly(sm));
                if sm.drag.is_some() {
                    sm.drag_move(x, y);
                    return LRESULT(0);
                }
                let h = sm.hit(x, y);
                if h != sm.hover {
                    sm.hover = h;
                    sm.paint();
                }
                if !sm.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    if TrackMouseEvent(&mut tme).is_ok() {
                        sm.tracking = true;
                    }
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let (x, y) = (lx(sm), ly(sm));
                sm.arm_drag(x, y);
                if sm.drag.is_some() {
                    SetCapture(hwnd);
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                // Capture stolen (alt-tab, menu) — the drag dies where it was.
                if sm.drag.take().is_some() {
                    sm.paint();
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                sm.tracking = false;
                if sm.hover.take().is_some() {
                    sm.paint();
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let (x, y) = (lx(sm), ly(sm));
                // Take the drag out first: ReleaseCapture reenters this proc
                // as WM_CAPTURECHANGED, which must find nothing to cancel.
                let drag = sm.drag.take();
                if GetCapture() == hwnd {
                    let _ = ReleaseCapture();
                }
                let sm = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut StartMenu);
                match drag {
                    Some(d) if d.live => sm.commit_drop(d),
                    _ => {
                        if let Some(a) = sm.hit(x, y) {
                            sm.act(a);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let (x, y) = (lx(sm), ly(sm));
                // Context menu. TrackPopupMenu pumps this wndproc reentrantly
                // (same lesson as desktop.rs) — collect owned data, let the
                // borrow lapse across the modal call, then re-deref
                // GWLP_USERDATA.
                let target: Option<RTarget> = match sm.hit(x, y) {
                    Some(Act::App(i)) => sm.apps.get(i).map(|a| RTarget::AppLike {
                        parsing: a.parsing.clone(),
                        name: a.name.clone(),
                        pinned: sm.pins.iter().any(|p| p.parsing == a.parsing),
                    }),
                    Some(Act::Result(i)) => {
                        sm.results.get(i).and_then(|&a| sm.apps.get(a)).map(|a| {
                            RTarget::AppLike {
                                parsing: a.parsing.clone(),
                                name: a.name.clone(),
                                pinned: sm.pins.iter().any(|p| p.parsing == a.parsing),
                            }
                        })
                    }
                    Some(Act::Freq(i)) => sm.freq.get(i).map(|f| RTarget::AppLike {
                        parsing: f.parsing.clone(),
                        name: f.name.clone(),
                        pinned: false,
                    }),
                    Some(Act::Tile(i)) => match sm.tile_items().get(i) {
                        Some(TileItem::Single(pi)) => sm.pins.get(*pi).map(|p| RTarget::Tile {
                            parsing: p.parsing.clone(),
                            name: p.name.clone(),
                            wide: p.wide,
                        }),
                        Some(TileItem::Folder(n, _)) => {
                            Some(RTarget::FolderTile { name: n.clone() })
                        }
                        None => None,
                    },
                    Some(Act::FolderItem(j)) => sm.open_folder.clone().and_then(|f| {
                        sm.members(&f).get(j).and_then(|&pi| sm.pins.get(pi)).map(|p| {
                            RTarget::Member {
                                parsing: p.parsing.clone(),
                                name: p.name.clone(),
                                wide: p.wide,
                            }
                        })
                    }),
                    _ => None,
                };
                if let Some(target) = target {
                    let groups = sm.groups();
                    let menu = match CreatePopupMenu() {
                        Ok(m) => m,
                        Err(_) => return LRESULT(0),
                    };
                    let wide_label = |wide: bool| {
                        if wide { w!("정사각 타일로") } else { w!("와이드 타일로") }
                    };
                    match &target {
                        RTarget::AppLike { pinned, .. } => {
                            let label = if *pinned {
                                w!("시작 화면에서 제거")
                            } else {
                                w!("시작 화면에 고정")
                            };
                            let _ = AppendMenuW(menu, MF_STRING, MENU_TOGGLE_PIN, label);
                        }
                        RTarget::Tile { wide, .. } => {
                            let _ = AppendMenuW(
                                menu,
                                MF_STRING,
                                MENU_TOGGLE_PIN,
                                w!("시작 화면에서 제거"),
                            );
                            let _ =
                                AppendMenuW(menu, MF_STRING, MENU_TOGGLE_WIDE, wide_label(*wide));
                            // AppendMenuW copies MF_STRING text, so the wide
                            // group names may drop before TrackPopupMenu.
                            if let Ok(sub) = CreatePopupMenu() {
                                let _ = AppendMenuW(sub, MF_STRING, MENU_NEW_GROUP, w!("새 그룹"));
                                for (gi, g) in groups.iter().enumerate() {
                                    let wg: Vec<u16> =
                                        g.encode_utf16().chain(std::iter::once(0)).collect();
                                    let _ = AppendMenuW(
                                        sub,
                                        MF_STRING,
                                        MENU_GROUP_BASE + gi,
                                        PCWSTR(wg.as_ptr()),
                                    );
                                }
                                let _ =
                                    AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("그룹에 추가"));
                            }
                        }
                        RTarget::FolderTile { .. } => {
                            let _ = AppendMenuW(menu, MF_STRING, MENU_DISSOLVE, w!("그룹 해제"));
                        }
                        RTarget::Member { wide, .. } => {
                            let _ =
                                AppendMenuW(menu, MF_STRING, MENU_UNGROUP, w!("그룹에서 빼기"));
                            let _ = AppendMenuW(
                                menu,
                                MF_STRING,
                                MENU_TOGGLE_PIN,
                                w!("시작 화면에서 제거"),
                            );
                            let _ =
                                AppendMenuW(menu, MF_STRING, MENU_TOGGLE_WIDE, wide_label(*wide));
                        }
                    }
                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    let cmd = TrackPopupMenu(
                        menu,
                        TPM_RIGHTBUTTON | TPM_RETURNCMD,
                        pt.x,
                        pt.y,
                        Some(0),
                        hwnd,
                        None,
                    );
                    let _ = DestroyMenu(menu);
                    let sm = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut StartMenu);
                    let (parsing, name) = match &target {
                        RTarget::AppLike { parsing, name, .. }
                        | RTarget::Tile { parsing, name, .. }
                        | RTarget::Member { parsing, name, .. } => {
                            (parsing.clone(), name.clone())
                        }
                        RTarget::FolderTile { name } => (String::new(), name.clone()),
                    };
                    match cmd.0 as usize {
                        MENU_TOGGLE_PIN => sm.toggle_pin(&parsing, &name),
                        MENU_TOGGLE_WIDE => sm.toggle_wide(&parsing),
                        MENU_DISSOLVE => sm.dissolve(&name),
                        MENU_UNGROUP => sm.set_folder(&parsing, None),
                        MENU_NEW_GROUP => {
                            let g = sm.next_group_name();
                            sm.set_folder(&parsing, Some(g));
                        }
                        c if c >= MENU_GROUP_BASE => {
                            if let Some(g) = groups.get(c - MENU_GROUP_BASE) {
                                sm.set_folder(&parsing, Some(g.clone()));
                            }
                        }
                        _ => {}
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                // lparam is screen coords here; map to client to pick the pane.
                let notches = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32 / 120.0;
                let mut pt = POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                let _ = ScreenToClient(hwnd, &mut pt);
                sm.wheel(pt.x as f32 / sm.scale, notches);
                LRESULT(0)
            }
            WM_CHAR => {
                let c = wparam.0 as u32;
                if c == 0x08 {
                    // Backspace
                    if sm.query.pop().is_some() {
                        sm.update_results();
                        sm.paint();
                    }
                } else if c >= 0x20 {
                    if let Some(ch) = char::from_u32(c) {
                        sm.query.push(ch);
                        sm.update_results();
                        sm.paint();
                    }
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wparam.0 == VK_ESCAPE.0 as usize {
                    // Esc backs out of a live drag, search, an open folder,
                    // then closes.
                    if sm.drag.take().is_some() {
                        if GetCapture() == hwnd {
                            let _ = ReleaseCapture();
                        }
                        let sm =
                            &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut StartMenu);
                        sm.paint();
                        return LRESULT(0);
                    }
                    if !sm.query.is_empty() {
                        sm.query.clear();
                        sm.update_results();
                        sm.paint();
                    } else if sm.open_folder.is_some() {
                        sm.open_folder = None;
                        sm.scroll_tiles = 0.0;
                        sm.hover = None;
                        sm.paint();
                    } else {
                        sm.hide();
                    }
                } else if !sm.query.is_empty() {
                    match VIRTUAL_KEY(wparam.0 as u16) {
                        VK_DOWN => {
                            if sm.selected + 1 < sm.results.len() {
                                sm.selected += 1;
                                sm.ensure_selected_visible();
                                sm.paint();
                            }
                        }
                        VK_UP => {
                            if sm.selected > 0 {
                                sm.selected -= 1;
                                sm.ensure_selected_visible();
                                sm.paint();
                            }
                        }
                        VK_RETURN => {
                            let sel = sm.selected;
                            if sel < sm.results.len() {
                                sm.act(Act::Result(sel));
                            }
                        }
                        _ => {}
                    }
                }
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == TIMER_REFRESH {
                    sm.refresh();
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, permute, permute_slots};

    fn pins(names: &[&str]) -> Vec<Entry> {
        names.iter().map(|n| Entry::new(n.to_string(), n.to_string())).collect()
    }

    fn names(pins: &[Entry]) -> Vec<&str> {
        pins.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn permute_reorders() {
        let out = permute(pins(&["a", "b", "c"]), &[2, 0, 1]);
        assert_eq!(names(&out), ["c", "a", "b"]);
    }

    #[test]
    fn permute_survives_repeated_index() {
        // The old `old[i].take().unwrap()` panicked on the second visit to 0.
        let out = permute(pins(&["a", "b", "c"]), &[0, 0, 1, 2]);
        assert_eq!(names(&out), ["a", "b", "c"]);
    }

    #[test]
    fn permute_survives_stale_index() {
        let out = permute(pins(&["a", "b"]), &[9, 1, 0]);
        assert_eq!(names(&out), ["b", "a"]);
    }

    #[test]
    fn permute_keeps_unmentioned_pins() {
        let out = permute(pins(&["a", "b", "c"]), &[2]);
        assert_eq!(names(&out), ["c", "a", "b"]);
    }

    #[test]
    fn permute_slots_shuffles_within_slots() {
        // Folder members live at pins 1 and 3; swapping them must leave 0 and 2
        // exactly where they are.
        let out = permute_slots(pins(&["a", "b", "c", "d"]), &[1, 3], &[3, 1]);
        assert_eq!(names(&out), ["a", "d", "c", "b"]);
    }

    #[test]
    fn permute_slots_survives_stale_index() {
        let out = permute_slots(pins(&["a", "b", "c"]), &[0, 2], &[7, 2, 0]);
        assert_eq!(names(&out).len(), 3);
    }
}
