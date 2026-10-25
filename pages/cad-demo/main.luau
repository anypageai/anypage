-- AnyPage CAD demo: a drawing shown as native vector tiles (crisp at every
-- zoom), its layers, picking, measuring, editing, opening and saving DXF.
--
-- The drawing is a resource held in an input cell. Edits (cad.add,
-- cad.move, ...) change it and return its next version, so everything that
-- reads it (tiles, panels) recomputes.
--
-- Files are opened off the page's thread: the handler makes an empty
-- drawing (cad.new{pending = true}) and hands it with the bytes to the job
-- cad.load, which fills it. The page shows it at once (progress in the
-- status line) and keeps editing that same drawing afterwards.

local rgba, alpha = color.rgba, color.alpha

local function R(x, y, w, h)
	return { x, y, w, h }
end

local function contains(r, x, y)
	return x >= r[1] and x <= r[1] + r[3] and y >= r[2] and y <= r[2] + r[4]
end

-- ---------------------------------------------------------------- layout

local TITLE = R(20, 16, 1100, 52)
local DRAW = R(20, 80, 1100, 900)
local SIDE = R(1140, 16, 440, 964)

-- side panel, local coordinates
local LAYER_TOP, ROW_H, MAX_ROWS = 64, 30, 12
local BUTTONS = {
	{ id = "fit", label = "適合 Fit", r = R(20, 560, 196, 40) },
	{ id = "move", label = "移動 Move →", r = R(224, 560, 196, 40) },
	{ id = "copy", label = "複製 Copy", r = R(20, 608, 196, 40) },
	{ id = "delete", label = "刪除 Delete", r = R(224, 608, 196, 40) },
	{ id = "open", label = "開啟 Open DXF", r = R(20, 656, 196, 40) },
	{ id = "save", label = "儲存 Save DXF", r = R(224, 656, 196, 40) },
}

local BG = 0x1B1E24FF
local PANEL_BG = rgba(22, 25, 32)
local TEXT_HI = rgba(232, 236, 244)
local TEXT_LO = rgba(140, 148, 164)
local ACCENT = rgba(255, 170, 40)

-- ---------------------------------------------------------------- state

local build_sample = import("sample")
local sample = build_sample()

local function fit_view(dwg)
	return dwg:bounds({ margin = 0.04 }) or { 0, 0, 100, 100 }
end

Dwg = page.input("drawing", sample)
-- the view rect in drawing units; nil fits the drawing (after opening a file,
-- whose bounds are known only when it has loaded)
View = page.input("view", fit_view(sample), { agent = true })
Layers = page.input("layers", {}, { agent = true }) -- name -> visible (overrides)
Sel = page.input("selected", {}, { agent = true })
Status = page.input("status", "點選圖元選取，滾輪縮放，拖放 DXF 開啟。")
-- { bytes, into, name } of the file being opened
Opening = page.input("opening", nil)
Loaded = page.job("loaded", "cad.load", { Opening })

-- "pending" / "error" while a file loads or failed, else "ready"
local function load_state(loaded)
	if typeof(loaded) == "table" and loaded.state then
		return loaded.state
	end
	return "ready"
end

local Params = page.derive("cad.params", { Dwg, View, Layers, Sel, Loaded }, function(dwg, view, layers, sel, loaded)
	return {
		drawing = dwg,
		rect = view or fit_view(dwg),
		layers = layers,
		selected = sel,
		background = BG,
		-- a new value when the load finishes, so tiles redraw the filled drawing
		loaded = load_state(loaded),
	}
end)

page.tiled({ rect = DRAW, source = "cad.drawing", params = Params })

-- ---------------------------------------------------------------- panels

local function layer_rows(dwg, overrides)
	local rows = {}
	for _, l in dwg:layers() do
		local on = overrides[l.name]
		if on == nil then
			on = l.on
		end
		table.insert(rows, { name = l.name, color = l.color, count = l.entities, on = on })
	end
	return rows
end

page.panel({
	rect = TITLE,
	name = "title",
	inputs = { Dwg, Status, Loaded, Opening },
	draw = function(d, dwg, status, loaded, opening)
		local info = dwg:info()
		d:rrect(0, 0, d.w, d.h, 12, PANEL_BG)
		d:text(16, 8, 20, TEXT_HI, "CAD 示範")
		d:text(130, 12, 14, TEXT_LO, string.format("%s · %d 圖元 · %d 圖層 · %d 圖塊", info.version, info.entities, info.layers, info.blocks))
		local state = load_state(loaded)
		local name = if opening then opening.name else ""
		if state == "pending" then
			status = string.format("開啟中 %s … %d%%", name, math.floor((loaded.progress or 0) * 100))
		elseif state == "error" then
			status = "無法開啟 " .. name .. ": " .. tostring(loaded.message)
		end
		d:text(560, 12, 14, ACCENT, status)
	end,
})

page.panel({
	rect = SIDE,
	name = "side",
	inputs = { Dwg, Layers, Sel, Loaded },
	draw = function(d, dwg, overrides, sel)
		d:rrect(0, 0, d.w, d.h, 12, PANEL_BG)
		d:text(20, 18, 18, TEXT_HI, "圖層 Layers")
		local rows = layer_rows(dwg, overrides)
		for i, row in rows do
			if i > MAX_ROWS then
				break
			end
			local y = LAYER_TOP + (i - 1) * ROW_H
			d:rrect(20, y + 4, 18, 18, 4, row.color)
			d:text(48, y + 4, 15, if row.on then TEXT_HI else TEXT_LO, row.name)
			d:text(300, y + 4, 14, TEXT_LO, tostring(row.count))
			d:rrect(360, y + 4, 60, 20, 10, if row.on then alpha(ACCENT, 200) else rgba(60, 64, 76))
			d:text(374, y + 6, 13, TEXT_HI, if row.on then "開" else "關")
		end

		local top = LAYER_TOP + MAX_ROWS * ROW_H + 20
		d:text(20, top, 18, TEXT_HI, "選取 Selection")
		if #sel == 0 then
			d:text(20, top + 32, 14, TEXT_LO, "沒有選取。點選圖面上的圖元。")
		else
			local e = dwg:entity(sel[1])
			local m = dwg:measure(sel)
			if e then
				d:text(20, top + 32, 15, TEXT_HI, string.format("%s #%d  圖層 %s", e.type, e.id, e.layer))
			end
			d:text(20, top + 56, 15, TEXT_HI, string.format("長度 %.1f   面積 %.1f", m.length, m.area))
			if e and e.text then
				d:text(20, top + 80, 15, TEXT_LO, e.text)
			end
		end

		for _, b in BUTTONS do
			local r = b.r
			d:rrect(r[1], r[2], r[3], r[4], 8, rgba(44, 50, 64))
			d:text(r[1] + 14, r[2] + 10, 15, TEXT_HI, b.label)
		end
		d:textbox(20, 716, 400, 220, 13, TEXT_LO,
			"tiles: cad.drawing — 原生向量繪製，任何縮放都銳利。\n" ..
			"編輯: cad.add / move / copy / delete / set_layer，存檔 dwg:save()。\n" ..
			"agent: 動作 select、move、delete、layer、add、measure、entities。")
	end,
})

-- ---------------------------------------------------------------- actions

local function selected()
	return page.get(Sel)
end

local function set_drawing(dwg, msg)
	page.set(Dwg, dwg)
	if msg then
		page.set(Status, msg)
	end
end

local function fit()
	page.set(View, fit_view(page.get(Dwg)))
end

-- the view in drawing units (nil in the cell means "fit the drawing")
local function current_view()
	return page.get(View) or fit_view(page.get(Dwg))
end

-- edits fail while an opened file is still loading: say so instead
local function edit(f, ...)
	local ok, dwg = pcall(f, page.get(Dwg), ...)
	if not ok then
		page.set(Status, tostring(dwg))
		return nil
	end
	return dwg
end

local function move_selection(dx, dy)
	local ids = selected()
	if #ids == 0 then
		return false
	end
	local dwg = edit(cad.move, ids, dx, dy)
	if dwg then
		set_drawing(dwg, string.format("移動 %d 個圖元", #ids))
	end
	return true
end

local function delete_selection()
	local ids = selected()
	if #ids == 0 then
		return false
	end
	local dwg = edit(cad.delete, ids)
	if dwg then
		set_drawing(dwg, string.format("刪除 %d 個圖元", #ids))
		page.set(Sel, {})
	end
	return true
end

local function copy_selection()
	local ids = selected()
	if #ids == 0 then
		return false
	end
	local view = current_view()
	local dwg = edit(cad.copy, ids, view[3] * 0.05, -view[4] * 0.05)
	if dwg then
		set_drawing(dwg, string.format("複製 %d 個圖元", #ids))
		page.set(Sel, dwg:last_ids())
	end
	return true
end

-- the job cad.load parses the file and fills `target`; until then the
-- drawing is empty, and the title shows the progress
local function open_bytes(name, data)
	local target = cad.new({ pending = true })
	page.set(Opening, { bytes = data, into = target, name = name })
	set_drawing(target, "已開啟 " .. name)
	page.set(Sel, {})
	page.set(Layers, {})
	page.set(View, nil)
	return true
end

local function save()
	local bytes = page.get(Dwg):save()
	page.save_file({ name = "drawing.dxf", data = bytes, done = function(r)
		if r.saved then
			page.set(Status, "已儲存 " .. r.saved)
		end
	end })
	return buffer.len(bytes)
end

local function toggle_layer(name)
	local overrides = table.clone(page.get(Layers))
	local current = overrides[name]
	if current == nil then
		for _, l in page.get(Dwg):layers() do
			if l.name == name then
				current = l.on
			end
		end
	end
	overrides[name] = not current
	page.set(Layers, overrides)
end

local function run_button(id)
	if id == "fit" then
		fit()
	elseif id == "move" then
		local view = current_view()
		move_selection(view[3] * 0.02, 0)
	elseif id == "copy" then
		copy_selection()
	elseif id == "delete" then
		delete_selection()
	elseif id == "open" then
		page.open_file({ filters = { "dxf" }, done = function(f)
			if f.data then
				open_bytes(f.name, f.data)
			end
		end })
	elseif id == "save" then
		save()
	end
end

-- ---------------------------------------------------------------- input

-- drawing units per page unit in the drawing area
local function drawing_scale(view)
	return math.max(view[3] / DRAW[3], view[4] / DRAW[4])
end

page.on("pointer_down", function(wx, wy)
	if contains(DRAW, wx, wy) then
		local view = current_view()
		local dwg = page.get(Dwg)
		local q = cad.to_drawing(DRAW, view, wx, wy)
		local id = dwg:pick(q.x, q.y, 6 * drawing_scale(view), { layers = page.get(Layers) })
		if id then
			page.set(Sel, { id })
			local e = dwg:entity(id)
			page.set(Status, string.format("選取 %s #%d", e and e.type or "?", id))
			return "handled"
		end
		page.set(Sel, {})
		return nil -- empty space: the engine pans
	end
	if contains(SIDE, wx, wy) then
		local x, y = wx - SIDE[1], wy - SIDE[2]
		for _, b in BUTTONS do
			if contains(b.r, x, y) then
				run_button(b.id)
				return "handled"
			end
		end
		local row = math.floor((y - LAYER_TOP) / ROW_H) + 1
		if y >= LAYER_TOP and row >= 1 and row <= MAX_ROWS then
			local rows = layer_rows(page.get(Dwg), page.get(Layers))
			if rows[row] then
				toggle_layer(rows[row].name)
				return "handled"
			end
		end
	end
	return nil
end)

page.on("wheel", function(wx, wy, dx, dy)
	if not contains(DRAW, wx, wy) then
		return false
	end
	local view = current_view()
	local q = cad.to_drawing(DRAW, view, wx, wy)
	local ok, zoomed = pcall(cad.zoom, view, q.x, q.y, 1.15 ^ dy)
	if ok then
		page.set(View, zoomed)
	end
	return true
end)

page.on("key", function(k)
	if k == "delete" or k == "backspace" then
		return delete_selection()
	elseif k == "f" then
		fit()
		return true
	elseif k == "escape" then
		page.set(Sel, {})
		return true
	end
	local view = current_view()
	local step = view[3] * 0.01
	local moves = { left = { -step, 0 }, right = { step, 0 }, up = { 0, step }, down = { 0, -step } }
	local m = moves[k]
	if m then
		return move_selection(m[1], m[2])
	end
	return false
end)

page.on("file_drop", function(f)
	if string.lower(string.sub(f.name, -4)) == ".dxf" then
		return open_bytes(f.name, f.data)
	end
	page.set(Status, "只支援 DXF：" .. f.name)
	return true
end)

page.on("control", function(name, value)
	if name == "fit" then
		fit()
		return true
	end
	return false
end)

-- ---------------------------------------------------------------- semantic port

page.action({
	name = "select",
	description = "Select entities by id",
	params = { ids = "list of entity ids" },
	run = function(p)
		page.set(Sel, p.ids or {})
		return { selected = p.ids or {} }
	end,
})

page.action({
	name = "move",
	description = "Move the selected entities by dx, dy (drawing units)",
	params = { dx = "number", dy = "number" },
	run = function(p)
		return { ok = move_selection(p.dx or 0, p.dy or 0) }
	end,
})

page.action({
	name = "delete",
	description = "Delete the selected entities",
	params = {},
	run = function()
		return { ok = delete_selection() }
	end,
})

page.action({
	name = "layer",
	description = "Show or hide a layer",
	params = { name = "layer name", on = "boolean" },
	run = function(p)
		local overrides = table.clone(page.get(Layers))
		overrides[p.name] = p.on ~= false
		page.set(Layers, overrides)
		return { layers = overrides }
	end,
})

page.action({
	name = "add",
	description = "Add an entity (the shape cad.entities returns, e.g. {type = 'LINE', x1, y1, x2, y2, layer})",
	params = { entity = "entity table" },
	run = function(p)
		local dwg = cad.add(page.get(Dwg), p.entity)
		set_drawing(dwg, "新增圖元")
		return { ids = dwg:last_ids() }
	end,
})

page.action({
	name = "measure",
	description = "Length and area of the selection",
	params = {},
	run = function()
		return page.get(Dwg):measure(selected())
	end,
})

page.action({
	name = "entities",
	description = "Entities of the drawing, filtered by {layer, type, ids, rect, limit}",
	params = { filter = "table" },
	run = function(p)
		return page.get(Dwg):entities(p.filter or { limit = 200 })
	end,
})

page.action({
	name = "save",
	description = "Save the drawing as DXF",
	params = {},
	run = function()
		return { bytes = save() }
	end,
})
