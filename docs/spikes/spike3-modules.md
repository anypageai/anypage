# Spike 3：能力模組與 App

2026-10-24。接續 [spike 2](spike2-script-pages.md)。目標（使用者）：用這個引擎可以做任何 App：高效率操作 PDF、讓 agent 操作 PDF、CAD、Excel、所有語言的文字編輯、雲端（AWS、GCP、OpenAI 格式串流）、影片直播、藍牙與 USB、3D 模型與點雲，而且這些功能可以融合。

## 做法

1. **先擴充核心**（引擎與執行時），讓模組有地方接：原生模組登記（函式、工作、串流、tile、GPU 圖層、Luau 函式庫）、由請求驅動的串流、影像值、GPU 圖層、滑鼠懸停與滾輪、具名按鍵、文字輸入與 IME、剪貼簿、檔案對話框、拖放、語義埠（`page.action`、`describe`、`act`）與桌面的本機 agent 連接埠。
2. **契約寫成文件**（docs/MODULES.md），包含共用資料格式（位元組、影像、表格、網格、點雲、向量路徑）與安全規則。
3. **十條工作線平行開發**：九個模組加上 Android 宿主的輸入工作。每條線都是「實作 → 獨立審查（要求找錯） → 修正」，共 30 個 agent。每個模組先是獨立的 cargo workspace，互不打斷，最後再併回主 workspace。
4. **整合**：併入 workspace、把新文字系統換進引擎、把各模組共同提出的核心需求做掉、`anypage-std` 一次登記所有模組、兩個宿主都接上，並在真實視窗裡逐頁截圖確認。

## 模組

| 模組 | 用什麼做 | 頁面能做什麼 | 測試 |
|---|---|---|---|
| text（引擎內） | cosmic-text、swash，自寫 COLRv1 彩色 emoji | 所有文字的造字（阿拉伯連字、印度系重排、泰文）、雙向文字、字型後備、UAX #14 斷行、以字素為單位的游標 | 60 |
| ui | Luau 函式庫 | 版面（row/column/grid）、按鈕、勾選、開關、滑桿、文字欄、多行文字、下拉、分頁、虛擬化清單與表格、捲動、對話框、提示；焦點、鍵盤、IME、剪貼簿 | 60 |
| pdf | hayro（繪製）、lopdf（結構與寫入），自寫文字定位與表格偵測 | 任何縮放的頁面 tile、文字與位置、搜尋、抽成區塊與表格、加文字、加向量路徑、加圖、螢光筆、註解、表單、增刪轉頁、合併、存檔 | 55 |
| sheet | IronCalc | 開啟與儲存 xlsx、CSV、公式與重算、範圍與表格、編輯、復原 | 48 |
| cad | dxf、tiny-skia，STEP 經 truck | DXF 圖層、圖塊、填充、尺寸、任何縮放的 tile、選取、量長度與面積、編輯、存 DXF、轉成 PDF 用的向量路徑、STEP 轉網格 | 58 |
| cloud | ureq + rustls（ring），自寫 SSE、SigV4、RS256 | OpenAI 相容的串流聊天（含工具呼叫）、HTTP、S3（SigV4、預簽 URL）、Google Cloud Storage（服務帳戶 JWT） | 85 |
| media | OpenH264（原始碼編譯）、自寫 MPEG-TS、m3u8-rs、symphonia + cpal | HLS 直播播放（H.264 + AAC，依 PTS 播放、音訊時鐘同步）、直播推流（編碼、切片、區網 HLS 伺服器）、Windows 攝影機 | 50 / 54 |
| devices | btleplug、nusb、serialport；Android 用橋接介面 | 藍牙 LE 掃描、連線、讀寫、通知；USB 列舉與傳輸；序列埠 | 61 |
| scene | wgpu（引擎的）、gltf、tobj、stl_io、las/laz | glTF/OBJ/STL/PLY/XYZ/LAS/LAZ 載入，網格與點雲的 GPU 圖層，千萬點 | 59 |

另有 Android 宿主 31 個測試、執行時 25 個、核心 19 個、示範 7 個；整個 workspace 的測試全部通過。

## 量測（摘錄）

Windows、RTX 3090、Ryzen 9 5950X；量測時常有其他編譯在跑，數字是當時的值。

| 項目 | 結果 |
|---|---|
| 字型載入 | 66 ms（Windows 176 個字型） |
| 造字 1 萬個混合文字字元（冷） | 122 ms；已快取的量寬每行 45 ns |
| UI：10 萬列清單的一幀 | 中位數約 1.1 ms |
| UI：200 個元件的表單，按鍵後一幀 | 中位數約 2.7–3.1 ms |
| PDF：200 頁文字檔開啟 | 1.0–2.3 ms；整份文字＋搜尋 0.6–1.8 s |
| 試算表：10 萬格（2 萬公式）開啟 | release 888 ms；單次編輯 159 ms |
| CAD：10 萬個圖元、5.4 MB DXF | 開啟 406 ms；整張圖 tile 110 ms，放大後 0.4–6 ms |
| 聊天串流（本機模擬伺服器） | 第一個字 3 ms；SSE 解析 240 MB/s |
| 直播 720p | 編碼 7 ms/幀、解碼 1.2 ms/幀、YUV→RGBA 1 ms |
| 直播 1080p | 編碼 15 ms/幀、解碼 3.5 ms/幀、YUV→RGBA 2 ms |
| 直播頁（真實視窗，1280×720 30 fps） | 推流編碼 8.9 ms/幀；播放 29.9 fps、掉幀 0 |
| 3D：1000 萬點 | 第一幀上傳 152 MB 約 0.1–0.25 s；之後每幀 4.5–22 ms |
| 這台電腦的實際裝置 | USB 8 個（2.5–4.3 ms 列完）、COM3/COM5、藍牙掃描 5 秒找到 5 個 |

## 審查找到並修掉的問題（摘錄）

每個模組的審查都找到了真問題，修正後才通過：

- 文字：段落分隔符會觸發 cosmic-text 的斷言；Android 15 的 emoji 字型只有 COLRv1，原本畫不出來（為此寫了 COLRv1 繪製器）。
- 試算表：兩個 blocker，惡意 xlsx 的壓縮炸彈與遞迴公式會耗盡資源。
- CAD：巢狀圖塊的檔案 20 KB 就能讓開檔花 10 秒、5 GB 記憶體；改為只存一次再實例化後是 1 ms、0.5 MB。
- PDF：40 KB 文字的換行要 24.6 秒；32k 字形的排版要 6.2 秒；都改成線性。
- 雲端：取消的請求會佔住工作執行緒到逾時；改成立刻釋放（16 ms）。
- 直播：短的 PES 標頭會 panic；頁面關閉要等 9.7 秒；都修掉。
- 3D：惡意 LAZ 會讓解碼器先配置 64 GB。
- 裝置：關掉的串流每秒醒來六百萬次。

## 整合時加進核心的東西

模組共同提出的需求，整合時做進核心（docs/MODULES.md 第 4 節）：`page.peek`、工作完成回呼與 eager 工作、原生程式碼的時間不算進腳本預算、所有原生邊界都攔 panic、GPU 圖層的錯誤範圍、頁面包可帶二進位素材（`page.asset`）、頁面之間導覽（`page.navigate`）、綁定主機的憑證、IPv6 主機樣式、`pointer_cancel`、右到左文字的函式。

## 平台

所有模組都通過 `cargo ndk -t arm64-v8a check`。Android 上：藍牙與 USB 走 Kotlin 橋接（第二輪實作），攝影機走宿主的 `HostCamera`。macOS、iOS、Linux、Web 這一輪沒有建置。

## App 與宿主（第二輪）

第二輪平行做了 Android 宿主與四個示範 App，同樣經過獨立審查；其中四個 agent 碰到用量上限，剩下的修正改由三個 agent 重做，Android 部分由我在模擬器上檢查。

| App | 做什麼 | 測試 |
|---|---|---|
| home | 啟動頁：列出所有頁面、各自用到的模組與本平台有沒有，點卡片用 `page.navigate` 開啟；手機版面 | 9 |
| studio | 開 DXF 平面圖 → 量測房間面積與牆長 → 試算表算單價、小計、稅、總計 → 產生 PDF 報告（圖面以向量路徑放進 PDF，加數量表與圖框）→ 存 PDF、XLSX、DXF；另外顯示 STEP 零件的 3D 圖；寬版與三個手機畫面 | 20 |
| pdf-agent | PDF 工作台：縮放、縮圖、搜尋與螢光標示、註記、表單、轉頁刪頁、表格匯出 XLSX；每個操作都是 `page.action`，外部 agent 可透過連接埠操作；頁面內的 AI 助理用 `cloud.chat` 的工具呼叫執行同一組動作；修改過的文件被取代前會先確認 | 12 |
| editor | 多語言編輯器：輸入法組字、字素為單位的游標、右到左段落、搜尋取代、復原、開檔與存檔（UTF-8、UTF-16）、檔名清理、手機版面 | 21 |

審查後補進核心與宿主的東西：頁面測試可在 repo 內直接跑；慢幀時已完成的 tile 不再卡住（模擬器上 PDF 與 CAD 原本要等下一次觸控才出現）；`text.decode`；`page.viewport()` 與 `viewport` 事件；重複的格名稱變成載入錯誤；雲端模組照 page.json 綁定憑證的主機；桌面版 Esc 不再關閉 App、Alt+Left 回上一頁、量測面板預設隱藏。

Android（模擬器，API 35 x86_64）：

- 14 個頁面都能開啟，logcat 沒有 FATAL 或 panic。
- 從啟動頁點卡片導覽到 studio，平面圖、數量表、3D 零件與 PDF 報告都正確；editor 與 pdf-agent 自動切成手機版面。
- 藍牙：權限對話框、開始掃描、連線逾時與斷線都走過；模擬器沒有周邊，連線成功、讀寫與通知沒測到。
- USB：模擬器沒有 USB 主機功能，只看到清楚的「此裝置不支援 USB 主機模式」。
- 攝影機：Camera2 開啟 640×480，直播推流與 HLS 播放都有畫面（24.8 fps）。
- 金鑰：Android Keystore AES-GCM 加密存在 App 私有空間。
- 大小：debug APK 93.5 MB（.so 不壓縮、16 KB 對齊）；剝除後的 arm64 函式庫 36.5 MB，gzip 後約 14.7 MB。所有 .so 的 LOAD 段都是 16 KB 對齊。

## 還沒做（摘錄，完整清單在各模組的 CORE_REQUESTS.md）

- 文字：只有一種字重；泰、寮、高棉、緬文的長段以字素斷行（沒有字典）。
- PDF：編輯後第一次讀取會重寫整份文件（大檔約 150 ms）；寫入的中文字用非內嵌的 Adobe 字型。
- 試算表：整欄範圍（`A:A`）每次重算都掃整欄；IronCalc 的計算不能中斷；不支援 LAMBDA。
- CAD：線型畫成實線、沒有 DWG、STEP 沒用真實 CAD 軟體匯出的檔案試過。
- 雲端：只對本機模擬伺服器測過，沒有連過真的 OpenAI、S3、GCS。
- 直播：沒有 AES 加密 HLS、fMP4、HEVC、自適應碼率。
- 裝置：Windows 上沒有接真的 WinUSB 裝置與藍牙周邊做傳輸測試。
- 3D：glTF 材質與動畫、拾取都還沒有。
- App：沒有在實體手機上跑過；雲端只對本機模擬伺服器測過；藍牙與 USB 的資料傳輸沒有用真實裝置測過；pdf-agent 的手機版側邊欄仍是縮小顯示；UI 函式庫還不處理 `pointer_cancel`。
- 宿主：桌面版關閉視窗與 Android 的返回鍵不會詢問未儲存的修改（需要一個 `leave` 事件）。
