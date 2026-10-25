//! pages/devices-demo, loaded and used headlessly on simulated devices:
//! clicks, typing and agent actions, with the panels rendered, written as
//! PNG under target/test-output/ and checked pixel by pixel.

mod common;

use std::path::{Path, PathBuf};

use anypage_devices::mock::{Mocks, UART_TX};
use anypage_devices::register_with;
use anypage_engine::{Image, Page, PointerResponse};
use anypage_runtime::{Natives, Package, ScriptPage};
use common::*;

fn package() -> Package {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pages/devices-demo");
    Package::from_dir(&dir).expect("the demo package")
}

fn load_demo(n: &Natives) -> ScriptPage {
    ScriptPage::load(&package(), n, text(), wake()).expect("the demo loads")
}

fn out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-output")
}

/// Rasterise one panel at scale 1 (also refreshes the page's hit boxes).
fn render(p: &mut ScriptPage, id: u32) -> (u32, u32, Vec<u8>) {
    let cell = p.spec().panels.iter().find(|x| x.id == id).expect("panel").cell;
    let g = p.parts().0;
    g.commit();
    let i = g.demand(cell);
    let img = i.get::<Image>().expect("a panel image");
    (img.w, img.h, img.rgba.clone())
}

fn save(name: &str, img: &(u32, u32, Vec<u8>)) {
    write_png(&out_dir().join(format!("devices-demo-{name}.png")), img.0, img.1, &img.2).unwrap();
}

fn px(img: &(u32, u32, Vec<u8>), x: u32, y: u32) -> [u8; 4] {
    let i = ((y * img.0 + x) * 4) as usize;
    [img.2[i], img.2[i + 1], img.2[i + 2], img.2[i + 3]]
}

fn near(a: [u8; 4], b: [u8; 3], tol: i32) -> bool {
    (0..3).all(|k| (a[k] as i32 - b[k] as i32).abs() <= tol)
}

/// Pixels close to `c` inside the panel-local rectangle.
fn count(img: &(u32, u32, Vec<u8>), r: (u32, u32, u32, u32), c: [u8; 3], tol: i32) -> usize {
    let mut n = 0;
    for y in r.1..(r.1 + r.3).min(img.1) {
        for x in r.0..(r.0 + r.2).min(img.0) {
            if near(px(img, x, y), c, tol) {
                n += 1;
            }
        }
    }
    n
}

const GOOD: [u8; 3] = [80, 220, 120];
const TEXT_LO: [u8; 3] = [140, 148, 164];
const HEART: [u8; 3] = [255, 90, 110];
const PANEL_BG: [u8; 3] = [22, 25, 32];
// alpha(ACCENT, 70) over the panel background
const SELECTED: [u8; 3] = [41, 62, 93];

#[test]
fn demo_on_simulated_devices() {
    let m = Mocks::standard();
    let mut n = Natives::new();
    register_with(&mut n, m.backends());
    let mut p = load_demo(&n);
    assert_eq!(p.spec().panels.len(), 5);
    eprintln!("{}", p.summary());

    // ---- Bluetooth: an agent starts the scan, a click connects
    let r = p.act(r#"{"action": "scan", "params": {"on": true}}"#);
    assert!(r.contains("\"scanning\":true"), "{r}");
    wait_for(&mut p, "scan", |d| last(d).get("count").num() == Some(2.0));
    let ble = render(&mut p, 1);
    assert!(near(px(&ble, 28, 64), GOOD, 8), "the scanning dot is green: {:?}", px(&ble, 28, 64));
    // first row: the heart-rate monitor, seen first
    assert_eq!(p.pointer_down(200.0, 200.0), PointerResponse::Handled);
    wait_for(&mut p, "link", |d| last(d).get("state").as_str() == Some("connected"));
    wait_for(&mut p, "rates", |d| d.items().len() >= 8);
    let ble = render(&mut p, 1);
    save("ble", &ble);
    assert!(near(px(&ble, 262, 92 + 28), SELECTED, 6), "the connected row is highlighted: {:?}", px(&ble, 262, 120));
    assert!(near(px(&ble, 262, 92 + 38 + 28), PANEL_BG, 4), "the beacon row is not");

    // read the name and ping the UART with the panel's buttons
    assert_eq!(p.pointer_down(100.0, 990.0), PointerResponse::Handled);
    let d = wait_for(&mut p, "read", |d| d.get("text").as_str().is_some());
    assert_eq!(d.get("text").as_str(), Some("AnyPage HR"));
    // ping is an effect (ble.send): it goes out at once, no panel has to ask for it
    assert_eq!(p.pointer_down(300.0, 990.0), PointerResponse::Handled);
    let d = wait_for(&mut p, "link", |d| last(d).get("latest").get(UART_TX).as_bytes() == Some(&b"ping 2"[..]));
    assert_eq!(last(&d).get("sent").num(), Some(1.0));
    assert_eq!(m.ble.value("C0:FF:EE:00:00:01", anypage_devices::ble::parse_uuid(anypage_devices::mock::UART_RX).unwrap()), Some(b"ping 2".to_vec()));
    let link = render(&mut p, 2);
    save("link", &link);
    let heart = count(&link, (24, 186, 732, 150), HEART, 30);
    assert!(heart > 300, "the heart-rate series is drawn ({heart} px)");
    assert!(count(&link, (24, 126, 80, 50), HEART, 40) > 40, "the current rate is shown");
    assert!(count(&link, (24, 340, 360, 60), GOOD, 40) > 40, "the read and write results are shown");
    assert!(near(px(&link, 28, 66), GOOD, 8), "the link state dot is green");

    // ---- USB: listed by a job; a click opens the first device and lists its endpoints
    wait_for(&mut p, "usb_list", |d| d.items().len() == 3);
    let usb = render(&mut p, 3);
    assert!(count(&usb, (24, 88, 400, 40), [232, 236, 244], 20) > 50, "a device row");
    assert_eq!(p.pointer_down(1000.0, 200.0), PointerResponse::Handled);
    let d = wait_for(&mut p, "usb_info", |d| d.get("interfaces").items().len() == 1);
    assert_eq!(d.get("product").as_str(), Some("Loopback"));
    assert_eq!(m.usb.handles("usb-loop"), 1, "the page holds the device open");
    let usb = render(&mut p, 3);
    save("usb", &usb);
    assert!(near(px(&usb, 400, 92 + 28), SELECTED, 6), "the open device row is highlighted: {:?}", px(&usb, 400, 120));
    assert!(count(&usb, (24, 330, 732, 96), GOOD, 40) > 100, "interfaces and endpoints are listed");
    // a device owned by another driver: the error is shown, not a crash
    let r = p.act(r#"{"action": "usb_open", "params": {"device": "usb-kbd"}}"#);
    assert!(r.contains("\"ok\":true"), "{r}");
    let d = wait_for(&mut p, "usb_info", |d| job_error(d).is_some());
    assert!(job_error(&d).unwrap().contains("HidUsb"));
    assert_eq!(m.usb.handles("usb-loop"), 0, "selecting another device closed the first (usb.close)");
    let usb = render(&mut p, 3);
    assert!(count(&usb, (24, 330, 732, 96), [255, 90, 90], 40) > 100, "the error is drawn in red");

    // ---- serial: click a port, type, Enter
    wait_for(&mut p, "ports", |d| d.items().len() == 2);
    let serial = render(&mut p, 4);
    assert!(count(&serial, (24, 56, 160, 30), [232, 236, 244], 20) > 30, "the first port is listed");
    assert_eq!(p.pointer_down(1000.0, 630.0), PointerResponse::Handled);
    wait_for(&mut p, "port", |d| last(d).get("state").as_str() == Some("open"));
    p.text_input("hello demo");
    assert!(p.key("enter"));
    let d = wait_for(&mut p, "port", |d| last(d).get("lines").items().len() == 1);
    assert_eq!(strs(last(&d).get("lines")), vec!["hello demo"]);
    assert_eq!(m.serial.written("MOCK0"), b"hello demo\r\n");
    let r = p.act(r#"{"action": "send", "params": {"text": "via agent\n"}}"#);
    assert!(r.contains("\"sent\":10"), "{r}");
    wait_for(&mut p, "port", |d| last(d).get("lines").items().len() == 2);
    let serial = render(&mut p, 4);
    save("serial", &serial);
    let green = count(&serial, (26, 228, 728, 176), GOOD, 60);
    assert!(green > 150, "received lines in the terminal ({green} px)");
    assert!(near(px(&serial, 28, 180), GOOD, 8), "the port state dot is green");

    // ---- the semantic port lists what an agent can do
    let desc: serde_json::Value = serde_json::from_str(&p.describe()).unwrap();
    let names: Vec<&str> = desc["actions"].as_array().unwrap().iter().filter_map(|a| a["name"].as_str()).collect();
    assert_eq!(names, vec!["scan", "connect", "disconnect", "read_name", "ping", "usb_open", "open_port", "close_port", "send"]);

    // ---- closing everything with the buttons and actions
    assert_eq!(p.pointer_down(600.0, 990.0), PointerResponse::Handled);
    wait_for(&mut p, "link", |d| last(d).get("state").as_str() == Some("idle"));
    assert_eq!(p.pointer_down(1500.0, 1000.0), PointerResponse::Handled);
    wait_for(&mut p, "port", |d| last(d).get("state").as_str() == Some("idle"));
    p.act(r#"{"action": "scan", "params": {"on": false}}"#);
    wait_for(&mut p, "scan", |d| last(d).get("scanning").as_bool() == Some(false));
    let ble = render(&mut p, 1);
    assert!(near(px(&ble, 28, 64), TEXT_LO, 8), "the scanning dot is grey again");
    let title = render(&mut p, 0);
    save("title", &title);
    assert!(count(&title, (0, 0, 1600, 80), [232, 236, 244], 20) > 500, "the title is drawn");
    // a device the panel has drawn (the script holds its handle) is still
    // released when the page goes
    p.act(r#"{"action": "usb_open", "params": {"device": "usb-loop"}}"#);
    wait_for(&mut p, "usb_info", |d| d.get("interfaces").items().len() == 1);
    let _ = render(&mut p, 3);
    assert_eq!(m.usb.handles("usb-loop"), 1);
    p.shutdown();
    drop(p);
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(m.ble.open_links(), 0);
    assert!(!m.serial.is_open("MOCK0"));
    assert_eq!(m.usb.handles("usb-loop"), 0, "closing the page released the USB device");
}

#[test]
fn demo_without_device_modules() {
    let mut p = load_demo(&Natives::new());
    assert!(p.cell_id("scan").is_none());
    let ble = render(&mut p, 1);
    save("ble-missing", &ble);
    // the note is drawn where the device list would be, no button
    assert!(count(&ble, (20, 70, 400, 30), TEXT_LO, 30) > 30, "the note is drawn");
    assert!(count(&ble, (560, 14, 200, 40), [232, 236, 244], 30) < 5, "no scan button");
    assert_eq!(p.pointer_down(100.0, 200.0), PointerResponse::Ignored);
}

#[test]
fn demo_on_this_machine_loads() {
    // the real backends: listing runs as jobs, off the evaluating thread,
    // within the default limits; nothing is opened
    let mut n = Natives::new();
    anypage_devices::register(&mut n);
    let t0 = std::time::Instant::now();
    let mut p = load_demo(&n);
    eprintln!("loaded in {:.1} ms: {}", t0.elapsed().as_secs_f64() * 1000.0, p.summary());
    let _ = render(&mut p, 3);
    let _ = render(&mut p, 4);
    if cfg!(windows) {
        let t0 = std::time::Instant::now();
        let usb = wait_for(&mut p, "usb_list", |d| d.get("state").as_str() != Some("pending"));
        assert!(job_error(&usb).is_none(), "{usb:?}");
        let ports = wait_for(&mut p, "ports", |d| d.get("state").as_str() != Some("pending"));
        assert!(job_error(&ports).is_none(), "{ports:?}");
        eprintln!("{} USB devices and {} serial ports listed by jobs in {:.1} ms", usb.items().len(), ports.items().len(), t0.elapsed().as_secs_f64() * 1000.0);
        // the panels draw the lists
        let _ = render(&mut p, 3);
        let _ = render(&mut p, 4);
        assert_eq!(p.handler_errors, 0);
    }
}
