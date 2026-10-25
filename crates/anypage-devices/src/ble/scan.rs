//! `ble.scan`: request `{ on, services, name_prefix, min_rssi, interval_ms,
//! forget_s, max }` → items (use `latest = true`)
//! `{ scanning, devices = { {id, name, rssi, services, manufacturer,
//! service_data, tx_power, connectable, last_seen} }, count, error }`.
//!
//! Devices keep the order they were first seen in (rows do not jump while
//! a user aims at one); a device not heard from for `forget_s` is dropped.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anypage_runtime::Data;
use uuid::Uuid;

use super::{parse_uuid, uuid_string, BleAdvert, BleBackend, Emit, ScanEvent};
use crate::util::{alive, inbox, is_empty, num_in, opt_n, opt_s, unix_now, Inbox, Msg, StreamIo, Throttle, IDLE_WAIT};

const RETRY: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ScanReq {
    pub services: Vec<Uuid>,
    pub name_prefix: Option<String>,
    pub min_rssi: Option<i16>,
    pub interval: Duration,
    pub forget: Duration,
    pub max: usize,
}

/// `None` when the request turns scanning off (nil, false, `{}`,
/// `{ on = false }`); `true` or `{ on = true }` scans for every device.
pub(crate) fn parse(d: &Data) -> Result<Option<ScanReq>, String> {
    if !d.truthy() || is_empty(d) {
        return Ok(None);
    }
    let mut services = Vec::new();
    for s in d.get("services").items() {
        let s = s.as_str().ok_or("services must be a list of UUID strings")?;
        services.push(parse_uuid(s)?);
    }
    Ok(Some(ScanReq {
        services,
        name_prefix: d.get("name_prefix").as_str().filter(|s| !s.is_empty()).map(str::to_lowercase),
        min_rssi: d.get("min_rssi").num().map(|n| n.clamp(-127.0, 20.0) as i16),
        interval: Duration::from_millis(num_in(d, "interval_ms", 250.0, 30.0, 10_000.0) as u64),
        forget: Duration::from_secs_f64(num_in(d, "forget_s", 60.0, 1.0, 3600.0)),
        max: num_in(d, "max", 256.0, 1.0, 2048.0) as usize,
    }))
}

struct Entry {
    adv: BleAdvert,
    seen: Instant,
    seen_unix: f64,
}

/// What the stream has seen, merged per device.
#[derive(Default)]
pub(crate) struct Table {
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
}

fn merge(into: &mut BleAdvert, a: BleAdvert) {
    if a.name.is_some() {
        into.name = a.name;
    }
    if a.rssi.is_some() {
        into.rssi = a.rssi;
    }
    for s in a.services {
        if !into.services.contains(&s) {
            into.services.push(s);
        }
    }
    for (k, v) in a.manufacturer_data {
        match into.manufacturer_data.iter_mut().find(|(c, _)| *c == k) {
            Some(slot) => slot.1 = v,
            None => into.manufacturer_data.push((k, v)),
        }
    }
    for (k, v) in a.service_data {
        match into.service_data.iter_mut().find(|(c, _)| *c == k) {
            Some(slot) => slot.1 = v,
            None => into.service_data.push((k, v)),
        }
    }
    if a.tx_power.is_some() {
        into.tx_power = a.tx_power;
    }
    if a.connectable.is_some() {
        into.connectable = a.connectable;
    }
}

impl Table {
    /// Record an advertisement; at most `cap` devices are remembered.
    pub fn add(&mut self, a: BleAdvert, cap: usize) -> bool {
        let now = Instant::now();
        if let Some(&i) = self.index.get(&a.id) {
            let e = &mut self.entries[i];
            merge(&mut e.adv, a);
            e.seen = now;
            e.seen_unix = unix_now();
            return true;
        }
        if a.id.is_empty() || self.entries.len() >= cap {
            return false;
        }
        self.index.insert(a.id.clone(), self.entries.len());
        self.entries.push(Entry { adv: a, seen: now, seen_unix: unix_now() });
        true
    }

    /// Drop devices not heard from for `age`; true if any went.
    pub fn forget(&mut self, age: Duration) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.seen.elapsed() < age);
        if self.entries.len() == before {
            return false;
        }
        self.index = self.entries.iter().enumerate().map(|(i, e)| (e.adv.id.clone(), i)).collect();
        true
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.index.clear();
    }

    fn matches(r: &ScanReq, a: &BleAdvert) -> bool {
        (r.services.is_empty() || a.services.iter().any(|s| r.services.contains(s)))
            && r.name_prefix.as_ref().is_none_or(|p| a.name.as_ref().is_some_and(|n| n.to_lowercase().starts_with(p)))
            && r.min_rssi.is_none_or(|m| a.rssi.is_some_and(|x| x >= m))
    }

    pub fn devices(&self, r: &ScanReq) -> Vec<Data> {
        self.entries.iter().filter(|e| Self::matches(r, &e.adv)).take(r.max).map(|e| advert_data(&e.adv, e.seen_unix)).collect()
    }
}

pub(crate) fn advert_data(a: &BleAdvert, last_seen: f64) -> Data {
    let services = a.services.iter().map(|u| Data::str(&uuid_string(u))).collect();
    let manufacturer = a.manufacturer_data.iter().map(|(c, d)| Data::map([("company", Data::Num(*c as f64)), ("data", Data::bytes(d.clone()))])).collect();
    let service_data = a.service_data.iter().map(|(u, d)| Data::map([("uuid", Data::str(&uuid_string(u))), ("data", Data::bytes(d.clone()))])).collect();
    Data::map([
        ("id", Data::str(&a.id)),
        ("name", opt_s(&a.name)),
        ("rssi", opt_n(a.rssi)),
        ("services", Data::list(services)),
        ("manufacturer", Data::list(manufacturer)),
        ("service_data", Data::list(service_data)),
        ("tx_power", opt_n(a.tx_power)),
        ("connectable", a.connectable.map(Data::Bool).unwrap_or_default()),
        ("last_seen", Data::Num(last_seen)),
    ])
}

pub(crate) fn run(backend: Arc<dyn BleBackend>, mut io: StreamIo, stop: Arc<AtomicBool>) {
    let Inbox { tx, rx, .. } = match inbox::<ScanEvent>(&mut io, stop.clone(), "ble.scan") {
        Ok(p) => p,
        Err(e) => {
            (io.push)(Data::map([("scanning", Data::Bool(false)), ("error", Data::str(&e))]));
            return;
        }
    };
    let emit: Emit<ScanEvent> = Arc::new(move |e| {
        let _ = tx.send(Msg::Event(e));
    });
    let mut req: Option<ScanReq> = None;
    let mut guard: Option<Box<dyn Send>> = None;
    let mut table = Table::default();
    let mut error: Option<String> = None;
    let mut retry_at: Option<Instant> = None;
    let mut throttle = Throttle::new(Duration::from_millis(250));
    let mut last_forget = Instant::now();

    while alive(&io, &stop) {
        io.woke();
        let mut wait = IDLE_WAIT;
        if let Some(d) = throttle.due_in() {
            wait = wait.min(d);
        }
        // a retry only counts while the radio is wanted and not running
        // (switched off after a failure, the thread must sleep, not spin)
        if req.is_some() && io.on() && guard.is_none() {
            if let Some(t) = retry_at {
                wait = wait.min(t.saturating_duration_since(Instant::now()));
            }
        }
        match rx.recv_timeout(wait) {
            Ok(Msg::Request(d)) => match parse(&io.request(d)) {
                Ok(r) => {
                    let filter_changed = r.as_ref().map(|x| (&x.services, &x.name_prefix, x.min_rssi)) != req.as_ref().map(|x| (&x.services, &x.name_prefix, x.min_rssi));
                    let services_changed = r.as_ref().map(|x| &x.services) != req.as_ref().map(|x| &x.services);
                    if filter_changed {
                        table.clear();
                    }
                    if services_changed || r.is_none() {
                        guard = None;
                    }
                    if let Some(x) = &r {
                        throttle.interval = x.interval;
                    }
                    req = r;
                    error = None;
                    retry_at = None;
                    throttle.dirty = true;
                }
                Err(e) => {
                    error = Some(e);
                    throttle.dirty = true;
                }
            },
            Ok(Msg::Event(ScanEvent::Seen(a))) => {
                if let Some(r) = &req {
                    if table.add(a, (r.max * 4).min(8192)) {
                        throttle.dirty = true;
                    }
                }
            }
            Ok(Msg::Event(ScanEvent::Failed(e))) => {
                guard = None;
                error = Some(e);
                retry_at = Some(Instant::now() + RETRY);
                throttle.dirty = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // run the radio while requested and the stream is on
        let want = req.is_some() && io.on();
        if !want && guard.is_some() {
            guard = None;
            throttle.dirty = true;
        }
        if want && guard.is_none() && retry_at.is_none_or(|t| Instant::now() >= t) {
            let services = req.as_ref().map(|r| r.services.clone()).unwrap_or_default();
            match backend.scan(&services, emit.clone()) {
                Ok(g) => {
                    guard = Some(g);
                    if retry_at.take().is_some() {
                        error = None;
                    }
                }
                Err(e) => {
                    error = Some(e);
                    retry_at = Some(Instant::now() + RETRY);
                }
            }
            throttle.dirty = true;
        }
        if let Some(r) = &req {
            if last_forget.elapsed() >= Duration::from_secs(1) {
                last_forget = Instant::now();
                if table.forget(r.forget) {
                    throttle.dirty = true;
                }
            }
        }
        if throttle.ready() {
            let devices = req.as_ref().map(|r| table.devices(r)).unwrap_or_default();
            let item = Data::map([
                ("scanning", Data::Bool(guard.is_some())),
                ("count", Data::Num(devices.len() as f64)),
                ("devices", Data::list(devices)),
                ("error", error.as_deref().map(Data::str).unwrap_or_default()),
            ]);
            (io.push)(item);
            throttle.pushed();
        }
    }
    drop(guard);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adv(id: &str, name: Option<&str>, rssi: i16, services: &[&str]) -> BleAdvert {
        BleAdvert {
            id: id.into(),
            name: name.map(str::to_string),
            rssi: Some(rssi),
            services: services.iter().map(|s| parse_uuid(s).unwrap()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn requests() {
        assert_eq!(parse(&Data::Nil).unwrap(), None);
        assert_eq!(parse(&Data::Bool(false)).unwrap(), None);
        assert_eq!(parse(&Data::map([("on", Data::Bool(false))])).unwrap(), None);
        assert_eq!(parse(&Data::map([])).unwrap(), None, "{{}} is off, like nil");
        assert!(parse(&Data::map([("on", Data::Bool(true))])).unwrap().is_some());
        let r = parse(&Data::Bool(true)).unwrap().unwrap();
        assert!(r.services.is_empty());
        assert_eq!(r.max, 256);
        let r = parse(&Data::map([("services", Data::list(vec![Data::str("180d")])), ("max", Data::Num(1e9)), ("name_prefix", Data::str("Polar"))])).unwrap().unwrap();
        assert_eq!(r.services.len(), 1);
        assert_eq!(r.max, 2048);
        assert_eq!(r.name_prefix.as_deref(), Some("polar"));
        assert!(parse(&Data::map([("services", Data::list(vec![Data::Num(1.0)]))])).is_err());
        assert!(parse(&Data::map([("services", Data::list(vec![Data::str("nope")]))])).is_err());
    }

    #[test]
    fn table_merges_filters_and_forgets() {
        let r = parse(&Data::map([("services", Data::list(vec![Data::str("180d")]))])).unwrap().unwrap();
        let mut t = Table::default();
        assert!(t.add(adv("A", None, -70, &[]), 10));
        assert!(t.add(adv("B", Some("Other"), -50, &["180f"]), 10));
        // scan response with the name and the service arrives later
        assert!(t.add(adv("A", Some("Polar H10"), -60, &["180d"]), 10));
        let d = t.devices(&r);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].get("name").as_str(), Some("Polar H10"));
        assert_eq!(d[0].get("rssi").num(), Some(-60.0));
        assert_eq!(d[0].get("services").at(1).as_str(), Some("0000180d-0000-1000-8000-00805f9b34fb"));
        // order of first sighting, name and signal filters
        let all = parse(&Data::Bool(true)).unwrap().unwrap();
        let ids: Vec<String> = t.devices(&all).iter().map(|d| d.get("id").str_or("").to_string()).collect();
        assert_eq!(ids, vec!["A", "B"]);
        let strong = parse(&Data::map([("min_rssi", Data::Num(-55.0))])).unwrap().unwrap();
        assert_eq!(t.devices(&strong).len(), 1);
        let named = parse(&Data::map([("name_prefix", Data::str("polar"))])).unwrap().unwrap();
        assert_eq!(t.devices(&named)[0].get("id").as_str(), Some("A"));
        // capacity
        assert!(!t.add(adv("C", None, -40, &[]), 2));
        assert!(!t.add(adv("", None, -40, &[]), 10));
        // forgetting
        std::thread::sleep(Duration::from_millis(30));
        assert!(t.add(adv("B", None, -50, &[]), 10));
        assert!(t.forget(Duration::from_millis(20)));
        let ids: Vec<String> = t.devices(&all).iter().map(|d| d.get("id").str_or("").to_string()).collect();
        assert_eq!(ids, vec!["B"]);
        assert!(t.add(adv("A", None, -70, &[]), 10));
        assert_eq!(t.devices(&all).len(), 2);
    }
}
