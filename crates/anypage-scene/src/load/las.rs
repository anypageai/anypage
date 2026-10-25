//! LAS 1.0-1.4 and LAZ (via the pure-Rust `laz` decompressor, so it builds
//! for Android too) with the `las` crate. Points are read in chunks through
//! its column accessors (no per-point structs), positions are made relative
//! to the header's bounds centre in `f64` before narrowing to `f32`, and RGB
//! is kept when the point format has it (8-bit values stored in 16-bit
//! fields are detected).
//!
//! Neither crate checks header counts against the file before allocating,
//! and a failed allocation aborts the process (no `catch_unwind` helps), so
//! [`preflight`] checks every size they would allocate from first: the
//! extended VLR length, the LAZ chunk table length, the layer sizes of
//! LAS 1.4 (layered) LAZ chunks, the point count a LAZ file of this size can
//! hold, and the record length. Reading then fills at most [`FILL_BYTES`]
//! per call, with cancellation checked between calls.

use std::io::Cursor;
use std::sync::Arc;

use super::{Bytes, LoadOptions};
use crate::model::{Ctl, Model, PointsBuilder, MAX_POINTS, WHITE};

/// Most bytes one `fill_points` call decodes into (its buffer is
/// `points * record length`, allocated before any data is read).
pub const FILL_BYTES: usize = 32 << 20;
const FILL_POINTS: usize = 1 << 20;
/// Longest LAZ point record accepted (base formats are 20-67 bytes; the
/// rest are extra bytes, rarely more than a few hundred).
pub const MAX_LAZ_RECORD: usize = 4096;

struct Shared(Arc<Vec<u8>>);

impl AsRef<[u8]> for Shared {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

fn at<const N: usize>(b: &[u8], pos: u64) -> Option<[u8; N]> {
    let start = usize::try_from(pos).ok()?;
    b.get(start..start.checked_add(N)?)?.try_into().ok()
}
fn u16_at(b: &[u8], pos: u64) -> Option<u16> {
    at::<2>(b, pos).map(u16::from_le_bytes)
}
fn u32_at(b: &[u8], pos: u64) -> Option<u32> {
    at::<4>(b, pos).map(u32::from_le_bytes)
}
fn u64_at(b: &[u8], pos: u64) -> Option<u64> {
    at::<8>(b, pos).map(u64::from_le_bytes)
}
fn i64_at(b: &[u8], pos: u64) -> Option<i64> {
    at::<8>(b, pos).map(i64::from_le_bytes)
}

/// What the LASzip VLR says about the compressed layout.
struct LazLayout {
    /// 1 point-wise, 2 point-wise chunked, 3 layered chunked.
    compressor: u16,
    /// `None` for variable-size chunks.
    chunk_size: Option<u32>,
    /// Bytes of the raw first point at the start of every chunk.
    raw_point: u64,
    /// Number of `u32` layer sizes after a layered chunk's point count.
    layers: u64,
}

/// The `laszip encoded` / 22204 VLR, found the way `las` finds it (regular
/// VLRs only), parsed like `laz::LazVlr::read_from`.
fn laz_layout(b: &[u8], header_size: u16, n_vlrs: u32) -> Option<LazLayout> {
    let mut pos = header_size as u64;
    for _ in 0..n_vlrs {
        let user = at::<16>(b, pos + 2)?;
        let record = u16_at(b, pos + 18)?;
        let len = u16_at(b, pos + 20)? as u64;
        let data = pos + 54;
        let name_end = user.iter().position(|&c| c == 0).unwrap_or(16);
        if &user[..name_end] == b"laszip encoded" && record == 22204 {
            let compressor = u16_at(b, data)?;
            let chunk_size = match u32_at(b, data + 12)? {
                0 | u32::MAX => None,
                c => Some(c),
            };
            let items = u16_at(b, data + 32)? as u64;
            let (mut raw_point, mut layers) = (0u64, 0u64);
            for i in 0..items {
                let ty = u16_at(b, data + 34 + 6 * i)?;
                let size = u16_at(b, data + 36 + 6 * i)? as u64;
                raw_point += size;
                layers += match ty {
                    10 => 9,   // Point14: xy, z, classification, flags, intensity, scan angle, user data, source, gps time
                    11 => 1,   // RGB14
                    12 => 2,   // RGBNIR14 (rgb + nir)
                    13 => 1,   // WavePacket14
                    14 => size, // Byte14: one layer per byte
                    _ => 0,
                };
            }
            return Some(LazLayout { compressor, chunk_size, raw_point, layers });
        }
        pos = data + len;
    }
    None
}

/// Reject headers that would make `las` or `laz` allocate far more than the
/// file could justify. Anything this cannot parse is left to the readers,
/// which then fail with an ordinary error.
pub(crate) fn preflight(b: &[u8]) -> Result<(), String> {
    let len = b.len() as u64;
    let (Some(header_size), Some(data_start), Some(n_vlrs), Some(format), Some(record_len), Some(legacy)) =
        (u16_at(b, 94), u32_at(b, 96), u32_at(b, 100), b.get(104).copied(), u16_at(b, 105), u32_at(b, 107))
    else {
        return Ok(());
    };
    let v14 = b.get(24) == Some(&1) && b.get(25).is_some_and(|m| *m >= 4);
    let points = if legacy > 0 { legacy as u64 } else if v14 { u64_at(b, 247).unwrap_or(0) } else { 0 };
    if points > MAX_POINTS as u64 {
        return Err(format!("{points} points; the limit is {MAX_POINTS} (use a thinned file)"));
    }
    // LAS 1.4 extended VLR: `las` reads the first one, sized by a u64 length
    if v14 {
        if let (Some(start), Some(n)) = (u64_at(b, 235), u32_at(b, 243)) {
            if start > 0 && n > 0 {
                if let Some(declared) = start.checked_add(20).and_then(|p| u64_at(b, p)) {
                    let room = len.saturating_sub(start.saturating_add(60));
                    if declared > room {
                        return Err(format!("an extended VLR declares {declared} bytes but only {room} follow it"));
                    }
                }
            }
        }
    }
    if format & 0x80 == 0 {
        return Ok(()); // uncompressed: the point count is checked against the size in `load`
    }
    if record_len as usize > MAX_LAZ_RECORD {
        return Err(format!("LAZ point records of {record_len} bytes; the limit is {MAX_LAZ_RECORD}"));
    }
    let Some(laz) = laz_layout(b, header_size, n_vlrs) else { return Ok(()) };
    let data_start = data_start as u64;
    let raw = laz.raw_point.max(1);
    // a fixed-size chunk holds at most `chunk_size` points and starts with one raw point
    if let (2 | 3, Some(c)) = (laz.compressor, laz.chunk_size) {
        let chunks = len.saturating_sub(data_start + 8) / raw;
        let most = chunks.saturating_mul(c as u64);
        if points > most {
            return Err(format!("the header declares {points} points but a LAZ file of this size holds at most {most}"));
        }
    }
    // the chunk table: `laz` reserves room for its declared entry count
    let mut table_chunks: Option<u64> = None;
    if matches!(laz.compressor, 2 | 3) {
        let mut offset = i64_at(b, data_start).unwrap_or(0);
        if offset <= data_start as i64 {
            offset = len.checked_sub(8).and_then(|p| i64_at(b, p)).unwrap_or(0);
        }
        if offset > data_start as i64 {
            if let Some(n) = u32_at(b, offset as u64 + 4) {
                let n = n as u64;
                let by_points = match laz.chunk_size {
                    Some(c) => points.div_ceil(c as u64) + 1,
                    None => points + 1,
                };
                let by_size = len / raw + 1;
                if n > by_points.min(by_size) {
                    return Err(format!("the LAZ chunk table declares {n} chunks for {points} points in {len} bytes"));
                }
                table_chunks = Some(n);
            }
        }
    }
    // layered chunks (LAS 1.4 point formats): every chunk header lists the
    // byte size of each layer, and `laz` allocates that much per layer
    if laz.compressor == 3 {
        check_layers(b, data_start + 8, &laz, points, table_chunks)?;
    }
    Ok(())
}

/// Walk the chunks `laz` will read in order (it reads sequentially) and
/// check that each chunk's layers fit in the file.
fn check_layers(b: &[u8], first: u64, laz: &LazLayout, points: u64, table_chunks: Option<u64>) -> Result<(), String> {
    let len = b.len() as u64;
    let head = laz.raw_point + 4 + 4 * laz.layers;
    // How many chunks `laz` reads: with fixed-size chunks, enough for the
    // header's point count. With variable-size chunks it follows the chunk
    // table, or each chunk's own point count when the table cannot be read;
    // cover both.
    let fixed = laz.chunk_size.map(|c| points.div_ceil(c as u64));
    let mut pos = first;
    let (mut chunk, mut seen) = (0u64, 0u64);
    let mut counts_done = false;
    loop {
        let done = match fixed {
            Some(n) => chunk >= n,
            None => table_chunks.is_none_or(|n| chunk >= n) && (counts_done || seen >= points),
        };
        if done || pos.saturating_add(head) > len {
            return Ok(()); // a header past the end fails in `laz` before it allocates
        }
        let count = u32_at(b, pos + laz.raw_point).unwrap_or(0) as u64;
        let mut sum = 0u64;
        for i in 0..laz.layers {
            sum += u32_at(b, pos + laz.raw_point + 4 + 4 * i).unwrap_or(0) as u64;
        }
        let room = len - (pos + head);
        if sum > room {
            return Err(format!("LAZ chunk {chunk} declares {sum} bytes of layers but the file has {room} bytes left"));
        }
        pos += head + sum;
        chunk += 1;
        // a chunk that claims no points: `laz` never leaves it
        counts_done |= count == 0;
        seen = seen.saturating_add(count);
    }
}

pub fn load(bytes: &Bytes, opts: &LoadOptions, ctl: &Ctl) -> Result<Model, String> {
    preflight(bytes)?;
    let mut reader = las::Reader::new(Cursor::new(Shared(bytes.clone()))).map_err(|e| format!("not a valid LAS/LAZ file: {e}"))?;
    let header = reader.header().clone();
    let total = header.number_of_points();
    let format = header.point_format();
    if total > MAX_POINTS as u64 {
        return Err(format!("{total} points; the limit is {MAX_POINTS} (use a thinned file)"));
    }
    if !format.is_compressed && total.saturating_mul(format.len() as u64) > bytes.len() as u64 {
        return Err(format!("the header declares {total} points but the file is too short for them"));
    }
    if format.is_compressed && format.len() as usize > MAX_LAZ_RECORD {
        return Err(format!("LAZ point records of {} bytes; the limit is {MAX_LAZ_RECORD}", format.len()));
    }
    let b = header.bounds();
    let centre = [(b.min.x + b.max.x) * 0.5, (b.min.y + b.max.y) * 0.5, (b.min.z + b.max.z) * 0.5];
    let origin = if centre.iter().all(|c| c.is_finite()) && b.min.x <= b.max.x { centre } else { [f64::NAN; 3] };
    // LAZ counts cannot be checked exactly up front: reserve for what the bytes could hold, grow if needed
    let capacity = if format.is_compressed { (total as usize).min(bytes.len() / 2) } else { total as usize };
    let mut pb = PointsBuilder::new(origin, capacity)?;
    pb.has_colors = format.has_color;
    let mut pd = las::PointDataBuilder::new().for_header(&header).build();
    // every fill allocates `fill * record length` bytes before decoding
    let fill = (FILL_BYTES / pd.record_len().max(1)).clamp(1, FILL_POINTS) as u64;
    // 16-bit colour channels, unless the first chunk shows 8-bit values
    let mut shift: Option<u32> = None;
    let mut read = 0u64;
    while read < total {
        ctl.check()?;
        let n = reader.fill_points(fill, &mut pd).map_err(|e| format!("reading points: {e}"))?;
        if n == 0 {
            break;
        }
        read += n;
        (ctl.progress)(0.85 * read as f32 / total.max(1) as f32);
        match pd.rgb() {
            Some(rgb) => {
                let rgb: Vec<(u16, u16, u16)> = rgb.collect();
                let s = *shift.get_or_insert_with(|| if rgb.iter().all(|c| c.0 < 256 && c.1 < 256 && c.2 < 256) { 0 } else { 8 });
                for (((x, y), z), c) in pd.x().zip(pd.y()).zip(pd.z()).zip(rgb) {
                    pb.push([x, y, z], [(c.0 >> s).min(255) as u8, (c.1 >> s).min(255) as u8, (c.2 >> s).min(255) as u8, 255])?;
                }
            }
            None => {
                for ((x, y), z) in pd.x().zip(pd.y()).zip(pd.z()) {
                    pb.push([x, y, z], WHITE)?;
                }
            }
        }
    }
    if pb.points.is_empty() {
        return Err("the file has no points".into());
    }
    let fmt = if format.is_compressed { "laz" } else { "las" };
    pb.build(fmt, opts.shuffle, opts.max_points, ctl)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_or_foreign_bytes_pass_to_the_reader() {
        assert!(preflight(b"").is_ok());
        assert!(preflight(&[0u8; 100]).is_ok());
        assert!(preflight(&[0xFFu8; 300]).is_err(), "a u32::MAX point count is over the limit");
    }

    /// Layered chunks: 4 raw bytes, the point count, two layer sizes, the
    /// layers; then what a chunk table would look like (with sizes that
    /// would fail the check if it were walked as a chunk).
    fn chunks(layers: &[(u32, [u32; 2])]) -> Vec<u8> {
        let mut b = Vec::new();
        for (count, sizes) in layers {
            b.extend([7u8; 4]);
            b.extend(count.to_le_bytes());
            for s in sizes {
                b.extend(s.to_le_bytes());
            }
            // (a corrupt size gets 64 bytes, not what it claims)
            b.extend(std::iter::repeat_n(1u8, ((sizes[0] + sizes[1]) as usize).min(64)));
        }
        b.extend(0u32.to_le_bytes());
        b.extend((layers.len() as u32).to_le_bytes());
        b.extend([0xEEu8; 4]);
        b.extend(u32::MAX.to_le_bytes());
        b.extend(u32::MAX.to_le_bytes());
        b
    }

    fn layout(chunk_size: Option<u32>) -> LazLayout {
        LazLayout { compressor: 3, chunk_size, raw_point: 4, layers: 2 }
    }

    #[test]
    fn the_layer_walk_stops_where_laz_stops() {
        let b = chunks(&[(3, [5, 7]), (2, [1, 0])]);
        // variable chunks, by the table's count or by the chunks' own counts
        assert!(check_layers(&b, 0, &layout(None), 5, Some(2)).is_ok());
        assert!(check_layers(&b, 0, &layout(None), 5, None).is_ok());
        // fixed chunks of 3: two chunks for 5 points
        assert!(check_layers(&b, 0, &layout(Some(3)), 5, Some(2)).is_ok());
        // more points than the chunks hold: laz would read the table as a chunk
        let e = check_layers(&b, 0, &layout(Some(3)), 9, Some(2)).unwrap_err();
        assert!(e.contains("LAZ chunk 2 declares"), "{e}");
        let e = check_layers(&b, 0, &layout(None), 5, Some(3)).unwrap_err();
        assert!(e.contains("LAZ chunk 2 declares"), "{e}");
        // a layer longer than the file
        let bad = chunks(&[(3, [5, 7]), (2, [1, 0xFFFF_FF00])]);
        let e = check_layers(&bad, 0, &layout(None), 5, None).unwrap_err();
        assert!(e.contains("LAZ chunk 1 declares 4294967041 bytes"), "{e}");
        // a chunk that claims no points: laz never leaves it, nothing after it is read
        let zero = chunks(&[(0, [5, 7])]);
        assert!(check_layers(&zero, 0, &layout(None), 5, None).is_ok());
    }
}
