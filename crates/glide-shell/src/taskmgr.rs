//! glide-native Task Manager (SHELL_DESIGN §9) — modelled on the Windows 11
//! process view for scannability, with Process-Hacker-grade control underneath.
//!
//! 프로세스 tab groups processes the way stock Task Manager does: a windowed app
//! and its whole child-process subtree collapse into one "앱" row (friendly
//! name + process count + summed CPU/memory), expandable by its chevron;
//! background processes fold by product name. CPU and memory cells carry a heat
//! tint so the heavy consumers pop. 서비스 tab is the SCM list with start/stop.
//!
//! Control is two-step — a click selects a row (or group), a footer button acts
//! on it: terminate / kill-tree / suspend / resume / priority. A group action
//! applies to every member.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ELLIPSE,
    D2D1_INTERPOLATION_MODE_LINEAR, ID2D1Bitmap1,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_TRAILING,
    DWRITE_WORD_WRAPPING_NO_WRAP, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dwm::{
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;
use windows_numerics::Vector2;

use crate::icons;
use crate::procs::{self, PRIORITIES, Proc};
use crate::render::{Renderer, fill_round, rect};
use crate::services::{self, Svc};
use crate::theme;

const WM_MOUSELEAVE: u32 = 0x02A3;
/// Posted by the sampler thread once a snapshot is waiting in the inbox.
const WM_SNAPSHOT: u32 = WM_APP + 1;
const ROW_H: f32 = 30.0;
const REFRESH_MS: u32 = 1500;
const WIN_W: f32 = 940.0;
const WIN_H: f32 = 620.0;

/// What the sampler thread is asked to produce, and hands back.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Procs,
    Svcs,
}

enum Snap {
    Procs(Vec<Proc>, HashSet<u32>),
    Svcs(Vec<Svc>),
}

/// Own the sampler on a worker thread and answer requests on it.
///
/// A cold enumeration costs ~1.3 s on this box — the per-process version
/// resource read (`FileDescription`, which is what gives a group its friendly
/// name) is disk I/O and dominates. Run on the UI thread that is a frozen
/// window for the whole first second, so the sampler lives here instead and
/// posts `WM_SNAPSHOT` when a result is ready. The caches inside `Sampler` mean
/// every later refresh is ~20 ms, but it stays off the UI thread regardless:
/// a machine that spawns processes in bulk pays the cold price again.
fn spawn_sampler(hwnd: isize, inbox: Arc<Mutex<Vec<Snap>>>) -> Sender<Kind> {
    let (tx, rx) = channel::<Kind>();
    std::thread::spawn(move || {
        let mut sampler = procs::Sampler::new();
        while let Ok(kind) = rx.recv() {
            let snap = match kind {
                Kind::Procs => Snap::Procs(sampler.sample(), procs::app_pids()),
                Kind::Svcs => Snap::Svcs(services::list()),
            };
            let Ok(mut slot) = inbox.lock() else { return };
            slot.push(snap);
            drop(slot);
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut std::ffi::c_void)),
                    WM_SNAPSHOT,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    });
    tx
}

#[derive(Clone, Copy, PartialEq)]
enum Act {
    Tab(usize),
    ModeToggle,
    Search,
    /// Select a leaf process by PID (the list reorders under auto-refresh).
    Proc(u32),
    /// Select a group by its index in `self.groups`.
    GroupSel(usize),
    /// Expand/collapse a group by its index.
    GroupToggle(usize),
    Svc(usize),
    Kill,
    KillTree,
    Suspend,
    Resume,
    Priority,
    SvcGo,
}

/// A collapsed app / product group. `members` are indices into `self.procs`.
struct Group {
    name: String,
    is_app: bool,
    members: Vec<usize>,
    cpu: f32,
    mem: u64,
    thr: u32,
    /// Image path of the group's representative process, for its icon.
    icon_path: String,
}

/// A drawn line in the process list.
enum Row {
    Head(&'static str, usize),
    /// A group header (index into `self.groups`).
    Group(usize),
    /// A process row: proc index, and whether it sits under an expanded group.
    Leaf(usize, bool),
}

/// Current process selection — a single process or a whole group.
#[derive(Clone, PartialEq)]
enum Sel {
    None,
    Pid(u32),
    Group(String),
}

/// Left-edge x of each numeric column; a cell runs to the next column's edge.
struct Cols {
    pid: f32,
    thr: f32,
    cpu: f32,
    mem: f32,
    user: f32,
}

pub struct TaskManagerApp {
    hwnd: HWND,
    renderer: Renderer,
    fmt_title: IDWriteTextFormat,
    fmt_tab: IDWriteTextFormat,
    fmt_head: IDWriteTextFormat,
    fmt_row: IDWriteTextFormat,
    fmt_num: IDWriteTextFormat,
    fmt_sub: IDWriteTextFormat,
    scale: f32,
    w: f32,
    h: f32,
    /// 0 = 프로세스, 1 = 서비스.
    tab: usize,
    /// true = grouped by app, false = flat 목록.
    grouped: bool,
    query: String,
    /// Current user's short name (lowercased) — own processes get an accent tint.
    me: String,
    procs: Vec<Proc>,
    /// PIDs owning a visible top-level window — grouped as "앱" at the top.
    apps: HashSet<u32>,
    groups: Vec<Group>,
    /// exe path → cached D2D icon (None = no icon), loaded lazily per visible row.
    icons: HashMap<String, Option<ID2D1Bitmap1>>,
    /// Group names currently expanded (persists across refresh).
    expanded: HashSet<String>,
    /// Column maxima for the heat tint, recomputed each refresh.
    max_cpu: f32,
    max_mem: u64,
    svcs: Vec<Svc>,
    /// Request channel to the sampler thread, and the slot it drops results in.
    req: Sender<Kind>,
    inbox: Arc<Mutex<Vec<Snap>>>,
    /// A request is in flight — the auto-refresh timer must not pile more on.
    pending: bool,
    /// false until the first process snapshot lands, so the list can say so
    /// instead of looking like a machine with no processes.
    loaded: bool,
    sel: Sel,
    sel_svc: Option<String>,
    scroll: f32,
    hover: Option<Act>,
    tracking: bool,
    hits: Vec<(D2D_RECT_F, Act)>,
    status: String,
}

impl TaskManagerApp {
    pub fn new(dpi: f32) -> anyhow::Result<Self> {
        unsafe {
            let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            let class = w!("glide_shell_taskmgr");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(taskmgr_wndproc),
                hInstance: hinstance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let scale = dpi / 96.0;
            let (w, h) = ((WIN_W * scale) as i32, (WIN_H * scale) as i32);
            let hwnd = CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP,
                class,
                w!("작업 관리자"),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                w,
                h,
                None,
                None,
                Some(hinstance.into()),
                None,
            )?;
            let dark: i32 = 1;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
            let backdrop: i32 = 2; // DWMSBT_MAINWINDOW — Mica
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &backdrop as *const _ as _, 4);

            // Start enumerating before the D2D device and fonts are built, so
            // the cold sample runs alongside window setup rather than after it.
            let inbox: Arc<Mutex<Vec<Snap>>> = Arc::new(Mutex::new(Vec::new()));
            let req = spawn_sampler(hwnd.0 as isize, inbox.clone());
            let _ = req.send(Kind::Procs);

            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let (cw, ch) = ((rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32);
            let renderer = Renderer::new(hwnd, cw, ch, dpi)?;

            let mk = |family, size, weight: DWRITE_FONT_WEIGHT| -> windows::core::Result<IDWriteTextFormat> {
                let f = renderer.dwrite.CreateTextFormat(
                    family,
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("ko-KR"),
                )?;
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                Ok(f)
            };
            let ui = w!("Segoe UI");
            let fmt_title = mk(ui, 20.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let fmt_tab = mk(ui, 13.5, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_tab.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_tab.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_head = mk(ui, 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            fmt_head.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_row = mk(ui, 13.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_row.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_num = mk(ui, 12.5, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_num.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;
            fmt_num.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_sub = mk(ui, 11.5, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_sub.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            let me = std::env::var("USERNAME").unwrap_or_default().to_lowercase();

            Ok(TaskManagerApp {
                hwnd,
                renderer,
                fmt_title,
                fmt_tab,
                fmt_head,
                fmt_row,
                fmt_num,
                fmt_sub,
                scale,
                w: cw as f32 / scale,
                h: ch as f32 / scale,
                tab: 0,
                grouped: true,
                query: String::new(),
                me,
                procs: Vec::new(),
                apps: HashSet::new(),
                groups: Vec::new(),
                icons: HashMap::new(),
                expanded: HashSet::new(),
                max_cpu: 1.0,
                max_mem: 1,
                svcs: Vec::new(),
                req,
                inbox,
                pending: true,
                loaded: false,
                sel: Sel::None,
                sel_svc: None,
                scroll: 0.0,
                hover: None,
                tracking: false,
                hits: Vec::new(),
                status: String::new(),
            })
        }
    }

    pub fn open(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, self as *mut TaskManagerApp as isize);
            if !IsWindowVisible(self.hwnd).as_bool() {
                let mut wa = RECT::default();
                let _ = SystemParametersInfoW(
                    SPI_GETWORKAREA,
                    0,
                    Some(&mut wa as *mut _ as _),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                );
                let (w, h) = ((WIN_W * self.scale) as i32, (WIN_H * self.scale) as i32);
                let x = wa.left + ((wa.right - wa.left) - w) / 2;
                let y = wa.top + ((wa.bottom - wa.top) - h) / 2;
                let _ = SetWindowPos(self.hwnd, None, x, y.max(wa.top), w, h, SWP_NOZORDER);
            }
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetForegroundWindow(self.hwnd);
            SetTimer(Some(self.hwnd), 1, REFRESH_MS, None);
        }
        // The prewarmed snapshot may have landed before GWLP_USERDATA was set,
        // in which case its WM_SNAPSHOT hit a null app pointer and was dropped.
        // Draining here is what makes that harmless.
        self.drain();
        self.paint();
    }

    fn hide(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), 1);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.hover = None;
    }

    fn resized(&mut self) {
        unsafe {
            let dpi = GetDpiForWindow(self.hwnd) as f32;
            self.scale = dpi / 96.0;
            let mut rc = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rc);
            let (cw, ch) = ((rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32);
            let _ = self.renderer.resize(cw, ch, dpi);
            self.w = cw as f32 / self.scale;
            self.h = ch as f32 / self.scale;
        }
        self.paint();
    }

    /// Ask the sampler thread for a fresh snapshot of the current tab. Never
    /// blocks; the list updates when `WM_SNAPSHOT` comes back.
    fn refresh(&mut self) {
        let kind = if self.tab == 0 { Kind::Procs } else { Kind::Svcs };
        if self.req.send(kind).is_ok() {
            self.pending = true;
        }
    }

    /// Apply everything the sampler thread has produced since the last drain.
    /// Returns true when something changed and the window needs a repaint.
    fn drain(&mut self) -> bool {
        let Ok(mut slot) = self.inbox.lock() else { return false };
        let snaps: Vec<Snap> = slot.drain(..).collect();
        drop(slot);
        if snaps.is_empty() {
            return false;
        }
        self.pending = false;
        for snap in snaps {
            match snap {
                Snap::Procs(procs, apps) => {
                    self.procs = procs;
                    self.apps = apps;
                    self.loaded = true;
                    self.build_groups();
                }
                Snap::Svcs(svcs) => self.svcs = svcs,
            }
        }
        true
    }

    /// Fold processes into one group per application (by friendly/product name,
    /// the way Task Manager consolidates "Zen (18)"), record the heat maxima. A
    /// group is an "앱" when any member owns a top-level window. Grouping by
    /// product — not by parent — keeps explorer.exe from swallowing every
    /// user-launched process into one giant row.
    fn build_groups(&mut self) {
        let procs = &self.procs;
        let mut map: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, p) in procs.iter().enumerate() {
            map.entry(dname(p)).or_default().push(i);
        }
        let mut groups: Vec<Group> = map
            .into_iter()
            .map(|(name, m)| {
                let is_app = m.iter().any(|&i| self.apps.contains(&procs[i].pid));
                // A windowed member's icon is the app's; else any non-empty path.
                let icon_path = m
                    .iter()
                    .find(|&&i| self.apps.contains(&procs[i].pid) && !procs[i].path.is_empty())
                    .or_else(|| m.iter().find(|&&i| !procs[i].path.is_empty()))
                    .map(|&i| procs[i].path.clone())
                    .unwrap_or_default();
                make_group(name, is_app, m, procs, icon_path)
            })
            .collect();

        // App groups first, each section ordered by memory.
        let (mut apps, mut bg): (Vec<Group>, Vec<Group>) = groups.drain(..).partition(|g| g.is_app);
        apps.sort_by(|a, b| b.mem.cmp(&a.mem));
        bg.sort_by(|a, b| b.mem.cmp(&a.mem));
        apps.extend(bg);
        self.groups = apps;

        self.max_cpu = self.groups.iter().map(|g| g.cpu).fold(1.0_f32, f32::max);
        self.max_mem = self.groups.iter().map(|g| g.mem).max().unwrap_or(1).max(1);
    }

    fn switch_tab(&mut self, t: usize) {
        if self.tab == t {
            return;
        }
        self.tab = t;
        self.scroll = 0.0;
        self.hover = None;
        self.status.clear();
        self.query.clear();
        self.refresh();
        self.paint();
    }

    // ---- display lists -----------------------------------------------------

    fn app_count(&self) -> usize {
        self.groups.iter().filter(|g| g.is_app).count()
    }

    fn matches(&self, p: &Proc) -> bool {
        let q = self.query.trim().to_lowercase();
        q.is_empty()
            || dname(p).to_lowercase().contains(&q)
            || p.name.to_lowercase().contains(&q)
            || p.user.to_lowercase().contains(&q)
            || p.pid.to_string().contains(&q)
    }

    fn display_rows(&self) -> Vec<Row> {
        // Search or 목록 mode → one flat, filtered list.
        if !self.query.trim().is_empty() {
            let hits: Vec<usize> = self
                .procs
                .iter()
                .enumerate()
                .filter(|(_, p)| self.matches(p))
                .map(|(i, _)| i)
                .collect();
            let mut out = vec![Row::Head("검색 결과", hits.len())];
            out.extend(hits.into_iter().map(|i| Row::Leaf(i, false)));
            return out;
        }
        if !self.grouped {
            let mut out = vec![Row::Head("모든 프로세스", self.procs.len())];
            out.extend((0..self.procs.len()).map(|i| Row::Leaf(i, false)));
            return out;
        }

        let mut out = vec![Row::Head("앱", self.app_count())];
        self.push_section(&mut out, true);
        let bg: usize = self.groups.iter().filter(|g| !g.is_app).map(|g| g.members.len()).sum();
        out.push(Row::Head("백그라운드 프로세스", bg));
        self.push_section(&mut out, false);
        out
    }

    fn push_section(&self, out: &mut Vec<Row>, is_app: bool) {
        for (gi, g) in self.groups.iter().enumerate() {
            if g.is_app != is_app {
                continue;
            }
            if g.members.len() == 1 {
                out.push(Row::Leaf(g.members[0], false));
            } else {
                out.push(Row::Group(gi));
                if self.expanded.contains(&g.name) {
                    out.extend(g.members.iter().map(|&m| Row::Leaf(m, true)));
                }
            }
        }
    }

    fn display_svcs(&self) -> Vec<usize> {
        let q = self.query.trim().to_lowercase();
        self.svcs
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                q.is_empty()
                    || s.display.to_lowercase().contains(&q)
                    || s.name.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn content_h(&self) -> f32 {
        let n = if self.tab == 0 { self.display_rows().len() } else { self.display_svcs().len() };
        n as f32 * ROW_H
    }

    // ---- selection & actions ----------------------------------------------

    fn sel_pids(&self) -> Vec<u32> {
        match &self.sel {
            Sel::Pid(p) => vec![*p],
            Sel::Group(name) => self
                .groups
                .iter()
                .find(|g| &g.name == name)
                .map(|g| g.members.iter().map(|&i| self.procs[i].pid).collect())
                .unwrap_or_default(),
            Sel::None => Vec::new(),
        }
    }

    fn sel_label(&self) -> String {
        match &self.sel {
            Sel::Pid(p) => self
                .procs
                .iter()
                .find(|q| q.pid == *p)
                .map(dname)
                .unwrap_or_default(),
            Sel::Group(name) => name.clone(),
            Sel::None => String::new(),
        }
    }

    fn do_kill(&mut self, tree: bool) {
        let pids = self.sel_pids();
        if pids.is_empty() {
            return;
        }
        let label = self.sel_label();
        let killed = if tree {
            let mut all: Vec<u32> = Vec::new();
            for p in &pids {
                all.extend(procs::descendants(*p, &self.procs));
            }
            all.sort_unstable();
            all.dedup();
            all.iter().filter(|&&p| procs::terminate(p)).count()
        } else {
            pids.iter().filter(|&&p| procs::terminate(p)).count()
        };
        self.status = if killed > 0 {
            format!("{label} — {killed}개 프로세스 끝냄")
        } else {
            format!("끝내기 실패 — {label} · 권한 부족일 수 있음")
        };
        self.sel = Sel::None;
        self.refresh();
        self.paint();
    }

    fn do_suspend(&mut self, suspend: bool) {
        let pids = self.sel_pids();
        if pids.is_empty() {
            return;
        }
        let label = self.sel_label();
        let n = pids.iter().filter(|&&p| procs::set_suspended(p, suspend)).count();
        let verb = if suspend { "일시중단" } else { "재개" };
        self.status = if n > 0 {
            format!("{verb} — {label} ({n}개)")
        } else {
            format!("{verb} 실패 — {label} · 권한 부족일 수 있음")
        };
        self.refresh();
        self.paint();
    }

    fn do_priority(&mut self) {
        let pids = self.sel_pids();
        let Some(&first) = pids.first() else { return };
        let cur = self.procs.iter().find(|p| p.pid == first).map(|p| p.prio).unwrap_or("");
        let idx = PRIORITIES.iter().position(|(l, _)| *l == cur).unwrap_or(2);
        let (label, class) = PRIORITIES[(idx + 1) % PRIORITIES.len()];
        let name = self.sel_label();
        let n = pids.iter().filter(|&&p| procs::set_priority(p, class)).count();
        self.status = if n > 0 {
            format!("우선순위 — {name} → {label} ({n}개)")
        } else {
            format!("우선순위 변경 실패 — {name} · 권한 부족일 수 있음")
        };
        self.refresh();
        self.paint();
    }

    fn do_svc(&mut self) {
        let Some(name) = self.sel_svc.clone() else { return };
        let running = self.svcs.iter().find(|s| s.name == name).map(|s| s.running).unwrap_or(false);
        let disp = self
            .svcs
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.display.clone())
            .unwrap_or_else(|| name.clone());
        let verb = if running { "중지" } else { "시작" };
        self.status = if services::set_running(&name, !running) {
            format!("{disp} — {verb} 요청됨")
        } else {
            format!("{disp} — {verb} 실패 · 관리자 권한이 필요합니다")
        };
        self.refresh();
        self.paint();
    }

    fn select_proc(&mut self, pid: u32) {
        if self.sel == Sel::Pid(pid) {
            self.sel = Sel::None;
            self.status.clear();
        } else {
            self.sel = Sel::Pid(pid);
            self.status = self
                .procs
                .iter()
                .find(|p| p.pid == pid)
                .map(|p| {
                    let path = if p.path.is_empty() { "경로 없음" } else { p.path.as_str() };
                    let prio = if p.prio.is_empty() { "—" } else { p.prio };
                    format!("{path}   ·   우선순위 {prio}")
                })
                .unwrap_or_default();
        }
    }

    fn select_group(&mut self, gi: usize) {
        let Some(g) = self.groups.get(gi) else { return };
        let name = g.name.clone();
        if self.sel == Sel::Group(name.clone()) {
            self.sel = Sel::None;
            self.status.clear();
        } else {
            self.status = format!("{}개 프로세스   ·   메모리 {}", g.members.len(), fmt_mem(g.mem));
            self.sel = Sel::Group(name);
        }
    }

    fn hit(&self, x: f32, y: f32) -> Option<Act> {
        self.hits
            .iter()
            .find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
            .map(|(_, a)| *a)
    }

    // ---- painting ----------------------------------------------------------

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

    fn dot(&self, cx: f32, cy: f32, radius: f32, c: D2D1_COLOR_F) {
        unsafe {
            if let Ok(b) = self.renderer.brush(c) {
                self.renderer.dc.FillEllipse(
                    &D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: radius, radiusY: radius },
                    &b,
                );
            }
        }
    }

    /// An 18px app icon at (x, y), lazily extracted and cached by exe path.
    fn draw_icon(&mut self, path: &str, x: f32, y: f32) {
        if path.is_empty() {
            return;
        }
        let icon = {
            let r = &self.renderer;
            self.icons
                .entry(path.to_string())
                .or_insert_with(|| icons::exe_icon(&r.dc, path))
                .clone()
        };
        if let Some(bmp) = icon {
            let t = y + (ROW_H - 18.0) / 2.0;
            let ic = rect(x, t, x + 18.0, t + 18.0);
            unsafe {
                self.renderer
                    .dc
                    .DrawBitmap(&bmp, Some(&ic), 1.0, D2D1_INTERPOLATION_MODE_LINEAR, None, None);
            }
        }
    }

    /// A blue gauge bar behind a CPU / memory cell: its width is the value's
    /// share of the column maximum, right-anchored under the number. Small
    /// values read as a short stub, heavy ones fill the cell.
    fn heat(&self, l: f32, y: f32, r: f32, frac: f32) {
        let f = frac.clamp(0.0, 1.0);
        if f < 0.01 {
            return;
        }
        let w = (r - l) * f;
        self.fill_round(rect(r - w, y + 4.0, r, y + ROW_H - 4.0), 3.0, theme::rgba(66, 132, 244, 0.36));
    }

    /// A collapse/expand chevron centred at (cx, cy).
    fn chevron(&self, cx: f32, cy: f32, open: bool) {
        let c = theme::TEXT_DIM;
        if open {
            self.text("ᐯ", &self.fmt_row.clone(), rect(cx - 8.0, cy - 12.0, cx + 8.0, cy + 12.0), c);
        } else {
            self.text("ᐳ", &self.fmt_row.clone(), rect(cx - 8.0, cy - 12.0, cx + 8.0, cy + 12.0), c);
        }
    }

    /// A footer pill button; registers its rect for hit-testing.
    fn button(&mut self, x1: f32, cy: f32, wide: f32, label: &str, act: Act, danger: bool, on: bool) {
        let btn = rect(x1 - wide, cy - 17.0, x1, cy + 17.0);
        let hot = self.hover == Some(act);
        let base = if danger { theme::rgba(220, 72, 72, 1.0) } else { theme::accent() };
        let bg = if on {
            base
        } else if hot {
            D2D1_COLOR_F { a: 0.85, ..base }
        } else {
            theme::rgba(255, 255, 255, 0.10)
        };
        self.fill_round(btn, 8.0, bg);
        let fg = if on || hot { theme::rgba(255, 255, 255, 1.0) } else { theme::TEXT };
        self.text(label, &self.fmt_tab.clone(), btn, fg);
        self.hits.push((btn, act));
    }

    fn cols(&self) -> Cols {
        Cols {
            pid: self.w - 470.0,
            thr: self.w - 400.0,
            cpu: self.w - 320.0,
            mem: self.w - 230.0,
            user: self.w - 150.0,
        }
    }

    fn paint(&mut self) {
        self.hits.clear();
        unsafe {
            self.renderer.dc.BeginDraw();
            self.renderer.dc.Clear(Some(&theme::rgba(24, 25, 30, 0.72)));
        }
        self.text("작업 관리자", &self.fmt_title.clone(), rect(20.0, 12.0, 220.0, 46.0), theme::TEXT);

        let tabs = ["프로세스", "서비스"];
        for (i, t) in tabs.iter().enumerate() {
            let x0 = 210.0 + i as f32 * 96.0;
            let pill = rect(x0, 14.0, x0 + 88.0, 44.0);
            let on = self.tab == i;
            let hot = self.hover == Some(Act::Tab(i));
            self.fill_round(
                pill,
                8.0,
                if on { theme::accent() } else { theme::rgba(255, 255, 255, if hot { 0.12 } else { 0.06 }) },
            );
            self.text(t, &self.fmt_tab.clone(), pill, if on { theme::rgba(23, 24, 28, 1.0) } else { theme::TEXT });
            self.hits.push((pill, Act::Tab(i)));
        }
        if self.tab == 0 {
            let tg = rect(406.0, 14.0, 486.0, 44.0);
            let hot = self.hover == Some(Act::ModeToggle);
            self.fill_round(tg, 8.0, theme::rgba(255, 255, 255, if hot { 0.12 } else { 0.06 }));
            self.text(if self.grouped { "그룹" } else { "목록" }, &self.fmt_tab.clone(), tg, theme::TEXT);
            self.hits.push((tg, Act::ModeToggle));
        }
        let pill = rect(self.w - 282.0, 14.0, self.w - 16.0, 44.0);
        let hot = self.hover == Some(Act::Search);
        self.fill_round(pill, 15.0, theme::rgba(255, 255, 255, if hot { 0.12 } else { 0.08 }));
        let (txt, col) = if self.query.is_empty() {
            ("이름 · PID · 사용자 검색".to_string(), theme::TEXT_DIM)
        } else {
            (format!("{}|", self.query), theme::TEXT)
        };
        self.text(&txt, &self.fmt_row.clone(), rect(self.w - 268.0, 14.0, self.w - 22.0, 44.0), col);
        self.hits.push((pill, Act::Search));

        // Column header.
        let hy = 56.0;
        if self.tab == 0 {
            let c = self.cols();
            self.text("이름", &self.fmt_sub.clone(), rect(18.0, hy, c.pid, hy + 22.0), theme::TEXT_DIM);
            self.col_head("PID", c.pid, c.thr, hy);
            self.col_head("스레드", c.thr, c.cpu, hy);
            self.col_head("CPU", c.cpu, c.mem, hy);
            self.col_head("메모리", c.mem, c.user - 8.0, hy);
            self.text("사용자", &self.fmt_sub.clone(), rect(c.user, hy, self.w - 16.0, hy + 22.0), theme::TEXT_DIM);
        } else {
            self.text("서비스", &self.fmt_sub.clone(), rect(18.0, hy, self.w - 140.0, hy + 22.0), theme::TEXT_DIM);
            self.text("상태", &self.fmt_num.clone(), rect(self.w - 140.0, hy, self.w - 18.0, hy + 22.0), theme::TEXT_DIM);
        }
        self.fill_round(rect(16.0, hy + 24.0, self.w - 16.0, hy + 25.0), 0.0, theme::rgba(255, 255, 255, 0.08));

        let top = 88.0;
        let bottom = self.h - 52.0;
        let clip = rect(8.0, top, self.w - 8.0, bottom);
        unsafe {
            self.renderer.dc.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED);
        }
        if self.tab == 0 {
            self.paint_procs(top, bottom);
        } else {
            self.paint_svcs(top, bottom);
        }
        unsafe {
            self.renderer.dc.PopAxisAlignedClip();
        }

        // Footer: status + action buttons.
        let fy0 = self.h - 46.0;
        self.fill_round(rect(0.0, fy0 - 6.0, self.w, fy0 - 5.0), 0.0, theme::rgba(255, 255, 255, 0.06));
        let cy = fy0 + 15.0;
        if self.tab == 0 && self.sel != Sel::None {
            let mut x = self.w - 16.0;
            self.button(x, cy, 84.0, "우선순위", Act::Priority, false, false);
            x -= 90.0;
            self.button(x, cy, 66.0, "재개", Act::Resume, false, false);
            x -= 72.0;
            self.button(x, cy, 66.0, "중단", Act::Suspend, false, false);
            x -= 72.0;
            self.button(x, cy, 84.0, "트리 종료", Act::KillTree, true, false);
            x -= 90.0;
            self.button(x, cy, 84.0, "끝내기", Act::Kill, true, false);
        } else if self.tab == 1 {
            if let Some(name) = self.sel_svc.clone() {
                let running = self.svcs.iter().find(|s| s.name == name).map(|s| s.running).unwrap_or(false);
                self.button(self.w - 16.0, cy, 96.0, if running { "중지" } else { "시작" }, Act::SvcGo, running, false);
            }
        }
        if !self.status.is_empty() {
            let right = if self.tab == 0 { self.w - 424.0 } else { self.w - 130.0 };
            self.text(&self.status, &self.fmt_sub.clone(), rect(18.0, fy0, right, fy0 + 34.0), theme::TEXT_DIM);
        }

        unsafe {
            let _ = self.renderer.dc.EndDraw(None, None);
            let _ = self.renderer.present();
        }
    }

    fn col_head(&self, s: &str, l: f32, r: f32, y: f32) {
        self.text(s, &self.fmt_num.clone(), rect(l, y, r, y + 22.0), theme::TEXT_DIM);
    }

    fn paint_procs(&mut self, top: f32, bottom: f32) {
        if !self.loaded {
            self.placeholder("프로세스를 읽는 중…", top, bottom);
            return;
        }
        let rows = self.display_rows();
        let c = self.cols();
        for (i, row) in rows.iter().enumerate() {
            let y = top - self.scroll + i as f32 * ROW_H;
            if y + ROW_H < top || y > bottom {
                continue;
            }
            match row {
                Row::Head(label, n) => {
                    let band = rect(10.0, y + 5.0, self.w - 10.0, y + ROW_H);
                    self.fill_round(band, 7.0, D2D1_COLOR_F { a: 0.16, ..theme::accent() });
                    self.text(
                        &format!("{label}   {n}"),
                        &self.fmt_head.clone(),
                        rect(20.0, y + 3.0, self.w - 16.0, y + ROW_H),
                        theme::accent(),
                    );
                }
                Row::Group(gi) => self.paint_group(*gi, y, &c),
                Row::Leaf(idx, member) => self.paint_leaf(*idx, *member, y, &c),
            }
        }
    }

    fn paint_group(&mut self, gi: usize, y: f32, c: &Cols) {
        let g = &self.groups[gi];
        let (name, count, cpu, mem, thr, path) =
            (g.name.clone(), g.members.len(), g.cpu, g.mem, g.thr, g.icon_path.clone());
        let sel = self.sel == Sel::Group(name.clone());
        let hot = matches!(self.hover, Some(Act::GroupSel(h) | Act::GroupToggle(h)) if h == gi);
        let open = self.expanded.contains(&name);
        let card = rect(10.0, y, self.w - 10.0, y + ROW_H);
        if sel {
            self.fill_round(card, 7.0, theme::rgba(255, 255, 255, 0.14));
        } else if hot {
            self.fill_round(card, 7.0, theme::HOVER_FILL);
        }
        self.chevron(22.0, y + ROW_H / 2.0, open);
        self.draw_icon(&path, 36.0, y);
        let label = format!("{name}   ({count})");
        self.text(&label, &self.fmt_head.clone(), rect(60.0, y, c.pid - 6.0, y + ROW_H), theme::TEXT);
        self.text(&thr.to_string(), &self.fmt_num.clone(), rect(c.thr, y, c.cpu, y + ROW_H), theme::TEXT_DIM);
        self.heat(c.cpu, y, c.mem - 4.0, cpu / self.max_cpu);
        self.text(&fmt_cpu(cpu), &self.fmt_num.clone(), rect(c.cpu, y, c.mem, y + ROW_H), theme::TEXT);
        self.heat(c.mem, y, c.user - 12.0, mem as f32 / self.max_mem as f32);
        self.text(&fmt_mem(mem), &self.fmt_num.clone(), rect(c.mem, y, c.user - 8.0, y + ROW_H), theme::TEXT);
        self.hits.push((rect(10.0, y, 36.0, y + ROW_H), Act::GroupToggle(gi)));
        self.hits.push((rect(36.0, y, self.w - 10.0, y + ROW_H), Act::GroupSel(gi)));
    }

    fn paint_leaf(&mut self, idx: usize, member: bool, y: f32, c: &Cols) {
        let p = &self.procs[idx];
        let (pid, threads, cpu, mem, name, user, path) = (
            p.pid,
            p.threads,
            p.cpu,
            p.mem,
            dname(p),
            p.user.clone(),
            p.path.clone(),
        );
        let card = rect(10.0, y, self.w - 10.0, y + ROW_H);
        let sel = self.sel == Sel::Pid(pid);
        let hot = self.hover == Some(Act::Proc(pid));
        let own = !user.is_empty() && user.to_lowercase() == self.me;
        if sel {
            self.fill_round(card, 7.0, theme::rgba(255, 255, 255, 0.14));
        } else if hot {
            self.fill_round(card, 7.0, theme::HOVER_FILL);
        } else if own {
            self.fill_round(card, 7.0, D2D1_COLOR_F { a: 0.05, ..theme::accent() });
        }
        let icon_x = if member { 48.0 } else { 20.0 };
        self.draw_icon(&path, icon_x, y);
        let name_x = icon_x + 24.0;
        self.text(&trunc(&name, 42), &self.fmt_row.clone(), rect(name_x, y, c.pid - 6.0, y + ROW_H), theme::TEXT);
        self.text(&pid.to_string(), &self.fmt_num.clone(), rect(c.pid, y, c.thr, y + ROW_H), theme::TEXT_DIM);
        self.text(&threads.to_string(), &self.fmt_num.clone(), rect(c.thr, y, c.cpu, y + ROW_H), theme::TEXT_DIM);
        self.heat(c.cpu, y, c.mem - 4.0, cpu / self.max_cpu);
        self.text(&fmt_cpu(cpu), &self.fmt_num.clone(), rect(c.cpu, y, c.mem, y + ROW_H), theme::TEXT);
        self.heat(c.mem, y, c.user - 12.0, mem as f32 / self.max_mem as f32);
        self.text(&fmt_mem(mem), &self.fmt_num.clone(), rect(c.mem, y, c.user - 8.0, y + ROW_H), theme::TEXT);
        self.text(&trunc(&user, 22), &self.fmt_row.clone(), rect(c.user, y, self.w - 16.0, y + ROW_H), theme::TEXT_DIM);
        self.hits.push((card, Act::Proc(pid)));
    }

    /// Centred one-liner for the window's empty first frame, while the sampler
    /// thread is still working.
    fn placeholder(&mut self, msg: &str, top: f32, bottom: f32) {
        let fmt = self.fmt_tab.clone();
        self.text(msg, &fmt, rect(0.0, top, self.w, bottom), theme::TEXT_DIM);
    }

    fn paint_svcs(&mut self, top: f32, bottom: f32) {
        if self.svcs.is_empty() && self.pending {
            self.placeholder("서비스를 읽는 중…", top, bottom);
            return;
        }
        let rows = self.display_svcs();
        for (i, idx) in rows.iter().enumerate() {
            let y = top - self.scroll + i as f32 * ROW_H;
            if y + ROW_H < top || y > bottom {
                continue;
            }
            let s = &self.svcs[*idx];
            let card = rect(10.0, y, self.w - 10.0, y + ROW_H);
            let sel = self.sel_svc.as_deref() == Some(s.name.as_str());
            let hot = self.hover == Some(Act::Svc(*idx));
            if sel {
                self.fill_round(card, 7.0, theme::rgba(255, 255, 255, 0.14));
            } else if hot {
                self.fill_round(card, 7.0, theme::HOVER_FILL);
            }
            let label = if s.display.is_empty() { s.name.clone() } else { s.display.clone() };
            self.text(&trunc(&label, 64), &self.fmt_row.clone(), rect(18.0, y, self.w - 150.0, y + ROW_H), theme::TEXT);
            let (dc, st) = if s.running {
                (theme::rgba(80, 200, 120, 1.0), "실행 중")
            } else {
                (theme::rgba(150, 153, 160, 1.0), "중지됨")
            };
            self.dot(self.w - 132.0, y + ROW_H / 2.0, 4.0, dc);
            self.text(st, &self.fmt_num.clone(), rect(self.w - 124.0, y, self.w - 18.0, y + ROW_H), theme::TEXT_DIM);
            self.hits.push((card, Act::Svc(*idx)));
        }
    }
}

fn make_group(
    name: String,
    is_app: bool,
    mut members: Vec<usize>,
    procs: &[Proc],
    icon_path: String,
) -> Group {
    members.sort_by(|&a, &b| procs[b].mem.cmp(&procs[a].mem));
    let cpu = members.iter().map(|&i| procs[i].cpu).sum();
    let mem = members.iter().map(|&i| procs[i].mem).sum();
    let thr = members.iter().map(|&i| procs[i].threads).sum();
    Group { name, is_app, members, cpu, mem, thr, icon_path }
}

/// Friendly display name: the version-resource description, else the exe name
/// with any ".exe" trimmed.
fn dname(p: &Proc) -> String {
    if !p.descr.is_empty() {
        return p.descr.clone();
    }
    p.name
        .strip_suffix(".exe")
        .or_else(|| p.name.strip_suffix(".EXE"))
        .unwrap_or(&p.name)
        .to_string()
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

fn fmt_mem(bytes: u64) -> String {
    let mb = bytes as f64 / 1_048_576.0;
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

fn fmt_cpu(pct: f32) -> String {
    if pct < 0.05 { "—".to_string() } else { format!("{pct:.1}%") }
}

unsafe extern "system" fn taskmgr_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let app = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TaskManagerApp;
        if app.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let app = &mut *app;
        match msg {
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                app.paint();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SIZE => {
                if wparam.0 as u32 != SIZE_MINIMIZED {
                    app.resized();
                }
                LRESULT(0)
            }
            WM_TIMER => {
                // Only ask when the last answer is in — a cold sample outlives
                // the refresh interval, and queued requests would never catch up.
                if IsWindowVisible(hwnd).as_bool() && !app.pending {
                    app.refresh();
                }
                LRESULT(0)
            }
            WM_SNAPSHOT => {
                if app.drain() {
                    app.paint();
                }
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let sug = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    sug.left,
                    sug.top,
                    sug.right - sug.left,
                    sug.bottom - sug.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                app.resized();
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
                mmi.ptMinTrackSize.x = (680.0 * app.scale) as i32;
                mmi.ptMinTrackSize.y = (440.0 * app.scale) as i32;
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32 / app.scale;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 / app.scale;
                let h = app.hit(x, y);
                if h != app.hover {
                    app.hover = h;
                    app.paint();
                }
                if !app.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    if TrackMouseEvent(&mut tme).is_ok() {
                        app.tracking = true;
                    }
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                app.tracking = false;
                if app.hover.take().is_some() {
                    app.paint();
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32 / app.scale;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 / app.scale;
                match app.hit(x, y) {
                    Some(Act::Tab(t)) => app.switch_tab(t),
                    Some(Act::ModeToggle) => {
                        app.grouped = !app.grouped;
                        app.scroll = 0.0;
                        app.paint();
                    }
                    Some(Act::Search) => {}
                    Some(Act::Proc(pid)) => {
                        app.select_proc(pid);
                        app.paint();
                    }
                    Some(Act::GroupSel(gi)) => {
                        app.select_group(gi);
                        app.paint();
                    }
                    Some(Act::GroupToggle(gi)) => {
                        if let Some(g) = app.groups.get(gi) {
                            let name = g.name.clone();
                            if !app.expanded.remove(&name) {
                                app.expanded.insert(name);
                            }
                        }
                        app.paint();
                    }
                    Some(Act::Svc(i)) => {
                        let name = app.svcs.get(i).map(|s| s.name.clone());
                        app.sel_svc = if app.sel_svc == name { None } else { name };
                        app.paint();
                    }
                    Some(Act::Kill) => app.do_kill(false),
                    Some(Act::KillTree) => app.do_kill(true),
                    Some(Act::Suspend) => app.do_suspend(true),
                    Some(Act::Resume) => app.do_suspend(false),
                    Some(Act::Priority) => app.do_priority(),
                    Some(Act::SvcGo) => app.do_svc(),
                    None => {}
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32 / 120.0 * ROW_H * 2.0;
                let vis = (app.h - 140.0).max(0.0);
                let max = (app.content_h() - vis).max(0.0);
                let ns = (app.scroll - delta).clamp(0.0, max);
                if ns != app.scroll {
                    app.scroll = ns;
                    app.hover = None;
                    app.paint();
                }
                LRESULT(0)
            }
            WM_CHAR => {
                let c = wparam.0 as u32;
                let mut changed = false;
                if c == 0x08 {
                    changed = app.query.pop().is_some();
                } else if c >= 0x20 && c != 0x7F {
                    if let Some(ch) = char::from_u32(c) {
                        app.query.push(ch);
                        changed = true;
                    }
                }
                if changed {
                    app.scroll = 0.0;
                    app.hover = None;
                    app.paint();
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wparam.0 == VK_ESCAPE.0 as usize {
                    if !app.query.is_empty() {
                        app.query.clear();
                        app.scroll = 0.0;
                        app.paint();
                    } else {
                        app.hide();
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                app.hide();
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
