//! CAD module (`cad`): DXF drawings read, drawn as crisp vector tiles at
//! every zoom, measured, edited and written back; drawings exported as
//! vector paths (for `pdf.add_path` and panels); STEP solids to meshes
//! (cargo feature `step`, on by default).
//!
//! A page declares `"capabilities": {"modules": ["cad"]}`.
//!
//! # Luau API
//!
//! Drawings are resources of kind `cad.drawing`; every function below is
//! also a method (`dwg:layers()` is `cad.layers(dwg)`). Coordinates are
//! drawing units with y up; rects are `{ x, y, w, h }` with (x, y) the lower
//! left corner. Entity ids are DXF handles (numbers), stable across saves.
//!
//! | function | |
//! |---|---|
//! | `cad.open(bytes, "dxf")` | parse a DXF (ASCII or binary, any version; legacy code pages such as Big5/GBK/Shift_JIS from `$DWGCODEPAGE`). Runs on the calling thread: for files from users prefer the job `cad.load` |
//! | `cad.new()` | an empty drawing (AutoCAD 2010 DXF) |
//! | `cad.new({ pending = true })` | an empty drawing that the job `cad.load` fills (`{bytes, into}`); edits are refused until then |
//! | `dwg:info()` | `{version, encoding, entities, drawn, layers, blocks, units, bounds, warnings, font, loading, load_error, load_ms}` |
//! | `dwg:layers()` | `{ {name, color, aci, on, entities} }` |
//! | `dwg:blocks()` | `{ {name, entities, base, anonymous} }` |
//! | `dwg:bounds({margin})` | `{x, y, w, h}` of the model space (nil when empty) |
//! | `dwg:entities({layer, type, ids, rect, limit})` | `{ {id, type, layer, color, aci, bounds, ...geometry} }` in drawing order |
//! | `dwg:entity(id)` | one entity or nil |
//! | `dwg:measure(ids)` | `{length, area, count, missing}` from the drawn geometry (inserts expanded, arcs exact) |
//! | `dwg:pick(x, y, tol, {layers})` | the id nearest to a drawing point, topmost first, or nil |
//! | `dwg:select(rect, {inside, layers})` | ids crossing (or inside) a rect |
//! | `dwg:to_paths({fit, flip_y, rect, layers, width, text, foreground})` | `{ {path, stroke, width} or {path, fill, even_odd}, id, layer } }` in the shared vector path form; arcs as cubic Béziers, text as glyph outlines |
//! | `dwg:save()` | DXF bytes (hatches included); the drawing itself is not changed |
//! | `dwg:last_ids()` | ids created by the last `cad.add` / `cad.copy` |
//! | `cad.to_drawing(layer_rect, view_rect, x, y)` / `cad.to_page(...)` | map between page and drawing coordinates for a tiled layer |
//! | `cad.zoom(view_rect, x, y, factor)` | a view rect zoomed about a drawing point; an error when the result would overflow or be finer than f64 resolves there |
//! | `cad.step_to_mesh(bytes, {tolerance})` | STEP → mesh form `{positions, normals, indices, shells, failed, triangles}`, files up to 4 MB and 2 million triangles (larger: the job `cad.step_mesh`) |
//!
//! Edits (call from handlers): `cad.add(dwg, entity_or_list)`,
//! `cad.add_block(dwg, {name, base, entities})`, `cad.move(dwg, ids, dx, dy)`
//! (all or nothing), `cad.copy(dwg, ids, dx, dy)`, `cad.delete(dwg, ids)`,
//! `cad.set_layer(dwg, ids, layer)`, `cad.set_color(dwg, ids, aci | "bylayer" |
//! "byblock" | {color})`, `cad.add_layer(dwg, {name, aci | color, on})`,
//! `cad.layer_on(dwg, name, on)`. Each returns the drawing at a version newer
//! than any handed out before, whichever handle it was given (a handle kept
//! from load time still produces a new value). A list given to `cad.add`,
//! and `cad.copy` / `cad.delete` of many ids, update the scene once.
//! Entities to add use the keys `entities` returns: `LINE` (x1, y1, x2, y2),
//! `CIRCLE` (cx, cy, r), `ARC` (+ start, end degrees), `ELLIPSE` (cx, cy, mx,
//! my, ratio, start, end), `LWPOLYLINE` (points, bulges, closed, width),
//! `SPLINE` (degree, control, knots, weights, fit), `TEXT` (x, y, text, height,
//! rotation, halign, valign), `MTEXT` (x, y, text, height, width, attachment),
//! `INSERT` (block, x, y, sx, sy, rotation, attributes), `POINT`, `SOLID`
//! (points), `HATCH` (loops, bulges, solid or pattern ANSI31/ANSI32/ANSI37/LINE/DASH/NET,
//! angle, scale); all take `layer` and `aci` (a whole number 0–256) or
//! `color` (0xRRGGBBAA, stored as true colour).
//!
//! Jobs: `cad.load` (input: bytes → a new drawing; `{bytes, into = pending
//! drawing}` → fills that drawing and returns its next version; nil → nil;
//! reports progress, cancellable after the parse); `cad.render` (inputs:
//! drawing, `{width_px, height_px, rect, layers, background}`) → image
//! (`cad.image`); `cad.step_mesh` (input: bytes) → mesh (cancellable between
//! shells).
//!
//! Tiles `cad.drawing`: params `{ drawing, rect = view rect, layers = {names}
//! or {name = bool}, background, foreground, selected = ids, highlight,
//! line_width }`. The view rect is fitted into the tiled layer's rect
//! (uniform scale, centred, y flipped).
//!
//! # What is drawn
//!
//! LINE, LWPOLYLINE and POLYLINE (bulges, constant and tapered widths, 3D
//! polylines, polygon and polyface meshes as wireframes), CIRCLE, ARC,
//! ELLIPSE (exact elliptic arcs at any zoom), SPLINE (non-rational degree
//! ≤ 3 converted exactly to Béziers; rational or higher degree sampled; fit
//! point splines as Catmull-Rom, an approximation), TEXT, MTEXT, ATTRIB and
//! top-level ATTDEF (system outline font; see below), INSERT (nested blocks,
//! MINSERT arrays, OCS / mirrored inserts), DIMENSION (its block; text only
//! when the block is missing), HATCH (solid, gradient as solid, pattern lines
//! with dashes clipped to the boundary; very dense patterns as a translucent
//! fill), SOLID, TRACE, 3DFACE, POINT, LEADER (no arrowhead), MLINE (centre
//! line), RAY, XLINE, IMAGE (frame only). Layers on/off, ACI and true
//! colours, BYLAYER / BYBLOCK, ACI 7 contrasting with the background.
//! Linetypes are drawn solid; lineweights as hairlines. Paper space is not
//! drawn (model space only). 3D content is shown in plan view (z dropped).
//!
//! Blocks are built once and drawn as instances (INSERTs and dimensions
//! keep a transform, not a copy), so memory follows the file: a 20 KB file
//! whose blocks nest 100 inserts three deep (10^8 lines) opens in a few ms
//! and under 1 MB. Rendering walks instances with culling and draws an
//! instance under a pixel as one pixel, with a cap on the work per image.
//!
//! Text: TEXT and MTEXT are laid out with one system outline font (CJK
//! capable when the system has one) because tile code has no access to the
//! page's text system; SHX fonts and inline MTEXT formatting (font, height,
//! colour changes) are not reproduced, stacked fractions become `a/b`. When
//! no font exists, characters are drawn as boxes.
//!
//! Files: HATCH is read from ASCII DXF only (the `dxf` crate does not know
//! it); gradient data and links to boundary objects are not kept on save.
//! Saving writes ASCII DXF in the drawing's version (an R12 drawing with
//! hatches is written as AutoCAD 2000; `info().warnings` says so from the
//! moment the hatches arrive). DWG is not supported.
//!
//! STEP (feature `step`, truck-stepio 0.3): B-rep shells to triangles; no
//! assembly placements, units or colours; TRIMMED_CURVE and offset surfaces
//! are not read (those shells are listed in `failed`). Not yet tried on
//! exports from commercial CAD systems. See CORE_REQUESTS.md.
//!
//! # Limits
//!
//! DXF files up to 256 MB (32 MB on Android: the `dxf` crate holds a parsed
//! file in about 20 times its size and peaks near 50 times while parsing).
//! Scene geometry up to 1 GB (256 MB on Android), with a warning when hit.
//! Picking subdivides curves only near the point and stops after a fixed
//! amount of work; measuring blocks under uneven scales stops with an error
//! past 4 million segments; `to_paths` refuses up front what would exceed
//! 8 million values. Loops over angles, counts and hatch patterns read from
//! files do a bounded amount of work whatever the values (huge or
//! non-finite angles, far-away pattern origins).
//!
//! Speed (release, Windows desktop, 100 000 entities / 5.4 MB): `cad.open`
//! ~0.4 s of which the `dxf` crate's parse is three quarters
//! (`dwg:info().load_ms` has the phases), a deep-zoom tile under 1 ms, a
//! whole-drawing overview tile ~0.11 s, `entities({})` ~0.23 s before the
//! conversion to Luau tables (which costs more; filter with `rect`, `type`,
//! `layer` or `limit`).

mod aci;
mod api;
mod doc;
mod font;
mod geom;
mod hatch;
mod render;
mod scene;
mod step;
mod text;

pub use api::{default_background, default_foreground, doc_of, entity_from_data, measure, pick, render_image, tile_source, KIND};
pub use doc::{CadDoc, MAX_DXF_BYTES};

use anypage_runtime::Natives;

/// Add this module's functions, jobs and tile source.
pub fn register(n: &mut Natives) {
    api::register(n);
}

/// Internals exposed for tests and benchmarks.
#[doc(hidden)]
pub mod internals {
    pub use crate::aci::rgb as aci_rgb;
    pub use crate::font::global as font;
    pub use crate::geom::{Aff, BBox, P};
    pub use crate::render::{fit, render, Style};
    pub use crate::scene::Scene;

    /// `dwg:entities(filter)` without the Luau conversion (for timing).
    pub fn entities(dwg: &anypage_runtime::Data, filter: &anypage_runtime::Data) -> Result<anypage_runtime::Data, String> {
        let (doc, _) = crate::api::doc_of(dwg)?;
        let st = doc.lock();
        crate::api::entities_data(&st, filter)
    }
}
