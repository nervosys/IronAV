//! Raw AC-3 (`.ac3`): a run of syncframes, each starting with the sync word 0x0B77, optionally behind
//! an ID3v2 tag.
//!
//! The file is walked once, frame to frame, with the codec's frame reader. The frames go to the MP4
//! reader's machinery (`Mp4Demuxer::from_parts`) as whole AC-3 frames, which is what the AC-3 decoder
//! takes. A frame counts only when the header and frame length are consistent; bytes that do not
//! start a frame are skipped. E-AC-3 streams (bit stream id above 10) are refused with an error.
//!
//! Not handled: streams that change sample rate or channel count partway (the frames after the
//! change are dropped).
//!
//! Written: the packets are the syncframes, one after another, with nothing between them, which is
//! what FFmpeg's raw AC-3 muxer writes.

use crate::mp4demux::{Config, Mp4Demuxer, Sample, TrackInfo};
use crate::WriteSeek;
use ironav_codec::ac3::frame_info;
use ironav_core::{CodecId, Error, ErrorCode, Muxer, Packet, Rational, Result, StreamInfo, StreamParams};
use std::io::{Read, Seek, SeekFrom, Write};

/// The largest file whose frames are indexed.
const MAX_SCAN: u64 = 2 << 30;

/// Samples per syncframe (6 blocks of 256).
const FRAME_SAMPLES: u32 = 1536;

/// Whether `magic` starts with an AC-3 sync word. E-AC-3 shares it; `open` refuses those.
pub fn looks_like_ac3(magic: &[u8]) -> bool {
    magic.len() >= 2 && magic[0] == 0x0B && magic[1] == 0x77
}

/// Open a raw AC-3 file for reading. The file is read once into memory to find its frames.
pub fn open<R: Read + Seek + Send>(mut r: R) -> Result<Mp4Demuxer<R>> {
    let len = r.seek(SeekFrom::End(0))?;
    if len > MAX_SCAN {
        return Err(Error::new(ErrorCode::UnsupportedFormat, "AC-3: files over 2 GiB are not supported"));
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

    let mut first: Option<(u32, u16)> = None;
    let mut samples = Vec::new();
    let mut total = 0i64;
    while pos < len {
        let info = match frame_info(&file[pos..]) {
            Ok(info) => info,
            Err(e) if e.code == ErrorCode::UnsupportedCodec => return Err(e),
            Err(_) => {
                pos += 1;
                continue;
            }
        };
        let start = pos + info.offset;
        let end = start + info.frame_size;
        if end > len {
            break; // the last frame must be whole
        }
        let (rate, channels) = *first.get_or_insert((info.sample_rate, info.channels));
        if (rate, channels) == (info.sample_rate, info.channels) {
            samples.push(Sample { stream: 0, sync: true, offset: start as u64, size: info.frame_size as u32, dts: total, cts_offset: 0, duration: FRAME_SAMPLES });
            total += FRAME_SAMPLES as i64;
        }
        pos = end;
    }
    let Some((rate, channels)) = first else {
        return Err(Error::new(ErrorCode::InvalidInput, "AC-3: no syncframes found"));
    };
    let stream = StreamInfo {
        index: 0,
        codec: CodecId::Ac3,
        time_base: Rational::new(1, rate as i64),
        duration: Some(total),
        bits: None,
        extradata: Vec::new(),
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: rate, channels },
    };
    let track = TrackInfo { stream, config: Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 }, convert: false, swap: 0, unwrap: Vec::new() };
    Ok(Mp4Demuxer::from_parts(r, "ac3", vec![track], samples))
}

/// Writes a raw AC-3 file: each packet's syncframe bytes, one frame after another.
pub struct Ac3Muxer {
    out: Box<dyn WriteSeek>,
    ready: bool,
}

impl Ac3Muxer {
    pub fn new(out: Box<dyn WriteSeek>) -> Self {
        Self { out, ready: false }
    }
}

impl Muxer for Ac3Muxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [s] = streams else {
            return Err(Error::invalid_arg("a raw AC-3 file holds exactly one audio stream"));
        };
        if s.codec != CodecId::Ac3 {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("a raw AC-3 file can only hold AC-3, not {}", s.codec.name())).alternatives(["ac3"]));
        }
        self.ready = true;
        Ok(())
    }

    fn write_packet(&mut self, p: &Packet) -> Result<()> {
        if !self.ready {
            return Err(Error::new(ErrorCode::Internal, "packet before the header"));
        }
        let whole = frame_info(&p.data).is_ok_and(|i| i.offset == 0 && i.frame_size == p.data.len());
        if !whole {
            return Err(Error::invalid_input("a raw AC-3 packet must be one AC-3 syncframe"));
        }
        self.out.write_all(&p.data)?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}
