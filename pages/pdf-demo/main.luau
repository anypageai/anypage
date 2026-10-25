-- AnyPage PDF demo: a page package using the `pdf` module.
--
-- * The document is an input cell holding a `pdf.document` handle. Every edit
--   returns the next version of the handle; setting it back into the cell makes
--   everything that reads the document (tiles, search, extraction) recompute.
-- * The page is drawn by native tiles (`pdf.page`), crisp at any zoom.
-- * Search and extraction run as jobs, off the UI thread.
-- * Click the page to add a note; type to change the search; arrows turn pages;
--   drop a PDF on the window to open it.

local rgba, alpha = color.rgba, color.alpha

local function R(x, y, w, h)
	return { x, y, w, h }
end

local PAGE = R(24, 84, 612, 792)
local HEADER = R(0, 0, 1520, 70)
local CONTROLS = R(660, 84, 840, 210)
local BLOCKS = R(660, 310, 840, 330)
local TABLE = R(660, 656, 840, 220)

-- controls, panel-local
local PREV = R(16, 16, 100, 36)
local NEXT = R(124, 16, 100, 36)
local QUERY = R(16, 66, 420, 36)
local HIGHLIGHT = R(452, 66, 170, 36)
local STAMP = R(636, 66, 188, 36)
local ROTATE = R(16, 116, 150, 36)
local INSERT = R(180, 116, 150, 36)
local OPEN = R(344, 116, 150, 36)
local SAVE = R(508, 116, 150, 36)

local BG = rgba(22, 25, 32)
local FG = rgba(232, 236, 244)
local DIM = rgba(140, 148, 164)
local ACCENT = rgba(255, 150, 40)

-- ---------------------------------------------------------------- the sample document

local function sample()
	local d = pdf.new({ width = 612, height = 792, pages = 2 })
	d = pdf.add_text(d, 1, { x = 56, y = 56, text = "AnyPage Quarterly Report", size = 26, bold = true })
	d = pdf.add_text(d, 1, {
		x = 56, y = 100, size = 11, width = 500,
		text = "Revenue grew in every region this quarter. This document was written by the page script with pdf.new and pdf.add_text, drawn by the engine as native tiles, and searched and extracted by the same module.",
	})
	d = pdf.add_text(d, 1, {
		x = 56, y = 160, size = 13, width = 500, color = rgba(30, 60, 140),
		text = "本季營收在各地區皆有成長。這份文件由頁面腳本產生，可以搜尋、擷取與編輯。",
	})
	local rows = {
		{ "Region", "Q1", "Q2", "Revenue" },
		{ "North", "120", "135", "1,250.00" },
		{ "South", "98", "110", "980.50" },
		{ "East", "143", "160", "1,620.75" },
		{ "West", "87", "91", "760.00" },
	}
	for i, row in ipairs(rows) do
		for j, cell in ipairs(row) do
			d = pdf.add_text(d, 1, { x = 56 + (j - 1) * 120, y = 236 + (i - 1) * 18, size = 11, text = cell, bold = i == 1 })
		end
	end
	-- a bar chart as vector paths
	local bars = {}
	for i, v in ipairs({ 1250, 980.5, 1620.75, 760 }) do
		local x, h = 80 + (i - 1) * 110, v / 10
		table.insert(bars, { path = { "M", x, 520, "L", x + 60, 520, x + 60, 520 - h, x, 520 - h, "Z" }, fill = rgba(70, 130, 220) })
		-- centred under its bar
		d = pdf.add_text(d, 1, { x = x + 30, y = 528, size = 10, align = "center", color = rgba(90, 90, 90), text = rows[i + 1][1] })
	end
	d = pdf.add_path(d, 1, bars)
	d = pdf.add_path(d, 1, { "M", 60, 520, "L", 540, 520 }, { stroke = rgba(40, 40, 40), width = 1.5 })
	d = pdf.add_text(d, 2, { x = 56, y = 56, text = "Notes", size = 22, bold = true })
	d = pdf.add_text(d, 2, {
		x = 56, y = 96, size = 12, width = 500, font = "serif",
		text = "Revenue figures are unaudited. Search for a word to see every place it appears; the highlight button turns the hits on this page into highlight annotations that any PDF reader shows.",
	})
	return d
end

-- ---------------------------------------------------------------- cells

local Doc = page.input("doc", sample())
local PageNo = page.input("page", 1, { agent = true })
local Query = page.input("query", "revenue", { agent = true })
local Notes = page.input("notes", 0)

-- where the current page sits inside the page box, keeping its shape
local function fit(doc, p)
	local sz = doc:page_size(p)
	local s = math.min(PAGE[3] / sz.w, PAGE[4] / sz.h)
	return R(PAGE[1] + (PAGE[3] - sz.w * s) / 2, PAGE[2], sz.w * s, sz.h * s), s
end

local Info = page.derive("info", { Doc }, function(doc)
	local m = doc:metadata()
	return { pages = doc:page_count(), title = m.title, version = tostring(doc) }
end)

local TileParams = page.derive("tile_params", { Doc, PageNo }, function(doc, p)
	local r = fit(doc, p)
	return { doc = doc, page = p, rect = r }
end)

local Hits = page.job("hits", "pdf.search", { Doc, Query })
local Extract = page.job("extract", "pdf.extract", { Doc, PageNo })

-- ---------------------------------------------------------------- drawing

local function button(d, r, label, on)
	d:rrect(r[1], r[2], r[3], r[4], 8, on and alpha(ACCENT, 70) or rgba(44, 50, 62))
	d:stroke(r[1] + 0.5, r[2] + 0.5, r[3] - 1, r[4] - 1, 8, 1, alpha(FG, 40))
	d:text(r[1] + 12, r[2] + 9, 15, FG, label)
end

local function pending(v)
	return type(v) ~= "table" or v.state == "pending" or v.state == "error"
end

page.panel({
	rect = HEADER, name = "header", inputs = { Info },
	draw = function(d, info)
		d:text(24, 12, 26, FG, "AnyPage PDF")
		d:text(24, 44, 13, DIM, "native tiles · search and extraction jobs · edits as new document versions · drop a PDF to open it")
		d:text(1100, 20, 14, DIM, info.version)
	end,
})

page.tiled({ rect = PAGE, source = "pdf.page", params = TileParams })

-- search hits over the page
page.panel({
	rect = PAGE, name = "hits_overlay", class = "async", inputs = { Hits, PageNo, Doc },
	draw = function(d, hits, p, doc)
		if pending(hits) then
			return
		end
		local r, s = fit(doc, p)
		local ox, oy = r[1] - PAGE[1], r[2] - PAGE[2]
		for _, h in ipairs(hits) do
			if h.page == p then
				for _, q in ipairs(h.rects) do
					local x, y, w, hh = ox + q[1] * s - 2, oy + q[2] * s - 2, q[3] * s + 4, q[4] * s + 4
					d:rrect(x, y, w, hh, 3, alpha(ACCENT, 50))
					d:stroke(x, y, w, hh, 3, 1.5, alpha(ACCENT, 220))
				end
			end
		end
	end,
})

page.panel({
	rect = CONTROLS, name = "controls", class = "ops", inputs = { Info, PageNo, Query, Hits },
	draw = function(d, info, p, q, hits)
		d:rrect(0, 0, d.w, d.h, 14, BG)
		button(d, PREV, "◀ Prev")
		button(d, NEXT, "Next ▶")
		d:text(240, 25, 16, FG, string.format("page %d / %d", p, info.pages))
		d:rrect(QUERY[1], QUERY[2], QUERY[3], QUERY[4], 8, rgba(14, 16, 22))
		d:stroke(QUERY[1] + 0.5, QUERY[2] + 0.5, QUERY[3] - 1, QUERY[4] - 1, 8, 1, alpha(ACCENT, 160))
		d:text(QUERY[1] + 12, QUERY[2] + 9, 15, FG, "Search: " .. q .. "▏")
		button(d, HIGHLIGHT, "Highlight hits", true)
		button(d, STAMP, "Add stamp")
		button(d, ROTATE, "Rotate page")
		button(d, INSERT, "Insert page")
		button(d, OPEN, "Open PDF…")
		button(d, SAVE, "Save PDF")
		local status
		if pending(hits) then
			status = "searching…"
		else
			local here = 0
			for _, h in ipairs(hits) do
				if h.page == p then
					here += 1
				end
			end
			status = string.format("%d hits (%d on this page)", #hits, here)
		end
		d:text(16, 172, 14, DIM, status .. " · click the page to add a note · type to search · ← → turn pages")
	end,
})

page.panel({
	rect = BLOCKS, name = "blocks", class = "async", inputs = { Extract },
	draw = function(d, ex)
		d:rrect(0, 0, d.w, d.h, 14, BG)
		d:text(16, 12, 16, FG, "Extracted blocks (reading order)")
		if pending(ex) then
			d:text(16, 44, 14, DIM, ex and ex.message or "extracting…")
			return
		end
		local y = 44
		for i, b in ipairs(ex.blocks) do
			if y > d.h - 36 then
				d:text(16, y, 13, DIM, string.format("… %d more", #ex.blocks - i + 1))
				break
			end
			d:text(16, y, 12, ACCENT, string.format("%d · %.0f pt · (%.0f, %.0f)", i, b.size, b.x, b.y))
			local lines = math.min(b.lines, 2)
			d:textbox(16, y + 16, d.w - 32, 18 * lines, 13, FG, b.text, false)
			y += 22 + 18 * lines
		end
	end,
})

page.panel({
	rect = TABLE, name = "table", class = "async", inputs = { Extract },
	draw = function(d, ex)
		d:rrect(0, 0, d.w, d.h, 14, BG)
		d:text(16, 12, 16, FG, "Detected table (computable cells)")
		if pending(ex) or #ex.tables == 0 then
			d:text(16, 44, 14, DIM, pending(ex) and "…" or "no table on this page")
			return
		end
		local t = ex.tables[1]
		local cw = math.min(180, (d.w - 32) / #t.columns)
		for j, c in ipairs(t.columns) do
			d:text(16 + (j - 1) * cw, 44, 14, ACCENT, c)
		end
		for i, row in ipairs(t.rows) do
			if i > 7 then
				break
			end
			for j = 1, #t.columns do
				local v = row[j]
				local s = v == nil and "" or (type(v) == "number" and string.format("%.2f", v) or tostring(v))
				d:text(16 + (j - 1) * cw, 44 + i * 20, 14, type(v) == "number" and rgba(140, 220, 160) or FG, s)
			end
		end
	end,
})

-- ---------------------------------------------------------------- actions

local function inside(r, x, y)
	return x >= r[1] and y >= r[2] and x < r[1] + r[3] and y < r[2] + r[4]
end

local function set_doc(d)
	page.set(Doc, d)
end

local function goto_page(p)
	local n = page.get(Doc):page_count()
	page.set(PageNo, math.clamp(p, 1, n))
end

local function highlight_hits()
	local doc, p = page.get(Doc), page.get(PageNo)
	local q = page.get(Query)
	if q == "" then
		return 0
	end
	local hits = doc:search(q, { page = p })
	if #hits > 0 then
		set_doc(pdf.highlight(doc, p, hits, rgba(255, 225, 0, 200), { contents = "search: " .. q }))
	end
	return #hits
end

local function stamp()
	local doc, p = page.get(Doc), page.get(PageNo)
	local sz = doc:page_size(p)
	local x = sz.w - 200
	doc = pdf.add_path(doc, p, { "M", x, 24, "L", x + 170, 24, x + 170, 64, x, 64, "Z" }, { stroke = rgba(200, 30, 30), width = 2.5 })
	doc = pdf.add_text(doc, p, { x = x + 12, y = 34, size = 18, bold = true, color = rgba(200, 30, 30), text = "已審閱 REVIEWED" })
	set_doc(doc)
end

local function open_bytes(bytes)
	local ok, doc = pcall(pdf.open, bytes)
	if ok then
		set_doc(doc)
		page.set(PageNo, 1)
	else
		page.log("could not open: " .. tostring(doc))
	end
end

page.on("pointer_down", function(x, y)
	local cx, cy = x - CONTROLS[1], y - CONTROLS[2]
	if inside(CONTROLS, x, y) then
		local p = page.get(PageNo)
		if inside(PREV, cx, cy) then
			goto_page(p - 1)
		elseif inside(NEXT, cx, cy) then
			goto_page(p + 1)
		elseif inside(HIGHLIGHT, cx, cy) then
			highlight_hits()
		elseif inside(STAMP, cx, cy) then
			stamp()
		elseif inside(ROTATE, cx, cy) then
			set_doc(pdf.rotate_page(page.get(Doc), p, 90))
		elseif inside(INSERT, cx, cy) then
			set_doc(pdf.insert_page(page.get(Doc), p + 1))
			goto_page(p + 1)
		elseif inside(OPEN, cx, cy) then
			page.open_file({ filters = { "pdf" }, done = function(f)
				if f.data then
					open_bytes(f.data)
				end
			end })
		elseif inside(SAVE, cx, cy) then
			page.save_file({ name = "anypage-edited.pdf", data = page.get(Doc):save() })
		end
		return "handled"
	end
	if inside(PAGE, x, y) then
		local doc, p = page.get(Doc), page.get(PageNo)
		local r, s = fit(doc, p)
		if inside(r, x, y) then
			local n = page.get(Notes) + 1
			page.set(Notes, n)
			set_doc(pdf.add_note(doc, p, { x = (x - r[1]) / s - 10, y = (y - r[2]) / s - 10, text = "Note " .. n, title = "AnyPage" }))
			return "handled"
		end
	end
	return false
end)

page.on("key", function(k)
	if k == "right" or k == "pagedown" then
		goto_page(page.get(PageNo) + 1)
		return true
	elseif k == "left" or k == "pageup" then
		goto_page(page.get(PageNo) - 1)
		return true
	elseif k == "backspace" then
		local q = page.get(Query)
		page.set(Query, string.sub(q, 1, text.prev(q, #q)))
		return true
	elseif k == "escape" then
		page.set(Query, "")
		return true
	end
	return false
end)

page.on("text", function(t)
	page.set(Query, page.get(Query) .. t)
end)

page.on("file_drop", function(f)
	open_bytes(f.data)
	return true
end)

-- the semantic port: what an agent can do here
page.action({
	name = "search",
	description = "Search the document. Returns the hits on the page shown now; the hits on every page arrive in the \"hits\" cell (read it with { get = \"hits\" }), filled by the pdf.search job off the UI thread",
	params = { query = "string" },
	run = function(a)
		local q = a.query or ""
		page.set(Query, q)
		-- one page inline is cheap; the whole document is the job's work
		local p = page.get(PageNo)
		local here = q == "" and {} or page.get(Doc):search(q, { page = p, limit = 100 })
		return { query = q, page = p, this_page = here, all_pages = "hits" }
	end,
})
page.action({
	name = "highlight_hits", description = "Turn the current search hits on the current page into highlight annotations",
	run = function()
		return { highlighted = highlight_hits() }
	end,
})
page.action({
	name = "goto", description = "Show a page", params = { page = "number" },
	run = function(a)
		goto_page(a.page or 1)
		return { page = page.get(PageNo) }
	end,
})
page.action({
	name = "extract", description = "Text blocks of a page in reading order, and its tables as CSV", params = { page = "number" },
	run = function(a)
		local ex = page.get(Doc):extract(a.page or page.get(PageNo))
		local blocks, tables = {}, {}
		for _, b in ipairs(ex.blocks) do
			table.insert(blocks, b.text)
		end
		for _, t in ipairs(ex.tables) do
			local lines = { table.concat(t.columns, ",") }
			for _, row in ipairs(t.rows) do
				local cells = {}
				for j = 1, #t.columns do
					cells[j] = row[j] == nil and "" or tostring(row[j])
				end
				table.insert(lines, table.concat(cells, ","))
			end
			table.insert(tables, table.concat(lines, "\n"))
		end
		return { page = ex.page, blocks = blocks, tables = tables }
	end,
})
page.action({
	name = "add_text", description = "Write text on a page (points from the top-left)",
	params = { page = "number", x = "number", y = "number", text = "string", size = "number" },
	run = function(a)
		set_doc(pdf.add_text(page.get(Doc), a.page or page.get(PageNo), { x = a.x, y = a.y, text = a.text, size = a.size or 12 }))
		return { version = tostring(page.get(Doc)) }
	end,
})
