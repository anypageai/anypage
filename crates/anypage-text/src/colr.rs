//! COLR colour glyphs: version 1 (gradients, transforms, layers and blend
//! modes) and version 0 (solid layers), painted by one painter.
//!
//! swash paints COLR version 0 layers and bitmap strikes, but not version 1,
//! and Android 13+ ships Noto Color Emoji as COLRv1 only (Windows 11's
//! Segoe UI Emoji has v1 glyphs too). skrifa walks the paint graph; this
//! painter evaluates the brushes per pixel (exact two-point conical and
//! sweep gradients, which tiny-skia lacks) and uses tiny-skia for glyph clip
//! masks and layer compositing. Version 0 goes through the same painter so
//! that both versions get the text colour for palette index 0xFFFF, the
//! synthetic italic slant and the same size limit.
//!
//! A glyph whose image would exceed [`MAX_GLYPH_SIDE`] is painted at the
//! largest size that fits and drawn scaled up (`Bitmap::scale`).

use std::cell::Cell;

use cosmic_text::skrifa::color::{Brush, Color, ColorGlyph, ColorPainter, ColorStop, CompositeMode, Extend, Transform as CTransform};
use cosmic_text::skrifa::instance::{LocationRef, Size};
use cosmic_text::skrifa::outline::{DrawSettings, OutlinePen};
use cosmic_text::skrifa::raw::types::BoundingBox;
use cosmic_text::skrifa::raw::TableProvider;
use cosmic_text::skrifa::{FontRef, GlyphId, MetadataProvider, OutlineGlyphCollection};
use tiny_skia::{BlendMode, FillRule, Mask, Path, PathBuilder, Pixmap, PixmapPaint};

use crate::raster::{Bitmap, GlyphKey, MAX_GLYPH_SIDE, SKEW};

/// Paint nesting deeper than this is cut off (skrifa also limits depth).
const MAX_LAYERS: usize = 64;

/// Affine map: x' = a x + b y + c, y' = d x + e y + f.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Affine {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Affine {
    const IDENTITY: Affine = Affine { a: 1.0, b: 0.0, c: 0.0, d: 0.0, e: 1.0, f: 0.0 };

    fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (self.a * x + self.b * y + self.c, self.d * x + self.e * y + self.f)
    }

    /// `self` after `t`: first `t`, then `self`.
    fn then(&self, t: &Affine) -> Affine {
        Affine {
            a: self.a * t.a + self.b * t.d,
            b: self.a * t.b + self.b * t.e,
            c: self.a * t.c + self.b * t.f + self.c,
            d: self.d * t.a + self.e * t.d,
            e: self.d * t.b + self.e * t.e,
            f: self.d * t.c + self.e * t.f + self.f,
        }
    }

    fn invert(&self) -> Option<Affine> {
        let det = self.a * self.e - self.b * self.d;
        if !det.is_finite() || det.abs() < 1e-12 {
            return None;
        }
        let (a, b, d, e) = (self.e / det, -self.b / det, -self.d / det, self.a / det);
        Some(Affine { a, b, c: -(a * self.c + b * self.f), d, e, f: -(d * self.c + e * self.f) })
    }
}

impl From<CTransform> for Affine {
    fn from(t: CTransform) -> Self {
        Affine { a: t.xx, b: t.xy, c: t.dx, d: t.yx, e: t.yy, f: t.dy }
    }
}

/// Straight RGBA, 0..1.
type Rgba = [f32; 4];

struct Pen<'a> {
    pb: PathBuilder,
    m: &'a Affine,
}

impl OutlinePen for Pen<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.m.apply(x, y);
        self.pb.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.m.apply(x, y);
        self.pb.line_to(x, y);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let (cx0, cy0) = self.m.apply(cx0, cy0);
        let (x, y) = self.m.apply(x, y);
        self.pb.quad_to(cx0, cy0, x, y);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let (cx0, cy0) = self.m.apply(cx0, cy0);
        let (cx1, cy1) = self.m.apply(cx1, cy1);
        let (x, y) = self.m.apply(x, y);
        self.pb.cubic_to(cx0, cy0, cx1, cy1, x, y);
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

/// Bounding box (x0, y0, x1, y1) of every point an outline passes through
/// or is pulled towards (control points included: a superset of the ink).
struct BoundsPen<'a> {
    m: &'a Affine,
    bb: &'a mut Option<[f32; 4]>,
}

impl BoundsPen<'_> {
    fn add(&mut self, x: f32, y: f32) {
        let (x, y) = self.m.apply(x, y);
        if !(x.is_finite() && y.is_finite()) {
            return;
        }
        *self.bb = Some(match *self.bb {
            Some([x0, y0, x1, y1]) => [x0.min(x), y0.min(y), x1.max(x), y1.max(y)],
            None => [x, y, x, y],
        });
    }
}

impl OutlinePen for BoundsPen<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.add(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.add(x, y);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.add(cx0, cy0);
        self.add(x, y);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.add(cx0, cy0);
        self.add(cx1, cy1);
        self.add(x, y);
    }
    fn close(&mut self) {}
}

/// Palette lookups, and whether the text colour was asked for.
struct Colors<'a> {
    /// CPAL palette 0, read on demand (Segoe UI Emoji's has tens of
    /// thousands of entries: converting it per glyph cost over 1 ms).
    palette: &'a [Color],
    /// The text colour for palette index 0xFFFF (black when not given).
    fg: [f32; 3],
    used_fg: Cell<bool>,
}

fn extend(t: f32, e: Extend) -> f32 {
    match e {
        Extend::Repeat => t - t.floor(),
        Extend::Reflect => {
            let r = t.rem_euclid(2.0);
            if r > 1.0 {
                2.0 - r
            } else {
                r
            }
        }
        _ => t.clamp(0.0, 1.0),
    }
}

impl Colors<'_> {
    fn color(&self, palette_index: u16, alpha: f32) -> Rgba {
        let c = if palette_index == 0xFFFF {
            self.used_fg.set(true);
            [self.fg[0], self.fg[1], self.fg[2], 1.0]
        } else {
            self.palette.get(palette_index as usize).map_or([0.0, 0.0, 0.0, 1.0], |c| [c.red as f32 / 255.0, c.green as f32 / 255.0, c.blue as f32 / 255.0, c.alpha as f32 / 255.0])
        };
        [c[0], c[1], c[2], (c[3] * alpha).clamp(0.0, 1.0)]
    }

    fn stop_color(&self, stops: &[ColorStop], t: f32) -> Rgba {
        let Some(first) = stops.first() else { return [0.0; 4] };
        if t <= first.offset {
            return self.color(first.palette_index, first.alpha);
        }
        for w in stops.windows(2) {
            let (s0, s1) = (&w[0], &w[1]);
            if t <= s1.offset {
                let span = s1.offset - s0.offset;
                let k = if span > 1e-6 { (t - s0.offset) / span } else { 1.0 };
                let (c0, c1) = (self.color(s0.palette_index, s0.alpha), self.color(s1.palette_index, s1.alpha));
                // interpolate premultiplied, return straight
                let a = c0[3] + (c1[3] - c0[3]) * k;
                if a <= 0.0 {
                    return [0.0; 4];
                }
                let mix = |i: usize| (c0[i] * c0[3] + (c1[i] * c1[3] - c0[i] * c0[3]) * k) / a;
                return [mix(0), mix(1), mix(2), a];
            }
        }
        let last = &stops[stops.len() - 1];
        self.color(last.palette_index, last.alpha)
    }

    /// Brush colour at a point in brush space, or None outside the shape
    /// (a two-point conical gradient leaves some points undefined).
    fn brush_at(&self, brush: &Brush<'_>, x: f32, y: f32) -> Option<Rgba> {
        match brush {
            Brush::Solid { palette_index, alpha } => Some(self.color(*palette_index, *alpha)),
            Brush::LinearGradient { p0, p1, color_stops, extend: e } => {
                let (dx, dy) = (p1.x - p0.x, p1.y - p0.y);
                let len2 = dx * dx + dy * dy;
                let t = if len2 > 1e-9 { ((x - p0.x) * dx + (y - p0.y) * dy) / len2 } else { 0.0 };
                Some(self.stop_color(color_stops, extend(t, *e)))
            }
            Brush::RadialGradient { c0, r0, c1, r1, color_stops, extend: e } => {
                // largest t with |p - c(t)| = r(t) and r(t) >= 0
                let (cdx, cdy, dr) = (c1.x - c0.x, c1.y - c0.y, r1 - r0);
                let (pdx, pdy) = (x - c0.x, y - c0.y);
                let a = cdx * cdx + cdy * cdy - dr * dr;
                let b = pdx * cdx + pdy * cdy + r0 * dr;
                let c = pdx * pdx + pdy * pdy - r0 * r0;
                let r_at = |t: f32| r0 + t * dr;
                let t = if a.abs() < 1e-6 {
                    if b.abs() < 1e-9 {
                        return None;
                    }
                    let t = c / (2.0 * b);
                    if r_at(t) < 0.0 {
                        return None;
                    }
                    t
                } else {
                    let disc = b * b - a * c;
                    if disc < 0.0 {
                        return None;
                    }
                    let s = disc.sqrt();
                    let (t1, t2) = ((b + s) / a, (b - s) / a);
                    let (hi, lo) = if t1 > t2 { (t1, t2) } else { (t2, t1) };
                    if r_at(hi) >= 0.0 {
                        hi
                    } else if r_at(lo) >= 0.0 {
                        lo
                    } else {
                        return None;
                    }
                };
                Some(self.stop_color(color_stops, extend(t, *e)))
            }
            Brush::SweepGradient { c0, start_angle, end_angle, color_stops, extend: e } => {
                let mut deg = (y - c0.y).atan2(x - c0.x).to_degrees();
                if deg < 0.0 {
                    deg += 360.0;
                }
                let span = end_angle - start_angle;
                let t = if span.abs() > 1e-6 { (deg - start_angle) / span } else { 0.0 };
                Some(self.stop_color(color_stops, extend(t, *e)))
            }
        }
    }
}

/// Pixel rectangle (x0, y0, x1, y1), end exclusive.
type PxRect = (usize, usize, usize, usize);

fn intersect(a: PxRect, b: PxRect) -> PxRect {
    let r = (a.0.max(b.0), a.1.max(b.1), a.2.min(b.2), a.3.min(b.3));
    if r.0 >= r.2 || r.1 >= r.3 {
        (0, 0, 0, 0)
    } else {
        r
    }
}

struct Painter<'a> {
    outlines: OutlineGlyphCollection<'a>,
    w: u32,
    h: u32,
    colors: Colors<'a>,
    transforms: Vec<Affine>,
    /// Clip coverage stack; the top is the intersection of all clips. A
    /// mask is valid only inside its rectangle (zero outside, whatever the
    /// buffer holds there).
    clips: Vec<(Mask, PxRect)>,
    /// Layer stack; the bottom one is the glyph image.
    layers: Vec<(Pixmap, CompositeMode)>,
    failed: bool,
}

impl Painter<'_> {
    fn ctm(&self) -> Affine {
        self.transforms.last().copied().unwrap_or(Affine::IDENTITY)
    }

    /// Pixel rectangle of a path's bounds, one pixel of margin for
    /// anti-aliasing, clamped to the canvas.
    fn rect_of(&self, path: &Path) -> PxRect {
        let b = path.bounds();
        let (w, h) = (self.w as f32, self.h as f32);
        let x0 = (b.left().floor() - 1.0).clamp(0.0, w) as usize;
        let y0 = (b.top().floor() - 1.0).clamp(0.0, h) as usize;
        let x1 = (b.right().ceil() + 1.0).clamp(0.0, w) as usize;
        let y1 = (b.bottom().ceil() + 1.0).clamp(0.0, h) as usize;
        if x0 >= x1 || y0 >= y1 {
            (0, 0, 0, 0)
        } else {
            (x0, y0, x1, y1)
        }
    }

    fn fill_mask(&self, path: Option<Path>) -> Option<(Mask, PxRect)> {
        let mut mask = Mask::new(self.w, self.h)?;
        let rect = match path {
            Some(path) => {
                mask.fill_path(&path, FillRule::Winding, true, tiny_skia::Transform::identity());
                self.rect_of(&path)
            }
            None => (0, 0, 0, 0),
        };
        Some((mask, rect))
    }

    fn glyph_mask(&self, glyph_id: GlyphId) -> Option<(Mask, PxRect)> {
        let outline = self.outlines.get(glyph_id)?;
        let m = self.ctm();
        let mut pen = Pen { pb: PathBuilder::new(), m: &m };
        outline.draw(DrawSettings::unhinted(Size::unscaled(), LocationRef::default()), &mut pen).ok()?;
        self.fill_mask(pen.pb.finish())
    }

    fn push_mask(&mut self, (mut mask, rect): (Mask, PxRect)) {
        let rect = match self.clips.last() {
            Some((top, top_rect)) => {
                let r = intersect(rect, *top_rect);
                let w = self.w as usize;
                let (d, s) = (mask.data_mut(), top.data());
                for y in r.1..r.3 {
                    for x in r.0..r.2 {
                        let i = y * w + x;
                        d[i] = crate::raster::div255(d[i] as u32 * s[i] as u32) as u8;
                    }
                }
                r
            }
            None => rect,
        };
        self.clips.push((mask, rect));
    }
}

impl ColorPainter for Painter<'_> {
    fn push_transform(&mut self, transform: CTransform) {
        let t = self.ctm().then(&Affine::from(transform));
        self.transforms.push(t);
    }

    fn pop_transform(&mut self) {
        if self.transforms.len() > 1 {
            self.transforms.pop();
        }
    }

    fn push_clip_glyph(&mut self, glyph_id: GlyphId) {
        // an outline that cannot be read clips everything (keeps push/pop balanced)
        match self.glyph_mask(glyph_id).or_else(|| self.fill_mask(None)) {
            Some(m) => self.push_mask(m),
            None => self.failed = true,
        }
    }

    fn push_clip_box(&mut self, b: BoundingBox<f32>) {
        let m = self.ctm();
        let mut pb = PathBuilder::new();
        let corners = [(b.x_min, b.y_min), (b.x_max, b.y_min), (b.x_max, b.y_max), (b.x_min, b.y_max)];
        for (i, &(x, y)) in corners.iter().enumerate() {
            let (x, y) = m.apply(x, y);
            if i == 0 {
                pb.move_to(x, y);
            } else {
                pb.line_to(x, y);
            }
        }
        pb.close();
        match self.fill_mask(pb.finish()) {
            Some(m) => self.push_mask(m),
            None => self.failed = true,
        }
    }

    fn pop_clip(&mut self) {
        self.clips.pop();
    }

    fn fill(&mut self, brush: Brush<'_>) {
        let Some(inv) = self.ctm().invert() else { return };
        let (w, h) = (self.w as usize, self.h as usize);
        let solid = match &brush {
            Brush::Solid { palette_index, alpha } => Some(self.colors.color(*palette_index, *alpha)),
            _ => None,
        };
        let (x0, y0, x1, y1) = self.clips.last().map_or((0, 0, w, h), |c| c.1);
        let clip = self.clips.last().map(|(m, _)| m.data());
        let colors = &self.colors;
        let Some((layer, _)) = self.layers.last_mut() else { return };
        let dst = layer.data_mut().as_chunks_mut::<4>().0;
        for y in y0..y1.min(h) {
            for x in x0..x1.min(w) {
                let cov = clip.map_or(255, |c| c[y * w + x]);
                if cov == 0 {
                    continue;
                }
                let c = match solid {
                    Some(c) => c,
                    None => {
                        let (bx, by) = inv.apply(x as f32 + 0.5, y as f32 + 0.5);
                        match colors.brush_at(&brush, bx, by) {
                            Some(c) => c,
                            None => continue,
                        }
                    }
                };
                let a = c[3] * cov as f32 / 255.0;
                let sa = (a * 255.0 + 0.5) as u32;
                if sa == 0 {
                    continue;
                }
                // premultiplied source over the layer
                let s = [(c[0] * a * 255.0 + 0.5) as u32, (c[1] * a * 255.0 + 0.5) as u32, (c[2] * a * 255.0 + 0.5) as u32, sa];
                let d = &mut dst[y * w + x];
                let inv_a = 255 - sa.min(255);
                for k in 0..4 {
                    d[k] = (s[k] + crate::raster::div255(d[k] as u32 * inv_a)).min(255) as u8;
                }
            }
        }
    }

    fn push_layer(&mut self, composite_mode: CompositeMode) {
        if self.layers.len() >= MAX_LAYERS {
            self.failed = true;
        }
        match Pixmap::new(self.w, self.h) {
            Some(p) => self.layers.push((p, composite_mode)),
            None => self.failed = true,
        }
    }

    fn pop_layer(&mut self) {
        if self.layers.len() < 2 {
            return;
        }
        let Some((top, mode)) = self.layers.pop() else { return };
        let Some((under, _)) = self.layers.last_mut() else { return };
        let paint = PixmapPaint { blend_mode: blend(mode), ..Default::default() };
        under.draw_pixmap(0, 0, top.as_ref(), &paint, tiny_skia::Transform::identity(), None);
    }
}

/// First pass over a paint graph without a clip box: the union of every
/// clip it sets, in font units (a superset of its ink).
struct Extent<'a> {
    outlines: OutlineGlyphCollection<'a>,
    transforms: Vec<Affine>,
    bb: Option<[f32; 4]>,
}

impl Extent<'_> {
    fn ctm(&self) -> Affine {
        self.transforms.last().copied().unwrap_or(Affine::IDENTITY)
    }
}

impl ColorPainter for Extent<'_> {
    fn push_transform(&mut self, transform: CTransform) {
        let t = self.ctm().then(&Affine::from(transform));
        self.transforms.push(t);
    }

    fn pop_transform(&mut self) {
        if self.transforms.len() > 1 {
            self.transforms.pop();
        }
    }

    fn push_clip_glyph(&mut self, glyph_id: GlyphId) {
        let Some(outline) = self.outlines.get(glyph_id) else { return };
        let m = self.ctm();
        let mut pen = BoundsPen { m: &m, bb: &mut self.bb };
        let _ = outline.draw(DrawSettings::unhinted(Size::unscaled(), LocationRef::default()), &mut pen);
    }

    fn push_clip_box(&mut self, b: BoundingBox<f32>) {
        let m = self.ctm();
        let mut pen = BoundsPen { m: &m, bb: &mut self.bb };
        pen.add(b.x_min, b.y_min);
        pen.add(b.x_max, b.y_max);
        pen.add(b.x_min, b.y_max);
        pen.add(b.x_max, b.y_min);
    }

    fn pop_clip(&mut self) {}

    fn fill(&mut self, _brush: Brush<'_>) {}

    fn push_layer(&mut self, _composite_mode: CompositeMode) {}
}

fn blend(m: CompositeMode) -> BlendMode {
    use CompositeMode as C;
    match m {
        C::Clear => BlendMode::Clear,
        C::Src => BlendMode::Source,
        C::Dest => BlendMode::Destination,
        C::DestOver => BlendMode::DestinationOver,
        C::SrcIn => BlendMode::SourceIn,
        C::DestIn => BlendMode::DestinationIn,
        C::SrcOut => BlendMode::SourceOut,
        C::DestOut => BlendMode::DestinationOut,
        C::SrcAtop => BlendMode::SourceAtop,
        C::DestAtop => BlendMode::DestinationAtop,
        C::Xor => BlendMode::Xor,
        C::Plus => BlendMode::Plus,
        C::Screen => BlendMode::Screen,
        C::Overlay => BlendMode::Overlay,
        C::Darken => BlendMode::Darken,
        C::Lighten => BlendMode::Lighten,
        C::ColorDodge => BlendMode::ColorDodge,
        C::ColorBurn => BlendMode::ColorBurn,
        C::HardLight => BlendMode::HardLight,
        C::SoftLight => BlendMode::SoftLight,
        C::Difference => BlendMode::Difference,
        C::Exclusion => BlendMode::Exclusion,
        C::Multiply => BlendMode::Multiply,
        C::HslHue => BlendMode::Hue,
        C::HslSaturation => BlendMode::Saturation,
        C::HslColor => BlendMode::Color,
        C::HslLuminosity => BlendMode::Luminosity,
        _ => BlendMode::SourceOver,
    }
}

/// Whether the font has a COLR table (cheap check before the paint graph
/// lookup).
pub(crate) fn has_colr(font: &cosmic_text::Font) -> bool {
    font.as_swash().table(u32::from_be_bytes(*b"COLR")).is_some_and(|t| t.len() >= 14)
}

/// A glyph outline in pixels: `px` per em, y down, origin at the pen on the
/// baseline moved right by `dx`, slanted by `skew` (x += skew * height).
/// Variable fonts at their regular weight.
pub(crate) fn glyph_path(font: &cosmic_text::Font, face_index: u32, id: u16, px: f32, skew: f32, dx: f32) -> Option<Path> {
    let f = FontRef::from_index(font.data(), face_index).ok()?;
    let outline = f.outline_glyphs().get(GlyphId::new(id as u32))?;
    let loc = f.axes().location([("wght", 400.0f32)]);
    let m = Affine { a: 1.0, b: skew, c: dx, d: 0.0, e: -1.0, f: 0.0 };
    let mut pen = Pen { pb: PathBuilder::new(), m: &m };
    outline.draw(DrawSettings::unhinted(Size::new(px), LocationRef::from(&loc)), &mut pen).ok()?;
    pen.pb.finish()
}

/// Paint a COLR glyph (v1 preferred, else v0), or None when the glyph has
/// no colour paint (then the caller uses bitmaps or the outline).
pub(crate) fn render(font: &cosmic_text::Font, face_index: u32, key: GlyphKey) -> Option<Bitmap> {
    let f = FontRef::from_index(font.data(), face_index).ok()?;
    let glyph = f.color_glyphs().get(GlyphId::new(key.id as u32))?;
    paint(&f, &glyph, key)
}

/// [`render`] of one COLR version (tests compare v0 with swash).
#[cfg(test)]
pub(crate) fn render_format(font: &cosmic_text::Font, face_index: u32, key: GlyphKey, v1: bool) -> Option<Bitmap> {
    let f = FontRef::from_index(font.data(), face_index).ok()?;
    use cosmic_text::skrifa::color::ColorGlyphFormat;
    let format = if v1 { ColorGlyphFormat::ColrV1 } else { ColorGlyphFormat::ColrV0 };
    let glyph = f.color_glyphs().get_with_format(GlyphId::new(key.id as u32), format)?;
    paint(&f, &glyph, key)
}

fn paint(f: &FontRef<'_>, glyph: &ColorGlyph<'_>, key: GlyphKey) -> Option<Bitmap> {
    let upem = f.head().ok()?.units_per_em().max(1) as f32;
    let px = key.px();
    let skew = if key.italic { SKEW } else { 0.0 };
    // extent in font units (y up): the clip box, else the clips the paint
    // sets, else a generous em box
    let fb = match glyph.bounding_box(LocationRef::default(), Size::unscaled()) {
        Some(b) => [b.x_min, b.y_min, b.x_max, b.y_max],
        None => {
            let mut e = Extent { outlines: f.outline_glyphs(), transforms: vec![Affine::IDENTITY], bb: None };
            let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| glyph.paint(LocationRef::default(), &mut e).is_ok())).unwrap_or(false);
            match e.bb {
                Some(bb) if ok => bb,
                _ => [-0.25 * upem, -0.5 * upem, 1.5 * upem, 1.25 * upem],
            }
        }
    };
    if !fb.iter().all(|v| v.is_finite()) || fb[2] < fb[0] || fb[3] < fb[1] {
        return None;
    }
    // the largest side at this size, slant included; over the cap, paint
    // smaller and let the caller scale it up
    let s0 = px / upem;
    let side = s0 * ((fb[2] - fb[0]) + skew * (fb[3] - fb[1])).max(fb[3] - fb[1]);
    let limit = MAX_GLYPH_SIDE as f32 - 3.0;
    let k = if side > limit { limit / side } else { 1.0 };
    let s = s0 * k;
    let bin = if k < 1.0 { 0.0 } else { key.bin as f32 / 4.0 };
    let left = (s * (fb[0] + skew * fb[1]) + bin).floor() as i32;
    let right = (s * (fb[2] + skew * fb[3]) + bin).ceil() as i32;
    let top = (s * fb[3]).ceil() as i32;
    let bottom = (s * fb[1]).floor() as i32;
    let (w, h) = ((right - left).max(0) as usize, (top - bottom).max(0) as usize);
    if w == 0 || h == 0 {
        return Some(Bitmap::empty());
    }
    if w > MAX_GLYPH_SIDE || h > MAX_GLYPH_SIDE {
        return None;
    }
    let palettes = f.color_palettes();
    let palette0 = palettes.get(0);
    let palette: &[Color] = palette0.as_ref().map_or(&[], |p| p.colors());
    let fg = key.fg_rgb().map_or([0.0; 3], |c| c.map(|v| v as f32 / 255.0));
    // font units (y up) to bitmap pixels (y down), slanted for synthetic italic
    let base = Affine { a: s, b: s * skew, c: bin - left as f32, d: 0.0, e: -s, f: top as f32 };
    let image = Pixmap::new(w as u32, h as u32)?;
    let mut p = Painter {
        outlines: f.outline_glyphs(),
        w: w as u32,
        h: h as u32,
        colors: Colors { palette, fg, used_fg: Cell::new(false) },
        transforms: vec![base],
        clips: Vec::new(),
        layers: vec![(image, CompositeMode::SrcOver)],
        failed: false,
    };
    let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| glyph.paint(LocationRef::default(), &mut p).is_ok())).unwrap_or(false);
    if !ok || p.failed {
        return None;
    }
    while p.layers.len() > 1 {
        p.pop_layer();
    }
    let uses_fg = p.colors.used_fg.get();
    let (image, _) = p.layers.pop()?;
    Some(Bitmap { left, top, w, h, color: true, data: image.take(), scale: 1.0 / k, direct: false, uses_fg })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affine_compose_and_invert() {
        let s = Affine { a: 2.0, b: 0.0, c: 1.0, d: 0.0, e: -2.0, f: 10.0 };
        let t = Affine { a: 1.0, b: 0.0, c: 3.0, d: 0.0, e: 1.0, f: 4.0 };
        // first t (translate), then s
        let st = s.then(&t);
        assert_eq!(st.apply(0.0, 0.0), s.apply(3.0, 4.0));
        let inv = st.invert().expect("invertible");
        let (x, y) = st.apply(5.0, 7.0);
        let (bx, by) = inv.apply(x, y);
        assert!((bx - 5.0).abs() < 1e-4 && (by - 7.0).abs() < 1e-4);
        assert!(Affine { a: 0.0, b: 0.0, c: 0.0, d: 0.0, e: 0.0, f: 0.0 }.invert().is_none());
    }

    #[test]
    fn extend_modes() {
        assert_eq!(extend(1.5, Extend::Pad), 1.0);
        assert_eq!(extend(-0.5, Extend::Pad), 0.0);
        assert!((extend(1.25, Extend::Repeat) - 0.25).abs() < 1e-6);
        assert!((extend(1.25, Extend::Reflect) - 0.75).abs() < 1e-6);
        assert!((extend(-0.25, Extend::Reflect) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn foreground_index_uses_the_text_colour() {
        let palette = [Color { red: 10, green: 20, blue: 30, alpha: 255 }];
        let c = Colors { palette: &palette, fg: [1.0, 0.5, 0.0], used_fg: Cell::new(false) };
        assert_eq!(c.color(0, 1.0), [10.0 / 255.0, 20.0 / 255.0, 30.0 / 255.0, 1.0]);
        assert!(!c.used_fg.get());
        assert_eq!(c.color(0xFFFF, 0.5), [1.0, 0.5, 0.0, 0.5]);
        assert!(c.used_fg.get());
        // an index past the palette is black, not the text colour
        let d = Colors { palette: &palette, fg: [1.0; 3], used_fg: Cell::new(false) };
        assert_eq!(d.color(7, 1.0), [0.0, 0.0, 0.0, 1.0]);
        assert!(!d.used_fg.get());
    }

    #[test]
    fn rect_intersection() {
        assert_eq!(intersect((0, 0, 10, 10), (5, 5, 20, 20)), (5, 5, 10, 10));
        assert_eq!(intersect((0, 0, 4, 4), (5, 5, 20, 20)), (0, 0, 0, 0));
    }
}

