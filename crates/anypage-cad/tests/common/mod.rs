//! Shared helpers: generated drawings, pages, PNG output, pixel checks.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use anypage_engine::{TextRaster, Wake};
use anypage_runtime::{Data, Effect, Natives, Package, ScriptPage};
use dxf::entities::*;
use dxf::enums::AcadVersion;
use dxf::tables::Layer;
use dxf::{Block, Color, Drawing, Point, Vector};

pub fn text() -> Arc<TextRaster> {
    Arc::new(TextRaster::load().expect("a system font"))
}

pub fn wake() -> Wake {
    Arc::new(|| {})
}

/// The cad module plus `test.bytes(name)` serving generated files to scripts.
pub fn natives(files: Vec<(&'static str, Vec<u8>)>) -> Natives {
    let mut n = Natives::new();
    anypage_cad::register(&mut n);
    let files: Vec<(String, Arc<Vec<u8>>)> = files.into_iter().map(|(k, v)| (k.to_string(), Arc::new(v))).collect();
    n.function("test.bytes", Effect::Pure, move |c| {
        let name = c.arg(0).as_str().ok_or("name")?;
        files.iter().find(|(k, _)| k == name).map(|(_, v)| Data::Bytes(v.clone())).ok_or_else(|| format!("no test file {name}"))
    });
    n
}

pub fn load_page(src: &str, natives: &Natives) -> Result<ScriptPage, String> {
    let manifest = r#"{ "format": 1, "id": "cad-test", "bounds": [0, 0, 800, 600],
        "capabilities": { "modules": ["cad", "test"], "files": true },
        "limits": { "handler_ms": 1000, "load_ms": 10000, "call_ms": 1000 } }"#;
    let pkg = Package::from_files("test", [("page.json", manifest), ("main.luau", src)])?;
    ScriptPage::load(&pkg, natives, text(), wake())
}

pub fn out_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-output");
    std::fs::create_dir_all(&d).expect("test-output dir");
    d
}

/// Write premultiplied RGBA8 as PNG (unpremultiplied).
pub fn save_png(name: &str, w: u32, h: u32, rgba: &[u8]) {
    let mut straight = rgba.to_vec();
    for px in straight.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a > 0 && a < 255 {
            for c in px.iter_mut().take(3) {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let path = out_dir().join(name);
    let file = std::fs::File::create(&path).expect("png file");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().expect("png header").write_image_data(&straight).expect("png data");
}

pub fn px(rgba: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

/// Is the pixel close to `c` (rgb only)?
pub fn near(a: [u8; 4], c: [u8; 3], tol: i32) -> bool {
    (0..3).all(|k| (a[k] as i32 - c[k] as i32).abs() <= tol)
}

/// Pixels in a box that are close to `c`.
#[allow(clippy::too_many_arguments)]
pub fn count(rgba: &[u8], w: u32, x0: u32, y0: u32, x1: u32, y1: u32, c: [u8; 3], tol: i32) -> usize {
    let mut n = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            if near(px(rgba, w, x, y), c, tol) {
                n += 1;
            }
        }
    }
    n
}

/// The pixel is `c` at some coverage over black (anti-aliased thin lines).
pub fn like(a: [u8; 4], c: [u8; 3]) -> bool {
    let m = a[0].max(a[1]).max(a[2]) as f64;
    if m < 50.0 {
        return false;
    }
    let cm = c[0].max(c[1]).max(c[2]) as f64;
    (0..3).all(|k| (a[k] as f64 / m - c[k] as f64 / cm).abs() < 0.3)
}

/// Does some pixel within `r` of (x, y) look like `c` (any coverage)?
pub fn like_any(rgba: &[u8], w: u32, x: u32, y: u32, r: u32, c: [u8; 3]) -> bool {
    for yy in y.saturating_sub(r)..=y + r {
        for xx in x.saturating_sub(r)..=x + r {
            if like(px(rgba, w, xx, yy), c) {
                return true;
            }
        }
    }
    false
}

/// Does some pixel within `r` of (x, y) match `c`?
pub fn near_any(rgba: &[u8], w: u32, x: u32, y: u32, r: u32, c: [u8; 3], tol: i32) -> bool {
    count(rgba, w, x.saturating_sub(r), y.saturating_sub(r), x + r + 1, y + r + 1, c, tol) > 0
}

fn ent(specific: EntityType, layer: &str, aci: Option<u8>) -> Entity {
    let mut e = Entity::new(specific);
    e.common.layer = layer.to_string();
    if let Some(i) = aci {
        e.common.color = Color::from_index(i);
    }
    e
}

fn pt(x: f64, y: f64) -> Point {
    Point::new(x, y, 0.0)
}

/// A 100 x 100 drawing with one of everything at known places:
///
/// - LINE (10,10)-(90,10) on layer RED (red, BYLAYER)
/// - CIRCLE (50,50) r 20, blue
/// - ARC (50,50) r 30, 0°..90°, green
/// - LWPOLYLINE (10,80) -bulge 1-> (30,80) -> (30,90), yellow (the arc dips to y 70)
/// - TEXT "CAD" at (60,85) h 8, magenta
/// - BLOCK MARK: a BYBLOCK cross ±2; INSERT at (20,40) cyan scale 2, INSERT at (80,40) cyan rotated 45°
/// - LINE (10,95)-(90,95) on layer HIDDEN (off)
/// - CIRCLE with extrusion (0,0,-1): OCS centre (-88, 70) is drawn at (88, 70), r 3, white
/// - HATCH solid red square (70,15)-(90,30); HATCH pattern yellow square (10,15)-(30,30)
/// - LINE (40,60)-(60,60) on layer GREEN (green), drawn after the solid hatch
pub fn sample_dxf() -> Vec<u8> {
    let mut d = Drawing::new();
    d.header.version = AcadVersion::R2010;
    d.add_layer(Layer { name: "RED".into(), color: Color::from_index(1), ..Default::default() });
    d.add_layer(Layer { name: "HIDDEN".into(), color: Color::from_index(3), is_layer_on: false, ..Default::default() });
    d.add_layer(Layer { name: "GREEN".into(), color: Color::from_index(3), ..Default::default() });
    d.add_entity(ent(EntityType::Line(Line::new(pt(10.0, 10.0), pt(90.0, 10.0))), "RED", None));
    d.add_entity(ent(EntityType::Circle(Circle::new(pt(50.0, 50.0), 20.0)), "0", Some(5)));
    d.add_entity(ent(EntityType::Arc(dxf::entities::Arc::new(pt(50.0, 50.0), 30.0, 0.0, 90.0)), "0", Some(3)));
    let pl = LwPolyline {
        vertices: vec![
            dxf::LwPolylineVertex { x: 10.0, y: 80.0, bulge: 1.0, ..Default::default() },
            dxf::LwPolylineVertex { x: 30.0, y: 80.0, ..Default::default() },
            dxf::LwPolylineVertex { x: 30.0, y: 90.0, ..Default::default() },
        ],
        ..Default::default()
    };
    d.add_entity(ent(EntityType::LwPolyline(pl), "0", Some(2)));
    let text = Text { location: pt(60.0, 85.0), text_height: 8.0, value: "CAD".into(), ..Default::default() };
    d.add_entity(ent(EntityType::Text(text), "0", Some(6)));
    let mut mark = Block { name: "MARK".into(), ..Default::default() };
    for (a, b) in [((-2.0, 0.0), (2.0, 0.0)), ((0.0, -2.0), (0.0, 2.0))] {
        let mut e = ent(EntityType::Line(Line::new(pt(a.0, a.1), pt(b.0, b.1))), "0", None);
        e.common.color = Color::by_block();
        mark.entities.push(e);
    }
    d.add_block(mark);
    let ins = Insert { name: "MARK".into(), location: pt(20.0, 40.0), x_scale_factor: 2.0, y_scale_factor: 2.0, ..Default::default() };
    d.add_entity(ent(EntityType::Insert(ins), "0", Some(4)));
    let ins = Insert { name: "MARK".into(), location: pt(80.0, 40.0), rotation: 45.0, ..Default::default() };
    d.add_entity(ent(EntityType::Insert(ins), "0", Some(4)));
    d.add_entity(ent(EntityType::Line(Line::new(pt(10.0, 95.0), pt(90.0, 95.0))), "HIDDEN", None));
    let mut c = Circle::new(pt(-88.0, 70.0), 3.0);
    c.normal = Vector::new(0.0, 0.0, -1.0);
    d.add_entity(ent(EntityType::Circle(c), "0", Some(7)));
    let green = ent(EntityType::Line(Line::new(pt(40.0, 60.0), pt(60.0, 60.0))), "GREEN", None);
    d.add_entity(green);
    let mut buf = Vec::new();
    d.save(&mut buf).expect("save sample");
    let text = String::from_utf8(buf).expect("ascii dxf");
    // hatches go in before the last line (by handle order, the green line is last)
    let solid = hatch("2000", "0", 1, true, &[(70.0, 15.0), (90.0, 15.0), (90.0, 30.0), (70.0, 30.0)]);
    let pattern = hatch("2001", "0", 2, false, &[(10.0, 15.0), (30.0, 15.0), (30.0, 30.0), (10.0, 30.0)]);
    let pos = text.rfind("  0\r\nLINE\r\n").expect("the last line");
    let mut out = String::new();
    out.push_str(&text[..pos]);
    out.push_str(&solid);
    out.push_str(&pattern);
    out.push_str(&text[pos..]);
    out.into_bytes()
}

/// Nested blocks: B0 holds 100 vertical unit lines at x = 0..99; Bk holds
/// 100 inserts of B(k-1) spaced 200^k apart; model space holds 100 inserts
/// of B(levels-1) at y = 0, 10, ... 990. `100^(levels+1)` lines are drawn.
pub fn nested_dxf(levels: usize) -> Vec<u8> {
    let mut s = String::from("  0\nSECTION\n  2\nBLOCKS\n");
    let mut h = 0x100u64;
    for b in 0..levels {
        s.push_str(&format!("  0\nBLOCK\n  5\n{h:X}\n  8\n0\n  2\nB{b}\n 70\n0\n 10\n0\n 20\n0\n"));
        h += 1;
        for i in 0..100 {
            if b == 0 {
                s.push_str(&format!("  0\nLINE\n  5\n{h:X}\n  8\n0\n 10\n{i}\n 20\n0\n 11\n{i}\n 21\n1\n"));
            } else {
                s.push_str(&format!("  0\nINSERT\n  5\n{h:X}\n  8\n0\n  2\nB{}\n 10\n{}\n 20\n0\n", b - 1, i as f64 * 200f64.powi(b as i32)));
            }
            h += 1;
        }
        s.push_str(&format!("  0\nENDBLK\n  5\n{h:X}\n  8\n0\n"));
        h += 1;
    }
    s.push_str("  0\nENDSEC\n  0\nSECTION\n  2\nENTITIES\n");
    for i in 0..100 {
        s.push_str(&format!("  0\nINSERT\n  5\n{h:X}\n  8\n0\n  2\nB{}\n 10\n0\n 20\n{}\n", levels - 1, i * 10));
        h += 1;
    }
    s.push_str("  0\nENDSEC\n  0\nEOF\n");
    s.into_bytes()
}

/// `n` short lines in a 1000-wide grid, handles from 0x100, layers L0..L6.
pub fn lines_dxf(n: u64) -> Vec<u8> {
    let mut s = String::from("  0\nSECTION\n  2\nENTITIES\n");
    for i in 0..n {
        let h = 0x100 + i;
        let x = (i % 1000) as f64;
        let y = (i / 1000) as f64 * 10.0;
        s.push_str(&format!("  0\nLINE\n  5\n{h:X}\n  8\nL{}\n 10\n{x}\n 20\n{y}\n 11\n{}\n 21\n{}\n", i % 7, x + 0.8, y + 5.0));
    }
    s.push_str("  0\nENDSEC\n  0\nEOF\n");
    s.into_bytes()
}

/// A HATCH entity as DXF text: one polyline loop.
pub fn hatch(handle: &str, layer: &str, aci: i32, solid: bool, pts: &[(f64, f64)]) -> String {
    let mut s = String::new();
    let mut pair = |c: i32, v: &str| s.push_str(&format!("{c:>3}\r\n{v}\r\n"));
    pair(0, "HATCH");
    pair(5, handle);
    pair(100, "AcDbEntity");
    pair(8, layer);
    pair(62, &aci.to_string());
    pair(100, "AcDbHatch");
    for (c, v) in [(10, "0.0"), (20, "0.0"), (30, "0.0"), (210, "0.0"), (220, "0.0"), (230, "1.0")] {
        pair(c, v);
    }
    pair(2, if solid { "SOLID" } else { "ANSI31" });
    pair(70, if solid { "1" } else { "0" });
    pair(71, "0");
    pair(91, "1");
    pair(92, "3");
    pair(72, "0");
    pair(73, "1");
    pair(93, &pts.len().to_string());
    for (x, y) in pts {
        pair(10, &x.to_string());
        pair(20, &y.to_string());
    }
    pair(97, "0");
    pair(75, "0");
    pair(76, "1");
    if !solid {
        pair(52, "0.0");
        pair(41, "1.0");
        pair(77, "0");
        pair(78, "1");
        for (c, v) in [(53, "45.0"), (43, "0.0"), (44, "0.0"), (45, "-2.0"), (46, "2.0"), (79, "0")] {
            pair(c, v);
        }
    }
    pair(98, "0");
    s
}

/// Two levels of blocks whose colours depend on the inserting entity:
/// INNER has lines at y 0 (BYBLOCK), y 2 (layer 0, BYLAYER) and y 4 (layer
/// BLUE); OUTER inserts INNER BYBLOCK at x 0 and yellow at x 20; model space
/// has OUTER on layer RED in green at y 0 and OUTER on layer 0 BYLAYER at y 10.
pub fn nested_colour_dxf() -> Vec<u8> {
    let line = |y: f64, layer: &str, color: Color| {
        let mut e = Entity::new(EntityType::Line(Line::new(Point::new(0.0, y, 0.0), Point::new(10.0, y, 0.0))));
        e.common.layer = layer.into();
        e.common.color = color;
        e
    };
    let insert = |name: &str, x: f64, y: f64, layer: &str, color: Color| {
        let mut e = Entity::new(EntityType::Insert(Insert { name: name.into(), location: Point::new(x, y, 0.0), ..Default::default() }));
        e.common.layer = layer.into();
        e.common.color = color;
        e
    };
    let mut d = Drawing::new();
    d.add_layer(Layer { name: "RED".into(), color: Color::from_index(1), ..Default::default() });
    d.add_layer(Layer { name: "BLUE".into(), color: Color::from_index(5), ..Default::default() });
    // INNER: y 0 BYBLOCK, y 2 BYLAYER on layer 0, y 4 on BLUE
    let mut inner = Block { name: "INNER".into(), ..Default::default() };
    inner.entities = vec![line(0.0, "0", Color::by_block()), line(2.0, "0", Color::by_layer()), line(4.0, "BLUE", Color::by_layer())];
    d.add_block(inner);
    // OUTER: INNER BYBLOCK on layer 0 at x 0, INNER yellow on layer 0 at x 20
    let mut outer = Block { name: "OUTER".into(), ..Default::default() };
    outer.entities = vec![insert("INNER", 0.0, 0.0, "0", Color::by_block()), insert("INNER", 20.0, 0.0, "0", Color::from_index(2))];
    d.add_block(outer);
    // model: OUTER on RED in green at y 0; OUTER on layer 0 BYLAYER at y 10
    d.add_entity(insert("OUTER", 0.0, 0.0, "RED", Color::from_index(3)));
    d.add_entity(insert("OUTER", 0.0, 10.0, "0", Color::by_layer()));
    let mut buf = Vec::new();
    d.save(&mut buf).unwrap();
    buf
}
