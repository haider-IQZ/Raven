use super::*;
use bitflags::bitflags;
use smithay::output::Output;
use smithay::backend::renderer::utils::with_renderer_surface_state;

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub(crate) struct WindowFullscreenMode: u8 {
        const NONE = 0;
        const MAXIMIZED = 1 << 0;
        const FULLSCREEN = 1 << 1;
    }
}

#[derive(Clone, Debug)]
pub(super) struct FullscreenSlot {
    pub(super) surface: WlSurface,
    pub(super) output: Option<Output>,
}

#[derive(Clone, Debug)]
pub(super) struct FullscreenRestoreState {
    pub(super) rect: Option<Rectangle<i32, Logical>>,
    pub(super) maximized: bool,
}

#[derive(Clone, Debug)]
pub(super) enum PendingFullscreenTransition {
    Enter { requested_output: Option<Output> },
    Exit,
}

#[derive(Debug)]
pub(super) struct FullscreenState {
    pub(super) owner_surfaces_by_workspace: Vec<Option<FullscreenSlot>>,
    pub(super) maximized_surfaces: HashSet<WlSurface>,
    pub(super) pending_unmapped_ids: HashSet<WlSurface>,
    pub(super) pending_transition_by_surface: HashMap<WlSurface, PendingFullscreenTransition>,
    pub(super) restore_state_by_surface: HashMap<WlSurface, FullscreenRestoreState>,
}

impl FullscreenState {
    pub(super) fn new() -> Self {
        Self {
            owner_surfaces_by_workspace: vec![None; WORKSPACE_COUNT],
            maximized_surfaces: HashSet::new(),
            pending_unmapped_ids: HashSet::new(),
            pending_transition_by_surface: HashMap::new(),
            restore_state_by_surface: HashMap::new(),
        }
    }
}

impl Raven {
    pub(super) fn apply_fullscreen_layout_if_needed(
        &mut self,
        _windows: &[Window],
        output: &Output,
        _out_geo: Rectangle<i32, Logical>,
    ) -> Result<bool, CompositorError> {
        let workspace_index = self.current_workspace;
        let Some(window) = self.workspace_fullscreen_owner_window(workspace_index) else {
            return Ok(false);
        };

        if self.window_is_unmapped_toplevel(&window) || !Self::window_root_surface_has_buffer(&window)
        {
            return Ok(false);
        }

        let Some(target_output) = self.fullscreen_target_output_for_window(&window) else {
            self.clear_fullscreen_owner_for_workspace(workspace_index, true);
            return Ok(false);
        };
        if &target_output != output {
            return Ok(false);
        }

        let Some(target_rect) = self.window_exclusive_target_rect(&window) else {
            self.clear_fullscreen_owner_for_workspace(workspace_index, true);
            return Ok(false);
        };

        let current_location = self.space.element_location(&window);
        let current_geometry = self
            .space
            .element_geometry(&window)
            .unwrap_or_else(|| window.geometry());
        let is_mapped = current_location.is_some();
        let needs_resize = current_geometry.size != target_rect.size;
        let needs_reposition = current_location != Some(target_rect.loc);
        let needs_reconfigure = self.assigned_rect_for_window(&window) != Some(target_rect);

        self.record_assigned_rect_for_window(&window, target_rect);

        if needs_resize || needs_reconfigure {
            self.apply_fullscreen_protocol_state(&window, true, Some(target_rect));
        }
        if !is_mapped || needs_resize || needs_reposition {
            self.map_window_to_rect(&window, target_rect, false);
        }
        self.space.raise_element(&window, true);
        Ok(true)
    }

    fn output_is_active(&self, output: &Output) -> bool {
        self.space.output_geometry(output).is_some()
    }

    fn slot_for_workspace(&self, workspace_index: usize) -> Option<&FullscreenSlot> {
        self.fullscreen
            .owner_surfaces_by_workspace
            .get(workspace_index)
            .and_then(|slot| slot.as_ref())
    }

    fn fullscreen_slot_output_for_workspace(&self, workspace_index: usize) -> Option<Output> {
        self.slot_for_workspace(workspace_index)
            .and_then(|slot| slot.output.clone())
            .filter(|output| self.output_is_active(output))
    }

    fn fullscreen_slot_output_for_window(&self, window: &Window) -> Option<Output> {
        let workspace_index = self.workspace_index_for_window(window)?;
        self.window_is_workspace_fullscreen_owner(window)
            .then(|| self.fullscreen_slot_output_for_workspace(workspace_index))
            .flatten()
    }

    fn fallback_output_for_window(&self, window: &Window) -> Option<Output> {
        self.space
            .outputs_for_element(window)
            .into_iter()
            .next()
            .or_else(|| self.space.outputs().next().cloned())
    }

    fn preferred_output_for_window(&self, window: &Window) -> Option<Output> {
        self.fullscreen_slot_output_for_window(window)
            .or_else(|| self.fallback_output_for_window(window))
    }

    fn resolve_fullscreen_output(
        &self,
        window: &Window,
        requested_output: Option<Output>,
    ) -> Option<Output> {
        requested_output
            .filter(|output| self.output_is_active(output))
            .or_else(|| self.fullscreen_slot_output_for_window(window))
            .or_else(|| self.fallback_output_for_window(window))
    }

    fn fullscreen_target_output_for_window(&self, window: &Window) -> Option<Output> {
        self.fullscreen_slot_output_for_window(window)
            .or_else(|| self.space.outputs_for_element(window).into_iter().next())
            .or_else(|| self.space.outputs().next().cloned())
    }

    pub(super) fn window_root_surface_has_buffer(window: &Window) -> bool {
        window.toplevel().is_some_and(|toplevel| {
            with_renderer_surface_state(toplevel.wl_surface(), |state| state.buffer().is_some())
                .unwrap_or(false)
        })
    }

    fn fullscreen_output_rect_for_window(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let output = self.fullscreen_target_output_for_window(window)?;
        self.space.output_geometry(&output)
    }

    fn fullscreen_output_rect_for_output(
        &self,
        output: Option<&Output>,
    ) -> Option<Rectangle<i32, Logical>> {
        output.and_then(|output| self.space.output_geometry(output))
    }

    fn sizes_match_target(
        actual: Size<i32, Logical>,
        target: Size<i32, Logical>,
    ) -> bool {
        (actual.w - target.w).abs() <= 1 && (actual.h - target.h).abs() <= 1
    }

    fn current_root_surface_size_for_window(&self, window: &Window) -> Option<Size<i32, Logical>> {
        let toplevel = window.toplevel()?;
        with_renderer_surface_state(toplevel.wl_surface(), |state| state.surface_size()).flatten()
    }

    fn fullscreen_restore_target_rect(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        if let Some(surface_id) = Self::window_surface_id(window)
            && let Some(restore_state) = self.fullscreen.restore_state_by_surface.get(&surface_id)
        {
            if restore_state.maximized {
                return self.work_area_rect_for_window(window);
            }
            if let Some(rect) = restore_state.rect {
                return Some(rect);
            }
        }

        if self.window_is_marked_maximized(window) {
            return self.work_area_rect_for_window(window);
        }

        self.compute_workspace_layout_target(window)
            .map(|(target_geometry, _)| target_geometry)
    }

    fn remember_fullscreen_restore_state(&mut self, window: &Window) {
        let Some(surface_id) = Self::window_surface_id(window) else {
            return;
        };
        if self.fullscreen.restore_state_by_surface.contains_key(&surface_id) {
            return;
        }

        let restore_rect = self
            .assigned_rect_for_window(window)
            .or_else(|| self.space.element_geometry(window));
        let restore_state = FullscreenRestoreState {
            rect: restore_rect,
            maximized: self.window_is_marked_maximized(window),
        };
        self.fullscreen
            .restore_state_by_surface
            .insert(surface_id, restore_state);
    }

    fn clear_fullscreen_restore_state_for_surface(&mut self, surface: &WlSurface) {
        self.fullscreen.restore_state_by_surface.remove(surface);
    }

    fn prepare_fullscreen_exit_protocol_state(&mut self, window: &Window) -> Option<Serial> {
        if self.window_is_marked_maximized(window) {
            return self.configure_window_for_maximized_layout(window);
        }

        if let Some((target_geometry, layout_bounds)) = self.compute_workspace_layout_target(window) {
            return self.configure_window_for_tiled_layout(window, target_geometry, layout_bounds);
        }

        self.apply_fullscreen_protocol_state(window, false, None)
    }

    fn finalize_fullscreen_exit_visual_state(&mut self, window: &Window) -> bool {
        if !self.window_effective_fullscreen_state(window) {
            return false;
        }

        if let Some(surface_id) = Self::window_surface_id(window) {
            self.clear_pending_fullscreen_transition_for_surface(&surface_id);
        }

        let workspace_index = self
            .workspace_index_for_window(window)
            .unwrap_or(self.current_workspace);
        if self.window_is_workspace_fullscreen_owner(window) {
            self.clear_fullscreen_owner_for_workspace(workspace_index, false);
        } else if let Some(surface_id) = Self::window_surface_id(window) {
            self.clear_fullscreen_owner_for_surface(&surface_id);
        }

        if self.window_is_marked_maximized(window) {
            if let Some(target_rect) = self.work_area_rect_for_window(window) {
                self.map_window_to_rect(window, target_rect, false);
            }
        } else if let Some(target_rect) = self.fullscreen_restore_target_rect(window) {
            self.map_window_to_rect(window, target_rect, false);
        }

        self.debug_assert_state_invariants("finalize_fullscreen_exit_visual_state");
        true
    }

    fn pending_fullscreen_transition_target_rect(
        &self,
        window: &Window,
        transition: &PendingFullscreenTransition,
    ) -> Option<Rectangle<i32, Logical>> {
        match transition {
            PendingFullscreenTransition::Enter { requested_output } => {
                let target_output =
                    self.resolve_fullscreen_output(window, requested_output.clone());
                self.fullscreen_output_rect_for_output(target_output.as_ref())
            }
            PendingFullscreenTransition::Exit => self.fullscreen_restore_target_rect(window),
        }
    }

    fn pending_fullscreen_transition_is_ready(
        &self,
        window: &Window,
        transition: &PendingFullscreenTransition,
    ) -> bool {
        let Some(target_rect) = self.pending_fullscreen_transition_target_rect(window, transition)
        else {
            return true;
        };

        let actual_size = self
            .committed_reported_size_for_window(window)
            .or_else(|| self.current_root_surface_size_for_window(window))
            .or_else(|| Some(window.geometry().size));

        actual_size
            .map(|size| Self::sizes_match_target(size, target_rect.size))
            .unwrap_or(false)
    }

    fn work_area_rect_for_window(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let output = self.preferred_output_for_window(window)?;
        let output_rect = self.space.output_geometry(&output)?;
        let mut layer_map = layer_map_for_output(&output);
        layer_map.arrange();
        let work_geo = layer_map.non_exclusive_zone();
        if work_geo.size.w > 0 && work_geo.size.h > 0 {
            Some(work_geo)
        } else {
            Some(output_rect)
        }
    }

    pub(crate) fn window_exclusive_target_rect(
        &self,
        window: &Window,
    ) -> Option<Rectangle<i32, Logical>> {
        match self.window_effective_exclusive_mode(window) {
            mode if mode.contains(WindowFullscreenMode::FULLSCREEN) => {
                self.fullscreen_output_rect_for_window(window)
            }
            mode if mode.contains(WindowFullscreenMode::MAXIMIZED) => {
                self.work_area_rect_for_window(window)
            }
            _ => None,
        }
    }

    fn window_has_pending_or_committed_state(
        window: &Window,
        state_flag: xdg_toplevel::State,
    ) -> bool {
        let Some(toplevel) = window.toplevel() else {
            return false;
        };

        let pending_has = toplevel.with_pending_state(|state| state.states.contains(state_flag));
        let committed_has = toplevel.with_committed_state(|state| {
            state
                .as_ref()
                .is_some_and(|state| state.states.contains(state_flag))
        });
        pending_has || committed_has
    }

    fn window_is_marked_maximized(&self, window: &Window) -> bool {
        let Some(surface_id) = Self::window_surface_id(window) else {
            return false;
        };
        self.fullscreen.maximized_surfaces.contains(&surface_id)
            || Self::window_has_pending_or_committed_state(window, xdg_toplevel::State::Maximized)
    }

    fn surface_is_workspace_fullscreen_owner(
        &self,
        workspace_index: usize,
        surface: &WlSurface,
    ) -> bool {
        self.slot_for_workspace(workspace_index)
            .is_some_and(|slot| &slot.surface == surface)
    }

    pub(super) fn window_is_workspace_fullscreen_owner(&self, window: &Window) -> bool {
        let Some(surface_id) = Self::window_surface_id(window) else {
            return false;
        };
        let Some(workspace_index) = self.workspace_index_for_window(window) else {
            return false;
        };
        self.surface_is_workspace_fullscreen_owner(workspace_index, &surface_id)
    }

    pub(super) fn workspace_fullscreen_owner_window(
        &self,
        workspace_index: usize,
    ) -> Option<Window> {
        self.slot_for_workspace(workspace_index)
            .and_then(|slot| self.window_for_surface(&slot.surface))
            .filter(|window| self.workspace_contains_window(workspace_index, window))
    }

    pub(crate) fn workspace_effective_exclusive_mode(
        &self,
        workspace_index: usize,
    ) -> WindowFullscreenMode {
        self.workspace_fullscreen_owner_window(workspace_index)
            .map(|_| WindowFullscreenMode::FULLSCREEN)
            .unwrap_or(WindowFullscreenMode::NONE)
    }

    pub(super) fn clear_fullscreen_owner_for_workspace(
        &mut self,
        workspace_index: usize,
        clear_window_state: bool,
    ) {
        let Some(slot) = self
            .fullscreen
            .owner_surfaces_by_workspace
            .get_mut(workspace_index)
            .and_then(Option::take)
        else {
            return;
        };

        if clear_window_state && let Some(owner_window) = self.window_for_surface(&slot.surface) {
            self.apply_fullscreen_protocol_state(&owner_window, false, None);
        }
    }

    pub(super) fn clear_fullscreen_owner_for_surface(&mut self, surface: &WlSurface) {
        for slot in &mut self.fullscreen.owner_surfaces_by_workspace {
            if slot.as_ref().is_some_and(|candidate| &candidate.surface == surface) {
                *slot = None;
            }
        }
    }

    fn assign_workspace_fullscreen_owner(
        &mut self,
        workspace_index: usize,
        window: &Window,
        output: Option<Output>,
    ) {
        if workspace_index >= self.fullscreen.owner_surfaces_by_workspace.len() {
            return;
        }

        let Some(surface_id) = Self::window_surface_id(window) else {
            return;
        };

        if self
            .slot_for_workspace(workspace_index)
            .is_some_and(|slot| slot.surface != surface_id)
        {
            self.clear_fullscreen_owner_for_workspace(workspace_index, true);
        }

        self.fullscreen.owner_surfaces_by_workspace[workspace_index] = Some(FullscreenSlot {
            surface: surface_id.clone(),
            output,
        });

        for (index, slot) in self
            .fullscreen
            .owner_surfaces_by_workspace
            .iter_mut()
            .enumerate()
        {
            if index != workspace_index
                && slot
                    .as_ref()
                    .is_some_and(|candidate| candidate.surface == surface_id)
            {
                *slot = None;
            }
        }
    }

    pub(super) fn move_workspace_fullscreen_owner_surface(
        &mut self,
        surface: &WlSurface,
        target_workspace: usize,
    ) {
        if target_workspace >= self.fullscreen.owner_surfaces_by_workspace.len() {
            return;
        }

        let source_workspace = self
            .fullscreen
            .owner_surfaces_by_workspace
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|candidate| &candidate.surface == surface));

        let Some(source_workspace) = source_workspace else {
            return;
        };
        if source_workspace == target_workspace {
            return;
        }

        if self
            .slot_for_workspace(target_workspace)
            .is_some_and(|slot| &slot.surface != surface)
        {
            self.clear_fullscreen_owner_for_workspace(target_workspace, true);
        }

        let slot = self.fullscreen.owner_surfaces_by_workspace[source_workspace].take();
        self.fullscreen.owner_surfaces_by_workspace[target_workspace] = slot;
    }

    fn apply_fullscreen_protocol_state(
        &mut self,
        window: &Window,
        fullscreen: bool,
        target_rect: Option<Rectangle<i32, Logical>>,
    ) -> Option<Serial> {
        let Some(toplevel) = window.toplevel() else {
            return None;
        };

        let output_bounds = target_rect
            .map(|rect| rect.size)
            .or_else(|| {
                self.fullscreen_target_output_for_window(window)
                    .and_then(|output| self.space.output_geometry(&output))
                    .map(|geometry| geometry.size)
            });
        let keep_maximized = !fullscreen && self.window_is_marked_maximized(window);

        let mut needs_configure = false;
        toplevel.with_pending_state(|state| {
            if fullscreen {
                if !state.states.contains(xdg_toplevel::State::Fullscreen) {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                    needs_configure = true;
                }
                if state.states.contains(xdg_toplevel::State::Maximized) {
                    state.states.unset(xdg_toplevel::State::Maximized);
                    needs_configure = true;
                }
                if state.states.contains(xdg_toplevel::State::TiledLeft) {
                    state.states.unset(xdg_toplevel::State::TiledLeft);
                    needs_configure = true;
                }
                if state.states.contains(xdg_toplevel::State::TiledRight) {
                    state.states.unset(xdg_toplevel::State::TiledRight);
                    needs_configure = true;
                }
                if state.states.contains(xdg_toplevel::State::TiledTop) {
                    state.states.unset(xdg_toplevel::State::TiledTop);
                    needs_configure = true;
                }
                if state.states.contains(xdg_toplevel::State::TiledBottom) {
                    state.states.unset(xdg_toplevel::State::TiledBottom);
                    needs_configure = true;
                }
                let desired_size = target_rect.map(|rect| rect.size);
                if state.size != desired_size {
                    state.size = desired_size;
                    needs_configure = true;
                }
                let desired_bounds = target_rect.map(|rect| rect.size).or(output_bounds);
                if state.bounds != desired_bounds {
                    state.bounds = desired_bounds;
                    needs_configure = true;
                }
                return;
            }

            if state.states.contains(xdg_toplevel::State::Fullscreen) {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                needs_configure = true;
            }
            if !keep_maximized {
                if state.size.is_some() {
                    state.size = None;
                    needs_configure = true;
                }
                if state.bounds.is_some() {
                    state.bounds = None;
                    needs_configure = true;
                }
            }
        });

        if needs_configure && toplevel.is_initial_configure_sent() {
            let fallback_size = target_rect
                .map(|rect| rect.size)
                .or_else(|| self.assigned_rect_for_window(window).map(|rect| rect.size));
            self.sync_reported_size_from_pending_state(window, fallback_size);
            toplevel.send_pending_configure()
        } else {
            None
        }
    }

    fn configure_window_for_maximized_layout(&mut self, window: &Window) -> Option<Serial> {
        let Some(toplevel) = window.toplevel() else {
            return None;
        };

        let target_rect = self.work_area_rect_for_window(window);
        let output_bounds = self
            .preferred_output_for_window(window)
            .and_then(|output| self.space.output_geometry(&output))
            .map(|geometry| geometry.size);

        let mut needs_configure = false;
        toplevel.with_pending_state(|state| {
            if !state.states.contains(xdg_toplevel::State::Maximized) {
                state.states.set(xdg_toplevel::State::Maximized);
                needs_configure = true;
            }
            if state.states.contains(xdg_toplevel::State::Fullscreen) {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                needs_configure = true;
            }
            if state.states.contains(xdg_toplevel::State::TiledLeft) {
                state.states.unset(xdg_toplevel::State::TiledLeft);
                needs_configure = true;
            }
            if state.states.contains(xdg_toplevel::State::TiledRight) {
                state.states.unset(xdg_toplevel::State::TiledRight);
                needs_configure = true;
            }
            if state.states.contains(xdg_toplevel::State::TiledTop) {
                state.states.unset(xdg_toplevel::State::TiledTop);
                needs_configure = true;
            }
            if state.states.contains(xdg_toplevel::State::TiledBottom) {
                state.states.unset(xdg_toplevel::State::TiledBottom);
                needs_configure = true;
            }

            let desired_size = target_rect.map(|rect| rect.size);
            if state.size != desired_size {
                state.size = desired_size;
                needs_configure = true;
            }

            let desired_bounds = target_rect.map(|rect| rect.size).or(output_bounds);
            if state.bounds != desired_bounds {
                state.bounds = desired_bounds;
                needs_configure = true;
            }
        });

        if needs_configure && toplevel.is_initial_configure_sent() {
            let fallback_size = target_rect
                .map(|rect| rect.size)
                .or_else(|| self.assigned_rect_for_window(window).map(|rect| rect.size));
            self.sync_reported_size_from_pending_state(window, fallback_size);
            toplevel.send_pending_configure()
        } else {
            None
        }
    }

    fn clear_window_maximized_protocol_state(&mut self, window: &Window) -> Option<Serial> {
        let Some(toplevel) = window.toplevel() else {
            return None;
        };

        let mut needs_configure = false;
        toplevel.with_pending_state(|state| {
            if state.states.contains(xdg_toplevel::State::Maximized) {
                state.states.unset(xdg_toplevel::State::Maximized);
                needs_configure = true;
            }
            if state.size.is_some() {
                state.size = None;
                needs_configure = true;
            }
            if state.bounds.is_some() {
                state.bounds = None;
                needs_configure = true;
            }
        });

        if needs_configure && toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure()
        } else {
            None
        }
    }

    pub fn output_has_fullscreen_window(&self, output: &Output) -> bool {
        self.fullscreen_window_for_output(output).is_some()
    }

    pub(crate) fn fullscreen_window_for_output(&self, output: &Output) -> Option<Window> {
        let workspace_index = self.current_workspace;
        let window = self.workspace_fullscreen_owner_window(workspace_index)?;
        let target_output = self
            .fullscreen_target_output_for_window(&window)
            .or_else(|| self.space.outputs_for_element(&window).into_iter().next());

        target_output
            .filter(|candidate| candidate == output)
            .map(|_| window)
    }

    pub(crate) fn window_visual_or_assigned_rect(
        &self,
        window: &Window,
    ) -> Option<Rectangle<i32, Logical>> {
        self.assigned_rect_for_window(window)
    }

    pub(super) fn compute_workspace_layout_target(
        &self,
        window: &Window,
    ) -> Option<(Rectangle<i32, Logical>, Size<i32, Logical>)> {
        let workspace_index = self
            .workspace_index_for_window(window)
            .unwrap_or(self.current_workspace);
        let output = self.preferred_output_for_window(window)?;
        let out_geo = self.space.output_geometry(&output)?;
        let mut layer_map = layer_map_for_output(&output);
        layer_map.arrange();
        let work_geo = layer_map.non_exclusive_zone();
        let layout_geo = if work_geo.size.w > 0 && work_geo.size.h > 0 {
            work_geo
        } else {
            out_geo
        };

        let workspace_windows = self.workspaces[workspace_index].clone();
        let tiled_windows: Vec<Window> = workspace_windows
            .iter()
            .filter(|candidate| !self.is_window_floating(candidate))
            .filter(|candidate| !self.window_is_unmapped_toplevel(candidate))
            .filter(|candidate| Self::window_has_live_client(candidate))
            .cloned()
            .collect();
        if tiled_windows.is_empty() {
            return None;
        }

        let gaps = GapConfig {
            outer_horizontal: self.config.gaps_outer_horizontal,
            outer_vertical: self.config.gaps_outer_vertical,
            inner_horizontal: self.config.gaps_inner_horizontal,
            inner_vertical: self.config.gaps_inner_vertical,
        };

        let geometries = self.layout.arrange(
            &tiled_windows,
            layout_geo.size.w as u32,
            layout_geo.size.h as u32,
            &gaps,
            self.config.master_factor,
            self.config.num_master,
            self.config.smart_gaps,
        );

        tiled_windows
            .into_iter()
            .zip(geometries)
            .find(|(candidate, _)| Self::windows_match(candidate, window))
            .map(|(_, geom)| {
                (
                    Rectangle::new(
                        Point::<i32, Logical>::from((
                            layout_geo.loc.x + geom.x_coordinate,
                            layout_geo.loc.y + geom.y_coordinate,
                        )),
                        Size::<i32, Logical>::from((geom.width as i32, geom.height as i32)),
                    ),
                    layout_geo.size,
                )
            })
    }

    pub(super) fn configure_window_for_tiled_layout(
        &mut self,
        window: &Window,
        target_geometry: Rectangle<i32, Logical>,
        layout_bounds: Size<i32, Logical>,
    ) -> Option<Serial> {
        let Some(toplevel) = window.toplevel() else {
            return None;
        };

        let mut needs_configure = false;
        toplevel.with_pending_state(|state| {
            if state.states.contains(xdg_toplevel::State::Fullscreen) {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                needs_configure = true;
            }
            if state.states.contains(xdg_toplevel::State::Maximized) {
                state.states.unset(xdg_toplevel::State::Maximized);
                needs_configure = true;
            }
            if !state.states.contains(xdg_toplevel::State::TiledLeft) {
                state.states.set(xdg_toplevel::State::TiledLeft);
                needs_configure = true;
            }
            if !state.states.contains(xdg_toplevel::State::TiledRight) {
                state.states.set(xdg_toplevel::State::TiledRight);
                needs_configure = true;
            }
            if !state.states.contains(xdg_toplevel::State::TiledTop) {
                state.states.set(xdg_toplevel::State::TiledTop);
                needs_configure = true;
            }
            if !state.states.contains(xdg_toplevel::State::TiledBottom) {
                state.states.set(xdg_toplevel::State::TiledBottom);
                needs_configure = true;
            }
            if state.size != Some(target_geometry.size) {
                state.size = Some(target_geometry.size);
                needs_configure = true;
            }
            if state.bounds != Some(layout_bounds) {
                state.bounds = Some(layout_bounds);
                needs_configure = true;
            }
        });

        if needs_configure && toplevel.is_initial_configure_sent() {
            self.sync_reported_size_from_pending_state(window, Some(target_geometry.size));
            toplevel.send_pending_configure()
        } else {
            None
        }
    }

    pub(crate) fn set_window_maximized_state(&mut self, window: &Window, maximized: bool) {
        let Some(surface_id) = Self::window_surface_id(window) else {
            return;
        };

        if maximized {
            self.fullscreen.maximized_surfaces.insert(surface_id);
        } else {
            self.fullscreen.maximized_surfaces.remove(&surface_id);
        }

        if self.window_effective_fullscreen_state(window) {
            return;
        }

        if maximized {
            self.configure_window_for_maximized_layout(window);
        } else {
            self.clear_window_maximized_protocol_state(window);
        }
    }

    pub(crate) fn set_window_fullscreen_state_for_output(
        &mut self,
        window: &Window,
        fullscreen: bool,
        requested_output: Option<Output>,
    ) -> Option<Serial> {
        let workspace_index = self
            .workspace_index_for_window(window)
            .unwrap_or(self.current_workspace);
        let target_rect = if fullscreen {
            let output = self.resolve_fullscreen_output(window, requested_output);
            self.assign_workspace_fullscreen_owner(workspace_index, window, output);
            self.window_exclusive_target_rect(window)
        } else {
            if self.window_is_workspace_fullscreen_owner(window) {
                self.clear_fullscreen_owner_for_workspace(workspace_index, false);
            } else if let Some(surface_id) = Self::window_surface_id(window) {
                self.clear_fullscreen_owner_for_surface(&surface_id);
            }
            None
        };

        self.apply_fullscreen_protocol_state(window, fullscreen, target_rect)
    }

    fn queue_pending_fullscreen_enter(
        &mut self,
        surface: &WlSurface,
        requested_output: Option<Output>,
    ) {
        self.fullscreen.pending_transition_by_surface.insert(
            surface.clone(),
            PendingFullscreenTransition::Enter { requested_output },
        );
    }

    fn queue_pending_fullscreen_exit(&mut self, surface: &WlSurface) {
        self.fullscreen
            .pending_transition_by_surface
            .insert(surface.clone(), PendingFullscreenTransition::Exit);
    }

    fn clear_pending_fullscreen_transition_for_surface(&mut self, surface: &WlSurface) {
        self.fullscreen.pending_transition_by_surface.remove(surface);
    }

    pub(crate) fn request_fullscreen_enter_on_output(
        &mut self,
        window: &Window,
        requested_output: Option<Output>,
    ) -> bool {
        let Some(surface_id) = Self::window_surface_id(window) else {
            return false;
        };
        if !self.window_effective_fullscreen_state(window) {
            self.remember_fullscreen_restore_state(window);
        }
        let target_output = self.resolve_fullscreen_output(window, requested_output.clone());
        let target_rect = self.fullscreen_output_rect_for_output(target_output.as_ref());
        self.queue_pending_fullscreen_enter(&surface_id, requested_output.clone());
        let configure_serial = self.apply_fullscreen_protocol_state(window, true, target_rect);
        if configure_serial.is_none() {
            if !self.is_window_mapped(window) {
                return false;
            }
            self.clear_pending_fullscreen_transition_for_surface(&surface_id);
            return self.enter_fullscreen_window_on_output(window, requested_output);
        }
        false
    }

    pub(crate) fn request_fullscreen_exit(&mut self, window: &Window) -> bool {
        let Some(surface_id) = Self::window_surface_id(window) else {
            return false;
        };
        if !self.window_effective_fullscreen_state(window) {
            self.clear_pending_fullscreen_transition_for_surface(&surface_id);
            self.apply_fullscreen_protocol_state(window, false, None);
            return false;
        }
        self.queue_pending_fullscreen_exit(&surface_id);
        let configure_serial = self.prepare_fullscreen_exit_protocol_state(window);
        if configure_serial.is_none() {
            if !self.is_window_mapped(window) {
                return false;
            }
            self.clear_pending_fullscreen_transition_for_surface(&surface_id);
            return self.finalize_fullscreen_exit_visual_state(window);
        }
        false
    }

    pub(crate) fn set_window_fullscreen_state(
        &mut self,
        window: &Window,
        fullscreen: bool,
    ) -> Option<Serial> {
        self.set_window_fullscreen_state_for_output(window, fullscreen, None)
    }

    pub fn enter_fullscreen_window_on_output(
        &mut self,
        window: &Window,
        requested_output: Option<Output>,
    ) -> bool {
        let was_fullscreen = self.window_effective_fullscreen_state(window);
        let previous_output = self.fullscreen_slot_output_for_window(window);

        self.set_window_floating(window, false);
        if let Some(surface_id) = Self::window_surface_id(window) {
            self.clear_floating_recenter_for_surface(&surface_id);
            self.clear_pending_fullscreen_transition_for_surface(&surface_id);
        }

        self.set_window_fullscreen_state_for_output(window, true, requested_output);

        if let Some(target_rect) = self.window_exclusive_target_rect(window) {
            self.record_assigned_rect_for_window(window, target_rect);
        }

        self.debug_assert_state_invariants("enter_fullscreen_window");

        let next_output = self.fullscreen_slot_output_for_window(window);
        !was_fullscreen || previous_output != next_output
    }

    pub fn enter_fullscreen_window(&mut self, window: &Window) -> bool {
        self.enter_fullscreen_window_on_output(window, None)
    }

    pub fn exit_fullscreen_window(&mut self, window: &Window) -> bool {
        if !self.window_effective_fullscreen_state(window) {
            return false;
        }

        if let Some(surface_id) = Self::window_surface_id(window) {
            self.clear_pending_fullscreen_transition_for_surface(&surface_id);
            self.clear_fullscreen_restore_state_for_surface(&surface_id);
        }

        self.set_window_fullscreen_state(window, false);

        if self.window_is_marked_maximized(window) {
            self.configure_window_for_maximized_layout(window);
            if let Some(target_rect) = self.window_exclusive_target_rect(window) {
                self.record_assigned_rect_for_window(window, target_rect);
            }
        } else if let Some((target_geometry, layout_bounds)) =
            self.compute_workspace_layout_target(window)
        {
            self.configure_window_for_tiled_layout(window, target_geometry, layout_bounds);
        }

        self.debug_assert_state_invariants("exit_fullscreen_window");
        true
    }

    pub fn toggle_fullscreen_focused_window(&mut self) -> Result<(), CompositorError> {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return Ok(());
        };
        let Some(focused_surface) = keyboard.current_focus() else {
            return Ok(());
        };
        let Some(window) = self.window_for_surface(&focused_surface) else {
            return Ok(());
        };

        if self.window_effective_fullscreen_state(&window) {
            if self.request_fullscreen_exit(&window) {
                return self.apply_layout();
            }
            return Ok(());
        }

        if self.request_fullscreen_enter_on_output(&window, None) {
            self.space.raise_element(&window, true);
            return self.apply_layout();
        }

        Ok(())
    }

    pub(crate) fn window_surface_id(window: &Window) -> Option<WlSurface> {
        window
            .toplevel()
            .map(|toplevel| toplevel.wl_surface().clone())
    }

    pub(crate) fn window_effective_exclusive_mode(&self, window: &Window) -> WindowFullscreenMode {
        if self.window_is_workspace_fullscreen_owner(window) {
            WindowFullscreenMode::FULLSCREEN
        } else if self.window_is_marked_maximized(window) {
            WindowFullscreenMode::MAXIMIZED
        } else {
            WindowFullscreenMode::NONE
        }
    }

    pub(crate) fn window_effective_fullscreen_state(&self, window: &Window) -> bool {
        self.window_effective_exclusive_mode(window)
            .contains(WindowFullscreenMode::FULLSCREEN)
    }

    pub(crate) fn window_has_exclusive_layout_state(&self, window: &Window) -> bool {
        self.window_effective_exclusive_mode(window) != WindowFullscreenMode::NONE
    }

    pub fn queue_pending_unmapped_fullscreen_for_surface(&mut self, surface: &WlSurface) {
        self.fullscreen.pending_unmapped_ids.insert(surface.clone());
    }

    pub fn clear_pending_unmapped_fullscreen_for_surface(&mut self, surface: &WlSurface) {
        self.fullscreen.pending_unmapped_ids.remove(surface);
    }

    pub fn maybe_apply_pending_fullscreen_transition_for_surface(&mut self, surface: &WlSurface) {
        let Some(transition) = self
            .fullscreen
            .pending_transition_by_surface
            .get(surface)
            .cloned()
        else {
            return;
        };

        let Some(window) = self.window_for_surface(surface) else {
            self.clear_pending_fullscreen_transition_for_surface(surface);
            return;
        };
        if !self.is_window_mapped(&window) {
            return;
        }
        if !self.pending_fullscreen_transition_is_ready(&window, &transition) {
            return;
        }
        self.clear_pending_fullscreen_transition_for_surface(surface);

        match transition {
            PendingFullscreenTransition::Enter { requested_output } => {
                if self.enter_fullscreen_window_on_output(&window, requested_output) {
                    self.space.raise_element(&window, true);
                }
                if let Err(err) = self.apply_layout() {
                    tracing::warn!(
                        "failed to apply layout after commit-synchronized fullscreen enter: {err}"
                    );
                }
            }
            PendingFullscreenTransition::Exit => {
                if self.finalize_fullscreen_exit_visual_state(&window)
                    && let Err(err) = self.apply_layout()
                {
                    tracing::warn!(
                        "failed to apply layout after commit-synchronized fullscreen exit: {err}"
                    );
                }
            }
        }
    }

    pub fn maybe_apply_pending_unmapped_state_for_surface(&mut self, surface: &WlSurface) {
        let wants_fullscreen = self.fullscreen.pending_unmapped_ids.contains(surface);
        let wants_maximized = self.pending_unmapped_maximized_ids.contains(surface);
        if !wants_fullscreen && !wants_maximized {
            return;
        }

        let Some(window) = self.window_for_surface(surface) else {
            self.clear_pending_unmapped_state_for_surface(surface);
            return;
        };
        if !self.is_window_mapped(&window) {
            return;
        }

        self.clear_pending_unmapped_fullscreen_for_surface(surface);
        self.pending_unmapped_maximized_ids.remove(surface);

        if self.window_has_exclusive_layout_state(&window) {
            self.space.raise_element(&window, true);
            if let Err(err) = self.apply_layout() {
                tracing::warn!(
                    "failed to apply layout after pending unmapped exclusive-state apply: {err}"
                );
            }
        }
    }
}
