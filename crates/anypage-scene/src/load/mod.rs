//! Loaders: bytes in, [`Model`] out. Every loader runs behind
//! [`std::panic::catch_unwind`] so a malformed file can never take the page
//! (or the app) down, checks declared counts against the data before it
//! allocates (a failed allocation aborts the process, which `catch_unwind`
//! cannot catch; see `las::preflight` for the checks made on behalf of the
//! `las` and `laz` crates), and honours cancellation in its long loops.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use crate::model::{Ctl, Model, NormalMode};

mod gltf;
mod las;
mod obj;
mod ply;
mod stl;
mod xyz;

pub use xyz::parse_xyz_points;

/// Formats `scene.load` accepts (aliases resolve to these).
pub const FORMATS: [&str; 8] = ["glb", "gltf", "obj", "stl", "ply", "xyz", "las", "laz"];

/// Largest file any loader accepts.
pub const MAX_FILE_BYTES: usize = 4 << 30;

#[derive(Clone, Debug)]
pub struct LoadOptions {
    /// Normals for meshes without any; `None` = the format's default
    /// (flat for glTF and STL, smooth for OBJ and PLY).
    pub normals: Option<NormalMode>,
    /// Discard source normals and shade flat.
    pub flat: bool,
    /// Shuffle point clouds (default true) so prefixes are uniform samples.
    pub shuffle: bool,
    /// Keep at most this many points: a uniform random sample when
    /// shuffled, otherwise every k-th point in file order.
    pub max_points: Option<usize>,
    /// Material library text for OBJ (`mtllib` files cannot be opened by path).
    pub mtl: Option<Vec<u8>>,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self { normals: None, flat: false, shuffle: true, max_points: None, mtl: None }
    }
}

/// Canonical format name for a user-given name or file extension.
pub fn canonical(format: &str) -> Option<&'static str> {
    let f = format.trim().trim_start_matches('.').to_ascii_lowercase();
    Some(match f.as_str() {
        "glb" => "glb",
        "gltf" => "gltf",
        "obj" => "obj",
        "stl" => "stl",
        "ply" => "ply",
        "xyz" | "txt" | "pts" | "csv" | "asc" => "xyz",
        "las" => "las",
        "laz" => "laz",
        _ => return None,
    })
}

/// Guess the format from the content (and the file name when there is one).
pub fn detect(bytes: &[u8], name: Option<&str>) -> Option<&'static str> {
    let head = &bytes[..bytes.len().min(512)];
    if head.starts_with(b"glTF") {
        return Some("glb");
    }
    if head.starts_with(b"LASF") {
        return Some("las");
    }
    if head.starts_with(b"ply") && head.get(3).is_some_and(|c| *c == b'\n' || *c == b'\r') {
        return Some("ply");
    }
    if bytes.len() >= 84 {
        let n = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
        if n > 0 && 84 + n.saturating_mul(50) == bytes.len() {
            return Some("stl");
        }
    }
    if let Some(ext) = name.and_then(|n| n.rsplit_once('.')).and_then(|(_, e)| canonical(e)) {
        return Some(ext);
    }
    let text = String::from_utf8_lossy(head);
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') && text.contains("\"asset\"") {
        return Some("gltf");
    }
    if trimmed.starts_with("solid") {
        return Some("stl");
    }
    // first meaningful line decides between OBJ and plain coordinates
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with("//") {
            continue;
        }
        let first = l.split(|c: char| c.is_whitespace() || c == ',' || c == ';').next().unwrap_or("");
        if matches!(first, "v" | "vn" | "vt" | "f" | "o" | "g" | "mtllib" | "usemtl" | "s") {
            return Some("obj");
        }
        if first.parse::<f64>().is_ok() {
            return Some("xyz");
        }
        break;
    }
    None
}

/// Shared bytes: the LAS reader needs an owned, `'static` source.
pub type Bytes = Arc<Vec<u8>>;

/// Load `bytes` as `format` (canonical or alias; "auto" or "" to detect).
pub fn load(bytes: &Bytes, format: &str, name: Option<&str>, opts: &LoadOptions, ctl: &Ctl) -> Result<Model, String> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!("file is {} MB; the limit is {} MB", bytes.len() >> 20, MAX_FILE_BYTES >> 20));
    }
    if bytes.is_empty() {
        return Err("the file is empty".into());
    }
    let fmt = if format.is_empty() || format == "auto" {
        detect(bytes, name).ok_or_else(|| format!("cannot tell the format of this file; pass one of {}", FORMATS.join(", ")))?
    } else {
        canonical(format).ok_or_else(|| format!("unknown format \"{format}\"; supported: {}", FORMATS.join(", ")))?
    };
    (ctl.progress)(0.0);
    let r = catch_unwind(AssertUnwindSafe(|| match fmt {
        "glb" | "gltf" => gltf::load(bytes, fmt, opts, ctl),
        "obj" => obj::load(bytes, opts, ctl),
        "stl" => stl::load(bytes, opts, ctl),
        "ply" => ply::load(bytes, opts, ctl),
        "xyz" => xyz::load(bytes, opts, ctl),
        "las" | "laz" => las::load(bytes, opts, ctl),
        _ => Err(format!("unknown format \"{fmt}\"")),
    }));
    let model = match r {
        Ok(r) => r.map_err(|e| if e == "cancelled" { e } else { format!("{fmt}: {e}") })?,
        Err(p) => {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            return Err(format!("{fmt}: the decoder failed on malformed data ({msg})"));
        }
    };
    (ctl.progress)(1.0);
    Ok(model)
}

/// Line iterator over bytes that tolerates `\r\n` and invalid UTF-8.
pub(crate) fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l))
}

/// Whitespace/comma/semicolon separated tokens of one line.
pub(crate) fn tokens(line: &[u8]) -> impl Iterator<Item = &[u8]> {
    line.split(|&b| b == b' ' || b == b'\t' || b == b',' || b == b';').filter(|t| !t.is_empty())
}

pub(crate) fn parse_f64(t: &[u8]) -> Option<f64> {
    std::str::from_utf8(t).ok()?.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection() {
        assert_eq!(detect(b"glTF\x02\0\0\0", None), Some("glb"));
        assert_eq!(detect(b"ply\nformat ascii 1.0\n", None), Some("ply"));
        assert_eq!(detect(b"ply\r\nformat ascii 1.0\r\n", None), Some("ply"));
        assert_eq!(detect(b"LASF....", None), Some("las"));
        assert_eq!(detect(b"solid x\nfacet normal 0 0 1\n", None), Some("stl"));
        assert_eq!(detect(b"# comment\nv 0 0 0\nv 1 0 0\n", None), Some("obj"));
        assert_eq!(detect(b"1.0 2.0 3.0\n4 5 6\n", None), Some("xyz"));
        assert_eq!(detect(b"1,2,3\n", None), Some("xyz"));
        assert_eq!(detect(b"{ \"asset\": { \"version\": \"2.0\" } }", None), Some("gltf"));
        assert_eq!(detect(b"???", Some("scan.LAZ")), Some("laz"));
        assert_eq!(detect(b"???", None), None);
        let mut stl = vec![0u8; 84 + 50];
        stl[80] = 1;
        assert_eq!(detect(&stl, None), Some("stl"));
    }

    #[test]
    fn errors_are_clear() {
        let b: Bytes = Arc::new(b"nothing useful".to_vec());
        let e = load(&b, "auto", None, &LoadOptions::default(), &Ctl::none()).unwrap_err();
        assert!(e.contains("cannot tell the format"), "{e}");
        let e = load(&b, "fbx", None, &LoadOptions::default(), &Ctl::none()).unwrap_err();
        assert!(e.contains("unknown format \"fbx\""), "{e}");
        let e = load(&Arc::new(Vec::new()), "obj", None, &LoadOptions::default(), &Ctl::none()).unwrap_err();
        assert!(e.contains("empty"), "{e}");
    }
}
