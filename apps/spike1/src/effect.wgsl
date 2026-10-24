// A GPU operator (docs/ARCHITECTURE.md section 4.2): a pull stream that is a
// pure function of time `t` and the page's `hue` input, evaluated per pixel.

struct Camera {
    scale: f32,
    tx: f32,
    ty: f32,
    _pad: f32,
    viewport: vec2<f32>,
    _pad2: vec2<f32>,
};

struct Effect {
    rect: vec4<f32>,
    t: f32,
    hue: f32,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> cam: Camera;
@group(1) @binding(0) var<uniform> fx: Effect;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0)
    );
    let c = corners[vi];
    let world = fx.rect.xy + c * fx.rect.zw;
    let screen = world * cam.scale + vec2<f32>(cam.tx, cam.ty);
    let ndc = vec2<f32>(screen.x / cam.viewport.x * 2.0 - 1.0, 1.0 - screen.y / cam.viewport.y * 2.0);
    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = c;
    return out;
}

fn hsv2rgb(c: vec3<f32>) -> vec3<f32> {
    let k = vec4<f32>(1.0, 2.0 / 3.0, 1.0 / 3.0, 3.0);
    let p = abs(fract(c.xxx + k.xyz) * 6.0 - k.www);
    return c.z * mix(k.xxx, clamp(p - k.xxx, vec3<f32>(0.0), vec3<f32>(1.0)), c.y);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let aspect = fx.rect.z / max(fx.rect.w, 1.0);
    let p = vec2<f32>(in.uv.x * aspect, in.uv.y) * 6.0;
    let t = fx.t;
    let centre = vec2<f32>(3.0 * aspect * 0.5 + sin(t * 0.5) * 2.0, 3.0 + cos(t * 0.37) * 1.5);
    let v = sin(p.x + t)
          + sin(p.y * 1.3 + t * 1.7)
          + sin((p.x + p.y) * 0.7 + t * 0.9)
          + sin(length(p - centre) * 1.5 - t * 2.0);
    let h = fract(fx.hue / 360.0 + v * 0.08);
    let rgb = hsv2rgb(vec3<f32>(h, 0.7, 0.35 + 0.25 * sin(v * 1.5)));
    return vec4<f32>(rgb, 1.0);
}
