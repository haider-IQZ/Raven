use std::{
    collections::HashSet,
    io::{Read, Write},
    os::unix::net::UnixStream,
};

use smithay::{
    reexports::wayland_server::Resource,
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};

use super::Raven;

fn write_ipc_response(stream: &mut UnixStream, message: &str) {
    if let Err(err) = stream.write_all(message.as_bytes()) {
        tracing::warn!("failed to write ipc response: {err}");
    }
}

pub(super) fn handle_ipc_stream(state: &mut Raven, stream: &mut UnixStream) {
    let mut request = String::new();
    if let Err(err) = stream.read_to_string(&mut request) {
        write_ipc_response(stream, &format!("error: failed to read request: {err}\n"));
        return;
    }

    match request.trim() {
        "clients" => {
            let output = render_clients_report(state);
            write_ipc_response(stream, &output);
        }
        "monitors" => {
            let output = render_monitors_report(state);
            write_ipc_response(stream, &output);
        }
        "reload" => match state.reload_config() {
            Ok(()) => write_ipc_response(stream, "ok\n"),
            Err(err) => write_ipc_response(stream, &format!("error: {err}\n")),
        },
        "" => {
            write_ipc_response(
                stream,
                "error: empty command (supported: clients, monitors, reload)\n",
            );
        }
        other => {
            write_ipc_response(
                stream,
                &format!(
                    "error: unsupported command `{other}` (supported: clients, monitors, reload)\n"
                ),
            );
        }
    }
}

pub(super) fn render_clients_report(state: &Raven) -> String {
    let focused_surface = state
        .seat
        .get_keyboard()
        .and_then(|keyboard| keyboard.current_focus());

    let mut seen_surfaces = HashSet::new();
    let mut windows = Vec::new();
    for window in state.workspace_windows().chain(state.space.elements()) {
        let Some(toplevel) = window.toplevel() else {
            continue;
        };
        let surface = toplevel.wl_surface();
        if seen_surfaces.insert(surface.clone()) {
            windows.push(window.clone());
        }
    }

    if windows.is_empty() {
        return "No clients.\n".to_owned();
    }

    let mut out = String::new();
    for (index, window) in windows.iter().enumerate() {
        let Some(toplevel) = window.toplevel() else {
            continue;
        };
        let wl_surface = toplevel.wl_surface().clone();

        let (app_id, title) = with_states(&wl_surface, |states| {
            let role = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .expect("xdg toplevel role data missing")
                .lock()
                .expect("xdg toplevel role lock poisoned");
            (role.app_id.clone(), role.title.clone())
        });

        let workspace = state
            .workspace_index_for_window(window)
            .map(|idx| idx + 1)
            .unwrap_or(state.current_workspace + 1);

        let class = app_id.as_deref().unwrap_or("<unknown>");
        let title = title.as_deref().unwrap_or("<untitled>");
        let focused = focused_surface.as_ref() == Some(&wl_surface);
        let mapped = state.is_window_mapped(window);
        let floating = state.is_window_floating(window);
        let fullscreen = state.window_effective_fullscreen_state(window);
        let surface_id = format!("{:?}", wl_surface.id());

        out.push_str(&format!("Client {}:\n", index + 1));
        out.push_str(&format!("  surface: {surface_id}\n"));
        out.push_str(&format!("  class: {class}\n"));
        out.push_str(&format!("  title: {title}\n"));
        out.push_str(&format!("  workspace: {workspace}\n"));
        out.push_str(&format!("  mapped: {mapped}\n"));
        out.push_str(&format!("  floating: {floating}\n"));
        out.push_str(&format!("  fullscreen: {fullscreen}\n"));
        out.push_str(&format!("  focused: {focused}\n"));
        out.push('\n');
    }

    out
}

pub(super) fn render_monitors_report(state: &Raven) -> String {
    let mut outputs: Vec<_> = state.space.outputs().cloned().collect();
    if outputs.is_empty() {
        return "No monitors.\n".to_owned();
    }

    outputs.sort_by_key(|output| {
        state
            .space
            .output_geometry(output)
            .map(|geo| (geo.loc.x, geo.loc.y))
            .unwrap_or((i32::MAX, i32::MAX))
    });

    let mut out = String::new();
    for (index, output) in outputs.iter().enumerate() {
        out.push_str(&format!("Monitor {}:\n", index + 1));
        out.push_str(&format!("  name: {}\n", output.name()));

        if let Some(mode) = output.current_mode() {
            if mode.refresh > 0 {
                out.push_str(&format!(
                    "  mode: {}x{}@{:.3}\n",
                    mode.size.w,
                    mode.size.h,
                    mode.refresh as f64 / 1000.0
                ));
            } else {
                out.push_str(&format!("  mode: {}x{}\n", mode.size.w, mode.size.h));
            }
        } else {
            out.push_str("  mode: <unknown>\n");
        }

        if let Some(geo) = state.space.output_geometry(output) {
            out.push_str(&format!("  position: {}, {}\n", geo.loc.x, geo.loc.y));
            out.push_str(&format!("  logical_size: {}x{}\n", geo.size.w, geo.size.h));
        } else {
            out.push_str("  position: <unknown>\n");
            out.push_str("  logical_size: <unknown>\n");
        }

        out.push_str(&format!(
            "  scale: {:.3}\n",
            output.current_scale().fractional_scale()
        ));
        out.push('\n');
    }

    out
}
