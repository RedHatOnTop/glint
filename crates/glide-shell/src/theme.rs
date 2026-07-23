//! Shared design language (SHELL_DESIGN §5). Values mirror
//! crates/glide/src/theme.rs — keep the two in sync by hand; this crate
//! cannot depend on egui types.

use std::sync::atomic::{AtomicU32, Ordering};
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;

pub const fn rgba(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

/// glide accent swatches (KDE-style). Index 0 (teal) is the default; the
/// settings app writes the chosen index to config and calls `set_accent`.
pub const ACCENT_PRESETS: [(&str, u8, u8, u8); 8] = [
    ("틸", 70, 192, 202),
    ("블루", 77, 144, 254),
    ("퍼플", 167, 139, 250),
    ("핑크", 244, 114, 182),
    ("그린", 52, 199, 123),
    ("오렌지", 251, 146, 60),
    ("레드", 248, 113, 113),
    ("그래파이트", 148, 163, 184),
];

/// Current accent, packed 0x00RRGGBB. Read on every paint via `accent()`.
static ACCENT_RGB: AtomicU32 = AtomicU32::new(0x0046C0CA);

/// Point the accent at a preset index (out-of-range falls back to teal).
pub fn set_accent(idx: u8) {
    let (_, r, g, b) = ACCENT_PRESETS
        .get(idx as usize)
        .copied()
        .unwrap_or(ACCENT_PRESETS[0]);
    ACCENT_RGB.store(
        ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
        Ordering::Relaxed,
    );
}

/// The live accent colour. Was a const; now runtime so the picker applies
/// without a relaunch.
pub fn accent() -> D2D1_COLOR_F {
    let v = ACCENT_RGB.load(Ordering::Relaxed);
    rgba((v >> 16) as u8, (v >> 8) as u8, v as u8, 1.0)
}
/// glide SURFACE (23,24,28), at partial alpha so DWM acrylic reads through.
pub const BAR_BG: D2D1_COLOR_F = rgba(23, 24, 28, 0.72);
pub const TEXT: D2D1_COLOR_F = rgba(232, 233, 238, 1.0);
pub const TEXT_DIM: D2D1_COLOR_F = rgba(148, 152, 162, 1.0);
/// Button fills — white washes like glide's HOVER, alpha carried in `a`.
pub const HOVER_FILL: D2D1_COLOR_F = rgba(255, 255, 255, 0.08);
pub const ACTIVE_FILL: D2D1_COLOR_F = rgba(255, 255, 255, 0.05);
pub const FLASH: D2D1_COLOR_F = rgba(224, 164, 80, 1.0);

/// Logical (96-dpi) metrics. Scale by dpi/96 at use sites.
///
/// Bar height is a density preset chosen in settings; baked at launch (the
/// appbar strut and every offset derive from it). 0 compact, 1 normal, 2 large.
static BAR_H: AtomicU32 = AtomicU32::new(40);

pub fn set_bar_density(d: u8) {
    BAR_H.store(match d { 0 => 34, 2 => 48, _ => 40 }, Ordering::Relaxed);
}

pub fn bar_height() -> f32 {
    BAR_H.load(Ordering::Relaxed) as f32
}
pub const BUTTON_MAX_W: f32 = 176.0;
pub const BUTTON_RADIUS: f32 = 6.0;
pub const UNDERLINE_H: f32 = 3.0;
/// Floating panel (Plasma-style): the slab is inset from the screen edges by
/// these gaps, which the desktop shows through. The reserved appbar strut is
/// BAR_HEIGHT + PANEL_MARGIN_BOTTOM tall; left/right gaps are cosmetic.
pub const PANEL_MARGIN_X: f32 = 10.0;
pub const PANEL_MARGIN_BOTTOM: f32 = 8.0;

/// Motion vocabulary: single 120–180ms ease-out (SHELL_DESIGN §5).
pub const ANIM_MS: f32 = 140.0;

pub fn with_alpha(c: D2D1_COLOR_F, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { a, ..c }
}
