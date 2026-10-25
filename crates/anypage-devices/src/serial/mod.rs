//! Serial ports: listing, a reading stream with line splitting, writes.
//!
//! [`SerialBackend`] is the platform part: the `serialport` crate on
//! Windows (COM ports), CDC-ACM over USB on Android ([`cdc::CdcSerial`]),
//! loopback ports in the mock.

pub mod cdc;
pub(crate) mod port;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anypage_runtime::{Data, Effect, Natives};

use crate::util::{availability, bytes_of, clip, ident, int, is_empty, job_request, lock, millis, opt_n, opt_s, req_str, Wait};

/// Largest single write a page may ask for, and the most a port may have queued.
pub const MAX_WRITE: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct SerialPortInfo {
    /// What [`SerialBackend::open`] takes: "COM3" on Windows, "usb:<device id>" for CDC-ACM.
    pub path: String,
    /// "usb", "pci", "bluetooth" or "unknown".
    pub kind: &'static str,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub serial: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parity {
    None,
    Odd,
    Even,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowControl {
    None,
    Software,
    Hardware,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SerialSettings {
    pub baud: u32,
    /// 5..=8
    pub data_bits: u8,
    pub parity: Parity,
    /// 1 or 2
    pub stop_bits: u8,
    pub flow: FlowControl,
    /// Set DTR / RTS after opening (`None` leaves the driver default; CDC-ACM raises both).
    pub dtr: Option<bool>,
    pub rts: Option<bool>,
}

impl Default for SerialSettings {
    fn default() -> Self {
        Self { baud: 115_200, data_bits: 8, parity: Parity::None, stop_bits: 1, flow: FlowControl::None, dtr: None, rts: None }
    }
}

/// `{ baud, data_bits, parity, stop_bits, flow, dtr, rts }` with defaults 115200 8N1.
pub fn settings_of(req: &Data) -> Result<SerialSettings, String> {
    let d = SerialSettings::default();
    let parity = match req.get("parity").as_str().unwrap_or("none") {
        "none" => Parity::None,
        "odd" => Parity::Odd,
        "even" => Parity::Even,
        other => return Err(format!("\"parity\" must be \"none\", \"odd\" or \"even\", got \"{}\"", clip(other))),
    };
    let flow = match req.get("flow").as_str().unwrap_or("none") {
        "none" => FlowControl::None,
        "software" | "xonxoff" => FlowControl::Software,
        "hardware" | "rtscts" => FlowControl::Hardware,
        other => return Err(format!("\"flow\" must be \"none\", \"software\" or \"hardware\", got \"{}\"", clip(other))),
    };
    Ok(SerialSettings {
        baud: int(req, "baud", 50, 20_000_000)?.map(|b| b as u32).unwrap_or(d.baud),
        data_bits: int(req, "data_bits", 5, 8)?.map(|b| b as u8).unwrap_or(d.data_bits),
        parity,
        stop_bits: int(req, "stop_bits", 1, 2)?.map(|b| b as u8).unwrap_or(d.stop_bits),
        flow,
        dtr: req.get("dtr").as_bool(),
        rts: req.get("rts").as_bool(),
    })
}

/// A serial port driver.
pub trait SerialBackend: Send + Sync + 'static {
    /// Enumerates the system's ports; blocking (tens of milliseconds on Windows).
    fn ports(&self) -> Result<Vec<SerialPortInfo>, String>;
    /// May wait (for the user to allow a USB device on Android) as long as
    /// `wait` allows; must give up when it is cancelled.
    fn open(&self, path: &str, settings: &SerialSettings, wait: &Wait) -> Result<Arc<dyn SerialConn>, String>;

    /// `Err(reason)` when calls cannot work at all (Android: the host has
    /// not installed its device bridge). Must return at once.
    fn status(&self) -> Result<(), String> {
        Ok(())
    }
}

/// An open port. Reads and writes may run at the same time from different
/// threads. Dropping the last reference closes it.
pub trait SerialConn: Send + Sync + 'static {
    /// What has arrived, up to `max` bytes, waiting up to `timeout` for the
    /// first byte; empty if nothing came.
    fn read(&self, max: usize, timeout: Duration) -> Result<Vec<u8>, String>;
    /// Write as much of `data` as the port takes before `wait` ends
    /// (cancelled, or past its deadline) and return how much that was. A
    /// port held up by flow control or a slow baud rate returns short; `Err`
    /// only when the port failed. Implementations check `wait` at least
    /// every few tens of milliseconds.
    fn write(&self, data: &[u8], wait: &Wait) -> Result<usize, String>;
}

/// Write all of `data`, or say why not and how far it got.
pub fn write_all(conn: &dyn SerialConn, data: &[u8], wait: &Wait) -> Result<(), String> {
    let n = conn.write(data, wait)?;
    if n >= data.len() {
        return Ok(());
    }
    Err(if wait.cancelled() {
        format!("cancelled with {n} of {} bytes written", data.len())
    } else {
        format!("timed out after {} ms with {n} of {} bytes written (flow control holding, or the device is not reading?)", wait.timeout().as_millis(), data.len())
    })
}

pub(crate) fn port_data(p: &SerialPortInfo) -> Data {
    Data::map([
        ("path", Data::str(&p.path)),
        ("type", Data::str(p.kind)),
        ("vid", opt_n(p.vid)),
        ("pid", opt_n(p.pid)),
        ("serial", opt_s(&p.serial)),
        ("manufacturer", opt_s(&p.manufacturer)),
        ("product", opt_s(&p.product)),
    ])
}

/// A port held open by a `serial.port` stream.
pub(crate) struct OpenPort {
    pub conn: Arc<dyn SerialConn>,
    /// Writes queued by `serial.send`, done by the stream's thread.
    pub queue: Sender<Vec<u8>>,
    pub queued: Arc<AtomicUsize>,
    pub tx: Arc<AtomicU64>,
    /// Which stream owns the entry (a newer stream may replace it).
    pub owner: u64,
}

pub(crate) struct SerialModule {
    pub backend: Arc<dyn SerialBackend>,
    pub ports: Mutex<HashMap<String, OpenPort>>,
    pub next_owner: AtomicU64,
}

impl SerialModule {
    pub fn new(backend: Arc<dyn SerialBackend>) -> Arc<Self> {
        Arc::new(Self { backend, ports: Mutex::new(HashMap::new()), next_owner: AtomicU64::new(1) })
    }

    /// `serial.send`: queue bytes for an open port; never blocks.
    pub fn send(&self, path: &str, data: Vec<u8>) -> Result<usize, String> {
        let ports = lock(&self.ports);
        let p = ports.get(path).ok_or_else(|| format!("{} is not open in this app; open it with page.stream(name, \"serial.port\", {{ request = ... }}) first", clip(path)))?;
        let n = data.len();
        if p.queued.load(Ordering::Relaxed) + n > MAX_WRITE {
            return Err(format!("{}: more than {MAX_WRITE} bytes are waiting to be written (the device is not reading?)", clip(path)));
        }
        p.queued.fetch_add(n, Ordering::Relaxed);
        p.queue.send(data).map_err(|_| format!("{} was closed", clip(path)))?;
        Ok(n)
    }
}

/// `serial.write`: `{ path, data, timeout_ms, ...settings }` → `{ written }`.
/// Uses the port a stream holds open, or opens it for this write. A write
/// the port does not take in time ends with how far it got; cancelling the
/// job (a new request, the page closing) stops it within a few tens of ms.
pub(crate) fn write_job(m: &SerialModule, req: &Data, cancelled: &dyn Fn() -> bool) -> Result<Data, String> {
    if is_empty(req) {
        return Ok(Data::Nil);
    }
    let path = req_str(req, "path")?;
    let data = bytes_of(req.get("data"), "data", MAX_WRITE)?;
    let wait = Wait::new(millis(req, "timeout_ms", 5000, 120_000), cancelled);
    let open = lock(&m.ports).get(path).map(|p| (p.conn.clone(), p.tx.clone()));
    match open {
        Some((conn, tx)) => {
            let r = write_all(&*conn, &data, &wait);
            if r.is_ok() {
                tx.fetch_add(data.len() as u64, Ordering::Relaxed);
            }
            r?;
        }
        None => {
            let conn = m.backend.open(path, &settings_of(req)?, &wait)?;
            write_all(&*conn, &data, &wait)?;
        }
    }
    Ok(Data::map([("written", Data::Num(data.len() as f64))]))
}

fn list(m: &SerialModule) -> Result<Data, String> {
    Ok(Data::list(m.backend.ports()?.iter().map(port_data).collect()))
}

pub(crate) fn register(n: &mut Natives, backend: Arc<dyn SerialBackend>) {
    let m = SerialModule::new(backend);

    // blocking enumeration: for handlers; cells use the serial.list job
    let mm = m.clone();
    n.function("serial.ports", Effect::Pure, move |_| list(&mm));

    let mm = m.clone();
    n.job_fn("serial.list", move |_| list(&mm).map_err(|e| format!("serial.list: {e}")));

    let mm = m.clone();
    n.function("serial.available", Effect::Pure, move |_| Ok(availability(mm.backend.status())));

    let mm = m.clone();
    n.function("serial.send", Effect::Effect, move |c| {
        let path = ident(c.arg(0).as_str().ok_or("expected a port path, then the data")?, "path")?;
        let data = bytes_of(c.arg(1), "data", MAX_WRITE)?;
        Ok(Data::Num(mm.send(path, data)? as f64))
    });

    let mm = m.clone();
    n.source("serial.port", move |ctx| {
        let m = mm.clone();
        let io = crate::util::StreamIo::from_ctx(ctx);
        let w = crate::util::Worker::spawn("serial.port", move |stop| port::run(m, io, stop))?;
        Ok(Box::new(w))
    });

    let mm = m;
    n.job_fn("serial.write", move |j| {
        let req = job_request(j);
        write_job(&mm, &req, &|| j.cancelled()).map_err(|e| format!("serial.write: {e}"))
    });
}

/// How long one read waits before the stream thread looks at requests and writes.
pub(crate) const READ_SLICE: Duration = Duration::from_millis(20);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        let s = settings_of(&Data::Nil).unwrap();
        assert_eq!(s, SerialSettings::default());
        let s = settings_of(&Data::map([("baud", Data::Num(9600.0)), ("parity", Data::str("even")), ("stop_bits", Data::Num(2.0)), ("flow", Data::str("rtscts")), ("dtr", Data::Bool(false))])).unwrap();
        assert_eq!((s.baud, s.parity, s.stop_bits, s.flow, s.dtr), (9600, Parity::Even, 2, FlowControl::Hardware, Some(false)));
        assert!(settings_of(&Data::map([("baud", Data::Num(9600.5))])).is_err());
        assert!(settings_of(&Data::map([("data_bits", Data::Num(9.0))])).unwrap_err().contains("5..=8"));
        assert!(settings_of(&Data::map([("parity", Data::str("mark"))])).unwrap_err().contains("parity"));
        assert!(settings_of(&Data::map([("flow", Data::str("dsr"))])).unwrap_err().contains("flow"));
    }
}
