//! The AnyPage engine (docs/ARCHITECTURE.md sections 3, 7, 8, 9, 14).
//!
//! One scheduler, one clock and one compositor run every kind of content a
//! page holds. A page is a computation graph plus a description of what to
//! show ([`page::PageSpec`]); the engine decides what to evaluate, when to
//! wake and what to redraw. Hosts are thin: they hand the engine a surface,
//! forward input, and call [`Engine::frame`] when it asks for one.

pub mod engine;
pub mod gpu;
pub mod page;
pub mod raster;
pub mod stats;
pub mod tiles;

pub use anypage_core as core;
pub use anypage_core::Wake;
pub use wgpu;

pub use engine::{Engine, EngineConfig, FrameOutcome};
pub use gpu::Target;
pub use page::{
    Class, EffectLayer, EffectParams, GpuLayer, GpuRenderer, GpuTarget, HostResult, Page, PageRequest, PageSpec, Panel,
    PointerResponse, TileSource, TiledLayer,
};
pub use raster::{Canvas, Direction, Image, Rgba, TextLoadOptions, TextRaster, TextSystem};
