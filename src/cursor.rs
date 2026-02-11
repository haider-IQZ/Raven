use std::{fs::File, io::Read, time::Duration};

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer, Texture,
        element::{
            AsRenderElements, Kind,
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
        },
    },
    input::pointer::CursorImageStatus,
    render_elements,
    utils::{Physical, Point, Scale},
};
use tracing::warn;
use xcursor::{
    CursorTheme,
    parser::{Image, parse_xcursor},
};

pub struct PointerElement {
    buffer: Option<MemoryRenderBuffer>,
    status: CursorImageStatus,
}

impl Default for PointerElement {
    fn default() -> Self {
        Self {
            buffer: None,
            status: CursorImageStatus::default_named(),
        }
    }
}

impl PointerElement {
    pub fn set_status(&mut self, status: CursorImageStatus) {
        self.status = status;
    }

    pub fn set_buffer(&mut self, buffer: MemoryRenderBuffer) {
        self.buffer = Some(buffer);
    }
}

render_elements! {
    pub PointerRenderElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Memory=MemoryRenderBufferRenderElement<R>,
}

impl<T, R> AsRenderElements<R> for PointerElement
where
    T: Texture + Clone + Send + 'static,
    R: Renderer<TextureId = T> + ImportAll + ImportMem,
{
    type RenderElement = PointerRenderElement<R>;

    fn render_elements<E>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<E>
    where
        E: From<PointerRenderElement<R>>,
    {
        match &self.status {
            CursorImageStatus::Hidden => Vec::new(),
            CursorImageStatus::Named(_) => {
                if let Some(buffer) = self.buffer.as_ref() {
                    MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        location.to_f64(),
                        buffer,
                        None,
                        None,
                        None,
                        Kind::Cursor,
                    )
                    .map(|elem| vec![PointerRenderElement::<R>::from(elem).into()])
                    .unwrap_or_default()
                } else {
                    Vec::new()
                }
            }
            CursorImageStatus::Surface(surface) => {
                let elements: Vec<PointerRenderElement<R>> = render_elements_from_surface_tree(
                    renderer,
                    surface,
                    location,
                    scale,
                    alpha,
                    Kind::Cursor,
                );
                elements.into_iter().map(E::from).collect()
            }
        }
    }
}

pub struct CursorThemeManager {
    icons: Vec<Image>,
    size: u32,
}

impl CursorThemeManager {
    pub fn load() -> Self {
        let name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".to_owned());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(24);

        let theme = CursorTheme::load(&name);
        let icons = load_default_cursor(&theme).unwrap_or_else(|err| {
            warn!("Unable to load xcursor theme ({err}), using fallback cursor");
            vec![fallback_cursor_image()]
        });

        Self { icons, size }
    }

    pub fn image(&self, scale: u32, time: Duration) -> Image {
        frame(
            time.as_millis() as u32,
            self.size.saturating_mul(scale),
            &self.icons,
        )
    }
}

fn load_default_cursor(theme: &CursorTheme) -> Result<Vec<Image>, String> {
    let path = theme
        .load_icon("default")
        .ok_or_else(|| "theme has no `default` cursor".to_owned())?;

    let mut file = File::open(path).map_err(|err| format!("failed to open cursor file: {err}"))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|err| format!("failed to read cursor file: {err}"))?;

    parse_xcursor(&data).ok_or_else(|| "failed to parse cursor data".to_owned())
}

fn nearest_images(size: u32, images: &[Image]) -> impl Iterator<Item = &Image> {
    let nearest = images
        .iter()
        .min_by_key(|image| (size as i32 - image.size as i32).abs())
        .expect("cursor image list is never empty");

    images
        .iter()
        .filter(move |image| image.width == nearest.width && image.height == nearest.height)
}

fn frame(mut millis: u32, size: u32, images: &[Image]) -> Image {
    let total_delay = nearest_images(size, images).fold(0, |acc, image| acc + image.delay);

    if total_delay == 0 {
        return nearest_images(size, images)
            .next()
            .expect("cursor image list is never empty")
            .clone();
    }

    millis %= total_delay;

    for image in nearest_images(size, images) {
        if millis < image.delay {
            return image.clone();
        }
        millis -= image.delay;
    }

    unreachable!("cursor frame selection should always return");
}

fn fallback_cursor_image() -> Image {
    const W: usize = 24;
    const H: usize = 24;

    let mut mask = vec![false; W * H];
    let idx = |x: usize, y: usize| y * W + x;

    for y in 0..16 {
        let right = (y / 2) + 1;
        for x in 0..=right {
            mask[idx(x, y)] = true;
        }
    }

    for y in 10..23 {
        for x in 4..=8 {
            mask[idx(x, y)] = true;
        }
    }

    let mut outline = vec![false; W * H];
    for y in 0..H {
        for x in 0..W {
            if !mask[idx(x, y)] {
                continue;
            }
            for oy in -1isize..=1 {
                for ox in -1isize..=1 {
                    if ox == 0 && oy == 0 {
                        continue;
                    }
                    let nx = x as isize + ox;
                    let ny = y as isize + oy;
                    if nx < 0 || ny < 0 || nx >= W as isize || ny >= H as isize {
                        continue;
                    }
                    let nidx = idx(nx as usize, ny as usize);
                    if !mask[nidx] {
                        outline[nidx] = true;
                    }
                }
            }
        }
    }

    let mut pixels = vec![0u8; W * H * 4];
    for y in 0..H {
        for x in 0..W {
            let i = idx(x, y);
            let rgba = if mask[i] {
                [0, 0, 0, 255]
            } else if outline[i] {
                [255, 255, 255, 255]
            } else {
                [0, 0, 0, 0]
            };

            let p = i * 4;
            pixels[p..p + 4].copy_from_slice(&rgba);
        }
    }

    Image {
        size: W as u32,
        width: W as u32,
        height: H as u32,
        xhot: 1,
        yhot: 1,
        delay: 1,
        pixels_rgba: pixels,
        pixels_argb: Vec::new(),
    }
}
