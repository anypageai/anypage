-- AnyPage home: the launcher. A card for every page package this app ships
-- (catalog.luau), with the modules each one uses and whether this platform
-- has them; tapping a card (or Enter on it) opens the page with
-- page.navigate(name).
--
-- Two layouts, two ui.apps, one catalog:
--   wide    desktop and tablet windows: the page.json bounds (16:10), a
--           three-column grid and a side column (about, this device, keys)
--   narrow  phones in portrait: 400 x 820 units, one column that scrolls
-- A page cannot see the window's size, so the platform picks the first
-- layout (Android and iOS start narrow) and page.fit_rect shows it; the
-- button in each header switches to the other one. The narrow app sits far
-- to the right of the wide one, so fitting either never shows the other,
-- and the engine draws only the panel in view.
--
-- Modules: page.json declares every module as optional, so a module's
-- table is nil exactly when this platform (or this build) lacks it. A page
-- whose required module is missing is marked and not opened.
--
-- Keyboard: arrows move between cards, Home/End jump, Enter opens, Tab
-- walks every control (the library), R fits the layout to the window.
-- Agents: actions open_page { name }, list_pages, set_layout { layout },
-- plus wide.* / narrow.* (describe, items, do) from the UI library.

local ui = import("ui")
local L = import("ui/layout")
local PAGES = import("catalog")

local rgba = color.rgba
local T = L.types
local floor, max, min = math.floor, math.max, math.min

-- ---------------------------------------------------------------- geometry

local WIDE = { 0, 0, 1440, 900 }
local NARROW = { 2400, 0, 400, 820 }
local RECTS = { wide = WIDE, narrow = NARROW }
-- grid columns, for the up and down arrows
local COLS = { wide = 3, narrow = 1 }
-- Five rows of cards fit the wide layout. Only a longer catalog puts them
-- in a scroll container: a clip makes the panel's raster about twice as
-- slow (the canvas rebuilds a full-size mask and fills through it; see
-- NOTES.md), and the wide panel re-rasters on every hover.
local WIDE_SCROLLS = #PAGES > COLS.wide * 5

local TH = ui.theme({ bg = rgba(18, 20, 25) }, ui.themes.dark)
local WHITE = rgba(255, 255, 255)

-- `c` with `tint` laid over it at `a` / 255, as one opaque colour: a card
-- fills once instead of twice (fills are most of the raster time here).
local function mix(c, tint, a)
	local function ch(v, k)
		return floor(v / 256 ^ k) % 256
	end
	local t = a / 255
	local function m(k)
		return floor(ch(c, k) * (1 - t) + ch(tint, k) * t + 0.5)
	end
	return rgba(m(3), m(2), m(1), ch(c, 0))
end

-- ---------------------------------------------------------------- platform and modules

local PLATFORM = tostring(if page.info and page.info.platform then page.info.platform else "unknown")
local PLATFORM_NAME = ({ windows = "Windows", android = "Android", linux = "Linux", macos = "macOS", ios = "iOS" })[PLATFORM]
	or PLATFORM
local MOBILE = PLATFORM == "android" or PLATFORM == "ios"

local MODULES = {
	{ name = "pdf", what = "PDF" },
	{ name = "sheet", what = "試算表" },
	{ name = "cad", what = "CAD" },
	{ name = "scene", what = "3D" },
	{ name = "media", what = "影音" },
	{ name = "cloud", what = "雲端" },
	{ name = "ble", what = "藍牙" },
	{ name = "usb", what = "USB" },
	{ name = "serial", what = "序列埠" },
	{ name = "demo", what = "示範" },
}

-- A module's global is nil when the platform lacks it. Never index these
-- tables blindly: a module with only streams or jobs is a stub that raises
-- on any field.
local TABLES = {
	pdf = pdf,
	sheet = sheet,
	cad = cad,
	scene = scene,
	media = media,
	cloud = cloud,
	ble = ble,
	usb = usb,
	serial = serial,
	demo = demo,
}

local HAS = { ui = true } -- ui: the Luau library this page itself runs on
local NOTE = {} -- present, but says it cannot work now (a device bridge missing)
local HAVE_COUNT = 0
for _, m in MODULES do
	local t = TABLES[m.name]
	HAS[m.name] = t ~= nil
	if t ~= nil then
		HAVE_COUNT += 1
		if m.name == "ble" or m.name == "usb" or m.name == "serial" then
			local ok, r = pcall(function()
				return t.available()
			end)
			if ok and type(r) == "table" and r.ok == false then
				NOTE[m.name] = tostring(r.reason or "not available")
			end
		end
	end
end

local BY_NAME = {}
local INDEX = {}
for i, p in PAGES do
	BY_NAME[p.name] = p
	INDEX[p.name] = i
	p.modules = p.modules or {}
	p.optional = p.optional or {}
	local missing, absent = {}, {}
	for _, m in p.modules do
		if not HAS[m] then
			missing[#missing + 1] = m
		end
	end
	for _, m in p.optional do
		if not HAS[m] then
			absent[#absent + 1] = m
		end
	end
	p.missing, p.absent = missing, absent
	p.can_open = #missing == 0
	-- card fills: normal, hovered, pressed
	p.fills = { mix(TH.surface, p.color, 16), mix(TH.surface2, p.color, 30), mix(TH.surface3, p.color, 40) }
end

-- ---------------------------------------------------------------- the launch card element
--
-- A card that is one control: focusable (Tab, arrows), pressed and hovered
-- as a whole, activated by a tap, a click or Enter. Its children are laid
-- out like a ui.card's. A press that moves turns into a scroll of the list
-- around it (the library does that for every pressable element).

T.launch_card = {
	fill = true,
	focusable = true,
	press = true,
	check = function(el)
		L.need_fns(el, "on_click")
	end,
	measure = function(el, aw, ah, cx)
		if el.padding == nil then
			el.padding = 16
		end
		if el.gap == nil then
			el.gap = 10
		end
		el.radius = el.radius or 12
		return T.column.measure(el, aw, ah, cx)
	end,
	arrange = T.column.arrange,
	draw = function(el, dc)
		local th, d = dc.th, dc.d
		local x, y, w, h, r = el.x, el.y, el.w, el.h, el.radius
		local hov, prs, foc = dc.hover == el.id, dc.press == el.id, dc.focus == el.id
		local f = el.fills or { th.surface, th.surface2, th.surface3 }
		-- the shadow only where it shows: a sliver under the bottom edge
		d:rrect(x + 2, y + h - 2 * r, w - 4, 2 * r + 3, r, th.shadow)
		d:rrect(x, y, w, h, r, if prs then f[3] elseif hov then f[2] else f[1])
		local edge = if foc then th.accent elseif hov then th.border_strong else th.border
		d:stroke(x + 0.5, y + 0.5, w - 1, h - 1, r, if foc then 1.5 else 1, edge)
	end,
	activate = function(el, app)
		app:call(el.on_click)
	end,
	describe = function(el)
		return { label = el.label, page = el.page, can_open = el.can_open }
	end,
}

-- ---------------------------------------------------------------- opening pages

local apps = {}
local layout = page.input("layout", if MOBILE then "narrow" else "wide")

local function active()
	return apps[page.get(layout)] or apps.wide
end

-- Ask the host for page `name`. Returns true, entry or false, message.
local function open(name)
	local p = BY_NAME[name]
	if p == nil then
		return false, "沒有名為「" .. tostring(name) .. "」的頁面 (no page \"" .. tostring(name) .. "\")"
	end
	if not p.can_open then
		return false, "「" .. p.title .. "」需要 " .. table.concat(p.missing, "、") .. " 模組，此平台沒有"
	end
	page.navigate(p.name)
	return true, p
end

local function open_card(p, app)
	local ok, r = open(p.name)
	if ok then
		app:set("opened", p.name)
		app:toast("正在開啟「" .. p.title .. "」…")
	else
		app:toast(r, { tone = "danger" })
	end
end

local function set_layout(mode)
	if RECTS[mode] == nil then
		error("layout must be \"wide\" or \"narrow\", got " .. tostring(mode), 0)
	end
	for m, a in apps do
		if m ~= mode then
			a:blur()
		end
	end
	page.set(layout, mode)
	page.fit_rect(RECTS[mode])
end

-- ---------------------------------------------------------------- pieces of the views

local function utf8_len(s)
	return utf8.len(s) or #s
end

-- The coloured square with the page's mark.
local function icon(p, size)
	local n = utf8_len(p.mark)
	local fs = if n <= 1 then size * 0.46 elseif n == 2 then size * 0.36 else size * 0.3
	return ui.stack({ width = size, height = size, bg = p.color, radius = floor(size / 4), align = "center" }, {
		ui.text(p.mark, { size = fs, color = WHITE, wrap = false }),
	})
end

local function logo(size)
	return ui.stack({ width = size, height = size, bg = TH.accent, radius = floor(size / 4), align = "center" }, {
		ui.text("A", { size = size * 0.55, color = WHITE, wrap = false }),
	})
end

-- One badge per module the page uses: green when this platform has it,
-- red when a required one is missing (the page cannot open), grey when an
-- optional one is missing.
local function module_badges(p)
	local out = {}
	for _, m in p.modules do
		if HAS[m] then
			out[#out + 1] = ui.badge(m, { tone = if NOTE[m] then "warning" else "success", tooltip = NOTE[m] })
		else
			out[#out + 1] = ui.badge(m .. " 缺", { tone = "danger", tooltip = "此平台沒有 " .. m .. " 模組" })
		end
	end
	for _, m in p.optional do
		if HAS[m] then
			out[#out + 1] = ui.badge(m, { tone = if NOTE[m] then "warning" else "success", tooltip = NOTE[m] or "選用模組 optional" })
		else
			out[#out + 1] = ui.badge(m .. " 無", { tone = "neutral", tooltip = "選用模組，此平台沒有" })
		end
	end
	if p.ui then
		out[#out + 1] = ui.badge("ui", { tone = "accent" })
	end
	if #out == 0 then
		out[1] = ui.badge("核心 core", { tone = "neutral", tooltip = "只用引擎核心，不需要模組" })
	end
	return out
end

local function status_badge(p)
	if not p.can_open then
		return ui.badge("無法開啟", { tone = "danger", solid = true })
	elseif #p.absent > 0 and #p.absent == #p.optional and #p.modules == 0 and not p.ui then
		return ui.badge("無可用模組", { tone = "warning" })
	elseif #p.absent > 0 then
		return ui.badge("部分功能", { tone = "warning" })
	elseif p.new then
		return ui.badge("新", { tone = "accent", solid = true })
	end
	return nil
end

local function page_card(p, compact)
	local isz = if compact then 40 else 44
	return {
		type = "launch_card",
		id = "card:" .. p.name,
		label = p.title,
		page = p.name,
		can_open = p.can_open,
		fills = p.fills,
		padding = if compact then 14 else 16,
		gap = 10,
		on_click = function(app)
			open_card(p, app)
		end,
		children = L.kids({
			ui.row({ gap = 12, align = "start" }, {
				icon(p, isz),
				ui.column({ gap = 4, grow = 1 }, {
					ui.row({ gap = 8 }, {
						ui.text(p.title, { size = if compact then 16 else 17, wrap = false }),
						ui.text(p.en, { size = 12.5, tone = "faint", wrap = false }),
						ui.spacer(),
						status_badge(p),
					}),
					ui.text(p.desc, { size = 13.5, tone = "dim", max_lines = 2 }),
				}),
			}),
			ui.row({ gap = 6 }, {
				module_badges(p),
				ui.spacer(),
				ui.text("pages/" .. p.name, { size = 11.5, tone = "faint", wrap = false }),
			}),
		}),
	}
end

local function cards(compact)
	local out = {}
	for i, p in PAGES do
		out[i] = page_card(p, compact)
	end
	return out
end

-- A titled box: like ui.card, without its full-size shadow fill.
local function section(title, compact, children)
	local kids = { ui.text(title, { size = if compact then 16 else 17 }) }
	for _, k in L.kids(children) do
		kids[#kids + 1] = k
	end
	return ui.column({ bg = TH.surface, radius = TH.radius + 2, border = true, padding = if compact then 14 else 16, gap = 8 }, kids)
end

local function about_card(compact)
	local fs = if compact then 13.5 else 14
	return section("關於 AnyPage", compact, {
		ui.text("AnyPage 是一個引擎：PDF、試算表、CAD 圖面、3D、影音與裝置，都由同一張計算圖畫在同一頁上，頁面就是 App。", { size = fs }),
		ui.text("本機功能免費且開源。雲端服務（AI 聊天、S3、GCS）是選用的，用你自己的帳號與金鑰，不用也能做完本機的事。", { size = fs, tone = "dim" }),
		ui.text("Local features are free and open source; cloud services are optional.", { size = fs - 1, tone = "faint" }),
	})
end

local function device_card(compact)
	local badges = {}
	for i, m in MODULES do
		local note = NOTE[m.name]
		local tone = if not HAS[m.name] then "danger" elseif note then "warning" else "success"
		local label = m.name .. (if HAS[m.name] then "" else " 缺")
		badges[i] = ui.badge(label, { tone = tone, tooltip = m.what .. " · " .. (note or (if HAS[m.name] then "可用" else "此平台沒有")) })
	end
	return section("此裝置 This device", compact, {
		ui.row({ gap = 8 }, {
			ui.text("平台", { size = 14, tone = "dim" }),
			ui.text(PLATFORM_NAME, { size = 14 }),
			ui.spacer(),
			ui.text("模組 " .. HAVE_COUNT .. " / " .. #MODULES, { size = 13, tone = "dim" }),
		}),
		ui.grid({ columns = 4, gap = 6 }, badges),
		ui.text("綠：可用　紅：缺少　灰：選用但沒有", { size = 12, tone = "faint" }),
	})
end

local function key_row(k, what)
	return ui.row({ gap = 10 }, {
		ui.badge(k, { tone = "neutral", width = 86 }),
		ui.text(what, { size = 13.5, tone = "dim" }),
	})
end

local function keys_card()
	return section("鍵盤 Keyboard", false, {
		key_row("← → ↑ ↓", "選擇頁面"),
		key_row("Enter", "開啟選到的頁面"),
		key_row("Home / End", "第一個 / 最後一個"),
		key_row("Tab", "在所有按鈕間移動"),
		key_row("R", "畫面重新置中"),
	})
end

-- ---------------------------------------------------------------- the two layouts

local function wide_view(s)
	return ui.column({ padding = 28, gap = 20 }, {
		ui.row({ gap = 14 }, {
			logo(48),
			ui.column({ gap = 2 }, {
				ui.text("AnyPage", { size = 28, wrap = false }),
				ui.text("選一個頁面開始 · Choose a page to open", { size = 14, tone = "dim", wrap = false }),
			}),
			ui.spacer(),
			ui.badge("平台 " .. PLATFORM_NAME, { tone = "accent" }),
			ui.badge(#PAGES .. " 個頁面", { tone = "neutral" }),
			ui.button("手機版面", {
				id = "to-narrow",
				variant = "secondary",
				size = "small",
				tooltip = "切換到直向的手機版面 (phone layout)",
				on_click = function()
					set_layout("narrow")
				end,
			}),
		}),
		ui.row({ gap = 24, align = "stretch", grow = 1 }, {
			if WIDE_SCROLLS
				then ui.scroll({ id = "pages", grow = 1 }, ui.grid({ columns = COLS.wide, gap = 14, padding = { 4, 12, 8, 4 } }, cards(false)))
				else ui.grid({ id = "pages", columns = COLS.wide, gap = 14, padding = 4, grow = 1 }, cards(false)),
			ui.column({ id = "side", width = 340, gap = 14, padding = 4 }, {
				about_card(false),
				device_card(false),
				keys_card(),
			}),
		}),
	})
end

local function narrow_view(s)
	local body = cards(true)
	body[#body + 1] = about_card(true)
	body[#body + 1] = device_card(true)
	return ui.column({ padding = { 18, 14, 10, 14 }, gap = 12 }, {
		ui.row({ gap = 10 }, {
			logo(38),
			ui.column({ gap = 0 }, {
				ui.text("AnyPage", { size = 22, wrap = false }),
				ui.text("本機免費開源 · 雲端服務選用", { size = 13, tone = "dim", wrap = false }),
			}),
			ui.spacer(),
			ui.badge(PLATFORM_NAME, { tone = "accent" }),
			ui.button("寬版", {
				id = "to-wide",
				variant = "secondary",
				size = "small",
				tooltip = "切換到寬版面 (desktop layout)",
				on_click = function()
					set_layout("wide")
				end,
			}),
		}),
		ui.scroll({ id = "pages", grow = 1 }, ui.column({ gap = 10, padding = { 3, 10, 8, 3 } }, body)),
	})
end

-- ---------------------------------------------------------------- keyboard
--
-- Registered before the apps, so it sees keys first: arrows move the focus
-- between cards even when nothing has focus yet (the library only routes
-- Tab then). Enter on a focused card is the library's: it activates it.

local function card_index(id)
	if type(id) ~= "string" then
		return nil
	end
	local name = string.match(id, "^card:(.+)$")
	return if name then INDEX[name] else nil
end

page.on("key", function(k)
	local mode = page.get(layout)
	local app = apps[mode]
	if app == nil then
		return false
	end
	if k == "r" then
		page.fit_rect(RECTS[mode])
		return true
	end
	local n = #PAGES
	local cols = COLS[mode] or 1
	local fid = app:focused_id()
	local cur = card_index(fid)
	local nxt
	if k == "left" or k == "right" then
		nxt = if cur == nil then 1 elseif k == "left" then max(1, cur - 1) else min(n, cur + 1)
	elseif k == "up" then
		nxt = if cur == nil then 1 elseif cur - cols >= 1 then cur - cols else cur
	elseif k == "down" then
		if cur == nil then
			nxt = 1
		elseif cur + cols <= n then
			nxt = cur + cols
		elseif (cur - 1) // cols < (n - 1) // cols then
			nxt = n -- a partial last row: its last card
		else
			nxt = cur
		end
	elseif k == "home" then
		nxt = 1
	elseif k == "end" then
		nxt = n
	elseif (k == "enter" or k == "space") and fid == nil then
		nxt = 1 -- the first Enter shows where the focus is; the next opens
	elseif (k == "tab" or k == "shift+tab") and fid == nil then
		-- with nothing focused the library would send Tab to the first app
		-- created (the wide one), also while the narrow one is shown
		nxt = if k == "tab" then 1 else n
	else
		return false
	end
	app:focus("card:" .. PAGES[nxt].name)
	return true
end)

-- ---------------------------------------------------------------- the apps

apps.wide = ui.app({
	name = "wide",
	rect = WIDE,
	theme = TH,
	state = { opened = "" },
	agent = true,
	blink = false,
	-- no full-panel fill: the engine's clear colour (about rgb 18 20 25) is
	-- the page background, and a hover re-rasters ~2.6 M fewer pixels
	background = false,
	view = wide_view,
})

apps.narrow = ui.app({
	name = "narrow",
	rect = NARROW,
	theme = TH,
	state = { opened = "" },
	agent = true,
	blink = false,
	background = false,
	view = narrow_view,
})

if MOBILE then
	page.fit_rect(NARROW)
end

-- ---------------------------------------------------------------- agents

page.action({
	name = "open_page",
	description = "Open another page package of this app by its folder name, as tapping its card does. "
		.. "list_pages gives the names and which ones this platform can open.",
	params = { name = "package folder name, e.g. \"pdf-demo\"" },
	run = function(params)
		local name = if type(params) == "table" then params.name else params
		if type(name) ~= "string" or name == "" then
			error("open_page needs { name = \"<package folder>\" }", 0)
		end
		local ok, r = open(name)
		if not ok then
			error(r, 0)
		end
		local app = active()
		app:set("opened", name)
		app:toast("正在開啟「" .. r.title .. "」…")
		return { ok = true, page = name, title = r.title }
	end,
})

page.action({
	name = "list_pages",
	description = "The page packages this launcher shows: name, title, description, modules it needs and uses, "
		.. "what is missing on this platform, and whether it can be opened here.",
	params = {},
	run = function()
		local out = {}
		for i, p in PAGES do
			out[i] = {
				name = p.name,
				title = p.title,
				en = p.en,
				description = p.desc,
				modules = table.concat(p.modules, ","),
				optional = table.concat(p.optional, ","),
				missing = table.concat(p.missing, ","),
				absent_optional = table.concat(p.absent, ","),
				can_open = p.can_open,
			}
		end
		local have = {}
		for _, m in MODULES do
			if HAS[m.name] then
				have[#have + 1] = m.name
			end
		end
		return { platform = PLATFORM, layout = page.get(layout), modules = table.concat(have, ","), pages = out }
	end,
})

page.action({
	name = "set_layout",
	description = "Show the wide (desktop) or narrow (phone portrait) layout and fit it to the window.",
	params = { layout = "\"wide\" or \"narrow\"" },
	run = function(params)
		local mode = if type(params) == "table" then params.layout else params
		set_layout(mode)
		return { ok = true, layout = mode }
	end,
})
