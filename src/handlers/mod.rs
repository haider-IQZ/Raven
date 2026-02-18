mod compositor;
mod layer_shell;
mod xdg_shell;

use smithay::{
    backend::renderer::ImportDma,
    delegate_data_device, delegate_dmabuf, delegate_drm_syncobj, delegate_fractional_scale,
    delegate_output, delegate_presentation,
    delegate_primary_selection, delegate_seat, delegate_viewporter,
    desktop::layer_map_for_output,
    input::{
        Seat, SeatHandler, SeatState,
        dnd::{DnDGrab, DndGrabHandler, GrabType},
        pointer::{CursorImageStatus, Focus},
    },
    output::Output,
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::SERIAL_COUNTER,
    wayland::{
        compositor::with_states,
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        drm_syncobj::{DrmSyncobjHandler, DrmSyncobjState},
        fractional_scale::{FractionalScaleHandler, with_fractional_scale},
        output::OutputHandler,
        selection::{
            SelectionHandler,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
            primary_selection::{
                PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
            },
        },
    },
};

use crate::{
    Raven, delegate_ext_workspace, delegate_foreign_toplevel, delegate_screencopy,
    protocols::{
        ext_workspace::{self, ExtWorkspaceHandler, ExtWorkspaceManagerState},
        foreign_toplevel::{self, ForeignToplevelHandler, ForeignToplevelManagerState},
        wlr_screencopy::{Screencopy, ScreencopyHandler, ScreencopyManagerState},
    },
};

impl SeatHandler for Raven {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);

        let focused_window = focused.and_then(|surface| self.window_for_surface(surface));
        self.sync_window_activation(focused_window.as_ref());
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }
}

delegate_seat!(Raven);

impl SelectionHandler for Raven {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Raven {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DndGrabHandler for Raven {}

impl WaylandDndGrabHandler for Raven {
    fn dnd_requested<S: smithay::input::dnd::Source>(
        &mut self,
        source: S,
        _icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: smithay::utils::Serial,
        type_: smithay::input::dnd::GrabType,
    ) {
        match type_ {
            GrabType::Pointer => {
                let ptr = seat.get_pointer().unwrap();
                let start_data = ptr.grab_start_data().unwrap();

                let grab = DnDGrab::new_pointer(&self.display_handle, start_data, source, seat);
                ptr.set_grab(self, grab, serial, Focus::Keep);
            }
            // TODO: handle touch grab
            GrabType::Touch => {}
        }
    }
}

delegate_data_device!(Raven);

impl OutputHandler for Raven {
    fn output_bound(
        &mut self,
        output: Output,
        wl_output: smithay::reexports::wayland_server::protocol::wl_output::WlOutput,
    ) {
        tracing::info!(
            output = %output.name(),
            wl_output_version = wl_output.version(),
            "wl_output bound"
        );
        self.refresh_ext_workspace();
        ext_workspace::on_output_bound(self, &output, &wl_output);
        foreign_toplevel::on_output_bound(self, &output, &wl_output);
    }
}

delegate_output!(Raven);

impl FractionalScaleHandler for Raven {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let preferred_scale = self
            .space
            .outputs()
            .next()
            .map(|output| output.current_scale().fractional_scale())
            .unwrap_or(1.0);

        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional_scale| {
                fractional_scale.set_preferred_scale(preferred_scale);
            });
        });
    }
}

delegate_fractional_scale!(Raven);
delegate_viewporter!(Raven);

impl PrimarySelectionHandler for Raven {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}
delegate_primary_selection!(Raven);

impl ScreencopyHandler for Raven {
    fn screencopy_state(&mut self) -> &mut ScreencopyManagerState {
        &mut self.screencopy_state
    }

    fn frame(&mut self, screencopy: Screencopy) {
        self.pending_screencopy = Some(screencopy);
    }
}

delegate_screencopy!(Raven);

impl ExtWorkspaceHandler for Raven {
    fn ext_workspace_manager_state(&mut self) -> &mut ExtWorkspaceManagerState {
        &mut self.ext_workspace_manager_state
    }

    fn activate_workspace(&mut self, workspace_index: usize) {
        if let Err(err) = self.switch_workspace(workspace_index) {
            tracing::warn!("failed to activate ext-workspace index {workspace_index}: {err}");
        }
    }
}

delegate_ext_workspace!(Raven);

impl ForeignToplevelHandler for Raven {
    fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState {
        &mut self.foreign_toplevel_manager_state
    }

    fn activate(&mut self, wl_surface: WlSurface) {
        let Some(window) = self.window_for_surface(&wl_surface) else {
            return;
        };

        if let Some(target_workspace) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.contains(&window))
            && target_workspace != self.current_workspace
            && let Err(err) = self.switch_workspace(target_workspace)
        {
            tracing::warn!("failed to switch workspace for foreign toplevel activate: {err}");
            return;
        }

        self.space.raise_element(&window, true);
        self.set_keyboard_focus(Some(wl_surface), SERIAL_COUNTER.next_serial());
    }

    fn close(&mut self, wl_surface: WlSurface) {
        if let Some(window) = self.window_for_surface(&wl_surface)
            && let Some(toplevel) = window.toplevel()
        {
            toplevel.send_close();
        }
    }

    fn set_fullscreen(
        &mut self,
        wl_surface: WlSurface,
        _wl_output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        let Some(window) = self.window_for_surface(&wl_surface) else {
            return;
        };

        if let Some(target_workspace) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.contains(&window))
            && target_workspace != self.current_workspace
            && let Err(err) = self.switch_workspace(target_workspace)
        {
            tracing::warn!("failed to switch workspace for foreign toplevel fullscreen: {err}");
            return;
        }

        if self.enter_fullscreen_window(&window) {
            self.space.raise_element(&window, true);
            if let Err(err) = self.apply_layout() {
                tracing::warn!("failed to apply layout after foreign toplevel fullscreen: {err}");
            }
        }
    }

    fn unset_fullscreen(&mut self, wl_surface: WlSurface) {
        let Some(window) = self.window_for_surface(&wl_surface) else {
            return;
        };

        if self.exit_fullscreen_window(&window) {
            if let Err(err) = self.apply_layout() {
                tracing::warn!("failed to apply layout after foreign toplevel unfullscreen: {err}");
            }
        }
    }

    fn set_maximized(&mut self, wl_surface: WlSurface) {
        let Some(window) = self.window_for_surface(&wl_surface) else {
            return;
        };
        let Some(toplevel) = window.toplevel() else {
            return;
        };

        let output_geometry = self.space.outputs().next().and_then(|output| {
            let mut layer_map = layer_map_for_output(output);
            layer_map.arrange();
            let work_geo = layer_map.non_exclusive_zone();
            if work_geo.size.w > 0 && work_geo.size.h > 0 {
                Some(work_geo)
            } else {
                self.space.output_geometry(output)
            }
        });

        if let Some(geometry) = output_geometry {
            toplevel.with_pending_state(|state| {
                state.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
                state.size = Some(geometry.size);
                state.bounds = Some(geometry.size);
            });
            self.space.map_element(window.clone(), geometry.loc, true);
        } else {
            toplevel.with_pending_state(|state| {
                state.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
            });
        }

        toplevel.send_pending_configure();
        self.space.raise_element(&window, true);
    }

    fn unset_maximized(&mut self, wl_surface: WlSurface) {
        let Some(window) = self.window_for_surface(&wl_surface) else {
            return;
        };
        let Some(toplevel) = window.toplevel() else {
            return;
        };

        toplevel.with_pending_state(|state| {
            state.states.unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
            state.size = None;
            state.bounds = None;
        });
        toplevel.send_pending_configure();
        if let Err(err) = self.apply_layout() {
            tracing::warn!("failed to apply layout after foreign toplevel unmaximize: {err}");
        }
    }
}

delegate_foreign_toplevel!(Raven);

impl DmabufHandler for Raven {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        self.dmabuf_state
            .as_mut()
            .expect("dmabuf_state not initialized")
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        if let Some(ref mut udev_data) = self.udev_data {
            if udev_data
                .gpus
                .single_renderer(&udev_data.primary_gpu)
                .and_then(|mut renderer| renderer.import_dmabuf(&dmabuf, None))
                .is_ok()
            {
                let _ = notifier.successful::<Raven>();
                return;
            }
        }
        notifier.failed();
    }
}

delegate_dmabuf!(Raven);

impl DrmSyncobjHandler for Raven {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.syncobj_state.as_mut()
    }
}

delegate_drm_syncobj!(Raven);
delegate_presentation!(Raven);
