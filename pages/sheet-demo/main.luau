-- AnyPage spreadsheet demo: a workbook computed by the native `sheet` module
-- (IronCalc, in Rust), shown as a grid drawn with plain page.panel calls.
--
-- The workbook lives in one input cell. Every edit is a native call that
-- changes the workbook in place, recalculates it and returns it at a new
-- version; putting that back into the cell redraws exactly the panels that
-- read it.
--
-- Keys: arrows / Tab move · Enter or F2 edits, Enter again commits and moves
-- down · typing starts editing (the first letter arrives lower-case: keys are
-- named, not typed, until a cell is being edited) · Esc cancels · Delete
-- clears · Ctrl+Z / Ctrl+Y undo / redo · Ctrl+C / Ctrl+V copy / paste
-- (tab-separated, so ranges pasted from other spreadsheets land in cells) ·
-- Ctrl+S saves xlsx · Ctrl+E saves CSV · Ctrl+O opens xlsx or CSV (or drop a
-- file) · wheel scrolls.
--
-- Files open in the background: the bytes go into an input cell, the
-- `sheet.load` job reads them off the page's thread and replaces the
-- workbook's content in place (`into`), so the handlers below, which can
-- only read input cells, edit the opened file. Panels take the job as an
-- input to redraw when it finishes and to show its progress or error.

local rgba, alpha = color.rgba, color.alpha

-- ---------------------------------------------------------------- layout (world units)

local W = 1280
local TITLE = { 0, 0, W, 64 }
local BAR = { 0, 72, W, 44 }
local GRID = { 0, 124, W, 560 }
local TOTAL = { 0, 692, W, 136 }

local HEAD_W, ROW_H = 52, 28
local COL_W = { 230, 120, 90, 120, 130, 120, 130, 130, 130 }
local NCOLS = #COL_W
local NROWS = GRID[4] // ROW_H - 1
local SIZE = 15

-- title bar buttons, panel-local
local BUTTONS = {
	{ id = "undo", label = "復原 Undo", rect = { 760, 14, 104, 36 } },
	{ id = "redo", label = "重做 Redo", rect = { 872, 14, 104, 36 } },
	{ id = "save", label = "存 xlsx", rect = { 984, 14, 90, 36 } },
	{ id = "csv", label = "存 CSV", rect = { 1082, 14, 84, 36 } },
	{ id = "open", label = "開啟", rect = { 1174, 14, 90, 36 } },
}

local BG = rgba(22, 25, 32)
local HEAD_BG = rgba(32, 36, 46)
local LINE = rgba(255, 255, 255, 22)
local TEXT_HI = rgba(232, 236, 244)
local TEXT_LO = rgba(140, 148, 164)
local ACCENT = rgba(88, 166, 255)
local SEL_FILL = rgba(88, 166, 255, 40)
local EDIT_BG = rgba(12, 14, 20)
local ERR = rgba(255, 120, 120)
local GOOD = rgba(120, 220, 160)

local ERRORS = {
	["#DIV/0!"] = true, ["#N/A"] = true, ["#VALUE!"] = true, ["#REF!"] = true, ["#NAME?"] = true,
	["#NUM!"] = true, ["#NULL!"] = true, ["#ERROR!"] = true, ["#N/IMPL"] = true, ["#SPILL!"] = true,
	["#CALC!"] = true, ["#CIRC!"] = true,
}

local function inside(r, x, y)
	return x >= r[1] and x < r[1] + r[3] and y >= r[2] and y < r[2] + r[4]
end

-- ---------------------------------------------------------------- the workbook

local SHEET = "Invoice"

local function sample()
	local wb = sheet.new({ name = "invoice" })
	wb = sheet.rename_sheet(wb, 1, SHEET)
	local items = {
		{ "Steel bolts M8", "2024-01-15", 120, "$0.35" },
		{ "Hex nuts M8", "2024-01-15", 120, "$0.12" },
		{ "Washers 8 mm", "2024-01-22", 240, "$0.05" },
		{ "Aluminium profile 2 m", "2024-02-03", 16, "$14.90" },
		{ "Corner brackets", "2024-02-03", 32, "$1.75" },
		{ "Cable ties (100)", "2024-02-10", 5, "$3.20" },
		{ "LED strip 5 m", "2024-02-18", 4, "$22.50" },
		{ "Power supply 24 V", "2024-02-18", 2, "$38.00" },
	}
	local rows = { { "Item", "Date", "Qty", "Unit price", "Amount", "Size" } }
	for i, it in items do
		local r = i + 1
		rows[r] = { it[1], it[2], it[3], it[4], `=C{r}*D{r}`, `=IF(E{r}>=40,"large","small")` }
	end
	rows[10] = { "" }
	rows[11] = { "Subtotal", "", "=SUM(C2:C9)", "", "=SUM(E2:E9)" }
	rows[12] = { "Tax", "", "", "5%", "=ROUND(E11*D12,2)" }
	rows[13] = { "Grand total", "", "", "", "=E11+E12" }
	rows[14] = { "Average unit price", "", "", "", "=AVERAGE(D2:D9)" }
	rows[15] = { "Largest line", "=XLOOKUP(E15,E2:E9,A2:A9)", "", "", "=MAX(E2:E9)" }
	rows[16] = { "Last delivery", '=TEXT(MAX(B2:B9),"yyyy-mm-dd")' }
	rows[17] = { "LED strip line", "", "", "", '=VLOOKUP("LED strip 5 m",A2:E9,5,FALSE)' }
	wb = sheet.set_range(wb, "A1", rows)
	wb = sheet.set_style(wb, "A1:F1", { fill = "#26324A", bold = true })
	wb = sheet.set_style(wb, "A13:E13", { fill = "#1E3B2C", bold = true })
	wb = sheet.set_style(wb, "E12", { format = "$#,##0.00" })
	wb = sheet.set_style(wb, "E15:E17", { format = "$#,##0.00" })
	-- the sample is where history starts: Ctrl+Z should not take it apart
	return sheet.clear_history(wb)
end

-- "#RRGGBB" -> packed colour for drawing
local function hex(c)
	return tonumber(c:sub(2), 16) * 256 + 255
end

-- ---------------------------------------------------------------- state

local book = page.input("book", sample())
local sel = page.input("sel", { row = 2, col = 3 }, { agent = true })
local view = page.input("view", { top = 1, left = 1 })
local edit = page.input("edit", { on = false })
local status = page.input("status", "")
-- a file to open: its bytes (nil until one is dropped or picked) and name
local file_bytes = page.input("file_bytes", nil)
local file_name = page.input("file_name", "invoice.xlsx")
-- reads the file on the worker pool and replaces the workbook's content
local loaded = page.job("loaded", "sheet.load", { file_bytes }, { into = page.get(book) })

local TOP_MAX = 1048576 - NROWS + 1

-- what the last file load is doing, for the title bar and for agents
local file_state = page.derive("file_state", { loaded, file_name }, function(ld, name)
	if type(ld) == "table" and ld.state == "pending" then
		return `正在開啟 opening {name} {math.floor((ld.progress or 0) * 100)}%`
	elseif type(ld) == "table" and ld.state == "error" then
		return `無法開啟 cannot open {name}: {ld.message}`
	elseif ld ~= nil then
		return "已開啟 opened " .. name
	end
	return ""
end)

-- a summary of an opened file, computed once per load (describe() walks
-- every cell, so it stays out of the panels that redraw on every edit)
local summary = page.derive("summary", { loaded }, function(ld)
	if type(ld) ~= "userdata" then
		return nil
	end
	local ok, info = pcall(function()
		return ld:describe()
	end)
	return if ok then info else nil
end)

local function selected_ref()
	local s = page.get(sel)
	return sheet.ref(s.row, s.col)
end

-- run an edit; keep the new workbook, or show why it failed
local function apply(f, ...)
	local ok, res = pcall(f, page.get(book), ...)
	if ok then
		page.set(book, res)
		page.set(status, "")
	else
		page.set(status, tostring(res))
	end
	return ok
end

local function show(row, col)
	local v = page.get(view)
	local top, left = v.top, v.left
	if row < top then
		top = row
	elseif row > top + NROWS - 1 then
		top = row - NROWS + 1
	end
	if col < left then
		left = col
	elseif col > left + NCOLS - 1 then
		left = col - NCOLS + 1
	end
	page.set(view, { top = top, left = left })
end

local function select_cell(row, col)
	row = math.clamp(row, 1, 1048576)
	col = math.clamp(col, 1, 16384)
	page.set(sel, { row = row, col = col })
	show(row, col)
end

local function move(dr, dc)
	local s = page.get(sel)
	select_cell(s.row + dr, s.col + dc)
end

-- where the caret is, in world units, so hosts place the keyboard / IME there
local function cell_x(col)
	local v = page.get(view)
	local x = GRID[1] + HEAD_W
	for j = 1, col - v.left do
		x += COL_W[j]
	end
	return x
end

local function focus(e)
	if not e.on then
		page.focus_text(nil)
		return
	end
	local s, v = page.get(sel), page.get(view)
	local x = cell_x(s.col) + 8 + text.caret(e.text, SIZE, e.caret)
	local y = GRID[2] + (s.row - v.top + 1) * ROW_H
	page.focus_text({ x, y + 3, 2, ROW_H - 6 })
end

local function set_edit(e)
	page.set(edit, e)
	focus(e)
end

local function start_edit(initial)
	set_edit({ on = true, text = initial, caret = #initial, pre = "" })
end

local function stop_edit()
	set_edit({ on = false })
end

local function commit(dr, dc)
	local e = page.get(edit)
	if apply(sheet.set, selected_ref(), e.text) then
		stop_edit()
		move(dr, dc)
	end
end

local function insert(t)
	local e = page.get(edit)
	local s = e.text
	set_edit({ on = true, text = s:sub(1, e.caret) .. t .. s:sub(e.caret + 1), caret = e.caret + #t, pre = "" })
end

-- ---------------------------------------------------------------- files

-- hand the file to the sheet.load job; the page stays responsive meanwhile
local function load_file(f)
	stop_edit()
	page.set(view, { top = 1, left = 1 })
	select_cell(1, 1)
	page.set(status, "")
	page.set(file_name, f.name or "file")
	page.set(file_bytes, f.data)
end

local function saved(r)
	page.set(status, if r.saved then "已儲存 saved " .. r.saved elseif r.error then r.error else "")
end

local function save_xlsx()
	local ok, bytes = pcall(function()
		return page.get(book):save()
	end)
	if ok then
		page.save_file({ name = "invoice.xlsx", data = bytes, done = saved })
	else
		page.set(status, tostring(bytes))
	end
end

local function save_csv()
	local ok, csv = pcall(function()
		return page.get(book):to_csv()
	end)
	if ok then
		page.save_file({ name = "invoice.csv", data = csv, done = saved })
	else
		page.set(status, tostring(csv))
	end
end

local function open()
	page.open_file({
		filters = { "xlsx", "csv" },
		done = function(f)
			if f.data then
				load_file(f)
			end
		end,
	})
end

-- ---------------------------------------------------------------- input

local ACTIONS = {
	undo = function()
		apply(sheet.undo)
	end,
	redo = function()
		apply(sheet.redo)
	end,
	save = save_xlsx,
	csv = save_csv,
	open = open,
}

page.on("key", function(k)
	local e = page.get(edit)
	if e.on then
		local s, c = e.text, e.caret
		if k == "enter" or k == "down" then
			commit(1, 0)
		elseif k == "up" then
			commit(-1, 0)
		elseif k == "tab" then
			commit(0, 1)
		elseif k == "shift+tab" then
			commit(0, -1)
		elseif k == "escape" then
			stop_edit()
		elseif k == "backspace" then
			local p = text.prev(s, c)
			set_edit({ on = true, text = s:sub(1, p) .. s:sub(c + 1), caret = p, pre = "" })
		elseif k == "delete" then
			local n = text.next(s, c)
			set_edit({ on = true, text = s:sub(1, c) .. s:sub(n + 1), caret = c, pre = "" })
		elseif k == "left" then
			set_edit({ on = true, text = s, caret = text.prev(s, c), pre = "" })
		elseif k == "right" then
			set_edit({ on = true, text = s, caret = text.next(s, c), pre = "" })
		elseif k == "home" then
			set_edit({ on = true, text = s, caret = 0, pre = "" })
		elseif k == "end" then
			set_edit({ on = true, text = s, caret = #s, pre = "" })
		elseif k == "ctrl+v" then
			page.paste()
		else
			return false
		end
		return true
	end
	if k == "left" or k == "shift+tab" then
		move(0, -1)
	elseif k == "right" or k == "tab" then
		move(0, 1)
	elseif k == "up" then
		move(-1, 0)
	elseif k == "down" then
		move(1, 0)
	elseif k == "pagedown" then
		move(NROWS, 0)
	elseif k == "pageup" then
		move(-NROWS, 0)
	elseif k == "ctrl+home" then
		select_cell(1, 1)
	elseif k == "enter" or k == "f2" then
		start_edit(page.get(book):input(selected_ref()))
	elseif k == "delete" or k == "backspace" then
		apply(sheet.set, selected_ref(), nil)
	elseif k == "ctrl+z" then
		ACTIONS.undo()
	elseif k == "ctrl+y" or k == "ctrl+shift+z" then
		ACTIONS.redo()
	elseif k == "ctrl+s" then
		save_xlsx()
	elseif k == "ctrl+e" then
		save_csv()
	elseif k == "ctrl+o" then
		open()
	elseif k == "ctrl+c" then
		page.copy(page.get(book):text(selected_ref()))
	elseif k == "ctrl+v" then
		page.paste()
	elseif k == "space" then
		start_edit(" ")
	elseif utf8.len(k) == 1 then
		start_edit(k)
	else
		return false
	end
	return true
end)

page.on("text", function(t)
	if page.get(edit).on then
		insert(t)
	else
		start_edit(t)
	end
	return true
end)

page.on("ime", function(t)
	local e = page.get(edit)
	if e.on then
		set_edit({ on = true, text = e.text, caret = e.caret, pre = t })
	end
	return true
end)

page.on("paste", function(t)
	if page.get(edit).on then
		insert((t:gsub("[\r\n\t]+", " ")))
		return true
	end
	-- tab-separated rows, as spreadsheets put ranges on the clipboard
	local rows = {}
	for line in (t .. "\n"):gmatch("(.-)\r?\n") do
		table.insert(rows, string.split(line, "\t"))
	end
	while #rows > 0 and #rows[#rows] == 1 and rows[#rows][1] == "" do
		table.remove(rows)
	end
	if #rows > 0 then
		apply(sheet.set_range, selected_ref(), rows)
	end
	return true
end)

page.on("pointer_down", function(x, y)
	if inside(GRID, x, y) then
		local gx, gy = x - GRID[1], y - GRID[2]
		local row_i = gy // ROW_H
		if row_i < 1 or gx < HEAD_W then
			return "handled"
		end
		local v = page.get(view)
		local cx, col = HEAD_W, nil
		for j = 1, NCOLS do
			if gx >= cx and gx < cx + COL_W[j] then
				col = v.left + j - 1
				break
			end
			cx += COL_W[j]
		end
		if not col then
			return "handled"
		end
		if page.get(edit).on then
			local e = page.get(edit)
			if not apply(sheet.set, selected_ref(), e.text) then
				return "handled"
			end
			stop_edit()
		end
		select_cell(v.top + row_i - 1, col)
		return "handled"
	end
	if inside(TITLE, x, y) then
		for _, b in BUTTONS do
			local r = b.rect
			if inside({ TITLE[1] + r[1], TITLE[2] + r[2], r[3], r[4] }, x, y) then
				ACTIONS[b.id]()
				return "handled"
			end
		end
	end
	return false
end)

page.on("wheel", function(x, y, dx, dy)
	if not inside(GRID, x, y) then
		return false
	end
	local v = page.get(view)
	page.set(view, { top = math.clamp(v.top - math.round(dy * 3), 1, TOP_MAX), left = v.left })
	return true
end)

page.on("file_drop", function(f)
	load_file(f)
	return true
end)

-- ---------------------------------------------------------------- for agents

page.action({
	name = "set_cell",
	description = "Type a value or =formula into a cell, as a person would (A1 reference, first sheet unless Sheet!A1)",
	params = { ref = "string", value = "string | number | boolean | nil" },
	run = function(p)
		local ok = apply(sheet.set, p.ref, p.value)
		return { ok = ok, value = if ok then page.get(book):get(p.ref) else nil, error = if ok then nil else page.get(status) }
	end,
})

page.action({
	name = "read",
	description = "Read a range as a table { columns, rows } (default: the whole first sheet, header row as column names)",
	params = { ref = "string?" },
	run = function(p)
		local wb = page.get(book)
		return if p and p.ref then wb:range(p.ref, { header = true }) else wb:table()
	end,
})

page.action({
	name = "formulas",
	description = "List every formula on the first sheet with its current value",
	params = {},
	run = function()
		return page.get(book):cells(nil, { formulas = true })
	end,
})

page.action({
	name = "undo",
	description = "Undo the last edit",
	params = {},
	run = function()
		return { ok = apply(sheet.undo) }
	end,
})

-- the grand total, for agents and tests (nil when another workbook is open)
page.derive("total", { book, loaded }, function(wb)
	local ok, v = pcall(function()
		return wb:get(SHEET .. "!E13")
	end)
	return if ok then v else nil
end)

-- ---------------------------------------------------------------- panels

local function button(d, b, enabled)
	local r = b.rect
	d:rrect(r[1], r[2], r[3], r[4], 8, if enabled then alpha(ACCENT, 46) else alpha(TEXT_LO, 20))
	d:stroke(r[1], r[2], r[3], r[4], 8, 1, alpha(if enabled then ACCENT else TEXT_LO, 110))
	local w = page.measure(b.label, 14)
	d:text(r[1] + (r[3] - w) / 2, r[2] + 8, 14, if enabled then TEXT_HI else TEXT_LO, b.label)
end

page.panel({
	name = "title",
	rect = TITLE,
	class = "static",
	inputs = { book, loaded, file_name, file_state },
	draw = function(d, wb, _, name, fs)
		d:rrect(0, 0, d.w, d.h, 12, BG)
		d:clip(0, 0, 740, d.h)
		d:text(18, 8, 22, TEXT_HI, "試算表 Spreadsheet · " .. name)
		if fs ~= "" and not string.find(fs, "^已開啟") then
			-- loading progress or why the file could not be opened
			d:text(18, 38, 13, if string.find(fs, "^無法") then ERR else ACCENT, fs)
		else
			d:text(18, 38, 13, TEXT_LO, "IronCalc 在 Rust 中計算；Luau 只畫格子。Enter 編輯，Ctrl+Z 復原，Ctrl+S 存檔，拖放 xlsx/CSV 開啟")
		end
		d:unclip()
		for _, b in BUTTONS do
			local enabled = if b.id == "undo" then wb:can_undo() elseif b.id == "redo" then wb:can_redo() else true
			button(d, b, enabled)
		end
	end,
})

page.panel({
	name = "formula_bar",
	rect = BAR,
	class = "ops",
	inputs = { book, sel, edit, status, loaded },
	draw = function(d, wb, s, e, st)
		d:rrect(0, 0, d.w, d.h, 10, BG)
		local ref = sheet.ref(s.row, s.col)
		d:rrect(8, 6, 84, d.h - 12, 6, HEAD_BG)
		d:text(8 + (84 - page.measure(ref, 15)) / 2, 12, 15, TEXT_HI, ref)
		d:text(104, 12, 15, TEXT_LO, "fx")
		local x0 = 132
		if e.on then
			d:text(x0, 12, 15, TEXT_HI, e.text)
			local cx = x0 + text.caret(e.text, 15, e.caret)
			if e.pre ~= "" then
				local pw = page.measure(e.pre, 15)
				d:text(cx, 12, 15, ACCENT, e.pre)
				d:rect(cx, 32, pw, 1, ACCENT)
			end
			d:rect(cx, 10, 2, 24, ACCENT)
		else
			local ok, content = pcall(function()
				return wb:input(ref)
			end)
			d:text(x0, 12, 15, TEXT_HI, if ok then content else "")
		end
		if st ~= "" then
			local w = math.min(page.measure(st, 13), 520)
			d:clip(d.w - w - 16, 0, w + 8, d.h)
			d:text(d.w - w - 12, 14, 13, ERR, st)
			d:unclip()
		end
	end,
})

page.panel({
	name = "grid",
	rect = GRID,
	class = "ops",
	inputs = { book, sel, view, edit, loaded },
	draw = function(d, wb, s, v, e)
		d:rect(0, 0, d.w, d.h, BG)
		local r2, c2 = v.top + NROWS - 1, v.left + NCOLS - 1
		local t = wb:range(sheet.ref(v.top, v.left) .. ":" .. sheet.ref(r2, c2), { text = true })
		local right = HEAD_W
		for j = 1, NCOLS do
			right += COL_W[j]
		end
		-- header row and row numbers
		d:rect(0, 0, right, ROW_H, HEAD_BG)
		d:rect(0, 0, HEAD_W, (NROWS + 1) * ROW_H, HEAD_BG)
		local x = HEAD_W
		for j = 1, NCOLS do
			local col = v.left + j - 1
			if col == s.col then
				d:rect(x, 0, COL_W[j], ROW_H, alpha(ACCENT, 60))
			end
			local name = sheet.col_name(col)
			d:text(x + (COL_W[j] - page.measure(name, 13)) / 2, 6, 13, TEXT_LO, name)
			x += COL_W[j]
		end
		for i = 1, NROWS do
			local row = v.top + i - 1
			local y = i * ROW_H
			if row == s.row then
				d:rect(0, y, HEAD_W, ROW_H, alpha(ACCENT, 60))
			end
			local label = tostring(row)
			d:text(HEAD_W - 8 - page.measure(label, 13), y + 6, 13, TEXT_LO, label)
			local texts, values = t.text[i], t.rows[i]
			x = HEAD_W
			for j = 1, NCOLS do
				local fill = wb:style(sheet.ref(row, v.left + j - 1)).fill
				if fill then
					d:rect(x, y, COL_W[j], ROW_H, hex(fill))
				end
				local shown = texts and texts[j]
				if shown and shown ~= "" then
					local value = values[j]
					local colour = if ERRORS[shown] and type(value) == "string" then ERR else TEXT_HI
					local tx = x + 8
					if type(value) == "number" then
						tx = x + COL_W[j] - 8 - page.measure(shown, SIZE)
					end
					d:clip(x + 1, y, COL_W[j] - 2, ROW_H)
					d:text(tx, y + 5, SIZE, colour, shown)
					d:unclip()
				end
				x += COL_W[j]
			end
		end
		-- grid lines
		x = HEAD_W
		for j = 0, NCOLS do
			d:rect(x, 0, 1, (NROWS + 1) * ROW_H, LINE)
			x += COL_W[j + 1] or 0
		end
		for i = 1, NROWS + 1 do
			d:rect(0, i * ROW_H, right, 1, LINE)
		end
		-- the selected cell, and the editor over it
		local si, sj = s.row - v.top + 1, s.col - v.left + 1
		if si >= 1 and si <= NROWS and sj >= 1 and sj <= NCOLS then
			local sx = HEAD_W
			for j = 1, sj - 1 do
				sx += COL_W[j]
			end
			local sy = si * ROW_H
			if e.on then
				d:rect(sx, sy, COL_W[sj], ROW_H, EDIT_BG)
				d:clip(sx, sy, COL_W[sj], ROW_H)
				d:text(sx + 8, sy + 5, SIZE, TEXT_HI, e.text)
				local cx = sx + 8 + text.caret(e.text, SIZE, e.caret)
				if e.pre ~= "" then
					d:text(cx, sy + 5, SIZE, ACCENT, e.pre)
				end
				d:rect(cx, sy + 4, 2, ROW_H - 8, ACCENT)
				d:unclip()
			else
				d:rect(sx, sy, COL_W[sj], ROW_H, SEL_FILL)
			end
			d:stroke(sx, sy, COL_W[sj], ROW_H, 2, 2, ACCENT)
		end
	end,
})

local function stat(d, x, label, value, colour)
	d:text(x, 84, 13, TEXT_LO, label)
	d:text(x, 102, 17, colour or TEXT_HI, value)
end

page.panel({
	name = "totals",
	rect = TOTAL,
	class = "static",
	inputs = { book, loaded, file_name, summary },
	draw = function(d, wb, _, name, info)
		d:rrect(0, 0, d.w, d.h, 12, BG)
		local ok, grand = pcall(function()
			return wb:text(SHEET .. "!E13")
		end)
		if not ok then
			-- another workbook is open: what it held when it was opened
			-- (computed once per load, not on every edit)
			d:text(24, 16, 15, TEXT_LO, "活頁簿 Workbook")
			d:text(24, 40, 30, TEXT_HI, name)
			local x = 24
			if type(info) == "table" and info.sheets then
				for _, sh in info.sheets do
					stat(d, x, sh.name, `{sh.cells} cells, {sh.formulas} formulas`)
					x += 260
				end
			else
				for _, sh in wb:sheets() do
					stat(d, x, sh, "")
					x += 260
				end
			end
			return
		end
		d:text(24, 14, 15, TEXT_LO, "總計 Grand total (Invoice!E13 = E11 + E12)")
		d:text(24, 36, 34, GOOD, grand)
		stat(d, 24, "小計 Subtotal", wb:text(SHEET .. "!E11"))
		stat(d, 224, "稅 Tax", wb:text(SHEET .. "!E12"))
		stat(d, 384, "數量 Units", wb:text(SHEET .. "!C11"))
		stat(d, 544, "最大項 Largest line", wb:text(SHEET .. "!B15") .. "  " .. wb:text(SHEET .. "!E15"))
		stat(d, 884, "最後交貨 Last delivery", wb:text(SHEET .. "!B16"))
		stat(d, 1084, "平均單價 Avg price", wb:text(SHEET .. "!E14"))
		local undo = if wb:can_undo() then "可復原 undo available" else "無可復原 nothing to undo"
		d:text(d.w - 24 - page.measure(undo, 13), 16, 13, TEXT_LO, undo)
	end,
})
