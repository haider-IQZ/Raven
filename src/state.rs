use smithay::{
    backend::renderer::utils::RendererSurfaceStateUserData,
    desktop::{PopupManager, Space, Window, WindowSurfaceType, layer_map_for_output},
    input::{
        Seat, SeatState,
        pointer::{CursorImageStatus, PointerHandle},
    },
    reexports::{
        calloop::{Interest, LoopHandle, LoopSignal, Mode, PostAction, generic::Generic},
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as XdgDecorationMode,
            shell::server::xdg_toplevel,
        },
        wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode as KdeDecorationsMode,
        wayland_server::{
            Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Clock, Logical, Monotonic, Point, SERIAL_COUNTER, Serial},
    wayland::{
        compositor::{CompositorClientState, CompositorState, with_states},
        dmabuf::DmabufState,
        drm_syncobj::DrmSyncobjState,
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        presentation::PresentationState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        shell::{
            kde::decoration::KdeDecorationState,
            wlr_layer::{Layer as WlrLayer, WlrLayerShellState},
            xdg::{XdgShellState, XdgToplevelSurfaceData, decoration::XdgDecorationState},
        },
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
    },
};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs,
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use crate::{
    CompositorError,
    config::{self, RuntimeConfig, WallpaperConfig, WindowRule},
    layout::{GapConfig, LayoutBox, LayoutType},
    protocols::{
        ext_workspace::ExtWorkspaceManagerState,
        foreign_toplevel::ForeignToplevelManagerState,
        wlr_screencopy::{Screencopy, ScreencopyManagerState},
    },
};

pub const WORKSPACE_COUNT: usize = 10;

#[derive(Clone, Copy, Debug)]
pub struct NewWindowRuleDecision {
    pub workspace_index: usize,
    pub floating: bool,
    pub fullscreen: bool,
    pub focus: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Clone, Debug)]
struct PendingInteractiveMove {
    window: Window,
    location: Point<i32, Logical>,
}

#[derive(Clone, Debug)]
struct PendingInteractiveResize {
    window: Window,
    size: smithay::utils::Size<i32, Logical>,
}

pub struct Raven {
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, Raven>,
    pub loop_signal: LoopSignal,

    pub space: Space<Window>,
    pub seat: Seat<Self>,
    pub layout: LayoutBox,
    pub config: RuntimeConfig,
    pub config_path: PathBuf,
    pub socket_name: OsString,
    pub start_time: std::time::Instant,

    // smithay state
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub kde_decoration_state: KdeDecorationState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub data_device_state: DataDeviceState,
    pub seat_state: SeatState<Self>,
    pub popups: PopupManager,
    pub primary_selection_state: PrimarySelectionState,
    pub layer_shell_state: WlrLayerShellState,
    pub ext_workspace_manager_state: ExtWorkspaceManagerState,
    pub foreign_toplevel_manager_state: ForeignToplevelManagerState,
    pub screencopy_state: ScreencopyManagerState,
    pub viewporter_state: ViewporterState,
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    pub presentation_state: PresentationState,

    pub pointer_location: Point<f64, Logical>,
    pub last_pointer_redraw_msec: Option<u32>,
    pub pending_screencopy: Option<Screencopy>,
    pending_interactive_moves: Vec<PendingInteractiveMove>,
    pending_interactive_resizes: Vec<PendingInteractiveResize>,
    pub current_workspace: usize,
    pub workspaces: Vec<Vec<Window>>,
    pub fullscreen_windows: Vec<Window>,
    pub fullscreen_ready_surface_ids: HashSet<u32>,
    pub fullscreen_transition_redraw_outputs: HashMap<String, u8>,
    pub floating_windows: Vec<Window>,
    pub pending_floating_recenter_ids: HashSet<u32>,
    pub pending_window_rule_recheck_ids: HashSet<u32>,
    pub autostart_started: bool,
    pub wallpaper_task_inflight: Arc<AtomicBool>,

    // DRM backend fields
    pub cursor_status: CursorImageStatus,
    pub clock: Clock<Monotonic>,
    pub dmabuf_state: Option<DmabufState>,
    pub syncobj_state: Option<DrmSyncobjState>,
    pub udev_data: Option<crate::backend::udev::UdevData>,
}

impl Raven {
    const SWWW_NAMESPACE: &'static str = "raven";

    pub fn new(
        display: Display<Self>,
        loop_handle: LoopHandle<'static, Raven>,
        loop_signal: LoopSignal,
    ) -> Result<Self, CompositorError> {
        let start_time = std::time::Instant::now();

        let display_handle = display.handle();

        // State
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state =
            XdgDecorationState::new_with_filter::<Self, _>(&display_handle, |client| {
                client
                    .get_data::<ClientState>()
                    .map(|data| data.can_view_decoration_globals)
                    .unwrap_or(false)
            });
        let kde_decoration_state = KdeDecorationState::new_with_filter::<Self, _>(
            &display_handle,
            KdeDecorationsMode::Server,
            |client| {
                client
                    .get_data::<ClientState>()
                    .map(|data| data.can_view_decoration_globals)
                    .unwrap_or(false)
            },
        );
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let popups = PopupManager::default();
        let primary_selection_state = PrimarySelectionState::new::<Self>(&display_handle);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&display_handle);
        let ext_workspace_manager_state =
            ExtWorkspaceManagerState::new::<Self, _>(&display_handle, |_| true);
        let foreign_toplevel_manager_state =
            ForeignToplevelManagerState::new::<Self, _>(&display_handle, |_| true);
        let screencopy_state = ScreencopyManagerState::new::<Self, _>(&display_handle, |_| true);
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);
        let fractional_scale_manager_state =
            FractionalScaleManagerState::new::<Self>(&display_handle);
        // CLOCK_MONOTONIC = 1 on Linux; must match Clock<Monotonic>
        let presentation_state = PresentationState::new::<Self>(&display_handle, 1);
        let mut seat_state = SeatState::new();

        let mut seat = seat_state.new_wl_seat(&display_handle, "winit");
        seat.add_keyboard(Default::default(), 200, 25)
            .expect("failed to add keyboard");
        seat.add_pointer();

        let space = Space::default();

        let socket_name = init_wayland_listener(display, &loop_handle);
        match init_ipc_listener(&loop_handle, &socket_name) {
            Ok(path) => tracing::info!(path = %path.display(), "ipc listener initialized"),
            Err(err) => tracing::warn!("failed to initialize ipc listener: {err}"),
        }

        // TODO: Get a brain
        let layout = LayoutType::from_str("tiling").unwrap().new();
        let loaded_config = config::load_or_create_default()?;
        config::apply_environment(&loaded_config.config);

        let state = Self {
            display_handle,
            loop_handle,
            loop_signal,

            space,
            layout,
            config: loaded_config.config,
            config_path: loaded_config.path,
            seat,
            socket_name,
            start_time,

            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            kde_decoration_state,
            shm_state,
            output_manager_state,
            data_device_state,
            seat_state,
            popups,
            primary_selection_state,
            layer_shell_state,
            ext_workspace_manager_state,
            foreign_toplevel_manager_state,
            screencopy_state,
            viewporter_state,
            fractional_scale_manager_state,
            presentation_state,

            pointer_location: Point::from((0.0, 0.0)),
            last_pointer_redraw_msec: None,
            pending_screencopy: None,
            pending_interactive_moves: Vec::new(),
            pending_interactive_resizes: Vec::new(),
            current_workspace: 0,
            workspaces: vec![Vec::new(); WORKSPACE_COUNT],
            fullscreen_windows: Vec::new(),
            fullscreen_ready_surface_ids: HashSet::new(),
            fullscreen_transition_redraw_outputs: HashMap::new(),
            floating_windows: Vec::new(),
            pending_floating_recenter_ids: HashSet::new(),
            pending_window_rule_recheck_ids: HashSet::new(),
            autostart_started: false,
            wallpaper_task_inflight: Arc::new(AtomicBool::new(false)),

            cursor_status: CursorImageStatus::default_named(),
            clock: Clock::new(),
            dmabuf_state: None,
            syncobj_state: None,
            udev_data: None,
        };

        Self::ensure_portal_preferences_file();
        state.sync_activation_environment();

        Ok(state)
    }

    pub fn apply_layout(&mut self) -> Result<(), CompositorError> {
        let windows: Vec<smithay::desktop::Window> = self.space.elements().cloned().collect();
        if windows.is_empty() {
            return Ok(());
        }

        let output = self
            .space
            .outputs()
            .next()
            .cloned()
            .ok_or_else(|| CompositorError::Backend("no output".into()))?;

        let out_geo = self
            .space
            .output_geometry(&output)
            .ok_or_else(|| CompositorError::Backend("no output geometry".into()))?;

        if let Some(fullscreen_window) = windows
            .iter()
            .find(|window| self.fullscreen_windows.contains(window))
            .cloned()
        {
            let fullscreen_ready =
                self.window_is_ready_fullscreen_on_output(&fullscreen_window, &output);
            let current_location = self.space.element_location(&fullscreen_window);
            let current_geometry = self
                .space
                .element_geometry(&fullscreen_window)
                .unwrap_or_else(|| fullscreen_window.geometry());
            let is_mapped = current_location.is_some();
            let needs_resize = current_geometry.size != out_geo.size;
            let needs_reposition = current_location != Some(out_geo.loc);
            // Avoid repeatedly reconfiguring/remapping an already-correct fullscreen window.
            if !is_mapped || needs_resize {
                self.set_window_fullscreen_state(&fullscreen_window, true);
            }
            // Keep the window's previous position until it has committed a fullscreen-sized frame.
            // This mirrors niri's commit-synchronized fullscreen transition and avoids first-frame
            // bottom-edge flashes from moving the old-size buffer too early.
            if !is_mapped {
                self.space.map_element(fullscreen_window, out_geo.loc, true);
            } else if needs_reposition && fullscreen_ready {
                self.space.map_element(fullscreen_window, out_geo.loc, true);
            }
            return Ok(());
        }

        let tiled_windows: Vec<smithay::desktop::Window> = windows
            .iter()
            .filter(|window| !self.floating_windows.contains(window))
            .cloned()
            .collect();
        if tiled_windows.is_empty() {
            return Ok(());
        }

        let gaps = GapConfig {
            outer_horizontal: self.config.gaps_outer_horizontal,
            outer_vertical: self.config.gaps_outer_vertical,
            inner_horizontal: self.config.gaps_inner_horizontal,
            inner_vertical: self.config.gaps_inner_vertical,
        };

        let master_factor = self.config.master_factor;
        let num_master = self.config.num_master;
        let smartgaps_enabled = self.config.smart_gaps;
        let mut layer_map = layer_map_for_output(&output);
        layer_map.arrange();
        let work_geo = layer_map.non_exclusive_zone();
        let layout_geo = if work_geo.size.w > 0 && work_geo.size.h > 0 {
            work_geo
        } else {
            out_geo
        };

        let geometries = self.layout.arrange(
            &tiled_windows,
            layout_geo.size.w as u32,
            layout_geo.size.h as u32,
            &gaps,
            master_factor,
            num_master,
            smartgaps_enabled,
        );

        for (window, geom) in tiled_windows.into_iter().zip(geometries.into_iter()) {
            let loc = Point::<i32, Logical>::from((
                layout_geo.loc.x + geom.x_coordinate,
                layout_geo.loc.y + geom.y_coordinate,
            ));
            let desired_size = (geom.width as i32, geom.height as i32).into();
            let current_location = self.space.element_location(&window);
            let current_geometry = self
                .space
                .element_geometry(&window)
                .unwrap_or_else(|| window.geometry());
            let is_mapped = current_location.is_some();
            let needs_resize = current_geometry.size != desired_size;
            let needs_reposition = current_location != Some(loc);

            if let Some(toplevel) = window.toplevel()
                && (!is_mapped || needs_resize)
            {
                toplevel.with_pending_state(|state| {
                    state.size = Some(desired_size);
                    state.bounds = Some(layout_geo.size);
                    state.states.unset(xdg_toplevel::State::Fullscreen);
                    state.states.unset(xdg_toplevel::State::Maximized);
                    state.states.set(xdg_toplevel::State::TiledLeft);
                    state.states.set(xdg_toplevel::State::TiledRight);
                    state.states.set(xdg_toplevel::State::TiledTop);
                    state.states.set(xdg_toplevel::State::TiledBottom);
                });
                toplevel.send_pending_configure();
            }

            if !is_mapped || needs_reposition {
                self.space.map_element(window, loc, false);
            }
        }

        Ok(())
    }

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|window| {
                window
                    .toplevel()
                    .is_some_and(|tl| tl.wl_surface() == surface)
            })
            .cloned()
            .or_else(|| {
                self.workspaces
                    .iter()
                    .flatten()
                    .find(|window| {
                        window
                            .toplevel()
                            .is_some_and(|tl| tl.wl_surface() == surface)
                    })
                    .cloned()
            })
    }

    pub fn window_under_pointer(&self) -> Option<(Window, Point<i32, Logical>)> {
        self.space
            .element_under(self.pointer_location)
            .map(|(w, p)| (w.clone(), p))
    }

    pub fn surface_under_pointer(&self) -> Option<(WlSurface, Point<f64, Logical>)> {
        let position = self.pointer_location;
        let (output, output_geo) = self.space.outputs().find_map(|output| {
            let geometry = self.space.output_geometry(output)?;
            if geometry.to_f64().contains(position) {
                Some((output, geometry))
            } else {
                None
            }
        })?;

        let layer_map = layer_map_for_output(output);
        let position_within_output = position - output_geo.loc.to_f64();
        let fullscreen_on_output = self.output_has_ready_fullscreen_window(output);

        let surface_on_layer = |layer: WlrLayer| {
            layer_map.layers_on(layer).rev().find_map(|layer_surface| {
                let layer_geo = layer_map.layer_geometry(layer_surface)?;
                layer_surface
                    .surface_under(
                        position_within_output - layer_geo.loc.to_f64(),
                        WindowSurfaceType::ALL,
                    )
                    .map(|(surface, local_pos)| {
                        (
                            surface,
                            output_geo.loc.to_f64() + layer_geo.loc.to_f64() + local_pos.to_f64(),
                        )
                    })
            })
        };

        let window_surface = || {
            self.space.element_under(position).and_then(|(window, location)| {
                window
                    .surface_under(position - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(surface, local_pos)| (surface, (local_pos + location).to_f64()))
            })
        };

        if fullscreen_on_output {
            surface_on_layer(WlrLayer::Overlay)
                .or_else(window_surface)
                .or_else(|| surface_on_layer(WlrLayer::Top))
                .or_else(|| surface_on_layer(WlrLayer::Bottom))
                .or_else(|| surface_on_layer(WlrLayer::Background))
        } else {
            surface_on_layer(WlrLayer::Overlay)
                .or_else(|| surface_on_layer(WlrLayer::Top))
                .or_else(window_surface)
                .or_else(|| surface_on_layer(WlrLayer::Bottom))
                .or_else(|| surface_on_layer(WlrLayer::Background))
        }
    }

    pub fn pointer(&self) -> PointerHandle<Self> {
        self.seat.get_pointer().expect("pointer not initialized")
    }

    pub fn add_window_to_current_workspace(&mut self, window: Window) {
        self.add_window_to_workspace(self.current_workspace, window);
    }

    pub fn add_window_to_workspace(&mut self, workspace_index: usize, window: Window) {
        let Some(workspace) = self.workspaces.get_mut(workspace_index) else {
            tracing::warn!("attempted to add window to invalid workspace index {workspace_index}");
            return;
        };
        if !workspace.contains(&window) {
            workspace.push(window);
        }
    }

    pub fn set_window_floating(&mut self, window: &Window, floating: bool) {
        if floating {
            if !self.floating_windows.contains(window) {
                self.floating_windows.push(window.clone());
            }
        } else {
            self.floating_windows
                .retain(|candidate| candidate != window);
        }
    }

    pub fn is_window_floating(&self, window: &Window) -> bool {
        self.floating_windows.contains(window)
    }

    pub fn output_has_fullscreen_window(&self, output: &smithay::output::Output) -> bool {
        self.fullscreen_windows.iter().any(|window| {
            self.space
                .outputs_for_element(window)
                .iter()
                .any(|candidate| candidate == output)
        })
    }

    pub fn output_has_ready_fullscreen_window(&self, output: &smithay::output::Output) -> bool {
        self.fullscreen_windows.iter().any(|window| {
            self.window_is_ready_fullscreen_on_output(window, output)
        })
    }

    fn window_is_ready_fullscreen_on_output(
        &self,
        window: &Window,
        output: &smithay::output::Output,
    ) -> bool {
        let Some(surface_id) = Self::window_surface_id(window) else {
            return false;
        };
        self.fullscreen_ready_surface_ids.contains(&surface_id)
            && self
                .space
                .outputs_for_element(window)
                .iter()
                .any(|candidate| candidate == output)
    }

    pub fn mark_fullscreen_ready_for_surface(&mut self, surface: &WlSurface) {
        let surface_id = surface.id().protocol_id();
        let Some(window) = self.window_for_surface(surface) else {
            return;
        };
        if !self.fullscreen_windows.contains(&window) {
            return;
        }

        let Some(output) = self.space.outputs_for_element(&window).into_iter().next() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        let window_geo = self
            .space
            .element_geometry(&window)
            .unwrap_or_else(|| window.geometry());
        const FULLSCREEN_READY_TOLERANCE: i32 = 2;
        let fullscreen_sized = window_geo.size.w + FULLSCREEN_READY_TOLERANCE >= output_geo.size.w
            && window_geo.size.h + FULLSCREEN_READY_TOLERANCE >= output_geo.size.h;
        let surface_buffer_sized = with_states(surface, |states| {
            states
                .data_map
                .get::<RendererSurfaceStateUserData>()
                .and_then(|data| data.lock().ok())
                .and_then(|data| data.buffer_size())
                .is_some_and(|size| {
                    size.w + FULLSCREEN_READY_TOLERANCE >= output_geo.size.w
                        && size.h + FULLSCREEN_READY_TOLERANCE >= output_geo.size.h
                })
        });

        let is_committed_fullscreen = window.toplevel().is_some_and(|toplevel| {
            toplevel.with_committed_state(|state| {
                state
                    .as_ref()
                    .is_some_and(|state| state.states.contains(xdg_toplevel::State::Fullscreen))
            })
        });

        if !is_committed_fullscreen || !fullscreen_sized || !surface_buffer_sized {
            self.fullscreen_ready_surface_ids.remove(&surface_id);
            return;
        }

        if self.fullscreen_ready_surface_ids.contains(&surface_id) {
            return;
        }

        let became_ready = self.fullscreen_ready_surface_ids.insert(surface_id);
        if became_ready {
            if self.space.element_location(&window) != Some(output_geo.loc) {
                self.space.map_element(window.clone(), output_geo.loc, true);
            }
            self.mark_fullscreen_transition_redraw_for_window(&window);
        }
    }

    pub fn clear_fullscreen_ready_for_window(&mut self, window: &Window) {
        if let Some(surface_id) = Self::window_surface_id(window) {
            self.fullscreen_ready_surface_ids.remove(&surface_id);
        }
        for output in self.space.outputs_for_element(window) {
            self.fullscreen_transition_redraw_outputs
                .remove(&output.name());
        }
    }

    pub fn take_fullscreen_transition_redraw_for_output(
        &mut self,
        output: &smithay::output::Output,
    ) -> bool {
        let key = output.name();
        let Some(frames_left) = self.fullscreen_transition_redraw_outputs.get_mut(&key) else {
            return false;
        };

        let should_redraw = *frames_left > 0;
        if *frames_left <= 1 {
            self.fullscreen_transition_redraw_outputs.remove(&key);
        } else {
            *frames_left -= 1;
        }
        should_redraw
    }

    pub fn queue_fullscreen_transition_redraw_for_window(&mut self, window: &Window) {
        self.mark_fullscreen_transition_redraw_for_window(window);
    }

    fn window_surface_id(window: &Window) -> Option<u32> {
        window
            .toplevel()
            .map(|toplevel| toplevel.wl_surface().id().protocol_id())
    }

    fn mark_fullscreen_transition_redraw_for_window(&mut self, window: &Window) {
        const FULLSCREEN_TRANSITION_REDRAW_FRAMES: u8 = 4;
        let mut outputs = self.space.outputs_for_element(window);
        if outputs.is_empty() {
            // During some transitions the window/output mapping can lag by a commit.
            // Fall back to all outputs so we don't miss the first fullscreen frames.
            outputs = self.space.outputs().cloned().collect();
        }
        for output in outputs {
            let key = output.name();
            self.fullscreen_transition_redraw_outputs
                .entry(key)
                .and_modify(|frames| {
                    *frames = (*frames).max(FULLSCREEN_TRANSITION_REDRAW_FRAMES);
                })
                .or_insert(FULLSCREEN_TRANSITION_REDRAW_FRAMES);
        }
    }

    fn active_output_for_pointer(&self) -> Option<smithay::output::Output> {
        self.space
            .outputs()
            .find(|output| {
                self.space
                    .output_geometry(output)
                    .is_some_and(|geo| geo.to_f64().contains(self.pointer_location))
            })
            .cloned()
            .or_else(|| self.space.outputs().next().cloned())
    }

    fn default_floating_location(&self, window: &Window) -> (i32, i32) {
        self.active_output_for_pointer()
            .as_ref()
            .and_then(|output| {
                let mut layer_map = layer_map_for_output(output);
                layer_map.arrange();
                let work_geo = layer_map.non_exclusive_zone();
                if work_geo.size.w > 0 && work_geo.size.h > 0 {
                    Some(work_geo)
                } else {
                    self.space.output_geometry(output)
                }
            })
            .map(|geometry| {
                let window_geo = window.geometry();
                let window_width = window_geo.size.w.clamp(1, geometry.size.w);
                let window_height = window_geo.size.h.clamp(1, geometry.size.h);
                let x = geometry.loc.x + (geometry.size.w - window_width) / 2;
                let y = geometry.loc.y + (geometry.size.h - window_height) / 2;
                (x, y)
            })
            .unwrap_or((80, 80))
    }

    pub fn initial_map_location_for_window(&self, window: &Window) -> (i32, i32) {
        if self.is_window_floating(window) {
            self.default_floating_location(window)
        } else {
            (0, 0)
        }
    }

    pub fn queue_interactive_move(&mut self, window: &Window, location: Point<i32, Logical>) {
        if let Some(pending) = self
            .pending_interactive_moves
            .iter_mut()
            .find(|pending| pending.window == *window)
        {
            pending.location = location;
            return;
        }
        self.pending_interactive_moves.push(PendingInteractiveMove {
            window: window.clone(),
            location,
        });
    }

    pub fn clear_pending_interactive_move(&mut self, window: &Window) {
        self.pending_interactive_moves
            .retain(|pending| pending.window != *window);
    }

    pub fn queue_interactive_resize(
        &mut self,
        window: &Window,
        size: smithay::utils::Size<i32, Logical>,
    ) {
        if let Some(pending) = self
            .pending_interactive_resizes
            .iter_mut()
            .find(|pending| pending.window == *window)
        {
            pending.size = size;
            return;
        }
        self.pending_interactive_resizes
            .push(PendingInteractiveResize {
                window: window.clone(),
                size,
            });
    }

    pub fn clear_pending_interactive_resize(&mut self, window: &Window) {
        self.pending_interactive_resizes
            .retain(|pending| pending.window != *window);
    }

    pub fn flush_interactive_frame_updates(&mut self) {
        let pending_moves = std::mem::take(&mut self.pending_interactive_moves);
        for pending in pending_moves {
            self.space
                .map_element(pending.window, pending.location, false);
        }

        let pending_resizes = std::mem::take(&mut self.pending_interactive_resizes);
        for pending in pending_resizes {
            let Some(toplevel) = pending.window.toplevel() else {
                continue;
            };
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
                state.size = Some(pending.size);
            });
            toplevel.send_pending_configure();
        }
    }

    fn surface_app_id_and_title(surface: &WlSurface) -> (Option<String>, Option<String>) {
        with_states(surface, |states| {
            let role = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .expect("xdg toplevel role data missing")
                .lock()
                .expect("xdg toplevel role lock poisoned");

            let app_id = role
                .app_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            let title = role
                .title
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            (app_id, title)
        })
    }

    pub fn queue_window_rule_recheck_for_surface(&mut self, surface: &WlSurface) {
        if self.should_defer_window_rules_for_surface(surface) {
            self.pending_window_rule_recheck_ids
                .insert(surface.id().protocol_id());
        }
    }

    pub fn queue_floating_recenter_for_surface(&mut self, surface: &WlSurface) {
        self.pending_floating_recenter_ids
            .insert(surface.id().protocol_id());
    }

    pub fn clear_floating_recenter_for_surface(&mut self, surface: &WlSurface) {
        self.pending_floating_recenter_ids
            .remove(&surface.id().protocol_id());
    }

    pub fn clear_window_rule_recheck_for_surface(&mut self, surface: &WlSurface) {
        self.pending_window_rule_recheck_ids
            .remove(&surface.id().protocol_id());
    }

    fn should_defer_window_rules_for_surface(&self, surface: &WlSurface) -> bool {
        if self.config.window_rules.is_empty() {
            return false;
        }

        let (app_id, title) = Self::surface_app_id_and_title(surface);
        for rule in &self.config.window_rules {
            if (rule.class.is_some() || rule.app_id.is_some()) && app_id.is_none() {
                return true;
            }
            if rule.title.is_some() && title.is_none() {
                return true;
            }
        }

        false
    }

    pub fn resolve_window_rules_for_surface(&self, surface: &WlSurface) -> NewWindowRuleDecision {
        let (app_id, title) = Self::surface_app_id_and_title(surface);

        let mut decision = NewWindowRuleDecision {
            workspace_index: self.current_workspace,
            floating: false,
            fullscreen: false,
            focus: true,
            width: None,
            height: None,
        };

        for rule in &self.config.window_rules {
            if !rule.matches(app_id.as_deref(), title.as_deref()) {
                continue;
            }
            Self::apply_window_rule_to_decision(rule, &mut decision);
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

    pub fn apply_window_rule_size_to_window(
        &self,
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

    fn workspace_index_for_window(&self, window: &Window) -> Option<usize> {
        self.workspaces
            .iter()
            .position(|workspace| workspace.contains(window))
    }

    fn move_window_to_workspace_internal(
        &mut self,
        window: &Window,
        target_workspace: usize,
    ) -> Result<(), CompositorError> {
        if target_workspace >= self.workspaces.len() {
            return Err(CompositorError::Backend(format!(
                "invalid workspace index {target_workspace}"
            )));
        }

        let source_workspace = self.workspace_index_for_window(window);

        match source_workspace {
            Some(source_workspace) if source_workspace == target_workspace => {}
            Some(source_workspace) => {
                self.workspaces[source_workspace].retain(|candidate| candidate != window);
                if !self.workspaces[target_workspace].contains(window) {
                    self.workspaces[target_workspace].push(window.clone());
                }

                if source_workspace == self.current_workspace {
                    self.space.unmap_elem(window);
                }
                if target_workspace == self.current_workspace {
                    let loc = self.initial_map_location_for_window(window);
                    self.space.map_element(window.clone(), loc, false);
                }
            }
            None => {
                self.add_window_to_workspace(target_workspace, window.clone());
                if target_workspace == self.current_workspace {
                    let loc = self.initial_map_location_for_window(window);
                    self.space.map_element(window.clone(), loc, false);
                }
            }
        }

        Ok(())
    }

    pub fn maybe_apply_deferred_window_rules(&mut self, surface: &WlSurface) {
        let surface_id = surface.id().protocol_id();
        if !self.pending_window_rule_recheck_ids.contains(&surface_id) {
            return;
        }

        let Some(window) = self.window_for_surface(surface) else {
            self.pending_window_rule_recheck_ids.remove(&surface_id);
            return;
        };

        let (app_id, title) = Self::surface_app_id_and_title(surface);
        if app_id.is_none() && title.is_none() {
            return;
        }

        let decision = self.resolve_window_rules_for_surface(surface);
        if let Err(err) = self.move_window_to_workspace_internal(&window, decision.workspace_index)
        {
            tracing::warn!("failed to move window after deferred rule resolution: {err}");
        }
        self.apply_window_rule_size_to_window(&window, &decision);
        if let Some(toplevel) = window.toplevel()
            && toplevel.is_initial_configure_sent()
        {
            toplevel.send_pending_configure();
        }

        let was_floating = self.is_window_floating(&window);
        self.set_window_floating(&window, decision.floating);
        if decision.floating && self.is_window_mapped(&window) {
            let loc = self.initial_map_location_for_window(&window);
            // Re-center when metadata arrives and after first commit size settles.
            // This centers in the working area after geometry settles.
            self.space.map_element(window.clone(), loc, !was_floating);
            self.queue_floating_recenter_for_surface(surface);
        }

        if decision.fullscreen && !self.fullscreen_windows.contains(&window) {
            let existing = self.fullscreen_windows.clone();
            for fullscreen_window in &existing {
                self.clear_fullscreen_ready_for_window(fullscreen_window);
                self.set_window_fullscreen_state(fullscreen_window, false);
            }
            self.fullscreen_windows.clear();
            self.fullscreen_windows.push(window.clone());
            self.clear_fullscreen_ready_for_window(&window);
            self.set_window_fullscreen_state(&window, true);
            self.queue_fullscreen_transition_redraw_for_window(&window);
        }

        if let Err(err) = self.apply_layout() {
            tracing::warn!("failed to apply layout after deferred rule resolution: {err}");
        }

        if decision.focus && decision.workspace_index == self.current_workspace {
            self.set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
        }

        self.pending_window_rule_recheck_ids.remove(&surface_id);
    }

    pub fn maybe_recenter_floating_window_after_commit(&mut self, surface: &WlSurface) {
        let surface_id = surface.id().protocol_id();
        if !self.pending_floating_recenter_ids.contains(&surface_id) {
            return;
        }

        let Some(window) = self.window_for_surface(surface) else {
            self.pending_floating_recenter_ids.remove(&surface_id);
            return;
        };
        if !self.is_window_floating(&window) || !self.is_window_mapped(&window) {
            self.pending_floating_recenter_ids.remove(&surface_id);
            return;
        }

        let size = window.geometry().size;
        if size.w <= 1 || size.h <= 1 {
            // Wait for the first commit with a real window size.
            return;
        }

        let loc = self.initial_map_location_for_window(&window);
        self.space.map_element(window, loc, false);
        self.pending_floating_recenter_ids.remove(&surface_id);
    }

    fn write_ipc_response(stream: &mut UnixStream, message: &str) {
        if let Err(err) = stream.write_all(message.as_bytes()) {
            tracing::warn!("failed to write ipc response: {err}");
        }
    }

    pub fn handle_ipc_stream(&mut self, mut stream: UnixStream) {
        let mut request = String::new();
        if let Err(err) = stream.read_to_string(&mut request) {
            Self::write_ipc_response(
                &mut stream,
                &format!("error: failed to read request: {err}\n"),
            );
            return;
        }

        match request.trim() {
            "clients" => {
                let output = self.render_clients_report();
                Self::write_ipc_response(&mut stream, &output);
            }
            "reload" => match self.reload_config() {
                Ok(()) => Self::write_ipc_response(&mut stream, "ok\n"),
                Err(err) => Self::write_ipc_response(&mut stream, &format!("error: {err}\n")),
            },
            "" => {
                Self::write_ipc_response(
                    &mut stream,
                    "error: empty command (supported: clients, reload)\n",
                );
            }
            other => {
                Self::write_ipc_response(
                    &mut stream,
                    &format!("error: unsupported command `{other}` (supported: clients, reload)\n"),
                );
            }
        }
    }

    fn render_clients_report(&self) -> String {
        let focused_surface = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());

        let mut seen_surfaces = HashSet::new();
        let mut windows = Vec::new();
        for window in self
            .workspaces
            .iter()
            .flatten()
            .chain(self.space.elements())
        {
            let Some(toplevel) = window.toplevel() else {
                continue;
            };
            let surface = toplevel.wl_surface();
            if seen_surfaces.insert(surface.clone()) {
                windows.push(window.clone());
            }
        }

        if windows.is_empty() {
            return "No clients.\n".to_owned();
        }

        let mut out = String::new();
        for (index, window) in windows.iter().enumerate() {
            let Some(toplevel) = window.toplevel() else {
                continue;
            };
            let wl_surface = toplevel.wl_surface().clone();

            let (app_id, title) = with_states(&wl_surface, |states| {
                let role = states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .expect("xdg toplevel role data missing")
                    .lock()
                    .expect("xdg toplevel role lock poisoned");
                (role.app_id.clone(), role.title.clone())
            });

            let workspace = self
                .workspaces
                .iter()
                .position(|ws| ws.contains(window))
                .map(|idx| idx + 1)
                .unwrap_or(self.current_workspace + 1);

            let class = app_id.as_deref().unwrap_or("<unknown>");
            let title = title.as_deref().unwrap_or("<untitled>");
            let focused = focused_surface.as_ref() == Some(&wl_surface);
            let mapped = self.is_window_mapped(window);
            let floating = self.is_window_floating(window);
            let fullscreen = self.fullscreen_windows.contains(window);
            let surface_id = format!("{:?}", wl_surface.id());

            out.push_str(&format!("Client {}:\n", index + 1));
            out.push_str(&format!("  surface: {surface_id}\n"));
            out.push_str(&format!("  class: {class}\n"));
            out.push_str(&format!("  title: {title}\n"));
            out.push_str(&format!("  workspace: {workspace}\n"));
            out.push_str(&format!("  mapped: {mapped}\n"));
            out.push_str(&format!("  floating: {floating}\n"));
            out.push_str(&format!("  fullscreen: {fullscreen}\n"));
            out.push_str(&format!("  focused: {focused}\n"));
            out.push('\n');
        }

        out
    }

    fn is_window_mapped(&self, window: &Window) -> bool {
        self.space.elements().any(|candidate| candidate == window)
    }

    pub fn sync_window_activation(&self, focused_window: Option<&Window>) {
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
                // Avoid sending pending configure before a window got its initial
                // configure; this can create unstable negotiation on fresh toplevels.
                if toplevel.is_initial_configure_sent() {
                    toplevel.send_pending_configure();
                }
            }
        }
    }

    pub fn set_keyboard_focus(&mut self, target: Option<WlSurface>, serial: Serial) {
        let current_focus = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        if current_focus.as_ref() == target.as_ref() {
            return;
        }

        let focused_window = target
            .as_ref()
            .and_then(|surface| self.window_for_surface(surface));
        self.sync_window_activation(focused_window.as_ref());

        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, target, serial);
        }
    }

    pub fn refocus_visible_window(&mut self) {
        if let Some(focused_surface) = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            && let Some(window) = self.window_for_surface(&focused_surface)
            && self.is_window_mapped(&window)
        {
            self.sync_window_activation(Some(&window));
            return;
        }

        let serial = SERIAL_COUNTER.next_serial();
        let pointer_target = self.window_under_pointer().and_then(|(window, _)| {
            window
                .toplevel()
                .map(|toplevel| toplevel.wl_surface().clone())
        });

        let fallback_target = self.space.elements().last().and_then(|window| {
            window
                .toplevel()
                .map(|toplevel| toplevel.wl_surface().clone())
        });

        let target = pointer_target.or(fallback_target);
        self.set_keyboard_focus(target, serial);
    }

    pub fn remove_window_from_workspaces(&mut self, window: &Window) {
        self.clear_fullscreen_ready_for_window(window);
        for workspace in &mut self.workspaces {
            workspace.retain(|candidate| candidate != window);
        }
        self.fullscreen_windows
            .retain(|candidate| candidate != window);
        self.floating_windows
            .retain(|candidate| candidate != window);
    }

    pub fn switch_workspace(&mut self, target_workspace: usize) -> Result<(), CompositorError> {
        if target_workspace >= self.workspaces.len() {
            return Err(CompositorError::Backend(format!(
                "invalid workspace index {target_workspace}"
            )));
        }

        if target_workspace == self.current_workspace {
            return Ok(());
        }

        let current_windows = self.workspaces[self.current_workspace].clone();
        for window in &current_windows {
            self.space.unmap_elem(window);
        }

        self.current_workspace = target_workspace;

        let target_windows = self.workspaces[target_workspace].clone();
        for window in target_windows {
            let loc = self.initial_map_location_for_window(&window);
            self.space.map_element(window, loc, false);
        }

        self.apply_layout()?;
        self.refocus_visible_window();
        self.refresh_ext_workspace();
        Ok(())
    }

    pub fn move_focused_window_to_workspace(
        &mut self,
        target_workspace: usize,
    ) -> Result<(), CompositorError> {
        if target_workspace >= self.workspaces.len() {
            return Err(CompositorError::Backend(format!(
                "invalid workspace index {target_workspace}"
            )));
        }

        let Some(keyboard) = self.seat.get_keyboard() else {
            return Ok(());
        };
        let Some(focused_surface) = keyboard.current_focus() else {
            return Ok(());
        };
        let Some(window) = self.window_for_surface(&focused_surface) else {
            return Ok(());
        };

        let source_workspace = self
            .workspaces
            .iter()
            .position(|workspace| workspace.contains(&window))
            .unwrap_or(self.current_workspace);

        if source_workspace == target_workspace {
            return Ok(());
        }

        self.workspaces[source_workspace].retain(|candidate| candidate != &window);
        if !self.workspaces[target_workspace].contains(&window) {
            self.workspaces[target_workspace].push(window.clone());
        }

        if source_workspace == self.current_workspace {
            self.space.unmap_elem(&window);
            self.apply_layout()?;
            self.refocus_visible_window();
        } else if target_workspace == self.current_workspace {
            let loc = self.initial_map_location_for_window(&window);
            self.space.map_element(window, loc, false);
            self.apply_layout()?;
        }

        self.refresh_ext_workspace();
        Ok(())
    }

    pub fn spawn_terminal(&self) {
        self.spawn_command(&self.config.terminal);
    }

    pub fn spawn_launcher(&self) {
        self.spawn_command(&self.config.launcher);
    }

    fn infer_command_program(command: &str) -> Option<&str> {
        let mut saw_env = false;
        for token in command.split_whitespace() {
            if token.is_empty() {
                continue;
            }

            if !saw_env && token == "env" {
                saw_env = true;
                continue;
            }

            if token.contains('=') && !token.starts_with('/') && !token.starts_with("./") {
                continue;
            }

            let program = token.rsplit('/').next().unwrap_or(token);
            return Some(program);
        }

        None
    }

    fn apply_no_csd_spawn_overrides(&self, command: &str) -> String {
        let trimmed = command.trim();
        if trimmed.is_empty() || !self.config.no_csd {
            return trimmed.to_owned();
        }

        let lower = trimmed.to_ascii_lowercase();
        let Some(program) = Self::infer_command_program(trimmed) else {
            return trimmed.to_owned();
        };

        match program {
            "alacritty" => {
                if lower.contains("window.decorations=") {
                    trimmed.to_owned()
                } else {
                    format!("{trimmed} -o window.decorations=None")
                }
            }
            "kitty" => {
                if lower.contains("hide_window_decorations") {
                    trimmed.to_owned()
                } else {
                    format!("{trimmed} -o hide_window_decorations=yes")
                }
            }
            "wezterm" => {
                if lower.contains("window_decorations=") {
                    trimmed.to_owned()
                } else {
                    format!("{trimmed} --config window_decorations=NONE")
                }
            }
            _ => trimmed.to_owned(),
        }
    }

    fn apply_wayland_browser_spawn_overrides(&self, command: &str) -> String {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return trimmed.to_owned();
        }

        let lower = trimmed.to_ascii_lowercase();
        let Some(program) = Self::infer_command_program(trimmed) else {
            return trimmed.to_owned();
        };

        let is_chromium_family = matches!(
            program,
            "brave"
                | "brave-browser"
                | "chromium"
                | "chromium-browser"
                | "google-chrome"
                | "chrome"
                | "microsoft-edge"
        );

        if !is_chromium_family {
            return trimmed.to_owned();
        }

        let mut out = trimmed.to_owned();
        if !lower.contains("--ozone-platform=") && !lower.contains("--ozone-platform-hint=") {
            out.push_str(" --ozone-platform=wayland");
        }

        // Select Chromium sync mode based on compositor capability and env overrides.
        if !lower.contains("waylandlinuxdrmsyncobj") {
            if self.chromium_explicit_sync_enabled() {
                out.push_str(" --enable-features=WaylandLinuxDrmSyncobj");
            } else {
                out.push_str(" --disable-features=WaylandLinuxDrmSyncobj");
            }
        }

        out
    }

    fn chromium_explicit_sync_enabled(&self) -> bool {
        let parse_bool = |name: &str| {
            std::env::var_os(name).map(|value| {
                let value = value.to_string_lossy().to_ascii_lowercase();
                matches!(value.as_str(), "1" | "true" | "yes" | "on")
            })
        };

        // Hard disable always wins.
        if parse_bool("RAVEN_DISABLE_EXPLICIT_SYNC").unwrap_or(false)
            || parse_bool("RAVEN_CHROMIUM_DISABLE_EXPLICIT_SYNC").unwrap_or(false)
        {
            return false;
        }

        // Explicit user override if provided.
        if let Some(explicit) = parse_bool("RAVEN_CHROMIUM_EXPLICIT_SYNC") {
            return explicit;
        }

        // Default to explicit sync only when the compositor exposed syncobj protocol.
        self.syncobj_state.is_some()
    }

    fn apply_wayland_child_env(&self, cmd: &mut Command) {
        cmd.env("WAYLAND_DISPLAY", &self.socket_name);
        if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            cmd.env("XDG_RUNTIME_DIR", runtime_dir);
        }
        cmd.env("XDG_SESSION_TYPE", "wayland");
        cmd.env("XDG_CURRENT_DESKTOP", "raven");
        cmd.env("XDG_SESSION_DESKTOP", "raven");
        cmd.env("MOZ_ENABLE_WAYLAND", "1");
        cmd.env("MOZ_DBUS_REMOTE", "0");
        cmd.env("QT_QPA_PLATFORM", "wayland");
        cmd.env("SDL_VIDEODRIVER", "wayland");
        // Prefer native Wayland paths for Chromium/Electron apps on NixOS/NVIDIA.
        cmd.env("NIXOS_OZONE_WL", "1");
        cmd.env("OZONE_PLATFORM", "wayland");
        cmd.env("OZONE_PLATFORM_HINT", "wayland");
        cmd.env("ELECTRON_OZONE_PLATFORM_HINT", "wayland");
        let chromium_sync_flags = if self.chromium_explicit_sync_enabled() {
            "--enable-features=WaylandLinuxDrmSyncobj"
        } else {
            "--disable-features=WaylandLinuxDrmSyncobj"
        };
        cmd.env("CHROMIUM_FLAGS", chromium_sync_flags);
        cmd.env("BRAVE_USER_FLAGS", chromium_sync_flags);
        if self.config.no_csd {
            cmd.env("QT_WAYLAND_DISABLE_WINDOWDECORATION", "1");
        } else {
            cmd.env_remove("QT_WAYLAND_DISABLE_WINDOWDECORATION");
        }
        if self.config.xwayland.enabled {
            cmd.env("DISPLAY", self.config.xwayland.display.trim());
        } else {
            cmd.env_remove("DISPLAY");
        }
        cmd.env_remove("HYPRLAND_INSTANCE_SIGNATURE");
        cmd.env_remove("HYPRLAND_CMD");
        cmd.env_remove("SWAYSOCK");
        cmd.env_remove("SWWW_SOCKET");
        cmd.env_remove("SWWW_DAEMON_SOCKET");
        cmd.env_remove("SWWW_NAMESPACE");
    }

    pub(crate) fn sync_activation_environment(&self) {
        let chromium_sync_flags = if self.chromium_explicit_sync_enabled() {
            "--enable-features=WaylandLinuxDrmSyncobj"
        } else {
            "--disable-features=WaylandLinuxDrmSyncobj"
        };
        let mut env_kv = vec![
            format!("WAYLAND_DISPLAY={}", self.socket_name.to_string_lossy()),
            "XDG_CURRENT_DESKTOP=raven".to_owned(),
            "XDG_SESSION_TYPE=wayland".to_owned(),
            "XDG_SESSION_DESKTOP=raven".to_owned(),
            "GDK_BACKEND=".to_owned(),
            "QT_QPA_PLATFORM=".to_owned(),
            "SDL_VIDEODRIVER=".to_owned(),
            "MOZ_ENABLE_WAYLAND=".to_owned(),
            "MOZ_DBUS_REMOTE=".to_owned(),
            format!("CHROMIUM_FLAGS={chromium_sync_flags}"),
            format!("BRAVE_USER_FLAGS={chromium_sync_flags}"),
        ];

        if self.config.xwayland.enabled {
            env_kv.push(format!("DISPLAY={}", self.config.xwayland.display.trim()));
        } else {
            env_kv.push("DISPLAY=".to_owned());
        }

        if self.config.no_csd {
            env_kv.push("QT_WAYLAND_DISABLE_WINDOWDECORATION=1".to_owned());
        } else {
            env_kv.push("QT_WAYLAND_DISABLE_WINDOWDECORATION=".to_owned());
        }

        let mut dbus_args = vec!["--systemd".to_owned()];
        dbus_args.extend(env_kv.iter().cloned());
        match Command::new("dbus-update-activation-environment")
            .args(&dbus_args)
            .output()
        {
            Ok(output) if output.status.success() => {
                tracing::info!("synced activation env via dbus-update-activation-environment");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                tracing::warn!(
                    status = ?output.status.code(),
                    stderr,
                    "failed to sync activation env via dbus-update-activation-environment"
                );
            }
            Err(err) => {
                tracing::warn!("failed to execute dbus-update-activation-environment: {err}");
            }
        }

        let mut systemd_env_kv = vec![
            format!("WAYLAND_DISPLAY={}", self.socket_name.to_string_lossy()),
            "XDG_CURRENT_DESKTOP=raven".to_owned(),
            "XDG_SESSION_TYPE=wayland".to_owned(),
            "XDG_SESSION_DESKTOP=raven".to_owned(),
            format!("CHROMIUM_FLAGS={chromium_sync_flags}"),
            format!("BRAVE_USER_FLAGS={chromium_sync_flags}"),
        ];
        if self.config.xwayland.enabled {
            systemd_env_kv.push(format!("DISPLAY={}", self.config.xwayland.display.trim()));
        }
        if self.config.no_csd {
            systemd_env_kv.push("QT_WAYLAND_DISABLE_WINDOWDECORATION=1".to_owned());
        } else {
            systemd_env_kv.push("QT_WAYLAND_DISABLE_WINDOWDECORATION=".to_owned());
        }

        match Command::new("systemctl")
            .arg("--user")
            .arg("set-environment")
            .args(&systemd_env_kv)
            .output()
        {
            Ok(output) if output.status.success() => {
                tracing::info!("synced activation env via systemctl --user set-environment");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                tracing::warn!(
                    status = ?output.status.code(),
                    stderr,
                    "failed to sync activation env via systemctl --user set-environment"
                );
            }
            Err(err) => {
                tracing::warn!("failed to execute systemctl --user set-environment: {err}");
            }
        }

        match Command::new("systemctl")
            .arg("--user")
            .arg("unset-environment")
            .args([
                "GDK_BACKEND",
                "QT_QPA_PLATFORM",
                "SDL_VIDEODRIVER",
                "MOZ_ENABLE_WAYLAND",
                "MOZ_DBUS_REMOTE",
            ])
            .output()
        {
            Ok(output) if output.status.success() => {
                tracing::info!("cleared portal-sensitive vars from systemctl --user environment");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                tracing::warn!(
                    status = ?output.status.code(),
                    stderr,
                    "failed to clear vars via systemctl --user unset-environment"
                );
            }
            Err(err) => {
                tracing::warn!("failed to execute systemctl --user unset-environment: {err}");
            }
        }

        if !self.config.xwayland.enabled {
            match Command::new("systemctl")
                .arg("--user")
                .arg("unset-environment")
                .arg("DISPLAY")
                .output()
            {
                Ok(output) if output.status.success() => {
                    tracing::info!("cleared DISPLAY from systemctl --user environment");
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                    tracing::warn!(
                        status = ?output.status.code(),
                        stderr,
                        "failed to clear DISPLAY via systemctl --user unset-environment"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        "failed to execute systemctl --user unset-environment DISPLAY: {err}"
                    );
                }
            }
        }
    }

    pub fn spawn_command(&self, command: &str) {
        if command.trim().is_empty() {
            return;
        }

        let command = self.apply_no_csd_spawn_overrides(command);
        let command = self.apply_wayland_browser_spawn_overrides(&command);
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&command);
        self.apply_wayland_child_env(&mut cmd);

        if let Err(err) = cmd.spawn() {
            tracing::warn!(command = %command, "failed to spawn command: {err}");
        }
    }

    pub fn run_startup_tasks(&mut self) {
        tracing::info!(
            output_count = self.space.outputs().count(),
            socket = ?self.socket_name,
            "running startup tasks"
        );
        self.start_xwayland_satellite();
        self.kick_portal_services_async();
        self.run_autostart_commands();
        self.ensure_waypaper_swww_daemon();
        self.apply_wallpaper();
        // Make startup surfaces visible promptly without requiring input events.
        crate::backend::udev::queue_redraw_all(self);
    }

    pub fn preferred_decoration_mode(&self) -> XdgDecorationMode {
        if self.config.no_csd {
            XdgDecorationMode::ServerSide
        } else {
            XdgDecorationMode::ClientSide
        }
    }

    pub fn apply_decoration_preferences(&self) {
        let mode = self.preferred_decoration_mode();
        for window in self.space.elements() {
            let Some(toplevel) = window.toplevel() else {
                continue;
            };

            toplevel.with_pending_state(|state| {
                state.decoration_mode = Some(mode);
            });

            if toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
        }
    }

    fn run_autostart_commands(&mut self) {
        if self.autostart_started {
            return;
        }
        self.autostart_started = true;

        for command in &self.config.autostart {
            tracing::info!(command, "starting autostart command");
            self.spawn_command(command);
        }
    }

    fn ensure_portal_preferences_file() {
        let config_root = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .map(|home| PathBuf::from(home).join(".config"))
            });

        let Some(config_root) = config_root else {
            tracing::warn!("unable to resolve config directory for xdg-desktop-portal preferences");
            return;
        };

        let portal_dir = config_root.join("xdg-desktop-portal");
        let portal_conf = portal_dir.join("raven-portals.conf");

        if portal_conf.exists() {
            match fs::read_to_string(&portal_conf) {
                Ok(existing) if existing.trim() == Self::legacy_portal_preferences().trim() => {
                    if let Err(err) = fs::write(&portal_conf, Self::default_portal_preferences()) {
                        tracing::warn!(
                            path = %portal_conf.display(),
                            "failed to migrate legacy portal preferences: {err}"
                        );
                    } else {
                        tracing::info!(
                            path = %portal_conf.display(),
                            "migrated legacy portal preferences"
                        );
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(
                        path = %portal_conf.display(),
                        "failed to read existing portal preferences: {err}"
                    );
                }
            }
            return;
        }

        if let Err(err) = fs::create_dir_all(&portal_dir) {
            tracing::warn!(
                path = %portal_dir.display(),
                "failed to create xdg-desktop-portal config directory: {err}"
            );
            return;
        }

        if let Err(err) = fs::write(&portal_conf, Self::default_portal_preferences()) {
            tracing::warn!(
                path = %portal_conf.display(),
                "failed to write portal preferences file: {err}"
            );
            return;
        }

        tracing::info!(path = %portal_conf.display(), "created default portal preferences");
    }

    fn default_portal_preferences() -> &'static str {
        "[preferred]\n\
default=gtk;\n\
org.freedesktop.impl.portal.Access=gtk;\n\
org.freedesktop.impl.portal.Notification=gtk;\n\
org.freedesktop.impl.portal.FileChooser=gtk;\n\
org.freedesktop.impl.portal.Settings=gtk;\n\
org.freedesktop.impl.portal.Secret=gnome-keyring;\n"
    }

    fn legacy_portal_preferences() -> &'static str {
        "[preferred]\n\
default=gnome;gtk;\n\
org.freedesktop.impl.portal.Access=gtk;\n\
org.freedesktop.impl.portal.Notification=gtk;\n\
org.freedesktop.impl.portal.Secret=gnome-keyring;\n"
    }

    fn kick_portal_services_async(&self) {
        thread::spawn(move || {
            const CONFLICTING_UNITS: [&str; 3] = [
                "xdg-desktop-portal-gnome.service",
                "xdg-desktop-portal-hyprland.service",
                "xdg-desktop-portal-kde.service",
            ];
            const UNITS: [&str; 3] = [
                "xdg-desktop-portal-gtk.service",
                "xdg-desktop-portal-wlr.service",
                "xdg-desktop-portal.service",
            ];

            for unit in CONFLICTING_UNITS {
                match Command::new("systemctl")
                    .arg("--user")
                    .arg("--no-block")
                    .arg("stop")
                    .arg(unit)
                    .output()
                {
                    Ok(output) if output.status.success() => {
                        tracing::info!(unit, "requested conflicting portal unit stop");
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                        if !stderr.contains("not found") && !stderr.contains("not loaded") {
                            tracing::warn!(
                                unit,
                                status = ?output.status.code(),
                                stderr,
                                "failed to stop conflicting portal unit"
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!("failed to execute systemctl --user stop for {unit}: {err}");
                    }
                }
            }

            for unit in UNITS {
                match Command::new("systemctl")
                    .arg("--user")
                    .arg("restart")
                    .arg(unit)
                    .output()
                {
                    Ok(output)
                        if output.status.success()
                            || output.status.code() == Some(1)
                                && String::from_utf8_lossy(&output.stderr)
                                    .contains("Job type restart is not applicable") =>
                    {
                        tracing::info!(unit, "requested portal unit restart");
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                        // Missing units are expected across different distros.
                        if stderr.contains("Job type restart is not applicable") {
                            match Command::new("systemctl")
                                .arg("--user")
                                .arg("--no-block")
                                .arg("start")
                                .arg(unit)
                                .output()
                            {
                                Ok(start_output) if start_output.status.success() => {
                                    tracing::info!(unit, "requested portal unit start");
                                }
                                Ok(start_output) => {
                                    let start_stderr =
                                        String::from_utf8_lossy(&start_output.stderr)
                                            .trim()
                                            .to_owned();
                                    if !start_stderr.contains("not found")
                                        && !start_stderr.contains("not loaded")
                                    {
                                        tracing::warn!(
                                            unit,
                                            status = ?start_output.status.code(),
                                            stderr = start_stderr,
                                            "failed to start portal unit"
                                        );
                                    }
                                }
                                Err(err) => {
                                    tracing::warn!(
                                        "failed to execute systemctl --user start for {unit}: {err}"
                                    );
                                }
                            }
                        } else if !stderr.contains("not found") && !stderr.contains("not loaded") {
                            tracing::warn!(
                                unit,
                                status = ?output.status.code(),
                                stderr,
                                "failed to restart portal unit"
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            "failed to execute systemctl --user restart for {unit}: {err}"
                        );
                    }
                }
            }
        });
    }

    fn start_xwayland_satellite(&self) {
        if !self.config.xwayland.enabled {
            return;
        }

        let path = self.config.xwayland.path.trim();
        let x11_display_text = self.config.xwayland.display.trim();
        if path.is_empty() || x11_display_text.is_empty() {
            tracing::warn!("xwayland is enabled but xwayland.path or xwayland.display is empty");
            return;
        }

        let mut cmd = Command::new(path);
        cmd.arg(x11_display_text)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        self.apply_wayland_child_env(&mut cmd);
        cmd.env_remove("RUST_BACKTRACE");
        cmd.env_remove("RUST_LIB_BACKTRACE");

        match cmd.spawn() {
            Ok(_) => {
                tracing::info!(
                    path = path,
                    x11_display = x11_display_text,
                    "started xwayland-satellite"
                );
            }
            Err(err) => {
                tracing::warn!(
                    path = path,
                    x11_display = x11_display_text,
                    "failed to start xwayland-satellite: {err}"
                );
            }
        }
    }

    fn ensure_waypaper_swww_daemon(&self) {
        let restore_command = self.config.wallpaper.restore_command.trim();
        if restore_command != "waypaper --restore" {
            return;
        }

        // Ensure an swww-daemon exists for waypaper's default namespace handling.
        self.spawn_command(
            "unset SWWW_SOCKET SWWW_DAEMON_SOCKET SWWW_NAMESPACE; swww query --namespace '' >/dev/null 2>&1 || (swww-daemon --namespace '' --quiet >/dev/null 2>&1 & sleep 0.2); swww query --namespace '' >/dev/null 2>&1 || (swww-daemon --quiet >/dev/null 2>&1 & sleep 0.2)",
        );
    }

    fn expand_home_path(raw_path: &str) -> PathBuf {
        if let Some(rest) = raw_path.strip_prefix("~/")
            && let Some(home) = std::env::var_os("HOME")
        {
            return PathBuf::from(home).join(rest);
        }

        PathBuf::from(raw_path)
    }

    fn apply_wayland_env_with_socket(
        cmd: &mut Command,
        socket_name: &OsString,
        runtime_dir: &Option<OsString>,
    ) {
        cmd.env("WAYLAND_DISPLAY", socket_name);
        if let Some(runtime_dir) = runtime_dir {
            cmd.env("XDG_RUNTIME_DIR", runtime_dir);
        }
        cmd.env("XDG_SESSION_TYPE", "wayland");
        cmd.env("XDG_CURRENT_DESKTOP", "raven");
        cmd.env("XDG_SESSION_DESKTOP", "raven");
        cmd.env_remove("DISPLAY");
        cmd.env_remove("HYPRLAND_INSTANCE_SIGNATURE");
        cmd.env_remove("HYPRLAND_CMD");
        cmd.env_remove("SWAYSOCK");
        cmd.env_remove("SWWW_SOCKET");
        cmd.env_remove("SWWW_DAEMON_SOCKET");
        cmd.env_remove("SWWW_NAMESPACE");
    }

    fn swww_is_ready(
        namespace: &str,
        socket_name: &OsString,
        runtime_dir: &Option<OsString>,
        log_failures: bool,
    ) -> bool {
        let mut cmd = Command::new("swww");
        cmd.arg("query")
            .arg("--namespace")
            .arg(namespace)
            .stdout(Stdio::null());
        Self::apply_wayland_env_with_socket(&mut cmd, socket_name, runtime_dir);
        let output = cmd.output();

        match output {
            Ok(result) if result.status.success() => true,
            Ok(result) => {
                if log_failures {
                    let stderr = String::from_utf8_lossy(&result.stderr).trim().to_owned();
                    tracing::warn!(
                        status = ?result.status.code(),
                        stderr,
                        "swww query failed"
                    );
                }
                false
            }
            Err(err) => {
                if log_failures {
                    tracing::warn!("failed to execute swww query: {err}");
                }
                false
            }
        }
    }

    fn apply_wallpaper_blocking(
        wallpaper: WallpaperConfig,
        image_path: PathBuf,
        socket_name: OsString,
        runtime_dir: Option<OsString>,
    ) {
        let namespace = Self::SWWW_NAMESPACE;

        let daemon_log_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("log")
            .join("swww-daemon.log");

        for daemon_start_attempt in 1..=4 {
            if Self::swww_is_ready(namespace, &socket_name, &runtime_dir, false) {
                break;
            }

            let mut daemon_cmd = Command::new("swww-daemon");
            daemon_cmd.arg("--namespace").arg(namespace).arg("--quiet");
            daemon_cmd.env("RUST_BACKTRACE", "1");
            if std::env::var("RAVEN_SWWW_WAYLAND_DEBUG").ok().as_deref() == Some("1") {
                daemon_cmd.env("WAYLAND_DEBUG", "1");
            }
            Self::apply_wayland_env_with_socket(&mut daemon_cmd, &socket_name, &runtime_dir);

            match OpenOptions::new()
                .create(true)
                .append(true)
                .open(&daemon_log_path)
            {
                Ok(log_file) => match log_file.try_clone() {
                    Ok(log_file_clone) => {
                        daemon_cmd.stdout(Stdio::from(log_file));
                        daemon_cmd.stderr(Stdio::from(log_file_clone));
                    }
                    Err(err) => {
                        tracing::warn!(
                            path = %daemon_log_path.display(),
                            "failed to clone swww-daemon log file handle: {err}"
                        );
                    }
                },
                Err(err) => {
                    tracing::warn!(
                        path = %daemon_log_path.display(),
                        "failed to open swww-daemon log file: {err}"
                    );
                }
            }

            match daemon_cmd.spawn() {
                Ok(mut child) => {
                    tracing::info!(
                        namespace,
                        daemon_start_attempt,
                        socket = ?socket_name,
                        runtime_dir = ?runtime_dir,
                        path = %daemon_log_path.display(),
                        "started swww-daemon"
                    );

                    thread::sleep(Duration::from_millis(120));
                    if let Ok(Some(status)) = child.try_wait() {
                        tracing::warn!(
                            namespace,
                            daemon_start_attempt,
                            path = %daemon_log_path.display(),
                            status = ?status.code(),
                            "swww-daemon exited early"
                        );
                    }
                }
                Err(err) => tracing::warn!("failed to start swww-daemon: {err}"),
            }

            for _ in 0..20 {
                if Self::swww_is_ready(namespace, &socket_name, &runtime_dir, false) {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }

        if !Self::swww_is_ready(namespace, &socket_name, &runtime_dir, true) {
            tracing::warn!(
                namespace,
                "swww-daemon did not become ready; skipping wallpaper"
            );
            return;
        }

        for attempt in 1..=8 {
            let mut cmd = Command::new("swww");
            cmd.arg("img")
                .arg("--namespace")
                .arg(namespace)
                .arg(&image_path)
                .arg("--resize")
                .arg(&wallpaper.resize)
                .arg("--transition-type")
                .arg(&wallpaper.transition_type)
                .arg("--transition-duration")
                .arg(wallpaper.transition_duration.to_string());
            Self::apply_wayland_env_with_socket(&mut cmd, &socket_name, &runtime_dir);
            let output = cmd.output();

            match output {
                Ok(result) if result.status.success() => {
                    tracing::info!(path = %image_path.display(), "applied wallpaper with swww");
                    return;
                }
                Ok(result) => {
                    let stderr = String::from_utf8_lossy(&result.stderr).trim().to_owned();
                    if attempt == 8 {
                        tracing::warn!(
                            status = ?result.status.code(),
                            stderr,
                            "swww img failed"
                        );
                        return;
                    }
                }
                Err(err) => {
                    if attempt == 8 {
                        tracing::warn!("failed to execute swww img: {err}");
                        return;
                    }
                }
            }

            thread::sleep(Duration::from_millis(125));
        }
    }

    pub fn apply_wallpaper(&self) {
        let wallpaper = self.config.wallpaper.clone();
        if !wallpaper.enabled {
            return;
        }

        let restore_command = wallpaper.restore_command.trim();
        if !restore_command.is_empty() {
            let command = if restore_command == "waypaper --restore" {
                // Common path: sanitize SWWW_* env and ensure a daemon for empty/default namespace
                // before waypaper restore.
                "unset SWWW_SOCKET SWWW_DAEMON_SOCKET SWWW_NAMESPACE; swww query --namespace '' >/dev/null 2>&1 || (swww-daemon --namespace '' --quiet >/dev/null 2>&1 & sleep 0.2); swww query --namespace '' >/dev/null 2>&1 || (swww-daemon --quiet >/dev/null 2>&1 & sleep 0.2); waypaper --restore"
            } else {
                restore_command
            };
            tracing::info!(command, "restoring wallpaper with external command");
            self.spawn_command(command);
            return;
        }

        let image = wallpaper.image.trim();
        if image.is_empty() {
            tracing::warn!("wallpaper is enabled but no image path is configured");
            return;
        }

        let image_path = Self::expand_home_path(image);
        if !image_path.exists() {
            tracing::warn!(path = %image_path.display(), "wallpaper image not found");
            return;
        }
        tracing::info!(path = %image_path.display(), "applying wallpaper");

        if self.wallpaper_task_inflight.swap(true, Ordering::AcqRel) {
            tracing::info!("wallpaper apply already in progress; skipping duplicate request");
            return;
        }

        let inflight = Arc::clone(&self.wallpaper_task_inflight);
        let socket_name = self.socket_name.clone();
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
        thread::spawn(move || {
            struct ResetInFlight(Arc<AtomicBool>);
            impl Drop for ResetInFlight {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }

            let _reset = ResetInFlight(inflight);
            Self::apply_wallpaper_blocking(wallpaper, image_path, socket_name, runtime_dir);
        });
    }

    pub fn reload_config(&mut self) -> Result<(), CompositorError> {
        let config = config::load_from_path(&self.config_path)?;
        config::apply_environment(&config);
        self.config = config;
        self.sync_activation_environment();
        self.apply_decoration_preferences();

        if self.udev_data.is_some() {
            crate::backend::udev::reload_cursor_theme(self);
        }

        self.apply_layout()?;
        self.apply_wallpaper();
        tracing::info!(path = %self.config_path.display(), "reloaded config.lua");
        Ok(())
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

        if self.fullscreen_windows.contains(&window) {
            self.fullscreen_windows
                .retain(|candidate| candidate != &window);
            self.clear_fullscreen_ready_for_window(&window);
            self.set_window_fullscreen_state(&window, false);
            return self.apply_layout();
        }

        let previous_fullscreen_windows = self.fullscreen_windows.clone();
        for fullscreen_window in &previous_fullscreen_windows {
            self.clear_fullscreen_ready_for_window(fullscreen_window);
            self.set_window_fullscreen_state(fullscreen_window, false);
        }

        self.fullscreen_windows.clear();
        self.fullscreen_windows.push(window.clone());
        self.clear_fullscreen_ready_for_window(&window);
        self.set_window_fullscreen_state(&window, true);
        self.queue_fullscreen_transition_redraw_for_window(&window);
        self.space.raise_element(&window, true);

        self.apply_layout()
    }

    pub fn toggle_floating_focused_window(&mut self) -> Result<(), CompositorError> {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return Ok(());
        };
        let Some(focused_surface) = keyboard.current_focus() else {
            return Ok(());
        };
        let Some(window) = self.window_for_surface(&focused_surface) else {
            return Ok(());
        };

        let currently_floating = self.is_window_floating(&window);
        self.set_window_floating(&window, !currently_floating);
        if !currently_floating && self.is_window_mapped(&window) {
            let loc = self.initial_map_location_for_window(&window);
            self.space.map_element(window.clone(), loc, true);
        }

        self.apply_layout()
    }

    pub(crate) fn set_window_fullscreen_state(&self, window: &Window, fullscreen: bool) {
        let Some(toplevel) = window.toplevel() else {
            return;
        };

        let fullscreen_size = if fullscreen {
            self.space
                .outputs()
                .next()
                .and_then(|output| self.space.output_geometry(output))
                .map(|geometry| geometry.size)
        } else {
            None
        };
        let output_bounds = self
            .space
            .outputs()
            .next()
            .and_then(|output| self.space.output_geometry(output))
            .map(|geometry| geometry.size);

        toplevel.with_pending_state(|state| {
            if fullscreen {
                state.states.set(xdg_toplevel::State::Fullscreen);
                state.states.unset(xdg_toplevel::State::Maximized);
                state.states.unset(xdg_toplevel::State::TiledLeft);
                state.states.unset(xdg_toplevel::State::TiledRight);
                state.states.unset(xdg_toplevel::State::TiledTop);
                state.states.unset(xdg_toplevel::State::TiledBottom);
                if let Some(size) = fullscreen_size {
                    state.size = Some(size);
                }
                state.bounds = fullscreen_size.or(output_bounds);
            } else {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                state.states.unset(xdg_toplevel::State::TiledLeft);
                state.states.unset(xdg_toplevel::State::TiledRight);
                state.states.unset(xdg_toplevel::State::TiledTop);
                state.states.unset(xdg_toplevel::State::TiledBottom);
                state.size = None;
                state.bounds = output_bounds;
            }
        });
        toplevel.send_pending_configure();
    }

    pub fn refresh_foreign_toplevel(&mut self) {
        crate::protocols::foreign_toplevel::refresh(self);
    }

    pub fn refresh_ext_workspace(&mut self) {
        crate::protocols::ext_workspace::refresh(self);
    }
}

pub fn init_wayland_listener(
    display: Display<Raven>,
    loop_handle: &LoopHandle<'static, Raven>,
) -> OsString {
    let listening_socket = ListeningSocketSource::new_auto().expect("failed to create socket");
    let socket_name = listening_socket.socket_name().to_os_string();

    loop_handle
        .insert_source(listening_socket, move |client_stream, _, state| {
            let client_state = ClientState {
                can_view_decoration_globals: state.config.no_csd,
                ..ClientState::default()
            };
            state
                .display_handle
                .insert_client(client_stream, Arc::new(client_state))
                .expect("failed to insert client");
        })
        .expect("failed to init wayland listener");

    loop_handle
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            move |_, display, state| {
                // Safety: we don't drop the display
                unsafe {
                    display.get_mut().dispatch_clients(state).unwrap();
                }
                state.space.refresh();
                if let Err(err) = state.display_handle.flush_clients() {
                    tracing::warn!("failed to flush clients after dispatch: {err}");
                }
                Ok(PostAction::Continue)
            },
        )
        .expect("failed to init display event source");

    socket_name
}

fn init_ipc_listener(
    loop_handle: &LoopHandle<'static, Raven>,
    socket_name: &OsString,
) -> Result<PathBuf, CompositorError> {
    let ipc_socket_path = ipc_socket_path_for_wayland_socket(socket_name)?;

    if ipc_socket_path.exists()
        && let Err(err) = std::fs::remove_file(&ipc_socket_path)
    {
        return Err(CompositorError::Backend(format!(
            "failed to remove stale ipc socket {}: {err}",
            ipc_socket_path.display()
        )));
    }

    let listener = UnixListener::bind(&ipc_socket_path).map_err(|err| {
        CompositorError::Backend(format!(
            "failed to bind ipc socket {}: {err}",
            ipc_socket_path.display()
        ))
    })?;
    listener.set_nonblocking(true).map_err(|err| {
        CompositorError::Backend(format!(
            "failed to set ipc socket nonblocking {}: {err}",
            ipc_socket_path.display()
        ))
    })?;

    loop_handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, state| {
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => state.handle_ipc_stream(stream),
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(err) => {
                            tracing::warn!("ipc accept failed: {err}");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|err| CompositorError::EventLoop(format!("failed to init ipc listener: {err}")))?;

    Ok(ipc_socket_path)
}

fn ipc_socket_path_for_wayland_socket(socket_name: &OsString) -> Result<PathBuf, CompositorError> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
        CompositorError::Backend("XDG_RUNTIME_DIR is not set; cannot create ipc socket".to_owned())
    })?;
    let wayland_socket = socket_name.to_string_lossy().trim().to_owned();
    if wayland_socket.is_empty() {
        return Err(CompositorError::Backend(
            "wayland socket name is empty; cannot create ipc socket".to_owned(),
        ));
    }
    Ok(PathBuf::from(runtime_dir).join(format!("raven-{wayland_socket}.sock")))
}

pub struct ClientState {
    pub compositor_state: CompositorClientState,
    pub can_view_decoration_globals: bool,
}

impl Default for ClientState {
    fn default() -> Self {
        Self {
            compositor_state: CompositorClientState::default(),
            can_view_decoration_globals: true,
        }
    }
}

impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        tracing::info!(?client_id, "wayland client initialized");
    }

    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        tracing::info!(?client_id, ?reason, "wayland client disconnected");
    }
}
