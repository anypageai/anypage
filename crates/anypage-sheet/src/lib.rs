//! Spreadsheet module `sheet`: xlsx and CSV in and out, formulas,
//! recalculation, ranges as tables, undo. The engine underneath is
//! [IronCalc](https://www.ironcalc.com) 0.9 (pure Rust, MIT/Apache-2.0).
//!
//! A workbook is a resource of kind `sheet.workbook`. Reads lock it; edits
//! mutate it in place, recalculate, and return the same resource at a new
//! version (`res:next_version()` semantics), so every cell that read the old
//! version recomputes. Keep it in an input cell and replace it after edits:
//!
//! ```lua
//! local book = page.input("book", sheet.new())
//! page.on("key", function(k)
//!     page.set(book, sheet.set(page.get(book), "Sheet1!B2", 42))
//! end)
//! local total = page.derive("total", { book }, function(wb) return wb:get("B10") end)
//! ```
//!
//! References are A1 style: `"B2"`, `"$B$2"`, `"Sheet1!B2:D20"`,
//! `"'My sheet'!A:C"`, `"3:5"`; without a sheet name they mean the first
//! sheet. Sheets are names or 1-based indexes; nil is the first sheet.
//!
//! **Formulas are always English**, whatever the workbook's locale:
//! English function names, `,` between arguments, `.` for decimals
//! (`=ROUND(2.345,1)`), and `wb:formula` / `wb:input` return them that way.
//! The `locale` option (`"de"`, `"fr"`, ...) only changes how numbers are
//! *shown* (`wb:text`, `wb:range{text=true}`, `to_csv{formatted=true}`:
//! `1.234,50` in German) and how *typed* numbers are read (`"1,5"`,
//! `"1.234,5"`, `"12,5%"` in a German workbook, also in CSV imports).
//! Functions evaluate with English conventions (`TEXT` format codes, `VALUE`).
//!
//! Values: numbers, strings, booleans, nil (empty), errors as
//! `{ error = "#DIV/0!" }`. Dates are numbers (Excel serials); `wb:cell` adds
//! `text` (as shown) and `date` (ISO 8601). Writing, a string is typed into
//! the cell as a person would type it: `"=A1*2"` is a formula, `"12"` a
//! number, `"2024-01-15"` a date, `"TRUE"` a boolean, `"'12"` the text 12,
//! and (as in Excel) `"-A1"` is a formula. Typed forms: `{ text = "..." }`,
//! `{ formula = "SUM(A1:A3)" }`, `{ date = "YYYY-MM-DD" }`, `{ error = "#N/A" }`.
//!
//! Tables use the shared form `{ columns = { ... }, rows = { { v, ... }, ... } }`
//! (errors appear as their text there, e.g. `"#DIV/0!"`). Written back with
//! `sheet.set_range` a table is *literal*: its strings stay text (`"00123"`
//! does not become 123, `"=..."` does not become a formula).
//!
//! # Luau API
//!
//! | create (pure) | |
//! |---|---|
//! | `sheet.new{name, locale, timezone}` | empty workbook with `Sheet1` |
//! | `sheet.open(bytes[, {format, name, locale, timezone, ...}])` | xlsx (or CSV when the bytes are not a zip or `format = "csv"`), on the page's thread: up to 200 000 cells |
//! | `sheet.from_csv(text[, {delimiter, sheet, formulas, text, name, locale}])` | one sheet from CSV (up to 200 000 cells); delimiter guessed (`,` `;` tab `\|`) unless given; `=` fields (and `+`/`-` ones that are not numbers) stay text unless `formulas = true`; `text = true` keeps every field as text |
//! | job `sheet.load` | `page.job(name, "sheet.load", {bytes_cell}, {format, ..., into = wb})`: the same off the page's thread, for files up to the full limits, with progress and cancellation. Gives nil while the input is nil. With `into`, the loaded content replaces `wb`'s in place and the job's value is `wb` at a new version, so handlers holding `wb` in an input cell edit the opened file |
//!
//! | read (pure: usable in cell functions) | |
//! |---|---|
//! | `wb:sheets()`, `wb:name()` | sheet names; the workbook's name |
//! | `wb:dims([sheet])` | `{name, rows, cols, first_row, first_col, last_row, last_col, used}` (rows/cols count from A1; 0 when empty) |
//! | `wb:get(ref)` | computed value of one cell |
//! | `wb:text(ref)` | the value as shown (number format applied, display locale) |
//! | `wb:formula(ref)` | `"=SUM(A1:A3)"` or nil |
//! | `wb:input(ref)` | what typing would recreate the cell (formula or value text), for editors |
//! | `wb:cell(ref)` | `{ref, row, col, value, text, input, type, format, formula?, date?, message?, origin?}` |
//! | `wb:range(ref[, {header, text}])` | a table; `header = true` names columns from the first row; `text = true` adds `text` rows of shown strings |
//! | `wb:table([sheet][, {header, text}])` | the sheet's used range as a table, first row as header unless `header = false` |
//! | `wb:cells([sheet][, {formulas, limit}])` | non-empty cells `{ {ref, row, col, value, formula?} }` (only formulas with `formulas = true`) |
//! | `wb:style(ref)` | `{format, bold, italic, underline, strike, size, color?, fill?, align?, valign?, wrap}` (colours `"#RRGGBB"`) |
//! | `wb:describe()` | `{name, sheets = { {name, rows, cols, cells, formulas} }, edits, can_undo, can_redo, locale, timezone, walk}`; walks every cell, so keep it out of drawing that runs on every edit |
//! | `wb:can_undo()`, `wb:can_redo()` | |
//! | `wb:save()` | xlsx bytes |
//! | `wb:to_csv([sheet][, {delimiter, formatted, escape_formulas}])` | CSV text from A1 (CRLF; dates as ISO unless `formatted`) |
//! | job `sheet.export` | `{wb_cell}`, opts `{format = "xlsx" \| "csv", sheet, delimiter}` → bytes / text, built from a copy taken under the lock, so the page keeps reading and editing meanwhile |
//!
//! | edit (effects: load time and handlers only; each returns the workbook at its new version) | |
//! |---|---|
//! | `sheet.set(wb, ref, value)` | one cell, then recalculate |
//! | `sheet.set_range(wb, ref, rows[, {header, literal}])` | rows (list of lists, one flat list, or a table, header row first) from the reference's top-left; one undo step, one recalculation; nil clears; up to 25 000 cells. `literal = true` (the default for a table) keeps strings as text |
//! | `sheet.clear(wb, range)` | clear contents |
//! | `sheet.set_style(wb, range, {format, bold, italic, underline, strike, size, color, fill, align, valign, wrap})` | number formats (`"0.00%"`, `"$#,##0.00"`, `"yyyy-mm-dd"`) and looks; colours `"#RRGGBB"` or packed `0xRRGGBBAA`. A cell range (up to 25 000 cells) is one undo step; whole columns `"B:D"` and rows `"3:5"` get column/row styles (also for cells written later), one undo step per property |
//! | `sheet.add_sheet(wb[, name])`, `sheet.delete_sheet(wb, sheet)`, `sheet.rename_sheet(wb, sheet, name)` | |
//! | `sheet.insert_rows(wb, sheet, row[, count])` or `(wb, "Sheet1!3:5")`; `sheet.delete_rows` likewise | formulas adjust |
//! | `sheet.insert_columns(wb, sheet, col[, count])` (col a number or letters) or `(wb, "B:D")`; `sheet.delete_columns` likewise | |
//! | `sheet.undo(wb)`, `sheet.redo(wb)` | |
//! | `sheet.recalc(wb)` | recalculate everything (NOW, RAND) |
//! | `sheet.clear_history(wb)` | drop the undo history (it grows with every edit) |
//!
//! An edit checks everything before it changes anything: an edit that
//! returns an error left the workbook as it was. (The one exception is an
//! IronCalc failure in the middle of a paste, which its message names.)
//!
//! | helpers (pure) | |
//! |---|---|
//! | `sheet.col_name(28)` → `"AB"`, `sheet.col_index("AB")` → 28 | |
//! | `sheet.ref(row, col[, sheet])` → `"Sheet1!B2"` | sheet names are quoted when needed |
//! | `sheet.parse_ref(text)` → `{sheet?, row?, col?, last_row?, last_col?}` | |
//!
//! # Safety limits
//!
//! IronCalc recurses once per level of a formula to parse, evaluate, print
//! and drop it, has no depth limit, and cannot be interrupted. So:
//!
//! - every formula (typed, pasted, in a CSV, in an xlsx cell, conditional
//!   format or defined name) is measured first ([`formula`]): at most 8 192
//!   characters, 64 nested functions or parentheses, an expression tree at
//!   most 4 200 levels deep, no `LAMBDA` (a lambda can call itself without
//!   end through `LET` or a defined name), and at most 4 194 304 cells read
//!   per recalculation;
//! - all IronCalc work runs on a thread with a 256 MB stack *reservation*
//!   ([`deep`]), so the deepest formula that passes needs a few MB of it and
//!   the page's own stack is never used;
//! - an xlsx is measured before it is parsed ([`xlsx`]): file 128 MB (64 MB
//!   on phones), unpacked 256 MB (128 MB), 4 000 000 cells (1 500 000),
//!   1 000 000 formulas (400 000), merged ranges 2 000 000 cells, one array
//!   formula 1 000 000 cells, XML nesting 256, and the cells all formulas and
//!   conditional formats read per recalculation 100 000 000. A file with no
//!   sheets, duplicate sheet names or formats pointing nowhere is refused;
//! - reads hand at most 500 000 cells to a script; writes take 25 000 cells
//!   per call, styles 25 000 (each about half a second on a busy desktop:
//!   a handler may run 1 s); CSV text 64 MB, 2 000 000 cells in a job;
//!   32 767 characters per cell; 1 024 sheets.
//!
//! # Cost of whole columns
//!
//! Every edit recalculates the whole workbook. IronCalc clips whole columns
//! and rows to the used area only in some functions: `SUM`, `PRODUCT`,
//! `SUMIF(S)`, `COUNTIF(S)`, `AVERAGEIF(S)`, `MAXIFS`, `MINIFS`, `XLOOKUP`,
//! `XMATCH`, `INDEX`, `ROWS`, `COLUMNS`, `OFFSET`, `STDEV*`, `VAR*`,
//! `DEVSQ`, `GCD`, `LCM`, `TEXTJOIN` cost nothing extra. Elsewhere a whole
//! column is read cell by cell on *every* recalculation: measured on a
//! 20-cell sheet, `=COUNTA(A:A)` 0.3 to 1.2 s, `=AVERAGE(A:A)` 0.7 s,
//! `=MATCH(9,A:A,0)` 0.3 to 1.8 s, `=VLOOKUP(9,A:B,1,FALSE)` 0.3 s, `=A:A`
//! 1 s; a conditional format over a whole column about 0.5 s. A page
//! handler may run at most 1 s, so use bounded ranges (`A1:A1000`) with
//! those functions. `wb:describe().walk` estimates the cells one
//! recalculation reads; edits that would push it past 100 000 000 are
//! refused.

pub mod book;
pub mod csvio;
pub mod deep;
pub mod formula;
pub mod module;
pub mod refs;
pub mod xlsx;

pub use book::{Book, BookOptions, Workbook};
pub use csvio::{CsvIn, CsvOut};
pub use module::KIND;

use anypage_runtime::Natives;

/// Add the `sheet` module's functions and jobs.
pub fn register(n: &mut Natives) {
    module::register(n)
}
