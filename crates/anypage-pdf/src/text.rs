//! Text with positions.
//!
//! hayro-interpret runs the page's content stream: text matrices and state
//! (`Tm`, `Td`, `TJ` adjustments, `Tc`, `Tw`, `Tz`, `Ts`, `TL`), font widths
//! (`/Widths`, CID `/W`, the standard 14 metrics), simple-font encodings
//! (Standard, WinAnsi, MacRoman, `/Differences`, glyph names through the Adobe
//! Glyph List), `ToUnicode` CMaps, CID fonts with predefined CMaps, form
//! XObjects, Type 3 fonts and annotation appearances. [`Collector`] receives
//! every positioned glyph and records its text and its box in page space.
//! Where a font gives no Unicode (no `ToUnicode`, symbolic encodings) the
//! embedded font's own `cmap` is read backwards (glyph id to character).
//!
//! Then a layout pass, entirely ours:
//! glyphs → lines (same baseline, small gaps) → words (spaces or gaps wider
//! than ~0.18 em) → blocks (lines stacked closely with overlapping extent) in
//! reading order (recursive XY cut on the widest whitespace gap) → tables
//! (rows of several cells whose columns line up).
//!
//! Every step is near-linear in the glyphs of the page, and the steps that
//! compare things with each other (blocks, reading order, tables) have work
//! budgets, so a crafted page of [`MAX_GLYPHS`] scattered glyphs costs about
//! as much as a dense real one. Long runs check a cancel callback every few
//! thousand glyphs; the jobs pass theirs.

use std::collections::HashMap;
use std::sync::Arc;

use hayro::hayro_interpret::font::{Glyph as HGlyph, GlyphRun, OutlineGlyph};
use hayro::hayro_interpret::hayro_cmap::BfString;
use hayro::hayro_interpret::{
    interpret_page, BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterCache, SoftMask,
};
use hayro::kurbo::{BezPath, Point, Rect as KRect};

use crate::doc::Snapshot;
use crate::util::R;

/// Glyphs collected per page at most (pathological files). A dense real page
/// has 5k to 50k; a crafted page at the limit takes about 0.6 s and 60 MB to
/// lay out (debug build), and is not kept in the text cache.
pub const MAX_GLYPHS: usize = 250_000;
/// The error of an extraction stopped by its cancel callback.
pub const CANCELLED: &str = "cancelled";
/// Glyphs (or lines) between two looks at the cancel callback.
const CHECK_EVERY: usize = 4096;
/// Glyph box in em units of glyph space (1000 per em): descent and ascent.
const DESCENT: f64 = -200.0;
const ASCENT: f64 = 800.0;

/// `[x, y, w, h]` in page points, origin top-left.
pub type Rect4 = [f32; 4];

#[derive(Clone, Debug)]
pub struct Glyph {
    pub text: String,
    /// x0, y0, x1, y1 in page space.
    pub bbox: [f32; 4],
    pub origin: (f32, f32),
    /// Unit vector along the baseline.
    pub dir: (f32, f32),
    /// Advance along the baseline, points.
    pub adv: f32,
    /// Em size, points.
    pub size: f32,
    pub invisible: bool,
}

impl Glyph {
    fn is_space(&self) -> bool {
        self.text.chars().all(char::is_whitespace)
    }
}

#[derive(Clone, Debug)]
pub struct Word {
    pub text: String,
    pub rect: Rect4,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub words: Vec<Word>,
    pub rect: Rect4,
    pub size: f32,
    pub dir: (f32, f32),
    /// y of the baseline (horizontal lines), for row grouping.
    pub baseline: f32,
    /// Glyphs of the line in order, with whitespace; for search.
    glyphs: Vec<usize>,
}

impl Line {
    pub fn text(&self) -> String {
        self.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
    }
    fn horizontal(&self) -> bool {
        self.dir.0 > 0.999
    }
}

#[derive(Clone, Debug)]
pub struct Block {
    /// Indices into `PageText::lines`, top to bottom.
    pub lines: Vec<usize>,
    pub rect: Rect4,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Empty,
    Num(f64),
    Str(String),
}

#[derive(Clone, Debug)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    pub rect: Rect4,
}

/// Everything known about one page's text.
#[derive(Debug, Default)]
pub struct PageText {
    pub width: f32,
    pub height: f32,
    pub glyphs: Vec<Glyph>,
    pub lines: Vec<Line>,
    /// In reading order.
    pub blocks: Vec<Block>,
    pub tables: Vec<Table>,
    /// Search index: characters in reading order, the glyph each came from
    /// (`None` for separators we inserted) and the line it is on.
    chars: Vec<char>,
    char_glyph: Vec<Option<u32>>,
    char_line: Vec<u32>,
}

// ---------------------------------------------------------------- collecting glyphs

/// Receives glyphs from hayro-interpret.
pub struct Collector<'c> {
    pub glyphs: Vec<Glyph>,
    reverse: HashMap<u128, Option<Arc<HashMap<u32, char>>>>,
    /// (text hash, origin in quarter points) of glyphs already drawn.
    exact: std::collections::HashSet<(u64, i32, i32)>,
    cancel: &'c dyn Fn() -> bool,
    seen: usize,
    /// Set once `cancel` said so: every later glyph is ignored (the
    /// interpreter itself cannot be stopped from a device).
    pub stopped: bool,
}

fn ligature(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{FB00}' => "ff",
        '\u{FB01}' => "fi",
        '\u{FB02}' => "fl",
        '\u{FB03}' => "ffi",
        '\u{FB04}' => "ffl",
        '\u{FB05}' | '\u{FB06}' => "st",
        _ => return None,
    })
}

/// Text as people search for it: ligatures expanded, controls dropped,
/// no-break spaces as spaces.
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{3000}' => out.push(' '),
            '\u{00AD}' | '\u{200B}'..='\u{200D}' | '\u{FEFF}' => {}
            c if c.is_control() => {}
            c => match ligature(c) {
                Some(l) => out.push_str(l),
                None => out.push(c),
            },
        }
    }
    out
}

fn private_use(c: char) -> bool {
    matches!(c as u32, 0xE000..=0xF8FF | 0xF0000..=0x10FFFF)
}

fn never() -> bool {
    false
}

impl Default for Collector<'static> {
    fn default() -> Self {
        Self::new(&never)
    }
}

impl<'c> Collector<'c> {
    pub fn new(cancel: &'c dyn Fn() -> bool) -> Self {
        Self { glyphs: Vec::new(), reverse: HashMap::new(), exact: Default::default(), cancel, seen: 0, stopped: false }
    }

    /// The character an embedded font's own cmap gives a glyph id.
    fn reverse_cmap(&mut self, o: &OutlineGlyph) -> Option<char> {
        let map = self.reverse.entry(o.font_cache_key()).or_insert_with(|| {
            let data = o.font_data()?;
            let bytes: &[u8] = (*data.data).as_ref();
            let font = skrifa::FontRef::from_index(bytes, 0).ok()?;
            use skrifa::MetadataProvider;
            let mut m = HashMap::new();
            for (cp, gid) in font.charmap().mappings() {
                if let Some(c) = char::from_u32(cp).filter(|c| !private_use(*c) && !c.is_control()) {
                    m.entry(gid.to_u32()).or_insert(c);
                }
            }
            (!m.is_empty()).then(|| Arc::new(m))
        });
        map.as_ref()?.get(&o.glyph_id().to_u32()).copied()
    }

    fn text_of(&mut self, g: &HGlyph<'_>) -> String {
        let raw = match g.as_unicode() {
            Some(BfString::Char(c)) if c != '\0' && !private_use(c) => Some(c.to_string()),
            Some(BfString::String(s)) if !s.is_empty() && !s.chars().all(|c| c == '\0') => Some(s),
            _ => None,
        };
        let raw = raw.or_else(|| match g {
            HGlyph::Outline(o) => self.reverse_cmap(o).map(String::from),
            HGlyph::Type3(_) => None,
        });
        match raw {
            Some(s) => clean(&s),
            None => "\u{FFFD}".into(),
        }
    }

    fn duplicate(&mut self, g: &Glyph) -> bool {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        g.text.hash(&mut h);
        let key = (h.finish(), (g.origin.0 * 4.0).round() as i32, (g.origin.1 * 4.0).round() as i32);
        if !self.exact.insert(key) {
            return true;
        }
        // fake bold: the same glyph again a hair away
        let tol = 0.12 * g.size;
        self.glyphs.iter().rev().take(8).any(|p| p.text == g.text && (p.origin.0 - g.origin.0).abs() < tol && (p.origin.1 - g.origin.1).abs() < tol)
    }
}

impl<'a> Device<'a> for Collector<'_> {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}

    fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, props: DrawProps<'a>, mode: &DrawMode) {
        if self.stopped {
            return;
        }
        let glyphs = run.glyphs();
        for (k, pg) in glyphs.iter().enumerate() {
            if self.glyphs.len() >= MAX_GLYPHS {
                return;
            }
            self.seen += 1;
            if self.seen.is_multiple_of(CHECK_EVERY) && (self.cancel)() {
                self.stopped = true;
                return;
            }
            let m = props.transform * pg.transform();
            if m.determinant().abs() < 1e-12 || !m.as_coeffs().iter().all(|c| c.is_finite()) {
                continue;
            }
            let adv = match &**pg {
                HGlyph::Outline(o) => o.advance_width().map(f64::from).filter(|a| a.is_finite() && *a > 0.0),
                HGlyph::Type3(_) => None,
            };
            let adv = adv.unwrap_or_else(|| {
                // the next glyph's origin, in this glyph's space
                glyphs
                    .get(k + 1)
                    .map(|n| (m.inverse() * ((props.transform * n.transform()) * Point::ORIGIN)).x)
                    .filter(|x| x.is_finite() && *x > 0.0 && *x < 4000.0)
                    .unwrap_or(500.0)
            });
            let o = m * Point::ORIGIN;
            let e = m * Point::new(adv, 0.0);
            let up = m * Point::new(0.0, 1000.0);
            let size = ((up.x - o.x).powi(2) + (up.y - o.y).powi(2)).sqrt();
            let x1000 = m * Point::new(1000.0, 0.0);
            let (dx, dy) = (x1000.x - o.x, x1000.y - o.y);
            let len = (dx * dx + dy * dy).sqrt();
            if size < 0.01 || len < 1e-9 {
                continue;
            }
            let corners = [m * Point::new(0.0, DESCENT), m * Point::new(adv, DESCENT), m * Point::new(adv, ASCENT), m * Point::new(0.0, ASCENT)];
            let b = crate::geom::bbox(&corners);
            let text = self.text_of(pg);
            if text.is_empty() {
                continue;
            }
            let g = Glyph {
                text,
                bbox: [b.x0 as f32, b.y0 as f32, b.x1 as f32, b.y1 as f32],
                origin: (o.x as f32, o.y as f32),
                dir: ((dx / len) as f32, (dy / len) as f32),
                adv: ((e.x - o.x).powi(2) + (e.y - o.y).powi(2)).sqrt() as f32,
                size: size as f32,
                invisible: matches!(mode, DrawMode::Invisible),
            };
            if !self.duplicate(&g) {
                self.glyphs.push(g);
            }
        }
    }
}

/// Run a page through the interpreter into a collector.
pub fn collect(snap: &Snapshot, i: usize) -> R<Vec<Glyph>> {
    collect_with(snap, i, &InterpreterCache::new(), &never)
}

/// As [`collect`], reusing parsed fonts and objects across pages; stops with
/// [`CANCELLED`] when `cancel` says so.
pub fn collect_with<'a>(snap: &'a Snapshot, i: usize, cache: &InterpreterCache<'a>, cancel: &dyn Fn() -> bool) -> R<Vec<Glyph>> {
    let page = snap.page(i)?;
    let geom = snap.geoms[i];
    let (w, h) = geom.size();
    crate::guard("text extraction", || {
        let mut ctx = Context::new(geom.user_to_page(), KRect::new(0.0, 0.0, w, h), cache, page.xref(), crate::render::interpreter_settings());
        let mut dev = Collector::new(cancel);
        interpret_page(page, &mut ctx, &mut dev);
        if dev.stopped {
            return Err(CANCELLED.into());
        }
        Ok(dev.glyphs)
    })
}

/// The text of page `i` of a snapshot, laid out.
pub fn extract(snap: &Snapshot, i: usize) -> R<PageText> {
    extract_with(snap, i, &InterpreterCache::new(), &never)
}

pub fn extract_with<'a>(snap: &'a Snapshot, i: usize, cache: &InterpreterCache<'a>, cancel: &dyn Fn() -> bool) -> R<PageText> {
    let glyphs = collect_with(snap, i, cache, cancel)?;
    let (w, h) = snap.geoms[i].size();
    layout_with(glyphs, w as f32, h as f32, cancel)
}

// ---------------------------------------------------------------- layout

fn union(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

fn to_rect4(b: [f32; 4]) -> Rect4 {
    [b[0], b[1], b[2] - b[0], b[3] - b[1]]
}

struct Seg {
    idx: Vec<usize>,
    dir: (f32, f32),
    origin: (f32, f32),
    size: f32,
    /// Along-baseline end of the last glyph, relative to `origin`.
    end: f32,
}

impl Seg {
    fn along(&self, p: (f32, f32)) -> f32 {
        (p.0 - self.origin.0) * self.dir.0 + (p.1 - self.origin.1) * self.dir.1
    }
    fn perp(&self, p: (f32, f32)) -> f32 {
        (p.0 - self.origin.0) * self.dir.1 - (p.1 - self.origin.1) * self.dir.0
    }
    fn accepts(&self, g: &Glyph) -> bool {
        let s = self.size.max(g.size);
        let same_dir = self.dir.0 * g.dir.0 + self.dir.1 * g.dir.1 > 0.995;
        let a = self.along(g.origin);
        same_dir && self.perp(g.origin).abs() < 0.45 * s && a >= self.end - 0.6 * s && a <= self.end + 1.6 * s
    }
    fn push(&mut self, i: usize, g: &Glyph) {
        let end = self.along(g.origin) + g.adv;
        self.end = self.end.max(end);
        self.size = self.size.max(g.size);
        self.idx.push(i);
    }
}

fn median(mut v: Vec<f32>) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

/// Glyphs to lines, words, blocks, tables and the search index.
pub fn layout(glyphs: Vec<Glyph>, w: f32, h: f32) -> PageText {
    layout_with(glyphs, w, h, &never).unwrap_or_default()
}

/// [`layout`] that stops with [`CANCELLED`] when `cancel` says so.
pub fn layout_with(mut glyphs: Vec<Glyph>, w: f32, h: f32, cancel: &dyn Fn() -> bool) -> R<PageText> {
    let check = |n: usize| if n.is_multiple_of(CHECK_EVERY) && cancel() { Err(CANCELLED.to_string()) } else { Ok(()) };
    // text outside the visible page is not on the page
    glyphs.retain(|g| {
        let (cx, cy) = ((g.bbox[0] + g.bbox[2]) * 0.5, (g.bbox[1] + g.bbox[3]) * 0.5);
        cx >= -1.0 && cy >= -1.0 && cx <= w + 1.0 && cy <= h + 1.0
    });

    // 1. lines in stream order
    let mut segs: Vec<Seg> = Vec::new();
    for (i, g) in glyphs.iter().enumerate() {
        check(i + 1)?;
        match segs.last_mut() {
            Some(s) if s.accepts(g) => s.push(i, g),
            _ => segs.push(Seg { idx: vec![i], dir: g.dir, origin: g.origin, size: g.size, end: g.adv }),
        }
    }
    // a line made of nothing but spaces is not a line
    segs.retain(|s| s.idx.iter().any(|&i| !glyphs[i].is_space()));

    // 2. join horizontal pieces of one baseline written out of order
    let (mut horiz, other): (Vec<Seg>, Vec<Seg>) = segs.into_iter().partition(|s| s.dir.0 > 0.999);
    horiz.sort_by(|a, b| a.origin.1.total_cmp(&b.origin.1).then(a.origin.0.total_cmp(&b.origin.0)));
    let mut merged: Vec<Seg> = Vec::new();
    let mut horiz = horiz.into_iter().peekable();
    let mut groups = 0;
    while let Some(first) = horiz.next() {
        groups += 1;
        check(groups)?;
        // one baseline group: the pieces within a quarter em of its first one
        let (y0, s0) = (first.origin.1, first.size);
        let mut group = vec![first];
        while let Some(s) = horiz.next_if(|s| (s.origin.1 - y0).abs() < 0.25 * s0.max(s.size)) {
            group.push(s);
        }
        group.sort_by(|a, b| a.origin.0.total_cmp(&b.origin.0));
        let mut cur: Option<Seg> = None;
        for s in group {
            cur = Some(match cur.take() {
                Some(mut c) => {
                    let gap = c.along(s.origin);
                    let size = c.size.max(s.size);
                    if gap >= c.end - 0.6 * size && gap <= c.end + 1.6 * size {
                        let shift = gap;
                        c.end = c.end.max(shift + s.end);
                        c.size = size;
                        c.idx.extend(s.idx);
                        c
                    } else {
                        merged.push(c);
                        s
                    }
                }
                None => s,
            });
        }
        merged.extend(cur);
    }
    merged.extend(other);

    // 3. words
    let mut lines: Vec<Line> = Vec::with_capacity(merged.len());
    for (n, s) in merged.iter().enumerate() {
        check(n + 1)?;
        let gaps: Vec<f32> = s
            .idx
            .windows(2)
            .map(|p| s.along(glyphs[p[1]].origin) - (s.along(glyphs[p[0]].origin) + glyphs[p[0]].adv))
            .collect();
        // letter spacing (Tc) shows as the typical gap; word gaps are wider than that
        let letter = median(gaps.clone()).clamp(0.0, 0.6 * s.size);
        let mut words: Vec<Word> = Vec::new();
        let mut cur: Option<(String, [f32; 4])> = None;
        for (n, &gi) in s.idx.iter().enumerate() {
            let g = &glyphs[gi];
            let thr = (0.18 * g.size).max(letter + 0.12 * g.size);
            let brk = n > 0 && gaps[n - 1] > thr;
            if g.is_space() || brk {
                if let Some((t, b)) = cur.take() {
                    words.push(Word { text: t, rect: to_rect4(b) });
                }
                if g.is_space() {
                    continue;
                }
            }
            cur = Some(match cur.take() {
                Some((mut t, b)) => {
                    t.push_str(&g.text);
                    (t, union(b, g.bbox))
                }
                None => (g.text.clone(), g.bbox),
            });
        }
        if let Some((t, b)) = cur {
            words.push(Word { text: t, rect: to_rect4(b) });
        }
        if words.is_empty() {
            continue;
        }
        let b = s.idx.iter().filter(|&&i| !glyphs[i].is_space()).map(|&i| glyphs[i].bbox).reduce(union).unwrap_or([0.0; 4]);
        lines.push(Line { words, rect: to_rect4(b), size: s.size, dir: s.dir, baseline: s.origin.1, glyphs: s.idx.clone() });
    }

    // 4. blocks
    let typical = median(lines.iter().map(|l| l.size).collect()).max(1.0);
    let blocks = group_blocks(&lines, typical, cancel)?;
    let mut ordered = Vec::with_capacity(blocks.len());
    let rects: Vec<Rect4> = blocks.iter().map(|b| b.rect).collect();
    // reading order may compare every block with every other block a few
    // times over; past this budget the rest is simply read top to bottom
    let mut budget = 16 * blocks.len() + 200_000;
    xy_cut((0..blocks.len()).collect(), &rects, typical, 0, &mut budget, &mut ordered);
    let mut slots: Vec<Option<Block>> = blocks.into_iter().map(Some).collect();
    let blocks: Vec<Block> = ordered.into_iter().filter_map(|i| slots.get_mut(i).and_then(Option::take)).collect();
    check(CHECK_EVERY)?;

    // 5. tables
    let tables = find_tables(&lines, w);
    check(CHECK_EVERY)?;

    // 6. search index
    let mut pt = PageText { width: w, height: h, glyphs, lines, blocks, tables, ..Default::default() };
    build_index(&mut pt);
    Ok(pt)
}

/// Lines into blocks. Lines are taken top to bottom; a horizontal line joins
/// the block whose last line ends just above it (a gap of -0.3 to 0.9 em, sizes
/// within 1.4x) and whose extent overlaps it, the closest such block.
///
/// Only blocks that can still take a line are candidates: a block expires
/// once the lines reach 1.27 of its em below its last line (no later line
/// could join it), and the live ones are indexed by x in up to 128 buckets,
/// so a line looks only at the live blocks above it. That keeps the pass about
/// linear even for pages of thousands of scattered labels; a work budget
/// bounds what is left (lines past it start blocks of their own).
fn group_blocks(lines: &[Line], typical: f32, cancel: &dyn Fn() -> bool) -> R<Vec<Block>> {
    let mut order: Vec<usize> = (0..lines.len()).collect();
    order.sort_by(|&a, &b| lines[a].rect[1].total_cmp(&lines[b].rect[1]).then(lines[a].rect[0].total_cmp(&lines[b].rect[0])));
    let mut blocks: Vec<Block> = Vec::new();
    if lines.is_empty() {
        return Ok(blocks);
    }
    let x0 = lines.iter().map(|l| l.rect[0]).fold(f32::INFINITY, f32::min);
    let x1 = lines.iter().map(|l| l.rect[0] + l.rect[2]).fold(f32::NEG_INFINITY, f32::max);
    let bw = ((x1 - x0) / 128.0).max(2.0 * typical).max(0.01);
    let nb = (((x1 - x0) / bw).ceil() as usize + 1).clamp(1, 130);
    let bucket = |x: f32| (((x - x0) / bw).floor().max(0.0) as usize).min(nb - 1);
    let mut buckets: Vec<Vec<u32>> = vec![Vec::new(); nb];
    // per block: em size (its first line's), last line, expiry, bucket range
    let mut size: Vec<f32> = Vec::new();
    let mut last: Vec<usize> = Vec::new();
    let mut expiry: Vec<f32> = Vec::new();
    let mut span: Vec<(usize, usize)> = Vec::new();
    let mut stamp: Vec<usize> = Vec::new();
    let budget = 4_000_000 + 64 * lines.len();
    let mut work = 0usize;
    for (n, &li) in order.iter().enumerate() {
        if (n + 1).is_multiple_of(CHECK_EVERY) && cancel() {
            return Err(CANCELLED.into());
        }
        let l = &lines[li];
        let lb = [l.rect[0], l.rect[1], l.rect[0] + l.rect[2], l.rect[1] + l.rect[3]];
        let top = l.rect[1];
        let mut best: Option<(usize, f32)> = None;
        if l.horizontal() && work < budget {
            let (b0, b1) = (bucket(lb[0]), bucket(lb[2]));
            for list in buckets.iter_mut().take(b1 + 1).skip(b0) {
                work += list.len();
                list.retain(|&bi| expiry[bi as usize] >= top);
                for &bi in list.iter() {
                    let bi = bi as usize;
                    if stamp[bi] == n + 1 {
                        continue;
                    }
                    stamp[bi] = n + 1;
                    let prev = &lines[last[bi]];
                    let s = l.size.max(size[bi]);
                    let ratio = s / l.size.min(size[bi]).max(0.01);
                    let gap = top - (prev.rect[1] + prev.rect[3]);
                    let r = blocks[bi].rect;
                    let overlap = lb[2].min(r[0] + r[2]) - lb[0].max(r[0]);
                    let closer = best.is_none_or(|(bb, g)| gap < g || (gap == g && bi < bb));
                    if ratio <= 1.4 && gap >= -0.3 * s && gap <= 0.9 * s && overlap > 0.0 && closer {
                        best = Some((bi, gap));
                    }
                }
            }
        }
        let bi = match best {
            Some((bi, _)) => {
                let b = &mut blocks[bi];
                b.lines.push(li);
                let r = b.rect;
                b.rect = to_rect4(union([r[0], r[1], r[0] + r[2], r[1] + r[3]], lb));
                last[bi] = li;
                bi
            }
            None => {
                blocks.push(Block { lines: vec![li], rect: l.rect });
                size.push(l.size);
                last.push(li);
                expiry.push(f32::NEG_INFINITY);
                span.push((1, 0));
                stamp.push(0);
                blocks.len() - 1
            }
        };
        // only blocks ending in a horizontal line take more lines
        if l.horizontal() && work < budget {
            // a line can join while its gap is at most 0.9 of the larger em,
            // and that em is at most 1.4 of the block's: 1.26 em
            expiry[bi] = lb[3] + 1.27 * size[bi];
            let r = blocks[bi].rect;
            let (b0, b1) = (bucket(r[0]), bucket(r[0] + r[2]));
            let (o0, o1) = span[bi];
            for (b, list) in buckets.iter_mut().enumerate().take(b1 + 1).skip(b0) {
                if b < o0 || b > o1 {
                    list.push(bi as u32);
                }
            }
            span[bi] = (b0, b1);
        }
    }
    if work >= budget {
        log::debug!(target: "anypage_pdf", "block grouping budget used up ({} lines): later lines start their own blocks", lines.len());
    }
    Ok(blocks)
}

/// Reading order: split the region at its widest whitespace gap, columns
/// left to right or bands top to bottom, recursively. `budget` counts the
/// blocks sorted so far; once it is spent, regions are read top to bottom.
fn xy_cut(ids: Vec<usize>, r: &[Rect4], typical: f32, depth: u32, budget: &mut usize, out: &mut Vec<usize>) {
    let by_pos = |mut v: Vec<usize>| {
        v.sort_by(|&a, &b| r[a][1].total_cmp(&r[b][1]).then(r[a][0].total_cmp(&r[b][0])));
        v
    };
    if ids.len() <= 1 || depth > 64 || *budget < ids.len() {
        out.extend(by_pos(ids));
        return;
    }
    *budget -= ids.len();
    // groups separated by a gap along one axis; returns (groups, widest gap)
    let split = |axis: usize| -> (Vec<Vec<usize>>, f32) {
        let mut v = ids.clone();
        v.sort_by(|&a, &b| r[a][axis].total_cmp(&r[b][axis]));
        let mut groups = vec![vec![v[0]]];
        let mut reach = r[v[0]][axis] + r[v[0]][axis + 2];
        let mut widest = 0.0f32;
        for &i in &v[1..] {
            let gap = r[i][axis] - reach;
            if gap > 0.0 {
                widest = widest.max(gap);
                groups.push(vec![i]);
            } else if let Some(g) = groups.last_mut() {
                g.push(i);
            }
            reach = reach.max(r[i][axis] + r[i][axis + 2]);
        }
        (groups, widest)
    };
    let (cols, vgap) = split(0);
    let (rows, hgap) = split(1);
    let use_cols = cols.len() > 1 && vgap >= 0.5 * typical && (rows.len() < 2 || vgap >= hgap);
    let groups = if use_cols {
        cols
    } else if rows.len() > 1 {
        rows
    } else {
        out.extend(by_pos(ids));
        return;
    };
    for g in groups {
        xy_cut(g, r, typical, depth + 1, budget, out);
    }
}

/// "1,234.50", "$12", "(7)" as numbers; dates, codes and percentages stay text.
pub fn parse_number(s: &str) -> Option<f64> {
    let t = s.trim();
    let (neg, t) = match t.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        Some(inner) => (true, inner.trim()),
        None => (false, t),
    };
    let t = t.trim_start_matches(['$', '\u{20AC}', '\u{00A3}', '\u{00A5}', '\u{FFE5}']).trim();
    if t.is_empty() || !t.chars().any(|c| c.is_ascii_digit()) || !t.chars().all(|c| c.is_ascii_digit() || matches!(c, ',' | '.' | '-' | '+' | 'e' | 'E')) {
        return None;
    }
    // thousands separators only in groups of three
    let int_part = t.split(['.', 'e', 'E']).next().unwrap_or("");
    if int_part.contains(',') {
        let groups: Vec<&str> = int_part.trim_start_matches(['-', '+']).split(',').collect();
        if groups[0].is_empty() || groups[0].len() > 3 || groups[1..].iter().any(|g| g.len() != 3) {
            return None;
        }
    }
    let v: f64 = t.replace(',', "").parse().ok()?;
    v.is_finite().then_some(if neg { -v } else { v })
}

fn cell_of(s: &str) -> Cell {
    let t = s.trim();
    if t.is_empty() {
        Cell::Empty
    } else if let Some(n) = parse_number(t) {
        Cell::Num(n)
    } else {
        Cell::Str(t.to_string())
    }
}

pub fn column_name(mut i: usize) -> String {
    let mut s = Vec::new();
    loop {
        s.push((b'A' + (i % 26) as u8) as char);
        if i < 26 {
            break;
        }
        i = i / 26 - 1;
    }
    s.iter().rev().collect()
}

/// Columns a table may have.
const MAX_TABLE_COLS: usize = 64;

/// Rows of two or more cells on one baseline whose columns line up, two rows
/// or more, cells short (not two columns of running text).
fn find_tables(lines: &[Line], page_w: f32) -> Vec<Table> {
    // rows: horizontal lines grouped by baseline
    let mut idx: Vec<usize> = (0..lines.len()).filter(|&i| lines[i].horizontal()).collect();
    idx.sort_by(|&a, &b| lines[a].baseline.total_cmp(&lines[b].baseline));
    let mut rows: Vec<Vec<usize>> = Vec::new();
    for i in idx {
        match rows.last_mut() {
            Some(r) if (lines[r[0]].baseline - lines[i].baseline).abs() < 0.3 * lines[r[0]].size.max(lines[i].size) => r.push(i),
            _ => rows.push(vec![i]),
        }
    }
    for r in &mut rows {
        r.sort_by(|&a, &b| lines[a].rect[0].total_cmp(&lines[b].rect[0]));
    }
    let span = |i: usize| (lines[i].rect[0], lines[i].rect[0] + lines[i].rect[2]);
    let mut tables = Vec::new();
    // current candidate: column extents and rows of (column, line)
    let mut cols: Vec<(f32, f32)> = Vec::new();
    let mut trows: Vec<Vec<(usize, usize)>> = Vec::new();
    let mut last_bottom = 0.0f32;
    let mut flush = |cols: &mut Vec<(f32, f32)>, trows: &mut Vec<Vec<(usize, usize)>>| {
        if cols.len() >= 2 && trows.len() >= 2 {
            if let Some(t) = make_table(lines, cols, trows, page_w) {
                tables.push(t);
            }
        }
        cols.clear();
        trows.clear();
    };
    for r in rows {
        let size = r.iter().map(|&i| lines[i].size).fold(0.0f32, f32::max);
        let top = r.iter().map(|&i| lines[i].rect[1]).fold(f32::INFINITY, f32::min);
        let bottom = r.iter().map(|&i| lines[i].rect[1] + lines[i].rect[3]).fold(0.0f32, f32::max);
        // cells are matched against columns, so both stay bounded: a "row" of
        // hundreds of pieces is scattered text, not a table
        if r.len() < 2 || r.len() > MAX_TABLE_COLS || cols.len() > MAX_TABLE_COLS {
            flush(&mut cols, &mut trows);
            continue;
        }
        if !trows.is_empty() && top - last_bottom > 2.0 * size {
            flush(&mut cols, &mut trows);
        }
        if trows.is_empty() {
            cols = r.iter().map(|&i| span(i)).collect();
            trows.push(r.iter().enumerate().map(|(c, &i)| (c, i)).collect());
            last_bottom = bottom;
            continue;
        }
        // map cells onto the columns
        let mut mapped = Vec::new();
        let mut matched = 0;
        let mut new_cols = Vec::new();
        for &i in &r {
            let (x0, x1) = span(i);
            let hit = cols.iter().position(|&(a, b)| x1.min(b) - x0.max(a) > 0.0);
            match hit {
                Some(c) => {
                    matched += 1;
                    mapped.push((c, i));
                }
                None => new_cols.push(i),
            }
        }
        let distinct = {
            let mut c: Vec<usize> = mapped.iter().map(|m| m.0).collect();
            c.dedup();
            c.len()
        };
        if matched >= 2 && distinct >= 2 {
            for (c, i) in &mapped {
                let (x0, x1) = span(*i);
                cols[*c] = (cols[*c].0.min(x0), cols[*c].1.max(x1));
            }
            for i in new_cols {
                cols.push(span(i));
                mapped.push((cols.len() - 1, i));
            }
            trows.push(mapped);
            last_bottom = bottom;
        } else {
            flush(&mut cols, &mut trows);
            cols = r.iter().map(|&i| span(i)).collect();
            trows.push(r.iter().enumerate().map(|(c, &i)| (c, i)).collect());
            last_bottom = bottom;
        }
    }
    flush(&mut cols, &mut trows);
    tables
}

fn make_table(lines: &[Line], cols: &[(f32, f32)], trows: &[Vec<(usize, usize)>], page_w: f32) -> Option<Table> {
    // cells are short; columns of running text are not
    let words: Vec<f32> = trows.iter().flatten().map(|&(_, i)| lines[i].words.len() as f32).collect();
    let widths: Vec<f32> = trows.iter().flatten().map(|&(_, i)| lines[i].rect[2]).collect();
    if median(words) > 5.0 || median(widths) > 0.3 * page_w {
        return None;
    }
    let width = cols.iter().map(|c| c.1).fold(0.0f32, f32::max) - cols.iter().map(|c| c.0).fold(f32::INFINITY, f32::min);
    if width > page_w * 1.05 {
        return None;
    }
    // columns left to right
    let mut order: Vec<usize> = (0..cols.len()).collect();
    order.sort_by(|&a, &b| cols[a].0.total_cmp(&cols[b].0));
    let pos: HashMap<usize, usize> = order.iter().enumerate().map(|(p, &c)| (c, p)).collect();
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut bounds: Option<[f32; 4]> = None;
    for tr in trows {
        let mut texts = vec![String::new(); cols.len()];
        for &(c, i) in tr {
            let p = pos[&c];
            if !texts[p].is_empty() {
                texts[p].push(' ');
            }
            texts[p].push_str(&lines[i].text());
            let r = lines[i].rect;
            let b = [r[0], r[1], r[0] + r[2], r[1] + r[3]];
            bounds = Some(bounds.map_or(b, |a| union(a, b)));
        }
        rows.push(texts.iter().map(|t| cell_of(t)).collect());
    }
    let header = rows[0].iter().all(|c| matches!(c, Cell::Str(_))) && rows[1..].iter().any(|r| r.iter().any(|c| matches!(c, Cell::Num(_))));
    let columns = if header {
        let h = rows.remove(0);
        h.into_iter()
            .enumerate()
            .map(|(i, c)| match c {
                Cell::Str(s) => s,
                _ => column_name(i),
            })
            .collect()
    } else {
        (0..cols.len()).map(column_name).collect()
    };
    Some(Table { columns, rows, rect: to_rect4(bounds.unwrap_or([0.0; 4])) })
}

// ---------------------------------------------------------------- search

/// Scripts written without spaces between words.
fn wide(c: char) -> bool {
    matches!(c as u32, 0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF)
}

fn build_index(pt: &mut PageText) {
    let mut chars = Vec::new();
    let mut cg = Vec::new();
    let mut cl = Vec::new();
    for b in &pt.blocks {
        for &li in &b.lines {
            let line = &pt.lines[li];
            // lines join with a space, except CJK text wrapped mid-sentence
            let first = line.glyphs.first().and_then(|&g| pt.glyphs[g].text.chars().next());
            let joined_cjk = chars.last().is_some_and(|c| wide(*c)) && first.is_some_and(wide);
            if !chars.is_empty() && !joined_cjk {
                chars.push(' ');
                cg.push(None);
                cl.push(li as u32);
            }
            let mut last_space = true;
            let mut prev: Option<usize> = None;
            for &gi in &line.glyphs {
                let g = &pt.glyphs[gi];
                // gaps that split words read as one space
                let gap_space = prev.is_some_and(|p| {
                    let pg = &pt.glyphs[p];
                    let along = (g.origin.0 - pg.origin.0) * line.dir.0 + (g.origin.1 - pg.origin.1) * line.dir.1 - pg.adv;
                    along > 0.18 * g.size
                });
                if g.is_space() || gap_space {
                    if !last_space {
                        chars.push(' ');
                        cg.push(None);
                        cl.push(li as u32);
                        last_space = true;
                    }
                    if g.is_space() {
                        prev = Some(gi);
                        continue;
                    }
                }
                for c in g.text.chars() {
                    chars.push(c);
                    cg.push(Some(gi as u32));
                    cl.push(li as u32);
                }
                last_space = false;
                prev = Some(gi);
            }
        }
    }
    pt.chars = chars;
    pt.char_glyph = cg;
    pt.char_line = cl;
}

#[derive(Clone, Debug)]
pub struct Hit {
    /// 1-based.
    pub page: usize,
    pub text: String,
    pub rects: Vec<Rect4>,
    pub context: String,
}

fn fold(chars: &[char], case: bool) -> (Vec<char>, Vec<usize>) {
    let mut out = Vec::with_capacity(chars.len());
    let mut map = Vec::with_capacity(chars.len());
    for (i, &c) in chars.iter().enumerate() {
        if case {
            out.push(c);
            map.push(i);
        } else {
            for l in c.to_lowercase() {
                out.push(l);
                map.push(i);
            }
        }
    }
    (out, map)
}

impl PageText {
    /// Plain text: lines joined with newlines, blocks with a blank line.
    pub fn plain(&self) -> String {
        self.blocks
            .iter()
            .map(|b| b.lines.iter().map(|&l| self.lines[l].text()).collect::<Vec<_>>().join("\n"))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn block_text(&self, b: &Block) -> String {
        b.lines.iter().map(|&l| self.lines[l].text()).collect::<Vec<_>>().join("\n")
    }

    /// Words in reading order.
    pub fn words(&self) -> impl Iterator<Item = &Word> {
        self.blocks.iter().flat_map(move |b| b.lines.iter().flat_map(move |&l| self.lines[l].words.iter()))
    }

    /// Every occurrence of `query` (whitespace-insensitive, case-insensitive
    /// unless `case`), with one rectangle per line it covers.
    pub fn search(&self, page: usize, query: &str, case: bool, limit: usize, out: &mut Vec<Hit>) {
        let q: Vec<char> = clean(query).split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
        if q.is_empty() {
            return;
        }
        let (hay, map) = fold(&self.chars, case);
        let (needle, _) = fold(&q, case);
        if needle.is_empty() || needle.len() > hay.len() {
            return;
        }
        // Knuth-Morris-Pratt: linear in the page and the query, whatever they hold
        let mut fail = vec![0usize; needle.len()];
        let mut k = 0;
        for i in 1..needle.len() {
            while k > 0 && needle[i] != needle[k] {
                k = fail[k - 1];
            }
            if needle[i] == needle[k] {
                k += 1;
            }
            fail[i] = k;
        }
        let mut starts = Vec::new();
        let mut k = 0;
        for (i, c) in hay.iter().enumerate() {
            while k > 0 && *c != needle[k] {
                k = fail[k - 1];
            }
            if *c == needle[k] {
                k += 1;
            }
            if k == needle.len() {
                starts.push(i + 1 - needle.len());
                if out.len() + starts.len() >= limit {
                    break;
                }
                // hits do not overlap: look again after this one
                k = 0;
            }
        }
        for i in starts {
            if out.len() >= limit {
                return;
            }
            let (a, b) = (map[i], map[i + needle.len() - 1] + 1);
            let mut rects: Vec<(u32, [f32; 4])> = Vec::new();
            for k in a..b {
                let Some(g) = self.char_glyph[k] else { continue };
                let bb = self.glyphs[g as usize].bbox;
                let line = self.char_line[k];
                match rects.last_mut() {
                    Some((l, r)) if *l == line => *r = union(*r, bb),
                    _ => rects.push((line, bb)),
                }
            }
            let c0 = a.saturating_sub(30);
            let c1 = (b + 30).min(self.chars.len());
            out.push(Hit {
                page,
                text: self.chars[a..b].iter().collect(),
                rects: rects.into_iter().map(|(_, r)| to_rect4(r)).collect(),
                context: self.chars[c0..c1].iter().collect(),
            });
        }
    }

    /// Approximate heap size, for the text cache's byte budget.
    pub fn approx_bytes(&self) -> usize {
        use std::mem::size_of;
        let s = |t: &String| t.capacity() + 16;
        let glyphs = self.glyphs.capacity() * size_of::<Glyph>() + self.glyphs.iter().map(|g| s(&g.text)).sum::<usize>();
        let lines = self.lines.capacity() * size_of::<Line>()
            + self.lines.iter().map(|l| l.glyphs.capacity() * size_of::<usize>() + l.words.capacity() * size_of::<Word>() + l.words.iter().map(|w| s(&w.text)).sum::<usize>()).sum::<usize>();
        let blocks = self.blocks.capacity() * size_of::<Block>() + self.blocks.iter().map(|b| b.lines.capacity() * size_of::<usize>()).sum::<usize>();
        let tables = self.tables.iter().map(|t| t.rows.iter().map(|r| r.len() * (size_of::<Cell>() + 16)).sum::<usize>() + t.columns.iter().map(s).sum::<usize>()).sum::<usize>();
        let index = self.chars.capacity() * size_of::<char>() + self.char_glyph.capacity() * size_of::<Option<u32>>() + self.char_line.capacity() * size_of::<u32>();
        size_of::<PageText>() + glyphs + lines + blocks + tables + index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(text: &str, x: f32, y: f32, size: f32, adv: f32) -> Glyph {
        Glyph {
            text: text.into(),
            bbox: [x, y - 0.8 * size, x + adv, y + 0.2 * size],
            origin: (x, y),
            dir: (1.0, 0.0),
            adv,
            size,
            invisible: false,
        }
    }

    /// A run of text, one glyph per char, 0.5 em advance, starting at x.
    fn run(out: &mut Vec<Glyph>, s: &str, x: f32, y: f32, size: f32) -> f32 {
        let mut x = x;
        for c in s.chars() {
            out.push(glyph(&c.to_string(), x, y, size, 0.5 * size));
            x += 0.5 * size;
        }
        x
    }

    #[test]
    fn words_split_on_gaps_and_spaces() {
        let mut g = Vec::new();
        let x = run(&mut g, "Hello", 10.0, 100.0, 10.0);
        let x = run(&mut g, "world", x + 3.0, 100.0, 10.0); // 0.3 em gap, no space glyph
        let x = run(&mut g, " ", x, 100.0, 10.0);
        run(&mut g, "again", x, 100.0, 10.0);
        let pt = layout(g, 600.0, 800.0);
        assert_eq!(pt.lines.len(), 1);
        let words: Vec<&str> = pt.words().map(|w| w.text.as_str()).collect();
        assert_eq!(words, ["Hello", "world", "again"]);
        assert_eq!(pt.plain(), "Hello world again");
    }

    #[test]
    fn out_of_order_pieces_join_one_line() {
        let mut g = Vec::new();
        // written right piece first, 3 pt apart
        run(&mut g, "right", 33.0, 100.0, 10.0);
        run(&mut g, "left", 10.0, 100.0, 10.0);
        let pt = layout(g, 600.0, 800.0);
        assert_eq!(pt.lines.len(), 1);
        assert_eq!(pt.plain(), "left right");
    }

    #[test]
    fn two_columns_read_column_by_column() {
        let mut g = Vec::new();
        for k in 0..5 {
            let y = 100.0 + 14.0 * k as f32;
            run(&mut g, &format!("left{k} text of a column that runs on"), 50.0, y, 10.0);
            run(&mut g, &format!("right{k} text of a column that runs on"), 320.0, y, 10.0);
        }
        let pt = layout(g, 612.0, 792.0);
        let text = pt.plain();
        let l4 = text.find("left4").unwrap();
        let r0 = text.find("right0").unwrap();
        assert!(l4 < r0, "{text}");
        assert!(pt.tables.is_empty(), "running text is not a table");
    }

    #[test]
    fn aligned_columns_become_a_table() {
        let mut g = Vec::new();
        let rows = [["Item", "Qty", "Price"], ["Apple", "3", "1,200.50"], ["Pear", "12", "(7)"]];
        for (k, r) in rows.iter().enumerate() {
            let y = 200.0 + 16.0 * k as f32;
            run(&mut g, r[0], 72.0, y, 10.0);
            run(&mut g, r[1], 200.0, y, 10.0);
            run(&mut g, r[2], 300.0, y, 10.0);
        }
        let pt = layout(g, 612.0, 792.0);
        assert_eq!(pt.tables.len(), 1);
        let t = &pt.tables[0];
        assert_eq!(t.columns, ["Item", "Qty", "Price"]);
        assert_eq!(t.rows[0], [Cell::Str("Apple".into()), Cell::Num(3.0), Cell::Num(1200.5)]);
        assert_eq!(t.rows[1][2], Cell::Num(-7.0));
    }

    #[test]
    fn search_spans_lines_and_ignores_case() {
        let mut g = Vec::new();
        run(&mut g, "The quick", 72.0, 100.0, 10.0);
        run(&mut g, "Brown fox", 72.0, 112.0, 10.0);
        let pt = layout(g, 612.0, 792.0);
        let mut hits = Vec::new();
        pt.search(1, "QUICK  brown", false, 10, &mut hits);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "quick Brown");
        assert_eq!(hits[0].rects.len(), 2);
        let r = hits[0].rects[0];
        assert!((r[0] - 92.0).abs() < 0.01 && (r[2] - 25.0).abs() < 0.01, "{r:?}");
        let mut none = Vec::new();
        pt.search(1, "QUICK", true, 10, &mut none);
        assert!(none.is_empty());
    }

    #[test]
    fn search_hits_do_not_overlap_and_respect_the_limit() {
        let mut g = Vec::new();
        run(&mut g, "aaaaa abab ababab", 72.0, 100.0, 10.0);
        let pt = layout(g, 612.0, 792.0);
        let count = |q: &str, limit: usize| {
            let mut h = Vec::new();
            pt.search(1, q, false, limit, &mut h);
            h.into_iter().map(|h| h.text).collect::<Vec<_>>()
        };
        assert_eq!(count("aa", 10), ["aa", "aa"]);
        assert_eq!(count("abab", 10), ["abab", "abab"]);
        assert_eq!(count("aab", 10), Vec::<String>::new());
        assert_eq!(count("a", 3).len(), 3);
        assert_eq!(count("baba", 10), ["baba"]);
    }

    /// Glyphs scattered over a page, none close enough to another to share a
    /// line or a block (map labels, CAD dimensions, a crafted file).
    fn scattered(n: usize, size: f32) -> Vec<Glyph> {
        let mut g = Vec::with_capacity(n);
        let mut seed = 0x2545_F491u32;
        for k in 0..n {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let x = (seed % 59_000) as f32 / 100.0 + 5.0;
            let y = ((seed / 59_000) % 76_000) as f32 / 100.0 + 15.0;
            let c = char::from(b'a' + (k % 26) as u8);
            g.push(glyph(&c.to_string(), x, y, size, 0.5 * size));
        }
        g
    }

    #[test]
    fn layout_stays_fast_on_scattered_and_crafted_pages() {
        let cases: Vec<(&str, Vec<Glyph>)> = vec![
            ("50k scattered", scattered(50_000, 1.0)),
            // a grid: 250 columns, 200 rows, every glyph its own line and block
            ("50k grid", (0..50_000).map(|k| glyph("x", 4.0 + (k % 250) as f32 * 2.4, 6.0 + (k / 250) as f32 * 3.9, 1.0, 0.5)).collect()),
            // 100k separate pieces on one baseline
            ("100k one row", (0..100_000).map(|k| glyph("x", 1.0 + k as f32 * 0.006, 400.0, 0.002, 0.001)).collect()),
            // tall lines stacked a hair apart: they overlap, so none joins another
            ("50k tall overlapping lines", (0..50_000).map(|k| glyph("w", 10.0, 300.0 + k as f32 * 0.004, 300.0, 590.0)).collect()),
        ];
        for (name, g) in cases {
            let n = g.len();
            let t0 = std::time::Instant::now();
            let pt = layout(g, 612.0, 792.0);
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            eprintln!("{name}: {n} glyphs, {} lines, {} blocks, layout {ms:.0} ms", pt.lines.len(), pt.blocks.len());
            assert!(ms < 4000.0, "{name}: {ms:.0} ms");
            assert!(!pt.blocks.is_empty());
            assert_eq!(pt.blocks.iter().map(|b| b.lines.len()).sum::<usize>(), pt.lines.len(), "{name}: every line in one block");
        }
    }

    #[test]
    fn scattered_glyphs_still_make_sense() {
        // a few real lines among scattered labels still form their paragraph
        let mut g = scattered(20_000, 1.0);
        g.retain(|g| !(60.0..420.0).contains(&g.origin.0) || !(270.0..340.0).contains(&g.origin.1));
        run(&mut g, "first line of a paragraph", 100.0, 300.0, 12.0);
        run(&mut g, "second line of a paragraph", 100.0, 314.0, 12.0);
        let pt = layout(g, 612.0, 792.0);
        let b = pt.blocks.iter().find(|b| b.lines.iter().any(|&l| pt.lines[l].text().starts_with("first"))).expect("paragraph block");
        assert_eq!(b.lines.len(), 2, "{:?}", b.lines.iter().map(|&l| pt.lines[l].text()).collect::<Vec<_>>());
    }

    #[test]
    fn layout_can_be_cancelled() {
        let g = scattered(20_000, 1.0);
        assert_eq!(layout_with(g.clone(), 612.0, 792.0, &|| true).err().as_deref(), Some(CANCELLED));
        assert!(layout_with(g, 612.0, 792.0, &|| false).is_ok());
    }

    #[test]
    fn numbers_and_names() {
        assert_eq!(parse_number("1,234.5"), Some(1234.5));
        assert_eq!(parse_number("$12"), Some(12.0));
        assert_eq!(parse_number("-3e2"), Some(-300.0));
        assert_eq!(parse_number("2026-10-24"), None);
        assert_eq!(parse_number("12,34"), None);
        assert_eq!(parse_number("12%"), None);
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(clean("\u{FB01}nd\u{00A0}x\u{0000}"), "find x");
    }
}
