// scene.view: meshes (headlight Lambert + ambient), points (1-pixel point
// list or instanced screen-aligned quads), grid and axes (line list).
// Output is premultiplied alpha into an sRGB target: colours are converted
// from sRGB bytes to linear here and the hardware encodes on write.

struct Item {
    mvp: mat4x4<f32>,
    // view-space normal matrix (inverse transpose of model-view), columns 0..2
    nmat: mat4x4<f32>,
    // linear RGB, straight alpha
    color: vec4<f32>,
    // height ramp: t = dot(height.xyz, p) + height.w, 0..1 across the item
    height: vec4<f32>,
    // x: point size px, y: colour mode (0 vertex x item, 1 item only, 2 height), z: ambient, w: lit (0/1)
    params: vec4<f32>,
    // x, y: target size px; z: round points (0/1)
    viewport: vec4<f32>,
};

@group(0) @binding(0) var<uniform> item: Item;

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

// blue -> cyan -> green -> yellow -> red, in sRGB, then linearised
fn ramp(t: f32) -> vec3<f32> {
    let x = clamp(t, 0.0, 1.0) * 4.0;
    let r = clamp(x - 2.0, 0.0, 1.0);
    let g = clamp(x, 0.0, 1.0) - clamp(x - 3.0, 0.0, 1.0);
    let b = 1.0 - clamp(x - 1.0, 0.0, 1.0);
    return srgb_to_linear(vec3<f32>(r, g, b));
}

fn base_color(p: vec3<f32>, c: vec4<f32>) -> vec4<f32> {
    let mode = item.params.y;
    if mode > 1.5 {
        let t = dot(item.height.xyz, p) + item.height.w;
        return vec4<f32>(ramp(t), item.color.a);
    }
    if mode > 0.5 {
        return item.color;
    }
    return vec4<f32>(srgb_to_linear(c.rgb) * item.color.rgb, c.a * item.color.a);
}

fn premultiply(c: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(c.rgb * c.a, c.a);
}

// ---------------------------------------------------------------- meshes

struct MeshOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) normal: vec3<f32>,
};

@vertex
fn vs_mesh(@location(0) p: vec3<f32>, @location(1) n: vec3<f32>, @location(2) c: vec4<f32>) -> MeshOut {
    var o: MeshOut;
    o.pos = item.mvp * vec4<f32>(p, 1.0);
    o.normal = (item.nmat * vec4<f32>(n, 0.0)).xyz;
    o.color = base_color(p, c);
    return o;
}

@fragment
fn fs_mesh(i: MeshOut) -> @location(0) vec4<f32> {
    var shade = 1.0;
    let len = length(i.normal);
    if item.params.w > 0.5 && len > 1e-20 {
        // headlight: the light sits at the eye, so facing the camera is fully lit;
        // two-sided, because scanned and STL meshes rarely have consistent winding
        let lambert = abs(i.normal.z / len);
        shade = item.params.z + (1.0 - item.params.z) * lambert;
    }
    return premultiply(vec4<f32>(i.color.rgb * shade, i.color.a));
}

// ---------------------------------------------------------------- points and lines

struct PointOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec2<f32>,
};

@vertex
fn vs_point(@location(0) p: vec3<f32>, @location(1) c: vec4<f32>) -> PointOut {
    var o: PointOut;
    o.pos = item.mvp * vec4<f32>(p, 1.0);
    o.color = base_color(p, c);
    o.uv = vec2<f32>(0.0, 0.0);
    return o;
}

// One instance per point, six vertices per instance: a quad of
// `point size` pixels around the projected point.
@vertex
fn vs_quad(@builtin(vertex_index) vi: u32, @location(0) p: vec3<f32>, @location(1) c: vec4<f32>) -> PointOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0)
    );
    let corner = corners[vi % 6u];
    var o: PointOut;
    var clip = item.mvp * vec4<f32>(p, 1.0);
    let size = item.params.x;
    clip.x = clip.x + corner.x * size / item.viewport.x * clip.w;
    clip.y = clip.y + corner.y * size / item.viewport.y * clip.w;
    o.pos = clip;
    o.color = base_color(p, c);
    o.uv = corner;
    return o;
}

@fragment
fn fs_point(i: PointOut) -> @location(0) vec4<f32> {
    if item.viewport.z > 0.5 && dot(i.uv, i.uv) > 1.0 {
        discard;
    }
    return premultiply(i.color);
}
