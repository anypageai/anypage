//! Audio for `media.hls`: AAC-LC (ADTS in MPEG-TS) decoded with symphonia
//! and played on the default output with cpal, behind the `audio` feature.
//! The pipe also provides the audio clock the video presenter follows while
//! sound is playing. Without the feature the pipe only notes that the stream
//! had audio.
//!
//! What plays: ADTS object type 2 (AAC-LC), mono or stereo. Other object
//! types and more than two channels are refused with a clear status.
//! HE-AAC (v1 and v2) in ADTS is *not* detected or refused: it is signalled
//! as AAC-LC at the core sample rate with implicit SBR, and symphonia decodes
//! the LC core only. Such streams play at the right speed, but with the
//! reduced bandwidth of the core (no SBR), and HE-AACv2 parametric stereo
//! plays as mono.

/// One ADTS frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Adts {
    /// MPEG-4 audio object type (2 = AAC-LC).
    pub object_type: u8,
    pub sample_rate_index: u8,
    pub sample_rate: u32,
    pub channels: u8,
    /// Header length (7, or 9 with CRC).
    pub header_len: usize,
    /// Whole frame length including the header.
    pub frame_len: usize,
    pub raw_blocks: u8,
}

pub const SAMPLE_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

pub fn parse_adts(b: &[u8]) -> Option<Adts> {
    if b.len() < 7 || b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
        return None;
    }
    let protection_absent = b[1] & 1 == 1;
    let object_type = ((b[2] >> 6) & 3) + 1;
    let sample_rate_index = (b[2] >> 2) & 0x0F;
    let channels = ((b[2] & 1) << 2) | (b[3] >> 6);
    let frame_len = (((b[3] & 3) as usize) << 11) | (b[4] as usize) << 3 | (b[5] >> 5) as usize;
    let header_len = if protection_absent { 7 } else { 9 };
    let sample_rate = *SAMPLE_RATES.get(sample_rate_index as usize)?;
    if frame_len < header_len {
        return None;
    }
    Some(Adts { object_type, sample_rate_index, sample_rate, channels, header_len, frame_len, raw_blocks: (b[6] & 3) + 1 })
}

/// Split a PES payload into ADTS frames (header, raw AAC payload).
pub fn adts_frames(mut b: &[u8]) -> Vec<(Adts, &[u8])> {
    let mut out = Vec::new();
    while b.len() >= 7 {
        match parse_adts(b) {
            Some(h) if h.frame_len <= b.len() => {
                out.push((h, &b[h.header_len..h.frame_len]));
                b = &b[h.frame_len..];
            }
            Some(_) => break,
            None => {
                // resync on the next syncword
                match b[1..].iter().position(|&x| x == 0xFF) {
                    Some(i) => b = &b[i + 1..],
                    None => break,
                }
            }
        }
    }
    out
}

/// AudioSpecificConfig (2 bytes) for an ADTS stream.
pub fn audio_specific_config(h: &Adts) -> [u8; 2] {
    let v: u16 = (h.object_type as u16) << 11 | (h.sample_rate_index as u16) << 7 | (h.channels as u16) << 3;
    v.to_be_bytes()
}

struct BitWriter {
    bytes: Vec<u8>,
    bit: u32,
}

impl BitWriter {
    fn put(&mut self, v: u32, n: u32) {
        for k in (0..n).rev() {
            if self.bit.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let b = (v >> k) & 1;
            let last = self.bytes.len() - 1;
            self.bytes[last] |= (b as u8) << (7 - self.bit % 8);
            self.bit += 1;
        }
    }
}

/// A minimal AAC-LC raw data block for one mono channel: `bands` (0..=49)
/// long-window scale-factor bands of perceptual noise substitution at noise
/// energy `nrg` (9-bit PCM value, 256 = global gain level), or digital silence
/// when `bands` is 0. Enough for test signals without an AAC encoder.
pub fn aac_noise_block(bands: u32, nrg: u32) -> Vec<u8> {
    let bands = bands.min(49);
    let mut w = BitWriter { bytes: Vec::new(), bit: 0 };
    w.put(0, 3); // ID_SCE
    w.put(0, 4); // element_instance_tag
    w.put(100, 8); // global_gain
    w.put(0, 1); // ics_reserved_bit
    w.put(0, 2); // ONLY_LONG_SEQUENCE
    w.put(0, 1); // window_shape
    w.put(bands, 6); // max_sfb
    w.put(0, 1); // predictor_data_present
    if bands > 0 {
        w.put(13, 4); // NOISE_HCB for all bands
        let mut left = bands;
        while left >= 31 {
            w.put(31, 5);
            left -= 31;
        }
        w.put(left, 5);
        w.put(nrg & 0x1FF, 9); // first noise energy, PCM coded
        for _ in 1..bands {
            w.put(0, 1); // delta 0 (scale-factor codeword "0")
        }
    }
    w.put(0, 1); // pulse_data_present
    w.put(0, 1); // tns_data_present
    w.put(0, 1); // gain_control_data_present
    w.put(7, 3); // ID_END
    w.bytes
}

/// An ADTS frame (AAC-LC) around a raw data block.
pub fn adts_wrap(raw: &[u8], sample_rate_index: u8, channels: u8) -> Vec<u8> {
    let len = raw.len() + 7;
    let (sr, ch) = (sample_rate_index & 0x0F, channels & 0x07);
    let mut h = vec![0xFF, 0xF1, (1 << 6) | (sr << 2) | (ch >> 2), ((ch & 3) << 6) | ((len >> 11) as u8 & 3), (len >> 3) as u8, ((len as u8 & 7) << 5) | 0x1F, 0xFC];
    h.extend_from_slice(raw);
    h
}

#[cfg(not(feature = "audio"))]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Audio is not built in: remembers whether the stream carried any.
    pub struct AudioPipe {
        seen: AtomicBool,
        enabled: bool,
    }

    impl AudioPipe {
        pub fn new(enabled: bool) -> Self {
            Self { seen: AtomicBool::new(false), enabled }
        }
        pub fn push(&self, _epoch: u32, _pts: u64, _adts: &[u8]) {
            self.seen.store(true, Ordering::Relaxed);
        }
        pub fn discontinuity(&self, _epoch: u32) {}
        pub fn clock(&self, _epoch: u32) -> Option<u64> {
            None
        }
        pub fn set_paused(&self, _p: bool) {}
        pub fn status(&self) -> String {
            match (self.enabled, self.seen.load(Ordering::Relaxed)) {
                (false, _) => "off".into(),
                (true, false) => "none".into(),
                (true, true) => "not in this build (feature \"audio\")".into(),
            }
        }
    }
}

#[cfg(feature = "audio")]
mod imp {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{mpsc, Arc, Mutex, MutexGuard};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
    use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
    use symphonia::core::packet::PacketRef;
    use symphonia::core::units::{Duration as SDuration, Timestamp};
    use symphonia::default::codecs::AacDecoder;

    use super::{adts_frames, audio_specific_config, Adts};
    use crate::ts::CLOCK;

    /// Seconds of decoded sound kept ahead of the output.
    const RING_SECONDS: f64 = 4.0;

    /// AAC decoding into an interleaved ring at the output's rate and layout.
    pub struct Decoded {
        dec: Option<(AacDecoder, [u8; 2])>,
        pub in_rate: u32,
        pub in_ch: usize,
        pub out_rate: u32,
        pub out_ch: usize,
        pub ring: VecDeque<f32>,
        /// Presentation time (90 kHz) of the first sample in `ring`.
        pub ring_pts: Option<u64>,
        pub epoch: u32,
        /// Fractional read position for the linear resampler, and the last input frame.
        pos: f64,
        prev: Vec<f32>,
        pub frames_decoded: u64,
        pub errors: u64,
        pub unsupported: Option<String>,
        pub underruns: u64,
        /// The clock at the last output callback: (pts of the next sample, when).
        pub played: Option<(u64, Instant)>,
        pub paused: bool,
        /// Times the stream's audio timestamps jumped and the ring restarted.
        pub jumps: u64,
    }

    impl Decoded {
        pub fn new(out_rate: u32, out_ch: usize) -> Self {
            Self {
                dec: None,
                in_rate: 0,
                in_ch: 0,
                out_rate: out_rate.max(1),
                out_ch: out_ch.max(1),
                ring: VecDeque::new(),
                ring_pts: None,
                epoch: 0,
                pos: 0.0,
                prev: Vec::new(),
                frames_decoded: 0,
                errors: 0,
                unsupported: None,
                underruns: 0,
                played: None,
                paused: false,
                jumps: 0,
            }
        }

        /// Presentation time just after the last sample in the ring.
        fn ring_end(&self) -> Option<u64> {
            let start = self.ring_pts?;
            Some(start.saturating_add((self.ring.len() / self.out_ch) as u64 * CLOCK / self.out_rate as u64))
        }

        fn decoder_for(&mut self, h: &Adts) -> Result<(), String> {
            let asc = audio_specific_config(h);
            if self.dec.as_ref().is_some_and(|(_, a)| *a == asc) {
                return Ok(());
            }
            if h.object_type != 2 {
                return Err(format!("AAC object type {} is not supported (AAC-LC only)", h.object_type));
            }
            if h.channels == 0 || h.channels > 2 {
                return Err(format!("{} AAC channels are not supported (mono and stereo)", h.channels));
            }
            let mut params = AudioCodecParameters::new();
            params.for_codec(CODEC_ID_AAC).with_extra_data(Box::new(asc));
            let dec = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).map_err(|e| format!("AAC: {e}"))?;
            self.dec = Some((dec, asc));
            self.in_rate = h.sample_rate;
            self.in_ch = h.channels as usize;
            self.pos = 0.0;
            self.prev.clear();
            Ok(())
        }

        /// Decode a PES payload of ADTS frames starting at `pts`.
        pub fn push(&mut self, epoch: u32, pts: u64, data: &[u8]) {
            if epoch != self.epoch {
                self.reset(epoch);
            }
            // a jump of more than a second against the sound already queued
            // (an encoder restart without a discontinuity tag): restart the
            // ring on the new timeline, so the clock jumps with the video
            if !self.ring.is_empty() && self.ring_end().is_some_and(|end| end.abs_diff(pts) > CLOCK) {
                self.ring.clear();
                self.ring_pts = None;
                self.played = None;
                self.pos = 0.0;
                self.prev.clear();
                self.jumps += 1;
            }
            let mut pts = pts;
            let mut pcm: Vec<f32> = Vec::new();
            for (h, payload) in adts_frames(data) {
                if h.raw_blocks != 1 {
                    self.errors += 1;
                    continue;
                }
                if let Err(e) = self.decoder_for(&h) {
                    self.unsupported = Some(e);
                    return;
                }
                let Some((dec, _)) = self.dec.as_mut() else { return };
                let packet = PacketRef::new(0, Timestamp::new(0), SDuration::new(1024), payload);
                match dec.decode_ref(&packet) {
                    Ok(buf) => {
                        pcm.clear();
                        buf.copy_to_vec_interleaved(&mut pcm);
                        self.frames_decoded += 1;
                        if self.ring_pts.is_none() || self.ring.is_empty() {
                            // the ring restarts here: its first sample plays at this frame's time
                            self.ring.clear();
                            self.ring_pts = Some(pts);
                            self.pos = 0.0;
                            self.prev.clear();
                        }
                        let chunk = std::mem::take(&mut pcm);
                        self.resample_into_ring(&chunk);
                        pcm = chunk;
                    }
                    Err(_) => self.errors += 1,
                }
                pts = pts.saturating_add(1024 * CLOCK / h.sample_rate.max(1) as u64);
            }
            // never hold more than a few seconds: drop the oldest
            let cap = (RING_SECONDS * self.out_rate as f64) as usize * self.out_ch;
            if self.ring.len() > cap {
                let extra = (self.ring.len() - cap) / self.out_ch * self.out_ch;
                self.ring.drain(..extra);
                if let Some(p) = self.ring_pts.as_mut() {
                    *p += (extra / self.out_ch) as u64 * CLOCK / self.out_rate as u64;
                }
            }
        }

        /// Linear resampling and channel mapping of interleaved input.
        fn resample_into_ring(&mut self, input: &[f32]) {
            let ic = self.in_ch.max(1);
            let frames = input.len() / ic;
            if frames == 0 {
                return;
            }
            let frame = |k: isize, prev: &[f32], c: usize| -> f32 {
                if k < 0 {
                    prev.get(c).copied().unwrap_or(0.0)
                } else {
                    input.get(k as usize * ic + c).copied().unwrap_or(0.0)
                }
            };
            let step = self.in_rate as f64 / self.out_rate as f64;
            // position relative to input frame 0; between -1 (the previous
            // chunk's last frame) and frames - 1, so both neighbours exist
            let mut pos = self.pos;
            while pos < (frames - 1) as f64 {
                let i = pos.floor() as isize;
                let t = (pos - i as f64) as f32;
                for oc in 0..self.out_ch {
                    let c = if ic == 1 { 0 } else { oc.min(ic - 1) };
                    let v = if oc >= ic && ic > 1 { 0.0 } else { frame(i, &self.prev, c) * (1.0 - t) + frame(i + 1, &self.prev, c) * t };
                    self.ring.push_back(v);
                }
                pos += step;
            }
            self.pos = pos - frames as f64;
            self.prev = input[(frames - 1) * ic..frames * ic].to_vec();
        }

        pub fn reset(&mut self, epoch: u32) {
            self.epoch = epoch;
            self.ring.clear();
            self.ring_pts = None;
            self.played = None;
            self.pos = 0.0;
            self.prev.clear();
            if let Some((d, _)) = self.dec.as_mut() {
                d.reset();
            }
        }

        /// Fill an output buffer (the device callback). `latency` is how far
        /// ahead of the speaker the callback runs.
        pub fn pull(&mut self, out: &mut [f32], latency: Duration) {
            let ch = self.out_ch;
            if self.paused || self.ring_pts.is_none() {
                out.fill(0.0);
                return;
            }
            let n = out.len().min(self.ring.len()) / ch * ch;
            for (o, s) in out.iter_mut().zip(self.ring.drain(..n)) {
                *o = s;
            }
            out[n..].fill(0.0);
            if n < out.len() {
                self.underruns += 1;
            }
            let frames = (n / ch) as u64;
            if let Some(p) = self.ring_pts.as_mut() {
                // what reaches the speaker now was written `latency` ago
                let audible = p.saturating_sub((latency.as_secs_f64() * CLOCK as f64) as u64);
                self.played = Some((audible, Instant::now()));
                *p += frames * CLOCK / self.out_rate as u64;
            }
            if self.ring.is_empty() {
                self.played = None; // ran dry: the video falls back to the wall clock
            }
        }

        /// Media time now audible, if sound is flowing.
        pub fn clock(&self, epoch: u32) -> Option<u64> {
            if epoch != self.epoch || self.paused {
                return None;
            }
            let (pts, at) = self.played?;
            let since = at.elapsed().min(Duration::from_millis(100));
            Some(pts + (since.as_secs_f64() * CLOCK as f64) as u64)
        }
    }

    enum Output {
        Pending,
        Running,
        Failed(String),
    }

    /// Decoder plus the output device, opened on the first AAC frame.
    pub struct AudioPipe {
        enabled: bool,
        state: Arc<Mutex<Decoded>>,
        output: Mutex<Output>,
        stop: Arc<AtomicBool>,
        thread: Mutex<Option<JoinHandle<()>>>,
        seen: AtomicBool,
        latency_us: Arc<AtomicU64>,
        /// The AAC decoder panicked on this stream: sound is off, video goes on.
        broken: AtomicBool,
    }

    fn lock(m: &Mutex<Decoded>) -> MutexGuard<'_, Decoded> {
        m.lock().unwrap_or_else(|p| p.into_inner())
    }

    impl AudioPipe {
        pub fn new(enabled: bool) -> Self {
            Self {
                enabled,
                state: Arc::new(Mutex::new(Decoded::new(48_000, 2))),
                output: Mutex::new(Output::Pending),
                stop: Arc::new(AtomicBool::new(false)),
                thread: Mutex::new(None),
                seen: AtomicBool::new(false),
                latency_us: Arc::new(AtomicU64::new(0)),
                broken: AtomicBool::new(false),
            }
        }

        fn open(&self) {
            let (tx, rx) = mpsc::channel::<Result<(u32, usize), String>>();
            let (state, stop, lat) = (self.state.clone(), self.stop.clone(), self.latency_us.clone());
            let t = std::thread::Builder::new().name("media-audio".into()).spawn(move || {
                let r = (|| -> Result<cpal::Stream, String> {
                    let host = cpal::default_host();
                    let dev = host.default_output_device().ok_or("no audio output device")?;
                    let cfg = dev.default_output_config().map_err(|e| format!("audio output: {e}"))?;
                    let mut sc = cfg.config();
                    sc.channels = sc.channels.clamp(1, 8);
                    let (rate, ch) = (sc.sample_rate, sc.channels as usize);
                    lock(&state).out_rate = rate;
                    lock(&state).out_ch = ch;
                    let st = state.clone();
                    let lat2 = lat.clone();
                    let stream = dev
                        .build_output_stream::<f32, _, _>(
                            sc,
                            move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                                let ts = info.timestamp();
                                let latency = ts.playback.duration_since(ts.callback);
                                lat2.store(latency.as_micros() as u64, Ordering::Relaxed);
                                lock(&st).pull(out, latency);
                            },
                            |e| log::warn!(target: "anypage_media", "audio output: {e}"),
                            None,
                        )
                        .map_err(|e| format!("audio output: {e}"))?;
                    stream.play().map_err(|e| format!("audio output: {e}"))?;
                    let _ = tx.send(Ok((rate, ch)));
                    Ok(stream)
                })();
                match r {
                    Ok(stream) => {
                        while !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(30));
                        }
                        drop(stream);
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            });
            let status = match t {
                Ok(t) => {
                    *self.thread.lock().unwrap_or_else(|p| p.into_inner()) = Some(t);
                    match rx.recv_timeout(Duration::from_secs(3)) {
                        Ok(Ok(_)) => Output::Running,
                        Ok(Err(e)) => Output::Failed(e),
                        Err(_) => Output::Failed("audio output did not start".into()),
                    }
                }
                Err(e) => Output::Failed(format!("audio thread: {e}")),
            };
            *self.output.lock().unwrap_or_else(|p| p.into_inner()) = status;
        }

        pub fn push(&self, epoch: u32, pts: u64, adts: &[u8]) {
            self.seen.store(true, Ordering::Relaxed);
            if !self.enabled || self.broken.load(Ordering::Relaxed) {
                return;
            }
            let pending = matches!(*self.output.lock().unwrap_or_else(|p| p.into_inner()), Output::Pending);
            if pending {
                self.open();
            }
            if matches!(*self.output.lock().unwrap_or_else(|p| p.into_inner()), Output::Running) {
                // symphonia parses stream data: a panic in it turns the sound
                // off for this session instead of ending the video too
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lock(&self.state).push(epoch, pts, adts)));
                if let Err(e) = r {
                    self.broken.store(true, Ordering::Relaxed);
                    let mut s = lock(&self.state);
                    s.unsupported = Some(format!("AAC decoder failed on this stream ({})", crate::panic_message(&*e)));
                    s.dec = None;
                    s.ring.clear();
                    s.played = None;
                }
            }
        }

        pub fn discontinuity(&self, epoch: u32) {
            lock(&self.state).reset(epoch);
        }

        pub fn clock(&self, epoch: u32) -> Option<u64> {
            if !self.enabled {
                return None;
            }
            lock(&self.state).clock(epoch)
        }

        pub fn set_paused(&self, p: bool) {
            lock(&self.state).paused = p;
        }

        pub fn status(&self) -> String {
            if !self.enabled {
                return "off".into();
            }
            if !self.seen.load(Ordering::Relaxed) {
                return "none".into();
            }
            if let Some(u) = &lock(&self.state).unsupported {
                return format!("unsupported: {u}");
            }
            match &*self.output.lock().unwrap_or_else(|p| p.into_inner()) {
                Output::Pending => "starting".into(),
                Output::Running => {
                    let s = lock(&self.state);
                    if s.played.is_some() {
                        format!("playing ({} Hz, {} ch)", s.in_rate, s.in_ch)
                    } else {
                        "buffering".into()
                    }
                }
                Output::Failed(e) => format!("unavailable: {e}"),
            }
        }
    }

    impl Drop for AudioPipe {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
                let _ = t.join();
            }
        }
    }
}

pub use imp::*;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn adts_headers() {
        let f = adts_wrap(&aac_noise_block(10, 256), 3, 1);
        let h = parse_adts(&f).unwrap();
        assert_eq!((h.object_type, h.sample_rate, h.channels, h.header_len, h.frame_len, h.raw_blocks), (2, 48000, 1, 7, f.len(), 1));
        assert_eq!(audio_specific_config(&h), [0x11, 0x88]);
        let mut two = f.clone();
        two.extend_from_slice(&[1, 2, 3]); // junk between frames
        two.extend_from_slice(&f);
        assert_eq!(adts_frames(&two).len(), 2);
        assert!(parse_adts(&[0xFF, 0xF1, 0, 0, 0, 0, 0]).is_none(), "frame shorter than its header");
        assert!(adts_frames(&f[..10]).is_empty());
    }

    #[cfg(feature = "audio")]
    #[test]
    fn decodes_aac_and_keeps_the_clock() {
        let silent = adts_wrap(&aac_noise_block(0, 0), 3, 1);
        let noise = adts_wrap(&aac_noise_block(40, 256 + 40), 3, 1);
        let mut d = Decoded::new(48_000, 2);
        let mut pes = Vec::new();
        for _ in 0..4 {
            pes.extend_from_slice(&silent);
        }
        d.push(0, 90_000, &pes);
        assert!(d.unsupported.is_none(), "{:?}", d.unsupported);
        assert_eq!(d.frames_decoded, 4);
        assert_eq!(d.ring.len(), (4 * 1024 - 1) * 2, "mono 48 kHz into stereo 48 kHz (the last input frame waits for its successor)");
        assert!(d.ring.iter().all(|s| *s == 0.0), "silence decodes to zeros");
        let mut pes = Vec::new();
        for _ in 0..4 {
            pes.extend_from_slice(&noise);
        }
        d.push(0, 90_000 + 4 * 1920, &pes);
        let tail: Vec<f32> = d.ring.iter().skip(6 * 1024 * 2).copied().collect();
        let rms = (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt();
        assert!(rms > 1e-4 && rms < 1.0, "noise substitution produced sound: rms {rms}");
        // the device pulls 10 ms: the clock moves by 900 ticks
        let mut buf = vec![0f32; 480 * 2];
        d.pull(&mut buf, std::time::Duration::ZERO);
        let c0 = d.clock(0).unwrap();
        assert!((90_000..90_000 + 200).contains(&c0), "{c0}");
        d.pull(&mut buf, std::time::Duration::ZERO);
        assert!(d.clock(0).unwrap() >= c0 + 900);
        assert!(d.clock(1).is_none(), "other epoch");
        // 44.1 kHz input is resampled to the 48 kHz output
        let mut r = Decoded::new(48_000, 1);
        let f441 = adts_wrap(&aac_noise_block(0, 0), 4, 1);
        let mut pes = Vec::new();
        for _ in 0..10 {
            pes.extend_from_slice(&f441);
        }
        r.push(0, 0, &pes);
        let expect = 10.0 * 1024.0 * 48_000.0 / 44_100.0;
        assert!((r.ring.len() as f64 - expect).abs() < 3.0, "{} vs {expect}", r.ring.len());
        // 5.1 is refused with a clear message (HE-AAC is not detected: it is
        // signalled as AAC-LC, and plays as its LC core; see the module docs)
        let six = adts_wrap(&aac_noise_block(0, 0), 3, 6);
        let mut u = Decoded::new(48_000, 2);
        u.push(0, 0, &six);
        assert!(u.unsupported.as_deref().unwrap_or("").contains("channels"));
        // object types other than LC are refused too (ADTS profile 0 = AAC Main)
        let mut main = adts_wrap(&aac_noise_block(0, 0), 3, 1);
        main[2] &= 0x3F;
        let mut u = Decoded::new(48_000, 2);
        u.push(0, 0, &main);
        assert!(u.unsupported.as_deref().unwrap_or("").contains("object type 1"), "{:?}", u.unsupported);
    }

    #[cfg(feature = "audio")]
    #[test]
    fn a_timestamp_jump_restarts_the_audio_timeline() {
        let silent = adts_wrap(&aac_noise_block(0, 0), 3, 1);
        let mut d = Decoded::new(48_000, 1);
        let four: Vec<u8> = (0..4).flat_map(|_| silent.clone()).collect();
        d.push(0, 90_000, &four);
        let mut buf = vec![0f32; 480];
        d.pull(&mut buf, std::time::Duration::ZERO);
        assert!(d.clock(0).unwrap() < 90_000 + 1000);
        // continuous timestamps keep the ring
        d.push(0, 90_000 + 4 * 1920, &four);
        assert_eq!(d.jumps, 0);
        // one hour later, without a discontinuity: the clock follows the new timeline
        let later = 90_000 + 3600 * CLOCK_TICKS;
        d.push(0, later, &four);
        assert_eq!(d.jumps, 1);
        assert_eq!(d.ring.len(), 4 * 1024 - 1);
        d.pull(&mut buf, std::time::Duration::ZERO);
        let c = d.clock(0).unwrap();
        assert!((later..later + 1000).contains(&c), "{c} vs {later}");
    }

    #[cfg(feature = "audio")]
    const CLOCK_TICKS: u64 = crate::ts::CLOCK;
}
