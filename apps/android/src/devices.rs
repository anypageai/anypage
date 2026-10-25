//! What crosses between Kotlin's `DeviceBridge` and the device module
//! (`anypage_devices::android`), in plain text so the JNI layer stays small:
//! USB device lists and GATT service tables come as lines of tab-separated
//! fields, errors as a line starting with `!`.
//!
//! ```text
//! D <id> <vid> <pid> <class> <subclass> <protocol> <manufacturer> <product> <serial>
//! I <number> <alt> <class> <subclass> <protocol> <name>
//! E <address> <type 0 control, 1 iso, 2 bulk, 3 interrupt> <max packet>
//! S <service uuid>
//! C <characteristic uuid> <properties>
//! ! <message>
//! ```
//!
//! `I` lines belong to the `D` line before them and `E` lines to the `I`
//! line before them; `C` lines to the `S` line before them. Kotlin replaces
//! tabs and line breaks inside names with spaces. Empty names are absent.

use anypage_devices::android::{AndroidGattCharacteristic, AndroidGattService};
use anypage_devices::usb::{EpKind, UsbDeviceInfo, UsbEndpointInfo, UsbInterfaceInfo};

/// Devices in one list (a hub tree is rarely more than a few dozen).
const MAX_DEVICES: usize = 256;
const MAX_SERVICES: usize = 512;
const MAX_FIELD: usize = 512;

fn field(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut end = s.len().min(MAX_FIELD);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    Some(s[..end].to_string())
}

fn num<T: std::str::FromStr>(s: Option<&&str>) -> Option<T> {
    s.and_then(|s| s.trim().parse().ok())
}

/// An error line (`!\t<message>`) anywhere in Kotlin's answer.
fn error_line(text: &str) -> Option<String> {
    text.lines().find_map(|l| l.strip_prefix('!').map(|m| m.trim_start_matches('\t').trim().to_string())).map(|m| if m.is_empty() { "the device bridge failed".into() } else { m })
}

/// `/dev/bus/usb/001/004` -> ("1", 4).
pub fn bus_address(device_name: &str) -> (String, u8) {
    let mut parts = device_name.rsplit('/');
    let addr = parts.next().and_then(|a| a.parse::<u16>().ok()).unwrap_or(0).min(255) as u8;
    let bus = parts.next().and_then(|b| b.parse::<u32>().ok()).map(|b| b.to_string()).unwrap_or_default();
    (bus, addr)
}

/// Kotlin's USB device list.
pub fn parse_usb_list(text: &str) -> Result<Vec<UsbDeviceInfo>, String> {
    if let Some(e) = error_line(text) {
        return Err(e);
    }
    let mut out: Vec<UsbDeviceInfo> = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.first().copied() {
            Some("D") => {
                if out.len() >= MAX_DEVICES {
                    break;
                }
                let Some(id) = f.get(1).and_then(|s| field(s)) else { continue };
                let (bus, address) = bus_address(&id);
                out.push(UsbDeviceInfo {
                    vid: num(f.get(2)).unwrap_or(0),
                    pid: num(f.get(3)).unwrap_or(0),
                    class: num(f.get(4)).unwrap_or(0),
                    subclass: num(f.get(5)).unwrap_or(0),
                    protocol: num(f.get(6)).unwrap_or(0),
                    manufacturer: f.get(7).and_then(|s| field(s)),
                    product: f.get(8).and_then(|s| field(s)),
                    serial: f.get(9).and_then(|s| field(s)),
                    bus,
                    address,
                    id,
                    ..Default::default()
                });
            }
            Some("I") => {
                let Some(d) = out.last_mut() else { continue };
                if d.interfaces.len() >= 64 {
                    continue;
                }
                d.interfaces.push(UsbInterfaceInfo {
                    number: num(f.get(1)).unwrap_or(0),
                    alt: num(f.get(2)).unwrap_or(0),
                    class: num(f.get(3)).unwrap_or(0),
                    subclass: num(f.get(4)).unwrap_or(0),
                    protocol: num(f.get(5)).unwrap_or(0),
                    name: f.get(6).and_then(|s| field(s)),
                    endpoints: Vec::new(),
                });
            }
            Some("E") => {
                let Some(i) = out.last_mut().and_then(|d| d.interfaces.last_mut()) else { continue };
                let kind = match num::<u8>(f.get(2)) {
                    Some(0) => EpKind::Control,
                    Some(1) => EpKind::Isochronous,
                    Some(2) => EpKind::Bulk,
                    Some(3) => EpKind::Interrupt,
                    _ => continue,
                };
                if i.endpoints.len() < 32 {
                    i.endpoints.push(UsbEndpointInfo { address: num(f.get(1)).unwrap_or(0), kind, max_packet: num(f.get(3)).unwrap_or(0) });
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Kotlin's answer to `usbOpen`: a file descriptor, or an error line.
pub fn parse_fd(answer: &str) -> Result<i32, String> {
    if let Some(e) = error_line(answer) {
        return Err(e);
    }
    match answer.trim().parse::<i32>() {
        Ok(fd) if fd >= 0 => Ok(fd),
        _ => Err(format!("the USB device could not be opened (answer \"{}\")", answer.chars().take(40).collect::<String>())),
    }
}

/// The GATT table Kotlin read after `onServicesDiscovered`.
pub fn parse_services(text: &str) -> Vec<AndroidGattService> {
    let mut out: Vec<AndroidGattService> = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.first().copied() {
            Some("S") if out.len() < MAX_SERVICES => {
                if let Some(uuid) = f.get(1).and_then(|s| field(s)) {
                    out.push(AndroidGattService { uuid, characteristics: Vec::new() });
                }
            }
            Some("C") => {
                if let (Some(s), Some(uuid)) = (out.last_mut(), f.get(1).and_then(|s| field(s))) {
                    if s.characteristics.len() < 256 {
                        s.characteristics.push(AndroidGattCharacteristic { uuid, properties: num(f.get(2)).unwrap_or(0) });
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Kotlin answers "could not start" with a message and "started" with null.
pub fn started(answer: Option<String>) -> Result<(), String> {
    match answer {
        None => Ok(()),
        Some(m) if m.trim().is_empty() => Err("the device bridge refused the request".into()),
        Some(m) => Err(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "D\t/dev/bus/usb/001/004\t9025\t67\t2\t0\t0\tArduino (www.arduino.cc)\t\t8573531\n\
I\t0\t0\t2\t2\t1\t\n\
E\t130\t3\t8\n\
I\t1\t0\t10\t0\t0\tCDC Data\n\
E\t4\t2\t64\n\
E\t131\t2\t64\n\
E\t9\t7\t64\n\
D\t/dev/bus/usb/002/011\t1155\t22336\t0\t0\t0\t\tSTM32 Virtual\t\n";

    #[test]
    fn usb_lists() {
        let d = parse_usb_list(LIST).unwrap();
        assert_eq!(d.len(), 2);
        let a = &d[0];
        assert_eq!((a.id.as_str(), a.vid, a.pid, a.class), ("/dev/bus/usb/001/004", 0x2341, 0x43, 2));
        assert_eq!((a.bus.as_str(), a.address), ("1", 4));
        assert_eq!(a.manufacturer.as_deref(), Some("Arduino (www.arduino.cc)"));
        assert_eq!((a.product.clone(), a.serial.as_deref()), (None, Some("8573531")));
        assert_eq!(a.interfaces.len(), 2);
        assert_eq!(a.interfaces[0].endpoints, vec![UsbEndpointInfo { address: 0x82, kind: EpKind::Interrupt, max_packet: 8 }]);
        assert_eq!(a.interfaces[1].name.as_deref(), Some("CDC Data"));
        assert_eq!(a.interfaces[1].endpoints.len(), 2, "an endpoint of an unknown type is skipped");
        assert_eq!(a.interfaces[1].endpoints[1].kind, EpKind::Bulk);
        assert_eq!((d[1].bus.as_str(), d[1].address, d[1].manufacturer.clone()), ("2", 11, None));
        assert!(parse_usb_list("").unwrap().is_empty());
        assert_eq!(parse_usb_list("!\tthis device has no USB host support").unwrap_err(), "this device has no USB host support");
        // stray lines and garbage are ignored, not fatal
        assert_eq!(parse_usb_list("I\t0\nE\t1\t2\t3\nD\t\nD\tx\tnot-a-number\nzzz").unwrap().len(), 1);
        let long = format!("D\t/dev/bus/usb/001/001\t1\t2\t0\t0\t0\t{}\n", "é".repeat(1000));
        let m = parse_usb_list(&long).unwrap()[0].manufacturer.clone().unwrap();
        assert!(m.len() <= MAX_FIELD && m.starts_with('é'));
    }

    #[test]
    fn bus_addresses() {
        assert_eq!(bus_address("/dev/bus/usb/003/127"), ("3".into(), 127));
        assert_eq!(bus_address("weird"), (String::new(), 0));
    }

    #[test]
    fn descriptors_and_errors() {
        assert_eq!(parse_fd("37"), Ok(37));
        assert_eq!(parse_fd("!\tpermission to use /dev/bus/usb/001/004 was denied").unwrap_err(), "permission to use /dev/bus/usb/001/004 was denied");
        assert!(parse_fd("-1").is_err());
        assert!(parse_fd("").is_err());
        assert_eq!(started(None), Ok(()));
        assert_eq!(started(Some("Bluetooth is off".into())).unwrap_err(), "Bluetooth is off");
        assert!(started(Some(" ".into())).is_err());
    }

    #[test]
    fn gatt_tables() {
        let s = parse_services("S\t0000180d-0000-1000-8000-00805f9b34fb\nC\t00002a37-0000-1000-8000-00805f9b34fb\t16\nC\t00002a38-0000-1000-8000-00805f9b34fb\t2\nS\t00001800-0000-1000-8000-00805f9b34fb\nC\tbad\tx\n");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].characteristics.len(), 2);
        assert_eq!(s[0].characteristics[0].properties, 16);
        assert_eq!(s[1].characteristics[0].properties, 0);
        assert!(parse_services("C\tx\t1\n").is_empty(), "a characteristic before any service is dropped");
    }
}
