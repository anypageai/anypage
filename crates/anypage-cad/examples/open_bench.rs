//! Timings for opening and drawing a large DXF:
//! `cargo run --release --example open_bench [entities]`.

use std::time::Instant;

use anypage_cad::{tile_source, CadDoc};
use anypage_core::{val, Rect, TileKey};
use anypage_runtime::{Data, Res};

fn main() {
    let n: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let mut s = String::from("  0\nSECTION\n  2\nENTITIES\n");
    for i in 0..n {
        let x = (i % 1000) as f64;
        let y = (i / 1000) as f64 * 10.0;
        match i % 4 {
            0 | 1 => s.push_str(&format!("  0\nLINE\n  5\n{:X}\n  8\nL{}\n 10\n{x}\n 20\n{y}\n 11\n{}\n 21\n{}\n", 0x100 + i, i % 7, x + 0.8, y + 5.0)),
            2 => s.push_str(&format!("  0\nCIRCLE\n  5\n{:X}\n  8\nC\n 10\n{x}\n 20\n{y}\n 40\n0.4\n", 0x100 + i)),
            _ => s.push_str(&format!("  0\nTEXT\n  5\n{:X}\n  8\nT\n 10\n{x}\n 20\n{y}\n 40\n0.5\n  1\nT{i}\n", 0x100 + i)),
        }
    }
    s.push_str("  0\nENDSEC\n  0\nEOF\n");
    let bytes = s.into_bytes();
    let t = Instant::now();
    let font = anypage_cad::internals::font();
    println!("font {:?} loaded in {:.0} ms", font.map(|f| f.source.clone()), t.elapsed().as_secs_f64() * 1000.0);
    let t = Instant::now();
    let d = dxf::Drawing::load(&mut &bytes[..]).expect("dxf");
    let parse = t.elapsed().as_secs_f64() * 1000.0;
    drop(d);
    let t = Instant::now();
    let doc = CadDoc::open(&bytes).expect("open");
    let open = t.elapsed().as_secs_f64() * 1000.0;
    let phases = doc.load_timings();
    let t = Instant::now();
    let saved = doc.save().expect("save");
    let save = t.elapsed().as_secs_f64() * 1000.0;
    let dwg = Data::Res(Res::new(anypage_cad::KIND, doc));
    let t = Instant::now();
    let all = anypage_cad::internals::entities(&dwg, &Data::Nil).expect("entities");
    let entities_ms = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(all.items().len() as u64, n);
    let src = tile_source(Rect::new(0.0, 0.0, 1000.0, 1000.0));
    let params = val(Data::map([("drawing", dwg)]));
    let mut tiles = Vec::new();
    for (level, tx, ty) in [(-2, 0, 0), (0, 1, 1), (2, 5, 5), (5, 40, 40)] {
        let t = Instant::now();
        src.render(TileKey { page: 0, level, tx, ty }, &params, &|| false).expect("tile");
        tiles.push(format!("L{level} {:.1} ms", t.elapsed().as_secs_f64() * 1000.0));
    }
    println!(
        "{n} entities, {:.1} MB: dxf crate parse alone {parse:.0} ms; cad.open {open:.0} ms = parse {:.0} + hatch scan {:.0} + fixups {:.0} + scene {:.0}; save {save:.0} ms ({:.1} MB); entities() native part {entities_ms:.0} ms; tiles: {}",
        bytes.len() as f64 / 1048576.0,
        phases[0],
        phases[1],
        phases[2],
        phases[3],
        saved.len() as f64 / 1048576.0,
        tiles.join(", ")
    );
}
