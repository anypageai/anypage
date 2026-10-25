//! What a page gives the engine. The engine never knows what the content is;
//! it sees cells, rectangles, tile sources, GPU operators and GPU layers.

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

/// Where a [`GpuRenderer`] draws: an offscreen texture the engine then
/// composites like a panel. Output premultiplied alpha.
pub struct GpuTarget<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub view: &'a wgpu::TextureView,
    pub format: wgpu::TextureFormat,
    /// Pixels.
    pub size: (u32, u32),
}

/// Native GPU code compiled into the app (3D scenes, point clouds, video
/// colour conversion). Called only when the layer's cell value or its pixel
/// size changed; the result is cached as a texture between calls.
pub trait GpuRenderer: Send + Sync + 'static {
    fn render(&self, target: &GpuTarget, value: &Value) -> Result<(), String>;
}

pub struct GpuLayer {
    pub id: u32,
    pub rect: Rect,
    pub cell: CellId,
    pub renderer: Arc<dyn GpuRenderer>,
}

pub struct PageSpec {
    pub bounds: Rect,
    pub panels: Vec<Panel>,
    pub tiled: Vec<TiledLayer>,
    pub effects: Vec<EffectLayer>,
    /// Drawn after tiles, before effects and panels.
    pub layers: Vec<GpuLayer>,
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

/// What a page asks of the engine or, through it, of the host platform.
#[derive(Clone, Debug, PartialEq)]
pub enum PageRequest {
    FitPage,
    /// Fit a world rectangle.
    FitRect(Rect),
    /// Put text on the system clipboard.
    Copy(String),
    /// Read the clipboard; the host answers with [`Page::paste`].
    Paste,
    /// Ask the user for a file; the host answers with [`Page::host_result`].
    OpenFile { id: u64, filters: Vec<String> },
    /// Ask the user where to save `bytes`; answered with [`Page::host_result`].
    SaveFile { id: u64, name: String, bytes: Arc<Vec<u8>> },
    /// Open another page package the app ships (by its folder name).
    Navigate(String),
}

/// The host's answer to a request that needed the user or the platform.
#[derive(Clone, Debug, PartialEq)]
pub enum HostResult {
    File { name: String, bytes: Arc<Vec<u8>> },
    Saved { name: String },
    Cancelled,
    Failed(String),
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
    /// The gesture was taken away (a second finger started a pinch, the OS
    /// cancelled the touch): end it without activating anything.
    fn pointer_cancel(&mut self) {
        self.pointer_up()
    }
    /// The pointer moved with no button held (hover).
    fn pointer_move(&mut self, _wx: f32, _wy: f32) {}
    /// Scroll in lines (positive `dy` = wheel up). Return true to consume it;
    /// otherwise the engine zooms.
    fn wheel(&mut self, _wx: f32, _wy: f32, _dx: f32, _dy: f32) -> bool {
        false
    }
    /// Key names: "a", "1", "space", "enter", "backspace", "left", ... with
    /// modifiers in front: "ctrl+c", "ctrl+shift+z", "shift+left".
    fn key(&mut self, _key: &str) -> bool {
        false
    }
    /// Committed text (typing, IME commit) while the page has text focus.
    fn text_input(&mut self, _text: &str) {}
    /// IME composition in progress; empty when it ends.
    fn ime_preedit(&mut self, _text: &str) {}
    /// Clipboard text the page asked for with [`PageRequest::Paste`].
    fn paste(&mut self, _text: &str) {}
    /// A file dropped onto the window.
    fn file_drop(&mut self, _name: &str, _bytes: Arc<Vec<u8>>) {}
    /// Answer to a [`PageRequest::OpenFile`] or [`PageRequest::SaveFile`].
    fn host_result(&mut self, _id: u64, _result: HostResult) {}
    /// Where the caret is (world), while the page wants text input. The host
    /// shows the keyboard or enables the IME and places its window there.
    fn text_focus(&self) -> Option<Rect> {
        None
    }
    fn take_request(&mut self) -> Option<PageRequest> {
        None
    }
    /// Called after the engine committed results from other threads:
    /// `completed` are async cells whose jobs finished.
    fn after_commit(&mut self, _completed: &[CellId]) {}
    /// A named message from the host or an automation script ("iterations",
    /// 768). Returns whether the page understood it.
    fn control(&mut self, _name: &str, _value: f64) -> bool {
        false
    }
    /// The window in logical pixels (and pixels per logical pixel), at start
    /// and whenever it changes, so the page can choose a layout.
    fn viewport(&mut self, _w: f32, _h: f32, _scale: f32) {}
    /// Semantic port: the page's actions and cells as JSON, for agents.
    fn describe(&self) -> String {
        "{}".into()
    }
    /// Semantic port: run `{"action": name, "params": {...}}` or read
    /// `{"get": cell}`; returns JSON.
    fn act(&mut self, _request: &str) -> String {
        r#"{"error":"this page has no actions"}"#.into()
    }
    fn shutdown(&mut self) {}
}

/// Lets a host pick the page at run time (a script page, a built-in page)
/// while the engine stays monomorphic.
impl Page for Box<dyn Page> {
    fn parts(&mut self) -> (&mut Graph, &PageSpec) {
        (**self).parts()
    }
    fn graph(&self) -> &Graph {
        (**self).graph()
    }
    fn spec(&self) -> &PageSpec {
        (**self).spec()
    }
    fn pointer_down(&mut self, wx: f32, wy: f32) -> PointerResponse {
        (**self).pointer_down(wx, wy)
    }
    fn pointer_drag(&mut self, wx: f32, wy: f32) {
        (**self).pointer_drag(wx, wy)
    }
    fn pointer_up(&mut self) {
        (**self).pointer_up()
    }
    fn pointer_cancel(&mut self) {
        (**self).pointer_cancel()
    }
    fn pointer_move(&mut self, wx: f32, wy: f32) {
        (**self).pointer_move(wx, wy)
    }
    fn wheel(&mut self, wx: f32, wy: f32, dx: f32, dy: f32) -> bool {
        (**self).wheel(wx, wy, dx, dy)
    }
    fn key(&mut self, key: &str) -> bool {
        (**self).key(key)
    }
    fn text_input(&mut self, text: &str) {
        (**self).text_input(text)
    }
    fn ime_preedit(&mut self, text: &str) {
        (**self).ime_preedit(text)
    }
    fn paste(&mut self, text: &str) {
        (**self).paste(text)
    }
    fn file_drop(&mut self, name: &str, bytes: Arc<Vec<u8>>) {
        (**self).file_drop(name, bytes)
    }
    fn host_result(&mut self, id: u64, result: HostResult) {
        (**self).host_result(id, result)
    }
    fn text_focus(&self) -> Option<Rect> {
        (**self).text_focus()
    }
    fn take_request(&mut self) -> Option<PageRequest> {
        (**self).take_request()
    }
    fn after_commit(&mut self, completed: &[CellId]) {
        (**self).after_commit(completed)
    }
    fn control(&mut self, name: &str, value: f64) -> bool {
        (**self).control(name, value)
    }
    fn viewport(&mut self, w: f32, h: f32, scale: f32) {
        (**self).viewport(w, h, scale)
    }
    fn describe(&self) -> String {
        (**self).describe()
    }
    fn act(&mut self, request: &str) -> String {
        (**self).act(request)
    }
    fn shutdown(&mut self) {
        (**self).shutdown()
    }
}
