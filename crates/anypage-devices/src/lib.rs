//! Device module: Bluetooth LE (`ble`), USB (`usb`) and serial ports
//! (`serial`) for page scripts. See docs/MODULES.md for the module contract.
//!
//! A page declares the modules it uses, normally as optional because not
//! every platform has every one (no USB host on iOS):
//! `"capabilities": { "optional_modules": ["ble", "usb", "serial"] }`, then
//! checks `if usb then ... end`.
//!
//! # Luau API
//!
//! Every request below is a table; jobs and streams take it as the value of
//! their first input cell (`page.job(name, "usb.control", { req_cell })`)
//! laid over the options given when they are declared, so fixed fields can
//! live in the options. A nil or `{}` request leaves a job idle (its value
//! is nil) and turns a stream's device off (`false` too); `true` asks a
//! stream for what its options say. Errors are clear strings: a job's cell
//! shows `{ state = "error", message }`, a stream item carries `error`;
//! page text quoted in them is cut to 64 characters, and ids and paths
//! longer than 512 bytes are refused. Bytes are Luau buffers; where data
//! goes in, a buffer, a string, a file's data or a list of numbers 0..255
//! is accepted.
//!
//! Long operations end early when their job is cancelled (a new request,
//! the page closing) and a stream's connect or open gives way at once to a
//! newer request, the stream being switched off, or the page closing. A
//! switched-off or idle stream thread sleeps (one wakeup per 250 ms).
//!
//! `ble.available()`, `usb.available()` and `serial.available()` return
//! `{ ok = true }` or `{ ok = false, reason }` at once: on Android, before
//! the host has installed its device bridge, they say so instead of every
//! call failing.
//!
//! ## `ble` — Bluetooth LE
//!
//! | name | kind | |
//! |---|---|---|
//! | `ble.uuid(s)` | function | `"180d"`, `"0x2A37"` or a full UUID → full lower-case UUID |
//! | `ble.available()` | function | `{ ok, reason }` |
//! | `ble.scan` | stream | request `{ on = true, services = { uuid... }, name_prefix, min_rssi, interval_ms = 250, forget_s = 60, max = 256 }` (`true` scans for everything with the defaults) → `{ scanning, count, devices = { {id, name, rssi, services, manufacturer = { {company, data} }, service_data = { {uuid, data} }, tx_power, connectable, last_seen} }, error }` |
//! | `ble.link` | stream | request `{ device = id, subscribe = { {service, characteristic, optional} }, keep = 64, reconnect = true, timeout_ms = 15000, interval_ms = 30 }` → `{ state, device, services = { {uuid, characteristics = { {uuid, properties} }} }, notifications = { {service, characteristic, data, at, seq} }, latest = { [characteristic] = data }, count, sent, error }` |
//! | `ble.read` | job | `{ device, service, characteristic, timeout_ms = 10000 }` → `{ data, text, at }` (`text`: the bytes as UTF-8 when they decode to printable text) |
//! | `ble.write` | job | `{ device, service, characteristic, data, response, timeout_ms = 10000 }` → `{ written, at }` |
//! | `ble.send(device, characteristic, data[, {service, response}])` | effect | queue a write on a link a `ble.link` stream holds; returns the byte count; errors show in the link's `error` |
//!
//! `state` of a link: "idle", "connecting", "connected", "reconnecting",
//! "disconnected", "failed". `service` may be omitted wherever a
//! characteristic is unique on the device. Jobs reuse a link a `ble.link`
//! stream holds open, otherwise they connect for the one operation.
//!
//! Jobs run when something needs their value (a panel that shows it, a
//! cell that reads it). For writes triggered by a click, prefer the
//! effects `ble.send` and `serial.send`, which run at once.
//!
//! ## `usb` — USB host
//!
//! | name | kind | |
//! |---|---|---|
//! | `usb.list` | job | the devices the system sees: `{ {id, vid, pid, manufacturer, product, serial, bus, address, class, subclass, protocol, speed, driver, interfaces = { {number, alt, class, subclass, protocol, name, endpoints} }} }`; give it an input (a refresh counter) to list again |
//! | `usb.devices()` | function | the same list, enumerated on the calling thread (5-30 ms on Windows): for handlers; cells use `usb.list` |
//! | `usb.available()` | function | `{ ok, reason }` |
//! | `usb.open` | job | `{ device, claim = { interface numbers } or "all", timeout_ms = 60000 }` → the device with `interfaces[i].endpoints = { {address, direction, type, max_packet} }` and `handle`, a "usb.device" resource that keeps it open |
//! | `usb.control` | job | `{ device, direction = "in"/"out", type = "vendor"/"class"/"standard", recipient = "device"/"interface"/"endpoint"/"other", request, value, index, length (in) / data (out), interface, timeout_ms = 1000 }` → `{ data, length }` / `{ written }` |
//! | `usb.bulk_in` | job | `{ device, endpoint, length, interface, timeout_ms = 5000 }` → `{ data, length }` (bulk or interrupt; length rounded up to whole packets) |
//! | `usb.bulk_out` | job | `{ device, endpoint, data, interface, timeout_ms = 5000 }` → `{ written }` |
//! | `usb.close(device)` | effect | release a device opened with `usb.open` (an id, or its `handle`: `handle:close()`); false if it was not open |
//!
//! A device stays open, with the interfaces `usb.open` claimed, while the
//! page holds the `handle` from `usb.open` (in the job's value, or a script
//! variable): it is released when the page closes, on `usb.close`, or when
//! the job runs again for another device and nothing refers to the old
//! value any more. A script that read the old value (a panel drew it)
//! keeps it until Luau's garbage collector frees it, so pages switching
//! devices call `usb.close` on the previous one. Transfers on a device the page
//! did not open with `usb.open` open it for that one job. A device that was
//! unplugged and is back is reopened (and its interfaces claimed again) on
//! next use. On Windows a device can be listed whatever its driver, but
//! transfers need it (or the interface) bound to WinUSB; the error says
//! which driver holds it. On Android the first use asks the user for
//! permission.
//!
//! ## `serial` — serial ports
//!
//! | name | kind | |
//! |---|---|---|
//! | `serial.list` | job | `{ {path, type = "usb"/"pci"/"bluetooth"/"unknown", vid, pid, serial, manufacturer, product} }`; an input (a refresh counter) lists again |
//! | `serial.ports()` | function | the same list on the calling thread (20-50 ms on Windows): for handlers; cells use `serial.list` |
//! | `serial.available()` | function | `{ ok, reason }` |
//! | `serial.port` | stream | request `{ path, baud = 115200, data_bits = 8, parity = "none", stop_bits = 1, flow = "none", dtr, rts, keep_lines = 200, keep_bytes = 4096, reconnect = true, interval_ms = 30, write_timeout_ms = 5000 }` → `{ state, path, lines, partial, bytes, rx, tx, error }` |
//! | `serial.write` | job | `{ path, data, timeout_ms = 5000, ...settings }` → `{ written }` (opens the port for the write if no stream holds it); a write the port does not finish in time fails with how far it got |
//! | `serial.send(path, data)` | effect | queue bytes on a port a stream holds open; returns the count. The stream writes them between reads; bytes the device takes nothing of for `write_timeout_ms` are dropped with an `error` |
//!
//! Streams are best declared with `latest = true`: every item is the whole
//! current state (device table, recent notifications, recent lines).
//!
//! # Platforms
//!
//! * Windows: btleplug (WinRT) on a one-thread tokio runtime, nusb (WinUSB),
//!   serialport (COM ports).
//! * Android: everything through the host's bridge ([`android`]); USB
//!   transfers run natively on the descriptor the host opens; serial ports
//!   are CDC-ACM devices over USB ([`serial::cdc`]).
//! * Elsewhere: nothing is registered yet, so the modules are absent and a
//!   page's `optional_modules` see nil.
//!
//! [`mock`] has simulated devices for tests and for hosts without hardware.

pub mod android;
pub mod ble;
pub mod mock;
#[cfg(any(windows, target_os = "android"))]
mod nusb_host;
pub mod serial;
#[cfg(test)]
mod stream_tests;
pub mod usb;
mod util;
#[cfg(windows)]
mod windows;

use std::sync::Arc;

use anypage_runtime::Natives;

pub use android::{set_android_bridge, AndroidDevices, AndroidHost};
pub use ble::{BleBackend, BleConnection};
pub use serial::{SerialBackend, SerialConn};
pub use usb::{UsbBackend, UsbHandle};
pub use util::{StreamIo, Wait, Worker};

/// The device stacks a build uses; a missing one leaves its module out.
#[derive(Clone, Default)]
pub struct Backends {
    pub ble: Option<Arc<dyn BleBackend>>,
    pub usb: Option<Arc<dyn UsbBackend>>,
    pub serial: Option<Arc<dyn SerialBackend>>,
}

impl Backends {
    /// This platform's real devices.
    pub fn platform() -> Self {
        #[cfg(windows)]
        {
            windows::platform_backends()
        }
        #[cfg(target_os = "android")]
        {
            android::platform_backends()
        }
        #[cfg(not(any(windows, target_os = "android")))]
        {
            Backends::default()
        }
    }
}

/// Add `ble`, `usb` and `serial` for this platform's devices.
pub fn register(n: &mut Natives) {
    register_with(n, Backends::platform());
}

/// Add the modules backed by `backends` (simulated devices, a custom host).
pub fn register_with(n: &mut Natives, backends: Backends) {
    if let Some(b) = backends.ble {
        ble::register(n, b);
    }
    if let Some(b) = backends.usb {
        usb::register(n, b);
    }
    if let Some(b) = backends.serial {
        serial::register(n, b);
    }
}
