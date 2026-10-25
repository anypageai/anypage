//! Hostile and expensive input: formulas deep enough to overflow the stack,
//! recursive lambdas, xlsx files that are small but would hang or exhaust
//! memory in IronCalc, malformed files, whole-column ranges, and the
//! per-call caps. Every case must end in a clear error (or a bounded
//! result), never in an abort or a hang.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use anypage_runtime::Data;
use anypage_sheet::book::{MAX_STYLE_CELLS, MAX_WRITE_CELLS};
use anypage_sheet::formula::{MAX_BOOK_WALK, MAX_DEPTH, MAX_NESTING};
use anypage_sheet::refs::Span;
use anypage_sheet::{Book, BookOptions, Workbook};

fn never() -> bool {
    false
}

fn ignore(_: f32) {}

fn new_book() -> Workbook {
    Workbook::new(&BookOptions::default()).unwrap()
}

fn put(b: &mut Book, row: i32, col: i32, s: &str) -> Result<(), String> {
    b.set(0, row, col, &Data::str(s))
}

fn num(b: &Book, row: i32, col: i32) -> f64 {
    let v = b.value(0, row, col).unwrap();
    v.num().unwrap_or_else(|| panic!("R{row}C{col} is {v:?}"))
}

fn nested(open: &str, n: usize) -> String {
    format!("={}1{}", open.repeat(n), ")".repeat(n))
}

#[test]
fn deep_formulas_are_refused_or_evaluated_on_a_large_stack() {
    let w = new_book();
    let mut b = w.lock().unwrap();
    // at the limit: evaluates (and prints, and moves when rows are inserted)
    put(&mut b, 1, 1, &nested("(", MAX_NESTING)).unwrap();
    put(&mut b, 2, 1, &nested("ABS(", MAX_NESTING)).unwrap();
    assert_eq!(num(&b, 1, 1), 1.0);
    assert_eq!(num(&b, 2, 1), 1.0);
    // just past it: refused before IronCalc sees it, nothing changes
    let before = b.edits();
    for f in [nested("(", MAX_NESTING + 1), nested("ABS(", MAX_NESTING + 1), nested("(", 1000), nested("IF(TRUE,", 300)] {
        let e = put(&mut b, 3, 1, &f).unwrap_err();
        assert!(e.contains("levels deep"), "{e}");
    }
    assert_eq!(b.edits(), before);
    // the longest chain an 8 192-character formula holds is fine: about 8 MB
    // of stack to evaluate, far more than a test thread's 2 MB
    let chain = format!("=1{}", "+1".repeat(4095));
    put(&mut b, 4, 1, &chain).unwrap();
    assert_eq!(num(&b, 4, 1), 4096.0);
    let concat = format!("=\"\"{}", "&\"a\"".repeat(1500));
    put(&mut b, 5, 1, &concat).unwrap();
    assert_eq!(b.value(0, 5, 1).unwrap().as_str().map(str::len), Some(1500));
    // printing it back, moving it, undoing it and saving it all recurse too
    assert_eq!(b.formula(0, 4, 1).unwrap().map(|f| f.len()), Some(chain.len()));
    b.insert_rows(0, 1, 1).unwrap();
    assert_eq!(num(&b, 5, 1), 4096.0);
    assert!(b.undo().unwrap());
    let bytes = b.to_xlsx().unwrap();
    drop(b);
    let back = Workbook::open_xlsx(&bytes, &BookOptions::default(), &never, &ignore).unwrap();
    assert_eq!(num(&back.lock().unwrap(), 4, 1), 4096.0);
    // unary runs (IronCalc reads "-----1" as a formula even without "=")
    let mut b = w.lock().unwrap();
    for f in [format!("={}1", "-".repeat(MAX_DEPTH + 10)), format!("{}1", "-".repeat(MAX_DEPTH + 10)), format!("=1{}", "%".repeat(MAX_DEPTH + 10))] {
        let e = put(&mut b, 6, 1, &f).unwrap_err();
        assert!(e.contains("too deep"), "{e}");
    }
    put(&mut b, 6, 1, &format!("={}1", "-".repeat(400))).unwrap();
    assert_eq!(num(&b, 6, 1), 1.0, "an even number of minus signs");
    // the same limits on every way in: a paste, a CSV with formulas
    let rows = vec![vec![Data::str("ok"), Data::str(&nested("(", 500))]];
    let e = b.set_rows(0, 10, 1, &rows, false).unwrap_err();
    assert!(e.starts_with("B10:") && e.contains("levels deep"), "{e}");
    drop(b);
    let csv = format!("a,b\n1,\"{}\"\n", nested("(", 500).replace('"', "\"\""));
    let o = anypage_sheet::CsvIn { formulas: true, ..Default::default() };
    let e = Workbook::from_csv(&csv, &o, &BookOptions::default(), &never, &ignore).err().unwrap();
    assert!(e.contains("row 2, field 2") && e.contains("levels deep"), "{e}");
}

#[test]
fn everything_runs_from_a_small_stack() {
    // the main thread on Windows has 1 MB of stack and page scripts run on
    // it; with 512 KB here, any IronCalc recursion left on the caller's
    // stack would abort the test process
    let worker = std::thread::Builder::new().stack_size(512 << 10).spawn(|| {
        let w = new_book();
        {
            let mut b = w.lock().unwrap();
            put(&mut b, 1, 1, &nested("ABS(", MAX_NESTING)).unwrap();
            put(&mut b, 2, 1, &format!("=1{}", "+1".repeat(4095))).unwrap();
            put(&mut b, 3, 1, &format!("={}1", "-".repeat(4000))).unwrap();
            assert_eq!(num(&b, 2, 1), 4096.0);
            assert_eq!(num(&b, 3, 1), 1.0);
            assert!(b.formula(0, 2, 1).unwrap().is_some());
            assert!(b.input(0, 3, 1).unwrap().starts_with("=--"));
            b.cell_info(0, 2, 1).unwrap();
            assert_eq!(b.cells(0, true, 10).unwrap().items().len(), 3);
            b.insert_rows(0, 1, 2).unwrap();
            assert!(b.undo().unwrap());
            b.clear_history().unwrap();
        }
        let bytes = w.lock().unwrap().to_xlsx().unwrap();
        let snap = w.lock().unwrap().snapshot().unwrap();
        assert_eq!(snap.to_xlsx().unwrap().len(), bytes.len());
        drop(snap);
        drop(w);
        let back = Workbook::open_xlsx(&bytes, &BookOptions::default(), &never, &ignore).unwrap();
        assert_eq!(num(&back.lock().unwrap(), 2, 1), 4096.0);
        drop(back);
        let csv = format!("\"=1{}\"\n", "+1".repeat(3000));
        let o = anypage_sheet::CsvIn { formulas: true, ..Default::default() };
        let c = Workbook::from_csv(&csv, &o, &BookOptions::default(), &never, &ignore).unwrap();
        assert_eq!(num(&c.lock().unwrap(), 1, 1), 3001.0);
    });
    assert!(worker.unwrap().join().is_ok());
}

#[test]
fn lambda_is_refused() {
    let w = new_book();
    let mut b = w.lock().unwrap();
    // these recurse without end in IronCalc (3 000 levels already overflow 1 MB)
    for f in ["=LET(f,LAMBDA(k,IF(k=0,0,1+f(k-1))),f(100000))", "=LAMBDA(f,n,f(f,n))(LAMBDA(g,k,IF(k=0,0,1+g(g,k-1))),100000)"] {
        let e = put(&mut b, 1, 1, f).unwrap_err();
        assert!(e.contains("LAMBDA is not supported"), "{e}");
    }
    // a CSV cell that starts with '-' is a formula only when formulas are allowed
    drop(b);
    let csv = "x\n\"-LAMBDA(a,a)(1)\"\n";
    let wb = Workbook::from_csv(csv, &Default::default(), &BookOptions::default(), &never, &ignore).unwrap();
    assert_eq!(wb.lock().unwrap().value(0, 2, 1).unwrap(), Data::str("-LAMBDA(a,a)(1)"), "kept as text");
}

// ---------------------------------------------------------------- crafted xlsx files

/// A real xlsx (IronCalc's own output) with one part rewritten.
fn xlsx_with(part: &str, edit: impl Fn(String) -> String) -> Vec<u8> {
    let w = new_book();
    {
        let mut b = w.lock().unwrap();
        b.set_rows(0, 1, 1, &[vec![Data::Num(1.0), Data::str("=A1+1")]], false).unwrap();
    }
    let bytes = w.lock().unwrap().to_xlsx().unwrap();
    let mut zin = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut zout = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut found = false;
    for i in 0..zin.len() {
        let mut f = zin.by_index(i).unwrap();
        let name = f.name().unwrap().to_string();
        let mut data = Vec::new();
        f.read_to_end(&mut data).unwrap();
        if name == part {
            data = edit(String::from_utf8(data).unwrap()).into_bytes();
            found = true;
        }
        zout.start_file(&name, opts).unwrap();
        zout.write_all(&data).unwrap();
    }
    assert!(found, "no part {part}");
    zout.finish().unwrap().into_inner()
}

const SHEET: &str = "xl/worksheets/sheet1.xml";
const BOOK: &str = "xl/workbook.xml";

/// Insert `xml` after the element that starts with `after` (its end tag or self-closing end).
fn insert_after(doc: String, after: &str, xml: &str) -> String {
    let start = doc.find(after).unwrap_or_else(|| panic!("no {after} in {doc}"));
    let end = start + doc[start..].find('>').unwrap() + 1;
    let end = if doc[start..end].ends_with("/>") {
        end
    } else {
        let tag = after.trim_start_matches('<');
        let close = format!("</{tag}>");
        end + doc[end..].find(&close).map(|k| k + close.len()).unwrap_or(0)
    };
    format!("{}{xml}{}", &doc[..end], &doc[end..])
}

/// Replace the first `<f>...</f>` of the sheet.
fn with_formula(f_xml: &str) -> Vec<u8> {
    let f_xml = f_xml.to_string();
    xlsx_with(SHEET, move |doc| {
        let a = doc.find("<f>").unwrap();
        let b = doc[a..].find("</f>").unwrap() + a + 4;
        format!("{}{f_xml}{}", &doc[..a], &doc[b..])
    })
}

fn open(bytes: &[u8]) -> Result<Workbook, String> {
    Workbook::open_xlsx(bytes, &BookOptions::default(), &never, &ignore)
}

/// Opening must fail quickly with a message containing `why`.
fn refused(bytes: &[u8], why: &str) {
    let t = Instant::now();
    let e = open(bytes).err().unwrap_or_else(|| panic!("opened; expected refusal with \"{why}\""));
    assert!(e.contains(why), "expected \"{why}\" in: {e}");
    assert!(t.elapsed() < Duration::from_secs(5), "refusing took {:?}", t.elapsed());
}

#[test]
fn crafted_xlsx_files_are_refused_before_they_hang_or_crash() {
    // the untouched file opens and computes
    let plain = with_formula("<f>A1+1</f>");
    assert_eq!(num(&open(&plain).unwrap().lock().unwrap(), 1, 2), 2.0);
    // a formula 1 000 parentheses deep (a stack overflow in IronCalc's parser)
    refused(&with_formula(&format!("<f>{}1{}</f>", "(".repeat(1000), ")".repeat(1000))), "levels deep");
    // a formula longer than Excel allows, and one that streams on and on
    refused(&with_formula(&format!("<f>1{}</f>", "+1".repeat(5000))), "longer than 8192");
    refused(&with_formula(&format!("<f>\"{}\"</f>", "x".repeat(200_000))), "longer than 8192");
    // an array formula over the whole sheet (IronCalc allocates every cell)
    refused(&with_formula("<f t=\"array\" ref=\"B1:XFD1048576\">1</f>"), "array formula covering");
    // a merged range over the whole sheet (IronCalc loops over every cell)
    refused(&xlsx_with(SHEET, |d| insert_after(d, "<sheetData", "<mergeCells count=\"1\"><mergeCell ref=\"A1:XFD1048576\"/></mergeCells>")), "merged ranges cover");
    // conditional formatting over the whole sheet (evaluated per cell, every recalculation)
    let cf = "<conditionalFormatting sqref=\"A1:XFD1048576\"><cfRule type=\"expression\" dxfId=\"0\" priority=\"1\"><formula>A1&gt;5</formula></cfRule></conditionalFormatting>";
    refused(&xlsx_with(SHEET, move |d| insert_after(d, "<sheetData", cf)), "on every recalculation");
    // a formula that reads the whole sheet on every recalculation
    refused(&with_formula("<f>COUNTA(A:XFD)</f>"), "on every recalculation");
    // ... or a modest one shared by thousands of cells
    let shared = {
        let mut cells = String::from("<row r=\"2\">");
        cells.push_str("<c r=\"A2\"><f t=\"shared\" ref=\"A2:A200\" si=\"0\">COUNTA(B:E)</f></c>");
        for r in 3..=200 {
            cells.push_str(&format!("<c r=\"A{r}\"><f t=\"shared\" si=\"0\"/></c>"));
        }
        cells.push_str("</row>");
        cells
    };
    refused(&xlsx_with(SHEET, move |d| d.replace("</sheetData>", &format!("{shared}</sheetData>"))), "on every recalculation");
    // a recursive LAMBDA in a defined name
    let names = "<definedNames><definedName name=\"F\">LAMBDA(n,IF(n=0,0,1+F(n-1)))</definedName></definedNames>";
    refused(&xlsx_with(BOOK, move |d| insert_after(d, "<sheets", names)), "LAMBDA is not supported");
    // XML nested deeper than any real file (dropping the tree would recurse)
    let deep_xml = format!("{}{}", "<x>".repeat(5000), "</x>".repeat(5000));
    refused(&xlsx_with(SHEET, move |d| d.replace("</sheetData>", &format!("</sheetData><extLst>{deep_xml}</extLst>"))), "nested more than");
    // a defined name for a whole column, read in full many times
    let names = "<definedNames><definedName name=\"Big\">Sheet1!$A:$Z</definedName></definedNames>";
    let many = {
        let mut cells = String::new();
        for r in 2..=10 {
            cells.push_str(&format!("<row r=\"{r}\"><c r=\"C{r}\"><f>COUNTA(Big)</f></c></row>"));
        }
        cells
    };
    let named = xlsx_with(BOOK, move |d| insert_after(d, "<sheets", names));
    let mut zin = zip::ZipArchive::new(std::io::Cursor::new(named.clone())).unwrap();
    let mut sheet = String::new();
    zin.by_name(SHEET).unwrap().read_to_string(&mut sheet).unwrap();
    assert!(sheet.contains("</sheetData>"));
    let rebuilt = {
        let mut zout = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        for i in 0..zin.len() {
            let mut f = zin.by_index(i).unwrap();
            let name = f.name().unwrap().to_string();
            let mut data = Vec::new();
            f.read_to_end(&mut data).unwrap();
            if name == SHEET {
                data = sheet.replace("</sheetData>", &format!("{many}</sheetData>")).into_bytes();
            }
            zout.start_file(&name, opts).unwrap();
            zout.write_all(&data).unwrap();
        }
        zout.finish().unwrap().into_inner()
    };
    refused(&rebuilt, "on every recalculation");
}

#[test]
fn malformed_xlsx_files_are_refused_with_a_reason() {
    // no sheets at all
    refused(&xlsx_with(BOOK, |d| {
        let a = d.find("<sheets>").unwrap();
        let b = d.find("</sheets>").unwrap() + "</sheets>".len();
        format!("{}<sheets/>{}", &d[..a], &d[b..])
    }), "no sheets");
    // a cell pointing at a format that does not exist (IronCalc would panic on reading it)
    refused(&xlsx_with(SHEET, |d| d.replacen("<c r=\"A1\"", "<c r=\"A1\" s=\"99999\"", 1)), "uses format 99999");
    // broken XML
    refused(&xlsx_with(SHEET, |d| d.replace("</sheetData>", "</sheetDta>")), "not a readable xlsx");
}

#[test]
fn small_files_still_open_quickly_and_the_scan_counts_them() {
    let w = new_book();
    {
        let mut b = w.lock().unwrap();
        let rows: Vec<Vec<Data>> = (1..=1000).map(|r| vec![Data::Num(r as f64), Data::str(&format!("=A{r}*2")), Data::str("=COUNTA(A:A)")]).collect();
        b.set_rows(0, 1, 1, &rows[..1], false).unwrap();
        b.set_rows(0, 2, 1, &rows[1..].iter().map(|r| r[..2].to_vec()).collect::<Vec<_>>(), false).unwrap();
    }
    let bytes = w.lock().unwrap().to_xlsx().unwrap();
    let report = anypage_sheet::xlsx::check_xlsx(&bytes, &never).unwrap();
    assert_eq!(report.formulas, 1001);
    assert_eq!(report.walk, 1_048_576, "COUNTA(A:A) reads a whole column");
    let opened = open(&bytes).unwrap();
    let b = opened.lock().unwrap();
    assert_eq!(b.walk(), 1_048_576);
    assert_eq!(num(&b, 1, 3), 1000.0);
}

// ---------------------------------------------------------------- whole columns and the budget

#[test]
fn whole_column_formulas_cost_and_budget() {
    let w = new_book();
    let mut b = w.lock().unwrap();
    for r in 1..=20 {
        put(&mut b, r, 1, &r.to_string()).unwrap();
    }
    // SUM clips a whole column to the used area: free
    let t = Instant::now();
    put(&mut b, 1, 3, "=SUM(A:A)").unwrap();
    let sum_ms = t.elapsed().as_secs_f64() * 1e3;
    assert_eq!(num(&b, 1, 3), 210.0);
    assert_eq!(b.walk(), 0);
    // COUNTA reads all 1 048 576 cells of the column on every recalculation
    let t = Instant::now();
    put(&mut b, 2, 3, "=COUNTA(A:A)").unwrap();
    let counta_ms = t.elapsed().as_secs_f64() * 1e3;
    assert_eq!(num(&b, 2, 3), 20.0);
    assert_eq!(b.walk(), 1_048_576);
    // and so does every later edit, anywhere
    let t = Instant::now();
    put(&mut b, 30, 30, "x").unwrap();
    let edit_ms = t.elapsed().as_secs_f64() * 1e3;
    eprintln!("=SUM(A:A) {sum_ms:.1} ms; =COUNTA(A:A) {counta_ms:.0} ms; an unrelated edit with COUNTA(A:A) in the book {edit_ms:.0} ms");
    assert!(counta_ms < 30_000.0 && edit_ms < 30_000.0);
    // typing the same formula again does not count it twice
    put(&mut b, 2, 3, "=COUNTA(A:A)").unwrap();
    assert_eq!(b.walk(), 1_048_576);
    // a formula reading more than four whole columns is refused outright
    let e = put(&mut b, 3, 3, "=COUNTA(A:XFD)").unwrap_err();
    assert!(e.contains("reads about") && e.contains("SUM, COUNTIF"), "{e}");
    // many heavy formulas in one write are refused before anything changes
    let heavy: Vec<Vec<Data>> = (0..30).map(|_| vec![Data::str("=COUNTA(B:E)")]).collect();
    let before = b.edits();
    let e = b.set_rows(0, 1, 10, &heavy, false).unwrap_err();
    assert!(e.contains(&MAX_BOOK_WALK.to_string()), "{e}");
    assert_eq!(b.edits(), before);
    // clearing the formula gives the budget back; undo brings it back
    b.clear(0, (2, 3, 2, 3)).unwrap();
    assert_eq!(b.walk(), 0);
    assert!(b.undo().unwrap());
    assert_eq!(b.walk(), 1_048_576);
    assert_eq!(b.describe().unwrap().get("walk"), &Data::Num(1_048_576.0));
}

// ---------------------------------------------------------------- per-call caps

#[test]
fn styles_on_whole_columns_and_the_style_cap() {
    let w = new_book();
    let mut b = w.lock().unwrap();
    for r in 1..=10 {
        put(&mut b, r, 2, &r.to_string()).unwrap();
    }
    // a whole column gets a column style: cells written later have it too
    b.set_style(0, &Span::Cols { c1: 2, c2: 2 }, &Data::map([("bold", Data::Bool(true)), ("format", Data::str("0.00"))])).unwrap();
    assert_eq!(b.style(0, 5, 2).unwrap().get("bold"), &Data::Bool(true));
    assert_eq!(b.style(0, 500_000, 2).unwrap().get("bold"), &Data::Bool(true), "an empty cell far down");
    put(&mut b, 600, 2, "7").unwrap();
    assert_eq!(b.text(0, 600, 2).unwrap(), "7.00");
    assert_eq!(b.style(0, 5, 3).unwrap().get("bold"), &Data::Bool(false), "the next column is untouched");
    // whole rows likewise
    let t = Instant::now();
    b.set_style(0, &Span::Rows { r1: 3, r2: 4 }, &Data::map([("fill", Data::str("#336699"))])).unwrap();
    assert!(t.elapsed() < Duration::from_secs(5));
    assert_eq!(b.style(0, 3, 9000).unwrap().get("fill"), &Data::str("#336699"));
    // a cell area is capped
    let e = b.set_style(0, &Span::Area { r1: 1, c1: 1, r2: 10_000, c2: 26 }, &Data::map([("bold", Data::Bool(true))])).unwrap_err();
    assert!(e.contains(&MAX_STYLE_CELLS.to_string()), "{e}");
    // the whole sheet is too many column styles times rows with data
    let e = b.set_style(0, &Span::Cols { c1: 1, c2: 16_384 }, &Data::map([("bold", Data::Bool(true))])).unwrap_err();
    assert!(e.contains("limited"), "{e}");
    // at the cap it is done in well under a second
    let rows_at_cap = (MAX_STYLE_CELLS / 4) as i32;
    let t = Instant::now();
    b.set_style(0, &Span::Area { r1: 1, c1: 5, r2: rows_at_cap, c2: 8 }, &Data::map([("bold", Data::Bool(true)), ("size", Data::Num(12.0)), ("fill", Data::str("#112233"))])).unwrap();
    let style_ms = t.elapsed().as_secs_f64() * 1e3;
    assert_eq!(b.style(0, rows_at_cap, 8).unwrap().get("fill"), &Data::str("#112233"));
    assert!(b.undo().unwrap());
    assert_eq!(b.style(0, rows_at_cap, 8).unwrap().get("fill"), &Data::Nil, "one undo step for all three properties");
    // writes are capped too, and a write at the cap is quick
    let rows: Vec<Vec<Data>> = (0..=MAX_WRITE_CELLS / 10).map(|_| vec![Data::Num(1.0); 10]).collect();
    let e = b.set_rows(0, 1, 1, &rows, false).unwrap_err();
    assert!(e.contains(&MAX_WRITE_CELLS.to_string()) && e.contains("sheet.load"), "{e}");
    let rows: Vec<Vec<Data>> = (1..=MAX_WRITE_CELLS / 10)
        .map(|r| (0..10).map(|c| if c == 9 { Data::str(&format!("=SUM(A{r}:I{r})")) } else { Data::Num((r * c) as f64) }).collect())
        .collect();
    let t = Instant::now();
    b.set_rows(0, 1, 20, &rows, false).unwrap();
    let write_ms = t.elapsed().as_secs_f64() * 1e3;
    eprintln!("at the caps: set_style {MAX_STYLE_CELLS} cells x 3 properties {style_ms:.0} ms; set_range {MAX_WRITE_CELLS} cells (10% formulas) {write_ms:.0} ms");
    assert!(style_ms < 20_000.0 && write_ms < 20_000.0);
}

#[test]
fn literal_writes_keep_text_as_text() {
    let w = new_book();
    let mut b = w.lock().unwrap();
    let row = vec![Data::str("00123"), Data::str("2024-01-15"), Data::str("TRUE"), Data::str("'lead"), Data::str("=1+1"), Data::str("-A1"), Data::Num(5.0), Data::Bool(true), Data::Nil];
    b.set_rows(0, 1, 1, std::slice::from_ref(&row), true).unwrap();
    let got = b.table(0, (1, 1, 1, 9), false, false).unwrap();
    let expect = Data::list(vec![
        Data::str("00123"),
        Data::str("2024-01-15"),
        Data::str("TRUE"),
        Data::str("'lead"),
        Data::str("=1+1"),
        Data::str("-A1"),
        Data::Num(5.0),
        Data::Bool(true),
        Data::Nil,
    ]);
    assert_eq!(got.get("rows").at(1), &expect);
    assert_eq!(b.formula(0, 1, 5).unwrap(), None, "no formula was made");
    // the same row typed: numbers, dates, booleans and formulas
    b.set_rows(0, 2, 1, std::slice::from_ref(&row), false).unwrap();
    assert_eq!(b.value(0, 2, 1).unwrap(), Data::Num(123.0));
    assert_eq!(b.value(0, 2, 5).unwrap(), Data::Num(2.0));
    // a table read back and written again is unchanged
    let t = b.table(0, (1, 1, 1, 9), false, false).unwrap();
    let rows: Vec<Vec<Data>> = t.get("rows").items().iter().map(|r| r.items().to_vec()).collect();
    b.set_rows(0, 5, 1, &rows, true).unwrap();
    assert_eq!(b.table(0, (5, 1, 5, 9), false, false).unwrap().get("rows"), t.get("rows"));
}

#[test]
fn export_snapshot_matches_and_releases_the_lock() {
    let w = new_book();
    {
        let mut b = w.lock().unwrap();
        let rows: Vec<Vec<Data>> = (1..=2000).map(|r| vec![Data::Num(r as f64), Data::str(&format!("=A{r}*2")), Data::str("text")]).collect();
        b.set_rows(0, 1, 1, &rows, false).unwrap();
        b.set_style(0, &Span::Area { r1: 1, c1: 1, r2: 1, c2: 3 }, &Data::map([("bold", Data::Bool(true))])).unwrap();
    }
    let t = Instant::now();
    let snap = w.lock().unwrap().snapshot().unwrap();
    let snap_ms = t.elapsed().as_secs_f64() * 1e3;
    // the live workbook can be edited while the copy is written
    {
        let mut b = w.lock().unwrap();
        put(&mut b, 1, 1, "100").unwrap();
    }
    let from_snapshot = snap.to_xlsx().unwrap();
    drop(snap);
    let reopened = open(&from_snapshot).unwrap();
    let r = reopened.lock().unwrap();
    assert_eq!(num(&r, 1, 2), 2.0, "the copy is the workbook as it was");
    assert_eq!(num(&r, 2000, 2), 4000.0);
    assert_eq!(r.formula(0, 2000, 2).unwrap().as_deref(), Some("=A2000*2"));
    assert_eq!(r.style(0, 1, 3).unwrap().get("bold"), &Data::Bool(true));
    assert_eq!(num(&w.lock().unwrap(), 1, 2), 200.0);
    eprintln!("snapshot of 6 000 cells: {snap_ms:.1} ms");
}

#[test]
fn a_loaded_workbook_can_replace_one_in_place() {
    let target = new_book();
    put(&mut target.lock().unwrap(), 1, 1, "old").unwrap();
    let before = target.lock().unwrap().edits();
    let loaded = new_book();
    put(&mut loaded.lock().unwrap(), 1, 1, "new").unwrap();
    let edits = target.replace_with(loaded).unwrap();
    assert!(edits > before);
    let b = target.lock().unwrap();
    assert_eq!(b.value(0, 1, 1).unwrap(), Data::str("new"));
    assert_eq!(b.edits(), edits);
}
