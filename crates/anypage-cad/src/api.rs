//! The `cad` module as pages see it: functions, jobs and the tile source.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anypage_core::{level_scale, Rect, TileKey, Value, TILE_SIZE};
use anypage_engine::{Image, TileSource};
use anypage_runtime::data::value_to_data;
use anypage_runtime::{Call, Data, Effect, JobCtx, Natives, Res};
use dxf::entities::{Entity, EntityType};

use crate::doc::{aci_color, at_version, CadDoc, NewEnt, State};
use crate::geom::{even_odd_area, flatten, p, path_bbox, path_length, point_in_ring, seg_dist, subpath_areas, transform_path, Aff, BBox, P};
use crate::hatch::{predefined_lines, Boundary, Hatch, Loop};
use crate::render::{contrast, fit, render, to_paths, PathOpts, Style};
use crate::scene::{aci_value, entity_kind, BlockGeom, Col, Ctx, Group, Item, Prim, Scene, MAX_DEPTH};

pub const KIND: &str = "cad.drawing";
const MAX_ENTITIES_OUT: usize = 200_000;
const MAX_POINTS_IN: usize = 1_000_000;
const MAX_TEXT_IN: usize = 100_000;
const DEFAULT_BG: u32 = 0x1B1E24FF;
const MAX_IMAGE_SIDE: f64 = 8192.0;

// ---------------------------------------------------------------- helpers

pub fn doc_of(d: &Data) -> Result<(Arc<CadDoc>, &Res), String> {
    let r = d.as_res().ok_or("expected a drawing (from cad.open or cad.new)")?;
    if r.kind() != KIND {
        return Err(format!("expected a drawing, got {}", r.kind()));
    }
    let doc = r.downcast_arc::<CadDoc>().ok_or("expected a drawing")?;
    Ok((doc, r))
}

fn num(d: &Data, what: &str) -> Result<f64, String> {
    match d.num() {
        Some(x) if x.is_finite() => Ok(x),
        Some(_) => Err(format!("{what} must be a finite number")),
        None => Err(format!("{what} must be a number")),
    }
}

fn num_or(d: &Data, def: f64, what: &str) -> Result<f64, String> {
    if d.is_nil() {
        Ok(def)
    } else {
        num(d, what)
    }
}

/// One id or a list of ids.
pub fn ids_of(d: &Data) -> Result<HashSet<u64>, String> {
    let mut out = HashSet::new();
    let mut add = |x: &Data| -> Result<(), String> {
        let v = num(x, "id")?;
        if v < 0.0 || v.fract() != 0.0 || v > 9.0e15 {
            return Err(format!("{v} is not an entity id"));
        }
        out.insert(v as u64);
        Ok(())
    };
    match d {
        Data::Num(_) => add(d)?,
        Data::List(l) => {
            for x in l.iter() {
                add(x)?;
            }
        }
        Data::Nil => {}
        _ => return Err("ids must be a number or a list of numbers".into()),
    }
    Ok(out)
}

/// `{x, y, w, h}` as a list or a map, in drawing units (x, y = lower left).
pub fn rect_of(d: &Data) -> Result<Option<BBox>, String> {
    let v: Vec<f64> = match d {
        Data::Nil => return Ok(None),
        Data::List(l) => l.iter().filter_map(Data::num).collect(),
        Data::Map(_) => ["x", "y", "w", "h"].iter().filter_map(|k| d.get(k).num()).collect(),
        _ => return Err("rect must be { x, y, w, h }".into()),
    };
    if v.len() != 4 || v.iter().any(|x| !x.is_finite()) || v[2] <= 0.0 || v[3] <= 0.0 || !(v[0] + v[2]).is_finite() || !(v[1] + v[3]).is_finite() {
        return Err("rect must be { x, y, w, h } with finite values and w, h > 0".into());
    }
    Ok(Some(BBox::new(v[0], v[1], v[0] + v[2], v[1] + v[3])))
}

pub fn rect_data(b: &BBox) -> Data {
    Data::nums([b.x0, b.y0, b.w(), b.h()])
}

fn layer_rect(d: &Data) -> Result<(f64, f64, f64, f64), String> {
    let b = rect_of(d)?.ok_or("a rect is required")?;
    Ok((b.x0, b.y0, b.w(), b.h()))
}

/// Flat `{x, y, x, y, ...}` or `{ {x, y}, ... }` / `{ {x = , y = }, ... }`.
fn points_of(d: &Data, what: &str) -> Result<Vec<P>, String> {
    let items = d.items();
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    if matches!(items[0], Data::List(_) | Data::Map(_)) {
        if items.len() > MAX_POINTS_IN {
            return Err(format!("{what}: more than {MAX_POINTS_IN} points"));
        }
        for q in items {
            let (x, y) = match q {
                Data::List(l) => (l.first().cloned().unwrap_or_default(), l.get(1).cloned().unwrap_or_default()),
                _ => (q.get("x").clone(), q.get("y").clone()),
            };
            out.push(p(num(&x, what)?, num(&y, what)?));
        }
    } else {
        if !items.len().is_multiple_of(2) {
            return Err(format!("{what} must be a flat list of x, y pairs"));
        }
        if items.len() / 2 > MAX_POINTS_IN {
            return Err(format!("{what}: more than {MAX_POINTS_IN} points"));
        }
        for [x, y] in items.as_chunks::<2>().0 {
            out.push(p(num(x, what)?, num(y, what)?));
        }
    }
    Ok(out)
}

fn flat(pts: impl IntoIterator<Item = P>) -> Data {
    Data::list(pts.into_iter().flat_map(|q| [Data::Num(q.x), Data::Num(q.y)]).collect())
}

fn nums(v: &[f64]) -> Data {
    Data::nums(v.iter().copied())
}

fn packed_of(d: &Data) -> Option<[u8; 4]> {
    let n = d.num().filter(|n| n.is_finite())?;
    let v = (n as i64 as u64 & 0xFFFF_FFFF) as u32;
    Some(v.to_be_bytes())
}

fn str_list(d: &Data) -> Vec<String> {
    match d {
        Data::Str(s) => vec![s.to_string()],
        Data::List(l) => l.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

/// Visible layers: the drawing's on/off state, or the names listed in
/// `layers` (list), or overrides `{ name = bool }` (map).
fn visibility(scene: &Scene, layers: &Data) -> Vec<bool> {
    let mut v: Vec<bool> = scene.layers.iter().map(|l| l.on).collect();
    match layers {
        // an empty Luau table is an empty list: no override
        Data::List(l) if l.is_empty() => {}
        Data::List(_) | Data::Str(_) => {
            let want: HashSet<String> = str_list(layers).iter().map(|s| s.to_uppercase()).collect();
            for (i, l) in scene.layers.iter().enumerate() {
                v[i] = want.contains(&l.name.to_uppercase());
            }
        }
        Data::Map(_) => {
            for (k, on) in layers.entries() {
                if let Some(i) = scene.layer_index(k) {
                    v[i as usize] = on.truthy();
                }
            }
        }
        _ => {}
    }
    v
}

fn style_of(scene: &Scene, opts: &Data) -> Style {
    let bg = packed_of(opts.get("background")).unwrap_or(DEFAULT_BG.to_be_bytes());
    let mut s = Style::new(scene, bg);
    s.visible = visibility(scene, opts.get("layers"));
    if let Some(fg) = packed_of(opts.get("foreground")) {
        s.fg = [fg[0], fg[1], fg[2]];
    }
    if let Some(w) = opts.get("line_width").num().filter(|w| w.is_finite()) {
        s.line_px = w.clamp(0.25, 16.0) as f32;
    }
    if let Some(h) = packed_of(opts.get("highlight")) {
        s.highlight = h;
    }
    s.selected = ids_of(opts.get("selected")).unwrap_or_default();
    s
}

fn resolve_color(scene: &Scene, e: &Entity) -> Col {
    let c = &e.common;
    if c.color_24_bit > 0 {
        let v = c.color_24_bit as u32;
        return Col::Rgb([(v >> 16) as u8, (v >> 8) as u8, v as u8]);
    }
    if c.color.is_by_layer() {
        return scene.layer_index(&c.layer).and_then(|i| scene.layers.get(i as usize)).map(|l| l.color).unwrap_or(Col::Fg);
    }
    crate::scene::color_of(&c.color).unwrap_or(Col::Fg)
}

// ---------------------------------------------------------------- entities out

thread_local! {
    /// Shared key and type-name strings: `entities` on a large drawing
    /// writes the same dozen keys hundreds of thousands of times.
    static NAMES: std::cell::RefCell<HashMap<&'static str, Arc<str>>> = std::cell::RefCell::new(HashMap::new());
}

fn name(k: &'static str) -> Arc<str> {
    NAMES.with(|m| m.borrow_mut().entry(k).or_insert_with(|| Arc::from(k)).clone())
}

/// `Data::map` with shared key strings.
fn fields_map(f: Vec<(&'static str, Data)>) -> Data {
    Data::Map(Arc::new(f.into_iter().map(|(k, v)| (name(k), v)).collect()))
}

fn common_fields(id: u64, kind: &'static str, layer: &str, color: Col, aci: i16, true_color: i32, group: Option<&Arc<Group>>) -> Vec<(&'static str, Data)> {
    let mut f = vec![
        ("id", Data::Num(id as f64)),
        ("type", Data::Str(name(kind))),
        ("layer", Data::str(layer)),
        ("color", Data::Num(color.packed())),
        ("aci", Data::Num(aci as f64)),
    ];
    if true_color > 0 {
        f.push(("true_color", Data::Num(true_color as f64)));
    }
    if let Some(g) = group.filter(|g| !g.bbox.is_empty()) {
        f.push(("bounds", rect_data(&g.bbox)));
    }
    f
}

fn text_fields(f: &mut Vec<(&'static str, Data)>, loc: &dxf::Point, value: &str, height: f64, rotation: f64) {
    f.push(("x", Data::Num(loc.x)));
    f.push(("y", Data::Num(loc.y)));
    f.push(("text", Data::str(&crate::text::decode_percent(value))));
    f.push(("height", Data::Num(height)));
    f.push(("rotation", Data::Num(rotation)));
}

pub fn entity_data(e: &Entity, scene: &Scene) -> Data {
    let id = e.common.handle.0;
    let kind = entity_kind(&e.specific);
    let group = scene.group(id);
    let mut f = common_fields(id, kind, &e.common.layer, resolve_color(scene, e), aci_value(&e.common.color), e.common.color_24_bit, group);
    if e.common.is_in_paper_space {
        f.push(("space", Data::str("paper")));
    }
    match &e.specific {
        EntityType::Line(l) => {
            f.extend([("x1", Data::Num(l.p1.x)), ("y1", Data::Num(l.p1.y)), ("x2", Data::Num(l.p2.x)), ("y2", Data::Num(l.p2.y))]);
            if l.p1.z != 0.0 || l.p2.z != 0.0 {
                f.extend([("z1", Data::Num(l.p1.z)), ("z2", Data::Num(l.p2.z))]);
            }
        }
        EntityType::Circle(c) => f.extend([("cx", Data::Num(c.center.x)), ("cy", Data::Num(c.center.y)), ("r", Data::Num(c.radius))]),
        EntityType::Arc(a) => f.extend([
            ("cx", Data::Num(a.center.x)),
            ("cy", Data::Num(a.center.y)),
            ("r", Data::Num(a.radius)),
            ("start", Data::Num(a.start_angle)),
            ("end", Data::Num(a.end_angle)),
        ]),
        EntityType::Ellipse(el) => f.extend([
            ("cx", Data::Num(el.center.x)),
            ("cy", Data::Num(el.center.y)),
            ("mx", Data::Num(el.major_axis.x)),
            ("my", Data::Num(el.major_axis.y)),
            ("ratio", Data::Num(el.minor_axis_ratio)),
            ("start", Data::Num(el.start_parameter)),
            ("end", Data::Num(el.end_parameter)),
        ]),
        EntityType::LwPolyline(pl) => {
            f.push(("points", flat(pl.vertices.iter().map(|v| p(v.x, v.y)))));
            f.push(("bulges", Data::nums(pl.vertices.iter().map(|v| v.bulge))));
            f.push(("closed", Data::Bool(pl.is_closed())));
            if pl.constant_width != 0.0 {
                f.push(("width", Data::Num(pl.constant_width)));
            }
            if e.common.elevation != 0.0 {
                f.push(("elevation", Data::Num(e.common.elevation)));
            }
        }
        EntityType::Polyline(pl) => {
            let vs: Vec<&dxf::entities::Vertex> = pl.vertices().filter(|v| v.flags & 16 == 0).collect();
            f.push(("points", flat(vs.iter().map(|v| p(v.location.x, v.location.y)))));
            f.push(("bulges", Data::nums(vs.iter().map(|v| v.bulge))));
            f.push(("closed", Data::Bool(pl.is_closed())));
            f.push(("flags", Data::Num(pl.flags as f64)));
            if pl.is_3d_polyline() {
                f.push(("z", Data::nums(vs.iter().map(|v| v.location.z))));
            }
        }
        EntityType::Spline(s) => {
            f.push(("degree", Data::Num(s.degree_of_curve as f64)));
            f.push(("control", flat(s.control_points.iter().map(|q| p(q.x, q.y)))));
            f.push(("knots", nums(&s.knot_values)));
            if !s.weight_values.is_empty() {
                f.push(("weights", nums(&s.weight_values)));
            }
            if !s.fit_points.is_empty() {
                f.push(("fit", flat(s.fit_points.iter().map(|q| p(q.x, q.y)))));
            }
            f.push(("closed", Data::Bool(s.is_closed())));
        }
        EntityType::Text(t) => {
            text_fields(&mut f, &t.location, &t.value, t.text_height, t.rotation);
            f.push(("halign", Data::Num(t.horizontal_text_justification as i32 as f64)));
            f.push(("valign", Data::Num(t.vertical_text_justification as i32 as f64)));
            if t.horizontal_text_justification as i32 != 0 || t.vertical_text_justification as i32 != 0 {
                f.push(("x2", Data::Num(t.second_alignment_point.x)));
                f.push(("y2", Data::Num(t.second_alignment_point.y)));
            }
            f.push(("style", Data::str(&t.text_style_name)));
        }
        EntityType::Attribute(t) => {
            text_fields(&mut f, &t.location, &t.value, t.text_height, t.rotation);
            f.push(("tag", Data::str(&t.attribute_tag)));
        }
        EntityType::AttributeDefinition(t) => {
            text_fields(&mut f, &t.location, &t.value, t.text_height, t.rotation);
            f.push(("tag", Data::str(&t.text_tag)));
            f.push(("prompt", Data::str(&t.prompt)));
        }
        EntityType::MText(t) => {
            let mut raw = t.extended_text.concat();
            raw.push_str(&t.text);
            f.push(("x", Data::Num(t.insertion_point.x)));
            f.push(("y", Data::Num(t.insertion_point.y)));
            f.push(("text", Data::str(&crate::text::decode_mtext(&raw).join("\n"))));
            f.push(("raw", Data::str(&raw)));
            f.push(("height", Data::Num(t.initial_text_height)));
            f.push(("width", Data::Num(t.reference_rectangle_width)));
            f.push(("rotation", Data::Num(t.rotation_angle)));
            f.push(("attachment", Data::Num(t.attachment_point as i32 as f64)));
        }
        EntityType::Insert(ins) => {
            f.push(("block", Data::str(&ins.name)));
            f.push(("x", Data::Num(ins.location.x)));
            f.push(("y", Data::Num(ins.location.y)));
            f.push(("sx", Data::Num(ins.x_scale_factor)));
            f.push(("sy", Data::Num(ins.y_scale_factor)));
            f.push(("rotation", Data::Num(ins.rotation)));
            if ins.column_count > 1 || ins.row_count > 1 {
                f.push(("cols", Data::Num(ins.column_count as f64)));
                f.push(("rows", Data::Num(ins.row_count as f64)));
                f.push(("col_spacing", Data::Num(ins.column_spacing)));
                f.push(("row_spacing", Data::Num(ins.row_spacing)));
            }
            let atts: Vec<Data> = ins.attributes().map(|a| Data::map([("tag", Data::str(&a.attribute_tag)), ("value", Data::str(&a.value))])).collect();
            if !atts.is_empty() {
                f.push(("attributes", Data::list(atts)));
            }
        }
        EntityType::ModelPoint(q) => f.extend([("x", Data::Num(q.location.x)), ("y", Data::Num(q.location.y))]),
        EntityType::Solid(s) => f.push(("points", flat([&s.first_corner, &s.second_corner, &s.third_corner, &s.fourth_corner].map(|q| p(q.x, q.y))))),
        EntityType::Trace(s) => f.push(("points", flat([&s.first_corner, &s.second_corner, &s.third_corner, &s.fourth_corner].map(|q| p(q.x, q.y))))),
        EntityType::Face3D(s) => f.push(("points", flat([&s.first_corner, &s.second_corner, &s.third_corner, &s.fourth_corner].map(|q| p(q.x, q.y))))),
        EntityType::Leader(l) => f.push(("points", flat(l.vertices.iter().map(|q| p(q.x, q.y))))),
        EntityType::MLine(l) => f.push(("points", flat(l.vertices.iter().map(|q| p(q.x, q.y))))),
        EntityType::Ray(r) => f.extend([
            ("x", Data::Num(r.start_point.x)),
            ("y", Data::Num(r.start_point.y)),
            ("dx", Data::Num(r.unit_direction_vector.x)),
            ("dy", Data::Num(r.unit_direction_vector.y)),
        ]),
        EntityType::XLine(r) => f.extend([
            ("x", Data::Num(r.first_point.x)),
            ("y", Data::Num(r.first_point.y)),
            ("dx", Data::Num(r.unit_direction_vector.x)),
            ("dy", Data::Num(r.unit_direction_vector.y)),
        ]),
        other => {
            let base = match other {
                EntityType::RotatedDimension(d) => Some((&d.dimension_base, "rotated")),
                EntityType::RadialDimension(d) => Some((&d.dimension_base, "radial")),
                EntityType::DiameterDimension(d) => Some((&d.dimension_base, "diameter")),
                EntityType::AngularThreePointDimension(d) => Some((&d.dimension_base, "angular")),
                EntityType::OrdinateDimension(d) => Some((&d.dimension_base, "ordinate")),
                _ => None,
            };
            if let Some((b, k)) = base {
                f.push(("dimension", Data::str(k)));
                f.push(("block", Data::str(&b.block_name)));
                f.push(("text", Data::str(&b.text)));
                f.push(("measurement", Data::Num(b.actual_measurement)));
                f.push(("x", Data::Num(b.text_mid_point.x)));
                f.push(("y", Data::Num(b.text_mid_point.y)));
            }
        }
    }
    fields_map(f)
}

pub fn hatch_data(h: &Hatch, scene: &Scene) -> Data {
    let group = scene.group(h.handle);
    let layer_col = scene.layer_index(&h.layer).and_then(|i| scene.layers.get(i as usize)).map(|l| l.color).unwrap_or(Col::Fg);
    let color = match (h.true_color, h.aci) {
        (Some(tc), _) if tc > 0 => Col::Rgb([(tc >> 16) as u8, (tc >> 8) as u8, tc as u8]),
        (_, 1..=255) => crate::scene::color_of(&aci_color(h.aci)).unwrap_or(Col::Fg),
        (_, 0) => Col::Fg,
        _ => layer_col,
    };
    let mut f = common_fields(h.handle, "HATCH", &h.layer, color, h.aci, h.true_color.unwrap_or(0), group);
    f.push(("solid", Data::Bool(h.solid)));
    f.push(("pattern", Data::str(&h.pattern)));
    if !h.solid {
        f.push(("angle", Data::Num(h.angle)));
        f.push(("scale", Data::Num(h.scale)));
    }
    let m = h.ocs();
    let mut budget = 200_000usize;
    let path = crate::geom::transform_path(&h.boundary_ocs(), &m);
    let bb = crate::geom::path_bbox(&path);
    let tol = (bb.w().hypot(bb.h()) * 1e-4).max(1e-9);
    let loops: Vec<Data> = flatten(&path, tol, None, &mut budget).into_iter().map(|poly| flat(poly.pts)).collect();
    f.push(("loops", Data::list(loops)));
    if h.paper {
        f.push(("space", Data::str("paper")));
    }
    fields_map(f)
}

// ---------------------------------------------------------------- entities in

/// A packed 0xRRGGBBAA colour given by a page for an entity: a whole
/// number from 0 to 0xFFFFFFFF.
fn packed_color(d: &Data) -> Result<[u8; 4], String> {
    let v = num(d, "color")?;
    if v < 0.0 || v > u32::MAX as f64 || v.fract() != 0.0 {
        return Err(format!("color {v} is not a packed 0xRRGGBBAA colour"));
    }
    Ok((v as u32).to_be_bytes())
}

/// An ACI number: a whole number from 0 (BYBLOCK) to 256 (BYLAYER).
fn aci_number(v: f64) -> Result<i16, String> {
    if !(0.0..=256.0).contains(&v) || v.fract() != 0.0 {
        return Err(format!("{v} is not an ACI colour: use 0 (BYBLOCK) to 256 (BYLAYER), or {{ color = 0xRRGGBBAA }} for a true colour"));
    }
    Ok(v as i16)
}

fn color_fields(d: &Data) -> Result<(i16, Option<i32>), String> {
    if !d.get("color").is_nil() {
        let c = packed_color(d.get("color"))?;
        let tc = ((c[0] as i32) << 16) | ((c[1] as i32) << 8) | c[2] as i32;
        return Ok((crate::aci::nearest([c[0], c[1], c[2]]) as i16, Some(tc.max(1))));
    }
    match d.get("aci") {
        Data::Nil => Ok((256, None)),
        x => Ok((aci_number(num(x, "aci")?)?, None)),
    }
}

fn s_of(d: &Data, key: &str, def: &str) -> Result<String, String> {
    match d.get(key) {
        Data::Nil => Ok(def.to_string()),
        Data::Str(s) if s.chars().count() <= MAX_TEXT_IN => Ok(s.to_string()),
        Data::Str(_) => Err(format!("{key} is longer than {MAX_TEXT_IN} characters")),
        _ => Err(format!("{key} must be a string")),
    }
}

fn dp(x: f64, y: f64) -> dxf::Point {
    dxf::Point::new(x, y, 0.0)
}

fn halign(d: &Data) -> Result<i32, String> {
    Ok(match d {
        Data::Nil => 0,
        Data::Str(s) => match s.to_ascii_lowercase().as_str() {
            "left" => 0,
            "center" | "centre" => 1,
            "right" => 2,
            "aligned" => 3,
            "middle" => 4,
            "fit" => 5,
            other => return Err(format!("halign \"{other}\" (left, center, right, aligned, middle, fit)")),
        },
        x => num(x, "halign")?.clamp(0.0, 5.0) as i32,
    })
}

fn valign(d: &Data) -> Result<i32, String> {
    Ok(match d {
        Data::Nil => 0,
        Data::Str(s) => match s.to_ascii_lowercase().as_str() {
            "baseline" => 0,
            "bottom" => 1,
            "middle" => 2,
            "top" => 3,
            other => return Err(format!("valign \"{other}\" (baseline, bottom, middle, top)")),
        },
        x => num(x, "valign")?.clamp(0.0, 3.0) as i32,
    })
}

/// An entity from its Luau description (the same keys `entities` returns).
pub fn entity_from_data(d: &Data) -> Result<NewEnt, String> {
    let kind = d.get("type").as_str().ok_or("entity needs a type (\"LINE\", \"CIRCLE\", ...)")?.to_ascii_uppercase();
    let layer = s_of(d, "layer", "0")?;
    let (aci, tc) = color_fields(d)?;
    let n = |k: &str| num(d.get(k), k);
    let n_or = |k: &str, def: f64| num_or(d.get(k), def, k);
    let specific = match kind.as_str() {
        "LINE" => EntityType::Line(dxf::entities::Line::new(
            dxf::Point::new(n("x1")?, n("y1")?, n_or("z1", 0.0)?),
            dxf::Point::new(n("x2")?, n("y2")?, n_or("z2", 0.0)?),
        )),
        "CIRCLE" => {
            let r = n("r")?;
            if r <= 0.0 {
                return Err("r must be > 0".into());
            }
            EntityType::Circle(dxf::entities::Circle::new(dp(n("cx")?, n("cy")?), r))
        }
        "ARC" => {
            let r = n("r")?;
            if r <= 0.0 {
                return Err("r must be > 0".into());
            }
            EntityType::Arc(dxf::entities::Arc::new(dp(n("cx")?, n("cy")?), r, n("start")?, n("end")?))
        }
        "ELLIPSE" => {
            let ratio = n("ratio")?;
            if !(ratio > 0.0 && ratio <= 1.0) {
                return Err("ratio must be in (0, 1]".into());
            }
            let el = dxf::entities::Ellipse {
                center: dp(n("cx")?, n("cy")?),
                major_axis: dxf::Vector::new(n("mx")?, n("my")?, 0.0),
                minor_axis_ratio: ratio,
                start_parameter: n_or("start", 0.0)?,
                end_parameter: n_or("end", std::f64::consts::TAU)?,
                ..Default::default()
            };
            EntityType::Ellipse(el)
        }
        "LWPOLYLINE" | "POLYLINE" => {
            let pts = points_of(d.get("points"), "points")?;
            if pts.len() < 2 {
                return Err("points needs at least 2 points".into());
            }
            let bulges: Vec<f64> = d.get("bulges").items().iter().map(|b| b.num().filter(|x| x.is_finite()).unwrap_or(0.0)).collect();
            let mut pl = dxf::entities::LwPolyline {
                constant_width: n_or("width", 0.0)?.max(0.0),
                vertices: pts
                    .iter()
                    .enumerate()
                    .map(|(i, q)| dxf::LwPolylineVertex { x: q.x, y: q.y, bulge: bulges.get(i).copied().unwrap_or(0.0), ..Default::default() })
                    .collect(),
                ..Default::default()
            };
            pl.set_is_closed(d.get("closed").truthy());
            EntityType::LwPolyline(pl)
        }
        "SPLINE" => {
            let ctrl = points_of(d.get("control"), "control")?;
            let fitp = points_of(d.get("fit"), "fit")?;
            if ctrl.len() < 2 && fitp.len() < 2 {
                return Err("SPLINE needs control or fit points".into());
            }
            let degree = n_or("degree", 3.0)?.clamp(1.0, 11.0) as usize;
            let mut knots: Vec<f64> = d.get("knots").items().iter().filter_map(Data::num).collect();
            if !ctrl.is_empty() && knots.len() != ctrl.len() + degree + 1 {
                if !knots.is_empty() {
                    return Err(format!("knots must have {} values (control points + degree + 1)", ctrl.len() + degree + 1));
                }
                let inner = ctrl.len().saturating_sub(degree);
                knots = (0..ctrl.len() + degree + 1).map(|i| (i.saturating_sub(degree)).min(inner) as f64).collect();
            }
            let weights: Vec<f64> = d.get("weights").items().iter().filter_map(Data::num).collect();
            let mut s = dxf::entities::Spline {
                degree_of_curve: degree as i32,
                knot_values: knots,
                weight_values: if weights.len() == ctrl.len() { weights } else { Vec::new() },
                control_points: ctrl.iter().map(|q| dp(q.x, q.y)).collect(),
                fit_points: fitp.iter().map(|q| dp(q.x, q.y)).collect(),
                ..Default::default()
            };
            s.set_is_closed(d.get("closed").truthy());
            EntityType::Spline(s)
        }
        "TEXT" => {
            let h = halign(d.get("halign"))?;
            let v = valign(d.get("valign"))?;
            let (x, y) = (n("x")?, n("y")?);
            let t = dxf::entities::Text {
                location: dp(x, y),
                second_alignment_point: dp(n_or("x2", x)?, n_or("y2", y)?),
                value: s_of(d, "text", "")?,
                text_height: n_or("height", 2.5)?.max(1e-9),
                rotation: n_or("rotation", 0.0)?,
                horizontal_text_justification: enum_h(h),
                vertical_text_justification: enum_v(v),
                ..Default::default()
            };
            EntityType::Text(t)
        }
        "MTEXT" => {
            let t = dxf::entities::MText {
                insertion_point: dp(n("x")?, n("y")?),
                text: s_of(d, "text", "")?,
                initial_text_height: n_or("height", 2.5)?.max(1e-9),
                reference_rectangle_width: n_or("width", 0.0)?.max(0.0),
                rotation_angle: n_or("rotation", 0.0)?,
                attachment_point: enum_attach(n_or("attachment", 1.0)? as i32),
                ..Default::default()
            };
            EntityType::MText(t)
        }
        "INSERT" => {
            let name = s_of(d, "block", "")?;
            if name.is_empty() {
                return Err("INSERT needs a block name".into());
            }
            let mut ins = dxf::entities::Insert {
                name,
                location: dp(n("x")?, n("y")?),
                x_scale_factor: n_or("sx", 1.0)?,
                y_scale_factor: n_or("sy", n_or("sx", 1.0)?)?,
                rotation: n_or("rotation", 0.0)?,
                column_count: n_or("cols", 1.0)?.clamp(1.0, 1000.0) as i16,
                row_count: n_or("rows", 1.0)?.clamp(1.0, 1000.0) as i16,
                column_spacing: n_or("col_spacing", 0.0)?,
                row_spacing: n_or("row_spacing", 0.0)?,
                ..Default::default()
            };
            for a in d.get("attributes").items() {
                let att = dxf::entities::Attribute {
                    attribute_tag: s_of(a, "tag", "")?,
                    value: s_of(a, "value", "")?,
                    location: dp(num_or(a.get("x"), ins.location.x, "x")?, num_or(a.get("y"), ins.location.y, "y")?),
                    text_height: num_or(a.get("height"), 2.5, "height")?.max(1e-9),
                    ..Default::default()
                };
                ins.__has_attributes = true;
                ins.__attributes_and_handles.push((att, dxf::Handle(0xFFFF_FFFF_FFFF_FFFF)));
            }
            EntityType::Insert(ins)
        }
        "POINT" => EntityType::ModelPoint(dxf::entities::ModelPoint::new(dp(n("x")?, n("y")?))),
        "SOLID" => {
            let pts = points_of(d.get("points"), "points")?;
            if !(3..=4).contains(&pts.len()) {
                return Err("SOLID needs 3 or 4 points".into());
            }
            let q4 = pts.get(3).copied().unwrap_or(pts[2]);
            EntityType::Solid(dxf::entities::Solid::new(dp(pts[0].x, pts[0].y), dp(pts[1].x, pts[1].y), dp(pts[2].x, pts[2].y), dp(q4.x, q4.y)))
        }
        "HATCH" => {
            let mut loops = Vec::new();
            let bulge_sets = d.get("bulges").items();
            for (k, l) in d.get("loops").items().iter().enumerate() {
                let pts = points_of(l, "loops")?;
                if pts.len() < 3 {
                    return Err("each hatch loop needs at least 3 points".into());
                }
                let bulges: Vec<f64> = bulge_sets.get(k).map(|b| b.items().iter().map(|x| x.num().unwrap_or(0.0)).collect()).unwrap_or_default();
                let flags = if k == 0 { 1 | 2 } else { 2 };
                loops.push(Loop {
                    flags,
                    boundary: Boundary::Poly { closed: true, pts: pts.iter().enumerate().map(|(i, q)| (*q, bulges.get(i).copied().unwrap_or(0.0))).collect() },
                });
            }
            if loops.is_empty() {
                return Err("HATCH needs loops = { {x, y, ...}, ... }".into());
            }
            let pattern = s_of(d, "pattern", "SOLID")?.to_ascii_uppercase();
            let solid = d.get("solid").as_bool().unwrap_or(pattern == "SOLID");
            let angle = n_or("angle", 0.0)?;
            let scale = n_or("scale", 1.0)?;
            let h = Hatch {
                layer,
                aci,
                true_color: tc,
                pattern: if solid { "SOLID".into() } else { pattern.clone() },
                solid,
                loops,
                angle,
                scale,
                lines: if solid { Vec::new() } else { predefined_lines(&pattern, angle, scale) },
                seeds: Vec::new(),
                ..Hatch::default()
            };
            return Ok(NewEnt::Hatch(Box::new(h)));
        }
        other => return Err(format!("cannot add a {other} (LINE, CIRCLE, ARC, ELLIPSE, LWPOLYLINE, SPLINE, TEXT, MTEXT, INSERT, POINT, SOLID, HATCH)")),
    };
    let mut e = Entity::new(specific);
    e.common.layer = if layer.is_empty() { "0".into() } else { layer };
    e.common.color = aci_color(aci);
    e.common.color_24_bit = tc.unwrap_or(0);
    Ok(NewEnt::Dxf(Box::new(e)))
}

fn enum_h(v: i32) -> dxf::enums::HorizontalTextJustification {
    use dxf::enums::HorizontalTextJustification as H;
    match v {
        1 => H::Center,
        2 => H::Right,
        3 => H::Aligned,
        4 => H::Middle,
        5 => H::Fit,
        _ => H::Left,
    }
}

fn enum_v(v: i32) -> dxf::enums::VerticalTextJustification {
    use dxf::enums::VerticalTextJustification as V;
    match v {
        1 => V::Bottom,
        2 => V::Middle,
        3 => V::Top,
        _ => V::Baseline,
    }
}

fn enum_attach(v: i32) -> dxf::enums::AttachmentPoint {
    use dxf::enums::AttachmentPoint as A;
    match v {
        2 => A::TopCenter,
        3 => A::TopRight,
        4 => A::MiddleLeft,
        5 => A::MiddleCenter,
        6 => A::MiddleRight,
        7 => A::BottomLeft,
        8 => A::BottomCenter,
        9 => A::BottomRight,
        _ => A::TopLeft,
    }
}

// ---------------------------------------------------------------- queries

/// Path segments one `measure` call may process (non-uniformly scaled
/// block instances have to be measured through their geometry).
const MAX_MEASURE_WORK: usize = 4_000_000;
/// Items and flattened points one `pick` call may visit.
const MAX_PICK_WORK: usize = 4_000_000;

fn measure_work(work: &mut usize, n: usize) -> Result<(), String> {
    *work += n;
    if *work > MAX_MEASURE_WORK {
        return Err(format!("measuring needs more than {MAX_MEASURE_WORK} path segments (blocks scaled unevenly, nested deeply); measure fewer entities"));
    }
    Ok(())
}

/// (length, area) of a leaf item mapped by `m`.
fn leaf_measure(it: &Item, m: &Aff, work: &mut usize) -> Result<(f64, f64), String> {
    match &it.prim {
        Prim::Stroke { path, .. } => {
            measure_work(work, path.len())?;
            let path = transform_path(path, m);
            Ok((path_length(&path), subpath_areas(&path).iter().map(|a| a.abs()).sum::<f64>()))
        }
        Prim::Fill { path } => {
            measure_work(work, path.len())?;
            let path = transform_path(path, m);
            let areas = subpath_areas(&path);
            let bb = path_bbox(&path);
            let mut budget = 2_000_000;
            let rings = flatten(&path, (bb.w().hypot(bb.h()) * 1e-5).max(1e-12), None, &mut budget);
            let pairs: Vec<(Vec<P>, f64)> = rings.into_iter().zip(areas).map(|(r, a)| (r.pts, a)).collect();
            Ok((path_length(&path), even_odd_area(&pairs)))
        }
        _ => Ok((0.0, 0.0)),
    }
}

/// (length, area) of a block in its own coordinates, computed once.
fn block_measure(b: &BlockGeom, depth: u32) -> Result<(f64, f64), String> {
    b.measure
        .get_or_init(|| {
            let mut work = 0;
            let mut acc = (0.0, 0.0);
            for it in &b.items {
                let (l, a) = item_measure(it, &Aff::IDENTITY, &mut work, depth + 1)?;
                acc.0 += l;
                acc.1 += a;
            }
            Ok(acc)
        })
        .clone()
}

/// (length, area) of an item mapped by `m`. An instance under a similarity
/// (move, rotate, uniform scale, mirror) scales its block's cached values;
/// areas scale by the determinant under any affine map; only lengths under
/// uneven scales walk the geometry.
fn item_measure(it: &Item, m: &Aff, work: &mut usize, depth: u32) -> Result<(f64, f64), String> {
    let Prim::Insert(inst) = &it.prim else {
        return leaf_measure(it, m, work);
    };
    if depth >= MAX_DEPTH {
        return Ok((0.0, 0.0));
    }
    let mm = m.mul(inst.xf);
    let (l, a) = block_measure(&inst.block, depth)?;
    let area = a * mm.det().abs();
    if let Some(s) = mm.similarity_scale() {
        return Ok((l * s, area));
    }
    let mut length = 0.0;
    for child in &inst.block.items {
        length += item_measure(child, &mm, work, depth + 1)?.0;
    }
    Ok((length, area))
}

/// Length and area of entities, from their drawn geometry (inserts expanded).
pub fn measure(scene: &Scene, ids: &HashSet<u64>) -> Result<Data, String> {
    let mut length = 0.0;
    let mut area = 0.0;
    let mut work = 0usize;
    let mut found: Vec<usize> = ids.iter().filter_map(|id| scene.group_index(*id)).collect();
    found.sort_unstable();
    for &k in &found {
        for it in &scene.groups[k].items {
            let (l, a) = item_measure(it, &Aff::IDENTITY, &mut work, 0)?;
            length += l;
            area += a;
        }
    }
    Ok(Data::map([
        ("length", Data::Num(length)),
        ("area", Data::Num(area)),
        ("count", Data::Num(found.len() as f64)),
        ("missing", Data::Num((ids.len() - found.len()) as f64)),
    ]))
}

struct PickWalk<'a> {
    scene: &'a Scene,
    q: P,
    tol: f64,
    /// The query point ± tol: geometry outside it cannot be picked, so
    /// curves are only subdivided inside it.
    qbox: BBox,
    visible: &'a [bool],
    work: usize,
}

impl PickWalk<'_> {
    fn on(&self, layer: u32) -> bool {
        self.visible.get(layer as usize).copied().unwrap_or(true)
    }

    /// Distance from the query point to `it` (mapped by `m`); infinite when
    /// it is hidden or out of reach.
    fn item(&mut self, it: &Item, m: &Aff, ctx: Ctx, depth: u32) -> f64 {
        let far = f64::INFINITY;
        if !self.on(ctx.layer_of(it)) || self.work > MAX_PICK_WORK {
            return far;
        }
        let bb = if *m == Aff::IDENTITY { it.bbox } else { it.bbox.transform(m) };
        if !bb.inflate(self.tol).contains(self.q) {
            return far;
        }
        self.work += 1;
        // flattening tolerance: fine against tol, but never below what the
        // item's own size can resolve in f64
        let ft = (self.tol * 0.05).max(bb.w().hypot(bb.h()) * 1e-9).max(1e-12);
        match &it.prim {
            Prim::Insert(inst) => {
                if depth >= MAX_DEPTH {
                    return far;
                }
                let mm = m.mul(inst.xf);
                let Some(inv) = mm.inverse() else { return far };
                let local = self.qbox.transform(&inv);
                let inner = ctx.inner(it, &self.scene.layers);
                let mut cand = Vec::new();
                inst.block.candidates(&local, &mut cand);
                let mut best = far;
                for i in cand {
                    best = best.min(self.item(&inst.block.items[i], &mm, inner, depth + 1));
                }
                best
            }
            Prim::Stroke { path, width } => {
                let path = transform_path(path, m);
                let half = (width * 0.5 * m.mean_scale()).max(0.0);
                let mut budget = 1_000_000;
                let polys = flatten(&path, ft, Some(&self.qbox.inflate(half + ft)), &mut budget);
                self.work += 1_000_000 - budget;
                let mut d = far;
                for poly in &polys {
                    let mut pts = poly.pts.clone();
                    if poly.closed && pts.len() > 1 {
                        pts.push(pts[0]);
                    }
                    for w in pts.windows(2) {
                        d = d.min(seg_dist(self.q, w[0], w[1]));
                    }
                }
                (d - half).max(0.0)
            }
            Prim::Fill { path } => {
                // pieces outside the box become chords; the even-odd test at
                // the query point does not change by it
                let path = transform_path(path, m);
                let mut budget = 2_000_000;
                let rings = flatten(&path, ft, Some(&self.qbox), &mut budget);
                self.work += 2_000_000 - budget;
                let inside = rings.iter().filter(|r| point_in_ring(self.q, &r.pts)).count() % 2 == 1;
                if inside {
                    0.0
                } else {
                    far
                }
            }
            Prim::Dot(c) => m.apply(*c).dist(self.q),
            Prim::Text(_) => {
                if bb.contains(self.q) {
                    0.0
                } else {
                    far
                }
            }
        }
    }
}

/// The entity nearest to (x, y) within `tol` (drawing units), topmost first.
/// Work is bounded: curves are only subdivided near the point, and the
/// walk stops after [`MAX_PICK_WORK`] steps with the best hit so far.
pub fn pick(scene: &Scene, q: P, tol: f64, visible: &[bool]) -> Option<u64> {
    let tol = if tol.is_finite() { tol.abs() } else { 0.0 };
    let qbox = BBox::new(q.x - tol, q.y - tol, q.x + tol, q.y + tol);
    let mut w = PickWalk { scene, q, tol, qbox, visible, work: 0 };
    let mut best: Option<(f64, usize)> = None;
    for k in scene.query(&qbox) {
        let g = &scene.groups[k];
        if !w.on(g.layer) || !g.bbox.inflate(tol).contains(q) {
            continue;
        }
        let ctx = Ctx::top(g.layer);
        let mut d = f64::INFINITY;
        for it in &g.items {
            d = d.min(w.item(it, &Aff::IDENTITY, ctx, 0));
        }
        if d <= tol && best.is_none_or(|(bd, _)| d <= bd) {
            best = Some((d, k));
        }
        if w.work > MAX_PICK_WORK {
            break;
        }
    }
    best.map(|(_, k)| scene.groups[k].id)
}

// ---------------------------------------------------------------- tiles

struct CadTiles {
    rect: Rect,
}

impl CadTiles {
    fn draw(&self, key: TileKey, d: &Data, cancelled: &dyn Fn() -> bool) -> Option<Vec<u8>> {
        let n = TILE_SIZE;
        let Ok((doc, _)) = doc_of(d.get("drawing")) else {
            // no drawing yet: the background alone
            let bg = packed_of(d.get("background")).unwrap_or(DEFAULT_BG.to_be_bytes());
            return Some(std::iter::repeat_n(premul(bg), (n * n) as usize).flatten().collect());
        };
        let scene = doc.scene();
        let view = rect_of(d.get("rect")).ok().flatten().unwrap_or_else(|| scene.view_bounds().inflate(scene.view_bounds().w().max(scene.view_bounds().h()) * 0.02));
        let style = style_of(&scene, d);
        let k = level_scale(key.level) as f64;
        let to_layer = fit(&view, (0.0, 0.0, self.rect.w as f64, self.rect.h as f64), true);
        let xf = Aff::translate(-(key.tx as f64) * n as f64, -(key.ty as f64) * n as f64).mul(Aff::scale(k, k)).mul(to_layer);
        render(&scene, &xf, n, n, &style, cancelled)
    }
}

fn premul(c: [u8; 4]) -> [u8; 4] {
    let a = c[3] as u32;
    [(c[0] as u32 * a / 255) as u8, (c[1] as u32 * a / 255) as u8, (c[2] as u32 * a / 255) as u8, c[3]]
}

impl TileSource for CadTiles {
    fn render(&self, key: TileKey, params: &Value, cancelled: &dyn Fn() -> bool) -> Option<Vec<u8>> {
        let d = value_to_data(&**params);
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.draw(key, &d, cancelled))).ok().flatten()
    }
}

/// The tile source for a layer rect, for hosts and tests.
pub fn tile_source(rect: Rect) -> Arc<dyn TileSource> {
    Arc::new(CadTiles { rect })
}

/// Render a view of the drawing to an image (`cad.render`).
pub fn render_image(doc: &CadDoc, opts: &Data, cancelled: &dyn Fn() -> bool) -> Result<Image, String> {
    let scene = doc.scene();
    let view = match rect_of(opts.get("rect"))? {
        Some(r) => r,
        None => {
            let b = scene.view_bounds();
            b.inflate(b.w().max(b.h()) * 0.02)
        }
    };
    let w = num_or(opts.get("width_px"), 1024.0, "width_px")?.clamp(1.0, MAX_IMAGE_SIDE);
    let h = match opts.get("height_px") {
        Data::Nil => (w * view.h() / view.w().max(1e-12)).clamp(1.0, MAX_IMAGE_SIDE),
        x => num(x, "height_px")?.clamp(1.0, MAX_IMAGE_SIDE),
    };
    let (w, h) = (w.round() as u32, h.round() as u32);
    let style = style_of(&scene, opts);
    let xf = fit(&view, (0.0, 0.0, w as f64, h as f64), true);
    let rgba = render(&scene, &xf, w, h, &style, cancelled).ok_or("cancelled")?;
    Ok(Image { w, h, rgba, newest: None })
}

// ---------------------------------------------------------------- registration

fn layers_data(st: &State) -> Data {
    let scene = &st.scene;
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for g in &scene.groups {
        *counts.entry(g.layer).or_insert(0) += 1;
    }
    Data::list(
        scene
            .layers
            .iter()
            .enumerate()
            .map(|(i, l)| {
                Data::map([
                    ("name", Data::str(&l.name)),
                    ("color", Data::Num(l.color.packed())),
                    ("aci", Data::Num(l.aci as f64)),
                    ("on", Data::Bool(l.on)),
                    ("entities", Data::Num(counts.get(&(i as u32)).copied().unwrap_or(0) as f64)),
                ])
            })
            .collect(),
    )
}

fn version_name(v: dxf::enums::AcadVersion) -> String {
    format!("{v:?}")
}

pub fn entities_data(st: &State, filter: &Data) -> Result<Data, String> {
    let scene = &st.scene;
    let layers: HashSet<String> = str_list(filter.get("layer")).iter().map(|s| s.to_uppercase()).collect();
    let types: HashSet<String> = str_list(filter.get("type")).iter().map(|s| s.to_uppercase()).collect();
    let ids = ids_of(filter.get("ids"))?;
    let rect = rect_of(filter.get("rect"))?;
    let limit = num_or(filter.get("limit"), MAX_ENTITIES_OUT as f64, "limit")?.clamp(0.0, MAX_ENTITIES_OUT as f64) as usize;
    let keep = |id: u64, kind: &str, layer: &str| {
        (layers.is_empty() || layers.contains(&layer.to_uppercase()))
            && (types.is_empty() || types.contains(kind))
            && (ids.is_empty() || ids.contains(&id))
            && rect.is_none_or(|r| scene.group(id).is_some_and(|g| g.bbox.intersects(&r)))
    };
    let mut out = Vec::new();
    // drawing order: hatches after the entity they follow
    let mut by_anchor: HashMap<Option<u64>, Vec<&Hatch>> = HashMap::new();
    for (a, h) in &st.hatches {
        by_anchor.entry(*a).or_default().push(h);
    }
    let push_h = |hs: Option<Vec<&Hatch>>, out: &mut Vec<Data>| {
        for h in hs.unwrap_or_default() {
            if out.len() < limit && keep(h.handle, "HATCH", &h.layer) {
                out.push(hatch_data(h, scene));
            }
        }
    };
    push_h(by_anchor.remove(&None), &mut out);
    for e in st.dwg.entities() {
        if out.len() >= limit {
            break;
        }
        if keep(e.common.handle.0, entity_kind(&e.specific), &e.common.layer) {
            out.push(entity_data(e, scene));
        }
        push_h(by_anchor.remove(&Some(e.common.handle.0)), &mut out);
    }
    let mut rest: Vec<(Option<u64>, Vec<&Hatch>)> = by_anchor.into_iter().collect();
    rest.sort_by_key(|(a, _)| *a);
    for (_, hs) in rest {
        push_h(Some(hs), &mut out);
    }
    Ok(Data::list(out))
}

/// Registration with every entry point behind `catch_unwind`: a panic in
/// drawing code (or in the dxf crate) becomes an error string for the
/// script, never an unwind through the Luau VM.
struct Guarded<'a>(&'a mut Natives);

impl Guarded<'_> {
    fn function(&mut self, name: &'static str, effect: Effect, f: impl Fn(&Call) -> Result<Data, String> + Send + Sync + 'static) {
        self.0.function(name, effect, move |c| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(c))).unwrap_or_else(|_| Err(format!("internal error in {name}")))
        });
    }
    fn job_fn(&mut self, name: &'static str, f: impl Fn(&JobCtx) -> Result<Data, String> + Send + Sync + 'static) {
        self.0.job_fn(name, move |j| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(j))).unwrap_or_else(|_| Err(format!("internal error in {name}")))
        });
    }
    fn tile(&mut self, name: &str, f: impl Fn(Rect, &Data) -> Result<Arc<dyn TileSource>, String> + Send + Sync + 'static) {
        self.0.tile(name, f);
    }
}

/// Run an edit on the drawing in `c.arg(0)`; the drawing's next version.
fn edit(c: &Call, f: impl FnOnce(&mut State) -> Result<(), String>) -> Result<Data, String> {
    let (doc, r) = doc_of(c.arg(0))?;
    Ok(doc.edit(r, f)?.0)
}

/// STEP files above this go to the job `cad.step_mesh`: the function runs
/// on the calling thread and cannot be cancelled.
const MAX_STEP_INLINE_BYTES: usize = 4 << 20;

/// Add this module's functions, jobs and tile source to `n`.
pub fn register(n: &mut Natives) {
    let n = &mut Guarded(n);
    n.function("cad.open", Effect::Pure, |c: &Call| {
        let bytes = c.arg(0).as_bytes().ok_or("cad.open needs the file's bytes")?;
        let fmt = c.arg(1).str_or("dxf").to_ascii_lowercase();
        match fmt.as_str() {
            "dxf" => {}
            "step" | "stp" => return Err("STEP files are meshes: use cad.step_to_mesh(bytes)".into()),
            "dwg" => return Err("DWG is not supported: save the drawing as DXF".into()),
            other => return Err(format!("unknown CAD format \"{other}\" (dxf)")),
        }
        Ok(Data::res(KIND, CadDoc::open(bytes)?))
    });
    n.function("cad.new", Effect::Pure, |c| {
        if c.arg(0).get("pending").truthy() {
            return Ok(Data::res(KIND, CadDoc::new_pending()));
        }
        Ok(Data::res(KIND, CadDoc::new_empty()))
    });
    n.function("cad.info", Effect::Pure, |c| {
        let (doc, _) = doc_of(c.arg(0))?;
        let st = doc.lock();
        let font = crate::font::global();
        let mut f = vec![
            ("version", Data::str(&version_name(st.dwg.header.version))),
            ("encoding", Data::str(st.encoding)),
            ("entities", Data::Num((st.dwg.entities().count() + st.hatches.len()) as f64)),
            ("drawn", Data::Num(st.scene.groups.len() as f64)),
            ("layers", Data::Num(st.scene.layers.len() as f64)),
            ("blocks", Data::Num(st.dwg.blocks().count() as f64)),
            ("units", Data::Num(st.dwg.header.default_drawing_units as i32 as f64)),
            ("warnings", Data::list(st.warnings.iter().map(|w| Data::str(w)).collect())),
            ("font", font.map(|f| Data::str(&f.source)).unwrap_or(Data::Nil)),
            ("loading", Data::Bool(st.pending)),
            (
                "load_ms",
                Data::map([
                    ("parse", Data::Num(st.timings[0])),
                    ("hatch_scan", Data::Num(st.timings[1])),
                    ("fixups", Data::Num(st.timings[2])),
                    ("scene", Data::Num(st.timings[3])),
                ]),
            ),
        ];
        if let Some(e) = &st.load_error {
            f.push(("load_error", Data::str(e)));
        }
        if !st.scene.bounds.is_empty() {
            f.push(("bounds", rect_data(&st.scene.bounds)));
        }
        Ok(Data::map(f))
    });
    n.function("cad.layers", Effect::Pure, |c| {
        let (doc, _) = doc_of(c.arg(0))?;
        let st = doc.lock();
        Ok(layers_data(&st))
    });
    n.function("cad.blocks", Effect::Pure, |c| {
        let (doc, _) = doc_of(c.arg(0))?;
        let st = doc.lock();
        Ok(Data::list(
            st.dwg
                .blocks()
                .map(|b| {
                    Data::map([
                        ("name", Data::str(&b.name)),
                        ("entities", Data::Num(b.entities.len() as f64)),
                        ("base", Data::nums([b.base_point.x, b.base_point.y])),
                        ("anonymous", Data::Bool(b.name.starts_with('*'))),
                    ])
                })
                .collect(),
        ))
    });
    n.function("cad.bounds", Effect::Pure, |c| {
        let (doc, _) = doc_of(c.arg(0))?;
        let scene = doc.scene();
        if scene.bounds.is_empty() {
            return Ok(Data::Nil);
        }
        let margin = num_or(c.arg(1).get("margin"), 0.0, "margin")?.clamp(0.0, 10.0);
        let b = scene.bounds;
        Ok(rect_data(&b.inflate(b.w().max(b.h()) * margin)))
    });
    n.function("cad.entities", Effect::Pure, |c| {
        let (doc, _) = doc_of(c.arg(0))?;
        let st = doc.lock();
        entities_data(&st, c.arg(1))
    });
    n.function("cad.entity", Effect::Pure, |c| {
        let (doc, _) = doc_of(c.arg(0))?;
        let v = num(c.arg(1), "id")?;
        if v < 0.0 || v.fract() != 0.0 || v > 9.0e15 {
            return Ok(Data::Nil);
        }
        let id = v as u64;
        let mut st = doc.lock();
        if let Some(i) = st.entity_index(id) {
            if let Some(e) = st.dwg.entities().nth(i) {
                return Ok(entity_data(e, &st.scene));
            }
        }
        Ok(st.hatches.iter().find(|(_, h)| h.handle == id).map(|(_, h)| hatch_data(h, &st.scene)).unwrap_or(Data::Nil))
    });
    n.function("cad.measure", Effect::Pure, |c| {
        let (doc, _) = doc_of(c.arg(0))?;
        let ids = ids_of(c.arg(1))?;
        measure(&doc.scene(), &ids)
    });
    n.function("cad.pick", Effect::Pure, |c| {
        let (doc, _) = doc_of(c.arg(0))?;
        let q = p(num(c.arg(1), "x")?, num(c.arg(2), "y")?);
        let tol = num_or(c.arg(3), 1.0, "tolerance")?.abs();
        let scene = doc.scene();
        let vis = visibility(&scene, c.arg(4).get("layers"));
        Ok(pick(&scene, q, tol, &vis).map(|id| Data::Num(id as f64)).unwrap_or(Data::Nil))
    });
    n.function("cad.select", Effect::Pure, |c| {
        let (doc, _) = doc_of(c.arg(0))?;
        let r = rect_of(c.arg(1))?.ok_or("cad.select needs a rect")?;
        let inside = c.arg(2).get("inside").truthy();
        let scene = doc.scene();
        let vis = visibility(&scene, c.arg(2).get("layers"));
        let ids: Vec<f64> = scene
            .query(&r)
            .into_iter()
            .map(|k| &scene.groups[k])
            .filter(|g| vis.get(g.layer as usize).copied().unwrap_or(true) && !g.bbox.is_empty())
            .filter(|g| if inside { r.contains_box(&g.bbox) } else { r.intersects(&g.bbox) })
            .map(|g| g.id as f64)
            .collect();
        Ok(Data::nums(ids))
    });
    n.function("cad.to_paths", Effect::Pure, |c| {
        let (doc, _) = doc_of(c.arg(0))?;
        let o = c.arg(1);
        let scene = doc.scene();
        let rect = rect_of(o.get("rect"))?;
        let flip = o.get("flip_y").as_bool().unwrap_or(true);
        let src = rect.unwrap_or_else(|| scene.view_bounds());
        let xf = match rect_of(o.get("fit"))? {
            Some(f) => {
                // fit is in output units: y down from its top when flip_y
                fit(&src, (f.x0, f.y0, f.w(), f.h()), flip)
            }
            None => {
                if flip {
                    Aff::scale(1.0, -1.0)
                } else {
                    Aff::IDENTITY
                }
            }
        };
        let mut style = style_of(&scene, o);
        style.fg = match packed_of(o.get("foreground")) {
            Some(fg) => [fg[0], fg[1], fg[2]],
            None => [0, 0, 0], // ACI 7 is black on paper
        };
        let opts = PathOpts {
            xf,
            style,
            rect,
            width: num_or(o.get("width"), 0.25, "width")?.max(0.0),
            text: o.get("text").as_bool().unwrap_or(true),
            max_numbers: 8_000_000,
        };
        Ok(Data::list(to_paths(&scene, &opts)?))
    });
    // writes bytes; the drawing itself is not changed
    n.function("cad.save", Effect::Pure, |c| {
        let (doc, _) = doc_of(c.arg(0))?;
        Ok(Data::bytes(doc.save()?))
    });
    n.function("cad.last_ids", Effect::Pure, |c| {
        let (doc, _) = doc_of(c.arg(0))?;
        let st = doc.lock();
        Ok(Data::nums(st.last_ids.iter().map(|x| *x as f64)))
    });
    n.function("cad.to_drawing", Effect::Pure, |c| {
        let lr = layer_rect(c.arg(0))?;
        let view = rect_of(c.arg(1))?.ok_or("view rect required")?;
        let m = fit(&view, (lr.0, lr.1, lr.2, lr.3), true).inverse().ok_or("degenerate view")?;
        let q = m.apply(p(num(c.arg(2), "x")?, num(c.arg(3), "y")?));
        Ok(Data::map([("x", Data::Num(q.x)), ("y", Data::Num(q.y))]))
    });
    n.function("cad.to_page", Effect::Pure, |c| {
        let lr = layer_rect(c.arg(0))?;
        let view = rect_of(c.arg(1))?.ok_or("view rect required")?;
        let q = fit(&view, (lr.0, lr.1, lr.2, lr.3), true).apply(p(num(c.arg(2), "x")?, num(c.arg(3), "y")?));
        Ok(Data::map([("x", Data::Num(q.x)), ("y", Data::Num(q.y))]))
    });
    n.function("cad.zoom", Effect::Pure, |c| {
        let r = rect_of(c.arg(0))?.ok_or("view rect required")?;
        let (cx, cy) = (num(c.arg(1), "x")?, num(c.arg(2), "y")?);
        let f = num(c.arg(3), "factor")?;
        if f <= 0.0 {
            return Err("factor must be > 0".into());
        }
        zoom_rect(&r, cx, cy, f).map(|b| rect_data(&b))
    });

    // ---- edits: each returns the drawing at its next version
    n.function("cad.add", Effect::Effect, |c| {
        let list: Vec<Data> = match c.arg(1) {
            Data::List(l) => l.to_vec(),
            d @ Data::Map(_) => vec![d.clone()],
            _ => return Err("cad.add needs an entity table or a list of them".into()),
        };
        let parsed = list.iter().map(entity_from_data).collect::<Result<Vec<_>, _>>()?;
        edit(c, |st| {
            st.last_ids = st.add_entities(parsed);
            Ok(())
        })
    });
    n.function("cad.add_block", Effect::Effect, |c| {
        let spec = c.arg(1);
        let name = s_of(spec, "name", "")?;
        if name.is_empty() || name.starts_with('*') {
            return Err("a block needs a name (not starting with *)".into());
        }
        let base = spec.get("base");
        let base = p(num_or(base.at(1), 0.0, "base")?, num_or(base.at(2), 0.0, "base")?);
        let mut ents = Vec::new();
        for e in spec.get("entities").items() {
            match entity_from_data(e)? {
                NewEnt::Dxf(e) => ents.push(*e),
                NewEnt::Hatch(_) => return Err("hatches inside new blocks are not supported; add them to the drawing".into()),
            }
        }
        edit(c, |st| {
            if st.dwg.blocks().any(|b| b.name.eq_ignore_ascii_case(&name)) {
                return Err(format!("block \"{name}\" exists"));
            }
            st.dwg.add_block(dxf::Block { name: name.clone(), base_point: dp(base.x, base.y), entities: ents, ..Default::default() });
            st.rebuild();
            Ok(())
        })
    });
    n.function("cad.move", Effect::Effect, |c| {
        let ids = ids_of(c.arg(1))?;
        let d = p(num(c.arg(2), "dx")?, num(c.arg(3), "dy")?);
        edit(c, |st| {
            st.move_ids(&ids, d)
                .map_err(|failed| format!("{} entities cannot be moved (unsupported type or tilted plane), nothing moved: {:?}", failed.len(), failed))
        })
    });
    n.function("cad.copy", Effect::Effect, |c| {
        let ids = ids_of(c.arg(1))?;
        let d = p(num(c.arg(2), "dx")?, num(c.arg(3), "dy")?);
        edit(c, |st| {
            st.last_ids = st.copy_ids(&ids, d);
            Ok(())
        })
    });
    n.function("cad.delete", Effect::Effect, |c| {
        let ids = ids_of(c.arg(1))?;
        edit(c, |st| {
            st.delete_ids(&ids);
            Ok(())
        })
    });
    n.function("cad.set_layer", Effect::Effect, |c| {
        let ids = ids_of(c.arg(1))?;
        let layer = c.arg(2).as_str().filter(|s| !s.is_empty() && s.len() <= 255).ok_or("layer must be a non-empty name")?.to_string();
        edit(c, |st| {
            st.set_layer(&ids, &layer);
            Ok(())
        })
    });
    n.function("cad.set_color", Effect::Effect, |c| {
        let ids = ids_of(c.arg(1))?;
        let (aci, tc) = match c.arg(2) {
            Data::Str(s) if s.eq_ignore_ascii_case("bylayer") => (256, None),
            Data::Str(s) if s.eq_ignore_ascii_case("byblock") => (0, None),
            Data::Num(v) => (aci_number(*v)?, None),
            d @ Data::Map(_) => color_fields(d)?,
            _ => return Err("colour must be an ACI number (0-256), \"bylayer\", \"byblock\", or { color = 0xRRGGBBAA }".into()),
        };
        edit(c, |st| {
            st.set_color(&ids, aci, tc);
            Ok(())
        })
    });
    n.function("cad.add_layer", Effect::Effect, |c| {
        let spec = c.arg(1);
        let name = s_of(spec, "name", "")?;
        if name.is_empty() || name.len() > 255 {
            return Err("a layer needs a name".into());
        }
        let (aci, _) = color_fields(spec)?;
        let aci = if (1..=255).contains(&aci) { aci } else { 7 };
        let on = spec.get("on").as_bool().unwrap_or(true);
        edit(c, |st| {
            st.ensure_layer(&name);
            for l in st.dwg.layers_mut() {
                if l.name.eq_ignore_ascii_case(&name) {
                    l.color = aci_color(aci);
                    l.is_layer_on = on;
                }
            }
            st.rebuild();
            Ok(())
        })
    });
    n.function("cad.layer_on", Effect::Effect, |c| {
        let name = c.arg(1).as_str().ok_or("layer name required")?.to_string();
        let on = c.arg(2).truthy();
        edit(c, |st| {
            let mut found = false;
            for l in st.dwg.layers_mut() {
                if l.name.eq_ignore_ascii_case(&name) {
                    l.is_layer_on = on;
                    found = true;
                }
            }
            if !found {
                return Err(format!("no layer \"{name}\""));
            }
            let mut scene = (*st.scene).clone();
            for l in scene.layers.iter_mut() {
                if l.name.eq_ignore_ascii_case(&name) {
                    l.on = on;
                }
            }
            st.scene = Arc::new(scene);
            Ok(())
        })
    });

    // ---- jobs
    // input: bytes → a new drawing; { bytes, into = cad.new({ pending = true }) }
    // → fills that drawing (a handler can hold it while it loads); nil → nil
    n.job_fn("cad.load", |j: &JobCtx| {
        let input = j.input(0);
        let (bytes, into) = match input {
            Data::Map(_) => (input.get("bytes"), Some(input.get("into"))),
            _ => (input, None),
        };
        if bytes.is_nil() && into.is_none() {
            return Ok(Data::Nil);
        }
        let bytes = bytes.as_bytes().ok_or("cad.load needs the file's bytes")?;
        let progress = |x: f32| j.progress(x);
        let cancelled = || j.cancelled();
        match into {
            Some(target) => {
                let (doc, r) = doc_of(target)?;
                let v = doc.fill(bytes, &progress, &cancelled)?;
                Ok(Data::Res(at_version(r, v)))
            }
            None => Ok(Data::res(KIND, CadDoc::open_with(bytes, &progress, &cancelled)?)),
        }
    });
    n.job_fn("cad.render", |j: &JobCtx| {
        let (doc, _) = doc_of(j.input(0))?;
        let opts = if j.input(1).is_nil() { j.opts.clone() } else { j.input(1).clone() };
        let img = render_image(&doc, &opts, &|| j.cancelled())?;
        Ok(Data::res("cad.image", img))
    });
    n.function("cad.step_to_mesh", Effect::Pure, |c| {
        let bytes = c.arg(0).as_bytes().ok_or("cad.step_to_mesh needs the file's bytes")?;
        if bytes.len() > MAX_STEP_INLINE_BYTES {
            return Err(format!(
                "STEP file larger than {} MB: use the job cad.step_mesh, which runs off the page's thread",
                MAX_STEP_INLINE_BYTES >> 20
            ));
        }
        crate::step::step_to_mesh(bytes, c.arg(1), crate::step::INLINE_MAX_TRIANGLES, &|| false)
    });
    n.job_fn("cad.step_mesh", |j: &JobCtx| {
        let bytes = j.input(0).as_bytes().ok_or("cad.step_mesh needs the file's bytes")?;
        let opts = if j.input(1).is_nil() { j.opts.clone() } else { j.input(1).clone() };
        crate::step::step_to_mesh(bytes, &opts, crate::step::JOB_MAX_TRIANGLES, &|| j.cancelled())
    });

    // ---- tiles
    n.tile("cad.drawing", |rect: Rect, _opts: &Data| Ok(tile_source(rect)));
}

/// `r` zoomed by `f` about (cx, cy); refused when the result cannot be a
/// usable view (overflow, or narrower than f64 can resolve at its place).
pub fn zoom_rect(r: &BBox, cx: f64, cy: f64, f: f64) -> Result<BBox, String> {
    if !(f.is_finite() && f > 0.0) {
        return Err("factor must be a finite number > 0".into());
    }
    let b = BBox::new(cx + (r.x0 - cx) / f, cy + (r.y0 - cy) / f, cx + (r.x1 - cx) / f, cy + (r.y1 - cy) / f);
    let finite = [b.x0, b.y0, b.x1, b.y1].iter().all(|v| v.is_finite()) && b.w().is_finite() && b.h().is_finite();
    let resolvable = b.w() > b.x0.abs().max(b.x1.abs()) * 1e-12 && b.h() > b.y0.abs().max(b.y1.abs()) * 1e-12 && b.w() > 1e-200 && b.h() > 1e-200;
    if !finite || !resolvable {
        return Err(format!("zoom by {f} gives an unusable view ({} x {}); zoom less", b.w(), b.h()));
    }
    Ok(b)
}

/// The default background, for pages and tests.
pub fn default_background() -> [u8; 4] {
    DEFAULT_BG.to_be_bytes()
}

/// ACI 7's colour on the default background.
pub fn default_foreground() -> [u8; 3] {
    contrast(DEFAULT_BG.to_be_bytes())
}
