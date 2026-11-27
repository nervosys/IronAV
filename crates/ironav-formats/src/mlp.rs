//! Raw TrueHD (`.thd`, `.truehd`) and MLP (`.mlp`): a run of access units, each starting with a
//! 16-bit word of which the low 12 bits are its length in 16-bit words (the top four are a parity
//! nibble), then a 16-bit timestamp. An access unit with a major sync (the stream's parameters,
//! after the 4-byte header) starts the stream, and then every 24 or so units; the others need the
//! one before them.
//!
//! The file is walked once, unit to unit, by the length field, as FFmpeg's parser does. A unit
//! without a major sync counts when its parity nibble checks (against the substream count of the
//! last major sync); where one does not, bytes are skipped until the next major sync. Each unit
//! becomes a packet, and a major sync marks a sync sample. They go to the MP4 reader's machinery
//! (`Mp4Demuxer::from_parts`) as whole units, which is what the decoder takes. Only the substreams
//! the decoder uses are decoded (see `ironav_codec::mlp`).
//!
//! Not handled: streams that change sample rate or channel count partway (the units after the
//! change are dropped); the AC-3 core that Blu-ray muxes beside TrueHD is in its own PES packets
//! in a transport stream and never in a raw file.

use crate::mp4demux::{Config, Mp4Demuxer, Sample, TrackInfo};
use ironav_codec::mlp::{access_unit_length, access_unit_parity_ok, major_sync_at_start, major_sync_kind};
use ironav_core::{CodecId, Error, ErrorCode, Rational, Result, StreamInfo, StreamParams};
use std::io::{Read, Seek, SeekFrom};

/// The largest file whose units are indexed.
const MAX_SCAN: u64 = 2 << 30;

/// Whether `magic` starts with an access unit that has a major sync and a sound header.
pub fn looks_like_mlp(magic: &[u8]) -> bool {
    major_sync_kind(magic).is_some() && major_sync_at_start(magic).is_some()
}

/// Open a raw TrueHD or MLP file for reading. The file is read once into memory to find its units.
pub fn open<R: Read + Seek + Send>(mut r: R) -> Result<Mp4Demuxer<R>> {
    let len = r.seek(SeekFrom::End(0))?;
    if len > MAX_SCAN {
        return Err(Error::new(ErrorCode::UnsupportedFormat, "TrueHD/MLP: files over 2 GiB are not supported"));
    }
    r.seek(SeekFrom::Start(0))?;
    let mut file = Vec::with_capacity(len as usize);
    r.by_ref().take(len).read_to_end(&mut file)?;
    let len = file.len();

    let mut pos = 0usize;
    // the substream count, samples per unit and the parameters of the last major sync; and the
    // first major sync's, which the stream keeps
    let mut current: Option<(usize, usize, (u32, u16))> = None;
    let mut first: Option<(bool, (u32, u16))> = None;
    let mut samples = Vec::new();
    let mut total = 0i64;
    while pos < len {
        let rest = &file[pos..];
        let Some(n) = access_unit_length(rest) else {
            current = None;
            pos += 1;
            continue;
        };
        let sync = major_sync_kind(rest).is_some();
        if sync {
            // a major sync that is cut short by the end of the file ends the walk
            let Some(mh) = major_sync_at_start(rest).filter(|_| n <= rest.len()) else {
                if n > rest.len() {
                    break;
                }
                current = None;
                pos += 1;
                continue;
            };
            let params = (mh.sample_rate, mh.channels() as u16);
            current = Some((mh.num_substreams, mh.access_unit_size, params));
            first.get_or_insert((mh.truehd, params));
        } else if !current.is_some_and(|(subs, _, _)| n <= rest.len() && access_unit_parity_ok(&rest[..n], subs)) {
            // lost step: only a major sync starts again
            current = None;
            pos += 1;
            continue;
        }
        let (_, size, params) = current.expect("a major sync was seen");
        if n > rest.len() {
            break; // the last unit must be whole
        }
        if Some(params) == first.map(|f| f.1) {
            samples.push(Sample { stream: 0, sync, offset: pos as u64, size: n as u32, dts: total, cts_offset: 0, duration: size as u32 });
            total += size as i64;
        }
        pos += n;
    }
    let Some((truehd, (rate, channels))) = first else {
        return Err(Error::new(ErrorCode::InvalidInput, "TrueHD/MLP: no access unit with a major sync found"));
    };
    let stream = StreamInfo {
        index: 0,
        codec: if truehd { CodecId::TrueHd } else { CodecId::Mlp },
        time_base: Rational::new(1, rate as i64),
        duration: Some(total),
        bits: None,
        extradata: Vec::new(),
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: rate, channels },
    };
    let track = TrackInfo { stream, config: Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 }, convert: false, swap: 0, unwrap: Vec::new(), meta: Default::default() };
    Ok(Mp4Demuxer::from_parts(r, if truehd { "truehd" } else { "mlp" }, vec![track], samples))
}
