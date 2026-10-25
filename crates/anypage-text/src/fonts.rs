//! Font discovery and per-platform fallback lists.
//!
//! `cosmic-text` ships fallback lists for Windows, macOS and desktop Linux
//! but none for Android (fontdb does not even scan `/system/fonts` there),
//! and its Windows list has no Arabic or Hebrew entry and no CJK entry for
//! CJK punctuation (Unicode script "Common"). The lists below cover the
//! scripts AnyPage promises on every platform it targets; family names that
//! are not installed are skipped.

use cosmic_text::fontdb::{self, Database, ID};
use cosmic_text::Fallback;
use unicode_script::Script;

/// Which Han glyph style comes first (Han unification).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cjk {
    Tc,
    Sc,
    Ja,
    Ko,
}

/// Locale to Han preference. Unknown locales get Traditional Chinese first,
/// which is what the engine's single-font text raster used (Microsoft
/// JhengHei) and the language AnyPage's own UI is written in.
pub(crate) fn cjk_for_locale(locale: &str) -> Cjk {
    let l = locale.to_ascii_lowercase().replace('_', "-");
    if l.starts_with("ja") {
        Cjk::Ja
    } else if l.starts_with("ko") {
        Cjk::Ko
    } else if l.starts_with("zh") {
        let traditional = ["tw", "hk", "mo", "hant"].iter().any(|t| l.split('-').skip(1).any(|p| p == *t));
        if traditional {
            Cjk::Tc
        } else {
            Cjk::Sc
        }
    } else {
        Cjk::Tc
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;

    pub const PRIMARY: &[&str] = &["Segoe UI", "Microsoft JhengHei UI", "Arial", "Tahoma"];

    pub fn han(c: Cjk) -> &'static [&'static str] {
        match c {
            Cjk::Tc => &["Microsoft JhengHei UI", "Microsoft JhengHei", "Microsoft YaHei UI", "Microsoft YaHei", "MingLiU", "Yu Gothic UI", "Malgun Gothic", "SimSun"],
            Cjk::Sc => &["Microsoft YaHei UI", "Microsoft YaHei", "Microsoft JhengHei UI", "SimSun", "Yu Gothic UI", "Malgun Gothic"],
            Cjk::Ja => &["Yu Gothic UI", "Yu Gothic", "Meiryo UI", "Meiryo", "MS Gothic", "Microsoft JhengHei UI", "Microsoft YaHei UI"],
            Cjk::Ko => &["Malgun Gothic", "Gulim", "Microsoft JhengHei UI", "Microsoft YaHei UI"],
        }
    }

    pub fn common(c: Cjk) -> &'static [&'static str] {
        match c {
            Cjk::Tc => &["Segoe UI", "Segoe UI Emoji", "Segoe UI Symbol", "Microsoft JhengHei UI", "Segoe UI Historic", "Cambria Math", "Microsoft YaHei UI"],
            Cjk::Sc => &["Segoe UI", "Segoe UI Emoji", "Segoe UI Symbol", "Microsoft YaHei UI", "Segoe UI Historic", "Cambria Math", "Microsoft JhengHei UI"],
            Cjk::Ja => &["Segoe UI", "Segoe UI Emoji", "Segoe UI Symbol", "Yu Gothic UI", "Segoe UI Historic", "Cambria Math", "Microsoft JhengHei UI"],
            Cjk::Ko => &["Segoe UI", "Segoe UI Emoji", "Segoe UI Symbol", "Malgun Gothic", "Segoe UI Historic", "Cambria Math", "Microsoft JhengHei UI"],
        }
    }

    pub fn script(s: Script, c: Cjk) -> &'static [&'static str] {
        match s {
            Script::Han => han(c),
            Script::Bopomofo => han(Cjk::Tc),
            Script::Hiragana | Script::Katakana => han(Cjk::Ja),
            Script::Hangul => han(Cjk::Ko),
            Script::Arabic => &["Segoe UI", "Tahoma", "Arial", "Times New Roman"],
            Script::Hebrew => &["Segoe UI", "Arial", "Tahoma", "David"],
            Script::Greek | Script::Cyrillic | Script::Armenian | Script::Georgian => &["Segoe UI", "Arial", "Sylfaen"],
            Script::Syriac => &["Segoe UI Historic", "Estrangelo Edessa"],
            Script::Thaana => &["MV Boli"],
            Script::Devanagari => &["Nirmala UI", "Nirmala Text", "Mangal"],
            Script::Bengali => &["Nirmala UI", "Vrinda"],
            Script::Gujarati => &["Nirmala UI", "Shruti"],
            Script::Gurmukhi => &["Nirmala UI", "Raavi"],
            Script::Kannada => &["Nirmala UI", "Tunga"],
            Script::Malayalam => &["Nirmala UI", "Kartika"],
            Script::Oriya => &["Nirmala UI", "Kalinga"],
            Script::Tamil => &["Nirmala UI", "Latha"],
            Script::Telugu => &["Nirmala UI", "Gautami"],
            Script::Sinhala => &["Nirmala UI", "Iskoola Pota"],
            Script::Chakma | Script::Ol_Chiki => &["Nirmala UI"],
            Script::Thai => &["Leelawadee UI", "Leelawadee", "Tahoma"],
            Script::Lao => &["Leelawadee UI", "Lao UI"],
            Script::Khmer => &["Leelawadee UI", "Khmer UI"],
            Script::Buginese => &["Leelawadee UI"],
            Script::Myanmar => &["Myanmar Text"],
            Script::Tibetan => &["Microsoft Himalaya"],
            Script::Mongolian => &["Mongolian Baiti"],
            Script::Yi => &["Microsoft Yi Baiti"],
            Script::Tai_Le => &["Microsoft Tai Le"],
            Script::New_Tai_Lue => &["Microsoft New Tai Lue"],
            Script::Ethiopic | Script::Adlam | Script::Vai | Script::Tifinagh | Script::Nko | Script::Osmanya => &["Ebrima"],
            Script::Cherokee | Script::Canadian_Aboriginal => &["Gadugi"],
            Script::Javanese => &["Javanese Text"],
            Script::Braille => &["Segoe UI Symbol"],
            _ => &["Segoe UI Historic"],
        }
    }
}

#[cfg(target_os = "android")]
mod platform {
    use super::*;

    pub const PRIMARY: &[&str] = &["Roboto", "Noto Sans", "Droid Sans"];

    pub fn han(c: Cjk) -> &'static [&'static str] {
        match c {
            Cjk::Tc => &["Noto Sans CJK TC", "Noto Sans CJK HK", "Noto Sans CJK SC", "Noto Sans CJK JP", "Noto Sans CJK KR", "Droid Sans Fallback"],
            Cjk::Sc => &["Noto Sans CJK SC", "Noto Sans CJK TC", "Noto Sans CJK JP", "Noto Sans CJK KR", "Droid Sans Fallback"],
            Cjk::Ja => &["Noto Sans CJK JP", "Noto Sans CJK TC", "Noto Sans CJK SC", "Droid Sans Fallback"],
            Cjk::Ko => &["Noto Sans CJK KR", "Noto Sans CJK TC", "Noto Sans CJK SC", "Droid Sans Fallback"],
        }
    }

    pub fn common(c: Cjk) -> &'static [&'static str] {
        match c {
            Cjk::Tc => &["Roboto", "Noto Color Emoji", "Noto Color Emoji Flags", "Noto Sans Symbols", "Noto Sans Symbols 2", "Noto Sans CJK TC", "Noto Sans Math", "Noto Sans"],
            Cjk::Sc => &["Roboto", "Noto Color Emoji", "Noto Color Emoji Flags", "Noto Sans Symbols", "Noto Sans Symbols 2", "Noto Sans CJK SC", "Noto Sans Math", "Noto Sans"],
            Cjk::Ja => &["Roboto", "Noto Color Emoji", "Noto Color Emoji Flags", "Noto Sans Symbols", "Noto Sans Symbols 2", "Noto Sans CJK JP", "Noto Sans Math", "Noto Sans"],
            Cjk::Ko => &["Roboto", "Noto Color Emoji", "Noto Color Emoji Flags", "Noto Sans Symbols", "Noto Sans Symbols 2", "Noto Sans CJK KR", "Noto Sans Math", "Noto Sans"],
        }
    }

    pub fn script(s: Script, c: Cjk) -> &'static [&'static str] {
        match s {
            Script::Han => han(c),
            Script::Bopomofo => han(Cjk::Tc),
            Script::Hiragana | Script::Katakana => han(Cjk::Ja),
            Script::Hangul => han(Cjk::Ko),
            Script::Arabic => &["Noto Naskh Arabic UI", "Noto Naskh Arabic", "Noto Sans Arabic UI", "Noto Sans Arabic"],
            Script::Hebrew => &["Noto Sans Hebrew"],
            Script::Greek | Script::Cyrillic => &["Roboto", "Noto Sans"],
            Script::Armenian => &["Noto Sans Armenian"],
            Script::Georgian => &["Noto Sans Georgian"],
            Script::Syriac => &["Noto Sans Syriac"],
            Script::Thaana => &["Noto Sans Thaana"],
            Script::Devanagari => &["Noto Sans Devanagari UI", "Noto Sans Devanagari"],
            Script::Bengali => &["Noto Sans Bengali UI", "Noto Sans Bengali"],
            Script::Gujarati => &["Noto Sans Gujarati UI", "Noto Sans Gujarati"],
            Script::Gurmukhi => &["Noto Sans Gurmukhi UI", "Noto Sans Gurmukhi"],
            Script::Kannada => &["Noto Sans Kannada UI", "Noto Sans Kannada"],
            Script::Malayalam => &["Noto Sans Malayalam UI", "Noto Sans Malayalam"],
            Script::Oriya => &["Noto Sans Oriya UI", "Noto Sans Oriya"],
            Script::Tamil => &["Noto Sans Tamil UI", "Noto Sans Tamil"],
            Script::Telugu => &["Noto Sans Telugu UI", "Noto Sans Telugu"],
            Script::Sinhala => &["Noto Sans Sinhala UI", "Noto Sans Sinhala"],
            Script::Thai => &["Noto Sans Thai UI", "Noto Sans Thai"],
            Script::Lao => &["Noto Sans Lao UI", "Noto Sans Lao"],
            Script::Khmer => &["Noto Sans Khmer UI", "Noto Sans Khmer"],
            Script::Myanmar => &["Noto Sans Myanmar UI", "Noto Sans Myanmar"],
            Script::Tibetan => &["Noto Serif Tibetan", "Noto Sans Tibetan"],
            Script::Ethiopic => &["Noto Sans Ethiopic"],
            Script::Cherokee => &["Noto Sans Cherokee"],
            Script::Canadian_Aboriginal => &["Noto Sans Canadian Aboriginal"],
            Script::Mongolian => &["Noto Sans Mongolian"],
            Script::Yi => &["Noto Sans Yi"],
            Script::Javanese => &["Noto Sans Javanese"],
            Script::Adlam => &["Noto Sans Adlam UI", "Noto Sans Adlam"],
            Script::Nko => &["Noto Sans NKo"],
            Script::Tifinagh => &["Noto Sans Tifinagh"],
            Script::Vai => &["Noto Sans Vai"],
            Script::Braille => &["Noto Sans Symbols 2"],
            _ => &[],
        }
    }
}

#[cfg(target_vendor = "apple")]
mod platform {
    use super::*;

    pub const PRIMARY: &[&str] = &["Helvetica Neue", "Helvetica", "Arial", "Lucida Grande"];

    pub fn han(c: Cjk) -> &'static [&'static str] {
        match c {
            Cjk::Tc => &["PingFang TC", "PingFang HK", "PingFang SC", "Heiti TC", "Hiragino Sans", "Apple SD Gothic Neo"],
            Cjk::Sc => &["PingFang SC", "PingFang TC", "Heiti SC", "Hiragino Sans", "Apple SD Gothic Neo"],
            Cjk::Ja => &["Hiragino Sans", "Hiragino Kaku Gothic ProN", "PingFang TC", "PingFang SC"],
            Cjk::Ko => &["Apple SD Gothic Neo", "PingFang TC", "PingFang SC"],
        }
    }

    pub fn common(c: Cjk) -> &'static [&'static str] {
        match c {
            Cjk::Tc => &["Helvetica Neue", "Apple Color Emoji", "Apple Symbols", "PingFang TC", "STIX Two Math"],
            Cjk::Sc => &["Helvetica Neue", "Apple Color Emoji", "Apple Symbols", "PingFang SC", "STIX Two Math"],
            Cjk::Ja => &["Helvetica Neue", "Apple Color Emoji", "Apple Symbols", "Hiragino Sans", "STIX Two Math"],
            Cjk::Ko => &["Helvetica Neue", "Apple Color Emoji", "Apple Symbols", "Apple SD Gothic Neo", "STIX Two Math"],
        }
    }

    pub fn script(s: Script, c: Cjk) -> &'static [&'static str] {
        match s {
            Script::Han => han(c),
            Script::Bopomofo => han(Cjk::Tc),
            Script::Hiragana | Script::Katakana => han(Cjk::Ja),
            Script::Hangul => han(Cjk::Ko),
            Script::Arabic => &["Geeza Pro", "SF Arabic", "Arial"],
            Script::Hebrew => &["Arial Hebrew", "SF Hebrew", "Arial"],
            Script::Devanagari => &["Kohinoor Devanagari", "Devanagari Sangam MN", "ITF Devanagari"],
            Script::Thai => &["Thonburi", "Sukhumvit Set", "Ayuthaya"],
            _ => &[],
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "android", target_vendor = "apple")))]
mod platform {
    use super::*;

    pub const PRIMARY: &[&str] = &["Noto Sans", "DejaVu Sans", "Liberation Sans", "Cantarell", "Ubuntu", "FreeSans"];

    pub fn han(c: Cjk) -> &'static [&'static str] {
        match c {
            Cjk::Tc => &["Noto Sans CJK TC", "Noto Sans CJK HK", "Noto Sans CJK SC", "Source Han Sans TC", "WenQuanYi Micro Hei", "WenQuanYi Zen Hei", "AR PL UMing TW", "Droid Sans Fallback"],
            Cjk::Sc => &["Noto Sans CJK SC", "Noto Sans CJK TC", "Source Han Sans SC", "WenQuanYi Micro Hei", "WenQuanYi Zen Hei", "Droid Sans Fallback"],
            Cjk::Ja => &["Noto Sans CJK JP", "Source Han Sans JP", "IPAGothic", "TakaoPGothic", "Noto Sans CJK TC", "Droid Sans Fallback"],
            Cjk::Ko => &["Noto Sans CJK KR", "Source Han Sans KR", "NanumGothic", "UnDotum", "Noto Sans CJK TC", "Droid Sans Fallback"],
        }
    }

    pub fn common(c: Cjk) -> &'static [&'static str] {
        match c {
            Cjk::Tc => &["Noto Sans", "DejaVu Sans", "Noto Color Emoji", "Noto Sans Symbols", "Noto Sans Symbols 2", "Noto Sans CJK TC", "Noto Sans Math", "FreeSans"],
            Cjk::Sc => &["Noto Sans", "DejaVu Sans", "Noto Color Emoji", "Noto Sans Symbols", "Noto Sans Symbols 2", "Noto Sans CJK SC", "Noto Sans Math", "FreeSans"],
            Cjk::Ja => &["Noto Sans", "DejaVu Sans", "Noto Color Emoji", "Noto Sans Symbols", "Noto Sans Symbols 2", "Noto Sans CJK JP", "Noto Sans Math", "FreeSans"],
            Cjk::Ko => &["Noto Sans", "DejaVu Sans", "Noto Color Emoji", "Noto Sans Symbols", "Noto Sans Symbols 2", "Noto Sans CJK KR", "Noto Sans Math", "FreeSans"],
        }
    }

    pub fn script(s: Script, c: Cjk) -> &'static [&'static str] {
        match s {
            Script::Han => han(c),
            Script::Bopomofo => han(Cjk::Tc),
            Script::Hiragana | Script::Katakana => han(Cjk::Ja),
            Script::Hangul => han(Cjk::Ko),
            Script::Arabic => &["Noto Sans Arabic", "Noto Naskh Arabic", "DejaVu Sans", "FreeSerif"],
            Script::Hebrew => &["Noto Sans Hebrew", "DejaVu Sans", "FreeSans"],
            Script::Devanagari => &["Noto Sans Devanagari", "Lohit Devanagari", "FreeSans"],
            Script::Thai => &["Noto Sans Thai", "Loma", "Garuda", "FreeSerif"],
            _ => &[],
        }
    }
}

/// The fallback order handed to cosmic-text. Scripts without an entry of
/// ours use cosmic-text's own platform list (empty on Android, where the
/// last resort is every installed face in turn).
pub(crate) struct Lists {
    pub cjk: Cjk,
    builtin: cosmic_text::PlatformFallback,
}

impl Lists {
    pub fn new(cjk: Cjk) -> Self {
        Self { cjk, builtin: cosmic_text::PlatformFallback }
    }
}

impl Fallback for Lists {
    fn common_fallback(&self) -> &[&'static str] {
        platform::common(self.cjk)
    }

    fn forbidden_fallback(&self) -> &[&'static str] {
        &[]
    }

    fn script_fallback(&self, script: Script, locale: &str) -> &[&'static str] {
        let ours = platform::script(script, self.cjk);
        if ours.is_empty() {
            self.builtin.script_fallback(script, locale)
        } else {
            ours
        }
    }
}

/// Whether the locale's own script is Han, kana or Hangul.
pub(crate) fn is_cjk_locale(locale: &str) -> bool {
    let l = locale.to_ascii_lowercase();
    l.starts_with("zh") || l.starts_with("ja") || l.starts_with("ko")
}

/// Families to try as the primary (default) font, best first. For a CJK
/// locale the CJK UI font comes first, as on the platforms themselves: it
/// has Latin glyphs too, and every ideograph then shapes in one pass
/// instead of failing in the Latin font first (UAX #14 makes each
/// ideograph its own word, so that miss would be paid per character).
pub(crate) fn primary_candidates(locale: &str, cjk: Cjk) -> Vec<&'static str> {
    let mut v = Vec::new();
    if is_cjk_locale(locale) {
        v.extend_from_slice(platform::han(cjk));
    }
    v.extend_from_slice(platform::PRIMARY);
    v
}

/// Load the platform's installed fonts. Returns the database and the
/// directories (or "system") that were scanned.
pub(crate) fn system_database() -> (Database, Vec<String>) {
    let mut db = Database::new();
    #[allow(unused_mut)]
    let mut sources = Vec::new();
    #[cfg(target_os = "android")]
    {
        // fontdb has no Android branch; these are the AOSP locations.
        for dir in ["/system/fonts", "/product/fonts", "/system/product/fonts"] {
            if std::path::Path::new(dir).is_dir() {
                db.load_fonts_dir(dir);
                sources.push(dir.to_string());
            }
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        db.load_system_fonts();
        #[cfg(target_os = "windows")]
        sources.push(std::env::var("SYSTEMROOT").map(|r| format!("{r}\\Fonts")).unwrap_or_else(|_| r"C:\Windows\Fonts".into()));
        #[cfg(not(target_os = "windows"))]
        sources.push("system font directories".to_string());
    }
    (db, sources)
}

pub(crate) fn has_family(db: &Database, name: &str) -> bool {
    db.faces().any(|f| f.families.iter().any(|(n, _)| n == name))
}

pub(crate) fn face_name(db: &Database, id: ID) -> String {
    db.face(id)
        .map(|f| f.families.first().map(|(n, _)| n.clone()).unwrap_or_else(|| f.post_script_name.clone()))
        .unwrap_or_else(|| "?".into())
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

/// Whether this colour font can be drawn in colour: bitmap strikes
/// (CBDT/sbix, through swash), COLR version 0 layers (swash) or version 1
/// paint graphs (`colr.rs`). SVG-only colour fonts cannot.
fn color_renderable(data: &[u8], index: u32) -> bool {
    let Some(f) = swash::FontRef::from_index(data, index as usize) else { return false };
    let tag = |t: &[u8; 4]| u32::from_be_bytes(*t);
    if f.table(tag(b"CBDT")).is_some() || f.table(tag(b"sbix")).is_some() {
        return true;
    }
    f.table(tag(b"COLR")).is_some_and(|t| be16(t, 0).is_some_and(|v| v >= 1) || be16(t, 2).is_some_and(|base_glyphs| base_glyphs > 0))
}

/// Drop emoji faces that cannot be drawn in colour (SVG only) when another
/// emoji face can. Returns what was removed, for `describe`.
pub(crate) fn prune_color_fonts(db: &mut Database) -> Vec<String> {
    let emoji: Vec<(ID, String, bool)> = db
        .faces()
        .filter(|f| f.families.iter().any(|(n, _)| n.contains("Emoji")))
        .map(|f| (f.id, f.post_script_name.clone()))
        .collect::<Vec<_>>()
        .into_iter()
        .map(|(id, name)| {
            let ok = db.with_face_data(id, color_renderable).unwrap_or(false);
            (id, name, ok)
        })
        .collect();
    if !emoji.iter().any(|e| e.2) {
        return Vec::new();
    }
    let mut removed = Vec::new();
    for (id, name, ok) in emoji {
        if !ok {
            db.remove_face(id);
            removed.push(name);
        }
    }
    removed
}

/// First installed family of each key script's fallback list, for `describe`.
pub(crate) fn coverage_report(db: &Database, cjk: Cjk) -> Vec<(&'static str, Option<&'static str>)> {
    let first = |list: &'static [&'static str]| list.iter().copied().find(|n| has_family(db, n));
    let emoji = platform::common(cjk).iter().copied().filter(|n| n.contains("Emoji")).find(|n| has_family(db, n));
    vec![
        ("Han", first(platform::han(cjk))),
        ("Arabic", first(platform::script(Script::Arabic, cjk))),
        ("Hebrew", first(platform::script(Script::Hebrew, cjk))),
        ("Devanagari", first(platform::script(Script::Devanagari, cjk))),
        ("Thai", first(platform::script(Script::Thai, cjk))),
        ("Emoji", emoji),
    ]
}

/// Faces to probe when deciding whether any font covers a character: the
/// regular faces of the families named in the fallback lists (likely, and
/// loaded by the shaper anyway), then every other face.
pub(crate) fn probe_order(db: &Database, cjk: Cjk, primary: &str) -> Vec<ID> {
    let mut names: Vec<&str> = vec![primary];
    names.extend_from_slice(platform::common(cjk));
    for s in [Script::Han, Script::Arabic, Script::Hebrew, Script::Devanagari, Script::Thai, Script::Bengali, Script::Tamil, Script::Hangul, Script::Hiragana] {
        names.extend_from_slice(platform::script(s, cjk));
    }
    let mut ids: Vec<ID> = Vec::new();
    for n in names {
        if let Some(id) = primary_query(db, n) {
            if !ids.contains(&id) && db.face(id).is_some_and(|f| f.families.iter().any(|(fam, _)| fam == n)) {
                ids.push(id);
            }
        }
    }
    let known: std::collections::HashSet<ID> = ids.iter().copied().collect();
    ids.extend(db.faces().map(|f| f.id).filter(|id| !known.contains(id)));
    ids
}

pub(crate) fn family_of_first_face(db: &Database) -> Option<String> {
    db.faces().next().and_then(|f| f.families.first().map(|(n, _)| n.clone()))
}

/// Regular upright faces sort first: a database query for the primary
/// family must not land on a Light or Italic face.
pub(crate) fn primary_query(db: &Database, family: &str) -> Option<ID> {
    db.query(&fontdb::Query { families: &[fontdb::Family::Name(family)], ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locales() {
        assert_eq!(cjk_for_locale("zh-TW"), Cjk::Tc);
        assert_eq!(cjk_for_locale("zh_Hant_HK"), Cjk::Tc);
        assert_eq!(cjk_for_locale("zh-CN"), Cjk::Sc);
        assert_eq!(cjk_for_locale("zh"), Cjk::Sc);
        assert_eq!(cjk_for_locale("ja-JP"), Cjk::Ja);
        assert_eq!(cjk_for_locale("ko"), Cjk::Ko);
        assert_eq!(cjk_for_locale("en-US"), Cjk::Tc);
    }

    #[test]
    fn every_platform_list_has_cjk_and_emoji() {
        assert!(!platform::han(Cjk::Tc).is_empty());
        assert!(platform::common(Cjk::Tc).iter().any(|n| n.contains("Emoji")));
        assert!(!platform::script(Script::Arabic, Cjk::Tc).is_empty());
        assert!(!platform::script(Script::Thai, Cjk::Tc).is_empty());
    }
}
