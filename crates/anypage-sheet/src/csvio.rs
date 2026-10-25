//! CSV in and out, with RFC 4180 quoting (the `csv` crate).

use anypage_runtime::Data;
use ironcalc::base::formatter::lexer::is_likely_date_number_format;
use ironcalc::base::locale::Locale;

use crate::book::{cell_data, check_sheet_name, delocalize, empty_model, iso_date, locale_of, looks_like_formula, number_text, Book, BookOptions, Workbook, MAX_CELL_CHARS, MAX_IMPORT_CELLS};
use crate::deep::{deep, DeepDrop};
use crate::formula::{self, MAX_BOOK_WALK};
use crate::refs::{MAX_COL, MAX_ROW};

/// Largest CSV text accepted.
pub const MAX_CSV_BYTES: usize = 64 << 20;
/// Most cells `to_csv` writes in one call.
pub const MAX_EXPORT_CELLS: usize = 8_000_000;

/// How to read CSV text into a workbook.
#[derive(Clone, Debug)]
pub struct CsvIn {
    /// `None`: guess from the first line (`,` `;` tab `|`).
    pub delimiter: Option<u8>,
    /// Treat fields starting with `=` (or `+`/`-` followed by something that
    /// is not a number) as formulas. Off: they stay text, so a CSV cannot
    /// run formulas it was not trusted with.
    pub formulas: bool,
    /// Keep every field as text (no number, date or boolean detection).
    pub all_text: bool,
    /// Name of the sheet the data goes into.
    pub sheet: Option<String>,
    /// Most cells to read ([`MAX_IMPORT_CELLS`] by default; less on the page's thread).
    pub max_cells: usize,
}

impl Default for CsvIn {
    fn default() -> Self {
        Self { delimiter: None, formulas: false, all_text: false, sheet: None, max_cells: MAX_IMPORT_CELLS }
    }
}

/// How to write a sheet as CSV.
#[derive(Clone, Debug)]
pub struct CsvOut {
    pub delimiter: u8,
    /// Every cell as it is shown (number formats applied) instead of raw values.
    pub formatted: bool,
    /// Prefix text starting with = + - @ with `'` so spreadsheet apps opening
    /// the file do not run it as a formula.
    pub escape_formulas: bool,
}

impl Default for CsvOut {
    fn default() -> Self {
        Self { delimiter: b',', formatted: false, escape_formulas: false }
    }
}

/// "," ";" "\t" "tab" "|" → the byte; anything else is an error.
pub fn delimiter_of(d: &Data) -> Result<Option<u8>, String> {
    let Some(s) = d.as_str() else {
        return if d.is_nil() { Ok(None) } else { Err("delimiter must be a one-character string".into()) };
    };
    let b = match s {
        "tab" | "\t" => b'\t',
        _ if s.len() == 1 => s.as_bytes()[0],
        _ => return Err(format!("delimiter \"{s}\" must be one ASCII character (or \"tab\")")),
    };
    if !b.is_ascii() || matches!(b, b'"' | b'\n' | b'\r') || b.is_ascii_alphanumeric() {
        return Err(format!("delimiter \"{s}\" cannot be used"));
    }
    Ok(Some(b))
}

impl CsvIn {
    pub fn from_data(d: &Data) -> Result<Self, String> {
        Ok(Self {
            delimiter: delimiter_of(d.get("delimiter"))?,
            formulas: d.get("formulas").truthy(),
            all_text: d.get("text").truthy(),
            sheet: d.get("sheet").as_str().map(str::to_string),
            max_cells: MAX_IMPORT_CELLS,
        })
    }
}

impl CsvOut {
    pub fn from_data(d: &Data) -> Result<Self, String> {
        Ok(Self {
            delimiter: delimiter_of(d.get("delimiter"))?.unwrap_or(b','),
            formatted: d.get("formatted").truthy(),
            escape_formulas: d.get("escape_formulas").truthy(),
        })
    }
}

/// The most frequent of `,` `;` tab `|` on the first line outside quotes; `,` on ties.
pub fn sniff_delimiter(text: &str) -> u8 {
    const CANDIDATES: [u8; 4] = *b",;\t|";
    let mut counts = [0usize; 4];
    let mut quoted = false;
    for b in text.bytes().take(1 << 16) {
        match b {
            b'"' => quoted = !quoted,
            b'\n' | b'\r' if !quoted => break,
            _ if !quoted => {
                if let Some(k) = CANDIDATES.iter().position(|&c| c == b) {
                    counts[k] += 1;
                }
            }
            _ => {}
        }
    }
    let mut best = 0;
    for k in 1..4 {
        if counts[k] > counts[best] {
            best = k;
        }
    }
    CANDIDATES[best]
}

/// Rows being collected, with the limits checked as they grow.
struct Rows {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    field: String,
    /// The current field began with a quote (so it exists even if empty).
    quoted: bool,
    cells: usize,
    max_cells: usize,
}

impl Rows {
    fn end_field(&mut self) -> Result<(), String> {
        self.row.push(std::mem::take(&mut self.field));
        self.quoted = false;
        self.cells += 1;
        if self.row.len() > MAX_COL as usize {
            return Err(format!("CSV row {} has more than {MAX_COL} fields (a sheet's columns)", self.rows.len() + 1));
        }
        if self.cells > self.max_cells {
            return Err(format!("CSV has more than {} cells{}", self.max_cells, if self.max_cells < MAX_IMPORT_CELLS { " (open bigger files with the sheet.load job)" } else { "" }));
        }
        Ok(())
    }

    fn end_row(&mut self) -> Result<(), String> {
        // a blank line is an empty row, so later rows keep their positions
        if !self.row.is_empty() || !self.field.is_empty() || self.quoted {
            self.end_field()?;
        }
        self.rows.push(std::mem::take(&mut self.row));
        if self.rows.len() > MAX_ROW as usize {
            return Err(format!("CSV has more than {MAX_ROW} rows"));
        }
        Ok(())
    }
}

/// Parse CSV (RFC 4180: `"` quotes, `""` inside quotes, CRLF, LF or CR line
/// ends) into rows of fields. Blank lines are kept as empty rows so cells stay
/// where they were; a quoted field may span lines. Lenient like spreadsheet
/// apps: a stray quote inside an unquoted field is kept as a character and an
/// unclosed quote runs to the end of the text.
pub fn read(text: &str, delimiter: u8, max_cells: usize, cancelled: &dyn Fn() -> bool) -> Result<Vec<Vec<String>>, String> {
    if text.len() > MAX_CSV_BYTES {
        return Err(format!("CSV text is {} MB; the limit is {} MB", text.len() >> 20, MAX_CSV_BYTES >> 20));
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let delim = delimiter as char;
    let mut st = Rows { rows: Vec::new(), row: Vec::new(), field: String::new(), quoted: false, cells: 0, max_cells };
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    st.field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                st.field.push(ch);
            }
            continue;
        }
        match ch {
            '"' if st.field.is_empty() && !st.quoted => {
                in_quotes = true;
                st.quoted = true;
            }
            c if c == delim => st.end_field()?,
            '\r' | '\n' => {
                if ch == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                st.end_row()?;
                if st.rows.len().is_multiple_of(4096) && cancelled() {
                    return Err("cancelled".into());
                }
            }
            c => st.field.push(c),
        }
    }
    if !st.row.is_empty() || !st.field.is_empty() || st.quoted {
        st.end_row()?;
    }
    while st.rows.last().is_some_and(Vec::is_empty) {
        st.rows.pop();
    }
    Ok(st.rows)
}

/// What to type into a cell for one CSV field, and what it costs per
/// recalculation when it is a formula.
fn field_input(v: &str, o: &CsvIn, locale: &Locale) -> Result<(String, u64), String> {
    let formula = looks_like_formula(v);
    if o.all_text || v.starts_with('\'') || (formula && !o.formulas) {
        return Ok((format!("'{v}"), 0));
    }
    if formula {
        let s = formula::check(v)?;
        return Ok((v.to_string(), s.walk));
    }
    Ok((delocalize(v, locale).into_owned(), 0))
}

impl Workbook {
    /// A one-sheet workbook from CSV text. Numbers, dates and booleans are
    /// recognised as a person typing them would see them recognised (numbers
    /// the way the workbook's locale writes them).
    pub fn from_csv(text: &str, csv_opts: &CsvIn, o: &BookOptions, cancelled: &dyn Fn() -> bool, progress: &dyn Fn(f32)) -> Result<Workbook, String> {
        let locale = locale_of(&o.locale)?;
        let delimiter = csv_opts.delimiter.unwrap_or_else(|| sniff_delimiter(text));
        let rows = read(text, delimiter, csv_opts.max_cells, cancelled)?;
        progress(0.3);
        let mut model = empty_model(o)?;
        if let Some(name) = &csv_opts.sheet {
            check_sheet_name(name)?;
            model.rename_sheet("Sheet1", name)?;
        }
        // dropped on a large stack if anything below fails
        let mut model = DeepDrop::new(model);
        // every field is checked first: formulas measured, nothing written yet
        let mut inputs: Vec<Vec<String>> = Vec::with_capacity(rows.len());
        let mut walk = 0u64;
        for (i, row) in rows.iter().enumerate() {
            let mut out = Vec::with_capacity(row.len());
            for (j, v) in row.iter().enumerate() {
                if v.is_empty() {
                    out.push(String::new());
                    continue;
                }
                let (input, w) = field_input(v, csv_opts, locale).map_err(|e| format!("CSV row {}, field {}: {e}", i + 1, j + 1))?;
                if input.chars().count() > MAX_CELL_CHARS + 1 {
                    return Err(format!("CSV row {}, field {}: longer than {MAX_CELL_CHARS} characters", i + 1, j + 1));
                }
                walk = walk.saturating_add(w);
                out.push(input);
            }
            inputs.push(out);
        }
        if walk > MAX_BOOK_WALK {
            return Err(format!(
                "CSV formulas would read more than {MAX_BOOK_WALK} cells on every recalculation (whole columns outside SUM, COUNTIF and similar are read cell by cell)"
            ));
        }
        drop(rows);
        // written in chunks on a large stack (formulas parse recursively),
        // with cancellation and progress between chunks
        const CHUNK: usize = 4096;
        let total = inputs.len().max(1) as f32;
        for (k, chunk) in inputs.chunks(CHUNK).enumerate() {
            let first = (k * CHUNK) as i32 + 1;
            let Some(m) = model.get_mut() else { return Err("CSV import lost its workbook".into()) };
            deep(move || -> Result<(), String> {
                for (i, row) in chunk.iter().enumerate() {
                    for (j, input) in row.iter().enumerate() {
                        if !input.is_empty() {
                            m.set_user_input(0, first + i as i32, j as i32 + 1, input.clone())?;
                        }
                    }
                }
                Ok(())
            })?;
            if cancelled() {
                return Err("cancelled".into());
            }
            progress(0.3 + 0.6 * ((k + 1) * CHUNK) as f32 / total);
        }
        drop(inputs);
        let Some(model) = model.take() else { return Err("CSV import lost its workbook".into()) };
        let locale_id = o.locale.as_str();
        let mut book = deep(move || {
            let mut model = model;
            model.evaluate();
            Book::from_model(model, locale_id)
        })?;
        progress(1.0);
        book.set_walk_from_import(walk);
        Ok(Workbook::from_book(book))
    }
}

impl Book {
    /// One sheet as CSV text, from A1 to the last used row and column (CRLF line ends).
    pub fn to_csv(&self, sheet: u32, o: &CsvOut) -> Result<String, String> {
        let Some((_, _, r2, c2)) = self.used(sheet)? else { return Ok(String::new()) };
        let width = c2 as usize;
        if width.saturating_mul(r2 as usize) > MAX_EXPORT_CELLS {
            return Err(format!("sheet is too large to write as CSV ({r2} rows x {c2} columns; the limit is {MAX_EXPORT_CELLS} cells)"));
        }
        let m = self.model();
        let ws = m.workbook.worksheet(sheet)?;
        let mut w = csv::WriterBuilder::new().delimiter(o.delimiter).has_headers(false).terminator(csv::Terminator::CRLF).from_writer(Vec::new());
        let mut record = vec![String::new(); width];
        for r in 1..=r2 {
            record.iter_mut().for_each(String::clear);
            for (c, cell) in ws.sheet_data.cells_in_row_between(r, 1, c2) {
                let k = (c - 1) as usize;
                let v = cell_data(&cell, &m.workbook.shared_strings);
                record[k] = if o.formatted {
                    self.text(sheet, r, c)?
                } else {
                    match &v {
                        Data::Nil => String::new(),
                        Data::Num(n) => {
                            let fmt = m.get_style_for_cell(sheet, r, c)?.num_fmt;
                            match is_likely_date_number_format(&fmt).then(|| iso_date(*n)).flatten() {
                                Some(d) => d,
                                None => number_text(*n),
                            }
                        }
                        Data::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
                        Data::Str(s) => s.to_string(),
                        _ => v.get("error").as_str().unwrap_or("#ERROR!").to_string(),
                    }
                };
                if o.escape_formulas && matches!(v, Data::Str(_)) && record[k].starts_with(['=', '+', '-', '@']) {
                    record[k].insert(0, '\'');
                }
            }
            w.write_record(&record).map_err(|e| e.to_string())?;
        }
        let bytes = w.into_inner().map_err(|e| e.to_string())?;
        String::from_utf8(bytes).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never() -> bool {
        false
    }

    #[test]
    fn sniffing() {
        assert_eq!(sniff_delimiter("a,b,c\n1;2"), b',');
        assert_eq!(sniff_delimiter("a;b;c\n"), b';');
        assert_eq!(sniff_delimiter("a\tb\n"), b'\t');
        assert_eq!(sniff_delimiter("\"x;y;z\",b\n"), b',');
        assert_eq!(sniff_delimiter("single"), b',');
    }

    #[test]
    fn reading_keeps_blank_lines_and_quoted_newlines() {
        let rows = read("a,b\r\n\r\n\"multi\nline\",\"q\"\"uote\"\n\nlast", b',', MAX_IMPORT_CELLS, &never).unwrap();
        assert_eq!(rows.len(), 5, "{rows:?}");
        assert_eq!(rows[0], vec!["a", "b"]);
        assert!(rows[1].is_empty(), "{rows:?}");
        assert_eq!(rows[2], vec!["multi\nline", "q\"uote"]);
        assert!(rows[3].is_empty());
        assert_eq!(rows[4], vec!["last"]);
        assert_eq!(read("\u{feff}x,y", b',', MAX_IMPORT_CELLS, &never).unwrap()[0][0], "x");
    }

    #[test]
    fn delimiters() {
        assert_eq!(delimiter_of(&Data::str("tab")).unwrap(), Some(b'\t'));
        assert_eq!(delimiter_of(&Data::str(";")).unwrap(), Some(b';'));
        assert_eq!(delimiter_of(&Data::Nil).unwrap(), None);
        assert!(delimiter_of(&Data::str("\"")).is_err());
        assert!(delimiter_of(&Data::str("ab")).is_err());
        assert!(delimiter_of(&Data::Num(1.0)).is_err());
    }

    #[test]
    fn reading_edge_cases() {
        // empty quoted fields exist, trailing delimiter makes an empty field
        assert_eq!(read("\"\",x,\n", b',', MAX_IMPORT_CELLS, &never).unwrap(), vec![vec!["", "x", ""]]);
        // CR-only line ends, stray quote kept, unclosed quote runs to the end
        assert_eq!(read("a\rb\"c\r\"open,\nend", b',', MAX_IMPORT_CELLS, &never).unwrap(), vec![vec!["a".to_string()], vec!["b\"c".into()], vec!["open,\nend".into()]]);
        // semicolons, with commas inside fields
        assert_eq!(read("1,5;2\n", b';', MAX_IMPORT_CELLS, &never).unwrap(), vec![vec!["1,5", "2"]]);
        assert!(read("", b',', MAX_IMPORT_CELLS, &never).unwrap().is_empty());
        assert!(read("\n\n", b',', MAX_IMPORT_CELLS, &never).unwrap().is_empty());
        let wide = vec!["x"; MAX_COL as usize + 1].join(",");
        assert!(read(&wide, b',', MAX_IMPORT_CELLS, &never).unwrap_err().contains("fields"));
        assert_eq!(read("a\nb\nc", b',', MAX_IMPORT_CELLS, &|| true).unwrap().len(), 3, "cancellation is polled every 4096 rows");
        assert!(read("a,b\nc,d", b',', 3, &never).unwrap_err().contains("more than 3 cells"));
    }
}
