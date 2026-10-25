//! Script coverage, carets, line breaking and drawing with the system fonts.
//! PNGs land in target/test-output/text-*.png and are checked numerically.
//!
//! Script-specific checks skip (with a message) when no installed font
//! covers the sample, so the suite still runs on a bare Linux box; on
//! Windows and Android every sample below is covered.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use anypage_text::{Direction, LoadOptions, TextSystem};

fn tr() -> &'static TextSystem {
    static T: OnceLock<TextSystem> = OnceLock::new();
    T.get_or_init(|| {
        let t = Instant::now();
        let s = TextSystem::load().expect("system fonts");
        eprintln!("TextSystem::load: {:.1} ms\n{}", t.elapsed().as_secs_f64() * 1e3, s.describe());
        s
    })
}

/// Whether every character of `s` has an installed font. On Windows and
/// Android all the samples must be covered (a miss there is a font
/// discovery bug, not a reason to skip); elsewhere the test skips.
fn covered(s: &str) -> bool {
    let missing = tr().missing_glyphs(s);
    if missing > 0 {
        #[cfg(any(windows, target_os = "android"))]
        panic!("{missing} characters of {s:?} have no font:\n{}", tr().describe());
        #[cfg(not(any(windows, target_os = "android")))]
        eprintln!("skipped: {missing} characters of {s:?} have no installed font");
    }
    missing == 0
}

/// A white canvas with text drawn on it.
struct Canvas {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        Self { w, h, px: vec![255; (w * h * 4) as usize] }
    }

    fn text(&mut self, x: f32, y: f32, size: f32, s: &str) -> f32 {
        let clip = (0, 0, self.w as i32, self.h as i32);
        tr().draw_line(&mut self.px, self.w, self.h, clip, x, y, size, [0, 0, 0, 255], s)
    }

    /// Darkness 0..=255 of a pixel (opaque white background).
    fn ink_at(&self, x: u32, y: u32) -> u32 {
        let i = ((y * self.w + x) * 4) as usize;
        255 - (self.px[i] as u32 + self.px[i + 1] as u32 + self.px[i + 2] as u32) / 3
    }

    fn ink(&self) -> u64 {
        let mut sum = 0;
        for y in 0..self.h {
            for x in 0..self.w {
                sum += self.ink_at(x, y) as u64;
            }
        }
        sum
    }

    fn column_ink(&self) -> Vec<u64> {
        (0..self.w).map(|x| (0..self.h).map(|y| self.ink_at(x, y) as u64).sum()).collect()
    }

    /// Pixels with clear colour (not grey).
    fn chroma_pixels(&self) -> usize {
        self.px
            .as_chunks::<4>().0.iter()
            .filter(|p| {
                let (mx, mn) = (p[0].max(p[1]).max(p[2]), p[0].min(p[1]).min(p[2]));
                mx - mn > 60
            })
            .count()
    }

    fn save(&self, name: &str) {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-output");
        std::fs::create_dir_all(&dir).expect("output dir");
        let f = std::fs::File::create(dir.join(format!("text-{name}.png"))).expect("png file");
        let mut e = png::Encoder::new(std::io::BufWriter::new(f), self.w, self.h);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.write_header().expect("png header").write_image_data(&self.px).expect("png data");
    }
}

const SIZE: f32 = 28.0;

/// Draw `s` alone; returns the canvas and the advance.
fn draw(s: &str) -> (Canvas, f32) {
    let w = (tr().measure(s, SIZE).ceil() as u32 + 40).clamp(40, 2400);
    let mut c = Canvas::new(w, 64);
    let adv = c.text(20.0, 10.0, SIZE, s);
    (c, adv)
}

fn graphemes(s: &str) -> Vec<usize> {
    // boundaries the way hit_x reports them: every caret position
    let mut v: Vec<usize> = Vec::new();
    let px = 20.0;
    let w = tr().measure(s, px);
    let mut x = -5.0;
    while x < w + 5.0 {
        let b = tr().hit_x(s, px, x);
        if !v.contains(&b) {
            v.push(b);
        }
        x += 0.5;
    }
    v.sort();
    v
}

#[test]
fn send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<TextSystem>();
}

/// The engine's `TextRaster` API, as the engine, runtime and hosts call it:
/// if these coercions compile, the swap compiles.
#[test]
fn same_signatures_as_the_engine_text_raster() {
    use std::ops::Range;
    let _: fn() -> Result<TextSystem, String> = TextSystem::load;
    let _: fn(&TextSystem, &str, f32) -> f32 = TextSystem::measure;
    let _: fn(&TextSystem, f32) -> f32 = TextSystem::line_height;
    let _: fn(&TextSystem, f32) -> (f32, f32) = TextSystem::line_metrics;
    let _: fn(&TextSystem, &str, f32, usize) -> f32 = TextSystem::caret_x;
    let _: fn(&TextSystem, &str, f32, f32) -> usize = TextSystem::hit_x;
    type Ranges = Vec<(Range<usize>, f32)>;
    let _: fn(&TextSystem, &str, f32, f32) -> Ranges = TextSystem::layout_ranges;
    let _: fn(&TextSystem, &str, f32, f32) -> Vec<String> = TextSystem::layout;
    let _: fn(Vec<Vec<u8>>) -> TextSystem = TextSystem::from_fonts;
    let _: fn(&TextSystem) -> String = TextSystem::describe;
    let source: &String = &tr().source;
    assert!(!source.is_empty());
}

/// `draw_line` places the line's top-left at (x, y_top): the baseline is at
/// y_top + ascent, like `Canvas::text`.
#[test]
fn top_left_and_baseline_placement() {
    let t = tr();
    let (px, x, y) = (40.0, 30.0, 20.0);
    let mut c = Canvas::new(200, 120);
    c.text(x, y, px, "H");
    let (ascent, lh) = t.line_metrics(px);
    let rows: Vec<u32> = (0..c.h).filter(|&r| (0..c.w).any(|col| c.ink_at(col, r) > 100)).collect();
    let cols: Vec<u32> = (0..c.w).filter(|&col| (0..c.h).any(|r| c.ink_at(col, r) > 100)).collect();
    let (top, bottom) = (rows[0] as f32, *rows.last().expect("ink") as f32);
    let baseline = (y + ascent).round();
    assert!((bottom + 1.0 - baseline).abs() <= 1.0, "H sits on the baseline: bottom {bottom}, baseline {baseline}");
    assert!(top > y && top < y + ascent, "cap top {top} between y {y} and the baseline");
    assert!(bottom < y + lh);
    let left = cols[0] as f32;
    assert!(left >= x && left < x + 0.2 * px, "left bearing: ink starts at {left}");
}

#[test]
fn describe_names_fonts() {
    let d = tr().describe();
    assert!(d.contains("font faces"), "{d}");
    assert!(d.contains("Han:") && d.contains("Arabic:") && d.contains("Emoji:"), "{d}");
    assert!(!tr().source.is_empty());
    let s = tr().stats();
    assert!(s.faces > 0 && s.fonts_in_use > 0);
}

#[test]
fn latin_widths_carets_and_png() {
    let t = tr();
    let s = "Hello, world! AV fi";
    let w = t.measure(s, 20.0);
    assert!(w > 50.0 && w < 400.0, "{w}");
    // linear in the size
    assert!((t.measure(s, 40.0) - 2.0 * w).abs() < 0.01 * w);
    assert_eq!(t.measure("", 20.0), 0.0);
    // carets: monotonic, from 0 to the width
    let b: Vec<usize> = s.char_indices().map(|(i, _)| i).chain([s.len()]).collect();
    let xs: Vec<f32> = b.iter().map(|&i| t.caret_x(s, 20.0, i)).collect();
    assert_eq!(xs[0], 0.0);
    assert!((xs[xs.len() - 1] - w).abs() < 0.01);
    assert!(xs.windows(2).all(|p| p[1] >= p[0]), "{xs:?}");
    // hit testing returns the caret it was given
    for &i in &b {
        assert_eq!(t.hit_x(s, 20.0, t.caret_x(s, 20.0, i)), i);
    }
    assert_eq!(t.hit_x(s, 20.0, -100.0), 0);
    assert_eq!(t.hit_x(s, 20.0, 1e6), s.len());
    // past the ends, even where every caret is equally far away in f32
    for x in [f32::INFINITY, f32::MAX, 1e30, 1e9] {
        assert_eq!(t.hit_x(s, 16.0, x), s.len(), "x = {x}");
    }
    for x in [f32::NEG_INFINITY, -1e30, -1e9] {
        assert_eq!(t.hit_x(s, 16.0, x), 0, "x = {x}");
    }
    // a trailing zero-width character is still before the end
    assert_eq!(t.hit_x("ab\u{200B}", 16.0, f32::INFINITY), "ab\u{200B}".len());
    let (c, adv) = draw(s);
    assert!((adv - t.measure(s, SIZE)).abs() < 0.01);
    assert!(c.ink() > 20_000, "ink {}", c.ink());
    c.save("latin");
}

#[test]
fn cjk_widths_and_png() {
    let s = "漢字排版，測試。";
    if !covered(s) {
        return;
    }
    let t = tr();
    let w = t.measure("漢字", 20.0);
    assert!((w - 40.0).abs() < 4.0, "two ideographs at 20 px: {w}");
    assert!((t.caret_x("漢字", 20.0, 3) - w / 2.0).abs() < 1.0);
    assert_eq!(graphemes("漢字"), vec![0, 3, 6]);
    let (c, _) = draw(s);
    assert!(c.ink() > 30_000);
    c.save("cjk");
}

/// Ink of `a` minus ink of `b`, per column, both drawn at the same x.
fn column_diff(a: &Canvas, b: &Canvas) -> Vec<u64> {
    let (ca, cb) = (a.column_ink(), b.column_ink());
    ca.iter().zip(cb.iter().chain(std::iter::repeat(&0))).map(|(x, y)| x.abs_diff(*y)).collect()
}

fn centre(v: &[u64]) -> f64 {
    let total: u64 = v.iter().sum();
    v.iter().enumerate().map(|(i, &m)| i as f64 * m as f64).sum::<f64>() / total.max(1) as f64
}

/// Right-to-left check on pixels: removing the first logical character
/// changes only the right end of the drawing (the rest keeps its place
/// relative to the left edge), so the first character is drawn on the right.
fn assert_first_char_on_the_right(s: &str, name: &str) {
    let t = tr();
    let first_len = s.chars().next().map(char::len_utf8).unwrap_or(0);
    let rest = &s[first_len..];
    let (full, wf) = draw(s);
    let mut short = Canvas::new(full.w, full.h);
    short.text(20.0, 10.0, SIZE, rest);
    let diff = column_diff(&full, &short);
    let total: u64 = diff.iter().sum();
    assert!(total > 2_000, "{name}: dropping the first character changed nothing");
    let c = centre(&diff);
    let (left, right) = (20.0, 20.0 + wf as f64);
    assert!(c > left + 0.6 * (right - left), "{name}: change centred at {c:.0} in {left:.0}..{right:.0}: first character not on the right");
    let left_part: u64 = diff[..(left + 0.35 * (right - left)) as usize].iter().sum();
    assert!(left_part * 20 < total, "{name}: the left part moved ({left_part} of {total})");
    assert!(t.is_rtl(s), "{name}: not detected as right to left");
    full.save(name);
}

#[test]
fn arabic_joins_runs_right_to_left_and_png() {
    let t = tr();
    let s = "مرحبا بالعالم";
    if !covered(s) {
        return;
    }
    // joined forms are narrower than isolated ones
    let joined = t.measure("بببب", 20.0);
    let isolated = 4.0 * t.measure("ب", 20.0);
    assert!(joined < 0.9 * isolated, "joined {joined} vs isolated {isolated}");
    // carets run right to left
    let w = t.measure(s, 20.0);
    assert!((t.caret_x(s, 20.0, 0) - w).abs() < 1.0);
    assert!(t.caret_x(s, 20.0, s.len()) < 1.0);
    let c2 = t.caret_x(s, 20.0, 2);
    assert!(c2 < w && c2 > 0.0);
    assert_first_char_on_the_right(s, "arabic");
}

#[test]
fn hebrew_runs_right_to_left_and_png() {
    let t = tr();
    let s = "שלום עולם";
    if !covered(s) {
        return;
    }
    let w = t.measure(s, 20.0);
    let xs: Vec<f32> = s.char_indices().map(|(i, _)| i).chain([s.len()]).map(|i| t.caret_x(s, 20.0, i)).collect();
    assert!((xs[0] - w).abs() < 1.0 && xs[xs.len() - 1] < 1.0, "{xs:?}");
    assert!(xs.windows(2).all(|p| p[1] <= p[0]), "carets must decrease: {xs:?}");
    for (k, (i, _)) in s.char_indices().enumerate() {
        assert_eq!(t.hit_x(s, 20.0, xs[k]), i);
    }
    // right to left: the logical end is past the left edge
    for x in [f32::INFINITY, 1e30, 1e9] {
        assert_eq!(t.hit_x(s, 16.0, x), 0, "x = {x}");
    }
    for x in [f32::NEG_INFINITY, -1e30, -1e9] {
        assert_eq!(t.hit_x(s, 16.0, x), s.len(), "x = {x}");
    }
    assert_first_char_on_the_right(s, "hebrew");
}

#[test]
fn mixed_bidi_carets_and_selection() {
    let t = tr();
    let s = "abc אבג def";
    if !covered(s) {
        return;
    }
    assert!(!t.is_rtl(s));
    let aleph = s.find('א').expect("aleph");
    let gimel = s.find('ג').expect("gimel");
    // the Hebrew run is reversed: its first letter's leading edge is right of the last's
    assert!(t.caret_x(s, 20.0, aleph) > t.caret_x(s, 20.0, gimel));
    // and the run sits between "abc " and " def"
    assert!(t.caret_x(s, 20.0, gimel) > t.caret_x(s, 20.0, 3));
    assert!(t.caret_x(s, 20.0, aleph) < t.caret_x(s, 20.0, s.find('d').expect("d")) + 0.5);
    // selecting "c א" covers two separate pieces of the line
    let sel = t.selection_rects(s, 20.0, 2, aleph + 2);
    assert_eq!(sel.len(), 2, "{sel:?}");
    let all = t.selection_rects(s, 20.0, 0, s.len());
    assert_eq!(all.len(), 1);
    assert!((all[0].1 - all[0].0 - t.measure(s, 20.0)).abs() < 0.5);
    let (c, _) = draw(s);
    assert!(c.ink() > 20_000);
    c.save("bidi");
}

#[test]
fn forced_direction_overrides_the_first_strong_character() {
    let t = tr();
    let s = "שלום abc";
    if !covered(s) {
        return;
    }
    assert!(t.is_rtl(s));
    let a = s.find('a').expect("a");
    let auto = t.caret_x(s, 20.0, a);
    let ltr = t.caret_x_dir(s, 20.0, a, Direction::Ltr);
    // auto (RTL): "abc" sits at the left end; LTR: after the Hebrew word
    assert!(auto < 2.0, "{auto}");
    assert!(ltr > t.measure("שלום", 20.0), "{ltr}");
    // same glyphs, same width, the direction mark leaves no trace
    assert_eq!(t.glyphs_dir(s, 20.0, Direction::Ltr).len(), t.glyphs(s, 20.0).len());
    let w_ltr: f32 = t.glyphs_dir(s, 20.0, Direction::Ltr).iter().map(|g| g.advance).sum();
    assert!((w_ltr - t.measure(s, 20.0)).abs() < 0.5);
    assert!(t.glyphs_dir(s, 20.0, Direction::Ltr).iter().all(|g| g.cluster.start < s.len()));
    assert_eq!(t.hit_x_dir(s, 20.0, ltr, Direction::Ltr), a);
    // Rtl on an RTL text and Ltr on Latin are the plain layouts
    assert_eq!(t.caret_x_dir(s, 20.0, a, Direction::Rtl), auto);
    assert_eq!(t.caret_x_dir("abc", 20.0, 1, Direction::Ltr), t.caret_x("abc", 20.0, 1));
    // a neutral-only text forced right to left: digits keep their order but sit at the right
    let n = "123";
    assert!(t.caret_x_dir(n, 20.0, 0, Direction::Rtl) <= t.caret_x_dir(n, 20.0, 3, Direction::Rtl));
}

#[test]
fn devanagari_reorders_and_forms_conjuncts() {
    let t = tr();
    let s = "हिन्दी क्षत्रिय";
    if !covered(s) {
        return;
    }
    // KA + vowel sign I: the sign is drawn before (left of) the consonant
    let ka = t.glyphs("क", 20.0);
    let ki = t.glyphs("कि", 20.0);
    assert_eq!(ka.len(), 1);
    assert_eq!(ki.len(), 2, "{ki:?}");
    assert_ne!(ki[0].glyph_id, ka[0].glyph_id, "vowel sign I must come first visually");
    assert_eq!(ki[1].glyph_id, ka[0].glyph_id);
    assert!(ki[0].x < ki[1].x);
    // KSSA conjunct: fewer glyphs than characters
    let ksa = "क्ष";
    assert!(t.glyphs(ksa, 20.0).len() < ksa.chars().count());
    // one grapheme per syllable: no caret inside "कि"
    assert_eq!(graphemes("कि"), vec![0, "कि".len()]);
    let (c, _) = draw(s);
    assert!(c.ink() > 20_000);
    c.save("devanagari");
}

#[test]
fn thai_marks_stay_on_their_base() {
    let t = tr();
    let s = "สวัสดีครับ";
    if !covered(s) {
        return;
    }
    // above-base vowels have no advance of their own: the word is as wide
    // as its consonants, and the marks are separate zero-width glyphs
    let word = "สวัสดี";
    assert!((t.measure(word, 20.0) - t.measure("สวสด", 20.0)).abs() < 0.5);
    let glyphs = t.glyphs(word, 20.0);
    assert_eq!(glyphs.iter().filter(|g| g.advance == 0.0).count(), 2, "{glyphs:?}");
    // SARA AM decomposes into NIKHAHIT + SARA AA (the Thai shaper at work)
    assert_eq!(t.glyphs("คำ", 20.0).len(), 3);
    // carets only between clusters: ส | วั | ส | ดี
    let b = graphemes(word);
    assert_eq!(b.len(), 5, "{b:?}");
    let (c, _) = draw(s);
    assert!(c.ink() > 15_000);
    c.save("thai");
}

#[test]
fn emoji_zwj_sequence_is_one_colour_glyph() {
    let t = tr();
    let family = "👨\u{200D}👩\u{200D}👧\u{200D}👦";
    if !covered(family) {
        return;
    }
    let one = t.measure("👨", 20.0);
    let seq = t.measure(family, 20.0);
    assert!(one > 10.0, "{one}");
    assert!(seq < 1.6 * one, "ZWJ sequence should be one glyph: {seq} vs {one}");
    assert_eq!(graphemes(family), vec![0, family.len()]);
    let flags = "🇹🇼🇯🇵";
    assert_eq!(graphemes(flags).len(), 3);
    let s = format!("ok {family} 👍🏽 ❤️ 🎉");
    let (c, _) = draw(&s);
    assert!(c.chroma_pixels() > 200, "emoji should be in colour: {} coloured pixels", c.chroma_pixels());
    c.save("emoji");
    // large: gradients and layer order are visible; a red heart is mostly red
    let big = "❤️😀🌈";
    let mut c = Canvas::new(360, 130);
    c.text(10.0, 10.0, 96.0, big);
    assert!(c.chroma_pixels() > 5_000, "{} coloured pixels", c.chroma_pixels());
    let heart_w = t.measure("❤️", 96.0) as u32;
    let red = (10..10 + heart_w)
        .flat_map(|x| (10..120).map(move |y| (x, y)))
        .filter(|&(x, y)| {
            let i = ((y * c.w + x) * 4) as usize;
            let p = &c.px[i..i + 4];
            p[0] > 150 && p[1] < 110 && p[2] < 140
        })
        .count();
    assert!(red > 1_500, "heart should be red: {red} red pixels");
    c.save("emoji-large");
}

#[test]
fn breaking_newlines_spaces_long_words_cjk() {
    let t = tr();
    let px = 20.0;
    let r = |s: &str, w: f32| -> Vec<String> { t.layout(s, px, w) };
    assert_eq!(r("a\nb", 1000.0), vec!["a", "b"]);
    assert_eq!(r("a\n\nb", 1000.0), vec!["a", "", "b"]);
    assert_eq!(r("a\n", 1000.0), vec!["a", ""]);
    assert_eq!(r("a\r\nb", 1000.0), vec!["a", "b"]);
    assert_eq!(r("", 1000.0), vec![""]);
    // a space at a break belongs to neither line
    let hw = t.measure("hello world", px);
    assert_eq!(r("hello world", hw + 1.0), vec!["hello world"]);
    assert_eq!(r("hello world", hw - 1.0), vec!["hello", "world"]);
    // a long word breaks between graphemes, every line within the width
    let long = "Antidisestablishmentarianism";
    let max = t.measure("Antidis", px);
    let lines = t.layout_ranges(long, px, max);
    assert!(lines.len() >= 4, "{lines:?}");
    assert!(lines.iter().all(|(_, w)| *w <= max + 0.01));
    assert_eq!(lines.iter().map(|(r, _)| &long[r.clone()]).collect::<String>(), long);
    // CJK breaks between ideographs, never before 。
    let cjk = "漢字漢字漢字。漢字漢字";
    if covered(cjk) {
        let lines = r(cjk, 2.5 * px);
        assert!(lines.len() >= 4, "{lines:?}");
        assert!(lines.iter().all(|l| !l.starts_with('。')), "{lines:?}");
        assert!(lines.iter().all(|l| t.measure(l, px) <= 2.5 * px + 0.01 || l.chars().count() == 2));
    }
    // widths are the shaped widths of the ranges
    let s = "The quick brown fox jumps over the lazy dog";
    for (range, w) in t.layout_ranges(s, px, 120.0) {
        assert!((w - t.measure(&s[range.clone()], px)).abs() < 0.5, "{:?}", &s[range]);
        assert!(w <= 120.0);
    }
}

#[test]
fn wrapped_paragraph_png() {
    let t = tr();
    let text = "AnyPage 把每一種內容放進同一個頁面。Text in every language: مرحبا بالعالم, שלום, नमस्ते, สวัสดี, 👋🏽.\nSecond paragraph with a verylongwordthatmustbreakbetweengraphemes.";
    let (px, width) = (22.0, 420.0);
    let lines = t.layout_lines(text, px, width);
    let lh = t.line_height(px);
    let mut c = Canvas::new(460, (lines.len() as f32 * lh + 30.0) as u32);
    let clip = (0, 0, c.w as i32, c.h as i32);
    for (i, l) in lines.iter().enumerate() {
        assert!(l.width <= width + 0.01, "line {i} is {} wide", l.width);
        assert_eq!(l.dir, Direction::Ltr, "both paragraphs start with Latin");
        let s = &text[l.range.clone()];
        // right-to-left paragraphs align right
        let x = if l.dir == Direction::Rtl { 20.0 + width - l.width } else { 20.0 };
        t.draw_line_dir(&mut c.px, c.w, c.h, clip, x, 15.0 + i as f32 * lh, px, [0, 0, 0, 255], s, l.dir);
    }
    // the wrapped line that starts with a Hebrew word keeps the paragraph's
    // left-to-right order: the Hebrew word is its leftmost part
    let hebrew_line = lines.iter().find(|l| text[l.range.clone()].starts_with('ש'));
    if let Some(l) = hebrew_line {
        let s = &text[l.range.clone()];
        let after = s.find(',').expect("comma after the Hebrew word");
        assert!(t.caret_x_dir(s, px, after, Direction::Ltr) < 0.3 * l.width, "{s:?}");
    }
    // nothing drawn right of the wrap width
    let cols = c.column_ink();
    assert!(cols[(20.0 + width + 3.0) as usize..].iter().all(|&m| m == 0), "ink beyond the wrap width");
    assert!(c.ink() > 100_000);
    c.save("paragraph");
}

#[test]
fn draw_respects_clip_and_buffer_size() {
    let t = tr();
    let (w, h) = (200u32, 60u32);
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let adv = t.draw_line(&mut buf, w, h, (50, 0, 120, 60), 10.0, 10.0, 24.0, [255, 0, 0, 255], "WWWWWWWWWWWW");
    assert!(adv > 150.0);
    for y in 0..h {
        for x in 0..w {
            let a = buf[((y * w + x) * 4 + 3) as usize];
            if !(50..120).contains(&x) {
                assert_eq!(a, 0, "ink outside the clip at ({x}, {y})");
            }
        }
    }
    assert!(buf.as_chunks::<4>().0.iter().any(|p| p[3] > 200 && p[0] > 200), "nothing drawn inside the clip");
    // premultiplied output: colour never exceeds alpha
    assert!(buf.as_chunks::<4>().0.iter().all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
    // a buffer shorter than w*h*4, an inverted clip, huge coordinates: no panic
    let mut small = vec![0u8; 100];
    t.draw_line(&mut small, w, h, (0, 0, 200, 60), 0.0, 0.0, 24.0, [0, 0, 0, 255], "short buffer");
    t.draw_line(&mut buf, w, h, (120, 50, 10, 0), 0.0, 0.0, 24.0, [0, 0, 0, 255], "inverted clip");
    t.draw_line(&mut buf, w, h, (i32::MIN, i32::MIN, i32::MAX, i32::MAX), -1e9, 1e9, 24.0, [0, 0, 0, 255], "far away");
    t.draw_line(&mut buf, 0, 0, (0, 0, 10, 10), 0.0, 0.0, 24.0, [0, 0, 0, 255], "zero size");
    // half-transparent text blends
    let mut white = Canvas::new(100, 40);
    let clip = (0, 0, 100, 40);
    t.draw_line(&mut white.px, 100, 40, clip, 2.0, 2.0, 28.0, [0, 0, 0, 128], "█");
    let darkest = white.px.as_chunks::<4>().0.iter().map(|p| p[0]).min().unwrap_or(255);
    assert!((120..=135).contains(&darkest), "50% black over white: {darkest}");
}

#[test]
fn hostile_input_does_not_panic() {
    let t = tr();
    let nasty = [
        "abc\nאבג\rdef\u{2029}ghi\u{85}",
        "\u{0}\u{1}\u{1B}[31mred\u{1B}[0m\u{7F}",
        "\u{E000}\u{10FFFD}\u{FFFF}",
        "e\u{301}\u{301}\u{301}\u{301}",
        "\u{202E}reversed\u{202C} \u{2067}isolate\u{2069}",
        "\u{FEFF}\u{200D}\u{200D}",
        "\t\ttabs",
    ];
    for s in nasty {
        let w = t.measure(s, 16.0);
        assert!(w.is_finite() && w >= 0.0);
        for b in 0..=s.len() + 2 {
            let x = t.caret_x(s, 16.0, b);
            assert!(x.is_finite());
        }
        let h = t.hit_x(s, 16.0, w / 2.0);
        assert!(s.is_char_boundary(h));
        let _ = t.layout_ranges(s, 16.0, 30.0);
        let _ = t.selection_rects(s, 16.0, 0, s.len());
        let mut c = Canvas::new(300, 40);
        c.text(0.0, 0.0, 16.0, s);
    }
    // characters no font has are counted and drawn as a replacement
    assert!(t.missing_glyphs("\u{E000}\u{10FFFD}") >= 1);
    for px in [f32::NAN, f32::INFINITY, -3.0, 0.0] {
        assert_eq!(t.measure("abc", px), 0.0);
        assert_eq!(t.hit_x("abc", px, 3.0), 0);
        assert_eq!(t.line_metrics(px), (0.0, 0.0));
    }
    assert_eq!(t.hit_x("abc", 16.0, f32::NAN), 0);
    let (a, lh) = t.line_metrics(1e9);
    assert!(a.is_finite() && lh.is_finite());
    let lines = t.layout_ranges("abc def", 16.0, f32::NAN);
    assert_eq!(lines.len(), 1);
}

#[test]
fn long_text_is_shaped_in_pieces() {
    let t = tr();
    let unit = "word ".repeat(1000); // 5 KB
    let long = unit.repeat(12); // 60 KB, over the 16 KB piece size
    let w_unit = t.measure(&unit, 10.0);
    let w_long = t.measure(&long, 10.0);
    assert!((w_long - 12.0 * w_unit).abs() < 0.01 * w_long, "{w_long} vs {}", 12.0 * w_unit);
    let caret_end = t.caret_x(&long, 10.0, long.len());
    assert!((caret_end - w_long).abs() < 1.0);
    let t0 = Instant::now();
    let lines = t.layout_ranges(&long, 10.0, 500.0);
    assert!(lines.len() > 100);
    eprintln!("layout of 60 KB: {} lines in {:.1} ms", lines.len(), t0.elapsed().as_secs_f64() * 1e3);
    let mut c = Canvas::new(400, 30);
    let adv = c.text(0.0, 0.0, 10.0, &long);
    assert!(adv > 0.0 && c.ink() > 0);
}

/// Ink and its bounding box (x0, y0, x1, y1), relative to the pen and the
/// line top, of `s` drawn alone at `px`.
fn ink_of(s: &str, px: f32) -> (u64, [i64; 4], Canvas) {
    let t = tr();
    let pad = 20.0;
    let w = (t.measure(s, px).ceil() + 2.0 * pad) as u32 + 2;
    let h = (t.line_height(px).ceil() + 2.0 * pad) as u32 + 2;
    let mut c = Canvas::new(w, h);
    c.text(pad, pad, px, s);
    let (mut x0, mut y0, mut x1, mut y1) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
    let mut sum = 0u64;
    for y in 0..c.h {
        for x in 0..c.w {
            let k = c.ink_at(x, y) as u64;
            if k > 0 {
                sum += k;
                if k > 64 {
                    let (x, y) = (x as i64 - pad as i64, y as i64 - pad as i64);
                    (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
                }
            }
        }
    }
    (sum, [x0, y0, x1, y1], c)
}

/// Glyphs larger than the cached-image cap (1024 px) still draw, with the
/// same shape as at a cached size: outlines are filled straight into the
/// visible window, colour glyphs are rendered at the cap and scaled.
#[test]
fn huge_glyphs_draw() {
    for (s, big, small) in [("H", 1200.0f32, 400.0f32), ("g", 1600.0, 400.0), ("漢", 1100.0, 275.0), ("H", 2000.0, 500.0)] {
        if !covered(s) {
            continue;
        }
        let k = big / small;
        let (ink_big, bb_big, c) = ink_of(s, big);
        let (ink_small, bb_small, _) = ink_of(s, small);
        let ratio = ink_big as f64 / (ink_small as f64 * (k * k) as f64);
        assert!((ratio - 1.0).abs() < 0.03, "{s} at {big} px: ink {ink_big}, expected about {} ({ratio:.3})", ink_small as f64 * (k * k) as f64);
        for i in 0..4 {
            let want = bb_small[i] as f32 * k;
            assert!((bb_big[i] as f32 - want).abs() <= k + 3.0, "{s} at {big} px: box {bb_big:?} vs {bb_small:?} x {k}");
        }
        if big == 2000.0 {
            c.save("huge-H");
        }
    }
    // only the visible window is filled, and it matches the full drawing
    let px = 2000.0;
    let (_, _, full) = ink_of("H", px);
    let mut part = Canvas::new(500, 500);
    let (dx, dy) = (320u32, 420u32);
    part.text(20.0 - dx as f32, 20.0 - dy as f32, px, "H");
    let mut worst = 0u32;
    for y in 0..part.h {
        for x in 0..part.w {
            worst = worst.max(part.ink_at(x, y).abs_diff(full.ink_at(x + dx, y + dy)));
        }
    }
    assert!(part.ink() > 1_000_000, "the stem crosses the window: ink {}", part.ink());
    assert!(worst <= 2, "window differs from the full drawing by {worst}");
    // a colour emoji over the cap: coloured, about the size of its advance
    let e = "😀";
    if covered(e) {
        for px in [1200.0f32, 2000.0] {
            let (_, bb, c) = ink_of(e, px);
            let yellow = c.px.as_chunks::<4>().0.iter().filter(|p| p[0] > 200 && p[1] > 140 && p[2] < 120).count();
            assert!(yellow as f32 > 0.25 * px * px, "{e} at {px} px: {yellow} yellow pixels");
            let w = (bb[2] - bb[0]) as f32;
            // Segoe UI Emoji's advance is about 1.4 em, its ink about 1 em
            assert!(w > 0.6 * tr().measure(e, px) && w < 1.3 * tr().measure(e, px), "{e} at {px} px is {w} wide");
            if px == 2000.0 {
                c.save("huge-emoji");
            }
        }
    }
}

/// Warm queries on a 1 MiB paragraph reuse its cached pieces: at least ten
/// times faster than the first (shaping) call.
#[test]
fn long_paragraph_warm_queries_are_fast() {
    // its own instance: other tests' entries must not evict the pieces
    let t = TextSystem::load().expect("fonts");
    let words = ["lorem", "ipsum", "dolor", "sit", "amet,", "consectetur", "adipiscing", "elit.", "AnyPage", "keeps", "every", "piece", "cached"];
    // numbered words: no two pieces of the paragraph have the same text
    let mut text = String::with_capacity(anypage_text::MAX_LINE_BYTES + 32);
    let mut i = 0;
    while text.len() < anypage_text::MAX_LINE_BYTES - 32 {
        text.push_str(words[i % words.len()]);
        text.push_str(&format!(" {i} "));
        i += 1;
    }
    let mid = text[..text.len() / 2].rfind(' ').expect("space") + 1;
    let time = |f: &dyn Fn()| {
        let t0 = Instant::now();
        f();
        t0.elapsed().as_secs_f64() * 1e3
    };
    t.clear_caches();
    let cold_measure = time(&|| {
        t.measure(&text, 16.0);
    });
    let warm_measure = time(&|| {
        t.measure(&text, 16.0);
    });
    t.clear_caches();
    let cold_caret = time(&|| {
        t.caret_x(&text, 16.0, mid);
    });
    let warm_caret = time(&|| {
        t.caret_x(&text, 16.0, mid + 6);
    });
    let warm_hit = time(&|| {
        t.hit_x(&text, 16.0, 1e6);
    });
    t.clear_caches();
    let cold_layout = time(&|| {
        t.layout_ranges(&text, 16.0, 600.0);
    });
    let warm_layout = time(&|| {
        t.layout_ranges(&text, 16.0, 600.0);
    });
    eprintln!(
        "1 MiB paragraph (ms): measure cold {cold_measure:.1} warm {warm_measure:.2}; caret_x cold {cold_caret:.1} warm {warm_caret:.2}; hit_x warm {warm_hit:.2}; layout_ranges cold {cold_layout:.1} warm {warm_layout:.1}; {:?}",
        t.stats()
    );
    assert!(warm_measure * 10.0 < cold_measure, "measure: warm {warm_measure} ms vs cold {cold_measure} ms");
    assert!(warm_caret * 10.0 < cold_caret, "caret_x: warm {warm_caret} ms vs cold {cold_caret} ms");
    assert!(warm_layout * 10.0 < cold_layout, "layout_ranges: warm {warm_layout} ms vs cold {cold_layout} ms");
    // and the answers are those of one line
    let w = t.measure(&text, 16.0);
    assert!((t.caret_x(&text, 16.0, text.len()) - w).abs() < 1.0);
    let x = t.caret_x(&text, 16.0, mid);
    assert!(x > 0.3 * w && x < 0.7 * w);
    assert_eq!(t.hit_x(&text, 16.0, x), mid);
    assert_eq!(t.hit_x(&text, 16.0, f32::INFINITY), text.len());
    let lines = t.layout_ranges(&text, 16.0, 600.0);
    assert!(lines.iter().all(|(_, w)| *w <= 600.0));
    assert_eq!(lines.first().map(|l| l.0.start), Some(0));
    assert_eq!(lines.last().map(|l| l.0.end), Some(text.len()));
    assert!(lines.windows(2).all(|p| p[0].0.end < p[1].0.start), "lines in order, a space between");
    // a sample of lines: widths are the measured widths of their text
    for (r, lw) in lines.iter().step_by(997) {
        assert!((t.measure(&text[r.clone()], 16.0) - lw).abs() < 0.5);
    }
}

/// Paragraphs longer than MAX_LINE_BYTES are laid out in segments: every
/// byte is still on some line.
#[test]
fn paragraphs_over_the_line_limit_are_laid_out_in_segments() {
    let t = tr();
    let text = "segment ".repeat(anypage_text::MAX_LINE_BYTES / 8 + 4096); // about 1.03 MiB
    let lines = t.layout_ranges(&text, 4.0, 4000.0);
    assert_eq!(lines.first().map(|l| l.0.start), Some(0));
    assert_eq!(lines.last().map(|l| l.0.end), Some(text.len()));
    assert!(lines.iter().all(|(_, w)| *w <= 4000.0));
    // single-line calls look at the first MAX_LINE_BYTES only
    let w = t.measure(&text, 4.0);
    assert!((w - t.measure(&text[..anypage_text::MAX_LINE_BYTES], 4.0)).abs() < 1e-3 * w);
}

#[test]
fn threads_share_one_text_system() {
    let t = Arc::new(TextSystem::load().expect("fonts"));
    let samples: Vec<String> = (0..64).map(|i| format!("thread line {i} 漢字 مرحبا {i}")).collect();
    let expected: Vec<f32> = samples.iter().map(|s| t.measure(s, 18.0)).collect();
    t.clear_caches();
    let handles: Vec<_> = (0..8)
        .map(|k| {
            let t = t.clone();
            let samples = samples.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 300 * 30 * 4];
                let mut out = Vec::new();
                for (i, s) in samples.iter().enumerate().skip(k % 4) {
                    t.draw_line(&mut buf, 300, 30, (0, 0, 300, 30), 0.0, 0.0, 18.0, [0, 0, 0, 255], s);
                    out.push((i, t.measure(s, 18.0)));
                }
                out
            })
        })
        .collect();
    for h in handles {
        for (i, w) in h.join().expect("thread") {
            assert!((w - expected[i]).abs() < 1e-3, "line {i}: {w} vs {}", expected[i]);
        }
    }
}

#[test]
fn bundled_fonts_only() {
    let candidates = [r"C:\Windows\Fonts\arial.ttf", "/system/fonts/Roboto-Regular.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", "/System/Library/Fonts/Supplemental/Arial.ttf"];
    let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
        eprintln!("skipped: no known single font file");
        return;
    };
    let t = TextSystem::from_fonts(vec![bytes.clone(), b"not a font".to_vec()]);
    assert!(t.describe().starts_with("1 font faces"), "{}", t.describe());
    assert!(t.measure("Hello", 20.0) > 20.0);
    // a Latin-only font: Han is missing but nothing panics
    assert!(t.missing_glyphs("漢字") >= 1);
    let mut buf = vec![0u8; 100 * 30 * 4];
    t.draw_line(&mut buf, 100, 30, (0, 0, 100, 30), 0.0, 0.0, 20.0, [0, 0, 0, 255], "Hi 漢字");
    assert!(buf.iter().any(|&b| b > 0));

    let empty = TextSystem::from_fonts(Vec::new());
    assert_eq!(empty.measure("abc", 20.0), 0.0);
    assert_eq!(empty.layout("a\nb", 20.0, 100.0), vec!["a", "b"]);
    assert_eq!(empty.draw_line(&mut buf, 100, 30, (0, 0, 100, 30), 0.0, 0.0, 20.0, [0, 0, 0, 255], "abc"), 0.0);
    assert!(TextSystem::load_with(LoadOptions { no_system_fonts: true, ..Default::default() }).is_err());
    let one = TextSystem::load_with(LoadOptions { no_system_fonts: true, extra_fonts: vec![bytes], ..Default::default() }).expect("bundled");
    assert!(one.measure("Hello", 20.0) > 20.0);
}

#[test]
fn timings_load_and_shape_10k_chars() {
    let t0 = Instant::now();
    let t = TextSystem::load().expect("fonts");
    let load = t0.elapsed();
    let samples = [
        "The quick brown fox jumps over the lazy dog. ",
        "敏捷的棕色狐狸跳過了懶狗。",
        "النص العربي يتصل ويكتب من اليمين. ",
        "טקסט עברי מימין לשמאל. ",
        "हिन्दी पाठ में संयुक्ताक्षर। ",
        "ภาษาไทยไม่มีการเว้นวรรค ",
        "Emoji 👍🏽 👨\u{200D}👩\u{200D}👧 ",
    ];
    let mut lines = Vec::new();
    let mut chars = 0;
    let mut i = 0;
    while chars < 10_000 {
        let s = format!("{i} {}", samples[i % samples.len()]);
        chars += s.chars().count();
        lines.push(s);
        i += 1;
    }
    let t0 = Instant::now();
    let w: f32 = lines.iter().map(|l| t.measure(l, 16.0)).sum();
    let shape = t0.elapsed();
    let t0 = Instant::now();
    let w2: f32 = lines.iter().map(|l| t.measure(l, 16.0)).sum();
    let cached = t0.elapsed();
    assert_eq!(w, w2);
    eprintln!(
        "load fonts {:.1} ms; shape {chars} mixed-script chars in {} lines: {:.1} ms cold ({:.2} us/char), {:.2} ms cached",
        load.as_secs_f64() * 1e3,
        lines.len(),
        shape.as_secs_f64() * 1e3,
        shape.as_secs_f64() * 1e6 / chars as f64,
        cached.as_secs_f64() * 1e3
    );
    // generous bounds: this is a debug-profile test on a shared machine
    assert!(load.as_secs() < 20 && shape.as_secs() < 20);
}
