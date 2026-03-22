use smithay::desktop::Window;

use crate::CompositorError;

use super::Raven;

pub(super) fn workspace_index_for_window(state: &Raven, window: &Window) -> Option<usize> {
    state
        .workspace_index_for_mapped_window(window)
        .or_else(|| state.workspace_index_for_unmapped_window(window))
}

pub(super) fn move_window_to_workspace_internal(
    state: &mut Raven,
    window: &Window,
    target_workspace: usize,
) -> Result<(), CompositorError> {
    if target_workspace >= state.workspaces.len() {
        return Err(CompositorError::Backend(format!(
            "invalid workspace index {target_workspace}"
        )));
    }

    let source_mapped_workspace = state.workspace_index_for_mapped_window(window);
    let source_unmapped_workspace = state.workspace_index_for_unmapped_window(window);
    let tracked_unmapped =
        state.window_is_unmapped_toplevel(window) || source_unmapped_workspace.is_some();

    if tracked_unmapped {
        Raven::remove_window_from_workspace_list(&mut state.workspaces, window);

        match source_unmapped_workspace {
            Some(source_workspace) if source_workspace == target_workspace => {}
            _ => {
                Raven::remove_window_from_workspace_list(&mut state.unmapped_workspaces, window);
                Raven::add_window_to_workspace_list(
                    &mut state.unmapped_workspaces,
                    target_workspace,
                    window.clone(),
                )?;
            }
        }

        if let Some(surface_id) = Raven::window_surface_id(window) {
            state.move_workspace_fullscreen_owner_surface(&surface_id, target_workspace);
        }
        state.debug_assert_state_invariants("move_window_to_workspace_internal_unmapped");
        return Ok(());
    }

    match source_mapped_workspace {
        Some(source_workspace) if source_workspace == target_workspace => {}
        Some(source_workspace) => {
            Raven::remove_window_from_workspace_list(&mut state.workspaces, window);
            Raven::add_window_to_workspace_list(
                &mut state.workspaces,
                target_workspace,
                window.clone(),
            )?;

            if source_workspace == state.current_workspace {
                state.unmap_window(window);
            }
            if target_workspace == state.current_workspace {
                state.map_window_to_initial_location_if_mappable(window, false);
            }
        }
        None => {
            state.add_window_to_workspace(target_workspace, window.clone());
            if target_workspace == state.current_workspace {
                state.map_window_to_initial_location_if_mappable(window, false);
            }
        }
    }

    Raven::remove_window_from_workspace_list(&mut state.unmapped_workspaces, window);
    if let Some(surface_id) = Raven::window_surface_id(window) {
        state.move_workspace_fullscreen_owner_surface(&surface_id, target_workspace);
    }
    state.debug_assert_state_invariants("move_window_to_workspace_internal");

    Ok(())
}

pub(super) fn remove_window_from_workspaces(state: &mut Raven, window: &Window) {
    if let Some(surface_id) = Raven::window_surface_id(window) {
        state.clear_assigned_rect_for_surface(&surface_id);
        state.clear_pending_unmapped_state_for_surface(&surface_id);
        state.clear_fullscreen_owner_for_surface(&surface_id);
    }
    Raven::remove_window_from_workspace_list(&mut state.workspaces, window);
    Raven::remove_window_from_workspace_list(&mut state.unmapped_workspaces, window);
    state
        .floating_windows
        .retain(|candidate| !Raven::windows_match(candidate, window));
    state.debug_assert_state_invariants("remove_window_from_workspaces");
}

pub(super) fn switch_workspace(
    state: &mut Raven,
    target_workspace: usize,
) -> Result<(), CompositorError> {
    if target_workspace >= state.workspaces.len() {
        return Err(CompositorError::Backend(format!(
            "invalid workspace index {target_workspace}"
        )));
    }

    if target_workspace == state.current_workspace {
        return Ok(());
    }

    state.prune_windows_without_live_client();

    let current_windows = state.workspaces[state.current_workspace].clone();
    for window in &current_windows {
        state.unmap_window(window);
    }

    state.current_workspace = target_workspace;

    let target_windows = state.workspaces[target_workspace].clone();
    for window in target_windows {
        let mapped_now = state.map_window_to_initial_location_if_mappable(&window, false);
        if mapped_now
            && let Some(toplevel) = window.toplevel()
            && toplevel.is_initial_configure_sent()
        {
            toplevel.send_pending_configure();
        }
        if mapped_now && let Some(toplevel) = window.toplevel() {
            state.maybe_apply_pending_unmapped_state_for_surface(toplevel.wl_surface());
        }
    }

    state.apply_layout()?;
    state.refocus_visible_window();
    state.refresh_ext_workspace();
    crate::backend::udev::queue_redraw_all(state);
    state.debug_assert_state_invariants("switch_workspace");
    Ok(())
}

pub(super) fn move_focused_window_to_workspace(
    state: &mut Raven,
    target_workspace: usize,
) -> Result<(), CompositorError> {
    if target_workspace >= state.workspaces.len() {
        return Err(CompositorError::Backend(format!(
            "invalid workspace index {target_workspace}"
        )));
    }

    let Some(keyboard) = state.seat.get_keyboard() else {
        return Ok(());
    };
    let Some(focused_surface) = keyboard.current_focus() else {
        return Ok(());
    };
    let Some(window) = state.window_for_surface(&focused_surface) else {
        return Ok(());
    };

    let source_workspace = state
        .workspace_index_for_window(&window)
        .unwrap_or(state.current_workspace);

    if source_workspace == target_workspace {
        return Ok(());
    }

    move_window_to_workspace_internal(state, &window, target_workspace)?;

    if source_workspace == state.current_workspace {
        state.apply_layout()?;
        state.refocus_visible_window();
    } else if target_workspace == state.current_workspace
        && (state.is_window_mapped(&window)
            || state.map_window_to_initial_location_if_mappable(&window, false))
    {
        state.apply_layout()?;
    }

    state.refresh_ext_workspace();
    Ok(())
}
