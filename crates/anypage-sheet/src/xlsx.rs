//! What an xlsx file will cost, measured before IronCalc reads it.
//!
//! A small xlsx can be a bomb in three ways, and IronCalc's import offers no
//! way to stop it once it has started:
//!
//! - **size**: deflate packs a gigabyte of spaces into a megabyte;
//! - **areas**: IronCalc loops over every cell of a merged range and makes a
//!   map entry for every cell an array formula covers, so
//!   `<mergeCell ref="A1:XFD1048576"/>` (17 billion cells) runs for hours and
//!   `<f t="array" ref="A1:XFD1048576">` eats memory until the process dies;
//!   conditional formats are evaluated over their whole range on every
//!   recalculation (a whole column costs about 0.5 s);
//! - **formulas**: a formula nested a thousand levels deep overflows the
//!   stack (an abort, not an error), and `COUNTA(A:XFD)` or a recursive
//!   `LAMBDA` never finishes.
//!
//! [`check_xlsx`] decompresses every part into a streaming XML reader (no
//! tree is built, nothing is kept) and refuses the file when a limit is
//! passed: unpacked bytes, XML nesting, cells, formulas, merged and array
//! areas, and the cells all formulas and conditional formats read per
//! recalculation (see [`crate::formula`]). Every formula text goes through
//! [`crate::formula::check`]. `<dimension>` and `<col min max>` spans are not
//! limited: IronCalc stores them and never walks them.

use std::cell::Cell as StdCell;
use std::collections::HashMap;
use std::io::{BufReader, Cursor, Read};

use quick_xml::events::{BytesStart, Event};
use quick_xml::XmlVersion;

use crate::formula::{self, MAX_FORMULA_CHARS};
use crate::refs::{self, Span, MAX_COL, MAX_ROW};

/// Limits for [`check_xlsx_with`]. The defaults are smaller on phones.
#[derive(Clone, Debug)]
pub struct XlsxLimits {
    /// The file itself.
    pub max_bytes: usize,
    /// Everything it unpacks to.
    pub max_unpacked: u64,
    pub max_parts: usize,
    /// Cells written in the sheets plus cells covered by array formulas.
    pub max_cells: u64,
    /// Cells holding a formula.
    pub max_formulas: u64,
    /// Cells covered by merged ranges.
    pub max_merged: u64,
    /// Cells one array formula may cover.
    pub max_array: u64,
    /// Cells all formulas and conditional formats read per recalculation.
    pub max_walk: u64,
    /// XML element nesting in any part.
    pub max_xml_depth: usize,
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
impl Default for XlsxLimits {
    fn default() -> Self {
        Self {
            max_bytes: 128 << 20,
            max_unpacked: 256 << 20,
            max_parts: 20_000,
            max_cells: 4_000_000,
            max_formulas: 1_000_000,
            max_merged: 2_000_000,
            max_array: 1_000_000,
            max_walk: formula::MAX_BOOK_WALK,
            max_xml_depth: 256,
        }
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
impl Default for XlsxLimits {
    fn default() -> Self {
        Self {
            max_bytes: 64 << 20,
            max_unpacked: 128 << 20,
            max_parts: 10_000,
            max_cells: 1_500_000,
            max_formulas: 400_000,
            max_merged: 1_000_000,
            max_array: 500_000,
            max_walk: formula::MAX_BOOK_WALK,
            max_xml_depth: 256,
        }
    }
}

/// What the scan counted in a file it accepted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct XlsxReport {
    pub unpacked: u64,
    pub cells: u64,
    pub formulas: u64,
    pub merged: u64,
    /// Estimated cells read per recalculation (formulas and conditional formats).
    pub walk: u64,
}

/// [`check_xlsx_with`] the default limits.
pub fn check_xlsx(bytes: &[u8], cancelled: &dyn Fn() -> bool) -> Result<XlsxReport, String> {
    check_xlsx_with(bytes, &XlsxLimits::default(), cancelled)
}

/// Cells in a reference such as `A1`, `B2:D9`, `A:C` or `3:5`; 0 if it is not one.
pub fn ref_area(r: &str) -> u64 {
    match refs::parse(r.trim()).map(|r| r.span) {
        Ok(Span::Cell { .. }) => 1,
        Ok(Span::Area { r1, c1, r2, c2 }) => (r1.abs_diff(r2) as u64 + 1) * (c1.abs_diff(c2) as u64 + 1),
        Ok(Span::Cols { c1, c2 }) => (c1.abs_diff(c2) as u64 + 1) * MAX_ROW as u64,
        Ok(Span::Rows { r1, r2 }) => (r1.abs_diff(r2) as u64 + 1) * MAX_COL as u64,
        Err(_) => 0,
    }
}

/// Cells in a space-separated list of references (`sqref`).
pub fn sqref_area(s: &str) -> u64 {
    s.split_whitespace().map(ref_area).fold(0u64, u64::saturating_add)
}

/// Totals over the whole file.
#[derive(Default)]
struct Totals {
    report: XlsxReport,
    /// Defined names and their formulas.
    names: Vec<(String, String)>,
    /// How often each name is used as a value: (in full, directly in a clipping function).
    name_uses: HashMap<String, (u64, u64)>,
}

/// A formula being read.
#[derive(Default)]
struct FormulaXml {
    kind: Option<String>,
    reference: Option<String>,
    shared_index: Option<String>,
    text: String,
}

/// The state of one part's scan.
#[derive(Default)]
struct Part {
    root: Option<String>,
    depth: usize,
    formula: Option<FormulaXml>,
    /// Text of a conditional-format `<formula>` or a `<definedName>`.
    other_text: Option<String>,
    defined_name: Option<String>,
    in_cf_rule: bool,
    /// Shared formulas of this sheet: index -> (walk, names used).
    shared: HashMap<String, (u64, Vec<(String, bool)>)>,
    /// Cells that reuse a shared formula, per index.
    shared_uses: HashMap<String, u64>,
}

struct Scan<'a> {
    limits: &'a XlsxLimits,
    totals: Totals,
    part_name: String,
}

fn attr(e: &BytesStart, name: &str) -> Result<Option<String>, String> {
    for a in e.attributes() {
        let a = a.map_err(|err| format!("not a readable xlsx file: {err}"))?;
        if a.key.local_name().as_ref() == name {
            let v = a.normalized_value(XmlVersion::Implicit1_0).map_err(|err| format!("not a readable xlsx file: {err}"))?;
            return Ok(Some(v.into_owned()));
        }
    }
    Ok(None)
}

impl Scan<'_> {
    fn refuse(&self, what: String) -> String {
        format!("xlsx refused: {what} (in {})", self.part_name)
    }

    fn add_cells(&mut self, n: u64) -> Result<(), String> {
        let r = &mut self.totals.report;
        r.cells = r.cells.saturating_add(n);
        if r.cells > self.limits.max_cells {
            return Err(self.refuse(format!("more than {} cells (including cells covered by array formulas)", self.limits.max_cells)));
        }
        Ok(())
    }

    fn add_walk(&mut self, n: u64) -> Result<(), String> {
        let r = &mut self.totals.report;
        r.walk = r.walk.saturating_add(n);
        if r.walk > self.limits.max_walk {
            return Err(walk_error(self.limits.max_walk, &self.part_name));
        }
        Ok(())
    }

    fn use_names(&mut self, names: &[(String, bool)], times: u64) {
        for (n, clipped) in names {
            let e = self.totals.name_uses.entry(n.clone()).or_default();
            if *clipped {
                e.1 = e.1.saturating_add(times);
            } else {
                e.0 = e.0.saturating_add(times);
            }
        }
    }

    fn open(&mut self, p: &mut Part, e: &BytesStart, empty: bool) -> Result<(), String> {
        p.depth += 1;
        if p.depth > self.limits.max_xml_depth {
            return Err(self.refuse(format!("XML nested more than {} levels deep", self.limits.max_xml_depth)));
        }
        let local = e.local_name();
        let tag = local.as_ref();
        if p.root.is_none() {
            p.root = Some(tag.to_string());
        }
        match (p.root.as_deref(), tag) {
            (Some("worksheet"), "c") => self.add_cells(1)?,
            (Some("worksheet"), "f") => {
                let f = FormulaXml { kind: attr(e, "t")?, reference: attr(e, "ref")?, shared_index: attr(e, "si")?, text: String::new() };
                if empty {
                    self.formula(p, f)?;
                } else {
                    p.formula = Some(f);
                }
            }
            (Some("worksheet"), "mergeCell") => {
                let area = attr(e, "ref")?.map(|r| ref_area(&r)).unwrap_or(0);
                let r = &mut self.totals.report;
                r.merged = r.merged.saturating_add(area);
                if r.merged > self.limits.max_merged {
                    return Err(self.refuse(format!("merged ranges cover more than {} cells", self.limits.max_merged)));
                }
            }
            (Some("worksheet"), "conditionalFormatting") => {
                // evaluated over the whole range on every recalculation
                let area = attr(e, "sqref")?.map(|s| sqref_area(&s)).unwrap_or(0);
                self.add_walk(area)?;
            }
            (Some("worksheet"), "cfRule") => p.in_cf_rule = !empty,
            (Some("worksheet"), "formula") if p.in_cf_rule && !empty => p.other_text = Some(String::new()),
            (Some("workbook"), "definedName") if !empty => {
                p.defined_name = attr(e, "name")?;
                p.other_text = Some(String::new());
            }
            _ => {}
        }
        if empty {
            p.depth -= 1;
        }
        Ok(())
    }

    fn close(&mut self, p: &mut Part, tag: &str) -> Result<(), String> {
        p.depth = p.depth.saturating_sub(1);
        match (p.root.as_deref(), tag) {
            (Some("worksheet"), "f") => {
                if let Some(f) = p.formula.take() {
                    self.formula(p, f)?;
                }
            }
            (Some("worksheet"), "cfRule") => p.in_cf_rule = false,
            (Some("worksheet"), "formula") => {
                if let Some(t) = p.other_text.take() {
                    formula::check(&t).map_err(|e| self.refuse(format!("conditional format formula: {e}")))?;
                }
            }
            (Some("workbook"), "definedName") => {
                if let Some(t) = p.other_text.take() {
                    let name = p.defined_name.take().unwrap_or_default();
                    formula::check(&t).map_err(|e| self.refuse(format!("defined name {name}: {e}")))?;
                    self.totals.names.push((name, t));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn text(&self, p: &mut Part, t: &str) -> Result<(), String> {
        let target = match (&mut p.formula, &mut p.other_text) {
            (Some(f), _) => &mut f.text,
            (None, Some(t)) => t,
            (None, None) => return Ok(()),
        };
        // 4 bytes per character at most: stop long before memory matters
        if target.len() + t.len() > MAX_FORMULA_CHARS * 4 {
            return Err(self.refuse(format!("a formula longer than {MAX_FORMULA_CHARS} characters")));
        }
        target.push_str(t);
        Ok(())
    }

    /// One cell's `<f>`.
    fn formula(&mut self, p: &mut Part, f: FormulaXml) -> Result<(), String> {
        let r = &mut self.totals.report;
        r.formulas += 1;
        if r.formulas > self.limits.max_formulas {
            return Err(self.refuse(format!("more than {} formulas", self.limits.max_formulas)));
        }
        if f.kind.as_deref() == Some("array") {
            let area = f.reference.as_deref().map(ref_area).unwrap_or(1);
            if area > self.limits.max_array {
                return Err(self.refuse(format!("an array formula covering {area} cells (the limit is {})", self.limits.max_array)));
            }
            // the anchor cell was counted as a cell already
            self.add_cells(area.saturating_sub(1))?;
        }
        if f.text.is_empty() {
            // a cell reusing a shared formula: its cost is known at the end of the sheet
            if let (Some("shared"), Some(si)) = (f.kind.as_deref(), f.shared_index) {
                *p.shared_uses.entry(si).or_default() += 1;
            }
            return Ok(());
        }
        let s = formula::check(&f.text).map_err(|e| self.refuse(format!("cell formula: {e}")))?;
        self.use_names(&s.names, 1);
        self.add_walk(s.walk)?;
        if let (Some("shared"), Some(si)) = (f.kind.as_deref(), f.shared_index) {
            p.shared.insert(si, (s.walk, s.names));
        }
        Ok(())
    }

    /// The end of a part: charge the cells that reused shared formulas.
    fn finish_part(&mut self, p: &mut Part) -> Result<(), String> {
        let uses = std::mem::take(&mut p.shared_uses);
        for (si, n) in uses {
            if let Some((walk, names)) = p.shared.get(&si).cloned() {
                self.use_names(&names, n);
                self.add_walk(walk.saturating_mul(n))?;
            }
        }
        Ok(())
    }
}

/// Counts what a part unpacks to and polls cancellation, so a zip bomb is
/// stopped while it is being read.
struct Counting<'a, R> {
    inner: R,
    total: &'a StdCell<u64>,
    max: u64,
    cancelled: &'a dyn Fn() -> bool,
    since_poll: usize,
    stop: &'a StdCell<Option<String>>,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let total = self.total.get() + n as u64;
        self.total.set(total);
        if total > self.max {
            self.stop.set(Some(format!("xlsx unpacks to more than {} MB; refused", self.max >> 20)));
            return Err(std::io::Error::other("unpacked limit"));
        }
        self.since_poll += n;
        if self.since_poll >= 64 << 10 {
            self.since_poll = 0;
            if (self.cancelled)() {
                self.stop.set(Some("cancelled".into()));
                return Err(std::io::Error::other("cancelled"));
            }
        }
        Ok(n)
    }
}

/// Measure an xlsx without building anything from it; see the module docs.
pub fn check_xlsx_with(bytes: &[u8], limits: &XlsxLimits, cancelled: &dyn Fn() -> bool) -> Result<XlsxReport, String> {
    if bytes.len() > limits.max_bytes {
        return Err(format!("xlsx file is {} MB; the limit is {} MB", bytes.len() >> 20, limits.max_bytes >> 20));
    }
    if !bytes.starts_with(b"PK") {
        return Err("not an xlsx file (xlsx files are zip archives; use sheet.from_csv for text)".into());
    }
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("not a readable xlsx file: {e}"))?;
    if zip.len() > limits.max_parts {
        return Err(format!("xlsx has {} parts; the limit is {}", zip.len(), limits.max_parts));
    }
    let total = StdCell::new(0u64);
    let stop: StdCell<Option<String>> = StdCell::new(None);
    let mut scan = Scan { limits, totals: Totals::default(), part_name: String::new() };
    let mut buf = Vec::new();
    for i in 0..zip.len() {
        let part = zip.by_index(i).map_err(|e| format!("not a readable xlsx file: {e}"))?;
        let name = part.name().map_err(|e| format!("not a readable xlsx file: {e}"))?.into_owned();
        let xml = name.ends_with(".xml") || name.ends_with(".rels");
        scan.part_name = name.clone();
        let mut counting = Counting { inner: part, total: &total, max: limits.max_unpacked, cancelled, since_poll: 0, stop: &stop };
        let stopped = |e: String| -> String { stop.take().unwrap_or(e) };
        if xml {
            let mut reader = quick_xml::Reader::from_reader(BufReader::new(&mut counting));
            let mut p = Part::default();
            loop {
                buf.clear();
                let ev = reader.read_event_into(&mut buf).map_err(|e| stopped(format!("not a readable xlsx file: {name}: {e}")))?;
                match ev {
                    Event::Start(e) => scan.open(&mut p, &e, false)?,
                    Event::Empty(e) => scan.open(&mut p, &e, true)?,
                    Event::End(e) => scan.close(&mut p, e.local_name().as_ref())?,
                    Event::Text(t) => scan.text(&mut p, &t.xml_content(XmlVersion::Implicit1_0))?,
                    Event::CData(t) => scan.text(&mut p, &t.xml_content(XmlVersion::Implicit1_0))?,
                    Event::GeneralRef(r) => {
                        let ch = r.resolve_char_ref().map_err(|e| format!("not a readable xlsx file: {name}: {e}"))?;
                        let s = match ch {
                            Some(c) => c.to_string(),
                            None => quick_xml::escape::resolve_predefined_entity(&r).unwrap_or("").to_string(),
                        };
                        scan.text(&mut p, &s)?;
                    }
                    Event::Eof => break,
                    _ => {}
                }
            }
            scan.finish_part(&mut p)?;
        }
        // whatever the XML reader did not consume still counts
        std::io::copy(&mut counting, &mut std::io::sink()).map_err(|e| stopped(format!("not a readable xlsx file: {name}: {e}")))?;
        if let Some(s) = stop.take() {
            return Err(s);
        }
    }
    let mut totals = scan.totals;
    totals.report.unpacked = total.get();
    if !totals.names.is_empty() {
        let costs = formula::name_costs(totals.names.iter().map(|(n, f)| (n.as_str(), f.as_str())));
        for (n, (full, clipped)) in &totals.name_uses {
            if let Some(&(c_full, c_clipped)) = costs.get(n) {
                let w = full.saturating_mul(c_full).saturating_add(clipped.saturating_mul(c_clipped));
                totals.report.walk = totals.report.walk.saturating_add(w);
            }
        }
        if totals.report.walk > limits.max_walk {
            return Err(walk_error(limits.max_walk, "the ranges of its defined names"));
        }
    }
    Ok(totals.report)
}

fn walk_error(max: u64, place: &str) -> String {
    format!(
        "xlsx refused: its formulas and conditional formats would read more than {max} cells on every recalculation (whole-column ranges \
         outside SUM, COUNTIF and similar are read cell by cell) (in {place})"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn areas() {
        assert_eq!(ref_area("A1"), 1);
        assert_eq!(ref_area("B2:D9"), 24);
        assert_eq!(ref_area("A1:XFD1048576"), 16_384 * 1_048_576);
        assert_eq!(ref_area("A:C"), 3 * 1_048_576);
        assert_eq!(ref_area("garbage"), 0);
        assert_eq!(sqref_area("A1:A10 C1:C5"), 15);
    }
}
