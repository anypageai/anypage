//! `ble.link`: request `{ device, subscribe = { {service, characteristic,
//! optional} }, keep, reconnect, timeout_ms, interval_ms }` (nil disconnects)
//! → items (use `latest = true`) `{ state, device, services, notifications =
//! { {service, characteristic, data, at, seq} }, latest = { [characteristic]
//! = data }, count, sent, error }`.
//!
//! `ble.send(device, characteristic, data)` from a handler queues a write
//! on the link; `sent` counts them, a failure shows in `error`.
//!
//! `state` is "idle", "connecting", "connected", "reconnecting",
//! "disconnected" (link lost, `reconnect = false`) or "failed". Changing only
//! `subscribe` keeps the link and subscribes or unsubscribes the
//! difference. While the stream is switched off the device is released.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anypage_runtime::Data;
use uuid::Uuid;

use super::{device_id, parse_uuid, props, resolve, services_data, uuid_string, write_checked, BleConnection, BleModule, Emit, LinkEvent, OpenLink};
use crate::util::{alive, flag, ident, inbox, is_empty, lock, map_owned, num_in, unix_now, Inbox, Msg, StreamIo, Throttle, Wait, IDLE_WAIT};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LinkReq {
    pub device: String,
    pub subscribe: Vec<Sub>,
    pub keep: usize,
    pub reconnect: bool,
    pub timeout: Duration,
    pub interval: Duration,
}

/// One `subscribe` entry. An optional one is skipped quietly on a device
/// that does not have it (apps that support several device models).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Sub {
    pub service: Option<Uuid>,
    pub characteristic: Uuid,
    pub optional: bool,
}

fn sub_entry(d: &Data) -> Result<Sub, String> {
    let pair = |s: Option<&str>, c: Option<&str>| -> Result<Sub, String> {
        let c = c.ok_or("each subscribe entry needs a characteristic")?;
        Ok(Sub { service: s.map(parse_uuid).transpose()?, characteristic: parse_uuid(c)?, optional: d.get("optional").truthy() })
    };
    match d {
        Data::Str(c) => pair(None, Some(c)),
        Data::Map(_) => pair(d.get("service").as_str(), d.get("characteristic").as_str()),
        Data::List(l) if l.len() == 2 => pair(l[0].as_str(), l[1].as_str()),
        _ => Err("subscribe entries are { service = ..., characteristic = ..., optional = bool }".into()),
    }
}

pub(crate) fn parse(d: &Data) -> Result<Option<LinkReq>, String> {
    if !d.truthy() || is_empty(d) {
        return Ok(None);
    }
    let device = d.get("device").as_str().filter(|s| !s.is_empty()).ok_or("a link request needs \"device\" (an id from ble.scan)")?;
    let device = ident(device, "device")?;
    let mut subscribe = Vec::new();
    for s in d.get("subscribe").items() {
        subscribe.push(sub_entry(s)?);
    }
    if subscribe.len() > 64 {
        return Err("at most 64 subscriptions per link".into());
    }
    Ok(Some(LinkReq {
        device: device_id(device),
        subscribe,
        keep: num_in(d, "keep", 64.0, 1.0, 1024.0) as usize,
        reconnect: flag(d, "reconnect", true),
        timeout: Duration::from_millis(num_in(d, "timeout_ms", super::CONNECT_TIMEOUT.as_millis() as f64, 500.0, 120_000.0) as u64),
        interval: Duration::from_millis(num_in(d, "interval_ms", 30.0, 0.0, 10_000.0) as u64),
    }))
}

/// What a link stream's thread receives besides page requests.
pub(crate) enum LinkIn {
    /// From the backend, tagged with the connection it belongs to.
    Event(u64, LinkEvent),
    /// From `ble.send`.
    Write(QueuedWrite),
}

pub(crate) struct QueuedWrite {
    pub service: Option<Uuid>,
    pub characteristic: Uuid,
    pub data: Vec<u8>,
    pub response: Option<bool>,
}

struct Open {
    gen: u64,
    conn: Arc<dyn BleConnection>,
    services: Data,
    /// Resolved (service, characteristic) pairs currently subscribed.
    subscribed: Vec<(Uuid, Uuid)>,
}

struct Link {
    m: Arc<BleModule>,
    req: Option<LinkReq>,
    open: Option<Open>,
    gen: u64,
    state: &'static str,
    error: Option<String>,
    next_try: Option<Instant>,
    backoff: Duration,
    notes: VecDeque<Data>,
    latest: BTreeMap<String, Data>,
    count: u64,
    /// Writes done for `ble.send`.
    sent: u64,
    pending: Arc<AtomicUsize>,
    throttle: Throttle,
}

const FIRST_BACKOFF: Duration = Duration::from_secs(1);

impl Link {
    fn close(&mut self) {
        if let Some(o) = self.open.take() {
            let mut links = lock(&self.m.links);
            if let Some(device) = &self.req.as_ref().map(|r| r.device.clone()) {
                if links.get(device).is_some_and(|l| l.conn.as_ptr() as *const () == Arc::as_ptr(&o.conn) as *const ()) {
                    links.remove(device);
                }
            }
            drop(links);
            o.conn.disconnect();
        }
    }

    /// A write queued by `ble.send`.
    fn queued_write(&mut self, w: QueuedWrite, wait: &Wait) {
        self.pending.fetch_sub(1, Ordering::Relaxed);
        let Some(o) = &self.open else {
            self.error = Some("ble.send: the link is not connected".into());
            self.throttle.dirty = true;
            return;
        };
        match write_checked(&*o.conn, w.service, w.characteristic, &w.data, w.response, wait) {
            Ok(()) => self.sent += 1,
            Err(e) => self.error = Some(format!("ble.send: {e}")),
        }
        self.throttle.dirty = true;
    }

    /// Bring subscriptions in line with the request; errors are reported,
    /// the link stays up.
    fn sync_subscriptions(&mut self, wait: &Wait) {
        let (Some(req), Some(open)) = (&self.req, &mut self.open) else { return };
        let services = open.conn.services();
        let mut want = Vec::new();
        let mut errors = Vec::new();
        for sub in &req.subscribe {
            match resolve(&services, sub.service, sub.characteristic) {
                Ok((s, p)) if p & (props::NOTIFY | props::INDICATE) != 0 => want.push((s, sub.characteristic)),
                Ok(_) => errors.push(format!("characteristic {} does not notify", uuid_string(&sub.characteristic))),
                Err(_) if sub.optional => {}
                Err(e) => errors.push(e),
            }
        }
        for pair in open.subscribed.clone() {
            if !want.contains(&pair) {
                if let Err(e) = open.conn.unsubscribe(pair.0, pair.1, &wait.sub(Duration::from_secs(5))) {
                    errors.push(e);
                }
                open.subscribed.retain(|p| *p != pair);
            }
        }
        for pair in want {
            if !open.subscribed.contains(&pair) {
                match open.conn.subscribe(pair.0, pair.1, &wait.sub(Duration::from_secs(10))) {
                    Ok(()) => open.subscribed.push(pair),
                    Err(e) => errors.push(e),
                }
            }
        }
        // cancelled by a newer request: that request syncs again, so the
        // "cancelled" errors are not news
        if !errors.is_empty() && !wait.cancelled() {
            self.error = Some(errors.join("; "));
        }
    }

    fn snapshot(&self) -> Data {
        let services = self.open.as_ref().map(|o| o.services.clone()).unwrap_or_else(|| Data::list(Vec::new()));
        Data::map([
            ("state", Data::str(self.state)),
            ("device", self.req.as_ref().map(|r| Data::str(&r.device)).unwrap_or_default()),
            ("services", services),
            ("notifications", Data::list(self.notes.iter().cloned().collect())),
            ("latest", map_owned(self.latest.iter().map(|(k, v)| (k.clone(), v.clone())))),
            ("count", Data::Num(self.count as f64)),
            ("sent", Data::Num(self.sent as f64)),
            ("error", self.error.as_deref().map(Data::str).unwrap_or_default()),
        ])
    }

    fn note(&mut self, service: Uuid, characteristic: Uuid, data: Vec<u8>) {
        let keep = self.req.as_ref().map(|r| r.keep).unwrap_or(64);
        self.count += 1;
        let bytes = Data::bytes(data);
        let c = uuid_string(&characteristic);
        self.latest.insert(c.clone(), bytes.clone());
        self.notes.push_back(Data::map([
            ("service", Data::str(&uuid_string(&service))),
            ("characteristic", Data::str(&c)),
            ("data", bytes),
            ("at", Data::Num(unix_now())),
            ("seq", Data::Num(self.count as f64)),
        ]));
        while self.notes.len() > keep {
            self.notes.pop_front();
        }
        self.throttle.dirty = true;
    }

    fn lost(&mut self, reason: String) {
        self.close();
        self.error = Some(reason);
        if self.req.as_ref().is_some_and(|r| r.reconnect) {
            self.state = "reconnecting";
            self.next_try = Some(Instant::now() + self.backoff);
            self.backoff = (self.backoff * 2).min(Duration::from_secs(30));
        } else {
            self.state = "disconnected";
            self.next_try = None;
        }
        self.throttle.dirty = true;
    }
}

pub(crate) fn run(m: Arc<BleModule>, mut io: StreamIo, stop: Arc<AtomicBool>) {
    let Inbox { tx, rx, requests } = match inbox::<LinkIn>(&mut io, stop.clone(), "ble.link") {
        Ok(p) => p,
        Err(e) => {
            (io.push)(Data::map([("state", Data::str("failed")), ("error", Data::str(&e))]));
            return;
        }
    };
    // a blocking connect or subscribe gives way to the page: closing, the
    // stream switched off, or any newer request (another device, nil)
    let newer = |seen: u64| requests.load(Ordering::Relaxed) != seen;
    let mut l = Link {
        m,
        req: None,
        open: None,
        gen: 0,
        state: "idle",
        error: None,
        next_try: None,
        backoff: FIRST_BACKOFF,
        notes: VecDeque::new(),
        latest: BTreeMap::new(),
        count: 0,
        sent: 0,
        pending: Arc::new(AtomicUsize::new(0)),
        throttle: Throttle::new(Duration::from_millis(30)),
    };
    l.throttle.dirty = true;

    while alive(&io, &stop) {
        io.woke();
        let mut wait = IDLE_WAIT;
        if let Some(d) = l.throttle.due_in() {
            wait = wait.min(d);
        }
        // a (re)connect that is due only counts while the stream is on:
        // switched off, the thread sleeps and looks again every IDLE_WAIT
        if io.on() && l.open.is_none() && l.req.is_some() {
            if let Some(t) = l.next_try {
                wait = wait.min(t.saturating_duration_since(Instant::now()));
            }
        }
        match rx.recv_timeout(wait) {
            Ok(Msg::Request(d)) => match parse(&io.request(d)) {
                Ok(None) => {
                    l.close();
                    l.req = None;
                    l.state = "idle";
                    l.error = None;
                    l.next_try = None;
                    l.throttle.dirty = true;
                }
                Ok(Some(r)) => {
                    let same_device = l.req.as_ref().is_some_and(|old| old.device == r.device);
                    l.throttle.interval = r.interval;
                    if same_device {
                        l.req = Some(r);
                        l.error = None;
                        if l.open.is_some() {
                            let seen = requests.load(Ordering::Relaxed);
                            let cancel = || !alive(&io, &stop) || newer(seen);
                            l.sync_subscriptions(&Wait::new(Duration::from_secs(30), &cancel));
                        } else {
                            // asked again while not connected: try now (or
                            // when switched on)
                            l.backoff = FIRST_BACKOFF;
                            l.next_try = Some(Instant::now());
                            if io.on() && l.state != "reconnecting" {
                                l.state = "connecting";
                            }
                        }
                    } else {
                        l.close();
                        l.req = Some(r);
                        l.notes.clear();
                        l.latest.clear();
                        l.count = 0;
                        l.error = None;
                        l.backoff = FIRST_BACKOFF;
                        l.next_try = Some(Instant::now());
                        l.state = "connecting";
                    }
                    l.throttle.dirty = true;
                }
                Err(e) => {
                    l.error = Some(e);
                    l.throttle.dirty = true;
                }
            },
            Ok(Msg::Event(LinkIn::Event(g, ev))) => {
                if l.open.as_ref().is_some_and(|o| o.gen == g) {
                    match ev {
                        LinkEvent::Notification { service, characteristic, data } => l.note(service, characteristic, data),
                        LinkEvent::Disconnected(reason) => l.lost(reason),
                    }
                }
            }
            Ok(Msg::Event(LinkIn::Write(w))) => {
                let cancel = || !alive(&io, &stop);
                l.queued_write(w, &Wait::new(Duration::from_secs(10), &cancel));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // switched off: release the device, reconnect when switched on
        if !io.on() && l.open.is_some() {
            l.close();
            l.state = "idle";
            l.next_try = Some(Instant::now());
            l.throttle.dirty = true;
        }
        if l.open.as_ref().is_some_and(|o| !o.conn.connected()) {
            l.lost("the link was lost".into());
        }

        let due = l.next_try.is_some_and(|t| Instant::now() >= t);
        if io.on() && due && l.open.is_none() {
            if let Some(req) = l.req.clone() {
                l.next_try = None;
                if l.state != "reconnecting" {
                    l.state = "connecting";
                }
                (io.push)(l.snapshot());
                l.throttle.pushed();
                l.gen += 1;
                let gen = l.gen;
                let txe = tx.clone();
                let emit: Emit<LinkEvent> = Arc::new(move |e| {
                    let _ = txe.send(Msg::Event(LinkIn::Event(gen, e)));
                });
                let seen = requests.load(Ordering::Relaxed);
                let cancel = || !alive(&io, &stop) || !io.on() || newer(seen);
                let wait = Wait::new(req.timeout, &cancel);
                match l.m.backend.connect(&req.device, emit, &wait) {
                    Ok(conn) => {
                        let weak: Weak<dyn BleConnection> = Arc::downgrade(&conn);
                        lock(&l.m.links).insert(req.device.clone(), OpenLink { conn: weak, outbox: tx.clone(), pending: l.pending.clone() });
                        let services = services_data(&conn.services());
                        l.open = Some(Open { gen, conn, services, subscribed: Vec::new() });
                        l.state = "connected";
                        l.error = None;
                        l.backoff = FIRST_BACKOFF;
                        l.sync_subscriptions(&Wait::new(Duration::from_secs(30), &cancel));
                    }
                    Err(_) if alive(&io, &stop) && (!io.on() || newer(seen)) => {
                        // gave way: no error to show. Switched off, it
                        // connects when switched on; a newer request decides
                        // what comes next (the same device tries again at once)
                        l.next_try = Some(Instant::now());
                        if !io.on() {
                            l.state = "idle";
                        }
                    }
                    Err(e) => {
                        l.error = Some(e);
                        if req.reconnect && alive(&io, &stop) {
                            l.state = "reconnecting";
                            l.next_try = Some(Instant::now() + l.backoff);
                            l.backoff = (l.backoff * 2).min(Duration::from_secs(30));
                        } else {
                            l.state = "failed";
                        }
                    }
                }
                l.throttle.dirty = true;
            }
        }
        if l.throttle.ready() {
            (io.push)(l.snapshot());
            l.throttle.pushed();
        }
    }
    l.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests() {
        assert_eq!(parse(&Data::Nil).unwrap(), None);
        assert_eq!(parse(&Data::map([])).unwrap(), None, "{{}} is off, like nil");
        assert!(parse(&Data::map([("keep", Data::Num(3.0))])).unwrap_err().contains("device"));
        let long = "A".repeat(100_000);
        let e = parse(&Data::map([("device", Data::str(&long))])).unwrap_err();
        assert!(e.contains("too long") && e.len() < 200, "{e}");
        let r = parse(&Data::map([
            ("device", Data::str("AA:BB")),
            (
                "subscribe",
                Data::list(vec![
                    Data::map([("service", Data::str("180d")), ("characteristic", Data::str("2a37"))]),
                    Data::list(vec![Data::str("180f"), Data::str("2a19")]),
                    Data::str("2a38"),
                ]),
            ),
            ("keep", Data::Num(5000.0)),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(r.subscribe.len(), 3);
        assert_eq!(r.subscribe[2].service, None);
        assert!(!r.subscribe[0].optional);
        assert_eq!(r.keep, 1024);
        assert!(r.reconnect);
        assert_eq!(r.device, "AA:BB");
        let o = parse(&Data::map([("device", Data::str("aa:bb")), ("subscribe", Data::list(vec![Data::map([("characteristic", Data::str("2a37")), ("optional", Data::Bool(true))])]))])).unwrap().unwrap();
        assert!(o.subscribe[0].optional);
        assert_eq!(o.device, "AA:BB", "addresses compare without case");
        assert!(parse(&Data::map([("device", Data::str("x")), ("subscribe", Data::list(vec![Data::Num(1.0)]))])).is_err());
        assert!(parse(&Data::map([("device", Data::str("x")), ("subscribe", Data::list(vec![Data::map([("service", Data::str("180d"))])]))]))
            .unwrap_err()
            .contains("characteristic"));
    }
}
