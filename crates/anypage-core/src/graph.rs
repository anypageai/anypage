//! The computation graph (docs/ARCHITECTURE.md section 4).
//!
//! A page is a graph of cells:
//!
//! * **input** cells are set by commands (user, script, agent, clock, camera);
//! * **stream** cells are input cells fed by producers on other threads, with a
//!   bounded buffer (backpressure) and an append or latest-wins policy;
//! * **derived** cells are an operator applied to other cells. Inline operators
//!   run on the evaluating thread; async operators run on the worker pool and
//!   the cell is *pending* until the result is committed.
//!
//! Evaluation is demand-driven: nothing is computed until a sink asks for it
//! with [`Graph::demand`]. Changing a cell marks its dependents dirty, and
//! cancels any async job that was computing from the old inputs. Results and
//! stream items produced on other threads enter the graph only at
//! [`Graph::commit`], so the graph itself is single-threaded and every frame
//! sees a consistent version of everything.

use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Instant;

use crate::pool::ThreadPool;

/// Type-erased, immutable, shareable cell value.
pub type Value = Arc<dyn Any + Send + Sync>;

/// Called from any thread when something is ready to be committed.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

pub fn val<T: Any + Send + Sync>(v: T) -> Value {
    Arc::new(v)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CellId(u32);

impl CellId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Where an operator's work happens (section 4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpClass {
    Native,
    Gpu,
    Script,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exec {
    Inline,
    Async,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Empty,
    Ready,
    Pending { progress: f32 },
    Error(String),
}

/// What an operator sees of one of its inputs.
#[derive(Clone)]
pub struct Input {
    pub value: Option<Value>,
    pub status: Status,
    pub version: u64,
}

impl Input {
    pub fn get<T: Any>(&self) -> Option<&T> {
        self.value.as_deref().and_then(|v| v.downcast_ref::<T>())
    }
    pub fn is_ready(&self) -> bool {
        matches!(self.status, Status::Ready)
    }
}

pub struct EvalCtx {
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
}

impl EvalCtx {
    fn new() -> Self {
        Self { cancel: Arc::new(AtomicBool::new(false)), progress: Arc::new(AtomicU32::new(0)) }
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    pub fn set_progress(&self, p: f32) {
        self.progress.store((p.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
    }
}

pub trait Op: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn class(&self) -> OpClass {
        OpClass::Native
    }
    fn exec(&self) -> Exec {
        Exec::Inline
    }
    fn eval(&self, inputs: &[Input], ctx: &EvalCtx) -> Result<Value, String>;
}

struct FnOp<F> {
    name: String,
    class: OpClass,
    exec: Exec,
    f: F,
}

impl<F> Op for FnOp<F>
where
    F: Fn(&[Input], &EvalCtx) -> Result<Value, String> + Send + Sync + 'static,
{
    fn name(&self) -> &str {
        &self.name
    }
    fn class(&self) -> OpClass {
        self.class
    }
    fn exec(&self) -> Exec {
        self.exec
    }
    fn eval(&self, inputs: &[Input], ctx: &EvalCtx) -> Result<Value, String> {
        (self.f)(inputs, ctx)
    }
}

/// Operator that runs on the evaluating thread.
pub fn op_inline<F>(name: &str, class: OpClass, f: F) -> Arc<dyn Op>
where
    F: Fn(&[Input], &EvalCtx) -> Result<Value, String> + Send + Sync + 'static,
{
    Arc::new(FnOp { name: name.to_string(), class, exec: Exec::Inline, f })
}

/// Operator that runs on the worker pool; its cell is pending until commit.
pub fn op_async<F>(name: &str, f: F) -> Arc<dyn Op>
where
    F: Fn(&[Input], &EvalCtx) -> Result<Value, String> + Send + Sync + 'static,
{
    Arc::new(FnOp { name: name.to_string(), class: OpClass::Native, exec: Exec::Async, f })
}

// ---------------------------------------------------------------------------
// streams

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamPolicy {
    /// Keep the last `cap` items (logs, token streams, time series).
    Append,
    /// Keep only the newest item (cursor positions, sensor state).
    LatestWins,
}

/// The value of a stream cell after a commit.
pub struct StreamSnapshot<T> {
    pub items: Vec<T>,
    /// Items ever received, including ones that scrolled out of `items`.
    pub total: u64,
}

trait StreamBuf: Send {
    fn drain(&mut self) -> Option<(Value, usize)>;
    fn dropped(&self) -> u64;
}

struct StreamState<T> {
    rx: Receiver<T>,
    ring: VecDeque<T>,
    cap: usize,
    policy: StreamPolicy,
    total: u64,
    dropped: Arc<AtomicU64>,
}

impl<T: Clone + Send + Sync + 'static> StreamBuf for StreamState<T> {
    fn drain(&mut self) -> Option<(Value, usize)> {
        let mut n = 0;
        while let Ok(item) = self.rx.try_recv() {
            n += 1;
            self.total += 1;
            match self.policy {
                StreamPolicy::LatestWins => {
                    self.ring.clear();
                    self.ring.push_back(item);
                }
                StreamPolicy::Append => {
                    if self.ring.len() >= self.cap {
                        self.ring.pop_front();
                    }
                    self.ring.push_back(item);
                }
            }
        }
        if n == 0 {
            return None;
        }
        let snap = StreamSnapshot { items: self.ring.iter().cloned().collect::<Vec<T>>(), total: self.total };
        Some((Arc::new(snap) as Value, n))
    }
    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Producer side of a stream cell; cheap to clone, usable from any thread.
pub struct StreamProducer<T> {
    tx: SyncSender<T>,
    dropped: Arc<AtomicU64>,
    wake: Option<Wake>,
}

impl<T> Clone for StreamProducer<T> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone(), dropped: self.dropped.clone(), wake: self.wake.clone() }
    }
}

impl<T> StreamProducer<T> {
    /// Non-blocking. Returns false and counts a drop when the buffer between
    /// commits is full (backpressure), or when the graph is gone.
    pub fn push(&self, item: T) -> bool {
        match self.tx.try_send(item) {
            Ok(()) => {
                if let Some(w) = &self.wake {
                    w();
                }
                true
            }
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// graph

enum Kind {
    Input,
    Stream(Box<dyn StreamBuf>),
    Derived { op: Arc<dyn Op>, inputs: Vec<CellId> },
}

struct Job {
    gen: u64,
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
    seen_progress: u32,
}

struct Cell {
    label: String,
    kind: Kind,
    value: Option<Value>,
    version: u64,
    status: Status,
    dirty: bool,
    dependents: Vec<CellId>,
    job: Option<Job>,
    eval_us: f32,
}

struct Completion {
    cell: CellId,
    gen: u64,
    result: Result<Value, String>,
    finished: Instant,
    eval_us: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GraphStats {
    pub inline_evals: u64,
    pub async_started: u64,
    pub async_completed: u64,
    pub async_cancelled: u64,
    pub async_discarded: u64,
    pub dirty_marks: u64,
    pub stream_items: u64,
}

#[derive(Debug, Default)]
pub struct CommitReport {
    pub stream_items: usize,
    pub streams_changed: Vec<CellId>,
    /// Async cells whose result was committed, with the time the job finished.
    pub completed: Vec<(CellId, Instant)>,
    pub progressed: usize,
    pub dirty_marked: usize,
    pub commit_us: f32,
}

pub struct Graph {
    cells: Vec<Cell>,
    streams: Vec<CellId>,
    pending: Vec<CellId>,
    tx: Sender<Completion>,
    rx: Receiver<Completion>,
    pool: Arc<ThreadPool>,
    wake: Option<Wake>,
    next_gen: u64,
    pub stats: GraphStats,
}

impl Graph {
    pub fn new(pool: Arc<ThreadPool>) -> Self {
        let (tx, rx) = channel();
        Self {
            cells: Vec::new(),
            streams: Vec::new(),
            pending: Vec::new(),
            tx,
            rx,
            pool,
            wake: None,
            next_gen: 1,
            stats: GraphStats::default(),
        }
    }

    /// Set before creating streams; producers and async jobs call it when they
    /// have something for the next commit.
    pub fn set_wake(&mut self, wake: Wake) {
        self.wake = Some(wake);
    }

    fn push(&mut self, label: &str, kind: Kind, value: Option<Value>, status: Status, dirty: bool) -> CellId {
        let id = CellId(self.cells.len() as u32);
        self.cells.push(Cell {
            label: label.to_string(),
            kind,
            value,
            version: 1,
            status,
            dirty,
            dependents: Vec::new(),
            job: None,
            eval_us: 0.0,
        });
        id
    }

    pub fn input(&mut self, label: &str, v: Value) -> CellId {
        self.push(label, Kind::Input, Some(v), Status::Ready, false)
    }

    /// `cap`: items kept in the snapshot. `bound`: items buffered between
    /// commits before producers start dropping.
    pub fn stream<T: Clone + Send + Sync + 'static>(
        &mut self,
        label: &str,
        cap: usize,
        bound: usize,
        policy: StreamPolicy,
    ) -> (CellId, StreamProducer<T>) {
        let (tx, rx) = sync_channel::<T>(bound.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        let state = StreamState {
            rx,
            ring: VecDeque::with_capacity(cap.clamp(1, 4096)),
            cap: cap.max(1),
            policy,
            total: 0,
            dropped: dropped.clone(),
        };
        let empty: Value = Arc::new(StreamSnapshot::<T> { items: Vec::new(), total: 0 });
        let id = self.push(label, Kind::Stream(Box::new(state)), Some(empty), Status::Ready, false);
        self.streams.push(id);
        (id, StreamProducer { tx, dropped, wake: self.wake.clone() })
    }

    pub fn derive(&mut self, label: &str, op: Arc<dyn Op>, inputs: &[CellId]) -> CellId {
        let id = self.push(label, Kind::Derived { op, inputs: inputs.to_vec() }, None, Status::Empty, true);
        for &i in inputs {
            self.cells[i.index()].dependents.push(id);
        }
        id
    }

    /// Command: replace an input cell's value. Returns how many cells became dirty.
    pub fn set(&mut self, id: CellId, v: Value) -> usize {
        let c = &mut self.cells[id.index()];
        assert!(matches!(c.kind, Kind::Input), "set() on non-input cell `{}`", c.label);
        c.value = Some(v);
        c.version += 1;
        c.status = Status::Ready;
        self.invalidate_dependents(id)
    }

    /// Mark everything downstream of `id` dirty and cancel jobs computing from
    /// stale inputs. Stops at cells that are already dirty: their dependents
    /// are dirty by construction.
    fn invalidate_dependents(&mut self, id: CellId) -> usize {
        let mut marked = 0;
        let mut stack: Vec<CellId> = self.cells[id.index()].dependents.clone();
        while let Some(d) = stack.pop() {
            let c = &mut self.cells[d.index()];
            if c.dirty {
                continue;
            }
            c.dirty = true;
            marked += 1;
            if let Some(job) = c.job.take() {
                job.cancel.store(true, Ordering::Relaxed);
                self.stats.async_cancelled += 1;
            }
            stack.extend_from_slice(&c.dependents);
        }
        self.stats.dirty_marks += marked as u64;
        marked
    }

    /// Ask a running async job to stop. Its cell ends in `Status::Error`.
    pub fn cancel(&mut self, id: CellId) -> bool {
        match &self.cells[id.index()].job {
            Some(job) => {
                job.cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    pub fn cancel_all(&mut self) {
        for id in self.pending.clone() {
            self.cancel(id);
        }
    }

    /// Bring stream items, async results and job progress produced on other
    /// threads into the graph, as one batch.
    pub fn commit(&mut self) -> CommitReport {
        let t0 = Instant::now();
        let mut rep = CommitReport::default();

        for k in 0..self.streams.len() {
            let id = self.streams[k];
            let drained = match &mut self.cells[id.index()].kind {
                Kind::Stream(buf) => buf.drain(),
                _ => None,
            };
            if let Some((v, n)) = drained {
                let c = &mut self.cells[id.index()];
                c.value = Some(v);
                c.version += 1;
                c.status = Status::Ready;
                rep.stream_items += n;
                rep.streams_changed.push(id);
                self.stats.stream_items += n as u64;
                rep.dirty_marked += self.invalidate_dependents(id);
            }
        }

        while let Ok(done) = self.rx.try_recv() {
            let c = &mut self.cells[done.cell.index()];
            let current = matches!(&c.job, Some(j) if j.gen == done.gen);
            if !current {
                self.stats.async_discarded += 1;
                continue;
            }
            c.job = None;
            c.eval_us = done.eval_us;
            match done.result {
                Ok(v) => {
                    c.value = Some(v);
                    c.status = Status::Ready;
                }
                Err(e) => c.status = Status::Error(e),
            }
            c.version += 1;
            self.stats.async_completed += 1;
            rep.completed.push((done.cell, done.finished));
            rep.dirty_marked += self.invalidate_dependents(done.cell);
        }

        let cells = &self.cells;
        self.pending.retain(|id| cells[id.index()].job.is_some());
        for k in 0..self.pending.len() {
            let id = self.pending[k];
            let changed = {
                let c = &mut self.cells[id.index()];
                match &mut c.job {
                    Some(job) => {
                        let p = job.progress.load(Ordering::Relaxed);
                        if p != job.seen_progress {
                            job.seen_progress = p;
                            c.status = Status::Pending { progress: p as f32 / 1000.0 };
                            true
                        } else {
                            false
                        }
                    }
                    None => false,
                }
            };
            if changed {
                rep.progressed += 1;
                rep.dirty_marked += self.invalidate_dependents(id);
            }
        }

        rep.commit_us = t0.elapsed().as_secs_f32() * 1e6;
        rep
    }

    /// Sink side: bring `id` up to date (or start its async job) and return it.
    pub fn demand(&mut self, id: CellId) -> Input {
        self.ensure(id);
        self.view(id)
    }

    pub fn view(&self, id: CellId) -> Input {
        let c = &self.cells[id.index()];
        Input { value: c.value.clone(), status: c.status.clone(), version: c.version }
    }

    /// Iterative post-order walk so deep graphs cannot overflow the stack.
    fn ensure(&mut self, root: CellId) {
        if !self.cells[root.index()].dirty {
            return;
        }
        let mut stack = vec![(root, false)];
        while let Some((id, expanded)) = stack.pop() {
            if !self.cells[id.index()].dirty {
                continue;
            }
            if expanded {
                self.recompute(id);
                continue;
            }
            stack.push((id, true));
            if let Kind::Derived { inputs, .. } = &self.cells[id.index()].kind {
                for &i in inputs {
                    if self.cells[i.index()].dirty {
                        stack.push((i, false));
                    }
                }
            }
        }
    }

    fn recompute(&mut self, id: CellId) {
        let (op, input_ids) = match &self.cells[id.index()].kind {
            Kind::Derived { op, inputs } => (op.clone(), inputs.clone()),
            _ => {
                self.cells[id.index()].dirty = false;
                return;
            }
        };
        let inputs: Vec<Input> = input_ids.iter().map(|&i| self.view(i)).collect();

        match op.exec() {
            Exec::Inline => {
                let ctx = EvalCtx::new();
                let t = Instant::now();
                let r = op.eval(&inputs, &ctx);
                let us = t.elapsed().as_secs_f32() * 1e6;
                let c = &mut self.cells[id.index()];
                c.dirty = false;
                c.eval_us = us;
                c.version += 1;
                match r {
                    Ok(v) => {
                        c.value = Some(v);
                        c.status = Status::Ready;
                    }
                    Err(e) => c.status = Status::Error(e),
                }
                self.stats.inline_evals += 1;
            }
            Exec::Async => {
                // An async operator waits until its inputs are settled; the
                // input's commit will mark this cell dirty again.
                if inputs.iter().any(|i| matches!(i.status, Status::Pending { .. } | Status::Empty)) {
                    let c = &mut self.cells[id.index()];
                    c.dirty = false;
                    c.status = Status::Pending { progress: 0.0 };
                    return;
                }
                let gen = self.next_gen;
                self.next_gen += 1;
                let ctx = EvalCtx::new();
                let cancel = ctx.cancel.clone();
                let progress = ctx.progress.clone();
                let tx = self.tx.clone();
                let wake = self.wake.clone();
                self.pool.spawn(move || {
                    let t = Instant::now();
                    let result = if ctx.cancelled() { Err("cancelled".to_string()) } else { op.eval(&inputs, &ctx) };
                    let _ = tx.send(Completion {
                        cell: id,
                        gen,
                        result,
                        finished: Instant::now(),
                        eval_us: t.elapsed().as_secs_f32() * 1e6,
                    });
                    if let Some(w) = wake {
                        w();
                    }
                });
                let c = &mut self.cells[id.index()];
                c.dirty = false;
                c.status = Status::Pending { progress: 0.0 };
                c.job = Some(Job { gen, cancel, progress, seen_progress: 0 });
                self.pending.push(id);
                self.stats.async_started += 1;
            }
        }
    }

    // --- inspection (the graph view of the semantic port starts here) ---

    pub fn len(&self) -> usize {
        self.cells.len()
    }
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
    pub fn is_dirty(&self, id: CellId) -> bool {
        self.cells[id.index()].dirty
    }
    pub fn version(&self, id: CellId) -> u64 {
        self.cells[id.index()].version
    }
    pub fn status(&self, id: CellId) -> &Status {
        &self.cells[id.index()].status
    }
    pub fn label(&self, id: CellId) -> &str {
        &self.cells[id.index()].label
    }
    pub fn eval_us(&self, id: CellId) -> f32 {
        self.cells[id.index()].eval_us
    }
    pub fn get<T: Any>(&self, id: CellId) -> Option<&T> {
        self.cells[id.index()].value.as_deref().and_then(|v| v.downcast_ref::<T>())
    }
    pub fn dirty_count(&self) -> usize {
        self.cells.iter().filter(|c| c.dirty).count()
    }
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
    pub fn stream_dropped(&self) -> u64 {
        self.streams
            .iter()
            .map(|id| match &self.cells[id.index()].kind {
                Kind::Stream(b) => b.dropped(),
                _ => 0,
            })
            .sum()
    }
    /// Inputs of a derived cell (empty for input and stream cells).
    pub fn inputs_of(&self, id: CellId) -> &[CellId] {
        match &self.cells[id.index()].kind {
            Kind::Derived { inputs, .. } => inputs,
            _ => &[],
        }
    }
    pub fn op_name(&self, id: CellId) -> Option<&str> {
        match &self.cells[id.index()].kind {
            Kind::Derived { op, .. } => Some(op.name()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn graph() -> Graph {
        Graph::new(Arc::new(ThreadPool::new(2, "test")))
    }

    fn sum_op() -> Arc<dyn Op> {
        op_inline("sum", OpClass::Native, |inp, _| {
            Ok(val(inp.iter().map(|i| *i.get::<i64>().unwrap()).sum::<i64>()))
        })
    }

    fn settle(g: &mut Graph, id: CellId) -> Input {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            g.commit();
            let v = g.demand(id);
            if !matches!(v.status, Status::Pending { .. }) {
                return v;
            }
            assert!(Instant::now() < deadline, "async cell never settled");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn inline_propagation_and_demand() {
        let mut g = graph();
        let a = g.input("a", val(1i64));
        let b = g.input("b", val(2i64));
        let s = g.derive("s", sum_op(), &[a, b]);
        let unused = g.derive("unused", sum_op(), &[a]);
        assert_eq!(*g.demand(s).get::<i64>().unwrap(), 3);
        // only `s` is newly marked: `unused` was never computed, so it is still dirty
        assert_eq!(g.set(a, val(10i64)), 1);
        assert_eq!(*g.demand(s).get::<i64>().unwrap(), 12);
        assert!(g.is_dirty(unused), "never demanded, never computed");
        assert_eq!(g.stats.inline_evals, 2);
    }

    #[test]
    fn async_completes_at_commit() {
        let mut g = graph();
        let x = g.input("x", val(21i64));
        let op = op_async("double", |inp, _| {
            std::thread::sleep(Duration::from_millis(20));
            Ok(val(inp[0].get::<i64>().unwrap() * 2))
        });
        let y = g.derive("y", op, &[x]);
        let first = g.demand(y);
        assert!(matches!(first.status, Status::Pending { .. }));
        assert_eq!(*settle(&mut g, y).get::<i64>().unwrap(), 42);
    }

    #[test]
    fn changing_input_cancels_running_job() {
        let mut g = graph();
        let x = g.input("x", val(1i64));
        let op = op_async("slow", |inp, ctx| {
            for _ in 0..200 {
                if ctx.cancelled() {
                    return Err("cancelled".into());
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(val(*inp[0].get::<i64>().unwrap()))
        });
        let y = g.derive("y", op, &[x]);
        g.demand(y);
        g.set(x, val(7i64));
        assert_eq!(g.stats.async_cancelled, 1);
        assert_eq!(*settle(&mut g, y).get::<i64>().unwrap(), 7);
    }

    #[test]
    fn stream_append_keeps_tail_and_counts() {
        let mut g = graph();
        let (s, p) = g.stream::<u32>("s", 3, 64, StreamPolicy::Append);
        for i in 1..=5 {
            assert!(p.push(i));
        }
        let rep = g.commit();
        assert_eq!(rep.stream_items, 5);
        let snap = g.get::<StreamSnapshot<u32>>(s).unwrap();
        assert_eq!(snap.items, vec![3, 4, 5]);
        assert_eq!(snap.total, 5);
    }

    #[test]
    fn stream_backpressure_drops_when_full() {
        let mut g = graph();
        let (_s, p) = g.stream::<u32>("s", 8, 2, StreamPolicy::LatestWins);
        assert!(p.push(1));
        assert!(p.push(2));
        assert!(!p.push(3));
        assert_eq!(g.stream_dropped(), 1);
    }

    #[test]
    fn deep_chain_does_not_overflow() {
        let mut g = graph();
        let mut prev = g.input("root", val(0i64));
        let inc = op_inline("inc", OpClass::Native, |inp, _| Ok(val(inp[0].get::<i64>().unwrap() + 1)));
        for _ in 0..50_000 {
            prev = g.derive("n", inc.clone(), &[prev]);
        }
        assert_eq!(*g.demand(prev).get::<i64>().unwrap(), 50_000);
    }
}
