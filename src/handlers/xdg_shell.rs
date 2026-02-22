use smithay::{
    delegate_kde_decoration, delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{
        PopupKind, PopupManager, Space, Window, find_popup_root_surface, get_popup_toplevel_coords,
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
            protocol::{wl_output, wl_seat, wl_surface::WlSurface},
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
        if self.window_for_surface(surface.wl_surface()).is_some() {
            tracing::warn!(
                surface_id = surface.wl_surface().id().protocol_id(),
                "duplicate new_toplevel for already tracked surface; ignoring"
            );
            surface.send_configure();
            return;
        }
        let window = Window::new_wayland_window(surface.clone());
        let rules = self.resolve_window_rules_for_surface(surface.wl_surface());
        let (effective_floating, _, _, _) = self.resolve_effective_floating_for_surface(
            surface.wl_surface(),
            &window,
            rules.floating,
        );
        let defer_initial_resolution =
            self.should_defer_window_rules_for_surface(surface.wl_surface());
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
                (mode == DecorationMode::ServerSide || self.config.no_csd) && !effective_floating,
            );
        });
        tracing::debug!("new_toplevel: step=with_pending_state:done");
        tracing::debug!("new_toplevel: step=add_window_to_workspace:start");
        self.add_unmapped_window_to_workspace(rules.workspace_index, window.clone());
        tracing::debug!("new_toplevel: step=add_window_to_workspace:done");
        // Start in explicit unmapped state; commit() drives initial configure + first map.
        self.mark_surface_unmapped_toplevel(surface.wl_surface());
        self.queue_initial_configure_for_surface(surface.wl_surface());
        self.apply_window_rule_size_to_window(&window, &rules);
        self.set_window_floating(&window, effective_floating);
        if effective_floating {
            self.queue_floating_recenter_for_surface(surface.wl_surface());
        }
        // Do not map here. Mapping before the first committed buffer causes a visible
        // top-left placeholder frame in some clients before tiling/full layout applies.
        // The compositor commit path maps once the surface is truly mapped.
        if rules.fullscreen {
            self.enter_fullscreen_window(&window);
        }
        if !defer_initial_resolution {
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
        }
        self.queue_window_rule_recheck_for_surface(surface.wl_surface());
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
        let Some(window) = self.window_for_surface(surface.wl_surface()) else {
            if surface.is_initial_configure_sent() {
                surface.send_configure();
            }
            return;
        };

        self.set_window_floating(&window, false);
        self.clear_floating_recenter_for_surface(surface.wl_surface());
        if self.is_window_mapped(&window) {
            self.clear_pending_unmapped_maximized_for_surface(surface.wl_surface());
        } else {
            // Match niri's commit-synchronized flow: remember maximize intent, don't map early.
            self.queue_pending_unmapped_maximized_for_surface(surface.wl_surface());
        }
        self.set_window_maximized_state(&window, true);
        if self.is_window_mapped(&window) {
            self.space.raise_element(&window, true);
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        let Some(window) = self.window_for_surface(surface.wl_surface()) else {
            if surface.is_initial_configure_sent() {
                surface.send_configure();
            }
            return;
        };

        self.clear_pending_unmapped_maximized_for_surface(surface.wl_surface());
        self.set_window_maximized_state(&window, false);
        if self.is_window_mapped(&window)
            && let Err(err) = self.apply_layout()
        {
            tracing::warn!("failed to apply layout after xdg unmaximize request: {err}");
        }
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _wl_output: Option<wl_output::WlOutput>,
    ) {
        let Some(window) = self.window_for_surface(surface.wl_surface()) else {
            if surface.is_initial_configure_sent() {
                surface.send_pending_configure();
            }
            return;
        };

        self.set_window_floating(&window, false);
        self.clear_floating_recenter_for_surface(surface.wl_surface());
        if !self.is_window_mapped(&window) {
            // Defer compositor-side fullscreen bookkeeping until first real map commit.
            self.queue_pending_unmapped_fullscreen_for_surface(surface.wl_surface());
            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Fullscreen);
                state.states.unset(xdg_toplevel::State::Maximized);
            });
            if surface.is_initial_configure_sent() {
                surface.send_configure();
            }
            return;
        }
        self.clear_pending_unmapped_fullscreen_for_surface(surface.wl_surface());

        if self.enter_fullscreen_window(&window) {
            self.space.raise_element(&window, true);
            if let Err(err) = self.apply_layout() {
                tracing::warn!("failed to apply layout after xdg fullscreen request: {err}");
            }
        } else {
            // xdg-shell fullscreen requests should still receive a configure response.
            self.set_window_fullscreen_state(&window, true);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let Some(window) = self.window_for_surface(surface.wl_surface()) else {
            if surface.is_initial_configure_sent() {
                surface.send_pending_configure();
            }
            return;
        };

        self.clear_pending_unmapped_fullscreen_for_surface(surface.wl_surface());
        if !self.is_window_mapped(&window) {
            let restore_maximized =
                self.has_pending_unmapped_maximized_for_surface(surface.wl_surface());
            surface.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                if restore_maximized {
                    state.states.set(xdg_toplevel::State::Maximized);
                }
            });
            if surface.is_initial_configure_sent() {
                surface.send_configure();
            }
            return;
        }

        if self.exit_fullscreen_window(&window) {
            if let Err(err) = self.apply_layout() {
                tracing::warn!("failed to apply layout after xdg unfullscreen request: {err}");
            }
        } else {
            // Mirror fullscreen_request behavior: always respond with a configure.
            self.set_window_fullscreen_state(&window, false);
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface();
        let was_tracked_unmapped = self.is_surface_unmapped_toplevel(wl_surface);
        let window = self.window_for_surface(wl_surface);

        self.clear_pending_unmapped_state_for_surface(wl_surface);
        self.clear_window_rule_recheck_for_surface(wl_surface);
        self.clear_floating_recenter_for_surface(wl_surface);

        let Some(window) = window else {
            return;
        };

        // Match niri semantics: if the toplevel was already in the unmapped phase, destroy should
        // only clean bookkeeping, not trigger another layout/remap cycle.
        if was_tracked_unmapped || !self.is_window_mapped(&window) {
            self.remove_window_from_workspaces(&window);
            return;
        }

        let outputs = self.space.outputs_for_element(&window);
        self.space.unmap_elem(&window);
        self.remove_window_from_workspaces(&window);

        if let Err(err) = self.apply_layout() {
            tracing::warn!("failed to apply layout after xdg toplevel destroy: {err}");
        }
        self.refocus_visible_window();
        self.queue_redraw_for_outputs_or_all(outputs);
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
    let surface_id = surface.id();
    if let Some(window) = space
        .elements()
        .find(|w| {
            w.toplevel()
                .is_some_and(|toplevel| toplevel.wl_surface().id() == surface_id)
        })
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
