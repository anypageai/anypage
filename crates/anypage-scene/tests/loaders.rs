//! Loaders on files generated here: glTF/GLB, OBJ (+MTL), STL ascii and
//! binary, PLY ascii / little / big endian, XYZ, LAS and LAZ; malformed
//! input, limits, cancellation, and save -> load round trips.

use std::io::Cursor;
use std::sync::Arc;

use anypage_scene::load::{load, Bytes, LoadOptions};
use anypage_scene::model::{Bounds, Ctl, Model};
use base64::Engine as _;

fn bytes(b: &[u8]) -> Bytes {
    Arc::new(b.to_vec())
}

fn ld(b: &[u8], format: &str) -> Result<Model, String> {
    load(&bytes(b), format, None, &LoadOptions::default(), &Ctl::none())
}

fn approx(b: Bounds, min: [f64; 3], max: [f64; 3]) -> bool {
    (0..3).all(|k| (b.min[k] - min[k]).abs() < 1e-4 && (b.max[k] - max[k]).abs() < 1e-4)
}

// ---------------------------------------------------------------- glTF

fn le<T: Copy>(v: &[T], f: impl Fn(T) -> Vec<u8>) -> Vec<u8> {
    v.iter().flat_map(|x| f(*x)).collect()
}

/// One triangle (positions + u16 indices) as a buffer of 44 bytes.
fn tri_buffer() -> Vec<u8> {
    let mut b = le(&[0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0], |x| x.to_le_bytes().to_vec());
    b.extend(le(&[0u16, 1, 2], |x| x.to_le_bytes().to_vec()));
    b.extend([0, 0]);
    b
}

fn gltf_json(buffer: &str, mode: u32) -> String {
    format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],
        "nodes":[{{"mesh":0,"translation":[10,0,0],"children":[1]}},{{"mesh":0,"scale":[2,2,2]}}],
        "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1,"material":0,"mode":{mode}}}]}}],
        "materials":[{{"pbrMetallicRoughness":{{"baseColorFactor":[1,0,0,1]}}}}],
        "buffers":[{buffer}],
        "bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}},{{"buffer":0,"byteOffset":36,"byteLength":6}}],
        "accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},
                     {{"bufferView":1,"componentType":5123,"count":3,"type":"SCALAR"}}]}}"#
    )
}

fn glb(json: &str, bin: &[u8]) -> Vec<u8> {
    let mut j = json.as_bytes().to_vec();
    while !j.len().is_multiple_of(4) {
        j.push(b' ');
    }
    let mut b = bin.to_vec();
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
    let total = 12 + 8 + j.len() + 8 + b.len();
    let mut out = Vec::new();
    out.extend(b"glTF");
    out.extend(2u32.to_le_bytes());
    out.extend((total as u32).to_le_bytes());
    out.extend((j.len() as u32).to_le_bytes());
    out.extend(b"JSON");
    out.extend(j);
    out.extend((b.len() as u32).to_le_bytes());
    out.extend(b"BIN\0");
    out.extend(b);
    out
}

#[test]
fn glb_nodes_materials_and_flat_normals() {
    let file = glb(&gltf_json(r#"{"byteLength":44}"#, 4), &tri_buffer());
    let m = ld(&file, "auto").unwrap();
    assert_eq!(m.format, "glb");
    assert_eq!(m.triangle_count(), 2, "the mesh is instanced by two nodes");
    // node 0 translates by 10, its child scales by 2
    assert!(approx(m.bounds(), [10.0, 0.0, 0.0], [12.0, 2.0, 0.0]), "{:?}", m.bounds());
    assert!(m.has_colors);
    let v = m.mesh[0].vertices[0];
    assert_eq!(v.color, [255, 0, 0, 255]);
    assert!((v.normal[2] - 1.0).abs() < 1e-6, "flat normal {:?}", v.normal);
    assert!(!m.source_normals);
}

#[test]
fn gltf_with_embedded_buffer_strips_and_points() {
    let uri = format!("data:application/octet-stream;base64,{}", base64::engine::general_purpose::STANDARD.encode(tri_buffer()));
    let json = gltf_json(&format!(r#"{{"byteLength":44,"uri":"{uri}"}}"#), 4);
    let m = ld(json.as_bytes(), "auto").unwrap();
    assert_eq!((m.format.as_str(), m.triangle_count()), ("gltf", 2));
    // a triangle strip of 3 indices is one triangle per node
    let m = ld(gltf_json(&format!(r#"{{"byteLength":44,"uri":"{uri}"}}"#), 5).as_bytes(), "gltf").unwrap();
    assert_eq!(m.triangle_count(), 2);
    // POINTS primitives become the point cloud
    let m = ld(gltf_json(&format!(r#"{{"byteLength":44,"uri":"{uri}"}}"#), 0).as_bytes(), "gltf").unwrap();
    assert_eq!((m.kind(), m.point_count()), ("points", 6));
    // external files are refused, never opened
    let e = ld(gltf_json(r#"{"byteLength":44,"uri":"secret.bin"}"#, 4).as_bytes(), "gltf").unwrap_err();
    assert!(e.contains("external file"), "{e}");
}

#[test]
fn malformed_gltf_is_an_error() {
    let file = glb(&gltf_json(r#"{"byteLength":44}"#, 4), &tri_buffer());
    for cut in [10, 30, file.len() - 20] {
        let e = ld(&file[..cut], "glb").unwrap_err();
        assert!(e.starts_with("glb:"), "{e}");
    }
    // a buffer that claims more bytes than the file has
    let file = glb(&gltf_json(r#"{"byteLength":4400}"#, 4), &tri_buffer());
    assert!(ld(&file, "glb").is_err());
}

// ---------------------------------------------------------------- OBJ

const CUBE_OBJ: &str = "mtllib cube.mtl\nusemtl red\n\
v -1 -1 -1\nv 1 -1 -1\nv 1 1 -1\nv -1 1 -1\nv -1 -1 1\nv 1 -1 1\nv 1 1 1\nv -1 1 1\n\
vn 0 0 -1\nvn 0 0 1\nvn 0 -1 0\nvn 0 1 0\nvn -1 0 0\nvn 1 0 0\n\
f 1//1 4//1 3//1 2//1\nf 5//2 6//2 7//2 8//2\nf 1//3 2//3 6//3 5//3\nf 4//4 8//4 7//4 3//4\nf 1//5 5//5 8//5 4//5\nf 2//6 3//6 7//6 6//6\n";

#[test]
fn obj_quads_normals_and_materials() {
    let m = ld(CUBE_OBJ.as_bytes(), "auto").unwrap();
    assert_eq!(m.format, "obj");
    assert_eq!(m.triangle_count(), 12);
    assert!(m.source_normals);
    assert!(approx(m.bounds(), [-1.0; 3], [1.0; 3]));
    assert!(!m.has_colors, "no .mtl given");
    let opts = LoadOptions { mtl: Some(b"newmtl red\nKd 1 0 0\n".to_vec()), ..Default::default() };
    let m = load(&bytes(CUBE_OBJ.as_bytes()), "obj", None, &opts, &Ctl::none()).unwrap();
    assert!(m.has_colors);
    assert!(m.mesh[0].vertices.iter().all(|v| v.color == [255, 0, 0, 255]));
    // vertex colours and computed (smooth) normals
    let m = ld(b"v 0 0 0 1 0 0\nv 1 0 0 0 1 0\nv 0 1 0 0 0 1\nf 1 2 3\n", "obj").unwrap();
    assert_eq!(m.mesh[0].vertices[1].color, [0, 255, 0, 255]);
    assert!((m.mesh[0].vertices[0].normal[2] - 1.0).abs() < 1e-6);
    assert!(!m.source_normals);
    // vertices only: a point cloud
    let m = ld(b"v 0 0 0\nv 1 2 3\nv 4 5 6\n", "obj").unwrap();
    assert_eq!((m.kind(), m.point_count()), ("points", 3));
    assert!(ld(b"f 1 2 3\n", "obj").is_err());
}

// ---------------------------------------------------------------- STL

fn stl_binary(tris: &[[[f32; 3]; 3]], declared: u32) -> Vec<u8> {
    let mut out = vec![0u8; 80];
    out.extend(declared.to_le_bytes());
    for t in tris {
        out.extend([0u8; 12]); // a zero normal: recomputed
        for v in t {
            for c in v {
                out.extend(c.to_le_bytes());
            }
        }
        out.extend([0, 0]);
    }
    out
}

#[test]
fn stl_ascii_and_binary() {
    let ascii = "solid t\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 2 0 0\nvertex 0 3 0\nendloop\nendfacet\n\
                 facet normal 0 0 1\nouter loop\nvertex 0 0 1\nvertex 2 0 1\nvertex 0 3 1\nendloop\nendfacet\nendsolid t\n";
    let m = ld(ascii.as_bytes(), "auto").unwrap();
    assert_eq!((m.triangle_count(), m.vertex_count()), (2, 6));
    assert!(approx(m.bounds(), [0.0; 3], [2.0, 3.0, 1.0]));
    let tris = [[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]], [[0.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]];
    let m = ld(&stl_binary(&tris, 2), "auto").unwrap();
    assert_eq!(m.triangle_count(), 2);
    // flat normals from the geometry
    assert!((m.mesh[0].vertices[0].normal[2] - 1.0).abs() < 1e-6);
    assert!((m.mesh[0].vertices[3].normal[0] - 1.0).abs() < 1e-6);
    // smooth on request: shared corners are welded
    let opts = LoadOptions { normals: Some(anypage_scene::NormalMode::Smooth), ..Default::default() };
    let m = load(&bytes(&stl_binary(&tris, 2)), "stl", None, &opts, &Ctl::none()).unwrap();
    assert_eq!(m.vertex_count(), 4);
    // truncated: the header promises 10 triangles
    let e = ld(&stl_binary(&tris, 10), "stl").unwrap_err();
    assert!(e.contains("triangle 3"), "{e}");
    // a header that promises 4 billion triangles allocates nothing
    assert!(ld(&stl_binary(&tris, u32::MAX), "stl").is_err());
}

// ---------------------------------------------------------------- PLY

#[test]
fn ply_ascii_mesh_with_colours_and_extra_elements() {
    let text = "ply\nformat ascii 1.0\ncomment made by hand\nelement vertex 4\nproperty float x\nproperty float y\nproperty float z\n\
                property uchar red\nproperty uchar green\nproperty uchar blue\nelement face 2\nproperty list uchar int vertex_indices\n\
                element edge 1\nproperty int vertex1\nproperty int vertex2\nend_header\n\
                0 0 0 255 0 0\n1 0 0 0 255 0\n1 1 0 0 0 255\n0 1 0 255 255 255\n4 0 1 2 3\n3 0 1 2\n0 1\n";
    let m = ld(text.as_bytes(), "auto").unwrap();
    assert_eq!((m.format.as_str(), m.triangle_count(), m.vertex_count()), ("ply", 3, 4));
    assert!(m.has_colors);
    assert_eq!(m.mesh[0].vertices[1].color, [0, 255, 0, 255]);
    let e = ld(text.replace("3 0 1 2\n", "3 0 1 9\n").as_bytes(), "ply").unwrap_err();
    assert!(e.contains("vertex 9"), "{e}");
}

#[test]
fn ply_binary_points_and_big_endian_mesh() {
    // little endian points with float colours and an unused property
    let mut b = b"ply\nformat binary_little_endian 1.0\nelement vertex 3\nproperty double x\nproperty double y\nproperty double z\n\
property float intensity\nproperty float red\nproperty float green\nproperty float blue\nend_header\n"
        .to_vec();
    for (p, c) in [([500000.5f64, 10.0, 1.0], [1.0f32, 0.0, 0.0]), ([500001.5, 11.0, 2.0], [0.0, 1.0, 0.0]), ([500002.5, 12.0, 3.0], [0.0, 0.0, 1.0])] {
        for v in p {
            b.extend(v.to_le_bytes());
        }
        b.extend(7f32.to_le_bytes());
        for v in c {
            b.extend(v.to_le_bytes());
        }
    }
    let m = ld(&b, "auto").unwrap();
    assert_eq!((m.kind(), m.point_count()), ("points", 3));
    assert!(approx(m.bounds(), [500000.5, 10.0, 1.0], [500002.5, 12.0, 3.0]), "{:?}", m.bounds());
    let mut colours: Vec<[u8; 4]> = m.points.iter().map(|p| p.color).collect();
    colours.sort();
    assert_eq!(colours, vec![[0, 0, 255, 255], [0, 255, 0, 255], [255, 0, 0, 255]]);
    // big endian mesh
    let mut b = b"ply\nformat binary_big_endian 1.0\nelement vertex 3\nproperty float x\nproperty float y\nproperty float z\n\
element face 1\nproperty list uchar uint vertex_indices\nend_header\n"
        .to_vec();
    for v in [0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
        b.extend(v.to_be_bytes());
    }
    b.push(3);
    for i in [0u32, 1, 2] {
        b.extend(i.to_be_bytes());
    }
    let m = ld(&b, "ply").unwrap();
    assert_eq!(m.triangle_count(), 1);
    assert!(approx(m.bounds(), [0.0; 3], [1.0, 1.0, 0.0]));
    // a header that declares a billion vertices over a few bytes
    let bomb = b"ply\nformat binary_little_endian 1.0\nelement vertex 1000000000\nproperty float x\nproperty float y\nproperty float z\nend_header\n\0\0\0\0";
    let e = ld(bomb, "ply").unwrap_err();
    assert!(e.contains("at most"), "{e}");
}

// ---------------------------------------------------------------- XYZ, LAS, LAZ

#[test]
fn xyz_and_cancellation() {
    let m = ld(b"# x y z r g b\n0 0 0 255 0 0\n1 1 1 0 255 0\n", "xyz").unwrap();
    assert_eq!((m.point_count(), m.has_colors, m.shuffled), (2, true, true));
    let mut big = String::new();
    for i in 0..300_000 {
        big.push_str(&format!("{i} 1 2\n"));
    }
    let ctl = Ctl { cancelled: &|| true, progress: &|_| {} };
    let e = load(&bytes(big.as_bytes()), "xyz", None, &LoadOptions::default(), &ctl).unwrap_err();
    assert_eq!(e, "cancelled");
    // max_points keeps a uniform sample
    let opts = LoadOptions { max_points: Some(1000), ..Default::default() };
    let m = load(&bytes(big.as_bytes()), "xyz", None, &opts, &Ctl::none()).unwrap();
    assert_eq!((m.point_count(), m.source_points), (1000, 300_000));
    let above = m.points.iter().filter(|p| m.origin[0] + p.pos[0] as f64 >= 150_000.0).count();
    assert!((350..650).contains(&above), "{above} of 1000 in the upper half");
}

fn las_file(compressed: bool, n: usize) -> Vec<u8> {
    let mut b = las::Builder::from((1, 2));
    b.point_format = las::point::Format::new(2).unwrap();
    b.point_format.is_compressed = compressed;
    b.transforms = las::Vector {
        x: las::Transform { scale: 0.001, offset: 500_000.0 },
        y: las::Transform { scale: 0.001, offset: 4_000_000.0 },
        z: las::Transform { scale: 0.001, offset: 0.0 },
    };
    let header = b.into_header().unwrap();
    let mut w = las::Writer::new(Cursor::new(Vec::new()), header).unwrap();
    for i in 0..n {
        let f = i as f64;
        w.write_point(las::Point {
            x: 500_000.0 + f * 0.01,
            y: 4_000_000.0 + (f * 0.37).sin(),
            z: 100.0 + f * 0.001,
            color: Some(las::Color::new(((i % 256) * 257) as u16, 0, 65535)),
            ..Default::default()
        })
        .unwrap();
    }
    w.into_inner().unwrap().into_inner()
}

#[test]
fn las_and_laz() {
    for compressed in [false, true] {
        let file = las_file(compressed, 5000);
        let m = ld(&file, "auto").unwrap();
        assert_eq!(m.format, if compressed { "laz" } else { "las" });
        assert_eq!(m.point_count(), 5000);
        assert!(m.has_colors && m.shuffled);
        let b = m.bounds();
        assert!((b.min[0] - 500_000.0).abs() < 1e-3 && (b.max[0] - 500_049.99).abs() < 1e-3, "{b:?}");
        assert!((b.min[2] - 100.0).abs() < 1e-3 && (b.max[2] - 104.999).abs() < 1e-3, "{b:?}");
        // millimetres survive although the coordinates are in the millions
        let mut xs: Vec<f64> = m.points.iter().map(|p| m.origin[0] + p.pos[0] as f64).collect();
        xs.sort_by(f64::total_cmp);
        assert!((xs[1] - xs[0] - 0.01).abs() < 5e-4, "{} {}", xs[0], xs[1]);
        // 16-bit colours scaled to bytes
        assert!(m.points.iter().all(|p| p.color[2] == 255 && p.color[1] == 0));
    }
    let mut file = las_file(false, 100);
    file.truncate(file.len() - 200);
    assert!(ld(&file, "las").is_err());
}

// ---------------------------------------------------------------- crafted LAS / LAZ headers

fn put_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}
fn get_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// A LAS 1.4 file in point format 7 (xyz, gps time, rgb): LAZ uses the
/// layered (v3) compressor for it.
fn las14_file(compressed: bool, n: usize) -> Vec<u8> {
    let mut b = las::Builder::from((1, 4));
    b.point_format = las::point::Format::new(7).unwrap();
    b.point_format.is_compressed = compressed;
    b.transforms = las::Vector {
        x: las::Transform { scale: 0.001, offset: 0.0 },
        y: las::Transform { scale: 0.001, offset: 0.0 },
        z: las::Transform { scale: 0.001, offset: 0.0 },
    };
    let header = b.into_header().unwrap();
    let mut w = las::Writer::new(Cursor::new(Vec::new()), header).unwrap();
    for i in 0..n {
        let f = i as f64;
        w.write_point(las::Point {
            x: (f * 0.37).sin() * 100.0,
            y: (f * 0.11).cos() * 100.0,
            z: f * 0.0001,
            gps_time: Some(f),
            color: Some(las::Color::new((i % 65536) as u16, 0, 65535)),
            ..Default::default()
        })
        .unwrap();
    }
    w.into_inner().unwrap().into_inner()
}

/// Loads that must fail fast with an error (and never abort the process).
fn refused(file: &[u8], want: &str) {
    let t0 = std::time::Instant::now();
    let e = ld(file, "auto").unwrap_err();
    assert!(e.contains(want), "{e}");
    assert!(t0.elapsed().as_secs_f64() < 2.0, "took {:?}", t0.elapsed());
}

#[test]
fn crafted_laz_headers_are_refused_before_allocating() {
    let good = las_file(true, 10);
    assert_eq!(ld(&good, "auto").unwrap().point_count(), 10);
    // the fuzzer's case: 26368 extra bytes per point and 4.3 M points in a
    // 1 kB file once asked `las` for a 27.7 GB buffer (the process aborted)
    let mut f = good.clone();
    put_u16(&mut f, 105, 26394);
    put_u32(&mut f, 107, 4_287_385);
    refused(&f, "LAZ point records of 26394 bytes");
    // only the count: more points than chunks of this file can hold
    let mut f = good.clone();
    put_u32(&mut f, 107, 4_287_385);
    refused(&f, "holds at most");
    // a count that passes that bound decodes garbage: an error or junk, quickly, in 32 MB steps
    let mut f = good.clone();
    put_u32(&mut f, 107, 300_000);
    let t0 = std::time::Instant::now();
    let _ = ld(&f, "auto");
    assert!(t0.elapsed().as_secs_f64() < 5.0, "took {:?}", t0.elapsed());
    // a chunk table that declares 4 billion entries (64 GB reserved by `laz`)
    let data_start = get_u32(&good, 96) as usize;
    let table = i64::from_le_bytes(good[data_start..data_start + 8].try_into().unwrap()) as usize;
    let mut f = good.clone();
    put_u32(&mut f, table + 4, u32::MAX);
    refused(&f, "chunk table declares");
}

#[test]
fn layered_laz_chunks_are_checked() {
    // three 50 000-point chunks of LAS 1.4 point format 7
    let file = las14_file(true, 120_000);
    let m = ld(&file, "auto").unwrap();
    assert_eq!((m.format.as_str(), m.point_count()), ("laz", 120_000));
    assert!(m.has_colors);
    let plain = ld(&las14_file(false, 1000), "auto").unwrap();
    assert_eq!((plain.format.as_str(), plain.point_count()), ("las", 1000));
    // the first layer of chunk 0 claims 4 GB: `laz` would allocate that much
    // (raw first point: Point14 30 bytes + RGB14 6, then the point count)
    let data_start = get_u32(&file, 96) as usize;
    let mut f = file.clone();
    put_u32(&mut f, data_start + 8 + 36 + 4, 0xFFFF_FF00);
    refused(&f, "LAZ chunk 0 declares");
    // the same in the last chunk
    let mut pos = data_start + 8;
    for _ in 0..2 {
        let sum: u64 = (0..10).map(|i| get_u32(&file, pos + 36 + 4 + 4 * i) as u64).sum();
        pos += 36 + 4 + 40 + sum as usize;
    }
    let mut f = file.clone();
    put_u32(&mut f, pos + 36 + 4 + 4 * 9, 0x7FFF_FFFF);
    refused(&f, "LAZ chunk 2 declares");
}

#[test]
fn a_huge_extended_vlr_is_refused() {
    // LAS 1.4 reads its first EVLR with a u64 length; 1 TB would abort
    let mut f = las14_file(false, 100);
    let start = f.len() as u64;
    let mut evlr = vec![0u8; 60];
    evlr[20..28].copy_from_slice(&(1u64 << 40).to_le_bytes());
    f.extend(evlr);
    put_u64(&mut f, 235, start);
    put_u32(&mut f, 243, 1);
    refused(&f, "extended VLR declares");
}

// ---------------------------------------------------------------- NaN first points, far meshes, STL headers

#[test]
fn a_nan_first_point_keeps_the_cloud() {
    let check = |m: Model, what: &str| {
        assert_eq!(m.point_count(), 2, "{what}");
        assert!(m.origin.iter().all(|x| x.is_finite()), "{what}: origin {:?}", m.origin);
        assert!(approx(m.bounds(), [1.0, 2.0, 3.0], [4.0, 5.0, 6.0]), "{what}: {:?}", m.bounds());
        assert!(m.points.iter().all(|p| p.pos.iter().all(|x| x.is_finite())), "{what}");
    };
    check(ld(b"nan nan nan\n1 2 3\n4 5 6\n", "xyz").unwrap(), "xyz");
    check(ld(b"v nan nan nan\nv 1 2 3\nv 4 5 6\n", "obj").unwrap(), "obj");
    let mut ply = b"ply\nformat binary_little_endian 1.0\nelement vertex 3\nproperty float x\nproperty float y\nproperty float z\nend_header\n".to_vec();
    for v in [f32::NAN, f32::NAN, f32::NAN, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0] {
        ply.extend(v.to_le_bytes());
    }
    check(ld(&ply, "ply").unwrap(), "ply");
    // glTF POINTS: no transforms on this node, NaN first position
    let mut buf = le(&[f32::NAN, f32::NAN, f32::NAN, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0], |x| x.to_le_bytes().to_vec());
    buf.extend([0u8; 8]);
    let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],"nodes":[{"mesh":0}],
        "meshes":[{"primitives":[{"attributes":{"POSITION":0},"mode":0}]}],
        "buffers":[{"byteLength":44}],"bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36}],
        "accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[1,2,3],"max":[4,5,6]}]}"#;
    check(ld(&glb(json, &buf), "glb").unwrap(), "glb");
    // only NaN: a clear error, not an empty cloud
    assert!(ld(b"nan nan nan\n", "xyz").unwrap_err().contains("finite"));
}

/// World x coordinates of every mesh vertex.
fn xs(m: &Model) -> Vec<f64> {
    m.mesh.iter().flat_map(|p| p.vertices.iter()).map(|v| m.origin[0] + v.pos[0] as f64).collect()
}

#[test]
fn far_meshes_keep_millimetres() {
    // f32 steps at 500 000 are 1/32 m: 500000.001 only survives an f64 recentre
    let want = 500_000.001;
    let near = |m: &Model| xs(m).iter().any(|x| (x - want).abs() < 1e-6);
    // glTF: a georeferenced node with a millimetre-scale child
    let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],
        "nodes":[{"translation":[500000,4000000,0],"children":[1]},{"mesh":0,"scale":[0.001,0.001,0.001]}],
        "meshes":[{"primitives":[{"attributes":{"POSITION":0},"indices":1}]}],
        "buffers":[{"byteLength":44}],
        "bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":6}],
        "accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]},
                     {"bufferView":1,"componentType":5123,"count":3,"type":"SCALAR"}]}"#;
    let m = ld(&glb(json, &tri_buffer()), "glb").unwrap();
    assert!(near(&m), "glb: {:?}", xs(&m));
    // PLY with double coordinates
    let mut ply = b"ply\nformat binary_little_endian 1.0\nelement vertex 3\nproperty double x\nproperty double y\nproperty double z\n\
element face 1\nproperty list uchar uint vertex_indices\nend_header\n"
        .to_vec();
    for v in [500_000.0, 4_000_000.0, 0.0, want, 4_000_000.0, 0.0, 500_000.0, 4_000_000.001, 0.0f64] {
        ply.extend(v.to_le_bytes());
    }
    ply.push(3);
    for i in [0u32, 1, 2] {
        ply.extend(i.to_le_bytes());
    }
    let m = ld(&ply, "ply").unwrap();
    assert!(near(&m), "ply: {:?}", xs(&m));
    // OBJ (parsed as f64)
    let m = ld(b"v 500000 4000000 0\nv 500000.001 4000000 0\nv 500000 4000000.001 0\nf 1 2 3\n", "obj").unwrap();
    assert!(near(&m), "obj: {:?}", xs(&m));
}

#[test]
fn binary_stl_with_a_solid_header() {
    // CAD exporters write "solid <name>" into the binary header; with a
    // "facet" line after it, stl_io took the file for ASCII and failed
    let tris = [[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]], [[0.0, 0.0, 1.0], [1.0, 0.0, 1.0], [0.0, 1.0, 1.0]]];
    let mut f = stl_binary(&tris, 2);
    let header = b"solid part\nfacet normal 0 0 1\n";
    f[..header.len()].copy_from_slice(header);
    for fmt in ["auto", "stl"] {
        let m = ld(&f, fmt).unwrap_or_else(|e| panic!("{fmt}: {e}"));
        assert_eq!(m.triangle_count(), 2);
        assert!(approx(m.bounds(), [0.0; 3], [1.0, 1.0, 1.0]));
    }
}

// ---------------------------------------------------------------- round trips

#[test]
fn save_and_load_again() {
    let mesh = ld(CUBE_OBJ.as_bytes(), "obj").unwrap();
    for fmt in ["ply", "stl", "obj"] {
        let bytes = anypage_scene::export::save(&mesh, fmt).unwrap();
        let back = ld(&bytes, "auto").unwrap_or_else(|e| panic!("{fmt}: {e}"));
        assert_eq!(back.triangle_count(), 12, "{fmt}");
        assert!(approx(back.bounds(), [-1.0; 3], [1.0; 3]), "{fmt}: {:?}", back.bounds());
    }
    let cloud = ld(&las_file(false, 1000), "las").unwrap();
    for fmt in ["ply", "xyz"] {
        let bytes = anypage_scene::export::save(&cloud, fmt).unwrap();
        let back = ld(&bytes, fmt).unwrap();
        assert_eq!(back.point_count(), 1000, "{fmt}");
        let (a, b) = (cloud.bounds(), back.bounds());
        assert!((0..3).all(|k| (a.min[k] - b.min[k]).abs() < 1e-3 && (a.max[k] - b.max[k]).abs() < 1e-3), "{fmt}: {a:?} vs {b:?}");
        assert!(back.has_colors);
    }
    assert!(anypage_scene::export::save(&cloud, "stl").is_err());
    assert!(anypage_scene::export::save(&mesh, "xyz").is_err());
}
