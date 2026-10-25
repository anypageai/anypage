//! # The `pdf` module
//!
//! Open, render, read, search, extract, annotate, fill, edit and save PDF
//! files, in pure Rust on every platform (Windows, Android, and nothing in the
//! way of iOS or the web).
//!
//! ## Luau API
//!
//! A page declares `"capabilities": { "modules": ["pdf"] }`. Pages are
//! 1-based. Coordinates are *page space*: points, origin at the top-left
//! corner of the page as displayed (crop box and `/Rotate` applied), y down.
//! Rects are `{ x, y, w, h }`; colours packed `0xRRGGBBAA`.
//!
//! | function | |
//! |---|---|
//! | `pdf.open(bytes[, {password}])` → doc | a `pdf.document` resource; `bytes` may be a `fs.bytes` resource |
//! | `pdf.new{width, height, pages}` → doc | blank pages (default Letter, 1 page) |
//! | `doc:page_count()`, `doc:page_size(i)` → `{w, h, rotation}` | never wait for an edited document to be read back |
//! | `doc:metadata()` | `{title, author, subject, keywords, creator, producer, created, modified, pages, version, bytes}`; `bytes` is the size of the file as opened, nil once edited |
//! | `doc:outline()` | `{ {title, page, level} }` in document order |
//! | `doc:text([i])` | plain text of page `i` (all pages when omitted, for documents of at most 32 pages): lines `\n`, blocks `\n\n` |
//! | `doc:words(i)` | `{ {text, x, y, w, h} }` in reading order |
//! | `doc:search(query, {case, page, limit})` | `{ {page, text, rects = {rect...}, context} }`; whitespace-insensitive, case-insensitive unless `case`. Without `page` it reads every page inline, so it is refused for documents over 32 pages (or 400k glyphs): use the `pdf.search` job |
//! | `doc:extract(i)` | `{ page, width, height, blocks = { {x, y, w, h, text, size, lines} }, tables = { {columns, rows, rect} } }` |
//! | `doc:annotations(i)` | `{ {type, rect, contents, title, color, field} }` |
//! | `doc:form_fields()` | `{ {name, type, value, readonly, required, widgets = { {page, x, y, w, h} }, options, states, multiline} }` |
//! | `pdf.measure_text(text, {size, font, bold, italic, width, line_height})` → `{w, h, lines}` | linear in the text; `width` at most 14400 pt |
//! | `doc:save()` → bytes | the current revision as a PDF file: an `fs.bytes` resource (never copied into the script VM), accepted by `page.save_file` and `pdf.open` |
//! | `doc:copy_pages({i, ...})` → doc | a new document with those pages (any order) |
//!
//! Edits are effects (load time and event handlers only). Each returns the
//! next version of the document handle (`Res::next_version`), so cells that
//! read the document recompute; set it back into the page's input cell. An
//! edit that fails (a missing form field, a bad value) changes nothing and
//! keeps the version: inputs are checked before the document is touched.
//! Coordinates must be finite (`0/0` and `1/0` are errors, not 0).
//!
//! | edit | |
//! |---|---|
//! | `pdf.add_text(doc, i, {x, y, text, size, color, font, bold, italic, width, line_height, align, lang})` | `y` is the top of the first line; baseline at `y + 0.8 size`; wraps at `width`; Latin in the standard fonts (`Helvetica`/`sans`, `Times-Roman`/`serif`, `Courier`/`mono`), CJK through Adobe's CJK collections (`lang` = `zh-Hant`, `zh-Hans`, `ja`, `ko`) |
//! | `pdf.add_path(doc, i, path, {stroke, fill, width, cap, join, dash, even_odd})` | the shared vector path form, or `{path, stroke, ...}`, or a list of those (what `cad` `to_paths` returns) |
//! | `pdf.add_image(doc, i, image, rect)` | an image value (`*.image` resource), or PNG / JPEG bytes (JPEG is embedded as is) |
//! | `pdf.highlight(doc, i, rects, color[, {contents}])` | a Highlight annotation (multiply blend, default `0xFFE14D99`); `rects` may be one rect, a list, or search hits |
//! | `pdf.add_note(doc, i, {x, y, text, title, color})` | a Text (sticky note) annotation, 20 pt icon at `x, y` |
//! | `pdf.fill_form(doc, {name = value})` | text, choice, check box, radio; appearances regenerated; all or nothing |
//! | `pdf.delete_page(doc, i)`, `pdf.insert_page(doc, at, {width, height})`, `pdf.rotate_page(doc, i, degrees)` | deleting a page also removes its annotations and form fields, and nulls links and outline entries that pointed at it |
//! | `pdf.merge(a, b)` | appends `b`'s pages (and form fields) to `a` |
//!
//! | job / tiles | |
//! |---|---|
//! | job `pdf.load` | inputs `{bytes}`, opts `{password, editable}` → doc, parsed off the UI thread (`editable` also parses the structure edits need) |
//! | job `pdf.render` | inputs `{doc, page, width_px}`, opts `{background}` → `pdf.image` (premultiplied RGBA) |
//! | job `pdf.search` | inputs `{doc, query[, opts]}` → hits, page by page, cancellable (also inside a page) with progress |
//! | job `pdf.extract` | inputs `{doc, page}` → as `doc:extract`, cancellable |
//! | tiles `pdf.page` | params `{doc, page, rect, background}`: `rect` is the world rect the page fills (default: the tiled layer's rect, which should have the page's aspect); `doc = nil` (still loading) draws nothing |
//!
//! ## Library decision (evaluated October 2026 from their sources)
//!
//! * **hayro 0.8** (+ hayro-syntax, hayro-interpret, vello_cpu; MIT/Apache,
//!   `forbid(unsafe_code)`): renders. Its interpreter is complete enough to be
//!   the one content-stream engine for both pixels and text: text state and
//!   matrices, `TJ` adjustments, simple-font encodings with `/Differences` and
//!   the Adobe Glyph List, the standard 14 metrics, `ToUnicode`, CID fonts with
//!   the 61 predefined CMaps (embedded), Type 3, form XObjects, optional
//!   content, annotation appearances, and invisible text (OCR layers). A custom
//!   [`hayro::hayro_interpret::Device`] gives every glyph with its transform,
//!   advance and Unicode, so text extraction here is built on it rather than
//!   on a second, weaker interpreter (lopdf's `extract_text` has no positions).
//!   `render_into` takes any affine, which is what tiles need: each 256 px tile
//!   is rasterised at its own scale, so vectors are crisp at every zoom. hayro
//!   reads the file lazily (xref first), so opening is fast for large files.
//!   Limits it documents: no blend isolation/knockout groups, partial
//!   annotation support. `Pdf` is `Send + Sync`; its render cache is not, so
//!   each tile gets a fresh cache.
//! * **lopdf 0.46** (MIT): structure and editing: the object model, page tree,
//!   writing, object streams, decryption, and a lenient reader that can repair
//!   files hayro rejects. Missing pieces written here: page-space edits on
//!   rotated/cropped pages, fonts for written text (standard 14 with widths
//!   probed from hayro's AFM tables, CJK Type 0 fonts over Adobe's
//!   collections), images (PNG decode, JPEG passthrough, soft masks),
//!   highlight and note annotations with appearance streams, AcroForm reading
//!   and filling with regenerated appearances, page insertion/deletion
//!   (lopdf's `delete_pages` leaves dangling `/Kids`), page import with
//!   inherited attributes made explicit (merge, copy), outlines with named
//!   destinations. Its `delete_pages` is not used.
//! * Both views are kept in step by a revision number: an edit changes the
//!   lopdf document and updates the page geometry and info at once; the first
//!   reader that needs pixels or text (a tile worker or a job, usually)
//!   re-serialises it and hands hayro the bytes (see [`doc`]). lopdf's
//!   `IncrementalDocument` was not used: it needs to know every object an
//!   edit touched, and a whole-file write measured 125-175 ms on a 57 MB file.
//! * hayro has no output cap on Flate/LZW decoding, so files are checked for
//!   decompression bombs before either library reads them ([`limits`]).
//! * PDFium stays an archived prototype (`docs/archive`); not needed.
//!
//! ## Costs and limits
//!
//! | | |
//! |---|---|
//! | file size | 512 MB (`pdf.open`); every stream at most 256 MB decoded, images at most 256 Mpx declared ([`limits`]). The check decodes only streams over ~240 KB compressed, at open: measured 114 ms for an 11 MB stream of 26 MB content, so open big files with the `pdf.load` job |
//! | text of a page | interpretation by hayro, then a layout pass that is near-linear in the glyphs with work budgets for blocks, reading order and tables; at most 250k glyphs a page. Measured (debug build with optimised dependencies, idle machine; 2-5x slower while other builds ran): a dense page of 4.6k glyphs 4 ms + 1 ms; 50k scattered glyphs 48 ms + 114 ms; 250k crafted glyphs 263 ms + 345 ms and ~66 MB while it lives |
//! | text cache | per document revision: 64 pages and 32 MB, least recently used out first; whole-document passes only fill free room |
//! | inline whole-document `doc:text()` / `doc:search(q)` | at most 32 pages and 400k glyphs, else an error pointing to the `pdf.search` job |
//! | written text | 1 MB per call; wrapping and measuring are linear (1 MB wraps in 20-70 ms) |
//! | rendering | 16384 px a side, 48 Mpx per `pdf.render`; tiles 256 px |
//!
//! ## Rust API for hosts
//!
//! [`register`] adds the module; [`set_cjk_font`] hands it the bytes of a CJK
//! font the host already has (otherwise a platform font is looked up once,
//! only when a document needs one: `msjh.ttc` and friends on Windows, Noto CJK
//! or DroidSansFallback on Android). [`PdfDoc`], [`text`], [`render`] and
//! [`edit`] are public for hosts and tests.
//!
//! ## Known limits
//!
//! * Written CJK text uses non-embedded Adobe CJK fonts (small files, standard
//!   practice; readers substitute their own CJK font), full width only.
//!   Scripts other than Latin-1 and CJK are not shaped: characters outside
//!   WinAnsi and the CJK collections are written as `?`.
//! * Tables are detected from aligned text only (ruling lines are not used);
//!   equations come out as many small blocks.
//! * Text extraction orders right-to-left scripts visually and treats vertical
//!   writing glyph by glyph.
//! * Saving re-serialises the whole document (no incremental update yet), and
//!   so does the first read after an edit.
//! * The decompression check cannot see into encrypted files or streams whose
//!   `/Filter` is an indirect object without a zlib header; hayro's
//!   interpreter cannot be interrupted, so a cancelled text job still lets
//!   hayro finish the page (it stops collecting glyphs).

pub mod api;
pub mod doc;
pub mod edit;
pub mod fonts;
pub mod geom;
pub mod limits;
pub mod render;
pub mod text;
pub mod util;

use std::sync::Arc;

use anypage_runtime::{Data, Natives};

pub use doc::{PdfDoc, Snapshot};
pub use render::{set_cjk_font, PdfTiles};
pub use util::R;

/// Add the `pdf` module's functions, jobs and tile source.
pub fn register(n: &mut Natives) {
    api::register(n);
}

/// Run library code that might panic on a malformed file, turning a panic
/// into an error instead of unwinding across the native boundary.
pub(crate) fn guard<T>(what: &str, f: impl FnOnce() -> R<T>) -> R<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            Err(format!("{what} failed on this file (internal error: {msg})"))
        }
    }
}

/// The document behind a `pdf.document` value.
pub fn doc_arg(d: &Data) -> R<Arc<PdfDoc>> {
    match d {
        Data::Res(r) if r.kind() == doc::KIND => r.downcast_arc::<PdfDoc>().ok_or_else(|| "internal: not a PDF document".into()),
        Data::Res(r) => Err(format!("expected a PDF document, got a {} resource", r.kind())),
        Data::Nil => Err("expected a PDF document (from pdf.open or pdf.new), got nil".into()),
        other => Err(format!("expected a PDF document, got {other:?}")),
    }
}

pub(crate) fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// `[x, y, w, h]` as the shared rect form, rounded to 1/100 pt.
pub(crate) fn rect_data(r: [f32; 4]) -> Data {
    Data::nums(r.iter().map(|v| round2(*v as f64)))
}

pub(crate) fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}
