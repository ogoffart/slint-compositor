//! Controller for the standalone Files browser.
//!
//! The window ([`Files`](crate::Files), compiled from `ui/files.slint`) is a
//! pure view; all behaviour — directory listing, navigation history, selection,
//! preview loading and launching — lives here. It is deliberately self-contained
//! (no dependency on the compositor crate) so the browser is an ordinary,
//! independent binary.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use slint::{Model, VecModel};

use crate::ops;
use crate::{Crumb, FileItem, Files};

/// One directory entry, cached unsorted so re-sorting and toggling hidden files
/// don't re-read the filesystem.
#[derive(Clone)]
struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    kind: &'static str,
    len: u64,
    mtime: Option<SystemTime>,
    size_str: String,
    modified_str: String,
    thumb: slint::Image,
}

pub struct Browser {
    weak: slint::Weak<Files>,
    items: Rc<VecModel<FileItem>>,
    cwd: PathBuf,
    /// Unsorted, unfiltered cache of the current directory.
    raw: Vec<Entry>,
    /// Visible paths in display order (parallel to the model rows).
    entries: Vec<PathBuf>,
    /// Directories visited, for the Back button.
    history: Vec<PathBuf>,
    /// Per-row selection flags (parallel to `entries`).
    marks: Vec<bool>,
    /// Active row (drives the preview and keyboard movement); -1 when empty.
    cursor: i32,
    /// Anchor row for range (Shift) selection.
    anchor: i32,
    /// Pending cut/copy: the paths and whether this is a move.
    clipboard: Vec<PathBuf>,
    clip_cut: bool,
    /// Sort column (0 name, 1 size, 2 modified, 3 type) and direction.
    sort_key: i32,
    sort_desc: bool,
    /// Whether dotfiles are shown.
    show_hidden: bool,
    /// Lower-cased name filter from the search box (empty = no filter).
    filter: String,
}

/// A shared, reference-counted browser.
pub type Shared = Rc<RefCell<Browser>>;

impl Browser {
    pub fn new(weak: slint::Weak<Files>, items: Rc<VecModel<FileItem>>) -> Self {
        Self {
            weak,
            items,
            cwd: PathBuf::from("/"),
            raw: Vec::new(),
            entries: Vec::new(),
            history: Vec::new(),
            marks: Vec::new(),
            cursor: -1,
            anchor: -1,
            clipboard: Vec::new(),
            clip_cut: false,
            sort_key: 0,
            sort_desc: false,
            show_hidden: false,
            filter: String::new(),
        }
    }

    /// Navigate to an arbitrary path (used by the Places sidebar / breadcrumb).
    pub fn go_to(&mut self, path: &str) {
        self.navigate(PathBuf::from(path));
    }

    /// Apply a name filter from the search box (case-insensitive substring).
    pub fn set_filter(&mut self, text: &str) {
        self.filter = text.trim().to_lowercase();
        self.render(true);
    }

    /// Set the sort column; clicking the active column again flips direction.
    pub fn set_sort(&mut self, key: i32) {
        if key == self.sort_key {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort_key = key;
            self.sort_desc = false;
        }
        self.render(true);
    }

    /// Show or hide dotfiles (re-filters the cache; no disk re-read).
    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.render(true);
    }

    /// Enter `dir`, pushing the current directory onto the back-history.
    pub fn navigate(&mut self, dir: PathBuf) {
        let prev = self.cwd.clone();
        self.list(dir);
        if self.cwd != prev {
            self.history.push(prev);
            self.update_back();
        }
    }

    pub fn go_up(&mut self) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.navigate(parent);
        }
    }

    pub fn go_home(&mut self) {
        self.navigate(home_dir());
    }

    pub fn go_back(&mut self) {
        if let Some(prev) = self.history.pop() {
            self.list(prev);
            self.update_back();
        }
    }

    /// Navigate to a path typed into the editable bar: a directory is entered, a
    /// file enters its parent and is selected.
    pub fn navigate_to(&mut self, path: &str) {
        let path = PathBuf::from(expand_tilde(path.trim()));
        if path.is_dir() {
            self.navigate(path);
        } else if path.is_file() {
            if let Some(parent) = path.parent().map(Path::to_path_buf) {
                self.navigate(parent);
                if let Some(idx) = self.entries.iter().position(|p| p == &path) {
                    self.set_single(idx as i32);
                }
            }
        }
    }

    // --- Selection ---------------------------------------------------------

    /// Pointer press on row `idx`, honouring Ctrl (toggle) and Shift (range).
    pub fn row_pressed(&mut self, idx: i32, ctrl: bool, shift: bool) {
        if idx < 0 || idx as usize >= self.entries.len() {
            return;
        }
        if shift && self.anchor >= 0 {
            self.range_to(idx);
        } else if ctrl {
            self.toggle(idx);
        } else {
            self.set_single(idx);
        }
    }

    fn set_single(&mut self, idx: i32) {
        for m in &mut self.marks {
            *m = false;
        }
        if let Some(m) = self.marks.get_mut(idx as usize) {
            *m = true;
        }
        self.cursor = idx;
        self.anchor = idx;
        self.sync();
    }

    fn toggle(&mut self, idx: i32) {
        if let Some(m) = self.marks.get_mut(idx as usize) {
            *m = !*m;
        }
        self.cursor = idx;
        self.anchor = idx;
        self.sync();
    }

    fn range_to(&mut self, idx: i32) {
        let (lo, hi) = (self.anchor.min(idx), self.anchor.max(idx));
        for (i, m) in self.marks.iter_mut().enumerate() {
            *m = (i as i32) >= lo && (i as i32) <= hi;
        }
        self.cursor = idx;
        self.sync();
    }

    pub fn select_all(&mut self) {
        for m in &mut self.marks {
            *m = true;
        }
        if self.cursor < 0 && !self.entries.is_empty() {
            self.cursor = 0;
            self.anchor = 0;
        }
        self.sync();
    }

    /// Move the cursor by `delta` rows (clamped). With `shift`, extend the
    /// selection from the anchor; otherwise select only the new row. Large
    /// magnitudes act as Home/End.
    pub fn move_cursor(&mut self, delta: i32, shift: bool) {
        let count = self.entries.len() as i32;
        if count == 0 {
            return;
        }
        let base = if self.cursor < 0 {
            if delta < 0 {
                count - 1
            } else {
                0
            }
        } else {
            self.cursor
        };
        let next = (base + delta).clamp(0, count - 1);
        if shift {
            if self.anchor < 0 {
                self.anchor = base;
            }
            self.range_to(next);
        } else {
            self.set_single(next);
        }
    }

    /// Open the entry at `idx`: enter a directory, or launch a file with the
    /// system default handler.
    pub fn activate(&mut self, idx: i32) {
        let Some(path) = self.entries.get(idx.max(0) as usize).cloned() else {
            return;
        };
        if path.is_dir() {
            self.navigate(path);
        } else {
            launch(&path);
        }
    }

    pub fn activate_selected(&mut self) {
        if self.cursor >= 0 {
            self.activate(self.cursor);
        }
    }

    // --- File operations ---------------------------------------------------

    /// The paths the next operation acts on: every marked row, or the cursor
    /// when nothing is marked.
    fn targets(&self) -> Vec<PathBuf> {
        let marked: Vec<PathBuf> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(i, _)| self.marks.get(*i).copied().unwrap_or(false))
            .map(|(_, p)| p.clone())
            .collect();
        if !marked.is_empty() {
            marked
        } else if self.cursor >= 0 {
            self.entries
                .get(self.cursor as usize)
                .cloned()
                .into_iter()
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn copy(&mut self) {
        self.clipboard = self.targets();
        self.clip_cut = false;
        self.update_can_paste();
    }

    pub fn cut(&mut self) {
        self.clipboard = self.targets();
        self.clip_cut = true;
        self.update_can_paste();
    }

    pub fn paste(&mut self) {
        if self.clipboard.is_empty() {
            return;
        }
        let cwd = self.cwd.clone();
        let mut last = None;
        for src in self.clipboard.clone() {
            let result = if self.clip_cut {
                ops::move_into(&src, &cwd)
            } else {
                ops::copy_into(&src, &cwd)
            };
            match result {
                Ok(path) => last = Some(path),
                Err(err) => log::warn!("paste {} failed: {err}", src.display()),
            }
        }
        if self.clip_cut {
            self.clipboard.clear();
            self.clip_cut = false;
        }
        self.list(cwd);
        if let Some(name) = last.as_ref().and_then(|p| p.file_name()) {
            self.select_by_name(&name.to_string_lossy());
        }
        self.update_can_paste();
    }

    pub fn trash(&mut self) {
        for path in self.targets() {
            if let Err(err) = ops::trash(&path) {
                log::warn!("trash {} failed: {err}", path.display());
            }
        }
        let cwd = self.cwd.clone();
        self.list(cwd);
    }

    pub fn rename(&mut self, new_name: &str) {
        let Some(path) = self.entries.get(self.cursor.max(0) as usize).cloned() else {
            return;
        };
        match ops::rename(&path, new_name) {
            Ok(dst) => {
                let cwd = self.cwd.clone();
                self.list(cwd);
                if let Some(name) = dst.file_name() {
                    self.select_by_name(&name.to_string_lossy());
                }
            }
            Err(err) => log::warn!("rename {} failed: {err}", path.display()),
        }
    }

    pub fn new_folder(&mut self, name: &str) {
        match ops::create_folder(&self.cwd.clone(), name) {
            Ok(dst) => {
                let cwd = self.cwd.clone();
                self.list(cwd);
                if let Some(name) = dst.file_name() {
                    self.select_by_name(&name.to_string_lossy());
                }
            }
            Err(err) => log::warn!("create folder failed: {err}"),
        }
    }

    fn select_by_name(&mut self, name: &str) {
        if let Some(idx) = self
            .entries
            .iter()
            .position(|p| p.file_name().map_or(false, |n| n == name))
        {
            self.set_single(idx as i32);
        }
    }

    fn update_can_paste(&self) {
        if let Some(w) = self.weak.upgrade() {
            w.set_can_paste(!self.clipboard.is_empty());
        }
    }

    /// Push selection state into the model: per-row flags, the cursor, the
    /// selection count, and the preview pane.
    fn sync(&self) {
        let Some(w) = self.weak.upgrade() else {
            return;
        };
        for i in 0..self.entries.len() {
            if let Some(mut item) = self.items.row_data(i) {
                let want = self.marks.get(i).copied().unwrap_or(false);
                if item.selected != want {
                    item.selected = want;
                    self.items.set_row_data(i, item);
                }
            }
        }
        w.set_cursor(self.cursor);
        w.set_selection_count(self.marks.iter().filter(|m| **m).count() as i32);
        w.set_can_paste(!self.clipboard.is_empty());

        let path = if self.cursor >= 0 {
            self.entries.get(self.cursor as usize).cloned()
        } else {
            None
        };
        let name = path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        w.set_selected_name(name.into());
        w.set_selected_info(path.as_deref().map(describe).unwrap_or_default().into());
        let image = path
            .as_ref()
            .filter(|p| p.is_file() && is_image(p))
            .and_then(|p| slint::Image::load_from_path(p).ok())
            .unwrap_or_default();
        w.set_preview(image);
    }

    fn update_back(&self) {
        if let Some(w) = self.weak.upgrade() {
            w.set_can_back(!self.history.is_empty());
        }
    }

    /// Read `dir` into the unsorted cache (without touching history), then render
    /// it. Returns whether the directory was readable.
    fn list(&mut self, dir: PathBuf) -> bool {
        let dir = if dir.is_dir() {
            dir
        } else {
            PathBuf::from("/")
        };
        self.cwd = std::fs::canonicalize(&dir).unwrap_or(dir);
        self.raw.clear();

        let readable = std::fs::read_dir(&self.cwd).is_ok();
        if let Ok(read) = std::fs::read_dir(&self.cwd) {
            for entry in read.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = entry.path();
                let is_dir = path.is_dir();
                let kind = file_kind(&path, is_dir);
                let meta = std::fs::metadata(&path).ok();
                let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                let mtime = meta.as_ref().and_then(|m| m.modified().ok());
                let size_str = if is_dir {
                    "—".to_string()
                } else {
                    meta.as_ref()
                        .map(|m| human_size(m.len()))
                        .unwrap_or_default()
                };
                let modified_str = mtime.map(format_time).unwrap_or_default();
                // A thumbnail for image files (skip very large ones to stay snappy).
                let thumb = if is_image(&path) && len < 8 << 20 {
                    slint::Image::load_from_path(&path).unwrap_or_default()
                } else {
                    slint::Image::default()
                };
                self.raw.push(Entry {
                    name,
                    path,
                    is_dir,
                    kind,
                    len,
                    mtime,
                    size_str,
                    modified_str,
                    thumb,
                });
            }
        }
        // A fresh directory starts unfiltered; clear the search box too.
        self.filter.clear();
        if let Some(w) = self.weak.upgrade() {
            w.set_search_text(Default::default());
        }
        self.render(false);
        readable
    }

    /// Sort and filter the cache into the model. With `preserve`, the selection
    /// is carried over by path; otherwise the first entry is selected.
    fn render(&mut self, preserve: bool) {
        let prev_selected: HashSet<PathBuf> = if preserve {
            self.entries
                .iter()
                .enumerate()
                .filter(|(i, _)| self.marks.get(*i).copied().unwrap_or(false))
                .map(|(_, p)| p.clone())
                .collect()
        } else {
            HashSet::new()
        };
        let prev_cursor = if preserve && self.cursor >= 0 {
            self.entries.get(self.cursor as usize).cloned()
        } else {
            None
        };

        let mut list: Vec<Entry> = self
            .raw
            .iter()
            .filter(|e| self.show_hidden || !e.name.starts_with('.'))
            .filter(|e| self.filter.is_empty() || e.name.to_lowercase().contains(&self.filter))
            .cloned()
            .collect();
        let key = self.sort_key;
        list.sort_by(|a, b| {
            // Folders always come first, regardless of column or direction.
            if a.is_dir != b.is_dir {
                return if a.is_dir {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                };
            }
            let ord = match key {
                1 => a.len.cmp(&b.len),
                2 => a.mtime.cmp(&b.mtime),
                3 => a
                    .kind
                    .cmp(b.kind)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            };
            if self.sort_desc {
                ord.reverse()
            } else {
                ord
            }
        });

        self.entries = list.iter().map(|e| e.path.clone()).collect();
        let rows: Vec<FileItem> = list
            .iter()
            .map(|e| FileItem {
                name: e.name.clone().into(),
                is_dir: e.is_dir,
                kind: e.kind.into(),
                size: e.size_str.clone().into(),
                modified: e.modified_str.clone().into(),
                thumb: e.thumb.clone(),
                selected: false,
            })
            .collect();
        self.items.set_vec(rows);

        self.marks = vec![false; self.entries.len()];
        if preserve {
            for (i, p) in self.entries.iter().enumerate() {
                if prev_selected.contains(p) {
                    self.marks[i] = true;
                }
            }
            self.cursor = prev_cursor
                .and_then(|cp| self.entries.iter().position(|p| *p == cp))
                .map(|i| i as i32)
                .unwrap_or(if self.entries.is_empty() { -1 } else { 0 });
            self.anchor = self.cursor;
        } else if self.entries.is_empty() {
            self.cursor = -1;
            self.anchor = -1;
        } else {
            self.marks[0] = true;
            self.cursor = 0;
            self.anchor = 0;
        }

        if let Some(w) = self.weak.upgrade() {
            w.set_path(self.cwd.to_string_lossy().as_ref().into());
            w.set_window_title(format!("{} — Files", self.title_name()).into());
            w.set_sort_key(self.sort_key);
            w.set_sort_desc(self.sort_desc);
            w.set_show_hidden(self.show_hidden);
            w.set_crumbs(Rc::new(VecModel::from(self.crumbs())).into());
        }
        self.sync();
    }

    /// Build the breadcrumb segments for the current directory.
    fn crumbs(&self) -> Vec<Crumb> {
        use std::path::Component;
        let mut crumbs = vec![Crumb {
            name: "/".into(),
            path: "/".into(),
        }];
        let mut acc = PathBuf::from("/");
        for comp in self.cwd.components() {
            if let Component::Normal(c) = comp {
                acc.push(c);
                crumbs.push(Crumb {
                    name: c.to_string_lossy().as_ref().into(),
                    path: acc.to_string_lossy().as_ref().into(),
                });
            }
        }
        crumbs
    }

    fn title_name(&self) -> String {
        self.cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string())
    }
}

/// Launch a file with the system default handler, detached from this process.
fn launch(path: &Path) {
    use std::process::{Command, Stdio};
    let opener = if which("xdg-open") { "xdg-open" } else { "gio" };
    let mut cmd = Command::new(opener);
    if opener == "gio" {
        cmd.arg("open");
    }
    cmd.arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match cmd.spawn() {
        Ok(_) => log::info!("opened {}", path.display()),
        Err(err) => log::warn!("failed to open {}: {err}", path.display()),
    }
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// A short, human-readable description of a path: "Folder", or a file's size and
/// kind.
fn describe(path: &Path) -> String {
    if path.is_dir() {
        return "Folder".to_string();
    }
    match std::fs::metadata(path) {
        Ok(meta) => format!("{} · {}", human_size(meta.len()), file_kind(path, false)),
        Err(_) => file_kind(path, false).to_string(),
    }
}

/// Format a modification time as a compact local "YYYY-MM-DD HH:MM" stamp.
fn format_time(time: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(time)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Classify a path into a coarse kind used by the UI to pick an icon.
fn file_kind(path: &Path, is_dir: bool) -> &'static str {
    if is_dir {
        return "dir";
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("png" | "jpg" | "jpeg" | "bmp" | "gif" | "webp" | "svg" | "ico" | "tiff") => "image",
        Some("mp3" | "flac" | "wav" | "ogg" | "opus" | "m4a" | "aac") => "audio",
        Some("mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "wmv") => "video",
        Some("zip" | "tar" | "gz" | "bz2" | "xz" | "zst" | "7z" | "rar") => "archive",
        Some(
            "rs" | "c" | "h" | "cpp" | "py" | "js" | "ts" | "go" | "java" | "sh" | "toml" | "json"
            | "yaml" | "yml" | "slint" | "html" | "css",
        ) => "code",
        Some("txt" | "md" | "log" | "rst" | "ini" | "conf") => "text",
        Some("pdf") => "pdf",
        Some("appimage" | "bin" | "run" | "exe") => "exec",
        _ => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(path) {
                    if meta.permissions().mode() & 0o111 != 0 {
                        return "exec";
                    }
                }
            }
            "file"
        }
    }
}

fn is_image(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    matches!(
        ext.as_deref(),
        Some("png" | "jpg" | "jpeg" | "bmp" | "gif" | "webp" | "svg")
    )
}

/// Expand a leading `~` to the home directory.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix('~') {
        return format!("{}{}", home_dir().display(), rest);
    }
    path.to_string()
}

/// The user's home directory, or `/`.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Sidebar shortcuts: Home, the common XDG user directories that exist, and the
/// filesystem root. Returned as (label, path) pairs.
pub fn places() -> Vec<(String, String)> {
    let home = home_dir();
    let mut places = vec![("Home".to_string(), home.to_string_lossy().into_owned())];
    for sub in [
        "Desktop",
        "Documents",
        "Downloads",
        "Music",
        "Pictures",
        "Videos",
    ] {
        let path = home.join(sub);
        if path.is_dir() {
            places.push((sub.to_string(), path.to_string_lossy().into_owned()));
        }
    }
    places.push(("Filesystem".to_string(), "/".to_string()));
    places
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_file_kinds() {
        assert_eq!(file_kind(Path::new("/x"), true), "dir");
        assert_eq!(file_kind(Path::new("a.png"), false), "image");
        assert_eq!(file_kind(Path::new("a.JPEG"), false), "image");
        assert_eq!(file_kind(Path::new("a.mp3"), false), "audio");
        assert_eq!(file_kind(Path::new("main.rs"), false), "code");
        assert_eq!(file_kind(Path::new("a.pdf"), false), "pdf");
        assert_eq!(
            file_kind(Path::new("/nonexistent/unknown.xyz"), false),
            "file"
        );
    }

    #[test]
    fn recognizes_images() {
        assert!(is_image(Path::new("a.png")));
        assert!(is_image(Path::new("a.JPG")));
        assert!(!is_image(Path::new("a.txt")));
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1024 * 1024 * 3), "3.0 MB");
    }

    #[test]
    fn expands_tilde() {
        std::env::set_var("HOME", "/home/u");
        assert_eq!(expand_tilde("~/x"), "/home/u/x");
        assert_eq!(expand_tilde("/abs"), "/abs");
    }

    // --- Model-level operation wiring (headless, no window) -----------------

    fn scratch() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("sfiles-model-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A browser with no attached window: navigation, selection and operations
    /// all keep their state in Rust, so the wiring is testable headlessly.
    fn headless() -> Browser {
        Browser::new(slint::Weak::default(), Rc::new(VecModel::default()))
    }

    #[test]
    fn copy_paste_via_selection() {
        let dir = scratch();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        let mut b = headless();
        b.navigate(dir.clone()); // selects the only entry
        b.copy();
        b.paste();
        assert_eq!(
            std::fs::read_to_string(dir.join("a (copy).txt")).unwrap(),
            "hello"
        );
        assert!(dir.join("a.txt").exists()); // original kept
    }

    #[test]
    fn range_selection_copies_all() {
        let dir = scratch();
        for n in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.join(n), n).unwrap();
        }
        let mut b = headless();
        b.navigate(dir.clone()); // cursor + anchor at 0 (a.txt)
        b.row_pressed(2, false, true); // Shift-select through c.txt
        b.copy();
        b.paste();
        for n in ["a (copy).txt", "b (copy).txt", "c (copy).txt"] {
            assert!(dir.join(n).exists(), "missing {n}");
        }
    }

    #[test]
    fn rename_and_new_folder() {
        let dir = scratch();
        std::fs::write(dir.join("old.txt"), "x").unwrap();
        let mut b = headless();
        b.navigate(dir.clone());
        b.rename("new.txt");
        assert!(dir.join("new.txt").exists() && !dir.join("old.txt").exists());
        b.new_folder("Project");
        assert!(dir.join("Project").is_dir());
    }

    #[test]
    fn trash_via_selection() {
        let dir = scratch();
        std::env::set_var("XDG_DATA_HOME", dir.join("xdgdata"));
        std::fs::write(dir.join("junk.txt"), "x").unwrap();
        let mut b = headless();
        b.navigate(dir.clone());
        b.trash();
        assert!(!dir.join("junk.txt").exists());
        assert!(dir.join("xdgdata/Trash/files/junk.txt").exists());
    }

    fn names(b: &Browser) -> Vec<String> {
        b.entries
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn sorting_keeps_folders_first() {
        let dir = scratch();
        std::fs::create_dir(dir.join("zdir")).unwrap();
        std::fs::write(dir.join("a.txt"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("b.txt"), vec![0u8; 1]).unwrap();
        let mut b = headless();
        b.navigate(dir.clone());
        // Name ascending (default): folder first, then a, b.
        assert_eq!(names(&b), ["zdir", "a.txt", "b.txt"]);
        // Size ascending: folder first, then smaller file first.
        b.set_sort(1);
        assert_eq!(names(&b), ["zdir", "b.txt", "a.txt"]);
        // Clicking Size again flips to descending.
        b.set_sort(1);
        assert_eq!(names(&b), ["zdir", "a.txt", "b.txt"]);
        // Back to name descending.
        b.set_sort(0);
        b.set_sort(0);
        assert_eq!(names(&b), ["zdir", "b.txt", "a.txt"]);
    }

    #[test]
    fn search_filters_by_name() {
        let dir = scratch();
        for n in ["apple.txt", "banana.txt", "apricot.md"] {
            std::fs::write(dir.join(n), "x").unwrap();
        }
        let mut b = headless();
        b.navigate(dir.clone());
        b.set_filter("AP"); // case-insensitive substring
        assert_eq!(names(&b), ["apple.txt", "apricot.md"]);
        b.set_filter("");
        assert_eq!(names(&b).len(), 3);
        // Navigating clears the filter.
        b.set_filter("zzz");
        assert!(names(&b).is_empty());
        b.go_up();
        b.go_to(dir.to_str().unwrap());
        assert_eq!(names(&b).len(), 3);
    }

    #[test]
    fn hidden_files_toggle() {
        let dir = scratch();
        std::fs::write(dir.join(".secret"), "x").unwrap();
        std::fs::write(dir.join("visible.txt"), "x").unwrap();
        let mut b = headless();
        b.navigate(dir.clone());
        assert_eq!(names(&b), ["visible.txt"]);
        b.toggle_hidden();
        assert_eq!(names(&b), [".secret", "visible.txt"]);
        b.toggle_hidden();
        assert_eq!(names(&b), ["visible.txt"]);
    }

    #[test]
    fn sort_preserves_selection_by_path() {
        let dir = scratch();
        for n in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.join(n), "x").unwrap();
        }
        let mut b = headless();
        b.navigate(dir.clone());
        b.row_pressed(1, false, false); // select b.txt
        assert_eq!(b.cursor, 1);
        b.set_sort(0); // name descending -> c, b, a
        assert_eq!(names(&b), ["c.txt", "b.txt", "a.txt"]);
        // b.txt is still the cursor (now at index 1 by coincidence) and selected.
        assert!(b.marks[b.cursor as usize]);
        assert_eq!(
            b.entries[b.cursor as usize]
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "b.txt"
        );
    }

    #[test]
    fn cut_then_paste_moves() {
        let dir = scratch();
        let src = dir.join("src");
        let dst = dir.join("dst");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(src.join("f.txt"), "data").unwrap();
        let mut b = headless();
        b.navigate(src.clone()); // selects f.txt
        b.cut();
        b.navigate(dst.clone());
        b.paste();
        assert!(dst.join("f.txt").exists());
        assert!(!src.join("f.txt").exists()); // moved, not copied
    }
}
