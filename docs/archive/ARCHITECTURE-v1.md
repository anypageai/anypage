# AnyPage 新架構：技術討論與實現目標

> **已被 v2 取代（2026-10-24）。** 現行架構見 [../ARCHITECTURE.md](../ARCHITECTURE.md)。本檔保留 v1「文件引擎」版原文供對照。

整理自 2026-10-22 至 10-24 的架構討論。狀態：**提案，尚無任何原型驗證**。
2026-10-24 補：先例與建材現況調查，修正了第 8、12、16 節的假設，並在第 5、10 節各補一段；來源列在第 16 節末。
所有效能主張都要靠第 12 節的第 0 階段 spike 來證實或推翻。

---

## 1. 決定與背景

### 放棄的方向（全部）
- 每人一台 Contabo VPS + Hermes agent 的「雲端電腦」模式
- Modal 上的垂直服務（照片→3D、土地測量）
- Streams 個人訊息流
- Play / instant-play 配方式小遊戲

舊 repo（`anypage`）只當備份，新架構另起新 repo。

### 新理念
**代碼本身是免費的。** AI 讓產生程式碼的成本趨近於零，所以 PDF、CAD、Excel、影片剪輯這類單一用途的文件與媒體工具，在這個時代都應該是免費的。要做的不是某一個工具，而是能讓這些工具被（人或 AI）低成本產生出來的**架構**。

### 使用者自己踩過的坑
- HTML / WebView 效率低，PDF 在 HTML 裡坑極多，影片剪輯在 HTML 裡不可能。
- 但 HTML 是唯一能在 App 裡動態下載執行的東西。這個矛盾就是核心問題。

### 方向
做一個**遊戲引擎等級效能**的框架：引擎原生編進 App，AI 生成的只是薄薄一層控制邏輯與宣告式介面。Figma、Zed、CapCut 內部都是這個做法。

---

## 2. 核心命題與一條鐵律

**鐵律：在 iOS 上，「可下載的程式碼」等於「被直譯的程式碼」。** iOS 禁止第三方 App 使用 JIT，所以任何能熱下載的語言到了 iOS 都只能跑在位元碼直譯器上。這條限制只有兩個合法例外：

1. **GPU shader**。Metal / Vulkan / WebGPU 都允許執行期編譯 shader 原始碼。這是唯一「可下載、又跑原生速度」的程式碼，而且天生被沙盒隔離。
2. **引擎內建的快路徑**。程式碼編進 App，腳本只下參數。

因此效能設計只有一句話：**每幀、每像素、每格子的工作全放進這兩個例外，腳本層只處理事件。** 腳本語言的選擇因此變成人因與安全問題，不是效能問題。

遊戲引擎正好解決兩個痛點：
- 動態又要快：引擎原生、玩法腳本可下載（Roblox 就這樣上架 App Store）。
- 所有文件都是「一個很大的世界，透過鏡頭去看」：PDF 切 tile、百萬列表格只畫可視格、CAD 視錐裁切 + LOD、影片硬體解碼直接成 GPU 貼圖再 shader 合成。四種工具共用鏡頭、tile 串流、GPU 合成。

但和遊戲引擎分道的地方：**沒有變化就是 0 fps**（耗電），**文字輸入交回原生**（IME、選取、無障礙是自繪 UI 的無底洞）。

---

## 3. 層級設計：微核心

```
L4  工具包    manifest(JSON) + 腳本位元碼 + WGSL + 資產          可下載、可熱換
L3  腳本 VM   直譯器，每個工具一個隔離 realm，只拿到能力物件    編進 App
L2  領域引擎  PDFium · IronCalc · OpenCascade · 硬體編解碼       編進 App，C ABI
L1  核心      Rust：wgpu 渲染、排版、tile 串流、排程、場景圖、能力代理、語義埠
L0  平台殼    surface、輸入、IME、無障礙、生命週期、檔案選擇     每平台一份，極薄
```

L1 是微核心：只懂「場景圖 + 鏡頭 + 指令 + 能力」，不知道什麼是 PDF 或影片。PDF 編輯器、影片剪輯器都是 L4 的工具包，透過 L3 驅動 L2。Emacs（C + Elisp）、Blender（C + Python）、Neovim（C + Lua）、Roblox（C++ + Luau）全是這個結構。

---

## 4. 語言體系：各層用現成語言

| 層 | 語言 | 為什麼 |
|---|---|---|
| 宣告 | JSON（帶 schema） | manifest、權限、佈局。可驗證、可 diff，AI 不會寫錯語法 |
| 邏輯 | Luau（或 TypeScript 編譯而來），發佈時編成位元碼 | 裝置只收位元碼。見第 10 節的 VM 決定 |
| GPU | WGSL | 濾鏡、轉場、合成、CAD 著色。經 naga 轉 MSL / SPIR-V / HLSL，各平台原生速度 |
| 核心擴充（選配） | WASM（Rust/C/Zig 編譯） | 開發者級的自訂解碼器、匯入器。Android/桌面/Web 可 AOT；iOS 只能直譯，約原生 1/5～1/10 |

取捨：
- 純 WASM 當唯一位元碼最符合「就是某種語言」，但在 iOS 退化成直譯，恰好在最關鍵的平台失去承諾，跨邊界呼叫也貴。適合當第四層，不適合當第二層。
- 研究方向：把 TS 限制成型別健全的子集（AssemblyScript、Static Hermes 的路），位元碼可用未裝箱數值和靜態派發，直譯快 2～5 倍，語言表面仍是 TS。

---

## 5. 七條不變量

1. **熱路徑不穿越語言邊界。** 腳本只在事件發生時執行，產出指令或樹；佈局、排版、繪製全在 L1。穩態時腳本每幀耗時為零。
2. **跨層只傳句柄或零拷貝緩衝。** 像素是 texture 句柄，表格是共享記憶體視圖，影片幀從硬體解碼器直接成為 texture。舊版 React Native bridge 的失敗就是違反這條。
3. **零環境權限，能力即物件。** 腳本沒有 `fs`、`fetch` 這類全域；manifest 宣告需求，使用者同意後核心把能力物件交進 realm。這是讓 AI 生成的程式碼在使用者裝置上安全執行的唯一正解（物件能力模型，WASI、Fuchsia 同理）。這同時也是 App Review 4.7.2 的要求：下載的程式碼不得在沒有使用者許可下取得原生 API，所以能力模型就是上架的合規說明。
4. **保留式場景、髒區重繪。** 沒有變化就是 0 fps。
5. **能下載的只有資料與位元碼，能編譯的只有核心，中間是帶版本的穩定 ABI。** `require("pdf@2")`，核心同時提供 v1、v2，工具包宣告相容範圍。
6. **一份核心，薄外殼。** 殼只做 OS 堅持要原生做的事。文字輸入用原生輸入框疊在引擎畫面上，引擎只負責顯示。
7. **隔離與配額。** 每個工具一個 VM 實例，有記憶體上限、時間片和看門狗；腳本崩潰不影響引擎。所有變更都是指令，所以錄製、重播、undo、協作同步是副產品。

---

## 6. 高度可變與跨平台

### 可變性怎麼落地
- **工具 = 包**：manifest + 位元碼 + shader + 資產，安裝更新不經商店。內建工具也用同一格式，所以核心沒有特權功能。
- **文件 = 開放場景圖**：PDF 頁是 tile 來源節點，影片片段是 texture 來源節點，表格是虛擬化網格節點。同構所以可組合：把 PDF 頁渲成影片疊層，或把 CAD 視圖貼進簡報。
- **能力 = 帶版本的模組**：新增一種領域引擎只是多一個 L2 模組和一組介面，L1 不變。
- **可變性的邊界**：新工具、新 shader、新 manifest 走 OTA；新的節點契約或新的 L2 引擎才需要上架新版 App。

### 跨平台約束

| 平台 | 硬限制 | 設計如何吸收 |
|---|---|---|
| iOS | 無 JIT；引擎不可事後下載 | 位元碼直譯；所有 L2 引擎隨 App 出廠；shader 執行期編譯合法 |
| Android | Vulkan 驅動碎片化 | wgpu 自動回退 GL ES；JIT 可用，WASM 層可 AOT；Play Feature Delivery 可按需下載 CAD 模組 |
| Web | 無檔案系統；多執行緒需 COOP/COEP | 核心編成 WASM + WebGPU；腳本可直接跑瀏覽器 JS 引擎；儲存用 OPFS；各引擎 lazy load |
| 桌面 | 幾乎沒有 | 可把腳本 AOT 成原生，當效能上限參考 |

執行期向工具包公佈一份**能力檔案**（`jit:false, wasm:"interp", gpu:"metal", threads:6`），工具包宣告最低需求，缺的功能降級而不是崩潰。

---

## 7. 執行時：執行緒與幀管線

| 執行緒 | 負責 | 為什麼獨立 |
|---|---|---|
| 主執行緒（殼） | 視窗、輸入、IME、生命週期、無障礙 | OS 強制 |
| 腳本執行器（N 條） | 跑各工具的 VM，一個 VM 同時只在一條執行緒上 | 腳本卡住不能卡畫面 |
| 渲染執行緒 | 場景遍歷、裁切、編碼 GPU 指令、present | 穩定幀時間 |
| 工作池 | tile 光柵化、解碼、排版、公式重算 | 可平行、可丟棄 |
| IO | 檔案、網路 | 會阻塞 |

```
輸入(主) → 事件批次 → 腳本處理 → 指令批次 → commit(渲染)
        → 髒區判定 → 佈局(只算髒子樹) → 遍歷 → GPU → present
```

兩個短路：
- commit 後髒區為空，渲染執行緒直接睡到下一個事件。
- **鏡頭移動不經腳本。** 拖曳、縮放是核心手勢，直接改鏡頭矩陣，腳本只收 `camera.changed`。瀏覽文件的 60fps 完全不依賴腳本（同瀏覽器把捲動放在合成執行緒）。

規則：**管線的階段是任務，不是執行緒。** Web 單執行緒時同一張任務圖序列執行即可。只有主執行緒是特殊的。

---

## 8. 文件模型：場景圖是模型的投影

MVC 落在引擎層：**模型在 L2，視圖在 L1，控制器在 L3。** PDF 的真正結構由 PDFium 持有，場景圖裡只有一個「頁面 tile 來源」節點。編輯是對 PDFium 下指令，它回報哪些頁髒了，核心只重算那些 tile。場景圖從不假裝自己是 PDF。

### 節點按渲染契約分類，不按領域

| 節點 | 契約 | 背後的 L2 | 核心負責 |
|---|---|---|---|
| Tiled | `raster(level,x,y) → 圖` | PDFium 頁、CAD 2D 圖紙、巨圖 | LOD 選擇、快取、預取、低解析度補位 |
| Streamed | `frame(t) → texture` | 硬體解碼器 | 以播放頭為中心預解碼、seek 對齊關鍵幀 |
| VirtualGrid | `cell(r,c) → 內容` + 行列尺寸 | IronCalc | 可視範圍裁切、凍結列，只向 L2 要看得見的格 |
| Vector | 路徑、填色、描邊 | 無，核心自繪 | 曲線光柵化：第 1 階段用 vello_cpu 在工作池畫進 tile，vello_gpu 穩定後再走 GPU 向量路徑，不用 compute 版 vello（見第 12 節建材現況）、批次合併 |
| Text | 段落 + 樣式 | 無 | 排版（rustybuzz / parley）、字形圖集 |
| Mesh | 三角網格 + 材質 | OpenCascade 鑲嵌 B-rep | 子視口與獨立鏡頭、簡化 PBR |
| Effect | WGSL + 輸入 texture | 無 | 編譯、快取 pipeline、離屏合成 |
| NativeOverlay | 一個矩形 | 殼 | 把原生視圖對齊到該矩形：輸入框、相機、地圖 |

鏡頭掛在視口上，決定可見範圍與 LOD；Mesh 可帶嵌套鏡頭，所以 CAD 3D 視圖能放進 2D 文件。

### Tile 系統
- 格 256 或 512 裝置像素，層級 = ⌊log₂(縮放)⌉，快取鍵 `(來源, 層, x, y)`。
- GPU 記憶體預算（手機約 256 MB），LRU 淘汰；沿速度方向在視口外預取一圈。
- 尚未光柵化的格先拿低一層放大補位，縮放永不空白。
- 文字型 PDF 可從 PDFium 取出路徑成為 Vector 節點，縮到極大時改走向量。純 Rust 的 hayro 可當這條路的實驗（2026-10：994 份 PDF 成功率 97.6%，與 PDFium 持平，但效能未優化，每個 tile 重新解譯整頁，縮放慢 6 倍），現階段不押。
- **第 1 階段基線是 CPU 光柵、GPU 合成。** Tiled、Vector、Text 全在工作池光柵成 tile 再上傳，GPU 只做合成與 Effect。這是瀏覽器多年的做法，繞開手機 GPU 驅動的風險；GPU 向量路徑等 vello_gpu 穩定再上。

### L2 執行緒契約
L2 引擎必須宣告執行緒契約。PDFium 完全不是執行緒安全的：全行程只能有**一條** PDFium 執行緒，不是每份文件一條（pdfium-render 以全域互斥鎖序列化所有呼叫，多份文件也共用）。tile 排程要圍繞這條單一佇列設計：按視口距離排優先序、鏡頭移動時取消過時請求、同頁 tile 批次化，互動性靠排程而不是靠平行。IronCalc 整份重算是一個工作（0.9 的 `evaluate()` 沒有公開的髒格依賴圖，20 萬格每次編輯約 130 ms；髒追蹤要在 L1 外層自己做，或等上游補 DAG）。解碼器是會話式的。核心按契約排程。

### 零拷貝影片

| 平台 | 解碼輸出 → GPU texture |
|---|---|
| iOS / macOS | `CVPixelBuffer` → `CVMetalTextureCache` → `MTLTexture`，wgpu 從 hal 匯入 |
| Android | `MediaCodec` → `AHardwareBuffer` → Vulkan 外部記憶體 |
| Web | WebCodecs `VideoFrame` → `importExternalTexture` |

現況（2026-10）：Rust 生態沒有任何專案在手機上出貨過零拷貝解碼到 wgpu。wgpu 27 起的 `ExternalTexture` 只包裝已經是 wgpu texture 的平面，只有 DX12 與 Metal，到 wgpu 30 仍標示進行中；`CVPixelBuffer` 與 `AHardwareBuffer` 匯入要走 wgpu-hal 的 `create_texture_from_hal`；Web 的 `importExternalTexture` 仍是開放議題。Rerun 用外部 ffmpeg，Gyroflow 的硬體互通清單全部未完成，Cap 只有桌面。參考碼：`wgpu-external-frame`（2026-10，覆蓋 IOSurface、AHardwareBuffer、DMA-BUF、D3D12，未經實戰）。Metal 端 `CVMetalTextureCache` 是標準做法，真正新的只有 wgpu-hal 那段膠水。**退路：每幀一次 NV12 上傳**，1080p 約 3 MB/幀，現代手機 30 fps 可承受。零拷貝是效能目標，不是出貨前提。

時間軸每幀被求值成一張合成圖：t 時刻哪些片段可見、疊哪些 Effect。Effect 跑 WGSL，AI 寫的轉場和濾鏡直接進 GPU 路。音訊不走 GPU，在獨立音訊執行緒混音，和影片只靠時鐘對齊。導出 = 同一張合成圖離屏渲染到目標解析度，餵硬體編碼器。

---

## 9. 腳本與核心的接口

- **句柄**是 `(index, generation)` 的 slotmap 鍵，過期即失效。
- **指令批次**：腳本在一次事件處理裡把所有操作追加到指令緩衝（typed array），結束時一次交給核心。邊界穿越次數等於事件批次數。
- **同步查詢只允許讀**：命中測試、量測、節點屬性，從上一次 commit 的快照回答，不阻塞渲染。
- **大資料不進指令**：共享緩衝加顯式「轉移所有權」。
- **驗證**：每條指令過 schema，非法指令回錯誤事件，核心絕不 panic。
- **宣告式 UI 是使用者空間的程式庫，不是核心功能。** 核心只懂保留式場景加指令。工具列、面板這種小樹用腳本層的 diff 程式庫轉成指令；文件本體永遠走指令，不走 diff。

一次剪輯操作：點擊 → 命中測試（快照）→ 腳本把 `timeline.split(clip, t)` 寫進緩衝 → commit → 時間軸模型更新，兩個片段節點變髒 → 下一幀重新求值合成圖 → GPU。腳本耗時微秒級。

---

## 10. VM 的決定：先定 VM，表面語言可以晚定

邊界在位元碼，所以**現在必須定的是 VM**，表面語言之後靠編譯器加上去，ABI 不變。

VM 選 **Luau**：
- 「每個工具一個 VM」必須便宜。Luau 一個 VM 幾十 KB、啟動微秒級；QuickJS 數百 KB；Hermes 以 MB 計。
- 配額內建：記憶體上限走自訂配置器，時間片走 `interrupt` 回呼，`luaL_sandbox` 把全域凍成唯讀。
- 無 JIT 平台的純運算直譯速度屬第一梯隊。
- C++，隨 Rust 核心一起編譯（`mlua` 的 `luau` 特性），經 Emscripten 連 Web 也能編。
- 漸進型別讓發佈前的靜態檢查成立。

現況（2026-10）：mlua 0.12 的 `luau` 特性直接提供 `sandbox()`、`set_interrupt()`、`set_memory_limit()`、`heap_dump()`，配額模型成立。兩個坑：`interrupt` 在單一大型內建函式內不會觸發（例如 `string.rep`），要另外限制字串長度或移除這類內建；Luau 官方沙盒文件要求跨信任邊界的位元碼必須簽章，所以 manifest 簽章不是選配。2026-09 一份 x64 基準測試中 Luau 比 QuickJS-ng 快 2～8 倍。Rive 於 2026-01 把 Luau 當 .riv 檔內的腳本語言正式上線，證明這條路在 iOS / Android / Web 上可出貨。

代價：Web 上 Luau 跑在 WASM 裡再直譯，比直接跑瀏覽器 JS 慢；AI 寫 Lua 的熟練度低於 TS，但 API 面小、型別標注齊全時差距不大。之後要 TS 表面，roblox-ts 已證明 TS→Luau 能在生產規模運作。

---

## 11. 語義埠：讓 agent 與純分類器操控引擎

Computer use 要看截圖、算座標，是因為它面對別人的 App。我們擁有引擎，所以 agent 不看像素，而是拿到腳本拿到的同一份東西。**Agent 就是 L3 的另一個控制器**，走同一條指令協定、同一套能力模型。

### 一個介面，多種消費者
核心裡的特權服務，從上次 commit 的快照提供觀測、接受指令。同時餵 LLM agent、純分類器、螢幕閱讀器、語音控制、UI 測試、巨集錄製。無障礙樹就是 agent 的觀測，無障礙動作就是 agent 的動作。

| 通道 | 內容 | 成本 | 用途 |
|---|---|---|---|
| 語義樹 | 節點 id、角色、標籤、狀態、可用動作、場景與螢幕座標、z 序、是否被遮擋、是否在視口外 | 低 | 預設 |
| 領域視圖 | PDF 文字層含字框與閱讀順序、表格範圍與公式、時間軸片段、CAD 特徵樹 | 低～中 | 精確操作 |
| 增量 | 自上次觀測以來的變化 | 極低 | 驗證結果、長任務 |
| 像素 | 任一視口或節點的渲染，可指定解析度、可疊標記 | 高 | 視覺確認、文件內圖片、3D |

### 動作空間：有限、有型別、有標籤
每個可互動節點宣告 affordance：`名稱、參數 schema、自然語言描述、前置條件、效果類別`。Agent 用 `(nodeId, action, args)` 行動，不用座標。這份宣告同時是 UI 按鈕、無障礙動作選單、agent 的 tool-use schema。工具的 manifest 就是 agent 的工具清單。

- **LLM**：動作空間轉成文法，受限解碼只產生合法動作。
- **純分類器**：空間有限且被枚舉，一個只會輸出類別的模型就能操控。輸入是語義樹特徵加使用者意圖，輸出一個 actionId。連續參數靠槽位：量化選項或相對調整。訓練資料是指令日誌裡的 `(觀測, 動作)` 對，可在裝置上訓練。搞不定時升級到 LLM，形成成本階梯。

### 靜態與動態 UI
- **靜態 UI**（manifest 宣告）：動作空間發佈時編譯好，id 固定，agent 一開始拿到整張地圖。
- **動態 UI**（腳本生成、資料驅動、agent 自己生成）：引擎在 commit 時**強制可描述性**，沒有角色與標籤的互動節點直接被拒；id 帶 key 跨幀穩定；增量通道告知出現與消失。

### 2D 與 3D 的觀測
- **2D**：領域視圖加空間查詢 `hitTest`、`nodesIn`、`find`、`nearest`。「把第三段第二句畫底線」不需要看圖。
- **3D**：特徵樹與物件表；**ID 緩衝與拾取**（每像素寫物件/面 id，`pick(x,y)` 回物件、面、3D 點、法向）；**可請求的視角** `render(camera, highlight, marks)`，Set-of-Mark 標記由引擎原生疊上，編號與節點 id 嚴格對應；量測是查詢不是目測：`distance`、`volume`、`interference`。

### 效率

| 機制 | 效果 |
|---|---|
| 增量觀測 | 上下文不隨時間膨脹 |
| 預算觀測 | agent 給 token 預算，引擎回最重要的子集，長列表折疊 |
| 動作批次 | 一回合多動作一次提交 |
| 以增量驗證 | 省掉截圖回合 |
| 前置條件 | 不執行就能規劃，引擎能解釋「為何不可用」 |
| 視口外操作 | 不必捲動找元素 |

### 非同步任務與外部 agent
- **Job 是一等公民**：`{id, kind, principal, status, progress, budget, result, cancel}`。影片導出、雲端重建、派子 agent，全是 Job，進度與取消自動進入動作空間。
- **派外部 agent 是能力**：`agent.dispatch(goal, scope, budget) → Job`。scope 是範圍化能力票：只能觀測與操作指定子樹、指定動作類別、有預算。外部 agent 從不直接碰引擎，一律經能力代理，送回的動作走同樣驗證。結果以資料或事件回到場景，不會是程式碼。
- **樂觀並發**：每條指令帶 base version；指令日誌讓衝突可重放與 rebase，衝突子集被拒絕並回報。

### 安全
- **主體與授權**：使用者、每個工具、每個 agent 都是 principal。grant 定義哪些文件、哪些動作類別、多少預算、能否用像素通道。
- **動作類別決定確認政策**：可逆本地編輯自動放行；不可逆或對外（傳送、發佈、付款、刪檔、連新主機）必須經使用者確認。確認介面由**核心擁有的受信任區域**繪製，腳本與 agent 畫不進去（同瀏覽器網址列原理）。
- **來源標記對抗提示注入**：每段文字帶 `origin: user | tool | document | agent`。角色與動作標籤只能來自 manifest；腳本讀文件後生成的 UI 標籤標為 tainted。文件內容永遠是資料通道。
- **分類器的可達集合**：只能觸發可逆動作，不可逆動作不在其輸出空間。
- **回合即交易**：一個 agent 回合的所有指令是一組，可一鍵撤銷。
- **審計就是指令日誌**。
- **像素通道遮蔽**：`NativeOverlay`（密碼框）與敏感節點在給 agent 的圖裡遮掉。

### 對核心的回頭要求
語義埠要求核心從第一天就滿足：所有互動節點有角色與標籤（commit 時強制）、所有變更是帶主體與版本的指令、有一塊腳本畫不進去的受信任區域。一開始沒做，之後補不回來。

---

## 12. 如何實現

### 不是新語言，是運行時 + 協定 + 工具鏈

| Web | 這個架構 | 性質 |
|---|---|---|
| 瀏覽器引擎 | Rust 核心 + 各平台殼 | 運行時 |
| DOM + 事件 | 場景圖 + 指令/事件/觀測協定 | **協定，真正的「語言」，但是資料格式** |
| HTML 元素 | 八種節點契約 | schema |
| manifest / CSP | 工具包 manifest + 能力授權 | schema |
| JavaScript | Luau（或 TS 編譯而來） | 借 |
| WebGL shader | WGSL | 借 |

交付物：**一份規格 + 一個參考實作 + 一套 SDK**。

為什麼不造語言：借語言的引擎贏了（Unity/C#、Roblox/Lua、Unreal/C++），自造的付了代價（GDScript 後來加 C#、Dart 是 Flutter 的阻力）；使用者一半是 AI，AI 寫得最好的是訓練資料最多的語言，新語言在模型眼裡是零資料。真正要設計的是 **API**。

### 唯一真相來源：Rust 型別，生成一切
```
Rust 型別（節點、指令、事件、affordance、manifest）
   ├→ 二進位指令緩衝的編碼/解碼（FlatBuffers 式零拷貝）
   ├→ Luau / TS 型別定義與綁定
   ├→ JSON Schema（manifest 驗證、發佈時檢查）
   ├→ agent 的 tool-use schema（語義埠直接輸出）
   └→ 文件
```
affordance 的前置條件與效果用 CEL 或枚舉的效果類別，不自造規劃語言；IDL 用 FlatBuffers 或直接以 Rust 型別為 IDL。

### 買與造

| 造（架構本身，全 Rust） | 買或借 |
|---|---|
| 場景圖與 commit、髒區追蹤 | 渲染後端 wgpu |
| tile / 串流排程 | 2D 光柵 vello_cpu（第 1 階段）→ vello_gpu（穩定後）；不用 compute 版 vello |
| 指令協定與驗證 | 排版 parley + swash + rustybuzz |
| 能力代理與受信任區域 | 佈局 taffy |
| 語義埠 | 無障礙 AccessKit |
| VM 宿主（隔離、配額） | VM 本體 Luau（經 mlua） |
| 工具包格式與發佈工具鏈 | PDFium（pdfium-render）、IronCalc、OpenCascade、平台編解碼器 |
| 各平台薄殼 | |

### 底座
選 **Linebender 棧的零件**當底：vello_cpu / vello_gpu、parley、AccessKit、kurbo、peniko。masonry 只借保留式樹的設計，不用它的程式碼（仍是 alpha，沒有 iOS）。從 Bevy 借任務池與資產串流的設計。產品是文件導向的：文字多、靜止多、保留式。

**借設計不借碼。** 不 fork 任何現有專案：Univer 借 Scene / Layer / Viewport 與元件骨架的分層；Typst 借 comemo 式的受限記憶化當失效追蹤的參考；Zed 的 embedded_gpui 借宿主與客端的邊界（客端只產保留式顯示清單、宿主重播、宿主在幀路徑上絕不呼叫客端）；MCP Apps 借「一個 Tool 結構同時帶 inputSchema 與 UI 中繼資料、visibility 分 model / app」；Apple App Intents 借 entity schema 的形狀。

### 建材現況（2026-10-24 調查）

| 建材 | 現況 | 對設計的影響 |
|---|---|---|
| Vello | compute 版 `vello` 已被官方降為 research / experimental；Android 上 Adreno 7xx 有 8/23 條 pipeline 編譯失敗，Mali 冷啟動 shader 編譯 0.7～1.2 秒。`vello_gpu`（前身 hybrid，CPU 前處理 + GPU 光柵、不用 compute shader）是官方預定的主力，仍是 beta、無 API 穩定承諾。`vello_cpu` 0.3 最成熟 | 第 1 階段 CPU 光柵 tile + GPU 合成；GPU 向量路徑等 vello_gpu |
| Parley 0.12 | 有 `PlainEditor`（游標、選取、剪貼簿、IME 預編輯）；每版都有破壞性變更；AccessKit 整合已從 crate 移出 | 可用，預期跟版成本 |
| AccessKit | iOS adapter 2026-05 才有基本版（0.2.x），Android 0.9，均無富文字 | 語義埠與無障礙樹共用一份來源，但 iOS 深度有限 |
| Masonry / Xilem | 0.4 alpha（2025-10），無 iOS，Android 不可用 | 只借設計 |
| Blitz | HTML 盒模型引擎，非通用場景圖，無 iOS | 不用 |
| PDFium / pdfium-render | PDFium 不是執行緒安全；pdfium-render 0.9 以全域互斥鎖序列化 | 每行程一條 PDFium 執行緒，見第 8 節 |
| hayro | 純 Rust PDF，成功率與 PDFium 持平，效能未優化，Typst 0.14 已用於嵌入 PDF | 實驗線，不押 |
| IronCalc 0.9.1 | 整份重算、無公開髒格 DAG，1.0 未到 | 髒追蹤自己做 |
| CAD | Fornjot 2026-06 封存；Truck 活著但小、無圓角；opencascade-rs 標示 WIP；Zoo 的幾何引擎在雲端 | OCCT 走自己的 C FFI，維持原案 |
| 零拷貝影片 | 無人在手機上出貨；wgpu `ExternalTexture` 進行中、僅 DX12 / Metal | 見第 8 節，備妥每幀一次上傳的退路 |
| Luau / mlua 0.12 | sandbox、interrupt、memory limit 齊全；interrupt 不進內建函式；位元碼需簽章 | 見第 10 節 |
| Rust UI 上手機 | egui 0.36 才修好 iOS / Android IME；Makepad 到 2026-10 仍有手機選取問題；Slint 1.17 起有 viewer 上架 | 不變量 6 正確：文字輸入交回原生 |
| iOS 規則 | 2.5.2 無 JIT 未變；行程內 JavaScriptCore 只能直譯；iOS 26 的 Trusted Execution Monitor 更緊；4.7 允許 mini app 與外掛，4.7.2 要求使用者許可才能暴露原生 API | 鐵律成立；能力模型即合規文件 |

### 分階段，按風險排序

**第 0 階段（數週）兩個 spike，任何一個不過就回到設計。** 順序配合開發機（第 14 節）：Windows 是主力，Android 是 GPU 驅動風險所在，iOS 殼最後。
1. Rust + wgpu 畫布 + PDFium tile 管線 + 手勢直接驅動鏡頭。先在 Windows 做桌面殼（DX12，滑鼠與觸控板手勢），量幀率、首幀時間、記憶體；接著以現成的 NDK 上 Android 真機，驗 Adreno / Mali；最後把殼搬到 iOS（Mac）。光柵走 vello_cpu 進 tile，PDFium 單佇列。
2. iOS 硬體解碼 → `CVMetalTextureCache` → wgpu texture 零拷貝，疊一個 WGSL 濾鏡即時播放。量每幀拷貝次數是否為零。只能在 Mac 做；參考 `wgpu-external-frame` 的 IOSurface 路徑。不過就退到每幀一次 NV12 上傳，記錄實測頻寬，不擋第 1 階段。

**第 1 階段，核心 MVP：** Tiled、Vector、Text、NativeOverlay 四種節點、指令協定、能力代理、每工具一個 Luau VM、manifest 與發佈工具鏈（含位元碼簽章）、語義埠的觀測與動作。渲染基線是 CPU 光柵 tile + GPU 合成。語義埠只做有先例可循的部分：語義樹、增量、以 id 操作、效果類別；受限解碼文法與純分類器列為研究線，不擋 MVP。第一個工具是 PDF 閱讀與批注，**必須以工具包形式走完整條發佈路徑**。

**第 2 階段：** Streamed 與 Effect（影片）、VirtualGrid（表格）、Job、agent 經語義埠完成一個完整任務。

**第 3 階段：** Mesh（CAD）、外部 agent 能力票、Web 目標。

### 何時才值得造新語言
只有語言本身就是產品、且現有語言在語義上做不到時。這個架構沒有這樣的缺口。

---

## 13. 資源估算

### App 體積（iOS arm64，release，strip）

| 元件 | 體積 |
|---|---|
| Rust 核心 | 5–10 MB |
| wgpu + naga | 3–5 MB |
| vello / parley / swash / rustybuzz（字型用系統的） | 3–5 MB |
| Luau VM | 1–1.5 MB |
| PDFium | 10–20 MB |
| IronCalc | 2–3 MB |
| 影片（用平台編解碼器，不帶 FFmpeg） | 0–2 MB |
| 殼、內建工具包、shader | 3–8 MB |
| **小計（PDF + 表格 + 影片）** | **約 35–60 MB** |
| OpenCascade（裁過） | 30–60 MB |
| **含 CAD** | **約 70–120 MB** |

同類 App（Acrobat、Excel、CapCut）200–400 MB，Shapr3D 約 400 MB。iOS 必須一次全帶；Android 可用 Feature Delivery 按需下載 CAD；Web 各引擎 lazy load。

### 裝置執行期
tile 快取 GPU 約 256 MB 預算；影片**就地引用**（iOS 用 PHAsset 不複製），只在需要時產 1080p 代理片（約 50 MB/分鐘），有上限；工具包每個 100 KB–數 MB；指令日誌有保留期限。

### 開發機硬碟
Xcode + 模擬器 20–25 GB；Android Studio + SDK + NDK 15–20 GB；Rust 工具鏈 3 GB；Rust `target/` 20–60 GB；PDFium 用預編譯（bblanchon/pdfium-binaries）；OpenCascade 四平台建置 10–20 GB；Emscripten、Node 3–4 GB；測試語料 5–10 GB。**最少 100 GB，舒適 150–300 GB**，記憶體建議 32 GB。

縮小 `target/`：`[profile.dev] debug = "line-tables-only"`、`split-debuginfo = "unpacked"`、共用 `CARGO_TARGET_DIR`、`cargo sweep --time 7`。

### 外接 SSD（若在 Mac 上開發）
Thunderbolt 3/4 或 USB4 NVMe 外接盒約 2,800–3,200 MB/s，與內建差距 5–10%；USB 3.2 Gen 2 隨身 SSD 慢 10–25%；**Gen 2×2 在 Apple Silicon 掉回 10 Gbps，別買**。硬碟選有 DRAM 的 Gen4 TLC（990 Pro、SN850X、P41、T500；SN770、990 EVO Plus 無 DRAM 不選）。一定格式化 APFS，Spotlight 排除，關閉硬碟睡眠，保留擁有權。

---

## 14. 開發環境：Windows 主力 + Mac 建置 iOS

| Windows 桌機（主力） | Mac（iOS 建置與測試機） |
|---|---|
| Rust 核心所有開發 | iOS 殼（Swift，幾千行） |
| Windows 殼（wgpu → DX12/Vulkan） | 真機效能驗證、Instruments、Metal 除錯器 |
| Android（NDK） | iOS 版 PDFium / OpenCascade 交叉編譯 |
| Web（wasm） | 零拷貝影片、IME 疊層等 Metal 專屬路徑 |
| OpenCascade / PDFium 的 Windows、Android 建置 | 當 CI runner：每次 push 自動建 iOS |

iOS 離不開 Mac 的部分：Xcode 編譯、模擬器、真機除錯、簽章上傳（`rcodesign` 等可在 CI 替代，不適合開發）。Mac 約佔 20–30% 工作量。

四個做法：iOS 專案用 XcodeGen / Tuist 文字定義；平台相關程式碼縮到最小藏在 trait 後；CI 每次 push 建全部目標；Windows 上 Defender 排除 `target/`、開長路徑、開發者模式開符號連結、注意檔案系統不分大小寫。Mac 不夠時 Mac mini M4 當無頭建置機；Apple Developer Program 附 Xcode Cloud 每月 25 小時。

---

## 15. 未決事項

- 架構服務的對象：終端使用者、開發者、AI 建構者，或三者。
- 商業模式（理念是「工具免費」，收費點未定）。
- 表面語言：直接用 Luau，或 TS 編譯到 Luau。
- affordance 的前置條件與效果如何形式化（決定 agent 能否真正規劃）。
- 分類器的特徵與訓練管線。
- 範圍化能力票的格式。
- 受信任區域與確認流程的 UX。
- 文件模型的抽象邊界放在哪：核心只到「tile 來源 / texture 來源 / 虛擬網格」，再往上會失真，需要原型回答。
- 文字與 IME：排版、雙向、選取、組字框、無障礙樹，自繪 UI 的稅，靠規則 6 把輸入交回原生。
- iOS 上的 WASM 擴充層只能直譯，開發者自訂解碼器在 iOS 不實用。

---

## 16. 先例對照

| 系統 | 快核心 | 可變皮 | 啟示 |
|---|---|---|---|
| Flutter | C++ Impeller | Dart | 一份核心 + 薄 embedder 跑六平台可行；Dart AOT 不能 OTA，Shorebird 另寫直譯器才做到，印證鐵律 |
| Figma | C++ → WASM 自繪畫布 | JS 插件、React 面板 | 畫布自繪、面板用現成 UI 的分工 |
| Roblox | C++ | Luau | 數百萬不可信腳本在 iOS 上安全執行 |
| Blender / Neovim / Emacs | C | Python / Lua / Elisp | 內建功能也走外掛路徑，核心才會真的小 |
| Zed | Rust，自研 GPU UI | — | 生產力工具用遊戲引擎手法達到 120fps |

### 2026-10-24 補查：誰已經做到哪一層

結論：**沒有人做出整套，但每一層都有人做到了。** 真正沒人做過的是組合本身。

| 設計元素 | 最接近的先例 | 還缺什麼 |
|---|---|---|
| 編譯進 App 的 GPU 核心 + 可下載 Luau 位元碼 | **Rive**：Luau 腳本 2026-01 正式上線，腳本在 .riv 檔內、以型別化 protocol 圈住能碰什麼，C++ 渲染器跑 iOS / Android / Web；2026-10 又加 TypeScript 語法編成 WASM 的 AnimaScript | 動畫執行時，非文件引擎；腳本與檔案同作者；每腳本配額與多 VM 隔離未公開 |
| 每工具一 VM + 時間片 + 記憶體上限 + 能力授權 + 宿主重播顯示清單 | **Zed embedded_gpui**（2026-07，實驗）：WASM 元件跑完整 GPUI 程式，wasmtime epoch 截止 + 記憶體上限，WASI 預設只給 stdout，客端輸出保留式顯示清單由宿主重播 | 桌面、WASM、未進產品 |
| 多種文件共用一張場景圖與鏡頭 | **Univer 1.0**（2026-09）：Sheets / Docs / Slides / Boards / Bases / PDF 全在同一個 Scene → Layers → Viewports 引擎上，插件 + 指令系統 | Canvas2D + TypeScript，無 GPU，無沙盒工具包 |
| 一份渲染核心擴到多種編輯器 | **Figma**：C++ → WASM 渲染器，2023 起 WebGPU，Slides 是同一份節點模型上的視口操作，外掛跑 QuickJS-in-WASM 沙盒 | 網頁技術棧；無 PDF / 影片 / CAD |
| 直譯 DSL + agent 生成 App | **Makepad Splash**（2026）：直譯式 UI DSL、熱重載；2026-05 GOSIM 的「A2App: Agent Generated Apps」 | 無沙盒、配額、能力模型、包格式 |
| iOS 接受「原生核心 + 直譯下載邏輯」 | **Lynx 4.x**（PrimJS）、**微信小程序 Skyline**、**Shorebird**（Dart 直譯器）、**HybridCLR**（自稱進入前 500 免費 iOS App 約 100 個） | 全是第一方信任程式碼 |
| 語義樹 + 以 id 操作 | **Playwright MCP** 的 aria snapshot 帶 ref 與座標框；Anthropic 瀏覽器工具的 `read_page` → ref 操作 | 無遮擋、視口外、affordance 宣告 |
| 一份動作宣告同時餵 UI 與模型 | **MCP Apps** 規格（2026-01-26 穩定）：同一個 Tool 結構帶 inputSchema 與 UI 中繼資料，iframe 與模型呼叫同一條 `tools/call`；**Builder.io Agent-Native**（2026-10-14）：定義一次動作，產生 UI、agent、MCP、CLI | 無前置條件與效果類別、無語義樹、無沙盒 |
| OS 層級型別化動作空間 | **Apple App Intents + View Annotations**（onscreen awareness，iOS 27 出貨）；**Android 16 AppFunctions** | 無樹，無跨 App 場景圖 |
| 來源標記防提示注入 | **CaMeL**（2025）：每個值帶 user / CaMeL / tool 來源與允許讀者；**FIDES**（2025） | 純研究，沒進任何 UI 引擎 |
| 「代碼免費，基底才重要」 | **Ink & Switch** Malleable software（2025-06）；Software as Content（2026-03） | 論述，非引擎 |

### 查無先例的部分

- 效果類別決定確認政策，且確認介面由核心擁有的受信任區域繪製。
- 用純分類器而非 LLM 在枚舉動作空間裡選動作。
- 受限解碼套在 GUI agent 的動作上（只有工程先例，無論文）。
- affordance 同時當按鈕、無障礙動作、agent tool schema 與解碼文法。
- 帶遮擋與視口外資訊的語義樹 + 領域視圖 + 增量通道。
- 在 App Store 上執行**第三方、不可信**的可下載 Luau 工具。Rive 的腳本同作者，Roblox 是單一特例。

這些正是「一開始沒做，之後補不回來」的三件事（第 11 節末）所在，也是這個架構的護城河。

### 來源（2026-10-24 查證）

- Rive 腳本上線：https://rive.app/blog/scripting-is-live-in-rive
- Zed embedded_gpui：https://github.com/zed-industries/embedded_gpui
- Univer 渲染架構：https://docs.univer.ai/blog/rendering
- Figma WebGPU 渲染：https://www.figma.com/blog/figma-rendering-powered-by-webgpu/
- Figma 外掛沙盒：https://www.figma.com/blog/an-update-on-plugin-security/
- Makepad：https://github.com/makepad/makepad
- Lynx 執行時：https://lynxjs.org/guide/scripting-runtime/
- Shorebird 架構：https://docs.shorebird.dev/code-push/system-architecture/
- App Store 審核指南：https://developer.apple.com/app-store/review/guidelines/
- Vello 發行紀錄：https://github.com/linebender/vello/releases
- Parley 發行紀錄：https://github.com/linebender/parley/releases
- AccessKit 發行紀錄：https://github.com/AccessKit/accesskit/releases
- pdfium-render：https://docs.rs/pdfium-render/latest/pdfium_render/
- hayro：https://github.com/LaurenzV/hayro
- IronCalc 重算效能議題：https://github.com/ironcalc/IronCalc/issues/849
- Fornjot 封存：https://github.com/hannobraun/fornjot
- wgpu-external-frame：https://github.com/water-rs/wgpu-external-frame
- mlua Luau 介面：https://docs.rs/mlua/latest/mlua/struct.Lua.html
- Luau 沙盒規範：https://luau.org/sandbox/
- Luau 對 QuickJS-ng 基準：https://quirrel.io/doc/performance.html
- MCP Apps 規格：https://github.com/modelcontextprotocol/ext-apps/blob/main/specification/2026-01-26/apps.mdx
- A2UI 規格：https://a2ui.org/specification/v1.0-a2ui/
- Apple Intelligence 指南：https://developer.apple.com/wwdc26/guides/apple-intelligence/
- Android AppFunctions：https://developer.android.com/ai/appfunctions
- Playwright MCP：https://playwright.dev/mcp/introduction
- Builder.io Agent-Native：https://www.builder.io/blog/introducing-agent-native
- CaMeL：https://arxiv.org/abs/2503.18813
- Malleable software：https://www.inkandswitch.com/essay/malleable-software/

查證時標記為**未能確認**的項目：Rive 的 WASM 在 iOS 上是直譯或 AOT、Rive 每腳本配額；微信 Skyline 底層是否為 Flutter 引擎；Robrix 是否真的上架；Vello 在 iOS / Metal 上的實測；Static Hermes 的 AOT 是否已成預設；Canva 的渲染器內部。

一句話總結：**這是一個帶 GPU 渲染器與能力代理的微核心，上面跑現成語言的位元碼。** 效能來自把工作推進 shader 與核心，可變性來自把所有語義推出核心，跨平台來自核心只依賴 wgpu 與 C ABI，可被 agent 操控來自每個節點都自帶語義與動作。
