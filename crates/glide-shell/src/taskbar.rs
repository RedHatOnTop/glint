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
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_ROUNDED_RECT, ID2D1Bitmap1,
};
use windows::Win32::Graphics::DirectWrite::DWRITE_MEASURING_MODE_NATURAL;
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmGetWindowAttribute,
    DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, ValidateRect,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::Shell::{
    ABE_BOTTOM, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, APPBARDATA, SHAppBarMessage,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCWSTR, PWSTR, w};

use crate::render::Renderer;
use crate::{icons, theme};

const WM_APPBAR: u32 = WM_APP + 1;
// In windows-rs metadata this lives in Win32_UI_Controls; not worth the
// feature for one message id.
const WM_MOUSELEAVE: u32 = 0x02A3;
const ABN_POSCHANGED_ID: usize = 1;
const TIMER_CLOCK: usize = 1;
const TIMER_ANIM: usize = 2;
const TIMER_RESYNC: usize = 3;
const CLOCK_W: f32 = 84.0;
const LAUNCHER_W: f32 = 40.0;
const MENU_PIN: usize = 1;
const MENU_UNPIN: usize = 2;
const MENU_CLOSE: usize = 3;

struct Entry {
    /// None = pinned exe with no window (launcher slot).
    hwnd: Option<HWND>,
    exe: Option<String>,
    title: Vec<u16>,
    width: f32,
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

struct Bar {
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
    tracking_leave: bool,
    anim_timer: bool,
    last_tick: Instant,
    shellhook_msg: u32,
    width: f32, // logical
}

pub fn run() -> anyhow::Result<()> {
    unsafe {
        let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
        let class = w!("glide_shell_bar");
        let wc = WNDCLASSW {
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
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
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
            tracking_leave: false,
            anim_timer: false,
            last_tick: Instant::now(),
            shellhook_msg,
            width: w_px as f32 / scale,
        };
        bar.refresh();
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut bar as *mut Bar as isize);

        SetTimer(Some(hwnd), TIMER_CLOCK, 1000, None);
        SetTimer(Some(hwnd), TIMER_RESYNC, 2000, None);

        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        bar.paint();

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
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

/// ABM_NEW + QUERYPOS/SETPOS. The system pushes our rect above any existing
/// bottom appbar (explorer's taskbar included), so coexistence is automatic.
fn appbar_negotiate(hwnd: HWND, height_px: i32) -> RECT {
    unsafe {
        let mon = primary_monitor_rect();
        let mut abd = APPBARDATA {
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
        };
        SHAppBarMessage(ABM_NEW, &mut abd);
        SHAppBarMessage(ABM_QUERYPOS, &mut abd);
        abd.rc.top = abd.rc.bottom - height_px;
        SHAppBarMessage(ABM_SETPOS, &mut abd);
        abd.rc
    }
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
                    TIMER_CLOCK => bar.paint(),
                    TIMER_ANIM => bar.tick_anims(),
                    TIMER_RESYNC => bar.resync(),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32 / bar.scale();
                bar.set_hover(bar.hit_test(x));
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
                bar.set_hover(None);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32 / bar.scale();
                if let Some(i) = bar.hit_test(x) {
                    bar.click(i);
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32 / bar.scale();
                if let Some(i) = bar.hit_test(x) {
                    bar.context_menu(i);
                }
                LRESULT(0)
            }
            WM_APPBAR => {
                if wparam.0 == ABN_POSCHANGED_ID {
                    bar.reposition();
                }
                LRESULT(0)
            }
            WM_DPICHANGED | WM_DISPLAYCHANGE => {
                bar.reposition();
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
        self.active = unsafe { GetForegroundWindow() };
        self.ensure_anim_timer();
        self.paint();
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
        Entry { hwnd: Some(h), exe, title, width, flash, pinned }
    }

    fn hit_test(&self, x: f32) -> Option<usize> {
        let mut cx = 8.0;
        for (i, e) in self.entries.iter().enumerate() {
            if x >= cx && x < cx + e.width {
                return Some(i);
            }
            cx += e.width + 4.0;
        }
        None
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
                        let _ = SetForegroundWindow(h);
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

            let mut cx = 8.0;
            let mut pin_section_end: Option<f32> = None;
            for (i, e) in self.entries.iter().enumerate() {
                if e.pinned {
                    pin_section_end = Some(cx + e.width);
                }
                let key = self.entry_keys[i];
                let a = self.anims.get(&key).copied().unwrap_or_default();
                let rect = D2D_RECT_F {
                    left: cx,
                    top: 5.0,
                    right: cx + e.width,
                    bottom: bar_h - 5.0,
                };

                // Fill: active wash + hover fade on top.
                let mut fill_a = theme::ACTIVE_FILL.a * a.active;
                fill_a += theme::HOVER_FILL.a * a.hover;
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
            if cloaked != 0 {
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
