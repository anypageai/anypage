//! Serial ports on Windows through the `serialport` crate (COM ports:
//! USB-serial adapters, Bluetooth SPP ports, built-in UARTs).

use std::io::{ErrorKind, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serialport::{DataBits, FlowControl as SpFlow, Parity as SpParity, SerialPort, SerialPortType, StopBits};

use crate::serial::{FlowControl, Parity, SerialBackend, SerialConn, SerialPortInfo, SerialSettings, READ_SLICE};
use crate::util::{clip, lock, lock_within, Wait};

pub(crate) struct NativeSerial;

fn open_err(path: &str, e: serialport::Error) -> String {
    let path = clip(path);
    match e.kind() {
        serialport::ErrorKind::NoDevice => format!("{path}: the port does not exist or is in use by another program"),
        serialport::ErrorKind::InvalidInput => format!("{path}: unsupported settings ({e})"),
        _ => format!("{path}: {e}"),
    }
}

impl SerialBackend for NativeSerial {
    fn ports(&self) -> Result<Vec<SerialPortInfo>, String> {
        let mut v: Vec<SerialPortInfo> = serialport::available_ports()
            .map_err(|e| format!("cannot list serial ports: {e}"))?
            .into_iter()
            .map(|p| {
                let mut info = SerialPortInfo { path: p.port_name, kind: "unknown", ..Default::default() };
                match p.port_type {
                    SerialPortType::UsbPort(u) => {
                        info.kind = "usb";
                        info.vid = Some(u.vid);
                        info.pid = Some(u.pid);
                        info.serial = u.serial_number;
                        info.manufacturer = u.manufacturer;
                        info.product = u.product;
                    }
                    SerialPortType::PciPort => info.kind = "pci",
                    SerialPortType::BluetoothPort => info.kind = "bluetooth",
                    SerialPortType::Unknown => {}
                }
                info
            })
            .collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(v)
    }

    fn open(&self, path: &str, s: &SerialSettings, wait: &Wait) -> Result<Arc<dyn SerialConn>, String> {
        wait.check(path)?;
        let data_bits = match s.data_bits {
            5 => DataBits::Five,
            6 => DataBits::Six,
            7 => DataBits::Seven,
            _ => DataBits::Eight,
        };
        let parity = match s.parity {
            Parity::None => SpParity::None,
            Parity::Odd => SpParity::Odd,
            Parity::Even => SpParity::Even,
        };
        let flow = match s.flow {
            FlowControl::None => SpFlow::None,
            FlowControl::Software => SpFlow::Software,
            FlowControl::Hardware => SpFlow::Hardware,
        };
        let mut port = serialport::new(path, s.baud)
            .data_bits(data_bits)
            .parity(parity)
            .stop_bits(if s.stop_bits == 2 { StopBits::Two } else { StopBits::One })
            .flow_control(flow)
            // one timeout for the port: reads return within it, writes are
            // retried until their own deadline
            .timeout(READ_SLICE)
            .open()
            .map_err(|e| open_err(path, e))?;
        if let Some(dtr) = s.dtr {
            port.write_data_terminal_ready(dtr).map_err(|e| format!("{path}: setting DTR: {e}"))?;
        }
        if let Some(rts) = s.rts {
            port.write_request_to_send(rts).map_err(|e| format!("{path}: setting RTS: {e}"))?;
        }
        let writer = port.try_clone().map_err(|e| format!("{path}: {e}"))?;
        Ok(Arc::new(NativeConn { path: path.to_string(), reader: Mutex::new(port), writer: Mutex::new(writer) }))
    }
}

struct NativeConn {
    path: String,
    reader: Mutex<Box<dyn SerialPort>>,
    writer: Mutex<Box<dyn SerialPort>>,
}

impl SerialConn for NativeConn {
    fn read(&self, max: usize, _timeout: Duration) -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; max.clamp(1, 64 * 1024)];
        let mut p = lock(&self.reader);
        match p.read(&mut buf) {
            Ok(n) => {
                buf.truncate(n);
                Ok(buf)
            }
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted | ErrorKind::WouldBlock) => Ok(Vec::new()),
            Err(e) => Err(format!("{}: {e} (unplugged?)", self.path)),
        }
    }

    fn write(&self, data: &[u8], wait: &Wait) -> Result<usize, String> {
        // another write (a job, the stream's queue) may hold the port
        let Ok(mut w) = lock_within(&self.writer, wait, &self.path) else { return Ok(0) };
        let mut done = 0;
        // each WriteFile returns within the port timeout (READ_SLICE) with
        // what the driver took, so `wait` is checked every ~20 ms. No
        // flush: FlushFileBuffers waits without a limit while flow control
        // holds, and timed-out writes have already left the driver.
        while done < data.len() && !wait.cancelled() && !wait.remaining().is_zero() {
            match w.write(&data[done..]) {
                Ok(n) => done += n,
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted | ErrorKind::WouldBlock) => {}
                Err(e) => return Err(format!("{}: {e}", self.path)),
            }
        }
        Ok(done)
    }
}
