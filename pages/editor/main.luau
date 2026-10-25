-- AnyPage 多語言編輯器 (multilingual editor): proves that a page can edit
-- text in any language.
--
--   * one large text area (editor.luau, an element of the UI library) with
--     IME preedit inline, grapheme-correct carets (kept on the right line
--     where a wrapped line breaks with no space), selection, clipboard,
--     undo / redo, word wrap on or off, zoom, line numbers, Tab inserting a
--     tab character;
--   * right-to-left paragraphs (Arabic, Hebrew) right-aligned with carets,
--     hit testing and highlights in their direction; mixed scripts;
--   * a status bar: line and column, direction, lines, words, graphemes,
--     bytes, missing glyphs (text.missing), encoding and line ends;
--   * sample paragraphs in fourteen writing systems;
--   * find and replace (case-insensitive for Latin, Greek and Cyrillic);
--   * open, drop and save .txt / .md: UTF-8 with or without a BOM, UTF-16
--     with a BOM (saved as UTF-8 with a BOM), CRLF / LF / CR line ends kept,
--     binary refused; a modified document is replaced (new, all samples,
--     open, drop) only after the user agrees; save names are sanitised;
--   * two layouts: wide (desktop windows, 1440 x 900) and narrow (phones in
--     portrait and narrow windows, 400 x 820: two toolbar rows, a stacked
--     find bar, a compact status bar), picked from the window size
--     (page.viewport) and shown with page.fit_rect;
--   * agent actions: get_text, set_text, insert, find, replace, stats,
--     load_text, save, set_layout (plus the UI library's editor.* and
--     narrow.* describe / items / do).
--
-- Keys: Ctrl+F find, Ctrl+H replace, F3 / Shift+F3 next / previous,
-- Ctrl+Z / Ctrl+Y (Ctrl+Shift+Z) undo / redo, Ctrl+S save, Ctrl+O open,
-- Ctrl+N new, Ctrl+= / Ctrl+- / Ctrl+0 zoom, Alt+Z word wrap, Tab inserts a
-- tab (Shift+Tab leaves the text), Escape closes the find bar or a dialog and
-- otherwise keeps the caret in the text, R (outside the text) fits the
-- layout to the window again.

local ui = import("ui")
local U = import("ui/util")
local E = import("editor")
local S = import("samples")

-- The narrow app sits far to the right of the wide one, so fitting either
-- never shows the other, and the engine draws only the panel in view.
local WIDE = { 0, 0, 1440, 900 }
local NARROW = { 2400, 0, 400, 820 }
local RECTS = { wide = WIDE, narrow = NARROW }
local FIT_MARGIN = 24 -- logical pixels the engine leaves around a fitted rectangle
local SIZE_MIN, SIZE_MAX, SIZE_DEFAULT = 10, 48, 18
local UNTITLED = "未命名.txt"
local th = ui.themes.dark
local find, sub, gsub = string.find, string.sub, string.gsub
local floor, max, min = math.floor, math.max, math.min
local num = E.num

local PLATFORM = tostring(if page.info and page.info.platform then page.info.platform else "unknown")
local MOBILE = PLATFORM == "android" or PLATFORM == "ios"
-- whether this runtime can read opened and dropped files (text.decode)
local CAN_OPEN = E.can_decode()

-- Where the caret is (byte offsets), for the status bar. The editor keeps
-- its edit state in the app's UI cell, which views do not get; this small
-- cell carries the part the view shows.
local Caret = page.input("editor.caret", { caret = 0, anchor = 0 })
-- The window size in logical pixels; the "viewport" event picks the layout.
local Viewport = page.viewport()

local app -- the wide app; its state cell (editor.state) is the document's state
local narrow -- the narrow app; it shows editor.state too
local apps = {}
local mode = if MOBILE then "narrow" else "wide"
local forced = nil -- a layout an agent asked for; nil: the window size decides
local fitted = false -- the first viewport event fits the layout once
local pending = nil -- what to run when the user agrees to replace a modified document

-- The app of the layout on screen.
local function cur()
	return apps[mode] or app
end

-- ---------------------------------------------------------------- helpers

local function clamp(v, lo, hi)
	return if v < lo then lo elseif v > hi then hi else v
end

-- 1234567 -> "1,234,567"
local function fmt(n)
	local s = tostring(floor(n))
	local neg = sub(s, 1, 1) == "-"
	if neg then
		s = sub(s, 2)
	end
	local out = s
	while true do
		local k
		out, k = gsub(out, "^(%d+)(%d%d%d)", "%1,%2")
		if k == 0 then
			break
		end
	end
	return if neg then "-" .. out else out
end

local function short(s, n)
	n = n or 24
	if #s <= n then
		return s
	end
	return sub(s, 1, U.utf8_cut(s, n)) .. "…"
end

-- The narrow app's handlers keep the tree it last built until one of its
-- own cells changes; the document state is not one of them, so a change to
-- it must say so (else a handler right after, such as focusing the find
-- field that just opened, would look for it in the old tree).
local function touched()
	if mode == "narrow" and narrow ~= nil then
		narrow:ui_set(narrow:ui_get())
	end
end

local function set(k, v)
	app:set(k, v)
	touched()
end

local function update(f)
	app:update(f)
	touched()
end

local function toast(msg, opts)
	cur():toast(msg, opts)
end

-- The document as the editor shows it: always a string, line breaks as
-- "\n", at most E.MAX bytes (longer text set from outside is shown
-- read-only). Returns the text and whether it was cut.
local function text_of(s)
	local t = s.text
	if type(t) ~= "string" then
		t = if type(t) == "number" then tostring(t) else ""
	end
	t = E.normalize(t)
	if #t > E.MAX then
		return sub(t, 1, U.utf8_cut(t, E.MAX)), true
	end
	return t, false
end

-- The font size: agents may write anything there, NaN included (which
-- passes every comparison, so it is caught before clamping).
local function size_of(s)
	return clamp(floor(num(s.size, SIZE_DEFAULT)), SIZE_MIN, SIZE_MAX)
end

local EOL_NAME = { ["\r\n"] = "CRLF", ["\r"] = "CR", ["\n"] = "LF" }

local function eol_of(s)
	return if s.eol == "\r\n" or s.eol == "\r" then s.eol else "\n"
end

local function encoding_of(s)
	if s.enc == "utf-16" then
		return "UTF-16→UTF-8 BOM"
	end
	return "UTF-8" .. (if s.bom == true then " BOM" else "")
end

local function current_text()
	return text_of(app:get())
end

local function on_text(v)
	update(function(st)
		st.text = v
		st.dirty = true
	end)
end

local function on_caret(c, an)
	page.set(Caret, { caret = c, anchor = an })
end

-- The editor element's props, from the document state. `source` gives
-- handlers the current text even when the tree they act on is older.
local function doc_props(s)
	local t, ro = text_of(s)
	return {
		value = t,
		size = size_of(s),
		wrap = s.wrap ~= false,
		readonly = ro,
		source = current_text,
		on_change = on_text,
		on_caret = on_caret,
	}
end

-- A change context for the document in the layout on screen (actions,
-- buttons, keys).
local function ctx()
	return E.ctx(cur(), "doc", doc_props(app:get()))
end

local function focus_doc()
	cur():focus("doc")
end

-- ---------------------------------------------------------------- find and replace

local function find_query()
	local s = app:get()
	local q = s.find
	if type(q) ~= "string" then
		q = ""
	end
	return q, s.case == true
end

-- Index of the first match at or after byte b (nil past the last).
local function first_at(ms, b)
	local lo, hi = 1, #ms + 1
	while lo < hi do
		local mid = (lo + hi) // 2
		if ms[mid] < b then
			lo = mid + 1
		else
			hi = mid
		end
	end
	return if lo <= #ms then lo else nil
end

-- Select the next (dir 1) or previous (dir -1) match. Returns its index and the match list.
local function find_step(dir)
	local q, case = find_query()
	if q == "" then
		return nil, {}
	end
	local c = ctx()
	local ms = E.find_all(c.s, q, case)
	if #ms == 0 then
		toast("找不到「" .. short(q) .. "」 (not found)", { tone = "warning" })
		return nil, ms
	end
	local a, b = E.selection(c)
	local i
	if dir > 0 then
		i = first_at(ms, if b > a then b else a)
		if i == nil then
			i = 1
			toast("已從頭開始搜尋 (wrapped to the start)")
		end
	else
		i = (first_at(ms, a) or (#ms + 1)) - 1
		if i < 1 then
			i = #ms
			toast("已從結尾開始搜尋 (wrapped to the end)")
		end
	end
	E.select(c, ms[i], ms[i] + #q)
	return i, ms
end

local function is_match(ms, a)
	local i = first_at(ms, a)
	return i ~= nil and ms[i] == a
end

local function replace_one()
	local q, case = find_query()
	if q == "" then
		return 0
	end
	local s = app:get()
	local c = ctx()
	local a, b = E.selection(c)
	local done = 0
	if b - a == #q and is_match(E.find_all(c.s, q, case), a) then
		E.replace(c, a, b, tostring(s.repl or ""), "edit")
		done = 1
	end
	find_step(1)
	return done
end

local function replace_all()
	local q, case = find_query()
	if q == "" then
		return 0
	end
	local n, why = E.replace_all(ctx(), q, tostring(app:get().repl or ""), case)
	if n == nil then
		toast(if why == "too_big" then "取代後會超過 256 KB 上限，未取代" else "文件是唯讀的", { tone = "danger" })
		return 0
	end
	toast(if n > 0 then "已取代 " .. fmt(n) .. " 處 (replaced)" else "找不到「" .. short(q) .. "」", { tone = if n > 0 then "success" else "warning" })
	return n
end

local function open_find(field)
	set("find_open", true)
	-- the selection, when it is one short line, is what to look for
	local c = ctx()
	local a, b = E.selection(c)
	if b > a and b - a <= 200 and not find(sub(c.s, a + 1, b), "\n", 1, true) then
		set("find", sub(c.s, a + 1, b))
	end
	cur():focus(field or "find")
end

local function close_find()
	set("find_open", false)
	focus_doc()
end

-- ---------------------------------------------------------------- replacing a modified document

-- Run f now, or once the user agrees when the document has unsaved changes.
-- `what` says what would replace it.
local function replace_doc(what, f)
	if app:get().dirty ~= true then
		f()
		return
	end
	pending = f
	set("confirm", {
		body = "目前的文件有尚未儲存的變更。" .. what .. "會取代它，檔名、行尾與編碼也會重設（文字還能用 Ctrl+Z 復原）。",
		ok = "捨棄變更並繼續",
	})
	cur():focus("confirm-cancel")
end

local function cancel_confirm()
	pending = nil
	set("confirm", nil)
	focus_doc()
end

local function accept_confirm()
	local f = pending
	pending = nil
	set("confirm", nil)
	if f then
		f()
	else
		focus_doc()
	end
end

-- ---------------------------------------------------------------- file names

-- What a file name never keeps: C0 controls and DEL, the characters Windows
-- forbids (path separators among them), C1 controls, the byte order mark,
-- and the invisible direction marks and overrides that can make
-- "x<U+202E>txt.exe" look like "xexe.txt".
local NAME_DROP = {
	"[%c<>:\"/\\|%?%*]",
	"\xC2[\x80-\x9F]",
	"\xE2\x80[\x8E\x8F\xAA-\xAE]",
	"\xE2\x81[\xA6-\xA9]",
	"\xD8\x9C",
	"\xEF\xBB\xBF",
}

-- Windows device names, which cannot be file names with any extension.
local RESERVED = { con = true, prn = true, aux = true, nul = true }
for i = 1, 9 do
	RESERVED["com" .. i] = true
	RESERVED["lpt" .. i] = true
end

-- A file name to show and remember: the last part of a path (from a dialog,
-- a drop or an agent), cleaned of the characters above and of spaces and
-- dots at either end, at most 120 bytes; nil when nothing is left.
local function clean_name(name)
	if type(name) ~= "string" then
		return nil
	end
	name = string.match(name, "([^/\\]*)$") or ""
	if utf8.len(name) == nil then
		return nil
	end
	for _, p in NAME_DROP do
		name = gsub(name, p, "")
	end
	name = gsub(name, "^[%s%.]+", "")
	name = gsub(name, "[%s%.]+$", "")
	if #name > 120 then
		name = sub(name, 1, U.utf8_cut(name, 120))
	end
	return if name == "" then nil else name
end

-- The name the save dialog suggests: clean, ending in .txt or .md (anything
-- else gets ".txt" added, so "x.bat" becomes "x.bat.txt"), never a device
-- name; 未命名.txt when there is none.
local function save_name(name)
	local n = clean_name(name)
	if n == nil then
		return UNTITLED
	end
	local ext = string.lower(string.match(n, "%.([^%.]+)$") or "")
	if ext ~= "txt" and ext ~= "md" then
		n ..= ".txt"
	end
	local stem = string.lower(string.match(n, "^[^%.]*") or "")
	stem = gsub(stem, "%s+$", "")
	if RESERVED[stem] then
		n = "_" .. n
	end
	return n
end

-- ---------------------------------------------------------------- files

local REFUSED = {
	unreadable = "無法讀取檔案內容：這個版本的 AnyPage 還不能把檔案的位元組交給頁面當文字讀 (file bytes cannot be read as text)",
	utf16 = "UTF-16 的文字請以檔案開啟或拖放 (UTF-16 text must come as a file)",
	binary = "這不是文字檔（含有二進位資料，或不是有效的 UTF-8 / UTF-16），未開啟 (binary file refused)",
	too_big = "檔案超過 256 KB 上限，未開啟 (file too large)",
}

local BINARY_EXT = {
	png = true, jpg = true, jpeg = true, gif = true, webp = true, bmp = true, ico = true, pdf = true,
	zip = true, gz = true, ["7z"] = true, rar = true, exe = true, dll = true, bin = true, so = true,
	xlsx = true, docx = true, pptx = true, xls = true, doc = true, mp3 = true, mp4 = true, wav = true,
	glb = true, stl = true, ttf = true, otf = true, woff = true, woff2 = true,
}

local function ext_of(name)
	return string.lower(string.match(name, "%.([%w]+)$") or "")
end

-- Check a file from the open dialog, a drop or load_text. Returns the file
-- { text, eol, bom, mixed, enc } or nil and why not, and its clean name.
local function check(f)
	local name = clean_name(f.name) or UNTITLED
	if BINARY_EXT[ext_of(name)] then
		return nil, "binary", name
	end
	if num(f.size, 0) > E.MAX_FILE then
		return nil, "too_big", name
	end
	local str, why, enc = E.decode_file(f)
	if str == nil then
		return nil, why, name
	end
	local file
	file, why = E.from_file(str)
	if file == nil then
		return nil, why, name
	end
	if enc ~= nil then
		-- text.decode took the byte order mark off: put it back on save (UTF-16
		-- is saved as UTF-8 with one, so Windows tools still see Unicode)
		file.bom = enc ~= "utf-8"
		file.enc = enc
	end
	return file, nil, name
end

local function reset_file(st)
	st.file = nil
	st.eol = "\n"
	st.bom = false
	st.enc = nil
	st.dirty = false
	st.notice = nil
end

-- Make an accepted file the document.
local function apply(file, name)
	E.set_text(ctx(), file.text, 0)
	update(function(st)
		st.file = name
		st.eol = file.eol
		st.bom = file.bom
		st.enc = if file.enc == "utf-16" then "utf-16" else nil
		st.dirty = false
		st.notice = nil
	end)
	local extra = ""
	if file.enc == "utf-16" then
		extra ..= "；UTF-16 已轉成 UTF-8，儲存時存成 UTF-8 (BOM)"
	end
	if file.mixed then
		extra ..= "；行尾混用，儲存時統一為 " .. EOL_NAME[file.eol]
	end
	toast("已開啟 " .. short(name, 40) .. "（" .. fmt(#file.text) .. " 位元組" .. extra .. "）", { tone = "success", duration = if extra ~= "" then 6 else nil })
	focus_doc()
end

local function refused(name, why)
	local msg = short(name, 40) .. "：" .. (REFUSED[why] or tostring(why))
	toast(msg, { tone = "danger", duration = 8 })
	set("notice", msg)
end

-- A file from the open dialog or the load_text action. Returns true when it
-- was opened, else false and the reason.
local function receive(f)
	if type(f) ~= "table" or f.cancelled then
		return false, "cancelled"
	end
	if f.error then
		toast("無法開啟：" .. tostring(f.error), { tone = "danger", duration = 6 })
		return false, "error"
	end
	local file, why, name = check(f)
	if file == nil then
		refused(name, why)
		return false, why
	end
	apply(file, name)
	return true
end

local function open_dialog()
	if not CAN_OPEN then
		toast("這個版本的 AnyPage 還不能開啟檔案 (files cannot be read here)", { tone = "warning" })
		return
	end
	replace_doc("開啟檔案", function()
		page.open_file({ filters = { "txt", "md" }, done = receive })
	end)
end

-- Save the document. Returns the request id, the byte count and the
-- suggested name, or nil, 0, nil and why not.
local function save(name)
	local s = app:get()
	local t, truncated = text_of(s)
	if truncated then
		local msg = "這份文件超過 256 KB，只顯示了前 256 KB（唯讀），存檔會遺失後面的內容，所以沒有儲存 (a truncated view is not saved)"
		toast(msg, { tone = "danger", duration = 8 })
		set("notice", msg)
		return nil, 0, nil, "truncated"
	end
	local fname = save_name(if name ~= nil then name else s.file)
	local bytes = E.to_file(t, eol_of(s), s.bom == true)
	local id = page.save_file({
		name = fname,
		data = bytes,
		done = function(r)
			if type(r) ~= "table" then
				return
			end
			if r.saved then
				update(function(st)
					st.file = clean_name(tostring(r.saved)) or fname
					st.enc = nil
					-- typing while the dialog was open is not saved
					if text_of(st) == t then
						st.dirty = false
					end
				end)
				toast("已儲存 " .. short(tostring(r.saved), 60) .. "（" .. fmt(#bytes) .. " 位元組）", { tone = "success" })
			elseif r.error then
				toast("儲存失敗：" .. tostring(r.error), { tone = "danger", duration = 6 })
			end
		end,
	})
	return id, #bytes, fname
end

local function new_doc()
	replace_doc("開新文件", function()
		E.set_text(ctx(), "", 0)
		update(reset_file)
		focus_doc()
	end)
end

local function all_samples()
	replace_doc("換成全部範例", function()
		E.set_text(ctx(), S.all(), 0)
		update(reset_file)
		focus_doc()
	end)
end

-- ---------------------------------------------------------------- view settings

local function zoom(step)
	local s = app:get()
	local size = if step == 0 then SIZE_DEFAULT else clamp(size_of(s) + step, SIZE_MIN, SIZE_MAX)
	set("size", size)
	E.reveal(ctx())
end

local function set_wrap(on)
	set("wrap", on)
	E.reveal(ctx())
end

local function insert_sample(i)
	local p = S.text(i)
	if p == nil then
		return
	end
	local c = ctx()
	local a, b = E.selection(c)
	local s = c.s
	-- a sample is a paragraph of its own
	local before = if a > 0 and sub(s, a, a) ~= "\n" then "\n" else ""
	local after = if b < #s and sub(s, b + 1, b + 1) ~= "\n" then "\n" else ""
	local caret = a + #before + #p
	E.replace(c, a, b, before .. p .. after, "edit", caret, caret)
	focus_doc()
end

-- ---------------------------------------------------------------- layouts

-- How large a rectangle would be shown in a w x h window.
local function fit_scale(w, h, r)
	return min((w - 2 * FIT_MARGIN) / r[3], (h - 2 * FIT_MARGIN) / r[4])
end

-- The layout for a window of w x h logical pixels: the narrow one when it
-- shows the text half as large again as the wide one would (phones in
-- portrait, narrow windows), else the wide one.
local function layout_for(w, h)
	w, h = num(w, 1280), num(h, 800)
	return if fit_scale(w, h, NARROW) > 1.5 * fit_scale(w, h, WIDE) then "narrow" else "wide"
end

-- Show layout m and fit it to the window. The caret, the selection and the
-- focus go along.
local function set_layout(m)
	local from, to = cur(), apps[m]
	if to ~= from then
		local fu, tu = from:ui_get(), to:ui_get()
		local ed = fu["mle:doc"]
		if type(ed) == "table" then
			tu["mle:doc"] = { caret = ed.caret, anchor = ed.anchor, up = ed.up }
		end
		to:ui_set(tu)
		local had = from:focused_id()
		from:blur()
		mode = m
		if had ~= nil or not MOBILE then
			if not (had ~= nil and to:focus(had)) then
				to:focus("doc")
			end
		end
	end
	page.fit_rect(RECTS[m])
	fitted = true
end

page.on("viewport", function(w, h)
	if forced == nil then
		local m = layout_for(w, h)
		if m ~= mode or not fitted then
			set_layout(m)
		end
	end
	return true
end)

-- ---------------------------------------------------------------- pieces of the views

local function stat(label, value, opts)
	opts = opts or {}
	return ui.row({ gap = 5 }, {
		ui.text(label, { tone = "faint", size = 12, wrap = false }),
		ui.text(value, { size = 13, wrap = false, tone = opts.tone, id = opts.id }),
	})
end

local function btn(id, label, tip, f, variant)
	return ui.button(label, {
		id = id,
		size = "small",
		variant = variant or "secondary",
		tooltip = tip,
		on_click = function()
			f()
		end,
	})
end

local function undo_btn()
	return btn("undo", "復原", "Ctrl+Z", function()
		E.undo(ctx(), false)
		focus_doc()
	end, "ghost")
end

local function redo_btn()
	return btn("redo", "重做", "Ctrl+Y / Ctrl+Shift+Z", function()
		E.undo(ctx(), true)
		focus_doc()
	end, "ghost")
end

local function toggle_find()
	if app:get().find_open then
		close_find()
	else
		open_find("find")
	end
end

local function samples_select(width, placeholder)
	return ui.select({
		id = "samples",
		options = S.labels,
		placeholder = placeholder,
		width = width,
		on_change = function(i)
			insert_sample(i)
		end,
	})
end

local function wrap_switch(s, label)
	return ui.switch(label, {
		id = "wrap",
		checked = s.wrap ~= false,
		tooltip = "Alt+Z",
		on_change = function(v)
			set_wrap(v)
		end,
	})
end

local function zoom_buttons(s, unit)
	return {
		btn("zoom-out", "A−", "縮小 (Ctrl+-)", function()
			zoom(-2)
		end),
		ui.text(size_of(s) .. unit, { id = "size", size = 13, tone = "dim", wrap = false }),
		btn("zoom-in", "A+", "放大 (Ctrl+=)", function()
			zoom(2)
		end),
	}
end

local function findbar(s, ms, cur_i, compact)
	local q = if type(s.find) == "string" then s.find else ""
	local count
	if q == "" then
		count = ui.text("輸入要尋找的文字", { tone = "faint", size = 13, wrap = false })
	elseif #ms == 0 then
		count = ui.text("找不到 (no match)", { id = "count", tone = "warning", size = 13, wrap = false })
	else
		local n = fmt(#ms) .. (if #ms >= 20000 then "+" else "")
		count = ui.text((if cur_i then fmt(cur_i) .. " / " else "") .. n .. " 個符合", { id = "count", tone = "accent", size = 13, wrap = false })
	end
	local find_field = ui.textfield({
		id = "find",
		value = q,
		placeholder = "要尋找的文字 Find",
		width = if compact then nil else 240,
		grow = if compact then 1 else nil,
		on_change = function(v)
			set("find", v)
		end,
		on_submit = function()
			find_step(1)
		end,
	})
	local repl_field = ui.textfield({
		id = "repl",
		value = if type(s.repl) == "string" then s.repl else "",
		placeholder = "取代成 Replace with",
		width = if compact then nil else 240,
		grow = if compact then 1 else nil,
		on_change = function(v)
			set("repl", v)
		end,
		on_submit = function()
			replace_one()
		end,
	})
	local case = ui.checkbox("區分大小寫 Aa", {
		id = "case",
		checked = s.case == true,
		on_change = function(v)
			set("case", v)
		end,
	})
	local prev = btn("prev", "上一個", "Shift+F3", function()
		find_step(-1)
	end)
	local nxt = btn("next", "下一個", "F3 / Enter", function()
		find_step(1)
	end)
	local rep = btn("replace", "取代", nil, function()
		replace_one()
	end)
	local rep_all = btn("replace-all", "全部取代", nil, function()
		replace_all()
	end)
	local close = btn("find-close", "✕", "Escape", close_find, "ghost")
	if compact then
		return ui.column({ padding = { 8, 12 }, gap = 6, bg = th.surface }, {
			ui.row({ gap = 6 }, { find_field, prev, nxt, close }),
			ui.row({ gap = 6 }, { repl_field, rep, rep_all }),
			ui.row({ gap = 8 }, { case, ui.spacer(), count }),
		})
	end
	return ui.row({ padding = { 8, 16 }, gap = 8, bg = th.surface }, {
		ui.text("尋找", { tone = "dim", size = 13, wrap = false }),
		find_field,
		ui.text("取代為", { tone = "dim", size = 13, wrap = false }),
		repl_field,
		case,
		prev,
		nxt,
		rep,
		rep_all,
		count,
		ui.spacer(),
		close,
	})
end

local function statusbar(s, t, readonly, caret, compact)
	local st = E.stats(t)
	-- the caret cell can be older than the text (the app rebuilds views in
	-- handlers with the inputs it last drew): keep it on character boundaries
	local c = U.utf8_cut(t, clamp(floor(num(caret.caret, 0)), 0, #t))
	local an = U.utf8_cut(t, clamp(floor(num(caret.anchor, c)), 0, #t))
	local ci = E.caret_info(t, c)
	local a, b = min(c, an), max(c, an)
	local sel = nil
	if b > a then
		sel = stat(if compact then "選取" else "已選取", fmt(text.graphemes(sub(t, a + 1, b))) .. " 字素", { id = "selection" })
	end
	local missing
	if st.missing > 0 then
		missing = ui.badge("缺字 " .. fmt(st.missing) .. (if compact then "" else "（" .. st.missing_paragraphs .. " 段）"), { id = "missing", tone = "danger", tooltip = "有字元沒有任何已安裝的字型能顯示 (text.missing)" })
	else
		missing = ui.badge("字型齊全", { id = "missing", tone = "success", tooltip = "每個字元都有字型能顯示 (text.missing = 0)" })
	end
	local dir_label = if ci.rtl then "從右到左 RTL" else "從左到右 LTR"
	if compact then
		dir_label = if ci.rtl then "RTL" else "LTR"
	end
	local dir = ui.badge(dir_label, { id = "dir", tone = if ci.rtl then "accent" else "neutral" })
	local pos = stat("位置", "第 " .. fmt(ci.paragraph) .. " 行，第 " .. fmt(ci.column) .. " 欄", { id = "pos" })
	local enc = encoding_of(s) .. " · " .. EOL_NAME[eol_of(s)]
	if compact then
		return ui.column({ padding = { 6, 12, 8, 12 }, gap = 4, bg = th.surface }, {
			ui.row({ gap = 10 }, { pos, dir, ui.spacer(), missing }),
			ui.row({ gap = 10 }, {
				sel or stat("字數", fmt(st.words), { id = "words" }),
				stat("位元組", fmt(st.bytes), { id = "bytes" }),
				ui.when(readonly, ui.badge("唯讀", { tone = "warning" })),
				ui.spacer(),
				ui.text(enc, { id = "encoding", tone = "dim", size = 12, wrap = false }),
			}),
		})
	end
	return ui.row({ padding = { 7, 16 }, gap = 18, bg = th.surface }, {
		pos,
		dir,
		sel,
		stat("行", fmt(st.paragraphs), { id = "lines" }),
		stat("字數", fmt(st.words), { id = "words" }),
		stat("字素", fmt(st.graphemes), { id = "graphemes" }),
		stat("位元組", fmt(st.bytes) .. " / " .. fmt(E.MAX), { id = "bytes" }),
		missing,
		ui.when(readonly, ui.badge("唯讀：只顯示前 256 KB", { tone = "warning" })),
		ui.spacer(),
		ui.text(enc .. " · " .. (if s.wrap ~= false then "自動換行" else "不換行"), { id = "encoding", tone = "dim", size = 12, wrap = false }),
	})
end

local function notice_row(s, compact)
	if type(s.notice) ~= "string" then
		return nil
	end
	return ui.row({ padding = { 6, if compact then 12 else 16 }, gap = 8, bg = color.alpha(th.danger, 40) }, {
		ui.text(s.notice, { id = "notice", size = 13, grow = 1 }),
		btn("notice-close", "知道了", nil, function()
			set("notice", nil)
			focus_doc()
		end, "ghost"),
	})
end

local function confirm_dialog(s)
	local c = s.confirm
	if type(c) ~= "table" then
		return nil
	end
	return ui.dialog({
		id = "confirm",
		open = true,
		title = "取代目前的文件？",
		body = if type(c.body) == "string" then c.body else "目前的文件有尚未儲存的變更。",
		width = 460,
		on_close = function()
			cancel_confirm()
		end,
		actions = {
			btn("confirm-cancel", "取消", nil, cancel_confirm),
			btn("confirm-ok", if type(c.ok) == "string" then c.ok else "捨棄變更並繼續", nil, accept_confirm, "danger"),
		},
	})
end

-- The document element and what the views show around it: the text, whether
-- it was cut (read-only), the find matches and the current one.
local function doc_part(s, caret, compact)
	local t, readonly = text_of(s)
	local q = if s.find_open and type(s.find) == "string" then s.find else ""
	local ms = if q ~= "" then E.find_all(t, q, s.case == true) else {}
	local cur_i = nil
	if q ~= "" then
		local c = floor(num(caret.caret, 0))
		local an = floor(num(caret.anchor, c))
		local a, b = min(c, an), max(c, an)
		if b - a == #q and is_match(ms, a) then
			cur_i = first_at(ms, a)
		end
	end
	local props = doc_props(s)
	props.id = "doc"
	props.grow = 1
	props.matches = if q ~= "" then ms else nil
	props.match_len = #q
	props.current_match = cur_i
	props.placeholder = if compact then "輸入任何語言的文字… Type here" else "在這裡輸入任何語言的文字… Type in any language"
	return t, readonly, ms, cur_i, E.element(props)
end

-- ---------------------------------------------------------------- the wide view

local LANGS = "中文 · 日本語 · 한국어 · العربية · עברית · हिन्दी · ไทย · Ελληνικά · Русский · \u{1F9D1}\u{200D}\u{1F4BB}"

local function wide_view(s, caret)
	if type(caret) ~= "table" then
		caret = {}
	end
	local t, readonly, ms, cur_i, doc = doc_part(s, caret, false)
	local file = clean_name(s.file) or "未命名 (untitled)"
	local toolbar = {
		ui.when(CAN_OPEN, btn("open", "開啟…", "開啟 .txt / .md 檔案，UTF-8 或 UTF-16 (Ctrl+O)", open_dialog)),
		btn("save", "儲存…", "存成 UTF-8 文字檔 (Ctrl+S)", function()
			save()
		end),
		btn("new", "新文件", "清空文件 (Ctrl+N)", new_doc),
		ui.divider({ margin = 4 }),
		undo_btn(),
		redo_btn(),
		ui.divider({ margin = 4 }),
		samples_select(230, "插入範例 Insert sample…"),
		btn("all-samples", "全部範例", "把文件換成所有語言的範例", all_samples),
		ui.divider({ margin = 4 }),
		wrap_switch(s, "自動換行"),
		ui.divider({ margin = 4 }),
		zoom_buttons(s, " px"), -- a plain list: spliced in
		ui.spacer(),
		btn("find-toggle", if s.find_open then "關閉尋找" else "尋找與取代", "Ctrl+F / Ctrl+H", toggle_find, if s.find_open then "secondary" else "primary"),
	}
	return ui.column({ gap = 0 }, {
		ui.row({ padding = { 12, 16, 4, 16 }, gap = 12 }, {
			ui.text("AnyPage 多語言編輯器", { size = 20, wrap = false }),
			ui.text("Multilingual editor · " .. LANGS, { tone = "dim", size = 13, wrap = false, shrink = true }),
			ui.spacer(),
			ui.text(short(file, 48), { id = "file", tone = "dim", size = 13, wrap = false }),
			ui.when(s.dirty == true, ui.badge("已修改 modified", { tone = "warning" })),
		}),
		ui.row({ padding = { 6, 16, 10, 16 }, gap = 8 }, toolbar),
		ui.when(s.find_open == true, findbar(s, ms, cur_i, false)),
		notice_row(s, false),
		ui.column({ padding = { 8, 16, 8, 16 }, grow = 1 }, { doc }),
		statusbar(s, t, readonly, caret, false),
		confirm_dialog(s),
	})
end

-- ---------------------------------------------------------------- the narrow view

-- The document state and the caret are inputs of this app (so it draws when
-- they change), read here with page.get: handlers rebuild a view with the
-- inputs of its last draw, which for this app would be an older document.
local function narrow_view()
	local s = app:get()
	local caret = page.get(Caret)
	if type(caret) ~= "table" then
		caret = {}
	end
	local t, readonly, ms, cur_i, doc = doc_part(s, caret, true)
	local file = clean_name(s.file) or "未命名"
	local row2 = {
		samples_select(132, "範例…"),
		wrap_switch(s, "換行"),
		ui.spacer(),
		zoom_buttons(s, ""),
	}
	return ui.column({ gap = 0 }, {
		ui.row({ padding = { 10, 12, 2, 12 }, gap = 8 }, {
			ui.text("多語言編輯器", { size = 17, wrap = false }),
			ui.text(short(file, 24), { id = "file", tone = "dim", size = 12, wrap = false, shrink = true }),
			ui.spacer(),
			ui.when(s.dirty == true, ui.badge("已修改", { tone = "warning" })),
		}),
		ui.row({ padding = { 6, 12, 2, 12 }, gap = 6 }, {
			ui.when(CAN_OPEN, btn("open", "開啟", "開啟 .txt / .md 檔案 (Ctrl+O)", open_dialog)),
			btn("save", "儲存", "存成 UTF-8 文字檔 (Ctrl+S)", function()
				save()
			end),
			btn("new", "新文件", "清空文件 (Ctrl+N)", new_doc),
			ui.spacer(),
			undo_btn(),
			redo_btn(),
			btn("find-toggle", if s.find_open then "關閉" else "尋找", "Ctrl+F / Ctrl+H", toggle_find, if s.find_open then "secondary" else "primary"),
		}),
		ui.row({ padding = { 6, 12, 8, 12 }, gap = 6 }, row2),
		ui.when(s.find_open == true, findbar(s, ms, cur_i, true)),
		notice_row(s, true),
		ui.column({ padding = { 2, 8, 6, 8 }, grow = 1 }, { doc }),
		statusbar(s, t, readonly, caret, true),
		confirm_dialog(s),
	})
end

-- ---------------------------------------------------------------- keys before the apps'
--
-- Registered before ui.app installs its handlers, so these shortcuts work
-- whichever element has focus (or none), and Tab and Escape reach the page
-- before the library moves or drops the focus.

local ZOOM_IN = { ["ctrl+="] = true, ["ctrl++"] = true, ["ctrl+shift++"] = true, ["ctrl+shift+="] = true }
local ZOOM_OUT = { ["ctrl+-"] = true, ["ctrl+shift+-"] = true, ["ctrl+shift+_"] = true }

-- Whether a text element of the layout on screen has the focus.
local function typing()
	local f = cur():focused_id()
	return f == "doc" or f == "find" or f == "repl"
end

local function shortcut(k)
	if app == nil then
		return false
	end
	local s = app:get()
	if type(s.confirm) == "table" then
		-- a dialog is open: Escape cancels it; the library moves between and
		-- presses its buttons; nothing else happens behind it
		if k == "escape" then
			cancel_confirm()
			return true
		end
		return false
	end
	if k == "escape" then
		-- never leaves the text with nowhere for the next keys to go
		if cur():ui_get().open ~= nil then
			return false -- an open drop-down: the library closes it
		end
		if s.find_open then
			close_find()
		else
			if type(s.notice) == "string" and cur():focused_id() == "doc" then
				set("notice", nil)
			end
			focus_doc()
		end
	elseif k == "tab" then
		if cur():focused_id() ~= "doc" then
			return false -- the library moves the focus
		end
		E.insert(ctx(), "\t", "edit")
	elseif k == "r" then
		if typing() then
			return false
		end
		page.fit_rect(RECTS[mode])
	elseif k == "ctrl+f" then
		open_find("find")
	elseif k == "ctrl+h" then
		open_find("repl")
	elseif k == "f3" then
		find_step(1)
	elseif k == "shift+f3" then
		find_step(-1)
	elseif k == "ctrl+s" then
		save()
	elseif k == "ctrl+o" then
		open_dialog()
	elseif k == "ctrl+n" then
		new_doc()
	elseif ZOOM_IN[k] then
		zoom(2)
	elseif ZOOM_OUT[k] then
		zoom(-2)
	elseif k == "ctrl+0" then
		zoom(0)
	elseif k == "alt+z" then
		set_wrap(s.wrap == false)
	else
		return false
	end
	return true
end

page.on("key", shortcut)

-- the same shortcuts for agents operating the app (editor.do {op = "key"})
local KEYS = {}
for _, k in { "ctrl+f", "ctrl+h", "f3", "shift+f3", "ctrl+s", "ctrl+o", "ctrl+n", "ctrl+0", "alt+z" } do
	KEYS[k] = function()
		shortcut(k)
	end
end
for k in ZOOM_IN do
	KEYS[k] = function()
		shortcut(k)
	end
end
for k in ZOOM_OUT do
	KEYS[k] = function()
		shortcut(k)
	end
end

-- A dropped file is checked first; a good one replaces a modified document
-- only once the user agrees.
page.on("file_drop", function(f)
	if type(f) ~= "table" then
		return true
	end
	local file, why, name = check(f)
	if file == nil then
		refused(name, why)
		return true
	end
	replace_doc("開啟「" .. short(name, 40) .. "」", function()
		apply(file, name)
	end)
	return true
end)

-- ---------------------------------------------------------------- the apps

app = ui.app({
	name = "editor",
	rect = WIDE,
	theme = th,
	agent = true,
	keys = KEYS,
	inputs = { Caret },
	-- a blinking caret redraws the whole panel twice a second (NOTES.md §5)
	blink = not MOBILE,
	state = {
		text = S.welcome({ can_open = CAN_OPEN, narrow = mode == "narrow" }),
		wrap = true,
		size = SIZE_DEFAULT,
		find_open = false,
		find = "",
		repl = "",
		case = false,
		eol = "\n",
		bom = false,
		dirty = false,
	},
	view = wide_view,
})

narrow = ui.app({
	name = "narrow",
	rect = NARROW,
	theme = th,
	agent = true,
	keys = KEYS,
	inputs = { app.state, Caret },
	blink = not MOBILE,
	state = {},
	view = narrow_view,
})

apps.wide, apps.narrow = app, narrow

if mode == "narrow" then
	page.fit_rect(NARROW)
end

-- start with the caret in the text, so typing goes there (on phones only
-- once tapped: focusing would raise the soft keyboard over the page)
if not MOBILE then
	focus_doc()
end

-- ---------------------------------------------------------------- agent actions

local function params(p)
	return if type(p) == "table" then p else {}
end

local function need_string(v, what)
	if type(v) ~= "string" then
		error(what .. " must be a string", 0)
	end
	return v
end

page.action({
	name = "get_text",
	description = "The document's text, at most 400 characters per call from byte offset `from` (default 0), "
		.. "ending on a grapheme boundary; `next` is the `from` of the following part (absent at the end).",
	params = { from = "byte offset to start at (default 0)", max_chars = "characters to return, 1..400 (default 400)" },
	run = function(p)
		p = params(p)
		local t = text_of(app:get())
		local from = E.snap(t, num(p.from, 0))
		local n = clamp(floor(num(p.max_chars, 400)), 1, 400)
		local e = utf8.offset(t, n + 1, from + 1)
		local to = if e then e - 1 else #t
		if to < #t then
			to = E.snap(t, to)
			if to <= from then
				to = text.next(t, from)
			end
		end
		return { text = sub(t, from + 1, to), from = from, to = to, next = if to < #t then to else nil, bytes = #t }
	end,
})

page.action({
	name = "set_text",
	description = "Replace the whole document (one undoable step). Line breaks become \\n; text past 256 KB is cut.",
	params = { text = "the new text" },
	run = function(p)
		local v = if type(p) == "string" then p else params(p).text
		need_string(v, "text")
		local c = ctx()
		E.set_text(c, v, 0)
		return { ok = true, bytes = #c.s, lines = E.stats(c.s).paragraphs, truncated = #E.normalize(v) > E.MAX }
	end,
})

page.action({
	name = "insert",
	description = "Insert text at a byte offset (snapped to a grapheme boundary), or at \"caret\", \"start\" or \"end\". "
		.. "One undoable step; the caret goes after the inserted text.",
	params = { at = "byte offset, or \"caret\" (default), \"start\", \"end\"", text = "the text to insert" },
	run = function(p)
		p = params(p)
		local ins = need_string(p.text, "text")
		local c = ctx()
		local at
		if p.at == nil or p.at == "caret" then
			at = c.ed.caret
		elseif p.at == "start" then
			at = 0
		elseif p.at == "end" then
			at = #c.s
		elseif num(p.at, nil) then
			at = E.snap(c.s, num(p.at, 0))
		else
			error("at must be a byte offset, \"caret\", \"start\" or \"end\"", 0)
		end
		local before = #c.s
		E.replace(c, at, at, ins, "edit")
		return { at = at, inserted = #c.s - before, bytes = #c.s, caret = c.ed.caret }
	end,
})

page.action({
	name = "find",
	description = "Find text (opens the find bar, selects the first match at or after the caret). "
		.. "Case-insensitive unless case = true. Returns the count and the first 20 matches with line and column.",
	params = { query = "text to find", case = "true: match case exactly" },
	run = function(p)
		p = params(p)
		local q = need_string(p.query, "query")
		update(function(st)
			st.find_open = true
			st.find = q
			st.case = p.case == true
		end)
		local i, ms = find_step(1)
		local t = text_of(app:get())
		local list = {}
		for j = 1, min(20, #ms) do
			local ci = E.caret_info(t, ms[j])
			list[j] = { at = ms[j], line = ci.paragraph, column = ci.column }
		end
		return { count = #ms, capped = #ms >= 20000, current = i, matches = list }
	end,
})

page.action({
	name = "replace",
	description = "Replace matches of `query` with `with`: every one (all = true, the default) as one undoable step, "
		.. "or the next one after the caret (all = false). Case-insensitive unless case = true.",
	params = { query = "text to find", with = "replacement", all = "false: only the next match", case = "true: match case exactly" },
	run = function(p)
		p = params(p)
		local q = need_string(p.query, "query")
		local with = if p.with == nil then "" else need_string(p.with, "with")
		update(function(st)
			st.find = q
			st.repl = with
			st.case = p.case == true
		end)
		if p.all == false then
			local c = ctx()
			local a, b = E.selection(c)
			if not (b - a == #q and is_match(E.find_all(c.s, q, p.case == true), a)) then
				find_step(1)
			end
			local n = replace_one()
			return { replaced = n }
		end
		local n, why = E.replace_all(ctx(), q, with, p.case == true)
		if n == nil then
			error(if why == "too_big" then "the result would be larger than 256 KB" else "the document is read-only", 0)
		end
		return { replaced = n, bytes = #text_of(app:get()) }
	end,
})

page.action({
	name = "stats",
	description = "Counts and state: bytes, lines, words, graphemes, right-to-left lines, missing glyphs, caret "
		.. "(line, column, direction), selection, wrap, size, file, encoding, line ends, layout, undo steps, "
		.. "and the last render's timings (ms).",
	params = {},
	run = function()
		local s = app:get()
		local t = text_of(s)
		local st = table.clone(E.stats(t))
		local c = ctx()
		local a, b = E.selection(c)
		local ci = E.caret_info(t, c.ed.caret)
		st.lines = st.paragraphs
		st.paragraphs = nil
		st.caret = c.ed.caret
		st.anchor = c.ed.anchor
		st.line = ci.paragraph
		st.column = ci.column
		st.direction = if ci.rtl then "rtl" else "ltr"
		st.selected_bytes = b - a
		st.wrap = s.wrap ~= false
		st.size = size_of(s)
		st.file = clean_name(s.file)
		st.dirty = s.dirty == true
		st.eol = string.lower(EOL_NAME[eol_of(s)])
		st.encoding = encoding_of(s)
		st.layout = mode
		local nu, nr = E.history_size("doc")
		st.undo_steps, st.redo_steps = nu, nr
		local r = cur():stats()
		st.view_ms, st.layout_ms, st.draw_ms, st.render_ms, st.frames = r.view_ms, r.layout_ms, r.draw_ms, r.total_ms, r.frames
		return st
	end,
})

page.action({
	name = "load_text",
	description = "Open text as if it were a file (same checks as a dropped file: no binary, 256 KB); "
		.. "`name` (cleaned to a file name) is used when saving. Replaces the document without asking.",
	params = { name = "file name, e.g. notes.md", text = "the file's content" },
	run = function(p)
		p = params(p)
		local t = need_string(p.text, "text")
		local ok, why = receive({ name = p.name, text = t, size = #t })
		return { opened = ok, reason = if ok then nil else why, name = if ok then app:get().file else nil }
	end,
})

page.action({
	name = "save",
	description = "Ask the user where to save the document (UTF-8, with the line ends and BOM it was opened with). "
		.. "The suggested name is cleaned: no folders or control characters, .txt or .md. "
		.. "A document cut to 256 KB for display is not saved.",
	params = { name = "suggested file name (default: the opened file's, or 未命名.txt)" },
	run = function(p)
		p = params(p)
		local id, n, fname, why = save(if type(p.name) == "string" and p.name ~= "" then p.name else nil)
		if id == nil then
			return { requested = false, reason = why }
		end
		return { requested = true, name = fname, bytes = n, request = id }
	end,
})

page.action({
	name = "set_layout",
	description = "Show the wide (desktop) or narrow (phone portrait) layout and fit it to the window; "
		.. "\"auto\" (the default) lets the window size decide again.",
	params = { layout = "\"wide\", \"narrow\" or \"auto\"" },
	run = function(p)
		local m = if type(p) == "table" then p.layout else p
		if m == nil or m == "auto" then
			forced = nil
			local v = page.get(Viewport)
			v = if type(v) == "table" then v else {}
			m = layout_for(v.w, v.h)
		elseif RECTS[m] == nil then
			error("layout must be \"wide\", \"narrow\" or \"auto\", got " .. tostring(m), 0)
		else
			forced = m
		end
		set_layout(m)
		return { layout = m, auto = forced == nil }
	end,
})
