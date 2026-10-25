//! Glyph rasterisation through swash and blending into premultiplied RGBA8.
//!
//! Glyph images up to [`MAX_GLYPH_SIDE`] pixels on a side are cached. A
//! larger outline glyph is not rendered into an image at all: it is filled
//! straight into the visible part of the target ([`draw_direct`]). A larger
//! colour glyph (COLR, bitmap strikes) is rendered at a size that fits and
//! drawn scaled up ([`blit_scaled`]).

use std::cell::RefCell;

use cosmic_text::Font;
use swash::scale::image::{Content, Image};
use swash::scale::{Render, ScaleContext, Scaler, Source, StrikeWith};
use swash::zeno::{Angle, Format, Transform, Vector};
use tiny_skia::{FillRule, Mask};

/// Cache key: font slot, glyph, size in quarter pixels, horizontal
/// subpixel position in quarter pixels, synthetic italic, and the text
/// colour for colour glyphs that paint with it (0 = not given).
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) struct GlyphKey {
    pub font: u16,
    pub id: u16,
    pub size_q: u32,
    pub bin: u8,
    pub italic: bool,
    /// 0x01RRGGBB, or 0.
    pub fg: u32,
}

impl GlyphKey {
    pub fn px(&self) -> f32 {
        self.size_q as f32 / 4.0
    }

    /// The text colour, when the key carries one.
    pub fn fg_rgb(&self) -> Option<[u8; 3]> {
        (self.fg >> 24 != 0).then(|| {
            let [_, r, g, b] = self.fg.to_be_bytes();
            [r, g, b]
        })
    }
}

/// A rendered glyph: coverage (`color == false`) or premultiplied RGBA.
pub(crate) struct Bitmap {
    pub left: i32,
    pub top: i32,
    pub w: usize,
    pub h: usize,
    pub color: bool,
    pub data: Vec<u8>,
    /// Drawn this many times larger than its pixels (a colour glyph too
    /// large to keep at its size is rendered smaller); `left` and `top` are
    /// in the image's own pixels.
    pub scale: f32,
    /// Too large to keep as an image: fill the outline straight into the
    /// target ([`draw_direct`]).
    pub direct: bool,
    /// The paint uses the text colour (COLR palette index 0xFFFF): this
    /// image was painted without it, look the glyph up again with
    /// [`GlyphKey::fg`] set.
    pub uses_fg: bool,
}

impl Bitmap {
    pub fn empty() -> Self {
        Self { left: 0, top: 0, w: 0, h: 0, color: false, data: Vec::new(), scale: 1.0, direct: false, uses_fg: false }
    }

    fn direct() -> Self {
        Self { direct: true, ..Self::empty() }
    }

    pub fn cost(&self) -> usize {
        64 + self.data.len()
    }
}

thread_local! {
    static SCALE: RefCell<ScaleContext> = RefCell::new(ScaleContext::new());
}

/// Largest glyph image kept (pixels per side): no glyph allocates more than
/// this squared, whatever the font size.
pub(crate) const MAX_GLYPH_SIDE: usize = 1024;
/// From this size an outline is measured before it is rendered, so a glyph
/// too large to keep never allocates its full image.
const CHECK_PX: f32 = 400.0;
/// Colour bitmap strikes (CBDT, sbix; at most about 160 px in the fonts)
/// are scaled up to at most this size and drawn scaled from there.
const BITMAP_MAX_PX: f32 = 512.0;
/// Synthetic italic slant: tan(14 degrees).
pub(crate) const SKEW: f32 = 0.249_328;

/// Run `f` with a swash scaler for `font` at `size` px (variable fonts at
/// their regular weight), on this thread's scale context.
fn with_scaler<R>(font: &Font, size: f32, f: impl FnOnce(&mut Scaler<'_>) -> Option<R>) -> Option<R> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        SCALE.with(|ctx| {
            let Ok(mut ctx) = ctx.try_borrow_mut() else { return None };
            let fr = font.as_swash();
            let wght = swash::tag_from_bytes(b"wght");
            let mut builder = ctx.builder(fr).size(size).hint(false);
            if fr.variations().find_by_tag(wght).is_some() {
                builder = builder.normalized_coords(fr.variations().normalized_coords([(wght, 400.0f32)]));
            }
            f(&mut builder.build())
        })
    }))
    .ok()
    .flatten()
}

fn swash_render(font: &Font, key: GlyphKey, size: f32, sources: &[Source]) -> Option<Image> {
    with_scaler(font, size, |scaler| {
        Render::new(sources)
            .format(Format::Alpha)
            .offset(Vector::new(key.bin as f32 / 4.0, 0.0))
            .transform(key.italic.then(|| Transform::skew(Angle::from_degrees(14.0), Angle::from_degrees(0.0))))
            .render(scaler, key.id)
    })
}

/// A swash image as a bitmap (`None` for an empty or malformed image).
fn to_bitmap(img: Image) -> Option<Bitmap> {
    let (w, h) = (img.placement.width as usize, img.placement.height as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let (left, top) = (img.placement.left, img.placement.top);
    match img.content {
        Content::Mask if img.data.len() >= w * h => {
            let mut data = img.data;
            data.truncate(w * h);
            Some(Bitmap { left, top, w, h, color: false, data, ..Bitmap::empty() })
        }
        Content::Color if img.data.len() >= w * h * 4 => {
            let mut data = img.data;
            data.truncate(w * h * 4);
            // Colour outlines (COLR) come out premultiplied; bitmap strikes
            // (CBDT, sbix PNGs) come out straight.
            if matches!(img.source, Source::ColorBitmap(_) | Source::Bitmap(_)) {
                for p in data.as_chunks_mut::<4>().0 {
                    let a = p[3] as u32;
                    for c in &mut p[..3] {
                        *c = div255(*c as u32 * a) as u8;
                    }
                }
            }
            Some(Bitmap { left, top, w, h, color: true, data, ..Bitmap::empty() })
        }
        _ => None,
    }
}

fn fits(b: &Bitmap) -> bool {
    b.w <= MAX_GLYPH_SIDE && b.h <= MAX_GLYPH_SIDE
}

/// Size (px) of the outline's bounding box, slant included.
fn outline_extent(font: &Font, key: GlyphKey) -> Option<(f32, f32)> {
    with_scaler(font, key.px(), |scaler| {
        let b = scaler.scale_outline(key.id)?.bounds();
        let (w, h) = (b.width(), b.height());
        Some(if key.italic { (w + h * SKEW, h) } else { (w, h) })
    })
}

const ALL: [Source; 3] = [Source::ColorOutline(0), Source::ColorBitmap(StrikeWith::BestFit), Source::Outline];

pub(crate) fn render(font: &Font, face_index: u32, key: GlyphKey) -> Bitmap {
    if crate::colr::has_colr(font) {
        if let Some(b) = crate::colr::render(font, face_index, key) {
            return b;
        }
    }
    let size = key.px();
    if size > CHECK_PX {
        // a bitmap strike: scaled up to at most BITMAP_MAX_PX, the rest when drawn
        if let Some(mut b) = swash_render(font, key, BITMAP_MAX_PX, &[Source::ColorBitmap(StrikeWith::BestFit)]).and_then(to_bitmap) {
            b.scale = size / BITMAP_MAX_PX;
            return if fits(&b) { b } else { Bitmap::empty() };
        }
        match outline_extent(font, key) {
            Some((w, h)) if w.max(h) > MAX_GLYPH_SIDE as f32 - 2.0 => return Bitmap::direct(),
            _ => {}
        }
    }
    let Some(b) = swash_render(font, key, size, &ALL).and_then(to_bitmap) else { return Bitmap::empty() };
    if fits(&b) {
        return b;
    }
    if !b.color {
        return Bitmap::direct();
    }
    // a colour image over the cap: render it smaller, draw it scaled
    let k = (MAX_GLYPH_SIDE as f32 - 2.0) / b.w.max(b.h) as f32;
    match swash_render(font, key, size * k, &ALL).and_then(to_bitmap) {
        Some(mut r) if fits(&r) => {
            r.scale = 1.0 / k;
            r
        }
        _ => Bitmap::empty(),
    }
}

/// Fill a glyph outline too large to keep as an image straight into the
/// visible part of the target: the pen at pixel `ox`, the baseline at row
/// `oy`. Memory is bounded by the clip, not by the glyph size.
pub(crate) fn draw_direct(t: &mut Target<'_>, font: &Font, face_index: u32, key: GlyphKey, ox: i32, oy: i32, color: [u8; 4]) {
    let clip = t.clip;
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Option<(Mask, i32, i32)> {
        let skew = if key.italic { SKEW } else { 0.0 };
        let path = crate::colr::glyph_path(font, face_index, key.id, key.px(), skew, key.bin as f32 / 4.0)?;
        let b = path.bounds();
        let (cx0, cy0, cx1, cy1) = clip;
        // float to int casts saturate, and the clip bounds the window
        let x0 = cx0.max(ox.saturating_add(b.left().floor() as i32));
        let x1 = cx1.min(ox.saturating_add(b.right().ceil() as i32));
        let y0 = cy0.max(oy.saturating_add(b.top().floor() as i32));
        let y1 = cy1.min(oy.saturating_add(b.bottom().ceil() as i32));
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        let mut mask = Mask::new((x1 - x0) as u32, (y1 - y0) as u32)?;
        let shift = tiny_skia::Transform::from_translate(ox.saturating_sub(x0) as f32, oy.saturating_sub(y0) as f32);
        mask.fill_path(&path, FillRule::Winding, true, shift);
        Some((mask, x0, y0))
    }));
    if let Ok(Some((mask, x0, y0))) = r {
        blit_raw(t, x0, y0, mask.width() as usize, mask.height() as usize, false, mask.data(), color);
    }
}

/// x / 255 rounded, exact for x in 0..=255*255.
#[inline(always)]
pub(crate) fn div255(x: u32) -> u32 {
    let x = x + 128;
    (x + (x >> 8)) >> 8
}

/// Destination surface: premultiplied RGBA8 rows of `w` pixels, and the
/// clip in pixels already intersected with the surface.
pub(crate) struct Target<'a> {
    pub dst: &'a mut [u8],
    pub w: usize,
    pub clip: (i32, i32, i32, i32),
}

/// Composite one glyph (source-over) with its top-left pixel at (gx, gy).
pub(crate) fn blit(t: &mut Target<'_>, gx: i32, gy: i32, bm: &Bitmap, color: [u8; 4]) {
    blit_raw(t, gx, gy, bm.w, bm.h, bm.color, &bm.data, color);
}

/// [`blit`] of a `w` x `h` image: coverage, or premultiplied RGBA when
/// `rgba`.
#[allow(clippy::too_many_arguments)]
fn blit_raw(t: &mut Target<'_>, gx: i32, gy: i32, w: usize, h: usize, rgba: bool, data: &[u8], color: [u8; 4]) {
    if data.len() < w * h * if rgba { 4 } else { 1 } {
        return;
    }
    let (cx0, cy0, cx1, cy1) = t.clip;
    let x0 = cx0.max(gx);
    let x1 = cx1.min(gx.saturating_add(w.min(i32::MAX as usize) as i32));
    let y0 = cy0.max(gy);
    let y1 = cy1.min(gy.saturating_add(h.min(i32::MAX as usize) as i32));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let ca = color[3] as u64;
    // the text colour with alpha 255, one channel per 16-bit lane
    let solid = lanes(u32::from_le_bytes([color[0], color[1], color[2], 255]));
    let opaque = [color[0], color[1], color[2], 255];
    let (sx0, sx1) = ((x0 - gx) as usize, (x1 - gx) as usize);
    for y in y0..y1 {
        let sy = (y - gy) as usize;
        let row = y as usize * t.w * 4;
        let Some(drow) = t.dst.get_mut(row + x0 as usize * 4..row + x1 as usize * 4) else { return };
        if rgba {
            let srow = &data[(sy * w + sx0) * 4..(sy * w + sx1) * 4];
            for (s, d) in srow.as_chunks::<4>().0.iter().zip(drow.as_chunks_mut::<4>().0) {
                if s[3] == 0 {
                    continue;
                }
                // premultiplied source faded by the text alpha, source-over:
                // out = (s * ca + d * (255 - sa)) / 255, all four lanes at once
                let sa = div255(s[3] as u32 * ca as u32) as u64;
                let src = lanes(u32::from_le_bytes(*s));
                let dst = lanes(u32::from_le_bytes(*d));
                *d = unlanes(div255_lanes(src.wrapping_mul(ca).wrapping_add(dst.wrapping_mul(255 - sa)))).to_le_bytes();
            }
        } else {
            let srow = &data[sy * w + sx0..sy * w + sx1];
            for (&k, d) in srow.iter().zip(drow.as_chunks_mut::<4>().0) {
                if k == 0 {
                    continue;
                }
                let sa = if ca == 255 { k as u64 } else { div255(k as u32 * ca as u32) as u64 };
                if sa >= 255 {
                    *d = opaque;
                    continue;
                }
                let dst = lanes(u32::from_le_bytes(*d));
                *d = unlanes(div255_lanes(solid.wrapping_mul(sa).wrapping_add(dst.wrapping_mul(255 - sa)))).to_le_bytes();
            }
        }
    }
}

/// Source pixels and weights (0..=256, 0 outside the image) of one target
/// column or row for bilinear sampling.
#[derive(Clone, Copy)]
struct Tap {
    a: usize,
    b: usize,
    wa: u32,
    wb: u32,
}

fn taps(from: i32, to: i32, origin: f32, inv: f32, n: usize) -> Vec<Tap> {
    (from..to)
        .map(|p| {
            let u = (p as f32 + 0.5 - origin) * inv - 0.5;
            let fl = u.floor();
            let f = ((u - fl) * 256.0).round().clamp(0.0, 256.0) as u32;
            let i = fl as i64;
            let inside = |j: i64| j >= 0 && (j as u64) < n as u64;
            let (a, wa) = if inside(i) { (i as usize, 256 - f) } else { (0, 0) };
            let (b, wb) = if inside(i + 1) { ((i + 1) as usize, f) } else { (0, 0) };
            Tap { a, b, wa, wb }
        })
        .collect()
}

/// Composite a glyph image drawn `bm.scale` times larger than its pixels,
/// its top-left corner at (`ox`, `oy`) px, sampled bilinearly (source-over).
pub(crate) fn blit_scaled(t: &mut Target<'_>, ox: f32, oy: f32, bm: &Bitmap, color: [u8; 4]) {
    let s = bm.scale;
    let ch = if bm.color { 4 } else { 1 };
    if !(s.is_finite() && s > 0.0) || !ox.is_finite() || !oy.is_finite() || bm.w == 0 || bm.h == 0 || bm.data.len() < bm.w * bm.h * ch {
        return;
    }
    let (cx0, cy0, cx1, cy1) = t.clip;
    let x0 = (cx0 as f32).max(ox.floor()) as i32;
    let x1 = (cx1 as f32).min((ox + bm.w as f32 * s).ceil()) as i32;
    let y0 = (cy0 as f32).max(oy.floor()) as i32;
    let y1 = (cy1 as f32).min((oy + bm.h as f32 * s).ceil()) as i32;
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let inv = 1.0 / s;
    let cols = taps(x0, x1, ox, inv, bm.w);
    let rows = taps(y0, y1, oy, inv, bm.h);
    let ca = color[3] as u32;
    let data = &bm.data;
    for (r, y) in rows.iter().zip(y0..y1) {
        let row = y as usize * t.w * 4;
        let Some(drow) = t.dst.get_mut(row + x0 as usize * 4..row + x1 as usize * 4) else { return };
        for (c, d) in cols.iter().zip(drow.as_chunks_mut::<4>().0) {
            let mut acc = [0u32; 4];
            for (yy, wy) in [(r.a, r.wa), (r.b, r.wb)] {
                if wy == 0 {
                    continue;
                }
                for (xx, wx) in [(c.a, c.wa), (c.b, c.wb)] {
                    if wx == 0 {
                        continue;
                    }
                    let w = wy * wx;
                    let i = (yy * bm.w + xx) * ch;
                    for k in 0..ch {
                        acc[k] += w * data[i + k] as u32;
                    }
                }
            }
            if bm.color {
                // premultiplied source faded by the text alpha, source-over
                let src = acc.map(|v| (v + 32768) >> 16);
                let sa = div255(src[3] * ca);
                if sa == 0 {
                    continue;
                }
                for k in 0..4 {
                    d[k] = (div255(src[k] * ca) + div255(d[k] as u32 * (255 - sa))).min(255) as u8;
                }
            } else {
                let sa = div255(((acc[0] + 32768) >> 16) * ca);
                if sa == 0 {
                    continue;
                }
                let solid = [color[0] as u32, color[1] as u32, color[2] as u32, 255];
                for k in 0..4 {
                    d[k] = div255(solid[k] * sa + d[k] as u32 * (255 - sa)) as u8;
                }
            }
        }
    }
}

/// RGBA8 (little-endian u32) to one byte per 16-bit lane of a u64.
#[inline(always)]
fn lanes(v: u32) -> u64 {
    let v = v as u64;
    (v & 0xFF) | ((v & 0xFF00) << 8) | ((v & 0xFF_0000) << 16) | ((v & 0xFF00_0000) << 24)
}

#[inline(always)]
fn unlanes(x: u64) -> u32 {
    ((x & 0xFF) | ((x >> 8) & 0xFF00) | ((x >> 16) & 0xFF_0000) | ((x >> 24) & 0xFF00_0000)) as u32
}

/// `div255` in each 16-bit lane; every lane must be at most 255 * 255 + 255.
#[inline(always)]
fn div255_lanes(x: u64) -> u64 {
    const LO: u64 = 0x00FF_00FF_00FF_00FF;
    let x = x.wrapping_add(0x0080_0080_0080_0080);
    (x.wrapping_add((x >> 8) & LO) >> 8) & LO
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn div255_is_exact() {
        for x in 0..=255u32 * 255 {
            assert_eq!(div255(x), (x as f64 / 255.0).round() as u32, "x = {x}");
        }
    }

    #[test]
    fn lane_blend_matches_scalar() {
        for &(c, d, sa) in &[([255u8, 0, 0, 255], [0u8, 0, 255, 255], 128u64), ([10, 200, 30, 255], [255, 255, 255, 255], 1), ([0, 0, 0, 255], [7, 8, 9, 10], 254)] {
            let solid = lanes(u32::from_le_bytes(c));
            let dst = lanes(u32::from_le_bytes(d));
            let out = unlanes(div255_lanes(solid.wrapping_mul(sa).wrapping_add(dst.wrapping_mul(255 - sa)))).to_le_bytes();
            for k in 0..4 {
                let want = div255(c[k] as u32 * sa as u32 + d[k] as u32 * (255 - sa as u32));
                assert_eq!(out[k] as u32, want, "channel {k} of {c:?} over {d:?} at {sa}");
            }
        }
        assert_eq!(unlanes(lanes(0x1234_5678)), 0x1234_5678);
    }

    #[test]
    fn blit_blends_and_clips() {
        let mut dst = vec![0u8; 4 * 4 * 4];
        let bm = Bitmap { w: 2, h: 2, data: vec![255, 128, 0, 255], ..Bitmap::empty() };
        let mut t = Target { dst: &mut dst, w: 4, clip: (0, 0, 3, 4) };
        blit(&mut t, 2, 1, &bm, [255, 0, 0, 255]);
        // (2,1) full red, (3,1) clipped away, (2,2) empty coverage
        assert_eq!(&dst[(4 + 2) * 4..(4 + 2) * 4 + 4], &[255, 0, 0, 255]);
        assert_eq!(&dst[(4 + 3) * 4..(4 + 3) * 4 + 4], &[0, 0, 0, 0]);
        assert_eq!(&dst[(8 + 2) * 4..(8 + 2) * 4 + 4], &[0, 0, 0, 0]);
        // half coverage over opaque white: premultiplied mix
        let mut white = vec![255u8; 4];
        let half = Bitmap { w: 1, h: 1, data: vec![128], ..Bitmap::empty() };
        let mut t = Target { dst: &mut white, w: 1, clip: (0, 0, 1, 1) };
        blit(&mut t, 0, 0, &half, [0, 0, 0, 255]);
        assert_eq!(white, vec![127, 127, 127, 255]);
        // data shorter than w * h: nothing drawn, no panic
        let short = Bitmap { w: 4, h: 4, data: vec![255; 3], ..Bitmap::empty() };
        let mut t = Target { dst: &mut dst, w: 4, clip: (0, 0, 4, 4) };
        blit(&mut t, 0, 0, &short, [0, 255, 0, 255]);
        assert!(dst.as_chunks::<4>().0.iter().all(|p| p[1] == 0));
    }

    #[test]
    fn scaled_blit_fills_the_scaled_rectangle() {
        // a 2x2 opaque red image drawn 2x as large at (1, 1) into 6x6
        let red = Bitmap { w: 2, h: 2, color: true, data: [255, 0, 0, 255].repeat(4), scale: 2.0, ..Bitmap::empty() };
        let mut dst = vec![0u8; 6 * 6 * 4];
        let mut t = Target { dst: &mut dst, w: 6, clip: (0, 0, 6, 6) };
        blit_scaled(&mut t, 1.0, 1.0, &red, [255, 255, 255, 255]);
        let px = |x: usize, y: usize| -> [u8; 4] { dst[(y * 6 + x) * 4..(y * 6 + x) * 4 + 4].try_into().unwrap_or([0; 4]) };
        // the inside is solid red
        assert_eq!(px(2, 2), [255, 0, 0, 255]);
        assert_eq!(px(3, 3), [255, 0, 0, 255]);
        // the corner pixel is three quarters covered on each axis
        let corner = px(1, 1);
        assert!((135..=150).contains(&corner[3]), "{corner:?}");
        assert_eq!(corner[0], corner[3]);
        // nothing outside the 4x4 rectangle
        for (x, y) in [(0, 0), (5, 5), (0, 3), (5, 2)] {
            assert_eq!(px(x, y), [0, 0, 0, 0], "({x}, {y})");
        }
        // premultiplied everywhere; half text alpha halves the inside
        assert!(dst.as_chunks::<4>().0.iter().all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
        let mut dst2 = vec![0u8; 6 * 6 * 4];
        let mut t = Target { dst: &mut dst2, w: 6, clip: (3, 0, 6, 6) };
        blit_scaled(&mut t, 1.0, 1.0, &red, [0, 0, 0, 128]);
        assert_eq!(&dst2[(2 * 6 + 3) * 4..(2 * 6 + 3) * 4 + 4], &[128, 0, 0, 128]);
        // clipped: column 2 untouched
        assert_eq!(&dst2[(2 * 6 + 2) * 4..(2 * 6 + 2) * 4 + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn foreground_key_round_trips() {
        let k = GlyphKey { font: 0, id: 1, size_q: 64, bin: 0, italic: false, fg: 0 };
        assert_eq!(k.fg_rgb(), None);
        let k = GlyphKey { fg: 0x0100_0000 | u32::from_be_bytes([0, 1, 2, 3]), ..k };
        assert_eq!(k.fg_rgb(), Some([1, 2, 3]));
        // black text is still a given colour
        assert_eq!(GlyphKey { fg: 0x0100_0000, ..k }.fg_rgb(), Some([0, 0, 0]));
        assert_eq!(k.px(), 16.0);
    }
}
