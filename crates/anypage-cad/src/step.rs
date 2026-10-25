//! STEP (ISO 10303-21) solids and shells to the shared mesh form through
//! truck (`truck-stepio` 0.3 + `truck-meshalgo` 0.4). Behind the `step`
//! feature; see CORE_REQUESTS.md for what was evaluated.

/// Largest STEP file accepted.
#[cfg(feature = "step")]
pub const MAX_STEP_BYTES: usize = 256 << 20;

/// Triangles `cad.step_to_mesh` (on the page's thread) may produce.
pub const INLINE_MAX_TRIANGLES: usize = 2_000_000;
/// Triangles the job `cad.step_mesh` may produce.
pub const JOB_MAX_TRIANGLES: usize = 20_000_000;

#[cfg(not(feature = "step"))]
pub fn step_to_mesh(_bytes: &[u8], _opts: &anypage_runtime::Data, _max_triangles: usize, _cancelled: &dyn Fn() -> bool) -> Result<anypage_runtime::Data, String> {
    Err("STEP import is not built into this app (anypage-cad feature \"step\")".into())
}

#[cfg(feature = "step")]
pub use imp::step_to_mesh;

#[cfg(feature = "step")]
mod imp {
    use std::collections::HashMap;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use anypage_runtime::Data;
    use truck_meshalgo::prelude::*;
    use truck_stepio::r#in::Table;

    use super::MAX_STEP_BYTES;

    fn f32_bytes(v: &[f32]) -> Data {
        Data::bytes(v.iter().flat_map(|x| x.to_le_bytes()).collect())
    }

    fn u32_bytes(v: &[u32]) -> Data {
        Data::bytes(v.iter().flat_map(|x| x.to_le_bytes()).collect())
    }

    #[derive(Default)]
    struct Out {
        positions: Vec<f32>,
        normals: Vec<f32>,
        indices: Vec<u32>,
    }

    impl Out {
        /// One mesh in: a vertex per distinct (position, normal) pair,
        /// polygons fanned into triangles.
        fn append(&mut self, poly: &PolygonMesh, max_triangles: usize) -> Result<(), String> {
            let pos = poly.positions();
            let nor = poly.normals();
            let mut map: HashMap<(usize, Option<usize>), u32> = HashMap::new();
            for face in poly.faces().face_iter() {
                let mut ids = Vec::with_capacity(face.len());
                for v in face {
                    let key = (v.pos, v.nor);
                    let next = (self.positions.len() / 3) as u32;
                    let id = *map.entry(key).or_insert_with(|| {
                        let p = pos.get(v.pos).unwrap_or(Point3::new(0.0, 0.0, 0.0));
                        self.positions.extend([p.x as f32, p.y as f32, p.z as f32]);
                        let n = v.nor.and_then(|i| nor.get(i)).unwrap_or(Vector3::new(0.0, 0.0, 0.0));
                        self.normals.extend([n.x as f32, n.y as f32, n.z as f32]);
                        next
                    });
                    ids.push(id);
                }
                for k in 1..ids.len().saturating_sub(1) {
                    self.indices.extend([ids[0], ids[k], ids[k + 1]]);
                }
                if self.indices.len() / 3 > max_triangles {
                    return Err(format!("more than {max_triangles} triangles; use a larger tolerance (or the job cad.step_mesh)"));
                }
            }
            Ok(())
        }
    }

    fn mesh_shell(table: &Table, shell: &truck_stepio::r#in::ShellHolder, tolerance: Option<f64>) -> Result<PolygonMesh, String> {
        let cshell = table.to_compressed_shell(shell).map_err(|e| format!("{e:?}"))?;
        let tol = match tolerance {
            Some(t) if t > 0.0 && t.is_finite() => t,
            _ => {
                // a coarse pass to learn the size, then 0.1 % of the diagonal
                let pre = cshell.robust_triangulation(0.01).to_polygon();
                let bb = pre.bounding_box();
                (bb.diameter() * 0.001).max(1e-6)
            }
        };
        let mut poly = cshell.robust_triangulation(tol).to_polygon();
        poly.put_together_same_attrs(TOLERANCE * 50.0).remove_degenerate_faces();
        if poly.normals().is_empty() {
            poly.add_naive_normals(true);
        }
        Ok(poly)
    }

    /// STEP bytes to `{ positions, normals, indices, shells, failed, triangles }`.
    /// `cancelled` is checked between shells: truck's tessellation of one
    /// shell cannot be interrupted.
    pub fn step_to_mesh(bytes: &[u8], opts: &Data, max_triangles: usize, cancelled: &dyn Fn() -> bool) -> Result<Data, String> {
        if bytes.len() > MAX_STEP_BYTES {
            return Err(format!("STEP file larger than {} MB", MAX_STEP_BYTES >> 20));
        }
        let text = String::from_utf8_lossy(bytes);
        if !text.trim_start().starts_with("ISO-10303-21") {
            return Err("not a STEP file (no ISO-10303-21 header)".into());
        }
        let table = catch_unwind(AssertUnwindSafe(|| Table::from_step(&text)))
            .map_err(|_| "the STEP reader failed on this file".to_string())?
            .ok_or("not a readable STEP file (parse error)")?;
        let tolerance = opts.get("tolerance").num();
        let mut shells: Vec<(&u64, &truck_stepio::r#in::ShellHolder)> = table.shell.iter().collect();
        shells.sort_by_key(|(k, _)| **k);
        if shells.is_empty() {
            return Err("the STEP file has no shells (no B-rep solids or surfaces truck can read)".into());
        }
        let mut out = Out::default();
        let mut failed = Vec::new();
        for (id, shell) in &shells {
            if cancelled() {
                return Err("cancelled".into());
            }
            match catch_unwind(AssertUnwindSafe(|| mesh_shell(&table, shell, tolerance))) {
                Ok(Ok(poly)) => out.append(&poly, max_triangles)?,
                Ok(Err(e)) => failed.push(format!("#{id}: {e}")),
                Err(_) => failed.push(format!("#{id}: tessellation failed")),
            }
        }
        if out.indices.is_empty() {
            return Err(format!("no shell could be tessellated: {}", failed.join("; ")));
        }
        let triangles = out.indices.len() / 3;
        Ok(Data::map([
            ("positions", f32_bytes(&out.positions)),
            ("normals", f32_bytes(&out.normals)),
            ("indices", u32_bytes(&out.indices)),
            ("shells", Data::Num(shells.len() as f64)),
            ("failed", Data::list(failed.iter().map(|s| Data::str(s)).collect())),
            ("triangles", Data::Num(triangles as f64)),
        ]))
    }
}
