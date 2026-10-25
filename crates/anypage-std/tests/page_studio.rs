//! The studio page (`pages/studio`): CAD, spreadsheet and PDF working on the
//! same data in one page. These tests load the real package with every
//! module (`anypage_std::natives()`), drive it through its agent actions and
//! check the takeoff totals, the saved files (the report PDF re-opens with
//! the drawing and the table text, the XLSX and DXF re-open), timings, and
//! that the page still loads without the optional `scene` module.
//!
//! The sample files in `pages/studio/assets` are made here:
//!
//! ```text
//! cargo test -p anypage-std --test page_studio -- --ignored generate_sample_assets
//! ```
//!
//! (the floor plan through the `cad` module's own API, the STEP part by a
//! small writer for planar B-rep solids).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anypage_engine::core::Status;
use anypage_engine::{Image, Page, PageRequest, TextRaster};
use anypage_runtime::{Data, DisplayList, Natives, Package, ScriptPage};

// ---------------------------------------------------------------- helpers

fn text() -> Arc<TextRaster> {
    static TEXT: OnceLock<Arc<TextRaster>> = OnceLock::new();
    TEXT.get_or_init(|| Arc::new(TextRaster::load().expect("a system font"))).clone()
}

/// `pages/studio` (the harness that runs this file from elsewhere sets
/// `ANYPAGE_STUDIO_DIR`).
fn studio_dir() -> PathBuf {
    match std::env::var_os("ANYPAGE_STUDIO_DIR") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("pages").join("studio"),
    }
}

fn out_dir() -> PathBuf {
    let d = studio_dir().join("..").join("..").join("target").join("test-output").join("studio");
    std::fs::create_dir_all(&d).expect("test-output dir");
    d
}

fn package() -> Package {
    Package::from_dir(&studio_dir()).expect("pages/studio package")
}

fn load_with(natives: &Natives) -> ScriptPage {
    ScriptPage::load(&package(), natives, text(), Arc::new(|| {})).unwrap_or_else(|e| panic!("studio failed to load: {e}"))
}

fn load() -> ScriptPage {
    load_with(&anypage_std::natives())
}

/// A throwaway page with `src` as its script and `assets`, for generating
/// and checking files through the modules' Luau APIs.
fn script_page(modules: &str, src: &str, assets: &[(&str, Vec<u8>)]) -> Result<ScriptPage, String> {
    let manifest = format!(
        r#"{{ "format": 1, "id": "studio-check", "bounds": [0, 0, 100, 100],
        "capabilities": {{ "modules": [{modules}], "files": true }},
        "limits": {{ "handler_ms": 1000, "load_ms": 10000, "call_ms": 1000, "memory_mb": 128 }} }}"#
    );
    let src = format!("page.panel({{ rect = {{ 0, 0, 10, 10 }}, draw = function(d) end }})\n{src}");
    let mut pkg = Package::from_files("studio-check", [("page.json", manifest.as_str()), ("main.luau", src.as_str())])?;
    for (name, bytes) in assets {
        pkg = pkg.with_asset(name, bytes.clone())?;
    }
    ScriptPage::load(&pkg, &anypage_std::natives(), text(), Arc::new(|| {}))
}

fn get(p: &mut ScriptPage, cell: &str) -> Data {
    p.cell_value(cell).unwrap_or_else(|| panic!("no cell {cell}"))
}

fn num(d: &Data, key: &str) -> f64 {
    d.get(key).num().unwrap_or_else(|| panic!("{key} is not a number in {d:?}"))
}

/// Commit job results and run their callbacks until `done` holds.
fn pump_until(p: &mut ScriptPage, what: &str, mut done: impl FnMut(&mut ScriptPage) -> bool) {
    let t0 = Instant::now();
    loop {
        p.pump();
        if done(p) {
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn act(p: &mut ScriptPage, name: &str, params: Data) -> Data {
    p.run_action(name, &params).unwrap_or_else(|e| panic!("action {name}: {e}"))
}

/// A string in a display list's commands that contains `what`.
fn find_text(d: &Data, what: &str) -> Option<String> {
    match d {
        Data::Str(s) if s.contains(what) => Some(s.to_string()),
        Data::List(l) => l.iter().find_map(|x| find_text(x, what)),
        Data::Map(m) => m.iter().find_map(|(_, v)| find_text(v, what)),
        _ => None,
    }
}

/// Every error card the page would show, evaluated as the engine would for
/// a frame: a panel whose draw function failed (the runtime's script-error
/// card), a UI app whose view failed (the UI library's card), and tiled or
/// GPU layers whose parameters failed.
fn error_cards(p: &mut ScriptPage) -> Vec<String> {
    let spec = p.spec();
    let panels: Vec<_> = spec.panels.iter().map(|x| (x.id, x.cell)).collect();
    let others: Vec<_> = spec.tiled.iter().map(|t| t.params).chain(spec.layers.iter().map(|l| l.cell)).collect();
    let g = p.parts().0;
    let mut out = Vec::new();
    for (id, cell) in panels {
        let input = g.demand(cell);
        if input.get::<Image>().is_none() {
            out.push(format!("panel {id} has no image ({:?})", input.status));
        }
        let Some(&script) = g.inputs_of(cell).first() else { continue };
        if let Status::Error(msg) = g.status(script) {
            out.push(format!("panel {id} ({}) shows the script-error card: {msg}", g.label(script)));
        }
        if let Some(t) = g.get::<DisplayList>(script).and_then(|l| find_text(&l.cmds, "the view failed")) {
            out.push(format!("panel {id} ({}) shows the UI error card: {t}", g.label(script)));
        }
    }
    for cell in others {
        g.demand(cell);
        if let Status::Error(msg) = g.status(cell) {
            out.push(format!("{} failed: {msg}", g.label(cell)));
        }
    }
    out
}

fn assert_no_error_cards(p: &mut ScriptPage) {
    let cards = error_cards(p);
    assert!(cards.is_empty(), "the page shows error cards:\n{}", cards.join("\n"));
}

/// The check above sees a UI app whose view fails.
#[test]
#[should_panic(expected = "the UI error card")]
fn error_card_check_detects_a_failing_view() {
    let src = r#"
        local ui = import("ui")
        ui.app({ name = "broken", rect = { 0, 0, 400, 200 }, view = function() error("boom") end })
    "#;
    let mut p = script_page("", src, &[]).unwrap_or_else(|e| panic!("{e}"));
    assert_no_error_cards(&mut p);
}

/// ... and a page.panel whose draw function fails.
#[test]
#[should_panic(expected = "the script-error card")]
fn error_card_check_detects_a_failing_panel() {
    let src = r#"
        page.panel({ rect = { 0, 0, 400, 200 }, name = "broken", draw = function(d) error("boom") end })
    "#;
    let mut p = script_page("", src, &[]).unwrap_or_else(|e| panic!("{e}"));
    assert_no_error_cards(&mut p);
}

fn status(p: &mut ScriptPage) -> String {
    get(p, "status").as_str().unwrap_or_default().to_string()
}

/// A DXF made by a cad script that leaves the drawing in `dwg`.
fn dxf_from(script: &str) -> Arc<Vec<u8>> {
    let src = format!("{script}\nOut = page.input(\"out\", dwg:save())");
    let mut g = script_page(r#""cad""#, &src, &[]).unwrap_or_else(|e| panic!("{e}"));
    match get(&mut g, "out") {
        Data::Bytes(b) => b,
        other => panic!("{other:?}"),
    }
}

/// `n` square rooms in a grid, each with `labels` texts (the first names it).
fn rooms_dxf(n: usize, labels: usize) -> Arc<Vec<u8>> {
    dxf_from(&format!(
        r#"local dwg = cad.new()
        dwg = cad.add_layer(dwg, {{ name = "A-AREA" }})
        dwg = cad.add_layer(dwg, {{ name = "A-AREA-IDEN" }})
        dwg = cad.add_layer(dwg, {{ name = "A-WALL" }})
        local E = {{}}
        local cols = math.ceil(math.sqrt({n}))
        for i = 0, {n} - 1 do
            local cx, cy = (i % cols) * 4000, (i // cols) * 4000
            table.insert(E, {{ type = "LWPOLYLINE", layer = "A-AREA", closed = true, points = {{ cx + 100, cy + 100, cx + 3900, cy + 100, cx + 3900, cy + 3900, cx + 100, cy + 3900 }} }})
            table.insert(E, {{ type = "LWPOLYLINE", layer = "A-WALL", closed = true, width = 200, points = {{ cx, cy, cx + 4000, cy, cx + 4000, cy + 4000, cx, cy + 4000 }} }})
            for k = 1, {labels} do
                table.insert(E, {{ type = "TEXT", layer = "A-AREA-IDEN", x = cx + 2000, y = cy + 600 + k * 250, text = "房間 ROOM " .. i .. "-" .. k, height = 200 }})
            end
        end
        dwg = cad.add(dwg, E)"#
    ))
}

/// Eight rooms and `n` short lines on a layer the takeoff does not use.
fn lines_dxf(n: usize) -> Arc<Vec<u8>> {
    dxf_from(&format!(
        r#"local dwg = cad.new()
        dwg = cad.add_layer(dwg, {{ name = "A-AREA" }})
        dwg = cad.add_layer(dwg, {{ name = "A-AREA-IDEN" }})
        dwg = cad.add_layer(dwg, {{ name = "A-WALL" }})
        dwg = cad.add_layer(dwg, {{ name = "A-HATCH" }})
        local E = {{}}
        for i = 0, 7 do
            local cx = i * 4000
            table.insert(E, {{ type = "LWPOLYLINE", layer = "A-AREA", closed = true, points = {{ cx + 100, 100, cx + 3900, 100, cx + 3900, 3900, cx + 100, 3900 }} }})
            table.insert(E, {{ type = "LWPOLYLINE", layer = "A-WALL", closed = true, width = 200, points = {{ cx, 0, cx + 4000, 0, cx + 4000, 4000, cx, 4000 }} }})
            table.insert(E, {{ type = "TEXT", layer = "A-AREA-IDEN", x = cx + 2000, y = 2000, text = "房間 " .. i, height = 300, halign = "center" }})
        end
        for k = 1, {n} do
            local x = (k * 37) % 32000
            local y = (k * 53) % 4000
            table.insert(E, {{ type = "LINE", layer = "A-HATCH", x1 = x, y1 = y, x2 = x + 150, y2 = y + 150 }})
        end
        dwg = cad.add(dwg, E)"#
    ))
}

/// Drop a DXF and wait until it is measured and its report made (or a step
/// failed); returns the status line.
fn drop_and_settle(p: &mut ScriptPage, name: &str, bytes: Arc<Vec<u8>>) -> String {
    p.file_drop(name, bytes);
    pump_until(p, "the dropped drawing to settle", |p| {
        let s = status(p);
        s.starts_with("報告") || s.contains("失敗") || s.contains("無法")
    });
    status(p)
}

/// The saved report: every page's text, checked for layout faults on the
/// way. No two glyphs overlap (their boxes, shrunk so that neighbours on a
/// line may touch, do not intersect) and every glyph is inside the border.
fn report_pages(pdf: &Arc<Vec<u8>>) -> Vec<String> {
    let doc = anypage_pdf::PdfDoc::open(pdf.clone(), "").expect("the saved report re-opens");
    let snap = doc.snapshot().expect("snapshot");
    let mut pages = Vec::new();
    for i in 0..snap.page_count() {
        let t = snap.text(i).expect("page text");
        let glyphs: Vec<_> = t.glyphs.iter().filter(|g| !g.invisible && !g.text.trim().is_empty()).collect();
        let mut boxes: Vec<([f32; 4], &str)> = Vec::new();
        for g in &glyphs {
            let [x0, y0, x1, y1] = g.bbox;
            assert!(x0 >= 18.0 && y0 >= 18.0 && x1 <= 824.0 && y1 <= 577.0, "page {}: glyph {:?} at {:?} is outside the border", i + 1, g.text, g.bbox);
            let (dx, dy) = ((x1 - x0) * 0.15, (y1 - y0) * 0.2);
            boxes.push(([x0 + dx, y0 + dy, x1 - dx, y1 - dy], g.text.as_str()));
        }
        boxes.sort_by(|a, b| a.0[0].total_cmp(&b.0[0]));
        for a in 0..boxes.len() {
            for b in a + 1..boxes.len() {
                if boxes[b].0[0] >= boxes[a].0[2] {
                    break;
                }
                let (p, q) = (boxes[a].0, boxes[b].0);
                assert!(p[1] >= q[3] || q[1] >= p[3], "page {}: glyphs {:?} {:?} and {:?} {:?} overlap", i + 1, boxes[a].1, p, boxes[b].1, q);
            }
        }
        pages.push(t.plain());
    }
    pages
}

/// The files the page asked the host to save, by name.
fn saved_files(p: &mut ScriptPage) -> BTreeMap<String, Arc<Vec<u8>>> {
    let mut out = BTreeMap::new();
    while let Some(r) = p.take_request() {
        if let PageRequest::SaveFile { name, bytes, .. } = r {
            out.insert(name, bytes);
        }
    }
    out
}

// ---------------------------------------------------------------- sample files

/// The floor plan, built with the cad module's API (layers, blocks, inserts,
/// wide polylines with a bulge, closed room outlines, labels, dimension
/// lines, text) and written with `dwg:save()`.
const PLAN_SCRIPT: &str = r##"
local W, H = 12000, 9000
local dwg = cad.new()
for _, l in {
	{ name = "A-WALL", aci = 7 },
	{ name = "A-DOOR", aci = 30 },
	{ name = "A-GLAZ", aci = 150 },
	{ name = "A-AREA", aci = 4 },
	{ name = "A-AREA-IDEN", aci = 2 },
	{ name = "A-DIMS", aci = 3 },
	{ name = "A-FURN", aci = 8 },
	{ name = "A-ANNO", aci = 7 },
} do
	dwg = cad.add_layer(dwg, l)
end
-- a door: leaf and swing; the opening runs along +x from the hinge
dwg = cad.add_block(dwg, { name = "DOOR", base = { 0, 0 }, entities = {
	{ type = "LWPOLYLINE", points = { 0, 0, 45, 0, 45, 900, 0, 900 }, closed = true },
	{ type = "ARC", cx = 0, cy = 0, r = 900, start = 0, ["end"] = 90 },
} })
-- a window: three glass lines and two jambs, 1500 wide, centred on the wall
dwg = cad.add_block(dwg, { name = "WINDOW", base = { 0, 0 }, entities = {
	{ type = "LINE", x1 = 0, y1 = -100, x2 = 1500, y2 = -100 },
	{ type = "LINE", x1 = 0, y1 = 0, x2 = 1500, y2 = 0 },
	{ type = "LINE", x1 = 0, y1 = 100, x2 = 1500, y2 = 100 },
	{ type = "LINE", x1 = 0, y1 = -100, x2 = 0, y2 = 100 },
	{ type = "LINE", x1 = 1500, y1 = -100, x2 = 1500, y2 = 100 },
} })

local E = {}
local function add(e)
	table.insert(E, e)
end
-- exterior wall: 200 wide, with a bay window bulging south
add({ type = "LWPOLYLINE", layer = "A-WALL", width = 200, closed = true,
	points = { 0, 0, 2000, 0, 5000, 0, W, 0, W, H, 0, H }, bulges = { 0, 0.4, 0, 0, 0, 0 } })
-- partitions, 120 wide, with door openings
local function wall(x1, y1, x2, y2)
	add({ type = "LWPOLYLINE", layer = "A-WALL", width = 120, points = { x1, y1, x2, y2 } })
end
wall(7000, 0, 7000, 1000)
wall(7000, 1900, 7000, 3900)
wall(7000, 4800, 7000, H)
wall(0, 5000, 1200, 5000)
wall(2100, 5000, 5600, 5000)
wall(6500, 5000, 7000, 5000)
wall(5000, 5000, 5000, H)
wall(7000, 3500, W, 3500)
-- doors
local function door(x, y, rot)
	add({ type = "INSERT", layer = "A-DOOR", block = "DOOR", x = x, y = y, rotation = rot })
end
door(0, 2900, 270)
door(7000, 1900, 270)
door(1200, 5000, 0)
door(5600, 5000, 0)
door(7000, 4800, 270)
-- windows
local function win(x, y, rot, sx)
	add({ type = "INSERT", layer = "A-GLAZ", block = "WINDOW", x = x, y = y, rotation = rot or 0, sx = sx or 1, sy = 1 })
end
win(1700, H)
win(5400, H, 0, 0.8)
win(8700, H)
win(W, 5400, 90)
win(W, 1000, 90)
-- rooms: closed outlines at the wall faces
local function room(pts, bulges)
	add({ type = "LWPOLYLINE", layer = "A-AREA", closed = true, points = pts, bulges = bulges })
end
room({ 100, 100, 2000, 100, 5000, 100, 6940, 100, 6940, 4940, 100, 4940 }, { 0, 0.4, 0, 0, 0, 0 })
room({ 7060, 100, 11900, 100, 11900, 3440, 7060, 3440 })
room({ 100, 5060, 4940, 5060, 4940, 8900, 100, 8900 })
room({ 5060, 5060, 6940, 5060, 6940, 8900, 5060, 8900 })
room({ 7060, 3560, 11900, 3560, 11900, 8900, 7060, 8900 })
-- room names and floor finishes
local function label(x, y, name, finish, h)
	h = h or 300
	add({ type = "TEXT", layer = "A-AREA-IDEN", x = x, y = y, text = name, height = h, halign = "center" })
	add({ type = "TEXT", layer = "A-AREA-IDEN", x = x, y = y - h * 1.6, text = finish, height = h * 0.6, halign = "center", aci = 8 })
end
label(3500, 1900, "客廳 LIVING", "地坪 木地板 WOOD")
label(9000, 2300, "廚房 KITCHEN", "地坪 磁磚 TILE")
label(3300, 5900, "主臥室 MASTER", "地坪 木地板 WOOD")
label(6050, 6500, "浴室 BATH", "地坪 磁磚 TILE", 220)
label(9480, 5000, "次臥室 BEDROOM", "地坪 木地板 WOOD")
-- furniture
local function box(x1, y1, x2, y2)
	add({ type = "LWPOLYLINE", layer = "A-FURN", closed = true, points = { x1, y1, x2, y1, x2, y2, x1, y2 } })
end
box(1700, 6600, 3500, 8800)
box(1850, 8350, 2550, 8700)
box(2650, 8350, 3350, 8700)
box(8700, 6600, 10300, 8800)
box(8850, 8350, 10150, 8700)
box(1500, 4000, 4500, 4800)
box(2300, 3000, 3700, 3600)
box(5200, 7700, 6800, 8750)
add({ type = "CIRCLE", layer = "A-FURN", cx = 5350, cy = 7250, r = 200 })
add({ type = "LWPOLYLINE", layer = "A-FURN", closed = true,
	points = { 7400, 300, 11700, 300, 11700, 3200, 11100, 3200, 11100, 900, 7400, 900 } })
add({ type = "CIRCLE", layer = "A-FURN", cx = 11400, cy = 2400, r = 180 })
-- dimensions, drawn as lines, ticks and text
local function tick(x, y)
	add({ type = "LINE", layer = "A-DIMS", x1 = x - 150, y1 = y - 150, x2 = x + 150, y2 = y + 150 })
end
local function hdim(x1, x2, y, from_y)
	add({ type = "LINE", layer = "A-DIMS", x1 = x1, y1 = y, x2 = x2, y2 = y })
	for _, x in { x1, x2 } do
		add({ type = "LINE", layer = "A-DIMS", x1 = x, y1 = from_y, x2 = x, y2 = y + (if y > from_y then 200 else -200) })
		tick(x, y)
	end
	add({ type = "TEXT", layer = "A-DIMS", x = (x1 + x2) / 2, y = y + 100, text = tostring(x2 - x1), height = 250, halign = "center" })
end
local function vdim(y1, y2, x, from_x)
	add({ type = "LINE", layer = "A-DIMS", x1 = x, y1 = y1, x2 = x, y2 = y2 })
	for _, y in { y1, y2 } do
		add({ type = "LINE", layer = "A-DIMS", x1 = from_x, y1 = y, x2 = x + (if x > from_x then 200 else -200), y2 = y })
		tick(x, y)
	end
	add({ type = "TEXT", layer = "A-DIMS", x = x - 100, y = (y1 + y2) / 2, text = tostring(y2 - y1), height = 250, rotation = 90, halign = "center" })
end
hdim(0, W, -1400, -200)
hdim(0, 5000, H + 1200, H + 200)
hdim(5000, 7000, H + 1200, H + 200)
hdim(7000, W, H + 1200, H + 200)
vdim(0, H, W + 1400, W + 200)
vdim(0, 5000, -1400, -200)
vdim(5000, H, -1400, -200)
-- title and north arrow
add({ type = "MTEXT", layer = "A-ANNO", x = 0, y = -2200, height = 350, width = 12000,
	text = "AnyPage 示範住宅 一層平面圖 1F PLAN\\P單位 mm · 比例 1:100 · 以 cad 模組產生" })
add({ type = "CIRCLE", layer = "A-ANNO", cx = 13600, cy = -2000, r = 450 })
add({ type = "LWPOLYLINE", layer = "A-ANNO", closed = true, points = { 13600, -1550, 13800, -2250, 13600, -2100, 13400, -2250 } })
add({ type = "TEXT", layer = "A-ANNO", x = 13600, y = -1400, text = "N", height = 300, halign = "center" })
dwg = cad.add(dwg, E)
Out = page.input("out", dwg:save())
"##;

fn plan_dxf() -> Vec<u8> {
    let mut p = script_page(r#""cad""#, PLAN_SCRIPT, &[]).unwrap_or_else(|e| panic!("plan script: {e}"));
    let bytes = match get(&mut p, "out") {
        Data::Bytes(b) => b.to_vec(),
        other => panic!("plan script gave {other:?}"),
    };
    // the drawing is in millimetres: $INSUNITS 4 (the cad API has no header setter)
    let text = String::from_utf8(bytes).expect("ASCII DXF");
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.split(nl).map(str::to_string).collect();
    let at = lines.iter().position(|l| l.trim() == "$INSUNITS").expect("$INSUNITS in the header");
    assert_eq!(lines[at + 1].trim(), "70");
    lines[at + 2] = "     4".into();
    lines.join(nl).into_bytes()
}

/// A STEP (ISO 10303-21, AP214) file with one solid: a prism over `profile`
/// (counter-clockwise, mm) extruded `height` along z, written as a planar
/// B-rep (vertices, lines, edge loops, planes, a closed shell).
fn prism_step(name: &str, profile: &[(f64, f64)], height: f64) -> String {
    struct W(Vec<String>);
    impl W {
        fn add(&mut self, s: String) -> usize {
            self.0.push(s);
            self.0.len()
        }
        fn point(&mut self, p: [f64; 3]) -> usize {
            self.add(format!("CARTESIAN_POINT('',({},{},{}))", r(p[0]), r(p[1]), r(p[2])))
        }
        fn dir(&mut self, d: [f64; 3]) -> usize {
            self.add(format!("DIRECTION('',({},{},{}))", r(d[0]), r(d[1]), r(d[2])))
        }
        fn placement(&mut self, o: [f64; 3], z: [f64; 3], x: [f64; 3]) -> usize {
            let (o, z, x) = (self.point(o), self.dir(z), self.dir(x));
            self.add(format!("AXIS2_PLACEMENT_3D('',#{o},#{z},#{x})"))
        }
    }
    fn r(x: f64) -> String {
        let s = format!("{x:.6}");
        if s == "-0.000000" {
            "0.000000".into()
        } else {
            s
        }
    }
    fn unit(a: [f64; 3], b: [f64; 3]) -> ([f64; 3], f64) {
        let d = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        ([d[0] / l, d[1] / l, d[2] / l], l)
    }
    let n = profile.len();
    let bot: Vec<[f64; 3]> = profile.iter().map(|&(x, y)| [x, y, 0.0]).collect();
    let top: Vec<[f64; 3]> = profile.iter().map(|&(x, y)| [x, y, height]).collect();
    let mut w = W(Vec::new());
    // product structure, so other STEP readers see a part
    let app = w.add("APPLICATION_CONTEXT('core data for automotive mechanical design processes')".into());
    w.add(format!("APPLICATION_PROTOCOL_DEFINITION('international standard','automotive_design',2000,#{app})"));
    let pc = w.add(format!("PRODUCT_CONTEXT('',#{app},'mechanical')"));
    let prod = w.add(format!("PRODUCT('{name}','{name}','AnyPage studio sample part',(#{pc}))"));
    let pdf = w.add(format!("PRODUCT_DEFINITION_FORMATION('','',#{prod})"));
    let pdc = w.add(format!("PRODUCT_DEFINITION_CONTEXT('part definition',#{app},'design')"));
    let pd = w.add(format!("PRODUCT_DEFINITION('design','',#{pdf},#{pdc})"));
    let pds = w.add(format!("PRODUCT_DEFINITION_SHAPE('','',#{pd})"));
    let mm = w.add("(LENGTH_UNIT() NAMED_UNIT(*) SI_UNIT(.MILLI.,.METRE.))".into());
    let rad = w.add("(NAMED_UNIT(*) PLANE_ANGLE_UNIT() SI_UNIT($,.RADIAN.))".into());
    let sr = w.add("(NAMED_UNIT(*) SI_UNIT($,.STERADIAN.) SOLID_ANGLE_UNIT())".into());
    let unc = w.add(format!("UNCERTAINTY_MEASURE_WITH_UNIT(LENGTH_MEASURE(1.E-07),#{mm},'distance_accuracy_value','')"));
    let ctx = w.add(format!(
        "(GEOMETRIC_REPRESENTATION_CONTEXT(3) GLOBAL_UNCERTAINTY_ASSIGNED_CONTEXT((#{unc})) GLOBAL_UNIT_ASSIGNED_CONTEXT((#{mm},#{rad},#{sr})) REPRESENTATION_CONTEXT('',''))"
    ));
    let origin = w.placement([0.0; 3], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]);

    // topology
    let vb: Vec<usize> = bot.iter().map(|&p| {
        let c = w.point(p);
        w.add(format!("VERTEX_POINT('',#{c})"))
    }).collect();
    let vt: Vec<usize> = top.iter().map(|&p| {
        let c = w.point(p);
        w.add(format!("VERTEX_POINT('',#{c})"))
    }).collect();
    let edge = |w: &mut W, a: [f64; 3], b: [f64; 3], va: usize, vb: usize| -> usize {
        let (d, l) = unit(a, b);
        let p = w.point(a);
        let dd = w.dir(d);
        let v = w.add(format!("VECTOR('',#{dd},{})", r(l)));
        let line = w.add(format!("LINE('',#{p},#{v})"));
        w.add(format!("EDGE_CURVE('',#{va},#{vb},#{line},.T.)"))
    };
    let eb: Vec<usize> = (0..n).map(|i| edge(&mut w, bot[i], bot[(i + 1) % n], vb[i], vb[(i + 1) % n])).collect();
    let et: Vec<usize> = (0..n).map(|i| edge(&mut w, top[i], top[(i + 1) % n], vt[i], vt[(i + 1) % n])).collect();
    let ev: Vec<usize> = (0..n).map(|i| edge(&mut w, bot[i], top[i], vb[i], vt[i])).collect();
    let face = |w: &mut W, edges: &[(usize, bool)], origin: [f64; 3], normal: [f64; 3], xdir: [f64; 3]| -> usize {
        let oe: Vec<String> = edges
            .iter()
            .map(|&(e, fwd)| format!("#{}", w.add(format!("ORIENTED_EDGE('',*,*,#{e},{})", if fwd { ".T." } else { ".F." }))))
            .collect();
        let lp = w.add(format!("EDGE_LOOP('',({}))", oe.join(",")));
        let bound = w.add(format!("FACE_OUTER_BOUND('',#{lp},.T.)"));
        let pl = w.placement(origin, normal, xdir);
        let plane = w.add(format!("PLANE('',#{pl})"));
        w.add(format!("ADVANCED_FACE('',(#{bound}),#{plane},.T.)"))
    };
    let mut faces = Vec::new();
    // top (+z): the profile in order; bottom (-z): reversed
    let (d0, _) = unit(top[0], top[1]);
    let top_edges: Vec<(usize, bool)> = et.iter().map(|&e| (e, true)).collect();
    faces.push(face(&mut w, &top_edges, top[0], [0.0, 0.0, 1.0], d0));
    let bot_edges: Vec<(usize, bool)> = (0..n).rev().map(|i| (eb[i], false)).collect();
    faces.push(face(&mut w, &bot_edges, bot[0], [0.0, 0.0, -1.0], [1.0, 0.0, 0.0]));
    // sides: outward normal (dy, -dx) of a counter-clockwise profile
    for i in 0..n {
        let j = (i + 1) % n;
        let (d, _) = unit(bot[i], bot[j]);
        let edges = [(eb[i], true), (ev[j], true), (et[i], false), (ev[i], false)];
        faces.push(face(&mut w, &edges, bot[i], [d[1], -d[0], 0.0], d));
    }
    let list: Vec<String> = faces.iter().map(|f| format!("#{f}")).collect();
    let shell = w.add(format!("CLOSED_SHELL('',({}))", list.join(",")));
    let brep = w.add(format!("MANIFOLD_SOLID_BREP('{name}',#{shell})"));
    let rep = w.add(format!("ADVANCED_BREP_SHAPE_REPRESENTATION('{name}',(#{origin},#{brep}),#{ctx})"));
    w.add(format!("SHAPE_DEFINITION_REPRESENTATION(#{pds},#{rep})"));

    let mut s = String::new();
    s.push_str("ISO-10303-21;\nHEADER;\n");
    s.push_str(&format!("FILE_DESCRIPTION(('AnyPage studio sample part: {name}'),'2;1');\n"));
    s.push_str(&format!("FILE_NAME('{name}.step','2026-10-24T00:00:00',('AnyPage'),('AnyPage'),'AnyPage studio generator','AnyPage','');\n"));
    s.push_str("FILE_SCHEMA(('AUTOMOTIVE_DESIGN { 1 0 10303 214 1 1 1 1 }'));\nENDSEC;\nDATA;\n");
    for (i, e) in w.0.iter().enumerate() {
        s.push_str(&format!("#{}={};\n", i + 1, e));
    }
    s.push_str("ENDSEC;\nEND-ISO-10303-21;\n");
    s
}

/// The bracket: an L profile with a chamfered inner corner, 40 mm deep.
const BRACKET: [(f64, f64); 7] = [(0.0, 0.0), (80.0, 0.0), (80.0, 10.0), (24.0, 10.0), (10.0, 24.0), (10.0, 60.0), (0.0, 60.0)];
const BRACKET_DEPTH: f64 = 40.0;

fn bracket_step() -> String {
    prism_step("bracket", &BRACKET, BRACKET_DEPTH)
}

fn polygon_area(p: &[(f64, f64)]) -> f64 {
    let n = p.len();
    (0..n).map(|i| p[i].0 * p[(i + 1) % n].1 - p[(i + 1) % n].0 * p[i].1).sum::<f64>() / 2.0
}

fn asset(name: &str) -> Vec<u8> {
    std::fs::read(studio_dir().join("assets").join(name)).unwrap_or_else(|e| panic!("assets/{name}: {e} (run generate_sample_assets)"))
}

/// Net room areas of the sample plan in m² (name, area): rectangles at the
/// wall faces, the living room with its bay (a circular segment, bulge 0.4
/// over a 3 m chord).
fn expected_rooms() -> Vec<(&'static str, f64)> {
    let theta = 4.0 * 0.4f64.atan();
    let radius = 3000.0 / (2.0 * (theta / 2.0).sin());
    let bay = radius * radius / 2.0 * (theta - theta.sin()) / 1e6;
    vec![
        ("客廳 LIVING", 6.84 * 4.84 + bay),
        ("廚房 KITCHEN", 4.84 * 3.34),
        ("主臥室 MASTER", 4.84 * 3.84),
        ("浴室 BATH", 1.88 * 3.84),
        ("次臥室 BEDROOM", 4.84 * 5.34),
    ]
}

/// The bundled files open with the modules: the plan's layers, rooms and
/// units, and the STEP part as a closed mesh of the right volume.
#[test]
fn sample_assets_open() {
    let src = r#"
        local dwg = cad.open(page.asset("floorplan.dxf"), "dxf")
        local info = dwg:info()
        local names = {}
        for _, l in dwg:layers() do
            names[l.name] = l.entities
        end
        local areas = {}
        for _, r in dwg:entities({ layer = "A-AREA" }) do
            table.insert(areas, dwg:measure({ r.id }).area)
        end
        Plan = page.input("plan", { units = info.units, entities = info.entities, layers = names, areas = areas })
        local m = cad.step_to_mesh(page.asset("bracket.step"))
        local pos, idx = m.positions, m.indices
        local function p(i)
            local o = buffer.readu32(idx, i * 4) * 12
            return buffer.readf32(pos, o), buffer.readf32(pos, o + 4), buffer.readf32(pos, o + 8)
        end
        local vol = 0
        for t = 0, m.triangles - 1 do
            local ax, ay, az = p(t * 3)
            local bx, by, bz = p(t * 3 + 1)
            local cx, cy, cz = p(t * 3 + 2)
            vol += (ax * (by * cz - bz * cy) - ay * (bx * cz - bz * cx) + az * (bx * cy - by * cx)) / 6
        end
        Part = page.input("part", { triangles = m.triangles, shells = m.shells, failed = #m.failed, volume = vol })
    "#;
    let mut p = script_page(r#""cad""#, src, &[("floorplan.dxf", asset("floorplan.dxf")), ("bracket.step", asset("bracket.step"))]).unwrap_or_else(|e| panic!("{e}"));
    let plan = get(&mut p, "plan");
    assert_eq!(num(&plan, "units"), 4.0, "the plan is in millimetres");
    for layer in ["A-WALL", "A-DOOR", "A-GLAZ", "A-AREA", "A-AREA-IDEN", "A-DIMS", "A-FURN", "A-ANNO"] {
        assert!(num(plan.get("layers"), layer) > 0.0, "layer {layer} has entities: {plan:?}");
    }
    let mut areas: Vec<f64> = plan.get("areas").items().iter().filter_map(Data::num).map(|a| a / 1e6).collect();
    let mut want: Vec<f64> = expected_rooms().iter().map(|r| r.1).collect();
    areas.sort_by(f64::total_cmp);
    want.sort_by(f64::total_cmp);
    assert_eq!(areas.len(), want.len());
    for (a, w) in areas.iter().zip(&want) {
        assert!((a - w).abs() < 1e-3, "room area {a} m², expected {w}");
    }
    let part = get(&mut p, "part");
    assert_eq!(num(&part, "shells"), 1.0);
    assert_eq!(num(&part, "failed"), 0.0);
    let want = polygon_area(&BRACKET) * BRACKET_DEPTH;
    let vol = num(&part, "volume").abs();
    assert!((vol - want).abs() / want < 1e-3, "bracket volume {vol}, expected {want}");
    println!("plan: {} entities; bracket: {} triangles, {vol:.0} mm³", num(&plan, "entities"), num(&part, "triangles"));
}

// ---------------------------------------------------------------- the page

/// Centre-line lengths of the sample's walls in metres: the exterior outline
/// (with the bay's arc) and the partitions.
fn expected_walls() -> (f64, f64) {
    let theta = 4.0 * 0.4f64.atan();
    let radius = 3000.0 / (2.0 * (theta / 2.0).sin());
    let exterior = (2000.0 + 7000.0 + 9000.0 + 12000.0 + 9000.0 + radius * theta) / 1000.0;
    let partitions = (1000.0 + 2000.0 + 4200.0 + 1200.0 + 3500.0 + 500.0 + 4000.0 + 5000.0) / 1000.0;
    (exterior, partitions)
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn rows(tt: &Data) -> Vec<Data> {
    tt.get("rows").items().to_vec()
}

fn row<'a>(rows: &'a [Data], name: &str) -> &'a Data {
    rows.iter().find(|r| r.get("name").as_str() == Some(name)).unwrap_or_else(|| panic!("no takeoff row {name}: {rows:?}"))
}

/// The workbook's arithmetic, checked from outside: every amount is
/// quantity × unit price (rounded), the subtotal their sum, the tax 5 %.
fn check_totals(tt: &Data) {
    let mut sum = 0.0;
    for r in rows(tt) {
        let (q, p, a) = (num(&r, "qty"), num(&r, "price"), num(&r, "amount"));
        assert_eq!(a, (q * p).round(), "amount of {r:?}");
        sum += a;
    }
    assert_eq!(num(tt, "subtotal"), sum);
    assert_eq!(num(tt, "tax"), (sum * num(tt, "rate")).round());
    assert_eq!(num(tt, "total"), num(tt, "subtotal") + num(tt, "tax"));
    assert!(tt.get("errors").items().is_empty(), "formula errors: {:?}", tt.get("errors"));
}

/// 1234567 -> "1,234,567", as the page and the report print money.
fn money(n: f64) -> String {
    let s = format!("{}", n.round() as i64);
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The sample is measured and priced while the page loads.
#[test]
fn loads_and_takes_off_the_sample() {
    let mut p = load();
    println!("{}", p.summary());
    assert!(p.load_ms < p.manifest.limits.load_ms, "loaded in {:.0} ms, limit {}", p.load_ms, p.manifest.limits.load_ms);
    let tt = get(&mut p, "totals");
    let rs = rows(&tt);
    for (name, area) in expected_rooms() {
        let r = row(&rs, name);
        assert!((num(r, "qty") - round2(area)).abs() < 0.0051, "{name}: {} m², expected {area:.3}", num(r, "qty"));
        let finish = if name.contains("KITCHEN") || name.contains("BATH") { "tile" } else { "wood" };
        assert_eq!(r.get("key").as_str(), Some(finish), "{name} floor finish");
        assert_eq!(r.get("ids").items().len(), 1, "{name} is one outline");
    }
    let (ext, int) = expected_walls();
    assert!((num(row(&rs, "外牆 Exterior walls"), "qty") - round2(ext)).abs() < 0.0051, "exterior {ext}");
    assert!((num(row(&rs, "隔間牆 Partitions"), "qty") - round2(int)).abs() < 0.0051, "partitions {int}");
    assert_eq!(num(row(&rs, "門 Doors"), "qty"), 5.0);
    assert_eq!(num(row(&rs, "窗 Windows"), "qty"), 5.0);
    let paint = num(row(&rs, "牆面油漆 Wall paint"), "qty");
    assert!((paint - round2((round2(int) * 2.0 + round2(ext)) * 2.8)).abs() < 1e-9, "paint {paint}");
    assert_eq!(num(&tt, "rate"), 0.05);
    check_totals(&tt);
    println!("sample takeoff: subtotal {} tax {} total {}", num(&tt, "subtotal"), num(&tt, "tax"), num(&tt, "total"));
    // the report was made at load and is current
    let rs = get(&mut p, "report_status");
    assert_eq!(rs.get("stale"), &Data::Bool(false), "{rs:?}");
    assert!(num(&rs, "paths") > 50.0, "the plan is in the report as paths: {rs:?}");
    assert_no_error_cards(&mut p);
    assert_eq!(p.handler_errors, 0);
}

/// Editing a unit price changes the workbook, and the derived totals follow.
#[test]
fn set_price_recomputes_totals() {
    let mut p = load();
    let before = get(&mut p, "totals");
    let wood = |tt: &Data| -> f64 { rows(tt).iter().filter(|r| r.get("key").as_str() == Some("wood")).map(|r| num(r, "amount")).sum() };
    let wood_qty: Vec<f64> = rows(&before).iter().filter(|r| r.get("key").as_str() == Some("wood")).map(|r| num(r, "qty")).collect();
    assert_eq!(wood_qty.len(), 3, "living, master and bedroom have wood floors");

    let r = act(&mut p, "set_price", Data::map([("item", Data::str("wood")), ("price", Data::Num(3000.0))]));
    assert_eq!(r.get("item").as_str(), Some("wood"));
    assert_eq!(num(&r, "price"), 3000.0);
    let after = get(&mut p, "totals");
    check_totals(&after);
    assert_eq!(num(after.get("prices"), "wood"), 3000.0);
    let want: f64 = wood_qty.iter().map(|q| (q * 3000.0).round()).sum();
    assert_eq!(wood(&after), want);
    assert_eq!(num(&after, "subtotal") - num(&before, "subtotal"), want - wood(&before));
    assert_eq!(num(&r, "total"), num(&after, "total"), "the action answers with the new total");

    // by part of a label; the tax rate as percent
    act(&mut p, "set_price", Data::map([("item", Data::str("磁磚")), ("price", Data::Num(3500.0))]));
    act(&mut p, "set_price", Data::map([("item", Data::str("tax")), ("price", Data::Num(10.0))]));
    let tt = get(&mut p, "totals");
    assert_eq!(num(tt.get("prices"), "tile"), 3500.0);
    assert_eq!(num(&tt, "rate"), 0.1);
    check_totals(&tt);

    // bad input is refused and changes nothing
    let total = num(&tt, "total");
    assert!(p.run_action("set_price", &Data::map([("item", Data::str("gold")), ("price", Data::Num(1.0))])).is_err());
    assert!(p.run_action("set_price", &Data::map([("item", Data::str("wood")), ("price", Data::Num(-5.0))])).is_err());
    assert!(p.run_action("set_price", &Data::map([("item", Data::str("wood")), ("price", Data::str("cheap"))])).is_err());
    assert_eq!(num(&get(&mut p, "totals"), "total"), total);

    // the report made at load no longer matches
    let rs = get(&mut p, "report_status");
    assert_eq!(rs.get("stale"), &Data::Bool(true), "{rs:?}");

    // the same through the agent port's JSON
    let out = p.act(r#"{"action":"set_price","params":{"item":"door","price":15000}}"#);
    assert!(out.contains("\"ok\":true"), "{out}");
    assert_eq!(num(get(&mut p, "totals").get("prices"), "door"), 15000.0);
    let desc = p.act(r#"{"describe":true}"#);
    for a in ["measure_rooms", "set_price", "make_report", "save", "takeoff.describe"] {
        assert!(desc.contains(&format!("\"name\":\"{a}\"")), "{a} missing from the semantic port");
    }
}

/// The report: made from the current data, saved as PDF, and the saved file
/// re-opens with the plan drawn as paths and the takeoff table as text. The
/// XLSX and DXF saves re-open with the same numbers and entities.
#[test]
fn report_and_saved_files() {
    let mut p = load();
    act(&mut p, "set_price", Data::map([("item", Data::str("ext_wall")), ("price", Data::Num(7200.0))]));
    let t0 = Instant::now();
    let r = act(&mut p, "make_report", Data::Nil);
    let report_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let tt = get(&mut p, "totals");
    assert_eq!(num(&r, "total"), num(&tt, "total"));
    assert_eq!(num(&r, "pages"), 1.0);
    assert_eq!(get(&mut p, "report_status").get("stale"), &Data::Bool(false));

    // PDF
    let s = act(&mut p, "save", Data::map([("format", Data::str("pdf"))]));
    let files = saved_files(&mut p);
    let pdf = files.get("studio-report.pdf").expect("a PDF save request").clone();
    assert_eq!(num(&s, "bytes") as usize, pdf.len());
    let pages = report_pages(&pdf);
    assert_eq!(pages.len(), 1, "the sample's report is one sheet");
    let text = &pages[0];
    let doc = anypage_pdf::PdfDoc::open(pdf.clone(), "").expect("the saved report re-opens");
    let snap = doc.snapshot().expect("snapshot");
    for want in ["Quantity takeoff", "Subtotal", "Total", "Unit prices", &money(num(&tt, "total")), &money(num(&tt, "subtotal")), "LIVING", "KITCHEN", "Exterior walls"] {
        assert!(text.contains(want), "report text lacks {want:?}:\n{text}");
    }
    let cjk = ["總計", "客廳"].iter().all(|w| text.contains(w));
    println!("report: {} bytes, {} paths, made in {report_ms:.1} ms; CJK text extracted: {cjk}", pdf.len(), num(&r, "paths"));
    // the plan is drawn inside its frame: dark ink there, as paths (not an image)
    let img = anypage_pdf::render::render_page(&snap, 0, 842.0, [255, 255, 255, 255], &|| false, &|_| {}).expect("render").expect("not cancelled");
    let dark = |x0: u32, y0: u32, x1: u32, y1: u32| {
        let mut n = 0;
        for y in y0..y1 {
            for x in x0..x1 {
                let i = ((y * img.w + x) * 4) as usize;
                if (img.rgba[i] as u32 + img.rgba[i + 1] as u32 + img.rgba[i + 2] as u32) < 300 {
                    n += 1;
                }
            }
        }
        n
    };
    let plan = dark(45, 75, 520, 540);
    let area = (520 - 45) * (540 - 75);
    assert!(plan > area / 50, "the plan's frame has {plan} dark pixels of {area}");
    write_png(&out_dir().join("report.png"), img.w, img.h, &img.rgba);
    let raw = String::from_utf8_lossy(&pdf);
    assert!(!raw.contains("/Subtype /Image") && !raw.contains("/Subtype/Image"), "the plan is vector paths, not an image");

    // XLSX: exported by the eager job; saved when that export is current
    let s = act(&mut p, "save", Data::map([("format", Data::str("xlsx"))]));
    let mut files = saved_files(&mut p);
    if s.get("pending").truthy() {
        pump_until(&mut p, "the XLSX export", |p| {
            files.extend(saved_files(p));
            files.contains_key("studio-takeoff.xlsx")
        });
    }
    let xlsx = files.get("studio-takeoff.xlsx").expect("an XLSX save request").clone();
    assert!(xlsx.starts_with(b"PK"), "xlsx is a zip");

    // DXF
    act(&mut p, "save", Data::map([("format", Data::str("dxf"))]));
    let dxf = saved_files(&mut p).get("floorplan.dxf").expect("a DXF save request").clone();

    let total_ref = tt.get("refs").get("total").as_str().expect("total ref").to_string();
    let src = format!(
        r#"
        local wb = sheet.open(page.asset("t.xlsx"))
        local dwg = cad.open(page.asset("t.dxf"), "dxf")
        local orig = cad.open(page.asset("o.dxf"), "dxf")
        Check = page.input("check", {{
            total = wb:get("{total_ref}"),
            formula = wb:formula("{total_ref}"),
            ext_price = wb:get("Prices!D4"),
            sheets = wb:sheets(),
            entities = dwg:info().entities,
            orig_entities = orig:info().entities,
        }})
    "#
    );
    let mut c = script_page(r#""sheet", "cad""#, &src, &[("t.xlsx", xlsx.to_vec()), ("t.dxf", dxf.to_vec()), ("o.dxf", asset("floorplan.dxf"))]).unwrap_or_else(|e| panic!("{e}"));
    let check = get(&mut c, "check");
    assert_eq!(num(&check, "total"), num(&tt, "total"), "the saved workbook computes the same total");
    assert!(check.get("formula").as_str().is_some_and(|f| f.starts_with('=')), "totals stay formulas: {check:?}");
    assert_eq!(num(&check, "ext_price"), 7200.0);
    assert_eq!(num(&check, "entities"), num(&check, "orig_entities"));
    assert_no_error_cards(&mut p);
    assert_eq!(p.handler_errors, 0);
}

/// A dropped DXF opens through the cad.load job; when it is done the page
/// measures it, rewrites the takeoff and remakes the report.
#[test]
fn dropped_drawing_is_measured() {
    let src = r#"
        local dwg = cad.new()
        dwg = cad.add_layer(dwg, { name = "ROOMS", aci = 4 })
        dwg = cad.add_layer(dwg, { name = "ROOM-NAMES", aci = 2 })
        dwg = cad.add_layer(dwg, { name = "WALLS", aci = 7 })
        dwg = cad.add(dwg, {
            { type = "LWPOLYLINE", layer = "WALLS", closed = true, width = 200, points = { 0, 0, 6000, 0, 6000, 4000, 0, 4000 } },
            { type = "LWPOLYLINE", layer = "ROOMS", closed = true, points = { 100, 100, 5900, 100, 5900, 3900, 100, 3900 } },
            { type = "TEXT", layer = "ROOM-NAMES", x = 3000, y = 2000, text = "辦公室 OFFICE", height = 300, halign = "center" },
            { type = "TEXT", layer = "ROOM-NAMES", x = 3000, y = 1500, text = "TILE", height = 150, halign = "center" },
        })
        Out = page.input("out", dwg:save())
    "#;
    let mut g = script_page(r#""cad""#, src, &[]).unwrap_or_else(|e| panic!("{e}"));
    let bytes = match get(&mut g, "out") {
        Data::Bytes(b) => b,
        other => panic!("{other:?}"),
    };
    let mut p = load();
    let report_before = get(&mut p, "report_meta");
    p.file_drop("office.dxf", bytes);
    assert_no_error_cards(&mut p);
    assert_eq!(get(&mut p, "file_name").as_str(), Some("office.dxf"));
    pump_until(&mut p, "the dropped drawing to load and be measured", |p| {
        let tt = get(p, "totals");
        rows(&tt).iter().any(|r| r.get("name").as_str() == Some("辦公室 OFFICE"))
    });
    let tt = get(&mut p, "totals");
    let rs = rows(&tt);
    assert_eq!(rs.iter().filter(|r| r.get("kind").as_str() == Some("room")).count(), 1);
    let office = row(&rs, "辦公室 OFFICE");
    assert!((num(office, "qty") - 5.8 * 3.8).abs() < 0.0051);
    assert_eq!(office.get("key").as_str(), Some("tile"));
    // the unitless drawing is taken as millimetres from its size
    assert_eq!(get(&mut p, "takeoff").get("unit").as_str(), Some("mm?"));
    assert!((num(row(&rs, "外牆 Exterior walls"), "qty") - 20.0).abs() < 1e-9);
    check_totals(&tt);
    let report_after = get(&mut p, "report_meta");
    assert!(num(&report_after, "n") > num(&report_before, "n"), "the report was remade");
    assert_eq!(get(&mut p, "report_status").get("stale"), &Data::Bool(false));
    assert_no_error_cards(&mut p);
    assert_eq!(p.handler_errors, 0);
}

/// A dropped file that is not a drawing: the page says so and shows the
/// drawing, takeoff and report it had.
#[test]
fn a_bad_file_keeps_the_drawing() {
    let mut p = load();
    let before = get(&mut p, "totals");
    let dwg = get(&mut p, "drawing");
    p.file_drop("broken.dxf", Arc::new(b"AutoCAD Binary DXF\r\n\x1a\x00\xff\xfe garbage".to_vec()));
    pump_until(&mut p, "the failed load", |p| get(p, "file_name").as_str() == Some("floorplan.dxf"));
    let status = get(&mut p, "status");
    assert!(status.as_str().is_some_and(|s| s.contains("無法開啟 broken.dxf")), "{status:?}");
    assert_eq!(get(&mut p, "drawing"), dwg, "the previous drawing is back");
    assert_eq!(num(&get(&mut p, "totals"), "total"), num(&before, "total"));
    assert_eq!(get(&mut p, "report_status").get("stale"), &Data::Bool(false));
    assert_no_error_cards(&mut p);
    assert_eq!(p.handler_errors, 0);
}

/// Without the optional scene module the page loads, says so in the 3D
/// corner, and everything else works.
#[test]
fn loads_without_the_scene_module() {
    let mut n = anypage_demo::script::natives();
    anypage_ui::register(&mut n);
    anypage_pdf::register(&mut n);
    anypage_sheet::register(&mut n);
    anypage_cad::register(&mut n);
    assert!(!n.has_module("scene"));
    let mut p = load_with(&n);
    assert!(p.cell_id("part_mesh").is_none(), "no 3D cells without the scene module");
    check_totals(&get(&mut p, "totals"));
    act(&mut p, "set_price", Data::map([("item", Data::str("paint")), ("price", Data::Num(400.0))]));
    act(&mut p, "make_report", Data::Nil);
    assert_no_error_cards(&mut p);
    assert_eq!(p.handler_errors, 0);
}

/// With the scene module the STEP part becomes a mesh through the job.
#[test]
fn step_part_meshes() {
    let mut p = load();
    let id = p.cell_id("part_mesh").expect("the part_mesh job");
    pump_until(&mut p, "the STEP mesh", |p| {
        let v = anypage_runtime::data::input_to_data(&p.parts().0.view(id));
        !v.get("triangles").is_nil()
    });
    let m = get(&mut p, "part_mesh");
    assert_eq!(num(&m, "shells"), 1.0);
    assert!(num(&m, "triangles") >= 24.0);
    let model = get(&mut p, "part_model");
    assert!(matches!(model, Data::Res(_)), "a scene model: {model:?}");
}

/// What one edit costs the page: the handler (workbook edit and
/// recalculation) and drawing every panel again (the takeoff table, the
/// plan overlay, the side panel), as the engine would for the next frame.
#[test]
fn edit_and_redraw_timings() {
    let mut p = load();
    let panels: Vec<_> = p.spec().panels.iter().map(|x| x.cell).collect();
    let draw_all = |p: &mut ScriptPage| {
        let t0 = Instant::now();
        for c in &panels {
            p.parts().0.demand(*c);
        }
        t0.elapsed().as_secs_f64() * 1000.0
    };
    let first = draw_all(&mut p);
    let mut edit = Vec::new();
    let mut redraw = Vec::new();
    for k in 0..10 {
        let t0 = Instant::now();
        act(&mut p, "set_price", Data::map([("item", Data::str("wood")), ("price", Data::Num(2800.0 + k as f64 * 10.0))]));
        edit.push(t0.elapsed().as_secs_f64() * 1000.0);
        redraw.push(draw_all(&mut p));
    }
    let t0 = Instant::now();
    act(&mut p, "measure_rooms", Data::Nil);
    let measure_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let t0 = Instant::now();
    act(&mut p, "make_report", Data::Nil);
    let report_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let med = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let (e, r) = (med(&mut edit), med(&mut redraw));
    println!(
        "studio timings (debug test build): load {:.1} ms, first draw of all panels {first:.1} ms, price edit {e:.2} ms, redraw after it {r:.1} ms, measure_rooms {measure_ms:.1} ms, make_report {report_ms:.1} ms",
        p.load_ms
    );
    // generous bounds: this is a debug test build on a machine that may be
    // busy; the desktop app's HUD gives the real numbers
    assert!(e < 400.0 && r < 400.0, "an edit took {e:.0} ms and its redraw {r:.0} ms");
}

fn save_pdf(p: &mut ScriptPage) -> Arc<Vec<u8>> {
    act(p, "save", Data::map([("format", Data::str("pdf"))]));
    saved_files(p).get("studio-report.pdf").expect("a PDF save request").clone()
}

fn room_names(tt: &Data) -> Vec<String> {
    rows(tt).iter().filter(|r| r.get("kind").as_str() == Some("room")).map(|r| r.get("name").as_str().unwrap_or_default().to_string()).collect()
}

/// The report is laid out from its contents: a takeoff too long for the
/// sheet continues on detail pages, the totals stay on the sheet and follow
/// the last row, and no text overlaps or leaves the page.
#[test]
fn report_layout_follows_the_takeoff() {
    for (n, want_pages) in [(12usize, 1usize), (16, 2), (40, 3)] {
        let mut p = load();
        let st = drop_and_settle(&mut p, &format!("rooms{n}.dxf"), rooms_dxf(n, 2));
        assert!(st.starts_with("報告"), "rooms{n}: {st}");
        let tt = get(&mut p, "totals");
        let names = room_names(&tt);
        assert_eq!(names.len(), n);
        check_totals(&tt);
        let pdf = save_pdf(&mut p);
        let pages = report_pages(&pdf);
        assert_eq!(pages.len(), want_pages, "rooms{n}: pages");
        let (total, sub) = (money(num(&tt, "total")), money(num(&tt, "subtotal")));
        for want in [&total, &sub, "Total", "Unit prices", "Project"] {
            assert!(pages[0].contains(want), "rooms{n}: the sheet lacks {want:?}:\n{}", pages[0]);
        }
        let last = pages.last().expect("a page");
        assert!(last.contains(&total) && last.contains(&sub), "rooms{n}: the totals follow the table:\n{last}");
        let all = pages.join("\n");
        for name in &names {
            let latin = name.split_once(' ').map_or(name.as_str(), |x| x.1);
            assert!(all.contains(latin), "rooms{n}: the report lacks {name:?}");
        }
        // for looking at: target/test-output/studio/report-rooms<n>-p<page>.png
        let doc = anypage_pdf::PdfDoc::open(pdf.clone(), "").expect("re-open");
        let snap = doc.snapshot().expect("snapshot");
        for i in 0..snap.page_count().min(3) {
            let img = anypage_pdf::render::render_page(&snap, i, 1263.0, [255, 255, 255, 255], &|| false, &|_| {}).expect("render").expect("not cancelled");
            write_png(&out_dir().join(format!("report-rooms{n}-p{}.png", i + 1)), img.w, img.h, &img.rgba);
        }
        assert_no_error_cards(&mut p);
        assert_eq!(p.handler_errors, 0);
    }
}

/// Agents get errors for values the page cannot use, and nothing breaks:
/// the view, layer and selection cells are not agent-writable, and the
/// actions that change them check their input.
#[test]
fn agent_input_is_checked() {
    let mut p = load();
    let tt = get(&mut p, "totals");
    let living = row(&rows(&tt), "客廳 LIVING").get("ids").items()[0].num().expect("an id");
    let ok = |out: &str| out.contains("\"ok\":true");
    // one id is taken as a list of one
    let out = p.act(&format!(r#"{{"action":"select","params":{{"ids":{living}}}}}"#));
    assert!(ok(&out), "{out}");
    assert_eq!(get(&mut p, "selected").items(), &[Data::Num(living)]);
    assert_no_error_cards(&mut p);
    for bad in [r#"["x"]"#, "[1.5]", "[-1]", "[99999999]", r#""64""#, "null", "[1e300]"] {
        let out = p.act(&format!(r#"{{"action":"select","params":{{"ids":{bad}}}}}"#));
        assert!(!ok(&out) && out.contains("error"), "select {bad}: {out}");
        assert_eq!(get(&mut p, "selected").items(), &[Data::Num(living)], "select {bad} kept the selection");
    }
    let out = p.act(r#"{"action":"select","params":{"ids":[]}}"#);
    assert!(ok(&out) && get(&mut p, "selected").items().is_empty(), "{out}");
    for (cell, v) in [("view", r#""x""#), ("layers", "5"), ("selected", "64"), ("part_camera", r#""x""#)] {
        let out = p.act(&format!(r#"{{"set":"{cell}","value":{v}}}"#));
        assert!(out.contains("not an agent-writable input"), "set {cell}: {out}");
    }
    // the takeoff app's state is the UI library's agent-writable input: garbage there is survived
    for v in ["5", r#"{"drafts":7}"#, r#"{"drafts":{"wood":[1,2]}}"#] {
        let out = p.act(&format!(r#"{{"set":"takeoff.state","value":{v}}}"#));
        assert!(ok(&out), "{out}");
        assert_no_error_cards(&mut p);
    }
    // view
    for bad in [r#"{"rect":"x"}"#, r#"{"rect":[0,0,0,10]}"#, r#"{"rect":[0,0,-5,10]}"#, "{}"] {
        let out = p.act(&format!(r#"{{"action":"view","params":{bad}}}"#));
        assert!(!ok(&out), "view {bad}: {out}");
    }
    let out = p.act(r#"{"action":"view","params":{"rect":[1000,1000,4000,3000]}}"#);
    assert!(ok(&out), "{out}");
    assert_eq!(get(&mut p, "view").items().iter().filter_map(Data::num).collect::<Vec<_>>(), vec![1000.0, 1000.0, 4000.0, 3000.0]);
    let out = p.act(r#"{"action":"view","params":{"rect":[0,0,1e-9,1e-9]}}"#);
    assert!(ok(&out), "{out}");
    let w = get(&mut p, "view").items()[2].num().expect("w");
    assert!(w > 1.0, "a tiny view is clamped to the zoom limit: {w}");
    assert!(ok(&p.act(r#"{"action":"view","params":{"fit":true}}"#)));
    assert!(get(&mut p, "view").is_nil());
    // layers
    for bad in [r#"{"name":"NOPE"}"#, r#"{"name":5}"#, r#"{"name":"A-FURN","on":"no"}"#] {
        let out = p.act(&format!(r#"{{"action":"layer","params":{bad}}}"#));
        assert!(!ok(&out), "layer {bad}: {out}");
    }
    assert!(ok(&p.act(r#"{"action":"layer","params":{"name":"A-FURN","on":false}}"#)));
    assert_eq!(get(&mut p, "layers").get("A-FURN"), &Data::Bool(false));
    assert_eq!(get(&mut p, "report_status").get("stale"), &Data::Bool(true));
    assert_no_error_cards(&mut p);
}

/// set_price: keys, whole labels and unique parts of labels; an ambiguous
/// part is refused with the candidates; the tax rate is percent from 1 up.
#[test]
fn set_price_items_and_tax_rules() {
    let mut p = load();
    let set = |p: &mut ScriptPage, item: &str, price: f64, unit: Option<&str>| {
        let mut m = vec![("item", Data::str(item)), ("price", Data::Num(price))];
        if let Some(u) = unit {
            m.push(("unit", Data::str(u)));
        }
        p.run_action("set_price", &Data::map(m))
    };
    for (item, key) in [("Floor tiles", "tile"), ("磁磚", "tile"), ("tiles", "tile"), ("Wood floor", "wood"), ("partition", "int_wall"), ("油漆 Wall paint", "paint")] {
        let r = set(&mut p, item, 1000.0, None).unwrap_or_else(|e| panic!("{item}: {e}"));
        assert_eq!(r.get("item").as_str(), Some(key), "{item}");
    }
    for (item, cands) in [("wall", vec!["ext_wall", "int_wall", "paint", "height"]), ("floor", vec!["wood", "tile"]), ("a", vec!["ext_wall", "tax"])] {
        let e = set(&mut p, item, 1000.0, None).expect_err(item);
        assert!(e.contains("matches several items"), "{item}: {e}");
        for c in cands {
            assert!(e.contains(c), "{item}: {e} lacks {c}");
        }
    }
    let rate = |p: &mut ScriptPage| num(&get(p, "totals"), "rate");
    set(&mut p, "tax", 1.0, None).expect("tax 1");
    assert_eq!(rate(&mut p), 0.01, "1 is 1 %");
    set(&mut p, "tax", 0.05, None).expect("tax 0.05");
    assert_eq!(rate(&mut p), 0.05, "0.05 is a fraction");
    set(&mut p, "tax", 0.5, Some("%")).expect("tax 0.5 %");
    assert_eq!(rate(&mut p), 0.005);
    set(&mut p, "tax", 0.08, Some("fraction")).expect("tax 0.08");
    assert_eq!(rate(&mut p), 0.08);
    assert!(set(&mut p, "tax", 150.0, None).is_err(), "150 % is refused");
    assert!(set(&mut p, "tax", 5.0, Some("percent")).is_err(), "unknown unit");
    check_totals(&get(&mut p, "totals"));
    // big prices stay plain numbers in the fields
    set(&mut p, "wood", 28003100.0, None).expect("big price");
    assert_no_error_cards(&mut p); // a frame: the UI describes what it drew
    let out = p.act(r#"{"action":"takeoff.describe","params":{"types":"textfield"}}"#);
    assert!(out.contains("28003100") && !out.contains("e+"), "{out}");
    assert_no_error_cards(&mut p);
}

/// Page coordinates of a drawing point with the plan showing `view`
/// (uniform fit, centred, y up), as cad.to_page computes them.
fn plan_point(view: [f64; 4], x: f64, y: f64) -> (f32, f32) {
    plan_point_in([12.0, 72.0, 780.0, 512.0], view, x, y)
}

/// The same for the plan shown in page rect `rect`.
fn plan_point_in(rect: [f64; 4], view: [f64; 4], x: f64, y: f64) -> (f32, f32) {
    let [cx, cy, cw, ch] = rect;
    let s = (cw / view[2]).min(ch / view[3]);
    let ox = cx + (cw - view[2] * s) / 2.0;
    let oy = cy + (ch - view[3] * s) / 2.0;
    ((ox + (x - view[0]) * s) as f32, (oy + (view[1] + view[3] - y) * s) as f32)
}

/// A press that is taken away (pointer_cancel) is not a click, on the plan
/// or on a UI button, and the next gesture works as usual.
#[test]
fn pointer_cancel_is_not_a_click() {
    let mut p = load();
    let view = [0.0, 0.0, 12000.0, 9000.0];
    assert!(p.act(r#"{"action":"view","params":{"rect":[0,0,12000,9000]}}"#).contains("\"ok\":true"));
    let tt = get(&mut p, "totals");
    let living = row(&rows(&tt), "客廳 LIVING").get("ids").items()[0].num().expect("an id");
    // an empty spot inside the living room
    let (x, y) = plan_point(view, 1000.0, 1500.0);
    p.pointer_down(x, y);
    p.pointer_cancel();
    assert!(get(&mut p, "selected").items().is_empty(), "a cancelled press selected {:?}", get(&mut p, "selected"));
    p.pointer_down(x, y);
    p.pointer_up();
    assert_eq!(get(&mut p, "selected").items(), &[Data::Num(living)], "a click selects the room");
    p.key("escape");
    assert!(get(&mut p, "selected").items().is_empty());

    // the report panel's 儲存 PDF button (at the panel's foot): a click saves...
    // (the UI hit-tests what it last drew: draw a frame first, as the engine does)
    assert_no_error_cards(&mut p);
    let bx = 1502.0;
    let by = (572..988)
        .step_by(4)
        .map(|y| y as f32)
        .find(|&y| {
            p.pointer_down(bx, y);
            p.pointer_up();
            saved_files(&mut p).contains_key("studio-report.pdf")
        })
        .expect("a click on 儲存 PDF saves the report");
    // ...a cancelled press does not, now or at the next release elsewhere
    p.pointer_down(bx, by);
    p.pointer_cancel();
    p.pointer_down(x, y);
    p.pointer_up();
    assert!(saved_files(&mut p).is_empty(), "a cancelled press on 儲存 PDF saved");
    assert_eq!(get(&mut p, "selected").items(), &[Data::Num(living)], "the next click on the plan still works");
    // a release in the UI later (here on the report panel's margin) does not
    // finish the cancelled press either
    p.pointer_down(1419.0, 700.0);
    p.pointer_up();
    assert!(saved_files(&mut p).is_empty(), "the cancelled press on 儲存 PDF fired later");
    assert_no_error_cards(&mut p);
    assert_eq!(p.handler_errors, 0);
}

/// Zoom stops at 1/1000 and 1000 times the drawing's size; tiny rooms get
/// no chips.
#[test]
fn zoom_is_limited() {
    let src = r#"B = page.input("b", cad.open(page.asset("floorplan.dxf"), "dxf"):bounds())"#;
    let mut c = script_page(r#""cad""#, src, &[("floorplan.dxf", asset("floorplan.dxf"))]).unwrap_or_else(|e| panic!("{e}"));
    let b = get(&mut c, "b");
    let size = b.items()[2].num().expect("w").max(b.items()[3].num().expect("h"));
    let mut p = load();
    let side = |p: &mut ScriptPage| {
        let v = get(p, "view");
        v.items()[2].num().expect("w").max(v.items()[3].num().expect("h"))
    };
    let (x, y) = (402.0, 330.0);
    for _ in 0..300 {
        p.wheel(x, y, 0.0, 1.0);
    }
    let w = side(&mut p);
    assert!((w / (size * 1e-3) - 1.0).abs() < 1e-6, "zoomed in to {w} units; the drawing is {size}");
    for _ in 0..600 {
        p.wheel(x, y, 0.0, -1.0);
    }
    let w = side(&mut p);
    assert!((w / (size * 1e3) - 1.0).abs() < 1e-6, "zoomed out to {w} units; the drawing is {size}");
    // so far out, the rooms are dots: no chips piled on them
    let overlay = p.spec().panels.iter().find(|x| x.rect.x == 12.0 && x.rect.y == 72.0).map(|x| x.cell).expect("the plan overlay");
    let g = p.parts().0;
    g.demand(overlay);
    let script = g.inputs_of(overlay)[0];
    let list = g.get::<DisplayList>(script).expect("a display list");
    assert!(find_text(&list.cmds, "m²").is_none(), "room chips at a far zoom");
    assert_no_error_cards(&mut p);
    assert_eq!(p.handler_errors, 0);
}

/// A STEP file the user opens reports in the status line when its mesh is
/// done, or why it could not be read.
#[test]
fn opened_step_part_reports_in_the_status() {
    let mut p = load();
    if p.cell_id("part_mesh").is_none() {
        return; // built without the scene module
    }
    p.file_drop("part.step", Arc::new(asset("bracket.step")));
    pump_until(&mut p, "the STEP part", |p| status(p).starts_with("STEP part.step"));
    assert!(status(&mut p).contains("三角形"), "{}", status(&mut p));
    p.file_drop("bad.step", Arc::new(b"ISO-10303-21; not really".to_vec()));
    pump_until(&mut p, "the bad STEP part", |p| status(p).contains("bad.step") && !status(p).contains("載入"));
    assert!(status(&mut p).starts_with("無法讀取 STEP bad.step"), "{}", status(&mut p));
    assert_no_error_cards(&mut p);
    assert_eq!(p.handler_errors, 0);
}

/// Big drawings: many labelled rooms, many entities. Opening one measures
/// it and makes the report inside the handler budget (the report prints the
/// takeoff layers only, or leaves texts out, and says so), and the actions
/// stay inside it too.
#[test]
fn big_drawings_stay_in_budget() {
    let budget = 1000.0;
    for (label, bytes, rooms) in [("rooms250x10", rooms_dxf(250, 10), 250usize), ("rooms320", rooms_dxf(320, 1), 300), ("lines20k", lines_dxf(20_000), 8), ("lines60k", lines_dxf(60_000), 8)] {
        let mut p = load();
        let t0 = Instant::now();
        let st = drop_and_settle(&mut p, &format!("{label}.dxf"), bytes.clone());
        let settle_ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert!(st.starts_with("報告"), "{label}: {st}");
        assert_eq!(p.handler_errors, 0, "{label}");
        let tt = get(&mut p, "totals");
        assert_eq!(room_names(&tt).len(), rooms, "{label}");
        check_totals(&tt);
        let t1 = Instant::now();
        act(&mut p, "measure_rooms", Data::Nil);
        let measure_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let t1 = Instant::now();
        let r = act(&mut p, "make_report", Data::Nil);
        let report_ms = t1.elapsed().as_secs_f64() * 1000.0;
        assert!(measure_ms < budget && report_ms < budget, "{label}: measure {measure_ms:.0} ms, report {report_ms:.0} ms");
        let pdf = save_pdf(&mut p);
        let pages = report_pages(&pdf);
        assert!(pages.last().expect("a page").contains(&money(num(&tt, "total"))), "{label}: the total is in the report");
        let t1 = Instant::now();
        assert_no_error_cards(&mut p);
        let draw_ms = t1.elapsed().as_secs_f64() * 1000.0;
        println!(
            "{label}: {} KB, open+measure+report {settle_ms:.0} ms; measure_rooms {measure_ms:.0} ms; make_report {report_ms:.0} ms ({} pages, {} paths, notes {:?}); draw all panels {draw_ms:.0} ms",
            bytes.len() / 1024,
            pages.len(),
            num(&r, "paths"),
            r.get("notes")
        );
        assert_eq!(p.handler_errors, 0, "{label}");
    }
}

/// The camera fits the page asked for since the last call (other requests
/// are dropped).
fn fits(p: &mut ScriptPage) -> Vec<[f32; 4]> {
    let mut out = Vec::new();
    while let Some(r) = p.take_request() {
        if let PageRequest::FitRect(r) = r {
            out.push([r.x, r.y, r.w, r.h]);
        }
    }
    out
}

fn layout(p: &mut ScriptPage) -> String {
    get(p, "layout").as_str().unwrap_or_default().to_string()
}

fn inside(r: [f32; 4], outer: [f32; 4]) -> bool {
    r[0] >= outer[0] && r[1] >= outer[1] && r[0] + r[2] <= outer[0] + outer[2] && r[1] + r[3] <= outer[1] + outer[3]
}

fn rect4(r: &anypage_engine::core::Rect) -> [f32; 4] {
    [r.x, r.y, r.w, r.h]
}

/// The window's size picks the arrangement: a desktop window gets the wide
/// layout, fitted again on every resize; a phone or a tall window gets the
/// narrow screens, one at a time, switched by their tabs. Every panel sits
/// in one arrangement, and the narrow plan works like the wide one.
#[test]
fn window_size_picks_the_layout() {
    let mut p = load();
    let wide = [0.0, 0.0, 1600.0, 1000.0];
    fits(&mut p);
    // the review's narrow landscape window and a square one: wide, fitted whole
    for (w, h) in [(1100.0, 760.0), (1000.0, 1000.0), (1600.0, 1000.0)] {
        p.viewport(w, h, 1.0);
        assert_eq!(layout(&mut p), "wide", "{w} x {h}");
        assert_eq!(fits(&mut p), vec![wide], "{w} x {h}");
    }
    // a phone, upright: the plan screen, right of the wide layout
    p.viewport(412.0, 915.0, 2.625);
    assert_eq!(layout(&mut p), "plan");
    let plan = fits(&mut p);
    assert_eq!(plan.len(), 1, "{plan:?}");
    let screen = plan[0];
    assert!(screen[0] >= wide[2] && screen[2] < screen[3], "a phone-shaped screen right of the wide layout: {screen:?}");
    // with the window's shape, from the top: the screen fills the width and
    // what the window has left over stays below it
    let aspect = (915.0 - 48.0) / (412.0 - 48.0);
    assert!(screen[1] == 0.0 && (screen[3] / screen[2] - aspect).abs() < 1e-3, "{screen:?}");
    // the three screens, through the agent action
    let mut screens = Vec::new();
    for name in ["plan", "takeoff", "report"] {
        act(&mut p, "layout", Data::map([("layout", Data::str(name))]));
        assert_eq!(layout(&mut p), name);
        screens.push(fits(&mut p)[0]);
    }
    assert_eq!(screens[0], screen);
    assert!(p.run_action("layout", &Data::map([("layout", Data::str("tiny"))])).is_err());
    assert_eq!(layout(&mut p), "report", "a bad layout changes nothing");
    let errors = p.handler_errors; // the refused action counts as one
    // every panel, tiled layer and GPU layer is inside the wide layout or one screen
    let spec = p.spec();
    let rects: Vec<[f32; 4]> = spec.panels.iter().map(|x| rect4(&x.rect)).chain(spec.tiled.iter().map(|t| rect4(&t.rect))).chain(spec.layers.iter().map(|l| rect4(&l.rect))).collect();
    let mut per_screen = [0usize; 3];
    for r in &rects {
        let homes: Vec<usize> = std::iter::once(wide).chain(screens.iter().copied()).enumerate().filter(|(_, s)| inside(*r, *s)).map(|(i, _)| i).collect();
        assert_eq!(homes.len(), 1, "{r:?} is in {homes:?} of the wide layout and the screens {screens:?}");
        if homes[0] > 0 {
            per_screen[homes[0] - 1] += 1;
        }
    }
    // plan: header, tiles, overlay, layers; takeoff: header, takeoff; report: header, page tiles, info
    assert_eq!(per_screen, [4, 2, 3], "items per narrow screen");
    // every panel of both arrangements draws (a frame, so the UI can be clicked)
    assert_no_error_cards(&mut p);

    // the plan screen's tabs switch to the takeoff screen
    act(&mut p, "layout", Data::map([("layout", Data::str("plan"))]));
    fits(&mut p);
    let tab = (0..110)
        .flat_map(|i| [44.0, 56.0, 68.0].map(|y| (screen[0] + 12.0 + i as f32 * 4.0, y)))
        .find(|&(x, y)| {
            p.pointer_down(x, y);
            p.pointer_up();
            layout(&mut p) == "takeoff"
        });
    assert!(tab.is_some(), "a click on the 數量 tab shows the takeoff screen");
    assert_eq!(fits(&mut p), vec![screens[1]]);
    // "r" fits the screen shown (the engine's own "r" would fit the wide layout)
    assert!(p.key("r"));
    assert_eq!(fits(&mut p), vec![screens[1]]);

    // the narrow plan: a cancelled press does not select, a click does
    act(&mut p, "layout", Data::map([("layout", Data::str("plan"))]));
    assert_eq!(fits(&mut p), vec![screens[0]]);
    let nplan = spec_tiled_in(&mut p, screens[0]);
    let view = [0.0, 0.0, 12000.0, 9000.0];
    assert!(p.act(r#"{"action":"view","params":{"rect":[0,0,12000,9000]}}"#).contains("\"ok\":true"));
    let living = row(&rows(&get(&mut p, "totals")), "客廳 LIVING").get("ids").items()[0].num().expect("an id");
    let (x, y) = plan_point_in(nplan.map(f64::from), view, 1000.0, 1500.0);
    p.pointer_down(x, y);
    p.pointer_cancel();
    assert!(get(&mut p, "selected").items().is_empty(), "a cancelled press on the narrow plan selected");
    p.pointer_down(x, y);
    p.pointer_up();
    assert_eq!(get(&mut p, "selected").items(), &[Data::Num(living)], "a click on the narrow plan selects the room");
    // dragging there pans the drawing, the wheel zooms it
    p.pointer_down(x, y);
    p.pointer_drag(x + 40.0, y);
    p.pointer_up();
    let v = get(&mut p, "view");
    assert!(v.items()[0].num().expect("x") < 0.0, "a drag to the right shows more on the left: {v:?}");
    p.wheel(x, y, 0.0, 2.0);
    let w = get(&mut p, "view").items()[2].num().expect("w");
    assert!(w < 12000.0 * 0.8, "the wheel zoomed in: {w}");

    // back to a desktop window, then upright again
    p.viewport(1600.0, 1000.0, 1.0);
    assert_eq!((layout(&mut p), fits(&mut p)), ("wide".to_string(), vec![wide]));
    p.viewport(480.0, 960.0, 1.0);
    assert_eq!(layout(&mut p), "plan");
    let f = fits(&mut p);
    assert!(f.len() == 1 && f[0][..3] == screens[0][..3] && f[0][3] > 2.0 * f[0][2], "the plan screen again, from the top: {f:?}");
    assert_no_error_cards(&mut p);
    assert_eq!(p.handler_errors, errors, "no handler failed");
}

/// The text commands (x, size, string) in panel `cell`'s display list that
/// contain `what`.
fn texts_with(p: &mut ScriptPage, cell: anypage_engine::core::CellId, what: &str) -> Vec<(f32, f32, String)> {
    let g = p.parts().0;
    g.demand(cell);
    let Some(&script) = g.inputs_of(cell).first() else { return Vec::new() };
    let Some(list) = g.get::<DisplayList>(script) else { return Vec::new() };
    fn walk(d: &Data, what: &str, out: &mut Vec<(f32, f32, String)>) {
        let a = d.items();
        if a.first().and_then(Data::as_str) == Some("text") {
            if let Some(s) = a.get(5).and_then(Data::as_str).filter(|s| s.contains(what)) {
                out.push((a[1].num().unwrap_or(0.0) as f32, a[3].num().unwrap_or(0.0) as f32, s.to_string()));
            }
            return;
        }
        for x in a {
            walk(x, what, out);
        }
    }
    let mut out = Vec::new();
    walk(&list.cmds, what, &mut out);
    out
}

/// Totals of 11 and more digits stay on one line at a smaller size, in the
/// takeoff's stat cards and the report panel, in both arrangements.
#[test]
fn big_totals_fit_their_cards() {
    let mut p = load();
    act(&mut p, "set_price", Data::map([("item", Data::str("wood")), ("price", Data::Num(1e9))]));
    act(&mut p, "make_report", Data::Nil);
    let total = format!("NT$ {}", money(num(&get(&mut p, "totals"), "total")));
    assert!(total.len() >= 18, "{total}");
    assert_no_error_cards(&mut p);
    // what each panel gives the number: a stat card in the takeoff apps
    // (wide, and the narrow takeoff screen), the report panels' inner width
    let panels: Vec<_> = p.spec().panels.iter().map(|x| (x.cell, x.rect)).collect();
    let mut seen = Vec::new();
    for (cell, r) in panels {
        let (what, room) = match (r.w as u32, r.x) {
            (784, _) => ("takeoff", 118.0),
            (172, _) => ("report", 148.0),
            (424, x) if x > 1600.0 => {
                let on_report = p.spec().tiled.iter().any(|t| t.rect.x == r.x && t.rect.y < r.y);
                if on_report { ("report_n", 400.0) } else { ("takeoff_n", 98.0) }
            }
            _ => continue,
        };
        for (_, size, s) in texts_with(&mut p, cell, &total) {
            let tw = text().measure(&s, size);
            assert!(s == total, "{what}: the total is one text: {s:?}");
            assert!(tw <= room + 0.5 && size >= 9.0, "{what}: {s:?} at {size} is {tw:.0} wide, more than {room}");
            println!("{what}: {s} at {size} pt, {tw:.0} of {room} wide (at 15 pt: {:.0})", text().measure(&s, 15.0));
            seen.push(what);
        }
    }
    seen.sort();
    assert_eq!(seen, ["report", "report_n", "takeoff", "takeoff_n"], "where the total is shown");
}

/// The rect of the one tiled layer inside `screen` (on the plan screen: the
/// narrow plan's tiles).
fn spec_tiled_in(p: &mut ScriptPage, screen: [f32; 4]) -> [f32; 4] {
    let found: Vec<[f32; 4]> = p.spec().tiled.iter().map(|t| rect4(&t.rect)).filter(|r| inside(*r, screen)).collect();
    assert_eq!(found.len(), 1, "one tiled layer on the plan screen: {found:?}");
    found[0]
}

/// PNG without a compression crate: stored deflate blocks.
fn write_png(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) {
    fn crc(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend(crc(&body).to_be_bytes());
    }
    let mut raw = Vec::with_capacity((w * 4 + 1) as usize * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * w as usize * 4..(y + 1) * w as usize * 4]);
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push(if i + 1 == blocks.len() { 1 } else { 0 });
        z.extend((b.len() as u16).to_le_bytes());
        z.extend((!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend(((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    std::fs::write(path, out).expect("write png");
}

/// Writes pages/studio/assets/floorplan.dxf and bracket.step.
#[test]
#[ignore = "writes the sample files into pages/studio/assets"]
fn generate_sample_assets() {
    let dir = studio_dir().join("assets");
    std::fs::create_dir_all(&dir).expect("assets dir");
    std::fs::write(dir.join("floorplan.dxf"), plan_dxf()).expect("write floorplan.dxf");
    std::fs::write(dir.join("bracket.step"), bracket_step()).expect("write bracket.step");
}
