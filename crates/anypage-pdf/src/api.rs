//! The `pdf` module as page scripts see it: functions, jobs and the tile source.

use std::sync::Arc;

use anypage_engine::Image;
use anypage_runtime::{Call, Data, Effect, JobCtx, Natives, Res};

use crate::doc::{new_document, PdfDoc, KIND};
use crate::edit::{self, ImageSrc, NoteOpts, TextOpts, MAX_PAGE_SIDE, MAX_RECTS};
use crate::render::{color_from, rect_from, render_page, PdfTiles};
use crate::text::{Cell, Hit, PageText};
use crate::util::R;
use crate::{doc_arg, fmt_num, rect_data, round2};

pub const IMAGE_KIND: &str = "pdf.image";
/// What file dialogs and drops give; `Data::as_bytes` reads it.
pub const BYTES_KIND: &str = "fs.bytes";
const DEFAULT_HITS: usize = 1000;
const MAX_HITS: usize = 100_000;
/// Pages a whole-document `doc:text()` / `doc:search(q)` reads inline (on the
/// thread that runs the script); larger documents use the `pdf.search` job.
pub const INLINE_PAGES: usize = 32;
/// Glyphs such an inline call reads at most (a few crafted pages are enough
/// to take seconds).
pub const INLINE_GLYPHS: usize = 400_000;

fn too_big_inline(what: &str, why: String) -> String {
    format!(
        "{what} without a page reads every page on the script's thread, and {why}: pass a page ({{ page = n }} for search, doc:text(n)) or run the pdf.search job (page.job(name, \"pdf.search\", {{ doc, query }})), which is cancellable and reports progress"
    )
}

/// File bytes without copying when they are already shared.
fn bytes_arg(d: &Data) -> R<Arc<Vec<u8>>> {
    match d {
        Data::Bytes(b) => Ok(b.clone()),
        Data::Res(r) => r.downcast_arc::<Vec<u8>>().ok_or_else(|| format!("expected PDF bytes, got a {} resource", r.kind())),
        Data::Str(s) => Ok(Arc::new(s.as_bytes().to_vec())),
        Data::Nil => Err("expected PDF bytes (from a file dialog, a drop or a download), got nil".into()),
        other => Err(format!("expected PDF bytes, got {other:?}")),
    }
}

fn res_arg(d: &Data) -> R<&Res> {
    d.as_res().filter(|r| r.kind() == KIND).ok_or_else(|| "expected a PDF document (from pdf.open or pdf.new)".into())
}

/// The handle for the document's current revision, so every cell that read
/// an older one recomputes.
fn bump(r: &Res, revision: u64) -> Data {
    let mut r = r.clone();
    while r.version() < revision {
        r = r.next_version();
    }
    Data::Res(r)
}

fn new_doc(doc: PdfDoc) -> Data {
    Data::Res(Res::from_arc(KIND, Arc::new(doc)))
}

fn open(bytes: Arc<Vec<u8>>, opts: &Data) -> R<Data> {
    let password = opts.get("password").str_or("");
    let doc = crate::guard("opening the PDF", || PdfDoc::open(bytes, password))?;
    Ok(new_doc(doc))
}

fn snapshot(d: &Data) -> R<(Arc<PdfDoc>, Arc<crate::doc::Snapshot>)> {
    let doc = doc_arg(d)?;
    let s = doc.snapshot()?;
    Ok((doc, s))
}

fn page_text(d: &Data, page: &Data) -> R<(Arc<crate::doc::Snapshot>, usize, Arc<PageText>)> {
    let (_, s) = snapshot(d)?;
    let i = s.index(page.num_or(1.0))?;
    let t = s.text(i)?;
    Ok((s, i, t))
}

fn word_data(w: &crate::text::Word) -> Data {
    Data::map([
        ("text", Data::str(&w.text)),
        ("x", Data::Num(round2(w.rect[0] as f64))),
        ("y", Data::Num(round2(w.rect[1] as f64))),
        ("w", Data::Num(round2(w.rect[2] as f64))),
        ("h", Data::Num(round2(w.rect[3] as f64))),
    ])
}

fn hit_data(h: &Hit) -> Data {
    Data::map([
        ("page", Data::Num(h.page as f64)),
        ("text", Data::str(&h.text)),
        ("rects", Data::list(h.rects.iter().map(|r| rect_data(*r)).collect())),
        ("context", Data::str(&h.context)),
    ])
}

fn cell_data(c: &Cell) -> Data {
    match c {
        Cell::Empty => Data::Nil,
        Cell::Num(n) => Data::Num(*n),
        Cell::Str(s) => Data::str(s),
    }
}

fn extract_data(t: &PageText, page: usize) -> Data {
    let blocks: Vec<Data> = t
        .blocks
        .iter()
        .map(|b| {
            let size = b.lines.iter().map(|&l| t.lines[l].size).fold(0.0f32, f32::max);
            Data::map([
                ("x", Data::Num(round2(b.rect[0] as f64))),
                ("y", Data::Num(round2(b.rect[1] as f64))),
                ("w", Data::Num(round2(b.rect[2] as f64))),
                ("h", Data::Num(round2(b.rect[3] as f64))),
                ("text", Data::str(&t.block_text(b))),
                ("size", Data::Num(round2(size as f64))),
                ("lines", Data::Num(b.lines.len() as f64)),
            ])
        })
        .collect();
    let tables: Vec<Data> = t
        .tables
        .iter()
        .map(|tb| {
            Data::map([
                ("columns", Data::list(tb.columns.iter().map(|c| Data::str(c)).collect())),
                ("rows", Data::list(tb.rows.iter().map(|r| Data::list(r.iter().map(cell_data).collect())).collect())),
                ("rect", rect_data(tb.rect)),
            ])
        })
        .collect();
    Data::map([
        ("page", Data::Num(page as f64 + 1.0)),
        ("width", Data::Num(round2(t.width as f64))),
        ("height", Data::Num(round2(t.height as f64))),
        ("blocks", Data::list(blocks)),
        ("tables", Data::list(tables)),
    ])
}

struct SearchOpts {
    case: bool,
    page: Option<f64>,
    limit: usize,
}

impl SearchOpts {
    fn from(d: &Data) -> SearchOpts {
        SearchOpts {
            case: d.get("case").truthy(),
            page: d.get("page").num(),
            limit: d.get("limit").num().map(|n| n.clamp(1.0, MAX_HITS as f64) as usize).unwrap_or(DEFAULT_HITS),
        }
    }
}

/// Inline whole-document reads stay within [`INLINE_PAGES`] and [`INLINE_GLYPHS`].
struct Inline<'a> {
    what: &'a str,
    glyphs: usize,
}

impl Inline<'_> {
    fn pages(&self, n: usize) -> R<()> {
        if n > INLINE_PAGES {
            return Err(too_big_inline(self.what, format!("this document has {n} pages (at most {INLINE_PAGES} are read inline)")));
        }
        Ok(())
    }
    fn add(&mut self, t: &PageText) -> R<()> {
        self.glyphs += t.glyphs.len();
        if self.glyphs > INLINE_GLYPHS {
            return Err(too_big_inline(self.what, format!("its pages hold more than {INLINE_GLYPHS} glyphs")));
        }
        Ok(())
    }
}

/// Search pages one by one. `inline` bounds a search that runs on the
/// script's thread; a job passes `None` and its cancel and progress.
fn search(doc: &Data, query: &str, o: &SearchOpts, mut inline: Option<Inline>, cancel: &dyn Fn() -> bool, progress: &dyn Fn(f32)) -> R<Vec<Hit>> {
    if query.len() > 4096 {
        return Err("query longer than 4096 bytes".into());
    }
    let (_, s) = snapshot(doc)?;
    let pages: Vec<usize> = match o.page {
        Some(p) => vec![s.index(p)?],
        None => (0..s.page_count()).collect(),
    };
    if let (Some(b), None) = (&inline, o.page) {
        b.pages(pages.len())?;
    }
    let mut hits = Vec::new();
    let mut done = 0usize;
    s.text_each(&pages, cancel, |i, t| {
        if let (Some(b), None) = (&mut inline, o.page) {
            b.add(&t)?;
        }
        t.search(i + 1, query, o.case, o.limit, &mut hits);
        done += 1;
        progress(done as f32 / pages.len() as f32);
        Ok(hits.len() < o.limit)
    })?;
    Ok(hits)
}

fn metadata(d: &Data) -> R<Data> {
    let m = doc_arg(d)?.meta();
    let i = &m.info;
    let s = |v: &Option<String>| v.as_deref().map(Data::str).unwrap_or(Data::Nil);
    Ok(Data::map([
        ("title", s(&i.title)),
        ("author", s(&i.author)),
        ("subject", s(&i.subject)),
        ("keywords", s(&i.keywords)),
        ("creator", s(&i.creator)),
        ("producer", s(&i.producer)),
        ("created", s(&i.created)),
        ("modified", s(&i.modified)),
        ("pages", Data::Num(m.page_count() as f64)),
        ("version", Data::str(&i.version)),
        ("bytes", i.bytes.map_or(Data::Nil, |b| Data::Num(b as f64))),
    ]))
}

/// Rects for `highlight`: one rect, a list of rects, or search hits.
fn rects_arg(d: &Data) -> R<Vec<[f64; 4]>> {
    let mut out = Vec::new();
    let mut push = |r: &Data| -> R<()> {
        let v: Vec<f64> = match r {
            Data::Map(_) if !r.get("rects").is_nil() => {
                for x in r.get("rects").items() {
                    let n: Vec<f64> = x.items().iter().filter_map(Data::num).collect();
                    if n.len() == 4 {
                        out.push([n[0], n[1], n[2], n[3]]);
                    }
                }
                return Ok(());
            }
            Data::Map(_) => ["x", "y", "w", "h"].iter().filter_map(|k| r.get(k).num()).collect(),
            _ => r.items().iter().filter_map(Data::num).collect(),
        };
        if v.len() != 4 || v.iter().any(|x| !x.is_finite() || x.abs() > edit::MAX_COORD) || v[2] < 0.0 || v[3] < 0.0 {
            return Err(format!("rects must be {{ x, y, w, h }} of finite numbers within {} points (or a list of them, or search hits)", edit::MAX_COORD));
        }
        out.push([v[0], v[1], v[2], v[3]]);
        Ok(())
    };
    match d {
        Data::List(l) if l.first().is_some_and(|f| matches!(f, Data::List(_) | Data::Map(_))) => {
            for r in l.iter() {
                push(r)?;
            }
        }
        _ => push(d)?,
    }
    if out.len() > MAX_RECTS {
        return Err(format!("at most {MAX_RECTS} rects"));
    }
    Ok(out)
}

/// Run an edit and hand back the next version of the handle.
fn edit(c: &Call, f: impl FnOnce(&mut lopdf::Document, &mut edit::EditCx) -> R<()>) -> R<Data> {
    let r = res_arg(c.arg(0))?;
    let doc = doc_arg(c.arg(0))?;
    let ((), rev) = crate::guard("editing", || doc.edit(f))?;
    Ok(bump(r, rev))
}

pub fn register(n: &mut Natives) {
    // ---------------------------------------------------------------- documents
    n.function("pdf.open", Effect::Pure, |c| open(bytes_arg(c.arg(0))?, c.arg(1)));
    n.function("pdf.new", Effect::Pure, |c| {
        let o = c.arg(0);
        let w = o.get("width").num_or(612.0);
        let h = o.get("height").num_or(792.0);
        let pages = o.get("pages").num_or(1.0);
        if !(1.0..=MAX_PAGE_SIDE).contains(&w) || !(1.0..=MAX_PAGE_SIDE).contains(&h) {
            return Err(format!("width and height must be 1..={MAX_PAGE_SIDE} points"));
        }
        if !(1.0..=10_000.0).contains(&pages) || pages.fract() != 0.0 {
            return Err("pages must be a whole number 1..=10000".into());
        }
        Ok(new_doc(PdfDoc::from_document(new_document(w, h, pages as usize))))
    });
    // geometry and info come from the document's current revision without
    // reading it back after an edit (that waits for the tiles and jobs)
    n.function("pdf.page_count", Effect::Pure, |c| Ok(Data::Num(doc_arg(c.arg(0))?.meta().page_count() as f64)));
    n.function("pdf.page_size", Effect::Pure, |c| {
        let g = doc_arg(c.arg(0))?.meta().geom(c.arg(1).num_or(1.0))?;
        let (w, h) = g.size();
        Ok(Data::map([("w", Data::Num(round2(w))), ("h", Data::Num(round2(h))), ("rotation", Data::Num(g.rotate as f64))]))
    });
    n.function("pdf.metadata", Effect::Pure, |c| metadata(c.arg(0)));
    n.function("pdf.outline", Effect::Pure, |c| {
        let doc = doc_arg(c.arg(0))?;
        let items = doc.with_lo(|d| Ok(edit::outline(d)))?;
        Ok(Data::list(
            items
                .into_iter()
                .map(|(t, p, l)| Data::map([("title", Data::str(&t)), ("page", p.map_or(Data::Nil, |p| Data::Num(p as f64 + 1.0))), ("level", Data::Num(l as f64))]))
                .collect(),
        ))
    });
    n.function("pdf.save", Effect::Pure, |c| {
        let doc = doc_arg(c.arg(0))?;
        let b = crate::guard("saving", || doc.save())?;
        // a byte resource, not a Luau buffer: a large file is never copied
        // into the script's memory quota; save_file and pdf.open take it as is
        Ok(Data::Res(Res::from_arc(BYTES_KIND, b)))
    });
    n.function("pdf.copy_pages", Effect::Pure, |c| {
        let doc = doc_arg(c.arg(0))?;
        let pages: Vec<f64> = match c.arg(1) {
            Data::Num(p) => vec![*p],
            d => d.items().iter().filter_map(Data::num).collect(),
        };
        let lo = crate::guard("copying pages", || doc.with_lo(|d| edit::copy_pages(d, &pages)))?;
        Ok(new_doc(PdfDoc::from_document(lo)))
    });

    // ---------------------------------------------------------------- text
    n.function("pdf.text", Effect::Pure, |c| {
        if c.arg(1).is_nil() {
            let (_, s) = snapshot(c.arg(0))?;
            let mut inline = Inline { what: "doc:text()", glyphs: 0 };
            inline.pages(s.page_count())?;
            let mut parts = Vec::with_capacity(s.page_count());
            let pages: Vec<usize> = (0..s.page_count()).collect();
            s.text_each(&pages, &|| false, |_, t| {
                inline.add(&t)?;
                parts.push(t.plain());
                Ok(true)
            })?;
            return Ok(Data::str(&parts.join("\n\n")));
        }
        let (_, _, t) = page_text(c.arg(0), c.arg(1))?;
        Ok(Data::str(&t.plain()))
    });
    n.function("pdf.words", Effect::Pure, |c| {
        let (_, _, t) = page_text(c.arg(0), c.arg(1))?;
        Ok(Data::list(t.words().map(word_data).collect()))
    });
    n.function("pdf.search", Effect::Pure, |c| {
        let q = c.arg(1).as_str().ok_or("search needs a query string")?;
        let inline = Inline { what: "doc:search(query)", glyphs: 0 };
        let hits = search(c.arg(0), q, &SearchOpts::from(c.arg(2)), Some(inline), &|| false, &|_| {})?;
        Ok(Data::list(hits.iter().map(hit_data).collect()))
    });
    n.function("pdf.extract", Effect::Pure, |c| {
        let (_, i, t) = page_text(c.arg(0), c.arg(1))?;
        Ok(extract_data(&t, i))
    });
    n.function("pdf.measure_text", Effect::Pure, |c| {
        let text = match c.arg(0) {
            Data::Str(s) => s.to_string(),
            Data::Num(x) => fmt_num(*x),
            _ => return Err("measure_text needs a string".into()),
        };
        let mut o = c.arg(1).clone();
        if let Data::Map(m) = &mut o {
            Arc::make_mut(m).insert(Arc::from("text"), Data::str(&text));
        } else {
            o = Data::map([("text", Data::str(&text))]);
        }
        let t = TextOpts::from_data(&o)?;
        let lines = t.lines();
        let e = t.extent_of(&lines);
        Ok(Data::map([("w", Data::Num(round2(e[2]))), ("h", Data::Num(round2(e[3]))), ("lines", Data::Num(lines.len() as f64))]))
    });

    // ---------------------------------------------------------------- structure
    n.function("pdf.annotations", Effect::Pure, |c| {
        let doc = doc_arg(c.arg(0))?;
        let page = c.arg(1).num_or(1.0);
        doc.with_lo(|d| Ok(Data::list(edit::annotations(d, edit::page_id(d, page)?))))
    });
    n.function("pdf.form_fields", Effect::Pure, |c| {
        let doc = doc_arg(c.arg(0))?;
        doc.with_lo(|d| Ok(Data::list(edit::fields_data(d))))
    });

    // ---------------------------------------------------------------- edits
    n.function("pdf.add_text", Effect::Effect, |c| {
        let page = c.arg(1).num_or(1.0);
        let o = TextOpts::from_data(c.arg(2))?;
        edit(c, |d, cx| {
            let pid = edit::page_id(d, page)?;
            edit::add_text(d, cx, pid, &o)
        })
    });
    n.function("pdf.add_path", Effect::Effect, |c| {
        let page = c.arg(1).num_or(1.0);
        let shapes = edit::shapes(c.arg(2), c.arg(3))?;
        edit(c, |d, cx| {
            let pid = edit::page_id(d, page)?;
            edit::add_path(d, cx, pid, &shapes)
        })
    });
    n.function("pdf.add_image", Effect::Effect, |c| {
        let page = c.arg(1).num_or(1.0);
        let src = ImageSrc::from_data(c.arg(2))?;
        let r = rect_from(c.arg(3)).ok_or("add_image needs a rect { x, y, w, h }")?;
        let r = [r.x as f64, r.y as f64, r.w as f64, r.h as f64];
        if r.iter().any(|v| v.abs() > edit::MAX_COORD) {
            return Err(format!("the image rect must lie within {} points of the page origin", edit::MAX_COORD));
        }
        edit(c, |d, cx| {
            let pid = edit::page_id(d, page)?;
            edit::add_image(d, cx, pid, src, r)
        })
    });
    n.function("pdf.highlight", Effect::Effect, |c| {
        let page = c.arg(1).num_or(1.0);
        let rects = rects_arg(c.arg(2))?;
        let color = c.arg(3).num().unwrap_or(0xFFE14D99u32 as f64);
        let note = c.arg(4).get("contents").as_str().map(str::to_string);
        edit(c, |d, _| {
            let pid = edit::page_id(d, page)?;
            edit::highlight(d, pid, &rects, color, note.as_deref())
        })
    });
    n.function("pdf.add_note", Effect::Effect, |c| {
        let page = c.arg(1).num_or(1.0);
        let o = c.arg(2);
        let note = NoteOpts {
            x: edit::coord(o, "x", 72.0)?,
            y: edit::coord(o, "y", 72.0)?,
            text: o.get("text").as_str().ok_or("add_note needs { text = \"...\" }")?.to_string(),
            title: o.get("title").as_str().map(str::to_string),
            color: o.get("color").num().unwrap_or(0xFFD84AFFu32 as f64),
        };
        edit(c, |d, _| {
            let pid = edit::page_id(d, page)?;
            edit::add_note(d, pid, &note)
        })
    });
    n.function("pdf.fill_form", Effect::Effect, |c| {
        let values: Vec<(String, Data)> = c.arg(1).entries().map(|(k, v)| (k.to_string(), v.clone())).collect();
        if values.is_empty() {
            return Err("fill_form needs { field_name = value, ... }".into());
        }
        edit(c, |d, cx| edit::fill_form(d, cx, &values).map(|_| ()))
    });
    n.function("pdf.delete_page", Effect::Effect, |c| {
        let page = c.arg(1).num_or(f64::NAN);
        edit(c, |d, _| edit::delete_page(d, page))
    });
    n.function("pdf.insert_page", Effect::Effect, |c| {
        let at = c.arg(1).num_or(f64::NAN);
        let o = c.arg(2);
        let size = match (o.get("width").num(), o.get("height").num()) {
            (Some(w), Some(h)) => Some((w, h)),
            (None, None) => None,
            _ => return Err("give both width and height, or neither".into()),
        };
        edit(c, |d, _| edit::insert_page(d, at, size))
    });
    n.function("pdf.rotate_page", Effect::Effect, |c| {
        let page = c.arg(1).num_or(f64::NAN);
        let deg = c.arg(2).num_or(90.0);
        edit(c, |d, _| edit::rotate_page(d, page, deg))
    });
    n.function("pdf.merge", Effect::Effect, |c| {
        let other = doc_arg(c.arg(1))?;
        let src = other.with_lo(|d| Ok(d.clone()))?;
        edit(c, |d, _| edit::merge(d, &src).map(|_| ()))
    });

    // ---------------------------------------------------------------- jobs
    n.job_fn("pdf.load", |j: &JobCtx| {
        let doc = open(bytes_arg(j.input(0))?, j.opts)?;
        j.progress(0.4);
        let d = doc_arg(&doc)?;
        // with { editable = true } the structure edits need is parsed here, off
        // the UI thread, instead of in the event handler of the first edit
        if j.opts.get("editable").truthy() && !j.cancelled() {
            crate::guard("loading the structure", || d.with_lo(|_| Ok(())))?;
        }
        j.progress(0.8);
        // the first page's text is ready when the job is
        if let Ok(s) = d.snapshot() {
            if s.page_count() > 0 && !j.cancelled() {
                let _ = s.text_with(0, &|| j.cancelled());
            }
        }
        Ok(doc)
    });
    n.job_fn("pdf.render", |j: &JobCtx| {
        let (_, s) = snapshot(j.input(0))?;
        let i = s.index(j.input(1).num_or(1.0))?;
        let width = j.input(2).num().or(j.opts.get("width_px").num()).unwrap_or(1024.0);
        let paper = color_from(j.opts.get("background"), [255, 255, 255, 255]);
        let img = render_page(&s, i, width, paper, &|| j.cancelled(), &|p| j.progress(p))?.ok_or("cancelled")?;
        Ok(Data::Res(Res::new(IMAGE_KIND, img)))
    });
    n.job_fn("pdf.search", |j: &JobCtx| {
        let q = j.input(1).as_str().ok_or("pdf.search needs a query string input")?;
        let opts = if j.input(2).is_nil() { j.opts } else { j.input(2) };
        let hits = search(j.input(0), q, &SearchOpts::from(opts), None, &|| j.cancelled(), &|p| j.progress(p))?;
        Ok(Data::list(hits.iter().map(hit_data).collect()))
    });
    n.job_fn("pdf.extract", |j: &JobCtx| {
        let (_, s) = snapshot(j.input(0))?;
        let i = s.index(j.input(1).num_or(1.0))?;
        let t = s.text_with(i, &|| j.cancelled())?;
        Ok(extract_data(&t, i))
    });

    // ---------------------------------------------------------------- tiles
    n.tile("pdf.page", |rect, _opts| Ok(Arc::new(PdfTiles { layer: rect }) as Arc<dyn anypage_engine::TileSource>));
}

/// An image value as the shared image format (for tests and hosts).
pub fn image_of(d: &Data) -> Option<&Image> {
    d.as_res()?.get::<Image>()
}
