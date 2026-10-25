//! The drawable form of a drawing: every model-space entity becomes a group
//! of items (strokes, fills, dots, text runs, block instances) in drawing
//! coordinates with resolved layer and colour. Tiles, `to_paths`, `measure`
//! and picking all read this one form.
//!
//! Blocks are built once, in their own coordinates, and INSERTs (and
//! dimensions) refer to them as instances with a transform. So memory is
//! proportional to the file, not to what the inserts multiply out to: a
//! 20 KB file whose blocks nest 100 inserts deep three times is 300 items
//! plus 100 instances, not 100 million lines. Inside a block, colours and
//! layers that depend on the inserting entity (BYBLOCK, layer 0) stay
//! deferred and are resolved by the walkers through a [`Ctx`].

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::{Arc, OnceLock};

use dxf::entities::{Entity, EntityType};
use dxf::{Block, Color, Drawing};

use crate::aci;
use crate::font::Font;
use crate::geom::{
    bspline, bulge_arc, circle_arc, fit_curve, ocs, p, path_bbox, path_points, transform_path, Aff, BBox, EArc, Path, PathB, Seg, P,
};
use crate::hatch::Hatch;
use crate::text::{mtext_run, text_run, MTextSpec, Metrics, TextRun, TextSpec};

/// A colour after BYLAYER / BYBLOCK resolution. `Fg` is ACI 7: white on a
/// dark background, black on a light one. `ByBlock` and `ByLayer` only occur
/// inside blocks, where they mean the inserting entity's colour and the
/// colour of the inserting entity's layer (for entities on layer 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Col {
    Rgb([u8; 3]),
    Fg,
    ByBlock,
    ByLayer,
}

impl Col {
    /// RGBA with `Fg` (and anything unresolved) as `fg`.
    pub fn rgba(self, fg: [u8; 3], alpha: u8) -> [u8; 4] {
        let c = match self {
            Col::Rgb(c) => c,
            _ => fg,
        };
        [c[0], c[1], c[2], alpha]
    }
    /// Packed 0xRRGGBBAA with ACI 7 as white.
    pub fn packed(self) -> f64 {
        let c = self.rgba([255, 255, 255], 255);
        u32::from_be_bytes(c) as f64
    }
}

/// One instance of a block: its geometry and block → parent transform
/// (the block's base point is already subtracted).
#[derive(Clone, Debug)]
pub struct Inst {
    pub block: Arc<BlockGeom>,
    pub xf: Aff,
}

#[derive(Clone, Debug)]
pub enum Prim {
    /// `width` in drawing units; 0 is a hairline.
    Stroke { path: Path, width: f64 },
    /// Closed subpaths, even-odd.
    Fill { path: Path },
    Dot(P),
    Text(Arc<TextRun>),
    Insert(Box<Inst>),
}

/// `Item::layer` of an item inside a block that is on layer 0: it takes the
/// inserting entity's layer.
pub const PARENT: u32 = u32::MAX;

#[derive(Clone, Debug)]
pub struct Item {
    pub prim: Prim,
    pub color: Col,
    pub alpha: u8,
    /// Index into [`Scene::layers`], or [`PARENT`].
    pub layer: u32,
    /// In the coordinates the item lives in (drawing, or its block's).
    pub bbox: BBox,
}

/// The layer and colour an item's deferred values resolve against: for a
/// top-level entity its own layer and ACI 7; inside an instance the
/// instance's resolved layer and colour.
#[derive(Clone, Copy, Debug)]
pub struct Ctx {
    pub layer: u32,
    /// Always resolved (`Rgb` or `Fg`).
    pub color: Col,
}

impl Ctx {
    pub fn top(layer: u32) -> Ctx {
        Ctx { layer, color: Col::Fg }
    }
    pub fn layer_of(&self, it: &Item) -> u32 {
        if it.layer == PARENT {
            self.layer
        } else {
            it.layer
        }
    }
    /// The item's colour, resolved (`Rgb` or `Fg`).
    pub fn color_of(&self, it: &Item, layers: &[LayerInfo]) -> Col {
        match it.color {
            Col::ByBlock => self.color,
            Col::ByLayer => layers.get(self.layer as usize).map(|l| l.color).unwrap_or(Col::Fg),
            c => c,
        }
    }
    /// The context for the contents of `it` (an instance) seen from here.
    pub fn inner(&self, it: &Item, layers: &[LayerInfo]) -> Ctx {
        Ctx { layer: self.layer_of(it), color: self.color_of(it, layers) }
    }
}

/// A block's geometry in block coordinates, built once and shared by every
/// instance.
#[derive(Debug)]
pub struct BlockGeom {
    pub name: String,
    pub items: Vec<Item>,
    pub bbox: BBox,
    /// Points the block draws when every nested instance is expanded
    /// (f64: nested blocks multiply). For limits and estimates.
    pub expanded_points: f64,
    /// Spatial index over `items`, built on first query (large blocks only).
    index: OnceLock<Grid>,
    /// (length, area) in block coordinates, computed on first measure.
    pub measure: OnceLock<Result<(f64, f64), String>>,
}

/// Blocks with fewer items are scanned instead of indexed.
const INDEX_MIN_ITEMS: usize = 48;

impl BlockGeom {
    /// Indices of items whose box may meet `view` (block coordinates), in
    /// drawing order.
    pub fn candidates(&self, view: &BBox, out: &mut Vec<usize>) {
        out.clear();
        if self.items.len() < INDEX_MIN_ITEMS {
            out.extend((0..self.items.len()).filter(|&i| self.items[i].bbox.intersects(view)));
            return;
        }
        let grid = self.index.get_or_init(|| Grid::build(self.items.iter().map(|it| (it.bbox, false)), self.items.len(), self.bbox));
        grid.query(view, out);
        out.retain(|&i| i < self.items.len() && self.items[i].bbox.intersects(view));
    }
}

/// Everything one top-level entity draws.
#[derive(Clone, Debug)]
pub struct Group {
    pub id: u64,
    pub kind: &'static str,
    /// The top-level entity's own layer (an INSERT's layer hides the whole insert).
    pub layer: u32,
    pub items: Vec<Item>,
    pub bbox: BBox,
    /// Rays and construction lines: not part of the drawing bounds.
    pub infinite: bool,
}

#[derive(Clone, Debug)]
pub struct LayerInfo {
    pub name: String,
    pub color: Col,
    pub aci: i16,
    pub on: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub groups: Vec<Arc<Group>>,
    pub layers: Vec<LayerInfo>,
    pub bounds: BBox,
    pub warnings: Vec<String>,
    /// Built blocks by upper-case name, reused by incremental refreshes.
    pub blocks: HashMap<String, Arc<BlockGeom>>,
    /// Spatial index over `groups`, built on first query after a change.
    index: Arc<OnceLock<Grid>>,
    /// Group index by id, built on first lookup after a change.
    ids: Arc<OnceLock<HashMap<u64, usize>>>,
    /// Layer index by upper-case name, built on first lookup after a change.
    layer_ids: Arc<OnceLock<HashMap<String, u32>>>,
}

/// A uniform grid of item indices over some bounds.
#[derive(Debug)]
pub struct Grid {
    bounds: BBox,
    n: usize,
    cells: Vec<Vec<u32>>,
    /// Items too large for cells (and infinite ones): always candidates.
    big: Vec<u32>,
}

const GRID_N: usize = 128;

impl Grid {
    fn build(boxes: impl Iterator<Item = (BBox, bool)>, count: usize, bounds: BBox) -> Grid {
        let n = ((count as f64).sqrt() as usize).clamp(4, GRID_N);
        let mut g = Grid { bounds, n, cells: vec![Vec::new(); n * n], big: Vec::new() };
        if bounds.is_empty() {
            g.big = (0..count as u32).collect();
            return g;
        }
        for (i, (bbox, infinite)) in boxes.enumerate() {
            match g.range(&bbox) {
                Some((x0, y0, x1, y1)) if !infinite && (x1 - x0 + 1) * (y1 - y0 + 1) <= (n * n / 16).max(1) => {
                    for y in y0..=y1 {
                        for x in x0..=x1 {
                            g.cells[y * n + x].push(i as u32);
                        }
                    }
                }
                Some(_) => g.big.push(i as u32),
                None if infinite => g.big.push(i as u32),
                None => {}
            }
        }
        g
    }

    /// Cell range a box covers (clamped), or None when it is outside.
    fn range(&self, b: &BBox) -> Option<(usize, usize, usize, usize)> {
        if !b.intersects(&self.bounds) {
            return None;
        }
        let (w, h) = (self.bounds.w().max(1e-300), self.bounds.h().max(1e-300));
        let n = self.n as f64;
        let cx = |x: f64| (((x - self.bounds.x0) / w * n).floor().clamp(0.0, n - 1.0)) as usize;
        let cy = |y: f64| (((y - self.bounds.y0) / h * n).floor().clamp(0.0, n - 1.0)) as usize;
        Some((cx(b.x0), cy(b.y0), cx(b.x1), cy(b.y1)))
    }

    /// Sorted, unique indices that may meet `view`.
    fn query(&self, view: &BBox, out: &mut Vec<usize>) {
        let mut v: Vec<u32> = self.big.clone();
        if let Some((x0, y0, x1, y1)) = self.range(view) {
            for y in y0..=y1 {
                for x in x0..=x1 {
                    v.extend_from_slice(&self.cells[y * self.n + x]);
                }
            }
        }
        v.sort_unstable();
        v.dedup();
        out.extend(v.into_iter().map(|i| i as usize));
    }
}

impl Default for BBox {
    fn default() -> Self {
        BBox::EMPTY
    }
}

/// Bytes of geometry one scene may hold (items, paths, text runs). Blocks
/// are stored once, so this bounds what the file itself describes.
#[cfg(target_os = "android")]
pub const MAX_SCENE_BYTES: usize = 256 << 20;
#[cfg(not(target_os = "android"))]
pub const MAX_SCENE_BYTES: usize = 1 << 30;
/// Nesting of blocks inside blocks.
pub const MAX_DEPTH: u32 = 24;
const MAX_HATCH_SEGMENTS: usize = 200_000;

pub type Anchored = Vec<(Option<u64>, Hatch)>;

#[derive(Clone, Copy)]
struct Parent {
    xf: Aff,
    /// [`PARENT`] inside blocks.
    layer: u32,
    /// `Fg` at top level, `ByBlock` inside blocks.
    color: Col,
    /// 0 at top level, 1 inside a block.
    depth: u32,
}

/// Lookup result for a block name.
enum BlockRef {
    Missing,
    /// Found but not drawable here (recursion, nesting too deep, empty).
    Skipped,
    Geom(Arc<BlockGeom>),
}

pub struct Builder<'a> {
    blocks: HashMap<String, &'a Block>,
    block_hatches: &'a HashMap<String, Anchored>,
    /// Blocks built so far (or taken from an earlier scene).
    pub memo: HashMap<String, Arc<BlockGeom>>,
    pub layers: Vec<LayerInfo>,
    layer_idx: HashMap<String, u32>,
    /// Exact spellings seen so far.
    exact: HashMap<String, u32>,
    zero: u32,
    metrics: Metrics<'a>,
    /// Bytes of geometry still allowed.
    budget: usize,
    truncated: bool,
    stack: Vec<String>,
    pub warnings: Vec<String>,
    unsupported: HashMap<&'static str, usize>,
}

fn key(name: &str) -> String {
    name.to_uppercase()
}

fn pt(q: &dxf::Point) -> P {
    p(q.x, q.y)
}

fn normal(v: &dxf::Vector) -> (f64, f64, f64) {
    (v.x, v.y, v.z)
}

pub fn color_of(c: &Color) -> Option<Col> {
    c.index().map(|i| if i == 7 { Col::Fg } else { Col::Rgb(aci::rgb(i)) })
}

/// Raw ACI value of a dxf colour (256 BYLAYER, 0 BYBLOCK).
pub fn aci_value(c: &Color) -> i16 {
    if c.is_by_layer() {
        256
    } else if c.is_by_block() {
        0
    } else {
        c.index().map(|i| i as i16).unwrap_or(256)
    }
}

pub fn entity_kind(e: &EntityType) -> &'static str {
    match e {
        EntityType::Face3D(_) => "3DFACE",
        EntityType::Solid3D(_) => "3DSOLID",
        EntityType::ProxyEntity(_) => "ACAD_PROXY_ENTITY",
        EntityType::Arc(_) => "ARC",
        EntityType::ArcAlignedText(_) => "ARCALIGNEDTEXT",
        EntityType::AttributeDefinition(_) => "ATTDEF",
        EntityType::Attribute(_) => "ATTRIB",
        EntityType::Body(_) => "BODY",
        EntityType::Circle(_) => "CIRCLE",
        EntityType::RotatedDimension(_)
        | EntityType::RadialDimension(_)
        | EntityType::DiameterDimension(_)
        | EntityType::AngularThreePointDimension(_)
        | EntityType::OrdinateDimension(_) => "DIMENSION",
        EntityType::Ellipse(_) => "ELLIPSE",
        EntityType::Helix(_) => "HELIX",
        EntityType::Image(_) => "IMAGE",
        EntityType::Insert(_) => "INSERT",
        EntityType::Leader(_) => "LEADER",
        EntityType::Light(_) => "LIGHT",
        EntityType::Line(_) => "LINE",
        EntityType::LwPolyline(_) => "LWPOLYLINE",
        EntityType::MLine(_) => "MLINE",
        EntityType::MText(_) => "MTEXT",
        EntityType::OleFrame(_) => "OLEFRAME",
        EntityType::Ole2Frame(_) => "OLE2FRAME",
        EntityType::ModelPoint(_) => "POINT",
        EntityType::Polyline(_) => "POLYLINE",
        EntityType::Ray(_) => "RAY",
        EntityType::Region(_) => "REGION",
        EntityType::RText(_) => "RTEXT",
        EntityType::Section(_) => "SECTION",
        EntityType::Seqend(_) => "SEQEND",
        EntityType::Shape(_) => "SHAPE",
        EntityType::Solid(_) => "SOLID",
        EntityType::Spline(_) => "SPLINE",
        EntityType::Text(_) => "TEXT",
        EntityType::Tolerance(_) => "TOLERANCE",
        EntityType::Trace(_) => "TRACE",
        EntityType::DgnUnderlay(_) => "DGNUNDERLAY",
        EntityType::DwfUnderlay(_) => "DWFUNDERLAY",
        EntityType::PdfUnderlay(_) => "PDFUNDERLAY",
        EntityType::Vertex(_) => "VERTEX",
        EntityType::Wipeout(_) => "WIPEOUT",
        EntityType::XLine(_) => "XLINE",
    }
}

fn dim_base(e: &EntityType) -> Option<&dxf::entities::DimensionBase> {
    match e {
        EntityType::RotatedDimension(d) => Some(&d.dimension_base),
        EntityType::RadialDimension(d) => Some(&d.dimension_base),
        EntityType::DiameterDimension(d) => Some(&d.dimension_base),
        EntityType::AngularThreePointDimension(d) => Some(&d.dimension_base),
        EntityType::OrdinateDimension(d) => Some(&d.dimension_base),
        _ => None,
    }
}

/// Is the linear part a scale / mirror / multiple of 90° (boxes map exactly)?
fn axis_aligned(m: &Aff) -> bool {
    (m.b == 0.0 && m.c == 0.0) || (m.a == 0.0 && m.d == 0.0)
}

/// Box of an instance in its parent's coordinates: exact for axis-aligned
/// transforms, otherwise the union of its items' transformed boxes.
fn instance_bbox(g: &BlockGeom, m: &Aff) -> BBox {
    if axis_aligned(m) || g.items.len() > 4096 {
        return g.bbox.transform(m);
    }
    let mut b = BBox::EMPTY;
    for it in &g.items {
        b.union(&it.bbox.transform(m));
    }
    b
}

/// Points an item draws with its instances expanded. A lower estimate:
/// arcs count one point and glyphs four, so twice this is at most the
/// number of values `to_paths` writes for it.
pub fn prim_points(p: &Prim) -> f64 {
    match p {
        Prim::Stroke { path, .. } | Prim::Fill { path } => path_points(path) as f64,
        Prim::Dot(_) => 1.0,
        Prim::Text(run) => run.glyphs.len() as f64 * 4.0,
        Prim::Insert(i) => i.block.expanded_points,
    }
}

impl<'a> Builder<'a> {
    pub fn new(dwg: &'a Drawing, block_hatches: &'a HashMap<String, Anchored>, font: Option<&'a Font>) -> Self {
        Self::with_layers(dwg, block_hatches, font, &[], &HashMap::new())
    }

    /// A builder whose layer indices start as `base` (an existing scene's),
    /// updated from the drawing's layer table, reusing built `blocks`.
    pub fn with_layers(
        dwg: &'a Drawing,
        block_hatches: &'a HashMap<String, Anchored>,
        font: Option<&'a Font>,
        base: &[LayerInfo],
        blocks: &HashMap<String, Arc<BlockGeom>>,
    ) -> Self {
        let mut b = Builder {
            blocks: dwg.blocks().map(|bl| (key(&bl.name), bl)).collect(),
            block_hatches,
            memo: blocks.clone(),
            layers: Vec::new(),
            layer_idx: HashMap::new(),
            exact: HashMap::new(),
            zero: 0,
            metrics: Metrics::new(font),
            budget: MAX_SCENE_BYTES,
            truncated: false,
            stack: Vec::new(),
            warnings: Vec::new(),
            unsupported: HashMap::new(),
        };
        for l in base {
            b.add_layer(&l.name, l.color, l.aci, l.on);
        }
        for l in dwg.layers() {
            let i = b.add_layer(&l.name, Col::Fg, 7, true) as usize;
            b.layers[i] = LayerInfo { name: l.name.clone(), color: color_of(&l.color).unwrap_or(Col::Fg), aci: aci_value(&l.color), on: l.is_layer_on };
        }
        b.zero = b.layer("0");
        b
    }

    fn add_layer(&mut self, name: &str, color: Col, aci: i16, on: bool) -> u32 {
        let k = key(name);
        if let Some(&i) = self.layer_idx.get(&k) {
            return i;
        }
        let i = self.layers.len() as u32;
        self.layers.push(LayerInfo { name: name.to_string(), color, aci, on });
        self.layer_idx.insert(k, i);
        i
    }

    /// The index of a layer, adding unknown names as white, on.
    pub fn layer(&mut self, name: &str) -> u32 {
        let name = if name.is_empty() { "0" } else { name };
        // most names repeat exactly as spelled: skip the case folding then
        if let Some(&i) = self.exact.get(name) {
            return i;
        }
        let i = match self.layer_idx.get(&key(name)) {
            Some(&i) => i,
            None => self.add_layer(name, Col::Fg, 7, true),
        };
        if self.exact.len() < 100_000 {
            self.exact.insert(name.to_string(), i);
        }
        i
    }

    fn resolve(&self, c: &Color, true_color: i32, layer: u32, parent: Col) -> Col {
        if true_color > 0 {
            let v = true_color as u32;
            return Col::Rgb([(v >> 16) as u8, (v >> 8) as u8, v as u8]);
        }
        if c.is_by_layer() {
            if layer == PARENT {
                Col::ByLayer
            } else {
                self.layers.get(layer as usize).map(|l| l.color).unwrap_or(Col::Fg)
            }
        } else if c.is_by_block() {
            parent
        } else {
            color_of(c).unwrap_or(Col::Fg)
        }
    }

    /// Take `bytes` from the geometry budget. The first refusal warns.
    fn charge(&mut self, bytes: usize) -> bool {
        if bytes > self.budget {
            self.budget = 0;
            if !self.truncated {
                self.truncated = true;
                self.warnings.push(format!("drawing too large: stopped at {} MB of geometry; the rest is not drawn", MAX_SCENE_BYTES >> 20));
            }
            return false;
        }
        self.budget -= bytes;
        true
    }

    fn path_cost(path: &[Seg]) -> usize {
        size_of::<Item>() + std::mem::size_of_val(path) + 16
    }

    fn stroke(&mut self, out: &mut Vec<Item>, path: Path, width: f64, color: Col, layer: u32) {
        if path.len() < 2 || !self.charge(Self::path_cost(&path)) {
            return;
        }
        let bbox = path_bbox(&path).inflate(width.max(0.0) * 0.5);
        out.push(Item { prim: Prim::Stroke { path, width }, color, alpha: 255, layer, bbox });
    }

    fn fill(&mut self, out: &mut Vec<Item>, path: Path, color: Col, alpha: u8, layer: u32) {
        if path.len() < 3 || !self.charge(Self::path_cost(&path)) {
            return;
        }
        let bbox = path_bbox(&path);
        out.push(Item { prim: Prim::Fill { path }, color, alpha, layer, bbox });
    }

    fn text(&mut self, out: &mut Vec<Item>, run: Option<TextRun>, color: Col, layer: u32) {
        let Some(run) = run else { return };
        let cost = size_of::<Item>() + size_of::<TextRun>() + std::mem::size_of_val(run.glyphs.as_slice()) + run.text.len() + 48;
        if !self.charge(cost) {
            return;
        }
        let bbox = run.bbox;
        out.push(Item { prim: Prim::Text(Arc::new(run)), color, alpha: 255, layer, bbox });
    }

    fn instance(&mut self, out: &mut Vec<Item>, block: &Arc<BlockGeom>, m: Aff, color: Col, layer: u32) {
        if block.items.is_empty() || !m.finite() || m.det().abs() < 1e-300 || !self.charge(size_of::<Item>() + size_of::<Inst>() + 16) {
            return;
        }
        let bbox = instance_bbox(block, &m);
        out.push(Item { prim: Prim::Insert(Box::new(Inst { block: block.clone(), xf: m })), color, alpha: 255, layer, bbox });
    }

    /// A polyline with bulges and optional widths. `verts` are
    /// (point, bulge, start width, end width) in the entity's coordinates.
    #[allow(clippy::too_many_arguments)]
    fn polyline(&mut self, out: &mut Vec<Item>, verts: &[(P, f64, f64, f64)], closed: bool, const_w: f64, xf: &Aff, color: Col, layer: u32) {
        let n = verts.len();
        if n < 2 && !(n == 1 && closed) {
            return;
        }
        let seg_count = if closed { n } else { n - 1 };
        let scale = xf.mean_scale();
        let varying = const_w <= 0.0 && verts.iter().take(seg_count).any(|v| v.2 > 0.0 || v.3 > 0.0);
        if !varying {
            let mut b = PathB::new();
            b.move_to(verts[0].0);
            for i in 0..seg_count {
                let (a, bulge, _, _) = verts[i];
                let z = verts[(i + 1) % n].0;
                match bulge_arc(a, z, bulge) {
                    Some(arc) => b.arc(arc),
                    None => b.line_to(z),
                }
            }
            if closed {
                b.close();
            }
            let path = transform_path(&b.finish(), xf);
            let w = if const_w > 0.0 { const_w * scale } else { 0.0 };
            self.stroke(out, path, w, color, layer);
            return;
        }
        // per-segment widths: straight segments as trapezoids, arcs as strokes
        for i in 0..seg_count {
            let (a, bulge, sw, ew) = verts[i];
            let z = verts[(i + 1) % n].0;
            if let Some(arc) = bulge_arc(a, z, bulge) {
                let mut b = PathB::new();
                b.arc(arc);
                let w = (sw + ew) * 0.5 * scale;
                self.stroke(out, transform_path(&b.finish(), xf), w, color, layer);
                continue;
            }
            let d = z.sub(a);
            let l = d.len();
            if l < 1e-12 {
                continue;
            }
            let nrm = p(-d.y / l, d.x / l);
            if sw <= 0.0 && ew <= 0.0 {
                let mut b = PathB::new();
                b.move_to(a);
                b.line_to(z);
                self.stroke(out, transform_path(&b.finish(), xf), 0.0, color, layer);
                continue;
            }
            let mut b = PathB::new();
            b.move_to(a.add(nrm.mul(sw * 0.5)));
            b.line_to(z.add(nrm.mul(ew * 0.5)));
            b.line_to(z.sub(nrm.mul(ew * 0.5)));
            b.line_to(a.sub(nrm.mul(sw * 0.5)));
            b.close();
            self.fill(out, transform_path(&b.finish(), xf), color, 255, layer);
        }
    }

    fn solid(&mut self, out: &mut Vec<Item>, q: [P; 4], xf: &Aff, color: Col, layer: u32) {
        let mut b = PathB::new();
        b.move_to(q[0]);
        b.line_to(q[1]);
        b.line_to(q[3]);
        if q[3].dist(q[2]) > 1e-12 {
            b.line_to(q[2]);
        }
        b.close();
        self.fill(out, transform_path(&b.finish(), xf), color, 255, layer);
    }

    fn hatch_items(&mut self, out: &mut Vec<Item>, h: &Hatch, par: &Parent) {
        if !h.visible || h.paper && par.depth == 0 || self.budget == 0 {
            return;
        }
        let own = self.layer(&h.layer);
        let layer = if par.depth > 0 && own == self.zero { par.layer } else { own };
        let c = match h.aci {
            256 => Color::by_layer(),
            0 => Color::by_block(),
            i if (1..=255).contains(&i) => Color::from_index(i as u8),
            _ => Color::by_layer(),
        };
        let color = self.resolve(&c, h.true_color.unwrap_or(0), layer, par.color);
        let xf = par.xf.mul(h.ocs());
        if h.solid || h.gradient || h.lines.is_empty() {
            let path = transform_path(&h.boundary_ocs(), &xf);
            let alpha = if h.solid || h.gradient { 255 } else { 90 };
            self.fill(out, path, color, alpha, layer);
            return;
        }
        match h.pattern_ocs(MAX_HATCH_SEGMENTS) {
            Some(lines) => {
                let path = transform_path(&lines, &xf);
                self.stroke(out, path, 0.0, color, layer);
            }
            None => {
                let path = transform_path(&h.boundary_ocs(), &xf);
                self.fill(out, path, color, 90, layer);
            }
        }
    }

    fn emit(&mut self, e: &Entity, par: &Parent, out: &mut Vec<Item>) {
        let c = &e.common;
        if !c.is_visible || self.budget == 0 {
            return;
        }
        let own = self.layer(&c.layer);
        let layer = if par.depth > 0 && own == self.zero { par.layer } else { own };
        let color = self.resolve(&c.color, c.color_24_bit, layer, par.color);
        let xf = par.xf;
        match &e.specific {
            EntityType::Line(l) => {
                let mut b = PathB::new();
                b.move_to(pt(&l.p1));
                b.line_to(pt(&l.p2));
                self.stroke(out, transform_path(&b.finish(), &xf), 0.0, color, layer);
            }
            EntityType::Circle(ci) => {
                if ci.radius.is_nan() || ci.radius <= 0.0 {
                    return;
                }
                let m = xf.mul(ocs(normal(&ci.normal), ci.center.z));
                let mut b = PathB::new();
                b.arc(circle_arc(pt(&ci.center), ci.radius, 0.0, 360.0));
                b.close();
                self.stroke(out, transform_path(&b.finish(), &m), 0.0, color, layer);
            }
            EntityType::Arc(a) => {
                if a.radius.is_nan() || a.radius <= 0.0 {
                    return;
                }
                let m = xf.mul(ocs(normal(&a.normal), a.center.z));
                let mut b = PathB::new();
                b.arc(circle_arc(pt(&a.center), a.radius, a.start_angle, a.end_angle));
                self.stroke(out, transform_path(&b.finish(), &m), 0.0, color, layer);
            }
            EntityType::Ellipse(el) => {
                if el.minor_axis_ratio.is_nan() || el.minor_axis_ratio <= 0.0 {
                    return;
                }
                let mj = &el.major_axis;
                let nn = &el.normal;
                // minor = (N × M) * ratio
                let minor = p((nn.y * mj.z - nn.z * mj.y) * el.minor_axis_ratio, (nn.z * mj.x - nn.x * mj.z) * el.minor_axis_ratio);
                let (t0, t1) = crate::geom::param_sweep(el.start_parameter, el.end_parameter);
                let arc = EArc { c: pt(&el.center), u: p(mj.x, mj.y), v: minor, t0, t1 };
                let mut b = PathB::new();
                b.arc(arc);
                if (t1 - t0 - std::f64::consts::TAU).abs() < 1e-9 {
                    b.close();
                }
                self.stroke(out, transform_path(&b.finish(), &xf), 0.0, color, layer);
            }
            EntityType::LwPolyline(pl) => {
                let m = xf.mul(ocs(normal(&pl.extrusion_direction), c.elevation));
                let verts: Vec<(P, f64, f64, f64)> = pl.vertices.iter().map(|v| (p(v.x, v.y), v.bulge, v.starting_width, v.ending_width)).collect();
                self.polyline(out, &verts, pl.is_closed(), pl.constant_width, &m, color, layer);
            }
            EntityType::Polyline(pl) => self.old_polyline(pl, &xf, color, layer, out),
            EntityType::Spline(s) => {
                let ctrl: Vec<P> = s.control_points.iter().map(pt).collect();
                let path = if ctrl.len() > s.degree_of_curve.max(1) as usize {
                    bspline(s.degree_of_curve.max(1) as usize, &s.knot_values, &ctrl, &s.weight_values)
                } else {
                    fit_curve(&s.fit_points.iter().map(pt).collect::<Vec<_>>(), s.is_closed())
                };
                match path {
                    Some(path) => self.stroke(out, transform_path(&path, &xf), 0.0, color, layer),
                    None => self.warn_once("SPLINE with invalid data"),
                }
            }
            EntityType::Text(t) => {
                let spec = TextSpec {
                    text: t.value.clone(),
                    height: t.text_height,
                    width_factor: t.relative_x_scale_factor,
                    oblique_deg: t.oblique_angle,
                    rotation_deg: t.rotation,
                    halign: t.horizontal_text_justification as i32,
                    valign: t.vertical_text_justification as i32,
                    p1: pt(&t.location),
                    p2: pt(&t.second_alignment_point),
                    backwards: t.is_text_backwards(),
                    upside_down: t.is_text_upside_down(),
                };
                let m = xf.mul(ocs(normal(&t.normal), t.location.z));
                let run = text_run(&self.metrics, &spec, &m);
                self.text(out, run, color, layer);
            }
            EntityType::Attribute(a) => {
                if !a.is_invisible() {
                    self.attribute(a, &xf, color, layer, out);
                }
            }
            EntityType::AttributeDefinition(a) => {
                if par.depth == 0 && !a.is_invisible() {
                    let spec = TextSpec {
                        text: a.text_tag.clone(),
                        height: a.text_height,
                        width_factor: a.relative_x_scale_factor,
                        oblique_deg: a.oblique_angle,
                        rotation_deg: a.rotation,
                        halign: a.horizontal_text_justification as i32,
                        valign: a.vertical_text_justification as i32,
                        p1: pt(&a.location),
                        p2: pt(&a.second_alignment_point),
                        backwards: false,
                        upside_down: false,
                    };
                    let m = xf.mul(ocs(normal(&a.normal), a.location.z));
                    let run = text_run(&self.metrics, &spec, &m);
                    self.text(out, run, color, layer);
                }
            }
            EntityType::MText(t) => {
                let run = self.mtext(t, &xf);
                self.text(out, run, color, layer);
            }
            EntityType::Insert(ins) => {
                self.insert(ins, &xf, color, layer, out);
            }
            EntityType::ModelPoint(pnt) => {
                let q = xf.apply(pt(&pnt.location));
                if q.finite() && self.charge(size_of::<Item>()) {
                    out.push(Item { prim: Prim::Dot(q), color, alpha: 255, layer, bbox: BBox::new(q.x, q.y, q.x, q.y) });
                }
            }
            EntityType::Solid(s) => {
                let m = xf.mul(ocs(normal(&s.extrusion_direction), s.first_corner.z));
                self.solid(out, [pt(&s.first_corner), pt(&s.second_corner), pt(&s.third_corner), pt(&s.fourth_corner)], &m, color, layer);
            }
            EntityType::Trace(s) => {
                let m = xf.mul(ocs(normal(&s.extrusion_direction), s.first_corner.z));
                self.solid(out, [pt(&s.first_corner), pt(&s.second_corner), pt(&s.third_corner), pt(&s.fourth_corner)], &m, color, layer);
            }
            EntityType::Face3D(f) => {
                let q = [pt(&f.first_corner), pt(&f.second_corner), pt(&f.third_corner), pt(&f.fourth_corner)];
                let hidden = [f.is_first_edge_invisible(), f.is_second_edge_invisible(), f.is_third_edge_invisible(), f.is_fourth_edge_invisible()];
                let mut b = PathB::new();
                for i in 0..4 {
                    let (a, z) = (q[i], q[(i + 1) % 4]);
                    if hidden[i] || a.dist(z) < 1e-12 {
                        continue;
                    }
                    b.move_to(a);
                    b.line_to(z);
                }
                self.stroke(out, transform_path(&b.finish(), &xf), 0.0, color, layer);
            }
            EntityType::Leader(l) => {
                let mut b = PathB::new();
                for (i, v) in l.vertices.iter().enumerate() {
                    if i == 0 {
                        b.move_to(pt(v));
                    } else {
                        b.line_to(pt(v));
                    }
                }
                self.stroke(out, transform_path(&b.finish(), &xf), 0.0, color, layer);
            }
            EntityType::MLine(ml) => {
                let mut b = PathB::new();
                for (i, v) in ml.vertices.iter().enumerate() {
                    if i == 0 {
                        b.move_to(pt(v));
                    } else {
                        b.line_to(pt(v));
                    }
                }
                if ml.flags & 2 != 0 {
                    b.close();
                }
                self.stroke(out, transform_path(&b.finish(), &xf), 0.0, color, layer);
            }
            EntityType::Ray(r) => {
                let s = pt(&r.start_point);
                let d = p(r.unit_direction_vector.x, r.unit_direction_vector.y);
                let far = 1.0e7 * (1.0 + s.x.abs().max(s.y.abs()));
                let mut b = PathB::new();
                b.move_to(s);
                b.line_to(s.add(d.mul(far)));
                self.stroke(out, transform_path(&b.finish(), &xf), 0.0, color, layer);
            }
            EntityType::XLine(r) => {
                let s = pt(&r.first_point);
                let d = p(r.unit_direction_vector.x, r.unit_direction_vector.y);
                let far = 1.0e7 * (1.0 + s.x.abs().max(s.y.abs()));
                let mut b = PathB::new();
                b.move_to(s.sub(d.mul(far)));
                b.line_to(s.add(d.mul(far)));
                self.stroke(out, transform_path(&b.finish(), &xf), 0.0, color, layer);
            }
            EntityType::Image(im) => {
                let o = pt(&im.location);
                let u = p(im.u_vector.x, im.u_vector.y).mul(im.image_size.x);
                let v = p(im.v_vector.x, im.v_vector.y).mul(im.image_size.y);
                let mut b = PathB::new();
                b.move_to(o);
                b.line_to(o.add(u));
                b.line_to(o.add(u).add(v));
                b.line_to(o.add(v));
                b.close();
                self.stroke(out, transform_path(&b.finish(), &xf), 0.0, color, layer);
            }
            other => {
                if dim_base(other).is_some() {
                    self.dimension(e, &xf, color, layer, out);
                } else {
                    *self.unsupported.entry(entity_kind(other)).or_insert(0) += 1;
                }
            }
        }
    }

    fn warn_once(&mut self, msg: &str) {
        if !self.warnings.iter().any(|w| w == msg) {
            self.warnings.push(msg.to_string());
        }
    }

    fn attribute(&mut self, a: &dxf::entities::Attribute, xf: &Aff, color: Col, layer: u32, out: &mut Vec<Item>) {
        let spec = TextSpec {
            text: a.value.clone(),
            height: a.text_height,
            width_factor: a.relative_x_scale_factor,
            oblique_deg: a.oblique_angle,
            rotation_deg: a.rotation,
            halign: a.horizontal_text_justification as i32,
            valign: a.vertical_text_justification as i32,
            p1: pt(&a.location),
            p2: pt(&a.second_alignment_point),
            backwards: false,
            upside_down: false,
        };
        let m = xf.mul(ocs(normal(&a.normal), a.location.z));
        let run = text_run(&self.metrics, &spec, &m);
        self.text(out, run, color, layer);
    }

    fn mtext(&self, t: &dxf::entities::MText, xf: &Aff) -> Option<TextRun> {
        let mut raw = t.extended_text.concat();
        raw.push_str(&t.text);
        let dir = &t.x_axis_direction;
        let default_dir = (dir.x - 1.0).abs() < 1e-12 && dir.y.abs() < 1e-12;
        let rotation = if !default_dir && (dir.x != 0.0 || dir.y != 0.0) { dir.y.atan2(dir.x) } else { t.rotation_angle.to_radians() };
        let spec = MTextSpec {
            raw,
            height: t.initial_text_height,
            insert: pt(&t.insertion_point),
            rotation_rad: rotation,
            attachment: t.attachment_point as i32,
            ref_width: t.reference_rectangle_width,
            line_spacing: t.line_spacing_factor,
        };
        mtext_run(&self.metrics, &spec, xf)
    }

    fn old_polyline(&mut self, pl: &dxf::entities::Polyline, xf: &Aff, color: Col, layer: u32, out: &mut Vec<Item>) {
        let verts: Vec<&dxf::entities::Vertex> = pl.vertices().collect();
        if pl.is_polyface_mesh() {
            let positions: Vec<P> = verts.iter().filter(|v| v.flags & 64 != 0).map(|v| pt(&v.location)).collect();
            let mut b = PathB::new();
            for f in verts.iter().filter(|v| v.flags & 128 != 0 && v.flags & 64 == 0) {
                let idx = [f.polyface_mesh_vertex_index1, f.polyface_mesh_vertex_index2, f.polyface_mesh_vertex_index3, f.polyface_mesh_vertex_index4];
                let used: Vec<i32> = idx.iter().copied().filter(|i| *i != 0).collect();
                for k in 0..used.len() {
                    let (a, z) = (used[k], used[(k + 1) % used.len()]);
                    if a < 0 {
                        continue; // invisible edge
                    }
                    let (Some(&qa), Some(&qz)) = (positions.get(a.unsigned_abs() as usize - 1), positions.get(z.unsigned_abs() as usize - 1)) else { continue };
                    b.move_to(qa);
                    b.line_to(qz);
                }
            }
            self.stroke(out, transform_path(&b.finish(), xf), 0.0, color, layer);
            return;
        }
        if pl.is_3d_polygon_mesh() {
            let (m, n) = (pl.polygon_mesh_m_vertex_count.max(0) as usize, pl.polygon_mesh_n_vertex_count.max(0) as usize);
            let pts: Vec<P> = verts.iter().map(|v| pt(&v.location)).collect();
            if m * n > pts.len() || m == 0 || n == 0 {
                return;
            }
            let (closed_m, closed_n) = (pl.flags & 1 != 0, pl.flags & 32 != 0);
            let mut b = PathB::new();
            for i in 0..m {
                b.move_to(pts[i * n]);
                for j in 1..n {
                    b.line_to(pts[i * n + j]);
                }
                if closed_n {
                    b.line_to(pts[i * n]);
                }
            }
            for j in 0..n {
                b.move_to(pts[j]);
                for i in 1..m {
                    b.line_to(pts[i * n + j]);
                }
                if closed_m {
                    b.line_to(pts[j]);
                }
            }
            self.stroke(out, transform_path(&b.finish(), xf), 0.0, color, layer);
            return;
        }
        // skip spline frame control points
        let used: Vec<&&dxf::entities::Vertex> = verts.iter().filter(|v| v.flags & 16 == 0).collect();
        if pl.is_3d_polyline() {
            let mut b = PathB::new();
            for (i, v) in used.iter().enumerate() {
                if i == 0 {
                    b.move_to(pt(&v.location));
                } else {
                    b.line_to(pt(&v.location));
                }
            }
            if pl.is_closed() {
                b.close();
            }
            self.stroke(out, transform_path(&b.finish(), xf), 0.0, color, layer);
            return;
        }
        let m = xf.mul(ocs(normal(&pl.normal), pl.location.z));
        let data: Vec<(P, f64, f64, f64)> = used
            .iter()
            .map(|v| {
                let sw = if v.starting_width != 0.0 { v.starting_width } else { pl.default_starting_width };
                let ew = if v.ending_width != 0.0 { v.ending_width } else { pl.default_ending_width };
                (pt(&v.location), v.bulge, sw, ew)
            })
            .collect();
        let const_w = if pl.default_starting_width > 0.0
            && pl.default_starting_width == pl.default_ending_width
            && used.iter().all(|v| v.starting_width == 0.0 && v.ending_width == 0.0)
        {
            pl.default_starting_width
        } else {
            0.0
        };
        self.polyline(out, &data, pl.is_closed(), const_w, &m, color, layer);
    }

    /// The built geometry of a block (building it on first use).
    fn block_geom(&mut self, name: &str) -> BlockRef {
        let k = key(name);
        if let Some(g) = self.memo.get(&k) {
            return BlockRef::Geom(g.clone());
        }
        let Some(block) = self.blocks.get(&k).copied() else {
            return BlockRef::Missing;
        };
        if self.stack.len() >= MAX_DEPTH as usize || self.stack.contains(&k) {
            self.warn_once("block nesting too deep or recursive; inner blocks skipped");
            return BlockRef::Skipped;
        }
        self.stack.push(k.clone());
        let par = Parent { xf: Aff::IDENTITY, layer: PARENT, color: Col::ByBlock, depth: 1 };
        let mut items = Vec::new();
        let hatches = self.block_hatches.get(&k);
        let mut by_anchor: HashMap<Option<u64>, Vec<&Hatch>> = HashMap::new();
        if let Some(hs) = hatches {
            for (a, h) in hs {
                by_anchor.entry(*a).or_default().push(h);
            }
        }
        if let Some(hs) = by_anchor.remove(&None) {
            for h in hs {
                self.hatch_items(&mut items, h, &par);
            }
        }
        for e in &block.entities {
            if self.budget == 0 {
                break;
            }
            self.emit(e, &par, &mut items);
            if let Some(hs) = by_anchor.remove(&Some(e.common.handle.0)) {
                for h in hs {
                    self.hatch_items(&mut items, h, &par);
                }
            }
        }
        let mut rest: Vec<(Option<u64>, Vec<&Hatch>)> = by_anchor.into_iter().collect();
        rest.sort_by_key(|(a, _)| *a);
        for (_, hs) in rest {
            for h in hs {
                self.hatch_items(&mut items, h, &par);
            }
        }
        self.stack.pop();
        let mut bbox = BBox::EMPTY;
        let mut points = 0.0;
        for it in &items {
            bbox.union(&it.bbox);
            points += prim_points(&it.prim);
        }
        let g = Arc::new(BlockGeom {
            name: block.name.clone(),
            items,
            bbox,
            expanded_points: points,
            index: OnceLock::new(),
            measure: OnceLock::new(),
        });
        self.memo.insert(k, g.clone());
        BlockRef::Geom(g)
    }

    fn block_base(&self, name: &str) -> P {
        self.blocks.get(&key(name)).map(|b| pt(&b.base_point)).filter(|q| q.finite()).unwrap_or_default()
    }

    fn insert(&mut self, ins: &dxf::entities::Insert, xf: &Aff, color: Col, layer: u32, out: &mut Vec<Item>) {
        let m0 = xf.mul(ocs(normal(&ins.extrusion_direction), ins.location.z));
        let loc = pt(&ins.location);
        let (sx, sy) = (if ins.x_scale_factor != 0.0 { ins.x_scale_factor } else { 1.0 }, if ins.y_scale_factor != 0.0 { ins.y_scale_factor } else { 1.0 });
        let cols = ins.column_count.clamp(1, 1000) as usize;
        let rows = ins.row_count.clamp(1, 1000) as usize;
        if cols * rows > 100_000 {
            self.warn_once("INSERT array larger than 100000 copies; drawn once");
        }
        let (cols, rows) = if cols * rows > 100_000 { (1, 1) } else { (cols, rows) };
        match self.block_geom(&ins.name) {
            BlockRef::Missing => self.warn_once(&format!("INSERT of missing block \"{}\"", ins.name)),
            BlockRef::Skipped => {}
            BlockRef::Geom(g) => {
                let base = self.block_base(&ins.name);
                for r in 0..rows {
                    for c in 0..cols {
                        if self.budget == 0 {
                            break;
                        }
                        let m = m0
                            .mul(Aff::translate(loc.x, loc.y))
                            .mul(Aff::rotate(ins.rotation.to_radians()))
                            .mul(Aff::translate(c as f64 * ins.column_spacing, r as f64 * ins.row_spacing))
                            .mul(Aff::scale(sx, sy))
                            .mul(Aff::translate(-base.x, -base.y));
                        self.instance(out, &g, m, color, layer);
                    }
                }
            }
        }
        // the dxf crate keeps no layer or colour for ATTRIBs: they take the INSERT's
        for a in ins.attributes() {
            if !a.is_invisible() {
                self.attribute(a, xf, color, layer, out);
            }
        }
    }

    fn dimension(&mut self, e: &Entity, xf: &Aff, color: Col, layer: u32, out: &mut Vec<Item>) {
        let Some(d) = dim_base(&e.specific) else { return };
        if !d.block_name.is_empty() {
            match self.block_geom(&d.block_name) {
                BlockRef::Geom(g) => {
                    let base = self.block_base(&d.block_name);
                    self.instance(out, &g, xf.mul(Aff::translate(-base.x, -base.y)), color, layer);
                    return;
                }
                BlockRef::Skipped => return,
                BlockRef::Missing => {}
            }
        }
        // no block: the measurement text at its position
        let text = if d.text.is_empty() || d.text.contains("<>") {
            let v = format!("{:.2}", d.actual_measurement);
            if d.text.is_empty() { v } else { d.text.replace("<>", &v) }
        } else {
            d.text.clone()
        };
        let spec = TextSpec {
            text,
            height: 2.5,
            width_factor: 1.0,
            oblique_deg: 0.0,
            rotation_deg: d.text_rotation_angle,
            halign: 4,
            valign: 0,
            p1: pt(&d.text_mid_point),
            p2: pt(&d.text_mid_point),
            backwards: false,
            upside_down: false,
        };
        let run = text_run(&self.metrics, &spec, xf);
        self.text(out, run, color, layer);
    }

    fn finish_group(&self, id: u64, kind: &'static str, layer: u32, items: Vec<Item>) -> Group {
        let mut bbox = BBox::EMPTY;
        for it in &items {
            bbox.union(&it.bbox);
        }
        Group { id, kind, layer, items, bbox, infinite: matches!(kind, "RAY" | "XLINE") }
    }

    /// The group of one top-level entity (model space only).
    pub fn entity_group(&mut self, e: &Entity) -> Option<Group> {
        if e.common.is_in_paper_space {
            return None;
        }
        let layer = self.layer(&e.common.layer);
        let par = Parent { xf: Aff::IDENTITY, layer, color: Col::Fg, depth: 0 };
        let mut items = Vec::new();
        self.emit(e, &par, &mut items);
        Some(self.finish_group(e.common.handle.0, entity_kind(&e.specific), layer, items))
    }

    pub fn hatch_group(&mut self, h: &Hatch) -> Option<Group> {
        if h.paper {
            return None;
        }
        let layer = self.layer(&h.layer);
        let par = Parent { xf: Aff::IDENTITY, layer, color: Col::Fg, depth: 0 };
        let mut items = Vec::new();
        self.hatch_items(&mut items, h, &par);
        Some(self.finish_group(h.handle, "HATCH", layer, items))
    }

    pub fn take_warnings(&mut self) -> Vec<String> {
        let mut w = std::mem::take(&mut self.warnings);
        let mut un: Vec<_> = self.unsupported.drain().collect();
        un.sort();
        for (k, n) in un {
            w.push(format!("{n} {k} entities not drawn"));
        }
        w
    }
}

/// Build the whole scene in drawing order (hatches after the entity they
/// followed in the file). `None` when `cancelled` said stop.
pub fn build(dwg: &Drawing, hatches: &Anchored, block_hatches: &HashMap<String, Anchored>, font: Option<&Font>, cancelled: &dyn Fn() -> bool) -> Option<Scene> {
    let mut b = Builder::new(dwg, block_hatches, font);
    let mut by_anchor: HashMap<Option<u64>, Vec<&Hatch>> = HashMap::new();
    for (a, h) in hatches {
        by_anchor.entry(*a).or_default().push(h);
    }
    let mut groups: Vec<Arc<Group>> = Vec::new();
    let push_hatches = |b: &mut Builder, groups: &mut Vec<Arc<Group>>, hs: Option<Vec<&Hatch>>| {
        for h in hs.unwrap_or_default() {
            if let Some(g) = b.hatch_group(h) {
                groups.push(Arc::new(g));
            }
        }
    };
    push_hatches(&mut b, &mut groups, by_anchor.remove(&None));
    for (i, e) in dwg.entities().enumerate() {
        if i % 1024 == 1023 && cancelled() {
            return None;
        }
        if let Some(g) = b.entity_group(e) {
            groups.push(Arc::new(g));
        }
        let hs = by_anchor.remove(&Some(e.common.handle.0));
        push_hatches(&mut b, &mut groups, hs);
    }
    let mut rest: Vec<(Option<u64>, Vec<&Hatch>)> = by_anchor.into_iter().collect();
    rest.sort_by_key(|(a, _)| *a);
    for (_, hs) in rest {
        push_hatches(&mut b, &mut groups, Some(hs));
    }
    let warnings = b.take_warnings();
    let blocks = std::mem::take(&mut b.memo);
    let mut scene = Scene { groups, layers: b.layers, bounds: BBox::EMPTY, warnings, blocks, index: Arc::default(), ids: Arc::default(), layer_ids: Arc::default() };
    scene.update_bounds();
    Some(scene)
}

impl Scene {
    /// Recompute the bounds after the groups changed (and drop the indices).
    pub fn update_bounds(&mut self) {
        let mut bb = BBox::EMPTY;
        for g in &self.groups {
            if !g.infinite {
                bb.union(&g.bbox);
            }
        }
        self.bounds = bb;
        self.index = Arc::default();
        self.ids = Arc::default();
        self.layer_ids = Arc::default();
    }

    /// Indices of groups whose box may meet `view`, in drawing order.
    pub fn query(&self, view: &BBox) -> Vec<usize> {
        let grid = self.index.get_or_init(|| Grid::build(self.groups.iter().map(|g| (g.bbox, g.infinite)), self.groups.len(), self.bounds));
        let mut out = Vec::new();
        grid.query(view, &mut out);
        out.retain(|&i| i < self.groups.len());
        out
    }

    /// Position of a group in `groups` by id.
    pub fn group_index(&self, id: u64) -> Option<usize> {
        let ids = self.ids.get_or_init(|| {
            let mut m = HashMap::with_capacity(self.groups.len());
            for (i, g) in self.groups.iter().enumerate() {
                m.entry(g.id).or_insert(i);
            }
            m
        });
        ids.get(&id).copied()
    }

    pub fn group(&self, id: u64) -> Option<&Arc<Group>> {
        self.group_index(id).and_then(|i| self.groups.get(i))
    }

    /// A layer's index by name (any case). Layer names may change only
    /// through [`Scene::update_bounds`] callers (refresh, rebuild).
    pub fn layer_index(&self, name: &str) -> Option<u32> {
        let ids = self.layer_ids.get_or_init(|| {
            let mut m = HashMap::with_capacity(self.layers.len());
            for (i, l) in self.layers.iter().enumerate() {
                m.entry(key(&l.name)).or_insert(i as u32);
            }
            m
        });
        let i = match ids.get(name) {
            Some(&i) => i,
            None => *ids.get(&key(name))?,
        };
        Some(i).filter(|&i| (i as usize) < self.layers.len())
    }

    /// Bounds for display: the drawing bounds, or a unit box when empty.
    pub fn view_bounds(&self) -> BBox {
        if self.bounds.is_empty() {
            BBox::new(0.0, 0.0, 1.0, 1.0)
        } else {
            self.bounds
        }
    }

    /// Points the scene draws with every instance expanded (a lower
    /// estimate for limits, see [`prim_points`]).
    pub fn expanded_points(&self) -> f64 {
        self.groups.iter().flat_map(|g| g.items.iter()).map(|it| prim_points(&it.prim)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested_blocks(levels: usize) -> Drawing {
        let mut d = Drawing::new();
        for b in 0..levels {
            let mut block = Block { name: format!("B{b}"), ..Default::default() };
            for i in 0..100 {
                let e = if b == 0 {
                    EntityType::Line(dxf::entities::Line::new(dxf::Point::new(i as f64, 0.0, 0.0), dxf::Point::new(i as f64, 1.0, 0.0)))
                } else {
                    EntityType::Insert(dxf::entities::Insert {
                        name: format!("B{}", b - 1),
                        location: dxf::Point::new(i as f64 * 200f64.powi(b as i32), 0.0, 0.0),
                        ..Default::default()
                    })
                };
                block.entities.push(Entity::new(e));
            }
            d.add_block(block);
        }
        for i in 0..100 {
            d.add_entity(Entity::new(EntityType::Insert(dxf::entities::Insert {
                name: format!("B{}", levels - 1),
                location: dxf::Point::new(0.0, i as f64 * 10.0, 0.0),
                ..Default::default()
            })));
        }
        d
    }

    #[test]
    fn blocks_are_built_once_and_instanced() {
        let d = nested_blocks(4);
        let t = std::time::Instant::now();
        let s = build(&d, &Vec::new(), &HashMap::new(), None, &|| false).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        // 10^10 lines drawn, 400 items + 100 instances stored
        assert_eq!(s.groups.len(), 100);
        assert_eq!(s.blocks.len(), 4);
        let stored: usize = s.blocks.values().map(|b| b.items.len()).sum::<usize>() + s.groups.iter().map(|g| g.items.len()).sum::<usize>();
        assert_eq!(stored, 500);
        assert!((s.expanded_points() - 2.0e10).abs() < 1.0, "{}", s.expanded_points());
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert!(ms < 2000.0, "{ms} ms");
        // bounds come from the instances: x up to 99 * 200^3 + ... , y up to 990 + 1
        assert!(s.bounds.x1 > 99.0 * 200f64.powi(3) && (s.bounds.y1 - 991.0).abs() < 1e-9, "{:?}", s.bounds);
    }

    #[test]
    fn budget_warns_even_when_it_lands_on_zero() {
        let d = nested_blocks(1);
        let none = HashMap::new();
        let mut b = Builder::new(&d, &none, None);
        b.budget = 10;
        assert!(b.charge(10));
        assert!(!b.charge(1));
        assert!(!b.charge(1));
        assert_eq!(b.warnings.len(), 1, "{:?}", b.warnings);
    }

    #[test]
    fn recursive_blocks_are_skipped() {
        let mut d = Drawing::new();
        let mut a = Block { name: "A".into(), ..Default::default() };
        a.entities.push(Entity::new(EntityType::Insert(dxf::entities::Insert { name: "A".into(), ..Default::default() })));
        a.entities.push(Entity::new(EntityType::Line(dxf::entities::Line::new(dxf::Point::new(0.0, 0.0, 0.0), dxf::Point::new(1.0, 0.0, 0.0)))));
        d.add_block(a);
        d.add_entity(Entity::new(EntityType::Insert(dxf::entities::Insert { name: "A".into(), ..Default::default() })));
        let s = build(&d, &Vec::new(), &HashMap::new(), None, &|| false).unwrap();
        assert_eq!(s.groups.len(), 1);
        assert!(s.warnings.iter().any(|w| w.contains("recursive")), "{:?}", s.warnings);
        assert_eq!(s.blocks["A"].items.len(), 1);
    }
}
