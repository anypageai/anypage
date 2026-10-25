-- A sample floor plan (millimetres) built through the cad API: layers, a
-- block inserted several times, polylines with widths and bulges, arcs,
-- a spline, an ellipse, hatches, text in two scripts and dimensions.

return function()
	local dwg = cad.new()
	local layers = {
		{ name = "WALLS", aci = 7 },
		{ name = "DOORS", aci = 3 },
		{ name = "FURNITURE", aci = 4 },
		{ name = "HATCH", aci = 8 },
		{ name = "TEXT", aci = 2 },
		{ name = "DIM", aci = 1 },
		{ name = "GARDEN", aci = 94 },
	}
	for _, l in layers do
		dwg = cad.add_layer(dwg, l)
	end

	-- a table with four chairs, drawn around its centre
	dwg = cad.add_block(dwg, {
		name = "TABLE",
		base = { 0, 0 },
		entities = {
			{ type = "LWPOLYLINE", points = { -600, -400, 600, -400, 600, 400, -600, 400 }, closed = true },
			{ type = "CIRCLE", cx = -300, cy = -650, r = 200 },
			{ type = "CIRCLE", cx = 300, cy = -650, r = 200 },
			{ type = "CIRCLE", cx = -300, cy = 650, r = 200 },
			{ type = "CIRCLE", cx = 300, cy = 650, r = 200, aci = 0 },
		},
	})

	local W, H = 12000, 8000
	dwg = cad.add(dwg, {
		-- outer walls: a closed polyline 200 wide, with a bay window (a positive
		-- bulge on a counter-clockwise outline bulges outwards)
		{ type = "LWPOLYLINE", layer = "WALLS", width = 200, closed = true,
			points = { 0, 0, W, 0, W, 3000, W, 5000, W, H, 0, H },
			bulges = { 0, 0, 0.6, 0, 0, 0 } },
		-- partitions
		{ type = "LINE", layer = "WALLS", x1 = 7000, y1 = 0, x2 = 7000, y2 = 3800 },
		{ type = "LINE", layer = "WALLS", x1 = 7000, y1 = 4800, x2 = 7000, y2 = H },
		{ type = "LINE", layer = "WALLS", x1 = 7000, y1 = 4300, x2 = W, y2 = 4300 },
		{ type = "LINE", layer = "WALLS", x1 = 0, y1 = 5200, x2 = 2600, y2 = 5200 },
		{ type = "LINE", layer = "WALLS", x1 = 3500, y1 = 5200, x2 = 7000, y2 = 5200 },
		-- doors: leaf and swing
		{ type = "LINE", layer = "DOORS", x1 = 7000, y1 = 3800, x2 = 7900, y2 = 3800 },
		{ type = "ARC", layer = "DOORS", cx = 7000, cy = 3800, r = 900, start = 0, ["end"] = 90 },
		{ type = "LINE", layer = "DOORS", x1 = 2600, y1 = 5200, x2 = 2600, y2 = 6100 },
		{ type = "ARC", layer = "DOORS", cx = 2600, cy = 5200, r = 900, start = 0, ["end"] = 90 },
		-- furniture: tables (block inserts), a rug, a sofa
		{ type = "INSERT", layer = "FURNITURE", block = "TABLE", x = 3500, y = 2500, aci = 4 },
		{ type = "INSERT", layer = "FURNITURE", block = "TABLE", x = 9500, y = 2000, rotation = 30, aci = 6 },
		{ type = "INSERT", layer = "FURNITURE", block = "TABLE", x = 9600, y = 6300, sx = 0.8, aci = 30 },
		{ type = "ELLIPSE", layer = "FURNITURE", cx = 3500, cy = 2500, mx = 1800, my = 0, ratio = 0.6 },
		{ type = "LWPOLYLINE", layer = "FURNITURE", closed = true,
			points = { 400, 1000, 1200, 1000, 1200, 4000, 400, 4000 } },
		-- floors
		{ type = "HATCH", layer = "HATCH", loops = { { 7100, 4400, 11900, 4400, 11900, 7900, 7100, 7900 } },
			pattern = "ANSI31", scale = 30 },
		{ type = "HATCH", layer = "HATCH", aci = 251, loops = { { 100, 5300, 2500, 5300, 2500, 7900, 100, 7900 } } },
		-- garden path outside
		{ type = "SPLINE", layer = "GARDEN", degree = 3,
			control = { -1500, -1500, 2000, -2600, 5000, -500, 8000, -2400, 11000, -800, 13500, -1800 } },
		{ type = "CIRCLE", layer = "GARDEN", cx = 13500, cy = 6500, r = 700 },
		{ type = "CIRCLE", layer = "GARDEN", cx = -1300, cy = 3000, r = 500 },
		-- room names
		{ type = "TEXT", layer = "TEXT", x = 3500, y = 4200, text = "客廳 LIVING", height = 350, halign = "center" },
		{ type = "TEXT", layer = "TEXT", x = 9500, y = 3600, text = "臥室 BEDROOM", height = 300, halign = "center" },
		{ type = "TEXT", layer = "TEXT", x = 9500, y = 7300, text = "書房 STUDY", height = 300, halign = "center" },
		{ type = "TEXT", layer = "TEXT", x = 1300, y = 5600, text = "浴室 BATH", height = 250, halign = "center" },
		{ type = "MTEXT", layer = "TEXT", x = 0, y = -3000, height = 280, width = 9000,
			text = "AnyPage CAD 示範平面圖\\P1:100 · 單位 mm · DXF 讀取、繪製、量測、編輯、存檔" },
		-- dimensions drawn as lines and text
		{ type = "LINE", layer = "DIM", x1 = 0, y1 = H + 700, x2 = W, y2 = H + 700 },
		{ type = "LINE", layer = "DIM", x1 = 0, y1 = H + 400, x2 = 0, y2 = H + 900 },
		{ type = "LINE", layer = "DIM", x1 = W, y1 = H + 400, x2 = W, y2 = H + 900 },
		{ type = "TEXT", layer = "DIM", x = W / 2, y = H + 800, text = "12000", height = 250, halign = "center" },
		{ type = "LINE", layer = "DIM", x1 = -700, y1 = 0, x2 = -700, y2 = H },
		{ type = "TEXT", layer = "DIM", x = -800, y = H / 2, text = "8000", height = 250, rotation = 90, halign = "center" },
	})
	return dwg
end
