//! Settings app window, styled after the Windows 11 Settings app: dark Mica
//! surface, left category nav with an accent indicator, right pane of rounded
//! cards with toggle switches. A real top-level window (shows up as a bar
//! button), hidden on close and revived by the bar menu.
//!
//! State flow: toggles mutate a local copy, save to settings.txt, and post
//! WM_SETTINGS_CHANGED to the bar, which reloads the file and applies.

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ELLIPSE, D2D1_ROUNDED_RECT};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_WORD_WRAPPING_NO_WRAP,
    IDWriteTextFormat,
};
use windows::Win32::Graphics::Dwm::{
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;
use windows_numerics::Vector2;

use crate::render::Renderer;
use crate::theme;

const WM_MOUSELEAVE: u32 = 0x02A3;
/// Bar reloads settings.txt when it receives this.
pub const WM_SETTINGS_CHANGED: u32 = WM_APP + 14;

const NAV_W: f32 = 250.0;
const NAV_ITEM_H: f32 = 40.0;
const CARD_H: f32 = 68.0;
const CARD_GAP: f32 = 4.0;

const CATS: [(&str, u16); 3] = [
    ("작업 표시줄", 0xE7F4),
    ("단축키", 0xE765),
    ("정보", 0xE946),
];

#[derive(Clone, Copy, PartialEq)]
enum Act {
    Nav(usize),
    Toggle(usize),
}

enum RowKind {
    /// Index into the toggle table below.
    Toggle(usize),
    Info,
}

struct Row {
    title: String,
    sub: String,
    kind: RowKind,
}

/// Toggle id → (category, title, sub). Ids are stable; `flip` maps them onto
/// config fields.
const TOGGLES: [(&str, &str); 5] = [
    ("창 버튼 라벨", "끄면 Win10 기본처럼 아이콘만 표시"),
    ("시계 초 표시", "시계를 HH:MM:SS로"),
    ("바탕화면 보기 버튼", "바 오른쪽 끝 슬리버 — 클릭 = 전체 최소화/복원"),
    ("보조 모니터 작업 표시줄", "모니터마다 아이콘 전용 바 + 시계"),
    ("Win 키로 시작 메뉴 열기", "끄면 Win 키가 glint 검색을 엽니다 (Win+S는 항상 검색)"),
];

pub struct SettingsApp {
    hwnd: HWND,
    renderer: Renderer,
    fmt_app: IDWriteTextFormat,
    fmt_cat: IDWriteTextFormat,
    fmt_row: IDWriteTextFormat,
    fmt_sub: IDWriteTextFormat,
    fmt_g16: IDWriteTextFormat,
    scale: f32,
    w: f32,
    h: f32,
    cat: usize,
    hover: Option<Act>,
    tracking: bool,
    hits: Vec<(D2D_RECT_F, Act)>,
    cfg: crate::config::Settings,
    bar: isize,
}

impl SettingsApp {
    pub fn new(dpi: f32) -> anyhow::Result<Self> {
        unsafe {
            let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            let class = w!("glide_shell_settings");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(settings_wndproc),
                hInstance: hinstance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let scale = dpi / 96.0;
            let (w, h) = ((900.0 * scale) as i32, (640.0 * scale) as i32);
            let hwnd = CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP,
                class,
                w!("설정"),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                w,
                h,
                None,
                None,
                Some(hinstance.into()),
                None,
            )?;
            let dark: i32 = 1;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
            let backdrop: i32 = 2; // DWMSBT_MAINWINDOW — Mica, like the Settings app
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &backdrop as *const _ as _, 4);

            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let (cw, ch) = ((rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32);
            let renderer = Renderer::new(hwnd, cw, ch, dpi)?;

            let mk = |family, size, weight: DWRITE_FONT_WEIGHT| {
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
            let mkv = |size, weight| mk(family, size, weight).or_else(|_| mk(w!("Segoe UI"), size, weight));
            let fmt_app = mkv(14.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let fmt_cat = mkv(24.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let fmt_row = mkv(13.5, DWRITE_FONT_WEIGHT_NORMAL)?;
            let fmt_sub = mkv(11.5, DWRITE_FONT_WEIGHT_NORMAL)?;
            for f in [&fmt_app, &fmt_cat, &fmt_row, &fmt_sub] {
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            }
            let fmt_g16 = mk(w!("Segoe Fluent Icons"), 16.0, DWRITE_FONT_WEIGHT_NORMAL)
                .or_else(|_| mk(w!("Segoe MDL2 Assets"), 16.0, DWRITE_FONT_WEIGHT_NORMAL))?;
            fmt_g16.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_g16.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            Ok(SettingsApp {
                hwnd,
                renderer,
                fmt_app,
                fmt_cat,
                fmt_row,
                fmt_sub,
                fmt_g16,
                scale,
                w: (cw as f32) / scale,
                h: (ch as f32) / scale,
                cat: 0,
                hover: None,
                tracking: false,
                hits: Vec::new(),
                cfg: crate::config::load(),
                bar: 0,
            })
        }
    }

    pub fn open(&mut self, bar: HWND) {
        self.bar = bar.0 as isize;
        self.cfg = crate::config::load();
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, self as *mut SettingsApp as isize);
            if !IsWindowVisible(self.hwnd).as_bool() {
                // Center on the primary work area on each fresh open.
                let mut wa = RECT::default();
                let _ = SystemParametersInfoW(
                    SPI_GETWORKAREA,
                    0,
                    Some(&mut wa as *mut _ as _),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                );
                let (w, h) = ((900.0 * self.scale) as i32, (640.0 * self.scale) as i32);
                let x = wa.left + ((wa.right - wa.left) - w) / 2;
                let y = wa.top + ((wa.bottom - wa.top) - h) / 2;
                let _ = SetWindowPos(self.hwnd, None, x, y.max(wa.top), w, h, SWP_NOZORDER);
            }
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.paint();
    }

    fn hide(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.hover = None;
    }

    fn resized(&mut self) {
        unsafe {
            let dpi = GetDpiForWindow(self.hwnd) as f32;
            self.scale = dpi / 96.0;
            let mut rc = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rc);
            let (cw, ch) = ((rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32);
            let _ = self.renderer.resize(cw, ch, dpi);
            self.w = cw as f32 / self.scale;
            self.h = ch as f32 / self.scale;
        }
        self.paint();
    }

    fn rows(&self) -> Vec<Row> {
        let row = |t: usize| Row {
            title: TOGGLES[t].0.to_string(),
            sub: TOGGLES[t].1.to_string(),
            kind: RowKind::Toggle(t),
        };
        let info = |title: &str, sub: String| Row { title: title.to_string(), sub, kind: RowKind::Info };
        match self.cat {
            0 => vec![row(0), row(1), row(2), row(3)],
            1 => vec![
                row(4),
                info("Win+S", "glint 검색".to_string()),
                info("Ctrl+Alt+Shift+E", "레스큐 — explorer 즉시 복귀".to_string()),
                info("Ctrl+Alt+Shift+R", "레스큐 — 셸 등록 해제 (다음 로그온부터 stock)".to_string()),
            ],
            _ => vec![
                info("버전", format!("glide-shell {}", env!("CARGO_PKG_VERSION"))),
                info(
                    "로그온 셸",
                    crate::safety::query_shell().unwrap_or_else(|| "stock explorer (Shell= 없음)".to_string()),
                ),
                info("셸 등록/해제", "glide-shell --register / --unregister (CLI, YES 확인)".to_string()),
                info("크래시 로그", "%APPDATA%\\glide-shell\\crash.log".to_string()),
            ],
        }
    }

    fn toggle_value(&self, t: usize) -> bool {
        match t {
            0 => self.cfg.labels,
            1 => self.cfg.clock_seconds,
            2 => self.cfg.desk_sliver,
            3 => self.cfg.secondary_bars,
            _ => self.cfg.winkey_start,
        }
    }

    fn flip(&mut self, t: usize) {
        match t {
            0 => self.cfg.labels = !self.cfg.labels,
            1 => self.cfg.clock_seconds = !self.cfg.clock_seconds,
            2 => self.cfg.desk_sliver = !self.cfg.desk_sliver,
            3 => self.cfg.secondary_bars = !self.cfg.secondary_bars,
            _ => self.cfg.winkey_start = !self.cfg.winkey_start,
        }
        crate::config::save(&self.cfg);
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.bar as *mut _)),
                WM_SETTINGS_CHANGED,
                WPARAM(0),
                LPARAM(0),
            );
        }
        self.paint();
    }

    fn hit(&self, x: f32, y: f32) -> Option<Act> {
        self.hits
            .iter()
            .find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
            .map(|(_, a)| *a)
    }

    // ---- painting ----------------------------------------------------------

    fn fill_round(&self, r: D2D_RECT_F, radius: f32, c: D2D1_COLOR_F) {
        unsafe {
            if let Ok(b) = self.renderer.brush(c) {
                self.renderer
                    .dc
                    .FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius }, &b);
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

    fn glyph(&self, cp: u16, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        unsafe {
            if let Ok(b) = self.renderer.brush(c) {
                self.renderer.dc.DrawText(
                    &[cp],
                    &self.fmt_g16,
                    &r,
                    &b,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
    }

    /// Win11 switch: 44×22 pill, accent when on, knob slides.
    fn switch(&self, right: f32, cy: f32, on: bool) {
        let pill = D2D_RECT_F { left: right - 44.0, top: cy - 11.0, right, bottom: cy + 11.0 };
        self.fill_round(pill, 11.0, if on { theme::ACCENT } else { theme::rgba(255, 255, 255, 0.14) });
        unsafe {
            let c = if on { theme::rgba(23, 24, 28, 1.0) } else { theme::rgba(200, 203, 210, 1.0) };
            if let Ok(b) = self.renderer.brush(c) {
                self.renderer.dc.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: Vector2 { X: if on { right - 11.0 } else { right - 33.0 }, Y: cy },
                        radiusX: 7.0,
                        radiusY: 7.0,
                    },
                    &b,
                );
            }
        }
    }

    fn paint(&mut self) {
        self.hits.clear();
        let r = &self.renderer;
        unsafe {
            r.dc.BeginDraw();
            // Low alpha: Mica does the wallpaper tinting, we just darken.
            r.dc.Clear(Some(&theme::rgba(24, 25, 30, 0.72)));
        }

        // -- left nav --
        self.text(
            "glide-shell 설정",
            &self.fmt_app.clone(),
            rect(20.0, 18.0, NAV_W - 12.0, 46.0),
            theme::TEXT,
        );
        for (i, (name, glyph)) in CATS.iter().enumerate() {
            let y = 64.0 + i as f32 * (NAV_ITEM_H + 2.0);
            let item = rect(8.0, y, NAV_W - 8.0, y + NAV_ITEM_H);
            if self.cat == i {
                self.fill_round(item, 6.0, theme::rgba(255, 255, 255, 0.08));
                // Accent indicator, the Win11 nav gesture.
                self.fill_round(
                    rect(8.0, y + 12.0, 11.0, y + NAV_ITEM_H - 12.0),
                    1.5,
                    theme::ACCENT,
                );
            } else if self.hover == Some(Act::Nav(i)) {
                self.fill_round(item, 6.0, theme::HOVER_FILL);
            }
            self.glyph(*glyph, rect(16.0, y, 48.0, y + NAV_ITEM_H), theme::TEXT);
            self.text(name, &self.fmt_row.clone(), rect(52.0, y, NAV_W - 12.0, y + NAV_ITEM_H), theme::TEXT);
            self.hits.push((item, Act::Nav(i)));
        }

        // -- content pane --
        let cx0 = NAV_W + 28.0;
        let cx1 = (self.w - 32.0).max(cx0 + 120.0);
        self.text(CATS[self.cat].0, &self.fmt_cat.clone(), rect(cx0, 18.0, cx1, 62.0), theme::TEXT);

        let rows = self.rows();
        for (i, row) in rows.iter().enumerate() {
            let y = 80.0 + i as f32 * (CARD_H + CARD_GAP);
            let card = rect(cx0, y, cx1, y + CARD_H);
            let toggle = matches!(row.kind, RowKind::Toggle(_));
            let hot = toggle && self.hover == Some(Act::Toggle(match row.kind {
                RowKind::Toggle(t) => t,
                RowKind::Info => 0,
            }));
            self.fill_round(card, 6.0, theme::rgba(255, 255, 255, if hot { 0.075 } else { 0.045 }));
            self.text(
                &row.title,
                &self.fmt_row.clone(),
                rect(cx0 + 18.0, y + 10.0, cx1 - 80.0, y + 38.0),
                theme::TEXT,
            );
            self.text(
                &row.sub,
                &self.fmt_sub.clone(),
                rect(cx0 + 18.0, y + 36.0, cx1 - 80.0, y + 58.0),
                theme::TEXT_DIM,
            );
            if let RowKind::Toggle(t) = row.kind {
                self.switch(cx1 - 18.0, y + CARD_H / 2.0, self.toggle_value(t));
                self.hits.push((card, Act::Toggle(t)));
            }
        }

        unsafe {
            let _ = r.dc.EndDraw(None, None);
            let _ = r.present();
        }
    }
}

fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F { left, top, right, bottom }
}

unsafe extern "system" fn settings_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let app = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsApp;
        if app.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let app = &mut *app;
        match msg {
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                app.paint();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SIZE => {
                if wparam.0 as u32 != SIZE_MINIMIZED {
                    app.resized();
                }
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let sug = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    sug.left,
                    sug.top,
                    sug.right - sug.left,
                    sug.bottom - sug.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                app.resized();
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
                mmi.ptMinTrackSize.x = (560.0 * app.scale) as i32;
                mmi.ptMinTrackSize.y = (420.0 * app.scale) as i32;
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32 / app.scale;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 / app.scale;
                let h = app.hit(x, y);
                if h != app.hover {
                    app.hover = h;
                    app.paint();
                }
                if !app.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    if TrackMouseEvent(&mut tme).is_ok() {
                        app.tracking = true;
                    }
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                app.tracking = false;
                if app.hover.take().is_some() {
                    app.paint();
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32 / app.scale;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 / app.scale;
                match app.hit(x, y) {
                    Some(Act::Nav(i)) => {
                        if app.cat != i {
                            app.cat = i;
                            app.paint();
                        }
                    }
                    Some(Act::Toggle(t)) => app.flip(t),
                    None => {}
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wparam.0 == VK_ESCAPE.0 as usize {
                    app.hide();
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                app.hide();
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
