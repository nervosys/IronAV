//! AVI reader: RIFF AVI, and OpenDML (AVIX continuation RIFFs with `indx` super indexes and `ix##`
//! standard indexes) for files over 1 GB.
//!
//! Like the Matroska reader this finds where each chunk lies and when it plays, and hands the list of
//! samples to the MP4 reader's machinery (`Mp4Demuxer::from_parts`), which reads the packets,
//! interleaves the streams and seeks. Where the chunks come from, in order of preference:
//!
//! 1. the OpenDML indexes, when every stream has a usable one (they cover the AVIX RIFFs, which `idx1`
//!    does not);
//! 2. the `idx1` index (offsets relative to the `movi` list, or absolute in files that wrote them so),
//!    checked against the chunk headers it points at;
//! 3. a walk over the `movi` lists (and the `rec ` lists in them), when there is no index or an index
//!    does not hold up. Key frames are then found from the bitstream.
//!
//! Timestamps: a video stream's ticks are its chunks (`dwScale / dwRate` each; a zero-length chunk is a
//! dropped frame, shown again as the one before it). Audio is counted in the stream's own units (bytes
//! over `dwSampleSize`, or chunks when it is zero, which is how VBR MP3 is) and put on a clock of
//! sample-rate ticks.
//!
//! DivX packed bitstream (marked by the user data string `DivX5xxbNNNp`) puts a P and a B picture in
//! one chunk, which the MPEG-4 decoder reads as it is; the tiny not-coded chunks that pad the stream
//! out to its frame count are dropped, and the frame before them lasts through their slots.
//!
//! Streams of a kind IronAV does not decode (other fourccs and wFormatTags, text, MIDI) are not played;
//! [`Demuxer::unsupported_streams`] says what they are, and a file with no stream it can play is refused.

use crate::mp4demux::{gcd, parse_avcc, Config, Mp4Demuxer, Sample, TrackInfo};
use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Packet, Rational, Result, StreamInfo, StreamParams};
use std::io::{BufReader, Read, Seek, SeekFrom};

/// The Header list is read whole; a longer one is not a header.
const MAX_HDRL: u64 = 16 << 20;
/// A chunk this long is not a picture or a block of sound: the index (or the file) is damaged.
const MAX_CHUNK: u64 = 256 << 20;
/// The most a stream format (`strf`) or an index chunk read whole may take.
const MAX_FORMAT: usize = 1 << 20;
/// Not-coded VOPs (DivX packed bitstream) are this long or shorter.
const NVOP_MAX: u32 = 8;

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::invalid_input(format!("AVI: {msg}")).hint("the file may be truncated or damaged; a file that is still being written may play once it is finished")
}

/// Whether `magic` starts an AVI file (`RIFF`, a size, `AVI `).
pub fn looks_like_avi(magic: &[u8]) -> bool {
    magic.len() >= 12 && &magic[..4] == b"RIFF" && &magic[8..12] == b"AVI "
}

fn le16(d: &[u8], at: usize) -> u16 {
    d.get(at..at + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
}
fn le32(d: &[u8], at: usize) -> u32 {
    d.get(at..at + 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}
fn le64(d: &[u8], at: usize) -> u64 {
    d.get(at..at + 8).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()))
}

/// The chunks of an in-memory list: each one's id, its body (cut off where the data ends) and, for a
/// `LIST`, its type (the body then follows it).
fn children(d: &[u8]) -> impl Iterator<Item = ([u8; 4], Option<[u8; 4]>, &[u8])> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        let h = d.get(at..at + 8)?;
        let id: [u8; 4] = h[..4].try_into().unwrap();
        let size = le32(h, 4) as usize;
        let start = at + 8;
        let end = start.saturating_add(size).min(d.len());
        at = start.saturating_add(size).saturating_add(size & 1);
        let body = &d[start..end];
        if &id == b"LIST" && body.len() >= 4 {
            Some((id, Some(body[..4].try_into().unwrap()), &body[4..]))
        } else {
            Some((id, None, body))
        }
    })
}

/// What a stream list (`strl`) says.
struct Strl {
    kind: [u8; 4],
    handler: [u8; 4],
    scale: u32,
    rate: u32,
    sample_size: u32,
    format: Vec<u8>,
    /// The OpenDML super index, when it has one.
    indx: Option<Vec<u8>>,
}

fn parse_hdrl(hdrl: &[u8]) -> Result<(u32, Vec<Strl>)> {
    let mut micros = 0;
    let mut out = Vec::new();
    for (id, list, body) in children(hdrl) {
        match (&id, list.as_ref()) {
            (b"avih", None) => micros = le32(body, 0),
            (b"LIST", Some(b"strl")) => {
                let mut s = Strl { kind: [0; 4], handler: [0; 4], scale: 0, rate: 0, sample_size: 0, format: Vec::new(), indx: None };
                let mut have_header = false;
                for (id, _, body) in children(body) {
                    match &id {
                        b"strh" if body.len() >= 48 => {
                            s.kind = body[..4].try_into().unwrap();
                            s.handler = body[4..8].try_into().unwrap();
                            s.scale = le32(body, 20);
                            s.rate = le32(body, 24);
                            s.sample_size = le32(body, 44);
                            have_header = true;
                        }
                        b"strf" => s.format = body[..body.len().min(MAX_FORMAT)].to_vec(),
                        b"indx" => s.indx = Some(body[..body.len().min(MAX_FORMAT)].to_vec()),
                        _ => {}
                    }
                }
                if !have_header {
                    return Err(bad("a stream list has no stream header"));
                }
                out.push(s);
            }
            _ => {}
        }
    }
    Ok((micros, out))
}

/// Where the lists and the index lie in the file.
struct Layout {
    hdrl: Option<Vec<u8>>,
    /// Each `movi` list's data (after its type) and end.
    movi: Vec<(u64, u64)>,
    idx1: Option<(u64, u64)>,
}

fn read_at<R: Read + Seek>(r: &mut R, at: u64, n: usize) -> Result<Vec<u8>> {
    r.seek(SeekFrom::Start(at))?;
    let mut v = vec![0u8; n];
    r.read_exact(&mut v).map_err(|_| bad("the file ends inside a chunk"))?;
    Ok(v)
}

/// Walks the top level: the first RIFF (`AVI `) and the OpenDML continuations (`AVIX`).
fn layout<R: Read + Seek>(r: &mut R, len: u64) -> Result<Layout> {
    let mut lay = Layout { hdrl: None, movi: Vec::new(), idx1: None };
    let mut pos = 0u64;
    let mut riffs = 0;
    while pos + 12 <= len {
        let h = read_at(r, pos, 12)?;
        let form = &h[8..12];
        if &h[..4] != b"RIFF" || (form != b"AVI " && form != b"AVIX") {
            break;
        }
        riffs += 1;
        let size = le32(&h, 4) as u64;
        // a streaming writer leaves 0 or all ones in the size
        let end = if size < 4 || pos + 8 + size > len { len } else { pos + 8 + size };
        let mut p = pos + 12;
        while p + 8 <= end {
            let h = read_at(r, p, 12.min((end - p) as usize))?;
            let size = le32(&h, 4) as u64;
            let body = p + 8;
            let list = h.len() >= 12 && &h[..4] == b"LIST";
            match (&h[..4], list.then(|| &h[8..12])) {
                (b"LIST", Some(b"hdrl")) if lay.hdrl.is_none() => {
                    let n = size.saturating_sub(4).min(end.saturating_sub(body + 4));
                    if n > MAX_HDRL {
                        return Err(bad("the header list is implausibly large"));
                    }
                    lay.hdrl = Some(read_at(r, body + 4, n as usize)?);
                }
                (b"LIST", Some(b"movi")) => lay.movi.push((body + 4, (body + size).min(end))),
                (b"idx1", _) if lay.idx1.is_none() => lay.idx1 = Some((body, size.min(end.saturating_sub(body)))),
                _ => {}
            }
            let next = body.saturating_add(size).saturating_add(size & 1);
            if next <= p || body + size > end {
                break;
            }
            p = next;
        }
        pos = end + (end & 1);
    }
    if riffs == 0 {
        return Err(bad("this is not a RIFF AVI file"));
    }
    Ok(lay)
}

/// One chunk of a stream: where its data lies, how long it is and whether it starts a picture.
#[derive(Clone, Copy)]
struct Entry {
    stream: usize,
    offset: u64,
    size: u32,
    key: bool,
}

/// The stream number and the kind of a chunk id: `00dc` is stream 0's video, `01wb` stream 1's audio.
fn chunk_stream(id: &[u8]) -> Option<usize> {
    if id.len() < 4 || !id[0].is_ascii_digit() || !id[1].is_ascii_digit() {
        return None;
    }
    // `ix##` indexes and `pc` palette changes are not data
    if matches!(&id[2..4], b"ix" | b"pc") {
        return None;
    }
    Some(((id[0] - b'0') * 10 + (id[1] - b'0')) as usize)
}

/// A walk over the `movi` lists, for a file with no index (or none that holds up).
fn scan<R: Read + Seek>(r: &mut R, len: u64, movi: &[(u64, u64)], nstreams: usize) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    for &(start, end) in movi {
        let mut p = start;
        while p + 8 <= end {
            let h = read_at(r, p, 8)?;
            let size = le32(&h, 4) as u64;
            if &h[..4] == b"LIST" {
                p += 12; // a `rec ` list: its chunks follow
                continue;
            }
            let data = p + 8;
            if data + size > len || size > MAX_CHUNK {
                break; // cut off, or not a chunk
            }
            if let Some(stream) = chunk_stream(&h[..4]).filter(|&s| s < nstreams) {
                out.push(Entry { stream, offset: data, size: size as u32, key: false });
            }
            p = data + size + (size & 1);
        }
    }
    Ok(out)
}

/// The `idx1` index. Its offsets are from the `movi` list's type, or from the start of the file in
/// writers that wrote them so; the first chunk it names says which.
fn read_idx1<R: Read + Seek>(r: &mut R, len: u64, movi: &[(u64, u64)], at: (u64, u64), nstreams: usize) -> Option<Vec<Entry>> {
    let base_movi = movi.first()?.0.checked_sub(4)?;
    let n = (at.1 / 16) as usize;
    r.seek(SeekFrom::Start(at.0)).ok()?;
    let mut rd = BufReader::new(r.by_ref().take(n as u64 * 16));
    let mut raw = Vec::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        let mut e = [0u8; 16];
        rd.read_exact(&mut e).ok()?;
        raw.push(e);
    }
    drop(rd);
    let first = raw.iter().find(|e| chunk_stream(&e[..4]).is_some())?;
    let check = |r: &mut R, base: u64| -> bool {
        let off = base.saturating_add(le32(first, 8) as u64);
        read_at(r, off, 4).is_ok_and(|id| id == first[..4])
    };
    let base = if check(r, base_movi) {
        base_movi
    } else if check(r, 0) {
        0
    } else {
        return None;
    };
    let mut out = Vec::new();
    for e in &raw {
        let Some(stream) = chunk_stream(&e[..4]).filter(|&s| s < nstreams) else { continue };
        let size = le32(e, 12) as u64;
        let data = base.saturating_add(le32(e, 8) as u64).saturating_add(8);
        if data.saturating_add(size) > len || size > MAX_CHUNK {
            return None; // lies beyond the file, or is not a chunk: scan instead
        }
        out.push(Entry { stream, offset: data, size: size as u32, key: le32(e, 4) & 0x10 != 0 });
    }
    // the last entry is checked as well, so an index of another file's offsets is not believed
    let last = out.last()?;
    if read_at(r, last.offset.checked_sub(8)?, 4).ok().and_then(|id| chunk_stream(&id)) != Some(last.stream) {
        return None;
    }
    Some(out)
}

/// The OpenDML indexes: each stream's super index names `ix##` chunks, which list its chunks. `None`
/// when a stream lacks one or one is damaged.
fn read_odml<R: Read + Seek>(r: &mut R, len: u64, strls: &[Strl]) -> Option<Vec<Entry>> {
    let mut out = Vec::new();
    for (stream, s) in strls.iter().enumerate() {
        let sup = s.indx.as_ref()?;
        // wLongsPerEntry 4, bIndexType 0 (super index of indexes), entries of offset, size, duration
        if le16(sup, 0) != 4 || sup.get(3) != Some(&0) {
            return None;
        }
        let n = (le32(sup, 4) as usize).min(sup.len().saturating_sub(24) / 16);
        if n == 0 {
            return None;
        }
        for i in 0..n {
            let at = 24 + i * 16;
            let (off, size) = (le64(sup, at), le32(sup, at + 8) as u64);
            if off.saturating_add(size) > len || size > MAX_FORMAT as u64 * 64 {
                return None;
            }
            let ix = read_at(r, off + 8, size as usize).ok()?;
            // wLongsPerEntry 2, bIndexType 1 (chunks), entries of offset (from the base) and size
            if le16(&ix, 0) != 2 || ix.get(3) != Some(&1) {
                return None;
            }
            let base = le64(&ix, 12);
            let count = (le32(&ix, 4) as usize).min(ix.len().saturating_sub(24) / 8);
            for j in 0..count {
                let (o, sz) = (le32(&ix, 24 + j * 8) as u64, le32(&ix, 28 + j * 8));
                let (size, delta) = ((sz & 0x7FFF_FFFF) as u64, sz >> 31 != 0);
                let data = base.saturating_add(o);
                if data.saturating_add(size) > len || size > MAX_CHUNK {
                    return None; // the index is not of this file, or the file is cut off: the chunks themselves are the truth
                }
                out.push(Entry { stream, offset: data, size: size as u32, key: !delta });
            }
        }
    }
    out.sort_by_key(|e| e.offset);
    (!out.is_empty()).then_some(out)
}

/// A waveformat's parts.
struct WaveFormat {
    tag: u16,
    channels: u16,
    rate: u32,
    avg_bytes: u32,
    block_align: u16,
    bits: u16,
    extra: Vec<u8>,
}

fn wave_format(f: &[u8]) -> Option<WaveFormat> {
    if f.len() < 16 {
        return None;
    }
    let mut w = WaveFormat { tag: le16(f, 0), channels: le16(f, 2), rate: le32(f, 4), avg_bytes: le32(f, 8), block_align: le16(f, 12), bits: le16(f, 14), extra: Vec::new() };
    if f.len() >= 18 {
        let cb = le16(f, 16) as usize;
        w.extra = f.get(18..18 + cb).unwrap_or(&f[18..]).to_vec();
    }
    // WAVE_FORMAT_EXTENSIBLE: the real tag opens the sub-format GUID after the valid bits and the channel mask
    if w.tag == 0xFFFE && w.extra.len() >= 24 {
        w.tag = le16(&w.extra, 6);
    }
    Some(w)
}

fn audio_codec(w: &WaveFormat) -> std::result::Result<CodecId, String> {
    Ok(match (w.tag, w.bits) {
        (1, 8) => CodecId::PcmU8,
        (1, 16) => CodecId::PcmS16Le,
        (1, 24) => CodecId::PcmS24Le,
        (1, 32) => CodecId::PcmS32Le,
        (3, 32) => CodecId::PcmF32Le,
        (3, 64) => CodecId::PcmF64Le,
        (1 | 3, b) => return Err(format!("PCM at {b} bits (wFormatTag {})", w.tag)),
        (0x55, _) => CodecId::Mp3,
        (0x50, _) => CodecId::Mp2,
        (0x2000, _) => CodecId::Ac3,
        (0xFF, _) => CodecId::Aac,
        (t, _) => return Err(format!("audio wFormatTag 0x{t:X}")),
    })
}

fn fourcc_text(f: &[u8; 4]) -> String {
    f.iter().map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '?' }).collect::<String>().trim_end().to_string()
}

fn video_codec(fourcc: &[u8; 4]) -> Option<CodecId> {
    let up = fourcc.map(|b| b.to_ascii_uppercase());
    Some(match &up {
        b"XVID" | b"DIVX" | b"DX50" | b"FMP4" | b"MP4V" | b"3IV2" | b"MP4S" | b"M4S2" => CodecId::Mpeg4,
        b"H264" | b"AVC1" | b"X264" | b"DAVC" => CodecId::H264,
        b"MJPG" | b"AVRN" | b"LJPG" => CodecId::Mjpeg,
        _ => return None,
    })
}

/// `DivX5xxbNNNp`: the user data DivX's packing encoders write, its last letter `p` for packed.
fn divx_packed(d: &[u8]) -> bool {
    let Some(at) = d.windows(4).position(|w| w == b"DivX") else { return false };
    let mut rest = &d[at + 4..];
    let digits = |r: &mut &[u8]| {
        let n = r.iter().take_while(|b| b.is_ascii_digit()).count();
        *r = &r[n..];
        n
    };
    if digits(&mut rest) == 0 || rest.first() != Some(&b'b') {
        return false;
    }
    rest = &rest[1..];
    digits(&mut rest);
    rest.first() == Some(&b'p')
}

/// The sequence and layer headers at the start of an MPEG-4 key frame: what a container calls the
/// decoder configuration.
fn mpeg4_config(d: &[u8]) -> Vec<u8> {
    let Some(start) = d.windows(4).position(|w| w[..3] == [0, 0, 1] && (w[3] == 0xB0 || (0x20..=0x2F).contains(&w[3]))) else { return Vec::new() };
    let end = d[start + 4..].windows(4).position(|w| w[..3] == [0, 0, 1] && matches!(w[3], 0xB6 | 0xB3 | 0xB2)).map_or(d.len(), |e| start + 4 + e);
    d[start..end].to_vec()
}

/// An AVI reader: the MP4 reader's machinery over the samples found, and what was left out.
pub struct AviDemuxer<R: Read + Seek + Send> {
    inner: Mp4Demuxer<BufReader<R>>,
    unsupported: Vec<String>,
}

impl<R: Read + Seek + Send> Demuxer for AviDemuxer<R> {
    fn format_name(&self) -> &'static str {
        "avi"
    }
    fn streams(&self) -> &[StreamInfo] {
        self.inner.streams()
    }
    fn read_packet(&mut self) -> Result<Option<Packet>> {
        self.inner.read_packet()
    }
    fn discard(&mut self, stream: usize) {
        self.inner.discard(stream)
    }
    fn seek(&mut self, seconds: f64) -> Result<f64> {
        self.inner.seek(seconds)
    }
    fn unsupported_streams(&self) -> Vec<String> {
        self.unsupported.clone()
    }
}

fn reduce(n: u32, d: u32) -> Rational {
    let g = gcd(n as i64, d as i64);
    Rational::new(n as i64 / g, d as i64 / g)
}

/// Opens an AVI file for reading.
pub fn open<R: Read + Seek + Send>(mut r: R) -> Result<AviDemuxer<R>> {
    let len = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let mut r = BufReader::with_capacity(256 << 10, r);
    let lay = layout(&mut r, len)?;
    let hdrl = lay.hdrl.as_deref().ok_or_else(|| bad("the file has no header list (hdrl)"))?;
    let (micros, strls) = parse_hdrl(hdrl)?;
    if strls.is_empty() {
        return Err(bad("the file lists no streams"));
    }

    let mut entries = None;
    if strls.iter().all(|s| s.indx.is_some()) {
        entries = read_odml(&mut r, len, &strls);
    }
    if entries.is_none() {
        if let Some(at) = lay.idx1 {
            entries = read_idx1(&mut r, len, &lay.movi, at, strls.len());
        }
    }
    let indexed = entries.is_some();
    let entries = match entries {
        Some(e) => e,
        None => scan(&mut r, len, &lay.movi, strls.len())?,
    };

    // what each stream is
    enum Kind {
        Video { codec: CodecId, width: u32, height: u32 },
        Audio { codec: CodecId, w: WaveFormat },
    }
    let mut kinds: Vec<std::result::Result<Kind, String>> = Vec::new();
    for (i, s) in strls.iter().enumerate() {
        kinds.push(match &s.kind {
            b"vids" if s.format.len() >= 40 => {
                // the codec is the bitmap header's compression, and the stream header's handler when that is empty
                let comp: [u8; 4] = s.format[16..20].try_into().unwrap();
                let fourcc = if comp == [0; 4] { s.handler } else { comp };
                let (w, h) = (le32(&s.format, 4) as i32, le32(&s.format, 8) as i32);
                match video_codec(&fourcc) {
                    Some(codec) if w > 0 && h != 0 => Ok(Kind::Video { codec, width: w as u32, height: h.unsigned_abs() }),
                    Some(_) => Err(format!("video stream {i}: the picture size is {w}x{h}")),
                    None => Err(format!("video stream {i}: codec '{}'", fourcc_text(&fourcc))),
                }
            }
            b"vids" => Err(format!("video stream {i}: no format header")),
            b"auds" => match wave_format(&s.format) {
                Some(w) if w.channels > 0 && w.rate > 0 => audio_codec(&w).map(|codec| Kind::Audio { codec, w }).map_err(|e| format!("audio stream {i}: {e}")),
                Some(_) => Err(format!("audio stream {i}: no channels or sample rate")),
                None => Err(format!("audio stream {i}: no format header")),
            },
            other => Err(format!("stream {i}: type '{}' (not audio or video)", fourcc_text(other))),
        });
    }
    let unsupported: Vec<String> = kinds.iter().filter_map(|k| k.as_ref().err().cloned()).collect();

    // the chunks of each stream, in file order
    let mut per: Vec<Vec<Entry>> = vec![Vec::new(); strls.len()];
    let mut entries = entries;
    entries.sort_by_key(|e| e.offset);
    for e in entries {
        per[e.stream].push(e);
    }

    let mut tracks: Vec<TrackInfo> = Vec::new();
    let mut samples: Vec<Sample> = Vec::new();
    for (i, (s, kind)) in strls.iter().zip(&kinds).enumerate() {
        let Ok(kind) = kind else { continue };
        let chunks = &per[i];
        let index = tracks.len();
        let head = |r: &mut BufReader<R>, e: &Entry, n: usize| -> Vec<u8> { read_at(r, e.offset, (e.size as usize).min(n)).unwrap_or_default() };
        match kind {
            Kind::Video { codec, width, height } => {
                let (mut scale, mut rate) = (s.scale, s.rate);
                if scale == 0 || rate == 0 {
                    (scale, rate) = if micros > 0 { (micros, 1_000_000) } else { (1, 25) };
                }
                let time_base = reduce(scale, rate);
                let frame_rate = reduce(rate, scale);
                let codec = *codec;
                let mut config = Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 };
                let mut convert = false;
                let mut extradata = Vec::new();
                let mut packed = false;
                if let Some(f) = chunks.iter().find(|e| e.size > 0) {
                    let d = head(&mut r, f, 4096);
                    match codec {
                        CodecId::H264 => {
                            // Annex B, or length-prefixed units described by an avcC record after the bitmap header
                            let annex_b = d.starts_with(&[0, 0, 1]) || d.starts_with(&[0, 0, 0, 1]);
                            if !annex_b && s.format.len() > 40 && s.format[40] == 1 {
                                config = parse_avcc(&s.format[40..])?;
                                convert = true;
                            }
                        }
                        CodecId::Mpeg4 => {
                            extradata = mpeg4_config(&d);
                            packed = divx_packed(&d);
                        }
                        _ => {}
                    }
                }
                // ticks are chunk slots: a dropped frame (an empty chunk, or a padding VOP) lasts into the next
                let mut slot = 0i64;
                let mut last: Option<usize> = None;
                for (n, e) in chunks.iter().enumerate() {
                    let dropped = e.size == 0 || (packed && e.size <= NVOP_MAX);
                    if dropped {
                        if let Some(l) = last {
                            samples[l].duration = samples[l].duration.saturating_add(1);
                        }
                        slot += 1;
                        continue;
                    }
                    let key = if indexed {
                        e.key || n == 0
                    } else {
                        let d = head(&mut r, e, 4096);
                        crate::keyframe::is_keyframe(codec, &d).unwrap_or(n == 0)
                    };
                    last = Some(samples.len());
                    samples.push(Sample { stream: index as u16, sync: key, offset: e.offset, size: e.size, dts: slot, cts_offset: 0, duration: 1 });
                    slot += 1;
                }
                let pix_fmt = config.pix_fmt();
                let duration = slot;
                let stream = StreamInfo { index, codec, time_base, duration: Some(duration), bits: None, extradata, lead_in: 0, params: StreamParams::Video { width: *width, height: *height, pix_fmt, frame_rate } };
                tracks.push(TrackInfo { stream, config, convert, swap: 0, unwrap: Vec::new(), meta: Default::default() });
            }
            Kind::Audio { codec, w } => {
                let codec = *codec;
                let rate = w.rate as i128;
                let time_base = Rational::new(1, w.rate as i64);
                let pcm = codec.sample_format().is_some();
                let frame_bytes = (w.channels as u32 * (w.bits as u32 / 8)).max(1);
                // the stream's own clock: `scale / rate` seconds a tick, a tick a chunk or `sample_size` bytes
                let (mut scale, mut trate, mut sample_size) = (s.scale, s.rate, s.sample_size);
                // VBR MPEG audio: a block align of a frame's samples, and a sample size to match, means a chunk a frame
                if matches!(codec, CodecId::Mp3 | CodecId::Mp2) && w.block_align >= 576 && (sample_size == 0 || sample_size == w.block_align as u32) {
                    let frame = if codec == CodecId::Mp3 && w.rate < 32000 { 576 } else { 1152 };
                    (scale, trate, sample_size) = (frame, w.rate, 0);
                }
                if scale == 0 || trate == 0 {
                    // no usable clock: bytes at the format's average rate
                    (scale, trate, sample_size) = (1, w.avg_bytes.max(1), 1);
                }
                if pcm {
                    (scale, trate, sample_size) = (1, w.rate, frame_bytes);
                }
                let mut bytes = 0u64;
                let mut count = 0u64;
                let mut total = 0i64;
                let at = |ticks: u64| ((ticks as i128 * scale as i128 * rate + trate as i128 / 2) / trate as i128) as i64;
                for e in chunks.iter().filter(|e| e.size > 0) {
                    let (t0, t1) = if sample_size > 0 {
                        let (a, b) = (bytes / sample_size as u64, (bytes + e.size as u64) / sample_size as u64);
                        bytes += e.size as u64;
                        (a, b)
                    } else {
                        count += 1;
                        (count - 1, count)
                    };
                    let (p0, p1) = (at(t0), at(t1));
                    total = p1;
                    samples.push(Sample { stream: index as u16, sync: true, offset: e.offset, size: e.size, dts: p0, cts_offset: 0, duration: u32::try_from((p1 - p0).max(0)).unwrap_or(u32::MAX) });
                }
                let extradata = if codec == CodecId::Aac { w.extra.clone() } else { Vec::new() };
                let bits = pcm.then_some(w.bits as u8);
                let stream = StreamInfo { index, codec, time_base, duration: Some(total), bits, extradata, lead_in: 0, params: StreamParams::Audio { sample_rate: w.rate, channels: w.channels } };
                tracks.push(TrackInfo { stream, config: Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 }, convert: false, swap: 0, unwrap: Vec::new(), meta: Default::default() });
            }
        }
    }
    if tracks.is_empty() {
        let what = if unsupported.is_empty() { "no stream".to_string() } else { unsupported.join("; ") };
        return Err(Error::new(ErrorCode::UnsupportedCodec, format!("AVI: no stream IronAV can play ({what})"))
            .hint("AVI files with MPEG-4 Part 2 (XVID, DIVX), H.264 or Motion-JPEG video and PCM, MP2, MP3, AC-3 or AAC audio are read"));
    }
    if samples.is_empty() {
        return Err(bad("the file has no frames (no index, and no chunks in a movi list)"));
    }
    Ok(AviDemuxer { inner: Mp4Demuxer::from_parts(r, "avi", tracks, samples), unsupported })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend((body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        if body.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn list(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        chunk(b"LIST", &[kind.as_slice(), body].concat())
    }

    fn strh(kind: &[u8; 4], handler: &[u8; 4], scale: u32, rate: u32, sample_size: u32) -> Vec<u8> {
        let mut h = vec![0u8; 56];
        h[..4].copy_from_slice(kind);
        h[4..8].copy_from_slice(handler);
        h[20..24].copy_from_slice(&scale.to_le_bytes());
        h[24..28].copy_from_slice(&rate.to_le_bytes());
        h[44..48].copy_from_slice(&sample_size.to_le_bytes());
        chunk(b"strh", &h)
    }

    fn bitmap(fourcc: &[u8; 4], w: i32, h: i32) -> Vec<u8> {
        let mut b = vec![0u8; 40];
        b[..4].copy_from_slice(&40u32.to_le_bytes());
        b[4..8].copy_from_slice(&w.to_le_bytes());
        b[8..12].copy_from_slice(&h.to_le_bytes());
        b[12..14].copy_from_slice(&1u16.to_le_bytes());
        b[14..16].copy_from_slice(&24u16.to_le_bytes());
        b[16..20].copy_from_slice(fourcc);
        chunk(b"strf", &b)
    }

    fn wavefmt(tag: u16, channels: u16, rate: u32, block: u16, bits: u16) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(tag.to_le_bytes());
        b.extend(channels.to_le_bytes());
        b.extend(rate.to_le_bytes());
        b.extend((rate * block as u32).to_le_bytes());
        b.extend(block.to_le_bytes());
        b.extend(bits.to_le_bytes());
        chunk(b"strf", &b)
    }

    fn video_strl(fourcc: &[u8; 4], extra: &[u8]) -> Vec<u8> {
        list(b"strl", &[strh(b"vids", fourcc, 1, 25, 0), bitmap(fourcc, 64, 48), extra.to_vec()].concat())
    }

    fn pcm_strl() -> Vec<u8> {
        list(b"strl", &[strh(b"auds", &[0; 4], 1, 8000, 2), wavefmt(1, 1, 8000, 2, 16)].concat())
    }

    /// An AVI with `video` (stream 0) and `audio` (stream 1) chunks in file order; `index` picks idx1 or none.
    struct Built {
        bytes: Vec<u8>,
        /// Where each chunk's data begins, in the order given.
        data_at: Vec<u64>,
    }

    fn avi(strls: &[Vec<u8>], chunks: &[(&[u8; 4], Vec<u8>, bool)], idx1: bool) -> Built {
        let mut hdrl = chunk(b"avih", &[0u8; 56]);
        for s in strls {
            hdrl.extend_from_slice(s);
        }
        let hdrl = list(b"hdrl", &hdrl);
        let mut movi_body = Vec::new();
        let mut rel = Vec::new();
        for (id, body, _) in chunks {
            rel.push(movi_body.len() as u32 + 4);
            movi_body.extend(chunk(id, body));
        }
        let movi = list(b"movi", &movi_body);
        let mut index = Vec::new();
        for ((id, body, key), off) in chunks.iter().zip(&rel) {
            index.extend_from_slice(*id);
            index.extend((if *key { 0x10u32 } else { 0 }).to_le_bytes());
            index.extend(off.to_le_bytes());
            index.extend((body.len() as u32).to_le_bytes());
        }
        let mut body = b"AVI ".to_vec();
        body.extend(&hdrl);
        let movi_at = 12 + hdrl.len() as u64;
        body.extend(&movi);
        if idx1 {
            body.extend(chunk(b"idx1", &index));
        }
        let mut bytes = b"RIFF".to_vec();
        bytes.extend((body.len() as u32).to_le_bytes());
        bytes.extend(body);
        let data_at = rel.iter().map(|r| movi_at + 12 + (*r as u64 - 4) + 8).collect();
        Built { bytes, data_at }
    }

    fn read_all(bytes: Vec<u8>) -> (Vec<StreamInfo>, Vec<Packet>) {
        let mut d = open(Cursor::new(bytes)).unwrap();
        let s = d.streams().to_vec();
        let mut p = Vec::new();
        while let Some(x) = d.read_packet().unwrap() {
            p.push(x);
        }
        (s, p)
    }

    fn mjpeg_audio_file(idx1: bool) -> Vec<u8> {
        let chunks: Vec<(&[u8; 4], Vec<u8>, bool)> = vec![
            (b"00dc", vec![0xFF, 0xD8, 1, 1], true),
            (b"01wb", vec![1; 1600], true),
            (b"00dc", vec![0xFF, 0xD8, 2, 2, 2], true),
            (b"01wb", vec![2; 1600], true),
            (b"00dc", vec![], false),
            (b"01wb", vec![3; 800], true),
            (b"00dc", vec![0xFF, 0xD8, 3], true),
        ];
        avi(&[video_strl(b"MJPG", &[]), pcm_strl()], &chunks, idx1).bytes
    }

    #[test]
    fn mjpeg_and_pcm_read_back_with_times_from_the_index() {
        let (s, p) = read_all(mjpeg_audio_file(true));
        assert_eq!((s[0].codec, s[1].codec), (CodecId::Mjpeg, CodecId::PcmS16Le));
        assert_eq!(s[0].time_base, Rational::new(1, 25));
        match s[0].params {
            StreamParams::Video { width, height, frame_rate, .. } => assert_eq!((width, height, frame_rate), (64, 48, Rational::new(25, 1))),
            _ => panic!("not video"),
        }
        let video: Vec<_> = p.iter().filter(|x| x.stream == 0).map(|x| (x.pts, x.duration, x.data.len())).collect();
        // the empty chunk is a dropped frame: the second picture lasts through its slot
        assert_eq!(video, vec![(0, 1, 4), (1, 2, 5), (3, 1, 3)]);
        let audio: Vec<_> = p.iter().filter(|x| x.stream == 1).map(|x| (x.pts, x.duration)).collect();
        assert_eq!(audio, vec![(0, 800), (800, 800), (1600, 400)]);
        assert_eq!(s[1].duration, Some(2000));
        assert_eq!(s[0].duration, Some(4));
    }

    #[test]
    fn a_file_with_no_index_is_scanned() {
        let (_, with) = read_all(mjpeg_audio_file(true));
        let (_, without) = read_all(mjpeg_audio_file(false));
        let key = |p: &Vec<Packet>| p.iter().map(|x| (x.stream, x.pts, x.duration, x.data.clone())).collect::<Vec<_>>();
        assert_eq!(key(&with), key(&without));
    }

    #[test]
    fn seeking_returns_to_a_key_frame() {
        let mut d = open(Cursor::new(mjpeg_audio_file(true))).unwrap();
        let landed = d.seek(0.07).unwrap();
        assert!((landed - 0.04).abs() < 1e-9, "{landed}");
        let first = d.read_packet().unwrap().unwrap();
        assert!(first.pts as f64 / 25.0 >= landed - 1e-9);
    }

    #[test]
    fn unknown_codecs_are_reported_not_played() {
        let chunks: Vec<(&[u8; 4], Vec<u8>, bool)> = vec![(b"00dc", vec![1, 2, 3], true), (b"01wb", vec![0; 64], true)];
        let wma = list(b"strl", &[strh(b"auds", &[0; 4], 1, 8000, 1), wavefmt(0x161, 2, 44100, 1, 16)].concat());
        let b = avi(&[video_strl(b"MJPG", &[]), wma.clone()], &chunks, true).bytes;
        let d = open(Cursor::new(b)).unwrap();
        assert_eq!(d.streams().len(), 1);
        let u = d.unsupported_streams();
        assert_eq!(u.len(), 1);
        assert!(u[0].contains("0x161"), "{u:?}");
        // nothing playable is an error that names what is in the file
        let b = avi(&[video_strl(b"WMV3", &[]), wma], &chunks, true).bytes;
        let e = open(Cursor::new(b)).err().unwrap();
        assert_eq!(e.code, ErrorCode::UnsupportedCodec);
        assert!(e.message.contains("WMV3") && e.message.contains("0x161"), "{}", e.message);
    }

    #[test]
    fn vbr_mp3_gets_a_frame_of_time_per_chunk() {
        // 1152 samples per chunk, as strh scale 1152 / rate 44100 with no sample size and a block align of 1152
        let mp3 = list(b"strl", &[strh(b"auds", &[0; 4], 1152, 44100, 0), wavefmt(0x55, 2, 44100, 1152, 0)].concat());
        let chunks: Vec<(&[u8; 4], Vec<u8>, bool)> = (0..4).map(|i| (b"00wb", vec![i; 100 + i as usize * 50], true)).collect();
        let (s, p) = read_all(avi(&[mp3], &chunks, true).bytes);
        assert_eq!(s[0].codec, CodecId::Mp3);
        assert_eq!(p.iter().map(|x| (x.pts, x.duration)).collect::<Vec<_>>(), vec![(0, 1152), (1152, 1152), (2304, 1152), (3456, 1152)]);
        // the same with the strh's sample size set to the block align, as some writers do
        let mp3 = list(b"strl", &[strh(b"auds", &[0; 4], 1, 44100, 1152), wavefmt(0x55, 2, 44100, 1152, 0)].concat());
        let (_, p) = read_all(avi(&[mp3], &chunks, true).bytes);
        assert_eq!(p[3].pts, 3456);
    }

    #[test]
    fn cbr_mp3_is_timed_by_its_bytes() {
        // 16000 bytes a second, a tick a byte
        let mp3 = list(b"strl", &[strh(b"auds", &[0; 4], 1, 16000, 1), wavefmt(0x55, 2, 44100, 1, 0)].concat());
        let chunks: Vec<(&[u8; 4], Vec<u8>, bool)> = vec![(b"00wb", vec![0; 4000], true), (b"00wb", vec![0; 4000], true)];
        let (_, p) = read_all(avi(&[mp3], &chunks, true).bytes);
        assert_eq!(p[1].pts, 11025, "a quarter of a second at 44.1 kHz");
    }

    #[test]
    fn packed_bitstream_drops_the_padding_vops() {
        let mut key = vec![0, 0, 1, 0xB0, 1, 0, 0, 1, 0x20, 0xAA, 0, 0, 1, 0xB2];
        key.extend(b"DivX503b1393p");
        key.extend([0, 0, 1, 0xB6, 0x10, 0x11]);
        let nvop = vec![0, 0, 1, 0xB6, 0x7F];
        let p_and_b = vec![0, 0, 1, 0xB6, 0x50, 1, 2, 3, 0, 0, 1, 0xB6, 0x90, 4, 5];
        let chunks: Vec<(&[u8; 4], Vec<u8>, bool)> = vec![(b"00dc", key, true), (b"00dc", p_and_b.clone(), false), (b"00dc", nvop, false), (b"00dc", p_and_b, false)];
        let (s, p) = read_all(avi(&[video_strl(b"DX50", &[])], &chunks, true).bytes);
        assert_eq!(s[0].codec, CodecId::Mpeg4);
        assert_eq!(s[0].extradata, vec![0, 0, 1, 0xB0, 1, 0, 0, 1, 0x20, 0xAA], "the headers before the user data");
        assert_eq!(p.iter().map(|x| (x.pts, x.duration)).collect::<Vec<_>>(), vec![(0, 1), (1, 2), (3, 1)]);
    }

    #[test]
    fn h264_annex_b_chunks_pass_through() {
        let idr = vec![0, 0, 0, 1, 0x65, 0x88, 0x84];
        let chunks: Vec<(&[u8; 4], Vec<u8>, bool)> = vec![(b"00dc", idr.clone(), true)];
        let (s, p) = read_all(avi(&[video_strl(b"H264", &[])], &chunks, false).bytes);
        assert_eq!(s[0].codec, CodecId::H264);
        assert_eq!(p[0].data, idr);
    }

    /// An OpenDML file: a first RIFF and an AVIX with one chunk each, indexed by super and standard indexes.
    fn opendml(break_it: bool) -> Vec<u8> {
        let mut super_idx = Vec::new(); // filled once the offsets are known
        let strl_body = |idx: &[u8]| [strh(b"vids", b"MJPG", 1, 25, 0), bitmap(b"MJPG", 64, 48), chunk(b"indx", idx)].concat();
        let placeholder = vec![0u8; 24 + 2 * 16];
        let hdrl = list(b"hdrl", &[chunk(b"avih", &[0u8; 56]), list(b"strl", &strl_body(&placeholder))].concat());
        let frame = |n: u8| chunk(b"00dc", &[0xFF, 0xD8, n, n]);
        // layout: RIFF AVI [hdrl][movi [frame 1][ix00]] RIFF AVIX [movi [frame 2][ix00]]
        let hdrl_len = hdrl.len();
        let std_index = |base: u64, off: u32, size: u32| {
            let mut ix = vec![2, 0, 0, 1];
            ix.extend(1u32.to_le_bytes());
            ix.extend(b"00dc");
            ix.extend(base.to_le_bytes());
            ix.extend(0u32.to_le_bytes());
            ix.extend(off.to_le_bytes());
            ix.extend(size.to_le_bytes());
            chunk(b"ix00", &ix)
        };
        let movi1_at = 12 + hdrl_len as u64;
        let f1_at = movi1_at + 12; // the chunk header
        let ix1_at = f1_at + frame(1).len() as u64;
        let ix1 = std_index(f1_at, 8, 4);
        let movi1 = list(b"movi", &[frame(1), ix1.clone()].concat());
        let riff1_len = 4 + hdrl_len + movi1.len();
        let riff2_at = 8 + riff1_len as u64;
        let f2_at = riff2_at + 12 + 12;
        let ix2_at = f2_at + frame(2).len() as u64;
        let ix2 = std_index(f2_at, 8, 4);
        let movi2 = list(b"movi", &[frame(2), ix2.clone()].concat());
        for (at, ix) in [(ix1_at, &ix1), (ix2_at, &ix2)] {
            super_idx.extend(at.to_le_bytes());
            super_idx.extend((ix.len() as u32 - 8).to_le_bytes());
            super_idx.extend(1u32.to_le_bytes());
        }
        let mut idx = vec![4, 0, 0, 0];
        idx.extend(2u32.to_le_bytes());
        idx.extend(b"00dc");
        idx.extend([0u8; 12]);
        idx.extend(&super_idx);
        let hdrl = list(b"hdrl", &[chunk(b"avih", &[0u8; 56]), list(b"strl", &strl_body(&idx))].concat());
        assert_eq!(hdrl.len(), hdrl_len);
        let mut f = b"RIFF".to_vec();
        f.extend((riff1_len as u32).to_le_bytes());
        f.extend(b"AVI ");
        f.extend(&hdrl);
        f.extend(&movi1);
        f.extend(b"RIFF");
        f.extend((4 + movi2.len() as u32).to_le_bytes());
        f.extend(b"AVIX");
        f.extend(&movi2);
        if break_it {
            // the second standard index names a chunk past the end of the file
            let at = ix2_at as usize + 8 + 12;
            f[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        }
        f
    }

    #[test]
    fn opendml_reads_frames_from_every_riff() {
        let (_, p) = read_all(opendml(false));
        assert_eq!(p.iter().map(|x| (x.pts, x.data[2])).collect::<Vec<_>>(), vec![(0, 1), (1, 2)]);
    }

    #[test]
    fn opendml_with_a_broken_index_falls_back_to_the_chunks() {
        let (_, p) = read_all(opendml(true));
        assert_eq!(p.iter().map(|x| x.data[2]).collect::<Vec<_>>(), vec![1, 2], "scanned from the movi lists");
    }

    #[test]
    fn damaged_files_are_errors_not_panics() {
        let good = mjpeg_audio_file(true);
        // every prefix of the file either opens or says why not; none panics
        for n in (0..good.len()).step_by(7) {
            if let Ok(mut d) = open(Cursor::new(good[..n].to_vec())) {
                while let Ok(Some(_)) = d.read_packet() {}
            }
        }
        // flipped bytes anywhere
        for i in (0..good.len()).step_by(3) {
            let mut b = good.clone();
            b[i] ^= 0xFF;
            if let Ok(mut d) = open(Cursor::new(b)) {
                while let Ok(Some(_)) = d.read_packet() {}
            }
        }
        assert!(open(Cursor::new(b"RIFF\0\0\0\0AVI ".to_vec())).is_err());
        assert!(open(Cursor::new(b"RIFF\0\0\0\0WAVEfmt ".to_vec())).is_err());
    }

    #[test]
    fn huge_sizes_do_not_allocate() {
        // a header list claiming 4 GB, a chunk claiming 4 GB, an idx1 whose entries point past the end
        let mut f = b"RIFF\xFF\xFF\xFF\xFFAVI LIST\xFF\xFF\xFF\xFFhdrl".to_vec();
        f.extend([0u8; 16]);
        assert!(open(Cursor::new(f)).is_err());
        let good = avi(&[video_strl(b"MJPG", &[])], &[(b"00dc", vec![1, 2, 3, 4], true)], true);
        let mut b = good.bytes.clone();
        let at = good.data_at[0] as usize - 4;
        b[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes()); // the chunk's size
        let n = b.len();
        b[n - 4..].copy_from_slice(&u32::MAX.to_le_bytes()); // the index entry's size
        assert!(open(Cursor::new(b)).is_err());
    }

    #[test]
    fn a_bad_index_is_not_believed() {
        let good = avi(&[video_strl(b"MJPG", &[])], &[(b"00dc", vec![1, 2, 3, 4], true), (b"00dc", vec![5, 6, 7, 8], true)], true);
        let mut b = good.bytes;
        let n = b.len();
        // the second entry's offset now points into the middle of nowhere
        b[n - 8..n - 4].copy_from_slice(&3u32.to_le_bytes());
        let (_, p) = read_all(b);
        assert_eq!(p.iter().map(|x| x.data[0]).collect::<Vec<_>>(), vec![1, 5], "the chunks were found by scanning");
    }
}
