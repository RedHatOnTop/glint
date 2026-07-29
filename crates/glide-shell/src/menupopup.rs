//! Own-drawn context menu (SHELL_DESIGN §5).
//!
//! The shell still builds a real HMENU — IContextMenu has nowhere else to put
//! its items, and an extension fills its submenu into one — but nothing ever
//! shows it. Each level is read back with GetMenuItemInfoW and painted into a
//! composition window of ours, so a right-click lands in the same floating
//! rounded panel as the bar and the action centre instead of the grey win32
//! box that TrackPopupMenuEx draws.
//!
//! What a real menu loop would have given us and we give up: owner-drawn
//! items. An extension that paints its own rows hands out no string, and is
//! skipped rather than rendered as a blank line.

use std::time::Instant;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ELLIPSE, D2D1_INTERPOLATION_MODE_LINEAR, ID2D1Bitmap1,
    ID2D1DeviceContext,
};
use windows::Win32::Graphics::DirectWrite::{DWRITE_MEASURING_MODE_NATURAL, IDWriteTextFormat};
use windows::Win32::Graphics::Dwm::{
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, GetMonitorInfoW, HBITMAP, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint, ValidateRect,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA,
    GetMenuItemCount, GetMenuItemInfoW, GetMessageW, GetWindowLongPtrW, GetWindowRect, HMENU,
    HWND_TOPMOST, IDC_ARROW, KillTimer, LoadCursorW, MENUITEMINFOW, MFS_CHECKED, MFS_GRAYED,
    MFT_RADIOCHECK, MFT_SEPARATOR, MIIM_BITMAP, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING,
    MIIM_SUBMENU, MSG, PostQuitMessage, RegisterClassW, SW_SHOWNA, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage,
    WM_ACTIVATEAPP, WM_CAPTURECHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::w;
use windows_numerics::{Matrix3x2, Vector2};

use crate::render::{Renderer, fill_round, rect};
use crate::theme;

/// Logical metrics. One row is taller than a win32 menu row on purpose: this
/// is a touchable panel, not a 1990s strip.
const PAD: f32 = 5.0;
const ROW_H: f32 = 30.0;
const SEP_H: f32 = 9.0;
const ICON_COL: f32 = 28.0;
const RIGHT_PAD: f32 = 12.0;
const ARROW_W: f32 = 18.0;
const ACCEL_GAP: f32 = 24.0;
const ROW_RADIUS: f32 = 7.0;
const MIN_W: f32 = 190.0;
const MAX_W: f32 = 460.0;
/// Hover dwell before a submenu opens — without it, dragging the pointer down
/// a column creates and destroys a composition window per row.
const SUB_DELAY_MS: u32 = 180;
const TIMER_SUB: usize = 1;
const TIMER_ANIM: usize = 2;

const CLASS: windows::core::PCWSTR = w!("glide_shell_menupopup");

struct Row {
    id: u32,
    text: String,
    /// Everything after the tab — "Ctrl+V" and friends, right-aligned.
    accel: String,
    sep: bool,
    enabled: bool,
    checked: bool,
    radio: bool,
    sub: Option<HMENU>,
    icon: Option<ID2D1Bitmap1>,
    top: f32,
    h: f32,
}

struct Popup {
    hwnd: HWND,
    r: Renderer,
    rows: Vec<Row>,
    hover: Option<usize>,
    /// Row whose submenu is currently open — stays lit while the pointer is
    /// inside the child.
    sub_row: Option<usize>,
    w: f32,
    h: f32,
    scale: f32,
    any_sub: bool,
    /// Widest accelerator in this level; the labels stop short of it.
    accel_w: f32,
    born: Instant,
}

struct Track {
    stack: Vec<Popup>,
    init: fn(HMENU, u32),
    picked: u32,
    done: bool,
    /// Applied by the loop, never by the wndproc: creating or destroying a
    /// window inside a message handler re-enters this proc with the stack
    /// half-torn-down.
    want: Option<(usize, usize, bool)>,
    close_to: Option<usize>,
    /// Last pointer position we acted on. Showing a window under a stationary
    /// cursor posts a WM_MOUSEMOVE, and taking that at face value closed the
    /// submenu the keyboard had just opened.
    last_pt: Option<POINT>,
}

/// Show `hmenu` at a screen point and block until something is picked or the
/// menu is dismissed. Returns the picked command id, 0 on dismissal.
///
/// `init` is called with (popup, item position) before each level is read, so
/// the caller can hand WM_INITMENUPOPUP to an IContextMenu2/3 the way a menu
/// loop would.
pub fn track(owner: HWND, hmenu: HMENU, x: i32, y: i32, init: fn(HMENU, u32)) -> u32 {
    unsafe {
        register();
        let dpi = dpi_at(x, y);
        let mut t = Box::new(Track {
            stack: Vec::new(),
            init,
            picked: 0,
            done: false,
            want: None,
            close_to: None,
            last_pt: None,
        });
        let ptr = &mut *t as *mut Track;
        let Some(mut root) = Popup::new(ptr, hmenu, 0, dpi, init, true) else {
            return 0;
        };
        let (wd, hd) = root.device_size();
        let work = work_area(x, y);
        // Down-right of the pointer, flipping rather than sliding: a menu that
        // slid would sit under the pointer and eat the next click.
        let px = if x + wd > work.right { (x - wd).max(work.left) } else { x };
        let py = if y + hd > work.bottom { (y - hd).max(work.top) } else { y };
        root.place(px, py, wd, hd);
        t.stack.push(root);

        // Activation alone does not move the keyboard: the popup is shown
        // without activating (SW_SHOWNA) so the desktop underneath keeps its
        // selection painted, so the focus has to be taken by hand. Same
        // thread as the owner, which is the only reason SetFocus is allowed.
        let _ = SetForegroundWindow(t.stack[0].hwnd);
        let _ = SetFocus(Some(t.stack[0].hwnd));
        SetCapture(t.stack[0].hwnd);

        let mut msg = MSG::default();
        while !t.done {
            let got = GetMessageW(&mut msg, None, 0, 0);
            if got.0 <= 0 {
                if got.0 == 0 {
                    PostQuitMessage(msg.wParam.0 as i32);
                }
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            t.apply_pending();
        }

        let _ = ReleaseCapture();
        while !t.stack.is_empty() {
            t.pop_level();
        }
        // The owner is a NOACTIVATE window hosting a menu: same epilogue the
        // classic tray menu needs so the input state fully unwinds.
        let _ = SetForegroundWindow(owner);
        t.picked
    }
}

unsafe fn register() {
    unsafe {
        let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None);
        let Ok(hinstance) = hinstance else { return };
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc); // 0 on re-register is fine
    }
}

fn dpi_at(x: i32, y: i32) -> f32 {
    unsafe {
        let hmon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        if dx == 0 { 96.0 } else { dx as f32 }
    }
}

fn work_area(x: i32, y: i32) -> RECT {
    unsafe {
        let hmon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            RECT { left: 0, top: 0, right: 1920, bottom: 1080 }
        }
    }
}

/// One item's string, in two passes: the first asks how long it is.
unsafe fn menu_text(hmenu: HMENU, pos: u32) -> String {
    unsafe {
        let mut mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING,
            ..Default::default()
        };
        if GetMenuItemInfoW(hmenu, pos, true, &mut mii).is_err() || mii.cch == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; mii.cch as usize + 1];
        mii.cch = buf.len() as u32;
        mii.dwTypeData = windows::core::PWSTR(buf.as_mut_ptr());
        if GetMenuItemInfoW(hmenu, pos, true, &mut mii).is_err() {
            return String::new();
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }
}

unsafe fn read_rows(dc: &ID2D1DeviceContext, hmenu: HMENU) -> Vec<Row> {
    unsafe {
        let count = GetMenuItemCount(Some(hmenu));
        let mut rows = Vec::new();
        for i in 0..count {
            let mut mii = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_SUBMENU | MIIM_BITMAP,
                ..Default::default()
            };
            if GetMenuItemInfoW(hmenu, i as u32, true, &mut mii).is_err() {
                continue;
            }
            let sep = (mii.fType & MFT_SEPARATOR) == MFT_SEPARATOR;
            let raw = if sep { String::new() } else { menu_text(hmenu, i as u32) };
            if !sep && raw.is_empty() {
                continue; // owner-drawn: no string to paint, and no menu loop
            }
            let (text, accel) = match raw.split_once('\t') {
                Some((t, a)) => (t.to_string(), a.to_string()),
                None => (raw, String::new()),
            };
            // Alt mnemonics are drawn as marks by win32; we have no menu bar
            // and no Alt navigation, so && collapses and & disappears.
            let text = text.replace("&&", "\u{1}").replace('&', "").replace('\u{1}', "&");
            rows.push(Row {
                id: mii.wID,
                text,
                accel,
                sep,
                enabled: (mii.fState.0 & MFS_GRAYED.0) == 0,
                checked: (mii.fState.0 & MFS_CHECKED.0) != 0,
                radio: (mii.fType & MFT_RADIOCHECK) == MFT_RADIOCHECK,
                sub: (!mii.hSubMenu.is_invalid()).then_some(mii.hSubMenu),
                icon: menu_icon(dc, mii.hbmpItem),
                top: 0.0,
                h: if sep { SEP_H } else { ROW_H },
            });
        }
        rows
    }
}

/// hbmpItem doubles as an enum: the HBMMENU_* system marks are tiny integers
/// cast to a handle, and asking GDI about one of those is a crash.
fn menu_icon(dc: &ID2D1DeviceContext, hbm: HBITMAP) -> Option<ID2D1Bitmap1> {
    let v = hbm.0 as isize;
    if v <= 16 && v >= -1 {
        return None;
    }
    crate::icons::hbitmap_bitmap(dc, hbm)
}

impl Popup {
    /// Window, renderer, items and size — in that order, because measuring the
    /// text needs a DirectWrite factory and the icons need a device context.
    unsafe fn new(
        track: *mut Track,
        hmenu: HMENU,
        pos: u32,
        dpi: f32,
        init: fn(HMENU, u32),
        activatable: bool,
    ) -> Option<Popup> {
        unsafe {
            let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).ok()?;
            let mut ex = WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP;
            if !activatable {
                // Only the root takes focus; the keys are handled there for
                // every level, so a submenu must never steal it.
                ex |= WS_EX_NOACTIVATE;
            }
            let hwnd = CreateWindowExW(
                ex,
                CLASS,
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
            )
            .ok()?;
            let dark: i32 = 1;
            let _ =
                DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
            let backdrop: i32 = 3; // DWMSBT_TRANSIENTWINDOW
            let _ =
                DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &backdrop as *const _ as _, 4);
            let corner = DWMWCP_ROUND.0;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const _ as _,
                4,
            );
            let Ok(r) = Renderer::new(hwnd, 64, 64, dpi) else {
                let _ = DestroyWindow(hwnd);
                return None;
            };
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, track as isize);

            // A lazily-filled submenu (새로 만들기) is empty until its owner
            // sees WM_INITMENUPOPUP, which is the one thing a menu loop did
            // for us that nobody else will.
            init(hmenu, pos);
            let rows = read_rows(&r.dc, hmenu);
            if rows.iter().all(|it| it.sep) {
                let _ = DestroyWindow(hwnd);
                return None;
            }
            let mut p = Popup {
                hwnd,
                r,
                rows,
                hover: None,
                sub_row: None,
                w: 0.0,
                h: 0.0,
                scale: dpi / 96.0,
                any_sub: false,
                accel_w: 0.0,
                born: Instant::now(),
            };
            p.measure();
            Some(p)
        }
    }

    fn measure(&mut self) {
        let fmt = self.r.fmt_title.clone();
        let mut text_w: f32 = 0.0;
        let mut accel_w: f32 = 0.0;
        for it in &self.rows {
            if it.sep {
                continue;
            }
            let t: Vec<u16> = it.text.encode_utf16().collect();
            text_w = text_w.max(self.r.text_width(&t, &fmt, MAX_W));
            if !it.accel.is_empty() {
                let a: Vec<u16> = it.accel.encode_utf16().collect();
                accel_w = accel_w.max(self.r.text_width(&a, &fmt, MAX_W));
            }
        }
        self.any_sub = self.rows.iter().any(|it| it.sub.is_some());
        self.accel_w = accel_w;
        let extra = if accel_w > 0.0 { ACCEL_GAP + accel_w } else { 0.0 }
            + if self.any_sub { ARROW_W } else { 0.0 };
        self.w = (PAD * 2.0 + ICON_COL + text_w + extra + RIGHT_PAD).clamp(MIN_W, MAX_W);
        let mut y = PAD;
        for it in &mut self.rows {
            it.top = y;
            y += it.h;
        }
        self.h = y + PAD;
    }

    fn device_size(&self) -> (i32, i32) {
        (
            (self.w * self.scale).round() as i32,
            (self.h * self.scale).round() as i32,
        )
    }

    fn place(&mut self, x: i32, y: i32, wd: i32, hd: i32) {
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                wd,
                hd,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = self.r.resize(wd as u32, hd as u32, self.scale * 96.0);
            let _ = ShowWindow(self.hwnd, SW_SHOWNA);
            SetTimer(Some(self.hwnd), TIMER_ANIM, 8, None);
        }
        self.born = Instant::now();
        self.paint();
    }

    fn screen_rect(&self) -> RECT {
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut r);
        }
        r
    }

    /// Screen point → row, in this popup's logical space.
    fn hit(&self, pt: POINT) -> Option<usize> {
        let r = self.screen_rect();
        if pt.x < r.left || pt.x >= r.right || pt.y < r.top || pt.y >= r.bottom {
            return None;
        }
        let y = (pt.y - r.top) as f32 / self.scale;
        self.rows.iter().position(|it| y >= it.top && y < it.top + it.h)
    }

    fn contains(&self, pt: POINT) -> bool {
        let r = self.screen_rect();
        pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
    }

    /// Next selectable row in `dir`, wrapping — separators and disabled items
    /// are not stops.
    fn step(&self, from: Option<usize>, dir: i32) -> Option<usize> {
        let n = self.rows.len();
        if n == 0 {
            return None;
        }
        let mut i = match from {
            Some(i) => i as i32,
            None => {
                if dir > 0 {
                    -1
                } else {
                    n as i32
                }
            }
        };
        for _ in 0..n {
            i = (i + dir).rem_euclid(n as i32);
            let it = &self.rows[i as usize];
            if !it.sep && it.enabled {
                return Some(i as usize);
            }
        }
        None
    }

    fn paint(&self) {
        let t = (self.born.elapsed().as_secs_f32() * 1000.0 / theme::ANIM_MS).clamp(0.0, 1.0);
        let e = 1.0 - (1.0 - t).powi(3);
        let fade = |c: D2D1_COLOR_F| theme::with_alpha(c, c.a * e);
        unsafe {
            let dc = &self.r.dc;
            dc.BeginDraw();
            dc.Clear(Some(&fade(theme::rgba(26, 27, 32, 0.92))));
            // Rows rise into place; the panel itself does not move, so the
            // acrylic edge stays put under the pointer.
            dc.SetTransform(&Matrix3x2::translation(0.0, (1.0 - e) * 6.0));
            // Accent hairline: the same signature the overflow panel wears.
            if let Ok(b) = self.r.brush(fade(theme::with_alpha(theme::accent(), 0.30))) {
                dc.FillRectangle(&rect(0.0, 0.0, self.w, 1.0 / self.scale), &b);
            }
            for (i, it) in self.rows.iter().enumerate() {
                if it.sep {
                    let y = it.top + (it.h / 2.0).floor();
                    fill_round(
                        &self.r,
                        rect(PAD + 10.0, y, self.w - PAD - 10.0, y + 1.0 / self.scale),
                        0.0,
                        fade(theme::rgba(255, 255, 255, 0.10)),
                    );
                    continue;
                }
                let row = rect(PAD, it.top, self.w - PAD, it.top + it.h);
                let open = self.sub_row == Some(i);
                if self.hover == Some(i) || open {
                    let fill = if self.hover == Some(i) {
                        theme::HOVER_FILL
                    } else {
                        theme::ACTIVE_FILL
                    };
                    fill_round(&self.r, row, ROW_RADIUS, fade(fill));
                    // Accent pill at the leading edge — the bar marks its
                    // active button the same way.
                    let mid = it.top + it.h / 2.0;
                    fill_round(
                        &self.r,
                        rect(PAD + 3.0, mid - 7.0, PAD + 6.0, mid + 7.0),
                        1.5,
                        fade(theme::accent()),
                    );
                }
                let ink = if it.enabled {
                    theme::TEXT
                } else {
                    theme::with_alpha(theme::TEXT_DIM, 0.55)
                };
                let icon_r = rect(PAD + 8.0, it.top, PAD + 8.0 + ICON_COL - 8.0, it.top + it.h);
                if it.checked {
                    if it.radio {
                        let c = fade(theme::accent());
                        if let Ok(b) = self.r.brush(c) {
                            dc.FillEllipse(
                                &D2D1_ELLIPSE {
                                    point: Vector2 {
                                        X: (icon_r.left + icon_r.right) / 2.0,
                                        Y: it.top + it.h / 2.0,
                                    },
                                    radiusX: 3.5,
                                    radiusY: 3.5,
                                },
                                &b,
                            );
                        }
                    } else {
                        self.text("\u{E73E}", &self.r.fmt_glyph.clone(), icon_r, fade(theme::accent()));
                    }
                } else if let Some(bmp) = &it.icon {
                    let cx = (icon_r.left + icon_r.right) / 2.0;
                    let cy = it.top + it.h / 2.0;
                    dc.DrawBitmap(
                        bmp,
                        Some(&rect(cx - 8.0, cy - 8.0, cx + 8.0, cy + 8.0)),
                        e,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        None,
                        None,
                    );
                }
                let text_l = PAD + ICON_COL;
                let arrow = if self.any_sub { ARROW_W } else { 0.0 };
                let text_r = self.w - PAD - RIGHT_PAD - arrow;
                let label_r = if self.accel_w > 0.0 { text_r - ACCEL_GAP - self.accel_w } else { text_r };
                self.text(
                    &it.text,
                    &self.r.fmt_title.clone(),
                    rect(text_l, it.top, label_r, it.top + it.h),
                    fade(ink),
                );
                if !it.accel.is_empty() {
                    let a: Vec<u16> = it.accel.encode_utf16().collect();
                    let aw = self.r.text_width(&a, &self.r.fmt_title.clone(), MAX_W);
                    self.text(
                        &it.accel,
                        &self.r.fmt_title.clone(),
                        rect(text_r - aw, it.top, text_r, it.top + it.h),
                        fade(theme::with_alpha(theme::TEXT_DIM, 0.9)),
                    );
                }
                if it.sub.is_some() {
                    self.text(
                        "\u{E76C}",
                        &self.r.fmt_glyph.clone(),
                        rect(self.w - PAD - ARROW_W, it.top, self.w - PAD, it.top + it.h),
                        fade(theme::with_alpha(ink, 0.75)),
                    );
                }
            }
            dc.SetTransform(&Matrix3x2::identity());
            let _ = dc.EndDraw(None, None);
            let _ = self.r.present();
        }
    }

    fn text(&self, s: &str, fmt: &IDWriteTextFormat, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        let t: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            if let Ok(b) = self.r.brush(c) {
                self.r.dc.DrawText(
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
}

impl Track {
    fn level_of(&self, hwnd: HWND) -> Option<usize> {
        self.stack.iter().position(|p| p.hwnd == hwnd)
    }

    /// Deepest level containing the point — the levels overlap, and the child
    /// is on top.
    fn level_at(&self, pt: POINT) -> Option<usize> {
        self.stack.iter().rposition(|p| p.contains(pt))
    }

    fn pop_level(&mut self) {
        if let Some(p) = self.stack.pop() {
            unsafe {
                let _ = KillTimer(Some(p.hwnd), TIMER_ANIM);
                let hwnd = p.hwnd;
                drop(p); // renderer first: it holds a composition target
                let _ = DestroyWindow(hwnd);
            }
            if let Some(parent) = self.stack.last_mut() {
                parent.sub_row = None;
                parent.paint();
            }
        }
    }

    fn apply_pending(&mut self) {
        if let Some(level) = self.close_to.take() {
            while self.stack.len() > level {
                self.pop_level();
            }
        }
        let Some((level, row, select_first)) = self.want.take() else {
            return;
        };
        while self.stack.len() > level + 1 {
            self.pop_level();
        }
        let Some(parent) = self.stack.get(level) else { return };
        let Some(sub) = parent.rows.get(row).and_then(|it| it.sub) else {
            return;
        };
        let pr = parent.screen_rect();
        let scale = parent.scale;
        let top = parent.rows[row].top;
        let dpi = parent.r.dpi;
        let init = self.init;
        let ptr = self as *mut Track;
        let Some(mut child) = (unsafe { Popup::new(ptr, sub, row as u32, dpi, init, false) })
        else {
            return;
        };
        let (wd, hd) = child.device_size();
        // Overlap the parent by a couple of pixels so the pointer never
        // crosses a gap on its way in.
        let gap = (2.0 * scale) as i32;
        let work = work_area(pr.left, pr.top);
        let x = if pr.right - gap + wd > work.right {
            (pr.left + gap - wd).max(work.left)
        } else {
            pr.right - gap
        };
        let y = (pr.top + (top * scale) as i32 - (PAD * scale) as i32)
            .min(work.bottom - hd)
            .max(work.top);
        child.place(x, y, wd, hd);
        if select_first {
            child.hover = child.step(None, 1);
            child.paint();
        }
        if let Some(parent) = self.stack.get_mut(level) {
            parent.sub_row = Some(row);
            parent.paint();
        }
        self.stack.push(child);
    }

    /// Activate a row: a submenu opens, anything else picks and ends.
    fn activate(&mut self, level: usize, row: usize) {
        let Some(p) = self.stack.get(level) else { return };
        let Some(it) = p.rows.get(row) else { return };
        if it.sep || !it.enabled {
            return;
        }
        if it.sub.is_some() {
            self.want = Some((level, row, true));
            return;
        }
        self.picked = it.id;
        self.done = true;
    }

    fn hover_move(&mut self, dir: i32) {
        let Some(top) = self.stack.len().checked_sub(1) else { return };
        let p = &mut self.stack[top];
        let next = p.step(p.hover, dir);
        if next != p.hover {
            p.hover = next;
            p.paint();
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Track;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let t = &mut *ptr;
        // Everything under capture arrives relative to the capture window, so
        // screen coordinates are the only common frame the levels share.
        let screen = || {
            let mut pt = POINT {
                x: (lparam.0 & 0xFFFF) as i16 as i32,
                y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
            };
            let _ = ClientToScreen(hwnd, &mut pt);
            pt
        };
        match msg {
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                if let Some(l) = t.level_of(hwnd) {
                    t.stack[l].paint();
                }
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_MOUSEMOVE => {
                let pt = screen();
                if t.last_pt == Some(pt) {
                    return LRESULT(0);
                }
                t.last_pt = Some(pt);
                if let Some(l) = t.level_at(pt) {
                    let row = t.stack[l].hit(pt);
                    // Only a change of row moves anything: re-arming the dwell
                    // timer on every WM_MOUSEMOVE would mean a submenu that
                    // never opens while the hand is on the mouse.
                    if t.stack[l].hover != row {
                        t.stack[l].hover = row;
                        t.stack[l].paint();
                        let sub_here =
                            row.and_then(|i| t.stack[l].rows.get(i)).and_then(|it| it.sub);
                        let already = t.stack[l].sub_row == row && t.stack.len() > l + 1;
                        if sub_here.is_some() && !already {
                            PENDING.with(|p| p.set(Some((l, row.unwrap()))));
                            SetTimer(Some(t.stack[0].hwnd), TIMER_SUB, SUB_DELAY_MS, None);
                        } else if !already {
                            // Anything deeper belongs to a row we just left.
                            PENDING.with(|p| p.set(None));
                            if t.stack.len() > l + 1 {
                                t.close_to = Some(l + 1);
                            }
                        }
                    }
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                if t.level_at(screen()).is_none() {
                    t.done = true;
                }
                LRESULT(0)
            }
            WM_LBUTTONUP | WM_RBUTTONUP => {
                let pt = screen();
                match t.level_at(pt) {
                    Some(l) => {
                        if let Some(row) = t.stack[l].hit(pt) {
                            t.activate(l, row);
                        }
                    }
                    None => t.done = true,
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let top = t.stack.len().saturating_sub(1);
                match windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(wparam.0 as u16) {
                    VK_ESCAPE => {
                        if t.stack.len() > 1 {
                            t.close_to = Some(top);
                        } else {
                            t.done = true;
                        }
                    }
                    VK_DOWN => t.hover_move(1),
                    VK_UP => t.hover_move(-1),
                    VK_RIGHT => {
                        if let Some(row) = t.stack[top].hover
                            && t.stack[top].rows[row].sub.is_some()
                        {
                            t.want = Some((top, row, true));
                        }
                    }
                    VK_LEFT => {
                        if t.stack.len() > 1 {
                            t.close_to = Some(top);
                        }
                    }
                    VK_RETURN => {
                        if let Some(row) = t.stack[top].hover {
                            t.activate(top, row);
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_TIMER => {
                match wparam.0 {
                    TIMER_SUB => {
                        let _ = KillTimer(Some(hwnd), TIMER_SUB);
                        if let Some((l, row)) = PENDING.with(|p| p.take()) {
                            t.want = Some((l, row, false));
                        }
                    }
                    TIMER_ANIM => {
                        if let Some(l) = t.level_of(hwnd) {
                            let done = t.stack[l].born.elapsed().as_secs_f32() * 1000.0
                                > theme::ANIM_MS;
                            t.stack[l].paint();
                            if done {
                                let _ = KillTimer(Some(hwnd), TIMER_ANIM);
                            }
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            // Capture lost or the app deactivated: something outside the menu
            // now owns the input, and a menu that outlives that is a stuck
            // menu.
            WM_CAPTURECHANGED => {
                if !t.done && t.stack.first().is_some_and(|p| p.hwnd == hwnd) {
                    t.done = true;
                }
                LRESULT(0)
            }
            WM_ACTIVATEAPP => {
                if wparam.0 == 0 {
                    t.done = true;
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

thread_local! {
    /// Row waiting on the submenu dwell timer. Lives outside Track only
    /// because the timer fires on the root window, which knows nothing about
    /// which level asked.
    static PENDING: std::cell::Cell<Option<(usize, usize)>> = const { std::cell::Cell::new(None) };
}
