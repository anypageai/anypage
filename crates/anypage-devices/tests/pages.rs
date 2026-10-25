//! The device modules driven through real pages, on simulated devices.

mod common;

use std::sync::Arc;
use std::time::Duration;

use anypage_devices::ble::parse_uuid;
use anypage_devices::mock::{MockUsb, MockUsbDevice, Mocks, UART_RX, UART_TX};
use anypage_devices::serial::cdc::CdcSerial;
use anypage_devices::{register_with, Backends};
use anypage_engine::Page;
use anypage_runtime::Natives;
use common::*;

const HR: &str = "C0:FF:EE:00:00:01";
const BEACON: &str = "C0:FF:EE:00:00:02";
const HR_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const HR_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";

fn natives(m: &Mocks) -> Natives {
    let mut n = Natives::new();
    register_with(&mut n, m.backends());
    n
}

#[test]
fn modules_are_declared_and_optional() {
    let m = Mocks::standard();
    let n = natives(&m);
    let e = load("{}", "ble.uuid('180d')", &n).err().unwrap();
    assert!(e.contains("\"ble\" module is not declared"), "{e}");
    let e = load("{}", "page.stream('s', 'ble.scan', {})", &n).err().unwrap();
    assert!(e.contains("not declared"), "{e}");
    // a build without devices: optional modules are simply absent
    load(r#"{ "optional_modules": ["ble", "usb", "serial"] }"#, "assert(ble == nil and usb == nil and serial == nil)", &Natives::new()).unwrap();
    let e = load(ALL, "", &Natives::new()).err().unwrap();
    assert!(e.contains("\"ble\" module, which this app or platform does not provide"), "{e}");
    // a build with only USB
    let mut only_usb = Natives::new();
    register_with(&mut only_usb, Backends { usb: m.backends().usb, ..Default::default() });
    load(r#"{ "optional_modules": ["ble", "usb"] }"#, "assert(ble == nil and usb ~= nil)", &only_usb).unwrap();
    // pure helper inside a cell function
    let mut p = load(ALL, "page.derive('u', {}, function() return ble.uuid('0x2A37') end)", &n).unwrap();
    assert_eq!(p.cell_value("u").unwrap().as_str(), Some(HR_MEASUREMENT));
    let e = load(ALL, "ble.uuid('nope')", &n).err().unwrap();
    assert!(e.contains("ble.uuid: not a UUID"), "{e}");
}

#[test]
fn scanning_filters_and_errors() {
    let m = Mocks::standard();
    let src = r#"
        Req = page.input("req", nil, { agent = true })
        Scan = page.stream("scan", "ble.scan", { request = Req, latest = true })
    "#;
    let mut p = load(ALL, src, &natives(&m)).unwrap();
    let d = wait_for(&mut p, "scan", |d| last(d).get("scanning").as_bool() == Some(false));
    assert!(last(&d).get("devices").items().is_empty());

    set(&mut p, "req", r#"{ "on": true, "interval_ms": 30 }"#);
    let d = wait_for(&mut p, "scan", |d| last(d).get("count").num() == Some(2.0));
    let devs = last(&d).get("devices").items();
    // order of first sighting, full shapes
    let hr = devs.iter().find(|x| x.get("id").as_str() == Some(HR)).unwrap();
    assert_eq!(hr.get("name").as_str(), Some("AnyPage HR"));
    assert!(hr.get("rssi").num().unwrap() <= -58.0);
    assert_eq!(strs(hr.get("services"))[0], HR_SERVICE);
    assert_eq!(hr.get("manufacturer").at(1).get("company").num(), Some(65535.0));
    assert_eq!(hr.get("manufacturer").at(1).get("data").as_bytes(), Some(&[1u8, 2, 3][..]));
    assert_eq!(hr.get("tx_power").num(), Some(4.0));
    assert_eq!(hr.get("connectable").as_bool(), Some(true));
    assert!(hr.get("last_seen").num().unwrap() > 1.7e9);
    let beacon = devs.iter().find(|x| x.get("id").as_str() == Some(BEACON)).unwrap();
    assert!(beacon.get("name").is_nil());
    assert_eq!(beacon.get("connectable").as_bool(), Some(false));

    // service filter
    set(&mut p, "req", r#"{ "services": ["180d"], "interval_ms": 30 }"#);
    let d = wait_for(&mut p, "scan", |d| last(d).get("count").num() == Some(1.0) && last(d).get("scanning").as_bool() == Some(true));
    assert_eq!(last(&d).get("devices").at(1).get("id").as_str(), Some(HR));
    // signal filter
    set(&mut p, "req", r#"{ "min_rssi": -70, "interval_ms": 30 }"#);
    let d = wait_for(&mut p, "scan", |d| last(d).get("count").num() == Some(1.0));
    assert_eq!(last(&d).get("devices").at(1).get("id").as_str(), Some(HR));
    // a bad request is reported, the scan goes on
    set(&mut p, "req", r#"{ "services": ["not-a-uuid"] }"#);
    let d = wait_for(&mut p, "scan", |d| last(d).get("error").as_str().is_some());
    assert!(last(&d).get("error").as_str().unwrap().contains("not a UUID"));

    // the radio goes off: the error shows, the stream retries
    set(&mut p, "req", r#"{ "on": true, "interval_ms": 30 }"#);
    wait_for(&mut p, "scan", |d| last(d).get("count").num() == Some(2.0) && last(d).get("error").is_nil());
    m.ble.set_powered(false);
    let d = wait_for(&mut p, "scan", |d| last(d).get("scanning").as_bool() == Some(false) && last(d).get("error").as_str().is_some());
    assert!(last(&d).get("error").as_str().unwrap().contains("switched off"), "{d:?}");
    m.ble.set_powered(true);
    let d = wait_for_in(&mut p, "scan", Duration::from_secs(8), &mut |d| last(d).get("scanning").as_bool() == Some(true));
    assert!(last(&d).get("error").is_nil());

    // off
    set(&mut p, "req", "false");
    wait_for(&mut p, "scan", |d| last(d).get("scanning").as_bool() == Some(false));
    p.shutdown();
}

#[test]
fn scan_from_options_without_a_request_cell() {
    let m = Mocks::standard();
    let mut p = load(ALL, r#"Scan = page.stream("scan", "ble.scan", { latest = true, services = { "180d" }, interval_ms = 30 })"#, &natives(&m)).unwrap();
    let d = wait_for(&mut p, "scan", |d| last(d).get("count").num() == Some(1.0));
    assert_eq!(last(&d).get("devices").at(1).get("id").as_str(), Some(HR));
}

#[test]
fn links_notify_echo_and_reconnect() {
    let m = Mocks::standard();
    let src = r#"
        Link = page.input("link", nil, { agent = true })
        L = page.stream("l", "ble.link", { request = Link, latest = true, keep = 8 })
        W = page.input("w", nil, { agent = true })
        Wr = page.job("wr", "ble.write", { W })
    "#;
    let mut p = load(ALL, src, &natives(&m)).unwrap();
    wait_for(&mut p, "l", |d| last(d).get("state").as_str() == Some("idle"));

    set(&mut p, "link", &format!(r#"{{ "device": "{HR}", "subscribe": [{{ "service": "180d", "characteristic": "2a37" }}] }}"#));
    let d = wait_for(&mut p, "l", |d| last(d).get("state").as_str() == Some("connected") && last(d).get("count").num().unwrap_or(0.0) >= 5.0);
    let l = last(&d);
    assert_eq!(l.get("device").as_str(), Some(HR));
    assert_eq!(l.get("services").items().len(), 4);
    assert_eq!(strs(l.get("services").at(2).get("characteristics").at(1).get("properties")), vec!["notify"]);
    let notes = l.get("notifications").items();
    assert!(notes.len() <= 8, "keep = 8 bounds the history");
    let n = notes.last().unwrap();
    assert_eq!(n.get("characteristic").as_str(), Some(HR_MEASUREMENT));
    assert_eq!(n.get("service").as_str(), Some(HR_SERVICE));
    let bpm = n.get("data").as_bytes().unwrap();
    assert_eq!(bpm.len(), 2);
    assert!((60..100).contains(&bpm[1]));
    assert!(n.get("seq").num().unwrap() >= 5.0);
    assert_eq!(l.get("latest").get(HR_MEASUREMENT).as_bytes().map(<[u8]>::len), Some(2));
    assert_eq!(m.ble.connects(), 1);

    // out of range: reconnects by itself
    assert_eq!(m.ble.drop_link(HR), 1);
    let d = wait_for(&mut p, "l", |d| last(d).get("state").as_str() == Some("reconnecting"));
    assert!(last(&d).get("error").as_str().unwrap().contains("out of range"));
    wait_for(&mut p, "l", |d| last(d).get("state").as_str() == Some("connected"));
    assert_eq!(m.ble.connects(), 2);

    // same device, other subscriptions: no reconnect; the UART echoes writes
    let uart = format!(r#"{{ "device": "{HR}", "subscribe": [{{ "service": "{}", "characteristic": "{}" }}] }}"#, anypage_devices::mock::UART_SERVICE, UART_TX);
    set(&mut p, "link", &uart);
    std::thread::sleep(Duration::from_millis(50));
    set(&mut p, "w", &format!(r#"{{ "device": "{HR}", "characteristic": "{UART_RX}", "data": "ping" }}"#));
    let w = wait_for(&mut p, "wr", |d| !d.is_nil() && d.get("state").as_str() != Some("pending"));
    assert_eq!(w.get("written").num(), Some(4.0), "{w:?}");
    let d = wait_for(&mut p, "l", |d| last(d).get("latest").get(UART_TX).as_bytes() == Some(b"ping"));
    assert_eq!(m.ble.connects(), 2, "the job used the open link");
    let count = last(&d).get("count").num().unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let d = wait_for(&mut p, "l", |_| true);
    assert_eq!(last(&d).get("count").num().unwrap(), count, "heart rate no longer subscribed");

    // a subscription the device cannot do is reported, the link stays
    set(&mut p, "link", &format!(r#"{{ "device": "{HR}", "subscribe": ["2a38", "2a99"] }}"#));
    let d = wait_for(&mut p, "l", |d| last(d).get("error").as_str().is_some());
    let e = last(&d).get("error").as_str().unwrap();
    assert!(e.contains("does not notify") && e.contains("no characteristic"), "{e}");
    assert_eq!(last(&d).get("state").as_str(), Some("connected"));

    // nil disconnects
    set(&mut p, "link", "null");
    wait_for(&mut p, "l", |d| last(d).get("state").as_str() == Some("idle"));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(m.ble.open_links(), 0);

    // an unknown device fails after its timeout; without reconnect it stays failed
    set(&mut p, "link", r#"{ "device": "00:00:00:00:00:09", "timeout_ms": 500, "reconnect": false }"#);
    let d = wait_for(&mut p, "l", |d| last(d).get("state").as_str() == Some("failed"));
    assert!(last(&d).get("error").as_str().unwrap().contains("did not answer within 500 ms"), "{d:?}");
    // a beacon refuses connections
    set(&mut p, "link", &format!(r#"{{ "device": "{BEACON}", "reconnect": false }}"#));
    let d = wait_for(&mut p, "l", |d| last(d).get("state").as_str() == Some("failed") && last(d).get("device").as_str() == Some(BEACON));
    assert!(last(&d).get("error").as_str().unwrap().contains("does not accept connections"));
    p.shutdown();
}

#[test]
fn ble_send_from_handlers() {
    let m = Mocks::standard();
    let src = &format!(
        r#"
        L = page.stream("l", "ble.link", {{ latest = true, device = "{HR}", subscribe = {{ "{UART_TX}" }} }})
        page.on("text", function(t) ble.send("{}", "{UART_RX}", t, {{ response = false }}) end)
        page.on("key", function(k)
            if k == "bad" then ble.send("{HR}", "2a00", "x") end
            if k == "nobody" then ble.send("00:00:00:00:00:09", "{UART_RX}", "x") end
            return true
        end)
    "#,
        HR.to_lowercase()
    );
    let mut p = load(ALL, src, &natives(&m)).unwrap();
    wait_for(&mut p, "l", |d| last(d).get("state").as_str() == Some("connected"));
    p.text_input("hello");
    p.text_input("again");
    let d = wait_for(&mut p, "l", |d| last(d).get("sent").num() == Some(2.0));
    assert_eq!(last(&d).get("latest").get(UART_TX).as_bytes().map(<[u8]>::to_vec), Some(b"again".to_vec()));
    assert_eq!(p.handler_errors, 0);
    // a characteristic that cannot be written: the link reports it
    assert!(p.key("bad"));
    let d = wait_for(&mut p, "l", |d| last(d).get("error").as_str().is_some());
    assert!(last(&d).get("error").as_str().unwrap().starts_with("ble.send: characteristic 00002a00"), "{d:?}");
    // a device no link holds: the handler fails with the reason
    p.key("nobody");
    assert_eq!(p.handler_errors, 1);
    assert_eq!(m.ble.connects(), 1);
}

#[test]
fn stream_switched_off_releases_the_device() {
    let m = Mocks::standard();
    let src = &format!(
        r#"
        On = page.input("on", true, {{ agent = true }})
        L = page.stream("l", "ble.link", {{ enabled = On, latest = true, device = "{HR}", subscribe = {{ "2a37" }} }})
    "#
    );
    let mut p = load(ALL, src, &natives(&m)).unwrap();
    wait_for(&mut p, "l", |d| last(d).get("count").num().unwrap_or(0.0) >= 1.0);
    set(&mut p, "on", "false");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(m.ble.open_links(), 0);
    set(&mut p, "on", "true");
    wait_for(&mut p, "l", |d| last(d).get("state").as_str() == Some("connected"));
    assert_eq!(m.ble.open_links(), 1);
    drop(p);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(m.ble.open_links(), 0, "closing the page releases the device");
}

#[test]
fn ble_read_and_write_jobs() {
    let m = Mocks::standard();
    let src = &format!(
        r#"
        R = page.input("r", nil, {{ agent = true }})
        Read = page.job("read", "ble.read", {{ R }}, {{ device = "{HR}", timeout_ms = 2000 }})
        W = page.input("w", nil, {{ agent = true }})
        Write = page.job("write", "ble.write", {{ W }}, {{ device = "{HR}" }})
    "#
    );
    let mut p = load(ALL, src, &natives(&m)).unwrap();
    assert!(wait_for(&mut p, "read", |_| true).is_nil(), "idle without a request");

    set(&mut p, "r", r#"{ "characteristic": "2a00" }"#);
    let d = wait_for(&mut p, "read", |d| d.get("data").as_bytes().is_some());
    assert_eq!(d.get("text").as_str(), Some("AnyPage HR"));
    assert!(d.get("at").num().unwrap() > 1.7e9);
    set(&mut p, "r", r#"{ "service": "180f", "characteristic": "2a19" }"#);
    let d = wait_for(&mut p, "read", |d| d.get("data").as_bytes() == Some(&[87u8][..]));
    assert_eq!(d.get("text").as_str(), Some("W"), "text is only a UTF-8 decoding");
    set(&mut p, "r", r#"{ "characteristic": "2a38" }"#);
    let d = wait_for(&mut p, "read", |d| d.get("data").as_bytes() == Some(&[1u8][..]));
    assert!(d.get("text").is_nil(), "control bytes are not text");

    set(&mut p, "r", r#"{ "characteristic": "2a37" }"#);
    let d = wait_for(&mut p, "read", |d| job_error(d).is_some());
    assert!(job_error(&d).unwrap().starts_with("ble.read: characteristic 00002a37"), "{d:?}");
    assert!(job_error(&d).unwrap().contains("cannot be read (properties: notify)"));
    set(&mut p, "r", r#"{ "service": "180f", "characteristic": "2a37" }"#);
    let d = wait_for(&mut p, "read", |d| job_error(d).is_some_and(|e| e.contains("has no characteristic")));
    assert!(job_error(&d).unwrap().contains("service 0000180f"));
    set(&mut p, "r", r#"{ "characteristic": "zz" }"#);
    let d = wait_for(&mut p, "read", |d| job_error(d).is_some_and(|e| e.contains("not a UUID")));
    assert!(job_error(&d).is_some());
    set(&mut p, "r", r#"{ "device": "00:00:00:00:00:09", "characteristic": "2a00", "timeout_ms": 200 }"#);
    let d = wait_for(&mut p, "read", |d| job_error(d).is_some_and(|e| e.contains("200 ms")));
    assert!(job_error(&d).unwrap().contains("did not answer"));

    // a write with a list of bytes, then read back
    set(&mut p, "w", r#"{ "characteristic": "2a39", "data": [1, 255] }"#);
    let d = wait_for(&mut p, "write", |d| d.get("written").num().is_some());
    assert_eq!(d.get("written").num(), Some(2.0));
    assert_eq!(m.ble.value(HR, parse_uuid("2a39").unwrap()), Some(vec![1, 255]));
    set(&mut p, "w", r#"{ "characteristic": "2a39", "data": [1, 256] }"#);
    let d = wait_for(&mut p, "write", |d| job_error(d).is_some());
    assert!(job_error(&d).unwrap().contains("item 2 must be a byte"));
    set(&mut p, "w", r#"{ "characteristic": "2a00", "data": "x" }"#);
    let d = wait_for(&mut p, "write", |d| job_error(d).is_some());
    assert!(job_error(&d).unwrap().contains("cannot be written"), "{d:?}");
    let big = format!(r#"{{ "characteristic": "2a39", "data": "{}" }}"#, "x".repeat(513));
    set(&mut p, "w", &big);
    let d = wait_for(&mut p, "write", |d| job_error(d).is_some_and(|e| e.contains("limit")));
    assert!(job_error(&d).unwrap().contains("512"));
    // without a response, where the characteristic allows it
    set(&mut p, "w", &format!(r#"{{ "characteristic": "{UART_RX}", "data": "a", "response": false }}"#));
    let d = wait_for(&mut p, "write", |d| d.get("written").num().is_some());
    assert_eq!(d.get("written").num(), Some(1.0));
    // every job connected for itself and let go
    assert_eq!(m.ble.open_links(), 0);
}

#[test]
fn usb_listing_control_and_bulk() {
    let m = Mocks::standard();
    let src = r#"
        Devices = page.derive("devices", {}, function() return usb.devices() end)
        O = page.input("o", nil, { agent = true })
        Open = page.job("open", "usb.open", { O })
        C = page.input("c", nil, { agent = true })
        Ctl = page.job("ctl", "usb.control", { C }, { device = "usb-loop" })
        BO = page.input("bo", nil, { agent = true })
        BulkOut = page.job("bulk_out", "usb.bulk_out", { BO }, { device = "usb-loop" })
        BI = page.input("bi", nil, { agent = true })
        BulkIn = page.job("bulk_in", "usb.bulk_in", { BI }, { device = "usb-loop" })
        page.on("key", function(k) return usb.close(k) end)
    "#;
    let mut p = load(ALL, src, &natives(&m)).unwrap();
    let devs = p.cell_value("devices").unwrap();
    assert_eq!(devs.items().len(), 3);
    let d = devs.at(1);
    assert_eq!(d.get("id").as_str(), Some("usb-loop"));
    assert_eq!((d.get("vid").num(), d.get("pid").num()), (Some(0x1209 as f64), Some(1.0)));
    assert_eq!(d.get("product").as_str(), Some("Loopback"));
    assert_eq!(d.get("speed").as_str(), Some("high"));
    assert_eq!(d.get("driver").as_str(), Some("WinUSB"));
    assert_eq!(d.get("interfaces").at(1).get("class").num(), Some(255.0));

    set(&mut p, "o", r#"{ "device": "usb-loop", "claim": [0] }"#);
    let o = wait_for(&mut p, "open", |d| d.get("interfaces").items().len() == 1);
    let eps = o.get("interfaces").at(1).get("endpoints");
    assert_eq!(eps.at(2).get("address").num(), Some(0x81 as f64));
    assert_eq!(eps.at(2).get("direction").as_str(), Some("in"));
    assert_eq!(eps.at(2).get("type").as_str(), Some("bulk"));
    assert_eq!(eps.at(3).get("type").as_str(), Some("interrupt"));
    assert_eq!(eps.at(1).get("max_packet").num(), Some(512.0));

    // vendor requests: store then read back
    set(&mut p, "c", r#"{ "request": 1, "data": "stored!" }"#);
    let d = wait_for(&mut p, "ctl", |d| d.get("written").num().is_some());
    assert_eq!(d.get("written").num(), Some(7.0));
    set(&mut p, "c", r#"{ "request": 2, "direction": "in", "length": 64 }"#);
    let d = wait_for(&mut p, "ctl", |d| d.get("data").as_bytes().is_some());
    assert_eq!(d.get("data").as_bytes(), Some(&b"stored!"[..]));
    // a standard GET_DESCRIPTOR(device)
    set(&mut p, "c", r#"{ "type": "standard", "request": 6, "value": 256, "length": 18 }"#);
    let d = wait_for(&mut p, "ctl", |d| d.get("length").num() == Some(18.0));
    let desc = d.get("data").as_bytes().unwrap();
    assert_eq!((desc[0], desc[1], u16::from_le_bytes([desc[8], desc[9]])), (18, 1, 0x1209));
    // the device refuses unknown requests
    set(&mut p, "c", r#"{ "request": 99, "length": 4 }"#);
    let d = wait_for(&mut p, "ctl", |d| job_error(d).is_some());
    assert!(job_error(&d).unwrap().contains("stalled"));
    set(&mut p, "c", r#"{ "request": 1, "direction": "in" }"#);
    let d = wait_for(&mut p, "ctl", |d| job_error(d).is_some_and(|e| e.contains("length")));
    assert!(job_error(&d).unwrap().starts_with("usb.control: missing \"length\""));

    // bulk loopback
    set(&mut p, "bo", r#"{ "endpoint": 1, "data": [1, 2, 3, 4, 5] }"#);
    let d = wait_for(&mut p, "bulk_out", |d| d.get("written").num().is_some());
    assert_eq!(d.get("written").num(), Some(5.0));
    set(&mut p, "bi", r#"{ "endpoint": 129, "length": 512 }"#);
    let d = wait_for(&mut p, "bulk_in", |d| d.get("data").as_bytes().is_some());
    assert_eq!(d.get("data").as_bytes(), Some(&[1u8, 2, 3, 4, 5][..]));
    // nothing more to read: a clear timeout
    set(&mut p, "bi", r#"{ "endpoint": 129, "length": 64, "timeout_ms": 100 }"#);
    let d = wait_for(&mut p, "bulk_in", |d| job_error(d).is_some());
    assert_eq!(job_error(&d), Some("usb.bulk_in: no data from endpoint 0x81 within 100 ms"));
    // interrupt endpoints work through bulk_in too
    m.usb.feed("usb-loop", 0x82, &[9, 9]);
    set(&mut p, "bi", r#"{ "endpoint": 130, "length": 8 }"#);
    let d = wait_for(&mut p, "bulk_in", |d| d.get("data").as_bytes().is_some());
    assert_eq!(d.get("data").as_bytes(), Some(&[9u8, 9][..]));
    // checks before any transfer
    set(&mut p, "bi", r#"{ "endpoint": 1, "length": 8 }"#);
    let d = wait_for(&mut p, "bulk_in", |d| job_error(d).is_some());
    assert!(job_error(&d).unwrap().contains("not an IN endpoint (IN endpoints are 0x81..0x8f)"));
    set(&mut p, "bi", r#"{ "endpoint": 133, "length": 8 }"#);
    let d = wait_for(&mut p, "bulk_in", |d| job_error(d).is_some_and(|e| e.contains("0x85")));
    assert!(job_error(&d).unwrap().contains("not in the device's active configuration (it has 0x01, 0x81, 0x82)"));
    set(&mut p, "bi", r#"{ "endpoint": 129, "length": 0 }"#);
    let d = wait_for(&mut p, "bulk_in", |d| job_error(d).is_some_and(|e| e.contains("length")));
    assert!(job_error(&d).is_some());
    set(&mut p, "bi", r#"{ "endpoint": 129, "length": 8, "interface": 1 }"#);
    let d = wait_for(&mut p, "bulk_in", |d| job_error(d).is_some_and(|e| e.contains("belongs to interface 0")));
    assert!(job_error(&d).is_some());

    // a device another driver owns
    set(&mut p, "bi", r#"{ "device": "usb-kbd", "endpoint": 129, "length": 8 }"#);
    let d = wait_for(&mut p, "bulk_in", |d| job_error(d).is_some_and(|e| e.contains("HidUsb")));
    assert!(job_error(&d).unwrap().contains("cannot claim interface 0"));

    // unplugged and back: the stale handle is dropped and the device reopened
    assert_eq!(m.usb.opens("usb-loop"), 1);
    m.usb.set_present("usb-loop", false);
    set(&mut p, "bo", r#"{ "endpoint": 1, "data": "x" }"#);
    let d = wait_for(&mut p, "bulk_out", |d| job_error(d).is_some());
    assert!(job_error(&d).unwrap().contains("disconnected"));
    m.usb.set_present("usb-loop", true);
    set(&mut p, "bo", r#"{ "endpoint": 1, "data": "y" }"#);
    wait_for(&mut p, "bulk_out", |d| d.get("written").num() == Some(1.0));
    assert_eq!(m.usb.opens("usb-loop"), 2);
    // usb.close lets go (an effect, from a handler)
    assert!(p.key("usb-loop"));
    assert!(!p.key("usb-loop"));
}

#[test]
fn long_jobs_are_cancelled() {
    // one worker: a job that ignored cancellation would hold it for 30 s
    let m = Mocks::standard();
    let manifest = r#"{ "format": 1, "id": "cancel", "bounds": [0, 0, 100, 100], "capabilities": { "modules": ["usb", "ble"] }, "limits": { "job_workers": 1 } }"#;
    let src = r#"
        page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        BI = page.input("bi", nil, { agent = true })
        BulkIn = page.job("bulk_in", "usb.bulk_in", { BI }, { device = "usb-loop" })
        R = page.input("r", nil, { agent = true })
        Read = page.job("read", "ble.read", { R })
    "#;
    let pkg = anypage_runtime::Package::from_files("t", [("page.json", manifest), ("main.luau", src)]).unwrap();
    let mut p = anypage_runtime::ScriptPage::load(&pkg, &natives(&m), text(), wake()).unwrap();

    // a read that waits for data that never comes, then a new request
    set(&mut p, "bi", r#"{ "endpoint": 129, "length": 8, "timeout_ms": 30000 }"#);
    assert_eq!(p.cell_value("bulk_in").unwrap().get("state").as_str(), Some("pending"));
    std::thread::sleep(Duration::from_millis(100));
    m.usb.feed("usb-loop", 0x82, &[5]);
    let t0 = std::time::Instant::now();
    set(&mut p, "bi", r#"{ "endpoint": 130, "length": 8 }"#);
    let d = wait_for(&mut p, "bulk_in", |d| d.get("data").as_bytes().is_some());
    assert_eq!(d.get("data").as_bytes(), Some(&[5u8][..]));
    assert!(t0.elapsed() < Duration::from_secs(2), "the waiting transfer was cancelled ({:?})", t0.elapsed());

    // a connect that would wait a minute, then a new request
    set(&mut p, "r", r#"{ "device": "00:00:00:00:00:09", "characteristic": "2a00", "timeout_ms": 60000 }"#);
    assert_eq!(p.cell_value("read").unwrap().get("state").as_str(), Some("pending"));
    std::thread::sleep(Duration::from_millis(100));
    let t0 = std::time::Instant::now();
    set(&mut p, "r", &format!(r#"{{ "device": "{HR}", "characteristic": "2a00" }}"#));
    let d = wait_for(&mut p, "read", |d| d.get("text").as_str().is_some());
    assert_eq!(d.get("text").as_str(), Some("AnyPage HR"));
    assert!(t0.elapsed() < Duration::from_secs(2), "the waiting connect was cancelled ({:?})", t0.elapsed());

    // page.cancel and shutdown end a waiting job too
    set(&mut p, "bi", r#"{ "endpoint": 129, "length": 8, "timeout_ms": 30000 }"#);
    assert_eq!(p.cell_value("bulk_in").unwrap().get("state").as_str(), Some("pending"));
    let t0 = std::time::Instant::now();
    p.shutdown();
    drop(p);
    assert!(t0.elapsed() < Duration::from_secs(2), "shutdown cancels the transfer ({:?})", t0.elapsed());
}

#[test]
fn usb_devices_are_released_with_the_page() {
    let m = Mocks::standard();
    let n = natives(&m);
    let src = r#"
        O = page.input("o", nil, { agent = true })
        Open = page.job("open", "usb.open", { O })
        BO = page.input("bo", nil, { agent = true })
        BulkOut = page.job("bulk_out", "usb.bulk_out", { BO }, { device = "usb-loop" })
        page.on("key", function(k) return usb.close(k) end)
    "#;
    let mut a = load(ALL, src, &n).unwrap();
    set(&mut a, "o", r#"{ "device": "usb-loop", "claim": [0] }"#);
    // (a copy of the value holds the device too: keep none past this line)
    let kind = wait_for(&mut a, "open", |d| d.get("handle").as_res().is_some()).get("handle").as_res().map(|r| r.kind().to_string());
    assert_eq!(kind.as_deref(), Some("usb.device"));
    assert_eq!(m.usb.handles("usb-loop"), 1);
    // transfers, from this page or another, use the device it holds
    set(&mut a, "bo", r#"{ "endpoint": 1, "data": "a" }"#);
    wait_for(&mut a, "bulk_out", |d| d.get("written").num() == Some(1.0));
    let mut b = load(ALL, src, &n).unwrap();
    set(&mut b, "bo", r#"{ "endpoint": 1, "data": "b" }"#);
    wait_for(&mut b, "bulk_out", |d| d.get("written").num() == Some(1.0));
    assert_eq!((m.usb.opens("usb-loop"), m.usb.handles("usb-loop")), (1, 1));

    // the page that opened it closes: the device is released (once the
    // page's worker threads have let go of its graph)
    a.shutdown();
    drop(a);
    let t0 = std::time::Instant::now();
    while m.usb.handles("usb-loop") > 0 {
        assert!(t0.elapsed() < Duration::from_secs(2), "closing the page released the device");
        std::thread::sleep(Duration::from_millis(2));
    }
    eprintln!("released {:?} after the page was dropped", t0.elapsed());
    // a transfer on a device no page holds opens it for that job alone
    set(&mut b, "bo", r#"{ "endpoint": 1, "data": "c" }"#);
    wait_for(&mut b, "bulk_out", |d| d.get("written").num() == Some(1.0));
    assert_eq!((m.usb.opens("usb-loop"), m.usb.handles("usb-loop")), (2, 0));

    // the open job running again for another device lets go of the first
    set(&mut b, "o", r#"{ "device": "usb-loop", "claim": "all" }"#);
    wait_for(&mut b, "open", |d| d.get("id").as_str() == Some("usb-loop"));
    assert_eq!(m.usb.handles("usb-loop"), 1);
    set(&mut b, "o", r#"{ "device": "usb-acm" }"#);
    wait_for(&mut b, "open", |d| d.get("id").as_str() == Some("usb-acm"));
    assert_eq!((m.usb.handles("usb-loop"), m.usb.handles("usb-acm")), (0, 1));
    // usb.close releases it at once, even while the page holds the value
    assert!(b.key("usb-acm"));
    assert_eq!(m.usb.handles("usb-acm"), 0);
    assert!(!b.key("usb-acm"), "already closed");
    // a failed open keeps nothing
    set(&mut b, "o", r#"{ "device": "usb-kbd", "claim": [0] }"#);
    wait_for(&mut b, "open", |d| job_error(d).is_some_and(|e| e.contains("HidUsb")));
    assert_eq!(m.usb.handles("usb-kbd"), 0);
}

#[test]
fn serial_write_is_cancellable() {
    let m = Mocks::standard();
    // flow control holds: MOCK1 takes nothing
    m.serial.set_stalled("MOCK1", true);
    let manifest = r#"{ "format": 1, "id": "cancel", "bounds": [0, 0, 100, 100], "capabilities": { "modules": ["serial"] }, "limits": { "job_workers": 1 } }"#;
    let src = r#"
        page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
        W = page.input("w", nil, { agent = true })
        Wr = page.job("wr", "serial.write", { W })
    "#;
    let pkg = anypage_runtime::Package::from_files("t", [("page.json", manifest), ("main.luau", src)]).unwrap();
    let mut p = anypage_runtime::ScriptPage::load(&pkg, &natives(&m), text(), wake()).unwrap();

    // a write that would wait a minute, then a new request: one worker, so
    // the new write only runs once the first gave up
    let big = format!(r#"{{ "path": "MOCK1", "data": "{}", "timeout_ms": 60000 }}"#, "x".repeat(4096));
    set(&mut p, "w", &big);
    assert_eq!(p.cell_value("wr").unwrap().get("state").as_str(), Some("pending"));
    std::thread::sleep(Duration::from_millis(100));
    let t0 = std::time::Instant::now();
    set(&mut p, "w", r#"{ "path": "MOCK0", "data": "ok" }"#);
    wait_for(&mut p, "wr", |d| d.get("written").num() == Some(2.0));
    assert!(t0.elapsed() < Duration::from_secs(2), "the waiting write was cancelled ({:?})", t0.elapsed());
    assert!(!m.serial.is_open("MOCK1"), "the port opened for the write is closed again");

    // a write the port does not take in time says how far it got
    set(&mut p, "w", r#"{ "path": "MOCK1", "data": "abc", "timeout_ms": 200 }"#);
    let d = wait_for(&mut p, "wr", |d| job_error(d).is_some());
    assert_eq!(job_error(&d), Some("serial.write: timed out after 200 ms with 0 of 3 bytes written (flow control holding, or the device is not reading?)"));

    // shutdown ends a waiting write too
    set(&mut p, "w", &big);
    assert_eq!(p.cell_value("wr").unwrap().get("state").as_str(), Some("pending"));
    std::thread::sleep(Duration::from_millis(100));
    let t0 = std::time::Instant::now();
    p.shutdown();
    drop(p);
    assert!(t0.elapsed() < Duration::from_secs(2), "shutdown cancels the write ({:?})", t0.elapsed());
}

#[test]
fn listing_jobs_and_availability() {
    let m = Mocks::standard();
    let src = r#"
        R = page.input("r", 0, { agent = true })
        U = page.job("u", "usb.list", { R })
        S = page.job("s", "serial.list", { R })
        A = page.derive("a", {}, function()
            return { ble = ble.available(), usb = usb.available(), serial = serial.available() }
        end)
    "#;
    let mut p = load(ALL, src, &natives(&m)).unwrap();
    let u = wait_for(&mut p, "u", |d| !d.items().is_empty());
    assert_eq!(u.items().len(), 3);
    assert_eq!(u.at(1).get("id").as_str(), Some("usb-loop"));
    let s = wait_for(&mut p, "s", |d| !d.items().is_empty());
    let paths: Vec<&str> = s.items().iter().map(|x| x.get("path").str_or("")).collect();
    assert_eq!(paths, vec!["MOCK0", "MOCK1"]);
    // a new input value lists again
    m.usb.set_present("usb-kbd", false);
    set(&mut p, "r", "1");
    wait_for(&mut p, "u", |d| d.items().len() == 2);
    let a = p.cell_value("a").unwrap();
    for module in ["ble", "usb", "serial"] {
        assert_eq!(a.get(module).get("ok").as_bool(), Some(true), "{module}: {a:?}");
    }
}

#[test]
fn empty_requests_and_long_input() {
    let m = Mocks::standard();
    let src = r#"
        Link = page.input("link", nil, { agent = true })
        L = page.stream("l", "ble.link", { request = Link, latest = true })
        C = page.input("c", nil, { agent = true })
        Ctl = page.job("ctl", "usb.control", { C })
        page.on("text", function(t) serial.send(t, "x") end)
    "#;
    let mut p = load(ALL, src, &natives(&m)).unwrap();
    // `{}` turns a stream's device off, like nil: no error
    set(&mut p, "link", &format!(r#"{{ "device": "{HR}" }}"#));
    wait_for(&mut p, "l", |d| last(d).get("state").as_str() == Some("connected"));
    set(&mut p, "link", "{}");
    let d = wait_for(&mut p, "l", |d| last(d).get("state").as_str() == Some("idle"));
    assert!(last(&d).get("error").is_nil(), "{d:?}");

    // page input is never echoed whole into errors
    let long = "z".repeat(100_000);
    set(&mut p, "c", &format!(r#"{{ "device": "{long}", "request": 1 }}"#));
    let d = wait_for(&mut p, "ctl", |d| job_error(d).is_some());
    let e = job_error(&d).unwrap();
    assert!(e.contains("\"device\" is too long") && e.len() < 200, "{}", &e[..e.len().min(300)]);
    set(&mut p, "c", &format!(r#"{{ "device": "usb-loop", "request": 1, "type": "{long}" }}"#));
    let d = wait_for(&mut p, "ctl", |d| job_error(d).is_some_and(|e| e.contains("\"type\"")));
    assert!(job_error(&d).unwrap().len() < 200);
    let e = load(ALL, "ble.uuid(string.rep('f', 100000))", &natives(&m)).err().unwrap();
    assert!(e.contains("not a UUID") && e.len() < 400, "{} bytes", e.len());
    p.text_input(&long);
    assert_eq!(p.handler_errors, 1);
}

#[test]
fn usb_effects_are_handlers_only() {
    let m = Mocks::standard();
    let mut p = load(ALL, "page.derive('x', {}, function() return usb.close('usb-loop') end)", &natives(&m)).unwrap();
    let r = p.act(r#"{"get": "x"}"#);
    assert!(r.contains("changes state"), "{r}");
}

#[test]
fn serial_stream_lines_writes_and_replug() {
    let m = Mocks::standard();
    let src = r#"
        Ports = page.derive("ports", {}, function() return serial.ports() end)
        Req = page.input("req", nil, { agent = true })
        S = page.stream("s", "serial.port", { request = Req, latest = true, baud = 9600 })
        W = page.input("w", nil, { agent = true })
        Wr = page.job("wr", "serial.write", { W })
        page.on("text", function(t) serial.send("MOCK0", t) end)
    "#;
    let mut p = load(ALL, src, &natives(&m)).unwrap();
    let ports = p.cell_value("ports").unwrap();
    assert_eq!(ports.items().iter().map(|x| x.get("path").str_or("").to_string()).collect::<Vec<_>>(), vec!["MOCK0", "MOCK1"]);
    assert_eq!(ports.at(1).get("type").as_str(), Some("usb"));
    assert_eq!(ports.at(1).get("vid").num(), Some(0x1209 as f64));

    set(&mut p, "req", r#"{ "path": "MOCK0", "keep_lines": 3, "keep_bytes": 8, "stop_bits": 2, "parity": "even" }"#);
    wait_for(&mut p, "s", |d| last(d).get("state").as_str() == Some("open"));
    let s = m.serial.settings("MOCK0").unwrap();
    assert_eq!((s.baud, s.stop_bits), (9600, 2), "options under the request");

    m.serial.inject("MOCK0", b"hello\r\nwor");
    let d = wait_for(&mut p, "s", |d| last(d).get("partial").as_str() == Some("wor"));
    assert_eq!(strs(last(&d).get("lines")), vec!["hello"]);
    assert_eq!(last(&d).get("rx").num(), Some(10.0));
    assert_eq!(last(&d).get("bytes").as_bytes(), Some(&b"llo\r\nwor"[..]), "the last 8 bytes");
    m.serial.inject("MOCK0", "ld\n字\n".as_bytes());
    let d = wait_for(&mut p, "s", |d| last(d).get("lines").items().len() == 3);
    assert_eq!(strs(last(&d).get("lines")), vec!["hello", "world", "字"]);
    m.serial.inject("MOCK0", b"4\n5\n");
    let d = wait_for(&mut p, "s", |d| strs(last(d).get("lines")).last().map(String::as_str) == Some("5"));
    assert_eq!(strs(last(&d).get("lines")), vec!["字", "4", "5"], "keep_lines = 3");

    // serial.send from a handler: the loopback port echoes it
    p.text_input("AT\r\n");
    let d = wait_for(&mut p, "s", |d| strs(last(d).get("lines")).last().map(String::as_str) == Some("AT"));
    assert_eq!(last(&d).get("tx").num(), Some(4.0));
    assert_eq!(m.serial.written("MOCK0"), b"AT\r\n");
    // a job write to the port the stream holds
    set(&mut p, "w", r#"{ "path": "MOCK0", "data": "OK\n" }"#);
    let d = wait_for(&mut p, "wr", |d| d.get("written").num().is_some());
    assert_eq!(d.get("written").num(), Some(3.0));
    wait_for(&mut p, "s", |d| strs(last(d).get("lines")).last().map(String::as_str) == Some("OK") && last(d).get("tx").num() == Some(7.0));
    // a job write to a port no stream holds opens it just for the write
    set(&mut p, "w", r#"{ "path": "MOCK1", "data": [7] }"#);
    wait_for(&mut p, "wr", |d| d.get("written").num() == Some(1.0));
    assert_eq!(m.serial.written("MOCK1"), vec![7]);
    assert!(!m.serial.is_open("MOCK1"));
    set(&mut p, "w", r#"{ "path": "COM99", "data": "x" }"#);
    let d = wait_for(&mut p, "wr", |d| job_error(d).is_some());
    assert_eq!(job_error(&d), Some("serial.write: COM99: the port does not exist"));

    // unplugged: closed with the error, reopened when it is back
    m.serial.set_present("MOCK0", false);
    let d = wait_for(&mut p, "s", |d| last(d).get("state").as_str() == Some("closed"));
    assert!(last(&d).get("error").as_str().unwrap().contains("unplugged"));
    m.serial.set_present("MOCK0", true);
    let d = wait_for(&mut p, "s", |d| last(d).get("state").as_str() == Some("open"));
    assert!(last(&d).get("error").is_nil());
    assert_eq!(strs(last(&d).get("lines")).len(), 3, "the same port keeps its history");

    // closing
    set(&mut p, "req", "null");
    wait_for(&mut p, "s", |d| last(d).get("state").as_str() == Some("idle"));
    std::thread::sleep(Duration::from_millis(50));
    assert!(!m.serial.is_open("MOCK0"));
    p.text_input("late");
    assert_eq!(p.handler_errors, 1, "serial.send to a closed port fails in the handler");
    // a port that is not there
    set(&mut p, "req", r#"{ "path": "COM99", "reconnect": false }"#);
    let d = wait_for(&mut p, "s", |d| last(d).get("state").as_str() == Some("failed"));
    assert_eq!(last(&d).get("error").as_str(), Some("COM99: the port does not exist"));
}

#[test]
fn serial_over_cdc_acm() {
    let usb = MockUsb::new(vec![MockUsbDevice::cdc_acm("acm0"), MockUsbDevice::vendor("other")]);
    let ub: Arc<dyn anypage_devices::UsbBackend> = Arc::new(usb.clone());
    let mut n = Natives::new();
    register_with(&mut n, Backends { usb: Some(ub.clone()), serial: Some(Arc::new(CdcSerial::new(ub))), ..Default::default() });
    let src = r#"
        Ports = page.derive("ports", {}, function() return serial.ports() end)
        Req = page.input("req", nil, { agent = true })
        S = page.stream("s", "serial.port", { request = Req, latest = true })
        page.on("text", function(t) serial.send("usb:acm0", t) end)
    "#;
    let mut p = load(r#"{ "modules": ["usb", "serial"] }"#, src, &n).unwrap();
    let ports = p.cell_value("ports").unwrap();
    assert_eq!(ports.items().len(), 1, "only CDC-ACM devices are serial ports");
    assert_eq!(ports.at(1).get("path").as_str(), Some("usb:acm0"));
    assert_eq!(ports.at(1).get("product").as_str(), Some("Uno"));

    set(&mut p, "req", r#"{ "path": "usb:acm0", "baud": 57600, "data_bits": 7, "parity": "odd", "dtr": true, "rts": false }"#);
    wait_for(&mut p, "s", |d| last(d).get("state").as_str() == Some("open"));
    assert_eq!(usb.line_coding("acm0"), Some(vec![0x00, 0xe1, 0x00, 0x00, 0, 1, 7]));
    assert_eq!(usb.control_lines("acm0"), Some(1));
    p.text_input("ping\nmore than one packet of data over a sixty-four byte bulk endpoint, looped\n");
    let d = wait_for(&mut p, "s", |d| last(d).get("lines").items().len() == 2);
    assert_eq!(strs(last(&d).get("lines"))[0], "ping");
    assert!(strs(last(&d).get("lines"))[1].ends_with("looped"));
    // unplugging the USB device closes the port
    usb.set_present("acm0", false);
    let d = wait_for(&mut p, "s", |d| last(d).get("state").as_str() == Some("closed"));
    assert!(last(&d).get("error").as_str().unwrap().contains("disconnected"));
    set(&mut p, "req", r#"{ "path": "COM1" }"#);
    let d = wait_for(&mut p, "s", |d| last(d).get("error").as_str().is_some_and(|e| e.contains("usb:<device id>")));
    assert!(d.get("last").get("state").as_str().is_some());
}

#[test]
fn agents_see_device_cells() {
    let m = Mocks::standard();
    let src = r#"
        Req = page.input("req", { on = true, interval_ms = 30 }, { agent = true })
        Scan = page.stream("scan", "ble.scan", { request = Req, latest = true })
    "#;
    let mut p = load(ALL, src, &natives(&m)).unwrap();
    wait_for(&mut p, "scan", |d| last(d).get("count").num() == Some(2.0));
    let v: serde_json::Value = serde_json::from_str(&p.act(r#"{"get": "scan"}"#)).unwrap();
    assert_eq!(v["value"]["last"]["count"], 2);
    assert_eq!(v["value"]["last"]["scanning"], true);
    // the runtime summarises deep values for agents
    assert_eq!(v["value"]["last"]["devices"].as_array().map(Vec::len), Some(2));
    let r: serde_json::Value = serde_json::from_str(&p.act(r#"{"set": "req", "value": {"on": false}}"#)).unwrap();
    assert_eq!(r["ok"], true);
    wait_for(&mut p, "scan", |d| last(d).get("scanning").as_bool() == Some(false));
}
