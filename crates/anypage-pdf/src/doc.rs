//! The document resource (`pdf.document`).
//!
//! One document is kept as up to three views, all tied to a revision number
//! that every successful edit moves on:
//!
//! * a [`lopdf::Document`] for structure and edits, loaded on first need
//!   (opening a 200 MB file to look at it never parses every object);
//! * a [`Snapshot`]: the bytes of a revision parsed by hayro, used for
//!   rendering and text. After an edit it is rebuilt lazily (the lopdf
//!   document is written out and read back) by the first caller that needs
//!   pixels or text: in practice a tile worker or a job, since the page
//!   redraws right after an edit. Inline text calls (`doc:text`, `doc:search`
//!   ...) made right after an edit pay for the rebuild themselves;
//! * [`Meta`]: page geometry and document info of the current revision, which
//!   every edit updates from the lopdf document. `page_count`, `page_size`
//!   and `metadata` read only this, so cells that lay out a page never wait
//!   for a rebuild, and a rebuild never holds the lock they read.
//!
//! Files from outside are checked for decompression bombs before either
//! library sees them ([`crate::limits`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock, RwLockWriteGuard};

use hayro::hayro_syntax::page::{Page, Rotation};
use hayro::hayro_syntax::{LoadPdfError, Pdf};
use lopdf::{dictionary, Dictionary, Document, LoadOptions, Object, ObjectId, Stream};

use crate::edit::EditCx;
use crate::geom::PageGeom;
use crate::text::PageText;
use crate::util::{text_string, R};

pub const KIND: &str = "pdf.document";
/// Largest file `pdf.open` accepts.
pub const MAX_BYTES: usize = 512 << 20;
/// Largest stream lopdf may inflate while loading (decompression bombs).
const MAX_INFLATE: usize = 256 << 20;
/// Pages of extracted text a snapshot keeps...
const TEXT_CACHE_PAGES: usize = 64;
/// ...and the bytes they may take (a crafted page of 400k glyphs is ~40 MB).
pub const TEXT_CACHE_BYTES: usize = 32 << 20;

pub struct PdfDoc {
    state: RwLock<State>,
    /// The newest snapshot built (possibly of an older revision).
    snap: RwLock<Option<Arc<Snapshot>>>,
    /// Geometry and info of the current revision.
    meta: RwLock<Arc<Meta>>,
    /// One rebuild at a time.
    building: Mutex<()>,
    password: String,
}

pub(crate) struct State {
    /// The file as opened; `None` for documents made in memory.
    pub original: Option<Arc<Vec<u8>>>,
    pub lo: Option<Document>,
    pub lo_err: Option<String>,
    /// `lo` holds changes the original bytes do not have.
    pub edited: bool,
    pub revision: u64,
    pub cx: EditCx,
}

/// Page geometry and document info of one revision.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub revision: u64,
    pub geoms: Vec<PageGeom>,
    pub info: Info,
}

/// The document information dictionary, decoded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Info {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub keywords: Option<String>,
    pub creator: Option<String>,
    pub producer: Option<String>,
    /// ISO 8601, e.g. `2026-10-24T12:00:00+08:00`.
    pub created: Option<String>,
    pub modified: Option<String>,
    /// From the header, e.g. `1.7`.
    pub version: String,
    /// Size of the file as opened; `None` once edited (or made in memory).
    pub bytes: Option<usize>,
}

impl Meta {
    pub fn page_count(&self) -> usize {
        self.geoms.len()
    }
    /// 1-based page number to its geometry, with a clear error.
    pub fn geom(&self, page: f64) -> R<PageGeom> {
        Ok(self.geoms[page_index(page, self.geoms.len())?])
    }
}

/// Extracted pages, least recently used out first.
#[derive(Default)]
struct TextCache {
    pages: HashMap<usize, (Arc<PageText>, usize, u64)>,
    bytes: usize,
    tick: u64,
}

impl TextCache {
    fn get(&mut self, i: usize) -> Option<Arc<PageText>> {
        self.tick += 1;
        let tick = self.tick;
        self.pages.get_mut(&i).map(|e| {
            e.2 = tick;
            e.0.clone()
        })
    }

    /// Keep `t`; `evict` makes room by dropping the least recently used
    /// pages, otherwise it is kept only if there is room.
    fn put(&mut self, i: usize, t: &Arc<PageText>, evict: bool) {
        let b = t.approx_bytes();
        if b > TEXT_CACHE_BYTES || self.pages.contains_key(&i) {
            return;
        }
        let full = |c: &Self| c.pages.len() >= TEXT_CACHE_PAGES || c.bytes + b > TEXT_CACHE_BYTES;
        if !evict && full(self) {
            return;
        }
        while full(self) {
            let Some((&old, _)) = self.pages.iter().min_by_key(|(_, e)| e.2) else { break };
            if let Some(e) = self.pages.remove(&old) {
                self.bytes -= e.1;
            }
        }
        self.tick += 1;
        self.pages.insert(i, (t.clone(), b, self.tick));
        self.bytes += b;
    }
}

/// One revision of the document as hayro sees it. Immutable; shared by tile
/// workers, jobs and inline calls.
pub struct Snapshot {
    pub revision: u64,
    pub bytes: Arc<Vec<u8>>,
    pub pdf: Pdf,
    pub geoms: Vec<PageGeom>,
    text: Mutex<TextCache>,
}

fn hayro_err(e: LoadPdfError) -> String {
    match e {
        LoadPdfError::Decryption(d) => {
            format!("the PDF is encrypted and could not be decrypted ({d:?}); pass its password: pdf.open(bytes, {{password = \"...\"}})")
        }
        LoadPdfError::Invalid => "not a PDF file, or damaged beyond repair".into(),
    }
}

pub fn geom_of(page: &Page<'_>) -> PageGeom {
    let m = page.media_box();
    let c = page.crop_box();
    let rot = match page.rotation() {
        Rotation::None => 0,
        Rotation::Horizontal => 90,
        Rotation::Flipped => 180,
        Rotation::FlippedHorizontal => 270,
    };
    PageGeom::new([m.x0, m.y0, m.x1, m.y1], Some([c.x0, c.y0, c.x1, c.y1]), rot)
}

fn never() -> bool {
    false
}

impl Snapshot {
    fn build(bytes: Arc<Vec<u8>>, revision: u64, password: &str) -> R<Snapshot> {
        crate::guard("reading the PDF", || {
            let pdf = Pdf::new_with_password(bytes.clone(), password).map_err(hayro_err)?;
            let geoms = pdf.pages().iter().map(geom_of).collect();
            Ok(Snapshot { revision, bytes, pdf, geoms, text: Mutex::new(TextCache::default()) })
        })
    }

    pub fn page_count(&self) -> usize {
        self.geoms.len()
    }

    /// 1-based page number to index, with a clear error.
    pub fn index(&self, page: f64) -> R<usize> {
        page_index(page, self.page_count())
    }

    pub fn page(&self, i: usize) -> R<&Page<'_>> {
        self.pdf.pages().get(i).ok_or_else(|| format!("page {} does not exist (the document has {})", i + 1, self.page_count()))
    }

    /// Info as hayro reads it.
    pub fn info(&self) -> Info {
        let m = crate::guard("reading the document info", || Ok(self.pdf.metadata().clone())).unwrap_or_default();
        let text = |v: &Option<Vec<u8>>| v.as_deref().map(crate::util::decode_text_string);
        let date = |d: &Option<hayro::hayro_syntax::object::DateTime>| d.map(|t| iso(t.year, t.month, t.day, t.hour, t.minute, t.second, t.utc_offset_hour, t.utc_offset_minute));
        let version = format!("{:?}", self.pdf.version());
        let version = version.trim_start_matches("Pdf");
        let version = if version.len() == 2 { format!("{}.{}", &version[..1], &version[1..]) } else { version.to_string() };
        Info {
            title: text(&m.title),
            author: text(&m.author),
            subject: text(&m.subject),
            keywords: text(&m.keywords),
            creator: text(&m.creator),
            producer: text(&m.producer),
            created: date(&m.creation_date),
            modified: date(&m.modification_date),
            version,
            bytes: Some(self.bytes.len()),
        }
    }

    /// Text of page `i` (0-based), extracted once per revision while the
    /// cache has room for it.
    pub fn text(&self, i: usize) -> R<Arc<PageText>> {
        self.text_with(i, &never)
    }

    /// [`Snapshot::text`] that stops with [`crate::text::CANCELLED`] when
    /// `cancel` says so.
    pub fn text_with(&self, i: usize, cancel: &dyn Fn() -> bool) -> R<Arc<PageText>> {
        if let Some(t) = self.cache().get(i) {
            return Ok(t);
        }
        let t = Arc::new(crate::text::extract_with(self, i, &hayro::hayro_interpret::InterpreterCache::new(), cancel)?);
        self.cache().put(i, &t, true);
        Ok(t)
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, TextCache> {
        self.text.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Bytes the text cache holds now.
    pub fn text_cache_bytes(&self) -> usize {
        self.cache().bytes
    }

    /// Text of several pages in order, parsing shared fonts once; `f`
    /// returns false to stop early (enough results). Pages read here are
    /// cached only while there is room, so a whole-document pass does not
    /// push out the pages on screen. Stops with [`crate::text::CANCELLED`]
    /// when `cancel` says so.
    pub fn text_each(&self, pages: &[usize], cancel: &dyn Fn() -> bool, mut f: impl FnMut(usize, Arc<PageText>) -> R<bool>) -> R<()> {
        let shared = hayro::hayro_interpret::InterpreterCache::new();
        for &i in pages {
            if cancel() {
                return Err(crate::text::CANCELLED.into());
            }
            let cached = self.cache().get(i);
            let t = match cached {
                Some(t) => t,
                None => match crate::text::extract_with(self, i, &shared, cancel) {
                    Ok(t) => {
                        let t = Arc::new(t);
                        self.cache().put(i, &t, false);
                        t
                    }
                    Err(e) if e == crate::text::CANCELLED => return Err(e),
                    // one damaged page does not stop a search of the others
                    Err(e) => {
                        log::warn!(target: "anypage_pdf", "page {}: {e}", i + 1);
                        Arc::new(PageText::default())
                    }
                },
            };
            if !f(i, t)? {
                break;
            }
        }
        Ok(())
    }
}

/// 1-based page number to a 0-based index.
pub fn page_index(page: f64, count: usize) -> R<usize> {
    if !page.is_finite() || page.fract() != 0.0 || page < 1.0 || page > count as f64 {
        return Err(format!("page {page} does not exist (pages are 1..={count})"));
    }
    Ok(page as usize - 1)
}

fn load_lopdf(bytes: &[u8], password: &str) -> R<Document> {
    crate::guard("reading the document structure", || {
        let opts = LoadOptions {
            password: (!password.is_empty()).then(|| password.to_string()),
            max_decompressed_size: Some(MAX_INFLATE),
            ..Default::default()
        };
        let mut doc = Document::load_mem_with_options(bytes, opts).map_err(|e| e.to_string())?;
        if doc.is_encrypted() {
            // an empty user password opens most "protected" files
            doc.decrypt(password).map_err(|e| format!("the PDF is encrypted: {e}"))?;
        }
        if doc.catalog().is_err() {
            return Err("the PDF has no document catalog".into());
        }
        Ok(doc)
    })
}

pub fn serialize(lo: &mut Document) -> R<Vec<u8>> {
    crate::guard("writing the PDF", || {
        let mut out = Vec::new();
        lo.save_to(&mut out).map_err(|e| format!("could not write the PDF: {e}"))?;
        Ok(out)
    })
}

fn lock_w(l: &RwLock<State>) -> RwLockWriteGuard<'_, State> {
    l.write().unwrap_or_else(|p| p.into_inner())
}

impl State {
    fn ensure_lo(&mut self, password: &str) -> R<&mut Document> {
        if self.lo.is_none() {
            if let Some(e) = &self.lo_err {
                return Err(e.clone());
            }
            let orig = self.original.clone().ok_or("internal: the document has no data")?;
            match load_lopdf(&orig, password) {
                Ok(d) => self.lo = Some(d),
                Err(e) => {
                    let e = format!("this PDF can be shown but not edited or inspected structurally: {e}");
                    self.lo_err = Some(e.clone());
                    return Err(e);
                }
            }
        }
        self.lo.as_mut().ok_or_else(|| "internal: document lost".into())
    }
}

/// `YYYY-MM-DDTHH:MM:SS+HH:MM`.
#[allow(clippy::too_many_arguments)]
fn iso(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8, off_h: i8, off_m: u8) -> String {
    let sign = if off_h < 0 { "-" } else { "+" };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{sign}{:02}:{off_m:02}", off_h.unsigned_abs())
}

/// A PDF date string (`D:YYYYMMDDHHmmSSOHH'mm'`) read the way hayro reads it,
/// so a document's dates look the same before and after an edit.
pub fn pdf_date(b: &[u8]) -> Option<String> {
    let mut r = b.strip_prefix(b"D:")?;
    fn num(r: &mut &[u8], n: usize, min: u16, max: u16) -> Option<u16> {
        if matches!(r.first()?, b'-' | b'+' | b'Z') || r.len() < n {
            return None;
        }
        let (head, tail) = r.split_at(n);
        *r = tail;
        let v: u16 = std::str::from_utf8(head).ok()?.parse().ok()?;
        (min..=max).contains(&v).then_some(v)
    }
    let year = num(&mut r, 4, 0, 9999)?;
    let month = num(&mut r, 2, 1, 12).unwrap_or(1) as u8;
    let day = num(&mut r, 2, 1, 31).unwrap_or(1) as u8;
    let hour = num(&mut r, 2, 0, 23).unwrap_or(0) as u8;
    let minute = num(&mut r, 2, 0, 59).unwrap_or(0) as u8;
    let second = num(&mut r, 2, 0, 59).unwrap_or(0) as u8;
    let (oh, om) = match r.split_first() {
        Some((&sign, rest)) => {
            r = rest;
            let m: i8 = if sign == b'-' { -1 } else { 1 };
            let h = m * num(&mut r, 2, 0, 23).unwrap_or(0) as i8;
            r = r.strip_prefix(b"'").unwrap_or(r);
            (h, num(&mut r, 2, 0, 59).unwrap_or(0) as u8)
        }
        None => (0, 0),
    };
    Some(iso(year, month, day, hour, minute, second, oh, om))
}

/// Geometry and info of a lopdf document (after an edit).
fn meta_of(lo: &Document, revision: u64) -> Meta {
    use crate::util::{decode_text_string, deref, dict_get};
    let geoms = crate::edit::page_ids(lo).into_iter().map(|p| crate::edit::geom(lo, p)).collect();
    let info = match lo.trailer.get(b"Info").map(|o| deref(lo, o)) {
        Ok(Object::Dictionary(d)) => Some(d),
        _ => None,
    };
    let string = |k: &[u8]| match info.and_then(|d| dict_get(lo, d, k)) {
        Some(Object::String(s, _)) => Some(s.as_slice()),
        _ => None,
    };
    let text = |k: &[u8]| string(k).map(decode_text_string);
    let date = |k: &[u8]| string(k).and_then(pdf_date);
    Meta {
        revision,
        geoms,
        info: Info {
            title: text(b"Title"),
            author: text(b"Author"),
            subject: text(b"Subject"),
            keywords: text(b"Keywords"),
            creator: text(b"Creator"),
            producer: text(b"Producer"),
            created: date(b"CreationDate"),
            modified: date(b"ModDate"),
            version: lo.version.clone(),
            bytes: None,
        },
    }
}

impl PdfDoc {
    /// Open a file. hayro must be able to read it (that is what shows it);
    /// when it cannot, lopdf's more forgiving reader gets a chance to rebuild it.
    pub fn open(bytes: Arc<Vec<u8>>, password: &str) -> R<PdfDoc> {
        if bytes.len() > MAX_BYTES {
            return Err(format!("the file is {} MB; the limit is {} MB", bytes.len() >> 20, MAX_BYTES >> 20));
        }
        if bytes.is_empty() {
            return Err("the file is empty".into());
        }
        crate::limits::check_streams(&bytes)?;
        let (snap, lo, edited) = match Snapshot::build(bytes.clone(), 0, password) {
            Ok(s) => (s, None, false),
            Err(e) => {
                let mut lo = load_lopdf(&bytes, password).map_err(|le| format!("{e} ({le})"))?;
                let rebuilt = Arc::new(serialize(&mut lo)?);
                let s = Snapshot::build(rebuilt, 0, password).map_err(|_| e)?;
                (s, Some(lo), true)
            }
        };
        let meta = Meta { revision: 0, geoms: snap.geoms.clone(), info: snap.info() };
        let state = State { original: Some(bytes), lo, lo_err: None, edited, revision: 0, cx: EditCx::default() };
        Ok(PdfDoc { state: RwLock::new(state), snap: RwLock::new(Some(Arc::new(snap))), meta: RwLock::new(Arc::new(meta)), building: Mutex::new(()), password: password.to_string() })
    }

    /// A document built in memory (new documents, copies).
    pub fn from_document(lo: Document) -> PdfDoc {
        let meta = crate::guard("reading the document", || Ok(meta_of(&lo, 0))).unwrap_or_default();
        let state = State { original: None, lo: Some(lo), lo_err: None, edited: true, revision: 0, cx: EditCx::default() };
        PdfDoc { state: RwLock::new(state), snap: RwLock::new(None), meta: RwLock::new(Arc::new(meta)), building: Mutex::new(()), password: String::new() }
    }

    /// The current revision (read from [`Meta`], so it never waits for a
    /// rebuild that holds the document).
    pub fn revision(&self) -> u64 {
        self.meta().revision
    }

    /// Page geometry and info of the current revision. Never waits for a
    /// snapshot rebuild.
    pub fn meta(&self) -> Arc<Meta> {
        self.meta.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn current_snap(&self, revision: u64) -> Option<Arc<Snapshot>> {
        self.snap.read().unwrap_or_else(|p| p.into_inner()).as_ref().filter(|s| s.revision == revision).cloned()
    }

    /// Whether the current revision has been read back by hayro already.
    pub fn has_current_snapshot(&self) -> bool {
        self.current_snap(self.revision()).is_some()
    }

    /// The current revision as hayro sees it, rebuilt here if an edit made
    /// it stale.
    pub fn snapshot(&self) -> R<Arc<Snapshot>> {
        if let Some(s) = self.current_snap(self.revision()) {
            return Ok(s);
        }
        let _one = self.building.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(s) = self.current_snap(self.revision()) {
            return Ok(s);
        }
        // the edit lock is held only while the document is written out
        let (bytes, revision) = {
            let mut st = lock_w(&self.state);
            let revision = st.revision;
            let bytes = if st.edited {
                let lo = st.ensure_lo(&self.password)?;
                Arc::new(serialize(lo)?)
            } else {
                st.original.clone().ok_or("internal: the document has no data")?
            };
            (bytes, revision)
        };
        let snap = Arc::new(Snapshot::build(bytes, revision, &self.password).map_err(|e| format!("the edited document could not be read back: {e}"))?);
        let mut slot = self.snap.write().unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().is_none_or(|s| s.revision < revision) {
            *slot = Some(snap.clone());
        }
        Ok(snap)
    }

    /// Read the structure (outline, forms, annotations).
    pub fn with_lo<T>(&self, f: impl FnOnce(&Document) -> R<T>) -> R<T> {
        {
            let st = self.state.read().unwrap_or_else(|p| p.into_inner());
            if let Some(lo) = &st.lo {
                return crate::guard("reading the document structure", || f(lo));
            }
        }
        let mut st = lock_w(&self.state);
        let lo = st.ensure_lo(&self.password)?;
        crate::guard("reading the document structure", || f(lo))
    }

    /// Change the document. An edit that fails changes nothing (the edit
    /// functions check their inputs before they touch the document) and keeps
    /// the revision; one that succeeds moves it on. A panic inside a library
    /// part way through also moves it on, since the document may have changed.
    pub fn edit<T>(&self, f: impl FnOnce(&mut Document, &mut EditCx) -> R<T>) -> R<(T, u64)> {
        let mut st = lock_w(&self.state);
        let password = self.password.clone();
        st.ensure_lo(&password)?;
        let State { lo, cx, .. } = &mut *st;
        let lo = lo.as_mut().ok_or("internal: document lost")?;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(lo, cx)));
        let r = match r {
            Ok(Err(e)) => return Err(e),
            Ok(Ok(v)) => Ok(v),
            Err(p) => {
                let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
                Err(format!("editing failed on this file (internal error: {msg})"))
            }
        };
        st.edited = true;
        st.revision += 1;
        let revision = st.revision;
        let meta = match st.lo.as_ref() {
            Some(lo) => crate::guard("reading the edited document", || Ok(meta_of(lo, revision))).unwrap_or_else(|_| Meta { revision, ..Meta::default() }),
            None => Meta { revision, ..Meta::default() },
        };
        *self.meta.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(meta);
        r.map(|v| (v, revision))
    }

    /// The file for the current revision.
    pub fn save(&self) -> R<Arc<Vec<u8>>> {
        Ok(self.snapshot()?.bytes.clone())
    }

    /// An independent copy of the current revision.
    pub fn duplicate(&self) -> R<PdfDoc> {
        let lo = self.with_lo(|d| Ok(d.clone()))?;
        Ok(PdfDoc::from_document(lo))
    }
}

/// A real number object, as an integer when it is one.
pub fn real(x: f64) -> Object {
    if x.fract() == 0.0 && x.abs() < 1e9 {
        Object::Integer(x as i64)
    } else {
        Object::Real(x as f32)
    }
}

/// A blank document: `pages` pages of `w` x `h` points.
pub fn new_document(w: f64, h: f64, pages: usize) -> Document {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let mut kids = Vec::with_capacity(pages);
    for _ in 0..pages {
        let page = blank_page(&mut doc, pages_id, w, h);
        kids.push(Object::Reference(page));
    }
    let count = kids.len() as i64;
    doc.objects.insert(pages_id, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count }));
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let info = doc.add_object(dictionary! { "Producer" => text_string("AnyPage") });
    doc.trailer.set("Info", info);
    doc
}

pub fn blank_page(doc: &mut Document, parent: ObjectId, w: f64, h: f64) -> ObjectId {
    let content = doc.add_object(Stream::new(Dictionary::new(), Vec::new()));
    doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => parent,
        "MediaBox" => vec![0.into(), 0.into(), real(w), real(h)],
        "Contents" => content,
        "Resources" => Dictionary::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_documents_open_in_hayro() {
        let doc = PdfDoc::from_document(new_document(300.0, 200.0, 3));
        assert_eq!(doc.meta().page_count(), 3);
        assert!(!doc.has_current_snapshot());
        let s = doc.snapshot().unwrap();
        assert_eq!(s.page_count(), 3);
        assert_eq!(s.geoms[0].size(), (300.0, 200.0));
        assert_eq!(s.geoms, doc.meta().geoms);
        assert!(PdfDoc::open(Arc::new(b"not a pdf".to_vec()), "").is_err());
        assert!(page_index(0.0, 3).is_err() && page_index(3.5, 3).is_err() && page_index(3.0, 3) == Ok(2));
    }

    #[test]
    fn dates_read_like_hayro() {
        assert_eq!(pdf_date(b"D:20261024120000+08'00'").as_deref(), Some("2026-10-24T12:00:00+08:00"));
        assert_eq!(pdf_date(b"D:2023").as_deref(), Some("2023-01-01T00:00:00+00:00"));
        assert_eq!(pdf_date(b"D:20231225083015Z").as_deref(), Some("2023-12-25T08:30:15+00:00"));
        assert_eq!(pdf_date(b"D:19991231235959-05'30").as_deref(), Some("1999-12-31T23:59:59-05:30"));
        assert_eq!(pdf_date(b"20231225"), None);
    }
}
