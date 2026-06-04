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

    // Enter a Tokio runtime (with IO enabled) for the program's life so the
    // zbus calls made by Slint's winit backend have a reactor in scope. See the
    // note in Cargo.toml.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build Tokio runtime");
    let _runtime_guard = runtime.enter();

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
    on!(on_show_properties, show_properties);
    on!(on_drop_on_row, drop_on_row, |idx|);
    on!(on_activate, activate, |idx|);
    on!(on_row_pressed, row_pressed, |idx, ctrl, shift|);
    on!(on_move_cursor, move_cursor, |delta, shift|);
    on!(on_sort_by, set_sort, |key|);
    {
        let b = browser.clone();
        window.on_type_ahead(move |ch| b.borrow_mut().type_ahead(ch.as_str()));
    }
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
        window.on_drop_on_place(move |path| b.borrow_mut().drop_on_place(path.as_str()));
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
mod ui {
    //! Headless UI tests driven by Slint's software renderer (no display server).
    //!
    //! Two things use this: `render_to_png` writes a PNG so the browser's
    //! appearance can be reviewed, and `main_view_scrolls` checks that the file
    //! list actually overflows its viewport (i.e. can scroll).
    //!
    //! (Slint's *testing* backend cannot rasterise in 1.16: its renderer only
    //! does layout, font metrics and element queries — `take_snapshot()` returns
    //! "not implemented by the platform". The software renderer is the supported
    //! headless rasteriser, and it performs the layout these tests rely on.)
    use super::*;
    use slint::platform::software_renderer::{
        MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
    };
    use slint::platform::{Platform, WindowAdapter};
    use std::cell::Cell;
    use std::sync::Mutex;

    thread_local! {
        // Windows the platform hands out, newest last, per (test) thread.
        static WINDOWS: RefCell<Vec<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(Vec::new()) };
    }

    // A platform that creates a fresh software window per component, so several
    // tests can each drive their own window through one process-wide platform.
    struct SwPlatform;
    impl Platform for SwPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
            WINDOWS.with(|ws| ws.borrow_mut().push(window.clone()));
            Ok(window)
        }
    }

    // Slint's platform is installed per thread, and libtest runs each test on
    // its own thread; install `SwPlatform` once per thread. The lock just keeps
    // the window-driving tests from running concurrently.
    static UI_LOCK: Mutex<()> = Mutex::new(());

    fn ensure_platform() {
        thread_local! {
            static INSTALLED: Cell<bool> = const { Cell::new(false) };
        }
        INSTALLED.with(|installed| {
            if !installed.replace(true) {
                let _ = slint::platform::set_platform(Box::new(SwPlatform));
            }
        });
    }

    /// Create a `Files` component and return it alongside its software window.
    fn new_files() -> (Files, Rc<MinimalSoftwareWindow>) {
        ensure_platform();
        let files = Files::new().unwrap();
        let window = WINDOWS.with(|ws| ws.borrow().last().expect("window created").clone());
        (files, window)
    }

    /// Size the window, render one frame, and return the premultiplied buffer.
    fn draw(window: &Rc<MinimalSoftwareWindow>, w: u32, h: u32) -> Vec<PremultipliedRgbaColor> {
        window.set_size(slint::PhysicalSize::new(w, h));
        let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
        let drawn = window.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, w as usize);
        });
        assert!(drawn, "software renderer reported nothing to draw");
        buffer
    }

    /// Filling the list with far more items than fit must make the scroll
    /// viewport overflow the visible area in every view, so the view can scroll.
    /// (Regression test: wrapping each view in an `if` left the ScrollView with
    /// only conditional children, so its viewport collapsed to the visible size.)
    #[test]
    fn main_view_scrolls() {
        let _guard = UI_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (files, window) = new_files();

        let items = Rc::new(VecModel::<FileItem>::default());
        files.set_items(items.clone().into());
        items.set_vec(
            (0..200)
                .map(|i| FileItem {
                    name: format!("item-{i}").into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        );

        let (w, h) = (900u32, 600u32);
        for view in [0, 1, 2] {
            files.set_view(view);
            files.show().unwrap();
            files.window().request_redraw();
            draw(&window, w, h);

            let visible = files.get_scroll_visible_height();
            let viewport = files.get_scroll_viewport_height();
            assert!(
                visible > 0.0,
                "view {view}: visible height should be laid out, got {visible}"
            );
            assert!(
                viewport > visible + 1.0,
                "view {view}: scroll viewport ({viewport}) should overflow the \
                 visible area ({visible}) so the list can scroll",
            );
            files.hide().unwrap();
        }
    }

    #[test]
    fn render_to_png() {
        let _guard = UI_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (w, h) = (920u32, 600u32);
        let (files, window) = new_files();
        window.set_size(slint::PhysicalSize::new(w, h));

        let dir = std::env::var("SFILES_SHOT_DIR").unwrap_or_else(|_| "/tmp/sfiles-sample".into());
        let out =
            std::env::var("SFILES_SHOT_OUT").unwrap_or_else(|_| "/tmp/sfiles-testing.png".into());

        let view: i32 = std::env::var("SFILES_SHOT_VIEW")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

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
        if std::env::var("SFILES_SHOT_PROPS").is_ok() {
            browser.borrow().show_properties();
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

        let buffer = draw(&window, w, h);

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
