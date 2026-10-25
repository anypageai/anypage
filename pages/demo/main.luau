-- AnyPage demo page, written as a page package: page.json + this file + plasma.wgsl.
--
-- This script runs once, at load. It does not draw anything itself: it builds
-- a computation graph. Every panel is a function of cells. The engine calls a
-- function only when one of its inputs changed, or when a time it declared has
-- come; a function that returns the same value stops propagation there.
--
--   page.input   a value commands can change          (slider, toggle, seed)
--   page.clock   time, in seconds
--   page.stream  a native producer pushing items       (ticks, tokens)
--   page.job     native async work with progress       (CPU jobs)
--   page.derive  a pure function of other cells
--   page.panel   a function of cells drawn into a rectangle
--   page.tiled   native tiles, keyed by a params cell  (the fractal)
--   page.effect  a GPU operator shipped in this package
--
-- A function returns an optional second value (first, for panels): the clock
-- time until which its result stays correct. math.huge means "constant".

local rgba, hsv, alpha = color.rgba, color.hsv, color.alpha

-- ---------------------------------------------------------------- layout (world units)

local function R(x, y, w, h)
	return { x, y, w, h }
end

local TITLE = R(0, 0, 1400, 84)
local CLOCK = R(1440, 4, 960, 82)
local MANDEL_LABEL = R(0, 100, 1400, 46)
local MANDEL = R(0, 156, 1400, 1400)
local CONTROLS = R(1440, 100, 960, 270)
local CHART = R(1440, 390, 960, 300)
local TEXT = R(1440, 710, 960, 300)
local EFFECT = R(1440, 1030, 470, 300)
local EFFECT_LABEL = R(1440, 1030, 470, 50)
local ORBIT = R(1930, 1030, 470, 300)
local JOBS = R(1440, 1350, 960, 206)

-- controls, panel-local
local ITER_TRACK = R(24, 96, 560, 20)
local HUE_TRACK = R(24, 170, 560, 20)
local BTN_STREAMS = R(620, 58, 316, 42)
local BTN_ANIM = R(620, 106, 316, 42)
local BTN_TIMERS = R(620, 154, 316, 42)
local BTN_RUN = R(24, 208, 300, 48)
local BTN_CANCEL = R(340, 208, 244, 48)
local BTN_FIT = R(620, 208, 316, 48)

local ITER_MIN, ITER_MAX = 32, 4096
local JOB_COUNT = 6
local BLINK = 0.53

local PANEL_BG = rgba(22, 25, 32)
local TEXT_HI = rgba(232, 236, 244)
local TEXT_LO = rgba(140, 148, 164)
local WHITE = rgba(255, 255, 255)

-- ---------------------------------------------------------------- drawing helpers

local badge_widths = {}
local function badge_width(class)
	local w = badge_widths[class]
	if not w then
		w = page.measure(classes[class].label, 14) + 18
		badge_widths[class] = w
	end
	return w
end

local function badge(d, x, y, class)
	local c = classes[class]
	d:rrect(x, y, badge_width(class), 24, 12, alpha(c.color, 46))
	d:text(x + 9, y + 3, 14, c.color, c.label)
end

-- background, class badge, title and subtitle
local function panel(d, class, title, sub)
	d:rrect(0, 0, d.w, d.h, 14, PANEL_BG)
	d:stroke(0.5, 0.5, d.w - 1, d.h - 1, 14, 1, alpha(WHITE, 18))
	badge(d, 18, 14, class)
	d:text(18 + badge_width(class) + 12, 13, 20, TEXT_HI, title)
	if sub then
		d:text(18, 44, 14, TEXT_LO, sub)
	end
end

local function button(d, r, label, on, accent)
	d:rrect(r[1], r[2], r[3], r[4], 10, if on then alpha(accent, 70) else alpha(WHITE, 14))
	d:stroke(r[1], r[2], r[3], r[4], 10, 1, alpha(accent, 120))
	d:text(r[1] + 14, r[2] + r[4] * 0.5 - 11, 17, TEXT_HI, label)
end

local function slider(d, r, frac, label, accent)
	frac = math.clamp(frac, 0, 1)
	d:text(r[1], r[2] - 30, 16, TEXT_HI, label)
	d:rrect(r[1], r[2], r[3], r[4], r[4] * 0.5, alpha(WHITE, 22))
	d:rrect(r[1], r[2], r[3] * frac, r[4], r[4] * 0.5, alpha(accent, 150))
	d:circle(r[1] + r[3] * frac, r[2] + r[4] * 0.5, r[4] * 0.75, TEXT_HI)
end

local function iter_frac(n)
	return math.clamp(math.log(n / ITER_MIN) / math.log(ITER_MAX / ITER_MIN), 0, 1)
end

local function iter_from_frac(f)
	return math.round(ITER_MIN * (ITER_MAX / ITER_MIN) ^ math.clamp(f, 0, 1))
end

local function contains(r, x, y)
	return x >= r[1] and x <= r[1] + r[3] and y >= r[2] and y <= r[2] + r[4]
end

local function hms(secs)
	local s = math.floor(math.max(secs, 0))
	return string.format("%02d:%02d:%02d", s // 3600, (s // 60) % 60, s % 60)
end

local function onoff(b)
	return if b then "開" else "關"
end

-- ---------------------------------------------------------------- inputs

local t = page.clock()
local iterations = page.input("iterations", 256)
local hue = page.input("hue", 205)
-- a toggle remembers when it last changed, so a paused function of time
-- can freeze exactly where it was
local streams = page.input("streams", { on = true, since = 0 })
local anim = page.input("animation", { on = true, since = 0 })
local timers = page.input("timers", { on = true, since = 0 })

-- push streams: native producers on their own threads, batched into one commit per frame
local ticks = page.stream("ticks", "demo.ticks", { enabled = streams, cap = 600 })
local tokens = page.stream("tokens", "demo.tokens", { enabled = streams, cap = 4000, tail = 1600 })

-- async jobs on the worker pool, one seed input each
local seeds, jobs = {}, {}
for k = 1, JOB_COUNT do
	seeds[k] = page.input("job.seed." .. k, 0)
	jobs[k] = page.job("job." .. k, "demo.compute", { seeds[k] })
end

-- ---------------------------------------------------------------- engine-computed content

-- static tiles: parameters are a pure function; equal results keep every tile
local mandel_params = page.derive("mandel.params", { iterations, hue }, function(n, h)
	return { iterations = n, hue = h }
end)
page.tiled({ rect = MANDEL, source = "mandelbrot", params = mandel_params })

-- GPU pull stream: a function of time while running, constant while paused
local effect_params = page.derive("effect.params", { t, hue, anim }, function(now, h, a)
	if a.on then
		return { now, h }
	end
	return { a.since, h }, math.huge
end)
page.effect({ rect = EFFECT, shader = "plasma.wgsl", params = effect_params })

-- ---------------------------------------------------------------- panels

page.panel({
	id = 0,
	name = "title",
	rect = TITLE,
	class = "static",
	priority = 4,
	draw = function(d)
		-- no inputs: drawn once, re-rasterised natively only when the zoom level changes
		d:text(4, 4, 36, TEXT_HI, "AnyPage — 一個頁面，所有時間性")
		d:text(6, 54, 17, TEXT_LO, "這一頁是 Luau 頁面包：每個面板都是函數，沒有靜態與動態之分。拖曳平移，滾輪或雙指縮放。")
	end,
})

page.panel({
	id = 8,
	name = "clock",
	rect = CLOCK,
	class = "pull",
	priority = 1,
	inputs = { t, timers },
	draw = function(d, now, tm)
		local shown, valid = tm.since, math.huge
		if tm.on then
			-- predictable: this cannot change before the next whole second
			shown, valid = now, math.floor(now) + 1
		end
		d:rrect(0, 0, d.w, d.h, 14, PANEL_BG)
		badge(d, 18, 14, "pull")
		d:text(18 + badge_width("pull") + 12, 12, 20, TEXT_HI, "頁面已開啟 " .. hms(shown))
		d:text(
			18,
			48,
			14,
			TEXT_LO,
			if tm.on
				then "可預測的時間函數：宣告「下一整秒前不會變」，引擎只在整秒醒來（T 暫停）"
				else "已暫停：宣告為常數，引擎不再為它醒來（T 繼續）"
		)
		return valid
	end,
})

page.panel({
	id = 1,
	name = "mandel.label",
	rect = MANDEL_LABEL,
	class = "static",
	priority = 4,
	inputs = { mandel_params },
	draw = function(d, p)
		badge(d, 0, 10, "static")
		d:text(
			badge_width("static") + 12,
			9,
			20,
			TEXT_HI,
			string.format("引擎自己計算的 tile：需求驅動、離開視口即取消、參數變了舊 tile 先頂著（迭代 %d）", p.iterations)
		)
	end,
})

page.panel({
	id = 2,
	name = "controls",
	rect = CONTROLS,
	class = "ops",
	priority = 0,
	inputs = { iterations, hue, streams, anim, timers },
	draw = function(d, n, h, st, an, tm)
		panel(d, "ops", "指令改輸入格，結果沿圖傳播", "色相一個輸入格同時影響 tile、圖表、GPU 動畫與 CPU 動畫。")
		local acc = classes.ops.color
		slider(d, ITER_TRACK, iter_frac(n), string.format("迭代次數 iterations：%d", n), acc)
		slider(d, HUE_TRACK, h / 360, string.format("色相 hue：%.0f°", h), hsv(h, 0.75, 0.95))
		button(d, BTN_STREAMS, "串流：" .. onoff(st.on) .. "（空白鍵）", st.on, classes.push.color)
		button(d, BTN_ANIM, "連續動畫：" .. onoff(an.on) .. "（A）", an.on, classes.pull.color)
		button(d, BTN_TIMERS, "定時內容：" .. onoff(tm.on) .. "（T）", tm.on, classes.pull.color)
		button(d, BTN_RUN, "執行非同步任務（J）", false, classes.async.color)
		button(d, BTN_CANCEL, "取消任務（C）", false, classes.async.color)
		button(d, BTN_FIT, "貼合整頁（R）", false, acc)
	end,
})

page.panel({
	id = 3,
	name = "chart",
	rect = CHART,
	class = "push",
	priority = 2,
	inputs = { ticks, hue },
	draw = function(d, tk, h)
		panel(d, "push", "即時資料 Live ticks", "生產者在另一條執行緒以不固定速率推送，偶爾連發；引擎每幀合批成一次 commit。")
		local x0, y0, w, hh = 20, 76, d.w - 40, d.h - 96
		d:rrect(x0, y0, w, hh, 8, alpha(WHITE, 8))
		if tk and #tk.v >= 2 then
			local n = #tk.v
			-- 600 points, scaled and stroked natively
			d:series(x0 + 8, y0 + 10, w - 16, hh - 20, tk.v, hsv(h, 0.7, 1.0), hsv(h, 0.7, 0.9, 40), 2, y0 + hh - 2, 0.5)
			d:text(
				d.w - 430,
				12,
				17,
				TEXT_HI,
				string.format("最新 %.2f　近 %.1f 秒 %d 筆　累計 %d", tk.v[n], tk.t[n] - tk.t[1], n, tk.total)
			)
		end
	end,
})

page.panel({
	id = 4,
	name = "tokens",
	rect = TEXT,
	class = "push",
	priority = 2,
	inputs = { tokens, t, timers },
	draw = function(d, tk, now, tm)
		local cursor_on, valid = true, math.huge
		if tm.on then
			-- the cursor blinks: predictable, so declare the next toggle
			local phase = math.floor(now / BLINK)
			cursor_on, valid = phase % 2 == 0, (phase + 1) * BLINK
		end
		panel(d, "push", "逐字輸出 Token stream", "模擬模型逐字輸出：小片段、速率不固定；游標閃爍是可預測的時間函數。")
		if tk then
			d:textbox(20, 76, d.w - 40, d.h - 92, 21, TEXT_HI, tk.text .. (if cursor_on then "▍" else " "), true)
		end
		return valid
	end,
})

page.panel({
	id = 5,
	name = "orbit",
	rect = ORBIT,
	class = "pull",
	priority = 1,
	inputs = { t, hue, anim },
	draw = function(d, now, h, a)
		local time, valid = now, nil -- running: changes every frame, declares nothing
		if not a.on then
			time, valid = a.since, math.huge
		end
		panel(
			d,
			"pull",
			"腳本函數，依賴 t",
			if a.on then "連續動畫：每幀都變，只在可見時求值。" else "已暫停：宣告為常數，0 fps。"
		)
		local cx, cy = d.w * 0.5, 72 + (d.h - 72) * 0.5
		for k = 0, 15 do
			local ang = time * (0.5 + k * 0.11) + k * 0.7
			local rad = 18 + k * 6.2
			d:circle(cx + math.cos(ang) * rad * 1.6, cy + math.sin(ang) * rad * 0.9, 4 + (k % 4) * 1.6, hsv(h + k * 18, 0.65, 1.0, 230))
		end
		return valid
	end,
})

page.panel({
	id = 6,
	name = "effect.label",
	rect = EFFECT_LABEL,
	class = "pull",
	priority = 1,
	over = true,
	draw = function(d)
		d:rrect(0, 0, d.w, d.h, 14, rgba(10, 12, 16, 170))
		badge(d, 18, 13, "pull")
		d:text(18 + badge_width("pull") + 12, 12, 20, TEXT_HI, "頁面提供的 GPU 算子（WGSL）")
	end,
})

page.panel({
	id = 7,
	name = "jobs",
	rect = JOBS,
	class = "async",
	priority = 3,
	inputs = jobs,
	draw = function(d, ...)
		local results = { ... }
		panel(d, "async", "工作池上的 Job（J 執行，C 取消）", "完成前畫佔位與進度；完成時合批成一次 commit；輸入沒變就不重跑。")
		local gap = 12
		local cw = (d.w - 40 - gap * (JOB_COUNT - 1)) / JOB_COUNT
		local acc = classes.async.color
		for k = 1, JOB_COUNT do
			local job = results[k]
			local x, y = 20 + (k - 1) * (cw + gap), 78
			d:rrect(x, y, cw, 108, 10, alpha(WHITE, 10))
			d:text(x + 12, y + 10, 16, TEXT_HI, "任務 " .. k)
			local line1, line2, prog, col = "—", nil, nil, TEXT_LO
			if job == nil then
				-- not started
			elseif job.state == "pending" then
				line1, prog, col = string.format("進行中 %.0f%%", job.progress * 100), job.progress, acc
			elseif job.state == "error" then
				line1, col = job.message, rgba(255, 110, 110)
			elseif job.state == "done" then
				line1, col = "完成", classes.push.color
				line2 = string.format("%.0f ms · %.3f", job.ms, math.abs(job.value) % 1)
			else
				line1 = "閒置"
			end
			d:text(x + 12, y + 40, 15, col, line1)
			if line2 then
				d:text(x + 12, y + 64, 13, TEXT_LO, line2)
			end
			if prog then
				d:rrect(x + 12, y + 84, cw - 24, 8, 4, alpha(WHITE, 24))
				d:rrect(x + 12, y + 84, (cw - 24) * prog, 8, 4, acc)
			end
		end
	end,
})

-- ---------------------------------------------------------------- commands

-- every UI action is a command on an input cell; the graph does the rest
local function set_on(cell, on)
	if page.get(cell).on ~= on then
		page.set(cell, { on = on, since = page.now() })
	end
end

local function flip(cell)
	set_on(cell, not page.get(cell).on)
end

local function set_iterations(n)
	n = math.clamp(math.round(n), ITER_MIN, ITER_MAX)
	if n ~= page.get(iterations) then
		page.set(iterations, n)
	end
end

local function set_hue(h)
	h = math.round(math.clamp(h, 0, 360))
	if math.abs(h - page.get(hue)) >= 1 then
		page.set(hue, h)
	end
end

local function run_jobs()
	for k = 1, JOB_COUNT do
		page.set(seeds[k], math.random(1, 2147483646))
	end
end

local function cancel_jobs()
	for k = 1, JOB_COUNT do
		page.cancel(jobs[k])
	end
end

local dragging = nil -- "iterations" | "hue" while a slider is held

local function slide(which, wx)
	local track = if which == "iterations" then ITER_TRACK else HUE_TRACK
	local f = math.clamp((wx - CONTROLS[1] - track[1]) / track[3], 0, 1)
	if which == "iterations" then
		set_iterations(iter_from_frac(f))
	else
		set_hue(f * 360)
	end
end

page.on("pointer_down", function(wx, wy)
	if not contains(CONTROLS, wx, wy) then
		return nil -- the engine pans
	end
	local x, y = wx - CONTROLS[1], wy - CONTROLS[2]
	for _, s in { { ITER_TRACK, "iterations" }, { HUE_TRACK, "hue" } } do
		local r = s[1]
		if contains(R(r[1] - 12, r[2] - 16, r[3] + 24, r[4] + 32), x, y) then
			slide(s[2], wx)
			dragging = s[2]
			return "captured"
		end
	end
	if contains(BTN_STREAMS, x, y) then
		flip(streams)
	elseif contains(BTN_ANIM, x, y) then
		flip(anim)
	elseif contains(BTN_TIMERS, x, y) then
		flip(timers)
	elseif contains(BTN_RUN, x, y) then
		run_jobs()
	elseif contains(BTN_CANCEL, x, y) then
		cancel_jobs()
	elseif contains(BTN_FIT, x, y) then
		page.request("fit")
	else
		return nil
	end
	return "handled"
end)

page.on("pointer_drag", function(wx, _wy)
	if dragging then
		slide(dragging, wx)
	end
end)

page.on("pointer_up", function()
	dragging = nil
end)

page.on("key", function(k)
	if k == "space" then
		flip(streams)
	elseif k == "a" then
		flip(anim)
	elseif k == "t" then
		flip(timers)
	elseif k == "j" then
		run_jobs()
	elseif k == "c" then
		cancel_jobs()
	elseif k == "1" then
		set_iterations(page.get(iterations) / 2)
	elseif k == "2" then
		set_iterations(page.get(iterations) * 2)
	else
		return false
	end
	return true
end)

-- named controls from the host or an automation script (the bench uses these)
page.on("control", function(name, v)
	if name == "iterations" then
		set_iterations(v)
	elseif name == "hue" then
		set_hue(v)
	elseif name == "jobs" then
		run_jobs()
	elseif name == "cancel" then
		cancel_jobs()
	elseif name == "streams" then
		set_on(streams, v ~= 0)
	elseif name == "anim" then
		set_on(anim, v ~= 0)
	elseif name == "timers" then
		set_on(timers, v ~= 0)
	else
		return false
	end
	return true
end)
