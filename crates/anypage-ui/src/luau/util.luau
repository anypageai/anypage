-- ui/util: small helpers shared by the layout, widgets and the app.
--
-- Text measurement goes through the host's text system (text.*). Widths are
-- cached per (size, string): measuring is deterministic for the life of a
-- page, so caching keeps view functions pure while saving native calls.

local M = {}

local huge = math.huge
local floor = math.floor
local twidth, theight = text.width, text.line_height

M.huge = huge

function M.clamp(v, lo, hi)
	if v < lo then
		return lo
	end
	if v > hi then
		return hi
	end
	return v
end

function M.min(a, b)
	return if a < b then a else b
end

function M.max(a, b)
	return if a > b then a else b
end

-- padding = n | { v, h } | { top, right, bottom, left }  ->  top, right, bottom, left
function M.pads(p)
	if p == nil then
		return 0, 0, 0, 0
	end
	if type(p) == "number" then
		return p, p, p, p
	end
	if type(p) == "table" then
		local n = #p
		if n == 2 then
			return p[1] or 0, p[2] or 0, p[1] or 0, p[2] or 0
		elseif n == 4 then
			return p[1] or 0, p[2] or 0, p[3] or 0, p[4] or 0
		end
		return p.top or 0, p.right or 0, p.bottom or 0, p.left or 0
	end
	return 0, 0, 0, 0
end

function M.inside(el, x, y)
	return x >= el.x and y >= el.y and x < el.x + el.w and y < el.y + el.h
end

-- ---------------------------------------------------------------- text metrics

local widths = {}
local width_count = 0
local heights = {}

function M.width(s, size)
	if s == "" then
		return 0
	end
	if #s > 256 then
		return twidth(s, size) -- long text (editors): not worth keeping
	end
	local bucket = widths[size]
	if bucket == nil then
		bucket = {}
		widths[size] = bucket
	end
	local w = bucket[s]
	if w == nil then
		if width_count > 8192 then
			table.clear(widths)
			width_count = 0
			bucket = {}
			widths[size] = bucket
		end
		w = twidth(s, size)
		bucket[s] = w
		width_count += 1
	end
	return w
end

function M.line_height(size)
	local h = heights[size]
	if h == nil then
		h = theight(size)
		heights[size] = h
	end
	return h
end

-- Byte length of the longest prefix of `s` no longer than `n` bytes that ends
-- on a UTF-8 code point boundary.
function M.utf8_cut(s, n)
	if n >= #s then
		return #s
	end
	if n <= 0 then
		return 0
	end
	local p = n
	while p > 0 do
		local b = string.byte(s, p + 1)
		-- a continuation byte (10xxxxxx) cannot start a code point
		if b == nil or b < 0x80 or b >= 0xC0 then
			break
		end
		p -= 1
	end
	return p
end

-- `s` cut to fit `maxw` at `size`, ending with an ellipsis when cut.
function M.truncate(s, size, maxw)
	if maxw <= 0 then
		return ""
	end
	if M.width(s, size) <= maxw then
		return s
	end
	local ell = "…"
	local room = maxw - M.width(ell, size)
	if room <= 0 then
		return ""
	end
	local p = text.hit(s, size, room)
	while p > 0 and text.caret(s, size, p) > room do
		p = text.prev(s, p)
	end
	return string.sub(s, 1, p) .. ell
end

-- ---------------------------------------------------------------- password masking

local DOT = "•" -- 3 bytes in UTF-8

-- Masked display string: one dot per grapheme.
function M.mask(s)
	return string.rep(DOT, text.graphemes(s))
end

-- Byte offset in the masked string for byte offset `pos` in `s`.
function M.mask_pos(s, pos)
	return text.graphemes(string.sub(s, 1, pos)) * #DOT
end

-- Byte offset in `s` for byte offset `dpos` in the masked string.
function M.unmask_pos(s, dpos)
	local n = floor(dpos / #DOT)
	local p = 0
	for _ = 1, n do
		local q = text.next(s, p)
		if q == p then
			break
		end
		p = q
	end
	return p
end

-- ---------------------------------------------------------------- misc

-- Keys in "ctrl+shift+left" form: base key and modifier flags.
function M.parse_key(k)
	local ctrl, shift, alt = false, false, false
	while true do
		local m, rest = string.match(k, "^(%a+)%+(.+)$")
		if m == nil then
			break
		end
		if m == "ctrl" or m == "meta" or m == "cmd" then
			ctrl = true
		elseif m == "shift" then
			shift = true
		elseif m == "alt" then
			alt = true
		else
			break
		end
		k = rest
	end
	return k, ctrl, shift, alt
end

-- ---------------------------------------------------------------- time
--
-- page.now() is the page clock at the last tick, and the engine ticks only
-- when something visible is due, so after a quiet spell it can be seconds
-- old. Stamps taken in handlers (toast end, tooltip start, caret blink phase)
-- need the current time on the page clock's scale. The page clock runs at
-- wall-clock rate, so `clock - os.clock()` is a constant; every observation
-- of the clock (a draw's `t`, page.now() in a handler) is at or before the
-- true time, so the largest `observed - os.clock()` seen is the best estimate
-- of that constant. A stale observation only ever lowers the estimate of
-- itself, never the maximum. Durations between two handler events (double
-- clicks) use os.clock() directly.

local oclock = if os ~= nil and os.clock ~= nil then os.clock else function()
	return 0
end
M.clock = oclock
local offset = -huge

-- Record that the page clock read `t` just now.
function M.observe(t)
	if type(t) == "number" then
		local o = t - oclock()
		if o > offset then
			offset = o
		end
	end
end

-- Current time on the page clock's scale (handlers).
function M.now()
	local pn = page.now()
	local c = oclock()
	local o = pn - c
	if o > offset then
		offset = o
	end
	local est = c + offset
	return if est > pn then est else pn
end

function M.copy(t)
	if type(t) ~= "table" then
		return {}
	end
	return table.clone(t)
end

-- Item label for list-like options: a string, a number, or { label = ... }.
function M.label_of(o)
	if type(o) == "table" then
		return tostring(o.label or o.title or o.name or o.value or "")
	end
	if o == nil then
		return ""
	end
	return tostring(o)
end

function M.fmt_num(v, step)
	if step and step >= 1 and floor(step) == step then
		return string.format("%d", floor(v + 0.5))
	end
	if step and step > 0 then
		local digits = math.max(0, math.ceil(-math.log10(step)))
		return string.format("%." .. digits .. "f", v)
	end
	if floor(v) == v then
		return string.format("%d", v)
	end
	return string.format("%.2f", v)
end

return M
