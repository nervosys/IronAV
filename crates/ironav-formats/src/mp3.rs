//! Raw MP3 files (`.mp3`): a run of Layer III frames, optionally behind an ID3v2 tag.
//!
//! The file is walked once, header to header, to find where every frame lies; the list of frames
//! goes to the MP4 reader's machinery (`Mp4Demuxer::from_parts`), which reads, seeks and
//! interleaves them. A frame counts only if the header after it agrees with it, so garbage between
//! frames is stepped over a byte at a time. A leading Xing/Info/VBRI frame (a header describing the
//! file, holding no audio) is skipped.
//!
//! The encoder delay and padding in a LAME tag, when there is one, go to the decoder as extradata
//! (two little-endian u32: how many decoded samples to drop at the start, and the decoded position
//! at which to stop), so playback is gapless the way FFmpeg's is: the delay plus the decoder's own
//! 529 samples are dropped at the start, the padding less 529 at the end.
//!
//! Writing: the frames as they come, behind an Info frame that carries the frame count, the size and
//! a LAME tag with the delay and padding, so players can play the file gapless and know its length.
//! It is written when the first packet arrives (it copies that frame's header) and patched at the end.
//!
//! Not handled: streams that change sample rate (frames at other rates than the first are dropped).

use crate::mp4demux::{Config, Mp4Demuxer, Sample, TrackInfo};
use ironav_codec::mp3::{parse_header, Header};
use crate::WriteSeek;
use ironav_core::{CodecId, Error, ErrorCode, Muxer, Packet, Rational, Result, StreamInfo, StreamParams};
use std::io::{Read, Seek, SeekFrom};

/// The largest file whose frames are indexed.
const MAX_SCAN: u64 = 2 << 30;

/// Whether `magic` starts an MP3 file: an ID3v2 tag or a Layer III frame header.
pub fn looks_like_mp3(magic: &[u8]) -> bool {
    magic.starts_with(b"ID3") || parse_header(magic).is_some()
}

/// The encoder delay and end padding of a LAME tag in the Xing/Info frame at `at`.
fn lame_tag(file: &[u8], at: usize, h: &Header) -> Option<(u32, u32)> {
    let xing = at + 4 + if h.crc { 2 } else { 0 } + h.side_len();
    let tag = file.get(xing..xing + 8)?;
    if !matches!(&tag[..4], b"Xing" | b"Info") {
        return None;
    }
    let flags = u32::from_be_bytes(tag[4..8].try_into().unwrap());
    // optional fields: frames, bytes, TOC, quality; then the encoder string and the LAME fields
    let skip: usize = [(1, 4), (2, 4), (4, 100), (8, 4)].iter().filter(|(f, _)| flags & f != 0).map(|(_, n)| n).sum();
    let enc_at = xing + 8 + skip;
    let enc = file.get(enc_at..enc_at + 9)?;
    if !(enc.starts_with(b"LAME") || enc.starts_with(b"Lavf") || enc.starts_with(b"Lavc")) {
        return None;
    }
    // revision/method, lowpass, replay gain, flags, ABR bitrate: 12 bytes, then the delay
    let t = file.get(enc_at + 9 + 12..enc_at + 9 + 12 + 3)?;
    let delay = (t[0] as u32) << 4 | (t[1] as u32) >> 4;
    let pad = ((t[1] as u32) & 0xF) << 8 | t[2] as u32;
    // a delay beyond a few frames is a garbage field
    (delay <= 2304 && pad <= 4095).then_some((delay, pad))
}

/// Whether the frame at `at` holds a Xing/Info/VBRI tag instead of audio.
fn is_info_frame(file: &[u8], at: usize, h: &Header) -> bool {
    let xing = 4 + if h.crc { 2 } else { 0 } + h.side_len();
    [xing, 4 + 32].iter().any(|&off| file.get(at + off..at + off + 4).is_some_and(|t| matches!(t, b"Xing" | b"Info" | b"VBRI")))
}

/// Open an MP3 file for reading. The file is read once into memory to find its frames (a seek for
/// every frame costs more than the decoding does), then dropped; packets are read from `r`.
pub fn open<R: Read + Seek + Send>(mut r: R) -> Result<Mp4Demuxer<R>> {
    let len = r.seek(SeekFrom::End(0))?;
    if len > MAX_SCAN {
        return Err(Error::new(ErrorCode::UnsupportedFormat, "MP3: files over 2 GiB are not supported"));
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
    let mut lame = None;
    while pos + 4 <= len {
        let Some(h) = parse_header(&file[pos..pos + 4]) else {
            pos += 1;
            continue;
        };
        // the next header must agree, unless this is the last frame
        let next = pos + h.frame_len;
        let ok = if next + 4 <= len {
            // a different header after the frame is fine when only a trailer (ID3v1, APE) is left
            parse_header(&file[next..next + 4]).is_some_and(|n| n.sample_rate == h.sample_rate && n.lsf == h.lsf) || len - next <= 256
        } else {
            true
        };
        if !ok {
            pos += 1;
            continue;
        }
        let size = h.frame_len.min(len - pos);
        match &first {
            None => {
                let info = is_info_frame(&file, pos, &h);
                first = Some(h);
                if info {
                    lame = lame_tag(&file, pos, &h);
                } else {
                    samples.push(Sample { stream: 0, sync: true, offset: pos as u64, size: size as u32, dts: total, cts_offset: 0, duration: h.samples() as u32 });
                    total += h.samples() as i64;
                }
            }
            Some(f) if f.sample_rate == h.sample_rate && f.lsf == h.lsf => {
                samples.push(Sample { stream: 0, sync: true, offset: pos as u64, size: size as u32, dts: total, cts_offset: 0, duration: h.samples() as u32 });
                total += h.samples() as i64;
            }
            Some(_) => {}
        }
        pos = next;
    }
    let Some(h) = first else {
        return Err(Error::new(ErrorCode::InvalidInput, "MP3: no Layer III frames found"));
    };
    if samples.is_empty() {
        return Err(Error::new(ErrorCode::InvalidInput, "MP3: the file holds no audio frames"));
    }
    // the decoder's own delay is 528 samples + 1
    let extradata = lame.map_or(Vec::new(), |(delay, pad)| {
        let start = (delay + 529).min(total as u32);
        let end = (total - (pad as i64 - 529).max(0)).max(start as i64) as u32;
        [start.to_le_bytes(), end.to_le_bytes()].concat()
    });
    let stream = StreamInfo {
        index: 0,
        codec: CodecId::Mp3,
        time_base: Rational::new(1, h.sample_rate as i64),
        duration: Some(total),
        bits: None,
        extradata,
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: h.sample_rate, channels: h.channels() },
    };
    let track = TrackInfo { stream, config: Config { prefix: Vec::new(), nal_length: 0, bits: 8 }, convert: false };
    Ok(Mp4Demuxer::from_parts(r, "mp3", vec![track], samples))
}

/// Writes MP3 frames, with an Info frame in front when the stream says how much of its start and end
/// is not audio.
pub struct Mp3Muxer {
    out: Box<dyn WriteSeek>,
    /// Decoded samples to skip at the start (the decoder's own 529 included), if the stream says.
    start: Option<u32>,
    /// The decoded position where the audio ends, when the stream says (a copied LAME tag does).
    end: Option<u32>,
    /// Where the Info frame starts, once written.
    info_at: Option<u64>,
    frames: u32,
    bytes: u64,
    samples: i64,
    /// Offset of the delay field inside the Info frame.
    delay_at: usize,
    xing_at: usize,
}

impl Mp3Muxer {
    pub fn new(out: Box<dyn WriteSeek>) -> Self {
        Self { out, start: None, end: None, info_at: None, frames: 0, bytes: 0, samples: 0, delay_at: 0, xing_at: 0 }
    }

    /// The Info frame for a stream whose frames look like `first`: all zeros but the header and the tag.
    fn info_frame(&mut self, first: &[u8], h: &Header) -> Vec<u8> {
        let len = h.frame_len - (first[2] >> 1 & 1) as usize;
        let mut f = vec![0u8; len];
        f[..4].copy_from_slice(&first[..4]);
        f[2] &= !2; // no padding byte
        f[3] &= 0xCF; // no joint-stereo extension
        let at = 4 + h.side_len();
        f[at..at + 4].copy_from_slice(b"Info");
        f[at + 4..at + 8].copy_from_slice(&3u32.to_be_bytes()); // flags: frame count and size
        // parsers (FFmpeg's, and the reader above) only trust the delay behind one of three names
        f[at + 16..at + 25].copy_from_slice(b"Lavc-IAV ");
        self.xing_at = at;
        self.delay_at = at + 16 + 9 + 12;
        f
    }
}

impl Muxer for Mp3Muxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [s] = streams else {
            return Err(Error::invalid_arg("mp3 holds exactly one audio stream"));
        };
        if s.codec != CodecId::Mp3 {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("mp3 can only hold MP3 audio, not {}", s.codec.name())).alternatives(["mp3"]));
        }
        if let [a, b, c, d, e, f, g, h] = s.extradata[..] {
            self.start = Some(u32::from_le_bytes([a, b, c, d]));
            self.end = Some(u32::from_le_bytes([e, f, g, h]));
        } else if s.lead_in > 0 {
            self.start = Some(s.lead_in as u32);
        }
        Ok(())
    }

    fn write_packet(&mut self, p: &Packet) -> Result<()> {
        if self.info_at.is_none() {
            let Some(h) = parse_header(&p.data) else {
                return Err(Error::invalid_input("mp3: a packet is not a Layer III frame"));
            };
            let pos = self.out.stream_position()?;
            self.info_at = Some(pos);
            if self.start.is_some() {
                let info = self.info_frame(&p.data, &h);
                self.out.write_all(&info)?;
                self.bytes += info.len() as u64;
            } else {
                self.info_at = Some(u64::MAX); // nothing to patch
            }
        }
        self.out.write_all(&p.data)?;
        self.bytes += p.data.len() as u64;
        self.frames += 1;
        self.samples += p.duration;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if let (Some(at), Some(start)) = (self.info_at.filter(|&a| a != u64::MAX), self.start) {
            let total = self.frames as i64 * 1152;
            let end = self.end.map_or(self.samples, |e| e as i64).min(total);
            let delay = start.saturating_sub(529).min(2304);
            let pad = ((total - end) as u32 + 529).min(4095);
            self.out.seek(SeekFrom::Start(at + self.xing_at as u64 + 8))?;
            self.out.write_all(&self.frames.to_be_bytes())?;
            self.out.write_all(&(self.bytes as u32).to_be_bytes())?;
            self.out.seek(SeekFrom::Start(at + self.delay_at as u64))?;
            self.out.write_all(&[(delay >> 4) as u8, ((delay & 0xF) << 4) as u8 | (pad >> 8) as u8, pad as u8])?;
            self.out.seek(SeekFrom::End(0))?;
        }
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::Demuxer;
    use std::io::Cursor;

    /// A 128 kb/s 44.1 kHz stereo frame (417 or 418 bytes) with the given first body byte.
    fn frame(pad: bool, body: u8) -> Vec<u8> {
        let len = 417 + pad as usize;
        let mut f = vec![0xFF, 0xFB, 0x90 | ((pad as u8) << 1), 0x00];
        f.resize(len, body);
        f
    }

    #[test]
    fn finds_frames_behind_an_id3_tag_and_junk() {
        let mut file = b"ID3\x04\x00\x00\x00\x00\x00\x05hello".to_vec();
        file.extend([0x12, 0x34, 0x56]); // junk before the first frame
        for i in 0..4 {
            file.extend(frame(i % 2 == 1, 7));
        }
        file.extend(b"TAG");
        file.extend(vec![0; 125]);
        assert!(looks_like_mp3(&file));
        let mut d = open(Cursor::new(file)).unwrap();
        let s = d.streams()[0].clone();
        assert_eq!(s.codec, CodecId::Mp3);
        assert_eq!(s.time_base, Rational::new(1, 44100));
        let mut pts = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            assert_eq!(&p.data[..2], &[0xFF, 0xFB]);
            pts.push(p.pts);
        }
        assert_eq!(pts, vec![0, 1152, 2304, 3456]);
    }

    #[test]
    fn a_lame_tag_gives_the_decoder_its_trim() {
        let mut info = frame(false, 0);
        info[36..40].copy_from_slice(b"Info");
        info[40..44].copy_from_slice(&1u32.to_be_bytes()); // flags: frame count only
        info[44..48].copy_from_slice(&3u32.to_be_bytes());
        info[48..57].copy_from_slice(b"LAME3.100");
        // delay 576 (0x240) and padding 1000 (0x3E8): 0x24 0x03 0xE8; they follow 12 more bytes
        info[69..72].copy_from_slice(&[0x24, 0x03, 0xE8]);
        let mut file = info;
        for _ in 0..3 {
            file.extend(frame(false, 1));
        }
        let d = open(Cursor::new(file)).unwrap();
        let x = &d.streams()[0].extradata;
        let (start, end) = (u32::from_le_bytes(x[..4].try_into().unwrap()), u32::from_le_bytes(x[4..].try_into().unwrap()));
        assert_eq!((start, end), (576 + 529, 3 * 1152 - (1000 - 529)));
    }

    #[test]
    fn skips_a_leading_info_frame() {
        let mut info = frame(false, 0);
        info[36..40].copy_from_slice(b"Info"); // after header (4) and stereo side info (32)
        let mut file = info;
        for _ in 0..3 {
            file.extend(frame(false, 1));
        }
        let mut d = open(Cursor::new(file)).unwrap();
        let mut n = 0;
        let mut first = None;
        while let Some(p) = d.read_packet().unwrap() {
            first.get_or_insert(p.pts);
            n += 1;
        }
        assert_eq!((n, first), (3, Some(0)));
    }
}
