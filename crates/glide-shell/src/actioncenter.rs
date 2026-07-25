//! Win10-style unified action center (SHELL_DESIGN §6.8): the missed-toast
//! backlog on top, a Quick Settings tile grid below. One panel, so settings
//! that don't earn a permanent taskbar cell (Bluetooth, airplane, rotation
//! lock) still have a home.
//!
//! The backlog is a live view of the real OS action center via
//! `UserNotificationListener` — we keep no history of our own; dismissing a
//! row calls `RemoveNotification`, and it vanishes from Windows too. All
//! WinRT `.join()` and radio work runs on short-lived MTA worker threads
//! (this window lives on the bar's STA); workers post `WM_AC_REFRESH` back
//! and the UI drains a snapshot. Everything paints from plain owned fields,
//! never under a lock.

use std::sync::{Arc, Mutex};

use windows::UI::Notifications::Management::{
    UserNotificationListener, UserNotificationListenerAccessStatus,
};
use windows::UI::Notifications::{KnownNotificationBindings, NotificationKinds};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ROUNDED_RECT};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_TRAILING,
    DWRITE_WORD_WRAPPING_NO_WRAP, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dwm::{
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::quicksettings::{self, Tri};
use crate::render::Renderer;
use crate::theme;

const WM_MOUSELEAVE: u32 = 0x02A3;
/// Worker → UI: a fresh notification + Quick Settings snapshot is waiting.
pub const WM_AC_REFRESH: u32 = WM_APP + 18;
const TIMER_REFRESH: usize = 1;

const W: f32 = 380.0;
const PAD: f32 = 12.0;
const HEADER_H: f32 = 30.0;
const ROW_H: f32 = 58.0;
const ROW_GAP: f32 = 6.0;
const TILE_H: f32 = 60.0;
const TILE_GAP: f32 = 8.0;
const MAXH: f32 = 600.0;

#[derive(Clone)]
struct Notif {
    id: u32,
    app: String,
    title: String,
    body: String,
}

#[derive(Clone, Copy)]
struct Qs {
    wifi: bool,
    bluetooth: Tri,
    airplane: Tri,
    rotate_supported: bool,
    rotate_on: bool,
}

impl Default for Qs {
    fn default() -> Self {
        Qs {
            wifi: false,
            bluetooth: Tri::Absent,
            airplane: Tri::Absent,
            rotate_supported: false,
            rotate_on: false,
        }
    }
}

struct Snap {
    notifs: Vec<Notif>,
    qs: Qs,
}

#[derive(Clone, Copy, PartialEq)]
enum Tile {
    Wifi,
    Bluetooth,
    Airplane,
    Rotate,
    NightLight,
    Settings,
}

const TILES: [Tile; 6] = [
    Tile::Wifi,
    Tile::Bluetooth,
    Tile::Airplane,
    Tile::Rotate,
    Tile::NightLight,
    Tile::Settings,
];

#[derive(Clone, Copy, PartialEq)]
enum Act {
    ClearAll,
    Dismiss(u32),
    Tap(Tile),
}

pub struct ActionCenter {
    hwnd: HWND,
    renderer: Renderer,
    fmt_head: IDWriteTextFormat,
    fmt_link: IDWriteTextFormat,
    fmt_app: IDWriteTextFormat,
    fmt_title: IDWriteTextFormat,
    fmt_body: IDWriteTextFormat,
    fmt_tile: IDWriteTextFormat,
    fmt_glyph: IDWriteTextFormat,
    fmt_glyph_big: IDWriteTextFormat,
    scale: f32,
    w: f32,
    h: f32,
    anchor_right: i32,
    anchor_bottom: i32,
    pub open: bool,
    last_dismissed: Option<std::time::Instant>,
    fg_at_open: HWND,
    hover: Option<Act>,
    tracking: bool,
    hits: Vec<(D2D_RECT_F, Act)>,
    /// Render source, updated only on the UI thread from `pending`.
    notifs: Vec<Notif>,
    qs: Qs,
    /// How many backlog rows didn't fit (shown as a footer line).
    overflow: usize,
    pending: Arc<Mutex<Option<Snap>>>,
}

impl ActionCenter {
    pub fn new(dpi: f32) -> anyhow::Result<Self> {
        unsafe {
            let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            let class = w!("glide_shell_actioncenter");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(ac_wndproc),
                hInstance: hinstance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            RegisterClassW(&wc);
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
            let _ =
                DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
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
            let ui = w!("Segoe UI Variable");
            let nowrap = |f: IDWriteTextFormat| -> windows::core::Result<IDWriteTextFormat> {
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                Ok(f)
            };
            let fmt_head = nowrap(
                mk(ui, 15.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)
                    .or_else(|_| mk(w!("Segoe UI"), 15.0, DWRITE_FONT_WEIGHT_SEMI_BOLD))?,
            )?;
            let fmt_link = {
                let f = mk(ui, 12.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)
                    .or_else(|_| mk(w!("Segoe UI"), 12.0, DWRITE_FONT_WEIGHT_SEMI_BOLD))?;
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                f
            };
            let fmt_app = nowrap(
                mk(ui, 11.0, DWRITE_FONT_WEIGHT_NORMAL)
                    .or_else(|_| mk(w!("Segoe UI"), 11.0, DWRITE_FONT_WEIGHT_NORMAL))?,
            )?;
            let fmt_title = nowrap(
                mk(ui, 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)
                    .or_else(|_| mk(w!("Segoe UI"), 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD))?,
            )?;
            let fmt_body = nowrap(
                mk(ui, 12.0, DWRITE_FONT_WEIGHT_NORMAL)
                    .or_else(|_| mk(w!("Segoe UI"), 12.0, DWRITE_FONT_WEIGHT_NORMAL))?,
            )?;
            let fmt_tile = {
                let f = mk(ui, 11.5, DWRITE_FONT_WEIGHT_NORMAL)
                    .or_else(|_| mk(w!("Segoe UI"), 11.5, DWRITE_FONT_WEIGHT_NORMAL))?;
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
                f
            };
            let mkg = |size: f32| {
                mk(w!("Segoe Fluent Icons"), size, DWRITE_FONT_WEIGHT_NORMAL)
                    .or_else(|_| mk(w!("Segoe MDL2 Assets"), size, DWRITE_FONT_WEIGHT_NORMAL))
            };
            let fmt_glyph = {
                let f = mkg(18.0)?;
                f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                f
            };
            let fmt_glyph_big = {
                let f = mkg(28.0)?;
                f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                f
            };

            Ok(ActionCenter {
                hwnd,
                renderer,
                fmt_head,
                fmt_link,
                fmt_app,
                fmt_title,
                fmt_body,
                fmt_tile,
                fmt_glyph,
                fmt_glyph_big,
                scale: dpi / 96.0,
                w: 0.0,
                h: 0.0,
                anchor_right: 0,
                anchor_bottom: 0,
                open: false,
                last_dismissed: None,
                fg_at_open: HWND::default(),
                hover: None,
                tracking: false,
                hits: Vec::new(),
                notifs: Vec::new(),
                qs: Qs::default(),
                overflow: 0,
                pending: Arc::new(Mutex::new(None)),
            })
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn toggle(&mut self, bar_rect: RECT) {
        if self.open {
            self.hide();
        } else if !self.just_dismissed() {
            self.show(bar_rect);
        }
    }

    fn show(&mut self, bar_rect: RECT) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, self as *mut ActionCenter as isize);
        }
        crate::clickaway::set_ac_open(true);
        self.open = true;
        self.hover = None;
        self.anchor_right = bar_rect.right - (12.0 * self.scale) as i32;
        self.anchor_bottom = bar_rect.top - (10.0 * self.scale) as i32;
        unsafe {
            self.fg_at_open = GetForegroundWindow();
        }
        self.kick_refresh();
        self.relayout();
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
            SetTimer(Some(self.hwnd), TIMER_REFRESH, 1000, None);
        }
    }

    pub fn hide(&mut self) {
        if !self.open {
            return;
        }
        crate::clickaway::set_ac_open(false);
        self.open = false;
        self.hover = None;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_REFRESH);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    /// Hidden by click-away / WA_INACTIVE rather than a toggle: stamp the time
    /// so the same click's UP on the bell doesn't reopen it.
    pub fn dismiss(&mut self) {
        if self.open {
            self.last_dismissed = Some(std::time::Instant::now());
        }
        self.hide();
    }

    pub fn just_dismissed(&self) -> bool {
        self.last_dismissed
            .is_some_and(|t| t.elapsed().as_millis() < 400)
    }

    /// Drain a worker snapshot into the render fields (WM_AC_REFRESH).
    fn drain(&mut self) {
        let snap = self.pending.lock().ok().and_then(|mut g| g.take());
        if let Some(s) = snap {
            self.notifs = s.notifs;
            self.qs = s.qs;
            if self.open {
                self.relayout();
            }
        }
    }

    fn kick_refresh(&self) {
        let hwnd_raw = self.hwnd.0 as isize;
        let pending = self.pending.clone();
        std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let snap = Snap { notifs: read_notifs(), qs: read_qs() };
            if let Ok(mut g) = pending.lock() {
                *g = Some(snap);
            }
            unsafe {
                let _ =
                    PostMessageW(Some(HWND(hwnd_raw as *mut _)), WM_AC_REFRESH, WPARAM(0), LPARAM(0));
            }
        });
    }

    // ---- layout -----------------------------------------------------------

    fn grid_block_h(&self) -> f32 {
        14.0 + 2.0 * TILE_H + TILE_GAP
    }

    /// Height of one notification row plus its gap.
    fn row_pitch() -> f32 {
        ROW_H + ROW_GAP
    }

    fn relayout(&mut self) {
        let avail = self.anchor_bottom as f32 / self.scale - 8.0;
        let chrome = PAD + HEADER_H + self.grid_block_h() + PAD;
        let list_budget = (MAXH.min(avail) - chrome).max(Self::row_pitch());
        let max_rows = (list_budget / Self::row_pitch()).floor().max(1.0) as usize;

        let list_h = if self.notifs.is_empty() {
            64.0
        } else if self.notifs.len() <= max_rows {
            self.overflow = 0;
            self.notifs.len() as f32 * Self::row_pitch()
        } else {
            // Reserve the last visible slot for the "+N more" footer.
            self.overflow = self.notifs.len() - (max_rows - 1);
            (max_rows - 1) as f32 * Self::row_pitch() + 22.0
        };

        self.w = W;
        self.h = PAD + HEADER_H + list_h + self.grid_block_h() + PAD;
        let wd = (self.w * self.scale).round() as i32;
        let hd = (self.h * self.scale).round() as i32;
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                self.anchor_right - wd,
                self.anchor_bottom - hd,
                wd,
                hd,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        let _ = self.renderer.resize(wd as u32, hd as u32, self.scale * 96.0);
        self.paint();
    }

    // ---- interaction ------------------------------------------------------

    fn hit(&self, x: f32, y: f32) -> Option<Act> {
        self.hits
            .iter()
            .find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
            .map(|(_, a)| *a)
    }

    fn act(&mut self, a: Act) {
        match a {
            Act::ClearAll => {
                self.notifs.clear();
                self.overflow = 0;
                spawn_notif_cmd(NotifCmd::ClearAll);
                self.relayout();
            }
            Act::Dismiss(id) => {
                self.notifs.retain(|n| n.id != id);
                spawn_notif_cmd(NotifCmd::Remove(id));
                self.relayout();
            }
            Act::Tap(Tile::Wifi) => {
                let on = !self.qs.wifi;
                if let Some(wf) = crate::wifi::Wifi::open() {
                    wf.set_radio(on);
                }
                self.qs.wifi = on;
                self.paint();
                self.kick_refresh();
            }
            Act::Tap(Tile::Bluetooth) => {
                if self.qs.bluetooth.present() {
                    let on = !self.qs.bluetooth.is_on();
                    self.qs.bluetooth = if on { Tri::On } else { Tri::Off };
                    spawn_radio_cmd(RadioCmd::Bluetooth(on));
                    self.paint();
                }
            }
            Act::Tap(Tile::Airplane) => {
                if self.qs.airplane.present() {
                    let on = !self.qs.airplane.is_on();
                    self.qs.airplane = if on { Tri::On } else { Tri::Off };
                    spawn_radio_cmd(RadioCmd::Airplane(on));
                    self.paint();
                }
            }
            Act::Tap(Tile::Rotate) => {
                if self.qs.rotate_supported {
                    let on = !self.qs.rotate_on;
                    quicksettings::set_autorotate(on);
                    self.qs.rotate_on = on;
                    self.paint();
                }
            }
            Act::Tap(Tile::NightLight) => {
                open_uri("ms-settings:nightlight");
                self.hide();
            }
            Act::Tap(Tile::Settings) => {
                open_uri("ms-settings:");
                self.hide();
            }
        }
    }

    // ---- painting ---------------------------------------------------------

    fn paint(&mut self) {
        if !self.open {
            return;
        }
        self.hits.clear();
        unsafe {
            let r = &self.renderer;
            r.dc.BeginDraw();
            r.dc.Clear(Some(&theme::rgba(26, 27, 32, 0.92)));
            if let Ok(b) = r.brush(theme::with_alpha(theme::accent(), 0.25)) {
                r.dc.FillRectangle(&rect(0.0, 0.0, self.w, 1.0 / self.scale), &b);
            }
        }
        self.paint_notifs();
        self.paint_grid();
        unsafe {
            let _ = self.renderer.dc.EndDraw(None, None);
            let _ = self.renderer.present();
        }
    }

    fn paint_notifs(&mut self) {
        let grid_top = self.h - PAD - self.grid_block_h();
        // Header: "알림" + a right-aligned "모두 지우기" when there's anything.
        self.text("알림", &self.fmt_head.clone(), rect(PAD, PAD, W - PAD, PAD + HEADER_H), theme::TEXT);
        if !self.notifs.is_empty() {
            let lr = rect(W - PAD - 90.0, PAD, W - PAD, PAD + HEADER_H);
            let hot = self.hover == Some(Act::ClearAll);
            self.text(
                "모두 지우기",
                &self.fmt_link.clone(),
                lr,
                if hot { theme::TEXT } else { theme::TEXT_DIM },
            );
            self.hits.push((lr, Act::ClearAll));
        }

        let mut y = PAD + HEADER_H;
        if self.notifs.is_empty() {
            let er = rect(PAD, y, W - PAD, grid_top);
            self.glyph(0xEA8F, &self.fmt_glyph_big.clone(), rect(PAD, y + 4.0, W - PAD, y + 40.0), theme::TEXT_DIM);
            self.text("새 알림이 없습니다", &self.fmt_app.clone(), rect(PAD, y + 40.0, W - PAD, y + 60.0), theme::TEXT_DIM);
            let _ = er;
            return;
        }

        let shown = if self.overflow > 0 {
            self.notifs.len() - self.overflow
        } else {
            self.notifs.len()
        };
        let notifs = self.notifs.clone();
        for n in notifs.iter().take(shown) {
            let card = rect(PAD, y, W - PAD, y + ROW_H);
            let xhot = self.hover == Some(Act::Dismiss(n.id));
            self.fill_round(card, 6.0, theme::rgba(255, 255, 255, if xhot { 0.07 } else { 0.04 }));
            // App name, then title, then body — clipped to one line each.
            self.text(&n.app, &self.fmt_app.clone(), rect(PAD + 12.0, y + 6.0, W - PAD - 34.0, y + 22.0), theme::TEXT_DIM);
            self.text(&n.title, &self.fmt_title.clone(), rect(PAD + 12.0, y + 21.0, W - PAD - 34.0, y + 39.0), theme::TEXT);
            self.text(&n.body, &self.fmt_body.clone(), rect(PAD + 12.0, y + 38.0, W - PAD - 34.0, y + 54.0), theme::TEXT_DIM);
            // Dismiss X, top-right of the card.
            let xr = rect(W - PAD - 30.0, y + 4.0, W - PAD - 4.0, y + 30.0);
            if xhot {
                self.fill_round(xr, 4.0, theme::HOVER_FILL);
            }
            self.glyph(0xE711, &self.fmt_glyph.clone(), xr, if xhot { theme::TEXT } else { theme::TEXT_DIM });
            self.hits.push((xr, Act::Dismiss(n.id)));
            y += Self::row_pitch();
        }
        if self.overflow > 0 {
            self.text(
                &format!("이전 알림 {}개", self.overflow),
                &self.fmt_app.clone(),
                rect(PAD + 12.0, y, W - PAD, y + 20.0),
                theme::TEXT_DIM,
            );
        }
    }

    fn paint_grid(&mut self) {
        let top = self.h - PAD - 2.0 * TILE_H - TILE_GAP;
        // Hairline above the grid.
        self.fill_round(rect(PAD, top - 8.0, W - PAD, top - 7.0), 0.0, theme::with_alpha(theme::TEXT_DIM, 0.25));
        let tile_w = (W - 2.0 * PAD - 2.0 * TILE_GAP) / 3.0;
        for (i, &tile) in TILES.iter().enumerate() {
            let col = (i % 3) as f32;
            let row = (i / 3) as f32;
            let tx = PAD + col * (tile_w + TILE_GAP);
            let ty = top + row * (TILE_H + TILE_GAP);
            let r = rect(tx, ty, tx + tile_w, ty + TILE_H);
            let (glyph, label, on, enabled) = self.tile_state(tile);
            let hot = self.hover == Some(Act::Tap(tile));
            // Toggle-on tiles fill accent; launch/hover use a wash.
            let fill = if on {
                theme::with_alpha(theme::accent(), if hot { 0.42 } else { 0.34 })
            } else if hot {
                theme::HOVER_FILL
            } else {
                theme::rgba(255, 255, 255, 0.04)
            };
            self.fill_round(r, 7.0, fill);
            // The accent fill already carries the on-state; at 0.34 alpha the
            // normal foreground still reads against it, so only the disabled
            // case changes colour.
            let fg = if enabled { theme::TEXT } else { theme::with_alpha(theme::TEXT_DIM, 0.45) };
            self.glyph(glyph, &self.fmt_glyph.clone(), rect(tx, ty + 8.0, tx + tile_w, ty + 34.0), fg);
            self.text(label, &self.fmt_tile.clone(), rect(tx + 4.0, ty + 36.0, tx + tile_w - 4.0, ty + 54.0), fg);
            self.hits.push((r, Act::Tap(tile)));
        }
    }

    /// (glyph, label, is_on, is_enabled) for a tile from current state.
    fn tile_state(&self, tile: Tile) -> (u16, &'static str, bool, bool) {
        match tile {
            Tile::Wifi => (0xE701, "Wi-Fi", self.qs.wifi, true),
            Tile::Bluetooth => (
                0xE702,
                "Bluetooth",
                self.qs.bluetooth.is_on(),
                self.qs.bluetooth.present(),
            ),
            Tile::Airplane => (
                0xE709,
                "비행기 모드",
                self.qs.airplane.is_on(),
                self.qs.airplane.present(),
            ),
            Tile::Rotate => (
                0xE7AD,
                "자동 회전",
                self.qs.rotate_on,
                self.qs.rotate_supported,
            ),
            Tile::NightLight => (0xE708, "야간 모드", false, true),
            Tile::Settings => (0xE713, "모든 설정", false, true),
        }
    }

    // ---- draw helpers (mirror flyout.rs) ----------------------------------

    fn fill_round(&self, r: D2D_RECT_F, radius: f32, c: D2D1_COLOR_F) {
        unsafe {
            if let Ok(b) = self.renderer.brush(c) {
                if radius > 0.0 {
                    self.renderer.dc.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius },
                        &b,
                    );
                } else {
                    self.renderer.dc.FillRectangle(&r, &b);
                }
            }
        }
    }

    fn text(&self, s: &str, fmt: &IDWriteTextFormat, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        let t: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            if let Ok(b) = self.renderer.brush(c) {
                self.renderer.dc.DrawText(
                    &t,
                    fmt,
                    &r,
                    &b,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
    }

    fn glyph(&self, cp: u16, fmt: &IDWriteTextFormat, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        unsafe {
            if let Ok(b) = self.renderer.brush(c) {
                self.renderer.dc.DrawText(
                    &[cp],
                    fmt,
                    &r,
                    &b,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
    }
}

fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F { left, top, right, bottom }
}

fn open_uri(uri: &str) {
    let wide: Vec<u16> = uri.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), None, None, SW_SHOWNORMAL);
    }
}

// ---- worker bodies (MTA threads) ------------------------------------------

fn listener() -> Option<UserNotificationListener> {
    let l = UserNotificationListener::Current().ok()?;
    matches!(
        l.RequestAccessAsync().and_then(|op| op.join()),
        Ok(UserNotificationListenerAccessStatus::Allowed)
    )
    .then_some(l)
}

fn read_notifs() -> Vec<Notif> {
    let Some(l) = listener() else { return Vec::new() };
    let Ok(list) = l
        .GetNotificationsAsync(NotificationKinds::Toast)
        .and_then(|op| op.join())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for n in &list {
        let Ok(id) = n.Id() else { continue };
        let app = n
            .AppInfo()
            .ok()
            .and_then(|a| a.DisplayInfo().ok())
            .and_then(|d| d.DisplayName().ok())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "알림".into());
        let mut texts: Vec<String> = Vec::new();
        if let Ok(binding) = n
            .Notification()
            .and_then(|c| c.Visual())
            .and_then(|v| {
                KnownNotificationBindings::ToastGeneric().and_then(|b| v.GetBinding(&b))
            })
        {
            if let Ok(elements) = binding.GetTextElements() {
                for t in &elements {
                    if let Ok(s) = t.Text() {
                        let s = s.to_string();
                        if !s.is_empty() {
                            texts.push(s);
                        }
                    }
                }
            }
        }
        let title = texts.first().cloned().unwrap_or_default();
        let body = if texts.len() > 1 { texts[1..].join(" ") } else { String::new() };
        out.push(Notif { id, app, title, body });
    }
    // Newest first, like the stock action center.
    out.reverse();
    out
}

fn read_qs() -> Qs {
    let wifi = crate::wifi::Wifi::open().map(|w| w.radio_on()).unwrap_or(false);
    let snap = quicksettings::snapshot();
    Qs {
        wifi,
        bluetooth: snap.bluetooth,
        airplane: snap.airplane,
        rotate_supported: quicksettings::autorotate_supported(),
        rotate_on: quicksettings::autorotate_on(),
    }
}

enum NotifCmd {
    Remove(u32),
    ClearAll,
}

fn spawn_notif_cmd(cmd: NotifCmd) {
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let Some(l) = listener() else { return };
        match cmd {
            NotifCmd::Remove(id) => {
                let _ = l.RemoveNotification(id);
            }
            NotifCmd::ClearAll => {
                let _ = l.ClearNotifications();
            }
        }
    });
}

enum RadioCmd {
    Bluetooth(bool),
    Airplane(bool),
}

fn spawn_radio_cmd(cmd: RadioCmd) {
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        match cmd {
            RadioCmd::Bluetooth(on) => quicksettings::set_bluetooth(on),
            RadioCmd::Airplane(on) => quicksettings::set_airplane(on),
        }
    });
}

extern "system" fn ac_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ActionCenter;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let ac = &mut *ptr;
        let lx = |a: &ActionCenter| (lparam.0 & 0xFFFF) as i16 as f32 / a.scale;
        let ly = |a: &ActionCenter| ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 / a.scale;
        match msg {
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                ac.paint();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_ACTIVATE => {
                if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    ac.dismiss();
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let h = ac.hit(lx(ac), ly(ac));
                if h != ac.hover {
                    ac.hover = h;
                    ac.paint();
                }
                if !ac.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    if TrackMouseEvent(&mut tme).is_ok() {
                        ac.tracking = true;
                    }
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                ac.tracking = false;
                if ac.hover.take().is_some() {
                    ac.paint();
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                if let Some(a) = ac.hit(lx(ac), ly(ac)) {
                    ac.act(a);
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wparam.0 == VK_ESCAPE.0 as usize {
                    ac.hide();
                }
                LRESULT(0)
            }
            WM_AC_REFRESH => {
                ac.drain();
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == TIMER_REFRESH {
                    ac.kick_refresh();
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
