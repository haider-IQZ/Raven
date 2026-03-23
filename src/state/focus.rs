use super::*;
use smithay::{
    desktop::{LayerSurface, Window, WindowSurfaceType, layer_map_for_output},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::SERIAL_COUNTER,
    wayland::{
        input_method::InputMethodSeat,
        shell::wlr_layer::{KeyboardInteractivity, Layer as WlrLayer},
    },
};

#[derive(Debug, Default)]
pub(super) struct FocusState {
    pub(super) active_window: Option<Window>,
    pub(super) layer_shell_on_demand_focus: Option<WlSurface>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FocusReason {
    PointerMotion,
    PointerButton,
    KeyboardAction,
    NewWindow,
    DeferredRules,
    ForeignActivate,
    LayerCommit,
    ExclusiveLayer,
}

impl FocusReason {
    fn is_explicit_user(self) -> bool {
        matches!(
            self,
            Self::PointerButton
                | Self::KeyboardAction
                | Self::ForeignActivate
                | Self::ExclusiveLayer
        )
    }
}

impl Raven {
    pub(crate) fn active_toplevel(&self) -> Option<Window> {
        self.focus
            .active_window
            .clone()
            .filter(|window| self.window_is_tracked_on_current_workspace(window))
    }

    pub(crate) fn focused_window(&self) -> Option<Window> {
        self.active_toplevel().or_else(|| {
            self.seat
                .get_keyboard()
                .and_then(|keyboard| keyboard.current_focus())
                .and_then(|surface| self.window_for_surface(&surface))
        })
    }

    pub(crate) fn request_focus_window(
        &mut self,
        window: &Window,
        reason: FocusReason,
        raise: bool,
    ) -> bool {
        if !self.should_allow_focus_window(window, reason) {
            return false;
        }

        self.focus.active_window = Some(window.clone());
        self.focus.layer_shell_on_demand_focus = None;

        if raise && self.is_window_mapped(window) {
            self.raise_window_preserving_layer(window);
        }

        true
    }

    pub(crate) fn request_focus_layer(&mut self, surface: Option<WlSurface>, _reason: FocusReason) {
        self.focus.layer_shell_on_demand_focus = surface;
    }

    pub fn refresh_focus(&mut self) {
        self.cleanup_focus_state();

        if self.focus.active_window.is_none() {
            if let Some(owner) = self.workspace_fullscreen_owner_window(self.current_workspace) {
                self.focus.active_window = Some(owner);
            } else if self.config.focus_follow_mouse
                && let Some(window) = self.pointer_contents.window.clone()
                && self.window_is_mapped_focusable(&window)
            {
                self.focus.active_window = Some(window);
            } else if let Some(window) = self.space.elements().last().cloned() {
                self.focus.active_window = Some(window);
            }
        }

        let target = self.resolve_keyboard_focus_target();
        self.apply_keyboard_focus_target(target);
    }

    pub(crate) fn refocus_visible_window(&mut self) {
        self.focus.layer_shell_on_demand_focus = None;
        self.focus.active_window = None;
        self.refresh_focus();
    }

    pub(crate) fn clear_focus_for_surface(&mut self, surface: &WlSurface) {
        if self
            .focus
            .active_window
            .as_ref()
            .is_some_and(|window| Self::window_matches_surface(window, surface))
        {
            self.focus.active_window = None;
        }
        if self.focus.layer_shell_on_demand_focus.as_ref() == Some(surface) {
            self.focus.layer_shell_on_demand_focus = None;
        }
    }

    pub(crate) fn update_focus_from_pointer(
        &mut self,
        location: Point<f64, Logical>,
        reason: FocusReason,
        raise: bool,
    ) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let input_method = self.seat.input_method();

        if self.pointer().is_grabbed()
            || (keyboard.is_grabbed() && !input_method.keyboard_grabbed())
        {
            return;
        }

        let Some(output) = self.space.outputs().next().cloned() else {
            self.refresh_focus();
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            self.refresh_focus();
            return;
        };
        let layers = layer_map_for_output(&output);

        if !raise
            && let Some(focused_surface) = self.focus.layer_shell_on_demand_focus.as_ref()
            && let Some(layer) =
                layers.layer_for_surface(focused_surface, WindowSurfaceType::TOPLEVEL)
            && matches!(layer.layer(), WlrLayer::Overlay | WlrLayer::Top)
            && layer.can_receive_keyboard_focus()
            && layers.layer_geometry(layer).is_some()
        {
            self.refresh_focus();
            return;
        }

        let pos_within_output = location - output_geo.loc.to_f64();

        if let Some(layer) = layers
            .layer_under(WlrLayer::Overlay, pos_within_output)
            .or_else(|| layers.layer_under(WlrLayer::Top, pos_within_output))
            && layer.can_receive_keyboard_focus()
            && let Some(layer_geo) = layers.layer_geometry(layer)
            && layer
                .surface_under(
                    pos_within_output - layer_geo.loc.to_f64(),
                    WindowSurfaceType::ALL,
                )
                .is_some()
        {
            self.request_focus_layer(Some(layer.wl_surface().clone()), reason);
            self.refresh_focus();
            return;
        }

        if let Some((window, _)) = self
            .space
            .element_under(location)
            .map(|(window, point)| (window.clone(), point))
        {
            self.request_focus_window(&window, reason, raise);
            self.refresh_focus();
            return;
        }

        if let Some(layer) = layers
            .layer_under(WlrLayer::Bottom, pos_within_output)
            .or_else(|| layers.layer_under(WlrLayer::Background, pos_within_output))
            && layer.can_receive_keyboard_focus()
            && let Some(layer_geo) = layers.layer_geometry(layer)
            && layer
                .surface_under(
                    pos_within_output - layer_geo.loc.to_f64(),
                    WindowSurfaceType::ALL,
                )
                .is_some()
        {
            self.request_focus_layer(Some(layer.wl_surface().clone()), reason);
            self.refresh_focus();
            return;
        }

        self.refresh_focus();
    }

    pub(crate) fn sync_window_activation(&self, focused_window: Option<&Window>) {
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        for window in windows {
            let is_focused = focused_window.is_some_and(|focused| focused == &window);
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    if is_focused {
                        state.states.set(xdg_toplevel::State::Activated);
                    } else {
                        state.states.unset(xdg_toplevel::State::Activated);
                    }
                });
                if toplevel.is_initial_configure_sent() {
                    toplevel.send_pending_configure();
                }
            }
        }
    }

    fn apply_keyboard_focus_target(&mut self, target: Option<WlSurface>) {
        let current_focus = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        let focused_window = self.active_toplevel();
        self.sync_window_activation(focused_window.as_ref());

        if current_focus.as_ref() == target.as_ref() {
            return;
        }

        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
        }
    }

    fn cleanup_focus_state(&mut self) {
        if self
            .focus
            .active_window
            .as_ref()
            .is_some_and(|window| !self.window_is_tracked_on_current_workspace(window))
        {
            self.focus.active_window = None;
        }

        if let Some(surface) = self.focus.layer_shell_on_demand_focus.clone() {
            let good = self
                .find_layer_surface(&surface)
                .is_some_and(|(_, layer)| self.layer_can_take_on_demand_focus(&layer));
            if !good {
                self.focus.layer_shell_on_demand_focus = None;
            }
        }

        if let Some(owner) = self.workspace_fullscreen_owner_window(self.current_workspace) {
            let keep_current = self
                .focus
                .active_window
                .as_ref()
                .is_some_and(|window| self.windows_share_focus_group(window, &owner));
            if !keep_current {
                self.focus.active_window = Some(owner);
            }
        }
    }

    fn resolve_keyboard_focus_target(&self) -> Option<WlSurface> {
        self.exclusive_layer_focus_target()
            .or_else(|| self.focus.layer_shell_on_demand_focus.clone())
            .or_else(|| {
                self.active_toplevel()
                    .filter(|window| self.window_is_mapped_focusable(window))
                    .and_then(|window| Self::window_surface_id(&window))
            })
            .or_else(|| {
                self.space
                    .elements()
                    .last()
                    .cloned()
                    .and_then(|window| Self::window_surface_id(&window))
            })
            .or_else(|| self.background_exclusive_layer_focus_target())
    }

    fn exclusive_layer_focus_target(&self) -> Option<WlSurface> {
        let output = self
            .active_output_for_pointer()
            .or_else(|| self.space.outputs().next().cloned())?;
        let layers = layer_map_for_output(&output);
        [WlrLayer::Overlay, WlrLayer::Top]
            .into_iter()
            .find_map(|layer_kind| {
                layers.layers_on(layer_kind).find_map(|layer| {
                    let is_exclusive = layer.cached_state().keyboard_interactivity
                        == KeyboardInteractivity::Exclusive;
                    if !is_exclusive || !layer.can_receive_keyboard_focus() {
                        return None;
                    }
                    self.layer_can_receive_focus(layer)
                        .then(|| layer.wl_surface().clone())
                })
            })
    }

    fn background_exclusive_layer_focus_target(&self) -> Option<WlSurface> {
        let output = self
            .active_output_for_pointer()
            .or_else(|| self.space.outputs().next().cloned())?;
        let layers = layer_map_for_output(&output);
        [WlrLayer::Bottom, WlrLayer::Background]
            .into_iter()
            .find_map(|layer_kind| {
                layers.layers_on(layer_kind).find_map(|layer| {
                    let is_exclusive = layer.cached_state().keyboard_interactivity
                        == KeyboardInteractivity::Exclusive;
                    if !is_exclusive || !layer.can_receive_keyboard_focus() {
                        return None;
                    }
                    self.layer_can_receive_focus(layer)
                        .then(|| layer.wl_surface().clone())
                })
            })
    }

    fn should_allow_focus_window(&self, window: &Window, reason: FocusReason) -> bool {
        if !self.window_is_tracked_on_current_workspace(window) {
            return false;
        }

        let Some(owner) = self.workspace_fullscreen_owner_window(self.current_workspace) else {
            return true;
        };

        self.windows_share_focus_group(window, &owner) || reason.is_explicit_user()
    }

    fn windows_share_focus_group(&self, left: &Window, right: &Window) -> bool {
        let Some(left_surface) = Self::window_surface_id(left) else {
            return false;
        };
        let Some(right_surface) = Self::window_surface_id(right) else {
            return false;
        };

        left_surface == right_surface
            || self.window_is_descendant_of_surface(left, &right_surface)
            || self.window_is_descendant_of_surface(right, &left_surface)
    }

    fn window_is_descendant_of_surface(&self, window: &Window, ancestor: &WlSurface) -> bool {
        let mut parent = window.toplevel().and_then(|toplevel| toplevel.parent());
        while let Some(surface) = parent {
            if &surface == ancestor {
                return true;
            }

            parent = self.window_for_surface(&surface).and_then(|parent_window| {
                parent_window
                    .toplevel()
                    .and_then(|toplevel| toplevel.parent())
            });
        }

        false
    }

    fn window_is_tracked_on_current_workspace(&self, window: &Window) -> bool {
        self.workspace_contains_window(self.current_workspace, window)
            && Self::window_has_live_client(window)
    }

    fn window_is_mapped_focusable(&self, window: &Window) -> bool {
        self.window_is_tracked_on_current_workspace(window) && self.is_window_mapped(window)
    }

    fn find_layer_surface(
        &self,
        surface: &WlSurface,
    ) -> Option<(smithay::output::Output, LayerSurface)> {
        self.space.outputs().find_map(|output| {
            let layers = layer_map_for_output(output);
            layers
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .cloned()
                .map(|layer| (output.clone(), layer))
        })
    }

    fn layer_can_receive_focus(&self, layer: &LayerSurface) -> bool {
        self.find_layer_surface(layer.wl_surface())
            .is_some_and(|(output, found)| {
                let layers = layer_map_for_output(&output);
                layers.layer_geometry(&found).is_some()
            })
    }

    fn layer_can_take_on_demand_focus(&self, layer: &LayerSurface) -> bool {
        layer.cached_state().keyboard_interactivity == KeyboardInteractivity::OnDemand
            && layer.can_receive_keyboard_focus()
            && self.layer_can_receive_focus(layer)
    }
}
