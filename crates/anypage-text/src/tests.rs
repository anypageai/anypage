//! Unit tests that need the crate's internals. They use the system fonts
//! and skip when none are installed.

use super::*;

fn system() -> Option<TextSystem> {
    match TextSystem::load() {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

#[test]
fn primary_font_is_slot_zero_with_sane_metrics() {
    let Some(t) = system() else { return };
    let (ascent, lh) = t.line_metrics(100.0);
    assert!(ascent > 60.0 && ascent < 130.0, "ascent {ascent}");
    assert!(lh > 100.0 && lh < 180.0, "line height {lh}");
    assert!(lh > ascent);
    let first = t.fonts.read().unwrap_or_else(PoisonError::into_inner).len();
    assert!(first >= 1);
    // Latin from the primary font: every glyph of "Hello" is slot 0
    assert!(t.glyphs("Hello", 16.0).iter().all(|g| g.font == 0));
}

#[test]
fn coverage_probe_is_cached_and_finds_fallback_faces() {
    let Some(t) = system() else { return };
    let mut st = lock(&t.state);
    assert!(st.covered('A'));
    assert!(st.covered('\u{FFFD}'));
    // private use: no system font maps it (and the answer is remembered)
    assert!(!st.covered('\u{F0000}'));
    assert_eq!(st.coverage.get(&0xF0000), Some(&false));
    let n = st.coverage.len();
    assert!(!st.covered('\u{F0000}'));
    assert_eq!(st.coverage.len(), n);
}

#[test]
fn uncovered_characters_do_not_reach_the_fallback() {
    let Some(t) = system() else { return };
    let before = t.fonts.read().unwrap_or_else(PoisonError::into_inner).len();
    // control characters and unassigned code points in many distinct lines
    for i in 0..50 {
        t.measure(&format!("log {i}: \u{1B}[1m\u{F0001} done"), 14.0);
    }
    let after = t.fonts.read().unwrap_or_else(PoisonError::into_inner).len();
    // the replacement glyph may bring one extra font, not one per face
    assert!(after <= before + 2, "{before} -> {after} fonts in use");
    assert!(t.missing_glyphs("\u{F0001}") == 1);
}

#[test]
fn forced_direction_drops_the_mark_glyph() {
    let Some(t) = system() else { return };
    let s = "abc";
    let forced = t.shape_piece(s, Some(true));
    let plain = t.shape_piece(s, None);
    assert!(forced.rtl && !plain.rtl);
    assert_eq!(forced.glyphs.len(), plain.glyphs.len());
    assert!(forced.glyphs.iter().all(|g| g.end as usize <= s.len()));
    assert!((forced.width - plain.width).abs() < 1e-4);
}

#[test]
fn chunked_line_matches_pieces() {
    let Some(t) = system() else { return };
    let piece = "lorem ipsum dolor sit amet ".repeat(700); // ~19 KB, five pieces
    let line = t.line(&piece, Direction::Auto);
    let parts = shape::chunks(&piece, CHUNK_BYTES);
    assert_eq!(parts.len(), 5);
    assert_eq!(line.pieces().len(), parts.len());
    let sum: f32 = parts.iter().map(|r| t.line(&piece[r.clone()], Direction::Auto).width()).sum();
    assert!((line.width() - sum).abs() < 1e-2);
    // glyph clusters are offsets into the whole text, increasing left to right
    let glyphs = t.glyphs(&piece, 1.0);
    assert!(glyphs.windows(2).all(|g| g[0].cluster.start <= g[1].cluster.start && g[0].x <= g[1].x));
    assert_eq!(glyphs.last().map(|g| g.cluster.end), Some(piece.len()));
    // the pieces are the cached pieces: shaping the line again shapes nothing
    let again = t.line(&piece, Direction::Auto);
    assert!(again.pieces().iter().zip(line.pieces()).all(|(a, b)| Arc::ptr_eq(&a.shaped, &b.shaped)));
}

/// An edit inside a long line reshapes only the piece it falls in.
#[test]
fn editing_a_long_line_reuses_the_other_pieces() {
    let Some(t) = system() else { return };
    let text = "the quick brown fox jumps over the lazy dog ".repeat(600); // ~26 KB
    let before = t.line(&text, Direction::Auto);
    let mut edited = text.clone();
    edited.insert(10_000, 'X');
    let after = t.line(&edited, Direction::Auto);
    let reused = after.pieces().iter().filter(|p| before.pieces().iter().any(|q| Arc::ptr_eq(&p.shaped, &q.shaped))).count();
    // the pieces before the edit, and after it once the piece boundaries
    // fall on the same spaces again (usually at once)
    assert!(reused + 3 >= after.pieces().len() && after.pieces().len() >= 6, "{reused} of {} pieces reused", after.pieces().len());
    assert!((t.measure(&edited, 16.0) - t.measure(&text, 16.0) - t.measure("X", 16.0)).abs() < 1.0);
}

/// Glyph images: oversized outlines are drawn directly, oversized colour
/// glyphs are rendered smaller and scaled, nothing over the cap is cached.
#[test]
fn oversized_glyphs_are_not_cached_as_huge_images() {
    let Some(t) = system() else { return };
    let h = t.glyphs("H", 16.0)[0].glyph_id;
    // H is about 0.7 em tall: over 1024 px from about 1500 px
    for px in [1600.0f32, 2000.0] {
        let key = GlyphKey { font: 0, id: h, size_q: (px * 4.0) as u32, bin: 0, italic: false, fg: 0 };
        let b = t.render_uncached(key).expect("font");
        assert!(b.direct && b.data.is_empty(), "H at {px} px should be drawn directly");
    }
    // large but under the cap: still an ordinary cached image
    for px in [500.0f32, 1100.0] {
        let key = GlyphKey { font: 0, id: h, size_q: (px * 4.0) as u32, bin: 0, italic: false, fg: 0 };
        let b = t.render_uncached(key).expect("font");
        assert!(!b.direct && b.w > 100 && b.w <= raster::MAX_GLYPH_SIDE && b.h <= raster::MAX_GLYPH_SIDE, "H at {px} px: {}x{}", b.w, b.h);
    }
    // a colour emoji over the cap: painted at most 1024 px, scaled when drawn
    let e = t.glyphs("😀", 16.0);
    if t.missing_glyphs("😀") == 0 && !e.is_empty() {
        let key = GlyphKey { font: e[0].font, id: e[0].glyph_id, size_q: 2000 * 4, bin: 0, italic: false, fg: 0 };
        let b = t.render_uncached(key).expect("font");
        assert!(b.color && b.w <= raster::MAX_GLYPH_SIDE && b.h <= raster::MAX_GLYPH_SIDE, "{}x{}", b.w, b.h);
        assert!(b.scale > 1.5, "scale {}", b.scale);
        assert!((b.w as f32 * b.scale) > 1500.0);
    }
}

/// COLR v0 layers painted by this crate's painter look like swash's
/// rendering of the same glyph (same coverage, same colours).
#[test]
fn colr_v0_painter_matches_swash() {
    let Some(t) = system() else { return };
    let e = t.glyphs("😀", 16.0);
    let Some(g) = e.first() else { return };
    let Some((font, index)) = t.font(g.font) else { return };
    let key = GlyphKey { font: g.font, id: g.glyph_id, size_q: 96 * 4, bin: 0, italic: false, fg: 0 };
    let Some(ours) = colr::render_format(&font, index, key, false) else {
        eprintln!("skipped: the emoji font has no COLR v0 glyph for U+1F600");
        return;
    };
    // swash's COLR v0 rendering of the same glyph
    let img = SCALE_TEST.with(|ctx| {
        let mut ctx = ctx.borrow_mut();
        let mut scaler = ctx.builder(font.as_swash()).size(96.0).hint(false).build();
        swash::scale::Render::new(&[swash::scale::Source::ColorOutline(0)]).format(swash::zeno::Format::Alpha).render(&mut scaler, g.glyph_id)
    });
    let Some(img) = img else { return };
    let alpha_sum = |d: &[u8]| d.as_chunks::<4>().0.iter().map(|p| p[3] as u64).sum::<u64>();
    let (a, b) = (alpha_sum(&ours.data), alpha_sum(&img.data));
    assert!(a > 0 && (a as f64 / b as f64 - 1.0).abs() < 0.05, "coverage: ours {a}, swash {b}");
    let mean = |d: &[u8]| {
        let n = d.len() as u64 / 4;
        [0, 1, 2].map(|k| d.as_chunks::<4>().0.iter().map(|p| p[k] as u64).sum::<u64>() / n.max(1))
    };
    // compare premultiplied means over the same total area
    let (ma, mb) = (mean(&ours.data), mean(&img.data));
    let (na, nb) = (ours.data.len() as f64, img.data.len() as f64);
    for k in 0..3 {
        let (x, y) = (ma[k] as f64 * na, mb[k] as f64 * nb);
        assert!((x - y).abs() <= 0.08 * x.max(y) + 1000.0, "channel {k}: ours {x}, swash {y}");
    }
}

thread_local! {
    static SCALE_TEST: std::cell::RefCell<swash::scale::ScaleContext> = std::cell::RefCell::new(swash::scale::ScaleContext::new());
}
