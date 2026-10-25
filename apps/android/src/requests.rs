//! Checks on what a page asks of the platform, applied before anything
//! crosses into Java, and the bookkeeping of file dialogs.
//!
//! Android hands clipboard text, a suggested file name and MIME filters to
//! the system in binder transactions. A transaction over about 1 MB (the
//! whole process shares one 1 MB buffer) throws `TransactionTooLargeException`,
//! which used to crash the app when a page copied a few MB of text. So each
//! of them has a size limit here, well below that.
//!
//! File dialogs: one at a time, and every answer is tied to the page that
//! asked. Page request ids restart at 1 on every page, so the host gives each
//! dialog a token of its own that is never reused; an answer that arrives
//! after the page was replaced finds no token and is dropped.

use std::collections::HashMap;
use std::sync::Arc;

use anypage_engine::{HostResult, PageRequest};

/// Clipboard text a page may copy at once, in UTF-8 bytes. The clip travels
/// as UTF-16 (up to twice this) in one binder transaction.
pub const MAX_COPY: usize = 256 * 1024;
/// Bytes of a suggested file name (most file systems allow 255).
pub const MAX_NAME: usize = 255;
/// Filters in one open request.
pub const MAX_FILTERS: usize = 32;
/// Bytes of one filter ("pdf", "image/png").
pub const MAX_FILTER: usize = 100;

/// The start of `s`, at most `n` bytes, for messages (toasts, logs).
pub fn short_message(s: &str, n: usize) -> String {
    short(s, n)
}

fn short(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Whether `text` may go to the clipboard; the error says why not.
pub fn check_copy(text: &str) -> Result<(), String> {
    if text.len() > MAX_COPY {
        return Err(format!(
            "copy refused: {} KB of text, the clipboard takes at most {} KB at once",
            text.len().div_ceil(1024),
            MAX_COPY / 1024
        ));
    }
    Ok(())
}

/// The name a save dialog suggests: no path separators or control
/// characters, at most [`MAX_NAME`] bytes, keeping a short extension.
pub fn save_name(name: &str) -> String {
    let clean: String = name.chars().map(|c| if c.is_control() || c == '/' || c == '\\' { '_' } else { c }).collect();
    let clean = clean.trim();
    if clean.is_empty() {
        return "file".into();
    }
    if clean.len() <= MAX_NAME {
        return clean.to_string();
    }
    let ext = match clean.rfind('.') {
        Some(i) if i > 0 && clean.len() - i <= 16 => &clean[i..],
        _ => "",
    };
    let mut end = MAX_NAME - ext.len();
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &clean[..end], ext)
}

/// Filters for the open dialog: extensions ("pdf", ".txt") or MIME types
/// ("image/png", "image/*"). Empty ones are skipped; too many, too long or
/// malformed ones fail the request with a message for the page.
pub fn open_filters(filters: &[String]) -> Result<Vec<String>, String> {
    if filters.len() > MAX_FILTERS {
        return Err(format!("open_file: {} filters, at most {MAX_FILTERS}", filters.len()));
    }
    let mut out = Vec::with_capacity(filters.len());
    for f in filters {
        let f = f.trim();
        if f.is_empty() {
            continue;
        }
        if f.len() > MAX_FILTER || f.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(format!("open_file: bad filter \"{}\" (an extension or a MIME type, at most {MAX_FILTER} bytes)", short(f, 40)));
        }
        out.push(f.to_string());
    }
    Ok(out)
}

struct Dialog {
    /// The page's request id.
    id: u64,
    /// The bytes of a save request.
    save: Option<Arc<Vec<u8>>>,
    /// The picker is on screen (not yet answered).
    picking: bool,
}

/// What a picked document is for.
pub struct Picked {
    pub id: u64,
    pub save: Option<Arc<Vec<u8>>>,
}

/// Open and save dialogs from the time the page asks until it has its answer.
#[derive(Default)]
pub struct FileDialogs {
    next: u64,
    jobs: HashMap<u64, Dialog>,
}

impl FileDialogs {
    /// A dialog for page request `id` (`save`: the bytes to write); the
    /// token goes to Kotlin and comes back with the answer. Fails while
    /// another picker is still on screen.
    pub fn start(&mut self, id: u64, save: Option<Arc<Vec<u8>>>) -> Result<u64, String> {
        if self.jobs.values().any(|d| d.picking) {
            return Err("a file dialog is already open".into());
        }
        self.next += 1;
        self.jobs.insert(self.next, Dialog { id, save, picking: true });
        Ok(self.next)
    }

    /// The user picked a document for `token`: the picker is gone and the
    /// reading or writing starts. `None` when the page that asked is gone.
    pub fn picked(&mut self, token: u64) -> Option<Picked> {
        let d = self.jobs.get_mut(&token)?;
        d.picking = false;
        Some(Picked { id: d.id, save: d.save.clone() })
    }

    /// The final answer for `token` is here: the page's request id, or
    /// `None` when that page is gone (the answer is then dropped).
    pub fn finish(&mut self, token: u64) -> Option<u64> {
        self.jobs.remove(&token).map(|d| d.id)
    }

    /// The page was replaced: answers still on their way are dropped.
    pub fn clear(&mut self) {
        self.jobs.clear();
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
}

/// What the host does with one page request.
#[derive(Debug, PartialEq)]
pub enum Route {
    /// Put the text on the clipboard.
    Copy(String),
    /// Read the clipboard; the answer comes back as `Engine::paste`.
    Paste,
    /// Show the open picker for dialog `token`.
    Open { token: u64, filters: Vec<String> },
    /// Show the save picker for dialog `token` (the bytes wait in [`FileDialogs`]).
    Save { token: u64, name: String, size: usize },
    /// Refused, with nothing to answer: tell the user.
    Notice(String),
    /// Answer the page's request `id` at once.
    Answer(u64, HostResult),
    /// Open the page package `assets/pages/<name>/` (a valid folder name):
    /// Kotlin reads it, the render thread switches to it.
    Navigate(String),
    /// The engine serves it itself (fit).
    Engine,
}

/// Decide what to do with `req`: every limit above applies here, and file
/// dialogs are registered in `dialogs`.
pub fn route(req: PageRequest, dialogs: &mut FileDialogs) -> Route {
    match req {
        PageRequest::Copy(text) => match check_copy(&text) {
            Ok(()) => Route::Copy(text),
            Err(msg) => Route::Notice(msg),
        },
        PageRequest::Paste => Route::Paste,
        PageRequest::OpenFile { id, filters } => match open_filters(&filters).and_then(|f| Ok((dialogs.start(id, None)?, f))) {
            Ok((token, filters)) => Route::Open { token, filters },
            Err(msg) => Route::Answer(id, HostResult::Failed(msg)),
        },
        PageRequest::SaveFile { id, name, bytes } => {
            let size = bytes.len();
            match dialogs.start(id, Some(bytes)) {
                Ok(token) => Route::Save { token, name: save_name(&name), size },
                Err(msg) => Route::Answer(id, HostResult::Failed(msg)),
            }
        }
        PageRequest::FitPage | PageRequest::FitRect(_) => Route::Engine,
        PageRequest::Navigate(name) => {
            if crate::packages::valid_page_name(&name) {
                Route::Navigate(name)
            } else {
                Route::Notice(format!("page.navigate: \"{}\" is not a page package name", short(&name, 64)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_has_a_size_limit() {
        assert!(check_copy("").is_ok());
        assert!(check_copy(&"x".repeat(MAX_COPY)).is_ok());
        let e = check_copy(&"x".repeat(3 * 1024 * 1024)).unwrap_err();
        assert!(e.contains("3072 KB") && e.contains("256 KB"), "{e}");
        // the limit is in bytes: 3-byte CJK characters reach it sooner
        assert!(check_copy(&"字".repeat(MAX_COPY / 3)).is_ok());
        assert!(check_copy(&"字".repeat(MAX_COPY / 3 + 1)).is_err());
    }

    #[test]
    fn save_names_are_short_and_plain() {
        assert_eq!(save_name("note.txt"), "note.txt");
        assert_eq!(save_name("  "), "file");
        assert_eq!(save_name("a/b\\c\n.txt"), "a_b_c_.txt");
        let long = format!("{}.pdf", "長".repeat(400));
        let n = save_name(&long);
        assert!(n.len() <= MAX_NAME && n.ends_with(".pdf") && n.starts_with('長'), "{} bytes: {n}", n.len());
        // a megabyte of name, no extension worth keeping
        let n = save_name(&"x".repeat(1 << 20));
        assert_eq!(n.len(), MAX_NAME);
        let n = save_name(&format!("{}.{}", "y".repeat(300), "z".repeat(300)));
        assert_eq!(n.len(), MAX_NAME);
        assert!(n.chars().all(|c| c == 'y'));
    }

    #[test]
    fn filters_are_checked() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(open_filters(&v(&["txt", " .pdf ", "", "image/*"])).unwrap(), v(&["txt", ".pdf", "image/*"]));
        assert!(open_filters(&v(&["a"; MAX_FILTERS])).is_ok());
        assert!(open_filters(&v(&["a"; MAX_FILTERS + 1])).unwrap_err().contains("filters"));
        let e = open_filters(&["x".repeat(1 << 20)]).unwrap_err();
        assert!(e.len() < 200, "the message stays short: {} bytes", e.len());
        assert!(open_filters(&v(&["text plain"])).is_err());
    }

    #[test]
    fn navigation_names_are_checked() {
        let mut d = FileDialogs::default();
        assert_eq!(route(PageRequest::Navigate("pdf-demo".into()), &mut d), Route::Navigate("pdf-demo".into()));
        for bad in ["../secrets", "a/b", "", ".hidden", "頁面"] {
            match route(PageRequest::Navigate(bad.into()), &mut d) {
                Route::Notice(m) => assert!(m.contains("not a page package name"), "{m}"),
                other => panic!("{bad}: {other:?}"),
            }
        }
        // a megabyte of name stays a short message
        let Route::Notice(m) = route(PageRequest::Navigate("x".repeat(1 << 20)), &mut d) else { panic!() };
        assert!(m.len() < 200, "{} bytes", m.len());
        assert!(d.is_empty());
    }

    #[test]
    fn one_dialog_at_a_time_and_answers_tied_to_the_page() {
        let mut d = FileDialogs::default();
        let t1 = d.start(1, None).unwrap();
        assert_eq!(d.start(2, None).unwrap_err(), "a file dialog is already open");
        // picked: the picker is gone, the read runs; another dialog may open
        let p = d.picked(t1).unwrap();
        assert_eq!((p.id, p.save.is_none()), (1, true));
        let t2 = d.start(2, Some(Arc::new(b"bytes".to_vec()))).unwrap();
        assert_eq!(d.finish(t1), Some(1));
        assert_eq!(d.finish(t1), None, "answered once");
        // the page is replaced while its save dialog is open
        d.clear();
        assert!(d.picked(t2).is_none());
        assert_eq!(d.finish(t2), None);
        // the new page's ids start at 1 again, but its token is new
        let t3 = d.start(1, None).unwrap();
        assert!(t3 != t1 && t3 != t2);
        assert_eq!(d.finish(t1), None, "an old answer does not reach the new page");
        assert_eq!(d.len(), 1);
        assert_eq!(d.finish(t3), Some(1));
        assert!(d.is_empty());
    }
}
