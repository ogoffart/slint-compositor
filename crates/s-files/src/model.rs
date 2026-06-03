//! Controller for the standalone Files browser.
//!
//! The window ([`Files`](crate::Files), compiled from `ui/files.slint`) is a
//! pure view; all behaviour — directory listing, navigation history, selection,
//! preview loading and launching — lives here. It is deliberately self-contained
//! (no dependency on the compositor crate) so the browser is an ordinary,
//! independent binary.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::VecModel;

use crate::{FileItem, Files};

pub struct Browser {
    weak: slint::Weak<Files>,
    items: Rc<VecModel<FileItem>>,
    cwd: PathBuf,
    /// Full path for each row currently shown.
    entries: Vec<PathBuf>,
    /// Directories visited, for the Back button.
    history: Vec<PathBuf>,
}

/// A shared, reference-counted browser.
pub type Shared = Rc<RefCell<Browser>>;

impl Browser {
    pub fn new(weak: slint::Weak<Files>, items: Rc<VecModel<FileItem>>) -> Self {
        Self {
            weak,
            items,
            cwd: PathBuf::from("/"),
            entries: Vec::new(),
            history: Vec::new(),
        }
    }

    /// Enter `dir`, pushing the current directory onto the back-history.
    pub fn navigate(&mut self, dir: PathBuf) {
        let prev = self.cwd.clone();
        if self.list(dir) && !self.entries.is_empty() {
            // (list() updated self.cwd; only record history on a real move.)
        }
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
                    self.update_selection(idx as i32);
                }
            }
        }
    }

    pub fn entry_clicked(&mut self, idx: i32) {
        if idx >= 0 && (idx as usize) < self.entries.len() {
            self.update_selection(idx);
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
        let sel = self.weak.upgrade().map(|w| w.get_selected()).unwrap_or(-1);
        if sel >= 0 {
            self.activate(sel);
        }
    }

    /// Move the keyboard selection by `delta` rows (clamped). Large magnitudes
    /// act as Home/End.
    pub fn move_selection(&mut self, delta: i32) {
        let count = self.entries.len() as i32;
        if count == 0 {
            return;
        }
        let current = self.weak.upgrade().map(|w| w.get_selected()).unwrap_or(-1);
        let next = if current < 0 {
            if delta < 0 {
                count - 1
            } else {
                0
            }
        } else {
            (current + delta).clamp(0, count - 1)
        };
        self.update_selection(next);
    }

    /// Set the current selection and refresh the preview pane.
    fn update_selection(&mut self, idx: i32) {
        let Some(w) = self.weak.upgrade() else {
            return;
        };
        w.set_selected(idx);
        let path = if idx >= 0 {
            self.entries.get(idx as usize).cloned()
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

    /// List `dir` into the model (without touching history). Returns whether the
    /// directory was readable.
    fn list(&mut self, dir: PathBuf) -> bool {
        let dir = if dir.is_dir() {
            dir
        } else {
            PathBuf::from("/")
        };
        self.cwd = std::fs::canonicalize(&dir).unwrap_or(dir);
        self.entries.clear();

        let (mut dirs, mut files) = (Vec::new(), Vec::new());
        let readable = std::fs::read_dir(&self.cwd).is_ok();
        if let Ok(read) = std::fs::read_dir(&self.cwd) {
            for entry in read.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    dirs.push((name, path));
                } else {
                    files.push((name, path));
                }
            }
        }
        dirs.sort_by_key(|(n, _)| n.to_lowercase());
        files.sort_by_key(|(n, _)| n.to_lowercase());

        let mut rows = Vec::with_capacity(dirs.len() + files.len());
        for (name, path) in dirs.into_iter().chain(files) {
            let is_dir = path.is_dir();
            let kind = file_kind(&path, is_dir);
            let meta = std::fs::metadata(&path).ok();
            let size = if is_dir {
                "—".to_string()
            } else {
                meta.as_ref()
                    .map(|m| human_size(m.len()))
                    .unwrap_or_default()
            };
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .map(format_time)
                .unwrap_or_default();
            // A thumbnail for image files (skip very large ones to stay snappy).
            let thumb = if is_image(&path) && meta.as_ref().map_or(false, |m| m.len() < 8 << 20) {
                slint::Image::load_from_path(&path).unwrap_or_default()
            } else {
                slint::Image::default()
            };
            self.entries.push(path);
            rows.push(FileItem {
                name: name.into(),
                is_dir,
                kind: kind.into(),
                size: size.into(),
                modified: modified.into(),
                thumb,
            });
        }
        let has_entries = !rows.is_empty();
        self.items.set_vec(rows);

        if let Some(w) = self.weak.upgrade() {
            w.set_path(self.cwd.to_string_lossy().as_ref().into());
            w.set_window_title(format!("{} — Files", self.title_name()).into());
        }
        self.update_selection(if has_entries { 0 } else { -1 });
        readable
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
}
