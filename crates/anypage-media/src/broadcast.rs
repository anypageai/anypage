//! `media.broadcast`: capture (test pattern or camera) -> H.264 -> MPEG-TS
//! segments in a ring -> live HLS playlist -> served on the local network.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anypage_engine::Image;
use anypage_runtime::{Data, Grants, Res};

use crate::camera;
use crate::h264::{EncoderSettings, VideoEncoder};
use crate::hls::{live_playlist, LiveSegment};
use crate::http::{Handler, Peers, Reply, Server};
use crate::panic_message;
use crate::pattern::test_pattern;
use crate::player::Out;
use crate::ts::{Muxer, CLOCK};
use crate::yuv::{i420_to_rgba_sampled, ColorSpace, I420};

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Test,
    Camera(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    /// Every interface (reachable from the local network; other peers are refused).
    Lan,
    /// This machine only.
    Loopback,
}

/// What a page asks `media.broadcast` for.
#[derive(Clone, Debug, PartialEq)]
pub struct BroadcastRequest {
    pub source: Source,
    pub width: usize,
    pub height: usize,
    pub fps: f64,
    pub bitrate: u32,
    pub segment_seconds: f64,
    /// Port to serve on (0: any free port); `None`: do not serve.
    pub serve: Option<u16>,
    pub host: Host,
    /// Segments listed in the playlist.
    pub list_size: usize,
    /// Small RGBA previews of what is being sent, a few per second.
    pub preview: bool,
    /// With `host = Lan`, also serve peers in 100.64.0.0/10 (an overlay
    /// network such as Tailscale). Off by default: on mobile carriers that
    /// range is shared with other subscribers.
    pub tailnet: bool,
}

impl Default for BroadcastRequest {
    fn default() -> Self {
        Self {
            source: Source::Test,
            width: 1280,
            height: 720,
            fps: 30.0,
            bitrate: 2_500_000,
            segment_seconds: 2.0,
            serve: None,
            host: Host::Lan,
            list_size: 5,
            preview: true,
            tailnet: false,
        }
    }
}

impl BroadcastRequest {
    /// `{ source, width, height, fps, bitrate, segment_seconds, serve = port, host, list_size, preview, camera, tailnet }`,
    /// or `nil` to stop.
    pub fn from_data(d: &Data) -> Result<Option<Self>, String> {
        if d.is_nil() || matches!(d.get("on").as_bool(), Some(false)) {
            return Ok(None);
        }
        if !matches!(d, Data::Map(_)) {
            return Err("request must be a table like { source = \"test\", serve = 8080 } or nil".into());
        }
        let def = Self::default();
        let num = |k: &str| d.get(k).num().filter(|x| x.is_finite());
        let source = match d.get("source").str_or("test") {
            "test" => Source::Test,
            "camera" => Source::Camera(num("camera").unwrap_or(0.0).clamp(0.0, 64.0) as usize),
            other => return Err(format!("unknown source \"{other}\" (test, camera)")),
        };
        let even = |x: f64| ((x as usize) & !1).clamp(16, 3840);
        let width = even(num("width").unwrap_or(def.width as f64));
        let height = even(num("height").unwrap_or(def.height as f64)).min(2160);
        let serve = match d.get("serve") {
            Data::Nil | Data::Bool(false) => None,
            Data::Bool(true) => Some(8080),
            Data::Num(p) if (0.0..=65535.0).contains(p) => Some(*p as u16),
            _ => return Err("serve must be a port number 0..65535 (0 = any free port)".into()),
        };
        let host = match d.get("host").str_or("lan") {
            "lan" => Host::Lan,
            "loopback" | "local" => Host::Loopback,
            other => return Err(format!("unknown host \"{other}\" (lan, loopback)")),
        };
        Ok(Some(Self {
            source,
            width,
            height,
            fps: num("fps").unwrap_or(def.fps).clamp(1.0, 60.0),
            bitrate: num("bitrate").unwrap_or(def.bitrate as f64).clamp(100_000.0, 20_000_000.0) as u32,
            segment_seconds: num("segment_seconds").unwrap_or(def.segment_seconds).clamp(0.25, 10.0),
            serve,
            host,
            list_size: num("list_size").unwrap_or(def.list_size as f64).clamp(2.0, 20.0) as usize,
            preview: d.get("preview").as_bool().unwrap_or(true),
            tailnet: d.get("tailnet").truthy(),
        }))
    }
}

/// Counters for one broadcast.
#[derive(Default)]
pub struct Stats {
    pub frames: AtomicU64,
    pub skipped: AtomicU64,
    pub segments: AtomicU64,
    pub bytes: AtomicU64,
    pub encode_ns: AtomicU64,
    pub capture_ns: AtomicU64,
}

impl Stats {
    pub fn encode_ms(&self) -> f64 {
        self.encode_ns.load(Ordering::Relaxed) as f64 / self.frames.load(Ordering::Relaxed).max(1) as f64 / 1e6
    }
}

/// The ring of finished segments and the playlist describing it.
pub struct Live {
    pub ring: VecDeque<LiveSegment>,
    pub playlist: Arc<Vec<u8>>,
    pub ended: bool,
    list_size: usize,
}

impl Live {
    fn new(list_size: usize) -> Self {
        Self { ring: VecDeque::new(), playlist: Arc::new(live_playlist(&[], list_size, false).into_bytes()), ended: false, list_size }
    }
    fn add(&mut self, s: LiveSegment) {
        self.ring.push_back(s);
        // keep a few more than listed, for clients still reading an older playlist
        while self.ring.len() > self.list_size + 3 {
            self.ring.pop_front();
        }
        self.refresh();
    }
    fn refresh(&mut self) {
        let v: Vec<LiveSegment> = self.ring.iter().cloned().collect();
        self.playlist = Arc::new(live_playlist(&v, self.list_size, self.ended).into_bytes());
    }
}

fn lock(m: &Mutex<Live>) -> MutexGuard<'_, Live> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub struct Broadcaster {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    server: Option<Server>,
    pub live: Arc<Mutex<Live>>,
    pub stats: Arc<Stats>,
    /// Where other devices on the network can play it.
    pub url: Option<String>,
    /// Where this device can play it.
    pub local_url: Option<String>,
}

fn handler(live: Arc<Mutex<Live>>) -> Arc<dyn Handler> {
    Arc::new(move |path: &str| -> Option<Reply> {
        let l = lock(&live);
        if path == "/live.m3u8" || path == "/index.m3u8" {
            return Some(("application/vnd.apple.mpegurl", l.playlist.clone(), "no-cache"));
        }
        let seq: u64 = path.strip_prefix("/seg")?.strip_suffix(".ts")?.parse().ok()?;
        l.ring.iter().find(|s| s.sequence == seq).map(|s| ("video/mp2t", s.data.clone(), "max-age=60"))
    })
}

impl Broadcaster {
    /// Start broadcasting. Encoding pauses while `on` is false; `quit` (the
    /// page's) and [`Broadcaster::stop`] end it.
    pub fn start(req: BroadcastRequest, grants: &Grants, out: Out, on: Arc<AtomicBool>, quit: Arc<AtomicBool>) -> Result<Broadcaster, String> {
        let live = Arc::new(Mutex::new(Live::new(req.list_size)));
        let stats = Arc::new(Stats::default());
        let stop = Arc::new(AtomicBool::new(false));
        // serving needs the bind address in capabilities.net ("127.0.0.1" or "0.0.0.0", optionally with the port)
        let (server, url, local_url) = match req.serve {
            None => (None, None, None),
            Some(port) => {
                let bind: IpAddr = match req.host {
                    Host::Lan => crate::http::unspecified(),
                    Host::Loopback => crate::http::loopback(),
                };
                let probe = if port == 0 { format!("http://{bind}/live.m3u8") } else { format!("http://{bind}:{port}/live.m3u8") };
                grants.check_url(&probe).map_err(|e| format!("serving on {bind}:{port} needs \"{bind}\" or \"{bind}:{port}\" in capabilities.net ({e})"))?;
                let peers = Peers { tailnet: req.tailnet && req.host == Host::Lan };
                let server = Server::start_with(bind, port, handler(live.clone()), peers)?;
                let p = server.addr.port();
                let local = format!("http://127.0.0.1:{p}/live.m3u8");
                let public = match req.host {
                    Host::Loopback => local.clone(),
                    Host::Lan => match crate::http::lan_ip() {
                        Some(ip) => format!("http://{ip}:{p}/live.m3u8"),
                        None => local.clone(),
                    },
                };
                (Some(server), Some(public), Some(local))
            }
        };
        // open the camera here so a missing one is an immediate, clear error
        let cam = match &req.source {
            Source::Test => None,
            Source::Camera(i) => Some(camera::open(*i, req.width, req.height, req.fps)?),
        };
        let ctx = Ctx {
            req: req.clone(),
            live: live.clone(),
            stats: stats.clone(),
            out,
            halt: {
                let (s, q) = (stop.clone(), quit);
                Arc::new(move || s.load(Ordering::Relaxed) || q.load(Ordering::Relaxed))
            },
            url: url.clone(),
            local_url: local_url.clone(),
            requests: server.as_ref().map(|s| s.requests.clone()),
            on,
        };
        let thread = std::thread::Builder::new()
            .name("media-broadcast".into())
            .spawn(move || {
                let r = catch_unwind(AssertUnwindSafe(|| encode_loop(&ctx, cam))).unwrap_or_else(|e| Err(format!("internal error in the broadcast: {}", panic_message(&*e))));
                if let Err(e) = r {
                    (ctx.out)(ctx.item(None, vec![("error", Data::str(&e))]));
                }
                let mut l = lock(&ctx.live);
                l.ended = true;
                l.refresh();
            })
            .map_err(|e| format!("cannot start the broadcast thread: {e}"))?;
        Ok(Broadcaster { stop, thread: Some(thread), server, live, stats, url, local_url })
    }

    /// Stop encoding and serving. Waits at most [`crate::player::STOP_WAIT`]
    /// for the encoder (a camera read still in progress finishes on its own);
    /// the listening socket closes before this returns.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = crate::join_within(vec![t], crate::player::STOP_WAIT);
        }
        if let Some(mut s) = self.server.take() {
            s.stop();
        }
    }

    pub fn segments(&self) -> Vec<LiveSegment> {
        lock(&self.live).ring.iter().cloned().collect()
    }
}

impl Drop for Broadcaster {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Ctx {
    req: BroadcastRequest,
    live: Arc<Mutex<Live>>,
    stats: Arc<Stats>,
    out: Out,
    halt: Arc<dyn Fn() -> bool + Send + Sync>,
    url: Option<String>,
    local_url: Option<String>,
    requests: Option<Arc<AtomicU64>>,
    on: Arc<AtomicBool>,
}

impl Ctx {
    fn item(&self, preview: Option<&Res>, extra: Vec<(&str, Data)>) -> Data {
        let s = &self.stats;
        let (seq, bitrate) = {
            let l = lock(&self.live);
            match l.ring.back() {
                Some(seg) => (seg.sequence as f64, seg.data.len() as f64 * 8.0 / seg.duration.max(0.01)),
                None => (-1.0, 0.0),
            }
        };
        let mut v: Vec<(&str, Data)> = vec![
            ("url", self.url.as_deref().map(Data::str).unwrap_or_default()),
            ("local_url", self.local_url.as_deref().map(Data::str).unwrap_or_default()),
            ("segments", Data::Num(s.segments.load(Ordering::Relaxed) as f64)),
            ("sequence", Data::Num(seq)),
            ("bitrate", Data::Num(bitrate.round())),
            ("frames", Data::Num(s.frames.load(Ordering::Relaxed) as f64)),
            ("skipped", Data::Num(s.skipped.load(Ordering::Relaxed) as f64)),
            ("encode_ms", Data::Num((s.encode_ms() * 100.0).round() / 100.0)),
            ("fps", Data::Num(self.req.fps)),
            ("width", Data::Num(self.req.width as f64)),
            ("height", Data::Num(self.req.height as f64)),
            ("requests", Data::Num(self.requests.as_ref().map(|r| r.load(Ordering::Relaxed)).unwrap_or(0) as f64)),
            ("source", Data::str(match self.req.source {
                Source::Test => "test",
                Source::Camera(_) => "camera",
            })),
        ];
        if let Some(p) = preview {
            v.push(("preview", Data::Res(p.clone())));
        }
        v.extend(extra);
        Data::map(v)
    }
}

fn wait_until(halt: &dyn Fn() -> bool, t: Instant) -> bool {
    loop {
        if halt() {
            return false;
        }
        let now = Instant::now();
        if now >= t {
            return true;
        }
        std::thread::sleep((t - now).min(Duration::from_millis(10)));
    }
}

fn preview_of(pic: &I420, cs: ColorSpace) -> Option<Res> {
    let step = pic.width.div_ceil(480).max(1);
    let (w, h, rgba) = i420_to_rgba_sampled(&pic.planes(), cs, step).ok()?;
    Some(Res::new("media.image", Image { w: w as u32, h: h as u32, rgba, newest: Some(Instant::now()) }))
}

/// Media time of the first frame (keeps PCR = PTS - 0.1 s positive).
const PTS0: u64 = CLOCK;

fn encode_loop(ctx: &Ctx, mut cam: Option<Box<dyn camera::CameraStream>>) -> Result<(), String> {
    let req = &ctx.req;
    let (w, h) = match &cam {
        Some(c) => {
            let (cw, ch) = c.size();
            // encode at the camera's size, capped to 1080p
            let scale = (1920.0 / cw.max(1) as f64).min(1080.0 / ch.max(1) as f64).min(1.0);
            (((cw as f64 * scale) as usize & !1).max(16), ((ch as f64 * scale) as usize & !1).max(16))
        }
        None => (req.width, req.height),
    };
    let seg_frames = ((req.segment_seconds * req.fps).round() as u32).max(1);
    let mut enc = VideoEncoder::new(EncoderSettings { width: w, height: h, fps: req.fps, bitrate: req.bitrate, keyframe_interval: seg_frames, threads: 1 })?;
    let cs = enc.color;
    let mut mux = Muxer::new(true, false);
    let halt = &*ctx.halt;
    let mut start = Instant::now();
    let frame_ticks = CLOCK as f64 / req.fps;
    let mut n: u64 = 0;
    let mut seq: u64 = 0;
    let mut cur: Vec<u8> = Vec::new();
    let mut cur_start: Option<u64> = None;
    let mut cur_frames = 0u32;
    let mut last_preview = Instant::now() - Duration::from_secs(1);
    let mut preview: Option<Res> = None;
    let seg_ticks = (req.segment_seconds * CLOCK as f64) as u64;
    let max_segment = 64usize << 20;
    let mut paused: Option<Instant> = None;
    loop {
        if halt() {
            break;
        }
        if !ctx.on.load(Ordering::Relaxed) {
            paused.get_or_insert_with(Instant::now);
            if let Some(c) = cam.as_mut() {
                let _ = c.next_frame(Duration::from_millis(20)); // keep the camera drained
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
            continue;
        }
        if let Some(p) = paused.take() {
            start += p.elapsed(); // media time does not run while paused
        }
        let t0 = Instant::now();
        let (pic, pts) = match cam.as_mut() {
            None => {
                let due = start + Duration::from_secs_f64(n as f64 / req.fps);
                if !wait_until(halt, due) {
                    break;
                }
                // a slow machine skips frames rather than drifting behind real time
                let behind = Instant::now().saturating_duration_since(due);
                if behind > Duration::from_millis(500) {
                    let now_n = (start.elapsed().as_secs_f64() * req.fps) as u64;
                    ctx.stats.skipped.fetch_add(now_n.saturating_sub(n), Ordering::Relaxed);
                    n = now_n;
                }
                (test_pattern(w, h, n, req.fps, cs), PTS0 + (n as f64 * frame_ticks).round() as u64)
            }
            Some(c) => match c.next_frame(Duration::from_millis(200))? {
                Some(f) => {
                    let pic = if f.pic.width == w && f.pic.height == h { f.pic } else { camera::resize_i420(&f.pic, w, h) };
                    (pic, PTS0 + (start.elapsed().as_secs_f64() * CLOCK as f64) as u64)
                }
                None => continue,
            },
        };
        let capture = t0.elapsed();
        // cut a segment before the frame that would make it too long
        let cut = match (cam.is_some(), cur_start) {
            (false, Some(_)) => cur_frames >= seg_frames,
            (true, Some(s)) => pts.saturating_sub(s) + (frame_ticks / 2.0) as u64 >= seg_ticks,
            (_, None) => false,
        };
        if cut || cur.len() > max_segment {
            let s0 = cur_start.unwrap_or(pts);
            let seg = LiveSegment { sequence: seq, duration: (pts.saturating_sub(s0)) as f64 / CLOCK as f64, data: Arc::new(std::mem::take(&mut cur)) };
            ctx.stats.bytes.fetch_add(seg.data.len() as u64, Ordering::Relaxed);
            lock(&ctx.live).add(seg);
            ctx.stats.segments.fetch_add(1, Ordering::Relaxed);
            seq += 1;
            cur_start = None;
            cur_frames = 0;
            (ctx.out)(ctx.item(preview.as_ref(), vec![]));
        }
        let first = cur_start.is_none();
        if first {
            mux.write_tables(&mut cur);
            cur_start = Some(pts);
        }
        let t1 = Instant::now();
        let encoded = enc.encode(&pic, first)?;
        ctx.stats.encode_ns.fetch_add(t1.elapsed().as_nanos() as u64, Ordering::Relaxed);
        ctx.stats.capture_ns.fetch_add(capture.as_nanos() as u64, Ordering::Relaxed);
        if let Some((au, key)) = encoded {
            mux.write_video(&mut cur, pts, None, key, &au);
        }
        cur_frames += 1;
        ctx.stats.frames.fetch_add(1, Ordering::Relaxed);
        if req.preview && last_preview.elapsed() >= Duration::from_millis(200) {
            last_preview = Instant::now();
            preview = preview_of(&pic, cs);
            (ctx.out)(ctx.item(preview.as_ref(), vec![]));
        }
        n += 1;
    }
    Ok(())
}
