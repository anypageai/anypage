//! The Windows backends: btleplug (WinRT Bluetooth LE), nusb (WinUSB),
//! serialport (COM ports).

mod ble;
mod serial;

use std::sync::Arc;

use crate::Backends;

pub(crate) fn platform_backends() -> Backends {
    Backends { ble: Some(Arc::new(ble::WinBle::new())), usb: Some(Arc::new(crate::nusb_host::NusbHost)), serial: Some(Arc::new(serial::NativeSerial)) }
}
