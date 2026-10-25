//! Static checks on formula text, before IronCalc parses it.
//!
//! IronCalc parses, evaluates and prints formulas recursively and has no
//! limit of its own on how deep a formula goes; a stack overflow aborts the
//! process. It also walks every cell of a range in most functions, so a short
//! formula such as `=COUNTA(A:XFD)` reads 17 billion cells on every
//! recalculation, and a lambda can call itself without end. This module reads
//! formula text with a small tokenizer (A1 references, English function
//! names, as IronCalc takes them) and measures:
//!
//! - **nesting**: parentheses, function calls and array constants inside one
//!   another (Excel allows 64 nested functions);
//! - **depth**: an upper bound on the depth of the expression tree, counting
//!   every operator of a nesting level as one level (a chain `1+1+1` is a
//!   left-deep tree as deep as it is long);
//! - **walk**: an estimate of the cells the formula reads on each evaluation,
//!   from the area of each range it names. A whole column or row given
//!   directly to a function that IronCalc clips to the used area (SUM,
//!   COUNTIF, XLOOKUP, ...) costs nothing; everywhere else it costs its full
//!   area (`COUNTA(A:A)` reads 1 048 576 cells, about 0.3 to 1 s);
//! - **lambda**: whether it uses `LAMBDA` (refused: recursion is unbounded).
//!
//! The estimate cannot see ranges built at run time (`INDIRECT`, `OFFSET`)
//! or what defined names refer to unless the caller resolves them
//! ([`Scan::names`]).

use std::collections::HashMap;

/// Excel's limit on the length of a formula, in characters.
pub const MAX_FORMULA_CHARS: usize = 8_192;
/// Excel's limit on nested functions; parentheses and array constants count too.
pub const MAX_NESTING: usize = 64;
/// Bound on the expression tree depth. Every binary chain that fits in
/// [`MAX_FORMULA_CHARS`] passes (`1+1+...` has at most 4 096 operators); long
/// runs of unary operators (`=-----...1`) do not.
pub const MAX_DEPTH: usize = 4_200;
/// Cells one formula may read on each recalculation: four whole columns.
pub const MAX_FORMULA_WALK: u64 = 4 * ROWS;
/// Cells all formulas (and conditional formats) of a workbook may read on
/// each recalculation, by the same estimate. At 0.3 to 1 microsecond a cell
/// this bounds one recalculation to roughly 30 to 100 seconds; anything
/// bigger is treated as a decompression-bomb style attack rather than a
/// spreadsheet.
pub const MAX_BOOK_WALK: u64 = 100_000_000;

const ROWS: u64 = 1_048_576;
const COLS: u64 = 16_384;

/// Functions that visit only the used area of a whole-column or whole-row
/// range passed directly as an argument, or that do not visit the cells at
/// all (measured on IronCalc 0.9.1: `=SUM(A:A)` 0.1 ms, `=COUNTA(A:A)` 300
/// to 800 ms on a 20-cell sheet; `ROW(A:A)` builds a million-row array and
/// is not in the list).
const CLIPPING: &[&str] = &[
    "SUM", "PRODUCT", "SUMIF", "SUMIFS", "COUNTIF", "COUNTIFS", "AVERAGEIF", "AVERAGEIFS", "MAXIFS", "MINIFS", "XLOOKUP", "XMATCH", "INDEX",
    "ROWS", "COLUMNS", "OFFSET", "ISREF", "STDEV", "STDEV.S", "STDEV.P", "STDEVA", "STDEVP", "STDEVPA", "VAR", "VAR.S", "VAR.P", "VARA", "VARP",
    "VARPA", "DEVSQ", "GCD", "LCM", "TEXTJOIN",
];

/// What [`scan`] found in one formula.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scan {
    /// Characters, without the leading `=`.
    pub chars: usize,
    /// Deepest nesting of parentheses, function calls and array constants.
    pub nesting: usize,
    /// Upper bound on the depth of the expression tree.
    pub depth: usize,
    /// Cells read on each evaluation through the references written in the formula.
    pub walk: u64,
    /// The same if every whole-column or whole-row reference sat directly in
    /// a clipping function: what a defined name costs when used that way.
    pub walk_clipped: u64,
    /// Other names used as values (defined names, LET variables, TRUE...),
    /// upper-cased, with whether each sat directly in a clipping function.
    pub names: Vec<(String, bool)>,
    /// `LAMBDA` is used.
    pub lambda: bool,
}

impl Scan {
    /// `walk` plus what the names cost, given the cost of each defined name
    /// as `(walk, walk_clipped)` (see [`name_costs`]).
    pub fn walk_with(&self, names: &HashMap<String, (u64, u64)>) -> u64 {
        let mut w = self.walk;
        if !names.is_empty() {
            for (n, clipped) in &self.names {
                if let Some(&(full, cl)) = names.get(n) {
                    w = w.saturating_add(if *clipped { cl } else { full });
                }
            }
        }
        w
    }
}

/// The cost of each defined name, `(walk, walk_clipped)`, from its formula text.
pub fn name_costs<'a>(names: impl IntoIterator<Item = (&'a str, &'a str)>) -> HashMap<String, (u64, u64)> {
    let mut out = HashMap::new();
    for (name, formula) in names {
        let s = scan(formula);
        if s.walk > 0 {
            let e = out.entry(name.to_uppercase()).or_insert((0, 0));
            e.0 = e.0.max(s.walk);
            e.1 = e.1.max(s.walk_clipped);
        }
    }
    out
}

/// Measure a formula (with or without its leading `=`) and refuse what could
/// crash or hang IronCalc. Returns the measurements.
pub fn check(formula: &str) -> Result<Scan, String> {
    let s = scan(formula);
    if s.chars > MAX_FORMULA_CHARS {
        return Err(format!("formula longer than {MAX_FORMULA_CHARS} characters ({})", s.chars));
    }
    if s.nesting > MAX_NESTING {
        return Err(format!("formula nests functions or parentheses {} levels deep; the limit is {MAX_NESTING} (as in Excel)", s.nesting));
    }
    if s.depth > MAX_DEPTH {
        return Err(format!(
            "formula is too deep to evaluate safely (about {} levels of operators; the limit is {MAX_DEPTH}): split it over several cells",
            s.depth
        ));
    }
    if s.lambda {
        return Err("LAMBDA is not supported: a lambda can call itself without limit (through LET or a defined name), which would overflow the stack".into());
    }
    if s.walk > MAX_FORMULA_WALK {
        return Err(format!(
            "formula reads about {} cells on every recalculation; the limit is {MAX_FORMULA_WALK}. Whole columns or rows (A:XFD, 1:1048576) are \
             read cell by cell except directly in SUM, COUNTIF, SUMIF, XLOOKUP, INDEX and similar: use a smaller range",
            s.walk
        ));
    }
    Ok(s)
}

/// One level of parentheses, a function call, an array constant, or the formula itself.
#[derive(Default)]
struct Frame {
    /// Operators seen at this level.
    ops: usize,
    /// Deepest tree below this level.
    child: usize,
    /// The function called, upper-cased, when this level is a call.
    func: Option<String>,
    /// Tokens in the current argument.
    arg_tokens: usize,
    /// A whole-column/row reference that is, so far, the whole argument.
    pending_walk: Option<(u64, u64)>,
    /// A name that is, so far, the whole argument.
    pending_name: Option<String>,
}

struct Scanner {
    frames: Vec<Frame>,
    nesting: usize,
    walk: u64,
    walk_clipped: u64,
    names: Vec<(String, bool)>,
    lambda: bool,
}

impl Scanner {
    fn top(&mut self) -> &mut Frame {
        if self.frames.is_empty() {
            self.frames.push(Frame::default());
        }
        let last = self.frames.len() - 1;
        &mut self.frames[last]
    }

    /// The current argument stops being a single token: what was pending is
    /// not directly in the function.
    fn flush(&mut self) {
        let f = self.top();
        let w = f.pending_walk.take();
        let n = f.pending_name.take();
        if let Some((full, _)) = w {
            self.walk = self.walk.saturating_add(full);
            self.walk_clipped = self.walk_clipped.saturating_add(full);
        }
        if let Some(n) = n {
            self.names.push((n, false));
        }
    }

    fn token(&mut self) {
        let f = self.top();
        f.arg_tokens += 1;
        if f.arg_tokens >= 2 {
            self.flush();
        }
    }

    fn op(&mut self) {
        self.top().ops += 1;
        self.token();
    }

    /// A range that reads `area` cells; `whole` when it spans full columns or rows.
    fn range(&mut self, area: u64, whole: bool) {
        self.token();
        if whole && self.top().arg_tokens == 1 {
            self.top().pending_walk = Some((area, 0));
        } else {
            self.walk = self.walk.saturating_add(area);
            self.walk_clipped = self.walk_clipped.saturating_add(if whole { 0 } else { area });
        }
    }

    fn name(&mut self, n: String) {
        if n == "LAMBDA" || n.ends_with(".LAMBDA") {
            // `LAMBDA (x, ...)` with a space before the bracket
            self.lambda = true;
        }
        self.token();
        if self.top().arg_tokens == 1 {
            self.top().pending_name = Some(n);
        } else {
            self.names.push((n, false));
        }
    }

    /// The current argument ended (`,`, `;` or the closing bracket).
    fn end_arg(&mut self) {
        let f = self.top();
        let clipping = f.func.as_deref().is_some_and(|n| CLIPPING.contains(&n));
        let bare = f.arg_tokens == 1;
        let w = f.pending_walk.take();
        let n = f.pending_name.take();
        f.arg_tokens = 0;
        if let Some((full, cl)) = w {
            let cost = if clipping && bare { cl } else { full };
            self.walk = self.walk.saturating_add(cost);
            self.walk_clipped = self.walk_clipped.saturating_add(cl);
        }
        if let Some(n) = n {
            self.names.push((n, clipping && bare));
        }
    }

    fn open(&mut self, func: Option<String>) {
        if func.as_deref() == Some("LAMBDA") {
            self.lambda = true;
        }
        self.top();
        self.frames.push(Frame { func, ..Frame::default() });
        self.nesting = self.nesting.max(self.frames.len() - 1);
    }

    fn close(&mut self) {
        if self.frames.len() <= 1 {
            // an unbalanced closing bracket: IronCalc refuses it; count it as an operator
            self.op();
            return;
        }
        self.end_arg();
        let f = self.frames.pop().unwrap_or_default();
        let d = f.ops + 1 + f.child;
        let parent = self.top();
        parent.child = parent.child.max(d);
        // the call or group is one operand of the enclosing argument
        self.token();
    }
}

fn is_atom_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '$' | '\\' | '?')
}

/// Column letters (1-based index), `None` past XFD.
fn col_of(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 3 || !s.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let n = s.bytes().fold(0u64, |n, b| n * 26 + (b.to_ascii_uppercase() - b'A' + 1) as u64);
    (n <= COLS).then_some(n)
}

fn row_of(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 7 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok().filter(|r| (1..=ROWS).contains(r))
}

/// What an atom of a reference is.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Part {
    Cell(u64, u64),
    Col(u64),
    Row(u64),
}

fn part_of(atom: &str) -> Option<Part> {
    let a = atom.replace('$', "");
    if let Some(r) = row_of(&a) {
        return Some(Part::Row(r));
    }
    if let Some(c) = col_of(&a) {
        return Some(Part::Col(c));
    }
    let split = a.find(|c: char| c.is_ascii_digit())?;
    let (c, r) = a.split_at(split);
    Some(Part::Cell(row_of(r)?, col_of(c)?))
}

/// Cells in `a:b`, and whether it spans full columns or rows; `None` if the
/// two parts do not make a range.
fn range_area(a: Part, b: Part) -> Option<(u64, bool)> {
    let span = |x: u64, y: u64| x.abs_diff(y) + 1;
    match (a, b) {
        (Part::Cell(r1, c1), Part::Cell(r2, c2)) => {
            let whole = (r1.min(r2) == 1 && r1.max(r2) == ROWS) || (c1.min(c2) == 1 && c1.max(c2) == COLS);
            Some((span(r1, r2) * span(c1, c2), whole))
        }
        (Part::Col(c1), Part::Col(c2)) => Some((span(c1, c2) * ROWS, true)),
        (Part::Row(r1), Part::Row(r2)) => Some((span(r1, r2) * COLS, true)),
        _ => None,
    }
}

fn normalize_function(name: &str) -> String {
    let mut n = name.to_uppercase();
    loop {
        let stripped = ["_XLFN.", "_XLWS.", "_XLPM."].iter().find_map(|p| n.strip_prefix(p).map(str::to_string));
        match stripped {
            Some(s) => n = s,
            None => return n,
        }
    }
}

/// Measure a formula; see the module documentation.
pub fn scan(formula: &str) -> Scan {
    let body = formula.strip_prefix('=').unwrap_or(formula);
    let chars: Vec<char> = body.chars().collect();
    let mut s = Scanner { frames: vec![Frame::default()], nesting: 0, walk: 0, walk_clipped: 0, names: Vec::new(), lambda: false };
    let n = chars.len();
    let mut i = 0;
    // the previous token ended an operand (for spill `#` and the space operator)
    let mut after_operand = false;
    let mut space_before = false;
    while i < n {
        let c = chars[i];
        if c.is_whitespace() {
            space_before = true;
            i += 1;
            continue;
        }
        let starts_operand = c == '"' || c == '\'' || c == '(' || c == '{' || c == '[' || is_atom_char(c) || (c == '#' && !after_operand);
        if space_before && after_operand && starts_operand {
            // `A1:B2 B1:C3`: the intersection operator
            s.op();
        }
        space_before = false;
        match c {
            '"' => {
                i += 1;
                while i < n {
                    if chars[i] == '"' {
                        if i + 1 < n && chars[i + 1] == '"' {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
                s.token();
                after_operand = true;
            }
            '{' => {
                s.open(None);
                i += 1;
                after_operand = false;
            }
            '(' => {
                s.open(None);
                i += 1;
                after_operand = false;
            }
            ')' | '}' => {
                s.close();
                i += 1;
                after_operand = true;
            }
            ',' | ';' => {
                s.end_arg();
                i += 1;
                after_operand = false;
            }
            '#' if after_operand => {
                // the spill operator `A1#`
                s.op();
                i += 1;
            }
            '#' => {
                // an error value such as #N/A or #DIV/0!
                i += 1;
                while i < n && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '/' | '!' | '?' | '_')) {
                    i += 1;
                }
                s.token();
                after_operand = true;
            }
            '[' => {
                // a structured reference such as [@Col] or [[#This Row],[Col]]
                i = skip_brackets(&chars, i);
                s.token();
                after_operand = true;
            }
            '+' | '-' | '*' | '/' | '^' | '&' | '=' | '<' | '>' | '@' | ':' | '!' => {
                if (c == '<' || c == '>') && i + 1 < n && (chars[i + 1] == '=' || chars[i + 1] == '>') {
                    i += 1;
                }
                s.op();
                i += 1;
                after_operand = false;
            }
            '%' => {
                s.op();
                i += 1;
                after_operand = true;
            }
            _ => {
                let (next, kind) = read_operand(&chars, i);
                i = next;
                match kind {
                    Operand::Function(name) => {
                        // `name(`: the bracket is consumed here
                        s.open(Some(normalize_function(&name)));
                        i += 1;
                        after_operand = false;
                        continue;
                    }
                    Operand::Range(area, whole) => s.range(area, whole),
                    Operand::Name(name) => s.name(name),
                    Operand::Other => s.token(),
                }
                after_operand = true;
            }
        }
    }
    // close what was left open (IronCalc refuses it, or adds one `)`)
    while s.frames.len() > 1 {
        s.close();
    }
    s.end_arg();
    let top = s.frames.pop().unwrap_or_default();
    Scan {
        chars: n,
        nesting: s.nesting,
        depth: top.ops + 1 + top.child,
        walk: s.walk,
        walk_clipped: s.walk_clipped,
        names: s.names,
        lambda: s.lambda,
    }
}

fn skip_brackets(chars: &[char], mut i: usize) -> usize {
    let mut depth = 0usize;
    while i < chars.len() {
        match chars[i] {
            // `'` escapes the next character inside structured references
            '\'' => i += 1,
            '[' => depth += 1,
            ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    i
}

enum Operand {
    Function(String),
    /// A range: its cells, and whether it spans whole columns or rows.
    Range(u64, bool),
    /// An identifier used as a value, upper-cased.
    Name(String),
    Other,
}

/// Read one atom of a reference, after an optional sheet prefix
/// (`Sheet1!`, `'My sheet'!`). Returns the next index, the atom, and whether
/// a sheet was given.
fn read_atom(chars: &[char], mut i: usize) -> (usize, String, bool) {
    let n = chars.len();
    let mut sheet = false;
    // sheet prefixes, quoted or not; a loop, so `a!b!c!...` cannot recurse
    loop {
        if i < n && chars[i] == '\'' {
            // a quoted sheet name: '' is an escaped quote
            i += 1;
            while i < n {
                if chars[i] == '\'' {
                    if i + 1 < n && chars[i + 1] == '\'' {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            if i < n && chars[i] == '!' {
                i += 1;
                sheet = true;
                continue;
            }
            return (i, String::new(), false);
        }
        let start = i;
        while i < n && is_atom_char(chars[i]) {
            i += 1;
        }
        // numbers: 1.5E+10
        if i < n && (chars[i] == '+' || chars[i] == '-') && i > start + 1 {
            let up: String = chars[start..i].iter().collect::<String>().to_ascii_uppercase();
            if up.ends_with('E') && up[..up.len() - 1].chars().all(|c| c.is_ascii_digit() || c == '.') {
                i += 1;
                while i < n && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
        }
        if i < n && chars[i] == '!' && i > start {
            // Sheet1!A1: what was read is a sheet name
            i += 1;
            sheet = true;
            continue;
        }
        let mut atom: String = chars[start..i].iter().collect();
        if atom.is_empty() && i < n {
            // a character that starts nothing we know: step over it
            atom.push(chars[i]);
            i += 1;
        }
        return (i, atom, sheet);
    }
}

fn read_operand(chars: &[char], i: usize) -> (usize, Operand) {
    let n = chars.len();
    let (mut j, atom, sheet) = read_atom(chars, i);
    if !sheet && j < n && chars[j] == '(' {
        return (j, Operand::Function(atom));
    }
    if j < n && chars[j] == '[' {
        // Table1[Col]
        return (skip_brackets(chars, j), Operand::Other);
    }
    let first = part_of(&atom);
    if let Some(a) = first {
        if j < n && chars[j] == ':' {
            let (k, atom2, _) = read_atom(chars, j + 1);
            if let Some((area, whole)) = part_of(&atom2).and_then(|b| range_area(a, b)) {
                j = k;
                return (j, Operand::Range(area, whole));
            }
        }
        if matches!(a, Part::Cell(..)) {
            return (j, Operand::Other);
        }
    }
    let starts_like_name = atom.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '\\');
    if starts_like_name {
        (j, Operand::Name(atom.to_uppercase()))
    } else {
        (j, Operand::Other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nesting_and_depth() {
        let s = scan("=SUM(1,2)");
        assert_eq!((s.nesting, s.walk, s.lambda), (1, 0, false));
        assert_eq!(scan(&format!("={}1{}", "(".repeat(70), ")".repeat(70))).nesting, 70);
        assert_eq!(scan(&format!("={}1{}", "ABS(".repeat(64), ")".repeat(64))).nesting, 64);
        // a chain is as deep as it is long
        let chain = scan(&format!("=1{}", "+1".repeat(1000)));
        assert!(chain.depth >= 1000 && chain.depth <= 1002, "{}", chain.depth);
        let unary = scan(&format!("={}1", "-".repeat(5000)));
        assert!(unary.depth >= 5000);
        // the longest possible chain passes, long unary runs do not
        assert!(check(&format!("=1{}", "+1".repeat(4095))).is_ok());
        assert!(check(&format!("={}1", "-".repeat(4500))).unwrap_err().contains("too deep"));
        assert!(check(&format!("={}1{}", "(".repeat(65), ")".repeat(65))).unwrap_err().contains("65 levels"));
        assert!(check(&format!("={}1{}", "ABS(".repeat(64), ")".repeat(64))).is_ok());
        assert!(check(&format!("=\"{}\"", "x".repeat(MAX_FORMULA_CHARS))).unwrap_err().contains("longer"));
        // strings, sheet names and structured references hide their brackets
        let s = scan("=\"((((\"&'a(b'!A1&T[[#This Row],[c(]]");
        assert_eq!(s.nesting, 0);
        // nested groups add the operators of each level
        let s = scan("=1+2*(3+4*(5+6))");
        assert!(s.depth >= 5 && s.depth <= 8, "{}", s.depth);
        // unbalanced brackets do not break the scanner
        assert_eq!(scan("=SUM(1,(2").nesting, 2);
        assert_eq!(scan("=1))").nesting, 0);
    }

    #[test]
    fn walk_estimates() {
        assert_eq!(scan("=SUM(A:A)").walk, 0);
        assert_eq!(scan("=SUM(A1:A1048576)").walk, 0, "explicit full columns are clipped too");
        assert_eq!(scan("=COUNTA(A:A)").walk, ROWS);
        assert_eq!(scan("=counta($A:$C)").walk, 3 * ROWS);
        assert_eq!(scan("=COUNTA(Sheet1!A:B)").walk, 2 * ROWS);
        assert_eq!(scan("=COUNTA('My sheet'!2:3)").walk, 2 * COLS);
        assert_eq!(scan("=SUM(A:A*2)").walk, ROWS, "an expression is not clipped");
        assert_eq!(scan("=SUM(A:A,B:B)").walk, 0);
        assert_eq!(scan("=A:A").walk, ROWS);
        assert_eq!(scan("=SUMIF(A:A,\">3\",B:B)").walk, 0);
        assert_eq!(scan("=VLOOKUP(1,A:B,2,FALSE)").walk, 2 * ROWS);
        assert_eq!(scan("=SUM(A1:B10)+COUNTA(C1:C5)").walk, 25);
        assert_eq!(scan("=_xlfn.XLOOKUP(1,A:A,B:B)").walk, 0);
        assert!(check("=COUNTA(A:XFD)").unwrap_err().contains("reads about"));
        assert!(check("=COUNTA(A:D)").is_ok());
        assert!(check("=A1:XFD1048576+0").is_err());
        assert_eq!(scan("=A1+B2").walk, 0);
        assert_eq!(scan("=1.5E+10+A1").walk, 0);
        // names: resolved by the caller
        let s = scan("=COUNTA(Data)+SUM(Col)+Rate*2");
        assert_eq!(s.names, vec![("DATA".to_string(), false), ("COL".to_string(), true), ("RATE".to_string(), false)]);
        let costs = name_costs([("Data", "Sheet1!$A:$A"), ("Col", "Sheet1!$B:$B"), ("Rate", "Sheet1!$C$1")]);
        assert_eq!(s.walk_with(&costs), ROWS, "Data is read in full, Col is clipped by SUM");
    }

    #[test]
    fn lambda_is_found() {
        assert!(scan("=LET(f,LAMBDA(k,IF(k=0,0,1+f(k-1))),f(5))").lambda);
        assert!(scan("=_xlfn.LAMBDA(_xlpm.x,_xlpm.x+1)(2)").lambda);
        assert!(!scan("=\"LAMBDA(\"&LAMBDAS").lambda);
        assert!(check("=LAMBDA(x,x)(1)").unwrap_err().contains("LAMBDA"));
    }
}
