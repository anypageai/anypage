//! Bluetooth LE on Windows through btleplug (WinRT). btleplug is async; it
//! runs on a tokio runtime with one worker thread, started on first use.
//! Every call from the module's threads is a task on that runtime awaited
//! with a deadline and cancellation (the task is aborted when the caller
//! gives up).

use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use btleplug::api::{BDAddr, Central, CentralEvent, CentralState, Characteristic, Manager as _, Peripheral as _, PeripheralProperties, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures_util::StreamExt;
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::ble::hub::{Radio, ScanHub};
use crate::ble::{BleAdvert, BleBackend, BleConnection, Emit, GattChar, GattService, LinkEvent, ScanEvent};
use crate::util::{clip, lock, lock_within, Wait};

fn err(e: btleplug::Error) -> String {
    match e {
        btleplug::Error::PermissionDenied => "Bluetooth access was denied (Windows Settings > Privacy > Bluetooth)".into(),
        btleplug::Error::DeviceNotFound => "the device was not found; scan for it first, and check it is on and in range".into(),
        btleplug::Error::NotConnected => "not connected".into(),
        btleplug::Error::TimedOut(d) => format!("timed out after {} ms", d.as_millis()),
        other => other.to_string(),
    }
}

/// Run `fut` on the runtime, waiting at most as `wait` allows.
fn run<T: Send + 'static>(rt: &Handle, wait: &Wait, what: &str, fut: impl Future<Output = Result<T, String>> + Send + 'static) -> Result<T, String> {
    let (tx, rx) = channel();
    let task = rt.spawn(async move {
        let _ = tx.send(fut.await);
    });
    match wait.recv(&rx, what) {
        Ok(r) => r.map_err(|e| format!("{what}: {e}")),
        Err(e) => {
            task.abort();
            Err(e)
        }
    }
}

struct Core {
    rt: Handle,
    adapter: Adapter,
}

/// Disconnects a peripheral when dropped, unless taken. A connect that was
/// aborted (timeout, cancel), failed half-way (service discovery), or whose
/// result nobody received any more must not leave the link open in Windows.
struct Disconnect {
    p: Option<Peripheral>,
    rt: Handle,
}

impl Disconnect {
    fn take(mut self) -> Option<Peripheral> {
        self.p.take()
    }
}

impl Drop for Disconnect {
    fn drop(&mut self) {
        if let Some(p) = self.p.take() {
            self.rt.spawn(async move {
                let _ = p.disconnect().await;
            });
        }
    }
}

pub(crate) struct WinBle {
    rt: OnceLock<Runtime>,
    core: Mutex<Option<Arc<Core>>>,
    /// Held while one caller looks for the adapter; others wait for it
    /// only as long as their own wait allows.
    init: Mutex<()>,
    hub: Arc<ScanHub>,
}

impl WinBle {
    pub fn new() -> Self {
        WinBle { rt: OnceLock::new(), core: Mutex::new(None), init: Mutex::new(()), hub: ScanHub::new() }
    }

    fn runtime(&self) -> Result<Handle, String> {
        if let Some(rt) = self.rt.get() {
            return Ok(rt.handle().clone());
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("anypage-ble")
            .enable_all()
            .build()
            .map_err(|e| format!("cannot start the Bluetooth thread: {e}"))?;
        Ok(self.rt.get_or_init(|| rt).handle().clone())
    }

    /// The adapter, found on first use; a failure is not remembered so a
    /// dongle plugged in later is picked up.
    fn core(&self, wait: &Wait) -> Result<Arc<Core>, String> {
        if let Some(c) = lock(&self.core).clone() {
            return Ok(c);
        }
        let _init = lock_within(&self.init, wait, "waiting for the Bluetooth adapter")?;
        if let Some(c) = lock(&self.core).clone() {
            return Ok(c);
        }
        let rt = self.runtime()?;
        let adapter = run(&rt, &wait.sub(Duration::from_secs(10)), "Bluetooth", async {
            let m = Manager::new().await.map_err(err)?;
            let adapters = m.adapters().await.map_err(err)?;
            adapters.into_iter().next().ok_or_else(|| "this computer has no Bluetooth adapter".to_string())
        })?;
        let a = adapter.clone();
        let events = run(&rt, &wait.sub(Duration::from_secs(5)), "Bluetooth events", async move { a.events().await.map_err(err) })?;
        rt.spawn(pump(adapter.clone(), events, self.hub.clone()));
        let c = Arc::new(Core { rt, adapter });
        *lock(&self.core) = Some(c.clone());
        Ok(c)
    }
}

fn advert_of(id: &PeripheralId, p: &PeripheralProperties) -> BleAdvert {
    let mut manufacturer_data: Vec<(u16, Vec<u8>)> = p.manufacturer_data.iter().map(|(k, v)| (*k, v.clone())).collect();
    manufacturer_data.sort_by_key(|(k, _)| *k);
    let mut service_data: Vec<(Uuid, Vec<u8>)> = p.service_data.iter().map(|(k, v)| (*k, v.clone())).collect();
    service_data.sort_by_key(|(k, _)| *k);
    BleAdvert {
        id: id.to_string().to_uppercase(),
        name: p.local_name.clone().or_else(|| p.advertisement_name.clone()).filter(|n| !n.is_empty()),
        rssi: p.rssi,
        services: p.services.clone(),
        manufacturer_data,
        service_data,
        tx_power: p.tx_power_level,
        connectable: None,
    }
}

/// Adapter events → advertisements for the scan hub.
async fn pump(adapter: Adapter, mut events: std::pin::Pin<Box<dyn futures_util::Stream<Item = CentralEvent> + Send>>, hub: Arc<ScanHub>) {
    while let Some(ev) = events.next().await {
        let id = match &ev {
            CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) => id,
            CentralEvent::ManufacturerDataAdvertisement { id, .. } | CentralEvent::ServiceDataAdvertisement { id, .. } | CentralEvent::ServicesAdvertisement { id, .. } | CentralEvent::RssiUpdate { id, .. } => id,
            CentralEvent::StateUpdate(CentralState::PoweredOff) => {
                hub.failed("Bluetooth was switched off");
                continue;
            }
            _ => continue,
        };
        if !hub.active() {
            continue;
        }
        let Ok(p) = adapter.peripheral(id).await else { continue };
        if let Ok(Some(props)) = p.properties().await {
            hub.deliver(&advert_of(id, &props));
        }
    }
}

struct WinRadio(Arc<Core>);

impl Radio for WinRadio {
    fn start(&self, services: &[Uuid]) -> Result<(), String> {
        let a = self.0.adapter.clone();
        let filter = ScanFilter { services: services.to_vec() };
        run(&self.0.rt, &Wait::timeout_only(Duration::from_secs(5)), "start scanning", async move {
            if a.adapter_state().await.ok() == Some(CentralState::PoweredOff) {
                return Err("Bluetooth is off".into());
            }
            a.start_scan(filter).await.map_err(err)
        })
    }

    fn stop(&self) {
        let a = self.0.adapter.clone();
        let _ = run(&self.0.rt, &Wait::timeout_only(Duration::from_secs(5)), "stop scanning", async move { a.stop_scan().await.map_err(err) });
    }
}

impl BleBackend for WinBle {
    fn scan(&self, services: &[Uuid], events: Emit<ScanEvent>) -> Result<Box<dyn Send>, String> {
        let core = self.core(&Wait::timeout_only(Duration::from_secs(15)))?;
        self.hub.subscribe(Arc::new(WinRadio(core)), services, events)
    }

    fn connect(&self, id: &str, events: Emit<LinkEvent>, wait: &Wait) -> Result<Arc<dyn BleConnection>, String> {
        let addr = BDAddr::from_str(id).map_err(|_| format!("\"{}\" is not a Bluetooth address (use an id from ble.scan)", clip(id)))?;
        let core = self.core(wait)?;
        let pid = PeripheralId::from(addr);
        let (a, p_id, rt) = (core.adapter.clone(), pid.clone(), core.rt.clone());
        let guard: Disconnect = run(&core.rt, wait, &format!("connect to {id}"), async move {
            if a.adapter_state().await.ok() == Some(CentralState::PoweredOff) {
                return Err("Bluetooth is off".into());
            }
            let p = match a.peripheral(&p_id).await {
                Ok(p) => p,
                Err(_) => a.add_peripheral(&p_id).await.map_err(err)?,
            };
            // from here on, giving up disconnects: the guard drops when this
            // task fails, is aborted, or its result is not received
            let guard = Disconnect { p: Some(p.clone()), rt };
            p.connect().await.map_err(err)?;
            p.discover_services().await.map_err(|e| format!("discovering services: {}", err(e)))?;
            Ok(guard)
        })?;
        let p = guard.take().ok_or("connect: the peripheral went missing")?;
        let services: Vec<GattService> = p
            .services()
            .into_iter()
            .map(|s| GattService { uuid: s.uuid, characteristics: s.characteristics.iter().map(|c| GattChar { uuid: c.uuid, properties: c.properties.bits() }).collect() })
            .collect();
        let chars: Vec<Characteristic> = p.characteristics().into_iter().collect();
        let alive = Arc::new(AtomicBool::new(true));

        let pn = p.clone();
        let notes = run(&core.rt, wait, "notifications", async move { pn.notifications().await.map_err(err) });
        let a = core.adapter.clone();
        let central = run(&core.rt, wait, "Bluetooth events", async move { a.events().await.map_err(err) });
        let (mut notes, mut central) = match (notes, central) {
            (Ok(n), Ok(c)) => (n, c),
            (Err(e), _) | (_, Err(e)) => {
                let pd = p.clone();
                core.rt.spawn(async move { pd.disconnect().await });
                return Err(e);
            }
        };
        let e1 = events.clone();
        let t1 = core.rt.spawn(async move {
            while let Some(n) = notes.next().await {
                e1(LinkEvent::Notification { service: n.service_uuid, characteristic: n.uuid, data: n.value });
            }
        });
        let al = alive.clone();
        let t2 = core.rt.spawn(async move {
            while let Some(ev) = central.next().await {
                if matches!(&ev, CentralEvent::DeviceDisconnected(d) if *d == pid) {
                    if al.swap(false, Ordering::Relaxed) {
                        events(LinkEvent::Disconnected("the device disconnected".into()));
                    }
                    break;
                }
            }
        });
        Ok(Arc::new(WinLink { rt: core.rt.clone(), p, services, chars, alive, tasks: vec![t1, t2] }))
    }
}

struct WinLink {
    rt: Handle,
    p: Peripheral,
    services: Vec<GattService>,
    chars: Vec<Characteristic>,
    alive: Arc<AtomicBool>,
    tasks: Vec<JoinHandle<()>>,
}

impl WinLink {
    fn find(&self, service: Uuid, characteristic: Uuid) -> Result<Characteristic, String> {
        self.chars.iter().find(|c| c.service_uuid == service && c.uuid == characteristic).cloned().ok_or_else(|| format!("no characteristic {characteristic} in service {service}"))
    }
}

impl BleConnection for WinLink {
    fn services(&self) -> Vec<GattService> {
        self.services.clone()
    }

    fn read(&self, service: Uuid, characteristic: Uuid, wait: &Wait) -> Result<Vec<u8>, String> {
        let (c, p) = (self.find(service, characteristic)?, self.p.clone());
        run(&self.rt, wait, "read", async move { p.read(&c).await.map_err(err) })
    }

    fn write(&self, service: Uuid, characteristic: Uuid, data: &[u8], with_response: bool, wait: &Wait) -> Result<(), String> {
        let (c, p, data) = (self.find(service, characteristic)?, self.p.clone(), data.to_vec());
        let kind = if with_response { WriteType::WithResponse } else { WriteType::WithoutResponse };
        run(&self.rt, wait, "write", async move { p.write(&c, &data, kind).await.map_err(err) })
    }

    fn subscribe(&self, service: Uuid, characteristic: Uuid, wait: &Wait) -> Result<(), String> {
        let (c, p) = (self.find(service, characteristic)?, self.p.clone());
        run(&self.rt, wait, "subscribe", async move { p.subscribe(&c).await.map_err(err) })
    }

    fn unsubscribe(&self, service: Uuid, characteristic: Uuid, wait: &Wait) -> Result<(), String> {
        let (c, p) = (self.find(service, characteristic)?, self.p.clone());
        run(&self.rt, wait, "unsubscribe", async move { p.unsubscribe(&c).await.map_err(err) })
    }

    fn connected(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn disconnect(&self) {
        if self.alive.swap(false, Ordering::Relaxed) {
            let p = self.p.clone();
            self.rt.spawn(async move { p.disconnect().await });
        }
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Drop for WinLink {
    fn drop(&mut self) {
        self.disconnect();
    }
}
