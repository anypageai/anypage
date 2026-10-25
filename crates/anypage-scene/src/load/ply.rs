//! PLY (Stanford polygon file): ascii, binary_little_endian and
//! binary_big_endian. Vertices with optional normals and colours; faces as
//! polygons (fan-triangulated). A file without faces is a point cloud. Other
//! elements (edges, materials, ...) are skipped. Declared counts are checked
//! against the bytes present before anything is allocated.

use super::LoadOptions;
use crate::model::{first_finite, relative, try_vec, Ctl, MeshBuilder, Model, NormalMode, PointsBuilder, MAX_POINTS, MAX_TRIANGLES, MAX_VERTICES, WHITE};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Encoding {
    Ascii,
    Le,
    Be,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Ty {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl Ty {
    fn parse(s: &str) -> Option<Ty> {
        Some(match s {
            "char" | "int8" => Ty::I8,
            "uchar" | "uint8" => Ty::U8,
            "short" | "int16" => Ty::I16,
            "ushort" | "uint16" => Ty::U16,
            "int" | "int32" => Ty::I32,
            "uint" | "uint32" => Ty::U32,
            "float" | "float32" => Ty::F32,
            "double" | "float64" => Ty::F64,
            _ => return None,
        })
    }
    fn size(self) -> usize {
        match self {
            Ty::I8 | Ty::U8 => 1,
            Ty::I16 | Ty::U16 => 2,
            Ty::I32 | Ty::U32 | Ty::F32 => 4,
            Ty::F64 => 8,
        }
    }
    /// Scale that maps this type's colour range to 0..255.
    fn color_scale(self) -> f64 {
        match self {
            Ty::U8 | Ty::I8 => 1.0,
            Ty::U16 | Ty::I16 => 1.0 / 257.0,
            Ty::U32 | Ty::I32 => 1.0 / 16_843_009.0,
            Ty::F32 | Ty::F64 => 255.0,
        }
    }
}

#[derive(Clone, Debug)]
enum Kind {
    Scalar(Ty),
    List(Ty, Ty),
}

#[derive(Clone, Debug)]
struct Prop {
    name: String,
    kind: Kind,
}

#[derive(Clone, Debug)]
struct Element {
    name: String,
    count: usize,
    props: Vec<Prop>,
}

impl Element {
    fn fixed_size(&self) -> Option<usize> {
        self.props.iter().map(|p| if let Kind::Scalar(t) = p.kind { Some(t.size()) } else { None }).sum()
    }
    /// Smallest bytes one record can take (binary), for count sanity checks.
    fn min_size(&self) -> usize {
        self.props.iter().map(|p| match p.kind {
            Kind::Scalar(t) => t.size(),
            Kind::List(c, _) => c.size(),
        }).sum::<usize>().max(1)
    }
    fn find(&self, names: &[&str]) -> Option<usize> {
        names.iter().find_map(|n| self.props.iter().position(|p| p.name == *n))
    }
}

struct Header {
    enc: Encoding,
    elements: Vec<Element>,
    body: usize,
}

fn header(bytes: &[u8]) -> Result<Header, String> {
    let end = bytes.windows(10).take(1 << 20).position(|w| w == b"end_header").ok_or("no end_header in the first megabyte")?;
    let mut body = end + 10;
    if bytes.get(body) == Some(&b'\r') {
        body += 1;
    }
    if bytes.get(body) == Some(&b'\n') {
        body += 1;
    }
    let text = std::str::from_utf8(&bytes[..end]).map_err(|_| "the header is not text")?;
    let mut enc = None;
    let mut elements: Vec<Element> = Vec::new();
    for (k, line) in text.lines().enumerate() {
        let w: Vec<&str> = line.split_whitespace().collect();
        match w.as_slice() {
            ["ply"] if k == 0 => {}
            _ if k == 0 => return Err("not a PLY file".into()),
            ["format", f, _v] => {
                enc = Some(match *f {
                    "ascii" => Encoding::Ascii,
                    "binary_little_endian" => Encoding::Le,
                    "binary_big_endian" => Encoding::Be,
                    other => return Err(format!("unknown format \"{other}\"")),
                })
            }
            ["comment", ..] | ["obj_info", ..] | [] => {}
            ["element", name, count] => {
                let count: usize = count.parse().map_err(|_| format!("bad element count \"{count}\""))?;
                elements.push(Element { name: name.to_string(), count, props: Vec::new() });
            }
            ["property", "list", c, i, name] => {
                let (c, i) = (Ty::parse(c).ok_or(format!("unknown type {c}"))?, Ty::parse(i).ok_or(format!("unknown type {i}"))?);
                elements.last_mut().ok_or("property before any element")?.props.push(Prop { name: name.to_string(), kind: Kind::List(c, i) });
            }
            ["property", t, name] => {
                let t = Ty::parse(t).ok_or(format!("unknown type {t}"))?;
                elements.last_mut().ok_or("property before any element")?.props.push(Prop { name: name.to_string(), kind: Kind::Scalar(t) });
            }
            other => return Err(format!("unexpected header line \"{}\"", other.join(" "))),
        }
    }
    Ok(Header { enc: enc.ok_or("missing format line")?, elements, body })
}

/// Reads values from the body in order.
struct Body<'a> {
    bytes: &'a [u8],
    pos: usize,
    enc: Encoding,
}

impl Body<'_> {
    #[inline]
    fn scalar(&mut self, t: Ty) -> Result<f64, String> {
        if self.enc == Encoding::Ascii {
            return self.token();
        }
        let n = t.size();
        let b = self.bytes.get(self.pos..self.pos + n).ok_or("unexpected end of data")?;
        self.pos += n;
        Ok(read_bin(b, t, self.enc == Encoding::Le))
    }

    fn token(&mut self) -> Result<f64, String> {
        let b = self.bytes;
        while self.pos < b.len() && b[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
        let start = self.pos;
        while self.pos < b.len() && !b[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
        if start == self.pos {
            return Err("unexpected end of data".into());
        }
        let s = std::str::from_utf8(&b[start..self.pos]).map_err(|_| "non-text value")?;
        s.parse::<f64>().map_err(|_| format!("bad number \"{s}\""))
    }

    fn skip_record(&mut self, e: &Element) -> Result<(), String> {
        for p in &e.props {
            match p.kind {
                Kind::Scalar(t) => {
                    self.scalar(t)?;
                }
                Kind::List(c, i) => {
                    let n = self.scalar(c)?;
                    if !(0.0..=1e6).contains(&n) {
                        return Err(format!("list of {n} items in {}", e.name));
                    }
                    for _ in 0..n as usize {
                        self.scalar(i)?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[inline]
fn read_bin(b: &[u8], t: Ty, le: bool) -> f64 {
    macro_rules! rd {
        ($ty:ty, $n:expr) => {{
            let mut a = [0u8; $n];
            a.copy_from_slice(&b[..$n]);
            (if le { <$ty>::from_le_bytes(a) } else { <$ty>::from_be_bytes(a) }) as f64
        }};
    }
    match t {
        Ty::I8 => b[0] as i8 as f64,
        Ty::U8 => b[0] as f64,
        Ty::I16 => rd!(i16, 2),
        Ty::U16 => rd!(u16, 2),
        Ty::I32 => rd!(i32, 4),
        Ty::U32 => rd!(u32, 4),
        Ty::F32 => rd!(f32, 4),
        Ty::F64 => rd!(f64, 8),
    }
}

struct VertexLayout {
    xyz: [usize; 3],
    normal: Option<[usize; 3]>,
    rgb: Option<[usize; 3]>,
    alpha: Option<usize>,
}

pub fn load(bytes: &[u8], opts: &LoadOptions, ctl: &Ctl) -> Result<Model, String> {
    let h = header(bytes)?;
    let remaining = bytes.len().saturating_sub(h.body);
    // declared counts must be possible with the bytes present
    for e in &h.elements {
        let at_most = if h.enc == Encoding::Ascii { remaining / 2 + 1 } else { remaining / e.min_size() };
        if e.count > at_most {
            return Err(format!("element \"{}\" declares {} records but the file holds at most {at_most}", e.name, e.count));
        }
    }
    let vi = h.elements.iter().position(|e| e.name == "vertex").ok_or("no vertex element")?;
    let ve = &h.elements[vi];
    let scalar_index = |names: &[&str]| -> Option<usize> { ve.find(names).filter(|&i| matches!(ve.props[i].kind, Kind::Scalar(_))) };
    let xyz = [scalar_index(&["x"]), scalar_index(&["y"]), scalar_index(&["z"])];
    let [Some(x), Some(y), Some(z)] = xyz else { return Err("vertices need x, y and z".into()) };
    let normal = match [scalar_index(&["nx"]), scalar_index(&["ny"]), scalar_index(&["nz"])] {
        [Some(a), Some(b), Some(c)] => Some([a, b, c]),
        _ => None,
    };
    let rgb = match [
        scalar_index(&["red", "r", "diffuse_red"]),
        scalar_index(&["green", "g", "diffuse_green"]),
        scalar_index(&["blue", "b", "diffuse_blue"]),
    ] {
        [Some(a), Some(b), Some(c)] => Some([a, b, c]),
        _ => None,
    };
    let layout = VertexLayout { xyz: [x, y, z], normal, rgb, alpha: scalar_index(&["alpha", "a", "diffuse_alpha"]) };
    let faces = h.elements.iter().position(|e| e.name == "face" && e.count > 0);
    if faces.is_some() && ve.count > MAX_VERTICES {
        return Err(format!("{} vertices; the limit is {MAX_VERTICES}", ve.count));
    }
    if faces.is_none() && ve.count > MAX_POINTS {
        return Err(format!("{} points; the limit is {MAX_POINTS}", ve.count));
    }

    let mut body = Body { bytes, pos: h.body, enc: h.enc };
    let mut verts: Vec<[f64; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut colors: Vec<[u8; 4]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut points: Option<PointsBuilder> = None;
    for (ei, e) in h.elements.iter().enumerate() {
        ctl.check()?;
        if ei == vi {
            if faces.is_some() {
                verts = try_vec(e.count, "vertices")?;
                if layout.normal.is_some() {
                    normals = try_vec(e.count, "normals")?;
                }
                if layout.rgb.is_some() {
                    colors = try_vec(e.count, "colours")?;
                }
            }
            read_vertices(&mut body, e, &layout, ctl, |p, n, c| {
                if faces.is_some() {
                    verts.push(p);
                    if let Some(n) = n {
                        normals.push(n);
                    }
                    if let Some(c) = c {
                        colors.push(c);
                    }
                    Ok(())
                } else {
                    if points.is_none() {
                        // origin: the first finite point (organised scans start with NaN)
                        let mut pb = PointsBuilder::auto(e.count)?;
                        pb.has_colors = c.is_some();
                        points = Some(pb);
                    }
                    match &mut points {
                        Some(pb) => pb.push(p, c.unwrap_or(WHITE)),
                        None => Ok(()),
                    }
                }
            })?;
        } else if Some(ei) == faces {
            let li = e
                .find(&["vertex_indices", "vertex_index"])
                .or_else(|| e.props.iter().position(|p| matches!(p.kind, Kind::List(..))))
                .ok_or("faces have no index list")?;
            let n_verts = verts.len();
            indices = try_vec(e.count.min(MAX_TRIANGLES) * 3, "indices")?;
            let mut poly: Vec<u32> = Vec::with_capacity(8);
            for k in 0..e.count {
                if k & 0xF_FFFF == 0 {
                    ctl.check()?;
                    (ctl.progress)(0.5 + 0.4 * k as f32 / e.count as f32);
                }
                for (pi, p) in e.props.iter().enumerate() {
                    match p.kind {
                        Kind::Scalar(t) => {
                            body.scalar(t)?;
                        }
                        Kind::List(c, it) => {
                            let n = body.scalar(c)?;
                            if !(0.0..=1e6).contains(&n) {
                                return Err(format!("face {k} has {n} corners"));
                            }
                            poly.clear();
                            for _ in 0..n as usize {
                                let v = body.scalar(it)?;
                                if pi == li {
                                    if !(v >= 0.0 && (v as usize) < n_verts) {
                                        return Err(format!("face {k} uses vertex {v}, but there are {n_verts}"));
                                    }
                                    poly.push(v as u32);
                                }
                            }
                            if pi == li {
                                for j in 2..poly.len() {
                                    indices.extend_from_slice(&[poly[0], poly[j - 1], poly[j]]);
                                }
                                if indices.len() / 3 > MAX_TRIANGLES {
                                    return Err(format!("more than {MAX_TRIANGLES} triangles"));
                                }
                            }
                        }
                    }
                }
            }
        } else {
            // skip an element we do not use
            if h.enc != Encoding::Ascii {
                if let Some(sz) = e.fixed_size() {
                    body.pos = body.pos.checked_add(sz.saturating_mul(e.count)).filter(|&p| p <= bytes.len()).ok_or("unexpected end of data")?;
                    continue;
                }
            }
            for _ in 0..e.count {
                body.skip_record(e)?;
            }
        }
    }
    if faces.is_none() {
        let pb = points.ok_or("the file has no vertices")?;
        return pb.build("ply", opts.shuffle, opts.max_points, ctl);
    }
    // narrow to f32 only after subtracting an f64 offset near the data
    let offset = first_finite(verts.iter().copied()).unwrap_or([0.0; 3]);
    let mb = MeshBuilder {
        positions: verts.iter().map(|p| relative(*p, offset)).collect(),
        normals: (layout.normal.is_some() && normals.len() == verts.len()).then_some(normals),
        colors: (layout.rgb.is_some() && colors.len() == verts.len()).then_some(colors),
        indices,
        offset,
    };
    drop(verts);
    mb.build("ply", opts.normals.unwrap_or(NormalMode::Smooth), opts.flat, ctl)
}

fn read_vertices(body: &mut Body, e: &Element, l: &VertexLayout, ctl: &Ctl, mut sink: impl FnMut([f64; 3], Option<[f32; 3]>, Option<[u8; 4]>) -> Result<(), String>) -> Result<(), String> {
    let types: Vec<Ty> = e.props.iter().map(|p| match p.kind {
        Kind::Scalar(t) => t,
        Kind::List(c, _) => c,
    }).collect();
    let mut vals = vec![0f64; e.props.len()];
    let color_of = |vals: &[f64]| -> Option<[u8; 4]> {
        let rgb = l.rgb?;
        let c = |i: usize| (vals[i] * types[i].color_scale()).round().clamp(0.0, 255.0) as u8;
        Some([c(rgb[0]), c(rgb[1]), c(rgb[2]), l.alpha.map(c).unwrap_or(255)])
    };
    // fast path: binary records of scalars only
    let fixed = e.fixed_size().filter(|_| body.enc != Encoding::Ascii);
    let offsets: Vec<usize> = e.props.iter().scan(0usize, |o, p| {
        let at = *o;
        *o += if let Kind::Scalar(t) = p.kind { t.size() } else { 0 };
        Some(at)
    }).collect();
    let le = body.enc == Encoding::Le;
    for k in 0..e.count {
        if k & 0xF_FFFF == 0 {
            ctl.check()?;
            (ctl.progress)(0.5 * k as f32 / e.count.max(1) as f32);
        }
        match fixed {
            Some(sz) => {
                let rec = body.bytes.get(body.pos..body.pos + sz).ok_or("unexpected end of data")?;
                for (i, t) in types.iter().enumerate() {
                    vals[i] = read_bin(&rec[offsets[i]..], *t, le);
                }
                body.pos += sz;
            }
            None => {
                for (i, p) in e.props.iter().enumerate() {
                    match p.kind {
                        Kind::Scalar(t) => vals[i] = body.scalar(t)?,
                        Kind::List(c, it) => {
                            let n = body.scalar(c)?;
                            if !(0.0..=1e6).contains(&n) {
                                return Err(format!("vertex {k}: list of {n} items"));
                            }
                            for _ in 0..n as usize {
                                body.scalar(it)?;
                            }
                        }
                    }
                }
            }
        }
        let p = [vals[l.xyz[0]], vals[l.xyz[1]], vals[l.xyz[2]]];
        let n = l.normal.map(|n| [vals[n[0]] as f32, vals[n[1]] as f32, vals[n[2]] as f32]);
        sink(p, n, color_of(&vals))?;
    }
    Ok(())
}
