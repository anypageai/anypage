//! STL, ASCII or binary. Triangles are kept as a flat-shaded soup (normals
//! recomputed from the vertices, the file's normal used only for degenerate
//! faces), which is how CAD parts are meant to look. A file whose size is
//! exactly `84 + 50 * count` is read as binary here, even when its 80-byte
//! header starts with `solid` (many CAD exporters write that); anything
//! else goes through `stl_io`'s streaming reader. Counts are checked against
//! the file size before anything is allocated. STL stores `f32`
//! coordinates, so far-away parts carry whatever precision the file has.

use std::io::Cursor;

use super::LoadOptions;
use crate::model::{face_normal, try_vec, Ctl, MeshBuilder, Model, NormalMode, MAX_VERTICES};

/// A triangle soup needs three vertices per triangle.
pub const MAX_STL_TRIANGLES: usize = MAX_VERTICES / 3;

fn f32_at(b: &[u8], at: usize) -> f32 {
    f32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub fn load(bytes: &[u8], opts: &LoadOptions, ctl: &Ctl) -> Result<Model, String> {
    let declared = if bytes.len() >= 84 { u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize } else { 0 };
    let is_binary_size = bytes.len() >= 84 && 84 + declared.saturating_mul(50) == bytes.len();
    // capacity from the data actually present, never from the header alone
    let capacity = if is_binary_size { declared } else { (bytes.len() / 120).max(1) };
    if capacity > MAX_STL_TRIANGLES {
        return Err(format!("{capacity} triangles; the limit is {MAX_STL_TRIANGLES} (3 vertices each, {MAX_VERTICES} vertices per model)"));
    }
    let mut mb = MeshBuilder { positions: try_vec(capacity * 3, "vertices")?, normals: Some(try_vec(capacity * 3, "normals")?), ..Default::default() };
    let normals = mb.normals.as_mut().ok_or("normals")?;
    let mut add = |v: [[f32; 3]; 3], file_normal: [f32; 3]| {
        let mut n = face_normal(v[0], v[1], v[2]);
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len > 0.0 && len.is_finite() {
            n = [n[0] / len, n[1] / len, n[2] / len];
        } else {
            n = file_normal;
        }
        mb.positions.extend_from_slice(&v);
        normals.extend_from_slice(&[n, n, n]);
    };
    if is_binary_size {
        for (k, rec) in bytes[84..].as_chunks::<50>().0.iter().enumerate() {
            if k & 0xF_FFFF == 0 {
                ctl.check()?;
                (ctl.progress)(0.8 * k as f32 / declared.max(1) as f32);
            }
            let v = |i: usize| [f32_at(rec, 12 + 12 * i), f32_at(rec, 16 + 12 * i), f32_at(rec, 20 + 12 * i)];
            add([v(0), v(1), v(2)], [f32_at(rec, 0), f32_at(rec, 4), f32_at(rec, 8)]);
        }
    } else {
        let mut cursor = Cursor::new(bytes);
        let iter = stl_io::create_stl_reader(&mut cursor).map_err(|e| format!("not a valid STL file: {e}"))?;
        for (k, t) in iter.enumerate() {
            if k & 0xF_FFFF == 0 {
                ctl.check()?;
            }
            if k >= MAX_STL_TRIANGLES {
                return Err(format!("more than {MAX_STL_TRIANGLES} triangles"));
            }
            let t = t.map_err(|e| format!("triangle {}: {e}", k + 1))?;
            add(t.vertices.map(|v| v.0), t.normal.0);
        }
    }
    if mb.positions.is_empty() {
        return Err("no triangles".into());
    }
    mb.indices = (0..mb.positions.len() as u32).collect();
    // smooth shading on request: weld identical positions first
    if opts.normals == Some(NormalMode::Smooth) && !opts.flat {
        mb = weld(mb);
        mb.normals = None;
    }
    mb.build("stl", NormalMode::Smooth, opts.flat, ctl).map(|mut m| {
        m.source_normals = false;
        m
    })
}

/// Merge vertices with bit-identical positions (STL repeats them per face).
fn weld(mb: MeshBuilder) -> MeshBuilder {
    use std::collections::HashMap;
    let mut map: HashMap<[u32; 3], u32> = HashMap::with_capacity(mb.positions.len() / 4);
    let mut positions = Vec::with_capacity(mb.positions.len() / 4);
    let indices = mb
        .positions
        .iter()
        .map(|p| {
            let key = p.map(f32::to_bits);
            *map.entry(key).or_insert_with(|| {
                positions.push(*p);
                (positions.len() - 1) as u32
            })
        })
        .collect();
    MeshBuilder { positions, indices, offset: mb.offset, ..Default::default() }
}
