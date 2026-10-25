//! Media module (`media`): HLS playback (H.264 + AAC in MPEG-TS), live
//! broadcasting to HLS from a test pattern or a camera, and the pieces they
//! are made of (MPEG-TS demuxer/muxer, H.264 via OpenH264 built from source,
//! YUV/RGBA conversion, playlists, a local-network HTTP server).
//!
//! A page declares `"capabilities": { "modules": ["media"], "net": [...] }`.
//! Every URL the player fetches (and every redirect) must match
//! `capabilities.net`; plain `http` is accepted only for local-network hosts
//! (loopback, RFC 1918, link-local, unique-local). Serving a broadcast needs
//! the bind address in `capabilities.net`: `"127.0.0.1"` (this device) or
//! `"0.0.0.0"` (the local network), optionally with the port
//! (`"0.0.0.0:8080"`). Carrier-grade NAT space (100.64.0.0/10) is not local:
//! on mobile networks it is shared with other subscribers. A request with
//! `tailnet = true` accepts it, for overlay networks such as Tailscale.
//!
//! # Luau API
//!
//! Sources. Each item carries a whole video frame, so a stream must be
//! declared with `latest = true` (the stream value's `last` is the newest
//! item), or with an explicit `cap` of at most 16; anything else is refused
//! when the page loads.
//!
//! | source | request cell value | items |
//! |---|---|---|
//! | `media.hls` | `{ url, live_edge = 3, max_height = 0, audio = true, buffer = 8, loop = false, tailnet = false }` or `nil` (stop) | `{ frame = image, pts, width, height, fps, buffered, dropped, decoded, presented, segments, stalls, decode_ms, convert_ms, audio, url, last_error, epoch }`; `{ error, frame = last }`; `{ ended = true, frame = last }` |
//! | `media.broadcast` | `{ source = "test" \| "camera", camera = 0, width, height, fps, bitrate, segment_seconds, serve = port, host = "lan" \| "loopback", list_size = 5, preview = true, tailnet = false }` or `nil` | `{ url, local_url, segments, sequence, bitrate, frames, skipped, encode_ms, fps, width, height, requests, source, preview = image }`; `{ error }` |
//!
//! Closing a page or changing a request never waits on the network: the
//! page's guard only signals the stream's own thread, and an old session is
//! given at most 250 ms ([`player::STOP_WAIT`]) before the new one starts;
//! a fetch still inside an HTTP call ends on its own at its timeout. A panic
//! inside the module becomes an `{ error }` item (sources) or an error result
//! (functions, jobs); none reaches the runtime or the host.
//!
//! ```lua
//! local req = page.input("req", { url = "https://example.com/live/index.m3u8" })
//! local video = page.stream("video", "media.hls", { request = req, latest = true })
//! local frame = page.derive("frame", { video }, function(v) return v and v.last and v.last.frame end)
//! page.image({ rect = { 0, 0, 1280, 720 }, source = frame })
//! ```
//!
//! Functions (pure unless noted):
//!
//! | function | result |
//! |---|---|
//! | `media.probe(bytes)` | a TS segment's `{ streams = { {pid, codec, stream_type} }, video = {codec, width, height, profile, frames, keyframes, fps, duration}, audio = {codec, sample_rate, channels, frames, duration}, duration, errors }` |
//! | `media.playlist(text, base_url)` | `{ kind = "media", live, target_duration, media_sequence, duration, segments = { {url, sequence, duration, discontinuity} } }` or `{ kind = "master", variants = { {url, bandwidth, width, height, codecs} } }` |
//! | `media.test_pattern{ width, height, frame, fps }` | image: the broadcast test pattern |
//! | `media.size(img)` / `img:size()` | `{ w, h }` |
//! | `media.pixel(img, x, y)` / `img:pixel(x, y)` | `{ r, g, b, a }` |
//! | `media.cameras()` | camera names, or an error naming why there are none. An effect (it asks the devices): at load and in handlers only. On Windows it enumerates on a short-lived thread of its own, so it works from the host's STA UI thread. |
//!
//! Jobs: `media.thumbnail` (input: TS bytes) -> the first decodable picture as an image.
//!
//! Images are `media.image` resources holding an [`anypage_engine::Image`]
//! (premultiplied RGBA8; `newest` is when it was decoded): `page.image{source = cell}`
//! shows them without a copy and `d:image(img, x, y, w, h)` draws them in a panel.
//!
//! Features: `audio` (AAC-LC through symphonia to cpal; the video follows
//! the audio clock while sound plays; HE-AAC plays as its LC core, see
//! [`audio`]), `camera` (Windows Media Foundation).
//! Android and iOS cameras come from the host through
//! [`camera::set_host_camera`].
//!
//! H.264 is decoded and encoded by Cisco's OpenH264 compiled from source
//! (BSD-2-Clause). Its patent licence covers only Cisco's own binaries: an app
//! that ships this build is responsible for H.264 patent licensing where it
//! applies.

pub mod audio;
pub mod broadcast;
pub mod camera;
pub mod h264;
pub mod hls;
pub mod http;
pub mod pattern;
pub mod player;
pub mod ts;
pub mod yuv;

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anypage_engine::Image;
use anypage_runtime::{Call, Data, Effect, Grants, JobCtx, Natives, Res, SourceCtx, SourceGuard};

use crate::broadcast::{BroadcastRequest, Broadcaster};
use crate::player::{Out, PlayRequest, Player};

/// Add this module's functions, jobs and sources.
pub fn register(n: &mut Natives) {
    n.source("media.hls", |ctx| {
        run_source(ctx, "media.hls", |req, grants, out, on, quit| match PlayRequest::from_data(req)? {
            Some(r) => Ok(Some(Box::new(Player::start(r, grants.clone(), out, on, quit)?) as Box<dyn Any + Send>)),
            None => Ok(None),
        })
    });
    n.source("media.broadcast", |ctx| {
        run_source(ctx, "media.broadcast", |req, grants, out, on, quit| match BroadcastRequest::from_data(req)? {
            Some(r) => Ok(Some(Box::new(Broadcaster::start(r, grants, out, on, quit)?) as Box<dyn Any + Send>)),
            None => Ok(None),
        })
    });
    n.function("media.probe", Effect::Pure, |c: &Call| {
        let bytes = c.arg(0).as_bytes().ok_or("probe needs the bytes of an MPEG-TS segment")?;
        guarded("media.probe", || Ok(probe(bytes)))
    });
    n.function("media.playlist", Effect::Pure, |c: &Call| {
        let text = c.arg(0).as_bytes().ok_or("playlist needs the playlist text")?;
        let base = c.arg(1).str_or("http://localhost/");
        guarded("media.playlist", || Ok(hls::to_data(&hls::parse(base, text)?)))
    });
    n.function("media.test_pattern", Effect::Pure, |c: &Call| {
        let o = c.arg(0);
        let num = |k: &str, d: f64| o.get(k).num().filter(|x| x.is_finite()).unwrap_or(d);
        let w = (num("width", 640.0) as usize).clamp(2, 3840) & !1;
        let h = (num("height", 360.0) as usize).clamp(2, 2160) & !1;
        let fps = num("fps", 30.0).clamp(1.0, 240.0);
        let frame = num("frame", 0.0).max(0.0) as u64;
        guarded("media.test_pattern", || {
            let cs = yuv::ColorSpace::for_height(h);
            let pic = pattern::test_pattern(w, h, frame, fps, cs);
            let mut rgba = vec![0u8; w * h * 4];
            yuv::i420_to_rgba_auto(&pic.planes(), cs, &mut rgba)?;
            Ok(Data::res("media.image", Image { w: w as u32, h: h as u32, rgba, newest: Some(Instant::now()) }))
        })
    });
    n.function("media.size", Effect::Pure, |c: &Call| {
        let img = image_arg(c.arg(0))?;
        Ok(Data::map([("w", Data::Num(img.w as f64)), ("h", Data::Num(img.h as f64))]))
    });
    n.function("media.pixel", Effect::Pure, |c: &Call| {
        let img = image_arg(c.arg(0))?;
        let (x, y) = (c.arg(1).num_or(-1.0), c.arg(2).num_or(-1.0));
        if !(x >= 0.0 && y >= 0.0 && (x as u32) < img.w && (y as u32) < img.h) {
            return Err(format!("pixel ({x}, {y}) is outside the {}x{} image", img.w, img.h));
        }
        let i = ((y as usize) * img.w as usize + x as usize) * 4;
        let p = img.rgba.get(i..i + 4).ok_or("image data is shorter than its size")?;
        Ok(Data::map([("r", Data::Num(p[0] as f64)), ("g", Data::Num(p[1] as f64)), ("b", Data::Num(p[2] as f64)), ("a", Data::Num(p[3] as f64))]))
    });
    // the device list depends on the outside world: load time and handlers only
    n.function("media.cameras", Effect::Effect, |_c: &Call| guarded("media.cameras", || Ok(Data::list(camera::list()?.iter().map(|s| Data::str(s)).collect()))));
    n.job_fn("media.thumbnail", |j: &JobCtx| {
        let bytes = j.input(0).as_bytes().ok_or("thumbnail needs the bytes of an MPEG-TS segment")?;
        guarded("media.thumbnail", || thumbnail(bytes, &|| j.cancelled()))
    });
}

fn image_arg(d: &Data) -> Result<&Image, String> {
    d.as_res().and_then(|r| r.get::<Image>()).ok_or_else(|| "expected an image".to_string())
}

/// The text of a caught panic.
pub(crate) fn panic_message(e: &(dyn Any + Send)) -> String {
    e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "unknown panic".into())
}

/// Run `f`, turning a panic into an error string: no panic crosses into the
/// runtime, the host, or a worker pool from this module.
pub(crate) fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(e) => Err(format!("{what}: internal error ({})", panic_message(&*e))),
    }
}

/// Wait at most `limit` for `threads` to end; the ones still running are
/// returned (dropping a handle detaches its thread, which then ends by itself
/// once it sees its stop flag). Media threads never join blocking network or
/// capture work from an engine or UI thread.
pub(crate) fn join_within(threads: Vec<JoinHandle<()>>, limit: Duration) -> Vec<JoinHandle<()>> {
    let end = Instant::now() + limit;
    let mut pending = threads;
    loop {
        let (done, rest): (Vec<_>, Vec<_>) = pending.into_iter().partition(|t| t.is_finished());
        for t in done {
            let _ = t.join();
        }
        pending = rest;
        if pending.is_empty() || Instant::now() >= end {
            return pending;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Starts a session for a request; `Ok(None)` means "nothing to run".
type Start = fn(&Data, &Arc<Grants>, Out, Arc<AtomicBool>, Arc<AtomicBool>) -> Result<Option<Box<dyn Any + Send>>, String>;

/// A stream's guard: dropping it (page shutdown, on the engine thread) only
/// signals the controller thread, which then stops its session on its own
/// time. Nothing here waits for a network or camera call to return.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Most items a stream without `latest = true` may keep: every item carries a
/// full picture (8 MB at 1080p), and the runtime's default keeps 256.
pub const MAX_APPEND_CAP: f64 = 16.0;

/// One controller thread per stream: every new request value replaces the
/// running session (dropping a session stops its threads).
fn run_source(ctx: SourceCtx, name: &str, start: Start) -> Result<SourceGuard, String> {
    let latest = ctx.opts.get("latest").truthy();
    let small_cap = ctx.opts.get("cap").num().is_some_and(|c| c <= MAX_APPEND_CAP);
    if !latest && !small_cap {
        return Err(format!(
            "{name} items carry whole video frames: declare the stream with latest = true \
             (or an explicit cap of at most {MAX_APPEND_CAP}), e.g. page.stream(\"{}\", \"{name}\", {{ request = req, latest = true }})",
            ctx.label
        ));
    }
    let (on, quit) = ctx.flags();
    let sink = ctx.sink.clone();
    let grants = ctx.grants.clone();
    let opts = ctx.opts.clone();
    let requests = ctx.requests;
    let out: Out = Arc::new(move |d| sink.push(d));
    let done = Arc::new(AtomicBool::new(false));
    let (q, d2) = (quit.clone(), done.clone());
    let what = name.to_string();
    std::thread::Builder::new()
        .name("media-source".into())
        .spawn(move || {
            let halt = || q.load(Ordering::Relaxed) || d2.load(Ordering::Relaxed);
            let mut session: Option<Box<dyn Any + Send>> = None;
            let apply = |req: &Data, session: &mut Option<Box<dyn Any + Send>>| {
                // stopping the old session waits briefly at most (see join_within)
                *session = None;
                match guarded(&what, || start(req, &grants, out.clone(), on.clone(), q.clone())) {
                    Ok(s) => *session = s,
                    Err(e) => {
                        out(Data::map([("error", Data::str(&e))]));
                    }
                }
            };
            // a request in the stream's own options starts at once
            if !opts.get("url").is_nil() || !opts.get("source").is_nil() {
                apply(&opts, &mut session);
            }
            while !halt() {
                match requests.recv_timeout(Duration::from_millis(50)) {
                    Ok(r) => apply(&r, &mut session),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
            drop(session);
        })
        .map_err(|e| format!("cannot start the media thread: {e}"))?;
    Ok(Box::new(StopOnDrop(done)))
}

/// What a TS segment holds.
pub fn probe(bytes: &[u8]) -> Data {
    let (streams, pes, errors) = ts::demux_all(bytes);
    let mut sps = None;
    let (mut vframes, mut keys, mut vfirst, mut vlast) = (0u64, 0u64, None::<u64>, None::<u64>);
    let (mut aframes, mut ahead) = (0u64, None::<audio::Adts>);
    let mut unwrap = ts::Unwrap::default();
    for p in &pes {
        match p.codec {
            ts::Codec::H264 => {
                vframes += 1;
                if h264::is_keyframe(&p.data) {
                    keys += 1;
                }
                if sps.is_none() {
                    sps = h264::find_sps(&p.data);
                }
                if let Some(t) = p.pts {
                    let t = unwrap.unwrap(t);
                    vfirst = Some(vfirst.map_or(t, |f| f.min(t)));
                    vlast = Some(vlast.map_or(t, |l| l.max(t)));
                }
            }
            ts::Codec::Aac => {
                let frames = audio::adts_frames(&p.data);
                if ahead.is_none() {
                    ahead = frames.first().map(|f| f.0);
                }
                aframes += frames.len() as u64;
            }
            _ => {}
        }
    }
    let span = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (Some(a), Some(b)) => (b - a) as f64 / ts::CLOCK as f64,
        _ => 0.0,
    };
    let vdur = if vframes > 1 { span(vfirst, vlast) * vframes as f64 / (vframes - 1) as f64 } else { 0.0 };
    let video = if streams.iter().any(|s| s.codec.is_video()) {
        let codec = streams.iter().find(|s| s.codec.is_video()).map(|s| s.codec.name()).unwrap_or_default();
        let fps = sps.as_ref().and_then(|s| s.fps).or_else(|| (vdur > 0.0).then(|| vframes as f64 / vdur));
        Data::map([
            ("codec", Data::str(&codec)),
            ("width", sps.as_ref().map(|s| Data::Num(s.width as f64)).unwrap_or_default()),
            ("height", sps.as_ref().map(|s| Data::Num(s.height as f64)).unwrap_or_default()),
            ("profile", sps.as_ref().map(|s| Data::str(s.profile_name())).unwrap_or_default()),
            ("frames", Data::Num(vframes as f64)),
            ("keyframes", Data::Num(keys as f64)),
            ("fps", fps.map(|f| Data::Num((f * 1000.0).round() / 1000.0)).unwrap_or_default()),
            ("duration", Data::Num((vdur * 1000.0).round() / 1000.0)),
        ])
    } else {
        Data::Nil
    };
    let audio = if streams.iter().any(|s| s.codec == ts::Codec::Aac) {
        Data::map([
            ("codec", Data::str("aac")),
            ("sample_rate", ahead.map(|h| Data::Num(h.sample_rate as f64)).unwrap_or_default()),
            ("channels", ahead.map(|h| Data::Num(h.channels as f64)).unwrap_or_default()),
            ("frames", Data::Num(aframes as f64)),
            ("duration", ahead.map(|h| Data::Num((aframes as f64 * 1024.0 / h.sample_rate as f64 * 1000.0).round() / 1000.0)).unwrap_or_default()),
        ])
    } else {
        Data::Nil
    };
    Data::map([
        (
            "streams",
            Data::list(
                streams
                    .iter()
                    .map(|s| Data::map([("pid", Data::Num(s.pid as f64)), ("codec", Data::str(&s.codec.name())), ("stream_type", Data::Num(s.stream_type as f64))]))
                    .collect(),
            ),
        ),
        ("video", video),
        ("audio", audio),
        ("duration", Data::Num((vdur * 1000.0).round() / 1000.0)),
        ("errors", Data::Num(errors as f64)),
    ])
}

/// The first picture of a TS segment.
pub fn thumbnail(bytes: &[u8], cancelled: &dyn Fn() -> bool) -> Result<Data, String> {
    let (streams, pes, _) = ts::demux_all(bytes);
    if !streams.iter().any(|s| s.codec == ts::Codec::H264) {
        return Err("no H.264 video in this segment".into());
    }
    let mut dec = h264::VideoDecoder::new()?;
    let mut started = false;
    for p in pes.iter().filter(|p| p.codec == ts::Codec::H264) {
        if cancelled() {
            return Err("cancelled".into());
        }
        if !started && !h264::is_keyframe(&p.data) {
            continue;
        }
        started = true;
        let mut frames = dec.decode(&p.data, p.pts.unwrap_or(0)).unwrap_or_default();
        if frames.is_empty() {
            continue;
        }
        let f = frames.remove(0);
        return to_image(&f);
    }
    let f = dec.flush().into_iter().next().ok_or("no picture could be decoded")?;
    to_image(&f)
}

fn to_image(f: &h264::DecodedFrame) -> Result<Data, String> {
    let rgba = f.to_rgba()?;
    Ok(Data::Res(Res::new("media.image", Image { w: f.width() as u32, h: f.height() as u32, rgba, newest: Some(Instant::now()) })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panics_become_errors() {
        let r: Result<(), String> = guarded("media.test", || panic!("boom {}", 7));
        let e = r.unwrap_err();
        assert!(e.contains("media.test") && e.contains("boom 7"), "{e}");
        assert_eq!(guarded("x", || Ok(5)), Ok(5));
        // the review's crafted segment through the public functions
        let seg = ts::tests::short_pes_with_dts_flags();
        assert!(probe(&seg).get("errors").num().unwrap_or(0.0) >= 1.0);
        assert!(thumbnail(&seg, &|| false).unwrap_err().contains("no picture"));
    }

    #[test]
    fn join_within_waits_only_so_long() {
        let quick = std::thread::spawn(|| std::thread::sleep(Duration::from_millis(10)));
        let slow = std::thread::spawn(|| std::thread::sleep(Duration::from_millis(1500)));
        let t = Instant::now();
        let left = join_within(vec![quick, slow], Duration::from_millis(200));
        assert!(t.elapsed() < Duration::from_millis(600), "{:?}", t.elapsed());
        assert_eq!(left.len(), 1, "the slow thread is handed back, still running");
    }
}
