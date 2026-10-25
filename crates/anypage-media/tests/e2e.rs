//! The whole loop on one machine: synthetic frames -> H.264 -> MPEG-TS
//! segments -> live playlist -> served on 127.0.0.1 -> fetched and played
//! by the HLS player -> decoded frames compared with the pattern they came from.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anypage_engine::Image;
use anypage_media::broadcast::{BroadcastRequest, Broadcaster, Host, Source};
use anypage_media::pattern::{bar_at, bars_height, test_pattern, BARS};
use anypage_media::player::{Out, PlayRequest, Player};
use anypage_media::ts::CLOCK;
use anypage_media::yuv::{i420_to_rgba, psnr_rgba, ColorSpace};
use anypage_runtime::{Caps, Data, Grants};

use common::{save_png, wait_for};

fn grants(net: &[&str]) -> Arc<Grants> {
    Arc::new(Grants::new("e2e", Caps { net: net.iter().map(|s| s.to_string()).collect(), ..Caps::default() }))
}

type Items = Arc<Mutex<Vec<(Instant, Data)>>>;

fn collector() -> (Items, Out) {
    let items: Items = Arc::new(Mutex::new(Vec::new()));
    let i2 = items.clone();
    (items, Arc::new(move |d| {
        i2.lock().unwrap().push((Instant::now(), d));
        true
    }))
}

fn raw_get(port: u16, path: &str) -> (String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut all = Vec::new();
    s.read_to_end(&mut all).unwrap();
    let split = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    (String::from_utf8_lossy(&all[..split]).to_string(), all[split..].to_vec())
}

#[test]
fn encode_segment_serve_play_decode_compare() {
    let (w, h, fps) = (320usize, 240usize, 30.0);
    let g = grants(&["127.0.0.1"]);
    let on = Arc::new(AtomicBool::new(true));
    let quit = Arc::new(AtomicBool::new(false));
    let (b_items, b_out) = collector();
    let req = BroadcastRequest { source: Source::Test, width: w, height: h, fps, bitrate: 2_000_000, segment_seconds: 0.5, serve: Some(0), host: Host::Loopback, list_size: 6, preview: true, tailnet: false };
    let mut b = Broadcaster::start(req, &g, b_out, on.clone(), quit.clone()).expect("broadcast starts");
    let url = b.local_url.clone().expect("served");
    let port: u16 = url.split(':').nth(2).and_then(|s| s.split('/').next()).and_then(|p| p.parse().ok()).unwrap();
    wait_for(10.0, "two segments", || (b.segments().len() >= 2).then_some(()));

    // served with the right content types
    let (head, body) = raw_get(port, "/live.m3u8");
    assert!(head.contains("200 OK") && head.contains("Content-Type: application/vnd.apple.mpegurl"), "{head}");
    let text = String::from_utf8(body).unwrap();
    assert!(text.starts_with("#EXTM3U") && text.contains("#EXT-X-TARGETDURATION:1") && text.contains("seg0.ts"), "{text}");
    let (head, seg0) = raw_get(port, "/seg0.ts");
    assert!(head.contains("Content-Type: video/mp2t"), "{head}");
    assert_eq!(seg0.len() % 188, 0);
    let info = anypage_media::probe(&seg0);
    let v = info.get("video");
    assert_eq!((v.get("width").num(), v.get("height").num()), (Some(w as f64), Some(h as f64)));
    assert_eq!(v.get("keyframes").num(), Some(1.0), "one IDR, at the start of the segment");
    assert_eq!(v.get("frames").num(), Some(15.0), "0.5 s at 30 fps");
    assert!((v.get("duration").num().unwrap() - 0.5).abs() < 0.01, "{info:?}");
    assert_eq!(info.get("errors").num(), Some(0.0));
    let (head, _) = raw_get(port, "/seg999.ts");
    assert!(head.contains("404"), "{head}");

    // play it back
    let (p_items, p_out) = collector();
    let mut player = Player::start(PlayRequest { live_edge: 2, ..PlayRequest::new(&url) }, g.clone(), p_out, on.clone(), quit.clone()).expect("player starts");
    wait_for(15.0, "45 presented frames", || (player.stats.presented.load(std::sync::atomic::Ordering::Relaxed) >= 45).then_some(()));
    player.stop();
    b.stop();

    let items = p_items.lock().unwrap().clone();
    let errors: Vec<String> = items.iter().filter_map(|(_, d)| d.get("error").as_str().map(str::to_string)).collect();
    assert!(errors.is_empty(), "player errors: {errors:?}");
    let frames: Vec<(Instant, &Data)> = items.iter().filter(|(_, d)| !d.get("frame").is_nil() && d.get("pts").num().is_some()).map(|(t, d)| (*t, d)).collect();
    assert!(frames.len() >= 45);

    // every decoded frame matches the pattern frame its PTS names
    let cs = ColorSpace::BT601;
    let mut psnrs = Vec::new();
    let mut saved = false;
    for (_, d) in &frames {
        let pts = d.get("pts").num().unwrap();
        let n = ((pts - 1.0) * fps).round() as u64;
        let img = d.get("frame").as_res().unwrap().get::<Image>().unwrap();
        assert_eq!((img.w as usize, img.h as usize), (w, h));
        assert!(img.newest.is_some(), "decode time is stamped for latency stats");
        let reference = test_pattern(w, h, n, fps, cs);
        let mut ref_rgba = vec![0u8; w * h * 4];
        i420_to_rgba(&reference.planes(), cs, &mut ref_rgba, 1).unwrap();
        let psnr = psnr_rgba(&img.rgba, &ref_rgba);
        psnrs.push(psnr);
        // exact colour blocks: the middle of every bar
        for x in (4..w - 4).step_by(10) {
            let bar = bar_at(w, fps, n, x);
            if bar_at(w, fps, n, x - 4) != bar || bar_at(w, fps, n, x + 4) != bar {
                continue;
            }
            let y = bars_height(h) / 2;
            let i = (y * w + x) * 4;
            for (got, want) in img.rgba[i..i + 3].iter().zip(BARS[bar]) {
                assert!((*got as i32 - want as i32).abs() <= 12, "frame {n} x {x}: {:?} vs {:?}", &img.rgba[i..i + 3], BARS[bar]);
            }
        }
        if !saved {
            save_png("e2e_decoded.png", img.w, img.h, &img.rgba);
            save_png("e2e_reference.png", w as u32, h as u32, &ref_rgba);
            saved = true;
        }
    }
    let mean = psnrs.iter().sum::<f64>() / psnrs.len() as f64;
    let min = psnrs.iter().cloned().fold(f64::MAX, f64::min);
    println!("e2e: {} frames, PSNR mean {mean:.1} dB, min {min:.1} dB", psnrs.len());
    assert!(mean > 32.0 && min > 28.0, "PSNR mean {mean:.1} min {min:.1}");

    // paced by PTS against the wall clock: the gap between frames on the
    // wall matches the gap in media time
    let (t0, d0) = frames[frames.len() / 3];
    let p0 = d0.get("pts").num().unwrap();
    let mut errs = Vec::new();
    for (t, d) in &frames[frames.len() / 3..] {
        if d.get("epoch").num() != d0.get("epoch").num() {
            continue;
        }
        let media = d.get("pts").num().unwrap() - p0;
        let wall = t.duration_since(t0).as_secs_f64();
        errs.push((wall - media).abs());
    }
    errs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = errs[errs.len() / 2];
    println!("e2e: pacing error median {:.1} ms over {} frames", median * 1000.0, errs.len());
    assert!(median < 0.030, "frames are paced to their timestamps (median error {:.1} ms)", median * 1000.0);
    let last = frames.last().unwrap().1;
    assert!(last.get("fps").num().unwrap() > 20.0, "{last:?}");
    assert!(last.get("decode_ms").num().unwrap() > 0.0);

    // the broadcaster reported its state and previews
    let bitems = b_items.lock().unwrap();
    let with_preview = bitems.iter().filter(|(_, d)| d.get("preview").as_res().is_some()).count();
    assert!(with_preview >= 2);
    let lastb = &bitems.last().unwrap().1;
    assert_eq!(lastb.get("url").as_str(), Some(url.as_str()));
    assert!(lastb.get("bitrate").num().unwrap() > 50_000.0, "{lastb:?}");
    assert!(lastb.get("requests").num().unwrap() >= 3.0);
    let _ = CLOCK;
}

#[test]
fn slow_consumers_get_late_frames_dropped() {
    let g = grants(&["127.0.0.1"]);
    let on = Arc::new(AtomicBool::new(true));
    let quit = Arc::new(AtomicBool::new(false));
    let (_bi, b_out) = collector();
    let req = BroadcastRequest { width: 160, height: 96, segment_seconds: 0.5, serve: Some(0), host: Host::Loopback, preview: false, ..BroadcastRequest::default() };
    let mut b = Broadcaster::start(req, &g, b_out, on.clone(), quit.clone()).unwrap();
    let url = b.local_url.clone().unwrap();
    wait_for(10.0, "segments", || (b.segments().len() >= 2).then_some(()));
    // a consumer that needs 100 ms per frame cannot keep up with 30 fps
    let out: Out = Arc::new(|_d| {
        std::thread::sleep(Duration::from_millis(100));
        true
    });
    let mut player = Player::start(PlayRequest { live_edge: 2, ..PlayRequest::new(&url) }, g, out, on, quit).unwrap();
    wait_for(15.0, "dropped frames", || (player.stats.dropped.load(std::sync::atomic::Ordering::Relaxed) >= 10).then_some(()));
    let (presented, dropped, decoded) = (
        player.stats.presented.load(std::sync::atomic::Ordering::Relaxed),
        player.stats.dropped.load(std::sync::atomic::Ordering::Relaxed),
        player.stats.decoded.load(std::sync::atomic::Ordering::Relaxed),
    );
    player.stop();
    b.stop();
    println!("slow consumer: decoded {decoded}, presented {presented}, dropped {dropped}");
    assert!(presented < decoded, "late frames were skipped, not shown");
}

#[test]
fn pausing_holds_the_clock() {
    let g = grants(&["127.0.0.1"]);
    let on = Arc::new(AtomicBool::new(true));
    let quit = Arc::new(AtomicBool::new(false));
    let (_bi, b_out) = collector();
    let req = BroadcastRequest { width: 160, height: 96, segment_seconds: 0.5, serve: Some(0), host: Host::Loopback, preview: false, ..BroadcastRequest::default() };
    let mut b = Broadcaster::start(req, &g, b_out, Arc::new(AtomicBool::new(true)), quit.clone()).unwrap();
    let url = b.local_url.clone().unwrap();
    let (items, out) = collector();
    let mut player = Player::start(PlayRequest { live_edge: 2, ..PlayRequest::new(&url) }, g, out, on.clone(), quit.clone()).unwrap();
    wait_for(15.0, "frames", || (player.stats.presented.load(std::sync::atomic::Ordering::Relaxed) >= 10).then_some(()));
    on.store(false, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(100));
    let n = items.lock().unwrap().len();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(items.lock().unwrap().len(), n, "nothing is presented while paused");
    on.store(true, std::sync::atomic::Ordering::Relaxed);
    wait_for(5.0, "frames after resuming", || (items.lock().unwrap().len() > n + 5).then_some(()));
    // after a pause the stream continues without a burst of drops
    player.stop();
    b.stop();
}

#[test]
fn refusals_are_clear() {
    let on = Arc::new(AtomicBool::new(true));
    let quit = Arc::new(AtomicBool::new(false));
    // serving needs the bind address in capabilities.net
    let (_i, out) = collector();
    let req = BroadcastRequest { serve: Some(0), host: Host::Loopback, ..BroadcastRequest::default() };
    let e = Broadcaster::start(req, &grants(&["example.com"]), out, on.clone(), quit.clone()).err().unwrap();
    assert!(e.contains("capabilities.net") && e.contains("127.0.0.1"), "{e}");
    // playing a host the page did not declare
    let (_i, out) = collector();
    let e = Player::start(PlayRequest::new("https://cdn.example.org/live.m3u8"), grants(&["127.0.0.1"]), out, on.clone(), quit.clone()).err().unwrap();
    assert!(e.contains("capabilities.net"), "{e}");
    // plain http to the internet
    let (_i, out) = collector();
    let e = Player::start(PlayRequest::new("http://cdn.example.org/live.m3u8"), grants(&["cdn.example.org"]), out, on.clone(), quit.clone()).err().unwrap();
    assert!(e.contains("https"), "{e}");
    // a server that is not there: the player reports, then gives up
    let (items, out) = collector();
    let mut p = Player::start(PlayRequest::new("http://127.0.0.1:9/live.m3u8"), grants(&["127.0.0.1"]), out, on, quit).unwrap();
    wait_for(30.0, "an error naming the server", || items.lock().unwrap().iter().any(|(_, d)| d.get("error").as_str().is_some()).then_some(()));
    p.stop();
    let all = items.lock().unwrap();
    assert!(all.iter().any(|(_, d)| d.get("error").as_str().is_some_and(|e| e.contains("127.0.0.1:9"))), "{all:?}");
}

/// A VOD playlist served on 127.0.0.1 from in-memory segments.
fn serve_vod(segments: Vec<Vec<u8>>, durations: &[f64]) -> (anypage_media::http::Server, String) {
    use anypage_media::http::{loopback, Handler, Reply, Server};
    let mut pl = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:2\n");
    for (k, d) in durations.iter().enumerate() {
        pl.push_str(&format!("#EXTINF:{d:.3},\ns{k}.ts\n"));
    }
    pl.push_str("#EXT-X-ENDLIST\n");
    let pl = Arc::new(pl.into_bytes());
    let segs: Vec<Arc<Vec<u8>>> = segments.into_iter().map(Arc::new).collect();
    let handler: Arc<dyn Handler> = Arc::new(move |path: &str| -> Option<Reply> {
        if path == "/vod.m3u8" {
            return Some(("application/vnd.apple.mpegurl", pl.clone(), "no-cache"));
        }
        let k: usize = path.strip_prefix("/s")?.strip_suffix(".ts")?.parse().ok()?;
        segs.get(k).map(|s| ("video/mp2t", s.clone(), "max-age=60"))
    });
    let server = Server::start(loopback(), 0, handler).unwrap();
    let url = format!("http://127.0.0.1:{}/vod.m3u8", server.addr.port());
    (server, url)
}

/// One TS segment of the test pattern: frames `first..first + count`, the
/// first an IDR, timestamps from `pts0`.
fn pattern_segment(enc: &mut anypage_media::h264::VideoEncoder, first: u64, count: u64, pts0: u64) -> Vec<u8> {
    use anypage_media::ts::Muxer;
    let (w, h) = (enc.settings.width, enc.settings.height);
    let mut mux = Muxer::new(true, false);
    let mut ts = Vec::new();
    mux.write_tables(&mut ts);
    for k in 0..count {
        let n = first + k;
        let (au, key) = enc.encode(&test_pattern(w, h, n, 30.0, enc.color), k == 0).unwrap().unwrap();
        mux.write_video(&mut ts, pts0 + k * 3000, None, key, &au);
    }
    ts
}

#[test]
fn a_forward_timestamp_jump_does_not_freeze_the_picture() {
    use anypage_media::h264::{EncoderSettings, VideoEncoder};
    // segment 2 continues the picture but its timestamps jump one hour ahead,
    // without EXT-X-DISCONTINUITY (what an encoder restart often looks like)
    let mut enc = VideoEncoder::new(EncoderSettings { width: 160, height: 96, fps: 30.0, bitrate: 300_000, keyframe_interval: 0, threads: 1 }).unwrap();
    let jump = 3600 * CLOCK;
    let a = pattern_segment(&mut enc, 0, 30, CLOCK);
    let b = pattern_segment(&mut enc, 30, 30, CLOCK + jump + 30 * 3000);
    let (mut server, url) = serve_vod(vec![a, b], &[1.0, 1.0]);
    let (items, out) = collector();
    let on = Arc::new(AtomicBool::new(true));
    let mut p = Player::start(PlayRequest::new(&url), grants(&["127.0.0.1"]), out, on, Arc::new(AtomicBool::new(false))).unwrap();
    wait_for(10.0, "the end of the VOD", || items.lock().unwrap().iter().any(|(_, d)| d.get("ended").truthy()).then_some(()));
    let stalls = p.stats.stalls.load(std::sync::atomic::Ordering::Relaxed);
    p.stop();
    server.stop();
    let all = items.lock().unwrap();
    let frames: Vec<(Instant, f64)> = all.iter().filter_map(|(t, d)| d.get("pts").num().map(|p| (*t, p))).collect();
    let before: Vec<&(Instant, f64)> = frames.iter().filter(|(_, p)| *p < 100.0).collect();
    let after: Vec<&(Instant, f64)> = frames.iter().filter(|(_, p)| *p > 3600.0).collect();
    println!("timestamp jump: {} frames before, {} after, stalls {stalls}", before.len(), after.len());
    assert!(before.len() >= 20 && after.len() >= 20, "{} / {}", before.len(), after.len());
    let gap = after[0].0.duration_since(before.last().unwrap().0);
    assert!(gap < Duration::from_millis(500), "the picture went on after the jump ({gap:?})");
    assert!(stalls >= 1, "the jump is counted as a stall");
    // and the frames after the jump are paced again (about 1 s for 30 frames)
    let span = after.last().unwrap().0.duration_since(after[0].0).as_secs_f64();
    assert!(span > 0.6, "frames after the jump are paced, not flushed ({span:.2} s)");
}

/// The review's crafted packet: a PES that announces a PTS and a DTS in a
/// header too short to hold them.
fn short_pes_segment() -> Vec<u8> {
    use anypage_media::ts::{Muxer, PID_VIDEO, SYNC};
    let mut ts = Vec::new();
    Muxer::new(true, false).write_tables(&mut ts);
    let mut p = vec![SYNC, 0x40 | (PID_VIDEO >> 8) as u8, PID_VIDEO as u8, 0x30, 172, 0x00];
    p.resize(4 + 1 + 172, 0xFF);
    p.extend_from_slice(&[0, 0, 1, 0xE0, 0, 0, 0x80, 0xC0, 0x00, 0x21, 0x00]);
    ts.extend_from_slice(&p);
    ts
}

#[test]
fn a_malformed_segment_ends_playback_instead_of_freezing_it() {
    let seg = short_pes_segment();
    let info = anypage_media::probe(&seg);
    assert!(info.get("errors").num().unwrap_or(0.0) >= 1.0, "{info:?}");
    assert!(anypage_media::thumbnail(&seg, &|| false).is_err());
    let (mut server, url) = serve_vod(vec![seg], &[1.0]);
    let (items, out) = collector();
    let mut p = Player::start(PlayRequest::new(&url), grants(&["127.0.0.1"]), out, Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false))).unwrap();
    wait_for(10.0, "an end item", || items.lock().unwrap().iter().any(|(_, d)| d.get("ended").truthy() || d.get("error").as_str().is_some()).then_some(()));
    wait_for(5.0, "every player thread to end", || p.finished().then_some(()));
    p.stop();
    server.stop();
}

#[test]
fn stopping_does_not_wait_for_a_silent_server() {
    use std::net::TcpListener;
    // a server that accepts and never answers: the fetcher sits in an HTTP call
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming().flatten() {
            held.push(s);
        }
    });
    let (_items, out) = collector();
    let mut p = Player::start(PlayRequest::new(&format!("http://127.0.0.1:{port}/live.m3u8")), grants(&["127.0.0.1"]), out, Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false))).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    p.stop();
    let took = t.elapsed();
    let t = Instant::now();
    drop(p);
    let dropped = t.elapsed();
    println!("Player::stop with a silent server: {took:?}; drop after that: {dropped:?}");
    assert!(took < Duration::from_millis(600), "stop took {took:?}");
    assert!(dropped < Duration::from_millis(50), "a stopped player drops at once ({dropped:?})");
}

#[cfg(feature = "audio")]
#[test]
fn plays_aac_audio_with_the_video() {
    use anypage_media::audio::{aac_noise_block, adts_wrap};
    use anypage_media::h264::{EncoderSettings, VideoEncoder};
    use anypage_media::http::{loopback, Handler, Reply, Server};
    use anypage_media::ts::Muxer;

    // two one-second VOD segments: 160x96 video and 48 kHz mono AAC (noise bursts)
    let mut enc = VideoEncoder::new(EncoderSettings { width: 160, height: 96, fps: 30.0, bitrate: 300_000, keyframe_interval: 30, threads: 1 }).unwrap();
    // far below audibility: the test may run on a machine with speakers
    let noise = adts_wrap(&aac_noise_block(40, 256 - 40), 3, 1);
    let silence = adts_wrap(&aac_noise_block(0, 0), 3, 1);
    let mut segments = Vec::new();
    let mut apts = CLOCK;
    for s in 0..2u64 {
        let mut mux = Muxer::new(true, true);
        let mut ts = Vec::new();
        mux.write_tables(&mut ts);
        for k in 0..30u64 {
            let n = s * 30 + k;
            let pts = CLOCK + n * 3000;
            let (au, key) = enc.encode(&test_pattern(160, 96, n, 30.0, enc.color), k == 0).unwrap().unwrap();
            mux.write_video(&mut ts, pts, None, key, &au);
            while apts < pts + 3000 {
                let frame = if (apts / 1920) % 25 < 3 { &noise } else { &silence };
                mux.write_audio(&mut ts, apts, frame);
                apts += 1920; // 1024 samples at 48 kHz
            }
        }
        segments.push(Arc::new(ts));
    }
    let playlist = Arc::new(b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.000,\ns0.ts\n#EXTINF:1.000,\ns1.ts\n#EXT-X-ENDLIST\n".to_vec());
    let segs = segments.clone();
    let handler: Arc<dyn Handler> = Arc::new(move |path: &str| -> Option<Reply> {
        match path {
            "/vod.m3u8" => Some(("application/vnd.apple.mpegurl", playlist.clone(), "no-cache")),
            "/s0.ts" => Some(("video/mp2t", segs[0].clone(), "max-age=60")),
            "/s1.ts" => Some(("video/mp2t", segs[1].clone(), "max-age=60")),
            _ => None,
        }
    });
    let mut server = Server::start(loopback(), 0, handler).unwrap();
    let url = format!("http://127.0.0.1:{}/vod.m3u8", server.addr.port());
    let probe = anypage_media::probe(&segments[0]);
    assert_eq!(probe.get("audio").get("sample_rate").num(), Some(48_000.0), "{probe:?}");
    assert_eq!(probe.get("audio").get("frames").num(), Some(47.0), "one second of 1024-sample frames at 48 kHz: {probe:?}");

    let (items, out) = collector();
    let on = Arc::new(AtomicBool::new(true));
    let quit = Arc::new(AtomicBool::new(false));
    let mut p = Player::start(PlayRequest::new(&url), grants(&["127.0.0.1"]), out, on, quit).unwrap();
    wait_for(20.0, "the end of the VOD", || items.lock().unwrap().iter().any(|(_, d)| d.get("ended").truthy()).then_some(()));
    p.stop();
    server.stop();
    let all = items.lock().unwrap();
    let statuses: std::collections::BTreeSet<String> = all.iter().filter_map(|(_, d)| d.get("audio").as_str().map(str::to_string)).collect();
    let presented = all.iter().filter(|(_, d)| d.get("pts").num().is_some()).count();
    println!("audio statuses: {statuses:?}; {presented} frames presented");
    assert!(presented >= 45, "{presented}");
    assert!(statuses.iter().any(|s| s.starts_with("playing") || s.starts_with("unavailable") || s == "buffering" || s == "starting"), "{statuses:?}");
    assert!(!statuses.iter().any(|s| s.starts_with("unsupported")), "{statuses:?}");
    // whichever clock led (audio when a device played it), frames kept media time
    let frames: Vec<(Instant, f64)> = all.iter().filter_map(|(t, d)| d.get("pts").num().map(|p| (*t, p))).collect();
    let (t0, p0) = frames[frames.len() / 4];
    let mut errs: Vec<f64> = frames[frames.len() / 4..].iter().map(|(t, p)| (t.duration_since(t0).as_secs_f64() - (p - p0)).abs()).collect();
    errs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("audio e2e: pacing error median {:.1} ms", errs[errs.len() / 2] * 1000.0);
    assert!(errs[errs.len() / 2] < 0.040);
}
