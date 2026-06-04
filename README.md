# s-compositor

A small but complete **Wayland desktop** written in Rust — a compositor, a
panel, a start menu, notifications, a system tray, a settings app and a file
manager, all in one. It uses [Slint](https://slint.dev) for the interface and
[Smithay](https://smithay.github.io/) for the Wayland protocol.

You can run it two ways:

- **Nested** — in a window inside your current X11/Wayland session. This is the
  easiest way to try it out (see [Quick start](#quick-start)).
- **On bare metal** — directly on a Linux TTY (no X11/Wayland needed),
  via the `linuxkms` backend.

## Quick start

### 1. Install the prerequisites

You need a Rust toolchain (stable — install via [rustup](https://rustup.rs)) and
a few system libraries. The renderer is Skia (compiled from source the first
time, so a C/C++ toolchain is required).

On Debian/Ubuntu:

```sh
sudo apt install build-essential clang libxkbcommon-dev libfontconfig-1-dev \
    libudev-dev libseat-dev libinput-dev libgbm-dev libdrm-dev libpulse-dev
```

### 2. Install a terminal (and other apps)

The desktop is just a shell — it launches *other* programs. Most importantly,
**install a terminal**: the default terminal is
**[Alacritty](https://alacritty.org)** (a fast, Wayland-native terminal written
in Rust), spawned by the `Super`+`Return` shortcut, the desktop right-click
"Open Terminal" entry, and the panel / start-menu launchers.

```sh
cargo install alacritty        # from crates.io, or:
sudo apt install alacritty     # on recent Debian/Ubuntu
```

If Alacritty isn't present, other terminals (WezTerm, foot, kitty) are
auto-detected — but avoid `gnome-terminal` when running nested, as it is a D-Bus
single-instance app and opens in your *outer* session instead.

A web browser (e.g. `firefox`) is picked up automatically too, and the file
manager (`s-files`) is built and bundled for you — no extra install needed.

### 3. Build and run

```sh
cargo run
```

This opens a window running the whole desktop. It prints the `WAYLAND_DISPLAY`
it created (e.g. `wayland-1`); any Wayland client pointed at that socket appears
inside it:

```sh
WAYLAND_DISPLAY=wayland-1 alacritty
```

…but normally you just launch apps from the panel, the start menu (⊞), the
command launcher (▶), or your keyboard shortcuts.

> **Tip:** when running nested, **resizing the window resizes the desktop** —
> the wallpaper, panel and apps reflow to fit.

> **If launching an app fails** (e.g. the program isn't installed), the desktop
> shows a *"Couldn't open application"* notification telling you what went wrong.

> **Linker note:** linking needs the `libxkbcommon.so` *development* symlink, not
> just the runtime `libxkbcommon.so.0`. If you see `error: unable to find library
> -lxkbcommon`, install `libxkbcommon-dev` (above) or symlink it:
> `sudo ln -s libxkbcommon.so.0 /usr/lib/x86_64-linux-gnu/libxkbcommon.so`.

## What's implemented

Everything below works today.

**Windows**
- Move by dragging the title bar — the compositor's, or the app's own for
  client-side-decorated apps like Alacritty — plus resize (corner grip),
  maximize and minimize.
- **Server-side decorations** drawn by the compositor (title bar with
  minimize/maximize/close, plus a border); client-side-decorated apps keep their
  own title bar and transparent drop-shadow instead.
- **Tiling / snapping**: snap to screen halves with `Super`+arrows or by dragging
  a window to a screen edge.
- **Always on top**: pin a window from its title-bar right-click menu.
- **Four workspaces**, an **Alt-Tab** window switcher, and a **Show Desktop**
  toggle.

**Panel** (dockable to any screen edge, size configurable)
- A live **clock** that stacks to two lines when the panel is narrow.
- A **taskbar**, a **command launcher** (▶) and a **start menu** (⊞) with app
  search and power actions (lock, suspend, restart, shut down, log out).
- Status applets: **battery** (UPower), **CPU/memory monitor**, **volume**
  (PulseAudio), **Wi-Fi** (NetworkManager: scan / connect / toggle), an optional
  **keyboard-layout switcher**, and a playful **xeyes** applet.
- A **system tray** (StatusNotifierItem host) and a **quick-settings** flyout.

**Desktop & system**
- **Notifications**: an `org.freedesktop.Notifications` daemon with on-screen
  popups, a history, and Do-Not-Disturb.
- **Theming**: accent colour and light/dark scheme, applied live, **saved** to
  `s-compositor.conf`, and **published to apps** over the XDG appearance portal
  so GTK/Qt apps follow your theme.
- A **lock screen** (optional password) and a **screenshot** action
  (`Super`+`p` → saved to `~/Pictures`).
- **Configurable global keyboard shortcuts** (see below).

**Apps & integration**
- **`s-files`**, a bundled file manager with icon / list / details views,
  navigation, multi-selection, copy / cut / paste, rename, trash, new folder,
  drag-and-drop, a properties dialog, search and column sorting.
- A reusable **file dialog**, also exposed as an XDG **FileChooser portal**
  backend so other apps can open files through it (see below).
- **XWayland**: X11 apps run via a rootless XWayland server with a built-in
  window manager.

## Keyboard shortcuts

Shortcuts live in `s-compositor.conf` (in the working directory) as
`bind = <combo> = <action>`:

```
bind = Super+Return = spawn:alacritty
bind = Super+d = start-menu
bind = Super+e = spawn:s-files
bind = Super+q = close-window
bind = Alt+Tab = next-window
bind = Super+1 = workspace:1
bind = Super+Shift+1 = move-to-workspace:1
bind = Super+Equal = volume-up
```

Modifiers are `Super`/`Ctrl`/`Alt`/`Shift`; keys are letters, digits or names
(`Return`, `Space`, `Tab`, `Escape`, `Minus`, `Equal`, `Comma`, `Period`).
Available actions: `spawn:<cmd>`, `start-menu`, `launcher`, `settings`,
`quick-settings`, `lock`, `logout`, `close-window`, `next-window`,
`prev-window`, `workspace:<n>`, `move-to-workspace:<n>`, `workspace-next`,
`workspace-prev`, `volume-up`, `volume-down`, `volume-mute`, `snap-left`,
`snap-right`, `snap-up`, `snap-down`, `maximize`, `screenshot`.

With no `bind` lines, sensible defaults apply (including `Super`+arrows to
tile/maximize the focused window and `Super`+`p` to screenshot).

To enable the keyboard-layout switcher, list two or more xkb layouts:

```
keyboard_layouts = us,fr,de
```

## File chooser portal

`s-compositor` implements `org.freedesktop.impl.portal.FileChooser`, so other
apps can open files through its dialog. To route requests to it:

```sh
sudo install -Dm644 data/s-compositor.portal \
  /usr/share/xdg-desktop-portal/portals/s-compositor.portal
mkdir -p ~/.config/xdg-desktop-portal
cp data/s-compositor-portals.conf ~/.config/xdg-desktop-portal/portals.conf
```

Test the backend directly (no `xdg-desktop-portal` needed):

```sh
gdbus call --session --dest org.freedesktop.impl.portal.desktop.scompositor \
  --object-path /org/freedesktop/portal/desktop \
  --method org.freedesktop.impl.portal.FileChooser.OpenFile \
  /req app "" "Pick a file" "@a{sv} {}"
# => (uint32 0, {'uris': <['file:///path/to/chosen']>})
```

## For developers

### How it works

`s-compositor` inverts the usual Smithay layering: **Slint owns rendering,
output, input and the main event loop** (via `backend-winit` when nested and
`backend-linuxkms` on bare metal), while **Smithay is used purely as the Wayland
protocol engine** — it manages clients, surfaces, buffers, `xdg-shell`,
`wlr-layer-shell` and the seat, but presents nothing itself.

Client windows are imported into GL textures (owned by the compositor, shared
with the Skia renderer as borrowed textures) and shown as Slint `Image`s; the
panel, taskbar and tray are ordinary Slint UI in the same scene graph.

```
┌──────────────── main thread (Slint) ─────────────────┐
│  UI + GL + clock        rendering-notifier imports    │
│  desktop.slint / panel   client buffers -> textures   │
└───────────────▲───────────────────────┬──────────────┘
        Event channel            calloop::channel
┌───────────────┴───────────────────────▼──────────────┐
│  wayland thread (Smithay + calloop)                   │
│  wl_compositor / xdg-shell / shm / seat / output      │
└───────────────────────────────────────────────────────┘
```

### Project layout

| Crate | Role |
|-------|------|
| `s-compositor` | The desktop binary: wires the Slint UI and the Wayland thread together. |
| `s-compositor-wayland` | Smithay protocol engine (no rendering). Unit-testable logic. |
| `s-compositor-render` | GL buffer-import bridge (shm, and experimental dmabuf). |
| `s-compositor-shell` | Plain-Rust data models (windows, workspaces, clock). |
| `s-compositor-tray` | System-tray (StatusNotifierItem) host. |
| `s-files` | The standalone file-manager binary. |

The Slint UI lives in `ui/`. Building `s-compositor` also builds `s-files` (the
compositor's `build.rs` builds it and drops it next to the compositor binary),
so a plain `cargo run` gives you a working file manager.

### Testing (no display or GPU required)

```sh
cargo test            # unit tests + headless UI layout tests
```

The headless tests render the real Slint scenes with the software renderer. You
can also write PNG snapshots to review the UI visually:

```sh
cargo run -p s-compositor --example shell_screenshot
```

This writes `shot_top_panel.png`, `shot_right_panel.png` and `shot_settings.png`
(git-ignored) — the quickest way to confirm a UI change after editing `ui/`.

## License

MIT OR Apache-2.0
