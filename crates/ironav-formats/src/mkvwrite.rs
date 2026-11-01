//! Matroska writer (`.mkv`, `.mka`, and `.webm` for VP8, VP9 or AV1 video and Opus or Vorbis audio): H.264, HEVC, VP8, VP9,
//! AV1 or Motion-JPEG video and AAC, FLAC, MP3, Opus, Vorbis or PCM audio, any number of tracks of each kind.
//! PCM is written as `A_PCM/INT/LIT` (or `A_PCM/FLOAT/IEEE`) with its sample size as BitDepth.
//!
//! Layout: EBML header, Segment (its size patched at the end), Info, Tracks, Clusters, Cues. The
//! timestamp scale is one millisecond. Nothing is written until every stream has delivered a
//! packet and the video's parameter sets have been seen, because the Tracks element needs them
//! (the packets that arrive meanwhile are held). A cluster is gathered in memory and written whole,
//! so no element has an unknown size: a new one starts every five seconds and at a video key
//! frame once a second has passed. Cues list the first video key frame of every cluster (or the
//! cluster itself, without video).
//!
//! Video comes in as Annex B access units and goes out length-prefixed, with the `avcC` / `hvcC`
//! record as CodecPrivate. AV1 keeps its `av1C` record as CodecPrivate and loses its temporal
//! delimiter and padding OBUs; VP8 and VP9 go in as they are. There is no SeekHead, no tags and no chapters.

use crate::flac::{flac_audio_params, FlacStats};
use crate::mp4::{annexb_sample, avcc_body, hvcc_body, ParamSets};
use crate::vpxav1::{av1_sample, vp9_is_key};
use crate::WriteSeek;
use ironav_core::{CodecId, Error, ErrorCode, Muxer, Packet, Result, StreamInfo, StreamParams};
use std::io::{Seek, SeekFrom};

/// An EBML element ID as its big-endian bytes.
fn id_bytes(id: u32) -> Vec<u8> {
    let b = id.to_be_bytes();
    let skip = b.iter().position(|&x| x != 0).unwrap_or(3);
    b[skip..].to_vec()
}

/// An EBML size field in the fewest bytes.
fn size_bytes(n: u64) -> Vec<u8> {
    for len in 1..=8u32 {
        // all-ones is reserved for "unknown"
        if n < (1u64 << (7 * len)) - 1 {
            let v = n | (1u64 << (7 * len));
            return v.to_be_bytes()[8 - len as usize..].to_vec();
        }
    }
    unreachable!("sizes are below 2^56")
}

fn el(id: u32, body: &[u8]) -> Vec<u8> {
    let mut v = id_bytes(id);
    v.extend(size_bytes(body.len() as u64));
    v.extend_from_slice(body);
    v
}

fn uint(id: u32, n: u64) -> Vec<u8> {
    let b = n.to_be_bytes();
    let skip = b.iter().position(|&x| x != 0).unwrap_or(7);
    el(id, &b[skip..])
}

fn float(id: u32, f: f64) -> Vec<u8> {
    el(id, &f.to_be_bytes())
}

fn string(id: u32, s: &str) -> Vec<u8> {
    el(id, s.as_bytes())
}

const SEGMENT: u32 = 0x1853_8067;
const CLUSTER: u32 = 0x1F43_B675;

struct Track {
    stream: StreamInfo,
    video: bool,
    params: ParamSets,
    flac: FlacStats,
    /// Packets seen so far (before the header is out, they are held).
    seen: u64,
    /// The first packet's duration in stream time units, for DefaultDuration.
    first_duration: i64,
    /// The decoded length of all packets so far, in stream time units (AAC's discard padding is what
    /// is left over once the codec delay and the stream's length are taken off).
    duration_sum: i64,
    /// The AV1 sequence header's `reduced_still_picture_header`.
    av1_still: bool,
}

/// Video that goes in as it comes, with no parameter sets to collect first.
/// The sample size in bits of a PCM codec (`None` for other codecs).
fn pcm_depth(codec: CodecId) -> Option<u8> {
    Some(match codec {
        CodecId::PcmU8 => 8,
        CodecId::PcmS16Le => 16,
        CodecId::PcmS24Le => 24,
        CodecId::PcmS32Le | CodecId::PcmF32Le => 32,
        CodecId::PcmF64Le => 64,
        _ => return None,
    })
}

/// Whether a track is AAC at its sample rate with a priming (lead-in), so its last packet may carry the
/// end padding (a discard padding) and is held back until the padding is known.
fn holds_aac_end(t: &Track) -> bool {
    let s = &t.stream;
    let samples = match s.params {
        StreamParams::Audio { sample_rate, .. } => s.time_base.num == 1 && s.time_base.den == sample_rate as i64,
        _ => false,
    };
    s.codec == CodecId::Aac && s.lead_in > 0 && samples && !t.video
}

fn self_contained(codec: CodecId) -> bool {
    matches!(codec, CodecId::Vp8 | CodecId::Vp9 | CodecId::Av1 | CodecId::Mjpeg | CodecId::Mpeg4)
}

struct Cluster {
    /// Start in milliseconds.
    tc: i64,
    body: Vec<u8>,
    /// Cue points (time in ms, track number) for blocks in this cluster.
    cues: Vec<(i64, usize)>,
}

pub struct MatroskaWriter {
    out: Box<dyn WriteSeek>,
    tracks: Vec<Track>,
    held: Vec<Packet>,
    /// Per track, the last Opus or AAC packet, written at the end so it can carry its padding.
    tails: Vec<Option<Packet>>,
    started: bool,
    segment_data: u64,
    segment_size_at: u64,
    duration_at: u64,
    /// Per FLAC track (its index), the offset of its STREAMINFO in the file, patched when the stream ends.
    flac_info_at: Vec<(usize, u64)>,
    cluster: Option<Cluster>,
    /// (time ms, track number, cluster offset from the segment data)
    cues: Vec<(i64, usize, u64)>,
    end_ms: i64,
    /// WebM holds only VP8, VP9 and AV1 video and Opus and Vorbis audio.
    webm: bool,
}

impl MatroskaWriter {
    pub fn new(out: Box<dyn WriteSeek>) -> Self {
        Self::with_doc_type(out, false)
    }

    /// A writer for `webm` (the same file with the document type `webm` and only the codecs WebM allows).
    pub fn webm(out: Box<dyn WriteSeek>) -> Self {
        Self::with_doc_type(out, true)
    }

    fn with_doc_type(out: Box<dyn WriteSeek>, webm: bool) -> Self {
        Self {
            out,
            tracks: Vec::new(),
            held: Vec::new(),
            tails: Vec::new(),
            started: false,
            segment_data: 0,
            segment_size_at: 0,
            duration_at: 0,
            flac_info_at: Vec::new(),
            cluster: None,
            cues: Vec::new(),
            end_ms: 0,
            webm,
        }
    }

    /// Stream ticks to milliseconds, rounded.
    fn ms(&self, t: usize, ticks: i64) -> i64 {
        let tb = self.tracks[t].stream.time_base;
        let n = ticks as i128 * tb.num as i128 * 1000;
        let d = tb.den.max(1) as i128;
        ((n * 2 + d).div_euclid(2 * d)) as i64
    }

    /// The ticks a packet's block is stamped with. A video track's presentation times run ahead of its
    /// decoding times by its lead-in (an Annex B stream's reordering depth, say), and Matroska has no edit
    /// list to say so, so the lead-in comes off and the first picture is at 0. Audio keeps its times: its
    /// priming is the codec delay instead. A time before the lead-in (none in a stream that starts there)
    /// is stamped 0.
    fn block_ticks(&self, t: usize, pts: i64) -> i64 {
        let tr = &self.tracks[t];
        if tr.video {
            (pts - tr.stream.lead_in).max(0)
        } else {
            pts
        }
    }

    fn pos(&mut self) -> Result<u64> {
        Ok(self.out.stream_position()?)
    }

    fn ready(&self) -> bool {
        self.tracks.iter().all(|t| {
            t.seen > 0 && (!t.video || self_contained(t.stream.codec) || (t.params.sps.is_some() && t.params.pps.is_some() && (t.stream.codec != CodecId::Hevc || t.params.vps.is_some())))
        })
    }

    fn codec_private(t: &Track) -> Result<Option<Vec<u8>>> {
        let missing = |what: &str| Error::new(ErrorCode::InvalidInput, format!("the video stream has no {what} to describe it to Matroska"));
        Ok(match t.stream.codec {
            CodecId::H264 => Some(avcc_body(&t.params, &missing)?),
            CodecId::Hevc => Some(hvcc_body(&t.params, &missing)?),
            CodecId::Vp8 | CodecId::Vp9 | CodecId::Mjpeg => None,
            CodecId::Av1 | CodecId::Mpeg4 | CodecId::Aac | CodecId::Opus | CodecId::Vorbis => Some(t.stream.extradata.clone()),
            CodecId::Flac => {
                let (rate, channels, bits) = flac_audio_params(&t.stream)?;
                let mut v = b"fLaC".to_vec();
                v.extend([0x80, 0, 0, 34]);
                v.extend(t.flac.streaminfo(rate, channels, bits)); // patched when the stream ends
                Some(v)
            }
            _ => None,
        })
    }

    fn start(&mut self) -> Result<()> {
        let mut ebml = Vec::new();
        ebml.extend(uint(0x4286, 1)); // EBMLVersion
        ebml.extend(uint(0x42F7, 1)); // EBMLReadVersion
        ebml.extend(uint(0x42F2, 4)); // EBMLMaxIDLength
        ebml.extend(uint(0x42F3, 8)); // EBMLMaxSizeLength
        ebml.extend(string(0x4282, if self.webm { "webm" } else { "matroska" }));
        ebml.extend(uint(0x4287, 4)); // DocTypeVersion
        ebml.extend(uint(0x4285, 2)); // DocTypeReadVersion
        self.out.write_all(&el(0x1A45_DFA3, &ebml))?;

        // the Segment, its size patched at the end
        self.out.write_all(&id_bytes(SEGMENT))?;
        self.segment_size_at = self.pos()?;
        self.out.write_all(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])?;
        self.segment_data = self.pos()?;

        // Info: the Duration is a fixed-size float (id 2, size 1, value 8) at its end, patched later
        let mut info = uint(0x2A_D7B1, 1_000_000);
        info.extend(string(0x4D80, "IronAV"));
        info.extend(string(0x5741, "IronAV"));
        let start = self.pos()?;
        let header_len = id_bytes(0x1549_A966).len() + size_bytes(info.len() as u64 + 11).len();
        self.duration_at = start + header_len as u64 + info.len() as u64 + 3;
        info.extend(float(0x4489, 0.0));
        self.out.write_all(&el(0x1549_A966, &info))?;

        let mut entries = Vec::new();
        // each FLAC track's STREAMINFO, as an offset from the start of the Tracks body
        let mut flac_rel = Vec::new();
        for (i, t) in self.tracks.iter().enumerate() {
            let s = &t.stream;
            let mut e = uint(0xD7, i as u64 + 1);
            e.extend(uint(0x73C5, i as u64 + 1));
            e.extend(uint(0x83, if t.video { 1 } else { 2 }));
            e.extend(uint(0x9C, 0)); // FlagLacing
            let id = match s.codec {
                CodecId::H264 => "V_MPEG4/ISO/AVC",
                CodecId::Hevc => "V_MPEGH/ISO/HEVC",
                CodecId::Vp8 => "V_VP8",
                CodecId::Vp9 => "V_VP9",
                CodecId::Av1 => "V_AV1",
                CodecId::Mjpeg => "V_MJPEG",
                CodecId::Mpeg4 => "V_MPEG4/ISO/SP",
                CodecId::Aac => "A_AAC",
                CodecId::Flac => "A_FLAC",
                CodecId::Opus => "A_OPUS",
                CodecId::Vorbis => "A_VORBIS",
                CodecId::Mp2 => "A_MPEG/L2",
                CodecId::PcmF32Le | CodecId::PcmF64Le => "A_PCM/FLOAT/IEEE",
                c if c.sample_format().is_some() => "A_PCM/INT/LIT",
                _ => "A_MPEG/L3",
            };
            e.extend(string(0x86, id));
            let mut flac_at = None;
            if let Some(private) = Self::codec_private(t)? {
                if s.codec == CodecId::Flac {
                    // the STREAMINFO follows "fLaC" and its block header (8 bytes)
                    flac_at = Some(e.len() + id_bytes(0x63A2).len() + size_bytes(private.len() as u64).len() + 8);
                }
                e.extend(el(0x63A2, &private));
            }
            match &s.params {
                StreamParams::Video { width, height, frame_rate, .. } => {
                    if frame_rate.num > 0 && frame_rate.den > 0 {
                        let ns = 1_000_000_000u128 * frame_rate.den as u128 / frame_rate.num as u128;
                        e.extend(uint(0x23_E383, ns as u64));
                    }
                    let mut v = uint(0xB0, *width as u64);
                    v.extend(uint(0xBA, *height as u64));
                    e.extend(el(0xE0, &v));
                }
                StreamParams::Audio { sample_rate, channels } => {
                    // a fixed frame length lets readers time the frames without the container
                    if matches!(s.codec, CodecId::Aac | CodecId::Mp3 | CodecId::Mp2) && t.first_duration > 0 {
                        let ns = t.first_duration as u128 * 1_000_000_000 * s.time_base.num as u128 / s.time_base.den as u128;
                        e.extend(uint(0x23_E383, ns as u64));
                    }
                    // AAC's and MPEG audio's priming is their codec delay, which the decoder drops (FFmpeg does too)
                    if matches!(s.codec, CodecId::Aac | CodecId::Mp2 | CodecId::Mp3) && s.lead_in > 0 && s.time_base.num == 1 && s.time_base.den == *sample_rate as i64 {
                        e.extend(uint(0x56AA, s.lead_in as u64 * 1_000_000_000 / *sample_rate as u64));
                    }
                    if s.codec == CodecId::Opus && s.extradata.len() >= 12 {
                        let skip = u16::from_le_bytes([s.extradata[10], s.extradata[11]]) as u64;
                        e.extend(uint(0x56AA, skip * 1_000_000_000 / 48_000)); // CodecDelay
                        e.extend(uint(0x56BB, 80_000_000)); // SeekPreRoll
                    }
                    let mut a = float(0xB5, *sample_rate as f64);
                    a.extend(uint(0x9F, *channels as u64));
                    if s.codec == CodecId::Flac {
                        if let Some(bits) = s.bits {
                            a.extend(uint(0x6264, bits as u64));
                        }
                    }
                    if let Some(bits) = pcm_depth(s.codec) {
                        a.extend(uint(0x6264, bits as u64)); // BitDepth: the sample size
                    }
                    e.extend(el(0xE1, &a));
                }
            }
            if let Some(at) = flac_at {
                flac_rel.push((i, entries.len() + id_bytes(0xAE).len() + size_bytes(e.len() as u64).len() + at));
            }
            entries.extend(el(0xAE, &e));
        }
        let tracks = el(0x1654_AE6B, &entries);
        let tracks_at = self.pos()?;
        self.out.write_all(&tracks)?;
        // the Tracks body starts after its own ID and size
        let body_at = tracks_at + (id_bytes(0x1654_AE6B).len() + size_bytes(entries.len() as u64).len()) as u64;
        self.flac_info_at = flac_rel.into_iter().map(|(i, rel)| (i, body_at + rel as u64)).collect();
        self.started = true;
        Ok(())
    }

    fn close_cluster(&mut self) -> Result<()> {
        if let Some(c) = self.cluster.take() {
            let at = self.pos()? - self.segment_data;
            let mut body = uint(0xE7, c.tc as u64);
            body.extend(c.body);
            self.cues.extend(c.cues.into_iter().map(|(ms, track)| (ms, track, at)));
            self.out.write_all(&id_bytes(CLUSTER))?;
            self.out.write_all(&size_bytes(body.len() as u64))?;
            self.out.write_all(&body)?;
        }
        Ok(())
    }

    fn write_block(&mut self, p: &Packet, sample: Vec<u8>, key: bool, discard_ns: Option<i64>) -> Result<()> {
        let t = p.stream;
        let pts = self.block_ticks(t, p.pts);
        let ms = self.ms(t, pts).max(0);
        let video = self.tracks[t].video;
        let any_video = self.tracks.iter().any(|t| t.video);
        self.end_ms = self.end_ms.max(self.ms(t, pts + p.duration.max(0)).max(ms));
        let new = match &self.cluster {
            None => true,
            Some(c) => {
                let age = ms - c.tc;
                age >= 5000 || !(-30_000..=32_000).contains(&age) || (video && key && age >= 1000)
            }
        };
        if new {
            self.close_cluster()?;
            self.cluster = Some(Cluster { tc: ms, body: Vec::new(), cues: Vec::new() });
        }
        let c = self.cluster.as_mut().expect("a cluster is open");
        // one cue per track in each cluster: the first key frame of each video track, or each audio track's first packet
        if key && (video || !any_video) && !c.cues.iter().any(|&(_, track)| track == t + 1) {
            c.cues.push((ms, t + 1));
        }
        let mut b = vec![0x80 | (t as u8 + 1)];
        b.extend(((ms - c.tc) as i16).to_be_bytes());
        b.push(if key { 0x80 } else { 0 });
        b.extend_from_slice(&sample);
        match discard_ns {
            // the last Opus packet decodes to more than the stream holds: say how much to drop
            Some(ns) => {
                let mut g = el(0xA1, &b);
                g.extend(el(0x75A2, &ns.to_be_bytes()));
                c.body.extend(el(0xA0, &g));
            }
            None => c.body.extend(el(0xA3, &b)),
        }
        Ok(())
    }

    fn put(&mut self, p: &Packet) -> Result<()> {
        let t = p.stream;
        let (sample, key) = if self.tracks[t].video && self.tracks[t].stream.codec == CodecId::Vp8 {
            // bit 0 of the frame tag is clear on key frames
            (p.data.clone(), p.data.first().is_some_and(|b| b & 1 == 0))
        } else if self.tracks[t].video && self.tracks[t].stream.codec == CodecId::Vp9 {
            (p.data.clone(), vp9_is_key(&p.data))
        } else if self.tracks[t].video && self.tracks[t].stream.codec == CodecId::Av1 {
            match av1_sample(&p.data, &mut self.tracks[t].av1_still) {
                Some((sample, _)) if sample.is_empty() => return Ok(()),
                Some(x) => x,
                None => return Err(Error::invalid_input(format!("an AV1 packet at {} does not parse as OBUs", p.pts))),
            }
        } else if self.tracks[t].video && self.tracks[t].stream.codec == CodecId::Mjpeg {
            // every JPEG picture stands alone
            (p.data.clone(), true)
        } else if self.tracks[t].video && self.tracks[t].stream.codec == CodecId::Mpeg4 {
            // an MPEG-4 picture is stored as it is; it is a key picture when its coding type is I
            (p.data.clone(), crate::keyframe::is_keyframe(CodecId::Mpeg4, &p.data) != Some(false))
        } else if self.tracks[t].video {
            let hevc = self.tracks[t].stream.codec == CodecId::Hevc;
            match annexb_sample(hevc, &mut self.tracks[t].params, &p.data) {
                Some(x) => x,
                None => return Ok(()),
            }
        } else {
            (p.data.clone(), true)
        };
        if !self.tracks[t].video && self.tracks[t].stream.codec == CodecId::Flac {
            self.tracks[t].flac.record(p);
        }
        if !self.tracks[t].video && (self.tracks[t].stream.codec == CodecId::Opus || holds_aac_end(&self.tracks[t])) {
            // the track's last packet is held back: it may carry the end padding
            if let Some(prev) = self.tails[t].replace(p.clone()) {
                return self.write_block(&prev, prev.data.clone(), true, None);
            }
            return Ok(());
        }
        self.write_block(p, sample, key, None)
    }

    fn flush_held(&mut self) -> Result<()> {
        for p in std::mem::take(&mut self.held) {
            self.put(&p)?;
        }
        Ok(())
    }
}

impl Muxer for MatroskaWriter {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        if streams.is_empty() {
            return Err(Error::invalid_arg("this Matroska writer needs at least one stream"));
        }
        for s in streams {
            if self.webm && !matches!(s.codec, CodecId::Opus | CodecId::Vorbis | CodecId::Vp8 | CodecId::Vp9 | CodecId::Av1) {
                return Err(Error::new(ErrorCode::UnsupportedCodec, format!("WebM holds VP8, VP9, AV1, Opus and Vorbis, not {}", s.codec.name()))
                    .hint("write .mkv for other codecs")
                    .alternatives(["vp8", "vp9", "av1", "opus", "vorbis"]));
            }
            let video = match (&s.params, s.codec) {
                (StreamParams::Video { .. }, CodecId::Av1) => {
                    if s.extradata.first() != Some(&0x81) || s.extradata.len() < 4 {
                        return Err(Error::new(ErrorCode::UnsupportedFormat, "AV1 needs its av1C configuration record to go into Matroska")
                            .hint("copy it from an MP4, Matroska or WebM file, which carries it"));
                    }
                    true
                }
                (StreamParams::Video { .. }, CodecId::H264 | CodecId::Hevc | CodecId::Vp8 | CodecId::Vp9 | CodecId::Mjpeg | CodecId::Mpeg4) => true,
                (StreamParams::Audio { .. }, CodecId::Aac | CodecId::Opus | CodecId::Vorbis) => {
                    if s.extradata.is_empty() {
                        return Err(Error::new(ErrorCode::UnsupportedFormat, format!("{} needs its codec configuration to go into Matroska", s.codec.name()))
                            .hint("copy it from a file that carries it (MP4, Matroska, Ogg or ADTS)"));
                    }
                    false
                }
                (StreamParams::Audio { .. }, CodecId::Flac | CodecId::Mp3 | CodecId::Mp2) => false,
                (StreamParams::Audio { .. }, c) if c.sample_format().is_some() => false,
                _ => {
                    return Err(Error::new(
                        ErrorCode::UnsupportedCodec,
                        format!("this Matroska writer holds h264, hevc, vp8, vp9, av1, mjpeg, aac, flac, mp3, opus, vorbis and pcm, not {}", s.codec.name()),
                    )
                    .alternatives(["h264", "hevc", "vp8", "vp9", "av1", "mjpeg", "aac", "flac", "mp3", "opus", "vorbis", "pcm_s16le"]));
                }
            };
            if s.time_base.den <= 0 || s.time_base.num <= 0 {
                return Err(Error::invalid_arg("a stream has no time base"));
            }
            // the av1C record's configuration OBUs may hold the sequence header
            let mut av1_still = false;
            if s.codec == CodecId::Av1 {
                let _ = av1_sample(&s.extradata[4..], &mut av1_still);
            }
            self.tracks.push(Track { stream: s.clone(), video, params: ParamSets::default(), flac: FlacStats::new(), seen: 0, first_duration: 0, duration_sum: 0, av1_still });
            self.tails.push(None);
        }
        Ok(())
    }

    fn write_packet(&mut self, p: &Packet) -> Result<()> {
        let t = p.stream;
        let tr = self.tracks.get_mut(t).ok_or_else(|| Error::new(ErrorCode::Internal, format!("packet for unknown output stream {t}")))?;
        if tr.seen == 0 {
            tr.first_duration = p.duration;
        }
        tr.seen += 1;
        // what the packets decode to: an AAC-LC frame is 1024 samples, whatever the source's timeline says
        // (FFmpeg's MP4 timeline can end before its last frame does)
        tr.duration_sum += if tr.stream.codec == CodecId::Aac { 1024 } else { p.duration };
        if self.started {
            return self.put(p);
        }
        // hold the packet, but look for parameter sets now so the header can go out as soon as they are known
        if tr.video && !self_contained(tr.stream.codec) {
            let _ = annexb_sample(tr.stream.codec == CodecId::Hevc, &mut tr.params, &p.data);
        }
        self.held.push(p.clone());
        if self.ready() {
            self.start()?;
            self.flush_held()?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if let Some(i) = self.tracks.iter().position(|t| t.seen == 0) {
            return Err(Error::invalid_input(format!("output stream {i} received no packets, so there is nothing to put in the Matroska file")));
        }
        if !self.started {
            if !self.ready() {
                return Err(Error::invalid_input("the video stream never showed its parameter sets (SPS/PPS), so it cannot be described in Matroska"));
            }
            self.start()?;
            self.flush_held()?;
        }
        for last in self.tails.iter_mut().filter_map(Option::take).collect::<Vec<_>>() {
            let t = last.stream;
            let d = if self.tracks[t].stream.codec == CodecId::Aac {
                // the packets hold more samples than the stream's length and priming leave: the rest is padding
                let s = &self.tracks[t].stream;
                let rate = match s.params {
                    StreamParams::Audio { sample_rate, .. } => sample_rate as i128,
                    _ => 0,
                };
                let declared = s.duration.unwrap_or(self.tracks[t].duration_sum);
                let pad = self.tracks[t].duration_sum as i128 - s.lead_in as i128 - declared as i128;
                (pad > 0 && rate > 0).then(|| (pad * 1_000_000_000 / rate) as i64)
            } else {
                let tb = self.tracks[t].stream.time_base;
                let have = ironav_codec::opus::packet_samples(&last.data).unwrap_or(0) as i128;
                let kept = if last.duration > 0 { last.duration as i128 * 48_000 * tb.num as i128 / tb.den as i128 } else { have };
                let pad = (have - kept).max(0) as i64 * 1_000_000_000 / 48_000;
                (pad > 0).then_some(pad)
            };
            self.write_block(&last, last.data.clone(), true, d)?;
        }
        self.close_cluster()?;

        if !self.cues.is_empty() {
            let mut cues = Vec::new();
            for &(ms, track, at) in &self.cues {
                let mut pos = uint(0xF7, track as u64); // CueTrack
                pos.extend(uint(0xF1, at)); // CueClusterPosition
                let mut point = uint(0xB3, ms as u64); // CueTime
                point.extend(el(0xB7, &pos));
                cues.extend(el(0xBB, &point));
            }
            self.out.write_all(&el(0x1C53_BB6B, &cues))?;
        }
        let end = self.pos()?;
        self.out.seek(SeekFrom::Start(self.segment_size_at))?;
        let mut size = (end - self.segment_data).to_be_bytes();
        size[0] = 0x01; // eight-byte size field
        self.out.write_all(&size)?;
        self.out.seek(SeekFrom::Start(self.duration_at))?;
        self.out.write_all(&(self.end_ms as f64).to_be_bytes())?;
        for &(t, at) in &self.flac_info_at {
            let (rate, channels, bits) = flac_audio_params(&self.tracks[t].stream)?;
            let info = self.tracks[t].flac.streaminfo(rate, channels, bits);
            self.out.seek(SeekFrom::Start(at))?;
            self.out.write_all(&info)?;
        }
        self.out.seek(SeekFrom::Start(end))?;
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::{Demuxer, PixelFormat, Rational};
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    struct Shared(Arc<Mutex<Cursor<Vec<u8>>>>);
    impl std::io::Write for Shared {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for Shared {
        fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
            self.0.lock().unwrap().seek(p)
        }
    }

    #[test]
    fn sizes_and_ids_are_ebml() {
        assert_eq!(size_bytes(0), vec![0x80]);
        assert_eq!(size_bytes(126), vec![0xFE]);
        assert_eq!(size_bytes(127), vec![0x40, 0x7F]); // 0xFF would mean "unknown"
        assert_eq!(id_bytes(0xA3), vec![0xA3]);
        assert_eq!(id_bytes(0x1A45_DFA3), vec![0x1A, 0x45, 0xDF, 0xA3]);
        assert_eq!(uint(0xD7, 0), vec![0xD7, 0x81, 0]);
        assert_eq!(uint(0xD7, 300), vec![0xD7, 0x82, 1, 44]);
    }

    /// Video with parameter sets in its first packet, and audio, come back out of our own reader with
    /// their times, key frames and payloads; the audio starts the file before the video's header is out.
    #[test]
    fn video_and_audio_round_trip_through_the_reader() {
        let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let mut m = MatroskaWriter::new(Box::new(Shared(sink.clone())));
        let video = StreamInfo {
            index: 0,
            codec: CodecId::H264,
            time_base: Rational::new(1, 25),
            duration: None,
            bits: None,
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) },
        };
        let audio = StreamInfo {
            index: 1,
            codec: CodecId::Aac,
            time_base: Rational::new(1, 44100),
            duration: None,
            bits: None,
            extradata: vec![0x12, 0x10],
            lead_in: 0,
            params: StreamParams::Audio { sample_rate: 44100, channels: 2 },
        };
        m.write_header(&[video, audio]).unwrap();
        let sc = [0, 0, 0, 1];
        let idr = [&sc[..], &[0x67, 0x42, 0, 0x1E, 0xAA], &sc, &[0x68, 0xCE], &sc, &[0x65, 0x88, 1, 2, 3]].concat();
        let p = [&sc[..], &[0x41, 0x9A, 4, 5]].concat();
        m.write_packet(&Packet { stream: 1, pts: 0, duration: 1024, data: vec![1; 10] }).unwrap();
        m.write_packet(&Packet { stream: 0, pts: 0, duration: 1, data: idr }).unwrap();
        for i in 1..30 {
            m.write_packet(&Packet { stream: 0, pts: i, duration: 1, data: p.clone() }).unwrap();
        }
        for i in 1..50 {
            m.write_packet(&Packet { stream: 1, pts: i * 1024, duration: 1024, data: vec![i as u8; 10] }).unwrap();
        }
        m.finish().unwrap();
        let bytes = sink.lock().unwrap().get_ref().clone();

        let mut d = crate::mkv::open(Cursor::new(bytes)).unwrap();
        let s = d.streams().to_vec();
        assert_eq!((s[0].codec, s[1].codec), (CodecId::H264, CodecId::Aac));
        assert_eq!(s[1].extradata, vec![0x12, 0x10]);
        let (mut v, mut a) = (Vec::new(), Vec::new());
        while let Some(p) = d.read_packet().unwrap() {
            if p.stream == 0 { v.push(p) } else { a.push(p) }
        }
        assert_eq!((v.len(), a.len()), (30, 50));
        // 25 fps: 40 ms apart, in milliseconds-rounded time
        assert_eq!(v[3].pts as f64 * s[0].time_base.as_f64(), 0.12);
        assert!(v[0].data.windows(5).any(|w| w == [0x65, 0x88, 1, 2, 3]), "the key frame's payload survives");
        assert_eq!(a[7].data, vec![7; 10]);
        let t = a[7].pts as f64 * s[1].time_base.as_f64();
        assert!((t - 7.0 * 1024.0 / 44100.0).abs() < 0.001, "{t}");
    }

    /// Two video tracks and two audio tracks come back out of our reader as four tracks, each with its own packets.
    #[test]
    fn several_tracks_of_each_kind_round_trip_through_the_reader() {
        let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let mut m = MatroskaWriter::new(Box::new(Shared(sink.clone())));
        let video = |index| StreamInfo {
            index,
            codec: CodecId::H264,
            time_base: Rational::new(1, 25),
            duration: None,
            bits: None,
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) },
        };
        let pcm = |index| StreamInfo {
            index,
            codec: CodecId::PcmS16Le,
            time_base: Rational::new(1, 8000),
            duration: None,
            bits: Some(16),
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Audio { sample_rate: 8000, channels: 1 },
        };
        m.write_header(&[video(0), video(1), pcm(2), pcm(3)]).unwrap();
        let sc = [0, 0, 0, 1];
        let idr = [&sc[..], &[0x67, 0x42, 0, 0x1E, 0xAA], &sc, &[0x68, 0xCE], &sc, &[0x65, 0x88, 1, 2, 3]].concat();
        let p = [&sc[..], &[0x41, 0x9A, 4, 5]].concat();
        for track in 0..2 {
            m.write_packet(&Packet { stream: track, pts: 0, duration: 1, data: idr.clone() }).unwrap();
        }
        for i in 1..25 {
            for track in 0..2 {
                m.write_packet(&Packet { stream: track, pts: i, duration: 1, data: p.clone() }).unwrap();
            }
        }
        for i in 0..40i64 {
            for track in 2..4usize {
                m.write_packet(&Packet { stream: track, pts: i * 200, duration: 200, data: vec![(track as i64 * 10 + i) as u8; 400] }).unwrap();
            }
        }
        m.finish().unwrap();
        let bytes = sink.lock().unwrap().get_ref().clone();

        let mut d = crate::mkv::open(Cursor::new(bytes)).unwrap();
        let s = d.streams().to_vec();
        let codecs: Vec<CodecId> = s.iter().map(|x| x.codec).collect();
        assert_eq!(codecs, vec![CodecId::H264, CodecId::H264, CodecId::PcmS16Le, CodecId::PcmS16Le]);
        let mut got: Vec<Vec<Packet>> = vec![Vec::new(); 4];
        while let Some(p) = d.read_packet().unwrap() {
            got[p.stream].push(p);
        }
        assert_eq!(got.iter().map(Vec::len).collect::<Vec<_>>(), vec![25, 25, 40, 40]);
        // the second audio track's payloads are its own
        for (i, pk) in got[3].iter().enumerate() {
            assert_eq!(pk.data, vec![(30 + i) as u8; 400], "audio track 3, packet {i}");
        }
        assert!(got[0][0].data.windows(5).any(|w| w == [0x65, 0x88, 1, 2, 3]), "the first video track keeps its key frame");
    }

    #[test]
    fn video_that_never_shows_parameter_sets_is_refused() {
        let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let mut m = MatroskaWriter::new(Box::new(Shared(sink)));
        m.write_header(&[StreamInfo {
            index: 0,
            codec: CodecId::H264,
            time_base: Rational::new(1, 25),
            duration: None,
            bits: None,
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) },
        }])
        .unwrap();
        m.write_packet(&Packet { stream: 0, pts: 0, duration: 1, data: vec![0, 0, 0, 1, 0x41, 1, 2] }).unwrap();
        assert!(m.finish().unwrap_err().message.contains("parameter sets"));
    }

    /// A video track whose presentation starts `lead_in` pictures in (reordered Annex B) is written with its
    /// first picture at 0; the audio keeps its times.
    #[test]
    fn video_lead_in_comes_off_the_block_times() {
        let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let mut m = MatroskaWriter::new(Box::new(Shared(sink.clone())));
        let video = StreamInfo {
            index: 0,
            codec: CodecId::H264,
            time_base: Rational::new(1, 25),
            duration: None,
            bits: None,
            extradata: Vec::new(),
            lead_in: 2,
            params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) },
        };
        m.write_header(&[video]).unwrap();
        let sc = [0, 0, 0, 1];
        let sps_pps_idr = [&sc[..], &[0x67, 0x42, 0, 0x1E, 0xAA], &sc, &[0x68, 0xCE], &sc, &[0x65, 0x88, 1, 2, 3]].concat();
        let p = [&sc[..], &[0x41, 0x9A, 4, 5]].concat();
        m.write_packet(&Packet { stream: 0, pts: 2, duration: 1, data: sps_pps_idr }).unwrap();
        for i in 1..6 {
            m.write_packet(&Packet { stream: 0, pts: 2 + i, duration: 1, data: p.clone() }).unwrap();
        }
        m.finish().unwrap();
        let bytes = sink.lock().unwrap().get_ref().clone();
        let mut d = crate::mkv::open(Cursor::new(bytes)).unwrap();
        let mut pts = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            pts.push(p.pts);
        }
        assert_eq!(pts, vec![0, 40, 80, 120, 160, 200]);
    }
}
