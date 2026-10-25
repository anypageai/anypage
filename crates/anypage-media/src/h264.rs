//! H.264: Annex B helpers, an SPS reader (size, colour description, frame
//! rate), and encoder/decoder wrappers over OpenH264 that speak I420 in and
//! RGBA8 out.

use openh264::decoder::{Decoder, DecoderConfig, Flush};
use openh264::encoder::{BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile, RateControlMode, UsageType, VuiConfig};
use openh264::formats::YUVSource;
use openh264::OpenH264API;

use crate::yuv::{i420_to_rgba_auto, ColorSpace, Matrix, Planes, I420};

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// Largest picture accepted from a stream (OpenH264's own limit is level 5.2).
pub const MAX_PIXELS: usize = 4096 * 2304;

/// NAL units of an Annex B byte stream, without their start codes.
pub fn nal_units(b: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 3 <= b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map(|&n| n - 3).unwrap_or(b.len());
        // a 4-byte start code leaves a zero byte at the end of the previous unit
        while end > s && b[end - 1] == 0 && k + 1 < starts.len() {
            end -= 1;
        }
        if end > s {
            units.push(&b[s..end]);
        }
    }
    units.into_iter()
}

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1F).unwrap_or(0)
}

/// Does this access unit hold an IDR picture?
pub fn is_keyframe(annexb: &[u8]) -> bool {
    nal_units(annexb).any(|n| nal_type(n) == NAL_IDR)
}

/// What a sequence parameter set says about the pictures.
#[derive(Clone, Debug, PartialEq)]
pub struct Sps {
    pub profile_idc: u8,
    pub level_idc: u8,
    pub width: usize,
    pub height: usize,
    /// VUI `matrix_coefficients`, when signalled.
    pub matrix: Option<u8>,
    pub full_range: bool,
    /// From VUI timing info, when signalled.
    pub fps: Option<f64>,
}

impl Sps {
    pub fn color_space(&self) -> ColorSpace {
        ColorSpace::from_vui(self.matrix, self.full_range, self.height)
    }
    pub fn profile_name(&self) -> &'static str {
        match self.profile_idc {
            66 => "baseline",
            77 => "main",
            88 => "extended",
            100 => "high",
            110 => "high10",
            122 => "high422",
            244 => "high444",
            _ => "other",
        }
    }
}

/// Bit reader over an RBSP (emulation prevention bytes removed).
struct Bits {
    data: Vec<u8>,
    pos: usize,
}

impl Bits {
    fn new(nal_payload: &[u8]) -> Bits {
        let mut data = Vec::with_capacity(nal_payload.len());
        let mut zeros = 0;
        for &b in nal_payload {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            data.push(b);
        }
        Bits { data, pos: 0 }
    }
    fn bit(&mut self) -> Result<u32, String> {
        let byte = self.data.get(self.pos / 8).ok_or("SPS ends early")?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(b as u32)
    }
    fn bits(&mut self, n: u32) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }
    fn ue(&mut self) -> Result<u32, String> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err("bad Exp-Golomb code in SPS".into());
            }
        }
        Ok(((1u64 << zeros) - 1 + self.bits(zeros)? as u64) as u32)
    }
    fn se(&mut self) -> Result<i64, String> {
        let k = self.ue()? as i64;
        Ok(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

/// Parse an SPS NAL unit (with its one-byte header).
pub fn parse_sps(nal: &[u8]) -> Result<Sps, String> {
    if nal_type(nal) != NAL_SPS || nal.len() < 4 {
        return Err("not an SPS".into());
    }
    let mut r = Bits::new(&nal[1..]);
    let profile_idc = r.bits(8)? as u8;
    let _constraints = r.bits(8)?;
    let level_idc = r.bits(8)? as u8;
    let _id = r.ue()?;
    let mut chroma_format_idc = 1;
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.bit()?;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bit()?; // qpprime_y_zero_transform_bypass
        if r.bit()? == 1 {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i64, 8i64);
                    for _ in 0..size {
                        if next != 0 {
                            let delta = r.se()?;
                            next = (last + delta).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            let n = r.ue()?;
            if n > 255 {
                return Err("bad SPS (poc cycle)".into());
            }
            for _ in 0..n {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?; // gaps_in_frame_num_value_allowed
    // all size arithmetic in u64 (the fields are up to 2^32 - 2 each)
    let w_mbs = r.ue()? as u64 + 1;
    let h_map = r.ue()? as u64 + 1;
    let frame_mbs_only = r.bit()? as u64;
    if frame_mbs_only == 0 {
        r.bit()?;
    }
    r.bit()?; // direct_8x8_inference
    let (mut cl, mut cr, mut ct, mut cb) = (0u64, 0u64, 0u64, 0u64);
    if r.bit()? == 1 {
        cl = r.ue()? as u64;
        cr = r.ue()? as u64;
        ct = r.ue()? as u64;
        cb = r.ue()? as u64;
    }
    let (sub_w, sub_h) = match chroma_format_idc {
        1 => (2u64, 2u64),
        2 => (2, 1),
        _ => (1, 1),
    };
    let crop_x = if chroma_format_idc == 0 { 1 } else { sub_w };
    let crop_y = (if chroma_format_idc == 0 { 1 } else { sub_h }) * (2 - frame_mbs_only);
    let full_w = w_mbs * 16;
    let full_h = (2 - frame_mbs_only) * h_map * 16;
    let width = full_w.checked_sub(crop_x * (cl + cr)).ok_or("bad SPS cropping")?;
    let height = full_h.checked_sub(crop_y * (ct + cb)).ok_or("bad SPS cropping")?;
    let pixels = width.checked_mul(height);
    if width == 0 || height == 0 || pixels.is_none_or(|p| p > MAX_PIXELS as u64) {
        return Err(format!("unsupported picture size {width}x{height}"));
    }
    // both fit: their product is at most MAX_PIXELS
    let (width, height) = (width as usize, height as usize);
    let mut sps = Sps { profile_idc, level_idc, width, height, matrix: None, full_range: false, fps: None };
    // VUI: everything after this point is optional; a short SPS is not an error
    let vui = (|| -> Result<(), String> {
        if r.bit()? == 0 {
            return Ok(());
        }
        if r.bit()? == 1 && r.bits(8)? == 255 {
            r.bits(16)?;
            r.bits(16)?;
        }
        if r.bit()? == 1 {
            r.bit()?;
        }
        if r.bit()? == 1 {
            r.bits(3)?; // video_format
            sps.full_range = r.bit()? == 1;
            if r.bit()? == 1 {
                r.bits(8)?;
                r.bits(8)?;
                sps.matrix = Some(r.bits(8)? as u8);
            }
        }
        if r.bit()? == 1 {
            r.ue()?;
            r.ue()?;
        }
        if r.bit()? == 1 {
            let units = r.bits(32)?;
            let scale = r.bits(32)?;
            if units > 0 && scale > 0 {
                sps.fps = Some(scale as f64 / (2.0 * units as f64));
            }
        }
        Ok(())
    })();
    let _ = vui;
    Ok(sps)
}

/// The SPS of an access unit, if it carries one.
pub fn find_sps(annexb: &[u8]) -> Option<Sps> {
    nal_units(annexb).find(|n| nal_type(n) == NAL_SPS).and_then(|n| parse_sps(n).ok())
}

// ---------------------------------------------------------------- encoder

#[derive(Clone, Copy, Debug)]
pub struct EncoderSettings {
    pub width: usize,
    pub height: usize,
    pub fps: f64,
    pub bitrate: u32,
    /// Frames between IDR pictures (0: only when forced).
    pub keyframe_interval: u32,
    pub threads: u16,
}

/// H.264 constrained-baseline encoder (no B-frames: decode order is display
/// order, which keeps live latency low and every segment independent).
pub struct VideoEncoder {
    enc: Encoder,
    pub settings: EncoderSettings,
    pub color: ColorSpace,
    pub frames: u64,
}

impl VideoEncoder {
    pub fn new(s: EncoderSettings) -> Result<Self, String> {
        if s.width < 16 || s.height < 16 || s.width % 2 == 1 || s.height % 2 == 1 {
            return Err(format!("video size {}x{} must be even and at least 16x16", s.width, s.height));
        }
        if s.width > 3840 || s.height > 2160 {
            return Err(format!("video size {}x{} is above the encoder's 3840x2160 limit", s.width, s.height));
        }
        let color = ColorSpace::for_height(s.height);
        let vui = if color.matrix == Matrix::Bt709 { VuiConfig::bt709() } else { VuiConfig::bt601() };
        let cfg = EncoderConfig::new()
            .bitrate(BitRate::from_bps(s.bitrate.clamp(50_000, 50_000_000)))
            .max_frame_rate(FrameRate::from_hz(s.fps.clamp(1.0, 120.0) as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(false)
            .usage_type(UsageType::CameraVideoRealTime)
            .profile(Profile::Baseline)
            .complexity(Complexity::Low)
            .intra_frame_period(IntraFramePeriod::from_num_frames(s.keyframe_interval))
            .num_threads(s.threads)
            .vui(vui);
        let enc = Encoder::with_api_config(OpenH264API::from_source(), cfg).map_err(|e| format!("H.264 encoder: {e}"))?;
        Ok(Self { enc, settings: s, color, frames: 0 })
    }

    /// Encode one picture; returns the access unit (Annex B) and whether it
    /// is an IDR, or `None` when the encoder produced nothing for it.
    pub fn encode(&mut self, pic: &I420, force_idr: bool) -> Result<Option<(Vec<u8>, bool)>, String> {
        if pic.width != self.settings.width || pic.height != self.settings.height {
            return Err(format!("picture {}x{} does not match the encoder's {}x{}", pic.width, pic.height, self.settings.width, self.settings.height));
        }
        if force_idr && self.frames > 0 {
            self.enc.force_intra_frame();
        }
        let ms = (self.frames as f64 * 1000.0 / self.settings.fps.max(1.0)) as u64;
        let bs = self.enc.encode_at(pic, openh264::Timestamp::from_millis(ms)).map_err(|e| format!("H.264 encode: {e}"))?;
        self.frames += 1;
        let kind = bs.frame_type();
        if matches!(kind, FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        let data = bs.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        let key = kind == FrameType::IDR || is_keyframe(&data);
        Ok(Some((data, key)))
    }
}

// ---------------------------------------------------------------- decoder

/// A decoded picture: compact I420 (a third of the RGBA size while it waits
/// in a queue; frames dropped as late are never converted).
pub struct DecodedFrame {
    pub pts: u64,
    pub pic: I420,
    pub color: ColorSpace,
    /// When the decoder produced it (becomes `Image::newest`).
    pub decoded_at: std::time::Instant,
}

impl DecodedFrame {
    pub fn width(&self) -> usize {
        self.pic.width
    }
    pub fn height(&self) -> usize {
        self.pic.height
    }
    pub fn to_rgba(&self) -> Result<Vec<u8>, String> {
        let (w, h) = (self.pic.width, self.pic.height);
        let mut rgba = vec![0u8; w * h * 4];
        i420_to_rgba_auto(&self.pic.planes(), self.color, &mut rgba)?;
        Ok(rgba)
    }
    /// Approximate memory held, for queue limits.
    pub fn bytes(&self) -> usize {
        self.pic.y.len() + self.pic.u.len() + self.pic.v.len()
    }
}

/// Copy decoder-owned planes into a tight I420 picture.
pub fn copy_planes(p: &Planes) -> Result<I420, String> {
    p.check()?;
    let (w, h) = (p.width, p.height);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut out = I420 { width: w, height: h, y: Vec::with_capacity(w * h), u: Vec::with_capacity(cw * ch), v: Vec::with_capacity(cw * ch) };
    for r in 0..h {
        out.y.extend_from_slice(&p.y[r * p.y_stride..r * p.y_stride + w]);
    }
    for r in 0..ch {
        out.u.extend_from_slice(&p.u[r * p.uv_stride..r * p.uv_stride + cw]);
        out.v.extend_from_slice(&p.v[r * p.uv_stride..r * p.uv_stride + cw]);
    }
    Ok(out)
}

/// H.264 decoder that keeps presentation timestamps attached to pictures:
/// pictures leave the decoder in presentation order, so each one takes the
/// smallest timestamp still waiting.
pub struct VideoDecoder {
    dec: Decoder,
    sps: Option<Sps>,
    pending: Vec<u64>,
    /// Use this colour space instead of the stream's (tests, odd streams).
    pub force_color: Option<ColorSpace>,
    pub errors: u64,
}

impl VideoDecoder {
    pub fn new() -> Result<Self, String> {
        let cfg = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
        let dec = Decoder::with_api_config(OpenH264API::from_source(), cfg).map_err(|e| format!("H.264 decoder: {e}"))?;
        Ok(Self { dec, sps: None, pending: Vec::new(), force_color: None, errors: 0 })
    }

    pub fn sps(&self) -> Option<&Sps> {
        self.sps.as_ref()
    }

    fn convert(sps: Option<&Sps>, force: Option<ColorSpace>, yuv: &impl YUVSource, pts: u64) -> Result<DecodedFrame, String> {
        let (w, h) = yuv.dimensions();
        if w == 0 || h == 0 || w.checked_mul(h).is_none_or(|p| p > MAX_PIXELS) {
            return Err(format!("decoded picture {w}x{h} is out of range"));
        }
        let (ys, us, _) = yuv.strides();
        let planes = Planes { y: yuv.y(), u: yuv.u(), v: yuv.v(), y_stride: ys, uv_stride: us, width: w, height: h };
        let color = force.unwrap_or_else(|| sps.map(Sps::color_space).unwrap_or_else(|| ColorSpace::for_height(h)));
        Ok(DecodedFrame { pts, pic: copy_planes(&planes)?, color, decoded_at: std::time::Instant::now() })
    }

    fn take_pts(&mut self) -> u64 {
        if self.pending.is_empty() {
            return 0;
        }
        let (i, _) = self.pending.iter().enumerate().min_by_key(|(_, p)| **p).unwrap_or((0, &0));
        self.pending.swap_remove(i)
    }

    /// Decode one access unit (Annex B) presented at `pts`.
    pub fn decode(&mut self, annexb: &[u8], pts: u64) -> Result<Vec<DecodedFrame>, String> {
        if let Some(sps) = find_sps(annexb) {
            self.sps = Some(sps);
        }
        self.pending.push(pts);
        if self.pending.len() > 32 {
            // pictures that never came out (damaged input): forget the oldest
            let p = self.take_pts();
            let _ = p;
        }
        let r = self.dec.decode(annexb);
        match r {
            Ok(Some(yuv)) => {
                let (sps, force) = (self.sps.clone(), self.force_color);
                let frame = Self::convert(sps.as_ref(), force, &yuv, 0);
                let pts = self.take_pts();
                frame.map(|mut f| {
                    f.pts = pts;
                    vec![f]
                })
            }
            Ok(None) => Ok(Vec::new()),
            Err(e) => {
                self.errors += 1;
                if let Some(i) = self.pending.iter().rposition(|p| *p == pts) {
                    self.pending.swap_remove(i);
                }
                Err(format!("H.264 decode: {e}"))
            }
        }
    }

    /// Pictures still inside the decoder (end of stream, discontinuity).
    pub fn flush(&mut self) -> Vec<DecodedFrame> {
        let (sps, force) = (self.sps.clone(), self.force_color);
        let mut out = Vec::new();
        let frames: Vec<Result<DecodedFrame, String>> = match self.dec.flush_remaining() {
            Ok(list) => list.iter().map(|y| Self::convert(sps.as_ref(), force, y, 0)).collect(),
            Err(_) => Vec::new(),
        };
        for f in frames.into_iter().flatten() {
            let pts = self.take_pts();
            out.push(DecodedFrame { pts, ..f });
        }
        self.pending.clear();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b() {
        let b = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 0];
        let units: Vec<&[u8]> = nal_units(&b).collect();
        assert_eq!(units, vec![&[0x67u8, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9, 0][..]]);
        assert!(is_keyframe(&b));
        assert!(!is_keyframe(&b[..12]));
        assert_eq!(nal_units(&[1, 2, 3]).count(), 0);
    }

    #[test]
    fn reads_an_sps() {
        // 1280x720 baseline with BT.709 VUI, made by our own encoder
        let mut enc = VideoEncoder::new(EncoderSettings { width: 1280, height: 720, fps: 30.0, bitrate: 1_000_000, keyframe_interval: 30, threads: 1 }).unwrap();
        let (au, key) = enc.encode(&I420::new(1280, 720), false).unwrap().unwrap();
        assert!(key);
        let sps = find_sps(&au).expect("an IDR carries the SPS");
        assert_eq!((sps.width, sps.height), (1280, 720));
        assert_eq!(sps.matrix, Some(1));
        assert_eq!(sps.color_space(), ColorSpace::BT709);
        assert_eq!(sps.profile_name(), "baseline");
        // odd crop: 1920x1080 is coded as 1920x1088 with cropping
        let mut enc = VideoEncoder::new(EncoderSettings { width: 1920, height: 1080, fps: 25.0, bitrate: 2_000_000, keyframe_interval: 0, threads: 1 }).unwrap();
        let (au, _) = enc.encode(&I420::new(1920, 1080), false).unwrap().unwrap();
        let sps = find_sps(&au).unwrap();
        assert_eq!((sps.width, sps.height), (1920, 1080));
        assert!(parse_sps(&[0x67]).is_err());
        assert!(parse_sps(&[0x67, 100, 0, 30]).is_err(), "truncated");
    }

    /// Exp-Golomb bit writer for hand-made SPS units.
    struct Bw(Vec<u8>);
    impl Bw {
        fn put(&mut self, v: u64, n: u32) {
            for k in (0..n).rev() {
                self.0.push(((v >> k) & 1) as u8);
            }
        }
        fn ue(&mut self, v: u64) {
            let x = v + 1;
            let len = 64 - x.leading_zeros();
            self.put(0, len - 1);
            self.put(x, len);
        }
        fn se(&mut self, v: i64) {
            self.ue(if v > 0 { 2 * v as u64 - 1 } else { (-2 * v) as u64 });
        }
        /// RBSP bytes with emulation prevention, behind an SPS NAL header.
        fn nal(&self) -> Vec<u8> {
            let mut bytes = Vec::new();
            for c in self.0.chunks(8) {
                bytes.push(c.iter().enumerate().fold(0u8, |b, (i, bit)| b | (bit << (7 - i))));
            }
            let mut out = vec![0x67];
            let mut zeros = 0;
            for b in bytes {
                if zeros >= 2 && b <= 3 {
                    out.push(3);
                    zeros = 0;
                }
                zeros = if b == 0 { zeros + 1 } else { 0 };
                out.push(b);
            }
            out
        }
    }

    fn sps_with(profile: u64, w_mbs_minus1: u64, h_map_minus1: u64, crop: Option<[u64; 4]>) -> Vec<u8> {
        let mut w = Bw(Vec::new());
        w.put(profile, 8);
        w.put(0, 8);
        w.put(30, 8);
        w.ue(0); // sps id
        if profile == 100 {
            w.ue(1); // chroma_format_idc
            w.ue(0);
            w.ue(0);
            w.put(0, 1);
            w.put(1, 1); // scaling matrices present
            w.put(1, 1); // list 0 present
            w.se(i32::MAX as i64); // a delta far outside -128..127
            for _ in 1..16 {
                w.se(0);
            }
            for _ in 1..8 {
                w.put(0, 1);
            }
        }
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(2); // poc type
        w.ue(1); // max refs
        w.put(0, 1); // gaps
        w.ue(w_mbs_minus1);
        w.ue(h_map_minus1);
        w.put(1, 1); // frame_mbs_only
        w.put(1, 1); // direct_8x8
        match crop {
            Some(c) => {
                w.put(1, 1);
                for v in c {
                    w.ue(v);
                }
            }
            None => w.put(0, 1),
        }
        w.put(0, 1); // no VUI
        w.put(1, 1); // stop bit
        w.nal()
    }

    #[test]
    fn hostile_sps_values_are_errors_not_panics() {
        // the reviewer's case: width and height in macroblocks of 2^32 - 1
        let huge = (1u64 << 32) - 2;
        let e = parse_sps(&sps_with(66, huge, huge, None)).unwrap_err();
        assert!(e.contains("unsupported picture size"), "{e}");
        assert!(parse_sps(&sps_with(66, huge, 0, None)).is_err());
        assert!(parse_sps(&sps_with(66, 0, huge, None)).is_err());
        // cropping larger than the picture, and cropping fields at their maximum
        assert!(parse_sps(&sps_with(66, 79, 44, Some([700, 0, 0, 0]))).unwrap_err().contains("cropping"));
        assert!(parse_sps(&sps_with(66, 79, 44, Some([huge, huge, huge, huge]))).is_err());
        // a scaling-list delta that overflowed i32 arithmetic
        let s = parse_sps(&sps_with(100, 79, 44, None)).unwrap();
        assert_eq!((s.width, s.height, s.profile_name()), (1280, 720, "high"));
        // the plain case still reads
        let s = parse_sps(&sps_with(66, 79, 44, Some([0, 0, 0, 4]))).unwrap();
        assert_eq!((s.width, s.height), (1280, 712));
        // and the whole stream path (probe) survives it
        let mut au = vec![0, 0, 0, 1];
        au.extend(sps_with(66, huge, huge, None));
        assert!(find_sps(&au).is_none());
    }

    #[test]
    fn rejects_bad_sizes() {
        let s = EncoderSettings { width: 15, height: 16, fps: 30.0, bitrate: 100_000, keyframe_interval: 0, threads: 1 };
        assert!(VideoEncoder::new(s).is_err());
        let s = EncoderSettings { width: 5000, height: 2000, ..s };
        assert!(VideoEncoder::new(s).is_err());
    }

    #[test]
    fn decoder_survives_garbage() {
        let mut d = VideoDecoder::new().unwrap();
        for junk in [&[][..], &[0, 0, 1], &[0, 0, 0, 1, 0x65, 0xFF, 0xFF, 0x00, 0x13], &[0x47; 400]] {
            let _ = d.decode(junk, 0);
        }
        let _ = d.flush();
    }
}
