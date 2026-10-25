//! YUV -> RGBA conversion speed: `cargo run --release --example yuv_bench`.
//!
//! "reused" converts into one buffer again and again (best case); "fresh"
//! allocates a new buffer per frame like the player does (each shown frame
//! becomes its own image), so first-touch page faults are part of the cost.

use std::time::Instant;

use anypage_media::pattern::test_pattern;
use anypage_media::yuv::{i420_to_rgba, i420_to_rgba_auto, ColorSpace};

/// The straightforward per-pixel loop, for comparison.
fn naive(pic: &anypage_media::yuv::I420, out: &mut [u8]) {
    let (w, h) = (pic.width, pic.height);
    let cw = w.div_ceil(2);
    for y in 0..h {
        for x in 0..w {
            let l = 1.164 * (pic.y[y * w + x] as f32 - 16.0);
            let u = pic.u[(y / 2) * cw + x / 2] as f32 - 128.0;
            let v = pic.v[(y / 2) * cw + x / 2] as f32 - 128.0;
            let i = (y * w + x) * 4;
            out[i] = (l + 1.793 * v).clamp(0.0, 255.0) as u8;
            out[i + 1] = (l - 0.213 * u - 0.533 * v).clamp(0.0, 255.0) as u8;
            out[i + 2] = (l + 2.112 * u).clamp(0.0, 255.0) as u8;
            out[i + 3] = 255;
        }
    }
}

fn stats(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(|a, b| a.total_cmp(b));
    (v[0], v[v.len() / 2], v.iter().sum::<f64>() / v.len() as f64)
}

fn main() {
    for (w, h) in [(1280usize, 720usize), (1920, 1080)] {
        let pic = test_pattern(w, h, 3, 30.0, ColorSpace::BT709);
        let mut out = vec![0u8; w * h * 4];
        let reps = 60;
        for threads in [1usize, 2, 4] {
            let reused: Vec<f64> = (0..reps)
                .map(|_| {
                    let t = Instant::now();
                    i420_to_rgba(&pic.planes(), ColorSpace::BT709, &mut out, threads).unwrap();
                    t.elapsed().as_secs_f64() * 1000.0
                })
                .collect();
            let fresh: Vec<f64> = (0..reps)
                .map(|_| {
                    let t = Instant::now();
                    let mut o = vec![0u8; w * h * 4];
                    i420_to_rgba(&pic.planes(), ColorSpace::BT709, &mut o, threads).unwrap();
                    std::hint::black_box(&o);
                    t.elapsed().as_secs_f64() * 1000.0
                })
                .collect();
            let (rb, rm, _) = stats(reused);
            let (fb, fm, fa) = stats(fresh);
            println!("{w}x{h} {threads} thread(s): reused best {rb:.2} median {rm:.2} ms; fresh best {fb:.2} median {fm:.2} mean {fa:.2} ms");
        }
        let mut used = std::collections::BTreeMap::<usize, usize>::new();
        let auto: Vec<f64> = (0..reps)
            .map(|_| {
                let t = Instant::now();
                let mut o = vec![0u8; w * h * 4];
                *used.entry(i420_to_rgba_auto(&pic.planes(), ColorSpace::BT709, &mut o).unwrap()).or_default() += 1;
                std::hint::black_box(&o);
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        let (ab, am, aa) = stats(auto);
        println!("{w}x{h} auto (fresh): best {ab:.2} median {am:.2} mean {aa:.2} ms, frames per thread count {used:?}");
        let mut best = f64::MAX;
        for _ in 0..20 {
            let t = Instant::now();
            naive(&pic, &mut out);
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        }
        println!("{w}x{h} naive float: best {best:.2} ms");
    }
}
