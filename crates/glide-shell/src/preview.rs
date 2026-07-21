//! Near-bar popup: hover preview for taskbar buttons (full title + DWM live
//! thumbnail) and text tips for tray/status cells. First user of the popup
//! machinery that balloons, toast cards and flyouts reuse later — acrylic
//! dark, DWM-rounded, per the §5 design language.

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::D2D1_DRAW_TEXT_OPTIONS_CLIP;
use windows::Win32::Graphics::DirectWrite::DWRITE_MEASURING_MODE_NATURAL;
use windows::Win32::Graphics::Dwm::{
    DWM_THUMBNAIL_PROPERTIES, DWM_TNP_RECTDESTINATION, DWM_TNP_SOURCECLIENTAREAONLY,
    DWM_TNP_VISIBLE, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmQueryThumbnailSourceSize,
    DwmRegisterThumbnail, DwmSetWindowAttribute, DwmUnregisterThumbnail,
    DwmUpdateThumbnailProperties,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use crate::render::Renderer;
use crate::theme;

const PAD: f32 = 12.0;
const TITLE_H: f32 = 22.0;
const THUMB_MAX_W: f32 = 300.0;
const THUMB_MAX_H: f32 = 188.0;
const TIP_MAX_W: f32 = 340.0;
const GAP_ABOVE_BAR: i32 = 10;

/// What the popup currently shows, for retarget dedup.
#[derive(Clone, Copy, PartialEq)]
pub enum Target {
    Window(isize),
    Tray(usize),
    Status(usize),
    Launcher(usize),
}

pub struct Preview {
    hwnd: HWND,
    renderer: Renderer,
    thumb: Option<isize>,
    pub current: Option<Target>,
    scale: f32,
}

impl Preview {
    pub fn new(dpi: f32) -> anyhow::Result<Self> {
        unsafe {
            let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            let class = w!("glide_shell_popup");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(popup_wndproc),
                hInstance: hinstance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc); // 0 on re-register is fine
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOPMOST
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOREDIRECTIONBITMAP,
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
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &dark as *const _ as _,
                4,
            );
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
            Ok(Preview { hwnd, renderer, thumb: None, current: None, scale: dpi / 96.0 })
        }
    }

    /// Live-thumbnail preview for a taskbar button's window.
    pub fn show_window(
        &mut self,
        target: Target,
        src: HWND,
        title: &[u16],
        center_x: i32,
        bar_top: i32,
    ) {
        if self.current == Some(target) {
            return;
        }
        unsafe {
            self.drop_thumb();
            let Ok(thumb) = DwmRegisterThumbnail(self.hwnd, src) else {
                self.show_tip(target, title, center_x, bar_top);
                return;
            };
            let src_sz = DwmQueryThumbnailSourceSize(thumb).unwrap_or_default();
            if src_sz.cx <= 0 || src_sz.cy <= 0 {
                let _ = DwmUnregisterThumbnail(thumb);
                self.show_tip(target, title, center_x, bar_top);
                return;
            }
            // Fit the source into the max box (device px), downscale only.
            let max_w = THUMB_MAX_W * self.scale;
            let max_h = THUMB_MAX_H * self.scale;
            let fit = (max_w / src_sz.cx as f32).min(max_h / src_sz.cy as f32).min(1.0);
            let tw = (src_sz.cx as f32 * fit).round() as i32;
            let th = (src_sz.cy as f32 * fit).round() as i32;

            let pad = (PAD * self.scale).round() as i32;
            let title_h = (TITLE_H * self.scale).round() as i32;
            let gap = (6.0 * self.scale) as i32;
            let min_w = (160.0 * self.scale) as i32;
            let w = (tw + pad * 2).max(min_w);
            let h = pad + title_h + gap + th + pad;
            let thumb_left = pad + (w - pad * 2 - tw) / 2;
            let thumb_top = pad + title_h + gap;

            self.place_and_paint(w, h, center_x, bar_top, title);

            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: RECT {
                    left: thumb_left,
                    top: thumb_top,
                    right: thumb_left + tw,
                    bottom: thumb_top + th,
                },
                rcSource: RECT::default(),
                opacity: 255,
                fVisible: true.into(),
                fSourceClientAreaOnly: false.into(),
            };
            let _ = DwmUpdateThumbnailProperties(thumb, &props);
            self.thumb = Some(thumb);
            self.current = Some(target);
        }
    }

    /// Plain text tip (tray icons, status cells, pinned launchers).
    pub fn show_tip(&mut self, target: Target, text: &[u16], center_x: i32, bar_top: i32) {
        if self.current == Some(target) || text.is_empty() {
            return;
        }
        unsafe {
            self.drop_thumb();
        }
        let text_w = self
            .renderer
            .text_width(text, &self.renderer.fmt_title, TIP_MAX_W * self.scale)
            .min(TIP_MAX_W);
        let pad = (PAD * self.scale).round() as i32;
        let w = ((text_w + 2.0) * self.scale) as i32 + pad * 2;
        let h = (TITLE_H * self.scale) as i32 + pad * 2 - (6.0 * self.scale) as i32;
        self.place_and_paint(w, h, center_x, bar_top, text);
        self.current = Some(target);
    }

    fn place_and_paint(&mut self, w: i32, h: i32, center_x: i32, bar_top: i32, title: &[u16]) {
        unsafe {
            let screen_w = GetSystemMetrics(SM_CXSCREEN);
            let x = (center_x - w / 2).clamp(8, (screen_w - w - 8).max(8));
            let y = bar_top - h - GAP_ABOVE_BAR;
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let dpi = self.scale * 96.0;
            let _ = self.renderer.resize(w as u32, h as u32, dpi);

            let r = &self.renderer;
            let lw = w as f32 / self.scale;
            r.dc.BeginDraw();
            r.dc.Clear(Some(&theme::rgba(26, 27, 32, 0.86)));
            if let Ok(b) = r.brush(theme::with_alpha(theme::ACCENT, 0.25)) {
                r.dc.FillRectangle(
                    &D2D_RECT_F { left: 0.0, top: 0.0, right: lw, bottom: 1.0 / self.scale },
                    &b,
                );
            }
            if let Ok(b) = r.brush(theme::TEXT) {
                r.dc.DrawText(
                    title,
                    &r.fmt_title,
                    &D2D_RECT_F {
                        left: PAD,
                        top: PAD - 4.0,
                        right: lw - PAD,
                        bottom: PAD + TITLE_H,
                    },
                    &b,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
            let _ = r.dc.EndDraw(None, None);
            let _ = r.present();
        }
    }

    pub fn hide(&mut self) {
        unsafe {
            self.drop_thumb();
            if self.current.is_some() {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.current = None;
        }
    }

    unsafe fn drop_thumb(&mut self) {
        if let Some(t) = self.thumb.take() {
            unsafe {
                let _ = DwmUnregisterThumbnail(t);
            }
        }
    }
}

extern "system" fn popup_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
