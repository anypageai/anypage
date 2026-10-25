//! Timings for the text system: font loading, shaping 10 000 characters of
//! mixed scripts, layout, and draw_line throughput (one and four threads).
//!
//! cargo run --release --example timings

use std::sync::Arc;
use std::time::Instant;

use anypage_text::TextSystem;

const SAMPLES: &[&str] = &[
    "The quick brown fox jumps over the lazy dog, again and again. ",
    "敏捷的棕色狐狸跳過了懶狗，一次又一次地跳過去。漢字排版測試。",
    "النص العربي يتصل بعضه ببعض ويكتب من اليمين إلى اليسار. ",
    "טקסט עברי נכתב מימין לשמאל ואינו מחובר. ",
    "हिन्दी पाठ में संयुक्ताक्षर और मात्राएँ होती हैं। ",
    "ภาษาไทยไม่มีการเว้นวรรคระหว่างคำ สวัสดีครับ ",
    "Emoji 👍🏽 👨‍👩‍👧‍👦 🇹🇼 ❤️ mixed with text. ",
];

/// About `n` characters of mixed-script text, as distinct lines (so nothing
/// is a cache hit the first time through).
fn corpus(n: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut count = 0;
    let mut i = 0;
    while count < n {
        let s = format!("{} {}", i, SAMPLES[i % SAMPLES.len()]);
        count += s.chars().count();
        lines.push(s);
        i += 1;
    }
    lines
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    let t = Instant::now();
    let tr = match TextSystem::load() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("font load failed: {e}");
            std::process::exit(1);
        }
    };
    println!("font load: {:.1} ms", ms(t));
    println!("{}", tr.describe());

    let lines = corpus(10_000);
    let chars: usize = lines.iter().map(|l| l.chars().count()).sum();

    let t = Instant::now();
    let mut w = 0.0;
    for l in &lines {
        w += tr.measure(l, 16.0);
    }
    let cold = ms(t);
    println!("shape {chars} chars of mixed scripts ({} lines), cold: {cold:.1} ms ({:.2} us/char), total width {w:.0} px", lines.len(), cold * 1000.0 / chars as f64);

    let t = Instant::now();
    let mut w2 = 0.0;
    for l in &lines {
        w2 += tr.measure(l, 16.0);
    }
    let warm = ms(t);
    println!("measure same lines again (cached): {warm:.2} ms ({:.0} ns/line), same total: {}", warm * 1e6 / lines.len() as f64, w == w2);

    let t = Instant::now();
    let mut w3 = 0.0;
    for l in &lines {
        w3 += tr.measure(l, 23.5);
    }
    println!("measure same lines at a new size (cached shape, scaled): {:.2} ms, total {w3:.0} px", ms(t));

    let text = lines.join("\n");
    let t = Instant::now();
    let laid = tr.layout_ranges(&text, 16.0, 400.0);
    println!("layout_ranges of the whole text at 400 px: {} lines in {:.2} ms (shapes cached)", laid.len(), ms(t));

    let big: String = SAMPLES.iter().cycle().take(SAMPLES.len() * 30).cloned().collect();
    let t = Instant::now();
    let laid = tr.layout_ranges(&big, 16.0, 640.0);
    println!("layout_ranges of a new {}-char paragraph (shaping included): {} lines in {:.2} ms", big.chars().count(), laid.len(), ms(t));

    let (w_px, h_px) = (1024u32, 768u32);
    let mut buf = vec![0u8; (w_px * h_px * 4) as usize];
    let clip = (0, 0, w_px as i32, h_px as i32);
    let glyphs_per_pass: usize = lines.iter().take(40).map(|l| tr.glyphs(l, 16.0).len()).sum();

    let t = Instant::now();
    for (i, l) in lines.iter().take(40).enumerate() {
        tr.draw_line(&mut buf, w_px, h_px, clip, 4.0, i as f32 * 19.0, 16.0, [20, 20, 20, 255], l);
    }
    println!("draw_line 40 lines, cold glyph cache: {:.2} ms", ms(t));

    let passes = 200;
    let t = Instant::now();
    for _ in 0..passes {
        for (i, l) in lines.iter().take(40).enumerate() {
            tr.draw_line(&mut buf, w_px, h_px, clip, 4.0, i as f32 * 19.0, 16.0, [20, 20, 20, 255], l);
        }
    }
    let el = ms(t);
    let n = passes * 40;
    println!(
        "draw_line warm: {n} lines in {el:.1} ms = {:.1} us/line, {:.0} k glyphs/s ({glyphs_per_pass} glyphs per 40 lines)",
        el * 1000.0 / n as f64,
        (passes * glyphs_per_pass) as f64 / el
    );

    // one cached line, repeated: per-glyph cost of the warm path
    let one = "The quick brown fox jumps over the lazy dog, again and again.";
    let one_glyphs = tr.glyphs(one, 16.0).len();
    tr.draw_line(&mut buf, w_px, h_px, clip, 4.0, 4.0, 16.0, [0, 0, 0, 255], one);
    let mut best = f64::MAX;
    for _ in 0..5 {
        let n = 5_000;
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(tr.draw_line(&mut buf, w_px, h_px, clip, 4.0, 4.0, 16.0, [0, 0, 0, 255], std::hint::black_box(one)));
        }
        best = best.min(ms(t) * 1e6 / n as f64);
    }
    println!("draw_line one cached {one_glyphs}-glyph line at 16 px (best of 5): {:.1} us/line, {:.0} ns/glyph", best / 1000.0, best / one_glyphs as f64);
    let mut best = f64::MAX;
    for _ in 0..5 {
        let n = 100_000;
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(tr.measure(std::hint::black_box(one), 16.0));
        }
        best = best.min(ms(t) * 1e6 / n as f64);
    }
    println!("measure of a cached line (best of 5): {best:.0} ns");

    // glyphs over the 1024 px image cap: filled directly / scaled colour
    for (s, px) in [("H", 1600.0f32), ("漢", 1100.0), ("😀", 1200.0), ("😀", 2000.0)] {
        let (w, h) = ((tr.measure(s, px).ceil() as u32 + 8).min(3000), (tr.line_height(px).ceil() as u32 + 8).min(3000));
        let mut big = vec![0u8; (w * h * 4) as usize];
        let clip = (0, 0, w as i32, h as i32);
        let t = Instant::now();
        tr.draw_line(&mut big, w, h, clip, 4.0, 4.0, px, [0, 0, 0, 255], s);
        let cold = ms(t);
        let t = Instant::now();
        tr.draw_line(&mut big, w, h, clip, 4.0, 4.0, px, [0, 0, 0, 255], s);
        println!("draw_line {s:?} at {px} px into {w}x{h}: first {cold:.1} ms, again {:.1} ms", ms(t));
    }

    // a 1 MiB paragraph on one line: cold, then warm (pieces cached)
    let mut para = String::new();
    let mut i = 0;
    while para.len() < anypage_text::MAX_LINE_BYTES - 32 {
        para.push_str(&format!("{} {i} ", ["lorem", "ipsum", "dolor", "sit", "amet,", "consectetur"][i % 6]));
        i += 1;
    }
    let mid = para[..para.len() / 2].rfind(' ').unwrap_or(0) + 1;
    let t = Instant::now();
    let w = tr.measure(&para, 16.0);
    let cold = ms(t);
    let t = Instant::now();
    tr.measure(&para, 16.0);
    let warm = ms(t);
    let t = Instant::now();
    let x = tr.caret_x(&para, 16.0, mid);
    let caret = ms(t);
    let t = Instant::now();
    let hit = tr.hit_x(&para, 16.0, x);
    let hit_ms = ms(t);
    let t = Instant::now();
    let n = tr.layout_ranges(&para, 16.0, 640.0).len();
    let lay_cold = ms(t);
    let t = Instant::now();
    tr.layout_ranges(&para, 16.0, 640.0);
    let lay_warm = ms(t);
    let mut edited = para.clone();
    edited.insert(mid, 'X');
    let t = Instant::now();
    tr.caret_x(&edited, 16.0, mid + 1);
    let edit = ms(t);
    println!(
        "1 MiB paragraph ({w:.0} px): measure cold {cold:.0} ms, warm {warm:.2} ms; caret_x {caret:.2} ms, hit_x {hit_ms:.2} ms (back at {}); layout_ranges {n} lines cold (tables built) {lay_cold:.1} ms, warm {lay_warm:.1} ms; caret_x after a one-character edit {edit:.1} ms",
        hit == mid
    );

    let tr = Arc::new(tr);
    let lines = Arc::new(lines);
    let t = Instant::now();
    let handles: Vec<_> = (0..4)
        .map(|k| {
            let tr = tr.clone();
            let lines = lines.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; (w_px * h_px * 4) as usize];
                for _ in 0..passes / 4 {
                    for (i, l) in lines.iter().skip(k * 10).take(40).enumerate() {
                        tr.draw_line(&mut buf, w_px, h_px, clip, 4.0, i as f32 * 19.0, 16.0, [20, 20, 20, 255], l);
                    }
                }
            })
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
    println!("draw_line 4 threads: {n} lines in {:.1} ms", ms(t));
    let s = tr.stats();
    println!(
        "caches: {} lines ({} KB), {} glyph images ({} KB), {} fonts in use of {} faces",
        s.lines_cached,
        s.line_bytes / 1024,
        s.glyphs_cached,
        s.glyph_bytes / 1024,
        s.fonts_in_use,
        s.faces
    );
}
