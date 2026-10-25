//! The workbook over IronCalc through the Rust API: formulas, functions,
//! structure edits, undo, xlsx and CSV round trips, limits and timings.

use std::io::Write;
use std::time::Instant;

use anypage_runtime::Data;
use anypage_sheet::book::{error_data, Bounds};
use anypage_sheet::refs::Span;
use anypage_sheet::xlsx::{check_xlsx_with, XlsxLimits};
use anypage_sheet::{Book, BookOptions, CsvIn, CsvOut, Workbook};

fn new_book() -> Workbook {
    Workbook::new(&BookOptions::default()).unwrap()
}

/// Shorthands for the edit calls these tests make most.
trait Edits {
    fn write_rows(&mut self, sheet: u32, row: i32, col: i32, rows: &[Vec<Data>]) -> Result<Bounds, String>;
    fn style_area(&mut self, sheet: u32, b: Bounds, props: &Data) -> Result<(), String>;
}

impl Edits for Book {
    fn write_rows(&mut self, sheet: u32, row: i32, col: i32, rows: &[Vec<Data>]) -> Result<Bounds, String> {
        self.set_rows(sheet, row, col, rows, false)
    }

    fn style_area(&mut self, sheet: u32, (r1, c1, r2, c2): Bounds, props: &Data) -> Result<(), String> {
        self.set_style(sheet, &Span::Area { r1, c1, r2, c2 }, props)
    }
}

fn never() -> bool {
    false
}

fn ignore(_: f32) {}

fn at(b: &Book, r: &str) -> (u32, i32, i32) {
    let (s, span) = b.resolve(r).unwrap();
    let (row, col) = span.origin();
    (s, row, col)
}

fn set(b: &mut Book, r: &str, v: Data) {
    let (s, row, col) = at(b, r);
    b.set(s, row, col, &v).unwrap_or_else(|e| panic!("set {r}: {e}"));
}

fn put(b: &mut Book, r: &str, s: &str) {
    set(b, r, Data::str(s));
}

fn get(b: &Book, r: &str) -> Data {
    let (s, row, col) = at(b, r);
    b.value(s, row, col).unwrap()
}

fn num(b: &Book, r: &str) -> f64 {
    get(b, r).num().unwrap_or_else(|| panic!("{r} is {:?}", get(b, r)))
}

fn formula(b: &Book, r: &str) -> Option<String> {
    let (s, row, col) = at(b, r);
    b.formula(s, row, col).unwrap()
}

fn info(b: &Book, r: &str) -> Data {
    let (s, row, col) = at(b, r);
    b.cell_info(s, row, col).unwrap()
}

fn rows(v: Vec<Vec<Data>>) -> Vec<Vec<Data>> {
    v
}

fn n(x: f64) -> Data {
    Data::Num(x)
}

fn s(x: &str) -> Data {
    Data::str(x)
}

#[test]
fn formulas_across_sheets() {
    let w = new_book();
    let mut b = w.lock().unwrap();
    assert_eq!(b.add_sheet(Some("Data")).unwrap(), 1);
    assert!(b.add_sheet(Some("data")).unwrap_err().contains("already"));
    b.write_rows(1, 1, 1, &rows(vec![vec![n(1.0), s("x")], vec![n(2.0), s("y")], vec![n(3.0), s("z")]])).unwrap();
    put(&mut b, "Sheet1!A1", "=SUM(Data!A1:A3)");
    put(&mut b, "A2", "=Data!A2*10");
    put(&mut b, "A3", "=Data!B3&\"!\"");
    assert_eq!(num(&b, "A1"), 6.0);
    assert_eq!(num(&b, "A2"), 20.0);
    assert_eq!(get(&b, "A3"), s("z!"));
    set(&mut b, "Data!A2", n(5.0));
    assert_eq!(num(&b, "Sheet1!A1"), 9.0, "recalculated after an edit on another sheet");
    assert_eq!(num(&b, "A2"), 50.0);

    b.rename_sheet(1, "My Data").unwrap();
    assert_eq!(formula(&b, "A1").as_deref(), Some("=SUM('My Data'!A1:A3)"));
    assert_eq!(num(&b, "A1"), 9.0);
    assert_eq!(b.sheet_names(), vec!["Sheet1", "My Data"]);

    b.delete_sheet(1).unwrap();
    assert_eq!(get(&b, "A1"), error_data("#REF!"));
    assert!(b.delete_sheet(0).unwrap_err().contains("at least one"));
    assert!(b.undo().unwrap());
    assert_eq!(num(&b, "A1"), 9.0, "undo brings the sheet and the values back");
    assert!(b.edits() >= 8);
}

#[test]
fn functions_values_and_typing() {
    let w = new_book();
    let mut b = w.lock().unwrap();
    b.write_rows(0, 1, 1, &rows((1..=5).map(|k| vec![n(10.0 * k as f64)]).collect())).unwrap();
    b.write_rows(0, 1, 4, &rows(vec![vec![s("apple"), n(1.5)], vec![s("banana"), n(0.25)], vec![s("cherry"), n(4.0)]])).unwrap();
    let cases: &[(&str, &str)] = &[
        ("B1", "=SUM(A1:A5)"),
        ("B2", "=AVERAGE(A1:A5)"),
        ("B3", "=IF(B1>100,\"big\",\"small\")"),
        ("B4", "=VLOOKUP(\"banana\",D1:E3,2,FALSE)"),
        ("B5", "=XLOOKUP(\"cherry\",D1:D3,E1:E3)"),
        ("B6", "=DATE(2024,1,15)"),
        ("B7", "=TEXT(B6,\"yyyy-mm-dd\")"),
        ("B8", "=TEXT(1234.5,\"#,##0.00\")"),
        ("B9", "=1/0"),
        ("B10", "=VLOOKUP(\"kiwi\",D1:E3,2,FALSE)"),
        ("B11", "=YEAR(B6)"),
        ("B12", "=B6+31"),
        ("B13", "=IFERROR(B9,-1)"),
        ("B14", "=A1&\"x\""),
        ("B15", "=SUMIF(A1:A5,\">25\")"),
        ("B16", "=COUNTIF(D1:D3,\"*an*\")"),
        ("B17", "=ROUND(2/3,3)"),
        ("B18", "=AND(A1>5,NOT(A2>100))"),
        ("B19", "=NOSUCHFUNCTION(1)"),
    ];
    for (r, f) in cases {
        put(&mut b, r, f);
    }
    assert_eq!(num(&b, "B1"), 150.0);
    assert_eq!(num(&b, "B2"), 30.0);
    assert_eq!(get(&b, "B3"), s("big"));
    assert_eq!(num(&b, "B4"), 0.25);
    assert_eq!(num(&b, "B5"), 4.0);
    assert_eq!(num(&b, "B6"), 45306.0);
    assert_eq!(info(&b, "B6").get("date").as_str(), Some("2024-01-15"), "{:?}", info(&b, "B6"));
    assert_eq!(get(&b, "B7"), s("2024-01-15"));
    assert_eq!(get(&b, "B8"), s("1,234.50"));
    assert_eq!(get(&b, "B9"), error_data("#DIV/0!"));
    let i9 = info(&b, "B9");
    assert_eq!(i9.get("type").as_str(), Some("error"));
    assert_eq!(i9.get("formula").as_str(), Some("=1/0"));
    assert_eq!(get(&b, "B10"), error_data("#N/A"));
    assert_eq!(num(&b, "B11"), 2024.0);
    assert_eq!(info(&b, "B12").get("date").as_str(), Some("2024-02-15"));
    assert_eq!(num(&b, "B13"), -1.0);
    assert_eq!(get(&b, "B14"), s("10x"));
    assert_eq!(num(&b, "B15"), 120.0);
    assert_eq!(num(&b, "B16"), 1.0);
    assert_eq!(num(&b, "B17"), 0.667);
    assert_eq!(get(&b, "B18"), Data::Bool(true));
    assert_eq!(get(&b, "B19"), error_data("#NAME?"));

    // typing semantics
    put(&mut b, "C1", "2024-03-01");
    assert_eq!(num(&b, "C1"), 45352.0);
    assert_eq!(info(&b, "C1").get("date").as_str(), Some("2024-03-01"));
    put(&mut b, "C2", "TRUE");
    assert_eq!(get(&b, "C2"), Data::Bool(true));
    put(&mut b, "C3", "'00123");
    assert_eq!(get(&b, "C3"), s("00123"));
    put(&mut b, "C4", "12%");
    assert!((num(&b, "C4") - 0.12).abs() < 1e-12);
    assert_eq!(info(&b, "C4").get("text").as_str(), Some("12%"));
    set(&mut b, "C5", Data::map([("text", s("=not a formula"))]));
    assert_eq!(get(&b, "C5"), s("=not a formula"));
    set(&mut b, "C6", Data::map([("date", s("2024-12-31"))]));
    assert_eq!(info(&b, "C6").get("date").as_str(), Some("2024-12-31"));
    set(&mut b, "C7", error_data("#N/A"));
    assert_eq!(get(&b, "C7"), error_data("#N/A"));
    set(&mut b, "C8", n(0.1 + 0.2));
    assert_eq!(num(&b, "C8"), 0.1 + 0.2, "numbers are stored exactly");
    set(&mut b, "C9", n(-1.5e-7));
    assert_eq!(num(&b, "C9"), -1.5e-7);
    set(&mut b, "C10", n(1.0e300));
    assert_eq!(num(&b, "C10"), 1.0e300);
    set(&mut b, "C11", s("混合 مرحبا 🙂"));
    assert_eq!(get(&b, "C11"), s("混合 مرحبا 🙂"));
    set(&mut b, "C11", Data::Nil);
    assert_eq!(get(&b, "C11"), Data::Nil);
    // what an editor shows for a cell
    let (sh, r, c) = at(&b, "C3");
    assert_eq!(b.input(sh, r, c).unwrap(), "'00123");
    let (sh, r, c) = at(&b, "B1");
    assert_eq!(b.input(sh, r, c).unwrap(), "=SUM(A1:A5)");

    // bad input is refused with a reason, and changes nothing
    let (sh, r, c) = at(&b, "D9");
    assert!(b.set(sh, r, c, &Data::Num(f64::INFINITY)).unwrap_err().contains("finite"));
    assert!(b.set(sh, r, c, &Data::bytes(vec![1])).unwrap_err().contains("cell value"));
    assert!(b.set(sh, 0, 1, &Data::Num(1.0)).unwrap_err().contains("outside"));
    assert!(b.resolve("Nope!A1").unwrap_err().contains("no sheet named"));
}

#[test]
fn ranges_tables_undo_and_structure() {
    let w = new_book();
    let mut b = w.lock().unwrap();
    b.write_rows(0, 1, 1, &rows(vec![vec![s("name"), s("qty")], vec![s("a"), n(2.0)], vec![s("b"), n(3.0)], vec![s("total"), s("=SUM(B2:B3)")]])).unwrap();
    let edits = b.edits();
    assert_eq!(edits, 1, "one write, one edit");
    let t = b.table(0, (1, 1, 4, 2), false, false).unwrap();
    assert_eq!(t.get("columns"), &Data::list(vec![s("A"), s("B")]));
    assert_eq!(t.get("rows").at(4).at(2), &n(5.0));
    let t = b.table(0, b.used(0).unwrap().unwrap(), true, true).unwrap();
    assert_eq!(t.get("columns"), &Data::list(vec![s("name"), s("qty")]));
    assert_eq!(t.get("rows").items().len(), 3);
    assert_eq!(t.get("text").at(3).at(2), &s("5"));

    // one undo step reverts the whole write
    assert!(b.undo().unwrap());
    assert_eq!(b.used(0).unwrap(), None);
    assert!(b.redo().unwrap());
    assert_eq!(num(&b, "B4"), 5.0);
    assert!(!b.redo().unwrap(), "nothing left to redo");

    // rows and columns: formulas adjust
    b.insert_rows(0, 3, 2).unwrap();
    assert_eq!(formula(&b, "B6").as_deref(), Some("=SUM(B2:B5)"));
    assert_eq!(num(&b, "B6"), 5.0);
    b.delete_rows(0, 3, 2).unwrap();
    assert_eq!(formula(&b, "B4").as_deref(), Some("=SUM(B2:B3)"));
    b.insert_columns(0, 1, 1).unwrap();
    assert_eq!(formula(&b, "C4").as_deref(), Some("=SUM(C2:C3)"));
    b.delete_columns(0, 1, 1).unwrap();
    assert_eq!(get(&b, "A2"), s("a"));
    assert!(b.insert_rows(0, 0, 1).is_err());
    assert!(b.delete_rows(0, 1_048_576, 2).is_err());

    // clear, cells listing, describe
    b.clear(0, (2, 1, 2, 2)).unwrap();
    assert_eq!(get(&b, "A2"), Data::Nil);
    assert_eq!(num(&b, "B4"), 3.0);
    let cells = b.cells(0, true, 100).unwrap();
    assert_eq!(cells.items().len(), 1);
    assert_eq!(cells.at(1).get("ref"), &s("B4"));
    let d = b.describe().unwrap();
    assert_eq!(d.get("sheets").at(1).get("formulas"), &n(1.0));
    assert_eq!(d.get("can_undo"), &Data::Bool(true));
    b.clear_history().unwrap();
    assert!(!b.can_undo());
    assert_eq!(num(&b, "B4"), 3.0);

    // writes that do not fit are refused before anything changes
    let before = b.edits();
    assert!(b.write_rows(0, 1_048_576, 1, &rows(vec![vec![n(1.0)], vec![n(2.0)]])).unwrap_err().contains("past the last row"));
    assert!(b.write_rows(0, 1, 1, &rows(vec![vec![Data::bytes(vec![0])]])).unwrap_err().starts_with("A1:"));
    assert!(b.style_area(0, (1, 1, 1_048_575, 16_384), &Data::map([("bold", Data::Bool(true))])).unwrap_err().contains("limited"));
    assert!(b.style_area(0, (1, 1, 1_000, 1_000), &Data::map([("bold", Data::Bool(true))])).unwrap_err().contains("limited"));
    assert_eq!(b.edits(), before);
    // clearing the whole sheet only visits what is used
    let t = Instant::now();
    b.clear(0, (1, 1, 1_048_576, 16_384)).unwrap();
    assert!(t.elapsed().as_secs_f64() < 2.0, "clear took {:?}", t.elapsed());
    assert_eq!(b.used(0).unwrap(), None);
}

#[test]
fn locales_and_timezones() {
    let de = BookOptions { locale: "de".into(), ..BookOptions::default() };
    let w = Workbook::new(&de).unwrap();
    let mut b = w.lock().unwrap();
    // numbers from scripts are exact whatever the locale's decimal separator
    set(&mut b, "A1", n(1.5));
    set(&mut b, "A2", n(1234.25));
    assert_eq!(num(&b, "A1"), 1.5);
    assert_eq!(num(&b, "A2"), 1234.25);
    put(&mut b, "A3", "=A1+A2");
    assert_eq!(num(&b, "A3"), 1235.75);
    let d = b.describe().unwrap();
    assert_eq!(d.get("locale"), &s("de"));

    // formulas stay English whatever the locale: comma separators, '.' decimals
    put(&mut b, "B1", "=SUM(1,2)");
    assert_eq!(num(&b, "B1"), 3.0, "not 1.2");
    put(&mut b, "B2", "=IF(TRUE,\"a\",\"b\")");
    assert_eq!(get(&b, "B2"), s("a"));
    put(&mut b, "B3", "=ROUND(2.345,1)");
    assert_eq!(num(&b, "B3"), 2.3);
    assert_eq!(formula(&b, "B3").as_deref(), Some("=ROUND(2.345,1)"));
    // the formula text reads back and parses again
    let again = formula(&b, "B3").unwrap();
    put(&mut b, "B4", &again);
    assert_eq!(num(&b, "B4"), 2.3);
    b.write_rows(0, 10, 1, &rows(vec![vec![n(4.0), n(5.0), s("=SUM(A10,B10)")]])).unwrap();
    assert_eq!(num(&b, "C10"), 9.0);
    // the locale is for display and typed numbers
    b.style_area(0, (2, 1, 2, 1), &Data::map([("format", s("#,##0.00"))])).unwrap();
    assert_eq!(b.text(0, 2, 1).unwrap(), "1.234,25");
    put(&mut b, "D1", "1,5");
    assert_eq!(num(&b, "D1"), 1.5, "typed with a decimal comma");
    put(&mut b, "D2", "-1.234,5");
    assert_eq!(num(&b, "D2"), -1234.5);
    put(&mut b, "D3", "12,5%");
    assert!((num(&b, "D3") - 0.125).abs() < 1e-12);
    assert_eq!(b.input(0, 1, 2).unwrap(), "=SUM(1,2)", "the editor shows English formulas");
    drop(b);
    // CSV from a German spreadsheet: ';' and decimal commas
    let de_csv = Workbook::from_csv("Wert;Anteil\n1,5;12,5%\n1.234,5;x\n", &CsvIn::default(), &de, &never, &ignore).unwrap();
    let c = de_csv.lock().unwrap();
    assert_eq!(num(&c, "A2"), 1.5);
    assert_eq!(num(&c, "A3"), 1234.5);
    assert!((num(&c, "B2") - 0.125).abs() < 1e-12);
    assert_eq!(c.to_csv(0, &CsvOut { delimiter: b';', formatted: true, escape_formulas: false }).unwrap(), "Wert;Anteil\r\n1,5;12,50%\r\n1234,5;x\r\n");
    drop(c);
    for bad in [BookOptions { locale: "xx-nope".into(), ..BookOptions::default() }, BookOptions { timezone: "Mars/Base".into(), ..BookOptions::default() }] {
        assert!(Workbook::new(&bad).is_err(), "{bad:?}");
    }
    let tw = Workbook::new(&BookOptions { timezone: "Asia/Taipei".into(), ..BookOptions::default() }).unwrap();
    assert_eq!(tw.lock().unwrap().describe().unwrap().get("timezone"), &s("Asia/Taipei"));
}

#[test]
fn styles_and_number_formats() {
    let w = new_book();
    let mut b = w.lock().unwrap();
    b.write_rows(0, 1, 1, &rows(vec![vec![n(0.125), n(45306.0), n(1234.5)]])).unwrap();
    let props = Data::map([("format", s("0.0%")), ("bold", Data::Bool(true)), ("fill", n(0x336699FF_u32 as f64)), ("color", s("#ff0000")), ("align", s("center"))]);
    b.style_area(0, (1, 1, 1, 1), &props).unwrap();
    b.style_area(0, (1, 2, 1, 2), &Data::map([("format", s("yyyy-mm-dd"))])).unwrap();
    b.style_area(0, (1, 3, 1, 3), &Data::map([("format", s("$#,##0.00")), ("size", n(14.0))])).unwrap();
    assert_eq!(b.text(0, 1, 1).unwrap(), "12.5%");
    assert_eq!(b.text(0, 1, 2).unwrap(), "2024-01-15");
    assert_eq!(b.text(0, 1, 3).unwrap(), "$1,234.50");
    let st = b.style(0, 1, 1).unwrap();
    assert_eq!(st.get("format"), &s("0.0%"));
    assert_eq!(st.get("bold"), &Data::Bool(true));
    assert_eq!(st.get("fill"), &s("#336699"));
    assert_eq!(st.get("color"), &s("#FF0000"));
    assert_eq!(st.get("align"), &s("center"));
    assert_eq!(b.style(0, 1, 3).unwrap().get("size"), &n(14.0));
    assert_eq!(b.style(0, 1, 2).unwrap().get("bold"), &Data::Bool(false));
    // one call is one undo step, whatever the number of properties
    assert!(b.undo().unwrap());
    let st = b.style(0, 1, 3).unwrap();
    assert_ne!(st.get("size"), &n(14.0), "the size change is undone");
    assert_eq!(st.get("format"), &s("general"), "and the format change with it");
    assert!(b.redo().unwrap());
    assert_eq!(b.text(0, 1, 3).unwrap(), "$1,234.50");
    // bad properties are refused before anything changes
    let before = b.edits();
    assert!(b.style_area(0, (1, 1, 1, 1), &Data::map([("bolder", Data::Bool(true))])).unwrap_err().contains("unknown style property"));
    assert!(b.style_area(0, (1, 1, 1, 1), &Data::map([("fill", s("red"))])).unwrap_err().contains("#RRGGBB"));
    assert!(b.style_area(0, (1, 1, 1, 1), &Data::map([("size", n(0.5))])).is_err());
    assert_eq!(b.edits(), before);
    // formats survive xlsx
    drop(b);
    let bytes = w.lock().unwrap().to_xlsx().unwrap();
    let back = Workbook::open_xlsx(&bytes, &BookOptions::default(), &never, &ignore).unwrap();
    let b = back.lock().unwrap();
    assert_eq!(b.text(0, 1, 1).unwrap(), "12.5%");
    assert_eq!(b.style(0, 1, 1).unwrap().get("fill"), &s("#336699"));
}

#[test]
fn xlsx_round_trip() {
    let w = new_book();
    {
        let mut b = w.lock().unwrap();
        b.add_sheet(Some("Data 2")).unwrap();
        b.write_rows(
            0,
            1,
            1,
            &rows(vec![
                vec![n(0.1 + 0.2), n(-1.5e-7), n(1.0e15), s("混合 مرحبا 🙂"), Data::Bool(true)],
                vec![s("2024-01-15"), s("12.5%"), s("'007"), s("=A1*2"), s("='Data 2'!A1+1")],
                vec![s("=1/0"), s("=SUM(A1:C1)"), s("=TEXT(A2,\"yyyy-mm-dd\")"), s("line1\nline2"), Data::Bool(false)],
            ]),
        )
        .unwrap();
        b.write_rows(1, 1, 1, &rows(vec![vec![n(41.0), s("=Sheet1!A1*10")]])).unwrap();
    }
    let refs = ["A1", "B1", "C1", "D1", "E1", "A2", "B2", "C2", "D2", "E2", "A3", "B3", "C3", "D3", "E3", "'Data 2'!A1", "'Data 2'!B1"];
    let snapshot = |w: &Workbook| -> Vec<(Data, Option<String>, String)> {
        let b = w.lock().unwrap();
        refs.iter()
            .map(|r| {
                let (s, row, col) = at(&b, r);
                (b.value(s, row, col).unwrap(), b.formula(s, row, col).unwrap(), b.text(s, row, col).unwrap())
            })
            .collect()
    };
    let original = snapshot(&w);
    assert_eq!(original[9].0, n(42.0), "cross-sheet formula before saving");
    let bytes = w.lock().unwrap().to_xlsx().unwrap();
    assert!(bytes.starts_with(b"PK"));
    std::fs::create_dir_all("target/test-output").unwrap();
    std::fs::write("target/test-output/round-trip.xlsx", &bytes).unwrap();

    let opened = Workbook::open_xlsx(&bytes, &BookOptions::default(), &never, &ignore).unwrap();
    let again = snapshot(&opened);
    for (k, (a, b)) in original.iter().zip(&again).enumerate() {
        assert_eq!(a, b, "{} differs after save and open", refs[k]);
    }
    assert_eq!(opened.lock().unwrap().sheet_names(), vec!["Sheet1", "Data 2"]);
    // edits survive a second round trip too
    {
        let mut b = opened.lock().unwrap();
        put(&mut b, "'Data 2'!A1", "100");
        assert_eq!(num(&b, "E2"), 101.0);
    }
    let bytes2 = opened.lock().unwrap().to_xlsx().unwrap();
    let third = Workbook::open_xlsx(&bytes2, &BookOptions::default(), &never, &ignore).unwrap();
    assert_eq!(num(&third.lock().unwrap(), "E2"), 101.0);

    // what is not an xlsx is refused with a reason
    for (bad, why) in [(&b"hello"[..], "not an xlsx"), (&bytes[..bytes.len() / 2], "not a readable xlsx"), (&b"PK\x03\x04garbage"[..], "not a readable xlsx")] {
        let e = Workbook::open_xlsx(bad, &BookOptions::default(), &never, &ignore).err().unwrap();
        assert!(e.contains(why), "{e}");
    }
    // a cancelled load stops
    let e = Workbook::open_xlsx(&bytes, &BookOptions::default(), &|| true, &ignore).err().unwrap();
    assert_eq!(e, "cancelled");
}

#[test]
fn zip_bombs_are_measured_before_parsing() {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("xl/worksheets/sheet1.xml", opts).unwrap();
    let zeros = vec![b' '; 1 << 20];
    for _ in 0..3 {
        zip.write_all(&zeros).unwrap();
    }
    let bytes = zip.finish().unwrap().into_inner();
    assert!(bytes.len() < 64 << 10, "3 MB of spaces compress to {} bytes", bytes.len());
    let small = XlsxLimits { max_unpacked: 2 << 20, ..XlsxLimits::default() };
    let e = check_xlsx_with(&bytes, &small, &never).unwrap_err();
    assert!(e.contains("unpacks to more than 2 MB"), "{e}");
    let roomy = XlsxLimits { max_unpacked: 4 << 20, ..XlsxLimits::default() };
    assert_eq!(check_xlsx_with(&bytes, &roomy, &never).unwrap().unpacked, 3 << 20);
}

#[test]
fn csv_import_and_export_with_quoting() {
    let text = "name,qty,price,when,flag,note\r\n\"Smith, John\",3,\"1,234.50\",2024-01-15,TRUE,\"said \"\"hi\"\"\nthen left\"\r\n,,,,,\r\n=1+1,'007,-5,12%,false,🙂\r\n";
    let w = Workbook::from_csv(text, &CsvIn::default(), &BookOptions::default(), &never, &ignore).unwrap();
    let b = w.lock().unwrap();
    assert_eq!(get(&b, "A2"), s("Smith, John"));
    assert_eq!(num(&b, "B2"), 3.0);
    assert_eq!(num(&b, "C2"), 1234.5);
    assert_eq!(num(&b, "D2"), 45306.0);
    assert_eq!(get(&b, "E2"), Data::Bool(true));
    assert_eq!(get(&b, "F2"), s("said \"hi\"\nthen left"));
    assert_eq!(get(&b, "A3"), Data::Nil, "the blank line stays a blank row");
    assert_eq!(get(&b, "A4"), s("=1+1"), "formulas stay text unless asked for");
    assert_eq!(get(&b, "B4"), s("'007"), "a leading apostrophe is kept");
    assert_eq!(num(&b, "C4"), -5.0);
    assert!((num(&b, "D4") - 0.12).abs() < 1e-12);
    assert_eq!(get(&b, "E4"), Data::Bool(false));
    assert_eq!(get(&b, "F4"), s("🙂"));

    let out = b.to_csv(0, &CsvOut::default()).unwrap();
    let expected = "name,qty,price,when,flag,note\r\n\"Smith, John\",3,1234.5,2024-01-15,TRUE,\"said \"\"hi\"\"\nthen left\"\r\n,,,,,\r\n=1+1,'007,-5,0.12,FALSE,🙂\r\n";
    assert_eq!(out, expected);
    // and reading what we wrote gives the same cells
    let back = Workbook::from_csv(&out, &CsvIn::default(), &BookOptions::default(), &never, &ignore).unwrap();
    let t1 = b.table(0, b.used(0).unwrap().unwrap(), false, false).unwrap();
    let bb = back.lock().unwrap();
    let t2 = bb.table(0, bb.used(0).unwrap().unwrap(), false, false).unwrap();
    assert_eq!(t1, t2);

    let shown = b.to_csv(0, &CsvOut { delimiter: b';', formatted: true, escape_formulas: true }).unwrap();
    assert!(shown.contains("Smith, John;3;1,234.50;"), "{shown}");
    assert!(shown.contains("'=1+1;'007;-5;12%;FALSE;"), "{shown}");

    // options: formulas, all text, a named sheet, sniffed delimiter
    let o = CsvIn { formulas: true, sheet: Some("Imported".into()), ..CsvIn::default() };
    let w = Workbook::from_csv("a;b\n1;=A2*3\n", &o, &BookOptions::default(), &never, &ignore).unwrap();
    let b = w.lock().unwrap();
    assert_eq!(b.sheet_names(), vec!["Imported"]);
    assert_eq!(num(&b, "B2"), 3.0);
    let o = CsvIn { all_text: true, ..CsvIn::default() };
    let w = Workbook::from_csv("1,TRUE,2024-01-15\n", &o, &BookOptions::default(), &never, &ignore).unwrap();
    assert_eq!(get(&w.lock().unwrap(), "A1"), s("1"));
    assert_eq!(get(&w.lock().unwrap(), "C1"), s("2024-01-15"));
    // empty input is an empty workbook
    let w = Workbook::from_csv("", &CsvIn::default(), &BookOptions::default(), &never, &ignore).unwrap();
    assert_eq!(w.lock().unwrap().to_csv(0, &CsvOut::default()).unwrap(), "");
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// 10 000 rows x 10 columns: 80 000 numbers, a row sum per row and a running
/// total chained through all rows (20 000 formulas, a 10 000-deep chain).
#[test]
fn hundred_thousand_cells() {
    const ROWS: usize = 10_000;
    let data: Vec<Vec<Data>> = (1..=ROWS)
        .map(|r| {
            let mut row: Vec<Data> = (1..=8).map(|c| n((r * c) as f64 * 0.5)).collect();
            row.push(s(&format!("=SUM(A{r}:H{r})")));
            row.push(s(&if r == 1 { "=I1".to_string() } else { format!("=J{}+I{r}", r - 1) }));
            row
        })
        .collect();
    let row_sum = |r: usize| (1..=8).map(|c| (r * c) as f64 * 0.5).sum::<f64>();
    let grand: f64 = (1..=ROWS).map(row_sum).sum();

    let w = new_book();
    let t = Instant::now();
    // one write is limited to MAX_WRITE_CELLS: several writes of whole rows
    let per_write = anypage_sheet::book::MAX_WRITE_CELLS / 10;
    for (k, chunk) in data.chunks(per_write).enumerate() {
        w.lock().unwrap().write_rows(0, (k * per_write) as i32 + 1, 1, chunk).unwrap();
    }
    let build_ms = ms(t);
    {
        let b = w.lock().unwrap();
        assert_eq!(num(&b, "I10000"), row_sum(ROWS));
        assert!((num(&b, "J10000") - grand).abs() < 1e-6 * grand);
    }
    let t = Instant::now();
    let bytes = w.lock().unwrap().to_xlsx().unwrap();
    let save_ms = ms(t);

    let t = Instant::now();
    let opened = Workbook::open_xlsx(&bytes, &BookOptions::default(), &never, &ignore).unwrap();
    let open_ms = ms(t);
    let mut b = opened.lock().unwrap();
    assert!((num(&b, "J10000") - grand).abs() < 1e-6 * grand);

    let t = Instant::now();
    put(&mut b, "A1", "1000.5");
    let edit_ms = ms(t);
    assert!((num(&b, "J10000") - (grand + 1000.0)).abs() < 1e-6 * grand, "an edit at the top reaches the end of the chain");

    let t = Instant::now();
    let table = b.table(0, b.used(0).unwrap().unwrap(), false, false).unwrap();
    let read_ms = ms(t);
    assert_eq!(table.get("rows").items().len(), ROWS);

    let t = Instant::now();
    let csv = b.to_csv(0, &CsvOut::default()).unwrap();
    let to_csv_ms = ms(t);
    let t = Instant::now();
    let from = Workbook::from_csv(&csv, &CsvIn::default(), &BookOptions::default(), &never, &ignore).unwrap();
    let from_csv_ms = ms(t);
    assert_eq!(from.lock().unwrap().describe().unwrap().get("sheets").at(1).get("cells"), &n((ROWS * 10) as f64));

    eprintln!(
        "100 000 cells (20 000 formulas): build+recalc {build_ms:.0} ms, save xlsx {save_ms:.0} ms ({} KB), open+recalc {open_ms:.0} ms, \
         one edit+recalc {edit_ms:.0} ms, read as table {read_ms:.0} ms, to CSV {to_csv_ms:.0} ms ({} KB), from CSV {from_csv_ms:.0} ms",
        bytes.len() / 1024,
        csv.len() / 1024
    );
    // generous bounds: this is a smoke check for pathologies, not a benchmark
    for (what, t) in [("build", build_ms), ("save", save_ms), ("open", open_ms), ("edit", edit_ms), ("read", read_ms), ("to_csv", to_csv_ms), ("from_csv", from_csv_ms)] {
        assert!(t < 60_000.0, "{what} took {t:.0} ms");
    }
}
