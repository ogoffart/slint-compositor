use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let ui = manifest_dir.join("../../ui/desktop.slint");
    slint_build::compile(&ui).expect("failed to compile Slint UI");

    build_bundled_s_files(&manifest_dir);
}

/// Build the bundled `s-files` browser so a plain `cargo run` of the compositor
/// also produces the `s-files` binary it launches as its file manager. The
/// compositor finds it next to its own executable (see `config::bundled`), so we
/// copy the freshly built binary into the compositor's output directory.
///
/// `s-files` is its own workspace member, so `cargo build`/`cargo run` of the
/// compositor would not otherwise build it. We invoke cargo from here, into a
/// *separate* target directory: reusing the outer build's target dir would
/// deadlock on its build lock.
fn build_bundled_s_files(manifest_dir: &Path) {
    let workspace = manifest_dir.join("../..");

    // Rebuild the bundled binary when any of its sources change.
    rerun_if_tree_changed(&workspace.join("crates/s-files/src"));
    rerun_if_tree_changed(&workspace.join("ui"));
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join("crates/s-files/Cargo.toml").display()
    );

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));
    // OUT_DIR is `<target>/<profile>/build/<pkg>-<hash>/out`; three levels up is
    // `<target>/<profile>`, where the compositor binary is placed.
    let Some(profile_dir) = out_dir.ancestors().nth(3) else {
        println!("cargo:warning=could not locate the compositor output directory");
        return;
    };
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let nested_target = out_dir.join("s-files-target");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let build_once = || {
        let mut cmd = Command::new(&cargo);
        cmd.current_dir(&workspace)
            .arg("build")
            .args(["--package", "s-files"])
            .arg("--target-dir")
            .arg(&nested_target)
            .env_remove("CARGO_TARGET_DIR");
        if profile == "release" {
            cmd.arg("--release");
        }
        cmd.output()
    };

    // Cold builds occasionally fail on a transient lock/race against the outer
    // build; retry a few times before giving up.
    let mut last = None;
    for attempt in 0..3 {
        match build_once() {
            Ok(out) if out.status.success() => {
                let built = nested_target.join(&profile).join("s-files");
                let dest = profile_dir.join("s-files");
                if let Err(err) = std::fs::copy(&built, &dest) {
                    println!(
                        "cargo:warning=built s-files but could not copy it to {}: {err}",
                        dest.display()
                    );
                }
                return;
            }
            Ok(out) => last = Some(String::from_utf8_lossy(&out.stderr).into_owned()),
            Err(err) => last = Some(err.to_string()),
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }

    println!("cargo:warning=could not build the bundled s-files file manager");
    for line in last.unwrap_or_default().lines() {
        println!("cargo:warning=  s-files: {line}");
    }
}

/// Emit `rerun-if-changed` for every file under `dir` (recursively), so edits to
/// any of them retrigger this build script.
fn rerun_if_tree_changed(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rerun_if_tree_changed(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
