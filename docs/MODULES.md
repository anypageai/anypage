# AnyPage 能力模組：契約與規格

本文件是每個能力模組（PDF、試算表、CAD、雲端、媒體、裝置、3D、文字、UI）的共同契約。模組是一個 Rust crate，用 `register(&mut Natives)` 把函式、工作、串流、tile 與 GPU 圖層登記給頁面腳本使用。頁面在 `page.json` 宣告它要用哪些模組，沒宣告的用不到。

The contract below is normative. Where it says "must", tests should prove it.

## 1. Ground rules for module work

- **Ownership.** Each module lives in its own folder: `crates/anypage-<name>/` plus its demo package `pages/<name>-demo/`. Change nothing else. If the core (runtime, engine, core) needs a change, write it in `crates/anypage-<name>/CORE_REQUESTS.md` with the reason and a proposed signature, and work around it meanwhile.
- **Standalone build.** While modules are built in parallel each crate is its own workspace (its `Cargo.toml` has `[workspace]`), with its own `Cargo.lock` and `target/`. It depends on the shared crates by path. Do not add it to the root workspace; integration does that.
- **Ground truth is the source.** Read `crates/anypage-runtime/src/natives.rs`, `data.rs`, `api.rs`, `page.rs` and `crates/anypage-engine/src/page.rs`, `raster.rs`. `crates/anypage-runtime/tests/core_api.rs` shows every mechanism used end to end.
- **Platforms.** Everything must build for Windows (x86_64-pc-windows-msvc) and Android (aarch64-linux-android, x86_64-linux-android). Check Android with
  `cargo ndk -t arm64-v8a -P 26 check` (env `ANDROID_NDK_HOME=%LOCALAPPDATA%\Android\Sdk\ndk\27.1.12297006`; the repo's `.cargo/config.toml` links C++ statically). A feature that cannot exist on a platform (USB host on iOS, a camera API) is compiled out with `cfg`, and the module either omits the function or returns a clear error such as `"usb is not available on this platform"`. Pages list such modules in `optional_modules`.
- **Dependencies.** Permissive licenses only (MIT, Apache-2.0, BSD, ISC, Zlib, MPL-2.0). No AGPL/GPL. No prebuilt binaries downloaded at build or run time; a pure-Rust or source-built (cc) path is required for Android. No `aws-lc-rs` (needs NASM/CMake on Windows): use `ring` or RustCrypto.
- **No network in tests** except `127.0.0.1` servers the test starts itself.
- **Quality.** `cargo clippy --all-targets` clean, `cargo test` green, no `unwrap()` on data from pages or files, no panics across the native boundary (return `Err(String)`), size limits on everything a page can make big, cancellation honoured in long jobs.

## 2. The runtime contract (summary)

```rust
pub fn register(n: &mut Natives) {
    // inline, on the calling thread; Pure ones may run inside cell functions
    n.function("pdf.page_count", Effect::Pure, |c: &Call| -> Result<Data, String> { ... });
    // changes state (a resource, the world): load time and event handlers only
    n.function("pdf.add_text", Effect::Effect, |c| { ... });
    // async on the worker pool; inputs are the values of the cells the page passes
    n.job_fn("pdf.render", |j: &JobCtx| { j.cancelled(); j.progress(0.5); ... });
    // a stream on your own thread(s); `ctx.requests` delivers the page's request cell values
    n.source("cloud.chat", |ctx: SourceCtx| -> Result<SourceGuard, String> { ... });
    // tiles: pixels as a pure function of (tile key, params cell value)
    n.tile("pdf.page", |rect, opts| Ok(Arc::new(MyTiles) as Arc<dyn TileSource>));
    // GPU layer: convert inputs to a value, the renderer draws it into an offscreen texture
    n.layer("scene.view", renderer, |inputs: &[Data]| -> Result<Value, String> { ... });
    // a Luau library pages load with import("ui")
    n.lua_module("ui", include_str!("ui.luau"));
}
```

Script side, for a page whose `page.json` has `"capabilities": {"modules": ["pdf"]}`:

```lua
local doc = pdf.open(file.data)          -- function: global table named after the module
local n = doc:page_count()                -- res:method(...) calls pdf.method(res, ...)
local thumb = page.job("thumb", "pdf.render", { doc_cell, page_cell })
local chat = page.stream("chat", "cloud.chat", { request = req_cell, latest = true })
page.tiled({ rect = R, source = "pdf.page", params = params_cell })
page.layer({ rect = R, kind = "scene.view", inputs = { scene_cell, camera_cell } })
```

### Values

`Data` is `Nil | Bool | Num | Str | Bytes | List | Map | Res`. Maps have string keys (`snake_case`). Numbers are `f64`. Positions in text are byte offsets `0..=len` (bytes before the caret).

`Res` is a handle to a native object: `Res::new("pdf.document", doc)`. Equality is (id, version). A resource mutated in place (a workbook after `set`) is returned as `res.next_version()` so everything that read the old version recomputes. Kinds are `"<module>.<type>"`; `res:method()` resolves `method` in the module's table.

### Shared interchange formats (all modules must use these)

| what | form |
|---|---|
| bytes | `Data::Bytes`, or a `Res` whose object is `Vec<u8>` (kind `fs.bytes`, what file dialogs and drops give). Accept both via `Data::as_bytes()`. |
| image | `Res` whose object is `anypage_engine::Image` (premultiplied RGBA8, `newest` = production time for latency stats). Kind `"<module>.image"`. `page.image{source = cell}` shows it with no copy; `d:image(img, x, y, w, h)` draws it in a panel. |
| table | `{ columns = { "A", "B" }, rows = { { v, v }, ... } }` with cell values Num/Str/Bool/Nil. |
| mesh | `{ positions = Bytes(f32 LE xyz...), indices = Bytes(u32 LE, optional), normals = Bytes(f32 xyz, optional), colors = Bytes(u8 rgba, optional) }` |
| points | `{ positions = Bytes(f32 LE xyz...), colors = Bytes(u8 rgba, optional) }` |
| vector path | `{ "M", x, y, "L", x, y, "C", x1, y1, x2, y2, x, y, "Z" }` flat list; with `stroke = color, fill = color, width = n` alongside where used. |
| rect | `{ x, y, w, h }` |
| colour | packed `0xRRGGBBAA` number (`color.rgba(...)` in Luau) |

### Security

Every native call receives `Grants`: `grants.check_url(url)` must pass before any network access (patterns from `capabilities.net`), and `grants.secret(name)` is the only way to obtain credentials (declared in `capabilities.secrets`, kept by the host; never return a secret to the script, never log it). File paths are never taken from scripts; files come from dialogs and drops as bytes.

### Tests a module must have

1. Unit tests of the native logic.
2. Headless page tests: load a real page with `ScriptPage::load(&Package::from_files(...), &natives, text, wake)` and drive it through the Luau API, as in `crates/anypage-runtime/tests/core_api.rs`.
3. Visual output written as PNG under `target/test-output/` when the module draws (tiles, images, GPU layers), and checked numerically (pixel colours, coverage), not only by eye.
4. `cargo ndk -t arm64-v8a -P 26 check` passes (or the module documents why a part is cfg'd out).

### Demo package

`pages/<name>-demo/page.json` + `main.luau` showing the module working with plain `page.panel` drawing (the UI library is being built in parallel). The demo must load in a headless test.

## 3. Module specifications

The function lists are the minimum; add what a real app needs. Names are `module.function`.

### text — `crates/anypage-text` (no runtime dependency; the engine will depend on it)

A text system good enough to edit text in every language: OpenType shaping (Arabic joining, Indic reordering, Thai, CJK, emoji with ZWJ sequences), bidirectional text, font fallback across scripts, line breaking (UAX #14), grapheme-aware carets. Use `cosmic-text` (or `parley`) with `swash`. Must be `Send + Sync` (internal locking) because tile and raster threads share it.

```rust
pub struct TextSystem;
impl TextSystem {
    pub fn load() -> Result<Self, String>;            // system fonts; on Android /system/fonts
    pub fn from_fonts(fonts: Vec<Vec<u8>>) -> Self;    // tests, bundled fonts
    pub fn describe(&self) -> String;                  // which fonts were found
    pub fn measure(&self, s: &str, px: f32) -> f32;
    pub fn line_metrics(&self, px: f32) -> (f32 /*ascent*/, f32 /*line height*/);
    pub fn caret_x(&self, s: &str, px: f32, byte: usize) -> f32;      // visual x, bidi aware
    pub fn hit_x(&self, s: &str, px: f32, x: f32) -> usize;           // nearest grapheme boundary
    pub fn layout_ranges(&self, s: &str, px: f32, max_w: f32) -> Vec<(std::ops::Range<usize>, f32)>;
    /// Draw one line into premultiplied RGBA8 `dst` (w x h), clipped to `clip` (x0, y0, x1, y1 px),
    /// top-left at (x, y_top) px, colour non-premultiplied. Returns the advance in px.
    pub fn draw_line(&self, dst: &mut [u8], w: u32, h: u32, clip: (i32, i32, i32, i32), x: f32, y_top: f32, px: f32, color: [u8; 4], s: &str) -> f32;
}
```

Semantics must match the engine's current `TextRaster` (same names in `crates/anypage-engine/src/raster.rs`) so integration is a swap. Tests: widths and carets for Latin, CJK, Arabic (RTL, joined forms narrower than isolated), Hebrew, Devanagari, Thai, emoji ZWJ; line breaking with `\n`, long words, CJK; draw output PNGs; time to load fonts and to shape 10 000 characters.

### ui — `crates/anypage-ui` (Luau library `import("ui")` + tests + `pages/ui-gallery`)

A component library for page scripts. The design below is the target API; refine it but keep its spirit (state in cells, views as pure functions, handlers as commands).

```lua
local ui = import("ui")
local app = ui.app({
    rect = { x, y, w, h }, name = "settings",
    inputs = { cell_a, cell_b },          -- graph cells the view reads
    state = { count = 0, name = "" },      -- initial app state, kept in an input cell `app.state`
    theme = ui.themes.dark,                -- or ui.themes.light, or a custom table
    view = function(s, a, b)               -- pure: returns an element tree
        return ui.column({ gap = 8, padding = 16 }, {
            ui.text("Hello " .. s.name, { size = 20 }),
            ui.row({ gap = 8 }, {
                ui.button("Add", { on_click = function() app:update(function(st) st.count += 1 end) end }),
                ui.button("Reset", { variant = "secondary", on_click = function() app:set("count", 0) end }),
            }),
            ui.textfield({ value = s.name, placeholder = "Name", on_change = function(v) app:set("name", v) end }),
            ui.list({ items = a, height = 300, row_height = 28, selected = s.sel,
                      row = function(item, i, selected) return ui.text(item.title) end,
                      on_select = function(item, i) app:set("sel", i) end }),
        })
    end,
})
```

Required elements: `text` (wrapping), `button` (variants, disabled), `checkbox`, `switch`, `radio`, `slider`, `textfield` (single line), `textarea` (multi-line, wrapping, scrolling), `select` (dropdown), `tabs`, `list` (virtualised, keyed, selection, keyboard navigation), `table` (header, columns, virtualised rows; the shared table format), `scroll` (wheel, drag, scrollbar), `row`/`column`/`stack`/`grid` with `gap`, `padding`, `align`, `justify`, `grow`, fixed/min/max sizes, `spacer`, `divider`, `image`, `progress`, `badge`, `card`, `dialog`/`toast` (overlays within the app's rect), tooltips on hover.

Behaviour: hover and pressed states, keyboard focus with Tab/Shift+Tab, Enter/Space activate, text editing with grapheme-correct caret movement (`text.next`/`text.prev`/`text.next_word`), selection with Shift+arrows and mouse drag, Home/End, Ctrl+A/C/X/V (via `page.copy`/`page.paste` when the page has the clipboard capability), IME preedit shown inline (`ime` event), `page.focus_text(caret_rect)` while a field is focused so hosts show the keyboard / place the IME window, scroll containers consume `wheel`. Each app re-renders only when its state cell, its UI-state cell or its inputs change. Lists of 100 000 rows must stay fast (render only visible rows; measure: view + layout time per frame).

Tests drive real pages headlessly (pointer, keys, text, wheel) and assert state changes and display-list content. The gallery page shows every element in dark and light themes.

### pdf — `crates/anypage-pdf` (module `pdf`)

Prefer a pure-Rust stack so Android/iOS/Web need nothing extra: `hayro` (render) and `lopdf` (parse, edit, write); write what is missing. The old PDFium prototype is kept at `docs/archive/anypage-pdf-pdfium-prototype.rs` and `third_party/pdfium/win-x64` has a Windows DLL; PDFium may be an optional desktop feature but must not be required.

- `pdf.open(bytes) -> doc` (`pdf.document`), job `pdf.load` for large files; `pdf.new{width, height, pages}`.
- `doc:page_count()`, `doc:page_size(i)` (points, 1-based pages), `doc:metadata()`, `doc:outline()`.
- `doc:text(i)` plain text; `doc:words(i)` `{ {text, x, y, w, h} }` in page points (origin top-left); `doc:search(query, {case})` `{ {page, text, rects} }`; `doc:extract(i)` → `{ blocks = { {x, y, w, h, text} }, tables = { <table> } }` (lines grouped into blocks; tables detected from aligned columns): the "computable form" agents use.
- tiles `pdf.page`: params `{ doc, page, rect = {x, y, w, h} world rect of the page }` (the tiled layer rect is the page's world rect). Renders vector content at any zoom.
- job `pdf.render`: inputs `{ doc, page, width_px }` → image.
- Editing (Effect; returns `doc:next_version()` semantics): `pdf.add_text(doc, page, {x, y, text, size, color})`, `pdf.add_path(doc, page, path, {stroke, fill, width})` (the shared vector path form: this is how CAD drawings go onto PDF pages), `pdf.add_image(doc, page, image, rect)`, `pdf.highlight(doc, page, rects, color)`, `pdf.add_note(doc, page, {x, y, text})`, `pdf.form_fields(doc)`, `pdf.fill_form(doc, {name = value})`, `pdf.delete_page`, `pdf.insert_page`, `pdf.rotate_page`, `pdf.merge(a, b)`, `doc:save()` → bytes.

Test PDFs are generated in tests (with lopdf) and must cover text, vector paths, images, multiple pages, rotation; check rendering numerically and that saved files re-open with the edits.

### sheet — `crates/anypage-sheet` (module `sheet`)

IronCalc (`ironcalc`, `ironcalc_base`) as the engine: formulas, functions, recalculation, xlsx in and out. `calamine` / `rust_xlsxwriter` only if IronCalc lacks something.

- `sheet.open(bytes) -> wb` (`sheet.workbook`, xlsx), `sheet.new()`, `sheet.from_csv(text)`, job `sheet.load`.
- `wb:sheets()`, `wb:dims(sheet)`, `wb:get("Sheet1!B2")` (computed value), `wb:formula(ref)`, `wb:range("A1:D20")` → table, `wb:table(sheet)` → table.
- Effect: `sheet.set(wb, ref, value_or_formula) -> wb'` (strings starting with `=` are formulas), `sheet.set_range(wb, ref, rows)`, `sheet.add_sheet`, `sheet.delete_rows`, `sheet.insert_rows`. Recalculate after edits.
- `wb:save()` → xlsx bytes, `wb:to_csv(sheet)`.
Tests: formulas across sheets, SUM/VLOOKUP/IF/dates, round trip xlsx, 100 000-cell workbook timings.

### cad — `crates/anypage-cad` (module `cad`)

`dxf` crate for DXF (2D drafting, also 3D faces), own renderer; `truck-stepio` + `truck-meshalgo` (or similar) for STEP to a mesh when feasible.

- `cad.open(bytes, "dxf") -> drawing` (`cad.drawing`), `dwg:layers()`, `dwg:bounds()`, `dwg:entities({layer, type})` → `{ {id, type, layer, color, ...geometry} }`, `dwg:measure(ids)` → `{length, area}`.
- tiles `cad.drawing`: params `{ drawing, rect = world rect, layers = {visible...}, background }`: crisp lines at every zoom.
- `dwg:to_paths({fit = rect, flip_y = true})` → list of `{ path = <vector path>, stroke, width }` in the shared form (for `pdf.add_path` and for panels).
- Effect: `cad.add(dwg, entity)`, `cad.move(dwg, ids, dx, dy)`, `cad.delete(dwg, ids)`, `cad.set_layer(dwg, ids, layer)`; `dwg:save()` → DXF bytes.
- `cad.step_to_mesh(bytes)` → mesh form (job `cad.step_mesh` for large files), if a pure-Rust STEP path exists; otherwise document it in CORE_REQUESTS.md.
Tests with generated drawings: lines, arcs, circles, polylines with bulges, text, blocks/inserts, layers; tile PNGs; round trip.

### cloud — `crates/anypage-cloud` (modules `cloud`, plus whatever helpers)

`ureq` 3 with rustls + ring (no aws-lc), webpki roots. Every request: `grants.check_url`, HTTPS required except `localhost`/`127.0.0.1`, timeouts, response size limit, redirects re-checked against the capability list.

- source `cloud.chat`: OpenAI-compatible `/chat/completions` streaming (SSE). Request cell value: `{ base_url, model, messages, tools, temperature, max_tokens, secret = "openai" }`; a new request cancels the one in flight; `nil` cancels. Items (use `latest = true`): `{ text, tool_calls = { {id, name, arguments} }, done, finish_reason, usage, error }` accumulated. Works with OpenAI, Ollama, vLLM, LM Studio, Azure-style base URLs.
- job `cloud.http`: `{ method, url, headers, body, secret, auth = "bearer" }` → `{ status, headers, body = Bytes, text, json }`.
- job `cloud.s3`: `{ op = "get"|"put"|"list"|"delete"|"head", bucket, key, prefix, body, region, endpoint, secret = "aws" }` with SigV4 (secret value is JSON `{access_key_id, secret_access_key, session_token}`); function `cloud.s3_presign{...}` → URL.
- job `cloud.gcs`: `{ op, bucket, object, prefix, body, secret = "gcp" }` (secret is a service-account JSON → RS256 JWT → OAuth token, cached until expiry; or a raw access token).
- Tests: the published AWS SigV4 test vectors, SSE parsing edge cases (multi-line data, keep-alives, `[DONE]`, tool-call deltas), a local mock server for chat streaming, S3 and GCS flows, JWT signing verified with the public key, capability refusals (host, redirect, scheme).

### media — `crates/anypage-media` (module `media`)

- source `media.hls`: request `{ url }` (or `nil` to stop) → items (latest) `{ frame = image, pts, width, height, fps, buffered, dropped, error }`. HLS live and VOD playlists (`m3u8-rs`), MPEG-TS demux (own or `mpeg2ts`), H.264 decode with `openh264` (source-built), frame pacing by PTS, YUV→RGBA fast path. Audio: AAC-LC via `symphonia` to `cpal` output when available (feature `audio`), A/V sync to the audio clock when audio plays.
- source `media.broadcast`: request `{ source = "test" | "camera", width, height, fps, bitrate, segment_seconds, serve = port }` → encodes H.264 (openh264), muxes MPEG-TS segments, keeps a live HLS playlist, serves it on `http://<lan ip>:port/live.m3u8` (local network only), items `{ url, segments, bitrate, error }`. Camera capture on Windows (Media Foundation, e.g. `nokhwa`) behind a feature; Android camera is the host's job (document the bridge you need).
- Tests: synthetic frames → encode → TS → playlist → served on 127.0.0.1 → played back by `media.hls` → decoded frames match the pattern (PSNR or exact colour blocks); demux unit tests; timings (decode ms per 1080p frame).

### devices — `crates/anypage-devices` (modules `ble`, `usb`, `serial`)

Windows backends now; Android backends behind a bridge trait the Android host implements over JNI (define it here: `pub trait AndroidDevices { ... }` plus `pub fn set_android_bridge(...)`, with the data the host must pass in).

- `ble`: source `ble.scan` (request `{ on = true, services = {...} }`) → `{ devices = { {id, name, rssi, services} } }`; source `ble.link` (request `{ device, subscribe = { {service, characteristic} } }`) → `{ state, notifications = { {characteristic, data = Bytes, at} } }`; jobs `ble.read`, `ble.write`. Windows via `btleplug` (tokio runtime on a background thread).
- `usb`: function `usb.devices()` → `{ {vid, pid, manufacturer, product, serial, bus, address, id} }`; jobs `usb.control`, `usb.bulk_in`, `usb.bulk_out` (`nusb`; Windows needs WinUSB-bound devices, report clearly when not).
- `serial`: function `serial.ports()`; source `serial.port` (request `{ path, baud, ... }`) → `{ lines, bytes, error }`; job `serial.write`. `serialport` on Windows; on Android USB-serial (CDC-ACM) over the USB bridge.
- Tests: enumeration works with no devices attached, data shapes, a loopback/mock backend for the stream logic.

### scene — `crates/anypage-scene` (module `scene`)

- `scene.load(bytes, format)` → model (`scene.model`) for `glb`/`gltf`, `obj`, `stl`, `ply` (mesh or points), `xyz`, `las`/`laz` (points); job `scene.load` for large files; `scene.mesh(mesh_form)`, `scene.points(points_form)`; `model:info()` → `{kind, vertices, triangles, points, bounds}`.
- GPU layer `scene.view`: inputs `{ scene, camera }` where scene is `{ items = { {model, color, transform (16 numbers), point_size} }, background, grid = true }` and camera `{ yaw, pitch, distance, target = {x, y, z}, fov }`. Depth-tested meshes with simple lighting, point clouds as GPU points/quads. Points are shuffled at load so any prefix is a uniform sample: draw a budget-limited prefix (all points when the budget allows). Must handle 10 million points on desktop.
- Pure helpers: `scene.orbit(camera, dx, dy)`, `scene.zoom(camera, factor)`, `scene.fit(camera, bounds)`.
- Tests: loaders on generated files, headless wgpu render to a texture (`GpuRenderer::render` with a device you create) read back and checked (the cube's colour at the centre, depth order), 10M-point upload and draw timings.

### android — `apps/android` (host work, not a module)

Native text input (InputConnection: commit, composing/preedit, delete, key events; show/hide the soft keyboard from `Engine::text_focus`), clipboard (ClipboardManager) for `PageRequest::Copy/Paste`, file open/save through the Storage Access Framework for `OpenFile/SaveFile`, page packages from `assets/pages/<name>/` (choose the package via an intent extra or a launcher list), wheel/scroll from mouse and trackpad, and keep everything working across surface loss and rotation.

## 4. Core additions after the first round (2026-10-24)

The modules' requests that several of them shared are now in the core. Use them instead of the workarounds.

| Need | Now |
|---|---|
| Read any cell from a handler or action | `page.peek(cell)`: the last computed value (no evaluation). At load too. Not inside a cell's function: pass the cell as an input there. |
| React when a job finishes | `page.job(name, source, inputs, { done = function(value) ... end })`: called (as a handler) with the job's value, or `{state = "error", message}`. |
| A job nobody displays (a write, a save) | `page.job(..., { eager = true })`: runs whenever its inputs change, read or not. |
| Native time against the script budget | Time spent inside native functions (`module.fn(...)`, `page.measure`) no longer counts against `call_ms`/`handler_ms`. Native code must bound itself. |
| Panics in native code | Caught at every boundary (functions, jobs, source and tile constructors, layer converters, tile rendering, GPU layer rendering): the call fails with `"<name>: internal error"`. |
| GPU layer errors | Validation, out-of-memory and internal errors and panics in a `GpuRenderer` disable that frame of that layer only. |
| Ship sample files with a page | Files in `<package>/assets/` (png jpg pdf xlsx csv dxf step glb gltf obj stl ply las laz xyz ttf ...): `page.asset("file.pdf")` returns a byte resource (`#res` is its length). `Package::with_asset(name, bytes)` for built-in packages. |
| Open another page | `page.navigate("pdf-demo")` (a package folder name). The desktop app looks next to the current package (or `--pages <dir>`). |
| Bind a secret to hosts | `"secrets": ["plain", {"name": "openai", "hosts": ["api.openai.com"]}]`; modules call `grants.secret_for(name, url)` (or `check_secret_url`). |
| IPv6 hosts | `capabilities.net` accepts `"[::1]"` and `"[::1]:8080"`. |
| Gesture taken away | `pointer_cancel` event (pinch start, OS cancel). Pages without a handler get `pointer_up` as before. |
| Stream defaults per source | `natives.source_defaults(name, latest, cap)`. |
| Source plumbing | `SourceCtx::has_request()`, `Sink::try_push() -> Pushed::{Ok, Full, Closed}`, `Res::at_version(v)`. |
| Right-to-left text | `text.layout` lines carry `rtl`; `text.caret(s, size, pos, dir)`, `text.hit(s, size, x, dir)`, `text.selection(s, size, from, to, dir)` → `{ {x0, x1} }`, `text.is_rtl(s)`, `text.missing(s)`; `d:text(x, y, size, c, s, dir)` with `dir` `"ltr"`/`"rtl"`. |
| The text system | The engine's `TextRaster` is now `anypage_text::TextSystem` (shaping, bidi, fallback, colour emoji). Page-facing `text.*` look at most 256 KB per call. |
| Every module in an app | `anypage_std::natives()` registers the demo operators and all modules; both apps use it. |
| Window size and phone layouts | `page.viewport()` returns an input cell `{ w, h, scale, portrait }` (logical pixels); the `viewport` event fires when it changes; fit a layout with `page.fit_rect(rect)`. |
| Reading files as text | `text.decode(bytes[, max])`: UTF-8 (BOM dropped) or UTF-16 with a BOM; fails on binary. |
| Cell names | Two cells may not share a name (load error). |

Still open (not built): `page.cursor`, rich text spans, lazy views of large lists, a per-page GPU layer identity, `Effect::Blocking`.
