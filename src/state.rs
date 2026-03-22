use smithay::{
    desktop::{PopupManager, Space, Window, layer_map_for_output},
    input::{Seat, SeatState, pointer::CursorImageStatus},
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
    utils::{Clock, Logical, Monotonic, Point, Rectangle, Serial, Size},
    wayland::{
        compositor::{CompositorClientState, CompositorState, with_states},
        dmabuf::DmabufState,
        drm_syncobj::DrmSyncobjState,
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        pointer_gestures::PointerGesturesState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        shell::{
            kde::decoration::KdeDecorationState,
            wlr_layer::WlrLayerShellState,
            xdg::{
                SurfaceCachedState, XdgShellState, XdgToplevelSurfaceData,
                decoration::XdgDecorationState,
            },
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
    io::Write,
    os::fd::AsRawFd,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    CompositorError,
    config::{self, RuntimeConfig, WallpaperConfig},
    layout::{GapConfig, LayoutBox, LayoutType},
    protocols::{
        ext_workspace::ExtWorkspaceManagerState,
        foreign_toplevel::ForeignToplevelManagerState,
        wlr_screencopy::{Screencopy, ScreencopyManagerState},
    },
};

mod fullscreen;
mod ipc;
mod rules;
mod runtime;
mod workspaces;

use fullscreen::{FullscreenState, WindowFullscreenMode};

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

#[derive(Default, Clone, PartialEq)]
pub struct PointContents {
    pub output: Option<smithay::output::Output>,
    pub surface: Option<(WlSurface, Point<f64, Logical>)>,
    pub window: Option<Window>,
    pub layer: Option<WlSurface>,
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
    pub pointer_constraints_state: PointerConstraintsState,
    pub pointer_gestures_state: PointerGesturesState,
    pub relative_pointer_state: RelativePointerManagerState,

    pub pointer_location: Point<f64, Logical>,
    pub pointer_contents: PointContents,
    pub last_pointer_redraw_msec: Option<u32>,
    pub pending_screencopy: Option<Screencopy>,
    pending_interactive_moves: Vec<PendingInteractiveMove>,
    pending_interactive_resizes: Vec<PendingInteractiveResize>,
    pub current_workspace: usize,
    // Mapped windows that currently participate in workspace rendering/layout.
    pub workspaces: Vec<Vec<Window>>,
    // Unmapped toplevels tracked per-workspace until their first real map commit.
    unmapped_workspaces: Vec<Vec<Window>>,
    // Fullscreen ownership/transition bookkeeping.
    fullscreen: FullscreenState,
    assigned_rects_by_surface: HashMap<WlSurface, Rectangle<i32, Logical>>,
    reported_sizes_by_surface: HashMap<WlSurface, Size<i32, Logical>>,
    // Track scanout rejection reasons per output to aid debugging/perf tuning.
    scanout_reject_counters: HashMap<String, u64>,
    pub floating_windows: Vec<Window>,
    // Per-surface lifecycle sets used during the unmapped -> mapped transition.
    // `pending_initial_configure_ids`: first configure still needs to be sent.
    // `pending_initial_configure_idle_ids`: idle callback already queued for that send.
    // `unmapped_toplevel_ids`: tracked in workspace state but must not be mapped yet.
    // `pending_unmapped_*_ids`: state requests received before the first map commit.
    pub pending_floating_recenter_ids: HashSet<WlSurface>,
    pub pending_window_rule_recheck_ids: HashSet<WlSurface>,
    pub pending_initial_configure_ids: HashSet<WlSurface>,
    pending_initial_configure_idle_ids: HashSet<WlSurface>,
    pub unmapped_toplevel_ids: HashSet<WlSurface>,
    pending_unmapped_maximized_ids: HashSet<WlSurface>,
    pub autostart_started: bool,
    pub wallpaper_task_inflight: Arc<AtomicBool>,
    xwayland_satellite: Option<Child>,
    xwayland_satellite_signature: Option<String>,
    xwayland_satellite_started_at: Option<Instant>,
    xwayland_satellite_backoff_until: Option<Instant>,
    xwayland_satellite_failure_count: u8,

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
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&display_handle);
        let pointer_gestures_state = PointerGesturesState::new::<Self>(&display_handle);
        let relative_pointer_state = RelativePointerManagerState::new::<Self>(&display_handle);
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

        let mut state = Self {
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
            pointer_constraints_state,
            pointer_gestures_state,
            relative_pointer_state,

            pointer_location: Point::from((0.0, 0.0)),
            pointer_contents: PointContents::default(),
            last_pointer_redraw_msec: None,
            pending_screencopy: None,
            pending_interactive_moves: Vec::new(),
            pending_interactive_resizes: Vec::new(),
            current_workspace: 0,
            workspaces: vec![Vec::new(); WORKSPACE_COUNT],
            unmapped_workspaces: vec![Vec::new(); WORKSPACE_COUNT],
            fullscreen: FullscreenState::new(),
            assigned_rects_by_surface: HashMap::new(),
            reported_sizes_by_surface: HashMap::new(),
            scanout_reject_counters: HashMap::new(),
            floating_windows: Vec::new(),
            pending_floating_recenter_ids: HashSet::new(),
            pending_window_rule_recheck_ids: HashSet::new(),
            pending_initial_configure_ids: HashSet::new(),
            pending_initial_configure_idle_ids: HashSet::new(),
            unmapped_toplevel_ids: HashSet::new(),
            pending_unmapped_maximized_ids: HashSet::new(),
            autostart_started: false,
            wallpaper_task_inflight: Arc::new(AtomicBool::new(false)),
            xwayland_satellite: None,
            xwayland_satellite_signature: None,
            xwayland_satellite_started_at: None,
            xwayland_satellite_backoff_until: None,
            xwayland_satellite_failure_count: 0,

            cursor_status: CursorImageStatus::default_named(),
            clock: Clock::new(),
            dmabuf_state: None,
            syncobj_state: None,
            udev_data: None,
        };

        Self::ensure_portal_preferences_file();
        state.ensure_xwayland_display();
        state.sync_activation_environment();

        Ok(state)
    }

    pub fn apply_layout(&mut self) -> Result<(), CompositorError> {
        self.prune_windows_without_live_client();

        // Ensure visible windows tracked on the current workspace are mapped into Space before
        // layout decisions. Without this, a fullscreen owner unmap/destroy can leave sibling
        // windows (e.g. Steam host) hidden until a workspace switch remaps them.
        let current_workspace_windows = self.workspaces[self.current_workspace].clone();
        for window in &current_workspace_windows {
            if self.space.element_location(window).is_some() {
                continue;
            }
            if self.window_is_unmapped_toplevel(window) {
                continue;
            }
            if !Self::window_root_surface_has_buffer(window) {
                continue;
            }
            self.map_window_to_initial_location(window, false);
        }

        let mut windows: Vec<smithay::desktop::Window> = self
            .space
            .elements()
            .filter(|window| Self::window_root_surface_has_buffer(window))
            .cloned()
            .collect();
        if windows.is_empty() {
            return Ok(());
        }

        // Guard against duplicate mapped entries for the same wl_surface.
        // Duplicate entries can create a fake second tile ("ghost right window").
        let mut seen_surface_ids: HashSet<WlSurface> = HashSet::new();
        for window in &windows {
            let Some(surface_id) = Self::window_surface_id(window) else {
                continue;
            };
            if !seen_surface_ids.insert(surface_id) {
                self.unmap_window(window);
            }
        }

        windows = self
            .space
            .elements()
            .filter(|window| Self::window_root_surface_has_buffer(window))
            .cloned()
            .collect();
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

        if self.apply_fullscreen_layout_if_needed(&windows, &output, out_geo)? {
            return Ok(());
        }

        let floating_windows: Vec<smithay::desktop::Window> = windows
            .iter()
            .filter(|window| self.is_window_floating(window))
            .cloned()
            .collect();

        for window in &floating_windows {
            if self.window_is_unmapped_toplevel(window) {
                continue;
            }
            let target_rect = self
                .space
                .element_geometry(window)
                .unwrap_or_else(|| self.initial_map_rect_for_window(window));
            self.record_assigned_rect_for_window(window, target_rect);
            if self.space.element_location(window).is_none() {
                self.map_window_to_rect(window, target_rect, false);
            }
        }

        let tiled_windows: Vec<smithay::desktop::Window> = windows
            .iter()
            .filter(|window| !self.is_window_floating(window))
            .cloned()
            .collect();
        if tiled_windows.is_empty() {
            self.restack_floating_windows_above_tiled();
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
            let target_geometry = Rectangle::new(loc, desired_size);
            self.record_assigned_rect_for_window(&window, target_geometry);

            if !is_mapped || needs_resize {
                self.configure_window_for_tiled_layout(&window, target_geometry, layout_geo.size);
            }

            if !is_mapped || needs_resize || needs_reposition {
                self.map_window_to_rect(&window, target_geometry, false);
            }
        }

        self.restack_floating_windows_above_tiled();

        Ok(())
    }

    fn restack_floating_windows_above_tiled(&mut self) {
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        if windows.len() < 2 {
            return;
        }

        // Keep exclusive-mode stacking untouched while the current workspace has an owner.
        if self.workspace_effective_exclusive_mode(self.current_workspace)
            != WindowFullscreenMode::NONE
        {
            return;
        }

        let mut tiled_windows = Vec::new();
        let mut floating_windows = Vec::new();
        for window in windows {
            if self.is_window_floating(&window) {
                floating_windows.push(window);
            } else {
                tiled_windows.push(window);
            }
        }

        if tiled_windows.is_empty() || floating_windows.is_empty() {
            return;
        }

        // Preserve relative order inside each group, but ensure floating stays above tiled.
        for window in &tiled_windows {
            self.space.raise_element(window, true);
        }
        for window in &floating_windows {
            self.space.raise_element(window, true);
        }
    }

    pub(crate) fn raise_window_preserving_layer(&mut self, window: &Window) {
        self.space.raise_element(window, true);
        self.restack_floating_windows_above_tiled();
    }

    pub(crate) fn workspace_windows(&self) -> impl Iterator<Item = &Window> {
        self.workspaces
            .iter()
            .chain(self.unmapped_workspaces.iter())
            .flatten()
    }

    #[cfg(debug_assertions)]
    pub(crate) fn debug_assert_state_invariants(&self, context: &str) {
        let mut mapped_surfaces: HashSet<WlSurface> = HashSet::new();
        let mut unmapped_surfaces: HashSet<WlSurface> = HashSet::new();

        for workspace in &self.workspaces {
            for window in workspace {
                let Some(surface) = Self::window_surface_id(window) else {
                    continue;
                };
                debug_assert!(
                    mapped_surfaces.insert(surface.clone()),
                    "state invariant failed ({context}): duplicate mapped workspace entry surface={:?}",
                    surface.id()
                );
            }
        }

        for workspace in &self.unmapped_workspaces {
            for window in workspace {
                let Some(surface) = Self::window_surface_id(window) else {
                    continue;
                };
                debug_assert!(
                    unmapped_surfaces.insert(surface.clone()),
                    "state invariant failed ({context}): duplicate unmapped workspace entry surface={:?}",
                    surface.id()
                );
            }
        }

        for surface in &mapped_surfaces {
            debug_assert!(
                !unmapped_surfaces.contains(surface),
                "state invariant failed ({context}): surface present in both mapped and unmapped stores surface={:?}",
                surface.id()
            );
            debug_assert!(
                !self.unmapped_toplevel_ids.contains(surface),
                "state invariant failed ({context}): mapped surface still marked unmapped surface={:?}",
                surface.id()
            );
        }

        for window in self.space.elements() {
            let Some(surface) = Self::window_surface_id(window) else {
                continue;
            };
            debug_assert!(
                mapped_surfaces.contains(&surface),
                "state invariant failed ({context}): mapped space element missing mapped workspace entry surface={:?}",
                surface.id()
            );
            debug_assert!(
                !unmapped_surfaces.contains(&surface),
                "state invariant failed ({context}): mapped space element still tracked unmapped surface={:?}",
                surface.id()
            );
            debug_assert!(
                !self.unmapped_toplevel_ids.contains(&surface),
                "state invariant failed ({context}): mapped space element has unmapped marker surface={:?}",
                surface.id()
            );
        }

        for surface in &self.unmapped_toplevel_ids {
            if !surface.is_alive() {
                continue;
            }
            debug_assert!(
                unmapped_surfaces.contains(surface),
                "state invariant failed ({context}): unmapped marker without unmapped workspace entry surface={:?}",
                surface.id()
            );
        }

        let mut owner_surfaces: HashSet<WlSurface> = HashSet::new();
        for (workspace_index, owner_slot) in self
            .fullscreen
            .owner_surfaces_by_workspace
            .iter()
            .enumerate()
        {
            let Some(owner_slot) = owner_slot else {
                continue;
            };
            let owner_surface = &owner_slot.surface;
            debug_assert!(
                owner_surfaces.insert(owner_surface.clone()),
                "state invariant failed ({context}): fullscreen owner surface assigned to multiple workspaces surface={:?}",
                owner_surface.id()
            );
            debug_assert!(
                mapped_surfaces.contains(owner_surface)
                    || unmapped_surfaces.contains(owner_surface),
                "state invariant failed ({context}): fullscreen owner missing workspace tracking workspace={} surface={:?}",
                workspace_index,
                owner_surface.id()
            );
            let owner_in_declared_workspace =
                self.workspaces
                    .get(workspace_index)
                    .is_some_and(|workspace| {
                        workspace
                            .iter()
                            .any(|window| Self::window_matches_surface(window, owner_surface))
                    })
                    || self
                        .unmapped_workspaces
                        .get(workspace_index)
                        .is_some_and(|workspace| {
                            workspace
                                .iter()
                                .any(|window| Self::window_matches_surface(window, owner_surface))
                        });
            debug_assert!(
                owner_in_declared_workspace,
                "state invariant failed ({context}): fullscreen owner points to wrong workspace workspace={} surface={:?}",
                workspace_index,
                owner_surface.id()
            );
        }
    }

    #[cfg(not(debug_assertions))]
    #[inline]
    pub(crate) fn debug_assert_state_invariants(&self, _context: &str) {}

    pub(crate) fn queue_redraw_for_outputs_or_all<I>(&mut self, outputs: I)
    where
        I: IntoIterator<Item = smithay::output::Output>,
    {
        let mut had_output = false;
        for output in outputs {
            had_output = true;
            crate::backend::udev::queue_redraw_for_output(self, &output);
        }
        if !had_output {
            crate::backend::udev::queue_redraw_all(self);
        }
    }

    pub fn record_scanout_rejection(
        &mut self,
        output: &smithay::output::Output,
        reason: &'static str,
    ) {
        let key = format!("{}:{reason}", output.name());
        let count = self
            .scanout_reject_counters
            .entry(key)
            .and_modify(|count| *count += 1)
            .or_insert(1);

        // Scanout rejections are expected during transitions and overlays; keep these at
        // debug level and heavily rate-limited to avoid logging-induced event-loop pressure.
        if *count <= 3 || *count % 3000 == 0 {
            tracing::debug!(output = %output.name(), reason, count = *count, "scanout rejected");
        }
    }

    pub fn add_window_to_current_workspace(&mut self, window: Window) {
        self.add_window_to_workspace(self.current_workspace, window);
    }

    fn windows_match(lhs: &Window, rhs: &Window) -> bool {
        match (lhs.toplevel(), rhs.toplevel()) {
            (Some(lhs_toplevel), Some(rhs_toplevel)) => {
                lhs_toplevel.wl_surface() == rhs_toplevel.wl_surface()
            }
            _ => lhs == rhs,
        }
    }

    fn workspace_contains_window_entry(workspace: &[Window], window: &Window) -> bool {
        workspace
            .iter()
            .any(|candidate| Self::windows_match(candidate, window))
    }

    fn add_window_to_workspace_list(
        workspaces: &mut [Vec<Window>],
        workspace_index: usize,
        window: Window,
    ) -> Result<(), CompositorError> {
        let Some(workspace) = workspaces.get_mut(workspace_index) else {
            return Err(CompositorError::Backend(format!(
                "invalid workspace index {workspace_index}"
            )));
        };
        if !Self::workspace_contains_window_entry(workspace, &window) {
            workspace.push(window);
        }
        Ok(())
    }

    fn remove_window_from_workspace_list(
        workspaces: &mut [Vec<Window>],
        window: &Window,
    ) -> Option<usize> {
        let mut first_removed: Option<usize> = None;
        for (workspace_index, workspace) in workspaces.iter_mut().enumerate() {
            let before = workspace.len();
            workspace.retain(|candidate| !Self::windows_match(candidate, window));
            if workspace.len() != before && first_removed.is_none() {
                first_removed = Some(workspace_index);
            }
        }
        first_removed
    }

    fn workspace_index_in_list(workspaces: &[Vec<Window>], window: &Window) -> Option<usize> {
        workspaces
            .iter()
            .position(|workspace| Self::workspace_contains_window_entry(workspace, window))
    }

    fn workspace_index_for_mapped_window(&self, window: &Window) -> Option<usize> {
        Self::workspace_index_in_list(&self.workspaces, window)
    }

    fn workspace_index_for_unmapped_window(&self, window: &Window) -> Option<usize> {
        Self::workspace_index_in_list(&self.unmapped_workspaces, window)
    }

    fn window_matches_surface(window: &Window, surface: &WlSurface) -> bool {
        Self::window_surface_id(window)
            .as_ref()
            .is_some_and(|candidate| candidate == surface)
    }

    pub(crate) fn promote_window_to_mapped_workspace(&mut self, window: &Window) {
        let workspace_index =
            Self::remove_window_from_workspace_list(&mut self.unmapped_workspaces, window)
                .or_else(|| self.workspace_index_for_mapped_window(window))
                .unwrap_or(self.current_workspace);
        if let Err(err) = Self::add_window_to_workspace_list(
            &mut self.workspaces,
            workspace_index,
            window.clone(),
        ) {
            tracing::warn!("failed to promote window to mapped workspace: {err}");
        }
        self.debug_assert_state_invariants("promote_window_to_mapped_workspace");
    }

    pub(crate) fn demote_window_to_unmapped_workspace(&mut self, window: &Window) {
        let workspace_index = Self::remove_window_from_workspace_list(&mut self.workspaces, window)
            .or_else(|| self.workspace_index_for_unmapped_window(window))
            .unwrap_or(self.current_workspace);
        if let Err(err) = Self::add_window_to_workspace_list(
            &mut self.unmapped_workspaces,
            workspace_index,
            window.clone(),
        ) {
            tracing::warn!("failed to demote window to unmapped workspace: {err}");
        }
        self.debug_assert_state_invariants("demote_window_to_unmapped_workspace");
    }

    fn window_has_live_client(window: &Window) -> bool {
        window.toplevel().is_some_and(|toplevel| {
            toplevel.alive()
                && toplevel.wl_surface().is_alive()
                && toplevel.wl_surface().client().is_some()
        })
    }

    fn prune_windows_without_live_client(&mut self) {
        let mut dead_windows: Vec<Window> = Vec::new();
        let mut seen_surface_ids: HashSet<WlSurface> = HashSet::new();

        for window in self.workspace_windows().chain(self.space.elements()) {
            if Self::window_has_live_client(window) {
                continue;
            }

            if let Some(surface_id) = Self::window_surface_id(window) {
                if !seen_surface_ids.insert(surface_id) {
                    continue;
                }
            } else if dead_windows
                .iter()
                .any(|candidate| Self::windows_match(candidate, window))
            {
                continue;
            }

            dead_windows.push(window.clone());
        }

        for window in &dead_windows {
            self.unmap_window(window);
            self.remove_window_from_workspaces(window);
            if let Some(toplevel) = window.toplevel() {
                let surface = toplevel.wl_surface().clone();
                self.pending_window_rule_recheck_ids.remove(&surface);
                self.pending_floating_recenter_ids.remove(&surface);
                self.clear_pending_unmapped_state_for_surface(&surface);
            }
        }
    }

    pub(crate) fn workspace_contains_window(
        &self,
        workspace_index: usize,
        window: &Window,
    ) -> bool {
        self.workspaces
            .get(workspace_index)
            .is_some_and(|workspace| Self::workspace_contains_window_entry(workspace, window))
            || self
                .unmapped_workspaces
                .get(workspace_index)
                .is_some_and(|workspace| Self::workspace_contains_window_entry(workspace, window))
    }

    pub fn add_window_to_workspace(&mut self, workspace_index: usize, window: Window) {
        if let Err(err) =
            Self::add_window_to_workspace_list(&mut self.workspaces, workspace_index, window)
        {
            tracing::warn!("attempted to add window to mapped workspace: {err}");
        }
        self.debug_assert_state_invariants("add_window_to_workspace");
    }

    pub fn add_unmapped_window_to_workspace(&mut self, workspace_index: usize, window: Window) {
        if let Err(err) = Self::add_window_to_workspace_list(
            &mut self.unmapped_workspaces,
            workspace_index,
            window,
        ) {
            tracing::warn!("attempted to add window to unmapped workspace: {err}");
        }
        self.debug_assert_state_invariants("add_unmapped_window_to_workspace");
    }

    pub fn set_window_floating(&mut self, window: &Window, floating: bool) {
        if floating {
            if !self
                .floating_windows
                .iter()
                .any(|candidate| Self::windows_match(candidate, window))
            {
                self.floating_windows.push(window.clone());
            }
        } else {
            self.floating_windows
                .retain(|candidate| !Self::windows_match(candidate, window));
        }
    }

    pub fn is_window_floating(&self, window: &Window) -> bool {
        self.floating_windows
            .iter()
            .any(|candidate| Self::windows_match(candidate, window))
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
                // For fixed-size popups (Steam splash/sign-in, dialogs), use size hints for
                // placement because geometry can still be a temporary tiled size during startup.
                let hint_size = window
                    .toplevel()
                    .and_then(|toplevel| Self::fixed_hint_size_for_surface(toplevel.wl_surface()));
                let hinted_or_current_w = hint_size.map(|size| size.w).unwrap_or(window_geo.size.w);
                let hinted_or_current_h = hint_size.map(|size| size.h).unwrap_or(window_geo.size.h);
                let window_width = hinted_or_current_w.clamp(1, geometry.size.w);
                let window_height = hinted_or_current_h.clamp(1, geometry.size.h);
                let x = geometry.loc.x + (geometry.size.w - window_width) / 2;
                let y = geometry.loc.y + (geometry.size.h - window_height) / 2;
                (x, y)
            })
            .unwrap_or((80, 80))
    }

    pub(crate) fn assigned_rect_for_surface(
        &self,
        surface: &WlSurface,
    ) -> Option<Rectangle<i32, Logical>> {
        self.assigned_rects_by_surface.get(surface).copied()
    }

    pub(crate) fn assigned_rect_for_window(
        &self,
        window: &Window,
    ) -> Option<Rectangle<i32, Logical>> {
        Self::window_surface_id(window).and_then(|surface| self.assigned_rect_for_surface(&surface))
    }

    pub(crate) fn record_assigned_rect_for_window(
        &mut self,
        window: &Window,
        rect: Rectangle<i32, Logical>,
    ) {
        let Some(surface) = Self::window_surface_id(window) else {
            return;
        };
        self.assigned_rects_by_surface.insert(surface, rect);
    }

    pub(crate) fn clear_assigned_rect_for_surface(&mut self, surface: &WlSurface) {
        self.assigned_rects_by_surface.remove(surface);
    }

    pub(crate) fn reported_size_for_surface(
        &self,
        surface: &WlSurface,
    ) -> Option<Size<i32, Logical>> {
        self.reported_sizes_by_surface.get(surface).copied()
    }

    pub(crate) fn committed_reported_size_for_window(
        &self,
        window: &Window,
    ) -> Option<Size<i32, Logical>> {
        let toplevel = window.toplevel()?;
        toplevel.with_committed_state(|state| state.and_then(|state| state.size))
    }

    pub(crate) fn reported_size_for_window(&self, window: &Window) -> Option<Size<i32, Logical>> {
        self.committed_reported_size_for_window(window).or_else(|| {
            Self::window_surface_id(window)
                .and_then(|surface| self.reported_size_for_surface(&surface))
        })
    }

    pub(crate) fn record_reported_size_for_window(
        &mut self,
        window: &Window,
        size: Size<i32, Logical>,
    ) {
        let Some(surface) = Self::window_surface_id(window) else {
            return;
        };
        self.reported_sizes_by_surface.insert(surface, size);
    }

    pub(crate) fn clear_reported_size_for_surface(&mut self, surface: &WlSurface) {
        self.reported_sizes_by_surface.remove(surface);
    }

    pub(crate) fn sync_reported_size_from_pending_state(
        &mut self,
        window: &Window,
        fallback_size: Option<Size<i32, Logical>>,
    ) -> Option<Size<i32, Logical>> {
        let Some(toplevel) = window.toplevel() else {
            return None;
        };
        let requested_size = toplevel
            .with_pending_state(|state| state.size)
            .or(fallback_size);
        if let Some(size) = requested_size {
            self.record_reported_size_for_window(window, size);
        }
        self.reported_size_for_window(window).or(requested_size)
    }

    fn fallback_window_size(&self, window: &Window) -> Size<i32, Logical> {
        self.assigned_rect_for_window(window)
            .map(|rect| rect.size)
            .or_else(|| self.space.element_geometry(window).map(|rect| rect.size))
            .unwrap_or_else(|| window.geometry().size)
    }

    pub(crate) fn map_window_to_rect(
        &mut self,
        window: &Window,
        rect: Rectangle<i32, Logical>,
        activate: bool,
    ) {
        self.record_assigned_rect_for_window(window, rect);
        self.space.map_element(window.clone(), rect.loc, activate);
    }

    pub(crate) fn map_window_to_location(
        &mut self,
        window: &Window,
        loc: Point<i32, Logical>,
        activate: bool,
    ) {
        self.map_window_to_rect(
            window,
            Rectangle::new(loc, self.fallback_window_size(window)),
            activate,
        );
    }

    pub(crate) fn unmap_window(&mut self, window: &Window) {
        self.space.unmap_elem(window);
        if let Some(surface) = Self::window_surface_id(window) {
            self.clear_assigned_rect_for_surface(&surface);
            self.clear_reported_size_for_surface(&surface);
        }
    }

    pub(crate) fn initial_map_rect_for_window(&self, window: &Window) -> Rectangle<i32, Logical> {
        if let Some(rect) = self.window_exclusive_target_rect(window) {
            return rect;
        }

        if let Some(rect) = self.window_visual_or_assigned_rect(window) {
            return rect;
        }

        if self.is_window_floating(window) {
            return Rectangle::new(
                self.default_floating_location(window).into(),
                self.fallback_window_size(window),
            );
        }

        self.pre_layout_tiled_slot_for_window(window)
            .map(|(loc, size, _)| Rectangle::new(loc, size))
            .unwrap_or_else(|| Rectangle::new((0, 0).into(), self.fallback_window_size(window)))
    }

    pub fn initial_map_location_for_window(&self, window: &Window) -> (i32, i32) {
        let rect = self.initial_map_rect_for_window(window);
        (rect.loc.x, rect.loc.y)
    }

    pub(crate) fn map_window_to_initial_location(&mut self, window: &Window, activate: bool) {
        let rect = self.initial_map_rect_for_window(window);
        self.map_window_to_rect(window, rect, activate);
    }

    pub(crate) fn map_window_to_initial_location_if_mappable(
        &mut self,
        window: &Window,
        activate: bool,
    ) -> bool {
        // Workspaces may contain unmapped toplevel entries; those map only via root commit.
        if self.window_is_unmapped_toplevel(window) {
            return false;
        }
        self.map_window_to_initial_location(window, activate);
        true
    }

    pub(crate) fn pre_layout_tiled_slot_for_window(
        &self,
        window: &Window,
    ) -> Option<(Point<i32, Logical>, Size<i32, Logical>, Size<i32, Logical>)> {
        let output = self.space.outputs().next().cloned()?;
        let out_geo = self.space.output_geometry(&output)?;

        let mut layer_map = layer_map_for_output(&output);
        layer_map.arrange();
        let work_geo = layer_map.non_exclusive_zone();
        let layout_geo = if work_geo.size.w > 0 && work_geo.size.h > 0 {
            work_geo
        } else {
            out_geo
        };

        // Predict where this window will be after the next layout pass by arranging
        // the currently mapped tiled set plus this window (if it is not mapped yet).
        let mut tiled_windows: Vec<Window> = self
            .space
            .elements()
            .filter(|candidate| !self.is_window_floating(candidate))
            .filter(|candidate| Self::window_has_live_client(candidate))
            .filter(|candidate| Self::window_root_surface_has_buffer(candidate))
            .cloned()
            .collect();

        if !self.is_window_mapped(window) && !self.is_window_floating(window) {
            tiled_windows.push(window.clone());
        }

        if tiled_windows.is_empty() {
            return None;
        }

        let mut seen_surface_ids: HashSet<WlSurface> = HashSet::new();
        tiled_windows.retain(|candidate| {
            let Some(surface_id) = Self::window_surface_id(candidate) else {
                return true;
            };
            seen_surface_ids.insert(surface_id)
        });
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
            .iter()
            .zip(geometries.iter())
            .find_map(|(candidate, geom)| {
                if Self::windows_match(candidate, window) {
                    let loc = Point::<i32, Logical>::from((
                        layout_geo.loc.x + geom.x_coordinate,
                        layout_geo.loc.y + geom.y_coordinate,
                    ));
                    let size = Size::<i32, Logical>::from((geom.width as i32, geom.height as i32));
                    Some((loc, size, layout_geo.size))
                } else {
                    None
                }
            })
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
            self.map_window_to_location(&pending.window, pending.location, false);
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
            self.sync_reported_size_from_pending_state(&pending.window, Some(pending.size));
            toplevel.send_pending_configure();
        }
    }

    pub(crate) fn surface_app_id_and_title(
        surface: &WlSurface,
    ) -> (Option<String>, Option<String>) {
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

    fn has_matching_explicit_floating_rule(
        &self,
        app_id: Option<&str>,
        title: Option<&str>,
    ) -> bool {
        self.config
            .window_rules
            .iter()
            .any(|rule| rule.floating.is_some() && rule.matches(app_id, title))
    }

    fn surface_min_max_size(surface: &WlSurface) -> (Size<i32, Logical>, Size<i32, Logical>) {
        with_states(surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceCachedState>();
            let data = guard.current();
            (data.min_size, data.max_size)
        })
    }

    fn has_window_rule_metadata_gap(&self, app_id: Option<&str>, title: Option<&str>) -> bool {
        self.config.window_rules.iter().any(|rule| {
            ((rule.class.is_some() || rule.app_id.is_some()) && app_id.is_none())
                || (rule.title.is_some() && title.is_none())
        })
    }

    fn compute_auto_floating_for_surface(
        &self,
        surface: &WlSurface,
        window: &Window,
    ) -> (bool, &'static str) {
        if self.window_has_exclusive_layout_state(window) {
            return (false, "exclusive-state");
        }

        if window
            .toplevel()
            .is_some_and(|toplevel| toplevel.parent().is_some())
        {
            return (true, "parent");
        }

        let (min_size, max_size) = Self::surface_min_max_size(surface);
        if min_size.h > 0 && min_size.h == max_size.h {
            return (true, "fixed-height");
        }

        (false, "none")
    }

    fn fixed_hint_size_for_surface(surface: &WlSurface) -> Option<Size<i32, Logical>> {
        let (min_size, max_size) = Self::surface_min_max_size(surface);
        let fixed_w = min_size.w > 0 && min_size.w == max_size.w;
        let fixed_h = min_size.h > 0 && min_size.h == max_size.h;
        if fixed_w && fixed_h {
            Some(min_size)
        } else {
            None
        }
    }

    pub fn resolve_effective_floating_for_surface(
        &self,
        surface: &WlSurface,
        window: &Window,
        configured_floating: bool,
    ) -> (bool, bool, bool, &'static str) {
        let (app_id, title) = Self::surface_app_id_and_title(surface);
        let has_explicit_floating_rule =
            self.has_matching_explicit_floating_rule(app_id.as_deref(), title.as_deref());
        let (auto_floating, auto_reason) = self.compute_auto_floating_for_surface(surface, window);
        let final_floating = if has_explicit_floating_rule {
            configured_floating
        } else {
            auto_floating
        };

        (
            final_floating,
            has_explicit_floating_rule,
            auto_floating,
            auto_reason,
        )
    }

    pub fn queue_window_rule_recheck_for_surface(&mut self, surface: &WlSurface) {
        rules::queue_window_rule_recheck_for_surface(self, surface);
    }

    pub fn queue_floating_recenter_for_surface(&mut self, surface: &WlSurface) {
        rules::queue_floating_recenter_for_surface(self, surface);
    }

    pub fn clear_floating_recenter_for_surface(&mut self, surface: &WlSurface) {
        rules::clear_floating_recenter_for_surface(self, surface);
    }

    pub fn clear_window_rule_recheck_for_surface(&mut self, surface: &WlSurface) {
        rules::clear_window_rule_recheck_for_surface(self, surface);
    }

    pub fn queue_initial_configure_for_surface(&mut self, surface: &WlSurface) {
        rules::queue_initial_configure_for_surface(self, surface);
    }

    pub fn clear_initial_configure_for_surface(&mut self, surface: &WlSurface) {
        rules::clear_initial_configure_for_surface(self, surface);
    }

    // Match niri's behavior: send initial configure from an idle callback while still unmapped.
    pub fn queue_initial_configure_idle_for_surface(&mut self, surface: &WlSurface) {
        rules::queue_initial_configure_idle_for_surface(self, surface);
    }

    pub fn mark_surface_unmapped_toplevel(&mut self, surface: &WlSurface) {
        self.unmapped_toplevel_ids.insert(surface.clone());
    }

    pub fn clear_surface_unmapped_toplevel(&mut self, surface: &WlSurface) {
        self.unmapped_toplevel_ids.remove(surface);
    }

    pub fn is_surface_unmapped_toplevel(&self, surface: &WlSurface) -> bool {
        self.unmapped_toplevel_ids.contains(surface)
    }

    pub fn window_is_unmapped_toplevel(&self, window: &Window) -> bool {
        Self::window_surface_id(window)
            .as_ref()
            .is_some_and(|surface| self.unmapped_toplevel_ids.contains(surface))
    }

    pub fn queue_pending_unmapped_maximized_for_surface(&mut self, surface: &WlSurface) {
        self.pending_unmapped_maximized_ids.insert(surface.clone());
    }

    pub fn clear_pending_unmapped_maximized_for_surface(&mut self, surface: &WlSurface) {
        self.pending_unmapped_maximized_ids.remove(surface);
    }

    pub fn has_pending_unmapped_maximized_for_surface(&self, surface: &WlSurface) -> bool {
        self.pending_unmapped_maximized_ids.contains(surface)
    }

    pub fn clear_pending_unmapped_state_for_surface(&mut self, surface: &WlSurface) {
        // Full reset for unmapped->mapped bookkeeping on this root surface.
        self.fullscreen.pending_unmapped_ids.remove(surface);
        self.fullscreen.pending_transition_by_surface.remove(surface);
        self.fullscreen.restore_state_by_surface.remove(surface);
        self.fullscreen.maximized_surfaces.remove(surface);
        self.clear_fullscreen_owner_for_surface(surface);
        self.clear_assigned_rect_for_surface(surface);
        self.clear_reported_size_for_surface(surface);
        self.pending_unmapped_maximized_ids.remove(surface);
        self.pending_initial_configure_ids.remove(surface);
        self.pending_initial_configure_idle_ids.remove(surface);
        self.unmapped_toplevel_ids.remove(surface);
    }

    pub(crate) fn should_defer_window_rules_for_surface(&self, surface: &WlSurface) -> bool {
        rules::should_defer_window_rules_for_surface(self, surface)
    }

    pub fn resolve_window_rules_for_surface(&self, surface: &WlSurface) -> NewWindowRuleDecision {
        rules::resolve_window_rules_for_surface(self, surface)
    }

    pub fn apply_window_rule_size_to_window(
        &self,
        window: &Window,
        decision: &NewWindowRuleDecision,
    ) {
        rules::apply_window_rule_size_to_window(self, window, decision);
    }

    pub fn send_initial_configure_for_surface(&mut self, surface: &WlSurface) {
        rules::send_initial_configure_for_surface(self, surface);
    }

    fn workspace_index_for_window(&self, window: &Window) -> Option<usize> {
        workspaces::workspace_index_for_window(self, window)
    }

    fn move_window_to_workspace_internal(
        &mut self,
        window: &Window,
        target_workspace: usize,
    ) -> Result<(), CompositorError> {
        workspaces::move_window_to_workspace_internal(self, window, target_workspace)
    }

    pub fn maybe_apply_deferred_window_rules(&mut self, surface: &WlSurface) {
        rules::maybe_apply_deferred_window_rules(self, surface);
    }

    pub fn maybe_recenter_floating_window_after_commit(&mut self, surface: &WlSurface) {
        rules::maybe_recenter_floating_window_after_commit(self, surface);
    }

    pub fn handle_ipc_stream(&mut self, mut stream: UnixStream) {
        ipc::handle_ipc_stream(self, &mut stream);
    }

    pub(crate) fn is_window_mapped(&self, window: &Window) -> bool {
        self.space.element_location(window).is_some()
    }

    pub fn remove_window_from_workspaces(&mut self, window: &Window) {
        workspaces::remove_window_from_workspaces(self, window);
    }

    pub fn switch_workspace(&mut self, target_workspace: usize) -> Result<(), CompositorError> {
        workspaces::switch_workspace(self, target_workspace)
    }

    pub fn move_focused_window_to_workspace(
        &mut self,
        target_workspace: usize,
    ) -> Result<(), CompositorError> {
        workspaces::move_focused_window_to_workspace(self, target_workspace)
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
        // Keep child env neutral so apps that require X11 (Steam/Proton/game launchers)
        // can still select Xwayland via DISPLAY instead of being forced onto native Wayland.
        cmd.env_remove("MOZ_ENABLE_WAYLAND");
        cmd.env_remove("MOZ_DBUS_REMOTE");
        cmd.env_remove("QT_QPA_PLATFORM");
        cmd.env_remove("SDL_VIDEODRIVER");
        cmd.env_remove("NIXOS_OZONE_WL");
        cmd.env_remove("OZONE_PLATFORM");
        cmd.env_remove("OZONE_PLATFORM_HINT");
        cmd.env_remove("ELECTRON_OZONE_PLATFORM_HINT");
        cmd.env_remove("CHROMIUM_FLAGS");
        cmd.env_remove("BRAVE_USER_FLAGS");
        if self.config.no_csd {
            cmd.env("QT_WAYLAND_DISABLE_WINDOWDECORATION", "1");
        } else {
            cmd.env_remove("QT_WAYLAND_DISABLE_WINDOWDECORATION");
        }
        let xwayland_display = self.config.xwayland.display.trim();
        if self.config.xwayland.enabled && !xwayland_display.is_empty() {
            cmd.env("DISPLAY", xwayland_display);
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

        let xwayland_display = self.config.xwayland.display.trim();
        if self.config.xwayland.enabled && !xwayland_display.is_empty() {
            env_kv.push(format!("DISPLAY={xwayland_display}"));
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
        if self.config.xwayland.enabled && !xwayland_display.is_empty() {
            systemd_env_kv.push(format!("DISPLAY={xwayland_display}"));
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

        if !self.config.xwayland.enabled || xwayland_display.is_empty() {
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
        runtime::spawn_command(self, command);
    }

    pub fn run_startup_tasks(&mut self) {
        runtime::run_startup_tasks(self);
    }

    pub fn preferred_decoration_mode(&self) -> XdgDecorationMode {
        runtime::preferred_decoration_mode(self)
    }

    pub fn apply_decoration_preferences(&self) {
        runtime::apply_decoration_preferences(self);
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

    fn ensure_xwayland_display(&mut self) -> bool {
        if !self.config.xwayland.enabled {
            return false;
        }
        if !self.config.xwayland.display.trim().is_empty() {
            return false;
        }

        let Some(selected_display) = Self::find_free_x11_display() else {
            tracing::warn!("xwayland.display is unset and no free X11 DISPLAY was found");
            return false;
        };

        self.config.xwayland.display = selected_display.clone();
        tracing::info!(
            x11_display = %selected_display,
            "selected automatic Xwayland DISPLAY"
        );
        true
    }

    fn find_free_x11_display() -> Option<String> {
        for display_num in 0..100 {
            let socket_path = format!("/tmp/.X11-unix/X{display_num}");
            let lock_path = format!("/tmp/.X{display_num}-lock");
            if !Path::new(&socket_path).exists() && !Path::new(&lock_path).exists() {
                return Some(format!(":{display_num}"));
            }
        }
        None
    }

    fn desired_xwayland_satellite_signature(&self) -> Option<String> {
        if !self.config.xwayland.enabled {
            return None;
        }

        let path = self.config.xwayland.path.trim();
        let x11_display_text = self.config.xwayland.display.trim();
        if path.is_empty() || x11_display_text.is_empty() {
            return None;
        }

        Some(format!("{path}|{x11_display_text}"))
    }

    fn note_xwayland_satellite_failure(&mut self, reason: &str) {
        self.xwayland_satellite_failure_count =
            self.xwayland_satellite_failure_count.saturating_add(1);
        let exp = self.xwayland_satellite_failure_count.min(5);
        let backoff_secs = (1u64 << exp).min(30);
        let backoff = Duration::from_secs(backoff_secs);
        self.xwayland_satellite_backoff_until = Some(Instant::now() + backoff);
        tracing::warn!(
            reason = reason,
            failures = self.xwayland_satellite_failure_count,
            backoff_secs = backoff_secs,
            "xwayland-satellite failure; delaying restart"
        );
    }

    fn xwayland_satellite_log_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("log")
            .join("xwayland-satellite.log")
    }

    fn prepare_xwayland_satellite_log_stdio(&self) -> (Stdio, Stdio, Option<PathBuf>) {
        let log_path = Self::xwayland_satellite_log_path();
        if let Some(parent) = log_path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            tracing::warn!(
                path = %parent.display(),
                "failed to create xwayland-satellite log directory: {err}"
            );
            return (Stdio::null(), Stdio::null(), None);
        }

        let mut file = match OpenOptions::new().create(true).append(true).open(&log_path) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!(
                    path = %log_path.display(),
                    "failed to open xwayland-satellite log file: {err}"
                );
                return (Stdio::null(), Stdio::null(), None);
            }
        };
        let _ = writeln!(
            file,
            "\n===== Raven start xwayland-satellite: display={} wayland={} =====",
            self.config.xwayland.display.trim(),
            self.socket_name.to_string_lossy()
        );

        let stderr_file = match file.try_clone() {
            Ok(clone) => clone,
            Err(err) => {
                tracing::warn!(
                    path = %log_path.display(),
                    "failed to clone xwayland-satellite log file handle: {err}"
                );
                return (Stdio::null(), Stdio::null(), None);
            }
        };

        (Stdio::from(file), Stdio::from(stderr_file), Some(log_path))
    }

    fn log_xwayland_satellite_context(&self, reason: &str) {
        tracing::info!(
            reason = reason,
            xwayland_enabled = self.config.xwayland.enabled,
            xwayland_path = self.config.xwayland.path.trim(),
            xwayland_display = self.config.xwayland.display.trim(),
            wayland_display = %self.socket_name.to_string_lossy(),
            "xwayland-satellite context"
        );
    }

    fn stop_xwayland_satellite(&mut self) {
        let Some(mut child) = self.xwayland_satellite.take() else {
            self.xwayland_satellite_signature = None;
            self.xwayland_satellite_started_at = None;
            return;
        };

        let pid = child.id();
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::info!(pid = pid, ?status, "xwayland-satellite already exited");
            }
            Ok(None) => {
                if let Err(err) = child.kill()
                    && err.kind() != std::io::ErrorKind::InvalidInput
                {
                    tracing::warn!(pid = pid, "failed to kill xwayland-satellite: {err}");
                }
                match child.wait() {
                    Ok(status) => {
                        tracing::info!(pid = pid, ?status, "stopped xwayland-satellite");
                    }
                    Err(err) => {
                        tracing::warn!(pid = pid, "failed to wait xwayland-satellite: {err}");
                    }
                }
            }
            Err(err) => {
                tracing::warn!(pid = pid, "failed to poll xwayland-satellite: {err}");
            }
        }

        self.xwayland_satellite_signature = None;
        self.xwayland_satellite_started_at = None;
    }

    fn spawn_xwayland_satellite(&mut self, signature: String) {
        let path = self.config.xwayland.path.trim();
        let x11_display_text = self.config.xwayland.display.trim();
        let satellite_rust_log = std::env::var("RAVEN_XWAYLAND_SATELLITE_RUST_LOG")
            .unwrap_or_else(|_| "xwayland_satellite=warn,xwayland_process=warn".to_owned());
        let (stdout, stderr, satellite_log_path) = self.prepare_xwayland_satellite_log_stdio();
        let mut cmd = Command::new(path);
        cmd.arg(x11_display_text)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        self.apply_wayland_child_env(&mut cmd);
        // Match niri: xwayland-satellite itself should not run with DISPLAY set.
        cmd.env_remove("DISPLAY");
        cmd.env("RUST_LOG", &satellite_rust_log);
        cmd.env_remove("RUST_BACKTRACE");
        cmd.env_remove("RUST_LIB_BACKTRACE");

        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                self.xwayland_satellite = Some(child);
                self.xwayland_satellite_signature = Some(signature);
                self.xwayland_satellite_started_at = Some(Instant::now());
                tracing::info!(
                    pid = pid,
                    path = path,
                    x11_display = x11_display_text,
                    satellite_rust_log = satellite_rust_log,
                    satellite_log = satellite_log_path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "<disabled>".to_owned()),
                    "started xwayland-satellite"
                );
            }
            Err(err) => {
                tracing::warn!(
                    path = path,
                    x11_display = x11_display_text,
                    satellite_rust_log = satellite_rust_log,
                    satellite_log = satellite_log_path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "<disabled>".to_owned()),
                    "failed to start xwayland-satellite: {err}"
                );
                self.note_xwayland_satellite_failure("spawn failed");
            }
        }
    }

    pub fn maintain_xwayland_satellite(&mut self) {
        let desired_signature = self.desired_xwayland_satellite_signature();

        if desired_signature.is_none() {
            self.stop_xwayland_satellite();
            self.xwayland_satellite_backoff_until = None;
            self.xwayland_satellite_failure_count = 0;
            return;
        }

        let desired_signature = desired_signature.expect("checked is_some");
        let mut observed_exit = None;
        let mut observed_probe_error = None;
        if let Some(child) = self.xwayland_satellite.as_mut() {
            let pid = child.id();
            match child.try_wait() {
                Ok(Some(status)) => observed_exit = Some((pid, status)),
                Ok(None) => {}
                Err(err) => observed_probe_error = Some((pid, err)),
            }
        }

        if let Some((pid, status)) = observed_exit {
            tracing::warn!(pid = pid, ?status, "xwayland-satellite exited");
            let short_lived = self
                .xwayland_satellite_started_at
                .is_some_and(|started| started.elapsed() < Duration::from_secs(8));
            self.xwayland_satellite = None;
            self.xwayland_satellite_signature = None;
            self.xwayland_satellite_started_at = None;
            if short_lived {
                self.note_xwayland_satellite_failure("exited soon after start");
            } else {
                self.xwayland_satellite_backoff_until = None;
                self.xwayland_satellite_failure_count = 0;
            }
        }

        if let Some((pid, err)) = observed_probe_error {
            tracing::warn!(pid = pid, "failed to poll xwayland-satellite status: {err}");
            self.xwayland_satellite = None;
            self.xwayland_satellite_signature = None;
            self.xwayland_satellite_started_at = None;
            self.note_xwayland_satellite_failure("status poll failed");
        }

        if self.xwayland_satellite.is_some() {
            if self.xwayland_satellite_signature.as_deref() == Some(desired_signature.as_str()) {
                if self.xwayland_satellite_failure_count > 0
                    && self
                        .xwayland_satellite_started_at
                        .is_some_and(|started| started.elapsed() >= Duration::from_secs(15))
                {
                    self.xwayland_satellite_failure_count = 0;
                    self.xwayland_satellite_backoff_until = None;
                }
                return;
            }

            tracing::info!("restarting xwayland-satellite due to config/display change");
            self.stop_xwayland_satellite();
            self.xwayland_satellite_backoff_until = None;
            self.xwayland_satellite_failure_count = 0;
        }

        if let Some(backoff_until) = self.xwayland_satellite_backoff_until
            && Instant::now() < backoff_until
        {
            return;
        }

        self.xwayland_satellite_backoff_until = None;
        self.spawn_xwayland_satellite(desired_signature);
    }

    fn ensure_waypaper_swww_daemon(&self) {
        let restore_command = self.config.wallpaper.restore_command.trim();
        if restore_command != "waypaper --restore" {
            return;
        }

        // Ensure an swww-daemon exists for waypaper's default namespace handling.
        // Note: intentionally not gated by wallpaper.enabled; this is a helper for
        // users who keep waypaper restore configured but disable built-in wallpaper.
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
        runtime::reload_config(self)
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
            self.map_window_to_initial_location(&window, true);
        }

        self.apply_layout()
    }

    pub fn refresh_foreign_toplevel(&mut self) {
        crate::protocols::foreign_toplevel::refresh(self);
    }

    pub fn refresh_ext_workspace(&mut self) {
        crate::protocols::ext_workspace::refresh(self);
    }
}

impl Drop for Raven {
    fn drop(&mut self) {
        self.stop_xwayland_satellite();
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
            tune_wayland_client_socket_buffers(&client_stream);
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
                Ok(PostAction::Continue)
            },
        )
        .expect("failed to init display event source");

    socket_name
}

fn tune_wayland_client_socket_buffers(stream: &UnixStream) {
    const TARGET_BUFFER_BYTES: libc::c_int = 16 * 1024 * 1024;

    let fd = stream.as_raw_fd();
    let value = TARGET_BUFFER_BYTES;
    let value_ptr = (&value as *const libc::c_int).cast::<libc::c_void>();
    let value_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;

    // Best-effort tuning: if this fails we keep the default kernel socket sizes.
    unsafe {
        if libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, value_ptr, value_len) != 0 {
            let err = std::io::Error::last_os_error();
            tracing::debug!("failed to tune client socket receive buffer: {err}");
        }
        if libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF, value_ptr, value_len) != 0 {
            let err = std::io::Error::last_os_error();
            tracing::debug!("failed to tune client socket send buffer: {err}");
        }
    }
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
