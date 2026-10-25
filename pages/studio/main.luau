-- AnyPage Studio 工作室: one floor plan; CAD, spreadsheet and PDF on the same data.
--
--   CAD    the plan (DXF) as native vector tiles, a layer panel, picking
--   sheet  the quantity takeoff: room areas and wall lengths measured from
--          the plan, priced by formulas (per-room cost, SUM, tax, total);
--          unit prices are editable and everything recomputes
--   PDF    a report: the plan as vector paths, the takeoff table, unit
--          prices and a title block, shown by native PDF tiles
--   3D     (optional) a STEP part as a mesh in a scene.view layer
--
-- The drawing, the workbook and the report are resources held in input
-- cells. Every edit returns the resource's next version, so whatever read
-- the old one recomputes: the tiles, the `totals` cell (the workbook's
-- computed values), the report's "out of date" badge. Slow work runs as
-- jobs: opening a DXF (cad.load), the STEP part's mesh (cad.step_mesh) and
-- the XLSX export (sheet.export, kept current so saving is instant).
--
-- Two arrangements of the same cells (see "layout"): wide for desktop
-- windows, three tabbed screens for phones and portrait windows, chosen
-- from page.viewport().
--
-- Agent actions: measure_rooms, set_price {item, price, unit}, make_report,
-- save {format = "pdf" | "xlsx" | "dxf"}, select {ids}, layer {name, on},
-- view {rect | fit}, layout {layout}. The view, layer and selection cells
-- are not agent-writable: the actions check what they are given.

local ui = import("ui")
local T = import("takeoff")
local Rep = import("report")
local rgba, alpha = color.rgba, color.alpha

local function R(x, y, w, h)
	return { x, y, w, h }
end

local function contains(r, x, y)
	return x >= r[1] and x <= r[1] + r[3] and y >= r[2] and y <= r[2] + r[4]
end

local function clamp(x, lo, hi)
	return math.max(lo, math.min(hi, x))
end

local function finite(x)
	return type(x) == "number" and x == x and x > -math.huge and x < math.huge
end

-- ---------------------------------------------------------------- checked values

-- Entity ids: whole numbers >= 0; anything else is dropped.
local function as_ids(v)
	local out = {}
	if type(v) == "table" then
		for _, x in v do
			if finite(x) and x >= 0 and x % 1 == 0 then
				table.insert(out, x)
			end
		end
	end
	return out
end

-- A view rect { x, y, w, h } with finite numbers and w, h > 0, or nil.
local function as_rect(v)
	if type(v) ~= "table" then
		return nil
	end
	local x, y, w, h = v[1], v[2], v[3], v[4]
	if finite(x) and finite(y) and finite(w) and finite(h) and w > 0 and h > 0 then
		return { x, y, w, h }
	end
	return nil
end

-- Layer visibility overrides: { name = bool }; anything else is dropped.
local function as_layers(v)
	local out = {}
	if type(v) == "table" then
		for k, on in v do
			if type(k) == "string" and type(on) == "boolean" then
				out[k] = on
			end
		end
	end
	return out
end

-- ---------------------------------------------------------------- layout
--
-- Two arrangements of the same cells:
--   wide    the page.json bounds: every panel at once (desktop, landscape)
--   narrow  portrait windows and phones: three phone-sized screens (plan,
--           takeoff, report) side by side, far right of the wide one. Each
--           has a header whose tabs switch screens; the page fits the camera
--           to the screen shown (page.fit_rect), so it fills the window's
--           width and the others stay out of view. The STEP part is in the
--           wide arrangement only.
-- page.viewport()'s "viewport" event picks the arrangement (and fits it
-- again) whenever the window's size changes; the engine draws only the
-- panels in view, so the other arrangement costs nothing.

local WIDE = R(0, 0, 1600, 1000)
local BAR = R(0, 0, 1600, 60)
local CAD = R(12, 72, 780, 512) -- the plan, native tiles
local SIDE = R(12, 596, 384, 392) -- layers and selection
local PART_HEAD = R(408, 596, 384, 40)
local PART = R(408, 636, 384, 352) -- the STEP part, GPU layer
local TAKE = R(804, 72, 784, 476) -- takeoff: prices, table, totals
local PDFBOX = R(804, 560, 600, 424) -- the report page (842 x 595 pt at 0.7126)
local RINFO = R(1416, 560, 172, 428)

local NX, NW, NH, NGAP = 2000, 440, 900, 360
local SCREEN_ORDER = { "plan", "takeoff", "report" }
local SCREEN_LABELS = { "平面圖 Plan", "數量 Takeoff", "報告 Report" }
local SCREENS = { wide = WIDE }
for i, k in SCREEN_ORDER do
	SCREENS[k] = R(NX + (i - 1) * (NW + NGAP), 0, NW, NH)
end
local HEAD_H = 152
local function in_screen(k, x, y, w, h)
	return R(SCREENS[k][1] + x, SCREENS[k][2] + y, w, h)
end
local NCAD = in_screen("plan", 8, HEAD_H + 8, NW - 16, 420)
local NSIDE = in_screen("plan", 8, HEAD_H + 436, NW - 16, NH - HEAD_H - 444)
local NTAKE = in_screen("takeoff", 8, HEAD_H + 8, NW - 16, NH - HEAD_H - 16)
local NPDF_H = math.floor((NW - 16) * 595 / 842 + 0.5)
local NPDF = in_screen("report", 8, HEAD_H + 8, NW - 16, NPDF_H)
local NRINFO = in_screen("report", 8, HEAD_H + 16 + NPDF_H, NW - 16, NH - HEAD_H - 24 - NPDF_H)
-- the plan's tiles and overlay, in both arrangements
local PLANS = { CAD, NCAD }

local function plan_at(x, y)
	for _, r in PLANS do
		if contains(r, x, y) then
			return r
		end
	end
	return nil
end

local TH = ui.themes.dark
local CAD_BG = rgba(22, 25, 31)
local PART_BG = rgba(22, 25, 31)
local HILITE = rgba(255, 170, 40)
local TEXT_HI = TH.text
local TEXT_LO = TH.text_dim

-- the UI apps, for blurring them when the plan takes the pointer
local APPS = {}

-- ---------------------------------------------------------------- the drawing

local SAMPLE = "floorplan.dxf"
local sample_dwg = cad.open(page.asset(SAMPLE), "dxf")

-- the whole drawing, with room at the top for the plan's header strip
local function fit_view(dwg)
	local b = dwg:bounds({ margin = 0.04 })
	if b == nil then
		return { 0, 0, 100, 100 }
	end
	return { b[1], b[2], b[3], b[4] * (1 + 36 / 512) }
end

-- zoom limits: the view's larger side from 1/1000 to 1000 times the drawing's
local ZOOM_IN, ZOOM_OUT = 1e-3, 1e3

local function drawing_size(dwg)
	local b = dwg:bounds()
	if b == nil then
		return 100, { 0, 0, 100, 100 }
	end
	return math.max(b[3], b[4], 1e-9), b
end

-- `v` within the zoom limits, its centre within reach of the drawing
local function clamp_view(dwg, v)
	local size, b = drawing_size(dwg)
	local lo, hi = size * ZOOM_IN, size * ZOOM_OUT
	local w = math.max(v[3], v[4])
	local f = if w < lo then lo / w elseif w > hi then hi / w else 1
	local cx = clamp(v[1] + v[3] / 2, b[1] - hi, b[1] + b[3] + hi)
	local cy = clamp(v[2] + v[4] / 2, b[2] - hi, b[2] + b[4] + hi)
	local nw, nh = v[3] * f, v[4] * f
	return { cx - nw / 2, cy - nh / 2, nw, nh }
end

Dwg = page.input("drawing", sample_dwg)
FileName = page.input("file_name", SAMPLE)
-- the view rect in drawing units; nil fits the drawing (agents: action `view`)
View = page.input("view", nil)
-- layer name -> visible (overrides; agents: action `layer`)
Layers = page.input("layers", {})
-- selected entity ids (agents: action `select`)
Sel = page.input("selected", {})
Status = page.input("status", "")
-- { bytes, into, name } while a dropped or opened DXF loads
Opening = page.input("opening", nil)

-- ---------------------------------------------------------------- the workbook

local q0 = T.measure(sample_dwg)
local wb0, meta0 = T.write(T.new_book(), q0)
Book = page.input("book", wb0)
-- which workbook rows hold which rooms and entities
Meta = page.input("takeoff", meta0)
-- the workbook's computed values: rows, prices, subtotal, tax, total
Totals = page.derive("totals", { Book, Meta }, function(wb, meta)
	return T.totals(wb, meta)
end)

local function layers_key(layers)
	local keys = {}
	for k, v in as_layers(layers) do
		table.insert(keys, k .. "=" .. (if v then "1" else "0"))
	end
	table.sort(keys)
	return table.concat(keys, ";")
end

local function roles_of(dwg)
	return T.classify(dwg:layers()).role
end

-- ---------------------------------------------------------------- the report

local report_count = 0

local function clock_text()
	local ok, s = pcall(os.date, "%H:%M:%S")
	return if ok and type(s) == "string" then s else ""
end

-- the report for the current drawing, layers and workbook (an effect)
local function build_report(dwg, wb, meta, layers, file)
	local tt = T.totals(wb, meta)
	local scale = T.unit_scale(dwg:info())
	local doc, info = Rep.build({ dwg = dwg, layers = as_layers(layers), totals = tt, roles = roles_of(dwg), unit_scale = scale, file = file })
	report_count += 1
	local m = {
		book = tostring(wb),
		dwg = tostring(dwg),
		layers = layers_key(layers),
		n = report_count,
		paths = info.paths,
		pages = info.pages,
		notes = info.notes,
		scale = info.scale,
		total = tt and tt.total,
		at = clock_text(),
	}
	return doc, m, tt
end

local doc0, rmeta0 = build_report(sample_dwg, wb0, meta0, {}, SAMPLE)
Report = page.input("report", doc0)
ReportMeta = page.input("report_meta", rmeta0)

-- is the report still what the page shows?
ReportState = page.derive("report_status", { Report, ReportMeta, Book, Dwg, Layers }, function(doc, m, wb, dwg, layers)
	if doc == nil or type(m) ~= "table" then
		return { ready = false, stale = true, reasons = { "尚未產生 Not made yet" } }
	end
	local reasons = {}
	if m.book ~= tostring(wb) then
		table.insert(reasons, "單價或數量已變更 Prices or quantities changed")
	end
	if m.dwg ~= tostring(dwg) then
		table.insert(reasons, "圖面已變更 Drawing changed")
	end
	if m.layers ~= layers_key(layers) then
		table.insert(reasons, "圖層顯示已變更 Layers changed")
	end
	return {
		ready = true,
		stale = #reasons > 0,
		reasons = reasons,
		n = m.n,
		paths = m.paths,
		pages = m.pages,
		notes = m.notes,
		scale = m.scale,
		total = m.total,
		at = m.at,
	}
end)

-- the report page shown (a long takeoff continues on detail pages)
ReportPage = page.input("report_page", 1)

local function report_pages(doc)
	local ok, n = pcall(pdf.page_count, doc)
	return if ok and type(n) == "number" then n else 1
end

local function shown_page(doc, pg)
	return clamp(if finite(pg) then math.floor(pg) else 1, 1, report_pages(doc))
end

-- no `rect`: each tiled layer shows the page filling its own rect (both
-- have the page's aspect)
local ReportParams = page.derive("report.params", { Report, ReportPage }, function(doc, pg)
	return { doc = doc, page = shown_page(doc, pg), background = 0xFFFFFFFF }
end)
page.tiled({ rect = PDFBOX, source = "pdf.page", params = ReportParams })
page.tiled({ rect = NPDF, source = "pdf.page", params = ReportParams })

-- ---------------------------------------------------------------- status and saving

local function status(s)
	page.set(Status, s)
end

local function saved(kind)
	return function(r)
		if r.saved then
			status("已儲存 " .. kind .. "：" .. r.saved)
		elseif r.error then
			status("無法儲存 " .. kind .. "：" .. r.error)
		end
	end
end

-- The XLSX export follows the workbook off the page's thread; `xlsx_book`
-- is the workbook version the last finished export was made from.
local xlsx_book = nil
local xlsx_pending = false
local XLSX_NAME = "studio-takeoff.xlsx"

Xlsx = page.job("xlsx", "sheet.export", { Book }, {
	format = "xlsx",
	eager = true,
	done = function(v)
		if type(v) == "buffer" then
			xlsx_book = tostring(page.get(Book))
			if xlsx_pending then
				xlsx_pending = false
				page.save_file({ name = XLSX_NAME, data = v, done = saved("XLSX") })
			end
		elseif type(v) == "table" and v.state == "error" then
			xlsx_pending = false
			status("XLSX 匯出失敗：" .. tostring(v.message))
		end
	end,
})

-- ---------------------------------------------------------------- operations (handlers only)

local function measure_note(q)
	if q.capped then
		return string.format("（只計前 %d 個房間）", q.capped)
	end
	return ""
end

-- Measure the drawing and rewrite the takeoff sheet (prices are kept).
local function measure_rooms()
	local dwg = page.get(Dwg)
	local q = T.measure(dwg)
	if q.loading then
		error("the drawing is still loading", 0)
	end
	local wb, meta = T.write(page.get(Book), q)
	page.set(Book, wb)
	page.set(Meta, meta)
	local rooms = {}
	for _, r in q.rooms do
		table.insert(rooms, { name = r.name, area = T.round(r.area, 2), finish = r.finish, id = r.id })
	end
	status(string.format("量測完成：%d 個房間%s、外牆 %.2f m、隔間 %.2f m、門 %d、窗 %d", #q.rooms, measure_note(q), q.ext.length, q.int.length, #q.doors.ids, #q.windows.ids))
	return {
		rooms = rooms,
		capped = q.capped,
		exterior_walls_m = T.round(q.ext.length, 2),
		partitions_m = T.round(q.int.length, 2),
		doors = #q.doors.ids,
		windows = #q.windows.ids,
		unit = q.unit,
		total = wb:get("Takeoff!F" .. meta.total),
	}
end

local function make_report()
	local t0 = os.clock()
	local doc, m, tt = build_report(page.get(Dwg), page.get(Book), page.get(Meta), page.get(Layers), page.get(FileName))
	page.set(Report, doc)
	page.set(ReportMeta, m)
	local ms = (os.clock() - t0) * 1000
	local notes = if #m.notes > 0 then " · " .. table.concat(m.notes, " · ") else ""
	status(string.format("報告第 %d 版：%d 頁、%d 條向量路徑，總計 NT$ %s%s", m.n, m.pages, m.paths, T.money(tt and tt.total), notes))
	return { pages = m.pages, paths = m.paths, total = tt and tt.total, version = m.n, notes = m.notes, ms = T.round(ms, 1) }
end

local function report_stale()
	local m = page.get(ReportMeta)
	return page.get(Report) == nil
		or type(m) ~= "table"
		or m.book ~= tostring(page.get(Book))
		or m.dwg ~= tostring(page.get(Dwg))
		or m.layers ~= layers_key(page.get(Layers))
end

-- `raw`: the price as stored (the UI passes the tax rate as a fraction);
-- `unit`: how an agent's tax rate is meant ("%" or "fraction")
local function set_price(item, price, raw, unit)
	local wb, key, n = T.set_price(page.get(Book), item, price, raw, unit)
	page.set(Book, wb)
	local meta = page.get(Meta)
	return { item = key, price = n, subtotal = wb:get("Takeoff!F" .. meta.sub), total = wb:get("Takeoff!F" .. meta.total) }
end

local function save(format)
	format = string.lower(tostring(format or "pdf"))
	if format == "pdf" then
		if report_stale() then
			make_report()
		end
		local bytes = page.get(Report):save()
		page.save_file({ name = "studio-report.pdf", data = bytes, done = saved("PDF") })
		return { format = "pdf", name = "studio-report.pdf", bytes = #bytes }
	elseif format == "xlsx" then
		if xlsx_book == tostring(page.get(Book)) then
			local v = page.peek(Xlsx)
			if type(v) == "buffer" then
				page.save_file({ name = XLSX_NAME, data = v, done = saved("XLSX") })
				return { format = "xlsx", name = XLSX_NAME, bytes = buffer.len(v) }
			end
		end
		-- the export of this version is still running: save when it is done
		xlsx_pending = true
		status("XLSX 匯出中…")
		return { format = "xlsx", name = XLSX_NAME, pending = true }
	elseif format == "dxf" then
		local bytes = page.get(Dwg):save()
		local name = page.get(FileName)
		if string.lower(string.sub(name, -4)) ~= ".dxf" then
			name ..= ".dxf"
		end
		page.save_file({ name = name, data = bytes, done = saved("DXF") })
		return { format = "dxf", name = name, bytes = buffer.len(bytes) }
	end
	error("format must be pdf, xlsx or dxf, got " .. format, 0)
end

-- DXF bytes from a dialog or a drop: the job cad.load fills an empty
-- drawing off the page's thread; when it is done the takeoff and the report
-- are redone (on_loaded)
-- the drawing shown before a file was opened, back on screen if it fails
local previous = nil

local function open_bytes(name, data)
	local target = cad.new({ pending = true })
	if previous == nil then
		previous = { dwg = page.get(Dwg), name = page.get(FileName), layers = page.get(Layers), view = page.get(View) }
	end
	page.set(Opening, { bytes = data, into = target, name = name })
	page.set(Dwg, target)
	page.set(FileName, name)
	page.set(Sel, {})
	page.set(Layers, {})
	page.set(View, nil)
	status("開啟中 " .. name .. " …")
end

local function open_failed(name, msg)
	if previous then
		page.set(Dwg, previous.dwg)
		page.set(FileName, previous.name)
		page.set(Layers, previous.layers)
		page.set(View, previous.view)
		previous = nil
	end
	page.set(Opening, nil)
	status("無法開啟 " .. tostring(name) .. "：" .. tostring(msg))
end

local function on_loaded(v)
	local opening = page.get(Opening)
	local name = if type(opening) == "table" then opening.name else ""
	if type(v) == "userdata" then
		local info = v:info()
		if info.load_error then
			open_failed(name, info.load_error)
			return
		end
		previous = nil
		page.set(Dwg, v)
		-- the drawing has its content now; let go of the file's bytes
		page.set(Opening, nil)
		-- two steps, each reported on its own when it fails
		local ok, err = pcall(measure_rooms)
		if not ok then
			status("已開啟 " .. tostring(name) .. "，但量測失敗：" .. tostring(err))
			return
		end
		local measured = page.get(Status)
		ok, err = pcall(make_report)
		if not ok then
			status(tostring(measured) .. " · 報告失敗：" .. tostring(err) .. "（可按「產生報告」重試）")
		end
	elseif type(v) == "table" and v.state == "error" then
		open_failed(name, v.message)
	end
end

Loaded = page.job("loaded", "cad.load", { Opening }, { eager = true, done = on_loaded })

local open_part -- defined with the 3D part below

local function open_dialog()
	page.open_file({ filters = { "dxf", "step", "stp" }, done = function(f)
		if f.data == nil then
			return
		end
		local ext = string.lower(string.match(f.name, "%.(%w+)$") or "")
		if ext == "dxf" then
			open_bytes(f.name, f.data)
		elseif ext == "step" or ext == "stp" then
			open_part(f.name, f.data)
		end
	end })
end

local function select_ids(ids)
	page.set(Sel, ids or {})
end

local function set_layer(name, on)
	local o = as_layers(page.get(Layers))
	o[name] = on
	page.set(Layers, o)
end

local function load_state(loaded)
	if type(loaded) == "table" and loaded.state then
		return loaded.state
	end
	return "ready"
end

-- ---------------------------------------------------------------- the plan (tiles + overlay)

local function current_view()
	return as_rect(page.get(View)) or fit_view(page.get(Dwg))
end

-- drawing units per page unit in plan area `rect`
local function units_per_px(view, rect)
	return math.max(view[3] / rect[3], view[4] / rect[4])
end

local CadParams = page.derive("cad.params", { Dwg, View, Layers, Sel, Loaded }, function(dwg, view, layers, sel, loaded)
	return {
		drawing = dwg,
		rect = as_rect(view) or fit_view(dwg),
		layers = as_layers(layers),
		selected = as_ids(sel),
		background = CAD_BG,
		highlight = HILITE,
		-- a new value when a load finishes, so tiles redraw the filled drawing
		loaded = load_state(loaded),
	}
end)
-- the view is fitted into each plan rect (uniformly, centred)
for _, r in PLANS do
	page.tiled({ rect = r, source = "cad.drawing", params = CadParams })
end

-- the row of the takeoff that holds entity `id`
local function row_of_id(tt, id)
	if tt == nil or id == nil then
		return nil
	end
	for i, r in tt.rows do
		for _, x in r.ids or {} do
			if x == id then
				return i, r
			end
		end
	end
	return nil
end

local function overlaps(a, b)
	return a[1] < b[1] + b[3] and b[1] < a[1] + a[3] and a[2] < b[2] + b[4] and b[2] < a[2] + a[4]
end

local CHIP_H, CHIP_SMALL_H = 34, 18

-- over the plan in `rect`: each room's measured area and cost, the frame,
-- hints (`hints`, when there is room for them)
local function plan_overlay(rect, name, hints)
	page.panel({
		rect = rect,
		name = name,
		inputs = { CadParams, Totals, Meta, Sel, Loaded, FileName },
		draw = function(d, params, tt, meta, sel, loaded, file)
			local view = params.rect
			local selected_row = row_of_id(tt, as_ids(sel)[1])
			-- chips only for the drawing the takeoff was measured from (not
			-- while another one loads)
			if tt and type(meta) == "table" and meta.dwg == tostring(params.drawing) then
				-- drawing units -> panel coordinates (the view's fit is affine)
				local a = cad.to_page(rect, view, view[1], view[2])
				local b = cad.to_page(rect, view, view[1] + view[3], view[2] + view[4])
				local sx, sy = (b.x - a.x) / view[3], (b.y - a.y) / view[4]
				local ox, oy = a.x - rect[1], a.y - rect[2]
				local function box(x0, y0, x1, y1)
					local px0, px1 = ox + (x0 - view[1]) * sx, ox + (x1 - view[1]) * sx
					local py0, py1 = oy + (y0 - view[2]) * sy, oy + (y1 - view[2]) * sy
					return { math.min(px0, px1), math.min(py0, py1), math.abs(px1 - px0), math.abs(py1 - py0) }
				end
				local screen = { 0, 0, d.w, d.h }
				local placed = {}
				for i, r in tt.rows do
					local m = meta.rows[i]
					if r.kind == "room" and m and m.points and #m.points >= 2 then
						local x0, y0, x1, y1 = math.huge, math.huge, -math.huge, -math.huge
						for k = 1, #m.points - 1, 2 do
							x0, x1 = math.min(x0, m.points[k]), math.max(x1, m.points[k])
							y0, y1 = math.min(y0, m.points[k + 1]), math.max(y1, m.points[k + 1])
						end
						local room = box(x0, y0, x1, y1)
						local s1 = string.format("%.2f m²", r.qty or 0)
						local s2 = "NT$ " .. T.money(r.amount)
						-- a full chip, a one-line chip, or none when the room is
						-- smaller on screen than the chip
						local w, h, big = math.max(page.measure(s1, 12), page.measure(s2, 11)) + 12, CHIP_H, true
						if room[3] < w + 12 or room[4] < h + 12 then
							w, h, big = page.measure(s1, 11) + 10, CHIP_SMALL_H, false
						end
						if overlaps(room, screen) and room[3] >= w + 8 and room[4] >= h + 8 then
							-- the room's names and earlier chips stay readable: try the
							-- corners, bottom-left first
							local blocked = table.clone(placed)
							for _, lb in m.labels or {} do
								table.insert(blocked, box(lb[1], lb[2], lb[1] + lb[3], lb[2] + lb[4]))
							end
							local l, t, rr, bb = room[1] + 6, room[2] + 6, room[1] + room[3] - w - 6, room[2] + room[4] - h - 6
							local spot = nil
							for _, c in { { l, bb }, { l, t }, { rr, bb }, { rr, t } } do
								local cand = { c[1], c[2], w, h }
								local free = true
								for _, bk in blocked do
									if overlaps(cand, bk) then
										free = false
										break
									end
								end
								if free then
									spot = cand
									break
								end
							end
							spot = spot or { l, bb, w, h }
							table.insert(placed, spot)
							local px, py = spot[1], spot[2]
							if px > -w and px < d.w and py > -h and py < d.h then
								local on = selected_row == i
								d:rrect(px, py, w, h, 6, if on then alpha(HILITE, 235) else alpha(TH.surface3, 225))
								local c = if on then rgba(20, 20, 20) else TEXT_HI
								if big then
									d:text(px + 6, py + 2, 12, c, s1)
									d:text(px + 6, py + 17, 11, if on then c else TEXT_LO, s2)
								else
									d:text(px + 5, py + 2, 11, c, s1)
								end
							end
						end
					end
				end
			end
			d:rrect(0, 0, d.w, 28, 0, alpha(TH.bg, 240))
			d:text(10, 6, 13, TEXT_HI, "平面圖 Plan · " .. tostring(file))
			d:text(d.w - page.measure(hints, 12) - 10, 7, 12, TEXT_LO, hints)
			local state = load_state(loaded)
			if state == "pending" then
				d:rrect(d.w / 2 - 160, d.h / 2 - 22, 320, 44, 10, alpha(TH.surface, 235))
				d:text(d.w / 2 - 140, d.h / 2 - 10, 15, TEXT_HI, string.format("開啟中 %s … %d%%", tostring(file), math.floor((loaded.progress or 0) * 100)))
			elseif state == "error" then
				d:rrect(20, d.h - 60, d.w - 40, 40, 8, alpha(TH.danger, 220))
				d:text(32, d.h - 50, 14, TEXT_HI, "無法開啟：" .. tostring(loaded.message))
			end
			d:stroke(0.5, 0.5, d.w - 1, d.h - 1, 0, 1, TH.border)
		end,
	})
end

plan_overlay(CAD, "plan.overlay", "滾輪縮放 · 拖曳平移 · 點選選取 · F 適合")
plan_overlay(NCAD, "plan_n.overlay", "拖曳平移 · 點選選取")

-- ---------------------------------------------------------------- the STEP part (optional)

-- scene is an optional module and STEP import an optional cad feature: a
-- platform without them shows a note in this corner instead
local HAS_3D = scene ~= nil and cad.step_to_mesh ~= nil
local PartBytes, PartName, PartMesh, PartCam
-- a part the user opened: its mesh job reports in the status line (the
-- bundled one loads quietly)
local part_opened = false

open_part = function(name, data)
	if not HAS_3D then
		status("這個平台沒有 3D 檢視（scene 模組或 STEP 匯入）")
		return
	end
	part_opened = true
	page.set(PartName, name)
	page.set(PartBytes, data)
	status("載入 STEP " .. name .. " …")
end

-- the part's camera, kept sane whatever the cell holds
local function part_cam(c)
	c = if type(c) == "table" then c else {}
	return {
		yaw = if finite(c.yaw) then c.yaw % 360 else 35,
		pitch = if finite(c.pitch) then clamp(c.pitch, -85, 85) else 25,
		zoom = if finite(c.zoom) then clamp(c.zoom, 0.2, 5) else 1,
	}
end

if HAS_3D then
	PartBytes = page.input("part_bytes", page.asset("bracket.step"))
	PartName = page.input("part_name", "bracket.step")
	PartMesh = page.job("part_mesh", "cad.step_mesh", { PartBytes }, {
		eager = true,
		done = function(m)
			if not part_opened then
				return
			end
			local name = tostring(page.get(PartName))
			if type(m) == "table" and m.state == "error" then
				status("無法讀取 STEP " .. name .. "：" .. tostring(m.message))
			elseif type(m) == "table" and type(m.triangles) == "number" then
				local failed = if type(m.failed) == "table" then #m.failed else 0
				status(string.format("STEP %s：%d 三角形 · %d shell%s", name, m.triangles, m.shells or 0,
					if failed > 0 then string.format("（%d 個 shell 無法轉換）", failed) else ""))
			end
		end,
	})
	PartCam = page.input("part_camera", { yaw = 35, pitch = 25, zoom = 1 })
	local PartModel = page.derive("part_model", { PartMesh }, function(m)
		if type(m) == "table" and m.positions ~= nil then
			return scene.mesh(m)
		end
		return nil
	end)
	local PartScene = page.derive("part_scene", { PartModel }, function(model)
		return {
			id = "studio-part",
			items = if model then { { model = model, color = rgba(190, 198, 214) } } else {},
			background = PART_BG,
			grid = { color = rgba(120, 130, 150, 70) },
		}
	end)
	local PartCamera = page.derive("part_camera.view", { PartCam, PartScene }, function(cell, s)
		local c = part_cam(cell)
		local cam = scene.camera({ yaw = c.yaw, pitch = c.pitch })
		if #s.items == 0 then
			return cam
		end
		cam = scene.fit(cam, s, { aspect = PART[3] / PART[4], margin = 1.2 })
		return scene.zoom(cam, c.zoom)
	end)
	page.layer({ rect = PART, kind = "scene.view", inputs = { PartScene, PartCamera }, name = "part3d" })
	page.panel({
		rect = PART_HEAD,
		name = "part.head",
		inputs = { PartName, PartMesh },
		draw = function(d, name, m)
			d:rrect(0, 0, d.w, d.h, 0, TH.surface)
			d:text(12, 4, 14, TEXT_HI, "3D 零件 STEP · " .. tostring(name))
			local info
			if type(m) == "table" and m.state == "pending" then
				info = string.format("轉換網格中… %d%%", math.floor((m.progress or 0) * 100))
			elseif type(m) == "table" and m.state == "error" then
				info = "無法讀取：" .. tostring(m.message)
			elseif type(m) == "table" then
				info = string.format("%d 三角形 · %d shell · 拖曳旋轉 · 滾輪縮放", m.triangles or 0, m.shells or 0)
			else
				info = ""
			end
			d:text(12, 22, 11, TEXT_LO, info)
		end,
	})
else
	page.panel({
		rect = R(PART_HEAD[1], PART_HEAD[2], PART_HEAD[3], PART_HEAD[4] + PART[4]),
		name = "part.missing",
		draw = function(d)
			d:rrect(0, 0, d.w, d.h, 8, TH.surface)
			d:text(14, 12, 15, TEXT_HI, "3D 零件 STEP")
			d:textbox(14, 44, d.w - 28, d.h - 60, 13, TEXT_LO,
				"這個 app 或平台沒有 3D 檢視（scene 模組）或 STEP 匯入，本區略過。\n" ..
				"CAD、試算表與 PDF 不受影響。")
		end,
	})
end

-- ---------------------------------------------------------------- pointer input
--
-- Registered before the UI apps, so these run first: a press on the plan
-- or the part is the page's (the UI library would otherwise keep a stale
-- capture after a cancelled press and swallow the next gesture's events).

local drag = nil

-- a click at page point (x, y) on the plan shown in `rect`
local function click_plan(rect, x, y)
	local dwg = page.get(Dwg)
	local view = current_view()
	local q = cad.to_drawing(rect, view, x, y)
	local ok, id = pcall(dwg.pick, dwg, q.x, q.y, 6 * units_per_px(view, rect), { layers = as_layers(page.get(Layers)) })
	if not ok then
		id = nil
	end
	if id == nil then
		-- inside a room: the room
		for _, r in page.get(Meta).rows do
			if r.kind == "room" and r.points and T.inside(r.points, q.x, q.y) then
				id = r.ids[1]
				break
			end
		end
	end
	if id then
		select_ids({ id })
		local e = dwg:entity(id)
		status(string.format("選取 %s #%d（%s）", e and e.type or "?", id, e and e.layer or ""))
	else
		select_ids({})
	end
end

page.on("pointer_down", function(x, y)
	local kind = nil
	local plan = plan_at(x, y)
	if plan then
		kind = "plan"
		drag = { kind = "plan", rect = plan, x0 = x, y0 = y, view = current_view(), moved = false }
	elseif HAS_3D and contains(PART, x, y) then
		kind = "orbit"
		drag = { kind = "orbit", x = x, y = y }
	end
	if kind == nil then
		return nil
	end
	-- what the UI library does for a press outside its apps: they let go of focus
	for _, a in APPS do
		a:blur()
	end
	return "captured"
end)

page.on("pointer_drag", function(x, y)
	local d = drag
	if d == nil then
		return false
	end
	if d.kind == "plan" then
		if not d.moved and math.abs(x - d.x0) + math.abs(y - d.y0) < 5 then
			return true
		end
		d.moved = true
		local v = d.view
		local s = units_per_px(v, d.rect)
		page.set(View, clamp_view(page.get(Dwg), { v[1] - (x - d.x0) * s, v[2] + (y - d.y0) * s, v[3], v[4] }))
	elseif d.kind == "orbit" then
		local c = part_cam(page.get(PartCam))
		c.yaw = (c.yaw + (x - d.x) * 0.5) % 360
		c.pitch = clamp(c.pitch + (y - d.y) * 0.4, -85, 85)
		d.x, d.y = x, y
		page.set(PartCam, c)
	end
	return true
end)

page.on("pointer_up", function()
	local d = drag
	drag = nil
	if d == nil then
		return false
	end
	if d.kind == "plan" and not d.moved then
		click_plan(d.rect, d.x0, d.y0)
	end
	return true
end)

-- The gesture was taken away (a second finger, the OS): no click. A press
-- held in a UI app is let go of here too, since the UI library listens only
-- for pointer_up, which would activate the pressed control (NOTES.md).
page.on("pointer_cancel", function()
	local d = drag
	drag = nil
	if d ~= nil then
		return true
	end
	for _, a in APPS do
		a._drag = nil
		local u = a:ui_get()
		if u.press ~= nil then
			u.press = nil
			a:ui_set(u)
		end
	end
	return true
end)

-- ---------------------------------------------------------------- UI: top bar

local function try(app, f, ...)
	local ok, err = pcall(f, ...)
	if not ok then
		app:toast(tostring(err), { tone = "danger" })
		return nil
	end
	return err
end

-- the status line: the last message, or the open's progress
local function status_line(st, loaded, file, hint)
	local msg, tone = tostring(st or ""), "dim"
	if load_state(loaded) == "pending" then
		msg, tone = string.format("開啟中 %s … %d%%", tostring(file), math.floor((loaded.progress or 0) * 100)), "accent"
	elseif msg == "" then
		msg = hint
	end
	return msg, tone
end

table.insert(APPS, ui.app({
	name = "bar",
	rect = BAR,
	theme = TH,
	inputs = { Status, Loaded, FileName, ReportState },
	view = function(s, st, loaded, file, rs)
		local msg, tone = status_line(st, loaded, file, "拖放 DXF 或 STEP 到視窗即可開啟 · 單價可直接修改，總計即時重算")
		return ui.row({ padding = 10, gap = 8 }, {
			ui.column({ gap = 0, width = 270 }, {
				ui.text("AnyPage Studio 工作室", { size = 19 }),
				ui.text("CAD × 試算表 × PDF，同一份資料", { size = 12, tone = "dim" }),
			}),
			ui.button("開啟 Open", { id = "open", variant = "secondary", tooltip = "開啟 DXF 平面圖或 STEP 零件；也可以拖放檔案", on_click = function()
				open_dialog()
			end }),
			ui.button("量測 Measure", { id = "measure", variant = "secondary", tooltip = "從圖面重新量測房間面積與牆長，寫入試算表", on_click = function(app)
				try(app, measure_rooms)
			end }),
			ui.button("產生報告 Report", { id = "report", variant = if rs and rs.stale then "primary" else "secondary", on_click = function(app)
				try(app, make_report)
			end }),
			ui.divider(),
			ui.button("存 DXF", { id = "save_dxf", variant = "ghost", on_click = function(app)
				try(app, save, "dxf")
			end }),
			ui.button("存 XLSX", { id = "save_xlsx", variant = "ghost", on_click = function(app)
				try(app, save, "xlsx")
			end }),
			ui.button("存 PDF", { id = "save_pdf", variant = "ghost", on_click = function(app)
				try(app, save, "pdf")
			end }),
			ui.spacer(12),
			ui.text(msg, { size = 13, tone = tone, grow = 1, wrap = false }),
		})
	end,
}))

-- ---------------------------------------------------------------- UI: the narrow screens' headers

-- what the page shows: "wide" or a narrow screen ("plan", "takeoff", "report")
Layout = page.input("layout", "wide")
-- the window: { w, h, scale, portrait } in logical pixels (and the
-- "viewport" event, below)
Viewport = page.viewport()

-- What to fit for arrangement `mode`: a narrow screen in a window taller
-- than it stays at the top (the rest of the window is empty below it).
local function fit_of(mode)
	local r = SCREENS[mode]
	local v = page.get(Viewport)
	if mode ~= "wide" and type(v) == "table" and finite(v.w) and finite(v.h) and v.w > 96 and v.h > 96 then
		-- the engine fits with a 24-pixel margin
		local aspect = (v.h - 48) / (v.w - 48)
		if aspect > r[4] / r[3] then
			return R(r[1], r[2], r[3], r[3] * aspect)
		end
	end
	return r
end

-- Show arrangement `mode` and fit the window to it.
local function show(mode)
	if type(mode) ~= "string" or SCREENS[mode] == nil then
		error("layout must be wide, plan, takeoff or report, got " .. tostring(mode), 0)
	end
	page.set(Layout, mode)
	page.fit_rect(fit_of(mode))
end

-- each screen's own buttons, under the tabs
local function screen_tools(k, rs)
	local function b(label, id, variant, f, ...)
		local args = table.pack(...)
		return ui.button(label, { id = id, size = "small", variant = variant, on_click = function(app)
			try(app, f, table.unpack(args, 1, args.n))
		end })
	end
	if k == "plan" then
		return { b("開啟 Open", "open", "secondary", open_dialog), b("量測 Measure", "measure", "secondary", measure_rooms), b("存 DXF", "save_dxf", "ghost", save, "dxf") }
	elseif k == "takeoff" then
		return { b("量測 Measure", "measure", "secondary", measure_rooms), b("存 XLSX", "save_xlsx", "ghost", save, "xlsx") }
	end
	return { b("產生報告 Report", "report", if rs and rs.stale then "primary" else "secondary", make_report), b("存 PDF", "save_pdf", "ghost", save, "pdf") }
end

for i, k in SCREEN_ORDER do
	table.insert(APPS, ui.app({
		name = "head_" .. k,
		rect = R(SCREENS[k][1], SCREENS[k][2], NW, HEAD_H),
		theme = TH,
		inputs = { Status, Loaded, FileName, ReportState },
		view = function(s, st, loaded, file, rs)
			local msg, tone = status_line(st, loaded, file, "拖放或開啟 DXF 平面圖 · 單價可直接修改")
			local tabs = table.clone(SCREEN_LABELS)
			if rs and rs.stale then
				tabs[3] ..= " •"
			end
			return ui.column({ padding = 12, gap = 6 }, {
				ui.text("AnyPage Studio 工作室", { size = 17, wrap = false }),
				ui.tabs({ id = "screen", tabs = tabs, selected = i, on_change = function(j)
					show(SCREEN_ORDER[j])
				end }),
				ui.row({ gap = 8 }, screen_tools(k, rs)),
				ui.text(msg, { size = 12, tone = tone, wrap = false }),
			})
		end,
	}))
end

-- ---------------------------------------------------------------- UI: takeoff

-- the table's columns: wide, and narrow (no unit price)
local COLUMNS = { "項目 Item", "數量", "單位", "單價", "金額 NT$" }
local COLUMNS_N = { "項目 Item", "數量", "單位", "金額 NT$" }
-- the width a stat card gives its number (three cards share the column;
-- the narrow screen's cards share its whole width), so that three cards
-- with numbers this wide still fit side by side
local STAT_W, STAT_W_N = 118, 98

-- The font size, at most `base`, at which `s` fits in `max_w`: a long
-- number gets a smaller font, never a line break.
local function fit_size(s, base, max_w)
	local w = page.measure(s, base)
	return if w > max_w then math.max(9, math.floor(base * max_w / w)) else base
end

local function stat(label, v, big, max_w)
	local s = "NT$ " .. T.money(v)
	local size = fit_size(s, if big then 22 else 17, max_w)
	return ui.card({ padding = 10, gap = 2, grow = 1, bg = if big then TH.surface3 else nil }, {
		ui.text(label, { size = 12, tone = "dim", wrap = false }),
		ui.text(s, { id = "stat." .. label, size = size, tone = if big then "warning" else nil, wrap = false }),
	})
end

-- what a price field shows: the user's draft while it is being typed
-- (and still means the stored price), else the stored price
local function price_shown(drafts, key, v)
	local stored = T.price_text(key, v)
	local d = drafts[key]
	if type(d) == "string" and (tonumber(d) == nil or tonumber(d) == tonumber(stored)) then
		return d
	end
	return stored
end

local function drafts_of(st)
	return if type(st) == "table" and type(st.drafts) == "table" then st.drafts else {}
end

local function edit_price(app, key, text)
	app:update(function(st)
		st.drafts = drafts_of(st)
		st.drafts[key] = text
	end)
	local n = tonumber(text)
	if n == nil then
		return
	end
	if key == "tax" then
		n = n / 100
	end
	try(app, set_price, key, n, true)
end

local function clear_draft(app, key)
	app:update(function(st)
		if type(st.drafts) == "table" then
			st.drafts[key] = nil
		end
	end)
end

-- The takeoff app's view: prices next to the table (wide), or above it
-- (narrow, a column of the phone screen's width).
local function takeoff_view(narrow)
	return function(s, tt, sel, meta)
		if tt == nil or type(meta) ~= "table" then
			return ui.column({ padding = 16 }, { ui.text("尚無數量資料", { tone = "dim" }) })
		end
		local drafts = drafts_of(s)
		local fields = {}
		for _, p in T.PRICES do
			table.insert(fields, ui.row({ gap = 6 }, {
				ui.text(p.label, { size = 13, grow = 1, wrap = false }),
				ui.textfield({
					id = "price." .. p.key,
					value = price_shown(drafts, p.key, tt.prices[p.key]),
					width = 100,
					height = 28,
					size = 14,
					max_length = 16,
					on_change = function(v, app)
						edit_price(app, p.key, v)
					end,
					on_submit = function(v, app)
						clear_draft(app, p.key)
					end,
				}),
				ui.text(if p.key == "tax" then "%" else p.unit, { size = 12, tone = "dim", width = 24 }),
			}))
		end
		local rows = {}
		for i, r in tt.rows do
			if narrow then
				rows[i] = { r.name or "", T.qty(r.qty, r.kind), r.unit or "", T.money(r.amount) }
			else
				rows[i] = { r.name or "", T.qty(r.qty, r.kind), r.unit or "", T.money(r.price), T.money(r.amount) }
			end
		end
		local selected = row_of_id(tt, as_ids(sel)[1])
		local rate = if type(tt.rate) == "number" then T.plain(T.round(tt.rate * 100, 2)) .. "%" else ""
		local nrooms = 0
		for _, r in tt.rows do
			if r.kind == "room" then
				nrooms += 1
			end
		end
		local notes = {}
		if nrooms == 0 then
			table.insert(notes, ui.text("找不到房間：房間要畫成封閉聚合線，放在名稱含 ROOM / AREA 的圖層。", { size = 12, tone = "warning" }))
		end
		if meta.capped then
			table.insert(notes, ui.text(string.format("圖面的房間超過 %d 個，只計前 %d 個。", meta.capped, meta.capped), { size = 12, tone = "warning" }))
		end
		for _, e in tt.errors do
			table.insert(notes, ui.text(e, { size = 12, tone = "danger" }))
		end
		local shown_rows = if narrow then 7 else 10
		local tbl = ui.table({
			id = "takeoff",
			data = { columns = if narrow then COLUMNS_N else COLUMNS, rows = rows },
			widths = if narrow then { 0.42, 0.19, 0.14 } else { 0.32, 0.15, 0.11, 0.17 },
			row_height = 27,
			header_height = 32,
			height = 32 + math.min(#rows, shown_rows) * 27 + 2,
			align_cols = if narrow then { nil, "end", nil, "end" } else { nil, "end", nil, "end", "end" },
			selected = selected,
			on_select = function(row, i)
				local m = page.get(Meta).rows[i]
				if m then
					select_ids(m.ids)
				end
			end,
		})
		local stat_w = if narrow then STAT_W_N else STAT_W
		local stats = ui.row({ gap = 10 }, {
			stat("小計 Subtotal", tt.subtotal, false, stat_w),
			stat("營業稅 Tax " .. rate, tt.tax, false, stat_w),
			stat("總計 Total", tt.total, true, stat_w),
		})
		local badge = ui.badge(string.format("%d 房間 · 單位 %s", nrooms, tostring(meta.unit or "?")), { tone = "neutral" })
		local prices = ui.card({ width = if narrow then nil else 282, padding = 12, gap = 4 }, {
			ui.text("單價 Unit prices（可編輯）", { size = 14 }),
			ui.column({ gap = 4 }, fields),
		})
		if narrow then
			return ui.column({ padding = 14, gap = 10 }, {
				ui.row({ gap = 10 }, { ui.text("數量計算 Takeoff", { size = 17 }), ui.spacer(), badge }),
				prices,
				tbl,
				stats,
				table.unpack(notes),
			})
		end
		return ui.column({ padding = 14, gap = 10 }, {
			ui.row({ gap = 10 }, {
				ui.text("數量計算 Quantity takeoff", { size = 18 }),
				badge,
				ui.spacer(),
				ui.text("工作表 Takeoff + Prices · ROUND / SUM / 稅", { size = 12, tone = "dim" }),
			}),
			ui.row({ gap = 12, align = "start", grow = 1 }, {
				prices,
				ui.column({ grow = 1, gap = 10 }, {
					tbl,
					stats,
					table.unpack(notes),
				}),
			}),
		})
	end
end

table.insert(APPS, ui.app({
	name = "takeoff",
	rect = TAKE,
	theme = TH,
	agent = true,
	state = { drafts = {} },
	inputs = { Totals, Sel, Meta },
	view = takeoff_view(false),
}))

-- the narrow screen's copy (its own drafts; agents use the wide one's port)
table.insert(APPS, ui.app({
	name = "takeoff_n",
	rect = NTAKE,
	theme = TH,
	state = { drafts = {} },
	inputs = { Totals, Sel, Meta },
	view = takeoff_view(true),
}))

-- ---------------------------------------------------------------- UI: layers and selection

local function selection_view(dwg, sel, tt)
	if #sel == 0 then
		return ui.text("點選圖面上的圖元，或點選數量表的一列。", { size = 13, tone = "dim" })
	end
	local okE, e = pcall(dwg.entity, dwg, sel[1])
	local okM, m = pcall(dwg.measure, dwg, sel)
	local s = T.unit_scale(dwg:info())
	local lines = {}
	if #sel == 1 and okE and e then
		table.insert(lines, ui.text(string.format("%s #%d · 圖層 %s", tostring(e.type), sel[1], tostring(e.layer)), { size = 13 }))
	else
		table.insert(lines, ui.text(string.format("%d 個圖元", #sel), { size = 13 }))
	end
	if okM and type(m) == "table" then
		table.insert(lines, ui.text(string.format("長度 %.2f m · 面積 %.2f m²", (m.length or 0) * s, (m.area or 0) * s * s), { size = 13, tone = "dim" }))
	end
	local _, row = row_of_id(tt, sel[1])
	if row then
		table.insert(lines, ui.text(string.format("數量表：%s · %s %s · NT$ %s", row.name or "", T.qty(row.qty, row.kind), row.unit or "", T.money(row.amount)), { size = 13, tone = "warning" }))
	end
	return ui.column({ gap = 4 }, lines)
end

-- the layer list scrolls in `list_h`
local function side_view(list_h)
	return function(s, dwg, layers, sel, tt)
		local overrides = as_layers(layers)
		local rows = {}
		for _, l in dwg:layers() do
			local on = overrides[l.name]
			if on == nil then
				on = l.on
			end
			table.insert(rows, ui.row({ gap = 8 }, {
				ui.canvas({ width = 12, height = 12, draw = function(d, x, y, w, h)
					d:rrect(x, y, w, h, 3, l.color)
				end }),
				ui.checkbox(l.name, { id = "layer." .. l.name, checked = on, grow = 1, on_change = function(v)
					set_layer(l.name, v == true)
				end }),
				ui.text(tostring(l.entities), { size = 12, tone = "dim" }),
			}))
		end
		return ui.column({ padding = 12, gap = 8 }, {
			ui.row({ gap = 8 }, {
				ui.text("圖層 Layers", { size = 16 }),
				ui.spacer(),
				ui.button("全部顯示", { id = "layers.all", size = "small", variant = "ghost", on_click = function()
					page.set(Layers, {})
				end }),
			}),
			ui.scroll({ id = "layers", height = list_h, padding = { 0, 14, 0, 0 } }, ui.column({ gap = 0 }, rows)),
			ui.divider(),
			ui.text("選取 Selection", { size = 16 }),
			selection_view(dwg, as_ids(sel), tt),
		})
	end
end

table.insert(APPS, ui.app({
	name = "side",
	rect = SIDE,
	theme = TH,
	inputs = { Dwg, Layers, Sel, Totals, Loaded },
	view = side_view(196),
}))

table.insert(APPS, ui.app({
	name = "side_n",
	rect = NSIDE,
	theme = TH,
	inputs = { Dwg, Layers, Sel, Totals, Loaded },
	view = side_view(110),
}))

-- ---------------------------------------------------------------- UI: report

-- the report's state, its pages, and (`buttons`) remake and save buttons
-- (the narrow screen has those in its header); `inner_w`: the width inside
-- the padding
local function report_view(buttons, inner_w)
	return function(s, rs, doc, pg)
		local badge
		if not rs.ready then
			badge = ui.badge("尚未產生", { tone = "warning" })
		elseif rs.stale then
			badge = ui.badge("需更新", { tone = "warning", solid = true })
		else
			badge = ui.badge("最新", { tone = "success" })
		end
		local kids = {
			ui.text("報告 Report", { size = 16 }),
			ui.text("A4 橫式 PDF，向量圖面", { size = 12, tone = "dim" }),
			badge,
		}
		if rs.ready then
			if rs.stale then
				for _, r in rs.reasons do
					table.insert(kids, ui.text(r, { size = 11, tone = "dim" }))
				end
			end
			table.insert(kids, ui.text(string.format("第 %d 版 · %s", rs.n or 0, rs.at or ""), { size = 12, tone = "dim" }))
			table.insert(kids, ui.text(string.format("%d 頁 · %d 條路徑", rs.pages or 1, rs.paths or 0), { size = 12, tone = "dim" }))
			table.insert(kids, ui.text(rs.scale or "", { size = 12, tone = "dim" }))
			for _, n in rs.notes or {} do
				table.insert(kids, ui.text(n, { size = 11, tone = "warning" }))
			end
			local total = "NT$ " .. T.money(rs.total)
			table.insert(kids, ui.text(total, { size = fit_size(total, 15, inner_w), wrap = false }))
		end
		table.insert(kids, ui.spacer())
		local n = if doc ~= nil then report_pages(doc) else 1
		if n > 1 then
			local cur = shown_page(doc, pg)
			table.insert(kids, ui.row({ gap = 6 }, {
				ui.button("‹", { id = "page.prev", size = "small", variant = "ghost", disabled = cur <= 1, on_click = function()
					page.set(ReportPage, cur - 1)
				end }),
				ui.text(string.format("第 %d / %d 頁", cur, n), { size = 12, grow = 1, align = "center" }),
				ui.button("›", { id = "page.next", size = "small", variant = "ghost", disabled = cur >= n, on_click = function()
					page.set(ReportPage, cur + 1)
				end }),
			}))
		end
		if buttons then
			table.insert(kids, ui.button("更新報告", { id = "make", fill = true, variant = if rs.stale then "primary" else "secondary", on_click = function(app)
				try(app, make_report)
			end }))
			table.insert(kids, ui.button("儲存 PDF", { id = "save", fill = true, variant = "secondary", on_click = function(app)
				try(app, save, "pdf")
			end }))
		end
		return ui.column({ padding = 12, gap = 8 }, kids)
	end
end

table.insert(APPS, ui.app({
	name = "report",
	rect = RINFO,
	theme = TH,
	inputs = { ReportState, Report, ReportPage },
	view = report_view(true, RINFO[3] - 24),
}))

table.insert(APPS, ui.app({
	name = "report_n",
	rect = NRINFO,
	theme = TH,
	inputs = { ReportState, Report, ReportPage },
	view = report_view(false, NRINFO[3] - 24),
}))

-- ---------------------------------------------------------------- other input

page.on("wheel", function(x, y, dx, dy)
	local plan = plan_at(x, y)
	if plan then
		local dwg = page.get(Dwg)
		local view = current_view()
		local q = cad.to_drawing(plan, view, x, y)
		-- within the zoom limits (a factor > 1 narrows the view)
		local size = drawing_size(dwg)
		local w = math.max(view[3], view[4])
		local f = clamp(1.15 ^ dy, w / (size * ZOOM_OUT), w / (size * ZOOM_IN))
		if math.abs(f - 1) > 1e-9 then
			local ok, z = pcall(cad.zoom, view, q.x, q.y, f)
			if ok then
				page.set(View, z)
			end
		end
		return true
	end
	if HAS_3D and contains(PART, x, y) then
		local c = part_cam(page.get(PartCam))
		c.zoom = clamp(c.zoom * 1.1 ^ -dy, 0.2, 5)
		page.set(PartCam, c)
		return true
	end
	return false
end)

page.on("key", function(k)
	if k == "escape" then
		select_ids({})
		return true
	elseif k == "f" then
		page.set(View, nil)
		return true
	elseif k == "r" then
		-- the engine's own "r" fits the page.json bounds: the wide layout
		local m = page.get(Layout)
		if m ~= "wide" and SCREENS[m] ~= nil then
			show(m)
			return true
		end
	end
	return false
end)

-- The window's size picks the arrangement: narrow for portrait windows
-- where the wide one would be small (phones, tablets upright, a tall
-- desktop window), else wide; either way the camera is fitted again, so a
-- resized window shows the whole layout instead of a corner of it.
local function narrow_window(w, h)
	if not (finite(w) and finite(h)) or w <= 0 or h <= 0 then
		return false
	end
	-- the engine fits with a 24-pixel margin
	local zoom = math.min((w - 48) / WIDE[3], (h - 48) / WIDE[4])
	return h > w and zoom < 0.6
end

page.on("viewport", function(w, h)
	if narrow_window(w, h) then
		local m = page.get(Layout)
		show(if m == "wide" or SCREENS[m] == nil then "plan" else m)
	else
		show("wide")
	end
	return true
end)

page.on("file_drop", function(f)
	local ext = string.lower(string.match(f.name, "%.(%w+)$") or "")
	if ext == "dxf" then
		open_bytes(f.name, f.data)
	elseif ext == "step" or ext == "stp" then
		open_part(f.name, f.data)
	else
		status("只支援 DXF 平面圖與 STEP 零件：" .. f.name)
	end
	return true
end)

page.on("control", function(name)
	if name == "fit" then
		page.set(View, nil)
		return true
	end
	return false
end)

-- ---------------------------------------------------------------- semantic port

page.action({
	name = "measure_rooms",
	description = "Measure the drawing (room areas from closed outlines on room layers, wall lengths, door and window counts) and rewrite the takeoff sheet; prices are kept",
	params = {},
	run = function()
		return measure_rooms()
	end,
})

page.action({
	name = "set_price",
	description = "Set a unit price in the Prices sheet; totals recompute. item: a key (wood, tile, ext_wall, int_wall, paint, door, window, height = wall height in m, tax) or a label naming one item (an ambiguous part is refused with the candidates). tax: 5 is 5 %, 1 is 1 %, values below 1 are a fraction (0.05 is 5 %); unit \"%\" or \"fraction\" says it explicitly",
	params = { item = "price key or label", price = "number (NT$ per unit; metres for height; the rate for tax)", unit = "tax only: \"%\" or \"fraction\" (optional)" },
	run = function(p)
		p = if type(p) == "table" then p else {}
		return set_price(p.item, p.price, false, p.unit)
	end,
})

page.action({
	name = "make_report",
	description = "Build the PDF report from the current drawing (visible layers), takeoff and prices",
	params = {},
	run = function()
		return make_report()
	end,
})

page.action({
	name = "save",
	description = "Save a file through the host's save dialog: pdf (the report, rebuilt first when out of date), xlsx (the workbook) or dxf (the drawing)",
	params = { format = "pdf | xlsx | dxf" },
	run = function(p)
		return save(if type(p) == "table" then p.format else nil)
	end,
})

page.action({
	name = "select",
	description = "Select entities of the drawing by id (the takeoff rows list their ids); an empty list clears the selection",
	params = { ids = "an entity id or a list of ids" },
	run = function(p)
		local raw = if type(p) == "table" then p.ids else nil
		local list
		if type(raw) == "number" then
			list = { raw }
		elseif type(raw) == "table" then
			list = raw
		else
			error("ids must be an entity id or a list of ids ([] clears the selection), got " .. type(raw), 0)
		end
		local dwg = page.get(Dwg)
		local ids, bad = {}, {}
		for _, x in list do
			local known = false
			if finite(x) and x >= 0 and x % 1 == 0 then
				local ok, e = pcall(dwg.entity, dwg, x)
				known = ok and e ~= nil
			end
			if known then
				table.insert(ids, x)
			else
				table.insert(bad, tostring(x))
			end
			if #ids + #bad > 10000 then
				error("at most 10000 ids", 0)
			end
		end
		if #bad > 0 then
			error("not entity ids of this drawing: " .. table.concat(bad, ", ", 1, math.min(#bad, 10)) .. (if #bad > 10 then ", …" else ""), 0)
		end
		select_ids(ids)
		return { selected = ids }
	end,
})

page.action({
	name = "layer",
	description = "Show or hide a layer of the drawing (the report prints the visible layers)",
	params = { name = "layer name", on = "boolean (default true)" },
	run = function(p)
		p = if type(p) == "table" then p else {}
		local name = p.name
		if type(name) ~= "string" then
			error("name must be a layer name", 0)
		end
		local found = false
		for _, l in page.get(Dwg):layers() do
			if l.name == name then
				found = true
				break
			end
		end
		if not found then
			error("no layer \"" .. name .. "\" in this drawing", 0)
		end
		local on = p.on
		if on == nil then
			on = true
		end
		if type(on) ~= "boolean" then
			error("on must be true or false", 0)
		end
		set_layer(name, on)
		return { name = name, on = on }
	end,
})

page.action({
	name = "layout",
	description = "Show the wide layout (every panel) or one narrow screen (plan, takeoff, report) and fit the window to it; the page picks one by itself when the window's size changes",
	params = { layout = "wide | plan | takeoff | report" },
	run = function(p)
		local m = if type(p) == "table" then p.layout else nil
		show(m)
		return { layout = m }
	end,
})

page.action({
	name = "view",
	description = "Show part of the drawing in the plan: rect = {x, y, w, h} in drawing units (y up), or fit = true for the whole drawing; kept within the zoom limits",
	params = { rect = "{x, y, w, h} (drawing units)", fit = "true: the whole drawing" },
	run = function(p)
		p = if type(p) == "table" then p else {}
		if p.fit == true then
			page.set(View, nil)
			return { fit = true }
		end
		local r = as_rect(p.rect)
		if r == nil then
			error("rect must be {x, y, w, h} with w, h > 0 (drawing units), or fit = true", 0)
		end
		r = clamp_view(page.get(Dwg), r)
		page.set(View, r)
		return { rect = r }
	end,
})
