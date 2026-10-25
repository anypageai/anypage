# Core requests from the `sheet` module

Things the runtime or engine could change to serve this module better. Each
has a workaround in place today, so nothing here blocks the module.

## 1. Publish a resource at a given version

**Reason.** A workbook is mutated in place and must be published with a new
version after every edit. The module keeps an edit counter inside the
workbook and wants the handle it returns to carry exactly that version, so a
stale handle (a page holding version 3 after the workbook reached version 7)
still publishes version 8 and never collides with an earlier value.
`Res::next_version` only adds one to the handle it is called on.

**Workaround.** `module::publish` calls `next_version()` in a loop until the
handle reaches the counter. It is cheap (Arc clones) but proportional to how
stale the handle is.

**Proposed signature** (`crates/anypage-runtime/src/data.rs`):

```rust
impl Res {
    /// The same object published at `version`, which must not be lower than
    /// this handle's (versions only move forward).
    pub fn at_version(&self, version: u64) -> Res;
}
```

## 2. Edits that take longer than a handler's budget

**Reason.** Every edit recalculates the workbook (IronCalc evaluates all
formulas). For the 100 000-cell test workbook with 20 000 formulas one edit
costs 60 to 130 ms on the development machine (debug and release alike,
since dependencies are built with opt-level 3; measured while other builds
ran); the default `handler_ms` is 50. The Luau interrupt then fails the handler
right after the native call returned, so the new workbook version is never
`page.set`, although the workbook object itself already changed.

**Workaround.** Pages with big workbooks raise `limits.handler_ms` (the demo
uses 1000). Reading and loading already run as jobs (`sheet.load`,
`sheet.export`). Per-call caps keep one native call under about a second
(25 000 cells per `set_range` and per `set_style`), and inline `sheet.open` /
`sheet.from_csv` take at most 200 000 cells.

**What still overruns.** `handler_ms` cannot be raised above 1000
(`package.rs` clamps it), and a recalculation is not interruptible. A
workbook with a few formulas such as `=COUNTA(A:A)`, `=MATCH(x,A:A,0)` or a
conditional format over a whole column spends 0.3 to 1.2 s *per formula* on
every recalculation (IronCalc reads every cell of the column in those
functions), so every edit of such a workbook fails the handler after the
workbook has already changed. The module refuses only the extreme cases
(over 4 whole columns in one formula, 100 000 000 cells per recalculation
for the whole workbook).

**Proposal.** Either do not count time spent inside native module calls
against `handler_ms` (count it separately, with its own larger limit), or add
effect jobs: `page.edit(cell, "sheet.set", {args})` that runs a native
`Effect` function on the worker pool and sets the input cell to its result
when it finishes, cancelling or queueing behind earlier edits of the same
cell.

## 2b. A handler should be able to use a finished job's value

**Reason.** Files must open off the page's thread (`sheet.load` job), but a
handler can read only input cells (`page.get`), so after the job finishes
no handler can get the workbook it produced to edit it, and there is no
event when a job finishes.

**Workaround.** `sheet.load` takes `{into = wb}`: it loads into a new
workbook off the thread, then swaps that content into `wb` (the object held
by the page's input cell) under its lock and returns `wb` at a version above
everything published before. Panels and derived cells take the job as an
input to redraw when it finishes. This is an in-place mutation by a job,
which works because workbooks are mutable resources anyway, but it is not
how jobs are meant to be used.

**Proposal.** Either `page.on("ready", cell, fn(value))`, called once on the
page's thread when a job or derived cell becomes ready with a new value, or
let `page.get` read any cell's last ready value (not only input cells).

## 3. Tables with empty cells through Luau

**Reason.** The shared table form puts `nil` in empty cells. A Luau table
with `nil` holes converted back by `Data::from_lua` becomes a `Map` with
string keys (`{"1": "a", "3": 2}`), so a table read with `wb:range` and
returned from an action or passed on to another module is no longer a list.

**Workaround.** `sheet.set_range` accepts rows in that map form. Agents
reading `{"action": "read"}` from the demo see sparse rows as objects.

**Proposal.** In `table_to_data`, treat a table whose keys are all integers
`1..=n` (n bounded, say by 4 * count + 16) as a list with `Nil` in the holes,
or offer a `json.null`-style sentinel value for scripts.

## 4. Text styles in display lists

**Reason.** Workbooks carry bold, italic and font colours (`wb:style`), but a
panel's `d:text` has only size and colour, so the demo grid can show fills
and number formats but not bold headers.

**Proposal.** An optional style argument on `Draw:text` / `Draw:textbox`:
`d:text(x, y, size, colour, s, { bold = true, italic = true })`, mapped to the
font's bold/italic faces (or synthetic emboldening) in `TextRaster`.

# Upstream (IronCalc), not core

These need changes in IronCalc itself (a patch upstream, or a vendored and
patched copy); the module works around them as described in `src/lib.rs`.

1. **Clip whole columns and rows to the used area in every range function**
   (COUNTA, COUNT, AVERAGE, MAX, MIN, MATCH, VLOOKUP, HLOOKUP, SUMPRODUCT,
   MEDIAN, LARGE, CONCAT, COUNTBLANK with the remainder counted, conditional
   formatting ranges). `clip_to_used_area` already exists and is used by
   SUM, SUMIF and a few others.
2. **A cancel flag and a cell budget in `Model::evaluate`**, checked in
   `evaluate_cell`, so a recalculation can be stopped (job cancellation, a
   page's time quota) and a runaway formula becomes an error.
3. **Depth limits in the parser and evaluator, and on LAMBDA calls.** Today
   one formula nested a few hundred levels deep overflows a 1 MB stack, and
   `=LET(f,LAMBDA(k,IF(k=0,0,1+f(k-1))),f(100000))` recurses without end;
   the module refuses such formulas before IronCalc sees them and runs
   IronCalc on a 256 MB stack.
