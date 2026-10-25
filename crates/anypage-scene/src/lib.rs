//! 3D module `scene`: meshes and point clouds loaded from glTF/GLB, OBJ, STL,
//! PLY, LAS/LAZ and XYZ, or built by scripts, drawn by the GPU layer
//! `scene.view` with a depth buffer, headlight shading, point budgets for
//! clouds of tens of millions of points, and a grid.
//!
//! Pages declare `"capabilities": { "modules": ["scene"] }`.
//!
//! # Luau API
//!
//! Models are `scene.model` resources; `m:info()` is `scene.info(m)` and so on.
//! Vectors are `{x, y, z}` (or `{1, 2, 3}`), colours packed `0xRRGGBBAA`.
//!
//! | function | |
//! |---|---|
//! | `scene.load(bytes_or_file, format?, opts?)` | model, for files up to 16 MB (larger: the job). `format`: `glb gltf obj stl ply xyz las laz` or `"auto"` (from the bytes, or the file table's `name`). `opts`: `normals = "smooth"\|"flat"` (when the file has none), `flat = true` (discard file normals), `shuffle = false`, `max_points = n` (keep `n` points: a random sample, or every k-th point in file order with `shuffle = false`), `mtl = text` (OBJ materials) |
//! | job `scene.load` | the same off the page thread, any size, with progress and cancellation: `page.job("m", "scene.load", { file_cell }, { format = "las" })`; input 2 may be the format or the options; a `nil` input gives `nil` |
//! | `scene.mesh(mesh_form, opts?)` | model from `{ positions = Bytes(f32 xyz) or {numbers}, indices = Bytes(u32) or {numbers}, normals, colors = Bytes(rgba) or {packed colours} }`; `opts.normals` as above; at most 4 M vertices inline |
//! | `scene.points(points_form, opts?)` | model from `{ positions, colors }`; shuffled unless `opts.shuffle == false`; `opts.max_points`; at most 4 M points inline |
//! | jobs `scene.mesh`, `scene.points` | the same for larger forms, with cancellation: `page.job("m", "scene.points", { form_cell }, opts)` |
//! | `scene.primitive(shape, opts?)` | `"box"` `{size = n or {x,y,z}}`, `"sphere"` `{radius, segments}`, `"cylinder"` `{radius, height, segments}`, `"plane"` `{size}` |
//! | `scene.synthetic{shape, count, seed}` | generated cloud (at most 1 M points inline): `"terrain"`, `"sphere"`, `"cube"`, `"helix"` |
//! | job `scene.synthetic` | any count up to the limit: `page.job("c", "scene.synthetic", {}, { shape, count, seed })` (or a params cell as input 1) |
//! | `m:info()` | `{ kind = "mesh"\|"points"\|"mixed", format, vertices, triangles, points, source_points, parts, colors, normals, shuffled, bytes, bounds = {min, max, center, size, radius} }` |
//! | `m:to_mesh()`, `m:to_points{max}` | the shared mesh / points forms (model coordinates, `f32`); at most 256 MB |
//! | `m:save(format)` | bytes, at most 16 MB inline: `"ply"` (binary, doubles for far coordinates), `"stl"`, `"obj"`, `"xyz"` |
//! | job `scene.save` | any size up to 2 GB: `page.job("b", "scene.save", { model_cell }, { format = "ply" })`; input 2 may be the format |
//! | `scene.camera(t?)` | a full camera table with defaults filled in |
//! | `scene.orbit(cam, dx, dy, speed?)` | new camera; `speed` degrees per unit (default 0.4) |
//! | `scene.zoom(cam, factor)` | new camera, distance times `factor` |
//! | `scene.pan(cam, dx, dy, view_height)` | new camera, target moved so the content follows a drag |
//! | `scene.fit(cam, what, {aspect, margin}?)` | new camera framing `what`: bounds, a model, a list of models or a scene table |
//! | `scene.bounds(what)` | `{min, max, center, size, radius}` of the same kinds of `what` (transforms applied) |
//! | `scene.project(cam, point, w, h)` | `{x, y, depth, visible}`: where a point lands in a `w` x `h` view (labels over the 3D view) |
//! | `scene.transform{translate, rotate, scale}` | 16 numbers (column-major); `rotate` in degrees, XYZ order |
//! | stream `scene.stats` | reactive render stats: `page.stream("st", "scene.stats", { id = "main", latest = true })`; after every render of the scene table with that `id` (no `id`: scenes without one) the stream's `last` is `{ ms, upload_ms, uploaded_bytes, gpu_bytes, points_drawn, points_total, triangles, items, pending, samples, width, height, draw_calls, renders }` |
//! | `scene.stats(id?)` | the same table as a one-off snapshot, at load and in handlers only (it is not a pure function of its inputs) |
//! | `scene.formats()` | the loadable formats |
//!
//! GPU layer `scene.view`: `page.layer{ rect, kind = "scene.view", inputs = { scene_cell, camera_cell } }`.
//! The scene table is described in [`frame`]; the camera in [`camera`]:
//! `{ yaw, pitch, distance, target = {x, y, z}, fov, up = "y"|"z", ortho, near, far }`.
//! Without `distance` (or without a camera) the view frames the whole scene.
//!
//! Point clouds are shuffled at load, so every prefix is a uniform sample:
//! when a scene holds more points than `point_budget` (30 M on desktop, 4 M
//! on phones by default) each cloud draws the same fraction of its points.
//! Limits: 60 M points, 40 M vertices and 40 M triangles per model (flat
//! shading and STL use 3 vertices per triangle, so 13.3 M triangles), 1024
//! items per scene, 4 GB per file, mesh GPU memory per frame 2 GB on
//! desktop and 384 MB on phones ([`render::MAX_MESH_GPU_BYTES`]).
//!
//! Positions are kept relative to an `f64` origin. PLY, LAS/LAZ, XYZ, OBJ
//! and glTF (node transforms included) are recentred in `f64` before being
//! narrowed to `f32`; STL stores `f32` itself.
//!
//! Render stats are kept per scene `id`. The renderer is shared by every
//! page and the engine does not say which page a layer belongs to, so two
//! pages that use the same `id` (or none) see each other's stats (timings
//! and counts only).
//!
//! Not supported: textures and PBR materials (base colour factor and vertex
//! colours only), skinning and animation, external glTF buffers, Draco or
//! meshopt compression, quantized glTF attributes, picking.

pub mod camera;
pub mod export;
pub mod frame;
pub mod load;
pub mod model;
pub mod render;
pub mod synthetic;

use std::sync::Arc;

use anypage_core::val;
use anypage_runtime::{Data, Effect, Natives, Res};

pub use camera::{Camera, Up};
pub use frame::Frame;
pub use load::{LoadOptions, FORMATS};
pub use model::{Bounds, Ctl, Model, NormalMode};
pub use render::{SceneRenderer, Stats};

/// The resource kind of models.
pub const MODEL_KIND: &str = "scene.model";

/// Add the `scene` module with a fresh renderer.
pub fn register(n: &mut Natives) {
    register_with(n, Arc::new(SceneRenderer::new()));
}

/// Wrap a model as the resource scripts hold.
pub fn model_data(m: Model) -> Data {
    Data::Res(Res::new(MODEL_KIND, m))
}

fn model_arg(d: &Data) -> Result<Arc<Model>, String> {
    frame::model_of(d).map(|m| m.0).ok_or_else(|| format!("expected a scene.model, got {}", type_name(d)))
}

fn type_name(d: &Data) -> String {
    match d {
        Data::Nil => "nil".into(),
        Data::Bool(_) => "a boolean".into(),
        Data::Num(_) => "a number".into(),
        Data::Str(_) => "a string".into(),
        Data::Bytes(_) => "bytes".into(),
        Data::List(_) => "a list".into(),
        Data::Map(_) => "a table".into(),
        Data::Res(r) => format!("a {} resource", r.kind()),
    }
}

/// Bytes from `Data::Bytes`, a byte resource, a string, or a file table
/// `{ name, data }` (what `page.open_file` and drops give); plus the name.
pub fn source_arg(d: &Data) -> Result<(load::Bytes, Option<String>), String> {
    Ok(match d {
        Data::Bytes(b) => (b.clone(), None),
        Data::Str(s) => (Arc::new(s.as_bytes().to_vec()), None),
        Data::Res(r) => (r.downcast_arc::<Vec<u8>>().ok_or_else(|| format!("expected bytes, got a {} resource", r.kind()))?, None),
        Data::Map(_) if !d.get("data").is_nil() => {
            let (b, _) = source_arg(d.get("data"))?;
            (b, d.get("name").as_str().map(str::to_string))
        }
        other => return Err(format!("expected bytes or a file, got {}", type_name(other))),
    })
}

/// Load options from a script table.
pub fn load_options(d: &Data) -> Result<LoadOptions, String> {
    let mut o = LoadOptions::default();
    match d {
        Data::Nil => return Ok(o),
        Data::Map(_) => {}
        other => return Err(format!("options must be a table, got {}", type_name(other))),
    }
    if let Some(s) = d.get("normals").as_str() {
        o.normals = Some(NormalMode::parse(Some(s), NormalMode::Smooth)?);
    }
    o.flat = d.get("flat").truthy();
    if let Some(b) = d.get("shuffle").as_bool() {
        o.shuffle = b;
    }
    if let Some(m) = d.get("max_points").num() {
        if m.is_nan() || m < 1.0 {
            return Err("max_points must be at least 1".into());
        }
        o.max_points = Some(m.min(model::MAX_POINTS as f64) as usize);
    }
    match d.get("mtl") {
        Data::Nil => {}
        m => o.mtl = Some(m.as_bytes().ok_or("mtl must be text or bytes")?.to_vec()),
    }
    Ok(o)
}

fn f32s(d: &Data, what: &str, max: usize) -> Result<Vec<f32>, String> {
    match d {
        Data::List(l) => {
            if l.len() > max {
                return Err(format!("{what}: {} numbers; the limit is {max}", l.len()));
            }
            l.iter().map(|x| x.num().map(|v| v as f32).ok_or_else(|| format!("{what} must be numbers"))).collect()
        }
        other => {
            let b = other.as_bytes().ok_or_else(|| format!("{what} must be bytes (f32 little-endian) or a list of numbers"))?;
            if b.len() % 4 != 0 {
                return Err(format!("{what}: {} bytes is not a whole number of f32 values", b.len()));
            }
            if b.len() / 4 > max {
                return Err(format!("{what}: {} values; the limit is {max}", b.len() / 4));
            }
            Ok(b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect())
        }
    }
}

fn vec3s(d: &Data, what: &str, max: usize) -> Result<Vec<[f32; 3]>, String> {
    let v = f32s(d, what, max.saturating_mul(3))?;
    if v.len() % 3 != 0 {
        return Err(format!("{what}: {} values is not a multiple of 3", v.len()));
    }
    Ok(v.as_chunks::<3>().0.to_vec())
}

fn colors_of(d: &Data, n: usize) -> Result<Option<Vec<[u8; 4]>>, String> {
    Ok(match d {
        Data::Nil => None,
        Data::List(l) => {
            if l.len() != n {
                return Err(format!("colors: {} entries for {n} vertices", l.len()));
            }
            Some(l.iter().map(anypage_runtime::draw::color).collect())
        }
        other => {
            let b = other.as_bytes().ok_or("colors must be bytes (r, g, b, a per vertex) or a list of colours")?;
            if b.len() != n * 4 {
                return Err(format!("colors: {} bytes for {n} vertices (4 per vertex)", b.len()));
            }
            Some(b.as_chunks::<4>().0.to_vec())
        }
    })
}

/// Vertices or points in a form's `positions` (bytes or a list), without parsing them.
fn vec3_count(d: &Data) -> usize {
    match d {
        Data::List(l) => l.len() / 3,
        other => other.as_bytes().map(|b| b.len() / 12).unwrap_or(0),
    }
}

/// Inline calls refuse forms over [`INLINE_FORM_VERTICES`] and name the job.
fn inline_form(form: &Data, what: &str) -> Result<(), String> {
    let n = vec3_count(form.get("positions"));
    if n > INLINE_FORM_VERTICES {
        return Err(format!("{n} positions is more than {what} takes inline ({INLINE_FORM_VERTICES}, it would block the page); use the job: page.job(name, \"{what}\", {{ form_cell }}, opts)"));
    }
    Ok(())
}

/// `scene.mesh(form, opts)`.
pub fn mesh_from_form(form: &Data, opts: &Data) -> Result<Model, String> {
    mesh_from_form_with(form, opts, &Ctl::none())
}

/// [`mesh_from_form`] with cancellation (the `scene.mesh` job).
pub fn mesh_from_form_with(form: &Data, opts: &Data, ctl: &Ctl) -> Result<Model, String> {
    let positions = vec3s(form.get("positions"), "positions", model::MAX_VERTICES)?;
    if positions.is_empty() {
        return Err("positions are empty".into());
    }
    let n = positions.len();
    let indices: Vec<u32> = match form.get("indices") {
        Data::Nil => (0..n as u32).collect(),
        Data::List(l) => l.iter().map(|x| x.num().filter(|v| *v >= 0.0 && v.fract() == 0.0 && *v < u32::MAX as f64).map(|v| v as u32).ok_or("indices must be whole numbers")).collect::<Result<_, _>>()?,
        other => {
            let b = other.as_bytes().ok_or("indices must be bytes (u32 little-endian) or a list")?;
            if b.len() % 4 != 0 {
                return Err(format!("indices: {} bytes is not a whole number of u32 values", b.len()));
            }
            b.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
        }
    };
    let normals = match form.get("normals") {
        Data::Nil => None,
        d => Some(vec3s(d, "normals", model::MAX_VERTICES)?),
    };
    let colors = colors_of(form.get("colors"), n)?;
    let mode = NormalMode::parse(opts.get("normals").as_str(), NormalMode::Smooth)?;
    let mb = model::MeshBuilder { positions, normals, colors, indices, ..Default::default() };
    ctl.check()?;
    mb.build("mesh", mode, opts.get("flat").truthy(), ctl)
}

/// `scene.points(form, opts)`.
pub fn points_from_form(form: &Data, opts: &Data) -> Result<Model, String> {
    points_from_form_with(form, opts, &Ctl::none())
}

/// [`points_from_form`] with cancellation (the `scene.points` job).
pub fn points_from_form_with(form: &Data, opts: &Data, ctl: &Ctl) -> Result<Model, String> {
    let positions = vec3s(form.get("positions"), "positions", model::MAX_POINTS)?;
    if positions.is_empty() {
        return Err("positions are empty".into());
    }
    let colors = colors_of(form.get("colors"), positions.len())?;
    let o = load_options(opts)?;
    // origin: the first finite point
    let mut pb = model::PointsBuilder::auto(positions.len())?;
    pb.has_colors = colors.is_some();
    for (k, p) in positions.iter().enumerate() {
        if k & 0xF_FFFF == 0 {
            ctl.check()?;
        }
        let c = colors.as_ref().map(|c| c[k]).unwrap_or(model::WHITE);
        pb.push([p[0] as f64, p[1] as f64, p[2] as f64], c)?;
    }
    pb.build("points", o.shuffle, o.max_points, ctl)
}

fn bytes_f32(v: impl Iterator<Item = f32>, n: usize) -> Data {
    let mut b = Vec::with_capacity(n * 4);
    for x in v {
        b.extend_from_slice(&x.to_le_bytes());
    }
    Data::bytes(b)
}

/// Largest mesh / points form handed to a script (use `m:save()` for more).
pub const MAX_FORM_BYTES: usize = 256 << 20;

/// The shared mesh form of a model's mesh parts (model coordinates).
pub fn to_mesh_form(m: &Model) -> Result<Data, String> {
    let nv = m.vertex_count();
    let size = nv * 28 + m.triangle_count() * 12;
    if size > MAX_FORM_BYTES {
        return Err(format!("the mesh form would be {} MB; the limit is {} MB (use m:save(format))", size >> 20, MAX_FORM_BYTES >> 20));
    }
    let o = m.origin;
    let pos = bytes_f32(m.mesh.iter().flat_map(|p| p.vertices.iter()).flat_map(|v| [(o[0] + v.pos[0] as f64) as f32, (o[1] + v.pos[1] as f64) as f32, (o[2] + v.pos[2] as f64) as f32]), nv * 3);
    let nor = bytes_f32(m.mesh.iter().flat_map(|p| p.vertices.iter()).flat_map(|v| v.normal), nv * 3);
    let mut col = Vec::with_capacity(nv * 4);
    let mut idx = Vec::with_capacity(m.triangle_count() * 12);
    let mut base = 0u32;
    for p in &m.mesh {
        for v in &p.vertices {
            col.extend_from_slice(&v.color);
        }
        for i in &p.indices {
            idx.extend_from_slice(&(i + base).to_le_bytes());
        }
        base += p.vertices.len() as u32;
    }
    let mut pairs = vec![("positions", pos), ("normals", nor), ("indices", Data::bytes(idx))];
    if m.has_colors {
        pairs.push(("colors", Data::bytes(col)));
    }
    Ok(Data::map(pairs))
}

/// The shared points form of (a prefix of) a model's points.
pub fn to_points_form(m: &Model, max: usize) -> Result<Data, String> {
    let n = m.point_count().min(max);
    if n * 16 > MAX_FORM_BYTES {
        return Err(format!("{n} points would be {} MB; the limit is {} MB (pass {{max = n}} or use m:save(format))", (n * 16) >> 20, MAX_FORM_BYTES >> 20));
    }
    let o = m.origin;
    let pos = bytes_f32(m.points[..n].iter().flat_map(|p| [(o[0] + p.pos[0] as f64) as f32, (o[1] + p.pos[1] as f64) as f32, (o[2] + p.pos[2] as f64) as f32]), n * 3);
    let mut pairs = vec![("positions", pos)];
    if m.has_colors {
        pairs.push(("colors", Data::bytes(m.points[..n].iter().flat_map(|p| p.color).collect())));
    }
    Ok(Data::map(pairs))
}

pub fn bounds_data(b: &Bounds) -> Data {
    if b.is_empty() {
        return Data::Nil;
    }
    Data::map([
        ("min", camera::vec3_data(b.min)),
        ("max", camera::vec3_data(b.max)),
        ("center", camera::vec3_data(b.center())),
        ("size", camera::vec3_data(b.size())),
        ("radius", Data::Num(b.radius())),
    ])
}

pub fn info(m: &Model) -> Data {
    Data::map([
        ("kind", Data::str(m.kind())),
        ("format", Data::str(&m.format)),
        ("vertices", Data::Num(m.vertex_count() as f64)),
        ("triangles", Data::Num(m.triangle_count() as f64)),
        ("points", Data::Num(m.point_count() as f64)),
        ("source_points", Data::Num(m.source_points as f64)),
        ("parts", Data::Num(m.mesh.len() as f64)),
        ("colors", Data::Bool(m.has_colors)),
        ("normals", Data::Bool(m.source_normals)),
        ("shuffled", Data::Bool(m.shuffled)),
        ("bytes", Data::Num(m.bytes() as f64)),
        ("bounds", bounds_data(&m.bounds())),
    ])
}

/// Bounds of bounds tables, models, lists of them, or a scene table.
pub fn bounds_of(d: &Data) -> Result<Bounds, String> {
    match d {
        Data::Res(_) => Ok(model_arg(d)?.bounds()),
        Data::Map(_) if !d.get("min").is_nil() => {
            let (Some(a), Some(b)) = (camera::vec3_of(d.get("min")), camera::vec3_of(d.get("max"))) else {
                return Err("bounds need min = {x, y, z} and max = {x, y, z}".into());
            };
            let mut out = Bounds::EMPTY;
            out.add(a);
            out.add(b);
            Ok(out)
        }
        Data::Map(_) => Ok(Frame::from_inputs(std::slice::from_ref(d))?.bounds),
        Data::List(l) => {
            let mut b = Bounds::EMPTY;
            for x in l.iter() {
                b = b.union(&match x {
                    Data::Map(_) if !x.get("model").is_nil() => Frame::from_inputs(&[Data::map([("items", Data::list(vec![x.clone()]))])])?.bounds,
                    _ => bounds_of(x)?,
                });
            }
            Ok(b)
        }
        other => Err(format!("expected bounds, a model, a list or a scene, got {}", type_name(other))),
    }
}

fn camera_arg(d: &Data) -> Result<Camera, String> {
    Camera::from_data(d).map(|c| c.0)
}

fn num_arg(d: &Data, what: &str) -> Result<f64, String> {
    d.num().filter(|x| x.is_finite()).ok_or_else(|| format!("{what} must be a finite number, got {}", type_name(d)))
}

/// Largest file `scene.load` reads inline (the job takes any size).
pub const INLINE_LOAD_BYTES: usize = 16 << 20;
/// Most points `scene.synthetic` generates inline (the job takes more).
pub const INLINE_SYNTHETIC_POINTS: usize = 1_000_000;
/// Largest file `m:save()` writes inline (the job writes up to 2 GB).
pub const INLINE_SAVE_BYTES: usize = 16 << 20;
/// Most positions `scene.mesh` / `scene.points` take inline (their jobs take more).
pub const INLINE_FORM_VERTICES: usize = 4_000_000;

/// `{ shape, count, seed }` -> a synthetic cloud of at most `max` points.
fn synthetic_from(o: &Data, max: usize, ctl: &Ctl) -> Result<Model, String> {
    let count = o.get("count").num_or(100_000.0);
    if !(count >= 1.0 && count <= model::MAX_POINTS as f64) {
        return Err(format!("count must be between 1 and {}", model::MAX_POINTS));
    }
    if count > max as f64 {
        return Err(format!("{count} points is more than scene.synthetic makes inline ({max}); use the job: page.job(name, \"scene.synthetic\", {{}}, {{ count = ... }})"));
    }
    let seed = o.get("seed").num_or(1.0) as u64;
    synthetic::synthetic_points(o.get("shape").str_or("terrain"), count as usize, seed, ctl)
}

fn primitive(shape: &str, o: &Data) -> Result<Model, String> {
    let pos = |key: &str, d: f64| -> Result<f32, String> {
        let v = o.get(key).num_or(d);
        if v.is_finite() && v > 0.0 && v < 1e12 {
            Ok(v as f32)
        } else {
            Err(format!("{key} must be a positive number"))
        }
    };
    let segments = o.get("segments").num_or(32.0).clamp(3.0, synthetic::MAX_SEGMENTS as f64) as usize;
    let b = match shape {
        "box" | "cube" => {
            let s = match o.get("size") {
                Data::Nil => [1.0; 3],
                Data::Num(n) => [*n; 3],
                v => camera::vec3_of(v).ok_or("size must be a number or {x, y, z}")?,
            };
            if s.iter().any(|x| !(x.is_finite() && *x > 0.0)) {
                return Err("size must be positive".into());
            }
            synthetic::cuboid(s[0] as f32, s[1] as f32, s[2] as f32)
        }
        "sphere" => synthetic::sphere(pos("radius", 0.5)?, segments),
        "cylinder" => synthetic::cylinder(pos("radius", 0.5)?, pos("height", 1.0)?, segments),
        "plane" => synthetic::plane(pos("size", 1.0)?),
        other => return Err(format!("unknown shape \"{other}\" (box, sphere, cylinder, plane)")),
    };
    synthetic::build_primitive(b, shape)
}

/// Add the `scene` module drawing with `renderer` (shared so a host or a
/// test can read its [`Stats`]).
pub fn register_with(n: &mut Natives, renderer: Arc<SceneRenderer>) {
    n.function("scene.load", Effect::Pure, |c| {
        let (bytes, name) = source_arg(c.arg(0))?;
        if bytes.len() > INLINE_LOAD_BYTES {
            return Err(format!(
                "the file is {} MB; scene.load reads at most {} MB inline (it would block the page): use the job, page.job(name, \"scene.load\", {{ file_cell }})",
                bytes.len() >> 20,
                INLINE_LOAD_BYTES >> 20
            ));
        }
        let (format, opts) = match c.arg(1) {
            Data::Map(_) => (c.arg(1).get("format").str_or("auto").to_string(), c.arg(1)),
            d => (d.str_or("auto").to_string(), c.arg(2)),
        };
        let o = load_options(opts)?;
        load::load(&bytes, &format, name.as_deref(), &o, &Ctl::none()).map(model_data)
    });
    n.job_fn("scene.load", |j| {
        let src = j.input(0);
        if src.is_nil() {
            return Ok(Data::Nil);
        }
        let (bytes, name) = source_arg(src)?;
        let format = match j.input(1) {
            Data::Str(s) => s.to_string(),
            m @ Data::Map(_) => m.get("format").str_or(j.opts.get("format").str_or("auto")).to_string(),
            _ => j.opts.get("format").str_or("auto").to_string(),
        };
        let opts = if matches!(j.input(1), Data::Map(_)) { j.input(1) } else { j.opts };
        let o = load_options(opts)?;
        let cancelled = || j.cancelled();
        let progress = |p: f32| j.progress(p);
        let ctl = Ctl { cancelled: &cancelled, progress: &progress };
        load::load(&bytes, &format, name.as_deref(), &o, &ctl).map(model_data)
    });
    n.function("scene.mesh", Effect::Pure, |c| {
        inline_form(c.arg(0), "scene.mesh")?;
        mesh_from_form(c.arg(0), c.arg(1)).map(model_data)
    });
    n.function("scene.points", Effect::Pure, |c| {
        inline_form(c.arg(0), "scene.points")?;
        points_from_form(c.arg(0), c.arg(1)).map(model_data)
    });
    n.job_fn("scene.mesh", |j| {
        if j.input(0).is_nil() {
            return Ok(Data::Nil);
        }
        let cancelled = || j.cancelled();
        let progress = |p: f32| j.progress(p);
        mesh_from_form_with(j.input(0), j.opts, &Ctl { cancelled: &cancelled, progress: &progress }).map(model_data)
    });
    n.job_fn("scene.points", |j| {
        if j.input(0).is_nil() {
            return Ok(Data::Nil);
        }
        let cancelled = || j.cancelled();
        let progress = |p: f32| j.progress(p);
        points_from_form_with(j.input(0), j.opts, &Ctl { cancelled: &cancelled, progress: &progress }).map(model_data)
    });
    n.function("scene.primitive", Effect::Pure, |c| {
        let shape = c.arg(0).as_str().ok_or("shape must be a string (box, sphere, cylinder, plane)")?;
        primitive(shape, c.arg(1)).map(model_data)
    });
    n.function("scene.synthetic", Effect::Pure, |c| synthetic_from(c.arg(0), INLINE_SYNTHETIC_POINTS, &Ctl::none()).map(model_data));
    n.job_fn("scene.synthetic", |j| {
        let params = if matches!(j.input(0), Data::Map(_)) { j.input(0) } else { j.opts };
        let cancelled = || j.cancelled();
        let progress = |p: f32| j.progress(p);
        synthetic_from(params, model::MAX_POINTS, &Ctl { cancelled: &cancelled, progress: &progress }).map(model_data)
    });
    n.function("scene.info", Effect::Pure, |c| {
        let m = model_arg(c.arg(0))?;
        Ok(info(&m))
    });
    n.function("scene.to_mesh", Effect::Pure, |c| {
        let m = model_arg(c.arg(0))?;
        to_mesh_form(&m)
    });
    n.function("scene.to_points", Effect::Pure, |c| {
        let m = model_arg(c.arg(0))?;
        let max = c.arg(1).get("max").num().map(|x| x.max(0.0) as usize).unwrap_or(usize::MAX);
        to_points_form(&m, max)
    });
    n.function("scene.save", Effect::Pure, |c| {
        let m = model_arg(c.arg(0))?;
        let format = c.arg(1).as_str().ok_or("format must be a string (ply, stl, obj, xyz)")?;
        if let Some(size) = export::estimate(&m, format).filter(|s| *s > INLINE_SAVE_BYTES) {
            return Err(format!(
                "the file would be about {} MB; m:save writes at most {} MB inline (it would block the page): use the job, page.job(name, \"scene.save\", {{ model_cell }}, {{ format = \"{format}\" }})",
                size >> 20,
                INLINE_SAVE_BYTES >> 20
            ));
        }
        export::save(&m, format).map(Data::bytes)
    });
    n.job_fn("scene.save", |j| {
        let src = j.input(0);
        if src.is_nil() {
            return Ok(Data::Nil);
        }
        let m = model_arg(src)?;
        let format = match j.input(1) {
            Data::Str(s) => s.to_string(),
            _ => j.opts.get("format").str_or("ply").to_string(),
        };
        let cancelled = || j.cancelled();
        let progress = |p: f32| j.progress(p);
        export::save_with(&m, &format, &Ctl { cancelled: &cancelled, progress: &progress }).map(Data::bytes)
    });
    n.function("scene.camera", Effect::Pure, |c| Ok(camera_arg(c.arg(0))?.to_data(c.arg(0))));
    n.function("scene.orbit", Effect::Pure, |c| {
        let mut cam = camera_arg(c.arg(0))?;
        let speed = if c.arg(3).is_nil() { 0.4 } else { num_arg(c.arg(3), "speed")? };
        cam.orbit(num_arg(c.arg(1), "dx")?, num_arg(c.arg(2), "dy")?, speed);
        Ok(cam.to_data(c.arg(0)))
    });
    n.function("scene.zoom", Effect::Pure, |c| {
        let mut cam = camera_arg(c.arg(0))?;
        cam.zoom(num_arg(c.arg(1), "factor")?);
        Ok(cam.to_data(c.arg(0)))
    });
    n.function("scene.pan", Effect::Pure, |c| {
        let mut cam = camera_arg(c.arg(0))?;
        cam.pan(num_arg(c.arg(1), "dx")?, num_arg(c.arg(2), "dy")?, num_arg(c.arg(3), "view_height")?);
        Ok(cam.to_data(c.arg(0)))
    });
    n.function("scene.fit", Effect::Pure, |c| {
        let mut cam = camera_arg(c.arg(0))?;
        let b = bounds_of(c.arg(1))?;
        let o = c.arg(2);
        cam.fit(&b, o.get("aspect").num_or(1.0), o.get("margin").num_or(1.1));
        Ok(cam.to_data(c.arg(0)))
    });
    n.function("scene.bounds", Effect::Pure, |c| Ok(bounds_data(&bounds_of(c.arg(0))?)));
    n.function("scene.project", Effect::Pure, |c| {
        let cam = camera_arg(c.arg(0))?;
        let p = camera::vec3_of(c.arg(1)).ok_or("point must be {x, y, z}")?;
        let (w, h) = (num_arg(c.arg(2), "w")?, num_arg(c.arg(3), "h")?);
        let (x, y, depth, visible) = cam.project(p, w, h);
        Ok(Data::map([("x", Data::Num(x)), ("y", Data::Num(y)), ("depth", Data::Num(depth)), ("visible", Data::Bool(visible && x.is_finite()))]))
    });
    n.function("scene.transform", Effect::Pure, |c| {
        let m = frame::transform_of(c.arg(0))?;
        Ok(Data::nums(m.to_cols_array()))
    });
    // render stats change outside the graph: a snapshot for handlers, a stream for cells and panels
    let r = renderer.clone();
    n.function("scene.stats", Effect::Effect, move |c| {
        let id = match c.arg(0) {
            Data::Nil => "",
            d => d.as_str().ok_or("scene.stats(id): id must be a string")?,
        };
        Ok(r.stats_for(id).unwrap_or_default().to_data())
    });
    let r = renderer.clone();
    n.source("scene.stats", move |ctx| {
        let id = match ctx.opts.get("id") {
            Data::Nil => String::new(),
            d => d.as_str().ok_or("scene.stats stream: id must be a string")?.to_string(),
        };
        Ok(Box::new(r.subscribe(&id, ctx.sink.clone())))
    });
    n.function("scene.formats", Effect::Pure, |_| Ok(Data::list(FORMATS.iter().map(|f| Data::str(f)).collect())));
    n.layer("scene.view", renderer, |inputs| Frame::from_inputs(inputs).map(val));
}
