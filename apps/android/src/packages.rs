//! Which page the app shows: the demo package compiled into the app, a page
//! package shipped in the APK under `assets/pages/<name>/` (chosen with the
//! intent extra `page`, or opened by a page with `page.navigate(name)`), or,
//! when the chosen page cannot load, a small page that says why.
//!
//! The APK carries every package of the repository's `pages/` folder (the
//! Gradle task `syncPages` copies it into the build, nothing is duplicated
//! in git). Kotlin reads a package's files (`PagePackages.kt`) and hands
//! them over as (name, bytes): the package's own files by name, its binary
//! samples (`assets/<file>`, read by pages with `page.asset(file)`) as
//! `"assets/<file>"`.

use std::sync::Arc;

use anypage_demo::{script, DemoPage};
use anypage_engine::{Page, TextRaster, Wake};
use anypage_runtime::{LoadOptions, Natives, Package, ScriptPage};

/// Where page packages live inside the APK's assets.
pub const ASSET_DIR: &str = "pages";
/// The page the app opens when the intent names none (when the APK has it).
pub const START_PAGE: &str = "home";
/// Binary files of a package: `<package>/assets/<file>`.
pub const ASSETS_PREFIX: &str = "assets/";
/// A file Kotlin adds when it could not read the package; its bytes are the reason.
pub const READ_ERROR: &str = "!error";
/// File kinds a package may contain (as the runtime's package loader).
const TEXT_EXTENSIONS: [&str; 5] = ["luau", "wgsl", "json", "txt", "md"];
const MAX_NAME: usize = 64;

/// The page to load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageSource {
    /// The demo package compiled into the app (pages/demo).
    Builtin,
    /// `assets/pages/<name>/`: the files Kotlin read from there, as (name, bytes).
    Assets { name: String, files: Vec<(String, Vec<u8>)> },
}

impl PageSource {
    pub fn describe(&self) -> String {
        match self {
            PageSource::Builtin => "built-in demo package".into(),
            PageSource::Assets { name, files } => {
                let assets = files.iter().filter(|(f, _)| f.starts_with(ASSETS_PREFIX)).count();
                let bytes: usize = files.iter().map(|(_, b)| b.len()).sum();
                format!("assets/{ASSET_DIR}/{name} ({} files, {assets} binary assets, {} KB)", files.len(), bytes.div_ceil(1024))
            }
        }
    }

    /// The package folder name (None for the built-in demo).
    pub fn name(&self) -> Option<&str> {
        match self {
            PageSource::Builtin => None,
            PageSource::Assets { name, .. } => Some(name),
        }
    }
}

/// What the host tells Kotlin about the page it shows (for the back stack
/// and the secrets screen).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageInfo {
    pub id: String,
    pub title: String,
    /// Secret names the page declares (`capabilities.secrets`).
    pub secrets: Vec<String>,
}

impl PageInfo {
    pub fn of(p: &ScriptPage) -> Self {
        PageInfo { id: p.manifest.id.clone(), title: p.manifest.name.clone(), secrets: p.manifest.caps.secrets.clone() }
    }
}

/// A package folder name: letters, digits, '.', '-', '_', not starting with a dot.
pub fn valid_page_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

fn is_package_file(name: &str) -> bool {
    !name.starts_with('.') && name.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && TEXT_EXTENSIONS.contains(&ext))
}

/// A package from files read out of `assets/pages/<name>/`. Text files a
/// package cannot contain (images next to page.json, folders' placeholders)
/// are skipped and reported; everything else must be UTF-8 text, and the
/// runtime enforces its own limits (2 MB a file, 16 MB in all, no
/// bytecode). Binary samples (`assets/<file>`) go through the runtime's
/// asset rules (allowed extensions, 64 MB each, 256 MB in all), as on the
/// desktop, where a package with a bad asset does not load either.
pub fn assets_package(name: &str, files: Vec<(String, Vec<u8>)>) -> Result<(Package, Vec<String>), String> {
    if !valid_page_name(name) {
        return Err(format!("\"{name}\" is not a page package name (letters, digits, '.', '-', '_')"));
    }
    if let Some((_, why)) = files.iter().find(|(f, _)| f == READ_ERROR) {
        return Err(format!("{name}: {}", String::from_utf8_lossy(why)));
    }
    if files.is_empty() {
        return Err(format!("這個 App 沒有這個頁面（assets/{ASSET_DIR}/{name}/ is missing or empty）"));
    }
    let mut text = Vec::with_capacity(files.len());
    let mut assets = Vec::new();
    let mut skipped = Vec::new();
    for (file, bytes) in files {
        if let Some(asset) = file.strip_prefix(ASSETS_PREFIX) {
            assets.push((asset.to_string(), bytes));
            continue;
        }
        if !is_package_file(&file) {
            skipped.push(file);
            continue;
        }
        let s = String::from_utf8(bytes).map_err(|_| format!("{name}/{file}: not UTF-8 text"))?;
        text.push((file, s));
    }
    let mut pkg = Package::from_files(&format!("assets:{ASSET_DIR}/{name}"), text).map_err(|e| format!("{name}: {e}"))?;
    for (asset, bytes) in assets {
        pkg = pkg.with_asset(&asset, bytes).map_err(|e| format!("{name}: assets/{e}"))?;
    }
    Ok((pkg, skipped))
}

/// `s` as a Luau string literal (any content, including quotes, newlines,
/// "]]" and control characters).
pub fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\{:03}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A page that shows why the chosen page could not be shown. The message is
/// also its input cell `message`, so agents and tests can read it.
pub fn error_package(title: &str, message: &str) -> Result<Package, String> {
    let main = format!(
        r#"-- Shown by the Android host when the chosen page cannot load.
local TITLE = {title}
local msg = page.input("message", {message})
page.panel({{ rect = {{ 0, 0, 900, 600 }}, name = "error", inputs = {{ msg }}, draw = function(d, m)
	d:rrect(0, 0, d.w, d.h, 18, color.rgba(52, 22, 28))
	d:stroke(0.5, 0.5, d.w - 1, d.h - 1, 18, 2, color.rgba(255, 120, 120, 120))
	d:text(36, 30, 36, color.rgba(255, 200, 200), TITLE)
	d:textbox(36, 100, d.w - 72, d.h - 136, 26, color.rgba(240, 232, 232), m)
end }})
"#,
        title = lua_string(title),
        message = lua_string(message),
    );
    let manifest = r#"{ "format": 1, "id": "com.anypage.host.error", "name": "Page error", "bounds": [0, 0, 900, 600] }"#;
    Package::from_files("builtin:error", [("page.json", manifest.to_string()), ("main.luau", main)])
}

/// Native operators pages on this app can name: the demo's and every
/// capability module (anypage-std).
pub fn natives() -> Natives {
    anypage_std::natives()
}

fn requests_short(name: &str) -> String {
    crate::requests::short_message(name, 40)
}

/// What [`load`] produced, with a line for the log.
pub struct Loaded {
    pub page: Box<dyn Page>,
    pub summary: String,
    /// The page that was asked for failed; `page` explains why.
    pub error: Option<String>,
    /// The page that loaded (None for the error page and the Rust fallback).
    pub info: Option<PageInfo>,
}

/// Load `source` exactly: the page, or why it cannot be shown. What
/// `page.navigate` uses (a page that fails to open leaves the current one).
pub fn load_page(source: &PageSource, natives: &Natives, text: Arc<TextRaster>, wake: Wake, opts: &LoadOptions) -> Result<ScriptPage, String> {
    match source {
        PageSource::Builtin => ScriptPage::load_with(&script::package(), natives, text, wake, opts.clone()),
        PageSource::Assets { name, files } => {
            let (pkg, skipped) = assets_package(name, files.clone())?;
            if !skipped.is_empty() {
                log::warn!("{name}: skipped files a page package cannot contain: {}", skipped.join(", "));
            }
            ScriptPage::load_with(&pkg, natives, text, wake, opts.clone())
        }
    }
}

/// Load `source`; if it fails, a page that shows the error; if even that
/// fails, the demo page built in Rust.
pub fn load(source: &PageSource, text: Arc<TextRaster>, wake: Wake, opts: &LoadOptions) -> Loaded {
    let natives = natives();
    let err = match load_page(source, &natives, text.clone(), wake.clone(), opts) {
        Ok(p) => return Loaded { summary: p.summary(), info: Some(PageInfo::of(&p)), page: Box::new(p), error: None },
        Err(e) => e,
    };
    // short: the error page draws it on one line
    let title = match source {
        PageSource::Builtin => "無法載入內建示範 Could not load the demo".to_string(),
        PageSource::Assets { name, .. } => format!("無法開啟頁面 {}", requests_short(name)),
    };
    match error_package(&title, &err).and_then(|pkg| ScriptPage::load(&pkg, &natives, text.clone(), wake.clone())) {
        Ok(p) => Loaded { summary: p.summary(), page: Box::new(p), error: Some(err), info: None },
        Err(e2) => Loaded {
            summary: format!("Rust demo page (the error page also failed: {e2})"),
            page: Box::new(DemoPage::new(text, wake)),
            error: Some(err),
            info: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text() -> Arc<TextRaster> {
        Arc::new(TextRaster::load().expect("a system font"))
    }

    fn wake() -> Wake {
        Arc::new(|| {})
    }

    #[test]
    fn page_names() {
        for ok in ["text-input-test", "demo", "a.b_c-1"] {
            assert!(valid_page_name(ok), "{ok}");
        }
        for bad in ["", ".hidden", "../etc", "a/b", "a\\b", "名前", &"x".repeat(65)] {
            assert!(!valid_page_name(bad), "{bad}");
        }
    }

    #[test]
    fn luau_string_literals_round_trip() {
        let realm = anypage_runtime::Realm::new(&Default::default()).unwrap();
        for s in ["plain", "quote \" back \\ slash", "lines\nand\ttabs\r", "]] [[ ]=]", "字 😀 \u{1}\u{7f}", ""] {
            let src = format!("return {}", lua_string(s));
            let out: Result<String, String> = realm.call(100.0, |lua| lua.load(src.as_str()).eval::<String>());
            assert_eq!(out.as_deref(), Ok(s), "{src}");
        }
    }

    #[test]
    fn assets_package_filters_and_validates() {
        let manifest = br#"{ "format": 1, "id": "t", "bounds": [0, 0, 100, 100] }"#.to_vec();
        let main = b"page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })".to_vec();
        let files = vec![
            ("page.json".to_string(), manifest.clone()),
            ("main.luau".to_string(), main.clone()),
            ("icon.png".to_string(), vec![0x89, b'P', b'N', b'G']),
        ];
        let (pkg, skipped) = assets_package("t", files).unwrap();
        assert_eq!(pkg.origin, "assets:pages/t");
        assert_eq!(skipped, vec!["icon.png".to_string()]);
        let e = assets_package("t", vec![("page.json".into(), manifest.clone()), ("main.luau".into(), vec![0xff, 0xfe])]).unwrap_err();
        assert!(e.contains("not UTF-8"), "{e}");
        assert!(assets_package("t", vec![]).unwrap_err().contains("missing or empty"));
        assert!(assets_package("../x", vec![("main.luau".into(), main)]).unwrap_err().contains("not a page package name"));
    }

    #[test]
    fn a_failing_page_shows_why() {
        let opts = LoadOptions::default();
        let bad = PageSource::Assets { name: "broken".into(), files: vec![("page.json".into(), b"{ not json".to_vec())] };
        let mut loaded = load(&bad, text(), wake(), &opts);
        let err = loaded.error.clone().expect("the error is reported");
        assert!(err.contains("page.json"), "{err}");
        assert!(loaded.info.is_none());
        let shown = loaded.page.act(r#"{"get": "message"}"#);
        assert!(shown.contains("page.json"), "{shown}");
        // a package that is missing entirely
        let missing = PageSource::Assets { name: "nope".into(), files: vec![] };
        let mut loaded = load(&missing, text(), wake(), &opts);
        assert!(loaded.page.act(r#"{"get": "message"}"#).contains("missing or empty"));
        loaded.page.shutdown();
        // Kotlin could not read it (too large for memory, an I/O error)
        let unread = PageSource::Assets { name: "big".into(), files: vec![(READ_ERROR.into(), b"assets/scan.las: larger than 64 MB".to_vec())] };
        let e = load_page(&unread, &Natives::new(), text(), wake(), &opts).err().unwrap();
        assert_eq!(e, "big: assets/scan.las: larger than 64 MB");
    }

    #[test]
    fn the_builtin_demo_loads() {
        let mut loaded = load(&PageSource::Builtin, text(), wake(), &LoadOptions::default());
        assert!(loaded.error.is_none(), "{:?}", loaded.error);
        assert!(loaded.summary.contains("com.anypage.demo"), "{}", loaded.summary);
        assert_eq!(loaded.info.as_ref().map(|i| i.id.as_str()), Some("com.anypage.demo"));
        assert!(!loaded.page.spec().panels.is_empty());
        loaded.page.shutdown();
    }

    const ASSET_PAGE: &str = r#"
local doc = page.asset("sample.pdf")
local n = page.input("n", #doc)
page.panel({ rect = { 0, 0, 10, 10 }, inputs = { n }, draw = function(d, v) end })
"#;

    /// Binary samples under `assets/` reach `page.asset`, with the
    /// runtime's rules on names and sizes.
    #[test]
    fn binary_assets_reach_the_page() {
        let manifest = br#"{ "format": 1, "id": "t.assets", "bounds": [0, 0, 10, 10] }"#.to_vec();
        let pdf = b"%PDF-1.4 sample".to_vec();
        let files = vec![
            ("page.json".to_string(), manifest.clone()),
            ("main.luau".to_string(), ASSET_PAGE.as_bytes().to_vec()),
            ("assets/sample.pdf".to_string(), pdf.clone()),
        ];
        let (pkg, skipped) = assets_package("t", files.clone()).unwrap();
        assert!(skipped.is_empty());
        assert_eq!(pkg.assets.get("sample.pdf").map(|a| a.len()), Some(pdf.len()));
        let source = PageSource::Assets { name: "t".into(), files: files.clone() };
        assert!(source.describe().contains("1 binary assets"), "{}", source.describe());
        let mut p = load_page(&source, &Natives::new(), text(), wake(), &LoadOptions::default()).unwrap();
        assert_eq!(p.cell_value("n").and_then(|d| d.num()), Some(pdf.len() as f64));
        p.shutdown();
        // a name the runtime does not take fails the package, as on the desktop
        let mut bad = files.clone();
        bad.push(("assets/tool.exe".into(), vec![0; 4]));
        let e = assets_package("t", bad).err().unwrap();
        assert!(e.starts_with("t: assets/tool.exe: not an allowed asset name"), "{e}");
        // a missing asset is a load error with the file's name
        let without: Vec<_> = files.into_iter().filter(|(f, _)| !f.starts_with(ASSETS_PREFIX)).collect();
        let e = load_page(&PageSource::Assets { name: "t".into(), files: without }, &Natives::new(), text(), wake(), &LoadOptions::default()).err().unwrap();
        assert!(e.contains("sample.pdf"), "{e}");
    }

    struct Vault;
    impl anypage_runtime::SecretStore for Vault {
        fn get(&self, page_id: &str, name: &str) -> Option<String> {
            (page_id == "t.secrets" && name == "openai").then(|| "sk-test-1234".to_string())
        }
    }

    /// The host's secret store reaches native modules (never the script).
    #[test]
    fn secrets_reach_native_code() {
        let manifest = br#"{ "format": 1, "id": "t.secrets", "bounds": [0, 0, 10, 10], "capabilities": { "modules": ["t"], "secrets": ["openai", "aws"] } }"#.to_vec();
        let main = br#"
local a = t.secret_len("openai")
local ok, err = pcall(t.secret_len, "aws")
page.input("a", a)
page.input("b", tostring(err))
page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
"#
        .to_vec();
        let mut n = Natives::new();
        n.function("t.secret_len", anypage_runtime::Effect::Pure, |c| {
            let name = c.arg(0).as_str().unwrap_or_default().to_string();
            c.grants.secret(&name).map(|s| anypage_runtime::Data::Num(s.len() as f64))
        });
        let source = PageSource::Assets { name: "s".into(), files: vec![("page.json".into(), manifest), ("main.luau".into(), main)] };
        let opts = LoadOptions { secrets: Some(Arc::new(Vault)) };
        let mut p = load_page(&source, &n, text(), wake(), &opts).unwrap();
        assert_eq!(p.cell_value("a").and_then(|d| d.num()), Some(12.0));
        let b = p.cell_value("b").and_then(|d| d.as_str().map(str::to_string)).unwrap_or_default();
        assert!(b.contains("not set on this device"), "{b}");
        assert_eq!(PageInfo::of(&p).secrets, vec!["openai".to_string(), "aws".to_string()]);
        p.shutdown();
    }

    /// Every package in the repository's pages/ folder loads the way the
    /// APK carries it (Gradle's syncPages copies the folder as it is): text
    /// files by name, `assets/` files as "assets/<file>".
    #[test]
    fn every_shipped_page_loads_on_the_android_path() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pages");
        let mut names: Vec<String> = std::fs::read_dir(&root).unwrap().flatten().filter(|e| e.path().join("page.json").is_file()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert!(names.len() >= 5, "{names:?}");
        let mut failed = Vec::new();
        for name in &names {
            let dir = root.join(name);
            let mut files: Vec<(String, Vec<u8>)> = Vec::new();
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                if e.path().is_file() {
                    files.push((e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap()));
                }
            }
            if let Ok(rd) = std::fs::read_dir(dir.join("assets")) {
                for e in rd.flatten().filter(|e| e.path().is_file()) {
                    files.push((format!("{ASSETS_PREFIX}{}", e.file_name().to_string_lossy()), std::fs::read(e.path()).unwrap()));
                }
            }
            let source = PageSource::Assets { name: name.clone(), files };
            let t0 = std::time::Instant::now();
            let mut loaded = load(&source, text(), wake(), &LoadOptions::default());
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            println!("{name}: {} in {ms:.0} ms", if loaded.error.is_none() { "loaded" } else { "FAILED" });
            if let Some(e) = &loaded.error {
                failed.push(format!("{name}: {e}"));
            } else {
                assert_eq!(loaded.info.as_ref().map(|i| i.id.is_empty()), Some(false));
            }
            loaded.page.shutdown();
        }
        assert!(failed.is_empty(), "{failed:#?}");
    }
}
