-- Engine prelude. Runs in the page's realm before the page, then the sandbox
-- freezes these globals: pages can use them but not change them.

local floor = math.floor

color = {}

-- Packed 0xRRGGBBAA, the colour form display lists carry.
function color.rgba(r, g, b, a)
	return floor(r) * 16777216 + floor(g) * 65536 + floor(b) * 256 + floor(a or 255)
end

-- The same colour with another alpha.
function color.alpha(c, a)
	return c - c % 256 + floor(a)
end

-- h in degrees, s and v in 0..1.
function color.hsv(h, s, v, a)
	h = h % 360
	local c = v * s
	local hp = h / 60
	local x = c * (1 - math.abs(hp % 2 - 1))
	local i = floor(hp)
	local r, g, b
	if i == 0 then
		r, g, b = c, x, 0
	elseif i == 1 then
		r, g, b = x, c, 0
	elseif i == 2 then
		r, g, b = 0, c, x
	elseif i == 3 then
		r, g, b = 0, x, c
	elseif i == 4 then
		r, g, b = x, 0, c
	else
		r, g, b = c, 0, x
	end
	local m = v - c
	return color.rgba((r + m) * 255, (g + m) * 255, (b + m) * 255, a or 255)
end

-- Display list builder handed to every panel's draw function as `d`.
-- Recording is all that happens here; the engine rasterises natively.
Draw = {}
Draw.__index = Draw

function Draw.new(w, h)
	return setmetatable({ w = w, h = h, n = 0, cmds = {} }, Draw)
end

local function push(d, cmd)
	local n = d.n + 1
	d.n = n
	d.cmds[n] = cmd
end

function Draw:clear(c)
	push(self, { "clear", c })
end

function Draw:rect(x, y, w, h, c)
	push(self, { "rrect", x, y, w, h, 0, c })
end

function Draw:rrect(x, y, w, h, r, c)
	push(self, { "rrect", x, y, w, h, r, c })
end

function Draw:stroke(x, y, w, h, r, width, c)
	push(self, { "stroke", x, y, w, h, r, width, c })
end

function Draw:circle(x, y, r, c)
	push(self, { "circle", x, y, r, c })
end

-- dir: nil (auto), "ltr" or "rtl" (a wrapped line keeps its paragraph's)
function Draw:text(x, y, size, c, s, dir)
	push(self, { "text", x, y, size, c, tostring(s), dir })
end

-- Word-wrapped text; with tail = true shows the last lines that fit.
function Draw:textbox(x, y, w, h, size, c, s, tail)
	push(self, { "textbox", x, y, w, h, size, c, tostring(s), tail == true })
end

-- pts: flat list { x1, y1, x2, y2, ... }
function Draw:line(pts, width, c)
	push(self, { "line", pts, width, c })
end

function Draw:area(pts, floor_y, c)
	push(self, { "area", pts, floor_y, c })
end

-- An image value (a decoded frame, a rendered page) scaled into a rectangle.
function Draw:image(img, x, y, w, h)
	push(self, { "image", img, x, y, w, h })
end

-- Clip everything drawn until the matching unclip to a rectangle; nests.
function Draw:clip(x, y, w, h)
	push(self, { "clip", x, y, w, h })
end

function Draw:unclip()
	push(self, { "unclip" })
end

-- A value series scaled into the box natively: no per-point work in the script.
function Draw:series(x, y, w, h, values, line, fill, width, floor_y, min_span)
	push(self, { "series", x, y, w, h, values, line, fill or 0, width or 2, floor_y or (y + h), min_span or 0 })
end
