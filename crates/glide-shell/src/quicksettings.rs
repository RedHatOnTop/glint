//! Actuators behind the action center's Quick Settings tiles (SHELL_DESIGN
//! §6.8). Two families:
//!
//! * Radios (Bluetooth, airplane) via WinRT `Windows.Devices.Radios`. The
//!   async `.join()` calls MUST run on an MTA thread — the action center owns
//!   a short-lived worker for exactly this, so these are plain blocking
//!   functions with no apartment of their own.
//! * Auto-rotate via the undocumented `user32!GetAutoRotationState` /
//!   `SetAutoRotation` pair (the same route the stock lock tile takes; there
//!   is no documented setter). Synchronous, apartment-agnostic.

use windows::Devices::Radios::{Radio, RadioAccessStatus, RadioKind, RadioState};

/// A radio tile's world: absent hardware vs present-and-(on/off).
#[derive(Clone, Copy, PartialEq)]
pub enum Tri {
    Absent,
    On,
    Off,
}

impl Tri {
    pub fn is_on(self) -> bool {
        matches!(self, Tri::On)
    }
    pub fn present(self) -> bool {
        !matches!(self, Tri::Absent)
    }
}

/// Snapshot both radio tiles in one enumeration (MTA thread only).
pub struct RadioSnapshot {
    pub bluetooth: Tri,
    /// Airplane = every togglable radio is off. Absent if nothing to toggle.
    pub airplane: Tri,
}

fn radios() -> Option<Vec<Radio>> {
    // Access is granted without a prompt for desktop apps; bail quietly if not.
    if !matches!(
        Radio::RequestAccessAsync().and_then(|op| op.join()),
        Ok(RadioAccessStatus::Allowed)
    ) {
        return None;
    }
    let list = Radio::GetRadiosAsync().and_then(|op| op.join()).ok()?;
    Some(list.into_iter().collect())
}

pub fn snapshot() -> RadioSnapshot {
    let Some(list) = radios() else {
        return RadioSnapshot { bluetooth: Tri::Absent, airplane: Tri::Absent };
    };
    let mut bluetooth = Tri::Absent;
    let mut togglable = 0u32;
    let mut on = 0u32;
    for r in &list {
        let kind = r.Kind().unwrap_or(RadioKind::Other);
        let state = r.State().unwrap_or(RadioState::Unknown);
        // Disabled = policy/hardware-blocked, not user-togglable.
        if matches!(state, RadioState::Disabled | RadioState::Unknown) {
            continue;
        }
        let is_on = matches!(state, RadioState::On);
        if kind == RadioKind::Bluetooth {
            bluetooth = if is_on { Tri::On } else { Tri::Off };
        }
        togglable += 1;
        if is_on {
            on += 1;
        }
    }
    let airplane = if togglable == 0 {
        Tri::Absent
    } else if on == 0 {
        Tri::On // all off == airplane engaged
    } else {
        Tri::Off
    };
    RadioSnapshot { bluetooth, airplane }
}

pub fn set_bluetooth(on: bool) {
    let Some(list) = radios() else { return };
    let target = if on { RadioState::On } else { RadioState::Off };
    for r in &list {
        if r.Kind() == Ok(RadioKind::Bluetooth) {
            if let Ok(op) = r.SetStateAsync(target) {
                let _ = op.join();
            }
        }
    }
}

/// Airplane on = force every togglable radio off. Airplane off = turn radios
/// back on (Wi-Fi + Bluetooth); WWAN stays as the user left it.
pub fn set_airplane(on: bool) {
    let Some(list) = radios() else { return };
    let target = if on { RadioState::Off } else { RadioState::On };
    for r in &list {
        let kind = r.Kind().unwrap_or(RadioKind::Other);
        if on || matches!(kind, RadioKind::WiFi | RadioKind::Bluetooth) {
            if !matches!(r.State(), Ok(RadioState::Disabled)) {
                if let Ok(op) = r.SetStateAsync(target) {
                    let _ = op.join();
                }
            }
        }
    }
}

// ---- auto-rotate (undocumented user32) ------------------------------------

const AR_DISABLED: i32 = 0x1;
const AR_NOSENSOR: i32 = 0x10;
const AR_NOT_SUPPORTED: i32 = 0x20;

type GetAutoRotationState = unsafe extern "system" fn(*mut i32) -> i32;
type SetAutoRotation = unsafe extern "system" fn(i32) -> i32;

fn user32_proc(name: &[u8]) -> Option<*const ()> {
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::core::{PCSTR, w};
    unsafe {
        let m = GetModuleHandleW(w!("user32.dll")).ok()?;
        GetProcAddress(m, PCSTR(name.as_ptr())).map(|p| p as *const ())
    }
}

fn ar_state() -> Option<i32> {
    let p = user32_proc(b"GetAutoRotationState\0")?;
    let f: GetAutoRotationState = unsafe { std::mem::transmute(p) };
    let mut state = 0i32;
    if unsafe { f(&mut state) } != 0 { Some(state) } else { None }
}

/// The device can auto-rotate at all (has a sensor, isn't a desktop).
pub fn autorotate_supported() -> bool {
    ar_state().is_some_and(|s| s & (AR_NOT_SUPPORTED | AR_NOSENSOR) == 0)
}

/// Auto-rotate is currently enabled (rotation lock off).
pub fn autorotate_on() -> bool {
    ar_state().is_some_and(|s| s & AR_DISABLED == 0)
}

pub fn set_autorotate(on: bool) {
    let Some(p) = user32_proc(b"SetAutoRotation\0") else { return };
    let f: SetAutoRotation = unsafe { std::mem::transmute(p) };
    unsafe {
        f(if on { 1 } else { 0 });
    }
}
