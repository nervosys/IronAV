//! MP4 / QuickTime (ISO BMFF) reader.
//!
//! The `moov` index is read into memory and turned into a flat list of samples; the media data
//! is never read until a packet is asked for. Packets come out in decode-time order across all
//! tracks, which is the order the engine's input merge expects. Fragmented files (`moof`) are
//! not supported.
//!
//! H.264 and HEVC samples are length-prefixed in the file; they are rewritten as Annex B here
//! (start codes, with the parameter sets from `avcC` / `hvcC` put in front of every sync
//! sample), which is the form the hardware decoder and the MP4 writer both use. Other codecs
//! are exposed so they can be named in errors, and their samples pass through untouched.

use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Packet, PixelFormat, Rational, Result, StreamInfo, StreamParams};
use std::io::{Read, Seek, SeekFrom};

/// Sanity cap on the size of the index we are willing to hold in memory.
const MAX_MOOV: u64 = 1 << 30;

fn corrupt(msg: impl std::fmt::Display) -> Error {
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
struct Config {
    /// Annex B parameter sets to put before sync samples (empty for other codecs).
    prefix: Vec<u8>,
    nal_length: usize,
}

struct TrackInfo {
    stream: StreamInfo,
    config: Config,
    convert: bool,
}

#[derive(Clone, Copy)]
struct Sample {
    stream: u16,
    sync: bool,
    offset: u64,
    size: u32,
    dts: i64,
    cts_offset: i32,
    duration: u32,
}

fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 { a.abs().max(1) } else { gcd(b, a % b) }
}

pub struct Mp4Demuxer<R: Read + Seek + Send> {
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
            at += total;
        }
        let moov = moov.ok_or_else(|| {
            Error::new(ErrorCode::UnsupportedFormat, "MP4 has no 'moov' index (a file cut short while recording, or not an MP4)")
        })?;
        if child(&moov, b"mvex")?.is_some() {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "fragmented MP4 (with moof boxes) is not supported yet").hint("remux it to a regular MP4 first"));
        }

        let mut tracks = Vec::new();
        let mut samples = Vec::new();
        for b in boxes(&moov) {
            let (kind, body) = b?;
            if &kind != b"trak" {
                continue;
            }
            if let Some(t) = parse_trak(body, tracks.len(), &mut samples)? {
                tracks.push(t);
            }
        }
        if tracks.is_empty() {
            return Err(Error::new(ErrorCode::UnsupportedCodec, "the MP4 has no audio or video track IronAV recognises"));
        }
        // Decode-time order across tracks; stable, so ties keep track order.
        let scale: Vec<f64> = tracks.iter().map(|t| t.stream.time_base.as_f64()).collect();
        samples.sort_by(|a, b| (a.dts as f64 * scale[a.stream as usize]).total_cmp(&(b.dts as f64 * scale[b.stream as usize])));
        let n = tracks.len();
        let streams = tracks.iter().map(|t| t.stream.clone()).collect();
        Ok(Self { r, pos: u64::MAX, streams, tracks, samples, next: 0, discarded: vec![false; n], seen: vec![0; n] })
    }
}

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
            _ => Config { prefix: Vec::new(), nal_length: 0 },
        };
        let total: i64 = samples[first..].iter().map(|s| s.duration as i64).sum();
        let frame_rate = if total > 0 {
            let (num, den) = (n * timescale as i64, total);
            let g = gcd(num, den);
            Rational::new(num / g, den / g)
        } else {
            Rational::new(0, 1)
        };
        let params = StreamParams::Video { width, height, pix_fmt: PixelFormat::Yuv420p, frame_rate };
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
        let params = StreamParams::Audio { sample_rate: rate, channels };
        (codec, params, bits, Config { prefix: Vec::new(), nal_length: 0 }, false)
    };
    let duration: i64 = samples[first..].iter().map(|s| s.duration as i64).sum();
    let stream = StreamInfo { index, codec, time_base, duration: Some(duration), bits, params };
    Ok(Some(TrackInfo { stream, config, convert }))
}

fn push_start_coded(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

fn parse_avcc(c: &[u8]) -> Result<Config> {
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
    if matches!(c[1], 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134) {
        if let Some(ext) = c.get(at..at + 3) {
            let (chroma, luma, chroma_bits) = (ext[0] & 3, ext[1] & 7, ext[2] & 7);
            if chroma != 1 || luma != 0 || chroma_bits != 0 {
                return Err(unsupported_format(chroma, luma + 8));
            }
        } else if matches!(c[1], 110 | 122 | 244) {
            return Err(Error::new(ErrorCode::UnsupportedCodec, "H.264 high-bit-depth profile without a stated bit depth; only 8-bit 4:2:0 is supported"));
        }
    }
    Ok(Config { prefix, nal_length: (c[4] & 3) as usize + 1 })
}

fn parse_hvcc(c: &[u8]) -> Result<Config> {
    if c.len() < 23 || c[0] != 1 {
        return Err(corrupt("hvcC is malformed"));
    }
    let (chroma, luma, chroma_bits) = (c[16] & 3, c[17] & 7, c[18] & 7);
    if chroma != 1 || luma != 0 || chroma_bits != 0 {
        return Err(unsupported_format(chroma, luma + 8));
    }
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
    Ok(Config { prefix, nal_length: (c[21] & 3) as usize + 1 })
}

fn unsupported_format(chroma: u8, bits: u8) -> Error {
    let chroma = ["4:0:0", "4:2:0", "4:2:2", "4:4:4"][chroma as usize & 3];
    Error::new(ErrorCode::UnsupportedCodec, format!("the video is {bits}-bit {chroma}; only 8-bit 4:2:0 is supported"))
        .hint("convert the source to 8-bit yuv420p first")
}

impl<R: Read + Seek + Send> Demuxer for Mp4Demuxer<R> {
    fn format_name(&self) -> &'static str {
        "mp4"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn discard(&mut self, stream: usize) {
        if let Some(d) = self.discarded.get_mut(stream) {
            *d = true;
        }
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
            return Ok(Some(Packet { stream: st, pts: s.dts + s.cts_offset as i64, duration: s.duration as i64, data }));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn avcc_yields_annex_b_parameter_sets_and_rejects_10_bit() {
        // version, profile 100, compat, level 40, 4-byte lengths, 1 SPS, 1 PPS, then the extension.
        let mut c = vec![1, 100, 0, 40, 0xFF, 0xE1, 0, 3, 0x67, 1, 2, 1, 0, 2, 0x68, 9];
        c.extend_from_slice(&[0xFD, 0xF8, 0xF8, 0]);
        let cfg = parse_avcc(&c).unwrap();
        assert_eq!(cfg.nal_length, 4);
        assert_eq!(cfg.prefix, vec![0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 9]);
        let n = c.len();
        c[n - 3] = 0xFA; // luma bit depth 10
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
                params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(30000, 1001) },
            },
            StreamInfo {
                index: 1,
                codec: CodecId::Flac,
                time_base: Rational::new(1, 48000),
                duration: None,
                bits: Some(16),
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
}
