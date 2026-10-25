//! The demo package (pages/cad-demo) loaded and driven headlessly.

mod common;

use std::path::Path;

use anypage_cad::internals::{fit, BBox};
use anypage_cad::CadDoc;
use anypage_core::{Status, TileKey, TILE_SIZE};
use anypage_engine::{Image, Page, PageRequest, PointerResponse};
use anypage_runtime::{Data, Package, ScriptPage};
use common::*;
use serde_json::Value as Json;

fn load_demo() -> ScriptPage {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pages/cad-demo");
    let pkg = Package::from_dir(&dir).expect("package");
    let mut n = anypage_runtime::Natives::new();
    anypage_cad::register(&mut n);
    ScriptPage::load(&pkg, &n, text(), wake()).expect("demo loads")
}

fn act(p: &mut ScriptPage, req: &str) -> Json {
    let r: Json = serde_json::from_str(&p.act(req)).expect("json");
    assert!(r.get("error").is_none(), "{req}: {r}");
    r
}

/// Length of a list the semantic port returned (long lists come summarised).
fn len_of(j: &Json) -> usize {
    match j.as_array() {
        Some(a) => a.len(),
        None => j["$len"].as_u64().expect("list or summary") as usize,
    }
}

/// Page point of a drawing point, the way the tiled layer maps it.
fn page_of(p: &mut ScriptPage, x: f64, y: f64) -> (f32, f32) {
    let view = p.cell_value("view").expect("view");
    let v: Vec<f64> = view.items().iter().filter_map(Data::num).collect();
    let src = BBox::new(v[0], v[1], v[0] + v[2], v[1] + v[3]);
    let m = fit(&src, (20.0, 80.0, 1100.0, 900.0), true);
    let q = m.apply(anypage_cad::internals::P { x, y });
    (q.x as f32, q.y as f32)
}

#[test]
fn demo_loads_draws_and_edits() {
    let mut p = load_demo();
    eprintln!("{} ", p.summary());
    assert_eq!(p.spec().panels.len(), 2);
    assert_eq!(p.spec().tiled.len(), 1);
    // every panel draws without a script error
    let cells: Vec<_> = p.spec().panels.iter().map(|x| x.cell).collect();
    {
        let g = p.parts().0;
        for c in cells {
            let i = g.demand(c);
            assert!(matches!(i.status, Status::Ready), "{:?}", i.status);
            let script = g.inputs_of(c)[0];
            assert!(matches!(g.status(script), Status::Ready), "{}: {:?}", g.label(script), g.status(script));
            assert!(i.get::<Image>().is_some());
        }
    }
    // tiles of the whole drawing at level 0, assembled into one picture
    let (params, src) = {
        let t = &p.spec().tiled[0];
        (t.params, t.source.clone())
    };
    let value = {
        let g = p.parts().0;
        let i = g.demand(params);
        assert!(matches!(i.status, Status::Ready), "{:?}", i.status);
        i.value.clone().expect("params value")
    };
    let (cols, rows) = (5u32, 4u32);
    let n = TILE_SIZE;
    let mut mosaic = vec![0u8; (cols * n * rows * n * 4) as usize];
    let bg = anypage_cad::default_background();
    let mut drawn = 0usize;
    for ty in 0..rows {
        for tx in 0..cols {
            let tile = src.render(TileKey { page: 0, level: 0, tx: tx as i32, ty: ty as i32 }, &value, &|| false).expect("tile");
            for y in 0..n {
                let dst = (((ty * n + y) * cols * n + tx * n) * 4) as usize;
                let s = (y * n * 4) as usize;
                mosaic[dst..dst + (n * 4) as usize].copy_from_slice(&tile[s..s + (n * 4) as usize]);
            }
            drawn += tile.as_chunks::<4>().0.iter().filter(|px| px[..3] != bg[..3]).count();
        }
    }
    save_png("cad_demo_level0.png", cols * n, rows * n, &mosaic);
    assert!(drawn > 20_000, "drawn pixels {drawn}");

    // the entities action (the agent's read path)
    let r = act(&mut p, r#"{"action": "entities", "params": {"filter": {"type": "CIRCLE", "layer": "GARDEN"}}}"#);
    let circles = r["result"].as_array().expect("list").clone();
    assert_eq!(circles.len(), 2, "{r}");
    let c = &circles[0];
    let (id, cx, cy, radius) = (c["id"].as_f64().unwrap(), c["cx"].as_f64().unwrap(), c["cy"].as_f64().unwrap(), c["r"].as_f64().unwrap());

    // click on the circle's edge selects it
    let (px_, py_) = page_of(&mut p, cx + radius, cy);
    assert_eq!(p.pointer_down(px_, py_), PointerResponse::Handled);
    p.pointer_up();
    assert_eq!(p.cell_value("selected").unwrap(), Data::nums([id]));
    let m = act(&mut p, r#"{"action": "measure"}"#);
    let len = m["result"]["length"].as_f64().unwrap();
    assert!((len - 2.0 * std::f64::consts::PI * radius).abs() < 1e-6, "{m}");
    // empty space: deselect, and the engine pans
    let (ex, ey) = page_of(&mut p, cx, cy - radius * 3.0);
    assert_eq!(p.pointer_down(ex, ey), PointerResponse::Ignored);
    assert_eq!(p.cell_value("selected").unwrap(), Data::list(vec![]));

    // select by action, move with an arrow key, then delete with a key
    act(&mut p, &format!(r#"{{"action": "select", "params": {{"ids": [{id}]}}}}"#));
    assert!(p.key("right"));
    let r = act(&mut p, &format!(r#"{{"action": "entities", "params": {{"filter": {{"ids": [{id}]}}}}}}"#));
    assert!(r["result"][0]["cx"].as_f64().unwrap() > cx, "{r}");
    let before = len_of(&act(&mut p, r#"{"action": "entities", "params": {"filter": {}}}"#)["result"]);
    assert!(p.key("delete"));
    let after = len_of(&act(&mut p, r#"{"action": "entities", "params": {"filter": {}}}"#)["result"]);
    assert_eq!(after, before - 1);

    // add through the semantic port
    let r = act(&mut p, r#"{"action": "add", "params": {"entity": {"type": "LINE", "x1": 0, "y1": 0, "x2": 1000, "y2": 1000, "layer": "DIM"}}}"#);
    assert_eq!(r["result"]["ids"].as_array().unwrap().len(), 1);

    // hide a layer through the agent port, then toggle it back from the side panel
    act(&mut p, r#"{"action": "layer", "params": {"name": "HATCH", "on": false}}"#);
    assert_eq!(p.cell_value("layers").unwrap().get("HATCH"), &Data::Bool(false));
    // layer rows start at y 64 (local), 30 high, in the panel at (1140, 16)
    let mut toggled = false;
    for row in 0..12 {
        let y = 16.0 + 64.0 + row as f32 * 30.0 + 12.0;
        if p.pointer_down(1140.0 + 60.0, y) == PointerResponse::Handled && p.cell_value("layers").unwrap().get("HATCH") == &Data::Bool(true) {
            toggled = true;
            break;
        }
    }
    assert!(toggled, "a layer row toggled HATCH back on");

    // wheel over the drawing zooms the view
    let v0 = p.cell_value("view").unwrap();
    assert!(p.wheel(500.0, 500.0, 0.0, 2.0));
    let v1 = p.cell_value("view").unwrap();
    assert!(v1.at(3).num().unwrap() < v0.at(3).num().unwrap());
    assert!(p.control("fit", 1.0));

    // save through the page: the host gets the bytes, which reopen
    act(&mut p, r#"{"action": "save"}"#);
    let bytes = match p.take_request() {
        Some(PageRequest::SaveFile { bytes, name, .. }) => {
            assert_eq!(name, "drawing.dxf");
            bytes
        }
        other => panic!("{other:?}"),
    };
    let doc = CadDoc::open(&bytes).expect("saved demo reopens");
    assert_eq!(doc.scene().groups.len(), after + 1);
    std::fs::write(out_dir().join("cad_demo_saved.dxf"), &*bytes).unwrap();

    // dropping a DXF replaces the drawing: the handler only starts the job
    // cad.load, which fills the new drawing off the page's thread
    let t = std::time::Instant::now();
    p.file_drop("sample.dxf", std::sync::Arc::new(sample_dxf()));
    let drop_ms = t.elapsed().as_secs_f64() * 1000.0;
    assert!(p.cell_value("status").unwrap().as_str().unwrap().contains("sample.dxf"));
    assert_eq!(p.cell_value("view"), Some(Data::Nil), "the view fits the drawing once it has loaded");
    let loaded = p.cell_id("loaded").unwrap();
    let t = std::time::Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        let i = g.demand(loaded);
        if matches!(i.status, Status::Ready) && i.value.as_deref().map(anypage_runtime::data::value_to_data).is_some_and(|d| d.as_res().is_some()) {
            break;
        }
        assert!(!matches!(i.status, Status::Error(_)), "{:?}", i.status);
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "load timed out: {:?}", i.status);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    eprintln!("file drop handler {drop_ms:.1} ms, load in the job {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    let r = act(&mut p, r#"{"action": "entities", "params": {"filter": {}}}"#);
    assert_eq!(len_of(&r["result"]), 12);
    // the tiles fit the loaded drawing (sample bounds start at x 10)
    let params = {
        let t = &p.spec().tiled[0];
        t.params
    };
    let value = {
        let g = p.parts().0;
        g.commit();
        let i = g.demand(params);
        anypage_runtime::data::input_to_data(&i)
    };
    assert_eq!(value.get("loaded").as_str(), Some("ready"));
    let rect: Vec<f64> = value.get("rect").items().iter().filter_map(Data::num).collect();
    assert!(rect.len() == 4 && rect[0] < 10.0 && rect[0] > 0.0, "{rect:?}");
    // the panels draw the loaded drawing without script errors
    let cells: Vec<_> = p.spec().panels.iter().map(|x| x.cell).collect();
    {
        let g = p.parts().0;
        for c in cells {
            let i = g.demand(c);
            assert!(matches!(i.status, Status::Ready), "{:?}", i.status);
            let script = g.inputs_of(c)[0];
            assert!(matches!(g.status(script), Status::Ready), "{}: {:?}", g.label(script), g.status(script));
        }
    }
    // and the page keeps editing the drawing the job filled
    act(&mut p, r#"{"action": "add", "params": {"entity": {"type": "POINT", "x": 1, "y": 1}}}"#);
    let r = act(&mut p, r#"{"action": "entities", "params": {"filter": {}}}"#);
    assert_eq!(len_of(&r["result"]), 13);

    // a large file: the drop returns at once, the panels draw the progress
    let t = std::time::Instant::now();
    p.file_drop("big.dxf", std::sync::Arc::new(lines_dxf(100_000)));
    let drop_ms = t.elapsed().as_secs_f64() * 1000.0;
    let mut saw_pending = false;
    loop {
        let g = p.parts().0;
        g.commit();
        let title = p.spec().panels[0].cell;
        let g = p.parts().0;
        let i = g.demand(title);
        assert!(matches!(i.status, Status::Ready), "{:?}", i.status);
        let script = g.inputs_of(title)[0];
        assert!(matches!(g.status(script), Status::Ready), "{}: {:?}", g.label(script), g.status(script));
        let l = g.demand(loaded);
        saw_pending |= matches!(l.status, Status::Ready) && anypage_runtime::data::input_to_data(&l).get("state").as_str() == Some("pending")
            || matches!(l.status, Status::Pending { .. });
        if matches!(l.status, Status::Ready) && l.value.as_deref().map(anypage_runtime::data::value_to_data).is_some_and(|d| d.as_res().is_some()) {
            break;
        }
        assert!(!matches!(l.status, Status::Error(_)), "{:?}", l.status);
        assert!(t.elapsed() < std::time::Duration::from_secs(30), "big load timed out");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    eprintln!("big file: drop handler {drop_ms:.1} ms, loaded after {:.0} ms (pending seen: {saw_pending})", t.elapsed().as_secs_f64() * 1000.0);
    assert!(drop_ms < 100.0, "the drop handler does not parse");
    let dwg = p.cell_value("drawing").unwrap();
    let (doc, _) = anypage_cad::doc_of(&dwg).unwrap();
    assert_eq!(doc.scene().groups.len(), 100_000);
    assert_eq!(p.handler_errors, 0);
    p.shutdown();
}
