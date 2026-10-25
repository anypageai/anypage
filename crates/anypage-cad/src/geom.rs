//! 2D geometry in drawing units (f64, y up): points, affine transforms,
//! paths with exact elliptic arcs, B-spline conversion, flattening with
//! culling, clipping, lengths and areas.

use std::f64::consts::{PI, TAU};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct P {
    pub x: f64,
    pub y: f64,
}

pub const fn p(x: f64, y: f64) -> P {
    P { x, y }
}

// Plain methods rather than operator traits: call sites read left to right
// and need no trait imports.
#[allow(clippy::should_implement_trait)]
impl P {
    pub fn add(self, o: P) -> P {
        p(self.x + o.x, self.y + o.y)
    }
    pub fn sub(self, o: P) -> P {
        p(self.x - o.x, self.y - o.y)
    }
    pub fn mul(self, k: f64) -> P {
        p(self.x * k, self.y * k)
    }
    pub fn dot(self, o: P) -> f64 {
        self.x * o.x + self.y * o.y
    }
    pub fn cross(self, o: P) -> f64 {
        self.x * o.y - self.y * o.x
    }
    pub fn len(self) -> f64 {
        self.x.hypot(self.y)
    }
    pub fn dist(self, o: P) -> f64 {
        self.sub(o).len()
    }
    pub fn lerp(self, o: P, t: f64) -> P {
        p(self.x + (o.x - self.x) * t, self.y + (o.y - self.y) * t)
    }
    pub fn finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// Affine map `x' = a x + c y + e`, `y' = b x + d y + f` (tiny-skia order).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aff {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Default for Aff {
    fn default() -> Self {
        Aff::IDENTITY
    }
}

impl Aff {
    pub const IDENTITY: Aff = Aff { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    pub fn translate(x: f64, y: f64) -> Aff {
        Aff { e: x, f: y, ..Aff::IDENTITY }
    }
    pub fn scale(sx: f64, sy: f64) -> Aff {
        Aff { a: sx, d: sy, ..Aff::IDENTITY }
    }
    pub fn rotate(rad: f64) -> Aff {
        let (s, c) = rad.sin_cos();
        Aff { a: c, b: s, c: -s, d: c, e: 0.0, f: 0.0 }
    }
    /// `self ∘ inner`: apply `inner` first, then `self`.
    #[allow(clippy::should_implement_trait)]
    pub fn mul(self, i: Aff) -> Aff {
        Aff {
            a: self.a * i.a + self.c * i.b,
            b: self.b * i.a + self.d * i.b,
            c: self.a * i.c + self.c * i.d,
            d: self.b * i.c + self.d * i.d,
            e: self.a * i.e + self.c * i.f + self.e,
            f: self.b * i.e + self.d * i.f + self.f,
        }
    }
    pub fn apply(&self, q: P) -> P {
        p(self.a * q.x + self.c * q.y + self.e, self.b * q.x + self.d * q.y + self.f)
    }
    /// The linear part only (directions, axis vectors).
    pub fn apply_v(&self, v: P) -> P {
        p(self.a * v.x + self.c * v.y, self.b * v.x + self.d * v.y)
    }
    pub fn det(&self) -> f64 {
        self.a * self.d - self.b * self.c
    }
    pub fn inverse(&self) -> Option<Aff> {
        let det = self.det();
        if det.abs() < 1e-300 || !det.is_finite() {
            return None;
        }
        let (a, b, c, d) = (self.d / det, -self.b / det, -self.c / det, self.a / det);
        Some(Aff { a, b, c, d, e: -(a * self.e + c * self.f), f: -(b * self.e + d * self.f) })
    }
    /// Largest stretch factor of the linear part.
    pub fn max_scale(&self) -> f64 {
        let (a, b, c, d) = (self.a, self.b, self.c, self.d);
        let s1 = a * a + b * b + c * c + d * d;
        let s2 = ((a * a + b * b - c * c - d * d).powi(2) + 4.0 * (a * c + b * d).powi(2)).sqrt();
        ((s1 + s2) * 0.5).sqrt()
    }
    /// Geometric mean scale (for widths and text heights).
    pub fn mean_scale(&self) -> f64 {
        self.det().abs().sqrt()
    }
    /// The uniform scale factor when the linear part is a similarity
    /// (rotation, uniform scale, mirror): lengths scale by it exactly.
    pub fn similarity_scale(&self) -> Option<f64> {
        let s1 = self.a.hypot(self.b);
        let s2 = self.c.hypot(self.d);
        let dot = self.a * self.c + self.b * self.d;
        let tol = 1e-9 * s1.max(s2);
        ((s1 - s2).abs() <= tol && dot.abs() <= tol * s1.max(s2) && s1.is_finite()).then_some(s1)
    }
    pub fn finite(&self) -> bool {
        [self.a, self.b, self.c, self.d, self.e, self.f].iter().all(|v| v.is_finite())
    }
}

/// Axis-aligned box; `EMPTY` until a point is added.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BBox {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl BBox {
    pub const EMPTY: BBox = BBox { x0: f64::INFINITY, y0: f64::INFINITY, x1: f64::NEG_INFINITY, y1: f64::NEG_INFINITY };

    pub fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> BBox {
        BBox { x0: x0.min(x1), y0: y0.min(y1), x1: x0.max(x1), y1: y0.max(y1) }
    }
    pub fn is_empty(&self) -> bool {
        !(self.x0 <= self.x1 && self.y0 <= self.y1)
    }
    pub fn add(&mut self, q: P) {
        if q.finite() {
            self.x0 = self.x0.min(q.x);
            self.y0 = self.y0.min(q.y);
            self.x1 = self.x1.max(q.x);
            self.y1 = self.y1.max(q.y);
        }
    }
    pub fn union(&mut self, o: &BBox) {
        if !o.is_empty() {
            self.x0 = self.x0.min(o.x0);
            self.y0 = self.y0.min(o.y0);
            self.x1 = self.x1.max(o.x1);
            self.y1 = self.y1.max(o.y1);
        }
    }
    pub fn inflate(&self, d: f64) -> BBox {
        if self.is_empty() {
            return *self;
        }
        BBox { x0: self.x0 - d, y0: self.y0 - d, x1: self.x1 + d, y1: self.y1 + d }
    }
    pub fn intersects(&self, o: &BBox) -> bool {
        !self.is_empty() && !o.is_empty() && self.x0 <= o.x1 && o.x0 <= self.x1 && self.y0 <= o.y1 && o.y0 <= self.y1
    }
    pub fn contains_box(&self, o: &BBox) -> bool {
        !o.is_empty() && self.x0 <= o.x0 && self.y0 <= o.y0 && o.x1 <= self.x1 && o.y1 <= self.y1
    }
    pub fn contains(&self, q: P) -> bool {
        q.x >= self.x0 && q.x <= self.x1 && q.y >= self.y0 && q.y <= self.y1
    }
    pub fn w(&self) -> f64 {
        (self.x1 - self.x0).max(0.0)
    }
    pub fn h(&self) -> f64 {
        (self.y1 - self.y0).max(0.0)
    }
    /// The box of this box's corners under `m`.
    pub fn transform(&self, m: &Aff) -> BBox {
        let mut b = BBox::EMPTY;
        if self.is_empty() {
            return b;
        }
        for q in [p(self.x0, self.y0), p(self.x1, self.y0), p(self.x0, self.y1), p(self.x1, self.y1)] {
            b.add(m.apply(q));
        }
        b
    }
}

/// Elliptic arc `c + u cos t + v sin t` for `t` from `t0` to `t1` (either
/// direction). Circles, ellipses and bulge segments are exact under any
/// affine transform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EArc {
    pub c: P,
    pub u: P,
    pub v: P,
    pub t0: f64,
    pub t1: f64,
}

impl EArc {
    pub fn at(&self, t: f64) -> P {
        let (s, c) = t.sin_cos();
        p(self.c.x + self.u.x * c + self.v.x * s, self.c.y + self.u.y * c + self.v.y * s)
    }
    pub fn deriv(&self, t: f64) -> P {
        let (s, c) = t.sin_cos();
        p(-self.u.x * s + self.v.x * c, -self.u.y * s + self.v.y * c)
    }
    pub fn start(&self) -> P {
        self.at(self.t0)
    }
    pub fn end(&self) -> P {
        self.at(self.t1)
    }
    pub fn transform(&self, m: &Aff) -> EArc {
        EArc { c: m.apply(self.c), u: m.apply_v(self.u), v: m.apply_v(self.v), t0: self.t0, t1: self.t1 }
    }
    /// Upper bound of the distance from the centre.
    pub fn radius_bound(&self) -> f64 {
        self.u.len() + self.v.len()
    }
    /// Exact bounding box: endpoints plus the axis extremes inside the sweep.
    pub fn bbox(&self) -> BBox {
        let mut b = BBox::EMPTY;
        b.add(self.start());
        b.add(self.end());
        let (lo, hi) = if self.t0 <= self.t1 { (self.t0, self.t1) } else { (self.t1, self.t0) };
        if hi - lo >= TAU {
            // whole ellipse
            let ex = self.u.x.hypot(self.v.x);
            let ey = self.u.y.hypot(self.v.y);
            b.add(p(self.c.x - ex, self.c.y - ey));
            b.add(p(self.c.x + ex, self.c.y + ey));
            return b;
        }
        for base in [self.v.x.atan2(self.u.x), self.v.y.atan2(self.u.y)] {
            for k in 0..2 {
                let t = base + k as f64 * PI;
                // the first t' = t + 2πn that is >= lo
                let t = t + ((lo - t) / TAU).ceil() * TAU;
                if t <= hi {
                    b.add(self.at(t));
                }
            }
        }
        b
    }
    /// Arc length by Gauss-Legendre quadrature on pieces of at most 15°.
    pub fn length(&self) -> f64 {
        const X: [f64; 5] = [0.0, -0.538_469_310_105_683, 0.538_469_310_105_683, -0.906_179_845_938_664, 0.906_179_845_938_664];
        const W: [f64; 5] = [0.568_888_888_888_889, 0.478_628_670_499_366, 0.478_628_670_499_366, 0.236_926_885_056_189, 0.236_926_885_056_189];
        let span = self.t1 - self.t0;
        let n = ((span.abs() / (PI / 12.0)).ceil() as usize).clamp(1, 4096);
        let h = span / n as f64;
        let mut total = 0.0;
        for k in 0..n {
            let a = self.t0 + h * k as f64;
            let mid = a + h * 0.5;
            for i in 0..5 {
                total += W[i] * self.deriv(mid + X[i] * h * 0.5).len();
            }
        }
        total * h.abs() * 0.5
    }
    /// `∫ (x dy - y dx) / 2` along the arc (signed area contribution).
    pub fn area_term(&self) -> f64 {
        let (c, u, v) = (self.c, self.u, self.v);
        let (s0, c0) = self.t0.sin_cos();
        let (s1, c1) = self.t1.sin_cos();
        0.5 * (c.cross(v) * (s1 - s0) + c.cross(u) * (c1 - c0) + u.cross(v) * (self.t1 - self.t0))
    }
    /// Cubic Béziers (each at most `max_step` radians of parameter).
    pub fn to_cubics(self, max_step: f64) -> Vec<[P; 3]> {
        let span = self.t1 - self.t0;
        let n = ((span.abs() / max_step).ceil() as usize).clamp(1, 1024);
        let h = span / n as f64;
        let k = 4.0 / 3.0 * (h / 4.0).tan();
        (0..n)
            .map(|i| {
                let a = self.t0 + h * i as f64;
                let b = a + h;
                let (pa, pb) = (self.at(a), self.at(b));
                [pa.add(self.deriv(a).mul(k)), pb.sub(self.deriv(b).mul(k)), pb]
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Seg {
    M(P),
    L(P),
    C(P, P, P),
    /// Continues from the current point, which is the arc's start.
    A(EArc),
    Z,
}

pub type Path = Vec<Seg>;

/// Builds paths, inserting a line to an arc's start when the pen is elsewhere.
#[derive(Default)]
pub struct PathB {
    pub segs: Path,
    cur: Option<P>,
    start: Option<P>,
}

impl PathB {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn move_to(&mut self, q: P) {
        if q.finite() {
            self.segs.push(Seg::M(q));
            self.cur = Some(q);
            self.start = Some(q);
        }
    }
    pub fn line_to(&mut self, q: P) {
        if !q.finite() {
            return;
        }
        if self.cur.is_none() {
            return self.move_to(q);
        }
        self.segs.push(Seg::L(q));
        self.cur = Some(q);
    }
    pub fn cubic_to(&mut self, a: P, b: P, q: P) {
        if !(a.finite() && b.finite() && q.finite()) {
            return;
        }
        if self.cur.is_none() {
            return self.move_to(q);
        }
        self.segs.push(Seg::C(a, b, q));
        self.cur = Some(q);
    }
    pub fn arc(&mut self, arc: EArc) {
        let s = arc.start();
        if !(s.finite() && arc.end().finite() && arc.u.finite() && arc.v.finite()) {
            return;
        }
        match self.cur {
            None => self.move_to(s),
            Some(c) if c.dist(s) > 1e-9 * (1.0 + c.x.abs().max(c.y.abs())) => self.line_to(s),
            _ => {}
        }
        self.segs.push(Seg::A(arc));
        self.cur = Some(arc.end());
    }
    pub fn close(&mut self) {
        if self.cur.is_some() {
            self.segs.push(Seg::Z);
            self.cur = self.start;
        }
    }
    pub fn finish(self) -> Path {
        self.segs
    }
}

pub fn transform_path(path: &[Seg], m: &Aff) -> Path {
    if *m == Aff::IDENTITY {
        return path.to_vec();
    }
    path.iter()
        .map(|s| match *s {
            Seg::M(q) => Seg::M(m.apply(q)),
            Seg::L(q) => Seg::L(m.apply(q)),
            Seg::C(a, b, q) => Seg::C(m.apply(a), m.apply(b), m.apply(q)),
            Seg::A(a) => Seg::A(a.transform(m)),
            Seg::Z => Seg::Z,
        })
        .collect()
}

pub fn path_bbox(path: &[Seg]) -> BBox {
    let mut b = BBox::EMPTY;
    for s in path {
        match s {
            Seg::M(q) | Seg::L(q) => b.add(*q),
            Seg::C(x, y, q) => {
                b.add(*x);
                b.add(*y);
                b.add(*q);
            }
            Seg::A(a) => b.union(&a.bbox()),
            Seg::Z => {}
        }
    }
    b
}

/// Number of points a path carries (for size limits).
pub fn path_points(path: &[Seg]) -> usize {
    path.iter().map(|s| if matches!(s, Seg::C(..)) { 3 } else { 1 }).sum()
}

/// Object coordinate system → drawing plane (plan view) by the DXF
/// arbitrary axis algorithm. `elevation` is the OCS z of the entity.
pub fn ocs(normal: (f64, f64, f64), elevation: f64) -> Aff {
    let (nx, ny, nz) = normal;
    let len = (nx * nx + ny * ny + nz * nz).sqrt();
    if !(len.is_finite() && len > 1e-12) {
        return Aff::IDENTITY;
    }
    let (nx, ny, nz) = (nx / len, ny / len, nz / len);
    if nx.abs() < 1e-15 && ny.abs() < 1e-15 && nz > 0.0 {
        return Aff::IDENTITY;
    }
    let lim = 1.0 / 64.0;
    let (ax, ay, az) = if nx.abs() < lim && ny.abs() < lim { (nz, 0.0, -nx) } else { (-ny, nx, 0.0) };
    let al = (ax * ax + ay * ay + az * az).sqrt();
    let (ax, ay, az) = (ax / al, ay / al, az / al);
    // Ay = N × Ax
    let (bx, by) = (ny * az - nz * ay, nz * ax - nx * az);
    Aff { a: ax, b: ay, c: bx, d: by, e: elevation * nx, f: elevation * ny }
}

// ---------------------------------------------------------------- bulges, arcs

/// The arc of a polyline segment from `a` to `b` with DXF bulge `bulge`
/// (tan of a quarter of the included angle, positive counter-clockwise).
pub fn bulge_arc(a: P, b: P, bulge: f64) -> Option<EArc> {
    if bulge.abs() < 1e-12 || !bulge.is_finite() {
        return None;
    }
    let d = b.sub(a);
    let l = d.len();
    if l < 1e-12 {
        return None;
    }
    let m = a.lerp(b, 0.5);
    let k = (1.0 - bulge * bulge) / (4.0 * bulge);
    let c = p(m.x - d.y * k, m.y + d.x * k);
    let r = l * (1.0 + bulge * bulge) / (4.0 * bulge.abs());
    let t0 = (a.y - c.y).atan2(a.x - c.x);
    let theta = 4.0 * bulge.atan();
    Some(EArc { c, u: p(r, 0.0), v: p(0.0, r), t0, t1: t0 + theta })
}

/// The ellipse parameter of the point at polar angle `angle` (radians) on an
/// ellipse with minor/major `ratio`, in the same turn as `angle`.
pub fn ellipse_param(ratio: f64, angle: f64) -> f64 {
    let t = angle.sin().atan2(ratio * angle.cos());
    // keep the same number of whole turns as the angle
    t + ((angle - t) / TAU).round() * TAU
}

/// Counter-clockwise sweep `(from, to)` with `0 < to - from <= 2π` for
/// angles in radians: equal angles (mod 2π) mean a whole turn. Any input
/// works (huge or non-finite angles from files become a whole turn from 0
/// or a reduced start) and the work is constant.
pub fn ccw_sweep(start: f64, end: f64) -> (f64, f64) {
    if !(start.is_finite() && end.is_finite()) {
        return (0.0, TAU);
    }
    let s = start.rem_euclid(TAU);
    let d = (end - start).rem_euclid(TAU);
    let d = if d > 0.0 && d < TAU { d } else { TAU };
    (s, s + d)
}

/// Counter-clockwise circular arc from `start` to `end` degrees; equal
/// angles mean a whole circle.
pub fn circle_arc(c: P, r: f64, start_deg: f64, end_deg: f64) -> EArc {
    let (t0, t1) = ccw_sweep(start_deg.to_radians(), end_deg.to_radians());
    EArc { c, u: p(r, 0.0), v: p(0.0, r), t0, t1 }
}

/// An ELLIPSE's parameter range `(t0, t1)` with `0 < t1 - t0 <= 2π`.
pub fn param_sweep(start: f64, end: f64) -> (f64, f64) {
    if (end - start).abs() < 1e-12 {
        let s = if start.is_finite() { start.rem_euclid(TAU) } else { 0.0 };
        return (s, s + TAU);
    }
    ccw_sweep(start, end)
}

// ---------------------------------------------------------------- splines

fn deboor(k: usize, deg: usize, u: f64, knots: &[f64], ctrl: &[[f64; 3]]) -> [f64; 3] {
    let mut d: Vec<[f64; 3]> = (0..=deg).map(|j| ctrl[j + k - deg]).collect();
    for r in 1..=deg {
        for j in (r..=deg).rev() {
            let lo = knots[j + k - deg];
            let hi = knots[j + 1 + k - r];
            let alpha = if hi - lo != 0.0 { (u - lo) / (hi - lo) } else { 0.0 };
            let prev = d[j - 1];
            for (v, pv) in d[j].iter_mut().zip(prev) {
                *v = (1.0 - alpha) * pv + alpha * *v;
            }
        }
    }
    d[deg]
}

const MAX_SPLINE_CTRL: usize = 100_000;
const MAX_DEGREE: usize = 15;

/// A B-spline (optionally rational) as path segments: exact cubic Béziers
/// for non-rational degree ≤ 3, otherwise sampled. Invalid data → `None`.
pub fn bspline(deg: usize, knots: &[f64], ctrl: &[P], weights: &[f64]) -> Option<Path> {
    let n = ctrl.len();
    if deg == 0 || deg > MAX_DEGREE || n < deg + 1 || n > MAX_SPLINE_CTRL || ctrl.iter().any(|q| !q.finite()) {
        return None;
    }
    let knots: Vec<f64> = if knots.len() == n + deg + 1 && knots.iter().all(|k| k.is_finite()) && knots.windows(2).all(|w| w[1] >= w[0]) {
        knots.to_vec()
    } else {
        // clamped uniform
        let inner = n - deg;
        (0..n + deg + 1).map(|i| (i.saturating_sub(deg)).min(inner) as f64).collect()
    };
    let rational = weights.len() == n && weights.iter().any(|w| (w - weights[0]).abs() > 1e-12) && weights.iter().all(|w| w.is_finite() && *w > 0.0);
    let mut b = PathB::new();
    let (lo, hi) = (knots[deg], knots[n]);
    if hi.partial_cmp(&lo) != Some(std::cmp::Ordering::Greater) {
        return None;
    }
    if rational || deg > 3 {
        let pw: Vec<[f64; 3]> =
            ctrl.iter().enumerate().map(|(i, q)| { let w = if rational { weights[i] } else { 1.0 }; [q.x * w, q.y * w, w] }).collect();
        let mut first = true;
        for k in deg..n {
            let (a, z) = (knots[k], knots[k + 1]);
            if z <= a {
                continue;
            }
            let steps = 24;
            for s in 0..=steps {
                if s == 0 && !first {
                    continue;
                }
                let u = a + (z - a) * s as f64 / steps as f64;
                let h = deboor(k, deg, u, &knots, &pw);
                if h[2].abs() < 1e-300 {
                    continue;
                }
                let q = p(h[0] / h[2], h[1] / h[2]);
                if first {
                    b.move_to(q);
                    first = false;
                } else {
                    b.line_to(q);
                }
            }
        }
        return Some(b.finish());
    }
    let pts: Vec<[f64; 3]> = ctrl.iter().map(|q| [q.x, q.y, 1.0]).collect();
    // derivative control points, degree deg-1 on knots[1..len-1]
    let dk = &knots[1..knots.len() - 1];
    let dpts: Vec<[f64; 3]> = (0..n - 1)
        .map(|i| {
            let den = knots[i + deg + 1] - knots[i + 1];
            let f = if den != 0.0 { deg as f64 / den } else { 0.0 };
            [(pts[i + 1][0] - pts[i][0]) * f, (pts[i + 1][1] - pts[i][1]) * f, 0.0]
        })
        .collect();
    let mut started = false;
    for k in deg..n {
        let (a, z) = (knots[k], knots[k + 1]);
        if z <= a {
            continue;
        }
        let pa = deboor(k, deg, a, &knots, &pts);
        let pz = deboor(k, deg, z, &knots, &pts);
        let (pa, pz) = (p(pa[0], pa[1]), p(pz[0], pz[1]));
        if !started {
            b.move_to(pa);
            started = true;
        }
        if deg == 1 {
            b.line_to(pz);
            continue;
        }
        let da = deboor(k - 1, deg - 1, a, dk, &dpts);
        let dz = deboor(k - 1, deg - 1, z, dk, &dpts);
        let h = (z - a) / 3.0;
        b.cubic_to(p(pa.x + da[0] * h, pa.y + da[1] * h), p(pz.x - dz[0] * h, pz.y - dz[1] * h), pz);
    }
    Some(b.finish())
}

/// Evaluate a (non-rational) B-spline at `u`, for tests.
#[cfg(test)]
pub fn bspline_point(deg: usize, knots: &[f64], ctrl: &[P], u: f64) -> Option<P> {
    let n = ctrl.len();
    if knots.len() != n + deg + 1 || n < deg + 1 {
        return None;
    }
    let mut k = deg;
    while k + 1 < n && knots[k + 1] <= u {
        k += 1;
    }
    let pts: Vec<[f64; 3]> = ctrl.iter().map(|q| [q.x, q.y, 1.0]).collect();
    let r = deboor(k, deg, u, knots, &pts);
    Some(p(r[0], r[1]))
}

/// Catmull-Rom curve through fit points (splines stored without control
/// points), as cubic Béziers. An approximation of AutoCAD's fit splines.
pub fn fit_curve(pts: &[P], closed: bool) -> Option<Path> {
    let pts: Vec<P> = pts.iter().copied().filter(|q| q.finite()).collect();
    if pts.len() < 2 || pts.len() > MAX_SPLINE_CTRL {
        return None;
    }
    let n = pts.len();
    let get = |i: isize| -> P {
        if closed {
            pts[i.rem_euclid(n as isize) as usize]
        } else {
            pts[i.clamp(0, n as isize - 1) as usize]
        }
    };
    let mut b = PathB::new();
    b.move_to(pts[0]);
    let segs = if closed { n } else { n - 1 };
    for i in 0..segs as isize {
        let (p0, p1, p2, p3) = (get(i - 1), get(i), get(i + 1), get(i + 2));
        b.cubic_to(p1.add(p2.sub(p0).mul(1.0 / 6.0)), p2.sub(p3.sub(p1).mul(1.0 / 6.0)), p2);
    }
    if closed {
        b.close();
    }
    Some(b.finish())
}

// ---------------------------------------------------------------- flattening

/// Flattened subpath in output space.
#[derive(Clone, Debug, Default)]
pub struct Poly {
    pub pts: Vec<P>,
    pub closed: bool,
}

/// Flattens `path` with tolerance `tol` (output units). Pieces whose hull
/// is outside `cull` become chords: the result is exact inside `cull` and
/// bounded in size at any zoom. `budget` caps the number of points.
pub fn flatten(path: &[Seg], tol: f64, cull: Option<&BBox>, budget: &mut usize) -> Vec<Poly> {
    let mut out: Vec<Poly> = Vec::new();
    let mut cur = Poly::default();
    let mut pen = p(0.0, 0.0);
    let mut start = pen;
    let tol = tol.max(1e-9);
    let push = |cur: &mut Poly, q: P, budget: &mut usize| {
        if *budget > 0 {
            *budget -= 1;
            cur.pts.push(q);
        }
    };
    for s in path {
        match *s {
            Seg::M(q) => {
                if cur.pts.len() > 1 {
                    out.push(std::mem::take(&mut cur));
                }
                cur = Poly::default();
                push(&mut cur, q, budget);
                pen = q;
                start = q;
            }
            Seg::L(q) => {
                if cur.pts.is_empty() {
                    push(&mut cur, pen, budget);
                }
                push(&mut cur, q, budget);
                pen = q;
            }
            Seg::C(a, b, q) => {
                if cur.pts.is_empty() {
                    push(&mut cur, pen, budget);
                }
                cubic_rec(pen, a, b, q, tol, cull, 0, &mut cur, budget);
                pen = q;
            }
            Seg::A(arc) => {
                if cur.pts.is_empty() {
                    push(&mut cur, arc.start(), budget);
                }
                let span = arc.t1 - arc.t0;
                // arcs are built with sweeps of at most a turn; a corrupt one
                // must not loop for long either
                let pieces = ((span.abs() / (PI / 2.0)).ceil() as usize).clamp(1, 64);
                let rb = arc.radius_bound();
                for i in 0..pieces {
                    let a = arc.t0 + span * i as f64 / pieces as f64;
                    let z = arc.t0 + span * (i + 1) as f64 / pieces as f64;
                    arc_rec(&arc, a, z, arc.at(a), arc.at(z), rb, tol, cull, 0, &mut cur, budget);
                }
                pen = arc.end();
            }
            Seg::Z => {
                if !cur.pts.is_empty() {
                    cur.closed = true;
                    out.push(std::mem::take(&mut cur));
                }
                pen = start;
            }
        }
    }
    if cur.pts.len() > 1 {
        out.push(cur);
    }
    out
}

// Both subdividers stop as soon as the point budget is spent: the budget
// bounds the work, not only the output.

#[allow(clippy::too_many_arguments)]
fn cubic_rec(p0: P, p1: P, p2: P, p3: P, tol: f64, cull: Option<&BBox>, depth: u32, out: &mut Poly, budget: &mut usize) {
    if *budget == 0 {
        return;
    }
    if let Some(c) = cull {
        let mut hull = BBox::EMPTY;
        for q in [p0, p1, p2, p3] {
            hull.add(q);
        }
        if !hull.intersects(c) {
            if *budget > 0 {
                *budget -= 1;
                out.pts.push(p3);
            }
            return;
        }
    }
    let dd = p0.sub(p1.mul(2.0)).add(p2).len().max(p1.sub(p2.mul(2.0)).add(p3).len()) * 0.75;
    if dd <= tol || depth >= 24 {
        if *budget > 0 {
            *budget -= 1;
            out.pts.push(p3);
        }
        return;
    }
    let (a, b, c) = (p0.lerp(p1, 0.5), p1.lerp(p2, 0.5), p2.lerp(p3, 0.5));
    let (d, e) = (a.lerp(b, 0.5), b.lerp(c, 0.5));
    let m = d.lerp(e, 0.5);
    cubic_rec(p0, a, d, m, tol, cull, depth + 1, out, budget);
    cubic_rec(m, e, c, p3, tol, cull, depth + 1, out, budget);
}

#[allow(clippy::too_many_arguments)]
fn arc_rec(arc: &EArc, a: f64, z: f64, pa: P, pz: P, rb: f64, tol: f64, cull: Option<&BBox>, depth: u32, out: &mut Poly, budget: &mut usize) {
    if *budget == 0 {
        return;
    }
    let dev = rb * (1.0 - ((z - a) * 0.5).cos());
    let done = |out: &mut Poly, budget: &mut usize| {
        if *budget > 0 {
            *budget -= 1;
            out.pts.push(pz);
        }
    };
    if let Some(c) = cull {
        let mut b = BBox::EMPTY;
        b.add(pa);
        b.add(pz);
        if !b.inflate(dev).intersects(c) {
            return done(out, budget);
        }
    }
    if dev <= tol || depth >= 48 {
        return done(out, budget);
    }
    let m = (a + z) * 0.5;
    let pm = arc.at(m);
    arc_rec(arc, a, m, pa, pm, rb, tol, cull, depth + 1, out, budget);
    arc_rec(arc, m, z, pm, pz, rb, tol, cull, depth + 1, out, budget);
}

// ---------------------------------------------------------------- clipping

/// Liang–Barsky: the part of segment `a`–`b` inside `r`.
pub fn clip_segment(a: P, b: P, r: &BBox) -> Option<(P, P)> {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let mut t0 = 0.0f64;
    let mut t1 = 1.0f64;
    for (pp, q) in [(-dx, a.x - r.x0), (dx, r.x1 - a.x), (-dy, a.y - r.y0), (dy, r.y1 - a.y)] {
        if pp == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let t = q / pp;
            if pp < 0.0 {
                if t > t1 {
                    return None;
                }
                t0 = t0.max(t);
            } else {
                if t < t0 {
                    return None;
                }
                t1 = t1.min(t);
            }
        }
    }
    Some((p(a.x + dx * t0, a.y + dy * t0), p(a.x + dx * t1, a.y + dy * t1)))
}

/// Sutherland–Hodgman: a closed ring clipped to `r` (keeps orientation).
pub fn clip_ring(ring: &[P], r: &BBox) -> Vec<P> {
    let mut pts: Vec<P> = ring.to_vec();
    // (axis, keep >= or <=, bound)
    let edges: [(u8, bool, f64); 4] = [(0, true, r.x0), (0, false, r.x1), (1, true, r.y0), (1, false, r.y1)];
    for (axis, ge, bound) in edges {
        if pts.is_empty() {
            break;
        }
        let inside = |q: &P| {
            let v = if axis == 0 { q.x } else { q.y };
            if ge {
                v >= bound
            } else {
                v <= bound
            }
        };
        let cross = |a: &P, b: &P| {
            let (va, vb) = if axis == 0 { (a.x, b.x) } else { (a.y, b.y) };
            let t = if vb != va { (bound - va) / (vb - va) } else { 0.0 };
            a.lerp(*b, t)
        };
        let mut out = Vec::with_capacity(pts.len() + 4);
        for i in 0..pts.len() {
            let cur = pts[i];
            let prev = pts[(i + pts.len() - 1) % pts.len()];
            match (inside(&prev), inside(&cur)) {
                (true, true) => out.push(cur),
                (true, false) => out.push(cross(&prev, &cur)),
                (false, true) => {
                    out.push(cross(&prev, &cur));
                    out.push(cur);
                }
                (false, false) => {}
            }
        }
        pts = out;
    }
    pts
}

// ---------------------------------------------------------------- measures

fn cubic_length(p0: P, p1: P, p2: P, p3: P, depth: u32) -> f64 {
    let chord = p0.dist(p3);
    let net = p0.dist(p1) + p1.dist(p2) + p2.dist(p3);
    if net - chord <= 1e-10 * (1.0 + net) || depth >= 18 {
        return (2.0 * chord + net) / 3.0;
    }
    let (a, b, c) = (p0.lerp(p1, 0.5), p1.lerp(p2, 0.5), p2.lerp(p3, 0.5));
    let (d, e) = (a.lerp(b, 0.5), b.lerp(c, 0.5));
    let m = d.lerp(e, 0.5);
    cubic_length(p0, a, d, m, depth + 1) + cubic_length(m, e, c, p3, depth + 1)
}

/// Signed area contribution `∫ (x dy - y dx) / 2` of a cubic (exact).
fn cubic_area_term(p0: P, p1: P, p2: P, p3: P) -> f64 {
    // the polynomial integral, expanded in control points
    (6.0 * p0.cross(p1) + 3.0 * p0.cross(p2) + p0.cross(p3) + 3.0 * p1.cross(p2) + 3.0 * p1.cross(p3) + 6.0 * p2.cross(p3)) / 20.0
}

/// Length of every segment of a path.
pub fn path_length(path: &[Seg]) -> f64 {
    let mut pen = p(0.0, 0.0);
    let mut start = pen;
    let mut total = 0.0;
    for s in path {
        match *s {
            Seg::M(q) => {
                pen = q;
                start = q;
            }
            Seg::L(q) => {
                total += pen.dist(q);
                pen = q;
            }
            Seg::C(a, b, q) => {
                total += cubic_length(pen, a, b, q, 0);
                pen = q;
            }
            Seg::A(a) => {
                total += a.length();
                pen = a.end();
            }
            Seg::Z => {
                total += pen.dist(start);
                pen = start;
            }
        }
    }
    total
}

/// Signed areas of the closed subpaths of a path (counter-clockwise positive).
pub fn subpath_areas(path: &[Seg]) -> Vec<f64> {
    let mut out = Vec::new();
    let mut pen = p(0.0, 0.0);
    let mut start = pen;
    let mut acc = 0.0;
    let mut open = false;
    for s in path {
        match *s {
            Seg::M(q) => {
                pen = q;
                start = q;
                acc = 0.0;
                open = true;
            }
            Seg::L(q) => {
                acc += 0.5 * pen.cross(q);
                pen = q;
            }
            Seg::C(a, b, q) => {
                acc += cubic_area_term(pen, a, b, q);
                pen = q;
            }
            Seg::A(a) => {
                acc += a.area_term();
                pen = a.end();
            }
            Seg::Z => {
                if open {
                    acc += 0.5 * pen.cross(start);
                    out.push(acc);
                }
                acc = 0.0;
                pen = start;
                open = false;
            }
        }
    }
    out
}

/// Even-odd point in polygon.
pub fn point_in_ring(q: P, ring: &[P]) -> bool {
    let mut inside = false;
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let (a, b) = (ring[i], ring[j]);
        if (a.y > q.y) != (b.y > q.y) && q.x < (b.x - a.x) * (q.y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Distance from `q` to segment `a`–`b`.
pub fn seg_dist(q: P, a: P, b: P) -> f64 {
    let d = b.sub(a);
    let l2 = d.dot(d);
    if l2 <= 0.0 {
        return q.dist(a);
    }
    let t = (q.sub(a).dot(d) / l2).clamp(0.0, 1.0);
    q.dist(a.add(d.mul(t)))
}

/// Area of a fill made of several rings with the even-odd rule: rings nested
/// at even depth add, at odd depth subtract.
pub fn even_odd_area(rings: &[(Vec<P>, f64)]) -> f64 {
    let mut total = 0.0;
    for (i, (ring, area)) in rings.iter().enumerate() {
        let Some(&sample) = ring.first() else { continue };
        let depth = rings.iter().enumerate().filter(|(j, (other, _))| *j != i && point_in_ring(sample, other)).count();
        if depth % 2 == 0 {
            total += area.abs();
        } else {
            total -= area.abs();
        }
    }
    total.max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn affine_compose_and_invert() {
        let m = Aff::translate(5.0, -2.0).mul(Aff::rotate(0.3)).mul(Aff::scale(2.0, 3.0));
        let q = p(1.5, -4.0);
        let back = m.inverse().unwrap().apply(m.apply(q));
        assert!(close(back.x, q.x, 1e-12) && close(back.y, q.y, 1e-12));
        assert!(close(Aff::scale(2.0, 3.0).max_scale(), 3.0, 1e-12));
    }

    #[test]
    fn ocs_mirror_and_identity() {
        assert_eq!(ocs((0.0, 0.0, 1.0), 5.0), Aff::IDENTITY);
        let m = ocs((0.0, 0.0, -1.0), 0.0);
        let q = m.apply(p(3.0, 4.0));
        assert!(close(q.x, -3.0, 1e-12) && close(q.y, 4.0, 1e-12), "{q:?}");
    }

    #[test]
    fn bulge_semicircle() {
        // bulge 1 = half circle, counter-clockwise from (0,0) to (2,0): passes below
        let a = bulge_arc(p(0.0, 0.0), p(2.0, 0.0), 1.0).unwrap();
        assert!(close(a.c.x, 1.0, 1e-12) && close(a.c.y, 0.0, 1e-12));
        assert!(close(a.u.x, 1.0, 1e-12));
        let mid = a.at((a.t0 + a.t1) * 0.5);
        assert!(close(mid.x, 1.0, 1e-12) && close(mid.y, -1.0, 1e-12), "{mid:?}");
        assert!(close(a.end().x, 2.0, 1e-12));
        assert!(close(a.length(), PI, 1e-9));
        // negative bulge goes above
        let b = bulge_arc(p(0.0, 0.0), p(2.0, 0.0), -1.0).unwrap();
        assert!(close(b.at((b.t0 + b.t1) * 0.5).y, 1.0, 1e-12));
    }

    #[test]
    fn arc_bbox_and_area() {
        let c = circle_arc(p(1.0, 2.0), 3.0, 0.0, 360.0);
        let b = c.bbox();
        assert!(close(b.x0, -2.0, 1e-9) && close(b.y1, 5.0, 1e-9));
        let quarter = circle_arc(p(0.0, 0.0), 1.0, 0.0, 90.0);
        let b = quarter.bbox();
        assert!(close(b.x0, 0.0, 1e-12) && close(b.x1, 1.0, 1e-12) && close(b.y1, 1.0, 1e-12));
        // a whole circle path: area π r²
        let mut pb = PathB::new();
        pb.arc(c);
        pb.close();
        let areas = subpath_areas(&pb.finish());
        assert!(close(areas[0], PI * 9.0, 1e-9), "{areas:?}");
    }

    #[test]
    fn cubic_area_is_exact() {
        // unit circle from four cubics: area close to π
        let c = circle_arc(p(0.0, 0.0), 1.0, 0.0, 360.0);
        let mut pb = PathB::new();
        pb.move_to(c.start());
        for [a, b, q] in c.to_cubics(PI / 2.0) {
            pb.cubic_to(a, b, q);
        }
        pb.close();
        let a = subpath_areas(&pb.finish())[0];
        assert!(close(a, PI, 2e-3), "{a}");
    }

    #[test]
    fn spline_beziers_match_de_boor() {
        let ctrl = [p(0.0, 0.0), p(1.0, 2.0), p(3.0, 3.0), p(4.0, 0.0), p(6.0, 1.0), p(7.0, 4.0)];
        let knots = [0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 3.0, 3.0, 3.0];
        let path = bspline(3, &knots, &ctrl, &[]).unwrap();
        // walk the Bézier segments and compare midpoints with de Boor
        let mut pen = match path[0] {
            Seg::M(q) => q,
            _ => panic!(),
        };
        let mut span = 0.0;
        for s in &path[1..] {
            if let Seg::C(a, b, q) = *s {
                let mid = pen.mul(0.125).add(a.mul(0.375)).add(b.mul(0.375)).add(q.mul(0.125));
                let want = bspline_point(3, &knots, &ctrl, span + 0.5).unwrap();
                assert!(mid.dist(want) < 1e-9, "{mid:?} vs {want:?}");
                pen = q;
                span += 1.0;
            }
        }
        assert_eq!(span, 3.0);
        assert!(pen.dist(p(7.0, 4.0)) < 1e-12);
    }

    #[test]
    fn flatten_culls_far_geometry() {
        // a huge circle seen through a small window stays cheap
        let c = circle_arc(p(0.0, 0.0), 1.0e7, 0.0, 360.0);
        let mut pb = PathB::new();
        pb.arc(c);
        let path = pb.finish();
        let window = BBox::new(1.0e7 - 100.0, -100.0, 1.0e7 + 100.0, 100.0);
        let mut budget = 1_000_000;
        let polys = flatten(&path, 0.25, Some(&window), &mut budget);
        let n: usize = polys.iter().map(|p| p.pts.len()).sum();
        assert!(n < 2000, "{n} points");
        // the points inside the window are on the circle within tolerance
        for q in polys.iter().flat_map(|p| p.pts.iter()).filter(|q| window.contains(**q)) {
            assert!((q.len() - 1.0e7).abs() < 0.3);
        }
    }

    #[test]
    fn sweeps_of_any_angle_are_bounded() {
        let t = std::time::Instant::now();
        for (s, e) in [(1e300, 0.0), (0.0, 1e300), (f64::INFINITY, 0.0), (f64::NAN, 5.0), (-1e18, 1e18), (370.0, 10.0)] {
            let a = circle_arc(p(0.0, 0.0), 1.0, s, e);
            let span = a.t1 - a.t0;
            assert!(span > 0.0 && span <= TAU + 1e-9 && a.t0.is_finite(), "{s} {e}: {a:?}");
            let (t0, t1) = param_sweep(s, e);
            assert!(t1 - t0 > 0.0 && t1 - t0 <= TAU + 1e-9);
        }
        assert!(t.elapsed().as_millis() < 100);
        // the usual cases keep their meaning
        let q = circle_arc(p(0.0, 0.0), 1.0, 0.0, 90.0);
        assert!(close(q.t1 - q.t0, PI / 2.0, 1e-12));
        let full = circle_arc(p(0.0, 0.0), 1.0, 30.0, 30.0);
        assert!(close(full.t1 - full.t0, TAU, 1e-12));
        let wrap = circle_arc(p(0.0, 0.0), 1.0, 350.0, 10.0);
        assert!(close(wrap.t1 - wrap.t0, 20f64.to_radians(), 1e-12));
        let (a, b) = param_sweep(1.0, 0.5);
        assert!(close(b - a, TAU - 0.5, 1e-12));
    }

    #[test]
    fn subdivision_stops_with_the_budget() {
        // tolerance far below what the radius can resolve, no culling: the
        // budget must stop the recursion, not only the output
        let c = circle_arc(p(0.0, 0.0), 1.0e12, 0.0, 360.0);
        let mut pb = PathB::new();
        pb.arc(c);
        pb.cubic_to(p(0.0, 1e12), p(1e12, 1e12), p(1e12, 0.0));
        let path = pb.finish();
        let t = std::time::Instant::now();
        let mut budget = 10_000;
        let polys = flatten(&path, 1e-9, None, &mut budget);
        assert_eq!(budget, 0);
        assert!(polys.iter().map(|p| p.pts.len()).sum::<usize>() <= 10_000);
        assert!(t.elapsed().as_millis() < 500, "{:?}", t.elapsed());
    }

    #[test]
    fn similarity() {
        assert_eq!(Aff::rotate(0.7).mul(Aff::scale(2.0, 2.0)).similarity_scale().map(|s| (s * 1e9).round() / 1e9), Some(2.0));
        assert_eq!(Aff::scale(-3.0, 3.0).similarity_scale(), Some(3.0));
        assert_eq!(Aff::scale(1.0, 2.0).similarity_scale(), None);
        assert_eq!(Aff::rotate(0.3).mul(Aff::scale(1.0, 2.0)).similarity_scale(), None);
    }

    #[test]
    fn clipping() {
        let r = BBox::new(0.0, 0.0, 10.0, 10.0);
        let (a, b) = clip_segment(p(-5.0, 5.0), p(15.0, 5.0), &r).unwrap();
        assert_eq!((a.x, b.x), (0.0, 10.0));
        assert!(clip_segment(p(-5.0, -5.0), p(-1.0, 20.0), &r).is_none());
        let ring = clip_ring(&[p(-5.0, -5.0), p(15.0, -5.0), p(15.0, 15.0), p(-5.0, 15.0)], &r);
        let area: f64 = (0..ring.len()).map(|i| ring[i].cross(ring[(i + 1) % ring.len()])).sum::<f64>() * 0.5;
        assert!(close(area, 100.0, 1e-9));
    }
}
