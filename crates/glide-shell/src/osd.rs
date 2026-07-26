//! Volume + brightness OSD (SHELL_DESIGN §6.7). An
//! IAudioEndpointVolumeCallback fires on any master-volume change — hardware
//! keys, the tray wheel, other apps — on an arbitrary COM thread; OnNotify
//! only posts the new level to the OSD window, so idle cost is zero (the
//! poll alternative would wake 10×/s forever on a battery machine).
//! Brightness rides the same pill: a worker thread blocks on a WMI
//! `WmiMonitorBrightnessEvent` notification query (fires for hotkeys and
//! ms-settings alike) and posts the new percent. The pill renders
//! bottom-center above the work area, never activates, hit-tests
//! transparent, and fades out 1.5s after the last change. Owns its own
//! enumerator/endpoint instead of borrowing status.rs's so neither module
//! depends on the other's lifecycle.

use std::time::Instant;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ROUNDED_RECT};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER, IDWriteTextFormat,
};
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::{
    AUDIO_VOLUME_NOTIFICATION_DATA, IMMDeviceEnumerator, MMDeviceEnumerator, eMultimedia, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoSetProxyBlanket, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Variant::{VARIANT, VT_I4, VT_UI1};
use windows::Win32::System::Wmi::{
    IWbemLocator, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_INFINITE,
    WbemLocator,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BSTR, PCWSTR, implement, w};

use crate::render::{Renderer, fill_round, rect};
use crate::theme;

const WM_APP_VOL: u32 = WM_APP + 12;
const WM_APP_BRIGHT: u32 = WM_APP + 13;
/// Posted by the volume flyout after switching the default endpoint: our
/// change subscription still points at the old device, resubscribe.
pub const WM_APP_REBIND: u32 = WM_APP + 16;
const TIMER_HIDE: usize = 1;
const TIMER_ANIM: usize = 2;
/// RPC_C_AUTHN_WINNT — the constant lives in Win32_System_Rpc; not worth the
/// feature for one u32.
const AUTHN_WINNT: u32 = 10;

const OSD_W: f32 = 280.0;
const OSD_H: f32 = 48.0;
const MARGIN: f32 = 24.0;
const HIDE_MS: u32 = 1500;
const ANIM_SECS: f32 = 0.15;

/// COM callback: arbitrary thread, so it only posts to the OSD window.
#[implement(IAudioEndpointVolumeCallback)]
struct VolWatch {
    hwnd: isize,
}

impl IAudioEndpointVolumeCallback_Impl for VolWatch_Impl {
    fn OnNotify(
        &self,
        pnotify: *mut AUDIO_VOLUME_NOTIFICATION_DATA,
    ) -> windows::core::Result<()> {
        unsafe {
            if let Some(d) = pnotify.as_ref() {
                let _ = PostMessageW(
                    Some(HWND(self.hwnd as *mut _)),
                    WM_APP_VOL,
                    WPARAM(d.bMuted.as_bool() as usize),
                    LPARAM((d.fMasterVolume * 1000.0) as isize),
                );
            }
        }
        Ok(())
    }
}

/// Which reading the pill currently shows.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Vol,
    Bright,
}

pub struct Osd {
    hwnd: HWND,
    renderer: Renderer,
    fmt_glyph: IDWriteTextFormat,
    fmt_pct: IDWriteTextFormat,
    scale: f32,
    mode: Mode,
    vol: f32,
    muted: bool,
    /// Brightness percent, 0..=100.
    bright: u32,
    /// 0→1 fade-in; runs back 1→0 when `closing`.
    state: f32,
    closing: bool,
    shown: bool,
    last_anim: Instant,
    anim_timer: bool,
    /// While this window (the volume flyout) is visible, volume bumps stay
    /// silent — the slider is already on screen showing the same number.
    quiet_peer: isize,
    /// Keep the COM pair alive for the process lifetime.
    _endpoint: Option<IAudioEndpointVolume>,
    _callback: Option<IAudioEndpointVolumeCallback>,
}

impl Osd {
    pub fn new(dpi: f32) -> anyhow::Result<Self> {
        unsafe {
            let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            let class = w!("glide_shell_osd");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(osd_wndproc),
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
            let fmt_glyph = mk(w!("Segoe Fluent Icons"), 16.0, DWRITE_FONT_WEIGHT_NORMAL)
                .or_else(|_| mk(w!("Segoe MDL2 Assets"), 16.0, DWRITE_FONT_WEIGHT_NORMAL))?;
            fmt_glyph.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_glyph.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_pct = mk(w!("Segoe UI Variable"), 12.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)
                .or_else(|_| mk(w!("Segoe UI"), 12.0, DWRITE_FONT_WEIGHT_SEMI_BOLD))?;
            fmt_pct.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_pct.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            Ok(Osd {
                hwnd,
                renderer,
                fmt_glyph,
                fmt_pct,
                scale: dpi / 96.0,
                mode: Mode::Vol,
                vol: 0.0,
                muted: false,
                bright: 0,
                state: 0.0,
                closing: false,
                shown: false,
                last_anim: Instant::now(),
                anim_timer: false,
                quiet_peer: 0,
                _endpoint: None,
                _callback: None,
            })
        }
    }

    /// Late GWLP_USERDATA arm + volume-change subscription; `run()` calls
    /// this once the struct address is final (the callback needs the hwnd,
    /// and the hwnd must already dispatch into an armed struct).
    pub fn arm(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, self as *mut Osd as isize);
        }
        self.subscribe();
        // Brightness watcher. The blocking Next() has no clean cancel; the
        // thread dies with the process, which is when we'd want it gone.
        let hwnd_raw = self.hwnd.0 as isize;
        std::thread::spawn(move || bright_worker(hwnd_raw));
    }

    pub fn disarm(&mut self) {
        if let (Some(ep), Some(cb)) = (&self._endpoint, &self._callback) {
            unsafe {
                let _ = ep.UnregisterControlChangeNotify(cb);
            }
        }
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
        }
    }

    /// (Re)subscribe to master-volume changes on the current default render
    /// endpoint. WM_APP_REBIND lands here after a default-device switch.
    fn subscribe(&mut self) {
        if let (Some(ep), Some(cb)) = (&self._endpoint, &self._callback) {
            unsafe {
                let _ = ep.UnregisterControlChangeNotify(cb);
            }
        }
        self._endpoint = None;
        self._callback = None;
        let hooked = (|| -> windows::core::Result<(IAudioEndpointVolume, IAudioEndpointVolumeCallback)> {
            unsafe {
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
                let endpoint: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None)?;
                let callback: IAudioEndpointVolumeCallback =
                    VolWatch { hwnd: self.hwnd.0 as isize }.into();
                endpoint.RegisterControlChangeNotify(&callback)?;
                Ok((endpoint, callback))
            }
        })();
        match hooked {
            Ok((ep, cb)) => {
                self._endpoint = Some(ep);
                self._callback = Some(cb);
            }
            Err(e) => crate::safety::note(&format!("volume OSD subscription failed: {e:?}")),
        }
    }

    pub fn set_quiet_peer(&mut self, hwnd: HWND) {
        self.quiet_peer = hwnd.0 as isize;
    }

    /// A volume change arrived: update, (re)show, restart the hide clock.
    fn bump(&mut self, vol: f32, muted: bool) {
        if self.quiet_peer != 0
            && unsafe { IsWindowVisible(HWND(self.quiet_peer as *mut _)) }.as_bool()
        {
            return;
        }
        self.mode = Mode::Vol;
        self.vol = vol.clamp(0.0, 1.0);
        self.muted = muted;
        self.reveal();
    }

    fn bump_bright(&mut self, pct: u32) {
        self.mode = Mode::Bright;
        self.bright = pct.min(100);
        self.reveal();
    }

    fn reveal(&mut self) {
        self.closing = false;
        if !self.shown {
            self.shown = true;
            let mut work = windows::Win32::Foundation::RECT::default();
            unsafe {
                let _ = SystemParametersInfoW(
                    SPI_GETWORKAREA,
                    0,
                    Some(&mut work as *mut _ as *mut _),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                );
            }
            let wd = (OSD_W * self.scale).round() as i32;
            let hd = (OSD_H * self.scale).round() as i32;
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    work.left + (work.right - work.left - wd) / 2,
                    work.bottom - (MARGIN * self.scale).round() as i32 - hd,
                    wd,
                    hd,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
            let _ = self
                .renderer
                .resize(wd as u32, hd as u32, self.scale * 96.0);
        }
        self.paint();
        self.ensure_anim();
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_HIDE, HIDE_MS, None);
        }
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

    /// Drive the fade; returns whether another frame is needed.
    fn step_anim(&mut self) -> bool {
        let dt = self.last_anim.elapsed().as_secs_f32().min(0.1);
        self.last_anim = Instant::now();
        let step = dt / ANIM_SECS;
        let target = if self.closing { 0.0 } else { 1.0 };
        self.state = if self.closing {
            (self.state - step).max(0.0)
        } else {
            (self.state + step).min(1.0)
        };
        if self.closing && self.state <= 0.0 {
            self.shown = false;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            return false;
        }
        self.paint();
        self.state != target
    }

    fn paint(&mut self) {
        let a = 1.0 - (1.0 - self.state) * (1.0 - self.state);
        let fade = |col: D2D1_COLOR_F| theme::with_alpha(col, col.a * a);
        unsafe {
            let r = &self.renderer;
            r.dc.BeginDraw();
            r.dc.Clear(Some(&theme::rgba(0, 0, 0, 0.0)));

            let rc = rect(0.0, 0.0, OSD_W, OSD_H);
            self.fill_round(rc, 10.0, fade(theme::rgba(30, 31, 37, 0.97)));
            if let Ok(b) = r.brush(fade(theme::rgba(255, 255, 255, 0.09))) {
                r.dc.DrawRoundedRectangle(
                    &D2D1_ROUNDED_RECT { rect: rc, radiusX: 10.0, radiusY: 10.0 },
                    &b,
                    1.0,
                    None,
                );
            }

            // Sun for brightness; the volume glyph tracks level and mute.
            let (glyph, frac, dim_fill) = match self.mode {
                Mode::Vol => (
                    crate::status::volume_glyph(Some((self.vol, self.muted))),
                    self.vol,
                    self.muted,
                ),
                Mode::Bright => (0xE706u16, self.bright as f32 / 100.0, false),
            };
            self.text(&[glyph], &self.fmt_glyph, rect(10.0, 0.0, 46.0, OSD_H), fade(theme::TEXT));

            // Track + fill; mute dims the fill instead of hiding it.
            let (tx0, tx1) = (54.0, OSD_W - 52.0);
            let cy = OSD_H / 2.0;
            self.fill_round(
                rect(tx0, cy - 2.0, tx1, cy + 2.0),
                2.0,
                fade(theme::with_alpha(theme::TEXT_DIM, 0.3)),
            );
            let fill = if dim_fill {
                fade(theme::with_alpha(theme::TEXT_DIM, 0.6))
            } else {
                fade(theme::accent())
            };
            let fx = tx0 + (tx1 - tx0) * frac;
            if fx > tx0 {
                self.fill_round(rect(tx0, cy - 2.0, fx, cy + 2.0), 2.0, fill);
            }

            let pct: Vec<u16> = format!("{}", (frac * 100.0).round() as u32)
                .encode_utf16()
                .collect();
            self.text(
                &pct,
                &self.fmt_pct,
                rect(OSD_W - 46.0, 0.0, OSD_W - 10.0, OSD_H),
                fade(theme::TEXT),
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
        fill_round(&self.renderer, rc, radius, color);
    }
}

/// Blocks forever on a WMI notification query; every brightness change on
/// any monitor posts its percent to the OSD window.
fn bright_worker(hwnd_raw: isize) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let r = (|| -> windows::core::Result<()> {
            let locator: IWbemLocator =
                CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
            let services = locator.ConnectServer(
                &BSTR::from(r"root\wmi"),
                &BSTR::new(),
                &BSTR::new(),
                &BSTR::new(),
                0,
                &BSTR::new(),
                None,
            )?;
            CoSetProxyBlanket(
                &services,
                AUTHN_WINNT,
                0,
                None,
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            )?;
            let rows = services.ExecNotificationQuery(
                &BSTR::from("WQL"),
                &BSTR::from("SELECT * FROM WmiMonitorBrightnessEvent"),
                WBEM_FLAG_RETURN_IMMEDIATELY | WBEM_FLAG_FORWARD_ONLY,
                None,
            )?;
            loop {
                let mut obj = [None; 1];
                let mut got = 0u32;
                let _ = rows.Next(WBEM_INFINITE, &mut obj, &mut got);
                let Some(obj) = obj[0].take() else { continue };
                if got == 0 {
                    continue;
                }
                let mut v = VARIANT::default();
                if obj.Get(w!("Brightness"), 0, &mut v, None, None).is_ok() {
                    let vt = v.Anonymous.Anonymous.vt;
                    let pct = if vt == VT_UI1 {
                        Some(v.Anonymous.Anonymous.Anonymous.bVal as u32)
                    } else if vt == VT_I4 {
                        Some(v.Anonymous.Anonymous.Anonymous.lVal as u32)
                    } else {
                        None
                    };
                    if let Some(pct) = pct {
                        let _ = PostMessageW(
                            Some(HWND(hwnd_raw as *mut _)),
                            WM_APP_BRIGHT,
                            WPARAM(0),
                            LPARAM(pct as isize),
                        );
                    }
                }
            }
        })();
        if let Err(e) = r {
            crate::safety::note(&format!("brightness OSD subscription failed: {e:?}"));
        }
    }
}

extern "system" fn osd_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Osd;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let o = &mut *ptr;
        match msg {
            WM_PAINT => {
                let _ = windows::Win32::Graphics::Gdi::ValidateRect(Some(hwnd), None);
                o.paint();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            // Display-only: everything falls through to whatever is beneath.
            WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
            WM_APP_VOL => {
                let muted = wparam.0 != 0;
                let vol = lparam.0 as f32 / 1000.0;
                o.bump(vol, muted);
                LRESULT(0)
            }
            WM_APP_BRIGHT => {
                o.bump_bright(lparam.0 as u32);
                LRESULT(0)
            }
            WM_APP_REBIND => {
                o.subscribe();
                LRESULT(0)
            }
            WM_TIMER => {
                match wparam.0 {
                    TIMER_HIDE => {
                        let _ = KillTimer(Some(hwnd), TIMER_HIDE);
                        if o.shown {
                            o.closing = true;
                            o.ensure_anim();
                        }
                    }
                    TIMER_ANIM => {
                        if !o.step_anim() {
                            o.anim_timer = false;
                            let _ = KillTimer(Some(hwnd), TIMER_ANIM);
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
