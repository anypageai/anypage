use std::collections::HashSet;
use std::hash::Hash;
use std::sync::{Arc, Condvar, Mutex};

struct State<K> {
    wanted: Vec<K>,
    inflight: HashSet<K>,
    shutdown: bool,
}

/// Queue between the render side and raster workers.
///
/// The render side republishes its *whole* prioritised wanted list whenever the
/// view changes. A worker always takes the head of the latest list, so a request
/// that fell off the list (the camera moved on, or a parameter changed and the
/// key now carries a newer version) is cancelled without any explicit message.
pub struct RasterQueue<K> {
    state: Mutex<State<K>>,
    cv: Condvar,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct QueueSnapshot {
    pub wanted: usize,
    pub inflight: usize,
}

impl<K: Copy + Eq + Hash> RasterQueue<K> {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State { wanted: Vec::new(), inflight: HashSet::new(), shutdown: false }),
            cv: Condvar::new(),
        })
    }

    /// Replace the wanted list, highest priority first. Keys already in flight
    /// are dropped here so the caller does not need to track them.
    pub fn publish(&self, wanted: impl IntoIterator<Item = K>) {
        let mut s = self.state.lock().unwrap();
        // stored back to front so workers pop the head in O(1)
        let mut list: Vec<K> = wanted.into_iter().filter(|k| !s.inflight.contains(k)).collect();
        list.reverse();
        if list == s.wanted {
            return;
        }
        s.wanted = list;
        if !s.wanted.is_empty() {
            self.cv.notify_all();
        }
    }

    /// Worker side: block until there is work (returns `None` on shutdown) and
    /// mark the returned key as in flight.
    pub fn take(&self) -> Option<K> {
        let mut s = self.state.lock().unwrap();
        loop {
            if s.shutdown {
                return None;
            }
            if let Some(key) = s.wanted.pop() {
                s.inflight.insert(key);
                return Some(key);
            }
            s = self.cv.wait(s).unwrap();
        }
    }

    /// Render side: the result for `key` has been received.
    pub fn complete(&self, key: K) {
        self.state.lock().unwrap().inflight.remove(&key);
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        let s = self.state.lock().unwrap();
        QueueSnapshot { wanted: s.wanted.len(), inflight: s.inflight.len() }
    }

    pub fn shutdown(&self) {
        self.state.lock().unwrap().shutdown = true;
        self.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::TileKey;

    fn k(tx: i32) -> TileKey {
        TileKey { page: 0, level: 0, tx, ty: 0 }
    }

    #[test]
    fn latest_publish_wins_and_inflight_is_filtered() {
        let q = RasterQueue::new();
        q.publish([k(1), k(2), k(3)]);
        assert_eq!(q.take(), Some(k(1)));
        // view moved: 1 is in flight, 2 is no longer wanted, 9 is new and urgent
        q.publish([k(9), k(1), k(3)]);
        assert_eq!(q.snapshot().wanted, 2);
        assert_eq!(q.take(), Some(k(9)));
        q.complete(k(1));
        q.complete(k(9));
        assert_eq!(q.snapshot().inflight, 0);
        q.shutdown();
        assert_eq!(q.take(), None);
    }
}
