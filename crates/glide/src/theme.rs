// glide's own visual language: layered near-black surfaces, one teal accent,
// tighter density than stock Explorer. Kept in one place so a reskin is a single
// edit rather than a hunt through the UI code. Deliberately NOT the OS accent —
// that drags in whatever system color the user set and reads as "unstyled".
use egui::{Color32, CornerRadius, Stroke};

/// glide brand accent (teal).
pub const ACCENT: Color32 = Color32::from_rgb(70, 192, 202);

/// Primary / secondary text on the dark surfaces.
pub const TEXT: Color32 = Color32::from_rgb(232, 233, 238);
pub const TEXT_DIM: Color32 = Color32::from_rgb(148, 152, 162);

/// Base surface (sidebar / toolbar / list): solid, so no busy Mica bleed.
pub const SURFACE: Color32 = Color32::from_rgb(23, 24, 28);
/// Row-hover wash — white at ~6% (premultiplied, so channels == alpha).
pub const HOVER: Color32 = Color32::from_rgba_premultiplied(16, 16, 16, 16);
/// Column-header / faint zebra band — white at ~3%.
pub const HEADER: Color32 = Color32::from_rgba_premultiplied(8, 8, 8, 8);
/// Selected-row fill: solid deep teal, dark enough that white text stays crisp
/// (translucent accent washed out to a low-contrast grey).
pub const SELECT: Color32 = Color32::from_rgb(30, 82, 90);

/// `base` at a given alpha — for accent washes over the dark surface.
pub fn alpha(base: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), a)
}

/// Install glide's base look into egui — recolors every framework widget
/// (buttons, menus, scrollbars, text fields, popups) so nothing falls back to
/// the tell-tale egui-default grey.
pub fn install(ctx: &egui::Context) {
    let r6 = CornerRadius::same(6);
    let white = |a: u8| Color32::from_rgba_unmultiplied(255, 255, 255, a);

    let mut v = egui::Visuals::dark();
    v.override_text_color = None;
    v.hyperlink_color = ACCENT;
    v.selection = egui::style::Selection {
        bg_fill: alpha(ACCENT, 60),
        stroke: Stroke::new(1.0, alpha(ACCENT, 210)),
    };
    v.faint_bg_color = white(6);
    v.extreme_bg_color = Color32::from_rgb(17, 18, 21);
    v.panel_fill = Color32::from_rgb(23, 24, 28);
    v.window_fill = Color32::from_rgb(31, 32, 37);
    v.window_stroke = Stroke::new(1.0, white(22));
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);

    let set = |w: &mut egui::style::WidgetVisuals,
               fill: Color32,
               stroke: Stroke,
               fg: Color32,
               exp: f32| {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = stroke;
        w.fg_stroke = Stroke::new(1.0, fg);
        w.corner_radius = r6;
        w.expansion = exp;
    };
    set(
        &mut v.widgets.noninteractive,
        Color32::TRANSPARENT,
        Stroke::NONE,
        TEXT_DIM,
        0.0,
    );
    set(&mut v.widgets.inactive, white(10), Stroke::NONE, TEXT, 0.0);
    set(
        &mut v.widgets.hovered,
        white(24),
        Stroke::new(1.0, white(28)),
        Color32::WHITE,
        1.0,
    );
    set(
        &mut v.widgets.active,
        alpha(ACCENT, 130),
        Stroke::new(1.0, ACCENT),
        Color32::WHITE,
        1.0,
    );
    v.widgets.open = v.widgets.hovered;
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 5.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.menu_margin = egui::Margin::same(6);
    // Slightly snappier than egui's 0.2 s default — hover/select fades read as
    // responsive, not laggy.
    style.animation_time = 0.12;
    ctx.set_style(style);
}
