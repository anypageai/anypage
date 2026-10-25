//! Device stream threads on simulated devices, below the page layer: an
//! idle or switched-off stream sleeps (no busy loop), and a connect or open
//! gives way at once to a newer request.
//!
//! The loop counter (`StreamIo::wakeups`) makes "does not spin" exact: a
//! sleeping thread wakes about four times a second, a spinning one hundreds
//! of thousands of times.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anypage_runtime::Data;

use crate::ble::{self, BleBackend, BleModule};
use crate::mock::Mocks;
use crate::serial::{self, SerialModule};
use crate::util::{lock, StreamIo, Worker};

const HR: &str = "C0:FF:EE:00:00:01";

/// Most loop passes a sleeping stream may make in a second (it wakes every
/// 250 ms, plus a few for pushes).
const ASLEEP: u64 = 10;

struct Harness {
    requests: Sender<Data>,
    items: Arc<Mutex<Vec<Data>>>,
    on: Arc<AtomicBool>,
    wakeups: Arc<AtomicU64>,
    _worker: Worker,
}

impl Harness {
    fn start(name: &str, run: impl FnOnce(StreamIo, Arc<AtomicBool>) + Send + 'static) -> Self {
        let (tx, rx) = channel();
        let items = Arc::new(Mutex::new(Vec::new()));
        let it = items.clone();
        let io = StreamIo::new(
            move |d| {
                lock(&it).push(d);
                true
            },
            rx,
            Data::Nil,
        );
        let (on, wakeups) = (io.on.clone(), io.wakeups.clone());
        let worker = Worker::spawn(name, move |stop| run(io, stop)).expect("a stream thread");
        Harness { requests: tx, items, on, wakeups, _worker: worker }
    }

    fn send(&self, d: Data) {
        self.requests.send(d).expect("the stream takes requests");
    }

    /// The page's request cell goes away: no more requests can come.
    fn end_requests(&mut self) {
        self.requests = channel().0;
    }

    fn last(&self) -> Data {
        lock(&self.items).last().cloned().unwrap_or_default()
    }

    fn wait_for(&self, what: &str, ok: impl Fn(&Data) -> bool) -> Duration {
        let t0 = Instant::now();
        while !ok(&self.last()) {
            assert!(t0.elapsed() < Duration::from_secs(5), "timed out waiting for {what}; last {:?}", self.last());
            std::thread::sleep(Duration::from_millis(2));
        }
        eprintln!("{what} after {:?}", t0.elapsed());
        t0.elapsed()
    }

    /// Loop passes during `d`.
    fn wakeups_in(&self, d: Duration) -> u64 {
        let before = self.wakeups.load(Ordering::Relaxed);
        std::thread::sleep(d);
        let n = self.wakeups.load(Ordering::Relaxed) - before;
        eprintln!("{n} wakeups in {d:?}");
        n
    }

    fn switch(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
    }
}

fn state(d: &Data) -> &str {
    d.get("state").str_or("")
}

#[test]
fn switched_off_link_sleeps() {
    let m = Mocks::standard();
    let module = BleModule::new(Arc::new(m.ble.clone()));
    let h = Harness::start("test.link", move |io, stop| ble::link::run(module, io, stop));
    h.send(Data::map([("device", Data::str(HR))]));
    h.wait_for("connected", |d| state(d) == "connected");
    let n = h.wakeups_in(Duration::from_secs(1));
    assert!(n <= ASLEEP, "{n} wakeups in 1 s while connected and quiet");

    // switched off: the device is released and the thread sleeps
    h.switch(false);
    h.wait_for("idle", |d| state(d) == "idle");
    assert_eq!(m.ble.open_links(), 0);
    let n = h.wakeups_in(Duration::from_secs(1));
    assert!(n <= ASLEEP, "{n} wakeups in 1 s while switched off");
    h.switch(true);
    h.wait_for("connected again", |d| state(d) == "connected");
    assert_eq!(m.ble.open_links(), 1);

    // switched off while a reconnect is pending (its time comes and goes)
    h.send(Data::map([("device", Data::str("00:00:00:00:00:09")), ("timeout_ms", Data::Num(500.0))]));
    h.wait_for("reconnecting", |d| state(d) == "reconnecting");
    h.switch(false);
    std::thread::sleep(Duration::from_millis(1100));
    let n = h.wakeups_in(Duration::from_secs(1));
    assert!(n <= ASLEEP, "{n} wakeups in 1 s, switched off with a reconnect due");
    assert_eq!(m.ble.open_links(), 0);
}

#[test]
fn switched_off_port_sleeps() {
    let m = Mocks::standard();
    let module = SerialModule::new(Arc::new(m.serial.clone()));
    let mut h = Harness::start("test.port", move |io, stop| serial::port::run(module, io, stop));
    h.send(Data::map([("path", Data::str("MOCK0"))]));
    h.wait_for("open", |d| state(d) == "open");
    // open, reads pace the loop: one pass per 20 ms read
    let n = h.wakeups_in(Duration::from_secs(1));
    assert!(n <= 60, "{n} wakeups in 1 s while open");

    h.switch(false);
    h.wait_for("idle", |d| state(d) == "idle");
    assert!(!m.serial.is_open("MOCK0"), "switched off releases the port");
    let n = h.wakeups_in(Duration::from_secs(1));
    assert!(n <= ASLEEP, "{n} wakeups in 1 s while switched off");
    // the request cell goes away too: still asleep
    h.end_requests();
    let n = h.wakeups_in(Duration::from_secs(1));
    assert!(n <= ASLEEP, "{n} wakeups in 1 s, switched off without requests");
    h.switch(true);
    h.wait_for("open again", |d| state(d) == "open");
    assert!(m.serial.is_open("MOCK0"));
}

#[test]
fn switched_off_scan_sleeps_after_a_radio_failure() {
    let m = Mocks::standard();
    let backend: Arc<dyn BleBackend> = Arc::new(m.ble.clone());
    let h = Harness::start("test.scan", move |io, stop| ble::scan::run(backend, io, stop));
    h.send(Data::map([("on", Data::Bool(true)), ("interval_ms", Data::Num(30.0))]));
    h.wait_for("scanning", |d| d.get("count").num() == Some(2.0));
    m.ble.set_powered(false);
    h.wait_for("the radio failure", |d| d.get("scanning").as_bool() == Some(false) && d.get("error").as_str().is_some());
    h.switch(false);
    // past the 3 s retry pause
    std::thread::sleep(Duration::from_millis(3200));
    let n = h.wakeups_in(Duration::from_secs(1));
    assert!(n <= ASLEEP, "{n} wakeups in 1 s, switched off with a retry due");
    m.ble.set_powered(true);
    h.switch(true);
    h.wait_for("scanning again", |d| d.get("scanning").as_bool() == Some(true));
    // `{}` turns it off like nil
    h.send(Data::map([]));
    h.wait_for("stopped", |d| d.get("scanning").as_bool() == Some(false) && d.get("error").is_nil());
}

#[test]
fn a_new_request_ends_a_waiting_connect() {
    let m = Mocks::standard();
    let module = BleModule::new(Arc::new(m.ble.clone()));
    let h = Harness::start("test.link", move |io, stop| ble::link::run(module, io, stop));
    // the mock, like a real stack, keeps trying an unknown device until the timeout
    let far = Data::map([("device", Data::str("00:00:00:00:00:09")), ("timeout_ms", Data::Num(120_000.0))]);
    h.send(far.clone());
    h.wait_for("connecting", |d| state(d) == "connecting");
    std::thread::sleep(Duration::from_millis(50));
    h.send(Data::Nil);
    let t = h.wait_for("idle", |d| state(d) == "idle");
    assert!(t < Duration::from_millis(300), "nil disconnected in {t:?}");
    assert!(h.last().get("error").is_nil(), "giving way is not an error");

    h.send(far);
    h.wait_for("connecting", |d| state(d) == "connecting" && d.get("device").as_str() == Some("00:00:00:00:00:09"));
    std::thread::sleep(Duration::from_millis(50));
    h.send(Data::map([("device", Data::str(HR))]));
    let t = h.wait_for("the other device", |d| state(d) == "connected");
    assert!(t < Duration::from_millis(500), "the other device connected in {t:?}");
    assert_eq!(h.last().get("device").as_str(), Some(HR));

    // switched off while connecting: idle at once, connects when on again
    h.send(Data::map([("device", Data::str("00:00:00:00:00:09")), ("timeout_ms", Data::Num(120_000.0))]));
    h.wait_for("connecting", |d| state(d) == "connecting" && d.get("device").as_str() == Some("00:00:00:00:00:09"));
    h.switch(false);
    let t = h.wait_for("idle", |d| state(d) == "idle");
    assert!(t < Duration::from_millis(500), "switched off in {t:?}");
}

#[test]
fn a_new_request_ends_a_waiting_open() {
    let m = Mocks::standard();
    m.serial.hold_open("MOCK1", true);
    let module = SerialModule::new(Arc::new(m.serial.clone()));
    let h = Harness::start("test.port", move |io, stop| serial::port::run(module, io, stop));
    // like a USB permission dialog nobody answers (the open waits up to 60 s)
    h.send(Data::map([("path", Data::str("MOCK1"))]));
    h.wait_for("opening", |d| state(d) == "opening");
    std::thread::sleep(Duration::from_millis(50));
    h.send(Data::Nil);
    let t = h.wait_for("idle", |d| state(d) == "idle");
    assert!(t < Duration::from_millis(300), "closed in {t:?}");
    assert!(h.last().get("error").is_nil());

    h.send(Data::map([("path", Data::str("MOCK1"))]));
    h.wait_for("opening", |d| state(d) == "opening" && d.get("path").as_str() == Some("MOCK1"));
    std::thread::sleep(Duration::from_millis(50));
    h.send(Data::map([("path", Data::str("MOCK0"))]));
    let t = h.wait_for("the other port", |d| state(d) == "open" && d.get("path").as_str() == Some("MOCK0"));
    assert!(t < Duration::from_millis(500), "the other port opened in {t:?}");
    assert!(!m.serial.is_open("MOCK1"));

    // the same port asked for again while waiting: still waiting, then opens when allowed
    h.send(Data::map([("path", Data::str("MOCK1"))]));
    h.wait_for("opening", |d| state(d) == "opening" && d.get("path").as_str() == Some("MOCK1"));
    h.send(Data::map([("path", Data::str("MOCK1"))]));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(state(&h.last()), "opening");
    m.serial.hold_open("MOCK1", false);
    h.wait_for("open", |d| state(d) == "open" && d.get("path").as_str() == Some("MOCK1"));
}

#[test]
fn queued_writes_drain_between_reads() {
    let m = Mocks::standard();
    let module = SerialModule::new(Arc::new(m.serial.clone()));
    let send = module.clone();
    let h = Harness::start("test.port", move |io, stop| serial::port::run(module, io, stop));
    h.send(Data::map([("path", Data::str("MOCK0")), ("write_timeout_ms", Data::Num(400.0))]));
    h.wait_for("open", |d| state(d) == "open");
    // flow control holds: the write waits, reading goes on
    m.serial.set_stalled("MOCK0", true);
    send.send("MOCK0", b"held\n".to_vec()).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    m.serial.inject("MOCK0", b"incoming\n");
    h.wait_for("a line while the write waits", |d| d.get("lines").items().iter().any(|l| l.as_str() == Some("incoming")));
    assert_eq!(h.last().get("tx").num(), Some(0.0));
    // released in time: the data goes out
    m.serial.set_stalled("MOCK0", false);
    h.wait_for("the write", |d| d.get("tx").num() == Some(5.0));
    assert_eq!(m.serial.written("MOCK0"), b"held\n");
    // held longer than write_timeout_ms: dropped with an error, the port stays open
    m.serial.set_stalled("MOCK0", true);
    send.send("MOCK0", b"lost".to_vec()).unwrap();
    h.wait_for("the error", |d| d.get("error").as_str().is_some());
    let e = h.last().get("error").str_or("").to_string();
    assert!(e.contains("dropped 4 of 4 bytes") && e.contains("400 ms"), "{e}");
    assert_eq!(state(&h.last()), "open");
    m.serial.set_stalled("MOCK0", false);
    send.send("MOCK0", b"ok\n".to_vec()).unwrap();
    h.wait_for("the next write", |d| d.get("tx").num() == Some(8.0));
}
