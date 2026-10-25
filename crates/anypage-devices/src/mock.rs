//! Simulated devices: a Bluetooth LE heart-rate monitor with a UART
//! service that echoes, a vendor USB device and a CDC-ACM USB serial
//! adapter with loopback endpoints, and loopback serial ports.
//!
//! Tests drive the real module logic through these, and a host can offer
//! them as a "simulated devices" mode on machines with no hardware.
//! Every mock can also be steered from the outside: inject data, unplug a
//! device, drop a link, switch the radio off.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::ble::{parse_uuid, props, BleAdvert, BleBackend, BleConnection, Emit, GattChar, GattService, LinkEvent, ScanEvent};
use crate::serial::cdc::{CLASS_COMM, CLASS_DATA, GET_LINE_CODING, SET_CONTROL_LINE_STATE, SET_LINE_CODING, SUBCLASS_ACM};
use crate::serial::{SerialBackend, SerialConn, SerialPortInfo, SerialSettings};
use crate::usb::{ControlKind, ControlSetup, EpKind, UsbBackend, UsbDeviceInfo, UsbEndpointInfo, UsbHandle, UsbInterfaceInfo};
use crate::util::{lock, Wait, SLICE};
use crate::Backends;

fn u(s: &str) -> Uuid {
    parse_uuid(s).unwrap_or_default()
}

// ---------------------------------------------------------------- Bluetooth LE

/// A simulated peripheral.
#[derive(Clone, Debug)]
pub struct MockPeripheral {
    pub advert: BleAdvert,
    pub services: Vec<GattService>,
    /// Initial characteristic values.
    pub values: Vec<(Uuid, Vec<u8>)>,
    /// Notify this characteristic with a heart-rate measurement every period while subscribed.
    pub tick: Option<(Uuid, Uuid, Duration)>,
    /// Writes to the first characteristic are notified on the second (a UART echo).
    pub echo: Option<(Uuid, Uuid)>,
    pub connectable: bool,
}

pub const UART_SERVICE: &str = "6e400001-b5a3-f393-e0a9-e50e24dcca9e";
pub const UART_RX: &str = "6e400002-b5a3-f393-e0a9-e50e24dcca9e";
pub const UART_TX: &str = "6e400003-b5a3-f393-e0a9-e50e24dcca9e";

impl MockPeripheral {
    /// A heart-rate monitor: Heart Rate (180d: 2a37 notify, 2a38 read, 2a39
    /// write), Battery (180f: 2a19 read/notify), Generic Access (1800: 2a00
    /// read) and a Nordic UART service whose RX writes are echoed on TX.
    pub fn heart_rate(id: &str, name: &str, rssi: i16) -> Self {
        let c = |s: &str, p: u8| GattChar { uuid: u(s), properties: p };
        let services = vec![
            GattService { uuid: u("1800"), characteristics: vec![c("2a00", props::READ)] },
            GattService { uuid: u("180d"), characteristics: vec![c("2a37", props::NOTIFY), c("2a38", props::READ), c("2a39", props::WRITE)] },
            GattService { uuid: u("180f"), characteristics: vec![c("2a19", props::READ | props::NOTIFY)] },
            GattService { uuid: u(UART_SERVICE), characteristics: vec![c(UART_RX, props::WRITE | props::WRITE_WITHOUT_RESPONSE), c(UART_TX, props::NOTIFY)] },
        ];
        MockPeripheral {
            advert: BleAdvert {
                id: id.into(),
                name: Some(name.into()),
                rssi: Some(rssi),
                services: vec![u("180d"), u("180f")],
                manufacturer_data: vec![(0xffff, vec![1, 2, 3])],
                tx_power: Some(4),
                connectable: Some(true),
                ..Default::default()
            },
            services,
            values: vec![(u("2a00"), name.as_bytes().to_vec()), (u("2a38"), vec![1]), (u("2a19"), vec![87])],
            tick: Some((u("180d"), u("2a37"), Duration::from_millis(40))),
            echo: Some((u(UART_RX), u(UART_TX))),
            connectable: true,
        }
    }

    /// A beacon: advertises, cannot be connected.
    pub fn beacon(id: &str, rssi: i16) -> Self {
        MockPeripheral {
            advert: BleAdvert { id: id.into(), rssi: Some(rssi), manufacturer_data: vec![(0x004c, vec![2, 21])], connectable: Some(false), ..Default::default() },
            services: Vec::new(),
            values: Vec::new(),
            tick: None,
            echo: None,
            connectable: false,
        }
    }
}

struct BleState {
    devices: Mutex<Vec<MockPeripheral>>,
    values: Mutex<HashMap<(String, Uuid), Vec<u8>>>,
    powered: AtomicBool,
    links: Mutex<Vec<Weak<MockLink>>>,
    adv_interval: Duration,
    connects: AtomicU64,
}

/// A simulated Bluetooth LE adapter.
#[derive(Clone)]
pub struct MockBle {
    st: Arc<BleState>,
}

impl MockBle {
    pub fn new(devices: Vec<MockPeripheral>) -> Self {
        let mut values = HashMap::new();
        for d in &devices {
            for (c, v) in &d.values {
                values.insert((d.advert.id.clone(), *c), v.clone());
            }
        }
        MockBle {
            st: Arc::new(BleState {
                devices: Mutex::new(devices),
                values: Mutex::new(values),
                powered: AtomicBool::new(true),
                links: Mutex::new(Vec::new()),
                adv_interval: Duration::from_millis(30),
                connects: AtomicU64::new(0),
            }),
        }
    }

    /// Switch the radio: off makes scans fail and links drop.
    pub fn set_powered(&self, on: bool) {
        self.st.powered.store(on, Ordering::Relaxed);
        if !on {
            for l in self.live_links(None) {
                l.lose("Bluetooth was switched off");
            }
        }
    }

    /// Simulate the device going out of range.
    pub fn drop_link(&self, id: &str) -> usize {
        let links = self.live_links(Some(id));
        for l in &links {
            l.lose("the device went out of range");
        }
        links.len()
    }

    /// A characteristic's value as last written (or initially set).
    pub fn value(&self, id: &str, characteristic: Uuid) -> Option<Vec<u8>> {
        lock(&self.st.values).get(&(id.to_string(), characteristic)).cloned()
    }

    /// Successful connections so far.
    pub fn connects(&self) -> u64 {
        self.st.connects.load(Ordering::Relaxed)
    }

    /// Links currently up.
    pub fn open_links(&self) -> usize {
        self.live_links(None).len()
    }

    fn live_links(&self, id: Option<&str>) -> Vec<Arc<MockLink>> {
        let mut links = lock(&self.st.links);
        links.retain(|w| w.strong_count() > 0);
        links.iter().filter_map(Weak::upgrade).filter(|l| l.alive.load(Ordering::Relaxed) && id.is_none_or(|i| l.id == i)).collect()
    }
}

struct ScanGuard(Arc<AtomicBool>);

impl Drop for ScanGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl BleBackend for MockBle {
    fn scan(&self, _services: &[Uuid], events: Emit<ScanEvent>) -> Result<Box<dyn Send>, String> {
        if !self.st.powered.load(Ordering::Relaxed) {
            return Err("Bluetooth is off".into());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (st, s) = (self.st.clone(), stop.clone());
        std::thread::Builder::new()
            .name("mock.ble.scan".into())
            .spawn(move || {
                let mut n = 0i16;
                while !s.load(Ordering::Relaxed) {
                    if !st.powered.load(Ordering::Relaxed) {
                        events(ScanEvent::Failed("Bluetooth was switched off".into()));
                        break;
                    }
                    for d in lock(&st.devices).iter() {
                        let mut a = d.advert.clone();
                        a.rssi = a.rssi.map(|r| r - (n % 3));
                        events(ScanEvent::Seen(a));
                    }
                    n = n.wrapping_add(1);
                    std::thread::sleep(st.adv_interval);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Box::new(ScanGuard(stop)))
    }

    fn connect(&self, id: &str, events: Emit<LinkEvent>, wait: &Wait) -> Result<Arc<dyn BleConnection>, String> {
        if !self.st.powered.load(Ordering::Relaxed) {
            return Err("Bluetooth is off".into());
        }
        let dev = lock(&self.st.devices).iter().find(|d| d.advert.id == id).cloned();
        let Some(dev) = dev else {
            // a real stack keeps trying until its timeout
            while wait.check("connect").is_ok() {
                std::thread::sleep(SLICE);
            }
            return Err(format!("connect to {id}: {}", if wait.cancelled() { "cancelled".to_string() } else { format!("the device did not answer within {} ms", wait.timeout().as_millis()) }));
        };
        if !dev.connectable {
            return Err(format!("{id} does not accept connections"));
        }
        std::thread::sleep(Duration::from_millis(5));
        wait.check("connect")?;
        let link = Arc::new(MockLink {
            st: self.st.clone(),
            id: id.to_string(),
            dev,
            events,
            alive: AtomicBool::new(true),
            subscribed: Mutex::new(HashSet::new()),
        });
        if let Some((_, chr, period)) = link.dev.tick {
            let weak = Arc::downgrade(&link);
            std::thread::Builder::new()
                .name("mock.ble.tick".into())
                .spawn(move || {
                    let mut n = 0u32;
                    loop {
                        std::thread::sleep(period);
                        let Some(l) = weak.upgrade() else { break };
                        if !l.alive.load(Ordering::Relaxed) {
                            break;
                        }
                        if lock(&l.subscribed).contains(&chr) {
                            n += 1;
                            let bpm = 60 + (n % 40) as u8;
                            l.notify(chr, vec![0x00, bpm]);
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
        }
        lock(&self.st.links).push(Arc::downgrade(&link));
        self.st.connects.fetch_add(1, Ordering::Relaxed);
        Ok(link)
    }
}

struct MockLink {
    st: Arc<BleState>,
    id: String,
    dev: MockPeripheral,
    events: Emit<LinkEvent>,
    alive: AtomicBool,
    subscribed: Mutex<HashSet<Uuid>>,
}

impl MockLink {
    fn lose(&self, why: &str) {
        if self.alive.swap(false, Ordering::Relaxed) {
            (self.events)(LinkEvent::Disconnected(why.to_string()));
        }
    }

    fn service_of(&self, chr: Uuid) -> Uuid {
        self.dev.services.iter().find(|s| s.characteristics.iter().any(|c| c.uuid == chr)).map(|s| s.uuid).unwrap_or_default()
    }

    fn notify(&self, chr: Uuid, data: Vec<u8>) {
        (self.events)(LinkEvent::Notification { service: self.service_of(chr), characteristic: chr, data });
    }

    fn check(&self, service: Uuid, chr: Uuid, need: u8) -> Result<(), String> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err("not connected".into());
        }
        let c = self
            .dev
            .services
            .iter()
            .filter(|s| s.uuid == service)
            .flat_map(|s| s.characteristics.iter())
            .find(|c| c.uuid == chr)
            .ok_or_else(|| format!("no characteristic {chr} in service {service}"))?;
        if c.properties & need == 0 {
            return Err(format!("operation not permitted on {chr}"));
        }
        Ok(())
    }
}

impl BleConnection for MockLink {
    fn services(&self) -> Vec<GattService> {
        self.dev.services.clone()
    }

    fn read(&self, service: Uuid, chr: Uuid, wait: &Wait) -> Result<Vec<u8>, String> {
        self.check(service, chr, props::READ)?;
        wait.check("read")?;
        Ok(lock(&self.st.values).get(&(self.id.clone(), chr)).cloned().unwrap_or_default())
    }

    fn write(&self, service: Uuid, chr: Uuid, data: &[u8], with_response: bool, _wait: &Wait) -> Result<(), String> {
        self.check(service, chr, if with_response { props::WRITE } else { props::WRITE_WITHOUT_RESPONSE })?;
        lock(&self.st.values).insert((self.id.clone(), chr), data.to_vec());
        if let Some((rx, tx)) = self.dev.echo {
            if rx == chr && lock(&self.subscribed).contains(&tx) {
                self.notify(tx, data.to_vec());
            }
        }
        Ok(())
    }

    fn subscribe(&self, service: Uuid, chr: Uuid, _wait: &Wait) -> Result<(), String> {
        self.check(service, chr, props::NOTIFY | props::INDICATE)?;
        lock(&self.subscribed).insert(chr);
        Ok(())
    }

    fn unsubscribe(&self, _service: Uuid, chr: Uuid, _wait: &Wait) -> Result<(), String> {
        lock(&self.subscribed).remove(&chr);
        Ok(())
    }

    fn connected(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn disconnect(&self) {
        self.alive.store(false, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- USB

/// A simulated USB device.
#[derive(Clone, Debug)]
pub struct MockUsbDevice {
    pub info: UsbDeviceInfo,
    /// Data written to the first (OUT) endpoint comes back on the second (IN).
    pub loopback: Vec<(u8, u8)>,
    /// Claiming fails with this message (a device bound to another driver).
    pub claim_error: Option<String>,
    /// Opening fails: the user refused access.
    pub deny: bool,
}

fn ep(address: u8, kind: EpKind, max_packet: u16) -> UsbEndpointInfo {
    UsbEndpointInfo { address, kind, max_packet }
}

impl MockUsbDevice {
    /// Vendor-specific interface 0: bulk OUT 0x01 looped to bulk IN 0x81,
    /// interrupt IN 0x82; vendor request 0x01 (OUT) stores data that
    /// request 0x02 (IN) returns.
    pub fn vendor(id: &str) -> Self {
        MockUsbDevice {
            info: UsbDeviceInfo {
                id: id.into(),
                vid: 0x1209,
                pid: 0x0001,
                manufacturer: Some("AnyPage".into()),
                product: Some("Loopback".into()),
                serial: Some("L1".into()),
                bus: "1".into(),
                address: 4,
                class: 0xff,
                speed: Some("high"),
                driver: Some("WinUSB".into()),
                interfaces: vec![UsbInterfaceInfo {
                    number: 0,
                    class: 0xff,
                    endpoints: vec![ep(0x01, EpKind::Bulk, 512), ep(0x81, EpKind::Bulk, 512), ep(0x82, EpKind::Interrupt, 8)],
                    ..Default::default()
                }],
                ..Default::default()
            },
            loopback: vec![(0x01, 0x81)],
            claim_error: None,
            deny: false,
        }
    }

    /// A CDC-ACM serial adapter whose data endpoints loop back.
    pub fn cdc_acm(id: &str) -> Self {
        MockUsbDevice {
            info: UsbDeviceInfo {
                id: id.into(),
                vid: 0x2341,
                pid: 0x0043,
                manufacturer: Some("Arduino".into()),
                product: Some("Uno".into()),
                bus: "1".into(),
                address: 5,
                class: CLASS_COMM,
                speed: Some("full"),
                interfaces: vec![
                    UsbInterfaceInfo { number: 0, class: CLASS_COMM, subclass: SUBCLASS_ACM, protocol: 1, endpoints: vec![ep(0x83, EpKind::Interrupt, 8)], ..Default::default() },
                    UsbInterfaceInfo { number: 1, class: CLASS_DATA, endpoints: vec![ep(0x02, EpKind::Bulk, 64), ep(0x82, EpKind::Bulk, 64)], ..Default::default() },
                ],
                ..Default::default()
            },
            loopback: vec![(0x02, 0x82)],
            claim_error: None,
            deny: false,
        }
    }

    /// A keyboard owned by the system's HID driver: listed, not claimable.
    pub fn keyboard(id: &str) -> Self {
        MockUsbDevice {
            info: UsbDeviceInfo {
                id: id.into(),
                vid: 0x046d,
                pid: 0xc31c,
                product: Some("Keyboard".into()),
                bus: "1".into(),
                address: 2,
                speed: Some("low"),
                driver: Some("HidUsb".into()),
                interfaces: vec![UsbInterfaceInfo { number: 0, class: 3, subclass: 1, protocol: 1, endpoints: vec![ep(0x81, EpKind::Interrupt, 8)], ..Default::default() }],
                ..Default::default()
            },
            loopback: Vec::new(),
            claim_error: Some("the device is bound to driver HidUsb, not WinUSB".into()),
            deny: false,
        }
    }
}

#[derive(Default)]
struct UsbDevState {
    present: bool,
    stored: Vec<u8>,
    line_coding: Option<Vec<u8>>,
    control_lines: Option<u16>,
    queues: HashMap<u8, VecDeque<Vec<u8>>>,
    opens: u32,
    /// Handles open now.
    live: u32,
}

struct UsbState {
    devices: Vec<MockUsbDevice>,
    state: Mutex<HashMap<String, UsbDevState>>,
    cv: Condvar,
}

/// A simulated USB host.
#[derive(Clone)]
pub struct MockUsb {
    st: Arc<UsbState>,
}

impl MockUsb {
    pub fn new(devices: Vec<MockUsbDevice>) -> Self {
        let state = devices.iter().map(|d| (d.info.id.clone(), UsbDevState { present: true, ..Default::default() })).collect();
        MockUsb { st: Arc::new(UsbState { devices, state: Mutex::new(state), cv: Condvar::new() }) }
    }

    /// Pull the device out (`false`) or plug it back in.
    pub fn set_present(&self, id: &str, present: bool) {
        if let Some(s) = lock(&self.st.state).get_mut(id) {
            s.present = present;
            s.queues.clear();
        }
        self.st.cv.notify_all();
    }

    /// Data the device sends on an IN endpoint.
    pub fn feed(&self, id: &str, endpoint: u8, data: &[u8]) {
        if let Some(s) = lock(&self.st.state).get_mut(id) {
            s.queues.entry(endpoint).or_default().push_back(data.to_vec());
        }
        self.st.cv.notify_all();
    }

    /// The last SET_LINE_CODING a CDC device received.
    pub fn line_coding(&self, id: &str) -> Option<Vec<u8>> {
        lock(&self.st.state).get(id).and_then(|s| s.line_coding.clone())
    }

    /// The last SET_CONTROL_LINE_STATE value (bit 0 DTR, bit 1 RTS).
    pub fn control_lines(&self, id: &str) -> Option<u16> {
        lock(&self.st.state).get(id).and_then(|s| s.control_lines)
    }

    /// How many times the device was opened.
    pub fn opens(&self, id: &str) -> u32 {
        lock(&self.st.state).get(id).map(|s| s.opens).unwrap_or(0)
    }

    /// How many handles to the device are open now (0: released).
    pub fn handles(&self, id: &str) -> u32 {
        lock(&self.st.state).get(id).map(|s| s.live).unwrap_or(0)
    }

    fn device(&self, id: &str) -> Option<&MockUsbDevice> {
        self.st.devices.iter().find(|d| d.info.id == id)
    }
}

impl UsbBackend for MockUsb {
    fn devices(&self) -> Result<Vec<UsbDeviceInfo>, String> {
        let state = lock(&self.st.state);
        Ok(self.st.devices.iter().filter(|d| state.get(&d.info.id).is_some_and(|s| s.present)).map(|d| d.info.clone()).collect())
    }

    fn open(&self, id: &str, _wait: &Wait) -> Result<Arc<dyn UsbHandle>, String> {
        let dev = self.device(id).ok_or_else(|| format!("no USB device {id}"))?.clone();
        let mut state = lock(&self.st.state);
        let s = state.get_mut(id).filter(|s| s.present).ok_or_else(|| format!("no USB device {id} (it was unplugged)"))?;
        if dev.deny {
            return Err(format!("permission to use USB device {id} was denied"));
        }
        s.opens += 1;
        s.live += 1;
        Ok(Arc::new(MockUsbHandle { usb: self.clone(), dev, claimed: Mutex::new(HashSet::new()) }))
    }
}

struct MockUsbHandle {
    usb: MockUsb,
    dev: MockUsbDevice,
    claimed: Mutex<HashSet<u8>>,
}

impl Drop for MockUsbHandle {
    fn drop(&mut self) {
        if let Some(s) = lock(&self.usb.st.state).get_mut(&self.dev.info.id) {
            s.live = s.live.saturating_sub(1);
        }
    }
}

impl MockUsbHandle {
    fn present(&self) -> Result<(), String> {
        match lock(&self.usb.st.state).get(&self.dev.info.id) {
            Some(s) if s.present => Ok(()),
            _ => Err("device disconnected".into()),
        }
    }

    fn claimed(&self, interface: u8) -> Result<(), String> {
        self.present()?;
        if lock(&self.claimed).contains(&interface) {
            Ok(())
        } else {
            Err(format!("interface {interface} is not claimed"))
        }
    }

    fn device_descriptor(&self) -> Vec<u8> {
        let i = &self.dev.info;
        let [vl, vh] = i.vid.to_le_bytes();
        let [pl, ph] = i.pid.to_le_bytes();
        vec![18, 1, 0x00, 0x02, i.class, i.subclass, i.protocol, 64, vl, vh, pl, ph, 0x00, 0x01, 1, 2, 3, 1]
    }
}

impl UsbHandle for MockUsbHandle {
    fn interfaces(&self) -> Vec<UsbInterfaceInfo> {
        self.dev.info.interfaces.clone()
    }

    fn claim(&self, interface: u8) -> Result<(), String> {
        self.present()?;
        if let Some(e) = &self.dev.claim_error {
            return Err(format!("cannot claim interface {interface}: {e}"));
        }
        if !self.dev.info.interfaces.iter().any(|i| i.number == interface) {
            return Err(format!("the device has no interface {interface}"));
        }
        lock(&self.claimed).insert(interface);
        Ok(())
    }

    fn release(&self, interface: u8) {
        lock(&self.claimed).remove(&interface);
    }

    fn control_in(&self, setup: &ControlSetup, length: u16, _interface: Option<u8>, _wait: &Wait) -> Result<Vec<u8>, String> {
        self.present()?;
        let mut st = lock(&self.usb.st.state);
        let s = st.get_mut(&self.dev.info.id).ok_or("device disconnected")?;
        let mut data = match (setup.kind, setup.request) {
            (ControlKind::Standard, 6) if setup.value == 0x0100 => self.device_descriptor(),
            (ControlKind::Vendor, 0x02) => s.stored.clone(),
            (ControlKind::Class, GET_LINE_CODING) => s.line_coding.clone().ok_or("endpoint stalled")?,
            _ => return Err("endpoint stalled".into()),
        };
        data.truncate(length as usize);
        Ok(data)
    }

    fn control_out(&self, setup: &ControlSetup, data: &[u8], _interface: Option<u8>, _wait: &Wait) -> Result<usize, String> {
        self.present()?;
        let mut st = lock(&self.usb.st.state);
        let s = st.get_mut(&self.dev.info.id).ok_or("device disconnected")?;
        match (setup.kind, setup.request) {
            (ControlKind::Vendor, 0x01) => s.stored = data.to_vec(),
            (ControlKind::Class, SET_LINE_CODING) if data.len() == 7 => s.line_coding = Some(data.to_vec()),
            (ControlKind::Class, SET_CONTROL_LINE_STATE) => s.control_lines = Some(setup.value),
            _ => return Err("endpoint stalled".into()),
        }
        Ok(data.len())
    }

    fn read(&self, interface: u8, endpoint: u8, length: usize, wait: &Wait) -> Result<Option<Vec<u8>>, String> {
        self.claimed(interface)?;
        let id = &self.dev.info.id;
        let mut st = lock(&self.usb.st.state);
        loop {
            let s = st.get_mut(id).ok_or("device disconnected")?;
            if !s.present {
                return Err("device disconnected".into());
            }
            if let Some(q) = s.queues.get_mut(&endpoint) {
                if let Some(mut chunk) = q.pop_front() {
                    if chunk.len() > length {
                        let rest = chunk.split_off(length);
                        q.push_front(rest);
                    }
                    return Ok(Some(chunk));
                }
            }
            if wait.cancelled() || wait.remaining().is_zero() {
                return Ok(None);
            }
            st = self.usb.st.cv.wait_timeout(st, wait.remaining().min(SLICE)).map(|(g, _)| g).unwrap_or_else(|p| p.into_inner().0);
        }
    }

    fn write(&self, interface: u8, endpoint: u8, data: &[u8], _wait: &Wait) -> Result<usize, String> {
        self.claimed(interface)?;
        if let Some((_, back)) = self.dev.loopback.iter().find(|(out, _)| *out == endpoint) {
            self.usb.feed(&self.dev.info.id, *back, data);
        }
        Ok(data.len())
    }
}

// ---------------------------------------------------------------- serial

#[derive(Default)]
struct PortState {
    present: bool,
    open: bool,
    loopback: bool,
    /// Flow control holds: writes take nothing.
    stalled: bool,
    /// Opening waits (like a permission dialog nobody answers).
    hold_open: bool,
    buf: VecDeque<u8>,
    written: Vec<u8>,
    settings: Option<SerialSettings>,
}

struct SerialState {
    ports: Mutex<HashMap<String, PortState>>,
    cv: Condvar,
}

/// Simulated serial ports. Loopback ports read back what is written.
#[derive(Clone)]
pub struct MockSerial {
    st: Arc<SerialState>,
}

impl MockSerial {
    /// `(path, loopback)` pairs.
    pub fn new(ports: &[(&str, bool)]) -> Self {
        let ports = ports.iter().map(|(p, l)| (p.to_string(), PortState { present: true, loopback: *l, ..Default::default() })).collect();
        MockSerial { st: Arc::new(SerialState { ports: Mutex::new(ports), cv: Condvar::new() }) }
    }

    /// Bytes the device sends.
    pub fn inject(&self, path: &str, data: &[u8]) {
        if let Some(p) = lock(&self.st.ports).get_mut(path) {
            p.buf.extend(data.iter().copied());
        }
        self.st.cv.notify_all();
    }

    /// Unplug (`false`) or plug the adapter back in.
    pub fn set_present(&self, path: &str, present: bool) {
        if let Some(p) = lock(&self.st.ports).get_mut(path) {
            p.present = present;
            if !present {
                p.open = false;
                p.buf.clear();
            }
        }
        self.st.cv.notify_all();
    }

    /// The device holds flow control (`true`): writes take nothing until released.
    pub fn set_stalled(&self, path: &str, stalled: bool) {
        if let Some(p) = lock(&self.st.ports).get_mut(path) {
            p.stalled = stalled;
        }
        self.st.cv.notify_all();
    }

    /// Opening the port waits (`true`) until released or the open gives up,
    /// like Android's USB permission dialog left unanswered.
    pub fn hold_open(&self, path: &str, hold: bool) {
        if let Some(p) = lock(&self.st.ports).get_mut(path) {
            p.hold_open = hold;
        }
        self.st.cv.notify_all();
    }

    /// Everything written to the port so far.
    pub fn written(&self, path: &str) -> Vec<u8> {
        lock(&self.st.ports).get(path).map(|p| p.written.clone()).unwrap_or_default()
    }

    pub fn settings(&self, path: &str) -> Option<SerialSettings> {
        lock(&self.st.ports).get(path).and_then(|p| p.settings)
    }

    pub fn is_open(&self, path: &str) -> bool {
        lock(&self.st.ports).get(path).is_some_and(|p| p.open)
    }
}

impl SerialBackend for MockSerial {
    fn ports(&self) -> Result<Vec<SerialPortInfo>, String> {
        let ports = lock(&self.st.ports);
        let mut v: Vec<SerialPortInfo> = ports
            .iter()
            .filter(|(_, p)| p.present)
            .map(|(path, _)| SerialPortInfo { path: path.clone(), kind: "usb", vid: Some(0x1209), pid: Some(0x0002), product: Some("Mock serial".into()), ..Default::default() })
            .collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(v)
    }

    fn open(&self, path: &str, settings: &SerialSettings, wait: &Wait) -> Result<Arc<dyn SerialConn>, String> {
        let mut ports = lock(&self.st.ports);
        while ports.get(path).is_some_and(|p| p.present && p.hold_open) {
            wait.check(&format!("{path}: waiting for access"))?;
            ports = self.st.cv.wait_timeout(ports, wait.remaining().min(SLICE)).map(|(g, _)| g).unwrap_or_else(|p| p.into_inner().0);
        }
        let p = ports.get_mut(path).filter(|p| p.present).ok_or_else(|| format!("{path}: the port does not exist"))?;
        if p.open {
            return Err(format!("{path}: the port is in use by another program"));
        }
        p.open = true;
        p.settings = Some(*settings);
        Ok(Arc::new(MockConn { st: self.st.clone(), path: path.to_string() }))
    }
}

struct MockConn {
    st: Arc<SerialState>,
    path: String,
}

impl SerialConn for MockConn {
    fn read(&self, max: usize, timeout: Duration) -> Result<Vec<u8>, String> {
        let deadline = Instant::now() + timeout;
        let mut ports = lock(&self.st.ports);
        loop {
            let p = ports.get_mut(&self.path).ok_or("the port is gone")?;
            if !p.present {
                return Err(format!("{}: the device was unplugged", self.path));
            }
            if !p.buf.is_empty() {
                let n = p.buf.len().min(max);
                return Ok(p.buf.drain(..n).collect());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Vec::new());
            }
            ports = self.st.cv.wait_timeout(ports, left).map(|(g, _)| g).unwrap_or_else(|p| p.into_inner().0);
        }
    }

    fn write(&self, data: &[u8], wait: &Wait) -> Result<usize, String> {
        let mut ports = lock(&self.st.ports);
        loop {
            let p = ports.get_mut(&self.path).ok_or("the port is gone")?;
            if !p.present {
                return Err(format!("{}: the device was unplugged", self.path));
            }
            if !p.stalled {
                p.written.extend_from_slice(data);
                if p.loopback {
                    p.buf.extend(data.iter().copied());
                }
                drop(ports);
                self.st.cv.notify_all();
                return Ok(data.len());
            }
            if wait.cancelled() || wait.remaining().is_zero() {
                return Ok(0);
            }
            ports = self.st.cv.wait_timeout(ports, wait.remaining().min(SLICE)).map(|(g, _)| g).unwrap_or_else(|p| p.into_inner().0);
        }
    }
}

impl Drop for MockConn {
    fn drop(&mut self) {
        if let Some(p) = lock(&self.st.ports).get_mut(&self.path) {
            p.open = false;
        }
    }
}

// ---------------------------------------------------------------- a full set

/// One of each, ready to register: `register_with(&mut n, mocks.backends())`.
#[derive(Clone)]
pub struct Mocks {
    pub ble: MockBle,
    pub usb: MockUsb,
    pub serial: MockSerial,
}

impl Mocks {
    /// A heart-rate monitor "C0:FF:EE:00:00:01" and a beacon; a vendor
    /// loopback USB device "usb-loop", a CDC-ACM adapter "usb-acm" and a
    /// keyboard "usb-kbd"; loopback serial ports "MOCK0" and "MOCK1".
    pub fn standard() -> Self {
        Mocks {
            ble: MockBle::new(vec![MockPeripheral::heart_rate("C0:FF:EE:00:00:01", "AnyPage HR", -58), MockPeripheral::beacon("C0:FF:EE:00:00:02", -80)]),
            usb: MockUsb::new(vec![MockUsbDevice::vendor("usb-loop"), MockUsbDevice::cdc_acm("usb-acm"), MockUsbDevice::keyboard("usb-kbd")]),
            serial: MockSerial::new(&[("MOCK0", true), ("MOCK1", true)]),
        }
    }

    pub fn backends(&self) -> Backends {
        Backends { ble: Some(Arc::new(self.ble.clone())), usb: Some(Arc::new(self.usb.clone())), serial: Some(Arc::new(self.serial.clone())) }
    }
}
