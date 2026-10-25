# Spike 2：頁面包與腳本建圖

2026-10-24。接續 [spike 1 第二輪](spike1b-engine-and-algorithms.md)。這一輪把頁面的寫法補完整：頁面不再是編進 App 的 Rust 程式，而是一個頁面包。App 載入它、在沙盒裡執行它的腳本，腳本建出計算圖，引擎照常排程與繪製。示範頁面整個用 Luau 重寫一次，Windows 與 Android 兩個宿主都改跑這個版本。

## 頁面包

```
pages/demo/
  page.json     身分、頁面邊界、能力宣告、配額
  main.luau     建圖、事件處理
  plasma.wgsl   頁面自帶的 GPU 算子
```

- 只收文字檔。副檔名限 luau、wgsl、json、txt、md；檔名不得含路徑；單檔 2 MB、全包 16 MB。
- **只收原始碼，拒收位元碼。** Luau 原始碼在 App 內編譯，`ChunkMode::Text` 讓 VM 本身也拒絕位元碼。這樣「位元碼跨信任邊界要簽章」的問題現在不存在；之後若為了啟動速度改收位元碼，必須先有簽章（架構第 13 節）。
- 能力宣告：`streams`、`jobs`、`tiles` 列出頁面要用的原生算子名稱，`gpu` 決定能否帶 WGSL。沒宣告的一律拒絕，載入時就報錯。
- 配額：記憶體上限、每次函數呼叫的時間預算、事件處理器的時間預算、載入時間預算、工作池大小，全部有上下限夾住。

## 腳本 API

入口腳本只在載入時跑一次。它不畫任何東西，只建圖：

| 呼叫 | 建出什麼 |
|---|---|
| `page.clock()` | 時鐘格（秒） |
| `page.input(name, 初值)` | 輸入格，事件處理器用 `page.set` 改它 |
| `page.derive(name, {輸入}, fn)` | 腳本算子：`fn(值...) -> 值[, 有效到]` |
| `page.stream(name, 來源, {enabled = 輸入格})` | 原生推式串流 |
| `page.job(name, 來源, {輸入})` | 原生非同步工作，有進度、可取消 |
| `page.panel{rect, class, inputs, draw}` | 面板：`draw(d, 值...) -> [有效到]` |
| `page.tiled{rect, source, params}` | 原生 tile，以參數格的版本為快取鍵 |
| `page.effect{rect, shader, params}` | 頁面的 WGSL 算子 |

載入後圖就固定了，再呼叫上面這些會報錯。之後腳本只以兩種身分執行：

- **算子**：某個輸入真的變了，或它宣告的時間到了，才被呼叫。回傳值和上一次相同時，傳播就停在這裡。
- **事件處理器**：`page.on("pointer_down" | "pointer_drag" | "pointer_up" | "key" | "control", fn)`。處理器裡的 `page.set`、`page.cancel`、`page.request` 先記下來，處理器結束後一次套用。

尚未就緒的輸入以 `{ state = "pending", progress }` 或 `{ state = "error", message }` 交給腳本，所以進度條、佔位、錯誤訊息都是普通的函數輸出。

### 面板回傳的是顯示清單，不是像素

`draw(d, ...)` 只記錄指令：`d:rrect(...)`、`d:text(...)`、`d:series(...)`。清單是普通的值：

1. 和上一次相同就截斷，不碰像素。
2. 光柵化在原生程式裡做，而且依縮放級距：換縮放級距只重做光柵化，不重跑腳本。
3. `d:series` 把整條數列交給原生程式縮放與描線，腳本不必逐點計算。

腳本出錯的面板顯示錯誤訊息，頁面其餘部分照常運作。

## 量測

Windows，RTX 3090，DX12，2400×1500，同一份 22 秒自動量測腳本，Luau 頁面與 Rust 頁面各跑一次（重跑後的數字，見下方「離群值」）：

| 指標 | Luau 頁面 | Rust 頁面 |
|---|---|---|
| 載入到可用 | 3.0 ms | 0.8 ms |
| 幀 CPU p50 / p95 / p99 | 1.50 / 5.03 / 7.98 ms | 1.29 / 4.68 / 7.54 ms |
| 連續時的幀間隔 p99 | 17.24 ms | 17.24 ms |
| 操作到上屏 p50 | 16.57 ms | 16.57 ms |
| 推式串流項目到上屏 p50 | 23.66 ms | 23.84 ms |
| 閒置 3 秒 | 0 幀，1 次喚醒 | 0 幀，1 次喚醒 |
| 可預測時間 3 秒（預測 8 次變化） | 8 幀，9 次喚醒 | 8 幀，9 次喚醒 |
| tile 光柵化 | 1252 張 | 1252 張 |
| 計算圖 | 44 格 | 32 格 |

腳本函數與它餵給的原生光柵化，各自的成本（`cargo run --release -p anypage-demo --example script_costs`，40 輪，每輪換一次縮放級距逼光柵化重跑）：

| 面板 | 腳本執行次數 | 腳本 p50 | 原生光柵化 p50 |
|---|---|---|---|
| title | 1 / 40 | 44 µs | 335 µs |
| clock | 3 / 40 | 86 µs | 413 µs |
| controls | 1 / 40 | 293 µs | 3013 µs |
| chart（600 點） | 40 / 40 | 160 µs | 4572 µs |
| tokens | 15 / 40 | 55 µs | 1137 µs |
| orbit | 40 / 40 | 82 µs | 862 µs |
| jobs | 2 / 40 | 116 µs | 2183 µs |

腳本只佔光柵化成本的十分之一左右，而且大多數幀根本不執行：時鐘一秒只跑一次，標題只跑一次。這驗證了「下載的程式碼只組裝計算，重活由原生算子做」。

Android 模擬器（x86_64，Vulkan）：

- 載入 85.9 ms（冷啟動，含第一次字形光柵化）。
- 點按鈕、非同步任務進度、暫停計時都正常。
- 全部暫停後 5 秒內 CPU 只用了 1 個 tick（10 ms）。
- debug APK 18.9 MB（兩個 ABI），加入 Luau 前是 14.7 MB。arm64 原生函式庫 8.67 MB。

### 離群值

Luau 頁面第一次跑量測時，閒置窗口畫了 23 幀，可預測窗口畫了 18 幀，連續時的幀間隔最大 1081 ms，tile 多光柵化了 809 張。之後 Luau、Rust、Luau 連跑三次，數字都和上表一致。判斷是剛建完執行檔第一次啟動時的一秒停頓，讓 tile 工作延後到閒置窗口裡完成。沒有找到程式上的原因，記下來，之後的量測都至少跑兩次。

## 安全邊界的現況

| 機制 | 做法 | 驗證 |
|---|---|---|
| 沙盒 | `sandbox(true)`：全域與標準庫凍結，頁面的全域變數寫進自己的環境 | 改 `color.rgba`、`math.floor` 會失敗 |
| 移除的全域 | `loadstring`、`load`、`require`、`getfenv`、`setfenv` 等 | `loadstring` 呼叫失敗 |
| 時間 | 每次呼叫設期限，`interrupt` 每 128 次檢查一次時鐘 | 無窮迴圈 20 ms 內停下，VM 之後仍可用 |
| 記憶體 | `set_memory_limit`，預設 64 MB | 1 億元素的表會失敗 |
| 位元碼 | 拒收 | 以 0x1B 開頭的檔案在打包和 VM 兩層都被拒 |
| 能力 | 只能叫宣告過的原生算子 | 未宣告的串流在載入時報錯 |
| 著色器 | 載入時用 naga 解析、驗證，並檢查 `vs_main` / `fs_main`；建管線時再包一層 wgpu 錯誤範圍，失敗只停用那個效果 | 語法錯的著色器在載入時被拒；綁定不符的著色器只停用該效果，頁面照常運作 |

錯誤訊息帶腳本行號，例如 `main.luau:2: stream source "demo.ticks" is not declared in page.json capabilities.streams`。

## 踩到的坑

- **Android 的 C++ 執行時。** Luau 是 C++，預設會讓函式庫依賴 `libc++_shared.so`，要另外打包。改成靜態連結（`.cargo/config.toml` 對 Android 目標設 `CXXSTDLIB_*=c++_static`）後，又有 38 個 C++ ABI 符號沒解析，因為 `libc++_static.a` 不含 `libc++abi`。`apps/android/build.rs` 在連結命令最後補上 `-lc++abi` 才解決。用 `llvm-readelf` 確認：兩個 ABI 都只依賴 Android 系統函式庫，沒有未解析的 C++ 符號，載入段仍是 16 KB 對齊。
- **mlua 的 chunk 名稱。** 直接用 `main.luau` 當名稱，錯誤會顯示成 `[string "main.luau"]:2:`；改成 `@main.luau` 才是 `main.luau:2:`。
- **主機函式報錯沒有行號。** 從 Rust 回呼丟出的錯誤只有 `[C]: in ?`。所有 `page.*` 都包一層，用 `inspect_stack` 找到呼叫它的腳本行。

- **壞掉的著色器會讓整個 App 崩潰。** 實測一個語法錯誤的 WGSL：wgpu 預設把錯誤當致命，`create_shader_module` 直接 panic。改成上表的兩層防護。

## 還沒做

- 原生算子（tile、job）的耗時不算進頁面配額。要按頁面計算工作池時間。
- 事件處理器的預算預設 50 ms，比一幀長。要量真實頁面後收緊，或改成超時就把處理器延後到下一幀。
- `page.get` 只能讀輸入格。
- 熱重載、多檔模組（`require`）、型別定義檔、簽章都還沒有。
