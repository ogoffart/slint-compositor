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

/// A simple rounded solid-colour icon, standing in for a real app icon.
fn solid_icon(r: u8, g: u8, b: u8) -> slint::Image {
    let size = 32u32;
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(size, size);
    let w = size as i32;
    let px = buf.make_mut_slice();
    for y in 0..w {
        for x in 0..w {
            // Round the corners a little.
            let corner = 6;
            let inside = !((x < corner && y < corner && (corner - x) + (corner - y) > corner)
                || (x >= w - corner
                    && y < corner
                    && (x - (w - corner - 1)) + (corner - y) > corner)
                || (x < corner
                    && y >= w - corner
                    && (corner - x) + (y - (w - corner - 1)) > corner)
                || (x >= w - corner
                    && y >= w - corner
                    && (x - (w - corner - 1)) + (y - (w - corner - 1)) > corner));
            let i = (y * w + x) as usize;
            px[i] = if inside {
                slint::Rgba8Pixel { r, g, b, a: 255 }
            } else {
                slint::Rgba8Pixel {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0,
                }
            };
        }
    }
    slint::Image::from_rgba8_premultiplied(buf)
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

    // A couple of fake windows, one decorated with geometry and a synthetic icon
    // so the title-bar and taskbar icons are visible.
    let windows = std::rc::Rc::new(slint::VecModel::from(vec![
        WindowTile {
            id: 1,
            title: "Terminal".into(),
            icon: solid_icon(0x89, 0xb4, 0xfa),
            decorated: true,
            focused: true,
            x: 220.0,
            y: 140.0,
            width: 480.0,
            height: 300.0,
            ..Default::default()
        },
        WindowTile {
            id: 2,
            title: "Editor".into(),
            icon: solid_icon(0xa6, 0xe3, 0xa1),
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

    // Snap preview (left half) — shown while dragging a window to an edge.
    ui.set_snap_preview(1);
    save(&ui, &window, PhysicalSize::new(1100, 720), "shot_snap.png");
    ui.set_snap_preview(0);

    // Alt-Tab window switcher.
    ui.set_switcher_windows(
        Rc::new(slint::VecModel::from(vec![
            WindowTile {
                id: 1,
                title: "Terminal".into(),
                icon: solid_icon(0x89, 0xb4, 0xfa),
                focused: true,
                ..Default::default()
            },
            WindowTile {
                id: 2,
                title: "Editor".into(),
                icon: solid_icon(0xa6, 0xe3, 0xa1),
                ..Default::default()
            },
            WindowTile {
                id: 3,
                title: "Browser".into(),
                icon: solid_icon(0xf9, 0xe2, 0xaf),
                ..Default::default()
            },
        ]))
        .into(),
    );
    ui.set_switcher_visible(true);
    save(
        &ui,
        &window,
        PhysicalSize::new(1100, 720),
        "shot_switcher.png",
    );
    ui.set_switcher_visible(false);

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
    ui.set_settings_visible(false);

    // 4. Start menu with some apps and the power actions.
    ui.set_panel_edge(0);
    ui.set_panel_size(72.0);
    let menu = Rc::new(slint::VecModel::from(vec![
        MenuEntry {
            icon: "🌐".into(),
            name: "Firefox".into(),
            command: "firefox".into(),
            kind: "app".into(),
        },
        MenuEntry {
            icon: "🖥".into(),
            name: "Terminal".into(),
            command: "foot".into(),
            kind: "app".into(),
        },
        MenuEntry {
            icon: "🎮".into(),
            name: "SuperTuxKart".into(),
            command: "supertuxkart".into(),
            kind: "app".into(),
        },
    ]));
    ui.set_menu_entries(menu.into());
    ui.set_start_menu_visible(true);
    save(
        &ui,
        &window,
        PhysicalSize::new(1100, 720),
        "shot_start_menu.png",
    );
    ui.set_start_menu_visible(false);

    // App launcher with filtered results.
    ui.set_launcher_results(
        Rc::new(slint::VecModel::from(vec![
            MenuEntry {
                icon: "🌐".into(),
                name: "Firefox".into(),
                command: "firefox".into(),
                kind: "app".into(),
            },
            MenuEntry {
                icon: "🖥".into(),
                name: "Terminal".into(),
                command: "foot".into(),
                kind: "app".into(),
            },
        ]))
        .into(),
    );
    ui.set_launcher_visible(true);
    save(
        &ui,
        &window,
        PhysicalSize::new(1100, 720),
        "shot_launcher.png",
    );
    ui.set_launcher_visible(false);

    // Desktop right-click menu.
    ui.set_desktop_menu_x(360.0);
    ui.set_desktop_menu_y(300.0);
    ui.set_desktop_menu_visible(true);
    save(
        &ui,
        &window,
        PhysicalSize::new(1100, 720),
        "shot_deskmenu.png",
    );
    ui.set_desktop_menu_visible(false);

    // Window title-bar right-click menu.
    ui.set_window_menu_x(300.0);
    ui.set_window_menu_y(170.0);
    ui.set_window_menu_visible(true);
    save(
        &ui,
        &window,
        PhysicalSize::new(1100, 720),
        "shot_winmenu.png",
    );
    ui.set_window_menu_visible(false);

    // Notification center (history + DND + clear).
    ui.set_notif_history(
        Rc::new(slint::VecModel::from(vec![
            Notification {
                id: 3,
                app_name: "Email".into(),
                summary: "New message".into(),
                body: "Alice: lunch at noon?".into(),
                icon: solid_icon(0x89, 0xb4, 0xfa),
            },
            Notification {
                id: 2,
                app_name: "Updates".into(),
                summary: "12 packages can be upgraded".into(),
                body: "Run your package manager to apply them.".into(),
                icon: solid_icon(0xa6, 0xe3, 0xa1),
            },
            Notification {
                id: 1,
                app_name: "s-compositor".into(),
                summary: "Screenshot".into(),
                body: "Saved Screenshot-20260603-120000.png".into(),
                icon: solid_icon(0xcb, 0xa6, 0xf7),
            },
        ]))
        .into(),
    );
    ui.set_notification_center_visible(true);
    save(
        &ui,
        &window,
        PhysicalSize::new(1100, 720),
        "shot_notifcenter.png",
    );
    ui.set_notification_center_visible(false);

    // 5. Quick settings (volume + Wi-Fi) with tray icons in the panel.
    ui.set_tray_icons(
        Rc::new(slint::VecModel::from(vec![
            TrayIcon {
                id: "a".into(),
                title: "Volume".into(),
                icon: solid_icon(0xf9, 0xe2, 0xaf),
            },
            TrayIcon {
                id: "b".into(),
                title: "Network".into(),
                icon: solid_icon(0xcb, 0xa6, 0xf7),
            },
        ]))
        .into(),
    );
    ui.set_volume(65.0);
    ui.set_battery_present(true);
    ui.set_battery_percent(72.0);
    ui.set_battery_charging(true);
    ui.set_wifi_enabled(true);
    ui.set_wifi_networks(
        Rc::new(slint::VecModel::from(vec![
            WifiNetwork {
                ssid: "home-wifi".into(),
                strength: 88,
                secure: true,
                active: true,
            },
            WifiNetwork {
                ssid: "cafe-guest".into(),
                strength: 54,
                secure: false,
                active: false,
            },
            WifiNetwork {
                ssid: "neighbour-5G".into(),
                strength: 31,
                secure: true,
                active: false,
            },
        ]))
        .into(),
    );
    ui.set_quick_settings_visible(true);
    save(&ui, &window, PhysicalSize::new(1100, 720), "shot_quick.png");
    ui.set_quick_settings_visible(false);

    // Volume OSD.
    ui.set_volume(40.0);
    ui.set_osd_visible(true);
    save(&ui, &window, PhysicalSize::new(1100, 720), "shot_osd.png");
    ui.set_osd_visible(false);

    // 6. Notification popups.
    ui.set_notifications(
        Rc::new(slint::VecModel::from(vec![
            Notification {
                id: 1,
                app_name: "Email".into(),
                summary: "New message".into(),
                body: "Alice: lunch at noon?".into(),
                icon: solid_icon(0x89, 0xb4, 0xfa),
            },
            Notification {
                id: 2,
                app_name: "Updates".into(),
                summary: "12 packages can be upgraded".into(),
                body: "Run your package manager to apply them.".into(),
                icon: solid_icon(0xa6, 0xe3, 0xa1),
            },
        ]))
        .into(),
    );
    save(
        &ui,
        &window,
        PhysicalSize::new(1100, 720),
        "shot_notify.png",
    );
    ui.set_notifications(Rc::new(slint::VecModel::default()).into());

    // 7. Calendar popover (June 2026: starts Monday, today = 15).
    let mut days: Vec<CalendarDay> = (1..=30)
        .map(|d| CalendarDay {
            day: d,
            today: d == 15,
        })
        .collect();
    while days.len() < 42 {
        days.push(CalendarDay {
            day: 0,
            today: false,
        });
    }
    ui.set_calendar_title("June 2026".into());
    ui.set_calendar_days(Rc::new(slint::VecModel::from(days)).into());
    ui.set_calendar_visible(true);
    save(
        &ui,
        &window,
        PhysicalSize::new(1100, 720),
        "shot_calendar.png",
    );
    ui.set_calendar_visible(false);

    // 8. Lock screen.
    ui.set_lock_has_password(true);
    ui.set_locked(true);
    save(&ui, &window, PhysicalSize::new(1100, 720), "shot_lock.png");
}
