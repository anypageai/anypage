//! Text for drawings: one system outline font, glyph outlines as paths in
//! font units (y up, like drawing space), so TEXT and MTEXT render as
//! vectors at any zoom and any rotation, and go into `to_paths` as fills.
//!
//! The engine's `TextRaster` is not used: tile code gets no handle to it and
//! it only draws axis-aligned bitmaps. When no font is found, characters are
//! drawn as boxes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use crate::geom::{p, Path, PathB};

pub struct Font {
    data: Vec<u8>,
    offset: u32,
    key: swash::CacheKey,
    pub units_per_em: f64,
    pub ascent: f64,
    pub descent: f64,
    pub cap_height: f64,
    pub source: String,
    /// Outlines shared by all tile workers: read-mostly, two generations
    /// (the older one is dropped when the newer fills: an approximate LRU
    /// without clearing everything at once).
    glyphs: RwLock<Glyphs>,
}

#[derive(Default)]
struct Glyphs {
    cur: HashMap<u16, Arc<Path>>,
    old: HashMap<u16, Arc<Path>>,
}

thread_local! {
    /// One scaler per thread, so tile workers outline glyphs in parallel.
    static SCALER: RefCell<swash::scale::ScaleContext> = RefCell::new(swash::scale::ScaleContext::new());
}

/// Candidate fonts with CJK coverage first (drawings from Taiwan, China,
/// Japan and Korea carry CJK text), Latin-only last.
const CANDIDATES: [&str; 14] = [
    r"C:\Windows\Fonts\msjh.ttc",
    r"C:\Windows\Fonts\msyh.ttc",
    r"C:\Windows\Fonts\simsun.ttc",
    "/system/fonts/NotoSansCJK-Regular.ttc",
    "/system/fonts/NotoSansSC-Regular.otf",
    "/system/fonts/NotoSansTC-Regular.otf",
    "/system/fonts/DroidSansFallback.ttf",
    "/System/Library/Fonts/PingFang.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    r"C:\Windows\Fonts\segoeui.ttf",
    r"C:\Windows\Fonts\arial.ttf",
    "/system/fonts/Roboto-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/System/Library/Fonts/Helvetica.ttc",
];

const MAX_FONT_BYTES: u64 = 64 << 20;
const MAX_CACHED_GLYPHS: usize = 20_000;

impl Font {
    pub fn from_bytes(data: Vec<u8>, source: &str) -> Option<Font> {
        let f = swash::FontRef::from_index(&data, 0)?;
        if f.charmap().map('A') == 0 {
            return None;
        }
        let m = f.metrics(&[]);
        if m.units_per_em == 0 {
            return None;
        }
        let (offset, key) = (f.offset, f.key);
        let upm = m.units_per_em as f64;
        let ascent = if m.ascent > 0.0 { m.ascent as f64 } else { upm * 0.8 };
        let cap = if m.cap_height > 0.0 { m.cap_height as f64 } else { ascent * 0.72 };
        Some(Font {
            units_per_em: upm,
            ascent,
            descent: (m.descent as f64).abs(),
            cap_height: cap,
            source: source.to_string(),
            data,
            offset,
            key,
            glyphs: RwLock::new(Glyphs::default()),
        })
    }

    fn font(&self) -> swash::FontRef<'_> {
        swash::FontRef { data: &self.data, offset: self.offset, key: self.key }
    }

    /// The character map and advance table, for many lookups.
    pub fn tables(&self) -> (swash::Charmap<'_>, swash::GlyphMetrics<'_>) {
        let f = self.font();
        (f.charmap(), f.glyph_metrics(&[]))
    }

    pub fn glyph_id(&self, ch: char) -> u16 {
        self.font().charmap().map(ch)
    }

    /// Advance in font units.
    pub fn advance(&self, gid: u16) -> f64 {
        self.font().glyph_metrics(&[]).advance_width(gid) as f64
    }

    /// The glyph outline in font units (y up), cached.
    pub fn outline(&self, gid: u16) -> Arc<Path> {
        {
            let g = self.glyphs.read().unwrap_or_else(|e| e.into_inner());
            if let Some(path) = g.cur.get(&gid) {
                return path.clone();
            }
            if let Some(path) = g.old.get(&gid).cloned() {
                drop(g);
                self.remember(gid, path.clone());
                return path;
            }
        }
        let path = Arc::new(SCALER.with(|s| match s.try_borrow_mut() {
            Ok(mut ctx) => self.scale(&mut ctx, gid),
            Err(_) => self.scale(&mut swash::scale::ScaleContext::new(), gid),
        }));
        self.remember(gid, path.clone());
        path
    }

    fn remember(&self, gid: u16, path: Arc<Path>) {
        let mut g = self.glyphs.write().unwrap_or_else(|e| e.into_inner());
        if g.cur.len() >= MAX_CACHED_GLYPHS / 2 {
            g.old = std::mem::take(&mut g.cur);
        }
        g.cur.insert(gid, path);
    }

    fn scale(&self, ctx: &mut swash::scale::ScaleContext, gid: u16) -> Path {
        let font = self.font();
        let mut scaler = ctx.builder(font).size(0.0).hint(false).build();
        let mut b = PathB::new();
        if let Some(outline) = scaler.scale_outline(gid) {
            let pts = outline.points();
            let mut i = 0usize;
            let pt = |k: usize| pts.get(k).map(|q| p(q.x as f64, q.y as f64));
            let mut pen = p(0.0, 0.0);
            for verb in outline.verbs() {
                match verb {
                    swash::zeno::Verb::MoveTo => {
                        if let Some(q) = pt(i) {
                            b.move_to(q);
                            pen = q;
                        }
                        i += 1;
                    }
                    swash::zeno::Verb::LineTo => {
                        if let Some(q) = pt(i) {
                            b.line_to(q);
                            pen = q;
                        }
                        i += 1;
                    }
                    swash::zeno::Verb::QuadTo => {
                        if let (Some(c), Some(q)) = (pt(i), pt(i + 1)) {
                            // exact quadratic to cubic
                            b.cubic_to(pen.lerp(c, 2.0 / 3.0), q.lerp(c, 2.0 / 3.0), q);
                            pen = q;
                        }
                        i += 2;
                    }
                    swash::zeno::Verb::CurveTo => {
                        if let (Some(c1), Some(c2), Some(q)) = (pt(i), pt(i + 1), pt(i + 2)) {
                            b.cubic_to(c1, c2, q);
                            pen = q;
                        }
                        i += 3;
                    }
                    swash::zeno::Verb::Close => b.close(),
                }
            }
        }
        b.finish()
    }
}

static FONT: OnceLock<Option<Arc<Font>>> = OnceLock::new();

/// The process-wide drawing font, loaded on first use; `None` when the
/// system has no usable font (text is then drawn as boxes).
pub fn global() -> Option<Arc<Font>> {
    FONT.get_or_init(|| {
        for path in CANDIDATES {
            let Ok(meta) = std::fs::metadata(path) else { continue };
            if meta.len() > MAX_FONT_BYTES {
                continue;
            }
            let Ok(bytes) = std::fs::read(path) else { continue };
            if let Some(f) = Font::from_bytes(bytes, path) {
                log::info!(target: "anypage_cad", "drawing font: {path}");
                return Some(Arc::new(f));
            }
        }
        log::warn!(target: "anypage_cad", "no outline font found; drawing text as boxes");
        None
    })
    .clone()
}

/// A box glyph (outer ring counter-clockwise, inner clockwise) in units of
/// the cap height, used when there is no font.
pub fn box_glyph(w: f64, h: f64) -> Path {
    let t = h * 0.08;
    let mut b = PathB::new();
    b.move_to(p(0.1 * w, 0.0));
    b.line_to(p(0.9 * w, 0.0));
    b.line_to(p(0.9 * w, h));
    b.line_to(p(0.1 * w, h));
    b.close();
    b.move_to(p(0.1 * w + t, t));
    b.line_to(p(0.1 * w + t, h - t));
    b.line_to(p(0.9 * w - t, h - t));
    b.line_to(p(0.9 * w - t, t));
    b.close();
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outlines_when_a_font_exists() {
        let Some(f) = global() else { return };
        let a = f.glyph_id('A');
        assert_ne!(a, 0);
        let o = f.outline(a);
        assert!(o.len() > 3);
        let b = crate::geom::path_bbox(&o);
        // 'A' sits on the baseline and is about cap height tall
        assert!(b.y0.abs() < f.units_per_em * 0.05, "{b:?}");
        assert!((b.y1 - f.cap_height).abs() < f.units_per_em * 0.1, "{b:?} cap {}", f.cap_height);
        assert!(f.advance(a) > 0.0);
    }

    #[test]
    fn outlines_in_parallel_with_a_bounded_cache() {
        let Some(f) = global() else { return };
        std::thread::scope(|s| {
            for t in 0..4u16 {
                let f = &f;
                s.spawn(move || {
                    for g in 0..3000u16 {
                        let _ = f.outline(g * 4 + t);
                    }
                });
            }
        });
        {
            let g = f.glyphs.read().unwrap();
            assert!(g.cur.len() <= MAX_CACHED_GLYPHS / 2 && g.old.len() <= MAX_CACHED_GLYPHS / 2);
        }
        // repeats come from the cache
        let a = f.glyph_id('A');
        assert!(Arc::ptr_eq(&f.outline(a), &f.outline(a)));
    }
}
