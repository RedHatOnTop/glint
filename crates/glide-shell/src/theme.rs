//! Shared design language (SHELL_DESIGN §5). Values mirror
//! crates/glide/src/theme.rs — keep the two in sync by hand; this crate
//! cannot depend on egui types.

use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;

pub const fn rgba(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

/// glide ACCENT (70,192,202) — teal.
pub const ACCENT: D2D1_COLOR_F = rgba(70, 192, 202, 1.0);
/// glide SURFACE (23,24,28), at partial alpha so DWM acrylic reads through.
pub const BAR_BG: D2D1_COLOR_F = rgba(23, 24, 28, 0.72);
pub const TEXT: D2D1_COLOR_F = rgba(232, 233, 238, 1.0);
pub const TEXT_DIM: D2D1_COLOR_F = rgba(148, 152, 162, 1.0);
/// Button fills — white washes like glide's HOVER, alpha carried in `a`.
pub const HOVER_FILL: D2D1_COLOR_F = rgba(255, 255, 255, 0.08);
pub const ACTIVE_FILL: D2D1_COLOR_F = rgba(255, 255, 255, 0.05);
pub const FLASH: D2D1_COLOR_F = rgba(224, 164, 80, 1.0);

/// Logical (96-dpi) metrics. Scale by dpi/96 at use sites.
pub const BAR_HEIGHT: f32 = 40.0;
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
