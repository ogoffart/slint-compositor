# slick

A Wayland desktop shell written in Rust, using [Slint](https://slint.dev) for the
UI and [Smithay](https://smithay.github.io/) for the Wayland protocol.

## Architecture

slick inverts the usual Smithay layering: **Slint owns rendering, output, input
and the main event loop** (via its own `backend-winit` for nested development and
`backend-linuxkms` for bare metal), while **Smithay is used purely as the Wayland
protocol engine** — it manages clients, surfaces, buffers, `xdg-shell`,
`wlr-layer-shell` and the seat, but does not present anything itself.

Client windows are imported into GL textures and shown as Slint `Image`s; the
panel, taskbar and tray are ordinary Slint UI in the same scene. This keeps the
whole desktop in one Slint scene graph.

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
| `slick` | Binary: wires the Slint UI + the Wayland thread together. |
| `slick-wayland` | Smithay protocol engine (no rendering). Unit-testable logic. |
| `slick-render` | GL buffer-import bridge (shm MVP, then dmabuf). |
| `slick-shell` | Plain-Rust data models (windows, workspaces, clock). |
| `slick-tray` | System tray (SNI) host — stub for now. |

The Slint UI lives in `ui/` (`desktop.slint`, `panel.slint`, `clock.slint`).

## Status

Early scaffolding. The compositor advertises the core Wayland globals
(`wl_compositor`, `xdg_shell`, `wl_shm`, `wl_seat`, outputs) on an auto-selected
socket, and the Slint panel docks to the right edge with a live clock. Client
window rendering, input forwarding, taskbar, tray and virtual-desktop switching
are tracked as milestones M2–M10 (see the project plan).

## Building & running

```sh
cargo build
cargo test          # pure-logic unit tests (no display needed)
cargo run -p slick  # requires a display (runs nested under your X11/Wayland session)
```

Once running, it prints the `WAYLAND_DISPLAY` it created; point a client at it:

```sh
WAYLAND_DISPLAY=wayland-1 foot
```

## License

MIT OR Apache-2.0
