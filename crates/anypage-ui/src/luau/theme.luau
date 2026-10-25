-- ui/theme: colour and metric tokens for the component library.
--
-- A theme is a plain table. Every widget reads its colours and sizes from
-- it, so a page can restyle everything with ui.theme({ accent = ... }, base).

local rgba, alpha = color.rgba, color.alpha

local M = {}

local function metrics(t)
	-- type scale
	t.font = t.font or 15
	t.font_small = t.font_small or 13
	t.font_large = t.font_large or 19
	t.font_title = t.font_title or 24
	-- shapes and spacing
	t.radius = t.radius or 8
	t.radius_small = t.radius_small or 5
	t.control_h = t.control_h or 36
	t.row_h = t.row_h or 32
	t.pad = t.pad or 12
	t.gap = t.gap or 8
	t.scrollbar = t.scrollbar or 8
	t.focus_width = t.focus_width or 2
	-- behaviour
	t.wheel_step = t.wheel_step or 40
	t.caret_blink = t.caret_blink or 0.53
	t.tooltip_delay = t.tooltip_delay or 0.5
	t.toast_seconds = t.toast_seconds or 3
	t.drag_slop = t.drag_slop or 8
	t.double_click = t.double_click or 0.4
	return t
end

M.dark = metrics({
	name = "dark",
	bg = rgba(16, 18, 23),
	surface = rgba(26, 29, 37),
	surface2 = rgba(35, 39, 49),
	surface3 = rgba(48, 53, 66),
	border = rgba(255, 255, 255, 30),
	border_strong = rgba(255, 255, 255, 70),
	text = rgba(232, 236, 244),
	text_dim = rgba(152, 160, 178),
	text_faint = rgba(104, 112, 128),
	accent = rgba(92, 152, 255),
	accent_hover = rgba(120, 172, 255),
	accent_press = rgba(64, 124, 228),
	accent_soft = rgba(92, 152, 255, 48),
	on_accent = rgba(255, 255, 255),
	danger = rgba(236, 80, 92),
	danger_hover = rgba(248, 104, 114),
	on_danger = rgba(255, 255, 255),
	success = rgba(52, 196, 124),
	warning = rgba(246, 176, 56),
	selection = rgba(92, 152, 255, 96),
	row_hover = rgba(255, 255, 255, 14),
	row_selected = rgba(92, 152, 255, 56),
	row_stripe = rgba(255, 255, 255, 6),
	focus = rgba(120, 172, 255),
	track = rgba(255, 255, 255, 34),
	thumb = rgba(255, 255, 255, 76),
	scrim = rgba(0, 0, 0, 150),
	shadow = rgba(0, 0, 0, 110),
	tooltip_bg = rgba(236, 239, 245),
	tooltip_text = rgba(20, 22, 28),
	knob = rgba(255, 255, 255),
	disabled_alpha = 96,
})

M.light = metrics({
	name = "light",
	bg = rgba(244, 245, 248),
	surface = rgba(255, 255, 255),
	surface2 = rgba(238, 240, 245),
	surface3 = rgba(224, 228, 236),
	border = rgba(20, 24, 36, 34),
	border_strong = rgba(20, 24, 36, 90),
	text = rgba(24, 27, 34),
	text_dim = rgba(92, 100, 116),
	text_faint = rgba(140, 147, 160),
	accent = rgba(36, 104, 228),
	accent_hover = rgba(58, 122, 240),
	accent_press = rgba(24, 84, 196),
	accent_soft = rgba(36, 104, 228, 34),
	on_accent = rgba(255, 255, 255),
	danger = rgba(212, 48, 64),
	danger_hover = rgba(228, 70, 84),
	on_danger = rgba(255, 255, 255),
	success = rgba(22, 150, 88),
	warning = rgba(200, 128, 16),
	selection = rgba(36, 104, 228, 70),
	row_hover = rgba(20, 24, 36, 10),
	row_selected = rgba(36, 104, 228, 40),
	row_stripe = rgba(20, 24, 36, 5),
	focus = rgba(36, 104, 228),
	track = rgba(20, 24, 36, 36),
	thumb = rgba(20, 24, 36, 70),
	scrim = rgba(10, 14, 22, 110),
	shadow = rgba(10, 14, 22, 50),
	tooltip_bg = rgba(30, 33, 40),
	tooltip_text = rgba(240, 242, 246),
	knob = rgba(255, 255, 255),
	disabled_alpha = 110,
})

-- A theme from overrides on top of a base theme (dark by default).
function M.make(overrides, base)
	local t = table.clone(base or M.dark)
	if overrides then
		for k, v in overrides do
			t[k] = v
		end
	end
	return metrics(t)
end

-- The colour for a tone name: "accent", "success", "warning", "danger", "neutral".
function M.tone(th, tone)
	if tone == "success" then
		return th.success
	elseif tone == "warning" then
		return th.warning
	elseif tone == "danger" then
		return th.danger
	elseif tone == "neutral" or tone == nil then
		return th.text_dim
	end
	return th.accent
end

-- Fade a colour for disabled widgets.
function M.dim(th, c)
	local a = c % 256
	return alpha(c, math.floor(a * th.disabled_alpha / 255))
end

return M
