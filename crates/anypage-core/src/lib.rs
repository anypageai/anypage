//! L1 runtime pieces that know nothing about any GPU API, platform or file format.
//!
//! * [`graph`]: the computation graph. Input, stream, derived and async cells,
//!   demand-driven evaluation, cancellation, commit.
//! * [`pool`]: a small worker pool for async operators.
//! * [`Camera`] and [`tiles`]: camera and tile addressing for `Tiled` outputs.
//! * [`RasterQueue`]: prioritised queue with implicit cancellation, for tiles.

pub mod camera;
pub mod graph;
pub mod pool;
pub mod sched;
pub mod tiles;

pub use camera::Camera;
pub use graph::{
    op_async, op_inline, val, CellId, CommitReport, EvalCtx, Exec, Graph, GraphStats, Input, Op, OpClass, Status,
    StreamPolicy, StreamProducer, StreamSnapshot, Value, Wake,
};
pub use pool::ThreadPool;
pub use sched::{QueueSnapshot, RasterQueue};
pub use tiles::*;
