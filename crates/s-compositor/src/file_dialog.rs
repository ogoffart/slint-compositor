//! Controller for the reusable file-open dialog.
//!
//! The dialog UI ([`FileDialog`](crate::FileDialog) in `file_dialog.slint`) is
//! purely a view; all logic — directory listing, navigation, selection — lives
//! here. The same controller backs both the in-shell settings flow and the XDG
//! desktop portal, so any caller can request a file via [`Controller::open`].

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::VecModel;

use crate::{Desktop, FileItem};

/// Called with the chosen path, or `None` if the dialog was cancelled.
pub type ResultCb = Box<dyn FnOnce(Option<PathBuf>)>;

pub struct Controller {
    weak: slint::Weak<Desktop>,
    items: Rc<VecModel<FileItem>>,
    cwd: PathBuf,
    /// Full path for each row currently shown.
    entries: Vec<PathBuf>,
    on_result: Option<ResultCb>,
    image_only: bool,
}

impl Controller {
    pub fn new(weak: slint::Weak<Desktop>, items: Rc<VecModel<FileItem>>) -> Self {
        Self {
            weak,
            items,
            cwd: PathBuf::from("/"),
            entries: Vec::new(),
            on_result: None,
            image_only: false,
        }
    }

    /// Open the dialog rooted at `start`. `cb` is invoked (exactly once) with the
    /// result. If a dialog is already open the request is rejected with `None`.
    /// The callback must not call back into the controller.
    pub fn open(&mut self, title: &str, start: PathBuf, image_only: bool, cb: ResultCb) {
        if self.on_result.is_some() {
            log::warn!("file dialog already open; rejecting new request");
            cb(None);
            return;
        }
        self.image_only = image_only;
        self.on_result = Some(cb);
        if let Some(d) = self.weak.upgrade() {
            d.set_file_dialog_title(title.into());
            d.set_file_dialog_visible(true);
        }
        self.navigate(start);
    }

    pub fn entry_clicked(&mut self, idx: i32) {
        if idx < 0 || idx as usize >= self.entries.len() {
            return;
        }
        // A single click selects (and previews); a directory is entered on
        // double-click, Enter or →, keeping mouse and keyboard consistent.
        self.update_selection(idx);
    }

    pub fn go_up(&mut self) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.navigate(parent);
        }
    }

    /// Navigate to a path typed into the editable path bar: a directory is
    /// entered, a file enters its parent and is selected.
    pub fn navigate_to(&mut self, path: &str) {
        let path = PathBuf::from(expand_tilde(path));
        if path.is_dir() {
            self.navigate(path);
        } else if path.is_file() {
            if let Some(parent) = path.parent().map(Path::to_path_buf) {
                self.navigate(parent);
                self.select_path(&path);
            }
        }
    }

    /// Move the keyboard selection by `delta` rows (clamped to the list). Large
    /// magnitudes act as Home/End.
    pub fn move_selection(&mut self, delta: i32) {
        let Some(d) = self.weak.upgrade() else {
            return;
        };
        let count = self.entries.len() as i32;
        if count == 0 {
            return;
        }
        let current = d.get_file_dialog_selected();
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

    /// Set the current selection and refresh the preview pane: the selected
    /// name, plus a loaded image when the selection is an image file.
    fn update_selection(&mut self, idx: i32) {
        let Some(d) = self.weak.upgrade() else {
            return;
        };
        d.set_file_dialog_selected(idx);
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
        d.set_file_dialog_selected_name(name.into());

        let image = path
            .as_ref()
            .filter(|p| p.is_file() && is_image(p))
            .and_then(|p| slint::Image::load_from_path(p).ok())
            .unwrap_or_default();
        d.set_file_dialog_preview(image);
    }

    /// Enter the selected directory, or accept the selected file (Enter key).
    pub fn activate_selected(&mut self) {
        let sel = self
            .weak
            .upgrade()
            .map(|d| d.get_file_dialog_selected())
            .unwrap_or(-1);
        match self.entries.get(sel.max(0) as usize).cloned() {
            Some(path) if sel >= 0 && path.is_dir() => self.navigate(path),
            Some(_) if sel >= 0 => self.accept(),
            _ => {}
        }
    }

    fn select_path(&mut self, path: &Path) {
        if let Some(idx) = self.entries.iter().position(|p| p == path) {
            self.update_selection(idx as i32);
        }
    }

    pub fn accept(&mut self) {
        let sel = self
            .weak
            .upgrade()
            .map(|d| d.get_file_dialog_selected())
            .unwrap_or(-1);
        let Some(path) = (if sel >= 0 {
            self.entries.get(sel as usize).cloned()
        } else {
            None
        }) else {
            return; // nothing selected
        };
        // "Open" on a directory enters it rather than returning it as a result.
        if path.is_dir() {
            self.navigate(path);
            return;
        }
        self.close();
        if let Some(cb) = self.on_result.take() {
            cb(Some(path));
        }
    }

    pub fn cancel(&mut self) {
        self.close();
        if let Some(cb) = self.on_result.take() {
            cb(None);
        }
    }

    fn close(&mut self) {
        if let Some(d) = self.weak.upgrade() {
            d.set_file_dialog_visible(false);
        }
    }

    fn navigate(&mut self, dir: PathBuf) {
        let dir = if dir.is_dir() {
            dir
        } else {
            PathBuf::from("/")
        };
        self.cwd = std::fs::canonicalize(&dir).unwrap_or(dir);
        self.entries.clear();

        let (mut dirs, mut files) = (Vec::new(), Vec::new());
        if let Ok(read) = std::fs::read_dir(&self.cwd) {
            for entry in read.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    dirs.push((name, path));
                } else if !self.image_only || is_image(&path) {
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
            self.entries.push(path);
            rows.push(FileItem {
                name: name.into(),
                is_dir,
                kind: kind.into(),
            });
        }
        let has_entries = !rows.is_empty();
        self.items.set_vec(rows);

        if let Some(d) = self.weak.upgrade() {
            d.set_file_dialog_path(self.cwd.to_string_lossy().as_ref().into());
        }
        // Pre-select the first entry so keyboard navigation and the preview work
        // immediately on entering a directory.
        self.update_selection(if has_entries { 0 } else { -1 });
    }
}

/// A shared, reference-counted controller.
pub type SharedController = Rc<RefCell<Controller>>;

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
            // Treat any executable-bit file as a program.
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
    use super::{file_kind, is_image};
    use std::path::Path;

    #[test]
    fn classifies_file_kinds() {
        assert_eq!(file_kind(Path::new("/x"), true), "dir");
        assert_eq!(file_kind(Path::new("a.png"), false), "image");
        assert_eq!(file_kind(Path::new("a.JPEG"), false), "image");
        assert_eq!(file_kind(Path::new("a.mp3"), false), "audio");
        assert_eq!(file_kind(Path::new("a.mkv"), false), "video");
        assert_eq!(file_kind(Path::new("a.tar"), false), "archive");
        assert_eq!(file_kind(Path::new("main.rs"), false), "code");
        assert_eq!(file_kind(Path::new("a.toml"), false), "code");
        assert_eq!(file_kind(Path::new("notes.txt"), false), "text");
        assert_eq!(file_kind(Path::new("a.pdf"), false), "pdf");
        assert_eq!(file_kind(Path::new("a.exe"), false), "exec");
        assert_eq!(
            file_kind(Path::new("/nonexistent/unknown.xyz"), false),
            "file"
        );
    }

    #[test]
    fn recognizes_image_extensions() {
        assert!(is_image(Path::new("a.png")));
        assert!(is_image(Path::new("a.JPG"))); // case-insensitive
        assert!(is_image(Path::new("/x/y.jpeg")));
        assert!(is_image(Path::new("a.webp")));
    }

    #[test]
    fn rejects_non_images() {
        assert!(!is_image(Path::new("a.txt")));
        assert!(!is_image(Path::new("noext")));
        assert!(!is_image(Path::new("a.tar.gz")));
    }
}
