//! The compositor: one wgpu device, a textured-quad pipeline for tiles, panels
//! and the HUD, one pipeline per page-supplied GPU operator, and a texture
//! store with an LRU byte budget for tiles. The surface can be detached and
//! re-attached (Android destroys it whenever the app goes to the background).

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use anypage_core::TileKey;
use wgpu::rwh::{RawDisplayHandle, RawWindowHandle};

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CameraUniform {
    pub scale: f32,
    pub tx: f32,
    pub ty: f32,
    pub _pad: f32,
    pub viewport: [f32; 2],
    pub _pad2: [f32; 2],
}

impl CameraUniform {
    pub fn new(scale: f32, tx: f32, ty: f32, w: f32, h: f32) -> Self {
        Self { scale, tx, ty, _pad: 0.0, viewport: [w, h], _pad2: [0.0; 2] }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct EffectUniform {
    pub rect: [f32; 4],
    pub p: [f32; 8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TexKey {
    Tile(u16, TileKey),
    Panel(u32),
    /// Offscreen target of a page's GPU layer.
    Layer(u32),
    Hud,
}

pub struct GpuTex {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    pub w: u32,
    pub h: u32,
    pub ver: u64,
    pub last_used: u64,
    pub pinned: bool,
}

pub enum Draw {
    Quads { key: TexKey, range: Range<u32>, screen: bool },
    Effect(usize),
}

/// What the host gives the engine to draw into.
pub enum Target {
    /// A window object (winit and friends).
    Handle(Arc<dyn wgpu::WindowHandle>),
    /// Raw platform handles; the host keeps them valid until `detach_surface`.
    Raw { display: RawDisplayHandle, window: RawWindowHandle },
}

impl Target {
    fn surface(&self, instance: &wgpu::Instance) -> Result<wgpu::Surface<'static>, String> {
        match self {
            Target::Handle(h) => instance.create_surface(h.clone()).map_err(|e| e.to_string()),
            // SAFETY: the host guarantees the handles outlive the surface (it
            // calls `detach_surface` before invalidating them).
            Target::Raw { display, window } => unsafe {
                instance
                    .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                        raw_display_handle: *display,
                        raw_window_handle: *window,
                    })
                    .map_err(|e| e.to_string())
            },
        }
    }
}

struct EffectGpu {
    pipeline: wgpu::RenderPipeline,
    buf: wgpu::Buffer,
    bg: wgpu::BindGroup,
}

pub const MAX_QUADS: usize = 16384;
const TEX_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

pub struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    surface: Option<wgpu::Surface<'static>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    vsync: bool,
    pub adapter_info: wgpu::AdapterInfo,
    quad_pipeline: wgpu::RenderPipeline,
    tex_layout: wgpu::BindGroupLayout,
    cam_layout: wgpu::BindGroupLayout,
    fx_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    world_cam: (wgpu::Buffer, wgpu::BindGroup),
    screen_cam: (wgpu::Buffer, wgpu::BindGroup),
    effects: Vec<EffectGpu>,
    vertex_buf: wgpu::Buffer,
    textures: HashMap<TexKey, GpuTex>,
    pub tile_budget_bytes: usize,
    pub frame: u64,
}

fn uniform_layout(device: &wgpu::Device, label: &str, vis: wgpu::ShaderStages) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: vis,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        }],
    })
}

fn uniform_buffer(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, size: u64, label: &str) -> (wgpu::Buffer, wgpu::BindGroup) {
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() }],
    });
    (buf, bg)
}

const PREMULTIPLIED: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
};

/// Backend fallback order (docs/ARCHITECTURE.md section 14.6). `ANYPAGE_BACKEND`
/// forces one of vulkan, dx12, metal, gl.
fn backend_order() -> Vec<wgpu::Backends> {
    match std::env::var("ANYPAGE_BACKEND").ok().as_deref() {
        Some("vulkan") => vec![wgpu::Backends::VULKAN],
        Some("dx12") => vec![wgpu::Backends::DX12],
        Some("metal") => vec![wgpu::Backends::METAL],
        Some("gl") => vec![wgpu::Backends::GL],
        _ if cfg!(windows) => vec![wgpu::Backends::DX12, wgpu::Backends::VULKAN, wgpu::Backends::GL],
        _ if cfg!(target_vendor = "apple") => vec![wgpu::Backends::METAL],
        _ => vec![wgpu::Backends::VULKAN, wgpu::Backends::GL],
    }
}

impl Gpu {
    pub fn new(target: &Target, w: u32, h: u32, vsync: bool, tile_budget_bytes: usize) -> Result<Self, String> {
        let mut chosen = None;
        let mut errors = Vec::new();
        for backends in backend_order() {
            let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends, ..Default::default() });
            let surface = match target.surface(&instance) {
                Ok(s) => s,
                Err(e) => {
                    errors.push(format!("{backends:?}: surface: {e}"));
                    continue;
                }
            };
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            }));
            match adapter {
                Ok(a) => {
                    chosen = Some((instance, surface, a));
                    break;
                }
                Err(e) => errors.push(format!("{backends:?}: adapter: {e}")),
            }
        }
        let (instance, surface, adapter) = chosen.ok_or_else(|| format!("no usable GPU backend: {}", errors.join("; ")))?;
        let adapter_info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("anypage"),
            required_limits: wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .map_err(|e| format!("request device: {e}"))?;

        let mut config = surface.get_default_config(&adapter, w.max(1), h.max(1)).ok_or("surface not supported by adapter")?;
        config.present_mode = if vsync { wgpu::PresentMode::AutoVsync } else { wgpu::PresentMode::AutoNoVsync };
        surface.configure(&device, &config);

        let tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tex"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let cam_layout = uniform_layout(&device, "camera", wgpu::ShaderStages::VERTEX);
        let fx_layout = uniform_layout(&device, "effect", wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT);

        let quad_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("quads"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let quad_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quads"),
            bind_group_layouts: &[&tex_layout, &cam_layout],
            push_constant_ranges: &[],
        });
        let target_state = [Some(wgpu::ColorTargetState { format: config.format, blend: Some(PREMULTIPLIED), write_mask: wgpu::ColorWrites::ALL })];
        let quad_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quads"),
            layout: Some(&quad_layout),
            vertex: wgpu::VertexState {
                module: &quad_shader,
                entry_point: Some("vs_main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &quad_shader,
                entry_point: Some("fs_main"),
                targets: &target_state,
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default() },
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let cam_size = std::mem::size_of::<CameraUniform>() as u64;
        let world_cam = uniform_buffer(&device, &cam_layout, cam_size, "world camera");
        let screen_cam = uniform_buffer(&device, &cam_layout, cam_size, "screen camera");
        let vertex_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quads"),
            size: (MAX_QUADS * 6 * std::mem::size_of::<Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            instance,
            adapter,
            surface: Some(surface),
            device,
            queue,
            config,
            vsync,
            adapter_info,
            quad_pipeline,
            tex_layout,
            cam_layout,
            fx_layout,
            sampler,
            world_cam,
            screen_cam,
            effects: Vec::new(),
            vertex_buf,
            textures: HashMap::new(),
            tile_budget_bytes,
            frame: 0,
        })
    }

    /// Compile a page-supplied GPU operator. Returns its index for `Draw::Effect`.
    /// Build a page's GPU operator. A shader the device rejects is an error
    /// for that operator only; it must never take the engine down.
    pub fn add_effect(&mut self, wgsl: &str) -> Result<usize, String> {
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let module = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("page effect"),
            source: wgpu::ShaderSource::Wgsl(wgsl.into()),
        });
        let layout = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("page effect"),
            bind_group_layouts: &[&self.cam_layout, &self.fx_layout],
            push_constant_ranges: &[],
        });
        let target_state = [Some(wgpu::ColorTargetState { format: self.config.format, blend: Some(PREMULTIPLIED), write_mask: wgpu::ColorWrites::ALL })];
        let pipeline = self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("page effect"),
            layout: Some(&layout),
            vertex: wgpu::VertexState { module: &module, entry_point: Some("vs_main"), buffers: &[], compilation_options: Default::default() },
            fragment: Some(wgpu::FragmentState { module: &module, entry_point: Some("fs_main"), targets: &target_state, compilation_options: Default::default() }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default() },
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        if let Some(err) = pollster::block_on(self.device.pop_error_scope()) {
            return Err(match err {
                wgpu::Error::Validation { description, .. } => description,
                other => other.to_string(),
            });
        }
        let (buf, bg) = uniform_buffer(&self.device, &self.fx_layout, std::mem::size_of::<EffectUniform>() as u64, "effect params");
        self.effects.push(EffectGpu { pipeline, buf, bg });
        Ok(self.effects.len() - 1)
    }

    pub fn has_surface(&self) -> bool {
        self.surface.is_some()
    }

    /// Drop the surface but keep the device and every texture.
    pub fn detach_surface(&mut self) {
        self.surface = None;
    }

    pub fn attach_surface(&mut self, target: &Target, w: u32, h: u32) -> Result<(), String> {
        let surface = target.surface(&self.instance)?;
        let mut config = surface.get_default_config(&self.adapter, w.max(1), h.max(1)).ok_or("surface not supported by adapter")?;
        config.format = self.config.format; // pipelines are built for this format
        config.present_mode = if self.vsync { wgpu::PresentMode::AutoVsync } else { wgpu::PresentMode::AutoNoVsync };
        surface.configure(&self.device, &config);
        self.config = config;
        self.surface = Some(surface);
        Ok(())
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        if let Some(s) = &self.surface {
            s.configure(&self.device, &self.config);
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn present_mode(&self) -> wgpu::PresentMode {
        self.config.present_mode
    }

    pub fn tex(&self, key: &TexKey) -> Option<&GpuTex> {
        self.textures.get(key)
    }

    pub fn touch(&mut self, key: &TexKey) {
        let f = self.frame;
        if let Some(t) = self.textures.get_mut(key) {
            t.last_used = f;
        }
    }

    /// Upload premultiplied RGBA8. Reuses the texture when the size matches.
    pub fn upload(&mut self, key: TexKey, w: u32, h: u32, rgba: &[u8], ver: u64, pinned: bool) {
        debug_assert_eq!(rgba.len(), (w * h * 4) as usize);
        let reuse = matches!(self.textures.get(&key), Some(t) if t.w == w && t.h == h);
        if !reuse {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TEX_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.tex_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            });
            self.textures.insert(key, GpuTex { texture, bind_group, w, h, ver, last_used: self.frame, pinned });
        }
        let t = self.textures.get_mut(&key).expect("texture just inserted");
        t.ver = ver;
        t.pinned = pinned;
        t.last_used = self.frame;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &t.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            rgba,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }

    /// Largest texture side this device accepts.
    pub fn max_texture(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }

    /// Run a page GPU layer's renderer into its offscreen texture, creating
    /// or resizing the texture as needed. The texture is then drawn with
    /// `Draw::Quads` like a panel.
    pub fn render_layer(&mut self, key: TexKey, w: u32, h: u32, ver: u64, renderer: &dyn crate::page::GpuRenderer, value: &anypage_core::Value) -> Result<(), String> {
        let reuse = matches!(self.textures.get(&key), Some(t) if t.w == w && t.h == h);
        if !reuse {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("gpu layer"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TEX_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.tex_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            });
            self.textures.insert(key, GpuTex { texture, bind_group, w, h, ver, last_used: self.frame, pinned: true });
        }
        let t = self.textures.get_mut(&key).expect("layer texture");
        t.ver = ver;
        t.last_used = self.frame;
        let view = t.texture.create_view(&Default::default());
        // a renderer's mistakes (invalid use, out of memory, a panic) cost
        // only its own layer, never the engine
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        self.device.push_error_scope(wgpu::ErrorFilter::Internal);
        let target = crate::page::GpuTarget { device: &self.device, queue: &self.queue, view: &view, format: TEX_FORMAT, size: (w, h) };
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| renderer.render(&target, value)))
            .unwrap_or_else(|_| Err("the layer's renderer panicked".into()));
        let mut first_err = None;
        for _ in 0..3 {
            if let Some(err) = pollster::block_on(self.device.pop_error_scope()) {
                first_err.get_or_insert(match err {
                    wgpu::Error::Validation { description, .. } => description,
                    other => other.to_string(),
                });
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => r,
        }
    }

    pub fn tile_count_and_bytes(&self) -> (usize, usize) {
        let mut n = 0;
        let mut b = 0;
        for (k, t) in &self.textures {
            if matches!(k, TexKey::Tile(..)) {
                n += 1;
                b += (t.w * t.h * 4) as usize;
            }
        }
        (n, b)
    }

    /// Evict least-recently-used, unpinned tiles that were not drawn this frame.
    pub fn evict_tiles_over_budget(&mut self) -> usize {
        let (_, bytes) = self.tile_count_and_bytes();
        if bytes <= self.tile_budget_bytes {
            return 0;
        }
        let current = self.frame;
        let mut victims: Vec<(u64, TexKey, usize)> = self
            .textures
            .iter()
            .filter(|(k, t)| matches!(k, TexKey::Tile(..)) && !t.pinned && t.last_used != current)
            .map(|(k, t)| (t.last_used, *k, (t.w * t.h * 4) as usize))
            .collect();
        victims.sort_unstable_by_key(|v| v.0);
        let mut over = bytes - self.tile_budget_bytes;
        let mut evicted = 0;
        for (_, k, b) in victims {
            if over == 0 {
                break;
            }
            self.textures.remove(&k);
            over = over.saturating_sub(b);
            evicted += 1;
        }
        evicted
    }

    /// Returns Ok(false) when there is no surface to draw into.
    pub fn render(
        &mut self,
        world: CameraUniform,
        screen: CameraUniform,
        effects: &[(usize, EffectUniform)],
        verts: &[Vertex],
        draws: &[Draw],
        clear: wgpu::Color,
    ) -> Result<bool, wgpu::SurfaceError> {
        let Some(surface) = &self.surface else { return Ok(false) };
        let frame = surface.get_current_texture()?;
        let view = frame.texture.create_view(&Default::default());

        self.queue.write_buffer(&self.world_cam.0, 0, bytemuck::bytes_of(&world));
        self.queue.write_buffer(&self.screen_cam.0, 0, bytemuck::bytes_of(&screen));
        for (i, u) in effects {
            if let Some(fx) = self.effects.get(*i) {
                self.queue.write_buffer(&fx.buf, 0, bytemuck::bytes_of(u));
            }
        }
        let n = verts.len().min(MAX_QUADS * 6);
        if n > 0 {
            self.queue.write_buffer(&self.vertex_buf, 0, bytemuck::cast_slice(&verts[..n]));
        }

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear), store: wgpu::StoreOp::Store },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_vertex_buffer(0, self.vertex_buf.slice(..));
            for d in draws {
                match d {
                    Draw::Quads { key, range, screen } => {
                        if range.end as usize > n {
                            continue;
                        }
                        let Some(t) = self.textures.get(key) else { continue };
                        rp.set_pipeline(&self.quad_pipeline);
                        rp.set_bind_group(0, &t.bind_group, &[]);
                        rp.set_bind_group(1, if *screen { &self.screen_cam.1 } else { &self.world_cam.1 }, &[]);
                        rp.draw(range.clone(), 0..1);
                    }
                    Draw::Effect(i) => {
                        let Some(fx) = self.effects.get(*i) else { continue };
                        rp.set_pipeline(&fx.pipeline);
                        rp.set_bind_group(0, &self.world_cam.1, &[]);
                        rp.set_bind_group(1, &fx.bg, &[]);
                        rp.draw(0..6, 0..1);
                    }
                }
            }
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        self.frame += 1;
        Ok(true)
    }
}
