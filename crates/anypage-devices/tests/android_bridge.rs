//! The Android backends, driven through a fake Kotlin host that answers
//! like Android does: asynchronously, through the callbacks, one GATT
//! operation at a time.

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anypage_devices::android::{self, AndroidGattCharacteristic, AndroidGattService, AndroidScanResult, FdOpener};
use anypage_devices::mock::{MockUsb, MockUsbDevice};
use anypage_devices::usb::UsbDeviceInfo;
use anypage_devices::{register_with, AndroidDevices, AndroidHost, UsbBackend, Wait};
use anypage_engine::Page;
use anypage_runtime::Natives;
use common::*;

const ADDR: &str = "AA:BB:CC:DD:EE:01";
const DEV: &str = "/dev/bus/usb/001/005";

#[derive(Default)]
struct Fake {
    host: Mutex<Option<Weak<AndroidHost>>>,
    scanning: Arc<AtomicBool>,
    scan_starts: Mutex<Vec<Vec<String>>>,
    busy: Mutex<HashSet<u64>>,
    overlaps: AtomicUsize,
    ops: AtomicUsize,
    notifiers: Mutex<HashMap<(u64, String), Arc<AtomicBool>>>,
    connects: Mutex<Vec<(u64, String)>>,
    disconnects: Mutex<Vec<u64>>,
    written: Mutex<Vec<(String, Vec<u8>)>>,
    permitted: AtomicBool,
    answer: AtomicBool,
    /// The permission dialog never answers (lost with a recreated activity).
    silent: AtomicBool,
    perm_requests: AtomicUsize,
    next_fd: AtomicI32,
    opened: Mutex<Vec<i32>>,
    closed: Mutex<Vec<i32>>,
}

fn hr_services() -> Vec<AndroidGattService> {
    let c = |u: &str, p: i32| AndroidGattCharacteristic { uuid: u.into(), properties: p };
    vec![
        AndroidGattService { uuid: "00001800-0000-1000-8000-00805f9b34fb".into(), characteristics: vec![c("00002a00-0000-1000-8000-00805f9b34fb", 0x02)] },
        AndroidGattService {
            uuid: "0000180d-0000-1000-8000-00805f9b34fb".into(),
            characteristics: vec![c("00002a37-0000-1000-8000-00805f9b34fb", 0x10), c("00002a38-0000-1000-8000-00805f9b34fb", 0x02), c("00002a39-0000-1000-8000-00805f9b34fb", 0x08)],
        },
    ]
}

impl Fake {
    fn new() -> Arc<Self> {
        let f = Fake { next_fd: AtomicI32::new(40), ..Default::default() };
        f.answer.store(true, Ordering::Relaxed);
        Arc::new(f)
    }

    fn host(&self) -> Arc<AndroidHost> {
        self.host.lock().unwrap().as_ref().and_then(Weak::upgrade).expect("host installed")
    }

    /// Answer later on another thread, as Android's binder threads do.
    fn later(&self, ms: u64, f: impl FnOnce(&AndroidHost) + Send + 'static) {
        let h = self.host();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            f(&h);
        });
    }

    fn op_start(&self, link: u64) {
        self.ops.fetch_add(1, Ordering::Relaxed);
        if !self.busy.lock().unwrap().insert(link) {
            self.overlaps.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Finish an operation: the link is free before the callback, as on Android.
fn op_done(fake: &Weak<Fake>, link: u64) {
    if let Some(f) = fake.upgrade() {
        f.busy.lock().unwrap().remove(&link);
    }
}

struct Bridge(Arc<Fake>);

impl AndroidDevices for Bridge {
    fn ble_start_scan(&self, services: &[String]) -> Result<(), String> {
        let f = &self.0;
        f.scan_starts.lock().unwrap().push(services.to_vec());
        f.scanning.store(true, Ordering::Relaxed);
        let (on, h) = (f.scanning.clone(), f.host());
        std::thread::spawn(move || {
            while on.load(Ordering::Relaxed) {
                h.on_scan_result(AndroidScanResult {
                    address: ADDR.to_lowercase(),
                    name: Some("Fake HR".into()),
                    rssi: -61,
                    service_uuids: vec!["0000180d-0000-1000-8000-00805f9b34fb".into()],
                    manufacturer_data: vec![(0x0059, vec![7])],
                    service_data: vec![("0000180f-0000-1000-8000-00805f9b34fb".into(), vec![90])],
                    tx_power: None,
                    connectable: Some(true),
                });
                std::thread::sleep(Duration::from_millis(15));
            }
        });
        Ok(())
    }

    fn ble_stop_scan(&self) {
        self.0.scanning.store(false, Ordering::Relaxed);
    }

    fn ble_connect(&self, link: u64, address: &str) -> Result<(), String> {
        self.0.connects.lock().unwrap().push((link, address.to_string()));
        let ok = address == ADDR;
        self.0.later(10, move |h| h.on_connection_state(link, ok, if ok { 0 } else { 133 }));
        Ok(())
    }

    fn ble_discover_services(&self, link: u64) -> Result<(), String> {
        self.0.later(5, move |h| h.on_services_discovered(link, 0, hr_services()));
        Ok(())
    }

    fn ble_read(&self, link: u64, op: u64, _service: &str, characteristic: &str) -> Result<(), String> {
        self.0.op_start(link);
        let (status, value) = match &characteristic[4..8] {
            "2a00" => (0, b"Fake HR".to_vec()),
            "2a38" => (0, vec![2]),
            _ => (2, Vec::new()),
        };
        let fake = Arc::downgrade(&self.0);
        self.0.later(15, move |h| {
            op_done(&fake, link);
            h.on_op_complete(op, status, value);
        });
        Ok(())
    }

    fn ble_write(&self, link: u64, op: u64, _service: &str, characteristic: &str, data: &[u8], _with_response: bool) -> Result<(), String> {
        self.0.op_start(link);
        self.0.written.lock().unwrap().push((characteristic.to_string(), data.to_vec()));
        let fake = Arc::downgrade(&self.0);
        self.0.later(10, move |h| {
            op_done(&fake, link);
            h.on_op_complete(op, 0, Vec::new());
        });
        Ok(())
    }

    fn ble_set_notify(&self, link: u64, op: u64, service: &str, characteristic: &str, enable: bool) -> Result<(), String> {
        self.0.op_start(link);
        let flag = self.0.notifiers.lock().unwrap().entry((link, characteristic.to_string())).or_insert_with(|| Arc::new(AtomicBool::new(false))).clone();
        flag.store(enable, Ordering::Relaxed);
        if enable {
            let (h, s, c, on) = (self.0.host(), service.to_string(), characteristic.to_string(), flag);
            std::thread::spawn(move || {
                let mut n = 0u8;
                while on.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(15));
                    n = n.wrapping_add(1);
                    h.on_notification(link, &s, &c, vec![0, 70 + n % 10]);
                }
            });
        }
        let fake = Arc::downgrade(&self.0);
        self.0.later(5, move |h| {
            op_done(&fake, link);
            h.on_op_complete(op, 0, Vec::new());
        });
        Ok(())
    }

    fn ble_disconnect(&self, link: u64) {
        self.0.disconnects.lock().unwrap().push(link);
        for ((l, _), on) in self.0.notifiers.lock().unwrap().iter() {
            if *l == link {
                on.store(false, Ordering::Relaxed);
            }
        }
    }

    fn usb_devices(&self) -> Result<Vec<UsbDeviceInfo>, String> {
        Ok(vec![MockUsbDevice::cdc_acm(DEV).info])
    }

    fn usb_has_permission(&self, _device: &str) -> bool {
        self.0.permitted.load(Ordering::Relaxed)
    }

    fn usb_request_permission(&self, device: &str) -> Result<(), String> {
        self.0.perm_requests.fetch_add(1, Ordering::Relaxed);
        if self.0.silent.load(Ordering::Relaxed) {
            return Ok(());
        }
        let (answer, fake, d) = (self.0.answer.load(Ordering::Relaxed), Arc::downgrade(&self.0), device.to_string());
        self.0.later(40, move |h| {
            if let Some(f) = fake.upgrade() {
                f.permitted.store(answer, Ordering::Relaxed);
            }
            h.on_usb_permission(&d, answer);
        });
        Ok(())
    }

    fn usb_open(&self, device: &str) -> Result<i32, String> {
        if !self.0.permitted.load(Ordering::Relaxed) {
            return Err(format!("no permission for {device}"));
        }
        let fd = self.0.next_fd.fetch_add(1, Ordering::Relaxed);
        self.0.opened.lock().unwrap().push(fd);
        Ok(fd)
    }

    fn usb_close(&self, fd: i32) {
        self.0.closed.lock().unwrap().push(fd);
    }
}

/// A host whose descriptors open simulated devices.
fn setup() -> (Arc<Fake>, Arc<AndroidHost>, MockUsb, Natives) {
    let fake = Fake::new();
    let usb = MockUsb::new(vec![MockUsbDevice::cdc_acm(DEV)]);
    let u = usb.clone();
    let opener: FdOpener = Arc::new(move |fd, info| {
        assert!(fd >= 40, "a descriptor from usb_open");
        u.open(&info.id, &Wait::timeout_only(Duration::from_secs(1)))
    });
    let host = AndroidHost::with_fd_opener(Arc::new(Bridge(fake.clone())), opener);
    *fake.host.lock().unwrap() = Some(Arc::downgrade(&host));
    let mut n = Natives::new();
    register_with(&mut n, host.backends());
    (fake, host, usb, n)
}

#[test]
fn bluetooth_through_the_bridge() {
    let (fake, host, _usb, n) = setup();
    let src = r#"
        Scan = page.stream("scan", "ble.scan", { latest = true, services = { "180d" }, interval_ms = 30 })
        Link = page.input("link", nil, { agent = true })
        L = page.stream("l", "ble.link", { request = Link, latest = true })
        R1 = page.input("r1", nil, { agent = true })
        R2 = page.input("r2", nil, { agent = true })
        Read1 = page.job("read1", "ble.read", { R1 })
        Read2 = page.job("read2", "ble.read", { R2 })
        W = page.input("w", nil, { agent = true })
        Write = page.job("write", "ble.write", { W })
    "#;
    let mut p = load(ALL, src, &n).unwrap();

    // scanning: one radio scan with the filter, results parsed
    let d = wait_for(&mut p, "scan", |d| last(d).get("count").num() == Some(1.0));
    let dev = last(&d).get("devices").at(1);
    assert_eq!(dev.get("id").as_str(), Some(ADDR), "addresses are upper case");
    assert_eq!(dev.get("name").as_str(), Some("Fake HR"));
    assert_eq!(dev.get("rssi").num(), Some(-61.0));
    assert_eq!(dev.get("manufacturer").at(1).get("company").num(), Some(89.0));
    assert_eq!(dev.get("service_data").at(1).get("uuid").as_str(), Some("0000180f-0000-1000-8000-00805f9b34fb"));
    assert!(dev.get("tx_power").is_nil());
    assert_eq!(*fake.scan_starts.lock().unwrap(), vec![vec!["0000180d-0000-1000-8000-00805f9b34fb".to_string()]]);

    // a link with notifications
    set(&mut p, "link", &format!(r#"{{ "device": "{}", "subscribe": ["2a37"] }}"#, ADDR.to_lowercase()));
    let d = wait_for(&mut p, "l", |d| last(d).get("state").as_str() == Some("connected") && last(d).get("count").num().unwrap_or(0.0) >= 3.0);
    assert_eq!(fake.connects.lock().unwrap()[0].1, ADDR, "getRemoteDevice gets upper case");
    let note = last(&d).get("notifications").items().last().unwrap().clone();
    assert_eq!(note.get("characteristic").as_str(), Some("00002a37-0000-1000-8000-00805f9b34fb"));
    assert_eq!(note.get("data").as_bytes().map(<[u8]>::len), Some(2));
    assert_eq!(last(&d).get("services").items().len(), 2);

    // two reads at once share the link, one GATT operation at a time
    let ops_before = fake.ops.load(Ordering::Relaxed);
    set(&mut p, "r1", &format!(r#"{{ "device": "{ADDR}", "characteristic": "2a00" }}"#));
    set(&mut p, "r2", &format!(r#"{{ "device": "{ADDR}", "characteristic": "2a38" }}"#));
    let a = wait_for(&mut p, "read1", |d| d.get("data").as_bytes().is_some());
    let b = wait_for(&mut p, "read2", |d| d.get("data").as_bytes().is_some());
    assert_eq!(a.get("text").as_str(), Some("Fake HR"));
    assert_eq!(b.get("data").as_bytes(), Some(&[2u8][..]));
    assert_eq!(fake.ops.load(Ordering::Relaxed) - ops_before, 2);
    assert_eq!(fake.overlaps.load(Ordering::Relaxed), 0, "GATT operations never overlap on a link");
    assert_eq!(fake.connects.lock().unwrap().len(), 1, "the jobs used the open link");
    // a status from Android becomes a message
    set(&mut p, "r1", &format!(r#"{{ "device": "{ADDR}", "characteristic": "2a39", "timeout_ms": 1000 }}"#));
    let d = wait_for(&mut p, "read1", |d| job_error(d).is_some());
    assert!(job_error(&d).unwrap().contains("cannot be read"), "{d:?}");

    set(&mut p, "w", &format!(r#"{{ "device": "{ADDR}", "characteristic": "2a39", "data": [1] }}"#));
    wait_for(&mut p, "write", |d| d.get("written").num() == Some(1.0));
    assert_eq!(fake.written.lock().unwrap().last().unwrap(), &("00002a39-0000-1000-8000-00805f9b34fb".to_string(), vec![1]));

    // Android reports the link lost: the stream reconnects
    let link = fake.connects.lock().unwrap()[0].0;
    host.on_connection_state(link, false, 8);
    let d = wait_for(&mut p, "l", |d| last(d).get("state").as_str() == Some("reconnecting"));
    assert!(last(&d).get("error").as_str().unwrap().contains("timed out"));
    wait_for_in(&mut p, "l", Duration::from_secs(5), &mut |d| last(d).get("state").as_str() == Some("connected"));
    assert_eq!(fake.connects.lock().unwrap().len(), 2);
    assert!(fake.disconnects.lock().unwrap().contains(&link), "the lost GATT is closed");

    // a device that does not answer
    set(&mut p, "link", r#"{ "device": "11:22:33:44:55:66", "reconnect": false }"#);
    let d = wait_for(&mut p, "l", |d| last(d).get("state").as_str() == Some("failed"));
    assert!(last(&d).get("error").as_str().unwrap().contains("GATT_ERROR 133"), "{d:?}");

    // the system stops the scan; the stream starts it again after a pause
    fake.scanning.store(false, Ordering::Relaxed);
    host.on_scan_failed(6);
    let d = wait_for(&mut p, "scan", |d| last(d).get("error").as_str().is_some());
    assert!(last(&d).get("error").as_str().unwrap().contains("too often"));
    assert_eq!(last(&d).get("scanning").as_bool(), Some(false));
    wait_for_in(&mut p, "scan", Duration::from_secs(6), &mut |d| last(d).get("scanning").as_bool() == Some(true) && last(d).get("error").is_nil());
    assert_eq!(fake.scan_starts.lock().unwrap().len(), 2);
    assert!(fake.scanning.load(Ordering::Relaxed));
    p.shutdown();
    drop(p);
    std::thread::sleep(Duration::from_millis(100));
    assert!(!fake.scanning.load(Ordering::Relaxed), "the scan stops with the page");
}

#[test]
fn usb_permission_and_cdc_serial() {
    let (fake, _host, usb, n) = setup();
    let src = r#"
        Req = page.input("req", nil, { agent = true })
        S = page.stream("s", "serial.port", { request = Req, latest = true })
        Ports = page.derive("ports", {}, function() return serial.ports() end)
        Devices = page.derive("devices", {}, function() return usb.devices() end)
        page.on("text", function(t) serial.send("usb:/dev/bus/usb/001/005", t) end)
    "#;
    let mut p = load(ALL, src, &n).unwrap();
    assert_eq!(p.cell_value("devices").unwrap().at(1).get("id").as_str(), Some(DEV));
    assert_eq!(p.cell_value("ports").unwrap().at(1).get("path").as_str(), Some("usb:/dev/bus/usb/001/005"));

    set(&mut p, "req", r#"{ "path": "usb:/dev/bus/usb/001/005", "baud": 9600 }"#);
    wait_for(&mut p, "s", |d| last(d).get("state").as_str() == Some("open"));
    assert_eq!(fake.perm_requests.load(Ordering::Relaxed), 1, "the user was asked once");
    assert_eq!(usb.line_coding(DEV), Some(vec![0x80, 0x25, 0, 0, 0, 0, 8]));
    assert_eq!(usb.control_lines(DEV), Some(3), "DTR and RTS raised");
    p.text_input("hello android\n");
    let d = wait_for(&mut p, "s", |d| last(d).get("lines").items().len() == 1);
    assert_eq!(strs(last(&d).get("lines")), vec!["hello android"]);

    // closing the port gives the descriptor back to the host
    let fd = fake.opened.lock().unwrap()[0];
    set(&mut p, "req", "null");
    wait_for(&mut p, "s", |d| last(d).get("state").as_str() == Some("idle"));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(*fake.closed.lock().unwrap(), vec![fd]);
}

#[test]
fn usb_permission_denied() {
    let (fake, _host, _usb, n) = setup();
    fake.answer.store(false, Ordering::Relaxed);
    let src = r#"
        O = page.input("o", nil, { agent = true })
        Open = page.job("open", "usb.open", { O })
    "#;
    let mut p = load(ALL, src, &n).unwrap();
    set(&mut p, "o", &format!(r#"{{ "device": "{DEV}" }}"#));
    let d = wait_for(&mut p, "open", |d| job_error(d).is_some());
    assert_eq!(job_error(&d), Some("usb.open: permission to use USB device /dev/bus/usb/001/005 was denied"));
    assert!(fake.opened.lock().unwrap().is_empty());
    set(&mut p, "o", r#"{ "device": "/dev/bus/usb/009/009" }"#);
    let d = wait_for(&mut p, "open", |d| job_error(d).is_some_and(|e| e.contains("009")));
    assert!(job_error(&d).unwrap().contains("unplugged"));
}

#[test]
fn usb_permission_asked_again_after_giving_up() {
    let (fake, _host, _usb, n) = setup();
    fake.silent.store(true, Ordering::Relaxed);
    let src = r#"
        O = page.input("o", nil, { agent = true })
        Open = page.job("open", "usb.open", { O })
    "#;
    let mut p = load(ALL, src, &n).unwrap();
    // the dialog never answers: the open gives up after its timeout
    set(&mut p, "o", &format!(r#"{{ "device": "{DEV}", "timeout_ms": 200 }}"#));
    let d = wait_for(&mut p, "open", |d| job_error(d).is_some());
    assert_eq!(job_error(&d), Some("usb.open: waiting for the user to allow the USB device: timed out after 200 ms"));
    assert_eq!(fake.perm_requests.load(Ordering::Relaxed), 1);
    // nobody waits any more, so the next open asks again (it does not wait
    // on the lost question until its own timeout)
    set(&mut p, "o", &format!(r#"{{ "device": "{DEV}", "timeout_ms": 200, "try": 2 }}"#));
    wait_for(&mut p, "open", |d| job_error(d).is_some());
    assert_eq!(fake.perm_requests.load(Ordering::Relaxed), 2);
    // the user answers this time
    fake.silent.store(false, Ordering::Relaxed);
    set(&mut p, "o", &format!(r#"{{ "device": "{DEV}", "timeout_ms": 5000, "try": 3 }}"#));
    let d = wait_for(&mut p, "open", |d| d.get("interfaces").items().len() == 2);
    assert!(d.get("handle").as_res().is_some());
    assert_eq!(fake.perm_requests.load(Ordering::Relaxed), 3);
    assert_eq!(fake.opened.lock().unwrap().len(), 1);
}

#[test]
fn availability_through_the_bridge() {
    let (_fake, _host, _usb, n) = setup();
    let mut p = load(ALL, "A = page.derive('a', {}, function() return { usb.available(), serial.available(), ble.available() } end)", &n).unwrap();
    let a = p.cell_value("a").unwrap();
    assert!(a.items().iter().all(|x| x.get("ok").as_bool() == Some(true)), "{a:?}");
}

#[test]
fn installed_bridge_and_free_callbacks() {
    let fake = Fake::new();
    let host = android::set_android_bridge(Arc::new(Bridge(fake.clone())));
    *fake.host.lock().unwrap() = Some(Arc::downgrade(&host));
    assert!(Arc::ptr_eq(&android::android_host().unwrap(), &host));
    // the free functions reach the installed host
    let events = Arc::new(Mutex::new(Vec::new()));
    let e = events.clone();
    let guard = host.backends().ble.unwrap().scan(&[], Arc::new(move |ev| e.lock().unwrap().push(format!("{ev:?}")))).unwrap();
    android::on_scan_failed(1);
    assert!(events.lock().unwrap().iter().any(|e| e.contains("already running")));
    android::on_scan_result(AndroidScanResult { address: "01:02:03:04:05:06".into(), rssi: -40, ..Default::default() });
    assert!(events.lock().unwrap().iter().any(|e| e.contains("01:02:03:04:05:06")));
    drop(guard);
    // callbacks for unknown links and operations are ignored
    android::on_connection_state(999, false, 0);
    android::on_op_complete(999, 0, Vec::new());
    android::on_notification(999, "180d", "2a37", vec![1]);
    android::on_usb_permission("nothing", true);
    android::on_usb_detached("nothing");
    android::on_services_discovered(999, 0, Vec::new());
    fake.scanning.store(false, Ordering::Relaxed);
}
