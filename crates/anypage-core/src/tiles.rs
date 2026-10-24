//! Tile addressing shared by every `Tiled` node source (ARCHITECTURE.md §8).
//!
//! World units are PDF points. A tile at `level` is rasterised at `2^level`
//! device pixels per point and therefore covers `TILE_SIZE / 2^level` points on
//! each side. Tiles are addressed per page, from the page's top-left corner.

pub const TILE_SIZE: u32 = 256;
pub const MIN_LEVEL: i32 = -6;
pub const MAX_LEVEL: i32 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    pub page: u32,
    pub level: i32,
    pub tx: i32,
    pub ty: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w * 0.5, self.y + self.h * 0.5)
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
    pub fn intersection(&self, o: &Rect) -> Option<Rect> {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        if r > x && b > y {
            Some(Rect::new(x, y, r - x, b - y))
        } else {
            None
        }
    }
    pub fn inflate(&self, d: f32) -> Rect {
        Rect::new(self.x - d, self.y - d, self.w + 2.0 * d, self.h + 2.0 * d)
    }
}

/// Where each page sits in world space.
#[derive(Clone, Debug, Default)]
pub struct PageLayout {
    pub pages: Vec<Rect>,
    pub bounds: Rect,
}

impl PageLayout {
    /// Pages stacked top to bottom with `gap` between them, horizontally centred
    /// on the widest page.
    pub fn vertical(sizes: &[(f32, f32)], gap: f32) -> Self {
        let max_w = sizes.iter().map(|s| s.0).fold(0.0f32, f32::max);
        let mut y = 0.0;
        let mut pages = Vec::with_capacity(sizes.len());
        for &(w, h) in sizes {
            pages.push(Rect::new((max_w - w) * 0.5, y, w, h));
            y += h + gap;
        }
        let total_h = if sizes.is_empty() { 0.0 } else { y - gap };
        Self { pages, bounds: Rect::new(0.0, 0.0, max_w, total_h) }
    }
}

/// Device pixels per point at `level`.
pub fn level_scale(level: i32) -> f32 {
    2f32.powi(level)
}

/// The tile level to display for a camera scale: ⌊log₂(scale)⌉, clamped.
pub fn level_for_scale(scale: f32) -> i32 {
    (scale.max(f32::MIN_POSITIVE).log2().round() as i32).clamp(MIN_LEVEL, MAX_LEVEL)
}

/// World-space side length of one tile at `level`.
pub fn tile_world_size(level: i32) -> f32 {
    TILE_SIZE as f32 / level_scale(level)
}

/// `(columns, rows)` of tiles needed to cover `page` at `level`.
pub fn tile_grid(page: &Rect, level: i32) -> (i32, i32) {
    let t = tile_world_size(level);
    ((page.w / t).ceil().max(1.0) as i32, (page.h / t).ceil().max(1.0) as i32)
}

pub fn tile_world_rect(layout: &PageLayout, key: TileKey) -> Rect {
    let page = layout.pages[key.page as usize];
    let t = tile_world_size(key.level);
    Rect::new(page.x + key.tx as f32 * t, page.y + key.ty as f32 * t, t, t)
}

/// Every tile at `level` whose world rect intersects `view`, page by page,
/// row-major within a page.
pub fn visible_tiles(layout: &PageLayout, view: &Rect, level: i32, out: &mut Vec<TileKey>) {
    let t = tile_world_size(level);
    for (pi, page) in layout.pages.iter().enumerate() {
        let Some(vis) = page.intersection(view) else { continue };
        let (cols, rows) = tile_grid(page, level);
        let tx0 = (((vis.x - page.x) / t).floor() as i32).clamp(0, cols - 1);
        let tx1 = (((vis.right() - page.x) / t).ceil() as i32).clamp(1, cols);
        let ty0 = (((vis.y - page.y) / t).floor() as i32).clamp(0, rows - 1);
        let ty1 = (((vis.bottom() - page.y) / t).ceil() as i32).clamp(1, rows);
        for ty in ty0..ty1 {
            for tx in tx0..tx1 {
                out.push(TileKey { page: pi as u32, level, tx, ty });
            }
        }
    }
}

/// Coarsest level worth keeping resident: the largest page fits in one tile.
/// These tiles are cheap, pinned, and are what the fallback path draws while
/// finer tiles are still being rasterised ("縮放永不空白").
pub fn overview_level(layout: &PageLayout) -> i32 {
    let max_dim = layout.pages.iter().map(|p| p.w.max(p.h)).fold(1.0f32, f32::max);
    ((TILE_SIZE as f32 / max_dim).log2().floor() as i32).clamp(MIN_LEVEL, 0)
}

/// The tile `d` levels coarser that contains `key`, and the sub-rectangle
/// `[u, v, w, h]` (0..1) of that coarser tile which corresponds to `key`.
pub fn ancestor(key: TileKey, d: i32) -> (TileKey, [f32; 4]) {
    debug_assert!(d > 0);
    let n = 1i32 << d;
    let ax = key.tx.div_euclid(n);
    let ay = key.ty.div_euclid(n);
    let inv = 1.0 / n as f32;
    (
        TileKey { page: key.page, level: key.level - d, tx: ax, ty: ay },
        [(key.tx - ax * n) as f32 * inv, (key.ty - ay * n) as f32 * inv, inv, inv],
    )
}

/// Affine `[a, b, c, d, e, f]` mapping page space (points, origin top-left,
/// y down) to this tile's pixel space. Handed to the L2 rasteriser as-is.
pub fn tile_matrix(key: TileKey) -> [f32; 6] {
    let s = level_scale(key.level);
    let t = TILE_SIZE as f32;
    [s, 0.0, 0.0, s, -(key.tx as f32) * t, -(key.ty as f32) * t]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn letter(n: usize) -> PageLayout {
        PageLayout::vertical(&vec![(612.0, 792.0); n], 16.0)
    }

    #[test]
    fn level_rounds_to_nearest_power_of_two() {
        assert_eq!(level_for_scale(1.0), 0);
        assert_eq!(level_for_scale(1.9), 1);
        assert_eq!(level_for_scale(0.26), -2);
        assert_eq!(level_for_scale(1e9), MAX_LEVEL);
        assert_eq!(level_for_scale(0.0), MIN_LEVEL);
    }

    #[test]
    fn grid_covers_page() {
        let l = letter(1);
        assert_eq!(tile_grid(&l.pages[0], 0), (3, 4)); // 612/256, 792/256
        assert_eq!(tile_grid(&l.pages[0], 1), (5, 7)); // 128pt tiles
        assert_eq!(tile_grid(&l.pages[0], -2), (1, 1));
    }

    #[test]
    fn visible_tiles_respects_page_bounds_and_view() {
        let l = letter(2);
        let mut out = Vec::new();
        // a view straddling the gap between page 0 and page 1
        visible_tiles(&l, &Rect::new(0.0, 700.0, 612.0, 200.0), 0, &mut out);
        assert!(out.iter().all(|k| k.tx >= 0 && k.tx < 3 && k.ty >= 0 && k.ty < 4));
        assert!(out.iter().any(|k| k.page == 0 && k.ty == 3));
        assert!(out.iter().any(|k| k.page == 1 && k.ty == 0));
        assert!(!out.iter().any(|k| k.page == 1 && k.ty > 0));
    }

    #[test]
    fn ancestor_subrect_is_consistent() {
        let k = TileKey { page: 0, level: 3, tx: 5, ty: 6 };
        let (a, uv) = ancestor(k, 2);
        assert_eq!(a, TileKey { page: 0, level: 1, tx: 1, ty: 1 });
        assert_eq!(uv, [0.25, 0.5, 0.25, 0.25]);
        let l = letter(1);
        let kr = tile_world_rect(&l, k);
        let ar = tile_world_rect(&l, a);
        assert!((ar.x + uv[0] * ar.w - kr.x).abs() < 1e-3);
        assert!((ar.y + uv[1] * ar.h - kr.y).abs() < 1e-3);
        assert!((uv[2] * ar.w - kr.w).abs() < 1e-3);
    }

    #[test]
    fn overview_fits_in_one_tile() {
        let l = letter(3);
        let ov = overview_level(&l);
        assert_eq!(tile_grid(&l.pages[0], ov), (1, 1));
        assert_ne!(tile_grid(&l.pages[0], ov + 1), (1, 1));
    }
}
