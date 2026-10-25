-- ui/layout: the element type registry and the layout engine.
--
-- An element is a plain table { type = "...", ...props, children = {...} }.
-- A frame runs three passes over the tree the view returned:
--   measure(el, avail_w, avail_h)  natural size within the space offered
--   arrange(el, x, y, w, h)        final rectangle (app-local), children placed
--   draw(el, dc)                   display-list commands, culled by the clip
-- Hit testing walks the arranged tree; an element is hit only if every
-- ancestor's rectangle contains the point, which makes scroll clipping free.
--
-- Sizing props on any element: width, height (fixed), min_w, max_w, min_h,
-- max_h, grow (share of the parent's free main-axis space), fill (stretch on
-- the parent's cross axis), align_self, shrink (false: never shrink in a row).
-- Containers: padding (n | {v, h} | {t, r, b, l}), gap, align
-- ("start" | "center" | "end" | "stretch", cross axis), justify ("start" |
-- "center" | "end" | "between" | "around", main axis), bg, radius, border.

local U = import("ui/util")
local Theme = import("ui/theme")

local huge = math.huge
local floor = math.floor
local max, min = math.max, math.min

local L = {}
local T = {}
L.types = T

-- ---------------------------------------------------------------- tree helpers

local function add_kids(list, out, depth)
	if depth > 8 then
		error("ui: children nested in plain lists more than 8 deep", 0)
	end
	-- holes (nil, false from `cond and el`) are skipped, so find the last index
	local n = 0
	for k in list do
		if type(k) == "number" and k > n and k == floor(k) then
			n = k
		end
	end
	for i = 1, n do
		local k = list[i]
		if type(k) == "table" then
			if k.type ~= nil then
				out[#out + 1] = k
			else
				add_kids(k, out, depth + 1)
			end
		elseif type(k) == "string" then
			out[#out + 1] = { type = "text", text = k }
		end
	end
	return out
end

-- Children as given by a page: a list of elements, possibly with holes, plain
-- strings (shown as text) and nested plain lists (spliced in).
function L.kids(list)
	if list == nil then
		return {}
	end
	if type(list) ~= "table" then
		error("ui: children must be a list of elements, got " .. type(list), 0)
	end
	if list.type ~= nil then
		return { list }
	end
	return add_kids(list, {}, 0)
end

-- A shallow copy of an element without its layout bookkeeping, for element
-- tables a view reuses (a constant header, the same divider twice).
function L.fresh(k)
	local c = table.clone(k)
	c._parent, c._id_set, c._seen, c._aw = nil, nil, nil, nil
	if c.children then
		c.children = table.clone(c.children)
	end
	return c
end

-- Give `k` (child `i` of `parent`) its id and parent link. An element table
-- that appears twice in a tree is copied so each place has its own rectangle.
function L.adopt(parent, k, i, cx)
	if k._parent ~= nil and k._parent ~= parent then
		k = L.fresh(k)
		parent.children[i] = k
	end
	k._parent = parent
	if k._id_set == nil then
		local id = k.id
		if id == nil then
			if k.key ~= nil then
				id = parent.id .. "/" .. tostring(k.key)
			else
				id = parent.id .. "/" .. i
			end
		end
		id = tostring(id)
		local byid = cx.byid
		if byid[id] ~= nil and byid[id] ~= k then
			local n = 2
			while byid[id .. "~" .. n] ~= nil do
				n += 1
			end
			id = id .. "~" .. n
		end
		byid[id] = k
		k.id = id
		k._id_set = true
	end
	return k
end

-- ---------------------------------------------------------------- prop checks
--
-- Wrong prop types are reported naming the element and the prop, instead of
-- surfacing later as a bare Luau error ("attempt to iterate over a string").

local WANT = { number = "a number", table = "a table", ["function"] = "a function", string = "a string", boolean = "true or false" }

-- Error unless el[prop] is nil or of type t1 (or t2).
function L.need(el, prop, t1, t2, want)
	local v = el[prop]
	if v == nil then
		return
	end
	local tv = type(v)
	if tv == t1 or tv == t2 then
		return
	end
	local got = tv
	if tv == "string" then
		got = "string \"" .. (if #v > 24 then string.sub(v, 1, U.utf8_cut(v, 24)) .. "…" else v) .. "\""
	elseif tv == "number" or tv == "boolean" then
		got = tv .. " " .. tostring(v)
	end
	error("ui." .. tostring(el.type) .. " \"" .. tostring(el.id) .. "\": " .. prop .. " must be " .. (want or WANT[t1] or t1) .. ", got " .. got, 0)
end

-- Several props that must be functions (callbacks).
function L.need_fns(el, ...)
	for i = 1, select("#", ...) do
		local k = select(i, ...)
		local v = el[k]
		if v ~= nil and type(v) ~= "function" then
			L.need(el, k, "function")
		end
	end
end

local SIZE_PROPS = { "width", "height", "min_w", "max_w", "min_h", "max_h", "grow", "gap" }

-- Sizing props: a number, or nil / false for none.
local function check_common(el)
	local a, b, c, d, e, f, g, h, p = el.width, el.height, el.min_w, el.max_w, el.min_h, el.max_h, el.grow, el.gap, el.padding
	if
		(not a or type(a) == "number")
		and (not b or type(b) == "number")
		and (not c or type(c) == "number")
		and (not d or type(d) == "number")
		and (not e or type(e) == "number")
		and (not f or type(f) == "number")
		and (not g or type(g) == "number")
		and (not h or type(h) == "number")
		and (p == nil or type(p) == "number")
	then
		return
	end
	for _, k in SIZE_PROPS do
		if el[k] ~= false then
			L.need(el, k, "number")
		end
	end
	L.need(el, "padding", "number", "table", "a number or a list of 2 or 4 numbers")
	if type(p) == "table" then
		for k, v in p do
			if type(v) ~= "number" then
				error("ui." .. tostring(el.type) .. " \"" .. tostring(el.id) .. "\": padding." .. tostring(k) .. " must be a number, got " .. type(v), 0)
			end
		end
	end
end

function L.measure(el, aw, ah, cx)
	local ty = T[el.type]
	if ty == nil then
		error("ui: unknown element type \"" .. tostring(el.type) .. "\" (id " .. tostring(el.id) .. ")", 0)
	end
	if not el._seen then
		el._seen = true
		cx.count += 1
		if cx.count > cx.limit then
			error("ui: the view has more than " .. cx.limit .. " elements; use ui.list or ui.table for long collections", 0)
		end
		check_common(el)
		if ty.check then
			ty.check(el)
		end
	end
	local fw, fh = el.width, el.height
	local w0 = fw or aw
	if el.max_w and el.max_w < w0 then
		w0 = el.max_w
	end
	local h0 = fh or ah
	if el.max_h and el.max_h < h0 then
		h0 = el.max_h
	end
	local w, h = ty.measure(el, max(0, w0), max(0, h0), cx)
	if fw then
		w = fw
	end
	if fh then
		h = fh
	end
	if el.min_w and w < el.min_w then
		w = el.min_w
	end
	if el.max_w and w > el.max_w then
		w = el.max_w
	end
	if el.min_h and h < el.min_h then
		h = el.min_h
	end
	if el.max_h and h > el.max_h then
		h = el.max_h
	end
	el._aw = aw
	el._mw, el._mh = max(0, w), max(0, h)
	return el._mw, el._mh
end

function L.arrange(el, x, y, w, h, cx)
	el.x, el.y, el.w, el.h = x, y, w, h
	local ty = T[el.type]
	if ty.arrange then
		ty.arrange(el, x, y, w, h, cx)
	end
end

-- The width a parent gives `k` when it offers `w`: a fixed width wins, then
-- max_w and min_w.
local function final_w(k, w)
	if k.width then
		return k.width
	end
	if k.max_w and w > k.max_w then
		w = k.max_w
	end
	if k.min_w and w < k.min_w then
		w = k.min_w
	end
	return w
end
L.final_w = final_w

local function final_h(k, h)
	if k.height then
		return k.height
	end
	if k.max_h and h > k.max_h then
		h = k.max_h
	end
	if k.min_h and h < k.min_h then
		h = k.min_h
	end
	return h
end
L.final_h = final_h

-- Should `k` stretch across its column parent?
local function fills_width(k, align)
	if k.fill ~= nil then
		return k.fill
	end
	if align == "stretch" then
		return true
	end
	if align == "center" or align == "end" then
		return false
	end
	return T[k.type].fill == true
end

local function fills_height(k, align)
	if k.fill ~= nil then
		return k.fill
	end
	if align == "stretch" then
		return true
	end
	return T[k.type].fill_h == true
end

local function shrinkable_x(k)
	if k.shrink ~= nil then
		return k.shrink ~= false
	end
	if k.width then
		return false
	end
	return not T[k.type].rigid
end

local function shrinkable_y(k)
	if k.shrink ~= nil then
		return k.shrink ~= false
	end
	if k.height then
		return false
	end
	return T[k.type].shrink_y == true
end

-- ---------------------------------------------------------------- containers

local function draw_box(el, dc)
	local d = dc.d
	if el.shadow then
		d:rrect(el.x, el.y + 2, el.w, el.h, el.radius or 0, dc.th.shadow)
	end
	if el.bg then
		d:rrect(el.x, el.y, el.w, el.h, el.radius or 0, el.bg)
	end
	if el.border then
		local c = if type(el.border) == "number" and el.border > 255 then el.border else dc.th.border
		d:stroke(el.x + 0.5, el.y + 0.5, el.w - 1, el.h - 1, el.radius or 0, 1, c)
	end
end
L.draw_box = draw_box

local function column_measure(el, aw, ah, cx)
	local pt, pr, pb, pl = U.pads(el.padding)
	local iw = max(0, aw - pl - pr)
	local kids = el.children
	local n = #kids
	local gap = el.gap or 0
	local mw, sh = 0, 0
	for i = 1, n do
		local k = L.adopt(el, kids[i], i, cx)
		k._axis = "column"
		local w, h = L.measure(k, iw, huge, cx)
		if w > mw then
			mw = w
		end
		sh += h
	end
	if n > 1 then
		sh += gap * (n - 1)
	end
	return mw + pl + pr, sh + pt + pb
end

local function column_arrange(el, x, y, w, h, cx)
	local pt, pr, pb, pl = U.pads(el.padding)
	local ix, iy = x + pl, y + pt
	local iw, ih = max(0, w - pl - pr), max(0, h - pt - pb)
	local kids = el.children
	local n = #kids
	if n == 0 then
		return
	end
	local gap = el.gap or 0
	local align = el.align
	local total, grow = gap * (n - 1), 0
	for i = 1, n do
		local k = kids[i]
		if k._aw ~= iw then
			L.measure(k, iw, huge, cx)
		end
		total += k._mh
		if (k.grow or 0) > 0 and not k.height then
			grow += k.grow
		end
	end
	local extra = ih - total
	local heights = table.create(n, 0)
	for i = 1, n do
		heights[i] = kids[i]._mh
	end
	local cy, between = iy, gap
	if extra > 0 and grow > 0 then
		for i = 1, n do
			local k = kids[i]
			if (k.grow or 0) > 0 and not k.height then
				local kh = heights[i] + extra * k.grow / grow
				if k.max_h and kh > k.max_h then
					kh = k.max_h
				end
				heights[i] = kh
			end
		end
	elseif extra > 0 then
		local j = el.justify
		if j == "center" then
			cy += extra / 2
		elseif j == "end" then
			cy += extra
		elseif j == "between" and n > 1 then
			between = gap + extra / (n - 1)
		elseif j == "around" then
			local s = extra / n
			cy += s / 2
			between = gap + s
		end
	elseif extra < 0 then
		-- overflow: shrink scrollers, lists and text areas down to their minimum
		local pool = 0
		for i = 1, n do
			local k = kids[i]
			if shrinkable_y(k) then
				pool += max(0, heights[i] - (k.min_h or 0))
			end
		end
		if pool > 0 then
			local deficit = min(-extra, pool)
			for i = 1, n do
				local k = kids[i]
				if shrinkable_y(k) then
					local room = max(0, heights[i] - (k.min_h or 0))
					heights[i] -= deficit * room / pool
				end
			end
		end
	end
	for i = 1, n do
		local k = kids[i]
		local kh = final_h(k, heights[i])
		local kw = final_w(k, if fills_width(k, align) then iw else min(k._mw, iw))
		local kx = ix
		local a = k.align_self or align
		if a == "center" then
			kx = ix + (iw - kw) / 2
		elseif a == "end" then
			kx = ix + iw - kw
		end
		L.arrange(k, kx, cy, kw, kh, cx)
		cy += kh + between
	end
end

-- Split a row's inner width among its children; leaves each child's width in
-- k._rw and returns the width used and the tallest child.
local function row_layout(el, iw, cx)
	local kids = el.children
	local n = #kids
	local gap = el.gap or 0
	local used = gap * max(0, n - 1)
	local grow = 0
	for i = 1, n do
		local k = L.adopt(el, kids[i], i, cx)
		k._axis = "row"
		if (k.grow or 0) > 0 and not k.width and iw < huge then
			grow += k.grow
			k._rw = nil
		else
			local w = L.measure(k, iw, huge, cx)
			k._rw = w
			used += w
		end
	end
	if used > iw and iw < huge then
		-- too wide: shrink what can wrap or scroll, in proportion to its width
		local pool = 0
		for i = 1, n do
			local k = kids[i]
			if k._rw and shrinkable_x(k) then
				pool += max(0, k._rw - (k.min_w or 0))
			end
		end
		if pool > 0 then
			local deficit = min(used - iw, pool)
			for i = 1, n do
				local k = kids[i]
				if k._rw and shrinkable_x(k) then
					local room = max(0, k._rw - (k.min_w or 0))
					local nw = k._rw - deficit * room / pool
					L.measure(k, nw, huge, cx)
					used -= k._rw - nw
					k._rw = nw
				end
			end
		end
	end
	if grow > 0 then
		local rest = max(0, iw - used)
		for i = 1, n do
			local k = kids[i]
			if k._rw == nil then
				local share = rest * k.grow / grow
				if k.min_w and share < k.min_w then
					share = k.min_w
				end
				if k.max_w and share > k.max_w then
					share = k.max_w
				end
				L.measure(k, share, huge, cx)
				k._rw = share
				used += share
			end
		end
	end
	local mh = 0
	for i = 1, n do
		local h = kids[i]._mh
		if h > mh then
			mh = h
		end
	end
	return used, mh
end

local function row_measure(el, aw, ah, cx)
	local pt, pr, pb, pl = U.pads(el.padding)
	local iw = max(0, aw - pl - pr)
	local used, mh = row_layout(el, iw, cx)
	el._row_iw = iw
	return used + pl + pr, mh + pt + pb
end

local function row_arrange(el, x, y, w, h, cx)
	local pt, pr, pb, pl = U.pads(el.padding)
	local ix, iy = x + pl, y + pt
	local iw, ih = max(0, w - pl - pr), max(0, h - pt - pb)
	local kids = el.children
	local n = #kids
	if n == 0 then
		return
	end
	if el._row_iw ~= iw then
		row_layout(el, iw, cx)
		el._row_iw = iw
	end
	local gap = el.gap or 0
	local used = gap * (n - 1)
	for i = 1, n do
		used += kids[i]._rw
	end
	local extra = iw - used
	local cx0, between = ix, gap
	if extra > 0 then
		local j = el.justify
		if j == "center" then
			cx0 += extra / 2
		elseif j == "end" then
			cx0 += extra
		elseif j == "between" and n > 1 then
			between = gap + extra / (n - 1)
		elseif j == "around" then
			local s = extra / n
			cx0 += s / 2
			between = gap + s
		end
	end
	local align = el.align or "center"
	for i = 1, n do
		local k = kids[i]
		local kw = k._rw
		local kh = final_h(k, if fills_height(k, align) then ih else min(k._mh, ih))
		local ky = iy
		local a = k.align_self or align
		if a == "center" then
			ky = iy + (ih - kh) / 2
		elseif a == "end" then
			ky = iy + ih - kh
		end
		L.arrange(k, cx0, ky, kw, kh, cx)
		cx0 += kw + between
	end
end

T.column = {
	fill = true,
	measure = column_measure,
	arrange = column_arrange,
	draw = draw_box,
}

T.row = {
	fill = true,
	measure = row_measure,
	arrange = row_arrange,
	draw = draw_box,
}

-- A card is a column on a raised surface with theme padding.
T.card = {
	fill = true,
	measure = function(el, aw, ah, cx)
		local th = cx.theme
		if el.padding == nil then
			el.padding = th.pad + 4
		end
		if el.gap == nil then
			el.gap = th.gap
		end
		el.radius = el.radius or th.radius
		return column_measure(el, aw, ah, cx)
	end,
	arrange = column_arrange,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		d:rrect(el.x, el.y + 2, el.w, el.h, el.radius, th.shadow)
		d:rrect(el.x, el.y, el.w, el.h, el.radius, el.bg or th.surface)
		d:stroke(el.x + 0.5, el.y + 0.5, el.w - 1, el.h - 1, el.radius, 1, th.border)
	end,
}

T.stack = {
	fill = true,
	measure = function(el, aw, ah, cx)
		local pt, pr, pb, pl = U.pads(el.padding)
		local iw, ih = max(0, aw - pl - pr), max(0, ah - pt - pb)
		local kids = el.children
		local mw, mh = 0, 0
		for i = 1, #kids do
			local k = L.adopt(el, kids[i], i, cx)
			k._axis = "stack"
			local w, h = L.measure(k, iw, ih, cx)
			mw, mh = max(mw, w), max(mh, h)
		end
		return mw + pl + pr, mh + pt + pb
	end,
	arrange = function(el, x, y, w, h, cx)
		local pt, pr, pb, pl = U.pads(el.padding)
		local ix, iy = x + pl, y + pt
		local iw, ih = max(0, w - pl - pr), max(0, h - pt - pb)
		local kids = el.children
		for i = 1, #kids do
			local k = kids[i]
			local a = k.align_self or el.align or "stretch"
			local kw, kh = iw, ih
			if a ~= "stretch" then
				kw, kh = min(k._mw, iw), min(k._mh, ih)
			end
			kw, kh = final_w(k, kw), final_h(k, kh)
			local kx, ky = ix, iy
			if a == "center" then
				kx, ky = ix + (iw - kw) / 2, iy + (ih - kh) / 2
			elseif a == "end" then
				kx, ky = ix + iw - kw, iy + ih - kh
			end
			L.arrange(k, kx, ky, kw, kh, cx)
		end
	end,
	draw = draw_box,
}

local function grid_measure(el, aw, ah, cx)
	local pt, pr, pb, pl = U.pads(el.padding)
	local iw = max(0, aw - pl - pr)
	local gap = el.gap or 0
	local cg, rg = el.col_gap or gap, el.row_gap or gap
	local kids = el.children
	local n = #kids
	local cols = el.columns
	if cols == nil then
		local mc = el.min_col or 160
		cols = if iw < huge then max(1, floor((iw + cg) / (mc + cg))) else max(1, n)
	end
	cols = max(1, floor(cols))
	local cw
	if iw < huge then
		cw = max(0, (iw - cg * (cols - 1)) / cols)
	else
		cw = 0
		for i = 1, n do
			local k = L.adopt(el, kids[i], i, cx)
			cw = max(cw, (L.measure(k, huge, huge, cx)))
		end
	end
	local rows = {}
	for i = 1, n do
		local k = L.adopt(el, kids[i], i, cx)
		k._axis = "grid"
		local _, h = L.measure(k, cw, huge, cx)
		local r = (i - 1) // cols + 1
		rows[r] = max(rows[r] or 0, h)
	end
	local total = 0
	for r = 1, #rows do
		total += rows[r]
	end
	if #rows > 1 then
		total += rg * (#rows - 1)
	end
	el._cols, el._cw, el._rows, el._grid_iw = cols, cw, rows, iw
	local width = if iw < huge then iw else cols * cw + cg * (cols - 1)
	return width + pl + pr, total + pt + pb
end

T.grid = {
	fill = true,
	measure = grid_measure,
	arrange = function(el, x, y, w, h, cx)
		local pt, pr, pb, pl = U.pads(el.padding)
		local iw = max(0, w - pl - pr)
		if el._grid_iw ~= iw then
			grid_measure(el, w, h, cx)
		end
		local gap = el.gap or 0
		local cg, rg = el.col_gap or gap, el.row_gap or gap
		local cols, cw, rows = el._cols, el._cw, el._rows
		local kids = el.children
		local ry = y + pt
		local align = el.align or "stretch"
		for i = 1, #kids do
			local k = kids[i]
			local r = (i - 1) // cols + 1
			local c = (i - 1) % cols
			if c == 0 and r > 1 then
				ry += rows[r - 1] + rg
			end
			local kx = x + pl + c * (cw + cg)
			local kw = final_w(k, if align == "stretch" or fills_width(k, nil) then cw else min(k._mw, cw))
			local kh = final_h(k, if align == "stretch" then rows[r] else k._mh)
			L.arrange(k, kx, ry, kw, kh, cx)
		end
	end,
	draw = draw_box,
}

T.spacer = {
	check = function(el)
		L.need(el, "size", "number")
	end,
	measure = function(el)
		local s = el.size or 0
		return s, s
	end,
}

T.divider = {
	fill = true,
	fill_h = true,
	check = function(el)
		L.need(el, "thickness", "number")
		L.need(el, "margin", "number")
	end,
	measure = function(el, aw, ah, cx)
		local t = el.thickness or 1
		local m = el.margin or 0
		if el._axis == "row" then
			return t + 2 * m, 0
		end
		return 0, t + 2 * m
	end,
	draw = function(el, dc)
		local t = el.thickness or 1
		local m = el.margin or 0
		local c = el.color or dc.th.border
		if el._axis == "row" then
			dc.d:rect(el.x + m, el.y, t, el.h, c)
		else
			dc.d:rect(el.x, el.y + m, el.w, t, c)
		end
	end,
}

-- ---------------------------------------------------------------- text

function L.text_color(el, th)
	local c = el.color
	if c == nil then
		local tone = el.tone
		if tone == "dim" then
			c = th.text_dim
		elseif tone == "faint" then
			c = th.text_faint
		elseif tone ~= nil then
			c = Theme.tone(th, tone)
		else
			c = th.text
		end
	end
	return c
end

T.text = {
	check = function(el)
		local s, z = el.text, el.size
		if type(s) ~= "string" or (z ~= nil and type(z) ~= "number") or (el.max_lines ~= nil and type(el.max_lines) ~= "number") then
			L.need(el, "text", "string")
			if s == nil then
				error("ui.text \"" .. tostring(el.id) .. "\": text is missing", 0)
			end
			L.need(el, "size", "number")
			L.need(el, "max_lines", "number")
		end
	end,
	measure = function(el, aw, ah, cx)
		local size = el.size or cx.theme.font
		local s = el.text
		local lh = U.line_height(size)
		el._size, el._lh = size, lh
		local single = el.wrap == false or (string.find(s, "\n", 1, true) == nil and U.width(s, size) <= aw)
		if single then
			el._lines = nil
			return min(U.width(s, size), aw), lh
		end
		local lay = text.layout(s, size, max(1, if aw < huge then aw else 1e9))
		local lines = lay.lines
		local n = #lines
		if el.max_lines and n > el.max_lines then
			n = el.max_lines
		end
		local w = 0
		for i = 1, n do
			local lw = lines[i].width
			if lw > w then
				w = lw
			end
		end
		el._lines, el._nl = lines, n
		return min(w, aw), lh * max(1, n)
	end,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local c = L.text_color(el, th)
		if dc.dim then
			c = Theme.dim(th, c)
		end
		local size, lh = el._size, el._lh
		local a = el.align
		local lines = el._lines
		if lines == nil then
			local s = el.text
			local w = U.width(s, size)
			if w > el.w + 0.5 then
				s = U.truncate(s, size, el.w)
				w = U.width(s, size)
			end
			local x = el.x
			if a == "center" then
				x += (el.w - w) / 2
			elseif a == "end" or a == "right" then
				x += el.w - w
			end
			d:text(x, el.y, size, c, s)
			return
		end
		local s = el.text
		local n = el._nl
		local clip = dc.clip
		for i = 1, n do
			local ly = el.y + (i - 1) * lh
			if ly + lh >= clip[2] and ly <= clip[4] then
				local ln = lines[i]
				local str = string.sub(s, ln.from + 1, ln.to)
				local w = ln.width
				if i == n and n < #lines then
					str = U.truncate(str .. "…", size, el.w)
					w = U.width(str, size)
				end
				local x = el.x
				if a == "center" then
					x += (el.w - w) / 2
				elseif a == "end" or a == "right" then
					x += el.w - w
				end
				d:text(x, ly, size, c, str)
			end
		end
	end,
}

-- ---------------------------------------------------------------- passes

function L.push_clip(dc, x, y, w, h)
	local c = dc.clip
	dc.clips[#dc.clips + 1] = c
	dc.clip = { max(c[1], x), max(c[2], y), min(c[3], x + w), min(c[4], y + h) }
	dc.d:clip(x, y, w, h)
end

function L.pop_clip(dc)
	local n = #dc.clips
	if n > 0 then
		dc.clip = dc.clips[n]
		dc.clips[n] = nil
		dc.d:unclip()
	end
end

function L.draw(el, dc)
	local c = dc.clip
	if el.x >= c[3] or el.y >= c[4] or el.x + el.w <= c[1] or el.y + el.h <= c[2] then
		return
	end
	local ty = T[el.type]
	local dim = dc.dim
	if el.disabled then
		dc.dim = true
	end
	if ty.draw then
		ty.draw(el, dc)
	end
	local kids = el.children
	if kids and not ty.leaf then
		for i = 1, #kids do
			L.draw(kids[i], dc)
		end
	end
	if ty.post then
		ty.post(el, dc)
	end
	if ty.focusable and dc.ring and dc.focus == el.id and not ty.own_focus then
		local th = dc.th
		local fw = th.focus_width
		dc.d:stroke(el.x - fw, el.y - fw, el.w + 2 * fw, el.h + 2 * fw, (el.radius or th.radius) + fw, fw, th.focus)
	end
	dc.dim = dim
end

-- The deepest element under (x, y), or nil.
function L.hit(el, x, y)
	if x < el.x or y < el.y or x >= el.x + el.w or y >= el.y + el.h then
		return nil
	end
	local ty = T[el.type]
	local kids = el.children
	if kids and not ty.leaf then
		for i = #kids, 1, -1 do
			local r = L.hit(kids[i], x, y)
			if r then
				return r
			end
		end
	end
	return el
end

-- Call f(el) for el and every arranged descendant, depth first.
function L.walk(el, f)
	f(el)
	local kids = el.children
	if kids and not T[el.type].leaf then
		for i = 1, #kids do
			L.walk(kids[i], f)
		end
	end
end

-- Is `el` or one of its ancestors disabled?
function L.disabled(el)
	while el do
		if el.disabled then
			return true
		end
		el = el._parent
	end
	return false
end

-- Is `a` inside the subtree of `root`?
function L.within(a, root)
	while a do
		if a == root then
			return true
		end
		a = a._parent
	end
	return false
end

return L
