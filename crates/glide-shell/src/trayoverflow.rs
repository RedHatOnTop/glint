//! Tray overflow flyout — the `^` chevron's hidden-icon grid (SHELL_DESIGN §6.2).
//!
//! The bar strip keeps only the first TRAY_MAX_VISIBLE tray icons; the rest
//! live here so a crowded tray can't push into the running-window buttons.
//! Icon bitmaps are bound to the device context that rasterised them, so the
//! bar's bitmaps can't be drawn on this window's own dc — we re-rasterise each
//! overflowed HICON on open (which is why the bar carries each icon's hicon).

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_ROUNDED_RECT, ID2D1Bitmap1,
};
use windows::Win32::Graphics::DirectWrite::DWRITE_MEASURING_MODE_NATURAL;
use windows::Win32::Graphics::Dwm::{
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use crate::render::{Renderer, fill_round, rect};
use crate::theme;
use crate::tray::{NIN_SELECT, forward};

const COLS: usize = 5;
/// Per-icon cell, logical px — matches the bar's TRAY_CELL_W feel.
const CELL: f32 = 30.0;
const PAD: f32 = 8.0;
const HEADER_H: f32 = 22.0;
const TIMER_REFRESH: usize = 1;
/// WM_MOUSELEAVE lives in UI::Controls (a feature we don't pull); define it
/// locally, as the bar and the flyout do.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// What the bar hands over for each demoted icon; enough to redraw it and to
/// forward a click without reaching back into the bar's Vec (whose indices
/// shift as icons come and go).
pub struct Src {
    pub owner: HWND,
    pub uid: u32,
    pub callback: u32,
    pub version: u32,
    pub hicon: isize,
}

struct Item {
    owner: HWND,
    uid: u32,
    callback: u32,
    version: u32,
    bitmap: Option<ID2D1Bitmap1>,
}

pub struct TrayOverflow {
    hwnd: HWND,
    renderer: Renderer,
    pub open: bool,
    /// Set by dismiss(): a real click's DOWN killed the panel (WA_INACTIVE /
    /// click-away) before the click's UP reached the bar; the chevron toggle
    /// consults this so UP doesn't reopen what its own DOWN just closed.
    last_dismissed: Option<std::time::Instant>,
    scale: f32,
    w: f32,
    h: f32,
    anchor_right: i32,
    anchor_bottom: i32,
    items: Vec<Item>,
    hover: Option<usize>,
    tracking: bool,
    hits: Vec<(D2D_RECT_F, usize)>,
}

impl TrayOverflow {
    pub fn new(dpi: f32) -> anyhow::Result<Self> {
        unsafe {
            let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            let class = w!("glide_shell_trayoverflow");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(overflow_wndproc),
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
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
            let backdrop: i32 = 3; // DWMSBT_TRANSIENTWINDOW
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &backdrop as *const _ as _, 4);
            let corner = DWMWCP_ROUND.0;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as _, 4);
            let renderer = Renderer::new(hwnd, 64, 64, dpi)?;

            Ok(TrayOverflow {
                hwnd,
                renderer,
                open: false,
                last_dismissed: None,
                scale: dpi / 96.0,
                w: 0.0,
                h: 0.0,
                anchor_right: 0,
                anchor_bottom: 0,
                items: Vec::new(),
                hover: None,
                tracking: false,
                hits: Vec::new(),
            })
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Same click either opens the grid or, if a fresh dismissal just closed
    /// it, swallows the reopen (see Flyout::just_dismissed).
    pub fn toggle(&mut self, srcs: Vec<Src>, anchor_right: i32, anchor_bottom: i32) {
        if self.open {
            self.hide();
        } else if !self.just_dismissed() {
            self.show(srcs, anchor_right, anchor_bottom);
        }
    }

    fn show(&mut self, srcs: Vec<Src>, anchor_right: i32, anchor_bottom: i32) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, self as *mut TrayOverflow as isize);
        }
        crate::clickaway::set_overflow_open(true);
        self.load(srcs);
        self.anchor_right = anchor_right;
        self.anchor_bottom = anchor_bottom;
        self.open = true;
        self.hover = None;
        self.relayout();
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
            SetTimer(Some(self.hwnd), TIMER_REFRESH, 1000, None);
        }
    }

    /// Re-rasterise each icon on our own dc from its HICON.
    fn load(&mut self, srcs: Vec<Src>) {
        self.items = srcs
            .into_iter()
            .map(|s| Item {
                owner: s.owner,
                uid: s.uid,
                callback: s.callback,
                version: s.version,
                bitmap: crate::icons::hicon_bitmap(
                    &self.renderer.dc,
                    windows::Win32::UI::WindowsAndMessaging::HICON(s.hicon as *mut _),
                ),
            })
            .collect();
    }

    pub fn hide(&mut self) {
        crate::clickaway::set_overflow_open(false);
        if self.open {
            self.open = false;
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_REFRESH);
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        self.hover = None;
        self.items.clear();
    }

    pub fn dismiss(&mut self) {
        if self.open {
            self.last_dismissed = Some(std::time::Instant::now());
        }
        self.hide();
    }

    fn just_dismissed(&self) -> bool {
        self.last_dismissed.is_some_and(|t| t.elapsed().as_millis() < 400)
    }

    fn cols(&self) -> usize {
        self.items.len().clamp(1, COLS)
    }

    fn rows(&self) -> usize {
        self.items.len().div_ceil(self.cols()).max(1)
    }

    fn relayout(&mut self) {
        let cols = self.cols();
        let rows = self.rows();
        self.w = PAD * 2.0 + cols as f32 * CELL;
        self.h = HEADER_H + rows as f32 * CELL + PAD * 2.0;
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

    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        self.hits
            .iter()
            .find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
            .map(|(_, i)| *i)
    }

    /// Left click: act then close, matching the stock overflow flyout.
    fn click(&mut self, idx: usize) {
        let Some(it) = self.items.get(idx) else { return };
        forward(it.owner, it.uid, it.callback, it.version, WM_LBUTTONUP);
        if it.version >= 4 {
            forward(it.owner, it.uid, it.callback, it.version, NIN_SELECT);
        }
        self.hide();
    }

    fn context(&mut self, idx: usize) {
        let Some(it) = self.items.get(idx) else { return };
        forward(it.owner, it.uid, it.callback, it.version, WM_RBUTTONDOWN);
        forward(it.owner, it.uid, it.callback, it.version, WM_RBUTTONUP);
        if it.version >= 4 {
            forward(it.owner, it.uid, it.callback, it.version, WM_CONTEXTMENU);
        }
        // The owner's menu takes foreground → WA_INACTIVE will dismiss us.
    }

    fn paint(&mut self) {
        if !self.open {
            return;
        }
        self.hits.clear();
        unsafe {
            let r = &self.renderer;
            r.dc.BeginDraw();
            r.dc.Clear(Some(&theme::rgba(26, 27, 32, 0.9)));
            if let Ok(b) = r.brush(theme::with_alpha(theme::accent(), 0.25)) {
                r.dc.FillRectangle(&rect(0.0, 0.0, self.w, 1.0 / self.scale), &b);
            }
        }
        self.text(
            "숨긴 아이콘",
            &self.renderer.fmt_status.clone(),
            rect(PAD, 2.0, self.w - PAD, HEADER_H),
            theme::TEXT_DIM,
        );
        let cols = self.cols();
        let n = self.items.len();
        for i in 0..n {
            let col = i % cols;
            let row = i / cols;
            let cx = PAD + col as f32 * CELL;
            let cy = HEADER_H + row as f32 * CELL;
            let cell_r = rect(cx, cy, cx + CELL, cy + CELL);
            if self.hover == Some(i) {
                self.fill_round(
                    rect(cx + 1.0, cy + 1.0, cx + CELL - 1.0, cy + CELL - 1.0),
                    4.0,
                    theme::HOVER_FILL,
                );
            }
            let icon_r = rect(
                cx + (CELL - 16.0) / 2.0,
                cy + (CELL - 16.0) / 2.0,
                cx + (CELL + 16.0) / 2.0,
                cy + (CELL + 16.0) / 2.0,
            );
            unsafe {
                if let Some(bmp) = &self.items[i].bitmap {
                    self.renderer.dc.DrawBitmap(
                        bmp,
                        Some(&icon_r),
                        1.0,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        None,
                        None,
                    );
                } else if let Ok(b) = self.renderer.brush(theme::with_alpha(theme::TEXT_DIM, 0.6)) {
                    self.renderer.dc.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT { rect: icon_r, radiusX: 8.0, radiusY: 8.0 },
                        &b,
                    );
                }
            }
            self.hits.push((cell_r, i));
        }
        unsafe {
            let _ = self.renderer.dc.EndDraw(None, None);
            let _ = self.renderer.present();
        }
    }

    fn fill_round(&self, r: D2D_RECT_F, radius: f32, c: D2D1_COLOR_F) {
        fill_round(&self.renderer, r, radius, c);
    }

    fn text(
        &self,
        s: &str,
        fmt: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
        r: D2D_RECT_F,
        c: windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F,
    ) {
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
}

extern "system" fn overflow_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayOverflow;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let ov = &mut *ptr;
        let lx = |o: &TrayOverflow| (lparam.0 & 0xFFFF) as i16 as f32 / o.scale;
        let ly = |o: &TrayOverflow| ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 / o.scale;
        match msg {
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                ov.paint();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_ACTIVATE => {
                if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    ov.dismiss();
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = (lx(ov), ly(ov));
                let h = ov.hit(x, y);
                if h != ov.hover {
                    ov.hover = h;
                    ov.paint();
                }
                if !ov.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    if TrackMouseEvent(&mut tme).is_ok() {
                        ov.tracking = true;
                    }
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                ov.tracking = false;
                if ov.hover.take().is_some() {
                    ov.paint();
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let (x, y) = (lx(ov), ly(ov));
                if let Some(i) = ov.hit(x, y) {
                    ov.click(i);
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let (x, y) = (lx(ov), ly(ov));
                if let Some(i) = ov.hit(x, y) {
                    ov.context(i);
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wparam.0 == VK_ESCAPE.0 as usize {
                    ov.hide();
                }
                LRESULT(0)
            }
            WM_TIMER => {
                // Owner may have died; drop icons whose window is gone.
                if wparam.0 == TIMER_REFRESH {
                    let before = ov.items.len();
                    ov.items.retain(|it| IsWindow(Some(it.owner)).as_bool());
                    if ov.items.is_empty() {
                        ov.hide();
                    } else if ov.items.len() != before {
                        ov.relayout();
                    }
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
