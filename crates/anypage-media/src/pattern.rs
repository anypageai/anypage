//! Synthetic broadcast source: colour bars that scroll one picture width every
//! four seconds over a clock band showing the media time and frame number in
//! seven-segment digits. Drawn straight into I420, so a 1080p frame costs a
//! few row copies.

use crate::yuv::{rgb_to_yuv, ColorSpace, I420};

/// White, yellow, cyan, green, magenta, red, blue, black.
pub const BARS: [[u8; 3]; 8] = [[255, 255, 255], [255, 255, 0], [0, 255, 255], [0, 255, 0], [255, 0, 255], [255, 0, 0], [0, 0, 255], [0, 0, 0]];
pub const BAND: [u8; 3] = [40, 40, 48];
pub const INK: [u8; 3] = [255, 255, 255];

/// Pixels the bars move per frame (even, so chroma stays aligned).
pub fn speed(width: usize, fps: f64) -> usize {
    let s = (width as f64 / (fps.max(1.0) * 4.0)).round() as usize;
    (s.max(2) + 1) & !1
}

/// Height of the bar area; the clock band fills the rest.
pub fn bars_height(height: usize) -> usize {
    (height * 3 / 4) & !1
}

/// Which bar (index into [`BARS`]) covers column `x` of frame `frame`.
pub fn bar_at(width: usize, fps: f64, frame: u64, x: usize) -> usize {
    let offset = (frame as usize).wrapping_mul(speed(width, fps)) % width.max(1);
    ((x + offset) % width) * 8 / width
}

/// Seven-segment masks (a, b, c, d, e, f, g as bits 0..6).
const DIGITS: [u8; 10] = [0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F];

fn fill_y(pic: &mut I420, x0: isize, y0: isize, w: isize, h: isize, y: u8, clip_top: usize) {
    let (pw, ph) = (pic.width as isize, pic.height as isize);
    let xa = x0.max(0);
    let xb = (x0 + w).min(pw);
    let ya = y0.max(clip_top as isize);
    let yb = (y0 + h).min(ph);
    if xa >= xb || ya >= yb {
        return;
    }
    for row in ya..yb {
        let start = row as usize * pic.width;
        pic.y[start + xa as usize..start + xb as usize].fill(y);
    }
}

fn digit(pic: &mut I420, x: isize, y: isize, h: isize, mask: u8, ink: u8, top: usize) {
    let w = h / 2;
    let t = (h / 9).max(1);
    let half = h / 2;
    let seg = |k: u8| mask & (1 << k) != 0;
    let mut bar = |sx: isize, sy: isize, sw: isize, sh: isize| fill_y(pic, x + sx, y + sy, sw, sh, ink, top);
    if seg(0) {
        bar(0, 0, w, t);
    }
    if seg(1) {
        bar(w - t, 0, t, half);
    }
    if seg(2) {
        bar(w - t, half, t, h - half);
    }
    if seg(3) {
        bar(0, h - t, w, t);
    }
    if seg(4) {
        bar(0, half, t, h - half);
    }
    if seg(5) {
        bar(0, 0, t, half);
    }
    if seg(6) {
        bar(0, half - t / 2, w, t);
    }
}

/// Draw `text` (digits, ':', '.', ' ') with its top-left at (x, y), digits `h` tall.
pub fn draw_text(pic: &mut I420, x: isize, y: isize, h: isize, text: &str, ink: u8, top: usize) -> isize {
    let mut pen = x;
    let gap = (h / 6).max(1);
    let t = (h / 9).max(1);
    for ch in text.chars() {
        match ch {
            '0'..='9' => {
                digit(pic, pen, y, h, DIGITS[(ch as u8 - b'0') as usize], ink, top);
                pen += h / 2 + gap;
            }
            ':' => {
                fill_y(pic, pen, y + h / 4, t, t, ink, top);
                fill_y(pic, pen, y + 3 * h / 4 - t, t, t, ink, top);
                pen += t + gap;
            }
            '.' => {
                fill_y(pic, pen, y + h - t, t, t, ink, top);
                pen += t + gap;
            }
            _ => pen += h / 3,
        }
    }
    pen
}

/// "HH:MM:SS.mmm" for a media time in seconds.
pub fn clock_text(seconds: f64) -> String {
    let ms = (seconds.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// Frame `frame` of the pattern at `fps`.
pub fn test_pattern(width: usize, height: usize, frame: u64, fps: f64, cs: ColorSpace) -> I420 {
    let mut pic = I420::new(width, height);
    if width < 2 || height < 2 {
        return pic;
    }
    let bars_h = bars_height(height);
    let cw = width.div_ceil(2);
    let yuv: Vec<[u8; 3]> = BARS.iter().map(|c| rgb_to_yuv(*c, cs)).collect();
    // one row of bars, copied down
    let row: Vec<u8> = (0..width).map(|x| yuv[bar_at(width, fps, frame, x)][0]).collect();
    let urow: Vec<u8> = (0..cw).map(|cx| yuv[bar_at(width, fps, frame, (cx * 2).min(width - 1))][1]).collect();
    let vrow: Vec<u8> = (0..cw).map(|cx| yuv[bar_at(width, fps, frame, (cx * 2).min(width - 1))][2]).collect();
    for r in 0..bars_h {
        pic.y[r * width..(r + 1) * width].copy_from_slice(&row);
    }
    for cr in 0..bars_h / 2 {
        pic.u[cr * cw..(cr + 1) * cw].copy_from_slice(&urow);
        pic.v[cr * cw..(cr + 1) * cw].copy_from_slice(&vrow);
    }
    // clock band
    let band = rgb_to_yuv(BAND, cs);
    pic.y[bars_h * width..].fill(band[0]);
    pic.u[(bars_h / 2) * cw..].fill(band[1]);
    pic.v[(bars_h / 2) * cw..].fill(band[2]);
    let band_h = height - bars_h;
    let digit_h = ((band_h as f64 * 0.6) as isize).max(5);
    let ink = rgb_to_yuv(INK, cs)[0];
    let text = format!("{} {}", clock_text(frame as f64 / fps.max(1.0)), frame);
    let y = bars_h as isize + (band_h as isize - digit_h) / 2;
    draw_text(&mut pic, (width / 40) as isize + 2, y, digit_h, &text, ink, bars_h);
    pic
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yuv::i420_to_rgba;

    #[test]
    fn bars_move_and_colours_are_right() {
        let (w, h, fps) = (320usize, 240usize, 30.0);
        let cs = ColorSpace::BT601;
        for frame in [0u64, 1, 17, 1000] {
            let pic = test_pattern(w, h, frame, fps, cs);
            let mut rgba = vec![0u8; w * h * 4];
            i420_to_rgba(&pic.planes(), cs, &mut rgba, 1).unwrap();
            for x in (5..w).step_by(40) {
                let bar = bar_at(w, fps, frame, x);
                // skip the columns right at a bar edge
                if bar_at(w, fps, frame, x.saturating_sub(3)) != bar || bar_at(w, fps, frame, (x + 3).min(w - 1)) != bar {
                    continue;
                }
                let px = &rgba[(10 * w + x) * 4..(10 * w + x) * 4 + 3];
                for c in 0..3 {
                    assert!((px[c] as i32 - BARS[bar][c] as i32).abs() <= 3, "frame {frame} x {x}: {px:?} vs {:?}", BARS[bar]);
                }
            }
        }
        assert_ne!(bar_at(w, fps, 0, 0), bar_at(w, fps, 20, 0), "the bars scroll");
    }

    #[test]
    fn clock_band_has_ink() {
        let pic = test_pattern(640, 360, 1234, 30.0, ColorSpace::BT601);
        let bars_h = bars_height(360);
        let ink = pic.y[bars_h * 640..].iter().filter(|&&v| v > 200).count();
        assert!(ink > 500, "digits drawn: {ink}");
        assert_eq!(clock_text(3723.4567), "01:02:03.457");
        // tiny sizes do not panic
        let _ = test_pattern(2, 2, 5, 30.0, ColorSpace::BT601);
        let _ = test_pattern(18, 10, 5, 30.0, ColorSpace::BT601);
    }
}
