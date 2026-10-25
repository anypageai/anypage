//! Cameras for pages: `anypage_media::camera::HostCamera` served by the
//! Kotlin `CameraBridge` (Camera2, `ImageReader` in `YUV_420_888`).
//!
//! Opening is asynchronous on Android (a permission dialog, then
//! `CameraDevice.StateCallback`, then a capture session), while the media
//! module's `open` must answer at once with a stream or an error. The
//! [`CameraHub`] gives every open a token, waits for Kotlin's answer for that
//! token ([`CameraHub::opened`] / [`CameraHub::failed`]), then hands frames
//! ([`CameraHub::frame`]) to the stream that owns the token. Dropping the
//! stream closes the camera. Everything here is plain Rust and tested on the
//! desktop with a fake host; the JNI side is in `bridge.rs`.
//!
//! Frames: Camera2's `YUV_420_888` planes have any row stride and a pixel
//! stride of 1 (planar) or 2 (semi-planar, the U and V planes interleaved in
//! one buffer); [`yuv_420_888_to_i420`] copies them into the tight I420 the
//! encoder takes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anypage_media::camera::{CameraFrame, CameraStream, HostCamera};
use anypage_media::yuv::I420;

/// Largest picture accepted from the camera (either side).
pub const MAX_SIDE: usize = 8192;
/// Frames waiting for the encoder; a camera faster than the encoder drops frames.
const QUEUE: usize = 2;

/// One plane of a `YUV_420_888` image.
#[derive(Clone, Copy)]
pub struct Plane<'a> {
    pub data: &'a [u8],
    pub row_stride: usize,
    pub pixel_stride: usize,
}

impl Plane<'_> {
    /// The plane holds `cols` x `rows` samples at its strides (the last row may be short).
    fn check(&self, what: &str, cols: usize, rows: usize) -> Result<(), String> {
        if self.pixel_stride == 0 || self.pixel_stride > 4 {
            return Err(format!("{what} plane: pixel stride {}", self.pixel_stride));
        }
        let row = (cols - 1) * self.pixel_stride + 1;
        if self.row_stride < row {
            return Err(format!("{what} plane: row stride {} is shorter than a row ({row} bytes)", self.row_stride));
        }
        let need = self.row_stride * (rows - 1) + row;
        if self.data.len() < need {
            return Err(format!("{what} plane: {} bytes, {need} needed for {cols}x{rows}", self.data.len()));
        }
        Ok(())
    }

    fn copy_into(&self, dst: &mut [u8], cols: usize, rows: usize) {
        for r in 0..rows {
            let src = &self.data[r * self.row_stride..];
            let out = &mut dst[r * cols..(r + 1) * cols];
            if self.pixel_stride == 1 {
                out.copy_from_slice(&src[..cols]);
            } else {
                for (c, o) in out.iter_mut().enumerate() {
                    *o = src[c * self.pixel_stride];
                }
            }
        }
    }
}

/// Copy a `YUV_420_888` image (any strides) into a tight I420 picture.
pub fn yuv_420_888_to_i420(width: usize, height: usize, y: Plane, u: Plane, v: Plane) -> Result<I420, String> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(format!("camera picture of {width}x{height}"));
    }
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    y.check("Y", width, height)?;
    u.check("U", cw, ch)?;
    v.check("V", cw, ch)?;
    let mut pic = I420 { width, height, y: vec![0; width * height], u: vec![0; cw * ch], v: vec![0; cw * ch] };
    y.copy_into(&mut pic.y, width, height);
    u.copy_into(&mut pic.u, cw, ch);
    v.copy_into(&mut pic.v, cw, ch);
    Ok(pic)
}

/// What Kotlin's `CameraBridge` does for the hub.
pub trait CameraHost: Send + Sync + 'static {
    /// Camera names ("0: back", "1: front").
    fn list(&self) -> Vec<String>;
    /// Start opening camera `index` for `token`; the answer comes back as
    /// [`CameraHub::opened`] or [`CameraHub::failed`]. `Err`: it could not
    /// even start (no such camera, no camera service).
    fn open(&self, token: u64, index: usize, width: usize, height: usize, fps: f64) -> Result<(), String>;
    /// Stop camera `token` (idempotent).
    fn close(&self, token: u64);
}

struct Slot {
    /// Waiting for the open to finish.
    ready: Option<Sender<Result<(usize, usize), String>>>,
    frames: SyncSender<CameraFrame>,
    /// Why the camera stopped, for the stream.
    error: Arc<Mutex<Option<String>>>,
}

/// Cameras in use, by token.
pub struct CameraHub {
    host: Arc<dyn CameraHost>,
    next: AtomicU64,
    slots: Mutex<HashMap<u64, Slot>>,
    /// How long an open may take, permission dialog included.
    open_timeout: Duration,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl CameraHub {
    pub fn new(host: Arc<dyn CameraHost>, open_timeout: Duration) -> Arc<Self> {
        Arc::new(CameraHub { host, next: AtomicU64::new(1), slots: Mutex::new(HashMap::new()), open_timeout })
    }

    /// Camera `token` is open and delivers `width` x `height`.
    pub fn opened(&self, token: u64, width: usize, height: usize) {
        let ready = lock(&self.slots).get_mut(&token).and_then(|s| s.ready.take());
        match ready {
            Some(tx) => {
                let _ = tx.send(Ok((width, height)));
            }
            None => log::info!("camera {token}: opened, but nobody waits for it any more"),
        }
    }

    /// Camera `token` failed: before it opened, the open fails with
    /// `message`; after, its stream ends with it.
    pub fn failed(&self, token: u64, message: &str) {
        let slot = {
            let mut slots = lock(&self.slots);
            match slots.get_mut(&token) {
                Some(s) if s.ready.is_some() => {
                    if let Some(tx) = s.ready.take() {
                        let _ = tx.send(Err(message.to_string()));
                    }
                    None
                }
                Some(_) => slots.remove(&token),
                None => None,
            }
        };
        if let Some(s) = slot {
            *lock(&s.error) = Some(message.to_string());
            // dropping the sender ends the stream once it has read what is queued
        }
    }

    /// A picture for `token`: `make` builds it only when a stream wants it.
    /// Returns false when nobody does (Kotlin then closes the camera).
    pub fn frame(&self, token: u64, time: f64, make: impl FnOnce() -> Result<I420, String>) -> bool {
        let tx = match lock(&self.slots).get(&token) {
            Some(s) if s.ready.is_none() => s.frames.clone(),
            Some(_) => return true, // not answered as open yet: skip
            None => return false,
        };
        match make() {
            Ok(pic) => {
                // the encoder takes pictures as it can; never block the camera thread
                let _ = tx.try_send(CameraFrame { pic, time });
            }
            Err(e) => {
                log::warn!("camera {token}: frame dropped: {e}");
            }
        }
        true
    }

    /// Streams open now.
    pub fn open_count(&self) -> usize {
        lock(&self.slots).len()
    }

    fn open_stream(self: &Arc<Self>, index: usize, width: usize, height: usize, fps: f64) -> Result<Box<dyn CameraStream>, String> {
        let token = self.next.fetch_add(1, Ordering::Relaxed);
        let (ready_tx, ready_rx) = channel();
        let (ftx, frx) = sync_channel(QUEUE);
        let error = Arc::new(Mutex::new(None));
        lock(&self.slots).insert(token, Slot { ready: Some(ready_tx), frames: ftx, error: error.clone() });
        let fail = |e: String| {
            lock(&self.slots).remove(&token);
            self.host.close(token);
            Err(e)
        };
        if let Err(e) = self.host.open(token, index, width, height, fps) {
            lock(&self.slots).remove(&token);
            return Err(e);
        }
        match ready_rx.recv_timeout(self.open_timeout) {
            Ok(Ok(size)) => {
                log::info!("camera {index} open as stream {token}: {}x{}", size.0, size.1);
                Ok(Box::new(HubStream { hub: self.clone(), token, rx: frx, size, error }))
            }
            Ok(Err(e)) => fail(e),
            Err(_) => fail(format!("camera {index} did not open within {} s", self.open_timeout.as_secs())),
        }
    }
}

struct HubStream {
    hub: Arc<CameraHub>,
    token: u64,
    rx: Receiver<CameraFrame>,
    size: (usize, usize),
    error: Arc<Mutex<Option<String>>>,
}

impl CameraStream for HubStream {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CameraFrame>, String> {
        match self.rx.recv_timeout(timeout) {
            Ok(f) => Ok(Some(f)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(lock(&self.error).clone().unwrap_or_else(|| "the camera stopped".into())),
        }
    }

    fn size(&self) -> (usize, usize) {
        self.size
    }
}

impl Drop for HubStream {
    fn drop(&mut self) {
        lock(&self.hub.slots).remove(&self.token);
        self.hub.host.close(self.token);
    }
}

/// The hub as the media module's host camera.
pub struct HubCamera(pub Arc<CameraHub>);

impl HostCamera for HubCamera {
    fn list(&self) -> Vec<String> {
        self.0.host.list()
    }

    fn open(&self, index: usize, width: usize, height: usize, fps: f64) -> Result<Box<dyn CameraStream>, String> {
        if width > MAX_SIDE || height > MAX_SIDE || !fps.is_finite() {
            return Err(format!("camera size {width}x{height} at {fps} fps is out of range"));
        }
        self.0.open_stream(index, width, height, fps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Weak;

    /// Y plane of `w` x `h` with row padding, value = x + 2y.
    fn y_plane(w: usize, h: usize, stride: usize) -> Vec<u8> {
        let mut v = vec![0xEE; stride * (h - 1) + w];
        for r in 0..h {
            for c in 0..w {
                v[r * stride + c] = (c + 2 * r) as u8;
            }
        }
        v
    }

    #[test]
    fn planar_with_padding() {
        let (w, h) = (6, 4);
        let y = y_plane(w, h, 8);
        let (cw, ch) = (3, 2);
        let u: Vec<u8> = (0..4 * (ch - 1) + cw).map(|i| 100 + i as u8).collect();
        let v: Vec<u8> = (0..4 * (ch - 1) + cw).map(|i| 200 + i as u8).collect();
        let p = yuv_420_888_to_i420(w, h, Plane { data: &y, row_stride: 8, pixel_stride: 1 }, Plane { data: &u, row_stride: 4, pixel_stride: 1 }, Plane { data: &v, row_stride: 4, pixel_stride: 1 }).unwrap();
        assert_eq!(p.y[..6], [0, 1, 2, 3, 4, 5]);
        assert_eq!(p.y[6..12], [2, 3, 4, 5, 6, 7]);
        assert_eq!(p.u, vec![100, 101, 102, 104, 105, 106]);
        assert_eq!(p.v, vec![200, 201, 202, 204, 205, 206]);
    }

    /// NV21 as Camera2 usually hands it out: V and U interleaved in one
    /// buffer, U's plane starting one byte after V's, pixel stride 2.
    #[test]
    fn semi_planar_shared_buffer_and_odd_size() {
        let (w, h) = (5, 3);
        let (cw, ch) = (3, 2);
        let y = y_plane(w, h, 8);
        let row = 8;
        let mut vu = vec![0u8; row * (ch - 1) + 2 * cw];
        for r in 0..ch {
            for c in 0..cw {
                vu[r * row + 2 * c] = (50 + 10 * r + c) as u8; // V
                vu[r * row + 2 * c + 1] = (150 + 10 * r + c) as u8; // U
            }
        }
        let v = Plane { data: &vu[..row * (ch - 1) + 2 * (cw - 1) + 1], row_stride: row, pixel_stride: 2 };
        let u = Plane { data: &vu[1..], row_stride: row, pixel_stride: 2 };
        let p = yuv_420_888_to_i420(w, h, Plane { data: &y, row_stride: 8, pixel_stride: 1 }, u, v).unwrap();
        assert_eq!((p.width, p.height, p.y.len(), p.u.len()), (5, 3, 15, 6));
        assert_eq!(p.y[10..15], [4, 5, 6, 7, 8]);
        assert_eq!(p.u, vec![150, 151, 152, 160, 161, 162]);
        assert_eq!(p.v, vec![50, 51, 52, 60, 61, 62]);
    }

    #[test]
    fn short_or_strange_planes_are_refused() {
        let y = vec![0u8; 64];
        let ok = Plane { data: &y, row_stride: 8, pixel_stride: 1 };
        let short = Plane { data: &y[..10], row_stride: 4, pixel_stride: 1 };
        let e = yuv_420_888_to_i420(8, 8, ok, short, ok).unwrap_err();
        assert!(e.starts_with("U plane"), "{e}");
        assert!(yuv_420_888_to_i420(8, 8, Plane { data: &y, row_stride: 4, pixel_stride: 1 }, ok, ok).unwrap_err().contains("row stride"));
        assert!(yuv_420_888_to_i420(8, 8, ok, Plane { data: &y, row_stride: 8, pixel_stride: 0 }, ok).is_err());
        assert!(yuv_420_888_to_i420(0, 8, ok, ok, ok).is_err());
        assert!(yuv_420_888_to_i420(MAX_SIDE + 2, 2, ok, ok, ok).is_err());
    }

    /// A camera that answers like Android: later, on another thread.
    #[derive(Default)]
    struct Fake {
        hub: Mutex<Weak<CameraHub>>,
        /// what to do on open: Some(msg) fails, None opens 4x2
        fail: Mutex<Option<String>>,
        silent: std::sync::atomic::AtomicBool,
        closed: Mutex<Vec<u64>>,
    }

    impl CameraHost for Fake {
        fn list(&self) -> Vec<String> {
            vec!["0: back".into()]
        }
        fn open(&self, token: u64, index: usize, _w: usize, _h: usize, _fps: f64) -> Result<(), String> {
            if index > 0 {
                return Err(format!("no camera {index}"));
            }
            if self.silent.load(Ordering::Relaxed) {
                return Ok(());
            }
            let hub = lock(&self.hub).upgrade().unwrap();
            let fail = lock(&self.fail).clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                match fail {
                    Some(m) => hub.failed(token, &m),
                    None => {
                        hub.opened(token, 4, 2);
                        for i in 0..5 {
                            let alive = hub.frame(token, i as f64 / 30.0, || {
                                let y = vec![i as u8; 8];
                                let c = vec![128u8; 2];
                                yuv_420_888_to_i420(4, 2, Plane { data: &y, row_stride: 4, pixel_stride: 1 }, Plane { data: &c, row_stride: 2, pixel_stride: 1 }, Plane { data: &c, row_stride: 2, pixel_stride: 1 })
                            });
                            if !alive {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        hub.failed(token, "camera disconnected");
                    }
                }
            });
            Ok(())
        }
        fn close(&self, token: u64) {
            lock(&self.closed).push(token);
        }
    }

    fn hub(timeout_ms: u64) -> (Arc<Fake>, Arc<CameraHub>, HubCamera) {
        let fake = Arc::new(Fake::default());
        let hub = CameraHub::new(fake.clone(), Duration::from_millis(timeout_ms));
        *lock(&fake.hub) = Arc::downgrade(&hub);
        (fake, hub.clone(), HubCamera(hub))
    }

    #[test]
    fn open_stream_frames_then_the_camera_ends() {
        let (fake, hub, cam) = hub(2000);
        assert_eq!(cam.list(), vec!["0: back".to_string()]);
        let mut s = cam.open(0, 1280, 720, 30.0).unwrap();
        assert_eq!(s.size(), (4, 2));
        let mut got = 0;
        let err = loop {
            match s.next_frame(Duration::from_millis(500)) {
                Ok(Some(f)) => {
                    assert_eq!((f.pic.width, f.pic.height), (4, 2));
                    got += 1;
                }
                Ok(None) => panic!("frames stopped without an error"),
                Err(e) => break e,
            }
        };
        assert!(got >= 1, "{got} frames");
        assert_eq!(err, "camera disconnected");
        drop(s);
        assert_eq!(hub.open_count(), 0);
        assert_eq!(lock(&fake.closed).len(), 1, "closing the stream closes the camera");
    }

    #[test]
    fn failures_are_clear() {
        let (fake, hub, cam) = hub(10_000);
        assert_eq!(cam.open(3, 640, 480, 30.0).err().unwrap(), "no camera 3");
        *lock(&fake.fail) = Some("camera permission was denied".into());
        assert_eq!(cam.open(0, 640, 480, 30.0).err().unwrap(), "camera permission was denied");
        assert_eq!(hub.open_count(), 0, "failed opens leave nothing behind");
        // a camera that never answers
        let (slow, slow_hub, slow_cam) = self::hub(100);
        slow.silent.store(true, Ordering::Relaxed);
        let e = slow_cam.open(0, 640, 480, 30.0).err().unwrap();
        assert!(e.contains("did not open within"), "{e}");
        assert_eq!(slow_hub.open_count(), 0);
        assert_eq!(lock(&slow.closed).len(), 1, "the host is told to stop trying");
        // a late answer for a token nobody waits for is ignored
        hub.opened(999, 4, 2);
        assert!(!hub.frame(999, 0.0, || unreachable!("nobody wants it")));
        assert!(cam.open(0, MAX_SIDE + 1, 10, 30.0).is_err());
    }
}
