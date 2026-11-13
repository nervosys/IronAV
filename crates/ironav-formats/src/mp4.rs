//! MP4 (ISO BMFF) writer for H.264, HEVC, VP9 or AV1 video and AAC, FLAC or ALAC audio, at most one track of each.
//!
//! H.264 and HEVC packets are Annex B access units from the encoder. They are rewritten as
//! length-prefixed samples, parameter sets are lifted out into the `avcC` / `hvcC` box, and the
//! `moov` index is first written after the media data; when the muxer is given its file path it then
//! copies the file once with the index moved in front ("fast start"). Decode times are constant
//! frame rate. A stream whose presentation order differs from decode order (B-frames) gets a `ctts`
//! box of composition offsets and an edit list that starts the track at its first shown frame.
//! FLAC and ALAC frames are stored as they come, one per sample, with the STREAMINFO block in a `dfLa` box
//! or the ALACSpecificConfig in an `alac` box;
//! AAC frames likewise, described by an `esds` box built from the stream's AudioSpecificConfig.
//! VP9 frames are stored as they come, with a `vpcC` box built from the first key frame's header;
//! AV1 temporal units lose their temporal delimiter and padding OBUs and keep the stream's `av1C`.
//!
//! Layout while writing: `ftyp`, `mdat` (64-bit size, patched at the end), `moov`. Each track's samples are
//! gathered into chunks of about half a second, so a player reading both tracks finds the
//! video and audio for the same moment close together in the file.

use crate::flac::{flac_audio_params, FlacStats};
use crate::vpxav1::{av1_sample, vp9_key, vpcc_body};
use ironav_codec::alac::specific_config;
use crate::WriteSeek;
use ironav_core::{CodecId, Error, ErrorCode, Muxer, Packet, Rational, Result, StreamInfo, StreamParams};
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
pub(crate) struct ParamSets {
    pub(crate) vps: Option<Vec<u8>>,
    pub(crate) sps: Option<Vec<u8>>,
    pub(crate) pps: Option<Vec<u8>>,
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
    /// Packet time base, to turn presentation times into media time units.
    time_base: Rational,
    /// Presentation time of the previous video sample, to catch reordered (B-frame) streams.
    last_pts: Option<i64>,
    reordered: bool,
    /// Presentation time of every video sample in decode order, in media time units.
    pts: Vec<i64>,
    params: ParamSets,
    flac: FlacStats,
    /// The AudioSpecificConfig of an AAC track, the `av1C` record of an AV1 track, or the `vpcC`
    /// body of a VP9 track (built at its first key frame).
    config: Vec<u8>,
    /// The AV1 sequence header's `reduced_still_picture_header`.
    av1_still: bool,
    /// Time skipped at the start of the stream (in `time_base` units), written as an edit list.
    lead_in: i64,
    /// The stream's declared length (in `time_base` units), which ends the edit list of an audio track.
    declared: Option<i64>,
    /// Fragmented output: the index has no sample tables, and samples are gathered per fragment.
    fragmented: bool,
    /// Fragmented output: the samples of the fragment being gathered (their data is in `pending`).
    frag: Vec<FragSample>,
    /// Fragmented output: the decode time (media units) of the next sample, and of the first sample
    /// of the gathered fragment.
    dts: u64,
    frag_dts: u64,
    /// Fragmented output: samples written so far, which `finish` checks for each track.
    frag_total: u64,
    /// Fragmented output: added to every composition offset, so that the first fragment's earliest
    /// offset is zero (as in the index-first file); set when the `moov` is written.
    frag_shift: i64,
}

/// One sample of a fragment: its size, duration, composition offset and whether it is a sync sample.
struct FragSample {
    size: u32,
    delta: u32,
    cts: i32,
    key: bool,
}

impl Track {
    fn is_video(&self) -> bool {
        matches!(self.kind, Kind::Video { .. })
    }

    /// The `dac3` body of an AC-3 track, taken from the first packet that holds a syncframe.
    fn ac3_config(&mut self, data: &[u8]) -> Result<()> {
        if self.codec == CodecId::Ac3 && self.config.is_empty() {
            let body = ironav_codec::ac3::dac3_body(data).ok_or_else(|| Error::invalid_input("an AC-3 packet holds no syncframe that MP4 can describe"))?;
            self.config = body.to_vec();
        }
        Ok(())
    }

    fn is_hevc(&self) -> bool {
        self.codec == CodecId::Hevc
    }

    fn sample_entry(&self) -> Result<Vec<u8>> {
        match self.kind {
            Kind::Video { width, height } => self.video_entry(width, height),
            Kind::Audio { rate, channels, bits } if self.codec == CodecId::Aac => Ok(self.aac_entry(rate, channels, bits)),
            Kind::Audio { rate, channels, bits } if self.codec == CodecId::Mp2 => Ok(self.mp4a_entry(rate, channels, bits, 0x69, &[])),
            Kind::Audio { rate, channels, bits } if self.codec == CodecId::Ac3 => Ok(self.ac3_entry(rate, channels, bits)),
            Kind::Audio { rate, channels, bits } if self.codec == CodecId::Alac => Ok(self.alac_entry(rate, channels, bits)),
            Kind::Audio { rate, channels, bits } if self.codec.sample_format().is_some() => Ok(self.lpcm_entry(rate, channels, bits)),
            Kind::Audio { rate, channels, bits } => Ok(self.audio_entry(rate, channels, bits)),
        }
    }

    fn video_entry(&self, width: u32, height: u32) -> Result<Vec<u8>> {
        let missing = |what: &str| Error::new(ErrorCode::Internal, format!("stream has no {what}; the encoder must emit parameter sets with its first frame"));
        let prores = ironav_codec::prores::Profile::from_fourcc(&self.config).unwrap_or(ironav_codec::prores::Profile::Hq).fourcc();
        let (kind, config) = match self.codec {
            CodecId::Hevc => (b"hvc1", self.hvcc(&missing)?),
            CodecId::Av1 => (b"av01", bx(b"av1C", &self.config)),
            CodecId::Vp9 if self.config.is_empty() => return Err(Error::invalid_input("the VP9 stream has no key frame to describe it to MP4")),
            CodecId::Vp9 => (b"vp09", bx(b"vpcC", &self.config)),
            CodecId::Mjpeg => (b"jpeg", Vec::new()),
            CodecId::ProRes => (&prores, Vec::new()),
            CodecId::Mpeg4 => (b"mp4v", visual_esds(&self.config, 0x20)),
            CodecId::Mpeg2Video => (b"mp4v", visual_esds(&self.config, 0x61)),
            _ => (b"avc1", self.avcc(&missing)?),
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

    /// `mp4a` sample entry with an `esds` box carrying the AudioSpecificConfig.
    fn aac_entry(&self, rate: u32, channels: u16, bits: u8) -> Vec<u8> {
        self.mp4a_entry(rate, channels, bits, 0x40, &self.config)
    }

    /// `mp4a` sample entry whose `esds` names object type `object` and carries `dsi` as the decoder
    /// specific info (none, for MPEG audio: FFmpeg writes MP2 as object type 0x69, with none).
    fn mp4a_entry(&self, rate: u32, channels: u16, bits: u8, object: u8, dsi: &[u8]) -> Vec<u8> {
        let mut e = vec![0u8; 6];
        e.extend_from_slice(&1u16.to_be_bytes()); // data reference index
        e.extend_from_slice(&[0u8; 8]);
        e.extend_from_slice(&channels.to_be_bytes());
        e.extend_from_slice(&(bits as u16).to_be_bytes());
        e.extend_from_slice(&[0u8; 4]);
        e.extend_from_slice(&(rate.min(65535) << 16).to_be_bytes());
        let mut config = vec![object, 0x15, 0, 0, 0]; // the object, audio stream, buffer size unknown
        config.extend_from_slice(&[0u8; 8]); // maximum and average bit rate unknown
        if !dsi.is_empty() {
            config.extend_from_slice(&[5, dsi.len() as u8]);
            config.extend_from_slice(dsi);
        }
        let mut es = vec![0, 0, 0]; // ES_ID, no optional fields
        es.extend_from_slice(&[4, config.len() as u8]);
        es.extend_from_slice(&config);
        es.extend_from_slice(&[6, 1, 2]); // SL config: MP4 file
        let mut esds = vec![3, es.len() as u8];
        esds.extend_from_slice(&es);
        e.extend_from_slice(&full(b"esds", 0, 0, &esds));
        bx(b"mp4a", &e)
    }

    /// QuickTime `lpcm` sample entry, version 2: little-endian samples, the format in its flags.
    fn lpcm_entry(&self, rate: u32, channels: u16, bits: u8) -> Vec<u8> {
        let flags: u32 = match self.codec {
            CodecId::PcmF32Le | CodecId::PcmF64Le => 1 | 8,
            CodecId::PcmU8 => 8,
            _ => 4 | 8,
        };
        let mut e = vec![0u8; 6];
        e.extend_from_slice(&1u16.to_be_bytes()); // data reference index
        e.extend_from_slice(&2u16.to_be_bytes()); // version
        e.extend_from_slice(&[0u8; 6]); // revision, vendor
        e.extend_from_slice(&3u16.to_be_bytes()); // channels, as version 2 always states them
        e.extend_from_slice(&16u16.to_be_bytes());
        e.extend_from_slice(&[0xFF, 0xFE, 0, 0]); // compression id -2, packet size
        e.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // unused in version 2
        e.extend_from_slice(&72u32.to_be_bytes()); // size of the version 2 fields
        e.extend_from_slice(&(rate as f64).to_be_bytes());
        e.extend_from_slice(&(channels as u32).to_be_bytes());
        e.extend_from_slice(&0x7F00_0000u32.to_be_bytes());
        e.extend_from_slice(&(bits as u32).to_be_bytes());
        e.extend_from_slice(&flags.to_be_bytes());
        e.extend_from_slice(&(channels as u32 * bits as u32 / 8).to_be_bytes()); // bytes per frame
        e.extend_from_slice(&1u32.to_be_bytes()); // frames per packet
        bx(b"lpcm", &e)
    }

    /// `ac-3` sample entry with the `dac3` box that describes the first syncframe (`self.config`).
    fn ac3_entry(&self, rate: u32, channels: u16, bits: u8) -> Vec<u8> {
        let mut e = vec![0u8; 6];
        e.extend_from_slice(&1u16.to_be_bytes()); // data reference index
        e.extend_from_slice(&[0u8; 8]);
        e.extend_from_slice(&channels.to_be_bytes());
        e.extend_from_slice(&(bits as u16).to_be_bytes());
        e.extend_from_slice(&[0u8; 4]); // pre-defined, reserved
        e.extend_from_slice(&(rate << 16).to_be_bytes());
        e.extend_from_slice(&bx(b"dac3", &self.config));
        bx(b"ac-3", &e)
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

    /// `alac` sample entry with an `alac` box holding the ALACSpecificConfig. The config comes from the
    /// stream's extradata when it has one (a copied ALAC track), otherwise it is built from the rate,
    /// channels and bit depth.
    fn alac_entry(&self, rate: u32, channels: u16, bits: u8) -> Vec<u8> {
        let mut e = vec![0u8; 6];
        e.extend_from_slice(&1u16.to_be_bytes()); // data reference index
        e.extend_from_slice(&[0u8; 8]);
        e.extend_from_slice(&channels.to_be_bytes());
        e.extend_from_slice(&16u16.to_be_bytes()); // sample size; the real depth is in the config
        e.extend_from_slice(&[0u8; 4]);
        e.extend_from_slice(&(if rate <= 65535 { rate << 16 } else { 0 }).to_be_bytes());
        let cfg: [u8; 24] = match self.config.len() {
            24 => self.config[..].try_into().unwrap(),
            36 => self.config[12..].try_into().unwrap(),
            _ => specific_config(rate, channels as u8, bits),
        };
        e.extend_from_slice(&full(b"alac", 0, 0, &cfg));
        bx(b"alac", &e)
    }

    fn avcc(&self, missing: &dyn Fn(&str) -> Error) -> Result<Vec<u8>> {
        Ok(bx(b"avcC", &avcc_body(&self.params, missing)?))
    }

    fn hvcc(&self, missing: &dyn Fn(&str) -> Error) -> Result<Vec<u8>> {
        Ok(bx(b"hvcC", &hvcc_body(&self.params, missing)?))
    }

    /// Track duration in the movie's millisecond time scale.
    fn movie_units(&self, movie_ts: u64) -> u64 {
        self.duration * movie_ts / self.timescale as u64
    }

    /// A packet time in this track's media time units (rounded to nearest).
    fn media_time(&self, pts: i64) -> i64 {
        let tb = self.time_base;
        if tb.num <= 0 || tb.den <= 0 {
            return pts * self.video_delta as i64; // no usable time base: the time is a frame count
        }
        let n = pts as i128 * tb.num as i128 * self.timescale as i128;
        let d = tb.den as i128;
        ((n + if n >= 0 { d / 2 } else { -(d / 2) }) / d) as i64
    }

    /// Composition offsets (presentation minus decode time, shifted to be non-negative) as
    /// run-length `(count, offset)` entries, and the shift that was added to make them so.
    fn composition(&self) -> (Vec<(u32, u32)>, i64) {
        let offsets: Vec<i64> = self.pts.iter().enumerate().map(|(i, &p)| p - i as i64 * self.video_delta as i64).collect();
        let shift = -offsets.iter().copied().min().unwrap_or(0).min(0);
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for &o in &offsets {
            let o = (o + shift).clamp(0, u32::MAX as i64) as u32;
            match runs.last_mut() {
                Some(last) if last.1 == o => last.0 += 1,
                _ => runs.push((1, o)),
            }
        }
        (runs, shift)
    }

    /// The `traf` of one fragment for this track: its id, decode time, and a version 1 `trun` whose samples
    /// carry their own duration, size, flags and composition offset. `data_at` is where the samples start
    /// in the `moof`'s `mdat` (the base is the `moof` itself).
    fn traf(&self, id: u32, data_at: i32) -> Vec<u8> {
        let tfhd = full(b"tfhd", 0, 0x02_0000, &u32s(&[id])); // default-base-is-moof
        let tfdt = full(b"tfdt", 1, 0, &self.frag_dts.to_be_bytes());
        let mut run = u32s(&[self.frag.len() as u32, data_at as u32]);
        for s in &self.frag {
            // a sync sample depends on nothing; any other depends on an earlier one
            let flags: u32 = if s.key { 0x0200_0000 } else { 0x0101_0000 };
            let cts = (s.cts as i64 + self.frag_shift) as i32;
            run.extend_from_slice(&u32s(&[s.delta, s.size, flags, cts as u32]));
        }
        // data offset, duration, size, flags and composition offset are present
        let trun = full(b"trun", 1, 0x0F01, &run);
        bx(b"traf", &[tfhd, tfdt, trun].concat())
    }

    /// `shift` is added to every chunk offset (used when the movie box is moved in front of the data).
    fn trak(&self, id: u32, shift: u64, movie_ts: u64) -> Result<Vec<u8>> {
        let ms = self.movie_units(movie_ts) as u32;
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
            co64.extend_from_slice(&(offset + shift).to_be_bytes());
        }
        let co64 = full(b"co64", 0, 0, &co64);

        let mut stbl = vec![stsd, stts];
        let mut edts = Vec::new();
        if self.fragmented {
            // The fragments carry the timing. The index trims the lead-in and the composition shift (as the
            // index-first file's edit list does), when the stream's length is known to end the edit.
            let start = self.media_time(self.lead_in) + self.frag_shift;
            if let (true, Some(d)) = (start > 0, self.declared.filter(|&d| d > 0)) {
                let start = start as u64;
                let shown = (self.media_time(d) as u64).saturating_sub(start) * movie_ts / self.timescale as u64;
                let mut elst = u32s(&[1, shown as u32, start as u32]);
                elst.extend_from_slice(&0x0001_0000u32.to_be_bytes());
                edts = bx(b"edts", &full(b"elst", 0, 0, &elst));
            }
        } else if self.is_video() && self.reordered {
            let (runs, shift) = self.composition();
            let mut ctts = u32s(&[runs.len() as u32]);
            for (count, offset) in runs {
                ctts.extend_from_slice(&u32s(&[count, offset]));
            }
            stbl.push(full(b"ctts", 0, 0, &ctts));
            // The source's own edit (if any), moved by the shift that keeps offsets non-negative.
            let start = self.media_time(self.lead_in) + shift;
            if start > 0 {
                let mut elst = u32s(&[1, self.movie_units(movie_ts) as u32, start as u32]);
                elst.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // media rate 1.0
                edts = bx(b"edts", &full(b"elst", 0, 0, &elst));
            }
        }
        if !self.fragmented && !self.is_video() && self.lead_in > 0 {
            let lead = self.media_time(self.lead_in) as u64;
            let lead_units = lead * movie_ts / self.timescale as u64;
            // the audio ends where the stream says, not at the end of the last (padded) frame
            let shown = self.movie_units(movie_ts).saturating_sub(lead_units);
            let shown = self.declared.filter(|&d| d > 0).map_or(shown, |d| shown.min(self.media_time(d) as u64 * movie_ts / self.timescale as u64));
            let mut elst = u32s(&[1, shown as u32, lead as u32]);
            elst.extend_from_slice(&0x0001_0000u32.to_be_bytes());
            edts = bx(b"edts", &full(b"elst", 0, 0, &elst));
        }
        if self.is_video() && !self.fragmented {
            let mut stss = u32s(&[self.sync.len() as u32]);
            stss.extend_from_slice(&u32s(&self.sync));
            stbl.push(full(b"stss", 0, 0, &stss));
        }
        stbl.extend([stsc, stsz, co64]);
        let stbl = bx(b"stbl", &stbl.concat());
        let minf = bx(b"minf", &[media_header, dinf, stbl].concat());
        let mdia = bx(b"mdia", &[mdhd, hdlr, minf].concat());
        Ok(bx(b"trak", &[tkhd, edts, mdia].concat()))
    }

    /// The sample for one video packet and whether it is a key frame: an Annex B access unit becomes
    /// length-prefixed (its parameter sets collected), AV1 loses the OBUs MP4 does not store.
    /// Returns `None` if the packet held nothing to store.
    fn video_sample(&mut self, p: &Packet) -> Result<Option<(Vec<u8>, bool)>> {
        match self.codec {
            CodecId::Vp9 => {
                let key = vp9_key(&p.data);
                if let (Some(k), true, Kind::Video { width, height }) = (&key, self.config.is_empty(), &self.kind) {
                    self.config = vpcc_body(k, *width, *height, Rational::new(self.timescale as i64, self.video_delta as i64))
                        .ok_or_else(|| Error::new(ErrorCode::UnsupportedFormat, "MP4 cannot describe 4:4:0 VP9").hint("write .webm or .mkv"))?;
                }
                Ok(Some((p.data.clone(), key.is_some())))
            }
            CodecId::Av1 => match av1_sample(&p.data, &mut self.av1_still) {
                Some((sample, _)) if sample.is_empty() => Ok(None),
                Some(x) => Ok(Some(x)),
                None => Err(Error::invalid_input(format!("an AV1 packet at {} does not parse as OBUs", p.pts))),
            },
            CodecId::Mjpeg | CodecId::ProRes => Ok(Some((p.data.clone(), true))),
            // an MPEG-4 or MPEG-2 picture is stored as it is; it is a key picture when its coding type is I
            CodecId::Mpeg4 | CodecId::Mpeg2Video => Ok(Some((p.data.clone(), crate::keyframe::is_keyframe(self.codec, &p.data) != Some(false)))),
            _ => Ok(annexb_sample(self.is_hevc(), &mut self.params, &p.data)),
        }
    }
}

/// The `esds` box of an MPEG-4 Visual (object type 0x20) or MPEG-2 Video (0x61, Main profile) sample
/// entry: an ES descriptor whose decoder specific info is the sequence header and extension, or the
/// visual object sequence and layer header (`config`), as the demuxer reads them back.
fn visual_esds(config: &[u8], object: u8) -> Vec<u8> {
    // a descriptor's length takes four bytes of seven bits each, the continuation bit set on all but the last
    let desc = |tag: u8, body: &[u8]| {
        let n = body.len() as u32;
        let mut d = vec![tag, 0x80 | (n >> 21) as u8 & 0x7F, 0x80 | (n >> 14) as u8 & 0x7F, 0x80 | (n >> 7) as u8 & 0x7F, (n & 0x7F) as u8];
        d.extend_from_slice(body);
        d
    };
    let mut dec = vec![object, 0x11, 0, 0, 0]; // the object type; a visual stream; buffer size unknown
    dec.extend_from_slice(&[0u8; 8]); // maximum and average bit rate unknown
    dec.extend(desc(5, config));
    let mut es = vec![0, 1, 0]; // ES_ID 1, no optional fields
    es.extend(desc(4, &dec));
    es.extend(desc(6, &[2])); // SL config: MP4 file
    full(b"esds", 0, 0, &desc(3, &es))
}

/// The body of an `avcC` box (also Matroska's H.264 CodecPrivate).
pub(crate) fn avcc_body(params: &ParamSets, missing: &dyn Fn(&str) -> Error) -> Result<Vec<u8>> {
    let sps = params.sps.as_ref().ok_or_else(|| missing("SPS"))?;
    let pps = params.pps.as_ref().ok_or_else(|| missing("PPS"))?;
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
    Ok(c)
}

/// The body of an `hvcC` box (also Matroska's HEVC CodecPrivate).
pub(crate) fn hvcc_body(params: &ParamSets, missing: &dyn Fn(&str) -> Error) -> Result<Vec<u8>> {
    let sps = params.sps.as_ref().ok_or_else(|| missing("SPS"))?;
    let pps = params.pps.as_ref().ok_or_else(|| missing("PPS"))?;
    let vps = params.vps.as_ref().ok_or_else(|| missing("VPS"))?;
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
    let (luma_minus8, chroma_minus8) = hevc_bit_depths(&p).unwrap_or((0, 0));
    c.push(0xF8 | luma_minus8); // luma bit depth - 8
    c.push(0xF8 | chroma_minus8); // chroma bit depth - 8
    c.extend_from_slice(&0u16.to_be_bytes()); // average frame rate: unspecified
    c.push((1 << 3) | (nesting << 2) | 3); // 1 temporal layer, 4-byte lengths
    c.push(3);
    for (ty, nal) in [(32u8, vps), (33, sps), (34, pps)] {
        c.push(0x80 | ty);
        c.extend_from_slice(&1u16.to_be_bytes());
        c.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        c.extend_from_slice(nal);
    }
    Ok(c)
}


/// Convert one Annex B access unit to a length-prefixed sample, collecting parameter sets.
/// Returns `None` if the packet held nothing but parameter sets and delimiters.
pub(crate) fn annexb_sample(hevc: bool, params: &mut ParamSets, data: &[u8]) -> Option<(Vec<u8>, bool)> {
    let mut sample = Vec::with_capacity(data.len() + 16);
    let mut key = false;
    for nal in split_nals(data) {
        let ty = if hevc { (nal[0] >> 1) & 0x3F } else { nal[0] & 0x1F };
        match (hevc, ty) {
            (false, 7) => params.sps = Some(nal.to_vec()),
            (false, 8) => params.pps = Some(nal.to_vec()),
            (true, 32) => params.vps = Some(nal.to_vec()),
            (true, 33) => params.sps = Some(nal.to_vec()),
            (true, 34) => params.pps = Some(nal.to_vec()),
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

pub struct Mp4Muxer {
    out: Box<dyn WriteSeek>,
    tracks: Vec<Track>,
    mdat_start: u64,
    data_len: u64,
    /// When set, `finish` rewrites this file with the movie box in front of the media data.
    faststart: Option<std::path::PathBuf>,
    /// QuickTime (MOV): a `qt  ` brand, and PCM and Motion-JPEG tracks as well.
    quicktime: bool,
    /// Fragmented output (see the module notes): the fragment length and the state of the fragment.
    fragments: Option<Fragments>,
}

/// The state of a fragmented writer: the fragment length in seconds, the number of fragments written,
/// the time (seconds) of the first sample gathered, and whether the `moov` has been written.
struct Fragments {
    seconds: f64,
    seq: u32,
    start: Option<f64>,
    moov_written: bool,
    /// A cut was asked for (`FragmentedMp4Writer::flush`) and waits for the next key frame.
    cut: bool,
}

/// Sample size in bits of a PCM codec.
fn pcm_bits(c: CodecId) -> u8 {
    match c {
        CodecId::PcmU8 => 8,
        CodecId::PcmS16Le => 16,
        CodecId::PcmS24Le => 24,
        CodecId::PcmS32Le | CodecId::PcmF32Le => 32,
        _ => 64,
    }
}

/// Luma and chroma bit depth (minus 8) from an unescaped HEVC SPS, for a stream with a single
/// temporal sub-layer (what the hardware encoder writes). `None` when the SPS is not that shape.
fn hevc_bit_depths(sps: &[u8]) -> Option<(u8, u8)> {
    if sps.len() < 16 || (sps[2] >> 1) & 7 != 0 {
        return None;
    }
    let (data, mut pos) = (sps, 15 * 8); // after the 2-byte NAL header, 1 byte, and 12-byte profile_tier_level
    let bit = |pos: &mut usize| -> Option<u32> {
        let b = *data.get(*pos / 8)?;
        let v = (b >> (7 - *pos % 8)) & 1;
        *pos += 1;
        Some(v as u32)
    };
    let ue = |pos: &mut usize| -> Option<u32> {
        let mut zeros = 0;
        while bit(pos)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut v = 0u32;
        for _ in 0..zeros {
            v = (v << 1) | bit(pos)?;
        }
        Some((1u32 << zeros) - 1 + v)
    };
    ue(&mut pos)?; // sps_seq_parameter_set_id
    let chroma = ue(&mut pos)?;
    if chroma == 3 {
        bit(&mut pos)?; // separate_colour_plane_flag
    }
    ue(&mut pos)?; // width
    ue(&mut pos)?; // height
    if bit(&mut pos)? == 1 {
        for _ in 0..4 {
            ue(&mut pos)?; // conformance window offsets
        }
    }
    let (l, c) = (ue(&mut pos)?, ue(&mut pos)?);
    (l < 8 && c < 8).then_some((l as u8, c as u8))
}

impl Mp4Muxer {
    pub fn new(out: Box<dyn WriteSeek>) -> Self {
        Self { out, tracks: Vec::new(), mdat_start: 0, data_len: 0, faststart: None, quicktime: false, fragments: None }
    }

    /// A fragmented MP4 (or QuickTime MOV) writer: the index comes first, with no samples in it, and then
    /// a `moof` and `mdat` for each fragment. A fragment is cut at the first key frame once it is at least
    /// `seconds` long (in a file with no video, at the first packet after that), so each one can be played
    /// alone. Used for live delivery and for files that are read while they are written.
    pub fn fragmented(out: Box<dyn WriteSeek>, quicktime: bool, seconds: f64) -> Self {
        Self { quicktime, fragments: Some(Fragments { seconds, seq: 0, start: None, moov_written: false, cut: false }), ..Self::new(out) }
    }

    /// Like `new`, but once the file is complete it is rewritten so that the index comes first and
    /// playback can begin before the download ends. `path` must be the file `out` writes to.
    pub fn with_faststart(out: Box<dyn WriteSeek>, path: &std::path::Path) -> Self {
        Self { faststart: Some(path.to_path_buf()), ..Self::new(out) }
    }

    /// The QuickTime (MOV) writer; `path`, the file `out` writes to, enables the fast-start rewrite.
    pub fn quicktime(out: Box<dyn WriteSeek>, path: Option<&std::path::Path>) -> Self {
        Self { quicktime: true, faststart: path.map(std::path::Path::to_path_buf), ..Self::new(out) }
    }

    /// The movie's time scale: a lone audio track's own, so that its edit list lengths are exact (as
    /// FFmpeg writes them), and milliseconds otherwise.
    fn movie_timescale(&self) -> u64 {
        match self.tracks.as_slice() {
            [t] if !t.is_video() => t.timescale as u64,
            _ => 1000,
        }
    }

    fn moov(&self, shift: u64) -> Result<Vec<u8>> {
        let movie_ts = self.movie_timescale();
        let ms = self.tracks.iter().map(|t| t.movie_units(movie_ts)).max().unwrap_or(0) as u32;
        let mut mvhd = u32s(&[0, 0, movie_ts as u32, ms]);
        mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
        mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
        mvhd.extend_from_slice(&[0u8; 10]);
        mvhd.extend_from_slice(&u32s(&MATRIX));
        mvhd.extend_from_slice(&[0u8; 24]);
        mvhd.extend_from_slice(&(self.tracks.len() as u32 + 1).to_be_bytes()); // next track id
        let mut moov = full(b"mvhd", 0, 0, &mvhd);
        for (i, t) in self.tracks.iter().enumerate() {
            moov.extend_from_slice(&t.trak(i as u32 + 1, shift, movie_ts)?);
        }
        if self.fragments.is_some() {
            // trex: the defaults of the fragments' samples are all given in each trun, so they are zero
            let mut mvex = Vec::new();
            for i in 0..self.tracks.len() {
                mvex.extend_from_slice(&full(b"trex", 0, 0, &u32s(&[i as u32 + 1, 1, 0, 0, 0])));
            }
            moov.extend_from_slice(&bx(b"mvex", &mvex));
        }
        Ok(bx(b"moov", &moov))
    }

    /// The `moof` of the gathered fragment: a `mfhd` and a `traf` for each track that has samples in it.
    /// `data_at` is where the first track's samples start, counted from the start of the `moof`.
    fn moof(&self, seq: u32, data_at: u64) -> Result<Vec<u8>> {
        let mut at = data_at;
        let mut body = full(b"mfhd", 0, 0, &u32s(&[seq]));
        for (i, t) in self.tracks.iter().enumerate() {
            if t.frag.is_empty() {
                continue;
            }
            let run = i32::try_from(at).map_err(|_| Error::invalid_input("a fragment is larger than 2 GiB"))?;
            body.extend_from_slice(&t.traf(i as u32 + 1, run));
            at += t.pending.len() as u64;
        }
        Ok(bx(b"moof", &body))
    }

    /// Write the gathered fragment as a `moof` and an `mdat` (preceded by the `moov` the first time).
    fn write_fragment(&mut self) -> Result<()> {
        let written = self.fragments.as_ref().is_some_and(|f| f.moov_written);
        if !written {
            // the first fragment's earliest composition offset becomes zero, which the edit list then cancels
            for t in &mut self.tracks {
                t.frag_shift = -t.frag.iter().map(|s| s.cts as i64).min().unwrap_or(0).min(0);
            }
            let moov = self.moov(0)?;
            self.out.write_all(&moov)?;
            if let Some(f) = self.fragments.as_mut() {
                f.moov_written = true;
            }
        }
        let total: u64 = self.tracks.iter().map(|t| t.pending.len() as u64).sum();
        if total == 0 {
            return Ok(());
        }
        let seq = self.fragments.as_ref().map_or(0, |f| f.seq) + 1;
        // the mdat header is 8 bytes, or 16 when its size needs 64 bits
        let mdat = if total + 8 <= u32::MAX as u64 {
            [((total + 8) as u32).to_be_bytes().to_vec(), b"mdat".to_vec()].concat()
        } else {
            [1u32.to_be_bytes().to_vec(), b"mdat".to_vec(), (total + 16).to_be_bytes().to_vec()].concat()
        };
        // the moof's size does not depend on the offsets it holds, so measure it first
        let head = self.moof(seq, 0)?.len() as u64 + mdat.len() as u64;
        let moof = self.moof(seq, head)?;
        self.out.write_all(&moof)?;
        self.out.write_all(&mdat)?;
        for t in &mut self.tracks {
            self.out.write_all(&t.pending)?;
            t.pending.clear();
            t.frag.clear();
        }
        if let Some(f) = self.fragments.as_mut() {
            f.seq = seq;
            f.start = None;
            f.cut = false;
        }
        Ok(())
    }

    /// One packet of a fragmented file: its sample joins the fragment being gathered. A video key frame
    /// (or any audio packet, when there is no video) ends the fragment once it is long enough.
    fn write_fragment_packet(&mut self, p: &Packet) -> Result<()> {
        let t = p.stream;
        let tr = self.tracks.get_mut(t).ok_or_else(|| Error::new(ErrorCode::Internal, format!("packet for unknown output stream {t}")))?;
        let (sample, key, delta, cts) = if tr.is_video() {
            let Some((sample, key)) = tr.video_sample(p)? else { return Ok(()) };
            let pts = tr.media_time(p.pts);
            (sample, key, tr.video_delta, Some(pts))
        } else {
            if p.duration <= 0 {
                return Err(Error::new(ErrorCode::Internal, "audio packet has no duration"));
            }
            tr.ac3_config(&p.data)?;
            (p.data.clone(), true, p.duration as u32, None)
        };
        if sample.len() > u32::MAX as usize {
            return Err(Error::invalid_input("a single sample exceeds 4 GiB"));
        }
        if !tr.is_video() && tr.codec == CodecId::Flac {
            tr.flac.record(p);
        }
        let at = p.pts as f64 * tr.time_base.as_f64();
        let cut_here = if tr.is_video() { key } else { !self.tracks.iter().any(Track::is_video) };
        let (seconds, start, cut) = match self.fragments.as_ref() {
            Some(f) => (f.seconds, f.start, f.cut),
            None => (0.0, None, false),
        };
        if cut_here && start.is_some_and(|s| cut || at - s >= seconds) {
            self.write_fragment()?;
        }
        if let Some(f) = self.fragments.as_mut() {
            f.start.get_or_insert(at);
        }
        let tr = &mut self.tracks[t];
        if tr.frag.is_empty() {
            tr.frag_dts = tr.dts;
        }
        // composition offset: presentation time minus decode time (negative only before the first frame's delay)
        let cts = cts.map_or(0, |pts| (pts - tr.dts as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32);
        tr.frag.push(FragSample { size: sample.len() as u32, delta, cts, key });
        tr.pending.extend_from_slice(&sample);
        tr.dts += delta as u64;
        tr.frag_total += 1;
        Ok(())
    }

    /// Close the fragment being gathered ahead of time. Without video that is done at once; with video the
    /// fragment ends at the next key frame (a fragment is only cut where it can be played alone), and the
    /// samples gathered until then stay pending.
    fn cut_fragment(&mut self) -> Result<()> {
        if self.tracks.iter().any(Track::is_video) {
            if let Some(f) = self.fragments.as_mut() {
                f.cut = true;
            }
            return Ok(());
        }
        if self.tracks.iter().all(|t| t.pending.is_empty()) {
            return Ok(());
        }
        self.write_fragment()
    }

    /// Copy the finished file to a new one in the order ftyp, moov, mdat, then replace it.
    fn move_index_first(&mut self, path: &std::path::Path) -> Result<()> {
        use std::io::{Read, Write};
        // The index has the same size wherever it sits, because offsets are fixed-width.
        let size = self.moov(0)?.len() as u64;
        let moov = self.moov(size)?;
        // Close our handle first: Windows cannot replace a file that is still open.
        self.out = Box::new(std::io::Cursor::new(Vec::new()));
        let mut src = std::fs::File::open(path)?;
        let tmp = path.with_extension("faststart.tmp");
        // created new: a name that is taken (left by a crash, or planted) is refused, not written through
        let tmp_file = ironav_core::create_new(&tmp)?;
        let result = (|| -> Result<()> {
            let mut dst = std::io::BufWriter::with_capacity(1 << 20, tmp_file);
            let mut head = vec![0u8; self.mdat_start as usize];
            src.read_exact(&mut head)?;
            dst.write_all(&head)?;
            dst.write_all(&moov)?;
            std::io::copy(&mut Read::by_ref(&mut src).take(16 + self.data_len), &mut dst)?;
            dst.flush()?;
            Ok(())
        })();
        drop(src);
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        ironav_core::replace_file(&tmp, path)?;
        Ok(())
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
                (StreamParams::Video { .. }, CodecId::Av1) if s.extradata.first() != Some(&0x81) || s.extradata.len() < 4 => {
                    return Err(Error::new(ErrorCode::UnsupportedFormat, "AV1 needs its av1C configuration record to go into an MP4")
                        .hint("copy it from an MP4, Matroska or WebM file, which carries it"));
                }
                (StreamParams::Video { .. }, CodecId::ProRes) if !self.quicktime => {
                    return Err(Error::new(ErrorCode::UnsupportedFormat, "ProRes is written to QuickTime (.mov) files").hint("write the output as .mov"));
                }
                (StreamParams::Video { width, height, frame_rate, .. }, CodecId::H264 | CodecId::Hevc | CodecId::Vp9 | CodecId::Av1 | CodecId::Mjpeg | CodecId::ProRes | CodecId::Mpeg4 | CodecId::Mpeg2Video) => {
                    if frame_rate.num <= 0 || frame_rate.den <= 0 || frame_rate.num > u32::MAX as i64 || frame_rate.den > u32::MAX as i64 {
                        return Err(Error::invalid_arg("MP4 needs a known, constant frame rate"));
                    }
                    if *width > 65535 || *height > 65535 {
                        return Err(Error::invalid_arg("MP4 sample entries limit dimensions to 65535"));
                    }
                    (Kind::Video { width: *width, height: *height }, frame_rate.num as u32, frame_rate.den as u32)
                }
                (StreamParams::Audio { sample_rate, channels }, CodecId::Aac) => {
                    if s.extradata.is_empty() || s.extradata.len() > 100 {
                        return Err(Error::new(ErrorCode::UnsupportedFormat, "AAC needs its AudioSpecificConfig (2 to 100 bytes) to go into an MP4")
                            .hint("copy the AAC from an MP4 or M4A file, which carries it"));
                    }
                    (Kind::Audio { rate: *sample_rate, channels: *channels, bits: 16 }, *sample_rate, 0)
                }
                (StreamParams::Audio { sample_rate, channels }, CodecId::Mp2 | CodecId::Ac3) => (Kind::Audio { rate: *sample_rate, channels: *channels, bits: 16 }, *sample_rate, 0),
                (StreamParams::Audio { sample_rate, channels }, c) if self.quicktime && c.sample_format().is_some() => {
                    (Kind::Audio { rate: *sample_rate, channels: *channels, bits: pcm_bits(c) }, *sample_rate, 0)
                }
                (StreamParams::Audio { .. }, CodecId::Flac) => {
                    let (rate, channels, bits) = flac_audio_params(s)?;
                    (Kind::Audio { rate, channels, bits }, rate, 0)
                }
                (StreamParams::Audio { sample_rate, channels }, CodecId::Alac) => {
                    (Kind::Audio { rate: *sample_rate, channels: *channels, bits: s.bits.unwrap_or(16) }, *sample_rate, 0)
                }
                _ => {
                    return Err(Error::new(ErrorCode::UnsupportedCodec, format!("this MP4 writer holds h264, hevc, vp9 and av1 (copied), aac (copied or encoded), mp2, ac3, flac and alac, not {}", s.codec.name()))
                        .alternatives(["h264", "hevc", "vp9", "av1", "aac", "mp2", "ac3", "flac", "alac"]));
                }
            };
            // the av1C record's configuration OBUs may hold the sequence header
            let mut av1_still = false;
            if s.codec == CodecId::Av1 {
                let _ = av1_sample(&s.extradata[4..], &mut av1_still);
            }
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
                time_base: s.time_base,
                last_pts: None,
                reordered: false,
                pts: Vec::new(),
                params: ParamSets::default(),
                flac: FlacStats::new(),
                config: if matches!(s.codec, CodecId::Vp9 | CodecId::Ac3) { Vec::new() } else { s.extradata.clone() },
                av1_still,
                lead_in: s.lead_in.max(0),
                declared: s.duration,
                fragmented: self.fragments.is_some(),
                frag: Vec::new(),
                dts: 0,
                frag_dts: 0,
                frag_total: 0,
                frag_shift: 0,
            });
        }

        let mut brands: Vec<&[u8; 4]> = vec![b"isom", b"iso2"];
        for t in &self.tracks {
            match t.codec {
                CodecId::Hevc => brands.push(b"hvc1"),
                CodecId::H264 => brands.push(b"avc1"),
                CodecId::Av1 => brands.push(b"av01"),
                _ => {}
            }
        }
        brands.push(b"mp41");
        let mut ftyp = b"isom".to_vec();
        ftyp.extend_from_slice(&512u32.to_be_bytes());
        for b in brands {
            ftyp.extend_from_slice(b);
        }
        let ftyp = if self.quicktime {
            let mut qt = b"qt  ".to_vec();
            qt.extend_from_slice(&0x2005_0300u32.to_be_bytes()); // minor version, as QuickTime writes it
            qt.extend_from_slice(b"qt  ");
            bx(b"ftyp", &qt)
        } else {
            bx(b"ftyp", &ftyp)
        };
        self.out.write_all(&ftyp)?;
        if self.fragments.is_some() {
            return Ok(()); // the moov follows with the first fragment, when every stream's description is known
        }
        self.mdat_start = ftyp.len() as u64;
        // 64-bit mdat header: size field 1, then the real size, patched in finish().
        self.out.write_all(&1u32.to_be_bytes())?;
        self.out.write_all(b"mdat")?;
        self.out.write_all(&0u64.to_be_bytes())?;
        Ok(())
    }

    fn write_packet(&mut self, p: &Packet) -> Result<()> {
        if self.fragments.is_some() {
            return self.write_fragment_packet(p);
        }
        let t = p.stream;
        let tr = self.tracks.get_mut(t).ok_or_else(|| Error::new(ErrorCode::Internal, format!("packet for unknown output stream {t}")))?;
        let (sample, key, delta) = if tr.is_video() {
            let Some((sample, key)) = tr.video_sample(p)? else { return Ok(()) };
            tr.reordered |= tr.last_pts.is_some_and(|last| p.pts < last);
            tr.last_pts = Some(p.pts);
            tr.pts.push(tr.media_time(p.pts));
            (sample, key, tr.video_delta)
        } else {
            if p.duration <= 0 {
                return Err(Error::new(ErrorCode::Internal, "audio packet has no duration"));
            }
            tr.ac3_config(&p.data)?;
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
        if !tr.is_video() && tr.codec == CodecId::Flac {
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
        let fragmented = self.fragments.is_some();
        let empty = |t: &Track| if fragmented { t.frag_total == 0 } else { t.sizes.is_empty() };
        if let Some(i) = self.tracks.iter().position(empty) {
            return Err(Error::invalid_input(format!("output stream {i} received no packets, so there is nothing to put in the MP4")));
        }
        if fragmented {
            self.write_fragment()?;
            return self.out.flush().map_err(Into::into);
        }
        for t in 0..self.tracks.len() {
            self.flush_chunk(t)?;
        }
        let moov = self.moov(0)?;
        self.out.write_all(&moov)?;
        self.out.seek(SeekFrom::Start(self.mdat_start + 8))?;
        self.out.write_all(&(16 + self.data_len).to_be_bytes())?;
        self.out.flush()?;
        match self.faststart.take() {
            Some(path) => self.move_index_first(&path),
            None => Ok(()),
        }
    }
}

/// A `Write`-and-`Seek` sink whose seeks fail, over a buffer shared with its owner: the fragmented writer
/// never seeks, and this lets `FragmentedMp4Writer` take any `Write`, with no `Send` or `'static` bound.
struct Staging(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Staging {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for Staging {
    fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "a fragmented MP4 stream cannot seek"))
    }
}

/// A streaming fragmented-MP4 writer: `ftyp`, then a `moov` with empty sample tables and a `mvex`, then a
/// `moof` and `mdat` per fragment, written to any `Write` with no seeking (for an HTTP response, say). It is
/// the box writer of the file muxer (`create_muxer_with(path, "mp4", Some(secs))`), so the same packets
/// give the same bytes. Holds one video and/or one audio stream of the codecs that muxer accepts
/// (H.264, HEVC, VP9, AV1, AAC, MP2, AC-3, FLAC, ALAC); as there, video has a constant frame rate.
///
/// A fragment is cut at the first key frame once it is `fragment_seconds` long (with no video, at the first
/// packet after that). Differences from the path-based file: none in the bytes; but the `moov` is not
/// written by `new`, it goes out with the first fragment, because it describes the streams from their first
/// packets (H.264 parameter sets are read from the first access unit). Only `ftyp` is written at once.
/// There is no `mfra`.
///
/// [`flush`](Self::flush) ends the fragment early: with no video at the next packet boundary, at once;
/// with video at the next key frame, so that every fragment starts at one (until it arrives, the video
/// packets gathered stay in the writer: a stream that wants a bounded delay needs regular key frames).
///
/// ```
/// use ironav_core::{CodecId, Packet, Rational, StreamInfo, StreamParams};
/// use ironav_formats::FragmentedMp4Writer;
///
/// let info = StreamInfo {
///     index: 0,
///     codec: CodecId::Aac,
///     time_base: Rational::new(1, 48_000),
///     duration: None,
///     bits: None,
///     extradata: vec![0x11, 0x88], // AudioSpecificConfig: AAC-LC, 48 kHz, mono
///     lead_in: 0,
///     params: StreamParams::Audio { sample_rate: 48_000, channels: 1 },
/// };
/// let mut w = FragmentedMp4Writer::new(Vec::new(), &[info], 2.0)?;
/// w.write_packet(&Packet { stream: 0, pts: 0, duration: 1024, data: vec![0x21, 0x10, 0x04] })?;
/// w.flush()?; // the fragment is closed and in the Vec
/// let bytes = w.finish()?;
/// assert_eq!(&bytes[4..8], b"ftyp");
/// # Ok::<(), ironav_core::Error>(())
/// ```
pub struct FragmentedMp4Writer<W: std::io::Write> {
    w: W,
    staged: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    mux: Mp4Muxer,
}

impl<W: std::io::Write> FragmentedMp4Writer<W> {
    /// Start a stream of `streams` (one video and/or one audio stream), writing `ftyp` to `w` now.
    pub fn new(w: W, streams: &[StreamInfo], fragment_seconds: f64) -> Result<Self> {
        let staged = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mux = Mp4Muxer::fragmented(Box::new(Staging(staged.clone())), false, fragment_seconds);
        mux.write_header(streams)?;
        let mut me = Self { w, staged, mux };
        me.drain()?;
        Ok(me)
    }

    fn drain(&mut self) -> Result<()> {
        let mut buf = self.staged.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !buf.is_empty() {
            self.w.write_all(&buf)?;
            buf.clear();
        }
        Ok(())
    }

    /// Add a packet; a finished fragment goes to `w` as soon as the packet that ends it arrives.
    pub fn write_packet(&mut self, p: &Packet) -> Result<()> {
        self.mux.write_packet(p)?;
        self.drain()
    }

    /// End the current fragment early (see the type's notes for video) and flush `w`.
    pub fn flush(&mut self) -> Result<()> {
        self.mux.cut_fragment()?;
        self.drain()?;
        self.w.flush()?;
        Ok(())
    }

    /// Write the last fragment and give back `w`. Errors when a stream received no packets.
    pub fn finish(mut self) -> Result<W> {
        self.mux.finish()?;
        self.drain()?;
        self.w.flush()?;
        Ok(self.w)
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
            extradata: Vec::new(),
            lead_in: 0,
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
    fn reordered_video_gets_composition_offsets_and_an_edit_list() {
        let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let mut m = Mp4Muxer::new(Box::new(Shared(sink.clone())));
        let info = StreamInfo {
            index: 0,
            codec: CodecId::H264,
            time_base: Rational::new(1, 30000),
            duration: None,
            bits: None,
            extradata: Vec::new(),
            lead_in: 1001,
            params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(30000, 1001) },
        };
        m.write_header(&[info]).unwrap();
        let sps = vec![0, 0, 0, 1, 0x67, 100, 0, 40, 0xAC];
        let pps = vec![0, 0, 0, 1, 0x68, 0xEE, 0x3C];
        let idr = [sps, pps, vec![0, 0, 0, 1, 0x65], vec![7; 20]].concat();
        let p = [&[0u8, 0, 0, 1, 0x41][..], &[5; 6]].concat();
        // Decode order I P B P with presentation order I B P P, one frame of delay.
        for (pts, data) in [(1001, &idr), (4004, &p), (2002, &p), (3003, &p)] {
            m.write_packet(&Packet { stream: 0, pts, duration: 1001, data: data.clone() }).unwrap();
        }
        m.finish().unwrap();
        let file = sink.lock().unwrap().get_ref().clone();
        let moov = find(&file, &[b"moov"]).unwrap();
        let ctts = find(moov, &[b"trak", b"mdia", b"minf", b"stbl", b"ctts"]).unwrap();
        let words: Vec<u32> = ctts[4..].chunks(4).map(|w| u32::from_be_bytes(w.try_into().unwrap())).collect();
        // dts 0, 1001, 2002, 3003 against pts 1001, 4004, 2002, 3003: three runs of offsets.
        assert_eq!(words, vec![3, 1, 1001, 1, 3003, 2, 0]);
        let elst = find(moov, &[b"trak", b"edts", b"elst"]).unwrap();
        assert_eq!(u32::from_be_bytes(elst[12..16].try_into().unwrap()), 1001, "the edit starts at the first shown frame");
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
    fn faststart_puts_the_index_first_and_keeps_every_offset_right() {
        let path = std::env::temp_dir().join(format!("ironav-faststart-{}.mp4", std::process::id()));
        let sps = vec![0, 0, 0, 1, 0x67, 100, 0, 40, 0xAC];
        let pps = vec![0, 0, 0, 1, 0x68, 0xEE, 0x3C];
        let idr = [&[0u8, 0, 0, 1, 0x65][..], &[7; 20]].concat();
        let p = [&[0u8, 0, 0, 1, 0x41][..], &[5; 6]].concat();
        let file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        let mut m = Mp4Muxer::with_faststart(Box::new(file), &path);
        let info = StreamInfo {
            index: 0,
            codec: CodecId::H264,
            time_base: Rational::new(1, 30),
            duration: None,
            bits: None,
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(30, 1) },
        };
        m.write_header(&[info]).unwrap();
        for (i, f) in [[sps, pps, idr].concat(), p.clone(), p].iter().enumerate() {
            m.write_packet(&Packet { stream: 0, pts: i as i64, duration: 1, data: f.clone() }).unwrap();
        }
        m.finish().unwrap();
        let file = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(!path.with_extension("faststart.tmp").exists());

        assert_eq!(&file[4..8], b"ftyp");
        let moov_at = u32::from_be_bytes(file[0..4].try_into().unwrap()) as usize;
        assert_eq!(&file[moov_at + 4..moov_at + 8], b"moov", "the index comes right after ftyp");
        let moov_len = u32::from_be_bytes(file[moov_at..moov_at + 4].try_into().unwrap()) as usize;
        assert_eq!(&file[moov_at + moov_len + 4..moov_at + moov_len + 8], b"mdat", "the data follows it");
        let stbl = find(&file[moov_at..], &[b"moov", b"trak", b"mdia", b"minf", b"stbl"]).unwrap();
        let co = find(stbl, &[b"co64"]).unwrap();
        let off = u64::from_be_bytes(co[8..16].try_into().unwrap()) as usize;
        assert_eq!(&file[off..off + 5], &[0, 0, 0, 21, 0x65], "the chunk offset points at the first sample");
        let total = moov_at + moov_len + 16 + (25 + 11 + 11);
        assert_eq!(file.len(), total, "nothing was lost or duplicated");
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
            extradata: Vec::new(),
            lead_in: 0,
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
                extradata: Vec::new(),
                lead_in: 0,
                params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(30, 1) },
            },
            StreamInfo {
                index: 1,
                codec: CodecId::Flac,
                time_base: Rational::new(1, 48000),
                duration: None,
                bits: Some(16),
                extradata: Vec::new(),
                lead_in: 0,
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
        let mut all = [(v[0], 'v'), (v[1], 'v'), (a[0], 'a'), (a[1], 'a')];
        all.sort();
        // Audio fills its second chunk just before video fills its own, so the order is a v a v.
        assert_eq!(all.iter().map(|x| x.1).collect::<String>(), "avav", "tracks should interleave");
    }

    /// The top-level boxes of a file, as (offset, kind).
    fn top_boxes(mut d: &[u8]) -> Vec<(usize, [u8; 4])> {
        let (mut at, mut out) = (0usize, Vec::new());
        while d.len() >= 8 {
            let size = u32::from_be_bytes(d[..4].try_into().unwrap()) as usize;
            out.push((at, d[4..8].try_into().unwrap()));
            if size < 8 || size > d.len() {
                break;
            }
            at += size;
            d = &d[size..];
        }
        out
    }

    #[test]
    fn fragmented_file_is_index_then_moof_mdat_pairs_that_read_back() {
        use crate::mp4demux::Mp4Demuxer;
        use ironav_core::Demuxer;
        let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        // fragments of at least half a second: a key frame every 10 frames at 30 fps cuts at 20, 40 and 60
        let mut m = Mp4Muxer::fragmented(Box::new(Shared(sink.clone())), false, 0.5);
        let info = StreamInfo {
            index: 0,
            codec: CodecId::H264,
            time_base: Rational::new(1, 30),
            duration: None,
            bits: None,
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(30, 1) },
        };
        m.write_header(&[info]).unwrap();
        let sps = vec![0, 0, 0, 1, 0x67, 100, 0, 40, 0xAC];
        let pps = vec![0, 0, 0, 1, 0x68, 0xEE, 0x3C];
        let idr = [&[0u8, 0, 0, 1, 0x65][..], &[7; 20]].concat();
        let p = [&[0u8, 0, 0, 1, 0x41][..], &[5; 6]].concat();
        for i in 0..70 {
            let data = match i {
                0 => [sps.clone(), pps.clone(), idr.clone()].concat(),
                _ if i % 10 == 0 => idr.clone(),
                _ => p.clone(),
            };
            m.write_packet(&Packet { stream: 0, pts: i, duration: 1, data }).unwrap();
        }
        m.finish().unwrap();
        let file = sink.lock().unwrap().get_ref().clone();

        let boxes = top_boxes(&file);
        let kinds: Vec<&[u8; 4]> = boxes.iter().map(|b| &b.1).collect();
        assert_eq!(kinds[0], b"ftyp");
        assert_eq!(kinds[1], b"moov", "the index comes right after ftyp");
        assert_eq!(kinds[2..], [b"moof", b"mdat", b"moof", b"mdat", b"moof", b"mdat", b"moof", b"mdat"], "a fragment starts at 0, 20, 40 and 60");
        let moov = find(&file, &[b"moov"]).unwrap();
        assert!(find(moov, &[b"mvex", b"trex"]).is_some());
        let stsz = find(moov, &[b"trak", b"mdia", b"minf", b"stbl", b"stsz"]).unwrap();
        assert_eq!(u32::from_be_bytes(stsz[8..12].try_into().unwrap()), 0, "the index holds no samples");

        // the first fragment's data offset points at its first sample, the length-prefixed key frame
        let first_moof = boxes[2].0;
        let trun = find(&file[first_moof..], &[b"moof", b"traf", b"trun"]).unwrap();
        let data_at = i32::from_be_bytes(trun[8..12].try_into().unwrap()) as usize;
        assert_eq!(&file[first_moof + data_at..first_moof + data_at + 5], &[0, 0, 0, 21, 0x65]);

        let mut d = Mp4Demuxer::open(Cursor::new(file)).unwrap();
        let mut pts = Vec::new();
        while let Some(pk) = d.read_packet().unwrap() {
            pts.push(pk.pts);
        }
        assert_eq!(pts, (0..70).collect::<Vec<i64>>(), "every frame reads back in order");
    }

    #[test]
    fn two_streams_of_one_kind_are_rejected() {
        let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let mut m = Mp4Muxer::new(Box::new(Shared(sink)));
        let [v, _] = av_streams();
        assert!(m.write_header(&[v.clone(), v]).is_err());
    }
}
