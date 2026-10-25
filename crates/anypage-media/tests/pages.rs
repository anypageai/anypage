//! Headless page tests: real packages driven through the Luau API.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anypage_core::{CellId, Status};
use anypage_engine::{Image, Page, PointerResponse, TextRaster, Wake};
use anypage_media::h264::{EncoderSettings, VideoEncoder};
use anypage_media::pattern::{test_pattern, BARS};
use anypage_media::ts::{Muxer, CLOCK};
use anypage_runtime::data::input_to_data;
use anypage_runtime::{Data, Natives, Package, ScriptPage};

use common::save_png;

fn natives() -> Natives {
    let mut n = Natives::new();
    anypage_media::register(&mut n);
    n
}

fn text() -> Arc<TextRaster> {
    Arc::new(TextRaster::load().expect("a system font"))
}

fn wake() -> Wake {
    Arc::new(|| {})
}

fn load(caps: &str, src: &str) -> Result<ScriptPage, String> {
    let m = format!(r#"{{ "format": 1, "id": "media-test", "bounds": [0, 0, 400, 300], "capabilities": {caps} }}"#);
    let pkg = Package::from_files("test", [("page.json", m.as_str()), ("main.luau", src)])?;
    ScriptPage::load(&pkg, &natives(), text(), wake())
}

fn value(p: &mut ScriptPage, cell: CellId) -> (Status, Data) {
    let g = p.parts().0;
    g.commit();
    let i = g.demand(cell);
    (i.status.clone(), input_to_data(&i))
}

fn wait_cell(p: &mut ScriptPage, name: &str, secs: f64, mut ok: impl FnMut(&Data) -> bool) -> Data {
    let cell = p.cell_id(name).unwrap_or_else(|| panic!("no cell {name}"));
    let t0 = Instant::now();
    loop {
        let (status, d) = value(p, cell);
        if matches!(status, Status::Ready) && ok(&d) {
            return d;
        }
        assert!(t0.elapsed() < Duration::from_secs_f64(secs), "timed out waiting on {name}: {d:?} {status:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn panel_image(p: &mut ScriptPage, x: f32, y: f32) -> Arc<Image> {
    let cell = p.spec().panels.iter().find(|pl| pl.rect.x == x && pl.rect.y == y).map(|pl| pl.cell).expect("panel at that position");
    let g = p.parts().0;
    g.commit();
    let i = g.demand(cell);
    let v = i.value.clone().expect("panel value");
    v.downcast::<Image>().expect("an image")
}

fn near_a_bar(px: &[u8], tol: i32) -> bool {
    BARS.iter().any(|c| (0..3).all(|k| (px[k] as i32 - c[k] as i32).abs() <= tol))
}

/// Share of pixels in the top half of `img` that are one of the bar colours.
fn bar_coverage(img: &Image) -> f64 {
    let (w, h) = (img.w as usize, img.h as usize);
    let mut hit = 0;
    let mut n = 0;
    for y in (h / 10..h / 2).step_by(7) {
        for x in (0..w).step_by(5) {
            let i = (y * w + x) * 4;
            n += 1;
            if near_a_bar(&img.rgba[i..i + 4], 16) {
                hit += 1;
            }
        }
    }
    hit as f64 / n as f64
}

#[test]
fn live_demo_page_broadcasts_and_plays() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pages/live-demo");
    let pkg = Package::from_dir(&dir).expect("demo package");
    let mut p = ScriptPage::load(&pkg, &natives(), text(), wake()).expect("demo page loads");
    println!("{}", p.summary());

    // the player plays the page's own broadcast
    let v = wait_cell(&mut p, "video", 25.0, |d| d.get("last").get("presented").num().unwrap_or(0.0) >= 15.0);
    let last = v.get("last");
    assert_eq!((last.get("width").num(), last.get("height").num()), (Some(1280.0), Some(720.0)), "{last:?}");
    assert!(last.get("error").is_nil(), "{last:?}");
    let b = wait_cell(&mut p, "broadcast", 5.0, |d| d.get("last").get("segments").num().unwrap_or(0.0) >= 1.0);
    let bl = b.get("last");
    assert_eq!(bl.get("url").as_str(), Some("http://127.0.0.1:18090/live.m3u8"));
    println!(
        "demo broadcast: {} frames, {} skipped, encode {} ms/frame, {} bit/s; player decode {} ms, convert {} ms, dropped {}",
        bl.get("frames").num().unwrap_or(0.0),
        bl.get("skipped").num().unwrap_or(0.0),
        bl.get("encode_ms").num().unwrap_or(0.0),
        bl.get("bitrate").num().unwrap_or(0.0),
        last.get("decode_ms").num().unwrap_or(0.0),
        last.get("convert_ms").num().unwrap_or(0.0),
        last.get("dropped").num().unwrap_or(0.0),
    );

    // what is on screen: the video image is the decoded frame itself, the preview too
    let video = panel_image(&mut p, 660.0, 92.0);
    assert_eq!((video.w, video.h), (1280, 720));
    let cov = bar_coverage(&video);
    assert!(cov > 0.85, "the played picture shows the colour bars ({cov:.2})");
    save_png("live_demo_video.png", video.w, video.h, &video.rgba);
    let preview = panel_image(&mut p, 20.0, 92.0);
    assert!(preview.w >= 320 && bar_coverage(&preview) > 0.85);
    save_png("live_demo_preview.png", preview.w, preview.h, &preview.rgba);
    let stats = panel_image(&mut p, 20.0, 456.0);
    save_png("live_demo_stats.png", stats.w, stats.h, &stats.rgba);
    let light = stats.rgba.as_chunks::<4>().0.iter().filter(|px| px[0] > 200 && px[1] > 200 && px[2] > 200).count();
    assert!(light > 500, "stats text drawn ({light} light pixels)");

    // the semantic port
    let d: serde_json::Value = serde_json::from_str(&p.describe()).unwrap();
    let actions: Vec<&str> = d["actions"].as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap()).collect();
    for a in ["play", "stop", "broadcast", "stop_broadcast", "share"] {
        assert!(actions.contains(&a), "{actions:?}");
    }
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "stop"}"#)).unwrap();
    assert_eq!(r["ok"], true);
    let stopped_at = wait_cell(&mut p, "video", 2.0, |_| true).get("last").get("presented").num().unwrap_or(0.0);
    std::thread::sleep(Duration::from_millis(400));
    let after = wait_cell(&mut p, "video", 2.0, |_| true).get("last").get("presented").num().unwrap_or(0.0);
    assert!(after <= stopped_at + 2.0, "playback stopped ({stopped_at} -> {after})");

    // the Play button (second button) starts it again
    let resp = p.pointer_down(20.0 + 220.0 + 16.0 + 50.0, 692.0 + 20.0);
    assert_eq!(resp, PointerResponse::Handled);
    wait_cell(&mut p, "video", 20.0, |d| d.get("last").get("presented").num().unwrap_or(0.0) >= 5.0 && d.get("last").get("presented").num() != Some(after));

    // a bigger broadcast through an action; the player follows the restarted stream
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"action": "broadcast", "params": {"width": 480, "height": 270}}"#)).unwrap();
    assert_eq!(r["result"]["width"], 480);
    wait_cell(&mut p, "video", 30.0, |d| d.get("last").get("width").num() == Some(480.0));
    p.shutdown();
}

fn ts_segment(w: usize, h: usize, frames: u64) -> Vec<u8> {
    let mut enc = VideoEncoder::new(EncoderSettings { width: w, height: h, fps: 30.0, bitrate: 1_000_000, keyframe_interval: 0, threads: 1 }).unwrap();
    let mut mux = Muxer::new(true, false);
    let mut ts = Vec::new();
    mux.write_tables(&mut ts);
    for n in 0..frames {
        let (au, key) = enc.encode(&test_pattern(w, h, n, 30.0, enc.color), false).unwrap().unwrap();
        mux.write_video(&mut ts, CLOCK + n * 3000, None, key, &au);
    }
    ts
}

#[test]
fn functions_and_jobs_through_luau() {
    let src = r##"
        page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        local img = media.test_pattern({ width = 320, height = 180, frame = 0 })
        local s = img:size()
        assert(s.w == 320 and s.h == 180, "size")
        local white = img:pixel(20, 20)
        assert(white.r > 240 and white.g > 240 and white.b > 240, "first bar is white")
        local yellow = img:pixel(60, 20)
        assert(yellow.r > 240 and yellow.g > 240 and yellow.b < 20, "second bar is yellow")
        local ok, err = pcall(media.pixel, img, 999, 0)
        assert(not ok and string.find(tostring(err), "outside"), "pixel bounds")
        local pl = media.playlist("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:5\n#EXTINF:2,\na.ts\n", "http://h/x/p.m3u8")
        assert(pl.kind == "media" and pl.live and pl.segments[1].url == "http://h/x/a.ts" and pl.segments[1].sequence == 5, "playlist")
        local bad, perr = pcall(media.playlist, "not a playlist")
        assert(not bad and string.find(tostring(perr), "EXTM3U"), "bad playlist")

        Seg = page.input("seg", nil)
        Info = page.input("info", nil)
        Thumb = page.job("thumb", "media.thumbnail", { Seg })
        page.on("file_drop", function(f)
            page.set(Seg, f.data)
            page.set(Info, media.probe(f.data))
            return true
        end)
    "##;
    let mut p = load(r#"{"modules": ["media"]}"#, src).expect("page loads");
    let seg = ts_segment(256, 144, 20);
    p.file_drop("clip.ts", Arc::new(seg));
    let info = p.cell_value("info").unwrap();
    let v = info.get("video");
    assert_eq!((v.get("codec").as_str(), v.get("width").num(), v.get("height").num()), (Some("h264"), Some(256.0), Some(144.0)), "{info:?}");
    assert_eq!(v.get("frames").num(), Some(20.0));
    assert_eq!(v.get("keyframes").num(), Some(1.0));
    assert_eq!(v.get("profile").as_str(), Some("baseline"));
    assert!((v.get("fps").num().unwrap() - 30.0).abs() < 0.01);
    assert_eq!(info.get("streams").items().len(), 1);
    let thumb = wait_cell(&mut p, "thumb", 10.0, |d| d.as_res().is_some());
    let img = thumb.as_res().unwrap().get::<Image>().unwrap();
    assert_eq!((img.w, img.h), (256, 144));
    assert!(bar_coverage(img) > 0.9, "the thumbnail is the first pattern frame");
    save_png("thumbnail.png", img.w, img.h, &img.rgba);
}

/// The review's crafted packet: a PES that announces a PTS and a DTS in a
/// header too short to hold them (it used to panic inside `media.probe`).
fn short_pes_segment() -> Vec<u8> {
    use anypage_media::ts::{PID_VIDEO, SYNC};
    let mut ts = Vec::new();
    Muxer::new(true, false).write_tables(&mut ts);
    let mut p = vec![SYNC, 0x40 | (PID_VIDEO >> 8) as u8, PID_VIDEO as u8, 0x30, 172, 0x00];
    p.resize(4 + 1 + 172, 0xFF);
    p.extend_from_slice(&[0, 0, 1, 0xE0, 0, 0, 0x80, 0xC0, 0x00, 0x21, 0x00]);
    ts.extend_from_slice(&p);
    ts
}

#[test]
fn hostile_segments_through_luau_without_pcall() {
    let src = r#"
        page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        Info = page.input("info", nil)
        Seg = page.input("seg", nil)
        Thumb = page.job("thumb", "media.thumbnail", { Seg })
        page.on("file_drop", function(f)
            page.set(Seg, f.data)
            page.set(Info, media.probe(f.data))
            return true
        end)
    "#;
    let mut p = load(r#"{"modules": ["media"]}"#, src).expect("page loads");
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.file_drop("bad.ts", Arc::new(short_pes_segment()))));
    assert!(r.is_ok(), "no panic reaches the host");
    assert_eq!(p.handler_errors, 0, "probe returned normally");
    let info = p.cell_value("info").unwrap();
    assert!(info.get("errors").num().unwrap_or(0.0) >= 1.0, "the short header is reported as a demux error: {info:?}");
    assert_eq!(info.get("video").get("frames").num(), Some(0.0));
    // the thumbnail job on the worker pool fails with a message instead of panicking
    let cell = p.cell_id("thumb").unwrap();
    let t0 = Instant::now();
    let status = loop {
        let (status, _) = value(&mut p, cell);
        if !matches!(status, Status::Pending { .. } | Status::Empty) {
            break status;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "thumbnail still {status:?}");
        std::thread::sleep(Duration::from_millis(20));
    };
    match status {
        Status::Error(e) => assert!(e.contains("no picture"), "{e}"),
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn streams_must_keep_only_the_latest_frame() {
    let src = |opts: &str| format!("page.panel({{ rect = {{ 0, 0, 10, 10 }}, draw = function(d) end }}) local req = page.input('req', nil) page.stream('video', 'media.hls', {{ request = req{opts} }})");
    let e = load(r#"{"modules": ["media"]}"#, &src("")).err().expect("refused without latest = true");
    assert!(e.contains("latest = true"), "{e}");
    let e = load(r#"{"modules": ["media"]}"#, &src(", cap = 200")).err().expect("a large cap is refused too");
    assert!(e.contains("latest = true"), "{e}");
    let mut ok = load(r#"{"modules": ["media"]}"#, &src(", cap = 4")).expect("a small explicit cap is fine");
    ok.shutdown();
    let mut ok = load(r#"{"modules": ["media"]}"#, &src(", latest = true")).expect("latest = true is fine");
    ok.shutdown();
}

/// A TCP server that accepts connections and never answers.
fn silent_server() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming().flatten() {
            held.push(s);
        }
    });
    port
}

#[test]
fn closing_or_changing_a_stream_never_waits_for_the_network() {
    let port = silent_server();
    let src = format!(
        r#"
        page.panel({{ rect = {{ 0, 0, 10, 10 }}, draw = function(d) end }})
        local req = page.input("req", {{ url = "http://127.0.0.1:{port}/live.m3u8" }}, {{ agent = true }})
        page.stream("video", "media.hls", {{ request = req, latest = true }})
    "#
    );
    let caps = r#"{"modules": ["media"], "net": ["127.0.0.1"]}"#;
    // a request change: the old session (stuck in an HTTP call) must not delay the new one
    let mut p = load(caps, &src).expect("page loads");
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"set": "req", "value": {"url": "https://elsewhere.example.com/x.m3u8"}}"#)).unwrap();
    assert_eq!(r["ok"], true);
    wait_cell(&mut p, "video", 5.0, |d| d.get("last").get("error").as_str().is_some_and(|e| e.contains("capabilities.net")));
    let switched = t.elapsed();
    p.shutdown();
    drop(p);
    // closing a page while its player is stuck in an HTTP call
    let mut p = load(caps, &src).expect("page loads");
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    p.shutdown();
    drop(p);
    let closed = t.elapsed();
    println!("silent server: request change answered in {switched:?}, page shutdown + drop took {closed:?}");
    assert!(switched < Duration::from_millis(1500), "request change took {switched:?}");
    assert!(closed < Duration::from_millis(200), "page close took {closed:?}");
}

#[test]
fn undeclared_hosts_and_bad_requests_come_back_as_errors() {
    let src = r#"
        page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        local req = page.input("req", { url = "https://elsewhere.example.com/live.m3u8" }, { agent = true })
        page.stream("video", "media.hls", { request = req, latest = true })
        local breq = page.input("breq", { source = "test", serve = 0, host = "loopback" }, { agent = true })
        page.stream("bcast", "media.broadcast", { request = breq, latest = true })
    "#;
    let mut p = load(r#"{"modules": ["media"], "net": ["example.org"]}"#, src).expect("page loads");
    let v = wait_cell(&mut p, "video", 5.0, |d| d.get("last").get("error").as_str().is_some());
    assert!(v.get("last").get("error").as_str().unwrap().contains("capabilities.net"), "{v:?}");
    let b = wait_cell(&mut p, "bcast", 5.0, |d| d.get("last").get("error").as_str().is_some());
    assert!(b.get("last").get("error").as_str().unwrap().contains("127.0.0.1"), "{b:?}");
    // a malformed request is reported, not a crash
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"set": "breq", "value": {"source": "satellite"}}"#)).unwrap();
    assert_eq!(r["ok"], true);
    wait_cell(&mut p, "bcast", 5.0, |d| d.get("last").get("error").as_str().is_some_and(|e| e.contains("satellite")));
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"set": "req", "value": {"url": 5}}"#)).unwrap();
    assert_eq!(r["ok"], true);
    wait_cell(&mut p, "video", 5.0, |d| d.get("last").get("error").as_str().is_some_and(|e| e.contains("url must be a string")));
    // a page without the module cannot use it
    let e = load(r#"{}"#, "page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end }) page.stream('v', 'media.hls', {})").err().unwrap();
    assert!(e.contains("media.hls"), "{e}");
    p.shutdown();
}
