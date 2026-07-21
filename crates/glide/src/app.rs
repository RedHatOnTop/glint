// Explorer-style single pane: window tabs on top, toolbar + command row,
// quick-access/drives sidebar, details panel, clipboard cut/copy/paste.
// Win11 Mica look, Win10-simple commands (no ribbon, no Home, no AI).
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::ops;
use crate::pane::{Pane, SortKey, humanize};
use crate::preview::ThumbCache;
use crate::shellmenu::{self, MenuOutcome};
use crate::sidebar::{self, Drive, QuickItem};
use glint_core::icons::IconCache;
use glint_core::platform;
use glint_core::sources;

// Segoe Fluent Icons
const GLYPH_BACK: &str = "\u{E72B}";
const GLYPH_FWD: &str = "\u{E72A}";
const GLYPH_UP: &str = "\u{E74A}";
const GLYPH_REFRESH: &str = "\u{E72C}";
const GLYPH_EYE: &str = "\u{E7B3}";
const GLYPH_INFO: &str = "\u{E946}";
const GLYPH_SETTINGS: &str = "\u{E713}";
const GLYPH_CLOSE: &str = "\u{E711}";
const GLYPH_ADD: &str = "\u{E710}";
const GLYPH_CHEVRON: &str = "\u{E76C}";
const GLYPH_DRIVE: &str = "\u{EDA2}";
const GLYPH_STAR: &str = "\u{E734}";
const GLYPH_STAR_FILL: &str = "\u{E735}";
const GLYPH_NEW_FOLDER: &str = "\u{E8F4}";
const GLYPH_CUT: &str = "\u{E8C6}";
const GLYPH_COPY: &str = "\u{E8C8}";
const GLYPH_PASTE: &str = "\u{E77F}";
const GLYPH_RENAME: &str = "\u{E8AC}";
const GLYPH_DELETE: &str = "\u{E74D}";
const GLYPH_FOLDER: &str = "\u{E8B7}";
const GLYPH_SORT_UP: &str = "\u{E70E}";
const GLYPH_SORT_DOWN: &str = "\u{E70D}";
const GLYPH_VIEW: &str = "\u{E8A9}";
const GLYPH_SORT: &str = "\u{E8CB}";
const GLYPH_SEARCH: &str = "\u{E721}";
const GLYPH_PC: &str = "\u{E977}";

const ROW_H: f32 = 26.0;
const TAB_H: f32 = 36.0;

// How the file listing lays items out. Sort/selection/DnD are shared across all.
#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    Details,
    Icons,
    List,
}

// Interactions a listing pass reports back to the shared post-loop handling,
// so Details and the grid views funnel through the same selection/menu code.
#[derive(Default)]
struct Hits {
    clicked: Option<usize>,
    dbl: Option<usize>,
    edit_action: Option<bool>, // Some(true)=commit rename, Some(false)=cancel
    menu_row: Option<usize>,
    menu_select: Option<usize>,
    drag_begin: Option<usize>,
    row_drop: Option<PathBuf>,
    mq_hits: Vec<usize>,
}

fn uv01() -> egui::Rect {
    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
}

// Ctrl+P quick-jump: an Everything-backed finder overlay that navigates anywhere.
struct Finder {
    buf: String,
    last_query: String,
    sel: usize,
    focus: bool,
}

impl Finder {
    fn new() -> Self {
        Self {
            buf: String::new(),
            last_query: String::new(),
            sel: 0,
            focus: true,
        }
    }
}

// Inline rename state (also used right after F7 creates a folder).
struct Edit {
    target: PathBuf,
    buf: String,
    focus: bool,
}

struct Clip {
    paths: Vec<PathBuf>,
    cut: bool,
}

// Address-bar direct path entry (Alt+D / Ctrl+L / click on breadcrumb background).
struct PathEdit {
    buf: String,
    focus: bool,
}

// Context-menu request, deferred one frame so the selection highlight paints
// before the (blocking) native menu opens.
enum PendingMenu {
    Item(usize),
    Background,
}

// Custom entry ids prepended to the native shell menu (must stay < 0x1000).
const MI_OPEN: u32 = 1;
const MI_OPEN_TAB: u32 = 2;
const MI_CUT: u32 = 3;
const MI_COPY: u32 = 4;
const MI_COPY_PATH: u32 = 5;
const MI_RENAME: u32 = 6;
const MI_DELETE: u32 = 7;
const MI_PIN: u32 = 8;
const MI_NEW_FOLDER: u32 = 9;
const MI_PASTE: u32 = 10;
const MI_REFRESH: u32 = 11;
const MI_SEP: shellmenu::CustomItem = (0, "", true);

pub struct GlideApp {
    tabs: Vec<Pane>,
    tab: usize,
    show_hidden: bool,
    show_details: bool,
    dark: bool,
    accent: egui::Color32,
    icons: IconCache,
    quick: Vec<QuickItem>,
    drives: Vec<Drive>,
    favorites: Vec<PathBuf>,
    edit: Option<Edit>,
    path_edit: Option<PathEdit>,
    clip: Option<Clip>,
    ops_tx: mpsc::Sender<()>,
    ops_rx: mpsc::Receiver<()>,
    hwnd: isize,
    thumbs: ThumbCache,
    pending_menu: Option<(PendingMenu, bool)>, // bool = still fresh this frame
    drag: Option<PathBuf>,
    drag_target: Option<PathBuf>,
    // Rubber-band selection: (drag-start pos, selection to keep as a base).
    marquee: Option<(egui::Pos2, BTreeSet<usize>)>,
    view: ViewMode,
    // Columns the grid views laid out last frame — keyboard nav steps by this.
    grid_cols: usize,
    file_search: sources::FileSearch,
    finder: Option<Finder>,
    // Cached content snippet for the details panel (text files only), keyed by
    // the currently-shown path so we re-read only when the selection changes.
    text_prev: Option<(PathBuf, String)>,
}

impl GlideApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Optional CLI args: glide.exe [dir]... — each becomes a tab.
        let mut tabs: Vec<Pane> = std::env::args()
            .skip(1)
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .map(|p| Pane::new(p, false))
            .collect();
        if tabs.is_empty() {
            // Default landing spot is 내 PC (drive list), Explorer muscle memory.
            tabs.push(Pane::new(crate::pane::this_pc(), false));
        }
        let (ops_tx, ops_rx) = mpsc::channel();
        Self {
            tabs,
            tab: 0,
            show_hidden: false,
            show_details: true,
            dark: platform::is_dark_mode(),
            accent: crate::theme::ACCENT,
            icons: IconCache::spawn(cc.egui_ctx.clone()),
            quick: sidebar::quick_items(),
            drives: sidebar::drives(),
            favorites: sidebar::load_favorites(),
            edit: None,
            path_edit: None,
            clip: None,
            ops_tx,
            ops_rx,
            hwnd: platform::hwnd_isize(cc),
            thumbs: ThumbCache::spawn(cc.egui_ctx.clone()),
            pending_menu: None,
            drag: None,
            drag_target: None,
            marquee: None,
            view: ViewMode::Details,
            grid_cols: 1,
            file_search: sources::FileSearch::spawn(cc.egui_ctx.clone()),
            finder: None,
            text_prev: None,
        }
    }

    fn cur(&mut self) -> &mut Pane {
        &mut self.tabs[self.tab]
    }

    fn cur_ref(&self) -> &Pane {
        &self.tabs[self.tab]
    }

    fn open_entry(&mut self, entry_idx: usize) {
        let sh = self.show_hidden;
        let pane = self.cur();
        let Some(e) = pane.entries.get(entry_idx) else {
            return;
        };
        let (is_dir, path) = (e.is_dir, e.path.clone());
        if is_dir {
            pane.navigate(path, sh);
        } else {
            std::thread::spawn(move || {
                let _ = open::that(path);
            });
        }
    }

    fn navigate_active(&mut self, to: PathBuf) {
        let sh = self.show_hidden;
        self.cur().navigate(to, sh);
    }

    fn new_tab(&mut self, dir: PathBuf) {
        self.edit = None;
        self.tabs.push(Pane::new(dir, self.show_hidden));
        self.tab = self.tabs.len() - 1;
    }

    fn close_tab(&mut self, i: usize) {
        if self.tabs.len() > 1 {
            self.edit = None;
            self.tabs.remove(i);
            self.tab = self.tab.min(self.tabs.len() - 1);
        }
    }

    fn refresh_all(&mut self) {
        let sh = self.show_hidden;
        for t in &mut self.tabs {
            t.refresh(sh);
        }
        self.drives = sidebar::drives();
    }

    fn entry_path(&self, i: usize) -> Option<PathBuf> {
        self.cur_ref().entries.get(i).map(|e| e.path.clone())
    }

    // Re-sort keeping the cursor on the same file. Clicking the active key again
    // flips order (header behaviour); the sort menu guards against that itself.
    fn set_sort_key(&mut self, key: SortKey) {
        let sh = self.show_hidden;
        let pane = &mut self.tabs[self.tab];
        if pane.sort == key {
            pane.asc = !pane.asc;
        } else {
            pane.sort = key;
            // Explorer defaults: name/type ascending, date/size descending.
            pane.asc = matches!(key, SortKey::Name | SortKey::Type);
        }
        Self::resort_keeping_cursor(pane, sh);
    }

    fn set_sort_asc(&mut self, asc: bool) {
        let sh = self.show_hidden;
        let pane = &mut self.tabs[self.tab];
        if pane.asc == asc {
            return;
        }
        pane.asc = asc;
        Self::resort_keeping_cursor(pane, sh);
    }

    fn resort_keeping_cursor(pane: &mut Pane, sh: bool) {
        let sel_path = pane.entries.get(pane.sel).map(|e| e.path.clone());
        pane.refresh(sh);
        if let Some(p) = sel_path {
            if let Some(i) = pane.entries.iter().position(|e| e.path == p) {
                pane.sel = i;
                pane.scroll_to_sel = true;
            }
        }
    }

    fn clip_set_selection(&mut self, cut: bool) {
        let paths = self.cur_ref().selected_paths();
        if !paths.is_empty() {
            self.clip = Some(Clip { paths, cut });
        }
    }

    fn op_paste(&mut self, ctx: &egui::Context) {
        let Some(clip) = self.clip.take() else { return };
        let dest = self.cur_ref().dir.clone();
        // Cut+paste into a source's own directory is a no-op; drop those entries.
        let srcs: Vec<PathBuf> = if clip.cut {
            clip.paths
                .iter()
                .filter(|p| p.parent() != Some(dest.as_path()))
                .cloned()
                .collect()
        } else {
            clip.paths.clone()
        };
        if !clip.cut {
            // Copy stays on the clipboard for repeated pastes, Explorer-style.
            self.clip = Some(Clip {
                paths: clip.paths.clone(),
                cut: false,
            });
        }
        if srcs.is_empty() {
            return;
        }
        // Explorer's "alpha (2).txt" rename when a copy lands in its own directory.
        let rename = !clip.cut && srcs.iter().any(|p| p.parent() == Some(dest.as_path()));
        let (tx, ctx) = (self.ops_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let refs: Vec<&std::path::Path> = srcs.iter().map(|p| p.as_path()).collect();
            if clip.cut {
                ops::shell_move_many(&refs, &dest);
            } else {
                ops::shell_copy_many(&refs, &dest, rename);
            }
            let _ = tx.send(());
            ctx.request_repaint();
        });
    }

    fn op_recycle_selection(&mut self, ctx: &egui::Context) {
        let paths = self.cur_ref().selected_paths();
        if paths.is_empty() {
            return;
        }
        if self
            .clip
            .as_ref()
            .is_some_and(|c| c.paths.iter().any(|p| paths.contains(p)))
        {
            self.clip = None;
        }
        let (tx, ctx) = (self.ops_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_path()).collect();
            ops::shell_recycle_many(&refs);
            let _ = tx.send(());
            ctx.request_repaint();
        });
    }

    fn op_new_folder(&mut self) {
        let dir = self.cur_ref().dir.clone();
        let Ok(created) = ops::create_new_folder(&dir) else {
            return;
        };
        self.refresh_all();
        let pane = self.cur();
        if let Some(i) = pane.entries.iter().position(|e| e.path == created) {
            pane.select_only(i);
            pane.scroll_to_sel = true;
        }
        let name = created
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        self.edit = Some(Edit {
            target: created,
            buf: name,
            focus: true,
        });
    }

    fn start_rename_at(&mut self, i: usize) {
        let pane = self.cur();
        pane.select_only(i);
        let Some(e) = pane.entries.get(pane.sel) else {
            return;
        };
        self.edit = Some(Edit {
            target: e.path.clone(),
            buf: e.name.clone(),
            focus: true,
        });
    }

    fn start_rename(&mut self) {
        let sel = self.cur_ref().sel;
        self.start_rename_at(sel);
    }

    fn commit_rename(&mut self) {
        let Some(ed) = self.edit.take() else { return };
        let new_name = ed.buf.trim();
        let Some(parent) = ed.target.parent() else {
            return;
        };
        let new_path = parent.join(new_name);
        let renamed = if !new_name.is_empty()
            && new_path != ed.target
            && std::fs::rename(&ed.target, &new_path).is_ok()
        {
            new_path
        } else {
            ed.target
        };
        self.refresh_all();
        let pane = self.cur();
        if let Some(i) = pane.entries.iter().position(|e| e.path == renamed) {
            pane.select_only(i);
            pane.scroll_to_sel = true;
        }
    }

    fn toggle_favorite_path(&mut self, dir: PathBuf) {
        if let Some(i) = self.favorites.iter().position(|p| p == &dir) {
            self.favorites.remove(i);
        } else {
            self.favorites.push(dir);
        }
        sidebar::save_favorites(&self.favorites);
    }

    fn toggle_favorite(&mut self) {
        let dir = self.cur_ref().dir.clone();
        self.toggle_favorite_path(dir);
    }

    /// Native shell context menu for row `i` (our entries + shell extensions).
    fn row_shell_menu(&mut self, ctx: &egui::Context, i: usize) {
        let Some(e) = self.cur_ref().entries.get(i) else {
            return;
        };
        let (path, is_dir) = (e.path.clone(), e.is_dir);
        let pinned = self.favorites.contains(&path);
        let mut items: Vec<shellmenu::CustomItem> = vec![(MI_OPEN, "열기", true)];
        if is_dir {
            items.push((MI_OPEN_TAB, "새 탭에서 열기", true));
        }
        items.push(MI_SEP);
        items.push((MI_CUT, "잘라내기", true));
        items.push((MI_COPY, "복사", true));
        items.push((MI_COPY_PATH, "경로 복사", true));
        items.push(MI_SEP);
        items.push((MI_RENAME, "이름 바꾸기", true));
        items.push((MI_DELETE, "삭제", true));
        if is_dir {
            items.push(MI_SEP);
            let label = if pinned {
                "즐겨찾기에서 제거"
            } else {
                "즐겨찾기에 추가"
            };
            items.push((MI_PIN, label, true));
        }
        match shellmenu::show_item_menu(self.hwnd, &path, &items) {
            MenuOutcome::Custom(MI_OPEN) => self.open_entry(i),
            MenuOutcome::Custom(MI_OPEN_TAB) => self.new_tab(path),
            MenuOutcome::Custom(MI_CUT) => self.clip_set_selection(true),
            MenuOutcome::Custom(MI_COPY) => self.clip_set_selection(false),
            MenuOutcome::Custom(MI_COPY_PATH) => ctx.copy_text(path.display().to_string()),
            MenuOutcome::Custom(MI_RENAME) => self.start_rename_at(i),
            MenuOutcome::Custom(MI_DELETE) => self.op_recycle_selection(ctx),
            MenuOutcome::Custom(MI_PIN) => self.toggle_favorite_path(path),
            MenuOutcome::Custom(_) => {}
            // A shell verb ran (속성 excluded — it changes nothing): the dir may
            // have changed under us (새로 만들기, delete via extension, …).
            MenuOutcome::Invoked => self.refresh_all(),
            MenuOutcome::Dismissed => {}
        }
    }

    /// Folder-background menu: our staples + shell's 새로 만들기/extensions.
    fn background_shell_menu(&mut self, ctx: &egui::Context) {
        let dir = self.cur_ref().dir.clone();
        let items: Vec<shellmenu::CustomItem> = vec![
            (MI_NEW_FOLDER, "새 폴더", true),
            (MI_PASTE, "붙여넣기", self.clip.is_some()),
            (MI_REFRESH, "새로 고침", true),
        ];
        match shellmenu::show_background_menu(self.hwnd, &dir, &items) {
            MenuOutcome::Custom(MI_NEW_FOLDER) => self.op_new_folder(),
            MenuOutcome::Custom(MI_PASTE) => self.op_paste(ctx),
            MenuOutcome::Custom(MI_REFRESH) => self.refresh_all(),
            MenuOutcome::Custom(_) => {}
            MenuOutcome::Invoked => self.refresh_all(),
            MenuOutcome::Dismissed => {}
        }
    }

    /// Finish an internal drag: move (default) or copy (Ctrl) the whole selection
    /// into `dest`. Sources already in `dest`, or containing it, are skipped.
    fn drop_onto(&mut self, ctx: &egui::Context, dest: PathBuf) {
        let srcs: Vec<PathBuf> = self
            .cur_ref()
            .selected_paths()
            .into_iter()
            .filter(|src| {
                src != &dest && src.parent() != Some(dest.as_path()) && !dest.starts_with(src)
            })
            .collect();
        if srcs.is_empty() {
            return;
        }
        let copy = ctx.input(|i| i.modifiers.ctrl);
        let (tx, ctx) = (self.ops_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let refs: Vec<&std::path::Path> = srcs.iter().map(|p| p.as_path()).collect();
            if copy {
                ops::shell_copy_many(&refs, &dest, false);
            } else {
                ops::shell_move_many(&refs, &dest);
            }
            let _ = tx.send(());
            ctx.request_repaint();
        });
    }
}

impl eframe::App for GlideApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0] // transparent → Mica
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ---------- theme ----------
        let (panel_bg, fg, fg_dim, hover, header_bg) = if self.dark {
            (
                crate::theme::SURFACE,
                crate::theme::TEXT,
                crate::theme::TEXT_DIM,
                crate::theme::HOVER,
                crate::theme::HEADER,
            )
        } else {
            (
                egui::Color32::from_rgba_unmultiplied(249, 249, 249, 170),
                egui::Color32::from_gray(20),
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 130),
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 9),
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 6),
            )
        };
        let sel_bg = if self.dark {
            crate::theme::SELECT
        } else {
            let a = self.accent;
            egui::Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), 70)
        };

        // Refresh panes when a background shell op (copy/move/recycle) finishes.
        while self.ops_rx.try_recv().is_ok() {
            self.refresh_all();
        }

        // Recomputed every frame while an internal drag is live.
        self.drag_target = None;

        // Files dragged in from Explorer/other apps → copy into the active tab.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            let dest = self.cur_ref().dir.clone();
            let (tx, ctx2) = (self.ops_tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let srcs: Vec<PathBuf> = dropped
                    .into_iter()
                    .filter(|p| p.parent() != Some(dest.as_path()))
                    .collect();
                let refs: Vec<&std::path::Path> = srcs.iter().map(|p| p.as_path()).collect();
                ops::shell_copy_many(&refs, &dest, false);
                let _ = tx.send(());
                ctx2.request_repaint();
            });
        }
        let hovering_external = ctx.input(|i| !i.raw.hovered_files.is_empty());

        // ---------- global keys ----------
        // Outside of inline rename there are no text inputs: egui's Tab-navigation
        // would focus a button, and the focused widget then consumes Enter.
        // Kill widget focus every frame unless the rename box needs it.
        if self.edit.is_none() && self.path_edit.is_none() && self.finder.is_none() {
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
        }
        // Ctrl+P toggles the quick-jump finder (its own text box owns focus while open).
        if self.edit.is_none()
            && self.path_edit.is_none()
            && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::P))
        {
            self.finder = match self.finder.take() {
                Some(_) => None,
                None => Some(Finder::new()),
            };
        }
        // `editing` also gates listing keys off while the finder is open (it runs its own).
        let editing = self.edit.is_some() || self.path_edit.is_some() || self.finder.is_some();
        let sh = self.show_hidden;
        // Escape unwinds one thing at a time: rename → drag → marquee → selection.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if editing {
                self.edit = None;
                self.path_edit = None;
            } else if self.drag.is_some() {
                self.drag = None;
            } else if self.marquee.is_some() {
                self.marquee = None;
            } else {
                self.cur().marked.clear();
            }
        }
        // Explorer address-bar muscle memory: Alt+D or Ctrl+L to type a path.
        if !editing
            && ctx.input(|i| {
                (i.modifiers.alt && i.key_pressed(egui::Key::D))
                    || (i.modifiers.ctrl && i.key_pressed(egui::Key::L))
            })
        {
            self.path_edit = Some(PathEdit {
                buf: self.cur_ref().dir.display().to_string(),
                focus: true,
            });
        }
        if !editing && ctx.input(|i| i.key_pressed(egui::Key::Backspace)) {
            self.cur().up(sh);
        }
        if !editing && ctx.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft)) {
            self.cur().back(sh);
        }
        if !editing && ctx.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight)) {
            self.cur().forward(sh);
        }
        if !editing && ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::H)) {
            self.show_hidden = !self.show_hidden;
            self.refresh_all();
        }
        if !editing
            && ctx.input(|i| {
                (i.modifiers.ctrl && i.key_pressed(egui::Key::R)) || i.key_pressed(egui::Key::F5)
            })
        {
            self.refresh_all();
        }
        // tabs
        if !editing && ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::T)) {
            self.new_tab(crate::pane::this_pc());
        }
        if !editing && ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::W)) {
            self.close_tab(self.tab);
        }
        if !editing && ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Tab)) {
            self.tab = (self.tab + 1) % self.tabs.len();
        }
        // clipboard + file ops — eframe turns Ctrl+C/X/V into Copy/Cut/Paste
        // events (Key::C/X/V never arrive), so listen for those.
        let (ev_copy, ev_cut, ev_paste) = ctx.input(|i| {
            let (mut c, mut x, mut v) = (false, false, false);
            for e in &i.events {
                match e {
                    egui::Event::Copy => c = true,
                    egui::Event::Cut => x = true,
                    egui::Event::Paste(_) => v = true,
                    _ => {}
                }
            }
            (c, x, v)
        });
        if !editing && ev_copy {
            self.clip_set_selection(false);
        }
        if !editing && ev_cut {
            self.clip_set_selection(true);
        }
        if !editing && ev_paste {
            self.op_paste(ctx);
        }
        if !editing && ctx.input(|i| i.key_pressed(egui::Key::F2)) {
            self.start_rename();
        }
        if !editing && ctx.input(|i| i.key_pressed(egui::Key::F7)) {
            self.op_new_folder();
        }
        if !editing && ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
            self.op_recycle_selection(ctx);
        }
        // Ctrl+A selects every entry in the active pane.
        if !editing && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::A)) {
            self.cur().select_all();
        }
        // View modes — Explorer's Ctrl+Shift+number family, trimmed to our three.
        if !editing {
            let vm = ctx.input(|i| {
                if !(i.modifiers.command && i.modifiers.shift) {
                    None
                } else if i.key_pressed(egui::Key::Num1) {
                    Some(ViewMode::Icons)
                } else if i.key_pressed(egui::Key::Num5) {
                    Some(ViewMode::List)
                } else if i.key_pressed(egui::Key::Num6) {
                    Some(ViewMode::Details)
                } else {
                    None
                }
            });
            if let Some(v) = vm {
                self.view = v;
            }
        }
        // Arrows / Home / End move the cursor; Shift extends from the anchor.
        // Grid views add left/right and step vertically by the column count.
        if !editing {
            let (down, up, left, right, home, end, shift) = ctx.input(|i| {
                (
                    !i.modifiers.alt && i.key_pressed(egui::Key::ArrowDown),
                    !i.modifiers.alt && i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowLeft),
                    i.key_pressed(egui::Key::ArrowRight),
                    i.key_pressed(egui::Key::Home),
                    i.key_pressed(egui::Key::End),
                    i.modifiers.shift,
                )
            });
            let grid = self.view != ViewMode::Details;
            let step = if grid { self.grid_cols.max(1) } else { 1 };
            let pane = self.cur();
            let n = pane.entries.len();
            if n > 0 {
                let to = if down {
                    Some((pane.sel + step).min(n - 1))
                } else if up {
                    Some(pane.sel.saturating_sub(step))
                } else if right && grid {
                    Some((pane.sel + 1).min(n - 1))
                } else if left && grid {
                    Some(pane.sel.saturating_sub(1))
                } else if home {
                    Some(0)
                } else if end {
                    Some(n - 1)
                } else {
                    None
                };
                if let Some(to) = to {
                    if shift {
                        pane.select_range(to);
                    } else {
                        pane.select_only(to);
                    }
                    pane.scroll_to_sel = true;
                }
            }
        }
        if !editing && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            let s = self.cur_ref().sel;
            self.open_entry(s);
        }

        // ---------- tab bar ----------
        egui::TopBottomPanel::top("tabbar")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::TRANSPARENT)
                    .inner_margin(egui::Margin {
                        left: 8,
                        right: 8,
                        top: 10,
                        bottom: 4,
                    }),
            )
            .show(ctx, |ui| {
                let mut switch_to: Option<usize> = None;
                let mut close: Option<usize> = None;
                let mut new_tab = false;
                let mut tab_drop: Option<PathBuf> = None;
                let closable = self.tabs.len() > 1;
                let accent = self.accent;
                let drag_ptr = self
                    .drag
                    .as_ref()
                    .and_then(|_| ctx.input(|i| i.pointer.interact_pos()));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (t, pane) in self.tabs.iter().enumerate() {
                        let title = if pane.is_this_pc() {
                            "내 PC".to_string()
                        } else {
                            pane.dir
                                .file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_else(|| pane.dir.display().to_string())
                        };
                        let active_tab = t == self.tab;
                        // Width follows the title but stays in touch-friendly bounds.
                        let text_w = ui
                            .fonts_mut(|f| {
                                f.layout_no_wrap(
                                    title.clone(),
                                    egui::FontId::proportional(12.5),
                                    fg,
                                )
                            })
                            .size()
                            .x;
                        let tab_w = (text_w + 76.0).clamp(120.0, 220.0);
                        let (rect, resp) =
                            ui.allocate_exact_size(egui::vec2(tab_w, TAB_H), egui::Sense::click());
                        let x_size = 24.0;
                        let x_rect = egui::Rect::from_center_size(
                            egui::pos2(rect.max.x - 18.0, rect.center().y),
                            egui::vec2(x_size, x_size),
                        );
                        let x_resp = ui.interact(
                            x_rect,
                            ui.id().with(("tab_close", t)),
                            egui::Sense::click(),
                        );
                        let show_x = closable && (active_tab || resp.hovered() || x_resp.hovered());

                        let p = ui.painter();
                        if active_tab {
                            p.rect_filled(rect, 7.0, header_bg);
                            // Accent line along the bottom edge marks the live tab.
                            let line = egui::Rect::from_min_max(
                                egui::pos2(rect.min.x + 10.0, rect.max.y - 2.5),
                                egui::pos2(rect.max.x - 10.0, rect.max.y),
                            );
                            p.rect_filled(line, 1.0, accent);
                        } else if resp.hovered() || x_resp.hovered() {
                            p.rect_filled(rect, 7.0, hover);
                        }
                        p.text(
                            egui::pos2(rect.min.x + 12.0, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            GLYPH_FOLDER,
                            egui::FontId::proportional(13.0),
                            if active_tab { accent } else { fg_dim },
                        );
                        let title_right = if show_x {
                            x_rect.min.x - 2.0
                        } else {
                            rect.max.x - 10.0
                        };
                        let title_clip = egui::Rect::from_min_max(
                            egui::pos2(rect.min.x + 32.0, rect.min.y),
                            egui::pos2(title_right, rect.max.y),
                        );
                        p.clone().with_clip_rect(title_clip).text(
                            egui::pos2(rect.min.x + 32.0, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            &title,
                            egui::FontId::proportional(12.5),
                            if active_tab { fg } else { fg_dim },
                        );
                        if show_x {
                            if x_resp.hovered() {
                                p.circle_filled(x_rect.center(), 10.0, hover.gamma_multiply(2.0));
                            }
                            p.text(
                                x_rect.center(),
                                egui::Align2::CENTER_CENTER,
                                GLYPH_CLOSE,
                                egui::FontId::proportional(9.0),
                                if x_resp.hovered() { fg } else { fg_dim },
                            );
                        }
                        if x_resp.clicked() {
                            close = Some(t);
                        } else if resp.clicked() {
                            switch_to = Some(t);
                        }
                        if resp.middle_clicked() {
                            close = Some(t);
                        }
                        // Dragging a file over a tab targets that tab's directory.
                        if let Some(ptr) = drag_ptr {
                            if rect.contains(ptr) {
                                tab_drop = Some(pane.dir.clone());
                                ui.painter().rect_stroke(
                                    rect.shrink(1.0),
                                    7.0,
                                    egui::Stroke::new(1.5, accent),
                                    egui::StrokeKind::Inside,
                                );
                            }
                        }
                    }
                    // New-tab button: full-height touch target with hover circle.
                    let (add_rect, add_resp) =
                        ui.allocate_exact_size(egui::vec2(TAB_H, TAB_H), egui::Sense::click());
                    let p = ui.painter();
                    if add_resp.hovered() {
                        p.circle_filled(add_rect.center(), 13.0, hover.gamma_multiply(2.0));
                    }
                    p.text(
                        add_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        GLYPH_ADD,
                        egui::FontId::proportional(12.0),
                        if add_resp.hovered() { fg } else { fg_dim },
                    );
                    if add_resp.clicked() {
                        new_tab = true;
                    }
                    add_resp.on_hover_text("새 탭 (Ctrl+T)");
                });
                if let Some(t) = switch_to {
                    self.edit = None;
                    self.tab = t;
                }
                if let Some(t) = close {
                    self.close_tab(t);
                }
                if new_tab {
                    self.new_tab(crate::pane::this_pc());
                }
                if tab_drop.is_some() {
                    self.drag_target = tab_drop;
                }
            });

        // ---------- toolbar: nav + breadcrumb + toggles ----------
        // All toolbar buttons share the breadcrumb's 32px height so every
        // glyph sits on the same vertical center line.
        let flat = |glyph: &str, color: egui::Color32| {
            egui::Button::new(egui::RichText::new(glyph).size(15.0).color(color))
                .fill(egui::Color32::TRANSPARENT)
                .min_size(egui::vec2(34.0, 32.0))
        };
        egui::TopBottomPanel::top("toolbar")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::TRANSPARENT)
                    .inner_margin(egui::Margin {
                        left: 8,
                        right: 8,
                        top: 6,
                        bottom: 6,
                    }),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().button_padding = egui::vec2(8.0, 6.0);
                ui.horizontal(|ui| {
                    let (can_back, can_fwd, can_up) = {
                        let p = self.cur_ref();
                        (
                            p.hist_pos > 0,
                            p.hist_pos + 1 < p.history.len(),
                            p.dir.parent().is_some(),
                        )
                    };
                    let sh = self.show_hidden;
                    if ui.add_enabled(can_back, flat(GLYPH_BACK, fg)).clicked() {
                        self.cur().back(sh);
                    }
                    if ui.add_enabled(can_fwd, flat(GLYPH_FWD, fg)).clicked() {
                        self.cur().forward(sh);
                    }
                    if ui.add_enabled(can_up, flat(GLYPH_UP, fg)).clicked() {
                        self.cur().up(sh);
                    }
                    if ui
                        .add(flat(GLYPH_REFRESH, fg))
                        .on_hover_text("새로 고침 (F5)")
                        .clicked()
                    {
                        self.refresh_all();
                    }
                    ui.add_space(8.0);

                    // breadcrumb of active tab, rooted at "내 PC"
                    let segs: Vec<(String, PathBuf)> = {
                        let mut acc = PathBuf::new();
                        let mut v = vec![("내 PC".to_string(), PathBuf::new())];
                        v.extend(self.cur_ref().dir.components().filter_map(|c| {
                            acc.push(c);
                            let label = match c {
                                std::path::Component::RootDir => return None,
                                std::path::Component::Prefix(p) => {
                                    format!("{}", p.as_os_str().to_string_lossy())
                                }
                                other => other.as_os_str().to_string_lossy().to_string(),
                            };
                            Some((label, acc.clone()))
                        }));
                        v
                    };
                    let mut go: Option<PathBuf> = None;
                    // Keep the breadcrumb out from under the right-side toggles:
                    // reserve the exact width of the 5-icon cluster (~206px) so the
                    // two never overlap, then pin the crumb scroll to the tail.
                    let bc_w = (ui.available_width() - 250.0).max(60.0);
                    if self.path_edit.is_some() {
                        // Address-bar mode: type a path, Enter navigates, Esc cancels.
                        let pe = self.path_edit.as_mut().unwrap();
                        let resp = ui.add_sized(
                            egui::vec2(bc_w, 30.0),
                            egui::TextEdit::singleline(&mut pe.buf)
                                .font(egui::FontId::proportional(13.5)),
                        );
                        if pe.focus {
                            resp.request_focus();
                            pe.focus = false;
                        }
                        let entered =
                            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if entered {
                            let cand = PathBuf::from(pe.buf.trim());
                            if cand.is_dir() {
                                go = Some(cand);
                            }
                            self.path_edit = None;
                        } else if resp.lost_focus() {
                            self.path_edit = None;
                        }
                    } else {
                        // Background click opens the path editor — registered before
                        // the segment buttons so those stay on top.
                        let bc_rect =
                            egui::Rect::from_min_size(ui.cursor().min, egui::vec2(bc_w, 32.0));
                        let bg = ui.interact(bc_rect, ui.id().with("bc_bg"), egui::Sense::click());

                        // Painter-drawn crumb: every segment sits on the exact row
                        // midline (like the file rows), tail pinned right, head clipped
                        // — no nested ScrollArea to drift the text below the buttons.
                        let font = egui::FontId::proportional(14.0);
                        let chev_font = egui::FontId::proportional(10.0);
                        let mid_y = bc_rect.center().y;
                        let painter = ui.painter().with_clip_rect(bc_rect);
                        let chev_w = painter
                            .layout_no_wrap(GLYPH_CHEVRON.to_string(), chev_font.clone(), fg_dim)
                            .size()
                            .x;
                        let pad = 9.0;
                        let mut widths = Vec::with_capacity(segs.len());
                        let mut total = 0.0f32;
                        for (i, (label, _)) in segs.iter().enumerate() {
                            let w = painter
                                .layout_no_wrap(label.clone(), font.clone(), fg)
                                .size()
                                .x;
                            widths.push(w);
                            if i > 0 {
                                total += chev_w + pad * 2.0;
                            }
                            total += w;
                        }
                        // Pin the tail: overflow scrolls off the left, clipped.
                        let mut x = if total <= bc_w {
                            bc_rect.min.x
                        } else {
                            bc_rect.max.x - total
                        };
                        for (i, (label, target)) in segs.iter().enumerate() {
                            if i > 0 {
                                x += pad;
                                painter.text(
                                    egui::pos2(x, mid_y),
                                    egui::Align2::LEFT_CENTER,
                                    GLYPH_CHEVRON,
                                    chev_font.clone(),
                                    fg_dim,
                                );
                                x += chev_w + pad;
                            }
                            let w = widths[i];
                            let seg_rect = egui::Rect::from_min_max(
                                egui::pos2(x, bc_rect.min.y),
                                egui::pos2(x + w, bc_rect.max.y),
                            );
                            let resp = ui.interact(
                                seg_rect,
                                ui.id().with(("crumb", i)),
                                egui::Sense::click(),
                            );
                            let last = i + 1 == segs.len();
                            let col = if resp.hovered() || last { fg } else { fg_dim };
                            if resp.hovered() {
                                painter.rect_filled(
                                    seg_rect.expand2(egui::vec2(3.0, -7.0)),
                                    4.0,
                                    hover,
                                );
                            }
                            painter.text(
                                egui::pos2(x, mid_y),
                                egui::Align2::LEFT_CENTER,
                                label,
                                font.clone(),
                                col,
                            );
                            if resp.clicked() {
                                go = Some(target.clone());
                            }
                            x += w;
                        }
                        ui.allocate_rect(bc_rect, egui::Sense::hover());
                        // Background click (not on a segment) opens the path editor.
                        if bg.clicked() && go.is_none() {
                            let cur = self.cur_ref();
                            self.path_edit = Some(PathEdit {
                                buf: if cur.is_this_pc() {
                                    "내 PC".to_string()
                                } else {
                                    cur.dir.display().to_string()
                                },
                                focus: true,
                            });
                        }
                    }
                    if let Some(t) = go {
                        // "C:" → "C:\" for a bare drive letter; leave the empty
                        // "내 PC" path and normal paths untouched.
                        let t = if !t.as_os_str().is_empty()
                            && t.parent().is_none()
                            && t.extension().is_none()
                        {
                            PathBuf::from(format!("{}\\", t.display()))
                        } else {
                            t
                        };
                        self.navigate_active(t);
                    }

                    // right side toggles
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Rightmost: default-file-manager registration (opt-in, reversible).
                        let gear = ui.add(flat(GLYPH_SETTINGS, fg_dim)).on_hover_text("설정");
                        egui::Popup::menu(&gear).show(|ui| {
                            ui.set_min_width(232.0);
                            ui.label(
                                egui::RichText::new("기본 파일 관리자")
                                    .size(11.0)
                                    .color(fg_dim),
                            );
                            ui.add_space(2.0);
                            let mut fh = crate::register::folder_handler_enabled();
                            if ui
                                .checkbox(&mut fh, "폴더·드라이브를 glide로 열기")
                                .changed()
                            {
                                let _ = crate::register::set_folder_handler(fh);
                            }
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new("체크 해제 시 Windows 탐색기로 복원")
                                    .size(10.0)
                                    .color(fg_dim),
                            );
                        });
                        let info_col = if self.show_details {
                            self.accent
                        } else {
                            fg_dim
                        };
                        if ui
                            .add(flat(GLYPH_INFO, info_col))
                            .on_hover_text("세부 정보")
                            .clicked()
                        {
                            self.show_details = !self.show_details;
                        }
                        let view_btn = ui.add(flat(GLYPH_VIEW, fg_dim)).on_hover_text("보기");
                        egui::Popup::menu(&view_btn).show(|ui| {
                            ui.set_min_width(150.0);
                            for (m, label) in [
                                (ViewMode::Details, "자세히"),
                                (ViewMode::Icons, "큰 아이콘"),
                                (ViewMode::List, "목록"),
                            ] {
                                if ui.selectable_label(self.view == m, label).clicked() {
                                    self.view = m;
                                }
                            }
                        });
                        let sort_btn = ui.add(flat(GLYPH_SORT, fg_dim)).on_hover_text("정렬 기준");
                        egui::Popup::menu(&sort_btn).show(|ui| {
                            ui.set_min_width(160.0);
                            let (cur_sort, cur_asc) = {
                                let p = &self.tabs[self.tab];
                                (p.sort, p.asc)
                            };
                            for (k, label) in [
                                (SortKey::Name, "이름"),
                                (SortKey::Type, "유형"),
                                (SortKey::Size, "크기"),
                                (SortKey::Date, "수정한 날짜"),
                            ] {
                                if ui.selectable_label(cur_sort == k, label).clicked()
                                    && cur_sort != k
                                {
                                    self.set_sort_key(k);
                                }
                            }
                            ui.separator();
                            if ui.selectable_label(cur_asc, "오름차순").clicked() {
                                self.set_sort_asc(true);
                            }
                            if ui.selectable_label(!cur_asc, "내림차순").clicked() {
                                self.set_sort_asc(false);
                            }
                        });
                        let cur_dir = self.cur_ref().dir.clone();
                        let pinned = self.favorites.contains(&cur_dir);
                        let (star, star_col) = if pinned {
                            (GLYPH_STAR_FILL, self.accent)
                        } else {
                            (GLYPH_STAR, fg_dim)
                        };
                        if ui
                            .add(flat(star, star_col))
                            .on_hover_text("현재 폴더 즐겨찾기")
                            .clicked()
                        {
                            self.toggle_favorite();
                        }
                        let eye_col = if sh { self.accent } else { fg_dim };
                        if ui
                            .add(flat(GLYPH_EYE, eye_col))
                            .on_hover_text("숨김 파일 (Ctrl+H)")
                            .clicked()
                        {
                            self.show_hidden = !self.show_hidden;
                            self.refresh_all();
                        }
                    });
                });
            });

        // ---------- command row: file operations, Explorer-style up top ----------
        egui::TopBottomPanel::top("commands")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::TRANSPARENT)
                    .inner_margin(egui::Margin {
                        left: 8,
                        right: 8,
                        top: 4,
                        bottom: 10,
                    }),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().button_padding = egui::vec2(12.0, 7.0);
                ui.horizontal(|ui| {
                    // Uniform 34px height keeps glyph+text pairs on one baseline
                    // across the whole row.
                    let cmd = |ui: &mut egui::Ui, enabled: bool, glyph: &str, label: &str| {
                        ui.add_enabled(
                            enabled,
                            egui::Button::new(
                                egui::RichText::new(format!("{glyph}  {label}"))
                                    .size(12.5)
                                    .color(fg),
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .min_size(egui::vec2(0.0, 34.0)),
                        )
                    };
                    let has_sel = !self.cur_ref().marked.is_empty();
                    let has_clip = self.clip.is_some();
                    if cmd(ui, true, GLYPH_NEW_FOLDER, "새 폴더").clicked() {
                        self.op_new_folder();
                    }
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);
                    if cmd(ui, has_sel, GLYPH_CUT, "잘라내기")
                        .on_hover_text("Ctrl+X")
                        .clicked()
                    {
                        self.clip_set_selection(true);
                    }
                    if cmd(ui, has_sel, GLYPH_COPY, "복사")
                        .on_hover_text("Ctrl+C")
                        .clicked()
                    {
                        self.clip_set_selection(false);
                    }
                    if cmd(ui, has_clip, GLYPH_PASTE, "붙여넣기")
                        .on_hover_text("Ctrl+V")
                        .clicked()
                    {
                        self.op_paste(ctx);
                    }
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);
                    if cmd(ui, has_sel, GLYPH_RENAME, "이름 바꾸기")
                        .on_hover_text("F2")
                        .clicked()
                    {
                        self.start_rename();
                    }
                    if cmd(ui, has_sel, GLYPH_DELETE, "삭제")
                        .on_hover_text("Del — 휴지통으로")
                        .clicked()
                    {
                        self.op_recycle_selection(ctx);
                    }
                });
            });

        // ---------- left sidebar: favorites + quick access + drives ----------
        let cur_dir = self.cur_ref().dir.clone();
        egui::SidePanel::left("sidebar")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::TRANSPARENT)
                    .inner_margin(8),
            )
            .exact_width(190.0)
            .resizable(false)
            .show(ctx, |ui| {
                let mut go: Option<PathBuf> = None;
                let mut unpin: Option<usize> = None;
                let mut side_drop: Option<PathBuf> = None;
                let drag_ptr = self
                    .drag
                    .as_ref()
                    .and_then(|_| ctx.input(|i| i.pointer.interact_pos()));
                let accent = self.accent;
                let drop_mark =
                    |ui: &egui::Ui, rect: egui::Rect, dest: &PathBuf, out: &mut Option<PathBuf>| {
                        if let Some(ptr) = drag_ptr {
                            if rect.contains(ptr) {
                                *out = Some(dest.clone());
                                ui.painter().rect_stroke(
                                    rect.shrink(1.0),
                                    5.0,
                                    egui::Stroke::new(1.5, accent),
                                    egui::StrokeKind::Inside,
                                );
                            }
                        }
                    };
                // 내 PC — the drive-list root, and where new tabs land.
                {
                    let resp = sidebar_row(
                        ui,
                        GLYPH_PC,
                        "내 PC",
                        fg,
                        fg_dim,
                        hover,
                        cur_dir.as_os_str().is_empty(),
                    );
                    if resp.clicked() {
                        go = Some(crate::pane::this_pc());
                    }
                }
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);
                if !self.favorites.is_empty() {
                    for (i, f) in self.favorites.iter().enumerate() {
                        let label = f
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| f.display().to_string());
                        let resp = sidebar_row(
                            ui,
                            GLYPH_STAR_FILL,
                            &label,
                            fg,
                            fg_dim,
                            hover,
                            *f == cur_dir,
                        )
                        .on_hover_text(f.display().to_string());
                        if resp.clicked() {
                            go = Some(f.clone());
                        }
                        drop_mark(ui, resp.rect, f, &mut side_drop);
                        resp.context_menu(|ui| {
                            if ui.button("즐겨찾기에서 제거").clicked() {
                                unpin = Some(i);
                            }
                        });
                    }
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(6.0);
                }
                if let Some(i) = unpin {
                    self.favorites.remove(i);
                    sidebar::save_favorites(&self.favorites);
                }
                for q in &self.quick {
                    let resp =
                        sidebar_row(ui, q.glyph, q.label, fg, fg_dim, hover, q.path == cur_dir);
                    if resp.clicked() {
                        go = Some(q.path.clone());
                    }
                    drop_mark(ui, resp.rect, &q.path, &mut side_drop);
                }
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);
                for d in &self.drives {
                    let used = d.total - d.free;
                    let frac = used as f32 / d.total as f32;
                    let label = format!("{} ", d.label);
                    let resp = sidebar_row(
                        ui,
                        GLYPH_DRIVE,
                        &label,
                        fg,
                        fg_dim,
                        hover,
                        d.root == cur_dir,
                    );
                    // capacity bar under the row
                    let r = resp.rect;
                    let bar = egui::Rect::from_min_size(
                        egui::pos2(r.min.x + 30.0, r.max.y - 7.0),
                        egui::vec2(r.width() - 40.0, 3.0),
                    );
                    let p = ui.painter();
                    p.rect_filled(bar, 2.0, hover.gamma_multiply(2.0));
                    let mut fill = bar;
                    fill.set_right(bar.min.x + bar.width() * frac);
                    let bar_col = if frac > 0.9 {
                        egui::Color32::from_rgb(220, 90, 90)
                    } else {
                        self.accent
                    };
                    p.rect_filled(fill, 2.0, bar_col);
                    // Same text line as the drive label; the bar keeps the bottom.
                    p.text(
                        egui::pos2(r.max.x - 6.0, r.center().y),
                        egui::Align2::RIGHT_CENTER,
                        format!("{} 남음", humanize(d.free)),
                        egui::FontId::proportional(10.0),
                        fg_dim,
                    );
                    if resp.clicked() {
                        go = Some(d.root.clone());
                    }
                    drop_mark(ui, resp.rect, &d.root, &mut side_drop);
                }
                if side_drop.is_some() {
                    self.drag_target = side_drop;
                }
                if let Some(t) = go {
                    self.navigate_active(t);
                }
            });

        // ---------- right sidebar: details ----------
        if self.show_details {
            egui::SidePanel::right("details")
                .frame(
                    egui::Frame::new()
                        .fill(egui::Color32::TRANSPARENT)
                        .inner_margin(10),
                )
                .exact_width(220.0)
                .resizable(false)
                .show(ctx, |ui| {
                    let pane = self.cur_ref();
                    // Several rows selected → summary instead of a single item.
                    if pane.marked.len() > 1 {
                        let (mut files, mut dirs, mut bytes) = (0u32, 0u32, 0u64);
                        for e in pane.marked.iter().filter_map(|&i| pane.entries.get(i)) {
                            if e.is_dir {
                                dirs += 1;
                            } else {
                                files += 1;
                                bytes += e.size;
                            }
                        }
                        ui.add_space(12.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(format!("{}개 선택됨", files + dirs))
                                    .size(15.0)
                                    .strong()
                                    .color(fg),
                            );
                        });
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(6.0);
                        let field = |ui: &mut egui::Ui, k: &str, v: String| {
                            ui.label(egui::RichText::new(k).size(11.0).color(fg_dim));
                            ui.label(egui::RichText::new(v).size(12.5).color(fg));
                            ui.add_space(6.0);
                        };
                        if dirs > 0 {
                            field(ui, "폴더", format!("{dirs}개"));
                        }
                        if files > 0 {
                            field(ui, "파일", format!("{files}개"));
                            field(ui, "합계 크기", humanize(bytes));
                        }
                        return;
                    }
                    let Some(e) = pane
                        .entries
                        .get(pane.sel)
                        .filter(|_| !pane.marked.is_empty())
                    else {
                        ui.label(egui::RichText::new("선택 없음").color(fg_dim));
                        return;
                    };
                    let (name, is_dir, size, modified, path) =
                        (e.name.clone(), e.is_dir, e.size, e.modified, e.path.clone());
                    let is_drive = glint_core::icons::is_drive_root(&path.to_string_lossy());
                    // Text files get a content snippet (cached until the selection
                    // changes) — shell thumbnails don't cover them.
                    let ext = path
                        .extension()
                        .map(|x| x.to_string_lossy().to_lowercase())
                        .unwrap_or_default();
                    let text_snip: Option<String> = if !is_dir && crate::preview::is_text_ext(&ext)
                    {
                        if self
                            .text_prev
                            .as_ref()
                            .map(|(p, _)| *p != path)
                            .unwrap_or(true)
                        {
                            let s = crate::preview::read_snippet(&path).unwrap_or_default();
                            self.text_prev = Some((path.clone(), s));
                        }
                        self.text_prev
                            .as_ref()
                            .map(|(_, s)| s.clone())
                            .filter(|s| !s.trim().is_empty())
                    } else {
                        None
                    };
                    ui.vertical_centered(|ui| {
                        ui.add_space(12.0);
                        // Text snippet first, else a shell thumbnail (images, video
                        // frames, PDFs …), else the plain file-type icon.
                        let thumb = if is_dir || text_snip.is_some() {
                            None
                        } else {
                            self.thumbs.get(&path.to_string_lossy(), modified).flatten()
                        };
                        let uv =
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                        if let Some(snip) = &text_snip {
                            let box_w = ui.available_width();
                            let (r, _) = ui.allocate_exact_size(
                                egui::vec2(box_w, 176.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect(
                                r,
                                egui::CornerRadius::same(6),
                                egui::Color32::from_rgb(17, 18, 21),
                                egui::Stroke::new(
                                    1.0,
                                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 20),
                                ),
                                egui::StrokeKind::Inside,
                            );
                            let inner = r.shrink(9.0);
                            let galley = ui.painter().layout(
                                snip.clone(),
                                egui::FontId::monospace(10.5),
                                fg,
                                inner.width(),
                            );
                            ui.painter()
                                .with_clip_rect(inner)
                                .galley(inner.min, galley, fg);
                            ui.add_space(10.0);
                        } else if let Some(tex) = thumb {
                            let tsize = tex.size_vec2();
                            let max = egui::vec2(ui.available_width() - 8.0, 150.0);
                            let scale = (max.x / tsize.x).min(max.y / tsize.y).min(1.0);
                            let size = tsize * scale;
                            let (r, _) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), size.y),
                                egui::Sense::hover(),
                            );
                            let img_rect = egui::Rect::from_center_size(r.center(), size);
                            ui.painter()
                                .image(tex.id(), img_rect, uv, egui::Color32::WHITE);
                            ui.add_space(10.0);
                        } else {
                            let icon_rect = egui::Rect::from_center_size(
                                ui.cursor().min + egui::vec2(ui.available_width() / 2.0, 32.0),
                                egui::vec2(64.0, 64.0),
                            );
                            if let Some(tex) =
                                self.icons.get_for_entry(&path.to_string_lossy(), is_dir)
                            {
                                ui.painter()
                                    .image(tex.id(), icon_rect, uv, egui::Color32::WHITE);
                            }
                            ui.add_space(76.0);
                        }
                        ui.label(egui::RichText::new(&name).size(14.0).strong().color(fg));
                        let kind = if is_drive {
                            "로컬 디스크".to_string()
                        } else if is_dir {
                            "폴더".to_string()
                        } else {
                            path.extension()
                                .map(|x| format!("{} 파일", x.to_string_lossy().to_uppercase()))
                                .unwrap_or_else(|| "파일".into())
                        };
                        ui.label(egui::RichText::new(kind).size(12.0).color(fg_dim));
                    });
                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(6.0);
                    let field = |ui: &mut egui::Ui, k: &str, v: String| {
                        ui.label(egui::RichText::new(k).size(11.0).color(fg_dim));
                        ui.label(egui::RichText::new(v).size(12.5).color(fg));
                        ui.add_space(6.0);
                    };
                    if !is_dir {
                        field(ui, "크기", humanize(size));
                    }
                    if is_drive {
                        if let Some(d) = self.drives.iter().find(|d| d.root == path) {
                            let used = d.total.saturating_sub(d.free);
                            let frac = used as f32 / d.total.max(1) as f32;
                            field(
                                ui,
                                "사용 공간",
                                format!("{} / {}", humanize(used), humanize(d.total)),
                            );
                            let (bar, _) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), 6.0),
                                egui::Sense::hover(),
                            );
                            ui.painter()
                                .rect_filled(bar, 3.0, crate::theme::alpha(fg_dim, 45));
                            let mut fill = bar;
                            fill.set_right(bar.min.x + bar.width() * frac);
                            let col = if frac > 0.9 {
                                egui::Color32::from_rgb(220, 90, 90)
                            } else {
                                self.accent
                            };
                            ui.painter().rect_filled(fill, 3.0, col);
                            ui.add_space(8.0);
                            field(ui, "여유 공간", humanize(d.free));
                        }
                    }
                    if let Some(m) = modified {
                        let dt: chrono::DateTime<chrono::Local> = m.into();
                        field(
                            ui,
                            "수정한 날짜",
                            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
                        );
                    }
                    field(ui, "경로", path.display().to_string());
                });
        }

        // ---------- status bar ----------
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::TRANSPARENT)
                    .inner_margin(6),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let p = self.cur_ref();
                    let count = p.marked.len();
                    let info = if count > 1 {
                        format!("{}개 항목    {}개 선택됨", p.entries.len(), count)
                    } else {
                        let sel_name = p
                            .entries
                            .get(p.sel)
                            .filter(|_| count == 1)
                            .map(|e| e.name.clone())
                            .unwrap_or_default();
                        format!("{}개 항목    {}", p.entries.len(), sel_name)
                    };
                    ui.label(egui::RichText::new(info).size(12.0).color(fg_dim));
                    if let Some(c) = &self.clip {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let verb = if c.cut {
                                "이동 대기"
                            } else {
                                "복사 대기"
                            };
                            let what = if c.paths.len() == 1 {
                                c.paths[0]
                                    .file_name()
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_default()
                            } else {
                                format!("{}개 항목", c.paths.len())
                            };
                            ui.label(
                                egui::RichText::new(format!("{what} — {verb} (Ctrl+V)"))
                                    .size(12.0)
                                    .color(fg_dim),
                            );
                        });
                    }
                });
            });

        // ---------- listing ----------
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::TRANSPARENT)
                    .inner_margin(8),
            )
            .show(ctx, |ui| {
                let full_h = ui.available_height();
                egui::Frame::new()
                    .fill(panel_bg)
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::same(6))
                    .show(ui, |ui| {
                        ui.set_min_height(full_h - 14.0);
                        ui.set_max_height(full_h - 14.0);
                        self.listing_ui(ui, ctx, fg, fg_dim, hover, sel_bg);
                    });
            });

        // ---------- internal drag: ghost + drop ----------
        if let Some(src) = self.drag.clone() {
            if ops::cursor_outside_window(self.hwnd) {
                // Pointer left the window mid-drag → hand the selection to the OS
                // as a real OLE drag so it can drop into other apps. Modal call.
                let paths = self.cur_ref().selected_paths();
                self.drag = None;
                self.drag_target = None;
                ops::ole_drag_out(self.hwnd, &paths);
                ctx.request_repaint();
            } else {
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    egui::Id::new("drag_ghost"),
                ));
                if let Some(ptr) = ctx.input(|i| i.pointer.interact_pos()) {
                    let name = src
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let verb = if self.drag_target.is_none() {
                        ""
                    } else if ctx.input(|i| i.modifiers.ctrl) {
                        "  —  복사"
                    } else {
                        "  —  이동"
                    };
                    let galley = painter.layout_no_wrap(
                        format!("{name}{verb}"),
                        egui::FontId::proportional(12.5),
                        fg,
                    );
                    let pos = ptr + egui::vec2(14.0, 16.0);
                    let pill =
                        egui::Rect::from_min_size(pos, galley.size() + egui::vec2(20.0, 12.0));
                    painter.rect_filled(pill, 6.0, panel_bg.to_opaque());
                    painter.rect_stroke(
                        pill,
                        6.0,
                        egui::Stroke::new(1.0, self.accent),
                        egui::StrokeKind::Inside,
                    );
                    painter.galley(pos + egui::vec2(10.0, 6.0), galley, fg);
                }
                if ctx.input(|i| i.pointer.any_released()) {
                    let target = self.drag_target.take();
                    self.drag = None;
                    if let Some(dest) = target {
                        self.drop_onto(ctx, dest);
                    }
                }
            }
        }

        // ---------- external drag-over hint ----------
        if hovering_external {
            let screen = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("drop_hint"),
            ));
            let pill = egui::Rect::from_center_size(
                egui::pos2(screen.center().x, screen.max.y - 48.0),
                egui::vec2(240.0, 40.0),
            );
            painter.rect_filled(pill, 8.0, panel_bg.to_opaque());
            painter.rect_stroke(
                pill,
                8.0,
                egui::Stroke::new(1.2, self.accent),
                egui::StrokeKind::Inside,
            );
            painter.text(
                pill.center(),
                egui::Align2::CENTER_CENTER,
                "놓으면 현재 폴더로 복사",
                egui::FontId::proportional(13.0),
                fg,
            );
        }

        // ---------- Ctrl+P quick-jump finder overlay ----------
        self.finder_ui(ctx, fg, fg_dim, panel_bg);

        // ---------- deferred native context menu ----------
        // Opened one frame late so the click's selection highlight is on screen
        // before TrackPopupMenuEx blocks the loop.
        if let Some((_, fresh)) = &mut self.pending_menu {
            if *fresh {
                *fresh = false;
                ctx.request_repaint();
            } else if let Some((req, _)) = self.pending_menu.take() {
                match req {
                    PendingMenu::Item(i) => self.row_shell_menu(ctx, i),
                    PendingMenu::Background => self.background_shell_menu(ctx),
                }
            }
        }
    }
}

fn sidebar_row(
    ui: &mut egui::Ui,
    glyph: &str,
    label: &str,
    fg: egui::Color32,
    fg_dim: egui::Color32,
    hover: egui::Color32,
    active: bool,
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), egui::Sense::click());
    // Hover eases in; the "you are here" row carries a steady accent wash.
    let t = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let p = ui.painter();
    if active {
        p.rect_filled(rect, 5.0, crate::theme::alpha(crate::theme::ACCENT, 38));
    }
    if t > 0.0 {
        p.rect_filled(rect, 5.0, hover.gamma_multiply(t));
    }
    let glyph_col = if active { crate::theme::ACCENT } else { fg_dim };
    p.text(
        egui::pos2(rect.min.x + 16.0, rect.center().y),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(14.0),
        glyph_col,
    );
    p.text(
        egui::pos2(rect.min.x + 30.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.0),
        fg,
    );
    resp
}

impl GlideApp {
    fn listing_ui(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        fg: egui::Color32,
        fg_dim: egui::Color32,
        hover: egui::Color32,
        sel_bg: egui::Color32,
    ) {
        if let Some(err) = self.cur_ref().err.clone() {
            ui.colored_label(egui::Color32::from_rgb(220, 90, 90), err);
            return;
        }

        // Empty-area interactions: right-click → folder menu, click → deselect,
        // drag → rubber-band. Registered before the header/rows so those win
        // their own presses; only genuinely empty space falls through to here.
        let bg = ui.interact(
            ui.available_rect_before_wrap(),
            ui.id().with("listing_bg"),
            egui::Sense::click_and_drag(),
        );
        if bg.drag_started() {
            if let Some(start) = ctx.input(|i| i.pointer.press_origin()) {
                let base = if ctx.input(|i| i.modifiers.command) {
                    self.cur_ref().marked.clone()
                } else {
                    BTreeSet::new()
                };
                self.marquee = Some((start, base));
            }
        }
        if bg.clicked() {
            self.cur().marked.clear();
        }
        let marquee_rect = self.marquee.as_ref().and_then(|(start, _)| {
            ctx.input(|i| i.pointer.interact_pos())
                .map(|cur| egui::Rect::from_two_pos(*start, cur))
        });

        let mut h = Hits::default();
        let mods = ctx.input(|i| i.modifiers);
        if self.view != ViewMode::Details {
            self.view_grid(
                ui,
                ctx,
                &mut h,
                fg,
                fg_dim,
                hover,
                sel_bg,
                marquee_rect,
                self.view == ViewMode::Icons,
            );
        } else {
            // Column header — clickable sort, Explorer-style (click again flips order).
            {
                let (hrect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 26.0),
                    egui::Sense::hover(),
                );
                // Faint band sets the header apart from the rows below it.
                ui.painter().rect_filled(hrect, 0.0, crate::theme::HEADER);
                let zones = [
                    (
                        SortKey::Name,
                        "이름",
                        egui::Rect::from_min_max(
                            egui::pos2(hrect.min.x + 8.0, hrect.min.y),
                            egui::pos2(hrect.max.x - 205.0, hrect.max.y),
                        ),
                        true,
                    ),
                    (
                        SortKey::Date,
                        "수정한 날짜",
                        egui::Rect::from_min_max(
                            egui::pos2(hrect.max.x - 205.0, hrect.min.y),
                            egui::pos2(hrect.max.x - 88.0, hrect.max.y),
                        ),
                        false,
                    ),
                    (
                        SortKey::Size,
                        "크기",
                        egui::Rect::from_min_max(
                            egui::pos2(hrect.max.x - 88.0, hrect.min.y),
                            egui::pos2(hrect.max.x - 8.0, hrect.max.y),
                        ),
                        false,
                    ),
                ];
                let (cur_sort, cur_asc) = {
                    let pane = &self.tabs[self.tab];
                    (pane.sort, pane.asc)
                };
                let mut new_sort: Option<SortKey> = None;
                for (key, label, zone, left) in zones {
                    let resp =
                        ui.interact(zone, ui.id().with(("colhdr", label)), egui::Sense::click());
                    if resp.hovered() {
                        ui.painter().rect_filled(zone, 4.0, hover);
                    }
                    let active = cur_sort == key;
                    let (pos, align) = if left {
                        (
                            egui::pos2(hrect.min.x + 34.0, hrect.center().y),
                            egui::Align2::LEFT_CENTER,
                        )
                    } else {
                        (
                            egui::pos2(zone.max.x, hrect.center().y),
                            egui::Align2::RIGHT_CENTER,
                        )
                    };
                    let text_rect = ui.painter().text(
                        pos,
                        align,
                        label,
                        egui::FontId::proportional(12.5),
                        if active { fg } else { fg_dim },
                    );
                    if active {
                        let glyph = if cur_asc {
                            GLYPH_SORT_UP
                        } else {
                            GLYPH_SORT_DOWN
                        };
                        let (apos, aalign) = if left {
                            (
                                egui::pos2(text_rect.max.x + 7.0, hrect.center().y),
                                egui::Align2::LEFT_CENTER,
                            )
                        } else {
                            (
                                egui::pos2(text_rect.min.x - 7.0, hrect.center().y),
                                egui::Align2::RIGHT_CENTER,
                            )
                        };
                        ui.painter().text(
                            apos,
                            aalign,
                            glyph,
                            egui::FontId::proportional(9.0),
                            self.accent,
                        );
                    }
                    if resp.clicked() {
                        new_sort = Some(key);
                    }
                }
                ui.painter().hline(
                    hrect.min.x..=hrect.max.x,
                    hrect.max.y - 0.5,
                    egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 28),
                    ),
                );
                if let Some(key) = new_sort {
                    self.set_sort_key(key);
                }
            }

            let n = self.cur_ref().entries.len();
            let drag_src = self.drag.clone();
            let drag_ptr = drag_src
                .as_ref()
                .and_then(|_| ctx.input(|i| i.pointer.interact_pos()));
            let accent = self.accent;

            egui::ScrollArea::vertical()
                .id_salt(("tab", self.tab))
                .auto_shrink([false, false])
                .show_rows(ui, ROW_H, n, |ui, range| {
                    for i in range {
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), ROW_H),
                            egui::Sense::click_and_drag(),
                        );
                        if let Some(mq) = marquee_rect {
                            if rect.intersects(mq) {
                                h.mq_hits.push(i);
                            }
                        }
                        let pane = &self.tabs[self.tab];
                        let e = &pane.entries[i];
                        let selected = pane.marked.contains(&i);
                        // Select fill + accent pill grow in; hover wash eases.
                        let sel_t = ui.ctx().animate_bool_with_time(resp.id, selected, 0.12);
                        if sel_t > 0.0 {
                            ui.painter()
                                .rect_filled(rect, 5.0, sel_bg.gamma_multiply(sel_t));
                            let pill = egui::Rect::from_center_size(
                                egui::pos2(rect.min.x + 1.5, rect.center().y),
                                egui::vec2(3.0, ROW_H * 0.5 * sel_t),
                            );
                            ui.painter().rect_filled(pill, 2.0, self.accent);
                        } else {
                            let hv = ui.ctx().animate_bool_with_time(
                                resp.id.with("hv"),
                                resp.hovered(),
                                0.12,
                            );
                            if hv > 0.0 {
                                ui.painter()
                                    .rect_filled(rect, 5.0, hover.gamma_multiply(hv));
                            }
                        }
                        // With several rows selected, ring the keyboard cursor row.
                        if i == pane.sel && pane.marked.len() > 1 {
                            ui.painter().rect_stroke(
                                rect.shrink(1.0),
                                5.0,
                                egui::Stroke::new(1.0, self.accent),
                                egui::StrokeKind::Inside,
                            );
                        }

                        let icon_rect = egui::Rect::from_center_size(
                            egui::pos2(rect.min.x + 18.0, rect.center().y),
                            egui::vec2(18.0, 18.0),
                        );
                        if let Some(tex) = self
                            .icons
                            .get_for_entry(&e.path.to_string_lossy(), e.is_dir)
                        {
                            ui.painter().image(
                                tex.id(),
                                icon_rect,
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }

                        let editing_this = self.edit.as_ref().is_some_and(|ed| ed.target == e.path);
                        if editing_this {
                            let ed = self.edit.as_mut().unwrap();
                            let resp = ui.put(
                                egui::Rect::from_min_max(
                                    egui::pos2(rect.min.x + 32.0, rect.min.y + 2.0),
                                    egui::pos2(rect.max.x - 205.0, rect.max.y - 2.0),
                                ),
                                egui::TextEdit::singleline(&mut ed.buf)
                                    .font(egui::FontId::proportional(14.0)),
                            );
                            if ed.focus {
                                resp.request_focus();
                                ed.focus = false;
                            }
                            if resp.lost_focus() {
                                // Escape is handled globally (cancels before we get here).
                                h.edit_action = Some(true);
                            }
                        } else {
                            // Cut items render dimmed, Explorer-style.
                            let is_cut = self
                                .clip
                                .as_ref()
                                .is_some_and(|c| c.cut && c.paths.contains(&e.path));
                            let name_color = if e.hidden || is_cut { fg_dim } else { fg };
                            // Clip long names so they don't run into the date/size columns.
                            let name_clip = egui::Rect::from_min_max(
                                egui::pos2(rect.min.x + 34.0, rect.min.y),
                                egui::pos2(rect.max.x - 205.0, rect.max.y),
                            );
                            ui.painter_at(name_clip).text(
                                egui::pos2(rect.min.x + 34.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                &e.name,
                                egui::FontId::proportional(14.0),
                                name_color,
                            );
                        }

                        let mut right = rect.max.x - 8.0;
                        if !e.is_dir {
                            ui.painter().text(
                                egui::pos2(right, rect.center().y),
                                egui::Align2::RIGHT_CENTER,
                                humanize(e.size),
                                egui::FontId::proportional(12.0),
                                fg_dim,
                            );
                        }
                        right -= 80.0;
                        if let Some(m) = e.modified {
                            let dt: chrono::DateTime<chrono::Local> = m.into();
                            ui.painter().text(
                                egui::pos2(right, rect.center().y),
                                egui::Align2::RIGHT_CENTER,
                                dt.format("%Y-%m-%d %H:%M").to_string(),
                                egui::FontId::proportional(12.0),
                                fg_dim,
                            );
                        }

                        // Right-click opens the native shell menu next frame (so the
                        // highlight paints first). Clicking outside the current
                        // selection reduces it to this row; inside it, the set stays.
                        if resp.secondary_clicked() {
                            if !pane.marked.contains(&i) {
                                h.menu_select = Some(i);
                            }
                            h.menu_row = Some(i);
                        }
                        // Drag source + drop target (directories only).
                        if resp.drag_started() {
                            h.drag_begin = Some(i);
                        }
                        if let Some(ptr) = drag_ptr {
                            if e.is_dir
                                && rect.contains(ptr)
                                && drag_src.as_deref() != Some(e.path.as_path())
                            {
                                h.row_drop = Some(e.path.clone());
                                ui.painter().rect_stroke(
                                    rect.shrink(1.0),
                                    5.0,
                                    egui::Stroke::new(1.5, accent),
                                    egui::StrokeKind::Inside,
                                );
                            }
                        }

                        let scroll_flag = self.tabs[self.tab].scroll_to_sel;
                        if i == self.tabs[self.tab].sel && scroll_flag {
                            resp.scroll_to_me(None);
                            self.tabs[self.tab].scroll_to_sel = false;
                        }
                        if resp.clicked() {
                            h.clicked = Some(i);
                        }
                        if resp.double_clicked() {
                            h.dbl = Some(i);
                        }
                    }
                });
        }

        // Rubber-band: cells the band touches, unioned with the kept base.
        let mq_base = self.marquee.as_ref().map(|(_, base)| base.clone());
        if let Some(base) = mq_base {
            let mut set = base;
            set.extend(h.mq_hits.iter().copied());
            if let Some(&last) = h.mq_hits.last() {
                self.cur().sel = last;
            }
            self.cur().marked = set;
        }
        if let Some(mq) = marquee_rect {
            let fill = egui::Color32::from_rgba_unmultiplied(
                self.accent.r(),
                self.accent.g(),
                self.accent.b(),
                40,
            );
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("marquee"),
            ));
            painter.rect_filled(mq, 2.0, fill);
            painter.rect_stroke(
                mq,
                2.0,
                egui::Stroke::new(1.0, self.accent),
                egui::StrokeKind::Inside,
            );
        }
        if self.marquee.is_some() && ctx.input(|i| i.pointer.any_released()) {
            self.marquee = None;
        }

        if let Some(commit) = h.edit_action {
            if commit {
                self.commit_rename();
            } else {
                self.edit = None;
            }
        }
        if let Some(i) = h.menu_select {
            self.cur().select_only(i);
        }
        if let Some(i) = h.clicked {
            let pane = self.cur();
            if mods.command && mods.shift {
                pane.add_range(i);
            } else if mods.command {
                pane.toggle(i);
            } else if mods.shift {
                pane.select_range(i);
            } else {
                pane.select_only(i);
            }
        }
        if let Some(i) = h.dbl {
            self.cur().select_only(i);
            self.open_entry(i);
        }
        if let Some(i) = h.drag_begin {
            if !self.cur_ref().marked.contains(&i) {
                self.cur().select_only(i);
            }
            self.drag = self.entry_path(i);
        }
        if let Some(drop) = h.row_drop.take() {
            self.drag_target = Some(drop);
        }
        if let Some(i) = h.menu_row {
            self.pending_menu = Some((PendingMenu::Item(i), true));
        } else if bg.secondary_clicked() {
            self.pending_menu = Some((PendingMenu::Background, true));
        }
    }

    // Icon/list grid: a wrapping, virtualized grid of cells sharing the details
    // views' selection/drag/menu contract via `h`. `big` = large thumbnails with
    // the name beneath; otherwise a compact icon + name row (multi-column).
    fn view_grid(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        h: &mut Hits,
        fg: egui::Color32,
        fg_dim: egui::Color32,
        hover: egui::Color32,
        sel_bg: egui::Color32,
        marquee_rect: Option<egui::Rect>,
        big: bool,
    ) {
        let n = self.cur_ref().entries.len();
        let drag_src = self.drag.clone();
        let drag_ptr = drag_src
            .as_ref()
            .and_then(|_| ctx.input(|i| i.pointer.interact_pos()));
        let accent = self.accent;

        let (cell_w, cell_h) = if big { (116.0, 122.0) } else { (232.0, 26.0) };
        let avail = (ui.available_width() - 2.0).max(cell_w);
        let cols = ((avail / cell_w).floor() as usize).max(1);
        self.grid_cols = cols;
        let grid_rows = n.div_ceil(cols);

        egui::ScrollArea::vertical()
            .id_salt(("grid", self.tab, big))
            .auto_shrink([false, false])
            .show_rows(ui, cell_h, grid_rows, |ui, rrange| {
                for gr in rrange {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        for col in 0..cols {
                            let i = gr * cols + col;
                            if i >= n {
                                break;
                            }
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(cell_w, cell_h),
                                egui::Sense::click_and_drag(),
                            );
                            if let Some(mq) = marquee_rect {
                                if rect.intersects(mq) {
                                    h.mq_hits.push(i);
                                }
                            }
                            // Snapshot the fields the &mut self painters need so
                            // they don't clash with the `entries` borrow.
                            let (path, name, is_dir, hidden, modified, selected, cursor, multi) = {
                                let pane = &self.tabs[self.tab];
                                let e = &pane.entries[i];
                                (
                                    e.path.clone(),
                                    e.name.clone(),
                                    e.is_dir,
                                    e.hidden,
                                    e.modified,
                                    pane.marked.contains(&i),
                                    i == pane.sel,
                                    pane.marked.len() > 1,
                                )
                            };
                            let inner = rect.shrink(if big { 4.0 } else { 2.0 });
                            let sel_t = ui.ctx().animate_bool_with_time(resp.id, selected, 0.12);
                            if sel_t > 0.0 {
                                ui.painter()
                                    .rect_filled(inner, 6.0, sel_bg.gamma_multiply(sel_t));
                            } else {
                                let hv = ui.ctx().animate_bool_with_time(
                                    resp.id.with("hv"),
                                    resp.hovered(),
                                    0.12,
                                );
                                if hv > 0.0 {
                                    ui.painter()
                                        .rect_filled(inner, 6.0, hover.gamma_multiply(hv));
                                }
                            }
                            if cursor && multi {
                                ui.painter().rect_stroke(
                                    inner,
                                    6.0,
                                    egui::Stroke::new(1.0, accent),
                                    egui::StrokeKind::Inside,
                                );
                            }

                            let is_cut = self
                                .clip
                                .as_ref()
                                .is_some_and(|c| c.cut && c.paths.contains(&path));
                            let name_color = if hidden || is_cut { fg_dim } else { fg };

                            if big {
                                let img_box = egui::Rect::from_center_size(
                                    egui::pos2(rect.center().x, rect.min.y + 46.0),
                                    egui::vec2(66.0, 66.0),
                                );
                                self.paint_thumb(ui, &path, is_dir, modified, img_box, 48.0, true);
                                let name_rect = egui::Rect::from_min_max(
                                    egui::pos2(rect.min.x + 4.0, rect.min.y + 82.0),
                                    egui::pos2(rect.max.x - 4.0, rect.max.y - 3.0),
                                );
                                if !self.paint_edit(ui, &path, name_rect, h) {
                                    let mut job = egui::text::LayoutJob::simple(
                                        name.clone(),
                                        egui::FontId::proportional(12.5),
                                        name_color,
                                        name_rect.width(),
                                    );
                                    job.halign = egui::Align::Center;
                                    job.wrap = egui::text::TextWrapping {
                                        max_width: name_rect.width(),
                                        max_rows: 2,
                                        break_anywhere: true,
                                        overflow_character: Some('…'),
                                    };
                                    let galley = ui.painter().layout_job(job);
                                    ui.painter_at(name_rect).galley(
                                        egui::pos2(name_rect.center().x, name_rect.min.y),
                                        galley,
                                        name_color,
                                    );
                                }
                            } else {
                                let icon_box = egui::Rect::from_center_size(
                                    egui::pos2(rect.min.x + 16.0, rect.center().y),
                                    egui::vec2(16.0, 16.0),
                                );
                                self.paint_thumb(ui, &path, is_dir, None, icon_box, 16.0, false);
                                let name_rect = egui::Rect::from_min_max(
                                    egui::pos2(rect.min.x + 30.0, rect.min.y),
                                    egui::pos2(rect.max.x - 6.0, rect.max.y),
                                );
                                if !self.paint_edit(ui, &path, name_rect, h) {
                                    ui.painter_at(name_rect).text(
                                        egui::pos2(name_rect.min.x, rect.center().y),
                                        egui::Align2::LEFT_CENTER,
                                        &name,
                                        egui::FontId::proportional(13.5),
                                        name_color,
                                    );
                                }
                            }

                            // Interactions — same contract as the details rows.
                            if resp.secondary_clicked() {
                                if !selected {
                                    h.menu_select = Some(i);
                                }
                                h.menu_row = Some(i);
                            }
                            if resp.drag_started() {
                                h.drag_begin = Some(i);
                            }
                            if let Some(ptr) = drag_ptr {
                                if is_dir
                                    && rect.contains(ptr)
                                    && drag_src.as_deref() != Some(path.as_path())
                                {
                                    h.row_drop = Some(path.clone());
                                    ui.painter().rect_stroke(
                                        inner,
                                        6.0,
                                        egui::Stroke::new(1.5, accent),
                                        egui::StrokeKind::Inside,
                                    );
                                }
                            }
                            let scroll_flag = self.tabs[self.tab].scroll_to_sel;
                            if cursor && scroll_flag {
                                resp.scroll_to_me(None);
                                self.tabs[self.tab].scroll_to_sel = false;
                            }
                            if resp.clicked() {
                                h.clicked = Some(i);
                            }
                            if resp.double_clicked() {
                                h.dbl = Some(i);
                            }
                        }
                    });
                }
            });
    }

    // Shell thumbnail if available (images/video/PDF…), else the file-type icon.
    // `want_thumb` is false for the compact list, which only ever wants icons.
    fn paint_thumb(
        &mut self,
        ui: &egui::Ui,
        path: &Path,
        is_dir: bool,
        modified: Option<std::time::SystemTime>,
        area: egui::Rect,
        icon_size: f32,
        want_thumb: bool,
    ) {
        if want_thumb && !is_dir {
            if let Some(tex) = self.thumbs.get(&path.to_string_lossy(), modified).flatten() {
                let sz = tex.size_vec2();
                if sz.x > 0.0 && sz.y > 0.0 {
                    let scale = (area.width() / sz.x).min(area.height() / sz.y);
                    let r = egui::Rect::from_center_size(area.center(), sz * scale);
                    ui.painter()
                        .image(tex.id(), r, uv01(), egui::Color32::WHITE);
                    return;
                }
            }
        }
        if let Some(tex) = self.icons.get_for_entry(&path.to_string_lossy(), is_dir) {
            let r = egui::Rect::from_center_size(area.center(), egui::vec2(icon_size, icon_size));
            ui.painter()
                .image(tex.id(), r, uv01(), egui::Color32::WHITE);
        }
    }

    // Inline rename box for the entry at `path`, shared by every view.
    // Returns true when this entry is the one being renamed (and drew the box).
    fn paint_edit(
        &mut self,
        ui: &mut egui::Ui,
        path: &Path,
        rect: egui::Rect,
        h: &mut Hits,
    ) -> bool {
        if !self.edit.as_ref().is_some_and(|ed| ed.target == *path) {
            return false;
        }
        let ed = self.edit.as_mut().unwrap();
        let resp = ui.put(
            rect,
            egui::TextEdit::singleline(&mut ed.buf).font(egui::FontId::proportional(13.0)),
        );
        if ed.focus {
            resp.request_focus();
            ed.focus = false;
        }
        if resp.lost_focus() {
            h.edit_action = Some(true);
        }
        true
    }

    // Ctrl+P overlay: type → Everything results → Enter/click jumps there.
    fn finder_ui(
        &mut self,
        ctx: &egui::Context,
        fg: egui::Color32,
        fg_dim: egui::Color32,
        panel_bg: egui::Color32,
    ) {
        // Drive the enter/exit animation off open-state every frame (even while
        // closed) so it resets to 0 and fades in fresh on the next open.
        let t =
            ctx.animate_bool_with_time(egui::Id::new("finder_open"), self.finder.is_some(), 0.11);
        if self.finder.is_none() {
            return;
        }
        let accent = self.accent;
        let screen = ctx.content_rect();
        // Enter animation: backdrop fades, the panel drops in a few px.
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("finder_dim"),
        ))
        .rect_filled(
            screen,
            0.0,
            egui::Color32::from_black_alpha((120.0 * t) as u8),
        );

        let mut close = false;
        let mut activate: Option<PathBuf> = None;

        egui::Window::new("finder")
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .order(egui::Order::Foreground)
            .anchor(
                egui::Align2::CENTER_TOP,
                egui::vec2(0.0, 96.0 - (1.0 - t) * 10.0),
            )
            .fixed_size(egui::vec2(660.0, 0.0))
            .frame(
                egui::Frame::new()
                    .fill(panel_bg.to_opaque())
                    .corner_radius(12.0)
                    .stroke(egui::Stroke::new(1.0, accent))
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(ctx, |ui| {
                let f = self.finder.as_mut().unwrap();
                ui.horizontal(|ui| {
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new(GLYPH_SEARCH).size(16.0).color(fg_dim));
                    ui.add_space(6.0);
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut f.buf)
                            .hint_text("어디서든 파일·폴더 검색  ·  Everything")
                            .frame(false)
                            .desired_width(f32::INFINITY)
                            .font(egui::FontId::proportional(16.0)),
                    );
                    if f.focus {
                        resp.request_focus();
                        f.focus = false;
                    }
                });

                if f.buf != f.last_query {
                    self.file_search.query(f.buf.trim());
                    f.last_query = f.buf.clone();
                    f.sel = 0;
                }
                let items = self.file_search.results.lock().unwrap().1.clone();
                let n = items.len();
                if f.sel >= n {
                    f.sel = n.saturating_sub(1);
                }
                let (down, up, enter, esc) = ctx.input(|i| {
                    (
                        i.key_pressed(egui::Key::ArrowDown),
                        i.key_pressed(egui::Key::ArrowUp),
                        i.key_pressed(egui::Key::Enter),
                        i.key_pressed(egui::Key::Escape),
                    )
                });
                if down && n > 0 {
                    f.sel = (f.sel + 1).min(n - 1);
                }
                if up {
                    f.sel = f.sel.saturating_sub(1);
                }
                if esc {
                    close = true;
                }
                if enter && n > 0 {
                    activate = Some(PathBuf::from(&items[f.sel].target));
                }
                let sel = f.sel;

                if !items.is_empty() {
                    ui.add_space(4.0);
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .max_height(440.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (i, it) in items.iter().enumerate() {
                                let (rect, resp) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), 42.0),
                                    egui::Sense::click(),
                                );
                                if i == sel {
                                    ui.painter().rect_filled(
                                        rect,
                                        6.0,
                                        egui::Color32::from_rgba_unmultiplied(
                                            accent.r(),
                                            accent.g(),
                                            accent.b(),
                                            40,
                                        ),
                                    );
                                    let pill = egui::Rect::from_min_size(
                                        rect.min + egui::vec2(0.0, 9.0),
                                        egui::vec2(3.0, 24.0),
                                    );
                                    ui.painter().rect_filled(pill, 2.0, accent);
                                } else if resp.hovered() {
                                    ui.painter().rect_filled(
                                        rect,
                                        6.0,
                                        egui::Color32::from_white_alpha(10),
                                    );
                                }
                                let is_dir = Path::new(&it.target).is_dir();
                                let icon_rect = egui::Rect::from_center_size(
                                    egui::pos2(rect.min.x + 21.0, rect.center().y),
                                    egui::vec2(20.0, 20.0),
                                );
                                if let Some(tex) = self.icons.get_for_entry(&it.target, is_dir) {
                                    ui.painter().image(
                                        tex.id(),
                                        icon_rect,
                                        uv01(),
                                        egui::Color32::WHITE,
                                    );
                                }
                                ui.painter().text(
                                    egui::pos2(rect.min.x + 42.0, rect.center().y - 8.0),
                                    egui::Align2::LEFT_CENTER,
                                    &it.title,
                                    egui::FontId::proportional(14.0),
                                    fg,
                                );
                                ui.painter().text(
                                    egui::pos2(rect.min.x + 42.0, rect.center().y + 10.0),
                                    egui::Align2::LEFT_CENTER,
                                    &it.subtitle,
                                    egui::FontId::proportional(11.5),
                                    fg_dim,
                                );
                                if resp.clicked() || resp.double_clicked() {
                                    activate = Some(PathBuf::from(&it.target));
                                }
                            }
                        });
                }
            });

        if let Some(p) = activate {
            self.finder = None;
            self.reveal_path(p);
        } else if close {
            self.finder = None;
        }
    }

    // Jump to a path from the finder: enter a dir, or open a file's folder + select it.
    fn reveal_path(&mut self, path: PathBuf) {
        if path.is_dir() {
            self.navigate_active(path);
        } else if let Some(parent) = path.parent().map(|p| p.to_path_buf()) {
            self.navigate_active(parent);
            if let Some(i) = self.cur_ref().entries.iter().position(|e| e.path == path) {
                self.cur().select_only(i);
                self.cur().scroll_to_sel = true;
            }
        }
    }
}
