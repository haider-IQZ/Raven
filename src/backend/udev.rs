use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
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
            ImportAll, ImportDma, ImportMem, ImportMemWl, Renderer, RendererSuper,
            element::{
                AsRenderElements, Element, Id, Kind, RenderElement, UnderlyingStorage,
                default_primary_scanout_output_compare,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
                texture::TextureRenderElement,
                utils::{
                    ConstrainAlign, ConstrainScaleBehavior, CropRenderElement,
                    RelocateRenderElement, RescaleRenderElement, constrain_as_render_elements,
                },
            },
            gles::GlesRenderer,
            multigpu::{GpuManager, MultiRenderer, gbm::GbmGlesBackend},
            utils::{CommitCounter, DamageSet, OpaqueRegions, with_renderer_surface_state},
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    desktop::{
        Window, layer_map_for_output,
        space::{SpaceRenderElements, space_render_elements},
        utils::{
            OutputPresentationFeedback, surface_presentation_feedback_flags_from_states,
            surface_primary_scanout_output, update_surface_primary_scanout_output,
        },
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
    utils::{
        Buffer as BufferCoords, DeviceFd, IsAlive, Physical, Point, Rectangle, Scale, Transform,
    },
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

fn render_trace_line(line: impl AsRef<str>) {
    static TRACE_ENABLED: OnceLock<bool> = OnceLock::new();
    static TRACE_INIT: OnceLock<()> = OnceLock::new();
    static TRACE_LINES: OnceLock<Mutex<usize>> = OnceLock::new();
    const TRACE_PATH: &str = "/tmp/raven-render-trace.log";
    const TRACE_MAX_LINES: usize = 4000;

    if !*TRACE_ENABLED.get_or_init(|| {
        std::env::var_os("RAVEN_RENDER_TRACE")
            .map(|value| {
                let value = value.to_string_lossy().to_ascii_lowercase();
                matches!(value.as_str(), "1" | "true" | "yes" | "on")
            })
            .unwrap_or(false)
    }) {
        return;
    }

    TRACE_INIT.get_or_init(|| {
        let _ = std::fs::write(TRACE_PATH, "");
    });

    let counter = TRACE_LINES.get_or_init(|| Mutex::new(0));
    let Ok(mut count) = counter.lock() else {
        return;
    };
    if *count >= TRACE_MAX_LINES {
        return;
    }
    *count += 1;

    let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(TRACE_PATH)
    else {
        return;
    };
    let _ = writeln!(file, "{}", line.as_ref());
}

fn trace_rect_logical(label: &str, rect: Rectangle<i32, smithay::utils::Logical>) -> String {
    format!(
        "{label}={}x{}@{},{}",
        rect.size.w, rect.size.h, rect.loc.x, rect.loc.y
    )
}

fn trace_rect_physical(label: &str, rect: Rectangle<i32, Physical>) -> String {
    format!(
        "{label}={}x{}@{},{}",
        rect.size.w, rect.size.h, rect.loc.x, rect.loc.y
    )
}

fn trace_src_buffer(label: &str, rect: Rectangle<f64, BufferCoords>) -> String {
    format!(
        "{label}={:.2}x{:.2}@{:.2},{:.2}",
        rect.size.w, rect.size.h, rect.loc.x, rect.loc.y
    )
}

fn scanout_rejection_reason(
    state: &Raven,
    output: &Output,
    fullscreen_on_output: bool,
) -> Option<&'static str> {
    if !fullscreen_on_output {
        return Some("fullscreen-not-active");
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
    Memory=MemoryRenderBufferRenderElement<R>,
}

smithay::backend::renderer::element::render_elements! {
    pub UdevCompositeRenderElement<R, E> where R: ImportAll + ImportMem + Renderer;
    Base=UdevRenderElement<R, E>,
    CorrectedBase=CorrectedWaylandSurfaceRenderElement<R>,
    CorrectedTexture=TextureRenderElement<R::TextureId>,
    ConstrainedWindow=CropRenderElement<RelocateRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<R>>>>,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
struct AssignedWindowRect {
    window: Window,
    surface_id: WlSurface,
    surface_ids: HashSet<Id>,
    is_fullscreen: bool,
    assigned_logical: Rectangle<i32, smithay::utils::Logical>,
    assigned_physical: Rectangle<i32, smithay::utils::Physical>,
    reported_logical_size: smithay::utils::Size<i32, smithay::utils::Logical>,
    reported_physical_size: smithay::utils::Size<i32, smithay::utils::Physical>,
    raw_root_logical: Rectangle<i32, smithay::utils::Logical>,
    raw_root_physical: Rectangle<i32, smithay::utils::Physical>,
    render_origin_logical: Point<i32, smithay::utils::Logical>,
    render_origin_physical: Point<i32, smithay::utils::Physical>,
}

impl AssignedWindowRect {
    fn needs_correction(&self) -> bool {
        self.assigned_logical.loc != self.raw_root_logical.loc
            || self.assigned_logical.size != self.raw_root_logical.size
            || self.assigned_logical.size != self.reported_logical_size
            || self.raw_root_logical.size != self.reported_logical_size
    }
}

#[derive(Debug)]
struct CorrectedWaylandSurfaceRenderElement<R: smithay::backend::renderer::Renderer> {
    inner: WaylandSurfaceRenderElement<R>,
    src_override: Option<Rectangle<f64, BufferCoords>>,
    geometry_override: Option<Rectangle<i32, Physical>>,
}

impl<R> Element for CorrectedWaylandSurfaceRenderElement<R>
where
    R: smithay::backend::renderer::Renderer + ImportAll,
{
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.geometry_override
            .map(|geometry| geometry.loc)
            .unwrap_or_else(|| self.inner.location(scale))
    }

    fn src(&self) -> Rectangle<f64, BufferCoords> {
        self.src_override.unwrap_or_else(|| self.inner.src())
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry_override
            .unwrap_or_else(|| self.inner.geometry(scale))
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        let source_geometry = self.inner.geometry(scale);
        let damage = self.inner.damage_since(scale, commit);
        let Some(target_geometry) = self.geometry_override else {
            if self.src_override.is_some() {
                return std::iter::once(source_geometry).collect();
            }
            return damage;
        };

        if self.src_override.is_some() || source_geometry != target_geometry {
            return [source_geometry, target_geometry].into_iter().collect();
        }

        remap_damage_or_regions(damage, source_geometry, target_geometry, true)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        let opaque_regions = self.inner.opaque_regions(scale);
        if self.src_override.is_some()
            && let Some(target_geometry) = self.geometry_override
        {
            if opaque_regions.is_empty() {
                return opaque_regions;
            }
            return std::iter::once(target_geometry).collect();
        }

        let Some(target_geometry) = self.geometry_override else {
            return opaque_regions;
        };
        let source_geometry = self.inner.geometry(scale);
        remap_damage_or_regions(opaque_regions, source_geometry, target_geometry, false)
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl<R> RenderElement<R> for CorrectedWaylandSurfaceRenderElement<R>
where
    R: smithay::backend::renderer::Renderer + ImportAll,
    R::TextureId: smithay::backend::renderer::Texture + 'static,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), R::Error> {
        self.inner.draw(frame, src, dst, damage, opaque_regions)
    }

    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}

fn remap_damage_or_regions<C>(
    rects: C,
    source_geometry: Rectangle<i32, Physical>,
    target_geometry: Rectangle<i32, Physical>,
    round_up: bool,
) -> C
where
    C: IntoIterator<Item = Rectangle<i32, Physical>> + FromIterator<Rectangle<i32, Physical>>,
{
    if source_geometry.size.w <= 0
        || source_geometry.size.h <= 0
        || source_geometry.size == target_geometry.size
    {
        return rects;
    }

    let scale_x = target_geometry.size.w as f64 / source_geometry.size.w as f64;
    let scale_y = target_geometry.size.h as f64 / source_geometry.size.h as f64;

    rects
        .into_iter()
        .map(|rect| {
            let relative_loc = rect.loc - source_geometry.loc;
            let mapped = Rectangle::new(
                Point::<f64, Physical>::from((
                    target_geometry.loc.x as f64 + relative_loc.x as f64 * scale_x,
                    target_geometry.loc.y as f64 + relative_loc.y as f64 * scale_y,
                )),
                smithay::utils::Size::<f64, Physical>::from((
                    rect.size.w as f64 * scale_x,
                    rect.size.h as f64 * scale_y,
                )),
            );
            if round_up {
                mapped.to_i32_up()
            } else {
                mapped.to_i32_round()
            }
        })
        .collect()
}

fn corrected_surface_src_for_visible_geometry<R>(
    element: &WaylandSurfaceRenderElement<R>,
    visible_geometry: Rectangle<i32, Physical>,
    output_scale: Scale<f64>,
) -> Option<Rectangle<f64, BufferCoords>>
where
    R: smithay::backend::renderer::Renderer + ImportAll,
{
    let element_geometry = element.geometry(output_scale);
    let intersection = element_geometry.intersection(visible_geometry)?;
    if intersection == element_geometry {
        return None;
    }

    let mut element_relative_intersection = intersection;
    element_relative_intersection.loc -= element_geometry.loc;
    let src = element.src();
    let transform = element.transform();
    let physical_to_buffer_scale = src.size
        / transform
            .invert()
            .transform_size(element_geometry.size)
            .to_f64();

    let mut cropped_src = element_relative_intersection
        .to_f64()
        .to_logical(1.0)
        .to_buffer(
            physical_to_buffer_scale,
            transform,
            &element_geometry.size.to_f64().to_logical(1.0),
        );
    cropped_src.loc += src.loc;
    Some(cropped_src)
}

fn corrected_surface_src_for_projection(
    src: Rectangle<f64, BufferCoords>,
    source_bounds: Rectangle<f64, BufferCoords>,
    projected_size: smithay::utils::Size<i32, Physical>,
    expected_size: smithay::utils::Size<i32, Physical>,
) -> Rectangle<f64, BufferCoords> {
    if expected_size.w <= 0 || expected_size.h <= 0 || projected_size == expected_size {
        return src;
    }

    let scale_x = projected_size.w as f64 / expected_size.w as f64;
    let scale_y = projected_size.h as f64 / expected_size.h as f64;
    let scaled_size = smithay::utils::Size::<f64, BufferCoords>::from((
        src.size.w * scale_x,
        src.size.h * scale_y,
    ));
    let delta_w = scaled_size.w - src.size.w;
    let delta_h = scaled_size.h - src.size.h;
    let min_loc = source_bounds.loc;
    let max_loc = Point::<f64, BufferCoords>::from((
        (source_bounds.loc.x + source_bounds.size.w - scaled_size.w).max(source_bounds.loc.x),
        (source_bounds.loc.y + source_bounds.size.h - scaled_size.h).max(source_bounds.loc.y),
    ));
    let centered_loc = Point::<f64, BufferCoords>::from((
        (src.loc.x - delta_w / 2.0).clamp(min_loc.x, max_loc.x),
        (src.loc.y - delta_h / 2.0).clamp(min_loc.y, max_loc.y),
    ));

    Rectangle::new(centered_loc, scaled_size)
}

fn corrected_surface_src_for_projection_logical(
    src: Rectangle<f64, smithay::utils::Logical>,
    source_bounds: Rectangle<f64, smithay::utils::Logical>,
    projected_size: smithay::utils::Size<i32, smithay::utils::Logical>,
    expected_size: smithay::utils::Size<i32, smithay::utils::Logical>,
) -> Rectangle<f64, smithay::utils::Logical> {
    if expected_size.w <= 0 || expected_size.h <= 0 || projected_size == expected_size {
        return src;
    }

    let scale_x = projected_size.w as f64 / expected_size.w as f64;
    let scale_y = projected_size.h as f64 / expected_size.h as f64;
    let scaled_size = smithay::utils::Size::<f64, smithay::utils::Logical>::from((
        src.size.w * scale_x,
        src.size.h * scale_y,
    ));
    let delta_w = scaled_size.w - src.size.w;
    let delta_h = scaled_size.h - src.size.h;
    let min_loc = source_bounds.loc;
    let max_loc = Point::<f64, smithay::utils::Logical>::from((
        (source_bounds.loc.x + source_bounds.size.w - scaled_size.w).max(source_bounds.loc.x),
        (source_bounds.loc.y + source_bounds.size.h - scaled_size.h).max(source_bounds.loc.y),
    ));
    let centered_loc = Point::<f64, smithay::utils::Logical>::from((
        (src.loc.x - delta_w / 2.0).clamp(min_loc.x, max_loc.x),
        (src.loc.y - delta_h / 2.0).clamp(min_loc.y, max_loc.y),
    ));

    Rectangle::new(centered_loc, scaled_size)
}

fn corrected_root_target_rect_logical(
    assignment: &AssignedWindowRect,
) -> Rectangle<i32, smithay::utils::Logical> {
    if assignment.is_fullscreen {
        return assignment.assigned_logical;
    }

    let raw_root_size = assignment.raw_root_logical.size;
    let reported_size = assignment.reported_logical_size;
    if reported_size.w <= 0
        || reported_size.h <= 0
        || raw_root_size.w > reported_size.w - 1
        || raw_root_size.h > reported_size.h - 1
    {
        return assignment.assigned_logical;
    }

    let scale_x = assignment.assigned_logical.size.w as f64 / reported_size.w as f64;
    let scale_y = assignment.assigned_logical.size.h as f64 / reported_size.h as f64;
    let correction = Point::<f64, smithay::utils::Logical>::from((
        ((reported_size.w - raw_root_size.w) as f64 / 2.0).max(0.0) * scale_x,
        ((reported_size.h - raw_root_size.h) as f64 / 2.0).max(0.0) * scale_y,
    ))
    .to_i32_round();
    let size = smithay::utils::Size::<f64, smithay::utils::Logical>::from((
        raw_root_size.w as f64 * scale_x,
        raw_root_size.h as f64 * scale_y,
    ))
    .to_i32_round();

    Rectangle::new(assignment.assigned_logical.loc + correction, size)
}

fn corrected_root_target_rect(assignment: &AssignedWindowRect) -> Rectangle<i32, Physical> {
    let logical_rect = corrected_root_target_rect_logical(assignment);
    Rectangle::new(
        logical_rect
            .loc
            .to_f64()
            .to_physical(output_scale_from_logical_rects(
                assignment.assigned_logical,
                assignment.assigned_physical,
            ))
            .to_i32_round(),
        logical_rect
            .size
            .to_f64()
            .to_physical(output_scale_from_logical_rects(
                assignment.assigned_logical,
                assignment.assigned_physical,
            ))
            .to_i32_round(),
    )
}

fn output_scale_from_logical_rects(
    logical: Rectangle<i32, smithay::utils::Logical>,
    physical: Rectangle<i32, smithay::utils::Physical>,
) -> Scale<f64> {
    let scale_x = if logical.size.w > 0 {
        physical.size.w as f64 / logical.size.w as f64
    } else {
        1.0
    };
    let scale_y = if logical.size.h > 0 {
        physical.size.h as f64 / logical.size.h as f64
    } else {
        1.0
    };
    Scale::from((scale_x, scale_y))
}

fn corrected_root_texture_element<'render, 'frame>(
    renderer: &'render mut UdevRenderer<'frame>,
    assignment: &AssignedWindowRect,
    root_element: &WaylandSurfaceRenderElement<UdevRenderer<'frame>>,
) -> Option<TextureRenderElement<<UdevRenderer<'frame> as RendererSuper>::TextureId>> {
    let target_logical = corrected_root_target_rect_logical(assignment);
    let projection_expected_size =
        if assignment.raw_root_logical.size != assignment.reported_logical_size {
            assignment.raw_root_logical.size
        } else {
            assignment.reported_logical_size
        };
    let root_surface_rect =
        Rectangle::new(assignment.raw_root_logical.loc, root_element.view().dst);
    let visible_rect = root_surface_rect.intersection(Rectangle::new(
        assignment.assigned_logical.loc,
        assignment.reported_logical_size,
    ))?;
    let local_visible_rect =
        Rectangle::new(visible_rect.loc - root_surface_rect.loc, visible_rect.size);
    let view = root_element.view();
    let scale_x = if view.src.size.w > 0.0 {
        view.dst.w as f64 / view.src.size.w
    } else {
        1.0
    };
    let scale_y = if view.src.size.h > 0.0 {
        view.dst.h as f64 / view.src.size.h
    } else {
        1.0
    };
    let visible_src = Rectangle::new(
        Point::<f64, smithay::utils::Logical>::from((
            view.src.loc.x + local_visible_rect.loc.x as f64 / scale_x,
            view.src.loc.y + local_visible_rect.loc.y as f64 / scale_y,
        )),
        smithay::utils::Size::<f64, smithay::utils::Logical>::from((
            local_visible_rect.size.w as f64 / scale_x,
            local_visible_rect.size.h as f64 / scale_y,
        )),
    );
    let corrected_src = corrected_surface_src_for_projection_logical(
        visible_src,
        view.src,
        target_logical.size,
        projection_expected_size,
    );

    with_renderer_surface_state(&assignment.surface_id, |state| {
        let texture = state.texture(renderer.context_id())?.clone();
        let opaque_regions = state.opaque_regions().and_then(|regions| {
            let buffer_size = state.buffer_size()?;
            Some(
                regions
                    .iter()
                    .map(|region| {
                        region.to_buffer(
                            state.buffer_scale(),
                            state.buffer_transform(),
                            &buffer_size,
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        });

        render_trace_line(format!(
            "root_path=custom {} {} {} {} expected={}x{} src={:.2}x{:.2}@{:.2},{:.2} opaque={}",
            trace_rect_logical("assigned", assignment.assigned_logical),
            trace_rect_logical("raw_root", assignment.raw_root_logical),
            trace_rect_logical("target", target_logical),
            trace_rect_physical("assigned_phys", assignment.assigned_physical),
            projection_expected_size.w,
            projection_expected_size.h,
            corrected_src.size.w,
            corrected_src.size.h,
            corrected_src.loc.x,
            corrected_src.loc.y,
            opaque_regions.is_some(),
        ));

        Some(TextureRenderElement::from_texture_with_damage(
            root_element.id().clone(),
            renderer.context_id(),
            target_logical
                .loc
                .to_f64()
                .to_physical(output_scale_from_logical_rects(
                    assignment.assigned_logical,
                    assignment.assigned_physical,
                )),
            texture,
            state.buffer_scale(),
            state.buffer_transform(),
            Some(root_element.alpha()),
            Some(corrected_src),
            Some(target_logical.size),
            opaque_regions,
            state.damage(),
            root_element.kind(),
        ))
    })
    .flatten()
}

fn translate_geometry_between_rects(
    geometry: Rectangle<i32, Physical>,
    source_rect: Rectangle<i32, Physical>,
    target_rect: Rectangle<i32, Physical>,
) -> Rectangle<i32, Physical> {
    let relative_loc = geometry.loc - source_rect.loc;
    Rectangle::new(target_rect.loc + relative_loc, geometry.size)
}

fn window_assignment_for_output(
    state: &Raven,
    output_geo: Rectangle<i32, smithay::utils::Logical>,
    output_scale: Scale<f64>,
    window: &Window,
) -> Option<AssignedWindowRect> {
    let toplevel = window.toplevel()?;
    let surface_id = toplevel.wl_surface().clone();
    let assigned_logical = state.window_visual_or_assigned_rect(window)?;
    let render_origin_logical = assigned_logical.loc - window.geometry().loc;
    let raw_root_size =
        with_renderer_surface_state(&surface_id, |renderer_state| renderer_state.surface_size())
            .flatten()
            .unwrap_or(window.geometry().size);
    let reported_logical_size = state
        .committed_reported_size_for_window(window)
        .unwrap_or(raw_root_size);

    let mut surface_ids = HashSet::new();
    window.with_surfaces(|wl_surface, _| {
        surface_ids.insert(Id::from_wayland_resource(wl_surface));
    });
    if surface_ids.is_empty() {
        return None;
    }

    let assigned_physical = Rectangle::new(
        (assigned_logical.loc - output_geo.loc)
            .to_f64()
            .to_physical(output_scale)
            .to_i32_round(),
        assigned_logical
            .size
            .to_f64()
            .to_physical(output_scale)
            .to_i32_round(),
    );
    let reported_physical_size = reported_logical_size
        .to_f64()
        .to_physical(output_scale)
        .to_i32_round();
    let raw_root_logical = Rectangle::new(render_origin_logical, raw_root_size);
    let raw_root_physical = Rectangle::new(
        (raw_root_logical.loc - output_geo.loc)
            .to_f64()
            .to_physical(output_scale)
            .to_i32_round(),
        raw_root_logical
            .size
            .to_f64()
            .to_physical(output_scale)
            .to_i32_round(),
    );
    let render_origin_physical = raw_root_physical.loc;

    Some(AssignedWindowRect {
        window: window.clone(),
        surface_id,
        surface_ids,
        is_fullscreen: state.window_effective_fullscreen_state(window),
        assigned_logical,
        assigned_physical,
        reported_logical_size,
        reported_physical_size,
        raw_root_logical,
        raw_root_physical,
        render_origin_logical,
        render_origin_physical,
    })
}

fn assigned_window_render_elements<'render, 'frame>(
    renderer: &'render mut UdevRenderer<'frame>,
    assignment: &AssignedWindowRect,
    output_scale: Scale<f64>,
) -> Vec<
    UdevCompositeRenderElement<
        UdevRenderer<'frame>,
        WaylandSurfaceRenderElement<UdevRenderer<'frame>>,
    >,
> {
    let Some(toplevel) = assignment.window.toplevel() else {
        return Vec::new();
    };

    let root_surface = toplevel.wl_surface();
    if !assignment.is_fullscreen {
        return render_elements_from_surface_tree::<
            _,
            WaylandSurfaceRenderElement<UdevRenderer<'frame>>,
        >(
            renderer,
            root_surface,
            assignment.render_origin_physical,
            output_scale,
            1.0,
            Kind::Unspecified,
        )
        .into_iter()
        .map(|element| {
            UdevCompositeRenderElement::from(CorrectedWaylandSurfaceRenderElement {
                inner: element,
                src_override: None,
                geometry_override: None,
            })
        })
        .collect();
    }

    if assignment.is_fullscreen && assignment.needs_correction() {
        return constrain_as_render_elements::<
            UdevRenderer<'frame>,
            Window,
            UdevCompositeRenderElement<
                UdevRenderer<'frame>,
                WaylandSurfaceRenderElement<UdevRenderer<'frame>>,
            >,
        >(
            &assignment.window,
            renderer,
            assignment.render_origin_physical,
            1.0,
            assignment.assigned_physical,
            assignment.raw_root_physical,
            ConstrainScaleBehavior::Stretch,
            ConstrainAlign::TOP_LEFT,
            output_scale,
        )
        .collect();
    }

    if !assignment.needs_correction() {
        render_trace_line(format!(
            "window_path=passthrough {} {} expected={}x{}",
            trace_rect_logical("assigned", assignment.assigned_logical),
            trace_rect_logical("raw_root", assignment.raw_root_logical),
            assignment.reported_logical_size.w,
            assignment.reported_logical_size.h,
        ));
        return render_elements_from_surface_tree::<
            _,
            WaylandSurfaceRenderElement<UdevRenderer<'frame>>,
        >(
            renderer,
            root_surface,
            assignment.render_origin_physical,
            output_scale,
            1.0,
            Kind::Unspecified,
        )
        .into_iter()
        .map(|element| {
            UdevCompositeRenderElement::from(CorrectedWaylandSurfaceRenderElement {
                inner: element,
                src_override: None,
                geometry_override: None,
            })
        })
        .collect();
    }

    let root_id = Id::from_wayland_resource(&assignment.surface_id);
    let reported_physical_rect = Rectangle::new(
        assignment.assigned_physical.loc,
        assignment.reported_physical_size,
    );
    let reported_tree_rect = Rectangle::new(
        assignment.raw_root_physical.loc,
        assignment.reported_physical_size,
    );
    let root_target_rect = corrected_root_target_rect(assignment);
    let projection_expected_size =
        if assignment.raw_root_physical.size != assignment.reported_physical_size {
            assignment.raw_root_physical.size
        } else {
            assignment.reported_physical_size
        };
    render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<UdevRenderer<'frame>>>(
        renderer,
        root_surface,
        assignment.render_origin_physical,
        output_scale,
        1.0,
        Kind::Unspecified,
    )
    .into_iter()
    .map(|element| {
        let is_root_surface = element.id() == &root_id;
        let element_geometry = element.geometry(output_scale);
        let root_src_override = if is_root_surface {
            let base_src = element.src();
            let visible_src = corrected_surface_src_for_visible_geometry(
                &element,
                reported_physical_rect,
                output_scale,
            )
            .unwrap_or(base_src);
            Some(corrected_surface_src_for_projection(
                visible_src,
                base_src,
                root_target_rect.size,
                projection_expected_size,
            ))
        } else {
            None
        };
        if is_root_surface
            && let Some(texture_element) =
                corrected_root_texture_element(renderer, assignment, &element)
        {
            return UdevCompositeRenderElement::from(texture_element);
        }

        let geometry_override = if is_root_surface {
            render_trace_line(format!(
                "root_path=wrapped {} {} {} {} {}",
                trace_rect_logical("assigned", assignment.assigned_logical),
                trace_rect_logical("raw_root", assignment.raw_root_logical),
                trace_rect_physical("target_phys", root_target_rect),
                root_src_override
                    .map(|src| trace_src_buffer("src", src))
                    .unwrap_or_else(|| "src=none".to_string()),
                trace_rect_physical("elem_phys", element_geometry),
            ));
            Some(root_target_rect)
        } else if reported_tree_rect.size != root_target_rect.size {
            let translated_geometry = translate_geometry_between_rects(
                element_geometry,
                assignment.raw_root_physical,
                root_target_rect,
            );
            render_trace_line(format!(
                "child_path=translated {} {} {}",
                trace_rect_physical("source_elem", element_geometry),
                trace_rect_physical("raw_root", assignment.raw_root_physical),
                trace_rect_physical("target_elem", translated_geometry),
            ));
            Some(translated_geometry)
        } else {
            None
        };
        UdevCompositeRenderElement::from(CorrectedWaylandSurfaceRenderElement {
            inner: element,
            src_override: root_src_override,
            geometry_override,
        })
    })
    .collect()
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
    WaitingForVBlank {
        redraw_needed: bool,
    },
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
            // Ensure queued redraws always produce at least one damaged region.
            // Without this, remapped workspaces can occasionally hit an empty-damage frame
            // and stay visually stale until a later input/commit triggers new damage.
            surface.backdrop.touch();
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
                // Force damage for explicit output redraw requests too.
                surface.backdrop.touch();
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
            backdrop: SolidColorBuffer::new(
                (wl_mode.size.w as f64, wl_mode.size.h as f64),
                CLEAR_COLOR,
            ),
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

    if fullscreen_requested_on_output {
        if !scanout_enabled() {
            state.record_scanout_rejection(&output, "scanout-disabled");
        } else if let Some(reason) =
            scanout_rejection_reason(state, &output, fullscreen_requested_on_output)
        {
            state.record_scanout_rejection(&output, reason);
        }
    }

    let output_scale = Scale::from(output.current_scale().fractional_scale());
    let output_geo = state.space.output_geometry(&output);
    let window_assignments: Vec<AssignedWindowRect> = output_geo
        .map(|output_geo| {
            state
                .space
                .elements_for_output(&output)
                .filter_map(|window| {
                    window_assignment_for_output(state, output_geo, output_scale, window)
                })
                .collect()
        })
        .unwrap_or_default();
    let mut window_assignment_indices = HashMap::new();
    for (index, assignment) in window_assignments.iter().enumerate() {
        for id in &assignment.surface_ids {
            window_assignment_indices.insert(id.clone(), index);
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
    // Fullscreen ownership only affects ordering. Every mapped window is rendered through the
    // same compositor-assigned rect pipeline below, regardless of whether it is tiled, floating,
    // fullscreen, or in a fullscreen visual handoff.
    let mut space_elements =
        match space_render_elements(&mut renderer, [&state.space], &output, 1.0) {
            Ok(elements) => elements,
            Err(e) => {
                tracing::warn!("Failed to collect render elements: {e:?}");
                Vec::new()
            }
        };
    if fullscreen_requested_on_output {
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
    let space_elements_converted: Vec<
        UdevCompositeRenderElement<UdevRenderer<'_>, WaylandSurfaceRenderElement<UdevRenderer<'_>>>,
    > = {
        let mut converted: Vec<
            UdevCompositeRenderElement<
                UdevRenderer<'_>,
                WaylandSurfaceRenderElement<UdevRenderer<'_>>,
            >,
        > = Vec::new();
        let mut rendered_assigned_windows = HashSet::new();

        for element in space_elements {
            let is_window_element = matches!(element, SpaceRenderElements::Element(_));
            let base = UdevRenderElement::from(element);

            if let Some(assignment_index) = window_assignment_indices.get(base.id()).copied()
                && rendered_assigned_windows.contains(&assignment_index)
            {
                continue;
            }

            if is_window_element
                && let Some(assignment_index) = window_assignment_indices.get(base.id()).copied()
            {
                let assignment = &window_assignments[assignment_index];
                if !assignment.is_fullscreen {
                    converted.push(UdevCompositeRenderElement::from(base));
                    continue;
                }
                if rendered_assigned_windows.insert(assignment_index) {
                    converted.retain(|element| !assignment.surface_ids.contains(element.id()));
                    converted.extend(assigned_window_render_elements(
                        &mut renderer,
                        assignment,
                        output_scale,
                    ));
                }
                continue;
            }

            converted.push(UdevCompositeRenderElement::from(base));
        }

        converted
    };

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

        let pointer_elements: Vec<PointerRenderElement<UdevRenderer<'_>>> = pointer_element
            .render_elements(
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
        SolidColorRenderElement::from_buffer(
            &surface_data.backdrop,
            (0.0, 0.0),
            1.0,
            Kind::Unspecified,
        ),
    )));

    // Render frame with collected elements
    let render_result =
        surface_data
            .drm_output
            .render_frame(&mut renderer, &elements, CLEAR_COLOR, frame_flags());

    match render_result {
        Ok(result) => {
            if result.needs_sync()
                && let smithay::backend::drm::compositor::PrimaryPlaneElement::Swapchain(
                    ref element,
                ) = result.primary_element
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
            let _ = surface_data;
            let _ = device;
            let _ = udev;
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
        layer.send_frame(
            output,
            state.start_time.elapsed(),
            Some(Duration::ZERO),
            should_send,
        );
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
    let seq = metadata
        .as_ref()
        .map(|meta| meta.sequence as u64)
        .unwrap_or(0);
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
