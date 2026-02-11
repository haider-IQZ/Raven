use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use smithay::{
    backend::{
        allocator::{
            Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode, NodeType,
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLDevice, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            ImportAll, ImportDma, ImportMem, ImportMemWl,
            element::{
                AsRenderElements, memory::MemoryRenderBuffer, surface::WaylandSurfaceRenderElement,
            },
            gles::GlesRenderer,
            multigpu::{GpuManager, MultiRenderer, gbm::GbmGlesBackend},
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    desktop::{
        layer_map_for_output,
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
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, IsAlive, Scale, Transform},
    wayland::{
        compositor,
        dmabuf::{DmabufFeedbackBuilder, DmabufState},
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
};

// Supported color formats for DRM output
const SUPPORTED_FORMATS: &[Fourcc] = &[Fourcc::Abgr8888, Fourcc::Argb8888];

// Background clear color (same as winit backend)
const CLEAR_COLOR: [f32; 4] = [150.0 / 255.0, 154.0 / 255.0, 171.0 / 255.0, 1.0];

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
    Space=SpaceRenderElements<R, E>,
    Pointer=PointerRenderElement<R>,
}

/// Per-GPU device state
struct BackendData {
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    drm_output_manager: DrmOutputManager<GbmAllocator<DrmDeviceFd>, GbmFbExporter, (), DrmDeviceFd>,
    drm_scanner: DrmScanner,
    render_node: Option<DrmNode>,
    registration_token: RegistrationToken,
}

/// Per-CRTC/output state
struct SurfaceData {
    output: Output,
    global: Option<GlobalId>,
    drm_output: DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFbExporter, (), DrmDeviceFd>,
}

impl Drop for SurfaceData {
    fn drop(&mut self) {
        self.output.leave_all();
    }
}

/// DRM/udev backend data stored alongside the compositor state
pub struct UdevData {
    pub session: LibSeatSession,
    pub primary_gpu: DrmNode,
    pub gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    cursor_theme: CursorThemeManager,
    pointer_images: Vec<(xcursor::parser::Image, MemoryRenderBuffer)>,
    backends: HashMap<DrmNode, BackendData>,
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
    let udev = state.udev_data.as_mut().unwrap();
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };
    let Some(surface_data) = device.surfaces.get_mut(&crtc) else {
        return;
    };

    let output = surface_data.output.clone();
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

    // Collect render elements from the space (windows + layer surfaces)
    let space_elements: Vec<
        UdevRenderElement<UdevRenderer<'_>, WaylandSurfaceRenderElement<UdevRenderer<'_>>>,
    > = match space_render_elements(&mut renderer, [&state.space], &output, 1.0) {
        Ok(elements) => elements,
        Err(e) => {
            tracing::warn!("Failed to collect render elements: {e:?}");
            Vec::new()
        }
    }
    .into_iter()
    .map(UdevRenderElement::from)
    .collect();

    // Render order is front-to-back, so cursor elements must come first.
    let mut elements: Vec<
        UdevRenderElement<UdevRenderer<'_>, WaylandSurfaceRenderElement<UdevRenderer<'_>>>,
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

        elements.extend(
            pointer_element.render_elements(
                &mut renderer,
                (cursor_pos - cursor_hotspot.to_f64())
                    .to_physical(scale)
                    .to_i32_round(),
                scale,
                1.0,
            ),
        );
    }

    elements.extend(space_elements);

    // Render frame with collected elements
    let render_result = surface_data.drm_output.render_frame(
        &mut renderer,
        &elements,
        CLEAR_COLOR,
        FrameFlags::DEFAULT,
    );

    match render_result {
        Ok(result) => {
            let rendered = !result.is_empty;

            if rendered {
                if let Err(e) = surface_data.drm_output.queue_frame(()) {
                    tracing::error!("Failed to queue frame: {e:?}");
                }
            }

            // Send frame callbacks to all windows on this output
            state.space.elements().for_each(|window| {
                window.send_frame(
                    &output,
                    state.start_time.elapsed(),
                    Some(Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            });
            let layer_map = layer_map_for_output(&output);
            layer_map.layers().for_each(|layer| {
                layer.send_frame(
                    &output,
                    state.start_time.elapsed(),
                    Some(Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            });

            state.space.refresh();
            state.display_handle.flush_clients().unwrap();

            // If nothing was rendered (no damage), reschedule to avoid missing frames
            if !rendered {
                let refresh_rate = output
                    .current_mode()
                    .map(|mode| mode.refresh as u64)
                    .unwrap_or(60_000);
                let frame_duration = Duration::from_micros(1_000_000u64 / (refresh_rate / 1_000));
                let timer = Timer::from_duration(frame_duration);
                let handle = state.loop_handle.clone();
                handle
                    .insert_source(timer, move |_, _, state| {
                        render_surface(state, node, crtc);
                        TimeoutAction::Drop
                    })
                    .ok();
            }
        }
        Err(e) => {
            tracing::error!("Failed to render frame: {e:?}");
        }
    }
}

/// Handle VBlank event (frame completion)
fn frame_finish(
    state: &mut Raven,
    node: DrmNode,
    crtc: crtc::Handle,
    _metadata: &mut Option<DrmEventMetadata>,
) {
    let udev = state.udev_data.as_mut().unwrap();
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };
    let Some(surface) = device.surfaces.get_mut(&crtc) else {
        return;
    };

    // Notify that the frame was submitted
    match surface.drm_output.frame_submitted() {
        Ok(_) => {}
        Err(e) => {
            tracing::error!("frame_submitted error: {e:?}");
            return;
        }
    }

    // Schedule next render after a short delay (60% of frame time for latency optimization)
    let output = surface.output.clone();
    let refresh_rate = output
        .current_mode()
        .map(|mode| mode.refresh as u64)
        .unwrap_or(60_000);
    let frame_duration_us = 1_000_000u64 / (refresh_rate / 1_000);
    let repaint_delay = Duration::from_micros((frame_duration_us as f64 * 0.6) as u64);

    let timer = Timer::from_duration(repaint_delay);
    let handle = state.loop_handle.clone();
    handle
        .insert_source(timer, move |_, _, state| {
            render_surface(state, node, crtc);
            TimeoutAction::Drop
        })
        .ok();
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
