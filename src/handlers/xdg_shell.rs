use smithay::{
    delegate_kde_decoration, delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{
        PopupKind, PopupManager, Space, Window, find_popup_root_surface, get_popup_toplevel_coords,
        layer_map_for_output,
    },
    input::{
        Seat,
        pointer::{Focus, GrabStartData as PointerGrabStartData},
    },
    reexports::{
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
            shell::server::xdg_toplevel,
        },
        wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration,
        wayland_server::{
            Resource, WEnum,
            protocol::{wl_seat, wl_surface::WlSurface},
        },
    },
    utils::{Rectangle, SERIAL_COUNTER, Serial},
    wayland::{
        compositor,
        shell::{
            kde::decoration::{KdeDecorationHandler, KdeDecorationState},
            xdg::{
                PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
                XdgToplevelSurfaceData, decoration::XdgDecorationHandler,
            },
        },
    },
};

use crate::{
    Raven,
    grabs::{move_grab::MoveGrab, resize_grab::ResizeSurfaceGrab},
};

impl XdgShellHandler for Raven {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface.clone());
        let rules = self.resolve_window_rules_for_surface(surface.wl_surface());
        let visible_on_current_workspace = rules.workspace_index == self.current_workspace;
        let mode = self.preferred_decoration_mode();
        tracing::debug!(
            ?mode,
            no_csd = self.config.no_csd,
            "new_toplevel: setting initial decoration state"
        );
        tracing::debug!("new_toplevel: step=with_pending_state:start");
        surface.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
            set_tiled_state(
                state,
                (mode == DecorationMode::ServerSide || self.config.no_csd) && !rules.floating,
            );
        });
        tracing::debug!("new_toplevel: step=with_pending_state:done");
        tracing::debug!("new_toplevel: step=add_window_to_workspace:start");
        self.add_window_to_workspace(rules.workspace_index, window.clone());
        tracing::debug!("new_toplevel: step=add_window_to_workspace:done");
        self.apply_window_rule_size_to_window(&window, &rules);
        self.set_window_floating(&window, rules.floating);
        if rules.floating {
            self.queue_floating_recenter_for_surface(surface.wl_surface());
        }
        if visible_on_current_workspace {
            tracing::debug!("new_toplevel: step=map_element:start");
            let location = self.initial_map_location_for_window(&window);
            self.space.map_element(window.clone(), location, false);
            tracing::debug!("new_toplevel: step=map_element:done");
        }
        if rules.fullscreen && !self.fullscreen_windows.contains(&window) {
            let existing = self.fullscreen_windows.clone();
            for fullscreen_window in &existing {
                self.set_window_fullscreen_state(fullscreen_window, false);
            }
            self.fullscreen_windows.clear();
            self.fullscreen_windows.push(window.clone());
            self.set_window_fullscreen_state(&window, true);
        }
        tracing::debug!("new_toplevel: step=apply_layout:start");
        self.apply_layout().ok();
        tracing::debug!("new_toplevel: step=apply_layout:done");
        if visible_on_current_workspace && rules.focus {
            tracing::debug!("new_toplevel: step=set_keyboard_focus:start");
            self.set_keyboard_focus(
                Some(surface.wl_surface().clone()),
                SERIAL_COUNTER.next_serial(),
            );
            tracing::debug!("new_toplevel: step=set_keyboard_focus:done");
        }
        self.queue_window_rule_recheck_for_surface(surface.wl_surface());
        tracing::debug!("new_toplevel: step=send_configure:start");
        surface.send_configure();
        tracing::debug!("new_toplevel: step=send_configure:done");
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("error while tracking popup: {err:?}");
        }
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    // TODO: Test this when you implement resize request
    // as it should be able to trigger this as well.
    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let seat = Seat::from_resource(&seat).unwrap();
        let wl_surface = surface.wl_surface();

        if let Some(start_data) = check_grab(&seat, wl_surface, serial) {
            let pointer = seat.get_pointer().unwrap();

            let window = self.window_for_surface(wl_surface).unwrap();

            let initial_window_location = self.space.element_location(&window).unwrap();

            let grab = MoveGrab {
                start_data,
                window,
                initial_window_location,
                current_window_location: initial_window_location,
            };

            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let seat = Seat::from_resource(&seat).unwrap();
        let wl_surface = surface.wl_surface();

        if let Some(start_data) = check_grab(&seat, wl_surface, serial) {
            let pointer = seat.get_pointer().unwrap();

            let window = self.window_for_surface(wl_surface).unwrap();

            let initial_window_location = self.space.element_location(&window).unwrap();
            let initial_window_size = window.geometry().size;

            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
            });

            surface.send_pending_configure();

            let grab = ResizeSurfaceGrab::start(
                start_data,
                window,
                edges.into(),
                Rectangle::new(initial_window_location, initial_window_size),
            );

            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        let window = self.window_for_surface(surface.wl_surface()).unwrap();
        let output = self.space.outputs().next().unwrap();
        let geometry = {
            let mut layer_map = layer_map_for_output(output);
            layer_map.arrange();
            let work_geo = layer_map.non_exclusive_zone();
            if work_geo.size.w > 0 && work_geo.size.h > 0 {
                work_geo
            } else {
                self.space.output_geometry(output).unwrap()
            }
        };

        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Maximized);
            state.size = Some(geometry.size);
        });
        self.space.map_element(window, geometry.loc, true);

        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Maximized);
            state.size = None;
        });

        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.clear_window_rule_recheck_for_surface(surface.wl_surface());
        self.clear_floating_recenter_for_surface(surface.wl_surface());
        let window = self.window_for_surface(surface.wl_surface());

        if let Some(window) = window {
            self.space.unmap_elem(&window);
            self.remove_window_from_workspaces(&window);
            self.apply_layout().ok();
        }

        self.refocus_visible_window();
    }
}

delegate_xdg_shell!(Raven);

impl XdgDecorationHandler for Raven {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        let mode = self.preferred_decoration_mode();
        tracing::debug!(
            ?mode,
            no_csd = self.config.no_csd,
            "new_decoration: client bound xdg-decoration"
        );
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
            set_tiled_state(state, mode == DecorationMode::ServerSide);
        });

        // Always use send_configure() here, not send_pending_configure().
        // The decoration object was just created so the client needs a configure
        // that includes the decoration mode event, even if the toplevel state
        // hasn't changed since the initial configure.
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        // Honor the client request here to avoid
        // client creation edge-cases caused by forcing a different mode
        // during the initial negotiation.
        tracing::debug!(
            ?mode,
            no_csd = self.config.no_csd,
            "request_mode: client requested decoration mode"
        );
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
            set_tiled_state(
                state,
                mode == DecorationMode::ServerSide || self.config.no_csd,
            );
        });

        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        let mode = self.preferred_decoration_mode();
        tracing::debug!(
            ?mode,
            no_csd = self.config.no_csd,
            "unset_mode: client unset decoration mode"
        );
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
            set_tiled_state(state, mode == DecorationMode::ServerSide);
        });

        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
    }
}

delegate_xdg_decoration!(Raven);

impl KdeDecorationHandler for Raven {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.kde_decoration_state
    }

    fn request_mode(
        &mut self,
        _surface: &WlSurface,
        decoration: &org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration,
        mode: WEnum<org_kde_kwin_server_decoration::Mode>,
    ) {
        let WEnum::Value(mode) = mode else {
            return;
        };
        tracing::debug!(?mode, "kde_decoration: client requested mode");
        // Keep protocol negotiation simple: echo
        // the requested mode.
        decoration.mode(mode);
    }
}

delegate_kde_decoration!(Raven);

fn set_tiled_state(state: &mut smithay::wayland::shell::xdg::ToplevelState, tiled: bool) {
    if tiled {
        state.states.set(xdg_toplevel::State::TiledLeft);
        state.states.set(xdg_toplevel::State::TiledRight);
        state.states.set(xdg_toplevel::State::TiledTop);
        state.states.set(xdg_toplevel::State::TiledBottom);
    } else {
        state.states.unset(xdg_toplevel::State::TiledLeft);
        state.states.unset(xdg_toplevel::State::TiledRight);
        state.states.unset(xdg_toplevel::State::TiledTop);
        state.states.unset(xdg_toplevel::State::TiledBottom);
    }
}

fn check_grab(
    seat: &Seat<Raven>,
    surface: &WlSurface,
    serial: Serial,
) -> Option<PointerGrabStartData<Raven>> {
    let pointer = seat.get_pointer()?;

    // Check that this surface has a click grab.
    if !pointer.has_grab(serial) {
        return None;
    }

    let start_data = pointer.grab_start_data()?;

    let (focus, _) = start_data.focus.as_ref()?;
    // If the focus was for a different surface, ignore the request.
    if !focus.id().same_client_as(&surface.id()) {
        return None;
    }

    Some(start_data)
}

/// Should be called on `WlSurface::commit`
pub fn handle_commit(popups: &mut PopupManager, space: &Space<Window>, surface: &WlSurface) {
    // Handle toplevel commits.
    if let Some(window) = space
        .elements()
        .find(|w| w.toplevel().unwrap().wl_surface() == surface)
        .cloned()
    {
        let initial_configure_sent = compositor::with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });

        if !initial_configure_sent {
            window.toplevel().unwrap().send_configure();
        }
    }

    // Handle popup commits.
    popups.commit(surface);
    if let Some(popup) = popups.find_popup(surface) {
        match &popup {
            PopupKind::Xdg(xdg) => {
                if !xdg.is_initial_configure_sent() {
                    xdg.send_configure().expect("initial configure failed");
                }
            }
            PopupKind::InputMethod(_) => {}
        }
    }
}

impl Raven {
    pub fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };

        let Some(window) = self.window_for_surface(&root) else {
            return;
        };

        let Some(output) = self.space.outputs().next() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(output) else {
            return;
        };
        let window_geo = self
            .space
            .element_geometry(&window)
            .unwrap_or_else(|| window.geometry());

        // The target geometry for the positioner should be relative to its parent's geometry, so
        // we will compute that here.
        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
