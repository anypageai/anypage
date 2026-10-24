use crate::tiles::Rect;

pub const MIN_SCALE: f32 = 1.0 / 64.0;
pub const MAX_SCALE: f32 = 128.0;

/// 2D camera. World units are PDF points (1/72 in); `scale` is device pixels
/// per point. `screen = world * scale + (tx, ty)`.
///
/// Camera moves never go through the script layer (ARCHITECTURE.md §7): drag and
/// zoom edit this struct directly on the render side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub scale: f32,
    pub tx: f32,
    pub ty: f32,
    pub viewport_w: f32,
    pub viewport_h: f32,
}

impl Camera {
    pub fn new(viewport_w: f32, viewport_h: f32) -> Self {
        Self { scale: 1.0, tx: 0.0, ty: 0.0, viewport_w: viewport_w.max(1.0), viewport_h: viewport_h.max(1.0) }
    }

    pub fn set_viewport(&mut self, w: f32, h: f32) {
        self.viewport_w = w.max(1.0);
        self.viewport_h = h.max(1.0);
    }

    pub fn world_to_screen(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.scale + self.tx, y * self.scale + self.ty)
    }

    pub fn screen_to_world(&self, sx: f32, sy: f32) -> (f32, f32) {
        ((sx - self.tx) / self.scale, (sy - self.ty) / self.scale)
    }

    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.tx += dx;
        self.ty += dy;
    }

    /// Multiply the scale by `factor`, keeping the world point under screen
    /// position `(sx, sy)` fixed.
    pub fn zoom_at(&mut self, sx: f32, sy: f32, factor: f32) {
        self.set_scale_at(sx, sy, self.scale * factor);
    }

    pub fn set_scale_at(&mut self, sx: f32, sy: f32, new_scale: f32) {
        let new_scale = new_scale.clamp(MIN_SCALE, MAX_SCALE);
        let f = new_scale / self.scale;
        self.tx = sx - (sx - self.tx) * f;
        self.ty = sy - (sy - self.ty) * f;
        self.scale = new_scale;
    }

    /// World rectangle currently covered by the viewport.
    pub fn visible_world(&self) -> Rect {
        let (x0, y0) = self.screen_to_world(0.0, 0.0);
        let (x1, y1) = self.screen_to_world(self.viewport_w, self.viewport_h);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    /// Fit `content` to the viewport width with `margin_px` on each side, top aligned.
    pub fn fit_width(&mut self, content: Rect, margin_px: f32) {
        let avail = (self.viewport_w - 2.0 * margin_px).max(1.0);
        self.scale = (avail / content.w.max(1.0)).clamp(MIN_SCALE, MAX_SCALE);
        self.tx = (self.viewport_w - content.w * self.scale) * 0.5 - content.x * self.scale;
        self.ty = margin_px - content.y * self.scale;
    }

    /// Fit the whole of `content` inside the viewport, centred.
    pub fn fit_rect(&mut self, content: Rect, margin_px: f32) {
        let aw = (self.viewport_w - 2.0 * margin_px).max(1.0);
        let ah = (self.viewport_h - 2.0 * margin_px).max(1.0);
        self.scale = (aw / content.w.max(1.0)).min(ah / content.h.max(1.0)).clamp(MIN_SCALE, MAX_SCALE);
        self.tx = (self.viewport_w - content.w * self.scale) * 0.5 - content.x * self.scale;
        self.ty = (self.viewport_h - content.h * self.scale) * 0.5 - content.y * self.scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_keeps_anchor_fixed() {
        let mut c = Camera::new(800.0, 600.0);
        c.pan(100.0, 50.0);
        let anchor = (300.0, 200.0);
        let before = c.screen_to_world(anchor.0, anchor.1);
        c.zoom_at(anchor.0, anchor.1, 2.5);
        let after = c.screen_to_world(anchor.0, anchor.1);
        assert!((before.0 - after.0).abs() < 1e-3 && (before.1 - after.1).abs() < 1e-3);
    }

    #[test]
    fn fit_width_centres_horizontally() {
        let mut c = Camera::new(1000.0, 500.0);
        c.fit_width(Rect::new(0.0, 0.0, 612.0, 792.0), 20.0);
        let (l, _) = c.world_to_screen(0.0, 0.0);
        let (r, _) = c.world_to_screen(612.0, 0.0);
        assert!((l - 20.0).abs() < 1e-3 && (r - 980.0).abs() < 1e-3);
    }
}
