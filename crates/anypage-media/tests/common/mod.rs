//! Test helpers: PNG output (stored deflate, no dependencies) and waiting.

#![allow(dead_code)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    !c
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut c = kind.to_vec();
    c.extend_from_slice(data);
    out.extend_from_slice(&c);
    out.extend_from_slice(&crc32(&c).to_be_bytes());
}

/// RGBA8 to PNG bytes.
pub fn png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for row in rgba.chunks_exact(w as usize * 4).take(h as usize) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push(if i + 1 == blocks.len() { 1 } else { 0 });
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut bsum) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65_521;
        bsum = (bsum + a) % 65_521;
    }
    z.extend_from_slice(&((bsum << 16) | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

pub fn out_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-output");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn save_png(name: &str, w: u32, h: u32, rgba: &[u8]) -> PathBuf {
    let p = out_dir().join(name);
    std::fs::write(&p, png(w, h, rgba)).expect("write png");
    p
}

/// Poll `f` until it returns `Some`, or panic after `secs`.
pub fn wait_for<T>(secs: f64, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let t0 = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(t0.elapsed() < Duration::from_secs_f64(secs), "timed out after {secs} s waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Mean absolute difference of RGB inside a rectangle of two RGBA images.
pub fn region_diff(a: &[u8], b: &[u8], w: usize, x0: usize, y0: usize, x1: usize, y1: usize) -> f64 {
    let mut s = 0u64;
    let mut n = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = (y * w + x) * 4;
            for c in 0..3 {
                s += (a[i + c] as i64 - b[i + c] as i64).unsigned_abs();
                n += 1;
            }
        }
    }
    s as f64 / n.max(1) as f64
}
