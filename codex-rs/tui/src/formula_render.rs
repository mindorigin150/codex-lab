use resvg::tiny_skia;
use resvg::usvg;
use rquickjs::Context;
use rquickjs::Function;
use rquickjs::Runtime;
use rquickjs::function::Func;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::formula_parser::FormulaKind;

pub(crate) const RASTER_SCALE: u32 = 2;
pub(crate) const MAX_FORMULA_SOURCE_BYTES: usize = 16 * 1024;
pub(crate) const MAX_RASTER_PIXELS: u64 = 32 * 1024 * 1024;
const QUICKJS_MEMORY_LIMIT: usize = 128 * 1024 * 1024;
const QUICKJS_STACK_LIMIT: usize = 1024 * 1024;
const FORMULA_FONT_CELL_HEIGHT_RATIO: f32 = 0.875;
const FORMULA_STROKE_WIDTH: u16 = 32;
pub(crate) const FORMULA_RENDER_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub(crate) struct FormulaRaster {
    pub png: Arc<[u8]>,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct FormulaRasterTarget {
    pub columns: u16,
    pub rows: u16,
    pub cell_pixel_width: u16,
    pub cell_pixel_height: u16,
    pub foreground_rgb: [u8; 3],
}

#[derive(Clone, Debug)]
pub(crate) struct FormulaLayoutRaster {
    pub(crate) raster: FormulaRaster,
    pub(crate) columns: u16,
    pub(crate) rows: u16,
    pub(crate) is_block: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FormulaLayoutTarget {
    pub(crate) max_columns: u16,
    pub(crate) cell_pixel_width: u16,
    pub(crate) cell_pixel_height: u16,
    pub(crate) foreground_rgb: [u8; 3],
    pub(crate) render_timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FormulaRenderError {
    #[error("formula source exceeds the 16 KiB rendering boundary")]
    SourceTooLarge,
    #[error("formula raster exceeds the pixel resource boundary")]
    RasterTooLarge,
    #[error("MathJax rendering failed: {0}")]
    MathJax(String),
    #[error("rendered SVG is malformed: {0}")]
    Svg(String),
    #[error("PNG encoding failed: {0}")]
    Png(String),
}

/// Synchronous MathJax -> SVG -> transparent PNG renderer.
///
/// The embedded QuickJS context owns the loaded MathJax bundle, so one renderer is moved to the
/// TUI's serial formula-render thread and reused there.
pub(crate) struct FormulaRenderer {
    _runtime: Runtime,
    context: Context,
    render_deadline: Rc<Cell<Option<Instant>>>,
}

impl FormulaRenderer {
    pub(crate) fn new() -> Result<Self, FormulaRenderError> {
        let runtime =
            Runtime::new().map_err(|error| FormulaRenderError::MathJax(error.to_string()))?;
        runtime.set_memory_limit(QUICKJS_MEMORY_LIMIT);
        runtime.set_max_stack_size(QUICKJS_STACK_LIMIT);
        let render_deadline = Rc::new(Cell::new(None));
        let interrupt_deadline = render_deadline.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            interrupt_deadline
                .get()
                .is_some_and(|deadline| Instant::now() >= deadline)
        })));
        let context = Context::full(&runtime)
            .map_err(|error| FormulaRenderError::MathJax(error.to_string()))?;
        context
            .with(|ctx| {
                ctx.globals()
                    .set("__host_log", Func::from(|_level: u32, _message: String| {}))?;
                ctx.eval::<(), _>(include_str!("../assets/mathjax/index.js"))?;
                ctx.eval::<(), _>("v_.options.linebreaks.inline = false")
            })
            .map_err(|error| FormulaRenderError::MathJax(error.to_string()))?;
        Ok(Self {
            _runtime: runtime,
            context,
            render_deadline,
        })
    }

    #[cfg(test)]
    pub(crate) fn render(
        &self,
        tex: &str,
        target: FormulaRasterTarget,
    ) -> Result<FormulaRaster, FormulaRenderError> {
        if tex.len() > MAX_FORMULA_SOURCE_BYTES {
            return Err(FormulaRenderError::SourceTooLarge);
        }

        let pixel_width = u64::from(target.columns)
            * u64::from(target.cell_pixel_width)
            * u64::from(RASTER_SCALE);
        let pixel_height =
            u64::from(target.rows) * u64::from(target.cell_pixel_height) * u64::from(RASTER_SCALE);
        if pixel_width > u64::from(u32::MAX)
            || pixel_height > u64::from(u32::MAX)
            || pixel_width * pixel_height > MAX_RASTER_PIXELS
        {
            return Err(FormulaRenderError::RasterTooLarge);
        }
        let pixel_width = pixel_width as u32;
        let pixel_height = pixel_height as u32;

        let font_size = formula_font_size(target.cell_pixel_height);
        let tree = self.render_tree(
            tex,
            FormulaKind::Display,
            target.foreground_rgb,
            font_size,
            FORMULA_RENDER_TIMEOUT,
        )?;
        rasterize_tree(tree, pixel_width, pixel_height)
    }

    pub(crate) fn render_for_layout(
        &self,
        tex: &str,
        kind: FormulaKind,
        target: FormulaLayoutTarget,
    ) -> Result<FormulaLayoutRaster, FormulaRenderError> {
        if tex.len() > MAX_FORMULA_SOURCE_BYTES {
            return Err(FormulaRenderError::SourceTooLarge);
        }

        let font_size = formula_font_size(target.cell_pixel_height);
        let tree = self.render_tree(
            tex,
            kind,
            target.foreground_rgb,
            font_size,
            target.render_timeout,
        )?;
        let source = tree.size();
        let natural_columns = (source.width() / f32::from(target.cell_pixel_width))
            .ceil()
            .max(1.0) as u16;
        let natural_rows = (source.height() / f32::from(target.cell_pixel_height))
            .ceil()
            .max(1.0) as u16;
        let is_block = kind == FormulaKind::Display
            || natural_rows > 1
            || natural_columns > target.max_columns;
        let (columns, rows) = if is_block {
            let columns = natural_columns.min(target.max_columns).max(1);
            let rows_for_width =
                (f32::from(columns) * f32::from(target.cell_pixel_width) * source.height()
                    / source.width()
                    / f32::from(target.cell_pixel_height))
                .ceil()
                .max(1.0) as u16;
            (columns, rows_for_width.min(12))
        } else {
            (natural_columns.max(1), 1)
        };
        let pixel_width = u32::from(columns) * u32::from(target.cell_pixel_width) * RASTER_SCALE;
        let pixel_height = u32::from(rows) * u32::from(target.cell_pixel_height) * RASTER_SCALE;
        if u64::from(pixel_width) * u64::from(pixel_height) > MAX_RASTER_PIXELS {
            return Err(FormulaRenderError::RasterTooLarge);
        }
        let raster = rasterize_tree(tree, pixel_width, pixel_height)?;
        Ok(FormulaLayoutRaster {
            raster,
            columns,
            rows,
            is_block,
        })
    }

    fn render_tree(
        &self,
        tex: &str,
        kind: FormulaKind,
        foreground_rgb: [u8; 3],
        font_size: f32,
        render_timeout: Duration,
    ) -> Result<usvg::Tree, FormulaRenderError> {
        self.render_deadline
            .set(Some(Instant::now() + render_timeout));
        let render_result = self.context.with(|ctx| {
            let render: Function = ctx.globals().get("__entry_renderTeX")?;
            render.call::<_, String>((
                tex,
                f64::from(font_size),
                1_i32,
                kind == FormulaKind::Display,
            ))
        });
        self.render_deadline.set(None);
        let svg = render_result.map_err(|error| FormulaRenderError::MathJax(error.to_string()))?;
        parse_svg_tree(&svg, foreground_rgb, font_size)
    }
}

fn formula_font_size(cell_pixel_height: u16) -> f32 {
    f32::from(cell_pixel_height) * FORMULA_FONT_CELL_HEIGHT_RATIO
}

fn parse_svg_tree(
    svg: &str,
    foreground_rgb: [u8; 3],
    font_size: f32,
) -> Result<usvg::Tree, FormulaRenderError> {
    let svg = strip_data_latex_attributes(svg);
    let options = usvg::Options {
        font_size,
        style_sheet: Some(format!(
            "svg {{ color: rgb({}, {}, {}); }} svg > g {{ stroke-width: {}; stroke-linejoin: round; }}",
            foreground_rgb[0], foreground_rgb[1], foreground_rgb[2], FORMULA_STROKE_WIDTH,
        )),
        ..usvg::Options::default()
    };
    usvg::Tree::from_str(&svg, &options).map_err(|error| FormulaRenderError::Svg(error.to_string()))
}

fn rasterize_tree(
    tree: usvg::Tree,
    pixel_width: u32,
    pixel_height: u32,
) -> Result<FormulaRaster, FormulaRenderError> {
    let mut pixmap = tiny_skia::Pixmap::new(pixel_width, pixel_height)
        .ok_or(FormulaRenderError::RasterTooLarge)?;

    let source = tree.size();
    let scale = (pixel_width as f32 / source.width()).min(pixel_height as f32 / source.height());
    let x = (pixel_width as f32 - source.width() * scale) / 2.0;
    let y = (pixel_height as f32 - source.height() * scale) / 2.0;
    let transform = tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, x, y);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let png = pixmap
        .encode_png()
        .map_err(|error| FormulaRenderError::Png(error.to_string()))?;
    Ok(FormulaRaster {
        png: Arc::from(png),
        pixel_width,
        pixel_height,
    })
}

/// Removes MathJax's HTML-only source attributes and inline-only root CSS before XML parsing.
///
/// MathJax escapes quotes in this attribute but can leave `<` literal, making otherwise valid SVG
/// invalid XML. This small start-tag tokenizer removes only the exact quoted attribute and leaves
/// text nodes and similarly named attributes untouched.
fn strip_data_latex_attributes(svg: &str) -> String {
    const ATTRIBUTES: [&[u8]; 3] = [b"data-latex", b"data-latex-item", b"style"];
    let bytes = svg.as_bytes();
    let root_tag_end = bytes
        .iter()
        .position(|byte| *byte == b'>')
        .unwrap_or(bytes.len());
    let mut output = String::with_capacity(svg.len());
    let mut copied_until = 0;
    let mut cursor = 0;
    let mut in_tag = false;

    while cursor < bytes.len() {
        match bytes[cursor] {
            b'<' => {
                in_tag = true;
                cursor += 1;
            }
            b'>' if in_tag => {
                in_tag = false;
                cursor += 1;
            }
            byte if in_tag && is_xml_space(byte) => {
                let whitespace_start = cursor;
                while cursor < bytes.len() && is_xml_space(bytes[cursor]) {
                    cursor += 1;
                }
                let attribute = ATTRIBUTES.into_iter().find(|attribute| {
                    (*attribute != b"style" || cursor < root_tag_end)
                        && bytes[cursor..].starts_with(attribute)
                        && !bytes
                            .get(cursor + attribute.len())
                            .is_some_and(|byte| is_name_byte(*byte))
                });
                if let Some(attribute) = attribute {
                    let mut after_name = cursor + attribute.len();
                    while bytes
                        .get(after_name)
                        .is_some_and(|byte| is_xml_space(*byte))
                    {
                        after_name += 1;
                    }
                    if bytes.get(after_name) != Some(&b'=') {
                        cursor = after_name;
                        continue;
                    }
                    after_name += 1;
                    while bytes
                        .get(after_name)
                        .is_some_and(|byte| is_xml_space(*byte))
                    {
                        after_name += 1;
                    }
                    let Some(quote @ (b'\'' | b'"')) = bytes.get(after_name).copied() else {
                        cursor = after_name;
                        continue;
                    };
                    let value_start = after_name + 1;
                    let Some(relative_end) =
                        bytes[value_start..].iter().position(|byte| *byte == quote)
                    else {
                        cursor = value_start;
                        continue;
                    };
                    let attribute_end = value_start + relative_end + 1;
                    output.push_str(&svg[copied_until..whitespace_start]);
                    copied_until = attribute_end;
                    cursor = attribute_end;
                }
            }
            _ => cursor += 1,
        }
    }
    output.push_str(&svg[copied_until..]);
    output
}

fn is_xml_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_rgba(raster: &FormulaRaster) -> image::RgbaImage {
        image::load_from_memory(&raster.png).unwrap().to_rgba8()
    }

    fn foreground_bounds(image: &image::RgbaImage) -> (u32, u32, u32, u32) {
        let mut left = image.width();
        let mut top = image.height();
        let mut right = 0;
        let mut bottom = 0;
        for (x, y, pixel) in image.enumerate_pixels() {
            if pixel.0[3] != 0 {
                left = left.min(x);
                top = top.min(y);
                right = right.max(x + 1);
                bottom = bottom.max(y + 1);
            }
        }
        (left, top, right, bottom)
    }

    fn alpha_mass(image: &image::RgbaImage) -> u64 {
        image.pixels().map(|pixel| u64::from(pixel.0[3])).sum()
    }

    #[test]
    fn strips_only_exact_data_latex_attribute() {
        let svg = r#"<svg style="vertical-align:-1ex" data-latex="x<y" data-latex-extra="keep"><text style="fill:red">data-latex="also keep"</text><g data-latex='z' data-latex-item="1">ok</g></svg>"#;
        assert_eq!(
            strip_data_latex_attributes(svg),
            r#"<svg data-latex-extra="keep"><text style="fill:red">data-latex="also keep"</text><g>ok</g></svg>"#
        );
    }

    #[test]
    fn renders_less_than_tex_centered_on_transparent_background() {
        let renderer = FormulaRenderer::new().unwrap();
        let tall_raster = renderer
            .render(
                r"x<y",
                FormulaRasterTarget {
                    columns: 8,
                    rows: 4,
                    cell_pixel_width: 8,
                    cell_pixel_height: 16,
                    foreground_rgb: [230, 230, 230],
                },
            )
            .unwrap();
        assert_eq!(
            (tall_raster.pixel_width, tall_raster.pixel_height),
            (128, 128)
        );
        assert_eq!(&tall_raster.png[..8], b"\x89PNG\r\n\x1a\n");
        let image = decode_rgba(&tall_raster);
        let transparent = image::Rgba([0, 0, 0, 0]);
        assert_eq!(*image.get_pixel(0, 0), transparent);
        assert_eq!(
            *image.get_pixel(image.width() - 1, image.height() - 1),
            transparent
        );
        let (_, top, _, bottom) = foreground_bounds(&image);
        assert!(top > 0);
        assert!(top.abs_diff(image.height() - bottom) <= 1);

        let wide_raster = renderer
            .render(
                r"x<y",
                FormulaRasterTarget {
                    columns: 8,
                    rows: 1,
                    cell_pixel_width: 8,
                    cell_pixel_height: 16,
                    foreground_rgb: [230, 230, 230],
                },
            )
            .unwrap();
        let image = decode_rgba(&wide_raster);
        let (left, _, right, _) = foreground_bounds(&image);
        assert!(left > 0);
        assert!(left.abs_diff(image.width() - right) <= 1);
    }

    #[test]
    fn renders_formula_with_strong_weight() {
        let renderer = FormulaRenderer::new().unwrap();
        let target = FormulaRasterTarget {
            columns: 12,
            rows: 2,
            cell_pixel_width: 8,
            cell_pixel_height: 16,
            foreground_rgb: [230, 230, 230],
        };
        let tex = r"x^2 + \frac{\Delta}{1+\alpha} = 42";
        let bold = renderer.render(tex, target).unwrap();

        let font_size = formula_font_size(target.cell_pixel_height);
        let svg = renderer
            .context
            .with(|ctx| {
                let render: Function = ctx.globals().get("__entry_renderTeX")?;
                render.call::<_, String>((tex, f64::from(font_size), 1_i32, true))
            })
            .unwrap();
        let svg = strip_data_latex_attributes(&svg);
        let plain_tree = usvg::Tree::from_str(
            &svg,
            &usvg::Options {
                font_size,
                style_sheet: Some("svg { color: rgb(230, 230, 230); }".to_string()),
                ..usvg::Options::default()
            },
        )
        .unwrap();
        let plain = rasterize_tree(plain_tree, bold.pixel_width, bold.pixel_height).unwrap();

        let bold_image = decode_rgba(&bold);
        let plain_image = decode_rgba(&plain);
        assert!(alpha_mass(&bold_image) * 10 > alpha_mass(&plain_image) * 11);
        assert_eq!(*bold_image.get_pixel(0, 0), image::Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn renders_inline_mathjax_output_as_inline_layout() {
        let renderer = FormulaRenderer::new().unwrap();
        let layout = renderer
            .render_for_layout(
                "x+1",
                FormulaKind::Inline,
                FormulaLayoutTarget {
                    max_columns: 80,
                    cell_pixel_width: 8,
                    cell_pixel_height: 16,
                    foreground_rgb: [230, 230, 230],
                    render_timeout: FORMULA_RENDER_TIMEOUT,
                },
            )
            .unwrap();

        assert!(!layout.is_block);
        assert_eq!(layout.rows, 1);
    }

    #[test]
    fn display_formula_tracks_terminal_font_height_without_forcing_two_rows() {
        let renderer = FormulaRenderer::new().unwrap();
        let layout = renderer
            .render_for_layout(
                r"H=1.25\times42.5=53.125",
                FormulaKind::Display,
                FormulaLayoutTarget {
                    max_columns: 80,
                    cell_pixel_width: 8,
                    cell_pixel_height: 18,
                    foreground_rgb: [230, 230, 230],
                    render_timeout: FORMULA_RENDER_TIMEOUT,
                },
            )
            .unwrap();

        assert!(layout.is_block);
        assert_eq!(layout.rows, 1);
        let image = decode_rgba(&layout.raster);
        let (_, top, _, bottom) = foreground_bounds(&image);
        let displayed_foreground_height = (bottom - top).div_ceil(RASTER_SCALE);
        assert!(displayed_foreground_height >= 12);
        assert!(displayed_foreground_height <= 18);
    }

    #[test]
    fn enforces_source_and_pixel_boundaries() {
        let renderer = FormulaRenderer::new().unwrap();
        let normal = FormulaRasterTarget {
            columns: 1,
            rows: 1,
            cell_pixel_width: 8,
            cell_pixel_height: 16,
            foreground_rgb: [255, 255, 255],
        };
        assert!(matches!(
            renderer.render(&"x".repeat(MAX_FORMULA_SOURCE_BYTES + 1), normal),
            Err(FormulaRenderError::SourceTooLarge)
        ));
        let huge = FormulaRasterTarget {
            columns: u16::MAX,
            rows: u16::MAX,
            cell_pixel_width: u16::MAX,
            cell_pixel_height: u16::MAX,
            foreground_rgb: [255; 3],
        };
        assert!(matches!(
            renderer.render("x", huge),
            Err(FormulaRenderError::RasterTooLarge)
        ));
    }
}
