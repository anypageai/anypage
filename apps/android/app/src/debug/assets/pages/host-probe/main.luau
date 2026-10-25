-- Host limits probe (debug builds only): page requests that used to crash
-- or break the Android host, one button each. The host must refuse or
-- survive every one of them; results show in the log below and in logcat
-- (tag AnyPage, "host-probe").

local rgba = color.rgba
local floor = math.floor

local W = 720
local COLS, BW, BH, GAP, TOP = 2, 350, 84, 20, 90
local MB = 1024 * 1024

local Log = page.input("log", { "tap a probe" })
local Last = page.input("last", "")

local function add(msg)
	local log = page.get(Log)
	table.insert(log, msg)
	while #log > 12 do
		table.remove(log, 1)
	end
	page.set(Log, log)
	page.set(Last, msg)
	page.log("host-probe: " .. msg)
end

local function answer(what)
	return function(r)
		if r.cancelled then
			add(what .. ": cancelled")
		elseif r.error then
			add(what .. ": failed: " .. tostring(r.error))
		elseif r.saved then
			add(what .. ": saved " .. #tostring(r.saved) .. " chars of name")
		else
			add(what .. ": opened " .. tostring(r.name) .. " (" .. tostring(r.size) .. " bytes)")
		end
	end
end

local PROBES = {
	{
		"copy 3 MB",
		function()
			page.copy(string.rep("x", 3 * MB))
			add("copy 3 MB: asked")
		end,
	},
	{
		"copy 200 KB",
		function()
			page.copy(string.rep("y", 200 * 1024))
			add("copy 200 KB: asked")
		end,
	},
	-- the runtime now refuses rectangles beyond 1e9 itself; the host still
	-- never follows a caret it cannot place (apps/android/src/view.rs)
	{
		"caret at 1e39",
		function()
			local ok = pcall(page.focus_text, { 10, 1e39, 2, 30 })
			add("caret at 1e39: " .. (if ok then "asked" else "refused by the runtime"))
		end,
	},
	{
		"caret at 1e30",
		function()
			local ok = pcall(page.focus_text, { 10, 1e30, 2, 30 })
			add("caret at 1e30: " .. (if ok then "asked" else "refused by the runtime"))
		end,
	},
	{
		"caret on page",
		function()
			page.focus_text({ 40, 40, 2, 30 })
			add("caret on page: asked")
		end,
	},
	{
		"focus off",
		function()
			page.focus_text(nil)
			add("focus off")
		end,
	},
	{
		"two opens",
		function()
			page.open_file({ filters = { "txt" }, done = answer("open 1") })
			page.open_file({ filters = { "txt" }, done = answer("open 2") })
			add("two opens: asked")
		end,
	},
	{
		"1 MB save name",
		function()
			page.save_file({ name = string.rep("n", MB) .. ".txt", data = "probe", done = answer("save") })
			add("1 MB save name: asked")
		end,
	},
	{
		"5000 filters",
		function()
			local f = {}
			for i = 1, 5000 do
				f[i] = "ext" .. i
			end
			page.open_file({ filters = f, done = answer("open 5000 filters") })
			add("5000 filters: asked")
		end,
	},
	{
		"1 MB filter",
		function()
			page.open_file({ filters = { string.rep("f", MB) }, done = answer("open 1 MB filter") })
			add("1 MB filter: asked")
		end,
	},
}

local function cell(i)
	local col, row = (i - 1) % COLS, floor((i - 1) / COLS)
	return { col * (BW + GAP), TOP + row * (BH + GAP), BW, BH }
end

page.on("pointer_down", function(x, y)
	for i, p in ipairs(PROBES) do
		local r = cell(i)
		if x >= r[1] and x < r[1] + r[3] and y >= r[2] and y < r[2] + r[4] then
			p[2]()
			return "handled"
		end
	end
	return false
end)

page.panel({
	rect = { 0, 0, W, 900 },
	name = "probe",
	inputs = { Log },
	draw = function(d, log)
		d:rect(0, 0, d.w, d.h, rgba(22, 25, 32))
		d:text(4, 20, 30, rgba(236, 240, 248), "Host limits probe")
		for i, p in ipairs(PROBES) do
			local r = cell(i)
			d:rrect(r[1], r[2], r[3], r[4], 12, rgba(36, 44, 62))
			d:text(r[1] + 18, r[2] + 26, 26, rgba(236, 240, 248), p[1])
		end
		local y = TOP + 5 * (BH + GAP) + 10
		for i, line in ipairs(log) do
			d:text(8, y + (i - 1) * 24, 18, rgba(170, 178, 194), line)
		end
	end,
})
