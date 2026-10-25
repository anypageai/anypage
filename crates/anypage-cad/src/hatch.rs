//! HATCH entities. The `dxf` crate skips them, so they are read here from
//! the raw code pairs (ASCII DXF), kept next to the drawing, drawn as solid
//! fills or pattern lines, and written back into the saved file.
//!
//! Not kept: gradient fills (drawn solid in the hatch colour), the links to
//! source boundary objects (written as non-associative), binary DXF hatches.

use std::f64::consts::TAU;

use crate::geom::{bspline, ellipse_param, fit_curve, flatten, ocs, p, Aff, BBox, EArc, Path, PathB, P};

#[derive(Clone, Debug, PartialEq)]
pub struct SplineEdge {
    pub degree: i32,
    pub rational: bool,
    pub periodic: bool,
    pub knots: Vec<f64>,
    pub ctrl: Vec<P>,
    pub weights: Vec<f64>,
    pub fit: Vec<P>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Edge {
    Line(P, P),
    /// centre, radius, start and end degrees, counter-clockwise
    Arc { c: P, r: f64, start: f64, end: f64, ccw: bool },
    /// centre, major axis end (relative), minor/major ratio, start and end degrees
    Ellipse { c: P, major: P, ratio: f64, start: f64, end: f64, ccw: bool },
    Spline(SplineEdge),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Boundary {
    /// vertices with bulges
    Poly { closed: bool, pts: Vec<(P, f64)> },
    Edges(Vec<Edge>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Loop {
    pub flags: i32,
    pub boundary: Boundary,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatLine {
    pub angle: f64,
    pub base: P,
    pub offset: P,
    pub dashes: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hatch {
    pub handle: u64,
    pub layer: String,
    /// ACI 0..=256 (256 BYLAYER, 0 BYBLOCK).
    pub aci: i16,
    pub true_color: Option<i32>,
    pub paper: bool,
    pub visible: bool,
    pub elevation: f64,
    pub normal: (f64, f64, f64),
    pub pattern: String,
    pub solid: bool,
    pub loops: Vec<Loop>,
    pub style: i16,
    pub pattern_type: i16,
    pub angle: f64,
    pub scale: f64,
    pub double: bool,
    pub lines: Vec<PatLine>,
    pub seeds: Vec<P>,
    pub gradient: bool,
}

impl Default for Hatch {
    fn default() -> Self {
        Hatch {
            handle: 0,
            layer: "0".into(),
            aci: 256,
            true_color: None,
            paper: false,
            visible: true,
            elevation: 0.0,
            normal: (0.0, 0.0, 1.0),
            pattern: "SOLID".into(),
            solid: true,
            loops: Vec::new(),
            style: 0,
            pattern_type: 1,
            angle: 0.0,
            scale: 1.0,
            double: false,
            lines: Vec::new(),
            seeds: Vec::new(),
            gradient: false,
        }
    }
}

// ---------------------------------------------------------------- raw pairs

/// What a container (model space or one block) holds, in file order.
#[derive(Clone, Debug)]
pub enum RawItem {
    Entity(u64),
    Hatch(Box<Hatch>),
}

#[derive(Debug, Default)]
pub struct Scan {
    pub model: Vec<RawItem>,
    pub blocks: Vec<(String, Vec<RawItem>)>,
    pub warnings: Vec<String>,
}

struct Lines<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Lines<'a> {
    fn next_line(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.data.len() {
            return None;
        }
        let rest = &self.data[self.pos..];
        let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
        self.pos += end + 1;
        let mut line = &rest[..end];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        Some(line)
    }
    fn pair(&mut self) -> Option<(i32, &'a [u8])> {
        let code = self.next_line()?;
        let code = std::str::from_utf8(code).ok()?.trim().parse::<i32>().ok()?;
        let value = self.next_line()?;
        Some((code, value))
    }
}

fn trim(v: &[u8]) -> &[u8] {
    let s = v.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(v.len());
    let e = v.iter().rposition(|b| !b.is_ascii_whitespace()).map(|i| i + 1).unwrap_or(s);
    &v[s..e.max(s)]
}

const MAX_HATCH_PAIRS: usize = 2_000_000;
/// Pattern lines times boundary edges one hatch may cost to clip; above it
/// the hatch is drawn as a translucent fill.
const MAX_PATTERN_WORK: f64 = 30_000_000.0;

/// Scan ASCII DXF bytes for the order of entities and the HATCH entities in
/// model space and in blocks. `decode` turns raw string bytes into text.
pub fn scan(bytes: &[u8], decode: &dyn Fn(&[u8]) -> String) -> Scan {
    let mut out = Scan::default();
    if bytes.starts_with(b"AutoCAD Binary DXF") {
        out.warnings.push("binary DXF: HATCH entities are not read".into());
        return out;
    }
    let mut lines = Lines { data: bytes, pos: 0 };
    let mut section = String::new();
    let mut block: Option<(String, Vec<RawItem>)> = None;
    let mut in_block = false;
    let mut pending: Option<(i32, Vec<u8>)> = None;
    loop {
        let (code, value) = match pending.take() {
            Some((c, v)) => (c, v),
            None => match lines.pair() {
                Some((c, v)) => (c, trim(v).to_vec()),
                None => break,
            },
        };
        if code != 0 {
            continue;
        }
        let name = String::from_utf8_lossy(&value).to_string();
        match name.as_str() {
            "SECTION" => {
                if let Some((2, v)) = lines.pair() {
                    section = String::from_utf8_lossy(trim(v)).to_string();
                }
            }
            "ENDSEC" => section.clear(),
            "BLOCK" if section == "BLOCKS" => {
                in_block = true;
                let mut bname = String::new();
                // read the header of the block up to its first entity
                loop {
                    match lines.pair() {
                        Some((0, v)) => {
                            pending = Some((0, trim(v).to_vec()));
                            break;
                        }
                        Some((2, v)) if bname.is_empty() => bname = decode(trim(v)),
                        Some(_) => {}
                        None => break,
                    }
                }
                block = Some((bname, Vec::new()));
            }
            "ENDBLK" => {
                if let Some(b) = block.take() {
                    out.blocks.push(b);
                }
                in_block = false;
            }
            "EOF" => break,
            _ if section == "ENTITIES" || (section == "BLOCKS" && in_block) => {
                // collect this entity's pairs up to the next code 0
                let mut pairs: Vec<(i32, Vec<u8>)> = Vec::new();
                loop {
                    match lines.pair() {
                        Some((0, v)) => {
                            pending = Some((0, trim(v).to_vec()));
                            break;
                        }
                        Some((c, v)) => {
                            if name == "HATCH" {
                                if pairs.len() < MAX_HATCH_PAIRS {
                                    pairs.push((c, trim(v).to_vec()));
                                }
                            } else if c == 5 && pairs.is_empty() {
                                pairs.push((c, trim(v).to_vec()));
                            }
                        }
                        None => break,
                    }
                }
                let target = if section == "ENTITIES" { &mut out.model } else { match &mut block { Some(b) => &mut b.1, None => continue } };
                match name.as_str() {
                    "HATCH" => match parse_hatch(&pairs, decode) {
                        Ok(h) => target.push(RawItem::Hatch(Box::new(h))),
                        Err(e) => out.warnings.push(format!("HATCH skipped: {e}")),
                    },
                    "VERTEX" | "SEQEND" | "ATTRIB" => {}
                    _ => {
                        let handle = pairs.iter().find(|(c, _)| *c == 5).and_then(|(_, v)| u64::from_str_radix(&String::from_utf8_lossy(v), 16).ok());
                        if let Some(h) = handle {
                            target.push(RawItem::Entity(h));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

struct Cursor<'a> {
    pairs: &'a [(i32, Vec<u8>)],
    i: usize,
    decode: &'a dyn Fn(&[u8]) -> String,
}

impl<'a> Cursor<'a> {
    fn peek(&self) -> Option<i32> {
        self.pairs.get(self.i).map(|x| x.0)
    }
    fn s(&self, v: &[u8]) -> String {
        (self.decode)(v)
    }
    fn num(v: &[u8]) -> Result<f64, String> {
        let s = std::str::from_utf8(v).map_err(|_| "not a number".to_string())?;
        let x: f64 = s.trim().parse().map_err(|_| format!("not a number: {s}"))?;
        if x.is_finite() {
            Ok(x)
        } else {
            Err("not a finite number".into())
        }
    }
    /// The value of `code` if it is next.
    fn take(&mut self, code: i32) -> Option<&'a [u8]> {
        if self.peek() == Some(code) {
            self.i += 1;
            let pairs: &'a [(i32, Vec<u8>)] = self.pairs;
            Some(&pairs[self.i - 1].1)
        } else {
            None
        }
    }
    fn f(&mut self, code: i32) -> Result<f64, String> {
        match self.take(code) {
            Some(v) => Self::num(v),
            None => Err(format!("expected group code {code}, found {:?}", self.peek())),
        }
    }
    fn f_or(&mut self, code: i32, d: f64) -> Result<f64, String> {
        match self.take(code) {
            Some(v) => Self::num(v),
            None => Ok(d),
        }
    }
    fn i(&mut self, code: i32) -> Result<i64, String> {
        self.f(code).map(|x| x as i64)
    }
    fn i_or(&mut self, code: i32, d: i64) -> Result<i64, String> {
        self.f_or(code, d as f64).map(|x| x as i64)
    }
    fn point(&mut self, x: i32) -> Result<P, String> {
        Ok(p(self.f(x)?, self.f(x + 10)?))
    }
    /// Skip pairs until `code` is next (or the end).
    fn seek(&mut self, codes: &[i32]) {
        while let Some(c) = self.peek() {
            if codes.contains(&c) {
                return;
            }
            self.i += 1;
        }
    }
}

const MAX_COUNT: i64 = 1_000_000;

fn count(n: i64) -> Result<usize, String> {
    if (0..=MAX_COUNT).contains(&n) {
        Ok(n as usize)
    } else {
        Err(format!("bad count {n}"))
    }
}

fn parse_edge(c: &mut Cursor) -> Result<Edge, String> {
    let t = c.i(72)?;
    Ok(match t {
        1 => Edge::Line(c.point(10)?, c.point(11)?),
        2 => {
            let ctr = c.point(10)?;
            let r = c.f(40)?;
            let start = c.f(50)?;
            let end = c.f(51)?;
            let ccw = c.i_or(73, 1)? != 0;
            Edge::Arc { c: ctr, r, start, end, ccw }
        }
        3 => {
            let ctr = c.point(10)?;
            let major = c.point(11)?;
            let ratio = c.f(40)?;
            let start = c.f(50)?;
            let end = c.f(51)?;
            let ccw = c.i_or(73, 1)? != 0;
            Edge::Ellipse { c: ctr, major, ratio, start, end, ccw }
        }
        4 => {
            let degree = c.i(94)? as i32;
            let rational = c.i_or(73, 0)? != 0;
            let periodic = c.i_or(74, 0)? != 0;
            let nk = count(c.i(95)?)?;
            let nc = count(c.i(96)?)?;
            let mut knots = Vec::with_capacity(nk.min(4096));
            for _ in 0..nk {
                knots.push(c.f(40)?);
            }
            let mut ctrl = Vec::with_capacity(nc.min(4096));
            let mut weights = Vec::new();
            for _ in 0..nc {
                ctrl.push(c.point(10)?);
                if let Some(v) = c.take(42) {
                    weights.push(Cursor::num(v)?);
                }
            }
            let mut fit = Vec::new();
            if let Some(v) = c.take(97) {
                let nf = count(Cursor::num(v)? as i64)?;
                for _ in 0..nf {
                    fit.push(c.point(11)?);
                }
                // optional start / end tangents
                if c.peek() == Some(12) {
                    c.point(12)?;
                }
                if c.peek() == Some(13) {
                    c.point(13)?;
                }
            }
            Edge::Spline(SplineEdge { degree, rational, periodic, knots, ctrl, weights, fit })
        }
        other => return Err(format!("unknown boundary edge type {other}")),
    })
}

fn parse_loop(c: &mut Cursor) -> Result<Loop, String> {
    let flags = c.i(92)? as i32;
    let boundary = if flags & 2 != 0 {
        let has_bulge = c.i_or(72, 0)? != 0;
        let closed = c.i_or(73, 1)? != 0;
        let n = count(c.i(93)?)?;
        let mut pts = Vec::with_capacity(n.min(65536));
        for _ in 0..n {
            let q = c.point(10)?;
            let b = if has_bulge || c.peek() == Some(42) { c.f_or(42, 0.0)? } else { 0.0 };
            pts.push((q, b));
        }
        Boundary::Poly { closed, pts }
    } else {
        let n = count(c.i(93)?)?;
        let mut edges = Vec::with_capacity(n.min(65536));
        for _ in 0..n {
            edges.push(parse_edge(c)?);
        }
        Boundary::Edges(edges)
    };
    // source boundary objects
    if let Some(v) = c.take(97) {
        let n = count(Cursor::num(v)? as i64)?;
        for _ in 0..n {
            c.take(330);
        }
    }
    Ok(Loop { flags, boundary })
}

/// One HATCH from its code pairs (without the leading `0 HATCH`).
pub fn parse_hatch(pairs: &[(i32, Vec<u8>)], decode: &dyn Fn(&[u8]) -> String) -> Result<Hatch, String> {
    let mut h = Hatch { aci: 256, ..Hatch::default() };
    let mut c = Cursor { pairs, i: 0, decode };
    // common entity part, up to the hatch subclass
    while let Some(code) = c.peek() {
        let v = &pairs[c.i].1;
        match code {
            5 => h.handle = u64::from_str_radix(&String::from_utf8_lossy(v), 16).unwrap_or(0),
            8 => h.layer = c.s(v),
            62 => h.aci = Cursor::num(v)? as i16,
            420 => h.true_color = Some(Cursor::num(v)? as i32),
            67 => h.paper = Cursor::num(v)? != 0.0,
            60 => h.visible = Cursor::num(v)? == 0.0,
            100 if &v[..] == b"AcDbHatch" => {
                c.i += 1;
                break;
            }
            10 => break, // R13 files without subclass markers
            _ => {}
        }
        c.i += 1;
    }
    h.elevation = if c.peek() == Some(10) {
        c.f(10)?;
        c.f(20)?;
        c.f_or(30, 0.0)?
    } else {
        0.0
    };
    if c.peek() == Some(210) {
        h.normal = (c.f(210)?, c.f(220)?, c.f(230)?);
    }
    if let Some(v) = c.take(2) {
        h.pattern = c.s(v);
    }
    h.solid = c.i_or(70, 0)? != 0;
    c.i_or(71, 0)?;
    let n = count(c.i(91)?)?;
    for _ in 0..n {
        h.loops.push(parse_loop(&mut c)?);
    }
    h.style = c.i_or(75, 0)? as i16;
    h.pattern_type = c.i_or(76, 1)? as i16;
    if !h.solid {
        h.angle = c.f_or(52, 0.0)?;
        h.scale = c.f_or(41, 1.0)?;
        h.double = c.i_or(77, 0)? != 0;
        let nl = count(c.i_or(78, 0)?)?;
        for _ in 0..nl {
            let angle = c.f(53)?;
            let base = p(c.f(43)?, c.f(44)?);
            let offset = p(c.f(45)?, c.f(46)?);
            let nd = count(c.i_or(79, 0)?)?;
            let mut dashes = Vec::with_capacity(nd.min(64));
            for _ in 0..nd {
                dashes.push(c.f(49)?);
            }
            lines_push(&mut h.lines, PatLine { angle, base, offset, dashes });
        }
    }
    c.seek(&[98, 450]);
    if let Some(v) = c.take(98) {
        let ns = count(Cursor::num(v)? as i64)?;
        for _ in 0..ns {
            if c.peek() != Some(10) {
                break;
            }
            h.seeds.push(c.point(10)?);
        }
    }
    c.seek(&[450]);
    if let Some(v) = c.take(450) {
        h.gradient = Cursor::num(v)? != 0.0;
    }
    Ok(h)
}

fn lines_push(v: &mut Vec<PatLine>, l: PatLine) {
    if v.len() < 1024 {
        v.push(l);
    }
}

// ---------------------------------------------------------------- geometry

/// Parameter range of a boundary arc: `ccw` runs up, otherwise down (the
/// stored angles are negated), at most one turn. Constant work for any
/// input, including huge or non-finite angles from a file.
fn sweep(start_deg: f64, end_deg: f64, ccw: bool) -> (f64, f64) {
    let (s, e) = if ccw { (start_deg, end_deg) } else { (-start_deg, -end_deg) };
    let (s, e) = (s.to_radians(), e.to_radians());
    if !(s.is_finite() && e.is_finite()) {
        return (0.0, if ccw { TAU } else { -TAU });
    }
    let d = if ccw { (e - s).rem_euclid(TAU) } else { (s - e).rem_euclid(TAU) };
    let d = if d > 0.0 && d < TAU { d } else { TAU };
    let s = s.rem_euclid(TAU);
    let full = ((end_deg - start_deg).abs() - 360.0).abs() < 1e-9;
    if full {
        return (s, if ccw { s + TAU } else { s - TAU });
    }
    (s, if ccw { s + d } else { s - d })
}

impl Edge {
    fn add_to(&self, b: &mut PathB) {
        match self {
            Edge::Line(a, z) => {
                b.line_to(*a);
                b.line_to(*z);
            }
            Edge::Arc { c, r, start, end, ccw } => {
                let (t0, t1) = sweep(*start, *end, *ccw);
                b.arc(EArc { c: *c, u: p(*r, 0.0), v: p(0.0, *r), t0, t1 });
            }
            Edge::Ellipse { c, major, ratio, start, end, ccw } => {
                let minor = p(-major.y * ratio, major.x * ratio);
                let (a0, a1) = sweep(*start, *end, *ccw);
                // the stored values are angles; the curve wants parameters
                let t0 = ellipse_param(*ratio, a0);
                let mut t1 = ellipse_param(*ratio, a1);
                if *ccw {
                    while t1 <= t0 {
                        t1 += TAU;
                    }
                    if (a1 - a0 - TAU).abs() < 1e-9 {
                        t1 = t0 + TAU;
                    }
                } else {
                    while t1 >= t0 {
                        t1 -= TAU;
                    }
                    if (a0 - a1 - TAU).abs() < 1e-9 {
                        t1 = t0 - TAU;
                    }
                }
                b.arc(EArc { c: *c, u: *major, v: minor, t0, t1 });
            }
            Edge::Spline(s) => {
                let path = if s.ctrl.len() > s.degree.max(0) as usize {
                    bspline(s.degree.max(1) as usize, &s.knots, &s.ctrl, if s.rational { &s.weights } else { &[] })
                } else {
                    fit_curve(&s.fit, false)
                };
                for seg in path.unwrap_or_default() {
                    match seg {
                        crate::geom::Seg::M(q) | crate::geom::Seg::L(q) => b.line_to(q),
                        crate::geom::Seg::C(x, y, q) => b.cubic_to(x, y, q),
                        crate::geom::Seg::A(a) => b.arc(a),
                        crate::geom::Seg::Z => {}
                    }
                }
            }
        }
    }
}

impl Hatch {
    pub fn ocs(&self) -> Aff {
        ocs(self.normal, self.elevation)
    }

    /// Boundary loops as closed subpaths in OCS.
    pub fn boundary_ocs(&self) -> Path {
        let mut b = PathB::new();
        for l in &self.loops {
            match &l.boundary {
                Boundary::Poly { pts, .. } => {
                    let n = pts.len();
                    if n < 2 {
                        continue;
                    }
                    b.move_to(pts[0].0);
                    for i in 0..n {
                        let (a, bulge) = pts[i];
                        let z = pts[(i + 1) % n].0;
                        if i + 1 == n && a.dist(z) < 1e-12 {
                            break;
                        }
                        match crate::geom::bulge_arc(a, z, bulge) {
                            Some(arc) => b.arc(arc),
                            None => b.line_to(z),
                        }
                    }
                    b.close();
                }
                Boundary::Edges(edges) => {
                    let mut first = true;
                    for e in edges {
                        let start = match e {
                            Edge::Line(a, _) => Some(*a),
                            _ => None,
                        };
                        if first {
                            match start {
                                Some(a) => b.move_to(a),
                                None => {
                                    // move to where the curve starts
                                    let mut tmp = PathB::new();
                                    e.add_to(&mut tmp);
                                    if let Some(crate::geom::Seg::M(q)) = tmp.segs.first() {
                                        b.move_to(*q);
                                    }
                                }
                            }
                            first = false;
                        }
                        e.add_to(&mut b);
                    }
                    if !first {
                        b.close();
                    }
                }
            }
        }
        b.finish()
    }

    /// Pattern lines clipped to the boundary (even-odd), in OCS. `None` when
    /// the pattern is too dense to draw as lines.
    pub fn pattern_ocs(&self, max_segments: usize) -> Option<Path> {
        let boundary = self.boundary_ocs();
        let bb = crate::geom::path_bbox(&boundary);
        if bb.is_empty() {
            return Some(Vec::new());
        }
        let diag = bb.w().hypot(bb.h()).max(1e-9);
        let mut budget = 2_000_000usize;
        let rings: Vec<Vec<P>> =
            flatten(&boundary, diag * 2e-4, None, &mut budget).into_iter().filter(|r| r.pts.len() >= 3).map(|r| r.pts).collect();
        let mut out = PathB::new();
        let mut segs = 0usize;
        for line in &self.lines {
            let (s, c) = line.angle.to_radians().sin_cos();
            let d = p(c, s);
            let n = p(-s, c);
            let spacing = line.offset.dot(n);
            if spacing.abs() < diag * 1e-6 || !spacing.is_finite() {
                continue;
            }
            let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
            for q in [p(bb.x0, bb.y0), p(bb.x1, bb.y0), p(bb.x0, bb.y1), p(bb.x1, bb.y1)] {
                let h = q.sub(line.base).dot(n);
                lo = lo.min(h);
                hi = hi.max(h);
            }
            let (k0, k1) = if spacing > 0.0 { ((lo / spacing).ceil(), (hi / spacing).floor()) } else { ((hi / spacing).ceil(), (lo / spacing).floor()) };
            if !(k0.is_finite() && k1.is_finite()) || k1 - k0 > max_segments as f64 {
                return None;
            }
            // every family line is tested against every boundary edge
            let edges: usize = rings.iter().map(|r| r.len()).sum();
            if (k1 - k0 + 1.0).max(0.0) * edges as f64 > MAX_PATTERN_WORK {
                return None;
            }
            let period: f64 = line.dashes.iter().map(|x| x.abs()).sum();
            let mut ts: Vec<f64> = Vec::new();
            // count with an integer: far from the origin `k += 1.0` may not change k
            let lines_n = if k1 >= k0 { (k1 - k0) as usize + 1 } else { 0 };
            for step in 0..lines_n {
                let k = k0 + step as f64;
                let origin = line.base.add(line.offset.mul(k));
                ts.clear();
                for ring in &rings {
                    for i in 0..ring.len() {
                        let a = ring[i];
                        let z = ring[(i + 1) % ring.len()];
                        let ha = a.sub(origin).dot(n);
                        let hz = z.sub(origin).dot(n);
                        if (ha > 0.0) != (hz > 0.0) {
                            let ta = a.sub(origin).dot(d);
                            let tz = z.sub(origin).dot(d);
                            ts.push(ta + (tz - ta) * (-ha) / (hz - ha));
                        }
                    }
                }
                ts.sort_by(|a, b| a.total_cmp(b));
                for &[t0, t1] in ts.as_chunks::<2>().0 {
                    if period <= diag * 1e-9 || line.dashes.is_empty() {
                        out.move_to(origin.add(d.mul(t0)));
                        out.line_to(origin.add(d.mul(t1)));
                        segs += 1;
                    } else {
                        if (t1 - t0) / period > max_segments as f64 {
                            return None;
                        }
                        let mut pos = (t0 / period).floor() * period;
                        // far from the line origin `pos += len` can stop
                        // moving: bound the walk by its dash count
                        let mut steps = ((t1 - t0) / period + 2.0) as usize * line.dashes.len() + 2;
                        'walk: while pos < t1 {
                            for dash in &line.dashes {
                                if steps == 0 {
                                    break 'walk;
                                }
                                steps -= 1;
                                let len = dash.abs();
                                if *dash >= 0.0 {
                                    let (a, z) = (pos.max(t0), (pos + len.max(period * 1e-3)).min(t1));
                                    if z > a {
                                        out.move_to(origin.add(d.mul(a)));
                                        out.line_to(origin.add(d.mul(z)));
                                        segs += 1;
                                    }
                                }
                                pos += len;
                                if pos >= t1 {
                                    break 'walk;
                                }
                            }
                        }
                    }
                    if segs > max_segments {
                        return None;
                    }
                }
            }
        }
        Some(out.finish())
    }

    /// Bounding box in drawing coordinates.
    pub fn bbox(&self) -> BBox {
        crate::geom::path_bbox(&crate::geom::transform_path(&self.boundary_ocs(), &self.ocs()))
    }

    pub fn translate(&mut self, d: P) {
        // move in OCS: the OCS axes are orthonormal, so project the delta
        let m = self.ocs();
        let ax = p(m.a, m.b);
        let ay = p(m.c, m.d);
        let (ax_len, ay_len) = (ax.dot(ax).max(1e-300), ay.dot(ay).max(1e-300));
        let dl = p(d.dot(ax) / ax_len, d.dot(ay) / ay_len);
        let mv = |q: &mut P| *q = q.add(dl);
        for l in &mut self.loops {
            match &mut l.boundary {
                Boundary::Poly { pts, .. } => pts.iter_mut().for_each(|(q, _)| mv(q)),
                Boundary::Edges(edges) => {
                    for e in edges {
                        match e {
                            Edge::Line(a, z) => {
                                mv(a);
                                mv(z);
                            }
                            Edge::Arc { c, .. } | Edge::Ellipse { c, .. } => mv(c),
                            Edge::Spline(s) => {
                                s.ctrl.iter_mut().for_each(mv);
                                s.fit.iter_mut().for_each(mv);
                            }
                        }
                    }
                }
            }
        }
        for l in &mut self.lines {
            mv(&mut l.base);
        }
        self.seeds.iter_mut().for_each(mv);
    }
}

// ---------------------------------------------------------------- writing

fn fnum(x: f64) -> String {
    let x = if x.is_finite() { x } else { 0.0 };
    let s = format!("{x}");
    if s.contains('.') {
        s
    } else {
        format!("{s}.0")
    }
}

/// Text for a DXF string value: one line; non-ASCII escaped as `\U+XXXX`
/// for versions that are not UTF-8.
pub fn dxf_string(s: &str, ascii_only: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\r' | '\n' => out.push(' '),
            c if ascii_only && !c.is_ascii() => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\U+{:04X}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// The hatch as DXF text (pairs with CRLF line ends, like the dxf crate).
pub fn write_hatch(h: &Hatch, owner: Option<u64>, ascii_only: bool) -> String {
    let mut s = String::new();
    let mut pair = |code: i32, v: &str| {
        s.push_str(&format!("{code:>3}\r\n{v}\r\n"));
    };
    pair(0, "HATCH");
    pair(5, &format!("{:X}", h.handle));
    if let Some(o) = owner {
        pair(330, &format!("{o:X}"));
    }
    pair(100, "AcDbEntity");
    if h.paper {
        pair(67, "1");
    }
    pair(8, &dxf_string(&h.layer, ascii_only));
    if h.aci != 256 {
        pair(62, &h.aci.to_string());
    }
    if let Some(tc) = h.true_color {
        pair(420, &tc.to_string());
    }
    if !h.visible {
        pair(60, "1");
    }
    pair(100, "AcDbHatch");
    pair(10, "0.0");
    pair(20, "0.0");
    pair(30, &fnum(h.elevation));
    pair(210, &fnum(h.normal.0));
    pair(220, &fnum(h.normal.1));
    pair(230, &fnum(h.normal.2));
    pair(2, &dxf_string(&h.pattern, ascii_only));
    pair(70, if h.solid { "1" } else { "0" });
    pair(71, "0");
    pair(91, &h.loops.len().to_string());
    for l in &h.loops {
        match &l.boundary {
            Boundary::Poly { closed, pts } => {
                pair(92, &(l.flags | 2).to_string());
                let bulges = pts.iter().any(|(_, b)| *b != 0.0);
                pair(72, if bulges { "1" } else { "0" });
                pair(73, if *closed { "1" } else { "0" });
                pair(93, &pts.len().to_string());
                for (q, b) in pts {
                    pair(10, &fnum(q.x));
                    pair(20, &fnum(q.y));
                    if bulges {
                        pair(42, &fnum(*b));
                    }
                }
            }
            Boundary::Edges(edges) => {
                pair(92, &(l.flags & !2).to_string());
                pair(93, &edges.len().to_string());
                for e in edges {
                    match e {
                        Edge::Line(a, z) => {
                            pair(72, "1");
                            pair(10, &fnum(a.x));
                            pair(20, &fnum(a.y));
                            pair(11, &fnum(z.x));
                            pair(21, &fnum(z.y));
                        }
                        Edge::Arc { c, r, start, end, ccw } => {
                            pair(72, "2");
                            pair(10, &fnum(c.x));
                            pair(20, &fnum(c.y));
                            pair(40, &fnum(*r));
                            pair(50, &fnum(*start));
                            pair(51, &fnum(*end));
                            pair(73, if *ccw { "1" } else { "0" });
                        }
                        Edge::Ellipse { c, major, ratio, start, end, ccw } => {
                            pair(72, "3");
                            pair(10, &fnum(c.x));
                            pair(20, &fnum(c.y));
                            pair(11, &fnum(major.x));
                            pair(21, &fnum(major.y));
                            pair(40, &fnum(*ratio));
                            pair(50, &fnum(*start));
                            pair(51, &fnum(*end));
                            pair(73, if *ccw { "1" } else { "0" });
                        }
                        Edge::Spline(sp) => {
                            pair(72, "4");
                            pair(94, &sp.degree.to_string());
                            pair(73, if sp.rational { "1" } else { "0" });
                            pair(74, if sp.periodic { "1" } else { "0" });
                            pair(95, &sp.knots.len().to_string());
                            pair(96, &sp.ctrl.len().to_string());
                            for k in &sp.knots {
                                pair(40, &fnum(*k));
                            }
                            for (i, q) in sp.ctrl.iter().enumerate() {
                                pair(10, &fnum(q.x));
                                pair(20, &fnum(q.y));
                                if sp.rational {
                                    pair(42, &fnum(sp.weights.get(i).copied().unwrap_or(1.0)));
                                }
                            }
                            if !sp.fit.is_empty() {
                                pair(97, &sp.fit.len().to_string());
                                for q in &sp.fit {
                                    pair(11, &fnum(q.x));
                                    pair(21, &fnum(q.y));
                                }
                            }
                        }
                    }
                }
            }
        }
        pair(97, "0");
    }
    pair(75, &h.style.to_string());
    pair(76, &h.pattern_type.to_string());
    if !h.solid {
        pair(52, &fnum(h.angle));
        pair(41, &fnum(h.scale));
        pair(77, if h.double { "1" } else { "0" });
        pair(78, &h.lines.len().to_string());
        for l in &h.lines {
            pair(53, &fnum(l.angle));
            pair(43, &fnum(l.base.x));
            pair(44, &fnum(l.base.y));
            pair(45, &fnum(l.offset.x));
            pair(46, &fnum(l.offset.y));
            pair(79, &l.dashes.len().to_string());
            for d in &l.dashes {
                pair(49, &fnum(*d));
            }
        }
    }
    pair(98, &h.seeds.len().to_string());
    for q in &h.seeds {
        pair(10, &fnum(q.x));
        pair(20, &fnum(q.y));
    }
    s
}

/// Standard pattern lines for a few common predefined patterns, scaled and
/// rotated the way DXF stores them; unknown names get ANSI31.
pub fn predefined_lines(name: &str, angle_deg: f64, scale: f64) -> Vec<PatLine> {
    let base: Vec<(f64, f64, f64, f64, f64, Vec<f64>)> = match name.to_ascii_uppercase().as_str() {
        "ANSI37" | "NET" => vec![(45.0, 0.0, 0.0, 0.0, 3.175, vec![]), (135.0, 0.0, 0.0, 0.0, 3.175, vec![])],
        "LINE" => vec![(0.0, 0.0, 0.0, 0.0, 3.175, vec![])],
        "DASH" => vec![(0.0, 0.0, 0.0, 3.175, 3.175, vec![3.175, -3.175])],
        "ANSI32" => vec![(45.0, 0.0, 0.0, 0.0, 9.525, vec![]), (45.0, 4.49, 0.0, 0.0, 9.525, vec![])],
        _ => vec![(45.0, 0.0, 0.0, 0.0, 3.175, vec![])],
    };
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    base.into_iter()
        .map(|(a, bx, by, dx, dy, dashes)| {
            let ang = a + angle_deg;
            let r = Aff::rotate(angle_deg.to_radians());
            let l_rot = Aff::rotate(a.to_radians());
            // offsets are given in the line's own frame (along, across)
            let off = r.apply_v(l_rot.apply_v(p(dx, dy))).mul(scale);
            PatLine { angle: ang, base: r.apply(p(bx, by)).mul(scale), offset: off, dashes: dashes.into_iter().map(|d| d * scale).collect() }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(v: &[u8]) -> String {
        String::from_utf8_lossy(v).to_string()
    }

    pub(crate) const SQUARE_HATCH: &str = "  0\nSECTION\n  2\nENTITIES\n  0\nLINE\n  5\n2A\n  8\n0\n 10\n0\n 20\n0\n 11\n1\n 21\n1\n  0\nHATCH\n  5\n2B\n100\nAcDbEntity\n  8\nFILL\n 62\n1\n100\nAcDbHatch\n 10\n0\n 20\n0\n 30\n0\n210\n0\n220\n0\n230\n1\n  2\nSOLID\n 70\n1\n 71\n0\n 91\n2\n 92\n7\n 72\n0\n 73\n1\n 93\n4\n 10\n0\n 20\n0\n 10\n10\n 20\n0\n 10\n10\n 20\n10\n 10\n0\n 20\n10\n 97\n0\n 92\n1\n 93\n1\n 72\n2\n 10\n5\n 20\n5\n 40\n2\n 50\n0\n 51\n360\n 73\n1\n 97\n0\n 75\n0\n 76\n1\n 98\n1\n 10\n1\n 20\n1\n  0\nENDSEC\n  0\nEOF\n";

    #[test]
    fn scans_and_parses() {
        let s = scan(SQUARE_HATCH.as_bytes(), &decode);
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert_eq!(s.model.len(), 2);
        assert!(matches!(s.model[0], RawItem::Entity(0x2A)));
        let RawItem::Hatch(h) = &s.model[1] else { panic!() };
        assert_eq!(h.handle, 0x2B);
        assert_eq!(h.layer, "FILL");
        assert_eq!(h.aci, 1);
        assert!(h.solid);
        assert_eq!(h.loops.len(), 2);
        assert_eq!(h.seeds, vec![p(1.0, 1.0)]);
        // square minus the circle hole
        let path = h.boundary_ocs();
        let areas = crate::geom::subpath_areas(&path);
        assert_eq!(areas.len(), 2);
        assert!((areas[0].abs() - 100.0).abs() < 1e-9);
        assert!((areas[1].abs() - std::f64::consts::PI * 4.0).abs() < 1e-9, "{areas:?}");
    }

    #[test]
    fn write_then_parse_round_trips() {
        let s = scan(SQUARE_HATCH.as_bytes(), &decode);
        let RawItem::Hatch(h) = &s.model[1] else { panic!() };
        let mut h2 = (**h).clone();
        h2.solid = false;
        h2.pattern = "ANSI31".into();
        h2.lines = predefined_lines("ANSI31", 0.0, 1.0);
        let text = write_hatch(&h2, Some(0x1F), false);
        let mut lines = Lines { data: text.as_bytes(), pos: 0 };
        let mut pairs = Vec::new();
        while let Some((c, v)) = lines.pair() {
            pairs.push((c, trim(v).to_vec()));
        }
        assert_eq!(pairs[0], (0, b"HATCH".to_vec()));
        let back = parse_hatch(&pairs[1..], &decode).unwrap();
        assert_eq!(back.loops, h2.loops);
        assert_eq!(back.lines, h2.lines);
        assert_eq!(back.layer, "FILL");
        assert!(!back.solid);
    }

    #[test]
    fn pattern_lines_stay_inside() {
        let s = scan(SQUARE_HATCH.as_bytes(), &decode);
        let RawItem::Hatch(h) = &s.model[1] else { panic!() };
        let mut h = (**h).clone();
        h.solid = false;
        h.lines = vec![PatLine { angle: 0.0, base: p(0.0, 0.0), offset: p(0.0, 1.0), dashes: vec![] }];
        let path = h.pattern_ocs(100_000).unwrap();
        // horizontal lines at y = 1..9; those through the hole are split in two
        let mut segs = Vec::new();
        let mut start = p(0.0, 0.0);
        for s in &path {
            match s {
                crate::geom::Seg::M(q) => start = *q,
                crate::geom::Seg::L(q) => segs.push((start, *q)),
                _ => {}
            }
        }
        assert!(segs.len() >= 9 + 3, "{}", segs.len());
        for (a, b) in &segs {
            let mid = a.lerp(*b, 0.5);
            assert!(mid.x > 0.0 && mid.x < 10.0);
            assert!(mid.dist(p(5.0, 5.0)) > 2.0 - 1e-6, "{mid:?} in the hole");
        }
    }

    /// Hostile numbers: a tiny dashed hatch far from its pattern origin
    /// (where `k += 1.0` and `pos += dash` stop changing the value), arcs with
    /// absurd angles, a dense pattern on a long boundary. All must finish fast.
    #[test]
    fn hostile_patterns_terminate() {
        let far = 1.0e10;
        let sq = |x: f64, y: f64, s: f64| Loop {
            flags: 1 | 2,
            boundary: Boundary::Poly { closed: true, pts: vec![(p(x, y), 0.0), (p(x + s, y), 0.0), (p(x + s, y + s), 0.0), (p(x, y + s), 0.0)] },
        };
        let t = std::time::Instant::now();
        let h = Hatch {
            solid: false,
            loops: vec![sq(far, far, 1e-4)],
            lines: vec![PatLine { angle: 0.0, base: p(0.0, 0.0), offset: p(0.0, 1e-9), dashes: vec![1e-7, -1e-7] }],
            ..Hatch::default()
        };
        let _ = h.pattern_ocs(200_000);
        let arcs = Hatch {
            solid: false,
            loops: vec![Loop {
                flags: 1,
                boundary: Boundary::Edges(vec![
                    Edge::Arc { c: p(0.0, 0.0), r: 1.0, start: 1e300, end: -1e300, ccw: true },
                    Edge::Arc { c: p(0.0, 0.0), r: 1.0, start: f64::INFINITY, end: 0.0, ccw: false },
                    Edge::Ellipse { c: p(0.0, 0.0), major: p(2.0, 0.0), ratio: 0.5, start: 1e300, end: 1e301, ccw: false },
                ]),
            }],
            lines: vec![PatLine { angle: 45.0, base: p(0.0, 0.0), offset: p(0.0, 0.1), dashes: vec![] }],
            ..Hatch::default()
        };
        let _ = arcs.pattern_ocs(200_000);
        // 200k-vertex boundary and 100k pattern lines: refused, not computed
        let pts: Vec<(P, f64)> = (0..200_000).map(|i| (p((i as f64 * 0.001).cos(), (i as f64 * 0.001).sin()), 0.0)).collect();
        let dense = Hatch {
            solid: false,
            loops: vec![Loop { flags: 1, boundary: Boundary::Poly { closed: true, pts } }],
            lines: vec![PatLine { angle: 0.0, base: p(0.0, 0.0), offset: p(0.0, 2e-5), dashes: vec![] }],
            ..Hatch::default()
        };
        assert!(dense.pattern_ocs(200_000).is_none());
        assert!(t.elapsed().as_secs_f64() < 5.0, "{:?}", t.elapsed());
    }
}
