-- AnyPage UI gallery: every element of import("ui"), in the dark theme on the
-- left and the light theme on the right. Each side is its own app with its
-- own state cell, so they can be used independently.

local ui = import("ui")
local alpha = color.alpha

local W, H = 1180, 1500

local FRUITS = { "Apple", "Banana", "Cherry", "Durian", "Elderberry", "Fig", "Grape", "Honeydew", "Kiwi", "Lemon", "Mango" }

local LOREM = "Text wraps to the width it is given. Resize nothing: the layout engine measures every line "
	.. "with the host's text system, so 中文、English and mixed lines break where they should."

-- 1 000 rows in the shared table format, made once at load
local COLUMNS = { "ID", "Name", "City", "Score", "Active" }
local CITIES = { "Taipei", "Kaohsiung", "Tainan", "Taichung", "Hsinchu", "Keelung" }
local NAMES = { "陳", "林", "黃", "張", "李", "王", "吳", "劉", "蔡", "楊" }
local ROWS = {}
for i = 1, 1000 do
	ROWS[i] = {
		i,
		NAMES[(i * 7) % #NAMES + 1] .. " " .. string.char(65 + i % 26) .. ".",
		CITIES[(i * 3) % #CITIES + 1],
		(i * 37) % 1000 / 10,
		i % 3 ~= 0,
	}
end

-- sorting is a pure function of the sort state; results are memoised
local sorted_cache = {}
local function sorted_rows(sort)
	if type(sort) ~= "table" or sort.col == nil then
		return ROWS
	end
	local key = sort.col .. (if sort.desc then "d" else "a")
	local hit = sorted_cache[key]
	if hit then
		return hit
	end
	local rows = table.clone(ROWS)
	local c, desc = sort.col, sort.desc
	table.sort(rows, function(a, b)
		local x, y = a[c], b[c]
		if type(x) == "boolean" then
			x, y = (if x then 1 else 0), (if y then 1 else 0)
		end
		if x == y then
			return a[1] < b[1]
		end
		if desc then
			return x > y
		end
		return x < y
	end)
	sorted_cache[key] = rows
	return rows
end

-- State cells are readable by agents ({"describe": true}, {"get": "dark.state"}),
-- so a password is not kept there: it lives in this script, and the state
-- only counts its edits (which is what makes the view draw again).
local secrets = { dark = "", light = "" }

-- on_change(value, ..., app) -> app:set(key, value); the app is always the last argument
local function setter(key)
	return function(v, ...)
		local app = select(select("#", ...), ...)
		app:set(key, v)
	end
end

local function tile(th, label)
	return ui.column({ bg = th.surface2, radius = 6, padding = 10 }, { ui.text(label, { size = 13, tone = "dim" }) })
end

local function gallery(th, title)
	return function(s)
		local lines = {}
		for i = 1, 30 do
			lines[i] = ui.text("Scrollable line " .. i, { size = 13 })
		end
		local fruit = FRUITS[s.fruit or 0] or "none"
		local left = ui.column({ gap = 16, grow = 1 }, {
			ui.card({ title = "Buttons" }, {
				ui.row({ gap = 8 }, {
					ui.button("Primary", {
						id = "primary",
						tooltip = "Adds one to the counter",
						on_click = function(app)
							app:update(function(st)
								st.count = (st.count or 0) + 1
							end)
						end,
					}),
					ui.button("Secondary", { id = "reset", variant = "secondary", on_click = function(app)
						app:set("count", 0)
					end }),
					ui.button("Ghost", { variant = "ghost", on_click = function(app)
						app:toast("Ghost button pressed")
					end }),
					ui.button("Danger", { variant = "danger", on_click = function(app)
						app:set("dialog", true)
					end }),
					ui.button("Disabled", { disabled = true }),
				}),
				ui.row({ gap = 8 }, {
					ui.text("Clicked " .. (s.count or 0) .. " times", { id = "count", tone = "dim" }),
					ui.spacer(),
					ui.button("Small", { size = "small", variant = "secondary", on_click = function(app)
						app:toast("Saved", { tone = "success" })
					end }),
				}),
			}),
			ui.card({ title = "Toggles" }, {
				ui.checkbox("I agree to the terms", { id = "agree", checked = s.agree, on_change = setter("agree") }),
				ui.checkbox("Send me news", { checked = s.news, on_change = setter("news") }),
				ui.switch("Wi-Fi", { id = "wifi", checked = s.wifi, on_change = setter("wifi") }),
				ui.radio({ id = "size", options = { "Small", "Medium", "Large" }, selected = s.size, direction = "row", on_change = setter("size") }),
			}),
			ui.card({ title = "Slider, progress, badges" }, {
				ui.field("Volume", ui.slider({ id = "volume", value = s.volume, min = 0, max = 100, step = 1, show_value = true, on_change = setter("volume") })),
				ui.progress((s.volume or 0) / 100, { label = "Progress " .. (s.volume or 0) .. "%" }),
				ui.row({ gap = 8 }, {
					ui.badge("neutral", { tone = "neutral" }),
					ui.badge("accent"),
					ui.badge("success", { tone = "success" }),
					ui.badge("warning", { tone = "warning" }),
					ui.badge("danger", { tone = "danger", solid = true }),
				}),
			}),
			ui.card({ title = "Text" }, {
				ui.grid({ columns = 2, gap = 12 }, {
					ui.field("Name", ui.textfield({
						id = "name",
						value = s.name,
						placeholder = "Your name",
						on_change = setter("name"),
						on_submit = function(v, app)
							app:toast("Hello, " .. v)
						end,
					})),
					ui.field("Password", ui.textfield({
						id = "password",
						value = secrets[title],
						password = true,
						placeholder = "secret",
						on_change = function(v, app)
							secrets[title] = v
							app:update(function(st)
								st.pw_edits = (st.pw_edits or 0) + 1
							end)
						end,
					})),
				}),
				ui.field("Fruit", ui.select({ id = "fruit", options = FRUITS, selected = s.fruit, placeholder = "Pick one", on_change = setter("fruit") })),
				ui.field("Notes", ui.textarea({ id = "notes", value = s.notes, rows = 4, placeholder = "Write something…", on_change = setter("notes") })),
				ui.text(LOREM, { tone = "dim", size = 14 }),
			}),
		})
		local right = ui.column({ gap = 16, grow = 1 }, {
			ui.card({ title = "Tabs" }, {
				ui.tabs({
					id = "tabs",
					tabs = { "Overview", "Details", "語言" },
					selected = s.tab,
					on_change = setter("tab"),
					content = function(i)
						if i == 1 then
							return ui.text("Tabs switch content. Focus the bar and use the arrow keys.")
						elseif i == 2 then
							return ui.text("Name: " .. (if s.name == "" then "—" else s.name) .. "   Fruit: " .. fruit .. "   Volume: " .. (s.volume or 0))
						end
						return ui.text("繁體中文 · 简体中文 · English · Español · Français")
					end,
				}),
			}),
			ui.card({ title = "List: 10 000 rows, virtualised" }, {
				ui.list({
					id = "list",
					count = 10000,
					height = 220,
					selected = s.row,
					row = function(item, i, selected)
						return ui.row({ gap = 8 }, {
							ui.text("Row " .. i, { size = 14, tone = if selected then "accent" else nil }),
							ui.spacer(),
							ui.text(string.format("%05d", (i * 7919) % 100000), { tone = "dim", size = 13 }),
						})
					end,
					on_select = function(item, i, app)
						app:set("row", i)
					end,
				}),
				ui.text(if s.row then "Selected row " .. s.row else "Click a row, or focus the list and use the arrow keys", { tone = "dim", size = 13 }),
			}),
			ui.card({ title = "Table: 1 000 rows, sortable" }, {
				ui.table({
					id = "table",
					data = { columns = COLUMNS, rows = sorted_rows(s.sort) },
					height = 240,
					widths = { 70 },
					selected = s.trow,
					sort = s.sort,
					on_sort = function(c, desc, app)
						app:set("sort", { col = c, desc = desc })
					end,
					on_select = function(row, i, app)
						app:set("trow", i)
					end,
					format = function(v, c)
						if c == 4 then
							return string.format("%.1f", v)
						elseif c == 5 then
							return if v then "yes" else "no"
						end
						return nil
					end,
				}),
			}),
			ui.card({ title = "Layout and overlays" }, {
				ui.grid({ columns = 3, gap = 8 }, {
					tile(th, "grid 1"),
					tile(th, "grid 2"),
					tile(th, "grid 3"),
					tile(th, "grid 4"),
					tile(th, "grid 5"),
					tile(th, "grid 6"),
				}),
				ui.divider(),
				ui.row({ gap = 12, align = "start" }, {
					ui.stack({ width = 160, height = 110 }, {
						ui.image(nil, { alt = "image", width = 160, height = 110 }),
						ui.badge("stack", { align_self = "start" }),
					}),
					ui.scroll({ id = "scroll", height = 110, grow = 1, bg = th.surface2, radius = 6 }, ui.column({ gap = 4, padding = 8 }, lines)),
				}),
				ui.row({ gap = 8 }, {
					ui.button("Open dialog", { id = "open-dialog", variant = "secondary", on_click = function(app)
						app:set("dialog", true)
					end }),
					ui.button("Show toast", { id = "toast", variant = "secondary", on_click = function(app)
						app:toast("Toasts disappear by themselves", { tone = "accent" })
					end }),
					ui.button("Hover me", { id = "hover", variant = "ghost", tooltip = "Tooltips appear after half a second" }),
				}),
			}),
		})
		return ui.column({ padding = 24, gap = 16 }, {
			ui.row({ gap = 12 }, {
				ui.text("AnyPage UI", { size = 26 }),
				ui.badge(title, { tone = "accent" }),
				ui.spacer(),
				ui.text("Tab moves focus · Enter activates · the wheel scrolls", { tone = "dim", size = 13 }),
			}),
			ui.scroll({ id = "body", grow = 1 }, ui.row({ gap = 16, align = "start" }, { left, right })),
			ui.dialog({
				id = "dialog",
				open = s.dialog == true,
				title = "Delete everything?",
				body = "A modal dialog: Tab stays inside it, Escape or a click outside closes it.",
				actions = {
					ui.button("Cancel", { id = "cancel", variant = "secondary", on_click = function(app)
						app:set("dialog", false)
					end }),
					ui.button("Delete", { id = "delete", variant = "danger", on_click = function(app)
						app:set("dialog", false)
						app:toast("Deleted (not really)", { tone = "danger" })
					end }),
				},
				on_close = function(app)
					app:set("dialog", false)
				end,
			}),
		})
	end
end

local function initial()
	return {
		count = 0,
		name = "",
		pw_edits = 0,
		notes = "Multi-line text wraps and scrolls.\nArrow keys move by line; Ctrl+Z undoes.",
		agree = true,
		news = false,
		wifi = true,
		size = 2,
		volume = 40,
		fruit = 3,
		tab = 1,
		dialog = false,
	}
end

Dark = ui.app({
	name = "dark",
	rect = { 0, 0, W, H },
	theme = ui.themes.dark,
	state = initial(),
	agent = true,
	view = gallery(ui.themes.dark, "dark"),
})

Light = ui.app({
	name = "light",
	rect = { 2400 - W, 0, W, H },
	theme = ui.themes.light,
	state = initial(),
	agent = true,
	view = gallery(ui.themes.light, "light"),
})
