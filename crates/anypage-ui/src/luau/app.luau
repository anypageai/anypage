-- ui/app: an app is a rectangle of the page with its own state.
--
--   state cell  `<name>.state`  the page's data (app:get/set/update)
--   UI cell     `<name>.ui`     focus, hover, press, caret, scroll offsets,
--                               open menu, toasts (the library's own state)
--   panel       draws view(state, inputs...) every time one of those cells
--               or an input changes; nothing runs otherwise
--
-- The panel's draw function is pure: it builds the element tree, lays it out
-- and records a display list. It also keeps the laid-out tree for the event
-- handlers (hit testing, focus order); a handler that runs after a state
-- change the panel has not drawn yet rebuilds the tree from the current cell
-- values first, so events never act on stale positions or values.
--
-- One set of page event handlers (installed with the first app) routes
-- pointer, wheel, key, text, IME and paste events to the right app.

local U = import("ui/util")
local Theme = import("ui/theme")
local L = import("ui/layout")
local C = import("ui/controls")
local F = import("ui/fields")
local V = import("ui/views")

local T = L.types
local huge = math.huge
local max, min, floor = math.max, math.min, math.floor
local clock = U.clock
-- what the semantic port shows of one list (longer lists are cut there)
local PORT_LIST = 40
-- scroll offsets kept in the UI cell before entries of vanished elements are dropped
local SCROLL_KEPT = 16

local A = {}

local apps = {}
local by_name = {}
local focused = nil -- the app with keyboard focus
local captured = nil -- the app that owns the current pointer drag
local hovered = nil -- the app under the pointer
local clipboard = nil -- in-page clipboard for pages without the clipboard capability
local installed = false

local App = {}
App.__index = App

local function interactive(e)
	local ty = T[e.type]
	if ty.interactive then
		return ty.interactive(e)
	end
	return ty.down ~= nil or ty.press == true
end

local function focusable(e)
	return T[e.type].focusable == true and e.focusable ~= false
end

-- ---------------------------------------------------------------- state access

-- The current time on the page clock's scale (see ui/util: page.now() is
-- the last tick, which can be old after a quiet spell).
function App:now()
	return U.now()
end

function App:get()
	local s = page.get(self.state)
	if type(s) ~= "table" then
		return {}
	end
	return s
end

function App:set(key, value)
	local s = self:get()
	s[key] = value
	page.set(self.state, s)
	self._rev += 1
end

function App:update(f)
	local s = self:get()
	local r = f(s)
	page.set(self.state, if r ~= nil then r else s)
	self._rev += 1
end

function App:replace(s)
	page.set(self.state, s)
	self._rev += 1
end

function App:ui_get()
	local u = page.get(self.ui)
	if type(u) ~= "table" then
		return {}
	end
	return u
end

-- Scroll offsets are keyed by element id; once there are many, drop those
-- of elements that are not in the current tree.
local function prune_ids(map, byid)
	if type(map) ~= "table" then
		return
	end
	local n = 0
	for _ in map do
		n += 1
	end
	if n <= SCROLL_KEPT then
		return
	end
	for id in map do
		if byid[id] == nil then
			map[id] = nil
		end
	end
end

function App:ui_set(u)
	local c = self._cache
	if c ~= nil and not c.broken then
		prune_ids(u.scroll, c.byid)
		prune_ids(u.scrollx, c.byid)
	end
	page.set(self.ui, u)
	self._rev += 1
end

-- Call a page callback with the app appended to its arguments.
function App:call(f, ...)
	if f == nil then
		return nil
	end
	local args = table.pack(...)
	local n = args.n
	args[n + 1] = self
	return f(table.unpack(args, 1, n + 1))
end

-- The caret rectangle (app-local) for the host's keyboard / IME window.
function App:host_caret(r)
	if focused ~= self or r == nil then
		return
	end
	local R = self.rect
	page.focus_text({ R[1] + r[1], R[2] + r[2], max(1, r[3]), max(1, r[4]) })
	self._text_focus = true
end

function App:_drop_text_focus()
	if self._text_focus then
		self._text_focus = false
		page.focus_text(nil)
	end
end

function App:copy(s)
	clipboard = s
	pcall(page.copy, s)
end

-- Ctrl+V in a field: ask the host for the clipboard (it answers with a
-- "paste" event); without the clipboard capability use the in-page one.
function App:paste_into(el)
	local ok = pcall(page.paste)
	if not ok and clipboard ~= nil then
		local f = T[el.type].paste
		if f then
			f(el, self, clipboard)
		end
	end
end

-- ---------------------------------------------------------------- building

function App:_dialog_panel(dlg, cx, W, H)
	local th = self.theme
	local kids = {}
	if dlg.title then
		kids[#kids + 1] = { type = "text", text = tostring(dlg.title), size = th.font_large }
	end
	local body = dlg.body
	if type(body) == "string" then
		kids[#kids + 1] = { type = "text", text = body, tone = "dim" }
	elseif type(body) == "table" and body.type ~= nil then
		kids[#kids + 1] = body
	end
	for _, k in L.kids(dlg.children) do
		kids[#kids + 1] = k
	end
	local acts = L.kids(dlg.actions)
	if #acts > 0 then
		kids[#kids + 1] = { type = "row", gap = th.gap, justify = "end", fill = true, children = acts }
	end
	local panel = { type = "card", id = dlg.id .. "/panel", padding = 20, gap = 14, children = kids, dialog = dlg, _id_set = true }
	cx.byid[panel.id] = panel
	local pw = min(dlg.width or 440, W - 32)
	L.measure(panel, pw, H - 32, cx)
	local ph = min(panel._mh, H - 32)
	L.arrange(panel, floor((W - pw) / 2), floor((H - ph) / 2), pw, ph, cx)
	return panel
end

function App:_toasts(u, t, W, H)
	local out = {}
	local list = u.toasts
	if type(list) ~= "table" then
		return out
	end
	local th = self.theme
	local fs = th.font
	local h = U.line_height(fs) + 20
	local y = H - 16
	for i = #list, 1, -1 do
		local tt = list[i]
		if type(tt) == "table" and (tonumber(tt.ends) or 0) > t then
			local s = tostring(tt.text or "")
			local w = min(W - 32, U.width(s, fs) + 40)
			y -= h
			out[#out + 1] = {
				type = "_toast",
				id = "toast:" .. tostring(tt.id),
				tid = tt.id,
				text = U.truncate(s, fs, w - 40),
				tone = tt.tone,
				ends = tonumber(tt.ends) or 0,
				x = (W - w) / 2,
				y = y,
				w = w,
				h = h,
			}
			y -= 8
		end
	end
	return out
end

function App:_build(s, u, args, t)
	local c0 = clock()
	local root = self.view(s, table.unpack(args, 1, self._nin))
	local c1 = clock()
	if type(root) ~= "table" then
		error("ui.app \"" .. self.name .. "\": view must return an element, got " .. type(root), 0)
	end
	if root.type == nil then
		root = { type = "column", children = L.kids(root) }
	elseif root._seen or root._parent ~= nil then
		root = L.fresh(root)
	end
	root.id = tostring(root.id or self.name)
	root._id_set = true
	root._parent = nil
	local th = self.theme
	local W, H = self.rect[3], self.rect[4]
	local cx = { theme = th, u = u, app = self, count = 0, limit = self.limit, byid = {}, dialogs = {} }
	cx.byid[root.id] = root
	L.measure(root, W, H, cx)
	L.arrange(root, 0, 0, L.final_w(root, W), L.final_h(root, H), cx)
	local dialogs = {}
	for _, dlg in cx.dialogs do
		dialogs[#dialogs + 1] = self:_dialog_panel(dlg, cx, W, H)
	end
	local modal = dialogs[#dialogs]
	local menu = nil
	if u.open ~= nil then
		local owner = cx.byid[u.open]
		if owner and owner.type == "select" and not L.disabled(owner) and (modal == nil or L.within(owner, modal)) then
			menu = C.build_menu(owner, u, th, W, H)
		end
	end
	local focusables = {}
	local function collect(e, dis)
		dis = dis or e.disabled == true
		local ty = T[e.type]
		if ty.focusable and not dis and e.focusable ~= false then
			focusables[#focusables + 1] = e
		end
		local kids = e.children
		if kids and not ty.leaf then
			for i = 1, #kids do
				collect(kids[i], dis)
			end
		end
	end
	collect(modal or root, false)
	return {
		root = root,
		dialogs = dialogs,
		modal = modal,
		menu = menu,
		toasts = self:_toasts(u, t, W, H),
		byid = cx.byid,
		focusables = focusables,
		rev = self._rev,
		u = u,
		t = t,
		count = cx.count,
		view_ms = (c1 - c0) * 1000,
		layout_ms = (clock() - c1) * 1000,
	}
end

-- The laid-out tree for event handling: the last drawn one, or a fresh one
-- when the app's state changed since.
function App:_tree()
	local c = self._cache
	if c ~= nil and c.rev == self._rev then
		return c
	end
	local u = self:ui_get()
	local ok, tr = pcall(self._build, self, self:get(), u, self._args or { n = 0 }, self:now())
	if not ok then
		tr = self:_empty_tree(u, self:_failed(tr))
	end
	self._cache = tr
	return tr
end

-- While the view fails, handlers get a tree with nothing in it: events do
-- nothing (rather than failing one by one) until the state changes.
function App:_empty_tree(u, msg)
	local W, H = self.rect[3], self.rect[4]
	local root = { type = "column", id = self.name, children = {}, x = 0, y = 0, w = W, h = H, _id_set = true }
	return {
		root = root,
		dialogs = {},
		toasts = {},
		byid = { [root.id] = root },
		focusables = {},
		rev = self._rev,
		u = u,
		t = 0,
		count = 0,
		view_ms = 0,
		layout_ms = 0,
		broken = msg,
	}
end

-- Log a view failure once (until it changes or the view recovers).
function App:_failed(e)
	local msg = tostring(e)
	if msg ~= self._error then
		self._error = msg
		page.log("ui.app \"" .. self.name .. "\": the view failed: " .. msg)
	end
	return msg
end

-- What a failing view shows: the error, in the app's rectangle.
function App:_error_card(d, msg)
	local th = self.theme
	local W, H = self.rect[3], self.rect[4]
	d:rect(0, 0, W, H, th.bg)
	-- small apps get small margins; the full message is in the log either way
	local m = min(16, floor(W / 12), floor(H / 12))
	local x, y, w = m, m, max(1, W - 2 * m)
	local fs = if W < 240 or H < 120 then th.font_small else th.font
	local lh = U.line_height(fs)
	local lines = text.layout(msg, fs, max(1, w - 24)).lines
	local n = min(#lines, max(1, floor((H - 2 * m - lh - 16) / lh)))
	local h = min(H - 2 * m, 16 + (n + 1) * lh)
	d:rrect(x, y, w, h, th.radius, th.surface)
	d:rrect(x, y, 4, h, 2, th.danger)
	d:text(x + 14, y + 6, fs, th.danger, "ui.app \"" .. self.name .. "\": the view failed")
	for i = 1, n do
		local ln = lines[i]
		d:text(x + 14, y + 10 + i * lh, fs, th.text_dim, string.sub(msg, ln.from + 1, ln.to))
	end
end

-- ---------------------------------------------------------------- drawing

T._toast = {
	leaf = true,
	measure = function()
		return 0, 0
	end,
	draw = function(e, dc)
		local th, d = dc.th, dc.d
		d:rrect(e.x, e.y + 3, e.w, e.h, th.radius, th.shadow)
		d:rrect(e.x, e.y, e.w, e.h, th.radius, th.tooltip_bg)
		d:rrect(e.x + 8, e.y + 8, 4, e.h - 16, 2, Theme.tone(th, e.tone or "accent"))
		local fs = th.font
		d:text(e.x + 22, e.y + (e.h - U.line_height(fs)) / 2, fs, th.tooltip_text, e.text)
	end,
}

function App:_tooltip(dc, tr, u, t)
	local id = u.tip
	if id == nil then
		return
	end
	local el = tr.byid[id]
	if el == nil or el.tooltip == nil or el.x == nil then
		return
	end
	local th, d = dc.th, dc.d
	local at = (tonumber(u.tip_t) or 0) + th.tooltip_delay
	if t < at then
		dc.want(at)
		return
	end
	local W, H = self.rect[3], self.rect[4]
	local fs = th.font_small
	local s = tostring(el.tooltip)
	local w = min(min(320, W - 16), U.width(s, fs) + 20)
	local lines = text.layout(s, fs, max(1, w - 20)).lines
	local lh = U.line_height(fs)
	local h = max(1, #lines) * lh + 12
	local x = U.clamp(el.x + el.w / 2 - w / 2, 8, max(8, W - w - 8))
	local y = el.y + el.h + 6
	if y + h > H - 4 then
		y = max(4, el.y - h - 6)
	end
	d:rrect(x, y + 2, w, h, 6, th.shadow)
	d:rrect(x, y, w, h, 6, th.tooltip_bg)
	for i, ln in lines do
		d:text(x + 10, y + 6 + (i - 1) * lh, fs, th.tooltip_text, string.sub(s, ln.from + 1, ln.to))
	end
end

-- The panel's draw. A failing view (or a failing page callback while
-- drawing) shows an error card, is logged once, and declares no time, so it
-- runs again only when its state, UI state or inputs change.
function App:_draw(d, t, s, u, ...)
	U.observe(t)
	-- if the view fails below, handlers must not keep acting on the last good tree
	self._cache = nil
	self._args = table.pack(...)
	self._t = t
	if type(s) ~= "table" then
		s = {}
	end
	if type(u) ~= "table" then
		u = {}
	end
	local ok, r = pcall(self._render, self, d, t, s, u)
	if ok then
		self._error = nil
		return r
	end
	local msg = self:_failed(r)
	table.clear(d.cmds)
	d.n = 0
	self._cache = self:_empty_tree(u, msg)
	self:_error_card(d, msg)
	return huge
end

function App:_render(d, t, s, u)
	local c0 = clock()
	local tr = self:_build(s, u, self._args, t)
	local c1 = clock()
	local th = self.theme
	local W, H = self.rect[3], self.rect[4]
	local next_t = huge
	local dc = {
		d = d,
		th = th,
		u = u,
		t = t,
		focus = u.focus,
		hover = u.hover,
		press = u.press,
		edit = u.edit,
		ime = u.ime,
		clip = { 0, 0, W, H },
		clips = {},
		ring = u.kb == true,
		dim = false,
		caret_on = true,
	}
	dc.want = function(at)
		if at < next_t then
			next_t = at
		end
	end
	local fel = if u.focus ~= nil then tr.byid[u.focus] else nil
	if fel and T[fel.type].text_input and self.blink and th.caret_blink > 0 then
		local b0 = tonumber(u.blink) or 0
		local period = th.caret_blink
		local k = floor(max(0, t - b0) / period)
		dc.caret_on = k % 2 == 0
		dc.want(b0 + (k + 1) * period)
	end
	if self.background ~= false then
		d:rect(0, 0, W, H, self.background or th.bg)
	end
	L.draw(tr.root, dc)
	for _, p in tr.dialogs do
		d:rect(0, 0, W, H, th.scrim)
		L.draw(p, dc)
	end
	if tr.menu then
		T._menu.draw(tr.menu, dc)
	end
	for _, e in tr.toasts do
		T._toast.draw(e, dc)
		dc.want(e.ends)
	end
	self:_tooltip(dc, tr, u, t)
	self._cache = tr
	if next(self._hist) ~= nil then
		F.prune_history(self, tr.byid)
	end
	local c2 = clock()
	local st = self._stats
	st.frames += 1
	st.view_ms = tr.view_ms
	st.layout_ms = tr.layout_ms
	st.draw_ms = (c2 - c1) * 1000
	st.total_ms = (c2 - c0) * 1000
	st.elements = tr.count
	st.commands = d.n
	return next_t
end

-- ---------------------------------------------------------------- focus

function App:_blur()
	local u = self:ui_get()
	if u.focus ~= nil or u.open ~= nil or u.edit ~= nil or u.ime ~= nil then
		u.focus, u.edit, u.ime, u.open, u.kb = nil, nil, nil, nil, nil
		self:ui_set(u)
	end
	if focused == self then
		focused = nil
	end
	self:_drop_text_focus()
end

function App:_focus_el(el, tr, how)
	if focused ~= self then
		if focused ~= nil then
			focused:_blur()
		end
		focused = self
	end
	local u = self:ui_get()
	if u.focus == el.id then
		if how == "key" and not u.kb then
			u.kb = true
			self:ui_set(u)
		end
		return
	end
	u.focus = el.id
	u.kb = how == "key"
	u.edit, u.ime = nil, nil
	if u.open ~= nil and u.open ~= el.id then
		u.open = nil
	end
	local ty = T[el.type]
	local rect = nil
	if ty.on_focus then
		rect = ty.on_focus(el, self, u, how)
	end
	self:ui_set(u)
	if rect then
		self:host_caret(rect)
	else
		self:_drop_text_focus()
	end
	self:call(el.on_focus)
end

-- Scroll the scroll containers around `el` so it is in view.
function App:_reveal(el)
	local e = el._parent
	while e do
		if e.type == "scroll" and e._vh then
			local top = el.y - e.y
			local bottom = el.y + el.h - e.y
			local sy = e._sy or 0
			if top < 0 then
				V.scroll_to(e, self, sy + top - 8, nil)
			elseif bottom > e._vh then
				V.scroll_to(e, self, sy + bottom - e._vh + 8, nil)
			end
		end
		e = e._parent
	end
end

function App:_tab(tr, dir)
	local list = tr.focusables
	local n = #list
	if n == 0 then
		return
	end
	local cur = 0
	local fid = tr.u.focus
	for i = 1, n do
		if list[i].id == fid then
			cur = i
			break
		end
	end
	local nxt
	if cur == 0 then
		nxt = if dir > 0 then 1 else n
	else
		nxt = (cur - 1 + dir) % n + 1
	end
	local el = list[nxt]
	self:_focus_el(el, tr, "key")
	self:_reveal(el)
end

-- ---------------------------------------------------------------- pointer

-- (element, layer) under an app-local point; overlays first.
function App:_hit(tr, x, y)
	for _, e in tr.toasts do
		if U.inside(e, x, y) then
			return e, "toast"
		end
	end
	if tr.menu then
		if U.inside(tr.menu, x, y) then
			return tr.menu, "menu"
		end
		return nil, "outside_menu"
	end
	if tr.modal then
		local h = L.hit(tr.modal, x, y)
		if h then
			return h, "dialog"
		end
		return tr.modal, "scrim"
	end
	return L.hit(tr.root, x, y), "main"
end

function App:_dismiss_toast(id)
	local u = self:ui_get()
	if type(u.toasts) ~= "table" then
		return
	end
	local keep = {}
	for _, tt in u.toasts do
		if type(tt) == "table" and tt.id ~= id then
			keep[#keep + 1] = tt
		end
	end
	u.toasts = keep
	self:ui_set(u)
end

function App:_press(tr, el, x, y, clicks)
	local target = el
	while target and not interactive(target) do
		target = target._parent
	end
	local fe = el
	while fe and not focusable(fe) do
		fe = fe._parent
	end
	if fe and not L.disabled(fe) then
		self:_focus_el(fe, tr, "pointer")
	else
		self:_blur()
	end
	if target == nil then
		return false
	end
	if L.disabled(target) then
		return true
	end
	local scroller = target._parent
	while scroller and not T[scroller.type].scroller do
		scroller = scroller._parent
	end
	local dr = { el = target, x0 = x, y0 = y, lx = x, ly = y, clicks = clicks, scroller = scroller, kind = "press" }
	self._drag = dr
	local ty = T[target.type]
	if ty.press then
		local u = self:ui_get()
		u.press = target.id
		self:ui_set(u)
		return "captured"
	end
	local r = ty.down(target, self, x, y, dr)
	if r ~= "captured" then
		self._drag = nil
		return true
	end
	return r
end

function App:_down(x, y)
	local tr = self:_tree()
	local th = self.theme
	-- an interval between two events: os.clock, not the (possibly stale) page clock
	local now = clock()
	local clicks = 1
	local last = self._last_down
	if last and now - last.t <= th.double_click and math.abs(x - last.x) <= 6 and math.abs(y - last.y) <= 6 then
		clicks = last.clicks + 1
	end
	self._last_down = { t = now, x = x, y = y, clicks = clicks }
	local el, layer = self:_hit(tr, x, y)
	if layer == "toast" then
		self:_dismiss_toast(el.tid)
		return true
	elseif layer == "outside_menu" then
		C.toggle_menu(tr.menu.owner, self, false)
		return true
	elseif layer == "menu" then
		self._drag = { kind = "menu", el = el, x0 = x, y0 = y, lx = x, ly = y, clicks = clicks, index = C.menu_index(el, x, y) }
		return "captured"
	elseif layer == "scrim" then
		local dlg = el.dialog
		if dlg.dismissible ~= false then
			self:call(dlg.on_close)
		end
		return true
	end
	if el == nil then
		self:_blur()
		return false
	end
	return self:_press(tr, el, x, y, clicks)
end

function App:_drag_to(x, y)
	local dr = self._drag
	if dr == nil then
		return
	end
	dr.lx, dr.ly = x, y
	if dr.kind == "menu" then
		return
	end
	local tr = self:_tree()
	local el = tr.byid[dr.el.id] or dr.el
	if dr.kind == "press" or dr.kind == "row" then
		local sc = if dr.kind == "row" then el else dr.scroller
		if sc ~= nil then
			sc = tr.byid[sc.id] or sc
		end
		local moved = math.abs(x - dr.x0) + math.abs(y - dr.y0)
		if sc ~= nil and moved > self.theme.drag_slop then
			-- a press that moves becomes a drag of the scroller around it (touch)
			if dr.kind == "press" then
				local u = self:ui_get()
				if u.press ~= nil then
					u.press = nil
					self:ui_set(u)
				end
			end
			dr.kind, dr.el = "scroll", sc
			dr.sy0, dr.sx0 = sc._sy or 0, sc._sx or 0
			dr.x0, dr.y0 = x, y
			return
		end
		if dr.kind == "press" then
			local want = if U.inside(el, x, y) then el.id else nil
			local u = self:ui_get()
			if u.press ~= want then
				u.press = want
				self:ui_set(u)
			end
		end
		return
	end
	local ty = T[el.type]
	if ty.drag then
		ty.drag(el, self, x, y, dr)
	end
end

function App:_up()
	local dr = self._drag
	self._drag = nil
	if dr == nil then
		return
	end
	local tr = self:_tree()
	local x, y = dr.lx, dr.ly
	if dr.kind == "menu" then
		local m = tr.menu
		if m then
			local i = C.menu_index(m, x, y)
			if i and i == dr.index then
				C.toggle_menu(m.owner, self, false)
				C.select_pick(m.owner, self, i)
			end
		end
		return
	end
	local el = tr.byid[dr.el.id] or dr.el
	if dr.kind == "press" then
		local u = self:ui_get()
		if u.press ~= nil then
			u.press = nil
			self:ui_set(u)
		end
		if U.inside(el, x, y) and not L.disabled(el) then
			local ty = T[el.type]
			if ty.activate then
				ty.activate(el, self, "pointer")
			end
		end
		return
	end
	if dr.kind == "scroll" or dr.kind == "vthumb" or dr.kind == "hthumb" then
		return
	end
	local ty = T[el.type]
	if ty.up then
		ty.up(el, self, x, y, dr)
	end
end

function App:_move(x, y)
	local tr = self:_tree()
	local el, layer = self:_hit(tr, x, y)
	local hid, tip = nil, nil
	if layer == "menu" then
		local i = C.menu_index(el, x, y)
		if i and i ~= tr.u.open_hi then
			local u = self:ui_get()
			u.open_hi = i
			self:ui_set(u)
		end
		return
	end
	if el ~= nil and (layer == "main" or layer == "dialog") then
		local e = el
		while e do
			if tip == nil and e.tooltip ~= nil then
				tip = e.id
			end
			if hid == nil and interactive(e) then
				local ty = T[e.type]
				hid = if ty.hover_id then ty.hover_id(e, x, y) else e.id
			end
			if hid ~= nil and tip ~= nil then
				break
			end
			e = e._parent
		end
	end
	if hid ~= self._hover or tip ~= self._tip then
		local u = self:ui_get()
		u.hover = hid
		if u.tip ~= tip then
			u.tip = tip
			u.tip_t = self:now()
		end
		self._hover, self._tip = hid, tip
		self:ui_set(u)
	end
end

function App:_leave()
	if self._hover ~= nil or self._tip ~= nil then
		local u = self:ui_get()
		u.hover, u.tip = nil, nil
		self._hover, self._tip = nil, nil
		self:ui_set(u)
	end
end

function App:_wheel(x, y, dx, dy)
	local tr = self:_tree()
	local el, layer = self:_hit(tr, x, y)
	if layer == "menu" then
		C.menu_wheel(el, self, dy)
		return true
	elseif layer == "outside_menu" or layer == "scrim" then
		return true
	end
	local edge = false
	local e = el
	while e do
		local ty = T[e.type]
		if ty.wheel then
			local r = ty.wheel(e, self, dx, dy)
			if r == "moved" then
				return true
			elseif r == "edge" then
				edge = true
			end
		end
		e = e._parent
	end
	return edge
end

-- ---------------------------------------------------------------- keyboard and text

function App:_key(k)
	local base, ctrl, shift, alt = U.parse_key(k)
	local tr = self:_tree()
	if tr.broken then
		return false
	end
	if tr.menu then
		if C.menu_key(tr.menu, self, base) then
			return true
		end
		if base ~= "tab" then
			return false
		end
		tr = self:_tree()
	end
	if base == "tab" and not ctrl and not alt then
		self:_tab(tr, if shift then -1 else 1)
		return true
	end
	local u = tr.u
	local el = if u.focus ~= nil then tr.byid[u.focus] else nil
	if el and tr.modal and not L.within(el, tr.modal) then
		el = nil
	end
	if el and not L.disabled(el) then
		local ty = T[el.type]
		if ty.key and ty.key(el, self, k, base, ctrl, shift, alt) then
			return true
		end
		if (base == "enter" or base == "space") and not ctrl and not alt and ty.activate then
			ty.activate(el, self, "key")
			return true
		end
	end
	if base == "escape" then
		if tr.modal then
			self:call(tr.modal.dialog.on_close)
			return true
		end
		if u.focus ~= nil then
			self:_blur()
			return true
		end
	end
	local f = self.keys and self.keys[k]
	if f then
		f(self)
		return true
	end
	return false
end

function App:_text_event(kind, s)
	local tr = self:_tree()
	local id = tr.u.focus
	local el = if id ~= nil then tr.byid[id] else nil
	if el == nil or L.disabled(el) then
		return false
	end
	local f = T[el.type][kind]
	if f then
		f(el, self, s)
		return true
	end
	return false
end

-- ---------------------------------------------------------------- public API

function App:toast(msg, opts)
	opts = opts or {}
	local u = self:ui_get()
	local now = self:now()
	local list = {}
	if type(u.toasts) == "table" then
		for _, tt in u.toasts do
			if type(tt) == "table" and (tonumber(tt.ends) or 0) > now then
				list[#list + 1] = tt
			end
		end
	end
	self._toast_seq += 1
	local dur = U.clamp(tonumber(opts.duration) or self.theme.toast_seconds, 0.05, 3600)
	list[#list + 1] = { id = self._toast_seq, text = tostring(msg), tone = opts.tone, ends = now + dur }
	while #list > 4 do
		table.remove(list, 1)
	end
	u.toasts = list
	self:ui_set(u)
end

function App:focus(id)
	local tr = self:_tree()
	local el = tr.byid[tostring(id)]
	if el == nil or not focusable(el) or L.disabled(el) then
		return false
	end
	self:_focus_el(el, tr, "key")
	self:_reveal(el)
	return true
end

function App:blur()
	self:_blur()
end

function App:focused_id()
	return self:ui_get().focus
end

function App:scroll_to(id, y)
	local tr = self:_tree()
	local el = tr.byid[tostring(id)]
	if el == nil then
		return false
	end
	return V.scroll_to(el, self, y, nil)
end

function App:stats()
	local st = table.clone(self._stats)
	st.history_bytes = self._hist_bytes
	return st
end

-- ---------------------------------------------------------------- describing
--
-- The laid-out UI as a flat list of nodes with world rectangles: what an
-- agent (or a test) needs to find, read and operate elements by id. The
-- semantic port cuts lists after 40 entries and turns lists or maps three
-- levels deep into counts, so the agent actions page through the nodes (at
-- most 40 at a time, `next` is the `from` of the following page) and encode
-- list-valued fields (options, tabs, columns) as JSON strings.

local function public_type(e)
	if e.type == "_row" then
		return "row_item"
	elseif e.dialog ~= nil and e.type == "card" then
		return "dialog"
	end
	return e.type
end

local function operable(e)
	return e.type == "_row" or T[e.type].focusable == true or interactive(e)
end

local function type_set(types)
	if types == nil then
		return nil
	end
	local set = {}
	if type(types) == "string" then
		for name in string.gmatch(types, "[^,%s]+") do
			set[name] = true
		end
	elseif type(types) == "table" then
		for _, name in types do
			set[tostring(name)] = true
		end
	else
		error("describe: types must be a list of type names or a comma-separated string", 0)
	end
	return set
end

-- Lists and maps in a node become JSON text, so they survive the port's depth limit.
local function flatten(n)
	for k, v in n do
		if type(v) == "table" then
			n[k] = json.encode(v)
		end
	end
	return n
end

function App:_node(e, parent_id, fid)
	local R = self.rect
	local ty = T[e.type]
	local n = { id = e.id, type = public_type(e), parent = parent_id, x = R[1] + e.x, y = R[2] + e.y, w = e.w, h = e.h }
	if e.type == "text" then
		n.text = e.text
	elseif e.type == "_row" then
		n.index = e.index
	end
	if ty.describe then
		for k, v in ty.describe(e) do
			n[k] = v
		end
	end
	if e.dialog ~= nil then
		n.title = e.dialog.title
	end
	if e.disabled then
		n.disabled = true
	end
	if fid == e.id then
		n.focused = true
	end
	if ty.focusable then
		n.focusable = true
	end
	if e.tooltip ~= nil then
		n.tooltip = tostring(e.tooltip)
	end
	return n
end

-- opts (all optional): from (1-based index into the matching nodes), limit,
-- within (an element id: only its subtree), types (list or "button,textfield"),
-- interactive_only (controls, rows, menus, toasts), flat (list fields as JSON
-- text, at most 40 nodes: what the agent action uses).
-- Returns { app, rect, focus, count (elements laid out), total (matching
-- nodes), from, next (absent on the last page), nodes }.
function App:describe(opts)
	if opts ~= nil and type(opts) ~= "table" then
		error("describe expects { from, limit, within, types, interactive_only }", 0)
	end
	opts = opts or {}
	local flat = opts.flat == true
	local tr = self:_tree()
	local R = self.rect
	local from = max(1, floor(tonumber(opts.from) or 1))
	local limit = floor(tonumber(opts.limit) or (if flat then PORT_LIST else 100000))
	limit = U.clamp(limit, 1, if flat then PORT_LIST else 100000)
	local last = from + limit - 1
	local types = type_set(opts.types)
	local only = opts.interactive_only == true
	local fid = tr.u.focus
	local nodes, total = {}, 0
	local function take(pt, make)
		if (types == nil or types[pt]) then
			total += 1
			if total >= from and total <= last then
				local n = make()
				nodes[#nodes + 1] = if flat then flatten(n) else n
			end
		end
	end
	local function visit(e, parent_id)
		if e.x == nil then
			return
		end
		local ty = T[e.type]
		if not only or operable(e) then
			take(public_type(e), function()
				return self:_node(e, parent_id, fid)
			end)
		end
		local kids = e.children
		if kids and not ty.leaf then
			for i = 1, #kids do
				visit(kids[i], e.id)
			end
		end
	end
	if opts.within ~= nil then
		local start = tr.byid[tostring(opts.within)]
		if start == nil then
			error("describe: no element \"" .. tostring(opts.within) .. "\" in app " .. self.name, 0)
		end
		visit(start, start._parent and start._parent.id)
	else
		visit(tr.root, nil)
		for _, p in tr.dialogs do
			visit(p, nil)
		end
		local m = tr.menu
		if m then
			take("menu", function()
				local labels = {}
				for i, o in m.options do
					labels[i] = U.label_of(o)
				end
				return { id = m.id, type = "menu", owner = m.owner.id, x = R[1] + m.x, y = R[2] + m.y, w = m.w, h = m.h, options = labels, highlighted = m.hi, first = m.first, item_h = m.ih }
			end)
		end
		for _, e in tr.toasts do
			take("toast", function()
				return { id = e.id, type = "toast", text = e.text, x = R[1] + e.x, y = R[2] + e.y, w = e.w, h = e.h }
			end)
		end
	end
	local res = { app = self.name, rect = { R[1], R[2], R[3], R[4] }, focus = fid, count = tr.count, total = total, from = from, nodes = nodes }
	if total > last then
		res.next = last + 1
	end
	return res
end

-- The entries of a select, radio group, tab bar, list or table, a page at a
-- time: opts { id, from, limit, flat }. Returns { id, type, count, from,
-- next, selected, items } (labels; for a table, rows of cell values, plus
-- columns).
function App:items(opts)
	if type(opts) ~= "table" or opts.id == nil then
		error("items expects { id, from, limit }", 0)
	end
	local tr = self:_tree()
	local el = tr.byid[tostring(opts.id)]
	if el == nil then
		error("no element \"" .. tostring(opts.id) .. "\" in app " .. self.name, 0)
	end
	local flat = opts.flat == true
	local from = max(1, floor(tonumber(opts.from) or 1))
	local limit = U.clamp(floor(tonumber(opts.limit) or PORT_LIST), 1, if flat then PORT_LIST else 100000)
	local t = el.type
	local res = { id = el.id, type = t, selected = el.selected }
	local count, get
	if t == "select" or t == "radio" or t == "tabs" then
		local o = (if t == "tabs" then el.tabs else el.options) or {}
		count = #o
		get = function(i)
			return U.label_of(o[i])
		end
	elseif t == "list" then
		count = el._n or 0
		get = function(i)
			return U.label_of(V.list_item(el, i))
		end
	elseif t == "table" then
		count = el._n or 0
		local cols = {}
		for c, v in el._cols or {} do
			cols[c] = U.label_of(v)
		end
		res.columns = cols
		get = function(i)
			local row = V.table_row(el, i)
			local out = {}
			if type(row) == "table" then
				for c = 1, #cols do
					local v = row[c]
					out[c] = if type(v) == "table" then U.label_of(v) else v
				end
			end
			return out
		end
	else
		error(t .. " \"" .. el.id .. "\" has no items (select, radio, tabs, list, table)", 0)
	end
	local last = min(count, from + limit - 1)
	local items = {}
	for i = from, last do
		items[#items + 1] = get(i)
	end
	res.count, res.from, res.items = count, from, items
	if last < count then
		res.next = last + 1
	end
	if flat and res.columns then
		res.columns = json.encode(res.columns)
	end
	return res
end

-- Operate an element as a user would: { id, op = "click" | "focus" | "type" |
-- "set" | "key", value }. For agents (the `<name>.do` action) and tests.
-- `set` goes through the same rules as typing or dragging: read-only fields
-- refuse it, one-line fields drop line breaks, max_length cuts, sliders snap
-- to their step.
function App:operate(p)
	if type(p) ~= "table" then
		error("expected { id, op, value }", 0)
	end
	local op = p.op or "click"
	if op == "key" then
		return { ok = self:_key(tostring(p.value)) }
	end
	local tr = self:_tree()
	local el = tr.byid[tostring(p.id)]
	if el == nil then
		error("no element \"" .. tostring(p.id) .. "\" in app " .. self.name, 0)
	end
	if L.disabled(el) then
		error("element \"" .. el.id .. "\" is disabled", 0)
	end
	local ty = T[el.type]
	local v = p.value
	local res = { ok = true }
	if op == "click" then
		if ty.activate then
			ty.activate(el, self, "agent")
		elseif el.type == "_row" then
			V.list_select(el._parent, self, el.index)
		elseif el.type == "canvas" and el.on_click then
			self:call(el.on_click, el.w / 2, el.h / 2)
		else
			error(el.type .. " \"" .. el.id .. "\" cannot be clicked", 0)
		end
	elseif op == "focus" then
		if not focusable(el) then
			error(el.type .. " \"" .. el.id .. "\" cannot take focus", 0)
		end
		self:_focus_el(el, tr, "agent")
	elseif op == "type" then
		if not ty.text then
			error(el.type .. " \"" .. el.id .. "\" does not take text", 0)
		end
		self:_focus_el(el, tr, "agent")
		local fresh = self:_tree().byid[el.id] or el
		ty.text(fresh, self, tostring(v or ""))
	elseif op == "set" then
		local t = el.type
		local what = t .. " \"" .. el.id .. "\""
		if t == "textfield" or t == "textarea" then
			local applied = F.set_value(el, self, v)
			if not el.password then
				res.value = applied
			end
		elseif t == "checkbox" or t == "switch" then
			if type(v) ~= "boolean" then
				error(what .. ": value must be true or false", 0)
			end
			if v ~= (el.checked == true) then
				self:call(el.on_change, v)
			end
		elseif t == "slider" then
			local n = tonumber(v)
			if n == nil then
				error(what .. ": value must be a number", 0)
			end
			local sv = C.slider_snap(el, n)
			if sv ~= el.value then
				self:call(el.on_change, sv)
			end
			res.value = sv
		elseif t == "select" or t == "radio" or t == "tabs" then
			local opts = (if t == "tabs" then el.tabs else el.options) or {}
			local n = #opts
			local i = tonumber(v)
			if i == nil or i ~= floor(i) or i < 1 or i > n then
				error(what .. ": index " .. tostring(v) .. " is out of range 1.." .. n, 0)
			end
			if i ~= el.selected then
				self:call(el.on_change, i, opts[i])
			end
		elseif t == "list" or t == "table" then
			local i = tonumber(v)
			if i == nil or i ~= floor(i) or i < 1 or i > (el._n or 0) then
				error(what .. ": row " .. tostring(v) .. " is out of range 1.." .. tostring(el._n or 0), 0)
			end
			if t == "list" then
				V.list_select(el, self, i)
			else
				V.table_select(el, self, i)
			end
		else
			error(what .. " has no value to set", 0)
		end
	else
		error("unknown op \"" .. tostring(op) .. "\" (click, focus, type, set, key)", 0)
	end
	return res
end

-- ---------------------------------------------------------------- routing

local function app_at(x, y)
	for i = #apps, 1, -1 do
		local a = apps[i]
		local r = a.rect
		if x >= r[1] and y >= r[2] and x < r[1] + r[3] and y < r[2] + r[4] then
			return a
		end
	end
	return nil
end

local function install()
	if installed then
		return
	end
	installed = true
	page.on("pointer_down", function(x, y)
		local a = app_at(x, y)
		for _, o in apps do
			if o ~= a then
				o:_blur()
			end
		end
		if a == nil then
			return false
		end
		local r = a:_down(x - a.rect[1], y - a.rect[2])
		if r == "captured" then
			captured = a
		end
		return r
	end)
	page.on("pointer_drag", function(x, y)
		local a = captured
		if a == nil then
			return false
		end
		a:_drag_to(x - a.rect[1], y - a.rect[2])
		return true
	end)
	-- Known gap: the engine also calls pointer_up when a second finger lands
	-- (a pinch starts) and when the host cancels the pointer, and the event
	-- carries no position or reason. A press held on a button is then
	-- activated as if released there (CORE_REQUESTS #11 asks for a separate
	-- pointer_cancel event).
	page.on("pointer_up", function()
		local a = captured
		if a == nil then
			return false
		end
		captured = nil
		a:_up()
		return true
	end)
	page.on("pointer_move", function(x, y)
		local a = app_at(x, y)
		if hovered ~= nil and hovered ~= a then
			hovered:_leave()
		end
		hovered = a
		if a ~= nil then
			a:_move(x - a.rect[1], y - a.rect[2])
		end
		return false
	end)
	page.on("wheel", function(x, y, dx, dy)
		local a = app_at(x, y)
		if a == nil then
			return false
		end
		return a:_wheel(x - a.rect[1], y - a.rect[2], dx, dy)
	end)
	page.on("key", function(k)
		local a = focused
		if a == nil then
			local base, _, shift = U.parse_key(k)
			if base ~= "tab" or #apps == 0 then
				return false
			end
			a = if shift then apps[#apps] else apps[1]
		end
		return a:_key(k)
	end)
	page.on("text", function(s)
		return focused ~= nil and focused:_text_event("text", s)
	end)
	page.on("ime", function(s)
		return focused ~= nil and focused:_text_event("ime", s)
	end)
	page.on("paste", function(s)
		return focused ~= nil and focused:_text_event("paste", s)
	end)
end

-- ---------------------------------------------------------------- creation

local function rect_of(r)
	if type(r) ~= "table" then
		error("ui.app: rect must be { x, y, w, h }", 0)
	end
	local x, y, w, h = tonumber(r[1]), tonumber(r[2]), tonumber(r[3]), tonumber(r[4])
	if x == nil or y == nil or w == nil or h == nil or w <= 0 or h <= 0 then
		error("ui.app: rect must be { x, y, w, h } with w, h > 0", 0)
	end
	return { x, y, w, h }
end

function A.app(spec)
	if type(spec) ~= "table" then
		error("ui.app expects a table { rect, view, state, inputs, theme, name }", 0)
	end
	if type(spec.view) ~= "function" then
		error("ui.app: view must be a function(state, inputs...) returning elements", 0)
	end
	local name = spec.name or ("app" .. (#apps + 1))
	if type(name) ~= "string" or name == "" then
		error("ui.app: name must be a non-empty string", 0)
	end
	if by_name[name] then
		error("ui.app: there is already an app named \"" .. name .. "\" on this page", 0)
	end
	local theme = spec.theme
	if theme == nil or theme == "dark" then
		theme = Theme.dark
	elseif theme == "light" then
		theme = Theme.light
	elseif type(theme) ~= "table" then
		error("ui.app: theme must be ui.themes.dark, ui.themes.light or a table", 0)
	end
	local state = spec.state
	if state == nil then
		state = {}
	elseif type(state) ~= "table" then
		error("ui.app: state must be a table", 0)
	end
	local self = setmetatable({}, App)
	self.name = name
	self.rect = rect_of(spec.rect)
	self.theme = theme
	self.view = spec.view
	self.keys = spec.keys
	self.limit = U.clamp(tonumber(spec.max_elements) or 20000, 100, 100000)
	self.blink = spec.blink ~= false
	self.background = spec.background
	self._rev = 0
	self._hist = {}
	self._hist_bytes = 0
	self._hist_seq = 0
	self._toast_seq = 0
	self._stats = { frames = 0, view_ms = 0, layout_ms = 0, draw_ms = 0, total_ms = 0, elements = 0, commands = 0 }
	self.state = page.input(name .. ".state", state, { agent = spec.agent == true })
	self.ui = page.input(name .. ".ui", {})
	local inputs = { page.clock(), self.state, self.ui }
	local extra = spec.inputs or {}
	for i = 1, #extra do
		inputs[#inputs + 1] = extra[i]
	end
	self._nin = #extra
	page.panel({
		rect = self.rect,
		name = name,
		id = spec.id,
		class = spec.class,
		priority = spec.priority,
		inputs = inputs,
		draw = function(d, t, s, u, ...)
			return self:_draw(d, t, s, u, ...)
		end,
	})
	apps[#apps + 1] = self
	by_name[name] = self
	install()
	if spec.agent then
		page.action({
			name = name .. ".describe",
			description = "The UI of app "
				.. name
				.. ": elements with ids, types, labels, values and world rectangles, up to 40 per call; "
				.. "pass `next` from the result as `from` for more. List fields (options, tabs, columns) are JSON text.",
			params = {
				from = "1-based index of the first node (default 1)",
				limit = "nodes per page, at most 40",
				within = "element id: only that element and what is inside it",
				types = "element types, e.g. \"button,textfield\"",
				interactive_only = "true: only controls, rows, menus and toasts",
			},
			run = function(p)
				local o = if type(p) == "table" then table.clone(p) else {}
				o.flat = true
				return self:describe(o)
			end,
		})
		page.action({
			name = name .. ".items",
			description = "The options of a select, radio group or tab bar, the items of a list, or the rows of a table in app "
				.. name
				.. ", up to 40 per call; pass `next` as `from` for more",
			params = { id = "element id", from = "1-based index (default 1)", limit = "at most 40" },
			run = function(p)
				local o = if type(p) == "table" then table.clone(p) else {}
				o.flat = true
				return self:items(o)
			end,
		})
		page.action({
			name = name .. ".do",
			description = "Operate an element of app "
				.. name
				.. ": { id, op = click | focus | type | set | key, value }. set follows the rules of typing: "
				.. "read-only refuses, one-line fields drop line breaks, max_length cuts, sliders snap to their step.",
			params = { id = "element id", op = "click | focus | type | set | key", value = "text, number, boolean or key name" },
			run = function(p)
				return self:operate(p)
			end,
		})
	end
	return self
end

function A.apps()
	return table.clone(apps)
end

return A
