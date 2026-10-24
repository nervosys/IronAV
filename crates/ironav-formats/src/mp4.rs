//! MP4 (ISO BMFF) writer for H.264 or HEVC video and FLAC audio, at most one track of each.
//!
//! Input packets are Annex B access units from the encoder. They are rewritten as
//! length-prefixed samples, parameter sets are lifted out into the `avcC` / `hvcC` box, and the
//! `moov` index is written after the media data (so the file is not "fast start"). Timing is
//! constant frame rate with no frame reordering, which is exactly what the NVENC provider
//! produces; a stream with B-frames would need `ctts` and is not supported here. FLAC frames are
//! stored as they come, one per sample, with the STREAMINFO block in a `dfLa` box.
//!
//! Layout: `ftyp`, `mdat` (64-bit size, patched at the end), `moov`. Each track's samples are
//! gathered into chunks of about half a second, so a player reading both tracks finds the
//! video and audio for the same moment close together in the file.

use crate::flac::{flac_audio_params, FlacStats};
use crate::WriteSeek;
use ironav_core::{CodecId, Error, ErrorCode, Muxer, Packet, Result, StreamInfo, StreamParams};
use std::io::{Seek, SeekFrom};

/// Split an Annex B buffer into NAL units (start codes removed, trailing zero padding trimmed).
pub(crate) fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new(); // (start of payload, start of the start-code)
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i + 3, i));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &(s, _)) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(data.len(), |n| n.1);
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

/// Remove emulation-prevention bytes (`00 00 03` -> `00 00`).
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + body.len());
    v.extend_from_slice(&(8 + body.len() as u32).to_be_bytes());
    v.extend_from_slice(kind);
    v.extend_from_slice(body);
    v
}

fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = vec![version];
    b.extend_from_slice(&flags.to_be_bytes()[1..]);
    b.extend_from_slice(body);
    bx(kind, &b)
}

fn u32s(vals: &[u32]) -> Vec<u8> {
    vals.iter().flat_map(|v| v.to_be_bytes()).collect()
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

#[derive(Default)]
struct ParamSets {
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

enum Kind {
    Video { width: u32, height: u32 },
    Audio { rate: u32, channels: u16, bits: u8 },
}

struct Track {
    codec: CodecId,
    kind: Kind,
    /// Media time units per second: the frame rate numerator for video, the sample rate for audio.
    timescale: u32,
    /// Duration of every video sample (the frame rate denominator); audio takes it from each packet.
    video_delta: u32,
    sizes: Vec<u32>,
    sync: Vec<u32>,
    /// Run-length coded sample durations: (count, delta).
    stts: Vec<(u32, u32)>,
    /// Finished chunks: (file offset, samples).
    chunks: Vec<(u64, u32)>,
    pending: Vec<u8>,
    pending_samples: u32,
    pending_dur: u64,
    duration: u64,
    params: ParamSets,
    flac: FlacStats,
}

impl Track {
    fn is_video(&self) -> bool {
        matches!(self.kind, Kind::Video { .. })
    }

    fn is_hevc(&self) -> bool {
        self.codec == CodecId::Hevc
    }

    fn sample_entry(&self) -> Result<Vec<u8>> {
        match self.kind {
            Kind::Video { width, height } => self.video_entry(width, height),
            Kind::Audio { rate, channels, bits } => Ok(self.audio_entry(rate, channels, bits)),
        }
    }

    fn video_entry(&self, width: u32, height: u32) -> Result<Vec<u8>> {
        let missing = |what: &str| Error::new(ErrorCode::Internal, format!("stream has no {what}; the encoder must emit parameter sets with its first frame"));
        let (kind, config) = if self.is_hevc() {
            (b"hvc1", self.hvcc(&missing)?)
        } else {
            (b"avc1", self.avcc(&missing)?)
        };
        let mut e = vec![0u8; 6];
        e.extend_from_slice(&1u16.to_be_bytes()); // data reference index
        e.extend_from_slice(&[0u8; 16]);
        e.extend_from_slice(&(width as u16).to_be_bytes());
        e.extend_from_slice(&(height as u16).to_be_bytes());
        e.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi
        e.extend_from_slice(&0x0048_0000u32.to_be_bytes());
        e.extend_from_slice(&0u32.to_be_bytes());
        e.extend_from_slice(&1u16.to_be_bytes()); // frames per sample
        e.extend_from_slice(&[0u8; 32]); // compressor name
        e.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
        e.extend_from_slice(&0xFFFFu16.to_be_bytes());
        e.extend_from_slice(&config);
        Ok(bx(kind, &e))
    }

    /// `fLaC` sample entry with a `dfLa` box holding the STREAMINFO metadata block (FLAC in ISO BMFF).
    fn audio_entry(&self, rate: u32, channels: u16, bits: u8) -> Vec<u8> {
        let mut e = vec![0u8; 6];
        e.extend_from_slice(&1u16.to_be_bytes()); // data reference index
        e.extend_from_slice(&[0u8; 8]);
        e.extend_from_slice(&channels.to_be_bytes());
        e.extend_from_slice(&(bits as u16).to_be_bytes());
        e.extend_from_slice(&[0u8; 4]); // pre-defined, reserved
        // 16.16 fixed point cannot hold rates of 65536 Hz or more; dfLa carries the true rate.
        e.extend_from_slice(&(if rate <= 65535 { rate << 16 } else { 0 }).to_be_bytes());
        let mut dfla = vec![0x80, 0, 0, 34]; // last metadata block, STREAMINFO, 34 bytes
        dfla.extend_from_slice(&self.flac.streaminfo(rate, channels, bits));
        e.extend_from_slice(&full(b"dfLa", 0, 0, &dfla));
        bx(b"fLaC", &e)
    }

    fn avcc(&self, missing: &dyn Fn(&str) -> Error) -> Result<Vec<u8>> {
        let sps = self.params.sps.as_ref().ok_or_else(|| missing("SPS"))?;
        let pps = self.params.pps.as_ref().ok_or_else(|| missing("PPS"))?;
        if sps.len() < 4 {
            return Err(Error::new(ErrorCode::Internal, "H.264 SPS is too short"));
        }
        let mut c = vec![1, sps[1], sps[2], sps[3], 0xFC | 3, 0xE0 | 1];
        c.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        c.extend_from_slice(sps);
        c.push(1);
        c.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        c.extend_from_slice(pps);
        // High-family profiles must also state chroma format and bit depth (4:2:0, 8-bit here).
        if matches!(sps[1], 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134) {
            c.extend_from_slice(&[0xFC | 1, 0xF8, 0xF8, 0]);
        }
        Ok(bx(b"avcC", &c))
    }

    fn hvcc(&self, missing: &dyn Fn(&str) -> Error) -> Result<Vec<u8>> {
        let sps = self.params.sps.as_ref().ok_or_else(|| missing("SPS"))?;
        let pps = self.params.pps.as_ref().ok_or_else(|| missing("PPS"))?;
        let vps = self.params.vps.as_ref().ok_or_else(|| missing("VPS"))?;
        let p = unescape(sps);
        // 2-byte NAL header, 1 byte (vps id, max sub layers, temporal-id nesting), then
        // profile_tier_level: profile byte, 4 compat bytes, 6 constraint bytes, level.
        if p.len() < 15 {
            return Err(Error::new(ErrorCode::Internal, "HEVC SPS is too short"));
        }
        let nesting = p[2] & 1;
        let mut c = vec![1];
        c.extend_from_slice(&p[3..14]); // profile byte, 4 compatibility bytes, 6 constraint bytes
        c.push(p[14]); // general_level_idc
        c.extend_from_slice(&0xF000u16.to_be_bytes()); // min spatial segmentation
        c.push(0xFC); // parallelism type
        c.push(0xFC | 1); // chroma 4:2:0
        c.push(0xF8); // luma bit depth 8
        c.push(0xF8); // chroma bit depth 8
        c.extend_from_slice(&0u16.to_be_bytes()); // average frame rate: unspecified
        c.push((1 << 3) | (nesting << 2) | 3); // 1 temporal layer, 4-byte lengths
        c.push(3);
        for (ty, nal) in [(32u8, vps), (33, sps), (34, pps)] {
            c.push(0x80 | ty);
            c.extend_from_slice(&1u16.to_be_bytes());
            c.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            c.extend_from_slice(nal);
        }
        Ok(bx(b"hvcC", &c))
    }

    /// Track duration in the movie's millisecond time scale.
    fn movie_ms(&self) -> u64 {
        self.duration * 1000 / self.timescale as u64
    }

    fn trak(&self, id: u32) -> Result<Vec<u8>> {
        let ms = self.movie_ms() as u32;
        let mut tkhd = u32s(&[0, 0, id, 0, ms]); // created, modified, track id, reserved, duration
        tkhd.extend_from_slice(&[0u8; 8]); // reserved
        tkhd.extend_from_slice(&0u32.to_be_bytes()); // layer, alternate group
        tkhd.extend_from_slice(&(if self.is_video() { 0u16 } else { 0x0100 }).to_be_bytes()); // volume
        tkhd.extend_from_slice(&0u16.to_be_bytes());
        tkhd.extend_from_slice(&u32s(&MATRIX));
        let (w, h) = match self.kind {
            Kind::Video { width, height } => (width, height),
            Kind::Audio { .. } => (0, 0),
        };
        tkhd.extend_from_slice(&(w << 16).to_be_bytes());
        tkhd.extend_from_slice(&(h << 16).to_be_bytes());
        let tkhd = full(b"tkhd", 0, 3, &tkhd);

        let mut mdhd = u32s(&[0, 0, self.timescale, self.duration as u32]);
        mdhd.extend_from_slice(&0x55C4u16.to_be_bytes()); // language "und"
        mdhd.extend_from_slice(&0u16.to_be_bytes());
        let mdhd = full(b"mdhd", 0, 0, &mdhd);

        let mut hdlr = u32s(&[0]);
        hdlr.extend_from_slice(if self.is_video() { b"vide" } else { b"soun" });
        hdlr.extend_from_slice(&[0u8; 12]);
        hdlr.extend_from_slice(b"IronAV\0");
        let hdlr = full(b"hdlr", 0, 0, &hdlr);

        let media_header = if self.is_video() { full(b"vmhd", 0, 1, &[0u8; 8]) } else { full(b"smhd", 0, 0, &[0u8; 4]) };
        let url = full(b"url ", 0, 1, &[]);
        let mut dref = u32s(&[1]);
        dref.extend_from_slice(&url);
        let dinf = bx(b"dinf", &full(b"dref", 0, 0, &dref));

        let n = self.sizes.len() as u32;
        let mut stsd = u32s(&[1]);
        stsd.extend_from_slice(&self.sample_entry()?);
        let stsd = full(b"stsd", 0, 0, &stsd);
        let mut stts = u32s(&[self.stts.len() as u32]);
        for &(count, delta) in &self.stts {
            stts.extend_from_slice(&u32s(&[count, delta]));
        }
        let stts = full(b"stts", 0, 0, &stts);
        // Samples-per-chunk runs: a new entry whenever the count changes.
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for (i, &(_, samples)) in self.chunks.iter().enumerate() {
            if runs.last().map(|r| r.1) != Some(samples) {
                runs.push((i as u32 + 1, samples));
            }
        }
        let mut stsc = u32s(&[runs.len() as u32]);
        for (first, samples) in runs {
            stsc.extend_from_slice(&u32s(&[first, samples, 1]));
        }
        let stsc = full(b"stsc", 0, 0, &stsc);
        let mut stsz = u32s(&[0, n]);
        stsz.extend_from_slice(&u32s(&self.sizes));
        let stsz = full(b"stsz", 0, 0, &stsz);
        let mut co64 = u32s(&[self.chunks.len() as u32]);
        for &(offset, _) in &self.chunks {
            co64.extend_from_slice(&offset.to_be_bytes());
        }
        let co64 = full(b"co64", 0, 0, &co64);

        let mut stbl = vec![stsd, stts];
        if self.is_video() {
            let mut stss = u32s(&[self.sync.len() as u32]);
            stss.extend_from_slice(&u32s(&self.sync));
            stbl.push(full(b"stss", 0, 0, &stss));
        }
        stbl.extend([stsc, stsz, co64]);
        let stbl = bx(b"stbl", &stbl.concat());
        let minf = bx(b"minf", &[media_header, dinf, stbl].concat());
        let mdia = bx(b"mdia", &[mdhd, hdlr, minf].concat());
        Ok(bx(b"trak", &[tkhd, mdia].concat()))
    }

    /// Convert one Annex B access unit to a length-prefixed sample, collecting parameter sets.
    /// Returns `None` if the packet held nothing but parameter sets and delimiters.
    fn video_sample(&mut self, data: &[u8]) -> Option<(Vec<u8>, bool)> {
        let hevc = self.is_hevc();
        let mut sample = Vec::with_capacity(data.len() + 16);
        let mut key = false;
        for nal in split_nals(data) {
            let ty = if hevc { (nal[0] >> 1) & 0x3F } else { nal[0] & 0x1F };
            match (hevc, ty) {
                (false, 7) => self.params.sps = Some(nal.to_vec()),
                (false, 8) => self.params.pps = Some(nal.to_vec()),
                (true, 32) => self.params.vps = Some(nal.to_vec()),
                (true, 33) => self.params.sps = Some(nal.to_vec()),
                (true, 34) => self.params.pps = Some(nal.to_vec()),
                (false, 9) | (true, 35) => {} // access unit delimiters carry nothing MP4 needs
                _ => {
                    key |= if hevc { (16..=21).contains(&ty) } else { ty == 5 };
                    sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    sample.extend_from_slice(nal);
                }
            }
        }
        (!sample.is_empty()).then_some((sample, key))
    }
}

pub struct Mp4Muxer {
    out: Box<dyn WriteSeek>,
    tracks: Vec<Track>,
    mdat_start: u64,
    data_len: u64,
}

impl Mp4Muxer {
    pub fn new(out: Box<dyn WriteSeek>) -> Self {
        Self { out, tracks: Vec::new(), mdat_start: 0, data_len: 0 }
    }

    /// Write a track's gathered samples to the file as one chunk.
    fn flush_chunk(&mut self, t: usize) -> Result<()> {
        let tr = &mut self.tracks[t];
        if tr.pending.is_empty() {
            return Ok(());
        }
        tr.chunks.push((self.mdat_start + 16 + self.data_len, tr.pending_samples));
        self.out.write_all(&tr.pending)?;
        self.data_len += tr.pending.len() as u64;
        tr.pending.clear();
        (tr.pending_samples, tr.pending_dur) = (0, 0);
        Ok(())
    }
}

impl Muxer for Mp4Muxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        if streams.is_empty() || streams.len() > 2 {
            return Err(Error::invalid_arg("this MP4 writer holds one video stream, one audio stream, or both"));
        }
        for (i, s) in streams.iter().enumerate() {
            let media = s.params.media_type();
            if streams[..i].iter().any(|o| o.params.media_type() == media) {
                return Err(Error::invalid_arg(format!("this MP4 writer holds at most one {media:?} stream")));
            }
            let (kind, timescale, video_delta) = match (&s.params, s.codec) {
                (StreamParams::Video { width, height, frame_rate, .. }, CodecId::H264 | CodecId::Hevc) => {
                    if frame_rate.num <= 0 || frame_rate.den <= 0 || frame_rate.num > u32::MAX as i64 || frame_rate.den > u32::MAX as i64 {
                        return Err(Error::invalid_arg("MP4 needs a known, constant frame rate"));
                    }
                    if *width > 65535 || *height > 65535 {
                        return Err(Error::invalid_arg("MP4 sample entries limit dimensions to 65535"));
                    }
                    (Kind::Video { width: *width, height: *height }, frame_rate.num as u32, frame_rate.den as u32)
                }
                (StreamParams::Audio { .. }, CodecId::Flac) => {
                    let (rate, channels, bits) = flac_audio_params(s)?;
                    (Kind::Audio { rate, channels, bits }, rate, 0)
                }
                _ => {
                    return Err(Error::new(ErrorCode::UnsupportedCodec, format!("this MP4 writer holds h264, hevc and flac, not {}", s.codec.name()))
                        .alternatives(["h264", "hevc", "flac"]));
                }
            };
            self.tracks.push(Track {
                codec: s.codec,
                kind,
                timescale,
                video_delta,
                sizes: Vec::new(),
                sync: Vec::new(),
                stts: Vec::new(),
                chunks: Vec::new(),
                pending: Vec::new(),
                pending_samples: 0,
                pending_dur: 0,
                duration: 0,
                params: ParamSets::default(),
                flac: FlacStats::new(),
            });
        }

        let mut brands: Vec<&[u8; 4]> = vec![b"isom", b"iso2"];
        for t in &self.tracks {
            match t.codec {
                CodecId::Hevc => brands.push(b"hvc1"),
                CodecId::H264 => brands.push(b"avc1"),
                _ => {}
            }
        }
        brands.push(b"mp41");
        let mut ftyp = b"isom".to_vec();
        ftyp.extend_from_slice(&512u32.to_be_bytes());
        for b in brands {
            ftyp.extend_from_slice(b);
        }
        let ftyp = bx(b"ftyp", &ftyp);
        self.out.write_all(&ftyp)?;
        self.mdat_start = ftyp.len() as u64;
        // 64-bit mdat header: size field 1, then the real size, patched in finish().
        self.out.write_all(&1u32.to_be_bytes())?;
        self.out.write_all(b"mdat")?;
        self.out.write_all(&0u64.to_be_bytes())?;
        Ok(())
    }

    fn write_packet(&mut self, p: &Packet) -> Result<()> {
        let t = p.stream;
        let tr = self.tracks.get_mut(t).ok_or_else(|| Error::new(ErrorCode::Internal, format!("packet for unknown output stream {t}")))?;
        let (sample, key, delta) = if tr.is_video() {
            let Some((sample, key)) = tr.video_sample(&p.data) else { return Ok(()) };
            (sample, key, tr.video_delta)
        } else {
            if p.duration <= 0 {
                return Err(Error::new(ErrorCode::Internal, "audio packet has no duration"));
            }
            (p.data.clone(), false, p.duration as u32)
        };
        if sample.len() > u32::MAX as usize {
            return Err(Error::invalid_input("a single sample exceeds 4 GiB"));
        }
        tr.sizes.push(sample.len() as u32);
        if key {
            tr.sync.push(tr.sizes.len() as u32); // sample numbers are 1-based
        }
        match tr.stts.last_mut() {
            Some(last) if last.1 == delta => last.0 += 1,
            _ => tr.stts.push((1, delta)),
        }
        if !tr.is_video() {
            tr.flac.record(p);
        }
        tr.duration += delta as u64;
        tr.pending.extend_from_slice(&sample);
        tr.pending_samples += 1;
        tr.pending_dur += delta as u64;
        if tr.pending_dur * 2 >= tr.timescale as u64 {
            self.flush_chunk(t)?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if let Some(i) = self.tracks.iter().position(|t| t.sizes.is_empty()) {
            return Err(Error::invalid_input(format!("output stream {i} received no packets, so there is nothing to put in the MP4")));
        }
        for t in 0..self.tracks.len() {
            self.flush_chunk(t)?;
        }
        let ms = self.tracks.iter().map(Track::movie_ms).max().unwrap_or(0) as u32;
        let mut mvhd = u32s(&[0, 0, 1000, ms]);
        mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
        mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
        mvhd.extend_from_slice(&[0u8; 10]);
        mvhd.extend_from_slice(&u32s(&MATRIX));
        mvhd.extend_from_slice(&[0u8; 24]);
        mvhd.extend_from_slice(&(self.tracks.len() as u32 + 1).to_be_bytes()); // next track id
        let mut moov = full(b"mvhd", 0, 0, &mvhd);
        for (i, t) in self.tracks.iter().enumerate() {
            moov.extend_from_slice(&t.trak(i as u32 + 1)?);
        }
        let moov = bx(b"moov", &moov);
        self.out.write_all(&moov)?;
        self.out.seek(SeekFrom::Start(self.mdat_start + 8))?;
        self.out.write_all(&(16 + self.data_len).to_be_bytes())?;
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::{PixelFormat, Rational};
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    /// A seekable sink the test can read back after the muxer is dropped.
    struct Shared(Arc<Mutex<Cursor<Vec<u8>>>>);
    impl std::io::Write for Shared {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for Shared {
        fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
            self.0.lock().unwrap().seek(p)
        }
    }

    #[test]
    fn splits_nals_with_three_and_four_byte_start_codes() {
        let d = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9];
        let n = split_nals(&d);
        assert_eq!(n, vec![&[0x67u8, 1, 2][..], &[0x68, 3][..], &[0x65, 9][..]]);
    }

    #[test]
    fn unescape_drops_only_emulation_prevention_bytes() {
        assert_eq!(unescape(&[1, 0, 0, 3, 1, 0, 0, 3, 3]), vec![1, 0, 0, 1, 0, 0, 3]);
    }

    fn mux(codec: CodecId, frames: &[Vec<u8>]) -> Vec<u8> {
        let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let mut m = Mp4Muxer::new(Box::new(Shared(sink.clone())));
        let info = StreamInfo {
            index: 0,
            codec,
            time_base: Rational::new(1, 30),
            duration: None,
            bits: None,
            params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(30000, 1001) },
        };
        m.write_header(&[info]).unwrap();
        for (i, f) in frames.iter().enumerate() {
            m.write_packet(&Packet { stream: 0, pts: i as i64, duration: 1, data: f.clone() }).unwrap();
        }
        m.finish().unwrap();
        let v = sink.lock().unwrap().get_ref().clone();
        v
    }

    /// Find a box by walking nested containers; returns its body.
    fn find<'a>(mut data: &'a [u8], path: &[&[u8; 4]]) -> Option<&'a [u8]> {
        let (head, rest) = path.split_first()?;
        while data.len() >= 8 {
            let size = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
            let (hdr, size) = if size == 1 { (16, u64::from_be_bytes(data[8..16].try_into().unwrap()) as usize) } else { (8, size) };
            if size < hdr || size > data.len() {
                return None;
            }
            if &data[4..8] == *head {
                let body = &data[hdr..size];
                return if rest.is_empty() { Some(body) } else { find(body, rest) };
            }
            data = &data[size..];
        }
        None
    }

    #[test]
    fn h264_file_has_consistent_index() {
        let sps = vec![0, 0, 0, 1, 0x67, 100, 0, 40, 0xAC];
        let pps = vec![0, 0, 0, 1, 0x68, 0xEE, 0x3C];
        let idr = [&[0u8, 0, 0, 1, 0x65][..], &[7; 20]].concat();
        let p = [&[0u8, 0, 0, 1, 0x41][..], &[5; 6]].concat();
        let file = mux(CodecId::H264, &[[sps, pps, idr].concat(), p.clone(), p]);

        assert_eq!(&file[4..8], b"ftyp");
        let mdat_at = u32::from_be_bytes(file[0..4].try_into().unwrap()) as usize;
        assert_eq!(&file[mdat_at + 4..mdat_at + 8], b"mdat");
        let mdat_size = u64::from_be_bytes(file[mdat_at + 8..mdat_at + 16].try_into().unwrap()) as usize;
        let moov = find(&file[mdat_at + mdat_size..], &[b"moov"]).expect("moov follows mdat");
        let stbl = find(moov, &[b"trak", b"mdia", b"minf", b"stbl"]).unwrap();
        let sizes = find(stbl, &[b"stsz"]).unwrap();
        assert_eq!(u32::from_be_bytes(sizes[8..12].try_into().unwrap()), 3, "three samples");
        // Samples are length-prefixed and exclude the parameter sets: 4+21, 4+7, 4+7.
        assert_eq!(&sizes[12..24], &[0, 0, 0, 25, 0, 0, 0, 11, 0, 0, 0, 11]);
        let stss = find(stbl, &[b"stss"]).unwrap();
        assert_eq!(&stss[4..12], &[0, 0, 0, 1, 0, 0, 0, 1], "only sample 1 is a sync sample");
        let avcc = find(stbl, &[b"stsd"]).unwrap();
        let pos = avcc.windows(4).position(|w| w == b"avcC").unwrap();
        assert_eq!(&avcc[pos + 4..pos + 8], &[1, 100, 0, 40], "profile/compat/level come from the SPS");
        // The chunk offset must point at the first sample's length prefix.
        let co = find(stbl, &[b"co64"]).unwrap();
        let off = u64::from_be_bytes(co[8..16].try_into().unwrap()) as usize;
        assert_eq!(&file[off..off + 5], &[0, 0, 0, 21, 0x65]);
    }

    #[test]
    fn finishing_with_no_frames_is_an_error() {
        let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let mut m = Mp4Muxer::new(Box::new(Shared(sink)));
        let info = StreamInfo {
            index: 0,
            codec: CodecId::H264,
            time_base: Rational::new(1, 30),
            duration: None,
            bits: None,
            params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(30, 1) },
        };
        m.write_header(&[info]).unwrap();
        assert!(m.finish().is_err());
    }

    fn av_streams() -> [StreamInfo; 2] {
        [
            StreamInfo {
                index: 0,
                codec: CodecId::H264,
                time_base: Rational::new(1, 30),
                duration: None,
                bits: None,
                params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(30, 1) },
            },
            StreamInfo {
                index: 1,
                codec: CodecId::Flac,
                time_base: Rational::new(1, 48000),
                duration: None,
                bits: Some(16),
                params: StreamParams::Audio { sample_rate: 48000, channels: 2 },
            },
        ]
    }

    #[test]
    fn audio_and_video_become_two_interleaved_tracks() {
        let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let mut m = Mp4Muxer::new(Box::new(Shared(sink.clone())));
        m.write_header(&av_streams()).unwrap();
        let idr = [&[0u8, 0, 0, 1, 0x67, 100, 0, 40, 0xAC, 0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0, 0, 0, 1, 0x65][..], &[7; 9]].concat();
        let p = [&[0u8, 0, 0, 1, 0x41][..], &[5; 4]].concat();
        // One second of media: 30 video frames and 12 FLAC frames of 4000 samples, as a merged stream delivers them.
        let mut a = 0;
        for f in 0..30 {
            m.write_packet(&Packet { stream: 0, pts: f, duration: 1, data: if f == 0 { idr.clone() } else { p.clone() } }).unwrap();
            while a < 12 && a * 4000 <= f * 1600 {
                m.write_packet(&Packet { stream: 1, pts: a * 4000, duration: 4000, data: vec![0xFF, 0xF8, a as u8, 1, 2, 3] }).unwrap();
                a += 1;
            }
        }
        m.finish().unwrap();
        let file = sink.lock().unwrap().get_ref().clone();

        let mdat_at = u32::from_be_bytes(file[0..4].try_into().unwrap()) as usize;
        let mdat_size = u64::from_be_bytes(file[mdat_at + 8..mdat_at + 16].try_into().unwrap()) as usize;
        let moov = find(&file[mdat_at + mdat_size..], &[b"moov"]).expect("moov follows mdat");
        // Two tracks: the second `trak` is found by skipping the first.
        let first = find(moov, &[b"trak"]).unwrap();
        let rest = &moov[moov.windows(4).position(|w| w == b"trak").unwrap() - 4 + 8 + first.len()..];
        let second = find(rest, &[b"trak"]).expect("second track");
        assert_eq!(find(first, &[b"mdia", b"hdlr"]).unwrap()[8..12], *b"vide");
        assert_eq!(find(second, &[b"mdia", b"hdlr"]).unwrap()[8..12], *b"soun");

        let stbl = find(second, &[b"mdia", b"minf", b"stbl"]).unwrap();
        let stsd = find(stbl, &[b"stsd"]).unwrap();
        let at = stsd.windows(4).position(|w| w == b"dfLa").unwrap();
        // dfLa: version/flags, block header (last, STREAMINFO, 34), then STREAMINFO itself.
        assert_eq!(&stsd[at + 8..at + 12], &[0x80, 0, 0, 34]);
        let si = &stsd[at + 12..at + 12 + 34];
        let packed = u64::from_be_bytes(si[10..18].try_into().unwrap());
        assert_eq!(packed >> 44, 48000, "sample rate");
        assert_eq!((packed >> 41) & 7, 1, "two channels");
        assert_eq!((packed >> 36) & 31, 15, "16 bits");
        assert_eq!(packed & 0xF_FFFF_FFFF, 48000, "total samples");
        let stsz = find(stbl, &[b"stsz"]).unwrap();
        assert_eq!(u32::from_be_bytes(stsz[8..12].try_into().unwrap()), 12, "twelve FLAC frames");
        assert!(find(stbl, &[b"stss"]).is_none(), "audio needs no sync table");

        // Chunks are half a second, so each track has two.
        let video_co = find(first, &[b"mdia", b"minf", b"stbl", b"co64"]).unwrap();
        let audio_co = find(stbl, &[b"co64"]).unwrap();
        let offs = |c: &[u8]| -> Vec<u64> { c[8..].chunks(8).map(|b| u64::from_be_bytes(b.try_into().unwrap())).collect() };
        let (v, a) = (offs(video_co), offs(audio_co));
        assert_eq!((v.len(), a.len()), (2, 2));
        let mut all = vec![(v[0], 'v'), (v[1], 'v'), (a[0], 'a'), (a[1], 'a')];
        all.sort();
        // Audio fills its second chunk just before video fills its own, so the order is a v a v.
        assert_eq!(all.iter().map(|x| x.1).collect::<String>(), "avav", "tracks should interleave");
    }

    #[test]
    fn two_streams_of_one_kind_are_rejected() {
        let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let mut m = Mp4Muxer::new(Box::new(Shared(sink)));
        let [v, _] = av_streams();
        assert!(m.write_header(&[v.clone(), v]).is_err());
    }
}
