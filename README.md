# AnyPage 引擎

把所有內容放進同一個頁面的引擎。靜態、流式、非同步與操作中的內容，都是同一張計算圖上時間性不同的格，由一個排程器、一個時鐘、一個合成器驅動。下載的頁面只描述這張圖，編進 App 的原生算子與 GPU 算子執行它。文件只是其中一類內容。

目標平台：iOS、macOS、Windows、Linux、Android、Web 第一天一起支援；鴻蒙、visionOS、電視、車機、嵌入式與遊戲主機靠移植契約接上（架構第 14 節）。

設計文件：[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)（v2）。v1「文件引擎」版封存在 [docs/archive/ARCHITECTURE-v1.md](docs/archive/ARCHITECTURE-v1.md)。

狀態：**第 0 階段。** Spike 1「一個頁面、所有時間性」的 Windows 部分已完成並量測，紀錄在 [docs/spikes/spike1-one-page-all-timings.md](docs/spikes/spike1-one-page-all-timings.md)。其他平台尚未做。

## 跑起來

```powershell
cargo run --release -p spike1                    # 互動模式
cargo run --release -p spike1 -- --bench         # 20 秒腳本量測
cargo run --release -p anypage-core --example graph_bench
cargo test --workspace
```

拖曳平移、滾輪縮放、空白鍵 串流、A 動畫、J 任務、C 取消、1 / 2 迭代、R 貼合、H 隱藏量測面板、Esc 離開。`ANYPAGE_BACKEND=vulkan|dx12|gl` 可強制 GPU 後端。

## 目錄

| 路徑 | 層 | 內容 |
|---|---|---|
| `crates/anypage-core` | L1 執行時 | 計算圖、工作池、可取消的 tile 佇列、鏡頭、tile 定址；不依賴任何 GPU API、平台或格式 |
| `apps/spike1` | L0 殼 + 示範頁面 | winit + wgpu 合成器、每幀排程、五種時間性的示範內容 |
| `crates/anypage-pdf` | L2 算子 | PDF 匯入與光柵，暫時排除在 workspace 外 |
| `docs/` | | 架構文件、spike 紀錄、封存的 v1、舊專案的 Play 簽署筆記 |
| `third_party/pdfium/` | | PDFium 預編譯庫，不進 git |
