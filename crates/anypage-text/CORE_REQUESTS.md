# anypage-text: requests to the core

`anypage-text` is standalone and depends on neither the engine nor the
runtime. Integration is the swap below; nothing here blocks the crate itself.

## 1. Engine: replace `TextRaster` with `TextSystem` (required to use the crate)

Reason: the engine's `TextRaster` draws every character with one font and no
shaping, so Arabic is unjoined and left to right, Indic and Thai are broken,
emoji ZWJ sequences fall apart and any script missing from Microsoft JhengHei
shows as boxes.

- `crates/anypage-engine/Cargo.toml`: `anypage-text = { path = "../anypage-text" }`.
  It resolves the same `swash` 0.2 the engine already uses. (Root workspace:
  add `crates/anypage-text` to `members` and remove its `[workspace]` table.)
- Root `Cargo.lock`, after adding the crate: run
  `cargo update -p skrifa@0.44.0 --precise 0.40.0`. Reason: the root lock has
  swash 0.2.10, which accepts skrifa 0.31-0.44 and resolves 0.44, while
  cosmic-text 0.19 needs skrifa 0.40; without the pin the build carries two
  copies each of skrifa, read-fonts and font-types (several hundred KB more
  in the Android library). The crate's own lock already has the pin, and
  swash 0.2.10 builds and passes every test against skrifa 0.40.
- `crates/anypage-engine/src/lib.rs`: keep the public name so hosts and the
  runtime compile unchanged:

  ```rust
  pub use anypage_text::{Direction, LoadOptions, TextSystem};
  pub type TextRaster = anypage_text::TextSystem;
  ```

  Every method the code base calls exists with the same signature: `load`,
  `measure`, `line_height`, `line_metrics`, `caret_x`, `hit_x`,
  `layout_ranges`, `layout`, and the `source` field (used by `Engine::summary`).
- `crates/anypage-engine/src/raster.rs`: delete `TextRaster`, `GlyphBits`,
  `is_cjk`, `blit`; the two text methods of `Canvas` become:

  ```rust
  /// Single line of text with its top at `y`. Returns the advance in units.
  pub fn text(&mut self, tr: &TextRaster, x: f32, y: f32, px: f32, c: Rgba, s: &str) -> f32 {
      self.text_dir(tr, x, y, px, c, s, anypage_text::Direction::Auto)
  }

  pub fn text_dir(&mut self, tr: &TextRaster, x: f32, y: f32, px: f32, c: Rgba, s: &str, dir: anypage_text::Direction) -> f32 {
      let s_ = self.scale;
      let clip = self.clip_px();
      let (w, h) = (self.pm.width(), self.pm.height());
      tr.draw_line_dir(self.pm.data_mut(), w, h, clip, x * s_, y * s_, px * s_, c, s, dir) / s_
  }

  /// Word-wrapped text; right-to-left paragraphs align right, and wrapped
  /// lines keep their paragraph's direction.
  pub fn text_box(&mut self, tr: &TextRaster, x: f32, y: f32, w: f32, h: f32, px: f32, c: Rgba, s: &str, tail: bool) -> usize {
      let size = px * self.scale;
      let line_h = tr.line_height(size);
      let lines = tr.layout_lines(s, size, w * self.scale);
      let fit = ((h * self.scale) / line_h).floor().max(1.0) as usize;
      let start = if tail && lines.len() > fit { lines.len() - fit } else { 0 };
      for (i, l) in lines.iter().skip(start).take(fit).enumerate() {
          let ly = y + (i as f32 * line_h) / self.scale;
          let lx = if l.dir == anypage_text::Direction::Rtl { x + w - l.width / self.scale } else { x };
          self.text_dir(tr, lx, ly, px, c, &s[l.range.clone()], l.dir);
      }
      lines.len()
  }
  ```

- Hosts (`apps/desktop`, `apps/android`): `TextRaster::load()` keeps working,
  but should become `load_with` with the app's language (see section 2).
  Loading takes 50-70 ms with a warm file cache on Windows (176 faces) and
  about 650 ms on a cold first run; if that matters at startup, construct it
  on the render thread before the first frame rather than on the UI thread.

Behaviour differences callers will notice (all deliberate; see the crate docs):
`layout_ranges("a\n")` now returns two lines (`"a"`, `""`) and `""` returns
one empty line; whitespace at a soft break is never part of a line;
`caret_x` is visual (right end at offset 0 in right-to-left text); `hit_x`
past the left end of a right-to-left line is the text length (past its right
end, 0); widths are shaped widths; single-line calls look at the first 1 MiB
of their text, and `draw_line` (`Canvas::text`) shapes, draws and returns the
advance of at most the first 64 KiB.

## 2. Hosts: pass the app's language (Han glyph style)

Reason: Han characters have regional glyph forms (Traditional, Simplified,
Japanese, Korean), and with no locale given the text system follows the OS
locale. On this zh-CN Windows machine the primary font is therefore Microsoft
YaHei UI, so the engine's own Traditional Chinese UI strings draw with
Simplified-style glyphs (the old raster used Microsoft JhengHei for
everything). The hosts pass no locale today.

Proposed change, desktop (`apps/desktop/src/main.rs`) and Android
(`apps/android/src/lib.rs`), wherever `TextRaster::load()` is called:

```rust
let text = TextRaster::load_with(anypage_text::LoadOptions {
    // the app's UI language; zh-TW for AnyPage today
    locale: Some(app_language.unwrap_or_else(|| "zh-TW".into())),
    ..Default::default()
})?;
```

On Android `app_language` comes from `Locale.getDefault().toLanguageTag()`
(which honours per-app languages; `sys-locale` reads the system property
`persist.sys.locale`). Later, when a page declares its language in
`page.json` (for example `"lang": "ja"`), the engine could load a second
`TextSystem` for that page, or the crate could gain a per-call language
argument; neither is needed yet.

## 3. Runtime `text` module: expose direction, selection and coverage

Reason: a text field or text area needs these for bidi text; without them a
page cannot right-align an Arabic paragraph or draw a correct selection.
Proposed additions to `crates/anypage-runtime/src/text.rs` (all `Effect::Pure`):

```lua
text.layout(s, size, width)      -- each line also gets rtl = true|false (its paragraph's direction)
text.caret(s, size, pos, dir?)   -- dir: "ltr" | "rtl" | nil (auto)
text.hit(s, size, x, dir?)
text.selection(s, size, from, to, dir?) -> { {x0, x1}, ... }   -- TextSystem::selection_rects_dir
text.is_rtl(s) -> bool                                            -- TextSystem::is_rtl
text.missing(s) -> number                                         -- TextSystem::missing_glyphs
```

and in the display list (`crates/anypage-runtime/src/draw.rs`), an optional
direction for `"text"`: `{ "text", x, y, size, color, s, dir }` with `dir`
`"ltr"`/`"rtl"`, mapped to `Canvas::text_dir`. Rust side:

```rust
fn dir(d: Option<&Data>) -> anypage_text::Direction {
    match d.and_then(Data::as_str) { Some("rtl") => Direction::Rtl, Some("ltr") => Direction::Ltr, _ => Direction::Auto }
}
```

## 4. Later (not needed now)

- Rich text (bold/italic/size/colour spans) needs an attributes parameter on
  shaping; cosmic-text supports spans (`AttrsList::add_span`), so the API
  would be `draw_runs(&[(Range, Style)])`. Not requested by any module yet.
- Dictionary line breaking for Thai/Lao/Khmer/Burmese would need ICU4X's
  segmenter data (about 1-2 MB); today long runs in those scripts break at
  grapheme boundaries (tracked follow-up).
- At integration (after the engine aliases `TextRaster` to `TextSystem`): a
  headless page test in `crates/anypage-runtime/tests/` that calls
  `text.width`, `text.layout`, `text.caret`, `text.hit` (including
  `math.huge`) and `d:text` with Arabic, CJK and emoji, and checks the panel
  pixels. Today the runtime still calls the engine's own `TextRaster`, so a
  page test cannot exercise this crate yet.
- The page-facing `text.*` functions could cap their input lower than the
  crate does (for example 256 KiB): a cold call on a 1 MiB line shapes it
  all (1-4 s on the engine thread, outside the Luau time limit); warm calls
  are milliseconds.
