# Raven

> A fast, Lua-configured Wayland compositor built in Rust on top of [Smithay](https://github.com/Smithay/smithay).

[![Status](https://img.shields.io/badge/status-alpha-orange)](#project-status)
[![License](https://img.shields.io/badge/license-GPL--3.0-blue)](./Cargo.toml)
[![Language](https://img.shields.io/badge/language-rust-black)](https://www.rust-lang.org/)

Raven supports:
- Nested sessions (inside an existing Wayland/X11 session via Winit)
- Native sessions on real hardware (DRM/KMS + libinput + libseat)

## Feature Highlights

- Master/stack tiling layout with configurable gaps and borders
- Fullscreen, floating toggle, focus cycling, close focused window
- 10 workspaces with built-in `Main+1..0` switch and `Main+Shift+1..0` move
- Runtime config reload (`Main+Shift+R`)
- Lua config (`~/.config/raven/config.lua`)
- Window rules (`class` / `app_id` / `title` -> workspace/floating/fullscreen/focus/size)
- Per-monitor config (mode/scale/transform/position/enable-disable)
- Layer-shell support (Waybar, launchers, notifications) with reserved space handling
- Xwayland via `xwayland-satellite` (auto start + `DISPLAY` export)
- Wallpaper restore flow (default `waypaper --restore`) + legacy `swww` mode
- Foreign toplevel management (`zwlr_foreign_toplevel_management_v1`)
- Ext workspace protocol (`ext_workspace_v1`)
- WLR screencopy protocol

## Quick Start (Nix)

```bash
nix develop
cargo run
```

Run mode controls:

```bash
# Force nested
cargo run -- --winit

# Force native
cargo run -- --drm
```

Spawn an app on startup:

```bash
cargo run -- foot
```

Debug logging:

```bash
RUST_LOG=debug cargo run
```

Scanout behavior:
- Default: enabled (performance-first)
- Disable explicitly when troubleshooting:

```bash
RAVEN_DISABLE_SCANOUT=1 cargo run -- --drm
```

## Requirements (Non-Nix)

- `rustc` + `cargo`
- Working Wayland/DRM graphics stack for Smithay backends
- `lua` in `PATH` (Raven evaluates `config.lua` through the Lua binary)

## CLI Commands

When Raven is running, these commands talk to Raven IPC:

| Command | Description |
| --- | --- |
| `raven clients` | Prints client list (class/app_id, title, workspace, mapped, floating, fullscreen, focused) |
| `raven monitors` | Prints active monitor names, mode, position, logical size, and scale |
| `raven reload` | Reloads `config.lua` at runtime |

## Configuration

Config file location:
- `~/.config/raven/config.lua`
- or `$XDG_CONFIG_HOME/raven/config.lua`

If the file is missing or empty, Raven writes a default config automatically.

### Starter Config

```lua
return {
  general = {
    modkey = "Super",
    terminal = "foot",
    launcher = "fuzzel",
    focus_follow_mouse = true,
    no_csd = true,
    gap_size = 8,
    border_size = 0,
  },

  keybindings = {
    { combo = "Main+Q", action = "exec", command = "foot" },
    { combo = "Main+X", action = "exec", command = "firefox" },
    { combo = "Main+D", action = "exec", command = "fuzzel" },
    { combo = "Main+F", action = "fullscreen" },
    { combo = "Main+Shift+R", action = "reload_config" },
    { combo = "Main+1", action = "workspace", arg = "1" },
    { combo = "Main+Shift+1", action = "movetoworkspace", arg = "1" },
  },

  monitors = {
    -- Keep empty for auto mode, or define monitors by name.
    ["eDP-1"] = { mode = "1920x1080@120.030", scale = 1.0, transform = "normal" },
  },

  autostart = { "waybar", "mako" },

  wallpaper = {
    enabled = false,
    restore_command = "waypaper --restore",
  },
}
```

## Monitor Configuration

- Keep `monitors = {}` empty for fully automatic output setup
- Discover output names with `raven monitors`
- Recommended form:
  - `monitors = { ["eDP-1"] = { ... } }`
- Array form also works:
  - `monitors = { { name = "eDP-1", ... } }`
- Use exactly one sizing style per monitor:
  - `mode = "1920x1080@120.030"` (or `mode = "1920x1080"`)
  - `width = 1920`, `height = 1080`, optional `refresh_hz = 120.030` (aliases: `refresh`, `hz`)
- Do not mix `mode` with `width`/`height`/`refresh_hz` in the same entry
- Disable an output with `off = true` (same as `enabled = false`)
- Position can be:
  - `position = { x = 1920, y = 0 }`
  - `x = 1920, y = 0`

Example:

```lua
monitors = {
  ["eDP-1"] = {
    mode = "1920x1080@120.030",
    scale = 1.25,
    transform = "normal",
    position = { x = 0, y = 0 },
  },
  ["DP-1"] = {
    width = 2560,
    height = 1440,
    refresh_hz = 165,
    x = 1920,
    y = 0,
  },
  ["HDMI-A-1"] = { off = true },
}
```

## Window Rules

Matchers:
- `class`
- `app_id`
- `title`

Actions:
- `workspace`
- `floating`
- `fullscreen`
- `focus`
- `width`
- `height`

Example:

```lua
window_rules = {
  { class = "Firefox", workspace = "2" },
  { class = "mpv", floating = true, width = 1280, height = 720 },
}
```

## Keybind Actions

Supported:
- `exec <command>`
- `terminal`
- `launcher`
- `close` / `close_window`
- `fullscreen`
- `toggle_floating`
- `focus_next`
- `focus_prev` / `focus_previous`
- `reload_config`
- `quit`
- `workspace <1..10>`
- `movetoworkspace <1..10>`

Parsed but currently unimplemented:
- `resize_left`
- `resize_right`
- `swap_master`

## Wallpaper Notes

- Recommended mode: `wallpaper.restore_command` (default `waypaper --restore`)
- Legacy mode: set `restore_command = ""`, then provide `image`, `resize`, `transition_type`, `transition_duration`
- External tools must exist in `PATH` (`waypaper`, or `swww`/`swww-daemon` for legacy mode)

## Xwayland Notes

- Install `xwayland-satellite`
- Keep `xwayland.enabled = true`
- Customize binary and display with `xwayland.path` and `xwayland.display`

## Runtime Behavior

- Auto backend selection:
  - Winit when `WAYLAND_DISPLAY` or `DISPLAY` exists
  - DRM/KMS on bare TTY
- Exports:
  - `XDG_CURRENT_DESKTOP=raven`
  - `XDG_SESSION_DESKTOP=raven`
- Auto portal setup:
  - Creates `~/.config/xdg-desktop-portal/raven-portals.conf` when missing
  - Starts available `xdg-desktop-portal*` units non-blocking at startup
- Logs:
  - Raven logs: `log/raven.log`
  - swww daemon logs: `log/swww-daemon.log` (when used)

## Project Status

Raven is currently **alpha**. Behavior and internals can still change quickly.

## Development Notes

- Entrypoint: `src/main.rs`
- Core compositor state/runtime: `src/state.rs`
- Config loading/parsing: `src/config.rs`
- Input handling: `src/input.rs`
- Backends:
  - Winit: `src/backend/winit.rs`
  - DRM/KMS: `src/backend/udev.rs`
