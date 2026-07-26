//! Win10-style flyout panels for the status cluster: volume slider, Wi-Fi
//! list with a radio toggle, battery detail (SHELL_DESIGN §6.6). Second user
//! of the popup machinery — unlike preview.rs this window takes input: it is
//! activatable, and dismisses itself when it loses foreground, Esc, or when
//! its status cell is clicked again.

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ELLIPSE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER,
    DWRITE_WORD_WRAPPING_NO_WRAP, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dwm::{
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, EDataFlow, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture,
    eMultimedia, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToString;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::Power::GetSystemPowerStatus;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};
use windows_numerics::Vector2;

use crate::render::{Renderer, fill_round, rect};
use crate::theme;

// Not exported by WindowsAndMessaging (it's a TrackMouseEvent notification);
// without a const the match arm silently becomes a catch-all binding.
const WM_MOUSELEAVE: u32 = 0x02A3;

const TIMER_REFRESH: usize = 1;
const W: f32 = 344.0;
const ROW_H: f32 = 42.0;
/// Audio device rows (volume panel) are a little tighter than Wi-Fi rows.
const DEV_ROW_H: f32 = 34.0;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Volume,
    Network,
    Battery,
    Calendar,
}

#[derive(Clone, Copy, PartialEq)]
enum Act {
    Mute,
    Slider,
    WifiToggle,
    Row(usize),
    OutDev(usize),
    InDev(usize),
    NetSettings,
    PowerSettings,
    CalPrev,
    CalNext,
    CalToday,
}

/// A device list drawn under one heading: the heading, its (name, is-default)
/// rows, and the Act constructor that turns a row index into a click target.
type DeviceSection = (&'static str, Vec<(String, bool)>, fn(usize) -> Act);

/// One render/capture endpoint in the volume panel.
struct AudioDev {
    /// Null-terminated MMDevice endpoint ID, ready for IPolicyConfig.
    id: Vec<u16>,
    name: String,
    default: bool,
}

pub struct Flyout {
    hwnd: HWND,
    renderer: Renderer,
    fmt_head: IDWriteTextFormat,
    fmt_big: IDWriteTextFormat,
    fmt_pct: IDWriteTextFormat,
    fmt_g16: IDWriteTextFormat,
    fmt_g30: IDWriteTextFormat,
    pub kind: Option<Kind>,
    /// Set by dismiss(): a real click's mouse-down killed the panel via
    /// WA_INACTIVE/click-away before the click's UP reached the bar. The
    /// bar's toggle consults this so that UP doesn't reopen what its own
    /// DOWN just closed.
    last_dismissed: Option<(Kind, std::time::Instant)>,
    scale: f32,
    w: f32,
    h: f32,
    anchor_right: i32,
    anchor_bottom: i32,
    fg_at_open: HWND,
    hover: Option<Act>,
    tracking: bool,
    dragging: bool,
    hits: Vec<(D2D_RECT_F, Act)>,
    slider: D2D_RECT_F,
    // volume
    endpoint: Option<IAudioEndpointVolume>,
    vol: Option<(f32, bool)>,
    device: String,
    outs: Vec<AudioDev>,
    ins: Vec<AudioDev>,
    // network
    wifi: Option<crate::wifi::Wifi>,
    nets: Vec<crate::wifi::Net>,
    radio_on: bool,
    connecting: Option<String>,
    // battery
    battery: Option<(u8, bool)>,
    charging: bool,
    // calendar (viewed month, not necessarily today's)
    cal_year: i32,
    cal_month: u32,
}

impl Flyout {
    pub fn new(dpi: f32) -> anyhow::Result<Self> {
        unsafe {
            let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            let class = w!("glide_shell_flyout");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(flyout_wndproc),
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
            let fmt_head = mk(family, 13.5, DWRITE_FONT_WEIGHT_SEMI_BOLD)
                .or_else(|_| mk(w!("Segoe UI"), 13.5, DWRITE_FONT_WEIGHT_SEMI_BOLD))?;
            fmt_head.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            fmt_head.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_big = mk(family, 26.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)
                .or_else(|_| mk(w!("Segoe UI"), 26.0, DWRITE_FONT_WEIGHT_SEMI_BOLD))?;
            fmt_big.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            fmt_big.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_pct = mk(family, 13.5, DWRITE_FONT_WEIGHT_SEMI_BOLD)
                .or_else(|_| mk(w!("Segoe UI"), 13.5, DWRITE_FONT_WEIGHT_SEMI_BOLD))?;
            fmt_pct.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_pct.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let mkg = |size: f32| {
                mk(w!("Segoe Fluent Icons"), size, DWRITE_FONT_WEIGHT_NORMAL)
                    .or_else(|_| mk(w!("Segoe MDL2 Assets"), size, DWRITE_FONT_WEIGHT_NORMAL))
            };
            let fmt_g16 = mkg(16.0)?;
            fmt_g16.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_g16.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_g30 = mkg(30.0)?;
            fmt_g30.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_g30.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            Ok(Flyout {
                hwnd,
                renderer,
                fmt_head,
                fmt_big,
                fmt_pct,
                fmt_g16,
                fmt_g30,
                kind: None,
                last_dismissed: None,
                scale: dpi / 96.0,
                w: 0.0,
                h: 0.0,
                anchor_right: 0,
                anchor_bottom: 0,
                fg_at_open: HWND::default(),
                hover: None,
                tracking: false,
                dragging: false,
                hits: Vec::new(),
                slider: rect(0.0, 0.0, 0.0, 0.0),
                endpoint: None,
                vol: None,
                device: String::new(),
                outs: Vec::new(),
                ins: Vec::new(),
                wifi: None,
                nets: Vec::new(),
                radio_on: false,
                connecting: None,
                battery: None,
                charging: false,
                cal_year: 2000,
                cal_month: 1,
            })
        }
    }

    /// For siblings that want to stay quiet while a panel is up (the OSD).
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn open(&mut self, kind: Kind, bar_rect: RECT) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, self as *mut Flyout as isize);
        }
        crate::clickaway::set_flyout_open(true);
        self.kind = Some(kind);
        self.hover = None;
        self.dragging = false;
        self.connecting = None;
        match kind {
            Kind::Volume => {
                self.poll_volume();
                self.device = device_name().unwrap_or_else(|| "스피커".to_string());
                self.poll_devices();
            }
            Kind::Network => {
                if self.wifi.is_none() {
                    self.wifi = crate::wifi::Wifi::open();
                }
                match &self.wifi {
                    Some(wf) => {
                        wf.scan();
                        self.radio_on = wf.radio_on();
                        self.nets = if self.radio_on { wf.networks() } else { Vec::new() };
                    }
                    None => {
                        self.radio_on = false;
                        self.nets = Vec::new();
                    }
                }
            }
            Kind::Battery => self.poll_battery(),
            Kind::Calendar => {
                let now = chrono::Local::now();
                self.cal_year = chrono::Datelike::year(&now);
                self.cal_month = chrono::Datelike::month(&now);
            }
        }
        self.anchor_right = bar_rect.right - (12.0 * self.scale) as i32;
        self.anchor_bottom = bar_rect.top - (10.0 * self.scale) as i32;
        unsafe {
            self.fg_at_open = GetForegroundWindow();
        }
        self.relayout();
        unsafe {
            // We received the status-cell click, so we hold the foreground
            // right; Win10 flyouts take focus and die on losing it.
            let _ = SetForegroundWindow(self.hwnd);
            SetTimer(Some(self.hwnd), TIMER_REFRESH, 1000, None);
        }
    }

    pub fn hide(&mut self) {
        crate::clickaway::set_flyout_open(false);
        if self.kind.take().is_some() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_REFRESH);
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        self.dragging = false;
        self.hover = None;
    }

    /// Hide caused by the user clicking elsewhere (WA_INACTIVE, click-away)
    /// rather than by a toggle — stamps the kind so the same click's UP on
    /// the status cell doesn't reopen the panel it just closed.
    pub fn dismiss(&mut self) {
        if let Some(k) = self.kind {
            self.last_dismissed = Some((k, std::time::Instant::now()));
        }
        self.hide();
    }

    /// True while a fresh dismissal of `kind` should swallow the toggle.
    pub fn just_dismissed(&self, kind: Kind) -> bool {
        self.last_dismissed
            .is_some_and(|(k, t)| k == kind && t.elapsed().as_millis() < 400)
    }

    fn measure(&self) -> (f32, f32) {
        match self.kind {
            Some(Kind::Volume) => {
                // 출력/입력 device sections above the slider block.
                let sect = |n: usize| {
                    if n == 0 { 0.0 } else { 24.0 + n as f32 * DEV_ROW_H + 6.0 }
                };
                (W, 8.0 + sect(self.outs.len()) + sect(self.ins.len()) + 6.0 + 36.0 + 8.0)
            }
            Some(Kind::Network) => {
                let rows = if self.radio_on { self.nets.len().max(1) } else { 1 };
                (W, 56.0 + rows as f32 * ROW_H + 17.0 + 40.0 + 8.0)
            }
            Some(Kind::Battery) => (W, 151.0),
            Some(Kind::Calendar) => (W, 366.0),
            None => (64.0, 64.0),
        }
    }

    /// Size to content, keep the bottom-right corner anchored above the
    /// status cluster, repaint.
    fn relayout(&mut self) {
        let (w, h) = self.measure();
        self.w = w;
        self.h = h;
        let wd = (w * self.scale).round() as i32;
        let hd = (h * self.scale).round() as i32;
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

    // ---- polling ----------------------------------------------------------

    fn poll_volume(&mut self) {
        unsafe {
            if self.endpoint.is_none() {
                self.endpoint = (|| -> windows::core::Result<IAudioEndpointVolume> {
                    let enumerator: IMMDeviceEnumerator =
                        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                    let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
                    device.Activate(CLSCTX_ALL, None)
                })()
                .ok();
            }
            let Some(ep) = &self.endpoint else {
                self.vol = None;
                return;
            };
            match (ep.GetMasterVolumeLevelScalar(), ep.GetMute()) {
                (Ok(v), Ok(m)) => self.vol = Some((v, m.as_bool())),
                _ => {
                    self.endpoint = None;
                    self.vol = None;
                }
            }
        }
    }

    /// Enumerate active render/capture endpoints and mark the defaults.
    fn poll_devices(&mut self) {
        unsafe {
            let Ok(enumerator) =
                CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
            else {
                return;
            };
            let list = |flow: EDataFlow| -> Vec<AudioDev> {
                let take_id = |p: windows::core::PWSTR| {
                    let v = p.as_wide().to_vec();
                    CoTaskMemFree(Some(p.0 as _));
                    v
                };
                let default_id = enumerator
                    .GetDefaultAudioEndpoint(flow, eMultimedia)
                    .ok()
                    .and_then(|d| d.GetId().ok().map(take_id))
                    .unwrap_or_default();
                let mut devs = Vec::new();
                let Ok(col) = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else {
                    return devs;
                };
                for i in 0..col.GetCount().unwrap_or(0) {
                    let Ok(dev) = col.Item(i) else { continue };
                    let Ok(idp) = dev.GetId() else { continue };
                    let mut id = take_id(idp);
                    let default = id == default_id;
                    let name = dev
                        .OpenPropertyStore(STGM_READ)
                        .ok()
                        .and_then(|ps| ps.GetValue(&PKEY_Device_FriendlyName).ok())
                        .and_then(|pv| {
                            let mut buf = [0u16; 128];
                            PropVariantToString(&pv, &mut buf).ok()?;
                            let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
                            let s = String::from_utf16_lossy(&buf[..len]);
                            (!s.is_empty()).then_some(s)
                        })
                        .unwrap_or_else(|| "오디오 장치".to_string());
                    id.push(0); // PCWSTR for IPolicyConfig
                    devs.push(AudioDev { id, name, default });
                }
                devs
            };
            self.outs = list(eRender);
            self.ins = list(eCapture);
        }
    }

    /// Device row clicked: make it the default and rebind everything that
    /// watches the (old) default.
    fn set_default_device(&mut self, id: Vec<u16>) {
        if let Err(e) = crate::audiopolicy::set_default_endpoint(&id) {
            crate::safety::note(&format!("SetDefaultEndpoint failed: {e:?}"));
            return;
        }
        // The slider endpoint and the OSD's change subscription both point at
        // the previous default; drop ours, tell the OSD to resubscribe.
        self.endpoint = None;
        self.poll_volume();
        self.device = device_name().unwrap_or_else(|| "스피커".to_string());
        self.poll_devices();
        unsafe {
            if let Ok(osd) = FindWindowW(w!("glide_shell_osd"), PCWSTR::null()) {
                let _ = PostMessageW(Some(osd), crate::osd::WM_APP_REBIND, WPARAM(0), LPARAM(0));
            }
            if let Ok(bar) = FindWindowW(w!("glide_shell_bar"), PCWSTR::null()) {
                let _ = PostMessageW(
                    Some(bar),
                    crate::taskbar::WM_AUDIO_REBIND,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
        self.relayout();
    }

    fn poll_battery(&mut self) {
        unsafe {
            let mut sps = Default::default();
            if GetSystemPowerStatus(&mut sps).is_err() {
                return;
            }
            if sps.BatteryFlag & 128 != 0 || sps.BatteryLifePercent == 255 {
                self.battery = None;
            } else {
                self.battery = Some((sps.BatteryLifePercent.min(100), sps.ACLineStatus == 1));
            }
            self.charging = sps.BatteryFlag & 8 != 0;
        }
    }

    fn refresh(&mut self) {
        unsafe {
            // SetForegroundWindow can be denied; if we never became foreground
            // and the user moved on, WA_INACTIVE never comes — close here.
            let fg = GetForegroundWindow();
            if fg != self.hwnd && fg != self.fg_at_open {
                self.dismiss();
                return;
            }
        }
        match self.kind {
            Some(Kind::Volume) => {
                let before = self.vol;
                self.poll_volume();
                if self.vol != before && !self.dragging {
                    self.paint();
                }
            }
            Some(Kind::Network) => {
                let (on, nets) = {
                    let Some(wf) = &self.wifi else { return };
                    let on = wf.radio_on();
                    (on, if on { wf.networks() } else { Vec::new() })
                };
                if let Some(c) = &self.connecting {
                    if nets.iter().any(|n| n.ssid == *c && n.connected) {
                        self.connecting = None;
                    }
                }
                if on != self.radio_on || nets != self.nets {
                    let resize = on != self.radio_on || nets.len() != self.nets.len();
                    self.radio_on = on;
                    self.nets = nets;
                    if resize { self.relayout() } else { self.paint() }
                }
            }
            Some(Kind::Battery) => {
                let before = (self.battery, self.charging);
                self.poll_battery();
                if (self.battery, self.charging) != before {
                    self.paint();
                }
            }
            // Seconds tick on the big clock.
            Some(Kind::Calendar) => self.paint(),
            None => {}
        }
    }

    // ---- input ------------------------------------------------------------

    fn hit(&self, x: f32, y: f32) -> Option<Act> {
        self.hits
            .iter()
            .find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
            .map(|(_, a)| *a)
    }

    fn slider_to(&mut self, x: f32) {
        let t = self.slider;
        if t.right <= t.left {
            return;
        }
        let v = ((x - t.left) / (t.right - t.left)).clamp(0.0, 1.0);
        unsafe {
            if let Some(ep) = &self.endpoint {
                let _ = ep.SetMasterVolumeLevelScalar(v, std::ptr::null());
                if matches!(self.vol, Some((_, true))) {
                    // Dragging the slider unmutes, like the stock flyout.
                    let _ = ep.SetMute(false, std::ptr::null());
                }
            }
        }
        self.vol = Some((v, false));
        self.paint();
    }

    fn act(&mut self, a: Act) {
        match a {
            Act::Slider => {}
            Act::Mute => {
                unsafe {
                    if let (Some(ep), Some((_, m))) = (&self.endpoint, self.vol) {
                        let _ = ep.SetMute(!m, std::ptr::null());
                    }
                }
                self.poll_volume();
                self.paint();
            }
            Act::WifiToggle => {
                let on = !self.radio_on;
                if let Some(wf) = &self.wifi {
                    wf.set_radio(on);
                    if on {
                        wf.scan();
                    }
                }
                // Optimistic; the 1s refresh corrects if the radio disagrees.
                self.radio_on = on;
                if !on {
                    self.nets.clear();
                }
                self.relayout();
            }
            Act::Row(i) => {
                let Some(net) = self.nets.get(i).cloned() else { return };
                if net.connected {
                    return;
                }
                match net.profile {
                    Some(profile) if self.wifi.is_some() => {
                        if let Some(wf) = &self.wifi {
                            wf.connect(&profile);
                        }
                        self.connecting = Some(net.ssid);
                        self.paint();
                    }
                    _ => {
                        // No saved profile — password entry is ms-settings' job.
                        open_settings("ms-settings:network-wifi");
                        self.hide();
                    }
                }
            }
            Act::OutDev(i) => {
                if let Some(d) = self.outs.get(i) {
                    if !d.default {
                        let id = d.id.clone();
                        self.set_default_device(id);
                    }
                }
            }
            Act::InDev(i) => {
                if let Some(d) = self.ins.get(i) {
                    if !d.default {
                        let id = d.id.clone();
                        self.set_default_device(id);
                    }
                }
            }
            Act::NetSettings => {
                open_settings("ms-settings:network");
                self.hide();
            }
            Act::PowerSettings => {
                open_settings("ms-settings:powersleep");
                self.hide();
            }
            Act::CalPrev => {
                self.cal_shift(-1);
                self.paint();
            }
            Act::CalNext => {
                self.cal_shift(1);
                self.paint();
            }
            Act::CalToday => {
                let now = chrono::Local::now();
                self.cal_year = chrono::Datelike::year(&now);
                self.cal_month = chrono::Datelike::month(&now);
                self.paint();
            }
        }
    }

    fn cal_shift(&mut self, delta: i32) {
        let mut y = self.cal_year;
        let mut m = self.cal_month as i32 + delta;
        while m < 1 {
            m += 12;
            y -= 1;
        }
        while m > 12 {
            m -= 12;
            y += 1;
        }
        self.cal_year = y;
        self.cal_month = m as u32;
    }

    fn wheel(&mut self, notches: f32) {
        match self.kind {
            Some(Kind::Volume) => {
                unsafe {
                    if let (Some(ep), Some((v, _))) = (&self.endpoint, self.vol) {
                        let nv = (v + notches * 0.02).clamp(0.0, 1.0);
                        let _ = ep.SetMasterVolumeLevelScalar(nv, std::ptr::null());
                    }
                }
                self.poll_volume();
                self.paint();
            }
            // Win10 scrolls the month grid.
            Some(Kind::Calendar) if notches != 0.0 => {
                self.cal_shift(if notches > 0.0 { -1 } else { 1 });
                self.paint();
            }
            _ => {}
        }
    }

    // ---- painting ---------------------------------------------------------

    fn paint(&mut self) {
        if self.kind.is_none() {
            return;
        }
        self.hits.clear();
        unsafe {
            let r = &self.renderer;
            r.dc.BeginDraw();
            r.dc.Clear(Some(&theme::rgba(26, 27, 32, 0.9)));
            if let Ok(b) = r.brush(theme::with_alpha(theme::accent(), 0.25)) {
                r.dc.FillRectangle(
                    &rect(0.0, 0.0, self.w, 1.0 / self.scale),
                    &b,
                );
            }
        }
        match self.kind {
            Some(Kind::Volume) => self.paint_volume(),
            Some(Kind::Network) => self.paint_network(),
            Some(Kind::Battery) => self.paint_battery(),
            Some(Kind::Calendar) => self.paint_calendar(),
            None => {}
        }
        unsafe {
            let _ = self.renderer.dc.EndDraw(None, None);
            let _ = self.renderer.present();
        }
    }

    fn paint_volume(&mut self) {
        // 출력/입력 device sections, default marked with a check; clicking a
        // non-default row switches the default endpoint (IPolicyConfig).
        let mut y = 8.0;
        let sections: [DeviceSection; 2] = [
            (
                "출력",
                self.outs.iter().map(|d| (d.name.clone(), d.default)).collect(),
                Act::OutDev as fn(usize) -> Act,
            ),
            (
                "입력",
                self.ins.iter().map(|d| (d.name.clone(), d.default)).collect(),
                Act::InDev as fn(usize) -> Act,
            ),
        ];
        for (title, devs, mk) in sections {
            if devs.is_empty() {
                continue;
            }
            self.text(title, &self.fmt_head.clone(), rect(16.0, y, 200.0, y + 24.0), theme::TEXT_DIM);
            y += 24.0;
            for (i, (name, default)) in devs.iter().enumerate() {
                let row = rect(8.0, y, 336.0, y + DEV_ROW_H);
                if self.hover == Some(mk(i)) {
                    self.fill_round(row, 6.0, theme::HOVER_FILL);
                }
                if *default {
                    self.glyph(0xE73E, &self.fmt_g16.clone(), rect(14.0, y, 44.0, y + DEV_ROW_H), theme::accent());
                }
                let tc = if *default { theme::TEXT } else { theme::TEXT_DIM };
                self.text(name, &self.renderer.fmt_title.clone(), rect(52.0, y, 330.0, y + DEV_ROW_H), tc);
                self.hits.push((row, mk(i)));
                y += DEV_ROW_H;
            }
            y += 6.0;
        }
        self.fill_round(rect(14.0, y, 330.0, y + 1.0), 0.0, theme::rgba(255, 255, 255, 0.08));
        y += 6.0;

        let (v, muted) = self.vol.unwrap_or((0.0, true));
        let btn = rect(14.0, y, 50.0, y + 36.0);
        if self.hover == Some(Act::Mute) {
            self.fill_round(btn, 6.0, theme::HOVER_FILL);
        }
        let g = crate::status::volume_glyph(self.vol);
        self.glyph(g, &self.fmt_g16.clone(), btn, theme::TEXT);
        self.hits.push((btn, Act::Mute));

        let track = rect(62.0, y + 16.0, 282.0, y + 20.0);
        self.slider = track;
        self.fill_round(track, 2.0, theme::rgba(255, 255, 255, 0.16));
        let fx = track.left + (track.right - track.left) * v;
        self.fill_round(
            rect(track.left, track.top, fx.max(track.left + 2.0), track.bottom),
            2.0,
            if muted { theme::rgba(148, 152, 162, 1.0) } else { theme::accent() },
        );
        unsafe {
            let color = if muted { theme::TEXT_DIM } else { theme::accent() };
            if let Ok(b) = self.renderer.brush(color) {
                self.renderer.dc.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: Vector2 { X: fx, Y: y + 18.0 },
                        radiusX: 8.0,
                        radiusY: 8.0,
                    },
                    &b,
                );
            }
        }
        self.hits.push((rect(54.0, y, 290.0, y + 36.0), Act::Slider));

        let pct = format!("{:.0}", v * 100.0);
        self.text(&pct, &self.fmt_pct.clone(), rect(290.0, y, 334.0, y + 36.0), theme::TEXT);
    }

    fn paint_network(&mut self) {
        self.text("Wi-Fi", &self.fmt_head.clone(), rect(16.0, 12.0, 200.0, 48.0), theme::TEXT);

        // Radio toggle pill.
        let pill = rect(284.0, 19.0, 330.0, 41.0);
        let on = self.radio_on;
        self.fill_round(
            pill,
            11.0,
            if on { theme::accent() } else { theme::rgba(255, 255, 255, 0.15) },
        );
        unsafe {
            let c = if on { theme::rgba(23, 24, 28, 1.0) } else { theme::TEXT_DIM };
            if let Ok(b) = self.renderer.brush(c) {
                self.renderer.dc.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: Vector2 { X: if on { 319.0 } else { 295.0 }, Y: 30.0 },
                        radiusX: 7.0,
                        radiusY: 7.0,
                    },
                    &b,
                );
            }
        }
        self.hits.push((rect(276.0, 12.0, 336.0, 48.0), Act::WifiToggle));

        let y0 = 56.0;
        let mut rows = 0usize;
        if !self.radio_on {
            let msg = if self.wifi.is_some() { "Wi-Fi 꺼짐" } else { "Wi-Fi 어댑터 없음" };
            self.text(msg, &self.renderer.fmt_title.clone(), rect(16.0, y0, 328.0, y0 + ROW_H), theme::TEXT_DIM);
            rows = 1;
        } else if self.nets.is_empty() {
            self.text("네트워크 없음", &self.renderer.fmt_title.clone(), rect(16.0, y0, 328.0, y0 + ROW_H), theme::TEXT_DIM);
            rows = 1;
        } else {
            let nets = self.nets.clone();
            for (i, n) in nets.iter().enumerate() {
                let y = y0 + i as f32 * ROW_H;
                let row = rect(8.0, y, 336.0, y + ROW_H);
                if self.hover == Some(Act::Row(i)) {
                    self.fill_round(row, 6.0, theme::HOVER_FILL);
                }
                let gc = if n.connected { theme::accent() } else { theme::TEXT };
                self.glyph(wifi_glyph(n.signal), &self.fmt_g16.clone(), rect(14.0, y, 44.0, y + ROW_H), gc);
                self.text(&n.ssid, &self.renderer.fmt_title.clone(), rect(52.0, y, 236.0, y + ROW_H), theme::TEXT);
                if self.connecting.as_deref() == Some(n.ssid.as_str()) && !n.connected {
                    self.text_right("연결 중…", rect(240.0, y, 330.0, y + ROW_H), theme::TEXT_DIM);
                } else if n.connected {
                    self.text_right("연결됨", rect(240.0, y, 330.0, y + ROW_H), theme::accent());
                } else if n.secured {
                    self.glyph(0xE72E, &self.fmt_g16.clone(), rect(306.0, y, 330.0, y + ROW_H), theme::TEXT_DIM);
                }
                self.hits.push((row, Act::Row(i)));
                rows += 1;
            }
        }

        let sep = y0 + rows as f32 * ROW_H + 8.0;
        self.fill_round(rect(14.0, sep, 330.0, sep + 1.0), 0.0, theme::rgba(255, 255, 255, 0.08));

        let f0 = sep + 9.0;
        let footer = rect(8.0, f0, 336.0, f0 + 40.0);
        let hot = self.hover == Some(Act::NetSettings);
        if hot {
            self.fill_round(footer, 6.0, theme::HOVER_FILL);
        }
        self.glyph(0xE713, &self.fmt_g16.clone(), rect(14.0, f0, 44.0, f0 + 40.0), theme::TEXT_DIM);
        self.text(
            "네트워크 및 인터넷 설정",
            &self.renderer.fmt_title.clone(),
            rect(52.0, f0, 328.0, f0 + 40.0),
            if hot { theme::TEXT } else { theme::TEXT_DIM },
        );
        self.hits.push((footer, Act::NetSettings));
    }

    fn paint_battery(&mut self) {
        let Some((p, ac)) = self.battery else { return };
        self.glyph(
            crate::status::battery_glyph(p, self.charging),
            &self.fmt_g30.clone(),
            rect(14.0, 10.0, 70.0, 64.0),
            theme::TEXT,
        );
        self.text(&format!("{p}%"), &self.fmt_big.clone(), rect(78.0, 14.0, 260.0, 62.0), theme::TEXT);

        // No runtime estimate on purpose — Windows' BatteryLifeTime numbers
        // are garbage (user verdict 0721), state only.
        let sub = if ac {
            if self.charging { "전원 연결됨 · 충전 중" } else { "전원 연결됨" }
        } else {
            "배터리 사용 중"
        };
        self.text(sub, &self.renderer.fmt_title.clone(), rect(16.0, 66.0, 328.0, 90.0), theme::TEXT_DIM);

        let sep = 94.0;
        self.fill_round(rect(14.0, sep, 330.0, sep + 1.0), 0.0, theme::rgba(255, 255, 255, 0.08));
        let f0 = 103.0;
        let footer = rect(8.0, f0, 336.0, f0 + 40.0);
        let hot = self.hover == Some(Act::PowerSettings);
        if hot {
            self.fill_round(footer, 6.0, theme::HOVER_FILL);
        }
        self.glyph(0xE713, &self.fmt_g16.clone(), rect(14.0, f0, 44.0, f0 + 40.0), theme::TEXT_DIM);
        self.text(
            "전원 및 배터리 설정",
            &self.renderer.fmt_title.clone(),
            rect(52.0, f0, 328.0, f0 + 40.0),
            if hot { theme::TEXT } else { theme::TEXT_DIM },
        );
        self.hits.push((footer, Act::PowerSettings));
    }

    /// Win10 clock flyout: live seconds clock + full date on top, month grid
    /// below, chevrons/wheel page months, title click jumps back to today.
    fn paint_calendar(&mut self) {
        use chrono::Datelike;
        let now = chrono::Local::now();
        let time = now.format("%H:%M:%S").to_string();
        self.text(&time, &self.fmt_big.clone(), rect(16.0, 8.0, 240.0, 52.0), theme::TEXT);
        let wd = ["월", "화", "수", "목", "금", "토", "일"]
            [now.weekday().num_days_from_monday() as usize];
        let date = format!("{}년 {}월 {}일 {}요일", now.year(), now.month(), now.day(), wd);
        self.text(&date, &self.renderer.fmt_title.clone(), rect(16.0, 52.0, 328.0, 76.0), theme::accent());

        self.fill_round(rect(14.0, 84.0, 330.0, 85.0), 0.0, theme::rgba(255, 255, 255, 0.08));

        let title = format!("{}년 {}월", self.cal_year, self.cal_month);
        self.text(&title, &self.fmt_head.clone(), rect(16.0, 92.0, 220.0, 124.0), theme::TEXT);
        self.hits.push((rect(8.0, 92.0, 220.0, 124.0), Act::CalToday));
        for (cp, act, x) in [(0xE70Eu16, Act::CalPrev, 252.0f32), (0xE70D, Act::CalNext, 294.0)] {
            let rr = rect(x, 92.0, x + 36.0, 124.0);
            if self.hover == Some(act) {
                self.fill_round(rr, 6.0, theme::HOVER_FILL);
            }
            self.glyph(cp, &self.fmt_g16.clone(), rr, theme::TEXT_DIM);
            self.hits.push((rr, act));
        }

        let x0 = 16.0;
        let cw = 312.0 / 7.0;
        for (i, d) in ["일", "월", "화", "수", "목", "금", "토"].iter().enumerate() {
            let x = x0 + i as f32 * cw;
            self.text(d, &self.fmt_pct.clone(), rect(x, 128.0, x + cw, 150.0), theme::TEXT_DIM);
        }

        let today = now.date_naive();
        let first = chrono::NaiveDate::from_ymd_opt(self.cal_year, self.cal_month, 1)
            .unwrap_or(today);
        let lead = first.weekday().num_days_from_sunday() as i64;
        let start = first - chrono::Duration::days(lead);
        for i in 0..42i64 {
            let d = start + chrono::Duration::days(i);
            let (row, col) = (i / 7, i % 7);
            let y = 154.0 + row as f32 * 34.0;
            let cell = rect(x0 + col as f32 * cw, y, x0 + (col + 1) as f32 * cw, y + 34.0);
            let in_month = d.month() == self.cal_month && d.year() == self.cal_year;
            if d == today {
                let px = (cw - 30.0) / 2.0;
                self.fill_round(
                    rect(cell.left + px, cell.top + 2.0, cell.right - px, cell.bottom - 2.0),
                    4.0,
                    theme::accent(),
                );
            }
            let c = if d == today {
                theme::rgba(23, 24, 28, 1.0)
            } else if in_month {
                theme::TEXT
            } else {
                theme::with_alpha(theme::TEXT_DIM, 0.45)
            };
            self.text(&d.day().to_string(), &self.fmt_pct.clone(), cell, c);
        }
    }

    // ---- draw helpers -----------------------------------------------------

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

    /// Right-aligned single line (fmt_title), for the row trailing labels.
    fn text_right(&self, s: &str, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        let t: Vec<u16> = s.encode_utf16().collect();
        let tw = self
            .renderer
            .text_width(&t, &self.renderer.fmt_title, r.right - r.left)
            .min(r.right - r.left);
        self.text(s, &self.renderer.fmt_title.clone(), rect(r.right - tw - 2.0, r.top, r.right, r.bottom), c);
    }

    fn glyph(&self, cp: u16, fmt: &IDWriteTextFormat, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        unsafe {
            if let Ok(b) = self.renderer.brush(c) {
                self.renderer.dc.DrawText(
                    &[cp],
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

fn wifi_glyph(quality: u32) -> u16 {
    match quality {
        0..=24 => 0xE872,  // Wifi1
        25..=49 => 0xE873, // Wifi2
        50..=74 => 0xE874, // Wifi3
        _ => 0xE701,       // Wifi
    }
}

fn open_settings(uri: &str) {
    let wide: Vec<u16> = uri.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
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

fn device_name() -> Option<String> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()?;
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let pv = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let mut buf = [0u16; 128];
        PropVariantToString(&pv, &mut buf).ok()?;
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        let s = String::from_utf16_lossy(&buf[..len]);
        (!s.is_empty()).then_some(s)
    }
}

extern "system" fn flyout_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Flyout;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let fly = &mut *ptr;
        let lx = |f: &Flyout| (lparam.0 & 0xFFFF) as i16 as f32 / f.scale;
        let ly = |f: &Flyout| ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 / f.scale;
        match msg {
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                fly.paint();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_ACTIVATE => {
                if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    fly.dismiss();
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = (lx(fly), ly(fly));
                if fly.dragging {
                    fly.slider_to(x);
                } else {
                    let h = fly.hit(x, y);
                    if h != fly.hover {
                        fly.hover = h;
                        fly.paint();
                    }
                }
                if !fly.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    if TrackMouseEvent(&mut tme).is_ok() {
                        fly.tracking = true;
                    }
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                fly.tracking = false;
                if fly.hover.take().is_some() {
                    fly.paint();
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let (x, y) = (lx(fly), ly(fly));
                if fly.hit(x, y) == Some(Act::Slider) {
                    fly.dragging = true;
                    SetCapture(hwnd);
                    fly.slider_to(x);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                // Take the drag flag BEFORE ReleaseCapture — it sends
                // WM_CAPTURECHANGED synchronously (same lesson as the bar).
                let was_drag = fly.dragging;
                fly.dragging = false;
                let _ = ReleaseCapture();
                if !was_drag {
                    let (x, y) = (lx(fly), ly(fly));
                    if let Some(a) = fly.hit(x, y) {
                        fly.act(a);
                    }
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                fly.dragging = false;
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let notches = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32 / 120.0;
                fly.wheel(notches);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wparam.0 == VK_ESCAPE.0 as usize {
                    fly.hide();
                }
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == TIMER_REFRESH {
                    fly.refresh();
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
