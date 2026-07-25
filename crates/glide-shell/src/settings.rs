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
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ELLIPSE,
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
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;
use windows_numerics::Vector2;

use crate::quicksettings::{self, RadioSnapshot, Tri};
use crate::render::{Renderer, fill_round, rect};
use crate::theme;
use crate::wifi::Wifi;

const WM_MOUSELEAVE: u32 = 0x02A3;
/// Bar reloads settings.txt when it receives this.
pub const WM_SETTINGS_CHANGED: u32 = WM_APP + 14;

const CARD_H: f32 = 68.0;
const CARD_GAP: f32 = 4.0;
/// Samsung two-pane master list: pane width, an icon row's height, and the gap
/// a group separator occupies.
const NAV_W2: f32 = 340.0;
const NAV_ROW_H: f32 = 50.0;
const NAV_SEP: f32 = 15.0;

const CATS: [(&str, u16); 12] = [
    ("개인화", 0xE771),
    ("작업 표시줄", 0xE7F4),
    ("시작 프로그램", 0xE7B5),
    ("Windows 조정", 0xE90F),
    ("단축키", 0xE765),
    ("고급 · 도구", 0xEC7A),
    ("시스템 정보", 0xE946),
    ("소리", 0xE767),
    ("전원 · 배터리", 0xE83E),
    ("네트워크", 0xE839),
    ("앱 · 프로그램", 0xE71D),
    ("날짜 · 시간", 0xE917),
];
const CAT_PERSONALIZE: usize = 0;
const CAT_TASKBAR: usize = 1;
const CAT_STARTUP: usize = 2;
const CAT_TWEAKS: usize = 3;
const CAT_SHORTCUTS: usize = 4;
const CAT_ADVANCED: usize = 5;
const CAT_ABOUT: usize = 6;
const CAT_SOUND: usize = 7;
const CAT_POWER: usize = 8;
const CAT_NETWORK: usize = 9;
const CAT_APPS: usize = 10;
const CAT_DATETIME: usize = 11;

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

/// One landing row: a vivid round icon + label, Samsung-settings style. A row
/// either drills into a glide detail category or launches a classic panel.
struct HomeRow {
    label: &'static str,
    glyph: u16,
    color: (u8, u8, u8),
    act: HomeAct,
}
#[derive(Clone, Copy)]
enum HomeAct {
    Cat(usize),
    Link(usize),
    /// Opens the glide Task Manager (processes + services).
    TaskMgr,
}

/// The landing, grouped into cards the way Samsung's settings clusters rows.
/// Network / sound / power / apps sit at the top level here, not buried.
const HOME: &[&[HomeRow]] = &[
    &[
        HomeRow { label: "네트워크", glyph: 0xE839, color: (46, 127, 239), act: HomeAct::Cat(CAT_NETWORK) },
        HomeRow { label: "소리", glyph: 0xE767, color: (245, 146, 60), act: HomeAct::Cat(CAT_SOUND) },
        HomeRow { label: "전원 · 배터리", glyph: 0xE83E, color: (52, 199, 123), act: HomeAct::Cat(CAT_POWER) },
    ],
    &[
        HomeRow { label: "개인화", glyph: 0xE771, color: (244, 114, 182), act: HomeAct::Cat(CAT_PERSONALIZE) },
        HomeRow { label: "작업 표시줄", glyph: 0xE7F4, color: (70, 192, 202), act: HomeAct::Cat(CAT_TASKBAR) },
    ],
    &[
        HomeRow { label: "앱 · 프로그램", glyph: 0xE71D, color: (167, 139, 250), act: HomeAct::Cat(CAT_APPS) },
        HomeRow { label: "시작 프로그램", glyph: 0xE7B5, color: (99, 102, 241), act: HomeAct::Cat(CAT_STARTUP) },
    ],
    &[
        HomeRow { label: "Windows 조정", glyph: 0xE90F, color: (100, 116, 139), act: HomeAct::Cat(CAT_TWEAKS) },
        HomeRow { label: "날짜 · 시간", glyph: 0xE917, color: (34, 211, 238), act: HomeAct::Cat(CAT_DATETIME) },
        HomeRow { label: "제어판 (클래식)", glyph: 0xE713, color: (148, 163, 184), act: HomeAct::Link(5) },
    ],
    &[
        HomeRow { label: "작업 관리자", glyph: 0xE9D9, color: (251, 191, 36), act: HomeAct::TaskMgr },
        HomeRow { label: "장치 관리자", glyph: 0xE772, color: (59, 130, 246), act: HomeAct::Link(6) },
        HomeRow { label: "디스크 관리", glyph: 0xEDA2, color: (16, 185, 129), act: HomeAct::Link(8) },
        HomeRow { label: "레지스트리 편집기", glyph: 0xE943, color: (244, 63, 94), act: HomeAct::Link(9) },
    ],
    &[
        HomeRow { label: "단축키", glyph: 0xE765, color: (139, 92, 246), act: HomeAct::Cat(CAT_SHORTCUTS) },
        HomeRow { label: "시스템 정보", glyph: 0xE946, color: (56, 189, 248), act: HomeAct::Cat(CAT_ABOUT) },
        HomeRow { label: "glide 설정 폴더", glyph: 0xE8B7, color: (148, 163, 184), act: HomeAct::Link(10) },
    ],
];

/// Flattened landing rows matching the current query (all rows when empty).
fn home_matches(query: &str) -> Vec<&'static HomeRow> {
    let q = query.trim();
    HOME.iter()
        .flat_map(|g| g.iter())
        .filter(|r| q.is_empty() || r.label.contains(q))
        .collect()
}

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
    Panel(usize),
    Accent(u8),
    Density(u8),
    /// Volume-slider track; the click x maps to a level within [l, r].
    Vol { l: f32, r: f32 },
    /// The mute button next to the volume slider.
    Mute,
    /// Pick cached render endpoint `i` as the default output device.
    AudioDev(usize),
    /// Activate cached power scheme `i`.
    PowerPlan(usize),
    /// Wi-Fi software radio toggle.
    WifiRadio,
    /// Connect to cached network `i` (or open the flyout for an unsaved one).
    WifiNet(usize),
    /// Bluetooth radio toggle.
    Bluetooth,
    /// Airplane-mode toggle.
    Airplane,
    /// Launch the uninstaller for cached app `i`.
    App(usize),
    /// Switch to cached time zone `i`.
    TimeZone(usize),
    /// Open the glide Task Manager.
    TaskMgr,
    Search,
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
    /// Master-volume slider + mute button for the default output.
    Volume,
    /// A selectable render endpoint (index into `audio_devs`).
    AudioDev(usize),
    /// A selectable power scheme (index into `power_plans`).
    PowerPlan(usize),
    /// Wi-Fi software-radio switch.
    WifiRadio,
    /// An available network (index into `net.nets`).
    WifiNet(usize),
    /// Bluetooth radio switch.
    Bluetooth,
    /// Airplane-mode switch.
    Airplane,
    /// An installed program (index into `apps`); click launches its uninstaller.
    App(usize),
    /// A selectable time zone (index into `zones`).
    TimeZone(usize),
    Info,
}

struct Row {
    title: String,
    sub: String,
    kind: RowKind,
}

/// Cached network state for the 네트워크 pane. Wi-Fi comes from wlanapi (any
/// apartment); the radios come from WinRT gathered on an MTA worker.
struct NetState {
    wifi_present: bool,
    wifi_on: bool,
    nets: Vec<crate::wifi::Net>,
    bt: Tri,
    airplane: Tri,
}

impl Default for NetState {
    fn default() -> Self {
        NetState { wifi_present: false, wifi_on: false, nets: Vec::new(), bt: Tri::Absent, airplane: Tri::Absent }
    }
}

/// Run a WinRT/COM job on a short-lived MTA thread and block for its result —
/// the settings UI thread is STA, and `Windows.Devices.Radios` async joins
/// deadlock there. Returns None only if the worker itself panicked.
fn on_mta<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        f()
    })
    .join()
    .ok()
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
    fmt_cat: IDWriteTextFormat,
    fmt_row: IDWriteTextFormat,
    fmt_sub: IDWriteTextFormat,
    fmt_seg: IDWriteTextFormat,
    fmt_g16: IDWriteTextFormat,
    scale: f32,
    w: f32,
    h: f32,
    /// Selected glide category shown in the right pane (when `panel` is None).
    cat: usize,
    /// When Some, the right pane shows a launch card for LINKS[panel] instead of
    /// a category; the matching master row is highlighted.
    panel: Option<usize>,
    /// Live filter typed into the master-list search pill (committed chars).
    query: String,
    hover: Option<Act>,
    tracking: bool,
    hits: Vec<(D2D_RECT_F, Act)>,
    cfg: crate::config::Settings,
    startup: Vec<crate::winsettings::StartupItem>,
    /// Render endpoints, cached on entry to the 소리 pane (COM enumerate is slow).
    audio_devs: Vec<crate::audiopolicy::Endpoint>,
    /// Default-output volume/mute, cached alongside `audio_devs`.
    vol: Option<(f32, bool)>,
    /// Power schemes, cached on entry to the 전원 pane.
    power_plans: Vec<crate::power::Plan>,
    /// Wi-Fi + radio state, cached on entry to the 네트워크 pane.
    net: NetState,
    /// Installed programs, cached on entry to the 앱 pane.
    apps: Vec<crate::apps::App>,
    /// Time zones, cached on entry to the 날짜·시간 pane.
    zones: Vec<crate::datetime::Zone>,
    /// Right-pane detail scroll; the master list has its own `nav_scroll`.
    scroll: f32,
    nav_scroll: f32,
    /// Last cursor client-x (logical), to route the wheel to a pane.
    mouse_x: f32,
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
            let fmt_cat = mkv(24.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let fmt_row = mkv(13.5, DWRITE_FONT_WEIGHT_NORMAL)?;
            let fmt_sub = mkv(11.5, DWRITE_FONT_WEIGHT_NORMAL)?;
            for f in [&fmt_cat, &fmt_row, &fmt_sub] {
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
                fmt_cat,
                fmt_row,
                fmt_sub,
                fmt_seg,
                fmt_g16,
                scale,
                w: (cw as f32) / scale,
                h: (ch as f32) / scale,
                cat: 0,
                panel: None,
                query: String::new(),
                hover: None,
                tracking: false,
                hits: Vec::new(),
                cfg: crate::config::load(),
                startup: Vec::new(),
                audio_devs: Vec::new(),
                vol: None,
                power_plans: Vec::new(),
                net: NetState::default(),
                apps: Vec::new(),
                zones: Vec::new(),
                scroll: 0.0,
                nav_scroll: 0.0,
                mouse_x: 0.0,
                bar: 0,
            })
        }
    }

    pub fn open(&mut self, bar: HWND) {
        self.bar = bar.0 as isize;
        self.cfg = crate::config::load();
        self.startup = crate::winsettings::list_startup();
        self.scroll = 0.0;
        self.nav_scroll = 0.0;
        self.cat = 0;
        self.panel = None;
        self.query.clear();
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
            CAT_SOUND => {
                let sub = match self.vol {
                    Some((v, true)) => format!("음소거됨 · {}%", (v * 100.0).round() as u32),
                    Some((v, false)) => format!("{}%", (v * 100.0).round() as u32),
                    None => "기본 출력 장치 없음".to_string(),
                };
                let mut v = vec![Row { title: "볼륨".to_string(), sub, kind: RowKind::Volume }];
                if self.audio_devs.is_empty() {
                    v.push(info("출력 장치 없음", "재생 가능한 오디오 장치가 없습니다".to_string()));
                } else {
                    v.extend(self.audio_devs.iter().enumerate().map(|(i, d)| Row {
                        title: if d.name.is_empty() { "(이름 없는 장치)".to_string() } else { d.name.clone() },
                        sub: if d.default { "기본 출력 장치".to_string() } else { "탭하여 기본으로 설정".to_string() },
                        kind: RowKind::AudioDev(i),
                    }));
                }
                v
            }
            CAT_POWER => {
                let bat = crate::power::battery();
                let mut v = vec![if bat.present {
                    let state = if bat.charging {
                        "충전 중"
                    } else if bat.ac {
                        "전원 연결됨"
                    } else {
                        "배터리 사용 중"
                    };
                    info("배터리", format!("{}% · {}", bat.percent, state))
                } else {
                    info("배터리", "배터리 없음 — 상시 전원 (데스크톱)".to_string())
                }];
                if self.power_plans.is_empty() {
                    v.push(info("전원 계획 없음", "powrprof에서 계획을 읽지 못했습니다".to_string()));
                } else {
                    v.extend(self.power_plans.iter().enumerate().map(|(i, p)| Row {
                        title: if p.name.is_empty() { "(이름 없는 계획)".to_string() } else { p.name.clone() },
                        sub: if p.active { "현재 활성 계획".to_string() } else { "탭하여 이 계획으로 전환".to_string() },
                        kind: RowKind::PowerPlan(i),
                    }));
                }
                v
            }
            CAT_NETWORK => {
                let mut v = Vec::new();
                if self.net.wifi_present {
                    v.push(Row {
                        title: "Wi‑Fi".to_string(),
                        sub: if self.net.wifi_on {
                            "켜짐 — 사용 가능한 네트워크".to_string()
                        } else {
                            "꺼짐".to_string()
                        },
                        kind: RowKind::WifiRadio,
                    });
                    if self.net.wifi_on {
                        if self.net.nets.is_empty() {
                            v.push(info("검색된 네트워크 없음", "잠시 후 다시 열면 목록이 채워집니다".to_string()));
                        } else {
                            v.extend(self.net.nets.iter().enumerate().map(|(i, n)| {
                                let mut tags = format!("{}%", n.signal);
                                if n.secured {
                                    tags.push_str(" · 보안");
                                }
                                if n.connected {
                                    tags.push_str(" · 연결됨");
                                } else if n.profile.is_some() {
                                    tags.push_str(" · 저장됨");
                                }
                                Row { title: n.ssid.clone(), sub: tags, kind: RowKind::WifiNet(i) }
                            }));
                        }
                    }
                } else {
                    v.push(info("Wi‑Fi 없음", "무선 어댑터를 찾지 못했습니다".to_string()));
                }
                if self.net.bt.present() {
                    v.push(Row {
                        title: "Bluetooth".to_string(),
                        sub: if self.net.bt.is_on() { "켜짐".to_string() } else { "꺼짐".to_string() },
                        kind: RowKind::Bluetooth,
                    });
                }
                if self.net.airplane.present() {
                    v.push(Row {
                        title: "비행기 모드".to_string(),
                        sub: if self.net.airplane.is_on() {
                            "모든 무선 꺼짐".to_string()
                        } else {
                            "꺼짐".to_string()
                        },
                        kind: RowKind::Airplane,
                    });
                }
                v
            }
            CAT_APPS => {
                if self.apps.is_empty() {
                    vec![info("설치된 프로그램 없음", "레지스트리에서 항목을 찾지 못했습니다".to_string())]
                } else {
                    let mut v = vec![info(
                        "설치된 프로그램",
                        format!("{}개 — 항목을 탭하면 제거 관리자가 실행됩니다", self.apps.len()),
                    )];
                    v.extend(self.apps.iter().enumerate().map(|(i, a)| {
                        let mut sub = a.publisher.clone();
                        if !a.version.is_empty() {
                            if !sub.is_empty() {
                                sub.push_str(" · ");
                            }
                            sub.push_str(&a.version);
                        }
                        if sub.is_empty() {
                            sub.push_str("탭하여 제거");
                        }
                        Row { title: a.name.clone(), sub: trunc(&sub, 80), kind: RowKind::App(i) }
                    }));
                    v
                }
            }
            CAT_DATETIME => {
                let (date, time) = crate::datetime::now();
                let mut v = vec![
                    info("날짜", date),
                    info("시간", time),
                    info("시간대 선택", "탭하여 표준 시간대를 변경합니다 (관리자 권한 불필요)".to_string()),
                ];
                v.extend(self.zones.iter().enumerate().map(|(i, z)| Row {
                    title: z.display.clone(),
                    sub: z.key.clone(),
                    kind: RowKind::TimeZone(i),
                }));
                v.push(info("시계·자동 동기화", "시각 설정과 인터넷 시간 동기화는 관리자 권한 — 고급·도구의 날짜·시간에서".to_string()));
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

    /// Reload the COM-backed caches for whichever dynamic pane is now shown.
    /// Cheap panes (personalize, tweaks…) leave the caches untouched.
    fn refresh_dynamic(&mut self) {
        match self.cat {
            CAT_SOUND => {
                self.audio_devs = crate::audiopolicy::list_render();
                self.vol = crate::audiopolicy::volume();
            }
            CAT_POWER => {
                self.power_plans = crate::power::plans();
            }
            CAT_NETWORK => self.refresh_network(),
            CAT_APPS => self.apps = crate::apps::installed(),
            CAT_DATETIME => self.zones = crate::datetime::zones(),
            _ => {}
        }
    }

    fn zone_pick(&mut self, i: usize) {
        if let Some(z) = self.zones.get(i) {
            if z.current {
                return;
            }
            crate::datetime::set_zone(z);
        }
        // Re-enumerate so the check mark and the clock reflect the new zone.
        self.zones = crate::datetime::zones();
        self.paint();
    }

    /// Re-read Wi-Fi (wlanapi, this thread) and the radios (WinRT via MTA).
    fn refresh_network(&mut self) {
        let (wifi_present, wifi_on, nets) = if let Some(w) = Wifi::open() {
            w.scan(); // async — populates the cache the next entry reads
            (true, w.radio_on(), w.networks())
        } else {
            (false, false, Vec::new())
        };
        let snap = on_mta(quicksettings::snapshot)
            .unwrap_or(RadioSnapshot { bluetooth: Tri::Absent, airplane: Tri::Absent });
        self.net = NetState { wifi_present, wifi_on, nets, bt: snap.bluetooth, airplane: snap.airplane };
    }

    fn wifi_toggle(&mut self) {
        if let Some(w) = Wifi::open() {
            w.set_radio(!self.net.wifi_on);
        }
        self.refresh_network();
        self.paint();
    }

    fn wifi_connect(&mut self, i: usize) {
        if let Some(n) = self.net.nets.get(i) {
            match n.profile.clone() {
                Some(prof) => {
                    if let Some(w) = Wifi::open() {
                        w.connect(&prof);
                    }
                }
                // No saved profile → needs a password UI we don't own; hand the
                // one join we can't do to the stock Wi-Fi entry.
                None => crate::winsettings::launch("ms-settings:network-wifi"),
            }
        }
        self.refresh_network();
        self.paint();
    }

    fn bt_toggle(&mut self) {
        let on = self.net.bt.is_on();
        if let Some(snap) =
            on_mta(move || {
                quicksettings::set_bluetooth(!on);
                quicksettings::snapshot()
            })
        {
            self.net.bt = snap.bluetooth;
            self.net.airplane = snap.airplane;
        }
        self.paint();
    }

    fn airplane_toggle(&mut self) {
        let on = self.net.airplane.is_on();
        let _ = on_mta(move || quicksettings::set_airplane(!on));
        // Airplane flips Wi-Fi too, so re-read the whole pane.
        self.refresh_network();
        self.paint();
    }

    fn app_uninstall(&mut self, i: usize) {
        if let Some(a) = self.apps.get(i) {
            crate::apps::uninstall(&a.uninstall);
        }
    }

    fn vol_set(&mut self, level: f32) {
        crate::audiopolicy::set_volume(level);
        self.vol = crate::audiopolicy::volume();
        self.paint();
    }

    fn mute_toggle(&mut self) {
        let now = self.vol.map(|(_, m)| m).unwrap_or(false);
        crate::audiopolicy::set_mute(!now);
        self.vol = crate::audiopolicy::volume();
        self.paint();
    }

    fn output_pick(&mut self, i: usize) {
        if let Some(dev) = self.audio_devs.get(i) {
            if dev.default {
                return;
            }
            let _ = crate::audiopolicy::set_default_endpoint(&dev.id);
        }
        // Re-read so the check mark and volume follow the new default.
        self.audio_devs = crate::audiopolicy::list_render();
        self.vol = crate::audiopolicy::volume();
        self.paint();
    }

    fn plan_pick(&mut self, i: usize) {
        if let Some(p) = self.power_plans.get(i) {
            if p.active {
                return;
            }
            crate::power::set_plan(&p.guid);
        }
        self.power_plans = crate::power::plans();
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
        fill_round(&self.renderer, r, radius, c);
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

    /// The right (detail) pane's content column, to the right of the master nav.
    fn right_col(&self) -> (f32, f32) {
        let cx0 = NAV_W2 + 28.0;
        let cx1 = (self.w - 32.0).max(cx0 + 120.0);
        (cx0, cx1)
    }

    fn paint(&mut self) {
        self.hits.clear();
        unsafe {
            self.renderer.dc.BeginDraw();
            // Low alpha: Mica does the wallpaper tinting, we just darken.
            self.renderer.dc.Clear(Some(&theme::rgba(24, 25, 30, 0.72)));
        }
        self.paint_nav();
        self.paint_right();
        unsafe {
            let _ = self.renderer.dc.EndDraw(None, None);
            let _ = self.renderer.present();
        }
    }

    /// Total scroll height of the master list (for the wheel clamp).
    fn nav_content_h(&self) -> f32 {
        if self.query.trim().is_empty() {
            HOME.iter().map(|g| g.len() as f32 * NAV_ROW_H).sum::<f32>()
                + (HOME.len().saturating_sub(1)) as f32 * NAV_SEP
        } else {
            home_matches(&self.query).len() as f32 * NAV_ROW_H
        }
    }

    /// One master-list row: colour circle + label, selection/hover fill.
    fn paint_nav_row(&mut self, y: f32, hr: &HomeRow) -> f32 {
        let (act, selected) = match hr.act {
            HomeAct::Cat(c) => (Act::Nav(c), self.panel.is_none() && self.cat == c),
            HomeAct::Link(l) => (Act::Panel(l), self.panel == Some(l)),
            HomeAct::TaskMgr => (Act::TaskMgr, false),
        };
        let row = rect(10.0, y, NAV_W2 - 10.0, y + NAV_ROW_H);
        if selected {
            self.fill_round(row, 12.0, theme::rgba(255, 255, 255, 0.10));
        } else if self.hover == Some(act) {
            self.fill_round(row, 12.0, theme::HOVER_FILL);
        }
        self.icon_circle(40.0, y + NAV_ROW_H / 2.0, hr.color, hr.glyph);
        self.text(hr.label, &self.fmt_row.clone(), rect(72.0, y, NAV_W2 - 16.0, y + NAV_ROW_H), theme::TEXT);
        self.hits.push((row, act));
        y + NAV_ROW_H
    }

    /// The left master list (Samsung two-pane): grouped colour-icon rows with
    /// separators, a selection highlight, and a pinned search pill.
    fn paint_nav(&mut self) {
        self.fill_round(rect(NAV_W2, 0.0, NAV_W2 + 1.0, self.h), 0.0, theme::rgba(255, 255, 255, 0.06));
        let content_top = 14.0;
        let clip = rect(0.0, content_top, NAV_W2, self.h - 66.0);
        unsafe {
            self.renderer.dc.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED);
        }
        let mut y = 18.0 - self.nav_scroll;
        if self.query.trim().is_empty() {
            for (gi, g) in HOME.iter().enumerate() {
                for hr in g.iter() {
                    y = self.paint_nav_row(y, hr);
                }
                if gi + 1 < HOME.len() {
                    self.fill_round(rect(24.0, y + 7.0, NAV_W2 - 24.0, y + 8.0), 0.0, theme::rgba(255, 255, 255, 0.06));
                    y += NAV_SEP;
                }
            }
        } else {
            let rows = home_matches(&self.query);
            if rows.is_empty() {
                self.text("결과 없음", &self.fmt_row.clone(), rect(24.0, y + 6.0, NAV_W2 - 12.0, y + 46.0), theme::TEXT_DIM);
            } else {
                for hr in &rows {
                    y = self.paint_nav_row(y, hr);
                }
            }
        }
        unsafe {
            self.renderer.dc.PopAxisAlignedClip();
        }

        // Pinned search pill (Samsung docks it bottom-left).
        let pill = rect(14.0, self.h - 54.0, NAV_W2 - 14.0, self.h - 14.0);
        let hot = self.hover == Some(Act::Search);
        self.fill_round(pill, 20.0, theme::rgba(255, 255, 255, if hot { 0.12 } else { 0.08 }));
        self.glyph(0xE721, rect(22.0, self.h - 54.0, 54.0, self.h - 14.0), theme::TEXT_DIM);
        let (txt, tcol) = if self.query.is_empty() {
            ("설정 검색".to_string(), theme::TEXT_DIM)
        } else {
            (format!("{}|", self.query), theme::TEXT)
        };
        self.text(&txt, &self.fmt_row.clone(), rect(56.0, self.h - 54.0, NAV_W2 - 22.0, self.h - 14.0), tcol);
        self.hits.push((pill, Act::Search));
    }

    /// A vivid filled circle with a white glyph — the Samsung row icon.
    fn icon_circle(&self, cx: f32, cy: f32, color: (u8, u8, u8), glyph: u16) {
        unsafe {
            if let Ok(b) = self.renderer.brush(theme::rgba(color.0, color.1, color.2, 1.0)) {
                self.renderer.dc.FillEllipse(
                    &D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: 18.0, radiusY: 18.0 },
                    &b,
                );
            }
        }
        self.glyph(glyph, rect(cx - 18.0, cy - 18.0, cx + 18.0, cy + 18.0), theme::rgba(255, 255, 255, 1.0));
    }

    /// The right detail pane: a launch card for a classic Windows panel, or the
    /// selected glide category's cards.
    fn paint_right(&mut self) {
        let (cx0, cx1) = self.right_col();
        if let Some(p) = self.panel {
            let (label, sub, _) = LINKS[p];
            self.text(label, &self.fmt_cat.clone(), rect(cx0, 14.0, cx1, 58.0), theme::TEXT);
            let card = rect(cx0, 80.0, cx1, 214.0);
            self.fill_round(card, 12.0, theme::rgba(255, 255, 255, 0.05));
            self.text(sub, &self.fmt_row.clone(), rect(cx0 + 22.0, 98.0, cx1 - 22.0, 132.0), theme::TEXT);
            self.text(
                "장치·서비스·디스크·레지스트리는 별도 시스템 콘솔입니다 — glide가 직접 실행합니다.",
                &self.fmt_sub.clone(),
                rect(cx0 + 22.0, 130.0, cx1 - 22.0, 158.0),
                theme::TEXT_DIM,
            );
            let btn = rect(cx0 + 22.0, 166.0, cx0 + 132.0, 200.0);
            let hot = self.hover == Some(Act::Link(p));
            self.fill_round(btn, 8.0, if hot { theme::accent() } else { theme::rgba(255, 255, 255, 0.12) });
            self.text(
                "열기",
                &self.fmt_seg.clone(),
                btn,
                if hot { theme::rgba(23, 24, 28, 1.0) } else { theme::TEXT },
            );
            self.hits.push((btn, Act::Link(p)));
            return;
        }
        self.text(CATS[self.cat].0, &self.fmt_cat.clone(), rect(cx0, 14.0, cx1, 58.0), theme::TEXT);
        let content_top = 72.0;
        let clip = rect(cx0 - 6.0, content_top, cx1 + 6.0, self.h);
        unsafe {
            self.renderer.dc.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED);
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
            #[allow(clippy::type_complexity)]
            let (act, sw, link, accent, density, volume, check): (
                Option<Act>,
                Option<bool>,
                bool,
                bool,
                Option<u8>,
                bool,
                Option<bool>,
            ) = match row.kind {
                RowKind::Toggle(t) => {
                    (Some(Act::Toggle(t)), Some(self.toggle_value(t)), false, false, None, false, None)
                }
                RowKind::Win(w) => (Some(Act::Win(w)), Some(self.win_value(w)), false, false, None, false, None),
                RowKind::Startup(s) => (
                    Some(Act::Startup(s)),
                    Some(self.startup.get(s).map(|x| x.enabled).unwrap_or(false)),
                    false,
                    false,
                    None,
                    false,
                    None,
                ),
                RowKind::Tweak(t) => {
                    (Some(Act::Tweak(t)), Some(self.tweak_value(t)), false, false, None, false, None)
                }
                RowKind::Link(l) => (Some(Act::Link(l)), None, true, false, None, false, None),
                RowKind::Accent => (None, None, false, true, None, false, None),
                RowKind::Density => (None, None, false, false, Some(self.cfg.bar_density), false, None),
                RowKind::Volume => (None, None, false, false, None, true, None),
                RowKind::AudioDev(i) => (
                    Some(Act::AudioDev(i)),
                    None,
                    false,
                    false,
                    None,
                    false,
                    Some(self.audio_devs.get(i).map(|d| d.default).unwrap_or(false)),
                ),
                RowKind::PowerPlan(i) => (
                    Some(Act::PowerPlan(i)),
                    None,
                    false,
                    false,
                    None,
                    false,
                    Some(self.power_plans.get(i).map(|p| p.active).unwrap_or(false)),
                ),
                RowKind::WifiRadio => {
                    (Some(Act::WifiRadio), Some(self.net.wifi_on), false, false, None, false, None)
                }
                RowKind::WifiNet(i) => (
                    Some(Act::WifiNet(i)),
                    None,
                    false,
                    false,
                    None,
                    false,
                    Some(self.net.nets.get(i).map(|n| n.connected).unwrap_or(false)),
                ),
                RowKind::Bluetooth => {
                    (Some(Act::Bluetooth), Some(self.net.bt.is_on()), false, false, None, false, None)
                }
                RowKind::Airplane => {
                    (Some(Act::Airplane), Some(self.net.airplane.is_on()), false, false, None, false, None)
                }
                RowKind::App(i) => (Some(Act::App(i)), None, true, false, None, false, None),
                RowKind::TimeZone(i) => (
                    Some(Act::TimeZone(i)),
                    None,
                    false,
                    false,
                    None,
                    false,
                    Some(self.zones.get(i).map(|z| z.current).unwrap_or(false)),
                ),
                RowKind::Info => (None, None, false, false, None, false, None),
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
            if volume {
                let (level, muted) = self.vol.unwrap_or((0.0, false));
                let cy = y + CARD_H / 2.0;
                // Mute button on the far right; accent-filled when muted.
                let mb = rect(cx1 - 44.0, cy - 15.0, cx1 - 14.0, cy + 15.0);
                self.fill_round(mb, 8.0, if muted { theme::accent() } else { theme::rgba(255, 255, 255, 0.10) });
                self.glyph(
                    if muted { 0xE74F } else { 0xE767 },
                    mb,
                    if muted { theme::rgba(23, 24, 28, 1.0) } else { theme::TEXT },
                );
                self.hits.push((mb, Act::Mute));
                // Slider track between the labels and the mute button.
                let tx0 = cx0 + 150.0;
                let tx1 = cx1 - 62.0;
                if tx1 > tx0 + 20.0 {
                    let knob = tx0 + level.clamp(0.0, 1.0) * (tx1 - tx0);
                    self.fill_round(rect(tx0, cy - 2.0, tx1, cy + 2.0), 2.0, theme::rgba(255, 255, 255, 0.16));
                    if !muted {
                        self.fill_round(rect(tx0, cy - 2.0, knob, cy + 2.0), 2.0, theme::accent());
                    }
                    unsafe {
                        let kc = if muted { theme::rgba(160, 163, 170, 1.0) } else { theme::accent() };
                        if let Ok(b) = self.renderer.brush(kc) {
                            self.renderer.dc.FillEllipse(
                                &D2D1_ELLIPSE { point: Vector2 { X: knob, Y: cy }, radiusX: 8.0, radiusY: 8.0 },
                                &b,
                            );
                        }
                    }
                    self.hits.push((
                        rect(tx0 - 8.0, cy - 14.0, tx1 + 8.0, cy + 14.0),
                        Act::Vol { l: tx0, r: tx1 },
                    ));
                }
            }
            if let Some(sel) = check {
                if sel {
                    // A radio-style check on the selected device / active plan.
                    self.glyph(0xE73E, rect(cx1 - 46.0, y, cx1 - 14.0, y + CARD_H), theme::accent());
                }
            }
            if let Some(a) = act {
                self.hits.push((card, a));
            }
        }
        unsafe {
            self.renderer.dc.PopAxisAlignedClip();
        }
    }
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
                app.mouse_x = x;
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
                    // A master-list category selects the right pane.
                    Some(Act::Nav(i)) => {
                        if app.panel.is_some() || app.cat != i {
                            app.panel = None;
                            app.cat = i;
                            app.scroll = 0.0;
                            // Startup can change out from under us (installers,
                            // Task Manager); re-read on entry so it's current.
                            if i == CAT_STARTUP {
                                app.startup = crate::winsettings::list_startup();
                            }
                            // Sound/power panes read live COM state on entry.
                            app.refresh_dynamic();
                            app.hover = None;
                            app.paint();
                        }
                    }
                    // A classic-panel row shows its launch card on the right.
                    Some(Act::Panel(p)) => {
                        if app.panel != Some(p) {
                            app.panel = Some(p);
                            app.scroll = 0.0;
                            app.hover = None;
                            app.paint();
                        }
                    }
                    // The pill only marks focus; typing filters live.
                    Some(Act::Search) => {}
                    Some(Act::Toggle(t)) => app.flip(t),
                    Some(Act::Win(w)) => app.win_flip(w),
                    Some(Act::Startup(s)) => app.startup_flip(s),
                    Some(Act::Tweak(t)) => app.tweak_flip(t),
                    Some(Act::Accent(i)) => app.accent_pick(i),
                    Some(Act::Density(d)) => app.density_pick(d),
                    Some(Act::Vol { l, r }) => {
                        let frac = ((x - l) / (r - l)).clamp(0.0, 1.0);
                        app.vol_set(frac);
                    }
                    Some(Act::Mute) => app.mute_toggle(),
                    Some(Act::AudioDev(i)) => app.output_pick(i),
                    Some(Act::PowerPlan(i)) => app.plan_pick(i),
                    Some(Act::WifiRadio) => app.wifi_toggle(),
                    Some(Act::WifiNet(i)) => app.wifi_connect(i),
                    Some(Act::Bluetooth) => app.bt_toggle(),
                    Some(Act::Airplane) => app.airplane_toggle(),
                    Some(Act::App(i)) => app.app_uninstall(i),
                    Some(Act::TimeZone(i)) => app.zone_pick(i),
                    Some(Act::TaskMgr) => {
                        if let Ok(exe) = std::env::current_exe() {
                            let _ = std::process::Command::new(exe).arg("--taskmgr").spawn();
                        }
                    }
                    // The launch card's 열기 button opens the classic panel.
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
                // Route the wheel to whichever pane the cursor was last over.
                if app.mouse_x < NAV_W2 {
                    let vis = (app.h - 80.0).max(0.0);
                    let max = (app.nav_content_h() - vis).max(0.0);
                    let ns = (app.nav_scroll - delta).clamp(0.0, max);
                    if ns != app.nav_scroll {
                        app.nav_scroll = ns;
                        app.hover = None;
                        app.paint();
                    }
                } else if app.panel.is_none() {
                    let content_h = app.rows().len() as f32 * (CARD_H + CARD_GAP);
                    let vis = (app.h - 92.0).max(0.0);
                    let max = (content_h - vis).max(0.0);
                    let ns = (app.scroll - delta).clamp(0.0, max);
                    if ns != app.scroll {
                        app.scroll = ns;
                        app.hover = None;
                        app.paint();
                    }
                }
                LRESULT(0)
            }
            WM_CHAR => {
                // Type-to-filter the master list. WM_CHAR delivers committed
                // characters (including whole Hangul syllables post-IME).
                let c = wparam.0 as u32;
                let mut changed = false;
                if c == 0x08 {
                    changed = app.query.pop().is_some();
                } else if c >= 0x20 && c != 0x7F {
                    if let Some(ch) = char::from_u32(c) {
                        app.query.push(ch);
                        changed = true;
                    }
                }
                if changed {
                    app.nav_scroll = 0.0;
                    app.hover = None;
                    app.paint();
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wparam.0 == VK_ESCAPE.0 as usize {
                    // Esc clears an active filter first, else closes.
                    if !app.query.is_empty() {
                        app.query.clear();
                        app.nav_scroll = 0.0;
                        app.paint();
                    } else {
                        app.hide();
                    }
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
