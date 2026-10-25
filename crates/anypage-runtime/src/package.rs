//! Page packages: `page.json` (what the page is and what it may use), Luau
//! source files and WGSL shaders. Text only. Precompiled bytecode is refused,
//! so the app's own compiler is the only one that ever produces bytecode
//! (docs/ARCHITECTURE.md section 13).

use std::collections::BTreeMap;
use std::path::Path;

use anypage_core::Rect;
use serde_json::Value as Json;

pub const FORMAT: u32 = 1;
const MAX_FILE: usize = 2 << 20;
const MAX_TOTAL: usize = 16 << 20;
/// Binary assets (sample documents, models, images) under `assets/`.
const MAX_ASSET: usize = 64 << 20;
const MAX_ASSETS_TOTAL: usize = 256 << 20;
const ASSET_EXTENSIONS: [&str; 26] = [
    "png", "jpg", "jpeg", "webp", "gif", "pdf", "xlsx", "csv", "dxf", "step", "stp", "glb", "gltf", "bin", "obj", "stl", "ply",
    "las", "laz", "xyz", "ttf", "otf", "wav", "mp4", "ts", "json",
];
const TEXT_EXTENSIONS: [&str; 5] = ["luau", "wgsl", "json", "txt", "md"];

/// What the page may use beyond pure computation. Everything not listed is
/// unavailable to its scripts.
#[derive(Clone, Debug, Default)]
pub struct Caps {
    /// Native modules the page needs; loading fails if one is missing.
    pub modules: Vec<String>,
    /// Native modules the page uses when present (USB, Bluetooth).
    pub optional_modules: Vec<String>,
    pub streams: Vec<String>,
    pub jobs: Vec<String>,
    pub tiles: Vec<String>,
    /// Page-supplied WGSL operators.
    pub gpu: bool,
    /// Hosts native modules may contact for this page: "api.openai.com",
    /// "*.amazonaws.com", "localhost:11434".
    pub net: Vec<String>,
    /// Names of host-kept secrets native modules may use for this page.
    pub secrets: Vec<String>,
    /// Secrets that may only go to some hosts: name -> host patterns.
    pub secret_hosts: std::collections::BTreeMap<String, Vec<String>>,
    /// Open and save dialogs, dropped files.
    pub files: bool,
    pub clipboard: bool,
}

impl Caps {
    /// May the page use native `name` ("pdf.open", "demo.ticks")?
    pub fn allows(&self, name: &str) -> bool {
        let module = name.split('.').next().unwrap_or(name);
        self.modules.iter().chain(&self.optional_modules).any(|m| m == module)
            || self.streams.iter().chain(&self.jobs).chain(&self.tiles).any(|n| n == name)
    }
}

/// Per-page quotas, clamped to what the host allows.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub memory_mb: u32,
    /// Budget for one script function call during evaluation.
    pub call_ms: f64,
    /// Budget for one event handler.
    pub handler_ms: f64,
    /// Budget for running the entry script once at load.
    pub load_ms: f64,
    pub job_workers: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { memory_mb: 64, call_ms: 50.0, handler_ms: 50.0, load_ms: 2000.0, job_workers: 4 }
    }
}

#[derive(Clone, Debug)]
pub struct Manifest {
    pub format: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub entry: String,
    pub bounds: Rect,
    pub caps: Caps,
    pub limits: Limits,
}

#[derive(Clone, Debug)]
pub struct Package {
    pub manifest: Manifest,
    pub files: BTreeMap<String, String>,
    /// Binary files from `assets/`, by file name; pages read them with `page.asset(name)`.
    pub assets: BTreeMap<String, std::sync::Arc<Vec<u8>>>,
    /// Where it came from, for messages: a folder path or "builtin:<name>".
    pub origin: String,
}

fn strings(v: &Json) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// `"secrets": ["openai", {"name": "aws", "hosts": ["*.amazonaws.com"]}]`
fn secret_names(v: &Json) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().or_else(|| x.get("name").and_then(Json::as_str)).map(str::to_string)).collect())
        .unwrap_or_default()
}

fn secret_bindings(v: &Json) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut m = std::collections::BTreeMap::new();
    for x in v.as_array().into_iter().flatten() {
        if let (Some(name), Some(hosts)) = (x.get("name").and_then(Json::as_str), x.get("hosts")) {
            m.insert(name.to_string(), strings(hosts));
        }
    }
    m
}

fn num(v: &Json, key: &str, default: f64, lo: f64, hi: f64) -> f64 {
    v.get(key).and_then(Json::as_f64).unwrap_or(default).clamp(lo, hi)
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, String> {
        let j: Json = serde_json::from_str(text).map_err(|e| format!("page.json: {e}"))?;
        let format = j.get("format").and_then(Json::as_u64).unwrap_or(0) as u32;
        if format != FORMAT {
            return Err(format!("page.json: format {format} is not supported (expected {FORMAT})"));
        }
        let s = |k: &str| j.get(k).and_then(Json::as_str).map(str::to_string);
        let id = s("id").ok_or("page.json: missing \"id\"")?;
        let entry = s("entry").unwrap_or_else(|| "main.luau".into());
        let b: Vec<f64> = j.get("bounds").and_then(Json::as_array).map(|a| a.iter().filter_map(Json::as_f64).collect()).unwrap_or_default();
        if b.len() != 4 || b[2] <= 0.0 || b[3] <= 0.0 || b.iter().any(|x| !x.is_finite()) {
            return Err("page.json: \"bounds\" must be [x, y, w, h] with w, h > 0".into());
        }
        let c = j.get("capabilities").cloned().unwrap_or(Json::Null);
        let flag = |k: &str| c.get(k).and_then(Json::as_bool).unwrap_or(false);
        let caps = Caps {
            modules: strings(&c["modules"]),
            optional_modules: strings(&c["optional_modules"]),
            streams: strings(&c["streams"]),
            jobs: strings(&c["jobs"]),
            tiles: strings(&c["tiles"]),
            gpu: flag("gpu"),
            net: strings(&c["net"]),
            secrets: secret_names(&c["secrets"]),
            secret_hosts: secret_bindings(&c["secrets"]),
            files: flag("files"),
            clipboard: flag("clipboard"),
        };
        let l = j.get("limits").cloned().unwrap_or(Json::Null);
        let d = Limits::default();
        let limits = Limits {
            memory_mb: num(&l, "memory_mb", d.memory_mb as f64, 8.0, 256.0) as u32,
            call_ms: num(&l, "call_ms", d.call_ms, 1.0, 1000.0),
            handler_ms: num(&l, "handler_ms", d.handler_ms, 1.0, 1000.0),
            load_ms: num(&l, "load_ms", d.load_ms, 10.0, 10_000.0),
            job_workers: num(&l, "job_workers", d.job_workers as f64, 1.0, 16.0) as usize,
        };
        Ok(Self {
            format,
            name: s("name").unwrap_or_else(|| id.clone()),
            id,
            version: s("version").unwrap_or_else(|| "0".into()),
            entry,
            bounds: Rect::new(b[0] as f32, b[1] as f32, b[2] as f32, b[3] as f32),
            caps,
            limits,
        })
    }
}

fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && !name.contains(['/', '\\', ':'])
        && name.rsplit_once('.').is_some_and(|(_, ext)| TEXT_EXTENSIONS.contains(&ext));
    if ok {
        Ok(())
    } else {
        Err(format!("{name}: not an allowed file name (plain name with extension {})", TEXT_EXTENSIONS.join("/")))
    }
}

impl Package {
    /// From in-memory files, e.g. a page bundled into the app with `include_str!`.
    pub fn from_files<I, K, V>(origin: &str, files: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut map = BTreeMap::new();
        let mut total = 0usize;
        for (k, v) in files {
            let (k, v): (String, String) = (k.into(), v.into());
            check_name(&k)?;
            if v.len() > MAX_FILE {
                return Err(format!("{k}: larger than {} MB", MAX_FILE >> 20));
            }
            if k.ends_with(".luau") && v.as_bytes().first() == Some(&0x1b) {
                return Err(format!("{k}: precompiled bytecode is not accepted, ship source"));
            }
            total += v.len();
            map.insert(k, v);
        }
        if total > MAX_TOTAL {
            return Err(format!("package larger than {} MB", MAX_TOTAL >> 20));
        }
        let manifest = Manifest::parse(map.get("page.json").ok_or("missing page.json")?)?;
        if !map.contains_key(&manifest.entry) {
            return Err(format!("entry script {} is not in the package", manifest.entry));
        }
        Ok(Self { manifest, files: map, assets: BTreeMap::new(), origin: origin.to_string() })
    }

    /// Add a binary asset (a sample PDF, a model) the page reads with `page.asset(name)`.
    pub fn with_asset(mut self, name: &str, bytes: Vec<u8>) -> Result<Self, String> {
        let ok = !name.is_empty()
            && !name.starts_with('.')
            && !name.contains(['/', '\\', ':'])
            && name.rsplit_once('.').is_some_and(|(_, ext)| ASSET_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()));
        if !ok {
            return Err(format!("{name}: not an allowed asset name (plain name with extension {})", ASSET_EXTENSIONS.join("/")));
        }
        if bytes.len() > MAX_ASSET {
            return Err(format!("{name}: asset larger than {} MB", MAX_ASSET >> 20));
        }
        let total: usize = self.assets.values().map(|a| a.len()).sum::<usize>() + bytes.len();
        if total > MAX_ASSETS_TOTAL {
            return Err(format!("assets larger than {} MB in total", MAX_ASSETS_TOTAL >> 20));
        }
        self.assets.insert(name.to_string(), std::sync::Arc::new(bytes));
        Ok(self)
    }

    /// From a folder: every top-level text file with an allowed extension.
    pub fn from_dir(dir: &Path) -> Result<Self, String> {
        let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut files = Vec::new();
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
            if check_name(&name).is_err() {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|e| format!("{name}: {e}"))?;
            let text = String::from_utf8(bytes).map_err(|_| format!("{name}: not UTF-8 text"))?;
            files.push((name, text));
        }
        let mut pkg = Self::from_files(&dir.display().to_string(), files)?;
        let assets = dir.join("assets");
        if let Ok(rd) = std::fs::read_dir(&assets) {
            for entry in rd.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
                if std::fs::metadata(&path).map(|m| m.len() as usize > MAX_ASSET).unwrap_or(true) {
                    return Err(format!("assets/{name}: larger than {} MB", MAX_ASSET >> 20));
                }
                let bytes = std::fs::read(&path).map_err(|e| format!("assets/{name}: {e}"))?;
                pkg = pkg.with_asset(&name, bytes)?;
            }
        }
        Ok(pkg)
    }

    pub fn file(&self, name: &str) -> Option<&str> {
        self.files.get(name).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{ "format": 1, "id": "t", "bounds": [0, 0, 100, 50],
        "capabilities": { "streams": ["a"], "gpu": true }, "limits": { "memory_mb": 9999 } }"#;

    #[test]
    fn parses_and_clamps() {
        let p = Package::from_files("test", [("page.json", MANIFEST), ("main.luau", "-- empty")]).unwrap();
        assert_eq!(p.manifest.caps.streams, vec!["a".to_string()]);
        assert!(p.manifest.caps.gpu);
        assert_eq!(p.manifest.limits.memory_mb, 256);
        assert_eq!(p.manifest.bounds.w, 100.0);
    }

    #[test]
    fn refuses_bytecode_and_paths() {
        assert!(Package::from_files("t", [("page.json", MANIFEST), ("main.luau", "\x1bLua")]).is_err());
        assert!(Package::from_files("t", [("page.json", MANIFEST), ("main.luau", ""), ("../x.luau", "")]).is_err());
        assert!(Package::from_files("t", [("page.json", MANIFEST)]).is_err());
    }
}
