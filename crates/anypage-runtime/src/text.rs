//! Built-in `text` and `json` modules: what a text field or an agent needs
//! from the host, as pure functions.
//!
//! Positions are byte offsets in 0..=#s: the number of bytes before the
//! caret, so `s:sub(1, pos)` is the text before it. Movement is by grapheme
//! cluster (what a reader sees as one character) and by word.

use std::sync::Arc;

use anypage_engine::{Direction, TextRaster};

/// Page-facing text calls look at no more than this much text: a cold call
/// shapes all of it on the engine thread.
const MAX_TEXT: usize = 256 << 10;

fn dir_of(d: &Data) -> Direction {
    match d.as_str() {
        Some("rtl") => Direction::Rtl,
        Some("ltr") => Direction::Ltr,
        _ => Direction::Auto,
    }
}
use unicode_segmentation::UnicodeSegmentation;

use crate::data::Data;
use crate::natives::{Effect, Natives};

fn clamp_to_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos.min(s.len());
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

/// Next grapheme boundary after `pos`.
pub fn next_grapheme(s: &str, pos: usize) -> usize {
    let pos = clamp_to_boundary(s, pos);
    s[pos..].grapheme_indices(true).nth(1).map(|(i, _)| pos + i).unwrap_or(s.len())
}

/// Previous grapheme boundary before `pos`.
pub fn prev_grapheme(s: &str, pos: usize) -> usize {
    let pos = clamp_to_boundary(s, pos);
    s[..pos].grapheme_indices(true).next_back().map(|(i, _)| i).unwrap_or(0)
}

/// End of the next word after `pos` (skipping spaces first).
pub fn next_word(s: &str, pos: usize) -> usize {
    let pos = clamp_to_boundary(s, pos);
    for (i, w) in s[pos..].split_word_bound_indices() {
        if !w.trim().is_empty() {
            return pos + i + w.len();
        }
    }
    s.len()
}

/// Start of the word before `pos`.
pub fn prev_word(s: &str, pos: usize) -> usize {
    let pos = clamp_to_boundary(s, pos);
    for (i, w) in s[..pos].split_word_bound_indices().rev() {
        if !w.trim().is_empty() {
            return i;
        }
    }
    0
}

fn arg_str<'a>(c: &'a crate::natives::Call, i: usize) -> Result<&'a str, String> {
    let s = c.arg(i).as_str().ok_or_else(|| format!("argument {} must be a string", i + 1))?;
    Ok(&s[..clamp_to_boundary(s, MAX_TEXT)])
}

fn arg_num(c: &crate::natives::Call, i: usize, d: f64) -> f64 {
    c.arg(i).num_or(d)
}

/// Register `text.*` (needs the page's text system) and `json.*`.
pub fn register(n: &mut Natives, tr: Arc<TextRaster>) {
    let t = tr.clone();
    n.function("text.width", Effect::Pure, move |c| {
        let s = arg_str(c, 0)?;
        Ok(Data::Num(t.measure(s, arg_num(c, 1, 16.0).clamp(1.0, 400.0) as f32) as f64))
    });
    let t = tr.clone();
    n.function("text.line_height", Effect::Pure, move |c| {
        Ok(Data::Num(t.line_height(arg_num(c, 0, 16.0).clamp(1.0, 400.0) as f32) as f64))
    });
    // text.layout(s, size, width) -> { lines = {{from, to, width}}, line_height, height }
    let t = tr.clone();
    n.function("text.layout", Effect::Pure, move |c| {
        let s = arg_str(c, 0)?;
        let size = arg_num(c, 1, 16.0).clamp(1.0, 400.0) as f32;
        let width = arg_num(c, 2, f64::INFINITY).max(1.0) as f32;
        let lines = t.layout_lines(s, size, width);
        let lh = t.line_height(size) as f64;
        let list: Vec<Data> = lines
            .iter()
            .map(|l| {
                Data::map([
                    ("from", Data::Num(l.range.start as f64)),
                    ("to", Data::Num(l.range.end as f64)),
                    ("width", Data::Num(l.width as f64)),
                    ("rtl", Data::Bool(l.dir == Direction::Rtl)),
                ])
            })
            .collect();
        let n = list.len().max(1) as f64;
        Ok(Data::map([("lines", Data::list(list)), ("line_height", Data::Num(lh)), ("height", Data::Num(lh * n))]))
    });
    // text.caret(s, size, pos) -> x of the caret before byte `pos` on one line
    let t = tr.clone();
    n.function("text.caret", Effect::Pure, move |c| {
        let s = arg_str(c, 0)?;
        let size = arg_num(c, 1, 16.0).clamp(1.0, 400.0) as f32;
        let pos = clamp_to_boundary(s, arg_num(c, 2, 0.0).max(0.0) as usize);
        Ok(Data::Num(t.caret_x_dir(s, size, pos, dir_of(c.arg(3))) as f64))
    });
    // text.hit(s, size, x[, dir]) -> byte position nearest to x on one line
    let t = tr.clone();
    n.function("text.hit", Effect::Pure, move |c| {
        let s = arg_str(c, 0)?;
        let size = arg_num(c, 1, 16.0).clamp(1.0, 400.0) as f32;
        Ok(Data::Num(t.hit_x_dir(s, size, arg_num(c, 2, 0.0) as f32, dir_of(c.arg(3))) as f64))
    });
    // text.selection(s, size, from, to[, dir]) -> { {x0, x1}, ... } (bidi runs)
    let t = tr.clone();
    n.function("text.selection", Effect::Pure, move |c| {
        let s = arg_str(c, 0)?;
        let size = arg_num(c, 1, 16.0).clamp(1.0, 400.0) as f32;
        let from = clamp_to_boundary(s, arg_num(c, 2, 0.0).max(0.0) as usize);
        let to = clamp_to_boundary(s, arg_num(c, 3, 0.0).max(0.0) as usize);
        let rects = t.selection_rects_dir(s, size, from.min(to), from.max(to), dir_of(c.arg(4)));
        Ok(Data::list(rects.into_iter().map(|(a, b)| Data::nums([a as f64, b as f64])).collect()))
    });
    let t = tr.clone();
    n.function("text.is_rtl", Effect::Pure, move |c| Ok(Data::Bool(t.is_rtl(arg_str(c, 0)?))));
    // text.missing(s) -> how many characters no installed font can show
    let t = tr;
    n.function("text.missing", Effect::Pure, move |c| Ok(Data::Num(t.missing_glyphs(arg_str(c, 0)?) as f64)));
    n.function("text.next", Effect::Pure, |c| Ok(Data::Num(next_grapheme(arg_str(c, 0)?, arg_num(c, 1, 0.0).max(0.0) as usize) as f64)));
    n.function("text.prev", Effect::Pure, |c| Ok(Data::Num(prev_grapheme(arg_str(c, 0)?, arg_num(c, 1, 0.0).max(0.0) as usize) as f64)));
    n.function("text.next_word", Effect::Pure, |c| Ok(Data::Num(next_word(arg_str(c, 0)?, arg_num(c, 1, 0.0).max(0.0) as usize) as f64)));
    n.function("text.prev_word", Effect::Pure, |c| Ok(Data::Num(prev_word(arg_str(c, 0)?, arg_num(c, 1, 0.0).max(0.0) as usize) as f64)));
    // text.graphemes(s) -> number of user-perceived characters
    n.function("text.graphemes", Effect::Pure, |c| Ok(Data::Num(arg_str(c, 0)?.graphemes(true).count() as f64)));

    // text.decode(bytes[, max_bytes]) -> string: UTF-8 (BOM dropped) or
    // UTF-16 with a BOM; fails on anything else (binary files)
    n.function("text.decode", Effect::Pure, |c| {
        let b = c.arg(0).as_bytes().ok_or("text.decode needs bytes, a string or a byte resource")?;
        let max = c.arg(1).num_or(16.0 * 1048576.0).clamp(0.0, 64.0 * 1048576.0) as usize;
        if b.len() > max {
            return Err(format!("text.decode: {} bytes is more than the limit of {max}", b.len()));
        }
        decode_text(b).map(|s| Data::str(&s))
    });
    n.function("json.encode", Effect::Pure, |c| {
        let pretty = c.arg(1).truthy();
        let j = c.arg(0).to_json();
        let s = if pretty { serde_json::to_string_pretty(&j) } else { serde_json::to_string(&j) };
        s.map(|s| Data::str(&s)).map_err(|e| e.to_string())
    });
    n.function("json.decode", Effect::Pure, |c| {
        let s = arg_str(c, 0)?;
        let j: serde_json::Value = serde_json::from_str(s).map_err(|e| format!("json.decode: {e}"))?;
        Ok(Data::from_json(&j))
    });
}

/// UTF-8 (with or without BOM) or UTF-16 with a BOM.
pub fn decode_text(b: &[u8]) -> Result<String, String> {
    let utf16 = |le: bool, body: &[u8]| -> Result<String, String> {
        if body.len() % 2 != 0 {
            return Err("text.decode: odd number of bytes for UTF-16".into());
        }
        let units = body.chunks_exact(2).map(|p| if le { u16::from_le_bytes([p[0], p[1]]) } else { u16::from_be_bytes([p[0], p[1]]) });
        char::decode_utf16(units).collect::<Result<String, _>>().map_err(|_| "text.decode: invalid UTF-16".to_string())
    };
    match b {
        [0xEF, 0xBB, 0xBF, rest @ ..] => std::str::from_utf8(rest).map(str::to_string).map_err(|e| format!("text.decode: not UTF-8 (byte {})", e.valid_up_to() + 3)),
        [0xFF, 0xFE, rest @ ..] => utf16(true, rest),
        [0xFE, 0xFF, rest @ ..] => utf16(false, rest),
        _ => {
            let s = std::str::from_utf8(b).map_err(|e| format!("text.decode: not UTF-8 text (byte {})", e.valid_up_to()))?;
            if s.contains('\0') {
                return Err("text.decode: contains NUL bytes (a binary file?)".into());
            }
            Ok(s.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding() {
        assert_eq!(decode_text("字a".as_bytes()).unwrap(), "字a");
        assert_eq!(decode_text(&[0xEF, 0xBB, 0xBF, b'x']).unwrap(), "x");
        assert_eq!(decode_text(&[0xFF, 0xFE, b'h', 0, b'i', 0]).unwrap(), "hi");
        assert_eq!(decode_text(&[0xFE, 0xFF, 0, b'h']).unwrap(), "h");
        assert!(decode_text(&[0xC3, 0x28]).unwrap_err().contains("byte 0"));
        assert!(decode_text(b"a\0b").is_err());
    }

    #[test]
    fn graphemes_and_words() {
        let s = "e\u{301}字👍🏽 ok";
        assert_eq!(next_grapheme(s, 0), 3); // e + combining accent
        assert_eq!(next_grapheme(s, 3), 6); // 字
        assert_eq!(next_grapheme(s, 6), 14); // thumbs up + skin tone
        assert_eq!(prev_grapheme(s, 14), 6);
        assert_eq!(prev_grapheme(s, 3), 0);
        assert_eq!(next_grapheme(s, s.len()), s.len());
        let w = "hello  world foo";
        assert_eq!(next_word(w, 0), 5);
        assert_eq!(next_word(w, 5), 12);
        assert_eq!(prev_word(w, 12), 7);
        assert_eq!(prev_word(w, 3), 0);
        assert_eq!(clamp_to_boundary("字", 2), 0);
    }
}
