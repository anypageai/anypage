# pages/home: notes

The launcher. A card for every page package (`catalog.luau`), the modules
each uses and whether this platform has them, an "about AnyPage" box, the
platform, keyboard help. Tap / click / Enter on a card calls
`page.navigate(name)`. Agent actions: `open_page {name}`, `list_pages`,
`set_layout {layout}`, plus `wide.*` / `narrow.*` from the UI library.

Run: `target\release\anypage.exe --page pages/home` (navigation finds the
other packages next to it). Test: `crates/anypage-std/tests/page_home.rs`.

## Layouts

A page cannot see the window, so there are two layouts, two `ui.app`s:

- `wide` at (0, 0, 1440, 900) = `page.json` bounds: 3-column grid + side column.
- `narrow` at (2400, 0, 400, 820): one scrolling column, sized for phones in
  portrait (16:9 to 20:9 after the engine's 24 px margins). It sits far
  right of `wide`, so fitting either never shows the other, and the engine
  never evaluates the panel that is out of view.

Android and iOS start `narrow` (`page.fit_rect` at load); other platforms
start `wide`. The header button (and `set_layout`, and `R`) switches/fits.

## Requests for the core, hosts and the UI library

1. **anypage-std has no dev-dependency on anypage-engine.** The test needs
   `Page`, `PageRequest` and `TextRaster`, which only `anypage-engine`
   exports. Add to `crates/anypage-std/Cargo.toml`:
   `[dev-dependencies] anypage-engine = { workspace = true }`
   (page_studio.rs needs the same). Until then `cargo test -p anypage-std`
   does not compile this file; I ran it from a scratch crate that depends on
   anypage-std + anypage-engine by path and points `[[test]]` at this file,
   with `ANYPAGE_ROOT=D:/anypage` (the test reads `pages/` from there).

2. **Clipping doubles panel raster time** (`anypage-engine/src/raster.rs`,
   `Canvas::push_clip` / `pop_clip`). Every push and pop allocates and fills
   a full-canvas `Mask` (2.6 MB at 2048 x 1280), and every fill after it goes
   through the masked pipeline. Measured on this page (release, warm glyph
   cache): wide layout at 2x, 17.1 ms with its two scroll clips vs 8.2 ms with
   the clip commands removed; narrow at 2x, 7.5 ms vs 3.3 ms. Proposed: keep
   clips as rectangles; draw shapes whose bounds lie inside the clip with no
   mask, skip those entirely outside, and build a mask (cached per clip
   rect) only for shapes crossing an edge. Workaround here: the wide layout
   uses no scroll container while five rows of cards fit (`WIDE_SCROLLS`).

3. **No viewport information for pages.** A page cannot learn the window's
   size or aspect, so the layout is chosen by `page.info.platform` (a
   landscape tablet starts narrow; a portrait desktop window starts wide).
   Proposed: `page.viewport()` / a `viewport` cell `{w, h, scale}` in logical
   pixels (or a `resize` event) so pages can pick a layout. Related: the
   engine fits the bounds only at start and on `R`, not after a resize or a
   rotation.

4. **No list of installed packages.** `catalog.luau` is static. Proposed:
   `page.packages()` -> `{ {name, id, title, modules, optional_modules} }`
   from the host's pages folder (desktop `--pages`, Android `assets/pages`).
   Meanwhile the test `catalog_matches_the_pages_folder` fails when a folder
   in `pages/` is missing from the catalog, a listed one has no `page.json`,
   or the modules differ from what its `page.json` declares.

5. **Android** (apps/android, not mine): at the end of this round the host
   syncs the repo's `pages/` into the APK (`build.gradle.kts`, `syncPages`),
   starts with `home` when it is there (`PagePackages.START`) and passes
   `page.navigate` to Kotlin (`host.rs`, `onNavigate`); earlier in the round
   `requests.rs` still answered Navigate with a "not supported on Android
   yet" notice. This page is **not verified on a device or emulator**: the
   narrow layout was checked in a 900 x 1600 desktop window and in a test
   that loads the package as if on Android.

6. **No way back to the launcher.** Desktop: Escape that no page takes quits
   the app. Proposed for both hosts: keep a page stack; Android Back /
   desktop Alt+Left returns to the previous page (or `home`).

7. **UI library: Tab with nothing focused goes to the first app created**
   (`app.luau`, the page `key` handler: `apps[1]`). Here that is `wide` even
   while `narrow` is shown; this page's own key handler (registered before
   the apps, so it runs first) sends Tab to the shown layout. Proposed: a
   `primary` flag on `ui.app`, or the app under the last pointer.

8. **UI library: no clickable container.** A card that is one control
   (hover, press, focus ring, Enter, tap; a moving press scrolls) is a custom
   element type registered in `import("ui/layout").types` (`launch_card` in
   main.luau, using the widget protocol: `focusable`, `press`, `activate`,
   `describe`). It depends on that internal protocol. Proposed: a public
   `ui.pressable(opts, children)` / `ui.card{on_click}`.

9. Action errors count as handler errors (`ScriptPage::handler_errors`):
   `open_page` with an unknown name raises (so the port answers
   `{"error": ...}`) and increments the counter. Fine, just noting it.

## Catalog of the new pages

`studio`, `editor`, `pdf-agent` were written in parallel. Their entries
follow their `page.json` as of 2026-10-24 22:50 (studio: cad, sheet, pdf +
optional scene; editor: none; pdf-agent: pdf + optional sheet, cloud); the
catalog test will flag any later change.

## Timings (this machine, release; RTX 3090 desktop, 1600 x 1000 window at 150 %)

- Load 16-38 ms (limit 2000), script memory about 2.6 MB (limit 32).
- View + layout after a focus or hover change: wide 2.2 ms, narrow 1.6 ms.
- Raster of the panel after a change, at the camera's 2x level: wide
  (2048 x 1280) about 8 ms, narrow (800 x 1640) about 7.5 ms (half of it the
  scroll clip, item 2). Before cutting overdraw (full-panel background, card
  tint and full-size shadow fills, scroll clips) the wide raster was 22 ms.
- Engine HUD in the real app, three arrow keys: frame CPU p50 18.5 ms, p95
  26.5 ms (it was 28 / 51 ms before those changes); in a portrait window
  with the narrow layout p50 11.5 ms.
