//! Drawing output checked by pixels: whole-view images, tiles at several
//! zoom levels, deep zoom on survey coordinates, layers and selection.

mod common;

use std::time::Instant;

use anypage_cad::internals::P;
use anypage_cad::{render_image, tile_source, CadDoc};
use anypage_core::{val, Rect, TileKey, TILE_SIZE};
use anypage_runtime::{Data, Res};
use common::*;

const BLACK: u32 = 0x000000FF;

fn drawing(bytes: &[u8]) -> Data {
    Data::Res(Res::new(anypage_cad::KIND, CadDoc::open(bytes).expect("open")))
}

fn opts(pairs: Vec<(&str, Data)>) -> Data {
    Data::map(pairs)
}

fn image(dwg: &Data, w: f64, extra: Vec<(&str, Data)>) -> anypage_engine::Image {
    let mut o = vec![("rect", Data::nums([0.0, 0.0, 100.0, 100.0])), ("width_px", Data::Num(w)), ("background", Data::Num(BLACK as f64))];
    o.extend(extra);
    let (doc, _) = anypage_cad::doc_of(dwg).expect("doc");
    render_image(&doc, &opts(o), &|| false).expect("render")
}

/// Drawing (x, y) to the pixel of a 400 x 400 view of 0..100.
fn at(x: f64, y: f64) -> (u32, u32) {
    ((x * 4.0).round() as u32, ((100.0 - y) * 4.0).round() as u32)
}

#[test]
fn whole_view_pixels() {
    let dwg = drawing(&sample_dxf());
    let t = Instant::now();
    let img = image(&dwg, 400.0, vec![]);
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    eprintln!("render 400x400: {ms:.2} ms");
    assert_eq!((img.w, img.h), (400, 400));
    save_png("cad_sample_400.png", img.w, img.h, &img.rgba);
    let (w, rgba) = (img.w, &img.rgba);
    let red = [255, 0, 0];
    let blue = [0, 0, 255];
    let green = [0, 255, 0];
    let yellow = [255, 255, 0];
    let cyan = [0, 255, 255];
    let magenta = [255, 0, 255];
    let white = [255, 255, 255];
    let check = |x: f64, y: f64, c: [u8; 3], what: &str| {
        let (px_, py_) = at(x, y);
        assert!(like_any(rgba, w, px_, py_, 1, c), "{what} at ({x}, {y}) -> {:?}", px(rgba, w, px_, py_));
    };
    let empty = |x: f64, y: f64, what: &str| {
        let (px_, py_) = at(x, y);
        assert!(count(rgba, w, px_ - 2, py_ - 2, px_ + 3, py_ + 3, [0, 0, 0], 8) == 25, "{what} at ({x}, {y}) should be background");
    };
    check(50.0, 10.0, red, "BYLAYER line on RED");
    check(70.0, 50.0, blue, "circle right");
    check(50.0, 30.0, blue, "circle bottom");
    empty(50.0, 50.0, "circle centre");
    let a = 45f64.to_radians();
    check(50.0 + 30.0 * a.cos(), 50.0 + 30.0 * a.sin(), green, "arc at 45°");
    empty(20.0, 50.0, "arc at 180° (not part of the arc)");
    check(20.0, 70.0, yellow, "bulge arc bottom");
    empty(20.0, 90.0, "no bulge above the chord");
    check(30.0, 85.0, yellow, "straight segment");
    // text: magenta coverage in its box
    let (tx0, ty1) = at(60.0, 84.0);
    let (tx1, ty0) = at(80.0, 94.0);
    let text_px = count(rgba, w, tx0, ty0, tx1, ty1, magenta, 90);
    assert!(text_px > 60, "text pixels {text_px}");
    // inserts: BYBLOCK cross takes the INSERT's cyan; scale 2 → arms to ±4
    check(23.0, 40.0, cyan, "scaled insert arm");
    check(20.0, 43.0, cyan, "scaled insert arm (vertical)");
    let r = 1.5 / 2f64.sqrt();
    check(80.0 + r, 40.0 + r, cyan, "rotated insert arm");
    empty(81.8, 40.0, "rotated insert: no horizontal arm");
    // layer off
    empty(50.0, 95.0, "HIDDEN layer");
    // OCS -Z circle: OCS x -88 is drawing x +88 (the unmirrored spot is off the view)
    check(91.0, 70.0, white, "mirrored OCS circle");
    empty(88.0, 70.0, "mirrored circle centre");
    // solid hatch
    let (hx, hy) = at(80.0, 22.0);
    assert!(near(px(rgba, w, hx, hy), red, 2), "solid hatch {:?}", px(rgba, w, hx, hy));
    let (x0, y1) = at(71.0, 16.0);
    let (x1, y0) = at(89.0, 29.0);
    let filled = count(rgba, w, x0, y0, x1, y1, red, 2);
    assert_eq!(filled, ((x1 - x0) * (y1 - y0)) as usize, "the hatch is filled edge to edge");
    // pattern hatch: some yellow lines inside, none outside its square
    let (x0, y1) = at(11.0, 16.0);
    let (x1, y0) = at(29.0, 29.0);
    let lines = count(rgba, w, x0, y0, x1, y1, yellow, 90);
    let area = ((x1 - x0) * (y1 - y0)) as usize;
    assert!(lines > area / 20 && lines < area / 2, "pattern coverage {lines} of {area}");
    empty(9.0, 22.0, "pattern stays inside");
    // draw order: the green line is drawn over nothing in particular, the
    // solid hatch (inserted before it in the file) must not hide it
    check(50.0, 60.0, green, "green line");
}

#[test]
fn tiles_match_the_image_and_stay_crisp() {
    let dwg = drawing(&sample_dxf());
    let img = image(&dwg, 400.0, vec![]);
    let src = tile_source(Rect::new(0.0, 0.0, 400.0, 400.0));
    let params = val(Data::map([("drawing", dwg.clone()), ("rect", Data::nums([0.0, 0.0, 100.0, 100.0])), ("background", Data::Num(BLACK as f64))]));
    let n = TILE_SIZE;
    // level 0: tile (0,0) is page pixels 0..256, the same mapping as the image
    let tile = src.render(TileKey { page: 0, level: 0, tx: 0, ty: 0 }, &params, &|| false).expect("tile");
    assert_eq!(tile.len(), (n * n * 4) as usize);
    save_png("cad_tile_l0_0_0.png", n, n, &tile);
    let mut diff = 0u64;
    for y in 0..n {
        for x in 0..n {
            let a = px(&tile, n, x, y);
            let b = px(&img.rgba, img.w, x, y);
            diff += (0..3).map(|k| (a[k] as i32 - b[k] as i32).unsigned_abs() as u64).sum::<u64>();
        }
    }
    let mean = diff as f64 / (n * n * 3) as f64;
    assert!(mean < 1.0, "tile and image differ: mean {mean}");
    // level 4 (16 px per page unit): the red line at drawing y = 10 (page y 360)
    // is about one pixel thick, at row 128 of tile (12, 22)
    let t = Instant::now();
    let tile = src.render(TileKey { page: 0, level: 4, tx: 12, ty: 22 }, &params, &|| false).expect("tile");
    eprintln!("deep tile: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
    save_png("cad_tile_l4_line.png", n, n, &tile);
    for x in [10u32, 128, 250] {
        let column: Vec<u32> = (0..n).filter(|&y| near(px(&tile, n, x, y), [255, 0, 0], 120) || px(&tile, n, x, y)[0] > 60).collect();
        assert!(!column.is_empty() && column.len() <= 3, "column {x}: rows {column:?}");
        assert!(column.iter().all(|&y| (126..=130).contains(&y)), "column {x}: rows {column:?}");
    }
    // cancelled work returns nothing
    assert!(src.render(TileKey { page: 0, level: 2, tx: 1, ty: 1 }, &params, &|| true).is_none());
}

#[test]
fn layers_and_selection_params() {
    let dwg = drawing(&sample_dxf());
    // only RED visible
    let img = image(&dwg, 400.0, vec![("layers", Data::list(vec![Data::str("RED")]))]);
    let (x, y) = at(50.0, 10.0);
    assert!(like_any(&img.rgba, img.w, x, y, 1, [255, 0, 0]));
    let (x, y) = at(70.0, 50.0);
    assert!(!like_any(&img.rgba, img.w, x, y, 2, [0, 0, 255]), "layer 0 hidden");
    // overrides: turn HIDDEN on
    let img = image(&dwg, 400.0, vec![("layers", Data::map([("HIDDEN", Data::Bool(true))]))]);
    let (x, y) = at(50.0, 95.0);
    assert!(like_any(&img.rgba, img.w, x, y, 1, [0, 255, 0]), "HIDDEN shown by override");
    // selection drawn in the highlight colour
    let line_id = {
        let (doc, _) = anypage_cad::doc_of(&dwg).unwrap();
        anypage_cad::pick(&doc.scene(), P { x: 50.0, y: 10.0 }, 1.0, &[]).expect("the red line is picked")
    };
    let img = image(&dwg, 400.0, vec![("selected", Data::nums([line_id as f64])), ("highlight", Data::Num(0x00FF80FFu32 as f64))]);
    let (x, y) = at(50.0, 10.0);
    assert!(like_any(&img.rgba, img.w, x, y, 1, [0, 255, 128]), "{:?}", px(&img.rgba, img.w, x, y));
    save_png("cad_selected.png", img.w, img.h, &img.rgba);
}

#[test]
fn survey_coordinates_at_deep_zoom() {
    // a line at y = 1 000 000.3 among coordinates in the millions
    let x0 = 2_500_000.0;
    let y0 = 999_950.0;
    let dxf_text = format!(
        "  0\nSECTION\n  2\nENTITIES\n  0\nLINE\n  5\n10\n  8\n0\n 62\n1\n 10\n{}\n 20\n{}\n 11\n{}\n 21\n{}\n  0\nCIRCLE\n  5\n11\n  8\n0\n 62\n3\n 10\n{}\n 20\n{}\n 40\n20.0\n  0\nENDSEC\n  0\nEOF\n",
        x0,
        1_000_000.3,
        x0 + 100.0,
        1_000_000.3,
        x0 + 50.0,
        1_000_050.0
    );
    let dwg = drawing(dxf_text.as_bytes());
    let src = tile_source(Rect::new(0.0, 0.0, 400.0, 400.0));
    let params = val(Data::map([("drawing", dwg), ("rect", Data::nums([x0, y0, 100.0, 100.0])), ("background", Data::Num(BLACK as f64))]));
    // page y = (y0 + 100 - 1_000_000.3) * 4 = 198.8; level 7: 128 px per page unit
    let level = 7;
    let k = 128.0f64;
    let py = 198.8 * k;
    let ty = (py / 256.0).floor() as i32;
    let row = py - ty as f64 * 256.0;
    let px_page = 200.0 * k; // drawing x = x0 + 50
    let tx = (px_page / 256.0).floor() as i32;
    let tile = src.render(TileKey { page: 0, level, tx, ty }, &val_clone(&params), &|| false).expect("tile");
    save_png("cad_survey_l7.png", TILE_SIZE, TILE_SIZE, &tile);
    let reds: Vec<u32> = (0..TILE_SIZE).filter(|&y| px(&tile, TILE_SIZE, 100, y)[0] > 40).collect();
    assert!(!reds.is_empty(), "the line is in the tile");
    let centre = reds.iter().sum::<u32>() as f64 / reds.len() as f64;
    assert!((centre - (row - 0.5)).abs() < 1.0, "line centre row {centre}, expected {row}");
    assert!(reds.len() <= 3, "crisp: {reds:?}");
}

fn val_clone(v: &anypage_core::Value) -> anypage_core::Value {
    v.clone()
}

#[test]
fn large_drawing_timings() {
    // 100 000 lines in a 1000 x 1000 grid of short segments
    let mut s = String::from("  0\nSECTION\n  2\nENTITIES\n");
    for i in 0..100_000u64 {
        let h = 0x100 + i;
        let x = (i % 1000) as f64;
        let y = (i / 1000) as f64 * 10.0;
        s.push_str(&format!("  0\nLINE\n  5\n{h:X}\n  8\nL{}\n 10\n{x}\n 20\n{y}\n 11\n{}\n 21\n{}\n", i % 7, x + 0.8, y + 5.0));
    }
    s.push_str("  0\nENDSEC\n  0\nEOF\n");
    let t = Instant::now();
    let doc = CadDoc::open(s.as_bytes()).expect("open");
    let open_ms = t.elapsed().as_secs_f64() * 1000.0;
    let scene = doc.scene();
    assert_eq!(scene.groups.len(), 100_000);
    let dwg = Data::Res(Res::new(anypage_cad::KIND, doc));
    let src = tile_source(Rect::new(0.0, 0.0, 1000.0, 1000.0));
    let params = val(Data::map([("drawing", dwg.clone())]));
    let t = Instant::now();
    let overview = src.render(TileKey { page: 0, level: -2, tx: 0, ty: 0 }, &params, &|| false).expect("tile");
    let overview_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let detail = src.render(TileKey { page: 0, level: 2, tx: 3, ty: 3 }, &params, &|| false).expect("tile");
    let detail_ms = t.elapsed().as_secs_f64() * 1000.0;
    save_png("cad_100k_overview.png", TILE_SIZE, TILE_SIZE, &overview);
    save_png("cad_100k_detail.png", TILE_SIZE, TILE_SIZE, &detail);
    eprintln!("100k lines: open {open_ms:.0} ms, overview tile {overview_ms:.1} ms, detail tile {detail_ms:.1} ms ({} bytes DXF)", s.len());
    assert!(open_ms < 20_000.0);
    assert!(overview_ms < 5_000.0 && detail_ms < 2_000.0);
    // something was drawn in both
    let bg = anypage_cad::default_background();
    let drawn = |t: &[u8]| t.as_chunks::<4>().0.iter().filter(|p| p[..3] != bg[..3]).count();
    assert!(drawn(&overview) > 1000);
    assert!(drawn(&detail) > 100);
}

/// Colours and layers that depend on the inserting entity (BYBLOCK, layer
/// 0) resolve per instance through two levels of blocks.
#[test]
fn nested_block_colours_resolve_per_instance() {
    let buf = nested_colour_dxf();
    let dwg = drawing(&buf);
    let (doc, _) = anypage_cad::doc_of(&dwg).unwrap();
    let render = |layers: Data| {
        let o = Data::map([
            ("rect", Data::nums([-1.0, -1.0, 32.0, 16.0])),
            ("width_px", Data::Num(640.0)),
            ("background", Data::Num(BLACK as f64)),
            ("layers", layers),
        ]);
        render_image(&doc, &o, &|| false).unwrap()
    };
    let img = render(Data::Nil);
    save_png("cad_nested_colours.png", img.w, img.h, &img.rgba);
    // 20 px per unit; drawing (x, y) → pixel ((x + 1) * 20, (15 - y) * 20)
    let at = |x: f64, y: f64| (((x + 1.0) * 20.0).round() as u32, ((15.0 - y) * 20.0).round() as u32);
    let check = |img: &anypage_engine::Image, x: f64, y: f64, c: [u8; 3], what: &str| {
        let (px_, py_) = at(x, y);
        assert!(like_any(&img.rgba, img.w, px_, py_, 1, c), "{what}: {:?}", px(&img.rgba, img.w, px_, py_));
    };
    let (green, red, blue, yellow, white) = ([0, 255, 0], [255, 0, 0], [0, 0, 255], [255, 255, 0], [255, 255, 255]);
    check(&img, 5.0, 0.0, green, "BYBLOCK through two BYBLOCK levels: the top insert's green");
    check(&img, 5.0, 2.0, red, "layer 0 BYLAYER through two levels: the top insert's layer RED");
    check(&img, 5.0, 4.0, blue, "own layer BLUE");
    check(&img, 25.0, 0.0, yellow, "BYBLOCK under the yellow inner insert");
    check(&img, 25.0, 2.0, red, "layer 0 under an inner insert on layer 0: still RED");
    check(&img, 5.0, 10.0, white, "BYBLOCK under a BYLAYER insert on layer 0: white on black");
    check(&img, 5.0, 12.0, white, "layer 0 all the way down");
    check(&img, 25.0, 10.0, yellow, "yellow inner insert in the second outer");
    // RED off hides the first outer insert (its own layer), BLUE off hides the BLUE lines inside blocks
    let img = render(Data::map([("RED", Data::Bool(false)), ("BLUE", Data::Bool(false))]));
    let (x, y) = at(5.0, 0.0);
    assert!(!like_any(&img.rgba, img.w, x, y, 2, green), "RED insert hidden");
    let (x, y) = at(5.0, 14.0);
    assert!(!like_any(&img.rgba, img.w, x, y, 2, blue), "BLUE lines hidden inside blocks");
    check(&img, 5.0, 12.0, white, "the rest still drawn");
    // picking reaches into the blocks and answers with the top-level insert
    let ids: Vec<u64> = doc.scene().groups.iter().map(|g| g.id).collect();
    assert_eq!(anypage_cad::pick(&doc.scene(), P { x: 25.0, y: 14.0 }, 0.2, &[]), Some(ids[1]));
    assert_eq!(anypage_cad::pick(&doc.scene(), P { x: 15.0, y: 14.0 }, 0.2, &[]), None);
    // 12 lines of 10 drawn through the two inserts
    let m = anypage_cad::measure(&doc.scene(), &ids.iter().copied().collect()).unwrap();
    assert!((m.get("length").num().unwrap() - 12.0 * 10.0).abs() < 1e-9, "{m:?}");
}
