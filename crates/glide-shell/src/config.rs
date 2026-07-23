//! Persisted taskbar settings: %APPDATA%\glide-shell\settings.txt, one
//! key=value per line. Missing file or missing keys = defaults, so old
//! installs and hand-edits stay valid.

#[derive(Clone, Copy, PartialEq)]
pub struct Settings {
    /// Window buttons show titles; off = 48px icon-only (Win10 default look).
    pub labels: bool,
    /// Clock renders with seconds (the 1s clock timer already ticks).
    pub clock_seconds: bool,
    /// 24-hour clock; off = 12-hour with 오전/오후.
    pub clock_24h: bool,
    /// Show the date line under the time; off = time only, centred.
    pub clock_date: bool,
    /// Show-desktop sliver at the bar's right edge.
    pub desk_sliver: bool,
    /// Per-monitor secondary bars (M5).
    pub secondary_bars: bool,
    /// Bare Win key opens the start menu; off = glint search (pre-0722
    /// behavior). Win+S is glint either way.
    pub winkey_start: bool,
    /// Own-notification toasts (UserNotificationListener). Off = silent.
    pub toasts_enabled: bool,
    /// glide accent colour, index into `theme::ACCENT_PRESETS`.
    pub accent: u8,
    /// Bar density: 0 compact, 1 normal, 2 large. Applied at launch.
    pub bar_density: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // Icon-only by default: the floating panel reads as a designed
            // dock, not a Win10 label strip. Toggle labels back in settings.
            labels: false,
            clock_seconds: false,
            clock_24h: true,
            clock_date: true,
            desk_sliver: true,
            secondary_bars: true,
            winkey_start: true,
            toasts_enabled: true,
            accent: 0,
            bar_density: 1,
        }
    }
}

fn path() -> std::path::PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(base).join("glide-shell").join("settings.txt")
}

pub fn load() -> Settings {
    let mut s = Settings::default();
    if let Ok(txt) = std::fs::read_to_string(path()) {
        for line in txt.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let b = v == "1";
            match k {
                "labels" => s.labels = b,
                "clock_seconds" => s.clock_seconds = b,
                "clock_24h" => s.clock_24h = b,
                "clock_date" => s.clock_date = b,
                "desk_sliver" => s.desk_sliver = b,
                "secondary_bars" => s.secondary_bars = b,
                "winkey_start" => s.winkey_start = b,
                "toasts_enabled" => s.toasts_enabled = b,
                "accent" => s.accent = v.parse().unwrap_or(0),
                "bar_density" => s.bar_density = v.parse().unwrap_or(1),
                _ => {}
            }
        }
    }
    s
}

pub fn save(s: &Settings) {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let b = |v: bool| if v { "1" } else { "0" };
    let _ = std::fs::write(
        p,
        format!(
            "labels={}\nclock_seconds={}\nclock_24h={}\nclock_date={}\ndesk_sliver={}\n\
             secondary_bars={}\nwinkey_start={}\ntoasts_enabled={}\naccent={}\nbar_density={}\n",
            b(s.labels),
            b(s.clock_seconds),
            b(s.clock_24h),
            b(s.clock_date),
            b(s.desk_sliver),
            b(s.secondary_bars),
            b(s.winkey_start),
            b(s.toasts_enabled),
            s.accent,
            s.bar_density,
        ),
    );
}
