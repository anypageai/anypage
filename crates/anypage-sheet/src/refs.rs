//! A1-style references: `B2`, `$B$2`, `Sheet1!B2:D20`, `'My sheet'!A:C`,
//! `3:5`. Rows and columns are 1-based, as in every spreadsheet.

/// The last row and column of an xlsx sheet (Excel's limits, also IronCalc's).
pub const MAX_ROW: i32 = 1_048_576;
pub const MAX_COL: i32 = 16_384;

/// What part of a sheet a reference names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Span {
    Cell { row: i32, col: i32 },
    /// Inclusive, normalised so `r1 <= r2` and `c1 <= c2`.
    Area { r1: i32, c1: i32, r2: i32, c2: i32 },
    /// Whole columns `A:C`.
    Cols { c1: i32, c2: i32 },
    /// Whole rows `3:5`.
    Rows { r1: i32, r2: i32 },
}

impl Span {
    /// Top-left cell (whole rows start at column 1, whole columns at row 1).
    pub fn origin(&self) -> (i32, i32) {
        match *self {
            Span::Cell { row, col } => (row, col),
            Span::Area { r1, c1, .. } => (r1, c1),
            Span::Cols { c1, .. } => (1, c1),
            Span::Rows { r1, .. } => (r1, 1),
        }
    }
}

/// A parsed reference: an optional sheet name and a span on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    pub sheet: Option<String>,
    pub span: Span,
}

/// 1 → "A", 27 → "AA", 16384 → "XFD".
pub fn col_name(mut col: i32) -> String {
    if col < 1 {
        return String::new();
    }
    let mut out = Vec::new();
    while col > 0 {
        let rem = (col - 1) % 26;
        out.push(b'A' + rem as u8);
        col = (col - 1) / 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// "A" → 1, "aa" → 27; `None` for anything that is not 1 to 3 letters within XFD.
pub fn col_index(s: &str) -> Option<i32> {
    if s.is_empty() || s.len() > 3 || !s.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let n = s.bytes().fold(0i32, |n, b| n * 26 + (b.to_ascii_uppercase() - b'A') as i32 + 1);
    (1..=MAX_COL).contains(&n).then_some(n)
}

/// "B2" for (2, 2).
pub fn cell_name(row: i32, col: i32) -> String {
    format!("{}{row}", col_name(col))
}

/// A sheet name as it must be written in a reference: quoted when it is not
/// a plain identifier (`'My sheet'`, `'2024'`), with `'` doubled.
pub fn quote_sheet(name: &str) -> String {
    let plain = !name.is_empty()
        && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && !looks_like_cell(name);
    if plain {
        name.to_string()
    } else {
        format!("'{}'", name.replace('\'', "''"))
    }
}

fn looks_like_cell(s: &str) -> bool {
    matches!(part(s), Some(Part::Cell(..)))
}

/// "Sheet1!B2" style text for a cell or range.
pub fn format_ref(sheet: Option<&str>, span: &Span) -> String {
    let addr = match *span {
        Span::Cell { row, col } => cell_name(row, col),
        Span::Area { r1, c1, r2, c2 } => format!("{}:{}", cell_name(r1, c1), cell_name(r2, c2)),
        Span::Cols { c1, c2 } => format!("{}:{}", col_name(c1), col_name(c2)),
        Span::Rows { r1, r2 } => format!("{r1}:{r2}"),
    };
    match sheet {
        Some(s) => format!("{}!{addr}", quote_sheet(s)),
        None => addr,
    }
}

enum Part {
    Cell(i32, i32),
    Col(i32),
    Row(i32),
}

/// One side of a reference: `B2`, `$B$2`, `B`, `$B`, `2`, `$2`.
fn part(s: &str) -> Option<Part> {
    let s = s.trim();
    let s = s.strip_prefix('$').unwrap_or(s);
    let letters = s.bytes().take_while(u8::is_ascii_alphabetic).count();
    let (l, rest) = s.split_at(letters);
    let rest = if letters > 0 { rest.strip_prefix('$').unwrap_or(rest) } else { rest };
    let row = if rest.is_empty() {
        None
    } else {
        if !rest.bytes().all(|b| b.is_ascii_digit()) || rest.len() > 7 {
            return None;
        }
        let r: i32 = rest.parse().ok()?;
        if !(1..=MAX_ROW).contains(&r) {
            return None;
        }
        Some(r)
    };
    match (letters > 0, row) {
        (true, Some(r)) => Some(Part::Cell(r, col_index(l)?)),
        (true, None) => Some(Part::Col(col_index(l)?)),
        (false, Some(r)) => Some(Part::Row(r)),
        (false, None) => None,
    }
}

/// Split `'It''s'!A1` / `Data!A1` / `A1` into (sheet, address).
fn split_sheet(s: &str) -> Result<(Option<String>, &str), String> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('\'') {
        let mut name = String::new();
        let mut chars = rest.char_indices();
        while let Some((i, c)) = chars.next() {
            if c == '\'' {
                if rest[i + 1..].starts_with('\'') {
                    name.push('\'');
                    chars.next();
                    continue;
                }
                let after = &rest[i + 1..];
                let addr = after.strip_prefix('!').ok_or_else(|| format!("bad reference \"{s}\": expected ! after the quoted sheet name"))?;
                return Ok((Some(name), addr));
            }
            name.push(c);
        }
        return Err(format!("bad reference \"{s}\": unterminated sheet name quote"));
    }
    match s.rsplit_once('!') {
        Some((sheet, addr)) if !sheet.is_empty() => Ok((Some(sheet.to_string()), addr)),
        Some(_) => Err(format!("bad reference \"{s}\": empty sheet name")),
        None => Ok((None, s)),
    }
}

/// Parse a reference. Errors name the text and say what was expected.
pub fn parse(s: &str) -> Result<Ref, String> {
    if s.len() > 300 {
        return Err("reference longer than 300 characters".into());
    }
    let (sheet, addr) = split_sheet(s)?;
    let bad = || format!("bad reference \"{s}\" (expected A1, A1:C9, A:C, 2:5, optionally after Sheet! or 'My sheet'!)");
    let span = match addr.split_once(':') {
        None => match part(addr).ok_or_else(bad)? {
            Part::Cell(row, col) => Span::Cell { row, col },
            _ => return Err(bad()),
        },
        Some((a, b)) => match (part(a).ok_or_else(bad)?, part(b).ok_or_else(bad)?) {
            (Part::Cell(ra, ca), Part::Cell(rb, cb)) => Span::Area { r1: ra.min(rb), c1: ca.min(cb), r2: ra.max(rb), c2: ca.max(cb) },
            (Part::Col(a), Part::Col(b)) => Span::Cols { c1: a.min(b), c2: a.max(b) },
            (Part::Row(a), Part::Row(b)) => Span::Rows { r1: a.min(b), r2: a.max(b) },
            _ => return Err(bad()),
        },
    };
    Ok(Ref { sheet, span })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns() {
        for (n, s) in [(1, "A"), (26, "Z"), (27, "AA"), (52, "AZ"), (53, "BA"), (702, "ZZ"), (703, "AAA"), (16384, "XFD")] {
            assert_eq!(col_name(n), s);
            assert_eq!(col_index(s), Some(n));
            assert_eq!(col_index(&s.to_lowercase()), Some(n));
        }
        assert_eq!(col_index("XFE"), None);
        assert_eq!(col_index("ABCD"), None);
        assert_eq!(col_index(""), None);
        assert_eq!(col_name(0), "");
    }

    #[test]
    fn references() {
        assert_eq!(parse("B2").unwrap(), Ref { sheet: None, span: Span::Cell { row: 2, col: 2 } });
        assert_eq!(parse("$b$2").unwrap().span, Span::Cell { row: 2, col: 2 });
        assert_eq!(parse("Sheet1!D20:A1").unwrap(), Ref { sheet: Some("Sheet1".into()), span: Span::Area { r1: 1, c1: 1, r2: 20, c2: 4 } });
        assert_eq!(parse("'It''s mine'!A:C").unwrap(), Ref { sheet: Some("It's mine".into()), span: Span::Cols { c1: 1, c2: 3 } });
        assert_eq!(parse("Data!5:3").unwrap().span, Span::Rows { r1: 3, r2: 5 });
        assert_eq!(parse("資料!A1").unwrap().sheet.as_deref(), Some("資料"));
        assert_eq!(parse(" XFD1048576 ").unwrap().span, Span::Cell { row: MAX_ROW, col: MAX_COL });
        for bad in ["", "A", "1", "A0", "A1048577", "XFE1", "A1:B", "1:B", "'x!A1", "'x'A1", "!A1", "A1:B2:C3", "B-2"] {
            assert!(parse(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn formatting() {
        assert_eq!(format_ref(Some("Sheet1"), &Span::Area { r1: 1, c1: 1, r2: 3, c2: 2 }), "Sheet1!A1:B3");
        assert_eq!(format_ref(Some("My sheet"), &Span::Cell { row: 1, col: 1 }), "'My sheet'!A1");
        assert_eq!(format_ref(Some("It's"), &Span::Cols { c1: 2, c2: 2 }), "'It''s'!B:B");
        assert_eq!(format_ref(Some("A1"), &Span::Rows { r1: 2, r2: 4 }), "'A1'!2:4");
        assert_eq!(format_ref(None, &Span::Cell { row: 7, col: 28 }), "AB7");
        assert_eq!(quote_sheet("2024"), "'2024'");
        assert_eq!(quote_sheet("資料"), "資料");
        // what we write parses back
        for name in ["Sheet1", "My sheet", "It's", "A1", "2024", "資料"] {
            let s = format_ref(Some(name), &Span::Cell { row: 3, col: 3 });
            assert_eq!(parse(&s).unwrap().sheet.as_deref(), Some(name), "{s}");
        }
    }
}
