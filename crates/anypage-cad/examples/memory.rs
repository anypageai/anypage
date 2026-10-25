//! Memory used by opening DXF files, measured with a counting allocator:
//! `cargo run --release --example memory [entities]`.
//!
//! Prints the live and peak heap for the `dxf` crate's parse alone and for
//! `CadDoc::open` (parse + scene), for a flat drawing and for a nested-block
//! file (a block inserted 100 times per level, three levels).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anypage_cad::CadDoc;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static A: Counting = Counting;

fn mb(b: usize) -> f64 {
    b as f64 / 1048576.0
}

fn flat(n: u64) -> Vec<u8> {
    let mut s = String::from("  0\nSECTION\n  2\nENTITIES\n");
    for i in 0..n {
        let x = (i % 1000) as f64;
        let y = (i / 1000) as f64 * 10.0;
        match i % 4 {
            0 | 1 => s.push_str(&format!("  0\nLINE\n  5\n{:X}\n  8\nL{}\n 10\n{x}\n 20\n{y}\n 11\n{}\n 21\n{}\n", 0x100 + i, i % 7, x + 0.8, y + 5.0)),
            2 => s.push_str(&format!("  0\nCIRCLE\n  5\n{:X}\n  8\nC\n 10\n{x}\n 20\n{y}\n 40\n0.4\n", 0x100 + i)),
            _ => s.push_str(&format!("  0\nTEXT\n  5\n{:X}\n  8\nT\n 10\n{x}\n 20\n{y}\n 40\n0.5\n  1\nT{i}\n", 0x100 + i)),
        }
    }
    s.push_str("  0\nENDSEC\n  0\nEOF\n");
    s.into_bytes()
}

/// B0: 100 lines; Bk: 100 inserts of B(k-1); model space: 100 inserts of B(levels-1).
fn nested(levels: usize) -> Vec<u8> {
    let mut s = String::from("  0\nSECTION\n  2\nBLOCKS\n");
    let mut h = 0x100u64;
    for b in 0..levels {
        s.push_str(&format!("  0\nBLOCK\n  5\n{h:X}\n  8\n0\n  2\nB{b}\n 70\n0\n 10\n0\n 20\n0\n"));
        h += 1;
        for i in 0..100 {
            if b == 0 {
                s.push_str(&format!("  0\nLINE\n  5\n{h:X}\n  8\n0\n 10\n{i}\n 20\n0\n 11\n{i}\n 21\n1\n"));
            } else {
                s.push_str(&format!("  0\nINSERT\n  5\n{h:X}\n  8\n0\n  2\nB{}\n 10\n{}\n 20\n0\n", b - 1, i as f64 * 200f64.powi(b as i32)));
            }
            h += 1;
        }
        s.push_str(&format!("  0\nENDBLK\n  5\n{h:X}\n  8\n0\n"));
        h += 1;
    }
    s.push_str("  0\nENDSEC\n  0\nSECTION\n  2\nENTITIES\n");
    for i in 0..100 {
        s.push_str(&format!("  0\nINSERT\n  5\n{h:X}\n  8\n0\n  2\nB{}\n 10\n0\n 20\n{}\n", levels - 1, i * 10));
        h += 1;
    }
    s.push_str("  0\nENDSEC\n  0\nEOF\n");
    s.into_bytes()
}

fn probe(what: &str, bytes: &[u8]) {
    // the font is loaded once per process; keep it out of the numbers
    let _ = anypage_cad::internals::font();
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let t = Instant::now();
    let d = dxf::Drawing::load(&mut &bytes[..]).expect("dxf");
    let (parse_ms, parse_live, parse_peak) = (t.elapsed().as_secs_f64() * 1000.0, LIVE.load(Ordering::Relaxed) - base, PEAK.load(Ordering::Relaxed) - base);
    drop(d);
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let base = LIVE.load(Ordering::Relaxed);
    let t = Instant::now();
    let doc = CadDoc::open(bytes).expect("open");
    let open_ms = t.elapsed().as_secs_f64() * 1000.0;
    let (live, peak) = (LIVE.load(Ordering::Relaxed) - base, PEAK.load(Ordering::Relaxed) - base);
    println!(
        "{what}: {:.1} MB DXF; dxf crate parse {parse_ms:.0} ms, live {:.1} MB, peak {:.1} MB; cad.open {open_ms:.0} ms, live {:.1} MB, peak {:.1} MB; {} groups",
        mb(bytes.len()),
        mb(parse_live),
        mb(parse_peak),
        mb(live),
        mb(peak),
        doc.scene().groups.len()
    );
}

fn main() {
    let n: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let levels: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    probe(&format!("{n} entities"), &flat(n));
    for l in 2..=levels {
        probe(&format!("nested blocks, {l} levels ({} lines drawn)", 100usize.pow(l as u32 + 1)), &nested(l));
    }
}
