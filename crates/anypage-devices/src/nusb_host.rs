//! USB through nusb: a device handle shared by Windows (devices opened by
//! instance id, WinUSB) and Android (devices opened from the host's file
//! descriptor, usbfs), and the Windows host itself.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nusb::descriptors::TransferType;
use nusb::transfer::{Buffer, Bulk, BulkOrInterrupt, ControlIn, ControlOut, ControlType, In, Interrupt, Out, TransferError};
use nusb::{Endpoint, ErrorKind, Interface, MaybeFuture};

use crate::usb::{ControlKind, ControlSetup, EpKind, Recipient, UsbDeviceInfo, UsbEndpointInfo, UsbHandle, UsbInterfaceInfo};
use crate::util::{lock, lock_within, Wait, SLICE};

/// "1209:0001 Loopback" for messages.
pub(crate) fn label(d: &UsbDeviceInfo) -> String {
    match d.product.as_deref() {
        Some(p) => format!("{:04x}:{:04x} ({p})", d.vid, d.pid),
        None => format!("{:04x}:{:04x}", d.vid, d.pid),
    }
}

enum Ep {
    BulkIn(Endpoint<Bulk, In>),
    BulkOut(Endpoint<Bulk, Out>),
    IntIn(Endpoint<Interrupt, In>),
    IntOut(Endpoint<Interrupt, Out>),
}

/// An endpoint in use; one transfer at a time.
type SharedEp = Arc<Mutex<Ep>>;

pub(crate) struct NusbHandle {
    dev: nusb::Device,
    label: String,
    driver: Option<String>,
    claimed: Mutex<HashMap<u8, Interface>>,
    /// Open endpoints by address, with the interface they belong to.
    eps: Mutex<HashMap<u8, (u8, SharedEp)>>,
}

fn xfer_err(e: TransferError) -> String {
    match e {
        TransferError::Stall => "the device refused the request (endpoint stalled)".into(),
        TransferError::Disconnected => "device disconnected".into(),
        TransferError::Cancelled => "the transfer was cancelled or timed out".into(),
        TransferError::Fault => "hardware fault or protocol error on the bus".into(),
        TransferError::InvalidArgument => "the request is invalid or not supported by this system's USB driver".into(),
        other => format!("transfer failed: {other}"),
    }
}

fn kind_of(t: TransferType) -> EpKind {
    match t {
        TransferType::Control => EpKind::Control,
        TransferType::Isochronous => EpKind::Isochronous,
        TransferType::Bulk => EpKind::Bulk,
        TransferType::Interrupt => EpKind::Interrupt,
    }
}

fn control_type(k: ControlKind) -> ControlType {
    match k {
        ControlKind::Standard => ControlType::Standard,
        ControlKind::Class => ControlType::Class,
        ControlKind::Vendor => ControlType::Vendor,
    }
}

fn recipient(r: Recipient) -> nusb::transfer::Recipient {
    match r {
        Recipient::Device => nusb::transfer::Recipient::Device,
        Recipient::Interface => nusb::transfer::Recipient::Interface,
        Recipient::Endpoint => nusb::transfer::Recipient::Endpoint,
        Recipient::Other => nusb::transfer::Recipient::Other,
    }
}

fn read_ep<T: BulkOrInterrupt>(e: &mut Endpoint<T, In>, length: usize, wait: &Wait) -> Result<Option<Vec<u8>>, String> {
    let packet = e.max_packet_size().max(1);
    let len = length.div_ceil(packet).max(1) * packet;
    // a transfer left over from an earlier timeout is still waiting for data: reuse it
    if e.pending() == 0 {
        e.submit(Buffer::new(len));
    }
    let done = |c: nusb::transfer::Completion| -> Result<Option<Vec<u8>>, String> {
        let n = c.actual_len;
        match c.status {
            Ok(()) => {
                let mut v = c.buffer.into_vec();
                v.truncate(n);
                Ok(Some(v))
            }
            Err(TransferError::Cancelled) if n > 0 => {
                let mut v = c.buffer.into_vec();
                v.truncate(n);
                Ok(Some(v))
            }
            Err(TransferError::Cancelled) => Ok(None),
            Err(err) => Err(xfer_err(err)),
        }
    };
    loop {
        if let Some(c) = e.wait_next_complete(wait.remaining().min(SLICE)) {
            return done(c);
        }
        if wait.cancelled() || wait.remaining().is_zero() {
            e.cancel_all();
            return match e.wait_next_complete(Duration::from_secs(1)) {
                Some(c) => done(c),
                None => Ok(None),
            };
        }
    }
}

fn write_ep<T: BulkOrInterrupt>(e: &mut Endpoint<T, Out>, data: &[u8], wait: &Wait) -> Result<usize, String> {
    if e.pending() > 0 {
        e.cancel_all();
        while e.pending() > 0 && e.wait_next_complete(Duration::from_secs(1)).is_some() {}
    }
    e.submit(Buffer::from(data.to_vec()));
    loop {
        if let Some(c) = e.wait_next_complete(wait.remaining().min(SLICE)) {
            return match c.status {
                Ok(()) => Ok(c.actual_len),
                Err(TransferError::Cancelled) => Err(format!("timed out after sending {} of {} bytes", c.actual_len, data.len())),
                Err(err) => Err(xfer_err(err)),
            };
        }
        if wait.cancelled() || wait.remaining().is_zero() {
            e.cancel_all();
            let sent = e.wait_next_complete(Duration::from_secs(1)).map(|c| c.actual_len).unwrap_or(0);
            let why = if wait.cancelled() { "cancelled".to_string() } else { format!("timed out after {} ms", wait.timeout().as_millis()) };
            return Err(format!("{why} after sending {sent} of {} bytes", data.len()));
        }
    }
}

impl NusbHandle {
    pub fn new(dev: nusb::Device, label: String, driver: Option<String>) -> Self {
        Self { dev, label, driver, claimed: Mutex::new(HashMap::new()), eps: Mutex::new(HashMap::new()) }
    }

    fn claim_err(&self, interface: u8, e: nusb::Error) -> String {
        const FIX: &str = "Install WinUSB for it (for example with Zadig), or give the firmware a Microsoft OS 2.0 descriptor that asks for WinUSB";
        match e.kind() {
            ErrorKind::Unsupported if cfg!(windows) && self.driver.as_deref().is_some_and(|d| d.eq_ignore_ascii_case("usbccgp")) => format!(
                "cannot use interface {interface} of {}: it is part of a composite device and Windows bound another driver to that interface, not WinUSB. {FIX} (in Zadig: Options > List All Devices, then the entry for interface {interface})",
                self.label
            ),
            ErrorKind::Unsupported if cfg!(windows) => format!(
                "cannot use interface {interface} of {}: Windows has the \"{}\" driver bound to it, and only devices (or interfaces) bound to WinUSB can be used directly. {FIX}",
                self.label,
                self.driver.as_deref().unwrap_or("unknown")
            ),
            ErrorKind::Busy => format!("interface {interface} of {} is in use by another program or driver", self.label),
            ErrorKind::PermissionDenied => format!("access to interface {interface} of {} was denied", self.label),
            ErrorKind::NotFound => format!("{} has no interface {interface}", self.label),
            ErrorKind::Disconnected => "device disconnected".into(),
            _ => format!("cannot claim interface {interface} of {}: {e}", self.label),
        }
    }

    fn interface(&self, n: u8) -> Result<Interface, String> {
        if let Some(i) = lock(&self.claimed).get(&n) {
            return Ok(i.clone());
        }
        let i = self.dev.detach_and_claim_interface(n).wait().map_err(|e| self.claim_err(n, e))?;
        Ok(lock(&self.claimed).entry(n).or_insert(i).clone())
    }

    fn endpoint(&self, interface: u8, address: u8) -> Result<SharedEp, String> {
        if let Some((_, e)) = lock(&self.eps).get(&address) {
            return Ok(e.clone());
        }
        let iface = self.interface(interface)?;
        let kind = iface
            .descriptor()
            .and_then(|d| d.endpoints().find(|e| e.address() == address).map(|e| e.transfer_type()))
            .ok_or_else(|| format!("interface {interface} has no endpoint 0x{address:02x}"))?;
        let open_err = |e: nusb::Error| format!("cannot open endpoint 0x{address:02x}: {e}");
        let ep = match (kind, address & 0x80 != 0) {
            (TransferType::Bulk, true) => Ep::BulkIn(iface.endpoint::<Bulk, In>(address).map_err(open_err)?),
            (TransferType::Bulk, false) => Ep::BulkOut(iface.endpoint::<Bulk, Out>(address).map_err(open_err)?),
            (TransferType::Interrupt, true) => Ep::IntIn(iface.endpoint::<Interrupt, In>(address).map_err(open_err)?),
            (TransferType::Interrupt, false) => Ep::IntOut(iface.endpoint::<Interrupt, Out>(address).map_err(open_err)?),
            (other, _) => return Err(format!("endpoint 0x{address:02x} is a {} endpoint; only bulk and interrupt are supported", kind_of(other).name())),
        };
        let ep = Arc::new(Mutex::new(ep));
        Ok(lock(&self.eps).entry(address).or_insert((interface, ep)).1.clone())
    }

    /// The interface a control transfer goes through on Windows (WinUSB
    /// sends control transfers through an interface handle).
    #[cfg(windows)]
    fn control_interface(&self, setup: &ControlSetup, interface: Option<u8>) -> Result<Interface, String> {
        let n = match interface {
            Some(n) => n,
            None if setup.recipient == Recipient::Interface => (setup.index & 0xff) as u8,
            None => match lock(&self.claimed).keys().next() {
                Some(n) => *n,
                None => self
                    .dev
                    .active_configuration()
                    .ok()
                    .and_then(|c| c.interfaces().next().map(|i| i.interface_number()))
                    .ok_or("the device has no interface to send control transfers through")?,
            },
        };
        self.interface(n)
    }
}

impl UsbHandle for NusbHandle {
    fn interfaces(&self) -> Vec<UsbInterfaceInfo> {
        let Ok(config) = self.dev.active_configuration() else { return Vec::new() };
        let claimed = lock(&self.claimed);
        config
            .interfaces()
            .map(|group| {
                let n = group.interface_number();
                let alt = claimed.get(&n).map(|i| i.get_alt_setting()).unwrap_or(0);
                let d = group.alt_settings().find(|a| a.alternate_setting() == alt).unwrap_or_else(|| group.first_alt_setting());
                UsbInterfaceInfo {
                    number: n,
                    alt: d.alternate_setting(),
                    class: d.class(),
                    subclass: d.subclass(),
                    protocol: d.protocol(),
                    name: None,
                    endpoints: d
                        .endpoints()
                        .map(|e| UsbEndpointInfo { address: e.address(), kind: kind_of(e.transfer_type()), max_packet: e.max_packet_size() as u16 })
                        .collect(),
                }
            })
            .collect()
    }

    fn claim(&self, interface: u8) -> Result<(), String> {
        self.interface(interface).map(|_| ())
    }

    fn release(&self, interface: u8) {
        lock(&self.eps).retain(|_, (i, _)| *i != interface);
        lock(&self.claimed).remove(&interface);
    }

    fn control_in(&self, setup: &ControlSetup, length: u16, interface: Option<u8>, wait: &Wait) -> Result<Vec<u8>, String> {
        wait.check("control transfer")?;
        let req = ControlIn { control_type: control_type(setup.kind), recipient: recipient(setup.recipient), request: setup.request, value: setup.value, index: setup.index, length };
        let timeout = wait.remaining();
        #[cfg(windows)]
        let r = self.control_interface(setup, interface)?.control_in(req, timeout).wait();
        #[cfg(not(windows))]
        let r = {
            let _ = interface;
            self.dev.control_in(req, timeout).wait()
        };
        r.map_err(|e| match e {
            TransferError::Cancelled => wait.timed_out("control transfer"),
            other => xfer_err(other),
        })
    }

    fn control_out(&self, setup: &ControlSetup, data: &[u8], interface: Option<u8>, wait: &Wait) -> Result<usize, String> {
        wait.check("control transfer")?;
        let req = ControlOut { control_type: control_type(setup.kind), recipient: recipient(setup.recipient), request: setup.request, value: setup.value, index: setup.index, data };
        let timeout = wait.remaining();
        #[cfg(windows)]
        let r = self.control_interface(setup, interface)?.control_out(req, timeout).wait();
        #[cfg(not(windows))]
        let r = {
            let _ = interface;
            self.dev.control_out(req, timeout).wait()
        };
        r.map(|()| data.len()).map_err(|e| match e {
            TransferError::Cancelled => wait.timed_out("control transfer"),
            other => xfer_err(other),
        })
    }

    fn read(&self, interface: u8, endpoint: u8, length: usize, wait: &Wait) -> Result<Option<Vec<u8>>, String> {
        let ep = self.endpoint(interface, endpoint)?;
        // one transfer at a time per endpoint; waiting behind another one
        // stays cancellable
        let mut ep = lock_within(&ep, wait, &format!("endpoint 0x{endpoint:02x} is busy with another transfer"))?;
        match &mut *ep {
            Ep::BulkIn(e) => read_ep(e, length, wait),
            Ep::IntIn(e) => read_ep(e, length, wait),
            _ => Err(format!("endpoint 0x{endpoint:02x} is not an IN endpoint")),
        }
    }

    fn write(&self, interface: u8, endpoint: u8, data: &[u8], wait: &Wait) -> Result<usize, String> {
        let ep = self.endpoint(interface, endpoint)?;
        let mut ep = lock_within(&ep, wait, &format!("endpoint 0x{endpoint:02x} is busy with another transfer"))?;
        match &mut *ep {
            Ep::BulkOut(e) => write_ep(e, data, wait),
            Ep::IntOut(e) => write_ep(e, data, wait),
            _ => Err(format!("endpoint 0x{endpoint:02x} is not an OUT endpoint")),
        }
    }
}

// ---------------------------------------------------------------- Windows host

#[cfg(windows)]
pub(crate) use windows_host::NusbHost;

#[cfg(windows)]
mod windows_host {
    use super::*;
    use crate::usb::UsbBackend;

    fn speed(s: Option<nusb::Speed>) -> Option<&'static str> {
        Some(match s? {
            nusb::Speed::Low => "low",
            nusb::Speed::Full => "full",
            nusb::Speed::High => "high",
            nusb::Speed::Super => "super",
            nusb::Speed::SuperPlus => "super_plus",
            _ => return None,
        })
    }

    /// Descriptor strings, without the padding some firmware adds.
    fn clean(s: Option<&str>) -> Option<String> {
        s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
    }

    pub(crate) fn info_of(d: &nusb::DeviceInfo) -> UsbDeviceInfo {
        UsbDeviceInfo {
            id: d.instance_id().to_string_lossy().into_owned(),
            vid: d.vendor_id(),
            pid: d.product_id(),
            manufacturer: clean(d.manufacturer_string()),
            product: clean(d.product_string()),
            serial: clean(d.serial_number()),
            bus: d.bus_id().to_string(),
            address: d.device_address(),
            class: d.class(),
            subclass: d.subclass(),
            protocol: d.protocol(),
            speed: speed(d.speed()),
            driver: d.driver().map(str::to_string),
            interfaces: d
                .interfaces()
                .map(|i| UsbInterfaceInfo {
                    number: i.interface_number(),
                    alt: 0,
                    class: i.class(),
                    subclass: i.subclass(),
                    protocol: i.protocol(),
                    name: clean(i.interface_string()),
                    endpoints: Vec::new(),
                })
                .collect(),
        }
    }

    /// Windows: every USB device the system knows; WinUSB-bound ones can be opened for transfers.
    pub(crate) struct NusbHost;

    impl UsbBackend for NusbHost {
        fn devices(&self) -> Result<Vec<UsbDeviceInfo>, String> {
            let list = nusb::list_devices().wait().map_err(|e| format!("cannot list USB devices: {e}"))?;
            Ok(list.map(|d| info_of(&d)).collect())
        }

        fn open(&self, id: &str, wait: &Wait) -> Result<Arc<dyn UsbHandle>, String> {
            wait.check("open")?;
            let list = nusb::list_devices().wait().map_err(|e| format!("cannot list USB devices: {e}"))?;
            let d = list.into_iter().find(|d| d.instance_id().to_string_lossy() == id).ok_or_else(|| format!("no USB device {} (unplugged?)", crate::util::clip(id)))?;
            let info = info_of(&d);
            let dev = d.open().wait().map_err(|e| match e.kind() {
                ErrorKind::Disconnected => "device disconnected".to_string(),
                _ => format!("cannot open {}: {e}", label(&info)),
            })?;
            Ok(Arc::new(NusbHandle::new(dev, label(&info), info.driver.clone())))
        }
    }
}
