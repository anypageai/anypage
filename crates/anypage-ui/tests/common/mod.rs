//! Shared helpers: load pages that use the ui library, drive them, and read
//! back what their panels drew.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anypage_core::{CellId, Status};
use anypage_engine::{Image, Page, TextRaster, Wake};
use anypage_runtime::{Data, DisplayList, Natives, Package, ScriptPage};

/// A text system of its own for each page (about 8 ms to load), as each
/// engine has one. Shared by tests running at the same time, its glyph cache
/// lock (taken once per character measured) makes layout 3-4x slower, which
/// would push pages over the default 50 ms budget for reasons a real page
/// does not have (CORE_REQUESTS #13).
pub fn text() -> Arc<TextRaster> {
    Arc::new(TextRaster::load().expect("a system font"))
}

/// The shared text system, for measuring expected widths in assertions.
pub fn metrics() -> Arc<TextRaster> {
    static T: OnceLock<Arc<TextRaster>> = OnceLock::new();
    T.get_or_init(|| Arc::new(TextRaster::load().expect("a system font"))).clone()
}

pub fn wake() -> Wake {
    Arc::new(|| {})
}

pub fn natives() -> Natives {
    let mut n = Natives::new();
    anypage_ui::register(&mut n);
    n
}

pub fn manifest(w: f64, h: f64, extra: &str) -> String {
    format!(
        r#"{{ "format": 1, "id": "ui.test", "bounds": [0, 0, {w}, {h}], "capabilities": {{ "clipboard": true }},
            "limits": {{ "call_ms": 1000, "handler_ms": 1000, "load_ms": 5000, "memory_mb": 128 }} {extra} }}"#
    )
}

/// A manifest with the runtime's default limits (50 ms per call and per
/// handler, 64 MB): what a real page gets unless it asks for more.
pub fn default_manifest(w: f64, h: f64) -> String {
    format!(r#"{{ "format": 1, "id": "ui.test", "bounds": [0, 0, {w}, {h}], "capabilities": {{ "clipboard": true }} }}"#)
}

pub fn load_pkg(manifest: &str, main: &str) -> Result<ScriptPage, String> {
    let pkg = Package::from_files("test", [("page.json", manifest), ("main.luau", main)])?;
    ScriptPage::load(&pkg, &natives(), text(), wake())
}

/// A page whose main.luau is `main`, bounds 1000 x 800, clipboard allowed.
pub fn load(main: &str) -> ScriptPage {
    load_pkg(&manifest(1000.0, 800.0, ""), main).unwrap_or_else(|e| panic!("load failed: {e}"))
}

/// The script cell (display list) behind a panel.
pub fn script_cell(p: &ScriptPage, panel: u32) -> CellId {
    let raster = p.spec().panels.iter().find(|x| x.id == panel).expect("panel").cell;
    p.graph().inputs_of(raster)[0]
}

pub fn raster_cell(p: &ScriptPage, panel: u32) -> CellId {
    p.spec().panels.iter().find(|x| x.id == panel).expect("panel").cell
}

/// The display list a panel draws now (evaluating it if needed).
pub fn display(p: &mut ScriptPage, panel: u32) -> Data {
    let c = script_cell(p, panel);
    let g = p.parts().0;
    let i = g.demand(c);
    match &i.status {
        Status::Ready => {}
        other => panic!("panel {panel} is not ready: {other:?}"),
    }
    i.get::<DisplayList>().expect("a display list").cmds.clone()
}

/// Every string drawn with "text" commands.
pub fn texts(cmds: &Data) -> Vec<String> {
    cmds.items()
        .iter()
        .filter(|c| c.at(1).as_str() == Some("text"))
        .filter_map(|c| c.at(6).as_str().map(str::to_string))
        .collect()
}

pub fn has_text(cmds: &Data, s: &str) -> bool {
    texts(cmds).iter().any(|t| t == s)
}

pub fn has_text_containing(cmds: &Data, s: &str) -> bool {
    texts(cmds).iter().any(|t| t.contains(s))
}

/// Commands of one kind ("rrect", "text", ...).
pub fn commands<'a>(cmds: &'a Data, op: &str) -> Vec<&'a Data> {
    cmds.items().iter().filter(|c| c.at(1).as_str() == Some(op)).collect()
}

/// A packed 0xRRGGBBAA colour as the display list carries it.
pub fn rgba(r: u8, g: u8, b: u8, a: u8) -> f64 {
    u32::from_be_bytes([r, g, b, a]) as f64
}

/// Rectangles (x, y, w, h) filled with colour `c`.
pub fn rects_of_color(cmds: &Data, c: f64) -> Vec<(f64, f64, f64, f64)> {
    commands(cmds, "rrect")
        .into_iter()
        .filter(|r| r.at(7).num() == Some(c))
        .map(|r| (r.at(2).num_or(0.0), r.at(3).num_or(0.0), r.at(4).num_or(0.0), r.at(5).num_or(0.0)))
        .collect()
}

/// The rasterised panel (premultiplied RGBA) at 1 px per world unit.
pub fn raster(p: &mut ScriptPage, panel: u32) -> Arc<Image> {
    let c = raster_cell(p, panel);
    let g = p.parts().0;
    let i = g.demand(c);
    assert!(matches!(i.status, Status::Ready), "raster: {:?}", i.status);
    let v = i.value.clone().expect("raster value");
    v.downcast::<Image>().unwrap_or_else(|_| panic!("panel value is not an image"))
}

pub fn pixel(img: &Image, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * img.w + x) * 4) as usize;
    [img.rgba[i], img.rgba[i + 1], img.rgba[i + 2], img.rgba[i + 3]]
}

pub fn out_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-output");
    std::fs::create_dir_all(&d).expect("test-output dir");
    d
}

pub fn save_png(name: &str, img: &Image) -> PathBuf {
    let path = out_dir().join(name);
    let file = std::fs::File::create(&path).expect("png file");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.w, img.h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    // un-premultiply for a normal PNG
    let mut px = img.rgba.clone();
    for p in px.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let mut w = enc.write_header().expect("png header");
    w.write_image_data(&px).expect("png data");
    path
}

/// Run a page action and return its result.
pub fn act(p: &mut ScriptPage, name: &str, params: Data) -> Data {
    p.run_action(name, &params).unwrap_or_else(|e| panic!("action {name}: {e}"))
}

/// `app:describe()` through the app's agent action, every page of it (the
/// action returns at most 40 nodes and a `next` cursor).
pub fn describe(p: &mut ScriptPage, app: &str) -> Data {
    let name = format!("{app}.describe");
    let first = act(p, &name, Data::Nil);
    let mut nodes: Vec<Data> = first.get("nodes").items().to_vec();
    let mut next = first.get("next").num();
    let mut pages = 1;
    while let Some(from) = next {
        let page = act(p, &name, Data::map([("from", Data::Num(from))]));
        nodes.extend(page.get("nodes").items().iter().cloned());
        next = page.get("next").num();
        pages += 1;
        assert!(pages < 1000, "describe never ends");
    }
    let mut out: Vec<(String, Data)> = Vec::new();
    for key in ["app", "rect", "focus", "count", "total"] {
        out.push((key.to_string(), first.get(key).clone()));
    }
    out.push(("nodes".to_string(), Data::list(nodes)));
    Data::map(out.iter().map(|(k, v)| (k.as_str(), v.clone())))
}

/// The described node with this id.
pub fn node(desc: &Data, id: &str) -> Data {
    desc.get("nodes")
        .items()
        .iter()
        .find(|n| n.get("id").as_str() == Some(id))
        .cloned()
        .unwrap_or_else(|| panic!("no node {id}"))
}

pub fn try_node(desc: &Data, id: &str) -> Option<Data> {
    desc.get("nodes").items().iter().find(|n| n.get("id").as_str() == Some(id)).cloned()
}

/// Centre of a described node, in world coordinates.
pub fn center(n: &Data) -> (f32, f32) {
    let x = n.get("x").num_or(0.0) + n.get("w").num_or(0.0) / 2.0;
    let y = n.get("y").num_or(0.0) + n.get("h").num_or(0.0) / 2.0;
    (x as f32, y as f32)
}

/// Press and release at a world point.
pub fn click(p: &mut ScriptPage, x: f32, y: f32) -> anypage_engine::PointerResponse {
    let r = p.pointer_down(x, y);
    p.pointer_up();
    r
}

/// Click the centre of element `id` of app `app`.
pub fn click_id(p: &mut ScriptPage, app: &str, id: &str) {
    let d = describe(p, app);
    let (x, y) = center(&node(&d, id));
    click(p, x, y);
}

pub fn state(p: &mut ScriptPage, app: &str) -> Data {
    p.cell_value(&format!("{app}.state")).expect("state cell")
}

pub fn ui_state(p: &mut ScriptPage, app: &str) -> Data {
    p.cell_value(&format!("{app}.ui")).expect("ui cell")
}

/// Advance the page clock (what the engine does every frame).
pub fn tick(p: &mut ScriptPage, t: f64) {
    p.parts().0.tick(t);
}

/// Frames made the way `Engine::frame` makes them: the page clock is real
/// time since `new()`, and it advances only when a visible panel is due
/// (`next_expiry <= now`), so after a quiet spell `page.now()` is old.
pub struct Frames {
    start: std::time::Instant,
}

impl Default for Frames {
    fn default() -> Self {
        Self::new()
    }
}

impl Frames {
    pub fn new() -> Self {
        Frames { start: std::time::Instant::now() }
    }

    pub fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// One frame: tick only if a panel is due, commit, evaluate every panel's
    /// display list. Returns whether the clock advanced.
    pub fn frame(&self, p: &mut ScriptPage) -> bool {
        let now = self.now();
        let sinks: Vec<CellId> = p.spec().panels.iter().map(|x| x.cell).collect();
        let scripts: Vec<CellId> = sinks.iter().map(|&c| p.graph().inputs_of(c)[0]).collect();
        let g = p.parts().0;
        let due = g.next_expiry(&sinks) <= now + 0.0005;
        if due {
            g.tick(now);
        }
        g.commit();
        for c in scripts {
            g.demand(c);
        }
        due
    }

    /// Wall-clock wait (both the page clock and os.clock move on).
    pub fn wait(&self, secs: f64) {
        std::thread::sleep(std::time::Duration::from_secs_f64(secs));
    }
}
