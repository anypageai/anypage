-- AnyPage devices demo: Bluetooth LE, USB and serial ports on plain panels.
--
-- Every device module is optional (page.json "optional_modules"): a
-- platform without one shows a note in its panel instead.
--
-- What the page uses:
--   ble.scan    stream  the device table while scanning
--   ble.link    stream  one connected device: state, notifications
--   ble.read    job     read a characteristic (the device name)
--   ble.send    effect  write a line to a UART service on the open link
--   usb.list    job     the USB devices the system sees (listed off the engine thread)
--   usb.open    job     open one, list its interfaces and endpoints; the
--                       device stays open while this page holds the result
--   serial.list job     the serial ports
--   serial.port stream  an open port: lines, bytes, counters
--   serial.send         typed text goes to the open port
--
-- Listing devices asks the operating system and takes tens of
-- milliseconds, so it runs as a job; the page keeps the default limits.
-- Handlers can read only input cells, so each draw function remembers
-- where it drew its rows; a click looks them up there.

local rgba, alpha = color.rgba, color.alpha

local function R(x, y, w, h)
	return { x, y, w, h }
end

local TITLE = R(0, 0, 1600, 80)
local BLE = R(0, 100, 780, 440)
local LINK = R(0, 560, 780, 480)
local USB = R(820, 100, 780, 440)
local SERIAL = R(820, 560, 780, 480)

-- panel-local
local BTN_TOP = R(560, 14, 200, 40)
local BTN_READ = R(24, 416, 230, 44)
local BTN_PING = R(270, 416, 230, 44)
local BTN_DROP = R(516, 416, 240, 44)
local BTN_CLOSE = R(560, 420, 200, 44)
local ROW_Y, ROW_H = 92, 38

local PANEL_BG = rgba(22, 25, 32)
local TEXT_HI = rgba(232, 236, 244)
local TEXT_LO = rgba(140, 148, 164)
local WHITE = rgba(255, 255, 255)
local ACCENT = rgba(90, 160, 255)
local GOOD = rgba(80, 220, 120)
local WARN = rgba(255, 190, 70)
local BAD = rgba(255, 90, 90)
local HEART = rgba(255, 90, 110)

local HR_SERVICE = "0000180d-0000-1000-8000-00805f9b34fb"
local HR_MEASUREMENT = "00002a37-0000-1000-8000-00805f9b34fb"
local DEVICE_NAME = "00002a00-0000-1000-8000-00805f9b34fb"
local UART_RX = "6e400002-b5a3-f393-e0a9-e50e24dcca9e"
local UART_TX = "6e400003-b5a3-f393-e0a9-e50e24dcca9e"

local STATES = {
	idle = { "未連線", TEXT_LO },
	scanning = { "掃描中", GOOD },
	stopped = { "未掃描", TEXT_LO },
	connecting = { "連線中", WARN },
	reconnecting = { "重新連線中", WARN },
	connected = { "已連線", GOOD },
	disconnected = { "已斷線", BAD },
	failed = { "失敗", BAD },
	opening = { "開啟中", WARN },
	open = { "已開啟", GOOD },
	closed = { "已關閉，稍後重試", WARN },
}

-- ---------------------------------------------------------------- drawing helpers

local function contains(r, x, y)
	return x >= r[1] and x <= r[1] + r[3] and y >= r[2] and y <= r[2] + r[4]
end

local function frame(d, title)
	d:rrect(0, 0, d.w, d.h, 14, PANEL_BG)
	d:stroke(0.5, 0.5, d.w - 1, d.h - 1, 14, 1, alpha(WHITE, 18))
	d:text(20, 18, 22, TEXT_HI, title)
end

local function button(d, r, label, on)
	d:rrect(r[1], r[2], r[3], r[4], 10, if on then alpha(ACCENT, 90) else alpha(WHITE, 16))
	d:stroke(r[1], r[2], r[3], r[4], 10, 1, alpha(ACCENT, 140))
	d:text(r[1] + 14, r[2] + r[4] * 0.5 - 11, 17, TEXT_HI, label)
end

local function note(d, text)
	d:textbox(20, 70, d.w - 40, d.h - 90, 17, TEXT_LO, text)
end

local function status(d, y, state, extra)
	local s = STATES[state] or { state, TEXT_LO }
	d:circle(28, y + 10, 7, s[2])
	d:text(44, y, 17, TEXT_HI, s[1] .. (if extra then "  " .. extra else ""))
end

local function hex4(n)
	return string.format("%04x", n or 0)
end

-- a job cell's value: nil (idle), { state = "pending" }, { state = "error", message }, or the result
local function job_line(v, ok)
	if v == nil then
		return nil, TEXT_LO
	elseif v.state == "pending" then
		return "進行中…", WARN
	elseif v.state == "error" then
		return v.message, BAD
	end
	return ok(v), GOOD
end

-- a listing job's value as (items, note, note color): the list, or why there is none yet
local function listing(v)
	if v == nil or v.state == "pending" then
		return {}, "列出中…", WARN
	elseif v.state == "error" then
		return {}, v.message, BAD
	end
	return v, nil, TEXT_LO
end

-- where the last draw put its rows: { y, h, value } in world units
local hits = { ble = {}, usb = {}, ports = {} }

local function row_at(list, wy)
	for _, h in list do
		if wy >= h.y and wy < h.y + h.h then
			return h.value
		end
	end
	return nil
end

-- ---------------------------------------------------------------- cells

local scan_req = page.input("scan_req", nil, { agent = true })
local link_req = page.input("link_req", nil, { agent = true })
local read_req = page.input("read_req", nil)
local usb_refresh = page.input("usb_refresh", 0)
local usb_sel = page.input("usb_sel", nil, { agent = true })
local ports_refresh = page.input("ports_refresh", 0)
local port_req = page.input("port_req", nil, { agent = true })

local scan, link, read_job, rates, usb_list, usb_info, ports, port

if ble then
	scan = page.stream("scan", "ble.scan", { request = scan_req, latest = true })
	link = page.stream("link", "ble.link", { request = link_req, latest = true, keep = 120 })
	read_job = page.job("read", "ble.read", { read_req })

	-- Heart Rate Measurement: a flags byte, then the rate as u8 or u16 (flags bit 0)
	local function heart_rate(data)
		local n = buffer.len(data)
		if n < 2 then
			return nil
		end
		if buffer.readu8(data, 0) % 2 == 1 and n >= 3 then
			return buffer.readu16(data, 1)
		end
		return buffer.readu8(data, 1)
	end

	rates = page.derive("rates", { link }, function(l)
		local out = {}
		local last = l and l.last
		if last and last.notifications then
			for _, n in last.notifications do
				if n.characteristic == HR_MEASUREMENT then
					local v = heart_rate(n.data)
					if v then
						table.insert(out, v)
					end
				end
			end
		end
		return out
	end)
end

if usb then
	-- runs again whenever the refresh counter changes
	usb_list = page.job("usb_list", "usb.list", { usb_refresh })
	usb_info = page.job("usb_info", "usb.open", { usb_sel })
end

if serial then
	ports = page.job("ports", "serial.list", { ports_refresh })
	port = page.stream("port", "serial.port", { request = port_req, latest = true, keep_lines = 200 })
end

-- ---------------------------------------------------------------- commands (handlers and agents)

local seq = 0
local function next_seq()
	seq += 1
	return seq
end

local function set_scan(on)
	page.set(scan_req, if on then { on = true } else nil)
end

local function connect(id)
	page.set(link_req, {
		device = id,
		subscribe = {
			{ service = HR_SERVICE, characteristic = HR_MEASUREMENT, optional = true },
			{ characteristic = UART_TX, optional = true },
		},
	})
end

local function linked_device()
	local r = page.get(link_req)
	return r and r.device
end

local function read_name()
	local id = linked_device()
	if id then
		page.set(read_req, { device = id, characteristic = DEVICE_NAME, seq = next_seq() })
	end
end

-- an effect: queued on the link at once (a job would wait until something shows its value)
local function ping()
	local id = linked_device()
	if id then
		return ble.send(id, UART_RX, "ping " .. next_seq())
	end
	return 0
end

-- one USB device open at a time. The usb.open result keeps its device open;
-- the panel has read the old result, so without usb.close it would stay
-- open until the script's garbage collector frees it.
local function select_usb(id)
	local prev = page.get(usb_sel)
	if prev and prev.device ~= id then
		usb.close(prev.device)
	end
	page.set(usb_sel, { device = id, claim = "all" })
end

local function open_port(path, baud)
	page.set(port_req, { path = path, baud = baud or 115200 })
end

local function open_path()
	local r = page.get(port_req)
	return r and r.path
end

local function send(text)
	local path = open_path()
	if path and serial then
		return serial.send(path, text)
	end
	return 0
end

-- ---------------------------------------------------------------- panels

page.panel({
	id = 0,
	name = "title",
	rect = TITLE,
	class = "static",
	draw = function(d)
		local have = {}
		if ble then
			table.insert(have, "藍牙 LE")
		end
		if usb then
			table.insert(have, "USB")
		end
		if serial then
			table.insert(have, "序列埠")
		end
		d:text(4, 4, 34, TEXT_HI, "AnyPage 裝置：藍牙、USB、序列埠")
		local list = if #have > 0 then table.concat(have, "、") else "無"
		d:text(6, 52, 16, TEXT_LO, "平台 " .. page.info.platform .. "，可用模組：" .. list .. "。點選列表項目連線或開啟。")
	end,
})

page.panel({
	id = 1,
	name = "ble",
	rect = BLE,
	class = "push",
	inputs = if ble then { scan, scan_req, link_req } else {},
	draw = function(d, s, req, lreq)
		frame(d, "藍牙 LE 掃描")
		if not ble then
			note(d, "這個平台沒有藍牙模組。")
			return
		end
		button(d, BTN_TOP, if req then "停止掃描" else "開始掃描", req ~= nil)
		local item = s and s.last
		local scanning = item and item.scanning
		status(d, 54, if scanning then "scanning" else "stopped", if item then item.count .. " 個裝置" else nil)
		if item and item.error then
			d:text(330, 56, 15, BAD, item.error)
		end
		hits.ble = {}
		for i, dev in (item and item.devices) or {} do
			if i > 8 then
				break
			end
			local y = ROW_Y + (i - 1) * ROW_H
			if lreq and lreq.device == dev.id then
				d:rrect(14, y - 4, d.w - 28, ROW_H - 2, 8, alpha(ACCENT, 70))
			end
			d:text(28, y + 2, 17, TEXT_HI, dev.name or "（無名稱）")
			d:text(300, y + 4, 14, TEXT_LO, dev.id)
			local rssi = dev.rssi or -100
			local frac = math.clamp((rssi + 100) / 70, 0, 1)
			d:rrect(560, y + 8, 120, 12, 6, alpha(WHITE, 22))
			d:rrect(560, y + 8, 120 * frac, 12, 6, if frac > 0.5 then GOOD else WARN)
			d:text(690, y + 2, 15, TEXT_LO, rssi .. " dBm")
			table.insert(hits.ble, { y = BLE[2] + y - 4, h = ROW_H, value = dev.id })
		end
	end,
})

page.panel({
	id = 2,
	name = "link",
	rect = LINK,
	class = "push",
	inputs = if ble then { link, rates, read_job } else {},
	draw = function(d, l, hr, rd)
		frame(d, "連線與通知")
		if not ble then
			note(d, "這個平台沒有藍牙模組。")
			return
		end
		local it = l and l.last
		local state = (it and it.state) or "idle"
		status(d, 56, state, it and it.device)
		if it and it.error then
			d:textbox(24, 84, d.w - 48, 40, 15, BAD, it.error)
		end
		if hr and #hr > 0 then
			d:text(24, 126, 40, HEART, tostring(hr[#hr]))
			d:text(110, 146, 17, TEXT_LO, "bpm，共 " .. ((it and it.count) or 0) .. " 則通知")
			d:series(24, 186, d.w - 48, 150, hr, HEART, alpha(HEART, 40), 3, nil, 20)
		else
			d:text(24, 140, 16, TEXT_LO, "連上心率裝置（服務 180d）後，這裡畫出 2a37 通知。")
		end
		local rtext, rcolor = job_line(rd, function(v)
			return "名稱：" .. (v.text or "（非文字）")
		end)
		if rtext then
			d:text(24, 352, 16, rcolor, rtext)
		end
		if it and it.sent > 0 then
			d:text(24, 380, 16, GOOD, "已送出 " .. it.sent .. " 筆")
		end
		if it and it.latest and it.latest[UART_TX] then
			d:text(400, 380, 16, TEXT_HI, "回音：" .. buffer.tostring(it.latest[UART_TX]))
		end
		button(d, BTN_READ, "讀取名稱", false)
		button(d, BTN_PING, "送出 ping", false)
		button(d, BTN_DROP, "中斷連線", false)
	end,
})

page.panel({
	id = 3,
	name = "usb",
	rect = USB,
	class = "async",
	inputs = if usb then { usb_list, usb_sel, usb_info } else {},
	draw = function(d, list, sel, info)
		frame(d, "USB 裝置")
		if not usb then
			note(d, "這個平台沒有 USB 主機模組。")
			return
		end
		button(d, BTN_TOP, "重新整理", false)
		local devices, why, why_color = listing(list)
		if why then
			d:textbox(24, 60, d.w - 48, 30, 15, why_color, why)
		else
			d:text(24, 60, 15, TEXT_LO, #devices .. " 個裝置")
		end
		hits.usb = {}
		for i, dev in devices do
			if i > 6 then
				break
			end
			local y = ROW_Y + (i - 1) * ROW_H
			if sel and sel.device == dev.id then
				d:rrect(14, y - 4, d.w - 28, ROW_H - 2, 8, alpha(ACCENT, 70))
			end
			d:text(28, y + 2, 16, TEXT_HI, hex4(dev.vid) .. ":" .. hex4(dev.pid))
			d:text(150, y + 2, 16, TEXT_HI, dev.product or dev.manufacturer or "（無名稱）")
			d:text(520, y + 4, 14, TEXT_LO, (dev.driver or "") .. " " .. (dev.speed or ""))
			table.insert(hits.usb, { y = USB[2] + y - 4, h = ROW_H, value = dev.id })
		end
		local text, c = job_line(info, function(v)
			local parts = {}
			for _, intf in v.interfaces do
				local eps = {}
				for _, e in intf.endpoints do
					table.insert(eps, string.format("0x%02x %s %s %d", e.address, e.type, e.direction, e.max_packet))
				end
				table.insert(parts, "介面 " .. intf.number .. "（類別 " .. intf.class .. "）：" .. (if #eps > 0 then table.concat(eps, "、") else "無端點"))
			end
			return table.concat(parts, "\n")
		end)
		if text then
			d:textbox(24, 330, d.w - 48, 96, 15, c, text)
		end
	end,
})

page.panel({
	id = 4,
	name = "serial",
	rect = SERIAL,
	class = "push",
	inputs = if serial then { ports, port } else {},
	draw = function(d, list, p)
		frame(d, "序列埠")
		if not serial then
			note(d, "這個平台沒有序列埠模組。")
			return
		end
		button(d, BTN_TOP, "重新整理", false)
		local it = p and p.last
		local found, why, why_color = listing(list)
		hits.ports = {}
		for i, info in found do
			if i > 3 then
				break
			end
			local y = 60 + (i - 1) * 34
			if it and it.path == info.path and it.state ~= "idle" then
				d:rrect(14, y - 4, d.w - 28, 32, 8, alpha(ACCENT, 70))
			end
			d:text(28, y, 16, TEXT_HI, info.path)
			d:text(200, y + 2, 14, TEXT_LO, info.type .. " " .. (info.product or ""))
			table.insert(hits.ports, { y = SERIAL[2] + y - 4, h = 34, value = info.path })
		end
		if why then
			d:text(24, 60, 15, why_color, why)
		end
		local state = (it and it.state) or "idle"
		status(d, 170, state, if it and it.path then it.path .. "  收 " .. it.rx .. "  送 " .. it.tx else nil)
		if it and it.error then
			d:text(24, 196, 15, BAD, it.error)
		end
		d:rrect(16, 220, d.w - 32, 190, 8, rgba(10, 12, 16))
		local lines = (it and it.lines) or {}
		local shown = table.concat(lines, "\n") .. (if it and it.partial ~= "" then "\n" .. it.partial else "")
		d:textbox(26, 228, d.w - 52, 176, 15, GOOD, if shown ~= "" then shown else "點選上方的序列埠開啟；鍵入的文字會送出（Enter 送出換行）。", true)
		button(d, BTN_CLOSE, "關閉序列埠", false)
	end,
})

-- ---------------------------------------------------------------- input

page.on("pointer_down", function(wx, wy)
	if ble and contains(BLE, wx, wy) then
		if contains(BTN_TOP, wx - BLE[1], wy - BLE[2]) then
			set_scan(page.get(scan_req) == nil)
			return "handled"
		end
		local id = row_at(hits.ble, wy)
		if id then
			connect(id)
			return "handled"
		end
	elseif ble and contains(LINK, wx, wy) then
		local x, y = wx - LINK[1], wy - LINK[2]
		if contains(BTN_READ, x, y) then
			read_name()
		elseif contains(BTN_PING, x, y) then
			ping()
		elseif contains(BTN_DROP, x, y) then
			page.set(link_req, nil)
		else
			return nil
		end
		return "handled"
	elseif usb and contains(USB, wx, wy) then
		if contains(BTN_TOP, wx - USB[1], wy - USB[2]) then
			page.set(usb_refresh, page.get(usb_refresh) + 1)
			return "handled"
		end
		local id = row_at(hits.usb, wy)
		if id then
			-- claiming shows at once whether transfers are possible (WinUSB on Windows)
			select_usb(id)
			return "handled"
		end
	elseif serial and contains(SERIAL, wx, wy) then
		local x, y = wx - SERIAL[1], wy - SERIAL[2]
		if contains(BTN_TOP, x, y) then
			page.set(ports_refresh, page.get(ports_refresh) + 1)
			return "handled"
		elseif contains(BTN_CLOSE, x, y) then
			page.set(port_req, nil)
			return "handled"
		end
		local path = row_at(hits.ports, wy)
		if path then
			open_port(path)
			return "handled"
		end
	end
	return nil
end)

page.on("text", function(t)
	if open_path() then
		send(t)
	end
end)

page.on("key", function(k)
	if k == "enter" and open_path() then
		send("\r\n")
		return true
	end
	return false
end)

-- ---------------------------------------------------------------- the semantic port

page.action({
	name = "scan",
	description = "開始或停止藍牙 LE 掃描",
	params = { on = "boolean" },
	run = function(p)
		set_scan(p == nil or p.on ~= false)
		return { scanning = page.get(scan_req) ~= nil }
	end,
})

page.action({
	name = "connect",
	description = "連上一個藍牙裝置（scan 結果中的 id），訂閱心率與 UART 通知",
	params = { device = "string" },
	run = function(p)
		connect(p.device)
		return { device = p.device }
	end,
})

page.action({
	name = "disconnect",
	description = "中斷藍牙連線",
	run = function()
		page.set(link_req, nil)
		return true
	end,
})

page.action({
	name = "read_name",
	description = "讀取已連線裝置的名稱（2a00）",
	run = function()
		read_name()
		return true
	end,
})

page.action({
	name = "ping",
	description = "寫一行文字到已連線裝置的 UART RX",
	run = function()
		ping()
		return { seq = seq }
	end,
})

page.action({
	name = "usb_open",
	description = "開啟一個 USB 裝置並列出介面與端點",
	params = { device = "string" },
	run = function(p)
		select_usb(p.device)
		return true
	end,
})

page.action({
	name = "open_port",
	description = "開啟序列埠",
	params = { path = "string", baud = "number" },
	run = function(p)
		open_port(p.path, p.baud)
		return true
	end,
})

page.action({
	name = "close_port",
	description = "關閉序列埠",
	run = function()
		page.set(port_req, nil)
		return true
	end,
})

page.action({
	name = "send",
	description = "送文字到開啟中的序列埠",
	params = { text = "string" },
	run = function(p)
		return { sent = send(p.text) }
	end,
})
