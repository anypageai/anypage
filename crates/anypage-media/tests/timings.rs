//! Encode and decode timings at 720p and 1080p, with a quality check of the
//! BT.709 path. Run with
//! `cargo test --release --test timings -- --nocapture --test-threads=1`
//! for representative numbers (a debug build compiles this crate at -O1;
//! parallel tests share the CPU). Note the machine's load with them: the
//! numbers move by a factor of 4 to 6 between an idle and a saturated CPU.

mod common;

use std::time::Instant;

use anypage_media::h264::{EncoderSettings, VideoDecoder, VideoEncoder};
use anypage_media::pattern::test_pattern;
use anypage_media::ts::{demux_all, Muxer, CLOCK};
use anypage_media::yuv::{i420_to_rgba, i420_to_rgba_auto, psnr_rgba, threads_for, ColorSpace};

fn run(w: usize, h: usize, bitrate: u32, frames: u64) {
    let fps = 30.0;
    let mut enc = VideoEncoder::new(EncoderSettings { width: w, height: h, fps, bitrate, keyframe_interval: 60, threads: 1 }).unwrap();
    assert_eq!(enc.color, ColorSpace::BT709, "HD uses BT.709 and says so in the VUI");
    let pics: Vec<_> = (0..frames).map(|n| test_pattern(w, h, n, fps, enc.color)).collect();
    let t_pattern = {
        let t = Instant::now();
        let _ = test_pattern(w, h, 7, fps, enc.color);
        t.elapsed().as_secs_f64() * 1000.0
    };
    let mut mux = Muxer::new(true, false);
    let mut ts = Vec::new();
    mux.write_tables(&mut ts);
    let t0 = Instant::now();
    let mut bytes = 0usize;
    for (n, pic) in pics.iter().enumerate() {
        if let Some((au, key)) = enc.encode(pic, false).unwrap() {
            bytes += au.len();
            mux.write_video(&mut ts, CLOCK + n as u64 * 3000, None, key, &au);
        }
    }
    let enc_ms = t0.elapsed().as_secs_f64() * 1000.0 / frames as f64;

    let (_, pes, errors) = demux_all(&ts);
    assert_eq!(errors, 0);
    assert_eq!(pes.len() as u64, frames);
    let mut dec = VideoDecoder::new().unwrap();
    let t1 = Instant::now();
    let mut decoded = Vec::new();
    for p in &pes {
        decoded.extend(dec.decode(&p.data, p.pts.unwrap()).unwrap());
    }
    decoded.extend(dec.flush());
    let dec_ms = t1.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    assert_eq!(decoded.len() as u64, frames);

    // what the player does per shown frame (thread count chosen by measurement)
    let t2 = Instant::now();
    let mut used = std::collections::BTreeMap::<usize, usize>::new();
    let rgba: Vec<Vec<u8>> = decoded
        .iter()
        .map(|f| {
            let mut out = vec![0u8; w * h * 4];
            *used.entry(i420_to_rgba_auto(&f.pic.planes(), f.color, &mut out).unwrap()).or_default() += 1;
            out
        })
        .collect();
    let conv_ms = t2.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    // one thread, best of 10
    let mut single = vec![0u8; w * h * 4];
    let conv1_ms = (0..10)
        .map(|_| {
            let t = Instant::now();
            i420_to_rgba(&decoded[0].pic.planes(), decoded[0].color, &mut single, 1).unwrap();
            t.elapsed().as_secs_f64() * 1000.0
        })
        .fold(f64::MAX, f64::min);

    // quality and timestamps
    let mut worst = f64::MAX;
    for (k, f) in decoded.iter().enumerate() {
        assert_eq!(f.pts, CLOCK + k as u64 * 3000, "timestamps stay attached to pictures");
        assert_eq!(f.color, ColorSpace::BT709);
        if k % 10 == 0 {
            let mut reference = vec![0u8; w * h * 4];
            i420_to_rgba(&pics[k].planes(), ColorSpace::BT709, &mut reference, 4).unwrap();
            worst = worst.min(psnr_rgba(&rgba[k], &reference));
        }
    }
    let name = format!("timing_{w}x{h}.png");
    common::save_png(&name, w as u32, h as u32, &rgba[frames as usize / 2]);
    println!(
        "{w}x{h}: encode {enc_ms:.2} ms/frame, decode {dec_ms:.2} ms/frame, YUV->RGBA {conv_ms:.2} ms/frame (frames per thread count {used:?}, up to {}; best single-thread {conv1_ms:.2} ms), pattern {t_pattern:.2} ms, {:.0} kbit/s, worst PSNR {worst:.1} dB",
        threads_for(w * h),
        bytes as f64 * 8.0 * fps / frames as f64 / 1000.0
    );
    assert!(worst > 30.0, "{w}x{h}: PSNR {worst:.1}");
}

#[test]
fn h264_720p() {
    run(1280, 720, 3_000_000, 60);
}

#[test]
fn h264_1080p() {
    run(1920, 1080, 6_000_000, 60);
}
