//! Decompression limits for files that come from outside.
//!
//! hayro decodes Flate and LZW streams to their end with no cap on the output
//! (`read_to_end`), and decodes compressed cross-reference and object streams
//! while it opens a file. A small file whose stream inflates to gigabytes would
//! exhaust memory (an abort on Android) when it is opened, drawn or read.
//! Until hayro takes an output cap (CORE_REQUESTS.md #7), [`check_streams`]
//! looks at every stream of a file once, before hayro or lopdf see it:
//!
//! * the stream's dictionary is read from the raw bytes for its `/Filter`
//!   chain (`/Length`, `/Width`, `/Height`, `/EarlyChange` too);
//! * a chain that cannot produce more than [`MAX_STREAM`] bytes from that
//!   many input bytes (deflate expands at most ~1032:1, LZW ~3641:1,
//!   RunLength 64:1) is passed without decoding, which is nearly every stream
//!   of a normal file (anything under ~240 KB compressed);
//! * the others are decoded with a counting decoder (Flate also as raw
//!   deflate, as hayro retries it; LZW; RunLength; ASCIIHex and ASCII85 in
//!   front), which stops as soon as the output passes the cap or can no
//!   longer reach it;
//! * images whose declared `/Width` x `/Height` exceeds [`MAX_IMAGE_PIXELS`]
//!   are refused too (they would be decoded whole to be drawn);
//! * every `stream` keyword is examined, also inside other streams' data, and
//!   the decoding over the whole file is budgeted ([`Limits`]), so a file of
//!   many streams just under the cap cannot keep the check busy for minutes.
//!
//! A file with such a stream is not opened. Not covered: streams whose
//! `/Filter` is an indirect object and whose data does not start with a zlib
//! header, names written with `#` escapes, and encrypted files (their stream
//! bytes are ciphertext here). Cost: nothing for normal files of small streams;
//! files with big compressed streams pay one counting decode of them at open
//! (about 30 ms for a 256 MB bomb; a few hundred MB/s for real content).

use std::borrow::Cow;

use memchr::memmem;

use crate::util::R;

/// Most bytes one stream may decode to.
pub const MAX_STREAM: u64 = 256 << 20;
/// Most pixels an image may declare.
pub const MAX_IMAGE_PIXELS: u64 = 256 << 20;

#[derive(Clone, Copy, Debug, PartialEq)]
enum F {
    Flate,
    Lzw,
    RunLength,
    Hex,
    A85,
    /// An image codec or crypt filter: decoding stops before it.
    End,
}

impl F {
    fn of(name: &[u8]) -> F {
        match name {
            b"FlateDecode" | b"Fl" => F::Flate,
            b"LZWDecode" | b"LZW" => F::Lzw,
            b"RunLengthDecode" | b"RL" => F::RunLength,
            b"ASCIIHexDecode" | b"AHx" => F::Hex,
            b"ASCII85Decode" | b"A85" => F::A85,
            _ => F::End,
        }
    }
    /// Most output bytes per input byte.
    fn ratio(self) -> f64 {
        match self {
            F::Flate => 1100.0,
            F::Lzw => 3700.0,
            F::RunLength => 64.0,
            F::Hex => 0.5,
            // 'z' is four zero bytes
            F::A85 => 4.0,
            F::End => 1.0,
        }
    }
}

/// The limits [`check_streams_with`] applies.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Most bytes one stream may decode to.
    pub stream: u64,
    /// Most pixels an image may declare.
    pub pixels: u64,
    /// Bytes the check may decode over the whole file, per file byte...
    pub work_per_byte: u64,
    /// ...and at least.
    pub work_floor: u64,
}

impl Default for Limits {
    fn default() -> Self {
        // flat images reach 100:1, so the budget is generous; a crafted file
        // can still only make the check decode 64x its size (a few seconds)
        Limits { stream: MAX_STREAM, pixels: MAX_IMAGE_PIXELS, work_per_byte: 64, work_floor: 2 << 30 }
    }
}

/// Check every stream of a file against [`MAX_STREAM`] and [`MAX_IMAGE_PIXELS`].
pub fn check_streams(bytes: &[u8]) -> R<()> {
    check_streams_with(bytes, &Limits::default())
}

/// [`check_streams`] with other limits (tests).
///
/// Every `stream` keyword is looked at, also inside the data of another
/// stream: skipping a stream's data by its (attacker-written) length could
/// hide the next stream. The decoding work over the whole file is budgeted
/// (by default 64 bytes out per file byte, at least 2 GB); a file that needs
/// more is refused as well.
pub fn check_streams_with(bytes: &[u8], limits: &Limits) -> R<()> {
    let (cap, max_pixels) = (limits.stream, limits.pixels);
    let keyword = memmem::Finder::new(b"stream");
    let end_keyword = memmem::Finder::new(b"endstream");
    let mut budget = (bytes.len() as u64).saturating_mul(limits.work_per_byte).max(limits.work_floor);
    let mut pos = 0;
    while let Some(off) = keyword.find(&bytes[pos..]) {
        let p = pos + off;
        pos = p + 6;
        if p >= 3 && &bytes[p - 3..p] == b"end" {
            continue;
        }
        // the keyword ends its line; the data starts on the next
        let mut start = p + 6;
        match bytes.get(start) {
            Some(b'\r') => {
                start += 1;
                if bytes.get(start) == Some(&b'\n') {
                    start += 1;
                }
            }
            Some(b'\n') => start += 1,
            _ => continue,
        }
        let dict = dict_before(bytes, p);
        let declared = direct_int(dict, b"/Length").map(|l| start.saturating_add(l as usize)).filter(|&e| e <= bytes.len() && ends_stream(&bytes[e..]));
        let end = declared.unwrap_or_else(|| end_keyword.find(&bytes[start..]).map_or(bytes.len(), |e| start + e));
        check_one(dict, &bytes[start..end], cap, max_pixels, &mut budget).map_err(|e| format!("the stream near byte {p} {e}; the file was not opened, to protect memory"))?;
    }
    Ok(())
}

/// `endstream` after optional whitespace.
fn ends_stream(b: &[u8]) -> bool {
    let k = b.iter().take(4).take_while(|c| c.is_ascii_whitespace()).count();
    b[k..].starts_with(b"endstream")
}

/// The dictionary of the stream whose `stream` keyword is at `p`: from the
/// object header (`N G obj`) before it, at most 64 KB back.
fn dict_before(bytes: &[u8], p: usize) -> &[u8] {
    let lo = p.saturating_sub(64 << 10);
    match memmem::rfind(&bytes[lo..p], b"obj") {
        Some(o) => &bytes[lo + o + 3..p],
        None => &bytes[lo..p],
    }
}

fn is_delim(c: u8) -> bool {
    c.is_ascii_whitespace() || b"/[]<>()%{}".contains(&c)
}

/// Where the value of key `key` (e.g. `/Length`) starts in `dict`.
fn value_of<'a>(dict: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut from = 0;
    while let Some(k) = memmem::find(&dict[from..], key) {
        let after = from + k + key.len();
        if dict.get(after).is_none_or(|c| is_delim(*c)) {
            let v = &dict[after..];
            let skip = v.iter().take_while(|c| c.is_ascii_whitespace()).count();
            return Some(&v[skip..]);
        }
        from = after;
    }
    None
}

/// An integer value that is not a reference (`12 0 R`).
fn direct_int(dict: &[u8], key: &[u8]) -> Option<u64> {
    let v = value_of(dict, key)?;
    let digits = v.iter().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits > 15 {
        return None;
    }
    let n: u64 = std::str::from_utf8(&v[..digits]).ok()?.parse().ok()?;
    // "12 0 R" is a reference
    let rest = &v[digits..];
    let ws = rest.iter().take_while(|c| c.is_ascii_whitespace()).count();
    let rest = &rest[ws..];
    let gen = rest.iter().take_while(|c| c.is_ascii_digit()).count();
    if ws > 0 && gen > 0 {
        let tail = &rest[gen..];
        let ws2 = tail.iter().take_while(|c| c.is_ascii_whitespace()).count();
        if tail.get(ws2) == Some(&b'R') {
            return None;
        }
    }
    Some(n)
}

/// The `/Filter` chain; `None` when it is not written in the dictionary itself.
fn filters_of(dict: &[u8]) -> Option<Vec<F>> {
    let Some(v) = value_of(dict, b"/Filter") else { return Some(Vec::new()) };
    let name = |v: &[u8]| -> (F, usize) {
        let n = v.iter().skip(1).take_while(|c| !is_delim(**c)).count();
        (F::of(&v[1..1 + n]), 1 + n)
    };
    match v.first()? {
        b'/' => Some(vec![name(v).0]),
        b'[' => {
            let mut out = Vec::new();
            let mut i = 1;
            while i < v.len() && out.len() < 16 {
                match v[i] {
                    b']' => return Some(out),
                    b'/' => {
                        let (f, n) = name(&v[i..]);
                        out.push(f);
                        i += n;
                    }
                    c if c.is_ascii_whitespace() => i += 1,
                    _ => return None,
                }
            }
            None
        }
        _ => None,
    }
}

fn zlib_header(d: &[u8]) -> bool {
    d.len() >= 2 && d[0] & 0x0F == 8 && (((d[0] as u16) << 8) | d[1] as u16).is_multiple_of(31)
}

struct Bomb(u64);

/// What a stage produced: the bytes (for a following stage), or only the
/// knowledge that it stays under the cap.
enum Out {
    Bytes(Vec<u8>),
    Within,
}

fn check_one(dict: &[u8], data: &[u8], cap: u64, max_pixels: u64, budget: &mut u64) -> R<()> {
    if memmem::find(dict, b"/Image").is_some() {
        if let (Some(w), Some(h)) = (direct_int(dict, b"/Width"), direct_int(dict, b"/Height")) {
            if w.saturating_mul(h) > max_pixels {
                return Err(format!("is an image of {w} x {h} pixels (at most {} Mpx are drawn)", max_pixels >> 20));
            }
        }
    }
    let chain = match filters_of(dict) {
        Some(f) => f,
        None if zlib_header(data) => vec![F::Flate],
        None => return Ok(()),
    };
    let chain: Vec<F> = chain.into_iter().take_while(|f| *f != F::End).collect();
    let early = direct_int(dict, b"/EarlyChange") != Some(0);
    let mut cur: Cow<[u8]> = Cow::Borrowed(data);
    for (k, f) in chain.iter().enumerate() {
        // can the rest of the chain produce more than the cap at all?
        let bound = chain[k..].iter().fold(cur.len() as f64, |b, f| b * f.ratio());
        if bound <= cap as f64 {
            return Ok(());
        }
        let last = k + 1 == chain.len();
        // a stream decodes to at most `cap`, and the whole file within its budget
        let limit = cap.min(*budget);
        match decode(*f, &cur, limit, !last, early) {
            Ok((out, work)) => {
                *budget = budget.saturating_sub(work);
                match out {
                    Out::Bytes(v) => cur = Cow::Owned(v),
                    Out::Within => return Ok(()),
                }
            }
            Err(Bomb(n)) if n > cap => return Err(format!("decodes to more than {} MB (at least {} MB seen)", cap >> 20, n >> 20)),
            Err(Bomb(_)) => return Err("is one of more compressed data than can be checked within the file's budget".into()),
        }
    }
    Ok(())
}

/// Decode `data` through `f`, keeping the bytes when `keep`; the output and
/// the bytes decoded on the way (hayro's retries included). `Err` once an
/// output passes `limit`. Undecodable data ends the output (hayro stops
/// there too).
fn decode(f: F, data: &[u8], limit: u64, keep: bool, early: bool) -> Result<(Out, u64), Bomb> {
    let mut kept = Vec::new();
    let mut sink = |b: &[u8]| {
        if keep {
            kept.extend_from_slice(b);
        }
    };
    let (n, work) = match f {
        F::Flate => {
            // hayro tries zlib, then raw deflate, then raw deflate after the header
            let (n, ok) = inflate(data, true, limit, keep, &mut sink)?;
            if ok {
                (n, n)
            } else {
                if keep {
                    kept.clear();
                }
                let mut sink2 = |b: &[u8]| {
                    if keep {
                        kept.extend_from_slice(b);
                    }
                };
                // each attempt's output is dropped before the next: the peak is one of them
                let (n2, ok2) = inflate(data, false, limit, keep, &mut sink2)?;
                if !ok2 && zlib_header(data) {
                    let (n3, _) = inflate(&data[2..], false, limit, false, &mut |_| {})?;
                    (n.max(n2).max(n3), n + n2 + n3)
                } else {
                    (n.max(n2), n + n2)
                }
            }
        }
        F::Lzw => {
            let n = lzw(data, early, limit, keep, &mut sink)?;
            (n, n)
        }
        F::RunLength => {
            let n = run_length(data, limit, &mut sink)?;
            (n, n)
        }
        F::Hex => {
            let v = hex(data);
            sink(&v);
            (v.len() as u64, v.len() as u64)
        }
        F::A85 => {
            let v = a85(data, limit)?;
            sink(&v);
            (v.len() as u64, v.len() as u64)
        }
        F::End => (0, 0),
    };
    if n > limit {
        return Err(Bomb(n));
    }
    Ok((if keep { Out::Bytes(kept) } else { Out::Within }, work))
}

/// Inflate, counting; `(bytes out, finished cleanly)`. Without `keep` it stops
/// early once the input left cannot carry the output past `cap`.
fn inflate(data: &[u8], zlib: bool, cap: u64, keep: bool, sink: &mut dyn FnMut(&[u8])) -> Result<(u64, bool), Bomb> {
    use flate2::{Decompress, FlushDecompress, Status};
    let mut d = Decompress::new(zlib);
    let mut buf = vec![0u8; 64 << 10];
    let mut inp = data;
    let mut total = 0u64;
    loop {
        let (in0, out0) = (d.total_in(), d.total_out());
        let st = d.decompress(inp, &mut buf, FlushDecompress::None);
        let used = (d.total_in() - in0) as usize;
        let made = (d.total_out() - out0) as usize;
        inp = &inp[used.min(inp.len())..];
        total += made as u64;
        if total > cap {
            return Err(Bomb(total));
        }
        sink(&buf[..made]);
        match st {
            Ok(Status::StreamEnd) => return Ok((total, true)),
            Ok(_) if used == 0 && made == 0 => return Ok((total, inp.is_empty())),
            Ok(_) => {}
            Err(_) => return Ok((total, false)),
        }
        if !keep && total as f64 + inp.len() as f64 * F::Flate.ratio() <= cap as f64 {
            return Ok((total, true));
        }
    }
}

fn lzw(data: &[u8], early: bool, cap: u64, keep: bool, sink: &mut dyn FnMut(&[u8])) -> Result<u64, Bomb> {
    use weezl::{decode::Decoder, BitOrder, LzwStatus};
    let mut d = if early { Decoder::with_tiff_size_switch(BitOrder::Msb, 8) } else { Decoder::new(BitOrder::Msb, 8) };
    let mut buf = vec![0u8; 64 << 10];
    let mut inp = data;
    let mut total = 0u64;
    loop {
        let r = d.decode_bytes(inp, &mut buf);
        inp = &inp[r.consumed_in.min(inp.len())..];
        total += r.consumed_out as u64;
        if total > cap {
            return Err(Bomb(total));
        }
        sink(&buf[..r.consumed_out]);
        match r.status {
            Ok(LzwStatus::Ok) if r.consumed_in > 0 || r.consumed_out > 0 => {}
            _ => return Ok(total),
        }
        if !keep && total as f64 + inp.len() as f64 * F::Lzw.ratio() <= cap as f64 {
            return Ok(total);
        }
    }
}

fn run_length(data: &[u8], cap: u64, sink: &mut dyn FnMut(&[u8])) -> Result<u64, Bomb> {
    let mut total = 0u64;
    let mut i = 0;
    let mut run = [0u8; 128];
    while i < data.len() {
        let n = data[i] as usize;
        if n == 128 {
            break;
        }
        if n < 128 {
            let lit = &data[(i + 1).min(data.len())..(i + 2 + n).min(data.len())];
            total += lit.len() as u64;
            sink(lit);
            i += 2 + n;
        } else {
            let Some(&b) = data.get(i + 1) else { break };
            let len = 257 - n;
            run[..len].fill(b);
            total += len as u64;
            sink(&run[..len]);
            i += 2;
        }
        if total > cap {
            return Err(Bomb(total));
        }
    }
    Ok(total)
}

fn hex(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2);
    let mut hi: Option<u8> = None;
    for &c in data {
        let v = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            b'>' => break,
            _ => continue,
        };
        match hi.take() {
            Some(h) => out.push(h << 4 | v),
            None => hi = Some(v),
        }
    }
    if let Some(h) = hi {
        out.push(h << 4);
    }
    out
}

fn a85(data: &[u8], cap: u64) -> Result<Vec<u8>, Bomb> {
    let mut out = Vec::with_capacity(data.len() * 4 / 5 + 4);
    let mut group = [0u8; 5];
    let mut n = 0;
    let data = data.strip_prefix(b"<~").unwrap_or(data);
    for &c in data {
        match c {
            b'~' => break,
            b'z' if n == 0 => out.extend_from_slice(&[0, 0, 0, 0]),
            b'!'..=b'u' => {
                group[n] = c - b'!';
                n += 1;
                if n == 5 {
                    let v = group.iter().fold(0u64, |a, &d| a * 85 + d as u64);
                    out.extend_from_slice(&(v.min(u32::MAX as u64) as u32).to_be_bytes());
                    n = 0;
                }
            }
            c if c.is_ascii_whitespace() => {}
            _ => break,
        }
        if out.len() as u64 > cap {
            return Err(Bomb(out.len() as u64));
        }
    }
    if n > 1 {
        for g in group.iter_mut().skip(n) {
            *g = 84;
        }
        let v = group.iter().fold(0u64, |a, &d| a * 85 + d as u64);
        out.extend_from_slice(&(v.min(u32::MAX as u64) as u32).to_be_bytes()[..n - 1]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        let _ = e.write_all(data);
        e.finish().unwrap_or_default()
    }

    fn pdf_with(dict: &str, data: &[u8]) -> Vec<u8> {
        let mut f = b"%PDF-1.7\n1 0 obj\n<< /Type /Catalog >>\nendobj\n4 0 obj\n".to_vec();
        f.extend_from_slice(format!("<< /Length {} {dict} >>\nstream\n", data.len()).as_bytes());
        f.extend_from_slice(data);
        f.extend_from_slice(b"\nendstream\nendobj\n%%EOF\n");
        f
    }

    /// 1 MB a stream, 1 Mpx an image.
    const L: Limits = Limits { stream: 1 << 20, pixels: 1 << 20, work_per_byte: 16, work_floor: 1 << 30 };

    fn check(f: &[u8]) -> R<()> {
        check_streams_with(f, &L)
    }

    #[test]
    fn small_and_normal_streams_pass() {
        let text = b"BT /F1 12 Tf 72 720 Td (Hello) Tj ET\n".repeat(2000);
        assert!(check(&pdf_with("/Filter /FlateDecode", &zlib(&text))).is_ok());
        assert!(check(&pdf_with("", &text)).is_ok());
        // 900 KB of zeros: under the cap
        assert!(check(&pdf_with("/Filter /FlateDecode", &zlib(&vec![0; 900 << 10]))).is_ok());
    }

    #[test]
    fn bombs_are_refused() {
        let zeros = vec![0u8; 3 << 20];
        let one = zlib(&zeros);
        let e = check(&pdf_with("/Filter /FlateDecode", &one)).unwrap_err();
        assert!(e.contains("more than 1 MB") && e.contains("not opened"), "{e}");
        // abbreviated name, in an array, behind ASCIIHex
        let hexed: String = one.iter().map(|b| format!("{b:02X}")).collect();
        assert!(check(&pdf_with("/Filter [/AHx /Fl]", hexed.as_bytes())).is_err());
        // nested: a few hundred bytes that inflate twice
        let two = zlib(&one);
        assert!(two.len() < 4096);
        assert!(check(&pdf_with("/Filter [/FlateDecode /FlateDecode]", &two)).is_err());
        // the filter is an indirect object: found by the zlib header
        assert!(check(&pdf_with("/Filter 9 0 R", &one)).is_err());
        // a raw deflate stream (no zlib header), as hayro also accepts
        let mut raw = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
        let _ = raw.write_all(&zeros);
        let raw = raw.finish().unwrap_or_default();
        assert!(check(&pdf_with("/Filter /FlateDecode", &raw)).is_err());
        // RunLength: 2 bytes for 128
        let rl: Vec<u8> = [129u8, 0].repeat(20_000);
        assert!(check(&pdf_with("/Filter /RunLengthDecode", &rl)).is_err());
        // an image declaring more pixels than are drawn
        let e = check(&pdf_with("/Type /XObject /Subtype /Image /Width 70000 /Height 70000", b"x")).unwrap_err();
        assert!(e.contains("70000 x 70000"), "{e}");
        // a /Length that is a reference, or wrong, still finds the data
        let mut f = b"%PDF-1.7\n4 0 obj\n<< /Length 7 0 R /Filter /FlateDecode >>\nstream\r\n".to_vec();
        f.extend_from_slice(&one);
        f.extend_from_slice(b"\r\nendstream\nendobj\n");
        assert!(check(&f).is_err());
    }

    #[test]
    fn a_stream_hidden_inside_another_is_still_checked() {
        // a decoy stream whose /Length covers the bomb's object
        let bomb = zlib(&vec![0u8; 3 << 20]);
        let mut inner = b"5 0 obj\n<< /Filter /FlateDecode >>\nstream\n".to_vec();
        inner.extend_from_slice(&bomb);
        inner.extend_from_slice(b"\nendstream\nendobj\n");
        let mut f = format!("%PDF-1.7\n4 0 obj\n<< /Length {} >>\nstream\n", inner.len()).into_bytes();
        f.extend_from_slice(&inner);
        f.extend_from_slice(b"\nendstream\nendobj\n");
        assert!(check(&f).is_err());
    }

    #[test]
    fn the_whole_file_has_a_decoding_budget() {
        // 40 streams that each decode to 900 KB (under the cap, but too big to
        // pass unchecked): together more than 1 byte per file byte
        let text: String = (0..100_000).map(|i| format!("{i} lorem {}\n", (i * 7919) % 10007)).collect::<String>();
        let one = zlib(&text.as_bytes()[..900 << 10]);
        let mut f = b"%PDF-1.7\n".to_vec();
        for k in 0..40 {
            f.extend_from_slice(format!("{k} 0 obj\n<< /Length {} /Filter /FlateDecode >>\nstream\n", one.len()).as_bytes());
            f.extend_from_slice(&one);
            f.extend_from_slice(b"\nendstream\nendobj\n");
        }
        let tight = Limits { work_per_byte: 1, work_floor: 0, ..L };
        let e = check_streams_with(&f, &tight).unwrap_err();
        assert!(e.contains("budget"), "{e}");
        assert!(check(&f).is_ok(), "within the default budget");
    }

    #[test]
    fn lzw_bombs_are_refused() {
        let mut e = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8);
        let data = e.encode(&vec![7u8; 4 << 20]).unwrap_or_default();
        assert!(!data.is_empty());
        let r = check(&pdf_with("/Filter /LZWDecode", &data));
        assert!(r.is_err(), "{} bytes of LZW", data.len());
        // the same data under a cap it fits in
        assert!(check_streams_with(&pdf_with("/Filter /LZWDecode", &data), &Limits { stream: 8 << 20, ..L }).is_ok());
    }

    #[test]
    fn dictionary_values() {
        assert_eq!(direct_int(b"<< /Length 42 /Filter /Fl >>", b"/Length"), Some(42));
        assert_eq!(direct_int(b"<< /Length 42 0 R >>", b"/Length"), None);
        assert_eq!(direct_int(b"<< /LengthX 5 /Length 6 >>", b"/Length"), Some(6));
        assert_eq!(filters_of(b"<< /Filter[/A85/Fl] >>"), Some(vec![F::A85, F::Flate]));
        assert_eq!(filters_of(b"<< /Length 3 >>"), Some(vec![]));
        assert_eq!(filters_of(b"<< /Filter /DCTDecode >>"), Some(vec![F::End]));
        for s in [&b"Hello world"[..], b"", b"a", b"\0\0\0\0xyz", b"0123456789"] {
            assert_eq!(a85(&a85_encode(s), 1 << 20).ok().as_deref(), Some(s), "{s:?}");
        }
        assert_eq!(hex(b"48 65 6c6C6f>"), b"Hello");
    }

    /// ASCII85 as PDF writers produce it (`z` for zero groups, `~>` at the end).
    fn a85_encode(b: &[u8]) -> Vec<u8> {
        let mut out = b"<~".to_vec();
        for chunk in b.chunks(4) {
            let mut g = [0u8; 4];
            g[..chunk.len()].copy_from_slice(chunk);
            let mut v = u32::from_be_bytes(g) as u64;
            if chunk.len() == 4 && v == 0 {
                out.push(b'z');
                continue;
            }
            let mut d = [0u8; 5];
            for k in (0..5).rev() {
                d[k] = (v % 85) as u8 + b'!';
                v /= 85;
            }
            out.extend_from_slice(&d[..chunk.len() + 1]);
        }
        out.extend_from_slice(b"~>");
        out
    }
}
