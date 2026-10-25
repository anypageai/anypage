//! UI component library for page scripts: layout, buttons, lists, text fields,
//! written in Luau and loaded by pages with `import("ui")`.
//!
//! The library is ordinary page code (sandboxed, quota-bound, no extra
//! powers): it only uses `page.*`, the built-in `text` and `json` modules and
//! the prelude's `color` and `Draw`. Nothing here is native, so it runs
//! wherever the runtime runs (Windows, Android, and later iOS and the web).
//!
//! # Model
//!
//! * **State in cells.** `ui.app{...}` creates two input cells: `<name>.state`
//!   (the page's data, readable and writable by agents when `agent = true`)
//!   and `<name>.ui` (focus, hover, pressed element, caret and selection,
//!   scroll offsets, the open menu, toasts).
//! * **Views are pure.** The app's panel draws `view(state, inputs...)`: the
//!   element tree is built, laid out and recorded as a display list. It runs
//!   only when the state cell, the UI cell or one of the app's inputs
//!   changes, or at a time it declared (caret blink, tooltip delay, toast
//!   expiry, an indeterminate progress bar).
//! * **Handlers are commands.** Event handlers call `app:set` /
//!   `app:update` (and the library updates the UI cell); the graph does the rest.
//! * **A failing view is contained.** If the view (or a page callback run
//!   while drawing, or a prop of the wrong type) raises an error, the app
//!   shows an error card with the message, logs it once with `page.log`, and
//!   runs again only when its state, UI state or inputs change. Events do
//!   nothing meanwhile. Prop errors name the element and the prop
//!   (`ui.select "fruit": options must be a list ..., got string "abc"`).
//!
//! # Luau API
//!
//! | function | what |
//! |---|---|
//! | `ui.app{rect, name, state, inputs, theme, view, keys, agent, blink, background, max_elements}` | an app; returns the app object |
//! | `app:get()`, `app:set(k, v)`, `app:update(fn)`, `app:replace(s)` | read / change the state cell |
//! | `app:toast(text, {tone, duration})` | a transient message at the bottom of the app |
//! | `app:focus(id)`, `app:blur()`, `app:focused_id()`, `app:scroll_to(id, y)` | focus and scrolling |
//! | `app:describe{from, limit, within, types, interactive_only}` | the UI as nodes with ids, types, values and world rectangles; paged with `next` |
//! | `app:items{id, from, limit}` | options of a select / radio / tab bar, items of a list, rows of a table, paged |
//! | `app:operate{id, op, value}` | operate an element as a user would: `click`, `focus`, `type`, `set`, `key` |
//! | `app:now()` | current time on the page clock's scale (see Time below) |
//! | `app:stats()` | `{frames, view_ms, layout_ms, draw_ms, total_ms, elements, commands, history_bytes}` |
//! | `ui.themes.dark`, `ui.themes.light`, `ui.theme(overrides, base)` | themes |
//! | `ui.limits` | `{field, area, undo_steps, undo_bytes}`: text size limits in bytes |
//! | `ui.column`, `ui.row`, `ui.stack`, `ui.grid`, `ui.card` `(opts, children)` | containers: `gap`, `padding`, `align`, `justify`, `bg`, `radius`, `border` |
//! | `ui.spacer([n])`, `ui.divider()`, `ui.scroll(opts, child)` | free space, lines, scrolling (wheel, drag, scrollbar) |
//! | `ui.text(s, opts)`, `ui.field(label, control)`, `ui.badge(s)`, `ui.progress(v)`, `ui.image(img)`, `ui.canvas{draw}` | content |
//! | `ui.button(label, opts)`, `ui.checkbox(label, opts)`, `ui.switch(label, opts)`, `ui.radio{options}`, `ui.slider{}` | controls |
//! | `ui.textfield{}`, `ui.textarea{}` | text editing: grapheme carets, words, selection, clipboard, IME preedit, undo |
//! | `ui.select{options}`, `ui.tabs{tabs, content}` | choices |
//! | `ui.list{items or count + item, row, key}` | a virtualised, keyed list with selection and keyboard navigation |
//! | `ui.table{data = {columns, rows}}` or `ui.table{columns, count, row(i)}` | a virtualised table in the shared table format (or rows on demand), sortable header |
//! | `ui.dialog{open, title, body, actions, on_close}` | a modal overlay inside the app |
//! | `tooltip = "..."` on any element | shown after hovering for `theme.tooltip_delay` |
//! | `ui.when(cond, el)` | conditional children |
//!
//! Sizing on any element: `width`, `height`, `min_w`, `max_w`, `min_h`,
//! `max_h`, `grow`, `fill`, `align_self`, `shrink`, plus `id` / `key` for
//! stable identity and `disabled`.
//!
//! Callbacks receive the app as their last argument (`on_click(app)`,
//! `on_change(value, app)`, `on_select(item, i, app)`).
//!
//! Keyboard: Tab / Shift+Tab move focus (trapped inside an open dialog),
//! Enter / Space activate, arrows / Home / End / PageUp / PageDown navigate
//! lists, tables, sliders, radios, tabs and selects, Escape closes menus and
//! dialogs. Text fields: Left/Right (Ctrl: by word), Shift to select,
//! Home/End, Up/Down/PageUp/PageDown in text areas, Backspace/Delete (Ctrl:
//! by word), Ctrl+A/C/X/V, Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y, Enter submits a
//! field (`on_submit`) or breaks a line in an area. While a field has focus
//! the app keeps `page.focus_text(caret)` current so hosts show the keyboard
//! and place the IME window.
//!
//! # Agents
//!
//! With `agent = true` an app registers three actions on the semantic port:
//!
//! * `<name>.describe` with params `{from, limit, within, types,
//!   interactive_only}`: at most 40 nodes per call (the port cuts longer
//!   lists), with `total` and `next` (the `from` of the following page).
//!   List-valued fields (`options`, `tabs`, `columns`) are JSON text, since
//!   the port turns lists three levels deep into counts.
//! * `<name>.items` with `{id, from, limit}`: the entries of a select, radio
//!   group, tab bar, list or table, 40 at a time.
//! * `<name>.do` with `{id, op, value}`. `set` follows the rules of typing:
//!   read-only fields refuse it, one-line fields turn line breaks into
//!   spaces, `max_length` and `ui.limits` cut, sliders snap to their step,
//!   toggles take booleans, choices take an index in range.
//!
//! Every named cell is readable by agents (`{"describe": true}`,
//! `{"get": "<name>.state"}`), agent-writable or not. Password values are
//! never described, copied or drawn, and an IME preedit in a password field
//! is kept in the UI cell only as dots; a page should keep secrets in its
//! script, not in its state cell (the gallery shows how).
//!
//! # Time
//!
//! `page.now()` in a handler is the page clock at the last tick, and the
//! engine ticks only when something visible is due, so after a quiet spell it
//! is old. Double clicks are timed with `os.clock()`; toast ends, tooltip
//! starts and the caret blink phase use `app:now()`, an estimate of the
//! current page time from the largest `clock - os.clock()` observed
//! (CORE_REQUESTS #7). Draws remain pure functions of their inputs and `t`.
//!
//! # Limits and known gaps
//!
//! * Text: a field holds up to `ui.limits.field` (32 KB), an area up to
//!   `ui.limits.area` (128 KB), measured to stay within a third of the
//!   default 50 ms budget (tests/limits.rs). A longer value set by the page
//!   shows its beginning, read-only, until replaced. Undo keeps 100 steps per
//!   field and 4 MB per app, and is dropped for fields that leave the view.
//! * The engine calls `pointer_up` also when a pinch starts or the host
//!   cancels the pointer; the library cannot tell, so a press held on a
//!   button is activated (CORE_REQUESTS #11).
//! * Large lists or tables fed from a cell are converted item by item every
//!   frame; use `count` + `item(i)` / `row(i)` for big collections
//!   (CORE_REQUESTS #2).
//!
//! See docs/MODULES.md for the module contract.

use anypage_runtime::{Natives, Package};

/// The Luau sources, by the name pages import them with. `"ui"` is the
/// public entry; the others are its parts (importable for advanced use).
pub const LUA_MODULES: [(&str, &str); 8] = [
    ("ui", include_str!("luau/ui.luau")),
    ("ui/theme", include_str!("luau/theme.luau")),
    ("ui/util", include_str!("luau/util.luau")),
    ("ui/layout", include_str!("luau/layout.luau")),
    ("ui/controls", include_str!("luau/controls.luau")),
    ("ui/fields", include_str!("luau/fields.luau")),
    ("ui/views", include_str!("luau/views.luau")),
    ("ui/app", include_str!("luau/app.luau")),
];

/// Add the `ui` Luau library (and its parts) to a native registry.
pub fn register(n: &mut Natives) {
    for (name, src) in LUA_MODULES {
        n.lua_module(name, src);
    }
}

const GALLERY_JSON: &str = include_str!("../../../pages/ui-gallery/page.json");
const GALLERY_MAIN: &str = include_str!("../../../pages/ui-gallery/main.luau");

/// The gallery page (every element, dark and light), bundled into the app.
pub fn gallery() -> Result<Package, String> {
    Package::from_files("builtin:ui-gallery", [("page.json", GALLERY_JSON), ("main.luau", GALLERY_MAIN)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_every_part() {
        let mut n = Natives::new();
        register(&mut n);
        let names = n.names();
        for (name, src) in LUA_MODULES {
            assert!(names.contains(&format!("lua:{name}")), "{name} missing from {names:?}");
            assert!(!src.trim().is_empty(), "{name} is empty");
            // every part ends by returning its table
            assert!(src.trim_end().lines().last().is_some_and(|l| l.starts_with("return ")), "{name} must end with a return");
        }
        // a pure-Luau library: no native module, so pages need no capability for it
        assert!(!n.has_module("ui"));
    }

    #[test]
    fn gallery_package_parses() {
        let p = gallery().expect("gallery package");
        assert_eq!(p.manifest.entry, "main.luau");
        assert!(p.manifest.bounds.w > 0.0);
    }
}
