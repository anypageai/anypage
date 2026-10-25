# anypage-media: requests to the core

Things the media module needed from the runtime or engine, with the
workaround in place today.

## 1. Let a request follow another cell (handlers cannot read stream values)

**Problem.** `page.get` reads input cells only, and a stream's `request` must be
an input cell (its value is forwarded only on `page.set`). So a page cannot
start playing the URL its own `media.broadcast` reports (`local_url`) without an
agent in the loop: no handler can see the broadcast item, and no derived cell
can feed the player's request.

**Workaround.** The demo serves on a fixed port (18090) and plays the URL it
knows in advance. Tests read the stream cell from Rust and run an action.

**Proposal** (either is enough):

```lua
-- a: requests may be any cell; the runtime forwards each new value of it
page.stream("video", "media.hls", { request = page.derive("req", { bcast }, function(b)
    return b and b.last and b.last.local_url and { url = b.last.local_url }
end) })
-- b: handlers may read the last committed value of any named cell
local b = page.peek(bcast)   -- read-only snapshot, nil while pending
```

Runtime change for (a): in `api.rs` `page.stream`, when `request` is not an
input, add a native observer operator on that cell that sends its value into
the source's request channel whenever it changes (same `Sender<Data>`).

## 2. A capability for serving on the network

**Problem.** `media.broadcast { serve = port }` opens a listening socket. There
is no capability for that; `capabilities.net` is about outgoing connections.

**Workaround.** The bind address must be listed in `capabilities.net`:
`"127.0.0.1"` (this device only) or `"0.0.0.0"` (local network), optionally
with the port (`"0.0.0.0:18090"`). Peers outside loopback/private/link-local
ranges are refused by the server itself.

**Proposal.** `"capabilities": { "serve": { "lan": true, "ports": [18090] } }`
and `Grants::check_serve(bind: IpAddr, port: u16) -> Result<(), String>`, so
hosts can show "this page shares on your network" before the first bind.

## 3. Capture capabilities and host permission prompts

**Problem.** A page can ask for `source = "camera"` with only `"modules":
["media"]`. On Windows the OS camera privacy setting is the only gate; on
Android the host must request `CAMERA` (and `RECORD_AUDIO` later).

**Proposal.** `capabilities.camera` / `capabilities.microphone` booleans in
`Caps`, checked by the module (`Grants::check_device("camera")`), and a
`PageRequest::Permission { what }` the host answers with `HostResult`, so
the platform prompt appears at the right moment.

## 4. Android camera bridge (host work)

Defined in this crate: `camera::HostCamera` (list/open) and
`camera::CameraStream` (next_frame/size), installed with
`camera::set_host_camera(Some(Arc<dyn HostCamera>))`. The Android host needs a
JNI side that opens CameraX `ImageAnalysis` with `YUV_420_888`, copies each
`ImageProxy`'s planes (honouring `rowStride`/`pixelStride`) into
`yuv::I420` (or uses `camera::nv12_to_i420` when `pixelStride == 2`), and
pushes them into the stream (e.g. a bounded channel, newest wins).

## 5. YUV images in the engine (avoid the CPU colour conversion)

**Problem.** `anypage_engine::Image` is RGBA8 only, so every shown video frame
is converted on the CPU and uploaded at 4 bytes per pixel instead of 1.5.
Measured on a Ryzen 9 5950X at 25-40% load (release build, a fresh output
buffer per frame as the player uses): 720p 1.7 ms on one thread, 0.9 ms on
four; 1080p 3.8 ms on one thread, 1.5 ms on four. `yuv::i420_to_rgba_auto`
picks one or up to four threads by measurement. That is affordable on a
desktop, but it is still a CPU pass and 8 MB of memory traffic per 1080p
frame, which matters on phones and at 60 fps.

**Proposal.** `Image { format: ImageFormat::Rgba8 | I420 { strides } | Nv12 {...} }`
(or a separate `VideoFrame` value) and a planar upload plus a YUV->RGB
fragment shader in the compositor, with the matrix (BT.601/709, range) as a
uniform. `media.hls` would then hand frames to `page.image` without converting
them at all.

## 6. Host audio session

With the `audio` feature, sound goes to the default output through cpal.
Hosts should own audio focus (Android `AudioManager` focus, ducking; iOS
`AVAudioSession` category). A small hook such as
`PageRequest::AudioFocus { gain: bool }` would let the module ask for it when
playback with sound starts and release it on stop.

## 7. A source's default stream policy

**Problem.** `page.stream` defaults to `StreamPolicy::Append` with a cap of
256. Every `media.hls` / `media.broadcast` item carries a whole picture
(about 0.9 MB at 640x360, 8.3 MB at 1080p), so a page that forgets
`latest = true` would keep up to about 2 GB alive.

**Workaround.** The module refuses, at load, a media stream declared without
`latest = true` or an explicit `cap` of at most 16, with a message that shows
the right call.

**Proposal.** Let a source declare its default policy when it registers:

```rust
n.source_with("media.hls", SourceDefaults { policy: StreamPolicy::LatestWins, cap: 1 }, |ctx| ...);
```

`page.stream` would use it unless the page sets `latest` / `cap` itself.

## 8. Catch panics at the native boundary

**Problem.** A panic in a native function called from Luau escaped through
`ScriptPage::file_drop` into the host, and a panic in a `job_fn` ran on the
worker pool with no `catch_unwind`. This module now guards every function,
job, source constructor and thread body itself, but every module has to
remember to.

**Proposal.** Wrap native calls in the runtime: `Natives::function` /
`job_fn` / `source` callers run the closure under `catch_unwind` and turn a
panic into `Err("<name>: internal error (<message>)")`, so one module's bug
cannot take down a page or the app.
