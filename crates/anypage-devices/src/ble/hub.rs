//! One radio scan shared by every `ble.scan` stream of the app.
//!
//! Platforms allow one scan per app (Android counts scan starts and blocks
//! apps that restart too often; WinRT has one watcher per adapter). The hub
//! runs the radio while anyone subscribes, with the union of the requested
//! service filters (no filter if any subscriber wants all devices), and
//! fans every advertisement out to all subscribers, which filter again.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use uuid::Uuid;

use super::{BleAdvert, Emit, ScanEvent};
use crate::util::lock;

/// The platform's scanner.
pub(crate) trait Radio: Send + Sync + 'static {
    fn start(&self, services: &[Uuid]) -> Result<(), String>;
    fn stop(&self);
}

#[derive(Default)]
struct State {
    next: u64,
    subs: BTreeMap<u64, (Vec<Uuid>, Emit<ScanEvent>)>,
    /// The filter the radio runs with; `None` while stopped.
    running: Option<Vec<Uuid>>,
}

pub(crate) struct ScanHub {
    state: Mutex<State>,
    /// Serialises radio start/stop; never held while delivering.
    radio_op: Mutex<()>,
}

impl ScanHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(State::default()), radio_op: Mutex::new(()) })
    }

    /// Anyone listening (the Windows pump skips work when not).
    #[cfg(any(windows, test))]
    pub fn active(&self) -> bool {
        !lock(&self.state).subs.is_empty()
    }

    pub fn subscribe(self: &Arc<Self>, radio: Arc<dyn Radio>, services: &[Uuid], emit: Emit<ScanEvent>) -> Result<Box<dyn Send>, String> {
        let id = {
            let mut st = lock(&self.state);
            let id = st.next;
            st.next += 1;
            st.subs.insert(id, (services.to_vec(), emit));
            id
        };
        if let Err(e) = self.apply(&*radio) {
            lock(&self.state).subs.remove(&id);
            let _ = self.apply(&*radio);
            return Err(e);
        }
        Ok(Box::new(Subscription { hub: self.clone(), radio, id }))
    }

    fn wanted(st: &State) -> Option<Vec<Uuid>> {
        if st.subs.is_empty() {
            return None;
        }
        if st.subs.values().any(|(s, _)| s.is_empty()) {
            return Some(Vec::new());
        }
        let mut all: Vec<Uuid> = st.subs.values().flat_map(|(s, _)| s.iter().copied()).collect();
        all.sort();
        all.dedup();
        Some(all)
    }

    /// Bring the radio in line with the subscribers.
    fn apply(&self, radio: &dyn Radio) -> Result<(), String> {
        let _op = lock(&self.radio_op);
        let (want, running) = {
            let st = lock(&self.state);
            (Self::wanted(&st), st.running.clone())
        };
        if want == running {
            return Ok(());
        }
        if running.is_some() {
            radio.stop();
        }
        let r = match &want {
            Some(w) => radio.start(w),
            None => Ok(()),
        };
        lock(&self.state).running = if r.is_ok() { want } else { None };
        r
    }

    pub fn deliver(&self, adv: &BleAdvert) {
        let emits: Vec<Emit<ScanEvent>> = lock(&self.state).subs.values().map(|(_, e)| e.clone()).collect();
        for e in emits {
            e(ScanEvent::Seen(adv.clone()));
        }
    }

    /// The system stopped the scan: tell everyone; the next subscription
    /// change (a stream retrying) starts it again.
    pub fn failed(&self, msg: &str) {
        let emits: Vec<Emit<ScanEvent>> = {
            let mut st = lock(&self.state);
            st.running = None;
            st.subs.values().map(|(_, e)| e.clone()).collect()
        };
        for e in emits {
            e(ScanEvent::Failed(msg.to_string()));
        }
    }

    #[cfg(test)]
    pub fn running(&self) -> Option<Vec<Uuid>> {
        lock(&self.state).running.clone()
    }
}

struct Subscription {
    hub: Arc<ScanHub>,
    radio: Arc<dyn Radio>,
    id: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        lock(&self.hub.state).subs.remove(&self.id);
        if let Err(e) = self.hub.apply(&*self.radio) {
            log::warn!(target: "anypage_devices", "ble scan: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ble::parse_uuid;

    #[derive(Default)]
    struct Log(Mutex<Vec<String>>);
    impl Radio for Log {
        fn start(&self, services: &[Uuid]) -> Result<(), String> {
            lock(&self.0).push(format!("start {}", services.len()));
            Ok(())
        }
        fn stop(&self) {
            lock(&self.0).push("stop".into());
        }
    }

    #[test]
    fn one_radio_scan_for_all_subscribers() {
        let hub = ScanHub::new();
        let log = Arc::new(Log::default());
        let radio: Arc<dyn Radio> = log.clone();
        let seen = Arc::new(Mutex::new(0));
        let s2 = seen.clone();
        let emit: Emit<ScanEvent> = Arc::new(move |_| *lock(&s2) += 1);
        let hr = parse_uuid("180d").unwrap();
        let bat = parse_uuid("180f").unwrap();
        let a = hub.subscribe(radio.clone(), &[hr], emit.clone()).unwrap();
        assert_eq!(hub.running(), Some(vec![hr]));
        let b = hub.subscribe(radio.clone(), &[bat, hr], emit.clone()).unwrap();
        let mut both = vec![hr, bat];
        both.sort();
        assert_eq!(hub.running(), Some(both));
        let c = hub.subscribe(radio.clone(), &[], emit.clone()).unwrap();
        assert_eq!(hub.running(), Some(vec![]));
        hub.deliver(&BleAdvert { id: "x".into(), ..Default::default() });
        assert_eq!(*lock(&seen), 3);
        drop(c);
        drop(b);
        assert_eq!(hub.running(), Some(vec![hr]));
        drop(a);
        assert_eq!(hub.running(), None);
        assert!(!hub.active());
        assert_eq!(*lock(&log.0), vec!["start 1", "stop", "start 2", "stop", "start 0", "stop", "start 2", "stop", "start 1", "stop"]);
    }

    #[test]
    fn failed_start_leaves_no_subscriber() {
        struct Off;
        impl Radio for Off {
            fn start(&self, _: &[Uuid]) -> Result<(), String> {
                Err("Bluetooth is off".into())
            }
            fn stop(&self) {}
        }
        let hub = ScanHub::new();
        let e = hub.subscribe(Arc::new(Off), &[], Arc::new(|_| {})).err().unwrap();
        assert_eq!(e, "Bluetooth is off");
        assert!(!hub.active());
        assert_eq!(hub.running(), None);
    }
}
