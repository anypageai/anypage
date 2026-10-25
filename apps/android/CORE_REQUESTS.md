# Core requests from the Android host

The Android host (apps/android) works with the engine and runtime as they are. These
changes to `anypage-engine` / `anypage-runtime` would make native text input on
Android (and IMEs in general) behave like a platform text field. Each lists what
the host does meanwhile.

## 1. Surrounding text for input methods (most important)

**Why.** Android IMEs (Gboard, Samsung, Sogou, ...) read the editor's text around
the caret (`getTextBeforeCursor`, `getSurroundingText`, `getExtractedText`) and
re-open words for composition (`setComposingRegion`, `replaceText`). They need it
for autocorrect, suggestions after moving the caret, auto-capitalisation,
word-wise delete and some CJK/Indic flows. The engine has no way to ask the page
for its text.

**Meanwhile.** The host's input connection keeps a shadow: the text the IME
typed since the caret last moved, which is the end of the page's text before
the caret. Corrections of what was just typed work (verified with Gboard on the
emulator: picking a correction arrives as `replaceText`, applied as backspaces
plus a commit), `deleteSurroundingText` inside the shadow deletes exactly the
graphemes it covers. Limits: text that was in the field before it got focus,
or before a tap/arrow key moved the caret, is invisible to the IME (it sees an
empty field there; a delete beyond the shadow becomes one backspace per UTF-16
unit); text after the caret is always invisible; a page that changes its own
text (an agent action, a button) desynchronises the shadow until the next
restart.

Checked against a platform `EditText` on the same emulator (debug-only
`ImeProbeActivity`, which logs every input-connection call): Gboard sends
English soft-key typing as one `commitText` per letter to the `EditText` too,
so that is Gboard's behaviour there (a hardware keyboard is attached), not
something this editor lacks. Gboard Zhuyin (中文 繁體) works in the engine's
field in both of Gboard's modes: with "Inline composing" on, `setComposingText`
arrives per Zhuyin symbol, the page draws the underlined preedit, a candidate
pick arrives as `commitText`, and backspace inside the composition shortens
it; with it off (Gboard's default), the composition stays in Gboard's own
window and the character arrives as `commitText`. Next-word predictions
appear after a commit (Gboard reads the shadow), and picking one commits it.
In both editors Gboard swallows the first backspace after a prediction pick.

**Proposed.**

```rust
// anypage-engine, page.rs
pub struct TextContext {
    /// The field's text, or a window of it around the caret (hosts ask for
    /// at most a few thousand characters).
    pub text: String,
    /// Byte offset of `text` within the whole field.
    pub offset: usize,
    /// Selection as byte offsets into `text` (start == end: a caret).
    pub selection: std::ops::Range<usize>,
    /// The composition being shown, if any.
    pub composing: Option<std::ops::Range<usize>>,
}
pub trait Page {
    fn text_context(&self) -> Option<TextContext> { None }
    /// Replace `range` (byte offsets in the field) with `text` and put the caret after it.
    fn replace_text(&mut self, _range: std::ops::Range<usize>, _text: &str) {}
    fn set_selection(&mut self, _range: std::ops::Range<usize>) {}
}
impl<P: Page> Engine<P> {
    pub fn text_context(&self) -> Option<TextContext>;
    pub fn replace_text(&mut self, range: std::ops::Range<usize>, text: &str);
    pub fn set_selection(&mut self, range: std::ops::Range<usize>);
}
```

Script side: `page.focus_text(rect, { text = s, caret = c, anchor = a })` (the
extra table optional), and events `replace(from, to, text)` and
`select(from, to)`. The UI library's textfield can fill these in.

The host then implements `getTextBeforeCursor`/`AfterCursor`/`getSelectedText`/
`getExtractedText`/`setComposingRegion`/`setSelection`/`deleteSurroundingText`
exactly and calls `InputMethodManager.updateSelection` when the caret moves.

## 2. What kind of text a field takes

**Why.** The soft keyboard should match the field: digits for a number, `@` for
e-mail, no suggestions and no learning for passwords, a "Send"/"Search"/"Done"
key instead of Enter, multi-line or not. Today every field gets
`TYPE_CLASS_TEXT` with `IME_ACTION_NONE`.

**Proposed.** `page.focus_text(rect, { kind = "text" | "multiline" | "number" |
"decimal" | "phone" | "email" | "url" | "password", action = "none" | "go" |
"search" | "send" | "next" | "done", suggestions = true })`, surfaced as

```rust
pub struct TextFieldHints { pub kind: String, pub action: String, pub suggestions: bool }
pub trait Page { fn text_hints(&self) -> Option<TextFieldHints> { None } }
```

The host maps them to `EditorInfo.inputType` / `imeOptions`; an editor action
arrives as the key `"enter"` (or a new `"action"` event with its name).

## 3. Keeping a rectangle visible above the keyboard

**Why.** When the keyboard covers the caret the host pans the camera itself
(`Engine::camera_mut().pan`), using `text_focus()` and the IME inset. That is
host logic every mobile host (iOS too) will repeat.

**Proposed.** `Engine::set_obscured_insets(top, right, bottom, left)` (pixels the
system covers: keyboard, notches, navigation bar), used by `fit` and by an
`Engine::reveal(rect)` that the engine calls when the text focus moves.

## 4. Reading opened and dropped files from scripts

**Why.** `page.open_file` and `file_drop` hand the page `{ name, size, data }`
where `data` is an `fs.bytes` resource. A page without a native module cannot
read it (no bytes-to-string or buffer conversion), so the text-input-test page
can only show the name and size of a file it opened.

**Proposed.** Built-in `text.decode(bytes [, "utf-8"])` -> string (error on
invalid UTF-8 unless a replacement flag is passed) and `bytes.buffer(res)` -> a
Luau buffer copy, with a size limit.

## 5. Pasting more than text

**Why.** `PageRequest::Paste` answers with text only. Android clipboards and IMEs
(`commitContent`: stickers, GIFs, images) carry URIs with MIME types.

**Proposed.** `Page::paste_data(&mut self, mime: &str, name: &str, bytes: Arc<Vec<u8>>)`
(script event `paste` with a file table like `file_drop`), and an accepted
MIME list in the hints of section 2 so the host can set
`EditorInfo.contentMimeTypes`.

## 6. Secrets on Android (done in the host)

Pages load with `LoadOptions { secrets }` backed by `SecretVault.kt`
(app-private preferences, AES-256-GCM under an Android Keystore key, the
entry name as authenticated data) and entered on `SecretsActivity`
(launcher shortcut "金鑰 Secrets"). No core change was needed. See NOTES.md
for what a core hook would add (asking for a missing secret at use).

## 7. Whether the page took a tap

**Why.** A soft keyboard the user dismissed (Back) should come back when the
user taps the field again, and only then. `Engine::pointer_down` returns
nothing, so the host cannot tell a tap the page handled from one on empty
page.

**Meanwhile.** The host tracks gestures itself (`view::Taps`: one finger,
within 8 dp) and brings the keyboard back after a tap that moved the caret or
landed on the caret's line (one caret height above to two below). A tap on
another control on the same line also brings it back.

**Proposed.** `Engine::pointer_down(..) -> PointerResponse` (what the page
answered; `Ignored` during a pinch), or `Engine::last_tap() -> Option<PointerResponse>`
set on the pointer-up that ends a tap.

## 8. Rectangles that overflow `f32`

**Why.** `rect_of` (anypage-runtime api.rs) checks that the four numbers are
finite as `f64`, then casts to `f32`. `page.focus_text({10, 1e39, 2, 30})`
passes and becomes `y = inf`; `Engine::text_focus()` then returns
`(76, inf, 2, 1)` (the height became NaN and `max(1.0)` hid it). Before this
host checked, panning the camera by `-inf` made the page vanish.

**Meanwhile.** The host never moves the camera for a caret that is not
finite or lies more than a page's size outside the page bounds, and tells
the IME `(0, 0, 1, 1)` for a caret it cannot place.

**Proposed.** In `rect_of`: reject any value with `|v| > f32::MAX` (or `> 1e9`,
a sane world-coordinate bound) with the same error message; in
`Engine::text_focus`: return `None` when the screen rectangle is not finite.

## 9. Telling the page that a copy failed

**Why.** `PageRequest::Copy` has no answer. The Android host refuses more than
256 KB (binder transactions above about 1 MB used to crash the app) and the
system clipboard can fail; the page cannot tell its user.

**Meanwhile.** The host shows a toast and logs the refusal.

**Proposed.** `PageRequest::Copy { id, text }` answered with
`Page::host_result(id, HostResult::Saved/Failed)`, or a `copy_failed(message)`
event; and a documented clipboard size every host accepts (256 KB).

## 10. Retrying a panel draw that ran out of time

**Why.** `Realm::call` measures wall-clock time, including native work such
as the first glyph rasterisation. On a loaded device (an emulator while the
host machine builds) a trivial first draw of the text-input-test page's file
panel took longer than the page's 50 ms `call_ms` and showed "script error:
time budget exceeded". The panel's input never changes, so it was never
drawn again.

**Meanwhile.** The test page sets `call_ms` 250 and `handler_ms` 200 (its
draws and handlers normally take well under 1 ms; the limits only stop
runaway scripts).

**Proposed.** When a panel draw fails with "time budget exceeded", redraw it
after a short backoff (at least once, or a few times with growing delay)
before showing the error panel; or count only Luau execution time against
the budget, not time spent in native calls or waiting for a lock.
