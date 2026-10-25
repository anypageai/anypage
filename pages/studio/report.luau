-- report: the studio's PDF, built from the drawing and the workbook's totals.
--
--   R.build{ dwg, layers, totals, roles, unit_scale, title, file } -> doc, info
--
-- A4 landscape. Page 1 is the drawing sheet: the plan as vector paths
-- (dwg:to_paths fitted into the frame, recoloured for paper by layer role)
-- and, in the right-hand column, the takeoff with its totals, the unit
-- prices and the title block. The layout is measured from the contents:
-- the table's rows shrink a little to keep it on the sheet; a takeoff that
-- still does not fit moves to detail pages after the sheet (the totals come
-- right after its last row), and the sheet keeps a summary and the totals.
--
-- Cost is bounded so a report fits in one handler: a drawing with more
-- than PLAN_BUDGET entities on its visible layers prints only the takeoff
-- layers (walls, doors, windows, rooms, labels), and its texts are printed
-- as outlines only up to GLYPH_BUDGET characters and when they would be
-- legible on paper. The sheet says what was left out.
--
-- Every pdf call is an edit, so this runs at load or in handlers only.

local T = import("takeoff")
local rgba = color.rgba

local R = {}

R.WIDTH, R.HEIGHT = 842, 595 -- A4 landscape, points

-- entities drawn into the plan, and text characters drawn as outlines
-- (dwg:to_paths + pdf.add_path cost about 30 µs per entity and 0.15 ms
-- per glyph in a release build)
R.PLAN_BUDGET = 6000
R.GLYPH_BUDGET = 1500
-- texts smaller than this on paper are not printed (points)
local MIN_TEXT_PT = 1.2

local INK = rgba(25, 27, 32)
local DIM = rgba(105, 110, 120)
local RULE = rgba(170, 175, 185)
local ACCENT = rgba(196, 98, 16)

-- paper colours by layer role (screen colours do not print: ACI 2 is yellow)
local PRINT = {
	walls = rgba(20, 20, 20),
	doors = rgba(200, 95, 20),
	windows = rgba(30, 95, 200),
	rooms = rgba(0, 140, 150),
	labels = rgba(30, 30, 30),
}
local PRINT_OTHER = rgba(95, 100, 110)

-- page 1: the plan's frame, the right-hand column and the title block
local FRAME = { 30, 58, 506, 507 }
local COL_X, COL_W = 552, 262
local COL_TOP = 58
local TB = { COL_X, 466, COL_W, 99 }
local COL_BOTTOM = TB[2] - 8
-- the takeoff table on page 1: row height range (points)
local ROW_MAX, ROW_MIN = 12.5, 10.4
-- detail pages
local D_TOP, D_BOTTOM = 58, 546
local D_ROW, D_SIZE = 13, 8
local TOTALS_H = 56

local function rect_path(x, y, w, h)
	return { "M", x, y, "L", x + w, y, "L", x + w, y + h, "L", x, y + h, "Z" }
end

local function hline(p, x1, x2, y)
	table.insert(p, "M")
	table.insert(p, x1)
	table.insert(p, y)
	table.insert(p, "L")
	table.insert(p, x2)
	table.insert(p, y)
end

local function today()
	local ok, s = pcall(os.date, "%Y-%m-%d")
	return if ok and type(s) == "string" then s else ""
end

-- Width of `s` at `size` points, estimated on the safe side (Latin 0.6 em,
-- everything else a full em, as the CJK fonts set it).
local function text_w(s, size)
	local w = 0
	for _, c in utf8.codes(s) do
		w += if c < 128 then 0.6 else 1.0
	end
	return w * size
end

-- `s` on one line in at most `max_w` points: control characters become
-- spaces, and a long text is cut with an ellipsis.
local function fit(s, max_w, size)
	s = string.gsub(tostring(s or ""), "%c", " ")
	if utf8.len(s) == nil then
		s = string.gsub(s, "[\128-\255]", "?")
	end
	if text_w(s, size) <= max_w then
		return s
	end
	local out, w = {}, 0
	for _, c in utf8.codes(s) do
		local cw = (if c < 128 then 0.6 else 1.0) * size
		if w + cw + size > max_w then
			break
		end
		w += cw
		table.insert(out, utf8.char(c))
	end
	return table.concat(out) .. "…"
end
R.fit = fit

-- One column of text, a line per row: one pdf edit for the whole column.
local function column(doc, page, x, y, lines, size, row_h, opts)
	if #lines == 0 then
		return doc
	end
	local o = { x = x, y = y, text = table.concat(lines, "\n"), size = size, line_height = row_h / size, lang = "zh-Hant" }
	for k, v in opts or {} do
		o[k] = v
	end
	return pdf.add_text(doc, page, o)
end

local function rate_text(tt)
	return if type(tt.rate) == "number" then string.format("%s%%", T.plain(T.round(tt.rate * 100, 2))) else ""
end

-- ---------------------------------------------------------------- the plan

local function visible(l, overrides)
	local on = overrides[l.name]
	if on == nil then
		on = l.on
	end
	return on ~= false
end

-- What the plan prints: the layer overrides for to_paths, whether texts are
-- drawn, and notes on what was left out. Bounded by the budgets above.
local function plan_choice(dwg, overrides, roles, pt_per_unit)
	local all, core, names, core_names = 0, 0, {}, {}
	local layers = dwg:layers()
	for _, l in layers do
		if visible(l, overrides) then
			all += l.entities
			table.insert(names, l.name)
			if roles[l.name] then
				core += l.entities
				table.insert(core_names, l.name)
			end
		end
	end
	local notes = {}
	local use, shown = overrides, names
	if all > R.PLAN_BUDGET then
		if core == 0 or core > R.PLAN_BUDGET then
			return nil, false, { string.format("圖面過大（%d 個圖元），未列印 Plan too large to print", all) }
		end
		use = {}
		for _, l in layers do
			use[l.name] = visible(l, overrides) and roles[l.name] ~= nil
		end
		shown = core_names
		table.insert(notes, string.format("圖面較大（%d 個圖元）：只列印數量相關圖層 Takeoff layers only", all))
	end
	-- texts: outlines are costly, and illegible ones are noise
	local text = #shown > 0
	if text then
		local texts = dwg:entities({ type = { "TEXT", "MTEXT" }, layer = shown, limit = R.GLYPH_BUDGET // 4 })
		local glyphs, heights = 0, {}
		for _, t in texts do
			glyphs += utf8.len(t.text or "") or #(t.text or "")
			if type(t.height) == "number" then
				table.insert(heights, t.height)
			end
		end
		table.sort(heights)
		local median = heights[(#heights + 1) // 2]
		if #texts >= R.GLYPH_BUDGET // 4 or glyphs > R.GLYPH_BUDGET then
			text = false
			table.insert(notes, "文字過多未列印 Texts omitted")
		elseif median ~= nil and median * pt_per_unit < MIN_TEXT_PT then
			text = false
			table.insert(notes, "文字太小未列印 Texts too small at this scale")
		end
	end
	return use, text, notes
end

-- Paths of the drawing fitted into `fit_rect`, recoloured by layer role.
local function plan_paths(dwg, layers, roles, fit_rect, text)
	local paths = dwg:to_paths({ fit = fit_rect, flip_y = true, layers = layers, width = 0.35, text = text })
	for _, it in paths do
		local role = roles[it.layer]
		local c = PRINT[role or ""] or PRINT_OTHER
		if it.stroke ~= nil then
			it.stroke = c
			-- wide walls stay wide; room outlines become thin lines
			if role == "rooms" then
				it.width = 0.3
			end
		elseif it.fill ~= nil then
			it.fill = c
		end
	end
	return paths
end

-- ---------------------------------------------------------------- pieces

local function header(doc, page, title, right)
	doc = pdf.add_path(doc, page, rect_path(18, 18, R.WIDTH - 36, R.HEIGHT - 36), { stroke = INK, width = 1.2 })
	doc = pdf.add_text(doc, page, { x = 30, y = 28, size = 13, bold = true, color = INK, lang = "zh-Hant", text = title })
	doc = pdf.add_text(doc, page, { x = R.WIDTH - 30, y = 30, size = 9, color = DIM, align = "right", lang = "zh-Hant", text = right })
	return doc
end

-- subtotal, tax and the grand total under a table, from `y`; returns the
-- document and the y below the block
local function totals_block(doc, page, rules, x0, w, y, tt)
	doc = column(doc, page, x0, y + 3, { "小計 Subtotal", "營業稅 Tax " .. rate_text(tt) }, 8, 13, { color = INK })
	doc = column(doc, page, x0 + w, y + 3, { T.money(tt.subtotal), T.money(tt.tax) }, 8, 13, { color = INK, align = "right" })
	y += 31
	hline(rules, x0, x0 + w, y)
	doc = pdf.add_text(doc, page, { x = x0, y = y + 5, size = 11, bold = true, color = ACCENT, lang = "zh-Hant", text = "總計 Total" })
	doc = pdf.add_text(doc, page, { x = x0 + w, y = y + 5, size = 11, bold = true, color = ACCENT, align = "right", text = "NT$ " .. T.money(tt.total) })
	y += 24
	hline(rules, x0, x0 + w, y)
	return doc, y
end

local PRICE_ROWS = 9
local function prices_block(doc, page, x0, w, y, tt)
	doc = pdf.add_text(doc, page, { x = x0, y = y, size = 9, bold = true, color = INK, lang = "zh-Hant", text = "單價 Unit prices" })
	y += 14
	local plabels, pvalues = {}, {}
	for _, p in T.PRICES do
		local v = tt.prices[p.key]
		table.insert(plabels, p.label)
		if p.key == "tax" then
			table.insert(pvalues, if type(v) == "number" then T.plain(T.round(v * 100, 2)) .. " %" else "—")
		elseif p.key == "height" then
			table.insert(pvalues, if type(v) == "number" then string.format("%.2f m", v) else "—")
		else
			table.insert(pvalues, "NT$ " .. T.money(v) .. " / " .. p.unit)
		end
	end
	doc = column(doc, page, x0, y, plabels, 7, 11, { color = DIM })
	doc = column(doc, page, x0 + w, y, pvalues, 7, 11, { color = INK, align = "right" })
	return doc, y + #plabels * 11
end
local PRICES_H = 14 + PRICE_ROWS * 11

local function title_block(doc, page, a, sheet)
	local rows = {
		{ "專案 Project", a.title or "AnyPage 示範住宅" },
		{ "圖名 Drawing", "平面圖與數量計算 Plan & takeoff" },
		{ "檔案 File", a.file or "" },
		{ "日期 Date", today() },
		{ "頁次 Sheet", sheet },
		{ "製圖 By", "AnyPage Studio (cad · sheet · pdf)" },
	}
	local tl, tv = {}, {}
	for _, r in rows do
		table.insert(tl, r[1])
		table.insert(tv, fit(r[2], TB[3] - 82, 7.5))
	end
	local grid = rect_path(TB[1], TB[2], TB[3], TB[4])
	local rh = TB[4] / #rows
	for i = 1, #rows - 1 do
		hline(grid, TB[1], TB[1] + TB[3], TB[2] + i * rh)
	end
	table.insert(grid, "M")
	table.insert(grid, TB[1] + 70)
	table.insert(grid, TB[2])
	table.insert(grid, "L")
	table.insert(grid, TB[1] + 70)
	table.insert(grid, TB[2] + TB[4])
	doc = pdf.add_path(doc, page, grid, { stroke = INK, width = 0.6 })
	doc = column(doc, page, TB[1] + 5, TB[2] + (rh - 7) / 2, tl, 7, rh, { color = DIM })
	doc = column(doc, page, TB[1] + 76, TB[2] + (rh - 7.5) / 2, tv, 7.5, rh, { color = INK })
	return doc
end

-- ---------------------------------------------------------------- layout

-- The page-1 table at row height `rh`: section title, header and rows,
-- totals, unit prices. Its height in points.
local function sheet_height(n, rh)
	return 18 + (n + 1) * rh + 2 + TOTALS_H + 14 + PRICES_H
end

-- Detail pages: { from, to } row ranges, the last one followed by the
-- totals (on a page of their own when they do not fit under the rows).
local function paginate(n)
	local per = math.floor((D_BOTTOM - D_TOP) / D_ROW) - 1
	local pages = {}
	local i = 1
	while i <= n do
		local last = math.min(n, i + per - 1)
		table.insert(pages, { from = i, to = last })
		i = last + 1
	end
	if #pages == 0 then
		table.insert(pages, { from = 1, to = 0 })
	end
	local lp = pages[#pages]
	local bottom = D_TOP + (lp.to - lp.from + 2) * D_ROW + 8
	if bottom + TOTALS_H > D_BOTTOM then
		table.insert(pages, { from = n + 1, to = n })
	end
	return pages
end

-- ---------------------------------------------------------------- build

-- Build the report. `a`: { dwg, layers (visibility overrides), totals (from
-- T.totals), roles (layer -> role), unit_scale (metres per unit), title, file }.
-- Returns the document and { paths, scale, pages, notes }.
function R.build(a)
	local tt = a.totals or { rows = {}, prices = {} }
	local rows = tt.rows or {}
	local n = #rows
	local W = R.WIDTH
	local avail = COL_BOTTOM - COL_TOP
	local rh = math.min(ROW_MAX, (avail - sheet_height(0, 0)) / (n + 1))
	local one_page = rh >= ROW_MIN
	local details = if one_page then {} else paginate(n)
	local npages = 1 + #details
	local doc = pdf.new({ width = W, height = R.HEIGHT, pages = npages })
	local file = a.file or ""

	-- ---- page 1: the drawing sheet
	doc = header(doc, 1, "AnyPage Studio · 平面圖與數量計算報告 Floor plan & quantity takeoff", today())
	local fx, fy, fw, fh = FRAME[1], FRAME[2], FRAME[3], FRAME[4]
	doc = pdf.add_path(doc, 1, rect_path(fx, fy, fw, fh), { stroke = RULE, width = 0.6 })
	local pad = 12
	local b = a.dwg:bounds()
	-- the plan's box (room at the bottom for `lines` lines of notes) and its
	-- scale in points per drawing unit
	local function plan_box(lines)
		local r = { fx + pad, fy + pad, fw - 2 * pad, fh - 2 * pad - 4 - 10 * lines }
		if b and b[3] > 0 and b[4] > 0 then
			return r, math.min(r[3] / b[3], r[4] / b[4])
		end
		return r, 0
	end
	local fit_rect, pt_per_unit = plan_box(1)
	local roles = a.roles or {}
	local layers, text, notes = plan_choice(a.dwg, a.layers or {}, roles, pt_per_unit)
	if #notes > 0 then
		fit_rect, pt_per_unit = plan_box(1 + #notes)
	end
	local scale_note = ""
	if pt_per_unit > 0 then
		local ratio = (a.unit_scale or 0.001) * 1000 / (pt_per_unit * 25.4 / 72)
		scale_note = string.format("比例 Scale 約 1:%d (A4)", math.floor(ratio + 0.5))
	end
	local paths = {}
	if layers ~= nil then
		paths = plan_paths(a.dwg, layers, roles, fit_rect, text)
		if #paths > 0 then
			doc = pdf.add_path(doc, 1, paths)
		end
	end
	local foot = { fit(file, 200, 7.5) .. "   " .. scale_note }
	for _, s in notes do
		table.insert(foot, 1, s)
	end
	doc = column(doc, 1, fx + 8, fy + fh - 4 - #foot * 10, foot, 7.5, 10, { color = DIM })

	-- the right-hand column
	local rules = {}
	local x0, w = COL_X, COL_W
	local y = COL_TOP
	if one_page then
		local size = rh * 7.2 / 12.5
		doc = pdf.add_text(doc, 1, { x = x0, y = y, size = 10, bold = true, color = INK, lang = "zh-Hant", text = "數量計算 Quantity takeoff" })
		y += 18
		local q_right, u_left = x0 + 168, x0 + 172
		local names, qtys, units, amounts = { "項目 Item" }, { "數量" }, { "單位" }, { "金額 NT$" }
		for _, r in rows do
			table.insert(names, fit(r.name, 124, size))
			table.insert(qtys, T.qty(r.qty, r.kind))
			table.insert(units, fit(r.unit, 22, size))
			table.insert(amounts, T.money(r.amount))
		end
		hline(rules, x0, x0 + w, y - 2)
		hline(rules, x0, x0 + w, y + rh - 2)
		doc = column(doc, 1, x0, y, names, size, rh, { color = INK })
		doc = column(doc, 1, q_right, y, qtys, size, rh, { color = INK, align = "right" })
		doc = column(doc, 1, u_left, y, units, size, rh, { color = DIM })
		doc = column(doc, 1, x0 + w, y, amounts, size, rh, { color = INK, align = "right" })
		y += #names * rh + 2
		hline(rules, x0, x0 + w, y - 2)
		doc, y = totals_block(doc, 1, rules, x0, w, y, tt)
	else
		-- a summary; the rows are on the detail pages
		doc = pdf.add_text(doc, 1, { x = x0, y = y, size = 10, bold = true, color = INK, lang = "zh-Hant", text = "數量計算摘要 Takeoff summary" })
		y += 18
		local rooms, area = 0, 0
		local by_kind = {}
		for _, r in rows do
			if r.kind == "room" then
				rooms += 1
				area += r.qty or 0
			elseif r.key then
				by_kind[r.key] = r
			end
		end
		local function q(key, unit)
			local r = by_kind[key]
			return if r then T.qty(r.qty, r.kind) .. " " .. unit else "—"
		end
		local labels = { "房間 Rooms", "外牆 Exterior walls", "隔間牆 Partitions", "門 / 窗 Doors / windows", "明細 Details" }
		local values = {
			string.format("%d 間 · %s m²", rooms, T.qty(area)),
			q("ext_wall", "m"),
			q("int_wall", "m"),
			q("door", "樘") .. " / " .. q("window", "樘"),
			if npages == 2 then "第 2 頁 page 2" else string.format("第 2–%d 頁 pages 2–%d", npages, npages),
		}
		doc = column(doc, 1, x0, y, labels, 7.5, 12, { color = DIM })
		doc = column(doc, 1, x0 + w, y, values, 7.5, 12, { color = INK, align = "right" })
		y += #labels * 12 + 6
		hline(rules, x0, x0 + w, y - 2)
		doc, y = totals_block(doc, 1, rules, x0, w, y, tt)
	end
	y += 14
	doc, y = prices_block(doc, 1, x0, w, y, tt)
	doc = title_block(doc, 1, a, string.format("1 / %d", npages))
	doc = pdf.add_path(doc, 1, rules, { stroke = RULE, width = 0.5 })

	-- ---- detail pages: the whole takeoff table, full width
	local cols = { item = 36, type = 330, qty = 560, unit = 566, price = 700, amount = W - 36 }
	for k, pg in details do
		local page = k + 1
		doc = header(doc, page, "AnyPage Studio · 數量計算明細 Quantity takeoff", string.format("第 %d / %d 頁 Page %d of %d", page, npages, page, npages))
		local prules = {}
		y = D_TOP
		local names, types, qtys, units, prices, amounts = { "項目 Item" }, { "類別 Type" }, { "數量 Qty" }, { "單位" }, { "單價 Unit price" }, { "金額 NT$" }
		for i = pg.from, pg.to do
			local r = rows[i]
			table.insert(names, fit(r.name, cols.type - cols.item - 12, D_SIZE))
			table.insert(types, fit(r.type, cols.qty - cols.type - 70, D_SIZE))
			table.insert(qtys, T.qty(r.qty, r.kind))
			table.insert(units, fit(r.unit, 26, D_SIZE))
			table.insert(prices, T.money(r.price))
			table.insert(amounts, T.money(r.amount))
		end
		if pg.to >= pg.from then
			hline(prules, 30, W - 30, y - 3)
			hline(prules, 30, W - 30, y + D_ROW - 3)
			doc = column(doc, page, cols.item, y, names, D_SIZE, D_ROW, { color = INK })
			doc = column(doc, page, cols.type, y, types, D_SIZE, D_ROW, { color = DIM })
			doc = column(doc, page, cols.qty, y, qtys, D_SIZE, D_ROW, { color = INK, align = "right" })
			doc = column(doc, page, cols.unit, y, units, D_SIZE, D_ROW, { color = DIM })
			doc = column(doc, page, cols.price, y, prices, D_SIZE, D_ROW, { color = INK, align = "right" })
			doc = column(doc, page, cols.amount, y, amounts, D_SIZE, D_ROW, { color = INK, align = "right" })
			y += #names * D_ROW + 2
			hline(prules, 30, W - 30, y - 3)
		end
		if k == #details then
			-- the totals right after the last row
			doc = totals_block(doc, page, prules, W - 36 - 300, 300, y + 6, tt)
		else
			doc = pdf.add_text(doc, page, { x = W - 36, y = y + 4, size = 7.5, color = DIM, align = "right", lang = "zh-Hant", text = "續下頁 Continued on the next page" })
		end
		doc = pdf.add_text(doc, page, { x = 36, y = D_BOTTOM + 12, size = 7, color = DIM, lang = "zh-Hant",
			text = fit(file, 400, 7) .. " · AnyPage Studio" })
		doc = pdf.add_path(doc, page, prules, { stroke = RULE, width = 0.5 })
	end
	return doc, { paths = #paths, scale = scale_note, pages = npages, notes = notes, text = text }
end

return R
