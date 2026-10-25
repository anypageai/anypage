//! The `scene.view` GPU layer renderer.
//!
//! The engine calls [`SceneRenderer::render`] only when the layer's value
//! (a [`Frame`]) or its pixel size changed. Everything heavy is cached
//! between calls:
//!
//! * geometry per (resource id, version): mesh parts as one vertex + index
//!   buffer each, point clouds as chunks of [`CHUNK_POINTS`] uploaded on
//!   first use, so a budget-limited prefix uploads only what it draws;
//!   entries die with their model (weak references) or by LRU over
//!   [`GPU_CACHE_BYTES`];
//! * pipelines per (target format, sample count, kind), compiled on first use;
//! * depth (and multisample colour) textures per target size.
//!
//! Matrices are composed in `f64` (model origin folded in, relative to the
//! eye) and narrowed to `f32` only as model-view-projection, so far-away
//! survey coordinates stay steady. Output is premultiplied alpha in
//! `target.format`; no storage buffers are used (the engine's device has
//! WebGL2-level limits).
//!
//! Every GPU object is created inside out-of-memory, validation and
//! internal error scopes ([`guarded`]), so running out of GPU memory makes
//! `render` return an error instead of reaching wgpu's default handler
//! (which panics). Meshes in one frame may use at most
//! [`MAX_MESH_GPU_BYTES`]; point clouds are bounded by the point budget.
//!
//! [`Stats`] are kept per scene `id` and pushed to `scene.stats` streams
//! after every render.

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Instant;

use anypage_core::Value;
use anypage_engine::wgpu;
use anypage_engine::{GpuRenderer, GpuTarget};
use anypage_runtime::{Data, Sink};
use bytemuck::{Pod, Zeroable};
use glam::{DMat4, DVec3, DVec4};

use crate::camera::Up;
use crate::frame::{ColorMode, Frame, Grid};
use crate::model::{srgb8_to_linear, Bounds, MeshVertex, Model, PointVertex};

/// Points per GPU buffer (64 MiB).
pub const CHUNK_POINTS: usize = 4 << 20;
const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const MAX_TARGETS: usize = 4;
/// Scene ids whose last stats are kept.
const MAX_STATS_IDS: usize = 64;

#[cfg(any(target_os = "android", target_os = "ios"))]
pub const GPU_CACHE_BYTES: u64 = 384 << 20;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub const GPU_CACHE_BYTES: u64 = 2 << 30;

/// Most GPU memory the meshes of one frame may take (vertex + index
/// buffers): 384 MB on phones (about 7 M triangles), 2 GB on desktop.
pub const MAX_MESH_GPU_BYTES: u64 = GPU_CACHE_BYTES;

const PREMULTIPLIED: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
    alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
};

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct ItemUniform {
    mvp: [[f32; 4]; 4],
    nmat: [[f32; 4]; 4],
    color: [f32; 4],
    height: [f32; 4],
    params: [f32; 4],
    viewport: [f32; 4],
}

/// What the last render did, for `scene.stats()` and tests.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub renders: u64,
    /// CPU time of the last render call (encoding, uploads, submit).
    pub ms: f64,
    pub upload_ms: f64,
    pub uploaded_bytes: u64,
    pub gpu_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub samples: u32,
    pub items: usize,
    pub pending: usize,
    pub triangles: usize,
    pub points_drawn: usize,
    pub points_total: usize,
    pub draw_calls: usize,
}

impl Stats {
    /// The table `scene.stats` returns and streams.
    pub fn to_data(&self) -> Data {
        Data::map([
            ("renders", Data::Num(self.renders as f64)),
            ("ms", Data::Num(self.ms)),
            ("upload_ms", Data::Num(self.upload_ms)),
            ("uploaded_bytes", Data::Num(self.uploaded_bytes as f64)),
            ("gpu_bytes", Data::Num(self.gpu_bytes as f64)),
            ("width", Data::Num(self.width as f64)),
            ("height", Data::Num(self.height as f64)),
            ("samples", Data::Num(self.samples as f64)),
            ("items", Data::Num(self.items as f64)),
            ("pending", Data::Num(self.pending as f64)),
            ("triangles", Data::Num(self.triangles as f64)),
            ("points_drawn", Data::Num(self.points_drawn as f64)),
            ("points_total", Data::Num(self.points_total as f64)),
            ("draw_calls", Data::Num(self.draw_calls as f64)),
        ])
    }
}

/// Last stats overall and per scene id.
#[derive(Default)]
struct StatsStore {
    last: Stats,
    by_id: HashMap<String, Stats>,
}

struct Subscriber {
    n: u64,
    id: String,
    sink: Sink,
}

type Subscribers = Arc<Mutex<Vec<Subscriber>>>;

/// Keeps a `scene.stats` stream subscribed; dropping it unsubscribes.
pub struct Subscription {
    subs: Weak<Mutex<Vec<Subscriber>>>,
    n: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(s) = self.subs.upgrade() {
            lock(&s).retain(|x| x.n != self.n);
        }
    }
}

/// Pipelines are compiled on first use (shader compilation dominates the
/// first frame on DX12), per (format, samples, kind).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Pipe {
    Mesh,
    MeshBlend,
    Points,
    Quads,
    Lines,
}

struct Targets {
    key: (u32, u32, u32, wgpu::TextureFormat),
    depth: wgpu::TextureView,
    color: Option<wgpu::TextureView>,
}

struct GpuModel {
    model: Weak<Model>,
    parts: Vec<(wgpu::Buffer, wgpu::Buffer, u32)>,
    chunks: Vec<Option<wgpu::Buffer>>,
    bytes: u64,
    last_used: u64,
}

struct Uniforms {
    buffer: wgpu::Buffer,
    group: wgpu::BindGroup,
    slots: u64,
}

#[derive(Default)]
struct State {
    device: Option<wgpu::Device>,
    shader: Option<wgpu::ShaderModule>,
    layout: Option<(wgpu::BindGroupLayout, wgpu::PipelineLayout)>,
    pipelines: HashMap<(wgpu::TextureFormat, u32, Pipe), wgpu::RenderPipeline>,
    /// Depth (and MSAA colour) textures for the last few target sizes: one
    /// renderer serves every scene.view layer of every page.
    targets: Vec<Targets>,
    uniforms: Option<Uniforms>,
    models: HashMap<(u64, u64), GpuModel>,
    /// Can `format` be rendered with 4x MSAA on this device?
    msaa_ok: HashMap<wgpu::TextureFormat, bool>,
    renders: u64,
}

/// The renderer registered for `scene.view`. One instance serves every page.
#[derive(Default)]
pub struct SceneRenderer {
    state: Mutex<State>,
    stats: Mutex<StatsStore>,
    subs: Subscribers,
    next_sub: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn mat(m: DMat4) -> [[f32; 4]; 4] {
    m.as_mat4().to_cols_array_2d()
}

fn linear(c: [u8; 4]) -> [f32; 4] {
    [srgb8_to_linear(c[0]), srgb8_to_linear(c[1]), srgb8_to_linear(c[2]), c[3] as f32 / 255.0]
}

/// 1, 2 or 5 times a power of ten, at least `x`.
fn nice_step(x: f64) -> f64 {
    if !(x.is_finite() && x > 0.0) {
        return 1.0;
    }
    let p = 10f64.powf(x.log10().floor());
    for m in [1.0, 2.0, 5.0, 10.0] {
        if p * m >= x {
            return p * m;
        }
    }
    p * 10.0
}

/// Grid (and axes) lines around the scene, relative to the returned centre.
pub(crate) fn grid_lines(g: &Grid, bounds: &Bounds, up: Up) -> ([f64; 3], Vec<PointVertex>, Bounds) {
    let (a, b, u) = match up {
        Up::Y => (0, 2, 1),
        Up::Z => (0, 1, 2),
    };
    let size = bounds.size();
    let half = g.size.unwrap_or_else(|| if bounds.is_empty() { 5.0 } else { (size[a].max(size[b]) * 0.75).max(size[u] * 0.25).max(1e-6) });
    let mut step = g.step.unwrap_or_else(|| nice_step(half / 10.0));
    while half / step > 100.0 {
        step *= 2.0;
    }
    let n = (half / step).ceil().max(1.0);
    let half = n * step;
    let c = bounds.center();
    let mut center = [0.0; 3];
    if !bounds.is_empty() {
        center[a] = (c[a] / step).round() * step;
        center[b] = (c[b] / step).round() * step;
        center[u] = bounds.min[u];
    }
    let mut v = Vec::with_capacity((2 * n as usize + 1) * 4 + 6);
    let p = |x: f64, y: f64| {
        let mut q = [0f32; 3];
        q[a] = x as f32;
        q[b] = y as f32;
        q
    };
    for i in -(n as i64)..=(n as i64) {
        let t = i as f64 * step;
        v.push(PointVertex { pos: p(t, -half), color: g.color });
        v.push(PointVertex { pos: p(t, half), color: g.color });
        v.push(PointVertex { pos: p(-half, t), color: g.color });
        v.push(PointVertex { pos: p(half, t), color: g.color });
    }
    let mut ext = Bounds::EMPTY;
    let mut lo = center;
    let mut hi = center;
    lo[a] -= half;
    lo[b] -= half;
    hi[a] += half;
    hi[b] += half;
    ext.add(lo);
    ext.add(hi);
    // axes from the world origin when it is on the grid
    let o = [-center[0], -center[1], -center[2]];
    if g.axes && o[a].abs() <= half && o[b].abs() <= half {
        let len = half * 0.5;
        let colors = [[230, 76, 76, 255], [88, 196, 96, 255], [80, 132, 240, 255]];
        for (k, col) in colors.iter().enumerate() {
            let from = [o[0] as f32, o[1] as f32, o[2] as f32];
            let mut to = from;
            to[k] += len as f32;
            v.push(PointVertex { pos: from, color: *col });
            v.push(PointVertex { pos: to, color: *col });
            let mut w = [0.0; 3];
            w[k] = len;
            ext.add([w[0], w[1], w[2]]);
        }
    }
    (center, v, ext)
}

impl SceneRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// What the last render (of any scene) did.
    pub fn stats(&self) -> Stats {
        lock(&self.stats).last.clone()
    }

    /// What the last render of the scene with this `id` did (`""` for
    /// scenes without one).
    pub fn stats_for(&self, id: &str) -> Option<Stats> {
        lock(&self.stats).by_id.get(id).cloned()
    }

    /// Push the stats of every render of scenes with this `id` to `sink`
    /// (and the last ones now, if any), until the returned guard is dropped.
    pub fn subscribe(&self, id: &str, sink: Sink) -> Subscription {
        if let Some(s) = self.stats_for(id) {
            sink.push(s.to_data());
        }
        let n = self.next_sub.fetch_add(1, Ordering::Relaxed);
        lock(&self.subs).push(Subscriber { n, id: id.to_string(), sink });
        Subscription { subs: Arc::downgrade(&self.subs), n }
    }

    /// Drop every cached GPU object (tests; a host after device loss).
    pub fn clear(&self) {
        *lock(&self.state) = State::default();
    }

    fn render_frame(&self, t: &GpuTarget, f: &Frame) -> Result<(), String> {
        let stats = self.draw(t, f)?;
        let id = f.id.clone().unwrap_or_default();
        {
            let mut store = lock(&self.stats);
            if !store.by_id.contains_key(&id) && store.by_id.len() >= MAX_STATS_IDS {
                // forget the scene rendered longest ago
                if let Some(old) = store.by_id.iter().min_by_key(|(_, s)| s.renders).map(|(k, _)| k.clone()) {
                    store.by_id.remove(&old);
                }
            }
            store.by_id.insert(id.clone(), stats.clone());
            store.last = stats.clone();
        }
        // push outside the lock (a push wakes the page)
        let sinks: Vec<Sink> = lock(&self.subs).iter().filter(|s| s.id == id).map(|s| s.sink.clone()).collect();
        if !sinks.is_empty() {
            let d = stats.to_data();
            for s in sinks {
                s.push(d.clone());
            }
        }
        Ok(())
    }

    fn draw(&self, t: &GpuTarget, f: &Frame) -> Result<Stats, String> {
        let t0 = Instant::now();
        let (w, h) = (t.size.0.max(1), t.size.1.max(1));
        let mut guard = lock(&self.state);
        let st = &mut *guard;
        if st.device.as_ref() != Some(t.device) {
            *st = State { device: Some(t.device.clone()), ..State::default() };
        }
        st.renders += 1;
        let frame_no = st.renders;
        let device = t.device;

        // meshes are uploaded whole: refuse what this device should not hold
        check_mesh_bytes(f, MAX_MESH_GPU_BYTES)?;

        if st.shader.is_none() {
            st.shader = Some(guarded(device, "the scene shader", || {
                device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("scene.view"),
                    source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
                })
            })?);
        }
        if st.layout.is_none() {
            st.layout = Some(guarded(device, "the pipeline layout", || {
                let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("scene.item"),
                    entries: &[wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: true,
                            min_binding_size: NonZeroU64::new(std::mem::size_of::<ItemUniform>() as u64),
                        },
                        count: None,
                    }],
                });
                let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("scene.view"), bind_group_layouts: &[&bgl], push_constant_ranges: &[] });
                (bgl, pl)
            })?);
        }
        let msaa_ok = *st.msaa_ok.entry(t.format).or_insert_with(|| probe_msaa(device, t.format));
        let samples = if f.msaa >= 4 && msaa_ok { 4 } else { 1 };
        let (Some(shader), Some((bgl, pl))) = (&st.shader, &st.layout) else { return Err("scene.view: pipeline setup failed".into()) };
        let counts = f.point_counts();
        let mut needed = Vec::new();
        if f.grid.is_some() {
            needed.push(Pipe::Lines);
        }
        for (it, &np) in f.items.iter().zip(&counts) {
            if it.model.triangle_count() > 0 {
                needed.push(if it.color[3] < 255 { Pipe::MeshBlend } else { Pipe::Mesh });
            }
            if np > 0 {
                needed.push(if it.point_size > 1.0 { Pipe::Quads } else { Pipe::Points });
            }
        }
        for kind in needed {
            if let std::collections::hash_map::Entry::Vacant(e) = st.pipelines.entry((t.format, samples, kind)) {
                e.insert(guarded(device, "a render pipeline", || build_pipeline(device, pl, shader, t.format, samples, kind))?);
            }
        }

        // render targets
        let tkey = (w, h, samples, t.format);
        if !st.targets.iter().any(|x| x.key == tkey) {
            let tex = |format, label| {
                device
                    .create_texture(&wgpu::TextureDescriptor {
                        label: Some(label),
                        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                        mip_level_count: 1,
                        sample_count: samples,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                        view_formats: &[],
                    })
                    .create_view(&Default::default())
            };
            let made = guarded(device, "the depth and multisample textures", || Targets {
                key: tkey,
                depth: tex(DEPTH, "scene.depth"),
                color: (samples > 1).then(|| tex(t.format, "scene.msaa")),
            })?;
            if st.targets.len() >= MAX_TARGETS {
                st.targets.remove(0);
            }
            st.targets.push(made);
        }

        // camera
        let aspect = w as f64 / h as f64;
        let mut cam = f.camera.clone();
        if f.auto_fit {
            cam.fit(&f.bounds, aspect, 1.1);
        }
        let grid = f.grid.as_ref().map(|g| grid_lines(g, &f.bounds, cam.up));
        let all = match &grid {
            Some((c, _, ext)) => f.bounds.union(&ext.offset([0.0; 3])).union(&Bounds::new(*c, *c)),
            None => f.bounds,
        };
        let (near, far) = cam.clip_planes(&all);
        let view = cam.view();
        let proj = cam.proj(aspect, near, far);
        let up_axis = if cam.up == Up::Z { 2 } else { 1 };

        // uniforms: slot 0 for the grid, then one per item
        let align = device.limits().min_uniform_buffer_offset_alignment.max(1) as u64;
        let stride = (std::mem::size_of::<ItemUniform>() as u64).div_ceil(align) * align;
        let slots = f.items.len() as u64 + 1;
        if st.uniforms.as_ref().is_none_or(|u| u.slots < slots) {
            let cap = slots.next_power_of_two().max(16);
            st.uniforms = Some(guarded(device, "the uniform buffer", || {
                let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("scene.items"),
                    size: cap * stride,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("scene.items"),
                    layout: bgl,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: &buffer, offset: 0, size: NonZeroU64::new(std::mem::size_of::<ItemUniform>() as u64) }),
                    }],
                });
                Uniforms { buffer, group, slots: cap }
            })?);
        }
        let mut ubytes = vec![0u8; (slots * stride) as usize];
        let viewport = |round: bool| [w as f32, h as f32, if round { 1.0 } else { 0.0 }, 0.0];
        let put = |ubytes: &mut Vec<u8>, slot: u64, u: &ItemUniform| {
            let at = (slot * stride) as usize;
            ubytes[at..at + std::mem::size_of::<ItemUniform>()].copy_from_slice(bytemuck::bytes_of(u));
        };
        if let Some((center, _, _)) = &grid {
            let m = DMat4::from_translation(DVec3::from(*center));
            let u = ItemUniform {
                mvp: mat(proj * view * m),
                nmat: mat(DMat4::IDENTITY),
                color: [1.0; 4],
                height: [0.0; 4],
                params: [1.0, 0.0, 1.0, 0.0],
                viewport: viewport(false),
            };
            put(&mut ubytes, 0, &u);
        }
        for (k, it) in f.items.iter().enumerate() {
            let model = it.transform * DMat4::from_translation(DVec3::from(it.model.origin));
            let mv = view * model;
            let nm = mv.inverse().transpose();
            let nm = if nm.is_finite() { nm } else { DMat4::IDENTITY };
            // height ramp: world "up" coordinate of a stored position, scaled to 0..1 over the item
            let row = DVec4::new(model.x_axis[up_axis], model.y_axis[up_axis], model.z_axis[up_axis], model.w_axis[up_axis]);
            let (lo, hi) = (it.world.min[up_axis], it.world.max[up_axis]);
            let height = if hi - lo > 1e-12 && row.is_finite() {
                let s = 1.0 / (hi - lo);
                [(row.x * s) as f32, (row.y * s) as f32, (row.z * s) as f32, ((row.w - lo) * s) as f32]
            } else {
                [0.0, 0.0, 0.0, 0.5]
            };
            let mode = match it.mode {
                ColorMode::Vertex => 0.0,
                ColorMode::Solid => 1.0,
                ColorMode::Height => 2.0,
            };
            let u = ItemUniform {
                mvp: mat(proj * mv),
                nmat: mat(nm),
                color: linear(it.color),
                height,
                params: [it.point_size, mode, f.ambient, if it.lit { 1.0 } else { 0.0 }],
                viewport: viewport(it.round),
            };
            put(&mut ubytes, k as u64 + 1, &u);
        }
        let Some(uniforms) = &st.uniforms else { return Err("scene.view: uniform buffer missing".into()) };
        t.queue.write_buffer(&uniforms.buffer, 0, &ubytes);

        // geometry on the GPU
        let tu = Instant::now();
        let max_buffer = device.limits().max_buffer_size;
        let mut uploaded = 0u64;
        for (it, &np) in f.items.iter().zip(&counts) {
            let g = st.models.entry(it.key).or_insert_with(|| GpuModel {
                model: Arc::downgrade(&it.model),
                parts: Vec::new(),
                chunks: Vec::new(),
                bytes: 0,
                last_used: frame_no,
            });
            g.last_used = frame_no;
            if g.parts.is_empty() && !it.model.mesh.is_empty() {
                let mut parts = Vec::with_capacity(it.model.mesh.len());
                for p in &it.model.mesh {
                    let vb_size = std::mem::size_of_val(p.vertices.as_slice()) as u64;
                    let ib_size = std::mem::size_of_val(p.indices.as_slice()) as u64;
                    if vb_size > max_buffer || ib_size > max_buffer {
                        return Err(format!("scene.view: a mesh part needs {} MB, more than this GPU's {} MB buffer limit", vb_size.max(ib_size) >> 20, max_buffer >> 20));
                    }
                    let vb = upload(device, "scene.mesh.vertices", bytemuck::cast_slice::<MeshVertex, u8>(&p.vertices), wgpu::BufferUsages::VERTEX)?;
                    let ib = upload(device, "scene.mesh.indices", bytemuck::cast_slice::<u32, u8>(&p.indices), wgpu::BufferUsages::INDEX)?;
                    parts.push((vb, ib, p.indices.len() as u32));
                    uploaded += vb_size + ib_size;
                }
                // only a complete mesh is kept (a failed upload leaves nothing half-drawn)
                g.bytes += parts.iter().map(|(v, i, _)| v.size() + i.size()).sum::<u64>();
                g.parts = parts;
            }
            let total = it.model.point_count();
            if np > 0 {
                let need = np.div_ceil(CHUNK_POINTS);
                if g.chunks.len() < total.div_ceil(CHUNK_POINTS) {
                    g.chunks.resize_with(total.div_ceil(CHUNK_POINTS), || None);
                }
                for c in 0..need {
                    if g.chunks[c].is_none() {
                        let range = c * CHUNK_POINTS..((c + 1) * CHUNK_POINTS).min(total);
                        let bytes: &[u8] = bytemuck::cast_slice::<PointVertex, u8>(&it.model.points[range]);
                        g.chunks[c] = Some(upload(device, "scene.points", bytes, wgpu::BufferUsages::VERTEX)?);
                        g.bytes += bytes.len() as u64;
                        uploaded += bytes.len() as u64;
                    }
                }
            }
        }
        let upload_ms = tu.elapsed().as_secs_f64() * 1000.0;

        // draw
        let grid_buf = match grid.as_ref().filter(|g| !g.1.is_empty()) {
            Some((_, v, _)) => Some((upload(device, "scene.grid", bytemuck::cast_slice::<PointVertex, u8>(v), wgpu::BufferUsages::VERTEX)?, v.len() as u32)),
            None => None,
        };
        let Some(targets) = st.targets.iter().find(|x| x.key == tkey) else { return Err("scene.view: render targets missing".into()) };
        let Some(uniforms) = &st.uniforms else { return Err("scene.view: uniform buffer missing".into()) };
        let bg = linear(f.background);
        let clear = wgpu::Color { r: (bg[0] * bg[3]) as f64, g: (bg[1] * bg[3]) as f64, b: (bg[2] * bg[3]) as f64, a: bg[3] as f64 };
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("scene.view") });
        let mut stats = Stats { samples, width: w, height: h, items: f.items.len(), pending: f.pending, points_total: f.total_points(), ..Stats::default() };
        {
            let (view_tex, resolve) = match &targets.color {
                Some(ms) => (ms, Some(t.view)),
                None => (t.view, None),
            };
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene.view"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: view_tex,
                    depth_slice: None,
                    resolve_target: resolve,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear), store: if resolve.is_some() { wgpu::StoreOp::Discard } else { wgpu::StoreOp::Store } },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &targets.depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            let slot = |k: usize| ((k as u64 + 1) * stride) as u32;
            let pipe = |kind: Pipe| st.pipelines.get(&(t.format, samples, kind));
            // opaque meshes, then points, then the grid, then translucent meshes
            for pass_kind in 0..4 {
                if pass_kind == 2 {
                    if let (Some((buf, n)), Some(p)) = (&grid_buf, pipe(Pipe::Lines)) {
                        pass.set_pipeline(p);
                        pass.set_bind_group(0, &uniforms.group, &[0]);
                        pass.set_vertex_buffer(0, buf.slice(..));
                        pass.draw(0..*n, 0..1);
                        stats.draw_calls += 1;
                    }
                    continue;
                }
                for (k, it) in f.items.iter().enumerate() {
                    let Some(g) = st.models.get(&it.key) else { continue };
                    let translucent = it.color[3] < 255;
                    match pass_kind {
                        0 | 3 if (pass_kind == 3) == translucent => {
                            let Some(p) = pipe(if translucent { Pipe::MeshBlend } else { Pipe::Mesh }).filter(|_| !g.parts.is_empty()) else { continue };
                            pass.set_pipeline(p);
                            pass.set_bind_group(0, &uniforms.group, &[slot(k)]);
                            for (vb, ib, n) in &g.parts {
                                pass.set_vertex_buffer(0, vb.slice(..));
                                pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                                pass.draw_indexed(0..*n, 0, 0..1);
                                stats.triangles += *n as usize / 3;
                                stats.draw_calls += 1;
                            }
                        }
                        1 => {
                            let np = counts[k];
                            if np == 0 {
                                continue;
                            }
                            let quads = it.point_size > 1.0;
                            let Some(p) = pipe(if quads { Pipe::Quads } else { Pipe::Points }) else { continue };
                            pass.set_pipeline(p);
                            pass.set_bind_group(0, &uniforms.group, &[slot(k)]);
                            for c in 0..np.div_ceil(CHUNK_POINTS) {
                                let Some(Some(buf)) = g.chunks.get(c) else { continue };
                                let n = (np - c * CHUNK_POINTS).min(CHUNK_POINTS) as u32;
                                pass.set_vertex_buffer(0, buf.slice(..));
                                if quads {
                                    pass.draw(0..6, 0..n);
                                } else {
                                    pass.draw(0..n, 0..1);
                                }
                                stats.points_drawn += n as usize;
                                stats.draw_calls += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        t.queue.submit(Some(enc.finish()));

        // forget models nobody holds any more, then the least recently used over budget
        st.models.retain(|_, g| g.model.strong_count() > 0);
        let mut total: u64 = st.models.values().map(|g| g.bytes).sum();
        if total > GPU_CACHE_BYTES {
            let mut old: Vec<((u64, u64), u64, u64)> = st.models.iter().filter(|(_, g)| g.last_used != frame_no).map(|(k, g)| (*k, g.last_used, g.bytes)).collect();
            old.sort_by_key(|x| x.1);
            for (k, _, b) in old {
                if total <= GPU_CACHE_BYTES {
                    break;
                }
                st.models.remove(&k);
                total -= b;
            }
        }
        stats.gpu_bytes = total;
        stats.uploaded_bytes = uploaded;
        stats.upload_ms = upload_ms;
        stats.renders = frame_no;
        stats.ms = t0.elapsed().as_secs_f64() * 1000.0;
        Ok(stats)
    }
}

impl GpuRenderer for SceneRenderer {
    fn render(&self, target: &GpuTarget, value: &Value) -> Result<(), String> {
        let f = value.downcast_ref::<Frame>().ok_or("scene.view: the layer value is not a scene frame")?;
        self.render_frame(target, f)
    }
}

/// GPU bytes of the vertex and index buffers of every distinct mesh in `f`.
pub fn mesh_gpu_bytes(f: &Frame) -> u64 {
    let mut seen: Vec<(u64, u64)> = Vec::new();
    let mut bytes = 0u64;
    for it in &f.items {
        if !seen.contains(&it.key) {
            seen.push(it.key);
            bytes += it.model.mesh.iter().map(|p| std::mem::size_of_val(p.vertices.as_slice()) as u64 + std::mem::size_of_val(p.indices.as_slice()) as u64).sum::<u64>();
        }
    }
    bytes
}

fn check_mesh_bytes(f: &Frame, limit: u64) -> Result<(), String> {
    let bytes = mesh_gpu_bytes(f);
    if bytes > limit {
        return Err(format!(
            "scene.view: the meshes in this scene need {} MB of GPU memory; the limit on this device is {} MB (simplify them or show fewer)",
            bytes.div_ceil(1 << 20),
            limit >> 20
        ));
    }
    Ok(())
}

/// Run `make` inside out-of-memory, validation and internal error scopes
/// and turn any error into `Err`, so a failed allocation never reaches
/// wgpu's uncaptured-error handler (which panics) and a broken object is
/// never used. Native wgpu reports errors synchronously, so popping does
/// not wait for the GPU.
pub fn guarded<T>(device: &wgpu::Device, what: &str, make: impl FnOnce() -> T) -> Result<T, String> {
    device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    device.push_error_scope(wgpu::ErrorFilter::Validation);
    device.push_error_scope(wgpu::ErrorFilter::Internal);
    let v = make();
    let internal = pollster::block_on(device.pop_error_scope());
    let validation = pollster::block_on(device.pop_error_scope());
    let oom = pollster::block_on(device.pop_error_scope());
    if let Some(e) = oom {
        return Err(format!("scene.view: out of GPU memory creating {what}: {e}"));
    }
    match validation.or(internal) {
        Some(e) => Err(format!("scene.view: creating {what} failed: {e}")),
        None => Ok(v),
    }
}

/// A vertex or index buffer holding `contents`, created mapped and filled
/// only once the device has accepted it (an out-of-memory buffer is never
/// mapped: wgpu panics on that).
fn upload(device: &wgpu::Device, label: &str, contents: &[u8], usage: wgpu::BufferUsages) -> Result<wgpu::Buffer, String> {
    let size = (contents.len() as u64).next_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT).max(wgpu::COPY_BUFFER_ALIGNMENT);
    let buf = guarded(device, &format!("{label} ({} KB)", size >> 10), || {
        device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size, usage, mapped_at_creation: true })
    })?;
    buf.slice(..).get_mapped_range_mut()[..contents.len()].copy_from_slice(contents);
    buf.unmap();
    Ok(buf)
}

/// Can this device render `format` with 4x MSAA? (WebGPU guarantees it for
/// the usual colour formats; checked once per device and format anyway.)
fn probe_msaa(device: &wgpu::Device, format: wgpu::TextureFormat) -> bool {
    guarded(device, "the MSAA probe", || {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("scene.msaa-probe"),
            size: wgpu::Extent3d { width: 4, height: 4, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 4,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
    })
    .is_ok()
}

fn build_pipeline(device: &wgpu::Device, layout: &wgpu::PipelineLayout, shader: &wgpu::ShaderModule, format: wgpu::TextureFormat, samples: u32, kind: Pipe) -> wgpu::RenderPipeline {
    use wgpu::PrimitiveTopology::{LineList, PointList, TriangleList};
    let mesh_attrs = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Unorm8x4];
    let point_attrs = wgpu::vertex_attr_array![0 => Float32x3, 1 => Unorm8x4];
    let mesh_buf = wgpu::VertexBufferLayout { array_stride: std::mem::size_of::<MeshVertex>() as u64, step_mode: wgpu::VertexStepMode::Vertex, attributes: &mesh_attrs };
    let point_buf = wgpu::VertexBufferLayout { array_stride: std::mem::size_of::<PointVertex>() as u64, step_mode: wgpu::VertexStepMode::Vertex, attributes: &point_attrs };
    let quad_buf = wgpu::VertexBufferLayout { step_mode: wgpu::VertexStepMode::Instance, ..point_buf.clone() };
    let (label, vs, fs, buf, topology, depth_write) = match kind {
        Pipe::Mesh => ("scene.mesh", "vs_mesh", "fs_mesh", &mesh_buf, TriangleList, true),
        Pipe::MeshBlend => ("scene.mesh.blend", "vs_mesh", "fs_mesh", &mesh_buf, TriangleList, false),
        Pipe::Points => ("scene.points", "vs_point", "fs_point", &point_buf, PointList, true),
        Pipe::Quads => ("scene.quads", "vs_quad", "fs_point", &quad_buf, TriangleList, true),
        Pipe::Lines => ("scene.lines", "vs_point", "fs_point", &point_buf, LineList, false),
    };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState { module: shader, entry_point: Some(vs), buffers: std::slice::from_ref(buf), compilation_options: Default::default() },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(fs),
            targets: &[Some(wgpu::ColorTargetState { format, blend: Some(PREMULTIPLIED), write_mask: wgpu::ColorWrites::ALL })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState { topology, cull_mode: None, ..Default::default() },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH,
            depth_write_enabled: depth_write,
            depth_compare: wgpu::CompareFunction::LessEqual,
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: wgpu::MultisampleState { count: samples, mask: !0, alpha_to_coverage_enabled: false },
        multiview: None,
        cache: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_and_grid() {
        assert_eq!(nice_step(0.7), 1.0);
        assert_eq!(nice_step(1.5), 2.0);
        assert_eq!(nice_step(3.0), 5.0);
        assert_eq!(nice_step(70.0), 100.0);
        let g = Grid { step: None, size: None, color: [1, 2, 3, 4], axes: true };
        let b = Bounds::new([-1.0, -1.0, -1.0], [1.0, 1.0, 1.0]);
        let (c, v, ext) = grid_lines(&g, &b, Up::Y);
        assert_eq!(c, [0.0, -1.0, 0.0]);
        assert!(v.len() > 20 && v.len() % 2 == 0);
        assert!(ext.max[0] >= 1.0 && ext.min[2] <= -1.0);
        // far-away data: no axes (the origin is not on the grid)
        let far = Bounds::new([500_000.0, 4_000_000.0, 10.0], [500_100.0, 4_000_100.0, 30.0]);
        let (c, v, _) = grid_lines(&g, &far, Up::Z);
        assert_eq!(c[2], 10.0);
        assert!(v.iter().all(|p| p.color == [1, 2, 3, 4]));
        assert!(v.iter().all(|p| p.pos[0].abs() < 1000.0 && p.pos[1].abs() < 1000.0));
    }

    #[test]
    fn mesh_memory_is_bounded_per_frame() {
        let boxm = crate::model_data(crate::synthetic::build_primitive(crate::synthetic::cuboid(1.0, 1.0, 1.0), "box").unwrap());
        let other = crate::model_data(crate::synthetic::build_primitive(crate::synthetic::cuboid(1.0, 1.0, 1.0), "box").unwrap());
        let item = |m: &Data| Data::map([("model", m.clone())]);
        // the same model twice is uploaded once: 24 vertices x 28 + 36 indices x 4
        let f = Frame::from_inputs(&[Data::map([("items", Data::list(vec![item(&boxm), item(&boxm), item(&other)]))])]).unwrap();
        assert_eq!(mesh_gpu_bytes(&f), 2 * 816);
        assert!(check_mesh_bytes(&f, 2 * 816).is_ok());
        let e = check_mesh_bytes(&f, 1000).unwrap_err();
        assert!(e.contains("need 1 MB") && e.contains("simplify"), "{e}");
    }
}
