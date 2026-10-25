//! What this machine has: USB devices (with their endpoints where the
//! device can be opened), serial ports, and Bluetooth LE devices heard
//! during a short scan. Used by `examples/enumerate.rs` and the ignored
//! `hardware` test.
//!
//! Nothing is written. Only devices bound to WinUSB (Windows), which no
//! other driver uses, have their first interface claimed briefly to show
//! that transfers would work; every other device (keyboards, mice, hubs,
//! cameras, storage) is only opened to read its descriptors, never
//! claimed, so no driver is detached or disturbed.

use std::sync::mpsc::channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anypage_devices::ble::{uuid_string, BleAdvert, ScanEvent};
use anypage_devices::{Backends, Wait};

pub struct Report {
    pub text: String,
    pub usb: Result<usize, String>,
    pub serial: Result<usize, String>,
    pub ble: Result<usize, String>,
}

pub fn enumerate(scan: Duration) -> Report {
    let b = Backends::platform();
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };

    let usb = match &b.usb {
        None => Err("no USB backend on this platform".to_string()),
        Some(u) => {
            let t0 = Instant::now();
            match u.devices() {
                Err(e) => Err(e),
                Ok(list) => {
                    line(format!("USB: {} devices (listed in {:.1} ms)", list.len(), t0.elapsed().as_secs_f64() * 1000.0));
                    for d in &list {
                        line(format!(
                            "  {:04x}:{:04x} {:<28} class {:02x} speed {:<5} driver {:<10} {}",
                            d.vid,
                            d.pid,
                            d.product.clone().or(d.manufacturer.clone()).unwrap_or_else(|| "(no name)".into()),
                            d.class,
                            d.speed.unwrap_or("?"),
                            d.driver.clone().unwrap_or_default(),
                            d.id
                        ));
                        let ifs: Vec<String> = d.interfaces.iter().map(|i| format!("{}:{:02x}/{:02x}/{:02x}", i.number, i.class, i.subclass, i.protocol)).collect();
                        if !ifs.is_empty() {
                            line(format!("      interfaces {}", ifs.join(" ")));
                        }
                        match u.open(&d.id, &Wait::timeout_only(Duration::from_secs(2))) {
                            Ok(h) => {
                                for i in h.interfaces() {
                                    let eps: Vec<String> = i.endpoints.iter().map(|e| format!("0x{:02x} {} {}", e.address, e.kind.name(), e.max_packet)).collect();
                                    line(format!("      opened: interface {} alt {} endpoints [{}]", i.number, i.alt, eps.join(", ")));
                                }
                                // claiming is what transfers need: try it only where
                                // no other driver can be disturbed, and let go at once
                                let winusb = d.driver.as_deref().is_some_and(|x| x.eq_ignore_ascii_case("winusb"));
                                match h.interfaces().first() {
                                    Some(first) if winusb => match h.claim(first.number) {
                                        Ok(()) => {
                                            line(format!("      interface {} can be claimed", first.number));
                                            h.release(first.number);
                                        }
                                        Err(e) => line(format!("      claim: {e}")),
                                    },
                                    Some(_) => line(format!("      not claimed: bound to {} (transfers need WinUSB)", d.driver.as_deref().unwrap_or("another driver"))),
                                    None => {}
                                }
                            }
                            Err(e) => line(format!("      open: {e}")),
                        }
                    }
                    Ok(list.len())
                }
            }
        }
    };

    let serial = match &b.serial {
        None => Err("no serial backend on this platform".to_string()),
        Some(s) => match s.ports() {
            Err(e) => Err(e),
            Ok(ports) => {
                line(format!("serial: {} ports", ports.len()));
                for p in &ports {
                    let ids = match (p.vid, p.pid) {
                        (Some(v), Some(i)) => format!("{v:04x}:{i:04x}"),
                        _ => String::new(),
                    };
                    line(format!("  {:<8} {:<9} {:<9} {}", p.path, p.kind, ids, p.product.clone().unwrap_or_default()));
                }
                Ok(ports.len())
            }
        },
    };

    let ble = match &b.ble {
        None => Err("no Bluetooth backend on this platform".to_string()),
        Some(ble) => {
            let (tx, rx) = channel();
            let t0 = Instant::now();
            match ble.scan(&[], Arc::new(move |e| {
                let _ = tx.send(e);
            })) {
                Err(e) => Err(e),
                Ok(guard) => {
                    let mut seen: Vec<BleAdvert> = Vec::new();
                    let mut failure = None;
                    while t0.elapsed() < scan {
                        match rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(ScanEvent::Seen(a)) => match seen.iter_mut().find(|s| s.id == a.id) {
                                Some(s) => {
                                    if a.name.is_some() {
                                        s.name = a.name;
                                    }
                                    s.rssi = a.rssi.or(s.rssi);
                                    for u in a.services {
                                        if !s.services.contains(&u) {
                                            s.services.push(u);
                                        }
                                    }
                                }
                                None => seen.push(a),
                            },
                            Ok(ScanEvent::Failed(e)) => {
                                failure = Some(e);
                                break;
                            }
                            Err(_) => {}
                        }
                    }
                    drop(guard);
                    match failure {
                        Some(e) => Err(e),
                        None => {
                            line(format!("Bluetooth LE: {} devices heard in {:.1} s", seen.len(), t0.elapsed().as_secs_f64()));
                            seen.sort_by_key(|a| -(a.rssi.unwrap_or(-127) as i32));
                            for a in &seen {
                                let services: Vec<String> = a.services.iter().map(|u| uuid_string(u)[4..8].to_string()).collect();
                                let companies: Vec<String> = a.manufacturer_data.iter().map(|(c, _)| format!("{c:04x}")).collect();
                                line(format!(
                                    "  {} {:>4} dBm {:<24} services [{}] manufacturer [{}]",
                                    a.id,
                                    a.rssi.map(|r| r.to_string()).unwrap_or_else(|| "?".into()),
                                    a.name.clone().unwrap_or_else(|| "(no name)".into()),
                                    services.join(" "),
                                    companies.join(" ")
                                ));
                            }
                            Ok(seen.len())
                        }
                    }
                }
            }
        }
    };
    for (what, r) in [("USB", &usb), ("serial", &serial), ("Bluetooth LE", &ble)] {
        if let Err(e) = r {
            line(format!("{what}: {e}"));
        }
    }
    Report { text: out, usb, serial, ble }
}
