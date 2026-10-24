# AnyPage 引擎

把所有內容放進同一個頁面的引擎。靜態、流式、非同步與操作中的內容，都是同一張計算圖上的函數，由一個排程器、一個時鐘、一個合成器驅動。沒有靜態元件與動態元件之分：靜態是輸入不變的函數，動態是輸入會變的函數，時間也只是一個輸入。

目標平台：iOS、macOS、Windows、Linux、Android、Web 第一天一起支援；鴻蒙、visionOS、電視、車機、嵌入式與遊戲主機靠移植契約接上（架構第 14 節）。

設計文件：[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。量測紀錄在 [docs/spikes/](docs/spikes/)。

狀態：**第 0 階段。** 引擎核心與 Windows、Android 兩個宿主已經跑起來並量測過。iOS、macOS、Linux、Web 還沒做。

## 目錄

| 路徑 | 層 | 內容 |
|---|---|---|
| `crates/anypage-core` | L1 執行時 | 計算圖：需求驅動、提早截斷、可預測時間（計時器堆積）、分幀求值；工作池；tile 佇列；鏡頭。不依賴任何 GPU API、平台或格式 |
| `crates/anypage-engine` | L1 執行時 | 引擎：每幀排程、預測喚醒、合成器（wgpu）、頁面提供的 GPU 算子、tile 系統、手勢、量測 |
| `crates/anypage-demo` | L4 頁面 | 內建示範頁面與自動量測腳本 |
| `apps/desktop` | L0 殼 | Windows、macOS、Linux 的 winit 殼（目前只在 Windows 驗證） |
| `apps/android` | L0 殼 | Android：Kotlin 原生宿主 + Rust JNI 函式庫 |
| `crates/anypage-pdf` | L2 算子 | PDF 匯入，暫時排除在 workspace 外 |

## 跑起來

Windows：

```powershell
cargo run --release -p anypage-desktop                 # 互動模式
cargo run --release -p anypage-desktop -- --bench      # 22 秒自動量測，報告寫進 bench-results/
cargo run --release -p anypage-core --example graph_bench
cargo test --workspace
```

Android（需要 Android SDK、NDK 27 與 `cargo install cargo-ndk`）：

```powershell
$env:ANDROID_NDK_HOME = "$env:LOCALAPPDATA\Android\Sdk\ndk\27.1.12297006"
cargo ndk -t arm64-v8a -t x86_64 -P 26 -o apps/android/app/src/main/jniLibs build --release -p anypage-android
cd apps/android
$env:JAVA_HOME = "C:\Program Files\Android\Android Studio\jbr"
.\gradlew.bat assembleDebug
adb install -r app\build\outputs\apk\debug\app-debug.apk
```

操作：拖曳平移，滾輪或雙指縮放，空白鍵 串流，A 連續動畫，T 定時內容，J 任務，C 取消，1 / 2 迭代，R 貼合，H 隱藏量測面板。`ANYPAGE_BACKEND=vulkan|dx12|gl` 可強制 GPU 後端。
