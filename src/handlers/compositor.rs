use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_shm,
    input::pointer::MotionEvent,
    reexports::wayland_server::protocol::{wl_buffer, wl_surface::WlSurface},
    utils::SERIAL_COUNTER,
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface,
        },
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

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
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
