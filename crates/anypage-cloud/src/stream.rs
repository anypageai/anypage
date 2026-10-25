//! Plumbing shared by the request-driven sources (`cloud.chat`, `cloud.sse`):
//! one controller thread reads the page's request cell, one worker thread
//! per request does the network I/O. A new request cancels the running one
//! (its socket is shut down, so it ends within milliseconds); a superseded
//! worker can never push after its successor.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anypage_runtime::{Data, Sink, SourceCtx};

use crate::cancel::Token;

/// Worker threads (running or still closing) one source may have.
pub const MAX_WORKERS: usize = 4;

/// Serialises pushes against the current request generation.
pub struct Gate {
    gen: Mutex<u64>,
}

impl Default for Gate {
    fn default() -> Self {
        Self { gen: Mutex::new(0) }
    }
}

impl Gate {
    /// Start a new generation (superseding the previous one).
    pub fn next(&self) -> u64 {
        let mut g = self.gen.lock().unwrap_or_else(|p| p.into_inner());
        *g += 1;
        *g
    }

    /// Run `f` only while `gen` is current (under the lock).
    pub fn if_current<R>(&self, gen: u64, f: impl FnOnce() -> R) -> Option<R> {
        let g = self.gen.lock().unwrap_or_else(|p| p.into_inner());
        (*g == gen).then(f)
    }

    pub fn is_current(&self, gen: u64) -> bool {
        *self.gen.lock().unwrap_or_else(|p| p.into_inner()) == gen
    }

    /// Push `item` if `gen` is still current. With `must`, retry for up to 5 s
    /// while the page's buffer is full (the final item of a request must
    /// arrive); gives up when `quit` is set.
    pub fn push(&self, sink: &Sink, gen: u64, item: Data, must: bool, quit: &AtomicBool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.if_current(gen, || sink.push(item.clone())) {
                None => return false,
                Some(true) => return true,
                Some(false) => {}
            }
            if !must || quit.load(Ordering::Relaxed) || Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

/// The guard a source returns: dropping it stops its threads (the
/// controller notices within ~100 ms and cancels the running request).
/// Never joins.
pub struct Guard {
    pub stop: Arc<AtomicBool>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// What the controller loop hands the source for each request.
pub enum Next {
    Request(Data),
    /// `nil`: cancel and wait.
    Cancel,
    Quit,
}

/// Wait for the next request value, cancelling `current` when one comes.
pub fn next_request(ctx: &SourceCtx, stop: &AtomicBool, current: &mut Option<Arc<Token>>) -> Next {
    loop {
        let msg = ctx.requests.recv_timeout(Duration::from_millis(100));
        if ctx.quit() || stop.load(Ordering::Relaxed) || matches!(msg, Err(RecvTimeoutError::Disconnected)) {
            if let Some(c) = current.take() {
                c.cancel();
            }
            return Next::Quit;
        }
        let Ok(req) = msg else { continue };
        if let Some(c) = current.take() {
            c.cancel();
        }
        return if req.is_nil() { Next::Cancel } else { Next::Request(req) };
    }
}

/// Counts a source's worker threads.
#[derive(Clone, Default)]
pub struct Workers {
    live: Arc<AtomicUsize>,
}

/// Held by a worker thread while it lives.
pub struct WorkerSlot(Arc<AtomicUsize>);

impl Drop for WorkerSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Workers {
    pub fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// A slot for a new worker. Earlier workers have all been cancelled when
    /// this is called and normally end within milliseconds; one stuck in a
    /// DNS lookup or TCP connect lingers until the connect timeout, so wait
    /// up to a second for a free slot, then refuse.
    pub fn admit(&self, stop: &AtomicBool) -> Result<WorkerSlot, String> {
        let t0 = Instant::now();
        loop {
            let n = self.live.load(Ordering::SeqCst);
            if n < MAX_WORKERS {
                if self.live.compare_exchange(n, n + 1, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                    return Ok(WorkerSlot(self.live.clone()));
                }
                continue;
            }
            if t0.elapsed() > Duration::from_secs(1) || stop.load(Ordering::Relaxed) {
                return Err(format!("{MAX_WORKERS} earlier requests of this stream are still connecting; try again in a moment"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
