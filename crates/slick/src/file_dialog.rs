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
        let Some(path) = self.entries.get(idx as usize).cloned() else {
            return;
        };
        if path.is_dir() {
            self.navigate(path);
        } else if let Some(d) = self.weak.upgrade() {
            d.set_file_dialog_selected(idx);
        }
    }

    pub fn go_up(&mut self) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.navigate(parent);
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
            self.entries.push(path);
            rows.push(FileItem {
                name: name.into(),
                is_dir,
            });
        }
        self.items.set_vec(rows);

        if let Some(d) = self.weak.upgrade() {
            d.set_file_dialog_path(self.cwd.to_string_lossy().as_ref().into());
            d.set_file_dialog_selected(-1);
        }
    }
}

/// A shared, reference-counted controller.
pub type SharedController = Rc<RefCell<Controller>>;

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

/// The user's home directory, or `/`.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}
