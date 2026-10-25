-- editor: a text editor element for import("ui") apps that edits text in
-- any language.
--
--   * carets, selection and deletion move by grapheme cluster (text.next /
--     text.prev), words by text.next_word / text.prev_word;
--   * each paragraph has its own direction (text.is_rtl): right-to-left
--     paragraphs are right-aligned and drawn, hit-tested and highlighted in
--     their direction (d:text dir, text.caret / text.hit / text.selection
--     with dir); Left / Right move visually, also across mixed-direction lines;
--   * the IME preedit is shown inline at the caret, underlined;
--   * word wrap on (text.layout per paragraph) or off (horizontal scrolling);
--   * line numbers, the caret's paragraph highlighted, a red dot beside
--     paragraphs with characters no font can show (text.missing);
--   * find highlights, undo / redo as edits (not snapshots).
--
-- The element type "mleditor" is added to the UI library's type registry
-- (ui/layout), so the app routes focus, keys, text, IME, paste, pointer and
-- wheel events to it like to the built-in controls. Positions are byte
-- offsets 0..#s (bytes before the caret). Paragraphs end at "\n".
--
-- State: the text is the element's `value` (the page keeps it in its state
-- cell and gets edits through on_change; an optional `source()` returns the
-- current text, read by handlers so an element built from older inputs
-- never edits an older text); caret, anchor, desired column, caret
-- affinity and horizontal scroll live in the app's UI cell as
-- u["mle:<id>"], the vertical scroll in u.scroll[id] like other scrollers.
-- The undo history is kept here, by id, so two apps showing the same
-- document under the same id share it.
--
-- Caret affinity: where a wrapped line breaks with no space (Chinese,
-- Japanese, Thai) one offset is both the end of a visual line and the start
-- of the next. `up` in the edit state says the caret is drawn at the end of
-- the earlier line (after End, or a click past a line's end); any other
-- move or edit clears it.

local U = import("ui/util")
local L = import("ui/layout")

local T = L.types
local max, min, floor, abs = math.max, math.min, math.floor, math.abs
local huge = math.huge
local find, sub, gsub = string.find, string.sub, string.gsub
local tnext, tprev, tnw, tpw = text.next, text.prev, text.next_word, text.prev_word
local tcaret, thit, tsel, twidth = text.caret, text.hit, text.selection, text.width
local clock = U.clock

local E = {}

-- Largest document the editor edits (bytes). The text module looks at most
-- 256 KB per call, so a single paragraph of this size is still whole.
E.MAX = 256 * 1024

local PADX = 14 -- text inset from the gutter
local PADY = 10
local SB = 10 -- scrollbar lane
local GUTTER_PAD = 12
local LINE_GAP = 1.2 -- line height over the font's own
local WIDE = 2 -- no glyph is wider than WIDE em per UTF-8 byte
local MIXED_MAX = 2000 -- longest line (bytes) moved through visually stop by stop
local UNDO_STEPS = 1000
local UNDO_BYTES = 8 * 1024 * 1024
local TYPE_PAUSE = 1.5 -- seconds of typing merged into one undo step

-- ---------------------------------------------------------------- small helpers

local function clamp(v, lo, hi)
	if v < lo then
		return lo
	end
	if v > hi then
		return hi
	end
	return v
end

-- A number from a prop that may hold anything (NaN included: it passes
-- every comparison, so clamp would let it through).
local function num(v, default)
	local n = tonumber(v)
	if n == nil or n ~= n then
		return default
	end
	return n
end
E.num = num

-- Two-generation caches: a lookup moves an entry into the current
-- generation; when the current one has seen `budget` bytes of keys it
-- becomes the old one, so memory stays bounded and live entries survive.
local function cache(budget)
	return { cur = {}, old = {}, bytes = 0, budget = budget }
end

local function cget(c, k)
	local v = c.cur[k]
	if v ~= nil then
		return v
	end
	v = c.old[k]
	if v ~= nil then
		c.cur[k] = v
		c.bytes += #k
	end
	return v
end

local function cput(c, k, v)
	c.cur[k] = v
	c.bytes += #k
	if c.bytes > c.budget then
		c.old, c.cur, c.bytes = c.cur, {}, 0
	end
end

-- The same keyed by (group, text), one budget for all groups (font sizes,
-- wrap widths), so old zoom levels do not keep their entries forever.
local function cput2(c, g, k, v)
	local m = c.cur[g]
	if m == nil then
		m = {}
		c.cur[g] = m
	end
	m[k] = v
	c.bytes += #k
	if c.bytes > c.budget then
		c.old, c.cur, c.bytes = c.cur, {}, 0
	end
end

local function cget2(c, g, k)
	local m = c.cur[g]
	local v = m and m[k]
	if v ~= nil then
		return v
	end
	m = c.old[g]
	v = m and m[k]
	if v ~= nil then
		cput2(c, g, k, v)
	end
	return v
end

local function line_h(size)
	return floor(U.line_height(size) * LINE_GAP + 0.5)
end

local function gutter_size(size)
	return max(11, floor(size * 0.72 + 0.5))
end

function E.normalize(s)
	if find(s, "\r", 1, true) then
		s = gsub(s, "\r\n?", "\n")
	end
	return s
end

-- ---------------------------------------------------------------- paragraphs
--
-- A paragraph's facts depend only on its text, so they are cached by it:
-- { rtl, missing glyphs, graphemes, words }. ASCII paragraphs need no native
-- call; only paragraphs with bytes that can start a right-to-left character
-- (or a right-to-left mark) ask text.is_rtl.

local RTL_HINTS = {
	"[\xD6-\xDF]", -- Hebrew, Arabic, Syriac, Thaana, NKo (U+0590-07FF)
	"\xE0[\xA0-\xA3]", -- Samaritan, Mandaic, Arabic Extended (U+0800-08FF)
	"\xEF[\xAC-\xB7\xB9-\xBB]", -- Hebrew and Arabic presentation forms
	"\xE2\x80[\x8F\xAB\xAE]", -- RLM, RLE, RLO
	"\xE2\x81\xA7", -- RLI
	"\xF0\x90[\xA0-\xBF]", -- U+10800-10FFF
	"\xF0\x9E[\xA0-\xBF]", -- U+1E800-1EFFF
}

local function maybe_rtl(ps)
	for _, p in RTL_HINTS do
		if find(ps, p) then
			return true
		end
	end
	return false
end
E.maybe_rtl = maybe_rtl

-- Characters counted one word each (Han, kana, compatibility ideographs),
-- and CJK punctuation, which separates words like a space.
local CJK = {
	"\xE3[\x81-\x83][\x80-\xBF]", -- kana
	"\xE3[\x90-\xBF][\x80-\xBF]", -- CJK extension A
	"[\xE4-\xE9][\x80-\xBF][\x80-\xBF]", -- CJK unified ideographs
	"\xEF[\xA4-\xAB][\x80-\xBF]", -- compatibility ideographs
	"\xF0[\xA0-\xAF][\x80-\xBF][\x80-\xBF]", -- extensions B and later
}
local CJK_PUNCT = { "\xE3\x80[\x80-\xBF]", "\xEF\xBC[\x80-\x8F\x9A-\xA0]" }

-- Words: runs separated by white space, each Han or kana character one word
-- (as word processors count East Asian text). Scripts written without
-- spaces between words (Thai) count by phrase.
local function words_of(ps, nonascii)
	if not nonascii then
		return select(2, gsub(ps, "%S+", ""))
	end
	local n, t = 0, ps
	for _, p in CJK do
		local c
		t, c = gsub(t, p, " ")
		n += c
	end
	for _, p in CJK_PUNCT do
		t = gsub(t, p, " ")
	end
	return n + select(2, gsub(t, "%S+", ""))
end

-- A long paragraph is counted in pieces cut after white space or CJK
-- punctuation (never inside a word), each cached by its text: the cuts do
-- not move when text is typed elsewhere, so an edit recounts one piece.
local PIECE = 4096
local piece_words = cache(2 * 1024 * 1024)

local function piece_end(ps, i)
	local from = i + PIECE
	if from >= #ps then
		return #ps
	end
	local _, e = find(ps, "%s", from)
	if e and e - from < PIECE then
		return e
	end
	_, e = find(ps, "\xE3\x80[\x80-\x82]", from)
	if e and e - from < PIECE then
		return e
	end
	return U.utf8_cut(ps, from + PIECE)
end

local function count_words(ps, nonascii)
	if #ps <= 2 * PIECE then
		return words_of(ps, nonascii)
	end
	local n, i = 0, 0
	while i < #ps do
		local e = piece_end(ps, i)
		local piece = sub(ps, i + 1, e)
		local v = piece_words.cur[piece] or piece_words.old[piece]
		if v == nil then
			v = words_of(piece, find(piece, "[\128-\255]") ~= nil)
		end
		piece_words.cur[piece] = v
		piece_words.bytes += #piece
		n += v
		i = e
	end
	if piece_words.bytes > piece_words.budget then
		piece_words.old, piece_words.cur, piece_words.bytes = piece_words.cur, {}, 0
	end
	return n
end
E.count_words = count_words

local infos = cache(6 * 1024 * 1024)

local function info(ps)
	local v = cget(infos, ps)
	if v ~= nil then
		return v
	end
	local nonascii = find(ps, "[\128-\255]") ~= nil
	local special = nonascii or find(ps, "[\1-\8\11-\31\127]") ~= nil or find(ps, "\0", 1, true) ~= nil
	local rtl = nonascii and maybe_rtl(ps) and text.is_rtl(ps)
	local miss = if special then text.missing(ps) else 0
	local gr = if special then text.graphemes(ps) else #ps
	v = { rtl == true, miss, gr, count_words(ps, nonascii) }
	cput(infos, ps, v)
	return v
end
E.info = info

-- The paragraphs of a document: { s, n, po = start offsets, pe = end
-- offsets (exclusive), ps = paragraph strings, inf = their facts }.
-- Kept for the last few documents (the text, the text with an IME preedit,
-- the previous text), so an edit and the draw that shows it share one.
local splits = {}
local split_order = {}
local SPLITS_KEPT = 4

local function split_of(s)
	local sp = splits[s]
	if sp ~= nil then
		return sp
	end
	local po, pe, pss, inf = {}, {}, {}, {}
	local i, k, len = 1, 0, #s
	while true do
		local j = find(s, "\n", i, true)
		local e = if j then j - 1 else len
		k += 1
		local ps = sub(s, i, e)
		po[k], pe[k], pss[k] = i - 1, e, ps
		inf[k] = info(ps)
		if j == nil then
			break
		end
		i = j + 1
	end
	sp = { s = s, n = k, po = po, pe = pe, ps = pss, inf = inf }
	splits[s] = sp
	split_order[#split_order + 1] = s
	while #split_order > SPLITS_KEPT do
		splits[table.remove(split_order, 1)] = nil
	end
	return sp
end
E.split_of = split_of

-- Paragraph index holding byte position p.
local function para_at(sp, p)
	local po = sp.po
	local lo, hi = 1, sp.n
	while lo < hi do
		local mid = (lo + hi + 1) // 2
		if po[mid] <= p then
			lo = mid
		else
			hi = mid - 1
		end
	end
	return lo
end
E.para_at = para_at

-- Totals for the status bar, cached with the split.
function E.stats(s)
	local sp = split_of(s)
	local st = sp.stats
	if st then
		return st
	end
	local gr, words, miss, rtl, missing_paras = 0, 0, 0, 0, 0
	for k = 1, sp.n do
		local f = sp.inf[k]
		if f[1] then
			rtl += 1
		end
		miss += f[2]
		if f[2] > 0 then
			missing_paras += 1
		end
		gr += f[3]
		words += f[4]
	end
	-- line breaks are graphemes too
	gr += sp.n - 1
	st = { bytes = #s, paragraphs = sp.n, graphemes = gr, words = words, missing = miss, missing_paragraphs = missing_paras, rtl_paragraphs = rtl }
	sp.stats = st
	return st
end

-- Paragraph (1-based), column in graphemes (1-based) and direction at byte p.
function E.caret_info(s, p)
	local sp = split_of(s)
	-- a caret from an older text can fall inside a character of this one
	p = U.utf8_cut(s, clamp(floor(num(p, 0)), 0, #s))
	local k = para_at(sp, p)
	local off = p - sp.po[k]
	local ps = sp.ps[k]
	local col = 1
	if off > 0 then
		local pre = sub(ps, 1, off)
		col = 1 + (if find(pre, "[\128-\255]") then text.graphemes(pre) else #pre)
	end
	return { paragraph = k, column = col, rtl = sp.inf[k][1] }
end

-- ---------------------------------------------------------------- layout

local widths = cache(6 * 1024 * 1024) -- size -> text -> single-line width
local lays = cache(6 * 1024 * 1024) -- "size|width" -> text -> wrapped lines

local function pwidth(ps, size)
	if ps == "" then
		return 0
	end
	local v = cget2(widths, size, ps)
	if v == nil then
		v = twidth(ps, size)
		cput2(widths, size, ps, v)
	end
	return v
end

-- Wrapped lines of a paragraph wider than w: false for one line, else the
-- text.layout lines { {from, to, width} } in paragraph-local offsets.
local function para_lines(key, ps, size, w)
	local v = cget2(lays, key, ps)
	if v ~= nil then
		return v
	end
	if pwidth(ps, size) <= w then
		v = false
	else
		local lines = text.layout(ps, size, w).lines
		v = if #lines > 1 then lines else false
	end
	cput2(lays, key, ps, v)
	return v
end

-- A document laid out at a size and width (0: no wrapping):
-- { sp, n = visual lines, pl = first line of each paragraph, lay = wrapped
--   paragraphs' lines, maxw = widest line when not wrapping }.
local docs = {}
local doc_order = {}
local DOCS_KEPT = 6

local function doc_of(sp, size, w)
	w = if w > 0 then max(1, floor(w)) else 0
	local key = size .. "|" .. w
	local per = docs[key]
	if per == nil then
		per = {}
		docs[key] = per
	end
	local d = per[sp.s]
	if d ~= nil then
		return d
	end
	local pl, lay = {}, {}
	local line, maxw = 1, 0
	local short = if w > 0 then w / (size * WIDE) else 0
	local pss = sp.ps
	for k = 1, sp.n do
		pl[k] = line
		local ps = pss[k]
		if w > 0 then
			if #ps > short then
				local v = para_lines(key, ps, size, w)
				if v then
					lay[k] = v
					line += #v
				else
					line += 1
				end
			else
				line += 1
			end
		else
			line += 1
			local pw = pwidth(ps, size)
			if pw > maxw then
				maxw = pw
			end
		end
	end
	d = { sp = sp, n = line - 1, pl = pl, lay = lay, maxw = maxw, size = size, w = w }
	per[sp.s] = d
	doc_order[#doc_order + 1] = { key, sp.s }
	while #doc_order > DOCS_KEPT do
		local o = table.remove(doc_order, 1)
		local m = docs[o[1]]
		if m then
			m[o[2]] = nil
		end
	end
	return d
end

-- Paragraph holding visual line i.
local function para_of_line(doc, i)
	local pl = doc.pl
	local lo, hi = 1, doc.sp.n
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

-- Visual line i: its text, byte range [f, t) in the document, paragraph, and
-- its layout line (nil for an unwrapped paragraph).
local function line_of(doc, i)
	local k = para_of_line(doc, i)
	local sp = doc.sp
	local base = sp.po[k]
	local v = doc.lay[k]
	if v then
		local ln = v[i - doc.pl[k] + 1]
		return sub(sp.ps[k], ln.from + 1, ln.to), base + ln.from, base + ln.to, k, ln
	end
	return sp.ps[k], base, sp.pe[k], k, nil
end

-- Visual line holding byte position p: the last line starting at or before
-- it; with `up` (upstream affinity), the earlier line when p is both that
-- line's end and the next one's start.
local function line_at(doc, p, up)
	local sp = doc.sp
	local k = para_at(sp, p)
	local v = doc.lay[k]
	if not v then
		return doc.pl[k]
	end
	local q = p - sp.po[k]
	local a, b = 1, #v
	while a < b do
		local mid = (a + b + 1) // 2
		if v[mid].from <= q then
			a = mid
		else
			b = mid - 1
		end
	end
	if up and a > 1 and v[a].from == q and v[a - 1].to == q then
		a -= 1
	end
	return doc.pl[k] + a - 1
end

-- Whether position p, reached on visual line i, needs upstream affinity to
-- stay drawn there (it is line i's end and the next line's start).
local function needs_up(doc, i, p)
	return line_at(doc, p) ~= i and line_at(doc, p, true) == i
end

local function line_width(doc, str, ln)
	if ln then
		return ln.width
	end
	return pwidth(str, doc.size)
end

-- Geometry of an arranged element, from the paragraph count (gutter width).
local function geom(el, np)
	local size = num(el._size, 18)
	local gs = gutter_size(size)
	local gw = 0
	if el.gutter ~= false then
		local digits = max(2, #tostring(max(1, np)))
		gw = floor(digits * U.width("0", gs) + 2 * GUTTER_PAD + 0.5)
	end
	local wrap = el.wrap ~= false
	return {
		size = size,
		lh = line_h(size),
		base = U.line_height(size),
		gs = gs,
		gw = gw,
		ix = el.x + gw + PADX,
		iy = el.y + PADY,
		iw = max(1, el.w - gw - 2 * PADX - SB),
		ih = max(1, el.h - 2 * PADY - (if wrap then 0 else SB)),
		wrap = wrap,
	}
end

local function layout(el, s)
	local sp = split_of(s)
	local g = geom(el, sp.n)
	return doc_of(sp, g.size, if g.wrap then g.iw else 0), g
end

-- Content width when not wrapping (room for the caret past the widest line).
local function content_w(doc, g)
	return max(g.iw, doc.maxw + 4)
end

-- Where line i starts (x relative to the text area's left edge) and its
-- direction. Right-to-left lines are right-aligned: to the text area when
-- wrapping, to the content width (then scrolled) when not.
local function place(doc, g, str, k, ln, sx)
	if not doc.sp.inf[k][1] then
		return -sx, "ltr"
	end
	local lw = line_width(doc, str, ln)
	if g.wrap then
		return g.iw - lw, "rtl"
	end
	return content_w(doc, g) - lw - sx, "rtl"
end

-- ---------------------------------------------------------------- edit state

local function key_of(id)
	return "mle:" .. tostring(id)
end

local function value_of(el)
	local v = el.value
	if type(v) ~= "string" then
		return if v == nil then "" else tostring(v)
	end
	return v
end

-- The element's current text: `source()` when it has one (handlers may run
-- on an element built from older inputs), else its value. Also says whether
-- the source asks for read-only.
local function current_of(el)
	local f = el.source
	if type(f) == "function" then
		local v, ro = f()
		if type(v) == "string" then
			return v, ro == true
		end
	end
	return value_of(el), el.readonly == true
end

local function edit_of(u, id, s)
	local ed = u[key_of(id)]
	if type(ed) ~= "table" then
		ed = { caret = 0, anchor = 0 }
	else
		ed = table.clone(ed)
	end
	ed.caret = U.utf8_cut(s, clamp(floor(num(ed.caret, 0)), 0, #s))
	ed.anchor = U.utf8_cut(s, clamp(floor(num(ed.anchor, ed.caret)), 0, #s))
	ed.up = if ed.up == true then true else nil
	return ed
end
E.edit_of = edit_of

local function sel_range(ed)
	local a, b = ed.anchor, ed.caret
	if a > b then
		a, b = b, a
	end
	return a, b
end

-- What the editor shows: the text with the IME preedit in place of the
-- selection. Returns the display string, caret, selection and preedit range.
local function display(s, ed, ime)
	local a, b = sel_range(ed)
	if ime ~= nil and ime ~= "" then
		local disp = sub(s, 1, a) .. ime .. sub(s, b + 1)
		local c = a + #ime
		return disp, c, c, c, a, c
	end
	return s, ed.caret, a, b, nil, nil
end

local function get_scroll(u, id)
	local s = u.scroll
	return if type(s) == "table" then num(s[id], 0) else 0
end

local function put_scroll(u, id, v)
	if type(u.scroll) ~= "table" then
		u.scroll = {}
	end
	u.scroll[id] = if v > 0 then v else nil
end

-- Scroll so the caret is in view (vertical scroll in u, horizontal in ed);
-- returns the caret rectangle, app-local.
local function follow(el, u, s, ed, ime)
	local disp, dcar = display(s, ed, ime)
	local doc, g = layout(el, disp)
	local i = line_at(doc, dcar, ed.up and (ime == nil or ime == ""))
	local y = (i - 1) * g.lh
	local sy = get_scroll(u, el.id)
	if y < sy then
		sy = y
	end
	if y + g.lh > sy + g.ih then
		sy = y + g.lh - g.ih
	end
	sy = clamp(sy, 0, max(0, doc.n * g.lh - g.ih))
	put_scroll(u, el.id, sy)
	local str, f, _, k, ln = line_of(doc, i)
	local sx = 0
	if not g.wrap then
		sx = num(ed.sx, 0)
		local lx0, dir = place(doc, g, str, k, ln, 0)
		local x = lx0 + tcaret(str, g.size, dcar - f, dir)
		if x - sx > g.iw - 4 then
			sx = x - g.iw + 4 + g.iw / 4
		end
		if x - sx < 4 then
			sx = x - 4 - g.iw / 4
		end
		sx = clamp(sx, 0, max(0, content_w(doc, g) - g.iw))
		ed.sx = if sx > 0 then sx else nil
	else
		ed.sx = nil
	end
	local lx, dir = place(doc, g, str, k, ln, sx)
	local x = lx + tcaret(str, g.size, dcar - f, dir)
	return { g.ix + x, g.iy + y - sy, 2, g.lh }
end

-- ---------------------------------------------------------------- undo history
--
-- Per editor id: edits { at, del, ins, c0, a0, c1, a1, len, kind, t }. Undo
-- puts `del` back where `ins` is; before that it checks the text still has
-- `ins` there and the length the edit left, and drops the history when the
-- text was changed some other way.

local hists = {}

local function history(id)
	local h = hists[id]
	if h == nil then
		h = { undo = {}, redo = {}, bytes = 0 }
		hists[id] = h
	end
	return h
end

local function entry_bytes(e)
	return #e.del + #e.ins + 64
end

local function drop_redo(h)
	for _, e in h.redo do
		h.bytes -= entry_bytes(e)
	end
	h.redo = {}
end

local function record(id, e)
	local h = history(id)
	drop_redo(h)
	local last = h.undo[#h.undo]
	local now = clock()
	if
		e.kind == "type"
		and last ~= nil
		and last.kind == "type"
		and e.del == ""
		and last.at + #last.ins == e.at
		and last.c1 == e.c0
		and now - last.t < TYPE_PAUSE
		and #last.ins < 512
	then
		h.bytes += #e.ins
		last.ins ..= e.ins
		last.c1, last.a1, last.len, last.t = e.c1, e.a1, e.len, now
		return
	end
	e.t = now
	h.undo[#h.undo + 1] = e
	h.bytes += entry_bytes(e)
	while #h.undo > 1 and (#h.undo > UNDO_STEPS or h.bytes > UNDO_BYTES) do
		h.bytes -= entry_bytes(table.remove(h.undo, 1))
	end
end

function E.history_size(id)
	local h = hists[id]
	if h == nil then
		return 0, 0
	end
	return #h.undo, #h.redo
end

function E.clear_history(id)
	hists[id] = nil
end

-- ---------------------------------------------------------------- applying changes

-- A context for changing editor `el` from a handler: { el, app, u, s, ed }.
local function ctx_of(el, app)
	local u = app:ui_get()
	local s, ro = current_of(el)
	if ro then
		el.readonly = true
	end
	return { el = el, app = app, u = u, s = s, ed = edit_of(u, el.id, s) }
end

-- Make (ns, caret, anchor) the editor's text and edit state; `up` keeps the
-- caret at the end of the earlier of two visual lines meeting at it.
local function commit(c, ns, caret, anchor, cx, up)
	local el, app, u = c.el, c.app, c.u
	local ed = { caret = caret, anchor = anchor or caret, sx = c.ed.sx, cx = cx, up = if up then true else nil }
	if u.focus == el.id then
		u.ime = nil
	end
	u.blink = app:now()
	local rect = follow(el, u, ns, ed, nil)
	u[key_of(el.id)] = ed
	app:ui_set(u)
	if ns ~= c.s then
		app:call(el.on_change, ns)
	end
	if el.on_caret then
		app:call(el.on_caret, caret, anchor or caret)
	end
	if u.focus == el.id then
		app:host_caret(rect)
	end
	c.s, c.ed = ns, ed
	return true
end

local function move(c, caret, anchor, cx, up)
	return commit(c, c.s, caret, anchor, cx, up)
end

local function refuse(c, why)
	c.app:toast(why, { tone = "warning" })
	return true
end

-- Replace bytes [a, b) with `ins`; the caret goes after it (or to `caret`,
-- `anchor` when given). kind "type" merges with the previous typing.
local function replace(c, a, b, ins, kind, caret, anchor)
	local el, s = c.el, c.s
	if el.readonly then
		return refuse(c, "這份文件是唯讀的 (read-only)")
	end
	ins = E.normalize(ins)
	local room = E.MAX - (#s - (b - a))
	if #ins > room then
		if room <= 0 then
			return refuse(c, "文件已達 " .. (E.MAX // 1024) .. " KB 上限")
		end
		ins = sub(ins, 1, U.utf8_cut(ins, room))
		c.app:toast("文件已達上限，只插入了前 " .. #ins .. " 位元組", { tone = "warning" })
	end
	if ins == "" and a == b then
		return true
	end
	local ns = sub(s, 1, a) .. ins .. sub(s, b + 1)
	local nc = caret or (a + #ins)
	local na = anchor or nc
	local typing = kind == "type" and a == b and #ins <= 16 and not find(ins, "%s")
	record(el.id, {
		at = a,
		del = sub(s, a + 1, b),
		ins = ins,
		c0 = c.ed.caret,
		a0 = c.ed.anchor,
		c1 = nc,
		a1 = na,
		len = #ns,
		kind = if typing then "type" else "edit",
	})
	return commit(c, ns, nc, na)
end

local function undo(c, redo)
	local h = history(c.el.id)
	local from, to = h.undo, h.redo
	if redo then
		from, to = h.redo, h.undo
	end
	local e = from[#from]
	if e == nil then
		return true
	end
	local s = c.s
	local ok
	if redo then
		ok = #s == e.len - #e.ins + #e.del and sub(s, e.at + 1, e.at + #e.del) == e.del
	else
		ok = #s == e.len and sub(s, e.at + 1, e.at + #e.ins) == e.ins
	end
	if not ok then
		-- the text changed outside the editor (an agent, a reload): start over
		E.clear_history(c.el.id)
		return refuse(c, "文件已在別處變更，復原紀錄已清除")
	end
	table.remove(from)
	to[#to + 1] = e
	e.t = -huge -- never merge typing into an entry that was undone
	if redo then
		return commit(c, sub(s, 1, e.at) .. e.ins .. sub(s, e.at + #e.del + 1), e.c1, e.a1)
	end
	return commit(c, sub(s, 1, e.at) .. e.del .. sub(s, e.at + #e.ins + 1), e.c0, e.a0)
end

local function insert(c, ins, kind)
	local a, b = sel_range(c.ed)
	return replace(c, a, b, ins, kind)
end

-- ---------------------------------------------------------------- moving

-- The grapheme boundary at or before byte `at` (inside a cluster: its start).
function E.snap(s, at)
	at = U.utf8_cut(s, clamp(floor(num(at, 0)), 0, #s))
	if at <= 0 or at >= #s then
		return at
	end
	return tprev(s, tnext(s, at))
end

-- Caret stops of one line sorted left to right: { {pos, x}, ... }.
local function stops_of(str, size, dir)
	local list = { { 0, tcaret(str, size, 0, dir) } }
	local p = 0
	while p < #str do
		local q = tnext(str, p)
		if q <= p then
			break
		end
		p = q
		list[#list + 1] = { p, tcaret(str, size, p, dir) }
	end
	table.sort(list, function(x, y)
		if x[2] == y[2] then
			return x[1] < y[1]
		end
		return x[2] < y[2]
	end)
	return list
end

-- Left / Right: the next caret stop on screen in that direction. A line of
-- one direction moves logically (Left goes back in a left-to-right line and
-- forward in a right-to-left one); a line mixing directions is walked stop
-- by stop in x order. At a line's end the caret continues logically.
local function step_lr(c, doc, g, left)
	local s, p = c.s, c.ed.caret
	local i = line_at(doc, p, c.ed.up)
	local str, f, _, k = line_of(doc, i)
	local rtl = doc.sp.inf[k][1]
	local forward = left == rtl
	local logical = if forward then tnext(s, p) else tprev(s, p)
	local mixed = if rtl then find(str, "[%w\xC2-\xD5\xE0-\xED]") ~= nil else maybe_rtl(str)
	if not mixed or #str > MIXED_MAX then
		return logical
	end
	local dir = if rtl then "rtl" else "ltr"
	local local_p = p - f
	local x = tcaret(str, g.size, local_p, dir)
	local list = stops_of(str, g.size, dir)
	local best = nil
	if left then
		for j = #list, 1, -1 do
			if list[j][2] < x - 0.5 then
				best = list[j]
				break
			end
		end
	else
		for j = 1, #list do
			if list[j][2] > x + 0.5 then
				best = list[j]
				break
			end
		end
	end
	if best == nil then
		return logical
	end
	local np = f + best[1]
	return np, needs_up(doc, i, np)
end

-- Up / Down / PageUp / PageDown: the same x on another visual line.
-- Returns the position, the x to keep, and the caret affinity.
local function step_v(c, doc, g, delta)
	local s, p = c.s, c.ed.caret
	local sx = if g.wrap then 0 else num(c.ed.sx, 0)
	local i = line_at(doc, p, c.ed.up)
	local str, f, _, k, ln = line_of(doc, i)
	local lx, dir = place(doc, g, str, k, ln, sx)
	local x = num(c.ed.cx, nil) or (lx + tcaret(str, g.size, p - f, dir))
	local j = i + delta
	if j < 1 then
		return 0, x
	end
	if j > doc.n then
		return #s, x
	end
	local str2, f2, _, k2, ln2 = line_of(doc, j)
	local lx2, dir2 = place(doc, g, str2, k2, ln2, sx)
	local np = f2 + thit(str2, g.size, x - lx2, dir2)
	return np, x, needs_up(doc, j, np)
end

-- Byte position under an app-local point, with the layout, the visual line,
-- the paragraph, and the caret affinity that keeps it on that line.
local function pos_at(el, u, s, x, y)
	local doc, g = layout(el, s)
	local sy = get_scroll(u, el.id)
	local i = clamp(floor((y - g.iy + sy) / g.lh) + 1, 1, doc.n)
	local str, f, _, k, ln = line_of(doc, i)
	local sx = 0
	if not g.wrap then
		sx = clamp(num(edit_of(u, el.id, s).sx, 0), 0, max(0, content_w(doc, g) - g.iw))
	end
	local lx, dir = place(doc, g, str, k, ln, sx)
	local p = f + thit(str, g.size, x - g.ix - lx, dir)
	return p, doc, i, k, needs_up(doc, i, p)
end

-- ---------------------------------------------------------------- keys

local function key(el, app, k, base, ctrl, shift, alt)
	local c = ctx_of(el, app)
	local s, ed = c.s, c.ed
	local a, b = sel_range(ed)
	local p = ed.caret
	local function go(nc, cx, up)
		return move(c, nc, if shift then ed.anchor else nc, cx, up)
	end
	if base == "left" or base == "right" then
		local left = base == "left"
		local doc, g = layout(el, s)
		local k2 = para_at(doc.sp, p)
		local rtl = doc.sp.inf[k2][1]
		if not shift and a ~= b and not ctrl then
			-- collapse to the selection's edge on that side of the screen
			return go(if left ~= rtl then a else b)
		end
		if ctrl then
			local forward = left == rtl
			return go(if forward then tnw(s, p) else tpw(s, p))
		end
		local np, up = step_lr(c, doc, g, left)
		return go(np, nil, up)
	elseif base == "up" or base == "down" or base == "pageup" or base == "pagedown" then
		local doc, g = layout(el, s)
		local page = max(1, floor(g.ih / g.lh) - 1)
		local delta = if base == "up" then -1 elseif base == "down" then 1 elseif base == "pageup" then -page else page
		local nc, cx, up = step_v(c, doc, g, delta)
		return go(nc, cx, up)
	elseif base == "home" or base == "end" then
		if ctrl then
			return go(if base == "home" then 0 else #s)
		end
		local doc = layout(el, s)
		local i = line_at(doc, p, ed.up)
		local str, f = line_of(doc, i)
		if base == "home" then
			return go(f)
		end
		-- the end of this visual line: where a wrap with no space breaks, that
		-- is also the next line's start, so the caret keeps to this line
		local e = f + #str
		return go(e, nil, needs_up(doc, i, e))
	elseif base == "backspace" then
		if a ~= b then
			return replace(c, a, b, "", "edit")
		end
		if p == 0 then
			return true
		end
		return replace(c, if ctrl then tpw(s, p) else tprev(s, p), p, "", "edit")
	elseif base == "delete" then
		if a ~= b then
			return replace(c, a, b, "", "edit")
		end
		if p >= #s then
			return true
		end
		return replace(c, p, if ctrl then tnw(s, p) else tnext(s, p), "", "edit")
	elseif ctrl and not alt and base == "a" then
		return move(c, #s, 0)
	elseif ctrl and not alt and (base == "c" or base == "x") then
		if a ~= b then
			app:copy(sub(s, a + 1, b))
			if base == "x" then
				return replace(c, a, b, "", "edit")
			end
		end
		return true
	elseif ctrl and not alt and base == "v" then
		app:paste_into(el)
		return true
	elseif ctrl and not alt and base == "z" then
		return undo(c, shift)
	elseif ctrl and not alt and base == "y" then
		return undo(c, true)
	elseif base == "enter" and not alt then
		return insert(c, "\n", "edit")
	elseif base == "space" and not ctrl and not alt then
		return insert(c, " ", "type")
	end
	return false
end

-- ---------------------------------------------------------------- pointer

local function down(el, app, x, y, dr)
	local c = ctx_of(el, app)
	local s, u = c.s, c.u
	local doc, g = layout(el, s)
	local total = doc.n * g.lh
	-- the scrollbars
	if x >= el.x + el.w - SB - 2 and total > g.ih then
		dr.kind, dr.sy0 = "mle_vbar", get_scroll(u, el.id)
		return "captured"
	end
	if not g.wrap and y >= el.y + el.h - SB - 2 and content_w(doc, g) > g.iw then
		dr.kind, dr.sx0 = "mle_hbar", num(c.ed.sx, 0)
		return "captured"
	end
	local p, _, _, k, up = pos_at(el, u, s, x, y)
	local caret, anchor = p, p
	local sp = doc.sp
	if x < el.x + g.gw or dr.clicks >= 3 then
		-- the gutter or a triple click: the whole paragraph with its line break
		anchor = sp.po[k]
		caret = if k < sp.n then sp.po[k + 1] else #s
		dr.kind, dr.unit = "mle_text", "para"
		up = false
	elseif dr.clicks == 2 then
		local e = tnw(s, p)
		caret, anchor = e, tpw(s, e)
		dr.kind, dr.unit = "mle_text", "word"
		up = false
	else
		dr.kind, dr.unit = "mle_text", "char"
	end
	dr.a0, dr.b0 = min(caret, anchor), max(caret, anchor)
	move(c, caret, anchor, nil, up)
	return "captured"
end

local function drag(el, app, x, y, dr)
	local c = ctx_of(el, app)
	local s, u = c.s, c.u
	if dr.kind == "mle_vbar" or dr.kind == "mle_hbar" then
		local doc, g = layout(el, s)
		if dr.kind == "mle_vbar" then
			local total = doc.n * g.lh
			local track = el.h - 8
			local thumb = max(24, track * g.ih / total)
			local sy = dr.sy0 + (y - dr.y0) * (total - g.ih) / max(1, track - thumb)
			put_scroll(u, el.id, clamp(sy, 0, max(0, total - g.ih)))
		else
			local cw = content_w(doc, g)
			local track = el.w - g.gw - 8
			local thumb = max(24, track * g.iw / cw)
			local ed = c.ed
			ed.sx = clamp(dr.sx0 + (x - dr.x0) * (cw - g.iw) / max(1, track - thumb), 0, max(0, cw - g.iw))
			u[key_of(el.id)] = ed
		end
		app:ui_set(u)
		return
	end
	if dr.kind ~= "mle_text" then
		return
	end
	local p, doc, _, k, up = pos_at(el, u, s, x, y)
	local sp = doc.sp
	local a, b = p, p
	if dr.unit == "para" then
		a = sp.po[k]
		b = if k < sp.n then sp.po[k + 1] else #s
	elseif dr.unit == "word" then
		b = tnw(s, p)
		a = tpw(s, b)
	end
	-- extend from the unit first pressed, keeping it selected
	local anchor, caret
	if a < dr.a0 then
		anchor, caret = dr.b0, a
	else
		anchor, caret = dr.a0, max(b, dr.b0)
	end
	up = dr.unit == "char" and caret == p and up
	if caret ~= c.ed.caret or anchor ~= c.ed.anchor or (up == true) ~= (c.ed.up == true) then
		move(c, caret, anchor, nil, up)
	end
end

local function wheel(el, app, dx, dy)
	local u = app:ui_get()
	local s = current_of(el)
	local doc, g = layout(el, s)
	local step = (app.theme and app.theme.wheel_step) or 40
	local moved = false
	local total = doc.n * g.lh
	local sy0 = get_scroll(u, el.id)
	local sy = clamp(sy0 - dy * step, 0, max(0, total - g.ih))
	if abs(sy - sy0) > 0.01 then
		put_scroll(u, el.id, sy)
		moved = true
	end
	if not g.wrap and dx ~= 0 then
		local ed = edit_of(u, el.id, s)
		local sx0 = num(ed.sx, 0)
		local sx = clamp(sx0 - dx * step, 0, max(0, content_w(doc, g) - g.iw))
		if abs(sx - sx0) > 0.01 then
			ed.sx = sx
			u[key_of(el.id)] = ed
			moved = true
		end
	end
	if moved then
		app:ui_set(u)
		return "moved"
	end
	return "edge"
end

-- ---------------------------------------------------------------- drawing

local function draw(el, dc)
	local th, d = dc.th, dc.d
	local focused = dc.focus == el.id
	local u = dc.u
	local s = value_of(el)
	local ed = edit_of(u, el.id, s)
	local ime = if focused then dc.ime else nil
	local disp, dcar, da, db, i0, i1 = display(s, ed, ime)
	local doc, g = layout(el, disp)
	local sp = doc.sp
	local size, lh = g.size, g.lh
	local toff = floor((lh - g.base) / 2)
	local r = th.radius_small
	d:rrect(el.x, el.y, el.w, el.h, r, th.surface)
	local total = doc.n * lh
	local sy = clamp(get_scroll(u, el.id), 0, max(0, total - g.ih))
	local cw = if g.wrap then g.iw else content_w(doc, g)
	local sx = if g.wrap then 0 else clamp(num(ed.sx, 0), 0, max(0, cw - g.iw))
	el._ch, el._vh, el._sy = total, g.ih, sy
	local first = max(1, floor(sy / lh) + 1)
	local last = min(doc.n, floor((sy + g.ih + PADY) / lh) + 1)
	local cur_line = line_at(doc, dcar, ed.up and i0 == nil)
	local cur_k = para_of_line(doc, cur_line)
	local inner_top, inner_h = el.y + 1, el.h - 2
	-- the gutter: paragraph numbers, a dot for missing glyphs, a mark for right-to-left
	if g.gw > 0 then
		d:rect(el.x + 1, inner_top, g.gw - 1, inner_h, th.surface2)
		d:rect(el.x + g.gw, inner_top, 1, inner_h, th.border)
		L.push_clip(dc, el.x + 1, inner_top, g.gw - 1, inner_h)
		local gs = g.gs
		local glh = U.line_height(gs)
		for i = first, last do
			local k = para_of_line(doc, i)
			if doc.pl[k] == i then
				local ly = g.iy + (i - 1) * lh - sy
				local num = tostring(k)
				local nw = U.width(num, gs)
				local col = if k == cur_k and focused then th.text else th.text_faint
				d:text(el.x + g.gw - GUTTER_PAD - nw, ly + (lh - glh) / 2, gs, col, num)
				local f = sp.inf[k]
				if f[2] > 0 then
					d:circle(el.x + 6, ly + lh / 2, 3, th.danger)
				end
				if f[1] then
					-- a small arrow pointing left: this paragraph runs right to left
					local ax, ay = el.x + g.gw - 5, ly + lh / 2
					d:line({ ax, ay - 3, ax - 3, ay, ax, ay + 3 }, 1.2, th.accent)
				end
			end
		end
		L.pop_clip(dc)
	end
	-- the text area
	local clip_x = el.x + g.gw + 1
	local clip_w = el.w - g.gw - 2
	L.push_clip(dc, clip_x, inner_top, clip_w, inner_h - (if g.wrap then 0 else SB))
	local col = th.text
	local matches = if ime == nil or ime == "" then el.matches else nil
	local qlen = tonumber(el.match_len) or 0
	local cur_match = tonumber(el.current_match)
	local mi = 1
	if type(matches) == "table" and #matches > 0 and qlen > 0 then
		-- first match that ends after the first visible line starts
		local f0 = select(2, line_of(doc, first))
		local lo, hi = 1, #matches + 1
		while lo < hi do
			local mid = (lo + hi) // 2
			if matches[mid] + qlen <= f0 then
				lo = mid + 1
			else
				hi = mid
			end
		end
		mi = lo
	else
		matches = nil
	end
	local match_bg = color.alpha(th.warning, 72)
	local match_cur = color.alpha(th.warning, 170)
	for i = first, last do
		local ly = g.iy + (i - 1) * lh - sy
		local str, f, t, k, ln = line_of(doc, i)
		local lx, dir = place(doc, g, str, k, ln, sx)
		local x0 = g.ix + lx
		local n = #str
		if focused and k == cur_k and da == db then
			d:rect(clip_x, ly, clip_w, lh, th.row_hover)
		end
		-- a very long unwrapped line: only the bytes in view (with a viewport
		-- to spare on each side) are drawn and highlighted
		local v0, v1 = 0, n
		if not g.wrap and n > MIXED_MAX then
			local a1 = thit(str, size, -lx - g.iw, dir)
			local a2 = thit(str, size, -lx + 2 * g.iw, dir)
			v0, v1 = tprev(str, min(a1, a2)), tnext(str, max(a1, a2))
		end
		-- find matches on this line
		if matches then
			while matches[mi] ~= nil and matches[mi] + qlen <= f do
				mi += 1
			end
			local j = mi
			while matches[j] ~= nil and matches[j] < f + n do
				local m = matches[j]
				local m0, m1 = max(m, f) - f, min(m + qlen, f + n) - f
				if m1 > m0 and m1 >= v0 and m0 <= v1 then
					local mc = if j == cur_match then match_cur else match_bg
					for _, rr in tsel(str, size, m0, m1, dir) do
						d:rrect(x0 + rr[1], ly + 1, rr[2] - rr[1], lh - 2, 3, mc)
					end
				end
				j += 1
			end
		end
		-- selection
		if da ~= db and da <= f + n and db >= f then
			local s0, s1 = max(da, f) - f, min(db, f + n) - f
			if s1 > s0 then
				for _, rr in tsel(str, size, s0, s1, dir) do
					d:rect(x0 + rr[1], ly, rr[2] - rr[1], lh, th.selection)
				end
			end
			if db > f + n and i < doc.n then
				-- the line break (or the wrap) is selected too: a sliver at the line's end
				local lw = line_width(doc, str, ln)
				if dir == "rtl" then
					d:rect(x0 - 6, ly, 6, lh, th.selection)
				else
					d:rect(x0 + lw, ly, 6, lh, th.selection)
				end
			end
		end
		if n > 0 then
			if dir == "ltr" and (v0 > 0 or v1 < n) then
				-- (a right-to-left part would reorder differently cut out, so those are drawn whole)
				if v1 > v0 then
					d:text(x0 + tcaret(str, size, v0, dir), ly + toff, size, col, sub(str, v0 + 1, v1), dir)
				end
			else
				d:text(x0, ly + toff, size, col, str, dir)
			end
		end
		-- the IME preedit, underlined
		if i0 and i0 <= f + n and i1 >= f then
			local p0, p1 = max(i0, f) - f, min(i1, f + n) - f
			if p1 > p0 then
				for _, rr in tsel(str, size, p0, p1, dir) do
					d:rect(x0 + rr[1], ly + lh - 3, max(1, rr[2] - rr[1]), 2, th.accent)
				end
			end
		end
	end
	-- the caret, with a small flag on the side the paragraph runs to when that is right to left
	if focused and dc.caret_on then
		local str, f, _, k, ln = line_of(doc, cur_line)
		local lx, dir = place(doc, g, str, k, ln, sx)
		local x = g.ix + lx + tcaret(str, size, dcar - f, dir)
		local y = g.iy + (cur_line - 1) * lh - sy
		d:rect(x - 1, y + 2, 2, lh - 4, th.accent)
		if dir == "rtl" then
			d:rect(x - 5, y + 2, 4, 2, th.accent)
		end
	end
	if s == "" and (ime == nil or ime == "") and el.placeholder then
		d:text(g.ix, g.iy + toff, size, th.text_faint, tostring(el.placeholder))
	end
	L.pop_clip(dc)
	-- scrollbars
	if total > g.ih then
		local track = el.h - 8 - (if g.wrap then 0 else SB)
		local thumb = max(24, track * g.ih / total)
		local ty = el.y + 4 + (track - thumb) * sy / (total - g.ih)
		d:rrect(el.x + el.w - SB + 1, ty, SB - 4, thumb, (SB - 4) / 2, th.thumb)
	end
	if not g.wrap and cw > g.iw then
		local track = el.w - g.gw - 8 - SB
		local thumb = max(24, track * g.iw / cw)
		local tx = el.x + g.gw + 4 + (track - thumb) * sx / (cw - g.iw)
		d:rrect(tx, el.y + el.h - SB + 1, thumb, SB - 4, (SB - 4) / 2, th.thumb)
	end
	d:stroke(el.x + 0.5, el.y + 0.5, el.w - 1, el.h - 1, r, if focused then 2 else 1, if focused then th.focus else th.border)
end

-- ---------------------------------------------------------------- the element type

local function on_ime(el, app, pre)
	local u = app:ui_get()
	local s = current_of(el)
	local ed = edit_of(u, el.id, s)
	u.ime = if pre ~= "" then pre else nil
	u.blink = app:now()
	local rect = follow(el, u, s, ed, u.ime)
	u[key_of(el.id)] = ed
	app:ui_set(u)
	app:host_caret(rect)
	return true
end

-- Elements are kept by app and id after layout, so actions (which run
-- outside the element's own handlers) can change an editor with its current
-- geometry.
E.last = {}

local function last_key(app, id)
	return tostring(app and app.name) .. "|" .. tostring(id)
end

T.mleditor = {
	focusable = true,
	fill = true,
	own_focus = true,
	text_input = true,
	shrink_y = true,
	scroller = true,
	check = function(el)
		L.need(el, "value", "string", "number")
		L.need(el, "size", "number")
		L.need(el, "placeholder", "string")
		L.need(el, "matches", "table", nil, "a list of byte offsets")
		L.need_fns(el, "on_change", "on_caret", "on_focus", "source")
	end,
	measure = function(el, aw, ah, cx)
		el._size = clamp(num(el.size, cx.theme.font), 6, 96)
		local w = if aw < huge then aw else 640
		return w, el.min_h or 160
	end,
	arrange = function(el, x, y, w, h, cx)
		local doc, g = layout(el, value_of(el))
		el._ch, el._vh = doc.n * g.lh, g.ih
		el._sy = clamp(get_scroll(cx.u, el.id), 0, max(0, el._ch - el._vh))
		E.last[last_key(cx.app, el.id)] = el
	end,
	-- an agent's click (editor.do op "click"): focus, caret at the end
	activate = function(el, app, how)
		if how ~= "agent" then
			return
		end
		app:focus(el.id)
		local c = ctx_of(el, app)
		move(c, #c.s, #c.s)
	end,
	draw = draw,
	down = down,
	drag = drag,
	wheel = wheel,
	key = key,
	text = function(el, app, s)
		return insert(ctx_of(el, app), s, "type")
	end,
	paste = function(el, app, s)
		return insert(ctx_of(el, app), s, "edit")
	end,
	ime = on_ime,
	on_focus = function(el, app, u, how)
		local s = current_of(el)
		local ed = edit_of(u, el.id, s)
		u.blink = app:now()
		local rect = follow(el, u, s, ed, nil)
		u[key_of(el.id)] = ed
		return rect
	end,
	describe = function(el)
		local s = value_of(el)
		return {
			length = #s,
			value = s,
			wrap = el.wrap ~= false,
			size = el._size,
			readonly = el.readonly == true,
			-- the UI library's op "set" has no hook for element types of a page
			hint = el.agent_hint
				or "op type inserts at the caret, op key presses keys, op click puts the caret at the end; "
					.. "op set is not available here: use the page's set_text action",
		}
	end,
}

-- An editor element: { id, value, size, wrap, matches, match_len,
-- current_match, readonly, placeholder, gutter, on_change(text),
-- on_caret(caret, anchor) } plus the usual sizing props.
function E.element(opts)
	local e = table.clone(opts)
	e.type = "mleditor"
	return e
end

-- ---------------------------------------------------------------- changing it from the page

-- A change context for the editor `id`, for actions and toolbar buttons:
-- `props` are the element's current props (value, size, wrap, callbacks); the
-- geometry comes from its last layout.
function E.ctx(app, id, props)
	local base = E.last[last_key(app, id)]
	local el = { id = id, x = 0, y = 0, w = 800, h = 600 }
	if base then
		el.x, el.y, el.w, el.h, el.gutter = base.x, base.y, base.w, base.h, base.gutter
	end
	for k, v in props do
		el[k] = v
	end
	el._size = clamp(num(el.size, 18), 6, 96)
	return ctx_of(el, app)
end

function E.selection(c)
	return sel_range(c.ed)
end

E.replace = replace
E.undo = undo
E.insert = insert

-- Select [a, b) with the caret at b (in view).
function E.select(c, a, b)
	return move(c, b, a)
end

-- Keep the caret where it is, scrolled into view (after a size or wrap change).
function E.reveal(c)
	return move(c, c.ed.caret, c.ed.anchor)
end

-- Replace every match of `q` (as E.find_all finds them) with `with`, as one
-- undoable step. Returns the number replaced, or nil and why not.
function E.replace_all(c, q, with, case)
	if type(q) ~= "string" or q == "" then
		return 0
	end
	with = E.normalize(tostring(with or ""))
	local s = c.s
	local hay, needle = s, q
	if not case then
		hay, needle = E.fold(s), E.fold(q)
	end
	local parts, n, i, last = {}, 0, 1, 1
	while true do
		local a = find(hay, needle, i, true)
		if a == nil then
			break
		end
		parts[#parts + 1] = sub(s, last, a - 1)
		parts[#parts + 1] = with
		n += 1
		last = a + #needle
		i = last
	end
	if n == 0 then
		return 0
	end
	parts[#parts + 1] = sub(s, last)
	local ns = table.concat(parts)
	if #ns > E.MAX then
		return nil, "too_big"
	end
	if c.el.readonly then
		return nil, "readonly"
	end
	local p = E.snap(ns, min(c.ed.caret, #ns))
	replace(c, 0, #s, ns, "edit", p, p)
	return n
end

-- Replace the whole text as one undoable step; the caret goes to `caret`.
function E.set_text(c, ns, caret)
	ns = E.normalize(ns)
	if #ns > E.MAX then
		ns = sub(ns, 1, U.utf8_cut(ns, E.MAX))
	end
	if ns == c.s then
		return move(c, clamp(caret or 0, 0, #ns), clamp(caret or 0, 0, #ns))
	end
	local cc = clamp(caret or 0, 0, #ns)
	local was = c.el.readonly
	c.el.readonly = false
	replace(c, 0, #c.s, ns, "edit", cc, cc)
	c.el.readonly = was
	return true
end

-- ---------------------------------------------------------------- find
--
-- Case-insensitive search folds ASCII and the two-byte capitals of Latin-1,
-- Greek and Cyrillic to small letters; every fold keeps the byte length, so
-- offsets in the folded text are offsets in the original.

local FOLD = {}
local function fold_range(lo, hi, delta)
	for cp = lo, hi do
		FOLD[utf8.char(cp)] = utf8.char(cp + delta)
	end
end
fold_range(0xC0, 0xD6, 0x20)
fold_range(0xD8, 0xDE, 0x20)
fold_range(0x391, 0x3A1, 0x20)
fold_range(0x3A3, 0x3AB, 0x20)
fold_range(0x410, 0x42F, 0x20)
fold_range(0x400, 0x40F, 0x50)
FOLD[utf8.char(0x386)] = utf8.char(0x3AC)
fold_range(0x388, 0x38A, 0x25)
FOLD[utf8.char(0x38C)] = utf8.char(0x3CC)
fold_range(0x38E, 0x38F, 0x3F)

function E.fold(s)
	return (gsub(string.lower(s), "[\xC3\xCE\xD0][\x80-\xBF]", FOLD))
end

local finds = {}
local find_order = {}
local MAX_MATCHES = 20000

-- Byte offsets of every match of `q` in `s` (at most MAX_MATCHES), cached.
function E.find_all(s, q, case)
	if type(q) ~= "string" or q == "" or type(s) ~= "string" then
		return {}
	end
	local key = (if case then "C" else "i") .. q
	local per = finds[key]
	if per == nil then
		per = {}
		finds[key] = per
	end
	local hit = per[s]
	if hit then
		return hit
	end
	local hay, needle = s, q
	if not case then
		hay, needle = E.fold(s), E.fold(q)
	end
	local out = {}
	local i = 1
	while #out < MAX_MATCHES do
		local a = find(hay, needle, i, true)
		if a == nil then
			break
		end
		out[#out + 1] = a - 1
		i = a + #needle
	end
	per[s] = out
	find_order[#find_order + 1] = { key, s }
	while #find_order > 6 do
		local o = table.remove(find_order, 1)
		local m = finds[o[1]]
		if m then
			m[o[2]] = nil
		end
	end
	return out
end

-- ---------------------------------------------------------------- files

-- Whether this runtime can read an opened or dropped file's bytes as text
-- (text.decode; older runtimes had no way to).
function E.can_decode()
	return type(rawget(text, "decode")) == "function"
end

-- Largest file (bytes) handed to text.decode: E.MAX of text as UTF-16, plus
-- its byte order mark.
E.MAX_FILE = 2 * E.MAX + 2

-- The text of a file from the open dialog, a drop ({ name, size, data = a
-- byte resource }) or the load_text action ({ text = a string }). Returns
-- the string and the encoding it was stored in ("utf-8", "utf-8-bom",
-- "utf-16"; nil when it came as a string, which E.from_file checks), or nil
-- and why not ("unreadable", "binary", "too_big").
--
-- text.decode drops the byte order mark and does not say which encoding it
-- read, so that comes from the sizes: as many bytes as the text has is
-- UTF-8, three more is UTF-8 with a BOM, anything else was UTF-16 (a few
-- tiny UTF-16 files have one of the first two sizes by chance; they are then
-- saved as UTF-8 with or without a BOM, which is what UTF-16 files are saved
-- as anyway). NOTES.md asks for text.decode to return the encoding.
function E.decode_file(f)
	local v = if f.text ~= nil then f.text else f.data
	if type(v) == "string" then
		return v, nil
	end
	if type(v) == "buffer" then
		return buffer.tostring(v), nil
	end
	local dec = rawget(text, "decode")
	if v == nil or type(dec) ~= "function" then
		return nil, "unreadable"
	end
	local ok, r = pcall(dec, v, E.MAX_FILE)
	if not ok then
		return nil, if find(tostring(r), "limit", 1, true) then "too_big" else "binary"
	end
	if type(r) ~= "string" then
		return nil, "unreadable"
	end
	local size = num(f.size, #r)
	local enc = if size == #r then "utf-8" elseif size == #r + 3 then "utf-8-bom" else "utf-16"
	return r, nil, enc
end

local function count(s, pat)
	return select(2, gsub(s, pat, ""))
end

-- Check and normalise file text: UTF-8 only, no binary. Returns
-- { text, eol, bom, mixed } or nil and a message. `eol` is the file's line
-- end ("\r\n", "\r" for classic Mac files, or "\n"; the most frequent one
-- when they are mixed, and `mixed` is then true): saving writes it back.
function E.from_file(s)
	if type(s) ~= "string" then
		return nil, "unreadable"
	end
	local b1, b2 = string.byte(s, 1, 2)
	if (b1 == 0xFF and b2 == 0xFE) or (b1 == 0xFE and b2 == 0xFF) then
		return nil, "utf16"
	end
	local bom = sub(s, 1, 3) == "\xEF\xBB\xBF"
	if bom then
		s = sub(s, 4)
	end
	if #s > E.MAX then
		return nil, "too_big"
	end
	if find(s, "\0", 1, true) or utf8.len(s) == nil then
		return nil, "binary"
	end
	-- other control characters (except tab, form feed, line breaks) mean binary too
	if find(s, "[\1-\8\14-\26\28-\31\127]") then
		return nil, "binary"
	end
	local eol, mixed = "\n", false
	if find(s, "\r", 1, true) then
		local crlf = count(s, "\r\n")
		local cr = count(s, "\r") - crlf
		local lf = count(s, "\n") - crlf
		if crlf >= cr and crlf >= lf then
			eol = "\r\n"
		elseif cr >= lf then
			eol = "\r"
		end
		mixed = (if crlf > 0 then 1 else 0) + (if cr > 0 then 1 else 0) + (if lf > 0 then 1 else 0) > 1
	end
	return { text = E.normalize(s), eol = eol, bom = bom, mixed = mixed }
end

-- The bytes to save: line ends and BOM as the file had them.
function E.to_file(s, eol, bom)
	if eol == "\r\n" then
		s = gsub(s, "\n", "\r\n")
	elseif eol == "\r" then
		s = gsub(s, "\n", "\r")
	end
	if bom then
		s = "\xEF\xBB\xBF" .. s
	end
	return s
end

return E
