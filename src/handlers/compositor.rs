use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_shm,
    input::pointer::MotionEvent,
    reexports::{
        calloop::Interest,
        wayland_server::{
            Resource,
            protocol::{wl_buffer, wl_surface::WlSurface},
        },
    },
    utils::SERIAL_COUNTER,
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface,
            with_states,
        },
        dmabuf::get_dmabuf,
        drm_syncobj::DrmSyncobjCachedState,
        shm::{ShmHandler, ShmState},
    },
};

use crate::{
    Raven,
    grabs::resize_grab,
    handlers::{layer_shell, xdg_shell},
    state::ClientState,
};

impl CompositorHandler for Raven {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, move |state, _dh, surface| {
            let mut acquire_point = None;
            let maybe_dmabuf = with_states(surface, |surface_data| {
                acquire_point.clone_from(
                    &surface_data
                        .cached_state
                        .get::<DrmSyncobjCachedState>()
                        .pending()
                        .acquire_point,
                );
                surface_data
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                    .and_then(|assignment| match assignment {
                        BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                        _ => None,
                    })
            });

            let Some(dmabuf) = maybe_dmabuf else {
                return;
            };

            if let Some(acquire_point) = acquire_point
                && let Ok((blocker, source)) = acquire_point.generate_blocker()
                && let Some(client) = surface.client()
            {
                let res = state.loop_handle.insert_source(source, move |_, _, data| {
                    let dh = data.display_handle.clone();
                    data.client_compositor_state(&client)
                        .blocker_cleared(data, &dh);
                    Ok(())
                });
                if res.is_ok() {
                    add_blocker(surface, blocker);
                    return;
                }
            }

            if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ)
                && let Some(client) = surface.client()
            {
                let res = state.loop_handle.insert_source(source, move |_, _, data| {
                    let dh = data.display_handle.clone();
                    data.client_compositor_state(&client)
                        .blocker_cleared(data, &dh);
                    Ok(())
                });
                if res.is_ok() {
                    add_blocker(surface, blocker);
                }
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        // Pre-import the buffer on the primary GPU right away.  Without this,
        // the import happens synchronously inside render_frame() which blocks
        // the render pipeline and causes frame jitter with heavy clients (Brave).
        crate::backend::udev::early_import(self, surface);

        let mut is_root = false;
        let mut root_surface = None;
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            is_root = root == *surface;

            if let Some(window) = self.window_for_surface(&root) {
                window.on_commit();
            }
            self.mark_fullscreen_ready_for_surface(&root);
            root_surface = Some(root);
        }

        xdg_shell::handle_commit(&mut self.popups, &self.space, surface);
        if let Some(root_surface) = root_surface.as_ref() {
            self.maybe_apply_deferred_window_rules(root_surface);
            self.maybe_recenter_floating_window_after_commit(root_surface);
        }
        resize_grab::handle_commit(&mut self.space, surface);
        let current_focus = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        let (layer_focus, relayout) = layer_shell::handle_commit(
            &mut self.space,
            self.pointer_location,
            current_focus.as_ref(),
            surface,
        );
        if relayout && let Err(err) = self.apply_layout() {
            tracing::warn!("failed to apply layout after layer-shell commit: {err}");
        }
        if let Some(layer_focus) = layer_focus {
            let serial = SERIAL_COUNTER.next_serial();
            let pointer = self.pointer();
            if !pointer.is_grabbed() {
                pointer.motion(
                    self,
                    self.surface_under_pointer(),
                    &MotionEvent {
                        location: self.pointer_location,
                        serial,
                        time: self.start_time.elapsed().as_millis() as u32,
                    },
                );
                pointer.frame(self);
            }
            self.set_keyboard_focus(Some(layer_focus), serial);
        }

        if is_root {
            self.refresh_ext_workspace();
            self.refresh_foreign_toplevel();
        }

        // Queue redraw only for the output that contains this surface,
        // not all outputs. This prevents excessive redraws that cause flickering with
        // heavy clients like Brave/Steam.
        if let Some(root) = root_surface {
            if let Some(window) = self.window_for_surface(&root) {
                if let Some(output) = self.space.outputs_for_element(&window).into_iter().next() {
                    crate::backend::udev::queue_redraw_for_output(self, &output);
                } else {
                    // Window not on any output yet, queue all
                    crate::backend::udev::queue_redraw_all(self);
                }
            } else {
                // No window found, queue all outputs
                crate::backend::udev::queue_redraw_all(self);
            }
        } else {
            // Subsurface commit - still need to redraw, but be more targeted
            crate::backend::udev::queue_redraw_all(self);
        }
    }
}

impl BufferHandler for Raven {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Raven {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_shm!(Raven);
delegate_compositor!(Raven);
