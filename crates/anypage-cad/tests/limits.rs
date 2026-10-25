//! Work and memory stay bounded with hostile or very large input: nested
//! blocks, huge curves under the pointer, absurd angles, bulk reads and
//! edits, stale handles, bad numbers. Timings are for the debug test build
//! (crate at opt-level 1) with generous margins.

mod common;

use std::time::{Duration, Instant};

use anypage_cad::internals::P;
use anypage_cad::{pick, render_image, CadDoc};
use anypage_core::Status;
use anypage_engine::Page;
use anypage_runtime::{Data, Res};
use common::*;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn res(doc: CadDoc) -> Data {
    Data::Res(Res::new(anypage_cad::KIND, doc))
}

fn run(src: &str, files: Vec<(&'static str, Vec<u8>)>) {
    run_page(src, files);
}

fn run_page(src: &str, files: Vec<(&'static str, Vec<u8>)>) -> anypage_runtime::ScriptPage {
    let n = natives(files);
    let full = format!("page.panel({{ rect = {{ 0, 0, 10, 10 }}, draw = function(d) end }})\n{src}");
    load_page(&full, &n).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn nested_blocks_stay_small_and_fast() {
    // 20 KB of DXF that draws 100 000 000 lines (the reviewer's block bomb)
    let bytes = nested_dxf(3);
    assert!(bytes.len() < 30_000);
    let t = Instant::now();
    let doc = CadDoc::open(&bytes).expect("open");
    let open_ms = ms(t);
    let scene = doc.scene();
    assert_eq!(scene.groups.len(), 100);
    // stored once: 300 block items + 100 instances
    let stored: usize = scene.blocks.values().map(|b| b.items.len()).sum::<usize>() + scene.groups.iter().map(|g| g.items.len()).sum::<usize>();
    assert_eq!(stored, 400);
    assert!((scene.expanded_points() - 2.0e8).abs() < 1.0);
    assert!(doc.lock().warnings.is_empty(), "{:?}", doc.lock().warnings);
    // bounds through the instances: x to 99·200² + 99·200 + 99, y to 991
    let b = scene.bounds;
    assert!((b.x1 - 3_979_899.0).abs() < 1e-6 && (b.y1 - 991.0).abs() < 1e-9, "{b:?}");
    let first = scene.groups[0].id;

    // pixels: one B0 (lines x = 0..99, y 0..1) of the lowest row, close up
    let dwg = res(doc);
    let (doc, _) = anypage_cad::doc_of(&dwg).unwrap();
    let t = Instant::now();
    let close = render_image(&doc, &Data::map([("rect", Data::nums([-10.0, -1.0, 120.0, 3.0])), ("width_px", Data::Num(1200.0)), ("background", Data::Num(255.0))]), &|| false).unwrap();
    let close_ms = ms(t);
    save_png("cad_nested_close.png", close.w, close.h, &close.rgba);
    // line x = 50 is at pixel x 600; between lines (x = 50.5) nothing
    let row = close.h / 2;
    assert!(like_any(&close.rgba, close.w, 600, row, 1, [255, 255, 255]), "line at x 50: {:?}", px(&close.rgba, close.w, 600, row));
    assert!(near(px(&close.rgba, close.w, 605, row), [0, 0, 0], 8), "gap at x 50.5");
    // the whole drawing: 10^8 lines, mostly sub-pixel instances
    let t = Instant::now();
    let whole = render_image(&doc, &Data::map([("width_px", Data::Num(1024.0)), ("height_px", Data::Num(256.0)), ("background", Data::Num(255.0))]), &|| false).unwrap();
    let whole_ms = ms(t);
    save_png("cad_nested_whole.png", whole.w, whole.h, &whole.rgba);
    let drawn = whole.rgba.chunks(4).filter(|p| p[0] > 20).count();
    assert!(drawn > 500, "drawn {drawn}");

    // pick reaches the line inside three levels of blocks
    let t = Instant::now();
    assert_eq!(pick(&doc.scene(), P { x: 50.0, y: 0.5 }, 0.1, &[]), Some(first));
    assert_eq!(pick(&doc.scene(), P { x: 50.5, y: 0.5 }, 0.1, &[]), None);
    let pick_ms = ms(t);
    // measures: a million unit lines in one insert, from the cached block measure
    let t = Instant::now();
    let m = anypage_cad::measure(&doc.scene(), &[first].into_iter().collect()).unwrap();
    let measure_ms = ms(t);
    assert!((m.get("length").num().unwrap() - 1.0e6).abs() < 1e-3, "{m:?}");
    eprintln!("nested blocks: open {open_ms:.1} ms, close-up {close_ms:.1} ms, whole {whole_ms:.0} ms, 2 picks {pick_ms:.1} ms, measure {measure_ms:.1} ms");
    assert!(open_ms < 2000.0 && close_ms < 2000.0 && whole_ms < 10_000.0 && pick_ms < 1000.0 && measure_ms < 1000.0);

    // to_paths refuses at once instead of building 600 million values
    run(
        r##"
        local dwg = cad.open(test.bytes("nested"))
        local t = os.clock()
        local ok, err = pcall(cad.to_paths, dwg, {})
        assert(not ok and string.find(tostring(err), "path values"), tostring(err))
        assert(os.clock() - t < 1, "refused late")
        -- one block of 100 lines is fine
        local small = cad.open(test.bytes("one"))
        assert(#small:to_paths({}) == 10000)
        "##,
        vec![("nested", bytes), ("one", nested_dxf(1))],
    );
}

#[test]
fn uneven_scales_measure_through_geometry_within_a_bound() {
    // top inserts scaled 2 x 1: lengths of vertical unit lines stay 1, areas double
    let two = String::from_utf8(nested_dxf(2)).unwrap().replace("  2\nB1\n 10\n0\n", "  2\nB1\n 41\n2\n 42\n1\n 10\n0\n");
    let doc = CadDoc::open(two.as_bytes()).unwrap();
    let scene = doc.scene();
    let t = Instant::now();
    let m = anypage_cad::measure(&scene, &[scene.groups[0].id].into_iter().collect()).unwrap();
    assert!((m.get("length").num().unwrap() - 1.0e4).abs() < 1e-6, "{m:?}");
    // four levels unevenly scaled: 10^8 lines would have to be walked
    let four = String::from_utf8(nested_dxf(4)).unwrap().replace("  2\nB3\n 10\n0\n", "  2\nB3\n 41\n2\n 42\n1\n 10\n0\n");
    let doc = CadDoc::open(four.as_bytes()).unwrap();
    let scene = doc.scene();
    let e = anypage_cad::measure(&scene, &[scene.groups[0].id].into_iter().collect()).unwrap_err();
    assert!(e.contains("measuring needs more than"), "{e}");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
}

/// The reviewer's probe: picks on and inside circles of radius up to 1e300.
#[test]
fn picking_near_huge_curves_is_fast() {
    // circles of radius r and arcs (10°..350°) of radius r/2 around the origin
    let mut s = String::from("  0\nSECTION\n  2\nENTITIES\n");
    let radii = [1e3, 1e6, 1e7, 1e12, 1e300];
    for (i, r) in radii.iter().enumerate() {
        s.push_str(&format!("  0\nCIRCLE\n  5\n{:X}\n  8\n0\n 10\n0\n 20\n0\n 40\n{r:e}\n", 0x100 + i));
        s.push_str(&format!("  0\nARC\n  5\n{:X}\n  8\n0\n 10\n0\n 20\n0\n 40\n{:e}\n 50\n10\n 51\n350\n", 0x200 + i, r * 0.5));
    }
    s.push_str("  0\nENDSEC\n  0\nEOF\n");
    let doc = CadDoc::open(s.as_bytes()).unwrap();
    let scene = doc.scene();
    assert_eq!(scene.groups.len(), 10);
    let t = Instant::now();
    // inside everything, zero and demo-like tolerances: nothing is near
    assert_eq!(pick(&scene, P { x: 0.0, y: 0.0 }, 0.0, &[]), None);
    assert_eq!(pick(&scene, P { x: 0.0, y: 0.0 }, 5.0, &[]), None);
    assert_eq!(pick(&scene, P { x: 3.0, y: -7.0 }, 0.0, &[]), None);
    // on the curves
    assert_eq!(pick(&scene, P { x: 1.0e6, y: 0.0 }, 1e-3, &[]), Some(0x101), "circle 1e6 at 0°");
    assert_eq!(pick(&scene, P { x: 0.0, y: 5.0e6 }, 1.0, &[]), Some(0x202), "arc 1e7/2 at 90°");
    assert_eq!(pick(&scene, P { x: -5.0e11, y: 0.0 }, 5.0, &[]), Some(0x203), "arc 1e12/2 at 180°");
    assert_eq!(pick(&scene, P { x: 5.0e11, y: 0.0 }, 5.0, &[]), None, "the arc's gap at 0°");
    // near but off: 1e7 circle, 2 units outside with tol 1
    assert_eq!(pick(&scene, P { x: 1.0e7 + 2.0, y: 0.0 }, 1.0, &[]), None);
    let all_ms = ms(t);
    eprintln!("9 picks on huge curves: {all_ms:.1} ms");
    assert!(all_ms < 2000.0, "{all_ms} ms");
}

#[test]
fn absurd_angles_do_not_hang() {
    let s = "  0\nSECTION\n  2\nENTITIES\n\
        0\nARC\n  5\n10\n 10\n0\n 20\n0\n 40\n1\n 50\n1e300\n 51\n-1e300\n\
        0\nARC\n  5\n11\n 10\n0\n 20\n0\n 40\n1\n 50\ninf\n 51\n0\n\
        0\nELLIPSE\n  5\n12\n 10\n0\n 20\n0\n 11\n2\n 21\n0\n 40\n0.5\n 41\n1e300\n 42\n-1e300\n\
        0\nELLIPSE\n  5\n13\n 10\n0\n 20\n0\n 11\n2\n 21\n0\n 40\n0.5\n 41\n-inf\n 42\n5\n\
        0\nENDSEC\n  0\nEOF\n";
    let t = Instant::now();
    let doc = CadDoc::open(s.as_bytes()).expect("opens");
    assert_eq!(doc.scene().groups.len(), 4);
    let dwg = res(doc);
    let (doc, _) = anypage_cad::doc_of(&dwg).unwrap();
    render_image(&doc, &Data::Nil, &|| false).unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}

#[test]
fn reading_100k_entities_is_linear() {
    let n = natives(vec![("big", lines_dxf(100_000))]);
    let src = r#"page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        D = page.input("d", cad.open(test.bytes("big")))
        Msg = page.input("msg", "")
        page.on("key", function(k)
            local ok, err = pcall(function()
                local dwg = page.get(D)
                local t = os.clock()
                local all = dwg:entities({})
                local t_all = os.clock() - t
                t = os.clock()
                local some = dwg:entities({ rect = { 0, 0, 500, 500 } })
                local t_rect = os.clock() - t
                t = os.clock()
                local e = dwg:entity(all[#all].id)
                local e2 = dwg:entity(all[1].id)
                local t_one = os.clock() - t
                t = os.clock()
                local none = dwg:entities({ type = "CIRCLE" })
                local t_scan = os.clock() - t
                assert(#all == 100000 and #some > 20000 and e.id == all[#all].id and e2.id == all[1].id and #none == 0, #all .. " " .. #some)
                page.set(Msg, string.format("entities({}) %.0f ms, with rect %.0f ms (%d), scan only %.0f ms, two entity() %.2f ms", t_all * 1000, t_rect * 1000, #some, t_scan * 1000, t_one * 1000))
                assert(t_all < 6 and t_rect < 6, "too slow")
            end)
            if not ok then
                page.set(Msg, "ERROR " .. tostring(err))
            end
            return true
        end)
    "#;
    // reading 100 000 entities into Luau tables takes longer than the usual handler budget
    let manifest = r#"{ "format": 1, "id": "cad-big", "bounds": [0, 0, 800, 600],
        "capabilities": { "modules": ["cad", "test"] },
        "limits": { "handler_ms": 20000, "load_ms": 20000, "call_ms": 20000, "memory_mb": 512 } }"#;
    let pkg = anypage_runtime::Package::from_files("test", [("page.json", manifest), ("main.luau", src)]).unwrap();
    let mut p = anypage_runtime::ScriptPage::load(&pkg, &n, text(), wake()).unwrap_or_else(|e| panic!("{e}"));
    assert!(p.key("x"));
    let msg = p.cell_value("msg").unwrap();
    let msg = msg.as_str().unwrap();
    eprintln!("100k lines: {msg}");
    assert!(!msg.is_empty() && !msg.starts_with("ERROR"), "{msg}");
}

#[test]
fn bulk_edits_are_linear() {
    let p = run_page(
        r##"
        local dwg = cad.new()
        local lines = {}
        for i = 1, 10000 do
            lines[i] = { type = "LINE", x1 = i, y1 = 0, x2 = i, y2 = 1, layer = if i % 2 == 0 then "EVEN" else "ODD" }
        end
        local t = os.clock()
        dwg = cad.add(dwg, lines)
        local t_add = os.clock() - t
        local ids = dwg:last_ids()
        assert(#ids == 10000 and dwg:info().entities == 10000)
        -- a hatch after the 5000th line keeps its place when lines around it go
        dwg = cad.add(dwg, { type = "HATCH", loops = { { 0, 0, 1, 0, 1, 1 } } })
        local hatch = dwg:last_ids()[1]
        t = os.clock()
        dwg = cad.copy(dwg, ids, 0, 10)
        local t_copy = os.clock() - t
        assert(#dwg:last_ids() == 10000 and dwg:info().entities == 20001)
        -- delete every other one of the 20 000
        local odd = {}
        for _, e in dwg:entities({ layer = "ODD" }) do table.insert(odd, e.id) end
        assert(#odd == 10000)
        t = os.clock()
        dwg = cad.delete(dwg, odd)
        local t_del = os.clock() - t
        local left = dwg:entities({})
        assert(#left == 10001, #left)
        assert(dwg:entity(odd[1]) == nil and dwg:entity(ids[2]) ~= nil)
        -- order kept: even lines, the hatch after line 10000 (the last even original), then copies
        assert(left[5000].id == ids[10000] and left[5001].id == hatch and left[5001].type == "HATCH", left[5001].type)
        assert(left[5002].type == "LINE" and left[5002].y1 == 10)
        Msg = page.input("msg", string.format("add 10k %.0f ms, copy 10k %.0f ms, delete 10k of 20k %.0f ms", t_add * 1000, t_copy * 1000, t_del * 1000))
        assert(t_add < 2 and t_copy < 2 and t_del < 2, "bulk edits too slow")
        -- the scene matches after a save and reopen
        local again = cad.open(dwg:save())
        assert(#again:entities({}) == 10001)
        "##,
        vec![],
    );
    let mut p = p;
    eprintln!("bulk edits: {}", p.cell_value("msg").unwrap().as_str().unwrap());
}

#[test]
fn edits_through_an_old_handle_still_update_the_view() {
    let n = natives(vec![]);
    let src = r#"page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        local first = cad.new()
        local a = cad.add(first, { type = "POINT", x = 1, y = 1 })
        local b = cad.add(first, { type = "POINT", x = 2, y = 2 })
        V = page.input("v", a)
        page.on("key", function(k)
            -- a handler that kept the load-time handle in a local
            page.set(V, cad.add(first, { type = "POINT", x = 3, y = 3 }))
            return true
        end)
        page.on("text", function(t) page.set(V, b) return true end)
    "#;
    let mut p = load_page(src, &n).unwrap();
    let version = |p: &mut anypage_runtime::ScriptPage| p.cell_value("v").and_then(|d| d.as_res().map(|r| r.version())).unwrap();
    assert_eq!(version(&mut p), 1);
    p.text_input("b");
    assert_eq!(version(&mut p), 2, "the second edit through the same handle is a new value");
    assert!(p.key("x"));
    assert_eq!(version(&mut p), 3, "and the third");
    let v = p.cell_value("v").unwrap();
    let (doc, _) = anypage_cad::doc_of(&v).unwrap();
    assert_eq!(doc.scene().groups.len(), 3);
}

#[test]
fn saving_leaves_the_drawing_unchanged() {
    // an R12 drawing with a hatch: written as R2000, still R12 in memory
    let mut s = String::from("  0\nSECTION\n  2\nHEADER\n  9\n$ACADVER\n  1\nAC1009\n  0\nENDSEC\n  0\nSECTION\n  2\nENTITIES\n");
    s.push_str("  0\nLINE\n  5\n20\n  8\n0\n 10\n0\n 20\n0\n 11\n5\n 21\n5\n");
    s.push_str(&hatch("21", "0", 1, true, &[(0.0, 0.0), (4.0, 0.0), (4.0, 2.0)]).replace("\r\n", "\n"));
    s.push_str("  0\nENDSEC\n  0\nEOF\n");
    run(
        r##"
        local dwg = cad.open(test.bytes("r12"))
        local before = dwg:info()
        assert(before.version == "R12", before.version)
        assert(#before.warnings == 1 and string.find(before.warnings[1], "AutoCAD 2000"), before.warnings[1])
        local bytes = dwg:save()
        local after = dwg:info()
        assert(after.version == "R12" and #after.warnings == 1, after.version .. " " .. #after.warnings)
        -- saving twice gives the same bytes: nothing was assigned on the way
        assert(buffer.tostring(dwg:save()) == buffer.tostring(bytes))
        local again = cad.open(bytes)
        assert(again:info().version == "R2000" and #again:entities({ type = "HATCH" }) == 1)
        "##,
        vec![("r12", s.into_bytes())],
    );
}

#[test]
fn bad_numbers_are_refused() {
    let n = natives(vec![]);
    let err = |src: &str| -> String {
        let full = format!(
            "page.panel({{ rect = {{ 0, 0, 10, 10 }}, draw = function(d) end }})\nlocal dwg = cad.add(cad.new(), {{ type = 'POINT', x = 0, y = 0 }})\nlocal id = dwg:last_ids()[1]\n{src}"
        );
        load_page(&full, &n).err().unwrap_or_else(|| panic!("accepted: {src}"))
    };
    assert!(err("cad.set_color(dwg, id, -5)").contains("not an ACI colour"));
    assert!(err("cad.set_color(dwg, id, 3.5)").contains("not an ACI colour"));
    assert!(err("cad.set_color(dwg, id, 0xFF0000FF)").contains("not an ACI colour"));
    assert!(err("cad.set_color(dwg, id, { color = -1 })").contains("not a packed"));
    assert!(err("cad.add(dwg, { type = 'LINE', x1 = 0, y1 = 0, x2 = 1, y2 = 1, color = 2^40 })").contains("not a packed"));
    assert!(err("cad.zoom({ 0, 0, 100, 100 }, 50, 50, 1e300)").contains("unusable view"));
    assert!(err("cad.zoom({ 0, 0, 1e10, 1e10 }, 0, 0, 1e-300)").contains("unusable view"));
    assert!(err("cad.zoom({ 1e6, 0, 100, 100 }, 1e6, 50, 1e12)").contains("unusable view"));
    assert!(err("cad.zoom({ 1e308, 0, 1e308, 100 }, 0, 0, 1)").contains("rect must be"));
    // the good ones still work
    run(
        r##"
        local dwg = cad.add(cad.new(), { type = "POINT", x = 0, y = 0 })
        local id = dwg:last_ids()[1]
        dwg = cad.set_color(dwg, id, 256)
        dwg = cad.set_color(dwg, id, { color = 0xFF0000FF })
        assert(dwg:entity(id).true_color == 0xFF0000)
        local z = cad.zoom({ 0, 0, 100, 100 }, 50, 50, 2)
        assert(z[1] == 25 and z[3] == 50)
        "##,
        vec![],
    );
}

#[test]
fn a_job_fills_a_drawing_the_handler_already_holds() {
    let n = natives(vec![("sample", sample_dxf())]);
    let src = r#"page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        Target = cad.new({ pending = true })
        assert(Target:info().loading == true)
        local ok, err = pcall(cad.add, Target, { type = "POINT", x = 0, y = 0 })
        assert(not ok and string.find(tostring(err), "still loading"), tostring(err))
        D = page.input("d", Target)
        Src = page.input("src", { bytes = test.bytes("sample"), into = Target })
        Loaded = page.job("loaded", "cad.load", { Src })
        Empty = page.job("empty", "cad.load", { page.input("none", nil) })
        page.on("key", function(k)
            page.set(D, cad.add(page.get(D), { type = "POINT", x = 5, y = 5 }))
            return true
        end)
    "#;
    let mut p = load_page(src, &n).unwrap_or_else(|e| panic!("{e}"));
    let cell = p.cell_id("loaded").unwrap();
    let t = Instant::now();
    let loaded = loop {
        let g = p.parts().0;
        g.commit();
        let i = g.demand(cell);
        if matches!(i.status, Status::Ready) {
            if let Some(d) = i.value.as_deref().map(anypage_runtime::data::value_to_data).filter(|d| !d.is_nil()) {
                break d;
            }
        }
        assert!(!matches!(i.status, Status::Error(_)), "{:?}", i.status);
        assert!(t.elapsed() < Duration::from_secs(10), "load timed out");
        std::thread::sleep(Duration::from_millis(10));
    };
    let r = loaded.as_res().unwrap();
    assert_eq!(r.version(), 1, "the filled drawing is a new version of the same resource");
    let (doc, _) = anypage_cad::doc_of(&loaded).unwrap();
    assert_eq!(doc.lock().dwg.entities().count(), 10);
    assert!(!doc.lock().pending);
    // the handler's handle (version 0) edits the loaded drawing; the result is newer than the job's
    assert!(p.key("x"));
    let d = p.cell_value("d").unwrap();
    assert_eq!(d.as_res().unwrap().version(), 2);
    assert_eq!(d.as_res().unwrap().id(), r.id());
    assert_eq!(doc.lock().dwg.entities().count(), 11);
    let empty = p.cell_id("empty").unwrap();
    let t = Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        let i = g.demand(empty);
        if matches!(i.status, Status::Ready) {
            assert!(i.value.as_deref().map(anypage_runtime::data::value_to_data).unwrap_or_default().is_nil());
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", i.status);
        std::thread::sleep(Duration::from_millis(10));
    }
    p.shutdown();
}

#[test]
fn entities_native_part_timing() {
    let doc = CadDoc::open(&lines_dxf(100_000)).unwrap();
    let dwg = res(doc);
    let t = Instant::now();
    let all = anypage_cad::internals::entities(&dwg, &Data::Nil).unwrap();
    let took = ms(t);
    eprintln!("native entities(100k): {took:.0} ms, {} items", all.items().len());
    assert_eq!(all.items().len(), 100_000);
    assert!(took < 5000.0, "{took} ms");
}
