# AnyPage

AnyPage is a Rust engine that renders every kind of content in one page: documents, live data streams, results of background work and user interaction. All of it runs on one computation graph, one scheduler, one clock and one GPU compositor.

Apps are page packages: a `page.json` manifest, Luau scripts, WGSL shaders and assets. A page runs in a sandboxed Luau realm with memory and time limits and uses native modules for heavy work.

## How the engine decides what to compute

- **Dependency tracking.** Every value records the inputs it read. A change recomputes only the values that read it.
- **Early cutoff.** When a recomputed value equals the previous one, propagation stops and nothing is redrawn.
- **Declared validity.** A function can state the time until which its value holds. The engine sleeps until the earliest such time instead of redrawing every frame.

When nothing changes, the engine draws no frames.

## Modules

| Module | Script name | Provides |
|---|---|---|
| text | built into the engine | Shaping, bidirectional text, font fallback, UAX #14 line breaking, colour emoji |
| ui | `import("ui")` | Layout, buttons, lists, tables, text fields, dialogs |
| pdf | `pdf` | Rendering, text with positions, search, table extraction, annotations, forms, editing, saving |
| sheet | `sheet` | xlsx and CSV with formulas (IronCalc) |
| cad | `cad` | DXF read, render, measure, edit, write; STEP to meshes |
| cloud | `cloud` | OpenAI-compatible streaming chat, HTTP, Amazon S3, Google Cloud Storage |
| media | `media` | HLS live playback, live broadcast to HLS, camera capture |
| devices | `ble`, `usb`, `serial` | Bluetooth LE, USB, serial ports |
| scene | `scene` | Meshes and point clouds as a GPU layer |

A page can only use the modules its manifest declares. Network hosts, secrets, file access and the clipboard are declared the same way. The module contract and the page API are in [docs/MODULES.md](docs/MODULES.md).

## Platforms

| Platform | Status |
|---|---|
| Windows | Supported and tested |
| Android | Supported, tested on the emulator (API 35) |
| macOS, Linux | Planned |
| iOS, Web | Planned |

## Example pages

| Page | What it shows |
|---|---|
| `pages/home` | Launcher for every page, with the modules each one needs |
| `pages/studio` | A CAD floor plan measured into a spreadsheet cost table, then placed with the table into a PDF report |
| `pages/pdf-agent` | A PDF workspace operated by people and by agents, with an in-page assistant that runs page actions through tool calls |
| `pages/editor` | A multilingual text editor: IME, right-to-left text, find and replace, open and save |
| `pages/ui-gallery` | Every UI element in dark and light themes |
| `pages/*-demo` | One page per module |

## Repository layout

| Path | Contents |
|---|---|
| `crates/anypage-core` | Computation graph, worker pool, tile queue, camera |
| `crates/anypage-engine` | Frame scheduling, wake-up prediction, wgpu compositor, tiles, GPU layers, input, text focus |
| `crates/anypage-text` | Text system |
| `crates/anypage-runtime` | Page packages, Luau realms, the `page.*` API, native module registry, semantic port |
| `crates/anypage-ui` | UI component library in Luau |
| `crates/anypage-{pdf,sheet,cad,cloud,media,devices,scene}` | Capability modules |
| `crates/anypage-std` | Registers every module; both apps use it |
| `crates/anypage-demo` | Demo operators and the scripted benchmark |
| `apps/desktop` | Desktop app (winit): clipboard, file dialogs, drag and drop, IME, page navigation, local agent port |
| `apps/android` | Android app (Kotlin and JNI): IME, clipboard, files, Bluetooth, USB, camera, encrypted secrets, page navigation |
| `pages/` | Page packages |

## Build and run

Windows, with Rust 1.92 or later:

```powershell
cargo run --release -p anypage-desktop -- --page pages/home
cargo run --release -p anypage-desktop -- --page pages/studio --agent-port 7700
cargo run --release -p anypage-desktop -- --bench
cargo test --workspace
```

| Option | Effect |
|---|---|
| `--page <dir>` | Load a page package from a folder; Luau changes need no rebuild |
| `--pages <dir>` | Where `page.navigate` looks for packages (default: next to `--page`) |
| `--agent-port <port>` | Accept agent requests on 127.0.0.1 |
| `--bench` | Run the 22-second scripted benchmark and write a report to `bench-results/` |
| `--hud` | Show the measurement panel (`h` toggles it) |
| `--rust-demo` | The built-in demo page written in Rust, for comparison |

Without `--page`, the built-in demo package loads. Alt+Left or the mouse back button returns to the previous page. Secrets for the cloud module come from `ANYPAGE_SECRET_<NAME>` environment variables or `%APPDATA%\AnyPage\secrets.json`. `ANYPAGE_BACKEND=vulkan|dx12|gl` forces a GPU backend.

Android, with the Android SDK, NDK 27 and `cargo install cargo-ndk`:

```powershell
$env:ANDROID_NDK_HOME = "$env:LOCALAPPDATA\Android\Sdk\ndk\27.1.12297006"
cargo ndk -t arm64-v8a -t x86_64 -P 26 -o apps/android/app/src/main/jniLibs build --release -p anypage-android
cd apps/android
$env:JAVA_HOME = "C:\Program Files\Android\Android Studio\jbr"
.\gradlew.bat assembleDebug
adb install -r app\build\outputs\apk\debug\app-debug.apk
```

The APK includes every package in `pages/` and starts on `home`. `adb shell am start -n com.anypage.engine/.MainActivity --es page studio` opens a specific page. The C++ runtime that Luau needs is linked statically, so no `libc++_shared.so` is shipped.

## Agents

With `--agent-port`, the desktop app prints a token. A client sends the token as its first line, then one JSON request per line, and gets one JSON line back for each. For `pages/pdf-agent`:

```text
{"describe": true}
{"action": "search", "params": {"query": "revenue"}}
{"get": "info"}
{"set": "page", "value": 2}
```

`describe` lists the page's actions and named cells. Pages declare actions with `page.action{...}`; `set` only accepts inputs a page marks as agent-writable.

## Documentation

- [docs/MODULES.md](docs/MODULES.md): module contract, shared data formats, page API
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): design (Traditional Chinese)
- [docs/spikes/](docs/spikes/): measurements (Traditional Chinese)

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Unless you state otherwise, any contribution you submit for inclusion is dual licensed the same way, without additional terms.

Third-party notes:

- Dependencies keep their own licenses (MIT, Apache-2.0, BSD, ISC, Zlib, MPL-2.0); `cargo tree` lists them.
- `anypage-media` builds Cisco's OpenH264 (BSD-2-Clause) from source. H.264 is covered by patents, and Cisco's patent license covers only binaries Cisco distributes. Check your obligations before distributing an app built with it.
- TLS root certificates come from `webpki-roots` (CDLA-Permissive-2.0).
- Fonts come from the operating system; this repository ships none.
