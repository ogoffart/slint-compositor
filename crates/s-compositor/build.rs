use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let ui = Path::new(&manifest_dir).join("../../ui/desktop.slint");
    slint_build::compile(&ui).expect("failed to compile Slint UI");
}
