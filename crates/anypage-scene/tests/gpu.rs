//! Headless rendering of `scene.view` with a device created here (no
//! surface), read back and checked numerically: colours, lighting, depth
//! order, points, transparency, grid, budgets and the GPU cache.

mod common;

use std::sync::Arc;
use std::time::Instant;

use anypage_core::val;
use anypage_runtime::Data;
use anypage_scene::camera::Camera;
use anypage_scene::model::{Ctl, PointsBuilder};
use anypage_scene::synthetic;
use anypage_scene::{model_data, Frame, SceneRenderer};
use common::*;

fn rgba(r: u8, g: u8, b: u8, a: u8) -> Data {
    Data::Num(u32::from_be_bytes([r, g, b, a]) as f64)
}

fn cube(size: f32) -> Data {
    model_data(synthetic::build_primitive(synthetic::cuboid(size, size, size), "box").unwrap())
}

fn front_camera(distance: f64) -> Data {
    Data::map([("yaw", Data::Num(0.0)), ("pitch", Data::Num(0.0)), ("distance", Data::Num(distance)), ("fov", Data::Num(45.0))])
}

fn scene(items: Vec<Data>, extra: Vec<(&str, Data)>) -> Data {
    let mut pairs = vec![("items", Data::list(items))];
    pairs.extend(extra);
    Data::map(pairs)
}

fn item(model: &Data, extra: Vec<(&str, Data)>) -> Data {
    let mut pairs = vec![("model", model.clone())];
    pairs.extend(extra);
    Data::map(pairs)
}

fn frame(scene: &Data, camera: &Data) -> anypage_core::Value {
    val(Frame::from_inputs(&[scene.clone(), camera.clone()]).expect("frame"))
}

const BG: [u8; 4] = [32, 40, 64, 255];

fn bg() -> Data {
    rgba(BG[0], BG[1], BG[2], BG[3])
}

/// Pixel position of a world point for a camera, in a w x h view.
fn at(cam: &Data, p: [f64; 3], w: u32, h: u32) -> (u32, u32) {
    let (c, _) = Camera::from_data(cam).unwrap();
    let (x, y, _, vis) = c.project(p, w as f64, h as f64);
    assert!(vis);
    (x.round() as u32, y.round() as u32)
}

#[test]
fn red_cube_front_view() {
    let Some(g) = gpu() else { return };
    let r = SceneRenderer::new();
    let (w, h) = (200, 150);
    let cam = front_camera(4.0);
    let s = scene(vec![item(&cube(1.0), vec![("color", rgba(255, 0, 0, 255))])], vec![("background", bg())]);
    let img = render(&g, &r, &frame(&s, &cam), w, h).unwrap();
    save_png("cube_front.png", &img, w, h);
    // facing the headlight: exactly the item colour
    assert!(close(px(&img, w, w / 2, h / 2), [255, 0, 0, 255], 2), "centre {:?} on {}", px(&img, w, w / 2, h / 2), g.info);
    assert!(close(px(&img, w, 2, 2), BG, 1), "corner {:?}", px(&img, w, 2, 2));
    // the front face is 2 * 0.5 / (3.5 * tan 22.5deg) * 75 = 51.8 px square: 8.9 % of the view
    let red = coverage(&img, |c| c[0] > 200 && c[1] < 60 && c[2] < 60);
    assert!((0.080..0.098).contains(&red), "red coverage {red}");
    let st = r.stats();
    assert_eq!((st.items, st.triangles, st.width, st.height), (1, 12, w, h));
    assert!(st.samples == 4 || st.samples == 1);
}

#[test]
fn one_renderer_serves_layers_of_different_sizes() {
    let Some(g) = gpu() else { return };
    let r = SceneRenderer::new();
    let s = scene(vec![item(&cube(1.0), vec![("color", rgba(0, 0, 255, 255))])], vec![("background", bg())]);
    let v = frame(&s, &front_camera(4.0));
    for (w, h) in [(64, 48), (300, 100), (64, 48), (90, 160), (300, 100)] {
        let img = render(&g, &r, &v, w, h).unwrap();
        assert!(close(px(&img, w, w / 2, h / 2), [0, 0, 255, 255], 2), "{w}x{h}: {:?}", px(&img, w, w / 2, h / 2));
        assert!(close(px(&img, w, 1, 1), BG, 1));
        assert_eq!((r.stats().width, r.stats().height), (w, h));
    }
}

#[test]
fn headlight_lambert_and_ambient() {
    let Some(g) = gpu() else { return };
    let r = SceneRenderer::new();
    let (w, h) = (240, 180);
    let cam = front_camera(4.0);
    let rot = Data::map([("rotate", Data::nums([0.0, 60.0, 0.0]))]);
    let s = scene(vec![item(&cube(1.0), vec![("color", rgba(255, 255, 255, 255)), ("transform", rot)])], vec![("background", bg()), ("ambient", Data::Num(0.3))]);
    let img = render(&g, &r, &frame(&s, &cam), w, h).unwrap();
    save_png("cube_lambert.png", &img, w, h);
    let srgb = |lin: f64| (if lin <= 0.003_130_8 { lin * 12.92 } else { 1.055 * lin.powf(1.0 / 2.4) - 0.055 } * 255.0).round() as i32;
    // the -X face turned 30 degrees from the eye, the +Z face 60 degrees
    let (lx, ly) = at(&cam, [-0.25, 0.0, 0.433], w, h);
    let (rx, ry) = at(&cam, [0.433, 0.0, 0.25], w, h);
    let left = px(&img, w, lx, ly)[0] as i32;
    let right = px(&img, w, rx, ry)[0] as i32;
    let want_left = srgb(0.3 + 0.7 * (30f64).to_radians().cos());
    let want_right = srgb(0.3 + 0.7 * (60f64).to_radians().cos());
    assert!((left - want_left).abs() <= 2, "left face {left}, expected {want_left}");
    assert!((right - want_right).abs() <= 2, "right face {right}, expected {want_right}");
    // unlit: flat item colour everywhere
    let s = scene(vec![item(&cube(1.0), vec![("color", rgba(255, 255, 255, 255)), ("transform", Data::map([("rotate", Data::nums([0.0, 60.0, 0.0]))])), ("lit", Data::Bool(false))])], vec![("background", bg())]);
    let img = render(&g, &r, &frame(&s, &cam), w, h).unwrap();
    assert_eq!(px(&img, w, rx, ry), [255, 255, 255, 255]);
}

#[test]
fn depth_order_does_not_depend_on_item_order() {
    let Some(g) = gpu() else { return };
    let r = SceneRenderer::new();
    let (w, h) = (160, 120);
    let cam = front_camera(8.0);
    let near = item(&cube(1.0), vec![("color", rgba(255, 0, 0, 255)), ("transform", Data::map([("translate", Data::nums([0.0, 0.0, 1.0]))]))]);
    let far = item(&cube(3.0), vec![("color", rgba(0, 255, 0, 255)), ("transform", Data::map([("translate", Data::nums([0.0, 0.0, -1.0]))]))]);
    for (k, items) in [vec![near.clone(), far.clone()], vec![far.clone(), near.clone()]].into_iter().enumerate() {
        let img = render(&g, &r, &frame(&scene(items, vec![("background", bg())]), &cam), w, h).unwrap();
        save_png(&format!("depth_order_{k}.png"), &img, w, h);
        assert!(close(px(&img, w, w / 2, h / 2), [255, 0, 0, 255], 2), "order {k}: centre {:?}", px(&img, w, w / 2, h / 2));
        let (gx, gy) = at(&cam, [1.0, 1.0, 0.5], w, h);
        assert!(close(px(&img, w, gx, gy), [0, 255, 0, 255], 2), "order {k}: far box {:?}", px(&img, w, gx, gy));
    }
}

#[test]
fn points_as_quads_and_single_pixels() {
    let Some(g) = gpu() else { return };
    let r = SceneRenderer::new();
    let (w, h) = (200, 200);
    let cam = front_camera(5.0);
    let spots = [([-1.0, 0.0, 0.0], [255u8, 0, 0, 255]), ([1.0, 0.0, 0.0], [0, 255, 0, 255]), ([0.0, 1.0, 0.0], [0, 0, 255, 255]), ([0.0, -1.0, 0.0], [255, 255, 0, 255])];
    let mut pb = PointsBuilder::new([0.0; 3], 4).unwrap();
    pb.has_colors = true;
    for (p, c) in spots {
        pb.push(p, c).unwrap();
    }
    let cloud = model_data(pb.build("test", true, None, &Ctl::none()).unwrap());
    for (size, shape) in [(9.0, "square"), (9.0, "round"), (1.0, "round")] {
        let s = scene(vec![item(&cloud, vec![("point_size", Data::Num(size)), ("shape", Data::str(shape))])], vec![("background", rgba(0, 0, 0, 255)), ("msaa", Data::Num(1.0))]);
        let img = render(&g, &r, &frame(&s, &cam), w, h).unwrap();
        save_png(&format!("points_{size}_{shape}.png"), &img, w, h);
        for (p, c) in spots {
            let (x, y) = at(&cam, p, w, h);
            // a 1-pixel point may land on either side of the rounded position
            let hit = (x.saturating_sub(1)..=x + 1).any(|xx| (y.saturating_sub(1)..=y + 1).any(|yy| close(px(&img, w, xx, yy), c, 2)));
            assert!(hit, "size {size} {shape}: {c:?} not at ({x}, {y})");
        }
        let lit = coverage(&img, |c| c[0] > 0 || c[1] > 0 || c[2] > 0) * (w * h) as f64;
        if size > 1.0 {
            // 4 quads of 9 x 9 px; round ones cover pi / 4 of that
            let want = if shape == "square" { 4.0 * 81.0 } else { 4.0 * 81.0 * std::f64::consts::FRAC_PI_4 };
            assert!((lit - want).abs() < want * 0.25, "{shape}: {lit} lit pixels, expected about {want}");
        } else {
            assert!(lit <= 4.0 * 4.0, "1-px points lit {lit} pixels");
        }
    }
    assert_eq!(r.stats().points_drawn, 4);
}

#[test]
fn transparent_background_and_translucent_items_are_premultiplied() {
    let Some(g) = gpu() else { return };
    let r = SceneRenderer::new();
    let (w, h) = (120, 90);
    // one translucent layer: a square facing the camera
    let plane = model_data(synthetic::build_primitive(synthetic::plane(1.0), "plane").unwrap());
    let facing = Data::map([("rotate", Data::nums([90.0, 0.0, 0.0]))]);
    let s = scene(vec![item(&plane, vec![("color", rgba(255, 255, 255, 128)), ("lit", Data::Bool(false)), ("transform", facing)])], vec![]);
    let img = render(&g, &r, &frame(&s, &front_camera(4.0)), w, h).unwrap();
    assert_eq!(px(&img, w, 1, 1), [0, 0, 0, 0]);
    let c = px(&img, w, w / 2, h / 2);
    assert!((c[3] as i32 - 128).abs() <= 1, "alpha {c:?}");
    // premultiplied: linear 1.0 x 0.502 alpha, sRGB-encoded = 188
    assert!((186..=190).contains(&c[0]) && c[0] == c[1] && c[1] == c[2], "{c:?}");
    // a translucent box shows its back faces through the front: two layers, 1 - (1 - a)^2
    let s = scene(vec![item(&cube(1.0), vec![("color", rgba(255, 255, 255, 128)), ("lit", Data::Bool(false))])], vec![]);
    let img = render(&g, &r, &frame(&s, &front_camera(4.0)), w, h).unwrap();
    let c = px(&img, w, w / 2, h / 2);
    let a = 128.0 / 255.0;
    let two = 1.0 - (1.0 - a) * (1.0 - a);
    assert!((c[3] as f64 - two * 255.0).abs() <= 1.5, "alpha {c:?}");
    let enc = (1.055 * f64::powf(two, 1.0 / 2.4) - 0.055) * 255.0;
    assert!((c[0] as f64 - enc).abs() <= 2.0, "{c:?} vs {enc}");
}

#[test]
fn grid_and_axes() {
    let Some(g) = gpu() else { return };
    let r = SceneRenderer::new();
    let (w, h) = (240, 180);
    let cam = Data::map([("yaw", Data::Num(30.0)), ("pitch", Data::Num(30.0)), ("distance", Data::Num(6.0)), ("target", Data::nums([0.0, 0.0, 0.0]))]);
    let small = item(&cube(0.5), vec![("color", rgba(255, 255, 255, 255)), ("transform", Data::map([("translate", Data::nums([0.0, 0.25, 0.0]))]))]);
    let grid = Data::map([("color", rgba(200, 200, 200, 255)), ("size", Data::Num(2.0)), ("step", Data::Num(0.5))]);
    let black = rgba(0, 0, 0, 255);
    let without = render(&g, &r, &frame(&scene(vec![small.clone()], vec![("background", black.clone())]), &cam), w, h).unwrap();
    let with = render(&g, &r, &frame(&scene(vec![small], vec![("background", black), ("grid", grid)]), &cam), w, h).unwrap();
    save_png("grid.png", &with, w, h);
    let grey = |c: [u8; 4]| c[0] > 100 && c[0].abs_diff(c[1]) < 12 && c[1].abs_diff(c[2]) < 12;
    let added = coverage(&with, grey) - coverage(&without, grey);
    assert!(added > 0.01, "grid adds {added} of the view");
    // the X axis is red and starts at the origin
    let reds = coverage(&with, |c| c[0] > 150 && c[1] < 110 && c[2] < 110);
    assert!(reds > 0.0005, "x axis coverage {reds}");
    // the grid point (1, 0, 1) is on a line
    let (x, y) = at(&cam, [1.0, 0.0, 1.0], w, h);
    let near = (x - 1..=x + 1).any(|xx| (y - 1..=y + 1).any(|yy| grey(px(&with, w, xx, yy))));
    assert!(near, "no grid line at ({x}, {y})");
}

#[test]
fn height_ramp_and_ortho() {
    let Some(g) = gpu() else { return };
    let r = SceneRenderer::new();
    let (w, h) = (200, 200);
    let terrain = model_data(synthetic::synthetic_points("terrain", 300_000, 5, &Ctl::none()).unwrap());
    let cam = Data::map([("yaw", Data::Num(0.0)), ("pitch", Data::Num(89.0)), ("ortho", Data::Bool(true))]);
    let s = scene(vec![item(&terrain, vec![("mode", Data::str("height")), ("point_size", Data::Num(2.0))])], vec![("background", rgba(0, 0, 0, 255))]);
    let img = render(&g, &r, &frame(&s, &cam), w, h).unwrap();
    save_png("terrain_height.png", &img, w, h);
    let blue = coverage(&img, |c| c[2] as i32 > c[0] as i32 + 60);
    let red = coverage(&img, |c| c[0] as i32 > c[2] as i32 + 60);
    let lit = coverage(&img, |c| c[0] > 0 || c[1] > 0 || c[2] > 0);
    assert!(blue > 0.02 && red > 0.02, "blue {blue}, red {red}");
    // ortho from the top: the 4 x 4 square fills most of the (auto-fitted) view
    assert!(lit > 0.3, "lit {lit}");
}

#[test]
fn far_coordinates_render_steadily() {
    let Some(g) = gpu() else { return };
    let r = SceneRenderer::new();
    let (w, h) = (160, 160);
    // a 10 m cube of points at the origin, and the same 4000 km away (UTM-like):
    // the two views (each framed automatically) must be the same image
    let cloud_at = |origin: [f64; 3]| {
        let mut pb = PointsBuilder::new(origin, 0).unwrap();
        let mut rng = anypage_scene::model::Rng(9);
        for _ in 0..50_000 {
            let p = [origin[0] + rng.unit() * 10.0 - 5.0, origin[1] + rng.unit() * 10.0 - 5.0, origin[2] + rng.unit() * 10.0 - 5.0];
            pb.push(p, [255, 255, 255, 255]).unwrap();
        }
        model_data(pb.build("test", true, None, &Ctl::none()).unwrap())
    };
    let mut imgs = Vec::new();
    for origin in [[0.0; 3], [500_000.0, 4_000_000.0, 120.0]] {
        let cloud = cloud_at(origin);
        let s = scene(vec![item(&cloud, vec![("point_size", Data::Num(2.0))])], vec![("background", rgba(0, 0, 0, 255)), ("msaa", Data::Num(1.0))]);
        imgs.push(render(&g, &r, &frame(&s, &Data::Nil), w, h).unwrap());
    }
    save_png("far_coordinates.png", &imgs[1], w, h);
    let lit = coverage(&imgs[1], |c| c[0] > 0);
    assert!(lit > 0.15, "lit {lit}");
    let differ = imgs[0].as_chunks::<4>().0.iter().zip(imgs[1].as_chunks::<4>().0).filter(|(a, b)| a != b).count() as f64 / (w * h) as f64;
    assert!(differ < 0.002, "{:.4} of the pixels differ between the near and the far copy", differ);
}

#[test]
fn budget_chunks_and_cache() {
    let Some(g) = gpu() else { return };
    let r = Arc::new(SceneRenderer::new());
    let (w, h) = (128, 128);
    let n = 5_000_000;
    let cloud = model_data(synthetic::synthetic_points("cube", n, 1, &Ctl::none()).unwrap());
    let cam = Data::map([("yaw", Data::Num(20.0)), ("pitch", Data::Num(20.0))]);
    let with_budget = |b: f64| scene(vec![item(&cloud, vec![])], vec![("point_budget", Data::Num(b))]);
    render(&g, &*r, &frame(&with_budget(1_000_000.0), &cam), w, h).unwrap();
    let st = r.stats();
    assert_eq!(st.points_drawn, 1_000_000);
    assert_eq!(st.points_total, n);
    // only the first chunk (4 Mi points of 16 bytes) went to the GPU
    assert_eq!(st.uploaded_bytes, (4u64 << 20) * 16);
    render(&g, &*r, &frame(&with_budget(1e9), &cam), w, h).unwrap();
    let st = r.stats();
    assert_eq!(st.points_drawn, n);
    assert_eq!(st.uploaded_bytes, (n as u64 - (4 << 20)) * 16);
    render(&g, &*r, &frame(&with_budget(1e9), &cam), w, h).unwrap();
    assert_eq!(r.stats().uploaded_bytes, 0, "cached");
    assert_eq!(r.stats().gpu_bytes, n as u64 * 16);
    // once nothing holds the model, its buffers go
    drop(cloud);
    render(&g, &*r, &frame(&scene(vec![], vec![]), &cam), w, h).unwrap();
    assert_eq!(r.stats().gpu_bytes, 0);
}

#[test]
fn bad_values_are_errors_not_panics() {
    let Some(g) = gpu() else { return };
    let r = SceneRenderer::new();
    let e = render(&g, &r, &val(5u32), 16, 16).unwrap_err();
    assert!(e.contains("not a scene frame"), "{e}");
    assert!(Frame::from_inputs(&[Data::map([("items", Data::list(vec![Data::map([("model", Data::Num(1.0))])]))])]).unwrap_err().contains("scene.model"));
    assert!(Frame::from_inputs(&[Data::map([("items", Data::list(vec![item(&cube(1.0), vec![("transform", Data::nums([1.0, 2.0]))])]))])]).unwrap_err().contains("16 numbers"));
    // a model still loading (a pending job) is skipped, not an error
    let pending = Data::map([("state", Data::str("pending")), ("progress", Data::Num(0.5))]);
    let f = Frame::from_inputs(&[Data::map([("items", Data::list(vec![item(&pending, vec![])]))])]).unwrap();
    assert_eq!((f.items.len(), f.pending), (0, 1));
    render(&g, &r, &val(f), 16, 16).unwrap();
}

#[test]
fn failed_gpu_allocations_are_errors() {
    let Some(g) = gpu() else { return };
    // a buffer the device refuses: an Err from inside the error scopes, no
    // uncaptured error (the default handler panics), no use of the object
    let too_big = g.device.limits().max_buffer_size + 4;
    let e = anypage_scene::render::guarded(&g.device, "a test buffer", || {
        g.device.create_buffer(&anypage_engine::wgpu::BufferDescriptor { label: None, size: too_big, usage: anypage_engine::wgpu::BufferUsages::VERTEX, mapped_at_creation: true })
    })
    .unwrap_err();
    assert!(e.contains("creating a test buffer"), "{e}");
    // the device and the renderer carry on
    let r = SceneRenderer::new();
    let img = render(&g, &r, &frame(&scene(vec![item(&cube(1.0), vec![("color", rgba(255, 0, 0, 255))])], vec![]), &front_camera(3.0)), 32, 32).unwrap();
    assert_eq!(px(&img, 32, 16, 16), [255, 0, 0, 255]);
}

/// 10 million points: generation, first render (upload) and redraw times,
/// as a point list and as 2-pixel quads, at 1080p. Skipped on software
/// rasterisers. `cargo test --release --test gpu -- --nocapture ten_million`
#[test]
fn ten_million_points() {
    let Some(g) = gpu() else { return };
    if g.software {
        eprintln!("SKIPPED: 10M points on a software rasteriser ({})", g.info);
        return;
    }
    let r = SceneRenderer::new();
    let (w, h) = (1920, 1080);
    let n = 10_000_000;
    let t = Instant::now();
    let cloud = model_data(synthetic::synthetic_points("terrain", n, 2, &Ctl::none()).unwrap());
    let gen_ms = t.elapsed().as_secs_f64() * 1000.0;
    // what a loader pays to make prefixes uniform samples
    let mut copy = anypage_scene::frame::model_of(&cloud).unwrap().0.points.clone();
    let t = Instant::now();
    anypage_scene::model::shuffle_points(&mut copy, 7, &Ctl::none()).unwrap();
    println!("shuffling 10M points: {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    drop(copy);
    let cam = Data::map([("yaw", Data::Num(30.0)), ("pitch", Data::Num(35.0))]);
    let target = target(&g, w, h);
    for (label, size) in [("point list", 1.0), ("quads 2 px", 2.0)] {
        let s = scene(vec![item(&cloud, vec![("point_size", Data::Num(size))])], vec![("background", rgba(0, 0, 0, 255))]);
        let v = frame(&s, &cam);
        let mut times = Vec::new();
        let mut first = None;
        for _ in 0..4 {
            let t = Instant::now();
            render_into(&g, &r, &v, &target).unwrap();
            let cpu = r.stats();
            wait(&g);
            times.push(t.elapsed().as_secs_f64() * 1000.0);
            first.get_or_insert(cpu);
        }
        let st = r.stats();
        let f = first.unwrap_or_default();
        println!(
            "10M {label} on {}: first frame {:.1} ms to GPU idle (CPU {:.1} ms, of which upload {:.1} ms for {} MB), then {:.1} / {:.1} / {:.1} ms (CPU {:.2} ms); drawn {}",
            g.info,
            times[0],
            f.ms,
            f.upload_ms,
            f.uploaded_bytes >> 20,
            times[1],
            times[2],
            times[3],
            st.ms,
            st.points_drawn
        );
        assert_eq!(st.points_drawn, n);
        assert_eq!(st.uploaded_bytes, 0);
    }
    let img = read(&g, &target);
    save_png("ten_million.png", &img, w, h);
    let lit = coverage(&img, |c| c[0] > 0 || c[1] > 0 || c[2] > 0);
    println!("generated in {gen_ms:.0} ms; lit {lit:.3} of the 1080p view");
    assert!(lit > 0.05);
}
