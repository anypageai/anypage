//! Shared pieces: waiting with a deadline and cancellation, request parsing,
//! byte inputs, and the thread plumbing of device streams.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anypage_runtime::{Data, JobCtx, SourceCtx};

/// Poll granularity of blocking waits: how quickly a cancelled job or a
/// closing page is noticed.
pub(crate) const SLICE: Duration = Duration::from_millis(25);

/// How long a device stream's thread sleeps when nothing is due: the
/// longest it takes to notice the stream was switched back on.
pub(crate) const IDLE_WAIT: Duration = Duration::from_millis(250);

/// How long a blocking device operation may take, and how to tell it to stop
/// early (a cancelled job, a page that closed, a stream that was switched off).
pub struct Wait<'a> {
    deadline: Instant,
    timeout: Duration,
    cancelled: &'a dyn Fn() -> bool,
}

fn never_cancelled() -> bool {
    false
}

impl<'a> Wait<'a> {
    pub fn new(timeout: Duration, cancelled: &'a dyn Fn() -> bool) -> Self {
        Self { deadline: Instant::now() + timeout, timeout, cancelled }
    }

    /// A wait that is only bounded by its timeout.
    pub fn timeout_only(timeout: Duration) -> Wait<'static> {
        Wait { deadline: Instant::now() + timeout, timeout, cancelled: &never_cancelled }
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub fn cancelled(&self) -> bool {
        (self.cancelled)()
    }

    /// `Err` once cancelled or past the deadline.
    pub fn check(&self, what: &str) -> Result<(), String> {
        if self.cancelled() {
            Err(format!("{what}: cancelled"))
        } else if self.remaining().is_zero() {
            Err(self.timed_out(what))
        } else {
            Ok(())
        }
    }

    pub fn timed_out(&self, what: &str) -> String {
        format!("{what}: timed out after {} ms", self.timeout.as_millis())
    }

    /// One step of a longer operation: at most `timeout`, never past this
    /// wait's deadline, same cancellation.
    pub fn sub(&self, timeout: Duration) -> Wait<'a> {
        let timeout = timeout.min(self.remaining());
        Wait { deadline: Instant::now() + timeout, timeout, cancelled: self.cancelled }
    }

    /// Receive one value, giving up when cancelled or at the deadline.
    pub fn recv<T>(&self, rx: &Receiver<T>, what: &str) -> Result<T, String> {
        loop {
            self.check(what)?;
            match rx.recv_timeout(self.remaining().min(SLICE)) {
                Ok(v) => return Ok(v),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(format!("{what}: no answer (the connection went away)")),
            }
        }
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Lock `m`, waiting only as long as `wait` allows: a second transfer on a
/// busy endpoint or a write behind another write stays cancellable.
pub(crate) fn lock_within<'m, T>(m: &'m Mutex<T>, wait: &Wait, what: &str) -> Result<MutexGuard<'m, T>, String> {
    loop {
        match m.try_lock() {
            Ok(g) => return Ok(g),
            Err(TryLockError::Poisoned(p)) => return Ok(p.into_inner()),
            Err(TryLockError::WouldBlock) => {
                wait.check(what)?;
                std::thread::sleep(wait.remaining().min(Duration::from_millis(5)));
            }
        }
    }
}

/// Page text quoted in an error message, cut to 64 characters: errors go
/// to logs, panels and agents, and must not grow with the input.
pub(crate) fn clip(s: &str) -> String {
    const MAX: usize = 64;
    match s.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Longest device id, port path or address a request may carry (Windows
/// device instance ids, the longest, are around 200 characters).
pub(crate) const MAX_ID: usize = 512;

/// `s` as an id or path named by `key`, refused when it is too long to be one.
pub(crate) fn ident<'a>(s: &'a str, key: &str) -> Result<&'a str, String> {
    if s.len() > MAX_ID {
        return Err(format!("\"{key}\" is too long ({} bytes; device ids and paths are at most {MAX_ID})", s.len()));
    }
    Ok(s)
}

pub(crate) fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

// ---------------------------------------------------------------- request values

/// `opts` (given when the job or stream was declared) overlaid with `input`
/// (the cell value): a page can fix the device in opts and vary the rest.
pub(crate) fn merged(opts: &Data, input: &Data) -> Data {
    match (opts, input) {
        (Data::Map(o), Data::Map(i)) => {
            let mut m = (**o).clone();
            for (k, v) in i.iter() {
                m.insert(k.clone(), v.clone());
            }
            Data::Map(Arc::new(m))
        }
        (Data::Map(_), Data::Nil) => opts.clone(),
        _ => input.clone(),
    }
}

/// A job's request: its first input laid over its options. A job with an
/// input that is nil has nothing to do yet (nil result), whatever its
/// options say; a job without inputs runs on its options alone.
pub(crate) fn job_request(j: &JobCtx) -> Data {
    if !j.inputs.is_empty() && j.input(0).is_nil() {
        return Data::Nil;
    }
    merged(j.opts, j.input(0))
}

/// True for nil and `{}`: a job with nothing to do yet.
pub(crate) fn is_empty(d: &Data) -> bool {
    match d {
        Data::Nil => true,
        Data::Map(m) => m.is_empty(),
        _ => false,
    }
}

pub(crate) fn opt_str<'a>(d: &'a Data, key: &str) -> Option<&'a str> {
    d.get(key).as_str().filter(|s| !s.is_empty())
}

pub(crate) fn req_str<'a>(d: &'a Data, key: &str) -> Result<&'a str, String> {
    match d.get(key) {
        Data::Str(s) if !s.is_empty() => ident(s, key),
        Data::Nil => Err(format!("missing \"{key}\"")),
        other => Err(format!("\"{key}\" must be a non-empty string, got {}", type_name(other))),
    }
}

/// An integer field in `lo..=hi`; numbers are f64 in `Data`, so fractions
/// and out-of-range values are refused rather than truncated.
pub(crate) fn int(d: &Data, key: &str, lo: i64, hi: i64) -> Result<Option<i64>, String> {
    match d.get(key) {
        Data::Nil => Ok(None),
        Data::Num(n) if n.fract() == 0.0 && *n >= lo as f64 && *n <= hi as f64 => Ok(Some(*n as i64)),
        Data::Num(n) => Err(format!("\"{key}\" must be a whole number in {lo}..={hi}, got {n}")),
        other => Err(format!("\"{key}\" must be a number, got {}", type_name(other))),
    }
}

pub(crate) fn req_int(d: &Data, key: &str, lo: i64, hi: i64) -> Result<i64, String> {
    int(d, key, lo, hi)?.ok_or_else(|| format!("missing \"{key}\""))
}

/// A number clamped to `lo..=hi`; anything else gives `default`.
pub(crate) fn num_in(d: &Data, key: &str, default: f64, lo: f64, hi: f64) -> f64 {
    d.get(key).num().filter(|n| n.is_finite()).unwrap_or(default).clamp(lo, hi)
}

pub(crate) fn millis(d: &Data, key: &str, default_ms: u64, max_ms: u64) -> Duration {
    Duration::from_millis(num_in(d, key, default_ms as f64, 1.0, max_ms as f64) as u64)
}

pub(crate) fn flag(d: &Data, key: &str, default: bool) -> bool {
    d.get(key).as_bool().unwrap_or(default)
}

pub(crate) fn type_name(d: &Data) -> &'static str {
    match d {
        Data::Nil => "nil",
        Data::Bool(_) => "boolean",
        Data::Num(_) => "number",
        Data::Str(_) => "string",
        Data::Bytes(_) => "bytes",
        Data::List(_) => "list",
        Data::Map(_) => "table",
        Data::Res(_) => "resource",
    }
}

/// Bytes from a page value: a buffer, a string, a byte resource (file
/// contents), or a list of numbers 0..=255. At most `max` bytes.
pub(crate) fn bytes_of(d: &Data, what: &str, max: usize) -> Result<Vec<u8>, String> {
    let v = match d {
        Data::List(l) => {
            let mut v = Vec::with_capacity(l.len().min(max));
            for (i, x) in l.iter().enumerate() {
                match x.num() {
                    Some(n) if n.fract() == 0.0 && (0.0..=255.0).contains(&n) => v.push(n as u8),
                    _ => return Err(format!("{what}: item {} must be a byte (0..255)", i + 1)),
                }
                if v.len() > max {
                    break;
                }
            }
            v
        }
        Data::Nil => return Err(format!("missing {what}")),
        other => match other.as_bytes() {
            Some(b) => b.to_vec(),
            None => return Err(format!("{what} must be bytes, a string or a list of bytes, got {}", type_name(other))),
        },
    };
    if v.len() > max {
        return Err(format!("{what} is larger than the limit of {max} bytes"));
    }
    Ok(v)
}

/// `{ ok = true }` or `{ ok = false, reason }`: what `<module>.available()` returns.
pub(crate) fn availability(r: Result<(), String>) -> Data {
    match r {
        Ok(()) => Data::map([("ok", Data::Bool(true))]),
        Err(e) => Data::map([("ok", Data::Bool(false)), ("reason", Data::str(&e))]),
    }
}

pub(crate) fn opt_s(s: &Option<String>) -> Data {
    s.as_deref().map(Data::str).unwrap_or_default()
}

pub(crate) fn opt_n<T: Into<f64> + Copy>(v: Option<T>) -> Data {
    v.map(|x| Data::Num(x.into())).unwrap_or_default()
}

/// `Data::Map` from owned keys (UUID strings and the like).
pub(crate) fn map_owned(pairs: impl IntoIterator<Item = (String, Data)>) -> Data {
    Data::Map(Arc::new(pairs.into_iter().map(|(k, v)| (Arc::from(k.as_str()), v)).collect()))
}

/// Text of `bytes` when it is UTF-8 without control characters other than
/// whitespace: device names, firmware versions, AT responses.
pub(crate) fn printable_text(bytes: &[u8]) -> Option<&str> {
    let s = std::str::from_utf8(bytes).ok()?;
    s.chars().all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t')).then_some(s)
}

// ---------------------------------------------------------------- stream threads

/// What a device stream's thread works with: built from a [`SourceCtx`]
/// when a page declares the stream, or directly by tests.
///
/// A stream's request is the `request` cell's value laid over the stream's
/// options, so `page.stream("p", "serial.port", { baud = 9600, request =
/// cell })` only needs `{ path = ... }` in the cell. A stream declared
/// without a request cell uses its options as the request.
pub struct StreamIo {
    pub push: Box<dyn Fn(Data) -> bool + Send>,
    pub requests: Receiver<Data>,
    pub on: Arc<AtomicBool>,
    pub quit: Arc<AtomicBool>,
    pub opts: Data,
    /// The first request, taken before the thread starts.
    pub initial: Option<Data>,
    /// How many times the stream's thread went round its loop (tests check
    /// that an idle or switched-off stream sleeps instead of spinning).
    pub(crate) wakeups: Arc<AtomicU64>,
}

/// Option keys the runtime itself reads (`page.stream` options).
const STREAM_KEYS: [&str; 2] = ["cap", "latest"];

impl StreamIo {
    pub fn from_ctx(ctx: SourceCtx) -> Self {
        let (on, quit) = ctx.flags();
        let sink = ctx.sink.clone();
        let opts = match &ctx.opts {
            Data::Map(m) => Data::Map(Arc::new(m.iter().filter(|(k, _)| !STREAM_KEYS.contains(&&***k)).map(|(k, v)| (k.clone(), v.clone())).collect())),
            _ => Data::Nil,
        };
        // the runtime queues the request cell's initial value before the
        // source starts (no cell: see `RequestFeed`)
        let initial = ctx.requests.try_recv().ok();
        StreamIo { push: Box::new(move |d| sink.push(d)), requests: ctx.requests, on, quit, opts, initial, wakeups: Arc::default() }
    }

    /// For tests: a stream fed by `requests`, pushing to `push`.
    pub fn new(push: impl Fn(Data) -> bool + Send + 'static, requests: Receiver<Data>, opts: Data) -> Self {
        StreamIo {
            push: Box::new(push),
            requests,
            on: Arc::new(AtomicBool::new(true)),
            quit: Arc::new(AtomicBool::new(false)),
            opts,
            initial: None,
            wakeups: Arc::default(),
        }
    }

    pub(crate) fn on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// Count one pass of the stream thread's loop.
    pub(crate) fn woke(&self) {
        self.wakeups.fetch_add(1, Ordering::Relaxed);
    }

    /// A request value laid over the stream's options. nil, false and `{}`
    /// turn the stream's device off (as nil and `{}` leave a job idle);
    /// `true` asks for what the options say.
    pub(crate) fn request(&self, d: Data) -> Data {
        match d {
            Data::Nil | Data::Bool(false) => Data::Nil,
            Data::Map(m) if m.is_empty() => Data::Nil,
            Data::Bool(true) if !is_empty(&self.opts) => self.opts.clone(),
            Data::Map(_) => merged(&self.opts, &d),
            other => other,
        }
    }

    /// The receiver for page requests, leaving a dead one in its place
    /// (the inbox thread takes it over).
    pub(crate) fn take_requests(&mut self) -> Receiver<Data> {
        std::mem::replace(&mut self.requests, channel().1)
    }
}

/// One queue for everything a stream thread reacts to.
pub(crate) enum Msg<E> {
    Request(Data),
    Event(E),
}

/// A stream thread's queue.
pub(crate) struct Inbox<E> {
    /// For device events (the thread keeps one, so `rx` never disconnects).
    pub tx: Sender<Msg<E>>,
    pub rx: Receiver<Msg<E>>,
    /// Bumped before each page request is queued, so a connect or open
    /// the thread is blocked in can give way to a newer request at once.
    pub requests: Arc<AtomicU64>,
}

/// Forward page requests into a queue that device events also feed, so the
/// stream thread wakes for either at once. The first message is the
/// stream's first request (see [`StreamIo`]).
pub(crate) fn inbox<E: Send + 'static>(io: &mut StreamIo, stop: Arc<AtomicBool>, name: &str) -> Result<Inbox<E>, String> {
    let requests = io.take_requests();
    let (tx, rx) = channel();
    let mut feed = RequestFeed::new(io);
    if let Some(d) = feed.first() {
        let _ = tx.send(Msg::Request(d));
    }
    let fwd = tx.clone();
    let quit = io.quit.clone();
    let count = Arc::new(AtomicU64::new(0));
    let bump = count.clone();
    std::thread::Builder::new()
        .name(format!("{name}.requests"))
        .spawn(move || loop {
            if quit.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed) {
                break;
            }
            match feed.next(&requests, Duration::from_millis(100)) {
                Some(Some(d)) => {
                    bump.fetch_add(1, Ordering::Relaxed);
                    if fwd.send(Msg::Request(d)).is_err() {
                        break;
                    }
                }
                Some(None) => {}
                None => break,
            }
        })
        .map_err(|e| format!("cannot start a thread for {name}: {e}"))?;
    Ok(Inbox { tx, rx, requests: count })
}

/// Where a stream's requests come from: the request cell's values, or,
/// for a stream declared without one, its options once.
pub(crate) struct RequestFeed {
    initial: Option<Data>,
    got_any: bool,
    from_opts: bool,
}

impl RequestFeed {
    pub fn new(io: &mut StreamIo) -> Self {
        let initial = io.initial.take();
        RequestFeed { got_any: initial.is_some(), initial, from_opts: !is_empty(&io.opts) }
    }

    pub fn first(&mut self) -> Option<Data> {
        self.initial.take()
    }

    /// `Some(Some(request))`, `Some(None)` when nothing came in `wait`,
    /// `None` once no more requests can come.
    pub fn next(&mut self, requests: &Receiver<Data>, wait: Duration) -> Option<Option<Data>> {
        match requests.recv_timeout(wait) {
            Ok(d) => {
                self.got_any = true;
                Some(Some(d))
            }
            Err(RecvTimeoutError::Timeout) => Some(None),
            // the runtime drops its sender once the stream is set up when
            // the page gave no request cell: then the options are the request
            Err(RecvTimeoutError::Disconnected) if !self.got_any && self.from_opts => {
                self.got_any = true;
                Some(Some(Data::Bool(true)))
            }
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }
}

/// Keeps a stream's thread for as long as the page lives. Dropping it asks
/// the thread to stop and waits briefly so devices are released before the
/// page is gone (a stuck driver call is left behind rather than blocking).
pub struct Worker {
    stop: Arc<AtomicBool>,
    done: Option<Receiver<()>>,
}

impl Worker {
    pub fn spawn(name: &str, f: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Result<Worker, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = channel();
        let s = stop.clone();
        std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                f(s);
                let _ = done_tx.send(());
            })
            .map_err(|e| format!("cannot start a thread for {name}: {e}"))?;
        Ok(Worker { stop, done: Some(done_rx) })
    }

    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(rx) = self.done.take() {
            let _ = rx.recv_timeout(Duration::from_millis(1500));
        }
    }
}

pub(crate) fn alive(io: &StreamIo, stop: &AtomicBool) -> bool {
    !io.quit.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed)
}

/// Push at most every `interval`, and only when something changed.
pub(crate) struct Throttle {
    pub interval: Duration,
    last: Option<Instant>,
    pub dirty: bool,
}

impl Throttle {
    pub fn new(interval: Duration) -> Self {
        Self { interval, last: None, dirty: false }
    }

    /// Time until a pending push is due (`None` when nothing is pending).
    pub fn due_in(&self) -> Option<Duration> {
        if !self.dirty {
            return None;
        }
        Some(match self.last {
            Some(t) => (t + self.interval).saturating_duration_since(Instant::now()),
            None => Duration::ZERO,
        })
    }

    pub fn ready(&self) -> bool {
        self.due_in().is_some_and(|d| d.is_zero())
    }

    pub fn pushed(&mut self) {
        self.last = Some(Instant::now());
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_from_every_form() {
        assert_eq!(bytes_of(&Data::str("AT\r\n"), "data", 16).unwrap(), b"AT\r\n");
        assert_eq!(bytes_of(&Data::bytes(vec![1, 2]), "data", 16).unwrap(), vec![1, 2]);
        assert_eq!(bytes_of(&Data::nums([0.0, 255.0]), "data", 16).unwrap(), vec![0, 255]);
        assert!(bytes_of(&Data::nums([256.0]), "data", 16).unwrap_err().contains("item 1"));
        assert!(bytes_of(&Data::nums([1.5]), "data", 16).is_err());
        assert!(bytes_of(&Data::bytes(vec![0; 17]), "data", 16).unwrap_err().contains("limit of 16"));
        assert!(bytes_of(&Data::nums(vec![1.0; 40]), "data", 16).unwrap_err().contains("limit"));
        assert!(bytes_of(&Data::Nil, "data", 16).unwrap_err().contains("missing"));
        assert!(bytes_of(&Data::Bool(true), "data", 16).unwrap_err().contains("boolean"));
    }

    #[test]
    fn integers_are_checked_not_truncated() {
        let d = Data::map([("a", Data::Num(3.0)), ("b", Data::Num(2.5)), ("c", Data::Num(300.0)), ("s", Data::str("x"))]);
        assert_eq!(int(&d, "a", 0, 255).unwrap(), Some(3));
        assert!(int(&d, "b", 0, 255).is_err());
        assert!(int(&d, "c", 0, 255).unwrap_err().contains("0..=255"));
        assert!(int(&d, "s", 0, 255).unwrap_err().contains("string"));
        assert_eq!(int(&d, "missing", 0, 255).unwrap(), None);
        assert!(req_int(&d, "missing", 0, 1).unwrap_err().contains("missing"));
        assert_eq!(num_in(&d, "c", 1.0, 0.0, 100.0), 100.0);
    }

    #[test]
    fn opts_under_inputs() {
        let o = Data::map([("device", Data::str("a")), ("length", Data::Num(8.0))]);
        let i = Data::map([("length", Data::Num(64.0))]);
        let m = merged(&o, &i);
        assert_eq!(m.get("device").as_str(), Some("a"));
        assert_eq!(m.get("length").num(), Some(64.0));
        assert_eq!(merged(&o, &Data::Nil), o);
        assert!(is_empty(&merged(&Data::Nil, &Data::Nil)));
        assert!(is_empty(&Data::map([])));
    }

    #[test]
    fn waits_end_on_cancel_and_deadline() {
        let (_tx, rx) = channel::<u8>();
        let t0 = Instant::now();
        let e = Wait::timeout_only(Duration::from_millis(60)).recv(&rx, "x").unwrap_err();
        assert!(e.contains("timed out after 60 ms"), "{e}");
        assert!(t0.elapsed() >= Duration::from_millis(55));
        let stop = AtomicBool::new(true);
        let c = || stop.load(Ordering::Relaxed);
        let e = Wait::new(Duration::from_secs(10), &c).recv(&rx, "x").unwrap_err();
        assert!(e.contains("cancelled"), "{e}");
        let w = Wait::timeout_only(Duration::from_millis(50));
        assert!(w.sub(Duration::from_secs(5)).timeout() <= Duration::from_millis(50));
        drop(_tx);
        let e = Wait::timeout_only(Duration::from_secs(1)).recv(&rx, "x").unwrap_err();
        assert!(e.contains("went away"), "{e}");
    }

    #[test]
    fn throttle_coalesces() {
        let mut t = Throttle::new(Duration::from_millis(40));
        assert_eq!(t.due_in(), None);
        t.dirty = true;
        assert!(t.ready());
        t.pushed();
        t.dirty = true;
        assert!(!t.ready());
        assert!(t.due_in().unwrap() <= Duration::from_millis(40));
    }

    #[test]
    fn page_text_in_errors_is_bounded() {
        assert_eq!(clip("COM3"), "COM3");
        let c = clip(&"字".repeat(1000));
        assert_eq!(c.chars().count(), 65, "64 characters and an ellipsis, cut on a character boundary");
        assert!(c.ends_with('…'));
        assert_eq!(ident("COM3", "path"), Ok("COM3"));
        let e = ident(&"x".repeat(MAX_ID + 1), "path").unwrap_err();
        assert!(e.contains("\"path\" is too long (513 bytes"), "{e}");
        let d = Data::map([("device", Data::str(&"x".repeat(100_000)))]);
        assert!(req_str(&d, "device").unwrap_err().len() < 120);
    }

    #[test]
    fn locks_wait_only_as_long_as_allowed() {
        let m = Mutex::new(1);
        assert_eq!(*lock_within(&m, &Wait::timeout_only(Duration::from_millis(10)), "x").unwrap(), 1);
        let held = lock(&m);
        let t0 = Instant::now();
        let e = lock_within(&m, &Wait::timeout_only(Duration::from_millis(60)), "endpoint 0x81 is busy").unwrap_err();
        assert_eq!(e, "endpoint 0x81 is busy: timed out after 60 ms");
        assert!(t0.elapsed() >= Duration::from_millis(55));
        let stop = AtomicBool::new(true);
        let c = || stop.load(Ordering::Relaxed);
        let t0 = Instant::now();
        assert!(lock_within(&m, &Wait::new(Duration::from_secs(10), &c), "x").unwrap_err().contains("cancelled"));
        assert!(t0.elapsed() < Duration::from_millis(50));
        drop(held);
    }

    #[test]
    fn empty_requests_turn_streams_off() {
        let io = StreamIo::new(|_| true, channel().1, Data::map([("baud", Data::Num(9600.0))]));
        assert!(io.request(Data::Nil).is_nil());
        assert!(io.request(Data::Bool(false)).is_nil());
        assert!(io.request(Data::map([])).is_nil(), "{{}} is off");
        assert_eq!(io.request(Data::Bool(true)).get("baud").num(), Some(9600.0));
        let r = io.request(Data::map([("path", Data::str("COM3"))]));
        assert_eq!((r.get("path").as_str(), r.get("baud").num()), (Some("COM3"), Some(9600.0)));
    }

    #[test]
    fn printable() {
        assert_eq!(printable_text(b"OK\r\n"), Some("OK\r\n"));
        assert_eq!(printable_text(&[0x01, 0x02]), None);
        assert_eq!(printable_text(&[0xff]), None);
    }
}
