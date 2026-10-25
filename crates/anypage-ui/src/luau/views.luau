-- ui/views: scroll containers, virtualised lists and tables.
--
-- Scroll offsets live in the app's UI state (u.scroll[id] vertical,
-- u.scrollx[id] horizontal), so scrolling is a state change like any other
-- and a re-render reads the offset as an input. Lists and tables build and
-- draw only the rows inside their viewport: 100 000 rows cost what 20 do.

local U = import("ui/util")
local L = import("ui/layout")
local Theme = import("ui/theme")

local T = L.types
local max, min, floor = math.max, math.min, math.floor
local huge = math.huge

local V = {}

local BAR_HIT = 14 -- width of the strip along the right edge that grabs the scrollbar

local function get(u, field, id)
	local s = u[field]
	return (type(s) == "table" and tonumber(s[id])) or 0
end
V.get_scroll = function(u, id)
	return get(u, "scroll", id)
end

local function put(u, field, id, v)
	if type(u[field]) ~= "table" then
		u[field] = {}
	end
	u[field][id] = if v ~= 0 then v else nil
end

-- Scroll `el` to (sy, sx) (sx may be nil), clamped. Returns true if it moved.
function V.scroll_to(el, app, sy, sx)
	local u = app:ui_get()
	local cy, cxx = get(u, "scroll", el.id), get(u, "scrollx", el.id)
	local ny = U.clamp(sy or cy, 0, max(0, (el._ch or 0) - (el._vh or 0)))
	local nx = cxx
	if sx ~= nil then
		nx = U.clamp(sx, 0, max(0, (el._cw or 0) - (el._vw or 0)))
	end
	if math.abs(ny - cy) < 0.01 and math.abs(nx - cxx) < 0.01 then
		return false
	end
	put(u, "scroll", el.id, ny)
	put(u, "scrollx", el.id, nx)
	app:ui_set(u)
	return true
end

-- Wheel for any scroller that records _ch/_vh (and _cw/_vw), _sy (and _sx).
function V.wheel(el, app, dx, dy)
	local ch, vh = el._ch or 0, el._vh or 0
	local cw, vw = el._cw or 0, el._vw or 0
	local can_y, can_x = ch > vh + 0.5, cw > vw + 0.5
	if not can_y and not can_x then
		return nil
	end
	local step = app.theme.wheel_step
	local sy = (el._sy or 0) - dy * step
	local sx = nil
	if can_x then
		sx = (el._sx or 0) - dx * step
		if not can_y then
			sx -= dy * step -- a plain wheel scrolls a horizontal-only view sideways
		end
	end
	if V.scroll_to(el, app, sy, sx) then
		return "moved"
	end
	return "edge"
end

-- Scrollbar thumbs, drawn over the content.
function V.bars(el, dc, x, y, w, h)
	local th, d = dc.th, dc.d
	local ch, vh = el._ch or 0, el._vh or 0
	local bw = th.scrollbar - 2
	if ch > vh + 0.5 then
		local track = h - 6
		local t_h = max(24, track * vh / ch)
		local ty = y + 3 + (track - t_h) * (el._sy or 0) / (ch - vh)
		d:rrect(x + w - bw - 3, ty, bw, t_h, bw / 2, th.thumb)
	end
	local cw, vw = el._cw or 0, el._vw or 0
	if cw > vw + 0.5 then
		local track = w - 6
		local t_w = max(24, track * vw / cw)
		local tx = x + 3 + (track - t_w) * (el._sx or 0) / (cw - vw)
		d:rrect(tx, y + h - bw - 3, t_w, bw, bw / 2, th.thumb)
	end
end

-- Pointer down on a scroller's own area: grab a scrollbar or drag the content.
local function scroller_down(el, app, x, y, dr, top)
	top = top or el.y
	dr.sy0, dr.sx0 = el._sy or 0, el._sx or 0
	if (el._ch or 0) > (el._vh or 0) + 0.5 and x >= el.x + el.w - BAR_HIT then
		dr.kind = "vthumb"
		return "captured"
	end
	if (el._cw or 0) > (el._vw or 0) + 0.5 and y >= el.y + el.h - BAR_HIT then
		dr.kind = "hthumb"
		return "captured"
	end
	dr.kind = "scroll"
	return "captured"
end
V.scroller_down = scroller_down

-- Drag for scrollers: content drag ("scroll") or thumb drag.
function V.scroller_drag(el, app, x, y, dr)
	local ch, vh = el._ch or 0, el._vh or 0
	local cw, vw = el._cw or 0, el._vw or 0
	if dr.kind == "scroll" then
		V.scroll_to(el, app, dr.sy0 - (y - dr.y0), if cw > vw then dr.sx0 - (x - dr.x0) else nil)
	elseif dr.kind == "vthumb" and ch > vh then
		local track = (el._vh or el.h) - 6
		local t_h = max(24, track * vh / ch)
		V.scroll_to(el, app, dr.sy0 + (y - dr.y0) * (ch - vh) / max(1, track - t_h), nil)
	elseif dr.kind == "hthumb" and cw > vw then
		local track = el.w - 6
		local t_w = max(24, track * vw / cw)
		V.scroll_to(el, app, nil, dr.sx0 + (x - dr.x0) * (cw - vw) / max(1, track - t_w))
	end
end

-- ---------------------------------------------------------------- scroll

T.scroll = {
	fill = true,
	shrink_y = true,
	scroller = true,
	measure = function(el, aw, ah, cx)
		local pt, pr, pb, pl = U.pads(el.padding)
		local kid = el.children[1]
		local cw, ch = 0, 0
		if kid then
			kid = L.adopt(el, kid, 1, cx)
			kid._axis = "column"
			local avail = if el.horizontal then huge else max(0, aw - pl - pr)
			cw, ch = L.measure(kid, avail, huge, cx)
		end
		local w = if aw < huge then aw else cw + pl + pr
		local h = ch + pt + pb
		if ah < huge then
			h = min(h, ah)
		end
		return w, h
	end,
	arrange = function(el, x, y, w, h, cx)
		local kid = el.children[1]
		el._vw, el._vh = w, h
		if kid == nil then
			el._cw, el._ch, el._sx, el._sy = 0, 0, 0, 0
			return
		end
		local pt, pr, pb, pl = U.pads(el.padding)
		local avail = if el.horizontal then huge else max(0, w - pl - pr)
		if kid._aw ~= avail then
			L.measure(kid, avail, huge, cx)
		end
		el._cw = if el.horizontal then kid._mw + pl + pr else w
		el._ch = kid._mh + pt + pb
		local u = cx.u
		el._sy = U.clamp(get(u, "scroll", el.id), 0, max(0, el._ch - h))
		el._sx = U.clamp(get(u, "scrollx", el.id), 0, max(0, el._cw - w))
		local kw = if el.horizontal then kid._mw else max(0, w - pl - pr)
		L.arrange(kid, x + pl - el._sx, y + pt - el._sy, kw, max(kid._mh, h - pt - pb), cx)
	end,
	draw = function(el, dc)
		L.draw_box(el, dc)
		L.push_clip(dc, el.x, el.y, el.w, el.h)
	end,
	post = function(el, dc)
		L.pop_clip(dc)
		V.bars(el, dc, el.x, el.y, el.w, el.h)
	end,
	wheel = V.wheel,
	down = function(el, app, x, y, dr)
		return scroller_down(el, app, x, y, dr)
	end,
	drag = V.scroller_drag,
	describe = function(el)
		return { scroll_y = el._sy, content_h = el._ch }
	end,
}

T.textarea.wheel = V.wheel

-- ---------------------------------------------------------------- list

local function list_count(el)
	if el.items ~= nil then
		return #el.items
	end
	return U.clamp(floor(tonumber(el.count) or 0), 0, 10000000)
end

local function list_item(el, i)
	if el.items ~= nil then
		return el.items[i]
	end
	if el.item then
		return el.item(i)
	end
	return i
end
V.list_item = list_item

-- Horizontal room a row's content leaves: padding, plus the scrollbar's lane.
local function row_inset(el, cx)
	local list = el._parent
	if list and (list._ch or 0) > (list._vh or 0) then
		return 22 + cx.theme.scrollbar
	end
	return 20
end

-- A list row: the page's row element inside a fixed-height band.
T._row = {
	measure = function(el, aw, ah, cx)
		local kid = L.adopt(el, el.children[1], 1, cx)
		kid._axis = "column"
		L.measure(kid, max(0, aw - row_inset(el, cx)), ah, cx)
		return aw, ah
	end,
	arrange = function(el, x, y, w, h, cx)
		local kid = el.children[1]
		local iw = max(0, w - row_inset(el, cx))
		if kid._aw ~= iw then
			L.measure(kid, iw, h, cx)
		end
		local kw = if kid.fill == false then min(kid._mw, iw) else iw
		local kh = min(kid._mh, h)
		L.arrange(kid, x + 10, y + (h - kh) / 2, kw, kh, cx)
	end,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local list = el._parent
		local bg = nil
		if list.selected == el.index then
			bg = th.row_selected
		elseif dc.hover == el.id then
			bg = th.row_hover
		elseif list.striped and el.index % 2 == 0 then
			bg = th.row_stripe
		end
		if bg then
			d:rect(el.x, el.y, el.w, el.h, bg)
		end
		if list.selected == el.index and dc.focus == list.id then
			d:rect(el.x, el.y, 3, el.h, th.accent)
		end
	end,
}

local function list_select(el, app, i, scroll_only)
	local n = el._n
	if n == 0 then
		return
	end
	i = U.clamp(i, 1, n)
	local rh = el._rh
	local u = app:ui_get()
	local sy = get(u, "scroll", el.id)
	local top = (i - 1) * rh
	if top < sy then
		sy = top
	elseif top + rh > sy + el._vh then
		sy = top + rh - el._vh
	end
	V.scroll_to(el, app, sy, nil)
	if not scroll_only and i ~= el.selected then
		app:call(el.on_select, list_item(el, i), i)
	end
end
V.list_select = list_select

local function nav_target(el, base)
	local n = el._n
	local cur = tonumber(el.selected) or 0
	local page_rows = max(1, floor(el._vh / el._rh) - 1)
	if base == "up" then
		return if cur <= 0 then 1 else cur - 1
	elseif base == "down" then
		return cur + 1
	elseif base == "pageup" then
		return cur - page_rows
	elseif base == "pagedown" then
		return if cur <= 0 then page_rows else cur + page_rows
	elseif base == "home" then
		return 1
	elseif base == "end" then
		return n
	end
	return nil
end

T.list = {
	focusable = true,
	fill = true,
	shrink_y = true,
	scroller = true,
	own_focus = true,
	check = function(el)
		L.need(el, "items", "table", nil, "a list")
		L.need(el, "count", "number")
		L.need(el, "row_height", "number")
		L.need(el, "rows", "number")
		L.need(el, "selected", "number")
		L.need(el, "empty", "string")
		L.need_fns(el, "item", "row", "key", "on_select", "on_activate")
	end,
	measure = function(el, aw, ah, cx)
		local th = cx.theme
		local rh = el.row_height or th.row_h
		local n = list_count(el)
		el._n, el._rh = n, rh
		local h = el.height
		if h == nil then
			h = min(max(n, 1), el.rows or 10) * rh
			if ah < huge then
				h = min(h, ah)
			end
		end
		return if aw < huge then aw else 280, h
	end,
	arrange = function(el, x, y, w, h, cx)
		local n, rh = el._n, el._rh
		el._vw, el._vh = w, h
		el._cw, el._ch = w, n * rh
		local sy = U.clamp(get(cx.u, "scroll", el.id), 0, max(0, n * rh - h))
		el._sy = sy
		local rows = {}
		el.children = rows
		if n == 0 then
			return
		end
		local i0 = floor(sy / rh) + 1
		local i1 = min(n, floor((sy + h - 0.001) / rh) + 1)
		local sel = el.selected
		local hover = cx.u.hover
		local keyf, rowf = el.key, el.row
		local fs = cx.theme.font
		for i = i0, i1 do
			local item = list_item(el, i)
			local key = if keyf then keyf(item, i) else i
			local rid = el.id .. "#" .. tostring(key)
			local content
			if rowf then
				content = rowf(item, i, sel == i, hover == rid)
			else
				content = { type = "text", text = U.label_of(item), size = fs, wrap = false }
			end
			if type(content) == "string" then
				content = { type = "text", text = content, wrap = false }
			elseif type(content) ~= "table" or content.type == nil then
				content = { type = "column", children = {} }
			end
			local row = { type = "_row", id = rid, index = i, item = item, children = { content }, _parent = el, _id_set = true }
			cx.byid[rid] = row
			L.measure(row, w, rh, cx)
			L.arrange(row, x, y + (i - 1) * rh - sy, w, rh, cx)
			rows[#rows + 1] = row
		end
	end,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local r = el.radius or th.radius_small
		d:rrect(el.x, el.y, el.w, el.h, r, el.bg or th.surface)
		local focused = dc.focus == el.id
		d:stroke(el.x + 0.5, el.y + 0.5, el.w - 1, el.h - 1, r, if focused and dc.ring then 2 else 1, if focused and dc.ring then th.focus else th.border)
		if el._n == 0 and el.empty then
			d:text(el.x + 12, el.y + 10, th.font, th.text_faint, U.truncate(el.empty, th.font, el.w - 24))
		end
		L.push_clip(dc, el.x + 1, el.y + 1, el.w - 2, el.h - 2)
	end,
	post = function(el, dc)
		L.pop_clip(dc)
		V.bars(el, dc, el.x, el.y, el.w, el.h)
	end,
	wheel = V.wheel,
	down = function(el, app, x, y, dr)
		local r = scroller_down(el, app, x, y, dr)
		if dr.kind == "scroll" then
			local i = floor((y - el.y + (el._sy or 0)) / el._rh) + 1
			if i >= 1 and i <= el._n then
				dr.kind = "row"
				dr.index = i
			end
		end
		return r
	end,
	drag = V.scroller_drag,
	up = function(el, app, x, y, dr)
		if dr.kind == "row" and dr.index then
			list_select(el, app, dr.index)
			if dr.clicks >= 2 then
				app:call(el.on_activate, list_item(el, dr.index), dr.index)
			end
		end
	end,
	key = function(el, app, k, base, ctrl, shift)
		local i = nav_target(el, base)
		if i ~= nil then
			list_select(el, app, i)
			return true
		end
		if (base == "enter" or base == "space") and el.selected then
			app:call(el.on_activate, list_item(el, el.selected), el.selected)
			return true
		end
		return false
	end,
	hover_id = function(el, x, y)
		for _, row in el.children do
			if y >= row.y and y < row.y + row.h then
				return row.id
			end
		end
		return el.id
	end,
	describe = function(el)
		return { count = el._n, selected = el.selected, first_visible = floor((el._sy or 0) / el._rh) + 1, scroll_y = el._sy }
	end,
}

-- ---------------------------------------------------------------- table

local CELL_PAD = 10

local function cell_text(el, v, c, i)
	if el.format then
		local s = el.format(v, c, i)
		if s ~= nil then
			return tostring(s)
		end
	end
	if v == nil then
		return ""
	end
	if type(v) == "number" then
		if v == floor(v) and math.abs(v) < 1e15 then
			return string.format("%d", v)
		end
		return string.format("%.6g", v)
	end
	if type(v) == "boolean" then
		return if v then "TRUE" else "FALSE"
	end
	if type(v) == "table" then
		return U.label_of(v)
	end
	return tostring(v)
end

local function table_col_at(el, x)
	local cx = x - el.x + (el._sx or 0)
	local acc = 0
	for c = 1, #el._ws do
		local w = el._ws[c]
		if cx >= acc and cx < acc + w then
			return c
		end
		acc += w
	end
	return nil
end

local function table_row_at(el, y)
	local by = el.y + el._hh
	if y < by then
		return 0 -- header
	end
	local i = floor((y - by + (el._sy or 0)) / el._rh) + 1
	if i >= 1 and i <= el._n then
		return i
	end
	return nil
end

-- Row i of a table: from data.rows, or from the page's row(i) (lazy tables).
local function table_row(el, i)
	local rows = el._rows
	if rows ~= nil then
		return rows[i]
	end
	return el.row(i)
end
V.table_row = table_row

local function table_select(el, app, i)
	local n = el._n
	if n == 0 then
		return
	end
	i = U.clamp(i, 1, n)
	local rh = el._rh
	local u = app:ui_get()
	local sy = get(u, "scroll", el.id)
	local top = (i - 1) * rh
	if top < sy then
		sy = top
	elseif top + rh > sy + el._vh then
		sy = top + rh - el._vh
	end
	V.scroll_to(el, app, sy, nil)
	if i ~= el.selected then
		app:call(el.on_select, table_row(el, i), i)
	end
end
V.table_select = table_select

T.table = {
	focusable = true,
	fill = true,
	shrink_y = true,
	scroller = true,
	own_focus = true,
	check = function(el)
		L.need(el, "data", "table", nil, "a table { columns = {...}, rows = {{...}, ...} }")
		local data = el.data
		if data ~= nil then
			if data.columns ~= nil and type(data.columns) ~= "table" then
				error("ui.table \"" .. tostring(el.id) .. "\": data.columns must be a list, got " .. type(data.columns), 0)
			end
			if data.rows ~= nil and type(data.rows) ~= "table" then
				error("ui.table \"" .. tostring(el.id) .. "\": data.rows must be a list of rows, got " .. type(data.rows), 0)
			end
		end
		L.need(el, "columns", "table", nil, "a list")
		L.need(el, "widths", "table", nil, "a list of numbers")
		L.need(el, "count", "number")
		L.need(el, "row_height", "number")
		L.need(el, "header_height", "number")
		L.need(el, "rows", "number")
		L.need(el, "selected", "number")
		L.need(el, "sort", "table", nil, "{ col, desc }")
		L.need_fns(el, "row", "format", "on_select", "on_activate", "on_sort")
		if el.row ~= nil and el.count == nil then
			error("ui.table \"" .. tostring(el.id) .. "\": row(i) needs count", 0)
		end
	end,
	measure = function(el, aw, ah, cx)
		local th = cx.theme
		local data = el.data
		if type(data) ~= "table" then
			data = {}
		end
		local cols = if el.columns ~= nil then el.columns else data.columns
		el._cols = if type(cols) == "table" then cols else {}
		if el.row ~= nil then
			-- lazy: count + row(i), rows built only when shown
			el._rows = nil
			el._n = U.clamp(floor(tonumber(el.count) or 0), 0, 10000000)
		else
			el._rows = if type(data.rows) == "table" then data.rows else {}
			el._n = #el._rows
		end
		el._rh = el.row_height or th.row_h
		el._hh = el.header_height or (th.row_h + 4)
		el._fs = el.size or th.font_small + 1
		local h = el.height
		if h == nil then
			h = el._hh + min(max(el._n, 1), el.rows or 10) * el._rh
			if ah < huge then
				h = min(h, ah)
			end
		end
		return if aw < huge then aw else 480, h
	end,
	arrange = function(el, x, y, w, h, cx)
		local ncol = #el._cols
		local ws = {}
		local given = el.widths
		local free, nfree = w, 0
		for c = 1, ncol do
			local gw = if type(given) == "table" then tonumber(given[c]) else nil
			if gw then
				if gw <= 1 then
					gw *= w
				end
				ws[c] = gw
				free -= gw
			else
				nfree += 1
			end
		end
		local each = if nfree > 0 then max(el.min_col or 72, free / nfree) else 0
		local total = 0
		for c = 1, ncol do
			ws[c] = ws[c] or each
			total += ws[c]
		end
		el._ws, el._tw = ws, total
		el._vw, el._vh = w, max(0, h - el._hh)
		el._cw, el._ch = total, el._n * el._rh
		el._sy = U.clamp(get(cx.u, "scroll", el.id), 0, max(0, el._ch - el._vh))
		el._sx = U.clamp(get(cx.u, "scrollx", el.id), 0, max(0, total - w))
	end,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local r = el.radius or th.radius_small
		local focused = dc.focus == el.id and dc.ring
		d:rrect(el.x, el.y, el.w, el.h, r, el.bg or th.surface)
		local fs = el._fs
		local lh = U.line_height(fs)
		local cols, ws = el._cols, el._ws
		local sx, sy = el._sx, el._sy
		local hh, rh = el._hh, el._rh
		local aligns = el.align_cols
		L.push_clip(dc, el.x + 1, el.y + 1, el.w - 2, el.h - 2)
		-- header
		d:rect(el.x, el.y, el.w, hh, th.surface2)
		local sort = el.sort
		local x = el.x - sx
		for c = 1, #cols do
			local w = ws[c]
			if x + w > el.x and x < el.x + el.w then
				local title = U.label_of(cols[c])
				local tw = w - 2 * CELL_PAD
				local sorted = type(sort) == "table" and sort.col == c
				if sorted then
					tw -= 14
				end
				d:text(x + CELL_PAD, el.y + (hh - lh) / 2, fs, th.text_dim, U.truncate(title, fs, tw))
				if sorted then
					local ax, ay = x + w - CELL_PAD - 5, el.y + hh / 2
					if sort.desc then
						d:line({ ax - 4, ay - 2, ax, ay + 2, ax + 4, ay - 2 }, 1.6, th.text_dim)
					else
						d:line({ ax - 4, ay + 2, ax, ay - 2, ax + 4, ay + 2 }, 1.6, th.text_dim)
					end
				end
				if dc.hover == el.id .. ":h" .. c then
					d:rect(x, el.y + hh - 2, w, 2, th.accent_soft)
				end
				d:rect(x + w - 1, el.y + 6, 1, hh - 12, th.border)
			end
			x += w
		end
		d:rect(el.x, el.y + hh - 1, el.w, 1, th.border)
		-- body
		local by = el.y + hh
		L.push_clip(dc, el.x + 1, by, el.w - 2, el.h - hh - 1)
		local n = el._n
		if n > 0 then
			local i0 = floor(sy / rh) + 1
			local i1 = min(n, floor((sy + el._vh - 0.001) / rh) + 1)
			for i = i0, i1 do
				local ry = by + (i - 1) * rh - sy
				local bg = nil
				if el.selected == i then
					bg = th.row_selected
				elseif dc.hover == el.id .. "#" .. i then
					bg = th.row_hover
				elseif el.striped ~= false and i % 2 == 0 then
					bg = th.row_stripe
				end
				if bg then
					d:rect(el.x, ry, el.w, rh, bg)
				end
				if el.selected == i and dc.focus == el.id then
					d:rect(el.x, ry, 3, rh, th.accent)
				end
				local row = table_row(el, i)
				if type(row) == "table" then
					local cxp = el.x - sx
					for c = 1, #cols do
						local w = ws[c]
						if cxp + w > el.x and cxp < el.x + el.w then
							local v = row[c]
							local s = cell_text(el, v, c, i)
							if s ~= "" then
								s = U.truncate(s, fs, w - 2 * CELL_PAD)
								local a = if type(aligns) == "table" then aligns[c] else nil
								if a == nil and type(v) == "number" then
									a = "end"
								end
								local tx = cxp + CELL_PAD
								if a == "end" or a == "right" then
									tx = cxp + w - CELL_PAD - U.width(s, fs)
								elseif a == "center" then
									tx = cxp + (w - U.width(s, fs)) / 2
								end
								d:text(tx, ry + (rh - lh) / 2, fs, th.text, s)
							end
						end
						cxp += w
					end
				end
			end
		elseif el.empty then
			d:text(el.x + CELL_PAD, by + 10, th.font, th.text_faint, U.truncate(el.empty, th.font, el.w - 2 * CELL_PAD))
		end
		L.pop_clip(dc)
		L.pop_clip(dc)
		V.bars(el, dc, el.x, by, el.w, el.h - hh)
		d:stroke(el.x + 0.5, el.y + 0.5, el.w - 1, el.h - 1, r, if focused then 2 else 1, if focused then th.focus else th.border)
	end,
	wheel = V.wheel,
	down = function(el, app, x, y, dr)
		local i = table_row_at(el, y)
		if i == 0 then
			dr.kind = "header"
			dr.col = table_col_at(el, x)
			return "captured"
		end
		local r = scroller_down(el, app, x, y, dr)
		if dr.kind == "scroll" and i then
			dr.kind = "row"
			dr.index = i
		end
		return r
	end,
	drag = V.scroller_drag,
	up = function(el, app, x, y, dr)
		if dr.kind == "header" then
			if dr.col and table_row_at(el, y) == 0 and table_col_at(el, x) == dr.col then
				local sort = el.sort
				local desc = type(sort) == "table" and sort.col == dr.col and not sort.desc
				app:call(el.on_sort, dr.col, desc)
			end
		elseif dr.kind == "row" and dr.index then
			table_select(el, app, dr.index)
			if dr.clicks >= 2 then
				app:call(el.on_activate, table_row(el, dr.index), dr.index)
			end
		end
	end,
	key = function(el, app, k, base)
		local i = nav_target(el, base)
		if i ~= nil then
			table_select(el, app, i)
			return true
		end
		if base == "left" or base == "right" then
			V.scroll_to(el, app, nil, (el._sx or 0) + (if base == "left" then -1 else 1) * app.theme.wheel_step)
			return true
		end
		if (base == "enter" or base == "space") and el.selected then
			app:call(el.on_activate, table_row(el, el.selected), el.selected)
			return true
		end
		return false
	end,
	hover_id = function(el, x, y)
		local i = table_row_at(el, y)
		if i == 0 then
			local c = table_col_at(el, x)
			return if c then el.id .. ":h" .. c else el.id
		end
		return if i then el.id .. "#" .. i else el.id
	end,
	describe = function(el)
		local cols = {}
		for c, v in el._cols do
			cols[c] = U.label_of(v)
		end
		return { columns = cols, rows = el._n, selected = el.selected, scroll_y = el._sy, first_visible = floor((el._sy or 0) / el._rh) + 1 }
	end,
}

return V
