//! CDC-ACM ("USB serial", Communications Device Class, Abstract Control
//! Model) over any [`UsbBackend`]. On Android this is how `serial.*` works:
//! the host hands over the USB device and this driver speaks the class
//! protocol (USB CDC 1.2, PSTN subclass 1.2 section 6.3) itself.
//!
//! Ports are named "usb:<device id>". Vendor protocols (FTDI, CP210x,
//! CH34x, PL2303) are not CDC-ACM and are not listed.

use std::sync::Arc;
use std::time::Duration;

use super::{Parity, SerialBackend, SerialConn, SerialPortInfo, SerialSettings};
use crate::usb::{ControlKind, ControlSetup, EpKind, Recipient, UsbBackend, UsbDeviceInfo, UsbHandle, UsbInterfaceInfo};
use crate::util::Wait;

pub const CLASS_COMM: u8 = 0x02;
pub const SUBCLASS_ACM: u8 = 0x02;
pub const CLASS_DATA: u8 = 0x0a;
pub const SET_LINE_CODING: u8 = 0x20;
pub const GET_LINE_CODING: u8 = 0x21;
pub const SET_CONTROL_LINE_STATE: u8 = 0x22;

/// The 7-byte line coding structure: dwDTERate, bCharFormat, bParityType, bDataBits.
pub fn line_coding(s: &SerialSettings) -> [u8; 7] {
    let b = s.baud.to_le_bytes();
    let stop = if s.stop_bits == 2 { 2 } else { 0 };
    let parity = match s.parity {
        Parity::None => 0,
        Parity::Odd => 1,
        Parity::Even => 2,
    };
    [b[0], b[1], b[2], b[3], stop, parity, s.data_bits]
}

fn is_acm(d: &UsbDeviceInfo) -> bool {
    d.class == CLASS_COMM || d.interfaces.iter().any(|i| (i.class == CLASS_COMM && i.subclass == SUBCLASS_ACM) || i.class == CLASS_DATA)
}

pub struct CdcSerial {
    usb: Arc<dyn UsbBackend>,
}

impl CdcSerial {
    pub fn new(usb: Arc<dyn UsbBackend>) -> Self {
        Self { usb }
    }
}

/// The data interface and its bulk IN / OUT endpoints.
fn data_interface(ifs: &[UsbInterfaceInfo]) -> Option<(u8, u8, u8, u16)> {
    let pick = |i: &UsbInterfaceInfo| {
        let bulk_in = i.endpoints.iter().find(|e| e.kind == EpKind::Bulk && e.address & 0x80 != 0)?;
        let bulk_out = i.endpoints.iter().find(|e| e.kind == EpKind::Bulk && e.address & 0x80 == 0)?;
        Some((i.number, bulk_in.address, bulk_out.address, bulk_in.max_packet.max(1)))
    };
    ifs.iter().filter(|i| i.class == CLASS_DATA).find_map(pick).or_else(|| ifs.iter().find_map(pick))
}

impl SerialBackend for CdcSerial {
    fn ports(&self) -> Result<Vec<SerialPortInfo>, String> {
        Ok(self
            .usb
            .devices()?
            .into_iter()
            .filter(is_acm)
            .map(|d| SerialPortInfo {
                path: format!("usb:{}", d.id),
                kind: "usb",
                vid: Some(d.vid),
                pid: Some(d.pid),
                serial: d.serial,
                manufacturer: d.manufacturer,
                product: d.product,
            })
            .collect())
    }

    fn status(&self) -> Result<(), String> {
        self.usb.status()
    }

    fn open(&self, path: &str, settings: &SerialSettings, wait: &Wait) -> Result<Arc<dyn SerialConn>, String> {
        let id = path.strip_prefix("usb:").ok_or_else(|| format!("{}: USB serial ports are named \"usb:<device id>\" (see serial.ports())", crate::util::clip(path)))?;
        let h = self.usb.open(id, wait)?;
        let ifs = h.interfaces();
        let (data_if, ep_in, ep_out, packet) = data_interface(&ifs).ok_or_else(|| format!("{path}: no CDC data interface with bulk IN and OUT endpoints"))?;
        let comm_if = ifs.iter().find(|i| i.class == CLASS_COMM && i.subclass == SUBCLASS_ACM).map(|i| i.number);
        if let Some(c) = comm_if {
            h.claim(c)?;
        }
        h.claim(data_if)?;
        if let Some(c) = comm_if {
            let setup = |request, value| ControlSetup { kind: ControlKind::Class, recipient: Recipient::Interface, request, value, index: c as u16 };
            h.control_out(&setup(SET_LINE_CODING, 0), &line_coding(settings), Some(c), &wait.sub(Duration::from_secs(2)))
                .map_err(|e| format!("{path}: setting the line coding failed: {e}"))?;
            // DTR and RTS on unless asked otherwise: many boards only send while DTR is set
            let lines = settings.dtr.unwrap_or(true) as u16 | ((settings.rts.unwrap_or(true) as u16) << 1);
            h.control_out(&setup(SET_CONTROL_LINE_STATE, lines), &[], Some(c), &wait.sub(Duration::from_secs(2)))
                .map_err(|e| format!("{path}: setting DTR/RTS failed: {e}"))?;
        }
        Ok(Arc::new(CdcConn { h, data_if, ep_in, ep_out, packet: packet as usize }))
    }
}

struct CdcConn {
    h: Arc<dyn UsbHandle>,
    data_if: u8,
    ep_in: u8,
    ep_out: u8,
    packet: usize,
}

impl SerialConn for CdcConn {
    fn read(&self, max: usize, timeout: Duration) -> Result<Vec<u8>, String> {
        let len = max.max(self.packet).div_ceil(self.packet) * self.packet;
        Ok(self.h.read(self.data_if, self.ep_in, len, &Wait::timeout_only(timeout))?.unwrap_or_default())
    }

    fn write(&self, data: &[u8], wait: &Wait) -> Result<usize, String> {
        // in pieces of whole packets, so a write cut short by the wait
        // (cancelled, timed out) reports how far it got
        let piece = self.packet * 64;
        let mut done = 0;
        for chunk in data.chunks(piece.max(64)) {
            if wait.cancelled() || wait.remaining().is_zero() {
                break;
            }
            match self.h.write(self.data_if, self.ep_out, chunk, wait) {
                Ok(n) => {
                    done += n;
                    if n < chunk.len() {
                        break;
                    }
                }
                // the transfer was cancelled because the wait ended
                Err(_) if wait.cancelled() || wait.remaining().is_zero() => break,
                Err(e) => return Err(e),
            }
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::UsbEndpointInfo;

    #[test]
    fn coding_bytes() {
        let s = SerialSettings { baud: 9600, data_bits: 7, parity: Parity::Even, stop_bits: 2, ..Default::default() };
        assert_eq!(line_coding(&s), [0x80, 0x25, 0, 0, 2, 2, 7]);
        assert_eq!(line_coding(&SerialSettings::default()), [0x00, 0xc2, 0x01, 0x00, 0, 0, 8]);
    }

    #[test]
    fn finds_the_data_interface() {
        let ep = |a, k| UsbEndpointInfo { address: a, kind: k, max_packet: 64 };
        let ifs = vec![
            UsbInterfaceInfo { number: 0, class: CLASS_COMM, subclass: SUBCLASS_ACM, endpoints: vec![ep(0x83, EpKind::Interrupt)], ..Default::default() },
            UsbInterfaceInfo { number: 1, class: CLASS_DATA, endpoints: vec![ep(0x02, EpKind::Bulk), ep(0x82, EpKind::Bulk)], ..Default::default() },
        ];
        assert_eq!(data_interface(&ifs), Some((1, 0x82, 0x02, 64)));
        assert_eq!(data_interface(&ifs[..1]), None);
        let d = UsbDeviceInfo { interfaces: ifs, ..Default::default() };
        assert!(is_acm(&d));
        assert!(!is_acm(&UsbDeviceInfo::default()));
    }
}
