//! Rasterising a scene into tiles or images, and exporting it as vector
//! paths in the shared form.
//!
//! Geometry is transformed to pixels in f64, curves are flattened in pixel
//! space (0.2 px tolerance) with off-screen pieces reduced to chords, and
//! everything is clipped to the target before tiny-skia sees it. So lines
//! stay crisp and coordinates stay small at any zoom, even for survey
//! coordinates in the millions.
//!
//! Block instances are walked with their composed transform; an instance
//! whose box is under a pixel is one plotted pixel, and large blocks are
//! culled through their own spatial index, so deeply nested blocks cost
//! what is visible, with a cap on the work per image.

use std::collections::HashSet;
use std::sync::Arc;

use anypage_runtime::Data;
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::font::{box_glyph, Font};
use crate::geom::{clip_ring, clip_segment, flatten, p, path_bbox, transform_path, Aff, BBox, Path, Seg, P};
use crate::scene::{prim_points, Ctx, Group, Item, Prim, Scene, MAX_DEPTH};
use crate::text::{TextRun, BOX_GLYPH};

pub const TOLERANCE_PX: f64 = 0.2;
const MAX_POINTS_PER_IMAGE: usize = 20_000_000;
/// Items (inside block instances) one image may visit.
const MAX_VISITS_PER_IMAGE: usize = 20_000_000;

#[derive(Clone, Debug)]
pub struct Style {
    /// Per scene layer: drawn or not.
    pub visible: Vec<bool>,
    pub background: [u8; 4],
    /// Colour of ACI 7 (contrasts with the background).
    pub fg: [u8; 3],
    /// Hairline width in pixels.
    pub line_px: f32,
    pub selected: HashSet<u64>,
    pub highlight: [u8; 4],
}

impl Style {
    pub fn new(scene: &Scene, background: [u8; 4]) -> Style {
        Style {
            visible: scene.layers.iter().map(|l| l.on).collect(),
            background,
            fg: contrast(background),
            line_px: 1.0,
            selected: HashSet::new(),
            highlight: [255, 170, 40, 255],
        }
    }
    pub fn layer_on(&self, layer: u32) -> bool {
        self.visible.get(layer as usize).copied().unwrap_or(true)
    }
}

/// White on dark backgrounds, black on light ones (transparent counts as dark).
pub fn contrast(bg: [u8; 4]) -> [u8; 3] {
    let l = (0.2126 * bg[0] as f64 + 0.7152 * bg[1] as f64 + 0.0722 * bg[2] as f64) / 255.0;
    if bg[3] > 0 && l > 0.5 {
        [0, 0, 0]
    } else {
        [255, 255, 255]
    }
}

fn paint(c: [u8; 4]) -> Paint<'static> {
    let mut pt = Paint::default();
    pt.set_color_rgba8(c[0], c[1], c[2], c[3]);
    pt.anti_alias = true;
    pt
}

struct Target<'a> {
    pm: &'a mut Pixmap,
    /// Clip rectangle in pixels (a margin around the target).
    clip: BBox,
    budget: usize,
}

impl Target<'_> {
    fn stroke_polys(&mut self, path: &Path, width_px: f32, color: [u8; 4]) {
        // tiny-skia's anti-aliased hairline path (device width <= 1) draws
        // partly clipped segments about a pixel off near the pixmap edge,
        // which shows as seams between tiles. Strokes just over one pixel
        // go through the stroker and are filled and clipped exactly.
        let width_px = width_px.max(1.001);
        let clip = self.clip.inflate(width_px as f64);
        let polys = flatten(path, TOLERANCE_PX, Some(&clip), &mut self.budget);
        let mut pb = PathBuilder::new();
        let mut any = false;
        for poly in &polys {
            let mut pts = poly.pts.clone();
            if poly.closed && pts.len() > 1 {
                pts.push(pts[0]);
            }
            let mut last: Option<P> = None;
            for w in pts.windows(2) {
                let Some((a, b)) = clip_segment(w[0], w[1], &clip) else {
                    last = None;
                    continue;
                };
                if last.is_none_or(|l| l.dist(a) > 1e-6) {
                    pb.move_to(a.x as f32, a.y as f32);
                }
                pb.line_to(b.x as f32, b.y as f32);
                last = Some(b);
                any = true;
            }
        }
        if !any {
            return;
        }
        if let Some(path) = pb.finish() {
            let wide = width_px > 1.5;
            let stroke = Stroke {
                width: width_px,
                line_cap: if wide { LineCap::Square } else { LineCap::Butt },
                line_join: if wide { LineJoin::Miter } else { LineJoin::Round },
                ..Default::default()
            };
            self.pm.stroke_path(&path, &paint(color), &stroke, Transform::identity(), None);
        }
    }

    fn fill_polys(&mut self, path: &Path, color: [u8; 4], rule: FillRule) {
        let polys = flatten(path, TOLERANCE_PX, Some(&self.clip), &mut self.budget);
        let mut pb = PathBuilder::new();
        let mut any = false;
        for poly in &polys {
            if poly.pts.len() < 3 {
                continue;
            }
            let ring = clip_ring(&poly.pts, &self.clip);
            if ring.len() < 3 {
                continue;
            }
            pb.move_to(ring[0].x as f32, ring[0].y as f32);
            for q in &ring[1..] {
                pb.line_to(q.x as f32, q.y as f32);
            }
            pb.close();
            any = true;
        }
        if !any {
            return;
        }
        if let Some(path) = pb.finish() {
            self.pm.fill_path(&path, &paint(color), rule, Transform::identity(), None);
        }
    }

    /// One pixel, source-over, for items smaller than a pixel (cheap when
    /// an overview holds a hundred thousand of them).
    fn plot(&mut self, q: P, color: [u8; 4]) {
        let (w, h) = (self.pm.width() as i64, self.pm.height() as i64);
        if !q.finite() {
            return;
        }
        let (x, y) = (q.x.floor() as i64, q.y.floor() as i64);
        if x < 0 || y < 0 || x >= w || y >= h {
            return;
        }
        let i = ((y * w + x) * 4) as usize;
        let data = self.pm.data_mut();
        let a = color[3] as u32;
        let inv = 255 - a;
        for k in 0..3 {
            data[i + k] = ((color[k] as u32 * a + data[i + k] as u32 * inv) / 255) as u8;
        }
        data[i + 3] = (a + data[i + 3] as u32 * inv / 255) as u8;
    }

    fn dot(&mut self, q: P, r: f32, color: [u8; 4]) {
        if !self.clip.contains(q) {
            return;
        }
        if let Some(c) = PathBuilder::from_circle(q.x as f32, q.y as f32, r.max(0.5)) {
            self.pm.fill_path(&c, &paint(color), FillRule::Winding, Transform::identity(), None);
        }
    }
}

/// Glyph outlines of a run as one path in output space.
pub fn text_path(run: &TextRun, font: Option<&Font>, m: &Aff, cull: Option<&BBox>) -> Path {
    let mut out: Path = Vec::new();
    let to_out = m.mul(run.xf);
    let fallback = box_glyph(600.0, 700.0);
    let mut cache: Option<(u16, Arc<Path>)> = None;
    for &(gid, gx, gy) in &run.glyphs {
        let outline: Arc<Path> = match (font, gid) {
            (_, BOX_GLYPH) | (None, _) => Arc::new(fallback.clone()),
            (Some(f), g) => match &cache {
                Some((cg, path)) if *cg == g => path.clone(),
                _ => {
                    let path = f.outline(g);
                    cache = Some((g, path.clone()));
                    path
                }
            },
        };
        if outline.is_empty() {
            continue;
        }
        let gm = to_out.mul(Aff::translate(gx, gy));
        let g = transform_path(&outline, &gm);
        if let Some(c) = cull {
            if !path_bbox(&g).intersects(c) {
                continue;
            }
        }
        out.extend(g);
    }
    out
}

/// One leaf item (not an instance) in `color`.
fn draw_leaf(t: &mut Target, it: &Item, xf: &Aff, style: &Style, color: [u8; 4], extra_px: f32, font: Option<&Font>) {
    let bb = it.bbox.transform(xf);
    if !bb.intersects(&t.clip) {
        return;
    }
    let tiny = bb.w() < 0.6 && bb.h() < 0.6;
    match &it.prim {
        _ if tiny => {
            let c = p((bb.x0 + bb.x1) * 0.5, (bb.y0 + bb.y1) * 0.5);
            t.plot(c, [color[0], color[1], color[2], color[3] / 2]);
        }
        Prim::Stroke { path, width } => {
            let w = if *width > 0.0 { ((*width * xf.mean_scale()) as f32).max(style.line_px) } else { style.line_px };
            t.stroke_polys(&transform_path(path, xf), w + extra_px, color);
        }
        Prim::Fill { path } => {
            t.fill_polys(&transform_path(path, xf), color, FillRule::EvenOdd);
            if extra_px > 0.0 {
                t.stroke_polys(&transform_path(path, xf), style.line_px + extra_px, color);
            }
        }
        Prim::Dot(q) => t.dot(xf.apply(*q), 1.5 + extra_px * 0.5, color),
        Prim::Text(run) => {
            let h_px = run.height * xf.mean_scale();
            if h_px < 1.8 {
                // greeked: a faint bar where the text is
                let mut b = crate::geom::PathB::new();
                b.move_to(p(bb.x0, bb.y0));
                b.line_to(p(bb.x1, bb.y0));
                b.line_to(p(bb.x1, bb.y1));
                b.line_to(p(bb.x0, bb.y1));
                b.close();
                t.fill_polys(&b.finish(), [color[0], color[1], color[2], color[3] / 4], FillRule::Winding);
                return;
            }
            let path = text_path(run, font, xf, Some(&t.clip));
            t.fill_polys(&path, color, FillRule::Winding);
        }
        Prim::Insert(_) => {}
    }
}

/// The walk of one image: items and the block instances inside them.
struct Walk<'a, 'b> {
    t: Target<'b>,
    scene: &'a Scene,
    style: &'a Style,
    font: Option<&'a Font>,
    cancelled: &'a dyn Fn() -> bool,
    visits: usize,
    stopped: bool,
    scratch: Vec<Vec<usize>>,
}

impl Walk<'_, '_> {
    /// Draw `it` (drawing → pixels through `m`); `over` replaces every
    /// colour (selection), `extra_px` widens strokes.
    fn item(&mut self, it: &Item, m: &Aff, ctx: Ctx, over: Option<[u8; 4]>, extra_px: f32, depth: u32) {
        if !self.style.layer_on(ctx.layer_of(it)) {
            return;
        }
        let col = ctx.color_of(it, &self.scene.layers);
        let color = over.unwrap_or_else(|| col.rgba(self.style.fg, it.alpha));
        let Prim::Insert(inst) = &it.prim else {
            draw_leaf(&mut self.t, it, m, self.style, color, extra_px, self.font);
            return;
        };
        if self.stopped {
            return;
        }
        let bb = it.bbox.transform(m);
        if !bb.intersects(&self.t.clip) {
            return;
        }
        let inner = ctx.inner(it, &self.scene.layers);
        if bb.w() < 0.6 && bb.h() < 0.6 {
            // the whole instance under a pixel: its first item's colour
            let c = match (over, inst.block.items.first()) {
                (Some(c), _) => c,
                (None, Some(first)) => inner.color_of(first, &self.scene.layers).rgba(self.style.fg, first.alpha),
                (None, None) => color,
            };
            self.t.plot(p((bb.x0 + bb.x1) * 0.5, (bb.y0 + bb.y1) * 0.5), [c[0], c[1], c[2], c[3] / 2]);
            return;
        }
        if depth >= MAX_DEPTH {
            return;
        }
        let mm = m.mul(inst.xf);
        let Some(inv) = mm.inverse() else { return };
        let view = self.t.clip.transform(&inv);
        let mut cand = self.scratch.pop().unwrap_or_default();
        inst.block.candidates(&view, &mut cand);
        for &i in &cand {
            self.visits += 1;
            if self.visits.is_multiple_of(1024) && ((self.cancelled)() || self.visits > MAX_VISITS_PER_IMAGE) {
                self.stopped = true;
            }
            if self.stopped {
                break;
            }
            self.item(&inst.block.items[i], &mm, inner, over, extra_px, depth + 1);
        }
        self.scratch.push(cand);
    }
}

/// Draw `scene` into a `w` x `h` premultiplied RGBA8 image through `xf`
/// (drawing → pixels). `None` when `cancelled` said stop.
pub fn render(scene: &Scene, xf: &Aff, w: u32, h: u32, style: &Style, cancelled: &dyn Fn() -> bool) -> Option<Vec<u8>> {
    let mut pm = Pixmap::new(w.max(1), h.max(1))?;
    let bg = style.background;
    pm.fill(tiny_skia::Color::from_rgba8(bg[0], bg[1], bg[2], bg[3]));
    let font = crate::font::global();
    let font = font.as_deref();
    let margin = 16.0;
    let clip = BBox::new(-margin, -margin, w as f64 + margin, h as f64 + margin);
    let inv = xf.inverse()?;
    let view = clip.transform(&inv);
    let t = Target { pm: &mut pm, clip, budget: MAX_POINTS_PER_IMAGE };
    let mut walk = Walk { t, scene, style, font, cancelled, visits: 0, stopped: false, scratch: Vec::new() };
    let fg = style.fg;
    let mut selected: Vec<&Arc<Group>> = Vec::new();
    for (n, k) in scene.query(&view).into_iter().enumerate() {
        if n % 64 == 0 && cancelled() {
            return None;
        }
        let g = &scene.groups[k];
        if !style.layer_on(g.layer) || !g.bbox.intersects(&view) {
            continue;
        }
        if !style.selected.is_empty() && style.selected.contains(&g.id) {
            selected.push(g);
            continue;
        }
        let ctx = Ctx::top(g.layer);
        // a whole entity under a pixel: one plot (overviews of big drawings)
        let gb = g.bbox.transform(xf);
        if gb.w() < 0.6 && gb.h() < 0.6 {
            if let Some(it) = g.items.iter().find(|it| style.layer_on(ctx.layer_of(it))) {
                let c = ctx.color_of(it, &scene.layers).rgba(fg, it.alpha);
                walk.t.plot(p((gb.x0 + gb.x1) * 0.5, (gb.y0 + gb.y1) * 0.5), [c[0], c[1], c[2], c[3] / 2]);
            }
            continue;
        }
        for it in &g.items {
            if !it.bbox.intersects(&view) {
                continue;
            }
            walk.item(it, xf, ctx, None, 0.0, 0);
        }
        if walk.stopped && cancelled() {
            return None;
        }
    }
    // selection on top, in the highlight colour
    walk.stopped = false;
    walk.visits = 0;
    for g in selected {
        let ctx = Ctx::top(g.layer);
        for it in &g.items {
            if it.bbox.intersects(&view) {
                walk.item(it, xf, ctx, Some(style.highlight), 1.5, 0);
            }
        }
    }
    if cancelled() {
        return None;
    }
    Some(pm.take())
}

/// Uniform fit of drawing rect `src` (y up) into output rect `dst`
/// (x, y, w, h; y down when `flip_y`), centred.
pub fn fit(src: &BBox, dst: (f64, f64, f64, f64), flip_y: bool) -> Aff {
    let (x, y, w, h) = dst;
    let sw = src.w().max(1e-12);
    let sh = src.h().max(1e-12);
    let s = (w / sw).min(h / sh);
    let ox = x + (w - sw * s) * 0.5;
    let oy = y + (h - sh * s) * 0.5;
    if flip_y {
        Aff { a: s, b: 0.0, c: 0.0, d: -s, e: ox - src.x0 * s, f: oy + src.y1 * s }
    } else {
        Aff { a: s, b: 0.0, c: 0.0, d: s, e: ox - src.x0 * s, f: oy - src.y0 * s }
    }
}

// ---------------------------------------------------------------- to_paths

pub struct PathOpts {
    pub xf: Aff,
    pub style: Style,
    /// Only groups touching this drawing rect.
    pub rect: Option<BBox>,
    /// Hairline width in output units.
    pub width: f64,
    pub text: bool,
    pub max_numbers: usize,
}

fn path_data(path: &Path, out: &mut Vec<Data>) {
    for s in path {
        match *s {
            Seg::M(q) => out.extend([Data::str("M"), Data::Num(q.x), Data::Num(q.y)]),
            Seg::L(q) => out.extend([Data::str("L"), Data::Num(q.x), Data::Num(q.y)]),
            Seg::C(a, b, q) => out.extend([Data::str("C"), Data::Num(a.x), Data::Num(a.y), Data::Num(b.x), Data::Num(b.y), Data::Num(q.x), Data::Num(q.y)]),
            Seg::A(arc) => {
                for [a, b, q] in arc.to_cubics(std::f64::consts::FRAC_PI_4) {
                    out.extend([Data::str("C"), Data::Num(a.x), Data::Num(a.y), Data::Num(b.x), Data::Num(b.y), Data::Num(q.x), Data::Num(q.y)]);
                }
            }
            Seg::Z => out.push(Data::str("Z")),
        }
    }
}

fn packed(c: [u8; 4]) -> Data {
    Data::Num(u32::from_be_bytes(c) as f64)
}

struct PathWalk<'a> {
    scene: &'a Scene,
    o: &'a PathOpts,
    font: Option<&'a Font>,
    out: Vec<Data>,
    numbers: usize,
    all_on: bool,
}

impl PathWalk<'_> {
    fn too_many(&self) -> String {
        format!("the drawing needs more than {} path values; pass layers or a smaller rect", self.o.max_numbers)
    }

    fn item(&mut self, it: &Item, m: &Aff, ctx: Ctx, id: u64, depth: u32) -> Result<(), String> {
        let layer = ctx.layer_of(it);
        if !self.o.style.layer_on(layer) {
            return Ok(());
        }
        let col = ctx.color_of(it, &self.scene.layers);
        let color = packed(col.rgba(self.o.style.fg, it.alpha));
        let mut d = Vec::new();
        let mut fields: Vec<(&str, Data)> = Vec::new();
        match &it.prim {
            Prim::Insert(inst) => {
                // two values per point at least (when nothing inside is
                // hidden): refuse early what cannot fit
                if self.all_on && self.numbers as f64 + inst.block.expanded_points * 2.0 > self.o.max_numbers as f64 {
                    return Err(self.too_many());
                }
                if depth >= MAX_DEPTH {
                    return Ok(());
                }
                let mm = m.mul(inst.xf);
                let inner = ctx.inner(it, &self.scene.layers);
                for child in &inst.block.items {
                    self.item(child, &mm, inner, id, depth + 1)?;
                }
                return Ok(());
            }
            Prim::Stroke { path, width } => {
                path_data(&transform_path(path, m), &mut d);
                fields.push(("stroke", color));
                fields.push(("width", Data::Num(if *width > 0.0 { width * m.mean_scale() } else { self.o.width })));
            }
            Prim::Fill { path } => {
                path_data(&transform_path(path, m), &mut d);
                fields.push(("fill", color));
                fields.push(("even_odd", Data::Bool(true)));
            }
            Prim::Dot(q) => {
                let c = m.apply(*q);
                let r = self.o.width.max(1e-6);
                let mut b = crate::geom::PathB::new();
                b.arc(crate::geom::EArc { c, u: p(r, 0.0), v: p(0.0, r), t0: 0.0, t1: std::f64::consts::TAU });
                b.close();
                path_data(&b.finish(), &mut d);
                fields.push(("fill", color));
            }
            Prim::Text(run) => {
                if !self.o.text {
                    return Ok(());
                }
                path_data(&text_path(run, self.font, m, None), &mut d);
                fields.push(("fill", color));
                fields.push(("text", Data::str(&run.text)));
            }
        }
        if d.is_empty() {
            return Ok(());
        }
        self.numbers += d.len();
        if self.numbers > self.o.max_numbers {
            return Err(self.too_many());
        }
        fields.push(("path", Data::list(d)));
        fields.push(("id", Data::Num(id as f64)));
        fields.push(("layer", Data::str(self.scene.layers.get(layer as usize).map(|l| l.name.as_str()).unwrap_or("0"))));
        self.out.push(Data::map(fields));
        Ok(())
    }
}

/// The scene as `{ path, stroke | fill, width, id, layer }` entries.
pub fn to_paths(scene: &Scene, o: &PathOpts) -> Result<Vec<Data>, String> {
    let font = crate::font::global();
    let all_on = o.style.visible.iter().all(|v| *v);
    let mut w = PathWalk { scene, o, font: font.as_deref(), out: Vec::new(), numbers: 0, all_on };
    let wanted = |g: &Group| o.style.layer_on(g.layer) && o.rect.is_none_or(|r| g.bbox.intersects(&r));
    if all_on {
        // refuse before writing anything what cannot fit
        let points: f64 = scene.groups.iter().filter(|g| wanted(g)).flat_map(|g| g.items.iter()).map(|it| prim_points(&it.prim)).sum();
        if points * 2.0 > o.max_numbers as f64 {
            return Err(w.too_many());
        }
    }
    for g in &scene.groups {
        if !o.style.layer_on(g.layer) {
            continue;
        }
        if let Some(r) = &o.rect {
            if !g.bbox.intersects(r) {
                continue;
            }
        }
        let ctx = Ctx::top(g.layer);
        for it in &g.items {
            w.item(it, &o.xf, ctx, g.id, 0)?;
        }
    }
    Ok(w.out)
}
