-- samples: one paragraph per writing system, for the editor's sample menu.
--
-- Every paragraph is original text written for this page. Emoji and
-- combining sequences are spelled with \u{...} escapes so the exact code
-- points (ZWJ, variation selectors, skin tones, combining marks) are visible
-- in the source.

local S = {}

local FAMILY = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}"
local CODER = "\u{1F9D1}\u{1F3FD}\u{200D}\u{1F4BB}"
local RAINBOW = "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}"
local THUMB = "\u{1F44D}\u{1F3FF}"
local FLAG = "\u{1F1F9}\u{1F1FC}"
local HEART_FIRE = "\u{2764}\u{FE0F}\u{200D}\u{1F525}"

-- { label shown in the menu, BCP 47 tag, paragraph }
S.list = {
	{
		"繁體中文",
		"zh-Hant",
		"繁體中文：這是一個多語言文字編輯器。中文的斷行規則不讓句號或逗號出現在行首，游標以「字素」為單位移動，輸入法的組字會直接顯示在游標位置。",
	},
	{
		"简体中文",
		"zh-Hans",
		"简体中文：这个编辑器支持输入法组字、剪切、复制、粘贴、撤销与重做，状态栏会统计行数、字数、字素和字节。",
	},
	{
		"日本語",
		"ja",
		"日本語：ひらがな、カタカナ、漢字が混ざった文章です。「、」や「。」は行頭に来ないように折り返されます。",
	},
	{
		"한국어",
		"ko",
		"한국어: 한글은 자음과 모음이 모여 하나의 음절을 이룹니다. 커서는 음절 단위로 움직입니다.",
	},
	{
		"العربية Arabic",
		"ar",
		"العربية: يُكتب هذا النص من اليمين إلى اليسار، وتتصل الحروف ببعضها حسب موقعها في الكلمة. يظهر الرقم 2026 والكلمة AnyPage في وسط الجملة.",
	},
	{
		"עברית Hebrew",
		"he",
		"עברית: הטקסט הזה נכתב מימין לשמאל. המספר 42 והמילה AnyPage מופיעים באמצע המשפט, והסמן זז לפי כיוון הפסקה.",
	},
	{
		"हिन्दी Hindi",
		"hi",
		"हिन्दी: देवनागरी लिपि में मात्राएँ और संयुक्ताक्षर होते हैं, जैसे क्षमा, विद्यार्थी और स्त्री। कर्सर पूरे अक्षर पर चलता है।",
	},
	{
		"ไทย Thai",
		"th",
		"ภาษาไทย: ภาษาไทยเขียนต่อกันโดยไม่เว้นวรรคระหว่างคำ และมีสระกับวรรณยุกต์ซ้อนกัน เช่น น้ำ ที่ กี่ ปี่",
	},
	{
		"Русский",
		"ru",
		"Русский: Съешь же ещё этих мягких французских булок, да выпей чаю. Поиск без учёта регистра находит и «ЧАЙ», и «чай».",
	},
	{
		"Ελληνικά",
		"el",
		"Ελληνικά: Ξεσκεπάζω την ψυχοφθόρα βδελυγμία. Καλημέρα κόσμε!",
	},
	{
		"Emoji ZWJ",
		"und",
		"Emoji："
			.. FAMILY
			.. " 家庭 · "
			.. CODER
			.. " 工程師 · "
			.. RAINBOW
			.. " 彩虹旗 · "
			.. THUMB
			.. " 膚色 · "
			.. FLAG
			.. " 旗幟 · "
			.. HEART_FIRE
			.. " 每一個都是單一字素，方向鍵與刪除鍵一次處理一整個。",
	},
	{
		"混合 Mixed",
		"mul",
		"混合 Mixed：English 中文 العربية עברית हिन्दी ไทย Ελληνικά Русский 日本語 한국어，同一行裡的雙向文字依 Unicode 規則排列。",
	},
	{
		"組合字元 Combining",
		"mul",
		"組合字元：Tiếng Việt có dấu；e\u{301} a\u{308} n\u{303} o\u{302}\u{301} 各是一個字素；Z\u{337}\u{31B}a\u{334}\u{31C}l\u{335}g\u{338}o\u{336}",
	},
	{
		"缺字示範 Missing glyphs",
		"mul",
		"缺字示範：這些字元可能沒有字型能顯示 \u{11F04}\u{11F05}（Kawi）、\u{1E4D0}\u{1E4D1}（Nag Mundari）、\u{0378}（未指派）。狀態列的「缺字」會計算它們。",
	},
}

S.labels = {}
for i, s in S.list do
	S.labels[i] = s[1]
end

function S.text(i)
	local s = S.list[i]
	return if s then s[3] else nil
end

-- Every sample, one paragraph each.
function S.all()
	local out = {}
	for i, s in S.list do
		out[i] = s[3]
	end
	return table.concat(out, "\n")
end

-- The document the page starts with. opts.can_open: this runtime can read
-- opened and dropped files (else the text does not offer it); opts.narrow:
-- the phone layout (its sample menu is called 範例).
function S.welcome(opts)
	opts = opts or {}
	local menu = if opts.narrow then "「範例」選單" else "工具列的「插入範例」"
	local files = if opts.can_open
		then "，也可以開啟或拖放 .txt / .md 檔案。"
		else "；文件可以存成 UTF-8 的 .txt / .md 檔案。"
	local out = {
		"歡迎使用 AnyPage 多語言編輯器 Multilingual editor",
		"",
		"這一頁證明「任何語言」都能編輯：輸入法組字直接顯示在游標處、從右到左的段落靠右對齊、表情符號序列與組合字元以字素為單位移動和刪除。"
			.. menu
			.. "可以加入更多語言"
			.. files,
		"",
		"快捷鍵：Ctrl+F 尋找 · Ctrl+H 取代 · Ctrl+Z / Ctrl+Y 復原與重做 · Ctrl+S 儲存"
			.. (if opts.can_open then " · Ctrl+O 開啟" else "")
			.. " · Ctrl+= / Ctrl+- 縮放 · Alt+Z 自動換行 · Tab 插入定位字元",
		"",
	}
	for _, i in { 1, 5, 6, 7, 8, 11, 12 } do
		out[#out + 1] = S.list[i][3]
	end
	return table.concat(out, "\n")
end

return S
