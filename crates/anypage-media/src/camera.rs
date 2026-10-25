//! Cameras for `media.broadcast { source = "camera" }`.
//!
//! * Windows: Media Foundation (source reader, NV12 output with the
//!   reader's own decoders and converters), behind the `camera` feature.
//! * Android, iOS and anything else: the host app owns the camera (Camera2 /
//!   CameraX, AVFoundation, permissions, lifecycle) and hands frames in through
//!   [`set_host_camera`]. The bridge the host implements is [`HostCamera`].
//!
//! Frames reach the encoder as I420; [`nv12_to_i420`] and
//! [`crate::yuv::rgba_to_i420`] cover the layouts hosts usually have.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::yuv::I420;

/// One camera picture and its capture time (seconds on any monotonic clock).
pub struct CameraFrame {
    pub pic: I420,
    pub time: f64,
}

/// An open camera.
pub trait CameraStream: Send {
    /// The next picture, waiting at most `timeout`; `Ok(None)` on timeout.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CameraFrame>, String>;
    /// Actual picture size.
    fn size(&self) -> (usize, usize);
}

/// What a host implements to lend its camera to pages (Android over JNI:
/// open a CameraX `ImageAnalysis` with `YUV_420_888`, copy each `Image`'s
/// planes into an [`I420`] (or NV12 via [`nv12_to_i420`]) and hand it to the
/// returned stream, e.g. through a channel).
pub trait HostCamera: Send + Sync {
    /// Camera names in the order `index` counts them.
    fn list(&self) -> Vec<String>;
    /// Open camera `index`, preferring `width` x `height` at `fps`.
    fn open(&self, index: usize, width: usize, height: usize, fps: f64) -> Result<Box<dyn CameraStream>, String>;
}

static HOST: Mutex<Option<Arc<dyn HostCamera>>> = Mutex::new(None);

/// Install (or remove) the host's camera. It takes precedence over the
/// built-in Windows capture.
pub fn set_host_camera(camera: Option<Arc<dyn HostCamera>>) {
    *HOST.lock().unwrap_or_else(|p| p.into_inner()) = camera;
}

fn host() -> Option<Arc<dyn HostCamera>> {
    HOST.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Camera names available to pages.
pub fn list() -> Result<Vec<String>, String> {
    if let Some(h) = host() {
        return Ok(h.list());
    }
    #[cfg(all(windows, feature = "camera"))]
    {
        mf::list()
    }
    #[cfg(not(all(windows, feature = "camera")))]
    {
        Err(unavailable())
    }
}

pub fn unavailable() -> String {
    if cfg!(windows) {
        "camera capture is not in this build (feature \"camera\")".into()
    } else {
        "camera is not available on this platform until the host app provides one (camera::set_host_camera)".into()
    }
}

pub fn open(index: usize, width: usize, height: usize, fps: f64) -> Result<Box<dyn CameraStream>, String> {
    if let Some(h) = host() {
        return h.open(index, width, height, fps);
    }
    #[cfg(all(windows, feature = "camera"))]
    {
        mf::open(index, width, height, fps)
    }
    #[cfg(not(all(windows, feature = "camera")))]
    {
        let _ = (index, width, height, fps);
        Err(unavailable())
    }
}

/// NV12 (Y plane, then interleaved UV) to I420.
pub fn nv12_to_i420(data: &[u8], width: usize, height: usize, stride: usize) -> Result<I420, String> {
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    if width == 0 || height == 0 || stride < width || data.len() < stride * height + stride * (ch - 1) + cw * 2 {
        return Err(format!("NV12 buffer too small for {width}x{height}"));
    }
    let mut out = I420::new(width, height);
    for r in 0..height {
        out.y[r * width..(r + 1) * width].copy_from_slice(&data[r * stride..r * stride + width]);
    }
    let uv = &data[stride * height..];
    for r in 0..ch {
        let row = &uv[r * stride..r * stride + cw * 2];
        for c in 0..cw {
            out.u[r * cw + c] = row[2 * c];
            out.v[r * cw + c] = row[2 * c + 1];
        }
    }
    Ok(out)
}

/// Nearest-neighbour resize of an I420 picture (camera size to encoder size).
pub fn resize_i420(src: &I420, width: usize, height: usize) -> I420 {
    if src.width == width && src.height == height {
        return src.clone();
    }
    let mut out = I420::new(width, height);
    let (sw, sh) = (src.width.max(1), src.height.max(1));
    for y in 0..height {
        let sy = (y * sh / height.max(1)).min(sh - 1);
        for x in 0..width {
            let sx = (x * sw / width.max(1)).min(sw - 1);
            out.y[y * width + x] = src.y[sy * src.width + sx];
        }
    }
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let (scw, sch) = (sw.div_ceil(2), sh.div_ceil(2));
    for y in 0..ch {
        let sy = (y * sch / ch.max(1)).min(sch - 1);
        for x in 0..cw {
            let sx = (x * scw / cw.max(1)).min(scw - 1);
            out.u[y * cw + x] = src.u[sy * scw + sx];
            out.v[y * cw + x] = src.v[sy * scw + sx];
        }
    }
    out
}

#[cfg(all(windows, feature = "camera"))]
mod mf {
    use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::Duration;

    use windows::core::PWSTR;
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED};

    use super::{nv12_to_i420, CameraFrame, CameraStream};

    /// COM and Media Foundation for the current thread. Callers use threads
    /// of their own (`list` and `open` both do), but a thread whose apartment
    /// is already set (an STA UI thread, where `CoInitializeEx` answers
    /// RPC_E_CHANGED_MODE) works too: COM is then used as it is, and not
    /// uninitialised by us.
    struct Com {
        uninit: bool,
    }
    impl Com {
        fn init() -> Result<Com, String> {
            unsafe {
                let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
                // S_OK and S_FALSE (already initialised) both need a matching CoUninitialize
                let uninit = hr.is_ok();
                if hr.is_err() && hr != RPC_E_CHANGED_MODE {
                    return Err(format!("COM: {hr:?}"));
                }
                MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| {
                    if uninit {
                        CoUninitialize();
                    }
                    format!("Media Foundation: {e}")
                })?;
                Ok(Com { uninit })
            }
        }
    }
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe {
                let _ = MFShutdown();
                if self.uninit {
                    CoUninitialize();
                }
            }
        }
    }

    /// Run `f` on the calling thread with COM and Media Foundation ready.
    fn with_com<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let _com = Com::init()?;
        f()
    }

    /// The video capture devices, activated lazily by index.
    fn devices() -> Result<Vec<IMFActivate>, String> {
        unsafe {
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 1).map_err(|e| e.to_string())?;
            let attrs = attrs.ok_or("no attributes")?;
            attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID).map_err(|e| e.to_string())?;
            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFEnumDeviceSources(&attrs, &mut list, &mut count).map_err(|e| format!("camera list: {e}"))?;
            let mut out = Vec::new();
            if !list.is_null() {
                for i in 0..count as usize {
                    if let Some(a) = (*list.add(i)).take() {
                        out.push(a);
                    }
                }
                CoTaskMemFree(Some(list as *const _));
            }
            Ok(out)
        }
    }

    fn name(a: &IMFActivate) -> String {
        unsafe {
            let mut p = PWSTR::null();
            let mut len = 0u32;
            if a.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut p, &mut len).is_ok() && !p.is_null() {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                s
            } else {
                "camera".into()
            }
        }
    }

    /// Camera names. Enumerates on a short-lived thread of its own (in the
    /// multithreaded apartment), whatever COM state the calling thread has:
    /// page scripts run on the host's UI thread, which is an STA.
    pub fn list() -> Result<Vec<String>, String> {
        let (tx, rx) = sync_channel::<Result<Vec<String>, String>>(1);
        std::thread::Builder::new()
            .name("media-camera-list".into())
            .spawn(move || {
                let r = std::panic::catch_unwind(|| with_com(|| Ok(devices()?.iter().map(name).collect())))
                    .unwrap_or_else(|e| Err(format!("camera list: internal error ({})", crate::panic_message(&*e))));
                let _ = tx.send(r);
            })
            .map_err(|e| format!("camera list thread: {e}"))?;
        rx.recv_timeout(LIST_TIMEOUT).unwrap_or_else(|_| Err(format!("camera list did not answer within {} s", LIST_TIMEOUT.as_secs())))
    }

    /// Same as [`list`] but on the calling thread (tests of the COM handling).
    #[cfg(test)]
    pub fn list_here() -> Result<Vec<String>, String> {
        with_com(|| Ok(devices()?.iter().map(name).collect()))
    }

    const LIST_TIMEOUT: Duration = Duration::from_secs(5);

    struct Mf {
        rx: Receiver<Result<CameraFrame, String>>,
        size: (usize, usize),
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl CameraStream for Mf {
        fn next_frame(&mut self, timeout: Duration) -> Result<Option<CameraFrame>, String> {
            match self.rx.recv_timeout(timeout) {
                Ok(Ok(f)) => Ok(Some(f)),
                Ok(Err(e)) => Err(e),
                Err(RecvTimeoutError::Timeout) => Ok(None),
                Err(RecvTimeoutError::Disconnected) => Err("camera stopped".into()),
            }
        }
        fn size(&self) -> (usize, usize) {
            self.size
        }
    }

    impl Drop for Mf {
        fn drop(&mut self) {
            // the capture thread ends after its current read (one frame time);
            // a camera that stopped delivering does not hold the caller up
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = crate::join_within(vec![t], Duration::from_millis(500));
            }
        }
    }

    struct Reader {
        reader: IMFSourceReader,
        width: usize,
        height: usize,
        stride: usize,
    }

    fn open_reader(index: usize, width: usize, height: usize, fps: f64) -> Result<Reader, String> {
        unsafe {
            let devs = devices()?;
            if devs.is_empty() {
                return Err("no camera found".into());
            }
            let act = devs.get(index).ok_or_else(|| format!("camera {index} does not exist ({} found)", devs.len()))?;
            let source: IMFMediaSource = act.ActivateObject().map_err(|e| format!("open camera: {e}"))?;
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 2).map_err(|e| e.to_string())?;
            let attrs = attrs.ok_or("no attributes")?;
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).map_err(|e| e.to_string())?;
            let reader = MFCreateSourceReaderFromMediaSource(&source, &attrs).map_err(|e| format!("camera reader: {e}"))?;
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            let want = |w: usize, h: usize, with_rate: bool| -> windows::core::Result<()> {
                let t = MFCreateMediaType()?;
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
                if w > 0 && h > 0 {
                    t.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
                }
                if with_rate {
                    t.SetUINT64(&MF_MT_FRAME_RATE, ((fps.round().max(1.0) as u64) << 32) | 1)?;
                }
                reader.SetCurrentMediaType(stream, None, &t)
            };
            want(width, height, true).or_else(|_| want(width, height, false)).or_else(|_| want(0, 0, false)).map_err(|e| format!("camera has no NV12 mode: {e}"))?;
            let cur = reader.GetCurrentMediaType(stream).map_err(|e| e.to_string())?;
            let size = cur.GetUINT64(&MF_MT_FRAME_SIZE).map_err(|e| e.to_string())?;
            let (w, h) = ((size >> 32) as usize, (size & 0xFFFF_FFFF) as usize);
            let stride = cur.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| (s as i32).unsigned_abs() as usize).unwrap_or(w).max(w);
            Ok(Reader { reader, width: w, height: h, stride })
        }
    }

    fn read(r: &Reader) -> Result<Option<CameraFrame>, String> {
        unsafe {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            r.reader
                .ReadSample(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample))
                .map_err(|e| format!("camera read: {e}"))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Err("camera stream ended".into());
            }
            let Some(sample) = sample else { return Ok(None) };
            let buf = sample.ConvertToContiguousBuffer().map_err(|e| e.to_string())?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut ptr, None, Some(&mut len)).map_err(|e| e.to_string())?;
            let pic = if ptr.is_null() {
                Err("camera returned an empty buffer".to_string())
            } else {
                let data = std::slice::from_raw_parts(ptr, len as usize);
                // some drivers pad rows; derive the stride from the buffer size
                let stride = if data.len() >= r.stride * r.height * 3 / 2 { r.stride } else { r.width };
                nv12_to_i420(data, r.width, r.height, stride)
            };
            let _ = buf.Unlock();
            Ok(Some(CameraFrame { pic: pic?, time: ts as f64 / 1e7 }))
        }
    }

    pub fn open(index: usize, width: usize, height: usize, fps: f64) -> Result<Box<dyn CameraStream>, String> {
        let (tx, rx) = sync_channel::<Result<CameraFrame, String>>(2);
        let (ready_tx, ready_rx) = sync_channel::<Result<(usize, usize), String>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("media-camera".into())
            .spawn(move || {
                let com = match Com::init() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let r = match open_reader(index, width, height, fps) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok((r.width, r.height)));
                while !s.load(Ordering::Relaxed) {
                    match read(&r) {
                        Ok(Some(f)) => {
                            // the encoder takes the newest picture; never block the camera
                            let _ = tx.try_send(Ok(f));
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            break;
                        }
                    }
                }
                drop(r);
                drop(com);
            })
            .map_err(|e| format!("camera thread: {e}"))?;
        let size = match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(sz)) => sz,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                return Err("camera did not start within 10 s".into());
            }
        };
        Ok(Box::new(Mf { rx, size, stop, thread: Some(thread) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_and_resize() {
        let (w, h) = (4usize, 2usize);
        let mut nv12 = vec![0u8; 8 + 4];
        nv12[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        nv12[8..].copy_from_slice(&[10, 20, 11, 21]);
        let p = nv12_to_i420(&nv12, w, h, w).unwrap();
        assert_eq!(p.y, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(p.u, vec![10, 11]);
        assert_eq!(p.v, vec![20, 21]);
        assert!(nv12_to_i420(&nv12[..5], w, h, w).is_err());
        let big = resize_i420(&p, 8, 4);
        assert_eq!((big.width, big.height, big.y.len(), big.u.len()), (8, 4, 32, 8));
        assert_eq!(big.y[0], 1);
        assert_eq!(big.u[3], 11);
    }

    struct Fake;
    struct FakeStream(u64);
    impl CameraStream for FakeStream {
        fn next_frame(&mut self, _t: Duration) -> Result<Option<CameraFrame>, String> {
            self.0 += 1;
            Ok(Some(CameraFrame { pic: I420::new(64, 48), time: self.0 as f64 / 30.0 }))
        }
        fn size(&self) -> (usize, usize) {
            (64, 48)
        }
    }
    impl HostCamera for Fake {
        fn list(&self) -> Vec<String> {
            vec!["fake".into()]
        }
        fn open(&self, _i: usize, _w: usize, _h: usize, _f: f64) -> Result<Box<dyn CameraStream>, String> {
            Ok(Box::new(FakeStream(0)))
        }
    }

    #[test]
    fn host_bridge_and_enumeration() {
        // whatever the machine has, listing never panics
        let _ = list();
        set_host_camera(Some(Arc::new(Fake)));
        assert_eq!(list().unwrap(), vec!["fake".to_string()]);
        let mut s = open(0, 64, 48, 30.0).unwrap();
        assert_eq!(s.size(), (64, 48));
        assert!(s.next_frame(Duration::from_millis(10)).unwrap().is_some());
        set_host_camera(None);
        #[cfg(not(all(windows, feature = "camera")))]
        assert!(open(0, 64, 48, 30.0).is_err());
    }

    /// Page scripts run on the desktop host's UI thread, which winit makes an
    /// STA (OleInitialize for drag and drop). Listing must work from there.
    #[cfg(all(windows, feature = "camera"))]
    #[test]
    fn listing_works_from_an_sta_thread() {
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
        // the Media Foundation path itself (another test installs a host camera meanwhile)
        let fresh = std::thread::spawn(super::mf::list).join().unwrap();
        let (on_sta, here_on_sta) = std::thread::spawn(|| {
            let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            assert!(hr.is_ok(), "{hr:?}");
            let r = (super::mf::list(), super::mf::list_here());
            unsafe { CoUninitialize() };
            r
        })
        .join()
        .unwrap();
        println!("cameras: fresh thread {fresh:?}; STA thread {on_sta:?}; on the STA itself {here_on_sta:?}");
        let fresh = fresh.expect("listing works on a plain thread");
        assert_eq!(on_sta.expect("listing works when called from an STA thread"), fresh);
        assert_eq!(here_on_sta.expect("an STA caller's apartment is used as it is (RPC_E_CHANGED_MODE)"), fresh);
    }
}
