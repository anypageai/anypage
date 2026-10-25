//! HLS playlists: reading master and media playlists (m3u8-rs), choosing a
//! variant, resolving segment URLs, and writing the sliding-window live
//! playlist the broadcaster serves.

use anypage_runtime::Data;
use m3u8_rs::Playlist;

/// Resolve `reference` (absolute, scheme-relative, absolute-path or relative)
/// against the URL of the document it appeared in.
pub fn resolve(base: &str, reference: &str) -> String {
    let r = reference.trim();
    if r.contains("://") {
        return r.to_string();
    }
    let (scheme, rest) = match base.split_once("://") {
        Some(p) => p,
        None => return r.to_string(),
    };
    if let Some(net) = r.strip_prefix("//") {
        return format!("{scheme}://{net}");
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let path_q = &rest[authority_end..];
    let path = path_q.split(['?', '#']).next().unwrap_or("");
    let (r_path, r_tail) = match r.find(['?', '#']) {
        Some(i) => (&r[..i], &r[i..]),
        None => (r, ""),
    };
    let joined = if r_path.starts_with('/') {
        r_path.to_string()
    } else if r_path.is_empty() {
        path.to_string()
    } else {
        let dir = match path.rfind('/') {
            Some(i) => &path[..=i],
            None => "/",
        };
        format!("{dir}{r_path}")
    };
    // remove dot segments
    let mut out: Vec<&str> = Vec::new();
    let segs: Vec<&str> = joined.split('/').collect();
    for (i, s) in segs.iter().enumerate() {
        match *s {
            "." => {
                if i == segs.len() - 1 {
                    out.push("");
                }
            }
            ".." => {
                if out.len() > 1 {
                    out.pop();
                }
                if i == segs.len() - 1 {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    let mut p = out.join("/");
    if !p.starts_with('/') {
        p.insert(0, '/');
    }
    let tail = if r_path.is_empty() && r_tail.is_empty() { path_q[path.len()..].to_string() } else { r_tail.to_string() };
    format!("{scheme}://{authority}{p}{tail}")
}

/// A segment to fetch.
#[derive(Clone, Debug, PartialEq)]
pub struct SegmentRef {
    pub sequence: u64,
    pub url: String,
    pub duration: f64,
    pub discontinuity: bool,
    /// (offset, length) for EXT-X-BYTERANGE.
    pub range: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    pub url: String,
    pub bandwidth: u64,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub codecs: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Parsed {
    Master(Vec<Variant>),
    Media(MediaInfo),
}

#[derive(Clone, Debug, PartialEq)]
pub struct MediaInfo {
    pub target_duration: f64,
    pub media_sequence: u64,
    /// No more segments will be added (VOD, or a finished live stream).
    pub ended: bool,
    pub segments: Vec<SegmentRef>,
}

pub const MAX_PLAYLIST: usize = 4 << 20;
/// Highest media sequence number accepted (2^53: exact in a Luau number, and
/// far from u64 overflow).
pub const MAX_SEQUENCE: u64 = 1 << 53;

/// Parse a playlist fetched from `url` (segment and variant URLs come out absolute).
pub fn parse(url: &str, bytes: &[u8]) -> Result<Parsed, String> {
    if bytes.len() > MAX_PLAYLIST {
        return Err("playlist larger than 4 MB".into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "playlist is not UTF-8 text".to_string())?;
    if !text.trim_start_matches('\u{feff}').trim_start().starts_with("#EXTM3U") {
        return Err("not an HLS playlist (no #EXTM3U)".into());
    }
    let pl = m3u8_rs::parse_playlist_res(bytes).map_err(|e| format!("bad playlist: {}", short_err(&format!("{e:?}"))))?;
    match pl {
        Playlist::MasterPlaylist(m) => {
            let v: Vec<Variant> = m
                .variants
                .iter()
                .filter(|v| !v.is_i_frame)
                .map(|v| Variant {
                    url: resolve(url, &v.uri),
                    bandwidth: v.bandwidth,
                    width: v.resolution.as_ref().map(|r| r.width),
                    height: v.resolution.as_ref().map(|r| r.height),
                    codecs: v.codecs.clone(),
                })
                .collect();
            if v.is_empty() {
                return Err("master playlist lists no variants".into());
            }
            Ok(Parsed::Master(v))
        }
        Playlist::MediaPlaylist(m) => {
            // sequence numbers must stay below u64::MAX so "the next one" always exists
            if m.media_sequence.checked_add(m.segments.len() as u64).is_none_or(|end| end > MAX_SEQUENCE) {
                return Err(format!("EXT-X-MEDIA-SEQUENCE {} is out of range", m.media_sequence));
            }
            let mut segments = Vec::with_capacity(m.segments.len());
            let mut next_offset: Option<(String, u64)> = None;
            for (i, s) in m.segments.iter().enumerate() {
                if let Some(k) = &s.key {
                    if !matches!(k.method, m3u8_rs::KeyMethod::None) {
                        return Err("encrypted HLS (EXT-X-KEY) is not supported".into());
                    }
                }
                if s.map.is_some() {
                    return Err("fragmented MP4 (EXT-X-MAP) segments are not supported; only MPEG-TS".into());
                }
                let seg_url = resolve(url, &s.uri);
                let range = s.byte_range.as_ref().map(|br| {
                    let offset = br.offset.unwrap_or_else(|| match &next_offset {
                        Some((u, o)) if *u == seg_url => *o,
                        _ => 0,
                    });
                    (offset, br.length)
                });
                if let Some((o, l)) = range {
                    // the last byte (o + l - 1) must be addressable
                    let end = o.checked_add(l).filter(|_| l > 0).ok_or_else(|| format!("EXT-X-BYTERANGE {l}@{o} is out of range"))?;
                    next_offset = Some((seg_url.clone(), end));
                }
                segments.push(SegmentRef {
                    sequence: m.media_sequence + i as u64,
                    url: seg_url,
                    duration: s.duration as f64,
                    discontinuity: s.discontinuity,
                    range,
                });
            }
            let vod = matches!(m.playlist_type, Some(m3u8_rs::MediaPlaylistType::Vod));
            Ok(Parsed::Media(MediaInfo { target_duration: m.target_duration as f64, media_sequence: m.media_sequence, ended: m.end_list || vod, segments }))
        }
    }
}

fn short_err(s: &str) -> String {
    s.chars().take(120).collect()
}

/// The variant to play: the best one whose height fits `max_height` (all if 0),
/// else the smallest.
pub fn choose_variant(v: &[Variant], max_height: u64) -> Option<&Variant> {
    let fits = |x: &&Variant| max_height == 0 || x.height.is_none_or(|h| h <= max_height);
    v.iter().filter(fits).max_by_key(|x| x.bandwidth).or_else(|| v.iter().min_by_key(|x| x.bandwidth))
}

/// A finished segment the broadcaster keeps in its ring.
#[derive(Clone, Debug)]
pub struct LiveSegment {
    pub sequence: u64,
    pub duration: f64,
    pub data: std::sync::Arc<Vec<u8>>,
}

pub fn segment_name(sequence: u64) -> String {
    format!("seg{sequence}.ts")
}

/// The live playlist for the last `list` segments of `ring`.
pub fn live_playlist(ring: &[LiveSegment], list: usize, ended: bool) -> String {
    let shown = &ring[ring.len().saturating_sub(list.max(1))..];
    let target = shown.iter().map(|s| s.duration.ceil() as u64).max().unwrap_or(1).max(1);
    let first = shown.first().map(|s| s.sequence).unwrap_or(0);
    let mut out = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
    for s in shown {
        out.push_str(&format!("#EXTINF:{:.3},\n{}\n", s.duration, segment_name(s.sequence)));
    }
    if ended {
        out.push_str("#EXT-X-ENDLIST\n");
    }
    out
}

/// A parsed playlist as script data (`media.playlist`).
pub fn to_data(p: &Parsed) -> Data {
    match p {
        Parsed::Master(v) => Data::map([
            ("kind", Data::str("master")),
            (
                "variants",
                Data::list(
                    v.iter()
                        .map(|x| {
                            Data::map([
                                ("url", Data::str(&x.url)),
                                ("bandwidth", Data::Num(x.bandwidth as f64)),
                                ("width", x.width.map(|w| Data::Num(w as f64)).unwrap_or_default()),
                                ("height", x.height.map(|h| Data::Num(h as f64)).unwrap_or_default()),
                                ("codecs", x.codecs.as_deref().map(Data::str).unwrap_or_default()),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]),
        Parsed::Media(m) => Data::map([
            ("kind", Data::str("media")),
            ("target_duration", Data::Num(m.target_duration)),
            ("media_sequence", Data::Num(m.media_sequence as f64)),
            ("live", Data::Bool(!m.ended)),
            ("duration", Data::Num(m.segments.iter().map(|s| s.duration).sum())),
            (
                "segments",
                Data::list(
                    m.segments
                        .iter()
                        .map(|s| {
                            Data::map([
                                ("url", Data::str(&s.url)),
                                ("sequence", Data::Num(s.sequence as f64)),
                                ("duration", Data::Num(s.duration)),
                                ("discontinuity", Data::Bool(s.discontinuity)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_urls() {
        let b = "https://cdn.example.com/live/stream/index.m3u8?token=1";
        assert_eq!(resolve(b, "seg1.ts"), "https://cdn.example.com/live/stream/seg1.ts");
        assert_eq!(resolve(b, "../hd/index.m3u8"), "https://cdn.example.com/live/hd/index.m3u8");
        assert_eq!(resolve(b, "/abs/x.ts?a=b"), "https://cdn.example.com/abs/x.ts?a=b");
        assert_eq!(resolve(b, "//other.example.com/y.ts"), "https://other.example.com/y.ts");
        assert_eq!(resolve(b, "http://x.test/z.ts"), "http://x.test/z.ts");
        assert_eq!(resolve("http://127.0.0.1:8080/live.m3u8", "seg3.ts"), "http://127.0.0.1:8080/seg3.ts");
        assert_eq!(resolve("http://h", "a.ts"), "http://h/a.ts");
        assert_eq!(resolve("http://h/a/b/c.m3u8", "./d/../e.ts"), "http://h/a/b/e.ts");
    }

    #[test]
    fn parses_media_and_master() {
        let media = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:40\n#EXTINF:2.000,\nseg40.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:1.500,\nseg41.ts\n";
        let Parsed::Media(m) = parse("http://h/x/live.m3u8", media).unwrap() else { panic!("media") };
        assert_eq!(m.media_sequence, 40);
        assert!(!m.ended);
        assert_eq!(m.segments.len(), 2);
        assert_eq!(m.segments[1].sequence, 41);
        assert_eq!(m.segments[1].url, "http://h/x/seg41.ts");
        assert!(m.segments[1].discontinuity);
        assert_eq!(m.segments[1].duration, 1.5);

        let master = b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\"\nhd/index.m3u8\n";
        let Parsed::Master(v) = parse("https://h/m.m3u8", master).unwrap() else { panic!("master") };
        assert_eq!(v.len(), 2);
        assert_eq!(choose_variant(&v, 0).unwrap().url, "https://h/hd/index.m3u8");
        assert_eq!(choose_variant(&v, 720).unwrap().url, "https://h/low/index.m3u8");
        assert_eq!(choose_variant(&v, 100).unwrap().url, "https://h/low/index.m3u8");
        assert_eq!(to_data(&Parsed::Master(v)).get("variants").items().len(), 2);
    }

    #[test]
    fn refuses_what_it_cannot_play() {
        assert!(parse("http://h/a", b"<html>").unwrap_err().contains("EXTM3U"));
        let enc = b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:2,\na.ts\n";
        assert!(parse("http://h/a", enc).unwrap_err().contains("encrypted"));
        let fmp4 = b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:2,\na.m4s\n";
        assert!(parse("http://h/a", fmp4).unwrap_err().contains("MP4"));
        assert!(parse("http://h/a", &[0xFF, 0xFE]).is_err());
    }

    #[test]
    fn live_window_round_trips_through_the_parser() {
        let ring: Vec<LiveSegment> = (7..15).map(|k| LiveSegment { sequence: k, duration: 0.5 + k as f64 * 0.01, data: Default::default() }).collect();
        let text = live_playlist(&ring, 4, false);
        let Parsed::Media(m) = parse("http://127.0.0.1:9/live.m3u8", text.as_bytes()).unwrap() else { panic!() };
        assert_eq!(m.media_sequence, 11);
        assert_eq!(m.segments.len(), 4);
        assert_eq!(m.target_duration, 1.0);
        assert_eq!(m.segments[3].url, "http://127.0.0.1:9/seg14.ts");
        assert!(!m.ended);
        assert!(live_playlist(&ring, 4, true).contains("#EXT-X-ENDLIST"));
        let Parsed::Media(m) = parse("http://a/b", live_playlist(&[], 3, false).as_bytes()).unwrap() else { panic!() };
        assert!(m.segments.is_empty());
    }

    #[test]
    fn byte_ranges() {
        let pl = b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\n#EXT-X-BYTERANGE:1000@0\nall.ts\n#EXTINF:2,\n#EXT-X-BYTERANGE:500\nall.ts\n";
        let Parsed::Media(m) = parse("http://h/p.m3u8", pl).unwrap() else { panic!() };
        assert_eq!(m.segments[0].range, Some((0, 1000)));
        assert_eq!(m.segments[1].range, Some((1000, 500)));
    }

    #[test]
    fn hostile_numbers_are_refused() {
        // found by review: these overflowed u64 arithmetic
        let range = b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\n#EXT-X-BYTERANGE:10@18446744073709551615\na.ts\n";
        assert!(parse("http://h/p.m3u8", range).unwrap_err().contains("BYTERANGE"));
        let seq = b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:18446744073709551615\n#EXTINF:2,\na.ts\n#EXTINF:2,\nb.ts\n";
        assert!(parse("http://h/p.m3u8", seq).unwrap_err().contains("MEDIA-SEQUENCE"));
        let empty = b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\n#EXT-X-BYTERANGE:0@5\na.ts\n";
        assert!(parse("http://h/p.m3u8", empty).is_err(), "a zero-length range has no last byte");
        // the largest accepted sequence still has a successor
        let ok = format!("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:{}\n#EXTINF:2,\na.ts\n", MAX_SEQUENCE - 1);
        let Parsed::Media(m) = parse("http://h/p.m3u8", ok.as_bytes()).unwrap() else { panic!() };
        assert_eq!(m.segments[0].sequence, MAX_SEQUENCE - 1);
        // a byte range ending exactly at u64::MAX is fine; the next implicit one is not
        let edge = b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\n#EXT-X-BYTERANGE:5@18446744073709551610\na.ts\n";
        assert!(parse("http://h/p.m3u8", edge).is_ok());
    }
}
