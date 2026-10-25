//! Colour conversion between planar YUV 4:2:0 (what H.264 carries) and
//! RGBA8 (what the engine draws). Fixed-point, branch-free inner loops,
//! split into row bands across a few threads for large frames.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// YCbCr matrix of a video stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorSpace {
    pub matrix: Matrix,
    /// Full range (0..255) instead of studio range (Y 16..235, C 16..240).
    pub full_range: bool,
}

impl ColorSpace {
    pub const BT601: ColorSpace = ColorSpace { matrix: Matrix::Bt601, full_range: false };
    pub const BT709: ColorSpace = ColorSpace { matrix: Matrix::Bt709, full_range: false };

    /// The usual convention when a stream does not say: BT.709 for HD, BT.601 for SD.
    pub fn for_height(h: usize) -> ColorSpace {
        if h >= 720 {
            Self::BT709
        } else {
            Self::BT601
        }
    }

    /// From H.264 VUI `matrix_coefficients` (1 = BT.709; 5, 6 = BT.601).
    pub fn from_vui(matrix: Option<u8>, full_range: bool, height: usize) -> ColorSpace {
        let m = match matrix {
            Some(1) => Matrix::Bt709,
            Some(5) | Some(6) => Matrix::Bt601,
            _ => Self::for_height(height).matrix,
        };
        ColorSpace { matrix: m, full_range }
    }

    fn to_rgb(self) -> ToRgb {
        // floating coefficients scaled by 2^14
        let s = |x: f64| (x * 16384.0).round() as i32;
        let (kr, kb) = match self.matrix {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
        };
        let kg = 1.0 - kr - kb;
        let (y_mul, c_mul, y_off) = if self.full_range { (1.0, 1.0, 0) } else { (255.0 / 219.0, 255.0 / 224.0, 16) };
        ToRgb {
            y_mul: s(y_mul),
            y_off,
            rv: s(2.0 * (1.0 - kr) * c_mul),
            gu: s(2.0 * (1.0 - kb) * kb / kg * c_mul),
            gv: s(2.0 * (1.0 - kr) * kr / kg * c_mul),
            bu: s(2.0 * (1.0 - kb) * c_mul),
        }
    }

    fn to_yuv(self) -> ToYuv {
        let s = |x: f64| (x * 16384.0).round() as i32;
        let (kr, kb) = match self.matrix {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
        };
        let kg = 1.0 - kr - kb;
        let (ys, cs, yo) = if self.full_range { (1.0, 1.0, 0) } else { (219.0 / 255.0, 224.0 / 255.0, 16) };
        ToYuv {
            yr: s(kr * ys),
            yg: s(kg * ys),
            yb: s(kb * ys),
            y_off: yo,
            ur: s(-kr / (2.0 * (1.0 - kb)) * cs),
            ug: s(-kg / (2.0 * (1.0 - kb)) * cs),
            ub: s(0.5 * cs),
            vr: s(0.5 * cs),
            vg: s(-kg / (2.0 * (1.0 - kr)) * cs),
            vb: s(-kb / (2.0 * (1.0 - kr)) * cs),
        }
    }
}

#[derive(Clone, Copy)]
struct ToRgb {
    y_mul: i32,
    y_off: i32,
    rv: i32,
    gu: i32,
    gv: i32,
    bu: i32,
}

#[derive(Clone, Copy)]
struct ToYuv {
    yr: i32,
    yg: i32,
    yb: i32,
    y_off: i32,
    ur: i32,
    ug: i32,
    ub: i32,
    vr: i32,
    vg: i32,
    vb: i32,
}

/// Borrowed I420 planes.
#[derive(Clone, Copy)]
pub struct Planes<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub y_stride: usize,
    pub uv_stride: usize,
    pub width: usize,
    pub height: usize,
}

impl Planes<'_> {
    pub fn check(&self) -> Result<(), String> {
        let (w, h) = (self.width, self.height);
        if w == 0 || h == 0 {
            return Err("empty picture".into());
        }
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let ok = self.y_stride >= w
            && self.uv_stride >= cw
            && self.y.len() >= self.y_stride * (h - 1) + w
            && self.u.len() >= self.uv_stride * (ch - 1) + cw
            && self.v.len() >= self.uv_stride * (ch - 1) + cw;
        if ok {
            Ok(())
        } else {
            Err(format!("YUV planes too small for {w}x{h}"))
        }
    }
}

/// An owned I420 picture with tight strides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I420 {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420 {
    pub fn new(width: usize, height: usize) -> I420 {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        I420 { width, height, y: vec![16; width * height], u: vec![128; cw * ch], v: vec![128; cw * ch] }
    }
    pub fn planes(&self) -> Planes<'_> {
        Planes { y: &self.y, u: &self.u, v: &self.v, y_stride: self.width, uv_stride: self.width.div_ceil(2), width: self.width, height: self.height }
    }
}

impl openh264::formats::YUVSource for I420 {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }
    fn strides(&self) -> (usize, usize, usize) {
        let c = self.width.div_ceil(2);
        (self.width, c, c)
    }
    fn y(&self) -> &[u8] {
        &self.y
    }
    fn u(&self) -> &[u8] {
        &self.u
    }
    fn v(&self) -> &[u8] {
        &self.v
    }
}

#[inline(always)]
fn clamp8(x: i32) -> u8 {
    x.clamp(0, 255) as u8
}

#[inline(always)]
fn clamp_px(x: i32) -> u32 {
    (x >> 14).clamp(0, 255) as u32
}

/// Rows `y0..` of the picture into `out`, with the widest vector unit the
/// CPU has (AVX2 on x86-64 when present; NEON is baseline on aarch64).
fn rgba_rows(p: &Planes, c: ToRgb, out: &mut [u8], y0: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: the CPU supports AVX2 (checked just above); the
            // function body is ordinary safe Rust compiled for AVX2.
            unsafe { rgba_rows_avx2(p, c, out, y0) };
            return;
        }
    }
    rgba_rows_generic(p, c, out, y0)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn rgba_rows_avx2(p: &Planes, c: ToRgb, out: &mut [u8], y0: usize) {
    rgba_rows_generic(p, c, out, y0)
}

/// Chroma terms are expanded to one per pixel once per chroma row, so the
/// per-pixel loop is straight-line arithmetic over parallel arrays that the
/// compiler vectorises.
#[inline(always)]
fn rgba_rows_generic(p: &Planes, c: ToRgb, out: &mut [u8], y0: usize) {
    let w = p.width;
    let rows = out.len() / (w * 4);
    const R: i32 = 1 << 13;
    let cw = w.div_ceil(2);
    let (mut dr, mut dg, mut db) = (vec![0i32; w], vec![0i32; w], vec![0i32; w]);
    let mut chroma_row = usize::MAX;
    for r in 0..rows {
        let y = y0 + r;
        let cy = y / 2;
        if cy != chroma_row {
            chroma_row = cy;
            let urow = &p.u[cy * p.uv_stride..cy * p.uv_stride + cw];
            let vrow = &p.v[cy * p.uv_stride..cy * p.uv_stride + cw];
            for (x, ((a, b), d)) in dr.iter_mut().zip(dg.iter_mut()).zip(db.iter_mut()).enumerate() {
                let (u, v) = (urow[x >> 1] as i32 - 128, vrow[x >> 1] as i32 - 128);
                *a = c.rv * v + R - c.y_off * c.y_mul;
                *b = R - c.gu * u - c.gv * v - c.y_off * c.y_mul;
                *d = c.bu * u + R - c.y_off * c.y_mul;
            }
        }
        let yrow = &p.y[y * p.y_stride..y * p.y_stride + w];
        let orow = &mut out[r * w * 4..(r + 1) * w * 4];
        for ((((o, &l), &a), &b), &d) in orow.as_chunks_mut::<4>().0.iter_mut().zip(yrow).zip(&dr).zip(&dg).zip(&db) {
            let l = l as i32 * c.y_mul;
            let px = clamp_px(l + a) | clamp_px(l + b) << 8 | clamp_px(l + d) << 16 | 0xFF00_0000;
            o.copy_from_slice(&px.to_le_bytes());
        }
    }
}

/// Frames below this many pixels convert on one thread (spawning would cost
/// about as much as it saves).
pub const THREADED_FROM: usize = 400_000;

/// How many threads a conversion of `pixels` may use: one below
/// [`THREADED_FROM`], else up to four (on a Ryzen 9 5950X a 1080p frame takes
/// 2.7 ms on one thread, 1.5 ms on two and 1.0 ms on four; 720p 1.1 / 0.8 /
/// 0.6 ms). [`i420_to_rgba_auto`] falls back to one thread when a loaded
/// machine makes spawning slower than converting alone.
pub fn threads_for(pixels: usize) -> usize {
    if pixels < THREADED_FROM {
        return 1;
    }
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 4)
}

/// Running cost (picoseconds per pixel, 0 = not measured yet) of converting
/// on one thread and on [`threads_for`] threads.
static COST_SINGLE: AtomicU64 = AtomicU64::new(0);
static COST_THREADED: AtomicU64 = AtomicU64::new(0);
static AUTO_CALLS: AtomicU64 = AtomicU64::new(0);

/// Calls that alternate between both ways before the estimates are used.
const AUTO_WARMUP: u64 = 6;
/// After the warm-up, one call in this many uses the way not in favour, so a
/// change in machine load is noticed.
const AUTO_RECHECK: u64 = 16;

/// [`i420_to_rgba`] with the thread count chosen by measurement: frames large
/// enough for threads alternate between one thread and [`threads_for`]
/// threads for a few calls, then use the cheaper way, re-checking the other
/// one call in 16 (a running average, each new sample weighted 1/4).
/// Returns the threads used.
pub fn i420_to_rgba_auto(p: &Planes, cs: ColorSpace, dst: &mut [u8]) -> Result<usize, String> {
    let pixels = p.width.saturating_mul(p.height).max(1);
    let many = threads_for(pixels);
    if many == 1 {
        i420_to_rgba(p, cs, dst, 1)?;
        return Ok(1);
    }
    let n = AUTO_CALLS.fetch_add(1, Ordering::Relaxed);
    let (single, threaded) = (COST_SINGLE.load(Ordering::Relaxed), COST_THREADED.load(Ordering::Relaxed));
    let use_threads = if n < AUTO_WARMUP || single == 0 || threaded == 0 {
        n % 2 == 1
    } else {
        (threaded <= single) != n.is_multiple_of(AUTO_RECHECK)
    };
    let threads = if use_threads { many } else { 1 };
    let t0 = Instant::now();
    i420_to_rgba(p, cs, dst, threads)?;
    let cost = ((t0.elapsed().as_nanos() as u64).saturating_mul(1000) / pixels as u64).max(1);
    let slot = if use_threads { &COST_THREADED } else { &COST_SINGLE };
    let old = slot.load(Ordering::Relaxed);
    slot.store(if old == 0 { cost } else { (old.saturating_mul(3).saturating_add(cost) / 4).max(1) }, Ordering::Relaxed);
    Ok(threads)
}

/// I420 to opaque RGBA8 (which is also premultiplied). `dst` is `w * h * 4`.
pub fn i420_to_rgba(p: &Planes, cs: ColorSpace, dst: &mut [u8], threads: usize) -> Result<(), String> {
    p.check()?;
    let (w, h) = (p.width, p.height);
    if dst.len() != w * h * 4 {
        return Err(format!("RGBA buffer is {} bytes, {w}x{h} needs {}", dst.len(), w * h * 4));
    }
    let c = cs.to_rgb();
    let threads = threads.clamp(1, 16);
    if threads == 1 {
        rgba_rows(p, c, dst, 0);
        return Ok(());
    }
    // bands of an even number of rows, so chroma rows are never shared
    let band = (h.div_ceil(threads) + 1) & !1;
    std::thread::scope(|s| {
        for (k, chunk) in dst.chunks_mut(band * w * 4).enumerate() {
            s.spawn(move || rgba_rows(p, c, chunk, k * band));
        }
    });
    Ok(())
}

/// I420 to RGBA8 at 1/`step` size in each direction (nearest sample), for previews.
pub fn i420_to_rgba_sampled(p: &Planes, cs: ColorSpace, step: usize) -> Result<(usize, usize, Vec<u8>), String> {
    p.check()?;
    let step = step.max(1);
    let (w, h) = (p.width.div_ceil(step), p.height.div_ceil(step));
    let c = cs.to_rgb();
    let mut out = vec![0u8; w * h * 4];
    for oy in 0..h {
        let y = (oy * step).min(p.height - 1);
        for ox in 0..w {
            let x = (ox * step).min(p.width - 1);
            let l = (p.y[y * p.y_stride + x] as i32 - c.y_off) * c.y_mul;
            let ci = (y / 2) * p.uv_stride + x / 2;
            let (u, v) = (p.u[ci] as i32 - 128, p.v[ci] as i32 - 128);
            let o = &mut out[(oy * w + ox) * 4..(oy * w + ox) * 4 + 4];
            o[0] = clamp8((l + c.rv * v + 8192) >> 14);
            o[1] = clamp8((l - c.gu * u - c.gv * v + 8192) >> 14);
            o[2] = clamp8((l + c.bu * u + 8192) >> 14);
            o[3] = 255;
        }
    }
    Ok((w, h, out))
}

/// RGBA8 (alpha ignored) to I420, chroma from the 2x2 average.
pub fn rgba_to_i420(rgba: &[u8], width: usize, height: usize, cs: ColorSpace) -> Result<I420, String> {
    if width == 0 || height == 0 || rgba.len() < width * height * 4 {
        return Err(format!("RGBA buffer too small for {width}x{height}"));
    }
    let k = cs.to_yuv();
    let mut out = I420::new(width, height);
    let cw = width.div_ceil(2);
    const R: i32 = 1 << 13;
    for (i, px) in rgba.as_chunks::<4>().0.iter().take(width * height).enumerate() {
        let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
        out.y[i] = clamp8(((k.yr * r + k.yg * g + k.yb * b + R) >> 14) + k.y_off);
    }
    for cy in 0..height.div_ceil(2) {
        for cx in 0..cw {
            let (mut r, mut g, mut b, mut n) = (0i32, 0i32, 0i32, 0i32);
            for dy in 0..2 {
                for dx in 0..2 {
                    let (x, y) = (cx * 2 + dx, cy * 2 + dy);
                    if x < width && y < height {
                        let i = (y * width + x) * 4;
                        r += rgba[i] as i32;
                        g += rgba[i + 1] as i32;
                        b += rgba[i + 2] as i32;
                        n += 1;
                    }
                }
            }
            let (r, g, b) = (r / n, g / n, b / n);
            out.u[cy * cw + cx] = clamp8(((k.ur * r + k.ug * g + k.ub * b + R) >> 14) + 128);
            out.v[cy * cw + cx] = clamp8(((k.vr * r + k.vg * g + k.vb * b + R) >> 14) + 128);
        }
    }
    Ok(out)
}

/// The YUV triple of an RGB colour.
pub fn rgb_to_yuv(rgb: [u8; 3], cs: ColorSpace) -> [u8; 3] {
    let k = cs.to_yuv();
    let (r, g, b) = (rgb[0] as i32, rgb[1] as i32, rgb[2] as i32);
    const R: i32 = 1 << 13;
    [
        clamp8(((k.yr * r + k.yg * g + k.yb * b + R) >> 14) + k.y_off),
        clamp8(((k.ur * r + k.ug * g + k.ub * b + R) >> 14) + 128),
        clamp8(((k.vr * r + k.vg * g + k.vb * b + R) >> 14) + 128),
    ]
}

/// Peak signal-to-noise ratio (dB) between two RGBA8 images over RGB.
pub fn psnr_rgba(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let mut se = 0u64;
    let mut count = 0u64;
    for (pa, pb) in a[..n].as_chunks::<4>().0.iter().zip(b[..n].as_chunks::<4>().0) {
        for c in 0..3 {
            let d = pa[c] as i64 - pb[c] as i64;
            se += (d * d) as u64;
        }
        count += 3;
    }
    if se == 0 {
        return 99.0;
    }
    let mse = se as f64 / count as f64;
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLOURS: [[u8; 3]; 8] = [[235, 235, 235], [235, 235, 16], [16, 235, 235], [16, 235, 16], [235, 16, 235], [235, 16, 16], [16, 16, 235], [16, 16, 16]];

    #[test]
    fn colours_survive_a_round_trip() {
        for cs in [ColorSpace::BT601, ColorSpace::BT709, ColorSpace { matrix: Matrix::Bt709, full_range: true }] {
            for c in COLOURS.iter().chain(&[[0, 0, 0], [255, 255, 255], [128, 64, 200], [12, 250, 90]]) {
                let yuv = rgb_to_yuv(*c, cs);
                let p = Planes { y: &[yuv[0]; 4], u: &[yuv[1]], v: &[yuv[2]], y_stride: 2, uv_stride: 1, width: 2, height: 2 };
                let mut out = [0u8; 16];
                i420_to_rgba(&p, cs, &mut out, 1).unwrap();
                for ch in 0..3 {
                    assert!((out[ch] as i32 - c[ch] as i32).abs() <= 2, "{cs:?} {c:?} -> {:?}", &out[..4]);
                }
                assert_eq!(out[3], 255);
            }
        }
    }

    #[test]
    fn studio_range_black_and_white() {
        let p = Planes { y: &[16, 235, 16, 235], u: &[128], v: &[128], y_stride: 2, uv_stride: 1, width: 2, height: 2 };
        let mut out = [0u8; 16];
        i420_to_rgba(&p, ColorSpace::BT709, &mut out, 1).unwrap();
        assert_eq!(&out[..8], &[0, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn threaded_matches_single_and_odd_sizes_work() {
        for (w, h) in [(641usize, 479usize), (640, 480), (1, 1), (3, 5)] {
            let mut img = vec![0u8; w * h * 4];
            for (i, px) in img.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                px.copy_from_slice(&[(i * 7) as u8, (i * 13 / w) as u8, (i % 251) as u8, 255]);
            }
            let yuv = rgba_to_i420(&img, w, h, ColorSpace::BT601).unwrap();
            let mut a = vec![0u8; w * h * 4];
            let mut b = vec![0u8; w * h * 4];
            i420_to_rgba(&yuv.planes(), ColorSpace::BT601, &mut a, 1).unwrap();
            i420_to_rgba(&yuv.planes(), ColorSpace::BT601, &mut b, 4).unwrap();
            assert_eq!(a, b, "{w}x{h}");
        }
        let small = I420::new(4, 4);
        let mut wrong = vec![0u8; 10];
        assert!(i420_to_rgba(&small.planes(), ColorSpace::BT601, &mut wrong, 1).is_err());
        let bad = Planes { y: &[0; 3], u: &[0], v: &[0], y_stride: 2, uv_stride: 1, width: 2, height: 2 };
        assert!(i420_to_rgba(&bad, ColorSpace::BT601, &mut [0u8; 16], 1).is_err());
    }

    #[test]
    fn auto_threading_gives_the_same_pixels() {
        let (w, h) = (1280usize, 720usize);
        let mut img = vec![0u8; w * h * 4];
        for (i, px) in img.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            px.copy_from_slice(&[(i * 7) as u8, (i / w) as u8, (i % 253) as u8, 255]);
        }
        let yuv = rgba_to_i420(&img, w, h, ColorSpace::BT709).unwrap();
        let mut one = vec![0u8; w * h * 4];
        i420_to_rgba(&yuv.planes(), ColorSpace::BT709, &mut one, 1).unwrap();
        let mut used = std::collections::BTreeSet::new();
        for _ in 0..40 {
            let mut auto = vec![0u8; w * h * 4];
            used.insert(i420_to_rgba_auto(&yuv.planes(), ColorSpace::BT709, &mut auto).unwrap());
            assert_eq!(auto, one);
        }
        // both ways were measured, and only the two candidates were used
        assert!(used.contains(&1) && used.iter().all(|&t| t == 1 || t == threads_for(w * h)), "{used:?}");
        assert_eq!(threads_for(320 * 240), 1);
        let mut small = vec![0u8; 16];
        assert_eq!(i420_to_rgba_auto(&I420::new(2, 2).planes(), ColorSpace::BT601, &mut small).unwrap(), 1);
    }

    #[test]
    fn psnr_of_identical_and_noisy() {
        let a = vec![100u8; 400];
        assert_eq!(psnr_rgba(&a, &a), 99.0);
        let mut b = a.clone();
        b[0] = 110;
        assert!(psnr_rgba(&a, &b) > 30.0);
    }
}
