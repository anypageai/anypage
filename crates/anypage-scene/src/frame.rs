//! The value a `scene.view` layer draws, converted from the page's inputs
//! `{ scene, camera }` on the graph thread (cheap: no geometry is copied).
//!
//! Scene table:
//!
//! ```lua
//! {
//!   items = {
//!     { model = m,                -- scene.model (items whose model is still loading are skipped)
//!       color = 0xRRGGBBAA,       -- multiplies vertex colours (default white, or grey without colours)
//!       transform = { 16 numbers },-- column-major like glTF, or { translate, rotate, scale }
//!       point_size = 1,           -- pixels; > 1 draws screen-aligned quads
//!       mode = "vertex",          -- "vertex" | "solid" (item colour only) | "height" (ramp along up)
//!       shape = "round",          -- point shape for point_size > 1: "round" | "square"
//!       lit = true,               -- headlight shading for meshes
//!       visible = true },
//!   },
//!   background = 0x101418FF,      -- default transparent
//!   grid = true,                  -- or { step, size, color, axes = true }
//!   point_budget = 30000000,      -- most points drawn per frame (prefix of each shuffled cloud)
//!   msaa = 4,                     -- 1 or 4
//!   ambient = 0.3,
//!   id = "main",                  -- names this scene's stats (scene.stats(id), stream scene.stats {id})
//! }
//! ```
//!
//! `scene` may also be a single model. A camera without `distance` (or no
//! camera) frames the whole scene.

use std::sync::Arc;

use glam::{DMat4, DQuat, DVec3, EulerRot};

use anypage_runtime::Data;

use crate::camera::{vec3_of, Camera};
use crate::model::{Bounds, Model};

pub const MAX_ITEMS: usize = 1024;

#[cfg(any(target_os = "android", target_os = "ios"))]
pub const DEFAULT_POINT_BUDGET: usize = 4_000_000;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub const DEFAULT_POINT_BUDGET: usize = 30_000_000;

#[cfg(any(target_os = "android", target_os = "ios"))]
pub const DEFAULT_MSAA: u32 = 1;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub const DEFAULT_MSAA: u32 = 4;

pub const MESH_GREY: [u8; 4] = [200, 204, 212, 255];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    Vertex,
    Solid,
    Height,
}

#[derive(Clone, Debug)]
pub struct FrameItem {
    pub model: Arc<Model>,
    /// (resource id, version): the GPU cache key.
    pub key: (u64, u64),
    pub transform: DMat4,
    pub color: [u8; 4],
    pub point_size: f32,
    pub mode: ColorMode,
    pub round: bool,
    pub lit: bool,
    /// World bounds (transform applied).
    pub world: Bounds,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Grid {
    pub step: Option<f64>,
    /// Half extent.
    pub size: Option<f64>,
    pub color: [u8; 4],
    pub axes: bool,
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub items: Vec<FrameItem>,
    pub background: [u8; 4],
    pub grid: Option<Grid>,
    pub camera: Camera,
    /// Frame the scene at render time (the camera did not say where to be).
    pub auto_fit: bool,
    pub bounds: Bounds,
    pub point_budget: usize,
    pub msaa: u32,
    pub ambient: f32,
    /// Items skipped because their model is not ready (a job still loading).
    pub pending: usize,
    /// The scene's `id`: its render stats are kept and streamed under it.
    pub id: Option<String>,
}

fn color_of(d: &Data, default: [u8; 4]) -> [u8; 4] {
    match d {
        Data::Nil => default,
        other => anypage_runtime::draw::color(other),
    }
}

/// `{16 numbers}` (column-major) or `{ translate = v3, rotate = v3 degrees (XYZ), scale = n | v3 }`.
pub fn transform_of(d: &Data) -> Result<DMat4, String> {
    match d {
        Data::Nil => Ok(DMat4::IDENTITY),
        Data::List(l) => {
            if l.len() != 16 {
                return Err(format!("transform must have 16 numbers (column-major), got {}", l.len()));
            }
            let mut m = [0f64; 16];
            for (k, v) in l.iter().enumerate() {
                m[k] = v.num().filter(|x| x.is_finite()).ok_or("transform must be 16 finite numbers")?;
            }
            Ok(DMat4::from_cols_array(&m))
        }
        Data::Map(_) => {
            let t = match d.get("translate") {
                Data::Nil => [0.0; 3],
                v => vec3_of(v).ok_or("transform.translate must be {x, y, z}")?,
            };
            let r = match d.get("rotate") {
                Data::Nil => [0.0; 3],
                v => vec3_of(v).ok_or("transform.rotate must be {x, y, z} in degrees")?,
            };
            let s = match d.get("scale") {
                Data::Nil => [1.0; 3],
                Data::Num(n) if n.is_finite() => [*n; 3],
                v => vec3_of(v).ok_or("transform.scale must be a number or {x, y, z}")?,
            };
            let q = DQuat::from_euler(EulerRot::XYZ, r[0].to_radians(), r[1].to_radians(), r[2].to_radians());
            Ok(DMat4::from_scale_rotation_translation(DVec3::from(s), q, DVec3::from(t)))
        }
        other => Err(format!("transform must be 16 numbers or a table, got {other:?}")),
    }
}

/// A `scene.model` resource behind `d`, if it is one.
pub fn model_of(d: &Data) -> Option<(Arc<Model>, (u64, u64))> {
    let r = d.as_res()?;
    if r.kind() != "scene.model" {
        return None;
    }
    Some((r.downcast_arc::<Model>()?, (r.id(), r.version())))
}

/// A not-ready input: `{ state = "pending" | "error", ... }`.
fn not_ready(d: &Data) -> bool {
    matches!(d.get("state").as_str(), Some("pending" | "error"))
}

fn item_of(d: &Data, k: usize) -> Result<Option<FrameItem>, String> {
    let at = |e: String| format!("scene item {k}: {e}");
    let (model_d, opts) = match d {
        Data::Res(_) => (d, &Data::Nil),
        Data::Map(_) => (d.get("model"), d),
        other => return Err(at(format!("expected a table or a model, got {other:?}"))),
    };
    if opts.get("visible").as_bool() == Some(false) || not_ready(model_d) || model_d.is_nil() {
        return Ok(None);
    }
    let (model, key) = model_of(model_d).ok_or_else(|| at(format!("model must be a scene.model (from scene.load, scene.mesh or scene.points), got {model_d:?}")))?;
    let transform = transform_of(opts.get("transform")).map_err(at)?;
    let default_color = if model.has_colors { [255; 4] } else { MESH_GREY };
    let mode = match opts.get("mode").as_str() {
        None | Some("vertex") => ColorMode::Vertex,
        Some("solid") => ColorMode::Solid,
        Some("height") => ColorMode::Height,
        Some(o) => return Err(at(format!("mode must be \"vertex\", \"solid\" or \"height\", not \"{o}\""))),
    };
    let round = match opts.get("shape").as_str() {
        None | Some("round") => true,
        Some("square") => false,
        Some(o) => return Err(at(format!("shape must be \"round\" or \"square\", not \"{o}\""))),
    };
    let world = model.bounds().transform(&transform);
    Ok(Some(FrameItem {
        key,
        transform,
        color: color_of(opts.get("color"), default_color),
        point_size: opts.get("point_size").num_or(1.0).clamp(0.5, 64.0) as f32,
        mode,
        round,
        lit: opts.get("lit").as_bool().unwrap_or(true),
        world,
        model,
    }))
}

impl Frame {
    /// From the layer's input values: `{ scene, camera }`.
    pub fn from_inputs(inputs: &[Data]) -> Result<Frame, String> {
        let scene = inputs.first().unwrap_or(&Data::Nil);
        let cam = inputs.get(1).unwrap_or(&Data::Nil);
        let (camera, placed) = if not_ready(cam) { (Camera::default(), false) } else { Camera::from_data(cam)? };
        let mut f = Frame {
            items: Vec::new(),
            background: [0; 4],
            grid: None,
            camera,
            auto_fit: !placed,
            bounds: Bounds::EMPTY,
            point_budget: DEFAULT_POINT_BUDGET,
            msaa: DEFAULT_MSAA,
            ambient: 0.3,
            pending: 0,
            id: None,
        };
        match scene {
            Data::Nil => {}
            Data::Res(_) => f.items.extend(item_of(scene, 1)?),
            Data::Map(_) if not_ready(scene) => f.pending += 1,
            Data::Map(_) => {
                let items = scene.get("items");
                if items.items().len() > MAX_ITEMS {
                    return Err(format!("{} scene items; the limit is {MAX_ITEMS}", items.items().len()));
                }
                for (k, it) in items.items().iter().enumerate() {
                    match item_of(it, k + 1)? {
                        Some(i) => f.items.push(i),
                        None if not_ready(it) || not_ready(it.get("model")) => f.pending += 1,
                        None => {}
                    }
                }
                f.background = color_of(scene.get("background"), [0; 4]);
                f.grid = match scene.get("grid") {
                    Data::Nil | Data::Bool(false) => None,
                    Data::Bool(true) => Some(Grid { step: None, size: None, color: [128, 136, 150, 110], axes: true }),
                    g @ Data::Map(_) => Some(Grid {
                        step: g.get("step").num().filter(|s| s.is_finite() && *s > 0.0),
                        size: g.get("size").num().filter(|s| s.is_finite() && *s > 0.0),
                        color: color_of(g.get("color"), [128, 136, 150, 110]),
                        axes: g.get("axes").as_bool().unwrap_or(true),
                    }),
                    other => return Err(format!("scene.grid must be true, false or a table, got {other:?}")),
                };
                if let Some(b) = scene.get("point_budget").num() {
                    f.point_budget = b.clamp(0.0, 1e10) as usize;
                }
                if let Some(m) = scene.get("msaa").num() {
                    f.msaa = if m >= 2.0 { 4 } else { 1 };
                }
                if let Some(a) = scene.get("ambient").num() {
                    f.ambient = a.clamp(0.0, 1.0) as f32;
                }
                match scene.get("id") {
                    Data::Nil => {}
                    Data::Str(s) if s.len() <= 128 => f.id = Some(s.to_string()),
                    other => return Err(format!("scene.id must be a string of at most 128 bytes, got {other:?}")),
                }
            }
            other => return Err(format!("scene must be a table or a model, got {other:?}")),
        }
        for it in &f.items {
            f.bounds = f.bounds.union(&it.world);
        }
        Ok(f)
    }

    pub fn total_points(&self) -> usize {
        self.items.iter().map(|i| i.model.point_count()).sum()
    }

    /// How many points of each item to draw: everything within the budget,
    /// otherwise the same fraction of every cloud (a uniform sample, since
    /// clouds are shuffled at load).
    pub fn point_counts(&self) -> Vec<usize> {
        let total = self.total_points();
        self.items
            .iter()
            .map(|i| {
                let n = i.model.point_count();
                if total <= self.point_budget {
                    n
                } else {
                    (n as u128 * self.point_budget as u128 / total as u128) as usize
                }
            })
            .collect()
    }
}
