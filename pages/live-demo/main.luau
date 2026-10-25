-- Live demo: this device broadcasts a test pattern as HLS (media.broadcast)
-- and plays its own stream back through the HLS player (media.hls), side by
-- side, with the numbers that show the pipeline working.
--
-- Agents: actions "play" { url }, "stop", "broadcast" { ... }, "stop_broadcast",
-- "share" { lan = true }; inputs "broadcast_request" and "play_request".

local PORT = 18090
local W = 1300

local BG = color.rgba(18, 20, 26)
local CARD = color.rgba(30, 34, 44)
local TEXT = color.rgba(230, 234, 240)
local DIM = color.rgba(150, 158, 172)
local ACCENT = color.rgba(96, 156, 255)
local WARN = color.rgba(255, 120, 110)
local GOOD = color.rgba(90, 210, 140)
local BUTTON = color.rgba(52, 60, 78)

-- 720p30: about 6 ms to encode and 2 ms to decode a frame on a desktop CPU
-- (release build); the broadcaster skips frames and says so when a machine
-- cannot keep up.
local function broadcast_request(host)
	return {
		source = "test",
		width = 1280,
		height = 720,
		fps = 30,
		bitrate = 3000000,
		segment_seconds = 1,
		serve = PORT,
		host = host or "loopback",
		list_size = 4,
	}
end

local function play_request(url)
	return { url = url or ("http://127.0.0.1:" .. PORT .. "/live.m3u8"), live_edge = 2 }
end

local bcast_req = page.input("broadcast_request", broadcast_request("loopback"), { agent = true })
local play_req = page.input("play_request", play_request(), { agent = true })

local bcast = page.stream("broadcast", "media.broadcast", { request = bcast_req, latest = true })
local video = page.stream("video", "media.hls", { request = play_req, latest = true })

local preview = page.derive("preview", { bcast }, function(b)
	return b and b.last and b.last.preview
end)
local frame = page.derive("frame", { video }, function(v)
	return v and v.last and v.last.frame
end)

-- ---------------------------------------------------------------- layout

page.panel({
	rect = { 0, 0, W, 56 },
	draw = function(d)
		d:clear(BG)
		d:text(20, 14, 22, TEXT, "Live: test pattern -> H.264 -> MPEG-TS -> HLS -> player")
	end,
})

page.panel({
	rect = { 20, 60, 620, 28 },
	inputs = { bcast_req },
	draw = function(d, req)
		d:text(0, 4, 16, DIM, req and "Broadcast preview (what is being encoded)" or "Broadcast stopped")
	end,
})

page.panel({
	rect = { 660, 60, 620, 28 },
	inputs = { play_req },
	draw = function(d, req)
		d:text(0, 4, 16, DIM, req and ("Playback of " .. req.url) or "Playback stopped")
	end,
})

page.image({ rect = { 20, 92, 620, 349 }, source = preview, name = "preview_image" })
page.image({ rect = { 660, 92, 620, 349 }, source = frame, name = "video_image" })

local function num(x, digits)
	if type(x) ~= "number" then
		return "-"
	end
	return string.format("%." .. (digits or 0) .. "f", x)
end

local function rows(d, x, y, list)
	for i, row in ipairs(list) do
		d:text(x, y + (i - 1) * 24, 15, DIM, row[1])
		d:text(x + 170, y + (i - 1) * 24, 15, row[3] or TEXT, row[2])
	end
end

page.panel({
	name = "stats",
	rect = { 20, 456, 1260, 220 },
	class = "push",
	inputs = { bcast, video },
	draw = function(d, b, v)
		d:rrect(0, 0, 1260, 220, 12, CARD)
		local bl = b and b.last or {}
		local vl = v and v.last or {}
		d:text(20, 12, 18, ACCENT, "Broadcast")
		rows(d, 20, 44, {
			{ "address", bl.url or "-" },
			{ "segments", num(bl.segments) .. "  (latest #" .. num(bl.sequence) .. ")" },
			{ "bitrate", num((bl.bitrate or 0) / 1000) .. " kbit/s" },
			{ "encode", num(bl.encode_ms, 2) .. " ms/frame, " .. num(bl.frames) .. " frames" },
			{ "requests served", num(bl.requests) },
			{ "error", bl.error or "none", bl.error and WARN or GOOD },
		})
		d:text(650, 12, 18, ACCENT, "Playback")
		rows(d, 650, 44, {
			{ "picture", num(vl.width) .. " x " .. num(vl.height) .. " at " .. num(vl.fps, 1) .. " fps" },
			{ "media time", num(vl.pts, 3) .. " s" },
			{ "buffered", num(vl.buffered, 2) .. " s" },
			{ "decode / convert", num(vl.decode_ms, 2) .. " / " .. num(vl.convert_ms, 2) .. " ms" },
			{ "dropped / stalls", num(vl.dropped) .. " / " .. num(vl.stalls) },
			{ "error", vl.error or vl.last_error or "none", (vl.error or vl.last_error) and WARN or GOOD },
		})
	end,
})

-- ---------------------------------------------------------------- buttons

local BY, BH, BW, GAP = 692, 44, 220, 16

local function button(d, i, label, active)
	local x = (i - 1) * (BW + GAP)
	d:rrect(x, 0, BW, BH, 10, active and ACCENT or BUTTON)
	d:text(x + 16, 12, 16, TEXT, label)
end

page.panel({
	rect = { 20, BY, 3 * BW + 2 * GAP, BH },
	inputs = { bcast_req, play_req },
	draw = function(d, breq, preq)
		button(d, 1, breq and "Stop broadcast" or "Start broadcast", breq ~= nil)
		button(d, 2, preq and "Stop playback" or "Play", preq ~= nil)
		local lan = breq and breq.host == "lan"
		button(d, 3, lan and "This device only" or "Share on local network", lan)
	end,
})

local function set_broadcast(req)
	page.set(bcast_req, req)
end

page.on("pointer_down", function(x, y)
	if y < BY or y > BY + BH then
		return false
	end
	local i = math.floor((x - 20) / (BW + GAP)) + 1
	local inside = (x - 20) - (i - 1) * (BW + GAP) <= BW
	if not inside then
		return false
	end
	local breq = page.get(bcast_req)
	if i == 1 then
		set_broadcast(if breq then nil else broadcast_request("loopback"))
	elseif i == 2 then
		page.set(play_req, if page.get(play_req) then nil else play_request())
	elseif i == 3 then
		local lan = breq and breq.host == "lan"
		set_broadcast(broadcast_request(if lan then "loopback" else "lan"))
	else
		return false
	end
	return "handled"
end)

-- ---------------------------------------------------------------- agents

page.action({
	name = "play",
	description = "Play an HLS playlist (default: this page's own broadcast)",
	params = { url = "string" },
	run = function(p)
		page.set(play_req, play_request(p and p.url))
		return { playing = (p and p.url) or "own broadcast" }
	end,
})

page.action({
	name = "stop",
	description = "Stop playback",
	run = function()
		page.set(play_req, nil)
		return { stopped = true }
	end,
})

page.action({
	name = "broadcast",
	description = "Start or change the broadcast: { width, height, fps, bitrate, segment_seconds }",
	params = { width = "number", height = "number", fps = "number", bitrate = "number", segment_seconds = "number" },
	run = function(p)
		local r = broadcast_request("loopback")
		for k, v in pairs(p or {}) do
			r[k] = v
		end
		set_broadcast(r)
		return r
	end,
})

page.action({
	name = "stop_broadcast",
	description = "Stop broadcasting",
	run = function()
		set_broadcast(nil)
		return { stopped = true }
	end,
})

page.action({
	name = "share",
	description = "Serve the broadcast to the local network ({ lan = true }) or to this device only",
	params = { lan = "boolean" },
	run = function(p)
		set_broadcast(broadcast_request(if p and p.lan then "lan" else "loopback"))
		return { lan = p and p.lan or false }
	end,
})
