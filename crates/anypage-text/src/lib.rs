//! Text system: shaping, bidi, font fallback and line breaking for every
//! script, drawn into premultiplied RGBA8.
//!
//! [`TextSystem`] is a drop-in replacement for the engine's single-font
//! `TextRaster` (`crates/anypage-engine/src/raster.rs`): the same method
//! names and units (`load`, `measure`, `line_height`, `line_metrics`,
//! `caret_x`, `hit_x`, `layout_ranges`, `layout`, the `source` field), plus
//! [`TextSystem::draw_line`], which replaces the body of `Canvas::text`:
//!
//! ```ignore
//! // Canvas::text, after the swap
//! let adv = tr.draw_line(pm.data_mut(), pm.width(), pm.height(), clip, x * s, y * s, px * s, c, text);
//! adv / s
//! ```
//!
//! # What it does
//!
//! - **Shaping** with HarfRust (the Rust port of HarfBuzz) through
//!   cosmic-text: Arabic joining, Indic reordering (Devanagari, Bengali,
//!   Tamil, ...), Thai and Lao marks, CJK, emoji ZWJ sequences, ligatures
//!   and kerning.
//! - **Bidirectional text** (UAX #9): mixed left-to-right and right-to-left
//!   runs are reordered for display; the paragraph direction comes from the
//!   first strong character, or is given ([`Direction`]) so that the wrapped
//!   lines of a paragraph keep its direction ([`TextSystem::layout_lines`]);
//!   carets, hit testing and selection highlights are visual.
//! - **Font fallback** across scripts with per-platform lists (Windows:
//!   Segoe UI, Microsoft JhengHei/YaHei, Nirmala UI, Leelawadee UI, Segoe UI
//!   Emoji; Android: Roboto, Noto Sans CJK, Noto Naskh Arabic, Noto Sans
//!   Hebrew/Devanagari/Thai, Noto Color Emoji; macOS and Linux defaults).
//!   Han glyph style follows the locale (TC, SC, JA, KO). For a Chinese,
//!   Japanese or Korean locale the CJK UI font is the primary font (as on
//!   the platforms themselves), otherwise the platform's Latin UI font.
//!   Characters no installed font has become U+FFFD before shaping, so
//!   fallback never walks every installed face for them.
//! - **Line breaking** with UAX #14 (no break before "。", "）", ...),
//!   emergency breaks inside over-long words at grapheme boundaries.
//! - **Grapheme-aware carets**: positions are byte offsets on extended
//!   grapheme cluster boundaries (UAX #29).
//! - **Colour glyphs**: COLRv1 paint graphs with linear, two-point conical
//!   and sweep gradients, transforms, layers and blend modes (Android 13+
//!   Noto Color Emoji, Windows 11 Segoe UI Emoji) and COLR v0 layers, both
//!   painted by this crate on skrifa + tiny-skia, with the text colour for
//!   palette index 0xFFFF; bitmap strikes (CBDT, sbix: older Noto Color
//!   Emoji, Apple Color Emoji) through swash.
//! - **Any size**: glyph images up to 1024 px on a side are cached. A
//!   larger outline glyph is filled straight into the visible part of the
//!   target (memory bounded by the clip, nothing cached); a larger colour
//!   glyph is rendered at 1024 px, cached, and drawn scaled up.
//!
//! # Why cosmic-text (and not parley)
//!
//! Both sit on the same Linebender stack (skrifa, HarfRust/swash). The
//! choice came down to what the engine needs today:
//! - cosmic-text owns system font discovery (fontdb) and a script-aware
//!   fallback iterator with a pluggable list; parley's fontique has no
//!   Android backend, which AnyPage needs now.
//! - Its shaped line is size-independent (em units) and its layout is
//!   linear in the font size, so one shaping serves every zoom level; this
//!   crate caches shaped lines by text and scales them.
//! - It renders through swash, which the engine already uses (one copy of
//!   the rasteriser, COLR v0 and CBDT colour glyphs).
//! - It is a single dependency with a stable API (0.19), MIT/Apache.
//!
//! What parley does better (rich-text styling spans, ICU4X dictionary line
//! breaking for Thai/Lao/Khmer/Burmese) is noted under limits below.
//!
//! # Threads and caches
//!
//! `TextSystem` is `Send + Sync`. Shaping takes one mutex (the font
//! system); everything else is lock-light: shaped lines and rendered glyphs
//! live in sharded two-generation caches, and large entries (pieces of long
//! paragraphs, big colour glyph images) in a separate LRU each, so they
//! neither evict their neighbours nor each other on insert. Bounds: shaped
//! lines about 40 MB small plus 80 MB large, glyph images 48 MB plus
//! 24 MB; these fill only with that much distinct text. Glyph
//! rasterisation uses a per-thread swash context. `draw_line` of a string
//! seen before does a hash lookup and blits cached glyph masks.
//!
//! Lines over 4 KiB are shaped in pieces cut after whitespace; each piece is
//! cached by its text and queries go to the piece they concern (no combined
//! copy is built). A warm `measure`, `caret_x` or `hit_x` on a 1 MiB
//! paragraph is a few milliseconds, and an edit inside it reshapes only the
//! piece it falls in (and, when a piece boundary moves, the next one or two).
//!
//! # Luau API
//!
//! This crate registers no Luau functions itself (it must not depend on the
//! runtime). The runtime's built-in `text` module (`anypage-runtime/src/text.rs`)
//! exposes it to pages unchanged once the engine swaps `TextRaster` for
//! [`TextSystem`]:
//!
//! | Luau | Rust |
//! |---|---|
//! | `text.width(s, size)` | [`TextSystem::measure`] |
//! | `text.line_height(size)` | [`TextSystem::line_height`] |
//! | `text.layout(s, size, width)` → `{ lines = {{from, to, width}}, line_height, height }` | [`TextSystem::layout_ranges`] |
//! | `text.caret(s, size, pos)` | [`TextSystem::caret_x`] |
//! | `text.hit(s, size, x)` | [`TextSystem::hit_x`] |
//! | `d:text(x, y, size, color, s)`, `d:textbox(...)` in panels | [`TextSystem::draw_line`] via `Canvas::text` |
//!
//! Additions a page could use once the runtime exposes them (proposed in
//! CORE_REQUESTS.md):
//!
//! | proposed Luau | Rust |
//! |---|---|
//! | `text.layout(...)` lines gain `rtl` | [`TextSystem::layout_lines`] ([`Line::dir`]) |
//! | `d:text(x, y, size, color, s, { dir = "rtl" })` | [`TextSystem::draw_line_dir`] |
//! | `text.caret(s, size, pos, dir)`, `text.hit(s, size, x, dir)` | [`TextSystem::caret_x_dir`], [`TextSystem::hit_x_dir`] |
//! | `text.selection(s, size, from, to, dir)` → `{ {x0, x1} }` | [`TextSystem::selection_rects_dir`] |
//! | `text.is_rtl(s)` | [`TextSystem::is_rtl`] |
//! | `text.missing(s)` | [`TextSystem::missing_glyphs`] |
//! | `text.glyphs(s, size)` (PDF text export) | [`TextSystem::glyphs`] |
//!
//! # Differences from `TextRaster`, on purpose
//!
//! - Measuring shapes the text (kerning, ligatures, joined Arabic), so
//!   widths differ from summed per-character advances.
//! - `caret_x`/`hit_x` work on grapheme boundaries and are visual: in a
//!   right-to-left line `caret_x(s, px, 0)` is the right end. Like
//!   `TextRaster`, `hit_x` past the right end of a left-to-right line
//!   (`math.huge` included) is the text length; mirrored right to left.
//! - `layout_ranges` follows UAX #14, and returns a final empty line for
//!   text ending in '\n' and one empty line for "" (a caret needs a line).
//!   At a soft break the whitespace belongs to neither line, always (the
//!   old breaker kept a trailing space when it fitted). A '\r' before '\n'
//!   is not part of the line.
//! - Fonts: the old raster drew everything with Microsoft JhengHei. Now the
//!   primary font follows the locale (Microsoft YaHei UI for zh-CN,
//!   JhengHei UI for zh-TW/HK, Segoe UI for other locales on Windows) and
//!   every script falls back to a font that has it. Pass
//!   [`LoadOptions::locale`] to pin the Han style regardless of the system
//!   (the hosts do not yet; see CORE_REQUESTS.md).
//!
//! # Limits
//!
//! - Single-line calls look at the first [`MAX_LINE_BYTES`] (1 MiB) of
//!   their text; `layout_lines`/`layout_ranges` at the first
//!   [`MAX_TEXT_BYTES`] (4 MiB), laying out a paragraph longer than 1 MiB
//!   in 1 MiB segments (a line ends at each segment end). Keeping one 1 MiB
//!   line shaped costs about 57 MB, which is why the limit is not higher.
//! - `draw_line` shapes at most [`MAX_DRAW_BYTES`] (64 KiB; more cannot be
//!   visible on one line) and returns the advance of what it shaped, so for
//!   a longer line it is less than `measure`.
//! - The plain methods ([`TextSystem::draw_line`], `caret_x`, ...) take the
//!   direction from the line itself; a wrapped line that starts with a word
//!   of the other direction needs the `_dir` variants with the paragraph's
//!   [`Line::dir`] to keep the paragraph's order.
//! - Thai, Lao, Khmer and Burmese have no spaces between words; UAX #14
//!   alone gives no break opportunities inside them (no dictionary), so long
//!   runs break at grapheme boundaries.
//! - One style: regular weight, upright, no letter spacing or rich-text
//!   spans. SVG-only colour fonts cannot be drawn in colour; on load such
//!   emoji faces are dropped when another emoji face is installed. COLR
//!   glyphs use palette 0.
//! - Characters no installed font covers draw as U+FFFD and count in
//!   [`TextSystem::missing_glyphs`]. Finding that out opens every installed
//!   face once (about 80 ms with 176 faces on Windows, the first time such
//!   a character appears; 0.4 ms for each new one after that, and the
//!   answer is cached per code point).
//! - Colour glyphs over 1024 px are scaled up from a 1024 px image (soft
//!   edges at 2000 px); bitmap strikes over 400 px from 512 px. Painting a
//!   COLRv1 emoji at 1024 px takes about 0.4-0.8 s once per glyph and size
//!   (then cached), on the calling thread.
//! - Cold shaping costs about 1-2 us per byte of Latin text and a few
//!   microseconds per character of mixed scripts (fallback across fonts is
//!   per word, and UAX #14 makes each ideograph a word); it is paid once per
//!   distinct piece, then cached. cosmic-text's own run cache is not
//!   enabled: its key ignores the run direction, so a neutral run shaped
//!   left to right could be reused unmirrored in a right-to-left one.
//!
//! # Measured (Windows 11, release, `cargo run --release --example timings`)
//!
//! Windows 11 (176 faces): font load 30-70 ms with a warm OS file cache,
//! about 650 ms on the very first run; shaping 10 000 mixed-script
//! characters cold 45-125 ms; cached `measure` 32-45 ns per line;
//! `draw_line` of a cached 61-glyph line at 16 px 16-30 us (270-490 ns per
//! glyph, blending included); a COLRv1 emoji rasterises once per size in
//! 0.4 ms at 16 px, 4-8 ms at 128 px. Over the image cap: "H" at 1600 px
//! drawn directly in about 10 ms each time; an emoji at 1200 px about
//! 0.75 s the first time and 40 ms after (scaled blit of 1656 x 1533 px).
//! A 1 MiB paragraph: `measure` cold 1-4 s (machine load), warm 1.7 ms;
//! `caret_x` 1.8 ms, `hit_x` 0.8 ms; `layout_ranges` (13 908 lines) 385 ms
//! the first time after shaping (caret tables built), 49 ms warm;
//! `caret_x` after a one-character edit 14 ms (one piece reshaped).
//!
//! Android 15 emulator (x86_64, 214 faces in `/system/fonts`): font load
//! 10-50 ms warm, 810 ms on the first run; 10 000 characters cold 61-77 ms;
//! cached 61-glyph line 24 us.
//!
//! The Windows machine was shared with other builds, so these are upper
//! bounds.

mod breaking;
mod cache;
mod fonts;
mod raster;
mod shape;
mod colr;

use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

use cosmic_text::fontdb::{self, Database, ID};
use cosmic_text::{Attrs, AttrsList, Family, Font, FontSystem, Hinting, ShapeLine, Shaping, Wrap};
use rustc_hash::FxHashMap;

use cache::Cache;
use raster::{Bitmap, GlyphKey, Target};
use shape::{Glyph, Graphemes, Shaped, ShapedLine};

/// Longest text [`TextSystem::layout_lines`] and [`TextSystem::layout_ranges`]
/// look at (bytes).
pub const MAX_TEXT_BYTES: usize = 4 << 20;
/// Longest text the single-line calls (`measure`, `caret_x`, `hit_x`,
/// `selection_rects`, `glyphs`, `is_rtl`, `missing_glyphs`) look at
/// (bytes); a longer line is cut there. Paragraphs of a laid-out text may
/// be longer: they are laid out in segments of this size.
pub const MAX_LINE_BYTES: usize = 1 << 20;
/// Longest text `draw_line` shapes (bytes).
pub const MAX_DRAW_BYTES: usize = 64 << 10;
/// Lines longer than this are shaped in pieces (bytes). Small pieces keep
/// an edit inside a long line cheap: only the piece that changed is shaped
/// again.
const CHUNK_BYTES: usize = 4 << 10;
/// Font sizes are clamped to this range (px).
pub const MIN_PX: f32 = 0.25;
pub const MAX_PX: f32 = 2048.0;

/// Shaped lines: small ones in the sharded cache, pieces of long lines in
/// the large-entry LRU (64 MB holds every piece of a 1 MiB paragraph, so
/// repeated queries on it stay warm).
const LINE_CACHE_BYTES: usize = 32 << 20;
const LINE_LARGE_BYTES: usize = 64 << 20;
const GLYPH_CACHE_BYTES: usize = 48 << 20;
/// Glyph images over about 190 KB (colour glyphs from about 220 px): the
/// largest is 4 MB (1024 x 1024 RGBA).
const GLYPH_LARGE_BYTES: usize = 24 << 20;
const COVERAGE_CACHE_MAX: usize = 1 << 17;
/// From this size glyphs are placed on whole pixels (no subpixel bins): a
/// quarter-pixel offset is invisible there, and big glyph images are not
/// kept four times.
const WHOLE_PIXEL_PX: f32 = 128.0;

/// Options for [`TextSystem::load_with`].
#[derive(Clone, Debug, Default)]
pub struct LoadOptions {
    /// BCP 47 locale used for Han glyph style, e.g. "zh-TW", "ja"
    /// (default: the system locale).
    pub locale: Option<String>,
    /// Preferred family for Latin text (default: the platform UI font).
    pub primary_family: Option<String>,
    /// Font files (TTF/OTF/TTC bytes) added to the system fonts, e.g.
    /// fonts bundled with an app.
    pub extra_fonts: Vec<Vec<u8>>,
    /// Skip the system fonts (only `extra_fonts`).
    pub no_system_fonts: bool,
}

/// Paragraph (base) direction of a line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Direction {
    /// From the line's first strong character (UAX #9 P2/P3); left to
    /// right when it has none.
    #[default]
    Auto,
    Ltr,
    Rtl,
}

/// A line from [`TextSystem::layout_lines`].
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    /// Byte range in the laid-out text.
    pub range: Range<usize>,
    /// Width in px.
    pub width: f32,
    /// Direction of the line's paragraph (`Ltr` or `Rtl`).
    pub dir: Direction,
}

/// A positioned glyph of a laid-out line, for exporters (PDF text, GPU
/// atlases) and tests.
#[derive(Clone, Debug, PartialEq)]
pub struct PositionedGlyph {
    /// Byte range of the cluster this glyph belongs to.
    pub cluster: Range<usize>,
    /// Pen position (px) from the line's left edge, offsets applied.
    pub x: f32,
    /// Vertical offset from the baseline (px, positive is up).
    pub y: f32,
    pub advance: f32,
    pub glyph_id: u16,
    /// Font of the glyph; see [`TextSystem::font_name`].
    pub font: u16,
    pub rtl: bool,
}

/// Cache occupancy, from [`TextSystem::stats`].
#[derive(Clone, Copy, Debug, Default)]
pub struct TextStats {
    pub faces: usize,
    pub fonts_in_use: usize,
    pub lines_cached: usize,
    pub line_bytes: usize,
    pub glyphs_cached: usize,
    pub glyph_bytes: usize,
}

struct State {
    fs: FontSystem,
    attrs: AttrsList,
    /// (face, weight) to font-table slot.
    slots: FxHashMap<(ID, u16), u16>,
    /// Whether any installed face maps a code point.
    coverage: FxHashMap<u32, bool>,
    /// Faces probed for coverage, likeliest first.
    probe: Vec<ID>,
    primary_id: Option<ID>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl State {
    fn covered(&mut self, c: char) -> bool {
        let cp = c as u32;
        if let Some(&v) = self.coverage.get(&cp) {
            return v;
        }
        if self.coverage.len() > COVERAGE_CACHE_MAX {
            self.coverage.clear();
        }
        // Likely faces first (the shaper loads them anyway), then the rest.
        // Each face is opened once through the font system's cache: on
        // Windows mapping a font file costs about 1 ms, so a lookup that
        // re-mapped files per character would be far slower after the first.
        let mut found = false;
        for i in 0..self.probe.len() {
            let id = self.probe[i];
            if self.fs.get_font(id, fontdb::Weight::NORMAL).is_some_and(|f| f.as_swash().charmap().map(c) != 0) {
                found = true;
                break;
            }
        }
        self.coverage.insert(cp, found);
        found
    }
}

/// The text system: fonts, shaping, layout and drawing. See the crate docs.
pub struct TextSystem {
    state: Mutex<State>,
    /// Font table: slot to font, readable without the shaping lock.
    fonts: RwLock<Vec<(Arc<Font>, u32)>>,
    /// Shaped lines in their own direction.
    lines: Cache<Box<str>, Arc<Shaped>>,
    /// Shaped lines laid out in the direction opposite to their own.
    flipped: Cache<Box<str>, Arc<Shaped>>,
    glyphs: Cache<GlyphKey, Arc<Bitmap>>,
    /// Primary font metrics in em: ascent, descent (positive), leading.
    metrics: (f32, f32, f32),
    replacement: &'static str,
    description: String,
    faces: usize,
    /// One line naming the primary font and the font count (the engine's
    /// summary prints it).
    pub source: String,
}

fn clamp_px(px: f32) -> Option<f32> {
    if px.is_finite() && px > 0.0 {
        Some(px.clamp(MIN_PX, MAX_PX))
    } else {
        None
    }
}

fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

fn clamp_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos.min(s.len());
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

impl TextSystem {
    /// System fonts: every installed face (Windows `%SYSTEMROOT%\Fonts` and
    /// the per-user font folders, Android `/system/fonts` and
    /// `/product/fonts`, macOS and Linux font directories).
    pub fn load() -> Result<Self, String> {
        Self::load_with(LoadOptions::default())
    }

    pub fn load_with(opts: LoadOptions) -> Result<Self, String> {
        let (mut db, mut sources) = if opts.no_system_fonts { (Database::new(), Vec::new()) } else { fonts::system_database() };
        if !opts.extra_fonts.is_empty() {
            let before = db.len();
            for f in opts.extra_fonts {
                db.load_font_data(f);
            }
            sources.push(format!("{} bundled faces", db.len() - before));
        }
        if db.is_empty() {
            return Err(format!("no usable font found (searched: {})", if sources.is_empty() { "nothing".into() } else { sources.join(", ") }));
        }
        let locale = opts.locale.unwrap_or_else(|| sys_locale::get_locale().unwrap_or_else(|| "en-US".into()));
        let t = Self::build(db, sources, locale, opts.primary_family);
        if t.fonts.read().unwrap_or_else(PoisonError::into_inner).is_empty() {
            return Err(format!("no usable font found: {}", t.description));
        }
        Ok(t)
    }

    /// Only the given font files (TTF/OTF/TTC bytes), for tests and apps
    /// that bundle their fonts. Data that is not a font is skipped; with no
    /// usable font everything measures 0 and draws nothing.
    pub fn from_fonts(fonts: Vec<Vec<u8>>) -> Self {
        let mut db = Database::new();
        for f in fonts {
            db.load_font_data(f);
        }
        // Validate every face up front: a face fontdb accepts but the
        // shaper cannot open must not be offered to the fallback.
        let bad: Vec<ID> = db.faces().map(|f| f.id).filter(|&id| Font::new(&db, id, fontdb::Weight::NORMAL).is_none()).collect();
        for id in bad {
            db.remove_face(id);
        }
        let n = db.len();
        Self::build(db, vec![format!("{n} bundled faces")], "en-US".into(), None)
    }

    fn build(mut db: Database, sources: Vec<String>, locale: String, primary: Option<String>) -> Self {
        let cjk = fonts::cjk_for_locale(&locale);
        let primary = primary
            .filter(|p| fonts::has_family(&db, p))
            .or_else(|| fonts::primary_candidates(&locale, cjk).into_iter().find(|n| fonts::has_family(&db, n)).map(|s| s.to_string()))
            .or_else(|| fonts::family_of_first_face(&db))
            .unwrap_or_default();
        db.set_sans_serif_family(primary.clone());
        let pruned = fonts::prune_color_fonts(&mut db);
        let coverage = fonts::coverage_report(&db, cjk);
        let probe = fonts::probe_order(&db, cjk, &primary);
        let faces = db.len();
        let primary_id = fonts::primary_query(&db, &primary).or_else(|| db.faces().next().map(|f| f.id));

        let mut fs = FontSystem::new_with_locale_and_db_and_fallback(locale.clone(), db, fonts::Lists::new(cjk));
        // The primary font is slot 0; its metrics set the line height. If it
        // cannot be opened, the first face that can takes its place.
        let mut table = Vec::new();
        let mut slots = FxHashMap::default();
        let mut metrics = (0.8, 0.2, 0.0);
        let mut primary_used = primary_id;
        let candidates: Vec<ID> = primary_id.into_iter().chain(fs.db().faces().map(|f| f.id)).collect();
        for id in candidates {
            if let Some(f) = fs.get_font(id, fontdb::Weight::NORMAL) {
                let m = f.metrics();
                let upem = m.units_per_em.max(1) as f32;
                if m.ascent > 0.0 {
                    metrics = (m.ascent / upem, m.descent.abs() / upem, m.leading.max(0.0) / upem);
                }
                slots.insert((id, fontdb::Weight::NORMAL.0), 0u16);
                table.push((f, fs.db().face(id).map_or(0, |i| i.index)));
                primary_used = Some(id);
                break;
            }
        }
        let primary_name = primary_used.map(|id| fonts::face_name(fs.db(), id)).unwrap_or_else(|| "none".into());
        let mut state = State {
            attrs: AttrsList::new(&Attrs::new().family(Family::SansSerif)),
            fs,
            slots,
            coverage: FxHashMap::default(),
            probe,
            primary_id: primary_used,
        };
        let replacement = if !table.is_empty() && state.covered('\u{FFFD}') { "\u{FFFD}" } else { "?" };

        let mut d = format!("{faces} font faces from {}; locale {locale}; primary font {primary_name}", sources.join(", "));
        for (script, fam) in &coverage {
            d.push_str(&format!("\n  {script}: {}", fam.unwrap_or("(no listed font; any installed face that covers it)")));
        }
        if !pruned.is_empty() {
            d.push_str(&format!("\n  dropped colour faces that cannot be drawn in colour (SVG only): {}", pruned.join(", ")));
        }
        let source = format!("{primary_name} + {} faces", faces.saturating_sub(1));
        Self {
            state: Mutex::new(state),
            fonts: RwLock::new(table),
            lines: Cache::new(LINE_CACHE_BYTES, LINE_LARGE_BYTES),
            flipped: Cache::new(LINE_CACHE_BYTES / 4, LINE_LARGE_BYTES / 4),
            glyphs: Cache::new(GLYPH_CACHE_BYTES, GLYPH_LARGE_BYTES),
            metrics,
            replacement,
            description: d,
            faces,
            source,
        }
    }

    /// Which fonts were found: count, sources, locale, the primary font and
    /// the face used for Han, Arabic, Hebrew, Devanagari, Thai and emoji.
    pub fn describe(&self) -> String {
        self.description.clone()
    }

    fn font(&self, slot: u16) -> Option<(Arc<Font>, u32)> {
        self.fonts.read().unwrap_or_else(PoisonError::into_inner).get(slot as usize).cloned()
    }

    /// Family name of a font slot from [`PositionedGlyph::font`].
    pub fn font_name(&self, slot: u16) -> String {
        let Some(f) = self.font(slot) else { return String::new() };
        let st = lock(&self.state);
        fonts::face_name(st.fs.db(), f.0.id())
    }

    // ------------------------------------------------------------ shaping

    /// Shape one piece. `force_rtl` sets the paragraph direction instead of
    /// taking it from the first strong character: the text is shaped behind
    /// an LRM or RLM mark (zero width, default ignorable) whose glyph is
    /// dropped again.
    fn shape_piece(&self, text: &str, force_rtl: Option<bool>) -> Shaped {
        if text.is_empty() {
            return Shaped::empty();
        }
        let mut guard = lock(&self.state);
        let st = &mut *guard;
        if st.primary_id.is_none() {
            return Shaped::empty();
        }
        let mark = match force_rtl {
            Some(true) => "\u{200F}",
            Some(false) => "\u{200E}",
            None => "",
        };
        let marked;
        let input = if mark.is_empty() {
            text
        } else {
            marked = format!("{mark}{text}");
            marked.as_str()
        };
        let skip = mark.len();
        let prepared = shape::prepare(input, |c| st.covered(c), self.replacement);
        let fs = &mut st.fs;
        let attrs = &st.attrs;
        let shaped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let line = ShapeLine::new(fs, &prepared.text, attrs, Shaping::Advanced, 4);
            let layout = line.layout(1.0, None, Wrap::None, None, None, Hinting::Disabled);
            (line.rtl, layout)
        }));
        let Ok((rtl, layout)) = shaped else {
            log::error!("text shaping failed for a {}-byte string", text.len());
            return Shaped::empty();
        };
        let mut glyphs = Vec::with_capacity(text.len());
        let mut width = 0.0f32;
        let mut missing = prepared.substituted;
        let mut last: Option<((ID, u16), u16)> = None;
        let mut new_fonts: Vec<(u16, (Arc<Font>, u32))> = Vec::new();
        let mut next_slot = self.fonts.read().unwrap_or_else(PoisonError::into_inner).len();
        for ll in &layout {
            width = width.max(ll.w);
            for g in &ll.glyphs {
                if g.start < skip && g.w == 0.0 {
                    continue; // the direction mark
                }
                let key = (g.font_id, g.font_weight.0);
                let slot = match last {
                    Some((k, s)) if k == key => s,
                    _ => {
                        let s = match st.slots.get(&key) {
                            Some(&s) => s,
                            None => match st.fs.get_font(g.font_id, g.font_weight) {
                                Some(f) if next_slot < u16::MAX as usize => {
                                    let s = next_slot as u16;
                                    next_slot += 1;
                                    st.slots.insert(key, s);
                                    let index = st.fs.db().face(g.font_id).map_or(0, |i| i.index);
                                    new_fonts.push((s, (f, index)));
                                    s
                                }
                                _ => 0,
                            },
                        };
                        last = Some((key, s));
                        s
                    }
                };
                if g.glyph_id == 0 {
                    missing += 1;
                }
                glyphs.push(Glyph {
                    start: prepared.original(g.start).saturating_sub(skip as u32),
                    end: prepared.original(g.end).saturating_sub(skip as u32),
                    x: g.x,
                    w: g.w,
                    dx: g.x_offset,
                    dy: g.y_offset,
                    font: slot,
                    id: g.glyph_id,
                    rtl: g.level.is_rtl(),
                    italic: g.cache_key_flags.contains(cosmic_text::CacheKeyFlags::FAKE_ITALIC),
                });
            }
        }
        if !new_fonts.is_empty() {
            let mut table = self.fonts.write().unwrap_or_else(PoisonError::into_inner);
            for (s, f) in new_fonts {
                if s as usize == table.len() {
                    table.push(f);
                }
            }
        }
        drop(guard);
        glyphs.shrink_to_fit();
        Shaped::new(glyphs, width, rtl, missing)
    }

    fn piece(&self, text: &str, dir: Direction) -> Arc<Shaped> {
        let auto = match self.lines.get(text) {
            Some(s) => s,
            None => {
                let s = Arc::new(self.shape_piece(text, None));
                self.lines.insert(text.into(), s.clone(), s.cost(text.len()));
                s
            }
        };
        match dir {
            Direction::Ltr if auto.rtl => {}
            Direction::Rtl if !auto.rtl => {}
            _ => return auto,
        }
        // The text's own direction is fixed, so "the other direction" is
        // unambiguous and the text alone is the key.
        if let Some(s) = self.flipped.get(text) {
            return s;
        }
        let s = Arc::new(self.shape_piece(text, Some(dir == Direction::Rtl)));
        self.flipped.insert(text.into(), s.clone(), s.cost(text.len()));
        s
    }

    /// The shaped line for `s`. Long text is shaped in pieces, each cached
    /// on its own and laid out in the direction of the first piece; the
    /// pieces are found again by their text, so a warm call on a long line
    /// costs a hash lookup per piece.
    fn line(&self, s: &str, dir: Direction) -> ShapedLine {
        if s.len() <= CHUNK_BYTES {
            return ShapedLine::one(self.piece(s, dir));
        }
        let ranges = shape::chunks(s, CHUNK_BYTES);
        let mut parts: Vec<(usize, Arc<Shaped>)> = Vec::with_capacity(ranges.len());
        let mut dir = dir;
        for r in ranges {
            let Some(t) = s.get(r.clone()) else { continue };
            let p = self.piece(t, dir);
            if dir == Direction::Auto {
                dir = if p.rtl { Direction::Rtl } else { Direction::Ltr };
            }
            parts.push((r.start, p));
        }
        ShapedLine::many(parts)
    }

    // ------------------------------------------------------------ metrics

    /// Width of a single line of text at `px`, in px (shaped: kerning,
    /// ligatures and joining included).
    pub fn measure(&self, s: &str, px: f32) -> f32 {
        let Some(px) = clamp_px(px) else { return 0.0 };
        self.line(cut(s, MAX_LINE_BYTES), Direction::Auto).width() * px
    }

    /// (ascent, line height) in px, from the primary font.
    pub fn line_metrics(&self, px: f32) -> (f32, f32) {
        let Some(px) = clamp_px(px) else { return (0.0, 0.0) };
        let (a, d, l) = self.metrics;
        if a <= 0.0 {
            return (px * 0.8, px * 1.25);
        }
        (a * px, (a + d + l) * px)
    }

    /// Distance between baselines at `px`.
    pub fn line_height(&self, px: f32) -> f32 {
        self.line_metrics(px).1
    }

    /// Visual x (px from the line's left edge) of the caret before byte
    /// `byte` on a single line. Offsets inside a grapheme cluster snap to
    /// its start. In right-to-left text the caret at 0 is on the right.
    pub fn caret_x(&self, s: &str, px: f32, byte: usize) -> f32 {
        self.caret_x_dir(s, px, byte, Direction::Auto)
    }

    /// [`TextSystem::caret_x`] for a line laid out in direction `dir`.
    pub fn caret_x_dir(&self, s: &str, px: f32, byte: usize, dir: Direction) -> f32 {
        let Some(px) = clamp_px(px) else { return 0.0 };
        let s = cut(s, MAX_LINE_BYTES);
        self.line(s, dir).caret(s, clamp_boundary(s, byte)) * px
    }

    /// Byte offset of the grapheme boundary whose caret is nearest to `x`
    /// (px from the line's left edge) on a single line. Past the right end
    /// of a left-to-right line (`x` = `f32::INFINITY` included) it is the
    /// text length, past its left end 0; mirrored for right-to-left lines.
    pub fn hit_x(&self, s: &str, px: f32, x: f32) -> usize {
        self.hit_x_dir(s, px, x, Direction::Auto)
    }

    /// [`TextSystem::hit_x`] for a line laid out in direction `dir`.
    pub fn hit_x_dir(&self, s: &str, px: f32, x: f32, dir: Direction) -> usize {
        let Some(px) = clamp_px(px) else { return 0 };
        let s = cut(s, MAX_LINE_BYTES);
        let x = if x.is_nan() { 0.0 } else { x };
        self.line(s, dir).hit(s, x / px)
    }

    /// Visual intervals (px, left to right, merged) that highlight the
    /// selection `from..to` (byte offsets) on a single line. Bidi text can
    /// give several intervals for one logical range.
    pub fn selection_rects(&self, s: &str, px: f32, from: usize, to: usize) -> Vec<(f32, f32)> {
        self.selection_rects_dir(s, px, from, to, Direction::Auto)
    }

    /// [`TextSystem::selection_rects`] for a line laid out in direction `dir`.
    pub fn selection_rects_dir(&self, s: &str, px: f32, from: usize, to: usize, dir: Direction) -> Vec<(f32, f32)> {
        let Some(px) = clamp_px(px) else { return Vec::new() };
        let s = cut(s, MAX_LINE_BYTES);
        self.line(s, dir).selection(s, from, to).into_iter().map(|(a, b)| (a * px, b * px)).collect()
    }

    /// Whether the text's own direction is right to left (its first strong
    /// character is right-to-left; UAX #9 rules P2 and P3).
    pub fn is_rtl(&self, s: &str) -> bool {
        self.line(cut(s, MAX_LINE_BYTES), Direction::Auto).rtl()
    }

    /// Characters in `s` that no installed font can draw (shown as U+FFFD
    /// or the font's missing-glyph box).
    pub fn missing_glyphs(&self, s: &str) -> usize {
        self.line(cut(s, MAX_LINE_BYTES), Direction::Auto).missing() as usize
    }

    /// The glyphs of one line at `px`, in visual order.
    pub fn glyphs(&self, s: &str, px: f32) -> Vec<PositionedGlyph> {
        self.glyphs_dir(s, px, Direction::Auto)
    }

    /// [`TextSystem::glyphs`] for a line laid out in direction `dir`.
    pub fn glyphs_dir(&self, s: &str, px: f32, dir: Direction) -> Vec<PositionedGlyph> {
        let Some(px) = clamp_px(px) else { return Vec::new() };
        let line = self.line(cut(s, MAX_LINE_BYTES), dir);
        let mut out = Vec::with_capacity(line.pieces().iter().map(|p| p.shaped.glyphs.len()).sum());
        let mut visual: Vec<&shape::Piece> = line.pieces().iter().collect();
        // pieces are in logical order; right to left they run leftwards
        if line.rtl() {
            visual.reverse();
        }
        for p in visual {
            out.extend(p.shaped.glyphs.iter().map(|g| PositionedGlyph {
                cluster: p.start + g.start as usize..p.start + g.end as usize,
                x: (p.x0 + g.x + g.dx) * px,
                y: g.dy * px,
                advance: g.w * px,
                glyph_id: g.id,
                font: g.font,
                rtl: g.rtl,
            }));
        }
        out
    }

    // ------------------------------------------------------------ layout

    /// Line breaking as byte ranges of `s` with their widths (px). Breaks
    /// at '\n' (an empty range for an empty line, including after a final
    /// '\n'), at UAX #14 opportunities (whitespace at a soft break belongs
    /// to neither line; between CJK ideographs but not before closing
    /// punctuation), and inside a word only when the word alone is wider
    /// than `max_w`, between grapheme clusters.
    pub fn layout_ranges(&self, s: &str, px: f32, max_w: f32) -> Vec<(Range<usize>, f32)> {
        self.layout_lines(s, px, max_w).into_iter().map(|l| (l.range, l.width)).collect()
    }

    /// [`TextSystem::layout_ranges`] with each line's paragraph direction:
    /// draw a line with [`TextSystem::draw_line_dir`] and `line.dir`, and
    /// align right-to-left lines to the right, so a wrapped line that
    /// starts with a word of the other direction keeps the order of its
    /// paragraph.
    pub fn layout_lines(&self, s: &str, px: f32, max_w: f32) -> Vec<Line> {
        let Some(px) = clamp_px(px) else { return vec![Line { range: 0..0, width: 0.0, dir: Direction::Ltr }] };
        let s = cut(s, MAX_TEXT_BYTES);
        let max = if max_w.is_nan() { f32::INFINITY } else { max_w.max(0.0) / px };
        let mut out = Vec::new();
        let mut ranges = Vec::new();
        let mut base = 0;
        for para in s.split('\n') {
            let text = para.strip_suffix('\r').unwrap_or(para);
            let mut dir = Direction::Auto;
            ranges.clear();
            if text.is_empty() {
                ranges.push((base..base, 0.0));
            } else {
                // A paragraph over MAX_LINE_BYTES is laid out in segments
                // (cut after whitespace where possible), all in the
                // direction of the first; a line ends at each segment end.
                for seg in shape::chunks(text, MAX_LINE_BYTES) {
                    let Some(t) = text.get(seg.clone()) else { continue };
                    let line = self.line(t, dir);
                    if dir == Direction::Auto {
                        dir = if line.rtl() { Direction::Rtl } else { Direction::Ltr };
                    }
                    breaking::paragraph(t, base + seg.start, &Graphemes::new(&line, t), max, &mut ranges);
                }
            }
            let dir = if dir == Direction::Rtl { Direction::Rtl } else { Direction::Ltr };
            out.extend(ranges.drain(..).map(|(range, w)| Line { range, width: w * px, dir }));
            base += para.len() + 1;
        }
        out
    }

    /// Greedy line breaking (see [`TextSystem::layout_ranges`]) as strings.
    pub fn layout(&self, s: &str, px: f32, max_w: f32) -> Vec<String> {
        self.layout_ranges(s, px, max_w).into_iter().map(|(r, _)| s[r].to_string()).collect()
    }

    // ------------------------------------------------------------ drawing

    fn glyph(&self, key: GlyphKey) -> Option<Arc<Bitmap>> {
        if let Some(b) = self.glyphs.get(&key) {
            return Some(b);
        }
        let font = self.font(key.font)?;
        let b = Arc::new(raster::render(&font.0, font.1, key));
        self.glyphs.insert(key, b.clone(), b.cost());
        Some(b)
    }

    /// Render one glyph without the cache (tests).
    #[cfg(test)]
    fn render_uncached(&self, key: GlyphKey) -> Option<Bitmap> {
        let font = self.font(key.font)?;
        Some(raster::render(&font.0, font.1, key))
    }

    /// Draw one line into premultiplied RGBA8 `dst` (`w` x `h`), clipped to
    /// `clip` (x0, y0, x1, y1 px), top-left at (`x`, `y_top`) px, colour
    /// non-premultiplied. Returns the advance in px. Right-to-left lines
    /// are drawn from `x` too (their visual left edge).
    ///
    /// Only the first [`MAX_DRAW_BYTES`] (64 KiB) of `s` are shaped and
    /// drawn (more cannot be visible on one line), and the advance returned
    /// is theirs: for a longer line it is less than [`TextSystem::measure`],
    /// which looks at up to [`MAX_LINE_BYTES`].
    ///
    /// Glyphs of any size are drawn. Glyph images up to 1024 px on a side
    /// are cached; a larger outline glyph is filled straight into the
    /// visible part of `dst`, and a larger colour glyph is rendered at
    /// 1024 px, cached, and drawn scaled up.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_line(&self, dst: &mut [u8], w: u32, h: u32, clip: (i32, i32, i32, i32), x: f32, y_top: f32, px: f32, color: [u8; 4], s: &str) -> f32 {
        self.draw_line_dir(dst, w, h, clip, x, y_top, px, color, s, Direction::Auto)
    }

    /// [`TextSystem::draw_line`] for a line laid out in direction `dir`
    /// (from [`TextSystem::layout_lines`]).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_line_dir(&self, dst: &mut [u8], w: u32, h: u32, clip: (i32, i32, i32, i32), x: f32, y_top: f32, px: f32, color: [u8; 4], s: &str, dir: Direction) -> f32 {
        let Some(px) = clamp_px(px) else { return 0.0 };
        let line = self.line(cut(s, MAX_DRAW_BYTES), dir);
        let advance = line.width() * px;
        if color[3] == 0 || line.pieces().iter().all(|p| p.shaped.glyphs.is_empty()) || w == 0 || !x.is_finite() || !y_top.is_finite() {
            return advance;
        }
        let rows = (dst.len() / 4 / w as usize).min(h as usize);
        let clip = (clip.0.max(0), clip.1.max(0), clip.2.min(w.min(i32::MAX as u32) as i32), clip.3.min(rows.min(i32::MAX as usize) as i32));
        if clip.0 >= clip.2 || clip.1 >= clip.3 {
            return advance;
        }
        let (ascent, _) = self.line_metrics(px);
        let baseline = (y_top + ascent).round();
        // glyph ink stays within about 1.5 em above and 1 em below the baseline
        if baseline - 1.6 * px > clip.3 as f32 || baseline + 1.1 * px < clip.1 as f32 {
            return advance;
        }
        let size_q = (px * 4.0).round().max(1.0) as u32;
        let whole_pixels = px >= WHOLE_PIXEL_PX;
        // COLR paint that asks for the text colour (palette index 0xFFFF)
        let fg = 0x0100_0000 | u32::from_be_bytes([0, color[0], color[1], color[2]]);
        let mut target = Target { dst, w: w as usize, clip };
        for p in line.pieces() {
            let p_left = x + p.x0 * px;
            if p_left + p.shaped.width * px + px < clip.0 as f32 || p_left - px > clip.2 as f32 {
                continue; // the whole piece is outside the clip
            }
            for g in &p.shaped.glyphs {
                let left = p_left + g.x * px;
                let right = left + g.w * px;
                if right + px < clip.0 as f32 || left - px > clip.2 as f32 {
                    continue;
                }
                let pen = p_left + (g.x + g.dx) * px;
                let (ix, bin) = if whole_pixels {
                    (pen.round() as i32, 0)
                } else {
                    let fl = pen.floor();
                    match ((pen - fl) * 4.0).round() as u8 {
                        b if b >= 4 => (fl as i32 + 1, 0),
                        b => (fl as i32, b),
                    }
                };
                let gy = (baseline - g.dy * px).round() as i32;
                let key = GlyphKey { font: g.font, id: g.id, size_q, bin, italic: g.italic, fg: 0 };
                let Some(mut bm) = self.glyph(key) else { continue };
                if bm.uses_fg {
                    match self.glyph(GlyphKey { fg, ..key }) {
                        Some(b) => bm = b,
                        None => continue,
                    }
                }
                if bm.direct {
                    if let Some((font, index)) = self.font(key.font) {
                        raster::draw_direct(&mut target, &font, index, key, ix, gy, color);
                    }
                } else if bm.w > 0 {
                    if bm.scale != 1.0 {
                        let (ox, oy) = (ix as f32 + bm.left as f32 * bm.scale, gy as f32 - bm.top as f32 * bm.scale);
                        raster::blit_scaled(&mut target, ox, oy, &bm, color);
                    } else {
                        raster::blit(&mut target, ix + bm.left, gy - bm.top, &bm, color);
                    }
                }
            }
        }
        advance
    }

    /// Cache occupancy and font counts.
    pub fn stats(&self) -> TextStats {
        let (a, b) = self.lines.stats();
        let (c, d) = self.flipped.stats();
        let (lines_cached, line_bytes) = (a + c, b + d);
        let (glyphs_cached, glyph_bytes) = self.glyphs.stats();
        TextStats {
            faces: self.faces,
            fonts_in_use: self.fonts.read().unwrap_or_else(PoisonError::into_inner).len(),
            lines_cached,
            line_bytes,
            glyphs_cached,
            glyph_bytes,
        }
    }

    /// Drop cached lines and glyph images (fonts stay loaded).
    pub fn clear_caches(&self) {
        self.lines.clear();
        self.flipped.clear();
        self.glyphs.clear();
    }
}

#[cfg(test)]
mod tests;
