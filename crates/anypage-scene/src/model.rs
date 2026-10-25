//! The native 3D model behind a `scene.model` resource: mesh parts and a
//! point cloud in GPU-ready layouts, positions relative to an `f64` origin.
//!
//! Positions are stored as `f32` *relative to* [`Model::origin`], so LAS/UTM
//! coordinates (millions of metres) keep millimetre precision: the renderer
//! folds the origin into an `f64` model-view matrix before casting to `f32`.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use glam::{DMat4, DVec3};

/// Hard limits on what one model may hold (also checked before allocating).
pub const MAX_POINTS: usize = 60_000_000;
pub const MAX_VERTICES: usize = 40_000_000;
pub const MAX_TRIANGLES: usize = 40_000_000;
/// A mesh is split into parts so each GPU buffer stays well under the
/// 256 MiB `max_buffer_size` the engine requests.
pub const PART_MAX_VERTICES: usize = 4 << 20;
pub const PART_MAX_INDICES: usize = 12 << 20;

/// One point as the GPU reads it: 16 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct PointVertex {
    pub pos: [f32; 3],
    /// sRGB-encoded RGBA8 (not premultiplied).
    pub color: [u8; 4],
}

/// One mesh vertex as the GPU reads it: 28 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct MeshVertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    /// sRGB-encoded RGBA8 (not premultiplied).
    pub color: [u8; 4],
}

/// A piece of a mesh small enough for one vertex and one index buffer.
#[derive(Clone, Debug, Default)]
pub struct MeshPart {
    pub vertices: Vec<MeshVertex>,
    /// Triangle list, always a multiple of 3, every index < `vertices.len()`.
    pub indices: Vec<u32>,
}

/// Axis-aligned bounds in `f64`. Empty bounds have `min > max`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl Default for Bounds {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl Bounds {
    pub const EMPTY: Bounds = Bounds { min: [f64::INFINITY; 3], max: [f64::NEG_INFINITY; 3] };

    pub fn new(min: [f64; 3], max: [f64; 3]) -> Self {
        Self { min, max }
    }
    pub fn is_empty(&self) -> bool {
        !(self.min[0] <= self.max[0] && self.min[1] <= self.max[1] && self.min[2] <= self.max[2])
    }
    pub fn add(&mut self, p: [f64; 3]) {
        for (k, &x) in p.iter().enumerate() {
            if x < self.min[k] {
                self.min[k] = x;
            }
            if x > self.max[k] {
                self.max[k] = x;
            }
        }
    }
    pub fn union(&self, o: &Bounds) -> Bounds {
        if o.is_empty() {
            return *self;
        }
        if self.is_empty() {
            return *o;
        }
        let mut b = *self;
        b.add(o.min);
        b.add(o.max);
        b
    }
    pub fn center(&self) -> [f64; 3] {
        if self.is_empty() {
            return [0.0; 3];
        }
        [(self.min[0] + self.max[0]) * 0.5, (self.min[1] + self.max[1]) * 0.5, (self.min[2] + self.max[2]) * 0.5]
    }
    pub fn size(&self) -> [f64; 3] {
        if self.is_empty() {
            return [0.0; 3];
        }
        [self.max[0] - self.min[0], self.max[1] - self.min[1], self.max[2] - self.min[2]]
    }
    /// Radius of the bounding sphere around [`Bounds::center`].
    pub fn radius(&self) -> f64 {
        let s = self.size();
        0.5 * (s[0] * s[0] + s[1] * s[1] + s[2] * s[2]).sqrt()
    }
    pub fn offset(&self, o: [f64; 3]) -> Bounds {
        if self.is_empty() {
            return *self;
        }
        Bounds { min: [self.min[0] + o[0], self.min[1] + o[1], self.min[2] + o[2]], max: [self.max[0] + o[0], self.max[1] + o[1], self.max[2] + o[2]] }
    }
    /// The axis-aligned bounds of the eight transformed corners.
    pub fn transform(&self, m: &DMat4) -> Bounds {
        if self.is_empty() {
            return *self;
        }
        let mut b = Bounds::EMPTY;
        for i in 0..8 {
            let c = DVec3::new(
                if i & 1 == 0 { self.min[0] } else { self.max[0] },
                if i & 2 == 0 { self.min[1] } else { self.max[1] },
                if i & 4 == 0 { self.min[2] } else { self.max[2] },
            );
            let p = m.transform_point3(c);
            if p.is_finite() {
                b.add(p.to_array());
            }
        }
        b
    }
}

/// A loaded or built model. Immutable once built; shared by `Arc` between the
/// script resource, layer values and the GPU cache.
#[derive(Debug, Default)]
pub struct Model {
    /// Where it came from: "glb", "obj", "ply", "las", "mesh", "points", ...
    pub format: String,
    /// Added to every stored position to get model coordinates.
    pub origin: [f64; 3],
    pub mesh: Vec<MeshPart>,
    /// Shuffled at load (unless asked not to) so any prefix is a uniform sample.
    pub points: Vec<PointVertex>,
    /// Bounds of the stored positions (relative to `origin`).
    pub local_bounds: Bounds,
    pub has_colors: bool,
    /// Normals came from the source (otherwise they were computed).
    pub source_normals: bool,
    pub shuffled: bool,
    /// Points in the source before any `max_points` sampling.
    pub source_points: u64,
}

impl Model {
    /// Bounds in model coordinates (origin applied).
    pub fn bounds(&self) -> Bounds {
        self.local_bounds.offset(self.origin)
    }
    pub fn vertex_count(&self) -> usize {
        self.mesh.iter().map(|p| p.vertices.len()).sum()
    }
    pub fn triangle_count(&self) -> usize {
        self.mesh.iter().map(|p| p.indices.len() / 3).sum()
    }
    pub fn point_count(&self) -> usize {
        self.points.len()
    }
    pub fn kind(&self) -> &'static str {
        match (self.triangle_count() > 0, !self.points.is_empty()) {
            (true, true) => "mixed",
            (true, false) => "mesh",
            (false, true) => "points",
            (false, false) => "empty",
        }
    }
    /// Approximate CPU memory held, in bytes.
    pub fn bytes(&self) -> usize {
        self.points.len() * std::mem::size_of::<PointVertex>()
            + self.mesh.iter().map(|p| p.vertices.len() * std::mem::size_of::<MeshVertex>() + p.indices.len() * 4).sum::<usize>()
    }
}

/// Cancellation and progress for long loads. Only used on the calling thread.
pub struct Ctl<'a> {
    pub cancelled: &'a dyn Fn() -> bool,
    pub progress: &'a dyn Fn(f32),
}

impl Ctl<'_> {
    pub fn none() -> Ctl<'static> {
        Ctl { cancelled: &|| false, progress: &|_| {} }
    }
    pub fn check(&self) -> Result<(), String> {
        if (self.cancelled)() {
            Err("cancelled".into())
        } else {
            Ok(())
        }
    }
}

/// Reserve without aborting the process when memory runs out.
pub fn try_vec<T>(n: usize, what: &str) -> Result<Vec<T>, String> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| format!("not enough memory for {n} {what}"))?;
    Ok(v)
}

/// How to get normals when the source has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NormalMode {
    /// Area-weighted vertex normals over shared vertices.
    Smooth,
    /// One normal per face (vertices are split).
    Flat,
}

impl NormalMode {
    pub fn parse(s: Option<&str>, default: NormalMode) -> Result<NormalMode, String> {
        match s {
            None => Ok(default),
            Some("smooth") => Ok(NormalMode::Smooth),
            Some("flat") => Ok(NormalMode::Flat),
            Some(o) => Err(format!("normals must be \"smooth\" or \"flat\", not \"{o}\"")),
        }
    }
}

/// Collects one indexed triangle mesh (any source), then normalises it into
/// [`MeshPart`]s: recentred, normals computed if missing, split for the GPU.
///
/// `positions` are relative to `offset`, which loaders with `f64` input
/// (PLY doubles, glTF node transforms, OBJ) set near the data before
/// narrowing to `f32`, so far-away coordinates keep their precision.
#[derive(Default)]
pub struct MeshBuilder {
    pub positions: Vec<[f32; 3]>,
    pub normals: Option<Vec<[f32; 3]>>,
    pub colors: Option<Vec<[u8; 4]>>,
    pub indices: Vec<u32>,
    /// Added to every position to get model coordinates.
    pub offset: [f64; 3],
}

/// The first point with three finite coordinates (an origin for `f32`
/// storage; NaN points, common at index 0 of organised clouds, are skipped).
pub fn first_finite(points: impl IntoIterator<Item = [f64; 3]>) -> Option<[f64; 3]> {
    points.into_iter().find(|p| p.iter().all(|x| x.is_finite()))
}

/// `p - offset` narrowed to `f32` (NaN for non-finite input).
#[inline]
pub fn relative(p: [f64; 3], offset: [f64; 3]) -> [f32; 3] {
    [(p[0] - offset[0]) as f32, (p[1] - offset[1]) as f32, (p[2] - offset[2]) as f32]
}

pub const WHITE: [u8; 4] = [255, 255, 255, 255];

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if l > 1e-30 && l.is_finite() {
        [v[0] / l, v[1] / l, v[2] / l]
    } else {
        [0.0; 3]
    }
}

/// Unnormalised face normal (length = 2 x area).
pub fn face_normal(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f32; 3] {
    cross(sub(b, a), sub(c, a))
}

impl MeshBuilder {
    pub fn vertex_count(&self) -> usize {
        self.positions.len()
    }

    /// Append another indexed mesh (its indices are local to it).
    pub fn append(&mut self, o: MeshBuilder) -> Result<(), String> {
        let base = self.positions.len();
        if base + o.positions.len() > MAX_VERTICES {
            return Err(format!("more than {MAX_VERTICES} vertices"));
        }
        if (self.indices.len() + o.indices.len()) / 3 > MAX_TRIANGLES {
            return Err(format!("more than {MAX_TRIANGLES} triangles"));
        }
        // keep optional channels aligned with positions
        match (&mut self.normals, o.normals) {
            (Some(n), Some(on)) => n.extend(on),
            (Some(n), None) => n.extend(std::iter::repeat_n([0.0; 3], o.positions.len())),
            (None, Some(on)) if base == 0 => self.normals = Some(on),
            (None, Some(_)) => {} // earlier parts had none: recompute all
            (None, None) => {}
        }
        match (&mut self.colors, o.colors) {
            (Some(c), Some(oc)) => c.extend(oc),
            (Some(c), None) => c.extend(std::iter::repeat_n(WHITE, o.positions.len())),
            (None, Some(oc)) => {
                let mut c = vec![WHITE; base];
                c.extend(oc);
                self.colors = Some(c);
            }
            (None, None) => {}
        }
        if base == 0 {
            self.offset = o.offset;
            self.positions.extend(o.positions);
        } else {
            // re-base onto this builder's offset; the difference is taken in f64
            let d = [o.offset[0] - self.offset[0], o.offset[1] - self.offset[1], o.offset[2] - self.offset[2]];
            if d == [0.0; 3] {
                self.positions.extend(o.positions);
            } else {
                self.positions.extend(o.positions.iter().map(|p| [(p[0] as f64 + d[0]) as f32, (p[1] as f64 + d[1]) as f32, (p[2] as f64 + d[2]) as f32]));
            }
        }
        self.indices.extend(o.indices.into_iter().map(|i| i + base as u32));
        Ok(())
    }

    /// Check index ranges and triangle counts.
    pub fn validate(&self) -> Result<(), String> {
        if self.positions.len() > MAX_VERTICES {
            return Err(format!("{} vertices; the limit is {MAX_VERTICES}", self.positions.len()));
        }
        if !self.indices.len().is_multiple_of(3) {
            return Err(format!("{} indices is not a multiple of 3 (triangle lists only)", self.indices.len()));
        }
        if self.indices.len() / 3 > MAX_TRIANGLES {
            return Err(format!("{} triangles; the limit is {MAX_TRIANGLES}", self.indices.len() / 3));
        }
        let n = self.positions.len() as u32;
        if let Some(bad) = self.indices.iter().find(|&&i| i >= n) {
            return Err(format!("index {bad} is out of range ({n} vertices)"));
        }
        if self.normals.as_ref().is_some_and(|v| v.len() != self.positions.len()) {
            return Err("normals must have one entry per vertex".into());
        }
        if self.colors.as_ref().is_some_and(|v| v.len() != self.positions.len()) {
            return Err("colors must have one entry per vertex".into());
        }
        Ok(())
    }

    fn smooth_normals(&self, ctl: &Ctl) -> Result<Vec<[f32; 3]>, String> {
        let mut acc = vec![[0f32; 3]; self.positions.len()];
        for (k, t) in self.indices.as_chunks::<3>().0.iter().enumerate() {
            if k & 0xF_FFFF == 0 {
                ctl.check()?;
            }
            let (a, b, c) = (t[0] as usize, t[1] as usize, t[2] as usize);
            let n = face_normal(self.positions[a], self.positions[b], self.positions[c]);
            for v in [a, b, c] {
                acc[v][0] += n[0];
                acc[v][1] += n[1];
                acc[v][2] += n[2];
            }
        }
        for n in &mut acc {
            *n = normalize(*n);
        }
        Ok(acc)
    }

    /// Split every vertex per triangle and give it the face normal.
    fn flatten(&mut self, ctl: &Ctl) -> Result<(), String> {
        let tris = self.indices.len() / 3;
        if tris.saturating_mul(3) > MAX_VERTICES {
            return Err(format!(
                "flat shading needs 3 vertices per triangle: {tris} triangles would be {} vertices; the limit is {MAX_VERTICES} (use normals = \"smooth\")",
                tris.saturating_mul(3)
            ));
        }
        let mut pos = try_vec(tris * 3, "vertices")?;
        let mut nor = try_vec(tris * 3, "normals")?;
        let mut col = match self.colors {
            Some(_) => Some(try_vec(tris * 3, "colours")?),
            None => None,
        };
        for (k, t) in self.indices.as_chunks::<3>().0.iter().enumerate() {
            if k & 0xF_FFFF == 0 {
                ctl.check()?;
            }
            let p = [self.positions[t[0] as usize], self.positions[t[1] as usize], self.positions[t[2] as usize]];
            let n = normalize(face_normal(p[0], p[1], p[2]));
            pos.extend_from_slice(&p);
            nor.extend_from_slice(&[n, n, n]);
            if let (Some(out), Some(src)) = (&mut col, &self.colors) {
                out.extend(t.iter().map(|&i| src[i as usize]));
            }
        }
        self.positions = pos;
        self.normals = Some(nor);
        self.colors = col;
        self.indices = (0..(tris * 3) as u32).collect();
        Ok(())
    }

    /// Compute normals now if there are none (before [`MeshBuilder::append`]
    /// merges pieces that differ in whether they have normals).
    pub fn ensure_normals(&mut self, mode: NormalMode, ctl: &Ctl) -> Result<(), String> {
        if self.normals.is_some() {
            return Ok(());
        }
        self.validate()?;
        match mode {
            NormalMode::Flat => self.flatten(ctl),
            NormalMode::Smooth => {
                self.normals = Some(self.smooth_normals(ctl)?);
                Ok(())
            }
        }
    }

    /// Normalise into a model. `normals` decides what to do when the source
    /// had none (or `force_flat` to discard source normals).
    pub fn build(mut self, format: &str, normals: NormalMode, force_flat: bool, ctl: &Ctl) -> Result<Model, String> {
        self.validate()?;
        let source_normals = self.normals.is_some() && !force_flat;
        if force_flat || (self.normals.is_none() && normals == NormalMode::Flat) {
            self.flatten(ctl)?;
        } else if self.normals.is_none() {
            self.normals = Some(self.smooth_normals(ctl)?);
        }
        let has_colors = self.colors.is_some();
        // recentre: origin = bounds centre, positions relative to it (the
        // positions are already relative to `offset`, so this stays precise)
        let mut b = Bounds::EMPTY;
        for p in &self.positions {
            if p.iter().all(|x| x.is_finite()) {
                b.add([p[0] as f64, p[1] as f64, p[2] as f64]);
            }
        }
        let centre = b.center();
        let off = if self.offset.iter().all(|x| x.is_finite()) { self.offset } else { [0.0; 3] };
        let origin = [off[0] + centre[0], off[1] + centre[1], off[2] + centre[2]];
        let mut local = Bounds::EMPTY;
        for p in &mut self.positions {
            for k in 0..3 {
                p[k] = if p[k].is_finite() { (p[k] as f64 - centre[k]) as f32 } else { 0.0 };
            }
            local.add([p[0] as f64, p[1] as f64, p[2] as f64]);
        }
        if self.positions.is_empty() {
            local = Bounds::EMPTY;
        }
        ctl.check()?;
        let parts = split_parts(&self, ctl)?;
        Ok(Model {
            format: format.to_string(),
            origin,
            mesh: parts,
            points: Vec::new(),
            local_bounds: local,
            has_colors,
            source_normals,
            shuffled: false,
            source_points: 0,
        })
    }
}

/// Split a mesh so every part fits [`PART_MAX_VERTICES`] / [`PART_MAX_INDICES`].
fn split_parts(m: &MeshBuilder, ctl: &Ctl) -> Result<Vec<MeshPart>, String> {
    let normals = m.normals.as_deref();
    let colors = m.colors.as_deref();
    let vertex = |i: usize| MeshVertex {
        pos: m.positions[i],
        normal: normals.map(|n| n[i]).unwrap_or([0.0; 3]),
        color: colors.map(|c| c[i]).unwrap_or(WHITE),
    };
    if m.indices.is_empty() {
        return Ok(Vec::new());
    }
    if m.positions.len() <= PART_MAX_VERTICES && m.indices.len() <= PART_MAX_INDICES {
        let mut vertices = try_vec(m.positions.len(), "vertices")?;
        vertices.extend((0..m.positions.len()).map(vertex));
        return Ok(vec![MeshPart { vertices, indices: m.indices.clone() }]);
    }
    // remap triangle by triangle; `local[v]` is v's index in the current part
    const NONE: u32 = u32::MAX;
    let mut local = vec![NONE; m.positions.len()];
    let mut touched: Vec<u32> = Vec::new();
    let mut parts = Vec::new();
    let mut cur = MeshPart::default();
    for (k, t) in m.indices.as_chunks::<3>().0.iter().enumerate() {
        if k & 0xF_FFFF == 0 {
            ctl.check()?;
        }
        if cur.vertices.len() + 3 > PART_MAX_VERTICES || cur.indices.len() + 3 > PART_MAX_INDICES {
            for &v in &touched {
                local[v as usize] = NONE;
            }
            touched.clear();
            parts.push(std::mem::take(&mut cur));
        }
        for &v in t {
            let slot = &mut local[v as usize];
            if *slot == NONE {
                *slot = cur.vertices.len() as u32;
                cur.vertices.push(vertex(v as usize));
                touched.push(v);
            }
            cur.indices.push(*slot);
        }
    }
    if !cur.indices.is_empty() {
        parts.push(cur);
    }
    Ok(parts)
}

/// Collects points relative to a fixed origin (chosen by the loader, e.g. the
/// LAS header's centre, or the first finite point), then shuffles.
pub struct PointsBuilder {
    pub origin: [f64; 3],
    pub points: Vec<PointVertex>,
    pub bounds: Bounds,
    pub has_colors: bool,
}

impl PointsBuilder {
    /// `origin` with any non-finite coordinate means "the first finite
    /// point pushed" (see [`PointsBuilder::auto`]).
    pub fn new(origin: [f64; 3], capacity: usize) -> Result<Self, String> {
        if capacity > MAX_POINTS {
            return Err(format!("{capacity} points; the limit is {MAX_POINTS}"));
        }
        Ok(Self { origin, points: try_vec(capacity, "points")?, bounds: Bounds::EMPTY, has_colors: false })
    }

    /// The origin becomes the first finite point pushed, so a cloud whose
    /// first points are NaN (organised scans) still keeps every valid point.
    pub fn auto(capacity: usize) -> Result<Self, String> {
        Self::new([f64::NAN; 3], capacity)
    }

    #[inline]
    pub fn push(&mut self, p: [f64; 3], color: [u8; 4]) -> Result<(), String> {
        if !(p[0].is_finite() && p[1].is_finite() && p[2].is_finite()) {
            return Ok(()); // skip NaN / infinite points
        }
        if self.points.len() >= MAX_POINTS {
            return Err(format!("more than {MAX_POINTS} points"));
        }
        if !self.origin.iter().all(|x| x.is_finite()) {
            self.origin = p;
        }
        let l = [(p[0] - self.origin[0]) as f32, (p[1] - self.origin[1]) as f32, (p[2] - self.origin[2]) as f32];
        self.bounds.add([l[0] as f64, l[1] as f64, l[2] as f64]);
        if self.points.len() == self.points.capacity() {
            self.points.try_reserve(self.points.len().max(1024)).map_err(|_| "not enough memory for the points".to_string())?;
        }
        self.points.push(PointVertex { pos: l, color });
        Ok(())
    }

    /// Shuffle (so prefixes are uniform samples), optionally keep only
    /// `max_points` (the first of the shuffled points, a random sample; or,
    /// unshuffled, every k-th point in file order), and finish.
    pub fn build(mut self, format: &str, shuffle: bool, max_points: Option<usize>, ctl: &Ctl) -> Result<Model, String> {
        let source_points = self.points.len() as u64;
        if !self.origin.iter().all(|x| x.is_finite()) {
            self.origin = [0.0; 3]; // nothing was pushed
        }
        if shuffle {
            shuffle_points(&mut self.points, 0x5eed_a11c_0ffe_e000, ctl)?;
        }
        if let Some(m) = max_points {
            if m < self.points.len() {
                if !shuffle {
                    stride_sample(&mut self.points, m);
                }
                self.points.truncate(m);
                self.points.shrink_to_fit();
                self.bounds = Bounds::EMPTY;
                for p in &self.points {
                    self.bounds.add([p.pos[0] as f64, p.pos[1] as f64, p.pos[2] as f64]);
                }
            }
        }
        Ok(Model {
            format: format.to_string(),
            origin: self.origin,
            mesh: Vec::new(),
            points: self.points,
            local_bounds: self.bounds,
            has_colors: self.has_colors,
            source_normals: false,
            shuffled: shuffle,
            source_points,
        })
    }
}

/// Move an evenly spaced selection of `m` points (in file order) to the
/// front: point `i` becomes source point `floor(i * n / m)`.
fn stride_sample(p: &mut [PointVertex], m: usize) {
    let n = p.len();
    if m == 0 || m >= n {
        return;
    }
    for i in 0..m {
        // the source index is >= i and increasing, so nothing is read after being overwritten
        let j = (i as u128 * n as u128 / m as u128) as usize;
        p[i] = p[j];
    }
}

/// SplitMix64: small, fast, good enough for shuffling and synthetic data.
pub struct Rng(pub u64);

impl Rng {
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in `0..n` (Lemire's multiply-shift; negligible bias for n < 2^32).
    #[inline]
    pub fn below(&mut self, n: usize) -> usize {
        (((self.next_u64() >> 32) * n as u64) >> 32) as usize
    }
    /// Uniform in [0, 1).
    #[inline]
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}

/// Fisher-Yates over the whole cloud: afterwards every prefix is a uniform
/// random sample, so drawing the first `budget` points is an unbiased preview.
pub fn shuffle_points(p: &mut [PointVertex], seed: u64, ctl: &Ctl) -> Result<(), String> {
    let mut rng = Rng(seed);
    let n = p.len();
    if n < 2 {
        return Ok(());
    }
    for i in (1..n).rev() {
        if i & 0xF_FFFF == 0 {
            ctl.check()?;
            (ctl.progress)(0.9 + 0.1 * (1.0 - i as f32 / n as f32));
        }
        // i + 1 <= u32::MAX is required by `below`; above that use the full u64 path
        let j = if i < u32::MAX as usize { rng.below(i + 1) } else { (rng.next_u64() % (i as u64 + 1)) as usize };
        p.swap(i, j);
    }
    Ok(())
}

/// sRGB-encoded byte from a linear 0..1 value (glTF colours are linear).
pub fn linear_to_srgb8(x: f32) -> u8 {
    let x = if x.is_finite() { x.clamp(0.0, 1.0) } else { 0.0 };
    let s = if x <= 0.003_130_8 { x * 12.92 } else { 1.055 * x.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0 + 0.5) as u8
}

/// Linear 0..1 from an sRGB-encoded byte.
pub fn srgb8_to_linear(c: u8) -> f32 {
    let s = c as f32 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

pub type ModelRef = Arc<Model>;

#[cfg(test)]
mod tests {
    use super::*;

    fn quad() -> MeshBuilder {
        MeshBuilder {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]],
            indices: vec![0, 1, 2, 0, 2, 3],
            ..Default::default()
        }
    }

    #[test]
    fn flat_shading_respects_the_vertex_limit() {
        // 13.4 M triangles over 3 vertices: fine indexed, too many split
        let tris = MAX_VERTICES / 3 + 100_000;
        let mut b = MeshBuilder { positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]], ..Default::default() };
        b.indices = [0u32, 1, 2].repeat(tris);
        let e = b.build("t", NormalMode::Flat, false, &Ctl::none()).unwrap_err();
        assert!(e.contains("flat shading needs 3 vertices per triangle") && e.contains("smooth"), "{e}");
    }

    #[test]
    fn offsets_keep_far_meshes_precise() {
        // two pieces 4000 km out, 1 mm apart, built relative to f64 offsets
        let far = [500_000.0, 4_000_000.0, 0.0];
        let piece = |dx: f64| MeshBuilder {
            positions: vec![[0.0, 0.0, 0.0], [0.001, 0.0, 0.0], [0.0, 0.001, 0.0]],
            indices: vec![0, 1, 2],
            offset: [far[0] + dx, far[1], far[2]],
            ..Default::default()
        };
        let mut all = piece(0.0);
        all.append(piece(0.001)).unwrap();
        let m = all.build("t", NormalMode::Smooth, false, &Ctl::none()).unwrap();
        let xs: Vec<f64> = m.mesh[0].vertices.iter().map(|v| m.origin[0] + v.pos[0] as f64).collect();
        let want = [far[0], far[0] + 0.001, far[0], far[0] + 0.001, far[0] + 0.002, far[0] + 0.001];
        // (f32 world coordinates at this distance step by 0.25-0.5 m)
        for (x, w) in xs.iter().zip(want) {
            assert!((x - w).abs() < 1e-6, "{x} vs {w}");
        }
    }

    #[test]
    fn nan_first_point_keeps_the_cloud() {
        let mut b = PointsBuilder::auto(4).unwrap();
        b.push([f64::NAN; 3], WHITE).unwrap();
        b.push([10.0, 20.0, 30.0], WHITE).unwrap();
        b.push([11.0, 22.0, 33.0], WHITE).unwrap();
        let m = b.build("t", false, None, &Ctl::none()).unwrap();
        assert_eq!(m.origin, [10.0, 20.0, 30.0]);
        assert_eq!(m.point_count(), 2);
        assert_eq!(m.bounds(), Bounds::new([10.0, 20.0, 30.0], [11.0, 22.0, 33.0]));
        // nothing pushed: an empty cloud at the origin, not NaN
        let m = PointsBuilder::auto(0).unwrap().build("t", true, None, &Ctl::none()).unwrap();
        assert_eq!(m.origin, [0.0; 3]);
    }

    #[test]
    fn unshuffled_max_points_takes_an_even_stride() {
        let mut b = PointsBuilder::new([0.0; 3], 100).unwrap();
        for i in 0..100 {
            b.push([i as f64, 0.0, 0.0], WHITE).unwrap();
        }
        let m = b.build("t", false, Some(10), &Ctl::none()).unwrap();
        let xs: Vec<f32> = m.points.iter().map(|p| p.pos[0]).collect();
        assert_eq!(xs, vec![0.0, 10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0]);
        assert_eq!(m.bounds(), Bounds::new([0.0; 3], [90.0, 0.0, 0.0]));
    }

    #[test]
    fn smooth_and_flat_normals() {
        let m = quad().build("t", NormalMode::Smooth, false, &Ctl::none()).unwrap();
        assert_eq!(m.vertex_count(), 4);
        assert_eq!(m.triangle_count(), 2);
        for v in &m.mesh[0].vertices {
            assert!((v.normal[2] - 1.0).abs() < 1e-6, "{:?}", v.normal);
        }
        // recentred around the bounds centre
        assert_eq!(m.origin, [0.5, 0.5, 0.0]);
        assert_eq!(m.bounds(), Bounds::new([0.0, 0.0, 0.0], [1.0, 1.0, 0.0]));
        let f = quad().build("t", NormalMode::Flat, false, &Ctl::none()).unwrap();
        assert_eq!(f.vertex_count(), 6);
        assert!(!f.source_normals);
    }

    #[test]
    fn bad_indices_are_errors() {
        let mut q = quad();
        q.indices.push(9);
        q.indices.extend([0, 1]);
        assert!(q.build("t", NormalMode::Smooth, false, &Ctl::none()).unwrap_err().contains("out of range"));
        let mut q = quad();
        q.indices.pop();
        assert!(q.build("t", NormalMode::Smooth, false, &Ctl::none()).unwrap_err().contains("multiple of 3"));
    }

    #[test]
    fn splitting_keeps_every_triangle() {
        // a strip of 2 * n triangles over 2 * (n + 1) vertices, forced to split
        let n = 3_000_000usize;
        let mut b = MeshBuilder::default();
        for i in 0..=n {
            b.positions.push([i as f32, 0.0, 0.0]);
            b.positions.push([i as f32, 1.0, 0.0]);
        }
        for i in 0..n as u32 {
            let (a, c) = (2 * i, 2 * i + 2);
            b.indices.extend([a, c, a + 1, a + 1, c, c + 1]);
        }
        let m = b.build("t", NormalMode::Smooth, false, &Ctl::none()).unwrap();
        assert!(m.mesh.len() >= 2, "{} parts", m.mesh.len());
        assert_eq!(m.triangle_count(), 2 * n);
        for p in &m.mesh {
            assert!(p.vertices.len() <= PART_MAX_VERTICES && p.indices.len() <= PART_MAX_INDICES);
            assert!(p.indices.iter().all(|&i| (i as usize) < p.vertices.len()));
        }
    }

    #[test]
    fn shuffle_is_a_permutation_and_prefixes_are_spread() {
        let mut b = PointsBuilder::new([0.0; 3], 100_000).unwrap();
        for i in 0..100_000 {
            b.push([i as f64, 0.0, 0.0], WHITE).unwrap();
        }
        let m = b.build("t", true, None, &Ctl::none()).unwrap();
        let mut xs: Vec<u32> = m.points.iter().map(|p| p.pos[0] as u32).collect();
        // the first 1000 points cover the whole range roughly evenly
        let first: Vec<u32> = xs[..1000].to_vec();
        let low = first.iter().filter(|&&x| x < 50_000).count();
        assert!((400..600).contains(&low), "{low} of 1000 in the lower half");
        xs.sort_unstable();
        assert!(xs.iter().enumerate().all(|(i, &x)| i as u32 == x));
    }

    #[test]
    fn srgb_round_trip() {
        for c in [0u8, 1, 10, 128, 200, 255] {
            assert_eq!(linear_to_srgb8(srgb8_to_linear(c)), c);
        }
    }

    #[test]
    fn far_coordinates_keep_precision() {
        let mut b = PointsBuilder::new([500_000.0, 4_000_000.0, 100.0], 2).unwrap();
        b.push([500_000.001, 4_000_000.002, 100.003], WHITE).unwrap();
        let m = b.build("t", false, None, &Ctl::none()).unwrap();
        let p = m.points[0].pos;
        assert!((p[0] as f64 - 0.001).abs() < 1e-6 && (p[1] as f64 - 0.002).abs() < 1e-6);
    }
}
