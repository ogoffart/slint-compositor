//! `s-files` — a small, standalone file browser for the slick desktop shell.
//!
//! It runs as an ordinary Wayland client (Slint's winit backend) and shares the
//! look and keyboard model of the in-shell file dialog: type-coloured icons, an
//! image preview pane and full keyboard navigation. Files are opened with the
//! system default handler (`xdg-open`); double-clicking or Enter on a directory
//! enters it.

mod model;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use slint::VecModel;

slint::include_modules!();

use model::Browser;

fn main() -> Result<(), slint::PlatformError> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let window = Files::new()?;
    let items = Rc::new(VecModel::<FileItem>::default());
    window.set_items(items.clone().into());

    let browser: model::Shared = Rc::new(RefCell::new(Browser::new(window.as_weak(), items)));

    // The starting directory: the first CLI argument, or $HOME.
    let start = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(model::home_dir);
    browser.borrow_mut().navigate(start);

    macro_rules! on {
        ($setter:ident, $method:ident) => {{
            let b = browser.clone();
            window.$setter(move || b.borrow_mut().$method());
        }};
        ($setter:ident, $method:ident, arg) => {{
            let b = browser.clone();
            window.$setter(move |arg| b.borrow_mut().$method(arg));
        }};
    }

    on!(on_go_back, go_back);
    on!(on_go_up, go_up);
    on!(on_go_home, go_home);
    on!(on_activate_selected, activate_selected);
    on!(on_entry_clicked, entry_clicked, arg);
    on!(on_activate, activate, arg);
    on!(on_move_selection, move_selection, arg);
    {
        let b = browser.clone();
        window.on_navigate_to(move |path| b.borrow_mut().navigate_to(path.as_str()));
    }

    window.run()
}
