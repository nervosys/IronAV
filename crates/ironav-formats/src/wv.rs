//! Raw WavPack (`.wv`): a run of `wvpk` blocks, each with a 32-byte header that gives its length, the
//! stream's total sample count, the index of its first sample and its sample count. The blocks of
//! the channels of one frame follow each other, the first flagged initial and the last final; a
//! frame is a packet, as FFmpeg's `wv` demuxer makes it.
//!
//! The file is walked once, header to header (the audio is not read), so the packets have their sample
//! positions and a seek lands on the frame that holds the time asked: WavPack blocks are decoded
//! without the blocks before them. A damaged stretch is skipped by looking for the next valid block
//! header. Blocks with no samples (metadata only) are left out, and so is a frame cut short by the end
//! of the file or by a missing final block. An APEv2 tag at the end of the file, with or without an
//! ID3v1 tag after it, ends the audio, and is read by [`metadata`]; ID3v1 is read when there is no APEv2.
//!
//! The first frame gives the stream's parameters: the rate from the flags (or the custom rate
//! sub-block), the channels from its blocks, the depth from the flags. The duration is where the last
//! frame ends. DSD files are refused.

use crate::mp4demux::{Config, Mp4Demuxer, Sample, TrackInfo};
use ironav_codec::wavpack::{stream_hints, BlockHeader, HEADER_SIZE};
use ironav_core::{CodecId, Error, ErrorCode, Rational, Result, StreamInfo, StreamParams};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The longest text kept of a tag value.
const MAX_TEXT: usize = 512;
/// The most tag items read.
const MAX_ITEMS: usize = 256;

/// Whether `magic` starts with a plausible WavPack block header.
pub fn looks_like_wv(magic: &[u8]) -> bool {
    BlockHeader::parse(magic).is_some()
}

fn bad(msg: &str) -> Error {
    Error::new(ErrorCode::InvalidInput, format!("WavPack: {msg}"))
}

/// The tag items of the file as (name, value) pairs, names in lower case: APEv2 (title, artist,
/// album, ...) or, without it, ID3v1 (title, artist, album, year, comment). It is text from the file,
/// so untrusted: control characters are removed and each value is cut to 512 characters.
pub fn metadata(path: &Path) -> Result<Vec<(String, String)>> {
    Ok(read_tags(&mut std::fs::File::open(path)?)?.1)
}

/// Removes control characters and cuts to the limit.
fn clean(d: &[u8]) -> String {
    String::from_utf8_lossy(d).chars().filter(|c| !c.is_control()).take(MAX_TEXT).collect()
}

fn read_at<R: Read + Seek>(r: &mut R, at: u64, n: usize) -> std::io::Result<Vec<u8>> {
    r.seek(SeekFrom::Start(at))?;
    let mut buf = Vec::new();
    r.take(n as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Where the audio ends (before any tags) and the tags' items.
fn read_tags<R: Read + Seek>(r: &mut R) -> Result<(u64, Vec<(String, String)>)> {
    let len = r.seek(SeekFrom::End(0))?;
    let mut end = len;
    let mut id3: Option<Vec<u8>> = None;
    if len >= 128 {
        let t = read_at(r, len - 128, 128)?;
        if t.len() == 128 && &t[..3] == b"TAG" {
            end = len - 128;
            id3 = Some(t);
        }
    }
    let mut items = Vec::new();
    if end >= 32 {
        let f = read_at(r, end - 32, 32)?;
        if f.len() == 32 && &f[..8] == b"APETAGEX" {
            let le = |at: usize| u32::from_le_bytes([f[at], f[at + 1], f[at + 2], f[at + 3]]);
            let (size, count, flags) = (le(12) as u64, le(16) as usize, le(20));
            // the size counts the items and this footer; a header (flag bit 31) is not in it
            let header = if flags & 0x8000_0000 != 0 { 32 } else { 0 };
            if size >= 32 && size + header <= end {
                let body = read_at(r, end - size, (size - 32) as usize)?;
                end -= size + header;
                let mut p = 0usize;
                for _ in 0..count.min(MAX_ITEMS) {
                    let Some(h) = body.get(p..p + 8) else { break };
                    let vlen = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
                    let iflags = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
                    p += 8;
                    let Some(k) = body[p..].iter().position(|&b| b == 0) else { break };
                    let key = clean(&body[p..p + k]).to_ascii_lowercase();
                    p += k + 1;
                    let Some(v) = body.get(p..p.saturating_add(vlen)) else { break };
                    p += vlen;
                    // text items only (bits 1 and 2 are the item type; 0 is UTF-8 text)
                    if iflags & 6 == 0 && !key.is_empty() {
                        let text: Vec<String> = v.split(|&b| b == 0).map(clean).filter(|s| !s.is_empty()).collect();
                        if !text.is_empty() {
                            items.push((key, text.join("; ")));
                        }
                    }
                }
            }
        }
    }
    if items.is_empty() {
        if let Some(t) = id3 {
            let field = |a: usize, b: usize| clean(&t[a..b].iter().copied().take_while(|&c| c != 0).collect::<Vec<u8>>()).trim().to_string();
            for (name, text) in [("title", field(3, 33)), ("artist", field(33, 63)), ("album", field(63, 93)), ("year", field(93, 97)), ("comment", field(97, 127))] {
                if !text.is_empty() {
                    items.push((name.to_string(), text));
                }
            }
        }
    }
    Ok((end, items))
}

/// The first block header at or after `from` (and before `end`), by searching for the marker.
fn find_header<R: Read + Seek>(r: &mut R, from: u64, end: u64) -> std::io::Result<Option<(u64, BlockHeader)>> {
    const CHUNK: usize = 1 << 16;
    let mut at = from;
    while at + HEADER_SIZE as u64 <= end {
        let want = (CHUNK as u64 + HEADER_SIZE as u64).min(end - at) as usize;
        let buf = read_at(r, at, want)?;
        let mut i = 0;
        while i < CHUNK && i + HEADER_SIZE <= buf.len() {
            if &buf[i..i + 4] == b"wvpk" {
                if let Some(h) = BlockHeader::parse(&buf[i..]) {
                    return Ok(Some((at + i as u64, h)));
                }
            }
            i += 1;
        }
        at += CHUNK as u64;
    }
    Ok(None)
}

/// A frame being gathered from its blocks.
struct Gathering {
    start: u64,
    first: BlockHeader,
    bytes: u64,
    channels: u32,
    /// the channel count the blocks say, and the custom rate
    declared: u32,
    rate: u32,
}

/// Open a raw WavPack file for reading.
pub fn open<R: Read + Seek + Send>(mut r: R) -> Result<Mp4Demuxer<R>> {
    let (end, _) = read_tags(&mut r)?;
    let mut samples: Vec<Sample> = Vec::new();
    // the stream's parameters, from the first complete frame: its first block, channels and custom rate
    let mut first: Option<(BlockHeader, u32, u32)> = None;
    let mut cur: Option<Gathering> = None;
    let mut base: Option<u32> = None;
    let mut pos = 0u64;
    while pos + HEADER_SIZE as u64 <= end {
        let head = read_at(&mut r, pos, HEADER_SIZE)?;
        let h = match BlockHeader::parse(&head) {
            Some(h) if pos + HEADER_SIZE as u64 + h.body_len() as u64 <= end => h,
            // damaged, or a last block cut short by the end of the file: look for the next one
            _ => match find_header(&mut r, pos + 1, end)? {
                Some((at, _)) => {
                    cur = None;
                    pos = at;
                    continue;
                }
                None => break,
            },
        };
        let total = HEADER_SIZE as u64 + h.body_len() as u64;
        if h.samples == 0 {
            pos += total;
            continue;
        }
        if h.dsd() {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "WavPack: DSD files are not supported"));
        }
        if h.initial() {
            cur = Some(Gathering { start: pos, first: h, bytes: 0, channels: 0, declared: 0, rate: 0 });
        }
        if let Some(g) = &mut cur {
            g.bytes += total;
            g.channels += if h.mono() { 1 } else { 2 };
            // the first frame's blocks tell the custom rate and the channel count
            if first.is_none() {
                let body = read_at(&mut r, pos + HEADER_SIZE as u64, h.body_len())?;
                let hints = stream_hints(&body);
                g.rate = g.rate.max(hints.sample_rate);
                g.declared = g.declared.max(hints.channels);
            }
            if h.is_final() {
                let g = cur.take().expect("a frame is being gathered");
                if first.is_none() {
                    if g.declared != 0 && g.declared != g.channels {
                        return Err(bad("the channel information does not match the blocks"));
                    }
                    first = Some((g.first, g.channels, g.rate));
                }
                let b0 = *base.get_or_insert(g.first.block_index);
                if let Ok(size) = u32::try_from(g.bytes) {
                    samples.push(Sample { stream: 0, sync: true, offset: g.start, size, dts: g.first.block_index.wrapping_sub(b0) as i64, cts_offset: 0, duration: g.first.samples });
                }
            }
        }
        pos += total;
    }
    let Some((fb, channels, custom)) = first else {
        return Err(bad("no complete frame of blocks found"));
    };
    let rate = if fb.rate() != 0 { fb.rate() } else { custom };
    if rate == 0 {
        return Err(bad("the sample rate is missing"));
    }
    let duration = samples.last().map_or(0, |s| s.dts + s.duration as i64);
    let float = fb.flags & 0x80 != 0;
    let stream = StreamInfo {
        index: 0,
        codec: CodecId::WavPack,
        time_base: Rational::new(1, rate as i64),
        duration: Some(duration),
        bits: Some(if float { 32 } else { fb.bits() }),
        extradata: fb.version.to_le_bytes().to_vec(),
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: rate, channels: channels.min(u16::MAX as u32) as u16 },
    };
    let track = TrackInfo { stream, config: Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 }, convert: false, swap: 0, unwrap: Vec::new(), meta: Default::default() };
    Ok(Mp4Demuxer::from_parts(r, "wavpack", vec![track], samples))
}
