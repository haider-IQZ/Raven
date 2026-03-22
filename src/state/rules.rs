use smithay::{
    backend::renderer::utils::RendererSurfaceStateUserData,
    desktop::Window,
    reexports::{
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as XdgDecorationMode,
            shell::server::xdg_toplevel,
        },
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::SERIAL_COUNTER,
    wayland::compositor::with_states,
};

use crate::{config::WindowRule, state::NewWindowRuleDecision};

use super::Raven;

pub(super) fn queue_window_rule_recheck_for_surface(state: &mut Raven, surface: &WlSurface) {
    if should_defer_window_rules_for_surface(state, surface) {
        state
            .pending_window_rule_recheck_ids
            .insert(surface.clone());
    }
}

pub(super) fn queue_floating_recenter_for_surface(state: &mut Raven, surface: &WlSurface) {
    state.pending_floating_recenter_ids.insert(surface.clone());
}

pub(super) fn clear_floating_recenter_for_surface(state: &mut Raven, surface: &WlSurface) {
    state.pending_floating_recenter_ids.remove(surface);
}

pub(super) fn clear_window_rule_recheck_for_surface(state: &mut Raven, surface: &WlSurface) {
    state.pending_window_rule_recheck_ids.remove(surface);
}

pub(super) fn queue_initial_configure_for_surface(state: &mut Raven, surface: &WlSurface) {
    state.pending_initial_configure_ids.insert(surface.clone());
}

pub(super) fn clear_initial_configure_for_surface(state: &mut Raven, surface: &WlSurface) {
    state.pending_initial_configure_ids.remove(surface);
}

pub(super) fn queue_initial_configure_idle_for_surface(state: &mut Raven, surface: &WlSurface) {
    if !state.pending_initial_configure_ids.contains(surface) {
        return;
    }
    if !state
        .pending_initial_configure_idle_ids
        .insert(surface.clone())
    {
        return;
    }

    let surface_id = surface.clone();
    state.loop_handle.insert_idle(move |state| {
        state.pending_initial_configure_idle_ids.remove(&surface_id);
        if !surface_id.is_alive() {
            return;
        }
        if !state.pending_initial_configure_ids.contains(&surface_id) {
            return;
        }
        if !state.unmapped_toplevel_ids.contains(&surface_id) {
            return;
        }

        state.send_initial_configure_for_surface(&surface_id);
        state.clear_initial_configure_for_surface(&surface_id);
    });
}

pub(super) fn should_defer_window_rules_for_surface(state: &Raven, surface: &WlSurface) -> bool {
    let (app_id, title) = Raven::surface_app_id_and_title(surface);
    if state.has_window_rule_metadata_gap(app_id.as_deref(), title.as_deref()) {
        return true;
    }

    !state.has_matching_explicit_floating_rule(app_id.as_deref(), title.as_deref())
}

pub(super) fn resolve_window_rules_for_surface(
    state: &Raven,
    surface: &WlSurface,
) -> NewWindowRuleDecision {
    let (app_id, title) = Raven::surface_app_id_and_title(surface);

    let mut decision = NewWindowRuleDecision {
        workspace_index: state.current_workspace,
        floating: false,
        fullscreen: false,
        focus: true,
        width: None,
        height: None,
    };

    for rule in &state.config.window_rules {
        if !rule.matches(app_id.as_deref(), title.as_deref()) {
            continue;
        }
        apply_window_rule_to_decision(rule, &mut decision);
    }

    decision
}

fn apply_window_rule_to_decision(rule: &WindowRule, decision: &mut NewWindowRuleDecision) {
    if let Some(workspace_index) = rule.workspace {
        decision.workspace_index = workspace_index;
    }
    if let Some(floating) = rule.floating {
        decision.floating = floating;
    }
    if let Some(fullscreen) = rule.fullscreen {
        decision.fullscreen = fullscreen;
    }
    if let Some(focus) = rule.focus {
        decision.focus = focus;
    }
    if let Some(width) = rule.width {
        decision.width = Some(width);
    }
    if let Some(height) = rule.height {
        decision.height = Some(height);
    }
}

pub(super) fn apply_window_rule_size_to_window(
    _state: &Raven,
    window: &Window,
    decision: &NewWindowRuleDecision,
) {
    let (Some(width), Some(height)) = (decision.width, decision.height) else {
        return;
    };

    let width = width.clamp(1, i32::MAX as u32) as i32;
    let height = height.clamp(1, i32::MAX as u32) as i32;

    let Some(toplevel) = window.toplevel() else {
        return;
    };
    toplevel.with_pending_state(|state| {
        state.size = Some((width, height).into());
    });
}

pub(super) fn send_initial_configure_for_surface(state: &mut Raven, surface: &WlSurface) {
    let Some(window) = state.window_for_surface(surface) else {
        return;
    };

    let mut decision = resolve_window_rules_for_surface(state, surface);
    let (effective_floating, _, _, _) =
        state.resolve_effective_floating_for_surface(surface, &window, decision.floating);
    let window_has_exclusive_state = state.window_has_exclusive_layout_state(&window);
    decision.floating = if window_has_exclusive_state {
        false
    } else {
        effective_floating
    };

    if let Err(err) = state.move_window_to_workspace_internal(&window, decision.workspace_index) {
        tracing::warn!("failed to move window during initial configure: {err}");
    }

    state.set_window_floating(&window, decision.floating && !window_has_exclusive_state);

    let Some(toplevel) = window.toplevel() else {
        return;
    };

    let mode = state.preferred_decoration_mode();
    let no_csd = state.config.no_csd;
    toplevel.with_pending_state(|pending_state| {
        pending_state.decoration_mode = Some(mode);
        let tiled = (mode == XdgDecorationMode::ServerSide || no_csd)
            && !decision.floating
            && !window_has_exclusive_state;
        if tiled {
            pending_state.states.set(xdg_toplevel::State::TiledLeft);
            pending_state.states.set(xdg_toplevel::State::TiledRight);
            pending_state.states.set(xdg_toplevel::State::TiledTop);
            pending_state.states.set(xdg_toplevel::State::TiledBottom);
        } else {
            pending_state.states.unset(xdg_toplevel::State::TiledLeft);
            pending_state.states.unset(xdg_toplevel::State::TiledRight);
            pending_state.states.unset(xdg_toplevel::State::TiledTop);
            pending_state.states.unset(xdg_toplevel::State::TiledBottom);
        }
    });

    apply_window_rule_size_to_window(state, &window, &decision);

    let visible_on_current_workspace = decision.workspace_index == state.current_workspace;
    if visible_on_current_workspace
        && !decision.floating
        && !decision.fullscreen
        && !window_has_exclusive_state
        && let Some((_, tiled_size, tiled_bounds)) = state.pre_layout_tiled_slot_for_window(&window)
    {
        toplevel.with_pending_state(|state| {
            state.size = Some(tiled_size);
            state.bounds = Some(tiled_bounds);
        });
    }

    let fallback_size = state
        .assigned_rect_for_window(&window)
        .map(|rect| rect.size)
        .or_else(|| Some(state.initial_map_rect_for_window(&window).size));
    state.sync_reported_size_from_pending_state(&window, fallback_size);
    toplevel.send_configure();
}

pub(super) fn maybe_apply_deferred_window_rules(state: &mut Raven, surface: &WlSurface) {
    let surface_id = surface.clone();
    if !state.pending_window_rule_recheck_ids.contains(&surface_id) {
        return;
    }
    let (app_id, title) = Raven::surface_app_id_and_title(surface);

    let Some(window) = state.window_for_surface(surface) else {
        state.pending_window_rule_recheck_ids.remove(&surface_id);
        return;
    };

    let is_mapped =
        smithay::backend::renderer::utils::with_renderer_surface_state(surface, |state| {
            state.buffer().is_some()
        })
        .unwrap_or(false);
    if !is_mapped {
        return;
    }

    let has_buffer = with_states(surface, |states| {
        states
            .data_map
            .get::<RendererSurfaceStateUserData>()
            .and_then(|data| data.lock().ok())
            .and_then(|data| data.buffer_size())
            .is_some()
    });
    if !has_buffer {
        return;
    }

    let mut decision = resolve_window_rules_for_surface(state, surface);
    let (effective_floating, has_explicit_floating_rule, auto_floating, auto_reason) =
        state.resolve_effective_floating_for_surface(surface, &window, decision.floating);
    let window_has_exclusive_state = state.window_has_exclusive_layout_state(&window);
    decision.floating = if window_has_exclusive_state {
        false
    } else {
        effective_floating
    };

    if let Err(err) = state.move_window_to_workspace_internal(&window, decision.workspace_index) {
        tracing::warn!("failed to move window after deferred rule resolution: {err}");
    }
    if let Some(toplevel) = window.toplevel() {
        let mode = state.preferred_decoration_mode();
        let no_csd = state.config.no_csd;
        let fixed_hint_size = if !has_explicit_floating_rule
            && auto_floating
            && decision.width.is_none()
            && decision.height.is_none()
            && !window_has_exclusive_state
        {
            Raven::fixed_hint_size_for_surface(surface)
        } else {
            None
        };
        toplevel.with_pending_state(|pending_state| {
            pending_state.decoration_mode = Some(mode);
            let tiled = (mode == XdgDecorationMode::ServerSide || no_csd)
                && !decision.floating
                && !window_has_exclusive_state;
            if tiled {
                pending_state.states.set(xdg_toplevel::State::TiledLeft);
                pending_state.states.set(xdg_toplevel::State::TiledRight);
                pending_state.states.set(xdg_toplevel::State::TiledTop);
                pending_state.states.set(xdg_toplevel::State::TiledBottom);
            } else {
                pending_state.states.unset(xdg_toplevel::State::TiledLeft);
                pending_state.states.unset(xdg_toplevel::State::TiledRight);
                pending_state.states.unset(xdg_toplevel::State::TiledTop);
                pending_state.states.unset(xdg_toplevel::State::TiledBottom);
            }
            if !has_explicit_floating_rule
                && auto_floating
                && decision.width.is_none()
                && decision.height.is_none()
                && !window_has_exclusive_state
            {
                pending_state.size = fixed_hint_size;
            }
        });
    }
    apply_window_rule_size_to_window(state, &window, &decision);
    if let Some(toplevel) = window.toplevel()
        && toplevel.is_initial_configure_sent()
    {
        let fallback_size = state
            .assigned_rect_for_window(&window)
            .map(|rect| rect.size)
            .or_else(|| Some(state.initial_map_rect_for_window(&window).size));
        state.sync_reported_size_from_pending_state(&window, fallback_size);
        toplevel.send_pending_configure();
    }

    let was_floating = state.is_window_floating(&window);
    state.set_window_floating(&window, decision.floating && !window_has_exclusive_state);
    let on_current_workspace = state.workspace_contains_window(state.current_workspace, &window);
    let tiled_slot = if on_current_workspace
        && !decision.floating
        && !decision.fullscreen
        && !window_has_exclusive_state
    {
        state.pre_layout_tiled_slot_for_window(&window)
    } else {
        None
    };
    if let Some((_, desired_size, desired_bounds)) = tiled_slot
        && let Some(toplevel) = window.toplevel()
    {
        let mut needs_configure = false;
        toplevel.with_pending_state(|state| {
            if state.size != Some(desired_size) {
                state.size = Some(desired_size);
                needs_configure = true;
            }
            if state.bounds != Some(desired_bounds) {
                state.bounds = Some(desired_bounds);
                needs_configure = true;
            }
        });
        if needs_configure && toplevel.is_initial_configure_sent() {
            state.sync_reported_size_from_pending_state(&window, Some(desired_size));
            toplevel.send_pending_configure();
        }
    }
    if decision.floating && state.is_window_mapped(&window) {
        let rect = state.initial_map_rect_for_window(&window);
        state.map_window_to_rect(&window, rect, !was_floating);
        queue_floating_recenter_for_surface(state, surface);
    }

    if decision.fullscreen {
        state.request_fullscreen_enter_on_output(&window, None);
    }

    if let Err(err) = state.apply_layout() {
        tracing::warn!("failed to apply layout after deferred rule resolution: {err}");
    }

    if decision.focus && decision.workspace_index == state.current_workspace {
        state.set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
    }

    let current_geo = state
        .space
        .element_geometry(&window)
        .unwrap_or_else(|| window.geometry());
    let has_real_mapped_size = current_geo.size.w > 1 && current_geo.size.h > 1;
    let keep_recheck_pending = state
        .has_window_rule_metadata_gap(app_id.as_deref(), title.as_deref())
        || (!window_has_exclusive_state
            && !has_explicit_floating_rule
            && decision.floating
            && auto_reason == "fixed-height"
            && !has_real_mapped_size);
    if !keep_recheck_pending {
        state.pending_window_rule_recheck_ids.remove(&surface_id);
    }
}

pub(super) fn maybe_recenter_floating_window_after_commit(state: &mut Raven, surface: &WlSurface) {
    let surface_id = surface.clone();
    if !state.pending_floating_recenter_ids.contains(&surface_id) {
        return;
    }

    let Some(window) = state.window_for_surface(surface) else {
        state.pending_floating_recenter_ids.remove(&surface_id);
        return;
    };
    if !state.is_window_floating(&window) || !state.is_window_mapped(&window) {
        state.pending_floating_recenter_ids.remove(&surface_id);
        return;
    }

    let size = window.geometry().size;
    if size.w <= 1 || size.h <= 1 {
        return;
    }

    let loc = state.initial_map_location_for_window(&window);
    state.map_window_to_location(&window, loc.into(), false);
    state.pending_floating_recenter_ids.remove(&surface_id);
}
