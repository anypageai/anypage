# Core requests from the ui library

Things the runtime or the engine could change so the UI library is faster or
more complete. The library works without any of them; each entry says how
it works around the gap today. Numbers are from single-threaded runs of
`cargo test --test perf -- --test-threads=1` and `cargo test --test limits`
(Windows, debug test profile with dependencies at opt-level 3), against the
runtime as of 2026-10-24 evening.

## 1. Cheaper script time budget (runtime, `realm.rs`)

**Status.** Partly addressed: the interrupt now reads the clock only every
128 calls. Each Luau call and loop back-edge still crosses into a Rust
closure (`set_interrupt`), which is the remaining overhead. Not re-measured
since the change.

**Proposal.** Check a plain `AtomicBool` that a watchdog thread sets when the
deadline passes, from a C-level `lua_callbacks(L)->interrupt`, so the hook
never leaves C. No API change.

## 2. Large inputs without per-item conversion (runtime, `data.rs` / `ops.rs`)

**Why.** Every evaluation converts each input cell `Data` into Luau tables,
item by item: about 0.5 us per item per frame now (it was 3-4 us). A list
over a 20 000-item cell spends about 10 ms per frame on that (0.4 ms in
Luau); 100 000 items would take about 50 ms, the whole default budget.

**Workaround.** `ui.list{count = n, item = function(i) ... end}` and
`ui.table{columns = ..., count = n, row = function(i) ... end}` read rows
on demand (from a native resource's method, or a table the page built at
load). A 100 000-row list then costs about 1.1 ms per wheel frame.

**Proposal.** Pass big lists and maps (say, more than 256 entries) to scripts
as a read-only userdata view over the `Arc` (`__index`, `__len`, `__iter`),
and convert a returned view back to its `Data` without a copy:

```rust
// data.rs
pub struct DataView(pub Data);           // UserData: __index(i | key), __len, __iter
pub fn input_to_lua(lua: &Lua, i: &Input) -> mlua::Result<LuaValue>;  // big List/Map -> DataView
// Data::from_lua: a DataView userdata converts back to its Data (no copy)
```

## 3. Faster display-list conversion (runtime, `draw.rs` / `ops.rs`)

**Why.** A panel's commands are Luau tables converted to `Data` one by one.
For the 200-control form (451 commands) a frame after a keystroke takes
2.7 ms, of which 1.9 ms is in Luau; most of the rest is that conversion.

**Proposal.** Have `Draw` record into a Luau `buffer` (opcode and f64
operands, plus a side table for strings and resources) that `DisplayList`
decodes natively. Equality stays byte equality; `d:rect(...)` does not change.

## 4. Rectangle clips without a full-canvas mask (engine, `raster.rs`)

**Why.** Each `Canvas::push_clip` / `pop_clip` allocates and fills a mask the
size of the whole canvas (1.7 MB at 1180 x 1500). Lists, tables, scroll
containers and overflowing text areas each push one.

**Workaround.** The library clips only when content really overflows. Text
fields never clip: they draw only the glyphs that fit.

**Proposal.** Keep the current rectangle as plain bounds and clip fills,
strokes and glyph blits against it; build a `Mask` only for non-rectangular
clips.

## 5. Pointer events: position on release, modifiers

**Why.** `Page::pointer_up()` has no coordinates, so the library uses the
last drag position. Pointer events carry no modifiers, so Shift+click cannot
extend a selection and Ctrl+click cannot multi-select.

**Proposal.**

```rust
fn pointer_up(&mut self, wx: f32, wy: f32);                   // script: pointer_up(x, y)
fn pointer_down(&mut self, wx: f32, wy: f32, mods: &str)      // script: pointer_down(x, y, "shift+ctrl")
```

## 6. Hover changes should schedule a frame (engine, `engine.rs`)

**Why.** `Engine::pointer_move` forwards hover to the page but does not call
`after_input`. A host that redraws only on `after_input` will not show hover
highlights or start tooltip timers until something else wakes it.

**Proposal.** Call `after_input()` from the hover branch of
`Engine::pointer_move` (or only when the page set an input cell).

## 7. Wall-clock `page.now()` in handlers (runtime)

**Why.** `page.now()` returns the clock of the last tick, and `Engine::frame`
ticks only when a visible sink is due, so after a quiet spell it can be
seconds or minutes old. Everything a handler stamps with it is wrong by that
much: a toast created after 100 idle seconds ended "97 seconds ago" and
vanished on the next frame; two clicks five seconds apart counted as a
double click; a tooltip skipped its delay; the caret blink phase started in
the past.

**Workaround (in place).** Durations between two events (double clicks) use
`os.clock()`. Stamps that a draw compares with its clock `t` (toast end,
tooltip start, blink phase) use an estimate of the current page time:
the page clock runs at wall-clock rate, so `clock - os.clock()` is constant,
and every observation of the clock (a draw's `t`, `page.now()` in a
handler) is at or before the true time; the largest `observed - os.clock()`
seen is the estimate (`ui/util` `now()`). Draws stay pure functions of their
inputs and `t`. `tests/timing.rs` drives frames with the engine's rule and
real waits, and fails without the workaround. The estimate can lag by the
time between a tick and the first draw that sees it (milliseconds).

**Proposal.** In handlers, return the current time on the page clock's
scale (`start.elapsed()`), not the last tick. Cell functions keep reading
the clock cell.

## 8. Detecting state changes that did not come from the app

**Why.** Handlers reuse the last laid-out tree until the app itself changes
state. A write to `<name>.state` through the semantic port (`{"set": ...}`)
is not visible to handlers until the next frame redraws the panel.

**Proposal.** Add `page.version(cell) -> number` for input cells; the library
would key its tree cache on the versions of the state and UI cells.

## 9. Text drawing for scrolled fields and font styles (engine / text system)

**Why.** A horizontally scrolled text field draws a substring at the width
of its prefix. With real shaping (Arabic joining, ligatures) a cut
substring can shape differently from the full line. Headings have only size
to stand out: there is no weight.

**Proposal.**
- A clipped text command, `{ "text", x, y, size, color, s, clip_x0, clip_x1 }`,
  that shapes the whole line and blits only glyphs inside the range (with
  request 4 this can simply be a cheap rectangle clip).
- An optional weight on text commands, `{ ..., weight = 600 }`.

## 10. Pointer cursor requests (engine / hosts)

**Why.** Desktop users expect an I-beam over text fields and a hand over
links and buttons.

**Proposal.** `page.cursor("default" | "text" | "pointer" | "grab")`, mapped
to `PageRequest::Cursor(String)` for hosts that have a mouse.

## 11. A pointer_cancel event (engine `engine.rs`, runtime `page.rs`)

**Why.** `Engine::pointer_down` for a second finger (a pinch starts) and
`Engine::pointer_cancel` both call `page.pointer_up()`. The page cannot tell
that from a release, so the library treats it as one at the last position:
a press held on a button is activated. On a touch screen, starting a pinch
on a button clicks it. (Documented in `ui/app` and the crate docs.)

**Proposal.**

```rust
// engine/page.rs, trait Page
fn pointer_cancel(&mut self) { self.pointer_up() }   // default keeps old pages working
// engine.rs: call page.pointer_cancel() at pinch start and in Engine::pointer_cancel
// runtime: dispatch a "pointer_cancel" script event
```

The library would then clear the press and the drag without activating.
Together with request 5 (`pointer_up(x, y)`) this makes release handling exact.

## 12. A native text document for large text areas (text system / runtime)

**Why.** A text area keeps its wrapped lines in Luau: each paragraph is laid
out with `text.layout` (one native call, plus one Luau table per line) and
cached by paragraph string, so a keystroke lays out only its paragraph. The
first layout of a value still costs about 6.5 us per wrapped line, inside
one draw under `call_ms`, and a keystroke in a very long paragraph lays all
of it out again. Measured at the library's limit of 128 KB: first frame
14 ms (wrapping prose), keystroke 11-16 ms (one 128 KB paragraph), 4 ms
(51 000 one-word lines). 256 KB took 25 ms and up to 32 ms, too close to the
default 50 ms budget, so `ui.limits.area` is 128 KB; longer values set from
outside are shown read-only.

**Proposal.** A `text.doc` resource owned natively: `text.doc(s)` ->
userdata with `:edit(from, to, insert)` (incremental re-wrap, resynchronising
after the edited line), `:line_count(size, width)`, `:lines(i0, i1)`
(visible lines only), `:line_at(pos)`, `:caret(pos)`, `:hit(line, x)`.
A cheaper stopgap: `text.breaks(s, size, width) -> buffer` of line ends,
which removes the per-line table.

## 13. A glyph cache that does not lock per character (engine, `raster.rs`)

**Why.** `TextRaster::glyph` takes the `state` mutex for every character
measured (`measure`, `caret_x`, `hit_x`, `layout_ranges`). Pages sharing a
`TextRaster` with each other or with raster threads serialise on it: when
the ui tests shared one raster, layout ran 3-4 times slower and a 128 KB
text area exceeded the 50 ms budget that it meets alone in 14 ms. The tests
now give each page its own raster, as each engine has; in an app, the
script thread still contends with tile and panel rasterisation.

**Proposal.** Advance widths in a lock-free or read-mostly structure (an
`RwLock` with a read fast path, or per-thread caches of advances), and take
the lock once per string rather than once per character.
