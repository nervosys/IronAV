//! Ogg writer for Opus (RFC 7845), Vorbis and FLAC (the native mapping): the headers, then the packets in
//! pages of about half a second, each stamped with the number of samples decoded once its last
//! packet has been (Opus: at 48 kHz, the pre-skip included). The final page's stamp is what lets
//! a player cut the padding off the end, so the last packet's duration decides where Opus ends.
//! FLAC's STREAMINFO goes into the first page before anything is known about the audio, so that
//! page is rewritten when the stream ends.

use crate::ogg::crc;
use crate::WriteSeek;
use crate::flac::{flac_audio_params, FlacStats};
use ironav_core::{CodecId, Error, ErrorCode, Muxer, Packet, Result, StreamInfo, StreamParams};
use std::io::{Seek, SeekFrom};

const SERIAL: u32 = 0x4952_4F4E;
/// A page is written once it holds this many samples (0.5 s at 48 kHz) …
const PAGE_SAMPLES: i64 = 24_000;
/// … or when another packet might not fit in its 255 segments.
const PAGE_SEGMENTS: usize = 240;
const HEADER_BOS: u8 = 0x02;
const HEADER_EOS: u8 = 0x04;

pub struct OggWriter {
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
    /// Granule units per second: 48 000 for Opus, the sample rate for FLAC.
    rate: i64,
    /// FLAC: (rate, channels, bits), the running totals, and the first page as written.
    flac: Option<((u32, u16, u8), FlacStats, Vec<u8>)>,
    /// Vorbis: sizes each packet's window, to stamp the pages.
    vorbis: Option<ironav_codec::vorbis::PacketTimer>,
    /// Samples the last packet's own duration cuts off its window's count (the end of the stream).
    last_trim: i64,
}

impl OggWriter {
    pub fn new(out: Box<dyn WriteSeek>) -> Self {
        Self { out, seq: 0, time_base: (1, 48_000), granule: 0, pending: Vec::new(), pending_segments: 0, pending_samples: 0, started: false, rate: 48_000, flac: None, vorbis: None, last_trim: 0 }
    }

    fn build_page(&self, kind: u8, granule: i64, packets: &[Vec<u8>]) -> Vec<u8> {
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
        page
    }

    fn page(&mut self, kind: u8, granule: i64, packets: &[Vec<u8>]) -> Result<()> {
        let page = self.build_page(kind, granule, packets);
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

impl Muxer for OggWriter {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [s] = streams else {
            return Err(Error::invalid_arg("Ogg holds exactly one audio stream here"));
        };
        if !matches!(s.codec, CodecId::Opus | CodecId::Flac | CodecId::Vorbis) || !matches!(s.params, StreamParams::Audio { .. }) {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("this Ogg writer holds Opus, Vorbis and FLAC, not {}", s.codec.name())).alternatives(["opus", "vorbis", "flac"]));
        }
        if s.time_base.num <= 0 || s.time_base.den <= 0 {
            return Err(Error::invalid_arg("a stream has no time base"));
        }
        self.time_base = (s.time_base.num, s.time_base.den);
        let vendor = b"IronAV";
        if s.codec == CodecId::Vorbis {
            let [id, comment, setup] = ironav_codec::vorbis::split_headers(&s.extradata)?;
            if comment.len() + setup.len() > (PAGE_SEGMENTS - 1) * 255 {
                return Err(Error::invalid_input("the Vorbis setup header is too large for one Ogg page"));
            }
            self.rate = match &s.params {
                StreamParams::Audio { sample_rate, .. } => *sample_rate as i64,
                _ => 48_000,
            };
            self.vorbis = Some(ironav_codec::vorbis::PacketTimer::new(&s.extradata)?);
            self.page(HEADER_BOS, 0, &[id.to_vec()])?;
            self.page(0, 0, &[comment.to_vec(), setup.to_vec()])?;
            self.started = true;
            return Ok(());
        }
        if s.codec == CodecId::Flac {
            let (rate, channels, bits) = flac_audio_params(s)?;
            self.rate = rate as i64;
            // 0x7F "FLAC", mapping version 1.0, one more header packet, then a native FLAC header
            let mut head = b"\x7fFLAC\x01\x00\x00\x01fLaC".to_vec();
            head.extend([0, 0, 0, 34]);
            head.extend(FlacStats::new().streaminfo(rate, channels, bits));
            // a Vorbis comment block, the last metadata block
            let mut tags = Vec::new();
            tags.extend((vendor.len() as u32).to_le_bytes());
            tags.extend(vendor);
            tags.extend(0u32.to_le_bytes());
            let mut comment = vec![0x84];
            comment.extend(&(tags.len() as u32).to_be_bytes()[1..]);
            comment.extend(tags);
            let first = self.build_page(HEADER_BOS, 0, &[head]);
            self.out.write_all(&first)?;
            self.seq += 1;
            self.page(0, 0, &[comment])?;
            self.flac = Some(((rate, channels, bits), FlacStats::new(), first));
            self.started = true;
            return Ok(());
        }
        if s.extradata.len() < 19 || !s.extradata.starts_with(b"OpusHead") {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "Opus needs its OpusHead to go into Ogg").hint("copy it from a file that carries it, or encode to opus"));
        }
        let mut tags = b"OpusTags".to_vec();
        tags.extend((vendor.len() as u32).to_le_bytes());
        tags.extend(vendor);
        tags.extend(0u32.to_le_bytes());
        self.page(HEADER_BOS, 0, std::slice::from_ref(&s.extradata))?;
        self.page(0, 0, &[tags])?;
        self.started = true;
        Ok(())
    }

    fn write_packet(&mut self, p: &Packet) -> Result<()> {
        if !self.started {
            return Err(Error::new(ErrorCode::Internal, "packet before the header"));
        }
        // the duration in 48 kHz samples; the packet itself knows when the container does not
        let samples = if let Some(t) = self.vorbis.as_mut() {
            // a Vorbis packet's window decides how much it adds; a shorter duration is the stream's end (only
            // trusted when counted in samples: Matroska's millisecond ticks are too coarse for it)
            let n = t.next(&p.data) as i64;
            let exact = self.time_base.0 == 1 && self.time_base.1 == self.rate;
            let cut = if p.duration > 0 && exact { (p.duration as i128 * self.rate as i128 * self.time_base.0 as i128 / self.time_base.1 as i128) as i64 } else { n };
            self.last_trim = (n - cut).max(0);
            n
        } else if p.duration > 0 {
            (p.duration as i128 * self.rate as i128 * self.time_base.0 as i128 / self.time_base.1 as i128) as i64
        } else {
            ironav_codec::opus::packet_samples(&p.data).unwrap_or(960) as i64
        };
        if let Some((_, stats, _)) = self.flac.as_mut() {
            stats.record(&Packet { duration: samples, ..p.clone() });
        }
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
            self.pending_samples -= self.last_trim;
            self.flush_page(true)?;
        }
        if let Some(((rate, channels, bits), stats, mut first)) = self.flac.take() {
            // STREAMINFO sits after the 27-byte page header, 1 table entry and 17 bytes of mapping header
            first[45..79].copy_from_slice(&stats.streaminfo(rate, channels, bits));
            first[22..26].fill(0);
            let sum = crc(&first);
            first[22..26].copy_from_slice(&sum.to_le_bytes());
            self.out.seek(SeekFrom::Start(0))?;
            self.out.write_all(&first)?;
            self.out.seek(SeekFrom::End(0))?;
        }
        self.out.flush()?;
        Ok(())
    }
}
