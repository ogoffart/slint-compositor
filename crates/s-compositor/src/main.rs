//! s-compositor — a Rust/Slint Wayland desktop shell.
//!
//! Slint owns the main thread (UI + output + input + GL); the Smithay-based
//! Wayland protocol engine runs on its own thread and reports state changes back
//! over a channel which we drain on the UI thread.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::channel;
use std::time::Duration;

use chrono::{Datelike, Local};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

mod calendar;
mod config;
mod file_dialog;
mod gl_bridge;
mod icons;
mod keybind;
mod network;
mod notify;
mod portal;
mod power;
mod session;
mod sysmon;
mod volume;
use gl_bridge::{Frame, GlBridge};

slint::include_modules!();

/// Number of virtual desktops.
const WORKSPACES: u32 = 4;

/// Shared UI-thread state touched by both the event pump and the rendering
/// notifier.
#[derive(Default)]
struct Windows {
    /// window id -> row index in the model.
    rows: HashMap<u64, usize>,
    /// Latest frame per window awaiting GPU upload (drained in the notifier).
    pending: HashMap<u64, Frame>,
    /// Latest dmabuf (GPU buffer) frame per window awaiting EGLImage import.
    pending_dmabuf: HashMap<u64, gl_bridge::DmabufFrame>,
    /// Windows removed since the last frame, whose textures must be freed.
    closed: Vec<u64>,
    /// Newly-mapped windows whose `appearing` flag should be cleared next tick
    /// (so the open animation has one frame to start from the hidden state).
    appear_new: Vec<u64>,
    appear_clear: Vec<u64>,
    /// Windows fading out: id -> when to actually remove them from the model.
    closing_at: HashMap<u64, std::time::Instant>,
    /// The currently focused window.
    focused: Option<u64>,
    /// Pre-maximize geometry (x, y, w, h) to restore on un-maximize.
    restore: HashMap<u64, (f32, f32, f32, f32)>,
    /// popup id -> row index in the popups model.
    popup_rows: HashMap<u64, usize>,
    /// layer-surface id -> row index in the layers model.
    layer_rows: HashMap<u64, usize>,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Make any thread's panic fatal and loud. The compositor runs across several
    // threads (Wayland engine, tray, network…); a panic on one of them would
    // otherwise just kill that thread and leave a half-dead shell limping along
    // (e.g. a dead Wayland thread → "no windows show"). Print the panic, then
    // abort the whole process so it can't go unnoticed.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        let thread = std::thread::current();
        log::error!(
            "fatal: thread '{}' panicked — aborting the compositor",
            thread.name().unwrap_or("<unnamed>")
        );
        std::process::abort();
    }));

    // `system-tray` turns on zbus's `tokio` feature for the whole binary, so the
    // zbus calls made on this (Slint main) thread — notably Slint's winit backend
    // watching the XDG colour scheme — route through `tokio::spawn_blocking` and
    // panic without a Tokio runtime in scope. Enter one for the program's life.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _runtime_guard = runtime.enter();

    // Load persisted settings from the working directory.
    let loaded = config::Config::load();

    // Spawn the Wayland compositor on its own thread, handing it the configured
    // keyboard layouts so the seat is created with them.
    let (tx, rx) = channel::<s_compositor_wayland::Event>();
    let (cmd_tx, cmd_rx) = s_compositor_wayland::command_channel();
    let kb_layouts = loaded.keyboard_layouts.clone();
    std::thread::Builder::new()
        .name("s-compositor-wayland".into())
        .spawn(move || {
            match s_compositor_wayland::run(tx, cmd_rx, kb_layouts) {
                Ok(()) => log::error!("wayland event loop ended unexpectedly"),
                Err(err) => log::error!("wayland thread exited: {err:?}"),
            }
            // The shell is useless without its Wayland engine: a panic aborts via
            // the hook above; a clean/error return lands here. Either way, bring
            // the whole process down instead of leaving a zombie UI.
            std::process::exit(1);
        })?;

    let desktop = Desktop::new()?;

    // Apply the loaded settings to the UI.
    let bg_path: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(loaded.background.clone()));
    apply_config(&desktop, &loaded);

    // Launcher lists — the start menu, desktop shortcut icons and panel
    // quick-launch buttons — are live, editable models (the Settings "Shortcuts"
    // tab mutates them in place). Each uses the configured entries, or seeded
    // defaults when the config specifies none.
    let to_entries = |apps: &[config::AppEntry]| {
        apps.iter()
            .map(|a| MenuEntry {
                icon: a.icon.clone().into(),
                name: a.name.clone().into(),
                command: a.command.clone().into(),
                kind: "app".into(),
            })
            .collect::<Vec<_>>()
    };
    let menu_seed = if loaded.menu.is_empty() {
        config::discover_default_apps()
    } else {
        loaded.menu.clone()
    };
    let desktop_seed = if loaded.desktop.is_empty() {
        config::discover_shortcuts()
    } else {
        loaded.desktop.clone()
    };
    let panel_seed = if loaded.panel_apps.is_empty() {
        vec![config::AppEntry {
            icon: "🗂".to_string(),
            name: "Files".to_string(),
            command: config::file_manager_command(),
        }]
    } else {
        loaded.panel_apps.clone()
    };
    let menu_model = Rc::new(VecModel::from(to_entries(&menu_seed)));
    let desktop_model = Rc::new(VecModel::from(to_entries(&desktop_seed)));
    let panel_model = Rc::new(VecModel::from(to_entries(&panel_seed)));
    desktop.set_menu_entries(ModelRc::from(menu_model.clone()));
    desktop.set_desktop_icons(ModelRc::from(desktop_model.clone()));
    desktop.set_panel_launchers(ModelRc::from(panel_model.clone()));

    // Output (monitor) layout. Seeded with a single default output; replaced by
    // the real layout when the compositor reports it via `Event::Ready`.
    let outputs_model = Rc::new(VecModel::<OutputRect>::from(vec![OutputRect {
        x: 0.0,
        y: 0.0,
        width: 1280.0,
        height: 800.0,
    }]));
    desktop.set_outputs(ModelRc::from(outputs_model.clone()));
    desktop.set_terminal_command(config::terminal_command().into());

    // App launcher: filter the (live) start-menu list as the query changes.
    let launcher_model = Rc::new(VecModel::<MenuEntry>::default());
    desktop.set_launcher_results(ModelRc::from(launcher_model.clone()));
    desktop.on_launcher_query({
        let menu_model = menu_model.clone();
        let launcher_model = launcher_model.clone();
        move |query| {
            let q = query.to_lowercase();
            let results: Vec<MenuEntry> = menu_model
                .iter()
                .filter(|a| {
                    q.is_empty()
                        || a.name.to_lowercase().contains(&q)
                        || a.command.to_lowercase().contains(&q)
                })
                .take(8)
                .collect();
            launcher_model.set_vec(results);
        }
    });

    // Start menu: a searchable view over the same configured app list.
    let start_menu_model = Rc::new(VecModel::<MenuEntry>::default());
    desktop.set_start_menu_entries(ModelRc::from(start_menu_model.clone()));
    desktop.on_start_menu_query({
        let menu_model = menu_model.clone();
        let start_menu_model = start_menu_model.clone();
        move |query| {
            let q = query.to_lowercase();
            let results: Vec<MenuEntry> = menu_model
                .iter()
                .filter(|a| {
                    q.is_empty()
                        || a.name.to_lowercase().contains(&q)
                        || a.command.to_lowercase().contains(&q)
                })
                .collect();
            start_menu_model.set_vec(results);
        }
    });

    // Lock-screen password (empty = unlock on Enter).
    let lock_password: Rc<RefCell<String>> = Rc::new(RefCell::new(loaded.lock_password.clone()));
    desktop.set_lock_has_password(!lock_password.borrow().is_empty());

    // Global keyboard shortcuts: configured bindings, or the defaults.
    let keybinds: Rc<Vec<keybind::Keybind>> = Rc::new(if loaded.keybinds.is_empty() {
        keybind::defaults()
    } else {
        loaded.keybinds.clone()
    });

    // Keyboard-layout switcher: only shown when more than one layout is set. The
    // panel displays the active layout's short code; clicking cycles in the
    // compositor, which reports the new active index back via an event.
    let kb_layout_codes: Rc<Vec<String>> = Rc::new(loaded.keyboard_layouts.clone());
    desktop.set_keyboard_layout_visible(kb_layout_codes.len() > 1);
    if let Some(first) = kb_layout_codes.first() {
        desktop.set_keyboard_layout(first.to_uppercase().into());
    }
    desktop.on_cycle_keyboard_layout({
        let cmd_tx = cmd_tx.clone();
        move || {
            let _ = cmd_tx.send(s_compositor_wayland::Command::CycleKeyboardLayout);
        }
    });

    let model = Rc::new(VecModel::<WindowTile>::default());
    desktop.set_windows(ModelRc::from(model.clone()));
    let popups_model = Rc::new(VecModel::<PopupTile>::default());
    desktop.set_popups(ModelRc::from(popups_model.clone()));
    let layers_model = Rc::new(VecModel::<LayerTile>::default());
    desktop.set_layers(ModelRc::from(layers_model.clone()));
    let windows = Rc::new(RefCell::new(Windows::default()));
    let icon_cache = Rc::new(icons::IconCache::default());
    let bridge = Rc::new(RefCell::new(GlBridge::default()));
    // (WAYLAND_DISPLAY, XDG_RUNTIME_DIR) of s-compositor's compositor, learned from Ready.
    let wayland_env: Rc<RefCell<Option<(String, String)>>> = Rc::new(RefCell::new(None));

    // Launched programs that fail to start are reported here (from a watcher
    // thread, since the failure surfaces only after `/bin/sh` exits) and turned
    // into an on-screen notification on the UI thread.
    let (launch_fail_tx, launch_fail_rx) = std::sync::mpsc::channel::<LaunchError>();

    // Upload client frames into shared GL textures during rendering, where the
    // GL context is current, and hand them to Slint as borrowed textures.
    desktop
        .window()
        .set_rendering_notifier({
            let bridge = bridge.clone();
            let windows = windows.clone();
            let model = model.clone();
            let popups_model = popups_model.clone();
            let layers_model = layers_model.clone();
            move |state, graphics_api| match state {
                slint::RenderingState::RenderingSetup => {
                    if let slint::GraphicsAPI::NativeOpenGL { get_proc_address } = graphics_api {
                        bridge.borrow_mut().init(get_proc_address);
                    }
                }
                slint::RenderingState::BeforeRendering => {
                    let mut bridge = bridge.borrow_mut();
                    if !bridge.ready() {
                        return;
                    }
                    let mut windows = windows.borrow_mut();
                    for id in std::mem::take(&mut windows.closed) {
                        bridge.remove(id);
                    }
                    // GPU (dmabuf) frames: import as EGLImage textures (toplevels
                    // only for now). Best-effort — a failed import keeps the
                    // previous frame.
                    let dmabufs: Vec<(u64, gl_bridge::DmabufFrame)> =
                        windows.pending_dmabuf.drain().collect();
                    for (id, frame) in dmabufs {
                        let Some(image) = bridge.import_dmabuf(id, &frame) else {
                            continue;
                        };
                        if let Some(&row) = windows.rows.get(&id) {
                            if let Some(mut tile) = model.row_data(row) {
                                tile.texture = image;
                                tile.width = frame.width as f32;
                                tile.height = frame.height as f32;
                                model.set_row_data(row, tile);
                            }
                        }
                    }

                    let frames: Vec<(u64, Frame)> = windows.pending.drain().collect();
                    for (id, frame) in frames {
                        let Some(image) = bridge.upload(id, &frame) else {
                            continue;
                        };
                        if let Some(&row) = windows.rows.get(&id) {
                            if let Some(mut tile) = model.row_data(row) {
                                tile.texture = image;
                                tile.width = frame.width as f32;
                                tile.height = frame.height as f32;
                                model.set_row_data(row, tile);
                            }
                        } else if let Some(&row) = windows.popup_rows.get(&id) {
                            if let Some(mut tile) = popups_model.row_data(row) {
                                tile.texture = image;
                                tile.width = frame.width as f32;
                                tile.height = frame.height as f32;
                                popups_model.set_row_data(row, tile);
                            }
                        } else if let Some(&row) = windows.layer_rows.get(&id) {
                            if let Some(mut tile) = layers_model.row_data(row) {
                                tile.texture = image;
                                tile.width = frame.width as f32;
                                tile.height = frame.height as f32;
                                layers_model.set_row_data(row, tile);
                            }
                        }
                    }
                }
                _ => {}
            }
        })
        .unwrap_or_else(|err| log::error!("could not set rendering notifier: {err:?}"));

    // Close button on a window's server-side decoration.
    desktop.on_close_window({
        let cmd_tx = cmd_tx.clone();
        move |id| {
            let _ = cmd_tx.send(s_compositor_wayland::Command::CloseWindow(
                s_compositor_wayland::WindowId(id as u64),
            ));
        }
    });

    // Reusable file dialog (also used by the portal backend).
    let fd_items = Rc::new(VecModel::<FileItem>::default());
    desktop.set_file_dialog_items(ModelRc::from(fd_items.clone()));
    let file_dialog: file_dialog::SharedController = Rc::new(RefCell::new(
        file_dialog::Controller::new(desktop.as_weak(), fd_items),
    ));

    // Expose the file dialog and the appearance settings to other apps via the
    // XDG desktop portal.
    let (portal_tx, portal_rx) = async_channel::unbounded::<portal::Request>();
    let appearance = std::sync::Arc::new(std::sync::Mutex::new(current_appearance(&desktop)));
    let (appearance_tx, appearance_rx) = async_channel::unbounded::<portal::Appearance>();
    portal::spawn(portal_tx, appearance.clone(), appearance_rx);

    desktop.on_change_background({
        let file_dialog = file_dialog.clone();
        let weak = desktop.as_weak();
        let bg_path = bg_path.clone();
        let lock_password = lock_password.clone();
        let keybinds = keybinds.clone();
        let keyboard_layouts = kb_layout_codes.clone();
        move || {
            let weak = weak.clone();
            let bg_path = bg_path.clone();
            let lock_password = lock_password.clone();
            let keybinds = keybinds.clone();
            let keyboard_layouts = keyboard_layouts.clone();
            file_dialog.borrow_mut().open(
                "Select background image",
                file_dialog::home_dir(),
                true,
                Box::new(move |path| {
                    if let (Some(path), Some(d)) = (path, weak.upgrade()) {
                        match slint::Image::load_from_path(&path) {
                            Ok(image) => {
                                d.set_background_image(image);
                                *bg_path.borrow_mut() = Some(path.to_string_lossy().into_owned());
                                current_config(
                                    &d,
                                    &bg_path,
                                    &lock_password,
                                    &keybinds,
                                    &keyboard_layouts,
                                )
                                .save();
                            }
                            Err(err) => log::error!("failed to load image {path:?}: {err}"),
                        }
                    }
                }),
            );
        }
    });

    // Persist settings and publish appearance changes to the portal.
    desktop.on_settings_changed({
        let weak = desktop.as_weak();
        let bg_path = bg_path.clone();
        let appearance_tx = appearance_tx.clone();
        let lock_password = lock_password.clone();
        let keybinds = keybinds.clone();
        let keyboard_layouts = kb_layout_codes.clone();
        move || {
            if let Some(d) = weak.upgrade() {
                current_config(&d, &bg_path, &lock_password, &keybinds, &keyboard_layouts).save();
                let _ = appearance_tx.try_send(current_appearance(&d));
            }
        }
    });

    // Shortcut editing (Settings "Shortcuts" tab). Mutating a model updates the
    // start menu / desktop icons / panel live; each edit re-saves the config.
    desktop.on_shortcut_set({
        let (menu, desk, pan) = (
            menu_model.clone(),
            desktop_model.clone(),
            panel_model.clone(),
        );
        let weak = desktop.as_weak();
        let bg_path = bg_path.clone();
        let lock_password = lock_password.clone();
        let keybinds = keybinds.clone();
        let keyboard_layouts = kb_layout_codes.clone();
        move |list, index, field, value| {
            let model = match list {
                1 => &desk,
                2 => &pan,
                _ => &menu,
            };
            if let Some(mut e) = model.row_data(index as usize) {
                match field {
                    0 => e.icon = value,
                    1 => e.name = value,
                    2 => e.command = value,
                    _ => {}
                }
                model.set_row_data(index as usize, e);
            }
            if let Some(d) = weak.upgrade() {
                current_config(&d, &bg_path, &lock_password, &keybinds, &keyboard_layouts).save();
            }
        }
    });
    desktop.on_shortcut_add({
        let (menu, desk, pan) = (
            menu_model.clone(),
            desktop_model.clone(),
            panel_model.clone(),
        );
        let weak = desktop.as_weak();
        let bg_path = bg_path.clone();
        let lock_password = lock_password.clone();
        let keybinds = keybinds.clone();
        let keyboard_layouts = kb_layout_codes.clone();
        move |list| {
            let model = match list {
                1 => &desk,
                2 => &pan,
                _ => &menu,
            };
            model.push(MenuEntry {
                icon: "📁".into(),
                name: "New".into(),
                command: String::new().into(),
                kind: "app".into(),
            });
            if let Some(d) = weak.upgrade() {
                current_config(&d, &bg_path, &lock_password, &keybinds, &keyboard_layouts).save();
            }
        }
    });
    desktop.on_shortcut_remove({
        let (menu, desk, pan) = (
            menu_model.clone(),
            desktop_model.clone(),
            panel_model.clone(),
        );
        let weak = desktop.as_weak();
        let bg_path = bg_path.clone();
        let lock_password = lock_password.clone();
        let keybinds = keybinds.clone();
        let keyboard_layouts = kb_layout_codes.clone();
        move |list, index| {
            let model = match list {
                1 => &desk,
                2 => &pan,
                _ => &menu,
            };
            let i = index as usize;
            if i < model.row_count() {
                model.remove(i);
            }
            if let Some(d) = weak.upgrade() {
                current_config(&d, &bg_path, &lock_password, &keybinds, &keyboard_layouts).save();
            }
        }
    });

    desktop.on_fd_entry_clicked({
        let file_dialog = file_dialog.clone();
        move |idx| file_dialog.borrow_mut().entry_clicked(idx)
    });
    desktop.on_fd_go_up({
        let file_dialog = file_dialog.clone();
        move || file_dialog.borrow_mut().go_up()
    });
    desktop.on_fd_accept({
        let file_dialog = file_dialog.clone();
        move || file_dialog.borrow_mut().accept()
    });
    desktop.on_fd_cancel({
        let file_dialog = file_dialog.clone();
        move || file_dialog.borrow_mut().cancel()
    });
    desktop.on_fd_navigate_to({
        let file_dialog = file_dialog.clone();
        move |path| file_dialog.borrow_mut().navigate_to(path.as_str())
    });
    desktop.on_fd_move_selection({
        let file_dialog = file_dialog.clone();
        move |delta| file_dialog.borrow_mut().move_selection(delta)
    });
    desktop.on_fd_activate_selected({
        let file_dialog = file_dialog.clone();
        move || file_dialog.borrow_mut().activate_selected()
    });

    // Minimize: hide the window (it stays in the taskbar).
    desktop.on_minimize_window({
        let model = model.clone();
        let windows = windows.clone();
        move |id| {
            let mut windows = windows.borrow_mut();
            if windows.focused == Some(id as u64) {
                windows.focused = None;
            }
            if let Some(&row) = windows.rows.get(&(id as u64)) {
                if let Some(mut tile) = model.row_data(row) {
                    tile.minimized = true;
                    tile.focused = false;
                    model.set_row_data(row, tile);
                }
            }
        }
    });

    // Show desktop: minimize every visible window on the active workspace, and
    // restore exactly those on the next click (classic peek-the-desktop toggle).
    let show_desktop_saved: Rc<RefCell<Vec<u64>>> = Rc::new(RefCell::new(Vec::new()));
    desktop.on_toggle_show_desktop({
        let model = model.clone();
        let windows = windows.clone();
        let weak = desktop.as_weak();
        let saved = show_desktop_saved.clone();
        move || {
            let Some(d) = weak.upgrade() else {
                return;
            };
            let ws = d.get_active_workspace();
            let mut saved = saved.borrow_mut();
            if saved.is_empty() {
                for i in 0..model.row_count() {
                    if let Some(mut tile) = model.row_data(i) {
                        if tile.workspace == ws && !tile.minimized {
                            saved.push(tile.id as u64);
                            tile.minimized = true;
                            tile.focused = false;
                            model.set_row_data(i, tile);
                        }
                    }
                }
                windows.borrow_mut().focused = None;
            } else {
                let ids = std::mem::take(&mut *saved);
                let w = windows.borrow();
                for id in ids {
                    if let Some(&row) = w.rows.get(&id) {
                        if let Some(mut tile) = model.row_data(row) {
                            tile.minimized = false;
                            model.set_row_data(row, tile);
                        }
                    }
                }
            }
        }
    });

    // Maximize / restore: fill the work area (screen minus panel) or restore.
    desktop.on_toggle_maximize_window({
        let cmd_tx = cmd_tx.clone();
        let model = model.clone();
        let windows = windows.clone();
        let weak = desktop.as_weak();
        move |id| {
            let Some(d) = weak.upgrade() else {
                return;
            };
            let currently = windows
                .borrow()
                .rows
                .get(&(id as u64))
                .and_then(|&row| model.row_data(row))
                .map(|t| t.maximized);
            if let Some(maxd) = currently {
                set_maximized(&d, &model, &windows, &cmd_tx, id as u64, !maxd);
            }
        }
    });

    // Always-on-top: toggle a window's pinned flag, then keep pinned windows
    // stacked above all others.
    desktop.on_toggle_window_pinned({
        let model = model.clone();
        let windows = windows.clone();
        move |id| {
            let row = windows.borrow().rows.get(&(id as u64)).copied();
            if let Some(row) = row {
                if let Some(mut tile) = model.row_data(row) {
                    tile.pinned = !tile.pinned;
                    model.set_row_data(row, tile);
                }
            }
            enforce_pinned_order(&model, &windows);
        }
    });

    // Launcher: run an arbitrary command, pointed at our compositor socket.
    desktop.on_launch({
        let wayland_env = wayland_env.clone();
        let launch_fail_tx = launch_fail_tx.clone();
        move |cmd| {
            let env = wayland_env.borrow();
            let env = env.as_ref().map(|(d, r)| (d.as_str(), r.as_str()));
            spawn_command(cmd.as_str(), env, &launch_fail_tx);
        }
    });

    // Start menu: log out quits the compositor; suspend/restart/shutdown go to
    // logind.
    desktop.on_logout(|| {
        log::info!("logout requested; quitting");
        let _ = slint::quit_event_loop();
    });
    desktop.on_suspend(|| session::request(session::PowerAction::Suspend));
    desktop.on_reboot(|| session::request(session::PowerAction::Reboot));
    desktop.on_shutdown(|| session::request(session::PowerAction::PowerOff));

    // Quick settings: volume via PulseAudio/PipeWire (libpulse).
    let (vol_rx, vol_cmd_tx) = match volume::spawn() {
        Some((rx, tx)) => (Some(rx), Some(tx)),
        None => (None, None),
    };
    desktop.on_set_volume({
        let tx = vol_cmd_tx.clone();
        move |v| {
            if let Some(tx) = &tx {
                let _ = tx.send(volume::VolCommand::Set(v));
            }
        }
    });
    desktop.on_toggle_mute({
        let tx = vol_cmd_tx.clone();
        move || {
            if let Some(tx) = &tx {
                let _ = tx.send(volume::VolCommand::ToggleMute);
            }
        }
    });

    // ... and Wi-Fi via NetworkManager.
    let wifi_model = Rc::new(VecModel::<WifiNetwork>::default());
    desktop.set_wifi_networks(ModelRc::from(wifi_model.clone()));
    let (net_rx, net_cmd_tx) = match network::spawn() {
        Some((rx, tx)) => (Some(rx), Some(tx)),
        None => (None, None),
    };
    desktop.on_wifi_scan({
        let tx = net_cmd_tx.clone();
        move || {
            if let Some(tx) = &tx {
                let _ = tx.try_send(network::NetCommand::Scan);
            }
        }
    });
    desktop.on_wifi_toggle({
        let tx = net_cmd_tx.clone();
        move || {
            if let Some(tx) = &tx {
                let _ = tx.try_send(network::NetCommand::Toggle);
            }
        }
    });
    desktop.on_wifi_connect({
        let tx = net_cmd_tx.clone();
        move |ssid, password| {
            if let Some(tx) = &tx {
                let _ = tx.try_send(network::NetCommand::Connect {
                    ssid: ssid.to_string(),
                    password: password.to_string(),
                });
            }
        }
    });

    // System tray (SNI host on its own thread).
    let tray_model = Rc::new(VecModel::<TrayIcon>::default());
    desktop.set_tray_icons(ModelRc::from(tray_model.clone()));
    let tray_ids = Rc::new(RefCell::new(Vec::<String>::new()));
    let (tray_rx, tray_cmd_tx) = match s_compositor_tray::run() {
        Some((rx, tx)) => (Some(rx), Some(tx)),
        None => (None, None),
    };
    desktop.on_tray_activate({
        let tx = tray_cmd_tx.clone();
        move |id| {
            if let Some(tx) = &tx {
                let _ = tx.send(s_compositor_tray::TrayCommand::Activate(id.to_string()));
            }
        }
    });

    // Notification daemon + on-screen popups + history.
    let notif = NotifState::new();
    desktop.set_notifications(ModelRc::from(notif.popups.clone()));
    desktop.set_notif_history(ModelRc::from(notif.history.clone()));
    let notif_rx = notify::spawn();
    desktop.on_dismiss_notification({
        let notif = notif.clone();
        move |id| notif.dismiss(id as u32)
    });
    desktop.on_clear_notifications({
        let notif = notif.clone();
        move || notif.clear_history()
    });
    desktop.on_toggle_dnd({
        let notif = notif.clone();
        let weak = desktop.as_weak();
        move || {
            let on = !notif.dnd.get();
            notif.dnd.set(on);
            if let Some(d) = weak.upgrade() {
                d.set_dnd(on);
            }
        }
    });

    // Battery status via UPower.
    let battery_rx = power::spawn();

    // CPU / memory load for the panel's system monitor.
    let sysmon_rx = sysmon::spawn();

    // Lock screen: unlock when the typed password matches (or none is set).
    desktop.on_unlock({
        let weak = desktop.as_weak();
        let lock_password = lock_password.clone();
        move |entered| {
            let Some(d) = weak.upgrade() else {
                return;
            };
            if entered.as_str() == lock_password.borrow().as_str() {
                d.set_lock_wrong(false);
                d.set_locked(false);
            } else {
                d.set_lock_wrong(true);
            }
        }
    });

    // Raise + focus a window (taskbar click or clicking the window).
    desktop.on_activate_window({
        let cmd_tx = cmd_tx.clone();
        let model = model.clone();
        let windows = windows.clone();
        let weak = desktop.as_weak();
        move |id| {
            raise_and_focus(&model, &windows, id as u64);
            // Follow the window to its workspace if it is on another one.
            if let (Some(d), Some(&row)) = (weak.upgrade(), windows.borrow().rows.get(&(id as u64)))
            {
                if let Some(tile) = model.row_data(row) {
                    d.set_active_workspace(tile.workspace);
                }
            }
            // Clicking a window dismisses any open menus.
            let _ = cmd_tx.send(s_compositor_wayland::Command::DismissPopups);
            let _ = cmd_tx.send(s_compositor_wayland::Command::FocusWindow(
                s_compositor_wayland::WindowId(id as u64),
            ));
        }
    });

    // Clicking the empty desktop dismisses open popups.
    desktop.on_dismiss_popups({
        let cmd_tx = cmd_tx.clone();
        move || {
            let _ = cmd_tx.send(s_compositor_wayland::Command::DismissPopups);
        }
    });

    // Move a window by dragging its title bar.
    // Move a specific window to a workspace (window menu).
    desktop.on_window_move_to_workspace({
        let model = model.clone();
        let windows = windows.clone();
        move |id, ws| {
            let windows = windows.borrow();
            if let Some(&row) = windows.rows.get(&(id as u64)) {
                if let Some(mut tile) = model.row_data(row) {
                    tile.workspace = ws;
                    model.set_row_data(row, tile);
                }
            }
        }
    });

    desktop.on_move_window({
        let model = model.clone();
        let windows = windows.clone();
        move |id, dx, dy| {
            let windows = windows.borrow();
            if let Some(&row) = windows.rows.get(&(id as u64)) {
                if let Some(mut tile) = model.row_data(row) {
                    tile.x += dx;
                    tile.y += dy;
                    model.set_row_data(row, tile);
                }
            }
        }
    });

    // Drop a window: snap it if released against a screen edge (left/right half
    // or top = maximize), GNOME-style.
    desktop.on_drop_window({
        let cmd_tx = cmd_tx.clone();
        let model = model.clone();
        let windows = windows.clone();
        let weak = desktop.as_weak();
        move |id, cx, cy| {
            let Some(d) = weak.upgrade() else {
                return;
            };
            let scale = d.window().scale_factor().max(0.01);
            let size = d.window().size();
            let sw = size.width as f32 / scale;
            let sh = size.height as f32 / scale;
            let edge = 16.0;
            let id = id as u64;
            // Corners snap to a quarter, left/right edges to a half, top edge maximizes.
            if cx <= edge {
                let snap = if cy <= sh * 0.33 {
                    Snap::TopLeft
                } else if cy >= sh * 0.66 {
                    Snap::BottomLeft
                } else {
                    Snap::Left
                };
                snap_window(&d, &model, &windows, &cmd_tx, id, snap);
            } else if cx >= sw - edge {
                let snap = if cy <= sh * 0.33 {
                    Snap::TopRight
                } else if cy >= sh * 0.66 {
                    Snap::BottomRight
                } else {
                    Snap::Right
                };
                snap_window(&d, &model, &windows, &cmd_tx, id, snap);
            } else if cy <= edge {
                set_maximized(&d, &model, &windows, &cmd_tx, id, true);
            }
        }
    });

    // Resize a window via the resize grip (asks the client to reconfigure).
    desktop.on_resize_window({
        let cmd_tx = cmd_tx.clone();
        let model = model.clone();
        let windows = windows.clone();
        move |id, dw, dh| {
            let windows = windows.borrow();
            if let Some(&row) = windows.rows.get(&(id as u64)) {
                if let Some(tile) = model.row_data(row) {
                    let _ = cmd_tx.send(s_compositor_wayland::Command::ResizeWindow {
                        id: s_compositor_wayland::WindowId(id as u64),
                        width: (tile.width + dw).max(1.0) as i32,
                        height: (tile.height + dh).max(1.0) as i32,
                    });
                }
            }
        }
    });

    // Pointer events over a client window.
    desktop.on_pointer_window({
        let cmd_tx = cmd_tx.clone();
        move |id, x, y, kind, btn| {
            let id = s_compositor_wayland::WindowId(id as u64);
            let cmd = match kind {
                1 | 2 => s_compositor_wayland::Command::PointerButton {
                    id,
                    button: evdev_button(btn),
                    pressed: kind == 1,
                },
                _ => s_compositor_wayland::Command::PointerMotion {
                    id,
                    x: x as f64,
                    y: y as f64,
                },
            };
            let _ = cmd_tx.send(cmd);
        }
    });

    desktop.on_scroll_window({
        let cmd_tx = cmd_tx.clone();
        move |id, dx, dy| {
            // Wayland axis is positive-down; Slint scroll delta is positive-up.
            let _ = cmd_tx.send(s_compositor_wayland::Command::PointerAxis {
                id: s_compositor_wayland::WindowId(id as u64),
                dx: -dx as f64,
                dy: -dy as f64,
            });
        }
    });

    // Keyboard: forward each press as a (modifier-wrapped) key tap.
    // Returns true if the key event matched a shortcut (and was consumed).
    // When the volume OSD / Alt-Tab switcher were last shown (drive auto-hide).
    let osd_until: Rc<RefCell<Option<std::time::Instant>>> = Rc::new(RefCell::new(None));
    let switcher_until: Rc<RefCell<Option<std::time::Instant>>> = Rc::new(RefCell::new(None));
    let switcher_model = Rc::new(VecModel::<WindowTile>::default());
    desktop.set_switcher_windows(ModelRc::from(switcher_model.clone()));

    // Scroll over the panel sound button to change volume (and flash the OSD).
    desktop.on_volume_scroll({
        let weak = desktop.as_weak();
        let tx = vol_cmd_tx.clone();
        let osd_until = osd_until.clone();
        move |dy| {
            let Some(d) = weak.upgrade() else {
                return;
            };
            let Some(tx) = &tx else {
                return;
            };
            // Slint wheel delta-y is positive scrolling down; up = louder.
            let step = if dy < 0.0 { 5.0 } else { -5.0 };
            let target = (d.get_volume() + step).clamp(0.0, 100.0);
            let _ = tx.send(volume::VolCommand::Set(target));
            d.set_osd_visible(true);
            *osd_until.borrow_mut() = Some(std::time::Instant::now() + Duration::from_millis(1200));
        }
    });
    let run_binding: Rc<dyn Fn(&str, bool, bool, bool, bool) -> bool> = {
        let keybinds = keybinds.clone();
        let weak = desktop.as_weak();
        let cmd_tx = cmd_tx.clone();
        let model = model.clone();
        let windows = windows.clone();
        let wayland_env = wayland_env.clone();
        let vol_tx = vol_cmd_tx.clone();
        let notif = notif.clone();
        let osd_until = osd_until.clone();
        let switcher_until = switcher_until.clone();
        let switcher_model = switcher_model.clone();
        let launch_fail_tx = launch_fail_tx.clone();
        Rc::new(move |text: &str, ctrl, alt, shift, meta| {
            let key = keybind::normalize_text(text);
            let Some(bind) = keybinds
                .iter()
                .find(|b| b.matches(&key, ctrl, alt, shift, meta))
            else {
                return false;
            };
            if let Some(d) = weak.upgrade() {
                run_action(
                    &bind.action,
                    &d,
                    &cmd_tx,
                    &model,
                    &windows,
                    &wayland_env,
                    &vol_tx,
                    &notif,
                    &launch_fail_tx,
                );
                // Flash the volume OSD on a volume key.
                if matches!(
                    bind.action,
                    keybind::Action::VolumeUp
                        | keybind::Action::VolumeDown
                        | keybind::Action::VolumeMute
                ) {
                    d.set_osd_visible(true);
                    *osd_until.borrow_mut() =
                        Some(std::time::Instant::now() + Duration::from_millis(1200));
                }
                // Flash the Alt-Tab switcher when cycling windows.
                if matches!(
                    bind.action,
                    keybind::Action::NextWindow | keybind::Action::PrevWindow
                ) {
                    let ws = d.get_active_workspace();
                    let items: Vec<WindowTile> = (0..model.row_count())
                        .filter_map(|i| model.row_data(i))
                        .filter(|t| t.workspace == ws && !t.minimized)
                        .collect();
                    switcher_model.set_vec(items);
                    d.set_switcher_visible(true);
                    *switcher_until.borrow_mut() =
                        Some(std::time::Instant::now() + Duration::from_millis(1200));
                }
            }
            true
        })
    };

    // Keys for the focused client: intercept shortcuts, else forward.
    desktop.on_key_window({
        let cmd_tx = cmd_tx.clone();
        let run_binding = run_binding.clone();
        move |_id, text, pressed, ctrl, alt, shift, meta| {
            if pressed && run_binding(text.as_str(), ctrl, alt, shift, meta) {
                return;
            }
            forward_key(&cmd_tx, text.as_str(), ctrl, alt, shift);
        }
    });

    // Keys when no client is focused (root focus scope): shortcuts only.
    desktop.on_key_shortcut({
        let run_binding = run_binding.clone();
        move |text, pressed, ctrl, alt, shift, meta| {
            if pressed {
                run_binding(text.as_str(), ctrl, alt, shift, meta);
            }
        }
    });

    // Clock + calendar: refresh once a second.
    let calendar_model = Rc::new(VecModel::<CalendarDay>::default());
    desktop.set_calendar_days(ModelRc::from(calendar_model.clone()));
    let update_clock = {
        let weak = desktop.as_weak();
        let calendar_model = calendar_model.clone();
        let last_day = RefCell::new(0u32);
        move || {
            if let Some(d) = weak.upgrade() {
                let now = Local::now();
                let (hours, minutes) = s_compositor_shell::format_clock(now);
                d.set_clock_hours(hours.into());
                d.set_clock_minutes(minutes.into());
                // Rebuild the month grid only when the day changes.
                if *last_day.borrow() != now.day() {
                    *last_day.borrow_mut() = now.day();
                    let (title, cells) = calendar::month_grid(now);
                    d.set_calendar_title(title.into());
                    calendar_model.set_vec(
                        cells
                            .into_iter()
                            .map(|c| CalendarDay {
                                day: c.day as i32,
                                today: c.today,
                            })
                            .collect::<Vec<_>>(),
                    );
                }
            }
        }
    };
    update_clock();
    let clock_timer = slint::Timer::default();
    clock_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(1),
        update_clock,
    );

    // Drain compositor events on the UI thread.
    let event_timer = slint::Timer::default();
    event_timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), {
        let weak = desktop.as_weak();
        let windows = windows.clone();
        let model = model.clone();
        let popups_model = popups_model.clone();
        let layers_model = layers_model.clone();
        let wayland_env = wayland_env.clone();
        let outputs_model = outputs_model.clone();
        let file_dialog = file_dialog.clone();
        let cmd_tx = cmd_tx.clone();
        let icon_cache = icon_cache.clone();
        let tray_model = tray_model.clone();
        let tray_ids = tray_ids.clone();
        let wifi_model = wifi_model.clone();
        let notif = notif.clone();
        let osd_until = osd_until.clone();
        let switcher_until = switcher_until.clone();
        let kb_layout_codes = kb_layout_codes.clone();
        let launch_fail_rx = launch_fail_rx;
        let mut battery_low_warned = false;
        // When a single output is advertised (the common nested/X11 case) the
        // output tracks the host window size; remember the last applied logical
        // size and whether we're in that single-output mode.
        let mut single_output = true;
        let mut last_screen: Option<(f32, f32)> = None;
        move || {
            let mut dirty = false;

            // Auto-hide the volume OSD once its window elapses.
            let osd_expired = osd_until
                .borrow()
                .is_some_and(|t| std::time::Instant::now() >= t);
            if osd_expired {
                *osd_until.borrow_mut() = None;
                if let Some(d) = weak.upgrade() {
                    d.set_osd_visible(false);
                    dirty = true;
                }
            }
            // Auto-hide the Alt-Tab switcher.
            let switcher_expired = switcher_until
                .borrow()
                .is_some_and(|t| std::time::Instant::now() >= t);
            if switcher_expired {
                *switcher_until.borrow_mut() = None;
                if let Some(d) = weak.upgrade() {
                    d.set_switcher_visible(false);
                    dirty = true;
                }
            }

            // Drain notifications, and expire timed-out ones.
            if let Some(rx) = &notif_rx {
                while let Ok(event) = rx.try_recv() {
                    notif.apply(event);
                    dirty = true;
                }
            }
            if notif.expire() {
                dirty = true;
            }

            // Surface programs that failed to launch as a notification.
            while let Ok(err) = launch_fail_rx.try_recv() {
                notif.internal(
                    "Couldn't open application",
                    &format!("{} — {}", err.cmd, err.reason),
                );
                dirty = true;
            }

            // Drain SNI tray updates.
            if let Some(rx) = &tray_rx {
                while let Ok(update) = rx.try_recv() {
                    apply_tray_update(update, &tray_model, &tray_ids);
                    dirty = true;
                }
            }

            // Drain NetworkManager updates.
            if let Some(rx) = &net_rx {
                while let Ok(event) = rx.try_recv() {
                    if let Some(d) = weak.upgrade() {
                        apply_net_event(event, &d, &wifi_model);
                        dirty = true;
                    }
                }
            }

            // Drain volume updates.
            if let Some(rx) = &vol_rx {
                while let Ok(event) = rx.try_recv() {
                    if let Some(d) = weak.upgrade() {
                        d.set_volume(event.volume);
                        d.set_muted(event.muted);
                        dirty = true;
                    }
                }
            }

            // Drain battery updates, and warn once when it gets low.
            if let Some(rx) = &battery_rx {
                while let Ok(b) = rx.try_recv() {
                    if let Some(d) = weak.upgrade() {
                        d.set_battery_present(b.present);
                        d.set_battery_percent(b.percent as f32);
                        d.set_battery_charging(b.charging);
                        dirty = true;
                    }
                    if b.present && !b.charging && b.percent <= 15.0 {
                        if !battery_low_warned {
                            battery_low_warned = true;
                            notif.internal(
                                "Battery low",
                                &format!("{}% remaining", b.percent.round() as i32),
                            );
                            dirty = true;
                        }
                    } else if b.charging || b.percent > 20.0 {
                        battery_low_warned = false;
                    }
                }
            }
            // Drain system-load samples.
            if let Some(rx) = &sysmon_rx {
                while let Ok(load) = rx.try_recv() {
                    if let Some(d) = weak.upgrade() {
                        d.set_cpu_load(load.cpu);
                        d.set_mem_load(load.mem);
                        dirty = true;
                    }
                }
            }
            let active_ws = weak
                .upgrade()
                .map(|d| d.get_active_workspace())
                .unwrap_or(0);
            while let Ok(event) = rx.try_recv() {
                // The compositor reported its output layout: size the window to the
                // bounding box of all monitors and publish the per-output rects.
                if let s_compositor_wayland::Event::Ready {
                    socket_name,
                    runtime_dir,
                    outputs,
                } = &event
                {
                    log::info!(
                        "compositor ready on WAYLAND_DISPLAY={socket_name} \
                         (XDG_RUNTIME_DIR={runtime_dir}, {} output(s))",
                        outputs.len()
                    );
                    *wayland_env.borrow_mut() = Some((socket_name.clone(), runtime_dir.clone()));
                    // A single advertised output follows the host window size
                    // (resize the window = resize the screen). A multi-monitor
                    // layout is fixed, so leave it as configured.
                    single_output = outputs.len() <= 1;
                    last_screen = None;
                    let bw = outputs
                        .iter()
                        .map(|o| o.x + o.w)
                        .max()
                        .unwrap_or(1280)
                        .max(1);
                    let bh = outputs
                        .iter()
                        .map(|o| o.y + o.h)
                        .max()
                        .unwrap_or(800)
                        .max(1);
                    outputs_model.set_vec(
                        outputs
                            .iter()
                            .map(|o| OutputRect {
                                x: o.x as f32,
                                y: o.y as f32,
                                width: o.w as f32,
                                height: o.h as f32,
                            })
                            .collect::<Vec<_>>(),
                    );
                    if let Some(d) = weak.upgrade() {
                        d.window()
                            .set_size(slint::PhysicalSize::new(bw as u32, bh as u32));
                    }
                    dirty = true;
                    continue;
                }
                // The active keyboard layout changed: show its short code in the
                // panel switcher.
                if let s_compositor_wayland::Event::KeyboardLayout(idx) = &event {
                    if let Some(d) = weak.upgrade() {
                        if let Some(code) = kb_layout_codes.get(*idx) {
                            d.set_keyboard_layout(code.to_uppercase().into());
                            dirty = true;
                        }
                    }
                    continue;
                }
                // Client-initiated (un)maximize reuses the work-area logic so it
                // never covers the panel.
                if let s_compositor_wayland::Event::WindowMaximizeRequested { id, maximized } =
                    &event
                {
                    if let Some(d) = weak.upgrade() {
                        set_maximized(&d, &model, &windows, &cmd_tx, id.0, *maximized);
                        dirty = true;
                    }
                    continue;
                }
                // A client (typically client-side-decorated) asked to be moved:
                // hand the window to the UI's interactive-move path, which follows
                // the pointer until the button is released.
                if let s_compositor_wayland::Event::WindowMoveRequested { id } = &event {
                    if let Some(d) = weak.upgrade() {
                        d.set_interactive_move_id(id.0 as i32);
                        d.invoke_activate_window(id.0 as i32);
                        dirty = true;
                    }
                    continue;
                }
                dirty |= handle_event(
                    event,
                    &model,
                    &popups_model,
                    &layers_model,
                    &windows,
                    &wayland_env,
                    &icon_cache,
                    active_ws,
                );
            }
            // Follow the host window size: when a single output is advertised,
            // resizing the window (e.g. on X11) resizes the screen so the
            // desktop fills it, and maximized windows re-fit the new work area.
            if single_output {
                if let Some(d) = weak.upgrade() {
                    let scale = d.window().scale_factor().max(0.01);
                    let size = d.window().size();
                    let lw = (size.width as f32 / scale).round();
                    let lh = (size.height as f32 / scale).round();
                    if lw >= 1.0 && lh >= 1.0 && last_screen != Some((lw, lh)) {
                        last_screen = Some((lw, lh));
                        outputs_model.set_row_data(
                            0,
                            OutputRect {
                                x: 0.0,
                                y: 0.0,
                                width: lw,
                                height: lh,
                            },
                        );
                        let _ = cmd_tx.send(s_compositor_wayland::Command::ResizeOutput {
                            width: lw as i32,
                            height: lh as i32,
                        });
                        refit_maximized(&d, &model, &cmd_tx);
                        dirty = true;
                    }
                }
            }

            // Drive window open/close animations (clear `appearing`, remove
            // windows whose fade-out has finished).
            if process_window_anims(&model, &windows) {
                dirty = true;
            }
            // Serve pending portal file-open requests with the same dialog.
            while let Ok(request) = portal_rx.try_recv() {
                let reply = request.reply;
                file_dialog.borrow_mut().open(
                    &request.title,
                    file_dialog::home_dir(),
                    false,
                    Box::new(move |path| {
                        let _ = reply.try_send(path);
                    }),
                );
            }
            if dirty {
                if let Some(d) = weak.upgrade() {
                    d.window().request_redraw();
                }
            }
        }
    });

    desktop.run()?;
    Ok(())
}

/// Apply a compositor event. Returns true if a redraw is needed.
#[allow(clippy::too_many_arguments)]
fn handle_event(
    event: s_compositor_wayland::Event,
    model: &Rc<VecModel<WindowTile>>,
    popups_model: &Rc<VecModel<PopupTile>>,
    layers_model: &Rc<VecModel<LayerTile>>,
    windows: &Rc<RefCell<Windows>>,
    _wayland_env: &Rc<RefCell<Option<(String, String)>>>,
    icon_cache: &Rc<icons::IconCache>,
    active_workspace: i32,
) -> bool {
    use s_compositor_wayland::Event;
    match event {
        // `Ready` is intercepted at the call site (it needs the window handle to
        // size the canvas to the output layout), so it never reaches here.
        Event::Ready { .. } => false,
        Event::WindowDmabuf {
            id,
            width,
            height,
            fourcc,
            modifier,
            planes,
        } => {
            // Single-plane import for now; queue it for the rendering notifier.
            if let Some(plane) = planes.into_iter().next() {
                windows.borrow_mut().pending_dmabuf.insert(
                    id.0,
                    gl_bridge::DmabufFrame {
                        width,
                        height,
                        fourcc,
                        modifier,
                        fd: plane.fd,
                        offset: plane.offset,
                        stride: plane.stride,
                    },
                );
                true
            } else {
                false
            }
        }
        Event::WindowBuffer {
            id,
            width,
            height,
            pixels,
            title,
            app_id,
            decorated,
        } => {
            let mut windows = windows.borrow_mut();
            if let Some(&row) = windows.rows.get(&id.0) {
                if let Some(mut tile) = model.row_data(row) {
                    tile.title = title.into();
                    tile.decorated = decorated;
                    model.set_row_data(row, tile);
                }
            } else {
                let row = model.row_count();
                let offset = 40.0 + row as f32 * 40.0;
                model.push(WindowTile {
                    id: id.0 as i32,
                    texture: slint::Image::default(),
                    icon: icon_cache.get(&app_id),
                    title: title.into(),
                    decorated,
                    focused: false,
                    minimized: false,
                    maximized: false,
                    pinned: false,
                    appearing: true,
                    closing: false,
                    workspace: active_workspace,
                    x: offset,
                    y: offset,
                    width: width as f32,
                    height: height as f32,
                });
                windows.rows.insert(id.0, row);
                windows.appear_new.push(id.0);
            }
            windows.pending.insert(
                id.0,
                Frame {
                    width,
                    height,
                    pixels,
                },
            );
            true
        }
        Event::WindowDecorated { id, decorated } => {
            let windows = windows.borrow();
            if let Some(&row) = windows.rows.get(&id.0) {
                if let Some(mut tile) = model.row_data(row) {
                    tile.decorated = decorated;
                    model.set_row_data(row, tile);
                }
            }
            true
        }
        Event::WindowRemoved(id) => {
            let mut windows = windows.borrow_mut();
            windows.pending.remove(&id.0);
            // Fade the window out, then remove it from the model after ~200ms
            // (handled in the event loop). The last frame stays on screen.
            if let Some(&row) = windows.rows.get(&id.0) {
                if let Some(mut tile) = model.row_data(row) {
                    tile.closing = true;
                    model.set_row_data(row, tile);
                }
                windows
                    .closing_at
                    .insert(id.0, std::time::Instant::now() + Duration::from_millis(200));
            }
            true
        }
        Event::PopupBuffer {
            id,
            parent,
            ox,
            oy,
            width,
            height,
            pixels,
        } => {
            let mut windows = windows.borrow_mut();
            let (px, py) = popup_origin(model, popups_model, &windows, parent);
            let (x, y) = (px + ox as f32, py + oy as f32);
            if let Some(&row) = windows.popup_rows.get(&id.0) {
                if let Some(mut tile) = popups_model.row_data(row) {
                    tile.x = x;
                    tile.y = y;
                    tile.width = width as f32;
                    tile.height = height as f32;
                    popups_model.set_row_data(row, tile);
                }
            } else {
                let row = popups_model.row_count();
                popups_model.push(PopupTile {
                    id: id.0 as i32,
                    texture: slint::Image::default(),
                    x,
                    y,
                    width: width as f32,
                    height: height as f32,
                });
                windows.popup_rows.insert(id.0, row);
            }
            windows.pending.insert(
                id.0,
                Frame {
                    width,
                    height,
                    pixels,
                },
            );
            true
        }
        Event::PopupRemoved(id) => {
            let mut windows = windows.borrow_mut();
            windows.pending.remove(&id.0);
            if let Some(removed) = windows.popup_rows.remove(&id.0) {
                popups_model.remove(removed);
                for row in windows.popup_rows.values_mut() {
                    if *row > removed {
                        *row -= 1;
                    }
                }
                windows.closed.push(id.0);
            }
            true
        }
        Event::LayerBuffer {
            id,
            layer,
            x,
            y,
            width,
            height,
            pixels,
        } => {
            let mut windows = windows.borrow_mut();
            if let Some(&row) = windows.layer_rows.get(&id.0) {
                if let Some(mut tile) = layers_model.row_data(row) {
                    tile.x = x as f32;
                    tile.y = y as f32;
                    tile.width = width as f32;
                    tile.height = height as f32;
                    layers_model.set_row_data(row, tile);
                }
            } else {
                let row = layers_model.row_count();
                layers_model.push(LayerTile {
                    id: id.0 as i32,
                    texture: slint::Image::default(),
                    layer: layer as i32,
                    x: x as f32,
                    y: y as f32,
                    width: width as f32,
                    height: height as f32,
                });
                windows.layer_rows.insert(id.0, row);
            }
            windows.pending.insert(
                id.0,
                Frame {
                    width,
                    height,
                    pixels,
                },
            );
            true
        }
        Event::LayerRemoved(id) => {
            let mut windows = windows.borrow_mut();
            windows.pending.remove(&id.0);
            if let Some(removed) = windows.layer_rows.remove(&id.0) {
                layers_model.remove(removed);
                for row in windows.layer_rows.values_mut() {
                    if *row > removed {
                        *row -= 1;
                    }
                }
                windows.closed.push(id.0);
            }
            true
        }
        Event::XwaylandReady { display } => {
            // X11 apps launched from now on inherit this (see spawn_command).
            std::env::set_var("S_COMPOSITOR_XDISPLAY", format!(":{display}"));
            log::info!("XWayland ready: DISPLAY=:{display}");
            false
        }
        other => {
            log::info!("compositor event: {other:?}");
            false
        }
    }
}

/// The on-screen origin a popup is positioned against: its parent window's
/// content origin (below the title bar) or its parent popup's origin.
fn popup_origin(
    model: &Rc<VecModel<WindowTile>>,
    popups_model: &Rc<VecModel<PopupTile>>,
    windows: &Windows,
    parent: Option<s_compositor_wayland::WindowId>,
) -> (f32, f32) {
    let Some(parent) = parent else {
        return (0.0, 0.0);
    };
    if let Some(&row) = windows.rows.get(&parent.0) {
        if let Some(tile) = model.row_data(row) {
            let titlebar = if tile.decorated { 28.0 } else { 0.0 };
            return (tile.x, tile.y + titlebar);
        }
    }
    if let Some(&row) = windows.popup_rows.get(&parent.0) {
        if let Some(tile) = popups_model.row_data(row) {
            return (tile.x, tile.y);
        }
    }
    (0.0, 0.0)
}

/// Raise a window to the top of the stack and mark it focused. Rebuilds the
/// id->row map from the resulting model order.
/// Move the focused window to a workspace (it disappears from the current one).
fn move_focused_to_workspace(
    model: &Rc<VecModel<WindowTile>>,
    windows: &Rc<RefCell<Windows>>,
    workspace: i32,
) {
    let row = {
        let w = windows.borrow();
        w.focused.and_then(|id| w.rows.get(&id).copied())
    };
    if let Some(row) = row {
        if let Some(mut tile) = model.row_data(row) {
            tile.workspace = workspace;
            model.set_row_data(row, tile);
        }
    }
}

fn raise_and_focus(model: &Rc<VecModel<WindowTile>>, windows: &Rc<RefCell<Windows>>, id: u64) {
    {
        let mut w = windows.borrow_mut();
        w.focused = Some(id);
        if let Some(&row) = w.rows.get(&id) {
            if row + 1 != model.row_count() {
                if let Some(tile) = model.row_data(row) {
                    model.remove(row);
                    model.push(tile);
                }
            }
        }
        for i in 0..model.row_count() {
            if let Some(mut tile) = model.row_data(i) {
                w.rows.insert(tile.id as u64, i);
                let want = tile.id as u64 == id;
                let unminimize = want && tile.minimized;
                if tile.focused != want || unminimize {
                    tile.focused = want;
                    if unminimize {
                        tile.minimized = false;
                    }
                    model.set_row_data(i, tile);
                }
            }
        }
    }
    // Keep always-on-top windows above the window we just raised.
    enforce_pinned_order(model, windows);
}

/// Keep pinned (always-on-top) windows above all non-pinned ones. The model is
/// painted in order, so the topmost window is last; pinned windows are moved to
/// the tail, preserving relative order within each group.
fn enforce_pinned_order(model: &Rc<VecModel<WindowTile>>, windows: &Rc<RefCell<Windows>>) {
    let n = model.row_count();
    let tiles: Vec<WindowTile> = (0..n).filter_map(|i| model.row_data(i)).collect();
    if tiles.len() != n || !tiles.iter().any(|t| t.pinned) {
        return;
    }
    let mut ordered: Vec<WindowTile> = tiles.iter().filter(|t| !t.pinned).cloned().collect();
    ordered.extend(tiles.iter().filter(|t| t.pinned).cloned());
    if ordered.iter().zip(tiles.iter()).all(|(a, b)| a.id == b.id) {
        return; // already in order
    }
    model.set_vec(ordered);
    let mut w = windows.borrow_mut();
    for i in 0..model.row_count() {
        if let Some(t) = model.row_data(i) {
            w.rows.insert(t.id as u64, i);
        }
    }
}

/// Focus the next/previous window on the active workspace (Alt-Tab).
fn cycle_focus(
    d: &Desktop,
    model: &Rc<VecModel<WindowTile>>,
    windows: &Rc<RefCell<Windows>>,
    cmd_tx: &s_compositor_wayland::CommandSender<s_compositor_wayland::Command>,
    forward: bool,
) {
    let ws = d.get_active_workspace();
    let mut ids: Vec<u64> = Vec::new();
    for i in 0..model.row_count() {
        if let Some(t) = model.row_data(i) {
            if t.workspace == ws && !t.minimized {
                ids.push(t.id as u64);
            }
        }
    }
    if ids.is_empty() {
        return;
    }
    let focused = windows.borrow().focused;
    let cur = focused.and_then(|f| ids.iter().position(|&x| x == f));
    let next = match cur {
        Some(i) if forward => (i + 1) % ids.len(),
        Some(i) => (i + ids.len() - 1) % ids.len(),
        None => 0,
    };
    let id = ids[next];
    raise_and_focus(model, windows, id);
    let _ = cmd_tx.send(s_compositor_wayland::Command::FocusWindow(
        s_compositor_wayland::WindowId(id),
    ));
}

/// Which half/region of the work area to snap a window to.
#[derive(Clone, Copy)]
enum Snap {
    Left,
    Right,
    Up,
    Down,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// Snap the focused window to a half of the work area (keyboard tiling).
fn snap_focused(
    d: &Desktop,
    model: &Rc<VecModel<WindowTile>>,
    windows: &Rc<RefCell<Windows>>,
    cmd_tx: &s_compositor_wayland::CommandSender<s_compositor_wayland::Command>,
    snap: Snap,
) {
    let focused = windows.borrow().focused;
    if let Some(id) = focused {
        snap_window(d, model, windows, cmd_tx, id, snap);
    }
}

/// Snap a specific window to a half of the work area.
fn snap_window(
    d: &Desktop,
    model: &Rc<VecModel<WindowTile>>,
    windows: &Rc<RefCell<Windows>>,
    cmd_tx: &s_compositor_wayland::CommandSender<s_compositor_wayland::Command>,
    id: u64,
    snap: Snap,
) {
    let mut w = windows.borrow_mut();
    let Some(&row) = w.rows.get(&id) else {
        return;
    };
    let Some(mut tile) = model.row_data(row) else {
        return;
    };
    let (wx, wy, ww, wh) = work_area(d, tile.x + tile.width / 2.0, tile.y + tile.height / 2.0);
    let (x, y, width, height) = match snap {
        Snap::Left => (wx, wy, ww / 2.0, wh),
        Snap::Right => (wx + ww / 2.0, wy, ww / 2.0, wh),
        Snap::Up => (wx, wy, ww, wh / 2.0),
        Snap::Down => (wx, wy + wh / 2.0, ww, wh / 2.0),
        Snap::TopLeft => (wx, wy, ww / 2.0, wh / 2.0),
        Snap::TopRight => (wx + ww / 2.0, wy, ww / 2.0, wh / 2.0),
        Snap::BottomLeft => (wx, wy + wh / 2.0, ww / 2.0, wh / 2.0),
        Snap::BottomRight => (wx + ww / 2.0, wy + wh / 2.0, ww / 2.0, wh / 2.0),
    };
    let titlebar = if tile.decorated { 28.0 } else { 0.0 };
    tile.x = x;
    tile.y = y;
    tile.width = width;
    tile.height = (height - titlebar).max(1.0);
    tile.maximized = false;
    w.restore.remove(&id);
    let _ = cmd_tx.send(s_compositor_wayland::Command::ResizeWindow {
        id: s_compositor_wayland::WindowId(id),
        width: tile.width as i32,
        height: tile.height as i32,
    });
    model.set_row_data(row, tile);
}

/// Run a keybinding action against the shell.
#[allow(clippy::too_many_arguments)]
fn run_action(
    action: &keybind::Action,
    d: &Desktop,
    cmd_tx: &s_compositor_wayland::CommandSender<s_compositor_wayland::Command>,
    model: &Rc<VecModel<WindowTile>>,
    windows: &Rc<RefCell<Windows>>,
    wayland_env: &Rc<RefCell<Option<(String, String)>>>,
    vol_tx: &Option<std::sync::mpsc::Sender<volume::VolCommand>>,
    notif: &NotifState,
    fail_tx: &std::sync::mpsc::Sender<LaunchError>,
) {
    use keybind::Action;
    let workspaces = WORKSPACES as i32;
    match action {
        Action::Spawn(cmd) => {
            let env = wayland_env.borrow();
            let env = env
                .as_ref()
                .map(|(disp, run)| (disp.as_str(), run.as_str()));
            spawn_command(cmd, env, fail_tx);
        }
        Action::StartMenu => d.set_start_menu_visible(!d.get_start_menu_visible()),
        Action::Launcher => d.set_launcher_visible(true),
        Action::Settings => d.set_settings_visible(true),
        Action::QuickSettings => d.set_quick_settings_visible(!d.get_quick_settings_visible()),
        Action::Lock => {
            d.set_lock_wrong(false);
            d.set_locked(true);
        }
        Action::Logout => {
            let _ = slint::quit_event_loop();
        }
        Action::CloseWindow => {
            if let Some(id) = windows.borrow().focused {
                let _ = cmd_tx.send(s_compositor_wayland::Command::CloseWindow(
                    s_compositor_wayland::WindowId(id),
                ));
            }
        }
        Action::NextWindow => cycle_focus(d, model, windows, cmd_tx, true),
        Action::PrevWindow => cycle_focus(d, model, windows, cmd_tx, false),
        Action::Workspace(n) => d.set_active_workspace(((*n as i32) - 1).clamp(0, workspaces - 1)),
        Action::MoveToWorkspace(n) => {
            move_focused_to_workspace(model, windows, ((*n as i32) - 1).clamp(0, workspaces - 1))
        }
        Action::WorkspaceNext => {
            d.set_active_workspace((d.get_active_workspace() + 1).rem_euclid(workspaces))
        }
        Action::WorkspacePrev => {
            d.set_active_workspace((d.get_active_workspace() - 1).rem_euclid(workspaces))
        }
        Action::VolumeUp | Action::VolumeDown => {
            if let Some(tx) = vol_tx {
                let delta = if matches!(action, Action::VolumeUp) {
                    5.0
                } else {
                    -5.0
                };
                let target = (d.get_volume() + delta).clamp(0.0, 100.0);
                let _ = tx.send(volume::VolCommand::Set(target));
            }
        }
        Action::VolumeMute => {
            if let Some(tx) = vol_tx {
                let _ = tx.send(volume::VolCommand::ToggleMute);
            }
        }
        Action::SnapLeft => snap_focused(d, model, windows, cmd_tx, Snap::Left),
        Action::SnapRight => snap_focused(d, model, windows, cmd_tx, Snap::Right),
        Action::SnapUp => snap_focused(d, model, windows, cmd_tx, Snap::Up),
        Action::SnapDown => snap_focused(d, model, windows, cmd_tx, Snap::Down),
        Action::Maximize => {
            let cur = {
                let w = windows.borrow();
                w.focused
                    .and_then(|id| w.rows.get(&id).copied())
                    .and_then(|row| model.row_data(row))
                    .map(|t| (t.id as u64, t.maximized))
            };
            if let Some((id, maxed)) = cur {
                set_maximized(d, model, windows, cmd_tx, id, !maxed);
            }
        }
        Action::Screenshot => take_screenshot(d, notif),
    }
}

/// Capture the whole screen to a PNG in the user's Pictures directory and show a
/// notification with the path.
fn take_screenshot(d: &Desktop, notif: &NotifState) {
    let buffer = match d.window().take_snapshot() {
        Ok(b) => b,
        Err(err) => {
            log::warn!("screenshot: take_snapshot failed: {err}");
            return;
        }
    };
    let dir = {
        let pics = file_dialog::home_dir().join("Pictures");
        if pics.is_dir() {
            pics
        } else {
            file_dialog::home_dir()
        }
    };
    let name = format!("Screenshot-{}.png", Local::now().format("%Y%m%d-%H%M%S"));
    let path = dir.join(&name);
    match image::save_buffer(
        &path,
        buffer.as_bytes(),
        buffer.width(),
        buffer.height(),
        image::ColorType::Rgba8,
    ) {
        Ok(()) => {
            log::info!("screenshot saved to {}", path.display());
            notif.internal("Screenshot", &format!("Saved {name}"));
        }
        Err(err) => log::warn!("screenshot: save failed: {err}"),
    }
}

/// Apply persisted settings to the UI.
fn apply_config(d: &Desktop, c: &config::Config) {
    let theme = d.global::<Theme>();
    theme.set_dark(c.dark);
    theme.set_accent(u32_to_color(c.accent));
    d.set_panel_edge(c.panel_edge);
    d.set_panel_size(c.panel_size);
    if let Some(path) = &c.background {
        if let Ok(image) = slint::Image::load_from_path(std::path::Path::new(path)) {
            d.set_background_image(image);
        }
    }
}

/// Read the current settings out of the UI, carrying over the non-UI bits
/// (lock password, keybinds) so saving doesn't drop them. The launcher lists are
/// read back from their live models, which the Settings dialog edits in place.
fn current_config(
    d: &Desktop,
    bg: &Rc<RefCell<Option<String>>>,
    lock_password: &Rc<RefCell<String>>,
    keybinds: &Rc<Vec<keybind::Keybind>>,
    keyboard_layouts: &Rc<Vec<String>>,
) -> config::Config {
    let theme = d.global::<Theme>();
    let read = |model: ModelRc<MenuEntry>| -> Vec<config::AppEntry> {
        model
            .iter()
            .filter(|e| !e.command.is_empty())
            .map(|e| config::AppEntry {
                icon: e.icon.to_string(),
                name: e.name.to_string(),
                command: e.command.to_string(),
            })
            .collect()
    };
    config::Config {
        dark: theme.get_dark(),
        accent: color_to_u32(theme.get_accent()),
        panel_edge: d.get_panel_edge(),
        panel_size: d.get_panel_size(),
        background: bg.borrow().clone(),
        menu: read(d.get_menu_entries()),
        desktop: read(d.get_desktop_icons()),
        panel_apps: read(d.get_panel_launchers()),
        lock_password: lock_password.borrow().clone(),
        keybinds: (**keybinds).clone(),
        keyboard_layouts: (**keyboard_layouts).clone(),
    }
}

/// Build the portal appearance (color-scheme + accent) from the current theme.
fn current_appearance(d: &Desktop) -> portal::Appearance {
    let theme = d.global::<Theme>();
    let c = theme.get_accent();
    portal::Appearance {
        scheme: if theme.get_dark() { 1 } else { 2 },
        accent: (
            c.red() as f64 / 255.0,
            c.green() as f64 / 255.0,
            c.blue() as f64 / 255.0,
        ),
    }
}

fn color_to_u32(c: slint::Color) -> u32 {
    ((c.red() as u32) << 16) | ((c.green() as u32) << 8) | (c.blue() as u32)
}

fn u32_to_color(n: u32) -> slint::Color {
    slint::Color::from_rgb_u8(
        ((n >> 16) & 0xff) as u8,
        ((n >> 8) & 0xff) as u8,
        (n & 0xff) as u8,
    )
}

/// Per-tick window-animation bookkeeping: clear the `appearing` flag one frame
/// after a window maps (so the open animation runs), and remove windows whose
/// close fade has elapsed. Returns true if the model changed.
fn process_window_anims(model: &Rc<VecModel<WindowTile>>, windows: &Rc<RefCell<Windows>>) -> bool {
    let mut dirty = false;
    let mut w = windows.borrow_mut();

    // Clear `appearing` for windows queued last tick.
    for id in std::mem::take(&mut w.appear_clear) {
        if let Some(&row) = w.rows.get(&id) {
            if let Some(mut tile) = model.row_data(row) {
                if tile.appearing {
                    tile.appearing = false;
                    model.set_row_data(row, tile);
                    dirty = true;
                }
            }
        }
    }
    // Queue this tick's new windows to be cleared next tick.
    w.appear_clear = std::mem::take(&mut w.appear_new);

    // Remove windows whose fade-out finished.
    let now = std::time::Instant::now();
    let due: Vec<u64> = w
        .closing_at
        .iter()
        .filter(|(_, &t)| t <= now)
        .map(|(&id, _)| id)
        .collect();
    for id in due {
        w.closing_at.remove(&id);
        if let Some(removed) = w.rows.remove(&id) {
            model.remove(removed);
            for row in w.rows.values_mut() {
                if *row > removed {
                    *row -= 1;
                }
            }
            w.closed.push(id);
            dirty = true;
        }
    }
    dirty
}

/// Maximize or restore a window, clamping a maximized window to the work area so
/// it never extends under the panel. Used by both the title-bar button and
/// client-initiated `xdg_toplevel.set_maximized` requests.
fn set_maximized(
    d: &Desktop,
    model: &Rc<VecModel<WindowTile>>,
    windows: &Rc<RefCell<Windows>>,
    cmd_tx: &s_compositor_wayland::CommandSender<s_compositor_wayland::Command>,
    id: u64,
    maximized: bool,
) {
    let mut windows = windows.borrow_mut();
    let Some(&row) = windows.rows.get(&id) else {
        return;
    };
    let Some(mut tile) = model.row_data(row) else {
        return;
    };
    if tile.maximized == maximized {
        return;
    }
    if maximized {
        windows
            .restore
            .insert(id, (tile.x, tile.y, tile.width, tile.height));
        let (x, y, w, h) = work_area(d, tile.x + tile.width / 2.0, tile.y + tile.height / 2.0);
        let titlebar = if tile.decorated { 28.0 } else { 0.0 };
        tile.x = x;
        tile.y = y;
        tile.width = w;
        tile.height = (h - titlebar).max(1.0);
        tile.maximized = true;
    } else {
        if let Some((x, y, w, h)) = windows.restore.remove(&id) {
            tile.x = x;
            tile.y = y;
            tile.width = w;
            tile.height = h;
        }
        tile.maximized = false;
    }
    let _ = cmd_tx.send(s_compositor_wayland::Command::ResizeWindow {
        id: s_compositor_wayland::WindowId(id),
        width: tile.width as i32,
        height: tile.height as i32,
    });
    model.set_row_data(row, tile);
}

/// Re-fit every maximized window to the current work area. Used after the
/// screen/output is resized so maximized windows keep filling it.
fn refit_maximized(
    d: &Desktop,
    model: &Rc<VecModel<WindowTile>>,
    cmd_tx: &s_compositor_wayland::CommandSender<s_compositor_wayland::Command>,
) {
    for row in 0..model.row_count() {
        let Some(mut tile) = model.row_data(row) else {
            continue;
        };
        if !tile.maximized {
            continue;
        }
        let (x, y, w, h) = work_area(d, tile.x + tile.width / 2.0, tile.y + tile.height / 2.0);
        let titlebar = if tile.decorated { 28.0 } else { 0.0 };
        tile.x = x;
        tile.y = y;
        tile.width = w;
        tile.height = (h - titlebar).max(1.0);
        let _ = cmd_tx.send(s_compositor_wayland::Command::ResizeWindow {
            id: s_compositor_wayland::WindowId(tile.id as u64),
            width: tile.width as i32,
            height: tile.height as i32,
        });
        model.set_row_data(row, tile);
    }
}

/// Apply one SNI tray update to the model. `Add` upserts; `Remove` drops the row.
fn apply_tray_update(
    update: s_compositor_tray::TrayUpdate,
    model: &Rc<VecModel<TrayIcon>>,
    ids: &Rc<RefCell<Vec<String>>>,
) {
    match update {
        s_compositor_tray::TrayUpdate::Add {
            id,
            title,
            icon_name,
            pixmap,
        } => {
            let icon = match pixmap {
                Some((w, h, px)) => image_from_rgba(w, h, &px),
                None => icons::image_for_icon_name(&icon_name),
            };
            let tile = TrayIcon {
                id: id.clone().into(),
                title: title.into(),
                icon,
            };
            let pos = ids.borrow().iter().position(|x| x == &id);
            match pos {
                Some(idx) => model.set_row_data(idx, tile),
                None => {
                    ids.borrow_mut().push(id);
                    model.push(tile);
                }
            }
        }
        s_compositor_tray::TrayUpdate::Remove { id } => {
            let pos = ids.borrow().iter().position(|x| x == &id);
            if let Some(idx) = pos {
                ids.borrow_mut().remove(idx);
                model.remove(idx);
            }
        }
    }
}

/// Apply a NetworkManager update to the UI.
fn apply_net_event(event: network::NetEvent, d: &Desktop, model: &Rc<VecModel<WifiNetwork>>) {
    match event {
        network::NetEvent::Enabled(on) => d.set_wifi_enabled(on),
        network::NetEvent::Networks(nets) => {
            let rows: Vec<WifiNetwork> = nets
                .into_iter()
                .map(|n| WifiNetwork {
                    ssid: n.ssid.into(),
                    strength: n.strength as i32,
                    secure: n.secure,
                    active: n.active,
                })
                .collect();
            model.set_vec(rows);
        }
    }
}

/// Apply a notification event: `Add` upserts a popup (and schedules its expiry),
/// `Close` removes it.
/// Notification state: on-screen popups, persistent history and Do-Not-Disturb.
struct NotifState {
    popups: Rc<VecModel<Notification>>,
    history: Rc<VecModel<Notification>>,
    expiry: RefCell<HashMap<u32, std::time::Instant>>,
    dnd: Cell<bool>,
}

impl NotifState {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            popups: Rc::new(VecModel::default()),
            history: Rc::new(VecModel::default()),
            expiry: RefCell::new(HashMap::new()),
            dnd: Cell::new(false),
        })
    }

    /// Apply an incoming notification event: record it in history, and (unless
    /// Do-Not-Disturb is on) show a popup that expires after its timeout.
    fn apply(&self, event: notify::NotifyEvent) {
        match event {
            notify::NotifyEvent::Add {
                id,
                app_name,
                summary,
                body,
                icon,
                timeout_ms,
            } => {
                let note = Notification {
                    id: id as i32,
                    app_name: app_name.into(),
                    summary: summary.into(),
                    body: body.into(),
                    icon: icons::image_for_icon_name(&icon),
                };
                self.history_upsert(note.clone());
                if self.dnd.get() {
                    return;
                }
                let pos = (0..self.popups.row_count())
                    .find(|&i| self.popups.row_data(i).map(|n| n.id as u32) == Some(id));
                match pos {
                    Some(i) => self.popups.set_row_data(i, note),
                    None => self.popups.push(note),
                }
                // 0 = never expire; -1 = default (5s); otherwise the given ms.
                if timeout_ms != 0 {
                    let ms = if timeout_ms < 0 {
                        5000
                    } else {
                        timeout_ms as u64
                    };
                    self.expiry
                        .borrow_mut()
                        .insert(id, std::time::Instant::now() + Duration::from_millis(ms));
                } else {
                    self.expiry.borrow_mut().remove(&id);
                }
            }
            notify::NotifyEvent::Close { id } => self.dismiss(id),
        }
    }

    /// Insert (newest-first) into the capped history, replacing any same id.
    fn history_upsert(&self, note: Notification) {
        if let Some(i) = (0..self.history.row_count())
            .find(|&i| self.history.row_data(i).map(|n| n.id) == Some(note.id))
        {
            self.history.remove(i);
        }
        self.history.insert(0, note);
        while self.history.row_count() > 50 {
            self.history.remove(self.history.row_count() - 1);
        }
    }

    /// Dismiss a popup (history is kept).
    fn dismiss(&self, id: u32) {
        self.expiry.borrow_mut().remove(&id);
        if let Some(i) = (0..self.popups.row_count())
            .find(|&i| self.popups.row_data(i).map(|n| n.id as u32) == Some(id))
        {
            self.popups.remove(i);
        }
    }

    /// Remove timed-out popups. Returns true if any were removed.
    fn expire(&self) -> bool {
        let now = std::time::Instant::now();
        let due: Vec<u32> = self
            .expiry
            .borrow()
            .iter()
            .filter(|(_, &t)| t <= now)
            .map(|(&id, _)| id)
            .collect();
        for id in &due {
            self.dismiss(*id);
        }
        !due.is_empty()
    }

    fn clear_history(&self) {
        self.history.set_vec(Vec::<Notification>::new());
    }

    /// Push a shell-internal notification (e.g. screenshot saved).
    fn internal(&self, summary: &str, body: &str) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(1_000_000_000);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        self.apply(notify::NotifyEvent::Add {
            id,
            app_name: "s-compositor".to_string(),
            summary: summary.to_string(),
            body: body.to_string(),
            icon: String::new(),
            timeout_ms: -1,
        });
    }
}

/// Build a Slint image from a tightly-packed RGBA8 buffer.
fn image_from_rgba(width: u32, height: u32, pixels: &[u8]) -> slint::Image {
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    let expected = (width * height * 4) as usize;
    if pixels.len() >= expected {
        buf.make_mut_bytes().copy_from_slice(&pixels[..expected]);
    }
    slint::Image::from_rgba8(buf)
}

/// The desktop work area `(x, y, w, h)` in logical pixels: the screen minus the
/// panel on its docked edge.
/// The usable area (minus the panel) of the output containing the point
/// `(cx, cy)` — used to maximize/tile a window onto its own monitor. Falls back
/// to the first output, then the whole canvas.
fn work_area(d: &Desktop, cx: f32, cy: f32) -> (f32, f32, f32, f32) {
    let outputs = d.get_outputs();
    let mut found = None;
    for i in 0..outputs.row_count() {
        if let Some(o) = outputs.row_data(i) {
            if cx >= o.x && cx < o.x + o.width && cy >= o.y && cy < o.y + o.height {
                found = Some((o.x, o.y, o.width, o.height));
                break;
            }
        }
    }
    let (ox, oy, ow, oh) = found
        .or_else(|| outputs.row_data(0).map(|o| (o.x, o.y, o.width, o.height)))
        .unwrap_or_else(|| {
            let scale = d.window().scale_factor().max(0.01);
            let size = d.window().size();
            (
                0.0,
                0.0,
                size.width as f32 / scale,
                size.height as f32 / scale,
            )
        });
    let panel = d.get_panel_size();
    match d.get_panel_edge() {
        1 => (ox + panel, oy, ow - panel, oh), // Left
        2 => (ox, oy + panel, ow, oh - panel), // Top
        3 => (ox, oy, ow, oh - panel),         // Bottom
        _ => (ox, oy, ow - panel, oh),         // Right (default)
    }
}

/// Map a Slint pointer-button index (1=left, 2=right, 3=middle) to an evdev code.
fn evdev_button(btn: i32) -> u32 {
    match btn {
        2 => 0x111, // BTN_RIGHT
        3 => 0x112, // BTN_MIDDLE
        _ => 0x110, // BTN_LEFT
    }
}

/// Forward one key press as a modifier-wrapped tap to the focused window.
fn forward_key(
    cmd_tx: &s_compositor_wayland::CommandSender<s_compositor_wayland::Command>,
    text: &str,
    ctrl: bool,
    alt: bool,
    shift_mod: bool,
) {
    let Some((keycode, needs_shift)) = evdev_keycode(text) else {
        return;
    };
    let mut mods = Vec::new();
    if ctrl {
        mods.push(29); // KEY_LEFTCTRL
    }
    if alt {
        mods.push(56); // KEY_LEFTALT
    }
    if needs_shift || shift_mod {
        mods.push(42); // KEY_LEFTSHIFT
    }
    let key = |keycode: u32, pressed: bool| {
        let _ = cmd_tx.send(s_compositor_wayland::Command::Key { keycode, pressed });
    };
    for m in &mods {
        key(*m, true);
    }
    key(keycode, true);
    key(keycode, false);
    for m in mods.iter().rev() {
        key(*m, false);
    }
}

/// Best-effort mapping of Slint key text to a US-layout evdev keycode plus
/// whether Shift is needed. Returns `None` for unmapped keys.
fn evdev_keycode(text: &str) -> Option<(u32, bool)> {
    let mut chars = text.chars();
    let c = chars.next()?;
    // Named keys (Slint encodes these as specific control/private-use chars).
    match c {
        '\u{000a}' | '\r' => return Some((28, false)), // Return
        '\u{0008}' => return Some((14, false)),        // Backspace
        '\u{0009}' => return Some((15, false)),        // Tab
        '\u{001b}' => return Some((1, false)),         // Escape
        '\u{007f}' => return Some((111, false)),       // Delete
        '\u{f700}' => return Some((103, false)),       // Up
        '\u{f701}' => return Some((108, false)),       // Down
        '\u{f702}' => return Some((105, false)),       // Left
        '\u{f703}' => return Some((106, false)),       // Right
        ' ' => return Some((57, false)),               // Space
        _ => {}
    }
    // Beyond here we only handle single printable characters.
    if chars.next().is_some() {
        return None;
    }
    if c.is_ascii_alphabetic() {
        let code = match c.to_ascii_lowercase() {
            'a' => 30,
            'b' => 48,
            'c' => 46,
            'd' => 32,
            'e' => 18,
            'f' => 33,
            'g' => 34,
            'h' => 35,
            'i' => 23,
            'j' => 36,
            'k' => 37,
            'l' => 38,
            'm' => 50,
            'n' => 49,
            'o' => 24,
            'p' => 25,
            'q' => 16,
            'r' => 19,
            's' => 31,
            't' => 20,
            'u' => 22,
            'v' => 47,
            'w' => 17,
            'x' => 45,
            'y' => 21,
            'z' => 44,
            _ => return None,
        };
        return Some((code, c.is_ascii_uppercase()));
    }
    let mapped = match c {
        '1' => (2, false),
        '2' => (3, false),
        '3' => (4, false),
        '4' => (5, false),
        '5' => (6, false),
        '6' => (7, false),
        '7' => (8, false),
        '8' => (9, false),
        '9' => (10, false),
        '0' => (11, false),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        '\\' => (43, false),
        '|' => (43, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        _ => return None,
    };
    Some(mapped)
}

/// A program launched via the shell that failed to start (e.g. the binary was
/// not found). Sent to the UI thread so it can show a notification.
struct LaunchError {
    cmd: String,
    reason: String,
}

/// Check whether a *simple* command's program can be found, returning a reason
/// ("command not found") if not. Commands with shell metacharacters (pipes,
/// redirects, variables, …) are left to the shell, as are env-assignment and `~`
/// prefixes, since we can't validate those without invoking the shell.
fn command_program_missing(cmd: &str) -> Option<String> {
    let shellish = cmd.contains([
        '|', '&', ';', '<', '>', '$', '`', '(', ')', '{', '}', '*', '?', '\'', '"', '\n',
    ]);
    if shellish {
        return None;
    }
    let prog = cmd.split_whitespace().next()?;
    if prog.contains('=') || prog.starts_with('~') {
        return None;
    }
    let found = if prog.contains('/') {
        std::path::Path::new(prog).is_file()
    } else {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(prog).is_file()))
            .unwrap_or(false)
    };
    (!found).then(|| "command not found".to_string())
}

/// Spawn a shell command detached, pointed at s-compositor's compositor. `wayland` is
/// `(WAYLAND_DISPLAY, XDG_RUNTIME_DIR)` of s-compositor's own socket. A fast
/// failure (missing binary, immediate crash) is reported on `fail_tx`.
fn spawn_command(
    cmd: &str,
    wayland: Option<(&str, &str)>,
    fail_tx: &std::sync::mpsc::Sender<LaunchError>,
) {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return;
    }
    // Fast path: report an obviously-missing program immediately, rather than
    // waiting out the launch-watch grace period below.
    if let Some(reason) = command_program_missing(cmd) {
        log::warn!("not launching `{cmd}`: {reason}");
        let _ = fail_tx.send(LaunchError {
            cmd: cmd.to_string(),
            reason,
        });
        return;
    }

    let mut command = std::process::Command::new("/bin/sh");
    command.arg("-c").arg(cmd);
    // Children must connect to s-compositor. Remove any inherited WAYLAND_SOCKET (an fd
    // s-compositor received from its own host): libwayland prefers it over
    // WAYLAND_DISPLAY, which would make the child target the wrong compositor and
    // fail with "permission denied". Also drop DISPLAY so toolkits don't fall
    // back to X.
    command.env_remove("WAYLAND_SOCKET");
    command.env_remove("DISPLAY");
    // Point X11 apps at our XWayland display (set once XWayland is ready), so
    // they fall back to X while Wayland apps still prefer WAYLAND_DISPLAY.
    if let Ok(x_display) = std::env::var("S_COMPOSITOR_XDISPLAY") {
        command.env("DISPLAY", x_display);
    }
    if let Some((display, runtime_dir)) = wayland {
        command.env("WAYLAND_DISPLAY", display);
        command.env("XDG_RUNTIME_DIR", runtime_dir);
        let socket_path = format!("{runtime_dir}/{display}");
        let exists = std::path::Path::new(&socket_path).exists();
        log::info!(
            "launching with WAYLAND_DISPLAY={display} XDG_RUNTIME_DIR={runtime_dir} \
             (socket {socket_path} exists={exists})"
        );
    } else {
        log::warn!(
            "launching `{cmd}` but s-compositor's compositor socket is not ready yet; \
             the child will inherit the host's environment"
        );
    }

    match command.spawn() {
        Ok(child) => {
            log::info!("launched `{cmd}` (pid {})", child.id());
            // We run via `/bin/sh -c`, so `spawn` almost always succeeds even
            // when the real program is missing — the shell then exits 127. Watch
            // briefly for such a fast failure and report it; otherwise let the
            // program run (and reap it whenever it eventually exits).
            let cmd = cmd.to_string();
            let fail_tx = fail_tx.clone();
            let _ = std::thread::Builder::new()
                .name("launch-watch".into())
                .spawn(move || {
                    let mut child = child;
                    std::thread::sleep(Duration::from_millis(700));
                    match child.try_wait() {
                        Ok(Some(status)) if !status.success() => {
                            let reason = match status.code() {
                                Some(127) => "command not found".to_string(),
                                Some(126) => "not executable".to_string(),
                                Some(code) => format!("exited immediately (status {code})"),
                                None => "terminated by a signal".to_string(),
                            };
                            let _ = fail_tx.send(LaunchError { cmd, reason });
                        }
                        // Still running after the grace period: assume it started
                        // fine, but wait so it doesn't linger as a zombie.
                        Ok(None) => {
                            let _ = child.wait();
                        }
                        _ => {}
                    }
                });
        }
        Err(err) => {
            log::error!("failed to launch `{cmd}`: {err}");
            let _ = fail_tx.send(LaunchError {
                cmd: cmd.to_string(),
                reason: err.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{command_program_missing, evdev_button, evdev_keycode};

    #[test]
    fn reports_missing_commands() {
        // Nonexistent program (bare name) and nonexistent path are reported.
        assert!(command_program_missing("definitely-not-a-real-binary-xyz").is_some());
        assert!(command_program_missing("/no/such/file-xyz").is_some());
        // Existing program (by path, and on PATH with args) is accepted.
        assert!(command_program_missing("/bin/sh").is_none());
        assert!(command_program_missing("sh -c true").is_none());
        // Shell-y / special commands are left to the shell, not pre-checked.
        assert!(command_program_missing("foo | bar").is_none());
        assert!(command_program_missing("echo $HOME").is_none());
        assert!(command_program_missing("~/bin/thing").is_none());
        assert!(command_program_missing("").is_none());
    }

    #[test]
    fn keycode_letters() {
        assert_eq!(evdev_keycode("a"), Some((30, false)));
        assert_eq!(evdev_keycode("z"), Some((44, false)));
        // Uppercase requires shift.
        assert_eq!(evdev_keycode("A"), Some((30, true)));
        assert_eq!(evdev_keycode("Q"), Some((16, true)));
    }

    #[test]
    fn keycode_digits_and_symbols() {
        assert_eq!(evdev_keycode("1"), Some((2, false)));
        assert_eq!(evdev_keycode("0"), Some((11, false)));
        // Shifted symbols share the digit keycodes.
        assert_eq!(evdev_keycode("!"), Some((2, true)));
        assert_eq!(evdev_keycode(")"), Some((11, true)));
        assert_eq!(evdev_keycode("/"), Some((53, false)));
        assert_eq!(evdev_keycode("?"), Some((53, true)));
    }

    #[test]
    fn keycode_named_keys() {
        assert_eq!(evdev_keycode(" "), Some((57, false))); // Space
        assert_eq!(evdev_keycode("\u{000a}"), Some((28, false))); // Return
        assert_eq!(evdev_keycode("\u{0008}"), Some((14, false))); // Backspace
        assert_eq!(evdev_keycode("\u{0009}"), Some((15, false))); // Tab
        assert_eq!(evdev_keycode("\u{001b}"), Some((1, false))); // Escape
        assert_eq!(evdev_keycode("\u{f702}"), Some((105, false))); // Left arrow
    }

    #[test]
    fn keycode_unmapped() {
        assert_eq!(evdev_keycode(""), None);
        assert_eq!(evdev_keycode("ab"), None); // more than one char
        assert_eq!(evdev_keycode("€"), None);
    }

    #[test]
    fn buttons_map_to_evdev() {
        assert_eq!(evdev_button(1), 0x110); // left
        assert_eq!(evdev_button(2), 0x111); // right
        assert_eq!(evdev_button(3), 0x112); // middle
        assert_eq!(evdev_button(0), 0x110); // fallback
    }
}

#[cfg(test)]
mod shot {
    //! Opt-in headless render of the desktop scene (set `SCOMP_SHOT=1`), to
    //! review the wallpaper layer — desktop shortcuts and the panel — without a
    //! display server. Uses Slint's software renderer, like the `s-files` test.
    use super::*;
    use slint::platform::software_renderer::{
        MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
    };
    use slint::platform::{Platform, WindowAdapter};
    use std::rc::Rc;

    struct SwPlatform {
        window: Rc<MinimalSoftwareWindow>,
    }
    impl Platform for SwPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.window.clone())
        }
    }

    #[test]
    fn desktop_shortcuts() {
        if std::env::var("SCOMP_SHOT").is_err() {
            return;
        }
        let multi = std::env::var("SCOMP_SHOT_MULTI").is_ok();
        let (w, h) = if multi {
            (2560u32, 800u32)
        } else {
            (1280u32, 800u32)
        };
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        slint::platform::set_platform(Box::new(SwPlatform {
            window: window.clone(),
        }))
        .unwrap();
        window.set_size(slint::PhysicalSize::new(w, h));

        let d = Desktop::new().unwrap();
        let model = |items: Vec<(&str, &str, &str)>| {
            ModelRc::from(Rc::new(VecModel::from(
                items
                    .into_iter()
                    .map(|(i, n, c)| MenuEntry {
                        icon: i.into(),
                        name: n.into(),
                        command: c.into(),
                        kind: "app".into(),
                    })
                    .collect::<Vec<_>>(),
            )))
        };
        d.set_menu_entries(model(vec![
            ("🌐", "Firefox", "firefox"),
            ("🖥", "Terminal", "alacritty"),
        ]));
        d.set_desktop_icons(model(vec![
            ("🗂", "Files", "s-files"),
            ("🖥", "Terminal", "alacritty"),
            ("🌐", "Browser", "firefox"),
        ]));
        d.set_panel_launchers(model(vec![("🗂", "Files", "s-files")]));

        // A simple gradient wallpaper so the per-output background is visible.
        let mut bg = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(64, 48);
        {
            let px = bg.make_mut_bytes();
            for y in 0..48u32 {
                for x in 0..64u32 {
                    let i = ((y * 64 + x) * 4) as usize;
                    px[i] = (x * 4) as u8;
                    px[i + 1] = (y * 5) as u8;
                    px[i + 2] = 150;
                    px[i + 3] = 255;
                }
            }
        }
        d.set_background_image(slint::Image::from_rgba8(bg));
        let out = |x: f32, y: f32, w: f32, h: f32| OutputRect {
            x,
            y,
            width: w,
            height: h,
        };
        d.set_outputs(ModelRc::from(Rc::new(VecModel::from(if multi {
            vec![out(0., 0., 1280., 800.), out(1280., 0., 1280., 800.)]
        } else {
            vec![out(0., 0., 1280., 800.)]
        }))));

        if multi {
            // Windows spread across both outputs, to check per-output taskbars.
            let win = |title: &str, x: f32| WindowTile {
                title: title.into(),
                x,
                y: 220.0,
                width: 360.0,
                height: 260.0,
                workspace: 0,
                ..Default::default()
            };
            d.set_windows(ModelRc::from(Rc::new(VecModel::from(vec![
                win("Editor (left)", 300.0),
                win("Browser (right)", 1620.0),
                win("Terminal (left)", 520.0),
            ]))));
        }

        if std::env::var("SCOMP_SHOT_SETTINGS").is_ok() {
            d.set_settings_visible(true);
            d.set_settings_tab(1);
        }
        if std::env::var("SCOMP_SHOT_STARTMENU").is_ok() {
            d.set_start_menu_entries(model(vec![
                ("🌐", "Firefox", "firefox"),
                ("🖥", "Terminal", "alacritty"),
                ("🗂", "Files", "s-files"),
            ]));
            d.set_start_menu_visible(true);
        }
        d.show().unwrap();
        window.window().request_redraw();

        let mut buf = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
        window.draw_if_needed(|r| {
            r.render(&mut buf, w as usize);
        });
        let mut rgba = Vec::with_capacity(buf.len() * 4);
        for p in &buf {
            let a = p.alpha;
            let u = |c: u8| {
                if a == 0 || a == 255 {
                    c
                } else {
                    ((c as u16 * 255) / a as u16) as u8
                }
            };
            rgba.extend_from_slice(&[u(p.red), u(p.green), u(p.blue), a]);
        }
        let out_path = if multi {
            "/tmp/multi-output.png"
        } else {
            "/tmp/desktop-icons.png"
        };
        image::save_buffer(out_path, &rgba, w, h, image::ExtendedColorType::Rgba8).unwrap();
        eprintln!("wrote {out_path}");
        d.hide().unwrap();
    }
}
