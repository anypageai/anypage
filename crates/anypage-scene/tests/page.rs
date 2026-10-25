//! Headless page tests: real pages driven through the Luau API, the demo
//! package loaded from pages/scene-demo, and its GPU layer value rendered
//! with a device created here.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anypage_core::{CellId, Status};
use anypage_engine::{Page, PointerResponse, TextRaster, Wake};
use anypage_runtime::{Data, Natives, Package, ScriptPage};
use anypage_scene::{Frame, SceneRenderer};
use common::*;

fn text() -> Arc<TextRaster> {
    Arc::new(TextRaster::load().expect("a system font"))
}

fn wake() -> Wake {
    Arc::new(|| {})
}

fn natives() -> (Natives, Arc<SceneRenderer>) {
    let r = Arc::new(SceneRenderer::new());
    let mut n = Natives::new();
    anypage_scene::register_with(&mut n, r.clone());
    (n, r)
}

fn load(src: &str) -> Result<ScriptPage, String> {
    let m = r#"{ "format": 1, "id": "scene-test", "bounds": [0, 0, 400, 300], "capabilities": { "modules": ["scene"], "files": true } }"#;
    let pkg = Package::from_files("test", [("page.json", m), ("main.luau", src)])?;
    ScriptPage::load(&pkg, &natives().0, text(), wake())
}

fn wait_for(p: &mut ScriptPage, cell: CellId, mut ok: impl FnMut(&Data) -> bool) -> Data {
    let t0 = Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        let i = g.demand(cell);
        let d = anypage_runtime::data::input_to_data(&i);
        if matches!(i.status, Status::Ready) && ok(&d) {
            return d;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "timed out; last {d:?} {:?}", i.status);
        std::thread::sleep(Duration::from_millis(10));
    }
}

const API: &str = r#"
page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
local m = scene.load("solid t\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\nendsolid t\n", "stl")
local i = m:info()
assert(i.kind == "mesh" and i.triangles == 1 and i.format == "stl", "stl info")
assert(i.bounds.max.x == 1 and i.bounds.min.y == 0, "bounds")
assert(m.kind == "scene.model", "resource kind")
local box = scene.primitive("box", { size = 2 })
assert(box:info().triangles == 12, "box")
local cam = scene.camera()
assert(cam.yaw == 30 and cam.up == "y" and cam.fov == 45, "default camera")
local fitted = scene.fit(cam, box, { aspect = 1 })
assert(fitted.target.x == 0 and fitted.distance > 1.7, "fit")
local o = scene.orbit(fitted, 10, 0)
assert(math.abs(o.yaw - 26) < 1e-9, "orbit " .. o.yaw)
local z = scene.zoom(fitted, 0.5)
assert(math.abs(z.distance - fitted.distance * 0.5) < 1e-9, "zoom")
local t = scene.transform({ translate = { 1, 2, 3 }, scale = 2 })
assert(#t == 16 and t[1] == 2 and t[13] == 1 and t[14] == 2 and t[15] == 3, "transform")
local b = scene.bounds({ items = { { model = box, transform = t } } })
assert(b.min.x == -1 and b.max.x == 3, "bounds with a transform")
local p = scene.project(scene.camera({ yaw = 0, pitch = 0, distance = 5 }), { x = 0, y = 0, z = 0 }, 100, 80)
assert(p.visible and math.abs(p.x - 50) < 1e-9 and math.abs(p.y - 40) < 1e-9, "project")
-- a mesh from a Luau buffer, and back
local pos = buffer.create(36)
for k, v in { 0, 0, 0, 1, 0, 0, 0, 1, 0 } do
	buffer.writef32(pos, (k - 1) * 4, v)
end
local tri = scene.mesh({ positions = pos })
assert(tri:info().vertices == 3, "mesh from bytes")
local back = tri:to_mesh()
assert(buffer.len(back.positions) == 36 and buffer.len(back.indices) == 12, "to_mesh")
local pts = scene.points({ positions = { 0, 0, 0, 1, 1, 1, 2, 2, 2 }, colors = { 0xFF0000FF, 0x00FF00FF, 0x0000FFFF } })
assert(pts:info().points == 3 and pts:info().colors, "points")
local again = scene.load(pts:save("ply"))
assert(again:info().points == 3 and again:info().format == "ply", "save and load")
local cloud = scene.synthetic({ shape = "sphere", count = 1000 })
assert(buffer.len(cloud:to_points({ max = 10 }).positions) == 120, "to_points")
assert(#scene.formats() == 8, "formats")
local function fails(want, f, ...)
	local ok, err = pcall(f, ...)
	err = tostring(err)
	assert(not ok and string.find(err, want, 1, true), err)
end
fails("glb", scene.load, "garbage", "glb")
fails("out of range", scene.mesh, { positions = { 0, 0, 0, 1, 0, 0, 0, 1, 0 }, indices = { 0, 1, 7 } })
fails("camera.yaw", scene.orbit, { yaw = "east" }, 1, 1)
fails("cannot tell the format", scene.load, { name = "x.fbx", data = "????" })
Done = page.input("done", true)
"#;

#[test]
fn luau_api() {
    let mut p = load(API).unwrap();
    assert_eq!(p.cell_value("done"), Some(Data::Bool(true)));
}

#[test]
fn undeclared_module_is_refused() {
    let m = r#"{ "format": 1, "id": "t", "bounds": [0, 0, 10, 10] }"#;
    let src = "page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end }) scene.primitive('box')";
    let pkg = Package::from_files("test", [("page.json", m), ("main.luau", src)]).unwrap();
    let e = ScriptPage::load(&pkg, &natives().0, text(), wake()).err().unwrap();
    assert!(e.contains("not declared"), "{e}");
}

#[test]
fn load_job_from_a_file_cell() {
    let src = r#"
        page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        File = page.input("file", nil)
        Model = page.job("model", "scene.load", { File })
        Info = page.derive("info", { Model }, function(m)
            if type(m) == "userdata" then return m:info() end
            return { kind = "none" }
        end)
        page.on("key", function(k)
            local data = if k == "bad" then "????" else "v 0 0 0\nv 2 0 0\nv 0 2 0\nf 1 2 3\n"
            page.set(File, { name = "tri." .. k, data = data })
            return true
        end)
    "#;
    let mut p = load(src).unwrap();
    let info = p.cell_id("info").unwrap();
    assert_eq!(wait_for(&mut p, info, |_| true).get("kind").as_str(), Some("none"));
    assert!(p.key("obj"));
    let d = wait_for(&mut p, info, |d| d.get("kind").as_str() == Some("mesh"));
    assert_eq!(d.get("triangles").num(), Some(1.0));
    assert_eq!(d.get("format").as_str(), Some("obj"));
    // content that is no known format: the job fails with a message
    assert!(p.key("bad"));
    let model = p.cell_id("model").unwrap();
    let t0 = Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        if let Status::Error(e) = &g.demand(model).status {
            assert!(e.contains("cannot tell the format"), "{e}");
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn load_with(src: &str, n: &Natives) -> Result<ScriptPage, String> {
    let m = r#"{ "format": 1, "id": "scene-test", "bounds": [0, 0, 400, 300], "capabilities": { "modules": ["scene"], "files": true } }"#;
    let pkg = Package::from_files("test", [("page.json", m), ("main.luau", src)])?;
    ScriptPage::load(&pkg, n, text(), wake())
}

fn wait_error(p: &mut ScriptPage, cell: CellId) -> String {
    let t0 = Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        if let Status::Error(e) = &g.demand(cell).status {
            return e.to_string();
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "no error");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn inline_limits_point_to_the_jobs() {
    let src = r#"
        page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        local function fails(want, f, ...)
            local ok, err = pcall(f, ...)
            err = tostring(err)
            assert(not ok and string.find(err, want, 1, true), err)
        end
        fails("use the job", scene.synthetic, { count = 2000000 })
        fails("use the job", scene.load, buffer.create(17 * 1048576), "xyz")
        local big = scene.synthetic({ shape = "cube", count = 1000000 })
        fails("use the job", function() return big:save("xyz") end)
        assert(buffer.len(scene.synthetic({ count = 100000 }):save("ply")) > 0, "small saves stay inline")
        Params = page.input("params", { shape = "helix", count = 1500000, seed = 5 })
        Cloud = page.job("cloud", "scene.synthetic", { Params })
        Fixed = page.job("fixed", "scene.synthetic", {}, { shape = "sphere", count = 1200 })
        Saved = page.job("saved", "scene.save", { Fixed }, { format = "xyz" })
        local many = buffer.create(12 * 4000001)
        fails("use the job", scene.points, { positions = many })
        Form = page.input("form", { positions = many })
        Pts = page.job("pts", "scene.points", { Form }, { max_points = 10 })
        PtsInfo = page.derive("pts.info", { Pts }, function(m) return m:info() end)
        Info = page.derive("info", { Cloud }, function(m) return m:info() end)
        Stat = page.derive("stat", {}, function() return scene.stats() end)
    "#;
    // room for the 48 MB position buffer built at load
    let m = r#"{ "format": 1, "id": "scene-test", "bounds": [0, 0, 400, 300], "capabilities": { "modules": ["scene"] }, "limits": { "memory_mb": 200, "load_ms": 20000 } }"#;
    let pkg = Package::from_files("test", [("page.json", m), ("main.luau", src)]).unwrap();
    let mut p = ScriptPage::load(&pkg, &natives().0, text(), wake()).unwrap();
    let info = p.cell_id("info").unwrap();
    let d = wait_for(&mut p, info, |_| true);
    assert_eq!(d.get("points").num(), Some(1_500_000.0));
    let pts = p.cell_id("pts.info").unwrap();
    let d = wait_for(&mut p, pts, |_| true);
    assert_eq!((d.get("points").num(), d.get("source_points").num()), (Some(10.0), Some(4_000_001.0)));
    let saved = p.cell_id("saved").unwrap();
    let b = wait_for(&mut p, saved, |d| d.as_bytes().is_some());
    let text = std::str::from_utf8(b.as_bytes().unwrap()).unwrap();
    assert_eq!(text.lines().count(), 1200);
    // scene.stats reads state outside the graph: refused inside a cell
    let stat = p.cell_id("stat").unwrap();
    let e = wait_error(&mut p, stat);
    assert!(e.contains("scene.stats changes state"), "{e}");
}

#[test]
fn stats_stream_follows_renders_of_its_scene() {
    let (n, r) = natives();
    let src = r#"
        page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        local box = scene.primitive("box")
        Main = page.input("main", { id = "main", items = { { model = box } } })
        Other = page.input("other", { id = "other", items = { { model = box }, { model = box } } })
        Cam = page.input("cam", { yaw = 30, pitch = 20, distance = 4 })
        page.layer({ rect = { 0, 0, 64, 48 }, kind = "scene.view", inputs = { Main, Cam } })
        page.layer({ rect = { 0, 0, 32, 24 }, kind = "scene.view", inputs = { Other, Cam } })
        Stats = page.stream("stats", "scene.stats", { id = "main", latest = true })
        Last = page.derive("last", { Stats }, function(s)
            if type(s) == "table" and s.last then return s.last end
            return { renders = 0 }
        end)
        Snap = page.input("snap", 0)
        page.on("key", function(k)
            page.set(Snap, scene.stats(k).items)
            return true
        end)
    "#;
    let mut p = load_with(src, &n).unwrap();
    let last = p.cell_id("last").unwrap();
    assert_eq!(wait_for(&mut p, last, |_| true).get("renders").num(), Some(0.0));
    let Some(g) = gpu() else { return };
    let frames: Vec<_> = (0..2).map(|k| (p.spec().layers[k].renderer.clone(), {
        let cell = p.spec().layers[k].cell;
        let gr = p.parts().0;
        gr.commit();
        gr.demand(cell).value.clone().expect("layer value")
    }))
    .collect();
    render(&g, &*frames[0].0, &frames[0].1, 64, 48).unwrap();
    let d = wait_for(&mut p, last, |d| d.get("renders").num().unwrap_or(0.0) >= 1.0);
    assert_eq!((d.get("items").num(), d.get("width").num()), (Some(1.0), Some(64.0)));
    // the other scene's render does not reach this stream
    render(&g, &*frames[1].0, &frames[1].1, 32, 24).unwrap();
    assert_eq!(r.stats_for("other").unwrap().items, 2);
    std::thread::sleep(Duration::from_millis(50));
    let d = wait_for(&mut p, last, |_| true);
    assert_eq!(d.get("width").num(), Some(64.0));
    // a snapshot by id in a handler
    assert!(p.key("other"));
    assert_eq!(p.cell_value("snap"), Some(Data::Num(2.0)));
    assert_eq!(p.handler_errors, 0);
}

fn demo() -> (ScriptPage, Arc<SceneRenderer>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pages/scene-demo");
    let pkg = Package::from_dir(&dir).unwrap();
    let (n, r) = natives();
    let p = ScriptPage::load(&pkg, &n, text(), wake()).unwrap_or_else(|e| panic!("demo: {e}"));
    (p, r)
}

fn layer_frame(p: &mut ScriptPage) -> Arc<dyn std::any::Any + Send + Sync> {
    let cell = p.spec().layers[0].cell;
    let g = p.parts().0;
    g.commit();
    let i = g.demand(cell);
    assert!(matches!(i.status, Status::Ready), "{:?}", i.status);
    i.value.clone().expect("layer value")
}

/// Every panel's draw function ran without a script error.
fn panels_ok(p: &mut ScriptPage) {
    let cells: Vec<CellId> = p.spec().panels.iter().map(|x| x.cell).collect();
    let g = p.parts().0;
    for c in cells {
        g.demand(c);
        let draw = g.inputs_of(c)[0];
        if let Status::Error(e) = g.status(draw) {
            panic!("{}: {e}", g.label(draw));
        }
    }
}

#[test]
fn demo_page_loads_renders_and_reacts() {
    let (mut p, r) = demo();
    assert_eq!(p.spec().layers.len(), 1);
    assert!(p.spec().panels.len() >= 3);
    let v = layer_frame(&mut p);
    let f = v.downcast_ref::<Frame>().expect("a scene frame");
    assert_eq!(f.items.len(), 6);
    assert!(!f.auto_fit, "the page computes its camera");
    assert!(f.grid.is_some());
    panels_ok(&mut p);

    // render the layer value as the engine would
    if let Some(g) = gpu() {
        let rect = p.spec().layers[0].rect;
        let (w, h) = ((rect.w / 2.0) as u32, (rect.h / 2.0) as u32);
        let renderer = p.spec().layers[0].renderer.clone();
        let img = render(&g, &*renderer, &v, w, h).unwrap();
        save_png("demo_page.png", &img, w, h);
        // the background colour in a corner, content (not background) over a good part of the view
        assert!(close(px(&img, w, 2, 2), [16, 19, 26, 255], 2), "{:?}", px(&img, w, 2, 2));
        let content = coverage(&img, |c| !close(c, [16, 19, 26, 255], 6));
        assert!(content > 0.15, "content covers {content}");
        let st = r.stats();
        assert_eq!(st.items, 6);
        assert_eq!(st.points_drawn, 460_000);
        assert!(st.triangles > 1000);
        let first = (st.ms, st.upload_ms);
        render(&g, &*renderer, &v, w, h).unwrap();
        let again = r.stats();
        assert_eq!(again.uploaded_bytes, 0, "geometry stays on the GPU");
        println!("demo render on {}: first {:.1} ms ({:.1} ms upload, pipelines compiled), again {:.2} ms, at {w}x{h}", g.info, first.0, first.1, again.ms);
    }

    // drag to orbit
    let rect = p.spec().layers[0].rect;
    let (cx, cy) = (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
    let yaw0 = p.cell_value("camera").unwrap().get("yaw").num().unwrap();
    assert_eq!(p.pointer_down(cx, cy), PointerResponse::Captured);
    p.pointer_drag(cx + 50.0, cy + 25.0);
    p.pointer_up();
    let cam = p.cell_value("camera").unwrap();
    assert!((cam.get("yaw").num().unwrap() - (yaw0 - 20.0)).abs() < 1e-9, "{cam:?}");
    assert!((cam.get("pitch").num().unwrap() - 34.0).abs() < 1e-9, "{cam:?}");
    // the derived camera follows
    let view = p.cell_value("camera.view").unwrap();
    assert!((view.get("yaw").num().unwrap() - (yaw0 - 20.0)).abs() < 1e-9);
    // wheel zooms only over the view
    let d0 = view.get("distance").num().unwrap();
    assert!(p.wheel(cx, cy, 0.0, 2.0));
    assert!(!p.wheel(1300.0, 500.0, 0.0, 2.0));
    let d1 = p.cell_value("camera.view").unwrap().get("distance").num().unwrap();
    assert!((d1 / d0 - 0.81).abs() < 1e-6, "{d0} -> {d1}");
    // keys and buttons change what is shown
    assert!(p.key("2"));
    let v = layer_frame(&mut p);
    assert_eq!(v.downcast_ref::<Frame>().unwrap().items.len(), 4);
    assert!(p.key("g"));
    let v = layer_frame(&mut p);
    assert!(v.downcast_ref::<Frame>().unwrap().grid.is_none());
    // the "cloud" button in the side panel (local 240, 420)
    assert_eq!(p.pointer_down(1120.0 + 250.0, 70.0 + 430.0), PointerResponse::Handled);
    let v = layer_frame(&mut p);
    assert_eq!(v.downcast_ref::<Frame>().unwrap().total_points(), 460_000);
    panels_ok(&mut p);
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn demo_agent_actions_and_file_drop() {
    let (mut p, _) = demo();
    let d: serde_json::Value = serde_json::from_str(&p.describe()).unwrap();
    let names: Vec<&str> = d["actions"].as_array().unwrap().iter().filter_map(|a| a["name"].as_str()).collect();
    assert!(names.contains(&"set_view") && names.contains(&"show") && names.contains(&"describe_models"), "{names:?}");
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "set_view", "params": {"yaw": 90, "pitch": 10, "zoom": 0.5}}"#)).unwrap();
    assert_eq!(r["result"]["yaw"], 90);
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "describe_models"}"#)).unwrap();
    assert_eq!(r["result"][4]["points"], 400000);
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"set": "camera", "value": {"yaw": 0, "pitch": 0, "zoom": 1, "pan_x": 0, "pan_y": 0}}"#)).unwrap();
    assert_eq!(r["ok"], true);
    assert!(p.control("point_size", 4.0));

    // drop a binary STL: loaded by the job, then shown alone
    let mut stl = vec![0u8; 80];
    stl.extend(1u32.to_le_bytes());
    stl.extend([0u8; 12]);
    for v in [0f32, 0.0, 0.0, 3.0, 0.0, 0.0, 0.0, 3.0, 0.0] {
        stl.extend(v.to_le_bytes());
    }
    stl.extend([0, 0]);
    p.file_drop("part.stl", Arc::new(stl));
    let loaded = p.cell_id("loaded").unwrap();
    let m = wait_for(&mut p, loaded, |d| d.as_res().is_some());
    assert_eq!(m.as_res().unwrap().kind(), "scene.model");
    let v = layer_frame(&mut p);
    let f = v.downcast_ref::<Frame>().unwrap();
    assert_eq!(f.items.len(), 1);
    assert_eq!(f.items[0].model.triangle_count(), 1);
    panels_ok(&mut p);
    // a broken file: the side panel shows the error, nothing panics
    p.file_drop("broken.glb", Arc::new(b"glTF\x02\0\0\0garbage".to_vec()));
    let t0 = Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        if let Status::Error(e) = &g.demand(loaded).status {
            assert!(e.contains("glb"), "{e}");
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    panels_ok(&mut p);
    assert_eq!(p.handler_errors, 0);
}
