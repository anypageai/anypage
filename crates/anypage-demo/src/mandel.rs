//! Static content the engine computes itself: a Tiled source whose pixels are a
//! pure function of (tile key, parameters). The engine keys tiles by the
//! parameter cell's version, so a parameter change makes old tiles stale (still
//! drawn as fallback) and cancels queued requests for the old version.

use anypage_core::{level_scale, Rect, TileKey, Value, TILE_SIZE};
use anypage_engine::TileSource;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MandelParams {
    pub iterations: u32,
    pub hue: f32,
}

pub struct Mandel {
    pub rect: Rect,
}

impl TileSource for Mandel {
    fn render(&self, key: TileKey, params: &Value, cancelled: &dyn Fn() -> bool) -> Option<Vec<u8>> {
        self.render_with(key, *params.downcast_ref::<MandelParams>()?, cancelled)
    }
}

impl Mandel {
    /// One tile for explicit parameters (shared by the Rust and the script page).
    pub fn render_with(&self, key: TileKey, p: MandelParams, cancelled: &dyn Fn() -> bool) -> Option<Vec<u8>> {
        let n = TILE_SIZE as usize;
        let s = level_scale(key.level) as f64;
        let r = self.rect;
        // complex plane: 3.2 units across the rect, centred on (-0.6, 0)
        let span = 3.2 / r.w as f64;
        let re0 = -0.6 - 1.6;
        let im0 = -(r.h as f64 / r.w as f64) * 1.6;
        let mut out = vec![0u8; n * n * 4];
        let max_it = p.iterations.max(8);
        for py in 0..n {
            if py % 16 == 0 && cancelled() {
                return None;
            }
            let wy = (key.ty as f64 * n as f64 + py as f64 + 0.5) / s;
            let ci = im0 + wy * span;
            for px in 0..n {
                let wx = (key.tx as f64 * n as f64 + px as f64 + 0.5) / s;
                let cr = re0 + wx * span;
                let rgb = shade(cr, ci, max_it, p.hue);
                let o = (py * n + px) * 4;
                out[o] = rgb[0];
                out[o + 1] = rgb[1];
                out[o + 2] = rgb[2];
                out[o + 3] = 255;
            }
        }
        Some(out)
    }
}

/// World point for a complex coordinate, for scripted camera moves.
pub fn world_of(rect: Rect, re: f64, im: f64) -> (f32, f32) {
    let span = 3.2 / rect.w as f64;
    let re0 = -0.6 - 1.6;
    let im0 = -(rect.h as f64 / rect.w as f64) * 1.6;
    (rect.x + ((re - re0) / span) as f32, rect.y + ((im - im0) / span) as f32)
}

fn shade(cr: f64, ci: f64, max_it: u32, hue: f32) -> [u8; 3] {
    // main cardioid and period-2 bulb are inside the set
    let q = (cr - 0.25) * (cr - 0.25) + ci * ci;
    if q * (q + (cr - 0.25)) <= 0.25 * ci * ci || (cr + 1.0) * (cr + 1.0) + ci * ci <= 0.0625 {
        return [6, 7, 14];
    }
    let (mut zr, mut zi) = (0.0f64, 0.0f64);
    let mut i = 0;
    while i < max_it {
        let zr2 = zr * zr;
        let zi2 = zi * zi;
        if zr2 + zi2 > 256.0 {
            break;
        }
        zi = 2.0 * zr * zi + ci;
        zr = zr2 - zi2 + cr;
        i += 1;
    }
    if i >= max_it {
        return [6, 7, 14];
    }
    let mag = (zr * zr + zi * zi).sqrt().max(1.000_001);
    let mu = i as f64 + 1.0 - (mag.ln().ln() / std::f64::consts::LN_2);
    let h = (hue as f64 + mu * 9.0).rem_euclid(360.0);
    let v = 0.18 + 0.82 * (1.0 - (-mu / 18.0).exp());
    hsv(h, 0.78, v)
}

pub fn hsv(h: f64, s: f64, v: f64) -> [u8; 3] {
    let c = v * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8]
}
