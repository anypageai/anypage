-- AnyPage 3D demo: the `scene` module.
--
-- Models are native resources (scene.model). This script only builds them,
-- arranges them in a scene table and keeps a camera; the GPU layer
-- `scene.view` draws meshes with a depth buffer and point clouds within a
-- point budget, entirely in Rust.
--
--   drag in the view    orbit (or pan, with the Pan button)
--   wheel               zoom
--   keys                arrows orbit, + / - zoom, g grid, o ortho, f fit, 1 2 3 4 what to show
--   drop a file         .glb .gltf .obj .stl .ply .xyz .las .laz (loaded by the job scene.load)

local rgba, alpha = color.rgba, color.alpha

local function R(x, y, w, h)
	return { x, y, w, h }
end

local TITLE = R(0, 0, 1600, 60)
local VIEW = R(0, 70, 1100, 930)
local SIDE = R(1120, 70, 480, 930)
local ASPECT = VIEW[3] / VIEW[4]

local BG = rgba(16, 19, 26)
local PANEL_BG = rgba(24, 28, 37)
local TEXT_HI = rgba(232, 236, 244)
local TEXT_LO = rgba(140, 148, 164)
local ACCENT = rgba(96, 156, 255)
local WHITE = rgba(255, 255, 255)

-- ---------------------------------------------------------------- models (built once, held natively)

-- an octahedron with per-vertex colours, as OBJ text
local OCTA_OBJ = [[
# octahedron with vertex colours
v  0  1  0  1.0 0.9 0.2
v  1  0  0  1.0 0.3 0.2
v  0  0  1  0.2 0.9 0.4
v -1  0  0  0.2 0.5 1.0
v  0  0 -1  0.8 0.3 1.0
v  0 -1  0  0.9 0.9 0.9
f 1 3 2
f 1 4 3
f 1 5 4
f 1 2 5
f 6 2 3
f 6 3 4
f 6 4 5
f 6 5 2
]]

local CUBE = scene.primitive("box", { size = 0.9 })
local BALL = scene.primitive("sphere", { radius = 0.55, segments = 48 })
local POST = scene.primitive("cylinder", { radius = 0.18, height = 1.2, segments = 32 })
local OCTA = scene.load(OCTA_OBJ, "obj", { normals = "flat" })
local TERRAIN = scene.synthetic({ shape = "terrain", count = 400000, seed = 7 })
local HELIX = scene.synthetic({ shape = "helix", count = 60000, seed = 3 })

local NAMES = {
	{ "方塊 box", CUBE, { -1.1, 0.7, -0.7 } },
	{ "球 sphere", BALL, { 0.1, 0.8, 0.7 } },
	{ "柱 cylinder", POST, { 1.3, 0.7, 0.8 } },
	{ "八面體 OBJ", OCTA, { 1.2, 0.95, -0.8 } },
	{ "地形點雲 terrain", TERRAIN, { 0, 0, 0 } },
	{ "螺旋點雲 helix", HELIX, { -1.3, 1.0, 1.0 } },
}

-- ---------------------------------------------------------------- state

-- what to show and how
local View = page.input("view", {
	show = "all", -- all | models | cloud | file
	point_size = 2,
	height = false,
	grid = true,
	ortho = false,
	pan = false,
}, { agent = true })

-- the camera relative to "frame everything": angles, zoom factor, pan in view heights
local Cam = page.input("camera", { yaw = 35, pitch = 24, zoom = 1, pan_x = 0, pan_y = 0 }, { agent = true })

-- a dropped or opened file, loaded off the page thread
local File = page.input("file", nil)
local Loaded = page.job("loaded", "scene.load", { File })

local function is_model(m)
	return type(m) == "userdata"
end

local function scene_for(v, loaded)
	local items = {}
	if v.show == "file" and is_model(loaded) then
		table.insert(items, { model = loaded, point_size = v.point_size, mode = if v.height then "height" else "vertex" })
	else
		local models = v.show == "all" or v.show == "models"
		local clouds = v.show == "all" or v.show == "cloud"
		if models then
			table.insert(items, { model = CUBE, color = rgba(240, 150, 60), transform = { translate = { -1.1, 0.25, -0.7 }, rotate = { 0, 25, 0 } } })
			table.insert(items, { model = BALL, color = rgba(90, 150, 240), transform = { translate = { 0.1, 0.35, 0.7 } } })
			table.insert(items, { model = POST, color = rgba(170, 176, 190), transform = { translate = { 1.3, 0.1, 0.8 } } })
			table.insert(items, { model = OCTA, transform = { translate = { 1.2, 0.55, -0.8 }, scale = 0.4 } })
		end
		if clouds then
			table.insert(items, { model = TERRAIN, point_size = v.point_size, mode = if v.height then "height" else "vertex" })
			table.insert(items, { model = HELIX, point_size = v.point_size + 1, transform = { translate = { -1.3, 1.0, 1.0 }, scale = 0.3 } })
		end
	end
	return {
		id = "demo",
		items = items,
		background = BG,
		grid = if v.grid then { color = rgba(120, 130, 150, 90) } else false,
	}
end

local Scene = page.derive("scene", { View, Loaded }, scene_for)

-- what each render of the view did (pushed by the renderer after it draws)
local Stats = page.stream("stats", "scene.stats", { id = "demo", latest = true })

-- the absolute camera: frame the scene, then apply the relative orbit / zoom / pan
local Camera = page.derive("camera.view", { Cam, Scene, View }, function(c, s, v)
	local base = scene.camera({ yaw = c.yaw, pitch = c.pitch, ortho = v.ortho })
	local cam = scene.fit(base, s, { aspect = ASPECT, margin = 1.05 })
	cam = scene.zoom(cam, c.zoom)
	if c.pan_x ~= 0 or c.pan_y ~= 0 then
		cam = scene.pan(cam, c.pan_x * VIEW[4], c.pan_y * VIEW[4], VIEW[4])
	end
	return cam
end)

-- ---------------------------------------------------------------- the 3D view

page.layer({ rect = VIEW, kind = "scene.view", inputs = { Scene, Camera }, name = "view3d" })

-- labels over the view: scene.project puts text where each model is
page.panel({
	rect = VIEW,
	name = "labels",
	inputs = { Camera, View },
	draw = function(d, cam, v)
		if v.show == "file" then
			return
		end
		for _, it in NAMES do
			local shown = v.show == "all" or (v.show == "models") == (it[2] ~= TERRAIN and it[2] ~= HELIX)
			if shown then
				local p = scene.project(cam, { x = it[3][1], y = it[3][2] + 0.45, z = it[3][3] }, d.w, d.h)
				if p.visible and p.x > 0 and p.x < d.w and p.y > 0 and p.y < d.h then
					local w = page.measure(it[1], 15) + 16
					d:rrect(p.x - w / 2, p.y - 13, w, 26, 8, alpha(BG, 190))
					d:text(p.x - w / 2 + 8, p.y - 10, 15, TEXT_HI, it[1])
				end
			end
		end
	end,
})

-- ---------------------------------------------------------------- title and side panel

page.panel({
	rect = TITLE,
	draw = function(d)
		d:rrect(0, 0, d.w, d.h, 12, PANEL_BG)
		d:text(20, 14, 24, TEXT_HI, "AnyPage 3D: scene 模組")
		d:text(420, 20, 15, TEXT_LO, "拖曳旋轉 · 滾輪縮放 · 方向鍵 · g 格線 · o 正交 · f 重新取景 · 拖入 glb/obj/stl/ply/las/laz/xyz")
	end,
})

local BUTTONS = {
	{ id = "all", label = "全部", r = R(20, 420, 100, 40) },
	{ id = "models", label = "網格", r = R(130, 420, 100, 40) },
	{ id = "cloud", label = "點雲", r = R(240, 420, 100, 40) },
	{ id = "file", label = "檔案", r = R(350, 420, 110, 40) },
	{ id = "size", label = "點大小", r = R(20, 470, 140, 40) },
	{ id = "height", label = "高度著色", r = R(170, 470, 140, 40) },
	{ id = "grid", label = "格線", r = R(320, 470, 140, 40) },
	{ id = "ortho", label = "正交", r = R(20, 520, 140, 40) },
	{ id = "pan", label = "平移", r = R(170, 520, 140, 40) },
	{ id = "fit", label = "重新取景", r = R(320, 520, 140, 40) },
	{ id = "open", label = "開啟檔案…", r = R(20, 570, 440, 40) },
}

local function button_on(id, v)
	if id == "all" or id == "models" or id == "cloud" or id == "file" then
		return v.show == id
	elseif id == "size" then
		return v.point_size > 1
	elseif id == "height" or id == "grid" or id == "ortho" or id == "pan" then
		return v[id]
	end
	return false
end

local function fmt_count(n)
	if n >= 1e6 then
		return string.format("%.2f M", n / 1e6)
	elseif n >= 1e3 then
		return string.format("%.1f k", n / 1e3)
	end
	return tostring(n)
end

page.panel({
	rect = SIDE,
	name = "side",
	inputs = { View, Camera, Scene, Loaded, File, Stats },
	draw = function(d, v, cam, s, loaded, file, stats)
		d:rrect(0, 0, d.w, d.h, 12, PANEL_BG)
		d:text(20, 16, 22, TEXT_HI, "場景內容")
		local y = 54
		local tris, pts = 0, 0
		for _, it in s.items do
			local info = scene.info(it.model)
			tris += info.triangles
			pts += info.points
			local what = if info.points > 0 then fmt_count(info.points) .. " 點" else fmt_count(info.triangles) .. " 三角形"
			d:text(20, y, 15, TEXT_LO, string.format("%-8s %s", info.format, what))
			y += 24
			if y > 300 then
				break
			end
		end
		d:text(20, 330, 15, TEXT_HI, string.format("合計 %s 三角形 · %s 點", fmt_count(tris), fmt_count(pts)))
		d:text(20, 356, 15, TEXT_LO, string.format("yaw %.0f°  pitch %.0f°  distance %.2f", cam.yaw, cam.pitch, cam.distance))
		local st = if type(stats) == "table" then stats.last else nil
		if st and st.renders > 0 then
			d:text(20, 380, 14, TEXT_LO, string.format("上次繪製 %.1f ms · %s 點 · %dx MSAA · GPU %.0f MB", st.ms, fmt_count(st.points_drawn), st.samples, st.gpu_bytes / 1048576))
		end
		for _, b in BUTTONS do
			local on = button_on(b.id, v)
			d:rrect(b.r[1], b.r[2], b.r[3], b.r[4], 9, if on then alpha(ACCENT, 80) else alpha(WHITE, 14))
			d:stroke(b.r[1], b.r[2], b.r[3], b.r[4], 9, 1, alpha(ACCENT, 120))
			d:text(b.r[1] + 12, b.r[2] + 9, 16, TEXT_HI, b.label)
		end
		-- the loaded file
		local fy = 640
		if file then
			d:text(20, fy, 16, TEXT_HI, "檔案: " .. tostring(file.name))
			if type(loaded) == "table" and loaded.state == "pending" then
				d:text(20, fy + 26, 15, TEXT_LO, string.format("載入中 %.0f%%", (loaded.progress or 0) * 100))
			elseif type(loaded) == "table" and loaded.state == "error" then
				d:textbox(20, fy + 26, d.w - 40, 120, 14, rgba(255, 120, 120), tostring(loaded.message), false)
			elseif is_model(loaded) then
				local info = scene.info(loaded)
				d:text(20, fy + 26, 15, TEXT_LO, string.format("%s · %s 頂點 · %s 三角形 · %s 點", info.kind, fmt_count(info.vertices), fmt_count(info.triangles), fmt_count(info.points)))
				local b = info.bounds
				if b then
					d:text(20, fy + 50, 14, TEXT_LO, string.format("大小 %.3g x %.3g x %.3g", b.size.x, b.size.y, b.size.z))
				end
			end
		else
			d:textbox(20, fy, d.w - 40, 80, 15, TEXT_LO, "把 3D 檔案拖進視窗，或按「開啟檔案」。大型點雲在背景載入，並在載入時打亂順序，所以任何前綴都是均勻取樣。", false)
		end
	end,
})

-- ---------------------------------------------------------------- input

local function contains(r, x, y)
	return x >= r[1] and x <= r[1] + r[3] and y >= r[2] and y <= r[2] + r[4]
end

local function update(cell, f)
	local v = table.clone(page.get(cell))
	f(v)
	page.set(cell, v)
end

local function open_file()
	page.open_file({
		filters = { "glb", "gltf", "obj", "stl", "ply", "xyz", "las", "laz" },
		done = function(f)
			if f.data then
				page.set(File, f)
				update(View, function(v)
					v.show = "file"
				end)
			end
		end,
	})
end

local function press(id)
	if id == "open" then
		open_file()
	elseif id == "fit" then
		page.set(Cam, { yaw = page.get(Cam).yaw, pitch = page.get(Cam).pitch, zoom = 1, pan_x = 0, pan_y = 0 })
	else
		update(View, function(v)
			if id == "all" or id == "models" or id == "cloud" or id == "file" then
				v.show = id
			elseif id == "size" then
				v.point_size = if v.point_size > 1 then 1 else 2
			else
				v[id] = not v[id]
			end
		end)
	end
end

local drag = nil

page.on("pointer_down", function(x, y)
	if contains(VIEW, x, y) then
		drag = { x = x, y = y }
		return "captured"
	end
	if contains(SIDE, x, y) then
		local lx, ly = x - SIDE[1], y - SIDE[2]
		for _, b in BUTTONS do
			if contains(b.r, lx, ly) then
				press(b.id)
				return "handled"
			end
		end
	end
	return false
end)

page.on("pointer_drag", function(x, y)
	if not drag then
		return
	end
	local dx, dy = x - drag.x, y - drag.y
	drag = { x = x, y = y }
	update(Cam, function(c)
		if page.get(View).pan then
			c.pan_x += dx / VIEW[4]
			c.pan_y += dy / VIEW[4]
		else
			c.yaw -= dx * 0.4
			c.pitch = math.clamp(c.pitch + dy * 0.4, -89, 89)
		end
	end)
end)

page.on("pointer_up", function()
	drag = nil
end)

page.on("wheel", function(x, y, dx, dy)
	if not contains(VIEW, x, y) then
		return false
	end
	update(Cam, function(c)
		c.zoom = math.clamp(c.zoom * 0.9 ^ dy, 0.02, 50)
	end)
	return true
end)

page.on("key", function(k)
	if k == "left" or k == "right" then
		update(Cam, function(c)
			c.yaw += if k == "left" then 10 else -10
		end)
	elseif k == "up" or k == "down" then
		update(Cam, function(c)
			c.pitch = math.clamp(c.pitch + (if k == "up" then 10 else -10), -89, 89)
		end)
	elseif k == "+" or k == "=" or k == "-" then
		update(Cam, function(c)
			c.zoom *= if k == "-" then 1.25 else 0.8
		end)
	elseif k == "g" or k == "o" then
		press(if k == "g" then "grid" else "ortho")
	elseif k == "f" then
		press("fit")
	elseif k == "1" or k == "2" or k == "3" or k == "4" then
		press(({ "all", "models", "cloud", "file" })[tonumber(k)])
	else
		return false
	end
	return true
end)

page.on("file_drop", function(f)
	page.set(File, f)
	update(View, function(v)
		v.show = "file"
	end)
	return true
end)

-- named controls for hosts and automation: ("yaw", 90), ("zoom", 0.5), ...
page.on("control", function(name, value)
	if name == "yaw" or name == "pitch" or name == "zoom" then
		update(Cam, function(c)
			c[name] = value
		end)
		return true
	elseif name == "point_size" then
		update(View, function(v)
			v.point_size = math.clamp(value, 1, 16)
		end)
		return true
	end
	return false
end)

-- ---------------------------------------------------------------- agent actions

page.action({
	name = "set_view",
	description = "Turn the camera: yaw and pitch in degrees, zoom as a factor of the framed distance",
	params = { yaw = "number", pitch = "number", zoom = "number" },
	run = function(p)
		update(Cam, function(c)
			c.yaw = p.yaw or c.yaw
			c.pitch = math.clamp(p.pitch or c.pitch, -89, 89)
			c.zoom = p.zoom or c.zoom
		end)
		return page.get(Cam)
	end,
})

page.action({
	name = "show",
	description = "Choose what the view shows: all, models, cloud or file",
	params = { what = "string" },
	run = function(p)
		if p.what ~= "all" and p.what ~= "models" and p.what ~= "cloud" and p.what ~= "file" then
			return { error = "what must be all, models, cloud or file" }
		end
		press(p.what)
		return page.get(View)
	end,
})

page.action({
	name = "describe_models",
	description = "Sizes and bounds of the demo models",
	params = {},
	run = function()
		local out = {}
		for _, it in NAMES do
			local info = scene.info(it[2])
			table.insert(out, { name = it[1], kind = info.kind, vertices = info.vertices, triangles = info.triangles, points = info.points })
		end
		return out
	end,
})
