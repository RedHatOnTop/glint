//! Persisted taskbar settings: %APPDATA%\glide-shell\settings.txt, one
//! key=value per line. Missing file or missing keys = defaults, so old
//! installs and hand-edits stay valid.

#[derive(Clone, Copy, PartialEq)]
pub struct Settings {
    /// Window buttons show titles; off = 48px icon-only (Win10 default look).
    pub labels: bool,
    /// Clock renders HH:MM:SS (the 1s clock timer already ticks).
    pub clock_seconds: bool,
    /// Show-desktop sliver at the bar's right edge.
    pub desk_sliver: bool,
    /// Per-monitor secondary bars (M5).
    pub secondary_bars: bool,
    /// Bare Win key opens the start menu; off = glint search (pre-0722
    /// behavior). Win+S is glint either way.
    pub winkey_start: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            labels: true,
            clock_seconds: false,
            desk_sliver: true,
            secondary_bars: true,
            winkey_start: true,
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
            let v = v.trim() == "1";
            match k.trim() {
                "labels" => s.labels = v,
                "clock_seconds" => s.clock_seconds = v,
                "desk_sliver" => s.desk_sliver = v,
                "secondary_bars" => s.secondary_bars = v,
                "winkey_start" => s.winkey_start = v,
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
            "labels={}\nclock_seconds={}\ndesk_sliver={}\nsecondary_bars={}\nwinkey_start={}\n",
            b(s.labels),
            b(s.clock_seconds),
            b(s.desk_sliver),
            b(s.secondary_bars),
            b(s.winkey_start),
        ),
    );
}
