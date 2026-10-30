//! RTP as an RTSP session delivers it (RFC 3550, RFC 2326 §10.12), depacketized into the same
//! packets the file readers give.
//!
//! The byte stream is a run of frames, `$`, a channel, a big-endian 16-bit length and that many
//! bytes: RTSP's interleaved framing, which a source over UDP produces too by framing each datagram.
//! Channel 255 carries the [`Description`] (the SDP, and which channel each set-up track arrives
//! on), and it comes first; a track's RTP arrives on its channel and its RTCP on the next one.
//!
//! Payloads: H.264 (RFC 6184: single NAL units, STAP-A, FU-A), HEVC (RFC 7798: single, AP, FU),
//! AAC (RFC 3640 `mpeg4-generic`, AAC-hbr and AAC-lbr), Opus (RFC 7587), and G.711 (PCMU, PCMA) and
//! L16, which become 16-bit little-endian PCM. Each track's packets pass through a reorder buffer
//! that costs nothing while they arrive in order and holds at most [`REORDER`] packets waiting for a
//! late one; a gap it gives up on is counted as loss, and the picture it damaged is dropped.
//!
//! Timestamps count from the `rtptime` the PLAY response's RTP-Info gave each track, which is the
//! same instant for all of them; a track without one counts from its first packet. Video starts at
//! a keyframe, and the parameter sets from the SDP are put in front of each keyframe that lacks
//! them, so the packets decode on their own. The streams are published once each track has what
//! its description needs (a video track: its sequence parameter set), or once [`MAX_HELD`] packets
//! have waited, without the tracks still missing it.

use crate::stream::{StreamDemuxer, StreamPacket};
use ironav_core::{CodecId, Error, Packet, PixelFormat, Rational, Result, StreamInfo, StreamParams};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

/// The channel that carries the [`Description`].
pub const DESCRIPTION_CHANNEL: u8 = 0xFF;
/// Packets held per track waiting for a missing one, at most.
pub const REORDER: usize = 64;
/// Packets, and bytes, held before the streams are published, at most.
const MAX_HELD: usize = 512;
const MAX_HELD_BYTES: usize = 32 << 20;
/// Tracks a description may set up, at most.
const MAX_TRACKS: usize = 64;
/// The largest access unit or audio frame put together.
const MAX_UNIT: usize = 16 << 20;
/// A sequence number this far from the expected one starts the count again instead of waiting.
const RESYNC: i32 = 3000;

/// What an RTSP source sends first: the session description, and the tracks it set up.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Description {
    pub sdp: String,
    pub tracks: Vec<TrackSetup>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackSetup {
    /// Index of the track's `m=` section in the SDP.
    pub media: usize,
    /// The channel its RTP arrives on (its RTCP on the next).
    pub channel: u8,
    /// RTP-Info's `rtptime`: the RTP timestamp of the play range's start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtptime: Option<u32>,
    /// RTP-Info's `seq`: the first sequence number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u16>,
}

/// One frame of the stream: `$`, the channel, the length and the payload (at most 65535 bytes).
pub fn frame(channel: u8, payload: &[u8]) -> Result<Vec<u8>> {
    let len = u16::try_from(payload.len()).map_err(|_| Error::invalid_input("an RTP frame is larger than 65535 bytes"))?;
    let mut f = Vec::with_capacity(4 + payload.len());
    f.push(b'$');
    f.push(channel);
    f.extend_from_slice(&len.to_be_bytes());
    f.extend_from_slice(payload);
    Ok(f)
}

/// The frame that carries `d`.
pub fn description_frame(d: &Description) -> Result<Vec<u8>> {
    frame(DESCRIPTION_CHANNEL, &serde_json::to_vec(d).map_err(|e| Error::invalid_input(e.to_string()))?)
}

// ---- SDP ----------------------------------------------------------------------------------------

/// A session description (RFC 8866), as much of it as playing needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sdp {
    /// The session-level `a=control`.
    pub control: Option<String>,
    pub media: Vec<Media>,
}

/// One `m=` section.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Media {
    /// `video`, `audio`, ...
    pub kind: String,
    /// The first payload type the section offers, and its `rtpmap`: encoding (upper case), clock
    /// rate and channels.
    pub payload: u8,
    pub encoding: String,
    pub clock: u32,
    pub channels: u16,
    /// The payload type's `fmtp` parameters, keys in lower case.
    pub fmtp: BTreeMap<String, String>,
    pub control: Option<String>,
    pub framerate: Option<f64>,
}

impl Sdp {
    pub fn parse(text: &str) -> Result<Sdp> {
        let mut sdp = Sdp::default();
        for line in text.lines().map(str::trim) {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k {
                "m" => {
                    let mut f = v.split_whitespace();
                    let kind = f.next().unwrap_or("").to_ascii_lowercase();
                    let payload = f.nth(2).and_then(|p| p.parse::<u8>().ok()).ok_or_else(|| Error::invalid_input(format!("SDP media line '{line}' has no payload type")))?;
                    let (encoding, clock, channels) = match payload {
                        0 => ("PCMU", 8000, 1),
                        8 => ("PCMA", 8000, 1),
                        10 => ("L16", 44100, 2),
                        11 => ("L16", 44100, 1),
                        _ => ("", 0, 1),
                    };
                    sdp.media.push(Media { kind, payload, encoding: encoding.into(), clock, channels, ..Media::default() });
                }
                "a" => {
                    let (name, value) = v.split_once(':').unwrap_or((v, ""));
                    let Some(m) = sdp.media.last_mut() else {
                        if name == "control" {
                            sdp.control = Some(value.trim().to_string());
                        }
                        continue;
                    };
                    let for_payload = |value: &str| -> Option<String> {
                        let (pt, rest) = value.split_once(char::is_whitespace)?;
                        (pt.parse::<u8>().ok()? == m.payload).then(|| rest.trim().to_string())
                    };
                    match name {
                        "rtpmap" => {
                            if let Some(map) = for_payload(value) {
                                let mut p = map.split('/');
                                m.encoding = p.next().unwrap_or("").to_ascii_uppercase();
                                m.clock = p.next().and_then(|c| c.parse().ok()).unwrap_or(0);
                                m.channels = p.next().and_then(|c| c.parse().ok()).unwrap_or(1);
                            }
                        }
                        "fmtp" => {
                            if let Some(params) = for_payload(value) {
                                for kv in params.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                                    let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                                    m.fmtp.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                                }
                            }
                        }
                        "control" => m.control = Some(value.trim().to_string()),
                        "framerate" => m.framerate = value.trim().parse().ok().filter(|f: &f64| (1.0..1000.0).contains(f)),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        Ok(sdp)
    }
}

impl Media {
    /// The codec IronAV reads this section's payload as, or why it cannot.
    pub fn codec(&self) -> std::result::Result<CodecId, String> {
        let mode = self.fmtp.get("mode").map(|m| m.to_ascii_lowercase());
        match self.encoding.as_str() {
            "H264" if self.fmtp.get("packetization-mode").is_none_or(|m| m == "0" || m == "1") => Ok(CodecId::H264),
            "H264" => Err("H.264 in interleaved packetization mode".into()),
            "H265" if self.fmtp.get("sprop-max-don-diff").is_none_or(|d| d == "0") => Ok(CodecId::Hevc),
            "H265" => Err("HEVC with decoding order numbers".into()),
            "MPEG4-GENERIC" if matches!(mode.as_deref(), Some("aac-hbr" | "aac-lbr")) && self.fmtp.contains_key("config") => Ok(CodecId::Aac),
            "MPEG4-GENERIC" => Err("mpeg4-generic other than AAC-hbr or AAC-lbr with a config".into()),
            "OPUS" => Ok(CodecId::Opus),
            "PCMU" | "PCMA" | "L16" if self.clock > 0 && (1..=8).contains(&self.channels) => Ok(CodecId::PcmS16Le),
            "" => Err(format!("payload type {} without an rtpmap", self.payload)),
            other => Err(format!("the '{other}' payload")),
        }
    }
}

// ---- small decoders -------------------------------------------------------------------------------

fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut n) = (0u32, 0);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b' ' | b'\t' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((acc >> n) as u8);
        }
    }
    Some(out)
}

fn hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// G.711 µ-law (`alaw` false) or A-law to linear 16-bit.
fn g711(b: u8, alaw: bool) -> i16 {
    if alaw {
        let a = b ^ 0x55;
        let seg = (a >> 4) & 7;
        let mut t = ((a & 15) as i16) << 4;
        t += if seg == 0 { 8 } else { 0x108 };
        if seg > 1 {
            t <<= seg - 1;
        }
        if a & 0x80 != 0 { t } else { -t }
    } else {
        let u = !b;
        let seg = (u >> 4) & 7;
        let t = ((((u & 15) as i16) << 3) + 0x84) << seg;
        if u & 0x80 != 0 { 0x84 - t } else { t - 0x84 }
    }
}

struct Bits<'a> {
    d: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.d.get(self.at / 8)?;
            v = (v << 1) | ((byte >> (7 - self.at % 8)) & 1) as u32;
            self.at += 1;
        }
        Some(v)
    }
}

// ---- tracks ---------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Payload {
    Video { hevc: bool },
    Aac { size_len: u32, index_len: u32, delta_len: u32, frame: i64 },
    Opus,
    G711 { alaw: bool },
    L16,
}

struct Track {
    channel: u8,
    payload_type: u8,
    payload: Payload,
    clock: u32,
    /// Channels of PCM output (0 for other payloads), for packet durations.
    pcm: u16,
    /// The description, until the streams are published; then its index, or `None` if the track
    /// was left out.
    info: Option<StreamInfo>,
    index: Option<usize>,
    /// Parameter sets from the SDP, Annex B, to put in front of keyframes that lack them.
    params: Vec<u8>,
    frame_rate: Rational,
    // reorder buffer: the next sequence number, and packets that came early
    expected: Option<u16>,
    held: BTreeMap<u16, Vec<u8>>,
    // timing: the timestamp counted from, and the last one with the time it gave
    base: Option<u32>,
    last: Option<(u32, i64)>,
    // access unit being put together
    au: Vec<u8>,
    au_ts: Option<u32>,
    au_key: bool,
    au_sps: bool,
    au_bad: bool,
    fu: Option<Vec<u8>>,
    aac_frag: Option<(u32, usize, Vec<u8>)>,
    started: bool,
    lost: u64,
}

#[derive(Default)]
pub struct RtpStream {
    buf: Vec<u8>,
    pos: usize,
    described: bool,
    tracks: Vec<Track>,
    streams: Vec<StreamInfo>,
    published: bool,
    held: VecDeque<(usize, Packet, bool)>,
    held_bytes: usize,
    out: VecDeque<StreamPacket>,
}

impl RtpStream {
    /// Packets lost so far, over all tracks.
    pub fn lost(&self) -> u64 {
        self.tracks.iter().map(|t| t.lost).sum()
    }

    fn describe(&mut self, body: &[u8]) -> Result<()> {
        let d: Description = serde_json::from_slice(body).map_err(|e| Error::invalid_input(format!("the RTP stream's description is not valid: {e}")))?;
        let sdp = Sdp::parse(&d.sdp)?;
        if d.tracks.len() > MAX_TRACKS {
            return Err(Error::invalid_input(format!("the description sets up {} tracks, more than {MAX_TRACKS}", d.tracks.len())));
        }
        for (k, setup) in d.tracks.iter().enumerate() {
            if d.tracks[..k].iter().any(|t| t.media == setup.media) {
                return Err(Error::invalid_input(format!("the description sets up media {} twice", setup.media)));
            }
            let m = sdp.media.get(setup.media).ok_or_else(|| Error::invalid_input(format!("the description sets up media {} of {}", setup.media, sdp.media.len())))?;
            if setup.channel >= DESCRIPTION_CHANNEL - 1 || self.tracks.iter().any(|t| t.channel.abs_diff(setup.channel) < 2) {
                return Err(Error::invalid_input(format!("channel {} cannot carry a track", setup.channel)));
            }
            let codec = m.codec().map_err(|why| Error::new(ironav_core::ErrorCode::UnsupportedCodec, format!("RTP {why} is not supported")))?;
            let (payload, info, params) = track_for(m, codec)?;
            let pcm = if codec == CodecId::PcmS16Le { m.channels } else { 0 };
            self.tracks.push(Track {
                channel: setup.channel,
                payload_type: m.payload,
                payload,
                clock: m.clock,
                pcm,
                info,
                index: None,
                params,
                frame_rate: m.framerate.map_or(Rational::new(25, 1), |f| if f.fract() == 0.0 { Rational::new(f as i64, 1) } else { Rational::new((f * 1000.0).round() as i64, 1000) }),
                expected: None,
                held: BTreeMap::new(),
                base: setup.rtptime,
                last: None,
                au: Vec::new(),
                au_ts: None,
                au_key: false,
                au_sps: false,
                au_bad: false,
                fu: None,
                aac_frag: None,
                started: false,
                lost: 0,
            });
        }
        if self.tracks.is_empty() {
            return Err(Error::new(ironav_core::ErrorCode::UnsupportedCodec, "the RTSP session set up no track IronAV can read"));
        }
        self.described = true;
        Ok(())
    }

    fn rtp(&mut self, channel: u8, body: &[u8]) -> Result<()> {
        let Some(ti) = self.tracks.iter().position(|t| t.channel == channel) else {
            return Ok(()); // RTCP, or a track that was not set up
        };
        if body.len() < 12 || body[0] >> 6 != 2 {
            return Ok(()); // not RTP version 2: dropped like any damaged datagram
        }
        let seq = u16::from_be_bytes([body[2], body[3]]);
        let t = &mut self.tracks[ti];
        let Some(expected) = t.expected else {
            t.expected = Some(seq.wrapping_add(1));
            return self.packet(ti, body);
        };
        let ahead = seq.wrapping_sub(expected) as i16 as i32;
        if ahead.abs() > RESYNC {
            // the sender restarted its numbering: what was held can no longer be placed
            t.lost += t.held.len() as u64;
            t.held.clear();
            t.expected = Some(seq.wrapping_add(1));
            self.loss(ti);
            return self.packet(ti, body);
        }
        if ahead < 0 {
            return Ok(()); // late or repeated
        }
        if ahead > 0 {
            t.held.insert(seq, body.to_vec());
            if t.held.len() <= REORDER {
                return Ok(());
            }
            // give up on what is missing: carry on from the earliest packet held
            let next = *t.held.keys().min_by_key(|&&s| s.wrapping_sub(expected)).unwrap();
            t.lost += next.wrapping_sub(expected) as u64;
            t.expected = Some(next);
            self.loss(ti);
        } else {
            t.expected = Some(seq.wrapping_add(1));
            self.packet(ti, body)?;
        }
        // release what now follows in order
        loop {
            let t = &mut self.tracks[ti];
            let e = t.expected.unwrap();
            let Some(p) = t.held.remove(&e) else { break };
            t.expected = Some(e.wrapping_add(1));
            self.packet(ti, &p)?;
        }
        Ok(())
    }

    /// Something was lost: the picture being put together is damaged, and so is the next one if
    /// none was (the lost packet may have begun it), so no damaged picture reaches a decoder.
    fn loss(&mut self, ti: usize) {
        let t = &mut self.tracks[ti];
        t.fu = None;
        t.aac_frag = None;
        t.au_bad = true;
    }

    /// One RTP packet, in order.
    fn packet(&mut self, ti: usize, p: &[u8]) -> Result<()> {
        let t = &mut self.tracks[ti];
        let (marker, pt) = (p[1] & 0x80 != 0, p[1] & 0x7F);
        if pt != t.payload_type {
            return Ok(());
        }
        let ts = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
        let mut at = 12 + 4 * (p[0] & 15) as usize;
        if p[0] & 0x10 != 0 {
            let Some(words) = p.get(at + 2..at + 4) else { return Ok(()) };
            at += 4 + 4 * u16::from_be_bytes([words[0], words[1]]) as usize;
        }
        let mut end = p.len();
        if p[0] & 0x20 != 0 {
            end = end.saturating_sub(*p.last().unwrap() as usize);
        }
        if at > end {
            return Ok(());
        }
        let data = &p[at..end];
        match t.payload {
            Payload::Video { hevc } => {
                if t.au_ts.is_some_and(|a| a != ts) {
                    self.flush_au(ti);
                }
                let t = &mut self.tracks[ti];
                t.au_ts = Some(ts);
                video(t, data, hevc);
                if marker {
                    self.flush_au(ti);
                }
            }
            Payload::Aac { size_len, index_len, delta_len, frame } => {
                for (k, f) in aac(t, data, ts, size_len, index_len, delta_len) {
                    self.emit(ti, ts, k as i64 * frame, f, true);
                }
            }
            Payload::Opus => {
                if !data.is_empty() {
                    self.emit(ti, ts, 0, data.to_vec(), true);
                }
            }
            Payload::G711 { alaw } => {
                let pcm = data.iter().flat_map(|&b| g711(b, alaw).to_le_bytes()).collect();
                self.emit(ti, ts, 0, pcm, true);
            }
            Payload::L16 => {
                let pcm = data.chunks_exact(2).flat_map(|s| [s[1], s[0]]).collect();
                self.emit(ti, ts, 0, pcm, true);
            }
        }
        Ok(())
    }

    fn flush_au(&mut self, ti: usize) {
        let t = &mut self.tracks[ti];
        let Some(ts) = t.au_ts.take() else { return };
        let (mut au, key, sps, bad) = (std::mem::take(&mut t.au), t.au_key, t.au_sps, t.au_bad);
        (t.au_key, t.au_sps, t.au_bad) = (false, false, false);
        t.fu = None;
        if au.is_empty() || bad {
            return;
        }
        if key && !sps && !t.params.is_empty() {
            au.splice(0..0, t.params.iter().copied());
        }
        if !t.started {
            if !key {
                return;
            }
            t.started = true;
        }
        self.emit(ti, ts, 0, au, key);
    }

    /// A packet of track `ti` at RTP time `ts` plus `offset` ticks.
    fn emit(&mut self, ti: usize, ts: u32, offset: i64, data: Vec<u8>, key: bool) {
        let t = &mut self.tracks[ti];
        let base = *t.base.get_or_insert(ts);
        let time = match t.last {
            Some((last_ts, last_time)) => last_time.saturating_add(ts.wrapping_sub(last_ts) as i32 as i64),
            None => ts.wrapping_sub(base) as i32 as i64,
        };
        t.last = Some((ts, time));
        let duration = if t.pcm > 0 { (data.len() / (2 * t.pcm as usize)) as i64 } else { 0 };
        let packet = Packet { stream: ti, pts: time.saturating_add(offset), duration, data };
        if self.published {
            if let Some(index) = self.tracks[ti].index {
                self.out.push_back(StreamPacket { packet: Packet { stream: index, ..packet }, key });
            }
            return;
        }
        self.held_bytes += packet.data.len();
        self.held.push_back((ti, packet, key));
        if self.tracks.iter().all(|t| t.info.is_some()) || self.held.len() >= MAX_HELD || self.held_bytes >= MAX_HELD_BYTES {
            self.publish();
        }
    }

    fn publish(&mut self) {
        for t in &mut self.tracks {
            if let Some(mut info) = t.info.take() {
                info.index = self.streams.len();
                if let StreamParams::Video { frame_rate, .. } = &mut info.params {
                    *frame_rate = t.frame_rate;
                }
                t.index = Some(self.streams.len());
                self.streams.push(info);
            }
        }
        self.published = true;
        self.held_bytes = 0;
        for (ti, p, key) in std::mem::take(&mut self.held) {
            if let Some(index) = self.tracks[ti].index {
                self.out.push_back(StreamPacket { packet: Packet { stream: index, ..p }, key });
            }
        }
    }
}

/// How a track's payload is read, its description when the SDP gives all of it, and the parameter
/// sets to put in front of keyframes.
fn track_for(m: &Media, codec: CodecId) -> Result<(Payload, Option<StreamInfo>, Vec<u8>)> {
    let audio = |codec, rate: u32, channels: u16, bits, extradata| StreamInfo {
        index: 0,
        codec,
        time_base: Rational::new(1, m.clock.max(1) as i64),
        duration: None,
        bits,
        extradata,
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: rate, channels },
    };
    if m.clock == 0 {
        return Err(Error::invalid_input(format!("the SDP gives the {} track no clock rate", m.encoding)));
    }
    Ok(match codec {
        CodecId::H264 | CodecId::Hevc => {
            let hevc = codec == CodecId::Hevc;
            let keys: &[&str] = if hevc { &["sprop-vps", "sprop-sps", "sprop-pps"] } else { &["sprop-parameter-sets"] };
            let mut params = Vec::new();
            let mut size = None;
            for k in keys {
                for set in m.fmtp.get(*k).map(String::as_str).unwrap_or("").split(',').filter_map(base64).filter(|s| !s.is_empty()) {
                    size = size.or_else(|| ironav_codec::sps_picture_size(codec, &set));
                    params.extend_from_slice(&[0, 0, 0, 1]);
                    params.extend_from_slice(&set);
                }
            }
            let info = size.map(|(w, h)| video_info(codec, m.clock, w, h));
            (Payload::Video { hevc }, info, params)
        }
        CodecId::Aac => {
            let config = m.fmtp.get("config").and_then(|c| hex(c)).filter(|c| c.len() >= 2).ok_or_else(|| Error::invalid_input("the AAC track's config is not hex"))?;
            let num = |k: &str, d: u32| m.fmtp.get(k).and_then(|v| v.parse::<u32>().ok()).unwrap_or(d);
            let lbr = m.fmtp.get("mode").is_some_and(|v| v.eq_ignore_ascii_case("aac-lbr"));
            let (size_len, index_len, delta_len) = (num("sizelength", if lbr { 6 } else { 13 }), num("indexlength", if lbr { 2 } else { 3 }), num("indexdeltalength", if lbr { 2 } else { 3 }));
            if size_len == 0 || size_len > 16 || index_len > 8 || delta_len > 8 {
                return Err(Error::invalid_input("the AAC track's AU header lengths are out of range"));
            }
            let mut b = Bits { d: &config, at: 0 };
            let aot = b.read(5).unwrap_or(0);
            let frame = if num("constantduration", 0) > 0 { num("constantduration", 0) as i64 } else if config[1] & 4 != 0 && aot != 39 { 960 } else { 1024 };
            let sfi = b.read(4).unwrap_or(15);
            if sfi == 15 {
                b.read(24);
            }
            let ch = b.read(4).unwrap_or(0) as u16;
            let channels = if m.channels > 1 || ch == 0 { m.channels } else if ch == 7 { 8 } else { ch };
            (Payload::Aac { size_len, index_len, delta_len, frame }, Some(audio(CodecId::Aac, m.clock, channels, None, config)), Vec::new())
        }
        CodecId::Opus => {
            let channels = if m.fmtp.get("sprop-stereo").is_some_and(|s| s == "0") { 1 } else { 2 };
            let mut head = b"OpusHead".to_vec();
            head.extend_from_slice(&[1, channels as u8, 0, 0]);
            head.extend_from_slice(&48_000u32.to_le_bytes());
            head.extend_from_slice(&[0, 0, 0]);
            (Payload::Opus, Some(audio(CodecId::Opus, 48_000, channels, None, head)), Vec::new())
        }
        _ => {
            let payload = match m.encoding.as_str() {
                "PCMU" => Payload::G711 { alaw: false },
                "PCMA" => Payload::G711 { alaw: true },
                _ => Payload::L16,
            };
            (payload, Some(audio(CodecId::PcmS16Le, m.clock, m.channels, Some(16), Vec::new())), Vec::new())
        }
    })
}

fn video_info(codec: CodecId, clock: u32, width: u32, height: u32) -> StreamInfo {
    StreamInfo {
        index: 0,
        codec,
        time_base: Rational::new(1, clock as i64),
        duration: None,
        bits: None,
        extradata: Vec::new(),
        lead_in: 0,
        params: StreamParams::Video { width, height, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) },
    }
}

/// Adds the NAL units of one H.264 or HEVC payload to the access unit being put together.
fn video(t: &mut Track, p: &[u8], hevc: bool) {
    let hdr = if hevc { 2 } else { 1 };
    if p.len() < hdr {
        return;
    }
    let kind = if hevc { (p[0] >> 1) & 63 } else { p[0] & 31 };
    let mut nals: Vec<Vec<u8>> = Vec::new();
    match (hevc, kind) {
        (false, 1..=23) | (true, 0..=47) => nals.push(p.to_vec()),
        (false, 24) | (true, 48) => {
            let mut i = hdr;
            while i + 2 <= p.len() {
                let n = u16::from_be_bytes([p[i], p[i + 1]]) as usize;
                i += 2;
                let Some(nal) = p.get(i..i + n) else {
                    t.au_bad = true;
                    return;
                };
                nals.push(nal.to_vec());
                i += n;
            }
        }
        (false, 28) | (true, 49) => {
            let Some(&fu) = p.get(hdr) else { return };
            let (start, end, inner) = (fu & 0x80 != 0, fu & 0x40 != 0, fu & if hevc { 63 } else { 31 });
            if start {
                if t.fu.is_some() {
                    t.au_bad = true; // the end of the last one never came
                }
                t.fu = Some(if hevc { vec![(p[0] & 0x81) | (inner << 1), p[1]] } else { vec![(p[0] & 0xE0) | inner] });
            }
            let Some(f) = t.fu.as_mut() else {
                t.au_bad = true; // the start was lost
                return;
            };
            if f.len() + p.len() > MAX_UNIT {
                t.fu = None;
                t.au_bad = true;
                return;
            }
            f.extend_from_slice(&p[hdr + 1..]);
            if end {
                nals.push(t.fu.take().unwrap());
            }
        }
        _ => return, // STAP-B, MTAP, FU-B, PACI: interleaved mode only
    }
    for nal in nals.into_iter().filter(|n| n.len() >= hdr) {
        let kind = if hevc { (nal[0] >> 1) & 63 } else { nal[0] & 31 };
        let codec = if hevc { CodecId::Hevc } else { CodecId::H264 };
        let sps = kind == if hevc { 33 } else { 7 };
        if sps {
            t.au_sps = true;
            if t.info.is_none() && t.index.is_none() {
                if let Some((w, h)) = ironav_codec::sps_picture_size(codec, &nal) {
                    t.info = Some(video_info(codec, t.clock, w, h));
                }
            }
        }
        t.au_key |= if hevc { (16..=23).contains(&kind) } else { kind == 5 };
        if t.au.len() + nal.len() + 4 > MAX_UNIT {
            t.au_bad = true;
            return;
        }
        t.au.extend_from_slice(&[0, 0, 0, 1]);
        t.au.extend_from_slice(&nal);
    }
}

/// The whole AAC frames in one RFC 3640 payload, with each one's place among them; a frame split
/// over packets comes out with the packet that ends it.
fn aac(t: &mut Track, p: &[u8], ts: u32, size_len: u32, index_len: u32, delta_len: u32) -> Vec<(usize, Vec<u8>)> {
    let Some(head) = p.get(..2) else { return Vec::new() };
    let header_bits = u16::from_be_bytes([head[0], head[1]]) as usize;
    let Some(headers) = p.get(2..2 + header_bits.div_ceil(8)) else { return Vec::new() };
    let mut data = &p[2 + header_bits.div_ceil(8)..];
    let mut b = Bits { d: headers, at: 0 };
    let mut sizes = Vec::new();
    while b.at < header_bits {
        let Some(size) = b.read(size_len) else { break };
        if b.read(if sizes.is_empty() { index_len } else { delta_len }).is_none() {
            break;
        }
        sizes.push(size as usize);
    }
    let mut frames = Vec::new();
    // the rest of a frame split over packets: one header giving the whole size
    if let Some((fts, size, mut part)) = t.aac_frag.take() {
        if fts == ts && sizes.len() == 1 && sizes[0] == size {
            part.extend_from_slice(data);
            if part.len() >= size {
                part.truncate(size);
                frames.push((0, part));
            } else if part.len() < MAX_UNIT {
                t.aac_frag = Some((fts, size, part));
            }
            return frames;
        }
    }
    for (k, &size) in sizes.iter().enumerate() {
        if size <= data.len() {
            frames.push((k, data[..size].to_vec()));
            data = &data[size..];
        } else {
            if sizes.len() == 1 && size <= MAX_UNIT {
                t.aac_frag = Some((ts, size, data.to_vec()));
            }
            break;
        }
    }
    frames
}

impl StreamDemuxer for RtpStream {
    fn format_name(&self) -> &'static str {
        "rtp"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(bytes);
        loop {
            let d = &self.buf[self.pos..];
            if d.len() < 4 {
                break;
            }
            if d[0] != b'$' {
                return Err(Error::invalid_input("the RTP stream lost its framing (a frame does not start with '$')"));
            }
            let (channel, len) = (d[1], u16::from_be_bytes([d[2], d[3]]) as usize);
            if d.len() < 4 + len {
                break;
            }
            let body = d[4..4 + len].to_vec();
            self.pos += 4 + len;
            if channel == DESCRIPTION_CHANNEL {
                if self.described {
                    return Err(Error::invalid_input("the RTP stream was described twice"));
                }
                self.describe(&body)?;
            } else if !self.described {
                return Err(Error::invalid_input("RTP arrived before the stream's description"));
            } else {
                self.rtp(channel, &body)?;
            }
        }
        self.buf.drain(..self.pos);
        self.pos = 0;
        Ok(())
    }
    fn next_packet(&mut self) -> Result<Option<StreamPacket>> {
        Ok(self.out.pop_front())
    }
    fn end(&mut self) {
        for ti in 0..self.tracks.len() {
            // what was held for a late packet comes out as it is, and so does the last picture
            let held = std::mem::take(&mut self.tracks[ti].held);
            let mut held: Vec<_> = held.into_iter().collect();
            let e = self.tracks[ti].expected.unwrap_or(0);
            held.sort_by_key(|(s, _)| s.wrapping_sub(e));
            for (s, p) in held {
                let t = &mut self.tracks[ti];
                let gap = s.wrapping_sub(t.expected.unwrap_or(s));
                if gap > 0 {
                    t.lost += gap as u64;
                    self.loss(ti);
                }
                self.tracks[ti].expected = Some(s.wrapping_add(1));
                let _ = self.packet(ti, &p);
            }
            self.flush_au(ti);
        }
        if !self.published && !self.held.is_empty() {
            self.publish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::Demuxer;

    fn sdp_for(sps_pps: &[Vec<u8>], asc: &[u8], rate: u32, channels: u16) -> String {
        let b64 = |d: &[u8]| -> String {
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut s = String::new();
            for c in d.chunks(3) {
                let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
                for i in 0..4 {
                    s.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
                }
            }
            s
        };
        let sets = sps_pps.iter().map(|s| b64(s)).collect::<Vec<_>>().join(",");
        let config: String = asc.iter().map(|b| format!("{b:02x}")).collect();
        format!(
            "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=test\r\nt=0 0\r\na=control:*\r\n\
             m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 packetization-mode=1;sprop-parameter-sets={sets}\r\na=control:trackID=0\r\n\
             m=audio 0 RTP/AVP 97\r\na=rtpmap:97 MPEG4-GENERIC/{rate}/{channels}\r\na=fmtp:97 streamtype=5;mode=AAC-hbr;config={config};sizeLength=13;indexLength=3;indexDeltaLength=3\r\na=control:trackID=1\r\n"
        )
    }

    fn nal_units(au: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut i = 0;
        let mut start = None;
        while i + 3 <= au.len() {
            if au[i..i + 3] == [0, 0, 1] {
                if let Some(s) = start {
                    let mut e = i;
                    while e > s && au[e - 1] == 0 {
                        e -= 1;
                    }
                    out.push(au[s..e].to_vec());
                }
                i += 3;
                start = Some(i);
            } else {
                i += 1;
            }
        }
        if let Some(s) = start {
            out.push(au[s..].to_vec());
        }
        out
    }

    fn rtp(pt: u8, seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x80, pt | if marker { 0x80 } else { 0 }];
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&0x1234_5678u32.to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    /// Packetizes like a camera would: small NAL units aggregated in STAP-A, large ones in FU-A.
    fn packetize_h264(nals: &[Vec<u8>], mtu: usize) -> Vec<(Vec<u8>, bool)> {
        let mut out: Vec<(Vec<u8>, bool)> = Vec::new();
        let mut stap: Vec<u8> = Vec::new();
        let flush = |stap: &mut Vec<u8>, out: &mut Vec<(Vec<u8>, bool)>| {
            if !stap.is_empty() {
                out.push((std::mem::take(stap), false));
            }
        };
        for nal in nals {
            if nal.len() + 3 < mtu / 4 {
                if stap.is_empty() {
                    stap.push(24 | (nal[0] & 0x60));
                }
                stap.extend_from_slice(&(nal.len() as u16).to_be_bytes());
                stap.extend_from_slice(nal);
                if stap.len() > mtu / 2 {
                    flush(&mut stap, &mut out);
                }
            } else if nal.len() <= mtu {
                flush(&mut stap, &mut out);
                out.push((nal.clone(), false));
            } else {
                flush(&mut stap, &mut out);
                let body = &nal[1..];
                let chunks: Vec<&[u8]> = body.chunks(mtu - 2).collect();
                for (i, c) in chunks.iter().enumerate() {
                    let mut p = vec![(nal[0] & 0xE0) | 28, nal[0] & 31];
                    if i == 0 {
                        p[1] |= 0x80;
                    }
                    if i == chunks.len() - 1 {
                        p[1] |= 0x40;
                    }
                    p.extend_from_slice(c);
                    out.push((p, false));
                }
            }
        }
        flush(&mut stap, &mut out);
        if let Some(l) = out.last_mut() {
            l.1 = true;
        }
        out
    }

    struct Source {
        description: Vec<u8>,
        /// (channel, rtp packet) in sending order
        packets: Vec<(u8, Vec<u8>)>,
        video: Vec<(i64, Vec<Vec<u8>>)>,
        audio: Vec<(i64, Vec<u8>)>,
    }

    /// The H.264 and AAC of a transport stream, as an RTSP server would send them.
    fn source() -> Source {
        let path = format!("{}/tests/data/h264_aac.ts", env!("CARGO_MANIFEST_DIR"));
        let mut d = crate::ts::TsDemuxer::open(std::io::BufReader::new(std::fs::File::open(path).unwrap())).unwrap();
        let streams = d.streams().to_vec();
        let vi = streams.iter().position(|s| s.codec == CodecId::H264).unwrap();
        let ai = streams.iter().position(|s| s.codec == CodecId::Aac).unwrap();
        let StreamParams::Audio { sample_rate, channels } = streams[ai].params else { unreachable!() };
        let vtb = streams[vi].time_base;
        let mut video = Vec::new();
        let mut audio = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            if p.stream == vi {
                // RTP carries no access unit delimiters
                let nals: Vec<Vec<u8>> = nal_units(&p.data).into_iter().filter(|n| n[0] & 31 != 9).collect();
                video.push((p.pts * 90_000 * vtb.num / vtb.den, nals));
            } else if p.stream == ai {
                // every frame is 1024 samples, so the times are counted rather than converted
                audio.push((audio.len() as i64 * 1024, p.data));
            }
        }
        let sps_pps: Vec<Vec<u8>> = video.iter().flat_map(|(_, n)| n.iter()).filter(|n| matches!(n[0] & 31, 7 | 8)).take(2).cloned().collect();
        let sdp = sdp_for(&sps_pps, &streams[ai].extradata, sample_rate, channels);
        let (v0, a0) = (1000u32, 50_000u32);
        let description = description_frame(&Description {
            sdp,
            tracks: vec![TrackSetup { media: 0, channel: 0, rtptime: Some(v0.wrapping_add(video[0].0 as u32)), seq: Some(65000) }, TrackSetup { media: 1, channel: 2, rtptime: Some(a0.wrapping_add(audio[0].0 as u32)), seq: Some(7) }],
        })
        .unwrap();
        // interleave by time, as a server does
        let mut timeline: Vec<(f64, u8, Vec<u8>)> = Vec::new();
        let mut vseq = 65000u16;
        for (pts, nals) in &video {
            for (payload, marker) in packetize_h264(nals, 1200) {
                timeline.push((*pts as f64 / 90_000.0, 0, rtp(96, vseq, v0.wrapping_add(*pts as u32), marker, &payload)));
                vseq = vseq.wrapping_add(1);
            }
        }
        let mut aseq = 7u16;
        for chunk in audio.chunks(3) {
            // three AAC frames per packet, each with a 16-bit AU header (13-bit size, 3-bit index)
            let mut payload = ((chunk.len() * 16) as u16).to_be_bytes().to_vec();
            for (_, f) in chunk {
                payload.extend_from_slice(&((f.len() as u16) << 3).to_be_bytes());
            }
            for (_, f) in chunk {
                payload.extend_from_slice(f);
            }
            timeline.push((chunk[0].0 as f64 / sample_rate as f64, 2, rtp(97, aseq, a0.wrapping_add(chunk[0].0 as u32), true, &payload)));
            aseq = aseq.wrapping_add(1);
        }
        timeline.sort_by(|a, b| a.0.total_cmp(&b.0));
        Source { description, packets: timeline.into_iter().map(|(_, c, p)| (c, p)).collect(), video, audio }
    }

    fn run(desc: &[u8], frames: &[Vec<u8>], piece: usize) -> (Vec<StreamInfo>, Vec<StreamPacket>, u64) {
        let mut bytes = desc.to_vec();
        for f in frames {
            bytes.extend_from_slice(f);
        }
        let mut s = RtpStream::default();
        let mut got = Vec::new();
        for c in bytes.chunks(piece) {
            s.feed(c).unwrap();
            while let Some(p) = s.next_packet().unwrap() {
                got.push(p);
            }
        }
        s.end();
        while let Some(p) = s.next_packet().unwrap() {
            got.push(p);
        }
        (s.streams().to_vec(), got, s.lost())
    }

    fn check(src: &Source, streams: &[StreamInfo], got: &[StreamPacket], skip_video: usize) {
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].codec, CodecId::H264);
        assert!(matches!(streams[0].params, StreamParams::Video { width, height, .. } if width > 0 && height > 0));
        assert_eq!(streams[1].codec, CodecId::Aac);
        let v: Vec<&StreamPacket> = got.iter().filter(|p| p.packet.stream == 0).collect();
        let a: Vec<&StreamPacket> = got.iter().filter(|p| p.packet.stream == 1).collect();
        assert_eq!(v.len(), src.video.len() - skip_video);
        let v0 = src.video[0].0;
        for (g, (pts, nals)) in v.iter().zip(&src.video[skip_video..]) {
            assert_eq!(g.packet.pts, pts - v0);
            let mut want = nals.clone();
            let have = nal_units(&g.packet.data);
            if have.len() > want.len() {
                // parameter sets were put in front of a keyframe that came without them
                assert!(g.key);
                want.splice(0..0, have[..have.len() - want.len()].iter().cloned());
            }
            assert_eq!(have, want);
        }
        assert!(v[0].key);
        assert_eq!(a.len(), src.audio.len());
        let a0 = src.audio[0].0;
        for (g, (pts, f)) in a.iter().zip(&src.audio) {
            assert_eq!((g.packet.pts, &g.packet.data), (pts - a0, f));
        }
    }

    #[test]
    fn interleaved_rtp_gives_the_streams_packets_back() {
        let src = source();
        let frames: Vec<Vec<u8>> = src.packets.iter().map(|(c, p)| frame(*c, p).unwrap()).collect();
        for piece in [1, 7, 1500, usize::MAX] {
            let (streams, got, lost) = run(&src.description, &frames, piece.min(1 << 24));
            assert_eq!(lost, 0);
            check(&src, &streams, &got, 0);
        }
    }

    #[test]
    fn reordered_packets_are_put_back_and_a_loss_drops_only_its_picture() {
        let src = source();
        let mut frames: Vec<Vec<u8>> = src.packets.iter().map(|(c, p)| frame(*c, p).unwrap()).collect();
        // swap neighbours throughout, as UDP across a busy network might
        for i in (10..frames.len() - 1).step_by(5) {
            frames.swap(i, i + 1);
        }
        let (streams, got, lost) = run(&src.description, &frames, 4096);
        assert_eq!(lost, 0);
        check(&src, &streams, &got, 0);

        // lose one packet of a picture in the middle: only that picture goes
        let (vi, _) = src.packets.iter().enumerate().filter(|(_, (c, _))| *c == 0).nth(40).unwrap();
        let mut frames: Vec<Vec<u8>> = src.packets.iter().map(|(c, p)| frame(*c, p).unwrap()).collect();
        frames.remove(vi);
        let (_, got, lost) = run(&src.description, &frames, 4096);
        assert_eq!(lost, 1);
        // the damaged picture goes, and the next one too when the lost packet may have begun it
        let v = got.iter().filter(|p| p.packet.stream == 0).count();
        assert!((src.video.len() - 2..src.video.len()).contains(&v), "{v} of {}", src.video.len());
        assert_eq!(got.iter().filter(|p| p.packet.stream == 1).count(), src.audio.len());
    }

    #[test]
    fn ffmpegs_packetization_reads_back() {
        // FFmpeg's RTP muxer sent 3 s of H.264 (B-frames, FU-A) and AAC (AAC-hbr), captured and framed
        let d = std::fs::read(format!("{}/tests/data/h264_aac.rtp", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let (streams, got, lost) = run(&d, &[], 977);
        assert_eq!(lost, 0);
        assert_eq!(streams.iter().map(|s| s.codec).collect::<Vec<_>>(), [CodecId::H264, CodecId::Aac]);
        assert!(matches!(streams[0].params, StreamParams::Video { width: 320, height: 240, .. }));
        let v: Vec<&StreamPacket> = got.iter().filter(|p| p.packet.stream == 0).collect();
        let a: Vec<i64> = got.iter().filter(|p| p.packet.stream == 1).map(|p| p.packet.pts).collect();
        assert_eq!((v.len(), a.len()), (75, 141));
        assert!(v[0].key && v.iter().filter(|p| p.key).count() == 3, "an IDR picture each second");
        // presentation times: every picture once, 3600 ticks apart, though sent in decode order
        let mut pts: Vec<i64> = v.iter().map(|p| p.packet.pts).collect();
        pts.sort();
        assert!(pts.windows(2).all(|w| w[1] - w[0] == 3600), "{pts:?}");
        assert!(a.windows(2).all(|w| w[1] - w[0] == 1024));
    }

    #[test]
    fn sdp_and_payload_details() {
        let s = Sdp::parse("v=0\r\na=control:rtsp://cam/live\r\nm=audio 0 RTP/AVP 0\r\na=control:track1\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 packetization-mode=2\r\nm=audio 0 RTP/AVP 98\r\na=rtpmap:98 opus/48000/2\r\na=fmtp:98 sprop-stereo=0\r\n").unwrap();
        assert_eq!(s.control.as_deref(), Some("rtsp://cam/live"));
        assert_eq!((s.media[0].encoding.as_str(), s.media[0].clock, s.media[0].codec()), ("PCMU", 8000, Ok(CodecId::PcmS16Le)));
        assert!(s.media[1].codec().is_err(), "interleaved mode is refused");
        assert_eq!(s.media[2].codec(), Ok(CodecId::Opus));
        // G.711 reference points (ITU-T G.711 tables)
        assert_eq!((g711(0xFF, false), g711(0x7F, false), g711(0x00, false), g711(0x80, false)), (0, 0, -32124, 32124));
        assert_eq!((g711(0xD5, true), g711(0x55, true), g711(0x2A, true), g711(0xAA, true)), (8, -8, -32256, 32256));
        assert_eq!(base64("Z0IAKeKQFAe2AtwEBAaQeJEV"), Some(vec![0x67, 0x42, 0x00, 0x29, 0xe2, 0x90, 0x14, 0x07, 0xb6, 0x02, 0xdc, 0x04, 0x04, 0x06, 0x90, 0x78, 0x91, 0x15]));
    }

    #[test]
    fn damage_never_panics() {
        let src = source();
        let good: Vec<u8> = std::iter::once(src.description.clone()).chain(src.packets.iter().take(300).map(|(c, p)| frame(*c, p).unwrap())).flatten().collect();
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..300 {
            let mut d = good.clone();
            for _ in 0..1 + round % 20 {
                let i = src.description.len() + rand() as usize % (d.len() - src.description.len());
                d[i] = rand() as u8;
            }
            let mut s = RtpStream::default();
            for c in d.chunks(1 + rand() as usize % 2000) {
                if s.feed(c).is_err() {
                    break;
                }
                while s.next_packet().unwrap().is_some() {}
            }
            s.end();
        }
    }
}
