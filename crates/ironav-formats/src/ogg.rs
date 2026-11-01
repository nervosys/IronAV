//! Ogg reader (Opus, Vorbis and FLAC).
//!
//! The file is read whole and its pages are unwrapped into one buffer of packet bytes, so a packet
//! that continues over a page boundary is contiguous again. The packets are then handed to the MP4
//! reader's machinery (`Mp4Demuxer::from_parts`) over that buffer, which reads, interleaves and
//! seeks them.
//!
//! Every logical stream is read. Multiplexed streams (sharing the file in time, each with its own
//! serial number) are separate tracks. Chained streams (one link after another, each beginning when
//! the last one ends) of Vorbis or FLAC are joined into one track when their setup matches, so the
//! timeline runs on across the links. Opus links are never joined: the decoder removes the pre-skip
//! once per track, so each Opus link stays a track of its own.
//!
//! Timing comes from the packets themselves (an Opus packet knows its length; a Vorbis packet's
//! block size follows from its mode, see `PacketTimer`), starting at zero: the Opus decoder drops
//! the pre-skip announced by `OpusHead`. The granule position of the last page of each link, which
//! gives its true length, shortens the link's last packet (see `trim_end`); the decoders cut their
//! output to it.
//!
//! FLAC in Ogg is the native-FLAC mapping: one FLAC frame per packet, durations from the frame
//! headers, no end trim (a FLAC frame is exact).
//!
//! Not handled: Speex, Theora and other codecs in Ogg (refused, or passed over when they share the
//! file with a readable stream) and files too big to hold in memory (refused above 1 GiB).

use crate::mp4demux::{Config, Mp4Demuxer, Sample, TrackInfo};
use ironav_core::{CodecId, Error, ErrorCode, Rational, Result, StreamInfo, StreamParams};
use std::collections::HashMap;
use std::io::{Cursor, Read};

const MAX_FILE: u64 = 1 << 30;

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::invalid_input(format!("Ogg: {msg}"))
}

/// Whether `magic` starts with an Ogg page.
pub fn looks_like_ogg(magic: &[u8]) -> bool {
    magic.starts_with(b"OggS")
}

/// Ogg's CRC-32: polynomial 0x04C11DB7, not reflected, zero initial value, no final xor.
pub(crate) fn crc(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut r = (i as u32) << 24;
            for _ in 0..8 {
                r = if r & 0x8000_0000 != 0 { (r << 1) ^ 0x04C1_1DB7 } else { r << 1 };
            }
            *e = r;
        }
        t
    });
    data.iter().fold(0u32, |c, &b| (c << 8) ^ t[((c >> 24) as u8 ^ b) as usize])
}

/// Shorten the last packet so the stream ends at the final page's granule position (the sample
/// count since the start; for Opus that includes the pre-skip, which counts from zero here too).
/// The codec decoders cut their output to a packet's duration. Granules that disagree with the
/// packets by a whole packet or more are not trusted.
fn trim_end(samples: &mut [Sample], total: &mut i64, granule: Option<u64>) {
    let (Some(g), Some(last)) = (granule, samples.last_mut()) else { return };
    let Ok(g) = i64::try_from(g) else { return };
    let excess = *total - g;
    if excess > 0 && excess < last.duration as i64 {
        last.duration -= excess as u32;
        *total = g;
    }
}

/// The codec of a logical Ogg stream, from its first packet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Opus,
    Vorbis,
    Flac,
}

impl Kind {
    /// The codec of a stream whose first packet is `first`, or `None` for codecs that are not read.
    fn of(first: &[u8]) -> Option<Kind> {
        if first.starts_with(b"OpusHead") {
            Some(Kind::Opus)
        } else if first.starts_with(b"\x01vorbis") {
            Some(Kind::Vorbis)
        } else if first.starts_with(b"\x7fFLAC") {
            Some(Kind::Flac)
        } else {
            None
        }
    }

    /// How many packets at the start of the stream are headers (not audio).
    fn headers(self) -> usize {
        match self {
            Kind::Opus => 2,
            Kind::Vorbis => 3,
            Kind::Flac => 1, // more metadata packets may follow; see `is_header`
        }
    }
}

/// One logical stream (a serial number, from its first page to its last): its audio packets and headers, as
/// offsets into the unwrapped packet buffer.
struct Logical {
    kind: Kind,
    /// Page index of the stream's first page (its beginning-of-stream) and of its last page so far.
    first_page: usize,
    last_page: usize,
    /// The packet being assembled from lacing values, which may continue over pages.
    partial: Vec<u8>,
    /// Packets completed so far.
    seen: usize,
    headers: Vec<(u64, usize)>,
    packets: Vec<(u64, usize)>,
    /// The granule position of its last page, which gives the stream's true length.
    last_granule: Option<u64>,
}

/// A track of the output: one logical stream, or several chained ones decoded as one.
struct Track {
    kind: Kind,
    /// Indices into the logical streams, in file order.
    streams: Vec<usize>,
    /// What must match for a chained stream to join the track (the decoder's setup, which is in the headers).
    key: Vec<u8>,
}

/// Open an Ogg file for reading.
///
/// Every logical stream is read: multiplexed streams (sharing the file in time) are separate tracks, and
/// chained streams (one after another) of Vorbis or FLAC whose setup matches are joined into one track. An
/// Opus stream is never joined to a chained one: the decoder removes the pre-skip once per track, so each link
/// stays a track of its own. Streams of other codecs (Speex, Theora, ...) are passed over.
pub fn open<R: Read>(mut r: R) -> Result<Mp4Demuxer<Cursor<Vec<u8>>>> {
    let mut file = Vec::new();
    r.by_ref().take(MAX_FILE + 1).read_to_end(&mut file)?;
    if file.len() as u64 > MAX_FILE {
        return Err(Error::new(ErrorCode::UnsupportedFormat, "Ogg: files over 1 GiB are not supported (the file is read into memory)"));
    }

    let mut data: Vec<u8> = Vec::new(); // unwrapped packet bytes of every stream
    let mut streams: Vec<Logical> = Vec::new();
    // the stream each serial number is in now: None for a codec that is not read
    let mut current: HashMap<u32, Option<usize>> = HashMap::new();
    let mut unsupported: Option<String> = None;

    let mut at = 0usize;
    let mut page_no = 0usize;
    while at + 27 <= file.len() {
        if &file[at..at + 4] != b"OggS" {
            return Err(bad(format!("expected a page at byte {at}")));
        }
        if file[at + 4] != 0 {
            return Err(bad("unknown page version"));
        }
        let flags = file[at + 5];
        let ser = u32::from_le_bytes(file[at + 14..at + 18].try_into().unwrap());
        let nseg = file[at + 26] as usize;
        let Some(table) = file.get(at + 27..at + 27 + nseg) else { break };
        let body: usize = table.iter().map(|&s| s as usize).sum();
        let end = at + 27 + nseg + body;
        if end > file.len() {
            break; // a truncated last page
        }
        let mut page = file[at..end].to_vec();
        let want = u32::from_le_bytes(page[22..26].try_into().unwrap());
        page[22..26].fill(0);
        if crc(&page) != want {
            return Err(bad(format!("the page at byte {at} fails its checksum")));
        }
        at = end;
        let this_page = page_no;
        page_no += 1;

        if flags & 0x02 != 0 {
            // a stream begins: read it if it is Opus, Vorbis or FLAC, and pass over it otherwise
            let first = &page[27 + nseg..];
            let slot = match Kind::of(first) {
                Some(kind) => {
                    streams.push(Logical { kind, first_page: this_page, last_page: this_page, partial: Vec::new(), seen: 0, headers: Vec::new(), packets: Vec::new(), last_granule: None });
                    Some(streams.len() - 1)
                }
                None => {
                    if unsupported.is_none() {
                        let name = [(&b"Speex"[..], "Speex"), (&b"\x80theora"[..], "Theora")].iter().find(|(m, _)| first.starts_with(m)).map_or("an unrecognised codec", |(_, n)| n);
                        unsupported = Some(name.to_string());
                    }
                    None
                }
            };
            current.insert(ser, slot);
        }
        let Some(&Some(si)) = current.get(&ser) else { continue };
        let s = &mut streams[si];
        s.last_page = this_page;
        let granule = u64::from_le_bytes(page[6..14].try_into().unwrap());
        if granule != u64::MAX {
            s.last_granule = Some(granule);
        }
        if flags & 0x01 == 0 {
            s.partial.clear(); // the packet that was being assembled lost its tail: drop it
        }

        let mut p = 27 + nseg;
        for &len in table {
            s.partial.extend_from_slice(&page[p..p + len as usize]);
            p += len as usize;
            if len < 255 {
                let pkt = std::mem::take(&mut s.partial);
                let off = data.len() as u64;
                data.extend_from_slice(&pkt);
                // FLAC's metadata packets never start with a frame sync code
                let is_header = if s.kind == Kind::Flac {
                    s.seen == 0 || pkt.len() < 2 || pkt[0] != 0xFF || pkt[1] & 0xFE != 0xF8
                } else {
                    s.seen < s.kind.headers()
                };
                if is_header {
                    s.headers.push((off, pkt.len()));
                } else {
                    s.packets.push((off, pkt.len()));
                }
                s.seen += 1;
            }
        }
    }

    if streams.is_empty() {
        return Err(match unsupported {
            Some(n) => Error::new(ErrorCode::UnsupportedCodec, format!("Ogg: the file holds {n}; only Opus, Vorbis and FLAC in Ogg are read so far")),
            None => bad("no Opus or Vorbis stream found"),
        });
    }

    // the tracks: each stream is either a track of its own or joins an earlier chained stream of its codec
    let mut tracks: Vec<Track> = Vec::new();
    for (i, s) in streams.iter().enumerate() {
        if s.headers.len() < s.kind.headers() {
            continue; // a stream cut off inside its headers has nothing to decode
        }
        let key = match s.kind {
            Kind::Opus => Vec::new(),
            Kind::Vorbis => [header(&data, s, 0)?, header(&data, s, 2)?].concat(), // identification and setup
            Kind::Flac => {
                let hd = header(&data, s, 0)?;
                if hd.len() < 51 {
                    return Err(bad("the FLAC mapping header is too short"));
                }
                // rate, channels and sample size: the STREAMINFO fields that a chained stream must share
                (u64::from_be_bytes(hd[27..35].try_into().unwrap()) >> 36).to_be_bytes().to_vec()
            }
        };
        let joined = match tracks.iter_mut().find(|t| s.kind != Kind::Opus && t.kind == s.kind && t.key == key && streams[*t.streams.last().unwrap()].last_page < s.first_page) {
            Some(t) => {
                t.streams.push(i);
                true
            }
            None => false,
        };
        if !joined {
            tracks.push(Track { kind: s.kind, streams: vec![i], key });
        }
    }
    if tracks.is_empty() {
        return Err(match unsupported {
            Some(n) => Error::new(ErrorCode::UnsupportedCodec, format!("Ogg: the file holds {n}; only Opus, Vorbis and FLAC in Ogg are read so far")),
            None => bad("no Opus or Vorbis stream found"),
        });
    }

    let mut samples: Vec<Sample> = Vec::new();
    let mut infos: Vec<StreamInfo> = Vec::new();
    for (ti, t) in tracks.iter().enumerate() {
        let first = &streams[t.streams[0]];
        let (stream, timer_headers) = track_info(t.kind, first, &data, ti)?;
        let mut pts = 0i64;
        for &si in &t.streams {
            let s = &streams[si];
            let link_at = samples.len();
            let mut link = 0i64;
            match t.kind {
                Kind::Flac => {
                    // STREAMINFO's minimum and maximum block size, for a frame whose header will not parse
                    let max_block = u16::from_be_bytes([header(&data, s, 0)?[19], header(&data, s, 0)?[20]]) as u32;
                    for &(off, size) in &s.packets {
                        let frame = &data[off as usize..off as usize + size];
                        let len = ironav_codec::flac::header::parse_header(frame).map_or(max_block, |h| h.block_size);
                        samples.push(Sample { stream: ti as u16, sync: true, offset: off, size: size as u32, dts: pts + link, cts_offset: 0, duration: len });
                        link += len as i64;
                    }
                }
                Kind::Vorbis => {
                    let mut timer = ironav_codec::vorbis::PacketTimer::new(&timer_headers)?;
                    for &(off, size) in &s.packets {
                        let len = timer.next(&data[off as usize..off as usize + size]);
                        samples.push(Sample { stream: ti as u16, sync: true, offset: off, size: size as u32, dts: pts + link, cts_offset: 0, duration: len });
                        link += len as i64;
                    }
                    trim_end(&mut samples[link_at..], &mut link, s.last_granule);
                }
                Kind::Opus => {
                    for &(off, size) in &s.packets {
                        let h = &data[off as usize..off as usize + size.min(2)];
                        let len = if size == 0 { 960 } else { ironav_codec::opus::packet_samples(h).unwrap_or(960) as u32 };
                        samples.push(Sample { stream: ti as u16, sync: true, offset: off, size: size as u32, dts: pts + link, cts_offset: 0, duration: len });
                        link += len as i64;
                    }
                    trim_end(&mut samples[link_at..], &mut link, s.last_granule);
                }
            }
            pts += link;
        }
        infos.push(StreamInfo { index: ti, duration: Some(pts), ..stream });
    }
    let tracks_out: Vec<TrackInfo> = infos
        .into_iter()
        .map(|stream| TrackInfo { stream, config: Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 }, convert: false, swap: 0, unwrap: Vec::new() })
        .collect();
    Ok(Mp4Demuxer::from_parts(Cursor::new(data), "ogg", tracks_out, samples))
}

/// The `i`-th header packet of a stream.
fn header<'a>(data: &'a [u8], s: &Logical, i: usize) -> Result<&'a [u8]> {
    let &(off, len) = s.headers.get(i).ok_or_else(|| bad("the stream ends inside its headers"))?;
    Ok(&data[off as usize..off as usize + len])
}

/// The stream description of a track, from the headers of its first stream, and the header bytes the
/// packet timer needs (the Vorbis headers joined; empty for the other codecs).
fn track_info(kind: Kind, first: &Logical, data: &[u8], index: usize) -> Result<(StreamInfo, Vec<u8>)> {
    Ok(match kind {
        Kind::Flac => {
            let hd = header(data, first, 0)?;
            // 0x7F "FLAC", version (2), header count (2), "fLaC", a metadata block header (4), STREAMINFO (34)
            if hd.len() < 51 || &hd[9..13] != b"fLaC" || hd[5] != 1 {
                return Err(bad("the FLAC mapping header is too short or has an unknown version"));
            }
            let b = &hd[17..51];
            let packed = u64::from_be_bytes(b[10..18].try_into().unwrap());
            let (rate, channels, bits) = ((packed >> 44) as u32, ((packed >> 41) & 7) as u16 + 1, ((packed >> 36) & 31) as u8 + 1);
            if rate == 0 {
                return Err(bad("STREAMINFO has a zero sample rate"));
            }
            let stream = StreamInfo {
                index,
                codec: CodecId::Flac,
                time_base: Rational::new(1, rate as i64),
                duration: None,
                bits: Some(bits),
                extradata: Vec::new(),
                lead_in: 0,
                params: StreamParams::Audio { sample_rate: rate, channels },
            };
            (stream, Vec::new())
        }
        Kind::Vorbis => {
            let extradata = ironav_codec::vorbis::join_headers([header(data, first, 0)?, header(data, first, 1)?, header(data, first, 2)?]);
            let (channels, rate) = ironav_codec::vorbis::PacketTimer::channels_and_rate(&extradata)?;
            let stream = StreamInfo {
                index,
                codec: CodecId::Vorbis,
                time_base: Rational::new(1, rate as i64),
                duration: None,
                bits: None,
                extradata: extradata.clone(),
                lead_in: 0,
                params: StreamParams::Audio { sample_rate: rate, channels },
            };
            (stream, extradata)
        }
        Kind::Opus => {
            let hd = header(data, first, 0)?;
            if hd.len() < 19 || hd[8] >> 4 != 0 {
                return Err(bad("OpusHead is too short or has an unknown version"));
            }
            let channels = hd[9] as u16;
            let stream = StreamInfo {
                index,
                codec: CodecId::Opus,
                time_base: Rational::new(1, 48_000),
                duration: None,
                bits: None,
                extradata: hd.to_vec(),
                lead_in: 0,
                params: StreamParams::Audio { sample_rate: 48_000, channels },
            };
            (stream, Vec::new())
        }
    })
}


#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::Demuxer;

    fn page(flags: u8, granule: u64, serial: u32, seq: u32, packets: &[&[u8]], last_open: bool) -> Vec<u8> {
        let mut table = Vec::new();
        let mut body = Vec::new();
        for (i, p) in packets.iter().enumerate() {
            let mut n = p.len();
            while n >= 255 {
                table.push(255);
                n -= 255;
            }
            if !(last_open && i + 1 == packets.len()) {
                table.push(n as u8);
            } else if p.len() % 255 != 0 {
                // an open packet must end on a 255 lacing value
                unreachable!();
            }
            body.extend_from_slice(p);
        }
        let mut v = b"OggS".to_vec();
        v.push(0);
        v.push(flags);
        v.extend(granule.to_le_bytes());
        v.extend(serial.to_le_bytes());
        v.extend(seq.to_le_bytes());
        v.extend([0; 4]);
        v.push(table.len() as u8);
        v.extend(&table);
        v.extend(&body);
        let c = crc(&v);
        v[22..26].copy_from_slice(&c.to_le_bytes());
        v
    }

    fn opus_head() -> Vec<u8> {
        let mut h = b"OpusHead".to_vec();
        h.extend([1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]);
        h
    }

    #[test]
    fn crc_matches_the_ogg_check_value() {
        // CRC-32/MPEG-2-style with zero init and no xor: check value of "123456789"
        assert_eq!(crc(b"123456789"), 0x89A1_897F);
    }

    #[test]
    fn reads_packets_across_pages_with_continuation() {
        // 20 ms CELT-only stereo packets (TOC 0xFC: config 31, stereo, code 0): 1 + 299 bytes
        let mut p1 = vec![0xFC];
        p1.extend(vec![1u8; 509]); // 510 bytes: needs 255+255+0 lacing
        let mut p2 = vec![0xFC];
        p2.extend(vec![2u8; 99]);
        let mut f = page(0x02, 0, 7, 0, &[&opus_head()], false);
        f.extend(page(0, 0, 7, 1, &[b"OpusTags....."], false));
        // p1 is split at its first 255 bytes: the first page ends with an open packet
        f.extend(page(0, 0, 7, 2, &[&p1[..255]], true));
        f.extend(page(0x01, 1920, 7, 3, &[&p1[255..], &p2], false));
        f.extend(page(0x04, 2880, 7, 4, &[&p2], false));
        let mut d = open(&f[..]).unwrap();
        let s = d.streams()[0].clone();
        assert_eq!(s.codec, CodecId::Opus);
        assert_eq!(s.time_base, Rational::new(1, 48_000));
        let mut got = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            got.push((p.pts, p.data.len()));
        }
        assert_eq!(got, vec![(0, 510), (960, 100), (1920, 100)]);
    }

    #[test]
    fn refuses_speex_and_bad_checksums() {
        let mut v = b"Speex   ".to_vec();
        v.extend([0; 20]);
        let f = page(0x02, 0, 1, 0, &[&v], false);
        let e = open(&f[..]).err().unwrap();
        assert!(e.message.contains("Speex"), "{}", e.message);

        let mut f = page(0x02, 0, 7, 0, &[&opus_head()], false);
        let n = f.len();
        f[n - 1] ^= 1;
        assert!(open(&f[..]).is_err());
    }

    /// A native-FLAC mapping header: 0x7F "FLAC", version 1, a header count, "fLaC", and STREAMINFO.
    fn flac_head(rate: u32, channels: u8, max_block: u16) -> Vec<u8> {
        let mut h = vec![0x7F];
        h.extend(b"FLAC");
        h.extend([1, 0, 0, 0]);
        h.extend(b"fLaC");
        h.extend([0x80, 0, 0, 34]); // last metadata block, STREAMINFO, 34 bytes
        let mut info = [0u8; 34];
        info[0..2].copy_from_slice(&max_block.to_be_bytes()); // minimum block size
        info[2..4].copy_from_slice(&max_block.to_be_bytes()); // maximum block size
        let packed = ((rate as u64) << 44) | (((channels - 1) as u64) << 41) | (15u64 << 36);
        info[10..18].copy_from_slice(&packed.to_be_bytes());
        h.extend(info);
        h
    }

    #[test]
    fn multiplexed_opus_streams_are_separate_tracks() {
        // two Opus streams, serials 7 and 9, whose pages alternate in the file
        let mut a = vec![0xFC];
        a.extend(vec![1u8; 99]);
        let mut b = vec![0xFC];
        b.extend(vec![2u8; 99]);
        let mut f = page(0x02, 0, 7, 0, &[&opus_head()], false);
        f.extend(page(0x02, 0, 9, 0, &[&opus_head()], false));
        f.extend(page(0, 0, 7, 1, &[b"OpusTags....."], false));
        f.extend(page(0, 0, 9, 1, &[b"OpusTags....."], false));
        f.extend(page(0, 960, 7, 2, &[&a], false));
        f.extend(page(0x04, 960, 9, 2, &[&b], false));
        f.extend(page(0x04, 1920, 7, 3, &[&a], false));
        let mut d = open(&f[..]).unwrap();
        assert_eq!(d.streams().len(), 2, "one track per stream");
        let mut got = [Vec::new(), Vec::new()];
        while let Some(p) = d.read_packet().unwrap() {
            got[p.stream].push((p.pts, p.data[0]));
        }
        assert_eq!(got[0], vec![(0, 0xFC), (960, 0xFC)]);
        assert_eq!(got[1], vec![(0, 0xFC)]);
    }

    #[test]
    fn chained_flac_streams_are_one_track_on_one_timeline() {
        // two links with the same rate, channels and sample size: the second starts where the first ends
        // a frame whose header does not parse (block size code 0 is reserved), so each takes the maximum block size
        let frame = |fill: u8| {
            let mut v = vec![0xFF, 0xF8, 0, 0];
            v.extend(vec![fill; 40]);
            v
        };
        let mut f = page(0x02, 0, 1, 0, &[&flac_head(44_100, 2, 4096)], false);
        f.extend(page(0, 0, 1, 1, &[&frame(1)], false));
        f.extend(page(0x04, 0, 1, 2, &[&frame(2)], false));
        f.extend(page(0x02, 0, 2, 0, &[&flac_head(44_100, 2, 4096)], false));
        f.extend(page(0, 0, 2, 1, &[&frame(3)], false));
        f.extend(page(0x04, 0, 2, 2, &[&frame(4)], false));
        let mut d = open(&f[..]).unwrap();
        assert_eq!(d.streams().len(), 1, "the chained links are one track");
        let mut got = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            got.push((p.pts, p.data[4]));
        }
        assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        // each frame is as long as the others, and the second link starts where the first one ends
        let step = got[1].0 - got[0].0;
        assert!(step > 0);
        assert_eq!(got[2].0, got[0].0 + 2 * step, "the second link continues the timeline");
        assert_eq!(got[3].0, got[0].0 + 3 * step);
    }

    #[test]
    fn chained_flac_with_another_format_stays_a_separate_track() {
        let mut f = page(0x02, 0, 1, 0, &[&flac_head(44_100, 2, 4096)], false);
        f.extend(page(0x04, 0, 1, 1, &[&[0xFF, 0xF8, 9, 9]], false));
        f.extend(page(0x02, 0, 2, 0, &[&flac_head(48_000, 2, 4096)], false));
        f.extend(page(0x04, 0, 2, 1, &[&[0xFF, 0xF8, 8, 8]], false));
        let d = open(&f[..]).unwrap();
        assert_eq!(d.streams().len(), 2, "a link at another rate is a track of its own");
    }

    #[test]
    fn chained_opus_links_are_separate_tracks() {
        let mut f = page(0x02, 0, 7, 0, &[&opus_head()], false);
        f.extend(page(0, 0, 7, 1, &[b"OpusTags....."], false));
        f.extend(page(0x04, 960, 7, 2, &[&[0xFC, 1, 1]], false));
        f.extend(page(0x02, 0, 9, 0, &[&opus_head()], false));
        f.extend(page(0, 0, 9, 1, &[b"OpusTags....."], false));
        f.extend(page(0x04, 960, 9, 2, &[&[0xFC, 2, 2]], false));
        let d = open(&f[..]).unwrap();
        assert_eq!(d.streams().len(), 2, "each Opus link keeps its own pre-skip");
    }
}
