//! Plain-text point clouds: one point per line, `x y z` then optional
//! columns, separated by spaces, tabs, commas or semicolons (XYZ, PTS, CSV
//! exports). Lines that do not start with three numbers (headers, comments,
//! a PTS point count) are skipped. Colours are read from columns 4-6
//! (`x y z r g b`) or 5-7 (`x y z intensity r g b`), as bytes, 16-bit
//! values or 0..1 floats, decided from a sample of lines.
//!
//! Parsing runs on all cores: the text is cut into chunks at line breaks,
//! parsed in parallel and concatenated in order.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use super::{lines, parse_f64, tokens, LoadOptions};
use crate::model::{try_vec, Bounds, Ctl, Model, PointVertex, PointsBuilder, MAX_POINTS, WHITE};

const CHUNK: usize = 4 << 20;

#[derive(Clone, Copy, Debug)]
struct Columns {
    /// First colour column, if the file has colours.
    rgb: Option<usize>,
    /// Multiplier to bytes.
    scale: f64,
}

fn data_line(line: &[u8]) -> Option<([f64; 3], usize)> {
    let mut t = tokens(line);
    let x = parse_f64(t.next()?)?;
    let y = parse_f64(t.next()?)?;
    let z = parse_f64(t.next()?)?;
    Some(([x, y, z], 3 + t.count()))
}

/// The first data line with three finite coordinates anywhere in the file.
fn first_finite_line(bytes: &[u8]) -> Option<[f64; 3]> {
    lines(bytes).filter_map(data_line).map(|(p, _)| p).find(|p| p.iter().all(|x| x.is_finite()))
}

fn columns(bytes: &[u8]) -> Option<(Option<[f64; 3]>, Columns)> {
    let mut origin = None;
    let mut any = false;
    let mut counts = [0usize; 16];
    let mut sample: Vec<Vec<f64>> = Vec::new();
    for line in lines(&bytes[..bytes.len().min(1 << 20)]) {
        let Some((p, n)) = data_line(line) else { continue };
        // the origin is the first finite point: organised clouds often start with NaN
        if p.iter().all(|x| x.is_finite()) {
            origin.get_or_insert(p);
        }
        any = true;
        counts[n.min(15)] += 1;
        sample.push(tokens(line).take(7).filter_map(parse_f64).collect());
        if sample.len() >= 1000 {
            break;
        }
    }
    if !any {
        return None;
    }
    let ncols = (0..16).max_by_key(|&k| counts[k]).unwrap_or(3);
    let rgb = match ncols {
        6 | 9 => Some(3),
        7 => Some(4),
        _ => None,
    };
    let rgb = rgb.and_then(|start| {
        let vals: Vec<f64> = sample.iter().filter(|r| r.len() >= start + 3).flat_map(|r| r[start..start + 3].to_vec()).collect();
        let max = vals.iter().cloned().fold(0.0f64, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let ints = vals.iter().all(|v| v.fract() == 0.0);
        if vals.is_empty() || min < 0.0 {
            None // e.g. normals in columns 4-6
        } else if max <= 1.0 && !ints {
            Some((start, 255.0))
        } else if ints && max <= 255.0 {
            Some((start, 1.0))
        } else if ints && max <= 65535.0 {
            Some((start, 1.0 / 257.0))
        } else {
            None
        }
    });
    Some((origin, Columns { rgb: rgb.map(|r| r.0), scale: rgb.map(|r| r.1).unwrap_or(1.0) }))
}

struct Part {
    points: Vec<PointVertex>,
    bounds: Bounds,
}

fn parse_chunk(text: &[u8], origin: [f64; 3], cols: Columns, stop: &AtomicBool) -> Result<Part, String> {
    let mut points = try_vec(text.len() / 24 + 16, "points")?;
    let mut bounds = Bounds::EMPTY;
    for (k, line) in lines(text).enumerate() {
        if k & 0xFFFF == 0 && stop.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let mut t = tokens(line);
        let (Some(x), Some(y), Some(z)) = (t.next().and_then(parse_f64), t.next().and_then(parse_f64), t.next().and_then(parse_f64)) else { continue };
        if !(x.is_finite() && y.is_finite() && z.is_finite()) {
            continue;
        }
        let color = match cols.rgb {
            Some(start) => {
                let mut c = [0f64; 3];
                let mut rest = t.skip(start - 3);
                for v in &mut c {
                    *v = rest.next().and_then(parse_f64).unwrap_or(0.0);
                }
                let b = |v: f64| (v * cols.scale).round().clamp(0.0, 255.0) as u8;
                [b(c[0]), b(c[1]), b(c[2]), 255]
            }
            None => WHITE,
        };
        let l = [(x - origin[0]) as f32, (y - origin[1]) as f32, (z - origin[2]) as f32];
        bounds.add([l[0] as f64, l[1] as f64, l[2] as f64]);
        if points.len() == points.capacity() {
            points.try_reserve(points.len()).map_err(|_| "not enough memory for the points".to_string())?;
        }
        points.push(PointVertex { pos: l, color });
    }
    Ok(Part { points, bounds })
}

/// Cut `bytes` into pieces of about `CHUNK` bytes that end at line breaks.
fn chunks(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        let mut end = (start + CHUNK).min(bytes.len());
        while end < bytes.len() && bytes[end - 1] != b'\n' {
            end += 1;
        }
        out.push(&bytes[start..end]);
        start = end;
    }
    out
}

/// Parse a text point cloud in parallel (unshuffled).
pub fn parse_xyz_points(bytes: &[u8], ctl: &Ctl) -> Result<PointsBuilder, String> {
    let (origin, cols) = columns(bytes).ok_or("no line starts with three numbers (x y z)")?;
    let origin = match origin {
        Some(o) => o,
        None => first_finite_line(bytes).ok_or("no point has three finite coordinates")?,
    };
    let pieces = chunks(bytes);
    let results: Mutex<Vec<Option<Result<Part, String>>>> = Mutex::new((0..pieces.len()).map(|_| None).collect());
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 16).min(pieces.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= pieces.len() || stop.load(Ordering::Relaxed) {
                    break;
                }
                let r = parse_chunk(pieces[i], origin, cols, &stop);
                if r.is_err() {
                    stop.store(true, Ordering::Relaxed);
                }
                if let Ok(mut g) = results.lock() {
                    g[i] = Some(r);
                }
                done.fetch_add(1, Ordering::Relaxed);
            });
        }
        // the calling thread watches for cancellation and reports progress
        while done.load(Ordering::Relaxed) < pieces.len() && !stop.load(Ordering::Relaxed) {
            if (ctl.cancelled)() {
                stop.store(true, Ordering::Relaxed);
                break;
            }
            (ctl.progress)(0.8 * done.load(Ordering::Relaxed) as f32 / pieces.len() as f32);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
    ctl.check()?;
    let parts = results.into_inner().map_err(|_| "a parser thread failed")?;
    let mut total = 0usize;
    for p in &parts {
        match p {
            Some(Ok(p)) => total += p.points.len(),
            Some(Err(e)) => return Err(e.clone()),
            None => return Err("cancelled".into()),
        }
    }
    if total > MAX_POINTS {
        return Err(format!("{total} points; the limit is {MAX_POINTS}"));
    }
    let mut points = try_vec(total, "points")?;
    let mut bounds = Bounds::EMPTY;
    for p in parts.into_iter().flatten().flatten() {
        bounds = bounds.union(&p.bounds);
        points.extend_from_slice(&p.points);
    }
    if points.is_empty() {
        return Err("no points".into());
    }
    Ok(PointsBuilder { origin, points, bounds, has_colors: cols.rgb.is_some() })
}

pub fn load(bytes: &[u8], opts: &LoadOptions, ctl: &Ctl) -> Result<Model, String> {
    parse_xyz_points(bytes, ctl)?.build("xyz", opts.shuffle, opts.max_points, ctl)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_and_colours() {
        let pb = parse_xyz_points(b"// x y z r g b\n1 2 3 255 0 0\n4,5,6,0,255,0\n7;8;9;0;0;255\nnot a point\n", &Ctl::none()).unwrap();
        assert_eq!(pb.points.len(), 3);
        assert!(pb.has_colors);
        assert_eq!(pb.origin, [1.0, 2.0, 3.0]);
        assert_eq!(pb.points[1].pos, [3.0, 3.0, 3.0]);
        assert_eq!(pb.points[1].color, [0, 255, 0, 255]);
        // x y z intensity r g b, with a PTS count line first
        let pb = parse_xyz_points(b"2\n0 0 0 -500 10 20 30\n1 1 1 -20 40 50 60\n", &Ctl::none()).unwrap();
        assert_eq!(pb.points[1].color, [40, 50, 60, 255]);
        // normals are not colours
        let pb = parse_xyz_points(b"0 0 0 0 0 -1\n1 0 0 0 1 0\n", &Ctl::none()).unwrap();
        assert!(!pb.has_colors);
        // float colours
        let pb = parse_xyz_points(b"0 0 0 0.5 1.0 0.25\n", &Ctl::none()).unwrap();
        assert_eq!(pb.points[0].color, [128, 255, 64, 255]);
        assert!(parse_xyz_points(b"hello\nworld\n", &Ctl::none()).is_err());
    }

    #[test]
    fn many_chunks_keep_order() {
        let mut s = String::new();
        for i in 0..400_000 {
            s.push_str(&format!("{i} {} 0.5\n", i % 7));
        }
        let pb = parse_xyz_points(s.as_bytes(), &Ctl::none()).unwrap();
        assert_eq!(pb.points.len(), 400_000);
        assert!(pb.points.iter().enumerate().all(|(i, p)| p.pos[0] as usize == i));
    }
}
