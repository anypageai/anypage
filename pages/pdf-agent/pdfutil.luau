-- pdfutil: pure helpers for the PDF workspace (geometry, word selection,
-- tables, summaries). Nothing here touches page cells or edits documents,
-- so every function can run inside cell functions as well as handlers.

local U = {}

local max, min, floor, abs = math.max, math.min, math.floor, math.abs

function U.clamp(v, lo, hi)
	if v < lo then
		return lo
	elseif v > hi then
		return hi
	end
	return v
end

function U.inside(r, x, y)
	return x >= r[1] and y >= r[2] and x < r[1] + r[3] and y < r[2] + r[4]
end

-- At most `n` characters of `s` (UTF-8 aware), with an ellipsis when cut.
function U.clip(s, n)
	s = tostring(s or "")
	if #s <= n then
		return s
	end
	local count = utf8.len(s)
	if count == nil then
		return string.sub(s, 1, n) .. "…"
	end
	if count <= n then
		return s
	end
	local cut = utf8.offset(s, n + 1)
	return string.sub(s, 1, (cut or (#s + 1)) - 1) .. "…"
end

-- The last `n` characters of `s` (UTF-8 aware), with an ellipsis in front when cut.
function U.tail(s, n)
	s = tostring(s or "")
	if #s <= n then
		return s
	end
	local count = utf8.len(s)
	if count == nil then
		return "…" .. string.sub(s, -n)
	end
	if count <= n then
		return s
	end
	return "…" .. string.sub(s, utf8.offset(s, count - n + 1) or 1)
end

-- A finite number from anything (agent-written values): `d` (default 0) otherwise.
function U.num(v, d)
	local n = tonumber(v)
	if n == nil or n ~= n or n == math.huge or n == -math.huge then
		return d or 0
	end
	return n
end

function U.trim(s)
	return (string.gsub(tostring(s or ""), "^%s*(.-)%s*$", "%1"))
end

-- One line: runs of white space (line breaks too) become single spaces.
function U.one_line(s)
	return U.trim((string.gsub(tostring(s or ""), "%s+", " ")))
end

-- ---------------------------------------------------------------- page geometry

U.PAD = 20
U.ZOOMS = { 0.5, 0.75, 1, 1.25, 1.5, 2, 3, 4 }

-- Where page `size` ({w, h} points) sits in viewport `V` ({x, y, w, h} world)
-- at `zoom` ("fit", "width" or a scale, 1 = one world unit per point) and
-- `scroll` ({x, y}). Returns the page's world rect, the scale, the largest
-- scroll offsets and the scroll actually used.
function U.layout(V, size, zoom, scroll)
	local pw, ph = max(1, size.w), max(1, size.h)
	local pad = U.PAD
	local aw, ah = V[3] - 2 * pad, V[4] - 2 * pad
	local s
	if zoom == "width" then
		s = aw / pw
	elseif type(zoom) == "number" then
		s = zoom
	else
		s = min(aw / pw, ah / ph)
	end
	if s ~= s then
		s = 1
	end
	s = U.clamp(s, 0.1, 8)
	local w, h = pw * s, ph * s
	local cw, ch = w + 2 * pad, h + 2 * pad
	local mx, my = max(0, cw - V[3]), max(0, ch - V[4])
	-- scroll comes from an agent-writable cell: anything but {x, y} numbers reads as 0
	if type(scroll) ~= "table" then
		scroll = {}
	end
	local sx = U.clamp(U.num(scroll.x), 0, mx)
	local sy = U.clamp(U.num(scroll.y), 0, my)
	local x = if cw <= V[3] then V[1] + (V[3] - w) / 2 else V[1] + pad - sx
	local y = if ch <= V[4] then V[2] + (V[4] - h) / 2 else V[2] + pad - sy
	return { x, y, w, h }, s, mx, my, sx, sy
end

-- The scale "fit" would use, to step from it to the next zoom level.
function U.scale_of(V, size, zoom)
	local _, s = U.layout(V, size, zoom, nil)
	return s
end

function U.next_zoom(s, dir)
	if dir > 0 then
		for _, z in U.ZOOMS do
			if z > s + 0.01 then
				return z
			end
		end
		return U.ZOOMS[#U.ZOOMS]
	end
	for i = #U.ZOOMS, 1, -1 do
		local z = U.ZOOMS[i]
		if z < s - 0.01 then
			return z
		end
	end
	return U.ZOOMS[1]
end

-- A page of size `size` fitted (centred) into box `b`.
function U.fit_box(b, size)
	local s = min(b[3] / max(1, size.w), b[4] / max(1, size.h))
	local w, h = size.w * s, size.h * s
	return { b[1] + (b[3] - w) / 2, b[2] + (b[4] - h) / 2, w, h }
end

function U.intersect(a, b)
	local x0, y0 = max(a[1], b[1]), max(a[2], b[2])
	local x1, y1 = min(a[1] + a[3], b[1] + b[3]), min(a[2] + a[4], b[2] + b[4])
	if x1 <= x0 or y1 <= y0 then
		return nil
	end
	return { x0, y0, x1 - x0, y1 - y0 }
end

-- Fill `area` (panel-local) except the rectangles in `holes`, which must not
-- overlap each other vertically (thumbnails in a column, one page).
function U.fill_except(d, area, holes, c)
	local list = {}
	for _, h in holes do
		local r = U.intersect(area, h)
		if r then
			table.insert(list, r)
		end
	end
	table.sort(list, function(a, b)
		return a[2] < b[2]
	end)
	local x, w = area[1], area[3]
	local y = area[2]
	for _, h in list do
		if h[2] > y then
			d:rect(x, y, w, h[2] - y, c)
		end
		if h[1] > x then
			d:rect(x, h[2], h[1] - x, h[4], c)
		end
		local right = h[1] + h[3]
		if right < x + w then
			d:rect(right, h[2], x + w - right, h[4], c)
		end
		y = max(y, h[2] + h[4])
	end
	local bottom = area[2] + area[4]
	if bottom > y then
		d:rect(x, y, w, bottom - y, c)
	end
end

-- ---------------------------------------------------------------- words and selection

-- The word under page point (x, y), with `pad` points of slack.
function U.word_at(words, x, y, pad)
	pad = pad or 2
	for i, w in words do
		if x >= w.x - pad and x <= w.x + w.w + pad and y >= w.y - pad and y <= w.y + w.h + pad then
			return i
		end
	end
	return nil
end

-- The word nearest to page point (x, y) (distance to its rectangle; a
-- vertical gap counts three times a horizontal one, so the pointer stays on
-- its line).
function U.nearest_word(words, x, y)
	local best, bd = nil, math.huge
	for i, w in words do
		local dx = max(w.x - x, 0, x - (w.x + w.w))
		local dy = max(w.y - y, 0, y - (w.y + w.h))
		local dd = dx + 3 * dy
		if dd < bd then
			best, bd = i, dd
		end
	end
	return best
end

-- Indices of the words whose rectangle meets rect r ({x, y, w, h}, page points).
function U.words_in_rect(words, r)
	local out = {}
	for i, w in words do
		if w.x + w.w >= r[1] and w.x <= r[1] + r[3] and w.y + w.h >= r[2] and w.y <= r[2] + r[4] then
			table.insert(out, i)
		end
	end
	return out
end

local function same_line(a, b)
	local ca, cb = a.y + a.h / 2, b.y + b.h / 2
	return abs(ca - cb) < max(2, min(a.h, b.h) * 0.5)
end
U.same_line = same_line

-- CJK and other non-Latin text runs join without spaces.
local function wide(s, at_end)
	if s == "" then
		return false
	end
	local b = if at_end then string.byte(s, #s) else string.byte(s, 1)
	return b ~= nil and b >= 0x80
end

-- The text of words `idx` (indices in reading order): spaces between words,
-- line breaks between lines.
function U.join(words, idx)
	local parts = {}
	local prev = nil
	for _, i in idx do
		local w = words[i]
		if prev ~= nil then
			if not same_line(prev, w) then
				table.insert(parts, "\n")
			elseif not (wide(prev.text, true) and wide(w.text, false)) then
				table.insert(parts, " ")
			end
		end
		table.insert(parts, w.text)
		prev = w
	end
	return table.concat(parts)
end

-- One rectangle per line for words `idx`: what a highlight covers.
function U.line_rects(words, idx)
	local rects = {}
	local cur = nil
	local last = nil
	for _, i in idx do
		local w = words[i]
		if cur ~= nil and last ~= nil and same_line(last, w) and w.x >= last.x - 1 then
			local x1 = max(cur[1] + cur[3], w.x + w.w)
			local y0 = min(cur[2], w.y)
			local y1 = max(cur[2] + cur[4], w.y + w.h)
			cur[3] = x1 - cur[1]
			cur[2], cur[4] = y0, y1 - y0
		else
			if cur ~= nil then
				table.insert(rects, cur)
			end
			cur = { w.x, w.y, w.w, w.h }
		end
		last = w
	end
	if cur ~= nil then
		table.insert(rects, cur)
	end
	return rects
end

function U.range(a, b)
	local out = {}
	for i = min(a, b), max(a, b) do
		table.insert(out, i)
	end
	return out
end

-- A selection value: { page, text, rects, words }.
function U.selection(page_no, words, idx)
	if #idx == 0 then
		return nil
	end
	return { page = page_no, text = U.join(words, idx), rects = U.line_rects(words, idx), words = #idx }
end

-- ---------------------------------------------------------------- tables and text

local function cell_text(v)
	if v == nil then
		return ""
	elseif type(v) == "number" then
		if v == floor(v) and abs(v) < 1e15 then
			return string.format("%d", v)
		end
		return string.format("%.2f", v)
	end
	return tostring(v)
end
U.cell_text = cell_text

-- A number with thousands separators and `decimals` places: 1620.75 -> "1,620.75".
function U.fmt_number(v, decimals)
	local s = string.format("%." .. (decimals or 0) .. "f", v)
	local sign, int, frac = string.match(s, "^(-?)(%d+)(.*)$")
	if int == nil then
		return s
	end
	int = string.reverse((string.gsub(string.reverse(int), "(%d%d%d)", "%1,")))
	if string.sub(int, 1, 1) == "," then
		int = string.sub(int, 2)
	end
	return sign .. int .. frac
end

-- Decimals each column of table t is written with. Extraction turns cells
-- into numbers ("820.00" -> 820), so the places come from the page's own
-- words inside the table (`words`, from doc:words(page)): a column uses the
-- most places any of its numbers is written with there. Without the words,
-- or when a fraction is not found among them, a fraction means 2.
function U.column_decimals(t, words)
	local out = {}
	for j = 1, #(t.columns or {}) do
		out[j] = 0
	end
	local written = nil
	local r = t.rect
	if type(words) == "table" and type(r) == "table" and #r == 4 then
		written = {}
		for _, w in words do
			local cx, cy = w.x + w.w / 2, w.y + w.h / 2
			if cx >= r[1] and cy >= r[2] and cx <= r[1] + r[3] and cy <= r[2] + r[4] then
				written[(string.gsub(tostring(w.text), ",", ""))] = true
			end
		end
	end
	for _, row in t.rows or {} do
		for j = 1, #out do
			local v = row[j]
			if type(v) == "number" then
				local d = 0
				if written ~= nil then
					for places = 4, 1, -1 do
						if written[string.format("%." .. places .. "f", v)] then
							d = places
							break
						end
					end
				end
				if d == 0 and v ~= floor(v) then
					d = 2
				end
				out[j] = max(out[j], d)
			end
		end
	end
	return out
end

local function csv_field(s)
	if string.find(s, '[,"\n]') then
		return '"' .. string.gsub(s, '"', '""') .. '"'
	end
	return s
end

-- CSV of table t; `decimals` (U.column_decimals) keeps the places numbers
-- are written with in the PDF ("820.00", not "820").
function U.table_csv(t, max_chars, decimals)
	local lines = {}
	local head = {}
	for j, c in t.columns or {} do
		head[j] = csv_field(cell_text(c))
	end
	table.insert(lines, table.concat(head, ","))
	local n = #(t.columns or {})
	for _, row in t.rows or {} do
		local cells = {}
		for j = 1, n do
			local v = row[j]
			local d = decimals and decimals[j]
			if type(v) == "number" and d ~= nil and d > 0 then
				cells[j] = string.format("%." .. d .. "f", v)
			else
				cells[j] = csv_field(cell_text(v))
			end
		end
		table.insert(lines, table.concat(cells, ","))
	end
	local s = table.concat(lines, "\n")
	if max_chars then
		return U.clip(s, max_chars)
	end
	return s
end

local function first_line(s)
	return U.one_line(string.match(tostring(s or ""), "^[^\n]*") or "")
end

-- Is block b inside one of the page's tables?
local function in_table(ex, b)
	local cx, cy = b.x + b.w / 2, b.y + b.h / 2
	for _, t in ex.tables or {} do
		local r = t.rect
		if r and cx >= r[1] and cy >= r[2] and cx <= r[1] + r[3] and cy <= r[2] + r[4] then
			return true
		end
	end
	return false
end

-- Headings: blocks set clearly larger than the page's body text (their
-- first line: a block's size is that of its largest line).
function U.headings(ex)
	local sizes = {}
	for _, b in ex.blocks or {} do
		table.insert(sizes, b.size or 0)
	end
	table.sort(sizes)
	local body = sizes[max(1, floor(#sizes / 2))] or 0
	local out = {}
	for _, b in ex.blocks or {} do
		if (b.size or 0) >= body * 1.25 and (b.size or 0) >= 13 then
			local h = first_line(b.text)
			if h ~= "" then
				table.insert(out, h)
			end
		end
	end
	return out, body
end

-- A structural summary of a page from its extraction (doc:extract), for
-- agents: headings, the opening text (outside tables), tables with their
-- shape. `lead` stays under the semantic port's 400-character cut.
function U.summarize(ex, opts)
	opts = opts or {}
	local heads, body = U.headings(ex)
	local chars = 0
	for _, b in ex.blocks or {} do
		chars += #(b.text or "")
	end
	local lead = {}
	local budget = opts.chars or 380
	for _, b in ex.blocks or {} do
		local t = U.one_line(b.text)
		if (b.size or 0) < body * 1.25 and utf8.len(t) ~= nil and #t > 30 and budget > 0 and not in_table(ex, b) then
			local piece = U.clip(t, budget)
			table.insert(lead, piece)
			budget -= (utf8.len(piece) or #piece) + 1
		end
	end
	local tables = {}
	for k, t in ex.tables or {} do
		local cols = t.columns or {}
		local row1 = (t.rows or {})[1] or {}
		local first = {}
		for j = 1, #cols do
			first[j] = cell_text(row1[j])
		end
		table.insert(tables, {
			index = k,
			columns = table.concat(cols, " | "),
			rows = #(t.rows or {}),
			first_row = table.concat(first, " | "),
		})
	end
	return {
		page = ex.page,
		size = string.format("%.0f x %.0f pt", ex.width or 0, ex.height or 0),
		headings = heads,
		blocks = #(ex.blocks or {}),
		characters = chars,
		lead = table.concat(lead, " "),
		tables = tables,
	}
end

return U
