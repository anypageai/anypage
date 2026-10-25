//! The `sheet` module as page scripts see it: argument checking, version
//! publishing and the panic guard around every native call.

use std::panic::{catch_unwind, AssertUnwindSafe};

use anypage_runtime::{Call, Data, Effect, JobCtx, Natives, Res};

use crate::book::{Book, BookOptions, Workbook, MAX_INLINE_CELLS, MAX_READ_CELLS};
use crate::csvio::{CsvIn, CsvOut};
use crate::refs::{self, Span, MAX_COL, MAX_ROW};
use crate::xlsx::XlsxLimits;

/// Resource kind of a workbook.
pub const KIND: &str = "sheet.workbook";

/// Run `f`, turning a panic inside IronCalc (or here) into an error string
/// so nothing unwinds across the native boundary.
pub fn guard(f: impl FnOnce() -> Result<Data, String>) -> Result<Data, String> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown".into());
            Err(format!("internal spreadsheet error: {msg}"))
        }
    }
}

/// The same resource at the workbook's current version. Versions only grow,
/// so a stale handle passed to an edit still publishes a new value.
pub fn publish(r: &Res, edits: u64) -> Data {
    let mut r = r.clone();
    while r.version() < edits {
        r = r.next_version();
    }
    Data::Res(r)
}

fn describe_arg(d: &Data) -> &'static str {
    match d {
        Data::Nil => "nil",
        Data::Bool(_) => "a boolean",
        Data::Num(_) => "a number",
        Data::Str(_) => "a string",
        Data::Bytes(_) => "bytes",
        Data::List(_) | Data::Map(_) => "a table",
        Data::Res(_) => "another resource",
    }
}

fn book_of<'a>(d: &'a Data, what: &str) -> Result<(&'a Res, &'a Workbook), String> {
    let r = d
        .as_res()
        .filter(|r| r.kind() == KIND)
        .ok_or_else(|| format!("{what} must be a workbook (from sheet.open, sheet.new or sheet.from_csv), not {}", describe_arg(d)))?;
    let w = r.get::<Workbook>().ok_or("workbook resource holds something else")?;
    Ok((r, w))
}

fn book_arg<'a>(c: &'a Call, i: usize) -> Result<(&'a Res, &'a Workbook), String> {
    book_of(c.arg(i), &format!("argument {}", i + 1))
}

fn str_arg<'a>(c: &'a Call, i: usize, what: &str) -> Result<&'a str, String> {
    c.arg(i).as_str().ok_or_else(|| format!("argument {} ({what}) must be a string, not {}", i + 1, describe_arg(c.arg(i))))
}

/// A single cell from a reference string.
fn cell_of(b: &Book, text: &str) -> Result<(u32, i32, i32), String> {
    match b.resolve(text)? {
        (sheet, Span::Cell { row, col }) => Ok((sheet, row, col)),
        (sheet, Span::Area { r1, c1, r2, c2 }) if r1 == r2 && c1 == c2 => Ok((sheet, r1, c1)),
        _ => Err(format!("\"{text}\" is a range; this takes one cell (use range or set_range for areas)")),
    }
}

/// A count argument: a whole number from 1, default 1.
fn count_arg(d: &Data, max: i32) -> Result<i32, String> {
    match d {
        Data::Nil => Ok(1),
        Data::Num(n) if n.fract() == 0.0 && *n >= 1.0 && *n <= max as f64 => Ok(*n as i32),
        _ => Err(format!("count must be a whole number from 1 to {max}")),
    }
}

/// Rows (`rows = true`) or columns to insert or delete: either a reference
/// like "Sheet1!3:5" / "B:D", or (sheet, start, count).
fn lines_arg(b: &Book, c: &Call, rows: bool) -> Result<(u32, i32, i32), String> {
    if let Some(s) = c.arg(1).as_str() {
        if let Ok(r) = refs::parse(s) {
            let sheet = match &r.sheet {
                Some(n) => b.sheet_by_name(n)?,
                None => b.sheet_index(&Data::Nil)?,
            };
            match (rows, r.span) {
                (true, Span::Rows { r1, r2 }) => return Ok((sheet, r1, r2 - r1 + 1)),
                (false, Span::Cols { c1, c2 }) => return Ok((sheet, c1, c2 - c1 + 1)),
                _ => {}
            }
        }
    }
    let sheet = b.sheet_index(c.arg(1))?;
    let max = if rows { MAX_ROW } else { MAX_COL };
    let start = match c.arg(2) {
        Data::Num(n) if n.fract() == 0.0 && *n >= 1.0 && *n <= max as f64 => *n as i32,
        Data::Str(s) if !rows => refs::col_index(s).ok_or_else(|| format!("\"{s}\" is not a column"))?,
        other => {
            let what = if rows { "row number" } else { "column number or letters" };
            return Err(format!("argument 3 must be a {what}, not {}", describe_arg(other)));
        }
    };
    Ok((sheet, start, count_arg(c.arg(3), max)?))
}

/// The shared table form `{ columns, rows }`.
fn is_table(d: &Data) -> bool {
    matches!(d, Data::Map(_)) && !d.get("rows").is_nil()
}

/// Rows for `set_range`: a list of lists, one flat list (a single row), or
/// the shared table form `{ columns, rows }` (header row first unless
/// `header = false`).
fn rows_of(d: &Data, opts: &Data) -> Result<Vec<Vec<Data>>, String> {
    fn row_of(d: &Data) -> Result<Vec<Data>, String> {
        match d {
            Data::Nil => Ok(Vec::new()),
            Data::List(l) => Ok(l.to_vec()),
            // a Luau row with nil holes arrives as a map with numeric keys
            Data::Map(_) => {
                let mut out = Vec::new();
                for (k, v) in d.entries() {
                    let i: usize = k.parse().map_err(|_| format!("row key \"{k}\" is not a column number"))?;
                    if i == 0 || i > MAX_COL as usize {
                        return Err(format!("row key {i} is outside the sheet"));
                    }
                    if out.len() < i {
                        out.resize(i, Data::Nil);
                    }
                    out[i - 1] = v.clone();
                }
                Ok(out)
            }
            _ => Err("each row must be a list of values".into()),
        }
    }
    if is_table(d) {
        let mut out = Vec::new();
        let cols = d.get("columns");
        if !opts.get("header").as_bool().is_some_and(|h| !h) && !cols.items().is_empty() {
            out.push(cols.items().to_vec());
        }
        for r in d.get("rows").items() {
            out.push(row_of(r)?);
        }
        return Ok(out);
    }
    // {error=}, {text=}, {formula=}, {date=}: one cell value, not a row
    let value_map = |x: &Data| ["error", "text", "formula", "date"].iter().any(|k| !x.get(k).is_nil());
    match d {
        Data::List(l) if l.iter().all(|x| !matches!(x, Data::List(_) | Data::Map(_)) || value_map(x)) => Ok(vec![l.to_vec()]),
        Data::List(l) => l.iter().map(row_of).collect(),
        Data::Map(_) => Ok(d.entries().map(|(k, v)| (k.parse::<usize>(), v)).try_fold(Vec::new(), |mut acc: Vec<Vec<Data>>, (k, v)| {
            let i = k.map_err(|_| "rows must be a list of lists".to_string())?;
            if i == 0 || i > MAX_ROW as usize {
                return Err(format!("row {i} is outside the sheet"));
            }
            if acc.len() < i {
                acc.resize(i, Vec::new());
            }
            acc[i - 1] = row_of(v)?;
            Ok(acc)
        })?),
        other => Err(format!("set_range needs rows (a list of lists or a {{columns, rows}} table), not {}", describe_arg(other))),
    }
}

/// Run an edit on the locked workbook and publish the new version. Edits
/// check everything before they change anything, so an error leaves the
/// workbook as it was (the one exception, an IronCalc failure in the middle
/// of a paste, says so in its message).
fn edit(c: &Call, f: impl FnOnce(&mut Book, &Call) -> Result<(), String>) -> Result<Data, String> {
    let (r, w) = book_arg(c, 0)?;
    let mut b = w.lock()?;
    f(&mut b, c)?;
    Ok(publish(r, b.edits()))
}

/// Run a read on the locked workbook.
fn read(c: &Call, f: impl FnOnce(&Book, &Call) -> Result<Data, String>) -> Result<Data, String> {
    let (_, w) = book_arg(c, 0)?;
    let b = w.lock()?;
    f(&b, c)
}

fn bytes_of<'a>(d: &'a Data, what: &str) -> Result<&'a [u8], String> {
    match d {
        Data::Str(_) | Data::Bytes(_) | Data::Res(_) => d.as_bytes().ok_or_else(|| format!("{what} must be bytes (a file's data)")),
        other => Err(format!("{what} must be bytes (a file's data), not {}", describe_arg(other))),
    }
}

fn never() -> bool {
    false
}

fn ignore(_: f32) {}

/// Limits for files opened on the page's thread.
fn inline_limits() -> XlsxLimits {
    let mut l = XlsxLimits::default();
    l.max_cells = l.max_cells.min(MAX_INLINE_CELLS);
    l.max_formulas = l.max_formulas.min(MAX_INLINE_CELLS);
    l.max_merged = l.max_merged.min(MAX_INLINE_CELLS);
    l.max_array = l.max_array.min(MAX_INLINE_CELLS);
    l
}

const INLINE_HINT: &str = " (sheet.open and sheet.from_csv run on the page's thread; open bigger files with the sheet.load job)";

/// Open bytes as xlsx or CSV ("PK" zip signature means xlsx unless `format`
/// says otherwise). `inline` (on the page's thread) allows smaller files.
fn load(bytes: &[u8], opts: &Data, inline: bool, cancelled: &dyn Fn() -> bool, progress: &dyn Fn(f32)) -> Result<Workbook, String> {
    let o = BookOptions::from_data(opts);
    if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
        return Err("this is an old binary .xls (Excel 97-2003) file, which is not supported; save it as .xlsx".into());
    }
    let format = match opts.get("format").as_str() {
        Some(f) => f.to_ascii_lowercase(),
        None if bytes.starts_with(b"PK") => "xlsx".into(),
        None => "csv".into(),
    };
    match format.as_str() {
        "xlsx" => {
            let limits = if inline { inline_limits() } else { XlsxLimits::default() };
            Workbook::open_xlsx_with(bytes, &o, &limits, cancelled, progress)
                .map_err(|e| if inline && e.starts_with("xlsx refused: more than") { format!("{e}{INLINE_HINT}") } else { e })
        }
        "csv" | "tsv" | "txt" => {
            let text = std::str::from_utf8(bytes).map_err(|e| format!("CSV must be UTF-8 text: {e}"))?;
            let mut c = CsvIn::from_data(opts)?;
            if inline {
                c.max_cells = MAX_INLINE_CELLS as usize;
            }
            if format == "tsv" && c.delimiter.is_none() {
                c.delimiter = Some(b'\t');
            }
            Workbook::from_csv(text, &c, &o, cancelled, progress).map_err(|e| if inline && e.contains("more than") { format!("{e}{INLINE_HINT}") } else { e })
        }
        other => Err(format!("unknown format \"{other}\" (xlsx, csv, tsv)")),
    }
}

fn table_opts(d: &Data, header_default: bool) -> (bool, bool) {
    (d.get("header").as_bool().unwrap_or(header_default), d.get("text").truthy())
}

fn dims(b: &Book, sheet: u32) -> Result<Data, String> {
    let name = Data::str(&b.sheet_name(sheet)?);
    Ok(match b.used(sheet)? {
        Some((r1, c1, r2, c2)) => Data::map([
            ("name", name),
            ("rows", Data::Num(r2 as f64)),
            ("cols", Data::Num(c2 as f64)),
            ("first_row", Data::Num(r1 as f64)),
            ("first_col", Data::Num(c1 as f64)),
            ("last_row", Data::Num(r2 as f64)),
            ("last_col", Data::Num(c2 as f64)),
            ("used", Data::str(&refs::format_ref(None, &Span::Area { r1, c1, r2, c2 }))),
        ]),
        None => Data::map([("name", name), ("rows", Data::Num(0.0)), ("cols", Data::Num(0.0))]),
    })
}

fn empty_table() -> Data {
    Data::map([("columns", Data::list(Vec::new())), ("rows", Data::list(Vec::new()))])
}

/// Register the `sheet` module.
pub fn register(n: &mut Natives) {
    // ---------------------------------------------------------- create
    n.function("sheet.new", Effect::Pure, |c| guard(|| Ok(Data::res(KIND, Workbook::new(&BookOptions::from_data(c.arg(0)))?))));
    n.function("sheet.open", Effect::Pure, |c| {
        guard(|| {
            let bytes = bytes_of(c.arg(0), "sheet.open's first argument")?;
            Ok(Data::res(KIND, load(bytes, c.arg(1), true, &never, &ignore)?))
        })
    });
    n.function("sheet.from_csv", Effect::Pure, |c| {
        guard(|| {
            let text = str_arg(c, 0, "CSV text")?;
            let o = BookOptions::from_data(c.arg(1));
            let mut csv = CsvIn::from_data(c.arg(1))?;
            csv.max_cells = MAX_INLINE_CELLS as usize;
            let wb = Workbook::from_csv(text, &csv, &o, &never, &ignore).map_err(|e| if e.contains("more than") { format!("{e}{INLINE_HINT}") } else { e })?;
            Ok(Data::res(KIND, wb))
        })
    });
    // page.job(name, "sheet.load", {bytes_cell}, {format, ..., into = wb})
    n.job_fn("sheet.load", |j: &JobCtx| {
        guard(|| {
            if j.input(0).is_nil() {
                // no file yet
                return Ok(Data::Nil);
            }
            let bytes = bytes_of(j.input(0), "sheet.load's input")?;
            let wb = load(bytes, j.opts, false, &|| j.cancelled(), &|p| j.progress(p))?;
            let into = j.opts.get("into");
            if into.is_nil() {
                return Ok(Data::res(KIND, wb));
            }
            // replace the content of a workbook the page already holds, so
            // its handlers (which read only input cells) edit the new file
            let (r, target) = book_of(into, "sheet.load's into option")?;
            if j.cancelled() {
                return Err("cancelled".into());
            }
            let edits = target.replace_with(wb)?;
            Ok(publish(r, edits))
        })
    });

    // ---------------------------------------------------------- read
    n.function("sheet.sheets", Effect::Pure, |c| guard(|| read(c, |b, _| Ok(Data::list(b.sheet_names().iter().map(|s| Data::str(s)).collect())))));
    n.function("sheet.name", Effect::Pure, |c| guard(|| read(c, |b, _| Ok(Data::str(&b.name())))));
    n.function("sheet.dims", Effect::Pure, |c| guard(|| read(c, |b, c| dims(b, b.sheet_index(c.arg(1))?))));
    n.function("sheet.get", Effect::Pure, |c| {
        guard(|| {
            read(c, |b, c| {
                let (s, r, col) = cell_of(b, str_arg(c, 1, "reference")?)?;
                b.value(s, r, col)
            })
        })
    });
    n.function("sheet.text", Effect::Pure, |c| {
        guard(|| {
            read(c, |b, c| {
                let (s, r, col) = cell_of(b, str_arg(c, 1, "reference")?)?;
                Ok(Data::str(&b.text(s, r, col)?))
            })
        })
    });
    n.function("sheet.formula", Effect::Pure, |c| {
        guard(|| {
            read(c, |b, c| {
                let (s, r, col) = cell_of(b, str_arg(c, 1, "reference")?)?;
                Ok(b.formula(s, r, col)?.map(|f| Data::str(&f)).unwrap_or_default())
            })
        })
    });
    n.function("sheet.input", Effect::Pure, |c| {
        guard(|| {
            read(c, |b, c| {
                let (s, r, col) = cell_of(b, str_arg(c, 1, "reference")?)?;
                Ok(Data::str(&b.input(s, r, col)?))
            })
        })
    });
    n.function("sheet.cell", Effect::Pure, |c| {
        guard(|| {
            read(c, |b, c| {
                let (s, r, col) = cell_of(b, str_arg(c, 1, "reference")?)?;
                b.cell_info(s, r, col)
            })
        })
    });
    n.function("sheet.range", Effect::Pure, |c| {
        guard(|| {
            read(c, |b, c| {
                let (sheet, span) = b.resolve(str_arg(c, 1, "range")?)?;
                let (header, text) = table_opts(c.arg(2), false);
                match b.bounds(sheet, &span)? {
                    Some(bounds) => b.table(sheet, bounds, header, text),
                    None => Ok(empty_table()),
                }
            })
        })
    });
    n.function("sheet.table", Effect::Pure, |c| {
        guard(|| {
            read(c, |b, c| {
                let sheet = b.sheet_index(c.arg(1))?;
                let (header, text) = table_opts(c.arg(2), true);
                match b.used(sheet)? {
                    Some(bounds) => b.table(sheet, bounds, header, text),
                    None => Ok(empty_table()),
                }
            })
        })
    });
    n.function("sheet.cells", Effect::Pure, |c| {
        guard(|| {
            read(c, |b, c| {
                let sheet = b.sheet_index(c.arg(1))?;
                let o = c.arg(2);
                let limit = o.get("limit").num_or(10_000.0).clamp(0.0, MAX_READ_CELLS as f64) as usize;
                b.cells(sheet, o.get("formulas").truthy(), limit)
            })
        })
    });
    n.function("sheet.style", Effect::Pure, |c| {
        guard(|| {
            read(c, |b, c| {
                let (s, r, col) = cell_of(b, str_arg(c, 1, "reference")?)?;
                b.style(s, r, col)
            })
        })
    });
    n.function("sheet.describe", Effect::Pure, |c| guard(|| read(c, |b, _| b.describe())));
    n.function("sheet.can_undo", Effect::Pure, |c| guard(|| read(c, |b, _| Ok(Data::Bool(b.can_undo())))));
    n.function("sheet.can_redo", Effect::Pure, |c| guard(|| read(c, |b, _| Ok(Data::Bool(b.can_redo())))));
    n.function("sheet.save", Effect::Pure, |c| guard(|| read(c, |b, _| Ok(Data::bytes(b.to_xlsx()?)))));
    n.function("sheet.to_csv", Effect::Pure, |c| guard(|| read(c, |b, c| Ok(Data::str(&b.to_csv(b.sheet_index(c.arg(1))?, &CsvOut::from_data(c.arg(2))?)?)))));
    n.job_fn("sheet.export", |j: &JobCtx| {
        guard(|| {
            let (_, w) = book_of(j.input(0), "sheet.export's input")?;
            if j.cancelled() {
                return Err("cancelled".into());
            }
            // copy under the lock, write without it: the page keeps reading
            // and editing while the file is built
            let snap = w.lock()?.snapshot()?;
            j.progress(0.1);
            if j.cancelled() {
                return Err("cancelled".into());
            }
            match j.opts.get("format").as_str().unwrap_or("xlsx") {
                "xlsx" => Ok(Data::bytes(snap.to_xlsx()?)),
                "csv" => Ok(Data::str(&snap.to_csv(snap.sheet_index(j.opts.get("sheet"))?, &CsvOut::from_data(j.opts)?)?)),
                other => Err(format!("unknown format \"{other}\" (xlsx, csv)")),
            }
        })
    });

    // ---------------------------------------------------------- references
    n.function("sheet.col_name", Effect::Pure, |c| match c.arg(0) {
        Data::Num(n) if n.fract() == 0.0 && *n >= 1.0 && *n <= MAX_COL as f64 => Ok(Data::str(&refs::col_name(*n as i32))),
        _ => Err(format!("column must be a whole number from 1 to {MAX_COL}")),
    });
    n.function("sheet.col_index", Effect::Pure, |c| {
        let s = str_arg(c, 0, "column letters")?;
        refs::col_index(s).map(|n| Data::Num(n as f64)).ok_or_else(|| format!("\"{s}\" is not a column (A to XFD)"))
    });
    n.function("sheet.ref", Effect::Pure, |c| {
        let (row, col) = (c.arg(0).num_or(0.0), c.arg(1).num_or(0.0));
        if row.fract() != 0.0 || col.fract() != 0.0 || !(1.0..=MAX_ROW as f64).contains(&row) || !(1.0..=MAX_COL as f64).contains(&col) {
            return Err("sheet.ref(row, col[, sheet]) needs a row from 1 and a column from 1 inside the sheet".into());
        }
        Ok(Data::str(&refs::format_ref(c.arg(2).as_str(), &Span::Cell { row: row as i32, col: col as i32 })))
    });
    n.function("sheet.parse_ref", Effect::Pure, |c| {
        let r = refs::parse(str_arg(c, 0, "reference")?)?;
        let num = |n: i32| Data::Num(n as f64);
        let mut pairs: Vec<(&str, Data)> = match r.span {
            Span::Cell { row, col } => vec![("row", num(row)), ("col", num(col)), ("last_row", num(row)), ("last_col", num(col))],
            Span::Area { r1, c1, r2, c2 } => vec![("row", num(r1)), ("col", num(c1)), ("last_row", num(r2)), ("last_col", num(c2))],
            Span::Cols { c1, c2 } => vec![("col", num(c1)), ("last_col", num(c2))],
            Span::Rows { r1, r2 } => vec![("row", num(r1)), ("last_row", num(r2))],
        };
        if let Some(s) = r.sheet {
            pairs.push(("sheet", Data::str(&s)));
        }
        Ok(Data::map(pairs))
    });

    // ---------------------------------------------------------- edits
    n.function("sheet.set", Effect::Effect, |c| {
        guard(|| {
            edit(c, |b, c| {
                let (s, r, col) = cell_of(b, str_arg(c, 1, "reference")?)?;
                b.set(s, r, col, c.arg(2))
            })
        })
    });
    n.function("sheet.set_range", Effect::Effect, |c| {
        guard(|| {
            edit(c, |b, c| {
                let text = str_arg(c, 1, "reference")?;
                let (sheet, span) = b.resolve(text)?;
                let opts = c.arg(3);
                // tables from other modules or from wb:range keep their types
                let literal = opts.get("literal").as_bool().unwrap_or_else(|| is_table(c.arg(2)));
                let rows = rows_of(c.arg(2), opts)?;
                if let Span::Area { r1, c1, r2, c2 } = span {
                    let (h, w) = (rows.len() as i32, rows.iter().map(Vec::len).max().unwrap_or(0) as i32);
                    if h > r2 - r1 + 1 || w > c2 - c1 + 1 {
                        return Err(format!("{h} rows x {w} columns do not fit in {text}"));
                    }
                }
                let (row, col) = span.origin();
                b.set_rows(sheet, row, col, &rows, literal).map(|_| ())
            })
        })
    });
    n.function("sheet.clear", Effect::Effect, |c| {
        guard(|| {
            edit(c, |b, c| {
                let (sheet, span) = b.resolve(str_arg(c, 1, "range")?)?;
                match b.bounds(sheet, &span)? {
                    Some(bounds) => b.clear(sheet, bounds),
                    None => Ok(()),
                }
            })
        })
    });
    n.function("sheet.set_style", Effect::Effect, |c| {
        guard(|| {
            edit(c, |b, c| {
                let (sheet, span) = b.resolve(str_arg(c, 1, "range")?)?;
                b.set_style(sheet, &span, c.arg(2))
            })
        })
    });
    n.function("sheet.add_sheet", Effect::Effect, |c| guard(|| edit(c, |b, c| b.add_sheet(c.arg(1).as_str()).map(|_| ()))));
    n.function("sheet.delete_sheet", Effect::Effect, |c| {
        guard(|| {
            edit(c, |b, c| {
                if c.arg(1).is_nil() {
                    return Err("say which sheet to delete (a name or a 1-based index)".into());
                }
                let s = b.sheet_index(c.arg(1))?;
                b.delete_sheet(s)
            })
        })
    });
    n.function("sheet.rename_sheet", Effect::Effect, |c| {
        guard(|| {
            edit(c, |b, c| {
                let s = b.sheet_index(c.arg(1))?;
                b.rename_sheet(s, str_arg(c, 2, "new name")?)
            })
        })
    });
    n.function("sheet.insert_rows", Effect::Effect, |c| {
        guard(|| {
            edit(c, |b, c| {
                let (s, start, count) = lines_arg(b, c, true)?;
                b.insert_rows(s, start, count)
            })
        })
    });
    n.function("sheet.delete_rows", Effect::Effect, |c| {
        guard(|| {
            edit(c, |b, c| {
                let (s, start, count) = lines_arg(b, c, true)?;
                b.delete_rows(s, start, count)
            })
        })
    });
    n.function("sheet.insert_columns", Effect::Effect, |c| {
        guard(|| {
            edit(c, |b, c| {
                let (s, start, count) = lines_arg(b, c, false)?;
                b.insert_columns(s, start, count)
            })
        })
    });
    n.function("sheet.delete_columns", Effect::Effect, |c| {
        guard(|| {
            edit(c, |b, c| {
                let (s, start, count) = lines_arg(b, c, false)?;
                b.delete_columns(s, start, count)
            })
        })
    });
    n.function("sheet.undo", Effect::Effect, |c| guard(|| edit(c, |b, _| b.undo().map(|_| ()))));
    n.function("sheet.redo", Effect::Effect, |c| guard(|| edit(c, |b, _| b.redo().map(|_| ()))));
    n.function("sheet.recalc", Effect::Effect, |c| {
        guard(|| {
            edit(c, |b, _| {
                b.recalc();
                Ok(())
            })
        })
    });
    n.function("sheet.clear_history", Effect::Effect, |c| guard(|| edit(c, |b, _| b.clear_history())));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_forms() {
        let row = |v: &[f64]| Data::nums(v.iter().copied());
        assert_eq!(rows_of(&row(&[1.0, 2.0]), &Data::Nil).unwrap(), vec![vec![Data::Num(1.0), Data::Num(2.0)]]);
        let two = Data::list(vec![row(&[1.0]), row(&[2.0, 3.0])]);
        assert_eq!(rows_of(&two, &Data::Nil).unwrap().len(), 2);
        let table = Data::map([("columns", Data::list(vec![Data::str("a"), Data::str("b")])), ("rows", two.clone())]);
        assert!(is_table(&table) && !is_table(&two));
        assert_eq!(rows_of(&table, &Data::Nil).unwrap().len(), 3);
        assert_eq!(rows_of(&table, &Data::map([("header", Data::Bool(false))])).unwrap().len(), 2);
        // a row with holes, as Luau hands it over
        let sparse = Data::list(vec![Data::map([("1", Data::Num(1.0)), ("3", Data::Num(3.0))])]);
        assert_eq!(rows_of(&sparse, &Data::Nil).unwrap()[0], vec![Data::Num(1.0), Data::Nil, Data::Num(3.0)]);
        assert!(rows_of(&Data::Num(1.0), &Data::Nil).is_err());
    }

    #[test]
    fn publishing_only_moves_forward() {
        let r = Res::new(KIND, 1u8);
        let Data::Res(v3) = publish(&r, 3) else { panic!() };
        assert_eq!(v3.version(), 3);
        let Data::Res(same) = publish(&v3, 3) else { panic!() };
        assert_eq!(same, v3);
    }

    #[test]
    fn panics_become_errors() {
        let e = guard(|| panic!("boom")).unwrap_err();
        assert!(e.contains("boom"), "{e}");
    }
}
