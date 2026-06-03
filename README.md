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
  draws the title bar (title + close button) and border itself.
- Right-edge panel with a live clock and a **command launcher** (▶).

Input forwarding, taskbar, tray and virtual-desktop switching are the next
milestones (M3–M10; see the project plan). Bare metal uses `backend-linuxkms`.

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
