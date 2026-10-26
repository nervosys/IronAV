//! Raw AAC in ADTS (`.aac`): a run of frames, each behind a 7-byte header (9 with a CRC), optionally
//! behind an ID3v2 tag.
//!
//! The file is walked once, header to header. The frames go to the MP4 reader's machinery
//! (`Mp4Demuxer::from_parts`) without their ADTS headers, as the raw access units the AAC decoder
//! reads from MP4, and the header's profile, sample rate and channel configuration become the
//! AudioSpecificConfig. A frame counts only if the header after it agrees with it.
//!
//! Not handled: ADTS frames holding several raw data blocks, channel configuration 0 (the layout is
//! in the stream), and streams that change their parameters (frames that differ from the first are
//! dropped). No encoder detects HE-AAC here either: an SBR stream decodes as its core.

use crate::mp4demux::{Config, Mp4Demuxer, Sample, TrackInfo};
use ironav_core::{CodecId, Error, ErrorCode, Rational, Result, StreamInfo, StreamParams};
use std::io::{Read, Seek, SeekFrom};

const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

#[derive(Clone, Copy, PartialEq)]
struct Header {
    profile: u8,
    sfi: u8,
    channels: u8,
    /// Header length in bytes (7, or 9 with a CRC).
    len: usize,
    /// Frame length including the header.
    frame: usize,
}

fn parse(b: &[u8]) -> Option<Header> {
    if b.len() < 7 || b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
        return None;
    }
    let sfi = (b[2] >> 2) & 0xF;
    let channels = ((b[2] & 1) << 2) | (b[3] >> 6);
    let frame = (((b[3] & 3) as usize) << 11) | ((b[4] as usize) << 3) | (b[5] >> 5) as usize;
    let len = if b[1] & 1 == 1 { 7 } else { 9 };
    // one raw data block per frame
    (sfi < 13 && channels != 0 && frame > len && b[6] & 3 == 0).then_some(Header { profile: b[2] >> 6, sfi, channels, len, frame })
}

/// Whether `magic` starts an ADTS frame.
pub fn looks_like_adts(magic: &[u8]) -> bool {
    // MP3 frames share the sync word but have a layer other than 0
    magic.len() >= 7 && parse(magic).is_some()
}

fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        let n = r.read(&mut buf[got..])?;
        if n == 0 {
            return Ok(false);
        }
        got += n;
    }
    Ok(true)
}

/// Open an ADTS file for reading.
pub fn open<R: Read + Seek + Send>(mut r: R) -> Result<Mp4Demuxer<R>> {
    let len = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let mut pos = 0u64;
    let mut id3 = [0u8; 10];
    if read_exact_or_eof(&mut r, &mut id3)? && id3.starts_with(b"ID3") {
        let size = id3[6..10].iter().fold(0u64, |a, &b| (a << 7) | (b & 0x7F) as u64);
        pos = 10 + size + if id3[5] & 0x10 != 0 { 10 } else { 0 };
    }

    let mut first: Option<Header> = None;
    let mut samples = Vec::new();
    let mut total = 0i64;
    let mut hdr = [0u8; 7];
    while pos + 7 <= len {
        r.seek(SeekFrom::Start(pos))?;
        if !read_exact_or_eof(&mut r, &mut hdr)? {
            break;
        }
        let Some(h) = parse(&hdr) else {
            pos += 1;
            continue;
        };
        let next = pos + h.frame as u64;
        let ok = if next + 7 <= len {
            r.seek(SeekFrom::Start(next))?;
            let mut n = [0u8; 7];
            // a different header after the frame is fine when only a trailer (ID3v1, APE) is left
            read_exact_or_eof(&mut r, &mut n)? && parse(&n).is_some_and(|n| n.sfi == h.sfi && n.channels == h.channels) || len - next <= 256
        } else {
            next <= len // the last frame must be whole
        };
        if !ok {
            pos += 1;
            continue;
        }
        let f = *first.get_or_insert(h);
        if (f.sfi, f.channels) == (h.sfi, h.channels) {
            samples.push(Sample { stream: 0, sync: true, offset: pos + h.len as u64, size: (h.frame - h.len) as u32, dts: total, cts_offset: 0, duration: 1024 });
            total += 1024;
        }
        pos = next;
    }
    let Some(h) = first else {
        return Err(Error::new(ErrorCode::InvalidInput, "AAC: no ADTS frames found"));
    };
    let rate = RATES[h.sfi as usize];
    // AudioSpecificConfig: object type (profile + 1) 5 bits, rate index 4, channel configuration 4
    let asc = ((h.profile as u16 + 1) << 11) | ((h.sfi as u16) << 7) | ((h.channels as u16) << 3);
    let stream = StreamInfo {
        index: 0,
        codec: CodecId::Aac,
        time_base: Rational::new(1, rate as i64),
        duration: Some(total),
        bits: None,
        extradata: asc.to_be_bytes().to_vec(),
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: rate, channels: if h.channels == 7 { 8 } else { h.channels as u16 } },
    };
    let track = TrackInfo { stream, config: Config { prefix: Vec::new(), nal_length: 0, bits: 8 }, convert: false };
    Ok(Mp4Demuxer::from_parts(r, "aac", vec![track], samples))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::Demuxer;
    use std::io::Cursor;

    /// An AAC-LC, 44.1 kHz, stereo frame of `body` payload bytes.
    fn frame(body: usize, crc: bool) -> Vec<u8> {
        let n = 7 + if crc { 2 } else { 0 } + body;
        let mut f = vec![0xFF, 0xF0 | 1 - crc as u8, 0x50, 0x80, (n >> 3) as u8, ((n & 7) << 5) as u8 | 0x1F, 0xFC];
        f.resize(n, 0xAA);
        f
    }

    #[test]
    fn strips_headers_and_builds_the_config() {
        let mut file = b"ID3\x04\x00\x00\x00\x00\x00\x03abc".to_vec();
        file.extend([0x00, 0x11]); // junk
        for i in 0..4 {
            file.extend(frame(20 + i, i == 2));
        }
        file.extend(b"TAG");
        file.extend(vec![0; 125]);
        let mut d = open(Cursor::new(file)).unwrap();
        let s = d.streams()[0].clone();
        assert_eq!((s.codec, s.time_base, s.extradata.clone()), (CodecId::Aac, Rational::new(1, 44100), vec![0x12, 0x10]));
        let mut got = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            got.push((p.pts, p.data.len()));
        }
        assert_eq!(got, vec![(0, 20), (1024, 21), (2048, 22), (3072, 23)]);
    }

    #[test]
    fn mp3_frames_are_not_adts() {
        assert!(!looks_like_adts(&[0xFF, 0xFB, 0x90, 0x00, 0, 0, 0, 0]));
        assert!(looks_like_adts(&frame(10, false)));
    }
}
