use crate::state::Raven;
use smithay::delegate_layer_shell;
use smithay::desktop::{LayerSurface, Space, Window, WindowSurfaceType, layer_map_for_output};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};
use smithay::wayland::compositor::{self, get_parent};
use smithay::wayland::shell::wlr_layer::{
    Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData, WlrLayerShellHandler,
    WlrLayerShellState,
};
use smithay::wayland::shell::xdg::PopupSurface;

impl WlrLayerShellHandler for Raven {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<WlOutput>,
        layer: Layer,
        namespace: String,
    ) {
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.space.outputs().next().cloned());

        let Some(output) = output else {
            tracing::warn!(namespace, "no output for new layer surface");
            return;
        };

        tracing::debug!(
            namespace,
            requested_layer = ?layer,
            output = %output.name(),
            "new layer surface"
        );

        let layer_surface = LayerSurface::new(surface, namespace);
        let mut layer_map = layer_map_for_output(&output);
        if let Err(err) = layer_map.map_layer(&layer_surface) {
            tracing::warn!("failed to map layer surface: {err:?}");
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        if let Some((mut map, layer)) = self.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            let layer = map
                .layers()
                .find(|layer| layer.layer_surface() == &surface)
                .cloned();

            layer.map(|layer| (map, layer))
        }) {
            map.unmap_layer(&layer);
        }
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        self.unconstrain_popup(&popup);
    }
}
delegate_layer_shell!(Raven);

/// Should be called on `WlSurface::commit`
pub fn handle_commit(
    space: &mut Space<Window>,
    pointer_location: Point<f64, Logical>,
    current_keyboard_focus: Option<&WlSurface>,
    surface: &WlSurface,
) -> (Option<WlSurface>, bool) {
    let mut root_surface = surface.clone();
    while let Some(parent) = get_parent(&root_surface) {
        root_surface = parent;
    }

    for output in space.outputs() {
        let mut layer_map = layer_map_for_output(output);
        if let Some(layer) = layer_map
            .layer_for_surface(&root_surface, WindowSurfaceType::TOPLEVEL)
            .cloned()
        {
            layer_map.arrange();

            let initial_configure_sent = compositor::with_states(&root_surface, |states| {
                states
                    .data_map
                    .get::<LayerSurfaceData>()
                    .map(|data| data.lock().unwrap().initial_configure_sent)
                    .unwrap_or(true)
            });

            let geometry = layer_map.layer_geometry(&layer);
            tracing::debug!(
                namespace = layer.namespace(),
                layer = ?layer.layer(),
                initial_configure_sent,
                geometry = ?geometry,
                "layer commit"
            );

            if !initial_configure_sent {
                tracing::debug!(
                    namespace = layer.namespace(),
                    "sending initial configure for layer"
                );
                layer.layer_surface().send_configure();
            }

            let should_focus = matches!(layer.layer(), Layer::Overlay | Layer::Top)
                && layer.can_receive_keyboard_focus()
                && space
                    .output_geometry(output)
                    .and_then(|output_geo| {
                        geometry.and_then(|layer_geo| {
                            layer
                                .surface_under(
                                    pointer_location
                                        - output_geo.loc.to_f64()
                                        - layer_geo.loc.to_f64(),
                                    WindowSurfaceType::ALL,
                                )
                                .map(|_| ())
                        })
                    })
                    .is_some()
                && current_keyboard_focus != Some(layer.wl_surface());

            if should_focus {
                let namespace = layer.namespace();
                tracing::debug!(
                    namespace,
                    "focusing layer on commit because pointer is over it"
                );
                return (Some(layer.wl_surface().clone()), true);
            }

            return (None, true);
        }
    }

    (None, false)
}
