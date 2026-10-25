# Core and host requests from the devices module

Things outside `crates/anypage-devices` that this module needs or would
benefit from, with what it does meanwhile.

## 1. Android host: implement the device bridge (apps/android)

**Needed for devices to work on Android at all.** The module side is done:
on Android, `register()` installs backends that go through
`anypage_devices::android` (no btleplug, no serialport). The host must:

1. Implement `anypage_devices::AndroidDevices` in Rust by calling a Kotlin
   object over JNI (keep a `GlobalRef` and the `JavaVM`; attach the calling
   thread). Every method returns quickly; results come back through callbacks.
   The mapping to Android APIs is in the `android` module docs (scan with
   `BluetoothLeScanner`, connect with `connectGatt(..., TRANSPORT_LE)`, one
   GATT op at a time is already enforced on the Rust side, notifications via
   descriptor 0x2902, USB through `UsbManager` and
   `UsbDeviceConnection.getFileDescriptor()`).
2. Call `anypage_devices::set_android_bridge(Arc::new(bridge))` once at
   startup (before or after pages load; backends look it up per call).
3. Export JNI functions that forward Kotlin callbacks to the free functions
   `anypage_devices::android::{on_scan_result, on_scan_failed,
   on_connection_state, on_services_discovered, on_op_complete,
   on_notification, on_usb_permission, on_usb_detached}`.
   Until the bridge is installed the modules are still registered; every
   call fails with a clear "has not installed the device bridge" error,
   and `ble.available()` / `usb.available()` / `serial.available()` return
   `{ ok = false, reason }` so a page can say so before the first use.
   Proposed Kotlin side: `object DeviceBridge { external fun onScanResult(address: String, name: String?, rssi: Int, services: Array<String>, mfgIds: IntArray, mfgData: Array<ByteArray>, connectable: Boolean); ... }`.
4. Manifest: `BLUETOOTH_SCAN` (`neverForLocation`), `BLUETOOTH_CONNECT`,
   `ACCESS_FINE_LOCATION` with `maxSdkVersion="30"`,
   `<uses-feature android:name="android.hardware.usb.host" android:required="false"/>`,
   `<uses-feature android:name="android.hardware.bluetooth_le" android:required="false"/>`,
   and runtime permission requests before the first scan.

`tests/android_bridge.rs` contains a fake host that behaves like Android
(asynchronous callbacks, one operation in flight, permission dialog) and
shows the exact call order the Kotlin side will see.

## 2. Runtime: tell a source whether it has a request cell

`SourceCtx::requests` gives the request cell's values. When a page declares
a stream without a request cell, the runtime's sender lives until the
source function returns, so a source cannot tell "no cell" from "no value
yet" at start. Workaround here: the stream thread treats a disconnect with
no value received as "no cell" and uses the stream options as the request
(`util::RequestFeed`).

Proposed: `pub fn has_request(&self) -> bool` on `SourceCtx`.

## 3. Runtime: dropping a page should cancel its running jobs

`ScriptPage::drop` only sets the quit flag. A native job waiting on a
device (a USB read with a 30 s timeout, a Bluetooth connect, a serial
write held by flow control) keeps its worker, and dropping the graph's
thread pool then waits for it. After `shutdown()` (which calls
`graph.cancel_all()`) the module's jobs end within about 25-50 ms
(`tests/pages.rs`: `long_jobs_are_cancelled`, `serial_write_is_cancellable`).

Proposed: `impl Drop for ScriptPage` also calls `self.graph.cancel_all()`.

## 4. Runtime/engine: jobs are lazy, effects need a better home

A job runs when something demands its value. A `ble.write` or
`usb.bulk_out` job set from a click handler does nothing until a panel or
cell reads it. This module offers effect functions for fire-and-forget
writes (`ble.send`, `serial.send`), which queue on the stream thread that
holds the device. USB has no such effect (it has no long-lived stream).

Proposed (either): `page.job(name, source, inputs, { eager = true })` that
the engine evaluates whenever its inputs change, or `page.run(source,
request, done)` for one-shot native operations from handlers, with the
result delivered to a callback like `open_file`.

## 5. Runtime: read any cell's last value from a handler

Handlers can `page.get` only input cells. A page that shows a list from a
stream (scanned devices, ports) cannot find which item a click hit without
remembering it during drawing; the demo's draw functions store row hit
boxes in script variables. The UI library will need the same.

Proposed: `page.peek(cell)` in handlers: the last computed value of any
cell, without evaluating.

## 6. Engine: Windows packaging note for Bluetooth

btleplug uses WinRT Bluetooth APIs. Unpackaged desktop apps work on
Windows 10 1803+ (verified here: 4-6 devices heard in 5-6 s scans), but
the user must allow Bluetooth access under Settings > Privacy & security >
Bluetooth for desktop apps; the module reports "Bluetooth access was
denied" when blocked. If the app is ever MSIX-packaged it needs the
`bluetooth` device capability in its manifest.

## 7. Runtime: release native resources a script has seen, promptly

`usb.open` returns a `usb.device` resource (`Res`) that keeps the device
open with its interfaces claimed (exclusive under WinUSB, an open
`UsbDeviceConnection` on Android). The graph drops its copy when the job
runs again or the page closes, and page teardown releases everything
(tested). But once a script has read the value (a panel drew it), the
Luau VM holds a userdata copy until its incremental garbage collector
happens to free it, which can be much later on a quiet page. The demo
works around this by calling `usb.close` on the previous device when the
user picks another; the crate docs tell pages to do the same.

Proposed (either): run a Luau GC step (`lua.gc_step()` or a full collect
when cheap) after handlers and draw calls that received values holding a
`Res`, or give `Res` an optional "exclusive" flag the runtime uses to
collect promptly when the graph replaces the cell value it came from.
This matters for every exclusive native resource (open files, cameras,
sockets), not only USB.

## 8. Runtime: a function class for blocking-but-pure calls

`usb.devices()` and `serial.ports()` ask the operating system (3-30 ms on
this machine, more on slower ones). They have no side effects, so they are
`Effect::Pure`, but run inline when a cell calls them and can hit the
50 ms `call_ms` budget. This module now also offers them as jobs
(`usb.list`, `serial.list`, which the demo uses with default limits), and
the docs steer cells to the jobs.

Proposed: `Effect::Blocking` (allowed in handlers and at load, refused in
cell functions with a message naming the job to use instead), so the
runtime enforces what the docs ask.
