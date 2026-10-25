//! The workbook resource: an IronCalc `UserModel` (the model plus its undo
//! history) behind a mutex. Edits mutate it in place; the module then
//! publishes the resource with a new version so every cell that read the old
//! one recomputes.
//!
//! Everything that makes IronCalc parse, evaluate, print or drop formulas
//! runs through [`deep`] (a thread with a large stack), and every formula is
//! measured by [`formula::check`] before IronCalc sees it.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::mem::ManuallyDrop;
use std::sync::{Mutex, MutexGuard};

use anypage_runtime::Data;
use ironcalc::base::expressions::parser::static_analysis::StaticResult;
use ironcalc::base::expressions::types::Area;
use ironcalc::base::formatter::dates::from_excel_date;
use ironcalc::base::formatter::format::format_number;
use ironcalc::base::formatter::lexer::is_likely_date_number_format;
use ironcalc::base::language::{get_language, Language};
use ironcalc::base::locale::{get_locale, Locale};
use ironcalc::base::types::{Alignment, Cell, Color, FormulaValue, HorizontalAlignment, SpillValue, Style, VerticalAlignment};
use ironcalc::base::{Model, UserModel};

use crate::deep::deep;
use crate::formula::{self, Scan, MAX_BOOK_WALK};
use crate::refs::{self, format_ref, Span, MAX_COL, MAX_ROW};
use crate::xlsx::{self, sqref_area, XlsxLimits};

/// Formulas are always parsed and printed in English: `=SUM(A1,B1)`, `1.5`.
pub const LANGUAGE: &str = "en";
/// The model's own locale, whatever the workbook's display locale is.
pub const MODEL_LOCALE: &str = "en";
pub const DEFAULT_LOCALE: &str = "en";
pub const DEFAULT_TIMEZONE: &str = "UTC";

/// Most cells one read (`range`, `table`, `cells`) hands to a script.
pub const MAX_READ_CELLS: usize = 500_000;
/// Most cells one inline write (`set_range`, a paste) may touch. Writing
/// costs 10 to 20 µs a cell (50 000 cells, 10% formulas: 0.9 s on a busy
/// desktop), and a page's handler may run at most 1 s (phones are slower),
/// so one call stays near half of that; bigger imports go through CSV in
/// the `sheet.load` job.
pub const MAX_WRITE_CELLS: usize = 25_000;
/// Most cells one `set_style` may restyle (all its properties at once, one
/// undo step; 50 000 cells with 3 properties took 1.0 s on a busy desktop).
/// For whole columns and rows this counts the cells IronCalc has to touch
/// per property (rows with data or styles, styled column spans).
pub const MAX_STYLE_CELLS: usize = 25_000;
/// Most cells a CSV import may write (in the `sheet.load` job).
pub const MAX_IMPORT_CELLS: usize = 2_000_000;
/// Most cells an inline `sheet.open` / `sheet.from_csv` may load on the
/// page's thread; bigger files must go through the `sheet.load` job.
pub const MAX_INLINE_CELLS: u64 = 200_000;
/// Excel's limit on the text in one cell, in characters.
pub const MAX_CELL_CHARS: usize = 32_767;
pub use crate::formula::MAX_FORMULA_CHARS;
pub const MAX_SHEETS: usize = 1_024;

const ERRORS: [&str; 12] = ["#NULL!", "#DIV/0!", "#VALUE!", "#REF!", "#NAME?", "#NUM!", "#N/A", "#ERROR!", "#N/IMPL", "#SPILL!", "#CALC!", "#CIRC!"];

/// A spreadsheet shared by pages and jobs. Lock it to read or edit.
pub struct Workbook {
    book: Mutex<Book>,
}

/// The locked workbook.
pub struct Book {
    /// Dropped on a large stack (see `Drop for Book`).
    um: ManuallyDrop<UserModel<'static>>,
    /// Number of edits so far: the version the resource is published at.
    edits: u64,
    /// How numbers are shown and how typed numbers such as "1,5" are read.
    locale: &'static Locale,
    locale_id: String,
    language: &'static Language,
    /// Estimated cells all formulas and conditional formats read per
    /// recalculation (see [`formula`]).
    walk: u64,
    /// A formula that reads many cells has existed in this workbook, so
    /// undo and structural edits recount `walk`.
    walk_seen: bool,
    /// Cost of each defined name, `(walk, walk_clipped)`.
    names: HashMap<String, (u64, u64)>,
}

impl Drop for Book {
    fn drop(&mut self) {
        // SAFETY: `um` is taken exactly once, here, and never used again.
        let um = unsafe { ManuallyDrop::take(&mut self.um) };
        // dropping parsed formulas recurses once per level of each expression
        deep(move || drop(um));
    }
}

/// Inclusive cell rectangle (r1, c1, r2, c2).
pub type Bounds = (i32, i32, i32, i32);

/// A cell's value as scripts see it: number, string, boolean, `{error = "#DIV/0!"}`, or nil.
pub fn cell_data(cell: &Cell, strings: &[String]) -> Data {
    match cell {
        Cell::EmptyCell { .. } => Data::Nil,
        Cell::BooleanCell { v, .. } => Data::Bool(*v),
        Cell::NumberCell { v, .. } => Data::Num(*v),
        Cell::ErrorCell { ei, .. } => error_data(&ei.to_string()),
        Cell::SharedString { si, .. } => usize::try_from(*si).ok().and_then(|i| strings.get(i)).map(|s| Data::str(s)).unwrap_or_default(),
        Cell::CellFormula { v, .. } | Cell::ArrayFormula { v, .. } => match v {
            FormulaValue::Unevaluated => error_data("#ERROR!"),
            FormulaValue::Boolean(b) => Data::Bool(*b),
            FormulaValue::Number(n) => Data::Num(*n),
            FormulaValue::Text(s) => Data::str(s),
            FormulaValue::Error { ei, .. } => error_data(&ei.to_string()),
        },
        Cell::SpillCell { v, .. } => match v {
            SpillValue::Boolean(b) => Data::Bool(*b),
            SpillValue::Number(n) => Data::Num(*n),
            SpillValue::Text(s) => Data::str(s),
            SpillValue::Error(ei) => error_data(&ei.to_string()),
        },
    }
}

pub fn error_data(e: &str) -> Data {
    Data::map([("error", Data::str(e))])
}

/// A number as plain decimal text (shortest form that reads back exactly).
pub fn number_text(n: f64) -> String {
    let s = format!("{n}");
    if s == "-0" {
        "0".into()
    } else {
        s
    }
}

/// An Excel serial date as ISO 8601 ("2024-01-15", or "2024-01-15T13:30:00" with a time).
pub fn iso_date(serial: f64) -> Option<String> {
    if !serial.is_finite() || serial < 0.0 {
        return None;
    }
    let days = serial.floor();
    let date = from_excel_date(days as i64).ok()?;
    let secs = (((serial - days) * 86_400.0).round() as i64).min(86_399);
    if secs == 0 {
        Some(date.to_string())
    } else {
        Some(format!("{date}T{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60))
    }
}

fn valid_iso_date(s: &str) -> bool {
    let p: Vec<&str> = s.split('-').collect();
    p.len() == 3
        && p[0].len() == 4
        && (1..=2).contains(&p[1].len())
        && (1..=2).contains(&p[2].len())
        && p.iter().all(|x| x.bytes().all(|b| b.is_ascii_digit()))
}

fn type_name(d: &Data) -> &'static str {
    match d {
        Data::Nil => "nil",
        Data::Bool(_) => "a boolean",
        Data::Num(_) => "a number",
        Data::Str(_) => "a string",
        Data::Bytes(_) => "bytes",
        Data::List(_) => "a list",
        Data::Map(_) => "a table",
        Data::Res(_) => "a resource",
    }
}

const VALUE_FORMS: &str = "a cell value is a number, string, boolean, nil, {error = \"#N/A\"}, {text = \"...\"}, {formula = \"SUM(A1:A3)\"} or {date = \"YYYY-MM-DD\"}";

/// The locale's decimal and group separators, or `None` for "1,234.5" style.
fn separators(locale: &Locale) -> Option<(char, char)> {
    let s = &locale.numbers.symbols;
    let d = s.decimal.chars().next().unwrap_or('.');
    let g = s.group.chars().next().unwrap_or(',');
    (d != '.' || g != ',').then_some((d, g))
}

/// A number typed the locale's way ("1.234,5", "12,5%", "-0,25") in the
/// English form IronCalc reads; anything else unchanged.
pub fn delocalize<'a>(s: &'a str, locale: &Locale) -> Cow<'a, str> {
    let Some((dec, group)) = separators(locale) else { return Cow::Borrowed(s) };
    let t = s.trim();
    let (sign, rest) = match t.strip_prefix(['-', '+']) {
        Some(r) => (&t[..1], r),
        None => ("", t),
    };
    let (body, pct) = match rest.strip_suffix('%') {
        Some(b) => (b, "%"),
        None => (rest, ""),
    };
    let (int, frac) = match body.split_once(dec) {
        Some((i, f)) => (i, Some(f)),
        None => (body, None),
    };
    if int.is_empty() || frac.is_some_and(|f| f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit())) {
        return Cow::Borrowed(s);
    }
    // the integer part: digits, or groups of three after the first group
    let groups: Vec<&str> = int.split(group).collect();
    let grouped = groups.len() > 1
        && (1..=3).contains(&groups[0].len())
        && groups[1..].iter().all(|g| g.len() == 3)
        && groups.iter().all(|g| g.bytes().all(|b| b.is_ascii_digit()));
    let plain = groups.len() == 1 && int.bytes().all(|b| b.is_ascii_digit());
    if !(grouped || plain) {
        return Cow::Borrowed(s);
    }
    let digits: String = groups.concat();
    Cow::Owned(match frac {
        Some(f) => format!("{sign}{digits}.{f}{pct}"),
        None => format!("{sign}{digits}{pct}"),
    })
}

/// What a script value means as cell input: the text IronCalc is given and,
/// for a formula, its measurements.
#[derive(Debug)]
pub struct Input {
    pub text: String,
    pub formula: Option<Scan>,
}

/// Would IronCalc read this input as a formula? (`=...`, or `+`/`-`
/// followed by something that is not a number.)
pub fn looks_like_formula(s: &str) -> bool {
    match s.as_bytes().first() {
        Some(b'=') => s.len() > 1,
        Some(b'+' | b'-') => {
            let rest = s[1..].trim().trim_end_matches('%').replace(',', "");
            let number = rest.starts_with(|c: char| c.is_ascii_digit() || c == '.') && rest.parse::<f64>().is_ok();
            !rest.is_empty() && !number
        }
        _ => false,
    }
}

/// Turn a script value into what a person would type into the cell. Strings
/// are typed as is: "=..." is a formula, "12" a number, "'12" the text 12;
/// with `literal`, strings are always text. Formulas are measured and
/// refused if they could crash or hang the engine.
pub fn input_text(v: &Data, locale: &Locale, literal: bool) -> Result<Input, String> {
    let s = match v {
        Data::Nil => String::new(),
        Data::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Data::Num(n) => {
            if !n.is_finite() {
                return Err("numbers in cells must be finite (no NaN or infinity)".into());
            }
            number_text(*n)
        }
        Data::Str(s) if literal && !s.is_empty() => format!("'{s}"),
        Data::Str(s) => delocalize(s, locale).into_owned(),
        Data::Map(_) => {
            if let Some(e) = v.get("error").as_str() {
                let up = e.to_uppercase();
                if !ERRORS.contains(&up.as_str()) {
                    return Err(format!("unknown error value \"{e}\" (one of {})", ERRORS.join(" ")));
                }
                up
            } else if let Some(t) = v.get("text").as_str() {
                format!("'{t}")
            } else if let Some(f) = v.get("formula").as_str() {
                format!("={}", f.strip_prefix('=').unwrap_or(f))
            } else if let Some(d) = v.get("date").as_str() {
                if !valid_iso_date(d) {
                    return Err(format!("date \"{d}\" is not YYYY-MM-DD"));
                }
                d.to_string()
            } else {
                return Err(VALUE_FORMS.into());
            }
        }
        other => return Err(format!("{VALUE_FORMS}; got {}", type_name(other))),
    };
    if looks_like_formula(&s) {
        let scan = formula::check(&s)?;
        return Ok(Input { text: s, formula: Some(scan) });
    }
    if s.chars().count() > MAX_CELL_CHARS + 1 {
        return Err(format!("cell text longer than {MAX_CELL_CHARS} characters"));
    }
    Ok(Input { text: s, formula: None })
}

/// Is a sheet name acceptable to Excel?
pub fn check_sheet_name(name: &str) -> Result<(), String> {
    let n = name.chars().count();
    if n == 0 || n > 31 {
        return Err(format!("sheet names have 1 to 31 characters (\"{name}\" has {n})"));
    }
    if name.contains(['[', ']', ':', '*', '?', '/', '\\']) {
        return Err(format!("sheet name \"{name}\" contains one of [ ] : * ? / \\"));
    }
    if name.starts_with('\'') || name.ends_with('\'') {
        return Err(format!("sheet name \"{name}\" starts or ends with '"));
    }
    Ok(())
}

/// A display locale by id.
pub fn locale_of(id: &str) -> Result<&'static Locale, String> {
    get_locale(id).map_err(|_| format!("unknown locale \"{id}\" (for example en, de, fr, es, it)"))
}

fn english() -> Result<&'static Language, String> {
    get_language(LANGUAGE)
}

/// Options for new workbooks and imports.
#[derive(Clone, Debug)]
pub struct BookOptions {
    pub name: String,
    /// Display locale: number formatting and typed numbers. Formulas are always English.
    pub locale: String,
    pub timezone: String,
}

impl Default for BookOptions {
    fn default() -> Self {
        Self { name: "workbook".into(), locale: DEFAULT_LOCALE.into(), timezone: DEFAULT_TIMEZONE.into() }
    }
}

impl BookOptions {
    /// From a script options table `{ name, locale, timezone }` (nil fields keep defaults).
    pub fn from_data(d: &Data) -> Self {
        let mut o = Self::default();
        if let Some(n) = d.get("name").as_str() {
            o.name = n.chars().take(200).collect();
        }
        if let Some(l) = d.get("locale").as_str() {
            o.locale = l.to_string();
        }
        if let Some(t) = d.get("timezone").as_str() {
            o.timezone = t.to_string();
        }
        o
    }
}

/// An empty IronCalc model with these options (its own locale is always English).
pub fn empty_model(o: &BookOptions) -> Result<Model<'static>, String> {
    locale_of(&o.locale)?;
    let mut m = Model::new_empty("workbook", MODEL_LOCALE, DEFAULT_TIMEZONE, LANGUAGE)?;
    if o.timezone != DEFAULT_TIMEZONE {
        m.set_timezone(&o.timezone).map_err(|e| format!("timezone \"{}\": {e}", o.timezone))?;
    }
    m.workbook.name = o.name.clone();
    Ok(m)
}

/// Refuse a loaded workbook that IronCalc would fail on later (no sheets,
/// two sheets with one name) or panic on (style indexes out of range).
fn validate(wb: &ironcalc::base::types::Workbook) -> Result<(), String> {
    let bad = |why: String| format!("not a readable xlsx file: {why}");
    if wb.worksheets.is_empty() {
        return Err(bad("it has no sheets".into()));
    }
    let mut names = HashSet::new();
    for ws in &wb.worksheets {
        if !names.insert(ws.name.to_lowercase()) {
            return Err(bad(format!("two sheets are named \"{}\"", ws.name)));
        }
    }
    let st = &wb.styles;
    let fits = |i: i32, n: usize| i >= 0 && (i as usize) < n;
    if st.cell_xfs.is_empty() {
        return Err(bad("it has no cell formats".into()));
    }
    for (k, xf) in st.cell_xfs.iter().enumerate() {
        if !fits(xf.font_id, st.fonts.len()) || !fits(xf.fill_id, st.fills.len()) || !fits(xf.border_id, st.borders.len()) {
            return Err(bad(format!("cell format {k} points to a font, fill or border that does not exist")));
        }
    }
    for (k, xf) in st.cell_style_xfs.iter().enumerate() {
        if !fits(xf.font_id, st.fonts.len()) || !fits(xf.fill_id, st.fills.len()) || !fits(xf.border_id, st.borders.len()) {
            return Err(bad(format!("cell style {k} points to a font, fill or border that does not exist")));
        }
    }
    let n = st.cell_xfs.len();
    for ws in &wb.worksheets {
        for (r, c, cell) in ws.sheet_data.cells() {
            let s = cell.get_style();
            if !fits(s, n) {
                let at = format_ref(Some(&ws.name), &Span::Cell { row: r, col: c });
                return Err(bad(format!("cell {at} uses format {s}, but there are only {n}")));
            }
        }
        if let Some(row) = ws.rows.iter().find(|row| !fits(row.s, n)) {
            return Err(bad(format!("row {} of {} uses format {}, but there are only {n}", row.r, ws.name, row.s)));
        }
        if let Some(col) = ws.cols.iter().find(|col| col.style.is_some_and(|s| !fits(s, n))) {
            return Err(bad(format!("columns {} to {} of {} use a format that does not exist", col.min, col.max, ws.name)));
        }
    }
    Ok(())
}

impl Workbook {
    pub fn new(o: &BookOptions) -> Result<Self, String> {
        Ok(Self::from_book(Book::from_model(empty_model(o)?, &o.locale)?))
    }

    pub fn from_book(b: Book) -> Self {
        Self { book: Mutex::new(b) }
    }

    /// Read an xlsx file and recalculate it, with the default limits.
    pub fn open_xlsx(bytes: &[u8], o: &BookOptions, cancelled: &dyn Fn() -> bool, progress: &dyn Fn(f32)) -> Result<Self, String> {
        Self::open_xlsx_with(bytes, o, &XlsxLimits::default(), cancelled, progress)
    }

    /// Read an xlsx file: measured first ([`xlsx::check_xlsx_with`]), then
    /// loaded, validated, parsed and evaluated on a large stack.
    /// `cancelled` is polled while unpacking and between stages (IronCalc's
    /// own parsing and evaluation are not interruptible).
    pub fn open_xlsx_with(bytes: &[u8], o: &BookOptions, limits: &XlsxLimits, cancelled: &dyn Fn() -> bool, progress: &dyn Fn(f32)) -> Result<Self, String> {
        locale_of(&o.locale)?;
        let report = xlsx::check_xlsx_with(bytes, limits, cancelled)?;
        progress(0.2);
        if cancelled() {
            return Err("cancelled".into());
        }
        let (name, tz) = (o.name.as_str(), o.timezone.as_str());
        let wb = deep(|| -> Result<_, String> {
            let wb = ironcalc::import::load_from_xlsx_bytes(bytes, name, MODEL_LOCALE, tz).map_err(|e| format!("not a readable xlsx file: {e}"))?;
            validate(&wb)?;
            Ok(wb)
        })?;
        if cancelled() {
            return Err("cancelled".into());
        }
        progress(0.5);
        let locale_id = o.locale.as_str();
        let mut book = deep(move || -> Result<Book, String> {
            let mut model = Model::from_workbook(wb, LANGUAGE).map_err(|e| format!("xlsx workbook: {e}"))?;
            model.evaluate();
            Book::from_model(model, locale_id)
        })?;
        progress(0.9);
        book.walk = report.walk;
        book.walk_seen = report.walk > 0;
        book.load_names()?;
        progress(1.0);
        Ok(Self::from_book(book))
    }

    /// The workbook, locked. Fails if an earlier internal error left it
    /// half-edited.
    pub fn lock(&self) -> Result<MutexGuard<'_, Book>, String> {
        self.book.lock().map_err(|_| "this workbook was damaged by an earlier internal error; open it again".to_string())
    }

    /// Take over the content of `other` (a file loaded by a job) in place.
    /// Returns the edit count to publish this workbook at: higher than
    /// anything either of them was published at.
    pub fn replace_with(&self, other: Workbook) -> Result<u64, String> {
        let incoming = other.book.into_inner().map_err(|_| "the loaded workbook was damaged by an internal error".to_string())?;
        let mut b = self.lock()?;
        let edits = b.edits.max(incoming.edits) + 1;
        let old = std::mem::replace(&mut *b, incoming);
        b.edits = edits;
        drop(b);
        // the old content is dropped after the lock is released
        drop(old);
        Ok(edits)
    }
}

/// One style property and its value, checked.
#[derive(Clone, Debug)]
enum StyleChange {
    Format(String),
    Bold(bool),
    Italic(bool),
    Underline(bool),
    Strike(bool),
    Size(i32),
    Color(String),
    Fill(String),
    Align(HorizontalAlignment, &'static str),
    VAlign(VerticalAlignment, &'static str),
    Wrap(bool),
}

impl StyleChange {
    /// IronCalc's style path and value text, for whole-row and whole-column styles.
    fn path(&self) -> (&'static str, String) {
        match self {
            Self::Format(f) => ("num_fmt", f.clone()),
            Self::Bold(b) => ("font.b", b.to_string()),
            Self::Italic(b) => ("font.i", b.to_string()),
            Self::Underline(b) => ("font.u", b.to_string()),
            Self::Strike(b) => ("font.strike", b.to_string()),
            Self::Size(n) => ("font.size", n.to_string()),
            Self::Color(c) => ("font.color", c.clone()),
            Self::Fill(c) => ("fill.color", c.clone()),
            Self::Align(_, s) => ("alignment.horizontal", s.to_string()),
            Self::VAlign(_, s) => ("alignment.vertical", s.to_string()),
            Self::Wrap(b) => ("alignment.wrap_text", b.to_string()),
        }
    }

    fn apply(&self, st: &mut Style) -> Result<(), String> {
        match self {
            Self::Format(f) => st.num_fmt = f.clone(),
            Self::Bold(b) => st.font.b = *b,
            Self::Italic(b) => st.font.i = *b,
            Self::Underline(b) => st.font.u = *b,
            Self::Strike(b) => st.font.strike = *b,
            Self::Size(n) => st.font.sz = *n,
            Self::Color(c) => st.font.color = Color::from_param(c)?,
            Self::Fill(c) => st.fill.color = Color::from_param(c)?,
            Self::Align(h, _) => st.alignment.get_or_insert_with(Alignment::default).horizontal = h.clone(),
            Self::VAlign(v, _) => st.alignment.get_or_insert_with(Alignment::default).vertical = v.clone(),
            Self::Wrap(w) => st.alignment.get_or_insert_with(Alignment::default).wrap_text = *w,
        }
        Ok(())
    }
}

const STYLE_KEYS: [&str; 11] = ["format", "bold", "italic", "underline", "strike", "size", "color", "fill", "align", "valign", "wrap"];

/// Check a script's style table: every property is valid before anything changes.
fn style_changes(props: &Data) -> Result<Vec<StyleChange>, String> {
    if !matches!(props, Data::Map(_)) {
        return Err("style properties must be a table such as { format = \"0.00\", bold = true }".into());
    }
    let mut out = Vec::new();
    for (k, v) in props.entries() {
        if !STYLE_KEYS.contains(&k) {
            return Err(format!("unknown style property \"{k}\" ({})", STYLE_KEYS.join(", ")));
        }
        let colour = |v: &Data| -> Result<String, String> {
            match v {
                Data::Nil => Ok(String::new()),
                Data::Num(n) if n.is_finite() && *n >= 0.0 && *n <= u32::MAX as f64 => Ok(format!("#{:06X}", (*n as u32) >> 8)),
                Data::Str(s) if s.is_empty() => Ok(String::new()),
                Data::Str(s) if s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|c| c.is_ascii_hexdigit()) => Ok(s.to_uppercase()),
                Data::Str(s) => Err(format!("{k} \"{s}\" is not #RRGGBB")),
                other => Err(format!("style property {k} cannot be {}", type_name(other))),
            }
        };
        let change = match (k, v) {
            ("format", Data::Str(s)) if s.is_empty() => StyleChange::Format("general".into()),
            ("format", Data::Str(s)) if s.len() <= 255 => StyleChange::Format(s.to_string()),
            ("format", _) => return Err("format must be a number format string of at most 255 bytes".into()),
            ("bold", Data::Bool(b)) => StyleChange::Bold(*b),
            ("italic", Data::Bool(b)) => StyleChange::Italic(*b),
            ("underline", Data::Bool(b)) => StyleChange::Underline(*b),
            ("strike", Data::Bool(b)) => StyleChange::Strike(*b),
            ("wrap", Data::Bool(b)) => StyleChange::Wrap(*b),
            ("size", Data::Num(n)) if n.fract() == 0.0 && (1.0..=409.0).contains(n) => StyleChange::Size(*n as i32),
            ("color", v) => StyleChange::Color(colour(v)?),
            ("fill", v) => StyleChange::Fill(colour(v)?),
            ("align", Data::Str(s)) => {
                let (h, name) = match s.to_lowercase().as_str() {
                    "left" => (HorizontalAlignment::Left, "left"),
                    "center" => (HorizontalAlignment::Center, "center"),
                    "right" => (HorizontalAlignment::Right, "right"),
                    "general" => (HorizontalAlignment::General, "general"),
                    "justify" => (HorizontalAlignment::Justify, "justify"),
                    "fill" => (HorizontalAlignment::Fill, "fill"),
                    "distributed" => (HorizontalAlignment::Distributed, "distributed"),
                    "centercontinuous" => (HorizontalAlignment::CenterContinuous, "centerContinuous"),
                    _ => return Err(format!("align \"{s}\" is not one of left, center, right, general, justify, fill, distributed")),
                };
                StyleChange::Align(h, name)
            }
            ("valign", Data::Str(s)) => {
                let (v, name) = match s.to_lowercase().as_str() {
                    "top" => (VerticalAlignment::Top, "top"),
                    "center" => (VerticalAlignment::Center, "center"),
                    "bottom" => (VerticalAlignment::Bottom, "bottom"),
                    "justify" => (VerticalAlignment::Justify, "justify"),
                    "distributed" => (VerticalAlignment::Distributed, "distributed"),
                    _ => return Err(format!("valign \"{s}\" is not one of top, center, bottom, justify, distributed")),
                };
                StyleChange::VAlign(v, name)
            }
            _ => return Err(format!("style property {k} cannot be {}", type_name(v))),
        };
        out.push(change);
    }
    Ok(out)
}

impl Book {
    /// Wrap an evaluated model. Its undo history starts empty.
    pub fn from_model(model: Model<'static>, locale: &str) -> Result<Book, String> {
        Ok(Book {
            um: ManuallyDrop::new(UserModel::from_model(model)),
            edits: 0,
            locale: locale_of(locale)?,
            locale_id: locale.to_string(),
            language: english()?,
            walk: 0,
            walk_seen: false,
            names: HashMap::new(),
        })
    }

    pub fn model(&self) -> &Model<'_> {
        self.um.get_model()
    }

    /// Run IronCalc work that may recurse on a large stack, with the model.
    fn calc<T: Send>(&mut self, f: impl FnOnce(&mut UserModel<'static>) -> T + Send) -> T {
        let um: &mut UserModel<'static> = &mut self.um;
        deep(move || f(um))
    }

    /// The same for reads.
    fn calc_ref<T: Send>(&self, f: impl FnOnce(&UserModel<'static>) -> T + Send) -> T {
        let um: &UserModel<'static> = &self.um;
        deep(move || f(um))
    }

    /// How many edits were made: the version the resource should be published at.
    pub fn edits(&self) -> u64 {
        self.edits
    }

    /// Estimated cells all formulas and conditional formats read per recalculation.
    pub fn walk(&self) -> u64 {
        self.walk
    }

    pub fn locale(&self) -> &'static Locale {
        self.locale
    }

    pub fn can_undo(&self) -> bool {
        self.um.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.um.can_redo()
    }

    pub fn name(&self) -> String {
        self.um.get_name()
    }

    pub fn sheet_names(&self) -> Vec<String> {
        self.model().workbook.get_worksheet_names()
    }

    pub fn sheet_name(&self, sheet: u32) -> Result<String, String> {
        Ok(self.model().workbook.worksheet(sheet)?.name.clone())
    }

    /// A sheet by name (exact, then case-insensitive as Excel does).
    pub fn sheet_by_name(&self, name: &str) -> Result<u32, String> {
        let names = self.sheet_names();
        let lower = name.to_lowercase();
        names
            .iter()
            .position(|n| n == name)
            .or_else(|| names.iter().position(|n| n.to_lowercase() == lower))
            .map(|i| i as u32)
            .ok_or_else(|| format!("no sheet named \"{name}\" (sheets: {})", names.join(", ")))
    }

    /// A sheet from a script value: a name, a 1-based index, or nil for the first.
    pub fn sheet_index(&self, sel: &Data) -> Result<u32, String> {
        let count = self.model().workbook.worksheets.len();
        match sel {
            Data::Nil if count > 0 => Ok(0),
            Data::Nil => Err("the workbook has no sheets".into()),
            Data::Num(n) if n.fract() == 0.0 && *n >= 1.0 && *n <= count as f64 => Ok(*n as u32 - 1),
            Data::Num(n) => Err(format!("no sheet {n} (the workbook has {count} sheets, numbered from 1)")),
            Data::Str(s) => self.sheet_by_name(s),
            other => Err(format!("a sheet is a name or a 1-based index, not {}", type_name(other))),
        }
    }

    /// Parse a reference; without a sheet name it refers to the first sheet.
    pub fn resolve(&self, text: &str) -> Result<(u32, Span), String> {
        let r = refs::parse(text)?;
        let sheet = match &r.sheet {
            Some(n) => self.sheet_by_name(n)?,
            None => self.sheet_index(&Data::Nil)?,
        };
        Ok((sheet, r.span))
    }

    /// "Sheet1!B2: <why>" for errors about one cell.
    fn at(&self, sheet: u32, row: i32, col: i32, why: impl std::fmt::Display) -> String {
        let name = self.sheet_name(sheet).unwrap_or_default();
        format!("{}: {why}", format_ref(Some(&name), &Span::Cell { row, col }))
    }

    /// The rectangle holding every non-empty cell, or `None` for an empty sheet.
    pub fn used(&self, sheet: u32) -> Result<Option<Bounds>, String> {
        let ws = self.model().workbook.worksheet(sheet)?;
        let mut b: Option<Bounds> = None;
        for (r, c, cell) in ws.sheet_data.cells() {
            if matches!(*cell, Cell::EmptyCell { .. }) {
                continue;
            }
            b = Some(match b {
                None => (r, c, r, c),
                Some((r1, c1, r2, c2)) => (r1.min(r), c1.min(c), r2.max(r), c2.max(c)),
            });
        }
        Ok(b)
    }

    /// The cells a span covers; whole rows and columns stop at the used range.
    /// `None` when that leaves nothing (whole columns of an empty sheet).
    pub fn bounds(&self, sheet: u32, span: &Span) -> Result<Option<Bounds>, String> {
        Ok(match *span {
            Span::Cell { row, col } => Some((row, col, row, col)),
            Span::Area { r1, c1, r2, c2 } => Some((r1, c1, r2, c2)),
            Span::Cols { c1, c2 } => self.used(sheet)?.map(|(_, _, r2, _)| (1, c1, r2, c2)),
            Span::Rows { r1, r2 } => self.used(sheet)?.map(|(_, _, _, c2)| (r1, 1, r2, c2)),
        })
    }

    pub fn value(&self, sheet: u32, row: i32, col: i32) -> Result<Data, String> {
        let m = self.model();
        let ws = m.workbook.worksheet(sheet)?;
        Ok(ws.cell(row, col).map(|c| cell_data(&c, &m.workbook.shared_strings)).unwrap_or_default())
    }

    /// The value as the cell shows it: its number format applied with the
    /// workbook's display locale.
    pub fn text(&self, sheet: u32, row: i32, col: i32) -> Result<String, String> {
        let m = self.model();
        let ws = m.workbook.worksheet(sheet)?;
        let Some(cell) = ws.cell(row, col) else { return Ok(String::new()) };
        let fmt = m.get_style_for_cell(sheet, row, col).map_err(|e| self.at(sheet, row, col, format!("cannot read its format ({e})")))?.num_fmt;
        Ok(cell.formatted_value(&m.workbook.shared_strings, self.language, |v| format_number(v, &fmt, self.locale).text))
    }

    /// "=SUM(A1:A3)", or `None` for a cell without a formula.
    pub fn formula(&self, sheet: u32, row: i32, col: i32) -> Result<Option<String>, String> {
        if !self.has_formula(sheet, row, col)? {
            return Ok(None);
        }
        self.calc_ref(|um| um.get_model().get_cell_formula(sheet, row, col))
    }

    fn has_formula(&self, sheet: u32, row: i32, col: i32) -> Result<bool, String> {
        Ok(self.model().workbook.worksheet(sheet)?.cell(row, col).is_some_and(|c| c.get_formula().is_some()))
    }

    /// What a person would type to recreate the cell (formula, or value text).
    pub fn input(&self, sheet: u32, row: i32, col: i32) -> Result<String, String> {
        let r = if self.has_formula(sheet, row, col)? {
            self.calc_ref(|um| um.get_cell_content(sheet, row, col))
        } else {
            self.um.get_cell_content(sheet, row, col)
        };
        r.map_err(|e| self.at(sheet, row, col, e))
    }

    pub fn number_format(&self, sheet: u32, row: i32, col: i32) -> Result<String, String> {
        Ok(self.model().get_style_for_cell(sheet, row, col).map_err(|e| self.at(sheet, row, col, format!("cannot read its format ({e})")))?.num_fmt)
    }

    /// Everything about one cell, for inspectors and agents.
    pub fn cell_info(&self, sheet: u32, row: i32, col: i32) -> Result<Data, String> {
        let m = self.model();
        let ws = m.workbook.worksheet(sheet)?;
        let cell: Option<Cow<Cell>> = ws.cell(row, col);
        let value = cell.as_ref().map(|c| cell_data(c, &m.workbook.shared_strings)).unwrap_or_default();
        let kind = match &value {
            Data::Nil => "empty",
            Data::Num(_) => "number",
            Data::Str(_) => "text",
            Data::Bool(_) => "bool",
            _ => "error",
        };
        let format = self.number_format(sheet, row, col)?;
        let mut pairs: Vec<(&str, Data)> = vec![
            ("ref", Data::str(&format_ref(Some(&ws.name), &Span::Cell { row, col }))),
            ("row", Data::Num(row as f64)),
            ("col", Data::Num(col as f64)),
            ("text", Data::str(&self.text(sheet, row, col)?)),
            ("input", Data::str(&self.input(sheet, row, col)?)),
            ("type", Data::str(kind)),
            ("format", Data::str(&format)),
        ];
        if let Some(f) = self.formula(sheet, row, col)? {
            pairs.push(("formula", Data::str(&f)));
        }
        if let (Data::Num(n), true) = (&value, is_likely_date_number_format(&format)) {
            if let Some(d) = iso_date(*n) {
                pairs.push(("date", Data::str(&d)));
            }
        }
        if let Some(Cell::CellFormula { v: FormulaValue::Error { d, .. }, .. } | Cell::ArrayFormula { v: FormulaValue::Error { d, .. }, .. }) = cell.as_deref() {
            pairs.push(("message", Data::str(&d.m)));
            pairs.push(("origin", Data::str(&d.o)));
        }
        pairs.push(("value", value));
        Ok(Data::map(pairs))
    }

    /// A rectangle as the shared table form `{ columns, rows }`. With
    /// `header` the first row names the columns; with `text` a `text` field
    /// holds the formatted strings next to the raw values.
    pub fn table(&self, sheet: u32, b: Bounds, header: bool, text: bool) -> Result<Data, String> {
        let (r1, c1, r2, c2) = b;
        let width = (c2 - c1 + 1) as usize;
        let height = (r2 - r1 + 1) as usize;
        if width.saturating_mul(height) > MAX_READ_CELLS {
            return Err(format!("{} cells requested; one read is limited to {MAX_READ_CELLS} (read the sheet in parts)", width.saturating_mul(height)));
        }
        let m = self.model();
        let ws = m.workbook.worksheet(sheet)?;
        let mut rows = Vec::with_capacity(height);
        let mut texts = Vec::with_capacity(if text { height } else { 0 });
        for r in r1..=r2 {
            let mut row = vec![Data::Nil; width];
            let mut trow = vec![Data::Nil; if text { width } else { 0 }];
            for (c, cell) in ws.sheet_data.cells_in_row_between(r, c1, c2) {
                let k = (c - c1) as usize;
                row[k] = cell_data(&cell, &m.workbook.shared_strings);
                // the shared table form holds Num/Str/Bool/Nil: errors as their text
                if let Some(e) = row[k].get("error").as_str() {
                    row[k] = Data::str(e);
                }
                if text && !row[k].is_nil() {
                    trow[k] = Data::str(&self.text(sheet, r, c)?);
                }
            }
            rows.push(Data::list(row));
            if text {
                texts.push(Data::list(trow));
            }
        }
        let mut columns: Vec<Data> = (c1..=c2).map(|c| Data::str(&refs::col_name(c))).collect();
        if header && !rows.is_empty() {
            let first = rows.remove(0);
            if text {
                texts.remove(0);
            }
            for (k, v) in first.items().iter().enumerate() {
                match v {
                    Data::Str(s) if !s.is_empty() => columns[k] = v.clone(),
                    Data::Num(n) => columns[k] = Data::str(&number_text(*n)),
                    Data::Bool(b) => columns[k] = Data::str(if *b { "TRUE" } else { "FALSE" }),
                    _ => {}
                }
            }
        }
        let mut pairs = vec![("columns", Data::list(columns)), ("rows", Data::list(rows))];
        if text {
            pairs.push(("text", Data::list(texts)));
        }
        Ok(Data::map(pairs))
    }

    /// Non-empty cells in row order: `{ ref, row, col, value, formula? }`.
    pub fn cells(&self, sheet: u32, formulas_only: bool, limit: usize) -> Result<Data, String> {
        self.calc_ref(|um| {
            let m = um.get_model();
            let ws = m.workbook.worksheet(sheet)?;
            let mut out = Vec::new();
            for (r, c, cell) in ws.sheet_data.cells() {
                if matches!(*cell, Cell::EmptyCell { .. }) {
                    continue;
                }
                let f = if cell.get_formula().is_some() { m.get_cell_formula(sheet, r, c)? } else { None };
                if formulas_only && f.is_none() {
                    continue;
                }
                if out.len() >= limit {
                    break;
                }
                let mut pairs = vec![
                    ("ref", Data::str(&refs::cell_name(r, c))),
                    ("row", Data::Num(r as f64)),
                    ("col", Data::Num(c as f64)),
                    ("value", cell_data(&cell, &m.workbook.shared_strings)),
                ];
                if let Some(f) = f {
                    pairs.push(("formula", Data::str(&f)));
                }
                out.push(Data::map(pairs));
            }
            Ok(Data::list(out))
        })
    }

    /// Per-sheet sizes and formula counts. This walks every cell: keep it
    /// out of drawing code that runs on every edit.
    pub fn describe(&self) -> Result<Data, String> {
        let mut sheets = Vec::new();
        for (i, name) in self.sheet_names().iter().enumerate() {
            let ws = self.model().workbook.worksheet(i as u32)?;
            let (mut cells, mut formulas) = (0usize, 0usize);
            for (_, _, cell) in ws.sheet_data.cells() {
                if !matches!(*cell, Cell::EmptyCell { .. }) {
                    cells += 1;
                    formulas += cell.get_formula().is_some() as usize;
                }
            }
            let (rows, cols) = self.used(i as u32)?.map(|(_, _, r2, c2)| (r2, c2)).unwrap_or((0, 0));
            sheets.push(Data::map([
                ("name", Data::str(name)),
                ("rows", Data::Num(rows as f64)),
                ("cols", Data::Num(cols as f64)),
                ("cells", Data::Num(cells as f64)),
                ("formulas", Data::Num(formulas as f64)),
            ]));
        }
        Ok(Data::map([
            ("name", Data::str(&self.name())),
            ("sheets", Data::list(sheets)),
            ("edits", Data::Num(self.edits as f64)),
            ("can_undo", Data::Bool(self.can_undo())),
            ("can_redo", Data::Bool(self.can_redo())),
            ("locale", Data::str(&self.locale_id)),
            ("timezone", Data::str(&self.model().get_timezone())),
            ("walk", Data::Num(self.walk as f64)),
        ]))
    }

    // ------------------------------------------------------------ cost accounting

    /// Read the defined names (from a loaded file) and what each costs.
    fn load_names(&mut self) -> Result<(), String> {
        let list = self.calc_ref(|um| um.get_defined_name_list());
        self.names = formula::name_costs(list.iter().map(|(n, _, f)| (n.as_str(), f.as_str())));
        Ok(())
    }

    /// What a formula input costs per recalculation, names included.
    fn input_walk(&self, i: &Input) -> u64 {
        i.formula.as_ref().map(|s| s.walk_with(&self.names)).unwrap_or(0)
    }

    /// What the formulas now in a rectangle cost (0 when nothing in the
    /// workbook reads many cells).
    fn area_walk(&self, sheet: u32, b: Bounds) -> Result<u64, String> {
        if self.walk == 0 {
            return Ok(0);
        }
        let names = &self.names;
        self.calc_ref(|um| {
            let m = um.get_model();
            let ws = m.workbook.worksheet(sheet)?;
            let mut total = 0u64;
            for r in b.0..=b.2 {
                for (c, cell) in ws.sheet_data.cells_in_row_between(r, b.1, b.3) {
                    if cell.get_formula().is_some() {
                        let text = m.get_cell_formula(sheet, r, c)?.unwrap_or_default();
                        total = total.saturating_add(formula::scan(&text).walk_with(names));
                    }
                }
            }
            Ok(total)
        })
    }

    /// The workbook's cost after replacing formulas costing `old` with ones costing `new`.
    fn charge(&self, new: u64, old: u64) -> Result<u64, String> {
        let total = self.walk.saturating_sub(old).saturating_add(new);
        if new > old && total > MAX_BOOK_WALK {
            return Err(format!(
                "this change would make the workbook read about {total} cells on every recalculation (the limit is {MAX_BOOK_WALK}): whole \
                 columns or rows outside SUM, COUNTIF, XLOOKUP and similar are read cell by cell; use smaller ranges"
            ));
        }
        Ok(total)
    }

    fn set_walk(&mut self, w: u64) {
        self.walk = w;
        self.walk_seen |= w > 0;
    }

    /// The cost measured while importing (CSV formulas).
    pub(crate) fn set_walk_from_import(&mut self, w: u64) {
        self.set_walk(w);
    }

    /// Recompute the cost exactly, after edits that move or restore formulas.
    fn recount(&mut self) -> Result<(), String> {
        if !self.walk_seen {
            return Ok(());
        }
        let names = &self.names;
        let w = self.calc_ref(|um| -> Result<u64, String> {
            let m = um.get_model();
            let mut total = 0u64;
            for (s, ws) in m.workbook.worksheets.iter().enumerate() {
                for cf in &ws.conditional_formatting {
                    total = total.saturating_add(sqref_area(&cf.range));
                }
                if ws.shared_formulas.is_empty() {
                    continue;
                }
                // cells sharing a formula read ranges of the same size
                let mut seen: HashMap<i32, u64> = HashMap::new();
                for (r, c, cell) in ws.sheet_data.cells() {
                    let Some(f) = cell.get_formula() else { continue };
                    let w = match seen.get(&f) {
                        Some(w) => *w,
                        None => {
                            let text = m.get_cell_formula(s as u32, r, c)?.unwrap_or_default();
                            let w = formula::scan(&text).walk_with(names);
                            seen.insert(f, w);
                            w
                        }
                    };
                    total = total.saturating_add(w);
                }
            }
            Ok(total)
        })?;
        self.walk = w;
        Ok(())
    }

    // ------------------------------------------------------------ edits

    fn edited(&mut self) {
        // the collaboration queue is not used here; drop it so it cannot grow
        let _ = self.um.flush_send_queue();
        self.edits += 1;
    }

    /// Set one cell from a script value (see [`input_text`]) and recalculate.
    pub fn set(&mut self, sheet: u32, row: i32, col: i32, v: &Data) -> Result<(), String> {
        check_cell(row, col)?;
        let input = input_text(v, self.locale, false).map_err(|e| self.at(sheet, row, col, e))?;
        let new = self.input_walk(&input);
        let old = self.area_walk(sheet, (row, col, row, col))?;
        let walk = self.charge(new, old)?;
        let text = input.text;
        self.calc(|um| um.set_user_input(sheet, row, col, &text))?;
        self.set_walk(walk);
        self.edited();
        Ok(())
    }

    /// Set one cell as if `s` were typed into it, and recalculate.
    pub fn set_input(&mut self, sheet: u32, row: i32, col: i32, s: &str) -> Result<(), String> {
        self.set(sheet, row, col, &Data::str(s))
    }

    /// Write rows of script values from (row, col) as one undoable edit and
    /// recalculate once. Short rows are padded with empty cells; nil clears.
    /// With `literal`, strings are stored as text, never as numbers, dates
    /// or formulas.
    pub fn set_rows(&mut self, sheet: u32, row: i32, col: i32, rows: &[Vec<Data>], literal: bool) -> Result<Bounds, String> {
        check_cell(row, col)?;
        let height = rows.len();
        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        if height == 0 || width == 0 {
            return Ok((row, col, row, col));
        }
        if width.saturating_mul(height) > MAX_WRITE_CELLS {
            return Err(format!(
                "{} cells in one write; the limit is {MAX_WRITE_CELLS} (write in parts, or load the data as CSV with the sheet.load job)",
                width.saturating_mul(height)
            ));
        }
        let (r2, c2) = (row as i64 + height as i64 - 1, col as i64 + width as i64 - 1);
        if r2 > MAX_ROW as i64 || c2 > MAX_COL as i64 {
            return Err(format!("{height} x {width} values starting at {} go past the last row or column", refs::cell_name(row, col)));
        }
        let (r2, c2) = (r2 as i32, c2 as i32);
        // every value is checked before anything changes
        let mut w = csv::WriterBuilder::new().delimiter(b'\t').has_headers(false).flexible(false).from_writer(Vec::new());
        let mut record = Vec::with_capacity(width);
        let mut new = 0u64;
        for (i, r) in rows.iter().enumerate() {
            record.clear();
            for (j, v) in r.iter().enumerate() {
                let input = input_text(v, self.locale, literal).map_err(|e| format!("{}: {e}", refs::cell_name(row + i as i32, col + j as i32)))?;
                new = new.saturating_add(self.input_walk(&input));
                record.push(input.text);
            }
            record.resize(width, String::new());
            w.write_record(&record).map_err(|e| e.to_string())?;
        }
        let tsv = w.into_inner().map_err(|e| e.to_string())?;
        let tsv = String::from_utf8(tsv).map_err(|e| e.to_string())?;
        let old = self.area_walk(sheet, (row, col, r2, c2))?;
        let walk = self.charge(new, old)?;
        let area = Area { sheet, row, column: col, width: width as i32, height: height as i32 };
        let pasted = self.calc(|um| -> Result<(), (bool, String)> {
            // the paste ends by selecting the pasted area in the model's view,
            // which must contain the view's selected cell: put it there first
            um.set_selected_sheet(sheet).map_err(|e| (false, e))?;
            um.set_selected_cell(row, col).map_err(|e| (false, e))?;
            match um.paste_csv_string(&area, &tsv) {
                Ok(()) => Ok(()),
                Err(e) => {
                    // IronCalc checks before it writes; if it failed after
                    // writing, keep what was written evaluated
                    um.evaluate();
                    Err((true, e))
                }
            }
        });
        match pasted {
            Ok(()) => {
                self.set_walk(walk);
                self.edited();
                Ok((row, col, r2, c2))
            }
            Err((false, e)) => Err(e),
            Err((true, e)) => {
                self.edited();
                self.recount()?;
                Err(format!("{e} (the write stopped part way; cells already written stay)"))
            }
        }
    }

    /// Clear contents (values and formulas; styles stay). Only the used part
    /// of the rectangle is visited, so clearing `A1:XFD1048576` is cheap.
    pub fn clear(&mut self, sheet: u32, b: Bounds) -> Result<(), String> {
        let Some((ur1, uc1, ur2, uc2)) = self.used(sheet)? else { return Ok(()) };
        let (r1, c1, r2, c2) = (b.0.max(ur1), b.1.max(uc1), b.2.min(ur2), b.3.min(uc2));
        if r1 > r2 || c1 > c2 {
            return Ok(());
        }
        let old = self.area_walk(sheet, (r1, c1, r2, c2))?;
        let area = Area { sheet, row: r1, column: c1, width: c2 - c1 + 1, height: r2 - r1 + 1 };
        self.calc(|um| um.range_clear_contents(&area))?;
        let w = self.walk.saturating_sub(old);
        self.set_walk(w);
        self.edited();
        Ok(())
    }

    /// Add a sheet at the end; returns its index.
    pub fn add_sheet(&mut self, name: Option<&str>) -> Result<u32, String> {
        let count = self.model().workbook.worksheets.len();
        if count >= MAX_SHEETS {
            return Err(format!("a workbook has at most {MAX_SHEETS} sheets"));
        }
        if let Some(n) = name {
            check_sheet_name(n)?;
            if self.sheet_by_name(n).is_ok() {
                return Err(format!("there is already a sheet named \"{n}\""));
            }
        }
        let index = count as u32;
        self.calc(|um| -> Result<(), String> {
            um.new_sheet()?;
            if let Some(n) = name {
                if let Err(e) = um.rename_sheet(index, n) {
                    let _ = um.undo();
                    return Err(e);
                }
            }
            um.evaluate();
            Ok(())
        })?;
        self.edited();
        Ok(index)
    }

    pub fn delete_sheet(&mut self, sheet: u32) -> Result<(), String> {
        if self.model().workbook.worksheets.len() <= 1 {
            return Err("a workbook keeps at least one sheet".into());
        }
        self.sheet_name(sheet)?;
        self.calc(|um| -> Result<(), String> {
            um.delete_sheet(sheet)?;
            um.evaluate();
            Ok(())
        })?;
        self.edited();
        self.recount()
    }

    pub fn rename_sheet(&mut self, sheet: u32, name: &str) -> Result<(), String> {
        check_sheet_name(name)?;
        self.sheet_name(sheet)?;
        match self.sheet_by_name(name) {
            Ok(other) if other != sheet => return Err(format!("there is already a sheet named \"{name}\"")),
            _ => {}
        }
        self.calc(|um| -> Result<(), String> {
            um.rename_sheet(sheet, name)?;
            um.evaluate();
            Ok(())
        })?;
        self.edited();
        Ok(())
    }

    pub fn insert_rows(&mut self, sheet: u32, row: i32, count: i32) -> Result<(), String> {
        check_span(row, count, MAX_ROW, "row")?;
        self.calc(|um| um.insert_rows(sheet, row, count))?;
        self.edited();
        self.recount()
    }

    pub fn delete_rows(&mut self, sheet: u32, row: i32, count: i32) -> Result<(), String> {
        check_span(row, count, MAX_ROW, "row")?;
        self.calc(|um| um.delete_rows(sheet, row, count))?;
        self.edited();
        self.recount()
    }

    pub fn insert_columns(&mut self, sheet: u32, col: i32, count: i32) -> Result<(), String> {
        check_span(col, count, MAX_COL, "column")?;
        self.calc(|um| um.insert_columns(sheet, col, count))?;
        self.edited();
        self.recount()
    }

    pub fn delete_columns(&mut self, sheet: u32, col: i32, count: i32) -> Result<(), String> {
        check_span(col, count, MAX_COL, "column")?;
        self.calc(|um| um.delete_columns(sheet, col, count))?;
        self.edited();
        self.recount()
    }

    /// The look of one cell: `{format, bold, italic, underline, strike, size,
    /// color?, fill?, align?, valign?, wrap}`, colours as "#RRGGBB".
    pub fn style(&self, sheet: u32, row: i32, col: i32) -> Result<Data, String> {
        let st = self.model().get_style_for_cell(sheet, row, col).map_err(|e| self.at(sheet, row, col, format!("cannot read its style ({e})")))?;
        let colour = |c: &Color| {
            let s = self.um.resolve_color(c);
            if s.is_empty() {
                Data::Nil
            } else {
                Data::str(&s.to_uppercase())
            }
        };
        let mut pairs = vec![
            ("format", Data::str(&st.num_fmt)),
            ("bold", Data::Bool(st.font.b)),
            ("italic", Data::Bool(st.font.i)),
            ("underline", Data::Bool(st.font.u)),
            ("strike", Data::Bool(st.font.strike)),
            ("size", Data::Num(st.font.sz as f64)),
            ("color", colour(&st.font.color)),
            ("fill", colour(&st.fill.color)),
            ("wrap", Data::Bool(st.alignment.as_ref().is_some_and(|a| a.wrap_text))),
        ];
        if let Some(a) = &st.alignment {
            pairs.push(("align", Data::str(&format!("{:?}", a.horizontal).to_lowercase())));
            pairs.push(("valign", Data::str(&format!("{:?}", a.vertical).to_lowercase())));
        }
        Ok(Data::map(pairs))
    }

    /// Change the look of cells. `props` holds any of `format` (a number
    /// format such as "0.00%"), `bold`, `italic`, `underline`, `strike`,
    /// `size`, `color`, `fill` ("#RRGGBB", a packed 0xRRGGBBAA number, or
    /// nil/"" for none), `align` (left, center, right, general, justify,
    /// fill, distributed), `valign` (top, center, bottom, justify,
    /// distributed), `wrap`.
    ///
    /// A cell rectangle gets all properties as one undo step. Whole columns
    /// (`B:D`) and rows (`3:5`) get column and row styles, so cells written
    /// there later have them too; that is one undo step per property.
    /// Everything is checked before anything changes, and a failure part way
    /// is undone.
    pub fn set_style(&mut self, sheet: u32, span: &Span, props: &Data) -> Result<(), String> {
        let changes = style_changes(props)?;
        if changes.is_empty() {
            return Ok(());
        }
        let (r1, c1, r2, c2) = match *span {
            Span::Cell { row, col } => (row, col, row, col),
            Span::Area { r1, c1, r2, c2 } => (r1, c1, r2, c2),
            Span::Cols { c1, c2 } => (1, c1, MAX_ROW, c2),
            Span::Rows { r1, r2 } => (r1, 1, r2, MAX_COL),
        };
        let full_cols = r1 == 1 && r2 == MAX_ROW;
        let full_rows = c1 == 1 && c2 == MAX_COL;
        if full_cols || full_rows {
            return self.style_lines(sheet, (r1, c1, r2, c2), full_cols, &changes);
        }
        let cells = (r2 - r1 + 1) as usize * (c2 - c1 + 1) as usize;
        if cells > MAX_STYLE_CELLS {
            return Err(format!(
                "{cells} cells to style; one call is limited to {MAX_STYLE_CELLS} (style whole columns such as \"B:B\", or in parts)"
            ));
        }
        // the new style of every cell, computed before anything changes
        let mut styles = Vec::with_capacity((r2 - r1 + 1) as usize);
        for r in r1..=r2 {
            let mut row = Vec::with_capacity((c2 - c1 + 1) as usize);
            for c in c1..=c2 {
                let mut st = self.model().get_style_for_cell(sheet, r, c).map_err(|e| self.at(sheet, r, c, format!("cannot read its style ({e})")))?;
                for ch in &changes {
                    ch.apply(&mut st)?;
                }
                row.push(st);
            }
            styles.push(row);
        }
        self.calc(|um| -> Result<(), String> {
            // the paste goes to the selection, which must hold the selected cell
            um.set_selected_sheet(sheet)?;
            um.set_selected_cell(r1, c1)?;
            um.set_selected_range(r1, c1, r2, c2)?;
            um.on_paste_styles(&styles)
        })?;
        self.edited();
        Ok(())
    }

    /// Whole columns or rows: IronCalc's column and row styles, one property at a time.
    fn style_lines(&mut self, sheet: u32, (r1, c1, r2, c2): Bounds, cols: bool, changes: &[StyleChange]) -> Result<(), String> {
        // what IronCalc will touch: every cell in a row with data or a row
        // style for each column; every used cell and styled column per row
        let ws = self.model().workbook.worksheet(sheet)?;
        let cost = if cols {
            let lines = ws.sheet_data.rows().len() + ws.rows.len();
            (c2 - c1 + 1) as usize * lines.max(1)
        } else {
            let styled: usize = ws.cols.iter().filter(|c| c.style.is_some()).map(|c| (c.max - c.min + 1).max(0) as usize).sum();
            let used = self.used(sheet)?.map(|b| b.3 as usize).unwrap_or(0);
            (r2 - r1 + 1) as usize * (used + styled).max(1)
        };
        if cost.saturating_mul(changes.len()) > MAX_STYLE_CELLS {
            return Err(format!(
                "styling these whole {} touches about {} cells; one call is limited to {MAX_STYLE_CELLS} (fewer {}, or a cell range)",
                if cols { "columns" } else { "rows" },
                cost.saturating_mul(changes.len()),
                if cols { "columns" } else { "rows" },
            ));
        }
        let area = Area { sheet, row: r1, column: c1, width: c2 - c1 + 1, height: r2 - r1 + 1 };
        let paths: Vec<(&'static str, String)> = changes.iter().map(StyleChange::path).collect();
        self.calc(|um| -> Result<(), String> {
            for (k, (path, value)) in paths.iter().enumerate() {
                if let Err(e) = um.update_range_style(&area, path, value) {
                    for _ in 0..k {
                        let _ = um.undo();
                    }
                    return Err(e);
                }
            }
            Ok(())
        })?;
        self.edited();
        Ok(())
    }

    /// Undo the last edit; false if there was nothing to undo.
    pub fn undo(&mut self) -> Result<bool, String> {
        if !self.um.can_undo() {
            return Ok(false);
        }
        self.calc(|um| um.undo())?;
        self.edited();
        self.recount()?;
        Ok(true)
    }

    pub fn redo(&mut self) -> Result<bool, String> {
        if !self.um.can_redo() {
            return Ok(false);
        }
        self.calc(|um| um.redo())?;
        self.edited();
        self.recount()?;
        Ok(true)
    }

    /// Recalculate everything (volatile functions such as NOW and RAND change).
    pub fn recalc(&mut self) {
        self.calc(|um| um.evaluate());
        self.edited();
    }

    /// Drop the undo history (it grows with every edit).
    pub fn clear_history(&mut self) -> Result<(), String> {
        self.calc(|um| -> Result<(), String> {
            let bytes = um.to_bytes();
            let mut fresh = UserModel::from_bytes(&bytes, LANGUAGE)?;
            fresh.evaluate();
            // the old model is dropped here, on the large stack
            *um = fresh;
            Ok(())
        })?;
        self.edited();
        self.recount()
    }

    // ------------------------------------------------------------ export

    pub fn to_xlsx(&self) -> Result<Vec<u8>, String> {
        self.calc_ref(|um| {
            let out = ironcalc::export::save_xlsx_to_writer(um.get_model(), Cursor::new(Vec::new())).map_err(|e| format!("xlsx export failed: {e}"))?;
            Ok(out.into_inner())
        })
    }

    /// A read-only copy to export from without holding the lock: the cells
    /// and styles are copied, the parsed formulas shared (they never change
    /// in place). It has no undo history.
    pub fn snapshot(&self) -> Result<Book, String> {
        let m = self.model();
        let mut copy = Model::new_empty("snapshot", MODEL_LOCALE, DEFAULT_TIMEZONE, LANGUAGE)?;
        copy.workbook = m.workbook.clone();
        // the xlsx writer reads only the workbook and the parsed formulas
        copy.parsed_formulas = m.parsed_formulas.iter().map(|sheet| sheet.iter().map(|(node, s)| (node.clone(), copy_static(s))).collect()).collect();
        Ok(Book {
            um: ManuallyDrop::new(UserModel::from_model(copy)),
            edits: self.edits,
            locale: self.locale,
            locale_id: self.locale_id.clone(),
            language: self.language,
            walk: self.walk,
            walk_seen: self.walk_seen,
            names: self.names.clone(),
        })
    }
}

fn copy_static(s: &StaticResult) -> StaticResult {
    match s {
        StaticResult::Scalar => StaticResult::Scalar,
        StaticResult::Array(r, c) => StaticResult::Array(*r, *c),
        StaticResult::Range(r, c) => StaticResult::Range(*r, *c),
        StaticResult::Unknown => StaticResult::Unknown,
    }
}

fn check_cell(row: i32, col: i32) -> Result<(), String> {
    if !(1..=MAX_ROW).contains(&row) || !(1..=MAX_COL).contains(&col) {
        return Err(format!("row {row}, column {col} is outside the sheet"));
    }
    Ok(())
}

fn check_span(start: i32, count: i32, max: i32, what: &str) -> Result<(), String> {
    if !(1..=max).contains(&start) {
        return Err(format!("{what} {start} is outside the sheet (1 to {max})"));
    }
    if count < 1 || start as i64 + count as i64 - 1 > max as i64 {
        return Err(format!("{what} count {count} must be at least 1 and stay within the sheet"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn en() -> &'static Locale {
        locale_of("en").unwrap()
    }

    fn text_of(v: &Data) -> String {
        input_text(v, en(), false).unwrap().text
    }

    #[test]
    fn inputs() {
        assert_eq!(text_of(&Data::Num(0.1 + 0.2)), "0.30000000000000004");
        assert_eq!(text_of(&Data::Num(-0.0)), "0");
        assert!(input_text(&Data::Num(f64::NAN), en(), false).is_err());
        assert_eq!(text_of(&Data::Bool(true)), "TRUE");
        assert_eq!(text_of(&Data::Nil), "");
        assert_eq!(text_of(&error_data("#n/a")), "#N/A");
        assert!(input_text(&error_data("#BOOM"), en(), false).is_err());
        assert_eq!(text_of(&Data::map([("text", Data::str("=1"))])), "'=1");
        assert_eq!(text_of(&Data::map([("formula", Data::str("SUM(A1:A2)"))])), "=SUM(A1:A2)");
        assert_eq!(text_of(&Data::map([("date", Data::str("2024-01-15"))])), "2024-01-15");
        assert!(input_text(&Data::map([("date", Data::str("15/01/2024"))]), en(), false).is_err());
        assert!(input_text(&Data::bytes(vec![1]), en(), false).is_err());
        assert!(input_text(&Data::str(&"x".repeat(MAX_CELL_CHARS + 2)), en(), false).is_err());
        // formulas are measured
        assert!(input_text(&Data::str("=SUM(A1:A3)"), en(), false).unwrap().formula.is_some());
        assert!(input_text(&Data::str(&format!("={}1{}", "(".repeat(100), ")".repeat(100))), en(), false).unwrap_err().contains("levels"));
        assert!(input_text(&Data::str(&"-".repeat(5000)), en(), false).unwrap_err().contains("too deep"), "a run of minus signs is a formula");
        assert!(input_text(&Data::str("-5"), en(), false).unwrap().formula.is_none());
        // literal: strings stay text
        assert_eq!(input_text(&Data::str("00123"), en(), true).unwrap().text, "'00123");
        assert_eq!(input_text(&Data::str("=A1"), en(), true).unwrap().text, "'=A1");
        assert_eq!(input_text(&Data::Num(5.0), en(), true).unwrap().text, "5");
    }

    #[test]
    fn localized_numbers() {
        let de = locale_of("de").unwrap();
        for (typed, en_form) in [("1,5", "1.5"), ("-1.234,5", "-1234.5"), ("12,5%", "12.5%"), ("1.234", "1234"), ("7", "7")] {
            assert_eq!(delocalize(typed, de), en_form, "{typed}");
        }
        for same in ["abc", "1,2,3", "1.5", "2024-01-15", ",5", "1,"] {
            assert_eq!(delocalize(same, de), same, "{same}");
        }
        assert_eq!(delocalize("1,234.5", en()), "1,234.5", "English input is left to IronCalc");
    }

    #[test]
    fn formula_detection() {
        assert!(looks_like_formula("=1"));
        assert!(!looks_like_formula("="));
        assert!(looks_like_formula("-A1"));
        assert!(looks_like_formula("+x"));
        assert!(!looks_like_formula("-5"));
        assert!(!looks_like_formula("-1,234.5"));
        assert!(!looks_like_formula("+12%"));
        assert!(!looks_like_formula("text"));
    }

    #[test]
    fn dates() {
        assert_eq!(iso_date(45306.0).as_deref(), Some("2024-01-15"));
        assert_eq!(iso_date(45306.5625).as_deref(), Some("2024-01-15T13:30:00"));
        assert_eq!(iso_date(f64::NAN), None);
        assert_eq!(iso_date(-1.0), None);
    }

    #[test]
    fn sheet_names() {
        assert!(check_sheet_name("Data 2024").is_ok());
        assert!(check_sheet_name("資料").is_ok());
        for bad in ["", "a/b", "x[1]", "'q", &"n".repeat(32)] {
            assert!(check_sheet_name(bad).is_err(), "{bad}");
        }
    }
}
