//! Real-camera broadcast. Ignored by default because it switches on the
//! machine's webcam; run it by hand:
//! `cargo test --features camera --test camera -- --ignored --nocapture`.

#![cfg(all(windows, feature = "camera"))]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anypage_engine::Image;
use anypage_media::broadcast::{BroadcastRequest, Broadcaster, Host, Source};
use anypage_media::player::{Out, PlayRequest, Player};
use anypage_runtime::{Caps, Data, Grants};

#[test]
#[ignore = "opens the real webcam"]
fn camera_to_hls_and_back() {
    let names = anypage_media::camera::list().expect("camera list");
    if names.is_empty() {
        println!("no camera: nothing to test");
        return;
    }
    println!("using camera 0: {}", names[0]);
    let g = Arc::new(Grants::new("cam", Caps { net: vec!["127.0.0.1".into()], ..Caps::default() }));
    let on = Arc::new(AtomicBool::new(true));
    let quit = Arc::new(AtomicBool::new(false));
    let b_out: Out = Arc::new(|d: Data| {
        if let Some(e) = d.get("error").as_str() {
            println!("broadcast error: {e}");
        }
        true
    });
    let req = BroadcastRequest { source: Source::Camera(0), width: 640, height: 480, segment_seconds: 1.0, serve: Some(0), host: Host::Loopback, ..BroadcastRequest::default() };
    let mut b = Broadcaster::start(req, &g, b_out, on.clone(), quit.clone()).expect("camera broadcast starts");
    let url = b.local_url.clone().unwrap();
    let last: Arc<Mutex<Option<Data>>> = Arc::new(Mutex::new(None));
    let l2 = last.clone();
    let p_out: Out = Arc::new(move |d: Data| {
        if d.get("frame").as_res().is_some() {
            *l2.lock().unwrap() = Some(d);
        }
        true
    });
    let mut p = Player::start(PlayRequest { live_edge: 2, ..PlayRequest::new(&url) }, g, p_out, on, quit).unwrap();
    common::wait_for(30.0, "camera frames played back", || (p.stats.presented.load(Ordering::Relaxed) >= 30).then_some(()));
    p.stop();
    b.stop();
    let d = last.lock().unwrap().clone().unwrap();
    let img = d.get("frame").as_res().unwrap().get::<Image>().unwrap();
    let path = common::save_png("camera_playback.png", img.w, img.h, &img.rgba);
    // a real picture is not one flat colour
    let (mut lo, mut hi) = (255u8, 0u8);
    for px in img.rgba.as_chunks::<4>().0.iter().step_by(97) {
        lo = lo.min(px[1]);
        hi = hi.max(px[1]);
    }
    println!("{}x{} frame saved to {}", img.w, img.h, path.display());
    assert!(hi - lo > 20, "the camera picture has detail");
}
