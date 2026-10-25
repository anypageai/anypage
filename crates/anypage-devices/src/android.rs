//! Android: the device bridge the Kotlin host implements over JNI, and the
//! backends built on it.
//!
//! Android's Bluetooth and USB APIs are Java-only and callback driven, so
//! the host implements [`AndroidDevices`] (Rust calls into Kotlin through
//! JNI) and reports results back through the `on_*` callbacks (Kotlin calls
//! into Rust through JNI, which calls [`AndroidHost`] methods or the free
//! functions below). This module turns that into the blocking, cancellable
//! [`BleBackend`] / [`UsbBackend`] the module logic uses:
//!
//! * BLE: one GATT operation at a time per connection (Android drops a
//!   second one while the first is outstanding), operations matched to
//!   their callbacks by id, timeouts, scans shared through the scan hub.
//! * USB: Kotlin lists devices, asks the user for permission and opens the
//!   device; Rust then does the transfers itself on the file descriptor
//!   (`UsbDeviceConnection.getFileDescriptor()`) with nusb's usbfs backend.
//!   Serial ports are CDC-ACM devices driven over that ([`crate::serial::cdc`]).
//!
//! # What the Kotlin host does
//!
//! | bridge method | Android API |
//! |---|---|
//! | `ble_start_scan(services)` | `BluetoothLeScanner.startScan(filters, settings, callback)`; one `ScanFilter` per service UUID, no filter when empty; `SCAN_MODE_LOW_LATENCY` |
//! | `ble_stop_scan()` | `stopScan(callback)` |
//! | `ble_connect(link, address)` | `adapter.getRemoteDevice(address).connectGatt(ctx, false, cb, TRANSPORT_LE)`; keep `link -> BluetoothGatt`; `requestMtu(517)` after connecting is recommended |
//! | `ble_discover_services(link)` | `gatt.discoverServices()` |
//! | `ble_read(link, op, s, c)` | `gatt.readCharacteristic(c)` |
//! | `ble_write(link, op, s, c, data, response)` | `gatt.writeCharacteristic(c, data, WRITE_TYPE_DEFAULT or WRITE_TYPE_NO_RESPONSE)` |
//! | `ble_set_notify(link, op, s, c, on)` | `setCharacteristicNotification(c, on)` then write descriptor 0x2902 with `ENABLE_NOTIFICATION_VALUE`, `ENABLE_INDICATION_VALUE` (indicate-only characteristics) or `DISABLE_NOTIFICATION_VALUE` |
//! | `ble_disconnect(link)` | `gatt.disconnect(); gatt.close()` |
//! | `usb_devices()` | `UsbManager.getDeviceList()` (id = `UsbDevice.getDeviceName()`) |
//! | `usb_has_permission(id)` | `UsbManager.hasPermission(device)` |
//! | `usb_request_permission(id)` | `UsbManager.requestPermission(device, pendingIntent)` |
//! | `usb_open(id)` | `UsbManager.openDevice(device).getFileDescriptor()`; keep the connection open |
//! | `usb_close(fd)` | `UsbDeviceConnection.close()` of the connection with that descriptor |
//!
//! | callback | from |
//! |---|---|
//! | `on_scan_result` | `ScanCallback.onScanResult` / `onBatchScanResults` |
//! | `on_scan_failed(code)` | `ScanCallback.onScanFailed` |
//! | `on_connection_state(link, connected, status)` | `BluetoothGattCallback.onConnectionStateChange` |
//! | `on_services_discovered(link, status, services)` | `onServicesDiscovered` (with `gatt.getServices()`) |
//! | `on_op_complete(op, status, value)` | `onCharacteristicRead`, `onCharacteristicWrite` (also for writes without response), `onDescriptorWrite` (notify on/off) |
//! | `on_notification(link, s, c, value)` | `onCharacteristicChanged` |
//! | `on_usb_permission(id, granted)` | the permission `BroadcastReceiver` |
//! | `on_usb_detached(id)` | `ACTION_USB_DEVICE_DETACHED` |
//!
//! The app needs `BLUETOOTH_SCAN` (with `neverForLocation`) and
//! `BLUETOOTH_CONNECT` on Android 12+, `ACCESS_FINE_LOCATION` before, and
//! `<uses-feature android:name="android.hardware.usb.host"/>`. The host asks
//! for runtime permissions; a bridge method returns `Err("...permission...")`
//! when they are missing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::ble::hub::{Radio, ScanHub};
use crate::ble::{parse_uuid, uuid_string, BleAdvert, BleBackend, BleConnection, Emit, GattChar, GattService, LinkEvent, ScanEvent};
use crate::serial::cdc::CdcSerial;
use crate::usb::{ControlSetup, UsbBackend, UsbDeviceInfo, UsbHandle, UsbInterfaceInfo};
use crate::util::{clip, lock, Wait};
use crate::Backends;

/// What the Android host implements (in Kotlin, reached through JNI).
///
/// Methods return quickly; results arrive through the [`AndroidHost`]
/// callbacks. `Err` means the call could not even start (no adapter,
/// permission missing, Bluetooth off, unknown id), with a message for the page.
pub trait AndroidDevices: Send + Sync + 'static {
    /// Start the one BLE scan; `services` are full UUID strings, empty for all devices.
    fn ble_start_scan(&self, services: &[String]) -> Result<(), String>;
    fn ble_stop_scan(&self);
    /// Connect to `address`; report with `on_connection_state(link, ...)`.
    fn ble_connect(&self, link: u64, address: &str) -> Result<(), String>;
    /// Report with `on_services_discovered(link, ...)`.
    fn ble_discover_services(&self, link: u64) -> Result<(), String>;
    /// Report with `on_op_complete(op, status, value)`.
    fn ble_read(&self, link: u64, op: u64, service: &str, characteristic: &str) -> Result<(), String>;
    /// Report with `on_op_complete(op, status, [])`.
    fn ble_write(&self, link: u64, op: u64, service: &str, characteristic: &str, data: &[u8], with_response: bool) -> Result<(), String>;
    /// Enable or disable notifications (or indications); report with
    /// `on_op_complete(op, status, [])` once the descriptor write finished.
    fn ble_set_notify(&self, link: u64, op: u64, service: &str, characteristic: &str, enable: bool) -> Result<(), String>;
    /// Disconnect and close; no callback needed.
    fn ble_disconnect(&self, link: u64);

    fn usb_devices(&self) -> Result<Vec<UsbDeviceInfo>, String>;
    fn usb_has_permission(&self, device: &str) -> bool;
    /// Ask the user; report with `on_usb_permission(device, granted)`.
    fn usb_request_permission(&self, device: &str) -> Result<(), String>;
    /// Open the device and return its usbfs file descriptor. The host keeps
    /// the connection (and the descriptor) open until `usb_close(fd)`.
    fn usb_open(&self, device: &str) -> Result<i32, String>;
    fn usb_close(&self, fd: i32);
}

/// An advertisement as `ScanResult` reports it.
#[derive(Clone, Debug, Default)]
pub struct AndroidScanResult {
    pub address: String,
    pub name: Option<String>,
    pub rssi: i32,
    pub service_uuids: Vec<String>,
    pub manufacturer_data: Vec<(u16, Vec<u8>)>,
    pub service_data: Vec<(String, Vec<u8>)>,
    /// `ScanRecord.getTxPowerLevel()`, `None` when absent (`Integer.MIN_VALUE`).
    pub tx_power: Option<i32>,
    /// `ScanResult.isConnectable()` (API 26+).
    pub connectable: Option<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct AndroidGattCharacteristic {
    pub uuid: String,
    /// `BluetoothGattCharacteristic.getProperties()`.
    pub properties: i32,
}

#[derive(Clone, Debug, Default)]
pub struct AndroidGattService {
    pub uuid: String,
    pub characteristics: Vec<AndroidGattCharacteristic>,
}

/// Opens a usbfs file descriptor the host handed over as a device handle.
pub type FdOpener = Arc<dyn Fn(i32, &UsbDeviceInfo) -> Result<Arc<dyn UsbHandle>, String> + Send + Sync>;

/// `BluetoothGatt` operation status codes.
pub fn gatt_status(status: i32) -> String {
    match status {
        0 => "success".into(),
        2 => "reading is not permitted (GATT_READ_NOT_PERMITTED)".into(),
        3 => "writing is not permitted (GATT_WRITE_NOT_PERMITTED)".into(),
        5 => "insufficient authentication: pair with the device first (GATT_INSUFFICIENT_AUTHENTICATION)".into(),
        6 => "the device does not support this request (GATT_REQUEST_NOT_SUPPORTED)".into(),
        7 => "invalid offset (GATT_INVALID_OFFSET)".into(),
        8 => "insufficient authorization (GATT_INSUFFICIENT_AUTHORIZATION)".into(),
        13 => "the value has the wrong length for this characteristic (GATT_INVALID_ATTRIBUTE_LENGTH)".into(),
        15 => "insufficient encryption: pair with the device first (GATT_INSUFFICIENT_ENCRYPTION)".into(),
        133 => "the Bluetooth stack reported a generic error (GATT_ERROR 133)".into(),
        143 => "the connection is congested; retry (GATT_CONNECTION_CONGESTED)".into(),
        257 => "the operation failed (GATT_FAILURE)".into(),
        other => format!("GATT status {other}"),
    }
}

/// `onConnectionStateChange` status codes (HCI reasons).
pub fn connection_status(status: i32) -> String {
    match status {
        0 => "disconnected".into(),
        8 => "the link timed out (the device went out of range?)".into(),
        19 => "the device ended the connection".into(),
        22 => "this phone ended the connection".into(),
        62 => "the connection could not be established".into(),
        133 => "the connection failed (GATT_ERROR 133; the device may be off, out of range or busy)".into(),
        other => format!("the connection ended (status {other})"),
    }
}

/// `ScanCallback.onScanFailed` codes.
pub fn scan_failure(code: i32) -> String {
    match code {
        1 => "a scan is already running (SCAN_FAILED_ALREADY_STARTED)".into(),
        2 => "the app could not register for scanning; toggle Bluetooth (SCAN_FAILED_APPLICATION_REGISTRATION_FAILED)".into(),
        3 => "internal Bluetooth error (SCAN_FAILED_INTERNAL_ERROR)".into(),
        4 => "this phone cannot scan for Bluetooth LE (SCAN_FAILED_FEATURE_UNSUPPORTED)".into(),
        5 => "out of Bluetooth scan resources (SCAN_FAILED_OUT_OF_HARDWARE_RESOURCES)".into(),
        6 => "the app scans too often; Android allows 5 scan starts per 30 seconds (SCAN_FAILED_SCANNING_TOO_FREQUENTLY)".into(),
        other => format!("scan failed (code {other})"),
    }
}

struct LinkSlot {
    events: Emit<LinkEvent>,
    alive: Arc<AtomicBool>,
    /// Waiting for the first connection state.
    connecting: Option<Sender<(bool, i32)>>,
    discovering: Option<Sender<(i32, Vec<GattService>)>>,
}

/// A USB permission question in flight for one device.
struct PermAsk {
    /// When the host last asked the user.
    asked: Instant,
    /// Everyone waiting for the answer, by waiter id.
    waiters: Vec<(u64, Sender<bool>)>,
}

/// Ask again when a question has gone this long without an answer: its
/// dialog may have been lost (an activity recreated, the app sent to the
/// background).
const PERMISSION_REASK: Duration = Duration::from_secs(30);

#[derive(Default)]
struct HostState {
    links: HashMap<u64, LinkSlot>,
    ops: HashMap<u64, Sender<(i32, Vec<u8>)>>,
    /// Permission questions in flight, by device.
    perms: HashMap<String, PermAsk>,
}

/// The Rust side of the bridge: owns the pending operations and turns
/// callbacks into results.
pub struct AndroidHost {
    bridge: Arc<dyn AndroidDevices>,
    st: Mutex<HostState>,
    hub: Arc<ScanHub>,
    next: AtomicU64,
    fd_opener: FdOpener,
}

impl AndroidHost {
    pub fn new(bridge: Arc<dyn AndroidDevices>) -> Arc<Self> {
        Self::with_fd_opener(bridge, Arc::new(open_fd))
    }

    /// With a custom way to turn a file descriptor into a device handle (tests).
    pub fn with_fd_opener(bridge: Arc<dyn AndroidDevices>, fd_opener: FdOpener) -> Arc<Self> {
        Arc::new(AndroidHost { bridge, st: Mutex::new(HostState::default()), hub: ScanHub::new(), next: AtomicU64::new(1), fd_opener })
    }

    /// Backends bound to this host.
    pub fn backends(self: &Arc<Self>) -> Backends {
        let usb: Arc<dyn UsbBackend> = Arc::new(AndroidUsb(HostRef::Fixed(self.clone())));
        Backends { ble: Some(Arc::new(AndroidBle(HostRef::Fixed(self.clone())))), serial: Some(Arc::new(CdcSerial::new(usb.clone()))), usb: Some(usb) }
    }

    fn id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    // ------------------------------------------------------------ callbacks

    pub fn on_scan_result(&self, r: AndroidScanResult) {
        let parse_all = |v: &[String]| v.iter().filter_map(|s| parse_uuid(s).ok()).collect::<Vec<Uuid>>();
        let adv = BleAdvert {
            id: r.address.to_uppercase(),
            name: r.name.filter(|n| !n.is_empty()),
            rssi: Some(r.rssi.clamp(-127, 127) as i16),
            services: parse_all(&r.service_uuids),
            manufacturer_data: r.manufacturer_data,
            service_data: r.service_data.into_iter().filter_map(|(u, d)| parse_uuid(&u).ok().map(|u| (u, d))).collect(),
            tx_power: r.tx_power.map(|t| t.clamp(-127, 127) as i16),
            connectable: r.connectable,
        };
        self.hub.deliver(&adv);
    }

    pub fn on_scan_failed(&self, code: i32) {
        self.hub.failed(&scan_failure(code));
    }

    pub fn on_connection_state(&self, link: u64, connected: bool, status: i32) {
        let mut st = lock(&self.st);
        let Some(slot) = st.links.get_mut(&link) else { return };
        if let Some(tx) = slot.connecting.take() {
            let _ = tx.send((connected, status));
            return;
        }
        if !connected && slot.alive.swap(false, Ordering::Relaxed) {
            let events = slot.events.clone();
            drop(st);
            events(LinkEvent::Disconnected(connection_status(status)));
        }
    }

    pub fn on_services_discovered(&self, link: u64, status: i32, services: Vec<AndroidGattService>) {
        let services = services
            .into_iter()
            .filter_map(|s| {
                let uuid = parse_uuid(&s.uuid).ok()?;
                let characteristics = s.characteristics.into_iter().filter_map(|c| Some(GattChar { uuid: parse_uuid(&c.uuid).ok()?, properties: c.properties as u8 })).collect();
                Some(GattService { uuid, characteristics })
            })
            .collect();
        if let Some(tx) = lock(&self.st).links.get_mut(&link).and_then(|s| s.discovering.take()) {
            let _ = tx.send((status, services));
        }
    }

    pub fn on_op_complete(&self, op: u64, status: i32, value: Vec<u8>) {
        if let Some(tx) = lock(&self.st).ops.remove(&op) {
            let _ = tx.send((status, value));
        }
    }

    pub fn on_notification(&self, link: u64, service: &str, characteristic: &str, value: Vec<u8>) {
        let (Ok(service), Ok(characteristic)) = (parse_uuid(service), parse_uuid(characteristic)) else { return };
        let events = lock(&self.st).links.get(&link).map(|s| s.events.clone());
        if let Some(e) = events {
            e(LinkEvent::Notification { service, characteristic, data: value });
        }
    }

    pub fn on_usb_permission(&self, device: &str, granted: bool) {
        let ask = lock(&self.st).perms.remove(device);
        for (_, tx) in ask.map(|a| a.waiters).unwrap_or_default() {
            let _ = tx.send(granted);
        }
    }

    pub fn on_usb_detached(&self, device: &str) {
        self.on_usb_permission(device, false);
    }

    // ------------------------------------------------------------ BLE

    fn gatt_op(&self, op_lock: &Mutex<()>, wait: &Wait, what: &str, start: impl FnOnce(u64) -> Result<(), String>) -> Result<Vec<u8>, String> {
        let _one_at_a_time = lock(op_lock);
        let op = self.id();
        let (tx, rx) = channel();
        lock(&self.st).ops.insert(op, tx);
        if let Err(e) = start(op) {
            lock(&self.st).ops.remove(&op);
            return Err(format!("{what}: {e}"));
        }
        let r = wait.recv(&rx, what);
        lock(&self.st).ops.remove(&op);
        let (status, value) = r?;
        if status != 0 {
            return Err(format!("{what}: {}", gatt_status(status)));
        }
        Ok(value)
    }

    fn connect(self: &Arc<Self>, address: &str, events: Emit<LinkEvent>, wait: &Wait) -> Result<Arc<dyn BleConnection>, String> {
        // BluetoothAdapter.getRemoteDevice only accepts upper-case addresses
        let address = &address.to_uppercase();
        let link = self.id();
        let alive = Arc::new(AtomicBool::new(false));
        let (ctx, crx) = channel();
        lock(&self.st).links.insert(link, LinkSlot { events, alive: alive.clone(), connecting: Some(ctx), discovering: None });
        let fail = |e: String| {
            lock(&self.st).links.remove(&link);
            self.bridge.ble_disconnect(link);
            Err(e)
        };
        if let Err(e) = self.bridge.ble_connect(link, address) {
            lock(&self.st).links.remove(&link);
            return Err(format!("connect to {address}: {e}"));
        }
        match wait.recv(&crx, &format!("connect to {address}")) {
            Ok((true, _)) => {}
            Ok((false, status)) => return fail(format!("connect to {address}: {}", connection_status(status))),
            Err(e) => return fail(e),
        }
        alive.store(true, Ordering::Relaxed);
        let (dtx, drx) = channel();
        if let Some(slot) = lock(&self.st).links.get_mut(&link) {
            slot.discovering = Some(dtx);
        }
        if let Err(e) = self.bridge.ble_discover_services(link) {
            return fail(format!("discovering services: {e}"));
        }
        let services = match wait.recv(&drx, "discovering services") {
            Ok((0, s)) => s,
            Ok((status, _)) => return fail(format!("discovering services: {}", gatt_status(status))),
            Err(e) => return fail(e),
        };
        Ok(Arc::new(AndroidLink { host: self.clone(), link, services, alive, op_lock: Mutex::new(()) }))
    }

    // ------------------------------------------------------------ USB

    /// Wait for the user to allow `id`, asking when no question is in
    /// flight (or the last one went unanswered too long). A waiter that
    /// gives up (timeout, cancel) leaves, so the next open asks again.
    fn usb_permission(&self, id: &str, wait: &Wait) -> Result<bool, String> {
        let (tx, rx) = channel();
        let me = self.id();
        let ask = {
            let mut st = lock(&self.st);
            let a = st.perms.entry(id.to_string()).or_insert_with(|| PermAsk { asked: Instant::now(), waiters: Vec::new() });
            a.waiters.push((me, tx));
            let ask = a.waiters.len() == 1 || a.asked.elapsed() >= PERMISSION_REASK;
            if ask {
                a.asked = Instant::now();
            }
            ask
        };
        if ask {
            if let Err(e) = self.bridge.usb_request_permission(id) {
                lock(&self.st).perms.remove(id);
                return Err(format!("asking for permission to use {id}: {e}"));
            }
        }
        let r = wait.recv(&rx, "waiting for the user to allow the USB device");
        if r.is_err() {
            let mut st = lock(&self.st);
            if let Some(a) = st.perms.get_mut(id) {
                a.waiters.retain(|(w, _)| *w != me);
                if a.waiters.is_empty() {
                    st.perms.remove(id);
                }
            }
        }
        r
    }

    fn usb_open(self: &Arc<Self>, id: &str, wait: &Wait) -> Result<Arc<dyn UsbHandle>, String> {
        let info = self.bridge.usb_devices()?.into_iter().find(|d| d.id == id).ok_or_else(|| format!("no USB device {} (unplugged?)", clip(id)))?;
        if !self.bridge.usb_has_permission(id) && !self.usb_permission(id, wait)? {
            return Err(format!("permission to use USB device {id} was denied"));
        }
        let fd = self.bridge.usb_open(id)?;
        match (self.fd_opener)(fd, &info) {
            Ok(inner) => Ok(Arc::new(AndroidUsbHandle { inner, host: self.clone(), fd })),
            Err(e) => {
                self.bridge.usb_close(fd);
                Err(e)
            }
        }
    }
}

/// Opens the descriptor with nusb. The host keeps its own descriptor open
/// until `usb_close`; nusb gets a duplicate it closes itself.
#[cfg(target_os = "android")]
fn open_fd(fd: i32, info: &UsbDeviceInfo) -> Result<Arc<dyn UsbHandle>, String> {
    use nusb::MaybeFuture;
    use std::os::fd::BorrowedFd;
    if fd < 0 {
        return Err(format!("the host returned an invalid file descriptor for {}", info.id));
    }
    // SAFETY: the bridge contract keeps `fd` open until `usb_close(fd)`,
    // which only runs after this handle is gone; we only duplicate it here.
    let owned = unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned().map_err(|e| format!("cannot duplicate the USB file descriptor: {e}"))?;
    let dev = nusb::Device::from_fd(owned).wait().map_err(|e| format!("cannot open USB device {}: {e}", info.id))?;
    Ok(Arc::new(crate::nusb_host::NusbHandle::new(dev, crate::nusb_host::label(info), None)))
}

#[cfg(not(target_os = "android"))]
fn open_fd(_fd: i32, _info: &UsbDeviceInfo) -> Result<Arc<dyn UsbHandle>, String> {
    Err("USB file descriptors from the Android host can only be used on Android".into())
}

struct BridgeRadio(Arc<AndroidHost>);

impl Radio for BridgeRadio {
    fn start(&self, services: &[Uuid]) -> Result<(), String> {
        let s: Vec<String> = services.iter().map(uuid_string).collect();
        self.0.bridge.ble_start_scan(&s)
    }
    fn stop(&self) {
        self.0.bridge.ble_stop_scan()
    }
}

struct AndroidLink {
    host: Arc<AndroidHost>,
    link: u64,
    services: Vec<GattService>,
    alive: Arc<AtomicBool>,
    op_lock: Mutex<()>,
}

impl AndroidLink {
    fn ids(service: Uuid, characteristic: Uuid) -> (String, String) {
        (uuid_string(&service), uuid_string(&characteristic))
    }
}

impl BleConnection for AndroidLink {
    fn services(&self) -> Vec<GattService> {
        self.services.clone()
    }

    fn read(&self, service: Uuid, characteristic: Uuid, wait: &Wait) -> Result<Vec<u8>, String> {
        let (s, c) = Self::ids(service, characteristic);
        self.host.gatt_op(&self.op_lock, wait, "read", |op| self.host.bridge.ble_read(self.link, op, &s, &c))
    }

    fn write(&self, service: Uuid, characteristic: Uuid, data: &[u8], with_response: bool, wait: &Wait) -> Result<(), String> {
        let (s, c) = Self::ids(service, characteristic);
        self.host.gatt_op(&self.op_lock, wait, "write", |op| self.host.bridge.ble_write(self.link, op, &s, &c, data, with_response)).map(|_| ())
    }

    fn subscribe(&self, service: Uuid, characteristic: Uuid, wait: &Wait) -> Result<(), String> {
        let (s, c) = Self::ids(service, characteristic);
        self.host.gatt_op(&self.op_lock, wait, "subscribe", |op| self.host.bridge.ble_set_notify(self.link, op, &s, &c, true)).map(|_| ())
    }

    fn unsubscribe(&self, service: Uuid, characteristic: Uuid, wait: &Wait) -> Result<(), String> {
        let (s, c) = Self::ids(service, characteristic);
        self.host.gatt_op(&self.op_lock, wait, "unsubscribe", |op| self.host.bridge.ble_set_notify(self.link, op, &s, &c, false)).map(|_| ())
    }

    fn connected(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn disconnect(&self) {
        self.alive.store(false, Ordering::Relaxed);
        if lock(&self.host.st).links.remove(&self.link).is_some() {
            self.host.bridge.ble_disconnect(self.link);
        }
    }
}

impl Drop for AndroidLink {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// A device handle whose descriptor the host closes when it goes.
struct AndroidUsbHandle {
    inner: Arc<dyn UsbHandle>,
    host: Arc<AndroidHost>,
    fd: i32,
}

impl Drop for AndroidUsbHandle {
    fn drop(&mut self) {
        self.host.bridge.usb_close(self.fd);
    }
}

impl UsbHandle for AndroidUsbHandle {
    fn interfaces(&self) -> Vec<UsbInterfaceInfo> {
        self.inner.interfaces()
    }
    fn claim(&self, interface: u8) -> Result<(), String> {
        self.inner.claim(interface)
    }
    fn release(&self, interface: u8) {
        self.inner.release(interface)
    }
    fn control_in(&self, setup: &ControlSetup, length: u16, interface: Option<u8>, wait: &Wait) -> Result<Vec<u8>, String> {
        self.inner.control_in(setup, length, interface, wait)
    }
    fn control_out(&self, setup: &ControlSetup, data: &[u8], interface: Option<u8>, wait: &Wait) -> Result<usize, String> {
        self.inner.control_out(setup, data, interface, wait)
    }
    fn read(&self, interface: u8, endpoint: u8, length: usize, wait: &Wait) -> Result<Option<Vec<u8>>, String> {
        self.inner.read(interface, endpoint, length, wait)
    }
    fn write(&self, interface: u8, endpoint: u8, data: &[u8], wait: &Wait) -> Result<usize, String> {
        self.inner.write(interface, endpoint, data, wait)
    }
}

// ---------------------------------------------------------------- the installed bridge

static HOST: Mutex<Option<Arc<AndroidHost>>> = Mutex::new(None);

pub(crate) const NO_BRIDGE: &str = "the Android host has not installed the device bridge (anypage_devices::android::set_android_bridge)";

/// Install the host's bridge (at startup, before or after pages load).
/// Returns the host object the JNI callbacks go to.
pub fn set_android_bridge(bridge: Arc<dyn AndroidDevices>) -> Arc<AndroidHost> {
    let host = AndroidHost::new(bridge);
    *lock(&HOST) = Some(host.clone());
    host
}

/// The installed host, for JNI callbacks.
pub fn android_host() -> Option<Arc<AndroidHost>> {
    lock(&HOST).clone()
}

/// Callbacks for JNI glue that does not keep the [`AndroidHost`]: each
/// forwards to the installed host and does nothing without one.
pub fn on_scan_result(r: AndroidScanResult) {
    if let Some(h) = android_host() {
        h.on_scan_result(r)
    }
}
pub fn on_scan_failed(code: i32) {
    if let Some(h) = android_host() {
        h.on_scan_failed(code)
    }
}
pub fn on_connection_state(link: u64, connected: bool, status: i32) {
    if let Some(h) = android_host() {
        h.on_connection_state(link, connected, status)
    }
}
pub fn on_services_discovered(link: u64, status: i32, services: Vec<AndroidGattService>) {
    if let Some(h) = android_host() {
        h.on_services_discovered(link, status, services)
    }
}
pub fn on_op_complete(op: u64, status: i32, value: Vec<u8>) {
    if let Some(h) = android_host() {
        h.on_op_complete(op, status, value)
    }
}
pub fn on_notification(link: u64, service: &str, characteristic: &str, value: Vec<u8>) {
    if let Some(h) = android_host() {
        h.on_notification(link, service, characteristic, value)
    }
}
pub fn on_usb_permission(device: &str, granted: bool) {
    if let Some(h) = android_host() {
        h.on_usb_permission(device, granted)
    }
}
pub fn on_usb_detached(device: &str) {
    if let Some(h) = android_host() {
        h.on_usb_detached(device)
    }
}

/// Which host a backend talks to: a fixed one, or whatever is installed
/// when the call happens (the app registers modules before the host may
/// have installed its bridge).
#[derive(Clone)]
enum HostRef {
    Fixed(Arc<AndroidHost>),
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    Installed,
}

impl HostRef {
    fn get(&self) -> Result<Arc<AndroidHost>, String> {
        match self {
            HostRef::Fixed(h) => Ok(h.clone()),
            HostRef::Installed => android_host().ok_or_else(|| NO_BRIDGE.to_string()),
        }
    }
}

struct AndroidBle(HostRef);

impl BleBackend for AndroidBle {
    fn scan(&self, services: &[Uuid], events: Emit<ScanEvent>) -> Result<Box<dyn Send>, String> {
        let host = self.0.get()?;
        let radio = Arc::new(BridgeRadio(host.clone()));
        host.hub.subscribe(radio, services, events)
    }

    fn connect(&self, id: &str, events: Emit<LinkEvent>, wait: &Wait) -> Result<Arc<dyn BleConnection>, String> {
        self.0.get()?.connect(id, events, wait)
    }

    fn status(&self) -> Result<(), String> {
        self.0.get().map(|_| ())
    }
}

struct AndroidUsb(HostRef);

impl UsbBackend for AndroidUsb {
    fn devices(&self) -> Result<Vec<UsbDeviceInfo>, String> {
        self.0.get()?.bridge.usb_devices()
    }

    fn open(&self, id: &str, wait: &Wait) -> Result<Arc<dyn UsbHandle>, String> {
        self.0.get()?.usb_open(id, wait)
    }

    fn status(&self) -> Result<(), String> {
        self.0.get().map(|_| ())
    }
}

/// The platform backends on Android: everything goes through the installed bridge.
#[cfg(target_os = "android")]
pub(crate) fn platform_backends() -> Backends {
    let usb: Arc<dyn UsbBackend> = Arc::new(AndroidUsb(HostRef::Installed));
    Backends { ble: Some(Arc::new(AndroidBle(HostRef::Installed))), serial: Some(Arc::new(CdcSerial::new(usb.clone()))), usb: Some(usb) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial::SerialBackend;

    #[test]
    fn status_messages() {
        assert!(gatt_status(5).contains("pair"));
        assert!(gatt_status(999).contains("999"));
        assert!(connection_status(8).contains("timed out"));
        assert!(scan_failure(6).contains("too often"));
    }

    #[test]
    fn no_bridge_is_a_clear_error() {
        let e = AndroidBle(HostRef::Installed).connect("AA:BB:CC:DD:EE:FF", Arc::new(|_| {}), &Wait::timeout_only(std::time::Duration::from_millis(10)));
        if android_host().is_none() {
            assert_eq!(e.err().unwrap(), NO_BRIDGE);
            // pages can ask before the first use
            assert_eq!(AndroidBle(HostRef::Installed).status(), Err(NO_BRIDGE.to_string()));
            let usb: Arc<dyn UsbBackend> = Arc::new(AndroidUsb(HostRef::Installed));
            assert_eq!(usb.status(), Err(NO_BRIDGE.to_string()));
            assert_eq!(CdcSerial::new(usb).status(), Err(NO_BRIDGE.to_string()));
        }
    }
}
