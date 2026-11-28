//! ASF (Advanced Systems Format, `.asf`, `.wma`, `.wmv`): a header object holding the file and
//! stream properties, a data object of fixed-size data packets, and an optional simple index.
//!
//! Read: WMA 1 and 2 audio (format tags 0x160 and 0x161, from the stream's WAVEFORMATEX, which is
//! the stream's `extradata`), with times in milliseconds less the preroll, as FFmpeg gives them.
//! A data packet is parsed as the ASF specification lays it out: the error correction data, the
//! payload parsing information (packet length, padding, send time), then one payload or several,
//! each a whole media object, a fragment of one (objects are reassembled from fragments across
//! packets, the replicated data giving the object's size and presentation time) or a compressed
//! payload (a run of small objects at a fixed time step). Audio streams whose error correction is
//! the audio spread (a span above 1) are descrambled as FFmpeg does.
//!
//! Streams IronAV does not decode go to `unsupported_streams`, named by their format tag or
//! FourCC: all video (WMV1, WMV2, WMV3, VC-1, MS MPEG-4), WMA Pro (0x162), WMA Lossless (0x163),
//! WMA Voice (0x0A) and other audio and data streams. A file with nothing playable is refused with
//! an error naming them.
//!
//! The header's File Properties (packet size, play duration, preroll), Stream Properties, Header
//! Extension (Extended Stream Properties, for a bit rate the WAVEFORMATEX lacks), Stream Bitrate
//! Properties, Codec List (names for the streams listed as unsupported), Content Description and
//! Extended Content Description are read; the last two as [`metadata`], text that came from the
//! file and so is untrusted: control characters are removed and each value is cut to 512 characters.
//!
//! Seeking uses the Simple Index object when the file has one, else a bisection of the packets by
//! their send times (the packets are of one size, so packet `n` is at a known offset). After
//! either, the reader reads on to the last object at or before the time, so it lands on one.
//!
//! Not handled: encrypted (DRM) files, multiple bit rate files' stream priorities (all audio
//! streams are delivered), the Media Object Index and Timecode Index objects, and packets of
//! varying size when seeking (the file then cannot seek).

use ironav_codec::wma::parse_waveformat;
use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Packet, Rational, Result, StreamInfo, StreamParams};
use std::collections::VecDeque;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// The size of the header object, at most (a header of more than this is refused).
const MAX_HEADER: u64 = 1 << 26;

/// The largest media object accepted.
const MAX_OBJECT: u64 = 1 << 24;

/// The largest data packet accepted.
const MAX_PACKET: u64 = 1 << 24;

/// The most objects looked at after the data object, for indexes.
const MAX_TRAILING_OBJECTS: usize = 64;

/// The longest text kept from the file's metadata, in characters.
const MAX_TEXT: usize = 512;

const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

const HEADER: [u8; 16] = guid(0x75B22630, 0x668E, 0x11CF, [0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C]);
const DATA: [u8; 16] = guid(0x75B22636, 0x668E, 0x11CF, [0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C]);
const FILE_PROPERTIES: [u8; 16] = guid(0x8CABDCA1, 0xA947, 0x11CF, [0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65]);
const STREAM_PROPERTIES: [u8; 16] = guid(0xB7DC0791, 0xA9B7, 0x11CF, [0x8E, 0xE6, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65]);
const HEADER_EXTENSION: [u8; 16] = guid(0x5FBF03B5, 0xA92E, 0x11CF, [0x8E, 0xE3, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65]);
const EXTENDED_STREAM_PROPERTIES: [u8; 16] = guid(0x14E6A5CB, 0xC672, 0x4332, [0x83, 0x99, 0xA9, 0x69, 0x52, 0x06, 0x5B, 0x5A]);
const CONTENT_DESCRIPTION: [u8; 16] = guid(0x75B22633, 0x668E, 0x11CF, [0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C]);
const EXTENDED_CONTENT_DESCRIPTION: [u8; 16] = guid(0xD2D0A440, 0xE307, 0x11D2, [0x97, 0xF0, 0x00, 0xA0, 0xC9, 0x5E, 0xA8, 0x50]);
const CODEC_LIST: [u8; 16] = guid(0x86D15240, 0x311D, 0x11D0, [0xA3, 0xA4, 0x00, 0xA0, 0xC9, 0x03, 0x48, 0xF6]);
const STREAM_BITRATE_PROPERTIES: [u8; 16] = guid(0x7BF875CE, 0x468D, 0x11D1, [0x8D, 0x82, 0x00, 0x60, 0x97, 0xC9, 0xA2, 0xB2]);
const SIMPLE_INDEX: [u8; 16] = guid(0x33000890, 0xE5B1, 0x11CF, [0x89, 0xF4, 0x00, 0xA0, 0xC9, 0x03, 0x49, 0xCB]);
const AUDIO_MEDIA: [u8; 16] = guid(0xF8699E40, 0x5B4D, 0x11CF, [0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B]);
const VIDEO_MEDIA: [u8; 16] = guid(0xBC19EFC0, 0x5B4D, 0x11CF, [0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B]);
const COMMAND_MEDIA: [u8; 16] = guid(0x59DACFC0, 0x59E6, 0x11D0, [0xA3, 0xAC, 0x00, 0xA0, 0xC9, 0x03, 0x48, 0xF6]);
const JFIF_MEDIA: [u8; 16] = guid(0xB61BE100, 0x5B4E, 0x11CF, [0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B]);
const BINARY_MEDIA: [u8; 16] = guid(0x3AFB65E2, 0x47EF, 0x40F2, [0xAC, 0x2C, 0x70, 0xA9, 0x0D, 0x71, 0xD3, 0x43]);

/// Whether `magic` starts an ASF file: the header object's GUID.
pub fn looks_like_asf(magic: &[u8]) -> bool {
    magic.len() >= 16 && magic[..16] == HEADER
}

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::invalid_input(format!("ASF: {msg}"))
}

/// A cursor over bytes that never goes past them.
struct Cur<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn new(d: &'a [u8]) -> Self {
        Self { d, pos: 0 }
    }

    fn left(&self) -> usize {
        self.d.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.left() {
            return Err(bad("data ends inside a field"));
        }
        let s = &self.d[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn guid(&mut self) -> Result<[u8; 16]> {
        Ok(self.take(16)?.try_into().unwrap())
    }

    /// A field of 0, 1, 2 or 4 bytes by its two-bit length type (0 is absent, reading as `absent`).
    fn var(&mut self, kind: u8, absent: u32) -> Result<u32> {
        match kind & 3 {
            0 => Ok(absent),
            1 => Ok(self.u8()? as u32),
            2 => Ok(self.u16()? as u32),
            _ => self.u32(),
        }
    }
}

/// UTF-16LE text, with its terminator and anything after it dropped, control characters removed and
/// cut to `MAX_TEXT` characters.
fn clean_text(d: &[u8]) -> String {
    let units: Vec<u16> = d.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).take_while(|&u| u != 0).collect();
    String::from_utf16_lossy(&units).chars().filter(|c| !c.is_control()).take(MAX_TEXT).collect()
}

/// What the Simple Index object says: the time step, and the packet number for each step.
struct SimpleIndex {
    /// The step in milliseconds.
    interval_ms: u64,
    packets: Vec<u32>,
}

/// The header's File Properties.
#[derive(Default)]
struct FileProps {
    play_duration: u64,
    preroll: u64,
    flags: u32,
    min_packet: u32,
    max_packet: u32,
}

/// One stream of the header, before it is decided whether it is played.
struct HeaderStream {
    number: u8,
    kind: [u8; 16],
    type_specific: Vec<u8>,
    /// The audio spread's virtual packet and chunk sizes, when the error correction data is long enough to hold it.
    spread: Option<[u16; 2]>,
    span: u8,
}

/// What a header holds that is not a stream.
#[derive(Default)]
struct Header {
    props: FileProps,
    streams: Vec<HeaderStream>,
    /// The bit rate each stream number is said to have, from the Extended Stream Properties.
    ext_bit_rate: Vec<(u8, u32)>,
    /// The same, from the Stream Bitrate Properties object.
    stream_bit_rate: Vec<(u8, u32)>,
    /// (stream type: 1 video, 2 audio, name) of the Codec List, in order.
    codecs: Vec<(u16, String)>,
    meta: Vec<(String, String)>,
}

fn parse_header(body: &[u8], count: u32) -> Result<Header> {
    let mut h = Header::default();
    let mut c = Cur::new(body);
    let mut seen = 0u32;
    while c.left() >= 24 && seen < count {
        seen += 1;
        let id = c.guid()?;
        let size = c.u64()?;
        let len = size.checked_sub(24).filter(|&l| l <= c.left() as u64).ok_or_else(|| bad("a header object is larger than the header"))? as usize;
        let obj = c.take(len)?;
        match id {
            FILE_PROPERTIES => h.props = parse_file_properties(obj)?,
            STREAM_PROPERTIES => h.streams.push(parse_stream_properties(obj)?),
            HEADER_EXTENSION => parse_header_extension(obj, &mut h)?,
            CONTENT_DESCRIPTION => parse_content_description(obj, &mut h.meta)?,
            EXTENDED_CONTENT_DESCRIPTION => parse_extended_content(obj, &mut h.meta)?,
            CODEC_LIST => parse_codec_list(obj, &mut h.codecs)?,
            STREAM_BITRATE_PROPERTIES => parse_stream_bitrates(obj, &mut h.stream_bit_rate)?,
            _ => {}
        }
    }
    Ok(h)
}

fn parse_file_properties(d: &[u8]) -> Result<FileProps> {
    let mut c = Cur::new(d);
    c.take(16 + 8 + 8 + 8)?; // file id, file size, creation date, packet count
    let play_duration = c.u64()?;
    c.u64()?; // send duration
    let preroll = c.u64()?;
    let flags = c.u32()?;
    let min_packet = c.u32()?;
    let max_packet = c.u32()?;
    Ok(FileProps { play_duration, preroll, flags, min_packet, max_packet })
}

fn parse_stream_properties(d: &[u8]) -> Result<HeaderStream> {
    let mut c = Cur::new(d);
    let kind = c.guid()?;
    c.guid()?; // the error correction type
    c.u64()?; // time offset
    let ts_len = c.u32()? as usize;
    let ec_len = c.u32()? as usize;
    let flags = c.u16()?;
    c.u32()?; // reserved
    if ts_len > c.left() {
        return Err(bad("a stream's type-specific data is longer than its object"));
    }
    let type_specific = c.take(ts_len)?.to_vec();
    let ec = c.take(ec_len.min(c.left()))?;
    // the audio spread: span, virtual packet size, virtual chunk size, silence data size, silence data
    let (mut span, mut spread) = (0, None);
    if ec.len() >= 8 {
        let mut e = Cur::new(ec);
        span = e.u8()?;
        let packet = e.u16()?;
        let chunk = e.u16()?;
        spread = Some([packet, chunk]);
    }
    Ok(HeaderStream { number: (flags & 0x7F) as u8, kind, type_specific, spread, span })
}

fn parse_header_extension(d: &[u8], h: &mut Header) -> Result<()> {
    let mut c = Cur::new(d);
    c.take(16 + 2)?;
    let size = c.u32()? as usize;
    let data = c.take(size.min(c.left()))?;
    let mut c = Cur::new(data);
    while c.left() >= 24 {
        let id = c.guid()?;
        let size = c.u64()?;
        let Some(len) = size.checked_sub(24).filter(|&l| l <= c.left() as u64) else { break };
        let obj = c.take(len as usize)?;
        if id == EXTENDED_STREAM_PROPERTIES && obj.len() >= 50 {
            let rate = u32::from_le_bytes(obj[16..20].try_into().unwrap());
            let number = u16::from_le_bytes([obj[48], obj[49]]);
            h.ext_bit_rate.push(((number & 0x7F) as u8, rate));
        }
    }
    Ok(())
}

fn parse_content_description(d: &[u8], meta: &mut Vec<(String, String)>) -> Result<()> {
    let mut c = Cur::new(d);
    let lens: Vec<usize> = (0..5).map(|_| c.u16().map(|l| l as usize)).collect::<Result<_>>()?;
    for (name, len) in ["title", "author", "copyright", "description", "rating"].into_iter().zip(lens) {
        let text = clean_text(c.take(len.min(c.left()))?);
        if !text.is_empty() {
            meta.push((name.into(), text));
        }
    }
    Ok(())
}

fn parse_extended_content(d: &[u8], meta: &mut Vec<(String, String)>) -> Result<()> {
    let mut c = Cur::new(d);
    let count = c.u16()?;
    for _ in 0..count {
        let name_len = c.u16()? as usize;
        let name = clean_text(c.take(name_len)?);
        let kind = c.u16()?;
        let len = c.u16()? as usize;
        let value = c.take(len)?;
        let text = match kind {
            0 => clean_text(value),
            2 if value.len() >= 4 => (u32::from_le_bytes(value[..4].try_into().unwrap()) != 0).to_string(),
            3 if value.len() >= 4 => u32::from_le_bytes(value[..4].try_into().unwrap()).to_string(),
            4 if value.len() >= 8 => u64::from_le_bytes(value[..8].try_into().unwrap()).to_string(),
            5 if value.len() >= 2 => u16::from_le_bytes(value[..2].try_into().unwrap()).to_string(),
            _ => continue,
        };
        if !name.is_empty() && !text.is_empty() {
            meta.push((name, text));
        }
    }
    Ok(())
}

fn parse_codec_list(d: &[u8], codecs: &mut Vec<(u16, String)>) -> Result<()> {
    let mut c = Cur::new(d);
    c.guid()?;
    let count = c.u32()?;
    for _ in 0..count.min(256) {
        let kind = c.u16()?;
        let name = c.u16()? as usize * 2;
        let name = clean_text(c.take(name)?);
        let desc = c.u16()? as usize * 2;
        c.take(desc)?;
        let info = c.u16()? as usize;
        c.take(info)?;
        codecs.push((kind, name));
    }
    Ok(())
}

fn parse_stream_bitrates(d: &[u8], out: &mut Vec<(u8, u32)>) -> Result<()> {
    let mut c = Cur::new(d);
    let count = c.u16()?;
    for _ in 0..count {
        let flags = c.u16()?;
        let rate = c.u32()?;
        out.push(((flags & 0x7F) as u8, rate));
    }
    Ok(())
}

fn parse_simple_index(d: &[u8]) -> Result<SimpleIndex> {
    let mut c = Cur::new(d);
    c.guid()?; // file id
    let interval = c.u64()?;
    c.u32()?; // the most packets an entry counts
    let count = c.u32()? as usize;
    // each entry is 6 bytes: the count is cut to what the object holds
    let count = count.min(c.left() / 6);
    let mut packets = Vec::with_capacity(count);
    for _ in 0..count {
        packets.push(c.u32()?);
        c.u16()?;
    }
    Ok(SimpleIndex { interval_ms: (interval / 10_000).max(1), packets })
}

/// A short name for a FourCC of a video stream.
fn video_name(tag: [u8; 4]) -> String {
    let text: String = tag.iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '?' }).collect();
    let name = match &text.to_ascii_uppercase()[..] {
        "WMV1" => "WMV1 (Windows Media Video 7)",
        "WMV2" => "WMV2 (Windows Media Video 8)",
        "WMV3" => "WMV3 (Windows Media Video 9)",
        "WMVA" | "WVC1" => "VC-1 (Windows Media Video 9 Advanced Profile)",
        "WMVP" | "WVP2" => "WMV9 Image",
        "MP41" | "MPG4" | "DIV1" => "MPEG-4 version 1 (MS MPEG-4 v1)",
        "MP42" | "DIV2" => "MPEG-4 version 2 (MS MPEG-4 v2)",
        "MP43" | "DIV3" => "MPEG-4 version 3 (MS MPEG-4 v3)",
        "MP4S" | "M4S2" | "MP4V" | "XVID" | "DIVX" | "FMP4" => "MPEG-4 Part 2",
        "H264" | "AVC1" => "H.264",
        "MJPG" => "Motion JPEG",
        _ => return format!("FourCC {text}"),
    };
    name.to_string()
}

/// A short name for the format tag of an audio stream.
fn audio_name(tag: u16) -> String {
    match tag {
        0x0001 => "PCM".into(),
        0x0050 => "MPEG audio layer 2".into(),
        0x0055 => "MP3".into(),
        0x000A | 0x000B => "WMA Voice".into(),
        0x0160 => "WMA 1".into(),
        0x0161 => "WMA 2".into(),
        0x0162 => "WMA Pro".into(),
        0x0163 => "WMA Lossless".into(),
        0x00FF | 0x1610 => "AAC".into(),
        0x2000 => "AC-3".into(),
        t => format!("format tag 0x{t:04X}"),
    }
}

/// How one stream is read: its number, the index of its stream in `streams`, and the audio
/// spread (descrambling) it uses.
struct Route {
    stream: usize,
    /// The span, virtual packet size and chunk size of the audio spread, when the span is above 1.
    spread: Option<(usize, usize, usize)>,
    /// The packet duration to report, in milliseconds.
    duration: i64,
}

/// A media object being put together from fragments.
#[derive(Default)]
struct Partial {
    active: bool,
    number: u32,
    size: usize,
    pts: i64,
    data: Vec<u8>,
}

/// The ASF reader.
pub struct AsfDemuxer<R: Read + Seek + Send> {
    r: BufReader<R>,
    /// Where the reader is, to avoid seeking to where it already is.
    cur: u64,
    streams: Vec<StreamInfo>,
    routes: Vec<Option<Route>>,
    partial: Vec<Partial>,
    discarded: Vec<bool>,
    unsupported: Vec<String>,
    meta: Vec<(String, String)>,
    preroll: i64,
    packet_size: usize,
    min_packet: usize,
    fixed: bool,
    data_start: u64,
    data_end: u64,
    /// Where the next data packet is.
    pos: u64,
    index: Option<SimpleIndex>,
    ready: VecDeque<Packet>,
}

/// What a data packet's header says.
struct PacketHeader {
    /// The packet's length, padding included.
    length: usize,
    /// The end of the payload data, before the padding.
    content_end: usize,
    send_time: u32,
    multiple: bool,
    payload_length_type: u8,
    property: u8,
    /// Where the payloads begin.
    payload_start: usize,
}

fn parse_packet_header(buf: &[u8], packet_size: usize, min_packet: usize) -> Result<PacketHeader> {
    let mut c = Cur::new(buf);
    let first = c.u8()?;
    let flags = if first & 0x80 != 0 {
        // error correction data
        if first & 0x60 != 0 {
            return Err(bad("a packet's error correction data is of an unknown kind"));
        }
        c.take((first & 0x0F) as usize)?;
        c.u8()?
    } else {
        first
    };
    let property = c.u8()?;
    let length = c.var(flags >> 5, packet_size as u32)? as u64;
    c.var(flags >> 1, 0)?; // the sequence number
    let padding = c.var(flags >> 3, 0)? as u64;
    let send_time = c.u32()?;
    c.u16()?; // the duration
    let multiple = flags & 1 != 0;
    let mut payload_length_type = 0;
    if multiple {
        payload_length_type = c.u8()? >> 6;
    }
    if length == 0 || length >= MAX_PACKET {
        return Err(bad(format!("a data packet's length is {length}")));
    }
    if padding >= length {
        return Err(bad(format!("a data packet's padding ({padding}) is as long as the packet ({length})")));
    }
    let content_end = (length - padding) as usize;
    if content_end < c.pos {
        return Err(bad("a data packet is shorter than its own header"));
    }
    Ok(PacketHeader { length: (length as usize).max(min_packet), content_end, send_time, multiple, payload_length_type, property, payload_start: c.pos })
}

/// The number of payloads a multiple-payload packet declares.
fn payload_count(buf: &[u8], h: &PacketHeader) -> usize {
    if !h.multiple {
        return 1;
    }
    // the payload flags byte is the last byte before the payloads
    (buf[h.payload_start - 1] & 0x3F) as usize
}

/// Opens an ASF file for reading.
pub fn open<R: Read + Seek + Send>(mut r: R) -> Result<AsfDemuxer<R>> {
    let total = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let mut r = BufReader::with_capacity(64 << 10, r);
    let mut head = [0u8; 30];
    r.read_exact(&mut head).map_err(|_| bad("the file is shorter than its header object"))?;
    if !looks_like_asf(&head) {
        return Err(bad("not an ASF file"));
    }
    let header_size = u64::from_le_bytes(head[16..24].try_into().unwrap());
    let count = u32::from_le_bytes(head[24..28].try_into().unwrap());
    if header_size < 30 || header_size > total || header_size > MAX_HEADER {
        return Err(bad(format!("the header object's size ({header_size}) does not fit the file")));
    }
    let mut body = vec![0u8; (header_size - 30) as usize];
    r.read_exact(&mut body).map_err(|_| bad("the header object is cut short"))?;
    let h = parse_header(&body, count)?;
    // the data object
    let mut dhead = [0u8; 50];
    r.read_exact(&mut dhead).map_err(|_| bad("the file has no data object after its header"))?;
    if dhead[..16] != DATA {
        return Err(bad("the object after the header is not the data object"));
    }
    let data_size = u64::from_le_bytes(dhead[16..24].try_into().unwrap());
    let data_start = header_size + 50;
    // a size that is 0, too small or past the end of the file (a live recording) means "to the end"
    let data_end = if data_size >= 50 && header_size.saturating_add(data_size) <= total { header_size + data_size } else { total };

    let props = &h.props;
    if props.max_packet == 0 || props.min_packet > props.max_packet {
        return Err(bad(format!("the file's packet sizes ({} to {}) are not usable", props.min_packet, props.max_packet)));
    }
    if props.max_packet as u64 >= MAX_PACKET {
        return Err(bad(format!("the file's packet size ({}) is absurd", props.max_packet)));
    }
    let preroll = props.preroll.min(i64::MAX as u64 / 20_000) as i64;

    // the streams
    let mut streams = Vec::new();
    let mut routes: Vec<Option<Route>> = (0..128).map(|_| None).collect();
    let mut unsupported = Vec::new();
    let (mut n_audio, mut n_video) = (0, 0);
    for hs in &h.streams {
        let rate = |hs: &HeaderStream| {
            let ext = h.ext_bit_rate.iter().find(|e| e.0 == hs.number).map(|e| e.1);
            let sb = h.stream_bit_rate.iter().find(|e| e.0 == hs.number).map(|e| e.1);
            ext.or(sb).unwrap_or(0)
        };
        let ts = &hs.type_specific[..];
        if hs.kind == AUDIO_MEDIA {
            n_audio += 1;
            let codec_name = h.codecs.iter().filter(|c| c.0 == 2).nth(n_audio - 1).map(|c| c.1.as_str()).filter(|n| !n.is_empty());
            let described = |what: String| match codec_name {
                Some(n) => format!("audio stream {}: {what} ({n})", hs.number),
                None => format!("audio stream {}: {what}", hs.number),
            };
            let Ok(wf) = parse_waveformat(ts) else {
                unsupported.push(described("a damaged audio format description".into()));
                continue;
            };
            let codec = match wf.tag {
                0x0160 => CodecId::WmaV1,
                0x0161 => CodecId::WmaV2,
                t => {
                    unsupported.push(described(audio_name(t)));
                    continue;
                }
            };
            let (channels, sample_rate) = (wf.channels, wf.sample_rate);
            if channels == 0 || channels > 2 || sample_rate == 0 || sample_rate > 50000 || wf.block_align == 0 {
                unsupported.push(described(format!("{} with {channels} channel(s) at {sample_rate} Hz, block align {}", audio_name(wf.tag), wf.block_align)));
                continue;
            }
            let mut extradata = ts.to_vec();
            if wf.avg_bytes_per_sec == 0 {
                // no byte rate in the format: the bit rate the header gives elsewhere
                let bytes = rate(hs) / 8;
                if bytes == 0 {
                    unsupported.push(described(format!("{} with no bit rate", audio_name(wf.tag))));
                    continue;
                }
                extradata[8..12].copy_from_slice(&bytes.to_le_bytes());
            }
            let avg = u32::from_le_bytes(extradata[8..12].try_into().unwrap()).max(1);
            let duration = (props.play_duration / 10_000) as i64 - preroll;
            let index = streams.len();
            let spread = match hs.spread {
                Some([packet, chunk]) if hs.span > 1 && chunk > 0 && packet / chunk > 1 && packet % chunk == 0 => Some((hs.span as usize, packet as usize, chunk as usize)),
                _ => None,
            };
            routes[hs.number as usize] = Some(Route { stream: index, spread, duration: (wf.block_align as i64 * 1000 + avg as i64 / 2) / avg as i64 });
            streams.push(StreamInfo {
                index,
                codec,
                time_base: Rational::new(1, 1000),
                duration: (props.flags & 1 == 0 && duration > 0).then_some(duration),
                bits: None,
                extradata,
                lead_in: 0,
                params: StreamParams::Audio { sample_rate, channels },
            });
        } else if hs.kind == VIDEO_MEDIA {
            n_video += 1;
            let codec_name = h.codecs.iter().filter(|c| c.0 == 1).nth(n_video - 1).map(|c| c.1.as_str()).filter(|n| !n.is_empty());
            // the type-specific data: width, height, reserved byte, format data size, then the BITMAPINFOHEADER
            let what = if ts.len() >= 11 + 20 {
                let (w, hgt) = (u32::from_le_bytes(ts[..4].try_into().unwrap()), u32::from_le_bytes(ts[4..8].try_into().unwrap()));
                let fourcc: [u8; 4] = ts[11 + 16..11 + 20].try_into().unwrap();
                format!("{} {w}x{hgt}", video_name(fourcc))
            } else {
                "a damaged video format description".to_string()
            };
            unsupported.push(match codec_name {
                Some(n) => format!("video stream {}: {what} ({n})", hs.number),
                None => format!("video stream {}: {what}", hs.number),
            });
        } else {
            let what = match hs.kind {
                COMMAND_MEDIA => "script commands",
                JFIF_MEDIA => "JPEG images",
                BINARY_MEDIA => "binary data",
                _ => "an unknown kind of data",
            };
            unsupported.push(format!("stream {}: {what}", hs.number));
        }
    }
    if streams.is_empty() {
        let what = if unsupported.is_empty() { "the file has no streams".to_string() } else { unsupported.join("; ") };
        return Err(Error::new(ErrorCode::UnsupportedCodec, format!("ASF: no stream IronAV can play ({what})")).hint("ASF files with WMA 1 or WMA 2 audio are read"));
    }

    // the index, after the data object
    let mut index = None;
    let mut at = data_end;
    for _ in 0..MAX_TRAILING_OBJECTS {
        if at.saturating_add(24) > total || r.seek(SeekFrom::Start(at)).is_err() {
            break;
        }
        let mut oh = [0u8; 24];
        if r.read_exact(&mut oh).is_err() {
            break;
        }
        let size = u64::from_le_bytes(oh[16..24].try_into().unwrap());
        if size < 24 || at.saturating_add(size) > total {
            break;
        }
        if oh[..16] == SIMPLE_INDEX && size <= MAX_HEADER && index.is_none() {
            let mut obj = vec![0u8; (size - 24) as usize];
            if r.read_exact(&mut obj).is_ok() {
                index = parse_simple_index(&obj).ok();
            }
        }
        at += size;
    }

    let n = streams.len();
    let packet_size = props.max_packet as usize;
    let min_packet = props.min_packet as usize;
    r.seek(SeekFrom::Start(data_start))?;
    Ok(AsfDemuxer {
        r,
        cur: data_start,
        streams,
        routes,
        partial: (0..n).map(|_| Partial::default()).collect(),
        discarded: vec![false; n],
        unsupported,
        meta: h.meta,
        preroll,
        packet_size,
        min_packet,
        fixed: props.min_packet == props.max_packet,
        data_start,
        data_end,
        pos: data_start,
        index,
        ready: VecDeque::new(),
    })
}

/// The file's title, author, copyright, description and rating and any extended content
/// description (as name, value pairs). It is text from the file, so untrusted: control characters
/// are removed and each value is cut to 512 characters.
pub fn metadata(path: &Path) -> Result<Vec<(String, String)>> {
    Ok(open(std::fs::File::open(path)?)?.meta)
}

impl<R: Read + Seek + Send> AsfDemuxer<R> {
    /// Reads up to `n` bytes at `at`, fewer at the end of the file.
    fn read_at(&mut self, at: u64, n: usize) -> Result<Vec<u8>> {
        if self.cur != at {
            self.r.seek(SeekFrom::Start(at))?;
        }
        let mut buf = Vec::new();
        let got = (&mut self.r).take(n as u64).read_to_end(&mut buf)?;
        self.cur = at + got as u64;
        Ok(buf)
    }

    /// Adds a finished object to the ready queue.
    fn emit(&mut self, stream: usize, pts: i64, data: Vec<u8>) {
        let duration = self.routes.iter().flatten().find(|r| r.stream == stream).map_or(0, |r| r.duration);
        self.ready.push_back(Packet { stream, pts, duration, data });
    }

    /// Descrambles an object of an audio spread stream, as FFmpeg does.
    fn descramble(spread: (usize, usize, usize), data: Vec<u8>) -> Vec<u8> {
        let (span, packet, chunk) = spread;
        if data.len() != packet * span {
            return data;
        }
        let mut out = vec![0u8; data.len()];
        for (i, dst) in out.chunks_exact_mut(chunk).enumerate() {
            let (row, col) = (i / span, i % span);
            let idx = row + col * packet / chunk;
            if let Some(src) = data.get(idx * chunk..(idx + 1) * chunk) {
                dst.copy_from_slice(src);
            }
        }
        out
    }

    /// Parses the data packet in `buf`, which holds `h`'s header, queueing the objects it completes.
    fn parse_payloads(&mut self, buf: &[u8], h: &PacketHeader) -> Result<()> {
        let mut c = Cur::new(&buf[..h.content_end.min(buf.len())]);
        c.pos = h.payload_start;
        let count = payload_count(buf, h);
        let mut done = 0;
        while done < count && c.left() > 0 {
            done += 1;
            let sn = c.u8()?;
            let number = (sn & 0x7F) as usize;
            let object = c.var(h.property >> 4, 0)?;
            let offset = c.var(h.property >> 2, 0)? as usize;
            let rep_len = c.var(h.property, 0)? as usize;
            let (mut object_size, mut presentation, mut delta) = (None, None, 0u32);
            match rep_len {
                0 => {}
                1 => delta = c.u8()? as u32,
                2..=7 => return Err(bad(format!("a payload's replicated data is {rep_len} bytes"))),
                _ => {
                    let rep = c.take(rep_len)?;
                    object_size = Some(u32::from_le_bytes(rep[..4].try_into().unwrap()) as u64);
                    presentation = Some(u32::from_le_bytes(rep[4..8].try_into().unwrap()));
                }
            }
            let len = if h.multiple {
                if h.payload_length_type == 0 {
                    return Err(bad("a payload has no length"));
                }
                c.var(h.payload_length_type, 0)? as usize
            } else {
                c.left()
            };
            if len > c.left() {
                return Err(bad(format!("a payload of {len} bytes runs past the end of its packet ({} left)", c.left())));
            }
            let data = c.take(len)?;
            let Some(route) = self.routes.get(number).and_then(|r| r.as_ref()) else { continue };
            let stream = route.stream;
            let spread = route.spread;
            if self.discarded[stream] {
                continue;
            }
            if rep_len == 1 {
                // a compressed payload: the offset field is the time of the first object, then each object is a length byte and data
                let mut d = Cur::new(data);
                let mut time = offset as i64;
                while d.left() > 0 {
                    let n = d.u8()? as usize;
                    let obj = d.take(n)?.to_vec();
                    self.emit(stream, time - self.preroll, obj);
                    time += delta as i64;
                }
                continue;
            }
            let size = match object_size {
                Some(s) => s,
                None => {
                    if offset != 0 {
                        continue;
                    }
                    data.len() as u64
                }
            };
            if size > MAX_OBJECT {
                return Err(bad(format!("a media object of {size} bytes")));
            }
            let size = size as usize;
            if offset.checked_add(data.len()).is_none_or(|end| end > size) {
                self.partial[stream].active = false;
                return Err(bad(format!("a fragment of {} bytes at {offset} lies outside its media object of {size} bytes", data.len())));
            }
            let pts = presentation.map_or(h.send_time as i64, |p| p as i64) - self.preroll;
            let p = &mut self.partial[stream];
            if offset == 0 {
                *p = Partial { active: true, number: object, size, pts, data: data.to_vec() };
            } else if p.active && p.number == object && p.size == size && p.data.len() == offset {
                p.data.extend_from_slice(data);
            } else if p.active && p.number == object && offset < p.data.len() {
                p.active = false;
                return Err(bad(format!("a fragment at {offset} overlaps the {} bytes of its media object already read", p.data.len())));
            } else {
                // the start of this object was not seen (after a seek): its fragments are skipped
                p.active = false;
                continue;
            }
            if p.active && p.data.len() == size {
                p.active = false;
                let pts = p.pts;
                let mut obj = std::mem::take(&mut p.data);
                if let Some(sp) = spread {
                    obj = Self::descramble(sp, obj);
                }
                self.emit(stream, pts, obj);
            }
        }
        Ok(())
    }

    /// Reads the data packet at `self.pos`: the next ones' objects go to the ready queue. Returns
    /// false at the end of the data.
    fn read_data_packet(&mut self) -> Result<bool> {
        if self.pos >= self.data_end {
            return Ok(false);
        }
        let want = (self.packet_size as u64).min(self.data_end - self.pos) as usize;
        let at = self.pos;
        let mut buf = self.read_at(at, want)?;
        if buf.is_empty() {
            self.pos = self.data_end;
            return Ok(false);
        }
        let h = match parse_packet_header(&buf, self.packet_size, self.min_packet) {
            Ok(h) => h,
            Err(e) => {
                // skip a packet of the usual size
                self.pos = at + self.packet_size as u64;
                return Err(e);
            }
        };
        if h.length > buf.len() {
            // a packet longer than the usual size: read the rest
            let more = self.read_at(at + buf.len() as u64, h.length - buf.len())?;
            buf.extend_from_slice(&more);
        }
        self.pos = at + h.length as u64;
        if h.content_end > buf.len() {
            self.pos = self.data_end;
            return Err(bad("the last data packet is cut short (truncated file?)"));
        }
        self.parse_payloads(&buf, &h)?;
        Ok(true)
    }

    /// The send time of data packet number `n`.
    fn send_time(&mut self, n: u64) -> Result<u32> {
        let at = self.data_start + n * self.packet_size as u64;
        let buf = self.read_at(at, 64.min(self.packet_size))?;
        Ok(parse_packet_header(&buf, self.packet_size, self.min_packet).map_err(|e| e.hint("seeking bisects the packets by send time"))?.send_time)
    }

    /// The packet to start a seek to `file_ms` (milliseconds, on the clock of the packets' send times).
    fn seek_start(&mut self, file_ms: u64) -> Result<u64> {
        let packets = (self.data_end - self.data_start) / self.packet_size as u64;
        if packets == 0 {
            return Ok(0);
        }
        if let Some(ix) = &self.index {
            if !ix.packets.is_empty() {
                let entry = ((file_ms / ix.interval_ms) as usize).min(ix.packets.len() - 1);
                return Ok((ix.packets[entry] as u64).min(packets - 1));
            }
        }
        // the last packet sent at or before the time
        let (mut lo, mut hi) = (0u64, packets);
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if self.send_time(mid)? as u64 <= file_ms {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }
}

impl<R: Read + Seek + Send> Demuxer for AsfDemuxer<R> {
    fn format_name(&self) -> &'static str {
        "asf"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(p) = self.ready.pop_front() {
                return Ok(Some(p));
            }
            if !self.read_data_packet()? {
                return Ok(None);
            }
        }
    }

    fn discard(&mut self, stream: usize) {
        if let Some(d) = self.discarded.get_mut(stream) {
            *d = true;
        }
    }

    /// Lands on the last object at or before `seconds` of each stream (every WMA object can be decoded
    /// from its start, but its overlap with the one before is lost, so a decoder is reset).
    fn seek(&mut self, seconds: f64) -> Result<f64> {
        if !self.fixed {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "ASF files with packets of varying size cannot seek"));
        }
        let target_ms = (seconds.max(0.0) * 1000.0).min(1e12) as i64;
        // the packets and the index are timed by send time, which is on the stream's clock (presentation time less preroll)
        let file_ms = target_ms.max(0) as u64;
        let mut start = self.seek_start(file_ms)?;
        // one packet earlier, for an object whose start is in the packet before
        start = start.saturating_sub(1);
        let mut attempt = 0;
        loop {
            self.pos = self.data_start + start * self.packet_size as u64;
            self.ready.clear();
            for p in &mut self.partial {
                p.active = false;
            }
            // read on to the first object after the time, keeping the last at or before it for each stream
            let n = self.streams.len();
            let mut last: Vec<Option<Packet>> = (0..n).map(|_| None).collect();
            let mut after: Vec<Option<Packet>> = (0..n).map(|_| None).collect();
            let mut open = (0..n).filter(|&s| !self.discarded[s]).count();
            let mut errors = 0;
            while open > 0 {
                let p = match self.read_packet() {
                    Ok(Some(p)) => p,
                    // a damaged packet is stepped over, the end ends the search
                    Err(_) if self.pos < self.data_end && errors < 1000 => {
                        errors += 1;
                        continue;
                    }
                    _ => break,
                };
                let s = p.stream;
                if after[s].is_some() {
                    continue;
                }
                if p.pts <= target_ms {
                    last[s] = Some(p);
                } else {
                    after[s] = Some(p);
                    open -= 1;
                }
            }
            if last.iter().any(Option::is_some) || start == 0 || attempt > 0 {
                let mut kept: Vec<Packet> = last.into_iter().chain(after).flatten().collect();
                kept.sort_by_key(|p| p.pts);
                let landing = kept.iter().filter(|p| p.pts <= target_ms).map(|p| p.pts).min().or_else(|| kept.first().map(|p| p.pts)).unwrap_or(target_ms);
                // the packets still queued came from the packets just read; the kept ones go first
                let rest: VecDeque<Packet> = std::mem::take(&mut self.ready);
                self.ready = kept.into_iter().collect();
                self.ready.extend(rest);
                return Ok(landing.max(0) as f64 / 1000.0);
            }
            // the index pointed past the time: look again by bisection
            attempt += 1;
            self.index = None;
            start = self.seek_start(file_ms)?.saturating_sub(1);
        }
    }

    fn unsupported_streams(&self) -> Vec<String> {
        self.unsupported.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn object(id: [u8; 16], body: &[u8]) -> Vec<u8> {
        let mut o = id.to_vec();
        o.extend_from_slice(&(24 + body.len() as u64).to_le_bytes());
        o.extend_from_slice(body);
        o
    }

    fn file_properties(packet: u32, preroll: u64) -> Vec<u8> {
        let mut b = vec![0u8; 16 + 8 + 8 + 8];
        b.extend_from_slice(&30_000_000u64.to_le_bytes());
        b.extend_from_slice(&30_000_000u64.to_le_bytes());
        b.extend_from_slice(&preroll.to_le_bytes());
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&packet.to_le_bytes());
        b.extend_from_slice(&packet.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        object(FILE_PROPERTIES, &b)
    }

    fn wma_stream(number: u16) -> Vec<u8> {
        let mut wf = Vec::new();
        wf.extend_from_slice(&0x161u16.to_le_bytes());
        wf.extend_from_slice(&2u16.to_le_bytes());
        wf.extend_from_slice(&44100u32.to_le_bytes());
        wf.extend_from_slice(&16000u32.to_le_bytes());
        wf.extend_from_slice(&743u16.to_le_bytes());
        wf.extend_from_slice(&16u16.to_le_bytes());
        wf.extend_from_slice(&10u16.to_le_bytes());
        wf.extend_from_slice(&[0, 0, 0, 0, 0x0d, 0, 0, 0, 0, 0]);
        let mut b = AUDIO_MEDIA.to_vec();
        b.extend_from_slice(&[0u8; 16]);
        b.extend_from_slice(&0u64.to_le_bytes());
        b.extend_from_slice(&(wf.len() as u32).to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&number.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&wf);
        object(STREAM_PROPERTIES, &b)
    }

    fn video_stream(number: u16, fourcc: &[u8; 4]) -> Vec<u8> {
        let mut ts = Vec::new();
        ts.extend_from_slice(&320u32.to_le_bytes());
        ts.extend_from_slice(&240u32.to_le_bytes());
        ts.push(2);
        ts.extend_from_slice(&40u16.to_le_bytes());
        let mut bmi = vec![0u8; 40];
        bmi[..4].copy_from_slice(&40u32.to_le_bytes());
        bmi[16..20].copy_from_slice(fourcc);
        ts.extend_from_slice(&bmi);
        let mut b = VIDEO_MEDIA.to_vec();
        b.extend_from_slice(&[0u8; 16]);
        b.extend_from_slice(&0u64.to_le_bytes());
        b.extend_from_slice(&(ts.len() as u32).to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&number.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&ts);
        object(STREAM_PROPERTIES, &b)
    }

    fn file(objects: &[Vec<u8>], packets: &[Vec<u8>], packet: usize) -> Vec<u8> {
        let body: Vec<u8> = objects.concat();
        let mut f = HEADER.to_vec();
        f.extend_from_slice(&(30 + body.len() as u64).to_le_bytes());
        f.extend_from_slice(&(objects.len() as u32).to_le_bytes());
        f.extend_from_slice(&[1, 2]);
        f.extend_from_slice(&body);
        let mut data = DATA.to_vec();
        data.extend_from_slice(&(50 + (packets.len() * packet) as u64).to_le_bytes());
        data.extend_from_slice(&[0u8; 16]);
        data.extend_from_slice(&(packets.len() as u64).to_le_bytes());
        data.extend_from_slice(&[1, 1]);
        f.extend_from_slice(&data);
        for p in packets {
            let mut p = p.clone();
            p.resize(packet, 0);
            f.extend_from_slice(&p);
        }
        f
    }

    /// A packet with one payload per entry of `payloads` as (stream, object, offset, object size, time, data).
    fn packet(payloads: &[(u8, u8, u32, u32, u32, &[u8])]) -> Vec<u8> {
        let mut p = vec![0x82, 0, 0];
        // multiple payloads, padding as a byte, no explicit length; word payload lengths
        p.push(0x09);
        p.push(0x5D);
        p.push(0); // padding
        p.extend_from_slice(&0u32.to_le_bytes());
        p.extend_from_slice(&0u16.to_le_bytes());
        p.push(0x80 | payloads.len() as u8);
        for &(stream, obj, offset, size, time, data) in payloads {
            p.push(stream);
            p.push(obj);
            p.extend_from_slice(&offset.to_le_bytes());
            p.push(8);
            p.extend_from_slice(&size.to_le_bytes());
            p.extend_from_slice(&time.to_le_bytes());
            p.extend_from_slice(&(data.len() as u16).to_le_bytes());
            p.extend_from_slice(data);
        }
        p
    }

    fn open_bytes(f: Vec<u8>) -> Result<AsfDemuxer<Cursor<Vec<u8>>>> {
        open(Cursor::new(f))
    }

    fn basic(packets: &[Vec<u8>]) -> Vec<u8> {
        file(&[file_properties(256, 0), wma_stream(1)], packets, 256)
    }

    #[test]
    fn objects_come_whole_and_in_fragments() {
        let f = basic(&[packet(&[(1, 0, 0, 4, 100, b"abcd"), (1, 1, 0, 6, 120, b"ab")]), packet(&[(1, 1, 2, 6, 120, b"cdef")])]);
        let mut d = open_bytes(f).unwrap();
        let a = d.read_packet().unwrap().unwrap();
        assert_eq!((a.pts, &a.data[..]), (100, &b"abcd"[..]));
        let b = d.read_packet().unwrap().unwrap();
        assert_eq!((b.pts, &b.data[..]), (120, &b"abcdef"[..]));
        assert!(d.read_packet().unwrap().is_none());
    }

    #[test]
    fn a_fragment_whose_start_was_never_seen_is_skipped() {
        let f = basic(&[packet(&[(1, 1, 2, 6, 120, b"cdef")]), packet(&[(1, 2, 0, 3, 140, b"xyz")])]);
        let mut d = open_bytes(f).unwrap();
        let a = d.read_packet().unwrap().unwrap();
        assert_eq!(&a.data[..], b"xyz");
    }

    #[test]
    fn overlapping_and_overlong_fragments_are_errors() {
        let f = basic(&[packet(&[(1, 1, 0, 6, 120, b"abcd")]), packet(&[(1, 1, 2, 6, 120, b"cdef")])]);
        let mut d = open_bytes(f).unwrap();
        assert!(d.read_packet().is_err());
        let f = basic(&[packet(&[(1, 1, 4, 6, 120, b"abcdef")])]);
        assert!(open_bytes(f).unwrap().read_packet().is_err());
    }

    #[test]
    fn a_payload_longer_than_its_packet_is_an_error() {
        let mut p = packet(&[(1, 0, 0, 4, 100, b"abcd")]);
        let n = p.len();
        p[n - 6] = 0xFF; // the payload length (a word before the four data bytes)
        p[n - 5] = 0x7F;
        let mut d = open_bytes(basic(&[p])).unwrap();
        assert!(d.read_packet().is_err());
        assert!(d.read_packet().unwrap().is_none());
    }

    #[test]
    fn absurd_object_sizes_do_not_allocate() {
        let f = basic(&[packet(&[(1, 0, 0, u32::MAX, 100, b"abcd")])]);
        assert!(open_bytes(f).unwrap().read_packet().is_err());
        let f = basic(&[packet(&[(1, 0, 0, 1 << 23, 100, b"abcd")])]);
        let mut d = open_bytes(f).unwrap();
        assert!(d.read_packet().unwrap().is_none());
    }

    #[test]
    fn compressed_payloads_are_split_by_their_length_bytes() {
        // multiple payloads; the property byte gives replicated data a length of one byte
        let mut p = vec![0x82, 0, 0, 0x01, 0x5D];
        p.extend_from_slice(&0u32.to_le_bytes());
        p.extend_from_slice(&0u16.to_le_bytes());
        p.push(0x40 | 1); // one payload, byte length
        p.push(1); // stream
        p.push(0); // object
        p.extend_from_slice(&1000u32.to_le_bytes()); // the time of the first
        p.push(1); // replicated data: one byte
        p.push(20); // the time step
        let body = [2u8, 1, 2, 3, 7, 8, 9];
        p.push(body.len() as u8);
        p.extend_from_slice(&body);
        let mut d = open_bytes(basic(&[p])).unwrap();
        let a = d.read_packet().unwrap().unwrap();
        let b = d.read_packet().unwrap().unwrap();
        assert_eq!((a.pts, &a.data[..]), (1000, &[1u8, 2][..]));
        assert_eq!((b.pts, &b.data[..]), (1020, &[7u8, 8, 9][..]));
    }

    #[test]
    fn video_is_listed_as_unsupported_and_audio_only_files_are_refused_when_nothing_plays() {
        let f = file(&[file_properties(256, 0), wma_stream(1), video_stream(2, b"WMV2")], &[], 256);
        let d = open_bytes(f).unwrap();
        assert_eq!(d.streams().len(), 1);
        assert_eq!(d.streams()[0].codec, CodecId::WmaV2);
        let u = d.unsupported_streams();
        assert_eq!(u.len(), 1);
        assert!(u[0].contains("WMV2") && u[0].contains("320x240"), "{u:?}");
        let f = file(&[file_properties(256, 0), video_stream(2, b"WVC1")], &[], 256);
        let e = open_bytes(f).err().unwrap();
        assert_eq!(e.code, ErrorCode::UnsupportedCodec);
        assert!(e.message.contains("VC-1"), "{}", e.message);
    }

    #[test]
    fn malformed_headers_are_errors() {
        let good = basic(&[packet(&[(1, 0, 0, 4, 100, b"abcd")])]);
        for cut in [0, 10, 29, 40, 100, 200] {
            assert!(open_bytes(good[..cut].to_vec()).is_err(), "cut at {cut}");
        }
        // an object larger than the header
        let mut f = good.clone();
        f[30 + 16..30 + 24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(open_bytes(f).is_err());
        // a header size that is absurd
        let mut f = good.clone();
        f[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(open_bytes(f).is_err());
        // a packet size of zero
        let f = file(&[file_properties(0, 0), wma_stream(1)], &[], 256);
        assert!(open_bytes(f).is_err());
        // not ASF at all
        assert!(open_bytes(vec![0u8; 100]).is_err());
    }

    #[test]
    fn a_truncated_last_packet_is_an_error_then_the_end() {
        let mut f = basic(&[packet(&[(1, 0, 0, 4, 100, b"abcd")]), packet(&[(1, 1, 0, 4, 120, b"efgh")])]);
        f.truncate(f.len() - 200);
        let mut d = open_bytes(f).unwrap();
        assert!(d.read_packet().unwrap().is_some());
        let r = d.read_packet();
        assert!(r.is_err() || r.unwrap().is_none());
    }

    #[test]
    fn text_from_the_file_is_cleaned() {
        let mut t: Vec<u8> = "ti\u{7}tle\u{1b}[31m".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        t.extend_from_slice(&[0, 0, 0x41, 0]);
        assert_eq!(clean_text(&t), "title[31m");
        let long: Vec<u8> = "x".repeat(2000).encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(clean_text(&long).len(), MAX_TEXT);
    }
}
