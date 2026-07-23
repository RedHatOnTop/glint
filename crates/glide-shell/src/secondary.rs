//! Secondary-monitor bars (M5). One lightweight bar per non-primary
//! monitor: mirrored window buttons (icon-only) plus a clock. Tray, status
//! cluster and the start button stay on the primary bar — the Win11 split.
//! The primary bar owns the set and tears it down and rebuilds it wholesale
//! on WM_DISPLAYCHANGE; the Duo's bottom panel docking on and off is just a
//! monitor appearing in or vanishing from that enumeration.

use std::collections::HashMap;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_ROUNDED_RECT, ID2D1Bitmap1,
};
use windows::Win32::Graphics::DirectWrite::{DWRITE_MEASURING_MODE_NATURAL, IDWriteTextFormat};
use windows::Win32::Graphics::Dwm::{
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    ValidateRect,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::Shell::{ABM_REMOVE, APPBARDATA, SHAppBarMessage};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use crate::render::Renderer;
use crate::theme;

const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_CLOCK: usize = 1;
const BTN_W: f32 = 48.0;
const BTN_X0: f32 = 8.0;
const ICON: f32 = 24.0;
const CLOCK_W: f32 = 84.0;

/// What the primary bar shares with its mirrors: one live window button.
pub struct Mirror {
    pub hwnd: isize,
    pub active: bool,
    pub flash: bool,
}

pub struct Secondary {
    hwnd: HWND,
    renderer: Renderer,
    scale: f32,
    width: f32,
    entries: Vec<Mirror>,
    /// Window → icon on THIS bar's D2D device; bitmaps don't cross devices.
    icons: HashMap<isize, Option<ID2D1Bitmap1>>,
    hover: Option<usize>,
    tracking: bool,
}

impl Secondary {
    /// Create a bar on `mon` (full monitor rect) and reserve its appbar slot.
    pub fn new(mon: RECT) -> anyhow::Result<Box<Self>> {
        unsafe {
            let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            let class = w!("glide_shell_bar2");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(sec_wndproc),
                hInstance: hinstance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            RegisterClassW(&wc); // 0 on re-register is fine

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
            let dark: i32 = 1;
            let _ =
                DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
            let backdrop: i32 = 3;
            let _ =
                DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &backdrop as *const _ as _, 4);
            let round = DWMWCP_ROUND.0;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &round as *const _ as _,
                4,
            );

            let dpi = GetDpiForWindow(hwnd) as f32;
            let scale = dpi / 96.0;
            // Floating slab, same as the primary bar: reserve BAR_HEIGHT +
            // PANEL_MARGIN_BOTTOM of strut, then inset the window into it.
            let strut = ((theme::bar_height() + theme::PANEL_MARGIN_BOTTOM) * scale).round() as i32;
            let band = crate::taskbar::appbar_negotiate_on(hwnd, strut, mon);
            let rect = crate::taskbar::panel_rect(band, scale);
            let (w_px, h_px) = (rect.right - rect.left, rect.bottom - rect.top);
            MoveWindow(hwnd, rect.left, rect.top, w_px, h_px, true)?;
            let renderer = Renderer::new(hwnd, w_px as u32, h_px as u32, dpi)?;

            let mut sec = Box::new(Secondary {
                hwnd,
                renderer,
                scale,
                width: w_px as f32 / scale,
                entries: Vec::new(),
                icons: HashMap::new(),
                hover: None,
                tracking: false,
            });
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut *sec as *mut Secondary as isize);
            SetTimer(Some(hwnd), TIMER_CLOCK, 1000, None);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            sec.paint();
            Ok(sec)
        }
    }

    /// New button set from the primary bar; drops icons of dead windows.
    pub fn sync(&mut self, entries: Vec<Mirror>) {
        self.icons.retain(|k, _| entries.iter().any(|e| e.hwnd == *k));
        if self.hover.is_some_and(|h| h >= entries.len()) {
            self.hover = None;
        }
        self.entries = entries;
        self.paint();
    }

    /// Re-negotiate the appbar slot against this bar's own monitor.
    fn reposition(&mut self) {
        unsafe {
            let hmon = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
                return;
            }
            let dpi = GetDpiForWindow(self.hwnd) as f32;
            self.scale = dpi / 96.0;
            let strut =
                ((theme::bar_height() + theme::PANEL_MARGIN_BOTTOM) * self.scale).round() as i32;
            let band = crate::taskbar::appbar_requery(self.hwnd, strut, mi.rcMonitor);
            let rect = crate::taskbar::panel_rect(band, self.scale);
            let (w_px, h_px) = (rect.right - rect.left, rect.bottom - rect.top);
            let _ = MoveWindow(self.hwnd, rect.left, rect.top, w_px, h_px, true);
            let _ = self.renderer.resize(w_px as u32, h_px as u32, dpi);
            self.width = w_px as f32 / self.scale;
            self.paint();
        }
    }

    fn hit(&self, x: f32) -> Option<usize> {
        let i = ((x - BTN_X0) / BTN_W).floor();
        (x >= BTN_X0 && i >= 0.0 && (i as usize) < self.entries.len()).then_some(i as usize)
    }

    fn click(&mut self, i: usize) {
        let Some(e) = self.entries.get(i) else { return };
        let h = HWND(e.hwnd as *mut _);
        unsafe {
            if h == GetForegroundWindow() {
                let _ = ShowWindow(h, SW_MINIMIZE);
            } else {
                if IsIconic(h).as_bool() {
                    let _ = ShowWindow(h, SW_RESTORE);
                }
                crate::taskbar::force_foreground(h);
            }
        }
    }

    fn paint(&mut self) {
        unsafe {
            let r = &self.renderer;
            r.dc.BeginDraw();
            r.dc.Clear(Some(&theme::BAR_BG));

            for i in 0..self.entries.len() {
                let x = BTN_X0 + i as f32 * BTN_W;
                let rc = rect(x, 4.0, x + BTN_W - 4.0, theme::bar_height() - 4.0);
                let (key, active, flash) = {
                    let e = &self.entries[i];
                    (e.hwnd, e.active, e.flash)
                };
                if active {
                    self.fill_round(rc, theme::BUTTON_RADIUS, theme::ACTIVE_FILL);
                }
                if self.hover == Some(i) {
                    self.fill_round(rc, theme::BUTTON_RADIUS, theme::HOVER_FILL);
                }
                let icon = self
                    .icons
                    .entry(key)
                    .or_insert_with(|| crate::icons::window_icon(&r.dc, HWND(key as *mut _)))
                    .clone();
                let cx = x + (BTN_W - 4.0) / 2.0;
                let ic = rect(
                    cx - ICON / 2.0,
                    (theme::bar_height() - ICON) / 2.0 - 2.0,
                    cx + ICON / 2.0,
                    (theme::bar_height() + ICON) / 2.0 - 2.0,
                );
                match icon {
                    Some(bmp) => r.dc.DrawBitmap(
                        &bmp,
                        Some(&ic),
                        1.0,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        None,
                        None,
                    ),
                    None => {
                        if let Ok(b) = r.brush(theme::TEXT_DIM) {
                            r.dc.DrawRoundedRectangle(
                                &D2D1_ROUNDED_RECT { rect: ic, radiusX: 4.0, radiusY: 4.0 },
                                &b,
                                1.0,
                                None,
                            );
                        }
                    }
                }
                if active || flash {
                    let color = if flash { theme::FLASH } else { theme::accent() };
                    self.fill_round(
                        rect(
                            cx - 8.0,
                            theme::bar_height() - theme::UNDERLINE_H - 1.0,
                            cx + 8.0,
                            theme::bar_height() - 1.0,
                        ),
                        1.5,
                        color,
                    );
                }
            }

            // Clock block, right-aligned: HH:MM over M/D (요일) — same shape
            // as the primary bar's.
            let now = chrono::Local::now();
            let hhmm: Vec<u16> = now.format("%H:%M").to_string().encode_utf16().collect();
            let wd = ["월", "화", "수", "목", "금", "토", "일"]
                [chrono::Datelike::weekday(&now).num_days_from_monday() as usize];
            let date: Vec<u16> = format!(
                "{}/{} ({})",
                chrono::Datelike::month(&now),
                chrono::Datelike::day(&now),
                wd
            )
            .encode_utf16()
            .collect();
            let cx0 = self.width - CLOCK_W;
            self.text(
                &hhmm,
                &r.fmt_clock.clone(),
                rect(cx0, 3.0, self.width - 10.0, 22.0),
                theme::TEXT,
            );
            self.text(
                &date,
                &r.fmt_date.clone(),
                rect(cx0, 21.0, self.width - 10.0, theme::bar_height() - 3.0),
                theme::TEXT_DIM,
            );

            let _ = r.dc.EndDraw(None, None);
            let _ = self.renderer.present();
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

impl Drop for Secondary {
    fn drop(&mut self) {
        unsafe {
            let mut abd = APPBARDATA {
                cbSize: std::mem::size_of::<APPBARDATA>() as u32,
                hWnd: self.hwnd,
                ..Default::default()
            };
            SHAppBarMessage(ABM_REMOVE, &mut abd);
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Full monitor rects with their primary flag, for the rebuild sweep.
pub fn monitors() -> Vec<(RECT, bool)> {
    use windows::Win32::Graphics::Gdi::EnumDisplayMonitors;
    unsafe extern "system" fn cb(
        hmon: HMONITOR,
        _hdc: HDC,
        _rc: *mut RECT,
        l: LPARAM,
    ) -> windows::core::BOOL {
        unsafe {
            let out = &mut *(l.0 as *mut Vec<(RECT, bool)>);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(hmon, &mut mi).as_bool() {
                // MONITORINFOF_PRIMARY
                out.push((mi.rcMonitor, mi.dwFlags & 1 != 0));
            }
        }
        true.into()
    }
    let mut out: Vec<(RECT, bool)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(cb),
            LPARAM(&mut out as *mut _ as isize),
        );
    }
    out
}

fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F { left, top, right, bottom }
}

extern "system" fn sec_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Secondary;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let sec = &mut *ptr;
        match msg {
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                sec.paint();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_MOUSEMOVE => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32 / sec.scale;
                let h = sec.hit(x);
                if h != sec.hover {
                    sec.hover = h;
                    sec.paint();
                }
                if !sec.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    if TrackMouseEvent(&mut tme).is_ok() {
                        sec.tracking = true;
                    }
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                sec.tracking = false;
                if sec.hover.take().is_some() {
                    sec.paint();
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32 / sec.scale;
                if let Some(i) = sec.hit(x) {
                    sec.click(i);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == TIMER_CLOCK {
                    sec.paint();
                }
                LRESULT(0)
            }
            crate::taskbar::WM_APPBAR => {
                if wparam.0 == crate::taskbar::ABN_POSCHANGED_ID {
                    sec.reposition();
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
