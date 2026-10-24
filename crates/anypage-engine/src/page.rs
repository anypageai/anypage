//! What a page gives the engine. The engine never knows what the content is;
//! it sees cells, rectangles, tile sources and GPU operators.

use std::sync::Arc;

use anypage_core::{CellId, Graph, Rect, TileKey, Value};

use crate::raster::Rgba;

/// How a piece of content changes over time (section 3.1). Only used for
/// labelling, scheduling priority and latency statistics: the engine derives
/// *when* to evaluate from the graph itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Static,
    PullStream,
    PushStream,
    Async,
    Operations,
}

impl Class {
    pub fn color(self) -> Rgba {
        match self {
            Class::Static => [96, 156, 255, 255],
            Class::PullStream => [196, 128, 255, 255],
            Class::PushStream => [72, 204, 140, 255],
            Class::Async => [255, 172, 64, 255],
            Class::Operations => [255, 104, 128, 255],
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Class::Static => "靜態",
            Class::PullStream => "拉式串流",
            Class::PushStream => "推式串流",
            Class::Async => "非同步",
            Class::Operations => "操作",
        }
    }
}

/// A rectangle in world units whose pixels are the `Image` value of `cell`.
pub struct Panel {
    pub id: u32,
    pub rect: Rect,
    pub cell: CellId,
    pub class: Class,
    /// Lower is evaluated first when the frame budget is tight (section 3.2).
    pub priority: u8,
    /// Drawn after GPU effects (labels over an effect).
    pub over_effects: bool,
}

/// A `Tiled` output (section 9): pixels as a pure function of (tile, params).
pub trait TileSource: Send + Sync + 'static {
    /// Premultiplied RGBA8, `TILE_SIZE`², or `None` if `cancelled` said stop.
    fn render(&self, key: TileKey, params: &Value, cancelled: &dyn Fn() -> bool) -> Option<Vec<u8>>;
}

pub struct TiledLayer {
    pub rect: Rect,
    /// Its version is part of every tile's cache key.
    pub params: CellId,
    pub source: Arc<dyn TileSource>,
}

/// A GPU operator: page-supplied WGSL drawn over `rect`. Bindings:
/// `@group(0) @binding(0)` the world camera, `@group(1) @binding(0)`
/// `struct { rect: vec4<f32>, p0: vec4<f32>, p1: vec4<f32> }`.
/// Entry points `vs_main` (6 vertices from `vertex_index`) and `fs_main`.
pub struct EffectLayer {
    pub rect: Rect,
    pub wgsl: String,
    /// Value: [`EffectParams`].
    pub params: CellId,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EffectParams(pub [f32; 8]);

pub struct PageSpec {
    pub bounds: Rect,
    pub panels: Vec<Panel>,
    pub tiled: Vec<TiledLayer>,
    pub effects: Vec<EffectLayer>,
    /// Input cell the engine sets to the current tile level (i32) so panels
    /// can rasterise at the right resolution.
    pub camera_level: Option<CellId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerResponse {
    /// Not on anything interactive: the engine pans.
    Ignored,
    /// A tap was consumed.
    Handled,
    /// The page wants the drag (a slider).
    Captured,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageRequest {
    FitPage,
}

pub trait Page: 'static {
    /// The graph and the spec at once, so the engine can read one and drive the other.
    fn parts(&mut self) -> (&mut Graph, &PageSpec);
    fn graph(&self) -> &Graph;
    fn spec(&self) -> &PageSpec;
    /// World coordinates.
    fn pointer_down(&mut self, _wx: f32, _wy: f32) -> PointerResponse {
        PointerResponse::Ignored
    }
    fn pointer_drag(&mut self, _wx: f32, _wy: f32) {}
    fn pointer_up(&mut self) {}
    /// Lower-case key names: "a", "j", "space", ...
    fn key(&mut self, _key: &str) -> bool {
        false
    }
    fn take_request(&mut self) -> Option<PageRequest> {
        None
    }
    fn shutdown(&mut self) {}
}
