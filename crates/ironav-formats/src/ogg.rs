//! Ogg reader (Opus, Vorbis and FLAC).
//!
//! The file is read whole and its pages are unwrapped into one buffer of packet bytes, so a packet
//! that continues over a page boundary is contiguous again. The packets are then handed to the MP4
//! reader's machinery (`Mp4Demuxer::from_parts`) over that buffer, which reads, interleaves and
//! seeks them.
//!
//! Timing comes from the packets themselves (an Opus packet knows its length; a Vorbis packet's
//! block size follows from its mode, see `PacketTimer`), starting at zero: the Opus decoder drops
//! the pre-skip announced by `OpusHead`. The granule position of the last page, which gives the
//! true length, shortens the last packet (see `trim_end`); the decoders cut their output to it.
//!
//! FLAC in Ogg is the native-FLAC mapping: one FLAC frame per packet, durations from the frame
//! headers, no end trim (a FLAC frame is exact).
//!
//! Not handled: Speex, Theora and other codecs in Ogg (refused), chained streams (only the first
//! Opus, Vorbis or FLAC stream is read) and files too big to hold in memory (refused above 1 GiB).

use crate::mp4demux::{Config, Mp4Demuxer, Sample, TrackInfo};
use ironav_core::{CodecId, Error, ErrorCode, Rational, Result, StreamInfo, StreamParams};
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
fn crc(data: &[u8]) -> u32 {
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

/// Open an Ogg file for reading.
pub fn open<R: Read>(mut r: R) -> Result<Mp4Demuxer<Cursor<Vec<u8>>>> {
    let mut file = Vec::new();
    r.by_ref().take(MAX_FILE + 1).read_to_end(&mut file)?;
    if file.len() as u64 > MAX_FILE {
        return Err(Error::new(ErrorCode::UnsupportedFormat, "Ogg: files over 1 GiB are not supported (the file is read into memory)"));
    }

    let mut data: Vec<u8> = Vec::new(); // unwrapped packet bytes
    let mut serial: Option<u32> = None; // the Opus or Vorbis stream being read
    let mut vorbis = false;
    let mut flac = false;
    let mut ignored: Vec<u32> = Vec::new();
    let mut headers: Vec<(u64, usize)> = Vec::new(); // OpusHead and OpusTags, or Vorbis's three headers
    let mut packets: Vec<(u64, usize)> = Vec::new(); // audio packets
    let mut seen = 0usize; // packets of the stream so far
    let mut start = 0usize; // where the packet being assembled begins in `data`
    let mut unsupported: Option<String> = None;
    let mut last_granule: Option<u64> = None; // position after the last page that completed a packet

    let mut at = 0usize;
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

        if flags & 0x02 != 0 && serial.is_none() {
            // a new stream begins: take it if it is Opus or Vorbis
            let first = &page[27 + nseg..];
            if first.starts_with(b"OpusHead") || first.starts_with(b"\x01vorbis") || first.starts_with(b"\x7fFLAC") {
                serial = Some(ser);
                vorbis = first[0] == 1;
                flac = first[0] == 0x7F;
            } else if unsupported.is_none() {
                let name = [(&b"Speex"[..], "Speex"), (&b"\x80theora"[..], "Theora")]
                    .iter()
                    .find(|(m, _)| first.starts_with(m))
                    .map_or("an unrecognised codec", |(_, n)| n);
                unsupported = Some(name.to_string());
            }
        }
        if serial != Some(ser) {
            if !ignored.contains(&ser) {
                ignored.push(ser);
            }
            continue;
        }

        let granule = u64::from_le_bytes(page[6..14].try_into().unwrap());
        last_granule = (granule != u64::MAX).then_some(granule);

        let mut p = 27 + nseg;
        if flags & 0x01 == 0 {
            if start != data.len() {
                data.truncate(start); // a packet lost its tail: drop it
            }
        }
        for &s in table {
            data.extend_from_slice(&page[p..p + s as usize]);
            p += s as usize;
            if s < 255 {
                let pkt = (start as u64, data.len() - start);
                let body = &data[start..];
                // FLAC's metadata packets never start with a frame sync code
                let header = if flac { seen == 0 || body.len() < 2 || body[0] != 0xFF || body[1] & 0xFE != 0xF8 } else { seen < if vorbis { 3 } else { 2 } };
                if header {
                    headers.push(pkt);
                } else {
                    packets.push(pkt);
                }
                seen += 1;
                start = data.len();
            }
        }
    }

    if headers.is_empty() {
        return Err(match unsupported {
            Some(n) => Error::new(ErrorCode::UnsupportedCodec, format!("Ogg: the file holds {n}; only Opus, Vorbis and FLAC in Ogg are read so far")),
            None => bad("no Opus or Vorbis stream found"),
        });
    }
    let hdr = |i: usize| -> Result<&[u8]> {
        let &(off, len) = headers.get(i).ok_or_else(|| bad("the stream ends inside its headers"))?;
        Ok(&data[off as usize..off as usize + len])
    };
    let mut samples = Vec::with_capacity(packets.len());
    let mut pts = 0i64;
    let stream;
    if flac {
        let hd = hdr(0)?;
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
        let max_block = u16::from_be_bytes([b[2], b[3]]) as u32;
        for &(off, size) in &packets {
            let frame = &data[off as usize..off as usize + size];
            let len = ironav_codec::flac::header::parse_header(frame).map_or(max_block, |h| h.block_size);
            samples.push(Sample { stream: 0, sync: true, offset: off, size: size as u32, dts: pts, cts_offset: 0, duration: len });
            pts += len as i64;
        }
        stream = StreamInfo {
            index: 0,
            codec: CodecId::Flac,
            time_base: Rational::new(1, rate as i64),
            duration: Some(pts),
            bits: Some(bits),
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Audio { sample_rate: rate, channels },
        };
    } else if vorbis {
        let extradata = ironav_codec::vorbis::join_headers([hdr(0)?, hdr(1)?, hdr(2)?]);
        let (channels, rate) = ironav_codec::vorbis::PacketTimer::channels_and_rate(&extradata)?;
        let mut timer = ironav_codec::vorbis::PacketTimer::new(&extradata)?;
        for &(off, size) in &packets {
            let len = timer.next(&data[off as usize..off as usize + size]);
            samples.push(Sample { stream: 0, sync: true, offset: off, size: size as u32, dts: pts, cts_offset: 0, duration: len });
            pts += len as i64;
        }
        trim_end(&mut samples, &mut pts, last_granule);
        stream = StreamInfo {
            index: 0,
            codec: CodecId::Vorbis,
            time_base: Rational::new(1, rate as i64),
            duration: Some(pts),
            bits: None,
            extradata,
            lead_in: 0,
            params: StreamParams::Audio { sample_rate: rate, channels },
        };
    } else {
        let hd = hdr(0)?;
        if hd.len() < 19 || hd[8] >> 4 != 0 {
            return Err(bad("OpusHead is too short or has an unknown version"));
        }
        let channels = hd[9] as u16;
        let extradata = hd.to_vec();
        for &(off, size) in &packets {
            let h = &data[off as usize..off as usize + size.min(2)];
            let len = if size == 0 { 960 } else { ironav_codec::opus::packet_samples(h).unwrap_or(960) as u32 };
            samples.push(Sample { stream: 0, sync: true, offset: off, size: size as u32, dts: pts, cts_offset: 0, duration: len });
            pts += len as i64;
        }
        trim_end(&mut samples, &mut pts, last_granule);
        stream = StreamInfo {
            index: 0,
            codec: CodecId::Opus,
            time_base: Rational::new(1, 48_000),
            duration: Some(pts),
            bits: None,
            extradata,
            lead_in: 0,
            params: StreamParams::Audio { sample_rate: 48_000, channels },
        };
    }
    let track = TrackInfo { stream, config: Config { prefix: Vec::new(), nal_length: 0, bits: 8 }, convert: false };
    Ok(Mp4Demuxer::from_parts(Cursor::new(data), "ogg", vec![track], samples))
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
}
