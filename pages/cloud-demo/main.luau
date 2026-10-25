-- AnyPage cloud demo: a chat panel streaming from an OpenAI-compatible
-- server (Ollama, LM Studio, vLLM, OpenAI) through the `cloud` module.
--
-- What page.json grants this page:
--   capabilities.modules  ["cloud"]
--   capabilities.net      hosts the module may contact: localhost and
--                         127.0.0.1 (any port: Ollama 11434, LM Studio 1234,
--                         vLLM 8000) and api.openai.com
--   capabilities.secrets  ["openai"]: the API key, kept by the host
--                         (ANYPAGE_SECRET_OPENAI in development); the page
--                         only names it, it never sees the value
--
-- The endpoint and model are input cells (`base_url`, `model`), so a preset
-- button, an agent ({"set": "base_url", ...}) or a test can point the page
-- at another server. The conversation is kept by the module
-- (keep_history = true): each request carries only the new user message.

local rgba, alpha = color.rgba, color.alpha

local function R(x, y, w, h)
	return { x, y, w, h }
end

local W = 1200
local HEADER = R(0, 0, W, 96)
local CHAT = R(0, 108, W, 560)
local INPUT = R(0, 680, W, 80)
local PRESETS = R(0, 772, W, 76)

-- inside INPUT (panel-local)
local FIELD = R(16, 14, 940, 52)
local SEND = R(972, 14, 100, 52)
local STOP = R(1084, 14, 100, 52)

local PRESET_LIST = {
	{ label = "Ollama · localhost:11434", base = "http://localhost:11434/v1", model = "llama3.2" },
	{ label = "LM Studio · localhost:1234", base = "http://localhost:1234/v1", model = "local-model" },
	{ label = "OpenAI · gpt-4o-mini", base = "https://api.openai.com/v1", model = "gpt-4o-mini" },
}
local PRESET_W = 270
local CLEAR = R(16 + 3 * (PRESET_W + 12), 14, 200, 48)

local BG = rgba(22, 25, 32)
local BORDER = rgba(255, 255, 255, 22)
local TEXT_HI = rgba(232, 236, 244)
local TEXT_LO = rgba(140, 148, 164)
local ACCENT = rgba(96, 165, 250)
local USER_BG = rgba(37, 64, 110)
local BOT_BG = rgba(34, 38, 48)
local TOOL_BG = rgba(64, 48, 24)
local ERROR = rgba(248, 113, 113)
local OK = rgba(74, 222, 128)

local MSG_SIZE = 18
local MAX_DRAFT = 8000

-- ---------------------------------------------------------------- cells

local base_url = page.input("base_url", "http://localhost:11434/v1", { agent = true })
local model = page.input("model", "llama3.2", { agent = true })
local draft = page.input("draft", "", { agent = true })
local ime = page.input("ime", "")
local focused = page.input("focused", false)
local request = page.input("request", nil)

-- the stream: each item is the whole reply so far (latest wins)
local chat = page.stream("chat", "cloud.chat", { request = request, latest = true })

-- the conversation as display blocks: a pure function of the stream
local transcript = page.derive("transcript", { chat }, function(c)
	local last = c and c.last
	local blocks = {}
	if not last then
		return blocks
	end
	for _, m in last.messages or {} do
		if m.role == "user" then
			table.insert(blocks, { who = "你", text = m.content or "", bg = USER_BG })
		elseif m.role == "assistant" then
			table.insert(blocks, { who = "助理", text = m.content or "", bg = BOT_BG })
		end
	end
	if (last.text or "") ~= "" or not last.done then
		local t = last.text or ""
		if t == "" and not last.done then
			t = "…"
		end
		table.insert(blocks, { who = "助理", text = t, bg = BOT_BG, live = not last.done })
	end
	for _, call in last.tool_calls or {} do
		table.insert(blocks, { who = "工具呼叫 " .. (call.name or "?"), text = call.arguments or "", bg = TOOL_BG })
	end
	if last.error then
		table.insert(blocks, { who = "錯誤", text = last.error, bg = rgba(80, 28, 32) })
	end
	return blocks
end)

-- ---------------------------------------------------------------- helpers

local function contains(r, x, y)
	return x >= r[1] and x <= r[1] + r[3] and y >= r[2] and y <= r[2] + r[4]
end

local function world(panel, r)
	return { panel[1] + r[1], panel[2] + r[2], r[3], r[4] }
end

local function frame(d)
	d:rrect(0, 0, d.w, d.h, 14, BG)
	d:stroke(0.5, 0.5, d.w - 1, d.h - 1, 14, 1, BORDER)
end

local function button(d, r, label, accent, on)
	d:rrect(r[1], r[2], r[3], r[4], 10, if on then alpha(accent, 90) else alpha(accent, 34))
	d:stroke(r[1], r[2], r[3], r[4], 10, 1, alpha(accent, 140))
	local w = page.measure(label, 16)
	d:text(r[1] + (r[3] - w) / 2, r[2] + r[4] / 2 - 11, 16, TEXT_HI, label)
end

-- The OpenAI key goes to OpenAI's own endpoint only: compare the scheme and
-- host, never a substring (base_url is agent-writable, and
-- "http://127.0.0.1:9/api.openai.com/v1" must not get the key).
local function needs_key(url)
	local u = string.lower(url)
	return u == "https://api.openai.com" or string.match(u, "^https://api%.openai%.com[/?]") ~= nil
end

local function caret_rect(s)
	local x = text.caret(s, 20, #s)
	local f = world(INPUT, FIELD)
	return { f[1] + 14 + x, f[2] + 12, 2, 28 }
end

-- ---------------------------------------------------------------- panels

page.panel({
	name = "header",
	rect = HEADER,
	class = "push",
	inputs = { base_url, model, chat },
	draw = function(d, url, m, c)
		frame(d)
		d:text(20, 12, 26, TEXT_HI, "雲端串流聊天")
		local allowed = cloud.check_url(url)
		local where = url .. "  ·  " .. m
		if needs_key(url) then
			where ..= "  ·  金鑰: secret \"openai\""
		end
		d:text(20, 52, 15, if allowed.ok then TEXT_LO else ERROR, if allowed.ok then where else (url .. " — " .. allowed.error))
		local last = c and c.last
		local status, col = "尚未送出", TEXT_LO
		if last then
			if last.error then
				status, col = "錯誤", ERROR
			elseif last.cancelled then
				status, col = "已停止", TEXT_LO
			elseif not last.done then
				status, col = string.format("串流中 · %d 位元組", #(last.text or "")), ACCENT
			else
				status = "完成"
				if last.finish_reason then
					status ..= " · " .. last.finish_reason
				end
				if last.usage and last.usage.total_tokens then
					status ..= string.format(" · %d tokens", math.floor(last.usage.total_tokens))
				end
				if last.first_token_ms then
					status ..= string.format(" · 首字 %d ms", math.floor(last.first_token_ms))
				end
				col = OK
			end
		end
		local w = page.measure(status, 16)
		d:rrect(d.w - w - 44, 26, w + 24, 34, 17, alpha(col, 40))
		d:text(d.w - w - 32, 32, 16, col, status)
	end,
})

page.panel({
	name = "chat",
	rect = CHAT,
	class = "push",
	inputs = { transcript },
	draw = function(d, blocks)
		frame(d)
		d:clip(1, 1, d.w - 2, d.h - 2)
		if #blocks == 0 then
			d:text(24, 24, 18, TEXT_LO, "在下方輸入訊息，按 Enter 送出。回覆會一個字一個字串流進來。")
			d:text(24, 56, 15, TEXT_LO, "預設連到本機 Ollama（http://localhost:11434/v1）；下方按鈕可切換到 LM Studio 或 OpenAI。")
		end
		-- newest at the bottom; lay out upwards until the panel is full
		local y = d.h - 16
		local first = math.max(1, #blocks - 30)
		for i = #blocks, first, -1 do
			local b = blocks[i]
			local user = b.who == "你"
			local bw = if user then d.w * 0.72 else d.w - 48
			local bx = if user then d.w - bw - 24 else 24
			local lay = text.layout(b.text, MSG_SIZE, bw - 28)
			local h = lay.height + 44
			y -= h
			d:rrect(bx, y, bw, h, 12, b.bg)
			d:text(bx + 14, y + 8, 13, if b.live then ACCENT else TEXT_LO, b.who .. (if b.live then " · 串流中" else ""))
			d:textbox(bx + 14, y + 30, bw - 28, lay.height + 4, MSG_SIZE, TEXT_HI, b.text)
			y -= 12
			if y < 0 then
				break
			end
		end
		d:unclip()
	end,
})

page.panel({
	name = "input",
	rect = INPUT,
	class = "push",
	priority = 1,
	inputs = { draft, ime, focused, chat },
	draw = function(d, s, pre, foc, c)
		frame(d)
		local f = FIELD
		d:rrect(f[1], f[2], f[3], f[4], 10, rgba(14, 16, 21))
		d:stroke(f[1], f[2], f[3], f[4], 10, if foc then 2 else 1, if foc then ACCENT else BORDER)
		d:clip(f[1] + 4, f[2], f[3] - 8, f[4])
		if s == "" and pre == "" then
			d:text(f[1] + 14, f[2] + 13, 20, TEXT_LO, if foc then "" else "點這裡輸入訊息…")
		end
		d:text(f[1] + 14, f[2] + 13, 20, TEXT_HI, s)
		local cx = f[1] + 14 + text.caret(s, 20, #s)
		if pre ~= "" then
			-- IME composition, underlined, at the caret
			local pw = page.measure(pre, 20)
			d:text(cx, f[2] + 13, 20, ACCENT, pre)
			d:rect(cx, f[2] + 40, pw, 2, ACCENT)
			cx += pw
		end
		if foc then
			d:rect(cx, f[2] + 12, 2, 28, ACCENT)
		end
		d:unclip()
		local busy = c and c.last and not c.last.done
		button(d, SEND, "送出", ACCENT, false)
		button(d, STOP, "停止", ERROR, busy)
	end,
})

page.panel({
	name = "presets",
	rect = PRESETS,
	class = "static",
	inputs = { base_url },
	draw = function(d, url)
		frame(d)
		for k, p in PRESET_LIST do
			button(d, R(16 + (k - 1) * (PRESET_W + 12), 14, PRESET_W, 48), p.label, OK, url == p.base)
		end
		button(d, CLEAR, "清除對話", TEXT_LO, false)
	end,
})

-- ---------------------------------------------------------------- behaviour

local turns = 0
local fresh = true

local function focus(on)
	page.set(focused, on)
	page.focus_text(if on then caret_rect(page.get(draft)) else nil)
end

local function send()
	local s = page.get(draft)
	if string.match(s, "^%s*$") then
		return false
	end
	turns += 1
	local url = page.get(base_url)
	local messages = { { role = "user", content = s } }
	if fresh then
		table.insert(messages, 1, { role = "system", content = "你是 AnyPage 示範頁裡的助理，回答簡短、直接。" })
		fresh = false
	end
	page.set(request, {
		base_url = url,
		model = page.get(model),
		messages = messages,
		keep_history = true,
		temperature = 0.7,
		secret = if needs_key(url) then "openai" else nil,
		turn = turns,
	})
	page.set(draft, "")
	page.set(ime, "")
	if page.get(focused) then
		page.focus_text(caret_rect(""))
	end
	return true
end

local function stop()
	page.set(request, nil)
end

local function clear()
	turns += 1
	fresh = true
	page.set(request, { base_url = page.get(base_url), keep_history = true, reset = true, turn = turns })
end

local function type_text(t)
	local s = page.get(draft) .. t
	if #s > MAX_DRAFT then
		return
	end
	page.set(draft, s)
	page.focus_text(caret_rect(s))
end

page.on("pointer_down", function(x, y)
	if contains(world(INPUT, FIELD), x, y) then
		focus(true)
		return "handled"
	end
	if page.get(focused) then
		focus(false)
	end
	if contains(world(INPUT, SEND), x, y) then
		send()
		return "handled"
	end
	if contains(world(INPUT, STOP), x, y) then
		stop()
		return "handled"
	end
	for k, p in PRESET_LIST do
		if contains(world(PRESETS, R(16 + (k - 1) * (PRESET_W + 12), 14, PRESET_W, 48)), x, y) then
			page.set(base_url, p.base)
			page.set(model, p.model)
			return "handled"
		end
	end
	if contains(world(PRESETS, CLEAR), x, y) then
		clear()
		return "handled"
	end
	return nil
end)

page.on("text", function(t)
	if page.get(focused) then
		type_text(t)
		return true
	end
	return false
end)

page.on("paste", function(t)
	if page.get(focused) then
		type_text(t)
		return true
	end
	return false
end)

page.on("ime", function(t)
	page.set(ime, t)
	return true
end)

page.on("key", function(k)
	if not page.get(focused) then
		return false
	end
	if k == "enter" then
		send()
	elseif k == "backspace" then
		local s = page.get(draft)
		local cut = text.prev(s, #s)
		page.set(draft, string.sub(s, 1, cut))
		page.focus_text(caret_rect(string.sub(s, 1, cut)))
	elseif k == "escape" then
		focus(false)
	elseif k == "ctrl+v" then
		page.paste()
	else
		return false
	end
	return true
end)

-- ---------------------------------------------------------------- the semantic port

page.action({
	name = "send",
	description = "Send a chat message to the configured model",
	params = { text = "string" },
	run = function(p)
		page.set(draft, tostring(p.text or ""))
		return { sent = send(), turn = turns }
	end,
})

page.action({ name = "stop", description = "Stop the reply being streamed", run = function()
	stop()
	return { stopped = true }
end })

page.action({ name = "clear", description = "Start a new conversation", run = function()
	clear()
	return { cleared = true }
end })

page.action({
	name = "use_endpoint",
	description = "Point the chat at an OpenAI-compatible endpoint",
	params = { base_url = "string", model = "string" },
	run = function(p)
		page.set(base_url, tostring(p.base_url))
		page.set(model, tostring(p.model or page.get(model)))
		return { ok = cloud.check_url(tostring(p.base_url)).ok }
	end,
})
