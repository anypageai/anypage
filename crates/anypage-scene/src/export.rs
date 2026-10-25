//! Writing models back out: binary PLY (mesh and/or points, with colours
//! and normals), binary STL (meshes), OBJ text (meshes; points as `v` lines)
//! and XYZ text (points). Coordinates are written in model space (origin
//! added back); PLY switches to `double` when the origin is far from zero so
//! survey coordinates survive a round trip. Long writes check for
//! cancellation (the `scene.save` job).

use std::fmt::Write as _;

use crate::model::{try_vec, Ctl, Model};

/// Largest file `save` produces.
pub const MAX_OUTPUT: usize = 2 << 30;

fn world(m: &Model, p: [f32; 3]) -> [f64; 3] {
    [m.origin[0] + p[0] as f64, m.origin[1] + p[1] as f64, m.origin[2] + p[2] as f64]
}

fn check(n: usize) -> Result<(), String> {
    if n > MAX_OUTPUT {
        Err(format!("the file would be {} MB; the limit is {} MB", n >> 20, MAX_OUTPUT >> 20))
    } else {
        Ok(())
    }
}

pub fn save(m: &Model, format: &str) -> Result<Vec<u8>, String> {
    save_with(m, format, &Ctl::none())
}

/// [`save`] with cancellation and progress.
pub fn save_with(m: &Model, format: &str, ctl: &Ctl) -> Result<Vec<u8>, String> {
    let out = match format.to_ascii_lowercase().as_str() {
        "ply" => ply(m, ctl),
        "stl" => stl(m, ctl),
        "obj" => obj(m, ctl),
        "xyz" => xyz(m, ctl),
        other => Err(format!("cannot save as \"{other}\" (ply, stl, obj, xyz)")),
    }?;
    (ctl.progress)(1.0);
    Ok(out)
}

/// About how many bytes [`save`] would produce (an upper estimate for the
/// text formats); `None` for an unknown format.
pub fn estimate(m: &Model, format: &str) -> Option<usize> {
    let (v, p, t) = (m.vertex_count(), m.point_count(), m.triangle_count());
    Some(match format.to_ascii_lowercase().as_str() {
        "ply" => (v + p) * 44 + t * 13 + 512,
        "stl" => 84 + t * 50,
        "obj" => (v + p) * 60 + t * 40,
        "xyz" => p * 64,
        _ => return None,
    })
}

/// Every `STEP` items: cancelled? and progress.
const STEP: usize = 1 << 20;

fn tick(ctl: &Ctl, k: usize, n: usize) -> Result<(), String> {
    if k.is_multiple_of(STEP) {
        ctl.check()?;
        (ctl.progress)(k as f32 / n.max(1) as f32);
    }
    Ok(())
}

fn ply(m: &Model, ctl: &Ctl) -> Result<Vec<u8>, String> {
    let double = m.origin.iter().any(|o| o.abs() > 1e5);
    let mesh = m.triangle_count() > 0;
    let nv = m.vertex_count() + m.point_count();
    let ft = if double { "double" } else { "float" };
    let per_vertex = if double { 24 } else { 12 } + if mesh { 12 } else { 0 } + 4;
    check(nv * per_vertex + m.triangle_count() * 13 + 512)?;
    let mut h = String::new();
    let _ = writeln!(h, "ply\nformat binary_little_endian 1.0\ncomment anypage-scene ({})", m.format);
    let _ = writeln!(h, "element vertex {nv}\nproperty {ft} x\nproperty {ft} y\nproperty {ft} z");
    if mesh {
        h.push_str("property float nx\nproperty float ny\nproperty float nz\n");
    }
    h.push_str("property uchar red\nproperty uchar green\nproperty uchar blue\nproperty uchar alpha\n");
    if mesh {
        let _ = writeln!(h, "element face {}\nproperty list uchar uint vertex_indices", m.triangle_count());
    }
    h.push_str("end_header\n");
    let mut out: Vec<u8> = try_vec(h.len() + nv * per_vertex + m.triangle_count() * 13, "bytes")?;
    out.extend_from_slice(h.as_bytes());
    let pos = |out: &mut Vec<u8>, p: [f32; 3]| {
        let w = world(m, p);
        for v in w {
            if double {
                out.extend_from_slice(&v.to_le_bytes());
            } else {
                out.extend_from_slice(&(v as f32).to_le_bytes());
            }
        }
    };
    let total = nv + m.triangle_count();
    let mut k = 0usize;
    for part in &m.mesh {
        for v in &part.vertices {
            tick(ctl, k, total)?;
            k += 1;
            pos(&mut out, v.pos);
            for n in v.normal {
                out.extend_from_slice(&n.to_le_bytes());
            }
            out.extend_from_slice(&v.color);
        }
    }
    for p in &m.points {
        tick(ctl, k, total)?;
        k += 1;
        pos(&mut out, p.pos);
        if mesh {
            out.extend_from_slice(&[0u8; 12]);
        }
        out.extend_from_slice(&p.color);
    }
    let mut base = 0u32;
    for part in &m.mesh {
        for t in part.indices.as_chunks::<3>().0 {
            tick(ctl, k, total)?;
            k += 1;
            out.push(3);
            for &i in t {
                out.extend_from_slice(&(i + base).to_le_bytes());
            }
        }
        base += part.vertices.len() as u32;
    }
    Ok(out)
}

fn stl(m: &Model, ctl: &Ctl) -> Result<Vec<u8>, String> {
    let n = m.triangle_count();
    if n == 0 {
        return Err("STL holds triangles; this model has none".into());
    }
    check(84 + n * 50)?;
    let mut out: Vec<u8> = try_vec(84 + n * 50, "bytes")?;
    let mut header = [0u8; 80];
    let tag = b"anypage-scene binary STL";
    header[..tag.len()].copy_from_slice(tag);
    out.extend_from_slice(&header);
    out.extend_from_slice(&(n as u32).to_le_bytes());
    let mut k = 0usize;
    for part in &m.mesh {
        for t in part.indices.as_chunks::<3>().0 {
            tick(ctl, k, n)?;
            k += 1;
            let v = [part.vertices[t[0] as usize], part.vertices[t[1] as usize], part.vertices[t[2] as usize]];
            let nrm = crate::model::face_normal(v[0].pos, v[1].pos, v[2].pos);
            let l = (nrm[0] * nrm[0] + nrm[1] * nrm[1] + nrm[2] * nrm[2]).sqrt().max(1e-30);
            for c in nrm {
                out.extend_from_slice(&(c / l).to_le_bytes());
            }
            for vx in v {
                for c in world(m, vx.pos) {
                    out.extend_from_slice(&(c as f32).to_le_bytes());
                }
            }
            out.extend_from_slice(&[0, 0]);
        }
    }
    Ok(out)
}

fn obj(m: &Model, ctl: &Ctl) -> Result<Vec<u8>, String> {
    check((m.vertex_count() + m.point_count()) * 60 + m.triangle_count() * 40)?;
    let mut s = String::new();
    let _ = writeln!(s, "# anypage-scene ({})", m.format);
    let col = |c: [u8; 4]| format!(" {:.4} {:.4} {:.4}", c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0);
    let total = 2 * m.vertex_count() + m.triangle_count() + m.point_count();
    let mut k = 0usize;
    for part in &m.mesh {
        for v in &part.vertices {
            tick(ctl, k, total)?;
            k += 1;
            let w = world(m, v.pos);
            let _ = writeln!(s, "v {} {} {}{}", w[0], w[1], w[2], if m.has_colors { col(v.color) } else { String::new() });
        }
    }
    for part in &m.mesh {
        for v in &part.vertices {
            tick(ctl, k, total)?;
            k += 1;
            let _ = writeln!(s, "vn {} {} {}", v.normal[0], v.normal[1], v.normal[2]);
        }
    }
    let mut base = 1u32;
    for part in &m.mesh {
        for t in part.indices.as_chunks::<3>().0 {
            tick(ctl, k, total)?;
            k += 1;
            let (a, b, c) = (t[0] + base, t[1] + base, t[2] + base);
            let _ = writeln!(s, "f {a}//{a} {b}//{b} {c}//{c}");
        }
        base += part.vertices.len() as u32;
    }
    for p in &m.points {
        tick(ctl, k, total)?;
        k += 1;
        let w = world(m, p.pos);
        let _ = writeln!(s, "v {} {} {}{}", w[0], w[1], w[2], if m.has_colors { col(p.color) } else { String::new() });
    }
    Ok(s.into_bytes())
}

fn xyz(m: &Model, ctl: &Ctl) -> Result<Vec<u8>, String> {
    if m.points.is_empty() {
        return Err("XYZ holds points; this model has none".into());
    }
    check(m.point_count() * 64)?;
    let mut s = String::new();
    s.try_reserve(m.point_count() * 40).map_err(|_| "not enough memory for the file".to_string())?;
    for (k, p) in m.points.iter().enumerate() {
        tick(ctl, k, m.point_count())?;
        let w = world(m, p.pos);
        if m.has_colors {
            let _ = writeln!(s, "{} {} {} {} {} {}", w[0], w[1], w[2], p.color[0], p.color[1], p.color[2]);
        } else {
            let _ = writeln!(s, "{} {} {}", w[0], w[1], w[2]);
        }
    }
    Ok(s.into_bytes())
}
