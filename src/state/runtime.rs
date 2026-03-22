use std::process::Command;

use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as XdgDecorationMode;

use crate::{CompositorError, config};

use super::Raven;

pub(super) fn spawn_command(state: &Raven, command: &str) {
    if command.trim().is_empty() {
        return;
    }

    let command = state.apply_no_csd_spawn_overrides(command);
    let command = state.apply_wayland_browser_spawn_overrides(&command);
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&command);
    state.apply_wayland_child_env(&mut cmd);

    if let Err(err) = cmd.spawn() {
        tracing::warn!(command = %command, "failed to spawn command: {err}");
    }
}

pub(super) fn run_startup_tasks(state: &mut Raven) {
    tracing::info!(
        output_count = state.space.outputs().count(),
        socket = ?state.socket_name,
        "running startup tasks"
    );
    if state.ensure_xwayland_display() {
        state.sync_activation_environment();
    }
    state.log_xwayland_satellite_context("startup");
    state.maintain_xwayland_satellite();
    state.kick_portal_services_async();
    run_autostart_commands(state);
    state.ensure_waypaper_swww_daemon();
    state.apply_wallpaper();
    crate::backend::udev::queue_redraw_all(state);
}

pub(super) fn preferred_decoration_mode(state: &Raven) -> XdgDecorationMode {
    if state.config.no_csd {
        XdgDecorationMode::ServerSide
    } else {
        XdgDecorationMode::ClientSide
    }
}

pub(super) fn apply_decoration_preferences(state: &Raven) {
    let mode = preferred_decoration_mode(state);
    for window in state.space.elements() {
        let Some(toplevel) = window.toplevel() else {
            continue;
        };

        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });

        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

pub(super) fn run_autostart_commands(state: &mut Raven) {
    if state.autostart_started {
        return;
    }
    state.autostart_started = true;

    for command in &state.config.autostart {
        tracing::info!(command, "starting autostart command");
        state.spawn_command(command);
    }
}

pub(super) fn reload_config(state: &mut Raven) -> Result<(), CompositorError> {
    let config = config::load_from_path(&state.config_path)?;
    config::apply_environment(&config);
    state.config = config;
    state.ensure_xwayland_display();
    state.sync_activation_environment();
    state.log_xwayland_satellite_context("reload");
    state.maintain_xwayland_satellite();
    apply_decoration_preferences(state);

    if state.udev_data.is_some() {
        crate::backend::udev::reload_cursor_theme(state);
    }

    state.apply_layout()?;
    state.apply_wallpaper();
    tracing::info!(path = %state.config_path.display(), "reloaded config.lua");
    Ok(())
}
