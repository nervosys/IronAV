//! Raw Blu-ray subtitle files (`.sup`): the segments of a PGS (Presentation Graphic Stream) one
//! after another, each behind a 13-byte header of `PG`, a 32-bit presentation time and a 32-bit decoding
//! time (both on the 90 kHz clock) and the segment's type and 16-bit length. One subtitle stream of the
//! `hdmv_pgs_subtitle` codec comes out, a packet per display set (its segments up to and including the
//! END segment, as Matroska stores them) at the time of its first segment, on a time base of 1/90000.
//! The packets are bitmaps in PGS's coding; `ironav_codec::bitmapsub` decodes them. A file is read
//! whole (up to [`MAX_FILE_BYTES`]), and a segment cut short at the end is dropped.

use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Packet, Rational, Result, StreamInfo, StreamParams};
use std::io::Read;

/// The largest file read.
pub const MAX_FILE_BYTES: u64 = 512 << 20;
const HEADER: usize = 13;
const END: u8 = 0x80;

/// Whether `m`, the first bytes of a file, begin with segments of a PGS stream.
pub fn looks_like_sup(m: &[u8]) -> bool {
    let mut at = 0;
    let mut segments = 0;
    while at + HEADER <= m.len() && segments < 4 {
        if &m[at..at + 2] != b"PG" || !matches!(m[at + 10], 0x14..=0x17 | END) {
            return false;
        }
        at += HEADER + u16::from_be_bytes([m[at + 11], m[at + 12]]) as usize;
        segments += 1;
    }
    segments > 0
}

/// The packets of a `.sup` file.
pub struct SupDemuxer {
    streams: Vec<StreamInfo>,
    packets: Vec<Packet>,
    next: usize,
}

impl SupDemuxer {
    pub fn open<R: Read>(r: R) -> Result<Self> {
        let mut data = Vec::new();
        r.take(MAX_FILE_BYTES + 1).read_to_end(&mut data)?;
        if data.len() as u64 > MAX_FILE_BYTES {
            return Err(Error::invalid_input(format!("the subtitle file is larger than {} MiB", MAX_FILE_BYTES >> 20)));
        }
        if !looks_like_sup(&data) {
            return Err(Error::new(ErrorCode::InvalidInput, "the file does not begin with PGS subtitle segments"));
        }
        let mut packets: Vec<Packet> = Vec::new();
        let (mut set, mut set_pts) = (Vec::new(), 0i64);
        let mut at = 0;
        while at + HEADER <= data.len() && &data[at..at + 2] == b"PG" {
            let len = u16::from_be_bytes([data[at + 11], data[at + 12]]) as usize;
            let Some(body) = data.get(at + 10..at + HEADER + len) else { break };
            if set.is_empty() {
                set_pts = u32::from_be_bytes([data[at + 2], data[at + 3], data[at + 4], data[at + 5]]) as i64;
            }
            set.extend_from_slice(body);
            if body[0] == END {
                packets.push(Packet { stream: 0, pts: set_pts, duration: 0, data: std::mem::take(&mut set) });
            }
            at += HEADER + len;
        }
        let end = packets.last().map(|p| p.pts);
        let stream = StreamInfo {
            index: 0,
            codec: CodecId::HdmvPgs,
            time_base: Rational::new(1, 90_000),
            duration: end,
            bits: None,
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Subtitle,
        };
        Ok(SupDemuxer { streams: vec![stream], packets, next: 0 })
    }
}

impl Demuxer for SupDemuxer {
    fn format_name(&self) -> &'static str {
        "sup"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        let p = self.packets.get(self.next).cloned();
        if p.is_some() {
            self.next += 1;
        }
        Ok(p)
    }

    /// Lands on the last display set at or before `seconds`, so the picture on screen is rebuilt.
    fn seek(&mut self, seconds: f64) -> Result<f64> {
        let t = (seconds.max(0.0) * 90_000.0) as i64;
        self.next = self.packets.partition_point(|p| p.pts <= t).saturating_sub(1);
        Ok(self.packets.get(self.next).map_or(0.0, |p| p.pts as f64 / 90_000.0).min(seconds.max(0.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_codec::bitmapsub::testing::*;

    fn file() -> Vec<u8> {
        let o = PgsObject { id: 1, x: 10, y: 10, w: 8, h: 4, pixels: vec![1; 32], crop: None, forced: false };
        let set = pgs_display_set((1920, 1080), 2, &[[1, 235, 128, 128, 255]], &[o], 0);
        sup_file(&[(90_000, set), (180_000, pgs_clear_set((1920, 1080)))])
    }

    #[test]
    fn a_display_set_is_a_packet_at_the_time_of_its_first_segment() {
        let f = file();
        assert!(looks_like_sup(&f));
        let mut d = SupDemuxer::open(&f[..]).unwrap();
        assert_eq!(d.streams()[0].codec, CodecId::HdmvPgs);
        let (a, b) = (d.read_packet().unwrap().unwrap(), d.read_packet().unwrap().unwrap());
        assert!(d.read_packet().unwrap().is_none());
        assert_eq!((a.pts, b.pts), (90_000, 180_000));
        // the packets decode to one event from 1 s to 2 s
        let mut dec = ironav_codec::bitmapsub::PgsDecoder::new(1.0 / 90_000.0);
        let mut events = dec.push(a.pts, &a.data);
        events.extend(dec.push(b.pts, &b.data));
        assert_eq!(events.len(), 1);
        assert_eq!((events[0].start, events[0].end, events[0].rects[0].pixels.len()), (1.0, 2.0, 32));
        assert_eq!(d.seek(1.5).unwrap(), 1.0);
        assert_eq!(d.read_packet().unwrap().unwrap().pts, 90_000);
    }

    #[test]
    fn other_data_and_cut_files_are_handled() {
        assert!(!looks_like_sup(b"PGxxxxxxxxxx\x99\0\0"));
        assert!(!looks_like_sup(b"RIFF"));
        assert!(SupDemuxer::open(&b"not a sup file at all"[..]).is_err());
        let f = file();
        let mut d = SupDemuxer::open(&f[..f.len() - 20]).unwrap();
        assert!(d.read_packet().unwrap().is_some());
        assert!(d.read_packet().unwrap().is_none(), "a display set cut short is dropped");
    }
}
