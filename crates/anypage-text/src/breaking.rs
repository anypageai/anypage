//! Line breaking: UAX #14 opportunities, greedy fill, widths from shaping.

use std::ops::Range;

use unicode_linebreak::BreakOpportunity;

use crate::shape::Graphemes;

/// Break one paragraph (no '\n' inside) into lines no wider than `max` em.
/// Ranges are byte offsets into `text` plus `base`; widths are em.
///
/// - Breaks only where UAX #14 allows (spaces, between CJK ideographs but
///   not before closing punctuation such as "。", after hyphens, ...), and
///   always at its mandatory breaks (U+2028, U+2029, CR, VT, FF, NEL).
/// - Whitespace at a break belongs to neither line; the last line keeps
///   its trailing whitespace (it is content the caret can sit after).
/// - A word wider than `max` alone breaks between grapheme clusters.
pub(crate) fn paragraph(text: &str, base: usize, e: &Graphemes<'_>, max: f32, out: &mut Vec<(Range<usize>, f32)>) {
    let n = e.count();
    let w = |i: usize, j: usize| (e.pre(j) - e.pre(i)) as f32;
    let is_ws = |gi: usize| text.get(e.bound(gi)..).and_then(|t| t.chars().next()).is_some_and(char::is_whitespace);
    let push = |a: usize, b: usize, out: &mut Vec<(Range<usize>, f32)>| {
        out.push((base + e.bound(a)..base + e.bound(b), w(a, b)));
    };

    let mut line_start = 0usize; // grapheme index
    let mut line_end = 0usize; // end of the content placed so far
    let mut seg_start = 0usize;
    for (pos, op) in unicode_linebreak::linebreaks(text) {
        let Some(end) = e.index_of(pos) else { continue };
        if end <= seg_start {
            continue;
        }
        let mut body_end = end;
        while body_end > seg_start && is_ws(body_end - 1) {
            body_end -= 1;
        }
        if line_end > line_start && w(line_start, body_end) > max {
            push(line_start, line_end, out);
            line_start = seg_start;
        }
        if body_end > line_start && w(line_start, body_end) > max {
            // emergency break inside the word, between grapheme clusters
            let mut from = line_start;
            for gi in line_start..body_end {
                if gi > from && w(from, gi + 1) > max {
                    push(from, gi, out);
                    from = gi;
                }
            }
            line_start = from;
        }
        line_end = body_end;
        seg_start = end;
        if op == BreakOpportunity::Mandatory && pos < text.len() {
            push(line_start, line_end, out);
            line_start = end;
            line_end = end;
        }
    }
    push(line_start, n, out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shape::{Glyph, Shaped, ShapedLine};
    use std::sync::Arc;

    /// Monospace stand-in: every char 1 em, logical order.
    fn mono(text: &str) -> Shaped {
        let glyphs = text
            .char_indices()
            .map(|(i, c)| Glyph { start: i as u32, end: (i + c.len_utf8()) as u32, x: 0.0, w: 1.0, dx: 0.0, dy: 0.0, font: 0, id: 1, rtl: false, italic: false })
            .scan(0.0f32, |x, mut g| {
                g.x = *x;
                *x += 1.0;
                Some(g)
            })
            .collect::<Vec<_>>();
        let w = glyphs.len() as f32;
        Shaped::new(glyphs, w, false, 0)
    }

    fn lines(text: &str, max: f32) -> Vec<(String, f32)> {
        let line = ShapedLine::one(Arc::new(mono(text)));
        let mut out = Vec::new();
        paragraph(text, 0, &Graphemes::new(&line, text), max, &mut out);
        out.into_iter().map(|(r, w)| (text[r].to_string(), w)).collect()
    }

    /// The same text shaped as pieces breaks the same way.
    #[test]
    fn pieces_break_like_one_line() {
        let text = "alpha beta gamma delta epsilon zeta eta theta";
        let ranges = crate::shape::chunks(text, 12);
        assert!(ranges.len() > 2);
        let parts = ranges.iter().map(|r| (r.start, Arc::new(mono(&text[r.clone()])))).collect();
        let line = ShapedLine::many(parts);
        for max in [5.0, 11.0, 17.0, 100.0] {
            let mut out = Vec::new();
            paragraph(text, 0, &Graphemes::new(&line, text), max, &mut out);
            let got: Vec<(String, f32)> = out.into_iter().map(|(r, w)| (text[r].to_string(), w)).collect();
            assert_eq!(got, lines(text, max), "max {max}");
        }
    }

    #[test]
    fn words_and_spaces() {
        assert_eq!(lines("hello world", 20.0), vec![("hello world".into(), 11.0)]);
        assert_eq!(lines("hello world", 8.0), vec![("hello".into(), 5.0), ("world".into(), 5.0)]);
        // trailing space of the last line is kept
        assert_eq!(lines("ab cd ", 3.0), vec![("ab".into(), 2.0), ("cd ".into(), 3.0)]);
        // several spaces at a break vanish
        assert_eq!(lines("ab    cd", 3.0), vec![("ab".into(), 2.0), ("cd".into(), 2.0)]);
    }

    #[test]
    fn long_word_breaks_by_grapheme() {
        let l = lines("abcdefghij", 4.0);
        assert_eq!(l.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["abcd", "efgh", "ij"]);
        let l = lines("x abcdefghij", 4.0);
        assert_eq!(l.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["x", "abcd", "efgh", "ij"]);
    }

    #[test]
    fn cjk_breaks_between_ideographs_not_before_closing_punctuation() {
        let l = lines("漢字漢字漢字", 2.5);
        assert_eq!(l.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["漢字", "漢字", "漢字"]);
        let l = lines("漢字。漢字", 2.5);
        assert!(l.iter().all(|x| !x.0.starts_with('。')), "{l:?}");
    }

    #[test]
    fn mandatory_breaks_inside_a_paragraph() {
        let l = lines("ab\u{2028}cd", 100.0);
        assert_eq!(l.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["ab", "cd"]);
    }

    #[test]
    fn zero_width_keeps_one_grapheme_per_line() {
        let l = lines("abc", 0.0);
        assert_eq!(l.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["a", "b", "c"]);
    }
}
