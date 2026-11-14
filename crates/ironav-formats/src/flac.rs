//! Native FLAC container: `fLaC` marker, metadata blocks (STREAMINFO is read and written;
//! other blocks such as Vorbis comments and pictures are skipped), then frames.

use crate::WriteSeek;
use ironav_codec::flac::crc::crc16_update;
use ironav_codec::flac::header::{parse_header, MAX_HEADER_LEN};
use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Muxer, Packet, Rational, Result, StreamInfo, StreamParams};
use std::io::{Read, Seek, SeekFrom};

// Small under test so unit tests exercise many refills and compactions.
const READ_CHUNK: usize = if cfg!(test) { 4096 } else { 1 << 20 };
const COMPACT_AT: usize = if cfg!(test) { 16 * 1024 } else { 4 * READ_CHUNK };

fn corrupt(msg: &str) -> Error {
    Error::invalid_input(format!("FLAC: {msg}"))
}

/// True if `magic` could start a FLAC file (directly, or after an ID3v2 tag).
pub fn looks_like_flac(magic: &[u8]) -> bool {
    magic.starts_with(b"fLaC") || magic.starts_with(b"ID3")
}

pub struct FlacDemuxer<R: Read + Seek + Send> {
    r: R,
    streams: Vec<StreamInfo>,
    buf: Vec<u8>,
    start: usize,
    eof: bool,
    pts: i64,
    frames_start: u64,
    /// A packet read while seeking that belongs to the new position.
    held: Option<Packet>,
}

impl<R: Read + Seek + Send> FlacDemuxer<R> {
    pub fn open(mut r: R) -> Result<Self> {
        let mut tag = [0u8; 10];
        r.read_exact(&mut tag[..4])?;
        if &tag[..3] == b"ID3" {
            r.read_exact(&mut tag[4..10])?;
            let size = ((tag[6] as u64 & 0x7F) << 21) | ((tag[7] as u64 & 0x7F) << 14) | ((tag[8] as u64 & 0x7F) << 7) | (tag[9] as u64 & 0x7F);
            r.seek(SeekFrom::Current(size as i64))?;
            r.read_exact(&mut tag[..4])?;
        }
        if &tag[..4] != b"fLaC" {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "not a FLAC stream (missing 'fLaC' marker)"));
        }
        let mut info: Option<[u8; 34]> = None;
        loop {
            let mut h = [0u8; 4];
            r.read_exact(&mut h)?;
            let (last, kind) = (h[0] & 0x80 != 0, h[0] & 0x7F);
            let len = u32::from_be_bytes([0, h[1], h[2], h[3]]) as u64;
            if kind == 0 {
                if len != 34 {
                    return Err(corrupt("STREAMINFO must be 34 bytes"));
                }
                let mut b = [0u8; 34];
                r.read_exact(&mut b)?;
                info = Some(b);
            } else {
                r.seek(SeekFrom::Current(len as i64))?;
            }
            if last {
                break;
            }
        }
        let b = info.ok_or_else(|| corrupt("missing STREAMINFO block"))?;
        let packed = u64::from_be_bytes([b[10], b[11], b[12], b[13], b[14], b[15], b[16], b[17]]);
        let rate = (packed >> 44) as u32;
        let channels = ((packed >> 41) & 7) as u16 + 1;
        let bits = ((packed >> 36) & 31) as u8 + 1;
        let total = (packed & 0xF_FFFF_FFFF) as i64;
        if rate == 0 {
            return Err(corrupt("STREAMINFO has a zero sample rate"));
        }
        let stream = StreamInfo {
            index: 0,
            codec: CodecId::Flac,
            time_base: Rational::new(1, rate as i64),
            duration: (total > 0).then_some(total),
            bits: Some(bits),
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Audio { sample_rate: rate, channels },
        };
        let frames_start = r.stream_position()?;
        Ok(Self { r, streams: vec![stream], buf: Vec::new(), start: 0, eof: false, pts: 0, frames_start, held: None })
    }

    /// Drop consumed bytes. Only called between packets: during a frame scan the buffer may
    /// grow but must never shift, because the scan holds indices into it.
    fn compact(&mut self) {
        if self.start > COMPACT_AT {
            self.buf.drain(..self.start);
            self.start = 0;
        }
    }

    fn fill(&mut self) -> Result<()> {
        let old = self.buf.len();
        self.buf.resize(old + READ_CHUNK, 0);
        let n = self.r.read(&mut self.buf[old..])?;
        self.buf.truncate(old + n);
        if n == 0 {
            self.eof = true;
        }
        Ok(())
    }
}

impl<R: Read + Seek + Send> Demuxer for FlacDemuxer<R> {
    fn format_name(&self) -> &'static str {
        "flac"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    /// Frames are concatenated with no length field, so the end of a frame is the first later
    /// position that both starts a valid frame header and makes the CRC-16 of everything
    /// before it come out to zero (the CRC is appended to each frame).
    fn read_packet(&mut self) -> Result<Option<Packet>> {
        if let Some(p) = self.held.take() {
            return Ok(Some(p));
        }
        self.read_frame()
    }

    /// FLAC frames carry no index, so seeking restarts at the first frame and walks forward
    /// (reading frame boundaries only, no decoding) to the one containing the wanted time.
    fn seek(&mut self, seconds: f64) -> Result<f64> {
        let rate = self.streams[0].time_base.invert().as_f64();
        let want = (seconds.max(0.0) * rate) as i64;
        self.r.seek(SeekFrom::Start(self.frames_start))?;
        self.buf.clear();
        (self.start, self.eof, self.pts, self.held) = (0, false, 0, None);
        loop {
            match self.read_frame()? {
                Some(p) if p.pts + p.duration <= want => continue,
                Some(p) => {
                    let at = p.pts as f64 / rate;
                    self.held = Some(p);
                    return Ok(at);
                }
                None => return Ok(self.pts as f64 / rate),
            }
        }
    }
}

impl<R: Read + Seek + Send> FlacDemuxer<R> {
    fn read_frame(&mut self) -> Result<Option<Packet>> {
        self.compact();
        while self.buf.len() - self.start < 2 * MAX_HEADER_LEN && !self.eof {
            self.fill()?;
        }
        if self.start >= self.buf.len() {
            return Ok(None);
        }
        let header = loop {
            match parse_header(&self.buf[self.start..]) {
                Ok(h) => break h,
                Err(ironav_codec::flac::HeaderError::Incomplete) if !self.eof => self.fill()?,
                Err(ironav_codec::flac::HeaderError::Incomplete) => return Err(corrupt("truncated frame header at end of file")),
                Err(ironav_codec::flac::HeaderError::Invalid(m)) => return Err(corrupt(&format!("lost frame sync ({m})"))),
            }
        };

        let (start, min_end) = (self.start, self.start + header.len + 2);
        let mut p = start;
        let mut crc = 0u16;
        let end = loop {
            if p + 2 * MAX_HEADER_LEN > self.buf.len() && !self.eof {
                self.fill()?;
                continue;
            }
            if p >= self.buf.len() {
                if crc == 0 && p >= min_end {
                    break p;
                }
                return Err(corrupt("last frame is truncated or corrupt (CRC-16 mismatch)"));
            }
            if p >= min_end && crc == 0 && self.buf[p] == 0xFF && parse_header(&self.buf[p..]).is_ok() {
                break p;
            }
            crc = crc16_update(crc, self.buf[p]);
            p += 1;
        };

        let data = self.buf[start..end].to_vec();
        self.start = end;
        let pts = self.pts;
        self.pts += header.block_size as i64;
        Ok(Some(Packet { stream: 0, pts, duration: header.block_size as i64, data }))
    }
}

/// Running totals over the FLAC frames written to a container, enough to fill STREAMINFO.
#[derive(Default)]
pub(crate) struct FlacStats {
    min_frame: u32,
    max_frame: u32,
    block_sizes: Vec<i64>,
    total: u64,
}

impl FlacStats {
    pub(crate) fn new() -> Self {
        Self { min_frame: u32::MAX, ..Self::default() }
    }

    pub(crate) fn record(&mut self, p: &Packet) {
        self.record_frame(p.data.len(), p.duration);
    }

    /// One frame of `bytes` bytes and `samples` samples.
    pub(crate) fn record_frame(&mut self, bytes: usize, samples: i64) {
        self.min_frame = self.min_frame.min(bytes as u32);
        self.max_frame = self.max_frame.max(bytes as u32);
        self.block_sizes.push(samples);
        self.total += samples as u64;
    }

    /// The 34-byte STREAMINFO body. The MD5 of the audio is left zero, which means "not computed".
    pub(crate) fn streaminfo(&self, rate: u32, channels: u16, bits: u8) -> [u8; 34] {
        // The last block may be shorter; STREAMINFO's minimum excludes it.
        let body = if self.block_sizes.len() > 1 { &self.block_sizes[..self.block_sizes.len() - 1] } else { &self.block_sizes[..] };
        let min_block = body.iter().copied().min().unwrap_or(0).max(16) as u16;
        let max_block = self.block_sizes.iter().copied().max().unwrap_or(0).max(16) as u16;
        let mut info = [0u8; 34];
        info[0..2].copy_from_slice(&min_block.to_be_bytes());
        info[2..4].copy_from_slice(&max_block.to_be_bytes());
        let (minf, maxf) = if self.block_sizes.is_empty() { (0, 0) } else { (self.min_frame, self.max_frame) };
        info[4..7].copy_from_slice(&minf.to_be_bytes()[1..]);
        info[7..10].copy_from_slice(&maxf.to_be_bytes()[1..]);
        let packed: u64 = ((rate as u64) << 44) | (((channels - 1) as u64) << 41) | (((bits - 1) as u64) << 36) | (self.total & 0xF_FFFF_FFFF);
        info[10..18].copy_from_slice(&packed.to_be_bytes());
        info
    }
}

/// Sample rate, channels and bit depth of a FLAC stream, with the checks FLAC itself imposes.
pub(crate) fn flac_audio_params(s: &StreamInfo) -> Result<(u32, u16, u8)> {
    let (StreamParams::Audio { sample_rate, channels }, CodecId::Flac) = (&s.params, s.codec) else {
        return Err(Error::new(ErrorCode::UnsupportedCodec, format!("expected FLAC audio, got {}", s.codec.name())).alternatives(["flac"]));
    };
    if *sample_rate > 1_048_575 {
        return Err(Error::invalid_arg("FLAC STREAMINFO limits the sample rate to 1048575 Hz"));
    }
    Ok((*sample_rate, *channels, s.bits.unwrap_or(16)))
}

pub struct FlacMuxer {
    out: Box<dyn WriteSeek>,
    info: Option<(u32, u16, u8)>,
    stats: FlacStats,
}

impl FlacMuxer {
    pub fn new(out: Box<dyn WriteSeek>) -> Self {
        Self { out, info: None, stats: FlacStats::new() }
    }
}

impl Muxer for FlacMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [s] = streams else {
            return Err(Error::invalid_arg("flac holds exactly one audio stream"));
        };
        if s.codec != CodecId::Flac {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("flac can only hold FLAC audio, not {}", s.codec.name())).alternatives(["flac"]));
        }
        self.info = Some(flac_audio_params(s)?);
        self.out.write_all(b"fLaC")?;
        self.out.write_all(&[0x80, 0, 0, 34])?; // last metadata block, STREAMINFO, 34 bytes
        self.out.write_all(&[0u8; 34])?; // patched in finish()
        Ok(())
    }

    fn write_packet(&mut self, p: &Packet) -> Result<()> {
        self.out.write_all(&p.data)?;
        self.stats.record(p);
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        let (rate, channels, bits) = self.info.ok_or_else(|| Error::new(ErrorCode::Internal, "muxer finished before write_header"))?;
        let info = self.stats.streaminfo(rate, channels, bits);
        self.out.seek(SeekFrom::Start(8))?;
        self.out.write_all(&info)?;
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_codec::flac::FlacEncoder;
    use ironav_core::{AudioFrame, Encoder, Frame};
    use std::io::Cursor;

    /// Regression: a long stream forces many buffer refills and compactions; every packet must
    /// come back byte-identical to what the encoder produced.
    #[test]
    fn demuxes_long_streams_across_buffer_refills() {
        let (rate, ch) = (44_100u32, 2u16);
        let mut x = 12345u64;
        let samples: Vec<f64> = (0..600_000 * ch as usize)
            .map(|i| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((i as f64 * 0.01).sin() * 0.5) + ((x >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 0.3
            })
            .collect();
        let mut enc = FlacEncoder::new(rate, ch, Some(16)).unwrap();
        let mut pkts = enc.encode(&Frame::Audio(AudioFrame { sample_rate: rate, channels: ch, pts: 0, samples })).unwrap();
        pkts.extend(enc.flush().unwrap());
        assert!(pkts.iter().map(|p| p.data.len()).sum::<usize>() > 4 * COMPACT_AT, "stream must outgrow the compaction threshold");

        let mut file = b"fLaC".to_vec();
        file.extend_from_slice(&[0x80, 0, 0, 34]);
        let mut info = [0u8; 34];
        info[10..18].copy_from_slice(&(((rate as u64) << 44) | (1u64 << 41) | (15u64 << 36) | 600_000).to_be_bytes());
        file.extend_from_slice(&info);
        for p in &pkts {
            file.extend_from_slice(&p.data);
        }

        let mut d = FlacDemuxer::open(Cursor::new(file)).unwrap();
        let mut n = 0;
        while let Some(p) = d.read_packet().unwrap() {
            assert_eq!(p.data, pkts[n].data, "frame {n}");
            assert_eq!((p.pts, p.duration), (pkts[n].pts, pkts[n].duration), "frame {n} timing");
            n += 1;
        }
        assert_eq!(n, pkts.len());
    }
}
