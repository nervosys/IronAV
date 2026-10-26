//! Ogg Opus writer (RFC 7845): `OpusHead`, `OpusTags`, then the packets in pages of about half a
//! second, each stamped with the number of samples (at 48 kHz, the pre-skip's included) decoded once
//! its last packet has been. The final page's stamp is what lets a player cut the padding off
//! the end, so the last packet's duration decides where the stream ends.

use crate::ogg::crc;
use crate::WriteSeek;
use ironav_core::{CodecId, Error, ErrorCode, Muxer, Packet, Result, StreamInfo, StreamParams};

const SERIAL: u32 = 0x4952_4F4E;
/// A page is written once it holds this many samples (0.5 s at 48 kHz) …
const PAGE_SAMPLES: i64 = 24_000;
/// … or when another packet might not fit in its 255 segments.
const PAGE_SEGMENTS: usize = 240;
const HEADER_BOS: u8 = 0x02;
const HEADER_EOS: u8 = 0x04;

pub struct OggOpusWriter {
    out: Box<dyn WriteSeek>,
    seq: u32,
    /// Input time base, to turn packet durations into 48 kHz samples.
    time_base: (i64, i64),
    /// Samples decoded by the end of the pages written so far, counted from the first one (so the
    /// pre-skip is part of it, and the packets' durations add up to it).
    granule: i64,
    pending: Vec<Vec<u8>>,
    pending_segments: usize,
    pending_samples: i64,
    started: bool,
}

impl OggOpusWriter {
    pub fn new(out: Box<dyn WriteSeek>) -> Self {
        Self { out, seq: 0, time_base: (1, 48_000), granule: 0, pending: Vec::new(), pending_segments: 0, pending_samples: 0, started: false }
    }

    fn page(&mut self, kind: u8, granule: i64, packets: &[Vec<u8>]) -> Result<()> {
        let mut table = Vec::new();
        for p in packets {
            let mut n = p.len();
            while n >= 255 {
                table.push(255u8);
                n -= 255;
            }
            table.push(n as u8);
        }
        debug_assert!(table.len() <= 255, "a page holds at most 255 segments");
        let mut page = Vec::with_capacity(27 + table.len() + packets.iter().map(Vec::len).sum::<usize>());
        page.extend_from_slice(b"OggS");
        page.push(0);
        page.push(kind);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&SERIAL.to_le_bytes());
        page.extend_from_slice(&self.seq.to_le_bytes());
        page.extend_from_slice(&[0; 4]);
        page.push(table.len() as u8);
        page.extend_from_slice(&table);
        for p in packets {
            page.extend_from_slice(p);
        }
        let sum = crc(&page);
        page[22..26].copy_from_slice(&sum.to_le_bytes());
        self.out.write_all(&page)?;
        self.seq += 1;
        Ok(())
    }

    fn flush_page(&mut self, last: bool) -> Result<()> {
        let packets = std::mem::take(&mut self.pending);
        self.granule += self.pending_samples;
        self.pending_samples = 0;
        self.pending_segments = 0;
        self.page(if last { HEADER_EOS } else { 0 }, self.granule, &packets)
    }
}

impl Muxer for OggOpusWriter {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [s] = streams else {
            return Err(Error::invalid_arg("Ogg Opus holds exactly one audio stream"));
        };
        if s.codec != CodecId::Opus || !matches!(s.params, StreamParams::Audio { .. }) {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("this Ogg writer holds Opus only, not {}", s.codec.name())).alternatives(["opus"]));
        }
        if s.extradata.len() < 19 || !s.extradata.starts_with(b"OpusHead") {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "Opus needs its OpusHead to go into Ogg").hint("copy it from a file that carries it, or encode to opus"));
        }
        if s.time_base.num <= 0 || s.time_base.den <= 0 {
            return Err(Error::invalid_arg("a stream has no time base"));
        }
        self.time_base = (s.time_base.num, s.time_base.den);
        let mut tags = b"OpusTags".to_vec();
        let vendor = b"IronAV";
        tags.extend((vendor.len() as u32).to_le_bytes());
        tags.extend(vendor);
        tags.extend(0u32.to_le_bytes());
        self.page(HEADER_BOS, 0, &[s.extradata.clone()])?;
        self.page(0, 0, &[tags])?;
        self.started = true;
        Ok(())
    }

    fn write_packet(&mut self, p: &Packet) -> Result<()> {
        if !self.started {
            return Err(Error::new(ErrorCode::Internal, "packet before the header"));
        }
        // the duration in 48 kHz samples; the packet itself knows when the container does not
        let samples = if p.duration > 0 {
            (p.duration as i128 * 48_000 * self.time_base.0 as i128 / self.time_base.1 as i128) as i64
        } else {
            ironav_codec::opus::packet_samples(&p.data).unwrap_or(960) as i64
        };
        let segments = p.data.len() / 255 + 1;
        if segments > PAGE_SEGMENTS {
            return Err(Error::invalid_input("an Opus packet is too large for an Ogg page"));
        }
        if !self.pending.is_empty() && (self.pending_segments + segments > PAGE_SEGMENTS || self.pending_samples >= PAGE_SAMPLES) {
            self.flush_page(false)?;
        }
        self.pending.push(p.data.clone());
        self.pending_segments += segments;
        self.pending_samples += samples;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            // no audio at all: the stream still needs its last page
            self.page(HEADER_EOS, self.granule, &[])?;
        } else {
            self.flush_page(true)?;
        }
        self.out.flush()?;
        Ok(())
    }
}
