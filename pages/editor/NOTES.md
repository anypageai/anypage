# pages/editor: notes for the core, the runtime and the UI library

The multilingual editor (`main.luau`, `editor.luau`, `samples.luau`) works
around the gaps below. Each entry says what, where, why it matters, and a
proposed fix. Tests: `crates/anypage-std/tests/page_editor.rs`
(`cargo test -p anypage-std --test page_editor`, 21 tests).

## 1. Opening files: works through text.decode; the encoding is guessed

**Done.** Open… and drag-and-drop read `{ name, size, data }` with
`pcall(text.decode, data, 2 * 256 KB + 2)`. UTF-8 with or without a BOM and
UTF-16 with a BOM open; text.decode's failures (invalid UTF-8 or UTF-16,
NUL, odd UTF-16 length) and the page's own checks (control characters,
binary extensions, size) refuse the file with a notice, and the document is
left as it was. CRLF, LF and lone CR line ends are kept for saving (mixed
ones: the most frequent, with a note in the toast). UTF-16 files are saved
as UTF-8 with a BOM (`page.save_file` takes a string) and the status bar
says so ("UTF-16→UTF-8 BOM").

**Gap.** text.decode drops the byte order mark and does not say which
encoding it read. The page infers it from sizes: `size == #text` is UTF-8,
`#text + 3` UTF-8 with a BOM, anything else UTF-16. A few tiny UTF-16 files
match one of the first two by chance (for example "ab中"); they are then
saved as UTF-8 with or without a BOM instead of "UTF-16→UTF-8 BOM".

**Proposed fix.** `text.decode` returns a second value, the encoding read:
`"utf-8" | "utf-8-bom" | "utf-16le" | "utf-16be"`. (Writing UTF-16 back would
also need `text.encode(s, "utf-16le")` returning bytes for
`page.save_file`.)

## 2. crates/anypage-std dev-dependencies: done

The page test now builds and runs in place.

## 3. UI library: handlers rebuild the view with the inputs of the last draw

**What.** `App:_tree()` (`crates/anypage-ui/src/luau/app.luau`) rebuilds a
stale tree in a handler with `self._args`, the extra input values the panel
last drew with, and only when the app's own state or UI cell changed
(`_rev`). Two consequences here:

- The caret cell (an input of the wide app) can point into the middle of a
  UTF-8 character of newer text; the view snaps input-derived offsets to
  character boundaries.
- The narrow app shows the document state of the wide app (`editor.state`)
  as an input. A handler that opens the find bar and then focuses its field
  would look in a tree built from the old state (no field, the focus is
  lost). The page works around it: `narrow_view` reads its inputs with
  `page.get` (current values; the cells are still declared as inputs so the
  panel redraws), and every change to the document state while the narrow
  layout is shown calls `narrow:ui_set(narrow:ui_get())` to bump the narrow
  app's revision.

**Proposed fix.** In `_tree()`, compare the extra inputs' current values
(`page.peek`, allowed in handlers) with `self._args` and rebuild when they
differ, building with the current values. Then a page could show one state
in two apps with no workaround.

Related: `text.*` report invalid UTF-8 as "argument 1 must be a string"
(`arg_str` in runtime text.rs). "argument 1 is not valid UTF-8" would point
at the cause at once.

## 4. UI library: custom element types are possible but not public

The editor is an element type of its own (`mleditor`), added to
`import("ui/layout").types`, relying on internals (`types`, `L.push_clip`,
`U.*`, `app:host_caret`, `app:ui_get/ui_set`, `app:copy`, `app:paste_into`).
Proposal: `ui.register_type(name, spec)` with the spec fields documented
(measure, arrange, draw, down/drag/up, wheel, key, text, paste, ime,
on_focus, activate, describe, focusable, text_input, own_focus).

The agent op `set` (`App:operate`) only knows the library's own types, so
`editor.do {id = "doc", op = "set"}` fails with "has no value to set". The
editor's `describe` gives a hint to use the `set_text` action instead; op
`click` works (the type's `activate` puts the caret at the end), as do
`type`, `key` and `focus`. Proposal: a `set(el, app, value)` hook in the
type spec that `operate` calls for other types.

## 5. Caret blink re-renders the whole app

With `blink` on, the app's panel is re-run and re-rasterised every 0.53 s
while a text field has focus (about 10 ms of CPU per blink on a 2400 px wide
window). The page now turns blinking off on Android and iOS (`blink = not
MOBILE`), where it costs battery; desktop keeps it. Proposal: a caret
overlay (a second small panel or an engine caret primitive) so blinking
costs a few hundred pixels.

## 6. Keys

- **Escape** is taken by the page (registered before `ui.app`): it cancels
  the confirmation dialog, else closes the find bar, else keeps or restores
  the focus on the text; with a drop-down open it is left to the library
  (which closes the menu). It never leaves the page with no focus. The
  desktop app no longer quits on Escape either.
- **Tab** with the text focused inserts a tab character (the page takes it
  before the library, which would move the focus); **Shift+Tab** leaves the
  text. Tab from another control moves into the text without inserting.
- **R** outside text fields fits the current layout (the engine's own `r`
  would fit the page bounds, which are the wide layout).
- UI library: `App:_text_event` does not check for a modal dialog, so text
  typed while a dialog is open reaches the focused element behind it. The
  page moves the focus to the dialog's 取消 button when it opens one; a
  `confirm` written into the state by an agent would not do that.

## 7. Layouts and phones

Two `ui.app`s: wide (`0, 0, 1440, 900`, the page bounds) and narrow
(`2400, 0, 400, 820`: two toolbar rows, a stacked find bar, a compact
status bar). The `viewport` event picks narrow when it would show the text
at least 1.5 times as large as the wide one (phones in portrait, desktop
windows narrower than about 900 logical pixels) and fits it with
`page.fit_rect`; the caret, selection, focus and undo history go along.
The `set_layout` action forces one (`"auto"` hands it back to the window).
Verified headlessly and in the desktop app by resizing the window to a
phone shape (540 x 1150 px) and back.

Not verified: the Android emulator, the soft keyboard (the Android host
pans the caret above the keyboard without resizing the window, so the
layout should not change when it opens), and phones in landscape, which get
the wide layout at about 0.4 scale (18-unit text near 7 dp; pinch zoom or
A+ helps). A third, short layout for landscape phones would fix that.

## 8. Smaller observations

- Windows has no colour flag emoji: "🇹🇼" is drawn as the letters "TW"
  (Segoe UI Emoji). Not an AnyPage bug; Android's Noto Color Emoji has flags.
- On this Windows 11 machine Kawi and Nag Mundari have fonts, so in the
  "缺字示範" sample only the unassigned U+0378 counts as missing.
- One paragraph of 128-256 KB is wrapped again on every keystroke (about
  17-18 ms in the handler plus 10-11 ms per frame at 256 KB, headless):
  inside the 250 ms budget but slower than a 60 Hz frame. Re-wrapping only
  the visual lines around the edit would fix it; not done. Ordinary
  multi-paragraph documents re-wrap only the edited paragraph.
- Save names are cleaned in the page: the base name only, no control
  characters, direction overrides or characters Windows forbids, `.txt` or
  `.md` (anything else gets `.txt` added), no device names (CON, LPT1...),
  `未命名.txt` when nothing is left. A document longer than 256 KB that was
  set from outside is shown read-only and is not saved (that would cut it).
