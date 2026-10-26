//! Raw MP3 files (`.mp3`): a run of Layer III frames, optionally behind an ID3v2 tag.
//!
//! The file is walked once, header to header, to find where every frame lies; the list of frames
//! goes to the MP4 reader's machinery (`Mp4Demuxer::from_parts`), which reads, seeks and
//! interleaves them. A frame counts only if the header after it agrees with it, so garbage between
//! frames is stepped over a byte at a time. A leading Xing/Info/VBRI frame (a header describing the
//! file, holding no audio) is skipped.
//!
//! Not handled: the encoder delay and padding in a LAME tag (decoded audio starts with the
//! encoder's delay of silence, as the frames hold it), and streams that change sample rate
//! (frames at other rates than the first are dropped).

use crate::mp4demux::{Config, Mp4Demuxer, Sample, TrackInfo};
use ironav_codec::mp3::{parse_header, Header};
use ironav_core::{CodecId, Error, ErrorCode, Rational, Result, StreamInfo, StreamParams};
use std::io::{Read, Seek, SeekFrom};

/// Whether `magic` starts an MP3 file: an ID3v2 tag or a Layer III frame header.
pub fn looks_like_mp3(magic: &[u8]) -> bool {
    magic.starts_with(b"ID3") || parse_header(magic).is_some()
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

/// Whether the frame at `h` holds a Xing/Info/VBRI tag instead of audio.
fn is_info_frame<R: Read + Seek>(r: &mut R, at: u64, h: &Header) -> Result<bool> {
    let xing = 4 + if h.crc { 2 } else { 0 } + h.side_len();
    let mut tag = [0u8; 4];
    for off in [xing, 4 + 32] {
        r.seek(SeekFrom::Start(at + off as u64))?;
        if read_exact_or_eof(r, &mut tag)? && matches!(&tag, b"Xing" | b"Info" | b"VBRI") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Open an MP3 file for reading.
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
    let mut hdr = [0u8; 4];
    loop {
        if pos + 4 > len {
            break;
        }
        r.seek(SeekFrom::Start(pos))?;
        if !read_exact_or_eof(&mut r, &mut hdr)? {
            break;
        }
        let Some(h) = parse_header(&hdr) else {
            pos += 1;
            continue;
        };
        // the next header must agree, unless this is the last frame
        let next = pos + h.frame_len as u64;
        let ok = if next + 4 <= len {
            r.seek(SeekFrom::Start(next))?;
            // a different header after the frame is fine when only a trailer (ID3v1, APE) is left
            read_exact_or_eof(&mut r, &mut hdr)? && parse_header(&hdr).is_some_and(|n| n.sample_rate == h.sample_rate && n.lsf == h.lsf) || len - next <= 256
        } else {
            true
        };
        if !ok {
            pos += 1;
            continue;
        }
        let size = (h.frame_len as u64).min(len - pos);
        match &first {
            None => {
                let info = is_info_frame(&mut r, pos, &h)?;
                first = Some(h);
                if !info {
                    samples.push(Sample { stream: 0, sync: true, offset: pos, size: size as u32, dts: total, cts_offset: 0, duration: h.samples() as u32 });
                    total += h.samples() as i64;
                }
            }
            Some(f) if f.sample_rate == h.sample_rate && f.lsf == h.lsf => {
                samples.push(Sample { stream: 0, sync: true, offset: pos, size: size as u32, dts: total, cts_offset: 0, duration: h.samples() as u32 });
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
    let stream = StreamInfo {
        index: 0,
        codec: CodecId::Mp3,
        time_base: Rational::new(1, h.sample_rate as i64),
        duration: Some(total),
        bits: None,
        extradata: Vec::new(),
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: h.sample_rate, channels: h.channels() },
    };
    let track = TrackInfo { stream, config: Config { prefix: Vec::new(), nal_length: 0, bits: 8 }, convert: false };
    Ok(Mp4Demuxer::from_parts(r, "mp3", vec![track], samples))
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
