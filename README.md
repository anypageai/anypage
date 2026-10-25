# AnyPage 引擎

把所有內容放進同一個頁面的引擎。靜態、流式、非同步與操作中的內容，都是同一張計算圖上的函數，由一個排程器、一個時鐘、一個合成器驅動。沒有靜態元件與動態元件之分：靜態是輸入不變的函數，動態是輸入會變的函數，時間也只是一個輸入。

目標平台：iOS、macOS、Windows、Linux、Android、Web 第一天一起支援；鴻蒙、visionOS、電視、車機、嵌入式與遊戲主機靠移植契約接上（架構第 14 節）。

設計文件：[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。量測紀錄在 [docs/spikes/](docs/spikes/)。

狀態：**第 0 階段。** 引擎、執行時、九個能力模組與 Windows、Android 兩個宿主都已跑起來並量測過。App 用頁面包寫：`page.json` + Luau 腳本 + WGSL + 素材，在沙盒裡載入，透過能力模組使用 PDF、試算表、CAD、雲端、影片直播、藍牙、USB、3D（模組契約：[docs/MODULES.md](docs/MODULES.md)）。iOS、macOS、Linux、Web 還沒做。

## 目錄

| 路徑 | 層 | 內容 |
|---|---|---|
| `crates/anypage-core` | L1 執行時 | 計算圖：需求驅動、提早截斷、可預測時間（計時器堆積）、分幀求值；工作池；tile 佇列；鏡頭 |
| `crates/anypage-engine` | L1 執行時 | 引擎：每幀排程、預測喚醒、合成器（wgpu）、tile、GPU 算子與 GPU 圖層、輸入、文字焦點、量測 |
| `crates/anypage-text` | L1 執行時 | 文字系統：造字、雙向文字、字型後備、UAX #14 斷行、彩色 emoji |
| `crates/anypage-runtime` | L3 腳本 | 頁面包、每頁一個 Luau realm（沙盒、配額、能力）、`page.*` API、原生模組登記、語義埠 |
| `crates/anypage-ui` | L3 腳本 | UI 元件庫（Luau）：版面、按鈕、清單、表格、文字欄、對話框 |
| `crates/anypage-pdf` | L2 模組 | PDF：繪製、文字與位置、搜尋、抽成表格、註記、表單、編輯、存檔 |
| `crates/anypage-sheet` | L2 模組 | 試算表（IronCalc）：xlsx、CSV、公式 |
| `crates/anypage-cad` | L2 模組 | CAD：DXF 讀寫、繪製、量測、編輯；STEP 轉網格 |
| `crates/anypage-cloud` | L2 模組 | 雲端：OpenAI 相容串流、HTTP、S3、Google Cloud Storage |
| `crates/anypage-media` | L2 模組 | 影片：HLS 直播播放、直播推流、攝影機 |
| `crates/anypage-devices` | L2 模組 | 藍牙 LE、USB、序列埠 |
| `crates/anypage-scene` | L2 模組 | 3D：網格與點雲 |
| `crates/anypage-std` | L2 模組 | 一次登記所有模組；兩個宿主都用它 |
| `crates/anypage-demo` | L4 頁面 | 示範頁面的 Rust 版、示範用原生算子、自動量測腳本 |
| `pages/` | L4 頁面 | 頁面包：`home`（啟動頁）、`studio`（CAD＋試算表＋PDF 融合）、`pdf-agent`（人與 agent 共用的 PDF 工作台）、`editor`（多語言編輯器）、`ui-gallery`、各模組的 `*-demo`、`demo` |
| `apps/desktop` | L0 殼 | winit 殼：剪貼簿、檔案對話框、拖放、IME、頁面導覽、本機 agent 連接埠（目前只在 Windows 驗證） |
| `apps/android` | L0 殼 | Kotlin 原生宿主 + Rust JNI：輸入法、剪貼簿、檔案、藍牙、USB、攝影機、加密金鑰、頁面導覽 |

## 跑起來

Windows：

```powershell
cargo run --release -p anypage-desktop                 # 互動模式
cargo run --release -p anypage-desktop -- --bench      # 22 秒自動量測，報告寫進 bench-results/
cargo run --release -p anypage-desktop -- --page pages/home   # 從資料夾載入頁面包（改了 Luau 不必重新編譯）
cargo run --release -p anypage-desktop -- --page pages/studio --agent-port 7700   # 開放本機 agent 連接埠
cargo run --release -p anypage-desktop -- --hud        # 顯示量測面板（h 切換）
cargo run --release -p anypage-desktop -- --rust-demo  # 同一個頁面的 Rust 版，用來對照
cargo run --release -p anypage-demo --example script_costs    # 每個面板的腳本成本與光柵化成本
cargo run --release -p anypage-core --example graph_bench
cargo test --workspace
```

不帶 `--page` 時載入編進 App 的 `pages/demo`。在頁面裡用 `page.navigate` 開別的頁面後，Alt+Left 或滑鼠的上一頁鍵回到上一頁。雲端模組用的金鑰放在 `%APPDATA%\AnyPage\secrets.json` 或環境變數 `ANYPAGE_SECRET_<名稱>`。

Android（需要 Android SDK、NDK 27 與 `cargo install cargo-ndk`；Luau 的 C++ 執行時由 `.cargo/config.toml` 與 `apps/android/build.rs` 靜態連結，不必另外打包 `libc++_shared.so`）：

```powershell
$env:ANDROID_NDK_HOME = "$env:LOCALAPPDATA\Android\Sdk\ndk\27.1.12297006"
cargo ndk -t arm64-v8a -t x86_64 -P 26 -o apps/android/app/src/main/jniLibs build --release -p anypage-android
cd apps/android
$env:JAVA_HOME = "C:\Program Files\Android\Android Studio\jbr"
.\gradlew.bat assembleDebug
adb install -r app\build\outputs\apk\debug\app-debug.apk
```

操作：拖曳平移，滾輪或雙指縮放，空白鍵 串流，A 連續動畫，T 定時內容，J 任務，C 取消，1 / 2 迭代，R 貼合，H 隱藏量測面板。`ANYPAGE_BACKEND=vulkan|dx12|gl` 可強制 GPU 後端。

## 授權 License

本機程式碼以 MIT 或 Apache-2.0 雙授權釋出，任選其一：[LICENSE-MIT](LICENSE-MIT)、[LICENSE-APACHE](LICENSE-APACHE)。

Licensed under either of MIT or Apache-2.0, at your option. Unless you state otherwise, any contribution you submit for inclusion is dual licensed the same way, without additional terms.

第三方元件 Third-party notes:

- 依賴的 crate 各自保有授權（MIT、Apache-2.0、BSD、ISC、Zlib、MPL-2.0；`cargo tree` 可列出）。
- `anypage-media` 從原始碼編譯 Cisco OpenH264（BSD-2-Clause）。H.264 有專利；自行編譯的 OpenH264 不在 Cisco 代付專利授權的範圍內，散布編譯好的 App 前請自行確認。
- TLS 根憑證清單來自 `webpki-roots`（CDLA-Permissive-2.0）。
- 字型取自作業系統，倉庫不附任何字型檔。
