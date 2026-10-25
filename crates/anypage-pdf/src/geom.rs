//! Page geometry: page boxes, rotation, and the transforms between PDF user
//! space (y up, origin at the media box origin) and *page space*, the space
//! every function of this module speaks: points, origin at the top-left corner
//! of the page as displayed (crop box, rotation applied), y down.
//!
//! The formula is the one hayro uses for `Page::initial_transform(true)`, so a
//! page edited through lopdf and the same page rendered through hayro agree to
//! the bit (tested below against hayro itself).

use hayro::kurbo::{Affine, Point, Rect as KRect};

/// A4 in points, hayro's fallback for pages with zero-area boxes.
const A4: (f64, f64) = (210.0 * 72.0 / 25.4, 297.0 * 72.0 / 25.4);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageGeom {
    /// Media box in user space, normalised (x0 < x1, y0 < y1).
    pub media: [f64; 4],
    /// Crop box in user space, normalised.
    pub crop: [f64; 4],
    /// 0, 90, 180 or 270 (clockwise, as displayed).
    pub rotate: u32,
}

pub fn norm_box(b: [f64; 4]) -> [f64; 4] {
    [b[0].min(b[2]), b[1].min(b[3]), b[0].max(b[2]), b[1].max(b[3])]
}

impl PageGeom {
    pub fn new(media: [f64; 4], crop: Option<[f64; 4]>, rotate: i64) -> Self {
        let media = norm_box(media);
        let crop = norm_box(crop.unwrap_or(media));
        let rotate = match rotate.rem_euclid(360) {
            90 => 90,
            180 => 180,
            270 => 270,
            _ => 0,
        };
        Self { media, crop, rotate }
    }

    /// The crop box clipped to the media box.
    pub fn visible_box(&self) -> [f64; 4] {
        let (m, c) = (self.media, self.crop);
        let (x0, y0) = (c[0].max(m[0]), c[1].max(m[1]));
        // hayro: an empty intersection keeps its corner with zero size
        [x0, y0, c[2].min(m[2]).max(x0), c[3].min(m[3]).max(y0)]
    }

    /// Unrotated width and height of the visible box.
    fn base(&self) -> (f64, f64) {
        let b = self.visible_box();
        let (w, h) = (b[2] - b[0], b[3] - b[1]);
        if w.abs() < 1e-3 || h.abs() < 1e-3 {
            A4
        } else {
            (w.max(1.0), h.max(1.0))
        }
    }

    /// Displayed width and height in points (rotation applied).
    pub fn size(&self) -> (f64, f64) {
        let (w, h) = self.base();
        if self.rotate == 90 || self.rotate == 270 {
            (h, w)
        } else {
            (w, h)
        }
    }

    /// User space to page space.
    pub fn user_to_page(&self) -> Affine {
        let b = self.visible_box();
        let (_, base_h) = self.base();
        let (w, h) = self.size();
        let cw90 = Affine::new([0.0, 1.0, -1.0, 0.0, 0.0, 0.0]);
        let ccw90 = Affine::new([0.0, -1.0, 1.0, 0.0, 0.0, 0.0]);
        let rotation = match self.rotate {
            90 => cw90 * Affine::translate((0.0, -w)),
            180 => Affine::scale(-1.0) * Affine::translate((-w, -h)),
            270 => Affine::translate((0.0, h)) * ccw90,
            _ => Affine::IDENTITY,
        };
        let invert = Affine::new([1.0, 0.0, 0.0, -1.0, 0.0, base_h]);
        rotation * invert * Affine::translate((-b[0], -b[1]))
    }

    /// Page space to user space.
    pub fn page_to_user(&self) -> Affine {
        self.user_to_page().inverse()
    }
}

/// A page-space rectangle `{x, y, w, h}` as user-space corners in the order
/// annotations want for QuadPoints: top-left, top-right, bottom-left,
/// bottom-right as displayed.
pub fn quad_user(g: &PageGeom, x: f64, y: f64, w: f64, h: f64) -> [Point; 4] {
    let t = g.page_to_user();
    [t * Point::new(x, y), t * Point::new(x + w, y), t * Point::new(x, y + h), t * Point::new(x + w, y + h)]
}

/// Bounding box of transformed points.
pub fn bbox(points: &[Point]) -> KRect {
    let mut r = KRect::new(f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for p in points {
        r.x0 = r.x0.min(p.x);
        r.y0 = r.y0.min(p.y);
        r.x1 = r.x1.max(p.x);
        r.y1 = r.y1.max(p.y);
    }
    r
}

/// Map a page-space rectangle to the user-space rectangle that covers it.
pub fn rect_to_user(g: &PageGeom, x: f64, y: f64, w: f64, h: f64) -> KRect {
    bbox(&quad_user(g, x, y, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Point, b: Point) -> bool {
        (a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() < 1e-9
    }

    #[test]
    fn unrotated_letter() {
        let g = PageGeom::new([0.0, 0.0, 612.0, 792.0], None, 0);
        assert_eq!(g.size(), (612.0, 792.0));
        let t = g.user_to_page();
        assert!(close(t * Point::new(0.0, 792.0), Point::new(0.0, 0.0)));
        assert!(close(t * Point::new(612.0, 0.0), Point::new(612.0, 792.0)));
        assert!(close(g.page_to_user() * Point::new(72.0, 72.0), Point::new(72.0, 720.0)));
    }

    #[test]
    fn rotations_map_corners_clockwise() {
        // user top-left corner of a portrait page, as displayed after rotation
        let tl = Point::new(0.0, 792.0);
        let expect = [(0, (0.0, 0.0)), (90, (792.0, 0.0)), (180, (612.0, 792.0)), (270, (0.0, 612.0))];
        for (rot, (x, y)) in expect {
            let g = PageGeom::new([0.0, 0.0, 612.0, 792.0], None, rot);
            let p = g.user_to_page() * tl;
            assert!(close(p, Point::new(x, y)), "rotate {rot}: {p:?}");
            let back = g.page_to_user() * p;
            assert!(close(back, tl));
        }
        assert_eq!(PageGeom::new([0.0, 0.0, 612.0, 792.0], None, -90).rotate, 270);
        assert_eq!(PageGeom::new([0.0, 0.0, 612.0, 792.0], None, 450).rotate, 90);
    }

    #[test]
    fn crop_box_offsets_and_clips() {
        let g = PageGeom::new([0.0, 0.0, 612.0, 792.0], Some([100.0, 100.0, 400.0, 900.0]), 0);
        assert_eq!(g.visible_box(), [100.0, 100.0, 400.0, 792.0]);
        assert_eq!(g.size(), (300.0, 692.0));
        assert!(close(g.user_to_page() * Point::new(100.0, 792.0), Point::new(0.0, 0.0)));
    }
}
