//! CPU rasterisation for native operators: vector shapes through tiny-skia,
//! glyphs through fontdue. Spike stand-ins for vello_cpu and parley/swash
//! (docs/ARCHITECTURE.md section 15), chosen for API stability.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tiny_skia::{FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};

pub type Rgba = [u8; 4];

/// Output of a panel operator: premultiplied RGBA8.
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
    /// Production time of the newest stream item drawn, for latency stats.
    pub newest: Option<Instant>,
}

pub const MAX_CANVAS: u32 = 2048;

/// Drawing surface in panel-local world units, rasterised at `scale` px/unit.
pub struct Canvas {
    pm: Pixmap,
    scale: f32,
}

fn paint(c: Rgba) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(c[0], c[1], c[2], c[3]);
    p.anti_alias = true;
    p
}

pub fn round_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w * 0.5).min(h * 0.5).max(0.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

impl Canvas {
    /// `world_w` x `world_h` units at `scale`, clamped so no side exceeds MAX_CANVAS.
    pub fn new(world_w: f32, world_h: f32, scale: f32) -> Self {
        let mut s = scale.max(0.05);
        let longest = world_w.max(world_h) * s;
        if longest > MAX_CANVAS as f32 {
            s *= MAX_CANVAS as f32 / longest;
        }
        let w = ((world_w * s).round() as u32).clamp(1, MAX_CANVAS);
        let h = ((world_h * s).round() as u32).clamp(1, MAX_CANVAS);
        Self { pm: Pixmap::new(w, h).expect("pixmap"), scale: s }
    }

    fn t(&self) -> Transform {
        Transform::from_scale(self.scale, self.scale)
    }

    pub fn clear(&mut self, c: Rgba) {
        self.pm.fill(tiny_skia::Color::from_rgba8(c[0], c[1], c[2], c[3]));
    }

    pub fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgba) {
        if let Some(p) = round_rect_path(x, y, w, h, r) {
            let t = self.t();
            self.pm.fill_path(&p, &paint(c), FillRule::Winding, t, None);
        }
    }

    pub fn stroke_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, width: f32, c: Rgba) {
        if let Some(p) = round_rect_path(x, y, w, h, r) {
            let t = self.t();
            let s = Stroke { width, ..Default::default() };
            self.pm.stroke_path(&p, &paint(c), &s, t, None);
        }
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgba) {
        if let Some(p) = PathBuilder::from_circle(cx, cy, r.max(0.01)) {
            let t = self.t();
            self.pm.fill_path(&p, &paint(c), FillRule::Winding, t, None);
        }
    }

    pub fn polyline(&mut self, pts: &[(f32, f32)], width: f32, c: Rgba) {
        if pts.len() < 2 {
            return;
        }
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].0, pts[0].1);
        for p in &pts[1..] {
            pb.line_to(p.0, p.1);
        }
        if let Some(path) = pb.finish() {
            let t = self.t();
            let s = Stroke { width, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() };
            self.pm.stroke_path(&path, &paint(c), &s, t, None);
        }
    }

    pub fn fill_under(&mut self, pts: &[(f32, f32)], floor: f32, c: Rgba) {
        if pts.len() < 2 {
            return;
        }
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].0, floor);
        for p in pts {
            pb.line_to(p.0, p.1);
        }
        pb.line_to(pts[pts.len() - 1].0, floor);
        pb.close();
        if let Some(path) = pb.finish() {
            let t = self.t();
            self.pm.fill_path(&path, &paint(c), FillRule::Winding, t, None);
        }
    }

    /// Single line of text with its top at `y`. Returns the advance in units.
    pub fn text(&mut self, tr: &TextRaster, x: f32, y: f32, px: f32, c: Rgba, s: &str) -> f32 {
        let size = px * self.scale;
        let (ascent, _) = tr.line_metrics(size);
        let baseline = y * self.scale + ascent;
        let mut pen = x * self.scale;
        for ch in s.chars() {
            let g = tr.glyph(ch, size);
            let (m, cov) = (&g.0, &g.1);
            let gx = (pen + m.xmin as f32).round() as i32;
            let gy = (baseline - m.ymin as f32 - m.height as f32).round() as i32;
            blit(&mut self.pm, gx, gy, m.width, m.height, cov, c);
            pen += m.advance_width;
        }
        pen / self.scale - x
    }

    /// Word-wrapped text inside a box. With `tail`, shows the last lines that
    /// fit (a log that scrolls). Returns the number of lines laid out.
    pub fn text_box(&mut self, tr: &TextRaster, x: f32, y: f32, w: f32, h: f32, px: f32, c: Rgba, s: &str, tail: bool) -> usize {
        let size = px * self.scale;
        let (_, line_h) = tr.line_metrics(size);
        let lines = tr.layout(s, size, w * self.scale);
        let fit = ((h * self.scale) / line_h).floor().max(1.0) as usize;
        let start = if tail && lines.len() > fit { lines.len() - fit } else { 0 };
        for (i, line) in lines.iter().skip(start).take(fit).enumerate() {
            let ly = y + (i as f32 * line_h) / self.scale;
            self.text(tr, x, ly, px, c, line);
        }
        lines.len()
    }

    pub fn into_image(self, newest: Option<Instant>) -> Image {
        let (w, h) = (self.pm.width(), self.pm.height());
        Image { w, h, rgba: self.pm.take(), newest }
    }
}

fn blit(pm: &mut Pixmap, gx: i32, gy: i32, gw: usize, gh: usize, cov: &[u8], c: Rgba) {
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let data = pm.data_mut();
    let a = c[3] as u32;
    for row in 0..gh as i32 {
        let y = gy + row;
        if y < 0 || y >= ph {
            continue;
        }
        for col in 0..gw as i32 {
            let x = gx + col;
            if x < 0 || x >= pw {
                continue;
            }
            let k = cov[row as usize * gw + col as usize] as u32;
            if k == 0 {
                continue;
            }
            let sa = k * a / 255;
            let inv = 255 - sa;
            let i = ((y * pw + x) * 4) as usize;
            data[i] = (c[0] as u32 * sa / 255 + data[i] as u32 * inv / 255) as u8;
            data[i + 1] = (c[1] as u32 * sa / 255 + data[i + 1] as u32 * inv / 255) as u8;
            data[i + 2] = (c[2] as u32 * sa / 255 + data[i + 2] as u32 * inv / 255) as u8;
            data[i + 3] = (sa + data[i + 3] as u32 * inv / 255) as u8;
        }
    }
}

type Glyph = Arc<(fontdue::Metrics, Vec<u8>)>;

pub struct TextRaster {
    font: fontdue::Font,
    cache: Mutex<HashMap<(char, u32), Glyph>>,
    pub source: String,
}

fn is_cjk(c: char) -> bool {
    // 0x2E80..=0x9FFF already covers CJK punctuation (0x3000..=0x303F)
    matches!(c as u32, 0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF)
}

impl TextRaster {
    pub fn load() -> Result<Self, String> {
        let candidates = [
            r"C:\Windows\Fonts\msjh.ttc",
            r"C:\Windows\Fonts\msyh.ttc",
            r"C:\Windows\Fonts\simsun.ttc",
            r"C:\Windows\Fonts\segoeui.ttf",
            r"C:\Windows\Fonts\arial.ttf",
            "/System/Library/Fonts/PingFang.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ];
        for path in candidates {
            let Ok(bytes) = std::fs::read(path) else { continue };
            let settings = fontdue::FontSettings { collection_index: 0, scale: 48.0, ..Default::default() };
            if let Ok(font) = fontdue::Font::from_bytes(bytes, settings) {
                return Ok(Self { font, cache: Mutex::new(HashMap::new()), source: path.to_string() });
            }
        }
        Err("no usable font found".into())
    }

    fn glyph(&self, ch: char, px: f32) -> Glyph {
        let key = (ch, (px * 4.0).round() as u32);
        let mut cache = self.cache.lock().unwrap();
        if let Some(g) = cache.get(&key) {
            return g.clone();
        }
        if cache.len() > 40_000 {
            cache.clear();
        }
        let g: Glyph = Arc::new(self.font.rasterize(ch, px));
        cache.insert(key, g.clone());
        g
    }

    fn advance(&self, ch: char, px: f32) -> f32 {
        self.glyph(ch, px).0.advance_width
    }

    /// (ascent, line height) in pixels.
    pub fn line_metrics(&self, px: f32) -> (f32, f32) {
        self.font
            .horizontal_line_metrics(px)
            .map(|m| (m.ascent, m.new_line_size))
            .unwrap_or((px * 0.8, px * 1.25))
    }

    /// Greedy line breaking. CJK characters break anywhere; other text breaks
    /// at spaces, or inside a word only when the word alone is too long.
    pub fn layout(&self, s: &str, px: f32, max_w: f32) -> Vec<String> {
        let mut lines = Vec::new();
        let mut cur = String::new();
        let mut cur_w = 0.0f32;
        let mut segs: Vec<String> = Vec::new();
        let mut word = String::new();
        for ch in s.chars() {
            if ch == '\n' || ch.is_whitespace() || is_cjk(ch) {
                if !word.is_empty() {
                    segs.push(std::mem::take(&mut word));
                }
                segs.push(ch.to_string());
            } else {
                word.push(ch);
            }
        }
        if !word.is_empty() {
            segs.push(word);
        }
        for seg in segs {
            if seg == "\n" {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0.0;
                continue;
            }
            let w: f32 = seg.chars().map(|c| self.advance(c, px)).sum();
            let is_space = seg.chars().all(char::is_whitespace);
            if cur_w + w <= max_w || cur.is_empty() && w <= max_w {
                cur.push_str(&seg);
                cur_w += w;
            } else if is_space {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0.0;
            } else if w > max_w {
                for c in seg.chars() {
                    let cw = self.advance(c, px);
                    if cur_w + cw > max_w && !cur.is_empty() {
                        lines.push(std::mem::take(&mut cur));
                        cur_w = 0.0;
                    }
                    cur.push(c);
                    cur_w += cw;
                }
            } else {
                lines.push(std::mem::take(&mut cur));
                cur.push_str(&seg);
                cur_w = w;
            }
        }
        if !cur.is_empty() {
            lines.push(cur);
        }
        lines
    }
}
