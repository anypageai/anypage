//! Small helpers shared by the module: PDF text strings, lopdf object access,
//! colours.

use lopdf::{Dictionary, Document, Object, StringFormat};

pub type R<T> = Result<T, String>;

/// PDFDocEncoding 0x18..=0x1F and 0x80..=0xA0 (everything else is Latin-1).
const PDFDOC_18: [char; 8] = ['\u{02D8}', '\u{02C7}', '\u{02C6}', '\u{02D9}', '\u{02DD}', '\u{02DB}', '\u{02DA}', '\u{02DC}'];
const PDFDOC_80: [char; 33] = [
    '\u{2022}', '\u{2020}', '\u{2021}', '\u{2026}', '\u{2014}', '\u{2013}', '\u{0192}', '\u{2044}', '\u{2039}', '\u{203A}', '\u{2212}',
    '\u{2030}', '\u{201E}', '\u{201C}', '\u{201D}', '\u{2018}', '\u{2019}', '\u{201A}', '\u{2122}', '\u{FB01}', '\u{FB02}', '\u{0141}',
    '\u{0152}', '\u{0160}', '\u{0178}', '\u{017D}', '\u{0131}', '\u{0142}', '\u{0153}', '\u{0161}', '\u{017E}', '\u{FFFD}', '\u{20AC}',
];

/// A PDF "text string": UTF-16BE or UTF-8 with a byte order mark, otherwise PDFDocEncoding.
pub fn decode_text_string(b: &[u8]) -> String {
    if let Some(rest) = b.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest.chunks(2).map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    b.iter()
        .map(|&c| match c {
            0x18..=0x1F => PDFDOC_18[(c - 0x18) as usize],
            0x80..=0xA0 => PDFDOC_80[(c - 0x80) as usize],
            _ => c as char,
        })
        .collect()
}

/// A text string object: a literal for ASCII, UTF-16BE with a BOM otherwise.
pub fn text_string(s: &str) -> Object {
    if s.chars().all(|c| (' '..='~').contains(&c) || c == '\n' || c == '\r' || c == '\t') {
        Object::String(s.as_bytes().to_vec(), StringFormat::Literal)
    } else {
        let mut v = vec![0xFE, 0xFF];
        for u in s.encode_utf16() {
            v.extend_from_slice(&u.to_be_bytes());
        }
        Object::String(v, StringFormat::Hexadecimal)
    }
}

pub fn name(s: &str) -> Object {
    Object::Name(s.as_bytes().to_vec())
}

/// Follow references (a bounded number of hops).
pub fn deref<'a>(doc: &'a Document, mut o: &'a Object) -> &'a Object {
    for _ in 0..32 {
        match o {
            Object::Reference(id) => match doc.get_object(*id) {
                Ok(t) => o = t,
                Err(_) => return &Object::Null,
            },
            _ => return o,
        }
    }
    &Object::Null
}

pub fn dict_get<'a>(doc: &'a Document, d: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
    d.get(key).ok().map(|o| deref(doc, o)).filter(|o| !matches!(o, Object::Null))
}

pub fn get_dict<'a>(doc: &'a Document, d: &'a Dictionary, key: &[u8]) -> Option<&'a Dictionary> {
    match dict_get(doc, d, key)? {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&s.dict),
        _ => None,
    }
}

pub fn num(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

pub fn get_num(doc: &Document, d: &Dictionary, key: &[u8]) -> Option<f64> {
    dict_get(doc, d, key).and_then(num)
}

pub fn get_name<'a>(doc: &'a Document, d: &'a Dictionary, key: &[u8]) -> Option<&'a [u8]> {
    match dict_get(doc, d, key)? {
        Object::Name(n) => Some(n),
        _ => None,
    }
}

pub fn get_text(doc: &Document, d: &Dictionary, key: &[u8]) -> Option<String> {
    match dict_get(doc, d, key)? {
        Object::String(s, _) => Some(decode_text_string(s)),
        Object::Name(n) => Some(String::from_utf8_lossy(n).into_owned()),
        _ => None,
    }
}

pub fn get_array<'a>(doc: &'a Document, d: &'a Dictionary, key: &[u8]) -> Option<&'a Vec<Object>> {
    match dict_get(doc, d, key)? {
        Object::Array(a) => Some(a),
        _ => None,
    }
}

/// A number for content streams: short, no exponent.
pub fn f(x: f64) -> String {
    if !x.is_finite() {
        return "0".into();
    }
    let r = (x * 10_000.0).round() / 10_000.0;
    if r == r.trunc() && r.abs() < 1e15 {
        format!("{}", r as i64)
    } else {
        let s = format!("{r:.4}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Packed 0xRRGGBBAA to (r, g, b, a) in 0..1.
pub fn unpack(c: f64) -> [f64; 4] {
    let v = if c.is_finite() { c.clamp(0.0, u32::MAX as f64) as u32 } else { 0xFF };
    let b = v.to_be_bytes();
    [b[0] as f64 / 255.0, b[1] as f64 / 255.0, b[2] as f64 / 255.0, b[3] as f64 / 255.0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_strings_round_trip() {
        for s in ["Hello", "中文標題", "Ünïcödé €"] {
            let o = text_string(s);
            let Object::String(b, _) = &o else { panic!() };
            assert_eq!(decode_text_string(b), s);
        }
        assert_eq!(decode_text_string(b"caf\xe9 \x80 \x93"), "caf\u{e9} \u{2022} \u{FB01}");
    }

    #[test]
    fn numbers_are_short() {
        assert_eq!(f(1.0), "1");
        assert_eq!(f(-0.5), "-0.5");
        assert_eq!(f(1.234567), "1.2346");
        assert_eq!(f(f64::NAN), "0");
        assert_eq!(unpack(0xFF0000FFu32 as f64), [1.0, 0.0, 0.0, 1.0]);
    }
}
