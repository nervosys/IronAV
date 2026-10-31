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
use crate::WriteSeek;
use ironav_core::{CodecId, Error, ErrorCode, Muxer, Packet, Rational, Result, StreamInfo, StreamParams};
use std::io::{Read, Seek, SeekFrom};

/// The largest file whose frames are indexed.
const MAX_SCAN: u64 = 2 << 30;

pub(crate) const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Header {
    pub(crate) profile: u8,
    pub(crate) sfi: u8,
    pub(crate) channels: u8,
    /// Header length in bytes (7, or 9 with a CRC).
    pub(crate) len: usize,
    /// Frame length including the header.
    pub(crate) frame: usize,
}

pub(crate) fn parse(b: &[u8]) -> Option<Header> {
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

/// Open an ADTS file for reading. The file is read once into memory to find its frames (a seek for
/// every frame costs more than it should), then dropped; packets are read from `r`.
pub fn open<R: Read + Seek + Send>(mut r: R) -> Result<Mp4Demuxer<R>> {
    let len = r.seek(SeekFrom::End(0))?;
    if len > MAX_SCAN {
        return Err(Error::new(ErrorCode::UnsupportedFormat, "AAC: files over 2 GiB are not supported"));
    }
    r.seek(SeekFrom::Start(0))?;
    let mut file = Vec::with_capacity(len as usize);
    r.by_ref().take(len).read_to_end(&mut file)?;
    let len = file.len();

    let mut pos = 0usize;
    if file.starts_with(b"ID3") && len >= 10 {
        let size = file[6..10].iter().fold(0usize, |a, &b| (a << 7) | (b & 0x7F) as usize);
        pos = 10 + size + if file[5] & 0x10 != 0 { 10 } else { 0 };
    }

    let mut first: Option<Header> = None;
    let mut samples = Vec::new();
    let mut total = 0i64;
    while pos + 7 <= len {
        let Some(h) = parse(&file[pos..pos + 7]) else {
            pos += 1;
            continue;
        };
        let next = pos + h.frame;
        let ok = if next + 7 <= len {
            // a different header after the frame is fine when only a trailer (ID3v1, APE) is left
            parse(&file[next..next + 7]).is_some_and(|n| n.sfi == h.sfi && n.channels == h.channels) || len - next <= 256
        } else {
            next <= len // the last frame must be whole
        };
        if !ok {
            pos += 1;
            continue;
        }
        let f = *first.get_or_insert(h);
        if (f.sfi, f.channels) == (h.sfi, h.channels) {
            samples.push(Sample { stream: 0, sync: true, offset: (pos + h.len) as u64, size: (h.frame - h.len) as u32, dts: total, cts_offset: 0, duration: 1024 });
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
    let track = TrackInfo { stream, config: Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 }, convert: false };
    Ok(Mp4Demuxer::from_parts(r, "aac", vec![track], samples))
}

/// Writes AAC access units behind ADTS headers (MPEG-4, no CRC), taking the profile, rate and channel
/// configuration from the stream's AudioSpecificConfig.
pub struct AdtsMuxer {
    out: Box<dyn WriteSeek>,
    /// The three header bytes after the sync word that do not depend on the frame's length.
    fixed: Option<[u8; 3]>,
}

impl AdtsMuxer {
    pub fn new(out: Box<dyn WriteSeek>) -> Self {
        Self { out, fixed: None }
    }
}

impl Muxer for AdtsMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [s] = streams else {
            return Err(Error::invalid_arg("ADTS holds exactly one audio stream"));
        };
        if s.codec != CodecId::Aac {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("ADTS can only hold AAC, not {}", s.codec.name())).alternatives(["aac"]));
        }
        let [a, b, ..] = s.extradata[..] else {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "ADTS needs the AudioSpecificConfig").hint("encode to aac, or copy from a file that carries it"));
        };
        let asc = u16::from_be_bytes([a, b]);
        let (aot, sfi, ch) = (asc >> 11, (asc >> 7) & 15, (asc >> 3) & 15);
        if !(1..=4).contains(&aot) || sfi >= 13 || !(1..=7).contains(&ch) {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "ADTS cannot describe this AudioSpecificConfig").hint("write it to mp4 or mkv"));
        }
        // ID 0 (MPEG-4), layer 0, no CRC | profile (object type - 1), rate, private 0, channels (high bit) | channels (low bits), original/home/copyright 0
        self.fixed = Some([0xF1, (((aot - 1) << 6) | (sfi << 2) | (ch >> 2)) as u8, ((ch & 3) << 6) as u8]);
        Ok(())
    }

    fn write_packet(&mut self, p: &Packet) -> Result<()> {
        let Some(f) = self.fixed else {
            return Err(Error::new(ErrorCode::Internal, "packet before the header"));
        };
        let len = p.data.len() + 7;
        if len >= 1 << 13 {
            return Err(Error::invalid_input("an AAC frame is too large for an ADTS header"));
        }
        // frame length (13 bits), buffer fullness 0x7FF (variable rate), one raw data block
        let h = [0xFF, f[0], f[1], f[2] | (len >> 11) as u8, (len >> 3) as u8, (((len & 7) << 5) | 0x1F) as u8, 0xFC];
        self.out.write_all(&h)?;
        self.out.write_all(&p.data)?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::Demuxer;
    use std::io::Cursor;

    /// An AAC-LC, 44.1 kHz, stereo frame of `body` payload bytes.
    fn frame(body: usize, crc: bool) -> Vec<u8> {
        let n = 7 + if crc { 2 } else { 0 } + body;
        let mut f = vec![0xFF, 0xF0 | (1 - crc as u8), 0x50, 0x80, (n >> 3) as u8, ((n & 7) << 5) as u8 | 0x1F, 0xFC];
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
