# 🐦‍⬛ Raven

> A Wayland compositor I built for myself. 

---

## What is this

It's a tiling Wayland compositor written in Rust, configured in Lua, built on [Smithay](https://github.com/Smithay/smithay).

I made it because I wanted a compositor that does exactly what I want and nothing else. No animations that look like a jelly fish having a stroke. No 47 options for how rounded your corners are. Just a window manager that gets out of the way and lets me be sad in peace.

It's fast. It's minimal. It has Lua config so I can change things at 2am without recompiling anything.

---

## Why not just use Hyprland / Sway / [insert compositor]

I tried. They were fi:ne. I just wanted to build my own thing and now I have one. This is that thing.

---

## Does it work

Yeah. I use it daily. It has not eaten any of my files yet.

It's alpha software though, which is a fancy way of saying: *works on my machine, godspeed on yours.*

---

## Features

Things Raven actually does:

- **Master/stack tiling** — windows go where they're told
- **10 workspaces** — one for every project I'll never finish
- **Fullscreen & floating** — for when tiling feels like a personal attack
- **Lua config** at `~/.config/raven/config.lua` — readable, hot-reloadable, civilized
- **Window rules** — tell specific apps where to go (and they actually go there)
- **Per-monitor config** — scales, modes, transforms, positions, the whole thing
- **Layer-shell** — Waybar, launchers, notifications all work
- **Xwayland** via xwayland-satellite — for the apps still living in 2009
- **Hot config reload** — save the file, it applies. no keybind, no restart, just works
- **IPC CLI** — `raven clients`, `raven monitors`, `raven reload`
- **WLR screencopy** — screenshots work, yes
- **Wallpaper** via swww — because a black desktop is a cry for help. use waypaper on top of it or set it manually via terminal, whatever you prefer

---

## Quick Start

```bash
nix develop
cargo run
```

That's it. If you don't use Nix, you'll need `rustc`, `cargo`, and a working graphics stack. You probably know what you're doing if you're reading a Smithay compositor README at this hour.

```bash
# Nested (inside an existing session, for testing)
cargo run -- --winit

# Native (on real hardware, living dangerously)
cargo run -- --drm
```

---

## Config

Raven writes a default config automatically if you don't have one. I got tired of seeing people open issues saying it didn't start — and honestly it's not even their fault, half the window managers out there dump their default config somewhere in `/etc` and then expect you to just know that. you don't know that. nobody knows that. Raven just writes the file to `~/.config/raven/config.lua` and moves on with its life.

Also the config is **hot-reloaded**. save the file, changes apply immediately. no keybind, no restart, nothing. just save.

```lua
return {
  general = {
    modkey = "Super",
    terminal = "foot",
    launcher = "fuzzel",
    focus_follow_mouse = true,
    gap_size = 8,
    border_size = 0,       -- borders are for people with opinions
  },

  keybindings = {
    { combo = "Main+Q", action = "exec", command = "foot" },
    { combo = "Main+D", action = "exec", command = "fuzzel" },
    { combo = "Main+F", action = "fullscreen" },
    { combo = "Main+1", action = "workspace", arg = "1" },
  },

  autostart = { "waybar", "mako" },
}
```

Full config docs are in the wiki. Or just read `config.rs`. It's not that long.

---

## Default Keybindings

| Combo | What happens |
|---|---|
| `Main+Q` | Terminal |
| `Main+D` | Launcher |
| `Main+C` | Close focused window (diplomatically) |
| `Main+F` | Fullscreen |
| `Main+V` | Toggle floating |
| `Main+J / K` | Focus next / previous |
| `Main+1..0` | Switch workspace |
| `Main+Shift+1..0` | Move window to workspace |
| `Main+Shift+Q` | Quit |

---

## Project Status

**Alpha.** I'm one person. I built this for myself. It does what I need it to do.

If something's broken for you and you actually want to help — open an issue and I'll look into what's wrong. that's it. no discord, no community, no roadmap. just a github issue and me eventually reading it.

---

## Source Layout

```
src/
  main.rs       — starts things
  state.rs      — the big struct that knows everything
  config.rs     — parses your Lua so you don't have to think about it
  input.rs      — keyboards, mice, your problems
  backend/
    winit.rs    — nested mode
    udev.rs     — real hardware mode
```

---

## License

Check the repo. It's there.

---

*Built with boredom.*
