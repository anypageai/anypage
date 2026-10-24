//! The compositor: one wgpu device, a textured-quad pipeline for tiles, panels
//! and the HUD, a GPU-operator pipeline for the effect, and a texture store
//! with an LRU byte budget for tiles.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use anypage_core::TileKey;
use winit::window::Window;

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
    pub t: f32,
    pub hue: f32,
    pub _pad: [f32; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TexKey {
    Tile(TileKey),
    Panel(u32),
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
    Effect,
}

pub const MAX_QUADS: usize = 16384;
const TEX_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

pub struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pub adapter_info: wgpu::AdapterInfo,
    quad_pipeline: wgpu::RenderPipeline,
    effect_pipeline: wgpu::RenderPipeline,
    tex_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    world_cam: (wgpu::Buffer, wgpu::BindGroup),
    screen_cam: (wgpu::Buffer, wgpu::BindGroup),
    effect: (wgpu::Buffer, wgpu::BindGroup),
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
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
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
    pub fn new(window: Arc<Window>, vsync: bool, tile_budget_bytes: usize) -> Self {
        let mut chosen = None;
        for backends in backend_order() {
            let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends, ..Default::default() });
            let Ok(surface) = instance.create_surface(window.clone()) else { continue };
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            }));
            if let Ok(adapter) = adapter {
                chosen = Some((surface, adapter));
                break;
            }
            eprintln!("backend {backends:?} unavailable, trying the next one");
        }
        let (surface, adapter) = chosen.expect("no usable GPU backend");
        let adapter_info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("anypage"),
            ..Default::default()
        }))
        .expect("request device");

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .expect("surface config");
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
        let fx_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("effect"),
            source: wgpu::ShaderSource::Wgsl(include_str!("effect.wgsl").into()),
        });

        let quad_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quads"),
            bind_group_layouts: &[&tex_layout, &cam_layout],
            push_constant_ranges: &[],
        });
        let target = [Some(wgpu::ColorTargetState {
            format: config.format,
            blend: Some(PREMULTIPLIED),
            write_mask: wgpu::ColorWrites::ALL,
        })];
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
                targets: &target,
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default() },
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });

        let fx_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("effect"),
            bind_group_layouts: &[&cam_layout, &fx_layout],
            push_constant_ranges: &[],
        });
        let effect_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("effect"),
            layout: Some(&fx_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &fx_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &fx_shader,
                entry_point: Some("fs_main"),
                targets: &target,
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
        let effect = uniform_buffer(&device, &fx_layout, std::mem::size_of::<EffectUniform>() as u64, "effect");
        let vertex_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quads"),
            size: (MAX_QUADS * 6 * std::mem::size_of::<Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            surface,
            device,
            queue,
            config,
            adapter_info,
            quad_pipeline,
            effect_pipeline,
            tex_layout,
            sampler,
            world_cam,
            screen_cam,
            effect,
            vertex_buf,
            textures: HashMap::new(),
            tile_budget_bytes,
            frame: 0,
        }
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
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
            wgpu::TexelCopyTextureInfo {
                texture: &t.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }

    pub fn tile_count_and_bytes(&self) -> (usize, usize) {
        let mut n = 0;
        let mut b = 0;
        for (k, t) in &self.textures {
            if matches!(k, TexKey::Tile(_)) {
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
            .filter(|(k, t)| matches!(k, TexKey::Tile(_)) && !t.pinned && t.last_used != current)
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

    pub fn render(
        &mut self,
        world: CameraUniform,
        screen: CameraUniform,
        effect: Option<EffectUniform>,
        verts: &[Vertex],
        draws: &[Draw],
        clear: wgpu::Color,
    ) -> Result<(), wgpu::SurfaceError> {
        let frame = self.surface.get_current_texture()?;
        let view = frame.texture.create_view(&Default::default());

        self.queue.write_buffer(&self.world_cam.0, 0, bytemuck::bytes_of(&world));
        self.queue.write_buffer(&self.screen_cam.0, 0, bytemuck::bytes_of(&screen));
        if let Some(fx) = effect {
            self.queue.write_buffer(&self.effect.0, 0, bytemuck::bytes_of(&fx));
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
                    Draw::Effect => {
                        if effect.is_none() {
                            continue;
                        }
                        rp.set_pipeline(&self.effect_pipeline);
                        rp.set_bind_group(0, &self.world_cam.1, &[]);
                        rp.set_bind_group(1, &self.effect.1, &[]);
                        rp.draw(0..6, 0..1);
                    }
                }
            }
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        self.frame += 1;
        Ok(())
    }
}
