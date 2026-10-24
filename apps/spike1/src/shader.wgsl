// Textured quads through a 2D camera. The same pipeline draws world-space quads
// (tiles, panels) with the world camera and screen-space quads (HUD) with an
// identity camera. Textures hold premultiplied alpha.

struct Camera {
    scale: f32,
    tx: f32,
    ty: f32,
    _pad: f32,
    viewport: vec2<f32>,
    _pad2: vec2<f32>,
};

@group(0) @binding(0) var t_tex: texture_2d<f32>;
@group(0) @binding(1) var s_tex: sampler;
@group(1) @binding(0) var<uniform> cam: Camera;

struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    let screen = vec2<f32>(in.pos.x * cam.scale + cam.tx, in.pos.y * cam.scale + cam.ty);
    let ndc = vec2<f32>(screen.x / cam.viewport.x * 2.0 - 1.0, 1.0 - screen.y / cam.viewport.y * 2.0);
    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = in.uv;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(t_tex, s_tex, in.uv);
}
