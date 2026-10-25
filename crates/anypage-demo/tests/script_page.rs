//! The Luau demo page, loaded and driven without a window or GPU.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anypage_core::{CellId, Status};
use anypage_demo::script;
use anypage_engine::{Image, Page, PageRequest, PointerResponse, TextRaster, Wake};
use anypage_runtime::{Data, Natives, Package, ScriptPage};

fn text() -> Arc<TextRaster> {
    Arc::new(TextRaster::load().expect("a system font"))
}

fn wake() -> Wake {
    Arc::new(|| {})
}

/// The script cell behind a panel's raster cell.
fn script_of(p: &ScriptPage, panel: u32) -> CellId {
    let raster = p.spec().panels.iter().find(|x| x.id == panel).expect("panel").cell;
    p.graph().inputs_of(raster)[0]
}

#[test]
fn builtin_demo_loads_and_draws_every_panel() {
    let mut p = script::load(text(), wake()).expect("load");
    assert_eq!(p.spec().panels.len(), 9);
    assert_eq!(p.spec().tiled.len(), 1);
    assert_eq!(p.spec().effects.len(), 1);
    assert!(p.load_ms < 1000.0, "load took {} ms", p.load_ms);
    let cells: Vec<CellId> = p.spec().panels.iter().map(|x| x.cell).collect();
    let g = p.parts().0;
    g.tick(1.25);
    for c in cells {
        let i = g.demand(c);
        assert!(matches!(i.status, Status::Ready), "{}: {:?}", g.label(c), i.status);
        let img = i.get::<Image>().expect("image");
        assert!(img.w > 0 && img.h > 0);
        let script = g.inputs_of(c)[0];
        assert!(matches!(g.status(script), Status::Ready), "{}: {:?}", g.label(script), g.status(script));
    }
    p.shutdown();
}

#[test]
fn declared_time_reaches_the_graph() {
    let mut p = script::load(text(), wake()).unwrap();
    let clock = script_of(&p, 8);
    let cursor = script_of(&p, 4);
    let orbit = script_of(&p, 5);
    let title = script_of(&p, 0);
    let g = p.parts().0;
    g.tick(1.25);
    for c in [clock, cursor, orbit, title] {
        g.demand(c);
    }
    // the clock holds until the next whole second, the cursor until its next blink
    assert_eq!(g.expires(clock), 2.0);
    assert!((g.expires(cursor) - 3.0 * 0.53).abs() < 1e-9, "{}", g.expires(cursor));
    // the orbit reads the clock and declares nothing: due every tick
    assert!(g.expires(orbit) <= 1.25);
    // pausing the animation makes it a constant
    assert!(p.control("anim", 0.0));
    let g = p.parts().0;
    g.tick(1.5);
    g.demand(orbit);
    assert_eq!(g.expires(orbit), f64::INFINITY);
    // a later tick before the declared time does not run the clock panel again
    let runs = g.stats.inline_evals;
    g.tick(1.9);
    g.demand(clock);
    assert_eq!(g.stats.inline_evals, runs);
    p.shutdown();
}

#[test]
fn keys_taps_and_controls_change_inputs() {
    let mut p = script::load(text(), wake()).unwrap();
    let params = p.spec().tiled[0].params;
    let iterations = |p: &mut ScriptPage| p.parts().0.demand(params).get::<Data>().unwrap().get("iterations").num_or(0.0);
    assert_eq!(iterations(&mut p), 256.0);
    assert!(p.key("2"));
    assert_eq!(iterations(&mut p), 512.0);
    assert!(p.control("iterations", 100_000.0));
    assert_eq!(iterations(&mut p), 4096.0);
    assert!(!p.key("x"));
    // an unchanged value is not a change: the params cell keeps its version
    let v = p.graph().version(params);
    p.control("iterations", 4096.0);
    p.parts().0.demand(params);
    assert_eq!(p.graph().version(params), v);

    // the fit button, a slider, and empty page space
    assert_eq!(p.pointer_down(1440.0 + 700.0, 100.0 + 230.0), PointerResponse::Handled);
    assert_eq!(p.take_request(), Some(PageRequest::FitPage));
    assert_eq!(p.pointer_down(1440.0 + 24.0 + 280.0, 100.0 + 180.0), PointerResponse::Captured);
    p.pointer_drag(1440.0 + 24.0 + 560.0, 100.0 + 180.0);
    p.pointer_up();
    let hue = p.parts().0.demand(params).get::<Data>().unwrap().get("hue").num_or(0.0);
    assert_eq!(hue, 360.0);
    assert_eq!(p.pointer_down(10.0, 10.0), PointerResponse::Ignored);
    assert_eq!(p.handler_errors, 0);
    p.shutdown();
}

#[test]
fn jobs_run_on_the_pool_and_report_back() {
    let mut p = script::load(text(), wake()).unwrap();
    let jobs = p.spec().panels.iter().find(|x| x.id == 7).unwrap().cell;
    let job1 = p.graph().inputs_of(script_of(&p, 7))[0];
    assert!(p.control("jobs", 1.0));
    let g = p.parts().0;
    g.demand(jobs);
    assert!(matches!(g.status(job1), Status::Pending { .. }));
    let t0 = Instant::now();
    loop {
        g.commit();
        g.demand(jobs);
        if matches!(g.status(job1), Status::Ready) {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "job did not finish");
        std::thread::sleep(Duration::from_millis(20));
    }
    let d = g.get::<Data>(job1).unwrap();
    assert_eq!(d.get("state").as_str(), Some("done"));
    assert!(d.get("ms").num_or(0.0) > 500.0);
    p.shutdown();
}

const MANIFEST: &str = r#"{ "format": 1, "id": "test", "bounds": [0, 0, 200, 200] }"#;

#[test]
fn a_failing_script_shows_an_error_panel_and_the_page_survives() {
    let src = r#"
        page.panel({ rect = { 0, 0, 100, 50 }, draw = function(d) error("boom") end })
        page.panel({ rect = { 0, 60, 100, 50 }, draw = function(d) while true do end end })
        page.panel({ rect = { 0, 120, 100, 50 }, draw = function(d) d:rect(0, 0, 10, 10, color.rgba(255, 0, 0)) end })
    "#;
    let pkg = Package::from_files("test", [("page.json", MANIFEST), ("main.luau", src)]).unwrap();
    let mut p = ScriptPage::load(&pkg, &Natives::new(), text(), wake()).unwrap();
    let panels: Vec<CellId> = p.spec().panels.iter().map(|x| x.cell).collect();
    let g = p.parts().0;
    let t0 = Instant::now();
    for (k, c) in panels.iter().enumerate() {
        let i = g.demand(*c);
        assert!(i.get::<Image>().is_some(), "panel {k} has an image");
        let script = g.inputs_of(*c)[0];
        match (k, g.status(script)) {
            (0, Status::Error(e)) => assert!(e.contains("boom"), "{e}"),
            (1, Status::Error(e)) => assert!(e.contains("time budget"), "{e}"),
            (2, Status::Ready) => {}
            (k, s) => panic!("panel {k}: {s:?}"),
        }
    }
    assert!(t0.elapsed() < Duration::from_secs(2));
}

#[test]
fn undeclared_capabilities_and_late_graph_changes_are_refused() {
    let src = r#"page.stream("x", "demo.ticks")"#;
    let pkg = Package::from_files("test", [("page.json", MANIFEST), ("main.luau", src)]).unwrap();
    let e = ScriptPage::load(&pkg, &script::natives(), text(), wake()).err().expect("refused");
    assert!(e.contains("not declared"), "{e}");

    let src = r#"
        page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        page.on("key", function(k) page.input("late", 1) return true end)
    "#;
    let pkg = Package::from_files("test", [("page.json", MANIFEST), ("main.luau", src)]).unwrap();
    let mut p = ScriptPage::load(&pkg, &Natives::new(), text(), wake()).unwrap();
    assert!(!p.key("a"));
    assert_eq!(p.handler_errors, 1);
}

#[test]
fn shaders_are_checked_before_the_gpu_sees_them() {
    use anypage_runtime::check_wgsl;
    check_wgsl("plasma.wgsl", script::PLASMA_WGSL).expect("the demo shader is valid");
    assert!(check_wgsl("x.wgsl", "this is not wgsl").is_err());
    let no_entry = "@fragment fn fs_main() -> @location(0) vec4<f32> { return vec4<f32>(1.0); }";
    assert!(check_wgsl("x.wgsl", no_entry).unwrap_err().contains("vs_main"));

    let manifest = r#"{ "format": 1, "id": "t", "bounds": [0, 0, 100, 100], "capabilities": { "gpu": true } }"#;
    let src = r#"page.effect({ rect = { 0, 0, 10, 10 }, shader = "bad.wgsl", params = page.input("p", { 0 }) })"#;
    let pkg = Package::from_files("t", [("page.json", manifest), ("main.luau", src), ("bad.wgsl", "fn")]).unwrap();
    let e = ScriptPage::load(&pkg, &Natives::new(), text(), wake()).err().expect("refused");
    assert!(e.starts_with("main.luau:1: bad.wgsl"), "{e}");
}
