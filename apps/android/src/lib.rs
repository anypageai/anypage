//! Android host glue (docs/ARCHITECTURE.md section 14.4, "engine as library").
//!
//! The Kotlin app owns the window and the lifecycle; it hands this library a
//! `Surface` and forwards input. The engine runs on its own render thread,
//! which blocks until there is input, a wake from another thread (stream
//! data, finished tiles or jobs), or the next predicted change.
//!
//! What the host serves for pages (docs/MODULES.md, "android"):
//!
//! | page side | Android side |
//! |---|---|
//! | `page.focus_text(rect)` / `nil` | soft keyboard shown / hidden; the caret's rectangle goes to the IME (`CursorAnchorInfo`) and the camera pans so the caret stays above the keyboard |
//! | `text` event | `InputConnection.commitText`, typed characters of hardware keys |
//! | `ime` event | `setComposingText` (preedit), `""` when composition ends |
//! | `key` event | hardware keys and IME key events by engine name ([`keys`]); `deleteSurroundingText` as backspace/delete, editor actions as enter, context-menu actions as ctrl+a/x/c/v |
//! | `page.copy` / `page.paste` + `paste` event | `ClipboardManager` |
//! | `page.open_file` / `page.save_file` | Storage Access Framework (`ACTION_OPEN_DOCUMENT` / `ACTION_CREATE_DOCUMENT`); Kotlin passes a detached file descriptor and the bytes are read or written here ([`files`]) |
//! | `file_drop` event | drag and drop of documents onto the view |
//! | `wheel` event | mouse wheel / trackpad (`MotionEvent.ACTION_SCROLL`) |
//! | `pointer_move` (hover) | `ACTION_HOVER_MOVE` |
//!
//! Limits ([`requests`]): what crosses into Java travels in binder
//! transactions, so `page.copy` takes at most 256 KB (more is refused with a
//! toast and a log line, never a crash), a save name at most 255 bytes, an
//! open request at most 32 filters. One file dialog is open at a time (a
//! second request fails with "a file dialog is already open"), and answers
//! that arrive after the page was replaced are dropped. A caret that is not
//! finite (or lies far off the page) never moves the camera. A keyboard the
//! user dismissed comes back on a tap on the field, not after a pan or a tap
//! elsewhere ([`view::Taps`]). Every JNI entry point catches panics.
//!
//! Debug builds also carry on-device test tools (app/src/debug, never in a
//! release APK): the `host-probe` page (intent extra `page=host-probe`), one
//! button per request that used to crash or break the host; `ImeProbeActivity`,
//! a platform `EditText` that logs every input-method call for comparison
//! with `EngineInputConnection`; and a shell-only debug broadcast
//! (`MainActivity.registerDebugReceiver`).
//!
//! The engine cannot hand a page's text to the input method yet
//! (CORE_REQUESTS.md, item 1), so the Kotlin input connection
//! (`EngineInputConnection.kt`) shows the IME a shadow: the text the IME typed
//! since the caret last moved. That keeps suggestions, corrections
//! (`replaceText`, `deleteSurroundingText`) and re-composing the last word
//! exact; anything that moves the caret where the IME cannot see (taps,
//! arrows, Enter, shortcuts) restarts the input with an empty shadow.
//!
//! Pages and navigation ([`packages`]): the APK carries every package of
//! the repository's `pages/` folder under `assets/pages/` (Gradle task
//! `syncPages`), binary samples (`assets/<file>`, `page.asset`) included.
//! The app starts on `home` when the APK has it, else the built-in demo;
//! the intent extra `page` chooses another, and a new intent with `page`
//! replaces the page in place. `page.navigate(name)` opens another package
//! (a package that fails to load leaves the current page and says why in a
//! toast); Back returns to the previous page.
//!
//! Platform services ([`bridge`], Android only; their logic is plain Rust in
//! [`devices`] and [`camera`]):
//!
//! | page side | Android side |
//! |---|---|
//! | `ble.*` | `BluetoothLeScanner`, `BluetoothGatt` (`DeviceBridge.kt`); asks for "Nearby devices" (API 31+) or location (older) at the first scan or connect |
//! | `usb.*`, `serial.*` (CDC-ACM) | `UsbManager`: list, the system's permission dialog, open; transfers run natively on the descriptor |
//! | `media.broadcast { source = "camera" }`, `media.cameras()` | Camera2 (`CameraBridge.kt`), `YUV_420_888` frames copied to I420 here; asks for the camera permission at the first open |
//! | `capabilities.secrets` | `SecretVault.kt`: values encrypted with an Android Keystore AES-GCM key in app-private storage, entered on the secrets screen (`SecretsActivity`, launcher shortcut "金鑰 Secrets") |
//!
//! Frames get a 16 ms budget (phones are slower than the desktop's 10 ms),
//! and a frame that overruns it is followed by another at once while frames
//! keep showing something new ([`view::Overrun`]): the engine reads finished
//! tiles only within the budget (NOTES.md, item 0).
//!
//! Everything except the JNI layer is plain Rust and is tested on the desktop.

pub mod camera;
pub mod devices;
pub mod files;
pub mod keys;
pub mod packages;
pub mod requests;
pub mod view;

#[cfg(target_os = "android")]
mod bridge;
#[cfg(target_os = "android")]
mod host;

#[cfg(test)]
mod page_tests;
