//! Solid color buffer and render element for backdrop rendering.
//!
//! This uses a SolidColorBuffer as a backdrop element instead of relying on
//! framebuffer clearing. This avoids damage tracking issues and prevents black
//! flickers.

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, OpaqueRegions};
use smithay::backend::renderer::{Color32F, Frame as _, Renderer};
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size};

/// A buffer that renders as a solid color.
///
/// Used as a backdrop element to avoid damage tracking issues with framebuffer clearing.
#[derive(Debug, Clone)]
pub struct SolidColorBuffer {
    id: Id,
    size: Size<f64, Logical>,
    commit: CommitCounter,
    color: Color32F,
}

impl Default for SolidColorBuffer {
    fn default() -> Self {
        Self {
            id: Id::new(),
            size: Default::default(),
            commit: Default::default(),
            color: Default::default(),
        }
    }
}

impl SolidColorBuffer {
    /// Create a new solid color buffer with the given size and color.
    pub fn new(size: impl Into<Size<f64, Logical>>, color: impl Into<Color32F>) -> Self {
        SolidColorBuffer {
            id: Id::new(),
            color: color.into(),
            commit: CommitCounter::default(),
            size: size.into(),
        }
    }

    /// Resize the buffer.
    pub fn resize(&mut self, size: impl Into<Size<f64, Logical>>) {
        let size = size.into();
        if size != self.size {
            self.size = size;
            self.commit.increment();
        }
    }

    /// Set the color of the buffer.
    pub fn set_color(&mut self, color: impl Into<Color32F>) {
        let color = color.into();
        if color != self.color {
            self.color = color;
            self.commit.increment();
        }
    }

    /// Update both size and color.
    pub fn update(&mut self, size: impl Into<Size<f64, Logical>>, color: impl Into<Color32F>) {
        let size = size.into();
        let color = color.into();
        if size != self.size || color != self.color {
            self.size = size;
            self.color = color;
            self.commit.increment();
        }
    }

    /// Force a new commit without changing visible properties.
    pub fn touch(&mut self) {
        self.commit.increment();
    }

    /// Get the current color.
    pub fn color(&self) -> Color32F {
        self.color
    }

    /// Get the current size.
    pub fn size(&self) -> Size<f64, Logical> {
        self.size
    }
}

/// A render element for a solid color buffer.
#[derive(Debug, Clone)]
pub struct SolidColorRenderElement {
    id: Id,
    geometry: Rectangle<f64, Logical>,
    commit: CommitCounter,
    color: Color32F,
    kind: Kind,
}

impl SolidColorRenderElement {
    /// Create a render element from a buffer at the given location.
    pub fn from_buffer(
        buffer: &SolidColorBuffer,
        location: impl Into<Point<f64, Logical>>,
        alpha: f32,
        kind: Kind,
    ) -> Self {
        let geo = Rectangle::new(location.into(), buffer.size());
        let color = buffer.color * alpha;
        Self::new(buffer.id.clone(), geo, buffer.commit, color, kind)
    }

    /// Create a new solid color render element.
    pub fn new(
        id: Id,
        geometry: Rectangle<f64, Logical>,
        commit: CommitCounter,
        color: Color32F,
        kind: Kind,
    ) -> Self {
        SolidColorRenderElement {
            id,
            geometry,
            commit,
            color,
            kind,
        }
    }

    /// Get the color of this element.
    pub fn color(&self) -> Color32F {
        self.color
    }

    /// Get the geometry of this element.
    pub fn geo(&self) -> Rectangle<f64, Logical> {
        self.geometry
    }
}

impl Element for SolidColorRenderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(Size::from((1., 1.)))
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry.to_physical_precise_round(scale)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        if self.color.is_opaque() {
            let rect = Rectangle::from_size(self.geometry.size).to_physical_precise_down(scale);
            OpaqueRegions::from_slice(&[rect])
        } else {
            OpaqueRegions::default()
        }
    }

    fn alpha(&self) -> f32 {
        self.color.a()
    }

    fn kind(&self) -> Kind {
        self.kind
    }
}

impl<R: Renderer> RenderElement<R> for SolidColorRenderElement {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), R::Error> {
        frame.draw_solid(dst, damage, self.color)
    }

    #[inline]
    fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
