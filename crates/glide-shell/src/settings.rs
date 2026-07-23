//! Settings app window, styled after the Windows 11 Settings app: dark Mica
//! surface, left category nav with an accent indicator, right pane of rounded
//! cards with toggle switches. A real top-level window (shows up as a bar
//! button), hidden on close and revived by the bar menu.
//!
//! State flow: toggles mutate a local copy, save to settings.txt, and post
//! WM_SETTINGS_CHANGED to the bar, which reloads the file and applies.

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ELLIPSE, D2D1_ROUNDED_RECT,
};
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

const CATS: [(&str, u16); 7] = [
    ("개인화", 0xE771),
    ("작업 표시줄", 0xE7F4),
    ("시작 프로그램", 0xE7B5),
    ("Windows 조정", 0xE90F),
    ("단축키", 0xE765),
    ("고급 · 도구", 0xEC7A),
    ("시스템 정보", 0xE946),
];
const CAT_PERSONALIZE: usize = 0;
const CAT_TASKBAR: usize = 1;
const CAT_STARTUP: usize = 2;
const CAT_TWEAKS: usize = 3;
const CAT_SHORTCUTS: usize = 4;
const CAT_ADVANCED: usize = 5;
const CAT_ABOUT: usize = 6;

/// (label, subtitle, deep-link URI). The stock panels glide doesn't own yet —
/// surfaced here so the settings app is one door to the whole system, and the
/// fragmented Control Panel / ms-settings maze collapses into this list. Each
/// opens via ShellExecute; ms-settings: URIs and control.exe applets both work.
/// The classic Control Panel applets + power tools — the *utilitarian* Windows
/// surface, routed through control.exe/.cpl/.msc, never ms-settings (that page
/// is the ad billboard we're replacing, not linking back into). This is the
/// fragmented Control Panel, reachable, ad-free.
const LINKS: [(&str, &str, &str); 11] = [
    ("네트워크 연결", "어댑터 · IP · 프록시 (ncpa.cpl)", "ncpa.cpl"),
    ("소리 장치", "재생 · 녹음 · 기본 장치 (mmsys.cpl)", "mmsys.cpl"),
    ("전원 옵션", "전원 계획 · 절전 동작 (powercfg.cpl)", "powercfg.cpl"),
    ("프로그램 제거", "설치된 앱 제거 (appwiz.cpl)", "appwiz.cpl"),
    ("날짜 · 시간", "시간대 · 인터넷 동기화 (timedate.cpl)", "timedate.cpl"),
    ("제어판 (클래식 홈)", "모든 클래식 항목", "control.exe"),
    ("장치 관리자", "하드웨어 · 드라이버", "devmgmt.msc"),
    ("서비스", "백그라운드 서비스 시작 · 중지", "services.msc"),
    ("디스크 관리", "파티션 · 볼륨 · 드라이브 문자", "diskmgmt.msc"),
    ("레지스트리 편집기", "고급 — regedit로 직접 편집", "regedit.exe"),
    ("glide 설정 폴더", "%APPDATA%\\glide-shell — 설정 파일 직접", "%APPDATA%\\glide-shell"),
];

/// A Windows-side toggle (registry-backed), distinct from glide's own config.
#[derive(Clone, Copy, PartialEq)]
enum WinTgl {
    Dark,
    Transparency,
    TitleAccent,
    GlideAutostart,
}

#[derive(Clone, Copy, PartialEq)]
enum Act {
    Nav(usize),
    Toggle(usize),
    Win(WinTgl),
    Startup(usize),
    Tweak(usize),
    Link(usize),
    Accent(u8),
    Density(u8),
}

enum RowKind {
    /// Index into glide's own toggle table below.
    Toggle(usize),
    /// A Windows setting glide reads/writes directly.
    Win(WinTgl),
    /// Index into the cached startup-item list.
    Startup(usize),
    /// Index into winsettings::TWEAKS — a registry-backed toggle.
    Tweak(usize),
    /// Index into LINKS — a deep-link to a stock panel; opens on click.
    Link(usize),
    /// The glide accent swatch strip; rendered as color circles, not a switch.
    Accent,
    /// The bar-density segmented control (compact/normal/large).
    Density,
    Info,
}

struct Row {
    title: String,
    sub: String,
    kind: RowKind,
}

/// Toggle id → (category, title, sub). Ids are stable; `flip` maps them onto
/// config fields.
const TOGGLES: [(&str, &str); 8] = [
    ("창 버튼 라벨", "끄면 Win10 기본처럼 아이콘만 표시"),
    ("시계 초 표시", "시계를 초까지 표시"),
    ("바탕화면 보기 버튼", "바 오른쪽 끝 슬리버 — 클릭 = 전체 최소화/복원"),
    ("보조 모니터 작업 표시줄", "모니터마다 아이콘 전용 바 + 시계"),
    ("Win 키로 시작 메뉴 열기", "끄면 Win 키가 glint 검색을 엽니다 (Win+S는 항상 검색)"),
    ("24시간제 시계", "끄면 오전/오후 12시간제"),
    ("시계에 날짜 표시", "끄면 시간만 표시 (날짜 줄 숨김)"),
    ("알림 표시", "앱 알림을 glide 토스트로 — 끄면 조용히"),
];

pub struct SettingsApp {
    hwnd: HWND,
    renderer: Renderer,
    fmt_app: IDWriteTextFormat,
    fmt_cat: IDWriteTextFormat,
    fmt_row: IDWriteTextFormat,
    fmt_sub: IDWriteTextFormat,
    fmt_seg: IDWriteTextFormat,
    fmt_g16: IDWriteTextFormat,
    scale: f32,
    w: f32,
    h: f32,
    cat: usize,
    hover: Option<Act>,
    tracking: bool,
    hits: Vec<(D2D_RECT_F, Act)>,
    cfg: crate::config::Settings,
    startup: Vec<crate::winsettings::StartupItem>,
    scroll: f32,
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
            // Centred label for the density segmented control.
            let fmt_seg = mkv(12.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            fmt_seg.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            fmt_seg.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_seg.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
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
                fmt_seg,
                fmt_g16,
                scale,
                w: (cw as f32) / scale,
                h: (ch as f32) / scale,
                cat: 0,
                hover: None,
                tracking: false,
                hits: Vec::new(),
                cfg: crate::config::load(),
                startup: Vec::new(),
                scroll: 0.0,
                bar: 0,
            })
        }
    }

    pub fn open(&mut self, bar: HWND) {
        self.bar = bar.0 as isize;
        self.cfg = crate::config::load();
        self.startup = crate::winsettings::list_startup();
        self.scroll = 0.0;
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
        let win = |w: WinTgl, title: &str, sub: &str| Row {
            title: title.to_string(),
            sub: sub.to_string(),
            kind: RowKind::Win(w),
        };
        let info = |title: &str, sub: String| Row { title: title.to_string(), sub, kind: RowKind::Info };
        let accent = || Row {
            title: "glide 강조색".to_string(),
            sub: "바 · 스위치 · 강조 전반에 쓰이는 색 — 즉시 적용".to_string(),
            kind: RowKind::Accent,
        };
        match self.cat {
            CAT_PERSONALIZE => vec![
                accent(),
                win(WinTgl::Dark, "어두운 모드", "앱·시스템을 어두운 테마로 — Windows 설정과 동기화"),
                win(WinTgl::Transparency, "투명 효과", "창·패널 배경의 아크릴/미카 투명"),
                win(WinTgl::TitleAccent, "제목 표시줄 강조색", "제목 표시줄과 창 테두리에 강조색 적용"),
            ],
            CAT_TASKBAR => vec![
                Row {
                    title: "바 밀도".to_string(),
                    sub: "표시줄 높이 — 재시작 후 적용".to_string(),
                    kind: RowKind::Density,
                },
                row(0),
                row(1),
                row(5),
                row(6),
                row(2),
                row(3),
                row(7),
            ],
            CAT_STARTUP => {
                let mut v = vec![win(
                    WinTgl::GlideAutostart,
                    "로그온 시 glide 자동 시작",
                    "Windows 시작과 함께 glide 바 실행 (HKCU\\Run — explorer와 병행)",
                )];
                if self.startup.is_empty() {
                    v.push(info(
                        "그 밖의 시작 프로그램 없음",
                        "로그온 시 자동 실행되는 다른 앱이 없습니다".to_string(),
                    ));
                } else {
                    v.extend(self.startup.iter().enumerate().map(|(i, s)| Row {
                        title: s.name.clone(),
                        sub: trunc(&s.detail, 78),
                        kind: RowKind::Startup(i),
                    }));
                }
                v
            }
            CAT_TWEAKS => crate::winsettings::TWEAKS
                .iter()
                .enumerate()
                .map(|(i, t)| Row {
                    title: t.title.to_string(),
                    sub: t.sub.to_string(),
                    kind: RowKind::Tweak(i),
                })
                .collect(),
            CAT_ADVANCED => LINKS
                .iter()
                .enumerate()
                .map(|(i, (title, sub, _))| Row {
                    title: title.to_string(),
                    sub: sub.to_string(),
                    kind: RowKind::Link(i),
                })
                .collect(),
            CAT_SHORTCUTS => vec![
                row(4),
                info("Win+S", "glint 검색".to_string()),
                info("Ctrl+Alt+Shift+E", "레스큐 — explorer 즉시 복귀".to_string()),
                info("Ctrl+Alt+Shift+R", "레스큐 — 셸 등록 해제 (다음 로그온부터 stock)".to_string()),
            ],
            CAT_ABOUT => {
                let mut v: Vec<Row> = crate::winsettings::system_info()
                    .into_iter()
                    .map(|(k, val)| info(&k, val))
                    .collect();
                v.push(info("glide-shell", format!("버전 {}", env!("CARGO_PKG_VERSION"))));
                v.push(info(
                    "로그온 셸",
                    crate::safety::query_shell().unwrap_or_else(|| "stock explorer (Shell= 없음)".to_string()),
                ));
                v.push(info("크래시 로그", "%APPDATA%\\glide-shell\\crash.log".to_string()));
                v
            }
            _ => Vec::new(),
        }
    }

    fn win_value(&self, w: WinTgl) -> bool {
        match w {
            WinTgl::Dark => crate::winsettings::dark_mode(),
            WinTgl::Transparency => crate::winsettings::transparency(),
            WinTgl::TitleAccent => crate::winsettings::title_accent(),
            WinTgl::GlideAutostart => crate::winsettings::glide_autostart(),
        }
    }

    fn win_flip(&mut self, w: WinTgl) {
        let now = self.win_value(w);
        match w {
            WinTgl::Dark => crate::winsettings::set_dark_mode(!now),
            WinTgl::Transparency => crate::winsettings::set_transparency(!now),
            WinTgl::TitleAccent => crate::winsettings::set_title_accent(!now),
            WinTgl::GlideAutostart => crate::winsettings::set_glide_autostart(!now),
        }
        self.paint();
    }

    fn startup_flip(&mut self, i: usize) {
        if let Some(item) = self.startup.get(i) {
            crate::winsettings::set_startup_enabled(item, !item.enabled);
        }
        // Re-read so the switch reflects the truth (writes can be no-ops).
        self.startup = crate::winsettings::list_startup();
        self.paint();
    }

    fn tweak_value(&self, i: usize) -> bool {
        crate::winsettings::TWEAKS
            .get(i)
            .map(crate::winsettings::tweak_enabled)
            .unwrap_or(false)
    }

    fn tweak_flip(&mut self, i: usize) {
        if let Some(t) = crate::winsettings::TWEAKS.get(i) {
            crate::winsettings::set_tweak(t, !crate::winsettings::tweak_enabled(t));
        }
        self.paint();
    }

    fn toggle_value(&self, t: usize) -> bool {
        match t {
            0 => self.cfg.labels,
            1 => self.cfg.clock_seconds,
            2 => self.cfg.desk_sliver,
            3 => self.cfg.secondary_bars,
            4 => self.cfg.winkey_start,
            5 => self.cfg.clock_24h,
            6 => self.cfg.clock_date,
            _ => self.cfg.toasts_enabled,
        }
    }

    fn flip(&mut self, t: usize) {
        match t {
            0 => self.cfg.labels = !self.cfg.labels,
            1 => self.cfg.clock_seconds = !self.cfg.clock_seconds,
            2 => self.cfg.desk_sliver = !self.cfg.desk_sliver,
            3 => self.cfg.secondary_bars = !self.cfg.secondary_bars,
            4 => self.cfg.winkey_start = !self.cfg.winkey_start,
            5 => self.cfg.clock_24h = !self.cfg.clock_24h,
            6 => self.cfg.clock_date = !self.cfg.clock_date,
            _ => self.cfg.toasts_enabled = !self.cfg.toasts_enabled,
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

    fn accent_pick(&mut self, idx: u8) {
        if self.cfg.accent == idx {
            return;
        }
        self.cfg.accent = idx;
        // Apply to this window immediately, persist, and nudge the bar so its
        // accent changes without a relaunch.
        crate::theme::set_accent(idx);
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

    fn density_pick(&mut self, d: u8) {
        if self.cfg.bar_density == d {
            return;
        }
        self.cfg.bar_density = d;
        crate::config::save(&self.cfg);
        // Density bakes into the appbar strut at launch, so no live nudge to the
        // bar — the picker just reflects the new choice until the next start.
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
        self.fill_round(pill, 11.0, if on { theme::accent() } else { theme::rgba(255, 255, 255, 0.14) });
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

    /// A colour circle for the accent picker; `sel` draws a white ring.
    fn swatch(&self, cx: f32, cy: f32, c: D2D1_COLOR_F, sel: bool) {
        unsafe {
            if let Ok(b) = self.renderer.brush(c) {
                self.renderer.dc.FillEllipse(
                    &D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: 10.0, radiusY: 10.0 },
                    &b,
                );
            }
            if sel {
                if let Ok(b) = self.renderer.brush(theme::rgba(255, 255, 255, 0.95)) {
                    self.renderer.dc.DrawEllipse(
                        &D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: 13.0, radiusY: 13.0 },
                        &b,
                        2.0,
                        None,
                    );
                }
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
                    theme::accent(),
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

        // Content scrolls under the fixed title; clip so scrolled rows never
        // paint over the title or the nav pane.
        let content_top = 72.0;
        let clip = rect(cx0 - 4.0, content_top, self.w, self.h);
        unsafe {
            r.dc.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED);
        }
        let rows = self.rows();
        for (i, row) in rows.iter().enumerate() {
            let y = 80.0 - self.scroll + i as f32 * (CARD_H + CARD_GAP);
            if y + CARD_H < content_top || y > self.h {
                continue; // offscreen — skip paint and hit-test
            }
            let card = rect(cx0, y, cx1, y + CARD_H);
            // act = clickable target; sw = switch state (toggle rows); link =
            // draw a chevron and open on click instead of a switch.
            let (act, sw, link, accent, density): (Option<Act>, Option<bool>, bool, bool, Option<u8>) =
                match row.kind {
                    RowKind::Toggle(t) => (Some(Act::Toggle(t)), Some(self.toggle_value(t)), false, false, None),
                    RowKind::Win(w) => (Some(Act::Win(w)), Some(self.win_value(w)), false, false, None),
                    RowKind::Startup(s) => (
                        Some(Act::Startup(s)),
                        Some(self.startup.get(s).map(|x| x.enabled).unwrap_or(false)),
                        false,
                        false,
                        None,
                    ),
                    RowKind::Tweak(t) => (Some(Act::Tweak(t)), Some(self.tweak_value(t)), false, false, None),
                    RowKind::Link(l) => (Some(Act::Link(l)), None, true, false, None),
                    RowKind::Accent => (None, None, false, true, None),
                    RowKind::Density => (None, None, false, false, Some(self.cfg.bar_density)),
                    RowKind::Info => (None, None, false, false, None),
                };
            let hot = act.map(|a| self.hover == Some(a)).unwrap_or(false);
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
            if let Some(on) = sw {
                self.switch(cx1 - 18.0, y + CARD_H / 2.0, on);
            }
            if link {
                // ChevronRight — the Win11 "opens elsewhere" affordance.
                self.glyph(0xE76C, rect(cx1 - 44.0, y, cx1 - 12.0, y + CARD_H), theme::TEXT_DIM);
            }
            if accent {
                // Swatch strip, right-aligned in the card; each circle is its
                // own hit target.
                let cy = y + CARD_H / 2.0;
                let n = theme::ACCENT_PRESETS.len();
                let gap = 30.0;
                for (k, (_, r8, g8, b8)) in theme::ACCENT_PRESETS.iter().enumerate() {
                    let scx = cx1 - 26.0 - ((n - 1 - k) as f32) * gap;
                    self.swatch(scx, cy, theme::rgba(*r8, *g8, *b8, 1.0), self.cfg.accent as usize == k);
                    self.hits.push((
                        rect(scx - 14.0, y, scx + 14.0, y + CARD_H),
                        Act::Accent(k as u8),
                    ));
                }
            }
            if let Some(sel) = density {
                // Three-segment pill, right-aligned; the chosen segment carries
                // the accent, and each is its own hit target.
                let labels = ["컴팩트", "보통", "크게"];
                let seg_w = 58.0;
                let seg_h = 30.0;
                let gap = 4.0;
                let cy = y + CARD_H / 2.0;
                let total = seg_w * labels.len() as f32 + gap * (labels.len() as f32 - 1.0);
                let x0 = cx1 - 14.0 - total;
                for (k, lbl) in labels.iter().enumerate() {
                    let sx = x0 + k as f32 * (seg_w + gap);
                    let seg = rect(sx, cy - seg_h / 2.0, sx + seg_w, cy + seg_h / 2.0);
                    let on = sel == k as u8;
                    self.fill_round(seg, 6.0, if on { theme::accent() } else { theme::rgba(255, 255, 255, 0.08) });
                    self.text(
                        lbl,
                        &self.fmt_seg.clone(),
                        seg,
                        if on { theme::rgba(23, 24, 28, 1.0) } else { theme::TEXT_DIM },
                    );
                    self.hits.push((seg, Act::Density(k as u8)));
                }
            }
            if let Some(a) = act {
                self.hits.push((card, a));
            }
        }
        unsafe {
            r.dc.PopAxisAlignedClip();
            let _ = r.dc.EndDraw(None, None);
            let _ = r.present();
        }
    }
}

fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F { left, top, right, bottom }
}

/// Clip a subtitle to `max` chars (counting by char, not byte, so multibyte
/// paths never split mid-codepoint) with an ellipsis.
fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
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
                            app.scroll = 0.0;
                            // Startup can change out from under us (installers,
                            // Task Manager); re-read on entry so it's current.
                            if i == CAT_STARTUP {
                                app.startup = crate::winsettings::list_startup();
                            }
                            app.hover = None;
                            app.paint();
                        }
                    }
                    Some(Act::Toggle(t)) => app.flip(t),
                    Some(Act::Win(w)) => app.win_flip(w),
                    Some(Act::Startup(s)) => app.startup_flip(s),
                    Some(Act::Tweak(t)) => app.tweak_flip(t),
                    Some(Act::Accent(i)) => app.accent_pick(i),
                    Some(Act::Density(d)) => app.density_pick(d),
                    Some(Act::Link(l)) => {
                        if let Some(e) = LINKS.get(l) {
                            crate::winsettings::launch(e.2);
                        }
                    }
                    None => {}
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32 / 120.0 * 52.0;
                let n = app.rows().len();
                let content_h = n as f32 * (CARD_H + CARD_GAP);
                let vis = (app.h - 92.0).max(0.0);
                let max = (content_h - vis).max(0.0);
                let ns = (app.scroll - delta).clamp(0.0, max);
                if ns != app.scroll {
                    app.scroll = ns;
                    app.hover = None;
                    app.paint();
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
