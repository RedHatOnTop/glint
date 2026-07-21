// UI: Win11 search-flyout look — acrylic, Segoe UI Variable, Fluent icon glyphs.
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use glint_core::icons::{IconCache, IconKey};
use glint_core::platform;
use glint_core::sources::{self, AppIndex, FileSearch, Item, Kind};
use global_hotkey::GlobalHotKeyManager;

pub static HOTKEY_PRESSED: AtomicBool = AtomicBool::new(false);
/// HWND of the main window (isize for Send); set once at startup.
pub static WINDOW_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// Window visibility, owned by the hotkey handler + update loop.
pub static VISIBLE: AtomicBool = AtomicBool::new(true);

// Segoe Fluent Icons glyphs (same font Win11 shell uses)
const GLYPH_SEARCH: &str = "\u{E721}";
const GLYPH_GLOBE: &str = "\u{E774}";
const GLYPH_DOC: &str = "\u{E7C3}";
const GLYPH_APPS: &str = "\u{E71D}";
const GLYPH_GEAR: &str = "\u{E713}";

pub struct GlintApp {
    _hotkeys: GlobalHotKeyManager,
    query: String,
    last_query: String,
    selected: usize,
    /// Keyboard moved the selection this frame → keep it in view.
    scroll_to_sel: bool,
    visible: bool,
    shown_at: Instant,
    dark: bool,
    accent: egui::Color32,
    apps: AppIndex,
    files: FileSearch,
    icons: IconCache,
}

impl GlintApp {
    pub fn new(cc: &eframe::CreationContext<'_>, hotkeys: GlobalHotKeyManager) -> Self {
        Self {
            _hotkeys: hotkeys,
            query: String::new(),
            last_query: String::new(),
            selected: 0,
            scroll_to_sel: false,
            visible: true,
            shown_at: Instant::now(),
            dark: platform::is_dark_mode(),
            accent: platform::accent_color(),
            apps: sources::build_app_index(),
            files: FileSearch::spawn(cc.egui_ctx.clone()),
            icons: IconCache::spawn(cc.egui_ctx.clone()),
        }
    }

    /// Housekeeping after the hotkey handler made the window visible.
    fn on_shown(&mut self) {
        self.visible = true;
        self.shown_at = Instant::now();
        self.dark = platform::is_dark_mode();
        self.accent = platform::accent_color();
    }

    /// Hide via Win32 directly (works regardless of eframe's paint state).
    fn hide(&mut self, _ctx: &egui::Context) {
        self.visible = false;
        VISIBLE.store(false, Ordering::SeqCst);
        self.query.clear();
        self.last_query.clear();
        self.selected = 0;
        platform::set_window_visible(WINDOW_HWND.load(Ordering::SeqCst), false);
    }

    /// Flattened result list for the current query: apps → files → web.
    fn results(&self) -> Vec<Item> {
        let q = self.query.trim();
        let mut out: Vec<Item> = Vec::new();
        if q.is_empty() {
            return out;
        }
        // apps
        let mut scored: Vec<(i32, Item)> = self
            .apps
            .lock()
            .unwrap()
            .iter()
            .filter_map(|it| sources::score(q, &it.title).map(|s| (s, it.clone())))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        out.extend(scored.into_iter().take(5).map(|(_, it)| it));
        // settings
        let mut settings = sources::search_settings(q);
        settings.sort_by(|a, b| b.0.cmp(&a.0));
        out.extend(settings.into_iter().take(4).map(|(_, it)| it));
        // files
        let files = self.files.results.lock().unwrap();
        out.extend(files.1.iter().take(8).cloned());
        drop(files);
        // web (always last)
        out.push(sources::web_item(q));
        out
    }
}

impl eframe::App for GlintApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0] // fully transparent → DWM acrylic shows through
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // --- global hotkey toggled visibility on the handler thread; sync state here
        if HOTKEY_PRESSED.swap(false, Ordering::SeqCst) {
            if VISIBLE.load(Ordering::SeqCst) {
                self.on_shown();
            } else {
                self.visible = false;
                self.query.clear();
                self.last_query.clear();
                self.selected = 0;
            }
        }

        // --- keys
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.hide(ctx);
        }
        if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Q)) {
            std::process::exit(0);
        }

        // --- focus lost → hide (grace period after showing)
        if self.visible
            && self.shown_at.elapsed().as_millis() > 300
            && ctx.input(|i| i.viewport().focused) == Some(false)
        {
            self.hide(ctx);
        }

        // --- theme
        let (bg, fg, fg_dim, hover) = if self.dark {
            (
                egui::Color32::from_rgba_unmultiplied(32, 32, 32, 215),
                egui::Color32::from_gray(255),
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 140),
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 14),
            )
        } else {
            (
                egui::Color32::from_rgba_unmultiplied(243, 243, 243, 225),
                egui::Color32::from_gray(20),
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 140),
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 10),
            )
        };
        let sel_bg = {
            let a = self.accent;
            egui::Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), 46)
        };

        let results = self.results();
        if self.selected >= results.len() {
            self.selected = results.len().saturating_sub(1);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) && !results.is_empty() {
            self.selected = (self.selected + 1) % results.len();
            self.scroll_to_sel = true;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) && !results.is_empty() {
            self.selected = (self.selected + results.len() - 1) % results.len();
            self.scroll_to_sel = true;
        }
        let mut launch_now: Option<Item> = None;
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            if let Some(it) = results.get(self.selected) {
                launch_now = Some(it.clone());
            }
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(bg)
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                // ---------- search row ----------
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    // Painted manually: the Fluent glyph's font metrics sit high
                    // relative to the 22pt TextEdit baseline, so nudge it down.
                    let (icon_rect, _) =
                        ui.allocate_exact_size(egui::vec2(24.0, 28.0), egui::Sense::hover());
                    ui.painter().text(
                        icon_rect.center() + egui::vec2(0.0, 3.0),
                        egui::Align2::CENTER_CENTER,
                        GLYPH_SEARCH,
                        egui::FontId::proportional(20.0),
                        fg_dim,
                    );
                    ui.add_space(8.0);
                    let edit = egui::TextEdit::singleline(&mut self.query)
                        .frame(false)
                        .font(egui::FontId::proportional(22.0))
                        .text_color(fg)
                        .hint_text(
                            egui::RichText::new("앱, 파일, 웹 검색")
                                .size(22.0)
                                .color(fg_dim),
                        )
                        .desired_width(f32::INFINITY);
                    let resp = ui.add(edit);
                    if self.visible {
                        resp.request_focus();
                    }
                });
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);

                // ---------- results ----------
                if self.query.trim().is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(120.0);
                        ui.label(
                            egui::RichText::new(GLYPH_SEARCH)
                                .size(44.0)
                                .color(fg_dim),
                        );
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new("검색어를 입력하세요").color(fg_dim));
                    });
                } else {
                    if self.query != self.last_query {
                        self.last_query = self.query.clone();
                        self.files.query(self.query.trim());
                        self.selected = 0;
                    }
                    let mut shown_kind: Option<Kind> = None;
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (i, item) in results.iter().enumerate() {
                                if shown_kind.as_ref() != Some(&item.kind) {
                                    shown_kind = Some(item.kind.clone());
                                    let (glyph, name) = match item.kind {
                                        Kind::App => (GLYPH_APPS, "앱"),
                                        Kind::Setting => (GLYPH_GEAR, "설정"),
                                        Kind::File => (GLYPH_DOC, "파일"),
                                        Kind::Web => (GLYPH_GLOBE, "웹"),
                                    };
                                    ui.add_space(6.0);
                                    ui.label(
                                        egui::RichText::new(format!("{glyph}  {name}"))
                                            .size(12.0)
                                            .color(fg_dim),
                                    );
                                    ui.add_space(2.0);
                                }
                                let row = self.result_row(
                                    ui, item, i == self.selected, fg, fg_dim, sel_bg, hover,
                                );
                                // Keyboard moved selection → keep the row in view.
                                if i == self.selected && self.scroll_to_sel {
                                    row.scroll_to_me(None);
                                }
                                if row.clicked() {
                                    launch_now = Some(item.clone());
                                }
                                // Hover only grabs selection when the mouse itself moved,
                                // so keyboard-driven scrolling doesn't fight the cursor.
                                if row.hovered()
                                    && ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO)
                                {
                                    self.selected = i;
                                }
                            }
                            self.scroll_to_sel = false;
                        });
                }
            });

        if let Some(it) = launch_now {
            sources::launch(&it);
            self.hide(ctx);
        }
    }
}

impl GlintApp {
    fn result_row(
        &self,
        ui: &mut egui::Ui,
        item: &Item,
        selected: bool,
        fg: egui::Color32,
        fg_dim: egui::Color32,
        sel_bg: egui::Color32,
        hover: egui::Color32,
    ) -> egui::Response {
        let h = 44.0;
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), h),
            egui::Sense::click(),
        );
        let painter = ui.painter();
        if selected {
            painter.rect_filled(rect, 6.0, sel_bg);
            // Win11 selection pill on the left edge
            let pill = egui::Rect::from_min_size(
                rect.min + egui::vec2(0.0, h * 0.28),
                egui::vec2(3.0, h * 0.44),
            );
            painter.rect_filled(pill, 2.0, self.accent);
        } else if resp.hovered() {
            painter.rect_filled(rect, 6.0, hover);
        }

        // icon
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(rect.min.x + 26.0, rect.center().y),
            egui::vec2(28.0, 28.0),
        );
        match item.kind {
            Kind::Web => {
                painter.text(
                    icon_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    GLYPH_GLOBE,
                    egui::FontId::proportional(20.0),
                    fg_dim,
                );
            }
            Kind::Setting => {
                painter.text(
                    icon_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    GLYPH_GEAR,
                    egui::FontId::proportional(20.0),
                    self.accent,
                );
            }
            _ => {
                if let Some(tex) = self.icons.get(IconKey::Path(item.target.clone())) {
                    painter.image(
                        tex.id(),
                        icon_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                } else {
                    painter.text(
                        icon_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        GLYPH_DOC,
                        egui::FontId::proportional(20.0),
                        fg_dim,
                    );
                }
            }
        }

        // text
        let tx = rect.min.x + 48.0;
        let has_sub = !item.subtitle.is_empty();
        let ty = if has_sub { rect.min.y + 7.0 } else { rect.center().y - 8.0 };
        painter.text(
            egui::pos2(tx, ty),
            egui::Align2::LEFT_TOP,
            &item.title,
            egui::FontId::proportional(15.0),
            fg,
        );
        if has_sub {
            painter.text(
                egui::pos2(tx, ty + 19.0),
                egui::Align2::LEFT_TOP,
                &item.subtitle,
                egui::FontId::proportional(11.5),
                fg_dim,
            );
        }
        resp
    }
}
