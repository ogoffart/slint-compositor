//! Headless render of the shell UI for visual verification.
//!
//! Uses Slint's software renderer (no GPU / no display needed) to rasterise the
//! `Desktop` scene into PNG files. Run with:
//!
//! ```sh
//! cargo run -p s-compositor --example shell_screenshot
//! ```

use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter, WindowEvent};
use slint::{LogicalPosition, PhysicalSize};

slint::include_modules!();

struct TestPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

fn save(ui: &Desktop, window: &MinimalSoftwareWindow, size: PhysicalSize, name: &str) {
    window.set_size(size);
    // Drive the property/layout passes, then snapshot.
    slint::platform::update_timers_and_animations();
    let buffer = ui
        .window()
        .take_snapshot()
        .expect("software renderer should support take_snapshot");
    // take_snapshot leaves the alpha channel at zero, so drop it and write RGB.
    let rgb: Vec<u8> = buffer
        .as_bytes()
        .chunks_exact(4)
        .flat_map(|px| [px[0], px[1], px[2]])
        .collect();
    image::save_buffer(
        name,
        &rgb,
        buffer.width(),
        buffer.height(),
        image::ColorType::Rgb8,
    )
    .expect("write png");
    println!("wrote {name} ({}x{})", buffer.width(), buffer.height());
}

fn main() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(TestPlatform {
        window: window.clone(),
    }))
    .unwrap();

    let ui = Desktop::new().unwrap();
    ui.show().unwrap();
    ui.set_clock_hours("12".into());
    ui.set_clock_minutes("34".into());

    // A couple of fake taskbar windows.
    let windows = std::rc::Rc::new(slint::VecModel::from(vec![
        WindowTile {
            id: 1,
            title: "Terminal".into(),
            focused: true,
            ..Default::default()
        },
        WindowTile {
            id: 2,
            title: "Editor".into(),
            ..Default::default()
        },
    ]));
    ui.set_windows(windows.into());

    // Point the eyes towards the top-left by moving the pointer there.
    ui.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(60.0, 80.0),
    });

    // 1. Wide top panel: clock on one line.
    ui.set_panel_edge(2); // Top
    ui.set_panel_size(72.0);
    save(
        &ui,
        &window,
        PhysicalSize::new(1100, 720),
        "shot_top_panel.png",
    );

    // 2. Narrow right panel: clock stacks onto two lines.
    ui.set_panel_edge(0); // Right
    ui.set_panel_size(56.0);
    // Look towards the bottom-right this time.
    ui.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(1000.0, 650.0),
    });
    save(
        &ui,
        &window,
        PhysicalSize::new(1100, 720),
        "shot_right_panel.png",
    );

    // 3. Settings dialog open, showing the panel-size SpinBox.
    ui.set_settings_visible(true);
    save(
        &ui,
        &window,
        PhysicalSize::new(1100, 720),
        "shot_settings.png",
    );
}
