//! The real backends on this machine.
//!
//! `listing_works` runs always: listing USB devices and serial ports must
//! work with nothing attached. `enumerate_this_machine` scans Bluetooth and
//! opens USB devices to read their descriptors (claiming only WinUSB-bound
//! ones), so it is ignored by default:
//! `cargo test --test hardware -- --ignored --nocapture`.

#[path = "support/enumerate.rs"]
mod enumerate;

use std::time::{Duration, Instant};

use anypage_devices::Backends;

#[test]
fn listing_works() {
    let b = Backends::platform();
    if cfg!(any(windows, target_os = "android")) {
        assert!(b.ble.is_some() && b.usb.is_some() && b.serial.is_some());
    }
    if let Some(u) = &b.usb {
        let t0 = Instant::now();
        let list = u.devices().expect("USB devices can be listed");
        eprintln!("{} USB devices in {:.1} ms", list.len(), t0.elapsed().as_secs_f64() * 1000.0);
        for d in &list {
            assert!(!d.id.is_empty());
        }
        assert!(u.open("no-such-device", &anypage_devices::Wait::timeout_only(Duration::from_secs(1))).err().unwrap().contains("no USB device"));
    }
    if let Some(s) = &b.serial {
        let t0 = Instant::now();
        let ports = s.ports().expect("serial ports can be listed");
        eprintln!("{} serial ports in {:.1} ms", ports.len(), t0.elapsed().as_secs_f64() * 1000.0);
        let e = s.open("COM255", &Default::default(), &anypage_devices::Wait::timeout_only(Duration::from_secs(1))).err();
        if cfg!(windows) {
            assert!(e.unwrap().starts_with("COM255: "), "a clear error for a missing port");
        }
    }
}

/// The real Bluetooth stack on a device that is not there: a clear error,
/// promptly (WinRT refuses an address it has never seen at once; a known
/// but unreachable device would instead run to the timeout or the cancel).
/// No real device is contacted.
#[test]
#[ignore = "uses this machine's Bluetooth adapter"]
fn ble_connect_to_an_absent_device_gives_up() {
    let Some(ble) = Backends::platform().ble else { return };
    let quiet: anypage_devices::ble::Emit<anypage_devices::ble::LinkEvent> = std::sync::Arc::new(|_| {});
    for (label, timeout) in [("5 s timeout", 5u64), ("60 s timeout, cancelled after 300 ms", 60)] {
        let t0 = Instant::now();
        let cancel = || timeout == 60 && t0.elapsed() > Duration::from_millis(300);
        let r = ble.connect("00:00:00:00:00:09", quiet.clone(), &anypage_devices::Wait::new(Duration::from_secs(timeout), &cancel));
        let e = r.err().unwrap_or_default();
        println!("{label}: {e:?} after {:?}", t0.elapsed());
        assert!(e.contains("not found") || e.contains("timed out") || e.contains("cancelled") || e.contains("Bluetooth"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(if timeout == 5 { 7 } else { 1 }), "{label}: {:?}", t0.elapsed());
    }
}

#[test]
#[ignore = "scans Bluetooth and opens USB devices on this machine"]
fn enumerate_this_machine() {
    let r = enumerate::enumerate(Duration::from_secs(6));
    println!("{}", r.text);
    r.usb.expect("USB listing");
    r.serial.expect("serial listing");
    // Bluetooth may be off or missing; the report says so
    if let Err(e) = &r.ble {
        println!("Bluetooth LE not scanned: {e}");
    }
}
