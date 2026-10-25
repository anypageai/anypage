-- sample: the workspace's sample document, built with the pdf module.
--
-- Pages 1-3 (report, expenses, notes) are written with pdf.new, pdf.add_text
-- and pdf.add_path. The pdf module reads and fills forms but does not create
-- form fields, so page 4 starts as a small one-page PDF with an AcroForm
-- written here by hand and appended with pdf.merge; its labels are then
-- written with pdf.add_text like the rest.
--
-- crates/anypage-std/tests/page_pdf_agent.rs runs S.build() and writes the
-- result to assets/sample.pdf; the page opens that file (page.asset) and
-- falls back to building it when the asset is missing.

local S = {}
local rgba = color.rgba

local INK = rgba(28, 32, 44)
local DIM = rgba(96, 104, 120)
local BLUE = rgba(36, 84, 168)

-- ---------------------------------------------------------------- the form page, by hand

-- Fields in page space (points, origin top-left): name, kind, x, y, w, h.
S.FIELDS = {
	{ name = "reviewer", kind = "text", label = "Reviewer 審核人", rect = { 200, 128, 320, 24 } },
	{ name = "department", kind = "combo", label = "Department 部門", rect = { 200, 168, 200, 24 }, options = { "Finance", "Sales", "Engineering", "Operations" } },
	{ name = "date", kind = "text", label = "Date 日期", rect = { 200, 208, 160, 24 } },
	{ name = "amount", kind = "text", label = "Amount (USD) 金額", rect = { 200, 248, 160, 24 } },
	{ name = "approved", kind = "check", label = "Approved 核准", rect = { 200, 292, 16, 16 } },
	{ name = "comments", kind = "multiline", label = "Comments 備註", rect = { 200, 322, 320, 90 } },
}

local PAGE_H = 792

local function pdf_rect(r)
	-- top-left page space to PDF user space (origin bottom-left)
	return string.format("[%g %g %g %g]", r[1], PAGE_H - r[2] - r[4], r[1] + r[3], PAGE_H - r[2])
end

local function form_pdf()
	local objs = {}
	local n_fields = #S.FIELDS
	-- 1 catalog, 2 pages, 3 page, 4 Helvetica, 5 contents, 6 check on, 7 check off, 8.. fields
	local field_ids = {}
	for i = 1, n_fields do
		field_ids[i] = 7 + i
	end
	local refs = {}
	for i, id in field_ids do
		refs[i] = id .. " 0 R"
	end
	local ref_list = "[" .. table.concat(refs, " ") .. "]"
	objs[1] = "<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields " .. ref_list .. " /DR << /Font << /Helv 4 0 R >> >> /DA (/Helv 0 Tf 0 g) >> >>"
	objs[2] = "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"
	objs[3] = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /Helv 4 0 R >> >> /Contents 5 0 R /Annots " .. ref_list .. " >>"
	objs[4] = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
	-- the boxes are part of the page so they show before anything is filled
	local ops = { "0.86 0.88 0.93 RG 1 w 56 701 m 556 701 l S" }
	for _, f in S.FIELDS do
		local r = f.rect
		local x, y = r[1], PAGE_H - r[2] - r[4]
		table.insert(ops, string.format("0.95 0.96 0.99 rg %g %g %g %g re f", x, y, r[3], r[4]))
		table.insert(ops, string.format("0.55 0.6 0.72 RG 0.8 w %g %g %g %g re S", x + 0.4, y + 0.4, r[3] - 0.8, r[4] - 0.8))
	end
	local content = table.concat(ops, "\n")
	objs[5] = "<< /Length " .. #content .. " >>\nstream\n" .. content .. "\nendstream"
	local on = "0.12 0.36 0.78 RG 2 w 3.5 8.5 m 6.8 4.5 l 12.5 12 l S"
	objs[6] = "<< /Type /XObject /Subtype /Form /BBox [0 0 16 16] /Length " .. #on .. " >>\nstream\n" .. on .. "\nendstream"
	objs[7] = "<< /Type /XObject /Subtype /Form /BBox [0 0 16 16] /Length 0 >>\nstream\n\nendstream"
	for i, f in S.FIELDS do
		local common = "/Type /Annot /Subtype /Widget /P 3 0 R /F 4 /Rect " .. pdf_rect(f.rect) .. " /T (" .. f.name .. ")"
		if f.kind == "check" then
			objs[field_ids[i]] = "<< /FT /Btn " .. common .. " /V /Off /AS /Off /MK << /CA (4) >> /AP << /N << /Yes 6 0 R /Off 7 0 R >> >> >>"
		elseif f.kind == "combo" then
			local opts = {}
			for k, o in f.options do
				opts[k] = "(" .. o .. ")"
			end
			objs[field_ids[i]] = "<< /FT /Ch /Ff 131072 " .. common .. " /Opt [" .. table.concat(opts, " ") .. "] /V () /DA (/Helv 11 Tf 0 g) >>"
		elseif f.kind == "multiline" then
			objs[field_ids[i]] = "<< /FT /Tx /Ff 4096 " .. common .. " /V () /DA (/Helv 10 Tf 0 g) >>"
		else
			objs[field_ids[i]] = "<< /FT /Tx " .. common .. " /V () /DA (/Helv 11 Tf 0 g) >>"
		end
	end
	local out = { "%PDF-1.7\n%\xE2\xE3\xCF\xD3\n" }
	local len = #out[1]
	local offsets = {}
	for i = 1, #objs do
		offsets[i] = len
		local s = i .. " 0 obj\n" .. objs[i] .. "\nendobj\n"
		table.insert(out, s)
		len += #s
	end
	local xref = { "xref\n0 " .. (#objs + 1) .. "\n0000000000 65535 f \n" }
	for i = 1, #objs do
		table.insert(xref, string.format("%010d 00000 n \n", offsets[i]))
	end
	table.insert(out, table.concat(xref))
	table.insert(out, "trailer\n<< /Size " .. (#objs + 1) .. " /Root 1 0 R >>\nstartxref\n" .. len .. "\n%%EOF\n")
	return table.concat(out)
end
S.form_pdf = form_pdf

-- ---------------------------------------------------------------- pages written with the pdf module

local function text(d, p, x, y, s, o)
	o = o or {}
	o.x, o.y, o.text = x, y, s
	o.size = o.size or 11
	o.color = o.color or INK
	return pdf.add_text(d, p, o)
end

-- A table of left-aligned cells: rows[1] is the header.
local function grid(d, p, x, y, widths, rows)
	local row_h = 18
	local total = 0
	for _, w in widths do
		total += w
	end
	for i, row in rows do
		local cx = x
		for j, cell in row do
			d = text(d, p, cx, y + (i - 1) * row_h, cell, { bold = i == 1 })
			cx += widths[j]
		end
	end
	-- rules under the header and the last row
	local y1 = y + row_h - 3
	local y2 = y + #rows * row_h - 3
	d = pdf.add_path(d, p, { "M", x, y1, "L", x + total - 20, y1, "M", x, y2, "L", x + total - 20, y2 }, { stroke = rgba(150, 156, 170), width = 0.6 })
	return d
end

local REVENUE = {
	{ "Region", "Q1", "Q2", "Q3", "Revenue" },
	{ "North", "120", "135", "142", "1,250.00" },
	{ "South", "98", "110", "118", "980.50" },
	{ "East", "143", "160", "171", "1,620.75" },
	{ "West", "87", "91", "96", "760.00" },
	{ "Total", "448", "496", "527", "4,611.25" },
}

local EXPENSES = {
	{ "Category", "Budget", "Actual", "Variance" },
	{ "Salaries", "820.00", "845.20", "-25.20" },
	{ "Marketing", "240.00", "198.40", "41.60" },
	{ "Cloud and IT", "150.00", "162.75", "-12.75" },
	{ "Travel", "60.00", "41.10", "18.90" },
	{ "Office", "45.00", "44.30", "0.70" },
}

local function footer(d, p, n)
	return text(d, p, 56, 750, string.format("AnyPage sample report · page %d of %d", p, n), { size = 9, color = DIM })
end

function S.build()
	local d = pdf.new({ width = 612, height = 792, pages = 3 })
	local W = 500

	-- page 1: the report and its revenue table
	d = text(d, 1, 56, 52, "AnyPage Quarterly Report", { size = 26, bold = true })
	d = text(d, 1, 56, 90, "第三季營運報告 · Q3 2026", { size = 15, color = BLUE })
	d = text(d, 1, 56, 126, "Revenue grew in every region this quarter. Total revenue reached 4,611.25 thousand USD, "
		.. "up 6.2% on the second quarter. East remains the largest region; West grew the least.", { width = W })
	d = text(d, 1, 56, 172, "本季各地區營收皆成長，總營收 4,611.25 千美元，比上季增加 6.2%。", { width = W, size = 12, color = BLUE })
	d = text(d, 1, 56, 214, "Revenue by region (thousand USD)", { size = 14, bold = true })
	d = grid(d, 1, 56, 240, { 110, 80, 80, 80, 110 }, REVENUE)
	-- a bar chart of the revenue column, as vector paths
	d = text(d, 1, 56, 372, "Revenue by region", { size = 14, bold = true })
	local bars = {}
	local vals = { 1250, 980.5, 1620.75, 760 }
	for i, v in vals do
		local x, h = 96 + (i - 1) * 112, v / 9
		table.insert(bars, { path = { "M", x, 600, "L", x + 64, 600, x + 64, 600 - h, x, 600 - h, "Z" }, fill = rgba(70, 120, 210) })
		d = text(d, 1, x + 32, 608, REVENUE[i + 1][1], { size = 10, align = "center", color = DIM })
		d = text(d, 1, x + 32, 600 - h - 16, REVENUE[i + 1][5], { size = 9, align = "center", color = DIM })
	end
	d = pdf.add_path(d, 1, bars)
	d = pdf.add_path(d, 1, { "M", 76, 600, "L", 556, 600 }, { stroke = rgba(40, 44, 56), width = 1.2 })
	d = text(d, 1, 56, 650, "Prepared by the finance team. Figures are unaudited; see the notes on page 3 and approve the report with the form on page 4.", { width = W, size = 10, color = DIM })
	d = footer(d, 1, 4)

	-- page 2: expenses
	d = text(d, 2, 56, 52, "Expenses and Outlook", { size = 22, bold = true })
	d = text(d, 2, 56, 86, "費用與展望", { size = 15, color = BLUE })
	d = text(d, 2, 56, 120, "Operating costs stayed close to budget. Salaries ran slightly over; marketing came in under plan. "
		.. "Revenue minus operating expenses leaves an operating margin of about 72 percent.", { width = W })
	d = text(d, 2, 56, 170, "Operating expenses (thousand USD)", { size = 14, bold = true })
	d = grid(d, 2, 56, 196, { 140, 100, 100, 100 }, EXPENSES)
	d = text(d, 2, 56, 330, "Outlook", { size = 14, bold = true })
	d = text(d, 2, 56, 354, "We expect revenue to grow again in the fourth quarter, led by the East and North regions. "
		.. "Cloud costs will rise with the new data platform; travel stays limited.", { width = W })
	d = text(d, 2, 56, 404, "第四季預期營收持續成長，雲端費用會隨新的資料平台增加。", { width = W, size = 12, color = BLUE })
	d = footer(d, 2, 4)

	-- page 3: notes
	d = text(d, 3, 56, 52, "Notes 附註", { size = 22, bold = true })
	d = text(d, 3, 56, 92, "1. Revenue figures are unaudited and rounded to two decimals.", { width = W })
	d = text(d, 3, 56, 116, "2. Regions follow the 2026 sales territories; East includes the new Kaohsiung office.", { width = W })
	d = text(d, 3, 56, 140, "3. Expenses are booked when invoiced, not when paid.", { width = W })
	d = text(d, 3, 56, 176, "本報告的營收數字尚未經過審計。請在第 4 頁的核准表填寫審核人、部門與核准金額，然後儲存 PDF。", { width = W, size = 12, color = BLUE })
	d = text(d, 3, 56, 230, "How to use this workspace", { size = 14, bold = true })
	d = text(d, 3, 56, 256, "Search for a word to see every place it appears. Drag across text to select it, then copy, highlight or annotate it. "
		.. "Ask the assistant to summarize a page, highlight every mention of revenue, or fill in the approval form.", { width = W })
	d = footer(d, 3, 4)

	-- page 4: the approval form
	local form = pdf.open(form_pdf())
	d = pdf.merge(d, form)
	d = text(d, 4, 56, 52, "Approval Form 核准表", { size = 22, bold = true })
	for _, f in S.FIELDS do
		d = text(d, 4, 56, f.rect[2] + (if f.kind == "check" then 1 else 6), f.label, { size = 11 })
	end
	d = text(d, 4, 56, 440, "Fill the fields in the 表單 (Form) tab, or ask the assistant to do it, then save the PDF.", { width = W, size = 10, color = DIM })
	d = footer(d, 4, 4)
	return d
end

return S
