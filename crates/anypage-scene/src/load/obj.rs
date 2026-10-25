//! Wavefront OBJ from bytes via `tobj` (triangulated, single index, parsed
//! as `f64` and narrowed only after subtracting an origin near the data).
//! Materials come only from `LoadOptions::mtl` (a page passes the .mtl
//! text; paths in `mtllib` are never opened): their diffuse colour, or the
//! per-vertex `v x y z r g b` colours, become vertex colours. A file with
//! vertices but no faces is read as a point cloud. `tobj` reads through a
//! [`Watched`] buffer, so parsing reports progress and stops on cancel.

use std::io::{BufRead, BufReader, Read};

use super::{lines, parse_f64, tokens, LoadOptions};
use crate::model::{first_finite, relative, Ctl, MeshBuilder, Model, NormalMode, PointsBuilder, MAX_POINTS};

fn unit_to_u8(x: f64) -> u8 {
    if x.is_finite() {
        (x.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
    } else {
        0
    }
}

/// `tobj` stores `f64` with its `use_f64` feature (enabled here) and `f32`
/// without; this compiles either way.
fn wide<T: Into<f64>>(x: T) -> f64 {
    x.into()
}

/// The source bytes as a `BufRead` that checks for cancellation (and
/// reports progress) every [`Watched::STEP`] bytes.
struct Watched<'a, 'c> {
    data: &'a [u8],
    pos: usize,
    next_check: usize,
    ctl: &'c Ctl<'c>,
}

impl Watched<'_, '_> {
    const STEP: usize = 1 << 20;
}

impl Read for Watched<'_, '_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = {
            let avail = self.fill_buf()?;
            let n = avail.len().min(out.len());
            out[..n].copy_from_slice(&avail[..n]);
            n
        };
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Watched<'_, '_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.pos >= self.next_check {
            if (self.ctl.cancelled)() {
                return Err(std::io::Error::other("cancelled"));
            }
            (self.ctl.progress)(0.5 * self.pos as f32 / self.data.len().max(1) as f32);
            self.next_check = self.pos + Self::STEP;
        }
        let end = self.data.len().min(self.next_check);
        Ok(&self.data[self.pos.min(end)..end])
    }
    fn consume(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.data.len());
    }
}

pub fn load(bytes: &[u8], opts: &LoadOptions, ctl: &Ctl) -> Result<Model, String> {
    let lo = tobj::LoadOptions { single_index: true, triangulate: true, ignore_points: true, ignore_lines: true };
    let mtl = opts.mtl.clone();
    let mut reader = Watched { data: bytes, pos: 0, next_check: 0, ctl };
    let parsed = tobj::load_obj_buf(&mut reader, &lo, |_path| match &mtl {
        Some(m) => tobj::load_mtl_buf(&mut BufReader::new(&m[..])),
        None => Err(tobj::LoadError::OpenFileFailed),
    });
    ctl.check()?;
    let (models, materials) = parsed.map_err(|e| format!("not a valid OBJ file: {e}"))?;
    let materials = materials.unwrap_or_default();
    let mode = opts.normals.unwrap_or(NormalMode::Smooth);
    let mut all = MeshBuilder::default();
    let total = models.len().max(1) as f32;
    let mut all_file_normals = true;
    for (k, m) in models.into_iter().enumerate() {
        ctl.check()?;
        (ctl.progress)(0.5 + 0.4 * k as f32 / total);
        let mesh = m.mesh;
        let n = mesh.positions.len() / 3;
        if mesh.indices.is_empty() || n == 0 {
            continue;
        }
        let world = || mesh.positions.as_chunks::<3>().0.iter().map(|p| [wide(p[0]), wide(p[1]), wide(p[2])]);
        let offset = first_finite(world()).unwrap_or([0.0; 3]);
        let positions: Vec<[f32; 3]> = world().map(|p| relative(p, offset)).collect();
        let normals = (mesh.normals.len() == n * 3).then(|| mesh.normals.as_chunks::<3>().0.iter().map(|v| [wide(v[0]) as f32, wide(v[1]) as f32, wide(v[2]) as f32]).collect());
        let diffuse = mesh.material_id.and_then(|i| materials.get(i)).and_then(|m| m.diffuse);
        let colors = if mesh.vertex_color.len() == n * 3 {
            Some(mesh.vertex_color.as_chunks::<3>().0.iter().map(|c| [unit_to_u8(wide(c[0])), unit_to_u8(wide(c[1])), unit_to_u8(wide(c[2])), 255]).collect())
        } else {
            diffuse.map(|d| vec![[unit_to_u8(wide(d[0])), unit_to_u8(wide(d[1])), unit_to_u8(wide(d[2])), 255]; n])
        };
        all_file_normals &= normals.is_some();
        let mut mb = MeshBuilder { positions, normals, colors, indices: mesh.indices, offset };
        mb.ensure_normals(mode, ctl)?;
        all.append(mb)?;
    }
    if all.indices.is_empty() {
        return points_only(bytes, opts, ctl);
    }
    let mut model = all.build("obj", mode, opts.flat, ctl)?;
    model.source_normals = all_file_normals && !opts.flat;
    Ok(model)
}

/// `v x y z [r g b]` lines as a point cloud.
fn points_only(bytes: &[u8], opts: &LoadOptions, ctl: &Ctl) -> Result<Model, String> {
    let mut pb: Option<PointsBuilder> = None;
    for (k, line) in lines(bytes).enumerate() {
        if k & 0xF_FFFF == 0 {
            ctl.check()?;
        }
        let mut t = tokens(line);
        if t.next() != Some(b"v".as_slice()) {
            continue;
        }
        let v: Vec<f64> = t.take(6).map_while(parse_f64).collect();
        if v.len() < 3 {
            continue;
        }
        if pb.is_none() {
            // origin: the first finite vertex (NaN vertices are skipped)
            pb = Some(PointsBuilder::auto(1024)?);
        }
        let Some(b) = pb.as_mut() else { continue };
        let color = if v.len() == 6 {
            b.has_colors = true;
            [unit_to_u8(v[3]), unit_to_u8(v[4]), unit_to_u8(v[5]), 255]
        } else {
            crate::model::WHITE
        };
        if b.points.len() >= MAX_POINTS {
            return Err(format!("more than {MAX_POINTS} points"));
        }
        b.push([v[0], v[1], v[2]], color)?;
    }
    let pb = pb.ok_or("the file has no faces and no vertices")?;
    if pb.points.is_empty() {
        return Err("no vertex has three finite coordinates".into());
    }
    pb.build("obj", opts.shuffle, opts.max_points, ctl)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn parsing_stops_when_cancelled() {
        let mut s = String::new();
        for i in 0..200_000 {
            s.push_str(&format!("v {i} 0 0\nv {i} 1 0\nv {i} 0 1\nf {} {} {}\n", 3 * i + 1, 3 * i + 2, 3 * i + 3));
        }
        assert!(s.len() > 3 * Watched::STEP);
        // cancel after the first megabyte has been handed out
        let calls = Cell::new(0);
        let cancelled = || {
            calls.set(calls.get() + 1);
            calls.get() > 1
        };
        let ctl = Ctl { cancelled: &cancelled, progress: &|_| {} };
        let e = load(s.as_bytes(), &LoadOptions::default(), &ctl).unwrap_err();
        assert_eq!(e, "cancelled");
        // and reading it all works
        let m = load(s.as_bytes(), &LoadOptions::default(), &Ctl::none()).unwrap();
        assert_eq!(m.triangle_count(), 200_000);
    }

    #[test]
    fn far_coordinates_survive() {
        let m = load(b"v 500000.001 4000000.002 0\nv 500001.001 4000000.002 0\nv 500000.001 4000001.002 0\nf 1 2 3\n", &LoadOptions::default(), &Ctl::none()).unwrap();
        let v = m.mesh[0].vertices[0].pos;
        let x = m.origin[0] + v[0] as f64;
        let y = m.origin[1] + v[1] as f64;
        assert!((x - 500_000.001).abs() < 1e-6 && (y - 4_000_000.002).abs() < 1e-6, "{x} {y}");
    }
}
