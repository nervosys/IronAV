//! Core Audio Format (`.caf`): a header, then chunks with 64-bit sizes: `desc` (the format), `kuki`
//! (the codec's magic cookie), `pakt` (the packet table), `data`.
//!
//! Read: linear PCM (`lpcm`: 8, 16, 24 and 32-bit integer and 32 and 64-bit float, either byte
//! order; read as `RawPcmDemuxer` reads AIFF), ALAC (`alac`, its cookie in either the old `frma`
//! form or the bare 24-byte one) and AAC (`aac `, whose cookie is an ES descriptor). Compressed
//! audio is found through the packet table (variable packet sizes and frame counts, the priming
//! and remainder frames), and goes to the MP4 reader's machinery (`Mp4Demuxer::from_parts`), which
//! reads and seeks by packet. For AAC the priming frames become the stream's `lead_in` and the
//! valid frame count its duration, so playback is gapless.
//!
//! Not handled: other codecs (IMA4, u-law, A-law, FLAC, Opus, MP3 and so on are refused by name),
//! the channel layout (`chan`), lpcm with non-byte-aligned samples, and edit counts.

use crate::aiff::{fourcc_text, PcmLayout, RawPcmDemuxer};
use crate::mp4demux::{aac_config, Config, Mp4Demuxer, Sample, TrackInfo};
use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Rational, Result, StreamInfo, StreamParams};
use std::io::{Read, Seek, SeekFrom};

/// The chunks looked at before giving up: a real file has a handful.
const MAX_CHUNKS: usize = 4096;

/// The largest cookie read.
const MAX_COOKIE: u64 = 1 << 20;

/// The most packets read: more than a hundred hours of AAC.
const MAX_PACKETS: usize = 1 << 24;

/// The largest packet table read.
const MAX_PAKT: u64 = 1 << 29;

/// Whether `magic` starts a CAF file.
pub fn looks_like_caf(magic: &[u8]) -> bool {
    magic.len() >= 8 && &magic[..4] == b"caff" && magic[4] == 0 && magic[5] == 1
}

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::invalid_input(format!("CAF: {msg}"))
}

fn unsupported(msg: String) -> Error {
    Error::new(ErrorCode::UnsupportedCodec, msg).hint("CAF files with linear PCM, ALAC or AAC are read")
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn be64(b: &[u8], at: usize) -> i64 {
    i64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

/// The audio description chunk.
struct Desc {
    rate: u32,
    format: [u8; 4],
    flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    channels: u32,
    bits: u32,
}

/// A variable-length integer of the packet table: seven bits a byte, most significant first, the
/// high bit set on all but the last byte.
fn vlq(d: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for _ in 0..9 {
        let b = *d.get(*at)?;
        *at += 1;
        v = v << 7 | (b & 0x7F) as u64;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// The ALACSpecificConfig (24 bytes) of an ALAC magic cookie: `frma` and `alac` atoms in the old
/// form, the bare configuration in the new.
fn alac_config(cookie: &[u8]) -> Option<&[u8]> {
    if cookie.len() >= 12 && &cookie[4..12] == b"frmaalac" {
        let mut at = 0;
        while at + 8 <= cookie.len() {
            let size = be32(cookie, at) as usize;
            if &cookie[at + 4..at + 8] == b"alac" && size >= 36 {
                return cookie.get(at + 12..at + 36);
            }
            at += size.max(8);
        }
        return None;
    }
    cookie.get(..24)
}

/// Opens a CAF file.
pub fn open<R: Read + Seek + Send + 'static>(mut r: R) -> Result<Box<dyn Demuxer>> {
    let total = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let mut head = [0u8; 8];
    r.read_exact(&mut head).map_err(|_| bad("the file is shorter than its header"))?;
    if !looks_like_caf(&head) {
        return Err(bad("not a CAF file, or not version 1"));
    }
    let mut pos = 8u64;
    let mut desc: Option<Desc> = None;
    let mut cookie: Option<Vec<u8>> = None;
    let mut pakt: Option<Vec<u8>> = None;
    let mut data: Option<(u64, u64)> = None;
    for _ in 0..MAX_CHUNKS {
        if pos + 12 > total {
            break;
        }
        r.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 12];
        r.read_exact(&mut h)?;
        let kind = [h[0], h[1], h[2], h[3]];
        let size = be64(&h, 4);
        let body = pos + 12;
        let avail = total - body;
        // only `data` may say "to the end of the file" (-1), and it may run past it
        let size = if size < 0 || size as u64 > avail {
            if &kind == b"data" {
                avail
            } else {
                return Err(bad(format!("the '{}' chunk's size is not valid", fourcc_text(&kind))));
            }
        } else {
            size as u64
        };
        match &kind {
            b"desc" => {
                if size < 32 {
                    return Err(bad("the desc chunk is too short"));
                }
                let mut b = [0u8; 32];
                r.read_exact(&mut b)?;
                let rate = f64::from_be_bytes(b[..8].try_into().unwrap());
                if !(1.0..=16_000_000.0).contains(&rate) {
                    return Err(bad("the sample rate is not a usable number"));
                }
                desc = Some(Desc {
                    rate: rate.round() as u32,
                    format: [b[8], b[9], b[10], b[11]],
                    flags: be32(&b, 12),
                    bytes_per_packet: be32(&b, 16),
                    frames_per_packet: be32(&b, 20),
                    channels: be32(&b, 24),
                    bits: be32(&b, 28),
                });
            }
            b"kuki" => {
                if size > MAX_COOKIE {
                    return Err(bad("the magic cookie is implausibly large"));
                }
                let mut b = vec![0u8; size as usize];
                r.read_exact(&mut b)?;
                cookie = Some(b);
            }
            b"pakt" => {
                if size > MAX_PAKT {
                    return Err(bad("the packet table is implausibly large"));
                }
                let mut b = vec![0u8; size as usize];
                r.read_exact(&mut b)?;
                pakt = Some(b);
            }
            b"data" => {
                if size < 4 {
                    return Err(bad("the data chunk is too short"));
                }
                data = Some((body + 4, size - 4)); // after the edit count
                if size == avail {
                    break;
                }
            }
            _ => {}
        }
        pos = body + size;
    }
    let d = desc.ok_or_else(|| bad("the file has no desc chunk"))?;
    let (start, len) = data.ok_or_else(|| bad("the file has no data chunk"))?;
    if d.channels == 0 || d.channels > 255 {
        return Err(bad("the channel count is zero or implausible"));
    }
    match &d.format {
        b"lpcm" => {
            let float = d.flags & 1 != 0;
            let codec = match (float, d.bits) {
                (true, 32) => CodecId::PcmF32Le,
                (true, 64) => CodecId::PcmF64Le,
                (false, 8) => CodecId::PcmU8,
                (false, 16) => CodecId::PcmS16Le,
                (false, 24) => CodecId::PcmS24Le,
                (false, 32) => CodecId::PcmS32Le,
                _ => return Err(unsupported(format!("CAF: {}-bit {} linear PCM is not supported", d.bits, if float { "float" } else { "integer" }))),
            };
            let width = d.bits as usize / 8;
            if d.bytes_per_packet as usize != width * d.channels as usize {
                return Err(unsupported("CAF: linear PCM whose samples are not packed to whole bytes is not supported".into()));
            }
            let layout = PcmLayout { codec, rate: d.rate, channels: d.channels as u16, width, big: d.flags & 2 == 0, signed8: true };
            Ok(Box::new(RawPcmDemuxer::new("caf", r, start, len, &layout)?))
        }
        b"alac" | b"aac " => compressed(r, d, cookie, pakt, start, len),
        other => Err(unsupported(format!("CAF: the '{}' format is not supported", fourcc_text(other)))),
    }
}

/// ALAC and AAC: packets found through the packet table (or the description, when packets are of one size).
fn compressed<R: Read + Seek + Send + 'static>(r: R, d: Desc, cookie: Option<Vec<u8>>, pakt: Option<Vec<u8>>, start: u64, len: u64) -> Result<Box<dyn Demuxer>> {
    let alac = &d.format == b"alac";
    let cookie = cookie.ok_or_else(|| bad("the file has no magic cookie (kuki chunk)"))?;
    let mut samples = Vec::new();
    let (mut priming, mut valid) = (0i64, None);
    let mut off = start;
    let end = start + len;
    let mut dts = 0i64;
    let mut push = |size: u64, frames: u64, off: &mut u64, dts: &mut i64| -> bool {
        let Ok(size32) = u32::try_from(size) else { return false };
        if size == 0 || *off + size > end || frames > u32::MAX as u64 || samples.len() >= MAX_PACKETS {
            return false;
        }
        samples.push(Sample { stream: 0, sync: true, offset: *off, size: size32, dts: *dts, cts_offset: 0, duration: frames as u32 });
        *off += size;
        *dts += frames as i64;
        true
    };
    match &pakt {
        Some(p) => {
            if p.len() < 24 {
                return Err(bad("the packet table is too short"));
            }
            let (n, v) = (be64(p, 0), be64(p, 8));
            priming = be32(p, 16) as i32 as i64;
            valid = Some(v);
            if n < 0 {
                return Err(bad("the packet table has a negative packet count"));
            }
            let mut at = 24;
            for _ in 0..n {
                let size = if d.bytes_per_packet == 0 { vlq(p, &mut at) } else { Some(d.bytes_per_packet as u64) };
                let frames = if d.frames_per_packet == 0 { vlq(p, &mut at) } else { Some(d.frames_per_packet as u64) };
                let (Some(size), Some(frames)) = (size, frames) else { break };
                if !push(size, frames, &mut off, &mut dts) {
                    break;
                }
            }
        }
        None => {
            if d.bytes_per_packet == 0 || d.frames_per_packet == 0 {
                return Err(bad("packets vary in size but the file has no packet table"));
            }
            while push(d.bytes_per_packet as u64, d.frames_per_packet as u64, &mut off, &mut dts) {}
        }
    }
    if samples.is_empty() {
        return Err(bad("the file holds no packets"));
    }
    let (mut channels, mut rate) = (d.channels as u16, d.rate);
    let time_base = Rational::new(1, rate as i64);
    let mut bits = None;
    let (codec, extradata) = if alac {
        let cfg = alac_config(&cookie).ok_or_else(|| bad("the ALAC magic cookie is malformed"))?;
        bits = Some(cfg[5]);
        channels = cfg[9] as u16;
        rate = u32::from_be_bytes([cfg[20], cfg[21], cfg[22], cfg[23]]);
        if channels == 0 || rate == 0 {
            return Err(bad("the ALAC magic cookie has no channels or no sample rate"));
        }
        (CodecId::Alac, cfg.to_vec())
    } else {
        // the cookie is an ES descriptor; mp4's esds box has a version and flags in front of it
        let esds = if cookie.first() == Some(&3) { [&[0u8; 4][..], &cookie].concat() } else { cookie.clone() };
        let asc = aac_config(&esds)?;
        // HE-AAC v2 is a mono core with parametric stereo; SBR doubles the core's rate
        if ironav_codec::aac::parametric_stereo(&asc) {
            channels = 2;
        }
        if let Some((out_rate, _)) = ironav_codec::aac::output_params(&asc) {
            if out_rate == rate.saturating_mul(2) {
                rate = out_rate;
            }
        }
        (CodecId::Aac, asc)
    };
    let mut total: i64 = samples.iter().map(|s| s.duration as i64).sum();
    // the last packet holds the remainder frames, which are not audio: it lasts only to the end of the valid ones
    if let (Some(v), Some(last)) = (valid.filter(|&v| v > 0 && priming >= 0), samples.last_mut()) {
        let before = total - last.duration as i64;
        let end = priming + v;
        if end > before && end < total {
            last.duration = (end - before) as u32;
            total = end;
        }
    }
    // AAC's priming frames are trimmed by the decoder, so its duration is what is kept; ALAC's are not
    let (lead_in, duration) = match valid {
        Some(v) if !alac && v > 0 && priming >= 0 && priming + v <= total => (priming, v),
        _ => (0, total),
    };
    let stream = StreamInfo { index: 0, codec, time_base, duration: Some(duration), bits, extradata, lead_in, params: StreamParams::Audio { sample_rate: rate, channels } };
    let track = TrackInfo { stream, config: Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 }, convert: false, swap: 0, unwrap: Vec::new(), meta: Default::default() };
    Ok(Box::new(Mp4Demuxer::from_parts(r, "caf", vec![track], samples)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = kind.to_vec();
        v.extend((body.len() as i64).to_be_bytes());
        v.extend(body);
        v
    }

    fn desc(rate: f64, format: &[u8; 4], flags: u32, bpp: u32, fpp: u32, channels: u32, bits: u32) -> Vec<u8> {
        let mut b = rate.to_be_bytes().to_vec();
        b.extend(format);
        for v in [flags, bpp, fpp, channels, bits] {
            b.extend(v.to_be_bytes());
        }
        chunk(b"desc", &b)
    }

    fn file(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut f = b"caff\0\x01\0\0".to_vec();
        for c in chunks {
            f.extend(c);
        }
        f
    }

    fn data(payload: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 4];
        b.extend(payload);
        chunk(b"data", &b)
    }

    fn vlq_bytes(mut v: u64) -> Vec<u8> {
        let mut out = vec![(v & 0x7F) as u8];
        v >>= 7;
        while v > 0 {
            out.push((v & 0x7F) as u8 | 0x80);
            v >>= 7;
        }
        out.reverse();
        out
    }

    #[test]
    fn linear_pcm_in_both_byte_orders() {
        let be = file(&[desc(8000.0, b"lpcm", 0, 2, 1, 1, 16), data(&[1, 2, 3, 4])]);
        let mut d = open(Cursor::new(be)).unwrap();
        assert_eq!((d.streams()[0].codec, d.streams()[0].duration), (CodecId::PcmS16Le, Some(2)));
        assert_eq!(d.read_packet().unwrap().unwrap().data, [2, 1, 4, 3]);
        let le = file(&[desc(8000.0, b"lpcm", 2, 2, 1, 1, 16), data(&[1, 2, 3, 4])]);
        assert_eq!(open(Cursor::new(le)).unwrap().read_packet().unwrap().unwrap().data, [1, 2, 3, 4]);
        let fl = file(&[desc(8000.0, b"lpcm", 1, 4, 1, 1, 32), data(&0.5f32.to_be_bytes())]);
        let mut d = open(Cursor::new(fl)).unwrap();
        assert_eq!(d.streams()[0].codec, CodecId::PcmF32Le);
        assert_eq!(d.read_packet().unwrap().unwrap().data, 0.5f32.to_le_bytes());
    }

    #[test]
    fn a_packet_table_gives_variable_packets_and_seeks() {
        // ES descriptor for AAC-LC 44.1 kHz stereo: ASC 0x12 0x10
        let esds = [3u8, 25, 0, 0, 0, 4, 17, 0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 2, 0x12, 0x10, 6, 1, 2];
        let sizes = [10u64, 300, 7, 20];
        let mut pakt = Vec::new();
        pakt.extend(4i64.to_be_bytes());
        pakt.extend((4 * 1024 - 100i64).to_be_bytes());
        pakt.extend(0i32.to_be_bytes());
        pakt.extend(100i32.to_be_bytes());
        for s in sizes {
            pakt.extend(vlq_bytes(s));
        }
        let payload: Vec<u8> = (0..sizes.iter().sum::<u64>()).map(|i| i as u8).collect();
        let f = file(&[desc(44100.0, b"aac ", 0, 0, 1024, 2, 0), chunk(b"kuki", &esds), chunk(b"pakt", &pakt), data(&payload)]);
        let mut d = open(Cursor::new(f)).unwrap();
        let s = &d.streams()[0];
        assert_eq!((s.codec, s.extradata.clone(), s.duration), (CodecId::Aac, vec![0x12, 0x10], Some(4 * 1024 - 100)));
        let landed = d.seek(2.5 * 1024.0 / 44100.0).unwrap();
        assert_eq!(landed, 2.0 * 1024.0 / 44100.0);
        let p = d.read_packet().unwrap().unwrap();
        assert_eq!((p.pts, p.data.len()), (2048, 7));
        assert_eq!(p.data[0], (10 + 300) as u8);
        assert_eq!(vlq(&[0x82, 0x2C], &mut 0), Some(300));
    }

    #[test]
    fn damaged_files_are_errors_not_panics() {
        let esds = [3u8, 25, 0, 0, 0, 4, 17, 0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 2, 0x12, 0x10, 6, 1, 2];
        let mut pakt = Vec::new();
        pakt.extend(2i64.to_be_bytes());
        pakt.extend(2048i64.to_be_bytes());
        pakt.extend([0u8; 8]);
        pakt.extend([5, 5]);
        let good = file(&[desc(44100.0, b"aac ", 0, 0, 1024, 2, 0), chunk(b"kuki", &esds), chunk(b"pakt", &pakt), data(&[0; 10])]);
        for n in 0..good.len() {
            if let Ok(mut d) = open(Cursor::new(good[..n].to_vec())) {
                while d.read_packet().ok().flatten().is_some() {}
            }
        }
        // a huge packet count with a short table, a huge chunk size, a negative size, zero channels, a bad rate
        let mut bad_pakt = pakt.clone();
        bad_pakt[..8].copy_from_slice(&i64::MAX.to_be_bytes());
        let f = file(&[desc(44100.0, b"aac ", 0, 0, 1024, 2, 0), chunk(b"kuki", &esds), chunk(b"pakt", &bad_pakt), data(&[0; 10])]);
        let _ = open(Cursor::new(f)).map(|mut d| while d.read_packet().ok().flatten().is_some() {});
        let mut f = good.clone();
        f[8 + 4..8 + 12].copy_from_slice(&i64::MAX.to_be_bytes());
        assert!(open(Cursor::new(f)).is_err());
        let mut f = good.clone();
        f[8 + 4..8 + 12].copy_from_slice(&(-5i64).to_be_bytes());
        assert!(open(Cursor::new(f)).is_err());
        assert!(open(Cursor::new(file(&[desc(44100.0, b"lpcm", 2, 2, 1, 0, 16), data(&[0; 8])]))).is_err());
        assert!(open(Cursor::new(file(&[desc(f64::NAN, b"lpcm", 2, 2, 1, 2, 16), data(&[0; 8])]))).is_err());
        let e = open(Cursor::new(file(&[desc(44100.0, b"ima4", 0, 34, 64, 2, 0), data(&[0; 8])]))).err().unwrap();
        assert!(e.message.contains("ima4"), "{}", e.message);
    }
}
