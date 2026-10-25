//! The native modules this desktop build offers to pages, and where it keeps
//! their secrets. Each capability crate contributes a `register` function;
//! a page can only use the ones it declares in page.json.

use std::sync::Arc;

use anypage_engine::TextRaster;
use anypage_runtime::{EnvSecrets, Natives, SecretStore};

pub fn natives(_text: &Arc<TextRaster>) -> Natives {
    anypage_std::natives()
}

/// Secrets for pages: `ANYPAGE_SECRET_<NAME>` environment variables, then
/// `secrets.json` in the per-user config folder
/// (`%APPDATA%\AnyPage\secrets.json` on Windows):
/// `{ "<page id>": { "<name>": "<value>" }, "*": { "<name>": "<value>" } }`.
pub struct DesktopSecrets {
    file: Option<serde_json::Value>,
}

impl DesktopSecrets {
    pub fn new() -> Self {
        let file = config_dir()
            .map(|d| d.join("secrets.json"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok());
        Self { file }
    }
}

fn config_dir() -> Option<std::path::PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|a| std::path::PathBuf::from(a).join("AnyPage"))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Library/Application Support/AnyPage"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
            .map(|d| d.join("anypage"))
    }
}

impl SecretStore for DesktopSecrets {
    fn get(&self, page_id: &str, name: &str) -> Option<String> {
        if let Some(v) = EnvSecrets.get(page_id, name) {
            return Some(v);
        }
        let f = self.file.as_ref()?;
        [page_id, "*"].iter().find_map(|scope| f.get(scope)?.get(name)?.as_str().map(str::to_string))
    }
}
