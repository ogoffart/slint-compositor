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

The Slint UI lives in `ui/` (`desktop.slint`, `panel.slint`, `clock.slint`).

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
- A panel dockable to any edge (configurable size) with a live clock, a
  **command launcher** (▶), a **taskbar**, and a **settings** button (⚙).
- **Theming**: accent colour and light/dark scheme, **persisted** to
  `s-compositor.conf` in the working directory and **published over the XDG
  `Settings` portal** (`org.freedesktop.appearance`) so apps follow it.
- A reusable **file dialog** (editable path, keyboard navigation), also exposed
  as an XDG **FileChooser portal** backend so other apps open files through it.

Virtual-desktop switching and the system tray are the next milestones. Bare
metal uses `backend-linuxkms`.

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

## Building & running

```sh
cargo build
cargo test          # pure-logic unit tests (no display needed)
cargo run -p s-compositor  # requires a display (runs nested under your X11/Wayland session)
```

Once running, it prints the `WAYLAND_DISPLAY` it created; point a client at it:

```sh
WAYLAND_DISPLAY=wayland-1 foot
```

## License

MIT OR Apache-2.0
