-- takeoff: what the studio measures in a drawing, the workbook that prices
-- it, and the totals read back from that workbook.
--
--   T.measure(dwg)        pure: rooms (closed outlines on room layers, named
--                         by the text inside them), wall lengths, door and
--                         window counts, in metres
--   T.new_book()          effect: a workbook with a Prices sheet
--   T.write(wb, q, dwg)   effect: the Takeoff sheet for quantities q, with
--                         formulas (per-row cost, SUM, tax, total); returns
--                         the workbook's next version and the row map
--   T.totals(wb, meta)    pure: rows, prices and totals as the workbook
--                         computed them (the derived cell `totals`)
--
-- Layers are recognised by name, as drafting standards name them: rooms on
-- layers with AREA / ROOM / SPACE, their names on ...-IDEN / NAME / TEXT,
-- walls on WALL, doors on DOOR, windows on GLAZ / WIN (Chinese names too).

local T = {}

-- The Prices sheet: one row per key, from row 2. Agents and the page change
-- column D (`set_price`); every formula reads it from there.
T.PRICES = {
	{ key = "wood", label = "木地板 Wood floor", unit = "m²", price = 2800 },
	{ key = "tile", label = "磁磚 Floor tiles", unit = "m²", price = 3200 },
	{ key = "ext_wall", label = "外牆 Exterior wall", unit = "m", price = 6800 },
	{ key = "int_wall", label = "隔間牆 Partition", unit = "m", price = 4200 },
	{ key = "paint", label = "油漆 Wall paint", unit = "m²", price = 380 },
	{ key = "door", label = "室內門 Door", unit = "樘", price = 12000 },
	{ key = "window", label = "鋁窗 Window", unit = "樘", price = 16500 },
	{ key = "height", label = "樓高 Wall height", unit = "m", price = 2.8 },
	{ key = "tax", label = "營業稅率 Tax rate", unit = "%", price = 0.05 },
}
T.ROW = {}
T.BY_KEY = {}
for i, p in T.PRICES do
	T.ROW[p.key] = i + 1
	T.BY_KEY[p.key] = p
end
T.LAST_PRICE_ROW = #T.PRICES + 1
-- the Takeoff sheet is cleared this far before it is rewritten
local CLEAR_ROWS = 400
-- rooms and texts read from one drawing
local MAX_ROOMS = 300
local MAX_LABELS = 5000

local UNIT_M = { [1] = 0.0254, [2] = 0.3048, [4] = 0.001, [5] = 0.01, [6] = 1, [14] = 0.1 }
local UNIT_NAME = { [1] = "in", [2] = "ft", [4] = "mm", [5] = "cm", [6] = "m", [14] = "dm" }

-- ---------------------------------------------------------------- helpers

local function has(s, words)
	for _, w in words do
		if string.find(s, w, 1, true) then
			return true
		end
	end
	return false
end

local ROOM_WORDS = { "AREA", "ROOM", "SPACE", "空間", "房間", "室" }
local LABEL_WORDS = { "IDEN", "NAME", "TEXT", "LABEL", "名稱", "標示" }
local WALL_WORDS = { "WALL", "牆" }
local DOOR_WORDS = { "DOOR", "門" }
local WINDOW_WORDS = { "GLAZ", "WIN", "窗" }

-- Layer names by role: { rooms, labels, walls, doors, windows, role = {name -> role} }.
function T.classify(layers)
	local c = { rooms = {}, labels = {}, walls = {}, doors = {}, windows = {}, role = {} }
	for _, l in layers do
		local n = string.upper(l.name)
		local role = nil
		if has(n, ROOM_WORDS) and has(n, LABEL_WORDS) then
			role = "labels"
		elseif has(n, ROOM_WORDS) then
			role = "rooms"
		elseif has(n, WALL_WORDS) then
			role = "walls"
		elseif has(n, DOOR_WORDS) then
			role = "doors"
		elseif has(n, WINDOW_WORDS) then
			role = "windows"
		end
		if role then
			table.insert(c[role], l.name)
			c.role[l.name] = role
		end
	end
	return c
end

-- Even-odd point in polygon over flat { x1, y1, x2, y2, ... } (bulges ignored).
function T.inside(pts, x, y)
	local n = #pts // 2
	local c = false
	local j = n
	for i = 1, n do
		local xi, yi = pts[2 * i - 1], pts[2 * i]
		local xj, yj = pts[2 * j - 1], pts[2 * j]
		if (yi > y) ~= (yj > y) and x < (xj - xi) * (y - yi) / (yj - yi) + xi then
			c = not c
		end
		j = i
	end
	return c
end

-- Metres per drawing unit and the unit's name ($INSUNITS; a unitless
-- drawing a few thousand units wide is taken as millimetres).
function T.unit_scale(info)
	local u = info.units
	if UNIT_M[u] then
		return UNIT_M[u], UNIT_NAME[u]
	end
	local b = info.bounds
	if b and math.max(b[3], b[4]) > 500 then
		return 0.001, "mm?"
	end
	return 1, "m?"
end

local function is_tile(s)
	local u = string.upper(s)
	return has(u, { "TILE", "磁磚", "瓷磚", "石材" })
end

local function first_line(s)
	return (string.match(s, "^[^\n]*") or s)
end

local function round(x, d)
	local m = 10 ^ (d or 2)
	return math.floor(x * m + 0.5) / m
end
T.round = round

-- ---------------------------------------------------------------- measuring

-- Texts bucketed on a grid by their insertion point, so that a room tests
-- only the texts inside its bounding box (rooms x texts point-in-polygon
-- tests otherwise: 250 rooms with 10 texts each took 0.7 s).
local function label_grid(labels)
	local pts = {}
	local x0, y0, x1, y1 = math.huge, math.huge, -math.huge, -math.huge
	for _, t in labels do
		local x, y = t.x2 or t.x, t.y2 or t.y
		if type(x) == "number" and type(y) == "number" and math.abs(x) < 1e300 and math.abs(y) < 1e300 then
			table.insert(pts, { px = x, py = y, e = t })
			x0, y0 = math.min(x0, x), math.min(y0, y)
			x1, y1 = math.max(x1, x), math.max(y1, y)
		end
	end
	local g = { pts = pts, cells = {}, n = 1, x0 = x0, y0 = y0, cw = 1, ch = 1 }
	if #pts == 0 then
		return g
	end
	local n = math.max(1, math.min(256, math.floor(math.sqrt(#pts))))
	g.n = n
	g.cw = math.max((x1 - x0) / n, 1e-9)
	g.ch = math.max((y1 - y0) / n, 1e-9)
	for _, p in pts do
		local i = math.min(n - 1, math.floor((p.px - x0) / g.cw))
		local j = math.min(n - 1, math.floor((p.py - y0) / g.ch))
		local key = j * n + i
		local cell = g.cells[key]
		if cell == nil then
			cell = {}
			g.cells[key] = cell
		end
		table.insert(cell, p)
	end
	return g
end

-- The grid's texts inside the bounding box of `pts` (flat x, y list).
local function near(g, pts)
	local out = {}
	if #g.pts == 0 then
		return out
	end
	local bx0, by0, bx1, by1 = math.huge, math.huge, -math.huge, -math.huge
	for k = 1, #pts - 1, 2 do
		local x, y = pts[k], pts[k + 1]
		bx0, bx1 = math.min(bx0, x), math.max(bx1, x)
		by0, by1 = math.min(by0, y), math.max(by1, y)
	end
	if not (bx0 <= bx1 and by0 <= by1) then
		return out
	end
	local n = g.n
	local i0 = math.max(0, math.floor((bx0 - g.x0) / g.cw))
	local i1 = math.min(n - 1, math.floor((bx1 - g.x0) / g.cw))
	local j0 = math.max(0, math.floor((by0 - g.y0) / g.ch))
	local j1 = math.min(n - 1, math.floor((by1 - g.y0) / g.ch))
	for j = j0, j1 do
		for i = i0, i1 do
			local cell = g.cells[j * n + i]
			if cell then
				for _, p in cell do
					if p.px >= bx0 and p.px <= bx1 and p.py >= by0 and p.py <= by1 then
						table.insert(out, p)
					end
				end
			end
		end
	end
	return out
end

-- Quantities of a drawing (pure): rooms { id, name, finish, area (m²),
-- points (drawing units) }, exterior and partition walls { ids, length (m) },
-- doors and windows { ids }, plus the unit and the layer roles; `capped` is
-- MAX_ROOMS when the drawing has more room outlines than are measured.
function T.measure(dwg)
	local info = dwg:info()
	local q = {
		rooms = {},
		ext = { ids = {}, length = 0 },
		int = { ids = {}, length = 0 },
		doors = { ids = {} },
		windows = { ids = {} },
		loading = info.loading == true,
		dwg = tostring(dwg),
	}
	local s, unit = T.unit_scale(info)
	q.scale, q.unit = s, unit
	local c = T.classify(dwg:layers())
	q.roles = c.role
	if q.loading then
		return q
	end
	-- room names: text on the label layers (any text when there are none)
	local filter = { type = { "TEXT", "MTEXT" }, limit = MAX_LABELS }
	if #c.labels > 0 then
		filter.layer = c.labels
	end
	local labels = dwg:entities(filter)
	local grid = label_grid(labels)
	if #c.rooms > 0 then
		local outlines = dwg:entities({ layer = c.rooms, type = { "LWPOLYLINE", "POLYLINE" }, limit = MAX_ROOMS + 1 })
		if #outlines > MAX_ROOMS then
			q.capped = MAX_ROOMS
		end
		for k, e in outlines do
			if k > MAX_ROOMS then
				break
			end
			if e.closed and #e.points >= 6 then
				local inside = {}
				for _, t in near(grid, e.points) do
					if T.inside(e.points, t.px, t.py) then
						table.insert(inside, t.e)
					end
				end
				table.sort(inside, function(a, b)
					if a.height ~= b.height then
						return a.height > b.height
					end
					return a.id < b.id
				end)
				local name = if inside[1] then first_line(inside[1].text) else ("房間 Room " .. (#q.rooms + 1))
				local finish = "wood"
				for _, t in inside do
					if is_tile(t.text) then
						finish = "tile"
					end
				end
				local m = dwg:measure({ e.id })
				local boxes = {}
				for _, t in inside do
					if t.bounds then
						table.insert(boxes, t.bounds)
					end
				end
				table.insert(q.rooms, { id = e.id, name = name, finish = finish, area = m.area * s * s, points = e.points, labels = boxes })
			end
		end
	end
	-- walls drawn as centre lines (wide polylines or lines): a closed or
	-- 180+ wide outline is exterior, the rest are partitions
	if #c.walls > 0 then
		local thick = 0.18 / s
		for _, e in dwg:entities({ layer = c.walls, type = { "LWPOLYLINE", "POLYLINE", "LINE", "ARC" }, limit = 20000 }) do
			local ext = e.closed == true or (e.width or 0) >= thick
			table.insert(if ext then q.ext.ids else q.int.ids, e.id)
		end
		if #q.ext.ids > 0 then
			q.ext.length = dwg:measure(q.ext.ids).length * s
		end
		if #q.int.ids > 0 then
			q.int.length = dwg:measure(q.int.ids).length * s
		end
	end
	-- doors and windows: block inserts on their layers, or blocks named so
	for _, e in dwg:entities({ type = "INSERT", limit = 20000 }) do
		local role = c.role[e.layer]
		local b = string.upper(e.block or "")
		if role == "doors" or (role == nil and has(b, DOOR_WORDS)) then
			table.insert(q.doors.ids, e.id)
		elseif role == "windows" or (role == nil and has(b, WINDOW_WORDS)) then
			table.insert(q.windows.ids, e.id)
		end
	end
	return q
end

-- ---------------------------------------------------------------- the workbook

-- A workbook with the Prices sheet and an empty Takeoff sheet.
function T.new_book()
	local wb = sheet.new({ name = "studio-takeoff" })
	wb = sheet.rename_sheet(wb, 1, "Takeoff")
	wb = sheet.add_sheet(wb, "Prices")
	local rows = { { "Key", "項目 Item", "單位 Unit", "單價 Unit price" } }
	for _, p in T.PRICES do
		table.insert(rows, { p.key, p.label, p.unit, p.price })
	end
	wb = sheet.set_range(wb, "Prices!A1", rows)
	wb = sheet.set_style(wb, "Prices!A1:D1", { bold = true, fill = "#DDE3EA" })
	wb = sheet.set_style(wb, "Prices!D2:D" .. (T.ROW.height - 1), { format = "#,##0" })
	wb = sheet.set_style(wb, "Prices!D" .. T.ROW.height, { format = "0.00" })
	wb = sheet.set_style(wb, "Prices!D" .. T.ROW.tax, { format = "0%" })
	return wb
end

local function price_ref(key)
	return "Prices!$D$" .. T.ROW[key]
end

local function concat(a, b)
	local out = table.clone(a)
	for _, x in b do
		table.insert(out, x)
	end
	return out
end

-- Write quantities `q` into the Takeoff sheet (replacing what was there).
-- Returns the workbook's next version and the row map:
-- { rows = { {row, name, kind, key, ids, points, labels} }, last, sub, tax,
-- total, dwg } (`labels`: drawing-unit boxes of the room's texts).
function T.write(wb, q)
	local rows = { { "項目 Item", "類別 Type", "數量 Qty", "單位 Unit", "單價 Unit price", "金額 Amount" } }
	local meta = { rows = {}, dwg = q.dwg, unit = q.unit, capped = q.capped }
	local function add(name, kind, qty, unit, key, ids, points, labels)
		local r = #rows + 1
		rows[r] = { name, T.BY_KEY[key].label, qty, unit, "=" .. price_ref(key), "=ROUND(C" .. r .. "*E" .. r .. ",0)" }
		table.insert(meta.rows, { row = r, name = name, kind = kind, key = key, ids = ids, points = points, labels = labels })
		return r
	end
	for _, room in q.rooms do
		add(room.name, "room", round(room.area, 2), "m²", room.finish, { room.id }, room.points, room.labels)
	end
	local ext = add("外牆 Exterior walls", "wall", round(q.ext.length, 2), "m", "ext_wall", q.ext.ids)
	local int = add("隔間牆 Partitions", "wall", round(q.int.length, 2), "m", "int_wall", q.int.ids)
	-- both faces of partitions, the inside face of exterior walls
	add("牆面油漆 Wall paint", "paint", "=ROUND((C" .. int .. "*2+C" .. ext .. ")*" .. price_ref("height") .. ",2)", "m²", "paint", concat(q.ext.ids, q.int.ids))
	add("門 Doors", "count", #q.doors.ids, "樘", "door", q.doors.ids)
	add("窗 Windows", "count", #q.windows.ids, "樘", "window", q.windows.ids)
	local last = #rows
	local sub, tax, total = last + 1, last + 2, last + 3
	rows[sub] = { "小計 Subtotal", "", "", "", "", "=SUM(F2:F" .. last .. ")" }
	rows[tax] = { "營業稅 Tax", "", "=" .. price_ref("tax"), "", "", "=ROUND(F" .. sub .. "*C" .. tax .. ",0)" }
	rows[total] = { "總計 Total", "", "", "", "", "=F" .. sub .. "+F" .. tax }
	meta.last, meta.sub, meta.tax, meta.total = last, sub, tax, total

	wb = sheet.clear(wb, "Takeoff!A1:F" .. CLEAR_ROWS)
	wb = sheet.set_range(wb, "Takeoff!A1", rows)
	wb = sheet.set_style(wb, "Takeoff!A1:F1", { bold = true, fill = "#DDE3EA" })
	wb = sheet.set_style(wb, "Takeoff!C2:C" .. last, { format = "#,##0.00" })
	wb = sheet.set_style(wb, "Takeoff!C" .. (last - 1) .. ":C" .. last, { format = "0" })
	wb = sheet.set_style(wb, "Takeoff!E2:F" .. total, { format = "#,##0" })
	wb = sheet.set_style(wb, "Takeoff!C" .. tax, { format = "0%" })
	wb = sheet.set_style(wb, "Takeoff!A" .. sub .. ":F" .. total, { bold = true })
	-- the takeoff is rebuilt, not edited cell by cell: no undo history to keep
	wb = sheet.clear_history(wb)
	return wb, meta
end

local KEYS = "wood, tile, ext_wall, int_wall, paint, door, window, height, tax"

local function trim(s)
	return (string.gsub(s, "^%s*(.-)%s*$", "%1"))
end

-- The price key `item` names: a key ("wood"), a whole label or either half
-- of one ("磁磚", "Floor tiles"), or a part of a label that only one item
-- has ("tiles"). A part that several labels share is refused with the
-- candidates.
function T.find_item(item)
	if type(item) ~= "string" then
		error("item must be a price key or a label (" .. KEYS .. "), got " .. type(item), 0)
	end
	local s = trim(item)
	if T.ROW[s] then
		return s
	end
	local want = string.upper(s)
	if want == "" then
		error("item is empty; one of " .. KEYS, 0)
	end
	for _, p in T.PRICES do
		local zh, en = string.match(p.label, "^(%S+)%s+(.+)$")
		if want == string.upper(p.label) or want == string.upper(p.key) or (zh and want == string.upper(zh)) or (en and want == string.upper(en)) then
			return p.key
		end
	end
	local hits = {}
	for _, p in T.PRICES do
		if string.find(string.upper(p.label), want, 1, true) or string.find(string.upper(p.key), want, 1, true) then
			table.insert(hits, p)
		end
	end
	if #hits == 1 then
		return hits[1].key
	elseif #hits == 0 then
		error("no price item \"" .. s .. "\" (" .. KEYS .. ")", 0)
	end
	local names = {}
	for _, p in hits do
		table.insert(names, p.key .. " (" .. p.label .. ")")
	end
	error("\"" .. s .. "\" matches several items: " .. table.concat(names, ", ") .. "; give the key", 0)
end

-- Set one unit price (or the wall height, or the tax rate). `item`: see
-- T.find_item. The tax rate: `unit` "%" or "fraction" says how to read it;
-- without one a value of 1 or more is percent (5 is 5 %, 1 is 1 %) and a
-- smaller one a fraction (0.05 is 5 %). `raw`: the page's own field, which
-- already passes a fraction. Returns wb', key, the stored value.
function T.set_price(wb, item, price, raw, unit)
	local key = T.find_item(item)
	local n = if type(price) == "number" then price elseif type(price) == "string" then tonumber(trim(price)) else nil
	if n == nil or n ~= n or n == math.huge or n < 0 or n > 1e9 then
		error("price must be a number from 0 to 1e9, got " .. tostring(price), 0)
	end
	if unit ~= nil and unit ~= "%" and unit ~= "fraction" then
		error("unit must be \"%\" or \"fraction\" (tax only), got " .. tostring(unit), 0)
	end
	if key == "tax" and not raw then
		if unit == "%" or (unit == nil and n >= 1) then
			n = n / 100
		end
	end
	if key == "tax" and n > 1 then
		error("the tax rate must be at most 100 %", 0)
	elseif key == "height" and n > 100 then
		error("the wall height is in metres: at most 100", 0)
	end
	return sheet.set(wb, "Prices!D" .. T.ROW[key], n), key, n
end

-- ---------------------------------------------------------------- reading back

local function value(v)
	if type(v) == "number" then
		return v
	end
	return nil
end

-- What the workbook computed (pure; the `totals` cell): rows { name, type,
-- qty, unit, price, amount, kind, key, ids }, prices { key -> number },
-- subtotal, tax, rate, total, and errors (cells that are formula errors).
function T.totals(wb, meta)
	if wb == nil or type(meta) ~= "table" or meta.total == nil then
		return nil
	end
	local t = wb:range("Takeoff!A1:F" .. meta.total, { header = true })
	local out = { rows = {}, prices = {}, errors = {} }
	for i, m in meta.rows do
		local r = t.rows[m.row - 1] or {}
		local row = { name = r[1], type = r[2], qty = r[3], unit = r[4], price = r[5], amount = r[6], kind = m.kind, key = m.key, ids = m.ids, row = m.row }
		for _, k in { "qty", "price", "amount" } do
			if type(row[k]) ~= "number" then
				table.insert(out.errors, "Takeoff row " .. m.row .. " " .. k .. ": " .. tostring(row[k]))
				row[k] = nil
			end
		end
		out.rows[i] = row
	end
	for _, p in T.PRICES do
		out.prices[p.key] = value(wb:get("Prices!D" .. T.ROW[p.key]))
	end
	out.subtotal = value(wb:get("Takeoff!F" .. meta.sub))
	out.tax = value(wb:get("Takeoff!F" .. meta.tax))
	out.rate = value(wb:get("Takeoff!C" .. meta.tax))
	out.total = value(wb:get("Takeoff!F" .. meta.total))
	out.refs = { subtotal = "Takeoff!F" .. meta.sub, tax = "Takeoff!F" .. meta.tax, total = "Takeoff!F" .. meta.total }
	out.book = tostring(wb)
	return out
end

-- ---------------------------------------------------------------- formatting

-- 1234567.4 -> "1,234,567"
function T.money(n)
	if type(n) ~= "number" or n ~= n then
		return "—"
	end
	if math.abs(n) >= 1e15 then
		return string.format("%.3e", n)
	end
	local neg = n < 0
	local s = string.format("%d", math.floor(math.abs(n) + 0.5))
	local out = string.reverse((string.gsub(string.reverse(s), "(%d%d%d)", "%1,")))
	if string.sub(out, 1, 1) == "," then
		out = string.sub(out, 2)
	end
	return (if neg then "-" else "") .. out
end

-- A quantity as the takeoff shows it.
function T.qty(n, kind)
	if type(n) ~= "number" then
		return "—"
	end
	if kind == "count" then
		return string.format("%d", n)
	end
	return string.format("%.2f", n)
end

-- A number as typed: no exponent, no thousands separators, at most four
-- decimals and no trailing zeros (28003100 -> "28003100", 2.5 -> "2.5").
function T.plain(n)
	if type(n) ~= "number" or n ~= n then
		return ""
	end
	if math.abs(n) >= 1e15 then
		return string.format("%.0f", n)
	end
	local s = string.format("%.4f", n)
	s = string.gsub(s, "0+$", "")
	s = string.gsub(s, "%.$", "")
	if s == "-0" then
		s = "0"
	end
	return s
end

-- A price as the price fields show it (no thousands separators: editable).
function T.price_text(key, n)
	if type(n) ~= "number" then
		return ""
	end
	if key == "tax" then
		return T.plain(round(n * 100, 4))
	end
	return T.plain(round(n, 4))
end

return T
