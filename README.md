# Raven

Minimal Wayland compositor built with Rust and [Smithay](https://github.com/Smithay/smithay).

## Development

```bash
nix develop
cargo run
```

Or spawn a program directly:

```bash
cargo run -- foot
```

## Keybindings

- Defaults come from `~/.config/raven/config.lua` (`main_key` + `keybinds` list)
- Workspace shortcuts are built in with your `main_key`:
  - `Main+1..0` - Switch workspace (0 = workspace 10)
  - `Main+Shift+1..0` - Send focused window to workspace (0 = workspace 10)
- `Main+Click` drag for moving windows is also tied to `main_key`

## Configuration

- Config path: `~/.config/raven/config.lua` (or `$XDG_CONFIG_HOME/raven/config.lua`)
- If missing, Raven auto-creates a default file on startup
- If file exists but is empty, Raven auto-writes the default config
- Reload config at runtime with `Main+Shift+R` by default
- Supports two keybind styles:
  - `keybinds = { "Super+X exec firefox", ... }`
  - `keys()` + `bind("Mod4 Shift", "Return", spawn(terminal))`
- Preferred section style is supported:
  - `general = { ... }`
  - `keybindings = { { combo = "Main+X", action = "exec", command = "firefox" }, ... }`
  - `window_rules = { ... }` (currently parsed but not applied)
- `autostart = { "waybar", "mako", ... }` runs once at compositor startup
- `monitors = { ... }` configures per-output mode/refresh/position/scale/transform
  - Example:
    - `monitors = { ["eDP-1"] = { mode = "1920x1080@120.030", scale = 2, transform = "normal", position = { x = 1280, y = 0 } } }`
  - Configure outputs by name; you can find active output names in `log/raven.log` (`Output initialized ...`)
  - `off = true` disables an output; Raven keeps at least one output enabled to avoid black screen
- `wallpaper = { enabled = true, restore_command = "waypaper --restore" }` restores wallpaper via any external tool
- Default `waypaper --restore` is auto-bootstrapped with `swww-daemon` by Raven
- Legacy `swww` mode still works if `restore_command` is empty and `wallpaper.image` is set
  - Example:
    - `wallpaper = { enabled = true, restore_command = "", image = "~/Pictures/wall.jpg", resize = "crop", transition_type = "simple", transition_duration = 0.7 }`
- `general.focus_follow_mouse = true/false` controls keyboard focus on pointer hover
- Also accepts Hyprland-like `bind = $mod, X, exec, firefox` lines (subset)
- Hypr-style `exec-once = ...` lines are imported as `autostart` commands
- Supported actions: `exec`, `terminal`, `launcher`, `close_window`, `fullscreen`, `focus_next`, `focus_prev`, `reload_config`, `quit`
- Hypr-style actions supported: `exec`, `killactive`, `movefocus`, `workspace`, `movetoworkspace`, `quit`
- `resize_left`, `resize_right`, and `swap_master` parse but are not implemented yet
- External wallpaper command mode requires the configured tool in `PATH` (default: `waypaper`)
- Legacy `swww` mode requires `swww` and `swww-daemon` in `PATH`
- Runtime logs are written to `log/raven.log` in the project root
- `swww-daemon` stderr/stdout is captured in `log/swww-daemon.log`

## Roadmap

1. Discuss codebase layout and architecture
2. Implement core features (tiling, workspaces, etc.)
3. Expand configuration system
