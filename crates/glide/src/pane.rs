// One file pane: current directory, listing, selection, history.
use std::collections::BTreeSet;
use std::os::windows::fs::MetadataExt;
use std::path::PathBuf;
use std::time::SystemTime;

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;

pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub hidden: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub enum SortKey {
    Name,
    Type,
    Date,
    Size,
}

pub struct Pane {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
    /// Keyboard cursor + rename/scroll target. Always a valid row when non-empty.
    pub sel: usize,
    /// Fixed end of a shift-range extension (set on every plain click / arrow).
    pub anchor: usize,
    /// The actual multi-selection. `sel` is the cursor within it.
    pub marked: BTreeSet<usize>,
    pub history: Vec<PathBuf>,
    pub hist_pos: usize,
    pub err: Option<String>,
    pub scroll_to_sel: bool,
    pub sort: SortKey,
    pub asc: bool,
}

impl Pane {
    pub fn new(dir: PathBuf, show_hidden: bool) -> Self {
        let mut p = Self {
            history: vec![dir.clone()],
            hist_pos: 0,
            dir,
            entries: Vec::new(),
            sel: 0,
            anchor: 0,
            marked: BTreeSet::new(),
            err: None,
            scroll_to_sel: false,
            sort: SortKey::Name,
            asc: true,
        };
        p.refresh(show_hidden);
        p.select_only(0);
        p
    }

    pub fn refresh(&mut self, show_hidden: bool) {
        self.err = None;
        // Snapshot selection by path so it survives the rebuild + re-sort.
        let prev_sel = self.entries.get(self.sel).map(|e| e.path.clone());
        let prev_anchor = self.entries.get(self.anchor).map(|e| e.path.clone());
        let prev_marked: Vec<PathBuf> = self
            .marked
            .iter()
            .filter_map(|&i| self.entries.get(i))
            .map(|e| e.path.clone())
            .collect();
        let mut out: Vec<Entry> = Vec::new();
        if self.is_this_pc() {
            // Virtual "내 PC" root: the drives stand in for directory entries.
            for d in crate::sidebar::drives() {
                out.push(Entry {
                    name: format!("로컬 디스크 ({})", d.label),
                    is_dir: true,
                    size: 0,
                    modified: None,
                    hidden: false,
                    path: d.root,
                });
            }
        } else {
            match std::fs::read_dir(&self.dir) {
                Ok(rd) => {
                    for e in rd.flatten() {
                        let path = e.path();
                        let Ok(md) = e.metadata() else { continue };
                        let hidden = md.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0;
                        if hidden && !show_hidden {
                            continue;
                        }
                        out.push(Entry {
                            name: e.file_name().to_string_lossy().to_string(),
                            is_dir: md.is_dir(),
                            size: md.len(),
                            modified: md.modified().ok(),
                            hidden,
                            path,
                        });
                    }
                }
                Err(e) => self.err = Some(e.to_string()),
            }
        }
        // Directories first (Explorer-style), then by the active sort key.
        // When sorting by size, directories fall back to name order.
        let (sort, asc) = (self.sort, self.asc);
        out.sort_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                let ord = match (a.is_dir, sort) {
                    (_, SortKey::Name) | (true, SortKey::Size) | (true, SortKey::Type) => {
                        a.name.to_lowercase().cmp(&b.name.to_lowercase())
                    }
                    (_, SortKey::Date) => a.modified.cmp(&b.modified),
                    (false, SortKey::Size) => a.size.cmp(&b.size),
                    (false, SortKey::Type) => {
                        let ext = |p: &std::path::Path| {
                            p.extension()
                                .map(|e| e.to_string_lossy().to_lowercase())
                                .unwrap_or_default()
                        };
                        ext(&a.path)
                            .cmp(&ext(&b.path))
                            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                    }
                };
                if asc { ord } else { ord.reverse() }
            })
        });
        self.entries = out;
        // Remap the snapshotted paths onto the new ordering; drop any that vanished.
        self.marked = prev_marked
            .iter()
            .filter_map(|p| self.entries.iter().position(|e| &e.path == p))
            .collect();
        self.sel = prev_sel
            .as_ref()
            .and_then(|p| self.entries.iter().position(|e| &e.path == p))
            .unwrap_or_else(|| self.sel.min(self.entries.len().saturating_sub(1)));
        self.anchor = prev_anchor
            .as_ref()
            .and_then(|p| self.entries.iter().position(|e| &e.path == p))
            .unwrap_or(self.sel);
    }

    /// Replace the selection with a single item; cursor + anchor move there.
    pub fn select_only(&mut self, i: usize) {
        self.marked.clear();
        if i < self.entries.len() {
            self.marked.insert(i);
            self.sel = i;
            self.anchor = i;
        }
    }

    /// Ctrl-click: flip one item's membership; cursor + anchor follow.
    pub fn toggle(&mut self, i: usize) {
        if i >= self.entries.len() {
            return;
        }
        if !self.marked.remove(&i) {
            self.marked.insert(i);
        }
        self.sel = i;
        self.anchor = i;
    }

    /// Shift-click / Shift-arrow: select the contiguous run anchor..=i.
    pub fn select_range(&mut self, i: usize) {
        if self.entries.is_empty() {
            return;
        }
        let i = i.min(self.entries.len() - 1);
        let (lo, hi) = (self.anchor.min(i), self.anchor.max(i));
        self.marked = (lo..=hi).collect();
        self.sel = i;
    }

    /// Ctrl+Shift-click: union the run anchor..=i into the existing selection.
    pub fn add_range(&mut self, i: usize) {
        if self.entries.is_empty() {
            return;
        }
        let i = i.min(self.entries.len() - 1);
        let (lo, hi) = (self.anchor.min(i), self.anchor.max(i));
        self.marked.extend(lo..=hi);
        self.sel = i;
    }

    pub fn select_all(&mut self) {
        self.marked = (0..self.entries.len()).collect();
        if !self.entries.is_empty() {
            self.sel = self.entries.len() - 1;
        }
    }

    /// Paths of the current multi-selection, in row order.
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.marked
            .iter()
            .filter_map(|&i| self.entries.get(i))
            .map(|e| e.path.clone())
            .collect()
    }

    pub fn navigate(&mut self, to: PathBuf, show_hidden: bool) {
        // Truncate forward history, then push.
        self.history.truncate(self.hist_pos + 1);
        self.history.push(to.clone());
        self.hist_pos = self.history.len() - 1;
        self.dir = to;
        self.scroll_to_sel = true;
        self.refresh(show_hidden);
        self.select_only(0);
    }

    pub fn back(&mut self, show_hidden: bool) {
        if self.hist_pos > 0 {
            self.hist_pos -= 1;
            self.dir = self.history[self.hist_pos].clone();
            self.scroll_to_sel = true;
            self.refresh(show_hidden);
            self.select_only(0);
        }
    }

    pub fn forward(&mut self, show_hidden: bool) {
        if self.hist_pos + 1 < self.history.len() {
            self.hist_pos += 1;
            self.dir = self.history[self.hist_pos].clone();
            self.scroll_to_sel = true;
            self.refresh(show_hidden);
            self.select_only(0);
        }
    }

    pub fn up(&mut self, show_hidden: bool) {
        if self.is_this_pc() {
            return; // already at the top
        }
        let from_path = self.dir.clone();
        // A drive root ("C:\") has no parent → step up to the 내 PC root.
        let target = match self.dir.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::new(),
        };
        self.navigate(target, show_hidden);
        // Reselect whatever we just came out of, explorer-style.
        if let Some(i) = self.entries.iter().position(|e| e.path == from_path) {
            self.select_only(i);
            self.scroll_to_sel = true;
        }
    }

    pub fn is_this_pc(&self) -> bool {
        self.dir.as_os_str().is_empty()
    }
}

/// The virtual "내 PC" location (drive list), represented by an empty path.
pub fn this_pc() -> PathBuf {
    PathBuf::new()
}

pub fn humanize(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut s = size as f64;
    let mut u = 0;
    while s >= 1024.0 && u < UNITS.len() - 1 {
        s /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{size} B")
    } else {
        format!("{s:.1} {}", UNITS[u])
    }
}
