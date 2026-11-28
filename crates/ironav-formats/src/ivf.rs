//! IVF (`.ivf`): the 32-byte `DKIF` header (FourCC, picture size, time base, frame count), then
//! each frame as a 12-byte header (size, 64-bit presentation time) and its bytes. It holds VP8
//! (`VP80`), VP9 (`VP90`) and AV1 (`AV01`, temporal units in the low-overhead OBU format).
//!
//! The file is walked once to build a table of frames, with a key frame found from each frame's
//! bitstream (VP8: the frame tag's bit; VP9: the frame type in the uncompressed header; AV1: a
//! sequence header and a shown key frame in the temporal unit); only the first bytes of a frame are
//! read to tell. The table goes to the MP4 reader's machinery (`Mp4Demuxer::from_parts`), which
//! reads, seeks (to the key frame at or before the time) and discards. The time base is the header's;
//! the frame rate is measured from the frame times, and the header's frame count is not trusted.
//! For AV1 the stream's extradata is an `av1C` record built from the first sequence header, so the
//! stream can be copied into MP4 and Matroska.
//!
//! Not handled: other FourCCs (H.264 and HEVC in IVF are refused by name); frames over 256 MiB are
//! not read; a frame cut short at the end of the file is dropped.

use crate::mp4demux::{Config, Mp4Demuxer, Sample, TrackInfo};
use crate::vpxav1::{vp9_is_key, vp9_key};
use ironav_codec::av1::obu::{parse_sequence_header, split_obus};
use ironav_core::{CodecId, Error, ErrorCode, Rational, Result, StreamInfo, StreamParams};
use std::io::{BufReader, Read, Seek};

/// The most frames indexed: over a hundred hours at 30 frames a second.
const MAX_FRAMES: usize = 1 << 24;

/// The largest frame read.
const MAX_FRAME: u64 = 256 << 20;

/// How much of a frame is read to tell whether it is a key frame.
const PEEK: usize = 4096;

/// Whether `magic` starts an IVF file.
pub fn looks_like_ivf(magic: &[u8]) -> bool {
    magic.len() >= 32 && &magic[..4] == b"DKIF"
}

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::invalid_input(format!("IVF: {msg}"))
}

/// Whether the AV1 temporal unit whose first bytes are `buf` (the whole unit when `whole`) holds a
/// sequence header and then a shown key frame. `None` when `buf` ends inside an OBU that is not the
/// frame header's, so more of the unit is needed.
fn av1_key(buf: &[u8], whole: bool) -> Option<bool> {
    let mut at = 0usize;
    let mut seq = false;
    for _ in 0..64 {
        let Some(&h) = buf.get(at) else { return if whole { Some(false) } else { None } };
        if h & 0x80 != 0 {
            return Some(false);
        }
        let mut p = at + 1 + (h >> 2 & 1) as usize;
        let len = if h & 2 != 0 {
            let mut size = 0u64;
            for i in 0..8 {
                let Some(&b) = buf.get(p) else { return if whole { Some(false) } else { None } };
                p += 1;
                size |= u64::from(b & 0x7F) << (7 * i);
                if b & 0x80 == 0 {
                    break;
                }
            }
            size as usize
        } else {
            usize::MAX
        };
        match h >> 3 & 15 {
            1 => seq = true,
            3 | 6 => {
                // the frame header's first bits: show_existing_frame, frame_type, show_frame (or none, in a reduced still header)
                let Some(&f) = buf.get(p) else { return if whole { Some(false) } else { None } };
                return Some(seq && f >> 7 == 0 && f >> 5 & 3 == 0 && f >> 4 & 1 == 1);
            }
            _ => {}
        }
        let next = p.checked_add(len).filter(|&n| n <= buf.len());
        match next {
            Some(n) => at = n,
            None if whole => return Some(false),
            None => return None,
        }
    }
    Some(false)
}

/// The level of a sequence header's first operating point, when it can be read without parsing the
/// timing info (0 when it cannot).
fn av1_level(seq: &[u8]) -> u8 {
    let bits = |from: usize, n: usize| -> u32 { (from..from + n).fold(0, |v, i| v << 1 | seq.get(i / 8).map_or(0, |b| (b >> (7 - i % 8) & 1) as u32)) };
    if bits(4, 1) == 1 {
        bits(5, 5) as u8 // reduced still picture header
    } else if bits(5, 1) == 0 {
        // timing_info_present_flag 0, initial_display_delay_present_flag, 5 bits of operating point count, a 12-bit idc
        bits(24, 5) as u8
    } else {
        0
    }
}

/// An `av1C` record for the sequence header OBU payload `seq`.
fn av1c(seq: &[u8], s: &ironav_codec::av1::obu::SequenceHeader) -> Vec<u8> {
    let (sx, sy) = (s.subsampling_x as u8, s.subsampling_y as u8);
    let mut v = vec![
        0x81,
        (s.profile as u8) << 5 | av1_level(seq),
        ((s.bit_depth > 8) as u8) << 6 | ((s.bit_depth == 12) as u8) << 5 | (s.mono_chrome as u8) << 4 | sx << 3 | sy << 2,
        0,
        0x0A, // a sequence header OBU with a size field
    ];
    let mut n = seq.len();
    loop {
        let more = n >= 0x80;
        v.push((n & 0x7F) as u8 | if more { 0x80 } else { 0 });
        n >>= 7;
        if !more {
            break;
        }
    }
    v.extend_from_slice(seq);
    v
}

/// Opens an IVF file for reading.
pub fn open<R: Read + Seek + Send>(mut r: R) -> Result<Mp4Demuxer<R>> {
    let total = r.seek(std::io::SeekFrom::End(0))?;
    r.seek(std::io::SeekFrom::Start(0))?;
    let mut head = [0u8; 32];
    r.read_exact(&mut head).map_err(|_| bad("the file is shorter than its header"))?;
    if !looks_like_ivf(&head) {
        return Err(bad("not an IVF file"));
    }
    let header_len = u16::from_le_bytes([head[6], head[7]]) as u64;
    let codec = match &head[8..12] {
        b"VP80" => CodecId::Vp8,
        b"VP90" => CodecId::Vp9,
        b"AV01" => CodecId::Av1,
        other => {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("IVF: the '{}' codec is not supported", crate::aiff::fourcc_text(other))).hint("IVF files with VP8, VP9 or AV1 (VP80, VP90, AV01) are read"))
        }
    };
    let (width, height) = (u16::from_le_bytes([head[12], head[13]]) as u32, u16::from_le_bytes([head[14], head[15]]) as u32);
    if width == 0 || height == 0 {
        return Err(bad("the header gives no picture size"));
    }
    let (rate, scale) = (u32::from_le_bytes(head[16..20].try_into().unwrap()), u32::from_le_bytes(head[20..24].try_into().unwrap()));
    let (num, den) = if rate == 0 || scale == 0 { (1, 1000) } else { (scale as i64, rate as i64) };
    let g = crate::mp4demux::gcd(num, den);
    let time_base = Rational::new(num / g, den / g);

    let mut samples: Vec<Sample> = Vec::new();
    let mut pts = Vec::new();
    // the first key frame's offset and size, and its first bytes, for the stream's configuration
    let mut first_key: Option<(u64, u64, Vec<u8>)> = None;
    let mut pos = header_len.max(32);
    {
        let mut b = BufReader::with_capacity(1 << 16, &mut r);
        b.seek(std::io::SeekFrom::Start(pos))?;
        while pos + 12 <= total && samples.len() < MAX_FRAMES {
            let mut h = [0u8; 12];
            if b.read_exact(&mut h).is_err() {
                break;
            }
            let size = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as u64;
            let Ok(t) = i64::try_from(u64::from_le_bytes(h[4..12].try_into().unwrap())) else { break };
            let body = pos + 12;
            if size > MAX_FRAME || body + size > total {
                break; // the last frame is cut short, or the size is not one
            }
            let mut peek = vec![0u8; (size as usize).min(PEEK)];
            b.read_exact(&mut peek)?;
            let mut read = peek.len() as u64;
            let key = match codec {
                CodecId::Vp8 => peek.first().is_some_and(|f| f & 1 == 0),
                CodecId::Vp9 => vp9_is_key(&peek),
                _ => match av1_key(&peek, size as usize <= PEEK) {
                    Some(k) => k,
                    None => {
                        // an OBU before the frame is longer than the peek: read the whole unit
                        let mut whole = peek.clone();
                        whole.resize(size as usize, 0);
                        b.read_exact(&mut whole[peek.len()..])?;
                        read = size;
                        peek = whole;
                        av1_key(&peek, true).unwrap_or(false)
                    }
                },
            };
            if key && first_key.is_none() {
                first_key = Some((body, size, peek.clone()));
            }
            b.seek_relative((size - read) as i64)?;
            pos = body + size;
            if size > 0 {
                samples.push(Sample { stream: 0, sync: key, offset: body, size: size as u32, dts: t, cts_offset: 0, duration: 0 });
                pts.push(t);
            }
        }
    }
    if samples.is_empty() {
        return Err(bad("the file holds no frames"));
    }
    // a frame lasts until the next one; the last lasts as long as the one before it
    let n = samples.len();
    for i in 0..n {
        let d = if i + 1 < n { pts[i + 1] - pts[i] } else if i > 0 { pts[i] - pts[i - 1] } else { 1 };
        samples[i].duration = u32::try_from(d.max(0)).unwrap_or(u32::MAX);
    }
    let span: i64 = samples.iter().map(|s| s.duration as i64).sum();
    let frame_rate = if span > 0 {
        let (fn_, fd) = (n as i64 * time_base.den, span * time_base.num);
        let g = crate::mp4demux::gcd(fn_, fd);
        Rational::new(fn_ / g, fd / g)
    } else {
        Rational::new(0, 1)
    };
    let (mut bits, mut chroma, mut extradata) = (8u8, 1u8, Vec::new());
    match (codec, &first_key) {
        (CodecId::Vp9, Some((_, _, head))) => {
            if let Some(k) = vp9_key(head) {
                bits = k.bits;
                chroma = match (k.ssx, k.ssy) {
                    (true, true) => 1,
                    (true, false) => 2,
                    _ => 3,
                };
            }
        }
        (CodecId::Av1, Some((offset, size, _))) => {
            let mut unit = vec![0u8; *size as usize];
            r.seek(std::io::SeekFrom::Start(*offset))?;
            r.read_exact(&mut unit)?;
            let seq = split_obus(&unit).ok().and_then(|obus| obus.into_iter().find(|o| o.kind == 1).map(|o| o.payload.to_vec()));
            if let Some((payload, s)) = seq.and_then(|p| parse_sequence_header(&p).ok().map(|s| (p, s))) {
                bits = s.bit_depth as u8;
                chroma = if s.mono_chrome { 0 } else { match (s.subsampling_x, s.subsampling_y) { (true, true) => 1, (true, false) => 2, _ => 3 } };
                extradata = av1c(&payload, &s);
            }
        }
        _ => {}
    }
    let config = Config { prefix: Vec::new(), nal_length: 0, bits, chroma };
    let params = StreamParams::Video { width, height, pix_fmt: config.pix_fmt(), frame_rate };
    let stream = StreamInfo { index: 0, codec, time_base, duration: Some(span), bits: None, extradata, lead_in: 0, params };
    let track = TrackInfo { stream, config, convert: false, swap: 0, unwrap: Vec::new(), meta: Default::default() };
    Ok(Mp4Demuxer::from_parts(r, "ivf", vec![track], samples))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::Demuxer;
    use std::io::Cursor;

    fn ivf(fourcc: &[u8; 4], frames: &[(u64, Vec<u8>)]) -> Vec<u8> {
        let mut f = b"DKIF".to_vec();
        f.extend([0, 0, 32, 0]);
        f.extend(fourcc);
        f.extend(64u16.to_le_bytes());
        f.extend(48u16.to_le_bytes());
        f.extend(30u32.to_le_bytes());
        f.extend(1u32.to_le_bytes());
        f.extend((frames.len() as u32).to_le_bytes());
        f.extend([0; 4]);
        for (pts, d) in frames {
            f.extend((d.len() as u32).to_le_bytes());
            f.extend(pts.to_le_bytes());
            f.extend(d);
        }
        f
    }

    fn vp8(key: bool) -> Vec<u8> {
        vec![if key { 0x10 } else { 0x11 }, 0x02, 0x00, 0x9D, 0x01, 0x2A, 0, 0]
    }

    #[test]
    fn vp8_frames_and_key_frames() {
        let frames: Vec<_> = (0..10u64).map(|i| (i, vp8(i % 4 == 0))).collect();
        let mut d = open(Cursor::new(ivf(b"VP80", &frames))).unwrap();
        let s = &d.streams()[0];
        assert_eq!((s.codec, s.time_base, s.duration), (CodecId::Vp8, Rational::new(1, 30), Some(10)));
        match s.params {
            StreamParams::Video { width, height, frame_rate, .. } => assert_eq!((width, height, frame_rate), (64, 48, Rational::new(30, 1))),
            _ => panic!(),
        }
        assert_eq!(d.seek(6.0 / 30.0).unwrap(), 4.0 / 30.0, "lands on the key frame before");
        let p = d.read_packet().unwrap().unwrap();
        assert_eq!(p.pts, 4);
        assert_eq!(d.seek(0.0).unwrap(), 0.0);
        assert_eq!(d.read_packet().unwrap().unwrap().data, vp8(true));
    }

    #[test]
    fn av1_key_frames_are_found_from_the_obus() {
        // temporal delimiter, sequence header (profile 0, level 0), then a frame OBU
        let td = [0x12, 0x00];
        let seq = [0x0A, 0x0A, 0, 0, 0, 0x04, 0x45, 0x7E, 0x3E, 0xFF, 0xFC, 0xC0];
        let key = [&td[..], &seq, &[0x32, 2, 0x10, 0x00]].concat(); // frame: show_existing 0, key, shown
        let inter = [&td[..], &[0x32, 2, 0x30, 0x00]].concat(); // frame_type 1
        assert_eq!(av1_key(&key, true), Some(true));
        assert_eq!(av1_key(&inter, true), Some(false));
        assert_eq!(av1_key(&key[..key.len() - 4], false), None, "cut inside the frame OBU's header");
        assert_eq!(av1_key(&[0x80, 0], true), Some(false));
    }

    #[test]
    fn damaged_files_are_errors_not_panics() {
        let frames: Vec<_> = (0..4u64).map(|i| (i, vp8(i == 0))).collect();
        let good = ivf(b"VP80", &frames);
        for n in 0..good.len() {
            if let Ok(mut d) = open(Cursor::new(good[..n].to_vec())) {
                while d.read_packet().ok().flatten().is_some() {}
            }
        }
        let mut huge = good.clone();
        huge[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(open(Cursor::new(huge)).is_err(), "a frame larger than the file leaves nothing");
        let mut zero = good.clone();
        zero[12..16].fill(0);
        assert!(open(Cursor::new(zero)).is_err());
        let mut other = good;
        other[8..12].copy_from_slice(b"H264");
        let e = open(Cursor::new(other)).err().unwrap();
        assert!(e.message.contains("H264"), "{}", e.message);
    }
}
