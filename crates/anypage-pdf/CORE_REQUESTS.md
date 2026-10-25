# Core requests from the `pdf` module

Changes to the runtime or engine that would help this module. Each has the
workaround the module uses today.

## 1. `Res::with_version(v)`

**Why.** An edit returns the next version of the document handle so cells
recompute. When a script edits twice through the same old handle

```lua
local a = pdf.add_text(d, 1, {...})
local b = pdf.add_text(d, 1, {...})   -- d is still the old handle
```

`d:next_version()` would give `b` the same (id, version) as `a`, and cells that
saw `a` would not see `b`. The document keeps its own revision counter; the
handle must carry that number.

**Proposed.** `impl Res { pub fn with_version(&self, version: u64) -> Res }`
(same kind, id and object).

**Workaround.** `api::bump` calls `next_version()` in a loop until the version
equals the revision (cheap: one `Arc` clone per step).

## 2. The semantic port cuts results at depth 3

**Why.** `ScriptPage::act` and `describe` summarise values: lists and maps
deeper than 3 levels become `{"$list": n}` / `{"$map": n}`, strings longer than
400 characters `{"$text": ...}`. The computable form of a PDF page
(`extract` → `tables` → table → `rows` → row → cell) is 5 levels deep, so an
agent calling an action that returns it sees only counts.

**Proposed.** Bound action results by size, not depth (for example 256 KB of
JSON, cutting the longest lists first and marking `$truncated`), or accept
`{"action": ..., "depth": n}`.

**Workaround.** The demo's `extract` action flattens: block texts as strings,
tables as CSV strings.

## 3. Native time inside event handlers counts against the handler budget

**Why.** `Realm::call` sets one wall-clock deadline for a handler. An edit on a
large document (the first edit parses the whole file with lopdf; `save`
serialises it) can take longer than `handler_ms` inside the native function;
the Luau code after it is then interrupted although the script itself was fast.

**Proposed.** Pause the deadline while a native function runs (the `func`
wrapper in `api.rs` can add the native call's duration to the deadline), and
let natives keep their own limits.

**Workaround.** Job `pdf.load` with `{ editable = true }` parses the structure
off the UI thread, so later edits are incremental in-memory changes. After an
edit, page counts, sizes and metadata come from the edited structure, so cells
that lay out the page do not wait for the document to be written out and read
back (that happens on a tile worker or in a job).

## 4. Tile sources cannot report an error

**Why.** `TileSource::render` returns `Option<Vec<u8>>`, where `None` means
"cancelled". A page that cannot be rendered (damaged content, a page removed
by an edit while tiles were queued) has no way to say so; returning `None`
makes the engine ask again.

**Proposed.** `fn render(...) -> Result<Option<Vec<u8>>, String>`, with the
engine drawing its own error tile and not re-requesting until params change.

**Workaround.** An error is logged and a striped error tile is returned.

## 5. One CJK font for the whole app

**Why.** The engine's `TextRaster` reads a CJK system font (msjh.ttc is about
30 MB); this module reads its own copy to draw PDFs whose CJK fonts are not
embedded. Two copies in memory on phones.

**Proposed.** A shared, lazily loaded font registry in the engine
(`anypage_engine::fonts::cjk() -> Option<(Arc<Vec<u8>>, u32)>`), which the
`text` module's `TextSystem` could also own.

**Workaround.** `anypage_pdf::set_cjk_font(bytes, index)` lets a host hand the
module the bytes it already has; otherwise the module loads the font itself,
once, and only when a document needs it.

## 6. (Optional) batched tiles

**Why.** hayro interprets the whole page for every tile (its caches are
`!Send` and borrow the document, so they cannot be shared between tile
workers). Measured: 1.5 to 8 ms per tile for text pages; complex vector pages
at high zoom repeat that work per tile.

**Proposed.** `fn render_batch(&self, keys: &[TileKey], params, cancelled) ->
Vec<Option<Vec<u8>>>` with a default that loops over `render`, so a source can
interpret once and rasterise the visible tiles of a page together.

## 7. Upstream (hayro-syntax): an output cap for stream decoding

Not a runtime or engine change; a request to file with hayro
(github.com/LaurenzV/hayro), recorded here so it is not lost.

**Why.** `hayro-syntax` 0.8 decodes Flate (`filter/lzw_flate.rs`,
`ZlibDecoder::read_to_end` and the pdf.js fallback) and LZW streams to their
end with no limit on the output, including the cross-reference and object
streams it decodes while opening a file. A file of a few hundred KB can make it
allocate gigabytes: an out-of-memory abort on Android. lopdf has
`LoadOptions::max_decompressed_size`; hayro has nothing equivalent.

**Proposed upstream.** A `max_decoded_size: Option<usize>` (or a total budget)
in `Pdf::new_with_options` / `ReaderContext`, honoured by every filter
(`read_to_end` replaced by `take(limit + 1)` and a check), with decoding failing
as for a corrupt stream when it is exceeded.

**Workaround.** `limits::check_streams` scans the raw bytes of every file before
hayro or lopdf see it: each stream's `/Filter` chain is read from its
dictionary, chains that cannot exceed 256 MB from their input size are passed
(nearly every stream), the rest are decoded with counting decoders (Flate as
zlib and raw deflate, LZW, RunLength, ASCIIHex/85 in front) that stop at the cap;
images declaring over 256 Mpx are refused too. A file with such a stream is
not opened. Blind spots: encrypted files (ciphertext) and streams whose
`/Filter` is an indirect object and whose data has no zlib header.

## 8. Upstream (hayro-interpret): interrupting `interpret_page`

**Why.** Text extraction runs hayro's interpreter with a collecting device.
A cancelled `pdf.extract` or `pdf.search` job stops collecting (the device
checks the job's cancel flag every 4096 glyphs) but hayro still runs the rest
of the page's content stream. Measured: ~260 ms for a crafted page of 250k
glyphs in a debug build.

**Proposed upstream.** A `should_stop: Option<Arc<dyn Fn() -> bool>>` in
`InterpreterSettings`, polled between operators.

**Workaround.** The device ignores everything after cancellation; the
layout pass and whole-document loops check the flag themselves.
