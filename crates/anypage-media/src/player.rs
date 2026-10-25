//! `media.hls`: HLS playback as a stream of frames.
//!
//! Three threads per session: a fetcher (playlists and segments, live reload,
//! catch-up when it falls behind), a decoder (demux, H.264 decode, AAC to the
//! audio output when the `audio` feature is on), and a presenter that hands
//! frames out by presentation time against a clock (the audio clock while
//! audio plays, else the wall clock), drops frames that are already late, and
//! re-anchors after stalls, discontinuities, and timestamp jumps of more than
//! [`MAX_AHEAD`] within one epoch (an encoder restart without
//! EXT-X-DISCONTINUITY would otherwise freeze the picture for the whole jump).
//!
//! Threads are signalled to stop and waited for at most [`STOP_WAIT`]; a
//! fetch inside an HTTP call is left to end at its own timeout. A panic in
//! any thread is reported as a fatal `{ error }` item.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anypage_engine::Image;
use anypage_runtime::{Data, Grants, Res};

use crate::audio::AudioPipe;
use crate::h264::{is_keyframe, DecodedFrame, VideoDecoder};
use crate::hls::{choose_variant, parse, Parsed};
use crate::http::{Fetcher, Peers};
use crate::panic_message;
use crate::ts::{Codec, Demuxer, Unwrap, CLOCK};

/// What a page asks `media.hls` to play.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayRequest {
    pub url: String,
    /// For live playlists: start this many segments before the live end.
    pub live_edge: usize,
    /// Pick the best variant at most this tall (0: the best there is).
    pub max_height: u64,
    /// Play the audio track when this build has audio output.
    pub audio: bool,
    /// Decoded pictures kept ready ahead of the clock.
    pub buffer_frames: usize,
    /// Start a finished (VOD) playlist again at the end.
    pub looping: bool,
    /// Treat 100.64.0.0/10 as the local network (an overlay network such as
    /// Tailscale; on mobile carriers that range is shared space).
    pub tailnet: bool,
}

impl PlayRequest {
    pub fn new(url: &str) -> Self {
        Self { url: url.to_string(), live_edge: 3, max_height: 0, audio: true, buffer_frames: 8, looping: false, tailnet: false }
    }

    /// `{ url, live_edge, max_height, audio, buffer, loop, tailnet }`, or `nil` / no url for "stop".
    pub fn from_data(d: &Data) -> Result<Option<Self>, String> {
        let url = match d {
            Data::Nil => return Ok(None),
            Data::Str(s) => s.to_string(),
            Data::Map(_) => match d.get("url") {
                Data::Nil => return Ok(None),
                Data::Str(s) => s.to_string(),
                _ => return Err("url must be a string".into()),
            },
            _ => return Err("request must be { url = ... } or nil".into()),
        };
        if url.len() > 4096 {
            return Err("url is longer than 4096 bytes".into());
        }
        let n = |k: &str, def: f64, lo: f64, hi: f64| d.get(k).num().filter(|x| x.is_finite()).unwrap_or(def).clamp(lo, hi);
        Ok(Some(Self {
            url,
            live_edge: n("live_edge", 3.0, 1.0, 20.0) as usize,
            max_height: n("max_height", 0.0, 0.0, 8640.0) as u64,
            audio: d.get("audio").as_bool().unwrap_or(true),
            buffer_frames: n("buffer", 8.0, 2.0, 60.0) as usize,
            looping: d.get("loop").truthy(),
            tailnet: d.get("tailnet").truthy(),
        }))
    }
}

/// Counters for one session (all monotonic).
#[derive(Default)]
pub struct Stats {
    pub segments: AtomicU64,
    pub decoded: AtomicU64,
    pub presented: AtomicU64,
    pub dropped: AtomicU64,
    pub stalls: AtomicU64,
    pub errors: AtomicU64,
    pub decode_ns: AtomicU64,
    pub convert_ns: AtomicU64,
    /// Microseconds of media in fetched segments not yet decoded.
    pub queued_us: AtomicU64,
    pub demux_errors: AtomicU64,
}

impl Stats {
    pub fn decode_ms(&self) -> f64 {
        let n = self.decoded.load(Ordering::Relaxed).max(1);
        self.decode_ns.load(Ordering::Relaxed) as f64 / n as f64 / 1e6
    }
    pub fn convert_ms(&self) -> f64 {
        let n = self.presented.load(Ordering::Relaxed).max(1);
        self.convert_ns.load(Ordering::Relaxed) as f64 / n as f64 / 1e6
    }
}

enum Seg {
    Data { duration: f64, discontinuity: bool, bytes: Vec<u8> },
    End,
    Error { message: String, fatal: bool },
}

struct Queued {
    epoch: u32,
    frame: DecodedFrame,
}

#[derive(Default)]
struct Queue {
    frames: VecDeque<Queued>,
    bytes: usize,
    ended: bool,
    errors: Vec<String>,
    fatal: bool,
}

type Shared = Arc<(Mutex<Queue>, Condvar)>;

fn lock(s: &Shared) -> MutexGuard<'_, Queue> {
    s.0.lock().unwrap_or_else(|p| p.into_inner())
}

/// Where items go (the source's sink in a page; a closure in tests).
pub type Out = Arc<dyn Fn(Data) -> bool + Send + Sync>;

/// One playback session.
pub struct Player {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub stats: Arc<Stats>,
}

/// Memory cap for decoded pictures waiting to be shown.
const QUEUE_BYTES: usize = 96 << 20;

/// How long [`Player::stop`] waits for its threads. The fetcher may be inside
/// an HTTP call that only returns at a timeout; it is left to finish alone
/// (it holds only shared counters and a channel nobody reads any more).
pub const STOP_WAIT: Duration = Duration::from_millis(250);

/// A presentation time this far ahead of the clock, in one epoch, is a jump
/// in the stream's timestamps (an encoder restart without
/// EXT-X-DISCONTINUITY), not a frame to wait for.
pub const MAX_AHEAD: u64 = 2 * CLOCK;

fn post_fatal(shared: &Shared, message: String) {
    let mut q = lock(shared);
    if q.errors.len() < 16 {
        q.errors.push(message);
    }
    q.fatal = true;
    shared.1.notify_all();
}

impl Player {
    /// Start playing. `on` pauses presentation while false; `quit` (the
    /// page's) and [`Player::stop`] end the session.
    pub fn start(req: PlayRequest, grants: Arc<Grants>, out: Out, on: Arc<AtomicBool>, quit: Arc<AtomicBool>) -> Result<Player, String> {
        let peers = Peers { tailnet: req.tailnet };
        crate::http::check_url_for(&grants, &req.url, peers)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::default());
        let shared: Shared = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let (tx, rx) = sync_channel::<Seg>(3);
        let audio = Arc::new(AudioPipe::new(req.audio));
        let halt = {
            let (s, q) = (stop.clone(), quit.clone());
            Arc::new(move || s.load(Ordering::Relaxed) || q.load(Ordering::Relaxed)) as Arc<dyn Fn() -> bool + Send + Sync>
        };
        // a panic in any of the three threads becomes a fatal `{ error }`
        // item and the end of the session, never a silent freeze
        let mut threads = Vec::new();
        {
            let (req, halt, stats, shared) = (req.clone(), halt.clone(), stats.clone(), shared.clone());
            threads.push(spawn("media-hls-fetch", move || {
                let mut f = Fetcher::new(grants);
                f.peers = peers;
                // held until the error is posted, so the decoder cannot see the
                // channel close (and end quietly) before it
                let keep = tx.clone();
                if let Err(e) = catch_unwind(AssertUnwindSafe(|| fetch_loop(req, f, tx, halt, stats))) {
                    post_fatal(&shared, format!("internal error while fetching: {}", panic_message(&*e)));
                }
                drop(keep);
            })?);
        }
        {
            let (shared, halt, stats, audio, cap) = (shared.clone(), halt.clone(), stats.clone(), audio.clone(), req.buffer_frames);
            threads.push(spawn("media-hls-decode", move || {
                if let Err(e) = catch_unwind(AssertUnwindSafe(|| decode_loop(rx, shared.clone(), halt, stats, audio, cap))) {
                    post_fatal(&shared, format!("internal error while decoding: {}", panic_message(&*e)));
                }
            })?);
        }
        {
            let (halt, stats, out2) = (halt.clone(), stats.clone(), out.clone());
            threads.push(spawn("media-hls-present", move || {
                if let Err(e) = catch_unwind(AssertUnwindSafe(|| present_loop(shared, halt, stats, audio, out, on, req.url))) {
                    out2(Data::map([("error", Data::str(&format!("internal error while presenting: {}", panic_message(&*e)))), ("ended", Data::Bool(true))]));
                }
            })?);
        }
        Ok(Player { stop, threads, stats })
    }

    /// Stop the session: signals every thread and waits at most
    /// [`STOP_WAIT`] for them (threads still blocked in a network call end on
    /// their own at its timeout).
    pub fn stop(&mut self) {
        if self.stop.swap(true, Ordering::Relaxed) {
            return; // already stopped: the threads were signalled and waited for once
        }
        self.threads = crate::join_within(std::mem::take(&mut self.threads), STOP_WAIT);
    }

    /// Have all its threads ended (after an error, the end of a VOD, or a stop)?
    pub fn finished(&self) -> bool {
        self.threads.iter().all(|t| t.is_finished())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>, String> {
    std::thread::Builder::new().name(name.into()).spawn(f).map_err(|e| format!("cannot start a media thread: {e}"))
}

fn sleep_unless(halt: &dyn Fn() -> bool, d: Duration) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if halt() {
            return false;
        }
        std::thread::sleep((end - Instant::now()).min(Duration::from_millis(20)));
    }
    !halt()
}

// ---------------------------------------------------------------- fetch

fn fetch_loop(req: PlayRequest, f: Fetcher, tx: SyncSender<Seg>, halt: Arc<dyn Fn() -> bool + Send + Sync>, stats: Arc<Stats>) {
    let halt = &*halt;
    let send = |s: Seg| tx.send(s).is_ok();
    let fail = |message: String, fatal: bool| {
        let _ = tx.send(Seg::Error { message, fatal });
    };
    // resolve master playlists down to a media playlist; a server that is
    // still starting (connection refused, 404) gets a few tries
    let mut url = req.url.clone();
    let mut next_seq: Option<u64> = None;
    let mut retries = 0u32;
    let mut discontinuity = false;
    let mut levels = 0;
    loop {
        if halt() {
            return;
        }
        match f.get(&url, None, halt) {
            Ok((final_url, body)) => match parse(&final_url, &body) {
                Ok(Parsed::Master(v)) => match choose_variant(&v, req.max_height) {
                    Some(var) if levels < 3 => {
                        url = var.url.clone();
                        levels += 1;
                    }
                    _ => return fail("no playable variant in the master playlist".into(), true),
                },
                Ok(Parsed::Media(_)) => {
                    url = final_url;
                    break;
                }
                Err(e) => return fail(e, true),
            },
            Err(e) => {
                if halt() {
                    return;
                }
                retries += 1;
                let fatal = retries > 8 || e.contains("capabilities.net") || e.contains("only http") || e.contains("plain http");
                if fatal {
                    return fail(e, true);
                }
                if retries >= 3 {
                    fail(format!("retrying: {e}"), false);
                }
                if !sleep_unless(halt, Duration::from_millis((250 * retries as u64).min(2000))) {
                    return;
                }
            }
        }
    }
    retries = 0;
    loop {
        if halt() {
            return;
        }
        let (final_url, body) = match f.get(&url, None, halt) {
            Ok(x) => x,
            Err(e) => {
                if halt() {
                    return;
                }
                retries += 1;
                fail(format!("playlist: {e}"), retries > 5);
                if retries > 5 || !sleep_unless(halt, Duration::from_millis(500 * retries as u64)) {
                    return;
                }
                continue;
            }
        };
        let media = match parse(&final_url, &body) {
            Ok(Parsed::Media(m)) => m,
            Ok(Parsed::Master(_)) => return fail("playlist turned into a master playlist".into(), true),
            Err(e) => return fail(e, true),
        };
        retries = 0;
        let last_seq = media.segments.last().map(|s| s.sequence);
        if next_seq.is_none() {
            // where to start: the beginning of a finished playlist, near the end of a live one
            let start = if media.ended { 0 } else { media.segments.len().saturating_sub(req.live_edge) };
            next_seq = media.segments.get(start).map(|s| s.sequence);
            if next_seq.is_none() && !media.ended {
                // a live playlist with no segments yet
                if !sleep_unless(halt, Duration::from_secs_f64(media.target_duration.clamp(0.1, 30.0) / 2.0)) {
                    return;
                }
                continue;
            }
        }
        // fell behind a live window, or the stream restarted with lower
        // sequence numbers: jump to the live edge
        if let (Some(n), Some(firstseq), Some(lastseq)) = (next_seq, media.segments.first().map(|s| s.sequence), last_seq) {
            if n < firstseq || n > lastseq.saturating_add(1) {
                let start = media.segments.len().saturating_sub(req.live_edge);
                next_seq = media.segments.get(start).map(|s| s.sequence);
                discontinuity = true;
                stats.stalls.fetch_add(1, Ordering::Relaxed);
            }
        }
        let mut got_new = false;
        for s in &media.segments {
            if next_seq.is_some_and(|n| s.sequence < n) {
                continue;
            }
            if halt() {
                return;
            }
            match f.get(&s.url, s.range, halt) {
                Ok((_, bytes)) => {
                    stats.segments.fetch_add(1, Ordering::Relaxed);
                    stats.queued_us.fetch_add((s.duration * 1e6) as u64, Ordering::Relaxed);
                    let seg = Seg::Data { duration: s.duration, discontinuity: s.discontinuity || discontinuity, bytes };
                    discontinuity = false;
                    if !send(seg) {
                        return;
                    }
                }
                Err(e) => {
                    if halt() {
                        return;
                    }
                    // skip a segment that cannot be fetched; the next one restarts decoding
                    fail(format!("segment {}: {e}", s.sequence), false);
                    discontinuity = true;
                }
            }
            next_seq = Some(s.sequence.saturating_add(1));
            got_new = true;
        }
        if media.ended {
            if req.looping && last_seq.is_some() {
                next_seq = media.segments.first().map(|s| s.sequence);
                discontinuity = true;
                continue;
            }
            let _ = send(Seg::End);
            return;
        }
        // live: reload after a target duration, sooner when nothing was new
        let target = media.target_duration.clamp(0.1, 30.0);
        let wait = if got_new { target } else { target / 2.0 };
        if !sleep_unless(halt, Duration::from_secs_f64(wait)) {
            return;
        }
    }
}

// ---------------------------------------------------------------- decode

fn decode_loop(rx: Receiver<Seg>, shared: Shared, halt: Arc<dyn Fn() -> bool + Send + Sync>, stats: Arc<Stats>, audio: Arc<AudioPipe>, cap: usize) {
    let halt = &*halt;
    let post_error = |message: String, fatal: bool| {
        let mut q = lock(&shared);
        if q.errors.len() < 16 {
            q.errors.push(message);
        }
        q.fatal |= fatal;
        shared.1.notify_all();
    };
    let mut dec = match VideoDecoder::new() {
        Ok(d) => d,
        Err(e) => return post_error(e, true),
    };
    let mut epoch = 0u32;
    let mut unwrap = Unwrap::default();
    let mut need_key = true;
    let mut last_pts = 0u64;
    let mut audio_unwrap = Unwrap::default();
    let enqueue = |epoch: u32, frame: DecodedFrame| -> bool {
        let mut q = lock(&shared);
        while (q.frames.len() >= cap || q.bytes + frame.bytes() > QUEUE_BYTES) && !q.frames.is_empty() {
            if halt() {
                return false;
            }
            q = shared.1.wait_timeout(q, Duration::from_millis(20)).map(|r| r.0).unwrap_or_else(|p| p.into_inner().0);
        }
        q.bytes += frame.bytes();
        q.frames.push_back(Queued { epoch, frame });
        shared.1.notify_all();
        true
    };
    loop {
        if halt() {
            return;
        }
        let seg = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(s) => s,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                // the fetcher ended without saying why: end the session
                // rather than wait forever (its own handler posts any panic)
                let mut q = lock(&shared);
                q.ended = true;
                shared.1.notify_all();
                return;
            }
        };
        match seg {
            Seg::Data { duration, discontinuity, bytes } => {
                stats.queued_us.fetch_sub(((duration * 1e6) as u64).min(stats.queued_us.load(Ordering::Relaxed)), Ordering::Relaxed);
                if discontinuity && stats.segments.load(Ordering::Relaxed) > 1 {
                    for f in dec.flush() {
                        if !enqueue(epoch, f) {
                            return;
                        }
                    }
                    epoch += 1;
                    unwrap = Unwrap::default();
                    audio_unwrap = Unwrap::default();
                    need_key = true;
                    audio.discontinuity(epoch);
                }
                let mut d = Demuxer::new();
                d.push(&bytes);
                d.flush();
                stats.demux_errors.fetch_add(d.errors, Ordering::Relaxed);
                let streams = d.streams().to_vec();
                if !streams.iter().any(|s| s.codec == Codec::H264) {
                    let what = if streams.iter().any(|s| s.codec == Codec::Hevc) {
                        "H.265/HEVC video is not supported (H.264 only)".to_string()
                    } else if streams.is_empty() {
                        "segment is not an MPEG-TS with a program map".to_string()
                    } else {
                        format!("no H.264 video in the stream ({})", streams.iter().map(|s| s.codec.name()).collect::<Vec<_>>().join(", "))
                    };
                    post_error(what, true);
                    return;
                }
                while let Some(p) = d.pop() {
                    if halt() {
                        return;
                    }
                    match p.codec {
                        Codec::H264 => {
                            let raw = p.pts.or(p.dts).unwrap_or(last_pts + 3000);
                            let pts = unwrap.unwrap(raw);
                            last_pts = raw;
                            if need_key {
                                if !is_keyframe(&p.data) {
                                    continue;
                                }
                                need_key = false;
                            }
                            let t0 = Instant::now();
                            let r = dec.decode(&p.data, pts);
                            stats.decode_ns.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                            match r {
                                Ok(frames) => {
                                    for f in frames {
                                        stats.decoded.fetch_add(1, Ordering::Relaxed);
                                        if !enqueue(epoch, f) {
                                            return;
                                        }
                                    }
                                }
                                Err(e) => {
                                    stats.errors.fetch_add(1, Ordering::Relaxed);
                                    if dec.errors % 50 == 1 {
                                        post_error(e, false);
                                    }
                                    need_key = true;
                                }
                            }
                        }
                        Codec::Aac => {
                            if let Some(raw) = p.pts {
                                audio.push(epoch, audio_unwrap.unwrap(raw), &p.data);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Seg::End => {
                for f in dec.flush() {
                    if !enqueue(epoch, f) {
                        return;
                    }
                }
                let mut q = lock(&shared);
                q.ended = true;
                shared.1.notify_all();
                return;
            }
            Seg::Error { message, fatal } => {
                post_error(message, fatal);
                if fatal {
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------- present

struct Anchor {
    at: Instant,
    pts: u64,
    epoch: u32,
}

impl Anchor {
    fn now(&self) -> u64 {
        self.pts.saturating_add((self.at.elapsed().as_secs_f64() * CLOCK as f64) as u64)
    }
}

fn present_loop(shared: Shared, halt: Arc<dyn Fn() -> bool + Send + Sync>, stats: Arc<Stats>, audio: Arc<AudioPipe>, out: Out, on: Arc<AtomicBool>, url: String) {
    let halt = &*halt;
    let mut anchor: Option<Anchor> = None;
    let mut paused_at: Option<Instant> = None;
    let mut last_frame: Option<Res> = None;
    let mut last_pts: Option<u64> = None;
    let mut last_error: Option<String> = None;
    let mut rebuffer = false;
    let mut frame_dur = 3000u64;
    let mut shown: VecDeque<Instant> = VecDeque::new();
    let mut size = (0usize, 0usize);
    let item = |frame: &Option<Res>, extra: Vec<(&str, Data)>, stats: &Stats, buffered: f64, fps: f64, size: (usize, usize), last_error: &Option<String>, audio: &AudioPipe| -> Data {
        let mut v: Vec<(&str, Data)> = vec![
            ("frame", frame.clone().map(Data::Res).unwrap_or_default()),
            ("width", Data::Num(size.0 as f64)),
            ("height", Data::Num(size.1 as f64)),
            ("fps", Data::Num((fps * 10.0).round() / 10.0)),
            ("buffered", Data::Num((buffered * 1000.0).round() / 1000.0)),
            ("dropped", Data::Num(stats.dropped.load(Ordering::Relaxed) as f64)),
            ("decoded", Data::Num(stats.decoded.load(Ordering::Relaxed) as f64)),
            ("presented", Data::Num(stats.presented.load(Ordering::Relaxed) as f64)),
            ("segments", Data::Num(stats.segments.load(Ordering::Relaxed) as f64)),
            ("stalls", Data::Num(stats.stalls.load(Ordering::Relaxed) as f64)),
            ("decode_ms", Data::Num((stats.decode_ms() * 100.0).round() / 100.0)),
            ("convert_ms", Data::Num((stats.convert_ms() * 100.0).round() / 100.0)),
            ("audio", Data::str(&audio.status())),
            ("url", Data::str(&url)),
            ("last_error", last_error.as_deref().map(Data::str).unwrap_or_default()),
        ];
        v.extend(extra);
        Data::map(v)
    };
    loop {
        if halt() {
            return;
        }
        if !on.load(Ordering::Relaxed) {
            if paused_at.is_none() {
                paused_at = Some(Instant::now());
                audio.set_paused(true);
            }
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if let Some(p) = paused_at.take() {
            if let Some(a) = anchor.as_mut() {
                a.at += p.elapsed();
            }
            audio.set_paused(false);
        }
        let mut q = lock(&shared);
        // errors first, so a page sees them even while frames still flow
        let errors = std::mem::take(&mut q.errors);
        if !errors.is_empty() {
            let fatal = q.fatal && q.frames.is_empty();
            drop(q);
            for e in errors {
                last_error = Some(e.clone());
                let buffered = 0.0;
                out(item(&last_frame, vec![("error", Data::str(&e))], &stats, buffered, 0.0, size, &last_error, &audio));
            }
            if fatal {
                return;
            }
            continue;
        }
        if q.frames.is_empty() {
            if q.ended || q.fatal {
                drop(q);
                out(item(&last_frame, vec![("ended", Data::Bool(true))], &stats, 0.0, 0.0, size, &last_error, &audio));
                return;
            }
            if let (Some(a), Some(lp)) = (&anchor, last_pts) {
                if a.now() > lp.saturating_add(4 * frame_dur) {
                    rebuffer = true; // ran dry: start the clock again when frames return
                }
            }
            let _ = shared.1.wait_timeout(q, Duration::from_millis(20));
            continue;
        }
        let (head_epoch, head_pts) = (q.frames[0].epoch, q.frames[0].frame.pts);
        let re_anchor = match &anchor {
            None => true,
            Some(a) => rebuffer || a.epoch != head_epoch,
        };
        if re_anchor {
            if rebuffer {
                stats.stalls.fetch_add(1, Ordering::Relaxed);
            }
            anchor = Some(Anchor { at: Instant::now(), pts: head_pts, epoch: head_epoch });
            rebuffer = false;
        }
        let Some(a) = anchor.as_mut() else { continue };
        // the audio clock leads while audio is actually playing and agrees
        // with the stream's timeline (after a timestamp jump it is ignored
        // until the sound catches up with the jump too)
        if let Some(apts) = audio.clock(a.epoch) {
            if apts.abs_diff(a.now()) < MAX_AHEAD {
                a.at = Instant::now();
                a.pts = apts;
            }
        }
        let now_pts = a.now();
        // late frames: drop every frame whose successor is already due
        while q.frames.len() >= 2 && q.frames[1].epoch == a.epoch && q.frames[1].frame.pts <= now_pts {
            if let Some(f) = q.frames.pop_front() {
                q.bytes = q.bytes.saturating_sub(f.frame.bytes());
                stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        let head = q.frames[0].frame.pts;
        if q.frames[0].epoch == a.epoch && head > now_pts {
            let ahead = head - now_pts;
            if ahead > MAX_AHEAD {
                // a forward jump in timestamps without a discontinuity tag:
                // waiting for the wall clock to get there would freeze the
                // picture for the whole jump, so start the clock again here
                stats.stalls.fetch_add(1, Ordering::Relaxed);
                *a = Anchor { at: Instant::now(), pts: head, epoch: a.epoch };
            } else {
                let wait = (ahead.saturating_mul(1000) / CLOCK).clamp(1, 20);
                let _ = shared.1.wait_timeout(q, Duration::from_millis(wait));
                continue;
            }
        }
        let Some(f) = q.frames.pop_front() else { continue };
        q.bytes = q.bytes.saturating_sub(f.frame.bytes());
        let span = match (q.frames.front(), q.frames.back()) {
            (Some(x), Some(y)) if x.epoch == y.epoch => y.frame.pts.saturating_sub(x.frame.pts) as f64 / CLOCK as f64,
            _ => 0.0,
        };
        shared.1.notify_all();
        drop(q);
        if now_pts > f.frame.pts.saturating_add(CLOCK / 2) {
            rebuffer = true; // half a second behind: re-anchor on the next frame
        }
        if let Some(lp) = last_pts {
            if f.frame.pts > lp && f.frame.pts - lp < CLOCK {
                frame_dur = (frame_dur * 7 + (f.frame.pts - lp)) / 8;
            }
        }
        last_pts = Some(f.frame.pts);
        let t0 = Instant::now();
        let rgba = match f.frame.to_rgba() {
            Ok(x) => x,
            Err(e) => {
                last_error = Some(e);
                continue;
            }
        };
        stats.convert_ns.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        size = (f.frame.width(), f.frame.height());
        let img = Image { w: size.0 as u32, h: size.1 as u32, rgba, newest: Some(f.frame.decoded_at) };
        let res = Res::new("media.image", img);
        last_frame = Some(res);
        stats.presented.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        shown.push_back(now);
        while shown.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1)) {
            shown.pop_front();
        }
        let fps = if shown.len() >= 2 { (shown.len() - 1) as f64 / now.duration_since(shown[0]).as_secs_f64().max(1e-3) } else { 0.0 };
        let buffered = span + stats.queued_us.load(Ordering::Relaxed) as f64 / 1e6;
        let pts_s = Data::Num(f.frame.pts as f64 / CLOCK as f64);
        let epoch = Data::Num(f.epoch as f64);
        out(item(&last_frame, vec![("pts", pts_s), ("epoch", epoch)], &stats, buffered, fps, size, &last_error, &audio));
    }
}
