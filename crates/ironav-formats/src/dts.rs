//! Raw DTS (`.dts`): a run of core frames, each behind the sync word 0x7FFE8001 (or its 16-bit
//! little-endian or 14-bit forms), with the DTS-HD extension substream that follows a core frame
//! (sync word 0x64582025) kept in the same packet. Also the DTS payload of WAV files (format tag
//! 0x2001, or 16-bit stereo PCM whose data is DTS, as DTS CDs are ripped).
//!
//! The file is walked once, frame to frame, with the codec's frame reader. The frames go to the MP4
//! reader's machinery (`Mp4Demuxer::from_parts`) as whole frames, which is what the DTS decoder
//! takes. A frame counts only when its header is sound and the frame is whole; bytes that do not
//! start a frame are skipped. Only the core is decoded (see `ironav_codec::dts`).
//!
//! Not handled: streams that change sample rate or channel count partway (the frames after the
//! change are dropped); files in the `.dtshd` wrapper (the `DTSHDHDR` chunks).

use crate::mp4demux::{Config, Mp4Demuxer, Sample, TrackInfo};
use ironav_codec::dts::frame_info_at_start;
use ironav_core::{CodecId, Error, ErrorCode, Rational, Result, StreamInfo, StreamParams};
use std::io::{Read, Seek, SeekFrom};

/// The largest file whose frames are indexed.
const MAX_SCAN: u64 = 2 << 30;

/// Whether `magic` starts with a DTS core frame (any byte order) or an extension substream.
pub fn looks_like_dts(magic: &[u8]) -> bool {
    magic.len() >= 4 && matches!(u32::from_be_bytes([magic[0], magic[1], magic[2], magic[3]]), 0x7FFE8001 | 0xFE7F0180 | 0x1FFFE800 | 0xFF1F00E8 | 0x64582025) && frame_info_at_start(magic).is_ok()
}

/// Open a raw DTS file for reading. The file is read once into memory to find its frames.
pub fn open<R: Read + Seek + Send>(mut r: R) -> Result<Mp4Demuxer<R>> {
    let len = r.seek(SeekFrom::End(0))?;
    open_range(r, 0, len, "dts")
}

/// Open the DTS data in `len` bytes from `start`, for the format `name` to report.
pub fn open_range<R: Read + Seek + Send>(mut r: R, start: u64, len: u64, name: &'static str) -> Result<Mp4Demuxer<R>> {
    if len > MAX_SCAN {
        return Err(Error::new(ErrorCode::UnsupportedFormat, "DTS: files over 2 GiB are not supported"));
    }
    r.seek(SeekFrom::Start(start))?;
    let mut file = Vec::with_capacity(len as usize);
    r.by_ref().take(len).read_to_end(&mut file)?;
    let len = file.len();

    let mut pos = 0usize;
    let mut first: Option<(u32, u16)> = None;
    let mut samples = Vec::new();
    let mut total = 0i64;
    while pos < len {
        let Ok(info) = frame_info_at_start(&file[pos..]) else {
            pos += 1;
            continue;
        };
        let end = pos + info.frame_size;
        if end > len {
            break; // the last frame must be whole
        }
        let (rate, channels) = *first.get_or_insert((info.sample_rate, info.channels));
        if (rate, channels) == (info.sample_rate, info.channels) {
            samples.push(Sample { stream: 0, sync: true, offset: start + pos as u64, size: info.frame_size as u32, dts: total, cts_offset: 0, duration: info.samples as u32 });
            total += info.samples as i64;
        }
        pos = end;
    }
    let Some((rate, channels)) = first else {
        return Err(Error::new(ErrorCode::InvalidInput, "DTS: no core frames found").hint("a DTS stream with only an extension substream (DTS Express) has no core to decode"));
    };
    let stream = StreamInfo {
        index: 0,
        codec: CodecId::Dts,
        time_base: Rational::new(1, rate as i64),
        duration: Some(total),
        bits: None,
        extradata: Vec::new(),
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: rate, channels },
    };
    let track = TrackInfo { stream, config: Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 }, convert: false, swap: 0, unwrap: Vec::new(), meta: Default::default() };
    Ok(Mp4Demuxer::from_parts(r, name, vec![track], samples))
}
