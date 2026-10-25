//! List this machine's USB devices, serial ports and nearby Bluetooth LE
//! devices through the module's real backends. Nothing is written to any
//! device. Devices bound to WinUSB have an interface claimed briefly to
//! report whether transfers would work; devices owned by other drivers
//! (keyboards, mice, hubs, storage) are only opened, never claimed.
//!
//! `cargo run --example enumerate [-- <scan seconds, default 6>]`

#[path = "../tests/support/enumerate.rs"]
mod enumerate;

fn main() {
    let secs = std::env::args().nth(1).and_then(|s| s.parse::<f64>().ok()).unwrap_or(6.0).clamp(0.0, 120.0);
    let r = enumerate::enumerate(std::time::Duration::from_secs_f64(secs));
    print!("{}", r.text);
    let counts = [("USB devices", &r.usb), ("serial ports", &r.serial), ("Bluetooth LE devices", &r.ble)];
    let found: Vec<String> = counts.iter().filter_map(|(what, n)| n.as_ref().ok().map(|n| format!("{n} {what}"))).collect();
    println!("found {}", found.join(", "));
    if r.usb.is_err() || r.serial.is_err() {
        std::process::exit(1);
    }
}
