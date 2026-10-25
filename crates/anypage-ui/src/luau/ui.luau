-- ui: the AnyPage component library for page scripts.
--
--   local ui = import("ui")
--   local app
--   app = ui.app({
--       rect = { 0, 0, 800, 600 }, name = "settings",
--       state = { count = 0, name = "" },
--       inputs = { some_cell },               -- extra cells the view reads
--       theme = ui.themes.dark,
--       view = function(s, some)
--           return ui.column({ gap = 8, padding = 16 }, {
--               ui.text("Hello " .. s.name, { size = 20 }),
--               ui.button("Add", { on_click = function() app:update(function(st) st.count += 1 end) end }),
--               ui.textfield({ value = s.name, on_change = function(v) app:set("name", v) end }),
--           })
--       end,
--   })
--
-- State lives in cells, the view is a pure function of them, handlers are
-- commands (app:set / app:update). Every callback also receives the app as its
-- last argument, so `on_click = function(app) app:set(...) end` works without
-- a forward-declared local.

local Theme = import("ui/theme")
local L = import("ui/layout")
local A = import("ui/app")
-- these register their element types into the layout registry
import("ui/controls")
local F = import("ui/fields")
import("ui/views")

local ui = {}

ui.themes = { dark = Theme.dark, light = Theme.light }

-- Size limits of text editing, in bytes: { field, area, undo_steps, undo_bytes }.
ui.limits = table.freeze(table.clone(F.limits))

-- A theme: overrides on top of a base theme (dark by default).
function ui.theme(overrides, base)
	return Theme.make(overrides, base)
end

ui.app = A.app

-- ---------------------------------------------------------------- helpers

local function is_list(t)
	return type(t) == "table" and t.type == nil and (t[1] ~= nil or next(t) == nil)
end

-- (opts, children) or (children): both forms are accepted by containers.
local function container(kind, opts, children)
	if children == nil and is_list(opts) then
		children, opts = opts, nil
	end
	local e = if opts then table.clone(opts) else {}
	e.type = kind
	e.children = L.kids(children or e.children)
	return e
end

local function element(kind, opts)
	local e = if opts then table.clone(opts) else {}
	e.type = kind
	return e
end

-- (label, opts) or (opts) with opts.label
local function labelled(kind, a, b)
	local e
	if type(a) == "table" then
		e = element(kind, a)
	else
		e = element(kind, b)
		e.label = a
	end
	if e.label ~= nil then
		e.label = tostring(e.label)
	end
	return e
end

-- ---------------------------------------------------------------- layout

-- Children top to bottom. opts: gap, padding, align, justify, bg, radius, border.
function ui.column(opts, children)
	return container("column", opts, children)
end

-- Children left to right; cross-axis align defaults to "center".
function ui.row(opts, children)
	return container("row", opts, children)
end

-- Children on top of each other (align: start | center | end | stretch).
function ui.stack(opts, children)
	return container("stack", opts, children)
end

-- Equal columns: opts.columns = n, or opts.min_col (auto count); gap, row_gap, col_gap.
function ui.grid(opts, children)
	return container("grid", opts, children)
end

-- A raised surface; opts.title adds a heading.
function ui.card(opts, children)
	if children == nil and is_list(opts) then
		children, opts = opts, nil
	end
	local e = container("card", opts, children)
	if e.title ~= nil then
		table.insert(e.children, 1, { type = "text", text = tostring(e.title), size = e.title_size or 18 })
	end
	return e
end

-- Free space: grows by default; ui.spacer(n) is a fixed gap of n.
function ui.spacer(size)
	if type(size) == "number" then
		return { type = "spacer", size = size }
	end
	local e = element("spacer", size)
	if e.size == nil and e.grow == nil then
		e.grow = 1
	end
	return e
end

-- A line across a column (or down a row).
function ui.divider(opts)
	return element("divider", opts)
end

-- A scroll container around one child (wheel, drag, scrollbar).
-- opts: height, horizontal, padding.
function ui.scroll(opts, child)
	if child == nil and type(opts) == "table" and (opts.type ~= nil or is_list(opts)) then
		child, opts = opts, nil
	end
	local e = element("scroll", opts)
	local kids = L.kids(child)
	if #kids > 1 then
		kids = { { type = "column", gap = (opts and opts.gap) or 0, children = kids } }
	end
	e.children = kids
	return e
end

-- ---------------------------------------------------------------- content

-- Text that wraps to its width. opts: size, color, tone ("dim" | "faint" |
-- "accent" | "success" | "warning" | "danger"), align, wrap = false, max_lines.
function ui.text(s, opts)
	local e = element("text", opts)
	e.text = if s == nil then "" else tostring(s)
	return e
end

-- A small label above a control: ui.field("Name", ui.textfield{...}).
function ui.field(label, control, opts)
	local e = container("column", opts, { { type = "text", text = tostring(label), tone = "dim", size = 13 }, control })
	if e.gap == nil then
		e.gap = 6
	end
	return e
end

function ui.badge(s, opts)
	local e = element("badge", opts)
	e.text = tostring(s)
	return e
end

-- value in 0..1, or nil for an indeterminate (animated) bar.
function ui.progress(value, opts)
	if type(value) == "table" then
		return element("progress", value)
	end
	local e = element("progress", opts)
	e.value = value
	return e
end

-- An image value (a native module's image resource). opts: width, height,
-- aspect (w / h of the image, to letterbox), fit = "contain" | "fill", alt.
function ui.image(img, opts)
	local e = element("image", opts)
	e.image = img
	return e
end

-- Custom drawing: opts.draw(d, x, y, w, h, theme, t); optional on_down,
-- on_drag, on_up, on_click (app-local coordinates relative to the canvas).
function ui.canvas(opts)
	return element("canvas", opts)
end

-- ---------------------------------------------------------------- controls

-- opts: on_click, variant ("primary" | "secondary" | "ghost" | "danger"),
-- size = "small", disabled, tooltip.
function ui.button(label, opts)
	return labelled("button", label, opts)
end

-- opts: checked, on_change(checked), label, mixed, disabled.
function ui.checkbox(label, opts)
	return labelled("checkbox", label, opts)
end

function ui.switch(label, opts)
	return labelled("switch", label, opts)
end

-- A radio group. opts: options, selected (index), on_change(index, option), direction = "row".
function ui.radio(opts)
	return element("radio", opts)
end

-- opts: value, min, max, step, on_change(value), show_value.
function ui.slider(opts)
	return element("slider", opts)
end

-- One line of text. opts: value, on_change(text), on_submit(text),
-- placeholder, password, readonly, max_length, size, width.
function ui.textfield(opts)
	return element("textfield", opts)
end

-- Multi-line, wrapping, scrolling. opts: value, on_change, placeholder,
-- rows (visible lines), height, readonly, max_length.
function ui.textarea(opts)
	return element("textarea", opts)
end

-- A drop-down. opts: options (strings or { label = ... }), selected,
-- on_change(index, option), placeholder.
function ui.select(opts)
	return element("select", opts)
end

-- A tab bar. opts: tabs, selected, on_change(index); with content = { el, ... }
-- or content = function(index) the selected tab's content follows the bar.
function ui.tabs(opts)
	local bar = element("tabs", opts)
	local content = bar.content
	if content == nil then
		return bar
	end
	bar.content = nil
	local i = bar.selected or 1
	local body
	if type(content) == "function" then
		body = content(i)
	elseif type(content) == "table" then
		body = content[i]
	end
	local col = { type = "column", gap = bar.gap or 12, grow = bar.grow, width = bar.width, height = bar.height, id = bar.id and (bar.id .. "/tabs"), children = { bar } }
	bar.grow, bar.height = nil, nil
	if body ~= nil then
		col.children[2] = body
	end
	return col
end

-- A virtualised list. opts: items (a list) or count + item(i); row(item, i,
-- selected, hovered) -> element or string; key(item, i); row_height; height
-- or rows; selected; on_select(item, i); on_activate(item, i); striped; empty.
function ui.list(opts)
	return element("list", opts)
end

-- A table in the shared table format: opts.data = { columns, rows }, or lazy:
-- opts.columns + count + row(i) -> { v, v, ... } (rows built only when shown).
-- opts: widths (numbers, <= 1 means a fraction), height or rows, row_height,
-- selected, on_select(row, i), on_activate, sort = { col, desc },
-- on_sort(col, desc), format(value, col, row) -> string, align_cols, striped, empty.
function ui.table(opts)
	return element("table", opts)
end

-- A modal dialog over the app while opts.open is true. opts: title, body
-- (string or element), children, actions (buttons), on_close, width,
-- dismissible (default true: clicking outside or Escape calls on_close).
function ui.dialog(opts)
	local e = element("dialog", opts)
	if e.children ~= nil then
		e.children = L.kids(e.children)
	end
	return e
end

-- `el` when `cond` holds, else nothing (children lists skip it).
function ui.when(cond, el)
	if cond then
		return el
	end
	return nil
end

return ui
