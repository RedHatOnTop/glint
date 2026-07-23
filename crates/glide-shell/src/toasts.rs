//! Toast cards (SHELL_DESIGN §6.7). UserNotificationListener passed
//! PASS-POLLING unpackaged on this box (spike 0721): NotificationChanged is
//! 0x80070490 without package identity, so a worker thread polls the action
//! center once a second and diffs `UserNotification.Id`; the first poll is a
//! silent baseline so the backlog doesn't replay as cards on shell start.
//! Cards stack bottom-right above the work area and never take activation
//! (WS_EX_NOACTIVATE + MA_NOACTIVATE) — a toast that steals focus from the
//! foreground app is worse than no toast. The listener cannot invoke another
//! app's toast actions (spike finding), so a card click = activate the source
//! app via `shell:AppsFolder\{AUMID}` + RemoveNotification(id); the X only
//! hides the card and leaves the notification in the action center.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::D2D1_ROUNDED_RECT;
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER,
    DWRITE_WORD_WRAPPING_NO_WRAP, IDWriteTextFormat,
};
use windows::Win32::Graphics::Direct2D::D2D1_DRAW_TEXT_OPTIONS_CLIP;
use windows::Win32::Graphics::Gdi::{ScreenToClient, ValidateRect};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::UI::Notifications::Management::{
    UserNotificationListener, UserNotificationListenerAccessStatus,
};
use windows::UI::Notifications::{KnownNotificationBindings, NotificationKinds};
use windows::core::{PCWSTR, w};

use crate::render::Renderer;
use crate::theme;

use std::sync::atomic::{AtomicBool, Ordering};

/// Live gate for own-notification toasts (settings: "알림 표시"). The worker
/// keeps polling either way; this only decides whether arrivals become cards.
static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_APP_TOAST: u32 = WM_APP + 11;
const TIMER_LIFE: usize = 1;
const TIMER_ANIM: usize = 2;

const CARD_W: f32 = 356.0;
const CARD_H: f32 = 96.0;
const GAP: f32 = 8.0;
const MARGIN: f32 = 12.0;
const MAX_CARDS: usize = 3;
const LIFE_SECS: u64 = 8;
/// Fade/slide duration in seconds.
const ANIM_SECS: f32 = 0.18;

/// Worker → UI: one freshly arrived toast.
struct Arrival {
    id: u32,
    app: String,
    texts: Vec<String>,
    aumid: Option<String>,
}

enum Cmd {
    Remove(u32),
}

struct Card {
    id: u32,
    app: Vec<u16>,
    title: Vec<u16>,
    body: Vec<u16>,
    aumid: Option<String>,
    born: Instant,
    /// 0→1 fade-in; runs back 1→0 when `closing`.
    state: f32,
    closing: bool,
    hovered: bool,
}

pub struct Toasts {
    hwnd: HWND,
    renderer: Renderer,
    fmt_app: IDWriteTextFormat,
    fmt_title: IDWriteTextFormat,
    fmt_body: IDWriteTextFormat,
    fmt_glyph: IDWriteTextFormat,
    scale: f32,
    h: f32,
    cards: Vec<Card>,
    arrivals: Receiver<Arrival>,
    cmds: Sender<Cmd>,
    last_anim: Instant,
    anim_timer: bool,
    tracking: bool,
}

impl Toasts {
    pub fn new(dpi: f32) -> anyhow::Result<Self> {
        unsafe {
            let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            let class = w!("glide_shell_toasts");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(toast_wndproc),
                hInstance: hinstance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            RegisterClassW(&wc); // 0 on re-register is fine
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
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
            let fmt_app = mk(family, 11.0, DWRITE_FONT_WEIGHT_NORMAL)
                .or_else(|_| mk(w!("Segoe UI"), 11.0, DWRITE_FONT_WEIGHT_NORMAL))?;
            fmt_app.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let fmt_title = mk(family, 12.5, DWRITE_FONT_WEIGHT_SEMI_BOLD)
                .or_else(|_| mk(w!("Segoe UI"), 12.5, DWRITE_FONT_WEIGHT_SEMI_BOLD))?;
            fmt_title.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            // Body wraps (up to two lines inside the card, then clipped).
            let fmt_body = mk(family, 12.0, DWRITE_FONT_WEIGHT_NORMAL)
                .or_else(|_| mk(w!("Segoe UI"), 12.0, DWRITE_FONT_WEIGHT_NORMAL))?;
            let fmt_glyph = mk(w!("Segoe Fluent Icons"), 10.0, DWRITE_FONT_WEIGHT_NORMAL)
                .or_else(|_| mk(w!("Segoe MDL2 Assets"), 10.0, DWRITE_FONT_WEIGHT_NORMAL))?;
            fmt_glyph.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_glyph.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            let (cmds, cmd_rx) = channel::<Cmd>();
            let (arrival_tx, arrivals) = channel::<Arrival>();
            let hwnd_raw = hwnd.0 as isize;
            std::thread::spawn(move || worker(cmd_rx, arrival_tx, hwnd_raw));

            Ok(Toasts {
                hwnd,
                renderer,
                fmt_app,
                fmt_title,
                fmt_body,
                fmt_glyph,
                scale: dpi / 96.0,
                h: 0.0,
                cards: Vec::new(),
                arrivals,
                cmds,
                last_anim: Instant::now(),
                anim_timer: false,
                tracking: false,
            })
        }
    }

    /// Late GWLP_USERDATA arm: `run()` calls this once the struct address is
    /// final; worker posts before this are drained on the next arrival.
    pub fn arm(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, self as *mut Toasts as isize);
        }
    }

    pub fn disarm(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
        }
    }

    fn on_arrivals(&mut self) {
        if !enabled() {
            // Toasts off: drain the channel so it never backs up, show nothing.
            while self.arrivals.try_recv().is_ok() {}
            return;
        }
        let mut new = false;
        while let Ok(a) = self.arrivals.try_recv() {
            let title = a.texts.first().cloned().unwrap_or_default();
            let body = a.texts.get(1..).unwrap_or(&[]).join(" ");
            self.cards.insert(
                0,
                Card {
                    id: a.id,
                    app: a.app.encode_utf16().collect(),
                    title: title.encode_utf16().collect(),
                    body: body.encode_utf16().collect(),
                    aumid: a.aumid,
                    born: Instant::now(),
                    state: 0.0,
                    closing: false,
                    hovered: false,
                },
            );
            new = true;
        }
        if !new {
            return;
        }
        // Overflow: fade the oldest out early instead of hard-dropping.
        for c in self.cards.iter_mut().skip(MAX_CARDS) {
            c.closing = true;
        }
        self.layout();
        self.ensure_anim();
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_LIFE, 1000, None);
        }
    }

    /// Size + place the window for the current card count (bottom-right of
    /// the work area, which the appbar already shrank past our own bar).
    fn layout(&mut self) {
        let n = self.cards.len().min(MAX_CARDS + 1);
        if n == 0 {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_LIFE);
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            return;
        }
        self.h = n as f32 * CARD_H + (n - 1) as f32 * GAP;
        let mut work = RECT::default();
        unsafe {
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut work as *mut _ as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
        }
        let wd = (CARD_W * self.scale).round() as i32;
        let hd = (self.h * self.scale).round() as i32;
        let m = (MARGIN * self.scale).round() as i32;
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                work.right - m - wd,
                work.bottom - m - hd,
                wd,
                hd,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        let _ = self
            .renderer
            .resize(wd as u32, hd as u32, self.scale * 96.0);
        self.paint();
    }

    fn card_rect(&self, i: usize) -> D2D_RECT_F {
        // cards[0] is newest, closest to the taskbar corner.
        let y = self.h - (i + 1) as f32 * CARD_H - i as f32 * GAP;
        rect(0.0, y, CARD_W, y + CARD_H)
    }

    fn close_rect(&self, i: usize) -> D2D_RECT_F {
        let rc = self.card_rect(i);
        rect(rc.right - 32.0, rc.top + 6.0, rc.right - 6.0, rc.top + 32.0)
    }

    fn card_at(&self, x: f32, y: f32) -> Option<usize> {
        (0..self.cards.len().min(MAX_CARDS + 1)).find(|&i| {
            let rc = self.card_rect(i);
            x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
        })
    }

    /// Drive fades; returns whether another animation frame is needed.
    fn step_anim(&mut self) -> bool {
        let dt = self.last_anim.elapsed().as_secs_f32().min(0.1);
        self.last_anim = Instant::now();
        let step = dt / ANIM_SECS;
        let mut busy = false;
        for c in &mut self.cards {
            let target = if c.closing { 0.0 } else { 1.0 };
            if c.state != target {
                c.state = if c.closing {
                    (c.state - step).max(0.0)
                } else {
                    (c.state + step).min(1.0)
                };
                busy = busy || c.state != target;
            }
        }
        let before = self.cards.len();
        self.cards.retain(|c| !(c.closing && c.state <= 0.0));
        if self.cards.len() != before {
            self.layout();
        } else {
            self.paint();
        }
        busy
    }

    fn ensure_anim(&mut self) {
        if !self.anim_timer {
            self.anim_timer = true;
            self.last_anim = Instant::now();
            unsafe {
                SetTimer(Some(self.hwnd), TIMER_ANIM, 16, None);
            }
        }
    }

    fn stop_anim(&mut self) {
        if self.anim_timer {
            self.anim_timer = false;
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_ANIM);
            }
        }
    }

    /// 1s lifetime tick: expire un-hovered cards.
    fn tick(&mut self) {
        let mut dirty = false;
        for c in &mut self.cards {
            if c.hovered {
                // Hover pauses the countdown Win-style.
                c.born = Instant::now();
            } else if !c.closing && c.born.elapsed().as_secs() >= LIFE_SECS {
                c.closing = true;
                dirty = true;
            }
        }
        if dirty {
            self.ensure_anim();
        }
    }

    fn click(&mut self, i: usize, x: f32, y: f32) {
        let xr = self.close_rect(i);
        let on_x = x >= xr.left && x < xr.right && y >= xr.top && y < xr.bottom;
        let Some(c) = self.cards.get_mut(i) else { return };
        c.closing = true;
        // X = hide the card only; body = activate the source app and clear
        // the notification from the action center.
        let target = (!on_x).then(|| (c.id, c.aumid.clone()));
        if let Some((id, aumid)) = target {
            if let Some(aumid) = aumid {
                let cmd: Vec<u16> = format!("shell:AppsFolder\\{aumid}")
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                unsafe {
                    ShellExecuteW(
                        None,
                        w!("open"),
                        PCWSTR(cmd.as_ptr()),
                        None,
                        None,
                        SW_SHOWNORMAL,
                    );
                }
            }
            let _ = self.cmds.send(Cmd::Remove(id));
        }
        self.ensure_anim();
    }

    fn paint(&mut self) {
        unsafe {
            let r = &self.renderer;
            r.dc.BeginDraw();
            // Window background stays clear; each card paints its own slab.
            r.dc.Clear(Some(&theme::rgba(0, 0, 0, 0.0)));
            for i in 0..self.cards.len().min(MAX_CARDS + 1) {
                self.paint_card(i);
            }
            let _ = r.dc.EndDraw(None, None);
            let _ = self.renderer.present();
        }
    }

    fn paint_card(&self, i: usize) {
        let c = &self.cards[i];
        let e = 1.0 - (1.0 - c.state) * (1.0 - c.state) * (1.0 - c.state);
        let a = e;
        let dx = (1.0 - e) * 26.0;
        let rc0 = self.card_rect(i);
        let rc = rect(rc0.left + dx, rc0.top, rc0.right + dx, rc0.bottom);

        let fade = |col: D2D1_COLOR_F| theme::with_alpha(col, col.a * a);
        self.fill_round(rc, 8.0, fade(theme::rgba(30, 31, 37, 0.97)));
        unsafe {
            if let Ok(b) = self.renderer.brush(fade(theme::rgba(255, 255, 255, 0.09))) {
                self.renderer.dc.DrawRoundedRectangle(
                    &D2D1_ROUNDED_RECT { rect: rc, radiusX: 8.0, radiusY: 8.0 },
                    &b,
                    1.0,
                    None,
                );
            }
        }
        // Teal accent stripe down the left edge (§5 design language).
        self.fill_round(
            rect(rc.left + 6.0, rc.top + 10.0, rc.left + 9.0, rc.bottom - 10.0),
            1.5,
            fade(theme::accent()),
        );

        let x0 = rc.left + 20.0;
        let x1 = rc.right - 14.0;
        self.text(&c.app, &self.fmt_app, rect(x0, rc.top + 10.0, x1 - 26.0, rc.top + 26.0), fade(theme::TEXT_DIM));
        self.text(&c.title, &self.fmt_title, rect(x0, rc.top + 28.0, x1, rc.top + 48.0), fade(theme::TEXT));
        self.text(&c.body, &self.fmt_body, rect(x0, rc.top + 50.0, x1, rc.bottom - 8.0), fade(theme::TEXT_DIM));

        // Close glyph, top-right.
        let xr = self.close_rect(i);
        let xr = rect(xr.left + dx, xr.top, xr.right + dx, xr.bottom);
        if c.hovered {
            self.fill_round(xr, 5.0, fade(theme::HOVER_FILL));
        }
        self.text(&[0xE8BB], &self.fmt_glyph, xr, fade(theme::TEXT_DIM));
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
        unsafe {
            if let Ok(b) = self.renderer.brush(color) {
                self.renderer.dc.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT { rect: rc, radiusX: radius, radiusY: radius },
                    &b,
                );
            }
        }
    }
}

fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F { left, top, right, bottom }
}

/// Poll the action center, diff ids, forward new toasts; the command channel
/// doubles as the tick clock (recv_timeout = poll cadence).
fn worker(cmds: Receiver<Cmd>, arrivals: Sender<Arrival>, hwnd_raw: isize) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let hwnd = HWND(hwnd_raw as *mut _);
    let listener = match UserNotificationListener::Current() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[toasts] listener unavailable: {e:?}");
            return;
        }
    };
    match listener.RequestAccessAsync().and_then(|op| op.join()) {
        Ok(UserNotificationListenerAccessStatus::Allowed) => {}
        Ok(s) => {
            eprintln!("[toasts] access {s:?} — enable in ms-settings:privacy-notifications");
            return;
        }
        Err(e) => {
            eprintln!("[toasts] RequestAccessAsync: {e:?}");
            return;
        }
    }

    // Baseline: whatever predates the shell stays in the action center only.
    let mut known: Vec<u32> = enumerate(&listener)
        .into_iter()
        .map(|(id, _, _, _)| id)
        .collect();

    loop {
        match cmds.recv_timeout(Duration::from_secs(1)) {
            Ok(Cmd::Remove(id)) => {
                let _ = listener.RemoveNotification(id);
            }
            Err(RecvTimeoutError::Timeout) => {
                let now = enumerate(&listener);
                let mut fresh = false;
                for (id, app, texts, aumid) in &now {
                    if !known.contains(id) {
                        let _ = arrivals.send(Arrival {
                            id: *id,
                            app: app.clone(),
                            texts: texts.clone(),
                            aumid: aumid.clone(),
                        });
                        fresh = true;
                    }
                }
                known = now.into_iter().map(|(id, _, _, _)| id).collect();
                if fresh {
                    unsafe {
                        let _ = PostMessageW(Some(hwnd), WM_APP_TOAST, WPARAM(0), LPARAM(0));
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

type Enumerated = (u32, String, Vec<String>, Option<String>);

fn enumerate(listener: &UserNotificationListener) -> Vec<Enumerated> {
    let mut out = Vec::new();
    let Ok(list) = listener
        .GetNotificationsAsync(NotificationKinds::Toast)
        .and_then(|op| op.join())
    else {
        return out;
    };
    for n in &list {
        let Ok(id) = n.Id() else { continue };
        let info = n.AppInfo();
        let app = info
            .as_ref()
            .ok()
            .and_then(|a| a.DisplayInfo().ok())
            .and_then(|d| d.DisplayName().ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "알 수 없는 앱".into());
        let aumid = info
            .as_ref()
            .ok()
            .and_then(|a| a.AppUserModelId().ok())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        let mut texts = Vec::new();
        if let Ok(visual) = n.Notification().and_then(|c| c.Visual()) {
            if let Ok(binding) =
                KnownNotificationBindings::ToastGeneric().and_then(|b| visual.GetBinding(&b))
            {
                if let Ok(elements) = binding.GetTextElements() {
                    for t in &elements {
                        if let Ok(s) = t.Text() {
                            texts.push(s.to_string());
                        }
                    }
                }
            }
        }
        out.push((id, app, texts, aumid));
    }
    out
}

extern "system" fn toast_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Toasts;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let t = &mut *ptr;
        let lx = |t: &Toasts| (lparam.0 & 0xFFFF) as i16 as f32 / t.scale;
        let ly = |t: &Toasts| ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 / t.scale;
        match msg {
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                t.paint();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_NCHITTEST => {
                // Screen coords; gaps between cards pass clicks through.
                let mut pt = POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                let _ = ScreenToClient(hwnd, &mut pt);
                let (x, y) = (pt.x as f32 / t.scale, pt.y as f32 / t.scale);
                if t.card_at(x, y).is_some() {
                    LRESULT(HTCLIENT as isize)
                } else {
                    LRESULT(HTTRANSPARENT as i32 as isize)
                }
            }
            WM_APP_TOAST => {
                t.on_arrivals();
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = (lx(t), ly(t));
                let over = t.card_at(x, y);
                let mut dirty = false;
                for (i, c) in t.cards.iter_mut().enumerate() {
                    let h = over == Some(i);
                    if c.hovered != h {
                        c.hovered = h;
                        dirty = true;
                    }
                }
                if dirty {
                    t.paint();
                }
                if !t.tracking {
                    let mut tme = windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<
                            windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT,
                        >() as u32,
                        dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    if windows::Win32::UI::Input::KeyboardAndMouse::TrackMouseEvent(&mut tme)
                        .is_ok()
                    {
                        t.tracking = true;
                    }
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                t.tracking = false;
                let mut dirty = false;
                for c in &mut t.cards {
                    if c.hovered {
                        c.hovered = false;
                        dirty = true;
                    }
                }
                if dirty {
                    t.paint();
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let (x, y) = (lx(t), ly(t));
                if let Some(i) = t.card_at(x, y) {
                    t.click(i, x, y);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                match wparam.0 {
                    TIMER_LIFE => t.tick(),
                    TIMER_ANIM => {
                        if !t.step_anim() {
                            t.stop_anim();
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
