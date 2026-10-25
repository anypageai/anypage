//! glTF 2.0 (`.glb` and `.gltf` with embedded `data:` buffers) via the `gltf`
//! crate, without its file-system importer: buffers are resolved here, so a
//! page can never make the loader open a path.
//!
//! The default scene is flattened into one mesh: node transforms are baked
//! in, the material's base colour factor and `COLOR_0` become vertex colours
//! (converted from glTF's linear space to sRGB bytes). Triangle strips and
//! fans are converted; `POINTS` primitives become the model's point cloud;
//! line primitives and textures are skipped.

use base64::Engine as _;
use glam::{DMat3, DMat4, DVec3};
use gltf::accessor::{DataType, Dimensions};
use gltf::mesh::{Mode, Semantic};

use super::LoadOptions;
use crate::model::{first_finite, linear_to_srgb8, relative, Ctl, MeshBuilder, Model, NormalMode, PointsBuilder, MAX_POINTS, MAX_VERTICES};

const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 1_000_000;

fn decode_data_uri(uri: &str) -> Result<Vec<u8>, String> {
    let (meta, data) = uri.split_once(',').ok_or("malformed data: URI")?;
    if !meta.ends_with(";base64") {
        return Err("only base64 data: URIs are supported".into());
    }
    base64::engine::general_purpose::STANDARD.decode(data.trim()).map_err(|e| format!("bad base64 in a buffer: {e}"))
}

struct Prim {
    mesh: Option<MeshBuilder>,
    /// The file gave this primitive's normals.
    file_normals: bool,
    points: Vec<([f64; 3], [u8; 4])>,
}

pub fn load(bytes: &[u8], fmt: &str, opts: &LoadOptions, ctl: &Ctl) -> Result<Model, String> {
    let g = gltf::Gltf::from_slice(bytes).map_err(|e| format!("not a valid glTF file: {e}"))?;
    let doc = &g.document;
    // resolve every buffer to bytes; external files are refused
    let mut decoded: Vec<Option<Vec<u8>>> = Vec::new();
    for b in doc.buffers() {
        decoded.push(match b.source() {
            gltf::buffer::Source::Bin => None,
            gltf::buffer::Source::Uri(uri) if uri.starts_with("data:") => Some(decode_data_uri(uri)?),
            gltf::buffer::Source::Uri(uri) => {
                return Err(format!("buffer {} refers to an external file \"{uri}\"; use a .glb or embed buffers as data: URIs", b.index()))
            }
        });
    }
    let mut datas: Vec<&[u8]> = Vec::new();
    for (b, d) in doc.buffers().zip(&decoded) {
        let data: &[u8] = match d {
            Some(v) => v,
            None => g.blob.as_deref().ok_or("the file has no binary chunk for its BIN buffer")?,
        };
        if data.len() < b.length() {
            return Err(format!("buffer {} is {} bytes but declares {}", b.index(), data.len(), b.length()));
        }
        datas.push(data);
    }
    ctl.check()?;

    // nodes of the default scene (or the first), with world transforms
    let mut work: Vec<(gltf::Node, DMat4, usize)> = Vec::new();
    match doc.default_scene().or_else(|| doc.scenes().next()) {
        Some(scene) => work.extend(scene.nodes().map(|n| (n, DMat4::IDENTITY, 0))),
        None => work.extend(doc.nodes().map(|n| (n, DMat4::IDENTITY, MAX_DEPTH))), // no scene: every node, no hierarchy
    }
    let normals_mode = opts.normals.unwrap_or(NormalMode::Flat); // the spec asks for flat normals when absent
    let mut mesh = MeshBuilder::default();
    let mut points: Vec<([f64; 3], [u8; 4])> = Vec::new();
    let mut skipped = 0usize;
    let mut visited = 0usize;
    let mut all_file_normals = true;
    while let Some((node, parent, depth)) = work.pop() {
        visited += 1;
        if visited > MAX_NODES {
            return Err("too many nodes (cyclic node graph?)".into());
        }
        ctl.check()?;
        let local = DMat4::from_cols_array_2d(&node.transform().matrix().map(|c| c.map(|x| x as f64)));
        let world = parent * local;
        if let Some(m) = node.mesh() {
            for prim in m.primitives() {
                match primitive(&prim, &world, &datas, normals_mode, ctl)? {
                    Some(p) => {
                        if let Some(pm) = p.mesh {
                            all_file_normals &= p.file_normals;
                            mesh.append(pm)?;
                        }
                        if points.len() + p.points.len() > MAX_POINTS {
                            return Err(format!("more than {MAX_POINTS} points"));
                        }
                        points.extend(p.points);
                    }
                    None => skipped += 1,
                }
            }
        }
        if depth < MAX_DEPTH {
            work.extend(node.children().map(|c| (c, world, depth + 1)));
        }
    }
    if mesh.indices.is_empty() && points.is_empty() {
        return Err(if skipped > 0 { "only line primitives found; nothing to show as a mesh or points".into() } else { "the scene has no meshes".into() });
    }
    let mut model = if mesh.indices.is_empty() {
        Model::default()
    } else {
        let mut m = mesh.build(fmt, normals_mode, opts.flat, ctl)?;
        m.source_normals = all_file_normals && !opts.flat;
        m
    };
    if !points.is_empty() {
        // a mesh's origin, or else the first finite point (NaN points are skipped)
        let origin = if model.mesh.is_empty() { [f64::NAN; 3] } else { model.origin };
        let mut pb = PointsBuilder::new(origin, points.len())?;
        pb.has_colors = true;
        for (p, c) in points {
            pb.push(p, c)?;
        }
        let pm = pb.build(fmt, opts.shuffle, opts.max_points, ctl)?;
        if model.mesh.is_empty() {
            model = pm;
        } else {
            model.local_bounds = model.local_bounds.union(&pm.local_bounds);
            model.source_points = pm.source_points;
            model.shuffled = pm.shuffled;
            model.points = pm.points;
        }
    }
    model.format = fmt.to_string();
    Ok(model)
}

fn check_f32(acc: &gltf::Accessor, what: &str, dims: Dimensions) -> Result<(), String> {
    if acc.data_type() != DataType::F32 || acc.dimensions() != dims {
        return Err(format!("{what} must be float {dims:?} (quantized attributes are not supported)"));
    }
    Ok(())
}

fn primitive(prim: &gltf::Primitive, world: &DMat4, datas: &[&[u8]], normals_mode: NormalMode, ctl: &Ctl) -> Result<Option<Prim>, String> {
    let mode = prim.mode();
    if matches!(mode, Mode::Lines | Mode::LineLoop | Mode::LineStrip) {
        return Ok(None);
    }
    let Some(pos_acc) = prim.get(&Semantic::Positions) else { return Ok(None) };
    check_f32(&pos_acc, "POSITION", Dimensions::Vec3)?;
    if pos_acc.count() > MAX_VERTICES {
        return Err(format!("a primitive has {} vertices; the limit is {MAX_VERTICES}", pos_acc.count()));
    }
    let reader = prim.reader(|b| datas.get(b.index()).copied());
    // world positions in f64 (node translations may be georeferenced), kept
    // relative to the first finite one so narrowing to f32 loses nothing
    let world_positions: Vec<[f64; 3]> = reader
        .read_positions()
        .ok_or("POSITION data is missing or out of the buffer's range")?
        .map(|p| world.transform_point3(DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64)).to_array())
        .collect();
    let n = world_positions.len();
    // colours: COLOR_0 times the base colour factor, both linear
    let factor = prim.material().pbr_metallic_roughness().base_color_factor();
    let vertex_colors: Option<Vec<[f32; 4]>> = reader.read_colors(0).map(|c| c.into_rgba_f32().collect());
    let colors: Option<Vec<[u8; 4]>> = match vertex_colors {
        Some(vc) if vc.len() == n => Some(
            vc.iter()
                .map(|c| {
                    [
                        linear_to_srgb8(c[0] * factor[0]),
                        linear_to_srgb8(c[1] * factor[1]),
                        linear_to_srgb8(c[2] * factor[2]),
                        ((c[3] * factor[3]).clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
                    ]
                })
                .collect(),
        ),
        _ if factor != [1.0, 1.0, 1.0, 1.0] => {
            let c = [linear_to_srgb8(factor[0]), linear_to_srgb8(factor[1]), linear_to_srgb8(factor[2]), (factor[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8];
            Some(vec![c; n])
        }
        _ => None,
    };
    ctl.check()?;
    if mode == Mode::Points {
        let c = colors.unwrap_or_else(|| vec![crate::model::WHITE; n]);
        let points = world_positions.into_iter().zip(c).collect();
        return Ok(Some(Prim { mesh: None, file_normals: false, points }));
    }
    let offset = first_finite(world_positions.iter().copied()).unwrap_or([0.0; 3]);
    let positions: Vec<[f32; 3]> = world_positions.iter().map(|p| relative(*p, offset)).collect();
    drop(world_positions);
    let normals: Option<Vec<[f32; 3]>> = match prim.get(&Semantic::Normals) {
        Some(acc) => {
            check_f32(&acc, "NORMAL", Dimensions::Vec3)?;
            let nm = DMat3::from_mat4(*world).inverse().transpose();
            let nm = if nm.is_finite() { nm } else { DMat3::IDENTITY };
            reader.read_normals().map(|it| {
                it.map(|v| {
                    let w = (nm * DVec3::new(v[0] as f64, v[1] as f64, v[2] as f64)).normalize_or_zero();
                    [w.x as f32, w.y as f32, w.z as f32]
                })
                .collect::<Vec<_>>()
            })
        }
        None => None,
    }
    .filter(|v| v.len() == n);
    let raw: Vec<u32> = match reader.read_indices() {
        Some(i) => i.into_u32().collect(),
        None => (0..n as u32).collect(),
    };
    let indices = match mode {
        Mode::Triangles => raw,
        Mode::TriangleStrip => (2..raw.len())
            .flat_map(|k| if k % 2 == 0 { [raw[k - 2], raw[k - 1], raw[k]] } else { [raw[k - 1], raw[k - 2], raw[k]] })
            .collect(),
        Mode::TriangleFan => (2..raw.len()).flat_map(|k| [raw[0], raw[k - 1], raw[k]]).collect(),
        _ => return Ok(None),
    };
    let mut indices = indices;
    indices.truncate(indices.len() / 3 * 3);
    let file_normals = normals.is_some();
    let mut mb = MeshBuilder { positions, normals, colors, indices, offset };
    mb.ensure_normals(normals_mode, ctl)?;
    Ok(Some(Prim { mesh: Some(mb), file_normals, points: Vec::new() }))
}
