//! Desktop host for the AnyPage engine (docs/ARCHITECTURE.md section 14.4,
//! "engine as app"): winit gives a window and events, the engine does the
//! rest. The host also serves what only the platform can: the clipboard,
//! file dialogs, dropped files, IME placement, and a local agent port.
//!
//!   anypage                          the built-in demo package
//!   anypage --page pages/ui          a page package folder
//!   anypage --rust-demo              the demo page built in Rust
//!   anypage --bench                  scripted 22 s run, writes bench-results/
//!   anypage --agent-port 7700        accept agent requests on 127.0.0.1:7700
//!   anypage --pages <dir>            where page.navigate finds packages (default: next to --page, or ./pages)
//!   anypage --hud                    show the engine's measurement panel (h toggles it)
//!   anypage --no-vsync
//!
//! Alt+Left or the mouse back button returns to the page `page.navigate` left.

mod agent;
mod keys;
mod modules;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anypage_demo::bench::Bench;
use anypage_demo::DemoPage;
use anypage_engine::{Engine, EngineConfig, HostResult, Page, PageRequest, Target, TextLoadOptions, TextRaster};
use anypage_runtime::{LoadOptions, Package, ScriptPage};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

const TITLE: &str = "AnyPage";
/// Dropped and opened files larger than this are refused.
const MAX_FILE: u64 = 1 << 30;

#[derive(Debug, Clone, Copy)]
enum UserEvent {
    Wake,
}

/// Which page the window shows.
#[derive(Clone)]
enum PageChoice {
    Builtin,
    Dir(std::path::PathBuf),
    Rust,
}

/// Warnings from everything, info from the engine and from page scripts (page.log, script errors).
struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        // fontdb warns about every malformed system font it skips: noise
        !m.target().starts_with("fontdb") && (m.level() <= log::Level::Warn || (m.level() <= log::Level::Info && m.target().starts_with("anypage")))
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{}] {}: {}", r.level(), r.target(), r.args());
        }
    }
    fn flush(&self) {}
}

static LOGGER: StderrLog = StderrLog;

struct App {
    bench_mode: bool,
    vsync: bool,
    proxy: Arc<Mutex<EventLoopProxy<UserEvent>>>,
    wake_flag: Arc<AtomicBool>,
    window: Option<Arc<Window>>,
    engine: Option<Engine<Box<dyn Page>>>,
    page_choice: PageChoice,
    page_desc: String,
    bench: Option<Bench>,
    cursor: (f32, f32),
    mods: ModifiersState,
    idle_title_secs: u64,
    /// We asked for the next redraw. Any other redraw comes from the OS
    /// (expose, uncover, capture) and must present, or Windows keeps resending WM_PAINT.
    we_requested: bool,
    /// IME state last given to the window (allowed, caret rectangle).
    ime: Option<(f32, f32, f32, f32)>,
    clipboard: Option<arboard::Clipboard>,
    agent_port: Option<u16>,
    agent_calls: Option<Receiver<agent::AgentCall>>,
    text: Option<Arc<TextRaster>>,
    /// Where `page.navigate(name)` looks for `<name>/page.json`.
    pages_root: Option<std::path::PathBuf>,
    pending_nav: Option<String>,
    /// Pages left by `page.navigate`, for Alt+Left / the mouse back button.
    history: Vec<PageChoice>,
    hud: bool,
}

impl App {
    fn wake(&self) -> anypage_engine::Wake {
        let flag = self.wake_flag.clone();
        let proxy = self.proxy.clone();
        Arc::new(move || {
            if !flag.swap(true, Ordering::AcqRel) {
                let _ = proxy.lock().unwrap().send_event(UserEvent::Wake);
            }
        })
    }

    fn redraw(&mut self) {
        if let Some(w) = &self.window {
            self.we_requested = true;
            w.request_redraw();
        }
    }

    fn finish_bench(&mut self, el: &ActiveEventLoop) {
        let (Some(b), Some(e)) = (&self.bench, &mut self.engine) else { return };
        let report = b.report(e, &self.page_desc);
        println!("\n{report}");
        let _ = std::fs::create_dir_all("bench-results");
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let path = format!("bench-results/engine-{stamp}.md");
        if std::fs::write(&path, &report).is_ok() {
            println!("written to {path}");
        }
        e.shutdown();
        el.exit();
    }

    fn load_page(&self, text: &Arc<TextRaster>, wake: anypage_engine::Wake) -> Result<Box<dyn Page>, String> {
        let opts = LoadOptions { secrets: Some(Arc::new(modules::DesktopSecrets::new())) };
        let natives = modules::natives(text);
        let pkg = match &self.page_choice {
            PageChoice::Rust => return Ok(Box::new(DemoPage::new(text.clone(), wake))),
            PageChoice::Builtin => anypage_demo::script::package(),
            PageChoice::Dir(d) => Package::from_dir(d)?,
        };
        let p = ScriptPage::load_with(&pkg, &natives, text.clone(), wake, opts)?;
        println!("{}", p.summary());
        Ok(Box::new(p))
    }

    /// Serve what the page asked of the platform, and keep the IME in step
    /// with the page's text focus.
    fn serve_page(&mut self) {
        let Some(e) = self.engine.as_mut() else { return };
        while let Some(req) = e.take_host_request() {
            match req {
                PageRequest::Copy(text) => {
                    if let Some(cb) = self.clipboard.as_mut() {
                        if let Err(err) = cb.set_text(text) {
                            log::warn!("clipboard: {err}");
                        }
                    }
                }
                PageRequest::Paste => {
                    let text = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()).unwrap_or_default();
                    e.paste(&text);
                }
                PageRequest::OpenFile { id, filters } => {
                    let mut dlg = rfd::FileDialog::new();
                    if !filters.is_empty() {
                        dlg = dlg.add_filter("files", &filters);
                    }
                    let result = match dlg.pick_file() {
                        None => HostResult::Cancelled,
                        Some(path) => read_file(&path),
                    };
                    e.host_result(id, result);
                }
                PageRequest::SaveFile { id, name, bytes } => {
                    let result = match rfd::FileDialog::new().set_file_name(&name).save_file() {
                        None => HostResult::Cancelled,
                        Some(path) => match std::fs::write(&path, bytes.as_slice()) {
                            Ok(()) => HostResult::Saved { name: path.display().to_string() },
                            Err(err) => HostResult::Failed(err.to_string()),
                        },
                    };
                    e.host_result(id, result);
                }
                PageRequest::Navigate(name) => self.pending_nav = Some(name),
                PageRequest::FitPage | PageRequest::FitRect(_) => {}
            }
        }
        let focus = e.text_focus();
        if focus != self.ime {
            if let Some(w) = &self.window {
                w.set_ime_allowed(focus.is_some());
                if let Some((x, y, fw, fh)) = focus {
                    w.set_ime_cursor_area(PhysicalPosition::new(x as f64, y as f64), PhysicalSize::new(fw as f64, fh as f64));
                }
            }
            self.ime = focus;
        }
    }

    /// Build the engine for the current page choice on the existing window.
    fn start_engine(&mut self) -> Result<(), String> {
        let window = self.window.clone().ok_or("no window")?;
        let text = match &self.text {
            Some(t) => t.clone(),
            None => {
                // the app's language decides the Han glyph style
                let t = Arc::new(TextRaster::load_with(TextLoadOptions { locale: Some("zh-TW".into()), ..Default::default() })?);
                self.text = Some(t.clone());
                t
            }
        };
        let wake = self.wake();
        let t0 = Instant::now();
        let page = self.load_page(&text, wake.clone())?;
        self.page_desc = match &self.page_choice {
            PageChoice::Rust => "built-in Rust page (DemoPage)".to_string(),
            PageChoice::Builtin => "built-in Luau package pages/demo".to_string(),
            PageChoice::Dir(d) => format!("Luau package {}", d.display()),
        };
        self.page_desc += &format!(", ready in {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0);
        println!("page: {}", self.page_desc);
        let size = window.inner_size();
        let target = Target::Handle(window.clone() as Arc<dyn wgpu::WindowHandle>);
        let cfg = EngineConfig { vsync: self.vsync, show_hud: self.hud || self.bench_mode, ..Default::default() };
        let engine = Engine::new(&target, (size.width, size.height), window.scale_factor() as f32, page, text, wake, cfg)?;
        println!("{}", engine.summary());
        self.engine = Some(engine);
        self.ime = None;
        Ok(())
    }

    /// `page.navigate(name)`: replace the page (and its engine) on the same window.
    fn navigate(&mut self, name: &str) {
        let Some(root) = self.pages_root.clone() else {
            log::warn!("page.navigate(\"{name}\"): no pages folder (pass --pages <dir>)");
            return;
        };
        let dir = root.join(name);
        if !dir.join("page.json").is_file() {
            log::warn!("page.navigate(\"{name}\"): {} has no page.json", dir.display());
            return;
        }
        let previous = std::mem::replace(&mut self.page_choice, PageChoice::Dir(dir));
        if self.switch_page() {
            self.history.push(previous);
            self.history.truncate(64);
        } else {
            self.page_choice = previous;
            if !self.switch_page() {
                eprintln!("could not reopen the previous page");
                std::process::exit(1);
            }
        }
    }

    /// Alt+Left: back to the page `page.navigate` left.
    fn go_back(&mut self) -> bool {
        let Some(prev) = self.history.pop() else { return false };
        let current = std::mem::replace(&mut self.page_choice, prev);
        if !self.switch_page() {
            self.page_choice = current;
            let _ = self.switch_page();
        }
        true
    }

    /// Replace the engine with one for `page_choice`; false if the page failed.
    fn switch_page(&mut self) -> bool {
        if let Some(mut e) = self.engine.take() {
            e.shutdown();
        }
        let ok = match self.start_engine() {
            Ok(()) => true,
            Err(err) => {
                eprintln!("page failed to load: {err}");
                false
            }
        };
        self.redraw();
        ok
    }

    fn serve_agents(&mut self) {
        let (Some(rx), Some(e)) = (&self.agent_calls, self.engine.as_mut()) else { return };
        while let Ok(call) = rx.try_recv() {
            let answer = e.page_mut().act(&call.request);
            e.mark_op();
            let _ = call.reply.send(answer);
        }
    }
}

fn read_file(path: &std::path::Path) -> HostResult {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    match std::fs::metadata(path) {
        Ok(m) if m.len() > MAX_FILE => return HostResult::Failed(format!("{name}: larger than 1 GB")),
        Err(err) => return HostResult::Failed(err.to_string()),
        _ => {}
    }
    match std::fs::read(path) {
        Ok(bytes) => HostResult::File { name, bytes: Arc::new(bytes) },
        Err(err) => HostResult::Failed(err.to_string()),
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.engine.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title(TITLE).with_inner_size(LogicalSize::new(1600.0, 1000.0));
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        self.window = Some(window.clone());
        if let Err(err) = self.start_engine() {
            eprintln!("page failed to load: {err}");
            std::process::exit(1);
        }
        let wake = self.wake();
        if self.bench_mode {
            self.bench = Some(Bench::new());
        }
        self.clipboard = arboard::Clipboard::new().map_err(|e| log::warn!("clipboard unavailable: {e}")).ok();
        if let Some(port) = self.agent_port {
            let token = agent::token();
            let w: Arc<dyn Fn() + Send + Sync> = Arc::new(move || wake());
            match agent::start(port, token.clone(), w) {
                Ok(rx) => {
                    println!("agent port: 127.0.0.1:{port} token {token}");
                    self.agent_calls = Some(rx);
                }
                Err(err) => eprintln!("agent port {port}: {err}"),
            }
        }
        self.redraw();
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _event: UserEvent) {
        self.wake_flag.store(false, Ordering::Release);
        self.serve_agents();
        self.serve_page();
        if let Some(name) = self.pending_nav.take() {
            self.navigate(&name);
        }
        self.redraw();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(e) = self.engine.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => {
                e.shutdown();
                el.exit();
                return;
            }
            WindowEvent::Resized(size) => e.resize(size.width, size.height),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => e.set_scale_factor(scale_factor as f32),
            WindowEvent::RedrawRequested => {
                if !std::mem::take(&mut self.we_requested) {
                    e.invalidate();
                }
                if let Some(b) = self.bench.as_mut() {
                    let t = e.start().elapsed().as_secs_f64();
                    b.step(e, t);
                }
                e.frame();
                if self.bench.as_ref().is_some_and(|b| b.done) {
                    self.finish_bench(el);
                }
                return;
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
                e.pointer_move(0, self.cursor.0, self.cursor.1);
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => e.pointer_down(0, self.cursor.0, self.cursor.1),
                ElementState::Released => e.pointer_up(0),
            },
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Back, .. } => {
                if self.go_back() {
                    return;
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 / 40.0, p.y as f32 / 40.0),
                };
                e.wheel(self.cursor.0, self.cursor.1, dx, dy);
            }
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
                return;
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let editing = e.text_focus().is_some();
                let typed = keys::typed_text(event.text.as_deref(), self.mods);
                if editing && typed.is_some() {
                    e.text_input(&typed.unwrap_or_default());
                } else if let Some(name) = keys::key_name(&event.logical_key, self.mods) {
                    // Escape belongs to the page; the window's close button quits
                    let handled = e.key(&name);
                    if !handled && name == "alt+left" && self.go_back() {
                        return;
                    }
                }
            }
            WindowEvent::Ime(ime) => match ime {
                Ime::Preedit(text, _) => e.ime_preedit(&text),
                Ime::Commit(text) => {
                    e.ime_preedit("");
                    e.text_input(&text);
                }
                Ime::Disabled => e.ime_preedit(""),
                Ime::Enabled => {}
            },
            WindowEvent::DroppedFile(path) => match read_file(&path) {
                HostResult::File { name, bytes } => e.file_drop(&name, bytes),
                other => log::warn!("dropped file: {other:?}"),
            },
            _ => return,
        }
        self.serve_page();
        if let Some(name) = self.pending_nav.take() {
            self.navigate(&name);
        }
        self.redraw();
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let (Some(e), Some(w)) = (self.engine.as_mut(), self.window.as_ref()) else { return };
        let now = Instant::now();
        let t = e.start().elapsed().as_secs_f64();
        let last = e.last();
        let bench_moving = self.bench.as_ref().is_some_and(|b| b.continuous(t));
        if last.continuous || bench_moving {
            self.we_requested = true;
            w.request_redraw();
            el.set_control_flow(ControlFlow::Wait);
            return;
        }
        // earliest of: a predicted change on the page, the next scripted action
        let bench_at = self.bench.as_ref().and_then(|b| b.next_time()).map(|s| e.start() + Duration::from_secs_f64(s));
        let wake_at = match (last.wake_at, bench_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if let Some(at) = wake_at {
            if now >= at {
                self.we_requested = true;
                w.request_redraw();
                el.set_control_flow(ControlFlow::Wait);
            } else {
                el.set_control_flow(ControlFlow::WaitUntil(at));
            }
            if self.idle_title_secs != 0 {
                self.idle_title_secs = 0;
                w.set_title(TITLE);
            }
            return;
        }
        // Nothing scheduled at all: sleep. Only the title ticks once a second,
        // an OS call, not a rendered frame.
        e.enter_idle(now);
        let since = e.stats().idle_start.unwrap_or(now);
        let secs = (now - since).as_secs();
        if secs >= 1 && secs != self.idle_title_secs {
            self.idle_title_secs = secs;
            w.set_title(&format!("{TITLE} — 閒置 {secs} 秒，0 幀"));
        }
        el.set_control_flow(ControlFlow::WaitUntil(since + Duration::from_secs(secs + 1)));
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info));
    let arg_after = |flag: &str| argv.iter().position(|a| a == flag).map(|i| argv.get(i + 1).cloned());
    let page_choice = if argv.iter().any(|a| a == "--rust-demo") {
        PageChoice::Rust
    } else {
        match arg_after("--page") {
            Some(Some(d)) => PageChoice::Dir(d.into()),
            Some(None) => {
                eprintln!("--page needs a folder");
                std::process::exit(2);
            }
            None => PageChoice::Builtin,
        }
    };
    let agent_port = match arg_after("--agent-port") {
        Some(Some(p)) => match p.parse::<u16>() {
            Ok(p) => Some(p),
            Err(_) => {
                eprintln!("--agent-port needs a port number");
                std::process::exit(2);
            }
        },
        Some(None) => {
            eprintln!("--agent-port needs a port number");
            std::process::exit(2);
        }
        None => None,
    };
    let pages_root = match arg_after("--pages") {
        Some(Some(d)) => Some(std::path::PathBuf::from(d)),
        _ => match &page_choice {
            PageChoice::Dir(d) => d.parent().map(|p| p.to_path_buf()),
            _ => Some(std::path::PathBuf::from("pages")).filter(|p| p.is_dir()),
        },
    };
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().expect("event loop");
    let proxy = Arc::new(Mutex::new(event_loop.create_proxy()));
    let mut app = App {
        bench_mode: argv.iter().any(|a| a == "--bench"),
        vsync: !argv.iter().any(|a| a == "--no-vsync"),
        proxy,
        wake_flag: Arc::new(AtomicBool::new(false)),
        window: None,
        engine: None,
        page_choice,
        page_desc: String::new(),
        bench: None,
        cursor: (0.0, 0.0),
        mods: ModifiersState::empty(),
        idle_title_secs: 0,
        we_requested: false,
        ime: None,
        clipboard: None,
        agent_port,
        agent_calls: None,
        text: None,
        pages_root,
        pending_nav: None,
        history: Vec::new(),
        hud: argv.iter().any(|a| a == "--hud"),
    };
    event_loop.run_app(&mut app).expect("run app");
}
