//! Bluetooth LE: scanning, links (connect, discover, notify) and one-off
//! reads and writes.
//!
//! The page-facing logic (requests, stream items, errors) lives here and in
//! [`scan`] and [`link`]; a platform only implements [`BleBackend`] and
//! [`BleConnection`]: btleplug on Windows, the host bridge on Android, the
//! mock in tests.

pub(crate) mod hub;
pub(crate) mod link;
pub(crate) mod scan;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anypage_runtime::{Data, Effect, Natives};
use uuid::Uuid;

use crate::util::{self, bytes_of, clip, ident, is_empty, job_request, lock, millis, opt_str, printable_text, req_str, unix_now, Msg, Wait};

/// A callback a backend calls from its own threads; implementations only
/// queue the event.
pub type Emit<E> = Arc<dyn Fn(E) + Send + Sync>;

/// One advertisement (or a merged advertisement and scan response).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BleAdvert {
    /// What [`BleBackend::connect`] takes: the Bluetooth address
    /// "AA:BB:CC:DD:EE:FF" on Windows and Android.
    pub id: String,
    pub name: Option<String>,
    pub rssi: Option<i16>,
    pub services: Vec<Uuid>,
    /// (company identifier, payload)
    pub manufacturer_data: Vec<(u16, Vec<u8>)>,
    pub service_data: Vec<(Uuid, Vec<u8>)>,
    pub tx_power: Option<i16>,
    pub connectable: Option<bool>,
}

#[derive(Clone, Debug)]
pub enum ScanEvent {
    Seen(BleAdvert),
    /// The system stopped the scan (radio off, permission revoked, too many scans).
    Failed(String),
}

/// Characteristic property bits (Bluetooth Core spec, also Android's
/// `BluetoothGattCharacteristic.PROPERTY_*` and btleplug's `CharPropFlags`).
pub mod props {
    pub const BROADCAST: u8 = 0x01;
    pub const READ: u8 = 0x02;
    pub const WRITE_WITHOUT_RESPONSE: u8 = 0x04;
    pub const WRITE: u8 = 0x08;
    pub const NOTIFY: u8 = 0x10;
    pub const INDICATE: u8 = 0x20;
    pub const SIGNED_WRITE: u8 = 0x40;
    pub const EXTENDED: u8 = 0x80;
    pub(crate) const NAMES: [(u8, &str); 8] = [
        (BROADCAST, "broadcast"),
        (READ, "read"),
        (WRITE_WITHOUT_RESPONSE, "write_without_response"),
        (WRITE, "write"),
        (NOTIFY, "notify"),
        (INDICATE, "indicate"),
        (SIGNED_WRITE, "signed_write"),
        (EXTENDED, "extended"),
    ];
}

#[derive(Clone, Debug, PartialEq)]
pub struct GattChar {
    pub uuid: Uuid,
    /// [`props`] bits.
    pub properties: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GattService {
    pub uuid: Uuid,
    pub characteristics: Vec<GattChar>,
}

#[derive(Clone, Debug)]
pub enum LinkEvent {
    Notification { service: Uuid, characteristic: Uuid, data: Vec<u8> },
    /// The link dropped (out of range, device off, the OS gave up).
    Disconnected(String),
}

/// A Bluetooth LE stack.
pub trait BleBackend: Send + Sync + 'static {
    /// Start delivering advertisements to `events` until the returned guard
    /// is dropped. `services` is a hint (an empty list means all devices);
    /// callers filter again, so a backend may deliver more.
    fn scan(&self, services: &[Uuid], events: Emit<ScanEvent>) -> Result<Box<dyn Send>, String>;

    /// Connect to `id` and discover its services. Notifications of
    /// characteristics later subscribed, and a lost link, go to `events`.
    fn connect(&self, id: &str, events: Emit<LinkEvent>, wait: &Wait) -> Result<Arc<dyn BleConnection>, String>;

    /// `Err(reason)` when calls cannot work at all (Android: the host has
    /// not installed its device bridge). Must return at once.
    fn status(&self) -> Result<(), String> {
        Ok(())
    }
}

/// An open link. Dropping the last reference disconnects.
pub trait BleConnection: Send + Sync + 'static {
    fn services(&self) -> Vec<GattService>;
    fn read(&self, service: Uuid, characteristic: Uuid, wait: &Wait) -> Result<Vec<u8>, String>;
    fn write(&self, service: Uuid, characteristic: Uuid, data: &[u8], with_response: bool, wait: &Wait) -> Result<(), String>;
    fn subscribe(&self, service: Uuid, characteristic: Uuid, wait: &Wait) -> Result<(), String>;
    fn unsubscribe(&self, service: Uuid, characteristic: Uuid, wait: &Wait) -> Result<(), String>;
    fn connected(&self) -> bool;
    fn disconnect(&self);
}

/// The longest attribute value Bluetooth LE allows.
pub const MAX_ATTRIBUTE: usize = 512;

const BASE_UUID_TAIL: u128 = 0x0000_0000_0000_1000_8000_0080_5f9b_34fb;

/// "180d", "0x180D", "0000180d", or a full UUID → the full 128-bit UUID.
/// 16- and 32-bit forms expand with the Bluetooth base UUID.
pub fn parse_uuid(s: &str) -> Result<Uuid, String> {
    let t = s.trim();
    let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    if (hex.len() == 4 || hex.len() == 8) && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let short = u32::from_str_radix(hex, 16).map_err(|_| format!("not a UUID: \"{}\"", clip(s)))?;
        return Ok(Uuid::from_u128(((short as u128) << 96) | BASE_UUID_TAIL));
    }
    Uuid::parse_str(t).map_err(|_| format!("not a UUID: \"{}\" (use \"180d\", \"0x2a37\" or a full 128-bit UUID)", clip(s)))
}

/// Lower-case hyphenated form, the one every item carries.
pub fn uuid_string(u: &Uuid) -> String {
    u.hyphenated().to_string()
}

fn uuid_field(d: &Data, key: &str) -> Result<Option<Uuid>, String> {
    match opt_str(d, key) {
        Some(s) => parse_uuid(s).map(Some).map_err(|e| format!("\"{key}\": {e}")),
        None => Ok(None),
    }
}

pub(crate) fn prop_names(p: u8) -> Data {
    Data::list(props::NAMES.iter().filter(|(bit, _)| p & bit != 0).map(|(_, n)| Data::str(n)).collect())
}

pub(crate) fn services_data(services: &[GattService]) -> Data {
    Data::list(
        services
            .iter()
            .map(|s| {
                let chars = s
                    .characteristics
                    .iter()
                    .map(|c| Data::map([("uuid", Data::str(&uuid_string(&c.uuid))), ("properties", prop_names(c.properties))]))
                    .collect();
                Data::map([("uuid", Data::str(&uuid_string(&s.uuid))), ("characteristics", Data::list(chars))])
            })
            .collect(),
    )
}

/// The service a characteristic belongs to: the one asked for (checked), or
/// the first service that has it.
pub(crate) fn resolve(services: &[GattService], service: Option<Uuid>, characteristic: Uuid) -> Result<(Uuid, u8), String> {
    let found = services
        .iter()
        .filter(|s| service.is_none_or(|u| u == s.uuid))
        .find_map(|s| s.characteristics.iter().find(|c| c.uuid == characteristic).map(|c| (s.uuid, c.properties)));
    match (found, service) {
        (Some(f), _) => Ok(f),
        (None, Some(svc)) if !services.iter().any(|s| s.uuid == svc) => Err(format!("the device has no service {}", uuid_string(&svc))),
        (None, Some(svc)) => Err(format!("service {} has no characteristic {}", uuid_string(&svc), uuid_string(&characteristic))),
        (None, None) => Err(format!("the device has no characteristic {}", uuid_string(&characteristic))),
    }
}

/// A link a `ble.link` stream holds open.
pub(crate) struct OpenLink {
    pub conn: Weak<dyn BleConnection>,
    /// The stream thread's queue, for `ble.send`.
    pub outbox: Sender<Msg<link::LinkIn>>,
    pub pending: Arc<AtomicUsize>,
}

/// Shared by the streams and jobs of one app.
pub(crate) struct BleModule {
    pub backend: Arc<dyn BleBackend>,
    /// Links open in `ble.link` streams, by device id: jobs and `ble.send` use them.
    pub links: Mutex<HashMap<String, OpenLink>>,
}

/// Writes `ble.send` may queue on one link.
const MAX_QUEUED: usize = 256;

impl BleModule {
    pub fn new(backend: Arc<dyn BleBackend>) -> Arc<Self> {
        Arc::new(Self { backend, links: Mutex::new(HashMap::new()) })
    }

    /// An open link to `device`, or a new one the caller closes (`true`).
    pub fn connection(&self, device: &str, wait: &Wait) -> Result<(Arc<dyn BleConnection>, bool), String> {
        let device = device_id(device);
        let open = lock(&self.links).get(&device).and_then(|l| l.conn.upgrade()).filter(|c| c.connected());
        if let Some(c) = open {
            return Ok((c, false));
        }
        let quiet: Emit<LinkEvent> = Arc::new(|_| {});
        Ok((self.backend.connect(&device, quiet, wait)?, true))
    }

    /// `ble.send`: queue a write on the link a stream holds; never blocks.
    /// Failures show in that stream's `error`.
    pub fn send(&self, device: &str, w: link::QueuedWrite) -> Result<usize, String> {
        let links = lock(&self.links);
        let l = links
            .get(device)
            .filter(|l| l.conn.upgrade().is_some_and(|c| c.connected()))
            .ok_or_else(|| format!("{} is not connected in this app; connect it with a ble.link stream first", clip(device)))?;
        if l.pending.load(Ordering::Relaxed) >= MAX_QUEUED {
            return Err(format!("{device}: more than {MAX_QUEUED} writes are waiting (the device is slow or gone)"));
        }
        let n = w.data.len();
        l.pending.fetch_add(1, Ordering::Relaxed);
        l.outbox.send(Msg::Event(link::LinkIn::Write(w))).map_err(|_| format!("{device}: the link was closed"))?;
        Ok(n)
    }
}

/// Device ids are Bluetooth addresses, compared without regard to case;
/// items always carry the upper-case form.
pub(crate) fn device_id(s: &str) -> String {
    s.trim().to_uppercase()
}

/// `ble.read`: `{ device, service, characteristic, timeout_ms }` → `{ data, text, at }`.
pub(crate) fn read_job(m: &BleModule, req: &Data, cancelled: &dyn Fn() -> bool) -> Result<Data, String> {
    if is_empty(req) {
        return Ok(Data::Nil);
    }
    let device = req_str(req, "device")?;
    let characteristic = uuid_field(req, "characteristic")?.ok_or("missing \"characteristic\"")?;
    let service = uuid_field(req, "service")?;
    let wait = Wait::new(millis(req, "timeout_ms", 10_000, 120_000), cancelled);
    let (conn, temporary) = m.connection(device, &wait)?;
    let r = resolve(&conn.services(), service, characteristic).and_then(|(svc, p)| {
        if p & props::READ == 0 {
            return Err(format!("characteristic {} cannot be read (properties: {})", uuid_string(&characteristic), names_text(p)));
        }
        conn.read(svc, characteristic, &wait)
    });
    if temporary {
        conn.disconnect();
    }
    let data = r?;
    let text = printable_text(&data).map(Data::str).unwrap_or_default();
    Ok(Data::map([("text", text), ("data", Data::bytes(data)), ("at", Data::Num(unix_now()))]))
}

/// `ble.write`: `{ device, service, characteristic, data, response, timeout_ms }` → `{ written, at }`.
pub(crate) fn write_job(m: &BleModule, req: &Data, cancelled: &dyn Fn() -> bool) -> Result<Data, String> {
    if is_empty(req) {
        return Ok(Data::Nil);
    }
    let device = req_str(req, "device")?;
    let characteristic = uuid_field(req, "characteristic")?.ok_or("missing \"characteristic\"")?;
    let service = uuid_field(req, "service")?;
    let data = bytes_of(req.get("data"), "data", MAX_ATTRIBUTE)?;
    let wait = Wait::new(millis(req, "timeout_ms", 10_000, 120_000), cancelled);
    let (conn, temporary) = m.connection(device, &wait)?;
    let r = write_checked(&*conn, service, characteristic, &data, req.get("response").as_bool(), &wait);
    if temporary {
        conn.disconnect();
    }
    r?;
    Ok(Data::map([("written", Data::Num(data.len() as f64)), ("at", Data::Num(unix_now()))]))
}

/// Write after checking the characteristic allows it; `response` nil picks
/// "with a response" where the characteristic offers it.
pub(crate) fn write_checked(conn: &dyn BleConnection, service: Option<Uuid>, characteristic: Uuid, data: &[u8], response: Option<bool>, wait: &Wait) -> Result<(), String> {
    let (svc, p) = resolve(&conn.services(), service, characteristic)?;
    let with_response = response.unwrap_or(p & props::WRITE != 0 || p & props::WRITE_WITHOUT_RESPONSE == 0);
    let allowed = if with_response { props::WRITE } else { props::WRITE_WITHOUT_RESPONSE };
    if p & allowed == 0 {
        let how = if with_response { "with a response" } else { "without a response" };
        return Err(format!("characteristic {} cannot be written {how} (properties: {})", uuid_string(&characteristic), names_text(p)));
    }
    conn.write(svc, characteristic, data, with_response, wait)
}

fn names_text(p: u8) -> String {
    let v: Vec<&str> = props::NAMES.iter().filter(|(bit, _)| p & bit != 0).map(|(_, n)| *n).collect();
    if v.is_empty() {
        "none".into()
    } else {
        v.join(", ")
    }
}

pub(crate) fn register(n: &mut Natives, backend: Arc<dyn BleBackend>) {
    let m = BleModule::new(backend);

    n.function("ble.uuid", Effect::Pure, |c| {
        let s = c.arg(0).as_str().ok_or("expected a UUID string")?;
        Ok(Data::str(&uuid_string(&parse_uuid(s)?)))
    });

    let mm = m.clone();
    n.function("ble.available", Effect::Pure, move |_| Ok(util::availability(mm.backend.status())));

    let mm = m.clone();
    n.source("ble.scan", move |ctx| {
        let backend = mm.backend.clone();
        let io = util::StreamIo::from_ctx(ctx);
        let w = util::Worker::spawn("ble.scan", move |stop| scan::run(backend, io, stop))?;
        Ok(Box::new(w))
    });

    let mm = m.clone();
    n.source("ble.link", move |ctx| {
        let m = mm.clone();
        let io = util::StreamIo::from_ctx(ctx);
        let w = util::Worker::spawn("ble.link", move |stop| link::run(m, io, stop))?;
        Ok(Box::new(w))
    });

    let mm = m.clone();
    n.function("ble.send", Effect::Effect, move |c| {
        const USAGE: &str = "ble.send(device, characteristic, data[, { service, response }])";
        let device = device_id(ident(c.arg(0).as_str().ok_or(USAGE)?, "device")?);
        let characteristic = parse_uuid(c.arg(1).as_str().ok_or(USAGE)?)?;
        let data = bytes_of(c.arg(2), "data", MAX_ATTRIBUTE)?;
        let opts = c.arg(3);
        let w = link::QueuedWrite { service: uuid_field(opts, "service")?, characteristic, data, response: opts.get("response").as_bool() };
        Ok(Data::Num(mm.send(&device, w)? as f64))
    });

    let mm = m.clone();
    n.job_fn("ble.read", move |j| {
        let req = job_request(j);
        read_job(&mm, &req, &|| j.cancelled()).map_err(|e| format!("ble.read: {e}"))
    });

    let mm = m;
    n.job_fn("ble.write", move |j| {
        let req = job_request(j);
        write_job(&mm, &req, &|| j.cancelled()).map_err(|e| format!("ble.write: {e}"))
    });
}

/// Default connect timeout for links.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_in_every_form() {
        let hr = "0000180d-0000-1000-8000-00805f9b34fb";
        assert_eq!(uuid_string(&parse_uuid("180d").unwrap()), hr);
        assert_eq!(uuid_string(&parse_uuid("0x180D").unwrap()), hr);
        assert_eq!(uuid_string(&parse_uuid("0000180D").unwrap()), hr);
        assert_eq!(uuid_string(&parse_uuid(&hr.to_uppercase()).unwrap()), hr);
        assert_eq!(uuid_string(&parse_uuid("6e400001b5a3f393e0a9e50e24dcca9e").unwrap()), "6e400001-b5a3-f393-e0a9-e50e24dcca9e");
        assert!(parse_uuid("18").is_err());
        assert!(parse_uuid("zz0d").unwrap_err().contains("not a UUID"));
        // page input is quoted in errors only up to a point
        let e = parse_uuid(&"f".repeat(100_000)).unwrap_err();
        assert!(e.len() < 200 && e.contains("ffff…"), "{} bytes: {}", e.len(), &e[..e.len().min(200)]);
    }

    #[test]
    fn characteristics_resolve_to_services() {
        let s = |u: &str, cs: &[(&str, u8)]| GattService {
            uuid: parse_uuid(u).unwrap(),
            characteristics: cs.iter().map(|(c, p)| GattChar { uuid: parse_uuid(c).unwrap(), properties: *p }).collect(),
        };
        let svcs = vec![s("1800", &[("2a00", props::READ)]), s("180d", &[("2a37", props::NOTIFY), ("2a39", props::WRITE)])];
        let hr = parse_uuid("180d").unwrap();
        assert_eq!(resolve(&svcs, None, parse_uuid("2a37").unwrap()).unwrap(), (hr, props::NOTIFY));
        assert_eq!(resolve(&svcs, Some(hr), parse_uuid("2a39").unwrap()).unwrap().0, hr);
        assert!(resolve(&svcs, Some(hr), parse_uuid("2a00").unwrap()).unwrap_err().contains("has no characteristic"));
        assert!(resolve(&svcs, Some(parse_uuid("180f").unwrap()), parse_uuid("2a19").unwrap()).unwrap_err().contains("no service"));
        assert!(resolve(&svcs, None, parse_uuid("2a19").unwrap()).unwrap_err().contains("no characteristic"));
        let d = services_data(&svcs);
        assert_eq!(d.at(2).get("characteristics").at(1).get("properties").at(1).as_str(), Some("notify"));
        assert_eq!(names_text(props::READ | props::NOTIFY), "read, notify");
    }
}
