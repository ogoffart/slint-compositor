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

        let files = Files::new().unwrap();
        let items = Rc::new(VecModel::<FileItem>::default());
        files.set_items(items.clone().into());
        let browser = Rc::new(RefCell::new(Browser::new(files.as_weak(), items)));
        browser.borrow_mut().navigate(PathBuf::from(&dir));
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
            let unp = |c: u8| if a == 0 || a == 255 { c } else { ((c as u16 * 255) / a as u16) as u8 };
            rgba.extend_from_slice(&[unp(p.red), unp(p.green), unp(p.blue), a]);
        }
        image::save_buffer(&out, &rgba, w, h, image::ExtendedColorType::Rgba8)
            .expect("encode png");
        eprintln!("wrote {out} ({w}x{h})");

        files.hide().unwrap();
    }
}
