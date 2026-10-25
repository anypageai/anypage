-- ui/controls: buttons, toggles, sliders, progress, badges, images, custom
-- canvases, tabs, drop-down selects (with their menu overlay) and dialogs.
--
-- Widget protocol (all optional), on top of the layout protocol:
--   focusable          takes keyboard focus (Tab order is tree order)
--   press              pressed while held; activate(el, app) on release inside
--   activate(el, app)  Enter / Space while focused, or a completed press
--   down(el, app, x, y, drag) -> "captured" | true    pointer pressed on it
--   drag(el, app, x, y, drag), up(el, app, x, y, drag)
--   key(el, app, key, base, ctrl, shift, alt) -> handled
--   wheel(el, app, dx, dy) -> "moved" | "edge" | nil
--   hover_id(el, x, y)  a finer hover key (a tab, a row) than the element id
--   describe(el)       semantic fields for agents and tests
-- Callbacks the page passes (on_click, on_change, ...) are called through
-- app:call, which appends the app as their last argument.

local U = import("ui/util")
local L = import("ui/layout")
local Theme = import("ui/theme")

local T = L.types
local max, min, floor = math.max, math.min, math.floor
local huge = math.huge
local alpha = color.alpha

local C = {}

local function center_text(d, x, y, w, h, size, c, s)
	local tw = U.width(s, size)
	if tw > w then
		s = U.truncate(s, size, w)
		tw = U.width(s, size)
	end
	local lh = U.line_height(size)
	d:text(x + (w - tw) / 2, y + (h - lh) / 2, size, c, s)
end
C.center_text = center_text

local function dimmed(dc, c)
	if dc.dim then
		return Theme.dim(dc.th, c)
	end
	return c
end

-- ---------------------------------------------------------------- button

local function check_label(el)
	L.need(el, "label", "string")
end

T.button = {
	focusable = true,
	press = true,
	rigid = true,
	check = function(el)
		if el.label == nil then
			el.label = ""
		end
		check_label(el)
		L.need_fns(el, "on_click")
	end,
	measure = function(el, aw, ah, cx)
		local th = cx.theme
		local small = el.size == "small"
		local fs = if small then th.font_small else th.font
		el._fs = fs
		local padx = if small then 10 else 16
		local h = if small then th.control_h - 8 else th.control_h
		return U.width(el.label, fs) + 2 * padx, h
	end,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local v = el.variant or "primary"
		local hov, prs = dc.hover == el.id, dc.press == el.id
		if dc.dim then
			hov, prs = false, false
		end
		local bg, fg, border
		if v == "secondary" then
			bg = if prs then th.surface3 elseif hov then th.surface3 else th.surface2
			fg, border = th.text, th.border
		elseif v == "ghost" then
			bg = if prs then th.surface3 elseif hov then th.surface2 else nil
			fg = th.accent
		elseif v == "danger" then
			bg = if prs then th.danger elseif hov then th.danger_hover else th.danger
			fg = th.on_danger
		else
			bg = if prs then th.accent_press elseif hov then th.accent_hover else th.accent
			fg = th.on_accent
		end
		local r = el.radius or th.radius
		if bg then
			d:rrect(el.x, el.y, el.w, el.h, r, dimmed(dc, bg))
		end
		if border then
			d:stroke(el.x + 0.5, el.y + 0.5, el.w - 1, el.h - 1, r, 1, dimmed(dc, border))
		end
		center_text(d, el.x + 6, el.y, el.w - 12, el.h, el._fs, dimmed(dc, fg), el.label)
	end,
	activate = function(el, app)
		app:call(el.on_click)
	end,
	describe = function(el)
		return { label = el.label, variant = el.variant or "primary" }
	end,
}

-- ---------------------------------------------------------------- checkbox, switch

local BOX = 18

local function check_toggle(el)
	check_label(el)
	L.need_fns(el, "on_change")
end

T.checkbox = {
	focusable = true,
	press = true,
	rigid = true,
	check = check_toggle,
	measure = function(el, aw, ah, cx)
		local fs = cx.theme.font
		el._fs = fs
		local w = BOX
		if el.label and el.label ~= "" then
			w += 8 + U.width(el.label, fs)
		end
		return w, max(U.line_height(fs), 24)
	end,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local hov = dc.hover == el.id or dc.press == el.id
		local by = el.y + (el.h - BOX) / 2
		local on = el.checked == true
		if on or el.mixed then
			d:rrect(el.x, by, BOX, BOX, 4, dimmed(dc, if hov then th.accent_hover else th.accent))
			if el.mixed and not on then
				d:rect(el.x + 4, by + 8, BOX - 8, 2, dimmed(dc, th.on_accent))
			else
				d:line({ el.x + 4, by + 9.5, el.x + 7.5, by + 13, el.x + 14, by + 5.5 }, 2, dimmed(dc, th.on_accent))
			end
		else
			d:rrect(el.x, by, BOX, BOX, 4, dimmed(dc, th.surface2))
			d:stroke(el.x + 0.5, by + 0.5, BOX - 1, BOX - 1, 4, 1, dimmed(dc, if hov then th.border_strong else th.border))
		end
		if el.label and el.label ~= "" then
			local fs = el._fs
			local s = U.truncate(el.label, fs, el.w - BOX - 8)
			d:text(el.x + BOX + 8, el.y + (el.h - U.line_height(fs)) / 2, fs, dimmed(dc, th.text), s)
		end
	end,
	activate = function(el, app)
		app:call(el.on_change, not el.checked)
	end,
	describe = function(el)
		return { label = el.label, checked = el.checked == true }
	end,
}

local SW, SH = 38, 22

T.switch = {
	focusable = true,
	press = true,
	rigid = true,
	check = check_toggle,
	measure = function(el, aw, ah, cx)
		local fs = cx.theme.font
		el._fs = fs
		local w = SW
		if el.label and el.label ~= "" then
			w += 10 + U.width(el.label, fs)
		end
		return w, max(U.line_height(fs), 26)
	end,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local hov = dc.hover == el.id or dc.press == el.id
		local on = el.checked == true
		local ty = el.y + (el.h - SH) / 2
		local track = if on then (if hov then th.accent_hover else th.accent) else (if hov then th.border_strong else th.track)
		d:rrect(el.x, ty, SW, SH, SH / 2, dimmed(dc, track))
		local kx = if on then el.x + SW - SH / 2 else el.x + SH / 2
		d:circle(kx, ty + SH / 2, SH / 2 - 3, dimmed(dc, th.knob))
		if el.label and el.label ~= "" then
			local fs = el._fs
			local s = U.truncate(el.label, fs, el.w - SW - 10)
			d:text(el.x + SW + 10, el.y + (el.h - U.line_height(fs)) / 2, fs, dimmed(dc, th.text), s)
		end
	end,
	activate = function(el, app)
		app:call(el.on_change, not el.checked)
	end,
	describe = function(el)
		return { label = el.label, checked = el.checked == true }
	end,
}

-- ---------------------------------------------------------------- radio group

local RR = 9

local function radio_index(el, x, y)
	local opts = el.options or {}
	if el.direction == "row" then
		local xs = el._xs
		for i = 1, #opts do
			if x >= el.x + xs[i] and x < el.x + xs[i] + el._ws[i] then
				return i
			end
		end
		return nil
	end
	local i = floor((y - el.y) / (el._ih + 4)) + 1
	if i >= 1 and i <= #opts then
		return i
	end
	return nil
end

local function radio_pick(el, app, i)
	local opts = el.options or {}
	if i < 1 or i > #opts then
		return
	end
	if i ~= el.selected then
		app:call(el.on_change, i, opts[i])
	end
end

local function check_choice(el, list)
	L.need(el, list, "table", nil, "a list of strings or { label = ... } tables")
	L.need(el, "selected", "number")
	L.need_fns(el, "on_change")
end

T.radio = {
	focusable = true,
	rigid = true,
	check = function(el)
		check_choice(el, "options")
	end,
	measure = function(el, aw, ah, cx)
		local fs = cx.theme.font
		local ih = max(U.line_height(fs), 26)
		el._fs, el._ih = fs, ih
		local opts = el.options or {}
		local n = #opts
		local xs, ws = {}, {}
		local w, x = 0, 0
		for i = 1, n do
			local iw = 2 * RR + 8 + U.width(U.label_of(opts[i]), fs)
			xs[i], ws[i] = x, iw
			x += iw + 18
			w = max(w, iw)
		end
		el._xs, el._ws = xs, ws
		if el.direction == "row" then
			return max(0, x - 18), ih
		end
		return w, n * ih + max(0, n - 1) * 4
	end,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local opts = el.options or {}
		local fs, ih = el._fs, el._ih
		local lh = U.line_height(fs)
		for i = 1, #opts do
			local ox, oy
			if el.direction == "row" then
				ox, oy = el.x + el._xs[i], el.y
			else
				ox, oy = el.x, el.y + (i - 1) * (ih + 4)
			end
			local cxp, cyp = ox + RR, oy + ih / 2
			local sel = el.selected == i
			local hov = dc.hover == el.id .. ":" .. i
			if sel then
				d:circle(cxp, cyp, RR, dimmed(dc, th.accent))
				d:circle(cxp, cyp, RR - 2, dimmed(dc, th.surface))
				d:circle(cxp, cyp, RR - 4.5, dimmed(dc, th.accent))
			else
				d:circle(cxp, cyp, RR, dimmed(dc, if hov then th.border_strong else th.border))
				d:circle(cxp, cyp, RR - 1, dimmed(dc, th.surface2))
			end
			d:text(ox + 2 * RR + 8, oy + (ih - lh) / 2, fs, dimmed(dc, th.text), U.label_of(opts[i]))
		end
	end,
	down = function(el, app, x, y, dr)
		dr.kind = "radio"
		dr.index = radio_index(el, x, y)
		return "captured"
	end,
	up = function(el, app, x, y, dr)
		local i = radio_index(el, x, y)
		if i and i == dr.index then
			radio_pick(el, app, i)
		end
	end,
	key = function(el, app, k, base)
		local n = #(el.options or {})
		local cur = el.selected or 0
		if base == "up" or base == "left" then
			radio_pick(el, app, if cur <= 1 then n else cur - 1)
		elseif base == "down" or base == "right" then
			radio_pick(el, app, if cur >= n then 1 else cur + 1)
		elseif base == "home" then
			radio_pick(el, app, 1)
		elseif base == "end" then
			radio_pick(el, app, n)
		else
			return false
		end
		return true
	end,
	hover_id = function(el, x, y)
		local i = radio_index(el, x, y)
		return if i then el.id .. ":" .. i else el.id
	end,
	describe = function(el)
		local labels = {}
		for i, o in el.options or {} do
			labels[i] = U.label_of(o)
		end
		return { options = labels, selected = el.selected }
	end,
}

-- ---------------------------------------------------------------- slider

local KNOB = 9

local function slider_geom(el)
	local x0 = el.x + KNOB + 1
	local x1 = el.x + el.w - KNOB - 1 - (el._vw or 0)
	return x0, max(x0 + 1, x1), el.y + el.h / 2
end

local function slider_range(el)
	local lo, hi = el.min or 0, el.max or 1
	if hi <= lo then
		hi = lo + 1
	end
	return lo, hi
end

local function slider_snap(el, v)
	local lo, hi = slider_range(el)
	local step = el.step
	if step and step > 0 then
		v = lo + math.round((v - lo) / step) * step
		-- trim float noise (0.30000000000000004)
		v = math.round(v * 1e9) / 1e9
	end
	return U.clamp(v, lo, hi)
end

C.slider_snap = slider_snap

local function slider_set(el, app, v)
	v = slider_snap(el, v)
	if v ~= el.value then
		app:call(el.on_change, v)
	end
end

local function slider_from_x(el, app, x)
	local x0, x1 = slider_geom(el)
	local lo, hi = slider_range(el)
	slider_set(el, app, lo + (hi - lo) * U.clamp((x - x0) / (x1 - x0), 0, 1))
end

T.slider = {
	focusable = true,
	fill = true,
	check = function(el)
		L.need(el, "value", "number")
		L.need(el, "min", "number")
		L.need(el, "max", "number")
		L.need(el, "step", "number")
		L.need_fns(el, "on_change")
	end,
	measure = function(el, aw, ah, cx)
		local th = cx.theme
		el._vw = 0
		if el.show_value then
			local fs = th.font_small
			local lo, hi = slider_range(el)
			el._vw = max(U.width(U.fmt_num(lo, el.step), fs), U.width(U.fmt_num(hi, el.step), fs)) + 14
		end
		local w = if aw < huge then min(aw, 240) else 240
		return w, 28
	end,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local x0, x1, cy = slider_geom(el)
		local lo, hi = slider_range(el)
		local v = U.clamp(tonumber(el.value) or lo, lo, hi)
		local f = (v - lo) / (hi - lo)
		local active = dc.hover == el.id or dc.press == el.id
		d:rrect(x0, cy - 3, x1 - x0, 6, 3, dimmed(dc, th.track))
		if f > 0 then
			d:rrect(x0, cy - 3, (x1 - x0) * f, 6, 3, dimmed(dc, th.accent))
		end
		local kx = x0 + (x1 - x0) * f
		d:circle(kx, cy, if active then KNOB + 1 else KNOB, dimmed(dc, th.accent))
		d:circle(kx, cy, if active then KNOB - 2 else KNOB - 2.5, dimmed(dc, th.knob))
		if el.show_value then
			local fs = th.font_small
			d:text(x1 + KNOB + 10, cy - U.line_height(fs) / 2, fs, dimmed(dc, th.text_dim), U.fmt_num(v, el.step))
		end
	end,
	down = function(el, app, x, y, dr)
		dr.kind = "slider"
		slider_from_x(el, app, x)
		return "captured"
	end,
	drag = function(el, app, x, y, dr)
		slider_from_x(el, app, x)
	end,
	key = function(el, app, k, base, ctrl, shift)
		local lo, hi = slider_range(el)
		local step = el.step or (hi - lo) / 100
		local v = tonumber(el.value) or lo
		if base == "left" or base == "down" then
			slider_set(el, app, v - (if shift then 10 * step else step))
		elseif base == "right" or base == "up" then
			slider_set(el, app, v + (if shift then 10 * step else step))
		elseif base == "pagedown" then
			slider_set(el, app, v - (hi - lo) / 10)
		elseif base == "pageup" then
			slider_set(el, app, v + (hi - lo) / 10)
		elseif base == "home" then
			slider_set(el, app, lo)
		elseif base == "end" then
			slider_set(el, app, hi)
		else
			return false
		end
		return true
	end,
	describe = function(el)
		local lo, hi = slider_range(el)
		return { value = el.value, min = lo, max = hi, step = el.step }
	end,
}

-- ---------------------------------------------------------------- progress, badge

T.progress = {
	fill = true,
	check = function(el)
		L.need(el, "value", "number")
		L.need(el, "label", "string")
	end,
	measure = function(el, aw, ah, cx)
		local th = cx.theme
		local h = 8
		if el.label then
			h += U.line_height(th.font_small) + 6
		end
		return if aw < huge then min(aw, 240) else 240, h
	end,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local y = el.y
		if el.label then
			local fs = th.font_small
			d:text(el.x, y, fs, th.text_dim, U.truncate(el.label, fs, el.w))
			y += U.line_height(fs) + 6
		end
		local c = Theme.tone(th, el.tone or "accent")
		d:rrect(el.x, y, el.w, 8, 4, th.track)
		local v = el.value
		if v == nil then
			-- indeterminate: a segment sweeping across, 30 frames a second
			local seg = el.w * 0.3
			local p = (dc.t * 0.8) % 1.3 - 0.3
			local a = U.clamp(el.x + p * el.w, el.x, el.x + el.w)
			local b = U.clamp(el.x + p * el.w + seg, el.x, el.x + el.w)
			if b - a > 0.5 then
				d:rrect(a, y, b - a, 8, 4, c)
			end
			dc.want(dc.t + 1 / 30)
		else
			local f = U.clamp(tonumber(v) or 0, 0, 1)
			if f > 0 then
				d:rrect(el.x, y, max(8, el.w * f), 8, 4, c)
			end
		end
	end,
	describe = function(el)
		return { value = el.value, label = el.label }
	end,
}

T.badge = {
	rigid = true,
	check = function(el)
		L.need(el, "text", "string")
	end,
	measure = function(el, aw, ah, cx)
		local fs = cx.theme.font_small
		el._fs = fs
		return U.width(el.text, fs) + 18, U.line_height(fs) + 6
	end,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local c = Theme.tone(th, el.tone or "accent")
		if el.solid then
			d:rrect(el.x, el.y, el.w, el.h, el.h / 2, c)
			center_text(d, el.x, el.y, el.w, el.h, el._fs, th.on_accent, el.text)
		else
			d:rrect(el.x, el.y, el.w, el.h, el.h / 2, alpha(c, 44))
			center_text(d, el.x, el.y, el.w, el.h, el._fs, c, el.text)
		end
	end,
	describe = function(el)
		return { text = el.text, tone = el.tone }
	end,
}

-- ---------------------------------------------------------------- image, canvas

T.image = {
	check = function(el)
		L.need(el, "aspect", "number")
		L.need(el, "alt", "string")
	end,
	measure = function(el, aw, ah, cx)
		local w = el.width or (if aw < huge then min(aw, 240) else 240)
		local h = el.height or (if el.aspect and el.aspect > 0 then w / el.aspect else 160)
		return w, h
	end,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local r = el.radius or th.radius_small
		d:rrect(el.x, el.y, el.w, el.h, r, th.surface2)
		local img = el.image
		if img == nil then
			center_text(d, el.x, el.y, el.w, el.h, th.font_small, th.text_faint, el.alt or "image")
			return
		end
		local x, y, w, h = el.x, el.y, el.w, el.h
		local a = el.aspect
		if a and a > 0 and el.fit ~= "fill" then
			if w / h > a then
				local nw = h * a
				x += (w - nw) / 2
				w = nw
			else
				local nh = w / a
				y += (h - nh) / 2
				h = nh
			end
		end
		d:image(img, x, y, w, h)
	end,
	describe = function(el)
		return { alt = el.alt }
	end,
}

T.canvas = {
	check = function(el)
		L.need_fns(el, "draw", "on_down", "on_drag", "on_up", "on_click")
	end,
	measure = function(el, aw, ah, cx)
		return el.width or (if aw < huge then aw else 240), el.height or 160
	end,
	draw = function(el, dc)
		if el.draw then
			el.draw(dc.d, el.x, el.y, el.w, el.h, dc.th, dc.t)
		end
	end,
	interactive = function(el)
		return el.on_down ~= nil or el.on_drag ~= nil or el.on_up ~= nil or el.on_click ~= nil
	end,
	down = function(el, app, x, y, dr)
		dr.kind = "canvas"
		app:call(el.on_down, x - el.x, y - el.y)
		return "captured"
	end,
	drag = function(el, app, x, y, dr)
		app:call(el.on_drag, x - el.x, y - el.y)
	end,
	up = function(el, app, x, y, dr)
		app:call(el.on_up, x - el.x, y - el.y)
		if U.inside(el, x, y) then
			app:call(el.on_click, x - el.x, y - el.y)
		end
	end,
}

-- ---------------------------------------------------------------- tabs

local TAB_PAD = 14

local function tab_index(el, x)
	local xs, ws = el._xs, el._ws
	for i = 1, #xs do
		if x >= el.x + xs[i] and x < el.x + xs[i] + ws[i] then
			return i
		end
	end
	return nil
end

local function tab_pick(el, app, i)
	local tabs = el.tabs or {}
	if i >= 1 and i <= #tabs and i ~= el.selected then
		app:call(el.on_change, i, tabs[i])
	end
end

T.tabs = {
	focusable = true,
	fill = true,
	own_focus = true,
	check = function(el)
		check_choice(el, "tabs")
	end,
	measure = function(el, aw, ah, cx)
		local th = cx.theme
		local fs = th.font
		el._fs = fs
		local xs, ws = {}, {}
		local x = 0
		for i, t in el.tabs or {} do
			local w = U.width(U.label_of(t), fs) + 2 * TAB_PAD
			xs[i], ws[i] = x, w
			x += w + 2
		end
		el._xs, el._ws = xs, ws
		return max(0, x - 2), th.control_h + 4
	end,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local fs = el._fs
		d:rect(el.x, el.y + el.h - 1, el.w, 1, th.border)
		local focused = dc.focus == el.id and dc.ring
		for i, t in el.tabs or {} do
			local x, w = el.x + el._xs[i], el._ws[i]
			if x > el.x + el.w then
				break
			end
			local sel = el.selected == i
			local hov = dc.hover == el.id .. ":" .. i
			if hov and not sel then
				d:rrect(x, el.y + 2, w, el.h - 5, th.radius_small, th.surface2)
			end
			local c = if sel then th.accent else th.text_dim
			center_text(d, x, el.y, w, el.h - 3, fs, dimmed(dc, c), U.label_of(t))
			if sel then
				d:rrect(x + 4, el.y + el.h - 3, w - 8, 3, 1.5, dimmed(dc, th.accent))
				if focused then
					d:stroke(x, el.y + 1, w, el.h - 3, th.radius_small, th.focus_width, th.focus)
				end
			end
		end
	end,
	down = function(el, app, x, y, dr)
		dr.kind = "tab"
		dr.index = tab_index(el, x)
		return "captured"
	end,
	up = function(el, app, x, y, dr)
		local i = tab_index(el, x)
		if i and i == dr.index and U.inside(el, x, y) then
			tab_pick(el, app, i)
		end
	end,
	key = function(el, app, k, base)
		local n = #(el.tabs or {})
		local cur = el.selected or 1
		if base == "left" then
			tab_pick(el, app, if cur <= 1 then n else cur - 1)
		elseif base == "right" then
			tab_pick(el, app, if cur >= n then 1 else cur + 1)
		elseif base == "home" then
			tab_pick(el, app, 1)
		elseif base == "end" then
			tab_pick(el, app, n)
		else
			return false
		end
		return true
	end,
	hover_id = function(el, x, y)
		local i = tab_index(el, x)
		return if i then el.id .. ":" .. i else el.id
	end,
	describe = function(el)
		local labels = {}
		for i, t in el.tabs or {} do
			labels[i] = U.label_of(t)
		end
		return { tabs = labels, selected = el.selected }
	end,
}

-- ---------------------------------------------------------------- select and its menu

local MENU_MAX = 8

local function select_label(el)
	local opts = el.options or {}
	local i = el.selected
	if i and opts[i] ~= nil then
		return U.label_of(opts[i]), true
	end
	return el.placeholder or "", false
end

local function select_pick(el, app, i)
	local opts = el.options or {}
	if i >= 1 and i <= #opts and i ~= el.selected then
		app:call(el.on_change, i, opts[i])
	end
end
C.select_pick = select_pick

-- Open (or close) the menu of select `el`.
function C.toggle_menu(el, app, open)
	local u = app:ui_get()
	if open == nil then
		open = u.open ~= el.id
	end
	if open then
		u.open = el.id
		u.open_hi = el.selected or 1
		local n = #(el.options or {})
		local first = U.clamp((u.open_hi or 1) - MENU_MAX // 2, 1, max(1, n - MENU_MAX + 1))
		u.menu_first = first
	else
		u.open, u.open_hi, u.menu_first = nil, nil, nil
	end
	app:ui_set(u)
end

T.select = {
	focusable = true,
	fill = true,
	check = function(el)
		check_choice(el, "options")
		L.need(el, "placeholder", "string")
	end,
	measure = function(el, aw, ah, cx)
		local th = cx.theme
		local fs = th.font
		el._fs = fs
		local w = U.width(el.placeholder or "", fs)
		for _, o in el.options or {} do
			w = max(w, U.width(U.label_of(o), fs))
		end
		w += 44
		return if aw < huge then min(aw, max(w, 160)) else max(w, 160), th.control_h
	end,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local open = dc.u.open == el.id
		local hov = dc.hover == el.id
		local r = th.radius_small
		d:rrect(el.x, el.y, el.w, el.h, r, dimmed(dc, if hov or open then th.surface3 else th.surface2))
		d:stroke(el.x + 0.5, el.y + 0.5, el.w - 1, el.h - 1, r, 1, dimmed(dc, if open then th.focus else th.border))
		local label, chosen = select_label(el)
		local fs = el._fs
		local lh = U.line_height(fs)
		d:text(el.x + 12, el.y + (el.h - lh) / 2, fs, dimmed(dc, if chosen then th.text else th.text_faint), U.truncate(label, fs, el.w - 40))
		local ax, ay = el.x + el.w - 20, el.y + el.h / 2
		if open then
			d:line({ ax - 5, ay + 3, ax, ay - 2, ax + 5, ay + 3 }, 1.8, dimmed(dc, th.text_dim))
		else
			d:line({ ax - 5, ay - 2, ax, ay + 3, ax + 5, ay - 2 }, 1.8, dimmed(dc, th.text_dim))
		end
	end,
	down = function(el, app, x, y, dr)
		dr.kind = "select"
		C.toggle_menu(el, app)
		return "captured"
	end,
	activate = function(el, app)
		C.toggle_menu(el, app, true)
	end,
	key = function(el, app, k, base)
		local n = #(el.options or {})
		local cur = el.selected or 0
		if base == "up" then
			select_pick(el, app, max(1, cur - 1))
		elseif base == "down" then
			select_pick(el, app, min(n, cur + 1))
		elseif base == "home" then
			select_pick(el, app, 1)
		elseif base == "end" then
			select_pick(el, app, n)
		else
			return false
		end
		return true
	end,
	describe = function(el)
		local labels = {}
		for i, o in el.options or {} do
			labels[i] = U.label_of(o)
		end
		return { options = labels, selected = el.selected, label = (select_label(el)) }
	end,
}

-- The menu overlay of the open select `owner`, placed inside an app of size W x H.
function C.build_menu(owner, u, th, W, H)
	local opts = owner.options or {}
	local n = #opts
	local ih = th.row_h
	local shown = min(n, MENU_MAX)
	local h = shown * ih + 8
	local w = max(owner.w, 160)
	local x = U.clamp(owner.x, 4, max(4, W - w - 4))
	local y = owner.y + owner.h + 4
	if y + h > H - 4 and owner.y - h - 4 >= 4 then
		y = owner.y - h - 4
	end
	local first = U.clamp(u.menu_first or 1, 1, max(1, n - shown + 1))
	return {
		type = "_menu",
		id = owner.id .. ":menu",
		owner = owner,
		options = opts,
		n = n,
		shown = shown,
		first = first,
		hi = u.open_hi,
		ih = ih,
		x = x,
		y = y,
		w = w,
		h = h,
	}
end

function C.menu_index(m, x, y)
	if not U.inside(m, x, y) then
		return nil
	end
	local i = m.first + floor((y - m.y - 4) / m.ih)
	if i >= m.first and i < m.first + m.shown and i <= m.n then
		return i
	end
	return nil
end

T._menu = {
	leaf = true,
	measure = function()
		return 0, 0
	end,
	draw = function(m, dc)
		local th, d = dc.th, dc.d
		d:rrect(m.x, m.y + 3, m.w, m.h, th.radius, th.shadow)
		d:rrect(m.x, m.y, m.w, m.h, th.radius, th.surface)
		d:stroke(m.x + 0.5, m.y + 0.5, m.w - 1, m.h - 1, th.radius, 1, th.border_strong)
		local fs = th.font
		local lh = U.line_height(fs)
		for k = 0, m.shown - 1 do
			local i = m.first + k
			if i > m.n then
				break
			end
			local y = m.y + 4 + k * m.ih
			if i == m.hi then
				d:rrect(m.x + 4, y, m.w - 8, m.ih, th.radius_small, th.accent_soft)
			end
			local c = if i == m.owner.selected then th.accent else th.text
			d:text(m.x + 12, y + (m.ih - lh) / 2, fs, c, U.truncate(U.label_of(m.options[i]), fs, m.w - 24))
		end
		if m.n > m.shown then
			local track = m.h - 8
			local th_h = max(16, track * m.shown / m.n)
			local ty = m.y + 4 + (track - th_h) * (m.first - 1) / (m.n - m.shown)
			d:rrect(m.x + m.w - 7, ty, 4, th_h, 2, th.thumb)
		end
	end,
}

-- Menu keys while a select is open. Returns true when handled.
function C.menu_key(m, app, base)
	local u = app:ui_get()
	local hi = u.open_hi or m.owner.selected or 1
	local n = m.n
	if base == "up" then
		hi = max(1, hi - 1)
	elseif base == "down" then
		hi = min(n, hi + 1)
	elseif base == "home" then
		hi = 1
	elseif base == "end" then
		hi = n
	elseif base == "pageup" then
		hi = max(1, hi - MENU_MAX)
	elseif base == "pagedown" then
		hi = min(n, hi + MENU_MAX)
	elseif base == "enter" or base == "space" then
		C.toggle_menu(m.owner, app, false)
		select_pick(m.owner, app, hi)
		return true
	elseif base == "escape" or base == "tab" then
		C.toggle_menu(m.owner, app, false)
		return base == "escape"
	else
		return false
	end
	u.open_hi = hi
	local first = u.menu_first or m.first
	if hi < first then
		first = hi
	elseif hi >= first + m.shown then
		first = hi - m.shown + 1
	end
	u.menu_first = first
	app:ui_set(u)
	return true
end

function C.menu_wheel(m, app, dy)
	if m.n <= m.shown then
		return
	end
	local u = app:ui_get()
	local first = U.clamp((u.menu_first or m.first) - (if dy > 0 then 1 else -1) * max(1, math.round(math.abs(dy))), 1, m.n - m.shown + 1)
	if first ~= u.menu_first then
		u.menu_first = first
		app:ui_set(u)
	end
end

-- ---------------------------------------------------------------- dialog

-- A dialog takes no space where it is declared; while open, the app lays it
-- out as a modal overlay centred in its rectangle (see ui/app).
T.dialog = {
	leaf = true,
	check = function(el)
		L.need_fns(el, "on_close")
	end,
	measure = function(el, aw, ah, cx)
		if el.open then
			cx.dialogs[#cx.dialogs + 1] = el
		end
		return 0, 0
	end,
	describe = function(el)
		return { title = el.title, open = el.open == true }
	end,
}

return C
