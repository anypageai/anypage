//! USB host: listing devices, opening and claiming interfaces, control,
//! bulk and interrupt transfers.
//!
//! A platform implements [`UsbBackend`] and [`UsbHandle`]: nusb on Windows
//! (WinUSB), nusb on a file descriptor the Android host opened, the mock in
//! tests.
//!
//! A device stays open only while a page holds it: `usb.open` returns a
//! `handle` resource (kind "usb.device"), and the device, with the
//! interfaces it claimed, is released when the last copy of that value goes
//! (the page closes, or the job runs again for another device) or on
//! `usb.close`. Transfers on a device no page holds open it for that one
//! job and release it after.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use anypage_runtime::{Data, Effect, Natives, Res};

use crate::util::{availability, bytes_of, clip, int, is_empty, job_request, lock, millis, opt_s, req_int, req_str, Wait};

/// Largest bulk or interrupt transfer a page may ask for at once.
pub const MAX_TRANSFER: usize = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpKind {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

impl EpKind {
    pub fn name(self) -> &'static str {
        match self {
            EpKind::Control => "control",
            EpKind::Isochronous => "isochronous",
            EpKind::Bulk => "bulk",
            EpKind::Interrupt => "interrupt",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsbEndpointInfo {
    /// Bit 7 set for IN (device to host).
    pub address: u8,
    pub kind: EpKind,
    pub max_packet: u16,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct UsbInterfaceInfo {
    pub number: u8,
    pub alt: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub name: Option<String>,
    /// Known once the device is open (listing does not read descriptors).
    pub endpoints: Vec<UsbEndpointInfo>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct UsbDeviceInfo {
    /// What [`UsbBackend::open`] takes: the device instance id on Windows,
    /// the usbfs device name ("/dev/bus/usb/001/004") on Android.
    pub id: String,
    pub vid: u16,
    pub pid: u16,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
    pub bus: String,
    pub address: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    /// "low", "full", "high", "super", "super_plus"
    pub speed: Option<&'static str>,
    /// The driver Windows bound to the device ("WinUSB", "usbccgp", "HidUsb", ...).
    pub driver: Option<String>,
    pub interfaces: Vec<UsbInterfaceInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlKind {
    Standard,
    Class,
    Vendor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recipient {
    Device,
    Interface,
    Endpoint,
    Other,
}

/// A SETUP packet without its direction and length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlSetup {
    pub kind: ControlKind,
    pub recipient: Recipient,
    pub request: u8,
    pub value: u16,
    pub index: u16,
}

impl ControlSetup {
    /// bmRequestType for this setup in direction `dir_in`.
    pub fn request_type(&self, dir_in: bool) -> u8 {
        let k = match self.kind {
            ControlKind::Standard => 0,
            ControlKind::Class => 1,
            ControlKind::Vendor => 2,
        };
        let r = match self.recipient {
            Recipient::Device => 0,
            Recipient::Interface => 1,
            Recipient::Endpoint => 2,
            Recipient::Other => 3,
        };
        ((dir_in as u8) << 7) | (k << 5) | r
    }
}

/// A USB host stack.
pub trait UsbBackend: Send + Sync + 'static {
    /// Enumerates the system's devices; blocking (5-30 ms on Windows).
    fn devices(&self) -> Result<Vec<UsbDeviceInfo>, String>;
    /// Open a device by [`UsbDeviceInfo::id`]. May wait for the user to
    /// grant access (Android) as long as `wait` allows.
    fn open(&self, id: &str, wait: &Wait) -> Result<Arc<dyn UsbHandle>, String>;

    /// `Err(reason)` when calls cannot work at all (Android: the host has
    /// not installed its device bridge). Must return at once.
    fn status(&self) -> Result<(), String> {
        Ok(())
    }
}

/// An open device. Dropping the last reference releases its interfaces and closes it.
pub trait UsbHandle: Send + Sync + 'static {
    /// Interfaces of the active configuration with their endpoints.
    fn interfaces(&self) -> Vec<UsbInterfaceInfo>;
    /// Claim for exclusive use; claiming a claimed interface is a no-op.
    fn claim(&self, interface: u8) -> Result<(), String>;
    fn release(&self, interface: u8);
    fn control_in(&self, setup: &ControlSetup, length: u16, interface: Option<u8>, wait: &Wait) -> Result<Vec<u8>, String>;
    fn control_out(&self, setup: &ControlSetup, data: &[u8], interface: Option<u8>, wait: &Wait) -> Result<usize, String>;
    /// A bulk or interrupt IN transfer of up to `length` bytes; `Ok(None)` if
    /// nothing arrived before the wait's deadline.
    fn read(&self, interface: u8, endpoint: u8, length: usize, wait: &Wait) -> Result<Option<Vec<u8>>, String>;
    /// A bulk or interrupt OUT transfer.
    fn write(&self, interface: u8, endpoint: u8, data: &[u8], wait: &Wait) -> Result<usize, String>;
}

pub(crate) fn interface_data(i: &UsbInterfaceInfo) -> Data {
    let eps = i
        .endpoints
        .iter()
        .map(|e| {
            Data::map([
                ("address", Data::Num(e.address as f64)),
                ("direction", Data::str(if e.address & 0x80 != 0 { "in" } else { "out" })),
                ("type", Data::str(e.kind.name())),
                ("max_packet", Data::Num(e.max_packet as f64)),
            ])
        })
        .collect();
    Data::map([
        ("number", Data::Num(i.number as f64)),
        ("alt", Data::Num(i.alt as f64)),
        ("class", Data::Num(i.class as f64)),
        ("subclass", Data::Num(i.subclass as f64)),
        ("protocol", Data::Num(i.protocol as f64)),
        ("name", opt_s(&i.name)),
        ("endpoints", Data::list(eps)),
    ])
}

pub(crate) fn device_data(d: &UsbDeviceInfo, interfaces: &[UsbInterfaceInfo]) -> Data {
    Data::map([
        ("id", Data::str(&d.id)),
        ("vid", Data::Num(d.vid as f64)),
        ("pid", Data::Num(d.pid as f64)),
        ("manufacturer", opt_s(&d.manufacturer)),
        ("product", opt_s(&d.product)),
        ("serial", opt_s(&d.serial)),
        ("bus", Data::str(&d.bus)),
        ("address", Data::Num(d.address as f64)),
        ("class", Data::Num(d.class as f64)),
        ("subclass", Data::Num(d.subclass as f64)),
        ("protocol", Data::Num(d.protocol as f64)),
        ("speed", d.speed.map(Data::str).unwrap_or_default()),
        ("driver", opt_s(&d.driver)),
        ("interfaces", Data::list(interfaces.iter().map(interface_data).collect())),
    ])
}

/// A device a page opened with `usb.open`, kept by the "usb.device"
/// resource in the job's result: the device stays open, with its
/// interfaces claimed, while any copy of that value lives.
pub(crate) struct Lease {
    id: String,
    st: Mutex<LeaseState>,
}

#[derive(Default)]
struct LeaseState {
    /// `None` after the device went away (reopened on next use) or `usb.close`.
    handle: Option<Arc<dyn UsbHandle>>,
    /// Interfaces claimed through `usb.open`, claimed again after a reopen.
    claims: Vec<u8>,
    closed: bool,
}

impl Lease {
    /// The device was unplugged: let go of `h`, so the next use reopens it.
    fn lost(&self, h: &Arc<dyn UsbHandle>) {
        let mut st = lock(&self.st);
        if st.handle.as_ref().is_some_and(|x| Arc::ptr_eq(x, h)) {
            st.handle = None;
        }
    }

    fn close(&self) {
        let mut st = lock(&self.st);
        st.closed = true;
        st.handle = None;
    }
}

/// The device one job works with.
struct Device {
    h: Arc<dyn UsbHandle>,
    lease: Option<Arc<Lease>>,
}

impl Device {
    /// Forget a handle whose device went away, so the next use reopens it.
    fn check<T>(&self, r: Result<T, String>) -> Result<T, String> {
        if let (Err(e), Some(lease)) = (&r, &self.lease) {
            if e.contains("disconnected") {
                lease.lost(&self.h);
            }
        }
        r
    }
}

/// Shared by the jobs of one app.
pub(crate) struct UsbModule {
    pub backend: Arc<dyn UsbBackend>,
    /// Devices pages hold open, by id. Weak: the pages' values own them.
    leases: Mutex<HashMap<String, Weak<Lease>>>,
}

impl UsbModule {
    pub fn new(backend: Arc<dyn UsbBackend>) -> Arc<Self> {
        Arc::new(Self { backend, leases: Mutex::new(HashMap::new()) })
    }

    /// The live lease on `id`, if a page holds the device open.
    fn lease(&self, id: &str) -> Option<Arc<Lease>> {
        let mut leases = lock(&self.leases);
        leases.retain(|_, w| w.strong_count() > 0);
        leases.get(id).and_then(Weak::upgrade).filter(|l| !lock(&l.st).closed)
    }

    /// The live lease on `id`, or a new one (not yet open).
    fn lease_or_new(&self, id: &str) -> Arc<Lease> {
        let mut leases = lock(&self.leases);
        leases.retain(|_, w| w.strong_count() > 0);
        if let Some(l) = leases.get(id).and_then(Weak::upgrade).filter(|l| !lock(&l.st).closed) {
            return l;
        }
        let l = Arc::new(Lease { id: id.to_string(), st: Mutex::default() });
        leases.insert(id.to_string(), Arc::downgrade(&l));
        l
    }

    /// The lease's device, opened again (and its interfaces claimed again)
    /// if it went away. The lease is not locked while opening, which may
    /// wait for the user (Android).
    fn lease_handle(&self, lease: &Lease, wait: &Wait) -> Result<Arc<dyn UsbHandle>, String> {
        let claims = {
            let st = lock(&lease.st);
            if let Some(h) = &st.handle {
                return Ok(h.clone());
            }
            st.claims.clone()
        };
        let h = self.backend.open(&lease.id, wait)?;
        for n in claims {
            h.claim(n)?;
        }
        let mut st = lock(&lease.st);
        if st.closed {
            // closed meanwhile: this job still gets it, the lease does not keep it
            return Ok(h);
        }
        Ok(st.handle.get_or_insert(h).clone())
    }

    /// The device for one job: the one a page holds open, or opened for
    /// this job alone (released when the job ends).
    fn device(&self, id: &str, wait: &Wait) -> Result<Device, String> {
        match self.lease(id) {
            Some(lease) => Ok(Device { h: self.lease_handle(&lease, wait)?, lease: Some(lease) }),
            None => Ok(Device { h: self.backend.open(id, wait)?, lease: None }),
        }
    }

    /// `usb.close`: release a device a page holds open.
    pub fn close(&self, id: &str) -> bool {
        match self.lease(id) {
            Some(l) => {
                l.close();
                lock(&self.leases).remove(id);
                true
            }
            None => false,
        }
    }
}

/// The interface that has `endpoint` in the active configuration.
pub(crate) fn interface_of(interfaces: &[UsbInterfaceInfo], endpoint: u8) -> Result<(u8, &UsbEndpointInfo), String> {
    interfaces
        .iter()
        .find_map(|i| i.endpoints.iter().find(|e| e.address == endpoint).map(|e| (i.number, e)))
        .ok_or_else(|| {
            let all: Vec<String> = interfaces.iter().flat_map(|i| i.endpoints.iter().map(|e| format!("0x{:02x}", e.address))).collect();
            format!("endpoint 0x{endpoint:02x} is not in the device's active configuration (it has {})", if all.is_empty() { "no endpoints".into() } else { all.join(", ") })
        })
}

fn setup_of(req: &Data) -> Result<ControlSetup, String> {
    let kind = match req.get("type").as_str().unwrap_or("vendor") {
        "standard" => ControlKind::Standard,
        "class" => ControlKind::Class,
        "vendor" => ControlKind::Vendor,
        other => return Err(format!("\"type\" must be \"standard\", \"class\" or \"vendor\", got \"{}\"", clip(other))),
    };
    let recipient = match req.get("recipient").as_str().unwrap_or("device") {
        "device" => Recipient::Device,
        "interface" => Recipient::Interface,
        "endpoint" => Recipient::Endpoint,
        "other" => Recipient::Other,
        other => return Err(format!("\"recipient\" must be \"device\", \"interface\", \"endpoint\" or \"other\", got \"{}\"", clip(other))),
    };
    Ok(ControlSetup {
        kind,
        recipient,
        request: req_int(req, "request", 0, 255)? as u8,
        value: int(req, "value", 0, 65535)?.unwrap_or(0) as u16,
        index: int(req, "index", 0, 65535)?.unwrap_or(0) as u16,
    })
}

/// `usb.control`: `{ device, direction, type, recipient, request, value,
/// index, length | data, interface, timeout_ms }` → `{ data, length }` (in)
/// or `{ written }` (out).
pub(crate) fn control_job(m: &UsbModule, req: &Data, cancelled: &dyn Fn() -> bool) -> Result<Data, String> {
    if is_empty(req) {
        return Ok(Data::Nil);
    }
    let id = req_str(req, "device")?;
    let setup = setup_of(req)?;
    let dir_in = match req.get("direction").as_str() {
        Some("in") => true,
        Some("out") => false,
        Some(other) => return Err(format!("\"direction\" must be \"in\" or \"out\", got \"{}\"", clip(other))),
        None => req.get("data").is_nil(),
    };
    let interface = int(req, "interface", 0, 255)?.map(|i| i as u8);
    let length = if dir_in { req_int(req, "length", 0, 65535)? as u16 } else { 0 };
    let data = if dir_in || req.get("data").is_nil() { Vec::new() } else { bytes_of(req.get("data"), "data", 65535)? };
    let wait = Wait::new(millis(req, "timeout_ms", 1000, 60_000), cancelled);
    let dev = m.device(id, &wait)?;
    if dir_in {
        let data = dev.check(dev.h.control_in(&setup, length, interface, &wait))?;
        Ok(Data::map([("length", Data::Num(data.len() as f64)), ("data", Data::bytes(data))]))
    } else {
        let n = dev.check(dev.h.control_out(&setup, &data, interface, &wait))?;
        Ok(Data::map([("written", Data::Num(n as f64))]))
    }
}

fn endpoint_target(h: &dyn UsbHandle, req: &Data, dir_in: bool) -> Result<(u8, u8, UsbEndpointInfo), String> {
    let endpoint = req_int(req, "endpoint", 0, 255)? as u8;
    if (endpoint & 0x80 != 0) != dir_in {
        let (want, hint) = if dir_in { ("IN", "0x81..0x8f") } else { ("OUT", "0x01..0x0f") };
        return Err(format!("endpoint 0x{endpoint:02x} is not an {want} endpoint ({want} endpoints are {hint})"));
    }
    let interfaces = h.interfaces();
    let (found, ep) = interface_of(&interfaces, endpoint)?;
    if !matches!(ep.kind, EpKind::Bulk | EpKind::Interrupt) {
        return Err(format!("endpoint 0x{endpoint:02x} is a {} endpoint; only bulk and interrupt endpoints are supported", ep.kind.name()));
    }
    let interface = match int(req, "interface", 0, 255)? {
        Some(i) if i as u8 != found => return Err(format!("endpoint 0x{endpoint:02x} belongs to interface {found}, not {i}")),
        _ => found,
    };
    Ok((interface, endpoint, ep.clone()))
}

/// `usb.bulk_in`: `{ device, endpoint, length, interface, timeout_ms }` → `{ data, length }`.
/// Works for interrupt IN endpoints too.
pub(crate) fn bulk_in_job(m: &UsbModule, req: &Data, cancelled: &dyn Fn() -> bool) -> Result<Data, String> {
    if is_empty(req) {
        return Ok(Data::Nil);
    }
    let id = req_str(req, "device")?;
    let length = req_int(req, "length", 1, MAX_TRANSFER as i64)? as usize;
    let wait = Wait::new(millis(req, "timeout_ms", 5000, 600_000), cancelled);
    let dev = m.device(id, &wait)?;
    let (interface, endpoint, _) = endpoint_target(&*dev.h, req, true)?;
    dev.check(dev.h.claim(interface))?;
    match dev.check(dev.h.read(interface, endpoint, length, &wait))? {
        Some(data) => Ok(Data::map([("length", Data::Num(data.len() as f64)), ("data", Data::bytes(data))])),
        None if wait.cancelled() => Err("cancelled".into()),
        None => Err(format!("no data from endpoint 0x{endpoint:02x} within {} ms", wait.timeout().as_millis())),
    }
}

/// `usb.bulk_out`: `{ device, endpoint, data, interface, timeout_ms }` → `{ written }`.
pub(crate) fn bulk_out_job(m: &UsbModule, req: &Data, cancelled: &dyn Fn() -> bool) -> Result<Data, String> {
    if is_empty(req) {
        return Ok(Data::Nil);
    }
    let id = req_str(req, "device")?;
    let data = bytes_of(req.get("data"), "data", MAX_TRANSFER)?;
    let wait = Wait::new(millis(req, "timeout_ms", 5000, 600_000), cancelled);
    let dev = m.device(id, &wait)?;
    let (interface, endpoint, _) = endpoint_target(&*dev.h, req, false)?;
    dev.check(dev.h.claim(interface))?;
    let n = dev.check(dev.h.write(interface, endpoint, &data, &wait))?;
    Ok(Data::map([("written", Data::Num(n as f64))]))
}

/// `usb.open`: `{ device, claim = { interface numbers } | "all" }` → the
/// device with its interfaces and endpoints, and `handle`, the resource
/// that keeps it open. Claiming checks the app may do transfers (on
/// Windows: WinUSB is bound).
pub(crate) fn open_job(m: &UsbModule, req: &Data, cancelled: &dyn Fn() -> bool) -> Result<Data, String> {
    if is_empty(req) {
        return Ok(Data::Nil);
    }
    let id = req_str(req, "device")?;
    const BAD_CLAIM: &str = "claim must be a list of interface numbers or \"all\"";
    let claim_all = matches!(req.get("claim"), Data::Str(s) if &**s == "all");
    let mut claim: Vec<u8> = match req.get("claim") {
        Data::Nil => Vec::new(),
        _ if claim_all => Vec::new(),
        Data::List(l) => l.iter().map(|c| c.num().filter(|n| n.fract() == 0.0 && (0.0..=255.0).contains(n)).map(|n| n as u8)).collect::<Option<_>>().ok_or(BAD_CLAIM)?,
        _ => return Err(BAD_CLAIM.into()),
    };
    // Android may show a permission dialog: give the user time.
    let wait = Wait::new(millis(req, "timeout_ms", 60_000, 600_000), cancelled);
    let lease = m.lease_or_new(id);
    let dev = Device { h: m.lease_handle(&lease, &wait)?, lease: Some(lease.clone()) };
    if claim_all {
        claim = dev.h.interfaces().iter().map(|i| i.number).collect();
    }
    for n in claim {
        dev.check(dev.h.claim(n))?;
        let mut st = lock(&lease.st);
        if !st.claims.contains(&n) {
            st.claims.push(n);
        }
    }
    let info = m.backend.devices().ok().and_then(|all| all.into_iter().find(|d| d.id == id)).unwrap_or_else(|| UsbDeviceInfo { id: id.to_string(), ..Default::default() });
    let mut out = match device_data(&info, &dev.h.interfaces()) {
        Data::Map(fields) => (*fields).clone(),
        _ => Default::default(),
    };
    out.insert(Arc::from("handle"), Data::Res(Res::from_arc("usb.device", lease)));
    Ok(Data::Map(Arc::new(out)))
}

/// The device id in `usb.close(x)`: an id, the `handle` from `usb.open`,
/// or the whole result table.
fn close_target(d: &Data) -> Option<String> {
    match d {
        Data::Str(s) => Some(s.to_string()),
        Data::Res(r) => r.downcast_arc::<Lease>().map(|l| l.id.clone()),
        Data::Map(_) => d.get("id").as_str().map(str::to_string),
        _ => None,
    }
}

fn list(m: &UsbModule) -> Result<Data, String> {
    let list = m.backend.devices()?;
    Ok(Data::list(list.iter().map(|d| device_data(d, &d.interfaces)).collect()))
}

pub(crate) fn register(n: &mut Natives, backend: Arc<dyn UsbBackend>) {
    let m = UsbModule::new(backend);

    // blocking enumeration: for handlers; cells use the usb.list job
    let mm = m.clone();
    n.function("usb.devices", Effect::Pure, move |_| list(&mm));

    let mm = m.clone();
    n.job_fn("usb.list", move |_| list(&mm).map_err(|e| format!("usb.list: {e}")));

    let mm = m.clone();
    n.function("usb.available", Effect::Pure, move |_| Ok(availability(mm.backend.status())));

    let mm = m.clone();
    n.function("usb.close", Effect::Effect, move |c| {
        let id = close_target(c.arg(0)).ok_or("expected a device id or the handle from usb.open")?;
        Ok(Data::Bool(mm.close(&id)))
    });

    type UsbJob = fn(&UsbModule, &Data, &dyn Fn() -> bool) -> Result<Data, String>;
    let jobs: [(&str, UsbJob); 4] = [("usb.open", open_job), ("usb.control", control_job), ("usb.bulk_in", bulk_in_job), ("usb.bulk_out", bulk_out_job)];
    for (name, f) in jobs {
        let mm = m.clone();
        n.job_fn(name, move |j| {
            let req = job_request(j);
            f(&mm, &req, &|| j.cancelled()).map_err(|e| format!("{name}: {e}"))
        });
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_types() {
        let s = ControlSetup { kind: ControlKind::Vendor, recipient: Recipient::Device, request: 1, value: 0, index: 0 };
        assert_eq!(s.request_type(true), 0xc0);
        assert_eq!(s.request_type(false), 0x40);
        let s = ControlSetup { kind: ControlKind::Class, recipient: Recipient::Interface, request: 0x20, value: 0, index: 0 };
        assert_eq!(s.request_type(false), 0x21);
        assert_eq!(s.request_type(true), 0xa1);
        let s = ControlSetup { kind: ControlKind::Standard, recipient: Recipient::Device, request: 6, value: 0x100, index: 0 };
        assert_eq!(s.request_type(true), 0x80);
    }

    #[test]
    fn setups_are_validated() {
        let ok = Data::map([("request", Data::Num(6.0)), ("type", Data::str("standard")), ("value", Data::Num(256.0))]);
        assert_eq!(setup_of(&ok).unwrap().value, 256);
        assert!(setup_of(&Data::map([("request", Data::Num(256.0))])).unwrap_err().contains("0..=255"));
        assert!(setup_of(&Data::map([])).unwrap_err().contains("missing \"request\""));
        assert!(setup_of(&Data::map([("request", Data::Num(1.0)), ("type", Data::str("odd"))])).unwrap_err().contains("\"type\""));
        assert!(setup_of(&Data::map([("request", Data::Num(1.0)), ("recipient", Data::str("bus"))])).unwrap_err().contains("\"recipient\""));
        assert!(setup_of(&Data::map([("request", Data::Num(1.0)), ("value", Data::Num(-1.0))])).is_err());
    }

    #[test]
    fn close_takes_an_id_or_the_handle() {
        let lease = Arc::new(Lease { id: "usb-loop".into(), st: Mutex::default() });
        let handle = Data::Res(Res::from_arc("usb.device", lease));
        assert_eq!(close_target(&handle).as_deref(), Some("usb-loop"));
        assert_eq!(close_target(&Data::str("x")).as_deref(), Some("x"));
        assert_eq!(close_target(&Data::map([("id", Data::str("y")), ("handle", handle)])).as_deref(), Some("y"));
        assert_eq!(close_target(&Data::Res(Res::new("pdf.document", 1u8))), None, "another module's resource");
        assert_eq!(close_target(&Data::Num(1.0)), None);
    }

    #[test]
    fn leases_reopen_after_a_replug_and_let_go() {
        use crate::mock::{MockUsb, MockUsbDevice};
        let usb = MockUsb::new(vec![MockUsbDevice::vendor("v")]);
        let m = UsbModule::new(Arc::new(usb.clone()));
        let w = Wait::timeout_only(std::time::Duration::from_secs(1));
        let lease = m.lease_or_new("v");
        lock(&lease.st).claims.push(0);
        let h = m.lease_handle(&lease, &w).unwrap();
        h.claim(0).unwrap();
        // jobs use the held device
        assert!(m.device("v", &w).unwrap().lease.is_some());
        assert_eq!((usb.opens("v"), usb.handles("v")), (1, 1));
        // unplugged: the transfer error drops the handle, the next use reopens and claims again
        usb.set_present("v", false);
        let dev = m.device("v", &w).unwrap();
        assert!(dev.check(dev.h.write(0, 0x01, b"x", &w)).unwrap_err().contains("disconnected"));
        drop((dev, h));
        assert_eq!(usb.handles("v"), 0);
        usb.set_present("v", true);
        let dev = m.device("v", &w).unwrap();
        assert_eq!(dev.h.write(0, 0x01, b"x", &w), Ok(1), "interface 0 was claimed again");
        drop(dev);
        assert_eq!((usb.opens("v"), usb.handles("v")), (2, 1));
        // the last holder goes: released, and the module forgets it
        drop(lease);
        assert_eq!(usb.handles("v"), 0);
        assert!(m.lease("v").is_none());
        assert!(!m.close("v"));
    }

    #[test]
    fn endpoints_resolve_to_interfaces() {
        let ifs = vec![
            UsbInterfaceInfo { number: 0, endpoints: vec![UsbEndpointInfo { address: 0x83, kind: EpKind::Interrupt, max_packet: 8 }], ..Default::default() },
            UsbInterfaceInfo {
                number: 1,
                endpoints: vec![UsbEndpointInfo { address: 0x02, kind: EpKind::Bulk, max_packet: 64 }, UsbEndpointInfo { address: 0x82, kind: EpKind::Bulk, max_packet: 64 }],
                ..Default::default()
            },
        ];
        assert_eq!(interface_of(&ifs, 0x82).unwrap().0, 1);
        assert_eq!(interface_of(&ifs, 0x83).unwrap().0, 0);
        let e = interface_of(&ifs, 0x81).unwrap_err();
        assert!(e.contains("0x83, 0x02, 0x82"), "{e}");
        assert!(interface_of(&[], 0x81).unwrap_err().contains("no endpoints"));
        let d = interface_data(&ifs[1]);
        assert_eq!(d.get("endpoints").at(2).get("direction").as_str(), Some("in"));
    }
}
