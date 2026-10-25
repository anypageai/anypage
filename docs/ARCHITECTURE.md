# AnyPage 引擎架構 v2：一個頁面渲染所有內容的引擎

2026-10-24 重寫。狀態：**提案，尚無任何原型驗證**。所有效能主張都要靠第 15 節的第 0 階段 spike 來證實或推翻。

同日修訂：定位從「類瀏覽器的執行時」改為「引擎」。瀏覽器只當對照組；重點是在同一個頁面裡，高效率地渲染靜態、流式、非同步與操作中的內容（第 3 節）。同日再修訂：iOS、macOS、Windows、Linux、Android、Web 第一天一起支援，其他平台靠移植契約接上（第 14 節）。

v1「文件引擎」版保存在 [archive/ARCHITECTURE-v1.md](archive/ARCHITECTURE-v1.md)。v2 保留 v1 的鐵律、不變量、執行緒模型、VM 決定、語義埠與 2026-10 調查結果，改掉的是定位，以及模型住在哪裡。

### v1 → v2 改了什麼

| 面向 | v1 | v2 |
|---|---|---|
| 定位 | 讓文件與媒體工具被低成本產生的框架 | 一個頁面渲染所有內容的引擎：靜態、流式、非同步、操作共用一個迴圈；文件只是其中一類內容 |
| 模型住在哪 | L2：PDFium 等引擎持有文件，場景圖是投影 | L1 的計算圖；L2 降為算子，外部引擎的狀態以資源格表示 |
| 下載的東西 | 工具包驅動引擎 | 頁面組裝計算圖、處理事件；重活由編進 App 的算子執行 |
| 渲染 | 場景圖 + 八種節點契約 | 場景是求值結果；八種契約成為出口格的輸出種類 |
| 流與非同步 | 影片是一種節點契約，非同步只有 Job | 串流輸入格、待定格與 Job 是一等公民，和靜態內容共用排程、時鐘與合成 |
| 組合 | 同構節點可互相嵌入 | 頁面之間以型別化埠連圖，組合發生在計算裡 |
| agent | 讀語義樹與領域視圖 | 另可讀計算圖本身：值、依賴、來源 |
| 平台 | Windows 主力，Android 次之，iOS 殼最後 | iOS、macOS、Windows、Linux、Android、Web 第一天一起建；其他平台靠移植契約 |
| 第 0 階段 | PDFium tile spike + 零拷貝影片 | 一個頁面同時跑靜態、流式、非同步與操作中的內容；腳本建圖；零拷貝影片 |

---

## 1. 定位：一個頁面渲染所有內容的引擎

AnyPage 是一個**引擎**，不是瀏覽器。它的工作是把性質完全不同的內容放進**同一個頁面**，用同一個求值迴圈、同一個時鐘、同一個 GPU 合成器，高效率地一起跑：

- **靜態內容**：文件頁、圖片、版面、表格。
- **流式內容**：影片、動畫、音訊、即時資料、模型逐字輸出、協作者的游標。
- **非同步內容**：網路請求、雲端運算、長時間的算子、agent 任務。
- **操作中的內容**：拖曳、縮放、編輯、手勢，以及 agent 的動作。

瀏覽器也能把這些東西放進一頁，但每一種都是各自的黑盒管線：`<video>`、`<canvas>`、iframe、WebGL、worker，靠 DOM 和主執行緒上的 JavaScript 黏起來，彼此搶同一條主執行緒，失效靠猜。AnyPage 用遊戲引擎的做法：**一個世界、一個時鐘、一個排程器、一個合成器**，每種內容只是同一張計算圖上時間性不同的格（第 3 節）。

頁面仍然可以像網頁一樣用位址下載、在沙盒裡執行、不用安裝（第 5 節），但那是載入方式，不是身份。

**任何平台。** 同一份核心跑在 iOS、macOS、Windows、Linux、Android、Web 上。鴻蒙、visionOS、電視、車機、嵌入式裝置乃至遊戲主機，只要實作一層薄殼就能接上（第 14 節）。

做得到靠三件事，也是整份架構的骨架：

1. **頁面被內化成計算。** 瀏覽器的頁面是一棵可變的文件樹（DOM），腳本命令式地改它，瀏覽器事後猜哪裡要重算。AnyPage 的頁面是一張**引擎擁有的計算圖**：輸入、由輸入導出的值、最後導出的畫面，全部是圖上的格，依賴關係是顯式的。引擎因此能精確地增量重算、在沒有變化時完全靜止、把任何狀態重放出來、回答「這個值為什麼是這樣」。
2. **下載的程式碼只組裝計算，執行計算的是編進 App 的算子。** 頁面裡的腳本負責建圖、處理事件、發指令；每幀、每像素、每格子的工作由原生算子與 GPU 算子完成。效能不取決於腳本直譯器，這是 iOS 鐵律（第 2 節）的結構化解法。
3. **agent 是一等使用者。** 瀏覽器上的 agent 只能看截圖或爬 DOM。AnyPage 的 agent 讀到的是計算本身：每個格的值、依賴、來源，以及每個可用動作的型別。動作是有型別的指令，不是座標。

**文件不是重點。** PDF、表格、影片、CAD 只是「部分輸入來自檔案」的頁面。格式知識住在匯入算子、匯出算子與處理器頁面裡。L1 核心不認識任何格式，就像瀏覽器核心不認識 PDF，內建的 PDF 檢視器只是另一個頁面。

---

## 2. 背景與鐵律

### 放棄的方向（全部）
- 每人一台 Contabo VPS + Hermes agent 的「雲端電腦」模式
- Modal 上的垂直服務（照片→3D、土地測量）
- Streams 個人訊息流
- Play / instant-play 配方式小遊戲

舊 repo 只當備份，新架構在本 repo。

### 理念
**代碼本身是免費的。** AI 讓產生程式碼的成本趨近於零，單一用途的工具在這個時代都應該是免費的。要做的不是某一個工具，而是讓工具能被人或 AI 低成本產生、即時下載、安全執行的**執行時**。

### 核心矛盾
HTML 是唯一能在 App 裡動態下載執行的東西，但 HTML / WebView 效率低，PDF 在 HTML 裡坑極多，影片剪輯在 HTML 裡不可能。AnyPage 要成為**第二個能動態下載執行的東西，而且有遊戲引擎等級的效能**。Figma、Zed、CapCut 內部都是「原生引擎 + 薄控制層」；Roblox 證明這種結構能上架 App Store。

### 鐵律
**在 iOS 上，「可下載的程式碼」等於「被直譯的程式碼」。** iOS 禁止第三方 App 使用 JIT（2026-10 查證，規則未變，iOS 26 的 Trusted Execution Monitor 更緊）。合法例外只有兩個：

1. **GPU shader。** Metal / Vulkan / WebGPU 都允許執行期編譯 shader 原始碼，天生被沙盒隔離。
2. **編進 App 的快路徑。** 程式碼隨 App 出廠，下載的東西只給參數。

v1 的結論是「每幀工作全放進這兩個例外，腳本層只處理事件」，靠紀律維持。v2 把它變成結構：**下載的頁面描述一張計算圖，圖上的重活只能由原生算子（例外 2）或 GPU 算子（例外 1）執行。** 腳本算子存在，但有預算，只在事件時或低頻求值，從不在每幀路徑上。腳本語言的選擇因此是人因與安全問題，不是效能問題。

### 和遊戲引擎分道的地方
- **沒有變化就是 0 fps**（耗電）。計算圖讓這件事自動成立：沒有髒格、沒有依賴時間的出口格，就沒有求值。
- **文字輸入交回原生**：IME、選取、無障礙是自繪 UI 的無底洞。

---

## 3. 一個頁面，所有內容

這一節是引擎和瀏覽器真正不同的地方。機制的細節在第 4、8、9 節。

### 3.1 內容的時間性

每種內容在圖上都是格，差別只在它**什麼時候會變**：

| 時間性 | 例子 | 在圖上的形狀 | 什麼時候算 |
|---|---|---|---|
| 靜態 | 文件頁、圖片、版面、公式結果 | 導出格，結果快取 | 只在依賴變了時 |
| 拉式串流 | 影片、動畫、音訊、時間軸播放 | 依賴時間 `t` 的導出格 | 播放時每幀，只算依賴 `t` 的子圖，並預先求值 |
| 推式串流 | 即時資料、模型逐字輸出、協作游標、感測器、網路串流 | 串流輸入格：追加，或最新者勝，帶背壓 | 資料到達時合批，進下一次 commit |
| 非同步 | 網路請求、雲端運算、長算子、agent 任務 | 待定格：`pending → ready / error`，背後是一個 Job | 完成時變成一條指令；完成前畫佔位與進度 |
| 操作 | 拖曳、縮放、編輯、手勢、agent 動作 | 指令，改輸入格或資源格 | 立即，最高優先 |

拉式串流又分兩種：每幀都變的（動畫），以及可預測的（時鐘、游標閃爍、倒數）。後者宣告自己的值在什麼時間之前不變，引擎只在那一刻醒來（第 4.8 節）。

因為全部是同一種東西，它們可以互相依賴：即時資料流驅動一張圖表，圖表疊在影片上，影片的濾鏡參數由使用者拖曳的滑桿決定，旁邊一個非同步任務算完後把結果填進表格。這在瀏覽器裡是五套 API 黏在一起；在這裡是一張圖。

### 3.2 一個迴圈排程全部

每一幀有一個截止時間：60 Hz 是 16.6 ms，120 Hz 是 8.3 ms。排程器按這個順序填滿預算：

1. **操作**：輸入到像素的延遲最敏感。指令立刻 commit，受影響的子圖最先求值。
2. **可見的串流**：播放中的影片、正在流入的資料。
3. **剛完成的非同步結果**：同一幀到達的結果合批進同一次 commit，不會一個結果觸發一次重排。
4. **可見但缺漏的靜態內容**：缺的 tile 先用低解析度補位。
5. **預取與背景工作**：預算有剩才做，隨時可取消。

超出預算時降級，不卡住：串流跳過中間幀，靜態內容先顯示低解析度，非同步結果延到下一幀。某一種內容的負載不會擴散成整頁卡頓。

### 3.3 只重畫有變的部分

引擎從圖上就知道哪些輸出依賴 `t`、哪些依賴串流、哪些是靜態，所以分層不是猜的，而是從依賴關係導出來的：

- 時間性不同的區域各自成為一個 GPU 圖層。靜態圖層是快取好的 texture。
- 每一幀只重畫格有變的圖層，最後一次合成全部圖層。
- 每幀成本跟「變了多少」成正比，不跟頁面大小成正比。一個 4K 頁面角落有一段 30 fps 影片和一條即時行情，每幀只付影片、行情和一次合成的錢。
- 什麼都沒在動時，0 fps。

瀏覽器也有合成圖層，但圖層的提升靠啟發式規則，一次 DOM 變更就可能讓整頁重新佈局。

### 3.4 時鐘與同步

- **一個主時鐘。** 拉式串流按這一幀**預計上屏的時間**求值，而不是按「現在」，這樣影音才對得上。有音訊播放時，音訊執行緒就是主時鐘，影片跟著它走。
- **推式串流帶來源時間戳。** 每幀合批時，狀態類資料（游標、價格）取最新值；日誌類資料（逐字輸出、訊息）追加。串流文字只重新排版最後一段。
- **非同步結果到達時，值被記錄成輸入。** 重放因此仍然是決定性的（第 4.5 節）。

### 3.5 背壓與預算

- 每個串流輸入格有上限與策略：丟最舊的、只留最新的、或讓生產者等。生產者可以是網路、解碼器或腳本。
- 每個頁面、每條串流都有預算：CPU 時間、GPU 記憶體、硬體解碼會話。硬體解碼會話的數量有上限，各裝置不同，要實測；超過時，螢幕外的影片暫停或改用較低解析度解碼。
- 非同步 Job 有預算，可取消。結果的出口格離開視口、又沒有別人需要時，Job 可以被取消，這是需求驅動求值的直接結果。

### 3.6 和瀏覽器的差別

瀏覽器是對照組，不是目標。值得借的有：位址定址、免安裝、沙盒、可檢視原始碼、規格先於實作。

| 瀏覽器 | AnyPage | 差別 |
|---|---|---|
| `<video>`、canvas、WebGL、worker 各一條管線 | 一個求值迴圈、一個排程器、一個合成器 | 串流、非同步、操作共用同一個預算與時鐘 |
| `fetch` 與 Promise 回呼直接改 DOM | Job + 待定格，完成時合批成指令 | 結果到達不會觸發整頁重排 |
| 網址、HTML 文件 | 頁面位址 + 頁面包（manifest、圖範本、Luau 位元碼、WGSL、資產、簽章） | 載入方式相同；內容是計算，不是標記 |
| DOM | **計算圖**：輸入格、導出格、資源格、出口格 | 依賴顯式，引擎精確失效；畫面是圖的導出值 |
| CSS 樣式與佈局 | 樣式是資料，佈局是原生算子（taffy） | 佈局只算髒子圖 |
| JavaScript（JIT） | Luau 位元碼（直譯）：建圖、處理事件、發指令 | 不在每幀路徑；效能來自算子 |
| WebGL / WebGPU | GPU 算子（WGSL） | 同一條合法例外 |
| WebAssembly | 選配的原生擴充算子 | 非 iOS 可 AOT；iOS 只能直譯 |
| 內建 PDF 檢視器 | 處理器頁面 + 匯入算子 | 沒有特權：內建處理器和第三方用同一格式 |
| Web API | 能力物件 | 零環境權限，manifest 宣告、使用者同意後才交進 realm |
| 同源政策 | 主體（principal）+ 授權（grant） | 粒度到格、埠、動作類別與預算 |
| 網址列（受信任 UI） | 核心擁有的受信任區域 | 確認介面頁面與 agent 畫不進去 |
| iframe + postMessage | 頁面之間的**型別化埠**，跨 realm 連圖 | 組合發生在計算裡，不是貼像素 |
| 無障礙樹（事後補） | 語義埠：由圖與場景直接導出 | agent 與螢幕閱讀器共用同一份 |
| DevTools | 圖檢視器：值、依賴、來源、時間旅行 | 重放是模型的副產品 |
| 檢視原始碼 | 頁面可檢視、可 fork、可修補 | 可塑軟體：人或 AI 改別人的頁面 |
| 多行程隔離 | 每頁一個 VM realm + 配額，單一行程 | iOS 一般 App 不能自建子行程；下載的程式碼從不是原生碼，這才是邊界 |

要避開的有：每種內容一條黑盒管線、可變文件樹加命令式腳本（失效靠猜、佈局抖動）、效能依賴 JIT、粗粒度的同源政策、無障礙事後補。

---

## 4. 計算模型：頁面是一張可增量求值的圖

這是 v2 的核心，「內化成可計算的運行方式」在這裡定義。

### 4.1 四種格

| 格 | 值從哪來 | 誰能改 | 例子 |
|---|---|---|---|
| 輸入格 | 外部：使用者、檔案、網路、時鐘、感測器、鏡頭 | 只能經由指令 | 文字內容、滑桿值、檔案位元組、當前時間 `t`、視口與鏡頭 |
| 導出格 | 一個算子作用在其他格上 | 沒有人；依賴變了引擎自動重算 | 版面、公式結果、tile 圖、合成後的影格、語義樹 |
| 資源格 | 外部引擎持有的狀態，圖上只有句柄與版本 | 只能經由指令，每次變更提升版本 | 開啟中的 PDFium 文件、OCCT 的 B-rep、硬體解碼會話 |
| 出口格 | 把導出值送出引擎 | 能力決定它能不能存在 | 畫到螢幕的視口、音訊輸出、匯出檔案、網路送出 |

輸入格可以是**串流**：追加，或最新者勝，帶背壓。導出格與資源格可以處於**待定**狀態，背後是一個 Job，下游看得到 pending、progress、error，先畫佔位。兩者都在第 3 節說明。

格的粒度是粗的：一頁、一個 tile、一個範圍、一個片段，不是每個像素或每個字元。細粒度資料放在格的值裡，用型別化緩衝（欄式陣列、texture 句柄、字形串）。目標規模是每頁 10⁵～10⁶ 個格，單一輸入變更的失效傳播在幀預算內完成，由 spike 證實。

### 4.2 算子

算子是有宣告簽章的函式：輸入型別、輸出型別、執行緒契約、成本等級、是否決定性。三種執行類別：

| 類別 | 實作 | 何時執行 | 例子 |
|---|---|---|---|
| 原生算子 | Rust / C / C++，編進 App（L2） | 任何時候，含每幀 | 佈局、排版、光柵、解碼、格式匯入匯出、公式求值、幾何鑲嵌 |
| GPU 算子 | WGSL，執行期編譯 | 任何時候，含每幀 | 濾鏡、轉場、合成、著色 |
| 腳本算子 | Luau 函式 | 只在事件時或低頻求值，有時間與記憶體預算 | 把資料整理成檢視、小型轉換、驗證 |

腳本算子必須是純的：拿不到能力物件，只能讀輸入、回傳輸出。副作用只能發生在事件處理器裡，以指令的形式。這讓引擎可以快取、重放、平行化所有導出格。

算子庫就是 ABI。新增一種能力等於新增一組算子；只有需要新的原生算子時才要上架新版 App，其餘全走 OTA。

### 4.3 求值：需求驅動、可取消

- **需求從出口格出發。** 可見的視口、播放中的音訊、進行中的匯出會向上游要值；沒人要的導出格不算。螢幕外的頁面、被摺疊的面板、離開視口的 tile 都不計算。
- **鏡頭與視口是輸入格。** v1 的 tile 串流因此不是特例，而是需求驅動求值的一個實例：Tiled 出口格向上游要 `raster(level, x, y)`，需求集合隨鏡頭改變，離開需求集合的請求自動取消。
- **優先序**按離視口的距離、使用者注意力、輸入延遲敏感度排序。
- **時間是輸入格。** 只有依賴 `t` 的子圖每幀重算。沒有任何出口格依賴 `t`、也沒有髒格時，渲染執行緒睡到下一個事件，0 fps。
- **取消是常態。** 鏡頭移動、輸入連打時，過時的求值直接丟棄，不排隊等它完成。

### 4.4 Commit 與快照

指令（來自使用者、腳本事件處理器、agent）以批次修改輸入格、資源格與圖的結構，然後 commit。每次 commit 產生一個不可變的快照版本：

- 渲染執行緒讀快照畫圖，不阻塞求值。
- 語義埠、命中測試、腳本的同步查詢都從快照回答。
- 求值在工作池上進行，結果寫進下一個版本。

### 4.5 決定性與重放

不決定的東西只能從輸入格進來：時鐘、網路回應、亂數、感測器，進來時值被記錄。於是「指令日誌 + 記錄的輸入」能重建任何時刻的任何狀態。以下全是副產品：

- **undo / redo**：倒退或重做指令。
- **協作**：指令帶 base version，衝突時重放並 rebase。
- **審計**：日誌就是審計紀錄。
- **測試**：重放一段日誌，比對出口格。
- **解釋**：任何導出格都能回答「依賴哪些輸入、經過哪些算子、哪個指令改變了它」。

### 4.6 內化的程度：資源格與原生化

誠實的部分：PDFium、OCCT、IronCalc 這類外部引擎有自己的內部狀態，不可能第一天就全部變成圖。所以分兩層：

- **資源格**：外部引擎持有狀態，圖上只有句柄與版本。變更經指令、提升版本，下游自動失效。上線快，但腳本與 agent 只看得到算子暴露的視圖。
- **原生化**：匯入算子把格式翻成引擎原生的值，例如字形串、路徑、圖片、表格欄位、時間軸、特徵樹。之後的編輯是編輯引擎資料，匯出算子再寫回格式。完全可計算、可查詢、可組合。

路線是每種格式先以資源格上線，再逐步原生化最常用的部分：

| 格式 | 先以資源格上線 | 優先原生化 |
|---|---|---|
| PDF | PDFium 光柵 | 文字層與閱讀順序、批注、表單欄位 |
| 表格 | IronCalc 整份求值 | 公式依賴進入計算圖，每格或每範圍一個導出格 |
| 影片 | 硬體解碼會話 | 時間軸天生是原生資料，合成本來就是 `t` 的函式 |
| CAD | OCCT B-rep | 參數化特徵樹；鑲嵌結果是導出格 |
| 圖片、字型、音訊 | 解碼器 | 解碼後的值本來就是原生緩衝 |

### 4.8 沒有靜態與動態之分

常見的前端框架把元件分成靜態和動態，例如 Flutter 的 StatelessWidget 與 StatefulWidget。AnyPage 不分：**每個面板、每個 tile 來源、每個 GPU 算子的參數，都是輸入格的函數。** 狀態只存在輸入格裡（不變量 4），畫面是函數的值。

- **靜態**是輸入永遠不變的函數：算一次，之後一直用快取。
- **動態**是輸入會變的函數：只在它讀到的東西變了才重算。
- **時間**也只是一個輸入格（時鐘）。讀時鐘的函數就是「依賴時間」的內容，不需要另外標記成動畫元件。

引擎靠四個機制讓「全部都是動態」仍然高效：

| 機制 | 做法 | 量測（spike 1 第二輪，Ryzen 9 5950X） |
|---|---|---|
| 需求驅動 | 只有可見的出口格會被求值；看不見的內容不花任何成本 | 不可見的面板、tile、GPU 算子完全不執行 |
| 提早截斷 | 格只在它讀到的輸入版本變了才重算；算子可宣告值相等，算出相同的值就停止往下傳 | 10 萬個下游格，輸入變了但截斷點的值沒變：只跑 1 次算子，其餘 10 萬格免算即確認 |
| 可預測的時間 | 讀時鐘的函數可宣告「這個值在時間 T 之前有效」；時鐘前進時，用計時器堆積只喚醒到期的格；引擎從可見格中最早的到期時間決定何時醒來 | 1 萬個定時格跑 1 秒：1,429 次算子而非 60 萬次，每次 tick 0.13 ms；頁面上只剩時鐘與游標時，3 秒畫 8 幀，正好等於預測的變化次數，60 fps 迴圈會畫 180 幀 |
| 分幀求值 | 每幀有預算；做不完的求值保留遍歷狀態，下一幀接著做，畫面先顯示舊值 | 10 萬格長鏈全部重算 38 ms，以每幀 4 ms 預算分 9 幀完成，沒有一幀超過 4.6 ms |

三種時間函數由函數本身宣告，引擎不必猜：

| 函數 | 宣告 | 引擎的反應 |
|---|---|---|
| 連續動畫 | 不宣告（讀了時鐘就視為每次 tick 都變） | 可見時每幀求值 |
| 時鐘、閃爍、倒數 | `valid_until(下一次變化的時間)` | 睡到那一刻，只在那一刻醒來 |
| 暫停的動畫、跑完的過場 | `valid_until(∞)` | 變回常數，0 fps |

這也讓「可預測」成為之後的優化入口：純時間函數的下一個值可以提前求值或交給 GPU，因為引擎知道它在什麼時候、依什麼變。

### 4.7 為什麼不是 DOM

| | DOM（瀏覽器） | 計算圖（AnyPage） |
|---|---|---|
| 失效 | 樣式與佈局失效靠推測，腳本讀寫交錯造成佈局抖動 | 依賴顯式，只重算髒子圖 |
| 腳本位置 | 腳本與渲染搶同一條主執行緒 | 腳本只在事件時執行，重活在算子 |
| 重放 | 做不到，狀態散在 DOM、JS 堆、閉包裡 | 指令日誌 + 記錄輸入即可重放 |
| agent 觀測 | 截圖或爬 DOM，看不到資料與邏輯 | 讀值、依賴、來源與型別化動作 |
| 平行 | 主執行緒瓶頸 | 圖的獨立分支自然平行 |
| 組合 | iframe 貼像素，postMessage 傳字串 | 型別化埠，跨頁面連圖 |

---

## 5. 頁面：包、定址、開檔、組合

### 5.1 頁面包

| 內容 | 語言 | 為什麼 |
|---|---|---|
| manifest、權限、圖範本、靜態 UI | JSON（帶 schema） | 可驗證、可 diff，AI 不會寫錯語法；靜態部分的動作空間發佈時就編好 |
| 邏輯 | Luau 原始碼（或 TypeScript 編譯而來） | 現階段裝置收原始碼、在 App 內編譯，拒收位元碼；日後改收位元碼必須先有簽章，見第 11、13 節 |
| GPU | WGSL | 經 naga 轉 MSL / SPIR-V / HLSL，各平台原生速度 |
| 資產 | 圖片、字型、資料 | 內容定址，可快取 |
| 簽章 | 發佈者簽章 | 驗證頁面來源；若日後跨信任邊界傳位元碼，Luau 沙盒規範要求簽章 |

現況（2026-10-24）：`pages/demo` 就是一個頁面包：`page.json`（身分、頁面邊界、能力宣告、配額）、`main.luau`、`plasma.wgsl`。只收文字檔，檔名不得含路徑，單檔 2 MB、全包 16 MB。能力宣告列出頁面要用的原生算子（串流、工作、tile 來源）與能否帶 WGSL，沒宣告的在載入時就被拒。圖範本還沒有獨立成 JSON，目前由入口腳本在載入時建圖。實作在 `crates/anypage-runtime`，量測在 docs/spikes/spike2-script-pages.md。

內建頁面（設定、檔案總管、各格式的預設處理器）用同一格式隨 App 出廠，**沒有特權**。核心因此沒有任何「只有內建功能才能做」的後門。

### 5.2 定址與導航

開一個頁面的流程：位址 → 下載或從快取取包 → 驗簽章 → 檢查算子 ABI 相容範圍（`require("pdf@2")` 式）→ 依 manifest 請求能力、使用者同意 → 建立 realm → 實例化圖範本 → 第一次 commit → 第一幀。

頁面包離線可用、有版本、可回滾。

### 5.3 開檔等於選處理器

和瀏覽器依 MIME 選檢視器一樣：檔案變成一個輸入格（位元組或檔案句柄），綁到一個宣告接受該型別的處理器頁面，處理器頁面用匯入算子把它變成資源格或原生值。「以其他頁面開啟」讓任何宣告接受該型別的頁面都能處理它。

### 5.4 組合：頁面嵌頁面

頁面可以宣告型別化的**輸出埠**（一個圖表、一個範圍、一段時間軸）與**輸入埠**。嵌入另一個頁面，等於跨 realm 把埠連起來，需要明確授權。

組合因此發生在計算裡，而不是貼像素：簡報裡的圖表直接依賴表格頁面的範圍，表格改了圖表自動重算；CAD 視圖放進文件時帶著自己的鏡頭；把 PDF 某頁當影片疊層，是影片頁面的合成算子依賴 PDF 頁面的 tile 輸出。

### 5.5 可塑性

任何頁面的圖與程式碼都可檢視、可 fork、可修補，修補本身也是頁面包。這是可塑軟體（Ink & Switch, 2025）的執行時：人或 AI 拿別人的頁面改成自己要的樣子，而且改的是計算，不是像素。

---

## 6. 層級

```
L4  頁面      manifest + 圖範本 + Luau（現為原始碼）+ WGSL + 資產 + 簽章（含內建處理器頁面）  可下載、可熱換
L3  腳本 VM   Luau，每頁一個 realm：建圖、處理事件、發指令；只拿到能力物件            編進 App
L2  算子庫    佈局、排版、光柵、解碼、格式匯入匯出、公式、幾何；宣告簽章與執行緒契約    編進 App，C ABI
L1  執行時    計算圖求值器、排程、commit 與快照、指令日誌、渲染器、能力代理、語義埠、頁面載入器
L0  平台殼    surface、輸入、IME、無障礙、生命週期、檔案、網路                          每平台一份，極薄
```

和 v1 的差別：

- **L1 擁有模型。** v1 的 L1 只懂「場景圖 + 鏡頭 + 指令 + 能力」，模型住在 L2。v2 的 L1 擁有計算圖，場景圖是圖的一個導出值。
- **L2 是函式，不是模型的主人。** PDFium、IronCalc、OCCT 從「持有文件的引擎」降為算子；仍持有狀態的部分以資源格表示。
- **L1 仍然不認識任何格式**，只認識格、算子簽章、指令與能力。

Emacs（C + Elisp）、Blender（C + Python + 幾何節點）、Roblox（C++ + Luau）、JAX（Python 建圖 + XLA 算子）都是「小核心 + 可下載的組裝層 + 編譯好的算子」。

---

## 7. 八條不變量

1. **每幀的計算只發生在原生算子與 GPU 算子。** 腳本算子只在事件時或有預算的低頻求值中執行；穩態時腳本每幀耗時為零。
2. **跨層只傳句柄或零拷貝緩衝。** 像素是 texture 句柄，表格是共享記憶體視圖，影片幀盡可能從硬體解碼器直接成為 texture。舊版 React Native bridge 的失敗就是違反這條。
3. **零環境權限，能力即物件。** 腳本沒有 `fs`、`fetch` 這類全域；manifest 宣告需求，使用者同意後核心把能力物件交進 realm。這也是 App Review 4.7.2 的要求：下載的程式碼不得在沒有使用者許可下取得原生 API。
4. **所有可觀測的狀態都在圖上。** 腳本 realm 裡可以有暫存變數，但需要持久、需要被畫出來、需要被 agent 看見的狀態，一定是輸入格或導出格。沒有藏在閉包裡的真相。
5. **需求驅動，無需求就不計算。** 沒有髒格、沒有依賴時間的出口格，就是 0 fps。
6. **能下載的只有資料與位元碼，能編譯的只有算子，中間是帶版本的穩定 ABI。** 算子簽章就是 ABI；執行時同時提供多個版本，頁面宣告相容範圍。
7. **一份核心，薄外殼。** 殼只做 OS 堅持要原生做的事。文字輸入用原生輸入框疊在引擎畫面上，引擎只負責顯示。
8. **隔離、配額，所有變更是帶主體與版本的指令。** 每頁一個 VM，有記憶體上限、時間片和看門狗；腳本崩潰不影響引擎。錄製、重播、undo、協作、審計都是副產品。

---

## 8. 執行時：執行緒與管線

| 執行緒 | 負責 | 為什麼獨立 |
|---|---|---|
| 主執行緒（殼） | 視窗、輸入、IME、生命週期、無障礙 | OS 強制 |
| 腳本執行器（N 條） | 各頁面 VM 的事件處理與腳本算子，一個 VM 同時只在一條執行緒上 | 腳本卡住不能卡畫面 |
| 求值工作池 | 原生算子：光柵、解碼、排版、公式、鑲嵌 | 可平行、可丟棄 |
| 專屬佇列 | 不是執行緒安全的外部引擎，例如 PDFium | 執行緒契約要求序列化 |
| 渲染執行緒 | 讀快照、遍歷場景、裁切、編碼 GPU 指令、present | 穩定幀時間 |
| 音訊執行緒 | 混音；有音訊播放時兼任主時鐘 | 即時性要求最高，不能被任何工作擋住 |
| IO | 檔案、網路 | 會阻塞 |

```
輸入(主) → 指令批次 → commit → 失效傳播(標髒) → 收集需求(可見出口格 + 預取)
        → 排程求值(工作池 / 專屬佇列 / GPU) → 場景快照 → 渲染 → present
```

兩個短路：

- commit 後沒有髒格、也沒有依賴 `t` 的出口格，渲染執行緒直接睡到下一個事件。
- **鏡頭移動不經腳本。** 拖曳、縮放是核心手勢，直接改鏡頭輸入格；需求集合隨之改變，腳本只收 `camera.changed`。瀏覽的 60fps 完全不依賴腳本，和瀏覽器把捲動放在合成執行緒同理。

規則：**管線的階段是任務，不是執行緒。** Web 單執行緒時同一張任務圖序列執行即可。只有主執行緒是特殊的。

排程器按第 3.2 節的優先序填滿每一幀的預算：操作、可見串流、剛完成的非同步結果、缺漏的靜態內容、預取。

### 算子的執行緒契約

算子必須宣告執行緒契約，排程器照契約辦事：

- **PDFium 完全不是執行緒安全的**：全行程只能有一條 PDFium 佇列，不是每份文件一條（pdfium-render 以全域互斥鎖序列化所有呼叫）。互動性靠需求驅動的優先序與取消，而不是靠平行。
- **IronCalc** 0.9 的 `evaluate()` 是整份重算，沒有公開的髒格依賴圖，20 萬格每次編輯約 130 ms。它先當一個資源格算子；公式依賴進入計算圖是原生化的第一步（第 4.6 節）。
- **解碼器**是會話式的，一個會話是一個資源格。

---

## 9. 渲染：場景是求值結果

場景圖是計算圖的一個導出值，渲染器只讀快照。v1 的八種節點契約保留，但意義變了：它們是**出口格的輸出種類**，按渲染契約分類，不按文件領域分類。

| 輸出種類 | 契約 | 上游通常是 | 渲染器負責 |
|---|---|---|---|
| Tiled | `raster(level,x,y) → 圖` | PDF 頁、CAD 2D 圖紙、巨圖、任何能按區域光柵的導出值 | LOD 選擇、快取、預取、低解析度補位 |
| Streamed | `frame(t) → texture` | 解碼會話、時間軸合成 | 以播放頭為中心預求值、seek 對齊關鍵幀 |
| VirtualGrid | `cell(r,c) → 內容` + 行列尺寸 | 表格、資料集、清單 | 可視範圍裁切、凍結列，只向上游要看得見的格 |
| Vector | 路徑、填色、描邊 | 圖形、圖表、批注 | 曲線光柵化 |
| Text | 段落 + 樣式 | 排版算子（parley） | 字形圖集 |
| Mesh | 三角網格 + 材質 | 鑲嵌算子（OCCT 等） | 子視口與獨立鏡頭、簡化 PBR |
| Effect | WGSL + 輸入 texture | GPU 算子 | 編譯、快取 pipeline、離屏合成 |
| NativeOverlay | 一個矩形 | 殼 | 把原生視圖對齊到該矩形：輸入框、相機、地圖，必要時是 WebView |

鏡頭掛在視口上，是輸入格；Mesh 可帶嵌套鏡頭，所以 3D 視圖能放進 2D 頁面。

### Tile 系統
- 格 256 或 512 裝置像素，層級 = ⌊log₂(縮放)⌉，快取鍵 `(來源格, 版本, 層, x, y)`。來源格的版本變了，對應 tile 自動失效。
- GPU 記憶體預算（手機約 256 MB），LRU 淘汰；沿速度方向在視口外預取一圈。
- 尚未求值的格先拿低一層放大補位，縮放永不空白。
- **第 1 階段基線是 CPU 光柵、GPU 合成。** Tiled、Vector、Text 全在工作池用 vello_cpu 光柵成 tile 再上傳，GPU 只做合成與 Effect。這是瀏覽器多年的做法，繞開手機 GPU 驅動的風險；GPU 向量路徑等 vello_gpu 穩定再上。

### 影片與零拷貝

時間軸每幀被求值成一張合成圖：`t` 時刻哪些片段可見、疊哪些 Effect。Effect 跑 WGSL，AI 寫的轉場和濾鏡直接進 GPU 路。音訊不走 GPU，在獨立音訊執行緒混音，和影片只靠時鐘對齊。導出等於同一張合成圖離屏求值到目標解析度，餵硬體編碼器。

| 平台 | 解碼輸出 → GPU texture |
|---|---|
| iOS / macOS | `CVPixelBuffer` → `CVMetalTextureCache` → `MTLTexture`，wgpu 從 hal 匯入 |
| Android | `MediaCodec` → `AHardwareBuffer` → Vulkan 外部記憶體 |
| Web | WebCodecs `VideoFrame` → `importExternalTexture` |

現況（2026-10）：Rust 生態沒有任何專案在手機上出貨過零拷貝解碼到 wgpu；wgpu 的 `ExternalTexture` 仍在進行中，只有 DX12 與 Metal；`CVPixelBuffer` 與 `AHardwareBuffer` 匯入要走 `create_texture_from_hal`。參考碼是 `wgpu-external-frame`（2026-10，未經實戰）。**退路是每幀一次 NV12 上傳**，1080p 約 3 MB/幀，現代手機 30 fps 可承受。零拷貝是效能目標，不是出貨前提。

---

## 10. 腳本與核心的接口

- **句柄**是 `(index, generation)` 的 slotmap 鍵，過期即失效。格、算子實例、埠都用句柄。
- **指令批次**：腳本在一次事件處理裡把所有操作追加到指令緩衝（typed array），結束時一次交給核心。邊界穿越次數等於事件批次數。指令分三類：
  - 改值：`set(cell, value)`、`patch(cell, delta)`
  - 改圖：`derive(op, inputs) → cell`、`remove(cell)`、`connect(port, port)`
  - 呼叫資源：`resource.call(cell, method, args)`，例如對 PDFium 文件加一個批注，回傳新版本
- **同步查詢只允許讀**：命中測試、量測、格的值與依賴，從上一次 commit 的快照回答，不阻塞渲染。
- **大資料不進指令**：共享緩衝加顯式「轉移所有權」。
- **驗證**：每條指令過 schema，非法指令回錯誤事件，核心絕不 panic。
- **宣告式 UI 是使用者空間的程式庫，不是核心功能。** 工具列、面板這種小樹用腳本層的 diff 程式庫轉成指令；大量資料永遠走導出格，不走 diff。

一次剪輯操作：點擊 → 命中測試（快照）→ 事件處理器把 `timeline.split(clip, t)` 寫進緩衝 → commit → 時間軸輸入格更新，兩個片段的導出格變髒 → 下一幀只重算受影響的合成 → GPU。腳本耗時微秒級。

**現況（2026-10-24，spike 2）。** 已實作的是這個接口的第一版，全部在全域 `page` 上：

| 時機 | 呼叫 |
|---|---|
| 只在載入時（之後圖固定） | `page.clock()`、`page.input(name, v)`、`page.derive(name, {輸入}, fn)`、`page.stream(name, 來源, opts)`、`page.job(name, 來源, {輸入})`、`page.panel{rect, class, inputs, draw}`、`page.tiled{rect, source, params}`、`page.effect{rect, shader, params}` |
| 事件處理器 | `page.on(事件, fn)`；`page.set`、`page.cancel`、`page.request` 先記下，處理器結束後一次套用；`page.get` 讀輸入格的最新值 |

- 句柄是 userdata 包著格的 id；值以一個可比較的資料型別在腳本與核心之間轉換，所以腳本算子的結果能做提早截斷。
- 腳本算子回傳第二個值宣告「有效到」（第 4.8 節）；面板的 `draw` 回傳顯示清單而不是像素，光柵化在原生程式裡做，換縮放級距不重跑腳本。
- 尚未就緒的輸入以 `{ state = "pending", progress }`、`{ state = "error", message }` 交給腳本。
- 主機函式的錯誤帶呼叫它的腳本行號；出錯的面板畫錯誤訊息，頁面其餘部分照常。
- 還沒做：`remove` 與 `connect`、資源呼叫、同步查詢只有 `page.measure` 與輸入格的 `page.get`、共享緩衝。

---

## 11. VM 的決定：先定 VM，表面語言可以晚定

邊界在位元碼與指令協定，所以**現在必須定的是 VM**，表面語言之後靠編譯器加上去，ABI 不變。

VM 選 **Luau**：

- 「每頁一個 VM」必須便宜。Luau 一個 VM 幾十 KB、啟動微秒級；QuickJS 數百 KB；Hermes 以 MB 計。
- 配額內建：記憶體上限走自訂配置器，時間片走 `interrupt` 回呼，`luaL_sandbox` 把全域凍成唯讀。
- 無 JIT 平台的純運算直譯速度屬第一梯隊。2026-09 一份 x64 基準測試中，Luau 比 QuickJS-ng 快 2～8 倍。
- C++，隨 Rust 核心一起編譯（`mlua` 的 `luau` 特性），經 Emscripten 連 Web 也能編。
- 漸進型別讓發佈前的靜態檢查成立。

現況（2026-10）：mlua 0.12 的 `luau` 特性直接提供 `sandbox()`、`set_interrupt()`、`set_memory_limit()`、`heap_dump()`。兩個坑：`interrupt` 在單一大型內建函式內不會觸發（例如 `string.rep`），要另外限制字串長度或移除這類內建；位元碼跨信任邊界必須簽章。Rive 於 2026-01 把 Luau 當 .riv 檔內的腳本語言正式上線，證明這條路在 iOS / Android / Web 上可出貨。

實測（2026-10-24，spike 2）：mlua 0.12.2 的 `luau` + `send` 特性在 Windows 與 Android（arm64、x86_64）都能編譯與執行。每頁一個 realm：預設 64 MB 記憶體上限；每次腳本函數呼叫 50 ms 期限，`interrupt` 每 128 次檢查一次時鐘，無窮迴圈 20 ms 內停下且 VM 之後仍可用；`sandbox(true)` 之後全域與標準庫唯讀；`loadstring`、`load`、`require`、`getfenv`、`setfenv` 移除；只收原始碼（`ChunkMode::Text`）。`string.rep` 這類不觸發 `interrupt` 的大型內建函式，靠記憶體上限擋住。示範頁面建 realm 加執行入口腳本：Windows 3.0 ms，Android 模擬器冷啟動 85.9 ms。Android 上 Luau 的 C++ 執行時改為靜態連結，App 仍只有一個原生函式庫。

代價：Web 上 Luau 跑在 WASM 裡再直譯，比直接跑瀏覽器 JS 慢；AI 寫 Lua 的熟練度低於 TS，但 API 面小、型別標注齊全時差距不大。之後要 TS 表面，roblox-ts 已證明 TS→Luau 能在生產規模運作。

---

## 12. 語義埠：agent 讀的是計算

Computer use 要看截圖、算座標，是因為它面對別人的 App。我們擁有執行時，所以 agent 不看像素，而是拿到腳本拿到的同一份東西，外加計算圖本身。**Agent 是 L3 的另一個控制器**，走同一條指令協定、同一套能力模型。

### 一個介面，多種消費者

核心裡的特權服務，從上次 commit 的快照提供觀測、接受指令，同時餵 LLM agent、純分類器、螢幕閱讀器、語音控制、UI 測試、巨集錄製。無障礙樹就是 agent 的觀測，無障礙動作就是 agent 的動作。

| 通道 | 內容 | 成本 | 用途 |
|---|---|---|---|
| 語義樹 | 節點 id、角色、標籤、狀態、可用動作、座標、z 序、是否被遮擋、是否在視口外 | 低 | 預設 |
| 圖視圖 | 格的值、型別、依賴、來源，以及哪些輸入影響某個出口格 | 低～中 | 規劃與解釋（v2 新增） |
| 領域視圖 | 處理器頁面或算子暴露的結構：PDF 文字層、表格範圍與公式、時間軸片段、CAD 特徵樹 | 低～中 | 精確操作 |
| 增量 | 自上次觀測以來的變化 | 極低 | 驗證結果、長任務 |
| 像素 | 任一視口或節點的渲染，可指定解析度、可疊標記 | 高 | 視覺確認、圖片內容、3D |

**圖視圖是可計算執行時獨有的。** Agent 想讓某個出口格變成某個樣子時，可以先查「它依賴哪些輸入格」，再只對那些輸入下指令；也可以問「這個值為什麼是 42」，得到一條依賴鏈，而不是一張截圖。

### 動作空間：有限、有型別、有標籤

每個可互動節點宣告 affordance：`名稱、參數 schema、自然語言描述、前置條件、效果類別`。Agent 用 `(nodeId, action, args)` 行動，不用座標。動作最終都是對輸入格或資源格的指令。這份宣告同時是 UI 按鈕、無障礙動作選單、agent 的 tool-use schema。頁面的 manifest 就是 agent 的工具清單。

- **LLM**：動作空間轉成文法，受限解碼只產生合法動作。
- **純分類器**：空間有限且被枚舉，一個只會輸出類別的模型就能操控。輸入是語義樹特徵加使用者意圖，輸出一個 actionId；連續參數靠量化槽位。訓練資料是指令日誌裡的 `(觀測, 動作)` 對，可在裝置上訓練。搞不定時升級到 LLM，形成成本階梯。

### 靜態與動態 UI
- **靜態 UI**（manifest 宣告）：動作空間發佈時編譯好，id 固定，agent 一開始拿到整張地圖。
- **動態 UI**（腳本生成、資料驅動、agent 生成）：引擎在 commit 時**強制可描述性**，沒有角色與標籤的互動節點直接被拒；id 帶 key 跨幀穩定；增量通道告知出現與消失。

### 2D 與 3D
- **2D**：領域視圖加空間查詢 `hitTest`、`nodesIn`、`find`、`nearest`。「把第三段第二句畫底線」不需要看圖。
- **3D**：特徵樹與物件表；ID 緩衝與拾取（`pick(x,y)` 回物件、面、3D 點、法向）；可請求的視角 `render(camera, highlight, marks)`，Set-of-Mark 標記由引擎原生疊上，編號與節點 id 嚴格對應；量測是查詢不是目測：`distance`、`volume`、`interference`。

### 效率

| 機制 | 效果 |
|---|---|
| 增量觀測 | 上下文不隨時間膨脹 |
| 預算觀測 | agent 給 token 預算，引擎回最重要的子集，長列表折疊 |
| 動作批次 | 一回合多動作一次提交 |
| 以增量驗證 | 省掉截圖回合 |
| 前置條件與依賴查詢 | 不執行就能規劃，引擎能解釋「為何不可用」 |
| 視口外操作 | 不必捲動找元素 |

### 非同步任務與外部 agent
- **Job 是一等公民**：`{id, kind, principal, status, progress, budget, result, cancel}`。影片導出、雲端重建、派子 agent，全是 Job；Job 的進度本身是輸入格，取消是動作。
- **派外部 agent 是能力**：`agent.dispatch(goal, scope, budget) → Job`。scope 是範圍化能力票：只能觀測與操作指定子圖、指定動作類別、有預算。外部 agent 從不直接碰引擎，一律經能力代理；結果以資料或事件回到圖上，不會是程式碼。
- **樂觀並發**：每條指令帶 base version；衝突的子集被拒絕並回報，可重放與 rebase。

### 對核心的回頭要求

語義埠要求核心從第一天就滿足三件事：所有互動節點有角色與標籤（commit 時強制）、所有變更是帶主體與版本的指令、有一塊腳本畫不進去的受信任區域。一開始沒做，之後補不回來。

---

## 13. 安全：取代同源政策

瀏覽器的安全模型是「同源政策 + 權限提示 + 網址列」。AnyPage 的對應是能力模型，粒度更細：

- **主體與授權**：使用者、每個頁面、每個 agent 都是 principal。grant 定義哪些格與埠、哪些動作類別、多少預算、能否用像素通道。跨頁面連埠也是一個 grant。
- **能力即物件**：頁面拿不到任何未被授予的能力。
- **動作類別決定確認政策**：可逆的本地編輯自動放行；不可逆或對外的動作（傳送、發佈、付款、刪檔、連新主機）必須經使用者確認。確認介面由**核心擁有的受信任區域**繪製，頁面與 agent 畫不進去，和瀏覽器網址列同理。
- **來源標記對抗提示注入**：每段文字帶 `origin: user | page | document | agent`，在圖上隨導出傳播。角色與動作標籤只能來自 manifest；腳本讀文件後生成的 UI 標籤標為 tainted。文件內容永遠是資料通道。
- **分類器的可達集合**：只能觸發可逆動作，不可逆動作不在其輸出空間。
- **回合即交易**：一個 agent 回合的所有指令是一組，可一鍵撤銷。
- **審計就是指令日誌。**
- **像素通道遮蔽**：`NativeOverlay`（密碼框）與敏感節點在給 agent 的圖裡遮掉。
- **隔離邊界**：下載的程式碼永遠是被直譯的腳本或 GPU shader，從不是原生碼。原生算子全部隨 App 出廠、經過審查。這是單一行程裡能成立的邊界。
- **頁面的著色器不能拖垮引擎**：載入時先用 naga 解析與驗證，並檢查入口點；建管線時再包一層 wgpu 錯誤範圍，失敗只停用那個效果。實測過：沒有這兩層時，一個語法錯誤的 WGSL 會讓整個 App panic（2026-10-24）。

---

## 14. 跨平台：一份核心，任何平台

目標：iOS、macOS、Windows、Linux、Android、Web 第一天就是一等公民。之後的平台，例如鴻蒙、visionOS、電視、車機、嵌入式、遊戲主機，只要實作一層薄殼就能接上。以下現況為 2026-10 查證。

### 14.1 為什麼做得到

- **核心不碰平台 API。** 計算圖、排程、算子、VM、語義埠都是純 Rust，所有平台同一份。
- **GPU 只經過 wgpu。** 一份 WGSL 經 naga 轉成 Metal、Vulkan、DX12、GL ES 或 WebGPU。
- **頁面是直譯的位元碼。** 同一份頁面包在所有平台執行結果一致，不必為每個平台重新編譯頁面。因為從不 JIT，所有平台都適用同一套規則。
- **平台差異全部關在 L0 殼後面。** 殼有明確的移植契約（第 14.3 節）。

### 14.2 平台矩陣

| 平台 | GPU（wgpu 後端） | 殼的做法 | 現況 |
|---|---|---|---|
| Windows x64 / ARM64 | DX12 為主，Vulkan | winit 起步；文字輸入自建 TSF 橋接 | 可用。ARM64 要隨附 dxcompiler.dll；Snapdragon X 有驅動問題回報 |
| macOS | Metal | winit；文字輸入走 NSTextInputClient | 可用 |
| Linux（X11、Wayland） | Vulkan 為主，GL ES | winit；文字輸入走 Wayland text-input-v3 或 X11 XIM | 可用。X11 的 IME 只能給游標位置 |
| iOS / iPadOS | Metal | **原生宿主**：Swift UIView 承載引擎畫面，文字輸入用原生 UITextInput 疊層 | 要殼的工作。winit 的 iOS 後端沒有 UITextInput，中文組字做不到 |
| Android | Vulkan 為主，GL ES 退路 | **原生宿主**：Kotlin View 或 GameActivity，文字輸入用原生 InputConnection 疊層 | 要殼的工作。wgpu 沒有裝置黑名單，引擎自己維護 |
| Web | WebGPU 為主，WebGL2 退路 | 瀏覽器 canvas；文字輸入用隱藏的 DOM 輸入框疊層 | 可用。winit 在 Web 不支援 IME |
| 鴻蒙 NEXT | GL ES 現在可用；Vulkan 預計在 wgpu v31 | **原生宿主**：ArkTS XComponent + napi-ohos | 早期。Rust 目標已是 Tier 2；winit 只有樹外移植；字型要引擎自己載入；無障礙轉接器只有草稿 |
| visionOS / tvOS | Metal，best effort | 視窗模式走 UIKit；沉浸模式要 Swift 的 CompositorServices 墊片 | 要殼的工作。Rust 目標已是 Tier 2 |
| 嵌入式 Linux | Vulkan 或 GL ES，可經 KMS/DRM 直接上屏 | 無合成器的直繪殼 | 視驅動而定 |
| Android TV、車機、ChromeOS | 同 Android 或 Linux | 同 Android 或 Linux，加方向鍵焦點導覽 | 沿用既有路徑 |
| 遊戲主機（Switch 2、PS5、Xbox） | 私有圖形 API，經 wgpu 的自訂後端介面接上 | 廠商 SDK | 需要 NDA 與開發機；目前沒有 Rust + wgpu 的公開先例 |

| 子系統 | Windows | macOS / iOS | Linux | Android | Web | 鴻蒙 |
|---|---|---|---|---|---|---|
| 文字輸入 | 自建 TSF（winit 只有 IMM32） | NSTextInputClient；iOS 用原生 UITextInput 疊層 | text-input-v3 或 XIM | 原生 InputConnection 疊層 | 隱藏 DOM 輸入框 | ArkTS 原生輸入框疊層 |
| 無障礙 | AccessKit UIA | AccessKit macOS；iOS 只有基本版 | AccessKit AT-SPI | AccessKit Android | 語義樹鏡像成隱藏 DOM | 只有草稿，需自建 |
| 硬體解碼 | Media Foundation（D3D11 DXVA）→ 共享句柄 → wgpu | VideoToolbox | VA-API（cros-codecs）或 Vulkan Video（gpu-video，H.264） | MediaCodec（ndk crate） | WebCodecs → `copy_external_image_to_texture` | 系統解碼器，需自建綁定 |
| 音訊 | cpal（WASAPI） | cpal（CoreAudio） | cpal（PipeWire、PulseAudio、ALSA） | cpal（AAudio） | cpal（Web Audio） | ohaudio 綁定，需自建 |
| 下載直譯位元碼 | Microsoft Store：限於 App 描述的功能範圍 | App Store：內建沒問題，下載屬灰色地帶，依據 4.7 小程式條款 | 無商店限制 | Google Play：直譯器例外有條件允許（只能間接存取 Android API、不得下載原生碼或 dex、開發者為內容負責） | 無限制 | AppGallery 3.9：下載改變功能的程式碼有風險 |

### 14.3 移植契約：新平台要提供什麼

接上一個新平台，就是實作 L0 的這組契約。缺的部分由能力檔案公佈，頁面降級而不是崩潰。

| 契約 | 內容 | 沒有時的降級 |
|---|---|---|
| 表面 | 一個 wgpu 能用的表面（raw-window-handle） | 必備 |
| GPU | Vulkan、Metal、DX12、GL ES、WebGPU 其中之一，或經 wgpu 自訂後端接上的私有 API | 必備；GL ES 3.0 是最低線 |
| 事件 | 指標、觸控、鍵盤、滾輪、生命週期 | 必備 |
| 文字輸入 | 原生輸入框疊層，或 IME 橋接 | 只有引擎內的簡易輸入，不支援組字 |
| 無障礙 | AccessKit 轉接器，或對平台 API 的橋接 | 語義埠仍在，但螢幕閱讀器看不到 |
| 硬體解碼 | 解碼到 GPU texture 或 CPU 緩衝 | 軟體解碼，或每幀上傳 |
| 音訊 | 低延遲輸出與時鐘 | 靜音，影片改用系統時鐘 |
| 執行緒 | 多執行緒 | 單執行緒：任務圖序列執行（第 8 節） |
| 儲存與網路 | 檔案、網路 | 能力檔案宣告缺少，頁面降級 |
| 程式碼規則 | 是否允許下載直譯位元碼 | 只跑隨 App 出廠的內建頁面，下載的只有資料 |

執行時向頁面公佈一份**能力檔案**（`jit:false, wasm:"interp", gpu:"metal", threads:6, ops:["pdf@2","calc@1"]`），頁面宣告最低需求。

### 14.4 殼的兩種形態

- **引擎當 App**：桌面與 Web。winit 或瀏覽器提供視窗與事件，殼和核心在同一個 Rust 執行檔裡。
- **引擎當嵌入式庫**：iOS、Android、鴻蒙、visionOS。原生 App（Swift、Kotlin、ArkTS）擁有視窗、生命週期、文字輸入與無障礙，引擎以 C ABI 庫的形式畫在它給的表面上。

行動平台選嵌入式庫，是因為這些平台的文字輸入、無障礙與生命週期都必須由原生視圖負責，而 winit 在行動平台的文字輸入有缺口。附帶的好處是 AnyPage 能嵌進別人的 App。

### 14.5 一致性

- 同一份頁面包、WGSL、指令協定在所有平台執行。
- 引擎自己排版文字（parley、swash），系統只提供字型檔，所以各平台的版面一致。沒有系統字型介面的平台（鴻蒙）由引擎自己載入字型。
- 每個平台跑同一套參考畫面測試，按 GPU API 族設像素容差。
- 文字輸入、無障礙、檔案選擇、通知等原生功能，行為跟隨平台慣例。

### 14.6 GPU 退路

- 啟動時探測：建 adapter、建一條測試 pipeline、渲染一幀並回讀檢查。任何一步失敗就換下一個後端：Vulkan 換 GL ES；Web 上 WebGPU 換 WebGL2。
- wgpu 不提供裝置黑名單，引擎自己維護驅動版本與型號的黑名單。黑名單是可 OTA 更新的資料。
- 探測結果寫進能力檔案，頁面據此降級。

---

## 15. 如何實現

### 不是新語言，是執行時 + 協定 + 工具鏈

| Web | AnyPage | 性質 |
|---|---|---|
| 瀏覽器引擎 | Rust 執行時 + 各平台殼 | 執行時 |
| DOM + 事件 | 計算圖 + 指令 / 事件 / 觀測協定 | **協定，真正的「語言」，但是資料格式** |
| HTML 元素 | 格的型別、算子簽章、八種輸出種類 | schema |
| manifest / CSP | 頁面包 manifest + 能力授權 | schema |
| JavaScript | Luau（或 TS 編譯而來） | 借 |
| WebGL shader | WGSL | 借 |

交付物：**一份規格 + 一個參考實作 + 一套 SDK**。

為什麼不造語言：借語言的引擎贏了（Unity/C#、Roblox/Lua、Unreal/C++），自造的付了代價（GDScript 後來加 C#、Dart 是 Flutter 的阻力）；使用者一半是 AI，AI 寫得最好的是訓練資料最多的語言，新語言在模型眼裡是零資料。真正要設計的是**協定與算子 API**。

### 唯一真相來源：Rust 型別，生成一切
```
Rust 型別（格、算子簽章、指令、事件、affordance、manifest）
   ├→ 二進位指令緩衝的編碼/解碼（FlatBuffers 式零拷貝）
   ├→ Luau / TS 型別定義與綁定
   ├→ JSON Schema（manifest 驗證、發佈時檢查）
   ├→ agent 的 tool-use schema（語義埠直接輸出）
   └→ 文件
```
affordance 的前置條件與效果用 CEL 或枚舉的效果類別，不自造規劃語言。

### 買與造

| 造（執行時本身，全 Rust） | 買或借 |
|---|---|
| 計算圖求值器：失效、需求、取消、快照、排程 | 渲染後端 wgpu |
| 指令協定、驗證、指令日誌 | 2D 光柵 vello_cpu（第 1 階段）→ vello_gpu（穩定後） |
| 頁面載入器：包格式、簽章、ABI 檢查 | 排版 parley + swash + rustybuzz |
| 能力代理與受信任區域 | 佈局 taffy |
| 語義埠 | 無障礙 AccessKit |
| VM 宿主（隔離、配額） | VM 本體 Luau（經 mlua） |
| 算子 ABI 與排程契約 | 算子實作：PDFium（pdfium-render）、IronCalc、OpenCascade、平台編解碼器 |
| 各平台薄殼 | |

求值器必須自己造。需求驅動、可取消、快照、跨執行緒排程、資源格這幾個要求合在一起，沒有現成的增量計算庫同時滿足。設計借 salsa（rust-analyzer 的增量查詢）、Typst 的 comemo、Jane Street 的 Incremental、Adapton 的需求驅動模型。

### 底座與借設計

選 **Linebender 棧的零件**當底：vello_cpu / vello_gpu、parley、AccessKit、kurbo、peniko。從 Bevy 借任務池與資產串流的設計。

**借設計不借碼**，不 fork 任何現有專案：

- Blink RenderingNG：管線分階段與每階段的失效。
- salsa、comemo、Incremental：增量求值。
- JAX、TensorFlow：「宿主語言建圖、編譯好的算子執行」的分工。
- Observable notebook：反應式格的使用者模型。
- Univer：Scene / Layer / Viewport 與元件骨架的分層。
- Zed embedded_gpui：宿主與客端的邊界。客端只產保留式結構，宿主重播，宿主在幀路徑上絕不呼叫客端。
- MCP Apps：一個 Tool 結構同時帶 inputSchema 與 UI 中繼資料。
- Apple App Intents：entity schema 的形狀。

### 建材現況（2026-10-24 調查）

| 建材 | 現況 | 對設計的影響 |
|---|---|---|
| Vello | compute 版 `vello` 已被官方降為 research / experimental；Android 上 Adreno 7xx 有 8/23 條 pipeline 編譯失敗，Mali 冷啟動 shader 編譯 0.7～1.2 秒。`vello_gpu`（前身 hybrid，CPU 前處理 + GPU 光柵、不用 compute shader）是官方預定的主力，仍是 beta、無 API 穩定承諾。`vello_cpu` 0.3 最成熟 | 第 1 階段 CPU 光柵 tile + GPU 合成；GPU 向量路徑等 vello_gpu |
| Parley 0.12 | 有 `PlainEditor`（游標、選取、剪貼簿、IME 預編輯）；每版都有破壞性變更；AccessKit 整合已從 crate 移出 | 可用，預期跟版成本 |
| AccessKit | iOS adapter 2026-05 才有基本版（0.2.x），Android 0.9，均無富文字 | 語義埠與無障礙樹共用一份來源，但 iOS 深度有限 |
| Masonry / Xilem | 0.4 alpha（2025-10），無 iOS，Android 不可用 | 只借設計 |
| Blitz | HTML 盒模型引擎，非通用場景圖，無 iOS | 不當底座；可能成為 HTML 處理器頁面背後的算子（第 18 節） |
| PDFium / pdfium-render | PDFium 不是執行緒安全；pdfium-render 0.9 以全域互斥鎖序列化 | 每行程一條 PDFium 執行緒，見第 8 節 |
| hayro | 純 Rust PDF，成功率與 PDFium 持平，效能未優化，Typst 0.14 已用於嵌入 PDF | 實驗線，不押 |
| IronCalc 0.9.1 | 整份重算、無公開髒格 DAG，1.0 未到 | 先當資源格算子，公式依賴再進計算圖（第 4.6 節） |
| CAD | Fornjot 2026-06 封存；Truck 活著但小、無圓角；opencascade-rs 標示 WIP；Zoo 的幾何引擎在雲端 | OCCT 走自己的 C FFI，維持原案 |
| 零拷貝影片 | 無人在手機上出貨；wgpu `ExternalTexture` 進行中、僅 DX12 / Metal | 見第 9 節，備妥每幀一次上傳的退路 |
| Luau / mlua 0.12 | sandbox、interrupt、memory limit 齊全；interrupt 不進內建函式；位元碼需簽章 | 見第 11 節 |
| Rust UI 上手機 | egui 0.36 才修好 iOS / Android IME；Makepad 到 2026-10 仍有手機選取問題；Slint 1.17 起有 viewer 上架 | 不變量 7 正確：文字輸入交回原生 |
| iOS 規則 | 2.5.2 無 JIT 未變；行程內 JavaScriptCore 只能直譯；iOS 26 的 Trusted Execution Monitor 更緊；4.7 允許 mini app 與外掛，4.7.2 要求使用者許可才能暴露原生 API | 鐵律成立；能力模型即合規文件 |

### 分階段，按風險排序

**第 0 階段（數週）四個 spike，任何一個不過就回到設計。** 2026-10-24 進度：spike 1 已在 Windows（DX12）與 Android 模擬器（Vulkan）上用同一個引擎跑起來並量測，引擎與演算法的改進見第 4.8 節，紀錄在 docs/spikes/。spike 2 的主體也完成了：頁面包（JSON 清單 + Luau + WGSL）在沙盒 realm 裡建圖，示範頁面整個改用 Luau 寫，Windows 與 Android 都跑這個版本；和同一頁面的 Rust 版相比，幀數、延遲與閒置行為相同，幀 CPU p50 多約 0.2 ms，腳本函數的成本約為它餵給的原生光柵化的十分之一（docs/spikes/spike2-script-pages.md）。 第一天就跨平台：spike 的程式碼從第一個 commit 起，就在 CI 上建 Windows、macOS、Linux、Android、iOS、Web 全部目標；量測按 GPU API 族逐一做（第 17 節）。

1. **一個頁面、所有時間性。** 最小求值器（四種格、串流輸入格、待定格、失效、需求、取消、快照）加 wgpu 桌面殼、單一排程器與分層合成器。同一個頁面同時放五種內容：靜態（引擎自己算出來的大型程序生成畫面，加一份 PDF 資源格）、拉式串流（依賴 `t` 的動畫，由原生算子或 WGSL 算）、推式串流（速率不固定的模擬即時資料與逐字輸出）、非同步（慢算子 Job，有佔位、進度、可取消）、操作（拖曳縮放，以及改一個輸入格讓結果沿圖傳播）。量：滿載時幀時間 p99 是否在預算內；各類內容從事件到像素的延遲；只有一個角落在動時，每幀成本是否只跟那個角落成正比；全部靜止時是否 0 fps；10⁵ 格的失效傳播耗時；記憶體。同一份程式碼要在 Windows（DX12）、macOS 與 iOS（Metal）、Linux 與 Android（Vulkan，含 GL ES 退路）、Web（WebGPU）上都跑起來並量測。repo 裡已有的鏡頭、tile 定址、可取消佇列沿用；PDFium 轉接層改為一個資源格算子。
2. **腳本建圖。** 一個 Luau realm 透過指令建一張圖、處理事件、定義一個腳本算子。量指令批次的跨界開銷、配額與看門狗、腳本算子被快取後的穩態成本是否為零。驗證「下載的程式碼只組裝、算子執行」這個主張。
3. **零拷貝影片。** iOS 硬體解碼 → `CVMetalTextureCache` → wgpu texture，疊一個 WGSL 濾鏡即時播放，量每幀拷貝次數。只能在 Mac 做；不過就退到每幀一次 NV12 上傳，不擋第 1 階段。其他平台的解碼路徑見第 14.2 節。
4. **原生宿主與文字輸入。** iOS 用 Swift UIView、Android 用原生 View 承載引擎畫面，疊原生輸入框，驗證中文組字、選取、剪貼簿與螢幕閱讀器在引擎畫面上都可用。winit 在 iOS 沒有 UITextInput、在 Android 沒有 InputConnection，這一步不能靠它。

**第 1 階段，執行時 MVP：** iOS、macOS、Windows、Linux、Android、Web 六個平台的殼與 CI、求值器、指令協定與日誌、Tiled / Vector / Text / NativeOverlay 四種輸出、能力代理、每頁一個 Luau VM、頁面包格式與簽章、位址導航與快取、語義埠的語義樹 + 圖視圖 + 增量 + 以 id 操作 + 效果類別。第一個示範頁面刻意混合所有時間性：一份 PDF（資源格）、一個引擎原生的計算區塊（例如表格）、一段由 `t` 驅動的動畫、一條即時資料流、一個非同步任務，彼此以埠相連（第 5.4 節），並以頁面包形式走完整條發佈路徑。真正的影片解碼在第 2 階段加入同一個頁面。

**第 2 階段：** Streamed 與 Effect（影片）、VirtualGrid、Job、agent 經語義埠完成一個完整任務、PDF 文字層原生化。

**第 3 階段：** Mesh（CAD）、外部 agent 能力票、鴻蒙與 visionOS 的殼。

### 何時才值得造新語言

只有語言本身就是產品、且現有語言在語義上做不到時。這個架構沒有這樣的缺口：圖範本是 JSON，邏輯是 Luau，著色是 WGSL。

---

## 16. 資源估算

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
| 殼、內建頁面、shader | 3–8 MB |
| **小計（PDF + 表格 + 影片）** | **約 35–60 MB** |
| OpenCascade（裁過） | 30–60 MB |
| **含 CAD** | **約 70–120 MB** |

同類 App（Acrobat、Excel、CapCut）200–400 MB，Shapr3D 約 400 MB。iOS 必須一次全帶；Android 可用 Feature Delivery 按需下載 CAD；Web 各引擎 lazy load。

### 裝置執行期
tile 快取 GPU 約 256 MB 預算；影片**就地引用**（iOS 用 PHAsset 不複製），只在需要時產 1080p 代理片（約 50 MB/分鐘），有上限；頁面包每個 100 KB–數 MB；指令日誌有保留期限。

### 開發機硬碟
Xcode + 模擬器 20–25 GB；Android Studio + SDK + NDK 15–20 GB；Rust 工具鏈 3 GB；Rust `target/` 20–60 GB；PDFium 用預編譯（bblanchon/pdfium-binaries）；OpenCascade 四平台建置 10–20 GB；Emscripten、Node 3–4 GB；測試語料 5–10 GB。**最少 100 GB，舒適 150–300 GB**，記憶體建議 32 GB。

縮小 `target/`：`[profile.dev] debug = "line-tables-only"`、`split-debuginfo = "unpacked"`、共用 `CARGO_TARGET_DIR`、`cargo sweep --time 7`。

### 外接 SSD（若在 Mac 上開發）
Thunderbolt 3/4 或 USB4 NVMe 外接盒約 2,800–3,200 MB/s，與內建差距 5–10%；USB 3.2 Gen 2 隨身 SSD 慢 10–25%；**Gen 2×2 在 Apple Silicon 掉回 10 Gbps，別買**。硬碟選有 DRAM 的 Gen4 TLC（990 Pro、SN850X、P41、T500；SN770、990 EVO Plus 無 DRAM 不選）。一定格式化 APFS，Spotlight 排除，關閉硬碟睡眠，保留擁有權。

---

## 17. 開發環境：一台 Windows、一台 Mac、一條 CI

| 機器 | 負責 | 現況（2026-10-24） |
|---|---|---|
| Windows 桌機（主力） | Rust 核心、Windows 殼（DX12 / Vulkan）、Android 建置（NDK）、Web 建置、Linux 建置與功能驗證（WSL2） | RTX 3090；Rust 1.99.1；NDK 26 與 27；WSL2 上的 Ubuntu 22.04 與 24.04；Docker；尚未接 Android 實機 |
| Mac | macOS 與 iOS 殼、Metal 除錯、Instruments、iOS 真機、visionOS 模擬器 | 舊專案用過一台 MacBook Air |
| CI（GitHub Actions） | 每次 push 建全部目標：Windows、Linux、Android、Web 在 Windows 與 Linux 機器上建，macOS 與 iOS 在 macOS 機器上建；跑單元測試，以及用軟體 Vulkan（lavapipe）跑參考畫面測試 | repo 尚未建立遠端 |
| 真機實驗室 | 每個 GPU API 族至少一台：DX12（已有）、Metal（Mac + iPhone）、Vulkan（Android Adreno、Android Mali、有獨顯的 Linux）、WebGPU（Chrome、Safari） | 缺 Android 實機與 Linux 實機 |

Apple 平台離不開 Mac 的部分是 Xcode 編譯、模擬器、真機除錯與簽章上傳。CI 的 macOS 機器負責日常建置，Mac 只做需要真機與除錯器的工作。WSL2 上的 wgpu 走轉譯過的 Vulkan，能驗功能，不能當效能數字。

做法：

- iOS 專案用 XcodeGen 或 Tuist 文字定義；Android 專案用 Gradle 加 cargo-ndk；平台相關程式碼縮到最小，藏在 trait 後面。
- CI 每次 push 建全部目標，任何一個平台壞掉都擋合併。
- Windows 上 Defender 排除 `target/`、開長路徑、開發者模式開符號連結，並注意檔案系統不分大小寫。
- Mac 不夠時用 Mac mini 當無頭建置機；Apple Developer Program 附 Xcode Cloud 每月 25 小時。

---

## 18. 未決事項

- 執行時服務的對象：終端使用者、開發者、AI 建構者，或三者。
- 商業模式（理念是「工具免費」，收費點未定）。
- **格的粒度與求值器成本**：每頁 10⁵～10⁶ 格的失效傳播能否在幀預算內，由 spike 1 回答。
- **算子 ABI 的版本策略**：多版本並存的上限、棄用流程。
- **每種格式的原生化邊界**：哪些留在資源格、哪些翻成原生值，按格式逐一決定。
- **跨頁面埠的型別系統**：結構型別或名義型別、版本相容。
- **浮點決定性**：同一份日誌在不同 GPU / CPU 上重放是否位元一致；不一致時，重放的保證降級為「語義一致」。
- 表面語言：直接用 Luau，或 TS 編譯到 Luau。
- affordance 的前置條件與效果如何形式化（決定 agent 能否真正規劃）。
- 分類器的特徵與訓練管線。
- 範圍化能力票的格式。
- 受信任區域與確認流程的 UX。
- 文字與 IME：排版、雙向、選取、組字框、無障礙樹，靠不變量 7 把輸入交回原生。winit 在 Windows 只有 IMM32，在 iOS、Android、Web 沒有組字，所以文字輸入一律由殼的原生輸入框負責。
- iOS 上的 WASM 擴充算子只能直譯，開發者自訂解碼器在 iOS 不實用。
- 和既有網頁內容的互通：是否、何時用 NativeOverlay 嵌 WebView，或以 Blitz 類引擎把 HTML 當成一種處理器頁面。
- **商店對下載頁面的界線**：內建頁面在所有商店都沒問題。下載的頁面在 App Store 與 Microsoft Store 要落在 App 描述的功能範圍內，App Store 以 4.7 小程式條款為依據。Google Play 不是無條件允許：「裝置與網路濫用」政策的直譯器例外（明文點名 Lua）要求腳本只能間接存取 Android API、不得下載原生碼或 dex、不得從遠端開啟隱藏功能，開發者要為下載的內容負責，可能有額外審查；第三方發佈的頁面還適用使用者產生內容的規範。
- **鴻蒙 NEXT 的發佈路徑**：AppGallery 3.9 禁止下載改變功能的程式碼，要決定頁面如何合規，或改走「內建頁面 + 只下載資料」。
- **遊戲主機**：需要廠商 NDA、開發機與私有 wgpu 後端，等有具體需求再評估。

---

## 19. 先例對照

### 執行時與引擎

| 系統 | 快核心 | 可變皮 | 啟示 |
|---|---|---|---|
| Flutter | C++ Impeller | Dart | 一份核心 + 薄 embedder 跑六平台可行；Dart AOT 不能 OTA，Shorebird 另寫直譯器才做到，印證鐵律 |
| Figma | C++ → WASM 自繪畫布 | JS 插件、React 面板 | 畫布自繪、面板用現成 UI 的分工 |
| Roblox | C++ | Luau | 數百萬不可信腳本在 iOS 上安全執行 |
| Blender / Neovim / Emacs | C | Python / Lua / Elisp | 內建功能也走外掛路徑，核心才會真的小 |
| Zed | Rust，自研 GPU UI | — | 生產力工具用遊戲引擎手法達到 120fps |

### 計算模型

| 系統 | 怎麼做 | 啟示 |
|---|---|---|
| Excel 等試算表 | 格 + 公式 + 計算鏈，只重算受影響的格 | 一般人能理解的增量計算；AnyPage 把它推廣到畫面與媒體 |
| salsa（rust-analyzer）、Typst comemo、Jane Street Incremental、Adapton | 增量查詢、受限記憶化、需求驅動的自調整計算 | 求值器的設計來源 |
| Blink RenderingNG | 樣式、佈局、繪製、合成分階段，每階段有失效 | 瀏覽器自己也在往顯式管線走，但被 DOM 的可變性拖住 |
| JAX、TensorFlow | 宿主語言建圖，編譯好的算子在 CPU / GPU 執行 | 「下載的程式碼組裝、算子執行」的直接先例 |
| Houdini、Blender 幾何節點、TouchDesigner、Unreal Blueprints | 節點圖驅動原生運算 | 圖可以是使用者與 AI 的編輯介面 |
| Observable notebook | 反應式格，依賴自動追蹤 | 反應式頁面的使用者模型 |
| Smalltalk、Lively Kernel | 整個環境即時、可檢視、可修改 | 可塑軟體的祖先 |

### 2026-10-24 調查：誰已經做到哪一層

結論：**沒有人做出整套，但每一層都有人做到了。** 真正沒人做過的是組合本身。

| 設計元素 | 最接近的先例 | 還缺什麼 |
|---|---|---|
| 編譯進 App 的 GPU 核心 + 可下載 Luau 位元碼 | **Rive**：Luau 腳本 2026-01 正式上線，腳本在 .riv 檔內、以型別化 protocol 圈住能碰什麼，C++ 渲染器跑 iOS / Android / Web；2026-10 又加 TypeScript 語法編成 WASM 的 AnimaScript | 動畫執行時，非文件引擎；腳本與檔案同作者；每腳本配額與多 VM 隔離未公開 |
| 每工具一 VM + 時間片 + 記憶體上限 + 能力授權 + 宿主重播顯示清單 | **Zed embedded_gpui**（2026-07，實驗）：WASM 元件跑完整 GPUI 程式，wasmtime epoch 截止 + 記憶體上限，WASI 預設只給 stdout，客端輸出保留式顯示清單由宿主重播 | 桌面、WASM、未進產品 |
| 多種文件共用一張場景圖與鏡頭 | **Univer 1.0**（2026-09）：Sheets / Docs / Slides / Boards / Bases / PDF 全在同一個 Scene → Layers → Viewports 引擎上，插件 + 指令系統 | Canvas2D + TypeScript，無 GPU，無沙盒的可下載包 |
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

這些正是「一開始沒做，之後補不回來」的三件事（第 12 節末）所在，也是這個架構的護城河。

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
- wgpu 發行紀錄：https://github.com/gfx-rs/wgpu/releases
- wgpu 自訂後端：https://github.com/gfx-rs/wgpu/pull/6658
- wgpu 鴻蒙 Vulkan 支援：https://github.com/gfx-rs/wgpu/pull/9908
- winit 發行紀錄：https://github.com/rust-windowing/winit/releases
- winit 平台支援：https://rust-windowing.github.io/winit/winit/platform/index.html
- Rust 的 OpenHarmony 目標：https://doc.rust-lang.org/rustc/platform-support/openharmony.html
- Rust 的 tvOS 與 visionOS 升為 Tier 2：https://github.com/rust-lang/rust/pull/152021
- AppGallery 審核規則：https://developer.huawei.com/consumer/en/doc/50104-03
- Google Play 下載程式碼政策：https://support.google.com/googleplay/android-developer/answer/16559646
- Microsoft Store 政策：https://learn.microsoft.com/en-us/windows/apps/publish/store-policies
- cpal：https://github.com/RustAudio/cpal
- gpu-video（Vulkan Video 到 wgpu）：https://docs.rs/gpu-video/latest/gpu_video/
- cros-codecs（VA-API）：https://lib.rs/crates/cros-codecs
- AccessKit 發行紀錄：https://github.com/AccessKit/accesskit/releases

查證時標記為**未能確認**的項目：Rive 的 WASM 在 iOS 上是直譯或 AOT、Rive 每腳本配額；微信 Skyline 底層是否為 Flutter 引擎；Robrix 是否真的上架；Vello 在 iOS / Metal 上的實測；Static Hermes 的 AOT 是否已成預設；Canva 的渲染器內部。平台調查中未能確認的有：鴻蒙 JIT 限制的官方說明、wgpu 各平台的最低 OS 版本、Android TV 與 ChromeOS 的細節、Xbox 經 GDK 走 DX12 的可行性。

一句話總結：**這是一個把所有內容放進同一個頁面的引擎。靜態、流式、非同步與操作中的內容，都是同一張計算圖上時間性不同的格，由一個排程器、一個時鐘、一個合成器驅動；下載的頁面只描述這張圖，編進 App 的原生算子與 GPU 算子執行它。** 效能來自把工作推進算子與 shader，可變性來自把所有語義推出核心，跨平台來自核心只依賴 wgpu 與 C ABI，可被 agent 操控來自每個值都可查詢、每個動作都有型別。

---

## 20. 能力模組：任何 App 都用同一套積木

目標（使用者，2026-10-24）：用這個引擎可以做任何 App。PDF、試算表、CAD、各種語言的文字編輯、雲端（AWS、GCP、OpenAI 格式串流）、影片直播、藍牙、USB、3D 模型與點雲，都能在同一個頁面裡使用，也能互相融合。平台做不到的功能（例如 App Store 上的 USB）就在那個平台缺席。只需要本機的功能免費、開源、可公開分享；雲端服務是 App 的收費方式，不是引擎的設計目的。

### 20.1 分層

| 層 | 用什麼寫 | 負責 |
|---|---|---|
| 引擎 | Rust | 計算圖、排程、時鐘、合成、tile、GPU 圖層、輸入與文字焦點 |
| 執行時 | Rust + Luau | 每頁一個沙盒 realm、`page.*` API、能力與配額、語義埠 |
| 能力模組 | Rust crate | 一個領域的原生功能：函式、工作、串流、tile、GPU 圖層 |
| UI 元件庫 | Luau | 版面、按鈕、清單、文字欄位，`import("ui")` |
| 頁面 | page.json + Luau + WGSL | 一個 App 的介面與邏輯 |

模組的契約、共用資料格式與各模組規格在 docs/MODULES.md。

### 20.2 模組能登記的六種東西

| 種類 | 在哪裡執行 | 頁面怎麼用 |
|---|---|---|
| 函式 | 呼叫它的執行緒，同步 | `pdf.page_count(doc)`、`doc:page_count()` |
| 工作 | 工作池，可取消、有進度 | `page.job(名稱, "pdf.render", {輸入})` |
| 來源 | 模組自己的執行緒，推送項目 | `page.stream(名稱, "cloud.chat", {request = 輸入格})` |
| tile | tile 工作執行緒 | `page.tiled{source = "pdf.page", params = 格}` |
| GPU 圖層 | GPU，畫進離屏紋理，有變化才重畫 | `page.layer{kind = "scene.view", inputs = {...}}` |
| Luau 函式庫 | 頁面自己的 realm | `import("ui")` |

函式分成純函式與有副作用兩種。有副作用的只能在載入時與事件處理器裡呼叫，不能在格的函數裡呼叫，所以計算圖的函數永遠是純的。

### 20.3 融合靠同一張圖與共用格式

所有模組都透過同一種值（`Data`）與資源句柄（`Res`）交換資料，所以融合不需要任何模組認識另一個模組：

- CAD 放到 PDF 上：`dwg:to_paths(...)` 產生共用的向量路徑，`pdf.add_path(...)` 把它畫進頁面。
- 用 CAD 與試算表一起運算：`dwg:measure(ids)` 的長度與面積寫進 `sheet.set(...)`，公式重算後的結果再回到圖上的其他格。
- 把 PDF 變成可運算的格式：`doc:extract(i)` 給出區塊與表格（共用表格格式），可以直接進試算表或交給 agent。

資源被就地修改時以新版本發佈（`Res::next_version`），所有讀過舊版本的格自動重算。

### 20.4 能力與平台

`page.json` 的 `capabilities.modules` 列出必需的模組，缺少時頁面不載入並說明原因；`optional_modules` 列出可有可無的模組（藍牙、USB），腳本用 `if usb then` 判斷。網路只能連 `capabilities.net` 列出的主機，憑證只能由宿主保管、由模組代用（`capabilities.secrets`），檔案只來自對話框與拖放。

### 20.5 Agent

頁面用 `page.action{...}` 宣告動作，`describe()` 與 `act(JSON)` 是語義埠。外部 agent 透過桌面宿主的 `--agent-port`（只聽 127.0.0.1，需要權杖）送 JSON；頁面內的 agent 用 `cloud.chat` 的工具呼叫，執行同一組動作。

### 20.6 現況

2026-10-25：九個模組（text、ui、pdf、sheet、cad、cloud、media、devices、scene）都已實作、經獨立審查與修正，併入主 workspace，由 `anypage-std` 一次登記；整個 workspace 701 個測試通過。四個示範 App（啟動頁 home、CAD＋試算表＋PDF 融合的 studio、人與 agent 共用的 pdf-agent、多語言編輯器 editor）在 Windows 真實視窗與 Android 模擬器上都跑得起來，並有窄螢幕版面。Android 宿主有輸入法、剪貼簿、檔案、藍牙與 USB 橋接、攝影機、Keystore 加密金鑰與頁面導覽。詳見 docs/spikes/spike3-modules.md。

