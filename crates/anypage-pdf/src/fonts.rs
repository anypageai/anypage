//! Fonts for text this module writes into PDFs.
//!
//! * Latin text: the standard 14 Type 1 fonts (Helvetica, Times, Courier and
//!   their styles) in WinAnsiEncoding. Every PDF reader has them, so nothing is
//!   embedded. Their advance widths (the Adobe AFM metrics) are read once from
//!   hayro itself by interpreting a probe document, and written into each font
//!   dictionary's `/Widths`, so our layout, hayro and every other reader agree.
//! * CJK text: a Type 0 font over one of Adobe's CJK character collections
//!   (CNS1, GB1, Japan1, Korea1) with its predefined UTF-16 CMap, not
//!   embedded: the reader supplies the glyphs (Acrobat's Asian fonts, pdf.js
//!   and PDFium substitutes; this module's renderer uses a system CJK font).
//!   Full-width advance (1000/em). A `ToUnicode` CMap keeps the text extractable
//!   by any tool.
//! * Anything else that no collection covers is written as `?`.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use hayro::hayro_interpret::font::GlyphRun;
use hayro::hayro_interpret::hayro_cmap::{self, CMap, CMapName};
use hayro::hayro_interpret::{
    interpret_page, BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterCache, SoftMask,
};
use hayro::kurbo::{Affine, BezPath, Rect as KRect};
use lopdf::{dictionary, Dictionary, Document, Object, Stream};

use crate::util::{text_string, R};

/// The standard Latin fonts this module writes, by PostScript name.
pub const STANDARD: [&str; 12] = [
    "Helvetica",
    "Helvetica-Bold",
    "Helvetica-Oblique",
    "Helvetica-BoldOblique",
    "Times-Roman",
    "Times-Bold",
    "Times-Italic",
    "Times-BoldItalic",
    "Courier",
    "Courier-Bold",
    "Courier-Oblique",
    "Courier-BoldOblique",
];

/// Where the baseline sits below the top of a line box, in em.
pub const ASCENT: f64 = 0.8;

/// Resolve a script's font choice ("Helvetica", "serif", "mono", ... with
/// bold/italic flags) to a standard font name.
pub fn pick_standard(font: &str, bold: bool, italic: bool) -> R<&'static str> {
    let f = font.to_ascii_lowercase();
    let family = match f.as_str() {
        "" | "helvetica" | "sans" | "sans-serif" | "arial" => 0,
        "times" | "times-roman" | "serif" | "times new roman" => 1,
        "courier" | "mono" | "monospace" | "courier new" => 2,
        _ => {
            if let Some(i) = STANDARD.iter().position(|s| s.eq_ignore_ascii_case(font)) {
                return Ok(STANDARD[i]);
            }
            return Err(format!("unknown font \"{font}\" (use Helvetica, Times-Roman, Courier, or sans, serif, mono; with bold/italic)"));
        }
    };
    let style = match (bold, italic) {
        (false, false) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (true, true) => 3,
    };
    Ok(STANDARD[family * 4 + style])
}

const CP1252_80: [u16; 32] = [
    0x20AC, 0, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0, 0x017D, 0, 0, 0x2018, 0x2019,
    0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0, 0x017E, 0x0178,
];

/// The WinAnsiEncoding code of a character, if it has one.
pub fn win_ansi(c: char) -> Option<u8> {
    let u = c as u32;
    match u {
        0x20..=0x7E | 0xA1..=0xFF => Some(u as u8),
        0xA0 => Some(0x20),
        _ => CP1252_80.iter().position(|&x| x != 0 && x as u32 == u).map(|i| 0x80 + i as u8),
    }
}

// ---------------------------------------------------------------- standard widths

/// Records advance widths, glyph by glyph.
struct Widths(Vec<f32>);

impl<'a> Device<'a> for Widths {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
    fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {
        for g in run.glyphs() {
            let w = match &**g {
                hayro::hayro_interpret::font::Glyph::Outline(o) => o.advance_width().unwrap_or(0.0),
                _ => 0.0,
            };
            self.0.push(w);
        }
    }
}

type WidthTable = Arc<[f32; 256]>;

/// Codes 32..=255 shown in each standard font, read back through hayro.
fn probe() -> HashMap<&'static str, WidthTable> {
    let mut doc = crate::doc::new_document(612.0, 792.0, 1);
    let mut fonts = Dictionary::new();
    let mut ops = Vec::new();
    let codes: Vec<u8> = (32u8..=255).collect();
    let hex: String = codes.iter().map(|c| format!("{c:02X}")).collect();
    for (k, name) in STANDARD.iter().enumerate() {
        let id = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => *name, "Encoding" => "WinAnsiEncoding" });
        fonts.set(format!("F{k}"), id);
        ops.extend_from_slice(format!("BT /F{k} 1000 Tf 0 0 Td <{hex}> Tj ET\n").as_bytes());
    }
    let mut out = HashMap::new();
    let Some(page_id) = doc.page_iter().next() else { return out };
    let content = doc.add_object(Stream::new(Dictionary::new(), ops));
    if let Ok(p) = doc.get_dictionary_mut(page_id) {
        p.set("Resources", dictionary! { "Font" => fonts });
        p.set("Contents", content);
    }
    let Ok(bytes) = crate::doc::serialize(&mut doc) else { return out };
    let Ok(pdf) = hayro::hayro_syntax::Pdf::new(Arc::new(bytes)) else { return out };
    let Some(page) = pdf.pages().first() else { return out };
    let cache = InterpreterCache::new();
    let mut ctx = Context::new(Affine::IDENTITY, KRect::new(0.0, 0.0, 612.0, 792.0), &cache, page.xref(), crate::render::interpreter_settings());
    let mut dev = Widths(Vec::new());
    interpret_page(page, &mut ctx, &mut dev);
    let n = codes.len();
    for (k, name) in STANDARD.iter().enumerate() {
        let Some(ws) = dev.0.get(k * n..(k + 1) * n) else { break };
        let mut t = [0f32; 256];
        for (c, w) in codes.iter().zip(ws) {
            t[*c as usize] = *w;
        }
        out.insert(*name, Arc::new(t));
    }
    out
}

/// Advance widths (1/1000 em) of a standard font for codes 0..=255.
pub fn widths(font: &str) -> WidthTable {
    static T: OnceLock<HashMap<&'static str, WidthTable>> = OnceLock::new();
    let t = T.get_or_init(|| crate::guard("font metrics", || Ok(probe())).unwrap_or_default());
    t.get(font).cloned().unwrap_or_else(|| {
        // hayro could not be probed: Courier-like metrics keep layout sane
        Arc::new([600.0; 256])
    })
}

// ---------------------------------------------------------------- CJK

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cjk {
    Cns1,
    Gb1,
    Japan1,
    Korea1,
}

impl Cjk {
    pub const ALL: [Cjk; 4] = [Cjk::Cns1, Cjk::Gb1, Cjk::Japan1, Cjk::Korea1];

    pub fn from_lang(lang: &str) -> Option<Cjk> {
        let l = lang.to_ascii_lowercase();
        Some(match l.as_str() {
            "zh-hant" | "zh-tw" | "zh-hk" | "tc" | "cns1" => Cjk::Cns1,
            "zh-hans" | "zh-cn" | "zh" | "sc" | "gb1" => Cjk::Gb1,
            "ja" | "jp" | "japan1" => Cjk::Japan1,
            "ko" | "kr" | "korea1" => Cjk::Korea1,
            _ => return None,
        })
    }
    fn cmap_name(self) -> CMapName<'static> {
        match self {
            Cjk::Cns1 => CMapName::UniCnsUtf16H,
            Cjk::Gb1 => CMapName::UniGbUtf16H,
            Cjk::Japan1 => CMapName::UniJisUtf16H,
            Cjk::Korea1 => CMapName::UniKsUtf16H,
        }
    }
    pub fn encoding(self) -> &'static str {
        match self {
            Cjk::Cns1 => "UniCNS-UTF16-H",
            Cjk::Gb1 => "UniGB-UTF16-H",
            Cjk::Japan1 => "UniJIS-UTF16-H",
            Cjk::Korea1 => "UniKS-UTF16-H",
        }
    }
    /// The font name readers know for this collection.
    pub fn base_font(self) -> &'static str {
        match self {
            Cjk::Cns1 => "MSung-Light",
            Cjk::Gb1 => "STSong-Light",
            Cjk::Japan1 => "HeiseiMin-W3",
            Cjk::Korea1 => "HYSMyeongJo-Medium",
        }
    }
    fn ordering(self) -> (&'static str, i64) {
        match self {
            Cjk::Cns1 => ("CNS1", 4),
            Cjk::Gb1 => ("GB1", 4),
            Cjk::Japan1 => ("Japan1", 4),
            Cjk::Korea1 => ("Korea1", 2),
        }
    }
    pub fn key(self) -> String {
        format!("cjk:{}", self.ordering().0)
    }

    fn cmap(self) -> Option<&'static CMap> {
        static M: OnceLock<HashMap<Cjk, Option<CMap>>> = OnceLock::new();
        M.get_or_init(|| {
            Cjk::ALL
                .iter()
                .map(|c| {
                    let m = hayro_cmap::load_embedded(c.cmap_name()).and_then(|b| CMap::parse(b, hayro_cmap::load_embedded));
                    (*c, m)
                })
                .collect()
        })
        .get(&self)?
        .as_ref()
    }

    /// The CID of a character in this collection (0 = not covered).
    pub fn cid(self, c: char) -> u32 {
        let Some(m) = self.cmap() else { return 0 };
        let mut buf = [0u16; 2];
        let units = c.encode_utf16(&mut buf);
        let (code, len) = match units.len() {
            1 => (units[0] as u32, 2),
            _ => (((units[0] as u32) << 16) | units[1] as u32, 4),
        };
        m.lookup_cid_code(code, len).unwrap_or(0)
    }

    pub fn covers(self, c: char) -> bool {
        self.cid(c) != 0
    }
}

pub fn is_kana(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F)
}

pub fn is_hangul(c: char) -> bool {
    matches!(c as u32, 0xAC00..=0xD7AF | 0x1100..=0x11FF | 0x3130..=0x318F)
}

/// The collection for a piece of text: the language asked for if it covers
/// every character, else kana means Japanese, Hangul Korean, otherwise
/// Traditional then Simplified Chinese.
pub fn pick_cjk(text: &str, lang: Option<Cjk>) -> Option<Cjk> {
    pick_cjk_of(|| text.chars(), lang)
}

/// [`pick_cjk`] over the characters `chars` yields (it is called once per
/// candidate collection), so callers need not build a string.
fn pick_cjk_of<I: Iterator<Item = char>>(chars: impl Fn() -> I, lang: Option<Cjk>) -> Option<Cjk> {
    let mut order: [Option<Cjk>; 7] = [None; 7];
    order[0] = lang;
    if chars().any(is_kana) {
        order[1] = Some(Cjk::Japan1);
    }
    if chars().any(is_hangul) {
        order[2] = Some(Cjk::Korea1);
    }
    for (k, c) in Cjk::ALL.iter().enumerate() {
        order[3 + k] = Some(*c);
    }
    order.into_iter().flatten().find(|c| chars().all(|ch| c.covers(ch)))
}

/// The collection for one character not in WinAnsi, without allocating.
fn pick_cjk_char(c: char, lang: Option<Cjk>) -> Option<Cjk> {
    if let Some(l) = lang.filter(|l| l.covers(c)) {
        return Some(l);
    }
    if is_kana(c) && Cjk::Japan1.covers(c) {
        return Some(Cjk::Japan1);
    }
    if is_hangul(c) && Cjk::Korea1.covers(c) {
        return Some(Cjk::Korea1);
    }
    Cjk::ALL.into_iter().find(|k| k.covers(c))
}

/// A Type 0 font for `c`, not embedded, with an identity ToUnicode map.
pub fn cjk_font_object(doc: &mut Document, c: Cjk) -> lopdf::ObjectId {
    let (ordering, supplement) = c.ordering();
    let descriptor = doc.add_object(dictionary! {
        "Type" => "FontDescriptor",
        "FontName" => c.base_font(),
        "Flags" => 6,
        "FontBBox" => vec![(-160).into(), (-250).into(), 1015.into(), 1070.into()],
        "ItalicAngle" => 0,
        "Ascent" => 880,
        "Descent" => -120,
        "CapHeight" => 880,
        "StemV" => 93,
    });
    let cid_font = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "CIDFontType0",
        "BaseFont" => c.base_font(),
        "CIDSystemInfo" => dictionary! { "Registry" => text_string("Adobe"), "Ordering" => text_string(ordering), "Supplement" => supplement },
        "FontDescriptor" => descriptor,
        "DW" => 1000,
    });
    let mut stream = Stream::new(Dictionary::new(), identity_to_unicode());
    let _ = stream.compress();
    let to_unicode = doc.add_object(stream);
    doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type0",
        "BaseFont" => c.base_font(),
        "Encoding" => c.encoding(),
        "DescendantFonts" => vec![cid_font.into()],
        "ToUnicode" => to_unicode,
    })
}

/// UTF-16 code to the same code point, for the BMP.
fn identity_to_unicode() -> Vec<u8> {
    let mut s = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /AnyPage-UTF16 def\n/CMapType 2 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    // 256 ranges, 100 per section; surrogates are left out
    let hi: Vec<u32> = (0u32..=0xFF).filter(|h| !(0xD8..=0xDF).contains(h)).collect();
    for chunk in hi.chunks(100) {
        s.push_str(&format!("{} beginbfrange\n", chunk.len()));
        for h in chunk {
            s.push_str(&format!("<{h:02X}00> <{h:02X}FF> <{h:02X}00>\n"));
        }
        s.push_str("endbfrange\n");
    }
    s.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    s.into_bytes()
}

/// A standard Latin font dictionary with explicit widths.
pub fn standard_font_object(doc: &mut Document, name: &str) -> lopdf::ObjectId {
    let w = widths(name);
    let widths: Vec<Object> = (32..=255).map(|c| Object::Integer(w[c].round() as i64)).collect();
    doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => name,
        "Encoding" => "WinAnsiEncoding",
        "FirstChar" => 32,
        "LastChar" => 255,
        "Widths" => widths,
    })
}

// ---------------------------------------------------------------- runs

#[derive(Clone, Debug, PartialEq)]
pub enum Run {
    /// WinAnsi codes in a standard font.
    Latin(Vec<u8>),
    /// UTF-16BE codes in a CJK font.
    Cjk(Cjk, Vec<u8>),
}

fn push_latin(out: &mut Vec<Run>, b: u8) {
    match out.last_mut() {
        Some(Run::Latin(v)) => v.push(b),
        _ => out.push(Run::Latin(vec![b])),
    }
}

/// Split text into font runs: WinAnsi codes for the standard font, UTF-16BE
/// codes per CJK collection, `?` for characters nothing covers. Linear in the
/// text: the collection for the whole text is chosen once.
pub fn runs(text: &str, lang: Option<Cjk>) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    let wide = || text.chars().filter(|c| win_ansi(*c).is_none());
    let cjk = if wide().next().is_none() { None } else { pick_cjk_of(wide, lang) };
    for c in text.chars() {
        if let Some(b) = win_ansi(c) {
            push_latin(&mut out, b);
            continue;
        }
        // the chosen collection, else the best one for this character
        match cjk.filter(|k| k.covers(c)).or_else(|| pick_cjk_char(c, lang)) {
            Some(k) => {
                let mut buf = [0u16; 2];
                let units = c.encode_utf16(&mut buf);
                let v = match out.last_mut() {
                    Some(Run::Cjk(kk, v)) if *kk == k => v,
                    _ => {
                        out.push(Run::Cjk(k, Vec::new()));
                        match out.last_mut() {
                            Some(Run::Cjk(_, v)) => v,
                            _ => continue,
                        }
                    }
                };
                for u in units.iter() {
                    v.extend_from_slice(&u.to_be_bytes());
                }
            }
            None => push_latin(&mut out, b'?'),
        }
    }
    out
}

/// Advance of one character in 1/1000 em, as [`runs`] writes it: WinAnsi in
/// the standard font, full width when a CJK collection covers it, else the
/// width of the `?` it becomes.
fn char_units(c: char, w: &[f32; 256]) -> f64 {
    match win_ansi(c) {
        Some(b) => w[b as usize] as f64,
        None if Cjk::ALL.iter().any(|k| k.covers(c)) => 1000.0,
        None => w[b'?' as usize] as f64,
    }
}

/// Width of runs in points at `size`, with `latin` the standard font.
pub fn runs_width(runs: &[Run], latin: &str, size: f64) -> f64 {
    let w = widths(latin);
    runs.iter()
        .map(|r| match r {
            Run::Latin(codes) => codes.iter().map(|&c| w[c as usize] as f64).sum::<f64>(),
            // full width per character; a surrogate pair is one character
            Run::Cjk(_, b) => {
                let units = b.chunks(2).map(|p| u16::from_be_bytes([p[0], p.get(1).copied().unwrap_or(0)]));
                char::decode_utf16(units).count() as f64 * 1000.0
            }
        })
        .sum::<f64>()
        * size
        / 1000.0
}

/// Width of text in points: the same number [`runs_width`] gives for its
/// runs, without building them.
pub fn text_width(text: &str, latin: &str, size: f64, _lang: Option<Cjk>) -> f64 {
    let w = widths(latin);
    text.chars().map(|c| char_units(c, &w)).sum::<f64>() * size / 1000.0
}

/// Greedy wrapping to `max_w` points: at spaces, between CJK characters, and
/// inside a word only when the word alone is too wide.
///
/// Linear in the text: every character is measured once (twice inside a word
/// that has to be broken), and the line keeps a running width.
pub fn wrap(text: &str, latin: &str, size: f64, max_w: f64, _lang: Option<Cjk>) -> Vec<String> {
    let w = widths(latin);
    // widths are sums of whole 1/1000 em units, so comparing them is exact
    let fits = |units: f64| units * size / 1000.0 <= max_w;
    let mut out = Vec::new();
    for para in text.split('\n') {
        // tokens: words with their trailing space, or single wide characters
        let mut tokens: Vec<(usize, usize)> = Vec::new();
        let mut start = 0;
        for (i, c) in para.char_indices() {
            let end = i + c.len_utf8();
            if win_ansi(c).is_none() && !c.is_whitespace() {
                if start < i {
                    tokens.push((start, i));
                }
                tokens.push((i, end));
                start = end;
            } else if c == ' ' {
                tokens.push((start, end));
                start = end;
            }
        }
        if start < para.len() {
            tokens.push((start, para.len()));
        }
        // the current line is para[line_start..line_end]: `units` wide, `ink`
        // wide without its trailing whitespace
        let (mut line_start, mut line_end, mut units, mut ink) = (0usize, 0usize, 0.0f64, 0.0f64);
        let mut empty = true;
        for (ts, te) in tokens {
            let tok = &para[ts..te];
            let (mut full, mut trimmed, mut has_ink) = (0.0, 0.0, false);
            for c in tok.chars() {
                full += char_units(c, &w);
                if !c.is_whitespace() {
                    (trimmed, has_ink) = (full, true);
                }
            }
            // whitespace never starts a new line: it only widens the current one
            if !empty && has_ink && !fits(units + trimmed) {
                out.push(para[line_start..line_end].trim_end().to_string());
                empty = true;
            }
            if empty {
                (line_start, units, ink, empty) = (ts, 0.0, 0.0, false);
            }
            if has_ink {
                ink = units + trimmed;
            }
            line_end = te;
            units += full;
            if !fits(ink) {
                // one token wider than the line on its own: break it by characters
                let (mut piece, mut piece_units, mut piece_ink) = (ts, 0.0, 0.0);
                for (off, c) in tok.char_indices() {
                    let cu = char_units(c, &w);
                    if piece < ts + off && !fits(piece_units + cu) {
                        out.push(para[piece..ts + off].trim_end().to_string());
                        (piece, piece_units, piece_ink) = (ts + off, 0.0, 0.0);
                    }
                    piece_units += cu;
                    if !c.is_whitespace() {
                        piece_ink = piece_units;
                    }
                }
                (line_start, units, ink) = (piece, piece_units, piece_ink);
            }
        }
        out.push(para[line_start..line_end].trim_end().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_widths_come_from_the_afm_metrics() {
        let h = widths("Helvetica");
        assert_eq!((h[b'H' as usize], h[b'e' as usize], h[b'l' as usize], h[b'o' as usize]), (722.0, 556.0, 222.0, 556.0));
        assert_eq!(widths("Courier")[b'W' as usize], 600.0);
        assert_eq!(widths("Times-Roman")[b' ' as usize], 250.0);
        assert!((text_width("Hello", "Helvetica", 12.0, None) - 27.336).abs() < 1e-6);
    }

    #[test]
    fn win_ansi_codes() {
        assert_eq!(win_ansi('A'), Some(65));
        assert_eq!(win_ansi('\u{20AC}'), Some(0x80));
        assert_eq!(win_ansi('\u{00E9}'), Some(0xE9));
        assert_eq!(win_ansi('\u{4E2D}'), None);
        assert_eq!(pick_standard("serif", true, false), Ok("Times-Bold"));
        assert_eq!(pick_standard("Courier-Oblique", false, false), Ok("Courier-Oblique"));
        assert!(pick_standard("Comic", false, false).is_err());
    }

    #[test]
    fn cjk_collections_and_runs() {
        assert!(Cjk::Cns1.covers('\u{9AD4}')); // traditional
        assert!(Cjk::Gb1.covers('\u{4F53}')); // simplified
        assert_eq!(pick_cjk("\u{3072}\u{3089}\u{304C}\u{306A}", None), Some(Cjk::Japan1));
        assert_eq!(pick_cjk("\u{D55C}\u{AE00}", None), Some(Cjk::Korea1));
        assert_eq!(pick_cjk("\u{4E2D}\u{6587}", None), Some(Cjk::Cns1));
        let r = runs("PDF \u{4E2D}\u{6587}!", None);
        assert_eq!(r, vec![Run::Latin(b"PDF ".to_vec()), Run::Cjk(Cjk::Cns1, vec![0x4E, 0x2D, 0x65, 0x87]), Run::Latin(b"!".to_vec())]);
        let w = runs_width(&r, "Helvetica", 10.0, );
        assert!((w - (text_width("PDF !", "Helvetica", 10.0, None) + 20.0)).abs() < 1e-6);
    }

    #[test]
    fn wrapping() {
        let lines = wrap("one two three four five six", "Courier", 10.0, 60.0, None);
        assert_eq!(lines, ["one two", "three four", "five six"]);
        let cjk = wrap("\u{4E2D}\u{6587}\u{6E2C}\u{8A66}\u{5B57}", "Helvetica", 10.0, 25.0, None);
        assert_eq!(cjk.len(), 3);
        assert_eq!(wrap("a\n\nb", "Helvetica", 10.0, 100.0, None), ["a", "", "b"]);
        let long = wrap("abcdefghijkl", "Courier", 10.0, 30.0, None);
        assert_eq!(long, ["abcde", "fghij", "kl"]);
        // a word too wide for a line is broken also when it does not start the text
        assert_eq!(wrap("ab abcdefghijkl x", "Courier", 10.0, 30.0, None), ["ab", "abcde", "fghij", "kl x"]);
        // trailing spaces never push a word to the next line or start a line
        assert_eq!(wrap("abcde     fg", "Courier", 10.0, 30.0, None), ["abcde", "fg"]);
        // every line fits, measured the way add_text measures it
        let text = "Revenue grew in every region this quarter, 中文混排 testing wrapped widths. ".repeat(20);
        for max_w in [40.0, 97.5, 200.0, 500.0] {
            let lines = wrap(&text, "Helvetica", 11.0, max_w, None);
            assert!(lines.len() > 1);
            for l in &lines {
                let lw = runs_width(&runs(l, None), "Helvetica", 11.0);
                assert!(lw <= max_w || l.chars().count() == 1, "{l:?} is {lw} wide at {max_w}");
                assert!((lw - text_width(l, "Helvetica", 11.0, None)).abs() < 1e-9);
            }
            let joined: String = lines.concat();
            assert_eq!(joined.replace(' ', ""), text.replace(' ', ""), "nothing lost at {max_w}");
        }
    }

    #[test]
    fn wrapping_is_linear() {
        // 1 MB on one line (width far beyond any page) and wrapped at the widest page
        let latin = "lorem ipsum dolor sit amet consectetur ".repeat((1 << 20) / 39);
        let cjk = "\u{4E2D}\u{6587}\u{6E2C}\u{8A66}\u{5B57}\u{9AD4}".repeat((1 << 20) / 18);
        for (name, text) in [("latin", &latin), ("cjk", &cjk)] {
            for max_w in [1e9, 14_400.0, 300.0] {
                let t0 = std::time::Instant::now();
                let lines = wrap(text, "Helvetica", 10.0, max_w, None);
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                assert!(!lines.is_empty());
                assert!(ms < 3000.0, "{name} at {max_w}: {ms:.0} ms");
                eprintln!("wrap 1 MB {name} at {max_w}: {} lines in {ms:.0} ms", lines.len());
            }
            let t0 = std::time::Instant::now();
            let r = runs(text, None);
            let w = runs_width(&r, "Helvetica", 10.0);
            assert!((w - text_width(text, "Helvetica", 10.0, None)).abs() < 1e-6 * w);
            assert!(t0.elapsed().as_secs_f64() < 3.0);
        }
    }
}
