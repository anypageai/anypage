//! Just enough XML for S3 responses (ListObjectsV2, errors): element text
//! and repeated element blocks, with entity decoding. Not a general parser:
//! S3 documents are flat, attribute-free (apart from `xmlns`) and use no
//! CDATA or comments in the parts read here.

/// Find `<tag ...>` (not `<tagSuffix>`) from `from`; returns (start of the
/// open tag, end of the open tag, self-closing).
fn find_open(xml: &str, tag: &str, from: usize) -> Option<(usize, usize, bool)> {
    let pat = format!("<{tag}");
    let mut pos = from;
    while let Some(i) = xml[pos..].find(&pat) {
        let start = pos + i;
        let after = start + pat.len();
        match xml.as_bytes().get(after) {
            Some(b'>') => return Some((start, after + 1, false)),
            Some(b' ' | b'\t' | b'\r' | b'\n' | b'/') => {
                let close = after + xml[after..].find('>')?;
                let self_closing = xml[..close].ends_with('/');
                return Some((start, close + 1, self_closing));
            }
            _ => pos = after,
        }
    }
    None
}

/// The raw inner text of every `<tag>` element, in document order (not nested
/// inside another `<tag>`).
pub fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some((_, inner_start, self_closing)) = find_open(xml, tag, pos) {
        if self_closing {
            out.push("");
            pos = inner_start;
            continue;
        }
        match xml[inner_start..].find(&close) {
            Some(j) => {
                out.push(&xml[inner_start..inner_start + j]);
                pos = inner_start + j + close.len();
            }
            None => break,
        }
    }
    out
}

/// Decoded text of the first `<tag>` element.
pub fn text(xml: &str, tag: &str) -> Option<String> {
    blocks(xml, tag).first().map(|s| decode(s))
}

/// Decode the five named entities and numeric references.
pub fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|e| *e <= 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                let n = if let Some(h) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(h, 16).ok()
                } else if let Some(d) = ent.strip_prefix('#') {
                    d.parse().ok()
                } else {
                    None
                };
                n.and_then(char::from_u32)
            }
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_result() {
        let x = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Prefix>p/</Prefix><KeyCount>2</KeyCount>
<IsTruncated>true</IsTruncated><NextContinuationToken>tok&amp;1</NextContinuationToken>
<Contents><Key>p/a&lt;1&gt;.txt</Key><Size>5</Size><ETag>&quot;abc&quot;</ETag></Contents>
<Contents><Key>p/&#x4E2D;&#25991;</Key><Size>0</Size></Contents>
<CommonPrefixes><Prefix>p/sub/</Prefix></CommonPrefixes><Marker/></ListBucketResult>"#;
        let c = blocks(x, "Contents");
        assert_eq!(c.len(), 2);
        assert_eq!(text(c[0], "Key").as_deref(), Some("p/a<1>.txt"));
        assert_eq!(text(c[0], "ETag").as_deref(), Some("\"abc\""));
        assert_eq!(text(c[1], "Key").as_deref(), Some("p/中文"));
        assert_eq!(text(x, "NextContinuationToken").as_deref(), Some("tok&1"));
        assert_eq!(blocks(x, "CommonPrefixes").iter().filter_map(|b| text(b, "Prefix")).collect::<Vec<_>>(), vec!["p/sub/".to_string()]);
        assert_eq!(text(x, "Marker").as_deref(), Some(""));
        // <Key> must not match <KeyCount>
        assert_eq!(text(x, "Key").as_deref(), Some("p/a<1>.txt"));
        assert_eq!(decode("a & b &bogus; &#xZZ;"), "a & b &bogus; &#xZZ;");
    }
}
