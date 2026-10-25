//! Small shared helpers: hashing, hex, percent-encoding, UTC timestamps,
//! base64 and redaction of secrets in messages.

use base64::Engine as _;
use ring::{digest, hmac};

pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let d = digest::digest(&digest::SHA256, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&sha256(data))
}

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data);
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn encode_with(s: &[u8], keep_slash: bool) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len() + 8);
    for &b in s {
        if unreserved(b) || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(DIGITS[(b >> 4) as usize] as char);
            out.push(DIGITS[(b & 15) as usize] as char);
        }
    }
    out
}

/// RFC 3986 percent-encoding: everything but `A-Z a-z 0-9 - . _ ~` becomes
/// `%XX` (upper-case hex), including `/`. This is the encoding SigV4 and the
/// Google APIs expect for query values and object names.
pub fn encode_strict(s: &str) -> String {
    encode_with(s.as_bytes(), false)
}

/// Like [`encode_strict`] but keeps `/`: for paths.
pub fn encode_path(s: &str) -> String {
    encode_with(s.as_bytes(), true)
}

/// Decode `%XX` escapes (malformed escapes are kept literally). `+` stays `+`.
pub fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = |c: u8| (c as char).to_digit(16);
            if let (Some(a), Some(c)) = (h(b[i + 1]), h(b[i + 2])) {
                out.push((a * 16 + c) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// `k=v&k2=v2` (already encoded) to decoded pairs.
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (String::from_utf8_lossy(&percent_decode(k)).into_owned(), String::from_utf8_lossy(&percent_decode(v)).into_owned())
        })
        .collect()
}

/// Strictly encoded `k=v` pairs joined with `&`, in the given order.
pub fn build_query(pairs: &[(String, String)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", encode_strict(k), encode_strict(v))).collect::<Vec<_>>().join("&")
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A UTC calendar time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Utc {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Seconds since 1970 to a UTC calendar time (proleptic Gregorian).
pub fn utc(secs: u64) -> Utc {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    Utc { year, month, day, hour: (rem / 3600) as u32, minute: (rem / 60 % 60) as u32, second: (rem % 60) as u32 }
}

/// `20150830T123600Z`
pub fn amz_date(secs: u64) -> String {
    let t = utc(secs);
    format!("{:04}{:02}{:02}T{:02}{:02}{:02}Z", t.year, t.month, t.day, t.hour, t.minute, t.second)
}

pub fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

pub fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

pub fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let clean: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(clean.as_bytes()).map_err(|e| format!("bad base64: {e}"))
}

/// The base64 body of a PEM block labelled `label` ("PRIVATE KEY").
pub fn pem_block(pem: &str, label: &str) -> Option<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem.find(&begin)? + begin.len();
    let stop = start + pem[start..].find(&end)?;
    b64_decode(&pem[start..stop]).ok()
}

/// Replace every occurrence of each secret (4+ characters) by `[redacted]`.
/// Used on every error string that could echo request data.
pub fn redact(msg: &str, secrets: &[&str]) -> String {
    let mut out = msg.to_string();
    for s in secrets {
        if s.len() >= 4 && out.contains(s) {
            out = out.replace(s, "[redacted]");
        }
    }
    out
}

/// Shortest value [`Scrubber`] looks for in responses: shorter strings are
/// not plausible secrets and would match ordinary text.
pub const MIN_SCRUB: usize = 8;
const REDACTED: &[u8] = b"[redacted]";

/// Removes secret values, and their base64 forms, from data returned to a
/// page: response bodies, headers, streamed text. A server that echoes a
/// credential (an error body quoting the key, an endpoint that reflects
/// request headers) cannot hand it to the script.
#[derive(Clone, Default)]
pub struct Scrubber {
    finders: Vec<memchr::memmem::Finder<'static>>,
    max_len: usize,
}

/// Never shows the values.
impl std::fmt::Debug for Scrubber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Scrubber({} values)", self.finders.len())
    }
}

impl Scrubber {
    pub fn new<S: AsRef<str>>(values: impl IntoIterator<Item = S>) -> Self {
        let mut v: Vec<String> = Vec::new();
        for s in values {
            let s = s.as_ref().trim();
            if s.len() < MIN_SCRUB {
                continue;
            }
            v.push(s.to_string());
            v.push(b64(s.as_bytes()));
            v.push(b64url(s.as_bytes()));
        }
        // longest first, so a value containing another is replaced whole
        v.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        v.dedup();
        let max_len = v.first().map_or(0, String::len);
        Self { finders: v.iter().map(|s| memchr::memmem::Finder::new(s.as_bytes()).into_owned()).collect(), max_len }
    }

    pub fn is_empty(&self) -> bool {
        self.finders.is_empty()
    }

    /// Length of the longest value looked for.
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    pub fn found_in(&self, hay: &[u8]) -> bool {
        self.finders.iter().any(|f| f.find(hay).is_some())
    }

    /// `b` with every value replaced by `[redacted]`.
    pub fn bytes(&self, b: Vec<u8>) -> Vec<u8> {
        if !self.found_in(&b) {
            return b;
        }
        let mut cur = b;
        for f in &self.finders {
            if f.find(&cur).is_none() {
                continue;
            }
            let mut out = Vec::with_capacity(cur.len());
            let mut last = 0;
            for i in f.find_iter(&cur) {
                out.extend_from_slice(&cur[last..i]);
                out.extend_from_slice(REDACTED);
                last = i + f.needle().len();
            }
            out.extend_from_slice(&cur[last..]);
            cur = out;
        }
        cur
    }

    /// `s` with every value replaced (UTF-8 stays valid: values are whole
    /// UTF-8 strings, so they only match on character boundaries).
    pub fn text(&self, s: &str) -> Option<String> {
        if !self.found_in(s.as_bytes()) {
            return None;
        }
        let out = self.bytes(s.as_bytes().to_vec());
        Some(String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
    }

    /// Scrub `s` in place, scanning only what was added since `scanned`
    /// (plus an overlap for a value split across appends).
    pub fn tail(&self, s: &mut String, scanned: &mut usize) {
        if self.is_empty() {
            *scanned = s.len();
            return;
        }
        let mut start = (*scanned).min(s.len()).saturating_sub(self.max_len.saturating_sub(1));
        while !s.is_char_boundary(start) {
            start -= 1;
        }
        if self.found_in(&s.as_bytes()[start..]) {
            if let Some(t) = self.text(s) {
                *s = t;
            }
        }
        *scanned = s.len();
    }

    /// Every string and byte string inside `d`, scrubbed.
    pub fn data(&self, d: anypage_runtime::Data) -> anypage_runtime::Data {
        use anypage_runtime::Data;
        if self.is_empty() || !self.found_in_data(&d) {
            return d;
        }
        match d {
            Data::Str(s) => match self.text(&s) {
                Some(t) => Data::str(&t),
                None => Data::Str(s),
            },
            Data::Bytes(b) => {
                if self.found_in(&b) {
                    Data::bytes(self.bytes(b.to_vec()))
                } else {
                    Data::Bytes(b)
                }
            }
            Data::List(l) => Data::list(l.iter().cloned().map(|x| self.data(x)).collect()),
            Data::Map(m) => {
                let pairs: Vec<(String, Data)> = m.iter().map(|(k, v)| (self.text(k).unwrap_or_else(|| k.to_string()), self.data(v.clone()))).collect();
                Data::map(pairs.iter().map(|(k, v)| (k.as_str(), v.clone())))
            }
            other => other,
        }
    }

    fn found_in_data(&self, d: &anypage_runtime::Data) -> bool {
        use anypage_runtime::Data;
        match d {
            Data::Str(s) => self.found_in(s.as_bytes()),
            Data::Bytes(b) => self.found_in(b),
            Data::List(l) => l.iter().any(|x| self.found_in_data(x)),
            Data::Map(m) => m.iter().any(|(k, v)| self.found_in(k.as_bytes()) || self.found_in_data(v)),
            _ => false,
        }
    }
}

/// Cut `s` to at most `max` bytes on a char boundary, marking the cut.
pub fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes() {
        assert_eq!(sha256_hex(b""), EMPTY_SHA256);
        // RFC 4231 test case 2
        assert_eq!(hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    #[test]
    fn encoding() {
        assert_eq!(encode_strict("a b/c+d~é"), "a%20b%2Fc%2Bd~%C3%A9");
        assert_eq!(encode_path("/a b/c"), "/a%20b/c");
        assert_eq!(percent_decode("a%20b%2fc%zz%"), b"a b/c%zz%".to_vec());
        assert_eq!(parse_query("b=2&a=%41&c"), vec![("b".into(), "2".into()), ("a".into(), "A".into()), ("c".into(), "".into())]);
    }

    #[test]
    fn dates() {
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(951_782_400), "20000229T000000Z");
        assert_eq!(amz_date(4_102_444_799), "20991231T235959Z");
    }

    #[test]
    fn scrubber() {
        use anypage_runtime::Data;
        let sc = Scrubber::new(["sk-secret-123456", "short", "Bearer sk-secret-123456"]);
        // short values are ignored (they would match ordinary text)
        assert_eq!(sc.text("a short note"), None);
        assert_eq!(sc.text("auth: Bearer sk-secret-123456!").as_deref(), Some("auth: [redacted]!"));
        assert_eq!(sc.text("k=sk-secret-123456 and 中文 sk-secret-123456").as_deref(), Some("k=[redacted] and 中文 [redacted]"));
        let b = b64(b"sk-secret-123456");
        assert_eq!(sc.text(&format!("x{b}y")).as_deref(), Some("x[redacted]y"));
        assert_eq!(sc.bytes(b"\x00\xffsk-secret-123456\x01".to_vec()), b"\x00\xff[redacted]\x01".to_vec());
        // incremental: a value split across appends is caught when complete
        let mut s = String::from("abc sk-secret-");
        let mut scanned = 0;
        sc.tail(&mut s, &mut scanned);
        assert_eq!(s, "abc sk-secret-");
        s.push_str("123456 def");
        sc.tail(&mut s, &mut scanned);
        assert_eq!((s.as_str(), scanned), ("abc [redacted] def", s.len()));
        // nested data
        let d = Data::map([("h", Data::map([("x", Data::str("Bearer sk-secret-123456"))])), ("l", Data::list(vec![Data::bytes(b"sk-secret-123456".to_vec()), Data::Num(1.0)]))]);
        let c = sc.data(d);
        assert_eq!(c.get("h").get("x").as_str(), Some("[redacted]"));
        assert_eq!(c.get("l").at(1).as_bytes(), Some(&b"[redacted]"[..]));
        assert_eq!(c.get("l").at(2).num(), Some(1.0));
        assert!(Scrubber::default().is_empty());
        assert_eq!(format!("{sc:?}"), "Scrubber(9 values)".replace('9', &sc.finders.len().to_string()));
    }

    #[test]
    fn redaction_and_clip() {
        assert_eq!(redact("Bearer sk-abc123 failed", &["sk-abc123", ""]), "Bearer [redacted] failed");
        assert_eq!(clip("字字字", 4), "字…");
        assert_eq!(pem_block("x\n-----BEGIN K-----\nAAEC\n-----END K-----\n", "K"), Some(vec![0, 1, 2]));
    }
}
