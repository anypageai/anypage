//! Android host glue (docs/ARCHITECTURE.md section 14.4, "engine as library").
//!
//! The Kotlin app owns the window, lifecycle and (later) text input. It hands
//! this library a `Surface` and forwards touches. The engine runs on its own
//! render thread, which blocks until there is input, a wake from another
//! thread (stream data, finished tiles or jobs), or the next predicted change.
#![cfg(target_os = "android")]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anypage_demo::DemoPage;
use anypage_engine::wgpu::rwh::{AndroidDisplayHandle, AndroidNdkWindowHandle, RawDisplayHandle, RawWindowHandle};
use anypage_engine::{Engine, EngineConfig, Target, TextRaster, Wake};
use jni::objects::{JClass, JObject};
use jni::sys::{jfloat, jint, jlong};
use jni::JNIEnv;
use ndk::native_window::NativeWindow;

enum Cmd {
    Surface(NativeWindow),
    Resize(u32, u32),
    SurfaceGone(Sender<()>),
    Touch(i32, u64, f32, f32),
    Wake,
    Quit,
}

struct Host {
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
}

fn target_for(win: &NativeWindow) -> Target {
    let ptr = NonNull::new(win.ptr().as_ptr() as *mut c_void).expect("ANativeWindow");
    Target::Raw {
        display: RawDisplayHandle::Android(AndroidDisplayHandle::new()),
        window: RawWindowHandle::AndroidNdk(AndroidNdkWindowHandle::new(ptr)),
    }
}

fn render_loop(rx: Receiver<Cmd>, tx: Sender<Cmd>, density: f32) {
    let wake_flag = Arc::new(AtomicBool::new(false));
    let wake: Wake = {
        let flag = wake_flag.clone();
        let tx = Mutex::new(tx);
        Arc::new(move || {
            if !flag.swap(true, Ordering::AcqRel) {
                let _ = tx.lock().unwrap().send(Cmd::Wake);
            }
        })
    };
    let text = match TextRaster::load() {
        Ok(t) => Arc::new(t),
        Err(e) => {
            log::error!("font: {e}");
            return;
        }
    };
    let mut engine: Option<Engine<DemoPage>> = None;
    // keeps the ANativeWindow referenced while the engine draws into it
    let mut window: Option<NativeWindow> = None;

    loop {
        // block until something can change: input, a wake, or a predicted change
        let first = match engine.as_ref().filter(|_| window.is_some()).map(|e| e.last()) {
            Some(last) if last.continuous => rx.try_recv().ok(),
            Some(last) => match last.wake_at {
                Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match rx.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                },
            },
            None => match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            },
        };
        // handle everything queued, so a burst of touch moves costs one frame
        let mut cmds: Vec<Cmd> = first.into_iter().collect();
        while let Ok(c) = rx.try_recv() {
            cmds.push(c);
        }
        for cmd in cmds {
            match cmd {
                Cmd::Surface(win) => {
                    let (w, h) = (win.width().max(1) as u32, win.height().max(1) as u32);
                    let target = target_for(&win);
                    match engine.as_mut() {
                        Some(e) => {
                            if let Err(err) = e.attach_surface(&target, w, h) {
                                log::error!("attach surface: {err}");
                            }
                        }
                        None => {
                            let page = DemoPage::new(text.clone(), wake.clone());
                            let cfg = EngineConfig { tile_workers: 4, tile_budget_mb: 128, ..Default::default() };
                            match Engine::new(&target, (w, h), density, page, text.clone(), wake.clone(), cfg) {
                                Ok(e) => {
                                    log::info!("{}", e.summary());
                                    engine = Some(e);
                                }
                                Err(err) => log::error!("engine: {err}"),
                            }
                        }
                    }
                    window = Some(win);
                }
                Cmd::Resize(w, h) => {
                    if let Some(e) = engine.as_mut() {
                        e.resize(w, h);
                    }
                }
                Cmd::SurfaceGone(ack) => {
                    if let Some(e) = engine.as_mut() {
                        e.detach_surface();
                    }
                    window = None;
                    let _ = ack.send(());
                }
                Cmd::Touch(action, id, x, y) => {
                    if let Some(e) = engine.as_mut() {
                        match action {
                            0 => e.pointer_down(id, x, y),
                            1 => e.pointer_move(id, x, y),
                            2 => e.pointer_up(id),
                            _ => e.pointer_cancel(),
                        }
                    }
                }
                Cmd::Wake => wake_flag.store(false, Ordering::Release),
                Cmd::Quit => {
                    if let Some(e) = engine.as_mut() {
                        e.shutdown();
                    }
                    return;
                }
            }
        }
        if let (Some(e), Some(_)) = (engine.as_mut(), window.as_ref()) {
            e.frame();
        }
    }
}

fn host<'a>(handle: jlong) -> Option<&'a Host> {
    // SAFETY: `handle` is the pointer returned by `create` and not yet destroyed.
    unsafe { (handle as *const Host).as_ref() }
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_create(_env: JNIEnv, _class: JClass, density: jfloat) -> jlong {
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("AnyPage"));
    let (tx, rx) = channel();
    let wake_tx = tx.clone();
    let thread = std::thread::Builder::new()
        .name("anypage-render".into())
        .spawn(move || render_loop(rx, wake_tx, density))
        .expect("spawn render thread");
    Box::into_raw(Box::new(Host { tx, thread: Some(thread) })) as jlong
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_surfaceCreated(env: JNIEnv, _class: JClass, handle: jlong, surface: JObject) {
    let Some(h) = host(handle) else { return };
    // SAFETY: valid JNIEnv and Surface for the duration of this call; the
    // returned NativeWindow holds its own reference.
    let win = unsafe { NativeWindow::from_surface(env.get_raw() as *mut _, surface.as_raw() as *mut _) };
    match win {
        Some(w) => {
            let _ = h.tx.send(Cmd::Surface(w));
        }
        None => log::error!("ANativeWindow_fromSurface failed"),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_surfaceChanged(_env: JNIEnv, _class: JClass, handle: jlong, w: jint, h: jint) {
    if let Some(hst) = host(handle) {
        let _ = hst.tx.send(Cmd::Resize(w.max(1) as u32, h.max(1) as u32));
    }
}

/// Blocks until the render thread has let go of the surface, as Android requires.
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_surfaceDestroyed(_env: JNIEnv, _class: JClass, handle: jlong) {
    if let Some(h) = host(handle) {
        let (ack_tx, ack_rx) = channel();
        if h.tx.send(Cmd::SurfaceGone(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

/// action: 0 down, 1 move, 2 up, 3 cancel. Coordinates in physical pixels.
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_touch(_env: JNIEnv, _class: JClass, handle: jlong, action: jint, id: jint, x: jfloat, y: jfloat) {
    if let Some(h) = host(handle) {
        let _ = h.tx.send(Cmd::Touch(action, id as u64, x, y));
    }
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_destroy(_env: JNIEnv, _class: JClass, handle: jlong) {
    if handle == 0 {
        return;
    }
    // SAFETY: created by `create`; Kotlin calls destroy exactly once.
    let mut h = unsafe { Box::from_raw(handle as *mut Host) };
    let _ = h.tx.send(Cmd::Quit);
    if let Some(t) = h.thread.take() {
        let _ = t.join();
    }
}
