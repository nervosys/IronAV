//! MP4 / QuickTime (ISO BMFF) reader.
//!
//! The `moov` index (and the `moof` boxes of a fragmented file) is read into memory and turned into a
//! flat list of samples; the media data is never read until a packet is asked for. Packets come out
//! in decode-time order across all tracks, which is the order the engine's input merge expects. The
//! Matroska reader builds the same list of samples and reuses everything below it.
//!
//! H.264 and HEVC samples are length-prefixed in the file; they are rewritten as Annex B here
//! (start codes, with the parameter sets from `avcC` / `hvcC` put in front of every sync
//! sample), which is the form the hardware decoder and the MP4 writer both use. Other codecs
//! are exposed so they can be named in errors, and their samples pass through untouched.

use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Packet, PixelFormat, Rational, Result, StreamInfo, StreamParams};
use std::io::{Read, Seek, SeekFrom};

pub(crate) mod live;

/// Sanity cap on the size of the index we are willing to hold in memory.
const MAX_MOOV: u64 = 1 << 30;

pub(crate) fn corrupt(msg: impl std::fmt::Display) -> Error {
    Error::invalid_input(format!("MP4: {msg}"))
}

/// Whether `magic` (the first 12 bytes of a file) looks like an MP4 or QuickTime file.
pub fn looks_like_mp4(magic: &[u8]) -> bool {
    magic.len() >= 12 && matches!(&magic[4..8], b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide")
}

struct Boxes<'a> {
    data: &'a [u8],
}

impl<'a> Iterator for Boxes<'a> {
    type Item = Result<([u8; 4], &'a [u8])>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.data.is_empty() {
            return None;
        }
        if self.data.len() < 8 {
            self.data = &[];
            return Some(Err(corrupt("truncated box header")));
        }
        let size = u32::from_be_bytes(self.data[..4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = self.data[4..8].try_into().unwrap();
        let (hdr, total) = match size {
            0 => (8, self.data.len() as u64),
            1 if self.data.len() >= 16 => (16, u64::from_be_bytes(self.data[8..16].try_into().unwrap())),
            1 => {
                self.data = &[];
                return Some(Err(corrupt("truncated 64-bit box header")));
            }
            n => (8, n),
        };
        if total < hdr as u64 || total > self.data.len() as u64 {
            self.data = &[];
            return Some(Err(corrupt(format!("box '{}' has a size that runs past its parent", String::from_utf8_lossy(&kind)))));
        }
        let body = &self.data[hdr..total as usize];
        self.data = &self.data[total as usize..];
        Some(Ok((kind, body)))
    }
}

fn boxes(data: &[u8]) -> Boxes<'_> {
    Boxes { data }
}

/// First child box of `kind`, if any.
fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Result<Option<&'a [u8]>> {
    for b in boxes(data) {
        let (k, body) = b?;
        if &k == kind {
            return Ok(Some(body));
        }
    }
    Ok(None)
}

fn need<'a>(data: &'a [u8], kind: &[u8; 4], within: &str) -> Result<&'a [u8]> {
    child(data, kind)?.ok_or_else(|| corrupt(format!("'{}' box missing from '{within}'", String::from_utf8_lossy(kind))))
}

fn be32(d: &[u8], at: usize) -> Result<u32> {
    d.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or_else(|| corrupt("box is shorter than its fields"))
}

fn be16(d: &[u8], at: usize) -> Result<u16> {
    d.get(at..at + 2).map(|b| u16::from_be_bytes(b.try_into().unwrap())).ok_or_else(|| corrupt("box is shorter than its fields"))
}

/// Entries of a table box: after version/flags and an entry count, `entry` bytes each.
fn table(d: &[u8], entry: usize) -> Result<(usize, &[u8])> {
    let n = be32(d, 4)? as usize;
    let rows = d.get(8..).unwrap_or(&[]);
    if n.checked_mul(entry).is_none_or(|need| need > rows.len()) {
        return Err(corrupt("table claims more entries than it holds"));
    }
    Ok((n, rows))
}

#[derive(Debug)]
pub(crate) struct Config {
    /// Annex B parameter sets to put before sync samples (empty for other codecs).
    pub prefix: Vec<u8>,
    pub nal_length: usize,
    /// Bits per sample of the video: 8, or 10 for 4:2:0 high-bit-depth profiles.
    pub bits: u8,
    /// chroma_format_idc of the video: 0 monochrome, 1 4:2:0, 2 4:2:2 (8-bit H.264 only).
    pub chroma: u8,
}

impl Config {
    /// The format the stream is declared as. Monochrome 8-bit is declared gray so the GPU, which cannot
    /// decode it, is not chosen.
    pub(crate) fn pix_fmt(&self) -> PixelFormat {
        let layout = match self.chroma {
            0 if self.bits == 8 => return PixelFormat::Gray8,
            2 => PixelFormat::Yuv422p,
            3 => PixelFormat::Yuv444p,
            _ => PixelFormat::Yuv420p,
        };
        layout.with_depth(self.bits).unwrap_or(layout)
    }
}

pub(crate) struct TrackInfo {
    pub stream: StreamInfo,
    pub config: Config,
    /// Whether samples are length-prefixed NAL units to be rewritten as Annex B (H.264 and HEVC).
    pub convert: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct Sample {
    pub stream: u16,
    pub sync: bool,
    pub offset: u64,
    pub size: u32,
    pub dts: i64,
    pub cts_offset: i32,
    pub duration: u32,
}

pub(crate) fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 { a.abs().max(1) } else { gcd(b, a % b) }
}

pub struct Mp4Demuxer<R: Read + Seek + Send> {
    name: &'static str,
    r: R,
    pos: u64,
    streams: Vec<StreamInfo>,
    tracks: Vec<TrackInfo>,
    samples: Vec<Sample>,
    next: usize,
    discarded: Vec<bool>,
    /// Samples handed out per stream, to know when a stream's first sample is due.
    seen: Vec<u64>,
}

impl<R: Read + Seek + Send> Mp4Demuxer<R> {
    pub fn open(mut r: R) -> Result<Self> {
        let len = r.seek(SeekFrom::End(0))?;
        let mut at = 0u64;
        let mut moov = None;
        let mut moofs: Vec<(u64, u64, u64)> = Vec::new(); // (box start, body start, body length)
        while at + 8 <= len {
            r.seek(SeekFrom::Start(at))?;
            let mut h = [0u8; 16];
            let n = r.read(&mut h)?.min(16);
            if n < 8 {
                break;
            }
            let size = u32::from_be_bytes(h[..4].try_into().unwrap()) as u64;
            let (hdr, total) = match size {
                0 => (8, len - at),
                1 if n >= 16 => (16, u64::from_be_bytes(h[8..16].try_into().unwrap())),
                1 => return Err(corrupt("truncated 64-bit box header")),
                s => (8, s),
            };
            if total < hdr || at + total > len {
                // A file cut short in mdat still has its index only if moov came first.
                if &h[4..8] == b"mdat" {
                    break;
                }
                return Err(corrupt(format!("box '{}' runs past the end of the file", String::from_utf8_lossy(&h[4..8]))));
            }
            if &h[4..8] == b"moov" {
                if total - hdr > MAX_MOOV {
                    return Err(corrupt("the index is implausibly large"));
                }
                r.seek(SeekFrom::Start(at + hdr))?;
                let mut body = vec![0u8; (total - hdr) as usize];
                r.read_exact(&mut body)?;
                moov = Some(body);
            }
            if &h[4..8] == b"moof" {
                if total - hdr > MAX_MOOV {
                    return Err(corrupt("a fragment header is implausibly large"));
                }
                moofs.push((at, at + hdr, total - hdr));
            }
            at += total;
        }
        let moov = moov.ok_or_else(|| {
            Error::new(ErrorCode::UnsupportedFormat, "MP4 has no 'moov' index (a file cut short while recording, or not an MP4)")
        })?;
        let mvex = child(&moov, b"mvex")?;
        if mvex.is_none() && !moofs.is_empty() {
            return Err(corrupt("the file has fragments but its index does not say so (no 'mvex')"));
        }

        let mut tracks = Vec::new();
        let mut ids: Vec<u32> = Vec::new();
        let mut samples = Vec::new();
        for b in boxes(&moov) {
            let (kind, body) = b?;
            if &kind != b"trak" {
                continue;
            }
            if let Some(t) = parse_trak(body, tracks.len(), &mut samples)? {
                let tkhd = need(body, b"tkhd", "trak")?;
                ids.push(if tkhd.first() == Some(&1) { be32(tkhd, 20)? } else { be32(tkhd, 12)? });
                tracks.push(t);
            }
        }
        if let Some(mvex) = mvex {
            let mut defaults: Vec<[u32; 3]> = vec![[0; 3]; ids.len()]; // duration, size, flags from each 'trex'
            for b in boxes(mvex) {
                let (kind, trex) = b?;
                if &kind == b"trex" {
                    if let Some(i) = ids.iter().position(|&id| id == be32(trex, 4).unwrap_or(0)) {
                        defaults[i] = [be32(trex, 12)?, be32(trex, 16)?, be32(trex, 20)?];
                    }
                }
            }
            let mut next_dts = vec![0i64; ids.len()];
            for (start, body_at, body_len) in moofs {
                r.seek(SeekFrom::Start(body_at))?;
                let mut body = vec![0u8; body_len as usize];
                r.read_exact(&mut body)?;
                parse_moof(&body, start, &ids, &defaults, &mut next_dts, &mut samples, usize::MAX)?;
            }
            // The index of a fragmented file is empty, so frame rate and length come from the fragments.
            for (i, t) in tracks.iter_mut().enumerate() {
                let (n, total) = samples.iter().filter(|s| s.stream as usize == i).fold((0i64, 0i64), |(n, d), s| (n + 1, d + s.duration as i64));
                t.stream.duration = Some(total);
                if let StreamParams::Video { frame_rate, .. } = &mut t.stream.params {
                    if total > 0 {
                        let (num, den) = (n * t.stream.time_base.den, total);
                        let g = gcd(num, den);
                        *frame_rate = Rational::new(num / g, den / g);
                    }
                }
            }
        }
        if tracks.is_empty() {
            return Err(Error::new(ErrorCode::UnsupportedCodec, "the MP4 has no audio or video track IronAV recognises"));
        }
        Ok(Self::from_parts(r, "mp4", tracks, samples))
    }

    /// A demuxer over samples some other container reader has found: `samples` index into `tracks`
    /// and `offset` is a position in `r`. Puts them in decode-time order across tracks.
    pub(crate) fn from_parts(r: R, name: &'static str, tracks: Vec<TrackInfo>, mut samples: Vec<Sample>) -> Self {
        // Decode-time order across tracks; stable, so ties keep track order.
        let scale: Vec<f64> = tracks.iter().map(|t| t.stream.time_base.as_f64()).collect();
        samples.sort_by(|a, b| (a.dts as f64 * scale[a.stream as usize]).total_cmp(&(b.dts as f64 * scale[b.stream as usize])));
        let n = tracks.len();
        let streams = tracks.iter().map(|t| t.stream.clone()).collect();
        Self { name, r, pos: u64::MAX, streams, tracks, samples, next: 0, discarded: vec![false; n], seen: vec![0; n] }
    }
}

fn be64(d: &[u8], at: usize) -> Result<u64> {
    d.get(at..at + 8).map(|b| u64::from_be_bytes(b.try_into().unwrap())).ok_or_else(|| corrupt("box is shorter than its fields"))
}

/// Add the samples of one `moof` to `samples`. `defaults` holds each track's `trex` duration, size
/// and flags; `next_dts` is where each track's next sample starts when a fragment has no `tfdt`.
/// At most `limit` samples are added.
fn parse_moof(moof: &[u8], moof_start: u64, ids: &[u32], defaults: &[[u32; 3]], next_dts: &mut [i64], samples: &mut Vec<Sample>, limit: usize) -> Result<()> {
    let first = samples.len();
    // a traf with neither a base offset nor default-base-is-moof continues where the previous
    // traf's data ended (ISO/IEC 14496-12 8.8.7.1); the first starts at the moof
    let mut prev_end = moof_start;
    for b in boxes(moof) {
        let (kind, traf) = b?;
        if &kind != b"traf" {
            continue;
        }
        let tfhd = need(traf, b"tfhd", "traf")?;
        let flags = be32(tfhd, 0)? & 0xFF_FFFF;
        let Some(ti) = ids.iter().position(|&id| id == be32(tfhd, 4).unwrap_or(0)) else { continue };
        let mut at = 8;
        let mut base = if flags & 0x2_0000 != 0 { moof_start } else { prev_end };
        if flags & 1 != 0 {
            base = be64(tfhd, at)?;
            at += 8;
        }
        if flags & 2 != 0 {
            at += 4;
        }
        let mut def = defaults[ti];
        for (bit, slot) in [(0x8, 0), (0x10, 1), (0x20, 2)] {
            if flags & bit != 0 {
                def[slot] = be32(tfhd, at)?;
                at += 4;
            }
        }
        if let Some(tfdt) = child(traf, b"tfdt")? {
            next_dts[ti] = if tfdt.first() == Some(&1) { be64(tfdt, 4)? as i64 } else { be32(tfdt, 4)? as i64 };
        }
        let mut cursor = base;
        for b in boxes(traf) {
            let (kind, trun) = b?;
            if &kind != b"trun" {
                continue;
            }
            let flags = be32(trun, 0)? & 0xFF_FFFF;
            let count = be32(trun, 4)? as usize;
            if count > trun.len() {
                return Err(corrupt("a fragment run claims more samples than it holds"));
            }
            if count > limit.saturating_sub(samples.len() - first) {
                return Err(corrupt("fragments describe more samples than IronAV holds at once"));
            }
            let mut at = 8;
            if flags & 1 != 0 {
                cursor = base.checked_add_signed(be32(trun, at)? as i32 as i64).ok_or_else(|| corrupt("a fragment's data offset is out of range"))?;
                at += 4;
            }
            let first_flags = if flags & 4 != 0 {
                at += 4;
                Some(be32(trun, at - 4)?)
            } else {
                None
            };
            for i in 0..count {
                let mut field = |present: bool, default: u32| -> Result<u32> {
                    if present {
                        at += 4;
                        be32(trun, at - 4)
                    } else {
                        Ok(default)
                    }
                };
                let duration = field(flags & 0x100 != 0, def[0])?;
                let size = field(flags & 0x200 != 0, def[1])?;
                let sample_flags = match (flags & 0x400 != 0, i, first_flags) {
                    (true, _, _) => field(true, 0)?,
                    (false, 0, Some(f)) => f,
                    _ => def[2],
                };
                let cts = field(flags & 0x800 != 0, 0)? as i32; // version 0 offsets are unsigned but never reach 2^31
                samples.push(Sample { stream: ti as u16, sync: sample_flags & 0x1_0000 == 0, offset: cursor, size, dts: next_dts[ti], cts_offset: cts, duration });
                cursor = cursor.checked_add(size as u64).ok_or_else(|| corrupt("a fragment's data runs past the largest offset"))?;
                next_dts[ti] = next_dts[ti].saturating_add(duration as i64);
            }
        }
        prev_end = cursor;
    }
    Ok(())
}

/// The most samples one track's index may list (a fixed-size `stsz` can claim billions in a few
/// bytes); 2^27 is a month of 48 kHz AAC packets or 50 days of 30 fps video.
const MAX_TRACK_SAMPLES: usize = 1 << 27;

fn parse_trak(trak: &[u8], index: usize, samples: &mut Vec<Sample>) -> Result<Option<TrackInfo>> {
    let mdia = need(trak, b"mdia", "trak")?;
    let mdhd = need(mdia, b"mdhd", "mdia")?;
    let timescale = if mdhd.first() == Some(&1) { be32(mdhd, 20)? } else { be32(mdhd, 12)? };
    if timescale == 0 {
        return Err(corrupt("track has a zero time scale"));
    }
    let hdlr = need(mdia, b"hdlr", "mdia")?;
    let handler: [u8; 4] = hdlr.get(8..12).ok_or_else(|| corrupt("hdlr is too short"))?.try_into().unwrap();
    let is_video = match &handler {
        b"vide" => true,
        b"soun" => false,
        _ => return Ok(None), // subtitles, timecode, hints
    };
    let stbl = need(need(mdia, b"minf", "mdia")?, b"stbl", "minf")?;
    let stsd = need(stbl, b"stsd", "stbl")?;
    let Some(entry) = boxes(stsd.get(8..).unwrap_or(&[])).next().transpose()? else {
        return Err(corrupt("stsd has no sample entry"));
    };
    let (fourcc, e) = entry;

    // Sample table, shared by every codec.
    let stts = need(stbl, b"stts", "stbl")?;
    let (n_stts, rows) = table(stts, 8)?;
    let deltas: Vec<(u32, u32)> = (0..n_stts).map(|i| Ok((be32(rows, i * 8)?, be32(rows, i * 8 + 4)?))).collect::<Result<_>>()?;
    let stsz = need(stbl, b"stsz", "stbl")?;
    let (fixed, count) = (be32(stsz, 4)?, be32(stsz, 8)? as usize);
    let size_rows = stsz.get(12..).unwrap_or(&[]);
    if fixed == 0 && count.checked_mul(4).is_none_or(|n| n > size_rows.len()) {
        return Err(corrupt("stsz claims more samples than it holds"));
    }
    if count > MAX_TRACK_SAMPLES {
        return Err(corrupt(format!("a track lists {count} samples, more than IronAV indexes")));
    }
    let size_of = |i: usize| -> u32 { if fixed != 0 { fixed } else { u32::from_be_bytes(size_rows[i * 4..i * 4 + 4].try_into().unwrap()) } };

    let mut offsets: Vec<u64> = Vec::new();
    if let Some(co64) = child(stbl, b"co64")? {
        let (n, rows) = table(co64, 8)?;
        for i in 0..n {
            offsets.push(u64::from_be_bytes(rows[i * 8..i * 8 + 8].try_into().unwrap()));
        }
    } else {
        let stco = need(stbl, b"stco", "stbl")?;
        let (n, rows) = table(stco, 4)?;
        for i in 0..n {
            offsets.push(be32(rows, i * 4)? as u64);
        }
    }
    let stsc = need(stbl, b"stsc", "stbl")?;
    let (n_stsc, rows) = table(stsc, 12)?;
    let runs: Vec<(u32, u32)> = (0..n_stsc).map(|i| Ok((be32(rows, i * 12)?, be32(rows, i * 12 + 4)?))).collect::<Result<_>>()?;
    let sync: Option<Vec<u32>> = match child(stbl, b"stss")? {
        Some(s) => {
            let (n, rows) = table(s, 4)?;
            Some((0..n).map(|i| be32(rows, i * 4)).collect::<Result<_>>()?)
        }
        None => None,
    };
    let ctts: Vec<(u32, i32)> = match child(stbl, b"ctts")? {
        Some(c) => {
            let (n, rows) = table(c, 8)?;
            (0..n).map(|i| Ok((be32(rows, i * 8)?, be32(rows, i * 8 + 4)? as i32))).collect::<Result<_>>()?
        }
        None => Vec::new(),
    };

    // Expand: chunk layout gives offsets, stts gives times, ctts gives composition offsets.
    let first = samples.len();
    let mut dts = 0i64;
    let mut di = deltas.iter().copied();
    let mut cur_delta = di.next();
    let mut ci = ctts.iter().copied();
    let mut cur_cts = ci.next();
    let mut sample_no = 0usize;
    let mut stss_pos = 0usize;
    for (chunk, &chunk_off) in offsets.iter().enumerate() {
        let per_chunk = runs.iter().rev().find(|r| r.0 as usize <= chunk + 1).map_or(0, |r| r.1);
        let mut off = chunk_off;
        for _ in 0..per_chunk {
            if sample_no >= count {
                break;
            }
            let size = size_of(sample_no);
            let dur = loop {
                match cur_delta {
                    Some((0, _)) => cur_delta = di.next(),
                    Some((n, d)) => {
                        cur_delta = Some((n - 1, d));
                        break d;
                    }
                    None => return Err(corrupt("stts covers fewer samples than stsz")),
                }
            };
            let cts_offset = loop {
                match cur_cts {
                    Some((0, _)) => cur_cts = ci.next(),
                    Some((n, o)) => {
                        cur_cts = Some((n - 1, o));
                        break o;
                    }
                    None => break 0,
                }
            };
            let is_sync = match &sync {
                None => true,
                Some(list) => {
                    while stss_pos < list.len() && (list[stss_pos] as usize) < sample_no + 1 {
                        stss_pos += 1;
                    }
                    list.get(stss_pos).is_some_and(|&s| s as usize == sample_no + 1)
                }
            };
            samples.push(Sample { stream: index as u16, sync: is_sync, offset: off, size, dts, cts_offset, duration: dur });
            off += size as u64;
            dts += dur as i64;
            sample_no += 1;
        }
    }
    if sample_no != count {
        return Err(corrupt("the chunk table covers fewer samples than stsz lists"));
    }
    let n = (samples.len() - first) as i64;

    let time_base = Rational::new(1, timescale as i64);
    let mut extradata = Vec::new();
    let (codec, params, bits, config, convert) = if is_video {
        let codec = match &fourcc {
            b"avc1" | b"avc3" => CodecId::H264,
            b"hvc1" | b"hev1" => CodecId::Hevc,
            b"av01" => CodecId::Av1,
            b"vp09" => CodecId::Vp9,
            b"mp4v" => CodecId::Mpeg4,
            b"jpeg" | b"mjpa" => CodecId::Mjpeg,
            _ => return Ok(None),
        };
        let (width, height) = (be16(e, 24)? as u32, be16(e, 26)? as u32);
        let kids = e.get(78..).unwrap_or(&[]);
        let config = match codec {
            CodecId::H264 => parse_avcc(need(kids, b"avcC", "the avc1 entry")?)?,
            CodecId::Hevc => parse_hvcc(need(kids, b"hvcC", "the hvc1 entry")?)?,
            CodecId::Av1 => {
                let c = need(kids, b"av1C", "the av01 entry")?;
                extradata = c.to_vec();
                av1c_config(c)?
            }
            CodecId::Vp9 => vpcc_config(child(kids, b"vpcC")?),
            _ => Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 },
        };
        let total: i64 = samples[first..].iter().map(|s| s.duration as i64).sum();
        let frame_rate = if total > 0 {
            let (num, den) = (n * timescale as i64, total);
            let g = gcd(num, den);
            Rational::new(num / g, den / g)
        } else {
            Rational::new(0, 1)
        };
        let pix_fmt = config.pix_fmt();
        let params = StreamParams::Video { width, height, pix_fmt, frame_rate };
        (codec, params, None, config, matches!(codec, CodecId::H264 | CodecId::Hevc))
    } else {
        let codec = match &fourcc {
            b"fLaC" => CodecId::Flac,
            b"mp4a" => CodecId::Aac,
            b"Opus" => CodecId::Opus,
            b"alac" => CodecId::Alac,
            b".mp3" => CodecId::Mp3,
            _ => return Ok(None),
        };
        // QuickTime sound entries of version 1 and 2 carry extra fields before the child boxes.
        let version = be16(e, 8)?;
        let kids_at = match version {
            1 => 28 + 16,
            2 => 28 + 36,
            _ => 28,
        };
        let mut channels = be16(e, 16)?;
        let mut rate = be32(e, 24)? >> 16;
        let mut bits = Some(be16(e, 18)? as u8).filter(|&b| b > 0);
        if codec == CodecId::Flac {
            let dfla = need(e.get(kids_at..).unwrap_or(&[]), b"dfLa", "the fLaC entry")?;
            let si = dfla.get(4..).filter(|b| b.len() >= 4 + 34 && b[0] & 0x7F == 0).ok_or_else(|| corrupt("dfLa has no STREAMINFO block"))?;
            let packed = u64::from_be_bytes(si[4 + 10..4 + 18].try_into().unwrap());
            rate = (packed >> 44) as u32;
            channels = ((packed >> 41) & 7) as u16 + 1;
            bits = Some(((packed >> 36) & 31) as u8 + 1);
        }
        if codec == CodecId::Aac {
            extradata = aac_config(need(e.get(kids_at..).unwrap_or(&[]), b"esds", "the mp4a entry")?)?;
        }
        if codec == CodecId::Alac {
            // the box holds a version and flags, then the 24-byte ALACSpecificConfig, which has the true rate, channels and depth
            let body = need(e.get(kids_at..).unwrap_or(&[]), b"alac", "the alac entry")?;
            let cfg = match body.len() {
                28 => &body[4..],
                24 => body,
                _ => return Err(corrupt("alac is not a 24-byte ALACSpecificConfig")),
            };
            extradata = cfg.to_vec();
            bits = Some(cfg[5]);
            channels = cfg[9] as u16;
            rate = u32::from_be_bytes([cfg[20], cfg[21], cfg[22], cfg[23]]);
        }
        let params = StreamParams::Audio { sample_rate: rate, channels };
        (codec, params, bits, Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 }, false)
    };
    let duration: i64 = samples[first..].iter().map(|s| s.duration as i64).sum();
    let lead_in = edit_lead_in(trak)?;
    let stream = StreamInfo { index, codec, time_base, duration: Some(duration), bits, extradata, lead_in, params };
    Ok(Some(TrackInfo { stream, config, convert }))
}

/// Media time at which the track's first non-empty edit starts, from `edts/elst`; 0 without one.
fn edit_lead_in(trak: &[u8]) -> Result<i64> {
    let Some(edts) = child(trak, b"edts")? else { return Ok(0) };
    let Some(elst) = child(edts, b"elst")? else { return Ok(0) };
    let wide = elst.first() == Some(&1);
    let width = if wide { 20 } else { 12 };
    let n = be32(elst, 4)? as usize;
    for i in 0..n {
        let entry = elst.get(8 + i * width..8 + (i + 1) * width).ok_or_else(|| corrupt("elst is cut short"))?;
        let media_time = if wide { i64::from_be_bytes(entry[8..16].try_into().unwrap()) } else { i32::from_be_bytes(entry[4..8].try_into().unwrap()) as i64 };
        if media_time >= 0 {
            return Ok(media_time);
        }
    }
    Ok(0)
}

/// The AudioSpecificConfig inside an `esds` box: ES descriptor (0x03) > decoder config (0x04,
/// object type 0x40 = MPEG-4 AAC) > decoder specific info (0x05).
fn aac_config(esds: &[u8]) -> Result<Vec<u8>> {
    fn descriptor(d: &[u8], at: &mut usize) -> Result<(u8, usize)> {
        let tag = *d.get(*at).ok_or_else(|| corrupt("esds is cut short"))?;
        *at += 1;
        let mut len = 0usize;
        for _ in 0..4 {
            let b = *d.get(*at).ok_or_else(|| corrupt("esds is cut short"))?;
            *at += 1;
            len = len << 7 | (b & 0x7F) as usize;
            if b & 0x80 == 0 {
                break;
            }
        }
        Ok((tag, len))
    }
    let d = esds.get(4..).ok_or_else(|| corrupt("esds is cut short"))?; // version and flags
    let mut at = 0;
    let (tag, _) = descriptor(d, &mut at)?;
    if tag != 3 {
        return Err(corrupt("esds does not start with an ES descriptor"));
    }
    let flags = *d.get(at + 2).ok_or_else(|| corrupt("esds is cut short"))?;
    at += 3; // ES_ID and flags
    if flags & 0x80 != 0 {
        at += 2;
    }
    if flags & 0x40 != 0 {
        at += 1 + *d.get(at).ok_or_else(|| corrupt("esds is cut short"))? as usize;
    }
    if flags & 0x20 != 0 {
        at += 2;
    }
    let (tag, _) = descriptor(d, &mut at)?;
    if tag != 4 {
        return Err(corrupt("esds has no decoder configuration"));
    }
    if d.get(at) != Some(&0x40) {
        return Err(Error::new(ErrorCode::UnsupportedCodec, "the mp4a track is not MPEG-4 AAC (object type is not 0x40)"));
    }
    at += 13; // object type, stream type, buffer size, max and average bit rate
    let (tag, len) = descriptor(d, &mut at)?;
    if tag != 5 {
        return Err(corrupt("esds has no AudioSpecificConfig"));
    }
    d.get(at..at + len).map(<[u8]>::to_vec).ok_or_else(|| corrupt("AudioSpecificConfig runs past the esds box"))
}

fn push_start_coded(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

/// Bit depth and chroma layout from a VP9 `vpcC` box (version 1); 8-bit 4:2:0 without one.
fn vpcc_config(c: Option<&[u8]>) -> Config {
    let (bits, chroma) = match c {
        Some(c) if c.len() >= 7 && c[0] == 1 => (c[6] >> 4, match c[6] >> 1 & 7 { 2 => 2, 3 => 3, _ => 1 }),
        _ => (8, 1),
    };
    Config { prefix: Vec::new(), nal_length: 0, bits, chroma }
}

/// Bit depth and chroma layout from an AV1CodecConfigurationRecord (`av1C`, also Matroska's
/// CodecPrivate for AV1). Twelve-bit streams are reported as 12 bits, which no decoder here takes.
pub(crate) fn av1c_config(c: &[u8]) -> Result<Config> {
    if c.len() < 4 || c[0] != 0x81 {
        return Err(corrupt("av1C is malformed"));
    }
    let bits = if c[2] & 0x40 == 0 { 8 } else if c[2] & 0x20 == 0 { 10 } else { 12 };
    let chroma = match (c[2] & 0x10 != 0, c[2] & 0x08 != 0, c[2] & 0x04 != 0) {
        (true, _, _) => 0,
        (false, true, true) => 1,
        (false, true, false) => 2,
        _ => 3,
    };
    Ok(Config { prefix: Vec::new(), nal_length: 0, bits, chroma })
}

pub(crate) fn parse_avcc(c: &[u8]) -> Result<Config> {
    if c.len() < 7 || c[0] != 1 {
        return Err(corrupt("avcC is malformed"));
    }
    let mut at = 6;
    let mut prefix = Vec::new();
    let n_sps = (c[5] & 0x1F) as usize;
    for _ in 0..n_sps {
        let len = be16(c, at)? as usize;
        push_start_coded(&mut prefix, c.get(at + 2..at + 2 + len).ok_or_else(|| corrupt("avcC SPS runs past the box"))?);
        at += 2 + len;
    }
    let n_pps = *c.get(at).ok_or_else(|| corrupt("avcC has no PPS count"))? as usize;
    at += 1;
    for _ in 0..n_pps {
        let len = be16(c, at)? as usize;
        push_start_coded(&mut prefix, c.get(at + 2..at + 2 + len).ok_or_else(|| corrupt("avcC PPS runs past the box"))?);
        at += 2 + len;
    }
    // High profiles may state chroma format and bit depth after the parameter sets.
    let (mut bits, mut chroma) = (8, 1);
    if matches!(c[1], 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134) {
        if let Some(ext) = c.get(at..at + 3) {
            let (cf, luma, chroma_bits) = (ext[0] & 3, ext[1] & 7, ext[2] & 7);
            bits = supported_depth(cf, luma, chroma_bits, 10)?;
            chroma = cf;
        } else if matches!(c[1], 110 | 122 | 244) {
            return Err(Error::new(ErrorCode::UnsupportedCodec, "H.264 high-bit-depth profile without a stated bit depth; only 8-bit and 10-bit 4:2:0 are supported"));
        }
    }
    Ok(Config { prefix, nal_length: (c[4] & 3) as usize + 1, bits, chroma })
}

pub(crate) fn parse_hvcc(c: &[u8]) -> Result<Config> {
    if c.len() < 23 || c[0] != 1 {
        return Err(corrupt("hvcC is malformed"));
    }
    let (chroma, luma, chroma_bits) = (c[16] & 3, c[17] & 7, c[18] & 7);
    // the HEVC decoder takes monochrome as well as 4:2:0, and 12 bits
    let bits = supported_depth(chroma, luma, chroma_bits, 12)?;
    let mut prefix = Vec::new();
    let mut at = 23;
    for _ in 0..c[22] {
        let n = be16(c, at + 1)? as usize;
        at += 3;
        for _ in 0..n {
            let len = be16(c, at)? as usize;
            push_start_coded(&mut prefix, c.get(at + 2..at + 2 + len).ok_or_else(|| corrupt("hvcC parameter set runs past the box"))?);
            at += 2 + len;
        }
    }
    Ok(Config { prefix, nal_length: (c[21] & 3) as usize + 1, bits, chroma })
}

/// 8-bit, 10-bit or (up to `max`) 12-bit video (monochrome, 4:2:0, 4:2:2 or 4:4:4) with the same
/// depth for luma and chroma; anything else is refused.
fn supported_depth(chroma: u8, luma_minus8: u8, chroma_minus8: u8, max: u8) -> Result<u8> {
    match (luma_minus8, chroma_minus8) {
        (0, 0) => Ok(8),
        (2, 2) => Ok(10),
        (4, 4) if max >= 12 => Ok(12),
        _ => Err(unsupported_format(chroma, luma_minus8 + 8, max)),
    }
}

fn unsupported_format(chroma: u8, bits: u8, max: u8) -> Error {
    let chroma = ["4:0:0", "4:2:0", "4:2:2", "4:4:4"][chroma as usize & 3];
    let depths = if max >= 12 { "8-bit, 10-bit and 12-bit" } else { "8-bit and 10-bit" };
    Error::new(ErrorCode::UnsupportedCodec, format!("the video is {bits}-bit {chroma}; only {depths} video with the same depth for luma and chroma is supported"))
        .hint("convert the source to one of those depths first")
}

impl<R: Read + Seek + Send> Demuxer for Mp4Demuxer<R> {
    fn format_name(&self) -> &'static str {
        self.name
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn discard(&mut self, stream: usize) {
        if let Some(d) = self.discarded.get_mut(stream) {
            *d = true;
        }
    }

    /// Lands on the last video keyframe at or before `seconds` (or the packet containing it when
    /// there is no video), so the decoder starts on a picture it can decode alone.
    fn seek(&mut self, seconds: f64) -> Result<f64> {
        let t = seconds.max(0.0);
        let at = |s: &Sample| s.dts as f64 * self.tracks[s.stream as usize].stream.time_base.as_f64();
        let video = |s: &Sample| {
            matches!(self.tracks[s.stream as usize].stream.params, StreamParams::Video { .. }) && !self.discarded[s.stream as usize]
        };
        let landing = if self.samples.iter().any(&video) {
            self.samples.iter().rfind(|s| video(s) && s.sync && at(s) <= t + 1e-9).map_or(0.0, at)
        } else {
            // No video: start on the packet that contains the requested time.
            self.samples.iter().rfind(|s| !self.discarded[s.stream as usize] && at(s) <= t + 1e-9).map_or(t, at)
        };
        self.next = self.samples.partition_point(|s| at(s) < landing - 1e-9);
        self.seen.fill(0); // the first video packet after a seek needs its parameter sets again
        self.pos = u64::MAX;
        Ok(landing)
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        while let Some(&s) = self.samples.get(self.next) {
            self.next += 1;
            let st = s.stream as usize;
            if self.discarded[st] {
                continue;
            }
            if self.pos != s.offset {
                self.r.seek(SeekFrom::Start(s.offset))?;
            }
            let mut raw = vec![0u8; s.size as usize];
            self.r.read_exact(&mut raw).map_err(|_| corrupt("a sample lies beyond the end of the file (truncated file?)"))?;
            self.pos = s.offset + s.size as u64;

            let t = &self.tracks[st];
            let data = if t.convert {
                let n = t.config.nal_length;
                let mut out = Vec::with_capacity(raw.len() + t.config.prefix.len() + 16);
                if s.sync || self.seen[st] == 0 {
                    out.extend_from_slice(&t.config.prefix);
                }
                let mut at = 0;
                while at < raw.len() {
                    let len = raw.get(at..at + n).ok_or_else(|| corrupt("video sample ends inside a NAL length"))?.iter().fold(0usize, |a, &b| (a << 8) | b as usize);
                    at += n;
                    push_start_coded(&mut out, raw.get(at..at + len).ok_or_else(|| corrupt("video sample ends inside a NAL unit"))?);
                    at += len;
                }
                out
            } else {
                raw
            };
            self.seen[st] += 1;
            return Ok(Some(Packet { stream: st, pts: s.dts.saturating_add(s.cts_offset as i64), duration: s.duration as i64, data }));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av1c_gives_bit_depth_and_chroma_layout() {
        let fmt = |b2: u8| av1c_config(&[0x81, 0x08, b2, 0x00]).unwrap().pix_fmt();
        assert_eq!(fmt(0x0C), PixelFormat::Yuv420p);
        assert_eq!(fmt(0x4C), PixelFormat::Yuv420p10);
        assert_eq!(fmt(0x1C), PixelFormat::Gray8);
        assert_eq!(fmt(0x08), PixelFormat::Yuv422p);
        assert_eq!(fmt(0x40), PixelFormat::Yuv444p10);
        assert_eq!(av1c_config(&[0x81, 0x08, 0x6C, 0x00]).unwrap().bits, 12);
        assert!(av1c_config(&[0x01, 0, 0, 0]).is_err());
        assert!(av1c_config(&[0x81, 0]).is_err());
    }

    #[test]
    fn recognises_mp4_by_its_first_box() {
        assert!(looks_like_mp4(b"\0\0\0\x20ftypisom...."));
        assert!(!looks_like_mp4(b"RIFF....WAVE"));
        assert!(!looks_like_mp4(b"short"));
    }

    #[test]
    fn box_sizes_that_overrun_the_parent_are_errors() {
        let bad = [0, 0, 0, 40, b'm', b'o', b'o', b'v', 1, 2];
        assert!(boxes(&bad).next().unwrap().is_err());
    }

    #[test]
    fn avcc_yields_annex_b_parameter_sets_and_checks_bit_depth() {
        // version, profile 100, compat, level 40, 4-byte lengths, 1 SPS, 1 PPS, then the extension.
        let mut c = vec![1, 100, 0, 40, 0xFF, 0xE1, 0, 3, 0x67, 1, 2, 1, 0, 2, 0x68, 9];
        c.extend_from_slice(&[0xFD, 0xF8, 0xF8, 0]);
        let cfg = parse_avcc(&c).unwrap();
        assert_eq!(cfg.nal_length, 4);
        assert_eq!(cfg.prefix, vec![0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 9]);
        let n = c.len();
        c[n - 3] = 0xFA; // luma 10-bit but chroma 8-bit: mixed depths are refused
        assert_eq!(parse_avcc(&c).unwrap_err().code, ErrorCode::UnsupportedCodec);
        c[n - 2] = 0xFA; // both 10-bit
        assert_eq!(parse_avcc(&c).unwrap().bits, 10);
        c[n - 4] = 0xFE; // 4:2:2 at 10 bits is accepted
        assert_eq!(parse_avcc(&c).unwrap().chroma, 2);
        c[n - 3] = 0xF8; // 4:2:2 at 8 bits is accepted
        c[n - 2] = 0xF8;
        assert_eq!(parse_avcc(&c).unwrap().chroma, 2);
        c[n - 4] = 0xFC; // monochrome
        assert_eq!(parse_avcc(&c).unwrap().chroma, 0);
        c[n - 4] = 0xFF; // 4:4:4 at 8 bits is accepted
        assert_eq!(parse_avcc(&c).unwrap().chroma, 3);
        c[n - 3] = 0xFA; // and at 10 bits
        c[n - 2] = 0xFA;
        assert_eq!(parse_avcc(&c).unwrap().bits, 10);
        c[n - 3] = 0xFC; // but mixed depths are still refused
        assert_eq!(parse_avcc(&c).unwrap_err().code, ErrorCode::UnsupportedCodec);
    }

    use crate::mp4::Mp4Muxer;
    use ironav_core::Muxer;
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
    fn copied_aac_keeps_its_config_and_encoder_delay() {
        let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let mut m = Mp4Muxer::new(Box::new(Shared(sink.clone())));
        let asc = vec![0x12, 0x10]; // AAC-LC, 44.1 kHz, stereo
        m.write_header(&[StreamInfo {
            index: 0,
            codec: CodecId::Aac,
            time_base: Rational::new(1, 44100),
            duration: None,
            bits: None,
            extradata: asc.clone(),
            lead_in: 1024,
            params: StreamParams::Audio { sample_rate: 44100, channels: 2 },
        }])
        .unwrap();
        for i in 0..4 {
            m.write_packet(&Packet { stream: 0, pts: i * 1024, duration: 1024, data: vec![i as u8; 10] }).unwrap();
        }
        m.finish().unwrap();
        let file = sink.lock().unwrap().get_ref().clone();
        let mut d = Mp4Demuxer::open(Cursor::new(file)).unwrap();
        let s = &d.streams()[0];
        assert_eq!((s.codec, s.extradata.clone(), s.lead_in), (CodecId::Aac, asc, 1024));
        assert_eq!(d.read_packet().unwrap().unwrap().data, vec![0u8; 10]);
    }

    /// Write a small video+audio MP4 with our own muxer and read it back.
    fn round_trip() -> (Vec<Packet>, Mp4Demuxer<Cursor<Vec<u8>>>) {
        let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let mut m = Mp4Muxer::new(Box::new(Shared(sink.clone())));
        let streams = [
            StreamInfo {
                index: 0,
                codec: CodecId::H264,
                time_base: Rational::new(1, 30),
                duration: None,
                bits: None,
                extradata: Vec::new(),
                lead_in: 0,
                params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(30000, 1001) },
            },
            StreamInfo {
                index: 1,
                codec: CodecId::Flac,
                time_base: Rational::new(1, 48000),
                duration: None,
                bits: Some(16),
                extradata: Vec::new(),
                lead_in: 0,
                params: StreamParams::Audio { sample_rate: 48000, channels: 2 },
            },
        ];
        m.write_header(&streams).unwrap();
        let idr = [&[0u8, 0, 0, 1, 0x67, 100, 0, 40, 0xAC, 0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0, 0, 0, 1, 0x65][..], &[7; 9]].concat();
        let p = [&[0u8, 0, 0, 1, 0x41][..], &[5; 4]].concat();
        let sent = vec![
            Packet { stream: 0, pts: 0, duration: 1, data: idr },
            Packet { stream: 1, pts: 0, duration: 4096, data: vec![0xFF, 0xF8, 1, 2, 3] },
            Packet { stream: 0, pts: 1, duration: 1, data: p.clone() },
            Packet { stream: 0, pts: 2, duration: 1, data: p },
            Packet { stream: 1, pts: 4096, duration: 4096, data: vec![0xFF, 0xF8, 9, 9] },
        ];
        for pk in &sent {
            m.write_packet(pk).unwrap();
        }
        m.finish().unwrap();
        let bytes = sink.lock().unwrap().get_ref().clone();
        (sent, Mp4Demuxer::open(Cursor::new(bytes)).unwrap())
    }

    #[test]
    fn reads_back_what_the_writer_wrote() {
        let (sent, mut d) = round_trip();
        let s = d.streams().to_vec();
        assert_eq!((s[0].codec, s[1].codec), (CodecId::H264, CodecId::Flac));
        assert!(matches!(s[0].params, StreamParams::Video { width: 64, height: 48, frame_rate, .. } if frame_rate == Rational::new(30000, 1001)));
        assert!(matches!(s[1].params, StreamParams::Audio { sample_rate: 48000, channels: 2 }));
        assert_eq!((s[1].bits, s[1].duration), (Some(16), Some(8192)));

        let mut got = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            got.push(p);
        }
        assert_eq!(got.len(), 5);
        // Video comes back as Annex B: parameter sets in front of the keyframe only.
        let video: Vec<&Packet> = got.iter().filter(|p| p.stream == 0).collect();
        assert_eq!(video[0].data, [&[0u8, 0, 0, 1, 0x67, 100, 0, 40, 0xAC, 0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0, 0, 0, 1, 0x65][..], &[7; 9]].concat());
        assert_eq!(video[1].data, sent[2].data);
        // The track's time base is 1/30000, so a 30000/1001 fps frame lasts 1001 ticks.
        assert_eq!(video.iter().map(|p| (p.pts, p.duration)).collect::<Vec<_>>(), vec![(0, 1001), (1001, 1001), (2002, 1001)]);
        // Audio frames are untouched, with their times.
        let audio: Vec<&Packet> = got.iter().filter(|p| p.stream == 1).collect();
        assert_eq!((audio[0].data.clone(), audio[0].pts, audio[0].duration), (sent[1].data.clone(), 0, 4096));
        assert_eq!((audio[1].data.clone(), audio[1].pts), (sent[4].data.clone(), 4096));
        // Across tracks the order is by decode time.
        let times: Vec<f64> = got.iter().map(|p| p.pts as f64 * s[p.stream].time_base.as_f64()).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1] + 1e-9), "{times:?}");
    }

    #[test]
    fn discarded_streams_are_not_read() {
        let (_, mut d) = round_trip();
        d.discard(1);
        let mut n = 0;
        while let Some(p) = d.read_packet().unwrap() {
            assert_eq!(p.stream, 0);
            n += 1;
        }
        assert_eq!(n, 3);
    }

    #[test]
    fn a_file_with_no_index_is_refused() {
        let junk = [&[0u8, 0, 0, 16, b'f', b't', b'y', b'p'][..], b"isom    ", &[0, 0, 0, 8, b'f', b'r', b'e', b'e']].concat();
        let e = Mp4Demuxer::open(Cursor::new(junk)).err().unwrap();
        assert_eq!(e.code, ErrorCode::UnsupportedFormat);
    }

    #[test]
    fn seeking_lands_on_a_keyframe_and_replays_from_there() {
        let (_, mut d) = round_trip();
        // Only the first video frame is a keyframe, so any seek lands on it and restarts cleanly.
        let landed = d.seek(0.07).unwrap();
        assert_eq!(landed, 0.0);
        let first = d.read_packet().unwrap().unwrap();
        assert_eq!(first.stream, 0);
        assert!(first.data.starts_with(&[0, 0, 0, 1, 0x67]), "parameter sets are repeated after a seek");
        // With video discarded the audio is positioned exactly.
        let (_, mut a) = round_trip();
        a.discard(0);
        let landed = a.seek(0.09).unwrap();
        assert!((landed - 4096.0 / 48000.0).abs() < 1e-9, "audio frame 2 starts at {landed}");
        assert_eq!(a.read_packet().unwrap().unwrap().pts, 4096);
    }
}
