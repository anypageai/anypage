//! `serial.port`: request `{ path, baud, data_bits, parity, stop_bits, flow,
//! dtr, rts, keep_lines, keep_bytes, reconnect, interval_ms,
//! write_timeout_ms }` (nil or `{}` closes) → items (use `latest = true`)
//! `{ state, path, lines, partial, bytes, rx, tx, error }`.
//!
//! `lines` are the last `keep_lines` complete lines (split at "\n", "\r\n"
//! or "\r", decoded as UTF-8 with replacement), `partial` the line being
//! received, `bytes` the last `keep_bytes` raw bytes, `rx`/`tx` byte totals.
//! `state` is "idle", "opening", "open", "closed" (will reopen) or "failed".
//!
//! Bytes queued with `serial.send` go out a slice at a time between reads,
//! so a port held up by flow control keeps receiving; data the device takes
//! nothing of for `write_timeout_ms` is dropped with an error.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anypage_runtime::Data;

use super::{settings_of, OpenPort, SerialConn, SerialModule, SerialSettings, READ_SLICE};
use crate::util::{alive, flag, ident, inbox, is_empty, lock, num_in, Inbox, Msg, StreamIo, Throttle, Wait, IDLE_WAIT};

/// Splits a byte stream into lines.
pub(crate) struct Lines {
    partial: Vec<u8>,
    /// The last byte was '\r': a following '\n' belongs to the same break.
    cr: bool,
    max_line: usize,
}

impl Lines {
    pub fn new(max_line: usize) -> Self {
        Self { partial: Vec::new(), cr: false, max_line }
    }

    pub fn feed(&mut self, data: &[u8], out: &mut impl FnMut(String)) {
        for &b in data {
            match b {
                b'\n' if self.cr => self.cr = false,
                b'\n' | b'\r' => {
                    self.cr = b == b'\r';
                    out(String::from_utf8_lossy(&self.partial).into_owned());
                    self.partial.clear();
                }
                _ => {
                    self.cr = false;
                    self.partial.push(b);
                    if self.partial.len() >= self.max_line {
                        out(String::from_utf8_lossy(&self.partial).into_owned());
                        self.partial.clear();
                    }
                }
            }
        }
    }

    pub fn partial(&self) -> String {
        String::from_utf8_lossy(&self.partial).into_owned()
    }

    pub fn clear(&mut self) {
        self.partial.clear();
        self.cr = false;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PortReq {
    pub path: String,
    pub settings: SerialSettings,
    pub keep_lines: usize,
    pub keep_bytes: usize,
    pub reconnect: bool,
    pub interval: Duration,
    /// How long queued data may wait for the device to take any of it.
    pub write_timeout: Duration,
}

pub(crate) fn parse(d: &Data) -> Result<Option<PortReq>, String> {
    if !d.truthy() || is_empty(d) {
        return Ok(None);
    }
    let path = d.get("path").as_str().filter(|s| !s.is_empty()).ok_or("a serial.port request needs \"path\" (from serial.ports())")?;
    Ok(Some(PortReq {
        path: ident(path, "path")?.to_string(),
        settings: settings_of(d)?,
        keep_lines: num_in(d, "keep_lines", 200.0, 1.0, 10_000.0) as usize,
        keep_bytes: num_in(d, "keep_bytes", 4096.0, 0.0, (1 << 20) as f64) as usize,
        reconnect: flag(d, "reconnect", true),
        interval: Duration::from_millis(num_in(d, "interval_ms", 30.0, 0.0, 10_000.0) as u64),
        write_timeout: Duration::from_millis(num_in(d, "write_timeout_ms", 5000.0, 50.0, 120_000.0) as u64),
    }))
}

/// `serial.send` data being written.
struct Outgoing {
    data: Vec<u8>,
    done: usize,
    /// When the device last took some of it.
    progress: Instant,
}

struct Open {
    conn: Arc<dyn SerialConn>,
    writes: Receiver<Vec<u8>>,
    queued: Arc<AtomicUsize>,
    out: Option<Outgoing>,
}

struct Port {
    m: Arc<SerialModule>,
    owner: u64,
    req: Option<PortReq>,
    open: Option<Open>,
    state: &'static str,
    error: Option<String>,
    next_try: Option<Instant>,
    backoff: Duration,
    lines: VecDeque<Data>,
    splitter: Lines,
    bytes: VecDeque<u8>,
    rx: u64,
    tx: Arc<AtomicU64>,
    throttle: Throttle,
}

const MAX_LINE: usize = 4096;

/// The most one loop pass spends writing queued data before it reads again.
const WRITE_SLICE: Duration = Duration::from_millis(50);

impl Port {
    fn close(&mut self) {
        if self.open.take().is_some() {
            if let Some(r) = &self.req {
                let mut ports = lock(&self.m.ports);
                if ports.get(&r.path).is_some_and(|p| p.owner == self.owner) {
                    ports.remove(&r.path);
                }
            }
        }
    }

    fn reset_data(&mut self) {
        self.lines.clear();
        self.splitter.clear();
        self.bytes.clear();
        self.rx = 0;
        self.tx = Arc::new(AtomicU64::new(0));
    }

    fn failed(&mut self, e: String) {
        self.close();
        self.error = Some(e);
        if self.req.as_ref().is_some_and(|r| r.reconnect) {
            self.state = "closed";
            self.next_try = Some(Instant::now() + self.backoff);
            self.backoff = (self.backoff * 2).min(Duration::from_secs(5));
        } else {
            self.state = "failed";
            self.next_try = None;
        }
        self.throttle.dirty = true;
    }

    fn received(&mut self, data: &[u8]) {
        let keep = self.req.as_ref().map(|r| (r.keep_lines, r.keep_bytes)).unwrap_or((200, 4096));
        self.rx += data.len() as u64;
        let lines = &mut self.lines;
        self.splitter.feed(data, &mut |l| {
            lines.push_back(Data::str(&l));
            while lines.len() > keep.0 {
                lines.pop_front();
            }
        });
        self.bytes.extend(data.iter().copied());
        while self.bytes.len() > keep.1 {
            self.bytes.pop_front();
        }
        self.throttle.dirty = true;
    }

    fn snapshot(&self) -> Data {
        Data::map([
            ("state", Data::str(self.state)),
            ("path", self.req.as_ref().map(|r| Data::str(&r.path)).unwrap_or_default()),
            ("lines", Data::list(self.lines.iter().cloned().collect())),
            ("partial", Data::str(&self.splitter.partial())),
            ("bytes", Data::bytes(self.bytes.iter().copied().collect())),
            ("rx", Data::Num(self.rx as f64)),
            ("tx", Data::Num(self.tx.load(Ordering::Relaxed) as f64)),
            ("error", self.error.as_deref().map(Data::str).unwrap_or_default()),
        ])
    }

    /// Open the requested port. Waiting (Android's permission dialog) ends
    /// early when `wait` is cancelled: then nothing is reported and the
    /// next request or the switch decides what happens.
    fn try_open(&mut self, wait: &Wait) {
        let Some(req) = self.req.clone() else { return };
        self.next_try = None;
        match self.m.backend.open(&req.path, &req.settings, wait) {
            Ok(conn) => {
                let (qtx, qrx) = channel();
                let queued = Arc::new(AtomicUsize::new(0));
                lock(&self.m.ports).insert(req.path.clone(), OpenPort { conn: conn.clone(), queue: qtx, queued: queued.clone(), tx: self.tx.clone(), owner: self.owner });
                self.open = Some(Open { conn, writes: qrx, queued, out: None });
                self.state = "open";
                self.error = None;
                self.backoff = Duration::from_millis(500);
            }
            Err(_) if wait.cancelled() => self.next_try = Some(Instant::now()),
            Err(e) => self.failed(e),
        }
        self.throttle.dirty = true;
    }

    /// Writes queued by `serial.send`: at most [`WRITE_SLICE`] per call, so
    /// reading goes on while a slow device drains a long write.
    fn flush_writes(&mut self, cancel: &dyn Fn() -> bool) {
        let stall = self.req.as_ref().map(|r| r.write_timeout).unwrap_or(Duration::from_secs(5));
        let Some(o) = &mut self.open else { return };
        let slice = Wait::new(WRITE_SLICE, cancel);
        let mut wrote = 0usize;
        let mut outcome: Result<(), String> = Ok(());
        let mut gave_up = None;
        loop {
            if o.out.is_none() {
                match o.writes.try_recv() {
                    Ok(data) => {
                        o.queued.fetch_sub(data.len(), Ordering::Relaxed);
                        o.out = Some(Outgoing { data, done: 0, progress: Instant::now() });
                    }
                    Err(_) => break,
                }
            }
            let Some(w) = &mut o.out else { break };
            if slice.cancelled() || slice.remaining().is_zero() {
                break;
            }
            match o.conn.write(&w.data[w.done..], &slice) {
                Ok(n) => {
                    w.done += n;
                    wrote += n;
                    if n > 0 {
                        w.progress = Instant::now();
                    }
                    if w.done >= w.data.len() {
                        o.out = None;
                        continue;
                    }
                    if w.progress.elapsed() >= stall {
                        gave_up = Some((w.done, w.data.len()));
                        o.out = None;
                    }
                    break;
                }
                Err(e) => {
                    outcome = Err(e);
                    break;
                }
            }
        }
        if wrote > 0 {
            self.tx.fetch_add(wrote as u64, Ordering::Relaxed);
            self.throttle.dirty = true;
        }
        if let Some((done, len)) = gave_up {
            self.error = Some(format!(
                "serial.send: dropped {} of {len} bytes: the device took nothing for {} ms (flow control holding, or it is not reading?)",
                len - done,
                stall.as_millis()
            ));
            self.throttle.dirty = true;
        }
        if let Err(e) = outcome {
            self.failed(e);
        }
    }

    fn request(&mut self, d: Data) {
        match parse(&d) {
            Ok(None) => {
                self.close();
                self.req = None;
                self.state = "idle";
                self.error = None;
                self.next_try = None;
            }
            Ok(Some(r)) => {
                self.throttle.interval = r.interval;
                let reopen = self.req.as_ref().is_none_or(|o| o.path != r.path || o.settings != r.settings);
                if reopen || self.open.is_none() {
                    self.close();
                    if self.req.as_ref().is_none_or(|o| o.path != r.path) {
                        self.reset_data();
                    }
                    self.state = "opening";
                    self.next_try = Some(Instant::now());
                    self.backoff = Duration::from_millis(500);
                }
                self.error = None;
                self.req = Some(r);
            }
            Err(e) => self.error = Some(e),
        }
        self.throttle.dirty = true;
    }
}

pub(crate) fn run(m: Arc<SerialModule>, mut io: StreamIo, stop: Arc<AtomicBool>) {
    // the thread keeps `_events` so the queue stays open after the page's
    // requests end
    let Inbox { tx: _events, rx, requests } = match inbox::<()>(&mut io, stop.clone(), "serial.port") {
        Ok(p) => p,
        Err(e) => {
            (io.push)(Data::map([("state", Data::str("failed")), ("error", Data::str(&e))]));
            return;
        }
    };
    let owner = m.next_owner.fetch_add(1, Ordering::Relaxed);
    let mut p = Port {
        m,
        owner,
        req: None,
        open: None,
        state: "idle",
        error: None,
        next_try: None,
        backoff: Duration::from_millis(500),
        lines: VecDeque::new(),
        splitter: Lines::new(MAX_LINE),
        bytes: VecDeque::new(),
        rx: 0,
        tx: Arc::new(AtomicU64::new(0)),
        throttle: Throttle::new(Duration::from_millis(30)),
    };
    p.throttle.dirty = true;

    while alive(&io, &stop) {
        io.woke();
        // never block on requests while the port is open: reads pace the loop
        let mut wait = if p.open.is_some() { Duration::ZERO } else { IDLE_WAIT };
        if let Some(d) = p.throttle.due_in() {
            wait = wait.min(d);
        }
        // an open that is due only counts while the stream is on: switched
        // off, the thread sleeps and looks again every IDLE_WAIT
        if io.on() && p.open.is_none() && p.req.is_some() {
            if let Some(t) = p.next_try {
                wait = wait.min(t.saturating_duration_since(Instant::now()));
            }
        }
        match rx.recv_timeout(wait) {
            Ok(Msg::Request(d)) => p.request(io.request(d)),
            Ok(Msg::Event(())) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if !io.on() && p.open.is_some() {
            p.close();
            p.state = "idle";
            p.next_try = Some(Instant::now());
            p.throttle.dirty = true;
        }
        if io.on() && p.open.is_none() && p.req.is_some() && p.next_try.is_some_and(|t| Instant::now() >= t) {
            if p.state != "closed" {
                p.state = "opening";
            }
            (io.push)(p.snapshot());
            p.throttle.pushed();
            // long enough for Android's USB permission dialog; a newer
            // request, the switch or the page closing ends it at once
            let seen = requests.load(Ordering::Relaxed);
            let cancel = || !alive(&io, &stop) || !io.on() || requests.load(Ordering::Relaxed) != seen;
            p.try_open(&Wait::new(Duration::from_secs(60), &cancel));
            if !io.on() && p.open.is_none() {
                p.state = "idle";
            }
        }

        if let Some(o) = &p.open {
            match o.conn.read(64 * 1024, READ_SLICE) {
                Ok(data) if !data.is_empty() => p.received(&data),
                Ok(_) => {}
                Err(e) => p.failed(e),
            }
            let cancel = || !alive(&io, &stop) || !io.on();
            p.flush_writes(&cancel);
        }
        if p.throttle.ready() {
            (io.push)(p.snapshot());
            p.throttle.pushed();
        }
    }
    p.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&[u8]]) -> (Vec<String>, String) {
        let mut l = Lines::new(8);
        let mut out = Vec::new();
        for c in chunks {
            l.feed(c, &mut |s| out.push(s));
        }
        (out, l.partial())
    }

    #[test]
    fn line_endings_and_chunks() {
        assert_eq!(split(&[b"a\r\nb\nc\rd"]), (vec!["a".into(), "b".into(), "c".into()], "d".into()));
        // "\r\n" split across reads is one break
        assert_eq!(split(&[b"ok\r", b"\nnext\n"]), (vec!["ok".into(), "next".into()], "".into()));
        // empty lines survive
        assert_eq!(split(&[b"\n\n"]), (vec!["".into(), "".into()], "".into()));
        // long lines are cut at the limit
        assert_eq!(split(&[b"0123456789"]), (vec!["01234567".into()], "89".into()));
        // a UTF-8 character split between reads decodes whole
        let s = "字\n".as_bytes();
        assert_eq!(split(&[&s[..1], &s[1..]]), (vec!["字".into()], "".into()));
        // invalid bytes are replaced, not dropped
        assert_eq!(split(&[&[0xff, b'\n']]).0, vec!["\u{fffd}".to_string()]);
    }

    #[test]
    fn requests() {
        assert_eq!(parse(&Data::Nil).unwrap(), None);
        assert_eq!(parse(&Data::map([])).unwrap(), None, "{{}} is off, like nil");
        assert!(parse(&Data::map([("baud", Data::Num(9600.0))])).unwrap_err().contains("path"));
        let r = parse(&Data::map([("path", Data::str("COM3")), ("baud", Data::Num(9600.0)), ("keep_lines", Data::Num(1e9))])).unwrap().unwrap();
        assert_eq!((r.path.as_str(), r.settings.baud, r.keep_lines, r.reconnect), ("COM3", 9600, 10_000, true));
        assert_eq!(r.write_timeout, Duration::from_secs(5));
        assert!(parse(&Data::map([("path", Data::str("COM3")), ("parity", Data::str("x"))])).is_err());
        let e = parse(&Data::map([("path", Data::str("COM3")), ("parity", Data::str(&"x".repeat(100_000)))])).unwrap_err();
        assert!(e.len() < 200, "page input is clipped in errors: {} bytes", e.len());
        assert!(parse(&Data::map([("path", Data::str(&"C".repeat(100_000)))])).unwrap_err().contains("too long"));
    }
}
