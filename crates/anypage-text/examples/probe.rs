//! Print shaping details for a few strings (diagnostics).
//!
//! cargo run --example probe -- "text" ...

use anypage_text::TextSystem;

fn main() {
    let t = TextSystem::load().expect("fonts");
    let t0 = std::time::Instant::now();
    t.measure("\u{F0000}", 16.0);
    println!("first character no font covers: {:.1} ms (probes every face once)", t0.elapsed().as_secs_f64() * 1e3);
    let t0 = std::time::Instant::now();
    t.measure("\u{F0001} again", 16.0);
    println!("second one: {:.2} ms", t0.elapsed().as_secs_f64() * 1e3);
    // colour glyph rasterisation, cold, per glyph
    let emoji = ["😀", "❤️", "🌈", "👍🏽", "🎉", "🇹🇼", "👨\u{200D}👩\u{200D}👧"];
    let mut buf = vec![0u8; 256 * 256 * 4];
    for px in [16.0f32, 48.0, 128.0] {
        t.clear_caches();
        for e in emoji {
            t.measure(e, px); // shaping out of the timing
        }
        let t0 = std::time::Instant::now();
        for e in emoji {
            t.draw_line(&mut buf, 256, 256, (0, 0, 256, 256), 0.0, 0.0, px, [0, 0, 0, 255], e);
        }
        println!("{} emoji rasterised cold at {px} px: {:.2} ms each", emoji.len(), t0.elapsed().as_secs_f64() * 1e3 / emoji.len() as f64);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let defaults = ["สวัสดี", "สวสด", "ส", "ั", "คำ"];
    let list: Vec<&str> = if args.is_empty() { defaults.to_vec() } else { args.iter().map(String::as_str).collect() };
    for s in list {
        println!("{s:?}: width {:.2} rtl {} missing {}", t.measure(s, 20.0), t.is_rtl(s), t.missing_glyphs(s));
        for g in t.glyphs(s, 20.0) {
            println!("   {:?} id {} x {:.2} adv {:.2} y {:.2} font {}", g.cluster, g.glyph_id, g.x, g.advance, g.y, t.font_name(g.font));
        }
    }
}
