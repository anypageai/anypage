-- The page packages this app ships, in the order the launcher shows them.
--
-- A page cannot list the packages next to it (there is no page.packages();
-- see NOTES.md), so this table is the launcher's knowledge of them. The
-- headless test (crates/anypage-std/tests/page_home.rs) compares it with
-- the pages/ folder: every package there must be listed here, and the
-- modules listed must match what each page.json declares.
--
--   name      the package folder, what page.navigate(name) opens
--   title     the card title (Traditional Chinese), en: a short English name
--   desc      one line in Traditional Chinese
--   modules   native modules the page needs (it does not load without them)
--   optional  modules it uses when the platform has them
--   mark      the text on the card's icon, color its colour
--   new       pages added in this round

local rgba = color.rgba

return {
	{
		name = "studio",
		title = "工作室",
		en = "Studio",
		desc = "一張平面圖：CAD 量測、試算表估價、PDF 報表，資料互通",
		modules = { "cad", "sheet", "pdf" },
		optional = { "scene" },
		ui = true,
		mark = "St",
		color = rgba(249, 115, 22),
		new = true,
	},
	{
		name = "editor",
		title = "多語言編輯器",
		en = "Editor",
		desc = "任何語言都能編輯：輸入法、右到左文字、尋找取代與存檔",
		modules = {},
		optional = {},
		ui = true,
		mark = "Ed",
		color = rgba(34, 197, 94),
		new = true,
	},
	{
		name = "pdf-agent",
		title = "PDF 工作台",
		en = "PDF agent",
		desc = "人和 AI 助理用同一組操作讀、搜尋、註記與填寫 PDF",
		modules = { "pdf" },
		optional = { "sheet", "cloud" },
		ui = true,
		mark = "AI",
		color = rgba(244, 63, 94),
		new = true,
	},
	{
		name = "pdf-demo",
		title = "PDF 文件",
		en = "PDF",
		desc = "開啟、檢視、搜尋、擷取、註記與儲存 PDF，純 Rust",
		modules = { "pdf" },
		mark = "PDF",
		color = rgba(220, 68, 68),
	},
	{
		name = "sheet-demo",
		title = "試算表",
		en = "Sheets",
		desc = "公式即時重算，開啟與儲存 xlsx / CSV",
		modules = { "sheet" },
		mark = "表",
		color = rgba(34, 160, 84),
	},
	{
		name = "cad-demo",
		title = "CAD 圖面",
		en = "CAD",
		desc = "DXF 向量圖磚，任何縮放都清晰；圖層、量測與編輯",
		modules = { "cad" },
		mark = "CAD",
		color = rgba(234, 150, 30),
	},
	{
		name = "scene-demo",
		title = "3D 場景",
		en = "3D",
		desc = "網格與上千萬點的點雲，由 GPU 繪製",
		modules = { "scene" },
		mark = "3D",
		color = rgba(139, 92, 246),
	},
	{
		name = "cloud-demo",
		title = "雲端聊天",
		en = "Cloud chat",
		desc = "串流 AI 聊天：Ollama、LM Studio 或 OpenAI（選用）",
		modules = { "cloud" },
		mark = "☁",
		color = rgba(14, 165, 233),
	},
	{
		name = "live-demo",
		title = "直播",
		en = "Live",
		desc = "把測試畫面編成 HLS 直播，同時在本機播放回來",
		modules = { "media" },
		mark = "HLS",
		color = rgba(236, 72, 153),
	},
	{
		name = "devices-demo",
		title = "裝置",
		en = "Devices",
		desc = "藍牙 LE、USB 與序列埠；平台沒有的會略過",
		modules = {},
		optional = { "ble", "usb", "serial" },
		mark = "BT",
		color = rgba(20, 184, 166),
	},
	{
		name = "demo",
		title = "引擎示範",
		en = "Engine demo",
		desc = "計算圖：串流、背景工作、碎形圖磚與 GPU 特效",
		modules = { "demo" },
		mark = "◆",
		color = rgba(99, 102, 241),
	},
	{
		name = "ui-gallery",
		title = "UI 元件庫",
		en = "UI gallery",
		desc = "所有 UI 元件的深色與淺色主題一覽",
		modules = {},
		ui = true,
		mark = "UI",
		color = rgba(59, 130, 246),
	},
	{
		name = "text-input-test",
		title = "文字輸入測試",
		en = "Text input",
		desc = "檢查鍵盤、輸入法、剪貼簿與檔案拖放",
		modules = {},
		mark = "字",
		color = rgba(100, 116, 139),
	},
}
