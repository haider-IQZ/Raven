use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use smithay::{
    backend::{
        allocator::{
            Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmEventTime, DrmNode, NodeType,
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLDevice, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            ImportAll, ImportDma, ImportMem, ImportMemWl,
            element::{
                AsRenderElements, Kind, default_primary_scanout_output_compare,
                memory::MemoryRenderBuffer,
                surface::WaylandSurfaceRenderElement,
                utils::CropRenderElement,
            },
            gles::GlesRenderer,
            multigpu::{GpuManager, MultiRenderer, gbm::GbmGlesBackend},
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    desktop::{
        layer_map_for_output,
        utils::{
            OutputPresentationFeedback, surface_presentation_feedback_flags_from_states,
            surface_primary_scanout_output, update_surface_primary_scanout_output,
        },
        space::{SpaceRenderElements, space_render_elements},
    },
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    output::{Mode as WlMode, Output, PhysicalProperties, Scale as OutputScale, Subpixel},
    reexports::{
        calloop::{
            EventLoop, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{Mode, ModeTypeFlags, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
        wayland_server::{backend::GlobalId, protocol::wl_surface::WlSurface},
    },
    utils::{DeviceFd, IsAlive, Rectangle, Scale, Transform},
    wayland::{
        compositor,
        dmabuf::{DmabufFeedbackBuilder, DmabufState},
        drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd},
        presentation::Refresh,
        shell::wlr_layer::Layer as WlrLayer,
    },
};
use smithay_drm_extras::{
    display_info,
    drm_scanner::{DrmScanEvent, DrmScanner},
};

use crate::{
    CompositorError, Raven,
    config::MonitorConfig,
    cursor::{CursorThemeManager, PointerElement, PointerRenderElement},
    render_helpers::{SolidColorBuffer, SolidColorRenderElement},
    vblank_throttle::VBlankThrottle,
};

// Supported color formats for DRM output
const SUPPORTED_FORMATS: &[Fourcc] = &[Fourcc::Abgr8888, Fourcc::Argb8888];

// Background clear color (same as winit backend)
const CLEAR_COLOR: [f32; 4] = [150.0 / 255.0, 154.0 / 255.0, 171.0 / 255.0, 1.0];

fn env_truthy(name: &str) -> Option<bool> {
    std::env::var_os(name).map(|value| {
        let value = value.to_string_lossy().to_ascii_lowercase();
        matches!(value.as_str(), "1" | "true" | "yes" | "on")
    })
}

fn scanout_enabled() -> bool {
    static ENABLE_SCANOUT: OnceLock<bool> = OnceLock::new();
    *ENABLE_SCANOUT.get_or_init(|| {
        // Performance-first default: keep scanout enabled unless explicitly disabled.
        if let Some(disabled) = env_truthy("RAVEN_DISABLE_SCANOUT") {
            return !disabled;
        }
        env_truthy("RAVEN_ENABLE_SCANOUT").unwrap_or(true)
    })
}

fn frame_flags() -> FrameFlags {
    if scanout_enabled() {
        FrameFlags::DEFAULT
    } else {
        FrameFlags::empty()
    }
}

fn force_full_redraw() -> bool {
    static FORCE_FULL_REDRAW: OnceLock<bool> = OnceLock::new();
    *FORCE_FULL_REDRAW.get_or_init(|| {
        std::env::var_os("RAVEN_FORCE_FULL_REDRAW")
            .map(|value| {
                let value = value.to_string_lossy().to_ascii_lowercase();
                matches!(value.as_str(), "1" | "true" | "yes" | "on")
            })
            .unwrap_or(false)
    })
}

fn scanout_rejection_reason(
    state: &Raven,
    output: &Output,
    fullscreen_on_output: bool,
    transition_clip_active: bool,
) -> Option<&'static str> {
    if !fullscreen_on_output {
        return Some("fullscreen-not-ready");
    }

    if transition_clip_active {
        return Some("fullscreen-transition-active");
    }

    let Some(output_geo) = state.space.output_geometry(output) else {
        return Some("missing-output-geometry");
    };

    let cursor_visible_on_output = output_geo.to_f64().contains(state.pointer_location)
        && !matches!(state.cursor_status, CursorImageStatus::Hidden);
    if cursor_visible_on_output {
        return Some("cursor-visible");
    }

    let layer_map = layer_map_for_output(output);
    let overlay_or_top_visible = layer_map.layers_on(WlrLayer::Overlay).next().is_some()
        || layer_map.layers_on(WlrLayer::Top).next().is_some();
    if overlay_or_top_visible {
        return Some("overlay-or-top-layer-visible");
    }

    None
}

// Type aliases for the renderer stack
type UdevRenderer<'a> = MultiRenderer<
    'a,
    'a,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

type GbmFbExporter = GbmFramebufferExporter<DrmDeviceFd>;

smithay::backend::renderer::element::render_elements! {
    pub UdevRenderElement<R, E> where R: ImportAll + ImportMem;
    Backdrop=SolidColorRenderElement,
    Space=SpaceRenderElements<R, E>,
    Pointer=PointerRenderElement<R>,
}

smithay::backend::renderer::element::render_elements! {
    pub UdevCompositeRenderElement<R, E> where R: ImportAll + ImportMem;
    Base=UdevRenderElement<R, E>,
    Cropped=CropRenderElement<UdevRenderElement<R, E>>,
}

/// Per-GPU device state
struct BackendData {
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    drm_output_manager: DrmOutputManager<
        GbmAllocator<DrmDeviceFd>,
        GbmFbExporter,
        Option<OutputPresentationFeedback>,
        DrmDeviceFd,
    >,
    drm_scanner: DrmScanner,
    render_node: Option<DrmNode>,
    registration_token: RegistrationToken,
}

#[derive(Debug, Default)]
enum RedrawState {
    #[default]
    Idle,
    Queued,
    WaitingForVBlank { redraw_needed: bool },
    WaitingForEstimatedVBlank(RegistrationToken),
    WaitingForEstimatedVBlankAndQueued(RegistrationToken),
}

impl RedrawState {
    fn queue_redraw(self) -> Self {
        match self {
            RedrawState::Idle => RedrawState::Queued,
            RedrawState::WaitingForEstimatedVBlank(token) => {
                RedrawState::WaitingForEstimatedVBlankAndQueued(token)
            }
            value @ (RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)) => {
                value
            }
            RedrawState::WaitingForVBlank { .. } => RedrawState::WaitingForVBlank {
                redraw_needed: true,
            },
        }
    }
}

/// Per-CRTC/output state
struct SurfaceData {
    output: Output,
    global: Option<GlobalId>,
    drm_output: DrmOutput<
        GbmAllocator<DrmDeviceFd>,
        GbmFbExporter,
        Option<OutputPresentationFeedback>,
        DrmDeviceFd,
    >,
    backdrop: SolidColorBuffer,
    redraw_state: RedrawState,
    frame_callback_sequence: u32,
    vblank_throttle: VBlankThrottle,
}

impl Drop for SurfaceData {
    fn drop(&mut self) {
        self.output.leave_all();
    }
}

#[derive(Default)]
struct SurfaceFrameThrottlingState {
    last_sent_at: RefCell<Option<(Output, u32)>>,
}

/// DRM/udev backend data stored alongside the compositor state
pub struct UdevData {
    pub session: LibSeatSession,
    pub primary_gpu: DrmNode,
    pub gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    cursor_theme: CursorThemeManager,
    pointer_images: Vec<(xcursor::parser::Image, MemoryRenderBuffer)>,
    backends: HashMap<DrmNode, BackendData>,
    queued_redraws: HashSet<(DrmNode, crtc::Handle)>,
}

/// Initialize the DRM/KMS backend
pub fn init_udev(event_loop: &mut EventLoop<Raven>, state: &mut Raven) -> crate::Result<()> {
    // 1. Initialize libseat session
    let (session, notifier) = LibSeatSession::new()
        .map_err(|e| CompositorError::Backend(format!("failed to create session: {e}")))?;

    let seat_name = session.seat();

    // 2. Detect primary GPU
    let primary_gpu = find_primary_gpu(&session)
        .ok_or_else(|| CompositorError::Backend("no GPU found".into()))?;
    tracing::info!(?primary_gpu, "Using primary GPU");

    // 3. Create GpuManager
    let gpus = GpuManager::new(GbmGlesBackend::default())
        .map_err(|e| CompositorError::Backend(format!("failed to create GPU manager: {e}")))?;

    // 4. Store udev data in state
    state.udev_data = Some(UdevData {
        session: session.clone(),
        primary_gpu,
        gpus,
        cursor_theme: CursorThemeManager::load(),
        pointer_images: Vec::new(),
        backends: HashMap::new(),
        queued_redraws: HashSet::new(),
    });

    // 5. Create UdevBackend for device enumeration
    let udev_backend = UdevBackend::new(&seat_name)
        .map_err(|e| CompositorError::Backend(format!("failed to create udev backend: {e}")))?;

    // 6. Process initial devices
    for (device_id, path) in udev_backend.device_list() {
        if let Ok(node) = DrmNode::from_dev_id(device_id) {
            if let Err(e) = device_added(state, node, &path, &state.loop_handle.clone()) {
                tracing::warn!(?node, "Failed to add device: {e}");
            }
        }
    }

    // 7. Set up dmabuf support
    setup_dmabuf(state)?;
    // 7b. Set up explicit sync global when supported by the DRM import device.
    setup_syncobj(state);

    // 8. Create libinput backend
    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput_context
        .udev_assign_seat(&seat_name)
        .map_err(|_| CompositorError::Backend("failed to assign seat to libinput".into()))?;
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

    // 9. Register libinput event source
    event_loop
        .handle()
        .insert_source(libinput_backend, |event, _, state| {
            state.handle_input_event(event);
        })
        .map_err(|e| CompositorError::Backend(format!("failed to insert libinput source: {e}")))?;

    // 10. Register session notifier
    event_loop
        .handle()
        .insert_source(notifier, move |event, _, state| {
            handle_session_event(state, event, &mut libinput_context.clone());
        })
        .map_err(|e| CompositorError::Backend(format!("failed to insert session source: {e}")))?;

    // 11. Register udev event source for hotplug
    event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, state| match event {
            UdevEvent::Added { device_id, path } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    if let Err(e) = device_added(state, node, &path, &state.loop_handle.clone()) {
                        tracing::warn!(?node, "Failed to add device: {e}");
                    } else {
                        setup_syncobj(state);
                    }
                }
            }
            UdevEvent::Changed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    device_changed(state, node);
                }
            }
            UdevEvent::Removed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    device_removed(state, node);
                }
            }
        })
        .map_err(|e| CompositorError::Backend(format!("failed to insert udev source: {e}")))?;

    // 12. Set WAYLAND_DISPLAY for child processes
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };

    // Refresh activation environment after backend globals are known.
    state.sync_activation_environment();

    tracing::info!(
        socket = ?state.socket_name,
        "DRM/KMS backend initialized"
    );

    Ok(())
}

pub fn reload_cursor_theme(state: &mut Raven) {
    let Some(udev) = state.udev_data.as_mut() else {
        return;
    };

    udev.cursor_theme = CursorThemeManager::load();
    udev.pointer_images.clear();
    tracing::info!("reloaded cursor theme");
}

/// Import buffers for a committed surface early, before the next render pass.
pub fn early_import(state: &mut Raven, surface: &WlSurface) {
    let Some(udev) = state.udev_data.as_mut() else {
        return;
    };

    if let Err(err) = udev.gpus.early_import(udev.primary_gpu, surface) {
        tracing::debug!("early import failed: {err:?}");
    }
}

/// Queue all outputs for redraw on the next drain cycle.
pub fn queue_redraw_all(state: &mut Raven) {
    let Some(udev) = state.udev_data.as_mut() else {
        return;
    };

    let mut to_queue: Vec<(DrmNode, crtc::Handle)> = Vec::new();
    for (node, backend) in &mut udev.backends {
        for (crtc, surface) in &mut backend.surfaces {
            surface.redraw_state = std::mem::take(&mut surface.redraw_state).queue_redraw();
            to_queue.push((*node, *crtc));
        }
    }

    for entry in to_queue {
        udev.queued_redraws.insert(entry);
    }
}

/// Queue a specific output for redraw on the next drain cycle.
pub fn queue_redraw_for_output(state: &mut Raven, output: &Output) {
    let Some(udev) = state.udev_data.as_mut() else {
        return;
    };

    let mut to_queue: Vec<(DrmNode, crtc::Handle)> = Vec::new();
    for (node, backend) in &mut udev.backends {
        for (crtc, surface) in &mut backend.surfaces {
            if surface.output == *output {
                surface.redraw_state = std::mem::take(&mut surface.redraw_state).queue_redraw();
                to_queue.push((*node, *crtc));
            }
        }
    }

    if to_queue.is_empty() {
        return;
    }

    for entry in to_queue {
        udev.queued_redraws.insert(entry);
    }
}

/// Drain queued redraw requests and render each targeted output once.
pub fn drain_queued_redraws(state: &mut Raven) {
    let queued = {
        let Some(udev) = state.udev_data.as_mut() else {
            return;
        };
        std::mem::take(&mut udev.queued_redraws)
    };

    for (node, crtc) in queued {
        render_surface(state, node, crtc);
    }
}

/// Find the primary GPU node
fn find_primary_gpu(session: &LibSeatSession) -> Option<DrmNode> {
    primary_gpu(session.seat())
        .ok()
        .flatten()
        .and_then(|path| DrmNode::from_path(path).ok())
        .and_then(|node| {
            node.node_with_type(NodeType::Render)
                .and_then(|n| n.ok())
                .or(Some(node))
        })
        .or_else(|| {
            all_gpus(session.seat()).ok().and_then(|gpus| {
                gpus.into_iter()
                    .find_map(|path| DrmNode::from_path(path).ok())
            })
        })
}

/// Set up DmabufState with default feedback from the primary GPU renderer
fn setup_dmabuf(state: &mut Raven) -> crate::Result<()> {
    let udev = state.udev_data.as_mut().unwrap();
    let primary_gpu = udev.primary_gpu;

    let renderer = udev
        .gpus
        .single_renderer(&primary_gpu)
        .map_err(|e| CompositorError::Backend(format!("failed to get renderer: {e}")))?;

    // Update SHM formats from renderer
    let shm_formats = renderer.shm_formats();
    state.shm_state.update_formats(shm_formats);

    // Set up dmabuf global
    let dmabuf_formats = renderer.dmabuf_formats();
    let default_feedback = DmabufFeedbackBuilder::new(primary_gpu.dev_id(), dmabuf_formats.clone())
        .build()
        .map_err(|e| CompositorError::Backend(format!("failed to build dmabuf feedback: {e}")))?;

    let mut dmabuf_state = DmabufState::new();
    let _global = dmabuf_state
        .create_global_with_default_feedback::<Raven>(&state.display_handle, &default_feedback);

    state.dmabuf_state = Some(dmabuf_state);

    Ok(())
}

/// Set up linux-drm-syncobj-v1 if the import device supports syncobj_eventfd.
fn setup_syncobj(state: &mut Raven) {
    if state.syncobj_state.is_some() {
        return;
    }

    let import_device = {
        let Some(udev) = state.udev_data.as_ref() else {
            return;
        };

        // Prefer the primary node backend when available.
        let primary_node = udev
            .primary_gpu
            .node_with_type(NodeType::Primary)
            .and_then(|node| node.ok())
            .unwrap_or(udev.primary_gpu);

        udev.backends
            .get(&primary_node)
            .or_else(|| udev.backends.get(&udev.primary_gpu))
            .map(|backend| backend.drm_output_manager.device().device_fd().clone())
    };

    let Some(import_device) = import_device else {
        return;
    };

    if supports_syncobj_eventfd(&import_device) {
        let syncobj_state = DrmSyncobjState::new::<Raven>(&state.display_handle, import_device);
        state.syncobj_state = Some(syncobj_state);
        tracing::info!("enabled linux-drm-syncobj protocol");
    } else {
        tracing::info!("linux-drm-syncobj unsupported by DRM import device");
    }
}

/// Handle a new DRM device being added
fn device_added(
    state: &mut Raven,
    node: DrmNode,
    path: &Path,
    handle: &smithay::reexports::calloop::LoopHandle<'static, Raven>,
) -> crate::Result<()> {
    let udev = state.udev_data.as_mut().unwrap();

    // Open the DRM device via libseat
    let fd = udev
        .session
        .open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .map_err(|e| CompositorError::Backend(format!("failed to open DRM device: {e}")))?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));

    // Create DRM and GBM devices
    let (drm, notifier) = DrmDevice::new(fd.clone(), true)
        .map_err(|e| CompositorError::Backend(format!("failed to create DRM device: {e}")))?;
    let gbm = GbmDevice::new(fd.clone())
        .map_err(|e| CompositorError::Backend(format!("failed to create GBM device: {e}")))?;

    // Register DRM event notifier for VBlank events
    let registration_token = handle
        .insert_source(notifier, move |event, metadata, state| match event {
            DrmEvent::VBlank(crtc) => {
                frame_finish(state, node, crtc, metadata);
            }
            DrmEvent::Error(error) => {
                tracing::error!(?error, "DRM error");
            }
        })
        .map_err(|e| CompositorError::Backend(format!("failed to insert DRM notifier: {e}")))?;

    // Try to get the render node via EGL
    let render_node = match unsafe { EGLDisplay::new(gbm.clone()) } {
        Ok(display) => {
            let egl_device = EGLDevice::device_for_display(&display).ok();
            let rn = egl_device
                .and_then(|dev| dev.try_get_render_node().ok().flatten())
                .unwrap_or(node);
            if let Err(e) = udev.gpus.as_mut().add_node(rn, gbm.clone()) {
                tracing::warn!("Failed to add GPU node: {e}");
                None
            } else {
                Some(rn)
            }
        }
        Err(e) => {
            tracing::warn!("Failed to create EGL display: {e}");
            None
        }
    };

    // Create allocator and framebuffer exporter
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let framebuffer_exporter = GbmFramebufferExporter::new(gbm.clone(), render_node.into());

    // Get renderer formats
    let render_formats = if let Some(rn) = render_node {
        udev.gpus
            .single_renderer(&rn)
            .ok()
            .map(|renderer| {
                renderer
                    .as_ref()
                    .egl_context()
                    .dmabuf_render_formats()
                    .clone()
            })
            .unwrap_or_default()
    } else {
        Default::default()
    };

    // Create DrmOutputManager
    let drm_output_manager = DrmOutputManager::new(
        drm,
        allocator,
        framebuffer_exporter,
        Some(gbm),
        SUPPORTED_FORMATS.iter().copied(),
        render_formats,
    );

    udev.backends.insert(
        node,
        BackendData {
            registration_token,
            drm_output_manager,
            drm_scanner: DrmScanner::new(),
            render_node,
            surfaces: HashMap::new(),
        },
    );

    // Scan for connected connectors
    device_changed(state, node);

    tracing::info!(?node, ?render_node, "DRM device added");
    Ok(())
}

/// Handle DRM device changes (connector hotplug)
fn device_changed(state: &mut Raven, node: DrmNode) {
    let udev = state.udev_data.as_mut().unwrap();
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };

    let scan_result = match device
        .drm_scanner
        .scan_connectors(device.drm_output_manager.device())
    {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!(?node, "Failed to scan connectors: {e}");
            return;
        }
    };

    let events: Vec<_> = scan_result.into_iter().collect();

    for event in events {
        match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => {
                connector_connected(state, node, connector, crtc);
            }
            DrmScanEvent::Disconnected {
                connector: _,
                crtc: Some(crtc),
            } => {
                connector_disconnected(state, node, crtc);
            }
            _ => {}
        }
    }
}

/// Handle a connector being connected
fn connector_connected(
    state: &mut Raven,
    node: DrmNode,
    connector: connector::Info,
    crtc: crtc::Handle,
) {
    let loop_handle = state.loop_handle.clone();
    let udev = state.udev_data.as_mut().unwrap();
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };

    let render_node = device.render_node.unwrap_or(udev.primary_gpu);

    // Get display info from EDID
    let display_info =
        display_info::for_connector(device.drm_output_manager.device(), connector.handle());
    let make = display_info
        .as_ref()
        .and_then(|info| info.make())
        .unwrap_or_else(|| "Unknown".into());
    let model = display_info
        .as_ref()
        .and_then(|info| info.model())
        .unwrap_or_else(|| "Unknown".into());
    let serial_number = display_info
        .as_ref()
        .and_then(|info| info.serial())
        .unwrap_or_else(|| "Unknown".into());

    tracing::info!(%make, %model, %serial_number, "Connector connected");

    // Determine output name
    let output_name = format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    );
    let monitor_config = select_monitor_config(&state.config.monitors, &output_name);

    if let Some(monitor) = monitor_config.as_ref()
        && !monitor.enabled
    {
        if state.space.outputs().next().is_none() {
            tracing::warn!(
                output = %output_name,
                "monitor is disabled in config, but no other outputs are active; keeping it enabled to avoid a black screen"
            );
        } else {
            tracing::info!(
                output = %output_name,
                "monitor is disabled in config; skipping connector"
            );
            return;
        }
    }

    if let Some(monitor) = monitor_config.as_ref() {
        tracing::info!(
            output = %output_name,
            matched_monitor = %monitor.name,
            "using monitor config"
        );
    }

    // Select mode from config if present, otherwise preferred mode.
    let mode_idx = select_mode_index(&output_name, connector.modes(), monitor_config.as_ref());
    let drm_mode = connector.modes()[mode_idx];
    let wl_mode = WlMode::from(drm_mode);

    // Create Wayland output
    let phys_size = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        output_name.clone(),
        PhysicalProperties {
            size: (phys_size.0 as i32, phys_size.1 as i32).into(),
            subpixel: Subpixel::Unknown,
            make,
            model,
            serial_number,
        },
    );
    let global = output.create_global::<Raven>(&state.display_handle);

    // Auto-position outputs left-to-right unless an explicit monitor position is set.
    let auto_x = state.space.outputs().fold(0, |acc, o| {
        acc + state
            .space
            .output_geometry(o)
            .map(|geo| geo.size.w)
            .unwrap_or(0)
    });
    let x = monitor_config
        .as_ref()
        .and_then(|monitor| monitor.x)
        .unwrap_or(auto_x);
    let y = monitor_config
        .as_ref()
        .and_then(|monitor| monitor.y)
        .unwrap_or(0);
    let transform = monitor_config
        .as_ref()
        .map(|monitor| monitor_transform_from_config(monitor.transform.as_deref(), &output_name))
        .unwrap_or(Transform::Normal);
    let scale = monitor_config
        .as_ref()
        .and_then(|monitor| monitor.scale)
        .map(output_scale_from_config)
        .unwrap_or(OutputScale::Integer(1));

    output.set_preferred(wl_mode);
    output.change_current_state(
        Some(wl_mode),
        Some(transform),
        Some(scale),
        Some((x, y).into()),
    );
    state.space.map_output(&output, (x, y));

    // Get renderer for this device
    let mut renderer = match udev.gpus.single_renderer(&render_node) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to get renderer for connector: {e}");
            return;
        }
    };

    // Initialize DrmOutput via the DrmOutputManager
    let drm_output = match device.drm_output_manager.lock().initialize_output::<
        _,
        SpaceRenderElements<UdevRenderer<'_>, WaylandSurfaceRenderElement<UdevRenderer<'_>>>,
    >(
        crtc,
        drm_mode,
        &[connector.handle()],
        &output,
        None,
        &mut renderer,
        &DrmOutputRenderElements::default(),
    ) {
        Ok(output) => output,
        Err(e) => {
            tracing::error!("Failed to initialize DRM output: {e:?}");
            return;
        }
    };

    device.surfaces.insert(
        crtc,
        SurfaceData {
            output: output.clone(),
            global: Some(global),
            drm_output,
            backdrop: SolidColorBuffer::new((wl_mode.size.w as f64, wl_mode.size.h as f64), CLEAR_COLOR),
            redraw_state: RedrawState::Queued,
            frame_callback_sequence: 0,
            vblank_throttle: VBlankThrottle::new(loop_handle, output_name.clone()),
        },
    );

    tracing::info!(
        ?crtc,
        output = %output_name,
        mode = ?wl_mode,
        transform = ?transform,
        scale = output.current_scale().fractional_scale(),
        position_x = x,
        position_y = y,
        "Output initialized"
    );

    // Schedule initial render
    let handle = state.loop_handle.clone();
    handle.insert_idle(move |state| {
        render_surface(state, node, crtc);
    });
}

fn select_mode_index(output_name: &str, modes: &[Mode], monitor: Option<&MonitorConfig>) -> usize {
    let preferred_idx = modes
        .iter()
        .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .unwrap_or(0);
    let Some(monitor) = monitor else {
        return preferred_idx;
    };
    let requested_size = monitor.width.zip(monitor.height);
    let requested_refresh = monitor.refresh_hz;

    if requested_size.is_none() && requested_refresh.is_none() {
        return preferred_idx;
    }

    let candidate = modes
        .iter()
        .enumerate()
        .filter(|(_, mode)| {
            if let Some((width, height)) = requested_size {
                mode.size() == (width, height)
            } else {
                true
            }
        })
        .min_by(|(_, left), (_, right)| {
            let left_refresh_diff = requested_refresh
                .map(|requested| ((left.vrefresh() as f64 - requested).abs() * 1000.0) as u64)
                .unwrap_or(0);
            let right_refresh_diff = requested_refresh
                .map(|requested| ((right.vrefresh() as f64 - requested).abs() * 1000.0) as u64)
                .unwrap_or(0);
            left_refresh_diff
                .cmp(&right_refresh_diff)
                .then_with(|| {
                    let left_preferred = left.mode_type().contains(ModeTypeFlags::PREFERRED);
                    let right_preferred = right.mode_type().contains(ModeTypeFlags::PREFERRED);
                    right_preferred.cmp(&left_preferred)
                })
                .then_with(|| right.vrefresh().cmp(&left.vrefresh()))
        });

    if let Some((index, selected_mode)) = candidate {
        tracing::info!(
            output = %output_name,
            requested_width = requested_size.map(|(w, _)| w),
            requested_height = requested_size.map(|(_, h)| h),
            requested_refresh_hz = requested_refresh,
            selected_width = selected_mode.size().0,
            selected_height = selected_mode.size().1,
            selected_refresh_hz = selected_mode.vrefresh(),
            "selected monitor mode from config"
        );
        return index;
    }

    tracing::warn!(
        output = %output_name,
        requested_width = requested_size.map(|(w, _)| w),
        requested_height = requested_size.map(|(_, h)| h),
        requested_refresh_hz = requested_refresh,
        "no mode matched monitor config; falling back to preferred mode"
    );
    preferred_idx
}

fn monitor_transform_from_config(raw: Option<&str>, output_name: &str) -> Transform {
    let Some(raw) = raw else {
        return Transform::Normal;
    };
    let key = raw.trim().to_ascii_lowercase();
    match key.as_str() {
        "normal" | "0" => Transform::Normal,
        "90" | "_90" | "rotate90" => Transform::_90,
        "180" | "_180" | "rotate180" => Transform::_180,
        "270" | "_270" | "rotate270" => Transform::_270,
        "flipped" | "flip" | "4" => Transform::Flipped,
        "flipped90" | "flip90" | "5" => Transform::Flipped90,
        "flipped180" | "flip180" | "6" => Transform::Flipped180,
        "flipped270" | "flip270" | "7" => Transform::Flipped270,
        _ => {
            tracing::warn!(
                output = %output_name,
                transform = raw,
                "invalid monitor transform, using normal"
            );
            Transform::Normal
        }
    }
}

fn output_scale_from_config(scale: f64) -> OutputScale {
    let rounded = scale.round();
    if rounded >= 1.0 && rounded <= i32::MAX as f64 && (scale - rounded).abs() < 1e-6 {
        OutputScale::Integer(rounded as i32)
    } else {
        OutputScale::Fractional(scale)
    }
}

fn output_name_matches(configured: &str, actual: &str) -> bool {
    configured.eq_ignore_ascii_case(actual)
        || canonical_output_name(configured).eq_ignore_ascii_case(&canonical_output_name(actual))
}

fn canonical_output_name(name: &str) -> String {
    let normalized = name.trim().to_ascii_uppercase();
    let mut parts = normalized.split('-').collect::<Vec<_>>();

    if parts.len() >= 3 {
        let last_index = parts.len() - 1;
        let penultimate_index = parts.len() - 2;
        let penultimate = parts[penultimate_index];
        let last = parts[last_index];

        if penultimate.len() == 1
            && penultimate.chars().all(|ch| ch.is_ascii_alphabetic())
            && last.chars().all(|ch| ch.is_ascii_digit())
        {
            parts.remove(penultimate_index);
        }
    }

    parts.join("-")
}

fn select_monitor_config(monitors: &[MonitorConfig], output_name: &str) -> Option<MonitorConfig> {
    monitors
        .iter()
        .find(|monitor| output_name_matches(&monitor.name, output_name))
        .cloned()
}

/// Handle a connector being disconnected
fn connector_disconnected(state: &mut Raven, node: DrmNode, crtc: crtc::Handle) {
    let udev = state.udev_data.as_mut().unwrap();
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };

    if let Some(mut surface_data) = device.surfaces.remove(&crtc) {
        state.space.unmap_output(&surface_data.output);
        if let Some(global) = surface_data.global.take() {
            state.display_handle.remove_global::<Raven>(global);
        }
        tracing::info!(?crtc, "Connector disconnected, output removed");
    }
}

/// Handle a DRM device being removed
fn device_removed(state: &mut Raven, node: DrmNode) {
    let udev = state.udev_data.as_mut().unwrap();
    if let Some(device) = udev.backends.remove(&node) {
        for (_crtc, mut surface_data) in device.surfaces {
            state.space.unmap_output(&surface_data.output);
            if let Some(global) = surface_data.global.take() {
                state.display_handle.remove_global::<Raven>(global);
            }
        }
        state.loop_handle.remove(device.registration_token);
        tracing::info!(?node, "DRM device removed");
    }
}

/// Render a surface for the given device and CRTC
fn render_surface(state: &mut Raven, node: DrmNode, crtc: crtc::Handle) {
    state.flush_interactive_frame_updates();
    let loop_handle = state.loop_handle.clone();
    let output = {
        let udev = state.udev_data.as_ref().unwrap();
        let Some(device) = udev.backends.get(&node) else {
            return;
        };
        let Some(surface_data) = device.surfaces.get(&crtc) else {
            return;
        };
        surface_data.output.clone()
    };
    let fullscreen_requested_on_output = state.output_has_fullscreen_window(&output);
    let fullscreen_on_output = state.output_has_ready_fullscreen_window(&output);
    let transition_full_redraw = state.take_fullscreen_transition_redraw_for_output(&output);
    let transition_clip_active =
        fullscreen_requested_on_output && (!fullscreen_on_output || transition_full_redraw);

    if fullscreen_requested_on_output {
        if !scanout_enabled() {
            state.record_scanout_rejection(&output, "scanout-disabled");
        } else if let Some(reason) =
            scanout_rejection_reason(state, &output, fullscreen_on_output, transition_clip_active)
        {
            state.record_scanout_rejection(&output, reason);
        }
    }

    let udev = state.udev_data.as_mut().unwrap();
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };
    let Some(surface_data) = device.surfaces.get_mut(&crtc) else {
        return;
    };
    match std::mem::take(&mut surface_data.redraw_state) {
        RedrawState::Queued => {}
        RedrawState::WaitingForEstimatedVBlankAndQueued(token) => {
            loop_handle.remove(token);
        }
        other => {
            surface_data.redraw_state = other;
            return;
        }
    }

    if let Some(output_geo) = state.space.output_geometry(&output) {
        surface_data.backdrop.update(
            (output_geo.size.w as f64, output_geo.size.h as f64),
            CLEAR_COLOR,
        );
    }
    if force_full_redraw() {
        surface_data.backdrop.touch();
    }
    let render_node = device.render_node.unwrap_or(udev.primary_gpu);
    let cursor_frame = udev.cursor_theme.image(1, state.clock.now().into());
    let pointer_image = udev
        .pointer_images
        .iter()
        .find_map(|(image, buffer)| {
            if image == &cursor_frame {
                Some(buffer.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            let buffer = MemoryRenderBuffer::from_slice(
                &cursor_frame.pixels_rgba,
                Fourcc::Argb8888,
                (cursor_frame.width as i32, cursor_frame.height as i32),
                1,
                Transform::Normal,
                None,
            );
            udev.pointer_images.push((cursor_frame, buffer.clone()));
            buffer
        });

    // Get renderer
    let mut renderer = match udev.gpus.single_renderer(&render_node) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to get renderer: {e}");
            return;
        }
    };

    // Collect render elements from the space (windows + layer surfaces).
    //
    // Full niri-style ordering on ready fullscreen:
    // overlay > windows > top > bottom/background.
    // Normal ordering remains the default from smithay space_render_elements.
    if transition_full_redraw {
        // One-shot full repaint when fullscreen becomes ready on this output. This avoids
        // transient bottom-edge artifacts without adding per-frame fullscreen repaint cost.
        surface_data.backdrop.touch();
    }
    let mut space_elements = match space_render_elements(&mut renderer, [&state.space], &output, 1.0)
    {
        Ok(elements) => elements,
        Err(e) => {
            tracing::warn!("Failed to collect render elements: {e:?}");
            Vec::new()
        }
    };
    if fullscreen_on_output {
        let mut window_elements = Vec::new();
        let mut lower_layer_elements = Vec::new();
        let mut saw_window_element = false;
        for element in space_elements {
            match element {
                SpaceRenderElements::Element(_) => {
                    saw_window_element = true;
                    window_elements.push(element);
                }
                SpaceRenderElements::Surface(_) if saw_window_element => {
                    lower_layer_elements.push(element);
                }
                SpaceRenderElements::Surface(_) => {}
                SpaceRenderElements::_GenericCatcher(_) => {}
            }
        }

        let layer_map = layer_map_for_output(&output);
        let mut collect_layer_elements = |layer: WlrLayer| {
            let mut out = Vec::new();
            for layer_surface in layer_map.layers_on(layer).rev() {
                let Some(layer_geo) = layer_map.layer_geometry(layer_surface) else {
                    continue;
                };
                out.extend(
                    AsRenderElements::<UdevRenderer<'_>>::render_elements::<
                        WaylandSurfaceRenderElement<UdevRenderer<'_>>,
                    >(
                        layer_surface,
                        &mut renderer,
                        layer_geo.loc.to_physical_precise_round(1.0),
                        Scale::from(1.0),
                        1.0,
                    )
                    .into_iter()
                    .map(SpaceRenderElements::Surface),
                );
            }
            out
        };
        let overlay_layer_elements = collect_layer_elements(WlrLayer::Overlay);
        let top_layer_elements = collect_layer_elements(WlrLayer::Top);

        let mut reordered = Vec::new();
        reordered.extend(overlay_layer_elements);
        reordered.extend(window_elements);
        reordered.extend(top_layer_elements);
        reordered.extend(lower_layer_elements);
        space_elements = reordered;
    }
    let output_scale = Scale::from(output.current_scale().fractional_scale());
    let fullscreen_window_crop_rect = state
        .fullscreen_windows
        .iter()
        .find(|window| {
            state
                .space
                .outputs_for_element(window)
                .iter()
                .any(|candidate| candidate == &output)
        })
        .and_then(|window| {
            let output_geo = state.space.output_geometry(&output)?;
            let bbox = state.space.element_bbox(window)?;
            Some(Rectangle::new(
                (bbox.loc - output_geo.loc)
                    .to_f64()
                    .to_physical(output_scale)
                    .to_i32_round(),
                bbox.size.to_f64().to_physical(output_scale).to_i32_round(),
            ))
        });
    let mut space_elements_converted: Vec<
        UdevCompositeRenderElement<UdevRenderer<'_>, WaylandSurfaceRenderElement<UdevRenderer<'_>>>,
    > = Vec::new();
    for element in space_elements {
        let is_window_element = matches!(element, SpaceRenderElements::Element(_));
        if transition_clip_active
            && is_window_element
            && let Some(crop_rect) = fullscreen_window_crop_rect
        {
            let base = UdevRenderElement::from(element);
            if let Some(cropped) = CropRenderElement::from_element(base, output_scale, crop_rect) {
                space_elements_converted.push(UdevCompositeRenderElement::from(cropped));
            }
            continue;
        }
        let base = UdevRenderElement::from(element);
        space_elements_converted.push(UdevCompositeRenderElement::from(base));
    }

    // Render order is front-to-back, so cursor elements must come first.
    let mut elements: Vec<
        UdevCompositeRenderElement<UdevRenderer<'_>, WaylandSurfaceRenderElement<UdevRenderer<'_>>>,
    > = Vec::new();

    // Render the cursor on outputs where the pointer currently is.
    if let Some(output_geo) = state.space.output_geometry(&output)
        && output_geo.to_f64().contains(state.pointer_location)
    {
        if let CursorImageStatus::Surface(ref surface) = state.cursor_status
            && !surface.alive()
        {
            state.cursor_status = CursorImageStatus::default_named();
        }

        let cursor_hotspot = if let CursorImageStatus::Surface(ref surface) = state.cursor_status {
            compositor::with_states(surface, |states| {
                states
                    .data_map
                    .get::<Mutex<CursorImageAttributes>>()
                    .and_then(|attrs| attrs.lock().ok().map(|attrs| attrs.hotspot))
                    .unwrap_or((0, 0).into())
            })
        } else {
            (0, 0).into()
        };

        let scale = Scale::from(output.current_scale().fractional_scale());
        let cursor_pos = state.pointer_location - output_geo.loc.to_f64();

        let mut pointer_element = PointerElement::default();
        pointer_element.set_buffer(pointer_image);
        pointer_element.set_status(state.cursor_status.clone());

        let pointer_elements: Vec<PointerRenderElement<UdevRenderer<'_>>> =
            pointer_element.render_elements(
                &mut renderer,
                (cursor_pos - cursor_hotspot.to_f64())
                    .to_physical(scale)
                    .to_i32_round(),
                scale,
                1.0,
            );
        elements.extend(
            pointer_elements
                .into_iter()
                .map(UdevRenderElement::from)
                .map(UdevCompositeRenderElement::from),
        );
    }

    elements.extend(space_elements_converted);
    elements.push(UdevCompositeRenderElement::from(UdevRenderElement::from(
        SolidColorRenderElement::from_buffer(&surface_data.backdrop, (0.0, 0.0), 1.0, Kind::Unspecified),
    )));

    // Render frame with collected elements
    let render_result = surface_data.drm_output.render_frame(
        &mut renderer,
        &elements,
        CLEAR_COLOR,
        frame_flags(),
    );

    match render_result {
        Ok(result) => {
            if result.needs_sync()
                && let smithay::backend::drm::compositor::PrimaryPlaneElement::Swapchain(
                    ref element,
                ) =
                    result.primary_element
                && let Err(err) = element.sync.wait()
            {
                tracing::warn!("error waiting for frame completion: {err:?}");
            }

            let rendered = !result.is_empty;

            if rendered {
                let render_element_states = result.states.clone();
                drop(result);

                let _ = surface_data;
                let _ = device;
                let _ = udev;

                update_primary_scanout_output_for_output(state, &output, &render_element_states);
                let output_presentation_feedback =
                    take_presentation_feedback_for_output(state, &output, &render_element_states);

                let queue_result = {
                    let udev = state.udev_data.as_mut().unwrap();
                    let Some(device) = udev.backends.get_mut(&node) else {
                        return;
                    };
                    let Some(surface_data) = device.surfaces.get_mut(&crtc) else {
                        return;
                    };

                    match surface_data
                        .drm_output
                        .queue_frame(Some(output_presentation_feedback))
                    {
                        Ok(()) => {
                            surface_data.redraw_state = RedrawState::WaitingForVBlank {
                                redraw_needed: false,
                            };
                            surface_data.frame_callback_sequence =
                                surface_data.frame_callback_sequence.wrapping_add(1);
                            Ok(surface_data.frame_callback_sequence)
                        }
                        Err(err) => {
                            tracing::error!("Failed to queue frame: {err:?}");
                            surface_data.redraw_state = RedrawState::Queued;
                            Err(())
                        }
                    }
                };

                if let Ok(frame_callback_sequence) = queue_result {
                    send_frame_callbacks_for_output(state, &output, frame_callback_sequence);
                }
            } else {
                // No frame was submitted to KMS; emulate vblank timing for callbacks.
                let refresh_rate = output
                    .current_mode()
                    .map(|mode| mode.refresh as u64)
                    .unwrap_or(60_000);
                let frame_duration = Duration::from_micros(1_000_000u64 / (refresh_rate / 1_000));
                let timer = Timer::from_duration(frame_duration);
                let token = loop_handle
                    .insert_source(timer, move |_, _, state| {
                        on_estimated_vblank_timer(state, node, crtc);
                        TimeoutAction::Drop
                    })
                    .ok();
                surface_data.redraw_state = if let Some(token) = token {
                    RedrawState::WaitingForEstimatedVBlank(token)
                } else {
                    RedrawState::Idle
                };
            }

            state.space.refresh();
            state.display_handle.flush_clients().unwrap();
        }
        Err(e) => {
            tracing::error!("Failed to render frame: {e:?}");
            surface_data.redraw_state = RedrawState::Queued;
        }
    }
}

fn send_frame_callbacks_for_output(
    state: &mut Raven,
    output: &Output,
    frame_callback_sequence: u32,
) {
    let should_send = |surface: &WlSurface, states: &compositor::SurfaceData| {
        if surface_primary_scanout_output(surface, states).as_ref() != Some(output) {
            return None;
        }

        let frame_throttling_state = states
            .data_map
            .get_or_insert(SurfaceFrameThrottlingState::default);
        let mut last_sent_at = frame_throttling_state.last_sent_at.borrow_mut();

        if let Some((last_output, last_sequence)) = &*last_sent_at
            && last_output == output
            && *last_sequence == frame_callback_sequence
        {
            return None;
        }

        *last_sent_at = Some((output.clone(), frame_callback_sequence));
        Some(output.clone())
    };

    state.space.elements().for_each(|window| {
        if state
            .space
            .outputs_for_element(window)
            .iter()
            .any(|candidate| candidate == output)
        {
            window.send_frame(
                output,
                state.start_time.elapsed(),
                Some(Duration::ZERO),
                should_send,
            );
        }
    });

    let layer_map = layer_map_for_output(output);
    layer_map.layers().for_each(|layer| {
        layer.send_frame(output, state.start_time.elapsed(), Some(Duration::ZERO), should_send);
    });
}

fn take_presentation_feedback_for_output(
    state: &Raven,
    output: &Output,
    render_element_states: &smithay::backend::renderer::element::RenderElementStates,
) -> OutputPresentationFeedback {
    let mut output_presentation_feedback = OutputPresentationFeedback::new(output);

    state.space.elements().for_each(|window| {
        if state
            .space
            .outputs_for_element(window)
            .iter()
            .any(|candidate| candidate == output)
        {
            window.take_presentation_feedback(
                &mut output_presentation_feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(surface, render_element_states)
                },
            );
        }
    });

    let layer_map = layer_map_for_output(output);
    for layer in layer_map.layers() {
        layer.take_presentation_feedback(
            &mut output_presentation_feedback,
            surface_primary_scanout_output,
            |surface, _| {
                surface_presentation_feedback_flags_from_states(surface, render_element_states)
            },
        );
    }

    output_presentation_feedback
}

fn update_primary_scanout_output_for_output(
    state: &mut Raven,
    output: &Output,
    render_element_states: &smithay::backend::renderer::element::RenderElementStates,
) {
    state.space.elements().for_each(|window| {
        window.with_surfaces(|surface, states| {
            update_surface_primary_scanout_output(
                surface,
                output,
                states,
                render_element_states,
                default_primary_scanout_output_compare,
            );
        });
    });

    let layer_map = layer_map_for_output(output);
    for layer in layer_map.layers() {
        layer.with_surfaces(|surface, states| {
            update_surface_primary_scanout_output(
                surface,
                output,
                states,
                render_element_states,
                default_primary_scanout_output_compare,
            );
        });
    }
}

fn on_estimated_vblank_timer(state: &mut Raven, node: DrmNode, crtc: crtc::Handle) {
    let (output, frame_callback_sequence) = {
        let udev = state.udev_data.as_mut().unwrap();
        let Some(device) = udev.backends.get_mut(&node) else {
            return;
        };
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };
        surface.frame_callback_sequence = surface.frame_callback_sequence.wrapping_add(1);
        match std::mem::take(&mut surface.redraw_state) {
            RedrawState::WaitingForEstimatedVBlank(_) => {
                surface.redraw_state = RedrawState::Idle;
            }
            RedrawState::WaitingForEstimatedVBlankAndQueued(_) => {
                surface.redraw_state = RedrawState::Queued;
                let handle = state.loop_handle.clone();
                handle.insert_idle(move |state| {
                    render_surface(state, node, crtc);
                });
                return;
            }
            other => {
                surface.redraw_state = other;
                return;
            }
        }
        (surface.output.clone(), surface.frame_callback_sequence)
    };

    send_frame_callbacks_for_output(state, &output, frame_callback_sequence);
}

fn output_refresh_interval(output: &Output) -> Option<Duration> {
    output
        .current_mode()
        .filter(|mode| mode.refresh > 0)
        .map(|mode| Duration::from_secs_f64(1000f64 / mode.refresh as f64))
}

/// Handle VBlank event (frame completion)
fn frame_finish(
    state: &mut Raven,
    node: DrmNode,
    crtc: crtc::Handle,
    metadata: &mut Option<DrmEventMetadata>,
) {
    let throttled = {
        let udev = state.udev_data.as_mut().unwrap();
        let Some(device) = udev.backends.get_mut(&node) else {
            return;
        };
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };

        let refresh_interval = output_refresh_interval(&surface.output);
        let timestamp = metadata.as_ref().and_then(|meta| match meta.time {
            DrmEventTime::Monotonic(ts) if !ts.is_zero() => Some(ts),
            _ => None,
        });
        let sequence = metadata.as_ref().map(|meta| meta.sequence).unwrap_or(0);

        if let Some(timestamp) = timestamp {
            surface
                .vblank_throttle
                .throttle(refresh_interval, timestamp, move |state| {
                    let mut throttled_meta = Some(DrmEventMetadata {
                        sequence,
                        time: DrmEventTime::Monotonic(Duration::ZERO),
                    });
                    frame_finish(state, node, crtc, &mut throttled_meta);
                })
        } else {
            false
        }
    };

    if throttled {
        return;
    }

    let udev = state.udev_data.as_mut().unwrap();
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };
    let Some(surface) = device.surfaces.get_mut(&crtc) else {
        return;
    };

    let output = surface.output.clone();
    let frame_duration = output
        .current_mode()
        .filter(|mode| mode.refresh > 0)
        .map(|mode| Duration::from_secs_f64(1_000f64 / mode.refresh as f64))
        .unwrap_or_else(|| Duration::from_micros(16_667));
    let seq = metadata.as_ref().map(|meta| meta.sequence as u64).unwrap_or(0);
    let tp = metadata.as_ref().and_then(|meta| match meta.time {
        DrmEventTime::Monotonic(tp) if !tp.is_zero() => Some(tp),
        _ => None,
    });
    let (clock, flags) = if let Some(tp) = tp {
        (
            tp.into(),
            wp_presentation_feedback::Kind::Vsync
                | wp_presentation_feedback::Kind::HwClock
                | wp_presentation_feedback::Kind::HwCompletion,
        )
    } else {
        (state.clock.now(), wp_presentation_feedback::Kind::Vsync)
    };

    // Notify that the frame was submitted
    match surface.drm_output.frame_submitted() {
        Ok(user_data) => {
            if let Some(mut output_feedback) = user_data.flatten() {
                output_feedback.presented(clock, Refresh::fixed(frame_duration), seq, flags);
            }
        }
        Err(e) => {
            tracing::error!("frame_submitted error: {e:?}");
            return;
        }
    }
    let redraw_needed = match std::mem::take(&mut surface.redraw_state) {
        RedrawState::WaitingForVBlank { redraw_needed } => redraw_needed,
        other => {
            tracing::warn!("unexpected redraw state at vblank: {other:?}");
            true
        }
    };
    if redraw_needed {
        surface.redraw_state = RedrawState::Queued;
        let handle = state.loop_handle.clone();
        handle.insert_idle(move |state| {
            render_surface(state, node, crtc);
        });
        return;
    }

    surface.redraw_state = RedrawState::Idle;
}

/// Handle session events (TTY switch)
fn handle_session_event(state: &mut Raven, event: SessionEvent, libinput_context: &mut Libinput) {
    match event {
        SessionEvent::PauseSession => {
            tracing::info!("Session paused (TTY switch away)");
            libinput_context.suspend();

            let udev = state.udev_data.as_mut().unwrap();
            for (_node, backend) in udev.backends.iter_mut() {
                backend.drm_output_manager.pause();
            }
        }
        SessionEvent::ActivateSession => {
            tracing::info!("Session activated (TTY switch back)");
            if let Err(e) = libinput_context.resume() {
                tracing::error!("Failed to resume libinput: {e:?}");
            }

            let udev = state.udev_data.as_mut().unwrap();
            let nodes: Vec<DrmNode> = udev.backends.keys().cloned().collect();
            for node in &nodes {
                if let Some(backend) = udev.backends.get_mut(node) {
                    if let Err(e) = backend.drm_output_manager.lock().activate(false) {
                        tracing::error!(?node, "Failed to activate DRM backend: {e}");
                    }
                }
            }

            // Schedule re-render for all outputs
            let handle = state.loop_handle.clone();
            for node in nodes {
                let udev = state.udev_data.as_ref().unwrap();
                if let Some(backend) = udev.backends.get(&node) {
                    let crtcs: Vec<_> = backend.surfaces.keys().cloned().collect();
                    for crtc in crtcs {
                        handle.insert_idle(move |state| {
                            render_surface(state, node, crtc);
                        });
                    }
                }
            }
        }
    }
}
