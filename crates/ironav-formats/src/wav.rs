use crate::WriteSeek;
use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Muxer, Packet, Rational, Result, SampleFormat, StreamInfo, StreamParams};
use std::io::{Read, Seek, SeekFrom};

/// Audio frames per packet. Large enough to amortise per-packet overhead, small enough
/// to keep end-to-end latency in the single-digit-millisecond range at 48 kHz.
const FRAMES_PER_PACKET: usize = 4096;

pub struct WavDemuxer<R: Read + Seek + Send> {
    r: R,
    streams: Vec<StreamInfo>,
    block_align: usize,
    remaining: u64,
    pos_frames: i64,
    data_start: u64,
    total_frames: u64,
}

impl<R: Read + Seek + Send> WavDemuxer<R> {
    pub fn open(mut r: R) -> Result<Self> {
        let total = r.seek(SeekFrom::End(0))?;
        r.seek(SeekFrom::Start(12))?;
        let mut fmt: Option<(u16, u16, u32, u16, u16)> = None;
        loop {
            let mut h = [0u8; 8];
            if r.read_exact(&mut h).is_err() {
                return Err(Error::invalid_input("WAV file has no 'data' chunk"));
            }
            let size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as u64;
            match &h[..4] {
                b"fmt " => {
                    // the format fields are a few dozen bytes; a longer chunk is not a format header
                    if size > 1 << 16 {
                        return Err(Error::invalid_input("WAV fmt chunk is implausibly large"));
                    }
                    let mut b = vec![0u8; size as usize];
                    r.read_exact(&mut b)?;
                    fmt = Some(fmt_fields(&b)?);
                    if size % 2 == 1 {
                        r.seek(SeekFrom::Current(1))?;
                    }
                }
                b"data" => {
                    let (tag, channels, rate, align, bits) =
                        fmt.ok_or_else(|| Error::invalid_input("WAV 'data' chunk precedes 'fmt ' chunk"))?;
                    let codec = pcm_codec(tag, bits, channels, rate, align)?;
                    let start = r.stream_position()?;
                    // Streaming writers leave 0 / 0xFFFFFFFF here; trust the file length instead.
                    let avail = total.saturating_sub(start);
                    let len = if size == 0 || size > avail { avail } else { size };
                    let frames = (len / align as u64) as i64;
                    let stream = StreamInfo {
                        index: 0,
                        codec,
                        time_base: Rational::new(1, rate as i64),
                        duration: Some(frames),
                        bits: Some(bits as u8),
                        extradata: Vec::new(),
                        lead_in: 0,
                        params: StreamParams::Audio { sample_rate: rate, channels },
                    };
                    return Ok(Self {
                        r,
                        streams: vec![stream],
                        block_align: align as usize,
                        remaining: frames as u64 * align as u64,
                        pos_frames: 0,
                        data_start: start,
                        total_frames: frames as u64,
                    });
                }
                _ => {
                    r.seek(SeekFrom::Current((size + (size & 1)) as i64))?;
                }
            }
        }
    }
}

/// Format tag (the subformat's, for WAVE_FORMAT_EXTENSIBLE), channels, sample rate, block
/// alignment and bits per sample from the body of a 'fmt ' chunk.
pub(crate) fn fmt_fields(b: &[u8]) -> Result<(u16, u16, u32, u16, u16)> {
    if b.len() < 16 {
        return Err(Error::invalid_input("WAV 'fmt ' chunk is too short"));
    }
    let mut tag = u16::from_le_bytes([b[0], b[1]]);
    if tag == 0xFFFE && b.len() >= 26 {
        tag = u16::from_le_bytes([b[24], b[25]]);
    }
    Ok((
        tag,
        u16::from_le_bytes([b[2], b[3]]),
        u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        u16::from_le_bytes([b[12], b[13]]),
        u16::from_le_bytes([b[14], b[15]]),
    ))
}

/// The PCM codec a WAV format describes, after checking it can be read.
pub(crate) fn pcm_codec(tag: u16, bits: u16, channels: u16, rate: u32, align: u16) -> Result<CodecId> {
    let codec = match (tag, bits) {
        (1, 8) => CodecId::PcmU8,
        (1, 16) => CodecId::PcmS16Le,
        (1, 24) => CodecId::PcmS24Le,
        (1, 32) => CodecId::PcmS32Le,
        (3, 32) => CodecId::PcmF32Le,
        (3, 64) => CodecId::PcmF64Le,
        _ => {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("WAV format tag {tag} with {bits} bits per sample is not supported"))
                .hint("supported: 8/16/24/32-bit integer PCM and 32/64-bit float PCM"))
        }
    };
    if channels == 0 || rate == 0 || align == 0 {
        return Err(Error::invalid_input("WAV header has zero channels, sample rate or block alignment"));
    }
    Ok(codec)
}

impl<R: Read + Seek + Send> Demuxer for WavDemuxer<R> {
    fn format_name(&self) -> &'static str {
        "wav"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn read_packet(&mut self) -> Result<Option<Packet>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let want = (FRAMES_PER_PACKET * self.block_align).min(self.remaining as usize);
        let mut data = vec![0u8; want];
        self.r.read_exact(&mut data)?;
        self.remaining -= want as u64;
        let frames = (want / self.block_align) as i64;
        let pts = self.pos_frames;
        self.pos_frames += frames;
        Ok(Some(Packet { stream: 0, pts, duration: frames, data }))
    }

    fn seek(&mut self, seconds: f64) -> Result<f64> {
        let rate = self.streams[0].time_base.invert().as_f64();
        let frame = ((seconds.max(0.0) * rate).round() as u64).min(self.total_frames);
        self.r.seek(SeekFrom::Start(self.data_start + frame * self.block_align as u64))?;
        self.remaining = (self.total_frames - frame) * self.block_align as u64;
        self.pos_frames = frame as i64;
        Ok(frame as f64 / rate)
    }
}

pub struct WavMuxer {
    out: Box<dyn WriteSeek>,
    data_bytes: u64,
}

impl WavMuxer {
    pub fn new(out: Box<dyn WriteSeek>) -> Self {
        Self { out, data_bytes: 0 }
    }
}

impl Muxer for WavMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [s] = streams else {
            return Err(Error::invalid_arg("wav holds exactly one audio stream"));
        };
        let StreamParams::Audio { sample_rate, channels } = s.params else {
            return Err(Error::invalid_arg("wav can only hold audio"));
        };
        let fmt = s.codec.sample_format().ok_or_else(|| {
            Error::new(ErrorCode::UnsupportedCodec, format!("wav cannot hold {}", s.codec.name()))
                .alternatives(SampleFormat::ALL.iter().map(|f| CodecId::pcm_for(*f).name()))
        })?;
        let tag: u16 = if matches!(fmt, SampleFormat::F32 | SampleFormat::F64) { 3 } else { 1 };
        let align = fmt.bytes() as u16 * channels;
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&tag.to_le_bytes());
        h.extend_from_slice(&channels.to_le_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&(sample_rate * align as u32).to_le_bytes());
        h.extend_from_slice(&align.to_le_bytes());
        h.extend_from_slice(&((fmt.bytes() * 8) as u16).to_le_bytes());
        h.extend_from_slice(b"data\0\0\0\0");
        self.out.write_all(&h)?;
        Ok(())
    }
    fn write_packet(&mut self, p: &Packet) -> Result<()> {
        self.out.write_all(&p.data)?;
        self.data_bytes += p.data.len() as u64;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        if self.data_bytes % 2 == 1 {
            self.out.write_all(&[0])?;
        }
        if self.data_bytes > u32::MAX as u64 - 64 {
            return Err(Error::invalid_input("output exceeds the 4 GiB WAV limit"));
        }
        let padded = self.data_bytes + (self.data_bytes & 1);
        self.out.seek(SeekFrom::Start(4))?;
        self.out.write_all(&((36 + padded) as u32).to_le_bytes())?;
        self.out.seek(SeekFrom::Start(40))?;
        self.out.write_all(&(self.data_bytes as u32).to_le_bytes())?;
        self.out.flush()?;
        Ok(())
    }
}
