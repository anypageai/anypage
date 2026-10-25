//! Generated geometry: primitives for pages that build scenes in code, and
//! synthetic point clouds for demos and load tests (random samples, so any
//! prefix is already a uniform sample).

use std::f64::consts::PI;

use crate::model::{try_vec, Ctl, MeshBuilder, Model, NormalMode, PointVertex, PointsBuilder, Rng, MAX_POINTS};

pub const MAX_SEGMENTS: usize = 1024;

fn quad(b: &mut MeshBuilder, corners: [[f32; 3]; 4], n: [f32; 3]) {
    let base = b.positions.len() as u32;
    b.positions.extend_from_slice(&corners);
    if let Some(ns) = &mut b.normals {
        ns.extend_from_slice(&[n; 4]);
    }
    b.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

/// An axis-aligned box centred on the origin with flat faces.
pub fn cuboid(sx: f32, sy: f32, sz: f32) -> MeshBuilder {
    let (x, y, z) = (sx * 0.5, sy * 0.5, sz * 0.5);
    let mut b = MeshBuilder { normals: Some(Vec::new()), ..Default::default() };
    quad(&mut b, [[-x, -y, z], [x, -y, z], [x, y, z], [-x, y, z]], [0.0, 0.0, 1.0]);
    quad(&mut b, [[x, -y, -z], [-x, -y, -z], [-x, y, -z], [x, y, -z]], [0.0, 0.0, -1.0]);
    quad(&mut b, [[x, -y, z], [x, -y, -z], [x, y, -z], [x, y, z]], [1.0, 0.0, 0.0]);
    quad(&mut b, [[-x, -y, -z], [-x, -y, z], [-x, y, z], [-x, y, -z]], [-1.0, 0.0, 0.0]);
    quad(&mut b, [[-x, y, z], [x, y, z], [x, y, -z], [-x, y, -z]], [0.0, 1.0, 0.0]);
    quad(&mut b, [[-x, -y, -z], [x, -y, -z], [x, -y, z], [-x, -y, z]], [0.0, -1.0, 0.0]);
    b
}

/// A UV sphere with smooth normals.
pub fn sphere(r: f32, segments: usize) -> MeshBuilder {
    let seg = segments.clamp(3, MAX_SEGMENTS);
    let rings = (seg / 2).max(2);
    let mut b = MeshBuilder { normals: Some(Vec::new()), ..Default::default() };
    for i in 0..=rings {
        let v = PI * i as f64 / rings as f64;
        for j in 0..=seg {
            let u = 2.0 * PI * j as f64 / seg as f64;
            let n = [(v.sin() * u.cos()) as f32, v.cos() as f32, (v.sin() * u.sin()) as f32];
            b.positions.push([n[0] * r, n[1] * r, n[2] * r]);
            if let Some(ns) = &mut b.normals {
                ns.push(n);
            }
        }
    }
    let w = seg as u32 + 1;
    for i in 0..rings as u32 {
        for j in 0..seg as u32 {
            let (a, c) = (i * w + j, (i + 1) * w + j);
            b.indices.extend_from_slice(&[a, a + 1, c, a + 1, c + 1, c]);
        }
    }
    b
}

/// A closed cylinder along Y, centred on the origin.
pub fn cylinder(r: f32, h: f32, segments: usize) -> MeshBuilder {
    let seg = segments.clamp(3, MAX_SEGMENTS);
    let hy = h * 0.5;
    let mut b = MeshBuilder { normals: Some(Vec::new()), ..Default::default() };
    for j in 0..seg {
        let a0 = 2.0 * PI * j as f64 / seg as f64;
        let a1 = 2.0 * PI * (j + 1) as f64 / seg as f64;
        let (c0, s0, c1, s1) = (a0.cos() as f32, a0.sin() as f32, a1.cos() as f32, a1.sin() as f32);
        let base = b.positions.len() as u32;
        b.positions.extend_from_slice(&[[r * c0, -hy, r * s0], [r * c1, -hy, r * s1], [r * c1, hy, r * s1], [r * c0, hy, r * s0]]);
        if let Some(ns) = &mut b.normals {
            ns.extend_from_slice(&[[c0, 0.0, s0], [c1, 0.0, s1], [c1, 0.0, s1], [c0, 0.0, s0]]);
        }
        b.indices.extend_from_slice(&[base, base + 2, base + 1, base, base + 3, base + 2]);
        // caps
        for (y, ny) in [(hy, 1.0f32), (-hy, -1.0)] {
            let base = b.positions.len() as u32;
            b.positions.extend_from_slice(&[[0.0, y, 0.0], [r * c0, y, r * s0], [r * c1, y, r * s1]]);
            if let Some(ns) = &mut b.normals {
                ns.extend_from_slice(&[[0.0, ny, 0.0]; 3]);
            }
            b.indices.extend_from_slice(&[base, base + 1, base + 2]);
        }
    }
    b
}

/// A square in the XZ plane (facing +Y), centred on the origin.
pub fn plane(size: f32) -> MeshBuilder {
    let s = size * 0.5;
    let mut b = MeshBuilder { normals: Some(Vec::new()), ..Default::default() };
    quad(&mut b, [[-s, 0.0, s], [s, 0.0, s], [s, 0.0, -s], [-s, 0.0, -s]], [0.0, 1.0, 0.0]);
    b
}

pub fn build_primitive(b: MeshBuilder, name: &str) -> Result<Model, String> {
    b.build(name, NormalMode::Smooth, false, &Ctl::none())
}

fn terrain_height(x: f64, z: f64) -> f64 {
    0.18 * (x * 1.3).sin() * (z * 1.1).cos() + 0.07 * (x * 3.7 + 1.0).sin() * (z * 2.9).sin() + 0.03 * (x * 9.1).cos() * (z * 8.3 + 2.0).sin() + 0.02 * (x * x + z * z)
}

fn lerp3(a: [f64; 3], b: [f64; 3], t: f64) -> [u8; 4] {
    let t = t.clamp(0.0, 1.0);
    let c = |k: usize| (a[k] + (b[k] - a[k]) * t).round().clamp(0.0, 255.0) as u8;
    [c(0), c(1), c(2), 255]
}

/// A synthetic cloud of `count` points: "terrain" (a 4 x 4 height field,
/// coloured by height), "sphere" (surface, coloured by direction), "cube"
/// (volume, coloured by position) or "helix".
pub fn synthetic_points(shape: &str, count: usize, seed: u64, ctl: &Ctl) -> Result<Model, String> {
    if count > MAX_POINTS {
        return Err(format!("{count} points; the limit is {MAX_POINTS}"));
    }
    if count == 0 {
        return Err("count must be at least 1".into());
    }
    let mut points: Vec<PointVertex> = try_vec(count, "points")?;
    let mut rng = Rng(seed ^ 0xA076_1D64_78BD_642F);
    for k in 0..count {
        if k & 0xF_FFFF == 0 {
            ctl.check()?;
            (ctl.progress)(k as f32 / count as f32);
        }
        let (p, c) = match shape {
            "terrain" => {
                let (x, z) = (rng.unit() * 4.0 - 2.0, rng.unit() * 4.0 - 2.0);
                let y = terrain_height(x, z);
                let t = (y + 0.25) / 0.6;
                let c = if t < 0.5 { lerp3([40.0, 110.0, 60.0], [150.0, 130.0, 70.0], t * 2.0) } else { lerp3([150.0, 130.0, 70.0], [245.0, 245.0, 250.0], t * 2.0 - 1.0) };
                ([x, y, z], c)
            }
            "sphere" => {
                let z = rng.unit() * 2.0 - 1.0;
                let a = rng.unit() * 2.0 * PI;
                let r = (1.0 - z * z).max(0.0).sqrt();
                let p = [r * a.cos(), z, r * a.sin()];
                let c = [((p[0] * 0.5 + 0.5) * 255.0) as u8, ((p[1] * 0.5 + 0.5) * 255.0) as u8, ((p[2] * 0.5 + 0.5) * 255.0) as u8, 255];
                (p, c)
            }
            "cube" => {
                let p = [rng.unit() * 2.0 - 1.0, rng.unit() * 2.0 - 1.0, rng.unit() * 2.0 - 1.0];
                let c = [((p[0] * 0.5 + 0.5) * 255.0) as u8, ((p[1] * 0.5 + 0.5) * 255.0) as u8, ((p[2] * 0.5 + 0.5) * 255.0) as u8, 255];
                (p, c)
            }
            "helix" => {
                let t = rng.unit();
                let a = t * 12.0 * PI;
                let jitter = [rng.unit() - 0.5, rng.unit() - 0.5, rng.unit() - 0.5];
                let p = [a.cos() + jitter[0] * 0.08, t * 4.0 - 2.0 + jitter[1] * 0.08, a.sin() + jitter[2] * 0.08];
                (p, lerp3([255.0, 80.0, 60.0], [60.0, 120.0, 255.0], t))
            }
            other => return Err(format!("unknown shape \"{other}\" (terrain, sphere, cube, helix)")),
        };
        points.push(PointVertex { pos: [p[0] as f32, p[1] as f32, p[2] as f32], color: c });
    }
    let mut pb = PointsBuilder::new([0.0; 3], 0)?;
    for p in &points {
        pb.bounds.add([p.pos[0] as f64, p.pos[1] as f64, p.pos[2] as f64]);
    }
    pb.points = points;
    pb.has_colors = true;
    // independent random samples: already in random order
    let mut m = pb.build("synthetic", false, None, ctl)?;
    m.shuffled = true;
    Ok(m)
}
