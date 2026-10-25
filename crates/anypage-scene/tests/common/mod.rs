//! Headless GPU helpers shared by the integration tests: a device with the
//! same limits the engine requests, render into an sRGB texture, read back.
#![allow(dead_code)]

use std::path::PathBuf;

use anypage_core::Value;
use anypage_engine::wgpu;
use anypage_engine::{GpuRenderer, GpuTarget};

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub info: String,
    /// A software rasteriser (WARP, llvmpipe): heavy timing tests skip.
    pub software: bool,
}

/// One instance per test binary. Vulkan / DX12 / Metal unless `WGPU_BACKEND`
/// says otherwise: GL contexts created from many test threads at once are
/// not something drivers handle well.
fn instance() -> &'static wgpu::Instance {
    static I: std::sync::OnceLock<wgpu::Instance> = std::sync::OnceLock::new();
    I.get_or_init(|| wgpu::Instance::new(&wgpu::InstanceDescriptor { backends: wgpu::Backends::from_env().unwrap_or(wgpu::Backends::PRIMARY), ..Default::default() }))
}

/// A device without a surface, with the engine's (WebGL2-level) limits.
/// `None` when the machine has no adapter at all (tests then skip). Each
/// test gets its own device: wgpu error scopes belong to the device.
pub fn gpu() -> Option<Gpu> {
    static CREATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_at_a_time = CREATE.lock().unwrap_or_else(|p| p.into_inner());
    let instance = instance();
    for fallback in [false, true] {
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: fallback,
        })) else {
            continue;
        };
        let info = adapter.get_info();
        let limits = wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits());
        if let Ok((device, queue)) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { label: Some("scene-test"), required_limits: limits, ..Default::default() })) {
            let software = info.device_type == wgpu::DeviceType::Cpu;
            return Some(Gpu { device, queue, info: format!("{} ({:?})", info.name, info.backend), software });
        }
    }
    eprintln!("SKIPPED: no GPU adapter on this machine");
    None
}

pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

pub struct Target {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub w: u32,
    pub h: u32,
}

pub fn target(g: &Gpu, w: u32, h: u32) -> Target {
    let texture = g.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test target"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    Target { texture, view, w, h }
}

/// Render `value` with `r` as the engine would (inside a validation error
/// scope) and return the pixels: sRGB-encoded, premultiplied RGBA8, rows top down.
pub fn render(g: &Gpu, r: &dyn GpuRenderer, value: &Value, w: u32, h: u32) -> Result<Vec<u8>, String> {
    let t = target(g, w, h);
    render_into(g, r, value, &t)?;
    Ok(read(g, &t))
}

pub fn render_into(g: &Gpu, r: &dyn GpuRenderer, value: &Value, t: &Target) -> Result<(), String> {
    g.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let target = GpuTarget { device: &g.device, queue: &g.queue, view: &t.view, format: FORMAT, size: (t.w, t.h) };
    let res = r.render(&target, value);
    if let Some(e) = pollster::block_on(g.device.pop_error_scope()) {
        return Err(format!("validation: {e}"));
    }
    res
}

/// Wait for the GPU to finish everything submitted so far.
pub fn wait(g: &Gpu) {
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let d = done.clone();
    g.queue.on_submitted_work_done(move || d.store(true, std::sync::atomic::Ordering::SeqCst));
    poll_until(g, &done);
}

/// Poll without blocking until `flag` is set; a stuck GPU fails the test
/// instead of hanging it.
fn poll_until(g: &Gpu, flag: &std::sync::atomic::AtomicBool) {
    let t0 = std::time::Instant::now();
    while !flag.load(std::sync::atomic::Ordering::SeqCst) {
        let _ = g.device.poll(wgpu::PollType::Poll);
        assert!(t0.elapsed() < std::time::Duration::from_secs(120), "the GPU did not finish within 2 minutes");
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
}

pub fn read(g: &Gpu, t: &Target) -> Vec<u8> {
    let bpr = (t.w * 4).div_ceil(256) * 256;
    let buf = g.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (bpr * t.h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = g.device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: &t.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bpr), rows_per_image: Some(t.h) } },
        wgpu::Extent3d { width: t.w, height: t.h, depth_or_array_layers: 1 },
    );
    g.queue.submit(Some(enc.finish()));
    let slice = buf.slice(..);
    let mapped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let m = mapped.clone();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        assert!(r.is_ok(), "map failed: {r:?}");
        m.store(true, std::sync::atomic::Ordering::SeqCst)
    });
    poll_until(g, &mapped);
    let data = slice.get_mapped_range();
    let mut out = Vec::with_capacity((t.w * t.h * 4) as usize);
    for row in 0..t.h as usize {
        let at = row * bpr as usize;
        out.extend_from_slice(&data[at..at + t.w as usize * 4]);
    }
    out
}

pub fn px(img: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2], img[i + 3]]
}

pub fn close(a: [u8; 4], b: [u8; 4], tol: u8) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= tol)
}

/// Fraction of pixels for which `f` holds.
pub fn coverage(img: &[u8], f: impl Fn([u8; 4]) -> bool) -> f64 {
    let n = img.len() / 4;
    img.as_chunks::<4>().0.iter().filter(|c| f(**c)).count() as f64 / n as f64
}

pub fn out_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-output");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Save as PNG under target/test-output/ (un-premultiplied for viewing).
pub fn save_png(name: &str, img: &[u8], w: u32, h: u32) {
    let mut straight = img.to_vec();
    for c in straight.as_chunks_mut::<4>().0 {
        let a = c[3] as u32;
        if a > 0 && a < 255 {
            for v in c.iter_mut().take(3) {
                *v = ((*v as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let path = out_dir().join(name);
    let Ok(file) = std::fs::File::create(&path) else { return };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    if let Ok(mut wr) = enc.write_header() {
        let _ = wr.write_image_data(&straight);
    }
}
