# Android host: notes for the core and the modules

What the Android host (apps/android) found while wiring the platform
services and page navigation, with what it does meanwhile. Requests that
predate this round (text input, IME, clipboard) are in CORE_REQUESTS.md.

## Dependencies added to apps/android/Cargo.toml

`anypage-devices`, `anypage-media` (to install their Android bridges; both
were already linked through `anypage-std`) and, for Android only,
`ndk-context` (already in the lock through cpal). Cargo records them in the
`anypage-android` entry of the root Cargo.lock; no new crates were
downloaded and no version changed.

## 0. engine: finished tiles left unread after a slow frame (most important)

**What.** In `Engine::frame` (crates/anypage-engine/src/engine.rs, "tiles:
finished ones first (budgeted)") finished tiles are taken from the workers'
channel only while `Instant::now() < deadline + 3 ms`, and that loop runs
after the panels and GPU layers. When a frame overruns its budget before it
gets there (every frame of pdf-demo, pdf-agent, studio on the emulator: 30
to 100 ms each), the finished tiles stay in the channel, nothing marks the
frame `deferred`, and the wake that announced them was already spent on
that frame. The engine then reports "no next wake" and the page shows blank
or blurry tiles until the next touch. Seen on the emulator with pdf-demo:
the HUD said 4 tiles in flight while the tile threads slept; one touch and
they appeared. The desktop does not show it because its frames fit the
10 ms budget.

**Meanwhile (host).** `FRAME_BUDGET_MS` = 16 on Android, and
`view::Overrun`: after a frame that overran its budget the host runs another
frame at once, for as long as frames keep showing something new, plus three
more, at most 240 in a row. Verified: pdf-demo, pdf-agent and studio now
finish their tiles without input (the last frame of a burst takes 0.2 ms
and shows nothing new).

**Proposed.** Take finished tiles before drawing panels (they are cheap to
upload), or set `deferred = true` when the loop stopped with results left
(`rx` not empty), so the frame outcome says "next frame".

## 1. media: opening a camera cannot be cancelled (anypage-media, camera.rs)

**What.** `HostCamera::open(index, w, h, fps)` must answer with a stream or
an error. On Android the first open shows the CAMERA permission dialog, so
the host waits for the user (up to 30 s, `CAMERA_OPEN` in src/bridge.rs).
The broadcast stream thread blocks in `open` meanwhile; a new request or
the page closing waits for that to end.

**Proposed.** `fn open(&self, index, w, h, fps, cancel: &dyn Fn() -> bool)`
(or a `Wait` like the devices module's) so the host polls it while waiting
for the dialog and the camera.

## 2. media: camera frames come in sensor orientation

Camera2 delivers pictures as the sensor sees them (landscape on phones,
usually rotated 90 degrees from the screen in portrait). The host hands
them over unrotated. A `rotation` field in `CameraFrame` (or the host
rotating I420 before handing it over, at a cost of one more copy per frame)
would let `media.broadcast` encode them upright.

## 3. devices: an asynchronous permission refusal has no clear message

**What.** `on_scan_failed(code)` only maps Android's scan error codes. When
the user denies "Nearby devices" after a scan was requested, the host can
only say so on the next start attempt.

**Meanwhile.** `DeviceBridge.bleStartScan` checks the permission first and
returns a message ("waiting for Bluetooth permission: allow it in the
dialog", then "Bluetooth permission was denied: allow ... in Settings")
while it shows the dialog; the scan stream retries every 3 s and starts on
its own once the user allows it.

**Proposed.** `anypage_devices::android::on_scan_error(message: &str)`.

## 4. runtime: page.navigate has no answer for the page

**What.** `PageRequest::Navigate(name)` is fire-and-forget. When the
package is missing or fails to load, the host keeps the current page and
shows a toast ("無法開啟頁面 x（Could not open the page）: ..."), but the
page cannot tell its user or try something else.

**Proposed.** A `navigate_failed(name, message)` event, or
`PageRequest::Navigate { id, name }` answered with `HostResult::Failed`.

## 5. runtime: a back stack belongs in the engine contract

The host keeps the history (`PageHistory.kt`: pages opened with
`page.navigate` go on a stack, Back returns). The desktop host has none.
`page.back()` and a "can go back" flag in `page.info` would let pages show
a back button that works the same on every host.

## 6. runtime: asking for a missing secret at the moment of use

`grants.secret(name)` fails with "not set on this device" and the page has
to tell the user to open the secrets screen (launcher shortcut
"金鑰 Secrets", or `SecretsActivity`). A `PageRequest::Secret { id, name }`
the host answers by asking the user (and storing the answer) would make the
first use smooth. The host stores per page id with an all-pages fallback,
the same lookup as the desktop's secrets.json.
