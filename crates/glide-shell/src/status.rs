//! M2 status cluster — battery / network / volume / 한영 (SHELL_DESIGN §6.6).
//! Polled at 1 Hz on the clock timer; v1 is display plus volume interaction,
//! richer flyouts arrive with M4's popup machinery.

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eMultimedia, eRender};
use windows::Win32::Networking::NetworkListManager::{
    INetworkListManager, NLM_CONNECTIVITY_IPV4_INTERNET, NLM_CONNECTIVITY_IPV6_INTERNET,
    NetworkListManager,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Power::GetSystemPowerStatus;
use windows::Win32::UI::Input::Ime::ImmGetDefaultIMEWnd;
use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayout;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, SMTO_ABORTIFHUNG, SendMessageTimeoutW,
    WM_IME_CONTROL,
};

const IMC_GETOPENSTATUS: usize = 0x0005;
const LANG_KOREAN: usize = 0x0412;

pub struct Status {
    /// (percent 0-100, on AC). None = no battery in the system.
    pub battery: Option<(u8, bool)>,
    pub net_connected: bool,
    /// (scalar 0.0-1.0, muted). None = no endpoint reachable.
    pub volume: Option<(f32, bool)>,
    /// Some(true) = 한글 mode. None = foreground layout isn't Korean IME.
    pub ime_hangul: Option<bool>,
    endpoint: Option<IAudioEndpointVolume>,
    nlm: Option<INetworkListManager>,
}

impl Status {
    pub fn new() -> Self {
        let mut s = Status {
            battery: None,
            net_connected: true,
            volume: None,
            ime_hangul: None,
            endpoint: None,
            nlm: None,
        };
        s.poll();
        s
    }

    pub fn poll(&mut self) {
        self.poll_battery();
        self.poll_net();
        self.poll_volume();
        self.poll_ime();
    }

    fn poll_battery(&mut self) {
        unsafe {
            let mut sps = Default::default();
            if GetSystemPowerStatus(&mut sps).is_err() {
                return;
            }
            // BatteryFlag 128 = no system battery, 255 = unknown.
            if sps.BatteryFlag & 128 != 0 || sps.BatteryLifePercent == 255 {
                self.battery = None;
            } else {
                self.battery = Some((sps.BatteryLifePercent.min(100), sps.ACLineStatus == 1));
            }
        }
    }

    fn poll_net(&mut self) {
        unsafe {
            if self.nlm.is_none() {
                self.nlm = CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL).ok();
            }
            let Some(nlm) = &self.nlm else { return };
            match nlm.GetConnectivity() {
                Ok(c) => {
                    self.net_connected = c.0
                        & (NLM_CONNECTIVITY_IPV4_INTERNET.0 | NLM_CONNECTIVITY_IPV6_INTERNET.0)
                        != 0;
                }
                Err(_) => self.nlm = None,
            }
        }
    }

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
                self.volume = None;
                return;
            };
            match (ep.GetMasterVolumeLevelScalar(), ep.GetMute()) {
                (Ok(v), Ok(m)) => self.volume = Some((v, m.as_bool())),
                // Default device changed or service hiccup — re-acquire next poll.
                _ => {
                    self.endpoint = None;
                    self.volume = None;
                }
            }
        }
    }

    fn poll_ime(&mut self) {
        unsafe {
            let fg = GetForegroundWindow();
            if fg.is_invalid() {
                return; // keep last known state across focus churn
            }
            let tid = GetWindowThreadProcessId(fg, None);
            let hkl = GetKeyboardLayout(tid);
            if hkl.0 as usize & 0xFFFF != LANG_KOREAN {
                self.ime_hangul = None;
                return;
            }
            let ime = ImmGetDefaultIMEWnd(fg);
            if ime.is_invalid() {
                self.ime_hangul = Some(false);
                return;
            }
            let mut out: usize = 0;
            let _ = SendMessageTimeoutW(
                ime,
                WM_IME_CONTROL,
                WPARAM(IMC_GETOPENSTATUS),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                50,
                Some(&mut out),
            );
            self.ime_hangul = Some(out != 0);
        }
    }

    pub fn toggle_mute(&mut self) {
        unsafe {
            if let (Some(ep), Some((_, muted))) = (&self.endpoint, self.volume) {
                let _ = ep.SetMute(!muted, std::ptr::null());
            }
        }
        self.poll_volume();
    }

    /// Wheel over the volume cell: ±steps of 2%.
    pub fn adjust_volume(&mut self, steps: f32) {
        unsafe {
            if let (Some(ep), Some((v, _))) = (&self.endpoint, self.volume) {
                let nv = (v + steps * 0.02).clamp(0.0, 1.0);
                let _ = ep.SetMasterVolumeLevelScalar(nv, std::ptr::null());
            }
        }
        self.poll_volume();
    }
}

/// Toggle the foreground app's 한/영 state by tapping VK_HANGUL, same as the
/// physical key. The IME processes the key against its own focus, so no
/// foreground games are needed from our WS_EX_NOACTIVATE bar.
pub fn send_hangul_key() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
        VK_HANGUL,
    };
    unsafe {
        let mk = |flags: KEYBD_EVENT_FLAGS| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_HANGUL,
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        };
        let inputs = [mk(KEYBD_EVENT_FLAGS(0)), mk(KEYEVENTF_KEYUP)];
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Fluent glyph for the current volume state.
pub fn volume_glyph(volume: Option<(f32, bool)>) -> u16 {
    match volume {
        None => 0xE74F,            // Mute (shown dim — no endpoint)
        Some((_, true)) => 0xE74F, // Mute
        Some((v, false)) if v <= 0.01 => 0xE992, // Volume0
        Some((v, false)) if v < 0.34 => 0xE993,  // Volume1
        Some((v, false)) if v < 0.67 => 0xE994,  // Volume2
        Some(_) => 0xE995,         // Volume3
    }
}

/// Fluent glyph for battery level; the charging set has two out-of-sequence
/// codepoints at 9/10 (MDL2 quirk kept by Segoe Fluent Icons).
pub fn battery_glyph(percent: u8, charging: bool) -> u16 {
    let level = (percent as u32 + 5) / 10; // 0..=10
    if charging {
        match level {
            10 => 0xEA93, // BatteryCharging10
            9 => 0xE83E,  // BatteryCharging9
            l => 0xE85A + l as u16,
        }
    } else if level >= 10 {
        0xE83F // Battery10
    } else {
        0xE850 + level as u16
    }
}

pub const GLYPH_WIFI: u16 = 0xE701;
