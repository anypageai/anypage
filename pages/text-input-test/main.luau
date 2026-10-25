-- Text input test: a hand-written single-line text field and an event log,
-- for checking a host's native text input (Android InputConnection, desktop
-- IME), clipboard, file dialogs, dropped files and the wheel.
--
-- The field is not a UI-library widget on purpose: it uses only the engine's
-- own events, so it shows exactly what the host delivers.
--
--   pointer_down   tap the field to focus it (the host shows the keyboard),
--                  tap elsewhere to unfocus (the host hides it)
--   text           committed text (typing, IME commit) inserted at the caret
--   ime            composition in progress, drawn underlined at the caret
--   key            backspace/delete/left/right/home/end (grapheme-correct),
--                  ctrl+left/right and ctrl+backspace by word, enter submits,
--                  escape unfocuses, ctrl+c/x/v use the clipboard
--   paste          clipboard text the page asked for with page.paste()
--   file_drop      a file dropped on the page
--   wheel          scrolls the event log (also drag it)
--
-- While the field has focus the page keeps page.focus_text(caret rect)
-- current, so the host can show the keyboard and place the IME window.
--
-- Robustness: an agent may set the "text" cell to any JSON value; the page
-- treats numbers and booleans as their text and anything else as empty.
-- Input longer than MAX_INPUT (one commit, paste or agent call) is refused
-- before any string work, so a 4 MB paste ends in "too long", not in a
-- handler timeout. page.log (logcat on Android) gets lengths, never the
-- typed text; only the on-screen event log shows it, shortened.

local rgba, alpha = color.rgba, color.alpha
local floor, max, min = math.floor, math.max, math.min

-- ---------------------------------------------------------------- layout (world units)

local W = 720
local TITLE = { 0, 0, W, 70 }
local FIELD = { 0, 84, W, 96 }
local STATUS = { 0, 192, W, 56 }
local BUTTONS = { 0, 260, W, 72 }
local FILE = { 0, 344, W, 60 }
local LOG = { 0, 416, W, 444 }

local SIZE = 30 -- field text size
local PAD = 20 -- field padding
local LINE = 28 -- log line height
local LOG_TOP = 52 -- log header height
local MAX_TEXT = 2000 -- bytes in the field
local MAX_INPUT = 16 * 1024 -- bytes of one commit, paste or agent text
local MAX_LINES = 50 -- lines (submits) in one input
local SHOWN = 80 -- bytes of typed text in one on-screen log line
local MAX_LOG = 80

local BUTTON_LABELS = { "Copy", "Paste", "Open…", "Save…", "Clear" }
local BUTTON_GAP = 10
local BUTTON_W = (W - BUTTON_GAP * (#BUTTON_LABELS - 1)) / #BUTTON_LABELS

local BG = rgba(22, 25, 32)
local FIELD_BG = rgba(12, 14, 19)
local BORDER = rgba(70, 78, 96)
local ACCENT = rgba(96, 156, 255)
local PRE = rgba(255, 196, 92)
local FG = rgba(236, 240, 248)
local DIM = rgba(140, 148, 164)
local WHITE = rgba(255, 255, 255)

local function inside(r, x, y)
	return x >= r[1] and x < r[1] + r[3] and y >= r[2] and y < r[2] + r[4]
end

-- ---------------------------------------------------------------- state (input cells)

local Text = page.input("text", "", { agent = true })
local Caret = page.input("caret", 0) -- byte offset into text, 0..#text
local Pre = page.input("preedit", "")
local Focus = page.input("focus", false)
local Log = page.input("log", { "ready: tap the field and type" })
local Scroll = page.input("scroll", 0) -- log lines scrolled back from the newest
local FileMsg = page.input("file", "no file yet")

local LINE_H = text.line_height(SIZE)
local LOG_ROWS = floor((LOG[4] - LOG_TOP - 8) / LINE)

-- ---------------------------------------------------------------- helpers

-- the field's text as a string, whatever an agent stored in the cell
local function as_text(v)
	local t = type(v)
	if t == "string" then
		return v
	elseif t == "number" or t == "boolean" then
		return tostring(v)
	end
	return ""
end

-- text and caret as the field uses them: a string of at most MAX_TEXT
-- bytes, and a caret on a grapheme boundary within it
local function field_of(s, c)
	s = as_text(s)
	if #s > MAX_TEXT then
		s = string.sub(s, 1, text.prev(s, MAX_TEXT + 1))
	end
	c = tonumber(c) or #s
	c = min(max(floor(c), 0), #s)
	if c > 0 and c < #s then
		c = text.prev(s, text.next(s, c))
	end
	return s, c
end

-- the field's current text, caret and composition (handlers only: an
-- agent's non-string text is written back as the string the field shows)
local function state()
	local raw, rc = page.get(Text), page.get(Caret)
	local s, c = field_of(raw, rc)
	if raw ~= s then
		page.set(Text, s)
	end
	if rc ~= c then
		page.set(Caret, c)
	end
	return s, c, as_text(page.get(Pre))
end

-- typed text for the on-screen log: at most SHOWN bytes, cut between graphemes
local function shown(s)
	if #s <= SHOWN then
		return s
	end
	return string.sub(s, 1, text.prev(s, SHOWN + 1)) .. "… (" .. #s .. " bytes)"
end

-- `msg` goes to the on-screen log; `logged` (default `msg`) to page.log,
-- which must not carry typed text
local function add(msg, logged)
	local log = page.get(Log)
	if type(log) ~= "table" then
		log = {}
	end
	table.insert(log, msg)
	while #log > MAX_LOG do
		table.remove(log, 1)
	end
	page.set(Log, log)
	page.log(logged or msg)
end

-- how far the field's text is scrolled left so the caret stays visible
local function scroll_x(s, caret, pre)
	local inner = FIELD[3] - 2 * PAD
	local cx = text.width(string.sub(s, 1, caret) .. pre, SIZE)
	return max(0, cx - inner + 4)
end

-- tell the host where the caret is while the field has focus
local function refresh_focus()
	if not page.get(Focus) then
		page.focus_text(nil)
		return
	end
	local s, c, pre = state()
	local cx = text.width(string.sub(s, 1, c) .. pre, SIZE) - scroll_x(s, c, pre)
	local y = FIELD[2] + (FIELD[4] - LINE_H) / 2
	page.focus_text({ FIELD[1] + PAD + cx, y, 2, LINE_H })
end

local function set_focus(on)
	if page.get(Focus) == on then
		return
	end
	page.set(Focus, on)
	if not on then
		page.set(Pre, "")
	end
	add(on and "focus: on" or "focus: off")
	refresh_focus()
end

local function set_text(s, c)
	page.set(Text, s)
	page.set(Caret, c)
	refresh_focus()
end

local function submit()
	local s = state()
	add("enter: " .. shown(s), "enter: " .. #s .. " bytes")
	set_text("", 0)
end

-- insert at the caret; a newline submits (this is a single-line field).
-- Returns false (and logs why) when the input is refused.
local function insert(t)
	if type(t) ~= "string" then
		add("input refused: not text")
		return false
	end
	-- sizes first: no string work on a megabyte of input
	if #t > MAX_INPUT then
		add("input too long: " .. #t .. " bytes (at most " .. MAX_INPUT .. ")")
		return false
	end
	t = string.gsub(t, "\r", "")
	local _, lines = string.gsub(t, "\n", "")
	if lines > MAX_LINES then
		add("input refused: " .. lines .. " lines (at most " .. MAX_LINES .. ")")
		return false
	end
	local first = true
	for part in string.gmatch(t .. "\n", "([^\n]*)\n") do
		if not first then
			submit()
		end
		first = false
		if #part > 0 then
			local s, c = state()
			if #s + #part > MAX_TEXT then
				add("field full: " .. MAX_TEXT .. " bytes")
				return false
			end
			set_text(string.sub(s, 1, c) .. part .. string.sub(s, c + 1), c + #part)
		end
	end
	return true
end

local function delete_range(a, b)
	local s = state()
	if b > a then
		set_text(string.sub(s, 1, a) .. string.sub(s, b + 1), a)
	end
end

local function copy_all(cut)
	local s = state()
	page.copy(s)
	add((cut and "cut: " or "copy: ") .. #s .. " bytes")
	if cut then
		set_text("", 0)
	end
end

local function open_file()
	add("open: asking the host")
	page.open_file({
		filters = { "txt" },
		done = function(f)
			local msg
			if f.cancelled then
				msg = "open cancelled"
			elseif f.error then
				msg = "open failed: " .. f.error
			else
				msg = "opened " .. shown(as_text(f.name)) .. " (" .. tostring(f.size) .. " bytes)"
			end
			page.set(FileMsg, msg)
			add(msg)
		end,
	})
end

local function save_file()
	local s = state()
	add("save: " .. #s .. " bytes")
	page.save_file({
		name = "anypage-note.txt",
		data = s,
		done = function(r)
			local msg
			if r.saved then
				msg = "saved " .. shown(as_text(r.saved)) .. " (" .. #s .. " bytes)"
			elseif r.cancelled then
				msg = "save cancelled"
			else
				msg = "save failed: " .. tostring(r.error)
			end
			page.set(FileMsg, msg)
			add(msg)
		end,
	})
end

local function press(i)
	local label = BUTTON_LABELS[i]
	if label == "Copy" then
		copy_all(false)
	elseif label == "Paste" then
		add("paste: asking the host")
		page.paste()
	elseif label == "Open…" then
		open_file()
	elseif label == "Save…" then
		save_file()
	else
		set_text("", 0)
		page.set(Pre, "")
		add("cleared")
	end
end

local function set_scroll(v)
	local n = #page.get(Log)
	page.set(Scroll, max(0, min(floor(v + 0.5), max(0, n - LOG_ROWS))))
end

-- ctrl+shift+c: copy 3 MB, more than the clipboard takes, to check that the
-- host refuses it instead of crashing (the Android host allows 256 KB)
local BIG_COPY = 3 * 1024 * 1024

-- ---------------------------------------------------------------- events

page.on("pointer_down", function(x, y)
	if inside(FIELD, x, y) then
		local s, c, pre = state()
		local local_x = x - FIELD[1] - PAD + scroll_x(s, c, pre)
		set_focus(true)
		page.set(Caret, text.hit(s, SIZE, local_x))
		refresh_focus()
		return "handled"
	end
	if inside(BUTTONS, x, y) then
		local i = floor((x - BUTTONS[1]) / (BUTTON_W + BUTTON_GAP)) + 1
		if i >= 1 and i <= #BUTTON_LABELS then
			press(i)
		end
		return "handled"
	end
	if page.get(Focus) then
		set_focus(false)
	end
	return false -- elsewhere: the engine pans
end)

-- drag the log
local drag = nil
page.on("pointer_down", function(x, y)
	if inside(LOG, x, y) then
		drag = { y = y, scroll = page.get(Scroll) }
		return "captured"
	end
	return false
end)
page.on("pointer_drag", function(x, y)
	if drag then
		set_scroll(drag.scroll + (y - drag.y) / LINE)
	end
end)
page.on("pointer_up", function()
	drag = nil
end)

page.on("text", function(t)
	if not page.get(Focus) then
		return false
	end
	-- the length first: shown() and insert() look at the text only when it is small
	if #t <= MAX_INPUT then
		add("text: " .. shown(t), "text: " .. #t .. " bytes")
	end
	page.set(Pre, "")
	insert(t)
	return true
end)

page.on("ime", function(t)
	if not page.get(Focus) then
		return false
	end
	if #t > MAX_INPUT then
		add("composition too long: " .. #t .. " bytes")
		t = ""
	end
	if t ~= as_text(page.get(Pre)) then
		if #t > 0 then
			add("ime: " .. shown(t), "ime: " .. #t .. " bytes")
		else
			add("ime: (end)")
		end
	end
	page.set(Pre, t)
	refresh_focus()
	return true
end)

page.on("paste", function(t)
	add("paste: " .. #t .. " bytes")
	if page.get(Focus) then
		insert(t)
	end
	return true
end)

page.on("key", function(k)
	if not page.get(Focus) then
		if k == "ctrl+v" or k == "enter" then
			set_focus(true)
		else
			return false
		end
	end
	add("key: " .. k)
	local s, c = state()
	if k == "backspace" then
		delete_range(text.prev(s, c), c)
	elseif k == "ctrl+backspace" then
		delete_range(text.prev_word(s, c), c)
	elseif k == "delete" then
		delete_range(c, text.next(s, c))
	elseif k == "ctrl+delete" then
		delete_range(c, text.next_word(s, c))
	elseif k == "left" then
		page.set(Caret, text.prev(s, c))
	elseif k == "right" then
		page.set(Caret, text.next(s, c))
	elseif k == "ctrl+left" then
		page.set(Caret, text.prev_word(s, c))
	elseif k == "ctrl+right" then
		page.set(Caret, text.next_word(s, c))
	elseif k == "home" or k == "up" then
		page.set(Caret, 0)
	elseif k == "end" or k == "down" then
		page.set(Caret, #s)
	elseif k == "enter" then
		submit()
	elseif k == "escape" then
		set_focus(false)
	elseif k == "ctrl+c" then
		copy_all(false)
	elseif k == "ctrl+x" then
		copy_all(true)
	elseif k == "ctrl+shift+c" then
		page.copy(string.rep("x", BIG_COPY))
		add("copy: " .. BIG_COPY .. " bytes (over the clipboard limit)")
	elseif k == "ctrl+v" then
		page.paste()
	elseif k == "ctrl+o" then
		open_file()
	elseif k == "ctrl+s" then
		save_file()
	else
		return false
	end
	refresh_focus()
	return true
end)

page.on("file_drop", function(f)
	local msg = "dropped " .. shown(as_text(f.name)) .. " (" .. tostring(f.size) .. " bytes)"
	page.set(FileMsg, msg)
	add(msg)
	return true
end)

page.on("wheel", function(x, y, dx, dy)
	if not inside(LOG, x, y) then
		return false -- the engine zooms
	end
	set_scroll(page.get(Scroll) + dy)
	page.log("wheel: " .. dx .. " " .. dy .. " -> scroll " .. page.get(Scroll))
	return true
end)

-- the semantic port: agents can type and clear
page.action({
	name = "type",
	description = "Insert text at the caret of the field (at most 16 KB)",
	params = { type = "object", properties = { text = { type = "string" } }, required = { "text" } },
	run = function(p)
		local t = type(p) == "table" and p.text or nil
		if type(t) ~= "string" then
			return { error = "text must be a string" }
		end
		if not insert(t) then
			return { error = "refused: see the event log", text = (state()) }
		end
		return { text = (state()) }
	end,
})
page.action({
	name = "clear",
	description = "Empty the field",
	run = function()
		set_text("", 0)
		return { text = "" }
	end,
})

-- ---------------------------------------------------------------- panels

local function frame(d, title)
	d:rrect(0, 0, d.w, d.h, 12, BG)
	d:stroke(0.5, 0.5, d.w - 1, d.h - 1, 12, 1, alpha(WHITE, 18))
	if title then
		d:text(16, 12, 20, DIM, title)
	end
end

page.panel({
	rect = TITLE,
	name = "title",
	draw = function(d)
		d:text(4, 4, 34, FG, "文字輸入測試 · Text input")
		d:text(6, 46, 18, DIM, "tap the field, type, compose (IME), paste, open, save, scroll the log")
	end,
})

page.panel({
	rect = FIELD,
	name = "field",
	inputs = { Text, Caret, Pre, Focus },
	draw = function(d, s, c, pre, focus)
		s, c = field_of(s, c)
		pre = as_text(pre)
		d:rrect(0, 0, d.w, d.h, 12, FIELD_BG)
		d:stroke(1, 1, d.w - 2, d.h - 2, 12, focus and 3 or 1.5, focus and ACCENT or BORDER)
		local y = (d.h - LINE_H) / 2
		d:clip(PAD - 4, 0, d.w - 2 * PAD + 8, d.h)
		if #s == 0 and #pre == 0 then
			d:text(PAD, y, SIZE, alpha(DIM, 150), focus and "type here…" or "tap here to type")
		end
		local x = PAD - scroll_x(s, c, pre)
		local before, after = string.sub(s, 1, c), string.sub(s, c + 1)
		local wb = text.width(before, SIZE)
		local wp = text.width(pre, SIZE)
		d:text(x, y, SIZE, FG, before)
		if #pre > 0 then
			d:rect(x + wb, y + LINE_H - 3, wp, 3, PRE)
			d:text(x + wb, y, SIZE, PRE, pre)
		end
		d:text(x + wb + wp, y, SIZE, FG, after)
		if focus then
			d:rect(x + wb + wp, y, 3, LINE_H, ACCENT)
		end
		d:unclip()
	end,
})

page.panel({
	rect = STATUS,
	name = "status",
	inputs = { Text, Caret, Pre, Focus },
	draw = function(d, s, c, pre, focus)
		s, c = field_of(s, c)
		pre = as_text(pre)
		frame(d)
		local line = string.format(
			"focus %s · caret %d / %d bytes · %d graphemes · preedit “%s”",
			focus and "on" or "off",
			c,
			#s,
			text.graphemes(s),
			shown(pre)
		)
		d:clip(0, 0, d.w, d.h)
		d:text(16, 16, 20, focus and FG or DIM, line)
		d:unclip()
	end,
})

page.panel({
	rect = BUTTONS,
	name = "buttons",
	draw = function(d)
		for i, label in ipairs(BUTTON_LABELS) do
			local x = (i - 1) * (BUTTON_W + BUTTON_GAP)
			d:rrect(x, 0, BUTTON_W, d.h, 12, rgba(36, 44, 62))
			d:stroke(x + 0.5, 0.5, BUTTON_W - 1, d.h - 1, 12, 1, alpha(ACCENT, 120))
			local tw = text.width(label, 24)
			d:text(x + (BUTTON_W - tw) / 2, (d.h - text.line_height(24)) / 2, 24, FG, label)
		end
	end,
})

page.panel({
	rect = FILE,
	name = "file",
	inputs = { FileMsg },
	draw = function(d, msg)
		frame(d)
		d:clip(0, 0, d.w, d.h)
		d:text(16, 16, 22, FG, "file: " .. msg)
		d:unclip()
	end,
})

page.panel({
	rect = LOG,
	name = "log",
	inputs = { Log, Scroll },
	draw = function(d, log, scroll)
		frame(d, string.format("events (%d, scrolled back %d) — wheel or drag to scroll", #log, scroll))
		local last = #log - scroll
		local first = max(1, last - LOG_ROWS + 1)
		d:clip(8, LOG_TOP - 4, d.w - 16, d.h - LOG_TOP)
		for i = first, last do
			local y = LOG_TOP + (i - first) * LINE
			d:text(16, y, 20, i == #log and FG or DIM, log[i])
		end
		d:unclip()
	end,
})
