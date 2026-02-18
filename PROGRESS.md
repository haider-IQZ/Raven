# Raven Compositor - Development Progress

## Project Overview

Raven is a tiling Wayland compositor written in Rust using Smithay 0.7.0. It runs both nested (inside an existing Wayland/X11 session via Winit) and standalone on bare hardware (via DRM/KMS). Configuration is done through Lua (`~/.config/raven/config.lua`) with Hyprland-style config compatibility.

**Project path:** `/home/soka/projects/Raven`
**Package name:** `raven`
**Total source:** ~6,600 lines of Rust across 22 files

---

## Architecture Overview

```
src/
├── main.rs              — Entry point, logging, backend auto-detection
├── lib.rs               — Module exports
├── state.rs             — Raven (core compositor state), workspace logic, wallpaper, autostart
├── config.rs            — Lua config loader, Hyprland-style parser, keybind system
├── cursor.rs            — Xcursor theme loading, PointerElement, fallback cursor
├── input.rs             — Keyboard/pointer/axis event handling, keybind dispatch
├── action.rs            — FocusNext/FocusPrevious actions
├── errors.rs            — CompositorError enum, Result type alias
├── backend/
│   ├── mod.rs           — Exports winit + udev
│   ├── winit.rs         — Nested backend (runs in a window), screencopy support
│   └── udev.rs          — DRM/KMS standalone backend, multi-GPU, cursor rendering
├── handlers/
│   ├── mod.rs           — Seat, DataDevice, Output, DnD, Dmabuf, FractionalScale, Screencopy delegates
│   ├── compositor.rs    — CompositorHandler, BufferHandler, ShmHandler
│   ├── xdg_shell.rs     — XDG toplevel/popup, decoration, maximize, move/resize requests
│   └── layer_shell.rs   — WLR layer shell (status bars, launchers, notifications)
├── grabs/
│   ├── mod.rs           — Exports
│   ├── move_grab.rs     — Pointer grab for window dragging (Main+LeftClick)
│   └── resize_grab.rs   — Pointer grab for client-initiated window resize
├── layout/
│   ├── mod.rs           — Layout trait, LayoutBox, GapConfig, WindowGeometry
│   └── tiling.rs        — Master-stack tiling layout (dwm-style)
├── protocols/
│   └── wlr_screencopy.rs — WLR screencopy protocol implementation
└── workspace/
    └── mod.rs           — Workspace struct (placeholder, logic lives in state.rs)
```

---

## Feature Summary

### Backends
| Feature | Status |
|---------|--------|
| Winit backend (nested mode) | Working |
| DRM/KMS backend (standalone) | Working |
| Backend auto-detection | Working — uses Winit if `WAYLAND_DISPLAY`/`DISPLAY` set, DRM otherwise |
| `--winit` CLI flag | Working — forces nested mode |
| TTY switching (session pause/resume) | Working |
| Connector hotplug | Working |
| Multi-GPU readiness | Working — uses `GpuManager<GbmGlesBackend>` with `MultiRenderer` |

### Window Management
| Feature | Status |
|---------|--------|
| Master-stack tiling layout | Working — configurable master_factor, num_master, gaps |
| 10 workspaces | Working — `Main+1..0` switch, `Main+Shift+1..0` move window |
| Focus cycling | Working — `Main+J/K` (next/previous) |
| Window close | Working — via keybind |
| Fullscreen toggle | Working — per-window, exclusive |
| Window move (drag) | Working — `Main+LeftClick` |
| Window resize (client-initiated) | Working — via XDG resize_request |
| Smart gaps | Working — single window removes outer gaps |
| Focus follows mouse | Working — configurable |
| Window activation state | Working — XDG Activated state synced on focus change |

### Configuration (`~/.config/raven/config.lua`)
| Feature | Status |
|---------|--------|
| Lua config format | Working — evaluated via external `lua` binary |
| Hyprland-style config | Working — auto-detected, translated to internal format |
| Default config generation | Working — created on first run |
| Runtime reload | Working — `Main+Shift+R` |
| Configurable main key | Working — Super/Alt/Ctrl |
| Custom keybindings | Working — combo + action format (exec, terminal, launcher, close, fullscreen, quit, focus, workspace, etc.) |
| `bind()` / `keys()` API | Working — Lua functions for keybind registration |
| Monitor configuration | Working — per-output name, mode, refresh, scale, transform, position, enabled/disabled |
| Gap configuration | Working — outer/inner horizontal/vertical, shorthand `gap_size` |
| Cursor theme + size | Working — `XCURSOR_THEME` / `XCURSOR_SIZE` env vars set |
| Terminal / launcher config | Working |
| Autostart commands | Working — run after 700ms delay |
| Wallpaper support | Working — via swww (image path + transition), or `waypaper --restore`, or custom command |
| no_csd mode | Working — protocol-level CSD removal (see CSD Removal section below), per-app CLI overrides (alacritty, kitty, wezterm) |
| Environment variables | Working — sets WAYLAND_DISPLAY, XDG_SESSION_TYPE, GDK_BACKEND, QT_QPA_PLATFORM, MOZ_ENABLE_WAYLAND, etc. for child processes |

### Wayland Protocols
| Protocol | Status |
|----------|--------|
| wl_compositor | Working |
| wl_shm | Working |
| wl_seat (keyboard, pointer) | Working |
| wl_output | Working |
| xdg_shell (toplevel, popup) | Working |
| xdg_decoration | Working — per-client filtered global, forced ServerSide when no_csd is on |
| KDE server decoration (`org_kde_kwin_server_decoration`) | Working — per-client filtered global, forced Server mode for GTK3 apps |
| wl_data_device (clipboard) | Working |
| wp_primary_selection | Working |
| zwlr_layer_shell (v1) | Working — status bars, launchers, notifications |
| wp_viewporter | Working |
| wp_fractional_scale | Working |
| linux_dmabuf | Working — GPU buffer import via primary renderer |
| DnD (drag and drop) | Working — pointer DnD grabs |
| zwlr_screencopy | Working (Winit only) |

### Rendering (DRM Backend)
| Feature | Status |
|---------|--------|
| Space + layer surface rendering | Working — `space_render_elements()` |
| Software cursor (xcursor) | Working — themed cursor drawn above windows |
| Cursor hotspot handling | Working — proper offset from CursorImageAttributes |
| Client cursor surfaces | Working — renders client-provided cursor surfaces |
| Fallback cursor | Working — procedurally generated arrow if theme fails |
| Cursor theme reload on config change | Working |
| VBlank-driven render loop | Working — 60% frame duration repaint delay |
| No-damage reschedule | Working — prevents compositor stalling |
| Per-output redraw queuing | Working — prevents excessive redraws on all outputs |
| Early buffer import | Working — pre-imports GPU buffers at commit time |
| Direct scanout | Working — allows fullscreen apps to bypass GPU compositing |
| DmabufFeedback (per-surface) | Not yet |
| Hardware cursor | Not yet |
| Screencopy on DRM | Not yet |

### Logging
| Feature | Status |
|---------|--------|
| Stderr logging | Working — with ANSI colors |
| File logging | Working — `log/raven.log` (non-rolling) |
| swww-daemon logging | Working — `log/swww-daemon.log` |
| Panic hook | Working — panics logged via `tracing::error!` |
| Env filter | Working — `RUST_LOG` support |

---

## Key Config Options

```lua
return {
  general = {
    modkey = "Super",           -- Super / Alt / Ctrl
    terminal = "foot",
    launcher = "fuzzel",
    focus_follow_mouse = true,
    no_csd = true,              -- Force server-side decorations
    gap_size = 8,               -- Shorthand for all gaps
    border_size = 0,
  },
  keybindings = {
    { combo = "Main+Q", action = "exec", command = "foot" },
    { combo = "Main+C", action = "close_window" },
    { combo = "Main+F", action = "fullscreen" },
    { combo = "Main+J", action = "focus_next" },
    { combo = "Main+1", action = "workspace", arg = "1" },
    -- ...
  },
  monitors = {
    ["eDP-1"] = { scale = 2, mode = "1920x1080@60" },
    ["HDMI-A-1"] = { position = { x = 1920, y = 0 } },
  },
  autostart = { "waybar", "mako" },
  wallpaper = {
    enabled = true,
    image = "~/Pictures/wallpaper.jpg",
    -- or: restore_command = "waypaper --restore",
  },
}
```

---

## DRM/KMS Backend Details

### Architecture (`src/backend/udev.rs`, ~1,020 lines)

**Structs:**
- `UdevData` — session, primary GPU, GpuManager, cursor theme, pointer image cache, per-device backends
- `BackendData` — per-GPU: DrmOutputManager, DrmScanner, surfaces, render node
- `SurfaceData` — per-CRTC: Output, DrmOutput, Wayland global

**Custom render element:**
```rust
render_elements! {
    pub UdevRenderElement<R, E> where R: ImportAll + ImportMem;
    Space=SpaceRenderElements<R, E>,
    Pointer=PointerRenderElement<R>,
}
```

**Initialization (`init_udev`):**
1. Create `LibSeatSession`
2. Detect primary GPU via `primary_gpu()` / `all_gpus()`
3. Create `GpuManager` with `GbmGlesBackend`
4. Enumerate devices → `device_added()` for each
5. Set up DmabufState with default feedback
6. Create `LibinputInputBackend`
7. Register calloop sources: libinput, session notifier, udev hotplug

**Monitor config application (`connector_connected`):**
- Matches output name (case-insensitive, handles connector name variants like `DP-1` vs `DP-A-1`)
- Supports: mode selection (width/height/refresh), transform (normal/90/180/270/flipped), scale (integer or fractional), position (x/y), enabled/disabled
- Falls back to preferred mode if config doesn't match

**Render loop:**
- `render_surface()` → cursor elements (front) + space elements (back) → `drm_output.render_frame()` + `queue_frame()`
- `frame_finish()` → `frame_submitted()` → schedule next render at 60% of frame duration
- No-damage path reschedules at full frame duration

**Session handling:**
- `PauseSession` → suspend libinput, pause DRM
- `ActivateSession` → resume libinput, activate DRM, schedule re-render

---

## Cursor System (`src/cursor.rs`)

- `CursorThemeManager` — loads xcursor theme from `XCURSOR_THEME`/`XCURSOR_SIZE` env vars
- `PointerElement` — holds either a `MemoryRenderBuffer` (for xcursor) or a client `WlSurface` cursor
- `PointerRenderElement` — render_elements macro combining Surface + Memory elements
- Animated cursor support — frame selection based on elapsed time
- `fallback_cursor_image()` — procedurally generated 24x24 arrow cursor if theme loading fails

---

## Build & Run

```bash
cd ~/projects/Raven
nix develop

# Nested mode (inside existing compositor)
cargo run -- --winit

# Standalone mode (from bare TTY)
cargo run

# With debug logging
RUST_LOG=debug cargo run

# Spawn a specific app
cargo run -- foot
```

---

## Dependencies

### Rust (Cargo.toml)
- `smithay` (git) — Wayland compositor framework, features: backend_winit, backend_drm, backend_gbm, backend_udev, backend_libinput, backend_session_libseat, backend_egl, use_system_lib, desktop, renderer_glow, renderer_gl, renderer_multi, wayland_frontend
- `smithay-drm-extras` (git) — DrmScanner, display_info
- `tracing` + `tracing-subscriber` (env-filter) + `tracing-appender` — structured logging
- `bitflags` — bit flag operations
- `xcursor` — Xcursor theme parsing

### Native (NixOS)
wayland, libxkbcommon, libGL, libglvnd, libX11, libXcursor, libXrandr, libXi, libinput, seatd, systemdMinimal (libudev), libgbm, mesa, libdisplay-info, lua

---

## CSD Removal Implementation (`no_csd = true`)

Raven uses proper Wayland protocol negotiation to remove Client-Side Decorations from all apps. No hacky environment variables — the protocols handle everything.

### Protocol Stack (3 layers)

**1. xdg-decoration-unstable-v1** (`src/handlers/xdg_shell.rs`, `src/state.rs`)
- For modern apps that support the standard decoration protocol (foot, alacritty via libdecor, etc.)
- Global created with `XdgDecorationState::new_with_filter` — only visible to clients when `no_csd` is enabled
- `new_decoration`: sets `decoration_mode = ServerSide`, sends configure
- `request_mode`: overrides client requests with `ServerSide` when `no_csd` is on (prevents clients requesting CSD back)
- `unset_mode`: resets to compositor preference (ServerSide)

**2. KDE server decoration (`org_kde_kwin_server_decoration`)** (`src/handlers/xdg_shell.rs`, `src/state.rs`)
- For GTK3 apps (waypaper, nvidia-settings, etc.) that use the legacy KDE decoration protocol instead of xdg-decoration
- Global created with `KdeDecorationState::new_with_filter` using `Mode::Server`
- Same per-client filter as xdg-decoration (checks `can_view_decoration_globals`)
- `request_mode`: always responds with `Server` mode when `no_csd` is enabled

**3. XDG tiled states** (`src/handlers/xdg_shell.rs`, `src/state.rs`)
- Sets `TiledLeft/Right/Top/Bottom` on all toplevel surfaces
- Signals to toolkits (GTK4/libadwaita) that the window is in a tiling WM
- Applied in `new_toplevel`, decoration handlers, and `apply_layout`

### Per-client global filtering (`src/state.rs`)
- `ClientState` has `can_view_decoration_globals: bool` set from `config.no_csd`
- Both xdg-decoration and KDE decoration globals use this filter
- When `no_csd` is true, clients see the decoration globals and can negotiate ServerSide
- When `no_csd` is false, decoration globals are hidden and clients use their own CSD

### Critical: `send_configure()` race condition fix
- The xdg-decoration object is created by the client AFTER the initial toplevel configure is sent
- `send_pending_configure()` would silently skip because the decoration mode was already set to ServerSide in `new_toplevel`
- Fix: all decoration handlers use `send_configure()` (unconditional) instead of `send_pending_configure()` (conditional)
- This ensures the decoration configure event reaches the client even if the toplevel state hasn't changed

### Environment variable (minimal)
- Only `QT_WAYLAND_DISABLE_WINDOWDECORATION=1` is set for Qt apps
- All other env hacks removed (`GTK_CSD`, `LIBDECOR_PLUGIN_DIR`, `GLFW_WAYLAND_LIBDECOR`, `SDL_VIDEO_WAYLAND_ALLOW_LIBDECOR`)
- `LIBDECOR_PLUGIN_DIR=/dev/null` was actively harmful — it broke libdecor's ability to negotiate xdg-decoration, causing apps like alacritty to fall back to built-in CSD

### Per-app spawn overrides (`src/state.rs: apply_no_csd_spawn_overrides`)
- Alacritty: `-o window.decorations=None`
- Kitty: `-o hide_window_decorations=yes`
- Wezterm: `--config window_decorations=NONE`
- Only applied when spawning through the compositor's `spawn_command`

### How different apps negotiate
| App type | Protocol used | Example apps |
|----------|--------------|--------------|
| Modern Wayland (SCTK) | xdg-decoration | foot |
| libdecor-based | xdg-decoration (via libdecor) | alacritty (winit) |
| GTK3 | KDE decoration | waypaper, nvidia-settings |
| Qt | QT_WAYLAND_DISABLE_WINDOWDECORATION env | Qt apps |
| GTK4/libadwaita | Tiled states (squares corners, minimizes CSD) | GNOME apps |

---

## Not Yet Implemented

- Multi-monitor workspace logic (outputs stack right, but workspaces are global)
- Hardware cursor (currently software cursor via compositor rendering)
- Window resize keybindings (resize works via client request only)
- Screencopy on DRM backend
- Per-surface DmabufFeedback (scanout optimization)
- DRM lease support (VR headsets)
- Window borders / decorations rendering
- Window rules enforcement (config has the field, not wired up)
- Status bar integration beyond layer shell
- XWayland support
- Animations / transitions
- Output power management (DPMS)
