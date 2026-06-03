//! Resolve a window's `app_id` to an icon image, the freedesktop way: find the
//! app's `.desktop` file, read its `Icon=` key, then locate that icon in the
//! icon theme directories. Results are cached so each app-id is resolved once.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Caches `app_id -> icon` lookups (including misses, stored as a default image).
#[derive(Default)]
pub struct IconCache {
    cache: RefCell<HashMap<String, slint::Image>>,
}

impl IconCache {
    /// Return the icon for `app_id`, or a default (empty) image if none is found.
    pub fn get(&self, app_id: &str) -> slint::Image {
        if app_id.is_empty() {
            return slint::Image::default();
        }
        if let Some(img) = self.cache.borrow().get(app_id) {
            return img.clone();
        }
        let img = resolve(app_id)
            .and_then(|path| slint::Image::load_from_path(&path).ok())
            .unwrap_or_default();
        self.cache
            .borrow_mut()
            .insert(app_id.to_string(), img.clone());
        img
    }
}

/// Base data directories per the XDG spec (`$XDG_DATA_HOME` + `$XDG_DATA_DIRS`).
fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(home));
    } else if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share"));
    }
    let extra = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    dirs.extend(
        extra
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    );
    dirs
}

/// Find the icon file path for an `app_id`.
fn resolve(app_id: &str) -> Option<PathBuf> {
    let icon_name = desktop_icon_name(app_id).unwrap_or_else(|| app_id.to_string());

    // An absolute path in the Icon= key is used directly.
    let p = Path::new(&icon_name);
    if p.is_absolute() && p.is_file() {
        return Some(p.to_path_buf());
    }
    find_icon_file(&icon_name).or_else(|| find_icon_file(&app_id.to_lowercase()))
}

/// Read the `Icon=` key from the app's `.desktop` file, if present.
fn desktop_icon_name(app_id: &str) -> Option<String> {
    // Try `<app_id>.desktop` and its lowercased form in each applications dir.
    let names = [
        format!("{app_id}.desktop"),
        format!("{}.desktop", app_id.to_lowercase()),
    ];
    for dir in data_dirs() {
        let apps = dir.join("applications");
        for name in &names {
            if let Ok(text) = std::fs::read_to_string(apps.join(name)) {
                for line in text.lines() {
                    if let Some(value) = line.strip_prefix("Icon=") {
                        let value = value.trim();
                        if !value.is_empty() {
                            return Some(value.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Search the icon theme directories for `<name>.{png,svg}`.
fn find_icon_file(name: &str) -> Option<PathBuf> {
    let sizes = [
        "scalable", "512x512", "256x256", "128x128", "96x96", "64x64", "48x48", "32x32",
    ];
    let themes = ["hicolor", "Adwaita", "breeze", "gnome"];
    let exts = ["png", "svg"];

    for dir in data_dirs() {
        let icons = dir.join("icons");
        for theme in &themes {
            for size in &sizes {
                for ext in &exts {
                    let candidate = icons
                        .join(theme)
                        .join(size)
                        .join("apps")
                        .join(format!("{name}.{ext}"));
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
    }

    // Legacy flat locations.
    for base in ["/usr/share/pixmaps", "/usr/share/icons"] {
        for ext in &exts {
            let candidate = Path::new(base).join(format!("{name}.{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
