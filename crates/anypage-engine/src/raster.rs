//! CPU rasterisation for native operators: vector shapes, clips and images
//! through tiny-skia; text through the text system (crates/anypage-text:
//! shaping, bidi, font fallback, UAX #14 line breaking, colour emoji).

use std::time::Instant;

pub use anypage_text::{Direction, LoadOptions as TextLoadOptions, TextSystem};
use tiny_skia::{FillRule, LineCap, LineJoin, Mask, Paint, Path, PathBuilder, Pixmap, PixmapPaint, PixmapRef, Stroke, Transform};

pub type Rgba = [u8; 4];

/// The engine's text system: shaping, bidi, font fallback and line breaking
/// for every script (crates/anypage-text). The old name stays so pages,
/// modules and hosts keep compiling.
pub type TextRaster = TextSystem;

/// Output of a panel operator: premultiplied RGBA8.
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
    /// Production time of the newest stream item drawn, for latency stats.
    pub newest: Option<Instant>,
}

pub const MAX_CANVAS: u32 = 2048;

/// Drawing surface in panel-local world units, rasterised at `scale` px/unit.
pub struct Canvas {
    pm: Pixmap,
    scale: f32,
    /// Clip rectangles in pixels (x0, y0, x1, y1); the effective clip is
    /// the intersection of all of them.
    clips: Vec<(f32, f32, f32, f32)>,
    mask: Option<Mask>,
}

fn paint(c: Rgba) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(c[0], c[1], c[2], c[3]);
    p.anti_alias = true;
    p
}

pub fn round_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    if w <= 0.01 || h <= 0.01 {
        return None; // e.g. a progress bar at 0%: nothing to fill
    }
    let r = r.min(w * 0.5).min(h * 0.5).max(0.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

impl Canvas {
    /// `world_w` x `world_h` units at `scale`, clamped so no side exceeds MAX_CANVAS.
    pub fn new(world_w: f32, world_h: f32, scale: f32) -> Self {
        let mut s = scale.max(0.05);
        let longest = world_w.max(world_h) * s;
        if longest > MAX_CANVAS as f32 {
            s *= MAX_CANVAS as f32 / longest;
        }
        let w = ((world_w * s).round() as u32).clamp(1, MAX_CANVAS);
        let h = ((world_h * s).round() as u32).clamp(1, MAX_CANVAS);
        Self { pm: Pixmap::new(w, h).expect("pixmap"), scale: s, clips: Vec::new(), mask: None }
    }

    /// Restrict drawing to a rectangle (world units), nested inside any
    /// clip already active. Pair with [`Canvas::pop_clip`].
    pub fn push_clip(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let s = self.scale;
        let mut r = (x * s, y * s, (x + w.max(0.0)) * s, (y + h.max(0.0)) * s);
        if let Some(&(a, b, c, d)) = self.clips.last() {
            r = (r.0.max(a), r.1.max(b), r.2.min(c), r.3.min(d));
        }
        self.clips.push(r);
        self.rebuild_mask();
    }

    pub fn pop_clip(&mut self) {
        self.clips.pop();
        self.rebuild_mask();
    }

    fn rebuild_mask(&mut self) {
        self.mask = self.clips.last().and_then(|&(x0, y0, x1, y1)| {
            let mut m = Mask::new(self.pm.width(), self.pm.height())?;
            if x1 > x0 && y1 > y0 {
                let rect = tiny_skia::Rect::from_ltrb(x0, y0, x1, y1)?;
                m.fill_path(&PathBuilder::from_rect(rect), FillRule::Winding, false, Transform::identity());
            }
            Some(m)
        });
    }

    /// Integer pixel bounds glyph blits must stay inside.
    fn clip_px(&self) -> (i32, i32, i32, i32) {
        match self.clips.last() {
            Some(&(x0, y0, x1, y1)) => (x0.floor() as i32, y0.floor() as i32, x1.ceil() as i32, y1.ceil() as i32),
            None => (0, 0, self.pm.width() as i32, self.pm.height() as i32),
        }
    }

    /// Draw a premultiplied RGBA image scaled into a rectangle (world units).
    pub fn image(&mut self, img: &Image, x: f32, y: f32, w: f32, h: f32) {
        let Some(src) = PixmapRef::from_bytes(&img.rgba, img.w, img.h) else { return };
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let s = self.scale;
        let t = Transform::from_row(w * s / img.w as f32, 0.0, 0.0, h * s / img.h as f32, x * s, y * s);
        let paint = PixmapPaint { quality: tiny_skia::FilterQuality::Bilinear, ..Default::default() };
        self.pm.draw_pixmap(0, 0, src, &paint, t, self.mask.as_ref());
    }

    fn t(&self) -> Transform {
        Transform::from_scale(self.scale, self.scale)
    }

    pub fn clear(&mut self, c: Rgba) {
        self.pm.fill(tiny_skia::Color::from_rgba8(c[0], c[1], c[2], c[3]));
    }

    pub fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgba) {
        if let Some(p) = round_rect_path(x, y, w, h, r) {
            let t = self.t();
            self.pm.fill_path(&p, &paint(c), FillRule::Winding, t, self.mask.as_ref());
        }
    }

    pub fn stroke_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, width: f32, c: Rgba) {
        if let Some(p) = round_rect_path(x, y, w, h, r) {
            let t = self.t();
            let s = Stroke { width, ..Default::default() };
            self.pm.stroke_path(&p, &paint(c), &s, t, self.mask.as_ref());
        }
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgba) {
        if let Some(p) = PathBuilder::from_circle(cx, cy, r.max(0.01)) {
            let t = self.t();
            self.pm.fill_path(&p, &paint(c), FillRule::Winding, t, self.mask.as_ref());
        }
    }

    pub fn polyline(&mut self, pts: &[(f32, f32)], width: f32, c: Rgba) {
        if pts.len() < 2 {
            return;
        }
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].0, pts[0].1);
        for p in &pts[1..] {
            pb.line_to(p.0, p.1);
        }
        if let Some(path) = pb.finish() {
            let t = self.t();
            let s = Stroke { width, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() };
            self.pm.stroke_path(&path, &paint(c), &s, t, self.mask.as_ref());
        }
    }

    pub fn fill_under(&mut self, pts: &[(f32, f32)], floor: f32, c: Rgba) {
        if pts.len() < 2 {
            return;
        }
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].0, floor);
        for p in pts {
            pb.line_to(p.0, p.1);
        }
        pb.line_to(pts[pts.len() - 1].0, floor);
        pb.close();
        if let Some(path) = pb.finish() {
            let t = self.t();
            self.pm.fill_path(&path, &paint(c), FillRule::Winding, t, self.mask.as_ref());
        }
    }

    /// Single line of text with its top at `y`. Returns the advance in units.
    pub fn text(&mut self, tr: &TextRaster, x: f32, y: f32, px: f32, c: Rgba, s: &str) -> f32 {
        self.text_dir(tr, x, y, px, c, s, Direction::Auto)
    }

    /// One line in a given direction (a wrapped line keeps its paragraph's).
    #[allow(clippy::too_many_arguments)]
    pub fn text_dir(&mut self, tr: &TextRaster, x: f32, y: f32, px: f32, c: Rgba, s: &str, dir: Direction) -> f32 {
        let sc = self.scale;
        let clip = self.clip_px();
        let (w, h) = (self.pm.width(), self.pm.height());
        tr.draw_line_dir(self.pm.data_mut(), w, h, clip, x * sc, y * sc, px * sc, c, s, dir) / sc
    }

    /// Word-wrapped text inside a box. With `tail`, shows the last lines that
    /// fit (a log that scrolls). Right-to-left paragraphs align right, and
    /// wrapped lines keep their paragraph's direction. Returns the number of
    /// lines laid out.
    #[allow(clippy::too_many_arguments)]
    pub fn text_box(&mut self, tr: &TextRaster, x: f32, y: f32, w: f32, h: f32, px: f32, c: Rgba, s: &str, tail: bool) -> usize {
        let size = px * self.scale;
        let line_h = tr.line_height(size);
        let lines = tr.layout_lines(s, size, w * self.scale);
        let fit = ((h * self.scale) / line_h).floor().max(1.0) as usize;
        let start = if tail && lines.len() > fit { lines.len() - fit } else { 0 };
        for (i, l) in lines.iter().skip(start).take(fit).enumerate() {
            let ly = y + (i as f32 * line_h) / self.scale;
            let lx = if l.dir == Direction::Rtl { x + w - l.width / self.scale } else { x };
            self.text_dir(tr, lx, ly, px, c, &s[l.range.clone()], l.dir);
        }
        lines.len()
    }

    pub fn into_image(self, newest: Option<Instant>) -> Image {
        let (w, h) = (self.pm.width(), self.pm.height());
        Image { w, h, rgba: self.pm.take(), newest }
    }
}
