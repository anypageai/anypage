//! MPEG transport stream (ISO/IEC 13818-1) for HLS segments: a streaming
//! demuxer (PAT, PMT, PES; H.264 and AAC ADTS elementary streams) and a muxer
//! that writes self-contained segments (tables first, PCR on the video PID,
//! random-access flags on keyframes).

use std::collections::{HashMap, VecDeque};

pub const PACKET: usize = 188;
pub const SYNC: u8 = 0x47;
pub const PID_PAT: u16 = 0x0000;
pub const PID_NULL: u16 = 0x1FFF;
pub const PID_PMT: u16 = 0x1000;
pub const PID_VIDEO: u16 = 0x0100;
pub const PID_AUDIO: u16 = 0x0101;
pub const STREAM_TYPE_H264: u8 = 0x1B;
pub const STREAM_TYPE_AAC: u8 = 0x0F;
pub const STREAM_TYPE_HEVC: u8 = 0x24;
/// PTS/DTS/PCR base units per second.
pub const CLOCK: u64 = 90_000;
/// PTS and DTS are 33-bit counters.
pub const PTS_MASK: u64 = (1 << 33) - 1;
/// One elementary-stream packet larger than this is treated as corrupt.
pub const MAX_PES: usize = 16 << 20;

/// What an elementary stream carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Aac,
    Hevc,
    Other(u8),
}

impl Codec {
    pub fn from_stream_type(t: u8) -> Codec {
        match t {
            STREAM_TYPE_H264 => Codec::H264,
            STREAM_TYPE_AAC => Codec::Aac,
            STREAM_TYPE_HEVC => Codec::Hevc,
            other => Codec::Other(other),
        }
    }
    pub fn name(self) -> String {
        match self {
            Codec::H264 => "h264".into(),
            Codec::Aac => "aac".into(),
            Codec::Hevc => "hevc".into(),
            Codec::Other(t) => format!("stream_type_0x{t:02x}"),
        }
    }
    pub fn is_video(self) -> bool {
        matches!(self, Codec::H264 | Codec::Hevc)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    pub pid: u16,
    pub stream_type: u8,
    pub codec: Codec,
}

/// One reassembled PES packet: for video one access unit (Annex B), for AAC
/// one or more ADTS frames.
#[derive(Clone, Debug)]
pub struct Pes {
    pub pid: u16,
    pub codec: Codec,
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    /// The adaptation field flagged a random access point (keyframe).
    pub random_access: bool,
    /// A continuity-counter gap was seen while this packet was assembled.
    pub damaged: bool,
    pub data: Vec<u8>,
}

/// MPEG-2 CRC-32 (polynomial 0x04C11DB7, no reflection), as used by PSI sections.
pub fn crc32_mpeg(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

/// Read a 5-byte PTS/DTS field.
pub fn read_timestamp(b: &[u8]) -> Option<u64> {
    if b.len() < 5 {
        return None;
    }
    Some(
        (((b[0] as u64) >> 1) & 0x07) << 30
            | (b[1] as u64) << 22
            | ((b[2] as u64) >> 1) << 15
            | (b[3] as u64) << 7
            | (b[4] as u64) >> 1,
    )
}

/// Write a 5-byte PTS/DTS field with the 4-bit prefix (`0b0010` PTS only,
/// `0b0011` PTS of a PTS+DTS pair, `0b0001` DTS).
pub fn write_timestamp(out: &mut Vec<u8>, prefix: u8, ts: u64) {
    let ts = ts & PTS_MASK;
    out.push((prefix << 4) | (((ts >> 30) as u8 & 0x07) << 1) | 1);
    out.push((ts >> 22) as u8);
    out.push((((ts >> 15) as u8) << 1) | 1);
    out.push((ts >> 7) as u8);
    out.push(((ts as u8) << 1) | 1);
}

struct PesBuf {
    data: Vec<u8>,
    random_access: bool,
    damaged: bool,
}

/// Streaming demuxer: feed bytes in any chunking with [`Demuxer::push`],
/// take packets with [`Demuxer::pop`], and call [`Demuxer::flush`] at the end
/// of the input so the last PES of each stream comes out.
#[derive(Default)]
pub struct Demuxer {
    pmt_pid: Option<u16>,
    streams: Vec<Stream>,
    pes: HashMap<u16, PesBuf>,
    psi: HashMap<u16, Vec<u8>>,
    cc: HashMap<u16, u8>,
    carry: Vec<u8>,
    out: VecDeque<Pes>,
    /// Packets with the transport error flag, continuity gaps, bad sections, lost sync.
    pub errors: u64,
    /// Last program clock reference seen (90 kHz base).
    pub pcr: Option<u64>,
    /// Bytes skipped to find packet sync.
    pub skipped: u64,
}

impl Demuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Elementary streams of the first program, once its PMT was seen.
    pub fn streams(&self) -> &[Stream] {
        &self.streams
    }

    pub fn pop(&mut self) -> Option<Pes> {
        self.out.pop_front()
    }

    /// Feed transport-stream bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(bytes);
        let mut i = 0usize;
        while i + PACKET <= buf.len() {
            if buf[i] != SYNC || (i + 2 * PACKET <= buf.len() && buf[i + PACKET] != SYNC) {
                // lost sync: find the next position that looks like a run of packets
                let next = (i + 1..buf.len()).find(|&k| buf[k] == SYNC && (k + PACKET >= buf.len() || buf[k + PACKET] == SYNC));
                let k = next.unwrap_or(buf.len());
                self.skipped += (k - i) as u64;
                self.errors += 1;
                i = k;
                continue;
            }
            let mut pkt = [0u8; PACKET];
            pkt.copy_from_slice(&buf[i..i + PACKET]);
            self.packet(&pkt);
            i += PACKET;
        }
        self.carry = buf[i..].to_vec();
    }

    /// End of input: emit every PES still being assembled.
    pub fn flush(&mut self) {
        let mut pids: Vec<u16> = self.pes.keys().copied().collect();
        pids.sort_unstable();
        for pid in pids {
            self.finish_pes(pid);
        }
    }

    fn codec_of(&self, pid: u16) -> Option<Codec> {
        self.streams.iter().find(|s| s.pid == pid).map(|s| s.codec)
    }

    fn packet(&mut self, p: &[u8; PACKET]) {
        if p[1] & 0x80 != 0 {
            self.errors += 1; // transport_error_indicator
            return;
        }
        let pusi = p[1] & 0x40 != 0;
        let pid = ((p[1] as u16 & 0x1F) << 8) | p[2] as u16;
        if pid == PID_NULL {
            return;
        }
        let afc = (p[3] >> 4) & 0x03;
        let cc = p[3] & 0x0F;
        let mut off = 4usize;
        let mut random_access = false;
        let mut discontinuity = false;
        if afc & 0x02 != 0 {
            let len = p[4] as usize;
            if 5 + len > PACKET {
                self.errors += 1;
                return;
            }
            if len > 0 {
                let flags = p[5];
                discontinuity = flags & 0x80 != 0;
                random_access = flags & 0x40 != 0;
                if flags & 0x10 != 0 && len >= 7 {
                    let b = &p[6..12];
                    let base = (b[0] as u64) << 25 | (b[1] as u64) << 17 | (b[2] as u64) << 9 | (b[3] as u64) << 1 | (b[4] as u64) >> 7;
                    self.pcr = Some(base);
                }
            }
            off = 5 + len;
        }
        if afc & 0x01 == 0 {
            return; // no payload
        }
        // continuity: a repeated counter is a duplicate packet, a gap is loss
        let gap = match self.cc.insert(pid, cc) {
            Some(prev) if prev == cc => return,
            Some(prev) => !discontinuity && (prev + 1) & 0x0F != cc,
            None => false,
        };
        if gap {
            self.errors += 1;
        }
        let payload = &p[off..];
        if pid == PID_PAT || Some(pid) == self.pmt_pid {
            self.psi_payload(pid, pusi, payload);
            return;
        }
        if self.codec_of(pid).is_none() {
            return;
        }
        if pusi {
            if gap {
                if let Some(b) = self.pes.get_mut(&pid) {
                    b.damaged = true; // the lost packet belonged to the unit being finished
                }
            }
            self.finish_pes(pid);
            self.pes.insert(pid, PesBuf { data: payload.to_vec(), random_access, damaged: false });
        } else if let Some(b) = self.pes.get_mut(&pid) {
            if gap {
                b.damaged = true;
            }
            if b.data.len() + payload.len() > MAX_PES {
                self.pes.remove(&pid);
                self.errors += 1;
                return;
            }
            b.data.extend_from_slice(payload);
            b.random_access |= random_access;
        } else {
            return; // continuation of a PES whose start we never saw
        }
        // a PES with an explicit length is complete as soon as it is all here
        if let Some(b) = self.pes.get(&pid) {
            if b.data.len() >= 6 {
                let len = (b.data[4] as usize) << 8 | b.data[5] as usize;
                if len > 0 && b.data.len() >= 6 + len {
                    self.finish_pes(pid);
                }
            }
        }
    }

    fn psi_payload(&mut self, pid: u16, pusi: bool, payload: &[u8]) {
        if pusi {
            let Some(&pointer) = payload.first() else { return };
            let start = 1 + pointer as usize;
            if start > payload.len() {
                self.errors += 1;
                return;
            }
            self.psi.insert(pid, payload[start..].to_vec());
        } else if let Some(b) = self.psi.get_mut(&pid) {
            if b.len() + payload.len() > 4096 {
                self.psi.remove(&pid);
                self.errors += 1;
                return;
            }
            b.extend_from_slice(payload);
        } else {
            return;
        }
        let Some(b) = self.psi.get(&pid) else { return };
        if b.len() < 3 {
            return;
        }
        if b[0] == 0xFF {
            self.psi.remove(&pid); // stuffing only
            return;
        }
        let total = 3 + (((b[1] as usize) & 0x0F) << 8 | b[2] as usize);
        if b.len() < total {
            return;
        }
        let section = b[..total].to_vec();
        self.psi.remove(&pid);
        if total < 12 || crc32_mpeg(&section) != 0 {
            self.errors += 1;
            return;
        }
        match section[0] {
            0x00 => self.pat(&section),
            0x02 => self.pmt(&section),
            _ => {}
        }
    }

    fn pat(&mut self, s: &[u8]) {
        let body = &s[8..s.len() - 4];
        for e in body.as_chunks::<4>().0 {
            let program = (e[0] as u16) << 8 | e[1] as u16;
            let pid = ((e[2] as u16 & 0x1F) << 8) | e[3] as u16;
            if program != 0 {
                if self.pmt_pid != Some(pid) {
                    self.pmt_pid = Some(pid);
                }
                return;
            }
        }
    }

    fn pmt(&mut self, s: &[u8]) {
        let end = s.len() - 4;
        let info_len = ((s[10] as usize & 0x0F) << 8) | s[11] as usize;
        let mut i = 12 + info_len;
        let mut streams = Vec::new();
        while i + 5 <= end {
            let stream_type = s[i];
            let pid = ((s[i + 1] as u16 & 0x1F) << 8) | s[i + 2] as u16;
            let es_len = ((s[i + 3] as usize & 0x0F) << 8) | s[i + 4] as usize;
            streams.push(Stream { pid, stream_type, codec: Codec::from_stream_type(stream_type) });
            i += 5 + es_len;
        }
        if streams != self.streams {
            self.streams = streams;
        }
    }

    fn finish_pes(&mut self, pid: u16) {
        let Some(buf) = self.pes.remove(&pid) else { return };
        let Some(codec) = self.codec_of(pid) else { return };
        let d = &buf.data;
        if d.len() < 9 || d[0] != 0 || d[1] != 0 || d[2] != 1 {
            self.errors += 1;
            return;
        }
        let stream_id = d[3];
        let declared = (d[4] as usize) << 8 | d[5] as usize;
        let no_header = matches!(stream_id, 0xBC | 0xBE | 0xBF | 0xF0 | 0xF1 | 0xF2 | 0xF8 | 0xFF);
        let (pts, dts, start) = if no_header {
            (None, None, 6)
        } else {
            let flags = d[7] >> 6;
            let hdl = d[8] as usize;
            let start = 9 + hdl;
            // the optional header must hold every field its flags announce:
            // 5 bytes for a PTS, 10 for a PTS and a DTS
            let needed = match flags {
                0x02 => 5,
                0x03 => 10,
                _ => 0,
            };
            if start > d.len() || hdl < needed {
                self.errors += 1;
                return;
            }
            let pts = if flags & 0x02 != 0 { d.get(9..14).and_then(read_timestamp) } else { None };
            let dts = if flags == 0x03 { d.get(14..19).and_then(read_timestamp) } else { None };
            (pts, dts, start)
        };
        let end = if declared > 0 { (6 + declared).min(d.len()) } else { d.len() };
        if end < start {
            self.errors += 1;
            return;
        }
        self.out.push_back(Pes {
            pid,
            codec,
            pts,
            dts,
            random_access: buf.random_access,
            damaged: buf.damaged,
            data: d[start..end].to_vec(),
        });
    }
}

/// Demux a whole buffer (one HLS segment).
pub fn demux_all(bytes: &[u8]) -> (Vec<Stream>, Vec<Pes>, u64) {
    let mut d = Demuxer::new();
    d.push(bytes);
    d.flush();
    let mut out = Vec::new();
    while let Some(p) = d.pop() {
        out.push(p);
    }
    (d.streams().to_vec(), out, d.errors)
}

// ---------------------------------------------------------------- muxer

/// Writes a single-program transport stream with an H.264 video PID and an
/// optional AAC (ADTS) audio PID.
pub struct Muxer {
    cc: HashMap<u16, u8>,
    audio: bool,
    video: bool,
    version: u8,
    /// PCR runs this far behind the video DTS (90 kHz units).
    pub pcr_delay: u64,
}

impl Muxer {
    pub fn new(video: bool, audio: bool) -> Self {
        Self { cc: HashMap::new(), audio, video, version: 0, pcr_delay: 9_000 }
    }

    fn next_cc(&mut self, pid: u16) -> u8 {
        let c = self.cc.entry(pid).or_insert(0x0F);
        *c = (*c + 1) & 0x0F;
        *c
    }

    fn section_packet(&mut self, out: &mut Vec<u8>, pid: u16, section: &[u8]) {
        let mut p = Vec::with_capacity(PACKET);
        p.push(SYNC);
        p.push(0x40 | (pid >> 8) as u8 & 0x1F);
        p.push(pid as u8);
        let cc = self.next_cc(pid);
        p.push(0x10 | cc);
        p.push(0); // pointer_field
        p.extend_from_slice(section);
        p.resize(PACKET, 0xFF);
        out.extend_from_slice(&p[..PACKET]);
    }

    /// PAT and PMT; every segment starts with them so it decodes on its own.
    pub fn write_tables(&mut self, out: &mut Vec<u8>) {
        // PAT: program 1 -> PMT PID
        let mut pat = vec![0x00, 0, 0, 0x00, 0x01, 0xC1 | (self.version << 1), 0x00, 0x00];
        pat.extend_from_slice(&[0x00, 0x01, 0xE0 | (PID_PMT >> 8) as u8, PID_PMT as u8]);
        finish_section(&mut pat);
        self.section_packet(out, PID_PAT, &pat);

        let pcr_pid = if self.video { PID_VIDEO } else { PID_AUDIO };
        let mut pmt = vec![0x02, 0, 0, 0x00, 0x01, 0xC1 | (self.version << 1), 0x00, 0x00];
        pmt.extend_from_slice(&[0xE0 | (pcr_pid >> 8) as u8, pcr_pid as u8, 0xF0, 0x00]);
        if self.video {
            pmt.extend_from_slice(&[STREAM_TYPE_H264, 0xE0 | (PID_VIDEO >> 8) as u8, PID_VIDEO as u8, 0xF0, 0x00]);
        }
        if self.audio {
            pmt.extend_from_slice(&[STREAM_TYPE_AAC, 0xE0 | (PID_AUDIO >> 8) as u8, PID_AUDIO as u8, 0xF0, 0x00]);
        }
        finish_section(&mut pmt);
        self.section_packet(out, PID_PMT, &pmt);
    }

    /// One H.264 access unit (Annex B, start codes included). An access unit
    /// delimiter is added when the unit does not start with one.
    pub fn write_video(&mut self, out: &mut Vec<u8>, pts: u64, dts: Option<u64>, keyframe: bool, annexb: &[u8]) {
        let mut es = Vec::with_capacity(annexb.len() + 6);
        let first_nal = crate::h264::nal_units(annexb).next().map(|n| n[0] & 0x1F);
        if first_nal != Some(9) {
            es.extend_from_slice(&[0, 0, 0, 1, 0x09, 0xF0]);
        }
        es.extend_from_slice(annexb);
        let pcr = dts.unwrap_or(pts).saturating_sub(self.pcr_delay);
        self.write_pes(out, PID_VIDEO, 0xE0, pts, dts, &es, Some(pcr), keyframe, false);
    }

    /// One or more ADTS frames starting at `pts`.
    pub fn write_audio(&mut self, out: &mut Vec<u8>, pts: u64, adts: &[u8]) {
        let pcr = if self.video { None } else { Some(pts.saturating_sub(self.pcr_delay)) };
        self.write_pes(out, PID_AUDIO, 0xC0, pts, None, adts, pcr, false, true);
    }

    #[allow(clippy::too_many_arguments)]
    fn write_pes(&mut self, out: &mut Vec<u8>, pid: u16, stream_id: u8, pts: u64, dts: Option<u64>, es: &[u8], pcr: Option<u64>, random_access: bool, bounded: bool) {
        let mut pes = Vec::with_capacity(es.len() + 19);
        pes.extend_from_slice(&[0, 0, 1, stream_id, 0, 0, 0x80]);
        match dts {
            Some(d) if d != pts => {
                pes.push(0xC0);
                pes.push(10);
                write_timestamp(&mut pes, 0x03, pts);
                write_timestamp(&mut pes, 0x01, d);
            }
            _ => {
                pes.push(0x80);
                pes.push(5);
                write_timestamp(&mut pes, 0x02, pts);
            }
        }
        let len = pes.len() - 6 + es.len();
        if bounded || len <= 0xFFFF {
            let l = len.min(0xFFFF);
            pes[4] = (l >> 8) as u8;
            pes[5] = l as u8;
        }
        pes.extend_from_slice(es);

        let mut pos = 0usize;
        let mut first = true;
        while pos < pes.len() {
            let mut af: Vec<u8> = Vec::new();
            if first && (pcr.is_some() || random_access) {
                let mut flags = 0u8;
                if random_access {
                    flags |= 0x40;
                }
                af.push(0); // length, set below
                if let Some(pcr) = pcr {
                    flags |= 0x10;
                    af.push(flags);
                    let b = pcr & PTS_MASK;
                    af.extend_from_slice(&[(b >> 25) as u8, (b >> 17) as u8, (b >> 9) as u8, (b >> 1) as u8, ((b as u8 & 1) << 7) | 0x7E, 0x00]);
                } else {
                    af.push(flags);
                }
            }
            let remaining = pes.len() - pos;
            let room = 184 - af.len();
            if remaining < room {
                // stuff the adaptation field so the packet is exactly full
                let stuff = room - remaining;
                if af.is_empty() {
                    if stuff == 1 {
                        af.push(0);
                    } else {
                        af.push(0);
                        af.push(0x00);
                        af.resize(stuff, 0xFF);
                    }
                } else {
                    af.resize(af.len() + stuff, 0xFF);
                }
            }
            if !af.is_empty() {
                af[0] = (af.len() - 1) as u8;
            }
            let take = (184 - af.len()).min(remaining);
            let afc = if af.is_empty() { 0x10 } else { 0x30 };
            let cc = self.next_cc(pid);
            out.push(SYNC);
            out.push(if first { 0x40 } else { 0 } | ((pid >> 8) as u8 & 0x1F));
            out.push(pid as u8);
            out.push(afc | cc);
            out.extend_from_slice(&af);
            out.extend_from_slice(&pes[pos..pos + take]);
            pos += take;
            first = false;
        }
    }
}

fn finish_section(s: &mut Vec<u8>) {
    // section_length counts from after the length field through the CRC
    let len = s.len() - 3 + 4;
    s[1] = 0xB0 | ((len >> 8) as u8 & 0x0F);
    s[2] = len as u8;
    let crc = crc32_mpeg(s);
    s.extend_from_slice(&crc.to_be_bytes());
}

/// Unwraps 33-bit timestamps into a monotonic 64-bit timeline.
#[derive(Default, Clone, Copy, Debug)]
pub struct Unwrap {
    last: Option<u64>,
    offset: u64,
}

impl Unwrap {
    pub fn unwrap(&mut self, ts: u64) -> u64 {
        let ts = ts & PTS_MASK;
        if let Some(last) = self.last {
            // a jump backwards by more than half the range is a wrap
            if ts + (1 << 32) < last {
                self.offset = self.offset.saturating_add(1 << 33);
            } else if last + (1 << 32) < ts && self.offset >= 1 << 33 {
                self.offset -= 1 << 33; // a late packet from before the wrap
            }
        }
        self.last = Some(ts);
        ts.saturating_add(self.offset)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn crc_of_a_known_pat() {
        // A PAT produced by common muxers (program 1 -> PMT 0x1000)
        let pat = [0x00u8, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00];
        assert_eq!(crc32_mpeg(&pat), 0x2AB1_04B2);
        let mut with_crc = pat.to_vec();
        with_crc.extend_from_slice(&0x2AB1_04B2u32.to_be_bytes());
        assert_eq!(crc32_mpeg(&with_crc), 0);
    }

    #[test]
    fn timestamps_round_trip() {
        for ts in [0u64, 1, 90_000, 0x1_2345_6789 & PTS_MASK, PTS_MASK] {
            let mut b = Vec::new();
            write_timestamp(&mut b, 0x02, ts);
            assert_eq!(b.len(), 5);
            assert_eq!(read_timestamp(&b), Some(ts));
            assert_eq!(b[0] >> 4, 0x02);
            assert!(b[0] & 1 == 1 && b[2] & 1 == 1 && b[4] & 1 == 1, "marker bits");
        }
    }

    fn au(n: usize, fill: u8) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, 0x65];
        v.extend(std::iter::repeat_n(fill, n));
        v
    }

    #[test]
    fn mux_then_demux_recovers_every_unit() {
        let mut m = Muxer::new(true, true);
        let mut ts = Vec::new();
        m.write_tables(&mut ts);
        let sizes = [1usize, 2, 170, 171, 172, 176, 177, 178, 182, 183, 184, 185, 1000, 70_000, 200_000];
        for (k, &n) in sizes.iter().enumerate() {
            let pts = 90_000 + k as u64 * 3000;
            m.write_video(&mut ts, pts, None, k == 0, &au(n, k as u8));
            if k % 3 == 0 {
                let adts = vec![0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC, k as u8];
                m.write_audio(&mut ts, pts + 10, &adts);
            }
        }
        assert_eq!(ts.len() % PACKET, 0);
        assert!(ts.chunks(PACKET).all(|p| p[0] == SYNC));
        // feed in odd chunk sizes to exercise packet reassembly
        let mut d = Demuxer::new();
        for chunk in ts.chunks(1000) {
            d.push(chunk);
        }
        d.flush();
        assert_eq!(d.errors, 0);
        assert_eq!(d.streams().len(), 2);
        assert_eq!(d.streams()[0].codec, Codec::H264);
        assert_eq!(d.streams()[1].codec, Codec::Aac);
        let mut video = Vec::new();
        let mut audio = Vec::new();
        while let Some(p) = d.pop() {
            if p.codec == Codec::H264 {
                video.push(p)
            } else {
                audio.push(p)
            }
        }
        assert_eq!(video.len(), sizes.len());
        for (k, p) in video.iter().enumerate() {
            assert_eq!(p.pts, Some(90_000 + k as u64 * 3000));
            // access unit delimiter added in front
            assert_eq!(&p.data[..6], &[0, 0, 0, 1, 0x09, 0xF0]);
            assert_eq!(&p.data[6..], &au(sizes[k], k as u8)[..], "unit {k}");
            assert_eq!(p.random_access, k == 0);
        }
        assert_eq!(audio.len(), 5);
        assert_eq!(audio[1].pts, Some(90_000 + 3 * 3000 + 10));
        assert_eq!(audio[1].data[7], 3);
        assert!(d.pcr.is_some());
    }

    #[test]
    fn pts_and_dts_pair() {
        let mut m = Muxer::new(true, false);
        let mut ts = Vec::new();
        m.write_tables(&mut ts);
        m.write_video(&mut ts, 100_000, Some(97_000), true, &au(10, 1));
        let (streams, pes, errors) = demux_all(&ts);
        assert_eq!(errors, 0);
        assert_eq!(streams.len(), 1);
        assert_eq!(pes[0].pts, Some(100_000));
        assert_eq!(pes[0].dts, Some(97_000));
    }

    #[test]
    fn resyncs_and_counts_damage() {
        let mut m = Muxer::new(true, false);
        let mut ts = Vec::new();
        m.write_tables(&mut ts);
        for k in 0..4 {
            m.write_video(&mut ts, k * 3000, None, k == 0, &au(500, k as u8));
        }
        // garbage in front and a dropped packet in the middle
        let mut bad = vec![1u8, 2, 3, 0x47, 9];
        bad.extend_from_slice(&ts[..PACKET * 4]);
        bad.extend_from_slice(&ts[PACKET * 5..]);
        let (streams, pes, errors) = demux_all(&bad);
        assert_eq!(streams.len(), 1);
        assert!(errors >= 2, "lost sync and a continuity gap: {errors}");
        assert_eq!(pes.len(), 4);
        assert!(pes.iter().any(|p| p.damaged));
        // truncated input never panics
        for cut in [0, 1, 187, 188, 189, 400, 1000] {
            let _ = demux_all(&ts[..cut.min(ts.len())]);
        }
        let _ = demux_all(&[0x47; 188 * 3]);
    }

    /// A video PES whose flags announce a PTS and a DTS (0xC0) but whose
    /// header is only 9 to 13 bytes long, alone in one packet after a 172-byte
    /// adaptation field (found by review: it used to panic on a slice index).
    pub(crate) fn short_pes_with_dts_flags() -> Vec<u8> {
        let mut ts = Vec::new();
        Muxer::new(true, false).write_tables(&mut ts);
        let mut p = vec![SYNC, 0x40 | (PID_VIDEO >> 8) as u8, PID_VIDEO as u8, 0x30];
        p.push(172); // adaptation field length
        p.push(0x00);
        p.resize(4 + 1 + 172, 0xFF);
        p.extend_from_slice(&[0, 0, 1, 0xE0, 0, 0, 0x80, 0xC0, 0x00, 0x21, 0x00]);
        assert_eq!(p.len(), PACKET);
        ts.extend_from_slice(&p);
        ts
    }

    #[test]
    fn short_pes_headers_are_errors_not_panics() {
        let (streams, pes, errors) = demux_all(&short_pes_with_dts_flags());
        assert_eq!(streams.len(), 1);
        assert!(pes.is_empty());
        assert_eq!(errors, 1, "the short header is counted as a demux error");
        // every header length below what the flags need, for PTS-only and PTS+DTS,
        // and every truncation of a valid PES
        for flags in [0x80u8, 0xC0, 0x40] {
            for hdl in 0u8..12 {
                for extra in 0usize..12 {
                    let mut es = vec![0, 0, 1, 0xE0, 0, 0, 0x80, flags, hdl];
                    es.extend(std::iter::repeat_n(0x21, extra));
                    let mut ts = Vec::new();
                    Muxer::new(true, false).write_tables(&mut ts);
                    let mut p = vec![SYNC, 0x40 | (PID_VIDEO >> 8) as u8, PID_VIDEO as u8, 0x30];
                    let af = 184 - 1 - es.len();
                    p.push(af as u8);
                    if af > 0 {
                        p.push(0);
                    }
                    p.resize(5 + af, 0xFF);
                    p.extend_from_slice(&es);
                    assert_eq!(p.len(), PACKET);
                    ts.extend_from_slice(&p);
                    let _ = demux_all(&ts);
                }
            }
        }
    }

    #[test]
    fn unwraps_33_bit_timestamps() {
        let mut u = Unwrap::default();
        assert_eq!(u.unwrap(PTS_MASK - 10), PTS_MASK - 10);
        assert_eq!(u.unwrap(5), PTS_MASK + 6);
        assert_eq!(u.unwrap(PTS_MASK - 4), PTS_MASK - 4); // late packet from before the wrap
        assert_eq!(u.unwrap(20), PTS_MASK + 21);
    }
}
