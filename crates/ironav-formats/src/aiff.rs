//! AIFF and AIFF-C (`.aiff`, `.aif`, `.aifc`): a FORM container of chunks, with the format in COMM
//! and the samples in SSND. Samples are big-endian, so each is reversed on reading, and 8-bit
//! samples (signed here) are turned into the unsigned form `pcm_u8` has.
//!
//! Read: 8, 16, 24 and 32-bit integer PCM (other widths are left-justified in their bytes, as the
//! format stores them), and AIFF-C `NONE`, `twos`, `in24`, `in32`, `raw ` (8-bit unsigned), `sowt`
//! (little-endian) and `fl32` and `fl64` (float). Any other AIFF-C compression (u-law, A-law, IMA4,
//! MACE and so on) is refused by name. The sample rate is read from its 80-bit extended form.
//!
//! The reader also serves CAF's linear PCM (`RawPcmDemuxer`).
//!
//! Not handled: markers, loops and comments (skipped); a file whose SSND chunk comes before COMM and
//! has a size that runs past the end of the file.

use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Packet, Rational, Result, StreamInfo, StreamParams};
use std::io::{Read, Seek, SeekFrom};

/// Audio frames per packet, as the WAV reader has.
const FRAMES_PER_PACKET: usize = 4096;

/// The chunks looked at before giving up: a real file has a handful.
const MAX_CHUNKS: usize = 4096;

/// Whether `magic` starts an AIFF or AIFF-C file.
pub fn looks_like_aiff(magic: &[u8]) -> bool {
    magic.len() >= 12 && &magic[..4] == b"FORM" && (&magic[8..12] == b"AIFF" || &magic[8..12] == b"AIFC")
}

/// How the samples of a raw PCM stream are stored.
pub(crate) struct PcmLayout {
    pub codec: CodecId,
    pub rate: u32,
    pub channels: u16,
    /// Bytes of one sample as stored (1 to 8).
    pub width: usize,
    /// The stored samples are big-endian.
    pub big: bool,
    /// The stored 8-bit samples are signed.
    pub signed8: bool,
}

/// Reads packets of PCM from a byte range of a file, turning the samples into the little-endian
/// form the PCM codecs have, and seeks to any frame.
pub(crate) struct RawPcmDemuxer<R: Read + Seek + Send> {
    name: &'static str,
    r: R,
    streams: Vec<StreamInfo>,
    align: usize,
    swap: usize,
    flip: bool,
    start: u64,
    total_frames: u64,
    pos_frames: u64,
}

impl<R: Read + Seek + Send> RawPcmDemuxer<R> {
    /// `len` bytes of samples start `start` bytes into `r`; a partial frame at the end is ignored.
    pub(crate) fn new(name: &'static str, mut r: R, start: u64, len: u64, l: &PcmLayout) -> Result<Self> {
        let align = l.width * l.channels as usize;
        if align == 0 || l.rate == 0 {
            return Err(Error::invalid_input(format!("{name}: no channels or no sample rate")));
        }
        let total_frames = len / align as u64;
        let stream = StreamInfo {
            index: 0,
            codec: l.codec,
            time_base: Rational::new(1, l.rate as i64),
            duration: Some(total_frames as i64),
            bits: Some((l.width * 8).min(255) as u8),
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Audio { sample_rate: l.rate, channels: l.channels },
        };
        r.seek(SeekFrom::Start(start))?;
        Ok(Self {
            name,
            r,
            streams: vec![stream],
            align,
            swap: if l.big && l.width > 1 { l.width } else { 0 },
            flip: l.signed8 && l.width == 1,
            start,
            total_frames,
            pos_frames: 0,
        })
    }
}

impl<R: Read + Seek + Send> Demuxer for RawPcmDemuxer<R> {
    fn format_name(&self) -> &'static str {
        self.name
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn read_packet(&mut self) -> Result<Option<Packet>> {
        let frames = (self.total_frames - self.pos_frames).min(FRAMES_PER_PACKET as u64) as usize;
        if frames == 0 {
            return Ok(None);
        }
        let mut data = vec![0u8; frames * self.align];
        self.r.read_exact(&mut data).map_err(|_| Error::invalid_input(format!("{}: the sound data is cut short", self.name)))?;
        if self.swap > 0 {
            data.chunks_exact_mut(self.swap).for_each(<[u8]>::reverse);
        }
        if self.flip {
            data.iter_mut().for_each(|b| *b ^= 0x80);
        }
        let pts = self.pos_frames as i64;
        self.pos_frames += frames as u64;
        Ok(Some(Packet { stream: 0, pts, duration: frames as i64, data }))
    }
    fn seek(&mut self, seconds: f64) -> Result<f64> {
        let rate = self.streams[0].time_base.invert().as_f64();
        let frame = ((seconds.max(0.0) * rate).round() as u64).min(self.total_frames);
        self.r.seek(SeekFrom::Start(self.start + frame * self.align as u64))?;
        self.pos_frames = frame;
        Ok(frame as f64 / rate)
    }
}

/// A sample rate from its IEEE 754 80-bit extended form.
fn extended(b: &[u8]) -> Option<f64> {
    let exp = (u16::from_be_bytes([b[0], b[1]]) & 0x7FFF) as i32;
    let mant = u64::from_be_bytes(b[2..10].try_into().ok()?);
    if mant == 0 {
        return Some(0.0);
    }
    if exp == 0x7FFF {
        return None; // infinity or NaN
    }
    Some(mant as f64 * 2f64.powi(exp - 16383 - 63))
}

/// Printable text for a four-character code.
pub(crate) fn fourcc_text(f: &[u8]) -> String {
    f.iter().map(|&c| if (0x20..0x7F).contains(&c) { c as char } else { '?' }).collect()
}

fn unsupported(msg: String) -> Error {
    Error::new(ErrorCode::UnsupportedCodec, msg).hint("AIFF files with integer PCM (8 to 32 bits), or AIFF-C NONE, sowt, fl32 and fl64, are read")
}

/// Opens an AIFF or AIFF-C file.
pub fn open<R: Read + Seek + Send + 'static>(mut r: R) -> Result<Box<dyn Demuxer>> {
    let total = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let mut head = [0u8; 12];
    r.read_exact(&mut head).map_err(|_| Error::invalid_input("AIFF: the file is shorter than its header"))?;
    if !looks_like_aiff(&head) {
        return Err(Error::invalid_input("AIFF: not a FORM AIFF or FORM AIFC file"));
    }
    let aifc = &head[8..12] == b"AIFC";
    let mut pos = 12u64;
    let mut comm: Option<(u16, u32, u16, f64, [u8; 4])> = None;
    let mut ssnd: Option<(u64, u64)> = None;
    for _ in 0..MAX_CHUNKS {
        if pos + 8 > total || (comm.is_some() && ssnd.is_some()) {
            break;
        }
        r.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 8];
        r.read_exact(&mut h)?;
        let size = u32::from_be_bytes([h[4], h[5], h[6], h[7]]) as u64;
        let body = pos + 8;
        match &h[..4] {
            b"COMM" => {
                let need = if aifc { 22 } else { 18 };
                if size < need || size > 1 << 16 || body + size > total {
                    return Err(Error::invalid_input("AIFF: the COMM chunk is the wrong size"));
                }
                let mut b = vec![0u8; size as usize];
                r.read_exact(&mut b).map_err(|_| Error::invalid_input("AIFF: the COMM chunk is cut short"))?;
                let rate = extended(&b[8..18]).filter(|r| (1.0..=16_000_000.0).contains(r)).ok_or_else(|| Error::invalid_input("AIFF: the sample rate is not a usable number"))?;
                let kind = if aifc { [b[18], b[19], b[20], b[21]] } else { *b"NONE" };
                comm = Some((u16::from_be_bytes([b[0], b[1]]), u32::from_be_bytes([b[2], b[3], b[4], b[5]]), u16::from_be_bytes([b[6], b[7]]), rate, kind));
            }
            b"SSND" => {
                if size < 8 {
                    return Err(Error::invalid_input("AIFF: the SSND chunk is too short"));
                }
                let mut o = [0u8; 4];
                r.read_exact(&mut o)?;
                let offset = u32::from_be_bytes(o) as u64;
                let start = body + 8 + offset;
                // a streaming writer leaves a size that runs past the file: trust the file's end
                let end = (body + size).min(total);
                ssnd = Some((start, end.saturating_sub(start)));
                if body + size > total {
                    break;
                }
            }
            _ => {}
        }
        pos = body + size + (size & 1);
    }
    let (channels, frames, bits, rate, kind) = comm.ok_or_else(|| Error::invalid_input("AIFF: the file has no COMM chunk"))?;
    let (start, len) = ssnd.ok_or_else(|| Error::invalid_input("AIFF: the file has no SSND chunk"))?;
    if channels == 0 || channels > 255 {
        return Err(Error::invalid_input("AIFF: the channel count is zero or implausible"));
    }
    if bits == 0 || bits > 64 {
        return Err(Error::invalid_input("AIFF: the sample size is zero or implausible"));
    }
    // (codec, bytes per sample, big-endian)
    let (codec, width, big) = match &kind {
        b"fl32" | b"FL32" => (CodecId::PcmF32Le, 4, true),
        b"fl64" | b"FL64" => (CodecId::PcmF64Le, 8, true),
        b"NONE" | b"twos" | b"in24" | b"in32" | b"sowt" | b"raw " => {
            if bits > 32 {
                return Err(unsupported(format!("AIFF: {bits}-bit integer samples are not supported")));
            }
            let width = (bits as usize).div_ceil(8);
            // 8-bit samples are signed here (except in `raw `): flipped on reading
            let codec = match width {
                1 => CodecId::PcmU8,
                2 => CodecId::PcmS16Le,
                3 => CodecId::PcmS24Le,
                _ => CodecId::PcmS32Le,
            };
            (codec, width, &kind != b"sowt")
        }
        other => return Err(unsupported(format!("AIFF-C compression '{}' is not supported", fourcc_text(other)))),
    };
    // the COMM chunk's frame count may be 0 for a streamed file, or larger than the sound data
    let have = len / (width as u64 * channels as u64);
    let frames = if frames == 0 { have } else { have.min(frames as u64) };
    let layout = PcmLayout { codec, rate: rate.round() as u32, channels, width, big, signed8: &kind != b"raw " };
    Ok(Box::new(RawPcmDemuxer::new("aiff", r, start, frames * width as u64 * channels as u64, &layout)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn extended_bytes(rate: u32) -> [u8; 10] {
        let shift = rate.leading_zeros();
        let exp = 16383 + 31 - shift as u16;
        let mant = (rate as u64) << (32 + shift);
        let mut b = [0u8; 10];
        b[..2].copy_from_slice(&exp.to_be_bytes());
        b[2..].copy_from_slice(&mant.to_be_bytes());
        b
    }

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend((body.len() as u32).to_be_bytes());
        v.extend(body);
        if body.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn file(kind: &[u8; 4], channels: u16, bits: u16, rate: u32, data: &[u8]) -> Vec<u8> {
        let aifc = kind != b"AIFF";
        let mut comm = channels.to_be_bytes().to_vec();
        comm.extend(((data.len() / (channels as usize * (bits as usize).div_ceil(8)).max(1)) as u32).to_be_bytes());
        comm.extend(bits.to_be_bytes());
        comm.extend(extended_bytes(rate));
        if aifc {
            comm.extend(kind);
            comm.extend([0, 0]); // empty name
        }
        let mut ssnd = vec![0u8; 8];
        ssnd.extend(data);
        let mut body = if aifc { b"AIFC".to_vec() } else { b"AIFF".to_vec() };
        body.extend(chunk(b"COMM", &comm));
        body.extend(chunk(b"SSND", &ssnd));
        let mut f = b"FORM".to_vec();
        f.extend((body.len() as u32).to_be_bytes());
        f.extend(body);
        f
    }

    #[test]
    fn the_extended_rate_is_read() {
        for rate in [8000, 11025, 44100, 48000, 96000, 192000] {
            assert_eq!(extended(&extended_bytes(rate)), Some(rate as f64));
        }
        assert_eq!(extended(&[0x7F, 0xFF, 0x80, 0, 0, 0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn big_endian_samples_are_reversed_and_8_bit_flipped() {
        let mut d = open(Cursor::new(file(b"AIFF", 1, 16, 8000, &[0x01, 0x02, 0xFF, 0xFE]))).unwrap();
        assert_eq!(d.streams()[0].codec, CodecId::PcmS16Le);
        assert_eq!(d.read_packet().unwrap().unwrap().data, [0x02, 0x01, 0xFE, 0xFF]);
        let mut d = open(Cursor::new(file(b"AIFF", 1, 8, 8000, &[0x00, 0x7F, 0x80, 0xFF]))).unwrap();
        assert_eq!(d.streams()[0].codec, CodecId::PcmU8);
        assert_eq!(d.read_packet().unwrap().unwrap().data, [0x80, 0xFF, 0x00, 0x7F]);
        let mut d = open(Cursor::new(file(b"sowt", 2, 16, 44100, &[1, 2, 3, 4]))).unwrap();
        assert_eq!(d.read_packet().unwrap().unwrap().data, [1, 2, 3, 4], "sowt is already little-endian");
        let mut d = open(Cursor::new(file(b"fl32", 1, 32, 48000, &1.0f32.to_be_bytes()))).unwrap();
        assert_eq!(d.streams()[0].codec, CodecId::PcmF32Le);
        assert_eq!(d.read_packet().unwrap().unwrap().data, 1.0f32.to_le_bytes());
    }

    #[test]
    fn other_compressions_are_named_in_the_error() {
        let e = open(Cursor::new(file(b"ulaw", 1, 16, 8000, &[0, 0]))).err().unwrap();
        assert_eq!(e.code, ErrorCode::UnsupportedCodec);
        assert!(e.message.contains("ulaw"), "{}", e.message);
    }

    #[test]
    fn seek_is_to_the_sample() {
        let data: Vec<u8> = (0..1000u16).flat_map(|i| i.to_be_bytes()).collect();
        let mut d = open(Cursor::new(file(b"AIFF", 1, 16, 1000, &data))).unwrap();
        assert_eq!(d.seek(0.25).unwrap(), 0.25);
        let p = d.read_packet().unwrap().unwrap();
        assert_eq!((p.pts, u16::from_le_bytes([p.data[0], p.data[1]])), (250, 250));
    }

    #[test]
    fn damaged_files_are_errors() {
        let good = file(b"AIFF", 2, 16, 44100, &[0; 64]);
        for n in 0..good.len() {
            if let Ok(mut d) = open(Cursor::new(good[..n].to_vec())) {
                while d.read_packet().ok().flatten().is_some() {}
            }
        }
        let mut bad = good.clone();
        bad[16..20].copy_from_slice(&u32::MAX.to_be_bytes()); // COMM size
        assert!(open(Cursor::new(bad)).is_err());
        let mut bad = good.clone();
        bad[20..22].copy_from_slice(&0u16.to_be_bytes()); // zero channels
        assert!(open(Cursor::new(bad)).is_err());
        let mut bad = good;
        bad[26..28].copy_from_slice(&0x7FFFu16.to_be_bytes()); // sample size 32767
        assert!(open(Cursor::new(bad)).is_err());
    }
}
