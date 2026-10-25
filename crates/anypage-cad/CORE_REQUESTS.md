# anypage-cad: core requests and evaluations

Things the CAD module needs from the runtime / engine, what it does meanwhile,
and the STEP evaluation the module spec asked for.

## 1. Text for tile sources (runtime + engine)

**Need.** Tile sources get `(Rect, &Data)` and nothing else, so CAD tiles cannot
use the page's `TextRaster` (or the coming `anypage-text` `TextSystem`). The
engine's `TextRaster` also only blits axis-aligned bitmaps through `Canvas`,
while drawing text is rotated, mirrored, obliqued and scaled arbitrarily.

**Workaround.** The module loads one system outline font itself (same candidate
list as `TextRaster::load`, CJK first) through `swash` and draws glyph
*outlines* as vector fills (`src/font.rs`; one scaler per thread and a shared
read-mostly outline cache, so tile workers do not wait on each other). Cost: a
second copy of the font in memory (msjh.ttc is ~20 MB) and no font fallback
across scripts beyond what that one font covers. Without any font,
characters are drawn as boxes.

**Proposed.** Hand tile builders an environment, and expose outlines:

```rust
pub struct TileEnv { pub text: Arc<TextRaster> }            // later Arc<TextSystem>
pub type TileFn = Arc<dyn Fn(Rect, &Data, &TileEnv) -> Result<Arc<dyn TileSource>, String> + Send + Sync>;

impl TextSystem {
    /// Shaped glyphs of one line: (font index, glyph id, x advance offset) in font units.
    pub fn shape_line(&self, s: &str) -> Vec<(u16, u16, f32)>;
    /// A glyph outline in font units (y up): move/line/quad/cubic/close.
    pub fn glyph_outline(&self, font: u16, glyph: u16) -> Option<Vec<PathCmd>>;
    pub fn units_per_em(&self, font: u16) -> f32;
}
```

With that, `crates/anypage-cad/src/font.rs` goes away and drawings get the same
fallback chain and shaping (Arabic, Indic) as the rest of the page.

## 2. Functions returning more than one value (runtime)

**Need.** Edits follow the contract "return the next version", but `cad.add`
and `cad.copy` also create ids the page wants.

**Workaround.** `dwg:last_ids()` returns the ids made by the last add/copy.

**Proposed.** Let a native function return several Luau values, e.g.
`Data::Multi(Arc<Vec<Data>>)` unpacked by `install_modules`, so
`local dwg, ids = cad.add(dwg, {...})` works.

## 3. A job's result cannot reach a handler (runtime)

**Need.** Opening a user's file is native work the Luau budget cannot
interrupt (100k entities / 5.4 MB: ~0.4 s release, ~1.6 s debug), so it belongs
in a job. But a job's value is only visible to cell functions; `page.get`
reads input cells only, and there is no callback when a job finishes. A
handler therefore never gets the drawing a `cad.load` job produced, and
edits need it.

**Workaround (implemented, used by the demo).** The handler creates an empty
*pending* drawing (`cad.new({ pending = true })`), stores it as the current
drawing at once, and puts `{ bytes, into = drawing }` in an input cell that
feeds the job `cad.load`. The job parses off the page's thread and fills that
same resource (edits are refused with "the drawing is still loading" until
then); filling happens at most once per drawing, so a re-run of the job never
overwrites edits. Derived cells take the job cell as an input so they redraw
when it completes, and the status line shows `{state = "pending", progress}`.
The drop handler now takes 0.4 ms for the sample and ~30 ms for a 5.4 MB file
(copying the bytes into the cell).

**Proposed.** One of:

```lua
page.job(name, source, inputs, { done = function(value) ... end })  -- handler context
page.on("ready", cell, function(value) ... end)
page.get(cell)                                                     -- last value of any cell, in handlers
```

Then large *saves* could also run in a job and hand their bytes to
`page.save_file` from the callback. Today `dwg:save()` still runs in the
handler (0.25 s release for 100k entities), which is why the demo keeps
`handler_ms` at 1000.

## 4. Converting large results to Luau (runtime)

`dwg:entities({})` on 100 000 entities spends 0.23 s (release; 0.64 s debug)
building the `Data` list, and about 3.5 s more (debug) turning it into Luau
tables in the handler. The module's part is linear now (an id index replaces
the old per-entity linear search; 40k entities used to take 4.1 s). Pages and
agents should filter (`rect`, `type`, `layer`, `ids`, `limit`). A cheaper
`Data` → Luau conversion, or tables materialised lazily from a userdata,
would help every module that returns big lists.

## 5. STEP → mesh: evaluation of truck-stepio 0.3 + truck-meshalgo 0.4

Implemented behind the cargo feature `step`, **on by default** since this
round (the module spec lists it): `cad.step_to_mesh` and the job
`cad.step_mesh` produce the shared mesh form (`positions`, `normals`,
`indices` as little-endian f32/u32 bytes, plus `shells`, `failed`,
`triangles`). With `default-features = false` both return
`STEP import is not built into this app (anypage-cad feature "step")`.

`cad.step_to_mesh` runs on the calling thread, so it takes files up to 4 MB
and 2 million triangles and points larger ones to the job (20 million
triangles). The job checks cancellation between shells; truck's
tessellation of one shell cannot be interrupted.

| check | result |
|---|---|
| builds on Windows x86_64 | yes |
| `cargo ndk -t arm64-v8a -P 26 check` (default features, so with STEP) | passes |
| licenses | truck-* and ruststep Apache-2.0; their tree is MIT/Apache/BSD |
| cube and cylinder written by truck's STEP writer, read back | volume 1.0 and 2π within 1 % (tests/step.rs) |
| same cube rewritten in commercial AP214 style (ADVANCED_FACE, FACE_OUTER_BOUND, styled items / colours, a `REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION` complex instance, product categories) | read, volume exact |
| real files from SolidWorks / Creo / NX / FreeCAD | **not verified**: no such files on this machine and tests may not download |

Known gaps, read from truck-stepio 0.3's source (`src/in/mod.rs`):

- No `TRIMMED_CURVE`, `OFFSET_SURFACE`, `DEGENERATE_TOROIDAL_SURFACE`; faces
  using them fail per shell (reported in `failed`, other shells still mesh).
- Assembly placements (`ITEM_DEFINED_TRANSFORMATION`, `MAPPED_ITEM`,
  `CONTEXT_DEPENDENT_SHAPE_REPRESENTATION`) are not applied: parts of an
  assembly come out at their own origins.
- Units are not applied: coordinates are in the file's length unit (usually mm).
- Colours are not read.
- `ruststep` pulls `nom` 3.2.1 and `quick-xml` 0.22, which rustc flags as
  "will be rejected by a future version of Rust". If a future toolchain
  rejects them, build with `default-features = false` until truck moves on.

**For integration:** docs/MODULES.md should say STEP is on by default and
not yet tried on real exports; try a corpus of real files before relying on
it. If assemblies matter, the placement graph can be walked here (it is plain
entity data) and applied to the meshes per shell.

## 6. Memory of the `dxf` crate (upstream, affects limits)

Measured with `examples/memory.rs` (counting allocator): a 5.4 MB DXF of
100 000 entities is 103 MB live after the `dxf` crate's parse and peaks at
262 MB during it; the module's scene adds 62 MB. So the module accepts DXF up
to 256 MB on desktop and **32 MB on Android** (about 0.6 GB live, 1.5 GB peak).
Blocks are no longer expanded (they are instanced), so the scene follows the
file's size: the reviewer's 20 KB nested-block file that used to take 10 s and
5.15 GB now opens in about 1 ms and 0.5 MB (release). A streaming DXF reader
(entities parsed straight into the scene) would cut the parse memory by an
order of magnitude; it is a larger piece of work than this module round.

## 7. Upstream issues in `dxf` 0.6.2 worked around here

Not core requests, but worth knowing at integration:

- **HATCH is not supported** by the crate (skipped on read, cannot be
  written). `src/hatch.rs` reads HATCH from the raw ASCII code pairs (model
  space and blocks, file order kept), draws it, and splices it back into the
  saved text after the entity it followed. Binary DXF hatches are not read.
  Gradient data and source-boundary links are not kept (written
  non-associative).
- **ATTRIB round trip in 2007+ files**: the writer adds an `AcDbXrecord` tail
  and an embedded MTEXT after every ATTRIB, and its own INSERT reader then
  stops at that MTEXT (stray MTEXT / SEQEND / ATTRIB entities at top level).
  Saving drops the tail and the embedded MTEXT; loading re-attaches strays to
  their INSERT.
- Sub-entity handles (ATTRIB, VERTEX, SEQEND) read from files keep a
  placeholder `FFFFFFFFFFFFFFFF`; they get real handles on load and when
  entities are added, so saving changes nothing in the drawing.
- `Drawing::remove_entity` is `Vec::remove`; bulk deletes swap kept entities
  forward and pop the rest off the end (one pass).
- Text in legacy code pages: the crate reads everything as Windows-1252 before
  R2007. The module reads `$DWGCODEPAGE` (Big5, GBK, Shift_JIS, EUC-KR,
  1250–1258) and passes that encoding, falling back to 1252 on decode errors;
  `\U+XXXX` escapes are decoded for all code pages.
- Parse speed: the crate parses ~15 MB/s (100k entities in 5.4 MB: 0.3 s
  release). It is three quarters of `cad.open` time.

## 8. Integration notes

- New dependencies: `dxf` 0.6.2 (MIT; brings `chrono`, `image` with BMP only,
  `uuid` v4, `encoding_rs`, `itertools`, `num`), `tiny-skia` 0.11 and `swash`
  0.2 at the engine's versions, `encoding_rs` 0.8, and (default feature
  `step`) truck-stepio 0.3, truck-meshalgo 0.4, truck-modeling 0.6,
  truck-topology 0.6.
- `anypage_cad::register(&mut natives)` adds functions, the jobs `cad.load`,
  `cad.render`, `cad.step_mesh`, and the tile source `cad.drawing`.
