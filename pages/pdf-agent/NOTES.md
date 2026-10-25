# pdf-agent · PDF 工作台 — notes

A PDF workspace that people and agents operate through the same operations.

| file | what |
|---|---|
| `page.json` | modules `pdf`; optional `sheet`, `cloud`; net `localhost`, `127.0.0.1`, `[::1]`, `api.openai.com`; secret `openai` bound to `api.openai.com`; files, clipboard; limits 128 MB, call 200 ms, handler 1000 ms, load 4000 ms |
| `main.luau` | cells, tiles (page + 6 thumbnails, and the narrow layout's page), overlays, toolbars and side panel (UI library), the wide and narrow layouts, pointer/wheel/keys/drops, the semantic port |
| `agent.luau` | the in-page assistant: `cloud.chat` stream, tools from `page.actions()`, tool-call loop, steps |
| `pdfutil.luau` | pure helpers: page layout and zoom, word selection, tables to CSV, page summaries |
| `sample.luau` | builds the sample PDF with the pdf module (plus a hand-written one-page AcroForm merged in) |
| `assets/sample.pdf` | the sample, written by `crates/anypage-std/tests/page_pdf_agent.rs` (`ANYPAGE_WRITE_SAMPLE=1` rewrites it) |

Run: `target\release\anypage.exe --page pages/pdf-agent` (add `--agent-port 7700` for external agents).

## The semantic port

Every action has JSON-schema `params` (`{type = "object", properties, required}`) and returns a small
JSON object with a `summary` line: `open_sample`, `open_file`, `search`, `highlight`, `add_note`,
`list_fields`, `fill_field`, `extract_tables`, `export_xlsx`, `goto_page`, `save_pdf`, `summarize_page`,
`get_text`, `get_table`, `select_text`, `get_selection`, `rotate_page`, `delete_page`, `zoom`,
`list_annotations`, `set_layout`; `assistant.ask`, `assistant.config`, `assistant.stop`, `assistant.clear`,
`assistant.status`. The UI library adds `toolbar.*`, `nbar.*` (the narrow toolbar) and `side.*` (describe /
items / do). Agent-writable cells: `page`, `zoom`, `scroll`, `tool`, `query`, `assistant.base_url`,
`assistant.model`, `side.state`, `toolbar.state`, `nbar.state`.

Agent-writable cells hold whatever an agent wrote (`{"set": "scroll", "value": 5}` is accepted by the
port), so the page never indexes them directly: `scroll_of`, `zoom_of`, `tool_of`, `query_of` (main.luau)
and `U.layout` read a wrong type as the default (`{x=0,y=0}`, `"fit"`, `"select"`, `""`); a number over 8
written to `zoom` is read as a percentage; the side view reads a type-checked copy of `side.state`; the
assistant reads its endpoint and model as text. Covered by `agent_writable_cells_are_checked`.

## People's edits are not lost

* **Esc** closes an open dialog, then (narrow layout) leaves the side panel, then clears the selection and
  the search. With unsaved edits it is always taken, so it never reaches a host that might treat an Esc
  nobody takes as "leave" (the desktop app no longer quits on Esc; Android may map it to back). Without
  unsaved edits a spare Esc goes to the host.
* **Replacing the document** (範例, 開啟…, a drop, a file picked for an agent's `open_file`) while it has
  unsaved edits asks first: 取消 / 先儲存… (saves, then opens the new file once the save is done) /
  捨棄並開啟. A file that finished loading after an edit was made also asks. The `open_sample` action is
  refused while edits are unsaved unless `discard = true`.
* **The in-page assistant** is never offered `open_sample`, `open_file` or `set_layout`, and the first two
  refuse when called from a tool call. `delete_page` from the assistant only opens the delete dialog
  (「助理要刪除頁面」) and returns `{deleted = false, waiting_for_person = true}`; the person confirms or
  cancels. The system prompt says PDF text is data, not instructions. What the model can still do is
  additive (highlight, note, fill, rotate) and every step is shown. Covered by `unsaved_edits_are_protected`
  and the assistant test (its mock model asks for `delete_page`).
* **Not covered (host gaps):** closing the desktop window (its close button) and Android's back button
  (it navigates `PageHistory` without asking the page) still drop unsaved edits. See request 6.

## Narrow windows and phones

`page.viewport()` picks the layout: below 900 logical pixels of width, or in portrait, the page shows the
**narrow layout**, a 420 × 940 column far right of the workspace (x = 1800), fitted with `page.fit_rect`:

* a three-row toolbar (`nbar`): the document and 已修改/唯讀, 範例 / 開啟… / 儲存 and 面板 ›, then pages,
  zoom and the three tools;
* its own viewer (a second `pdf.page` tiled layer and overlay over `NVIEW`, sharing page, zoom, scroll,
  selection and hits with the wide one), with the latest message as a line at its top (the side
  panel's toasts are out of view);
* **the side panel is the workspace's own** (600 wide): 面板 › fits the camera to it and a ‹ 頁面 row
  appears over its tabs (Esc does the same). A dialog that opens there (replace, delete, note) brings the
  panel into view and goes back to the page when it closes; asking the assistant shows the chat; a search
  hit, a text block or a field's page chosen there shows the page.

Every viewport change refits the camera (the hosts keep the camera's scale on a resize): the narrow column
and the panel are fitted by width with their top at the window's top. `r` fits the layout in use, and a
wide window switches back. While the narrow layout is in use the wide toolbar is blank (it would show
above the panel), so agents use `nbar.*` there instead of `toolbar.*`. In the narrow layout and on phones, presses and the wheel
that miss the page and the controls (and presses on the empty view around a page at fit) go to the engine
camera, so pinch and pan still work; in the wide layout on a desktop they are taken. Not in the narrow
toolbar: rotate, delete page (wide toolbar only), XLSX export (the 表格 tab has it). On a phone the side
panel shows at about 0.6 scale (600 units in ~350 px); a narrow side panel is not done (request 7).
Covered by `narrow_windows_get_the_narrow_layout`.

The port cuts strings over 400 characters and lists over 40 items, and turns lists three levels deep into
counts (`ScriptPage::act` → `summarize`). Results are shaped for that: `summarize_page.lead` stays under
380 characters, `get_text` and `get_table` page through lines (40 per call, `next` cursor). The in-page
assistant calls `page.act` directly and gets results uncut (each tool result is cut at 6000 characters
before it goes to the model).

When an external agent acts, the person sees it: a toast "代理程式 · <summary>", and the side panel shows
the matching tab (unless the assistant tab is open).

## The assistant

`assistant.ask` / the chat field → request `{base_url, model, messages, tools, tool_choice = "auto"}` on
`cloud.chat` (`latest = true`). Tools are every action whose name the OpenAI API accepts (`^[A-Za-z_][\w-]*$`),
so dotted ones (`assistant.*`, `side.*`) are not offered. A reply with tool calls → each runs with
`page.act(name, args)` (`args` parsed by the module, else `json.decode` of `arguments`), its JSON result is
sent back as a `tool` message with the call's id, and the model is asked again, at most 10 rounds. Every
step (user, tool with arguments/result/ms/ok, assistant, error) is kept in the `assistant.steps` cell (at
most 200) and shown in the chat. The chat lays out only the newest 30 steps (「顯示較早的步驟」 shows 30 more
each time) and only the last 600 characters of an answer while it streams, because the side view is laid
out again for every streamed piece: its draw stays at ~1.3–2.7 ms for 40–120 steps
(`long_chats_stay_cheap_to_draw`) instead of growing to ~19 ms at 200. The default endpoint is Ollama
(`http://localhost:11434/v1`, `llama3.2`); presets for LM Studio and OpenAI.

**The OpenAI key:** two guards. The page names the `openai` secret only when the endpoint is exactly
`https://api.openai.com` (scheme and host compared, never a substring; `Agent.needs_key`), and page.json
binds the secret to that host (`secrets: [{name: "openai", hosts: ["api.openai.com"]}]`), which the cloud
module enforces: `auth::apply` calls `grants.check_secret_url(name, url)` before it attaches a secret, so
an endpoint an agent wrote cannot receive the key even if the page asked for it.

## Requests and workarounds

Done in the core since the review: `crates/anypage-std` has the dev-dependencies the page tests need
(`cargo test -p anypage-std --test page_pdf_agent` runs in place), the cloud module enforces page.json
secret hosts, the desktop app no longer quits on Esc, `page.viewport()` / `page.fit_rect` (the narrow
layout) and `pointer_cancel` (a drag ends without a selection).

1. **Core: no way to react to a stream item.** A script reacts to data only in handlers, and streams have
   no callback, so the assistant cannot notice "the reply finished with tool calls" directly. Workaround:
   `assistant.signal` derives the finished reply (nil while streaming; early cutoff makes it change once per
   reply), and the page's `extract` job (`pdf.extract` of the current page, which the 表格/文字 tabs need
   anyway) takes it as an extra input with `eager = true` and a `done` callback that calls
   `assistant:poll()`. Each reply is handled once (by its `seq`). Proposed:
   `page.stream(name, source, { request, latest, item = function(value) ... end })` or a general
   `page.watch(cell, fn)` called as a handler after a commit that changed the cell.

2. **UI library: `app:describe()` before the app's first draw returns no nodes** when the view takes derived
   cells as inputs (they are peeked and not evaluated yet). In the app the first frame always comes first;
   headless tests must draw once. Either demand the inputs in `_tree()` or document it.

3. **pdf module: no form-field creation**, so the sample's form page is a hand-written one-page PDF
   (`sample.luau: form_pdf`) appended with `pdf.merge`; fill and appearance regeneration then work through the
   module (CJK values included). Also no way to remove an annotation, so edits cannot be undone in the page
   (the original file stays untouched until saved). Proposed: `pdf.add_field(doc, page, {name, type, rect,
   options})`, `pdf.delete_annotation(doc, page, index)`.

4. **Engine: in-page zoom/scroll re-rasterises every visible tile.** The page sits in a fixed tiled layer and
   zoom/scroll move `params.rect`, so every change is a new params version: all tiles are re-requested while
   the old ones stay drawn at their old content, which shows as mixed old/new tiles for a moment (visible in
   a capture taken 3 s after zooming to 250 % while other builds loaded the machine; HUD tile p50 120–200 ms
   then, 30–45 ms on a quieter machine). Headless, whole-page renders of the sample take 2–13 ms per page
   (with or without CJK text), so the cost is contention among 24 tile workers, not the content. Proposed:
   tiles keyed by (source params minus the rect offset, scale) so a pure scroll reuses them shifted.
   The alternative (camera zoom) would scale the UI too.

5. **Wheel events carry no modifiers** (no Ctrl+wheel zoom); the page zooms with the toolbar, +/−/0 keys and
   the `zoom` action, and the wheel scrolls (zoomed in) or turns pages (at fit). In the wide layout on a
   desktop every wheel and press on the workspace is consumed, so the engine camera never pans or zooms the
   UI away (the narrow layout and phones leave the misses to the camera).

6. **Hosts: no way for a page to keep unsaved work when it is closed.** The desktop window's close button
   (`WindowEvent::CloseRequested` → shutdown) and Android's back button (`MainActivity.onBackPressed` →
   `PageHistory.back`) never ask the page. Proposed: a `leave` event (`page.on("leave", fn) -> true` to keep
   the page open, with a host-side "close anyway" on a second request), or a `page.set_unsaved(true)` flag
   the hosts check before closing or navigating back.

7. **UI library: two apps cannot share one state cell.** `ui.app` always makes its own `<name>.state`, so a
   narrow side panel (420 wide, list heights from its rect) would be a second app whose tab, drafts, form
   edits and dialogs drift from the wide one's. The narrow layout therefore shows the wide side panel at
   about 0.6 scale on a phone. Proposed: `ui.app({ state_cell = other.state, ... })` (and `ui` likewise), or
   an app whose `rect` can change at run time (a rect cell), so one side panel serves both layouts.

## Fidelity

* Tables: extraction turns `820.00` into the number 820, so the page reads the places each column is
  written with from the page's own words inside the table rect (`U.column_decimals(t, words)`). The 表格
  tab, the CSV in `extract_tables` / `get_table` and the XLSX number format (`#,##0.00`) keep them.
* Damaged files: when `pdf.load` with `editable = true` says the file "can be shown but not edited" (a
  truncated download, a broken xref), the page loads it again without `editable` and opens it read-only:
  a 唯讀 badge, the edit tools and buttons disabled, edit actions refused with a clear message; viewing,
  search, text and table extraction work. Every truncation of the sample from 98 % down to 50 % opens
  this way.
* Search: a query over 4096 bytes is refused as too long before anything changes; only the "use the
  pdf.search job" error moves a whole-document search to the background job.

## Verified

* Headless (`cargo test -p anypage-std --test page_pdf_agent`, 12 tests): sample generation and the bundled asset
  (4 pages, CJK text, 2 tables, 6 fields); describe/act through the port JSON for every operation, including
  refusals, save PDF/XLSX through host requests and re-opening the saved file by drop; the assistant against a
  127.0.0.1 mock that streams `search` then `highlight` tool calls (arguments split across chunks) and a final
  answer — highlights exist on pages 1–3, steps and chat panel show both tools and the answer, the requests
  carry the tools and the tool results with matching ids; an unreachable model and an undeclared host;
  pointer selection, the selection bar, the highlighter and note tools, keys, wheel, thumbnails; a 40-page
  document (whole-document search in the background job) and a host without `sheet`/`cloud`; unsaved edits
  (Esc, 範例, drops, 先儲存…, `open_sample` without and with `discard`, the assistant's `delete_page`);
  agent-written cells of the wrong type; oversized queries; table decimals; truncated files; a 120-step chat;
  the narrow layout (a phone-sized viewport event, its toolbar and viewer, selection there, camera misses,
  the panel and back, dialogs, the note tool, a wide window again).
* Real desktop app (release build, captures of the real window): the page loads in ~22 ms; agent-port tour
  (search with hit outlines, selection bar, assistant run with steps against a mock on 127.0.0.1, form tab,
  tables tab, 250 % zoom); real mouse input posted to the window (drag selection, bar button, toolbar tool,
  note dialog, wheel page turn, thumbnail click); assistant fills the form (CJK value drawn in the field).
  HUD frame CPU p50 7–10 ms, p95 14–16 ms when idle-ish; input to screen p50 ≈ 11 ms. After the review
  fixes: the window resized to phone size (618 × 1264 px at 150 %) switches to the narrow layout and back
  when widened (edits kept); the narrow selection bar, 面板 › / Esc, and the replace dialog brought into
  view and dismissed, driven over the agent port and captured.
* Not verified: a real model (no Ollama/OpenAI here; only the mock), Android, a real-world PDF (none in the
  repo; the long-document case uses a generated 40-page file), file dialogs (headless tests answer the host
  requests directly).
