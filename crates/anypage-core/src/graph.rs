//! The computation graph (docs/ARCHITECTURE.md section 4).
//!
//! There is no split between "static" and "dynamic" content. Everything on a
//! page is a function of cells:
//!
//! * **input** cells are set by commands (user, script, agent, camera);
//! * **stream** cells are input cells fed from other threads, with a bounded
//!   buffer (backpressure) and an append or latest-wins policy;
//! * the **clock** is an input cell holding seconds (`f64`);
//! * **derived** cells apply an operator to other cells, inline or async.
//!
//! Static content is just a function whose inputs never change: computed once,
//! then served from cache. Dynamic content is a function of inputs that change.
//! Efficiency comes from three mechanisms:
//!
//! 1. **Demand.** Nothing is computed until a sink asks; invisible sinks cost nothing.
//! 2. **Early cutoff.** A cell is recomputed only if an input's version changed,
//!    and an operator may declare value equality, so a recomputation that
//!    yields the same value stops propagation there.
//! 3. **Predictable time.** An operator that reads the clock can declare how
//!    long its value stays valid ([`EvalCtx::valid_until`]). Advancing the
//!    clock before that moment does not re-run it, and
//!    [`Graph::next_expiry`] tells the scheduler exactly when to wake: every
//!    frame for an animation, once a second for a clock, never for a stopped
//!    animation.
//!
//! Results and stream items produced on other threads enter the graph only at
//! [`Graph::commit`], so the graph is single-threaded and every frame sees one
//! consistent version.

use std::any::Any;
use std::cell::Cell as StdCell;
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::marker::PhantomData;
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
    expires: StdCell<f64>,
    declared: StdCell<bool>,
}

impl EvalCtx {
    fn new() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(AtomicU32::new(0)),
            expires: StdCell::new(f64::INFINITY),
            declared: StdCell::new(false),
        }
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    pub fn set_progress(&self, p: f32) {
        self.progress.store((p.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
    }
    /// The value just computed stays correct while the clock is below `t`.
    /// `f64::INFINITY` means "constant from now on". An operator that reads the
    /// clock without calling this is assumed to change on every tick.
    pub fn valid_until(&self, t: f64) {
        self.declared.set(true);
        self.expires.set(self.expires.get().min(t));
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
    /// Value equality for early cutoff. Default: never equal.
    fn same(&self, _a: &Value, _b: &Value) -> bool {
        false
    }
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

struct MemoOp<T, F> {
    name: String,
    class: OpClass,
    f: F,
    _t: PhantomData<fn() -> T>,
}

impl<T, F> Op for MemoOp<T, F>
where
    T: PartialEq + Any + Send + Sync,
    F: Fn(&[Input], &EvalCtx) -> Result<T, String> + Send + Sync + 'static,
{
    fn name(&self) -> &str {
        &self.name
    }
    fn class(&self) -> OpClass {
        self.class
    }
    fn eval(&self, inputs: &[Input], ctx: &EvalCtx) -> Result<Value, String> {
        (self.f)(inputs, ctx).map(|v| Arc::new(v) as Value)
    }
    fn same(&self, a: &Value, b: &Value) -> bool {
        match (a.downcast_ref::<T>(), b.downcast_ref::<T>()) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        }
    }
}

/// Operator that runs on the evaluating thread.
pub fn op_inline<F>(name: &str, class: OpClass, f: F) -> Arc<dyn Op>
where
    F: Fn(&[Input], &EvalCtx) -> Result<Value, String> + Send + Sync + 'static,
{
    Arc::new(FnOp { name: name.to_string(), class, exec: Exec::Inline, f })
}

/// Inline operator with a typed, comparable result: an unchanged result stops
/// propagation (early cutoff).
pub fn op_memo<T, F>(name: &str, class: OpClass, f: F) -> Arc<dyn Op>
where
    T: PartialEq + Any + Send + Sync,
    F: Fn(&[Input], &EvalCtx) -> Result<T, String> + Send + Sync + 'static,
{
    Arc::new(MemoOp { name: name.to_string(), class, f, _t: PhantomData })
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
    /// `inputs` is shared so recomputation can hold it without copying.
    Derived { op: Arc<dyn Op>, inputs: Arc<[CellId]> },
}

/// Min-heap entry: a clock reader whose value expires at `.0`.
struct Timer(f64, CellId);

impl PartialEq for Timer {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == CmpOrdering::Equal
    }
}
impl Eq for Timer {}
impl PartialOrd for Timer {
    fn partial_cmp(&self, o: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(o))
    }
}
impl Ord for Timer {
    fn cmp(&self, o: &Self) -> CmpOrdering {
        // reversed: BinaryHeap is a max-heap, we want the earliest expiry on top
        o.0.total_cmp(&self.0).then_with(|| o.1.cmp(&self.1))
    }
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
    /// Bumps whenever the value or the status observably changes.
    version: u64,
    status: Status,
    /// "Maybe stale": an input's version may have changed. Verified on demand.
    dirty: bool,
    dependents: Vec<CellId>,
    /// Input versions the current value (or running job) was computed from.
    deps_seen: Vec<u64>,
    job: Option<Job>,
    eval_us: f32,
    /// For cells reading the clock: the value holds while clock < expires.
    expires: f64,
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
    /// Operator runs on the evaluating thread.
    pub inline_evals: u64,
    /// Dirty cells found unchanged without running their operator.
    pub verified: u64,
    /// Operator ran but produced an equal value; propagation stopped.
    pub cutoffs: u64,
    /// Clock advanced but the cell's declared validity had not expired.
    pub clock_skips: u64,
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
    clock: Option<CellId>,
    /// Clock readers ordered by expiry; `tick` only touches the ones due.
    timers: BinaryHeap<Timer>,
    /// Bumps when cells are added; invalidates structural caches.
    structure: u64,
    /// Bumps whenever dirtiness changes from outside evaluation; a saved
    /// traversal is only resumed if nothing was marked since it was saved.
    mutations: u64,
    resume: Option<(CellId, u64, Vec<(CellId, bool)>)>,
    readers_cache: HashMap<CellId, Vec<CellId>>,
    readers_structure: u64,
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
            clock: None,
            timers: BinaryHeap::new(),
            structure: 0,
            mutations: 0,
            resume: None,
            readers_cache: HashMap::new(),
            readers_structure: u64::MAX,
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
            deps_seen: Vec::new(),
            job: None,
            eval_us: 0.0,
            expires: f64::NEG_INFINITY,
        });
        self.structure += 1;
        id
    }

    pub fn input(&mut self, label: &str, v: Value) -> CellId {
        self.push(label, Kind::Input, Some(v), Status::Ready, false)
    }

    /// The page clock: an input cell holding seconds as `f64`.
    pub fn clock(&mut self, label: &str) -> CellId {
        let id = self.input(label, val(0.0f64));
        self.clock = Some(id);
        id
    }

    pub fn clock_id(&self) -> Option<CellId> {
        self.clock
    }

    pub fn clock_now(&self) -> f64 {
        self.clock.and_then(|c| self.get::<f64>(c).copied()).unwrap_or(0.0)
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
        let id = self.push(label, Kind::Derived { op, inputs: Arc::from(inputs) }, None, Status::Empty, true);
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

    /// Command: replace an input's value only if it differs. Returns whether it did.
    pub fn set_if_changed<T: PartialEq + Any + Send + Sync>(&mut self, id: CellId, v: T) -> bool {
        if self.get::<T>(id) == Some(&v) {
            return false;
        }
        self.set(id, val(v));
        true
    }

    /// Advance the clock. Only clock readers whose declared validity has
    /// expired are invalidated (with their dependents); the rest are not even
    /// visited, so a tick costs O(cells due), not O(cells reading time).
    pub fn tick(&mut self, now: f64) -> usize {
        let Some(clock) = self.clock else { return 0 };
        {
            let c = &mut self.cells[clock.index()];
            c.value = Some(val(now));
            c.version += 1;
        }
        let mut marked = 0;
        while let Some(top) = self.timers.peek() {
            if top.0 > now {
                break;
            }
            let Timer(at, id) = self.timers.pop().expect("peeked");
            let c = &mut self.cells[id.index()];
            if c.expires != at || c.dirty {
                continue; // stale entry, or already scheduled for verification
            }
            c.dirty = true;
            self.stats.dirty_marks += 1;
            marked += 1 + self.invalidate_dependents(id);
        }
        marked
    }

    /// Mark everything downstream of `id` as maybe-stale. Stops at cells that
    /// are already dirty: their dependents are dirty by construction. Jobs are
    /// not cancelled here, because a dirty cell may still verify unchanged.
    fn invalidate_dependents(&mut self, id: CellId) -> usize {
        self.mutations += 1;
        let mut marked = 0;
        let mut stack: Vec<CellId> = self.cells[id.index()].dependents.clone();
        while let Some(d) = stack.pop() {
            let c = &mut self.cells[d.index()];
            if c.dirty {
                continue;
            }
            c.dirty = true;
            marked += 1;
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

    fn set_status(&mut self, id: CellId, s: Status) -> bool {
        let c = &mut self.cells[id.index()];
        if c.status != s {
            c.status = s;
            c.version += 1;
            true
        } else {
            false
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
            c.expires = f64::INFINITY;
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
            let progressed = {
                let c = &mut self.cells[id.index()];
                match &mut c.job {
                    Some(job) => {
                        let p = job.progress.load(Ordering::Relaxed);
                        if p != job.seen_progress {
                            job.seen_progress = p;
                            Some(p)
                        } else {
                            None
                        }
                    }
                    None => None,
                }
            };
            if let Some(p) = progressed {
                self.set_status(id, Status::Pending { progress: p as f32 / 1000.0 });
                rep.progressed += 1;
                rep.dirty_marked += self.invalidate_dependents(id);
            }
        }

        rep.commit_us = t0.elapsed().as_secs_f32() * 1e6;
        rep
    }

    /// Sink side: bring `id` up to date (or start its async job) and return it.
    pub fn demand(&mut self, id: CellId) -> Input {
        self.ensure(id, None);
        self.view(id)
    }

    /// Like [`Graph::demand`], but stops at `deadline`. Work done so far is
    /// kept; the next call continues from there. Returns `None` if unfinished.
    pub fn demand_until(&mut self, id: CellId, deadline: Instant) -> Option<Input> {
        if self.ensure(id, Some(deadline)) {
            Some(self.view(id))
        } else {
            None
        }
    }

    pub fn view(&self, id: CellId) -> Input {
        let c = &self.cells[id.index()];
        Input { value: c.value.clone(), status: c.status.clone(), version: c.version }
    }

    /// Iterative post-order walk so deep graphs cannot overflow the stack.
    /// Returns false if the deadline stopped it before `root` was settled.
    fn ensure(&mut self, root: CellId, deadline: Option<Instant>) -> bool {
        if !self.cells[root.index()].dirty {
            return true;
        }
        let mut stack = match self.resume.take() {
            Some((r, m, s)) if r == root && m == self.mutations => s,
            _ => vec![(root, false)],
        };
        let mut steps = 0u32;
        while let Some((id, expanded)) = stack.pop() {
            if !self.cells[id.index()].dirty {
                continue;
            }
            if expanded {
                self.recompute(id);
                steps += 1;
                if let Some(d) = deadline {
                    if steps % 32 == 0 && Instant::now() >= d {
                        if self.cells[root.index()].dirty {
                            self.resume = Some((root, self.mutations, stack));
                            return false;
                        }
                        return true;
                    }
                }
                continue;
            }
            stack.push((id, true));
            if let Kind::Derived { inputs, .. } = &self.cells[id.index()].kind {
                for &i in inputs.iter() {
                    if self.cells[i.index()].dirty {
                        stack.push((i, false));
                    }
                }
            }
        }
        true
    }

    /// Verify a dirty derived cell whose inputs are settled, and recompute it
    /// only when something it actually depends on has changed.
    fn recompute(&mut self, id: CellId) {
        let (op, input_ids) = match &self.cells[id.index()].kind {
            Kind::Derived { op, inputs } => (op.clone(), inputs.clone()),
            _ => {
                self.cells[id.index()].dirty = false;
                return;
            }
        };
        let clock_now = self.clock_now();

        // 1) verification, allocation-free: did anything this cell read change?
        let (computed, other_changed, clock_changed) = {
            let c = &self.cells[id.index()];
            let computed = c.deps_seen.len() == input_ids.len() && !matches!(c.status, Status::Empty);
            let mut other = false;
            let mut clock = false;
            if computed {
                for (k, &i) in input_ids.iter().enumerate() {
                    if c.deps_seen[k] != self.cells[i.index()].version {
                        if Some(i) == self.clock {
                            clock = true;
                        } else {
                            other = true;
                            break;
                        }
                    }
                }
            }
            (computed, other, clock)
        };
        if computed && !other_changed && (!clock_changed || clock_now < self.cells[id.index()].expires) {
            for (k, &i) in input_ids.iter().enumerate() {
                let v = self.cells[i.index()].version;
                self.cells[id.index()].deps_seen[k] = v;
            }
            self.cells[id.index()].dirty = false;
            if clock_changed {
                self.stats.clock_skips += 1;
            } else {
                self.stats.verified += 1;
            }
            return;
        }
        let versions: Vec<u64> = input_ids.iter().map(|i| self.cells[i.index()].version).collect();

        let inputs: Vec<Input> = input_ids.iter().map(|&i| self.view(i)).collect();
        let reads_clock = self.clock.map_or(false, |c| input_ids.contains(&c));

        match op.exec() {
            Exec::Inline => {
                let ctx = EvalCtx::new();
                let t = Instant::now();
                let r = op.eval(&inputs, &ctx);
                let us = t.elapsed().as_secs_f32() * 1e6;
                let expires = if ctx.declared.get() {
                    ctx.expires.get()
                } else if reads_clock {
                    clock_now // stale at the next tick
                } else {
                    f64::INFINITY
                };
                self.stats.inline_evals += 1;
                if reads_clock && expires.is_finite() {
                    self.timers.push(Timer(expires, id));
                }
                let c = &mut self.cells[id.index()];
                c.dirty = false;
                c.eval_us = us;
                c.deps_seen = versions;
                c.expires = expires;
                match r {
                    Ok(v) => {
                        let same = matches!(c.status, Status::Ready) && c.value.as_ref().map_or(false, |old| op.same(old, &v));
                        if same {
                            self.stats.cutoffs += 1;
                        } else {
                            c.value = Some(v);
                            c.status = Status::Ready;
                            c.version += 1;
                        }
                    }
                    Err(e) => {
                        let e = Status::Error(e);
                        if c.status != e {
                            c.status = e;
                            c.version += 1;
                        }
                    }
                }
            }
            Exec::Async => {
                // inputs changed: the running job (if any) computes from stale data
                if let Some(job) = self.cells[id.index()].job.take() {
                    job.cancel.store(true, Ordering::Relaxed);
                    self.stats.async_cancelled += 1;
                }
                // wait until inputs settle; their commit marks this cell dirty again
                if inputs.iter().any(|i| matches!(i.status, Status::Pending { .. } | Status::Empty)) {
                    self.cells[id.index()].dirty = false;
                    self.set_status(id, Status::Pending { progress: 0.0 });
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
                {
                    let c = &mut self.cells[id.index()];
                    c.dirty = false;
                    c.deps_seen = versions;
                    c.job = Some(Job { gen, cancel, progress, seen_progress: 0 });
                }
                self.set_status(id, Status::Pending { progress: 0.0 });
                self.pending.push(id);
                self.stats.async_started += 1;
            }
        }
    }

    // --- predictable time -----------------------------------------------------

    /// Cells upstream of `sink` (inclusive) that read the clock directly.
    /// Cached until the graph's structure changes.
    pub fn clock_readers(&mut self, sink: CellId) -> &[CellId] {
        if self.readers_structure != self.structure {
            self.readers_cache.clear();
            self.readers_structure = self.structure;
        }
        if !self.readers_cache.contains_key(&sink) {
            let mut found = Vec::new();
            if let Some(clock) = self.clock {
                let mut seen = HashSet::new();
                let mut stack = vec![sink];
                while let Some(id) = stack.pop() {
                    if !seen.insert(id) {
                        continue;
                    }
                    if let Kind::Derived { inputs, .. } = &self.cells[id.index()].kind {
                        if inputs.contains(&clock) {
                            found.push(id);
                        }
                        stack.extend_from_slice(inputs);
                    }
                }
            }
            self.readers_cache.insert(sink, found);
        }
        &self.readers_cache[&sink]
    }

    /// The clock time at which any of these sinks may next change because of
    /// time alone. `f64::INFINITY`: nothing they show depends on time any more.
    /// A value at or before the current clock means "every frame".
    pub fn next_expiry(&mut self, sinks: &[CellId]) -> f64 {
        let mut next = f64::INFINITY;
        for &s in sinks {
            let readers: Vec<CellId> = self.clock_readers(s).to_vec();
            for r in readers {
                let c = &self.cells[r.index()];
                let e = if c.deps_seen.is_empty() { f64::NEG_INFINITY } else { c.expires };
                next = next.min(e);
            }
        }
        next
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
    pub fn expires(&self, id: CellId) -> f64 {
        self.cells[id.index()].expires
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
            Kind::Derived { inputs, .. } => &inputs[..],
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
    fn early_cutoff_stops_propagation() {
        let mut g = graph();
        let x = g.input("x", val(200i64));
        let clamp = g.derive("clamp", op_memo("clamp", OpClass::Native, |i, _| Ok((*i[0].get::<i64>().unwrap()).min(100))), &[x]);
        let heavy = g.derive("heavy", sum_op(), &[clamp]);
        assert_eq!(*g.demand(heavy).get::<i64>().unwrap(), 100);
        let evals = g.stats.inline_evals;
        g.set(x, val(300i64)); // clamp still yields 100
        assert_eq!(*g.demand(heavy).get::<i64>().unwrap(), 100);
        assert_eq!(g.stats.inline_evals - evals, 1, "only clamp re-ran");
        assert_eq!(g.stats.cutoffs, 1);
        assert_eq!(g.stats.verified, 1, "heavy verified without running");
    }

    #[test]
    fn declared_validity_skips_clock_ticks_and_predicts_next_wake() {
        let mut g = graph();
        let clock = g.clock("t");
        // a 1 Hz clock display: value changes only at whole seconds
        let secs = g.derive(
            "secs",
            op_memo("secs", OpClass::Native, |i, ctx| {
                let t = *i[0].get::<f64>().unwrap();
                ctx.valid_until(t.floor() + 1.0);
                Ok(t.floor() as i64)
            }),
            &[clock],
        );
        let view = g.derive("view", sum_op(), &[secs]);
        g.tick(0.10);
        assert_eq!(*g.demand(view).get::<i64>().unwrap(), 0);
        assert_eq!(g.next_expiry(&[view]), 1.0);
        let evals = g.stats.inline_evals;
        for k in 1..=9 {
            assert_eq!(g.tick(0.10 + k as f64 * 0.09), 0, "sub-second ticks touch nothing");
            g.demand(view);
        }
        assert_eq!(g.stats.inline_evals, evals, "no re-evaluation before the second turns");
        assert!(g.tick(1.02) >= 1);
        assert_eq!(*g.demand(view).get::<i64>().unwrap(), 1);
        assert_eq!(g.next_expiry(&[view]), 2.0);
    }

    #[test]
    fn undeclared_clock_reader_is_every_frame_and_constant_is_never() {
        let mut g = graph();
        let clock = g.clock("t");
        let anim = g.derive("anim", op_inline("anim", OpClass::Native, |i, _| Ok(val(*i[0].get::<f64>().unwrap()))), &[clock]);
        let still = g.derive(
            "still",
            op_inline("still", OpClass::Native, |_, ctx| {
                ctx.valid_until(f64::INFINITY);
                Ok(val(0.0f64))
            }),
            &[clock],
        );
        g.tick(5.0);
        g.demand(anim);
        g.demand(still);
        assert!(g.next_expiry(&[anim]) <= 5.0, "animation wants every frame");
        assert_eq!(g.next_expiry(&[still]), f64::INFINITY, "constant never wakes");
    }

    #[test]
    fn budgeted_demand_resumes_across_calls() {
        let mut g = graph();
        let root = g.input("root", val(0i64));
        let slow = op_inline("slow", OpClass::Native, |inp, _| {
            std::thread::sleep(Duration::from_micros(200));
            Ok(val(inp[0].get::<i64>().unwrap() + 1))
        });
        let mut prev = root;
        for _ in 0..200 {
            prev = g.derive("n", slow.clone(), &[prev]);
        }
        let mut calls = 0;
        let v = loop {
            calls += 1;
            if let Some(v) = g.demand_until(prev, Instant::now() + Duration::from_millis(5)) {
                break v;
            }
            assert!(calls < 1000);
        };
        assert_eq!(*v.get::<i64>().unwrap(), 200);
        assert!(calls > 1, "40 ms of work in 5 ms slices needs several calls");
        assert_eq!(g.stats.inline_evals, 200, "no work repeated across slices");
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
        g.demand(y);
        assert_eq!(g.stats.async_cancelled, 1);
        assert_eq!(*settle(&mut g, y).get::<i64>().unwrap(), 7);
    }

    #[test]
    fn unchanged_inputs_do_not_restart_async_job() {
        let mut g = graph();
        let x = g.input("x", val(5i64));
        let y = g.derive("y", op_async("id", |inp, _| Ok(val(*inp[0].get::<i64>().unwrap()))), &[x]);
        assert_eq!(*settle(&mut g, y).get::<i64>().unwrap(), 5);
        let started = g.stats.async_started;
        assert!(!g.set_if_changed(x, 5i64));
        g.demand(y);
        assert_eq!(g.stats.async_started, started);
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
