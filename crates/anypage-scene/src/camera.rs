//! The orbit camera scripts hold as a plain table:
//! `{ yaw, pitch, distance, target = {x, y, z}, fov, up = "y" | "z", ortho, near, far }`.
//!
//! Angles are degrees. `yaw = 0, pitch = 0` looks at the front of the
//! model: from +Z toward -Z when Y is up, from -Y toward +Y when Z is up
//! (CAD and survey data). Positive pitch looks down from above. `fov` is the
//! vertical field of view; with `ortho = true` the same `distance` and `fov`
//! give the size of the orthographic view, so switching keeps the framing.

use glam::{DMat4, DVec3, DVec4};

use anypage_runtime::Data;

use crate::model::Bounds;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Up {
    Y,
    Z,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Camera {
    pub yaw: f64,
    pub pitch: f64,
    pub distance: f64,
    pub target: [f64; 3],
    pub fov: f64,
    pub up: Up,
    pub ortho: bool,
    pub near: Option<f64>,
    pub far: Option<f64>,
}

impl Default for Camera {
    fn default() -> Self {
        Self { yaw: 30.0, pitch: 25.0, distance: 10.0, target: [0.0; 3], fov: 45.0, up: Up::Y, ortho: false, near: None, far: None }
    }
}

pub const MAX_PITCH: f64 = 89.9;
const MIN_DISTANCE: f64 = 1e-9;
const MAX_DISTANCE: f64 = 1e12;

/// `{x, y, z}` or `{1, 2, 3}`.
pub fn vec3_of(d: &Data) -> Option<[f64; 3]> {
    let v = match d {
        Data::Map(_) => [d.get("x").num()?, d.get("y").num()?, d.get("z").num()?],
        Data::List(l) if l.len() == 3 => [l[0].num()?, l[1].num()?, l[2].num()?],
        _ => return None,
    };
    v.iter().all(|x| x.is_finite()).then_some(v)
}

pub fn vec3_data(v: [f64; 3]) -> Data {
    Data::map([("x", Data::Num(v[0])), ("y", Data::Num(v[1])), ("z", Data::Num(v[2]))])
}

fn finite(d: &Data, key: &str) -> Result<Option<f64>, String> {
    match d.get(key) {
        Data::Nil => Ok(None),
        Data::Num(n) if n.is_finite() => Ok(Some(*n)),
        other => Err(format!("camera.{key} must be a number, got {other:?}")),
    }
}

impl Camera {
    /// Parse a camera table; missing fields take defaults. Also returns
    /// whether the table placed the camera itself (`distance` given): when it
    /// did not, the layer frames the whole scene.
    pub fn from_data(d: &Data) -> Result<(Camera, bool), String> {
        let mut c = Camera::default();
        match d {
            Data::Nil => return Ok((c, false)),
            Data::Map(_) => {}
            other => return Err(format!("a camera must be a table, got {other:?}")),
        }
        if let Some(v) = finite(d, "yaw")? {
            c.yaw = v;
        }
        if let Some(v) = finite(d, "pitch")? {
            c.pitch = v.clamp(-MAX_PITCH, MAX_PITCH);
        }
        let placed = match finite(d, "distance")? {
            Some(v) => {
                c.distance = v.abs().clamp(MIN_DISTANCE, MAX_DISTANCE);
                true
            }
            None => false,
        };
        if let Some(v) = finite(d, "fov")? {
            c.fov = v.clamp(1.0, 170.0);
        }
        match d.get("target") {
            Data::Nil => {}
            t => c.target = vec3_of(t).ok_or("camera.target must be {x, y, z}")?,
        }
        c.up = match d.get("up").as_str() {
            None | Some("y") | Some("Y") => Up::Y,
            Some("z") | Some("Z") => Up::Z,
            Some(o) => return Err(format!("camera.up must be \"y\" or \"z\", not \"{o}\"")),
        };
        c.ortho = d.get("ortho").truthy();
        c.near = finite(d, "near")?.filter(|v| *v > 0.0 || c.ortho);
        c.far = finite(d, "far")?;
        Ok((c, placed))
    }

    /// The camera as a table, keeping any extra keys `base` had.
    pub fn to_data(&self, base: &Data) -> Data {
        let mut m: std::collections::BTreeMap<std::sync::Arc<str>, Data> = match base {
            Data::Map(m) => (**m).clone(),
            _ => Default::default(),
        };
        let mut set = |k: &str, v: Data| {
            m.insert(k.into(), v);
        };
        set("yaw", Data::Num(self.yaw));
        set("pitch", Data::Num(self.pitch));
        set("distance", Data::Num(self.distance));
        set("target", vec3_data(self.target));
        set("fov", Data::Num(self.fov));
        set("up", Data::str(if self.up == Up::Z { "z" } else { "y" }));
        set("ortho", Data::Bool(self.ortho));
        Data::Map(std::sync::Arc::new(m))
    }

    pub fn up_vec(&self) -> DVec3 {
        match self.up {
            Up::Y => DVec3::Y,
            Up::Z => DVec3::Z,
        }
    }

    /// Unit vector from the target to the eye.
    pub fn offset_dir(&self) -> DVec3 {
        let (y, p) = (self.yaw.to_radians(), self.pitch.clamp(-MAX_PITCH, MAX_PITCH).to_radians());
        match self.up {
            Up::Y => DVec3::new(p.cos() * y.sin(), p.sin(), p.cos() * y.cos()),
            Up::Z => DVec3::new(p.cos() * y.sin(), -p.cos() * y.cos(), p.sin()),
        }
    }

    pub fn eye(&self) -> DVec3 {
        DVec3::from(self.target) + self.offset_dir() * self.distance
    }

    pub fn view(&self) -> DMat4 {
        DMat4::look_at_rh(self.eye(), DVec3::from(self.target), self.up_vec())
    }

    /// Near and far planes that hold `bounds` (unless the camera fixes them).
    pub fn clip_planes(&self, bounds: &Bounds) -> (f64, f64) {
        let (near, far) = if bounds.is_empty() {
            (self.distance * 0.01, self.distance * 100.0)
        } else {
            let c = DVec3::from(bounds.center());
            let r = bounds.radius().max(1e-9) * 1.05;
            // distance along the view axis, not to the centre: the sphere's extent in depth
            let eye = self.eye();
            let fwd = (DVec3::from(self.target) - eye).normalize_or_zero();
            let d = (c - eye).dot(fwd);
            let far = (d + r).max(1e-6);
            let near = if self.ortho { d - r } else { (d - r).max(far * 1e-4) };
            (near, far)
        };
        let near = self.near.unwrap_or(near);
        let far = self.far.unwrap_or(far).max(near + 1e-9);
        (near, far)
    }

    /// Projection with [0, 1] depth for wgpu.
    pub fn proj(&self, aspect: f64, near: f64, far: f64) -> DMat4 {
        let aspect = if aspect.is_finite() && aspect > 0.0 { aspect } else { 1.0 };
        if self.ortho {
            let hh = self.distance * (self.fov.to_radians() * 0.5).tan();
            let hw = hh * aspect;
            DMat4::orthographic_rh(-hw, hw, -hh, hh, near, far)
        } else {
            DMat4::perspective_rh(self.fov.to_radians(), aspect, near.max(1e-12), far)
        }
    }

    /// Drag by (`dx`, `dy`) units at `speed` degrees per unit: right turns
    /// the model right, down tilts its top toward the viewer.
    pub fn orbit(&mut self, dx: f64, dy: f64, speed: f64) {
        self.yaw = (self.yaw - dx * speed + 180.0).rem_euclid(360.0) - 180.0;
        self.pitch = (self.pitch + dy * speed).clamp(-MAX_PITCH, MAX_PITCH);
    }

    /// Multiply the distance (`factor < 1` moves closer).
    pub fn zoom(&mut self, factor: f64) {
        if factor.is_finite() && factor > 0.0 {
            self.distance = (self.distance * factor).clamp(MIN_DISTANCE, MAX_DISTANCE);
        }
    }

    /// Move the target so the content follows a drag of (`dx`, `dy`) pixels
    /// in a view `view_h` pixels tall.
    pub fn pan(&mut self, dx: f64, dy: f64, view_h: f64) {
        let per_px = 2.0 * self.distance * (self.fov.to_radians() * 0.5).tan() / view_h.max(1.0);
        let fwd = -self.offset_dir();
        let right = fwd.cross(self.up_vec()).normalize_or_zero();
        let up = right.cross(fwd).normalize_or_zero();
        let t = DVec3::from(self.target) - right * dx * per_px + up * dy * per_px;
        if t.is_finite() {
            self.target = t.to_array();
        }
    }

    /// Aim at the centre of `bounds` from a distance that shows all of it in
    /// a view of `aspect` (width / height), with `margin` (1.1 = 10%) around.
    pub fn fit(&mut self, bounds: &Bounds, aspect: f64, margin: f64) {
        if bounds.is_empty() {
            return;
        }
        self.target = bounds.center();
        let r = bounds.radius().max(1e-6) * margin.max(1.0);
        let half_v = (self.fov.to_radians() * 0.5).clamp(1e-3, 1.5);
        let aspect = if aspect.is_finite() && aspect > 0.0 { aspect } else { 1.0 };
        let half_h = (half_v.tan() * aspect).atan();
        let half = half_v.min(half_h);
        self.distance = if self.ortho { r / half.tan() } else { r / half.sin() }.clamp(MIN_DISTANCE, MAX_DISTANCE);
    }

    /// Where `p` lands in a `w` x `h` view: pixels from the top left, the
    /// distance in front of the camera, and whether it is in front at all.
    pub fn project(&self, p: [f64; 3], w: f64, h: f64) -> (f64, f64, f64, bool) {
        let view = self.view();
        let proj = self.proj(w / h.max(1e-9), self.distance * 1e-3, self.distance * 1e3);
        let v = view * DVec4::new(p[0], p[1], p[2], 1.0);
        let c = proj * v;
        let depth = -v.z;
        if c.w.abs() < 1e-300 {
            return (f64::NAN, f64::NAN, depth, false);
        }
        let (nx, ny) = (c.x / c.w, c.y / c.w);
        let x = (nx * 0.5 + 0.5) * w;
        let y = (0.5 - ny * 0.5) * h;
        (x, y, depth, self.ortho || depth > 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_round_trip() {
        let (c, placed) = Camera::from_data(&Data::Nil).unwrap();
        assert!(!placed);
        assert_eq!(c, Camera::default());
        let d = Data::map([("yaw", Data::Num(10.0)), ("distance", Data::Num(5.0)), ("target", Data::nums([1.0, 2.0, 3.0])), ("extra", Data::str("kept"))]);
        let (c, placed) = Camera::from_data(&d).unwrap();
        assert!(placed);
        assert_eq!(c.target, [1.0, 2.0, 3.0]);
        let back = c.to_data(&d);
        assert_eq!(back.get("extra").as_str(), Some("kept"));
        assert_eq!(Camera::from_data(&back).unwrap().0, c);
        assert!(Camera::from_data(&Data::map([("up", Data::str("x"))])).is_err());
        assert!(Camera::from_data(&Data::map([("yaw", Data::str("x"))])).is_err());
    }

    #[test]
    fn front_view_looks_down_negative_z() {
        let c = Camera { yaw: 0.0, pitch: 0.0, distance: 5.0, ..Default::default() };
        assert!((c.eye() - DVec3::new(0.0, 0.0, 5.0)).length() < 1e-12);
        let (x, y, depth, vis) = c.project([0.0, 0.0, 0.0], 200.0, 100.0);
        assert!((x - 100.0).abs() < 1e-9 && (y - 50.0).abs() < 1e-9 && (depth - 5.0).abs() < 1e-9 && vis);
        // +x is to the right, +y up (smaller pixel y)
        let (x, y, _, _) = c.project([1.0, 1.0, 0.0], 200.0, 100.0);
        assert!(x > 100.0 && y < 50.0);
        // z up: the front view looks along +Y, x to the right, z up
        let c = Camera { yaw: 0.0, pitch: 0.0, distance: 5.0, up: Up::Z, ..Default::default() };
        assert!((c.eye() - DVec3::new(0.0, -5.0, 0.0)).length() < 1e-12);
        let (x, y, _, _) = c.project([1.0, 0.0, 1.0], 200.0, 100.0);
        assert!(x > 100.0 && y < 50.0);
    }

    #[test]
    fn orbit_zoom_pan_fit() {
        let mut c = Camera { yaw: 170.0, pitch: 80.0, ..Default::default() };
        c.orbit(-40.0, 40.0, 0.5);
        assert!((c.yaw - -170.0).abs() < 1e-9, "{}", c.yaw);
        assert_eq!(c.pitch, MAX_PITCH);
        c.zoom(0.5);
        assert_eq!(c.distance, 5.0);
        c.zoom(-1.0);
        assert_eq!(c.distance, 5.0);
        let mut c = Camera { yaw: 0.0, pitch: 0.0, distance: 10.0, ..Default::default() };
        c.pan(100.0, 0.0, 100.0);
        assert!(c.target[0] < 0.0 && c.target[1].abs() < 1e-12);
        let b = Bounds::new([9.0, -1.0, -1.0], [11.0, 1.0, 1.0]);
        c.fit(&b, 1.0, 1.0);
        assert_eq!(c.target, [10.0, 0.0, 0.0]);
        // every corner of the box is inside the view
        for i in 0..8 {
            let p = [if i & 1 == 0 { 9.0 } else { 11.0 }, if i & 2 == 0 { -1.0 } else { 1.0 }, if i & 4 == 0 { -1.0 } else { 1.0 }];
            let (x, y, _, vis) = c.project(p, 100.0, 100.0);
            assert!(vis && (0.0..=100.0).contains(&x) && (0.0..=100.0).contains(&y), "{p:?} -> {x} {y}");
        }
        let (n, f) = c.clip_planes(&b);
        assert!(n > 0.0 && n < c.distance - 1.0 && f > c.distance + 1.0);
    }
}
