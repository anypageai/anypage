//! Shaped lines in em units, and what editing needs from them: grapheme
//! boundaries with bidi-aware caret positions and per-grapheme widths.
//!
//! A line is shaped once at 1 px per em (cosmic-text's layout is linear in
//! the font size with hinting off), cached by its text, and scaled to any
//! pixel size afterwards. That is why the same string at a new zoom level
//! costs a hash lookup, not a reshape.
//!
//! Long lines are shaped in pieces, each cached on its own, and answered
//! piece by piece ([`ShapedLine`], [`Graphemes`]): a warm query on a long
//! paragraph is a few hash lookups plus work on the one piece it touches,
//! and an edit inside it reshapes only the piece that changed.

use std::sync::{Arc, OnceLock};

use unicode_segmentation::UnicodeSegmentation;

/// One positioned glyph, visual order, em units.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Glyph {
    /// Logical cluster: byte range in the original text.
    pub start: u32,
    pub end: u32,
    /// Left edge of the glyph's advance box and the advance.
    pub x: f32,
    pub w: f32,
    /// Placement offsets; `dy` positive moves up.
    pub dx: f32,
    pub dy: f32,
    /// Slot in the text system's font table.
    pub font: u16,
    pub id: u16,
    pub rtl: bool,
    pub italic: bool,
}

pub(crate) struct Shaped {
    pub glyphs: Vec<Glyph>,
    /// Advance of the whole line (em).
    pub width: f32,
    /// Paragraph direction (first strong character, UAX #9 P2/P3).
    pub rtl: bool,
    /// Characters no installed font has, drawn as U+FFFD, plus glyphs the
    /// shaper reported as missing.
    pub missing: u32,
    edit: OnceLock<Edit>,
}

/// Editing view of a shaped line.
pub(crate) struct Edit {
    /// Grapheme boundaries in logical order, from 0 to the text length.
    pub bounds: Vec<u32>,
    /// Caret x (em) at each boundary: the leading edge of the grapheme that
    /// starts there (its left edge if it runs left to right, its right edge
    /// if right to left); at the end, the trailing edge of the last one.
    pub carets: Vec<f32>,
    /// Visual extent (em) of each grapheme, for selection highlights.
    pub spans: Vec<(f32, f32)>,
    /// Prefix sums of grapheme advances (em), `bounds.len()` entries.
    pub pre: Vec<f32>,
    /// Paragraph direction: which end of the line is its logical end.
    pub rtl: bool,
}

impl Shaped {
    pub fn new(glyphs: Vec<Glyph>, width: f32, rtl: bool, missing: u32) -> Self {
        Self { glyphs, width, rtl, missing, edit: OnceLock::new() }
    }

    pub fn empty() -> Self {
        Self::new(Vec::new(), 0.0, false, 0)
    }

    /// `text` must be the text this line was shaped from.
    pub fn edit(&self, text: &str) -> &Edit {
        self.edit.get_or_init(|| Edit::new(self, text))
    }

    /// Approximate heap size, for the cache budget: the key, the glyphs and
    /// the editing tables built on first use (20 bytes per grapheme, at
    /// most one grapheme per byte).
    pub fn cost(&self, text_len: usize) -> usize {
        128 + text_len * 22 + self.glyphs.len() * std::mem::size_of::<Glyph>()
    }
}

struct Cluster {
    start: u32,
    end: u32,
    x0: f32,
    x1: f32,
    rtl: bool,
}

fn clusters(glyphs: &[Glyph], len: u32) -> Vec<Cluster> {
    let mut c: Vec<Cluster> = glyphs
        .iter()
        .map(|g| Cluster { start: g.start.min(len), end: g.end.min(len), x0: g.x, x1: g.x + g.w, rtl: g.rtl })
        .collect();
    c.sort_by_key(|k| k.start);
    let mut out: Vec<Cluster> = Vec::with_capacity(c.len());
    for k in c {
        match out.last_mut() {
            Some(last) if last.start == k.start => {
                last.x0 = last.x0.min(k.x0);
                last.x1 = last.x1.max(k.x1);
                last.end = last.end.max(k.end);
            }
            _ => out.push(k),
        }
    }
    for i in 1..out.len() {
        let next_start = out[i].start;
        let prev = &mut out[i - 1];
        if prev.end > next_start || prev.end <= prev.start {
            prev.end = next_start;
        }
    }
    if let Some(last) = out.last_mut() {
        if last.end <= last.start {
            last.end = len;
        }
    }
    out
}

impl Edit {
    fn new(shaped: &Shaped, text: &str) -> Self {
        let len = text.len() as u32;
        let mut bounds: Vec<u32> = text.grapheme_indices(true).map(|(i, _)| i as u32).collect();
        bounds.push(len);
        let n = bounds.len() - 1; // graphemes
        let cl = clusters(&shaped.glyphs, len);

        // Owner cluster of each grapheme (the one containing its start).
        let mut owner: Vec<Option<usize>> = vec![None; n];
        let mut count = vec![0u32; cl.len()];
        let mut first = vec![usize::MAX; cl.len()];
        let mut ci = 0;
        for (gi, own) in owner.iter_mut().enumerate() {
            let a = bounds[gi];
            while ci + 1 < cl.len() && cl[ci + 1].start <= a {
                ci += 1;
            }
            if let Some(c) = cl.get(ci) {
                if c.start <= a && a < c.end {
                    *own = Some(ci);
                    count[ci] += 1;
                    first[ci] = first[ci].min(gi);
                }
            }
        }

        let mut carets = vec![0.0f32; n + 1];
        let mut spans = vec![(0.0f32, 0.0f32); n];
        let mut widths = vec![0.0f32; n];
        // trailing edge of the last cluster starting at or before `a`
        let trailing_before = |a: u32| -> Option<f32> {
            let k = cl.partition_point(|c| c.start <= a);
            if k == 0 {
                return None;
            }
            let c = &cl[k - 1];
            Some(if c.rtl { c.x0 } else { c.x1 })
        };
        for gi in 0..n {
            match owner[gi] {
                Some(c_i) => {
                    let c = &cl[c_i];
                    let k = count[c_i].max(1) as f32;
                    let j = (gi - first[c_i]) as f32;
                    let cw = c.x1 - c.x0;
                    let (a, b) = (j / k, (j + 1.0) / k);
                    if c.rtl {
                        carets[gi] = c.x1 - a * cw;
                        spans[gi] = (c.x1 - b * cw, c.x1 - a * cw);
                    } else {
                        carets[gi] = c.x0 + a * cw;
                        spans[gi] = (c.x0 + a * cw, c.x0 + b * cw);
                    }
                    widths[gi] = cw / k;
                }
                None => {
                    let x = trailing_before(bounds[gi])
                        .or_else(|| cl.first().map(|c| if c.rtl { c.x1 } else { c.x0 }))
                        .unwrap_or(0.0);
                    carets[gi] = x;
                    spans[gi] = (x, x);
                }
            }
        }
        // Clusters that start inside a grapheme (rare: a font switch in the
        // middle of one) widen the grapheme that contains them.
        for (c_i, c) in cl.iter().enumerate() {
            if count[c_i] == 0 {
                let gi = bounds.partition_point(|&b| b <= c.start).saturating_sub(1).min(n.saturating_sub(1));
                if n > 0 {
                    widths[gi] += c.x1 - c.x0;
                    spans[gi] = (spans[gi].0.min(c.x0), spans[gi].1.max(c.x1));
                }
            }
        }
        carets[n] = cl.iter().max_by_key(|c| c.start).map(|c| if c.rtl { c.x0 } else { c.x1 }).unwrap_or(0.0);
        let mut pre = Vec::with_capacity(n + 1);
        let mut acc = 0.0f64;
        pre.push(0.0);
        for w in &widths {
            acc += *w as f64;
            pre.push(acc as f32);
        }
        Self { bounds, carets, spans, pre, rtl: shaped.rtl }
    }

    /// Index of the grapheme boundary at or before byte `b`.
    pub fn boundary_at_or_before(&self, b: usize) -> usize {
        self.bounds.partition_point(|&x| x as usize <= b).saturating_sub(1)
    }

    /// Caret x (em) for byte offset `b` (snapped to the grapheme start).
    pub fn caret(&self, b: usize) -> f32 {
        self.carets[self.boundary_at_or_before(b)]
    }

    /// Byte offset of the grapheme boundary whose caret is nearest to `x`
    /// (em). Past either end of the line it is the logical end on that side
    /// (the text length right of a left-to-right line, 0 left of it), like
    /// `TextRaster`: `x` = infinity, or so far out that every caret is
    /// equally far in f32, still finds the end.
    pub fn hit(&self, x: f32) -> usize {
        let len = self.bounds.last().copied().unwrap_or(0) as usize;
        let (lo, hi) = self.carets.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &c| (lo.min(c), hi.max(c)));
        if x > hi {
            return if self.rtl { 0 } else { len };
        }
        if x < lo {
            return if self.rtl { len } else { 0 };
        }
        let mut best = (f32::INFINITY, 0usize);
        for (i, &c) in self.carets.iter().enumerate() {
            let d = (c - x).abs();
            if d < best.0 {
                best = (d, i);
            }
        }
        self.bounds[best.1] as usize
    }

    /// Visual intervals (em, sorted, merged) covered by bytes `from..to`.
    pub fn selection(&self, from: usize, to: usize) -> Vec<(f32, f32)> {
        let (a, b) = (from.min(to), from.max(to));
        let mut v: Vec<(f32, f32)> = (0..self.spans.len())
            .filter(|&gi| {
                let s = self.bounds[gi] as usize;
                s >= a && s < b
            })
            .map(|gi| self.spans[gi])
            .filter(|s| s.1 > s.0)
            .collect();
        v.sort_by(|p, q| p.0.total_cmp(&q.0));
        let mut out: Vec<(f32, f32)> = Vec::with_capacity(v.len());
        for s in v {
            match out.last_mut() {
                Some(last) if s.0 <= last.1 + 1e-4 => last.1 = last.1.max(s.1),
                _ => out.push(s),
            }
        }
        out
    }
}

/// Bidi class B (paragraph separators): cosmic-text asserts that a line has
/// a single paragraph direction, so these never reach the shaper.
pub(crate) fn is_paragraph_separator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{1C}'..='\u{1E}' | '\u{85}' | '\u{2029}')
}

/// Default-ignorable code points: shapers hide them when the font lacks
/// them, so they must not count as missing (ZWJ in emoji sequences, bidi
/// controls, variation selectors).
pub(crate) fn is_default_ignorable(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x034F | 0x061C | 0x115F | 0x1160 | 0x17B4 | 0x17B5 | 0x180B..=0x180F
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0x3164 | 0xFE00..=0xFE0F
        | 0xFEFF | 0xFFA0 | 0xFFF0..=0xFFF8 | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A
        | 0xE0000..=0xE0FFF)
}

/// Text prepared for the shaper, with a map back to original byte offsets
/// when replacements changed lengths.
pub(crate) struct Prepared<'a> {
    pub text: std::borrow::Cow<'a, str>,
    /// Shaped byte index to original byte index (len + 1 entries), or
    /// `None` for the identity.
    pub map: Option<Vec<u32>>,
    pub substituted: u32,
}

impl Prepared<'_> {
    pub fn original(&self, i: usize) -> u32 {
        match &self.map {
            Some(m) => m.get(i).copied().unwrap_or_else(|| m.last().copied().unwrap_or(0)),
            None => i as u32,
        }
    }
}

/// Replace what the shaper must not see: paragraph separators become a
/// space, other control characters a zero-width space, and characters no
/// installed font covers U+FFFD (or `?`), so the font fallback never walks
/// every installed face looking for them.
pub(crate) fn prepare<'a>(s: &'a str, mut covered: impl FnMut(char) -> bool, replacement: &'static str) -> Prepared<'a> {
    let needs = |c: char| -> Option<&'static str> {
        if c == '\t' {
            None
        } else if is_paragraph_separator(c) {
            Some(" ")
        } else if c.is_control() {
            Some("\u{200B}")
        } else {
            None
        }
    };
    // fast scan: is anything to replace?
    let first = s.char_indices().find(|&(_, c)| needs(c).is_some() || (!c.is_ascii() && !is_default_ignorable(c) && !covered(c)));
    let Some((first_i, _)) = first else {
        return Prepared { text: std::borrow::Cow::Borrowed(s), map: None, substituted: 0 };
    };
    let mut out = String::with_capacity(s.len() + 16);
    let mut map: Vec<u32> = Vec::with_capacity(s.len() + 17);
    out.push_str(&s[..first_i]);
    map.extend(0..first_i as u32);
    let mut substituted = 0;
    for (i, c) in s[first_i..].char_indices() {
        let i = i + first_i;
        let rep = match needs(c) {
            Some(r) => Some(r),
            None if !c.is_ascii() && !is_default_ignorable(c) && !covered(c) => {
                substituted += 1;
                Some(replacement)
            }
            None => None,
        };
        match rep {
            Some(r) => {
                out.push_str(r);
                map.extend(std::iter::repeat_n(i as u32, r.len()));
            }
            None => {
                out.push(c);
                map.extend(i as u32..(i + c.len_utf8()) as u32);
            }
        }
    }
    map.push(s.len() as u32);
    Prepared { text: std::borrow::Cow::Owned(out), map: Some(map), substituted }
}

/// Split a long text into pieces of at most `max` bytes, after whitespace
/// when there is some in the second half of the piece, else at a grapheme
/// boundary.
pub(crate) fn chunks(s: &str, max: usize) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    while s.len() - start > max {
        let mut limit = start + max;
        while !s.is_char_boundary(limit) {
            limit -= 1;
        }
        let window = &s[start..limit];
        let cut = window
            .char_indices()
            .rev()
            .take_while(|&(i, _)| i >= max / 2)
            .find(|&(_, c)| c.is_whitespace())
            .map(|(i, c)| start + i + c.len_utf8())
            .or_else(|| window.grapheme_indices(true).next_back().map(|(i, _)| start + i).filter(|&c| c > start))
            .unwrap_or(limit);
        out.push(start..cut);
        start = cut;
    }
    out.push(start..s.len());
    out
}

/// One piece of a line: its first byte, its left edge (em) in the line and
/// its shaping.
pub(crate) struct Piece {
    pub start: usize,
    pub x0: f32,
    pub shaped: Arc<Shaped>,
}

/// A shaped line: one piece, or several laid side by side in logical order
/// (right to left when the first one runs right to left). Pieces stay
/// separate (they are cached separately), and queries go to the piece they
/// concern.
pub(crate) enum ShapedLine {
    One([Piece; 1]),
    Many { pieces: Vec<Piece>, width: f32, rtl: bool, missing: u32 },
}

impl ShapedLine {
    pub fn one(shaped: Arc<Shaped>) -> Self {
        Self::One([Piece { start: 0, x0: 0.0, shaped }])
    }

    /// `parts`: (first byte, shaping) in logical order.
    pub fn many(parts: Vec<(usize, Arc<Shaped>)>) -> Self {
        if parts.is_empty() {
            return Self::one(Arc::new(Shaped::empty()));
        }
        let rtl = parts.first().is_some_and(|p| p.1.rtl);
        let total: f64 = parts.iter().map(|p| p.1.width as f64).sum();
        let mut acc = 0.0f64;
        let mut missing = 0u32;
        let pieces = parts
            .into_iter()
            .map(|(start, shaped)| {
                let w = shaped.width as f64;
                let x0 = if rtl { total - acc - w } else { acc };
                acc += w;
                missing = missing.saturating_add(shaped.missing);
                Piece { start, x0: x0 as f32, shaped }
            })
            .collect();
        Self::Many { pieces, width: total as f32, rtl, missing }
    }

    pub fn pieces(&self) -> &[Piece] {
        match self {
            Self::One(p) => p,
            Self::Many { pieces, .. } => pieces,
        }
    }

    /// Advance of the whole line (em).
    pub fn width(&self) -> f32 {
        match self {
            Self::One([p]) => p.shaped.width,
            Self::Many { width, .. } => *width,
        }
    }

    pub fn rtl(&self) -> bool {
        match self {
            Self::One([p]) => p.shaped.rtl,
            Self::Many { rtl, .. } => *rtl,
        }
    }

    pub fn missing(&self) -> u32 {
        match self {
            Self::One([p]) => p.shaped.missing,
            Self::Many { missing, .. } => *missing,
        }
    }

    /// The text of piece `i`, given the line's text.
    pub fn piece_text<'t>(&self, i: usize, text: &'t str) -> &'t str {
        let ps = self.pieces();
        let Some(p) = ps.get(i) else { return "" };
        let end = ps.get(i + 1).map_or(text.len(), |q| q.start);
        text.get(p.start..end).unwrap_or("")
    }

    /// Caret x (em) for byte offset `b` (a char boundary of `text`).
    pub fn caret(&self, text: &str, b: usize) -> f32 {
        let ps = self.pieces();
        let i = ps.partition_point(|p| p.start <= b).saturating_sub(1);
        let p = &ps[i];
        p.x0 + p.shaped.edit(self.piece_text(i, text)).caret(b - p.start)
    }

    /// Byte offset of the grapheme boundary whose caret is nearest to `x`
    /// (em); see [`Edit::hit`].
    pub fn hit(&self, text: &str, x: f32) -> usize {
        let ps = self.pieces();
        let n = ps.len();
        let i = if n == 1 {
            0
        } else if x >= self.width() {
            if self.rtl() { 0 } else { n - 1 }
        } else if x <= 0.0 || x.is_nan() {
            if self.rtl() { n - 1 } else { 0 }
        } else {
            ps.iter().position(|p| x >= p.x0 && x <= p.x0 + p.shaped.width).unwrap_or(0)
        };
        let p = &ps[i];
        p.start + p.shaped.edit(self.piece_text(i, text)).hit(x - p.x0)
    }

    /// Visual intervals (em, sorted, merged) covered by bytes `from..to`.
    pub fn selection(&self, text: &str, from: usize, to: usize) -> Vec<(f32, f32)> {
        if let Self::One([p]) = self {
            return p.shaped.edit(text).selection(from, to);
        }
        let (a, b) = (from.min(to), from.max(to));
        let mut v: Vec<(f32, f32)> = Vec::new();
        for (i, p) in self.pieces().iter().enumerate() {
            let t = self.piece_text(i, text);
            let end = p.start + t.len();
            if b <= p.start || a >= end {
                continue;
            }
            let (la, lb) = (a.saturating_sub(p.start), (b - p.start).min(t.len()));
            v.extend(p.shaped.edit(t).selection(la, lb).into_iter().map(|(x, y)| (x + p.x0, y + p.x0)));
        }
        v.sort_by(|p, q| p.0.total_cmp(&q.0));
        let mut out: Vec<(f32, f32)> = Vec::with_capacity(v.len());
        for s in v {
            match out.last_mut() {
                Some(last) if s.0 <= last.1 + 1e-4 => last.1 = last.1.max(s.1),
                _ => out.push(s),
            }
        }
        out
    }
}

struct GraphemePart<'a> {
    start: usize,
    /// Index of the piece's first grapheme in the whole line.
    first: usize,
    /// Advance of everything before the piece (em).
    base: f64,
    edit: &'a Edit,
}

/// Grapheme boundaries and advance prefix sums of a whole line across its
/// pieces, for line breaking: no table is copied, each lookup goes to its
/// piece.
pub(crate) struct Graphemes<'a> {
    parts: Vec<GraphemePart<'a>>,
    n: usize,
    len: usize,
    total: f64,
}

impl<'a> Graphemes<'a> {
    /// `text` must be the text `line` was shaped from.
    pub fn new(line: &'a ShapedLine, text: &str) -> Self {
        let mut parts = Vec::with_capacity(line.pieces().len());
        let (mut first, mut base) = (0usize, 0.0f64);
        for (i, p) in line.pieces().iter().enumerate() {
            let edit = p.shaped.edit(line.piece_text(i, text));
            let n = edit.bounds.len().saturating_sub(1);
            parts.push(GraphemePart { start: p.start, first, base, edit });
            first += n;
            base += edit.pre.get(n).copied().unwrap_or(0.0) as f64;
        }
        Self { parts, n: first, len: text.len(), total: base }
    }

    /// Number of graphemes (boundaries are `0..=count()`).
    pub fn count(&self) -> usize {
        self.n
    }

    fn part(&self, gi: usize) -> &GraphemePart<'a> {
        let k = self.parts.partition_point(|p| p.first <= gi).saturating_sub(1);
        &self.parts[k]
    }

    /// Byte offset of boundary `gi`.
    pub fn bound(&self, gi: usize) -> usize {
        if gi >= self.n || self.parts.is_empty() {
            return self.len;
        }
        let p = self.part(gi);
        p.start + p.edit.bounds[gi - p.first] as usize
    }

    /// Summed advance of the graphemes before boundary `gi` (em).
    pub fn pre(&self, gi: usize) -> f64 {
        if gi >= self.n || self.parts.is_empty() {
            return self.total;
        }
        let p = self.part(gi);
        p.base + p.edit.pre[gi - p.first] as f64
    }

    /// Index of the boundary at byte `b`, if `b` is one.
    pub fn index_of(&self, b: usize) -> Option<usize> {
        if b >= self.len || self.parts.is_empty() {
            return (b == self.len).then_some(self.n);
        }
        let k = self.parts.partition_point(|p| p.start <= b).saturating_sub(1);
        let p = &self.parts[k];
        let local = u32::try_from(b - p.start).ok()?;
        p.edit.bounds.binary_search(&local).ok().map(|j| p.first + j)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(start: u32, end: u32, x: f32, w: f32, rtl: bool) -> Glyph {
        Glyph { start, end, x, w, dx: 0.0, dy: 0.0, font: 0, id: 1, rtl, italic: false }
    }

    #[test]
    fn ltr_carets_and_hits() {
        let s = Shaped::new(vec![g(0, 1, 0.0, 1.0, false), g(1, 2, 1.0, 2.0, false)], 3.0, false, 0);
        let e = s.edit("ab");
        assert_eq!(e.bounds, vec![0, 1, 2]);
        assert_eq!(e.carets, vec![0.0, 1.0, 3.0]);
        assert_eq!(e.hit(1.4), 1);
        assert_eq!(e.hit(2.6), 2);
        assert_eq!(e.hit(-5.0), 0);
        assert_eq!(e.pre, vec![0.0, 1.0, 3.0]);
    }

    #[test]
    fn rtl_carets_run_right_to_left() {
        // two RTL letters: logical 0 drawn on the right
        let s = Shaped::new(vec![g(2, 4, 0.0, 1.0, true), g(0, 2, 1.0, 1.5, true)], 2.5, true, 0);
        let e = s.edit("אב");
        assert_eq!(e.carets, vec![2.5, 1.0, 0.0]);
        assert_eq!(e.hit(2.4), 0);
        assert_eq!(e.hit(0.1), 4);
        assert_eq!(e.selection(0, 2), vec![(1.0, 2.5)]);
    }

    #[test]
    fn hit_past_the_ends_finds_the_logical_end() {
        let s = Shaped::new(vec![g(0, 1, 0.0, 1.0, false), g(1, 2, 1.0, 2.0, false)], 3.0, false, 0);
        let e = s.edit("ab");
        // 1e9 px at 16 px is 6.25e7 em: every caret is equally far in f32
        for x in [f32::INFINITY, 1e30, 6.25e7, 3.5] {
            assert_eq!(e.hit(x), 2, "x = {x}");
        }
        for x in [f32::NEG_INFINITY, -1e30, -6.25e7] {
            assert_eq!(e.hit(x), 0, "x = {x}");
        }
        // a trailing zero-width grapheme (ZWSP) is still before the end
        let z = Shaped::new(vec![g(0, 1, 0.0, 1.0, false), g(1, 4, 1.0, 0.0, false)], 1.0, false, 0);
        assert_eq!(z.edit("a\u{200B}").hit(f32::INFINITY), 4);
        // right to left: the logical end is on the left
        let r = Shaped::new(vec![g(2, 4, 0.0, 1.0, true), g(0, 2, 1.0, 1.5, true)], 2.5, true, 0);
        let e = r.edit("אב");
        for x in [f32::INFINITY, 1e30, 6.25e7] {
            assert_eq!(e.hit(x), 0, "x = {x}");
        }
        for x in [f32::NEG_INFINITY, -1e30, -6.25e7] {
            assert_eq!(e.hit(x), 4, "x = {x}");
        }
        // empty text
        assert_eq!(Shaped::empty().edit("").hit(f32::INFINITY), 0);
    }

    fn two(text_a: &str, rtl: bool) -> Arc<Shaped> {
        // two one-byte-or-more graphemes of 1 em each, visual order
        let mut it = text_a.char_indices();
        let (i0, c0) = it.next().expect("char");
        let (i1, c1) = it.next().expect("char");
        let (a, b) = (g(i0 as u32, (i0 + c0.len_utf8()) as u32, 0.0, 1.0, rtl), g(i1 as u32, (i1 + c1.len_utf8()) as u32, 1.0, 1.0, rtl));
        let glyphs = if rtl { vec![Glyph { x: 0.0, ..b }, Glyph { x: 1.0, ..a }] } else { vec![a, b] };
        Arc::new(Shaped::new(glyphs, 2.0, rtl, 0))
    }

    #[test]
    fn pieces_answer_like_one_line() {
        let text = "abcd";
        let line = ShapedLine::many(vec![(0, two("ab", false)), (2, two("cd", false))]);
        assert_eq!(line.width(), 4.0);
        assert!(!line.rtl());
        for b in 0..=4 {
            assert_eq!(line.caret(text, b), b as f32, "caret {b}");
        }
        assert_eq!(line.hit(text, 2.6), 3);
        assert_eq!(line.hit(text, 1.9), 2);
        assert_eq!(line.hit(text, f32::INFINITY), 4);
        assert_eq!(line.hit(text, f32::NEG_INFINITY), 0);
        assert_eq!(line.selection(text, 1, 3), vec![(1.0, 3.0)]);
        let g = Graphemes::new(&line, text);
        assert_eq!(g.count(), 4);
        for i in 0..=4 {
            assert_eq!(g.bound(i), i);
            assert_eq!(g.pre(i), i as f64);
            assert_eq!(g.index_of(i), Some(i));
        }
        assert_eq!(g.index_of(9), None);
    }

    #[test]
    fn right_to_left_pieces_start_on_the_right() {
        let text = "אבגד";
        let line = ShapedLine::many(vec![(0, two("אב", true)), (4, two("גד", true))]);
        assert!(line.rtl());
        // the first piece is drawn on the right
        assert_eq!(line.pieces()[0].x0, 2.0);
        assert_eq!(line.pieces()[1].x0, 0.0);
        let carets: Vec<f32> = [0, 2, 4, 6, 8].iter().map(|&b| line.caret(text, b)).collect();
        assert_eq!(carets, vec![4.0, 3.0, 2.0, 1.0, 0.0]);
        for (k, &b) in [0usize, 2, 4, 6, 8].iter().enumerate() {
            assert_eq!(line.hit(text, carets[k]), b, "hit at {}", carets[k]);
        }
        assert_eq!(line.hit(text, f32::INFINITY), 0);
        assert_eq!(line.hit(text, f32::NEG_INFINITY), 8);
        // "בג" spans both pieces: one interval in the middle
        assert_eq!(line.selection(text, 2, 6), vec![(1.0, 3.0)]);
        // no pieces at all: an empty line, never an empty piece list
        let none = ShapedLine::many(Vec::new());
        assert_eq!((none.pieces().len(), none.caret("", 0), none.hit("", 5.0)), (1, 0.0, 0));
    }

    #[test]
    fn ligature_cluster_splits_by_grapheme() {
        // "ffi" as one glyph 3 em wide: carets at thirds
        let s = Shaped::new(vec![g(0, 3, 0.0, 3.0, false)], 3.0, false, 0);
        let e = s.edit("ffi");
        assert_eq!(e.carets, vec![0.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn prepare_maps_offsets() {
        let p = prepare("a\nb\u{1B}c", |_| true, "\u{FFFD}");
        assert_eq!(p.text, "a b\u{200B}c");
        // 'c' sits at shaped byte 6, original byte 4
        assert_eq!(p.original(6), 4);
        assert_eq!(p.original(p.text.len()), 5);
        let q = prepare("plain", |_| true, "\u{FFFD}");
        assert!(q.map.is_none());
        let r = prepare("x\u{E000}y", |c| c != '\u{E000}', "?");
        assert_eq!(r.text, "x?y");
        assert_eq!(r.substituted, 1);
        assert_eq!(r.original(2), 4);
    }

    #[test]
    fn chunking_prefers_spaces() {
        let s = "aaaa bbbb cccc dddd";
        let c = chunks(s, 8);
        assert!(c.iter().all(|r| r.len() <= 8));
        assert_eq!(c.first(), Some(&(0..5)));
        assert_eq!(c.last().map(|r| r.end), Some(s.len()));
        let joined: String = c.iter().map(|r| &s[r.clone()]).collect();
        assert_eq!(joined, s);
        let cjk = "漢字漢字漢字漢字";
        let c = chunks(cjk, 7);
        assert!(c.iter().all(|r| cjk.is_char_boundary(r.start) && r.len() <= 7));
    }
}
