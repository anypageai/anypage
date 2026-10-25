//! TEXT, MTEXT and ATTRIB: control codes decoded, laid out as glyph runs in
//! font units, placed into the drawing with an affine transform.
//!
//! Approximations (documented in the crate docs): MTEXT inline formatting
//! (fonts, heights, colours, widths, stacking) is dropped and the text drawn
//! in the base height; stacked fractions are written `a/b`; the text height
//! is mapped to the font's cap height; SHX fonts are replaced by the system
//! outline font.

use crate::font::Font;
use crate::geom::{Aff, BBox, P};

/// Glyph id used for box glyphs when there is no font.
pub const BOX_GLYPH: u16 = u16::MAX;
const MAX_CHARS: usize = 20_000;
const MAX_LINES: usize = 2_000;

/// A run of glyphs in font units with its placement.
#[derive(Clone, Debug)]
pub struct TextRun {
    /// (glyph id, x, y) in font units, y up.
    pub glyphs: Vec<(u16, f64, f64)>,
    /// Font units → drawing units.
    pub xf: Aff,
    /// Drawing-space box of the laid-out text.
    pub bbox: BBox,
    /// Decoded text (lines joined with '\n').
    pub text: String,
    /// Cap height in drawing units.
    pub height: f64,
}

/// Font metrics, real or the fallback used without a font. Holds the
/// font's character map and advance table ready for lookups, plus a small
/// cache: drawings repeat the same few characters many times.
pub struct Metrics<'a> {
    tables: Option<(swash::Charmap<'a>, swash::GlyphMetrics<'a>)>,
    cache: std::cell::RefCell<std::collections::HashMap<char, (u16, f64)>>,
    pub cap: f64,
    pub descent: f64,
}

impl<'a> Metrics<'a> {
    pub fn new(font: Option<&'a Font>) -> Self {
        let cache = Default::default();
        match font {
            Some(f) => Metrics { tables: Some(f.tables()), cache, cap: f.cap_height, descent: f.descent },
            None => Metrics { tables: None, cache, cap: 700.0, descent: 200.0 },
        }
    }
    fn is_wide(ch: char) -> bool {
        matches!(ch as u32, 0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x20000..=0x3FFFD)
    }
    /// (glyph id, advance) for one character.
    pub fn glyph(&self, ch: char) -> (u16, f64) {
        let Some((cmap, gm)) = &self.tables else {
            return (BOX_GLYPH, if Self::is_wide(ch) { 1000.0 } else { 600.0 });
        };
        if let Some(&g) = self.cache.borrow().get(&ch) {
            return g;
        }
        let g = cmap.map(ch);
        let v = (g, gm.advance_width(g) as f64);
        let mut c = self.cache.borrow_mut();
        if c.len() < 65_536 {
            c.insert(ch, v);
        }
        v
    }
    pub fn width(&self, s: &str) -> f64 {
        s.chars().map(|c| self.glyph(c).1).sum()
    }
}

/// `%%d`, `%%p`, `%%c`, `%%nnn`, and dropping `%%u`/`%%o`/`%%k` toggles.
pub fn decode_percent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' && matches!(it.peek(), Some('U') | Some('u')) {
            let mut probe = it.clone();
            probe.next();
            if let Some(ch) = unicode_escape(&mut probe) {
                out.push(ch);
                it = probe;
                continue;
            }
        }
        if c == '%' && it.peek() == Some(&'%') {
            let mut probe = it.clone();
            probe.next();
            match probe.peek().copied() {
                Some('d') | Some('D') => {
                    out.push('°');
                    it = probe;
                    it.next();
                }
                Some('p') | Some('P') => {
                    out.push('±');
                    it = probe;
                    it.next();
                }
                Some('c') | Some('C') => {
                    out.push('Ø');
                    it = probe;
                    it.next();
                }
                Some('%') => {
                    out.push('%');
                    it = probe;
                    it.next();
                }
                Some('u') | Some('U') | Some('o') | Some('O') | Some('k') | Some('K') => {
                    it = probe;
                    it.next();
                }
                Some(d) if d.is_ascii_digit() => {
                    let mut code = 0u32;
                    let mut n = 0;
                    while n < 3 {
                        match probe.peek().and_then(|c| c.to_digit(10)) {
                            Some(v) => {
                                code = code * 10 + v;
                                probe.next();
                                n += 1;
                            }
                            None => break,
                        }
                    }
                    out.push(char::from_u32(code).unwrap_or('?'));
                    it = probe;
                }
                _ => out.push('%'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn unicode_escape(it: &mut std::iter::Peekable<std::str::Chars>) -> Option<char> {
    // after "\U": expects "+XXXX"
    if it.peek() != Some(&'+') {
        return None;
    }
    it.next();
    let mut code = 0u32;
    for _ in 0..4 {
        code = code * 16 + it.next()?.to_digit(16)?;
    }
    char::from_u32(code)
}

/// MTEXT content without formatting codes, split into paragraphs.
pub fn decode_mtext(raw: &str) -> Vec<String> {
    let mut lines = vec![String::new()];
    let mut it = raw.chars().peekable();
    let skip_to_semicolon = |it: &mut std::iter::Peekable<std::str::Chars>| {
        for c in it.by_ref() {
            if c == ';' {
                break;
            }
        }
    };
    while let Some(c) = it.next() {
        let cur = lines.last_mut().expect("at least one line");
        match c {
            '\\' => match it.next() {
                Some('P') | Some('X') => lines.push(String::new()),
                Some('~') => cur.push(' '),
                Some('\\') => cur.push('\\'),
                Some('{') => cur.push('{'),
                Some('}') => cur.push('}'),
                Some('U') | Some('u') => {
                    if let Some(ch) = unicode_escape(&mut it) {
                        cur.push(ch);
                    }
                }
                Some('S') => {
                    let mut stack = String::new();
                    for c in it.by_ref() {
                        if c == ';' {
                            break;
                        }
                        stack.push(c);
                    }
                    let parts: Vec<&str> = stack.split(['^', '/', '#']).collect();
                    let joined = parts.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/");
                    cur.push_str(&joined);
                }
                Some('f') | Some('F') | Some('H') | Some('W') | Some('Q') | Some('T') | Some('A') | Some('C') | Some('c') | Some('p')
                | Some('M') => skip_to_semicolon(&mut it),
                Some('L') | Some('l') | Some('O') | Some('o') | Some('K') | Some('k') | Some('N') => {}
                Some(other) => {
                    cur.push('\\');
                    cur.push(other);
                }
                None => {}
            },
            '{' | '}' => {}
            '^' => match it.peek().copied() {
                Some('I') => {
                    it.next();
                    cur.push_str("    ");
                }
                Some('J') | Some('M') => {
                    it.next();
                    lines.push(String::new());
                }
                Some(' ') => {
                    it.next();
                    cur.push('^');
                }
                _ => cur.push('^'),
            },
            '\n' => lines.push(String::new()),
            '\r' => {}
            '\t' => cur.push_str("    "),
            _ => cur.push(c),
        }
        if lines.len() > MAX_LINES {
            break;
        }
    }
    lines.into_iter().map(|l| decode_percent(&l)).collect()
}

fn breakable(ch: char) -> bool {
    Metrics::is_wide(ch)
}

/// Greedy line breaking of one paragraph at `max_w` font units.
pub fn wrap(m: &Metrics, line: &str, max_w: f64) -> Vec<String> {
    if max_w.is_nan() || max_w <= 0.0 || m.width(line) <= max_w {
        return vec![line.to_string()];
    }
    // tokens: words (with their trailing space) and single wide characters
    let mut tokens: Vec<String> = Vec::new();
    let mut word = String::new();
    for ch in line.chars() {
        if breakable(ch) {
            if !word.is_empty() {
                tokens.push(std::mem::take(&mut word));
            }
            tokens.push(ch.to_string());
        } else {
            word.push(ch);
            if ch == ' ' {
                tokens.push(std::mem::take(&mut word));
            }
        }
    }
    if !word.is_empty() {
        tokens.push(word);
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0.0;
    for t in tokens {
        let w = m.width(&t);
        if cur_w + m.width(t.trim_end()) <= max_w || cur.is_empty() {
            if cur.is_empty() && w > max_w && t.chars().count() > 1 {
                // a word longer than the line: break by characters
                for ch in t.chars() {
                    let cw = m.glyph(ch).1;
                    if cur_w + cw > max_w && !cur.is_empty() {
                        out.push(std::mem::take(&mut cur).trim_end().to_string());
                        cur_w = 0.0;
                    }
                    cur.push(ch);
                    cur_w += cw;
                }
                continue;
            }
            cur.push_str(&t);
            cur_w += w;
        } else {
            out.push(std::mem::take(&mut cur).trim_end().to_string());
            cur = t.trim_start().to_string();
            cur_w = m.width(&cur);
        }
        if out.len() > MAX_LINES {
            break;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur.trim_end().to_string());
    }
    out
}

fn clip_chars(s: &str) -> &str {
    match s.char_indices().nth(MAX_CHARS) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Glyphs for lines laid out top to bottom; each line is `(text, x offset)`
/// with baselines at `y0 - k * spacing`.
fn glyph_lines(m: &Metrics, lines: &[(String, f64)], y0: f64, spacing: f64) -> Vec<(u16, f64, f64)> {
    let mut out = Vec::new();
    let mut budget = MAX_CHARS;
    for (k, (line, x0)) in lines.iter().enumerate() {
        let y = y0 - k as f64 * spacing;
        let mut x = *x0;
        for ch in line.chars() {
            if budget == 0 {
                return out;
            }
            budget -= 1;
            let (g, adv) = m.glyph(ch);
            if !ch.is_whitespace() {
                out.push((g, x, y));
            }
            x += adv;
        }
    }
    out
}

fn run_bbox(m: &Metrics, lines: &[(String, f64)], y0: f64, spacing: f64, xf: &Aff) -> BBox {
    let mut b = BBox::EMPTY;
    for (k, (line, x0)) in lines.iter().enumerate() {
        let y = y0 - k as f64 * spacing;
        let w = m.width(line);
        if w <= 0.0 {
            continue;
        }
        let local = BBox::new(*x0, y - m.descent, x0 + w, y + m.cap * 1.15);
        b.union(&local.transform(xf));
    }
    b
}

/// Single-line TEXT / ATTRIB placement.
pub struct TextSpec {
    pub text: String,
    pub height: f64,
    pub width_factor: f64,
    pub oblique_deg: f64,
    pub rotation_deg: f64,
    /// 0 left, 1 center, 2 right, 3 aligned, 4 middle, 5 fit.
    pub halign: i32,
    /// 0 baseline, 1 bottom, 2 middle, 3 top.
    pub valign: i32,
    pub p1: P,
    pub p2: P,
    pub backwards: bool,
    pub upside_down: bool,
}

/// Lay out a TEXT entity; `ocs` maps its object coordinates to the drawing.
pub fn text_run(m: &Metrics, spec: &TextSpec, ocs: &Aff) -> Option<TextRun> {
    let text = decode_percent(clip_chars(&spec.text));
    let text = text.replace(['\r', '\n'], " ");
    if text.trim().is_empty() || !(spec.height.is_finite() && spec.height > 0.0) {
        return None;
    }
    let w = m.width(&text);
    let wf = if spec.width_factor.is_finite() && spec.width_factor > 0.0 { spec.width_factor } else { 1.0 };
    let mut sy = spec.height / m.cap;
    let mut sx = sy * wf;
    let mut rot = spec.rotation_deg.to_radians();
    let (mut dx, mut dy) = (0.0, 0.0);
    let mut anchor = spec.p1;
    let dir = spec.p2.sub(spec.p1);
    let fit = matches!(spec.halign, 3 | 5) && dir.len() > 1e-12 && w > 0.0;
    if fit {
        rot = dir.y.atan2(dir.x);
        sx = dir.len() / w;
        if spec.halign == 3 {
            sy = sx / wf;
        }
    } else {
        let h = if matches!(spec.halign, 3 | 5) { 0 } else { spec.halign };
        if h != 0 || spec.valign != 0 {
            anchor = spec.p2;
        }
        dx = match h {
            1 | 4 => -w / 2.0,
            2 => -w,
            _ => 0.0,
        };
        dy = if h == 4 {
            -m.cap / 2.0
        } else {
            match spec.valign {
                1 => m.descent,
                2 => -m.cap / 2.0,
                3 => -m.cap,
                _ => 0.0,
            }
        };
    }
    let skew = Aff { c: spec.oblique_deg.to_radians().tan().clamp(-10.0, 10.0), ..Aff::IDENTITY };
    let mirror = Aff::scale(if spec.backwards { -1.0 } else { 1.0 }, if spec.upside_down { -1.0 } else { 1.0 });
    let xf = ocs
        .mul(Aff::translate(anchor.x, anchor.y))
        .mul(Aff::rotate(rot))
        .mul(skew)
        .mul(Aff::scale(sx, sy))
        .mul(mirror)
        .mul(Aff::translate(dx, dy));
    if !xf.finite() {
        return None;
    }
    let lines = vec![(text.clone(), 0.0)];
    let glyphs = glyph_lines(m, &lines, 0.0, 0.0);
    let bbox = run_bbox(m, &lines, 0.0, 0.0, &xf);
    Some(TextRun { glyphs, xf, bbox, text, height: sy * m.cap })
}

/// MTEXT placement.
pub struct MTextSpec {
    pub raw: String,
    pub height: f64,
    pub insert: P,
    pub rotation_rad: f64,
    /// 1..9: top/middle/bottom × left/center/right.
    pub attachment: i32,
    pub ref_width: f64,
    pub line_spacing: f64,
}

pub fn mtext_run(m: &Metrics, spec: &MTextSpec, ocs: &Aff) -> Option<TextRun> {
    if !(spec.height.is_finite() && spec.height > 0.0) {
        return None;
    }
    let s = spec.height / m.cap;
    let max_w = if spec.ref_width.is_finite() && spec.ref_width > 0.0 { spec.ref_width / s * 1.0001 } else { 0.0 };
    let mut lines: Vec<String> = Vec::new();
    for para in decode_mtext(clip_chars(&spec.raw)) {
        lines.extend(wrap(m, &para, max_w));
        if lines.len() > MAX_LINES {
            break;
        }
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.iter().all(|l| l.trim().is_empty()) {
        return None;
    }
    let factor = if spec.line_spacing.is_finite() && spec.line_spacing > 0.0 { spec.line_spacing.clamp(0.25, 4.0) } else { 1.0 };
    let spacing = m.cap * 5.0 / 3.0 * factor;
    let total = m.cap + (lines.len() as f64 - 1.0) * spacing;
    let a = spec.attachment.clamp(1, 9);
    let y0 = -m.cap
        + match (a - 1) / 3 {
            0 => 0.0,
            1 => total / 2.0,
            _ => total,
        };
    let placed: Vec<(String, f64)> = lines
        .into_iter()
        .map(|l| {
            let w = m.width(&l);
            let x = match (a - 1) % 3 {
                0 => 0.0,
                1 => -w / 2.0,
                _ => -w,
            };
            (l, x)
        })
        .collect();
    let xf = ocs.mul(Aff::translate(spec.insert.x, spec.insert.y)).mul(Aff::rotate(spec.rotation_rad)).mul(Aff::scale(s, s));
    if !xf.finite() {
        return None;
    }
    let glyphs = glyph_lines(m, &placed, y0, spacing);
    let bbox = run_bbox(m, &placed, y0, spacing, &xf);
    let text = placed.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join("\n");
    Some(TextRun { glyphs, xf, bbox, text, height: spec.height })
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::p;

    #[test]
    fn percent_codes() {
        assert_eq!(decode_percent("45%%d %%p0.1 %%c20 100%%%"), "45° ±0.1 Ø20 100%");
        assert_eq!(decode_percent("%%uunder%%u"), "under");
        assert_eq!(decode_percent("%%065"), "A");
        assert_eq!(decode_percent(r"x \U+00B1 \U+4E2D"), "x ± 中");
        assert_eq!(decode_percent("5%"), "5%");
    }

    #[test]
    fn mtext_codes() {
        let lines = decode_mtext(r"{\fArial|b1;Title}\PLine \H2.5;two\~x \S1/2; and \\ \U+4E2D");
        assert_eq!(lines, vec!["Title".to_string(), "Line two x 1/2 and \\ 中".to_string()]);
        assert_eq!(decode_mtext(r"a\Pb\Pc").len(), 3);
        assert_eq!(decode_mtext(r"\A1;\C1;red"), vec!["red".to_string()]);
    }

    #[test]
    fn wrapping() {
        let m = Metrics::new(None);
        // fallback metrics: 600 units per Latin char
        let lines = wrap(&m, "aaa bbb ccc", 600.0 * 7.5);
        assert_eq!(lines, vec!["aaa bbb".to_string(), "ccc".to_string()]);
        let cjk = wrap(&m, "中文字測試", 2000.0);
        assert_eq!(cjk.len(), 3);
    }

    #[test]
    fn placement_without_font() {
        let m = Metrics::new(None);
        let spec = TextSpec {
            text: "AB".into(),
            height: 7.0,
            width_factor: 1.0,
            oblique_deg: 0.0,
            rotation_deg: 0.0,
            halign: 2,
            valign: 0,
            p1: p(0.0, 0.0),
            p2: p(100.0, 50.0),
            backwards: false,
            upside_down: false,
        };
        let run = text_run(&m, &spec, &Aff::IDENTITY).unwrap();
        // right aligned at p2: the text ends at x = 100, 2 chars * 600 units * 0.01
        assert!((run.bbox.x1 - 100.0).abs() < 1e-9, "{:?}", run.bbox);
        assert!((run.bbox.x0 - 88.0).abs() < 1e-9, "{:?}", run.bbox);
        assert!((run.height - 7.0).abs() < 1e-12);
    }
}
