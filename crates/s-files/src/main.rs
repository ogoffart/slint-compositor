//! `s-files` — a small, standalone file browser for the slick desktop shell.
//!
//! It runs as an ordinary Wayland client (Slint's winit backend) and shares the
//! look and keyboard model of the in-shell file dialog: type-coloured icons, an
//! image preview pane and full keyboard navigation. Files are opened with the
//! system default handler (`xdg-open`); double-clicking or Enter on a directory
//! enters it.

mod model;
mod ops;

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

    // The sidebar shortcuts are fixed for the session.
    let places = Rc::new(VecModel::<Place>::default());
    places.set_vec(
        model::places()
            .into_iter()
            .map(|(name, path)| Place {
                name: name.into(),
                path: path.into(),
            })
            .collect::<Vec<_>>(),
    );
    window.set_places(places.into());

    let browser: model::Shared = Rc::new(RefCell::new(Browser::new(window.as_weak(), items)));

    // The starting directory: the first CLI argument, or $HOME.
    let start = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(model::home_dir);
    browser.borrow_mut().navigate(start);

    macro_rules! on {
        // Zero-argument callback.
        ($setter:ident, $method:ident) => {{
            let b = browser.clone();
            window.$setter(move || b.borrow_mut().$method());
        }};
        // Callback forwarding its argument(s) verbatim.
        ($setter:ident, $method:ident, |$($a:ident),+|) => {{
            let b = browser.clone();
            window.$setter(move |$($a),+| b.borrow_mut().$method($($a),+));
        }};
    }

    on!(on_go_back, go_back);
    on!(on_go_up, go_up);
    on!(on_go_home, go_home);
    on!(on_activate_selected, activate_selected);
    on!(on_select_all, select_all);
    on!(on_copy, copy);
    on!(on_cut, cut);
    on!(on_paste, paste);
    on!(on_trash, trash);
    on!(on_toggle_hidden, toggle_hidden);
    on!(on_activate, activate, |idx|);
    on!(on_row_pressed, row_pressed, |idx, ctrl, shift|);
    on!(on_move_cursor, move_cursor, |delta, shift|);
    on!(on_sort_by, set_sort, |key|);
    {
        let b = browser.clone();
        window.on_navigate_to(move |path| b.borrow_mut().navigate_to(path.as_str()));
    }
    {
        let b = browser.clone();
        window.on_search(move |text| b.borrow_mut().set_filter(text.as_str()));
    }
    {
        let b = browser.clone();
        window.on_go_to(move |path| b.borrow_mut().go_to(path.as_str()));
    }
    {
        let b = browser.clone();
        window.on_rename(move |name| b.borrow_mut().rename(name.as_str()));
    }
    {
        let b = browser.clone();
        window.on_new_folder(move |name| b.borrow_mut().new_folder(name.as_str()));
    }

    window.run()
}

#[cfg(test)]
mod screenshot {
    //! Render the browser headlessly with Slint's software renderer and write a
    //! PNG, so the dialog's appearance can be reviewed without a display server.
    //!
    //! (Slint's *testing* backend cannot do this in 1.16: its renderer only does
    //! layout, font metrics and element queries — `Window::take_snapshot()`
    //! returns "not implemented by the platform". The software renderer is the
    //! supported headless rasteriser.)
    use super::*;
    use slint::platform::software_renderer::{
        MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
    };
    use slint::platform::{Platform, WindowAdapter};

    struct SwPlatform {
        window: Rc<MinimalSoftwareWindow>,
    }

    impl Platform for SwPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.window.clone())
        }
    }

    #[test]
    fn render_to_png() {
        let (w, h) = (920u32, 600u32);
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        // `set_platform` must run before any component is created. This is the
        // only window-creating test in the crate, so the global install is safe.
        slint::platform::set_platform(Box::new(SwPlatform {
            window: window.clone(),
        }))
        .expect("install software platform");
        window.set_size(slint::PhysicalSize::new(w, h));

        let dir = std::env::var("SFILES_SHOT_DIR").unwrap_or_else(|_| "/tmp/sfiles-sample".into());
        let out =
            std::env::var("SFILES_SHOT_OUT").unwrap_or_else(|_| "/tmp/sfiles-testing.png".into());

        let view: i32 = std::env::var("SFILES_SHOT_VIEW")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let files = Files::new().unwrap();
        let items = Rc::new(VecModel::<FileItem>::default());
        files.set_items(items.clone().into());
        // A few sidebar entries so the screenshot shows the Places panel.
        let places = Rc::new(VecModel::<Place>::default());
        places.set_vec(vec![
            Place {
                name: "Home".into(),
                path: dir.clone().into(),
            },
            Place {
                name: "Documents".into(),
                path: format!("{dir}/Documents").into(),
            },
            Place {
                name: "Pictures".into(),
                path: format!("{dir}/Pictures").into(),
            },
            Place {
                name: "Filesystem".into(),
                path: "/".into(),
            },
        ]);
        files.set_places(places.into());
        let browser = Rc::new(RefCell::new(Browser::new(files.as_weak(), items)));
        browser.borrow_mut().navigate(PathBuf::from(&dir));
        // Optionally apply a sort column (clicked again = descending).
        if let Some(s) = std::env::var("SFILES_SHOT_SORT")
            .ok()
            .and_then(|v| v.parse::<i32>().ok())
        {
            browser.borrow_mut().set_sort(s);
        }
        if let Ok(f) = std::env::var("SFILES_SHOT_FILTER") {
            browser.borrow_mut().set_filter(&f);
        }
        // Optionally Shift-select rows 0..=N to show multi-selection.
        if let Some(n) = std::env::var("SFILES_SHOT_RANGE")
            .ok()
            .and_then(|v| v.parse::<i32>().ok())
        {
            browser.borrow_mut().row_pressed(0, false, false);
            browser.borrow_mut().row_pressed(n, false, true);
        }
        files.set_view(view);
        files.show().unwrap();
        files.window().request_redraw();

        let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
        let drawn = window.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, w as usize);
        });
        assert!(drawn, "software renderer reported nothing to draw");

        // Un-premultiply into straight RGBA8 for PNG encoding.
        let mut rgba = Vec::with_capacity(buffer.len() * 4);
        for p in &buffer {
            let a = p.alpha;
            let unp = |c: u8| {
                if a == 0 || a == 255 {
                    c
                } else {
                    ((c as u16 * 255) / a as u16) as u8
                }
            };
            rgba.extend_from_slice(&[unp(p.red), unp(p.green), unp(p.blue), a]);
        }
        image::save_buffer(&out, &rgba, w, h, image::ExtendedColorType::Rgba8).expect("encode png");
        eprintln!("wrote {out} ({w}x{h})");

        files.hide().unwrap();
    }
}
