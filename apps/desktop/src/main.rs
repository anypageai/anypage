//! Desktop host for the AnyPage engine (docs/ARCHITECTURE.md section 14.4,
//! "engine as app"): winit gives a window and events, the engine does the rest.
//!
//!   cargo run --release -p anypage-desktop                interactive
//!   cargo run --release -p anypage-desktop -- --bench     scripted 22 s run, writes bench-results/
//!   cargo run --release -p anypage-desktop -- --no-vsync

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anypage_demo::bench::Bench;
use anypage_demo::DemoPage;
use anypage_engine::{Engine, EngineConfig, Target, TextRaster};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

const TITLE: &str = "AnyPage — 一個頁面，所有時間性";

#[derive(Debug, Clone, Copy)]
enum UserEvent {
    Wake,
}

struct App {
    bench_mode: bool,
    vsync: bool,
    proxy: Arc<Mutex<EventLoopProxy<UserEvent>>>,
    wake_flag: Arc<AtomicBool>,
    window: Option<Arc<Window>>,
    engine: Option<Engine<DemoPage>>,
    bench: Option<Bench>,
    cursor: (f32, f32),
    idle_title_secs: u64,
    /// We asked for the next redraw. Any other redraw comes from the OS
    /// (expose, uncover, capture) and must present, or Windows keeps resending WM_PAINT.
    we_requested: bool,
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
        let report = b.report(e);
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
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.engine.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title(TITLE).with_inner_size(LogicalSize::new(1600.0, 1000.0));
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        let text = Arc::new(TextRaster::load().expect("font"));
        let wake = self.wake();
        let page = DemoPage::new(text.clone(), wake.clone());
        let size = window.inner_size();
        let target = Target::Handle(window.clone() as Arc<dyn wgpu::WindowHandle>);
        let cfg = EngineConfig { vsync: self.vsync, ..Default::default() };
        let engine = Engine::new(&target, (size.width, size.height), window.scale_factor() as f32, page, text, wake, cfg).expect("engine");
        println!("{}", engine.summary());
        self.engine = Some(engine);
        if self.bench_mode {
            self.bench = Some(Bench::new());
        }
        self.window = Some(window);
        self.redraw();
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _event: UserEvent) {
        self.wake_flag.store(false, Ordering::Release);
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
                if self.bench.as_ref().map_or(false, |b| b.done) {
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
            WindowEvent::MouseWheel { delta, .. } => {
                let factor = match delta {
                    MouseScrollDelta::LineDelta(_, y) => 1.15f32.powf(y),
                    MouseScrollDelta::PixelDelta(p) => 1.0015f32.powf(p.y as f32),
                };
                e.wheel(self.cursor.0, self.cursor.1, factor);
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let name = match &event.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        e.shutdown();
                        el.exit();
                        return;
                    }
                    Key::Named(NamedKey::Space) => "space".to_string(),
                    Key::Character(c) => c.to_lowercase(),
                    _ => return,
                };
                e.key(&name);
            }
            _ => return,
        }
        self.redraw();
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let (Some(e), Some(w)) = (self.engine.as_mut(), self.window.as_ref()) else { return };
        let now = Instant::now();
        let t = e.start().elapsed().as_secs_f64();
        let last = e.last();
        let bench_moving = self.bench.as_ref().map_or(false, |b| b.continuous(t));
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
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().expect("event loop");
    let proxy = Arc::new(Mutex::new(event_loop.create_proxy()));
    let mut app = App {
        bench_mode: argv.iter().any(|a| a == "--bench"),
        vsync: !argv.iter().any(|a| a == "--no-vsync"),
        proxy,
        wake_flag: Arc::new(AtomicBool::new(false)),
        window: None,
        engine: None,
        bench: None,
        cursor: (0.0, 0.0),
        idle_title_secs: 0,
        we_requested: false,
    };
    event_loop.run_app(&mut app).expect("run app");
}
