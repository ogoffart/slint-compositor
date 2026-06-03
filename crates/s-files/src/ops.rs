//! Filesystem operations for the browser: copy, move, trash, rename and folder
//! creation. Kept free of any UI types so the logic is unit-testable headlessly.

use std::io;
use std::path::{Path, PathBuf};

/// Pick a non-colliding path for `name` inside `dir`. If `dir/name` is free it
/// is returned as-is; otherwise " (copy)", " (copy 2)", … is inserted before the
/// extension.
pub fn unique_destination(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = split_name(name);
    for n in 1.. {
        let suffix = if n == 1 {
            " (copy)".to_string()
        } else {
            format!(" (copy {n})")
        };
        let new_name = match &ext {
            Some(ext) => format!("{stem}{suffix}.{ext}"),
            None => format!("{stem}{suffix}"),
        };
        let candidate = dir.join(&new_name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

/// Split a file name into (stem, extension). Dotfiles like ".bashrc" keep the
/// whole name as the stem.
fn split_name(name: &str) -> (String, Option<String>) {
    match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), Some(name[i + 1..].to_string())),
        _ => (name.to_string(), None),
    }
}

/// Copy `src` (file or directory) into `dst_dir`, choosing a collision-free name.
/// Returns the path of the created copy.
pub fn copy_into(src: &Path, dst_dir: &Path) -> io::Result<PathBuf> {
    let name = file_name(src)?;
    let dst = unique_destination(dst_dir, &name);
    copy_recursive(src, &dst)?;
    Ok(dst)
}

fn copy_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// Move `src` into `dst_dir`, choosing a collision-free name. Falls back to
/// copy-then-delete when a plain rename can't cross filesystems. Returns the new
/// path.
pub fn move_into(src: &Path, dst_dir: &Path) -> io::Result<PathBuf> {
    let name = file_name(src)?;
    let dst = unique_destination(dst_dir, &name);
    match std::fs::rename(src, &dst) {
        Ok(()) => Ok(dst),
        Err(_) => {
            copy_recursive(src, &dst)?;
            remove(src)?;
            Ok(dst)
        }
    }
}

/// Rename `path` in place to `new_name` (a bare file name, not a path).
pub fn rename(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    let new_name = new_name.trim();
    if new_name.is_empty() || new_name.contains('/') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid name"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent"))?;
    let dst = parent.join(new_name);
    if dst == path {
        return Ok(dst);
    }
    if dst.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a file with that name already exists",
        ));
    }
    std::fs::rename(path, &dst)?;
    Ok(dst)
}

/// Create a new directory `name` inside `dir` (collision-free). Returns its path.
pub fn create_folder(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() || name.contains('/') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid name"));
    }
    let dst = unique_destination(dir, name);
    std::fs::create_dir(&dst)?;
    Ok(dst)
}

fn remove(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Move `path` to the freedesktop trash (`$XDG_DATA_HOME/Trash`), writing the
/// accompanying `.trashinfo` so it can be restored.
pub fn trash(path: &Path) -> io::Result<()> {
    let trash_dir = data_home().join("Trash");
    let files = trash_dir.join("files");
    let info = trash_dir.join("info");
    std::fs::create_dir_all(&files)?;
    std::fs::create_dir_all(&info)?;

    let name = file_name(path)?;
    // Find a free name shared by both files/ and info/.
    let mut target = name.clone();
    let mut n = 1;
    while files.join(&target).exists() || info.join(format!("{target}.trashinfo")).exists() {
        n += 1;
        let (stem, ext) = split_name(&name);
        target = match &ext {
            Some(ext) => format!("{stem}.{n}.{ext}"),
            None => format!("{stem}.{n}"),
        };
    }

    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let info_text = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        percent_encode(&absolute.to_string_lossy()),
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S"),
    );
    // Write the info file first so a partly-trashed item is always described.
    std::fs::write(info.join(format!("{target}.trashinfo")), info_text)?;

    let dst = files.join(&target);
    if let Err(_) = std::fs::rename(path, &dst) {
        copy_recursive(path, &dst)?;
        remove(path)?;
    }
    Ok(())
}

/// Percent-encode a path for a `.trashinfo` `Path=` value (everything but the
/// unreserved set and `/`).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn file_name(path: &Path) -> io::Result<String> {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))
}

/// `$XDG_DATA_HOME`, or `~/.local/share`.
fn data_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    home.join(".local/share")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sfiles-ops-{}", uniq()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn uniq() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        static C: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = C.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
            ^ (n << 32)
    }

    #[test]
    fn unique_destination_dedups() {
        let dir = tmp();
        assert_eq!(unique_destination(&dir, "a.txt"), dir.join("a.txt"));
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        assert_eq!(unique_destination(&dir, "a.txt"), dir.join("a (copy).txt"));
        std::fs::write(dir.join("a (copy).txt"), "x").unwrap();
        assert_eq!(
            unique_destination(&dir, "a.txt"),
            dir.join("a (copy 2).txt")
        );
        // Dotfile with no extension.
        std::fs::write(dir.join("notes"), "x").unwrap();
        assert_eq!(unique_destination(&dir, "notes"), dir.join("notes (copy)"));
    }

    #[test]
    fn copy_file_and_dir() {
        let dir = tmp();
        let src = dir.join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("f.txt"), "hello").unwrap();
        let nested = src.join("sub");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("g.txt"), "world").unwrap();

        let dst_dir = dir.join("dst");
        std::fs::create_dir(&dst_dir).unwrap();
        let out = copy_into(&src, &dst_dir).unwrap();
        assert_eq!(out, dst_dir.join("src"));
        assert_eq!(std::fs::read_to_string(out.join("f.txt")).unwrap(), "hello");
        assert_eq!(
            std::fs::read_to_string(out.join("sub/g.txt")).unwrap(),
            "world"
        );
        // Original survives a copy.
        assert!(src.join("f.txt").exists());

        // A second copy into the same dir dedups.
        let out2 = copy_into(&src, &dst_dir).unwrap();
        assert_eq!(out2, dst_dir.join("src (copy)"));
    }

    #[test]
    fn move_removes_source() {
        let dir = tmp();
        let src = dir.join("m.txt");
        std::fs::write(&src, "data").unwrap();
        let dst_dir = dir.join("d");
        std::fs::create_dir(&dst_dir).unwrap();
        let out = move_into(&src, &dst_dir).unwrap();
        assert_eq!(out, dst_dir.join("m.txt"));
        assert!(!src.exists());
        assert_eq!(std::fs::read_to_string(out).unwrap(), "data");
    }

    #[test]
    fn rename_and_new_folder() {
        let dir = tmp();
        let f = dir.join("old.txt");
        std::fs::write(&f, "x").unwrap();
        let nf = rename(&f, "new.txt").unwrap();
        assert_eq!(nf, dir.join("new.txt"));
        assert!(!f.exists() && nf.exists());
        // Rejects collisions and bad names.
        std::fs::write(dir.join("taken.txt"), "y").unwrap();
        assert!(rename(&nf, "taken.txt").is_err());
        assert!(rename(&nf, "a/b").is_err());

        let folder = create_folder(&dir, "Project").unwrap();
        assert!(folder.is_dir());
        let folder2 = create_folder(&dir, "Project").unwrap();
        assert_eq!(folder2, dir.join("Project (copy)"));
    }

    #[test]
    fn trash_moves_and_writes_info() {
        let dir = tmp();
        // Point the trash at an isolated data home.
        let data = dir.join("data");
        std::env::set_var("XDG_DATA_HOME", &data);

        let victim = dir.join("doomed.txt");
        std::fs::write(&victim, "bye").unwrap();
        trash(&victim).unwrap();
        assert!(!victim.exists());

        let files = data.join("Trash/files/doomed.txt");
        let info = data.join("Trash/info/doomed.txt.trashinfo");
        assert_eq!(std::fs::read_to_string(&files).unwrap(), "bye");
        let info_text = std::fs::read_to_string(&info).unwrap();
        assert!(info_text.contains("[Trash Info]"));
        assert!(info_text.contains("Path=/"));
        assert!(info_text.contains("DeletionDate="));

        // A second file of the same name gets a distinct trash name.
        std::fs::write(&victim, "again").unwrap();
        trash(&victim).unwrap();
        assert!(data.join("Trash/files/doomed.2.txt").exists());
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(percent_encode("/a/b c"), "/a/b%20c");
        assert_eq!(percent_encode("/tmp/x.txt"), "/tmp/x.txt");
        assert_eq!(percent_encode("/é"), "/%C3%A9");
    }
}
