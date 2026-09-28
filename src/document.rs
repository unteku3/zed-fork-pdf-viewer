//! Loading, rasterizing and text-layer extraction for PDF documents.
//!
//! The text layer is what makes a PDF selectable: PDF files have no notion of
//! words or lines, only glyphs stamped at arbitrary positions. We collect every
//! glyph the interpreter draws, together with its box in page space, then group
//! those glyphs back into lines so they can be hit-tested and selected like text.

use hayro::{RenderCache, RenderSettings, render};
use hayro_interpret::font::Glyph;
use hayro_interpret::hayro_cmap::BfString;
use hayro_interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterCache,
    InterpreterSettings, Paint, PathDrawMode, SoftMask, TransformExt, interpret_page,
};
use hayro_syntax::Pdf;
use image::ImageEncoder as _;
use kurbo::{Affine, BezPath, Point, Rect, Shape as _};
use std::ops::Range;

/// Horizontal gap between two glyphs, as a fraction of the em size, that we read
/// as a word break when the PDF itself does not emit a space glyph.
const SPACE_GAP_RATIO: f64 = 0.22;
/// How far two glyphs' baselines may differ, as a fraction of the em size, while
/// still counting as the same line.
const SAME_LINE_RATIO: f64 = 0.45;
/// Baseline shift, as a fraction of the em size, above which a glyph reads as a
/// superscript or subscript rather than body text.
const SCRIPT_SHIFT_RATIO: f64 = 0.18;
/// A run of script glyphs is only worth marking up if it is set smaller than the
/// surrounding text; this is the largest relative size we still treat as script.
const SCRIPT_SIZE_RATIO: f64 = 0.92;

const MAX_RENDER_PIXELS: f32 = 16_000_000.0;
const MAX_RENDER_DIMENSION: f32 = 8192.0;

pub(super) fn render_scale(width: f32, height: f32, requested: f32) -> Option<f32> {
    if !width.is_finite()
        || !height.is_finite()
        || !requested.is_finite()
        || width <= 0.0
        || height <= 0.0
        || requested <= 0.0
    {
        return None;
    }
    Some(
        requested
            .min(MAX_RENDER_DIMENSION / width.max(height))
            .min((MAX_RENDER_PIXELS / (width * height)).sqrt()),
    )
}

pub struct PdfDocument {
    pdf: Pdf,
}

pub struct RenderedPage {
    pub width: u32,
    pub height: u32,
    /// Premultiplied BGRA, which is the byte order gpui uploads to the GPU.
    pub bgra: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct PlacedGlyph {
    pub text: String,
    pub bounds: Rect,
    pub baseline: f64,
    /// Size of the em square in page units; stands in for the font size.
    pub em: f64,
}

#[derive(Clone, Debug)]
pub struct TextLine {
    pub glyphs: Range<usize>,
    pub baseline: f64,
    pub em: f64,
    pub bounds: Rect,
}

/// Every glyph on a page, in reading order, plus the lines they form.
#[derive(Clone, Debug, Default)]
pub struct PageTextLayer {
    pub glyphs: Vec<PlacedGlyph>,
    pub lines: Vec<TextLine>,
}

impl PdfDocument {
    pub fn load(bytes: Vec<u8>) -> Result<Self, String> {
        let pdf = Pdf::new(bytes).map_err(|e| format!("could not read PDF: {e:?}"))?;
        Ok(Self { pdf })
    }

    pub fn page_count(&self) -> usize {
        self.pdf.pages().len()
    }

    /// Size of a page in PDF points, with page rotation already applied.
    pub fn page_size(&self, index: usize) -> Option<(f32, f32)> {
        Some(self.pdf.pages().iter().nth(index)?.render_dimensions())
    }

    pub fn render_page(&self, index: usize, scale: f32) -> Option<RenderedPage> {
        let page = self.pdf.pages().iter().nth(index)?;
        let (width, height) = page.render_dimensions();
        let scale = render_scale(width, height, scale)?;
        let cache = RenderCache::new();
        let settings = RenderSettings {
            x_scale: scale,
            y_scale: scale,
            width: Some((width * scale).floor().max(1.0) as u16),
            height: Some((height * scale).floor().max(1.0) as u16),
            bg_color: hayro::vello_cpu::color::palette::css::WHITE,
            ..Default::default()
        };
        let pixmap = render(&page, &cache, &InterpreterSettings::default(), &settings);

        let (width, height) = (pixmap.width() as u32, pixmap.height() as u32);
        let mut bgra = Vec::with_capacity((width * height * 4) as usize);
        for px in pixmap.data() {
            bgra.extend_from_slice(&[px.b, px.g, px.r, px.a]);
        }

        Some(RenderedPage {
            width,
            height,
            bgra,
        })
    }

    pub fn text_layer(&self, index: usize) -> Option<PageTextLayer> {
        let page = self.pdf.pages().iter().nth(index)?;
        let (width, height) = page.render_dimensions();

        let cache = InterpreterCache::new();
        let mut context = Context::new(
            page.initial_transform(true).to_kurbo(),
            Rect::new(0.0, 0.0, width as f64, height as f64),
            &cache,
            page.xref(),
            InterpreterSettings::default(),
        );

        let mut device = TextLayerDevice {
            clips: vec![Rect::new(0.0, 0.0, width as f64, height as f64)],
            ..Default::default()
        };
        interpret_page(&page, &mut context, &mut device);

        Some(PageTextLayer::new(device.glyphs))
    }
}

impl PageTextLayer {
    fn new(mut glyphs: Vec<PlacedGlyph>) -> Self {
        if glyphs.is_empty() {
            return Self::default();
        }

        // Content streams may stamp glyphs in any order, so recover reading order
        // ourselves: group by baseline into lines, then order each line by x.
        glyphs.sort_by(|a, b| {
            a.baseline
                .partial_cmp(&b.baseline)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    a.bounds
                        .x0
                        .partial_cmp(&b.bounds.x0)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });

        let mut ordered: Vec<PlacedGlyph> = Vec::with_capacity(glyphs.len());
        let mut lines: Vec<TextLine> = Vec::new();
        let mut current: Vec<PlacedGlyph> = Vec::new();

        for glyph in glyphs {
            // Compare against the line's first glyph rather than its previous one:
            // measuring from a moving reference lets a column of stacked fraction
            // rows chain together into one bogus line.
            let same_line = current.first().is_some_and(|anchor| {
                // Tolerance follows the largest glyph on the line so a big
                // integral sign still gathers the text set beside it.
                let em = current
                    .iter()
                    .map(|placed| placed.em)
                    .fold(glyph.em, f64::max);
                (glyph.baseline - anchor.baseline).abs() <= em * SAME_LINE_RATIO
            });

            if !same_line && !current.is_empty() {
                Self::flush_line(&mut current, &mut ordered, &mut lines);
            }
            current.push(glyph);
        }
        if !current.is_empty() {
            Self::flush_line(&mut current, &mut ordered, &mut lines);
        }

        Self {
            glyphs: ordered,
            lines,
        }
    }

    fn flush_line(
        current: &mut Vec<PlacedGlyph>,
        ordered: &mut Vec<PlacedGlyph>,
        lines: &mut Vec<TextLine>,
    ) {
        current.sort_by(|a, b| {
            a.bounds
                .x0
                .partial_cmp(&b.bounds.x0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let start = ordered.len();
        // A line's own baseline and em come from its body text, so take the most
        // common em rather than an average that superscripts would drag down.
        let Some(first) = current.first() else {
            return;
        };
        let em = dominant_em(current);
        let baseline = current
            .iter()
            .find(|glyph| (glyph.em - em).abs() < f64::EPSILON.max(em * 0.05))
            .map_or(first.baseline, |glyph| glyph.baseline);

        let bounds = current
            .iter()
            .fold(first.bounds, |bounds, glyph| bounds.union(glyph.bounds));

        ordered.append(current);
        lines.push(TextLine {
            glyphs: start..ordered.len(),
            baseline,
            em,
            bounds,
        });
    }

    /// Caret position closest to `point`, as an index into `glyphs`.
    pub fn index_at(&self, point: Point) -> usize {
        if self.glyphs.is_empty() {
            return 0;
        }

        // Prefer the line the point falls in vertically; otherwise the nearest one.
        let line = self
            .lines
            .iter()
            .find(|line| point.y >= line.bounds.y0 && point.y <= line.bounds.y1)
            .or_else(|| {
                self.lines.iter().min_by(|a, b| {
                    let da = vertical_distance(a.bounds, point.y);
                    let db = vertical_distance(b.bounds, point.y);
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                })
            });

        let Some(line) = line else {
            return 0;
        };

        for index in line.glyphs.clone() {
            let Some(glyph) = self.glyphs.get(index) else {
                break;
            };
            if point.x < glyph.bounds.x0 + glyph.bounds.width() / 2.0 {
                return index;
            }
        }
        line.glyphs.end
    }

    /// Highlight rectangles for a selection, one per line it covers.
    pub fn selection_rects(&self, range: Range<usize>) -> Vec<Rect> {
        let range = normalize(range, self.glyphs.len());
        let mut rects = Vec::new();

        for line in &self.lines {
            let start = range.start.max(line.glyphs.start);
            let end = range.end.min(line.glyphs.end);
            if start >= end {
                continue;
            }

            let Some(glyphs) = self.glyphs.get(start..end) else {
                continue;
            };
            let Some(first) = glyphs.first() else {
                continue;
            };
            rects.push(
                glyphs
                    .iter()
                    .fold(first.bounds, |rect, glyph| rect.union(glyph.bounds)),
            );
        }

        rects
    }

    /// The selected text, with word gaps, line breaks and — because we kept each
    /// glyph's baseline — superscripts and subscripts written back out as `^`/`_`.
    pub fn selected_text(&self, range: Range<usize>) -> String {
        self.selected_text_matching(range, |_| true)
    }

    fn selected_text_matching(
        &self,
        range: Range<usize>,
        include: impl Fn(&PlacedGlyph) -> bool,
    ) -> String {
        let range = normalize(range, self.glyphs.len());
        let mut out = String::new();

        for line in self.lines.iter() {
            let start = range.start.max(line.glyphs.start);
            let end = range.end.min(line.glyphs.end);
            let Some(glyphs) = self.glyphs.get(start..end).filter(|g| !g.is_empty()) else {
                continue;
            };
            let mut glyphs = glyphs.iter().filter(|glyph| include(glyph)).peekable();
            if glyphs.peek().is_none() {
                continue;
            }

            if !out.is_empty() {
                // Hyphenated words split across lines get joined back together.
                if out.ends_with('-') {
                    out.pop();
                } else {
                    out.push('\n');
                }
            }

            let mut script = Script::Body;
            let mut previous: Option<&PlacedGlyph> = None;

            for glyph in glyphs {
                if let Some(prev) = previous {
                    let gap = glyph.bounds.x0 - prev.bounds.x1;
                    if gap > glyph.em.max(prev.em) * SPACE_GAP_RATIO
                        && !glyph.text.starts_with(' ')
                        && !prev.text.ends_with(' ')
                    {
                        script = close_script(&mut out, script);
                        out.push(' ');
                    }
                }

                let wanted = script_of(glyph, line);
                if wanted != script {
                    close_script(&mut out, script);
                    match wanted {
                        Script::Super => out.push_str("^{"),
                        Script::Sub => out.push_str("_{"),
                        Script::Body => {}
                    }
                    script = wanted;
                }

                out.push_str(&glyph.text);
                previous = Some(glyph);
            }

            close_script(&mut out, script);
        }

        out
    }

    /// Every glyph whose box intersects `region`, as text — used when the user
    /// rubber-bands an area rather than sweeping across a line.
    pub fn text_in_region(&self, region: Rect) -> String {
        self.selected_text_matching(0..self.glyphs.len(), |glyph| {
            !region.intersect(glyph.bounds).is_zero_area()
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    Body,
    Super,
    Sub,
}

fn close_script(out: &mut String, script: Script) -> Script {
    if script != Script::Body {
        out.push('}');
    }
    Script::Body
}

fn script_of(glyph: &PlacedGlyph, line: &TextLine) -> Script {
    if glyph.em > line.em * SCRIPT_SIZE_RATIO {
        return Script::Body;
    }
    let shift = glyph.baseline - line.baseline;
    if shift.abs() < line.em * SCRIPT_SHIFT_RATIO {
        return Script::Body;
    }
    // Page space is y-down, so a raised glyph sits at a smaller y.
    if shift < 0.0 {
        Script::Super
    } else {
        Script::Sub
    }
}

fn dominant_em(glyphs: &[PlacedGlyph]) -> f64 {
    let mut best = glyphs.first().map_or(1.0, |glyph| glyph.em);
    let mut best_count = 0;
    for candidate in glyphs {
        let count = glyphs
            .iter()
            .filter(|g| (g.em - candidate.em).abs() <= candidate.em * 0.05)
            .count();
        if count > best_count {
            best_count = count;
            best = candidate.em;
        }
    }
    best
}

fn vertical_distance(rect: Rect, y: f64) -> f64 {
    if y < rect.y0 {
        rect.y0 - y
    } else if y > rect.y1 {
        y - rect.y1
    } else {
        0.0
    }
}

fn normalize(range: Range<usize>, len: usize) -> Range<usize> {
    let (start, end) = if range.start <= range.end {
        (range.start, range.end)
    } else {
        (range.end, range.start)
    };
    start.min(len)..end.min(len)
}

#[derive(Default)]
struct TextLayerDevice {
    glyphs: Vec<PlacedGlyph>,
    clips: Vec<Rect>,
}

impl<'a> Device<'a> for TextLayerDevice {
    fn set_soft_mask(&mut self, _: Option<SoftMask<'a>>) {}
    fn set_blend_mode(&mut self, _: BlendMode) {}
    fn draw_path(&mut self, _: &BezPath, _: Affine, _: &Paint<'a>, _: &PathDrawMode) {}
    fn push_clip_path(&mut self, clip_path: &ClipPath) {
        let bounds = clip_path.path.bounding_box();
        self.clips.push(
            self.clips
                .last()
                .map_or(bounds, |clip| clip.intersect(bounds)),
        );
    }
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: Affine) {}
    fn pop_clip_path(&mut self) {
        self.clips.pop();
    }
    fn pop_transparency_group(&mut self) {}

    fn draw_glyph(
        &mut self,
        glyph: &Glyph<'a>,
        transform: Affine,
        glyph_transform: Affine,
        _paint: &Paint<'a>,
        // Invisible glyphs are kept on purpose: that is how scanned pages carry
        // their OCR layer, and it is exactly the text we want to be selectable.
        _draw_mode: &GlyphDrawMode,
    ) {
        let text = match glyph.as_unicode() {
            Some(BfString::Char(c)) => c.to_string(),
            Some(BfString::String(s)) => s,
            None => return,
        };

        // Outlines live on a 1000-unit em square, with the baseline at y = 0.
        let advance = match glyph {
            Glyph::Outline(outline) => outline.advance_width().unwrap_or(500.0) as f64,
            Glyph::Type3(_) => 500.0,
        };
        let placement = transform * glyph_transform;

        // Math fonts may stretch a tiny outline with a huge font transform.
        // A transformed em box then covers unrelated paragraphs or the entire page.
        let bounds = match glyph {
            Glyph::Outline(outline) => {
                let path = outline.outline();
                (!path.elements().is_empty()).then(|| (placement * path).bounding_box())
            }
            Glyph::Type3(_) => None,
        }
        .unwrap_or_else(|| placement.transform_rect_bbox(Rect::new(0.0, -220.0, advance, 780.0)));
        // Stretched delimiters are often clipped to a small visible segment.
        let bounds = self
            .clips
            .last()
            .map_or(bounds, |clip| clip.intersect(bounds));
        if bounds.is_zero_area() {
            return;
        }

        let origin = placement * Point::ZERO;
        let horizontal_em = (placement * Point::new(1000.0, 0.0) - origin).hypot();
        let vertical_em = (placement * Point::new(0.0, 1000.0) - origin).hypot();
        let em = horizontal_em.min(vertical_em).max(1e-6);
        if !bounds.is_finite() || !origin.is_finite() || !em.is_finite() {
            return;
        }

        self.glyphs.push(PlacedGlyph {
            text,
            bounds,
            baseline: origin.y.clamp(bounds.y0, bounds.y1 + em * 0.3),
            em,
        });
    }
}

impl PdfDocument {
    /// Rasterize just `region` of a page, in page-space points.
    ///
    /// A rubber-banded area is sent to the agent as an image, which is the only
    /// faithful way to hand over display math, diagrams or a scanned page: the
    /// flat text layer cannot express two-dimensional layout such as fractions.
    pub fn render_region(&self, index: usize, region: Rect, scale: f32) -> Option<RenderedPage> {
        if !region.is_finite() || region.width() <= 0.0 || region.height() <= 0.0 {
            return None;
        }
        let (width, height) = self.page_size(index)?;
        let scale = render_scale(width, height, scale)?;
        let page = self.render_page(index, scale)?;

        let left = ((region.x0 * scale as f64).floor().max(0.0) as u32).min(page.width);
        let top = ((region.y0 * scale as f64).floor().max(0.0) as u32).min(page.height);
        let right = ((region.x1 * scale as f64).ceil().max(0.0) as u32).min(page.width);
        let bottom = ((region.y1 * scale as f64).ceil().max(0.0) as u32).min(page.height);

        let (width, height) = (right.saturating_sub(left), bottom.saturating_sub(top));
        if width == 0 || height == 0 {
            return None;
        }

        let mut bgra = Vec::with_capacity((width * height * 4) as usize);
        for row in top..bottom {
            let start = ((row * page.width + left) * 4) as usize;
            let end = start + (width * 4) as usize;
            bgra.extend_from_slice(&page.bgra[start..end]);
        }

        Some(RenderedPage {
            width,
            height,
            bgra,
        })
    }
}

impl RenderedPage {
    pub fn to_png(&self) -> anyhow::Result<Vec<u8>> {
        // The buffer is kept in gpui's BGRA order, so put it back to RGBA first.
        let mut rgba = self.bgra.clone();
        for pixel in rgba.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }

        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png).write_image(
            &rgba,
            self.width,
            self.height,
            image::ExtendedColorType::Rgba8,
        )?;
        Ok(png)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PdfSelection, PdfSendMode, PdfViewerSettings};
    use std::sync::Arc;

    #[test]
    fn agent_send_modes_filter_text_and_image_independently() {
        let document = Arc::new(document());
        assert_eq!(PdfSendMode::default(), PdfSendMode::ImageAndText);
        for (send_mode, includes_text, includes_image) in [
            (PdfSendMode::Image, false, true),
            (PdfSendMode::Text, true, false),
            (PdfSendMode::ImageAndText, true, true),
        ] {
            let selection = PdfSelection {
                text: "selected content".into(),
                page: 0,
                page_count: 1,
                path: "sample.pdf".into(),
                region: Some(Rect::new(10.0, 20.0, 40.0, 50.0)),
                settings: PdfViewerSettings {
                    send: send_mode,
                    ..Default::default()
                },
                document: document.clone(),
            };
            let context = selection.context_text_for_agent(true, true);
            assert_eq!(context.contains("selected content"), includes_text);
            assert_eq!(selection.image_region().is_some(), includes_image);
            assert!(context.contains("PDF: sample.pdf\nPage 1 of 1"));
            assert!(context.contains("10.0, 20.0 - 40.0, 50.0"));
            assert!(!context.contains("not attached"));
            assert_eq!(
                context,
                selection.context_text_for_agent(true, false),
                "with no templates configured, a repeat attachment reads the same"
            );

            let unsupported_context = selection.context_text_for_agent(false, true);
            assert_eq!(unsupported_context.contains("not attached"), includes_image);
            assert_eq!(
                unsupported_context.contains("selected content"),
                includes_text
            );
        }
    }

    #[test]
    fn text_only_scan_reports_missing_text_without_attaching_an_image() {
        let mut selection = PdfSelection {
            text: " \n".into(),
            page: 0,
            page_count: 1,
            path: "scan.pdf".into(),
            region: Some(Rect::new(0.0, 0.0, 50.0, 50.0)),
            settings: PdfViewerSettings {
                send: PdfSendMode::Text,
                ..Default::default()
            },
            document: Arc::new(document()),
        };
        assert!(
            selection
                .context_text_for_agent(true, true)
                .contains("No text could be extracted")
        );
        assert!(selection.image_region().is_none());
        selection.settings.send = PdfSendMode::Image;
        assert!(
            !selection
                .context_text_for_agent(true, true)
                .contains("No text could be extracted")
        );
        assert!(selection.image_region().is_some());
    }

    fn document() -> PdfDocument {
        let content = "1 0 0 rg 0 50 50 50 re f\n0 0 1 rg 50 0 50 50 re f\nBT /F1 12 Tf 10 70 Td (Hello PDF) Tj ET";
        document_with_content(content)
    }

    #[test]
    fn clipboard_region_is_a_png_image_without_text_or_context_pages() {
        let document = document_with_content("1 0 0 rg 0 0 100 100 re f");
        let item = crate::render_clipboard_region(&document, 0, Rect::new(10.0, 20.0, 40.0, 60.0))
            .expect("clipboard image");
        let decoded = image::load_from_memory(&item)
            .expect("valid PNG")
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (60, 80));
        assert_eq!(decoded.get_pixel(30, 40).0, [255, 0, 0, 255]);
    }

    #[test]
    fn invalid_clipboard_region_returns_error_instead_of_empty_image() {
        let document = document();
        assert!(
            crate::render_clipboard_region(&document, 1, Rect::new(0.0, 0.0, 10.0, 10.0)).is_err()
        );
        assert!(
            crate::render_clipboard_region(&document, 0, Rect::new(0.0, 0.0, 0.0, 10.0)).is_err()
        );
    }

    fn document_with_content(content: &str) -> PdfDocument {
        document_with_pages(&[content])
    }

    fn document_with_pages(contents: &[&str]) -> PdfDocument {
        let children = (0..contents.len())
            .map(|page| format!("{} 0 R", 3 + page * 3))
            .collect::<Vec<_>>()
            .join(" ");
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!(
                "<< /Type /Pages /Kids [{children}] /Count {} >>",
                contents.len()
            ),
        ];
        for (index, content) in contents.iter().enumerate() {
            let font = 4 + index * 3;
            let stream = 5 + index * 3;
            objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 {font} 0 R >> >> /Contents {stream} 0 R >>"));
            objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());
            objects.push(format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ));
        }
        let count = objects.len() + 1;
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
        }
        let xref = pdf.len();
        pdf.push_str(&format!("xref\n0 {count}\n0000000000 65535 f \n"));
        for offset in offsets {
            pdf.push_str(&format!("{offset:010} 00000 n \n"));
        }
        pdf.push_str(&format!(
            "trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        ));
        PdfDocument::load(pdf.into_bytes()).expect("valid fixture")
    }

    fn settings(before: usize, after: usize, current: bool) -> PdfViewerSettings {
        PdfViewerSettings {
            pages_before: before,
            pages_after: after,
            include_current_page: current,
            ..Default::default()
        }
    }

    #[test]
    fn context_pages_are_bounded_and_default_to_the_selection_alone() {
        let pages = |before, after, current, page, count| {
            crate::context_pages(&settings(before, after, current), page, count)
        };
        assert_eq!(PdfViewerSettings::default().pages_before, 0);
        assert_eq!(PdfViewerSettings::default().pages_after, 0);
        assert!(!PdfViewerSettings::default().include_current_page);
        assert!(pages(0, 0, false, 39, 576).is_empty());
        assert_eq!(pages(0, 0, true, 39, 576), vec![39]);
        assert_eq!(pages(1, 1, true, 39, 576), vec![38, 39, 40]);
        // Neighbours without the page itself leave a hole a range could not express.
        assert_eq!(pages(1, 1, false, 39, 576), vec![38, 40]);
        assert_eq!(pages(2, 3, true, 39, 576), vec![37, 38, 39, 40, 41, 42]);
        // Both ends clamp to the document rather than wrapping or panicking.
        assert_eq!(pages(1, 1, true, 0, 576), vec![0, 1]);
        assert_eq!(pages(1, 1, true, 575, 576), vec![574, 575]);
        assert_eq!(pages(5, 5, true, 0, 1), vec![0]);
        assert!(pages(5, 5, true, 0, 0).is_empty());
        assert!(pages(5, 5, true, 600, 576).is_empty());
    }

    #[test]
    fn page_lists_collapse_runs_and_keep_gaps() {
        assert_eq!(crate::page_list(&[]), "");
        assert_eq!(crate::page_list(&[39]), "40");
        assert_eq!(crate::page_list(&[37, 38, 39]), "38-40");
        assert_eq!(crate::page_list(&[38, 40]), "39, 41");
        assert_eq!(crate::page_list(&[0, 1, 5, 7, 8]), "1-2, 6, 8-9");
    }

    #[test]
    fn preamble_templates_replace_the_built_in_header_only_when_configured() {
        let mut selection = PdfSelection {
            text: "focus".into(),
            page: 59,
            page_count: 673,
            path: "/books/ddia.pdf".into(),
            region: Some(Rect::new(52.4, 27.0, 475.4, 190.5)),
            settings: PdfViewerSettings {
                send: PdfSendMode::Image,
                ..Default::default()
            },
            document: Arc::new(document()),
        };
        assert!(
            selection
                .context_text(true)
                .starts_with("PDF: /books/ddia.pdf\nPage 60 of 673")
        );

        let directory = tempfile::tempdir().expect("temp dir");
        let write = |name: &str, body: &str| {
            let path = directory.path().join(name);
            std::fs::write(&path, body).expect("write preamble");
            path
        };

        selection.settings.preamble_first = Some(write(
            "first.md",
            "Reading {filename}, page {page}/{page_count} ({area}).\n",
        ));
        selection.settings.preamble_repeat = Some(write("repeat.md", "Also from {filename}."));
        assert_eq!(
            selection.context_text(true),
            "Reading ddia.pdf, page 60/673 (52.4, 27.0 - 475.4, 190.5)."
        );
        assert_eq!(selection.context_text(false), "Also from ddia.pdf.");

        // A template set for one position leaves the other on the built-in header.
        selection.settings.preamble_repeat = None;
        assert!(
            selection
                .context_text(false)
                .starts_with("PDF: /books/ddia.pdf")
        );

        // A whole-page selection has no area to interpolate.
        selection.region = None;
        assert_eq!(
            selection.context_text(true),
            "Reading ddia.pdf, page 60/673 ()."
        );
    }

    #[test]
    fn an_unusable_preamble_file_falls_back_instead_of_sending_a_bare_image() {
        let directory = tempfile::tempdir().expect("temp dir");
        let selection = |path: std::path::PathBuf| PdfSelection {
            text: String::new(),
            page: 59,
            page_count: 673,
            path: "/books/ddia.pdf".into(),
            region: None,
            settings: PdfViewerSettings {
                send: PdfSendMode::Image,
                preamble_first: Some(path),
                ..Default::default()
            },
            document: Arc::new(document()),
        };

        let missing = directory.path().join("does-not-exist.md");
        let blank = directory.path().join("blank.md");
        std::fs::write(&blank, "   \n\n").expect("write blank");
        let oversized = directory.path().join("huge.md");
        std::fs::write(
            &oversized,
            "x".repeat(crate::pdf_viewer_settings::MAX_PREAMBLE_BYTES as usize + 1),
        )
        .expect("write oversized");

        for path in [missing, blank, oversized, directory.path().to_path_buf()] {
            // Silently dropping the header would leave the agent an image with
            // no file name, no page and no instruction at all.
            assert!(
                selection(path.clone())
                    .context_text(true)
                    .starts_with("PDF: /books/ddia.pdf\nPage 60 of 673"),
                "{} should fall back to the built-in header",
                path.display()
            );
        }
    }

    #[test]
    fn context_packet_respects_all_format_and_depth_combinations() {
        let document = Arc::new(document_with_pages(&[
            "BT /F1 12 Tf 10 70 Td (First) Tj ET",
            "BT /F1 12 Tf 10 70 Td (Second) Tj ET",
            "BT /F1 12 Tf 10 70 Td (Third) Tj ET",
        ]));
        // (pages_before, pages_after, include_current_page)
        for depth in [(0, 0, false), (0, 0, true), (1, 1, true), (1, 1, false)] {
            let (before, after, current) = depth;
            for mode in [
                PdfSendMode::Image,
                PdfSendMode::Text,
                PdfSendMode::ImageAndText,
            ] {
                let selection = PdfSelection {
                    text: "focus".into(),
                    page: 1,
                    page_count: 3,
                    path: "three.pdf".into(),
                    region: Some(Rect::new(10.0, 20.0, 40.0, 50.0)),
                    settings: PdfViewerSettings {
                        send: mode,
                        ..settings(before, after, current)
                    },
                    document: document.clone(),
                };
                let text = selection.context_page_text().expect("context text");
                assert_eq!(text.contains("Second"), mode.includes_text() && current);
                for word in ["First", "Third"] {
                    assert_eq!(
                        text.contains(word),
                        mode.includes_text() && before > 0 && after > 0
                    );
                }
                let images = selection.image_regions();
                assert_eq!(
                    images.len(),
                    if mode.includes_image() {
                        1 + selection.context_pages().len()
                    } else {
                        0
                    }
                );
                if mode.includes_image() && before > 0 && after > 0 {
                    let expected = if current {
                        vec![1, 0, 1, 2]
                    } else {
                        vec![1, 0, 2]
                    };
                    assert_eq!(
                        images.iter().map(|(page, _)| *page).collect::<Vec<_>>(),
                        expected
                    );
                    assert!(images[0].1.is_some());
                    assert!(images[1..].iter().all(|(_, region)| region.is_none()));
                }
                assert_eq!(
                    selection.context_text_for_terminal(true).contains("Second"),
                    mode.includes_text() && current
                );
            }
        }
    }

    #[test]
    fn whole_page_selection_is_not_attached_twice_and_scans_are_explicit() {
        let mut selection = PdfSelection {
            text: String::new(),
            page: 0,
            page_count: 1,
            path: "scan.pdf".into(),
            region: Some(Rect::new(0.0, 0.0, 100.0, 100.0)),
            settings: PdfViewerSettings {
                send: PdfSendMode::ImageAndText,
                ..settings(1, 1, true)
            },
            document: Arc::new(document_with_content("1 0 0 rg 0 0 100 100 re f")),
        };
        assert_eq!(selection.image_regions(), vec![(0, None)]);
        assert!(
            selection
                .context_page_text()
                .expect("scan context")
                .contains("OCR has not been performed")
        );
        selection.settings = PdfViewerSettings {
            send: PdfSendMode::ImageAndText,
            ..Default::default()
        };
        assert_eq!(selection.image_regions(), vec![(0, selection.region)]);
    }

    #[test]
    fn region_uses_top_left_coordinates_and_preserves_png_colors() {
        let document = document();
        let crop = document
            .render_region(0, Rect::new(0.0, 0.0, 20.0, 20.0), 2.0)
            .expect("crop");
        assert_eq!((crop.width, crop.height), (40, 40));
        let decoded = image::load_from_memory(&crop.to_png().expect("PNG"))
            .expect("decode")
            .to_rgba8();
        assert_eq!(decoded.get_pixel(10, 10).0, [255, 0, 0, 255]);
        let crop = document
            .render_region(0, Rect::new(70.0, 70.0, 110.0, 110.0), 2.0)
            .expect("clipped crop");
        assert_eq!((crop.width, crop.height), (60, 60));
        let decoded = image::load_from_memory(&crop.to_png().expect("PNG"))
            .expect("decode")
            .to_rgba8();
        assert_eq!(decoded.get_pixel(10, 10).0, [0, 0, 255, 255]);
    }

    #[test]
    fn selection_extracts_text_and_region_without_text_still_renders() {
        let document = document();
        let layer = document.text_layer(0).expect("text layer");
        assert_eq!(layer.selected_text(0..layer.glyphs.len()), "Hello PDF");
        let region = Rect::new(70.0, 70.0, 90.0, 90.0);
        assert!(layer.text_in_region(region).is_empty());
        assert!(document.render_region(0, region, 2.0).is_some());
        assert!(layer.selected_text(usize::MAX..usize::MAX).is_empty());
        assert!(layer.selection_rects(usize::MAX..usize::MAX).is_empty());
    }

    #[test]
    fn region_does_not_include_other_columns_or_intervening_glyphs() {
        let glyph = |text: &str, x, y| PlacedGlyph {
            text: text.into(),
            bounds: Rect::new(x, y, x + 8.0, y + 10.0),
            baseline: y + 8.0,
            em: 10.0,
        };
        let layer = PageTextLayer::new(vec![
            glyph("A", 10.0, 10.0),
            glyph("outside", 100.0, 10.0),
            glyph("B", 10.0, 30.0),
            glyph("outside", 100.0, 30.0),
            glyph("below", 10.0, 50.0),
        ]);
        assert_eq!(
            layer.text_in_region(Rect::new(0.0, 0.0, 30.0, 45.0)),
            "A\nB"
        );
        assert_eq!(
            layer.text_in_region(Rect::new(90.0, 0.0, 120.0, 45.0)),
            "outside\noutside"
        );
    }

    #[test]
    fn text_hit_boxes_cover_visible_letters_above_the_baseline() {
        let layer = document().text_layer(0).expect("text layer");
        let first = layer.glyphs.first().expect("H glyph");
        assert_eq!(first.text, "H");
        assert!(first.bounds.y0 < 23.0, "{:?}", first.bounds);
        assert!(first.bounds.y1 <= 31.0, "{:?}", first.bounds);
        assert!(
            layer
                .text_in_region(Rect::new(9.0, 20.0, 19.0, 25.0))
                .starts_with('H')
        );
    }

    #[test]
    fn clipped_glyphs_do_not_leak_into_other_regions() {
        let document = document_with_content(
            "q 0 0 100 40 re W n q 0 5 100 30 re W n BT /F1 200 Tf 10 -40 Td (I) Tj ET Q Q BT /F1 12 Tf 10 70 Td (Hello) Tj ET",
        );
        let layer = document.text_layer(0).expect("text layer");
        let delimiter = layer
            .glyphs
            .iter()
            .find(|glyph| glyph.text == "I")
            .expect("clipped glyph");
        assert!(delimiter.bounds.y0 >= 65.0, "{:?}", delimiter.bounds);
        assert!(delimiter.bounds.y1 <= 95.0, "{:?}", delimiter.bounds);
        assert_eq!(
            layer.text_in_region(Rect::new(0.0, 0.0, 100.0, 50.0)),
            "Hello"
        );
    }

    #[test]
    fn invalid_regions_and_scales_do_not_render() {
        let document = document();
        for region in [
            Rect::ZERO,
            Rect::new(-20.0, -20.0, -1.0, -1.0),
            Rect::new(10.0, 10.0, 5.0, 5.0),
        ] {
            assert!(document.render_region(0, region, 2.0).is_none());
        }
        for scale in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(document.render_page(0, scale).is_none());
        }
        assert!(document.render_page(99, 1.0).is_none());
        assert!(PdfDocument::load(b"not a PDF".to_vec()).is_err());
    }

    #[test]
    fn raster_budget_limits_large_pages_and_retina_zoom() {
        for (width, height, requested) in [(600.0, 800.0, 16.0), (100_000.0, 10_000.0, 2.0)] {
            let scale = render_scale(width, height, requested).expect("scale");
            assert!(width * height * scale * scale <= MAX_RENDER_PIXELS * 1.0001);
            assert!(width.max(height) * scale <= MAX_RENDER_DIMENSION);
        }
    }
}
