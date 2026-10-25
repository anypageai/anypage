# Studio 工作室：notes

The fusion showcase: one floor plan, and CAD, spreadsheet and PDF working on
the same data in one page.

```
anypage --page pages/studio
```

## What is where

| file | |
|---|---|
| `page.json` | modules `cad`, `sheet`, `pdf`; `scene` optional; files, clipboard; 1600 x 1000 (the wide layout) |
| `main.luau` | layouts, cells, the plan's tiles and overlay, the UI apps, input, agent actions |
| `takeoff.luau` | measuring a drawing (pure), writing the workbook (effects), reading totals back (pure) |
| `report.luau` | the A4 landscape PDF: plan as vector paths, takeoff table (detail pages when long), totals, unit prices, title block |
| `assets/floorplan.dxf` | sample plan (made through the `cad` module's API, see below) |
| `assets/bracket.step` | sample STEP part (a planar B-rep written by the test) |
| `crates/anypage-std/tests/page_studio.rs` | headless tests and the sample generator |

The sample files are regenerated with
`cargo test -p anypage-std --test page_studio -- --ignored generate_sample_assets`.
A regeneration differs from the bundled DXF only in the header timestamps and
a GUID.

## How the graph is used

* **Resources with versions in input cells**: `drawing` (cad.drawing),
  `book` (sheet.workbook), `report` (pdf.document). Every edit returns the
  next version and is `page.set` back; whatever read the old version
  recomputes (tiles, the `totals` cell, the `report_status` badge).
* **Derived cells**: `totals` = the workbook's computed rows, prices,
  subtotal, tax and total (`T.totals(book, takeoff)`); `report_status`
  compares the versions the report was made from with the current drawing,
  workbook and visible layers; `cad.params` / `report.params` feed the tiles.
* **Jobs for slow work**: `loaded` (cad.load, eager, `done` re-measures and
  remakes the report; a failed file restores the previous drawing),
  `part_mesh` (cad.step_mesh, eager), `xlsx` (sheet.export, eager: the
  export follows the workbook off the page's thread, so "save XLSX" hands
  over bytes that already exist, or waits for the running export).
* **Formulas do the arithmetic**: per row `=ROUND(C*E,0)` with `E` pointing
  at the Prices sheet, paint area `=ROUND((partitions*2+exterior)*height,2)`,
  `=SUM(...)`, tax `=ROUND(subtotal*rate,0)`, total. The page never adds
  money itself; tests check the workbook's arithmetic from outside.
* **Agent actions**: `measure_rooms`, `set_price {item, price, unit}`,
  `make_report`, `save {format}`, `select {ids}`, `layer {name, on}`,
  `view {rect | fit}`, `layout {layout}`, plus `takeoff.describe/items/do`
  from the UI library (the wide takeoff app; the narrow copies have no port).
  The `view`, `layers`, `selected` and `part_camera` cells are **not**
  agent-writable; the actions check their input and answer with an error
  (`select` takes one id or a list and refuses ids the drawing does not
  have; `layer` refuses unknown layers and non-boolean `on`; `view` refuses
  a non-rect and clamps to the zoom limits). Every view also sanitises what
  it reads from those cells (`as_ids`, `as_rect`, `as_layers`), and the
  takeoff view survives garbage in `takeoff.state` (the UI library's own
  agent-writable state).
* **set_price matching**: a key, a whole label, or either half of one
  (`磁磚`, `Floor tiles`), or a part of a label that only one item has
  (`tiles`); a part several labels share (`wall`, `floor`) is refused with
  the candidates. The tax rate is percent from 1 up (5 = 5 %, 1 = 1 %) and a
  fraction below 1 (0.05 = 5 %); `unit = "%"` or `"fraction"` says it
  explicitly; more than 100 % is refused.

## The page's layouts

Two arrangements of the same cells, chosen from `page.viewport()`:

* **wide** (`page.json` bounds, 1600 x 1000): every panel at once. Used for
  landscape windows and any window where it shows at 0.6x or more.
* **narrow**: three phone-sized screens (440 x 900 units) side by side at
  x = 2000 and beyond: 平面圖 Plan (plan, layers and selection), 數量
  Takeoff (prices above a 4-column table, the totals), 報告 Report (the PDF
  page, its state and pages). Each screen has a header with the title, tabs,
  that screen's buttons and the status line. Used for portrait windows where
  the wide layout would be smaller than 0.6x (phones, upright tablets, a
  tall desktop window). The STEP part is in the wide layout only.

The `viewport` event shows the arrangement (and the narrow screen last
shown) and fits the camera to it with `page.fit_rect`, so a resized window
shows the whole layout again instead of a corner of it. The tabs (and the
`layout` action) switch screens the same way; `r` fits the screen shown
(the engine's own `r` would fit the wide bounds). Both plan panels share the
`cad.params` cell; both PDF panels share `report.params` (no `rect`: each
tiled layer fills its own rect, both have the page's aspect). The engine
draws only what is in view, so the other arrangement costs nothing.
A narrow screen in a window taller than it is fitted from the top (the
window's leftover height stays below it). Checked in the desktop app
(window resized, screens switched through the `layout` action) at
1600 x 1000, 1100 x 760 (wide, whole), 493 x 960 and 412 x 915 logical
(each narrow screen), and after resizing back; on a phone only through the
same path headlessly (`window_size_picks_the_layout`).

## The report's layout

A4 landscape, laid out from its contents (`report.luau`):

* Page 1 is the drawing sheet: the plan in its frame, the takeoff in the
  right-hand column, then totals, unit prices, and the title block at the
  bottom right. Rows shrink from 12.5 pt to 10.4 pt to keep the table on the
  sheet (up to 17 rows: 12 rooms).
* A longer takeoff moves to detail pages (full width, 36 rows each); the
  totals follow its last row (on a page of their own when they do not fit),
  and the sheet keeps a summary (rooms and floor area, walls, doors and
  windows, which pages hold the details) and the totals.
* Long names are cut with an ellipsis to their column; every text column is
  one pdf edit.
* Bounded cost, so a report fits in one handler (measured, release build:
  `to_paths` + `add_path` cost ~30 µs per entity and ~0.15 ms per text
  glyph): more than 6000 entities on the visible layers print only the
  takeoff layers (walls, doors, windows, rooms, labels), or no plan when
  even those are too many; texts are printed as outlines only up to 1500
  characters and when legible (median height >= 1.2 pt on paper). The sheet
  and the status line say what was left out.
* The report panel pages through a multi-page report (‹ ›).

Tests check the saved PDF of 12, 16 and 40-room drawings (1, 2 and 3 pages):
the total and subtotal are on the sheet and after the last row, every room
is listed, and no two glyph boxes overlap or leave the border (that check
fails on the old fixed layout).

## Tests

`cargo test -p anypage-std --test page_studio` (runs in place). Besides the
takeoff, report and saved files: agent input of the wrong type is refused
or survived (`agent_input_is_checked`), a cancelled press selects nothing
on either plan or UI button (`pointer_cancel_is_not_a_click`), zoom limits,
STEP status, set_price matching and tax rules, big drawings within the
handler budget, 11-digit totals fitting their cards (`big_totals_fit_their_cards`),
the layouts (`window_size_picks_the_layout`), and the error-card check
itself (it sees both the runtime's script-error card and the UI library's
card, by cell status and display list, not by pixel colour).

## Measured (Windows, RTX 3090, release desktop app unless noted)

* Load: 51 ms headless (debug test build, idle machine), 330 ms in the app
  (cold, includes the first sheet recalculation and the report).
* Price edit: 0.6 ms for the edit and recalculation, 10 ms to redraw every
  panel (headless); in the app the HUD shows input-to-screen p50 3.5 to 8.8 ms
  and frame CPU p50 4.7 ms while panning, zooming and typing.
* `make_report` 27 ms, `measure_rooms` 4 ms (headless).
* Big drawings (headless, workspace dev profile; opening = cad.load job,
  then measuring and the report in its done callback):

  | drawing | open + measure + report | measure_rooms | make_report |
  |---|---|---|---|
  | 250 rooms x 10 labels (791 KB) | 186 ms | 82 ms | 41 ms (9 pages, texts left out) |
  | 320 rooms (only 300 measured, said so) | 101 ms | 39 ms | 42 ms (10 pages) |
  | 8 rooms + 20 000 lines (4 MB) | 228 ms | 14 ms | 10 ms (takeoff layers only) |
  | 8 rooms + 60 000 lines (12 MB) | 619 ms | 33 ms | 20 ms (takeoff layers only) |

  Measuring buckets the texts on a grid, so each room tests only the texts
  in its bounding box (250 x 10 took 744 ms before). The load callback runs
  measuring and the report as two steps and says which one failed.

## Requests for the core and modules

Done in the core since the first round, and used here: anypage-std's
dev-dependencies (the tests run in place), duplicate cell names are a load
error, the HUD is off by default, `page.viewport()` and its event (the
layouts), `pointer_cancel` (the plan). Still open:

1. **ui: no `pointer_cancel` handling.** The UI library listens only for
   `pointer_up`, so once a page registers `pointer_cancel` (as this one does,
   so that a cancelled press on the plan is not a click), a press held on a
   UI button stays captured and the next release anywhere in that app
   activates it. The studio works around it in its own `pointer_cancel`
   handler by clearing each app's private `_drag` and the `press` UI state
   (main.luau, "pointer input"); the module-local `captured` stays stale but
   harmless because the studio's pointer handlers are registered before the
   first `ui.app` and run first. Proposed in `crates/anypage-ui/src/luau/app.luau`
   `install()`: `page.on("pointer_cancel", function() local a = captured;
   captured = nil; if a then a:_cancel() end return false end)` with
   `App:_cancel()` dropping `_drag` and `press` without activating, and the
   handler returning false so pages see the event too.
2. **`page.peek` of a cell whose inputs changed returns the old value with
   no sign that it is old.** Handlers here recompute from the input cells
   (`T.totals(page.get(Book), page.get(Meta))`) instead of peeking `totals`.
   Proposed: `page.peek(cell)` returns a second value `fresh` (false when
   dirty), or `page.peek(cell, {fresh = true})` evaluates pure cells.
3. **A job's `done` callback cannot tell which input values the result came
   from.** The XLSX export notes `tostring(page.get(Book))` when `done`
   runs. Proposed: pass the input versions (or the inputs' `tostring`) as a
   second argument to `done`.
4. **cad: no way to set the drawing's units** (`$INSUNITS`). The generator
   patches the DXF text after `dwg:save()`. Proposed `cad.new{units = "mm"}`
   and `cad.set_units(dwg, code)`.
5. **cad: `cad.add` cannot create DIMENSION entities**, so the sample's
   dimensions are exploded (lines, ticks, text on `A-DIMS`), as many exports
   are. Proposed: linear/aligned DIMENSION with a generated `*D` block.
6. **ui: tables have no editable cells**, so unit prices are text fields next
   to the table; and text fields have no `on_blur`, so a half-typed price
   stays as a draft (shown only while it still means the stored price).
7. **ui: hit-testing and `describe` use the last drawn tree.** Headless
   tests must draw a frame (demand the panels) before clicking or calling
   `<app>.describe`, or the app answers as if empty. Worth a sentence in
   the UI library docs, or `describe` could build the tree itself.
8. **cad + pdf: plans go through Luau tables.** `dwg:to_paths` followed by
   `pdf.add_path` costs ~30 µs per entity and ~0.15 ms per text glyph in a
   release build (20 000 lines: 270 + 350 ms; 2 500 short labels: 0.9 +
   2.6 s), most of it converting paths to Luau tables and back, plus a
   Luau loop to recolour by layer. Proposed: a native route,
   `pdf.add_drawing(doc, page, dwg, {fit, layers, colors = {layer -> rgba},
   width, text})` or `to_paths{as = "res"}` returning an opaque path
   resource `pdf.add_path` accepts; or a job form for big drawings. Until
   then the report bounds its plan (see "The report's layout").
9. **runtime: no way to continue work in a later handler.** Slow work in
   Luau (the report for a big drawing) cannot be split across frames: there
   is no `page.defer(fn)` / next-frame callback, only native jobs. Proposed:
   `page.after(seconds, fn)` running `fn` as a handler with its own budget.
10. **Layouts: panel rects are fixed at load.** A page that wants a narrow
    arrangement builds a second set of panels elsewhere in the world (as
    here and in pages/home) and fits the camera to it. It works and costs
    nothing off screen, but every UI app exists twice (two states, two
    agent ports if both had one) and the 3D part is left out of the narrow
    screens rather than shown by a second `scene.view` layer of the same
    scene (a per-page GPU layer identity is still open in MODULES.md §4).
    Panels whose rect is a cell (re-laid out on `viewport`) would let one
    set of panels serve both. Also: the wheel over a narrow screen zooms
    the camera (the engine's default), where a phone-shaped column would
    rather scroll.

## Known limits of the page

* Rooms are closed polylines on layers whose names contain ROOM / AREA /
  SPACE (or 空間 / 房間 / 室); their names come from the largest text inside
  (layers with IDEN / NAME / TEXT preferred); `TILE` / 磁磚 in a room's text
  selects the tile price, else wood. Point-in-room ignores bulges.
* Walls are measured as centre lines (wide polylines or lines on WALL
  layers); double-line walls would be counted twice. Closed or 180 mm+ wide
  outlines count as exterior.
* Doors and windows are block inserts on DOOR / GLAZ / WIN layers or with
  such block names.
* A unitless drawing more than 500 units across is taken as millimetres
  (shown as `mm?`).
* The PDF writes CJK with Adobe's non-embedded CJK fonts (the pdf module's
  choice); viewers substitute a local CJK font.
* The STEP corner needs the `scene` module and the cad `step` feature; the
  page loads without them and says so there (tested).
* At most 300 rooms are measured (the takeoff and the status line say so
  when a drawing has more).
* Zoom is limited to 1/1000 .. 1000 times the drawing's size; room chips
  shrink to one line, then disappear, when a room is smaller on screen than
  the chip.
* Checked in the desktop app: the real open dialog (a 40-room DXF and the
  STEP part), the real save dialog (the 3-page PDF re-opens with the total
  on pages 1 and 3), mouse clicks, typing, drag and wheel.
* Not checked: Android (the package is not in the Android app's assets),
  real OS drag and drop (driven headlessly through `Page::file_drop`), and a
  real `pointer_cancel` (a mouse cannot produce one; driven headlessly).
