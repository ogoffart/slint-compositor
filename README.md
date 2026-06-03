A Wayland desktop shell written in Rust, using [Slint](https://slint.dev) for the
UI and [Smithay](https://smithay.github.io/) for the Wayland protocol.

## Architecture

s-compositor inverts the usual Smithay layering: **Slint owns rendering, output, input
and the main event loop** (via its own `backend-winit` for nested development and
`backend-linuxkms` for bare metal), while **Smithay is used purely as the Wayland
protocol engine** — it manages clients, surfaces, buffers, `xdg-shell`,
`wlr-layer-shell` and the seat, but does not present anything itself.

Client windows are imported into GL textures (owned by s-compositor, shared with the
Skia renderer as borrowed textures) and shown as Slint `Image`s; the panel,
taskbar and tray are ordinary Slint UI in the same scene. This keeps the whole
desktop in one Slint scene graph.

```
┌──────────────── main thread (Slint) ─────────────────┐
│  UI + GL + clock        rendering-notifier imports    │
│  desktop.slint / panel   client buffers -> textures   │
└───────────────▲───────────────────────┬──────────────┘
        Event channel            calloop::channel (later)
┌───────────────┴───────────────────────▼──────────────┐
│  wayland thread (Smithay + calloop)                   │
│  wl_compositor / xdg-shell / shm / seat / output      │
└───────────────────────────────────────────────────────┘
```

## Workspace layout

| Crate | Role |
|-------|------|
| `s-compositor` | Binary: wires the Slint UI + the Wayland thread together. |
| `s-compositor-wayland` | Smithay protocol engine (no rendering). Unit-testable logic. |
| `s-compositor-render` | GL buffer-import bridge (shm MVP, then dmabuf). |
| `s-compositor-shell` | Plain-Rust data models (windows, workspaces, clock). |
| `s-compositor-tray` | System tray (SNI) host — stub for now. |

The Slint UI lives in `ui/` (`desktop.slint`, `panel.slint`, `clock.slint`,
`eyes.slint`, `settings.slint`, `launcher.slint`, `file_dialog.slint`,
`theme.slint`).

## Status

Working today (verified nested under Xvfb + llvmpipe):

- Compositor advertises the core globals (`wl_compositor`, `xdg_shell`,
  `wl_shm`, `wl_seat`, a virtual `wl_output`) on an auto-selected socket.
- Client windows are composited into the Slint scene. Their `wl_shm` buffers
  are uploaded into **GL textures we own and share with Slint's renderer** via
  `BorrowedOpenGLTextureBuilder` (rendered with the **Skia** OpenGL renderer).
  Frame callbacks are throttled to ~60Hz.
- **Server-side decorations**: s-compositor forces `zxdg-decoration` ServerSide and
  draws the title bar (title, minimize/maximize/close) and border itself.
- **Input**: pointer and keyboard are forwarded to the focused client; windows
  can be moved (title-bar drag), resized (corner grip), maximized and minimized.
- A panel dockable to any edge (configurable size) with a live clock that shows
  `HH:MM` on one line when the panel is wide enough and stacks to two lines when
  it is narrow, an **xeyes-style applet** whose pupils follow the pointer, a
  **command launcher** (▶), a **taskbar**, and a **settings** button (⚙).
- **Theming**: accent colour and light/dark scheme, **persisted** to
  `s-compositor.conf` in the working directory and **published over the XDG
  `Settings` portal** (`org.freedesktop.appearance`) so apps follow it.
- A reusable **file dialog** (editable path, keyboard navigation), also exposed
  as an XDG **FileChooser portal** backend so other apps open files through it.
- A **system tray**: an SNI (`StatusNotifierItem`) host, **Wi-Fi** (NetworkManager:
  scan / connect with password / toggle) and **volume** (libpulse) in a
  quick-settings flyout.
- A **notification daemon** (`org.freedesktop.Notifications`) with on-screen
  popups.
- **Configurable global keyboard shortcuts** (see below).

Bare metal uses `backend-linuxkms`.

## Keyboard shortcuts

Shortcuts are configured in `s-compositor.conf` as `bind = <combo> = <action>`,
e.g.:

```
bind = Super+Return = spawn:alacritty
bind = Super+d = start-menu
bind = Super+q = close-window
bind = Alt+Tab = next-window
bind = Super+1 = workspace:1
bind = Super+Shift+1 = move-to-workspace:1
bind = Super+Equal = volume-up
```

Modifiers are `Super`/`Ctrl`/`Alt`/`Shift`; keys are letters, digits or names
(`Return`, `Space`, `Tab`, `Escape`, `Minus`, `Equal`, `Comma`, `Period`).
Actions: `spawn:<cmd>`, `start-menu`, `launcher`, `settings`, `quick-settings`,
`lock`, `logout`, `close-window`, `next-window`, `prev-window`, `workspace:<n>`,
`move-to-workspace:<n>`, `workspace-next`, `workspace-prev`, `volume-up`,
`volume-down`, `volume-mute`, `snap-left`, `snap-right`, `snap-up`, `snap-down`,
`maximize`. With no `bind` lines, sensible defaults are used (including
`Super`+arrows to tile/maximize the focused window).

## File chooser portal

s-compositor implements `org.freedesktop.impl.portal.FileChooser`, so other
applications can open files through its dialog. To route requests to it, install
the backend declaration and configuration:

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

## Building & running (for humans)

### System prerequisites

You need a Rust toolchain (stable, install via [rustup](https://rustup.rs)) and a
few system libraries. The renderer is Skia (built from source by `skia-bindings`,
which needs a C/C++ toolchain), and the seat/keymap handling needs `libxkbcommon`.

On Debian/Ubuntu:

```sh
sudo apt install build-essential clang libxkbcommon-dev libfontconfig-1-dev \
    libudev-dev libseat-dev libinput-dev libgbm-dev libdrm-dev libpulse-dev
```

The `libudev`/`libseat`/`libinput`/`libgbm`/`libdrm` packages are needed because
the bare-metal `backend-linuxkms` backend is enabled by default (so the shell can
run directly on a TTY without an X11/Wayland session).

> **Note:** the linker needs the `libxkbcommon.so` *development* symlink, not just
> the runtime `libxkbcommon.so.0`. If you see `error: unable to find library
> -lxkbcommon` at link time, install `libxkbcommon-dev` (the command above) — or,
> if only the runtime lib is present, symlink it:
> `sudo ln -s libxkbcommon.so.0 /usr/lib/x86_64-linux-gnu/libxkbcommon.so`.

### Build & run

```sh
cargo build
cargo run -p s-compositor   # runs nested inside your current X11/Wayland session
```

`cargo run` opens a window for the shell. It then prints the `WAYLAND_DISPLAY` it
created; point a Wayland client at that socket to see it composited:

```sh
WAYLAND_DISPLAY=wayland-1 foot
```

Settings (panel edge/size, theme, accent, background) are reachable from the ⚙
button and persisted to `s-compositor.conf` in the working directory.

## Testing (for agents / CI)

Two layers of verification work **without a display or GPU**:

**1. Unit tests** — pure-logic models (windows, clock formatting, config):

```sh
cargo test
```

**2. Headless UI screenshots** — render the real Slint shell scene with the
software renderer (no display, no GPU) and write PNGs you can inspect:

```sh
cargo run -p s-compositor --example shell_screenshot
```

This produces `shot_top_panel.png` (wide panel — clock on one line),
`shot_right_panel.png` (narrow panel — clock stacked, eyes looking toward the
pointer) and `shot_settings.png` (the Settings dialog). It is the quickest way to
confirm a UI change visually after editing anything under `ui/`. The example lives
in `crates/s-compositor/examples/shell_screenshot.rs`; add cases there (set
properties, dispatch `WindowEvent`s, call `take_snapshot`) to cover new UI. The
generated `shot_*.png` files are git-ignored.

> The same `libxkbcommon` dev-symlink note from above applies: linking the
> example (or any binary) needs `libxkbcommon.so`.

## License

MIT OR Apache-2.0
