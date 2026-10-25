//! Tiles must join without seams: a mosaic of tiles equals one render of
//! the same view.

mod common;

use anypage_cad::internals::{fit, render, Aff, BBox, Style};
use anypage_cad::{tile_source, CadDoc};
use anypage_core::{val, Rect, TileKey, TILE_SIZE};
use anypage_runtime::{Data, Res};
use common::*;

#[test]
fn tile_mosaic_equals_one_render() {
    // curves and lines crossing the seams between tiles at several angles
    let mut s = String::from("  0\nSECTION\n  2\nENTITIES\n");
    let bodies = [
        ("ELLIPSE", " 62\n4\n 10\n64\n 20\n64\n 30\n0\n 11\n40\n 21\n0\n 31\n0\n 40\n0.6\n 41\n0\n 42\n6.283185307179586\n"),
        ("CIRCLE", " 62\n2\n 10\n40\n 20\n32.4\n 40\n10\n"),
        ("LINE", " 62\n1\n 10\n5\n 20\n70\n 11\n120\n 21\n58\n"),
        ("LINE", " 62\n3\n 10\n70\n 20\n5\n 11\n58\n 21\n120\n"),
    ];
    for (i, (kind, rest)) in bodies.iter().enumerate() {
        s.push_str(&format!("  0\n{kind}\n  5\n{:X}\n  8\n0\n{rest}", 0x20 + i));
    }
    s.push_str("  0\nENDSEC\n  0\nEOF\n");
    let doc = CadDoc::open(s.as_bytes()).expect("open");
    let scene = doc.scene();
    let dwg = Data::Res(Res::new(anypage_cad::KIND, doc));
    let view = BBox::new(0.0, 0.0, 128.0, 128.0);
    let n = TILE_SIZE;
    // layer rect 512 x 512 page units, level 0: 2 x 2 tiles
    let src = tile_source(Rect::new(0.0, 0.0, 512.0, 512.0));
    let params = val(Data::map([("drawing", dwg), ("rect", Data::nums([0.0, 0.0, 128.0, 128.0])), ("background", Data::Num(255.0))]));
    let w = 2 * n;
    let mut mosaic = vec![0u8; (w * w * 4) as usize];
    for ty in 0..2u32 {
        for tx in 0..2u32 {
            let tile = src.render(TileKey { page: 0, level: 0, tx: tx as i32, ty: ty as i32 }, &params, &|| false).unwrap();
            for y in 0..n {
                let d = (((ty * n + y) * w + tx * n) * 4) as usize;
                let s = (y * n * 4) as usize;
                mosaic[d..d + (n * 4) as usize].copy_from_slice(&tile[s..s + (n * 4) as usize]);
            }
        }
    }
    let mut style = Style::new(&scene, [0, 0, 0, 255]);
    style.line_px = 1.0;
    let xf: Aff = fit(&view, (0.0, 0.0, 512.0, 512.0), true);
    let whole = render(&scene, &xf, w, w, &style, &|| false).unwrap();
    save_png("cad_seams_mosaic.png", w, w, &mosaic);
    save_png("cad_seams_whole.png", w, w, &whole);
    let mut worst = 0i32;
    let mut at = (0, 0);
    for y in 0..w {
        for x in 0..w {
            let (a, b) = (px(&mosaic, w, x, y), px(&whole, w, x, y));
            let d = (0..3).map(|k| (a[k] as i32 - b[k] as i32).abs()).max().unwrap();
            if d > worst {
                worst = d;
                at = (x, y);
            }
        }
    }
    // curves are flattened per tile, so anti-aliasing may differ slightly
    assert!(worst <= 24, "tiles differ from one render by {worst} at {at:?}");
}
