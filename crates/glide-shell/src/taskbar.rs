//! M1 taskbar (SHELL_DESIGN §6.1): bottom bar, appbar reservation, window
//! list via shell hook + EnumWindows resync, click to activate/minimize,
//! pinned apps, clock. Alongside-explorer mode: the appbar system stacks us
//! above the stock taskbar; once explorer is gone we own the true bottom edge.
//!
//! Ordering contract (user feedback 0721): buttons never reshuffle on
//! activation. Pinned exes hold the leftmost slots in pin order; running
//! windows keep first-seen order, new ones append at the end.

use std::collections::HashMap;
use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows_numerics::Vector2;
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_ROUNDED_RECT, ID2D1Bitmap1,
};
use windows::Win32::Graphics::DirectWrite::DWRITE_MEASURING_MODE_NATURAL;
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmGetWindowAttribute,
    DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, ScreenToClient,
    ValidateRect,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::Shell::{
    ABE_BOTTOM, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, APPBARDATA, SHAppBarMessage,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCWSTR, PWSTR, w};

use crate::render::Renderer;
use crate::{icons, theme};

pub(crate) const WM_APPBAR: u32 = WM_APP + 1;
// In windows-rs metadata this lives in Win32_UI_Controls; not worth the
// feature for one message id.
const WM_MOUSELEAVE: u32 = 0x02A3;
/// Shell_NotifyIcon v4 event codes (WM_USER-relative on the wire).
const NIN_SELECT: u32 = 0x0400;
pub(crate) const ABN_POSCHANGED_ID: usize = 1;
const TIMER_CLOCK: usize = 1;
const TIMER_ANIM: usize = 2;
const TIMER_RESYNC: usize = 3;
const TIMER_PREVIEW: usize = 4;
const PREVIEW_DELAY_MS: u32 = 350;
/// Buttons shrink under crowding but never below this.
const BUTTON_MIN_W: f32 = 48.0;
const CLOCK_W: f32 = 84.0;
const LAUNCHER_W: f32 = 40.0;
const TRAY_CELL_W: f32 = 24.0;
const STATUS_CELL_W: f32 = 26.0;
const MENU_PIN: usize = 1;
const MENU_UNPIN: usize = 2;
const MENU_CLOSE: usize = 3;
const MENU_TASKMGR: usize = 4;
const MENU_RESTART: usize = 5;
const MENU_QUIT: usize = 6;
/// Start button: glyph square at the far left, entries begin after it.
const START_X: f32 = 8.0;
const START_BTN_W: f32 = 40.0;
const ENTRY_X0: f32 = START_X + START_BTN_W + 4.0;

struct TrayIcon {
    owner: HWND,
    uid: u32,
    callback: u32,
    version: u32,
    bitmap: Option<ID2D1Bitmap1>,
    tip: String,
    hidden: bool,
}

struct Entry {
    /// None = pinned exe with no window (launcher slot).
    hwnd: Option<HWND>,
    exe: Option<String>,
    title: Vec<u16>,
    /// Display width; shrunk from `full_width` when the bar is crowded.
    width: f32,
    full_width: f32,
    flash: bool,
    pinned: bool,
}

impl Entry {
    /// Animation key, stable across refreshes: window handle, or the pin
    /// slot index for launchers.
    fn key(&self, pin_idx: usize) -> i64 {
        match self.hwnd {
            Some(h) => h.0 as i64,
            None => -(pin_idx as i64 + 1),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Anim {
    hover: f32,
    active: f32,
}

struct Drag {
    idx: usize,
    press_x: f32,
    cur_x: f32,
    /// press_x − button left at press time (grab anchor).
    offset: f32,
    /// True once movement exceeds the click threshold.
    active: bool,
}

pub struct Bar {
    hwnd: HWND,
    renderer: Renderer,
    entries: Vec<Entry>,
    /// entries[i] → key for anims (precomputed at refresh).
    entry_keys: Vec<i64>,
    anims: HashMap<i64, Anim>,
    icon_cache: HashMap<isize, Option<ID2D1Bitmap1>>,
    exe_icon_cache: HashMap<String, Option<ID2D1Bitmap1>>,
    /// Pinned exe paths (lowercase), leftmost-first. Persisted.
    pins: Vec<String>,
    /// First-seen order of running windows — the anti-reshuffle contract.
    running_order: Vec<isize>,
    active: HWND,
    hover: Option<usize>,
    drag: Option<Drag>,
    tracking_leave: bool,
    anim_timer: bool,
    last_tick: Instant,
    shellhook_msg: u32,
    width: f32, // logical
    tray_icons: Vec<TrayIcon>,
    tray_hover: Option<usize>,
    status: crate::status::Status,
    status_hover: Option<usize>,
    preview: crate::preview::Preview,
    flyout: crate::flyout::Flyout,
    /// One lightweight bar per non-primary monitor (M5); rebuilt wholesale
    /// on WM_DISPLAYCHANGE.
    secondaries: Vec<Box<crate::secondary::Secondary>>,
    start: crate::startmenu::StartMenu,
    start_hover: bool,
}

/// Cells in the status cluster, left→right; presence varies (no battery on
/// desktops, no 한/영 on non-Korean layouts).
#[derive(Clone, Copy, PartialEq)]
enum StatusCell {
    Ime,
    Net,
    Vol,
    Bat,
}

pub fn run(claim_tray: bool) -> anyhow::Result<()> {
    unsafe {
        // Status cluster (volume/network) talks COM on this thread.
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
        let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
        let class = w!("glide_shell_bar");
        let wc = WNDCLASSW {
            // CS_DBLCLKS: legacy tray icons act on WM_LBUTTONDBLCLK.
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            anyhow::bail!("RegisterClassW failed");
        }

        let mon = primary_monitor_rect();
        // Rough initial placement; the appbar negotiation below moves us.
        // TOPMOST like explorer's taskbar: without it any normal window that
        // ignores the work area (Zetile tiles the full monitor) sits over the
        // bar and swallows its clicks. Fullscreen-app auto-hide is an M6 item.
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
            class,
            w!("glide-shell taskbar"),
            WS_POPUP,
            mon.left,
            mon.bottom - 60,
            mon.right - mon.left,
            60,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?;

        // Dark mode + acrylic backdrop behind our premultiplied swapchain.
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
        let backdrop: i32 = 3; // DWMSBT_TRANSIENTWINDOW (acrylic) — glint's recipe
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &backdrop as *const _ as _, 4);

        let dpi = GetDpiForWindow(hwnd) as f32;
        let scale = dpi / 96.0;
        let bar_h = (theme::BAR_HEIGHT * scale).round() as i32;
        let rect = appbar_negotiate(hwnd, bar_h);
        let (w_px, h_px) = (rect.right - rect.left, rect.bottom - rect.top);
        MoveWindow(hwnd, rect.left, rect.top, w_px, h_px, true)?;

        let renderer = Renderer::new(hwnd, w_px as u32, h_px as u32, dpi)?;

        let shellhook_msg = RegisterWindowMessageW(w!("SHELLHOOK"));
        let _ = RegisterShellHookWindow(hwnd);

        let mut bar = Bar {
            hwnd,
            renderer,
            entries: Vec::new(),
            entry_keys: Vec::new(),
            anims: HashMap::new(),
            icon_cache: HashMap::new(),
            exe_icon_cache: HashMap::new(),
            pins: load_pins(),
            running_order: Vec::new(),
            active: GetForegroundWindow(),
            hover: None,
            drag: None,
            tracking_leave: false,
            anim_timer: false,
            last_tick: Instant::now(),
            shellhook_msg,
            width: w_px as f32 / scale,
            tray_icons: Vec::new(),
            tray_hover: None,
            status: crate::status::Status::new(),
            status_hover: None,
            preview: crate::preview::Preview::new(dpi)?,
            flyout: crate::flyout::Flyout::new(dpi)?,
            secondaries: Vec::new(),
            start: crate::startmenu::StartMenu::new(dpi)?,
            start_hover: false,
        };
        bar.refresh();
        bar.rebuild_secondaries();
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut bar as *mut Bar as isize);
        if claim_tray {
            crate::tray::claim(&mut bar as *mut Bar)?;
        }

        SetTimer(Some(hwnd), TIMER_CLOCK, 1000, None);
        SetTimer(Some(hwnd), TIMER_RESYNC, 2000, None);

        if let Err(e) = crate::desktop::spawn(dpi) {
            eprintln!("glide-shell: desktop window failed: {e}");
        }
        crate::winkey::install(hwnd);

        // Toast cards and the volume OSD live on this thread but own their
        // windows; the bar never needs to know about them.
        let mut toasts = crate::toasts::Toasts::new(dpi)?;
        toasts.arm();
        let mut osd = crate::osd::Osd::new(dpi)?;
        osd.arm();
        osd.set_quiet_peer(bar.flyout.hwnd());

        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        bar.paint();

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        osd.disarm();
        toasts.disarm();
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: hwnd,
            ..Default::default()
        };
        SHAppBarMessage(ABM_REMOVE, &mut abd);
    }
    Ok(())
}

fn pins_path() -> std::path::PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(base).join("glide-shell").join("pins.txt")
}

fn load_pins() -> Vec<String> {
    std::fs::read_to_string(pins_path())
        .map(|s| {
            s.lines()
                .map(|l| l.trim().to_lowercase())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn save_pins(pins: &[String]) {
    let p = pins_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, pins.join("\n"));
}

/// Bring a window to the foreground from this WS_EX_NOACTIVATE bar.
///
/// Plain SetForegroundWindow loses the foreground-lock fight: our click never
/// made us the foreground process, so the system refuses the switch (the tray
/// had the mirror problem — see AllowSetForegroundWindow in tray_forward).
/// Borrow the current foreground thread's input state via AttachThreadInput;
/// as a last resort tap Alt, which resets the lock (the keybd_event
/// workaround every shell replacement ends up shipping).
pub(crate) fn force_foreground(hwnd: HWND) {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
        VK_MENU,
    };
    unsafe {
        if SetForegroundWindow(hwnd).as_bool() {
            return;
        }
        let fg = GetForegroundWindow();
        if !fg.is_invalid() {
            let fg_tid = GetWindowThreadProcessId(fg, None);
            let our_tid = GetCurrentThreadId();
            if fg_tid != 0 && fg_tid != our_tid {
                let _ = AttachThreadInput(our_tid, fg_tid, true);
                let ok = SetForegroundWindow(hwnd).as_bool();
                let _ = AttachThreadInput(our_tid, fg_tid, false);
                if ok {
                    return;
                }
            }
        }
        let mk = |flags: KEYBD_EVENT_FLAGS| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: VK_MENU, dwFlags: flags, ..Default::default() },
            },
        };
        let inputs = [mk(KEYBD_EVENT_FLAGS(0)), mk(KEYEVENTF_KEYUP)];
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        let _ = SetForegroundWindow(hwnd);
    }
}

fn window_exe(hwnd: HWND) -> Option<String> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]).to_lowercase())
    }
}

fn primary_monitor_rect() -> RECT {
    unsafe {
        let hmon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            mi.rcMonitor
        } else {
            RECT { left: 0, top: 0, right: 1920, bottom: 1080 }
        }
    }
}

/// ABM_NEW + QUERYPOS/SETPOS against `mon`. The system pushes our rect above
/// any existing bottom appbar (explorer's taskbar included), so coexistence
/// is automatic. Shared with the secondary-monitor bars.
pub(crate) fn appbar_negotiate_on(hwnd: HWND, height_px: i32, mon: RECT) -> RECT {
    unsafe {
        let mut abd = appbar_data(hwnd, height_px, mon);
        SHAppBarMessage(ABM_NEW, &mut abd);
        SHAppBarMessage(ABM_QUERYPOS, &mut abd);
        abd.rc.top = abd.rc.bottom - height_px;
        SHAppBarMessage(ABM_SETPOS, &mut abd);
        abd.rc
    }
}

/// QUERYPOS/SETPOS only — re-negotiate a slot that already did ABM_NEW.
pub(crate) fn appbar_requery(hwnd: HWND, height_px: i32, mon: RECT) -> RECT {
    unsafe {
        let mut abd = appbar_data(hwnd, height_px, mon);
        SHAppBarMessage(ABM_QUERYPOS, &mut abd);
        abd.rc.top = abd.rc.bottom - height_px;
        SHAppBarMessage(ABM_SETPOS, &mut abd);
        abd.rc
    }
}

fn appbar_data(hwnd: HWND, height_px: i32, mon: RECT) -> APPBARDATA {
    APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        hWnd: hwnd,
        uCallbackMessage: WM_APPBAR,
        uEdge: ABE_BOTTOM,
        rc: RECT {
            left: mon.left,
            right: mon.right,
            top: mon.bottom - height_px,
            bottom: mon.bottom,
        },
        ..Default::default()
    }
}

fn appbar_negotiate(hwnd: HWND, height_px: i32) -> RECT {
    appbar_negotiate_on(hwnd, height_px, primary_monitor_rect())
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let bar_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Bar;
        if bar_ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let bar = &mut *bar_ptr;
        if msg == bar.shellhook_msg {
            bar.on_shellhook(wparam.0, lparam.0);
            return LRESULT(0);
        }
        match msg {
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                bar.paint();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_TIMER => {
                match wparam.0 {
                    TIMER_CLOCK => {
                        bar.status.poll();
                        bar.paint();
                    }
                    TIMER_ANIM => bar.tick_anims(),
                    TIMER_RESYNC => bar.resync(),
                    TIMER_PREVIEW => {
                        let _ = KillTimer(Some(hwnd), TIMER_PREVIEW);
                        bar.preview_fire();
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32 / bar.scale();
                if bar.drag.is_some() {
                    bar.drag_move(x);
                } else {
                    let th = bar.tray_hit(x);
                    let sh = bar.status_hit(x);
                    let sth = th.is_none() && sh.is_none() && bar.start_hit(x);
                    let eh = if th.is_none() && sh.is_none() { bar.hit_test(x) } else { None };
                    let changed =
                        th != bar.tray_hover || sh != bar.status_hover || eh != bar.hover;
                    if th != bar.tray_hover || sh != bar.status_hover || sth != bar.start_hover {
                        bar.tray_hover = th;
                        bar.status_hover = sh;
                        bar.start_hover = sth;
                        bar.paint();
                    }
                    bar.set_hover(eh);
                    if changed {
                        bar.schedule_preview(hwnd);
                    }
                }
                if !bar.tracking_leave {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    if TrackMouseEvent(&mut tme).is_ok() {
                        bar.tracking_leave = true;
                    }
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                bar.tracking_leave = false;
                let _ = KillTimer(Some(hwnd), TIMER_PREVIEW);
                bar.preview.hide();
                if bar.tray_hover.take().is_some()
                    | bar.status_hover.take().is_some()
                    | std::mem::take(&mut bar.start_hover)
                {
                    bar.paint();
                }
                bar.set_hover(None);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                bar.preview.hide();
                let x = (lparam.0 & 0xFFFF) as i16 as f32 / bar.scale();
                if let Some(t) = bar.tray_hit(x) {
                    bar.tray_forward(t, WM_LBUTTONDOWN);
                } else if let Some(i) = bar.hit_test(x) {
                    bar.drag = Some(Drag {
                        idx: i,
                        press_x: x,
                        cur_x: x,
                        offset: x - bar.entry_left(i),
                        active: false,
                    });
                    SetCapture(hwnd);
                }
                LRESULT(0)
            }
            WM_LBUTTONDBLCLK => {
                // CS_DBLCLKS swallows the second LBUTTONDOWN of a fast pair;
                // tray icons get the dblclk, entries treat it as another press.
                bar.preview.hide();
                let x = (lparam.0 & 0xFFFF) as i16 as f32 / bar.scale();
                if let Some(t) = bar.tray_hit(x) {
                    bar.tray_forward(t, WM_LBUTTONDBLCLK);
                } else if bar.start_hit(x) {
                    bar.start_toggle(hwnd);
                } else if let Some(i) = bar.hit_test(x) {
                    bar.drag = Some(Drag {
                        idx: i,
                        press_x: x,
                        cur_x: x,
                        offset: x - bar.entry_left(i),
                        active: false,
                    });
                    SetCapture(hwnd);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                // Take the drag BEFORE ReleaseCapture: ReleaseCapture sends
                // WM_CAPTURECHANGED synchronously and that arm would consume
                // the drag first, eating the click.
                let pending = bar.drag.take();
                let _ = ReleaseCapture();
                let x = (lparam.0 & 0xFFFF) as i16 as f32 / bar.scale();
                if let Some(d) = pending {
                    if d.active {
                        save_pins(&bar.pins);
                        bar.paint();
                    } else {
                        bar.click(d.idx);
                    }
                } else if let Some(t) = bar.tray_hit(x) {
                    bar.tray_forward(t, WM_LBUTTONUP);
                    // v4 apps act on NIN_SELECT, not the raw button pair —
                    // explorer's taskbar sends it after LBUTTONUP.
                    if bar.tray_icons.get(t).is_some_and(|i| i.version >= 4) {
                        bar.tray_forward(t, NIN_SELECT);
                    }
                } else if bar.start_hit(x) {
                    bar.start_toggle(hwnd);
                } else if let Some(s) = bar.status_hit(x) {
                    match bar.status_cells().get(s) {
                        Some(StatusCell::Ime) => crate::status::send_hangul_key(),
                        // Win10-style flyout panels; mute moved into the
                        // volume panel's speaker button.
                        Some(StatusCell::Vol) => {
                            bar.flyout_toggle(hwnd, crate::flyout::Kind::Volume)
                        }
                        Some(StatusCell::Net) => {
                            bar.flyout_toggle(hwnd, crate::flyout::Kind::Network)
                        }
                        Some(StatusCell::Bat) => {
                            bar.flyout_toggle(hwnd, crate::flyout::Kind::Battery)
                        }
                        None => {}
                    }
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                if bar.drag.take().is_some() {
                    save_pins(&bar.pins);
                    bar.paint();
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                bar.preview.hide();
                let x = (lparam.0 & 0xFFFF) as i16 as f32 / bar.scale();
                if let Some(t) = bar.tray_hit(x) {
                    // Standard sequence the owner expects for its menu.
                    bar.tray_forward(t, WM_RBUTTONDOWN);
                    bar.tray_forward(t, WM_RBUTTONUP);
                    if bar.tray_icons.get(t).is_some_and(|i| i.version >= 4) {
                        bar.tray_forward(t, WM_CONTEXTMENU);
                    }
                } else if let Some(i) = bar.hit_test(x) {
                    bar.context_menu(i);
                } else if bar.status_hit(x).is_none() {
                    bar.bar_menu();
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                // Unlike the other mouse messages, wheel lParam is in SCREEN
                // coordinates.
                let mut pt = POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                let _ = ScreenToClient(hwnd, &mut pt);
                let x = pt.x as f32 / bar.scale();
                if let Some(s) = bar.status_hit(x) {
                    if bar.status_cells().get(s) == Some(&StatusCell::Vol) {
                        let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32 / 120.0;
                        bar.status.adjust_volume(delta);
                        bar.paint();
                    }
                }
                LRESULT(0)
            }
            WM_APPBAR => {
                if wparam.0 == ABN_POSCHANGED_ID {
                    bar.reposition();
                }
                LRESULT(0)
            }
            crate::winkey::WM_WINKEY => {
                crate::winkey::toggle_glint();
                LRESULT(0)
            }
            WM_DPICHANGED | WM_DISPLAYCHANGE => {
                bar.reposition();
                bar.rebuild_secondaries();
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

impl Bar {
    fn scale(&self) -> f32 {
        self.renderer.dpi / 96.0
    }

    fn on_shellhook(&mut self, code: usize, lparam: isize) {
        const HSHELL_FLASH_FULL: usize = 0x8006;
        match code & 0x7FFF {
            1 | 2 => self.refresh(), // HSHELL_WINDOWCREATED / DESTROYED
            4 => {
                // HSHELL_WINDOWACTIVATED / RUDEAPPACTIVATED
                self.active = unsafe { GetForegroundWindow() };
                self.sync_secondaries();
                for e in &mut self.entries {
                    if e.hwnd == Some(self.active) {
                        e.flash = false;
                    }
                }
                self.ensure_anim_timer();
                self.paint();
            }
            6 => {
                if code == HSHELL_FLASH_FULL {
                    let flashed = HWND(lparam as *mut _);
                    for e in &mut self.entries {
                        if e.hwnd == Some(flashed) {
                            e.flash = true;
                        }
                    }
                    self.paint();
                    self.sync_secondaries();
                } else {
                    self.refresh(); // HSHELL_REDRAW: titles changed
                }
            }
            _ => {}
        }
    }

    fn resync(&mut self) {
        let live = enumerate_taskbar_windows(self.hwnd);
        let mut fresh: Vec<isize> = live.iter().map(|h| h.0 as isize).collect();
        fresh.sort_unstable();
        let mut current: Vec<isize> = self.running_order.clone();
        current.sort_unstable();
        if current != fresh {
            self.refresh();
        }
        // Tray icons whose owner died without NIM_DELETE (crashed apps).
        let before = self.tray_icons.len();
        self.tray_icons.retain(|t| unsafe { IsWindow(Some(t.owner)).as_bool() });
        if self.tray_icons.len() != before {
            self.paint();
        }
    }

    /// Called from the Shell_TrayWnd wndproc (same thread).
    pub fn on_tray_event(&mut self, ev: crate::tray::TrayEvent) {
        use crate::tray::*;
        match ev.message {
            NIM_ADD | NIM_MODIFY => {
                let idx = self
                    .tray_icons
                    .iter()
                    .position(|t| t.owner == ev.owner && t.uid == ev.uid)
                    .unwrap_or_else(|| {
                        // NIM_MODIFY before ADD happens in the wild; upsert.
                        self.tray_icons.push(TrayIcon {
                            owner: ev.owner,
                            uid: ev.uid,
                            callback: 0,
                            version: 0,
                            bitmap: None,
                            tip: String::new(),
                            hidden: false,
                        });
                        self.tray_icons.len() - 1
                    });
                let hicon = ev.hicon;
                let flags = ev.flags;
                if flags & NIF_ICON != 0 {
                    let bmp = crate::icons::hicon_bitmap(
                        &self.renderer.dc,
                        windows::Win32::UI::WindowsAndMessaging::HICON(hicon as *mut _),
                    );
                    self.tray_icons[idx].bitmap = bmp;
                }
                let t = &mut self.tray_icons[idx];
                if flags & NIF_MESSAGE != 0 {
                    t.callback = ev.callback;
                }
                if flags & NIF_TIP != 0 {
                    t.tip = ev.tip;
                }
                if flags & NIF_STATE != 0 && ev.state_mask & NIS_HIDDEN != 0 {
                    t.hidden = ev.state & NIS_HIDDEN != 0;
                }
                self.apply_overflow();
                self.paint();
            }
            NIM_DELETE => {
                self.tray_icons
                    .retain(|t| !(t.owner == ev.owner && t.uid == ev.uid));
                self.apply_overflow();
                self.paint();
            }
            NIM_SETVERSION => {
                if let Some(t) = self
                    .tray_icons
                    .iter_mut()
                    .find(|t| t.owner == ev.owner && t.uid == ev.uid)
                {
                    t.version = ev.version;
                }
            }
            _ => {}
        }
    }

    fn tray_visible(&self) -> Vec<usize> {
        (0..self.tray_icons.len())
            .filter(|i| !self.tray_icons[*i].hidden)
            .collect()
    }

    fn status_cells(&self) -> Vec<StatusCell> {
        let mut cells = Vec::with_capacity(4);
        if self.status.ime_hangul.is_some() {
            cells.push(StatusCell::Ime);
        }
        cells.push(StatusCell::Net);
        cells.push(StatusCell::Vol);
        if self.status.battery.is_some() {
            cells.push(StatusCell::Bat);
        }
        cells
    }

    /// Logical x of the status cluster's left edge (right of it: clock).
    fn status_left(&self) -> f32 {
        self.width - CLOCK_W - (self.status_cells().len() as f32 * STATUS_CELL_W) - 2.0
    }

    /// x → index into status_cells().
    fn status_hit(&self, x: f32) -> Option<usize> {
        let cells = self.status_cells();
        let left = self.status_left();
        if x < left || x >= left + cells.len() as f32 * STATUS_CELL_W {
            return None;
        }
        Some(((x - left) / STATUS_CELL_W) as usize)
    }

    /// Logical x of the tray area's left edge.
    fn tray_left(&self) -> f32 {
        self.status_left() - (self.tray_visible().len() as f32 * TRAY_CELL_W) - 4.0
    }

    /// x → index into tray_icons.
    fn tray_hit(&self, x: f32) -> Option<usize> {
        let visible = self.tray_visible();
        if visible.is_empty() {
            return None;
        }
        let left = self.tray_left();
        if x < left || x >= left + visible.len() as f32 * TRAY_CELL_W {
            return None;
        }
        let cell = ((x - left) / TRAY_CELL_W) as usize;
        visible.get(cell).copied()
    }

    /// Version-aware Shell_NotifyIcon callback: v4 packs coords in wParam and
    /// (event, uid) in lParam; v0-v3 use (uid, event).
    fn tray_forward(&self, idx: usize, event: u32) {
        let Some(t) = self.tray_icons.get(idx) else { return };
        if t.callback == 0 {
            return;
        }
        unsafe {
            // We are WS_EX_NOACTIVATE, so the click never made anyone
            // foreground — hand our received-last-input right to the owner or
            // its ShowWindow/SetForegroundWindow response gets denied.
            let mut pid = 0u32;
            let _ = GetWindowThreadProcessId(t.owner, Some(&mut pid));
            if pid != 0 {
                let _ = AllowSetForegroundWindow(pid);
            }
            if event == WM_RBUTTONDOWN {
                // Owner's popup menu must be able to take foreground.
                let _ = SetForegroundWindow(t.owner);
            }
            let (wparam, lparam) = if t.version >= 4 {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                (
                    WPARAM((((pt.y as u32 as usize) & 0xFFFF) << 16) | (pt.x as u32 as usize & 0xFFFF)),
                    LPARAM((((t.uid as isize) & 0xFFFF) << 16) | (event as isize & 0xFFFF)),
                )
            } else {
                (WPARAM(t.uid as usize), LPARAM(event as isize))
            };
            let _ = SendNotifyMessageW(t.owner, t.callback, wparam, lparam);
        }
    }

    /// Rebuild entries with stable ordering: pins first (pin order), then
    /// running windows in first-seen order.
    fn refresh(&mut self) {
        let live = enumerate_taskbar_windows(self.hwnd);
        let live_keys: Vec<isize> = live.iter().map(|h| h.0 as isize).collect();

        // first-seen order: drop dead, append new at the end (enum order).
        self.running_order.retain(|k| live_keys.contains(k));
        for k in &live_keys {
            if !self.running_order.contains(k) {
                self.running_order.push(*k);
            }
        }

        let mut exe_of: HashMap<isize, Option<String>> = HashMap::new();
        for h in &live {
            exe_of.insert(h.0 as isize, window_exe(*h));
        }

        let mut consumed: Vec<isize> = Vec::new();
        let mut entries: Vec<Entry> = Vec::new();
        let mut entry_keys: Vec<i64> = Vec::new();

        // Pin section: every running window of a pinned exe sits in the pin's
        // slot region; no window → icon-only launcher.
        let pins = self.pins.clone();
        for (pin_idx, pin) in pins.iter().enumerate() {
            let windows: Vec<isize> = self
                .running_order
                .iter()
                .filter(|k| exe_of.get(*k).and_then(|e| e.as_deref()) == Some(pin.as_str()))
                .copied()
                .collect();
            if windows.is_empty() {
                self.exe_icon_cache
                    .entry(pin.clone())
                    .or_insert_with(|| icons::exe_icon(&self.renderer.dc, pin));
                let e = Entry {
                    hwnd: None,
                    exe: Some(pin.clone()),
                    title: Vec::new(),
                    width: LAUNCHER_W,
                    full_width: LAUNCHER_W,
                    flash: false,
                    pinned: true,
                };
                entry_keys.push(e.key(pin_idx));
                entries.push(e);
            } else {
                for k in windows {
                    consumed.push(k);
                    let e = self.make_window_entry(HWND(k as *mut _), Some(pin.clone()), true);
                    entry_keys.push(e.key(pin_idx));
                    entries.push(e);
                }
            }
        }

        // Running section, first-seen order.
        let order = self.running_order.clone();
        for k in order {
            if consumed.contains(&k) {
                continue;
            }
            let exe = exe_of.get(&k).cloned().flatten();
            let e = self.make_window_entry(HWND(k as *mut _), exe, false);
            entry_keys.push(e.key(0));
            entries.push(e);
        }

        self.icon_cache.retain(|k, _| live_keys.contains(k));
        self.anims.retain(|k, _| entry_keys.contains(k));
        self.entries = entries;
        self.entry_keys = entry_keys;
        self.apply_overflow();
        self.active = unsafe { GetForegroundWindow() };
        self.ensure_anim_timer();
        self.paint();
        self.sync_secondaries();
    }

    /// Push the current window-button set to every secondary bar.
    fn sync_secondaries(&mut self) {
        if self.secondaries.is_empty() {
            return;
        }
        let mirror = |bar: &Bar| -> Vec<crate::secondary::Mirror> {
            bar.entries
                .iter()
                .filter_map(|e| {
                    let h = e.hwnd?;
                    Some(crate::secondary::Mirror {
                        hwnd: h.0 as isize,
                        active: h == bar.active,
                        flash: e.flash,
                    })
                })
                .collect()
        };
        for i in 0..self.secondaries.len() {
            let m = mirror(self);
            self.secondaries[i].sync(m);
        }
    }

    /// Tear down and re-create the per-monitor bars from the current display
    /// set. WM_DISPLAYCHANGE lands here — the Duo's bottom panel docking on
    /// or off is just this list changing.
    fn rebuild_secondaries(&mut self) {
        self.secondaries.clear();
        for (mon, primary) in crate::secondary::monitors() {
            if primary {
                continue;
            }
            match crate::secondary::Secondary::new(mon) {
                Ok(sec) => self.secondaries.push(sec),
                Err(e) => eprintln!("glide-shell: secondary bar failed: {e}"),
            }
        }
        self.sync_secondaries();
    }

    fn make_window_entry(&mut self, h: HWND, exe: Option<String>, pinned: bool) -> Entry {
        let key = h.0 as isize;
        let mut title = [0u16; 256];
        let n = unsafe { GetWindowTextW(h, &mut title) } as usize;
        let title: Vec<u16> = title[..n].to_vec();
        let text_w = self
            .renderer
            .text_width(&title, &self.renderer.fmt_title, theme::BUTTON_MAX_W);
        let width = (10.0 + 20.0 + 8.0 + text_w + 12.0).min(theme::BUTTON_MAX_W);
        self.icon_cache
            .entry(key)
            .or_insert_with(|| icons::window_icon(&self.renderer.dc, h));
        let flash = self.entries.iter().any(|e| e.hwnd == Some(h) && e.flash);
        Entry { hwnd: Some(h), exe, title, width, full_width: width, flash, pinned }
    }

    /// Shrink window buttons evenly when they would run into the tray;
    /// launcher slots keep their fixed width.
    fn apply_overflow(&mut self) {
        for e in &mut self.entries {
            e.width = e.full_width;
        }
        let avail = self.tray_left() - ENTRY_X0 - 4.0;
        let total: f32 = self.entries.iter().map(|e| e.width + 4.0).sum();
        if total <= avail {
            return;
        }
        let fixed: f32 = self
            .entries
            .iter()
            .filter(|e| e.hwnd.is_none())
            .map(|e| e.width + 4.0)
            .sum();
        let n = self.entries.iter().filter(|e| e.hwnd.is_some()).count();
        if n == 0 {
            return;
        }
        let cap = ((avail - fixed) / n as f32 - 4.0).clamp(BUTTON_MIN_W, theme::BUTTON_MAX_W);
        for e in self.entries.iter_mut().filter(|e| e.hwnd.is_some()) {
            e.width = e.full_width.min(cap);
        }
    }

    /// Toggle the start menu from its bar button; closes the other popups.
    fn start_toggle(&mut self, hwnd: HWND) {
        unsafe {
            let _ = KillTimer(Some(hwnd), TIMER_PREVIEW);
        }
        self.preview.hide();
        self.flyout.hide();
        if self.start.open {
            self.start.hide();
        } else {
            let mut rect = RECT::default();
            unsafe {
                let _ = GetWindowRect(self.hwnd, &mut rect);
            }
            self.start.show(rect);
        }
        self.paint();
    }

    /// Toggle a status-cell flyout: same cell closes, another cell switches.
    fn flyout_toggle(&mut self, hwnd: HWND, kind: crate::flyout::Kind) {
        unsafe {
            let _ = KillTimer(Some(hwnd), TIMER_PREVIEW);
        }
        self.preview.hide();
        self.start.hide();
        if self.flyout.kind == Some(kind) {
            self.flyout.hide();
        } else {
            let mut rect = RECT::default();
            unsafe {
                let _ = GetWindowRect(self.hwnd, &mut rect);
            }
            self.flyout.open(kind, rect);
        }
    }

    /// Hover settled or moved: open/retarget/close the preview popup.
    fn schedule_preview(&mut self, hwnd: HWND) {
        // No tooltips while a flyout panel is up — they'd fight for the same
        // spot above the status cluster.
        if self.flyout.kind.is_some() {
            return;
        }
        unsafe {
            if self.hover.is_none() && self.tray_hover.is_none() && self.status_hover.is_none() {
                let _ = KillTimer(Some(hwnd), TIMER_PREVIEW);
                self.preview.hide();
            } else if self.preview.current.is_some() {
                // Popup already open: switch targets without the delay,
                // explorer-style.
                self.preview_fire();
            } else {
                SetTimer(Some(hwnd), TIMER_PREVIEW, PREVIEW_DELAY_MS, None);
            }
        }
    }

    fn preview_fire(&mut self) {
        use crate::preview::Target;
        if self.drag.as_ref().is_some_and(|d| d.active) {
            return;
        }
        let mut rect = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rect);
        }
        let scale = self.scale();
        let anchor = |cx: f32| rect.left + (cx * scale).round() as i32;

        if let Some(i) = self.hover {
            if i >= self.entries.len() {
                return;
            }
            let (hwnd_opt, exe_opt, title, center) = {
                let e = &self.entries[i];
                (
                    e.hwnd,
                    e.exe.clone(),
                    e.title.clone(),
                    self.entry_left(i) + e.width / 2.0,
                )
            };
            match hwnd_opt {
                Some(h) => {
                    self.preview.show_window(
                        Target::Window(h.0 as isize),
                        h,
                        &title,
                        anchor(center),
                        rect.top,
                    );
                }
                None => {
                    if let Some(exe) = exe_opt {
                        let name = exe.rsplit('\\').next().unwrap_or(&exe);
                        let t: Vec<u16> = name.encode_utf16().collect();
                        self.preview.show_tip(Target::Launcher(i), &t, anchor(center), rect.top);
                    }
                }
            }
        } else if let Some(ti) = self.tray_hover {
            let Some(icon) = self.tray_icons.get(ti) else { return };
            if icon.tip.is_empty() {
                return;
            }
            let visible = self.tray_visible();
            let Some(cell) = visible.iter().position(|v| *v == ti) else { return };
            let center = self.tray_left() + cell as f32 * TRAY_CELL_W + TRAY_CELL_W / 2.0;
            let tip: Vec<u16> = icon.tip.encode_utf16().collect();
            self.preview.show_tip(Target::Tray(ti), &tip, anchor(center), rect.top);
        } else if let Some(s) = self.status_hover {
            let cells = self.status_cells();
            let Some(cell) = cells.get(s) else { return };
            let text = match cell {
                StatusCell::Ime => match self.status.ime_hangul {
                    Some(true) => "한글 입력".to_string(),
                    _ => "영문 입력".to_string(),
                },
                StatusCell::Net => {
                    if self.status.net_connected {
                        "인터넷 연결됨".to_string()
                    } else {
                        "네트워크 연결 안 됨".to_string()
                    }
                }
                StatusCell::Vol => match self.status.volume {
                    Some((_, true)) => "음소거".to_string(),
                    Some((v, false)) => format!("볼륨 {:.0}%", v * 100.0),
                    None => "오디오 장치 없음".to_string(),
                },
                StatusCell::Bat => match self.status.battery {
                    Some((p, true)) => format!("배터리 {p}% · 전원 연결됨"),
                    Some((p, false)) => format!("배터리 {p}%"),
                    None => return,
                },
            };
            let t: Vec<u16> = text.encode_utf16().collect();
            let center = self.status_left() + s as f32 * STATUS_CELL_W + STATUS_CELL_W / 2.0;
            self.preview.show_tip(Target::Status(s), &t, anchor(center), rect.top);
        }
    }

    fn start_hit(&self, x: f32) -> bool {
        x >= START_X - 4.0 && x < ENTRY_X0 - 2.0
    }

    fn hit_test(&self, x: f32) -> Option<usize> {
        let mut cx = ENTRY_X0;
        for (i, e) in self.entries.iter().enumerate() {
            if x >= cx && x < cx + e.width {
                return Some(i);
            }
            cx += e.width + 4.0;
        }
        None
    }

    fn entry_left(&self, i: usize) -> f32 {
        ENTRY_X0 + self.entries[..i].iter().map(|e| e.width + 4.0).sum::<f32>()
    }

    fn drag_move(&mut self, x: f32) {
        let Some(d) = &mut self.drag else { return };
        d.cur_x = x;
        if !d.active && (x - d.press_x).abs() > 4.0 {
            d.active = true;
        }
        if !d.active {
            return;
        }
        // Swap with a neighbor when the dragged button's center crosses the
        // neighbor's center. One swap per event; sections don't mix.
        let idx = d.idx;
        let center = d.cur_x - d.offset + self.entries[idx].width / 2.0;
        if idx > 0 && self.entries[idx - 1].pinned == self.entries[idx].pinned {
            let n_center = self.entry_left(idx - 1) + self.entries[idx - 1].width / 2.0;
            if center < n_center {
                self.swap_entries(idx - 1, idx);
                if let Some(d) = &mut self.drag {
                    d.idx = idx - 1;
                }
                self.paint();
                return;
            }
        }
        if idx + 1 < self.entries.len() && self.entries[idx + 1].pinned == self.entries[idx].pinned
        {
            let n_center = self.entry_left(idx + 1) + self.entries[idx + 1].width / 2.0;
            if center > n_center {
                self.swap_entries(idx, idx + 1);
                if let Some(d) = &mut self.drag {
                    d.idx = idx + 1;
                }
            }
        }
        self.paint();
    }

    /// Swap two adjacent entries and mirror the move into the backing order
    /// (pins for the pin section, running_order for windows) so the next
    /// refresh() reproduces the dropped arrangement.
    fn swap_entries(&mut self, a: usize, b: usize) {
        let (ea, eb) = (&self.entries[a], &self.entries[b]);
        if ea.pinned && eb.pinned {
            let (xa, xb) = (ea.exe.clone(), eb.exe.clone());
            if xa != xb {
                let pa = xa.and_then(|x| self.pins.iter().position(|p| *p == x));
                let pb = xb.and_then(|x| self.pins.iter().position(|p| *p == x));
                if let (Some(pa), Some(pb)) = (pa, pb) {
                    self.pins.swap(pa, pb);
                }
            } else if let (Some(ha), Some(hb)) = (ea.hwnd, eb.hwnd) {
                self.swap_running(ha, hb);
            }
        } else if let (Some(ha), Some(hb)) = (ea.hwnd, eb.hwnd) {
            self.swap_running(ha, hb);
        }
        self.entries.swap(a, b);
        self.entry_keys.swap(a, b);
    }

    fn swap_running(&mut self, ha: HWND, hb: HWND) {
        let pa = self.running_order.iter().position(|k| *k == ha.0 as isize);
        let pb = self.running_order.iter().position(|k| *k == hb.0 as isize);
        if let (Some(pa), Some(pb)) = (pa, pb) {
            self.running_order.swap(pa, pb);
        }
    }

    fn set_hover(&mut self, h: Option<usize>) {
        if self.hover != h {
            self.hover = h;
            self.ensure_anim_timer();
        }
    }

    fn click(&mut self, i: usize) {
        let Some(e) = self.entries.get(i) else { return };
        unsafe {
            match e.hwnd {
                Some(h) => {
                    if h == GetForegroundWindow() {
                        let _ = ShowWindow(h, SW_MINIMIZE);
                    } else {
                        if IsIconic(h).as_bool() {
                            let _ = ShowWindow(h, SW_RESTORE);
                        }
                        force_foreground(h);
                    }
                }
                None => {
                    // Launcher slot: start the pinned exe.
                    if let Some(exe) = &e.exe {
                        let wide: Vec<u16> =
                            exe.encode_utf16().chain(std::iter::once(0)).collect();
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
            }
        }
    }

    fn context_menu(&mut self, i: usize) {
        let Some(e) = self.entries.get(i) else { return };
        let exe = e.exe.clone();
        let hwnd_opt = e.hwnd;
        let pinned = exe.as_deref().is_some_and(|x| self.pins.iter().any(|p| p == x));
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let add = |id: usize, label: &str| {
                let wide: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
                let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(wide.as_ptr()));
            };
            if exe.is_some() {
                if pinned {
                    add(MENU_UNPIN, "고정 해제");
                } else {
                    add(MENU_PIN, "작업 표시줄에 고정");
                }
            }
            if hwnd_opt.is_some() {
                add(MENU_CLOSE, "창 닫기");
            }
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // Menu on a NOACTIVATE window: same trap as tray menus — bring
            // ourselves foreground first or the menu never dismisses.
            let _ = SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_BOTTOMALIGN,
                pt.x,
                pt.y,
                Some(0),
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            match cmd.0 as usize {
                MENU_PIN => {
                    if let Some(x) = exe {
                        if !self.pins.contains(&x) {
                            self.pins.push(x);
                            save_pins(&self.pins);
                            self.refresh();
                        }
                    }
                }
                MENU_UNPIN => {
                    if let Some(x) = exe {
                        self.pins.retain(|p| p != &x);
                        save_pins(&self.pins);
                        self.refresh();
                    }
                }
                MENU_CLOSE => {
                    if let Some(h) = hwnd_opt {
                        let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                }
                _ => {}
            }
        }
    }

    /// Empty-bar right-click: shell housekeeping (the stock bar's own menu
    /// is Settings-only on Win11; ours earns its keep while dogfooding).
    fn bar_menu(&mut self) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let add = |id: usize, label: &str| {
                let wide: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
                let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(wide.as_ptr()));
            };
            add(MENU_TASKMGR, "작업 관리자");
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            add(MENU_RESTART, "glide-shell 다시 시작");
            add(MENU_QUIT, "glide-shell 종료");
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_BOTTOMALIGN,
                pt.x,
                pt.y,
                Some(0),
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            match cmd.0 as usize {
                MENU_TASKMGR => {
                    windows::Win32::UI::Shell::ShellExecuteW(
                        None,
                        w!("open"),
                        w!("taskmgr.exe"),
                        None,
                        None,
                        SW_SHOWNORMAL,
                    );
                }
                MENU_RESTART => {
                    // New instance first; the appbar slots resolve via
                    // ABN_POSCHANGED once this one exits.
                    if let Ok(exe) = std::env::current_exe() {
                        use std::os::windows::process::CommandExt;
                        let _ = std::process::Command::new(exe)
                            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                            .spawn();
                    }
                    PostQuitMessage(0);
                }
                MENU_QUIT => PostQuitMessage(0),
                _ => {}
            }
        }
    }

    fn reposition(&mut self) {
        unsafe {
            let dpi = GetDpiForWindow(self.hwnd) as f32;
            let bar_h = (theme::BAR_HEIGHT * dpi / 96.0).round() as i32;
            let rect = {
                let mon = primary_monitor_rect();
                let mut abd = APPBARDATA {
                    cbSize: std::mem::size_of::<APPBARDATA>() as u32,
                    hWnd: self.hwnd,
                    uCallbackMessage: WM_APPBAR,
                    uEdge: ABE_BOTTOM,
                    rc: RECT {
                        left: mon.left,
                        right: mon.right,
                        top: mon.bottom - bar_h,
                        bottom: mon.bottom,
                    },
                    ..Default::default()
                };
                SHAppBarMessage(ABM_QUERYPOS, &mut abd);
                abd.rc.top = abd.rc.bottom - bar_h;
                SHAppBarMessage(ABM_SETPOS, &mut abd);
                abd.rc
            };
            let (w_px, h_px) = (rect.right - rect.left, rect.bottom - rect.top);
            let _ = MoveWindow(self.hwnd, rect.left, rect.top, w_px, h_px, true);
            let _ = self.renderer.resize(w_px as u32, h_px as u32, dpi);
            self.width = w_px as f32 / (dpi / 96.0);
            self.apply_overflow();
            self.paint();
        }
    }

    fn ensure_anim_timer(&mut self) {
        if !self.anim_timer {
            self.anim_timer = true;
            self.last_tick = Instant::now();
            unsafe { SetTimer(Some(self.hwnd), TIMER_ANIM, 16, None) };
        }
    }

    fn tick_anims(&mut self) {
        let dt = self.last_tick.elapsed().as_secs_f32() * 1000.0;
        self.last_tick = Instant::now();
        let step = (dt / theme::ANIM_MS).min(1.0);
        let mut settled = true;
        for (i, e) in self.entries.iter().enumerate() {
            let key = self.entry_keys[i];
            let a = self.anims.entry(key).or_default();
            let hover_target = if self.hover == Some(i) { 1.0 } else { 0.0 };
            let active_target = if e.hwnd == Some(self.active) { 1.0 } else { 0.0 };
            for (v, target) in [(&mut a.hover, hover_target), (&mut a.active, active_target)] {
                let d = target - *v;
                if d.abs() < 0.01 {
                    *v = target;
                } else {
                    *v += d * step * 2.2; // ease-out flavor: big steps far away
                    settled = false;
                }
            }
        }
        if settled {
            self.anim_timer = false;
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_ANIM);
            }
        }
        self.paint();
    }

    /// One button at the given layout left. `floating` = the dragged copy:
    /// solid backing so it reads as lifted above the row.
    fn draw_entry(&self, i: usize, cx: f32, floating: bool) {
        let r = &self.renderer;
        let bar_h = theme::BAR_HEIGHT;
        let e = &self.entries[i];
        let key = self.entry_keys[i];
        let a = self.anims.get(&key).copied().unwrap_or_default();
        let rect = D2D_RECT_F {
            left: cx,
            top: 5.0,
            right: cx + e.width,
            bottom: bar_h - 5.0,
        };
        unsafe {
            if floating {
                if let Ok(b) = r.brush(theme::rgba(23, 24, 28, 0.92)) {
                    r.dc.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect,
                            radiusX: theme::BUTTON_RADIUS,
                            radiusY: theme::BUTTON_RADIUS,
                        },
                        &b,
                    );
                }
            }

            // Fill: active wash + hover fade on top; floating gets a firm wash.
            let mut fill_a = theme::ACTIVE_FILL.a * a.active + theme::HOVER_FILL.a * a.hover;
            if floating {
                fill_a = fill_a.max(theme::HOVER_FILL.a);
            }
            if fill_a > 0.005 {
                if let Ok(b) = r.brush(theme::rgba(255, 255, 255, fill_a)) {
                    r.dc.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect,
                            radiusX: theme::BUTTON_RADIUS,
                            radiusY: theme::BUTTON_RADIUS,
                        },
                        &b,
                    );
                }
            }

            // Icon 20×20: left-aligned in window buttons, centered in
            // launcher slots. Launchers render dimmed.
            let icon_left = if e.hwnd.is_some() { cx + 10.0 } else { cx + (e.width - 20.0) / 2.0 };
            let icon_rect = D2D_RECT_F {
                left: icon_left,
                top: (bar_h - 20.0) / 2.0,
                right: icon_left + 20.0,
                bottom: (bar_h + 20.0) / 2.0,
            };
            let bmp = match e.hwnd {
                Some(h) => self.icon_cache.get(&(h.0 as isize)),
                None => e.exe.as_ref().and_then(|x| self.exe_icon_cache.get(x)),
            };
            let opacity = if e.hwnd.is_some() { 1.0 } else { 0.55 + 0.45 * a.hover };
            if let Some(Some(bmp)) = bmp {
                r.dc.DrawBitmap(
                    bmp,
                    Some(&icon_rect),
                    opacity,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    None,
                    None,
                );
            } else if let Ok(b) = r.brush(theme::with_alpha(theme::ACCENT, 0.5 * opacity)) {
                r.dc.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT { rect: icon_rect, radiusX: 4.0, radiusY: 4.0 },
                    &b,
                );
            }

            // Title (window buttons only), clipped to the button.
            if e.hwnd.is_some() {
                let text_color = if e.flash { theme::FLASH } else { theme::TEXT };
                if let Ok(b) = r.brush(text_color) {
                    let text_rect = D2D_RECT_F {
                        left: cx + 38.0,
                        top: 0.0,
                        right: cx + e.width - 10.0,
                        bottom: bar_h,
                    };
                    r.dc.DrawText(
                        &e.title,
                        &r.fmt_title,
                        &text_rect,
                        &b,
                        D2D1_DRAW_TEXT_OPTIONS_CLIP,
                        DWRITE_MEASURING_MODE_NATURAL,
                    );
                }
            }

            // Active underline: grows from the center (glide accent-pill
            // gesture), amber when flashing. Window buttons only.
            let grow = a.active;
            if e.hwnd.is_some() && (grow > 0.01 || e.flash) {
                let full = e.width - 28.0;
                let w = if e.flash { full } else { 8.0 + (full - 8.0) * grow };
                let mid = cx + e.width / 2.0;
                let color = if e.flash { theme::FLASH } else { theme::ACCENT };
                if let Ok(b) = r.brush(color) {
                    r.dc.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: D2D_RECT_F {
                                left: mid - w / 2.0,
                                top: bar_h - theme::UNDERLINE_H - 2.0,
                                right: mid + w / 2.0,
                                bottom: bar_h - 2.0,
                            },
                            radiusX: theme::UNDERLINE_H / 2.0,
                            radiusY: theme::UNDERLINE_H / 2.0,
                        },
                        &b,
                    );
                }
            }
        }
    }

    /// Status cluster between tray and clock: [한/A][net][vol][batt].
    fn draw_status(&self) {
        let r = &self.renderer;
        let bar_h = theme::BAR_HEIGHT;
        let cells = self.status_cells();
        let left = self.status_left();
        unsafe {
            for (i, cell) in cells.iter().enumerate() {
                let cx = left + i as f32 * STATUS_CELL_W;
                let rect = D2D_RECT_F {
                    left: cx,
                    top: 0.0,
                    right: cx + STATUS_CELL_W,
                    bottom: bar_h,
                };
                if self.status_hover == Some(i) {
                    if let Ok(b) = r.brush(theme::rgba(255, 255, 255, theme::HOVER_FILL.a)) {
                        r.dc.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: D2D_RECT_F {
                                    left: cx,
                                    top: 7.0,
                                    right: cx + STATUS_CELL_W,
                                    bottom: bar_h - 7.0,
                                },
                                radiusX: 4.0,
                                radiusY: 4.0,
                            },
                            &b,
                        );
                    }
                }
                let draw_glyph = |ch: u16, color| {
                    if let Ok(b) = r.brush(color) {
                        r.dc.DrawText(
                            &[ch],
                            &r.fmt_glyph,
                            &rect,
                            &b,
                            D2D1_DRAW_TEXT_OPTIONS_CLIP,
                            DWRITE_MEASURING_MODE_NATURAL,
                        );
                    }
                };
                match cell {
                    StatusCell::Ime => {
                        let hangul = self.status.ime_hangul == Some(true);
                        let (s, color) = if hangul {
                            ("한", theme::ACCENT)
                        } else {
                            ("A", theme::TEXT)
                        };
                        let utf16: Vec<u16> = s.encode_utf16().collect();
                        if let Ok(b) = r.brush(color) {
                            r.dc.DrawText(
                                &utf16,
                                &r.fmt_status,
                                &rect,
                                &b,
                                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                                DWRITE_MEASURING_MODE_NATURAL,
                            );
                        }
                    }
                    StatusCell::Net => {
                        if self.status.net_connected {
                            draw_glyph(crate::status::GLYPH_WIFI, theme::TEXT);
                        } else {
                            draw_glyph(
                                crate::status::GLYPH_WIFI,
                                theme::with_alpha(theme::TEXT_DIM, 0.55),
                            );
                            if let Ok(b) = r.brush(theme::FLASH) {
                                r.dc.DrawLine(
                                    Vector2 { X: cx + 6.0, Y: bar_h - 12.0 },
                                    Vector2 { X: cx + STATUS_CELL_W - 6.0, Y: 12.0 },
                                    &b,
                                    1.5,
                                    None,
                                );
                            }
                        }
                    }
                    StatusCell::Vol => {
                        let dim = matches!(self.status.volume, None | Some((_, true)));
                        let color = if dim { theme::TEXT_DIM } else { theme::TEXT };
                        draw_glyph(crate::status::volume_glyph(self.status.volume), color);
                    }
                    StatusCell::Bat => {
                        if let Some((pct, on_ac)) = self.status.battery {
                            let color = if on_ac {
                                theme::ACCENT
                            } else if pct <= 20 {
                                theme::FLASH
                            } else {
                                theme::TEXT
                            };
                            draw_glyph(crate::status::battery_glyph(pct, on_ac), color);
                        }
                    }
                }
            }
        }
    }

    /// Win11-logo start button: four rounded squares, accent when engaged.
    fn draw_start(&self) {
        let r = &self.renderer;
        let engaged = self.start_hover || self.start.open;
        unsafe {
            if engaged {
                let fill = if self.start_hover { theme::HOVER_FILL } else { theme::ACTIVE_FILL };
                if let Ok(b) = r.brush(fill) {
                    r.dc.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: D2D_RECT_F {
                                left: START_X,
                                top: 4.0,
                                right: START_X + START_BTN_W,
                                bottom: theme::BAR_HEIGHT - 4.0,
                            },
                            radiusX: theme::BUTTON_RADIUS,
                            radiusY: theme::BUTTON_RADIUS,
                        },
                        &b,
                    );
                }
            }
            let color = if engaged { theme::ACCENT } else { theme::TEXT };
            if let Ok(b) = r.brush(color) {
                let cx = START_X + START_BTN_W / 2.0;
                let cy = theme::BAR_HEIGHT / 2.0;
                for (dx, dy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                    let (sx, sy) = (cx + dx * 4.5, cy + dy * 4.5);
                    r.dc.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: D2D_RECT_F {
                                left: sx - 3.5,
                                top: sy - 3.5,
                                right: sx + 3.5,
                                bottom: sy + 3.5,
                            },
                            radiusX: 1.5,
                            radiusY: 1.5,
                        },
                        &b,
                    );
                }
            }
        }
    }

    fn paint(&mut self) {
        let r = &self.renderer;
        let scale = self.scale();
        let bar_h = theme::BAR_HEIGHT;
        unsafe {
            r.dc.BeginDraw();
            r.dc.Clear(Some(&theme::BAR_BG));

            // Top hairline: 1px accent-tinted separation from content above.
            if let Ok(b) = r.brush(theme::with_alpha(theme::ACCENT, 0.25)) {
                r.dc.FillRectangle(
                    &D2D_RECT_F { left: 0.0, top: 0.0, right: self.width, bottom: 1.0 / scale },
                    &b,
                );
            }

            let dragging = self
                .drag
                .as_ref()
                .filter(|d| d.active)
                .map(|d| (d.idx, d.cur_x - d.offset));
            self.draw_start();
            let mut cx = ENTRY_X0;
            let mut pin_section_end: Option<f32> = None;
            for (i, e) in self.entries.iter().enumerate() {
                if e.pinned {
                    pin_section_end = Some(cx + e.width);
                }
                if dragging.map(|(di, _)| di) != Some(i) {
                    self.draw_entry(i, cx, false);
                }
                cx += e.width + 4.0;
            }

            // Hairline between pin section and running section.
            if let Some(px) = pin_section_end {
                if px + 4.0 < cx {
                    if let Ok(b) = r.brush(theme::with_alpha(theme::TEXT_DIM, 0.35)) {
                        r.dc.FillRectangle(
                            &D2D_RECT_F {
                                left: px + 1.5,
                                top: 12.0,
                                right: px + 2.5,
                                bottom: bar_h - 12.0,
                            },
                            &b,
                        );
                    }
                }
            }

            // Tray cells, right of the running section, left of the clock.
            {
                let visible = self.tray_visible();
                let left = self.tray_left();
                for (cell, idx) in visible.iter().enumerate() {
                    let t = &self.tray_icons[*idx];
                    let cx = left + cell as f32 * TRAY_CELL_W;
                    if self.tray_hover == Some(*idx) {
                        if let Ok(b) = r.brush(theme::rgba(255, 255, 255, theme::HOVER_FILL.a)) {
                            r.dc.FillRoundedRectangle(
                                &D2D1_ROUNDED_RECT {
                                    rect: D2D_RECT_F {
                                        left: cx,
                                        top: 7.0,
                                        right: cx + TRAY_CELL_W,
                                        bottom: bar_h - 7.0,
                                    },
                                    radiusX: 4.0,
                                    radiusY: 4.0,
                                },
                                &b,
                            );
                        }
                    }
                    let icon_rect = D2D_RECT_F {
                        left: cx + (TRAY_CELL_W - 16.0) / 2.0,
                        top: (bar_h - 16.0) / 2.0,
                        right: cx + (TRAY_CELL_W + 16.0) / 2.0,
                        bottom: (bar_h + 16.0) / 2.0,
                    };
                    if let Some(bmp) = &t.bitmap {
                        r.dc.DrawBitmap(
                            bmp,
                            Some(&icon_rect),
                            1.0,
                            D2D1_INTERPOLATION_MODE_LINEAR,
                            None,
                            None,
                        );
                    } else if let Ok(b) = r.brush(theme::with_alpha(theme::TEXT_DIM, 0.6)) {
                        r.dc.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT { rect: icon_rect, radiusX: 8.0, radiusY: 8.0 },
                            &b,
                        );
                    }
                }
            }

            self.draw_status();

            // Clock block, right-aligned: HH:MM over M/D (요일).
            let now = chrono::Local::now();
            let hhmm: Vec<u16> = now.format("%H:%M").to_string().encode_utf16().collect();
            let wd = ["월", "화", "수", "목", "금", "토", "일"]
                [chrono::Datelike::weekday(&now).num_days_from_monday() as usize];
            let date: Vec<u16> = format!(
                "{}/{} ({wd})",
                chrono::Datelike::month(&now),
                chrono::Datelike::day(&now)
            )
            .encode_utf16()
            .collect();
            let clock_left = self.width - CLOCK_W;
            if let Ok(b) = r.brush(theme::TEXT) {
                r.dc.DrawText(
                    &hhmm,
                    &r.fmt_clock,
                    &D2D_RECT_F { left: clock_left, top: 4.0, right: self.width - 8.0, bottom: 22.0 },
                    &b,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
            if let Ok(b) = r.brush(theme::TEXT_DIM) {
                r.dc.DrawText(
                    &date,
                    &r.fmt_date,
                    &D2D_RECT_F { left: clock_left, top: 22.0, right: self.width - 8.0, bottom: 37.0 },
                    &b,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }

            // Dragged button floats on top, following the cursor.
            if let Some((di, float_left)) = dragging {
                let w = self.entries[di].width;
                let left =
                    float_left.clamp(ENTRY_X0, (self.width - CLOCK_W - w - 4.0).max(ENTRY_X0));
                self.draw_entry(di, left, true);
            }

            let end = r.dc.EndDraw(None, None);
            if end.is_ok() {
                let _ = r.present();
            } else {
                // Device lost: rebuild the whole renderer next repaint.
                let dpi = r.dpi;
                let mut rc = RECT::default();
                let _ = GetClientRect(self.hwnd, &mut rc);
                if let Ok(new_r) =
                    Renderer::new(self.hwnd, (rc.right - rc.left) as u32, (rc.bottom - rc.top) as u32, dpi)
                {
                    self.renderer = new_r;
                    self.icon_cache.clear();
                    self.exe_icon_cache.clear();
                }
            }
        }
    }
}

/// EnumWindows filtered the way explorer's taskbar does it (§6.1).
fn enumerate_taskbar_windows(own: HWND) -> Vec<HWND> {
    struct Ctx {
        own: HWND,
        out: Vec<HWND>,
    }
    extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let ctx = &mut *(lparam.0 as *mut Ctx);
            if hwnd == ctx.own || !IsWindowVisible(hwnd).as_bool() {
                return true.into();
            }
            if GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
                return true.into();
            }
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOOLWINDOW.0 != 0 && ex & WS_EX_APPWINDOW.0 == 0 {
                return true.into();
            }
            let mut cloaked: u32 = 0;
            let _ = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut _ as *mut _,
                4,
            );
            // Minimized XAML/UWP windows (Win11 notepad included) report
            // cloaked — they still belong on the taskbar.
            if cloaked != 0 && !IsIconic(hwnd).as_bool() {
                return true.into();
            }
            let mut title = [0u16; 8];
            if GetWindowTextW(hwnd, &mut title) == 0 {
                return true.into();
            }
            let mut class = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut class) as usize;
            let class = String::from_utf16_lossy(&class[..n]);
            if class == "Progman" || class == "WorkerW" || class == "Shell_TrayWnd" {
                return true.into();
            }
            ctx.out.push(hwnd);
            true.into()
        }
    }
    let mut ctx = Ctx { own, out: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.out
}
