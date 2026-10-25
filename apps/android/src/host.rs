//! The JNI side of the Android host.
//!
//! Kotlin calls the `NativeBridge` functions below on the UI thread; each one
//! only queues a command for the render thread, which owns the engine. The
//! render thread answers through the Kotlin `NativeHost` object it was given
//! (show/hide the keyboard, clipboard, file pickers, opening another page);
//! those callbacks post to the UI thread and never block, so neither thread
//! ever waits on the other except while a surface is being destroyed.
//!
//! `page.navigate(name)`: the render thread asks Kotlin (`onNavigate`) to
//! read `assets/pages/<name>/` off the UI thread; the files come back with
//! `loadPage(.., mode = navigate)` and the render thread loads the page
//! first and only then replaces the current one, so a page that fails to
//! load leaves the current page in place (with a toast saying why). Every
//! page shown is reported back (`onPageShown`) for the back stack and the
//! secrets screen. Platform services (devices, camera, secrets) are in
//! [`crate::bridge`].

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anypage_engine::core::Rect;
use anypage_engine::wgpu::rwh::{AndroidDisplayHandle, AndroidNdkWindowHandle, RawDisplayHandle, RawWindowHandle};
use anypage_engine::{Engine, EngineConfig, HostResult, Page, PageRequest, Target, TextRaster, Wake};
use anypage_runtime::LoadOptions;
use jni::objects::{GlobalRef, JByteArray, JClass, JObject, JObjectArray, JString, JValue};
use jni::sys::{jboolean, jfloat, jint, jlong, JNI_FALSE, JNI_TRUE};
use jni::{JNIEnv, JavaVM};
use ndk::native_window::NativeWindow;

use crate::files;
use crate::keys::KeyPress;
use crate::packages::{self, PageInfo, PageSource};
use crate::requests::{self, FileDialogs, Route};
use crate::view::{self, ScreenRect, Taps};

/// `loadPage` modes.
const LOAD_REPLACE: jint = 0;
const LOAD_NAVIGATE: jint = 1;
/// Agent requests from the debug broadcast, and their answers in the log.
const MAX_ACT: usize = 64 * 1024;
/// Work per frame before the engine defers the rest: one 60 Hz frame
/// (the engine's 10 ms default is a desktop number; phones are slower).
const FRAME_BUDGET_MS: f32 = 16.0;

/// Pointer id for mouse hover (no button held); never a touch id.
const HOVER_ID: u64 = u64::MAX;
/// Upper bound for keys synthesised from one IME delete request.
const MAX_REPEAT: i32 = 256;
/// How far a finger may wander and still tap, in dp (Android's touch slop).
const TAP_SLOP_DP: f32 = 8.0;

enum Cmd {
    Surface(NativeWindow),
    Resize(u32, u32),
    SurfaceGone(Sender<()>),
    /// action: 0 down, 1 move, 2 up, 3 cancel, 4 hover. Physical pixels.
    Touch(i32, u64, f32, f32),
    Wheel(f32, f32, f32, f32),
    Key(KeyPress),
    KeyName(String),
    Text(String),
    Preedit(String),
    Paste(String),
    Dropped(String, Arc<Vec<u8>>),
    /// A picked document to read for the dialog `token` ([`FileDialogs`]).
    OpenFd { token: u64, name: String, fd: i32 },
    /// A picked place to write the bytes of the save dialog `token` to.
    SaveFd { token: u64, name: String, fd: i32 },
    /// A dropped document to read.
    DropFd { name: String, fd: i32 },
    /// The final answer for the dialog `token`.
    Answer(u64, HostResult),
    /// Pixels at the bottom of the surface the soft keyboard covers.
    ImeInset(f32),
    /// Show this page; if it fails, a page that says why.
    Load(PageSource),
    /// Open this page (`page.navigate`, back); if it fails, keep the current one.
    Navigate(PageSource),
    /// A semantic-port request for the page (debug broadcast); the answer is logged.
    Act(String),
    Wake,
    Quit,
}

struct Host {
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
}

// ---------------------------------------------------------------- calls into Kotlin

/// The Kotlin `NativeHost`, called from the render thread.
struct Java {
    vm: JavaVM,
    host: GlobalRef,
}

impl Java {
    fn call(&self, what: &str, f: impl FnOnce(&mut JNIEnv) -> jni::errors::Result<()>) {
        let mut env = match self.vm.attach_current_thread_permanently() {
            Ok(env) => env,
            Err(e) => {
                log::error!("{what}: cannot attach the render thread to the JVM: {e}");
                return;
            }
        };
        let r = env.with_local_frame(16, |env| f(env));
        if let Err(e) = r {
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_describe();
                let _ = env.exception_clear();
            }
            log::warn!("{what}: {e}");
        }
    }

    /// `show`: ask for the keyboard (focus gained, or a tap while focused
    /// after the user dismissed it).
    fn text_focus(&self, focus: Option<ScreenRect>, show: bool) {
        let (on, (x, y, w, h)) = match focus {
            Some(r) => (true, r),
            None => (false, (0.0, 0.0, 0.0, 0.0)),
        };
        self.call("onTextFocus", |env| {
            let args = [JValue::Bool(on as jboolean), JValue::Bool(show as jboolean), JValue::Float(x), JValue::Float(y), JValue::Float(w), JValue::Float(h)];
            env.call_method(&self.host, "onTextFocus", "(ZZFFFF)V", &args).map(drop)
        });
    }

    fn copy(&self, text: &str) {
        self.call("onCopy", |env| {
            let s = env.new_string(text)?;
            env.call_method(&self.host, "onCopy", "(Ljava/lang/String;)V", &[JValue::Object(&s)]).map(drop)
        });
    }

    fn paste(&self) {
        self.call("onPaste", |env| env.call_method(&self.host, "onPaste", "()V", &[]).map(drop));
    }

    /// A short message for the user (a toast): a request the host refused.
    fn notice(&self, message: &str) {
        self.call("onNotice", |env| {
            let s = env.new_string(message)?;
            env.call_method(&self.host, "onNotice", "(Ljava/lang/String;)V", &[JValue::Object(&s)]).map(drop)
        });
    }

    fn open_file(&self, id: u64, filters: &[String]) {
        self.call("onOpenFile", |env| {
            let arr = env.new_object_array(filters.len() as i32, "java/lang/String", JObject::null())?;
            for (i, f) in filters.iter().enumerate() {
                let s = env.new_string(f)?;
                env.set_object_array_element(&arr, i as i32, s)?;
            }
            env.call_method(&self.host, "onOpenFile", "(J[Ljava/lang/String;)V", &[JValue::Long(id as i64), JValue::Object(&arr)]).map(drop)
        });
    }

    fn save_file(&self, id: u64, name: &str, size: usize) {
        self.call("onSaveFile", |env| {
            let s = env.new_string(name)?;
            env.call_method(&self.host, "onSaveFile", "(JLjava/lang/String;J)V", &[JValue::Long(id as i64), JValue::Object(&s), JValue::Long(size as i64)]).map(drop)
        });
    }

    /// Read `assets/pages/<name>/` and answer with `loadPage(.., navigate)`.
    fn navigate(&self, name: &str) {
        self.call("onNavigate", |env| {
            let s = env.new_string(name)?;
            env.call_method(&self.host, "onNavigate", "(Ljava/lang/String;)V", &[JValue::Object(&s)]).map(drop)
        });
    }

    /// The page now shown: its package name (None: the built-in demo), and
    /// what it is (None: it failed and an error page is shown).
    fn page_shown(&self, name: Option<&str>, info: Option<&PageInfo>) {
        self.call("onPageShown", |env| {
            let n = match name {
                Some(n) => JObject::from(env.new_string(n)?),
                None => JObject::null(),
            };
            let empty = PageInfo::default();
            let i = info.unwrap_or(&empty);
            let (id, title) = (env.new_string(&i.id)?, env.new_string(&i.title)?);
            let secrets = env.new_object_array(i.secrets.len() as i32, "java/lang/String", JObject::null())?;
            for (k, s) in i.secrets.iter().enumerate() {
                let js = env.new_string(s)?;
                env.set_object_array_element(&secrets, k as i32, js)?;
            }
            let args = [JValue::Object(&n), JValue::Bool(info.is_some() as jboolean), JValue::Object(&id), JValue::Object(&title), JValue::Object(&secrets)];
            env.call_method(&self.host, "onPageShown", "(Ljava/lang/String;ZLjava/lang/String;Ljava/lang/String;[Ljava/lang/String;)V", &args).map(drop)
        });
    }
}

// ---------------------------------------------------------------- the render thread

fn target_for(win: &NativeWindow) -> Option<Target> {
    let ptr = NonNull::new(win.ptr().as_ptr() as *mut c_void)?;
    Some(Target::Raw {
        display: RawDisplayHandle::Android(AndroidDisplayHandle::new()),
        window: RawWindowHandle::AndroidNdk(AndroidNdkWindowHandle::new(ptr)),
    })
}

struct Render {
    rx: Receiver<Cmd>,
    tx: Sender<Cmd>,
    density: f32,
    hud: bool,
    java: Option<Java>,
    text: Arc<TextRaster>,
    wake: Wake,
    wake_flag: Arc<AtomicBool>,
    /// Set at quit: file reads and writes in flight stop.
    quit: Arc<AtomicBool>,
    engine: Option<Engine<Box<dyn Page>>>,
    /// Keeps the ANativeWindow referenced while the engine draws into it.
    window: Option<NativeWindow>,
    source: PageSource,
    /// Open and save dialogs in flight, tied to the page that asked.
    dialogs: FileDialogs,
    /// What Kotlin was last told about the caret (None = no text focus).
    focus_sent: Option<ScreenRect>,
    /// The page's caret in world units, to tell typing from camera moves.
    focus_world: Option<Rect>,
    ime_inset: f32,
    reveal_pending: bool,
    /// Pointer gestures, to find taps.
    taps: Taps,
    /// The page's caret when the current gesture began.
    focus_at_down: Option<Rect>,
    /// A tap ended since the last focus sync: where it started, and whether
    /// it moved the caret.
    tap: Option<((f32, f32), bool)>,
    /// A caret that could not be followed was reported already.
    warned_caret: bool,
    /// Frames that ran past their budget get a follow-up ([`view::Overrun`]).
    overrun: view::Overrun,
}

impl Render {
    fn run(mut self) {
        loop {
            // block until something can change: input, a wake, or a predicted change
            let first = match self.engine.as_ref().filter(|_| self.window.is_some()).map(|e| e.last()) {
                Some(last) if last.continuous || self.overrun.again() => self.rx.try_recv().ok(),
                Some(last) => match last.wake_at {
                    Some(at) => match self.rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                        Ok(c) => Some(c),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => return,
                    },
                    None => match self.rx.recv() {
                        Ok(c) => Some(c),
                        Err(_) => return,
                    },
                },
                None => match self.rx.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                },
            };
            // handle everything queued, so a burst of touch moves costs one frame
            let mut cmds: Vec<Cmd> = first.into_iter().collect();
            while let Ok(c) = self.rx.try_recv() {
                cmds.push(c);
            }
            for cmd in cmds {
                if !self.handle(cmd) {
                    return;
                }
            }
            self.serve_requests();
            self.sync_focus();
            if let (Some(e), Some(_)) = (self.engine.as_mut(), self.window.as_ref()) {
                let t0 = Instant::now();
                let out = e.frame();
                // a frame past its budget may have left finished tiles unread
                self.overrun.frame(t0.elapsed(), out.rendered, out.continuous);
            }
        }
    }

    /// Options every page loads with: the platform's secret store.
    fn load_options() -> LoadOptions {
        LoadOptions { secrets: crate::bridge::secrets() }
    }

    /// Load the current source (an error page when it fails) and tell Kotlin.
    fn load_current(&self) -> Box<dyn Page> {
        let t0 = Instant::now();
        let loaded = packages::load(&self.source, self.text.clone(), self.wake.clone(), &Self::load_options());
        if let Some(err) = &loaded.error {
            log::error!("page {} failed to load: {err}", self.source.describe());
        }
        log::info!("{} · loaded in {:.1} ms", loaded.summary, t0.elapsed().as_secs_f64() * 1000.0);
        if let Some(java) = self.java.as_ref() {
            java.page_shown(self.source.name(), loaded.info.as_ref());
        }
        loaded.page
    }

    fn create_engine(&self, target: &Target, w: u32, h: u32, page: Box<dyn Page>) -> Option<Engine<Box<dyn Page>>> {
        let t0 = Instant::now();
        let cfg = EngineConfig { tile_workers: 4, tile_budget_mb: 128, show_hud: self.hud, frame_budget_ms: FRAME_BUDGET_MS, ..Default::default() };
        match Engine::new(target, (w, h), self.density, page, self.text.clone(), self.wake.clone(), cfg) {
            Ok(e) => {
                log::info!("{} · engine ready in {:.1} ms", e.summary(), t0.elapsed().as_secs_f64() * 1000.0);
                Some(e)
            }
            Err(err) => {
                log::error!("engine: {err}");
                None
            }
        }
    }

    fn attach(&mut self, win: NativeWindow) {
        let (w, h) = (win.width().max(1) as u32, win.height().max(1) as u32);
        let Some(target) = target_for(&win) else {
            log::error!("surface without a native window");
            return;
        };
        match self.engine.as_mut() {
            Some(e) => {
                if let Err(err) = e.attach_surface(&target, w, h) {
                    log::error!("attach surface: {err}");
                }
            }
            None => {
                let page = self.load_current();
                self.engine = self.create_engine(&target, w, h, page);
            }
        }
        self.window = Some(win);
    }

    /// Forget everything tied to the page being replaced.
    fn retire_page(&mut self) {
        if let Some(mut old) = self.engine.take() {
            old.shutdown();
        }
        // answers for the old page (dialogs still open, reads in flight) are dropped
        if !self.dialogs.is_empty() {
            log::info!("dropping {} file dialogs of the replaced page", self.dialogs.len());
        }
        self.dialogs.clear();
        self.focus_world = None;
        self.taps = Taps::default();
        self.tap = None;
        self.warned_caret = false;
    }

    /// Run `page` on the current surface (if there is one yet).
    fn show(&mut self, page: Box<dyn Page>) {
        let Some(win) = self.window.as_ref() else { return };
        let (w, h) = (win.width().max(1) as u32, win.height().max(1) as u32);
        if let Some(target) = target_for(win) {
            self.engine = self.create_engine(&target, w, h, page);
        }
    }

    /// Replace the page (a new intent asked for another package).
    fn reload(&mut self, source: PageSource) {
        self.retire_page();
        self.source = source;
        log::info!("loading {}", self.source.describe());
        // without a surface the page loads when one arrives (attach)
        if self.window.is_some() {
            let page = self.load_current();
            self.show(page);
        }
    }

    /// `page.navigate` (or back): load the new page first; only a page that
    /// loads replaces the current one.
    fn navigate(&mut self, source: PageSource) {
        let t0 = Instant::now();
        let what = source.describe();
        let shown = source.name().unwrap_or("demo").to_string();
        log::info!("navigating to {what}");
        let natives = packages::natives();
        match packages::load_page(&source, &natives, self.text.clone(), self.wake.clone(), &Self::load_options()) {
            Ok(page) => {
                log::info!("{} · loaded in {:.1} ms", page.summary(), t0.elapsed().as_secs_f64() * 1000.0);
                let info = PageInfo::of(&page);
                self.retire_page();
                self.source = source;
                if let Some(java) = self.java.as_ref() {
                    java.page_shown(self.source.name(), Some(&info));
                }
                if self.window.is_some() {
                    self.show(Box::new(page));
                }
                // without a surface: the page loads again when one arrives
            }
            Err(e) => {
                log::error!("could not open {what}: {e}");
                let missing = matches!(&source, PageSource::Assets { files, .. } if files.is_empty());
                let why = if missing { "這個 App 沒有這個頁面（no such page in this app）".to_string() } else { requests::short_message(&e, 300) };
                if let Some(java) = self.java.as_ref() {
                    // a toast shows two lines: name the page, then why
                    java.notice(&format!("無法開啟頁面 {shown}：{why}"));
                }
            }
        }
    }

    fn spawn_file(&self, what: &'static str, fd: i32, job: impl FnOnce(&AtomicBool) -> Option<Cmd> + Send + 'static) {
        let (tx, quit) = (self.tx.clone(), self.quit.clone());
        let spawned = std::thread::Builder::new().name("anypage-file".into()).spawn(move || {
            if let Some(cmd) = job(&quit) {
                let _ = tx.send(cmd);
            }
        });
        if let Err(e) = spawned {
            log::error!("{what}: no thread for file work: {e}");
            // SAFETY: the job never ran, so the descriptor is still ours to close.
            drop(unsafe { files::fd::take(fd) });
        }
    }

    /// Handle one command; false when the host is quitting.
    fn handle(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Surface(win) => self.attach(win),
            Cmd::SurfaceGone(ack) => {
                if let Some(e) = self.engine.as_mut() {
                    e.detach_surface();
                }
                self.window = None;
                let _ = ack.send(());
            }
            Cmd::Wake => self.wake_flag.store(false, Ordering::Release),
            Cmd::Quit => {
                self.quit.store(true, Ordering::Relaxed);
                if let Some(e) = self.engine.as_mut() {
                    e.shutdown();
                }
                return false;
            }
            Cmd::Load(source) => self.reload(source),
            Cmd::Navigate(source) => self.navigate(source),
            Cmd::Act(request) => match self.engine.as_mut() {
                Some(e) => {
                    let answer = e.page_mut().act(&request);
                    e.mark_op();
                    log::info!("act {} -> {}", requests::short_message(&request, 2000), requests::short_message(&answer, 8000));
                }
                None => log::warn!("act {}: no page is running", requests::short_message(&request, 200)),
            },
            Cmd::ImeInset(px) => {
                if (px - self.ime_inset).abs() >= 1.0 {
                    log::info!("keyboard covers {px:.0} px");
                    self.ime_inset = px;
                    self.reveal_pending = true;
                }
            }
            // file work closes its descriptor even when no page is there to answer
            Cmd::OpenFd { token, name, fd } => match self.dialogs.picked(token) {
                Some(p) => {
                    log::info!("open #{}: reading {name}", p.id);
                    self.spawn_file("open", fd, move |quit| {
                        // SAFETY: Kotlin detached `fd` from its ParcelFileDescriptor for us.
                        Some(Cmd::Answer(token, unsafe { files::fd::read(fd, &name, quit) }))
                    });
                }
                None => {
                    log::warn!("open dialog {token}: the page that asked is gone; not reading {name}");
                    // SAFETY: as above; nothing will read it.
                    drop(unsafe { files::fd::take(fd) });
                }
            },
            Cmd::DropFd { name, fd } => {
                log::info!("drop: reading {name}");
                self.spawn_file("drop", fd, move |quit| {
                    // SAFETY: as above.
                    match unsafe { files::fd::read(fd, &name, quit) } {
                        HostResult::File { name, bytes } => Some(Cmd::Dropped(name, bytes)),
                        other => {
                            log::warn!("dropped file: {other:?}");
                            None
                        }
                    }
                });
            }
            Cmd::SaveFd { token, name, fd } => match self.dialogs.picked(token) {
                Some(requests::Picked { id, save: Some(bytes) }) => {
                    log::info!("save #{id}: writing {} bytes to {name}", bytes.len());
                    self.spawn_file("save", fd, move |quit| {
                        // SAFETY: as above.
                        Some(Cmd::Answer(token, unsafe { files::fd::write(fd, &name, &bytes, quit) }))
                    });
                }
                other => {
                    if other.is_some() {
                        // an open dialog answered as a save: nothing to write
                        self.handle(Cmd::Answer(token, HostResult::Failed(format!("{name}: no data to save"))));
                    }
                    log::warn!("save dialog {token}: no pending data (the page was replaced?)");
                    // SAFETY: as above; nothing will write to it.
                    drop(unsafe { files::fd::take(fd) });
                }
            },
            Cmd::Answer(token, result) => match self.dialogs.finish(token) {
                Some(id) => {
                    log::info!("file request #{id} answered: {}", describe(&result));
                    if let Some(e) = self.engine.as_mut() {
                        e.host_result(id, result);
                    }
                }
                None => log::info!("file dialog {token} answered after its page was replaced: dropped ({})", describe(&result)),
            },
            input => {
                let Some(e) = self.engine.as_mut() else { return true };
                match input {
                    Cmd::Resize(w, h) => e.resize(w, h),
                    Cmd::Touch(action, id, x, y) => match action {
                        0 => {
                            if self.taps.idle() {
                                self.focus_at_down = e.page().text_focus();
                            }
                            self.taps.down(id, x, y);
                            e.pointer_down(id, x, y);
                        }
                        1 => {
                            self.taps.moved(id, x, y, TAP_SLOP_DP * self.density);
                            e.pointer_move(id, x, y);
                        }
                        2 => {
                            e.pointer_up(id);
                            // a tap that gave the page text focus showed the keyboard already
                            if let (Some(at), Some(before)) = (self.taps.up(id), self.focus_at_down) {
                                self.tap = Some((at, e.page().text_focus() != Some(before)));
                            }
                        }
                        4 => e.pointer_move(HOVER_ID, x, y),
                        _ => {
                            self.taps.cancel();
                            e.pointer_cancel();
                        }
                    },
                    Cmd::Wheel(x, y, dx, dy) => e.wheel(x, y, dx, dy),
                    Cmd::Key(k) => {
                        let editing = e.text_focus().is_some();
                        match (k.typed, k.name) {
                            (Some(t), _) if editing => e.text_input(&t),
                            (_, Some(name)) => {
                                e.key(&name);
                            }
                            _ => {}
                        }
                    }
                    Cmd::KeyName(name) => {
                        e.key(&name);
                    }
                    Cmd::Text(t) => e.text_input(&t),
                    Cmd::Preedit(t) => e.ime_preedit(&t),
                    Cmd::Paste(t) => e.paste(&t),
                    Cmd::Dropped(name, bytes) => {
                        log::info!("drop: {name} ({} bytes) to the page", bytes.len());
                        e.file_drop(&name, bytes);
                    }
                    _ => {}
                }
            }
        }
        true
    }

    /// Serve what the page asked of the platform.
    fn serve_requests(&mut self) {
        let Some(e) = self.engine.as_mut() else { return };
        while let Some(req) = e.take_host_request() {
            let Some(java) = self.java.as_ref() else {
                // no Kotlin host: answer what needs an answer
                match req {
                    PageRequest::OpenFile { id, .. } | PageRequest::SaveFile { id, .. } => e.host_result(id, HostResult::Failed("this app has no file picker".into())),
                    PageRequest::Paste => e.paste(""),
                    _ => {}
                }
                continue;
            };
            // everything that goes to Java is size-checked first: the clipboard,
            // a document name and MIME filters travel in binder transactions
            match requests::route(req, &mut self.dialogs) {
                Route::Copy(text) => {
                    log::info!("copy: {} bytes to the clipboard", text.len());
                    java.copy(&text);
                }
                Route::Paste => {
                    log::info!("paste: reading the clipboard");
                    java.paste();
                }
                Route::Open { token, filters } => {
                    log::info!("open dialog {token}: picker for {} filters", filters.len());
                    java.open_file(token, &filters);
                }
                Route::Save { token, name, size } => {
                    log::info!("save dialog {token}: picker for a {}-byte name ({size} bytes of data)", name.len());
                    java.save_file(token, &name, size);
                }
                Route::Notice(msg) => {
                    log::warn!("{msg}");
                    java.notice(&msg);
                }
                Route::Answer(id, result) => {
                    log::warn!("file request #{id} refused: {}", describe(&result));
                    e.host_result(id, result);
                }
                Route::Navigate(name) => {
                    log::info!("page.navigate(\"{name}\"): reading the package");
                    java.navigate(&name);
                }
                Route::Engine => {}
            }
        }
    }

    /// Keep the caret visible above the keyboard, and tell Kotlin when text
    /// focus starts, moves or ends.
    fn sync_focus(&mut self) {
        let Some(e) = self.engine.as_mut() else { return };
        let world = e.page().text_focus();
        if let Some(caret_world) = world.filter(|w| Some(*w) != self.focus_world || self.reveal_pending) {
            // a caret with no usable position (1e39 overflows f32) or far off
            // the page must not take the camera with it
            if !view::revealable(caret_world, e.page().spec().bounds) {
                if !std::mem::replace(&mut self.warned_caret, true) {
                    log::warn!("the page's caret {caret_world:?} is not finite or far outside the page; not following it");
                }
            } else if let Some(caret) = e.text_focus() {
                let (w, h) = e.surface_size();
                let (dx, dy) = view::reveal(caret, w as f32, h as f32, self.ime_inset, 24.0 * self.density);
                if dx != 0.0 || dy != 0.0 {
                    e.camera_mut().pan(dx, dy);
                }
            }
        }
        self.reveal_pending = false;
        self.focus_world = world;
        let screen = e.text_focus().map(view::sanitize);
        let gained = screen.is_some() && self.focus_sent.is_none();
        // a dismissed keyboard comes back on a tap on the field (or one that
        // moved the caret), not after a pan, drag, pinch or a tap elsewhere
        let reopen = match (screen, self.tap.take()) {
            (Some(caret), Some((at, moved))) => view::tap_reopens_keyboard(at, caret, moved),
            _ => false,
        };
        if view::focus_changed(self.focus_sent, screen) || reopen {
            if screen.is_some() != self.focus_sent.is_some() {
                log::info!("text focus {}", if screen.is_some() { "on: showing the keyboard" } else { "off: hiding the keyboard" });
            }
            self.focus_sent = screen;
            if let Some(java) = self.java.as_ref() {
                java.text_focus(screen, gained || reopen);
            }
        }
    }
}

fn describe(r: &HostResult) -> String {
    match r {
        HostResult::File { name, bytes } => format!("file {name} ({} bytes)", bytes.len()),
        HostResult::Saved { name } => format!("saved {name}"),
        HostResult::Cancelled => "cancelled".into(),
        HostResult::Failed(e) => format!("failed: {e}"),
    }
}

// ---------------------------------------------------------------- JNI helpers

pub(crate) fn init_logging() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("AnyPage"));
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            log::error!("panic: {info}");
            default(info);
        }));
    });
}

/// Run `f`, turning a panic into `fallback` so it never unwinds into the JVM.
pub(crate) fn guard<T>(what: &str, fallback: T, f: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        log::error!("{what}: panicked");
        fallback
    })
}

fn host<'a>(handle: jlong) -> Option<&'a Host> {
    // SAFETY: `handle` is the pointer returned by `create` and not yet
    // destroyed; Kotlin zeroes its copy before calling `destroy`.
    unsafe { (handle as *const Host).as_ref() }
}

fn send(handle: jlong, cmd: Cmd) {
    if let Some(h) = host(handle) {
        let _ = h.tx.send(cmd);
    }
}

fn string_arg(env: &mut JNIEnv, s: &JString) -> Option<String> {
    if s.is_null() {
        return None;
    }
    match env.get_string(s) {
        Ok(js) => Some(js.into()),
        Err(e) => {
            log::warn!("string argument: {e}");
            None
        }
    }
}

/// The (name, bytes) pairs Kotlin read from `assets/pages/<name>/`.
fn package_files(env: &mut JNIEnv, names: &JObjectArray, contents: &JObjectArray) -> jni::errors::Result<Vec<(String, Vec<u8>)>> {
    if names.is_null() || contents.is_null() {
        return Ok(Vec::new());
    }
    let n = env.get_array_length(names)?.min(env.get_array_length(contents)?);
    let mut files = Vec::with_capacity(n.max(0) as usize);
    for i in 0..n {
        let file = env.with_local_frame(4, |env| -> jni::errors::Result<(String, Vec<u8>)> {
            let name = JString::from(env.get_object_array_element(names, i)?);
            let data = JByteArray::from(env.get_object_array_element(contents, i)?);
            let name: String = env.get_string(&name)?.into();
            Ok((name, env.convert_byte_array(&data)?))
        })?;
        files.push(file);
    }
    Ok(files)
}

fn page_source(env: &mut JNIEnv, page: &JString, names: &JObjectArray, contents: &JObjectArray) -> PageSource {
    match string_arg(env, page).filter(|n| !n.is_empty()) {
        None => PageSource::Builtin,
        Some(name) => {
            let files = package_files(env, names, contents).unwrap_or_else(|e| {
                log::error!("page package {name}: {e}");
                Vec::new()
            });
            PageSource::Assets { name, files }
        }
    }
}

// ---------------------------------------------------------------- JNI exports

/// Starts the render thread. `host` is the Kotlin `NativeHost`; `page` the
/// asset package name (null: the built-in demo) with its files.
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_create<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    density: jfloat,
    host: JObject<'l>,
    page: JString<'l>,
    names: JObjectArray<'l>,
    contents: JObjectArray<'l>,
    hud: jboolean,
) -> jlong {
    init_logging();
    guard("create", 0, || {
        let java = match env.get_java_vm() {
            Ok(vm) if !host.is_null() => match env.new_global_ref(&host) {
                Ok(host) => Some(Java { vm, host }),
                Err(e) => {
                    log::error!("host object: {e}");
                    None
                }
            },
            Ok(_) => None,
            Err(e) => {
                log::error!("JavaVM: {e}");
                None
            }
        };
        let source = page_source(&mut env, &page, &names, &contents);
        log::info!("starting with {}", source.describe());
        let (tx, rx) = channel();
        let render_tx = tx.clone();
        let hud = hud != JNI_FALSE;
        let thread = std::thread::Builder::new().name("anypage-render".into()).spawn(move || {
            let wake_flag = Arc::new(AtomicBool::new(false));
            let wake: Wake = {
                let flag = wake_flag.clone();
                let tx = Mutex::new(render_tx.clone());
                Arc::new(move || {
                    if !flag.swap(true, Ordering::AcqRel) {
                        let _ = tx.lock().unwrap_or_else(|p| p.into_inner()).send(Cmd::Wake);
                    }
                })
            };
            let text = match TextRaster::load_with(anypage_engine::TextLoadOptions { locale: Some("zh-TW".into()), ..Default::default() }) {
                Ok(t) => Arc::new(t),
                Err(e) => {
                    log::error!("font: {e}");
                    return;
                }
            };
            let render = Render {
                rx,
                tx: render_tx,
                density,
                hud,
                java,
                text,
                wake,
                wake_flag,
                quit: Arc::new(AtomicBool::new(false)),
                engine: None,
                window: None,
                source,
                dialogs: FileDialogs::default(),
                focus_sent: None,
                focus_world: None,
                ime_inset: 0.0,
                reveal_pending: false,
                taps: Taps::default(),
                focus_at_down: None,
                tap: None,
                warned_caret: false,
                overrun: view::Overrun::new(FRAME_BUDGET_MS),
            };
            if std::panic::catch_unwind(AssertUnwindSafe(|| render.run())).is_err() {
                log::error!("the render thread stopped after a panic");
            }
        });
        match thread {
            Ok(t) => Box::into_raw(Box::new(Host { tx, thread: Some(t) })) as jlong,
            Err(e) => {
                log::error!("render thread: {e}");
                0
            }
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_surfaceCreated(env: JNIEnv, _class: JClass, handle: jlong, surface: JObject) {
    guard("surfaceCreated", (), || {
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
    })
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_surfaceChanged(_env: JNIEnv, _class: JClass, handle: jlong, w: jint, h: jint) {
    guard("surfaceChanged", (), || send(handle, Cmd::Resize(w.max(1) as u32, h.max(1) as u32)));
}

/// Blocks until the render thread has let go of the surface, as Android requires.
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_surfaceDestroyed(_env: JNIEnv, _class: JClass, handle: jlong) {
    guard("surfaceDestroyed", (), || {
        if let Some(h) = host(handle) {
            let (ack_tx, ack_rx) = channel();
            if h.tx.send(Cmd::SurfaceGone(ack_tx)).is_ok() {
                let _ = ack_rx.recv_timeout(Duration::from_secs(2));
            }
        }
    })
}

/// action: 0 down, 1 move, 2 up, 3 cancel, 4 hover (mouse, no button). Physical pixels.
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_touch(_env: JNIEnv, _class: JClass, handle: jlong, action: jint, id: jint, x: jfloat, y: jfloat) {
    guard("touch", (), || {
        if x.is_finite() && y.is_finite() {
            send(handle, Cmd::Touch(action, id as u64, x, y));
        }
    })
}

/// Mouse wheel or trackpad scroll in lines; positive `dy` = wheel up.
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_wheel(_env: JNIEnv, _class: JClass, handle: jlong, x: jfloat, y: jfloat, dx: jfloat, dy: jfloat) {
    guard("wheel", (), || {
        if [x, y, dx, dy].iter().all(|v| v.is_finite()) {
            send(handle, Cmd::Wheel(x, y, dx, dy));
        }
    })
}

/// A key press: the key code, meta state, the character the key types
/// without Ctrl/Alt/Meta, and (right Alt held, no Ctrl/Meta) the character
/// of the layout's Alt level, AltGr ([`KeyPress::from_android`]). Returns whether
/// the engine takes the key (Kotlin then consumes it; otherwise Android
/// handles it: Back, volume, ...).
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_key(_env: JNIEnv, _class: JClass, handle: jlong, code: jint, meta: jint, plain: jint, alt: jint) -> jboolean {
    guard("key", JNI_FALSE, || match KeyPress::from_android(code, meta, plain, alt) {
        Some(k) => {
            send(handle, Cmd::Key(k));
            JNI_TRUE
        }
        None => JNI_FALSE,
    })
}

/// A key by its engine name ("backspace", "enter", "ctrl+v"), repeated `count`
/// times: what the input connection turns IME edit commands into.
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_keyName(mut env: JNIEnv, _class: JClass, handle: jlong, name: JString, count: jint) {
    guard("keyName", (), || {
        if let Some(name) = string_arg(&mut env, &name).filter(|n| !n.is_empty()) {
            for _ in 0..count.clamp(0, MAX_REPEAT) {
                send(handle, Cmd::KeyName(name.clone()));
            }
        }
    })
}

/// Committed text (IME commit, typed characters).
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_text(mut env: JNIEnv, _class: JClass, handle: jlong, text: JString) {
    guard("text", (), || {
        if let Some(t) = string_arg(&mut env, &text).filter(|t| !t.is_empty()) {
            send(handle, Cmd::Text(t));
        }
    })
}

/// The IME's composition in progress; empty when it ends.
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_preedit(mut env: JNIEnv, _class: JClass, handle: jlong, text: JString) {
    guard("preedit", (), || {
        let t = string_arg(&mut env, &text).unwrap_or_default();
        send(handle, Cmd::Preedit(t));
    })
}

/// Clipboard text the page asked for.
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_paste(mut env: JNIEnv, _class: JClass, handle: jlong, text: JString) {
    guard("paste", (), || {
        let t = string_arg(&mut env, &text).unwrap_or_default();
        send(handle, Cmd::Paste(t));
    })
}

/// The user picked a document for open dialog `token` (the id Kotlin was
/// given); `fd` is detached and now ours.
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_fileOpened(mut env: JNIEnv, _class: JClass, handle: jlong, token: jlong, name: JString, fd: jint) {
    guard("fileOpened", (), || {
        let name = string_arg(&mut env, &name).unwrap_or_else(|| "file".into());
        send(handle, Cmd::OpenFd { token: token as u64, name, fd });
    })
}

/// The user picked where to save for dialog `token`; `fd` is detached and now ours.
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_fileSaveTo(mut env: JNIEnv, _class: JClass, handle: jlong, token: jlong, name: JString, fd: jint) {
    guard("fileSaveTo", (), || {
        let name = string_arg(&mut env, &name).unwrap_or_else(|| "file".into());
        send(handle, Cmd::SaveFd { token: token as u64, name, fd });
    })
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_fileCancelled(_env: JNIEnv, _class: JClass, handle: jlong, token: jlong) {
    guard("fileCancelled", (), || send(handle, Cmd::Answer(token as u64, HostResult::Cancelled)));
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_fileFailed(mut env: JNIEnv, _class: JClass, handle: jlong, token: jlong, message: JString) {
    guard("fileFailed", (), || {
        let m = string_arg(&mut env, &message).unwrap_or_else(|| "the file could not be opened".into());
        send(handle, Cmd::Answer(token as u64, HostResult::Failed(m)));
    })
}

/// A document dropped on the view; `fd` is detached and now ours.
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_fileDropped(mut env: JNIEnv, _class: JClass, handle: jlong, name: JString, fd: jint) {
    guard("fileDropped", (), || {
        let name = string_arg(&mut env, &name).unwrap_or_else(|| "file".into());
        send(handle, Cmd::DropFd { name, fd });
    })
}

/// Pixels at the bottom of the view the soft keyboard covers (0 when hidden).
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_imeInset(_env: JNIEnv, _class: JClass, handle: jlong, bottom: jfloat) {
    guard("imeInset", (), || {
        if bottom.is_finite() {
            send(handle, Cmd::ImeInset(bottom.max(0.0)));
        }
    })
}

/// Show another page package (or the built-in demo when `page` is null).
/// `mode` 0: replace it (an intent; a page that fails shows why); 1:
/// navigate (`page.navigate`, back; a page that fails leaves the current one).
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_loadPage(mut env: JNIEnv, _class: JClass, handle: jlong, page: JString, names: JObjectArray, contents: JObjectArray, mode: jint) {
    guard("loadPage", (), || {
        let source = page_source(&mut env, &page, &names, &contents);
        match mode {
            LOAD_NAVIGATE => send(handle, Cmd::Navigate(source)),
            LOAD_REPLACE => send(handle, Cmd::Load(source)),
            other => log::warn!("loadPage: unknown mode {other}"),
        }
    })
}

/// A semantic-port request for the page (`{"describe": true}`, `{"action":
/// "x", "params": {}}`, `{"get": "cell"}`); the answer goes to the log.
/// Debug builds call it from their shell-only broadcast.
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_act(mut env: JNIEnv, _class: JClass, handle: jlong, request: JString) {
    guard("act", (), || {
        match string_arg(&mut env, &request) {
            Some(r) if r.len() <= MAX_ACT => send(handle, Cmd::Act(r)),
            Some(r) => log::warn!("act: {} bytes, at most {MAX_ACT}", r.len()),
            None => {}
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_destroy(_env: JNIEnv, _class: JClass, handle: jlong) {
    if handle == 0 {
        return;
    }
    // SAFETY: created by `create`; Kotlin calls destroy exactly once.
    let mut h = unsafe { Box::from_raw(handle as *mut Host) };
    guard("destroy", (), move || {
        let _ = h.tx.send(Cmd::Quit);
        if let Some(t) = h.thread.take() {
            let _ = t.join();
        }
    })
}
