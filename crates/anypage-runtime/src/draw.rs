//! Display lists: what a script panel returns instead of pixels.
//!
//! A panel's script function only records drawing commands (cheap, in Luau).
//! The list is a plain value, so an unchanged list stops propagation before
//! any pixel work, and rasterising stays native and resolution-aware: the
//! same list is re-rasterised at a new zoom level without running the script.
//!
//! Commands are lists: `{ "rrect", x, y, w, h, r, color }`. Colours are
//! packed `0xRRGGBBAA` numbers or `{ r, g, b, a }` lists.

use std::time::Instant;

use anypage_engine::{Canvas, Image, Rgba, TextRaster};

/// Clip rectangles nest at most this deep; deeper pushes are ignored.
const MAX_CLIP_DEPTH: usize = 32;

use crate::data::Data;

pub const MAX_COMMANDS: usize = 50_000;
const MAX_POINTS: usize = 200_000;
const MAX_TEXT: usize = 20_000;

/// A recorded panel. Equality ignores `newest`: if nothing visible changed,
/// nothing new reached the screen either.
#[derive(Clone, Debug)]
pub struct DisplayList {
    pub cmds: Data,
    pub newest: Option<Instant>,
}

impl PartialEq for DisplayList {
    fn eq(&self, o: &Self) -> bool {
        self.cmds == o.cmds
    }
}

pub fn color(d: &Data) -> Rgba {
    match d {
        Data::Num(n) if n.is_finite() => {
            let v = (*n as i64 as u64 & 0xFFFF_FFFF) as u32;
            [(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8]
        }
        Data::List(l) => {
            let c = |i: usize, d: f64| l.get(i).and_then(Data::num).unwrap_or(d).clamp(0.0, 255.0) as u8;
            [c(0, 0.0), c(1, 0.0), c(2, 0.0), c(3, 255.0)]
        }
        _ => [0, 0, 0, 0],
    }
}

fn points(d: Option<&Data>) -> Vec<(f32, f32)> {
    let Some(d) = d else { return Vec::new() };
    let items = d.items();
    let n = (items.len() / 2).min(MAX_POINTS);
    (0..n).map(|k| (items[2 * k].num_or(0.0) as f32, items[2 * k + 1].num_or(0.0) as f32)).collect()
}

fn clip_text(s: &str) -> &str {
    match s.char_indices().nth(MAX_TEXT) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[allow(clippy::too_many_arguments)]
fn series(c: &mut Canvas, x: f32, y: f32, w: f32, h: f32, vals: &[Data], line: Rgba, fill: Rgba, width: f32, floor: f32, min_span: f32) {
    let v: Vec<f32> = vals.iter().take(MAX_POINTS).filter_map(Data::num).map(|n| n as f32).collect();
    if v.len() < 2 {
        return;
    }
    let (lo, hi) = v.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &x| (lo.min(x), hi.max(x)));
    let span = (hi - lo).max(min_span).max(1e-6);
    let last = (v.len() - 1) as f32;
    let pts: Vec<(f32, f32)> = v.iter().enumerate().map(|(k, &val)| (x + w * k as f32 / last, y + h - h * (val - lo) / span)).collect();
    if fill[3] > 0 {
        c.fill_under(&pts, floor, fill);
    }
    c.polyline(&pts, width, line);
}

fn draw_one(c: &mut Canvas, tr: &TextRaster, cmd: &Data, clips: &mut usize) {
    let a = cmd.items();
    let Some(op) = a.first().and_then(Data::as_str) else { return };
    let n = |i: usize| {
        let v = a.get(i).and_then(Data::num).unwrap_or(0.0) as f32;
        if v.is_finite() {
            v.clamp(-1.0e6, 1.0e6)
        } else {
            0.0
        }
    };
    let size = |i: usize| n(i).clamp(1.0, 200.0);
    let col = |i: usize| a.get(i).map(color).unwrap_or([0; 4]);
    let s = |i: usize| clip_text(a.get(i).and_then(Data::as_str).unwrap_or(""));
    match op {
        "clear" => c.clear(col(1)),
        "rrect" => c.round_rect(n(1), n(2), n(3), n(4), n(5), col(6)),
        "stroke" => c.stroke_round_rect(n(1), n(2), n(3), n(4), n(5), n(6), col(7)),
        "circle" => c.circle(n(1), n(2), n(3), col(4)),
        "text" => {
            let dir = match a.get(6).and_then(Data::as_str) {
                Some("rtl") => anypage_engine::Direction::Rtl,
                Some("ltr") => anypage_engine::Direction::Ltr,
                _ => anypage_engine::Direction::Auto,
            };
            c.text_dir(tr, n(1), n(2), size(3), col(4), s(5), dir);
        }
        "textbox" => {
            let tail = a.get(8).is_some_and(Data::truthy);
            c.text_box(tr, n(1), n(2), n(3), n(4), size(5), col(6), s(7), tail);
        }
        "line" => c.polyline(&points(a.get(1)), n(2), col(3)),
        "area" => c.fill_under(&points(a.get(1)), n(2), col(3)),
        "series" => {
            let vals = a.get(5).map(Data::items).unwrap_or(&[]);
            series(c, n(1), n(2), n(3), n(4), vals, col(6), col(7), n(8), n(9), n(10));
        }
        "image" => {
            if let Some(img) = a.get(1).and_then(Data::as_res).and_then(|r| r.get::<Image>()) {
                c.image(img, n(2), n(3), n(4), n(5));
            }
        }
        "clip" => {
            if *clips < MAX_CLIP_DEPTH {
                c.push_clip(n(1), n(2), n(3), n(4));
                *clips += 1;
            }
        }
        "unclip" => {
            if *clips > 0 {
                c.pop_clip();
                *clips -= 1;
            }
        }
        _ => {}
    }
}

/// Rasterise a display list for a `w` x `h` (world units) panel at `scale` px/unit.
pub fn render(list: &DisplayList, w: f32, h: f32, scale: f32, tr: &TextRaster) -> Image {
    let mut c = Canvas::new(w, h, scale);
    let mut clips = 0;
    for cmd in list.cmds.items().iter().take(MAX_COMMANDS) {
        draw_one(&mut c, tr, cmd, &mut clips);
    }
    c.into_image(list.newest)
}

/// What a panel shows when its script failed: the page stays usable.
pub fn error_image(w: f32, h: f32, scale: f32, tr: &TextRaster, msg: &str) -> Image {
    let mut c = Canvas::new(w, h, scale);
    c.round_rect(0.0, 0.0, w, h, 14.0, [52, 18, 24, 255]);
    c.stroke_round_rect(0.5, 0.5, w - 1.0, h - 1.0, 14.0, 1.0, [255, 110, 110, 160]);
    c.text(tr, 18.0, 14.0, 18.0, [255, 140, 140, 255], "腳本錯誤 script error");
    c.text_box(tr, 18.0, 46.0, (w - 36.0).max(1.0), (h - 58.0).max(1.0), 14.0, [236, 210, 214, 255], clip_text(msg), false);
    c.into_image(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours() {
        assert_eq!(color(&Data::Num(0x11223344u32 as f64)), [0x11, 0x22, 0x33, 0x44]);
        assert_eq!(color(&Data::nums([1.0, 2.0, 3.0])), [1, 2, 3, 255]);
        assert_eq!(color(&Data::Num(f64::NAN)), [0, 0, 0, 0]);
    }

    #[test]
    fn equality_ignores_timestamp() {
        let a = DisplayList { cmds: Data::nums([1.0]), newest: None };
        let b = DisplayList { cmds: Data::nums([1.0]), newest: Some(Instant::now()) };
        assert_eq!(a, b);
    }
}
