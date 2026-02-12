# Raven

Raven is a tiling Wayland compositor written in Rust using [Smithay](https://github.com/Smithay/smithay).

It supports both:
- Nested mode (inside an existing Wayland/X11 session via Winit)
- Standalone mode on real hardware (DRM/KMS + libinput + libseat)

Project status: alpha.

## Features

- Master/stack tiling layout with configurable gaps and border size
- 10 workspaces with built-in `Main+1..0` switching and `Main+Shift+1..0` move
- Fullscreen toggle, focus cycling, close focused window
- Layer-shell integration (Waybar, launchers, notifications) with reserved space handling
- Runtime config reload (`Main+Shift+R` by default)
- Lua config at `~/.config/raven/config.lua`
- Hyprland-style config compatibility (subset)
- Window rules (`class`/`app_id`/`title` match with workspace/floating/fullscreen/focus actions)
- Per-output monitor configuration (mode, refresh, scale, transform, position, enable/disable)
- Wallpaper restore flow (external command, default `waypaper --restore`) with optional legacy `swww` mode
- `no_csd` mode with server decoration preference + environment/spawn overrides
- Foreign toplevel management (`zwlr_foreign_toplevel_management_v1`)
- Ext workspace protocol (`ext_workspace_v1`)
- WLR screencopy protocol support

## Quick Start (Nix)

```bash
nix develop
cargo run
```

Run nested explicitly:

```bash
cargo run -- --winit
```

Spawn an app on startup:

```bash
cargo run -- foot
```

Debug logs:

```bash
RUST_LOG=debug cargo run
```

## Requirements (non-Nix)

If you are not using `nix develop`, Raven expects at minimum:
- Rust toolchain (`cargo`, `rustc`)
- A working Wayland/DRM graphics stack for Smithay backends
- `lua` in `PATH` (Raven evaluates `config.lua` via the `lua` binary)

## Runtime Behavior

- Raven auto-selects backend:
  - Uses Winit when `WAYLAND_DISPLAY` or `DISPLAY` is present.
  - Uses DRM/KMS when running from a bare TTY.
- Logs are written to `log/raven.log`.
- `swww-daemon` output is written to `log/swww-daemon.log` (when used).

### CLI Commands

When running inside a Raven session:
- `raven clients` prints a Hyprland-style client list (class/app_id, title, workspace, mapped, floating, fullscreen, focused).
- `raven reload` triggers runtime config reload through IPC.

## Configuration

### Location

- `~/.config/raven/config.lua` (or `$XDG_CONFIG_HOME/raven/config.lua`)
- If missing: Raven creates a default config automatically.
- If empty: Raven writes the default config automatically.

### Default Style

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
    { combo = "Main+D", action = "exec", command = "fuzzel" },
    { combo = "Main+C", action = "close_window" },
    { combo = "Main+F", action = "fullscreen" },
    { combo = "Main+V", action = "toggle_floating" },
    { combo = "Main+J", action = "focus_next" },
    { combo = "Main+K", action = "focus_prev" },
    { combo = "Main+Shift+R", action = "reload_config" },
  },

  monitors = {
    ["eDP-1"] = {
      mode = "1920x1080@120.030",
      scale = 2,
      transform = "normal",
      position = { x = 0, y = 0 },
    },
  },

  autostart = { "waybar", "mako" },

  wallpaper = {
    enabled = false,
    restore_command = "waypaper --restore",
  },
}
```

### Supported Config Shapes

- Structured Lua style:
  - `general`, `keybindings`, `monitors`, `autostart`, `wallpaper`
- String keybind list style:
  - `keybinds = { "Super+X exec firefox", ... }`
- Function helper style:
  - `keys()` with `bind("Mod4", "j", focus_next)` and `spawn(...)`
- Hyprland-like style (subset):
  - `bind = ...`, `exec-once = ...`, `general { ... }`, `input { ... }`

### Window Rules

Window rules support:
- matchers: `class`, `app_id`, `title`
- actions: `workspace`, `floating`, `fullscreen`, `focus`, `width`, `height`

Example:

```lua
window_rules = {
  { class = "mpv", floating = true, width = 1280, height = 720 },
}
```

### Keybind Actions

Supported actions:
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

## Monitor Configuration Notes

- Configure outputs by connector name (for example `eDP-1`, `HDMI-A-1`, `DP-1`).
- You can define monitors as:
  - Keyed table: `monitors = { ["eDP-1"] = { ... } }`
  - Array form: `monitors = { { name = "eDP-1", ... } }`
- Use either:
  - `mode = "<width>x<height>@<refresh>"` (or without refresh)
  - Or explicit `width` + `height` (+ optional `refresh_hz`)
- Use `off = true` (or `enabled = false`) to disable an output.
- Check `log/raven.log` for output initialization lines to confirm names.

## Wallpaper Configuration Notes

- Recommended mode: set `wallpaper.restore_command` (default `waypaper --restore`).
- Legacy mode: set `restore_command = ""` and provide `image`, `resize`, `transition_type`, `transition_duration`.
- Requires the corresponding external tools in `PATH` (`waypaper`, or `swww`/`swww-daemon` for legacy mode).

## Limitations

- Some parsed actions are placeholders (`resize_left`, `resize_right`, `swap_master`).
- This project is still alpha; expect behavior changes.

## Development Notes

- Main entrypoint: `src/main.rs`
- Compositor state and runtime logic: `src/state.rs`
- Config loading and parsing: `src/config.rs`
- Input handling: `src/input.rs`
- Backends:
  - Nested: `src/backend/winit.rs`
  - DRM/KMS: `src/backend/udev.rs`
