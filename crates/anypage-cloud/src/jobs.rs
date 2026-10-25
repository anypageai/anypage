//! Network jobs (`cloud.http`, `cloud.s3`, `cloud.gcs`) run on a thread of
//! their own, not on the page's job worker.
//!
//! The page's worker waits for the answer and polls the job's cancellation
//! every 20 ms. When the job is cancelled (its inputs changed, the page went
//! away) the worker returns `"cancelled"` at once and is free for the next
//! job; the request thread is interrupted through its [`Token`] (its socket
//! is shut down) and ends within milliseconds. Only a request still resolving
//! the host name or connecting lingers, until the connect timeout; at most
//! [`MAX_LIVE_PER_PAGE`] request threads per page exist at any time.

use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anypage_runtime::{Data, Grants, JobCtx};

use crate::cancel::{self, Token};

/// Request threads (running or still closing) one page may have.
pub const MAX_LIVE_PER_PAGE: usize = 32;

fn live() -> &'static Mutex<HashMap<String, usize>> {
    static L: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Counts one request thread of a page while it lives.
struct Live(String);

impl Live {
    fn acquire(page: &str) -> Result<Live, String> {
        let mut m = live().lock().unwrap_or_else(|p| p.into_inner());
        let n = m.entry(page.to_string()).or_insert(0);
        if *n >= MAX_LIVE_PER_PAGE {
            return Err(format!("this page already has {MAX_LIVE_PER_PAGE} network requests running or closing; try again shortly"));
        }
        *n += 1;
        Ok(Live(page.to_string()))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let mut m = live().lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = m.get_mut(&self.0) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.0);
            }
        }
    }
}

/// Request threads alive for `page` (tests and diagnostics).
pub fn live_requests(page: &str) -> usize {
    live().lock().unwrap_or_else(|p| p.into_inner()).get(page).copied().unwrap_or(0)
}

/// An owned copy of a page's grants, for a request thread.
pub fn clone_grants(g: &Grants) -> Grants {
    Grants { page_id: g.page_id.clone(), caps: g.caps.clone(), secrets: g.secrets.clone() }
}

enum Msg {
    Progress(f32),
    Done(Result<Data, String>),
}

/// Run `op` (a whole network operation) on its own thread, relaying progress
/// to the job and returning `"cancelled"` as soon as the job is cancelled.
pub fn run<F>(j: &JobCtx, what: &'static str, op: F) -> Result<Data, String>
where
    F: FnOnce(&Grants, &dyn Fn() -> bool, &dyn Fn(f32)) -> Result<Data, String> + Send + 'static,
{
    if j.cancelled() {
        return Err("cancelled".into());
    }
    let live = Live::acquire(&j.grants.page_id).map_err(|e| format!("{what}: {e}"))?;
    let grants = clone_grants(j.grants);
    let token = Token::new();
    let (tx, rx) = mpsc::channel();
    let t2 = token.clone();
    std::thread::Builder::new()
        .name(format!("{what} request"))
        .spawn(move || {
            let _live = live;
            let progress_tx = tx.clone();
            let progress = move |p: f32| {
                let _ = progress_tx.send(Msg::Progress(p));
            };
            let r = cancel::scope(&t2, || crate::guarded(what, || op(&grants, &|| t2.is_cancelled(), &progress)));
            let _ = tx.send(Msg::Done(r));
        })
        .map_err(|e| format!("{what}: cannot start a request thread: {e}"))?;
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Msg::Done(r)) => return r,
            Ok(Msg::Progress(p)) => j.progress(p),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(format!("{what}: the request thread ended without an answer")),
        }
        if j.cancelled() {
            token.cancel();
            return Err("cancelled".into());
        }
    }
}
