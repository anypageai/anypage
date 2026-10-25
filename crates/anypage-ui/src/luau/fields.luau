-- ui/fields: text fields (one line) and text areas (wrapping, scrolling).
--
-- Positions are byte offsets 0..#s (bytes before the caret). Movement is by
-- grapheme (text.next / text.prev) and by word (text.next_word /
-- text.prev_word); hit testing and caret x come from text.hit / text.caret, so
-- the host's text system decides what a character is and how wide it is.
--
-- The value is controlled: a field shows `value` and reports edits through
-- on_change(new_value, app). Caret, selection, horizontal scroll and the IME
-- preedit live in the app's UI state cell (u.edit, u.ime); undo history is
-- per field, kept on the Luau side.

local U = import("ui/util")
local L = import("ui/layout")
local Theme = import("ui/theme")

local T = L.types
local max, min, floor = math.max, math.min, math.floor
local huge = math.huge
local tnext, tprev, tnw, tpw = text.next, text.prev, text.next_word, text.prev_word
local tcaret, thit = text.caret, text.hit

local F = {}

local PAD = 10 -- horizontal padding inside a field
local PADY = 8 -- vertical padding inside a text area
-- Size limits in bytes. A text area's first layout costs about 6.5 us per
-- wrapped line plus 0.2 us per paragraph on the development machine (debug
-- test build), inside one draw under the page's call_ms budget (default
-- 50 ms); a keystroke lays out its paragraph again. At 128 KB the worst
-- measured shapes take about 13 ms (first frame of wrapping prose) and
-- 11-16 ms per keystroke (one 128 KB paragraph), a third of the default
-- budget (tests/limits.rs). Values set from outside that are longer show
-- their beginning, read-only. Larger documents need a native text document
-- (core request #12).
local MAX_FIELD = 32768
local MAX_AREA = 131072
local MAX_UNDO = 100

local function value_of(el)
	local v = el.value
	if v == nil then
		return ""
	end
	if type(v) ~= "string" then
		return tostring(v)
	end
	return v
end
F.value_of = value_of

local function sel_range(ed)
	local a, b = ed.anchor, ed.caret
	if a > b then
		a, b = b, a
	end
	return a, b
end

-- The edit state of `el` (from ui state `u`), clamped to the current value.
local function edit_of(u, el, s)
	local ed = u.edit
	if type(ed) ~= "table" or ed.id ~= el.id then
		ed = { id = el.id, caret = #s, anchor = #s }
	else
		ed = table.clone(ed)
	end
	ed.caret = U.utf8_cut(s, U.clamp(floor(tonumber(ed.caret) or #s), 0, #s))
	ed.anchor = U.utf8_cut(s, U.clamp(floor(tonumber(ed.anchor) or ed.caret), 0, #s))
	return ed
end
F.edit_of = edit_of

-- What the field shows: the value (masked for passwords) with the IME
-- preedit in place of the selection. Returns the display string, the caret,
-- the selection, and the preedit range, all as display byte offsets.
local function display(el, s, ed, ime)
	local caret = ed.caret
	local a, b = sel_range(ed)
	if el.password then
		caret, a, b = U.mask_pos(s, caret), U.mask_pos(s, a), U.mask_pos(s, b)
		s = U.mask(s)
	end
	if ime ~= nil and ime ~= "" then
		local pre = if el.password then U.mask(ime) else ime
		local disp = string.sub(s, 1, a) .. pre .. string.sub(s, b + 1)
		return disp, a + #pre, a + #pre, a + #pre, a, a + #pre
	end
	return s, caret, a, b, nil, nil
end

-- ---------------------------------------------------------------- wrapped lines
--
-- A text area's value is laid out paragraph by paragraph (split at "\n").
-- The wrapped lines of each paragraph are cached by (size, width, paragraph
-- string), so an edit lays out again only the paragraph it touched; the rest
-- of a keystroke is a split of the value and table lookups. A document (the
-- paragraphs of one value with their byte offsets and first line numbers)
-- is cached by the value string, which Luau interns, so the handler that
-- made an edit and the draw that shows it share one.

-- A paragraph of n bytes cannot be wider than n * size * WIDE (no glyph is
-- two ems wide per UTF-8 byte), so short paragraphs need no measuring, no
-- substring and no cache entry.
local WIDE = 2
-- Bytes of paragraph text the line cache keeps before it starts a new
-- generation (paragraphs still in use are carried over on their next lookup).
local PARA_BUDGET = 4 * 1024 * 1024
-- Documents kept: the value, the value with an IME preedit, the previous
-- value; and at most this many paragraphs over all of them.
local DOCS_KEPT = 4
local DOC_PARAS_KEPT = 200000

local para_cur, para_old = {}, {}
local para_bytes = 0
local twidth = text.width

-- Wrapped lines of paragraph `ps` (no newline in it, longer than the
-- one-line bound): false for one line, else the layout's { {from, to} } in
-- paragraph-local byte offsets.
local function para_lines(key, ps, size, w)
	local cur = para_cur[key]
	if cur == nil then
		cur = {}
		para_cur[key] = cur
	end
	local v = cur[ps]
	if v ~= nil then
		return v
	end
	local old = para_old[key]
	if old ~= nil then
		v = old[ps]
	end
	if v == nil then
		if twidth(ps, size) <= w then
			v = false
		else
			local lines = text.layout(ps, size, w).lines
			v = if #lines > 1 then lines else false
		end
	end
	cur[ps] = v
	para_bytes += #ps
	if para_bytes > PARA_BUDGET then
		para_old, para_cur, para_bytes = para_cur, {}, 0
	end
	return v
end

local docs = {} -- key -> { [value] = doc }
local doc_order = {} -- { {key, value, paragraphs} } oldest first
local doc_paras = 0

-- The wrapped lines of `s` at `size` within width `w`:
--   { s, n = visual lines, np = paragraphs, po = byte offset of each
--     paragraph, pl = its first line, lay = { [k] = lines } for the
--     paragraphs that wrap }
-- Always at least one line; a trailing newline gives an empty last line.
local function doc_of(s, size, w)
	w = max(1, floor(w))
	local key = size .. "|" .. w
	local per = docs[key]
	if per == nil then
		per = {}
		docs[key] = per
	end
	local d = per[s]
	if d ~= nil then
		return d
	end
	local po, pl, lay = {}, {}, {}
	local short = w / (size * WIDE)
	local i, k, line, len = 1, 0, 1, #s
	local find, sub = string.find, string.sub
	while true do
		local j = find(s, "\n", i, true)
		local e = if j then j - 1 else len
		k += 1
		po[k], pl[k] = i - 1, line
		if e - i + 1 > short then
			local v = para_lines(key, sub(s, i, e), size, w)
			if v then
				lay[k] = v
				line += #v
			else
				line += 1
			end
		else
			line += 1
		end
		if j == nil then
			break
		end
		i = j + 1
	end
	d = { s = s, n = line - 1, np = k, po = po, pl = pl, lay = lay }
	per[s] = d
	doc_order[#doc_order + 1] = { key, s, k }
	doc_paras += k
	while #doc_order > 1 and (#doc_order > DOCS_KEPT or doc_paras > DOC_PARAS_KEPT) do
		local o = table.remove(doc_order, 1)
		doc_paras -= o[3]
		local m = docs[o[1]]
		if m then
			m[o[2]] = nil
		end
	end
	return d
end
F.doc_of = doc_of

-- The paragraph holding visual line i.
local function para_of_line(doc, i)
	local pl = doc.pl
	local lo, hi = 1, doc.np
	while lo < hi do
		local mid = (lo + hi + 1) // 2
		if pl[mid] <= i then
			lo = mid
		else
			hi = mid - 1
		end
	end
	return lo
end

-- Visual line i: its text (no line break), and its byte range in the value.
local function line_str(doc, i)
	local k = para_of_line(doc, i)
	local s = doc.s
	local base = doc.po[k]
	local pend = if k < doc.np then doc.po[k + 1] - 1 else #s
	local f, t = base, pend
	local v = doc.lay[k]
	if v then
		local ln = v[i - doc.pl[k] + 1]
		f, t = base + ln.from, base + ln.to
	end
	local str = string.sub(s, f + 1, t)
	if t == pend and string.byte(str, #str) == 13 then
		str = string.sub(str, 1, #str - 1) -- "\r\n" line ends set from outside
	end
	return str, f, t
end

-- Index of the visual line holding byte position p: the last line starting at or before p.
local function line_at(doc, p)
	local po = doc.po
	local lo, hi = 1, doc.np
	while lo < hi do
		local mid = (lo + hi + 1) // 2
		if po[mid] <= p then
			lo = mid
		else
			hi = mid - 1
		end
	end
	local v = doc.lay[lo]
	if not v then
		return doc.pl[lo]
	end
	local q = p - po[lo]
	local a, b = 1, #v
	while a < b do
		local mid = (a + b + 1) // 2
		if v[mid].from <= q then
			a = mid
		else
			b = mid - 1
		end
	end
	return doc.pl[lo] + a - 1
end

local function caret_x_in(doc, size, i, p)
	local str, f = line_str(doc, i)
	return tcaret(str, size, U.clamp(p - f, 0, #str))
end

local function area_geom(el)
	return el.x + PAD, el.y + PADY, max(1, el.w - 2 * PAD), max(1, el.h - 2 * PADY)
end

-- ---------------------------------------------------------------- caret visibility

local function get_scroll(u, id)
	local s = u.scroll
	return (type(s) == "table" and tonumber(s[id])) or 0
end

local function put_scroll(u, id, v)
	if type(u.scroll) ~= "table" then
		u.scroll = {}
	end
	u.scroll[id] = if v ~= 0 then v else nil
end

-- Scroll so the caret stays visible; returns the caret rect (app-local).
local function follow(el, u, s, ed, ime)
	local size = el._size
	local lh = U.line_height(size)
	local disp, dcar = display(el, s, ed, ime)
	if el.type == "textfield" then
		local vis = max(1, el.w - 2 * PAD)
		local x = tcaret(disp, size, dcar)
		local sx = tonumber(ed.sx) or 0
		if x - sx > vis - 2 then
			sx = x - vis + 2
		end
		if x < sx then
			sx = x
		end
		local total = U.width(disp, size)
		if sx > 0 and total - sx < vis - 2 then
			sx = max(0, total - vis + 2)
		end
		ed.sx = if sx > 0 then sx else nil
		return { el.x + PAD + x - sx, el.y + (el.h - lh) / 2, 2, lh }
	end
	local ix, iy, iw, ih = area_geom(el)
	local doc = doc_of(disp, size, iw)
	local i = line_at(doc, dcar)
	local y = (i - 1) * lh
	local sy = get_scroll(u, el.id)
	if y < sy then
		sy = y
	end
	if y + lh > sy + ih then
		sy = y + lh - ih
	end
	sy = U.clamp(sy, 0, max(0, doc.n * lh - ih))
	put_scroll(u, el.id, sy)
	local x = caret_x_in(doc, size, i, dcar)
	return { ix + x, iy + y - sy, 2, lh }
end

-- ---------------------------------------------------------------- undo history
--
-- Per field, on the Luau side: full-value snapshots { value, caret, anchor,
-- seq }, at most MAX_UNDO per field and HIST_BUDGET bytes per app. Over
-- budget, the oldest snapshots of the whole app go first (the field being
-- edited keeps at least its newest one), then other fields' redo steps.
-- Histories of elements that left the view are dropped (F.prune_history,
-- called after each draw).

local HIST_BUDGET = 4 * 1024 * 1024
F.limits = { field = MAX_FIELD, area = MAX_AREA, undo_steps = MAX_UNDO, undo_bytes = HIST_BUDGET }

local function snap_bytes(list)
	local n = 0
	for _, sn in list do
		n += #sn[1]
	end
	return n
end

local function history(app, id)
	local h = app._hist[id]
	if h == nil then
		h = { undo = {}, redo = {} }
		app._hist[id] = h
	end
	return h
end

local function snap(app, s, caret, anchor)
	app._hist_seq += 1
	return { s, caret, anchor, app._hist_seq }
end

local function trim_history(app, keep)
	while app._hist_bytes > HIST_BUDGET do
		-- the field whose oldest undo step is the oldest of all
		local victim, oldest = nil, math.huge
		for _, h in app._hist do
			local first = h.undo[1]
			if first ~= nil and (h ~= keep or #h.undo > 1) and first[4] < oldest then
				victim, oldest = h, first[4]
			end
		end
		if victim == nil then
			break
		end
		app._hist_bytes -= #table.remove(victim.undo, 1)[1]
	end
	if app._hist_bytes > HIST_BUDGET then
		for _, h in app._hist do
			if h ~= keep and #h.redo > 0 then
				app._hist_bytes -= snap_bytes(h.redo)
				h.redo = {}
			end
		end
	end
end

-- Record `s` (with its caret and anchor) as an undo step of field `id`.
local function push_undo(app, h, s, caret, anchor)
	local list = h.undo
	list[#list + 1] = snap(app, s, caret, anchor)
	app._hist_bytes += #s
	if #list > MAX_UNDO then
		app._hist_bytes -= #table.remove(list, 1)[1]
	end
	if #h.redo > 0 then
		app._hist_bytes -= snap_bytes(h.redo)
		h.redo = {}
	end
	trim_history(app, h)
end

-- Drop the histories of fields that are not in the laid-out tree any more.
function F.prune_history(app, byid)
	for id, h in app._hist do
		if byid[id] == nil then
			app._hist_bytes -= snap_bytes(h.undo) + snap_bytes(h.redo)
			app._hist[id] = nil
		end
	end
end

-- ---------------------------------------------------------------- applying edits

-- Make (ns, caret, anchor) the field's value and edit state. `kind`:
-- "type" (coalesced into one undo step), "edit", "move", "vertical", "undo".
local function apply(el, app, u, s, ed0, ns, caret, anchor, kind)
	local changed = ns ~= s
	if changed and (el.readonly or L.disabled(el)) then
		return true
	end
	local ed = { id = el.id, caret = caret, anchor = anchor or caret, sx = ed0.sx }
	if kind == "vertical" then
		ed.cx = ed0.cx
	end
	local h = history(app, el.id)
	if changed and kind ~= "undo" then
		if not (kind == "type" and h.typing) then
			push_undo(app, h, s, ed0.caret, ed0.anchor)
		elseif #h.redo > 0 then
			app._hist_bytes -= snap_bytes(h.redo)
			h.redo = {}
		end
	end
	h.typing = kind == "type"
	u.edit = ed
	u.ime = nil
	u.blink = app:now()
	local rect = follow(el, u, ns, ed, nil)
	app:ui_set(u)
	if changed then
		app:call(el.on_change, ns)
	end
	app:host_caret(rect)
	return true
end

-- Bytes a field may hold: its max_length, never more than the library's limit.
function F.limit_of(el)
	local own = tonumber(el.max_length)
	local lib = if el.type == "textfield" then MAX_FIELD else MAX_AREA
	if own ~= nil and own < lib then
		return max(0, floor(own))
	end
	return lib
end

local function insert(el, app, ins, kind)
	local u = app:ui_get()
	local s = value_of(el)
	local ed = edit_of(u, el, s)
	if el.type == "textfield" then
		ins = (string.gsub(ins, "[\r\n]+", " "))
	else
		ins = (string.gsub(ins, "\r\n?", "\n"))
	end
	local a, b = sel_range(ed)
	local room = F.limit_of(el) - (#s - (b - a))
	if room <= 0 then
		return true
	end
	if #ins > room then
		ins = string.sub(ins, 1, U.utf8_cut(ins, room))
	end
	if ins == "" and a == b then
		return true
	end
	local ns = string.sub(s, 1, a) .. ins .. string.sub(s, b + 1)
	local typing = kind == "type" and #ins <= 4 and not string.find(ins, "%s")
	return apply(el, app, u, s, ed, ns, a + #ins, a + #ins, if typing then "type" else "edit")
end
F.insert = insert

-- Replace the whole value as an agent's `set` does: through the same rules
-- as typing (read-only refused, line breaks of a one-line field turned into
-- spaces, max_length and the size limit), as one undoable step. Returns the
-- value applied.
function F.set_value(el, app, v)
	-- read-only by the page refuses; read-only only because too long accepts a replacement
	if el.readonly and not (el._full and not el._own_readonly) then
		error(el.type .. " \"" .. el.id .. "\" is read-only", 0)
	end
	local s = if v == nil then "" else tostring(v)
	if el.type == "textfield" then
		s = (string.gsub(s, "[\r\n]+", " "))
	else
		s = (string.gsub(s, "\r\n?", "\n"))
	end
	local limit = F.limit_of(el)
	if #s > limit then
		s = string.sub(s, 1, U.utf8_cut(s, limit))
	end
	local old = value_of(el)
	if s == old and not el._full then
		return s
	end
	local u = app:ui_get()
	if el._full then
		-- what is shown is only the beginning of the real value: no undo step back to it
		app:call(el.on_change, s)
	elseif u.focus == el.id then
		apply(el, app, u, old, edit_of(u, el, old), s, #s, #s, "edit")
	else
		local ed = u.edit
		local mine = type(ed) == "table" and ed.id == el.id
		push_undo(app, history(app, el.id), old, if mine then ed.caret else #old, if mine then ed.anchor else #old)
		history(app, el.id).typing = false
		app:call(el.on_change, s)
	end
	return s
end

local function undo(el, app, redo)
	local u = app:ui_get()
	local s = value_of(el)
	local ed = edit_of(u, el, s)
	local h = history(app, el.id)
	local from, to = h.undo, h.redo
	if redo then
		from, to = h.redo, h.undo
	end
	local sn = table.remove(from)
	if sn == nil then
		return true
	end
	to[#to + 1] = snap(app, s, ed.caret, ed.anchor)
	local ns = sn[1]
	app._hist_bytes += #s - #ns
	trim_history(app, h)
	return apply(el, app, u, s, ed, ns, min(sn[2], #ns), min(sn[3], #ns), "undo")
end

-- Up / down / page keys in a text area: same x on another visual line.
local function vertical(el, s, ed, base)
	local size = el._size
	local lh = U.line_height(size)
	local _, _, iw, ih = area_geom(el)
	local doc = doc_of(s, size, iw)
	local i = line_at(doc, ed.caret)
	local x = tonumber(ed.cx) or caret_x_in(doc, size, i, ed.caret)
	local step = if base == "pageup" or base == "pagedown" then max(1, floor(ih / lh) - 1) else 1
	local j = i + (if base == "up" or base == "pageup" then -step else step)
	if j < 1 then
		return 0, x
	end
	if j > doc.n then
		return #s, x
	end
	local str, f = line_str(doc, j)
	return f + thit(str, size, x), x
end

local function line_edge(el, s, p, to_end)
	local _, _, iw = area_geom(el)
	local doc = doc_of(s, el._size, iw)
	local str, f = line_str(doc, line_at(doc, p))
	if to_end then
		return f + #str
	end
	return f
end

-- Key handling shared by fields and areas. Returns true when handled.
local function key(el, app, k, base, ctrl, shift, alt)
	local multiline = el.type == "textarea"
	local u = app:ui_get()
	local s = value_of(el)
	local ed = edit_of(u, el, s)
	local a, b = sel_range(ed)
	local c = ed.caret
	local function move(nc, kind)
		return apply(el, app, u, s, ed, s, nc, if shift then ed.anchor else nc, kind or "move")
	end
	if base == "left" or base == "right" then
		if not shift and a ~= b then
			return move(if base == "left" then a else b)
		end
		if base == "left" then
			return move(if ctrl then tpw(s, c) else tprev(s, c))
		end
		return move(if ctrl then tnw(s, c) else tnext(s, c))
	elseif base == "home" or base == "end" then
		if multiline and not ctrl then
			return move(line_edge(el, s, c, base == "end"))
		end
		return move(if base == "home" then 0 else #s)
	elseif multiline and (base == "up" or base == "down" or base == "pageup" or base == "pagedown") then
		local nc, cx = vertical(el, s, ed, base)
		ed.cx = cx
		return move(nc, "vertical")
	elseif base == "backspace" then
		if a ~= b then
			return apply(el, app, u, s, ed, string.sub(s, 1, a) .. string.sub(s, b + 1), a, a, "edit")
		end
		if c == 0 then
			return true
		end
		local p = if ctrl then tpw(s, c) else tprev(s, c)
		return apply(el, app, u, s, ed, string.sub(s, 1, p) .. string.sub(s, c + 1), p, p, "edit")
	elseif base == "delete" then
		if a ~= b then
			return apply(el, app, u, s, ed, string.sub(s, 1, a) .. string.sub(s, b + 1), a, a, "edit")
		end
		if c >= #s then
			return true
		end
		local q = if ctrl then tnw(s, c) else tnext(s, c)
		return apply(el, app, u, s, ed, string.sub(s, 1, c) .. string.sub(s, q + 1), c, c, "edit")
	elseif ctrl and base == "a" then
		return apply(el, app, u, s, ed, s, #s, 0, "move")
	elseif ctrl and (base == "c" or base == "x") then
		if a ~= b and not el.password then
			app:copy(string.sub(s, a + 1, b))
			if base == "x" then
				return apply(el, app, u, s, ed, string.sub(s, 1, a) .. string.sub(s, b + 1), a, a, "edit")
			end
		end
		return true
	elseif ctrl and base == "v" then
		app:paste_into(el)
		return true
	elseif ctrl and base == "z" then
		return undo(el, app, shift)
	elseif ctrl and base == "y" then
		return undo(el, app, true)
	elseif base == "enter" then
		if multiline and not ctrl then
			return insert(el, app, "\n", "edit")
		end
		app:call(el.on_submit, s)
		return true
	elseif base == "space" and not ctrl and not alt then
		-- hosts normally send typed spaces as text; accept the key too
		return insert(el, app, " ", "edit")
	end
	return false
end

-- Byte position in the value for an app-local point.
local function position_at(el, u, s, x, y)
	local size = el._size
	if el.type == "textfield" then
		local ed = u.edit
		local sx = if type(ed) == "table" and ed.id == el.id then tonumber(ed.sx) or 0 else 0
		local disp = if el.password then U.mask(s) else s
		local p = thit(disp, size, x - (el.x + PAD) + sx)
		if el.password then
			p = U.unmask_pos(s, p)
		end
		return p
	end
	local ix, iy, iw = area_geom(el)
	local doc = doc_of(s, size, iw)
	local lh = U.line_height(size)
	local i = U.clamp(floor((y - iy + get_scroll(u, el.id)) / lh) + 1, 1, doc.n)
	local str, f = line_str(doc, i)
	return f + thit(str, size, x - ix)
end

local function field_down(el, app, x, y, dr)
	local u = app:ui_get()
	local s = value_of(el)
	local ed = edit_of(u, el, s)
	local p = position_at(el, u, s, x, y)
	local caret, anchor = p, p
	if dr.clicks >= 3 then
		caret, anchor = #s, 0
	elseif dr.clicks == 2 and not el.password then
		local e = tnw(s, p)
		caret, anchor = e, tpw(s, e)
	end
	dr.kind = "text"
	dr.anchor = anchor
	return apply(el, app, u, s, ed, s, caret, anchor, "move") and "captured"
end

local function field_drag(el, app, x, y, dr)
	local u = app:ui_get()
	local s = value_of(el)
	local ed = edit_of(u, el, s)
	-- dragging past the edges scrolls by moving the caret there
	local p = position_at(el, u, s, x, y)
	if p ~= ed.caret or dr.anchor ~= ed.anchor then
		apply(el, app, u, s, ed, s, p, U.clamp(dr.anchor or p, 0, #s), "move")
	end
end

local function on_focus(el, app, u, how)
	local s = value_of(el)
	-- Tab selects everything (to retype); a click places the caret afterwards
	if how == "key" and el.type == "textfield" then
		u.edit = { id = el.id, caret = #s, anchor = 0 }
	else
		u.edit = { id = el.id, caret = #s, anchor = #s }
	end
	u.ime = nil
	u.blink = app:now()
	return follow(el, u, s, u.edit, nil)
end

local function caret_rect(el, app, u)
	local s = value_of(el)
	local ed = edit_of(u, el, s)
	local uu = table.clone(u)
	return follow(el, uu, s, ed, u.ime)
end

local function on_ime(el, app, pre)
	local u = app:ui_get()
	-- the UI cell is readable by agents: a password's preedit is kept only
	-- as dots (the field shows dots anyway; the commit arrives as text)
	if el.password and pre ~= "" then
		pre = U.mask(pre)
	end
	u.ime = if pre ~= "" then pre else nil
	u.blink = app:now()
	local s = value_of(el)
	local ed = edit_of(u, el, s)
	local rect = follow(el, u, s, ed, u.ime)
	u.edit = ed
	app:ui_set(u)
	app:host_caret(rect)
	return true
end

-- ---------------------------------------------------------------- drawing

-- One line of `s` scrolled left by `sx`, showing only the glyphs that fit in `w`.
local function draw_span(d, s, size, x, y, w, sx, c)
	if sx <= 0 and U.width(s, size) <= w then
		d:text(x, y, size, c, s)
		return
	end
	local p = thit(s, size, sx)
	if tcaret(s, size, p) < sx - 0.5 then
		p = tnext(s, p)
	end
	local e = thit(s, size, sx + w)
	while e > p and tcaret(s, size, e) > sx + w + 0.5 do
		e = tprev(s, e)
	end
	if e > p then
		d:text(x + tcaret(s, size, p) - sx, y, size, c, string.sub(s, p + 1, e))
	end
end

local function draw_box(el, dc, focused)
	local th, d = dc.th, dc.d
	local r = th.radius_small
	d:rrect(el.x, el.y, el.w, el.h, r, if dc.dim or el.readonly then th.surface else th.surface2)
	local bc = if focused then th.focus elseif dc.hover == el.id then th.border_strong else th.border
	if dc.dim then
		bc = Theme.dim(th, bc)
	end
	d:stroke(el.x + 0.5, el.y + 0.5, el.w - 1, el.h - 1, r, if focused then 2 else 1, bc)
end

local ZERO = { caret = 0, anchor = 0 }

local function draw_field(el, dc)
	local th, d = dc.th, dc.d
	local focused = dc.focus == el.id
	draw_box(el, dc, focused)
	local s = value_of(el)
	local ed = nil
	if focused then
		ed = edit_of(dc.u, el, s)
	end
	local ime = if focused then dc.ime else nil
	local size = el._size
	local lh = U.line_height(size)
	local tx, ty, tw = el.x + PAD, el.y + (el.h - lh) / 2, max(1, el.w - 2 * PAD)
	local col = if dc.dim then Theme.dim(th, th.text) else th.text
	if s == "" and (ime == nil or ime == "") then
		if el.placeholder then
			d:text(tx, ty, size, th.text_faint, U.truncate(el.placeholder, size, tw))
		end
		if ed and dc.caret_on then
			d:rect(tx, ty, 1.5, lh, th.accent)
		end
		return
	end
	local disp, dcar, da, db, i0, i1 = display(el, s, ed or ZERO, ime)
	local sx = (ed and tonumber(ed.sx)) or 0
	if ed and da ~= db then
		local x0 = max(0, tcaret(disp, size, da) - sx)
		local x1 = min(tw, tcaret(disp, size, db) - sx)
		if x1 > x0 then
			d:rect(tx + x0, ty, x1 - x0, lh, th.selection)
		end
	end
	draw_span(d, disp, size, tx, ty, tw, sx, col)
	if i0 then
		local x0 = max(0, tcaret(disp, size, i0) - sx)
		local x1 = min(tw, tcaret(disp, size, i1) - sx)
		d:rect(tx + x0, ty + lh - 2, max(1, x1 - x0), 1.5, th.accent)
	end
	if ed and dc.caret_on then
		local x = tcaret(disp, size, dcar) - sx
		if x >= -1 and x <= tw + 1 then
			d:rect(tx + x, ty, 1.5, lh, th.accent)
		end
	end
end

local function draw_area(el, dc)
	local th, d = dc.th, dc.d
	local focused = dc.focus == el.id
	draw_box(el, dc, focused)
	local s = value_of(el)
	local ed = nil
	if focused then
		ed = edit_of(dc.u, el, s)
	end
	local ime = if focused then dc.ime else nil
	local size = el._size
	local lh = U.line_height(size)
	local ix, iy, iw, ih = area_geom(el)
	if s == "" and (ime == nil or ime == "") then
		if el.placeholder then
			d:textbox(ix, iy, iw, ih, size, th.text_faint, el.placeholder)
		end
		if ed and dc.caret_on then
			d:rect(ix, iy, 1.5, lh, th.accent)
		end
		el._ch, el._vh, el._sy = lh, ih, 0
		return
	end
	local disp, dcar, da, db, i0, i1 = display(el, s, ed or ZERO, ime)
	local doc = doc_of(disp, size, iw)
	local nl = doc.n
	local total = nl * lh
	local sy = U.clamp(get_scroll(dc.u, el.id), 0, max(0, total - ih))
	el._ch, el._vh, el._sy = total, ih, sy
	local clip = total > ih or sy > 0
	if clip then
		L.push_clip(dc, el.x + 2, iy, el.w - 4, ih)
	end
	local col = if dc.dim then Theme.dim(th, th.text) else th.text
	local first = max(1, floor(sy / lh) + 1)
	local last = min(nl, floor((sy + ih) / lh) + 1)
	for i = first, last do
		local ly = iy + (i - 1) * lh - sy
		local str, f, t = line_str(doc, i)
		if ed and da ~= db and da <= t and db >= f then
			local s0, s1 = max(da, f), min(db, f + #str)
			local x0 = tcaret(str, size, U.clamp(s0 - f, 0, #str))
			local x1 = tcaret(str, size, U.clamp(s1 - f, 0, #str))
			-- a selected line break shows as a sliver past the line end
			if db > f + #str and i < nl then
				x1 += 5
			end
			if x1 > x0 then
				d:rect(ix + x0, ly, x1 - x0, lh, th.selection)
			end
		end
		if str ~= "" then
			d:text(ix, ly, size, col, str)
		end
		if i0 and i0 <= f + #str and i1 >= f then
			local x0 = tcaret(str, size, U.clamp(i0 - f, 0, #str))
			local x1 = tcaret(str, size, U.clamp(i1 - f, 0, #str))
			d:rect(ix + x0, ly + lh - 2, max(1, x1 - x0), 1.5, th.accent)
		end
	end
	if ed and dc.caret_on then
		local i = line_at(doc, dcar)
		local x = caret_x_in(doc, size, i, dcar)
		d:rect(ix + x, iy + (i - 1) * lh - sy, 1.5, lh, th.accent)
	end
	if clip then
		L.pop_clip(dc)
	end
	if total > ih then
		local track = el.h - 8
		local th_h = max(20, track * ih / total)
		local ty = el.y + 4 + (track - th_h) * sy / (total - ih)
		d:rrect(el.x + el.w - 7, ty, 4, th_h, 2, th.thumb)
	end
	if el._full then
		local fs = th.font_small
		local note = string.format("read-only: the first %d of %d bytes", #s, el._full)
		local nw = min(U.width(note, fs) + 12, el.w - 16)
		local nh = U.line_height(fs) + 4
		d:rrect(el.x + el.w - nw - 10, el.y + el.h - nh - 4, nw, nh, 4, th.surface3)
		d:text(el.x + el.w - nw - 4, el.y + el.h - nh - 2, fs, th.text_dim, U.truncate(note, fs, nw - 12))
	end
end

-- ---------------------------------------------------------------- types

local function check_field(el)
	L.need(el, "value", "string", "number")
	L.need(el, "placeholder", "string")
	L.need(el, "max_length", "number")
	L.need(el, "size", "number")
	L.need(el, "rows", "number")
	L.need_fns(el, "on_change", "on_submit", "on_focus")
end

-- A value longer than the size limit (set by the page, not typed) is shown
-- from its beginning and cannot be edited, so no keystroke ever cuts it;
-- an agent's `set` may still replace it with a value that fits.
local function clamp_value(el)
	local s = el.value
	local limit = if el.type == "textfield" then MAX_FIELD else MAX_AREA
	if type(s) == "string" and #s > limit and el._full == nil then
		el._full = #s
		el._own_readonly = el.readonly == true
		el.value = string.sub(s, 1, U.utf8_cut(s, limit))
		el.readonly = true
	end
end

local function describe_field(el)
	local n = {
		value = if el.password then nil else value_of(el),
		placeholder = el.placeholder,
		readonly = el.readonly == true,
	}
	if el.type == "textfield" then
		n.password = el.password == true
	end
	if el._full then
		n.length = el._full
		n.truncated = true
	end
	return n
end

T.textfield = {
	focusable = true,
	fill = true,
	own_focus = true,
	text_input = true,
	check = check_field,
	measure = function(el, aw, ah, cx)
		local th = cx.theme
		clamp_value(el)
		el._size = el.size or th.font
		local w = if aw < huge then min(aw, 220) else 220
		return w, th.control_h
	end,
	draw = draw_field,
	down = field_down,
	drag = field_drag,
	key = key,
	text = function(el, app, s)
		return insert(el, app, s, "type")
	end,
	paste = function(el, app, s)
		return insert(el, app, s, "edit")
	end,
	ime = on_ime,
	on_focus = on_focus,
	caret = caret_rect,
	describe = describe_field,
}

T.textarea = {
	focusable = true,
	fill = true,
	own_focus = true,
	text_input = true,
	shrink_y = true,
	scroller = true,
	check = check_field,
	measure = function(el, aw, ah, cx)
		local th = cx.theme
		clamp_value(el)
		local size = el.size or th.font
		el._size = size
		local w = if aw < huge then aw else 320
		return w, (el.rows or 4) * U.line_height(size) + 2 * PADY
	end,
	arrange = function(el, x, y, w, h, cx)
		-- content extent for wheel scrolling before the first draw
		local s = value_of(el)
		local doc = doc_of(s, el._size, max(1, w - 2 * PAD))
		local lh = U.line_height(el._size)
		local ih = max(1, h - 2 * PADY)
		el._ch, el._vh = doc.n * lh, ih
		el._sy = U.clamp(get_scroll(cx.u, el.id), 0, max(0, el._ch - ih))
	end,
	draw = draw_area,
	down = field_down,
	drag = field_drag,
	key = key,
	text = function(el, app, s)
		return insert(el, app, s, "type")
	end,
	paste = function(el, app, s)
		return insert(el, app, s, "edit")
	end,
	ime = on_ime,
	on_focus = on_focus,
	caret = caret_rect,
	describe = describe_field,
}

return F
