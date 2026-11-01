//! Container demuxers and muxers, and the format registry.

mod adts;
mod annexb;
mod audio_stream;
pub(crate) mod flac;
mod mkv;
mod mkvwrite;
mod mp4;
mod mp3;
mod mp4demux;
pub mod stream;
mod ogg;
mod vpxav1;
mod oggwrite;
pub mod renditions;
pub mod rtp;
pub mod ts;
mod wav;
mod jpeg;
mod keyframe;
pub use keyframe::is_keyframe;
mod y4m;

use ironav_core::{CodecId, Demuxer, Error, ErrorCode, MediaType, Muxer, Result};
use serde::Serialize;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct FormatSpec {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub description: &'static str,
    pub media: MediaType,
    pub demux: bool,
    pub mux: bool,
    pub codecs: &'static [CodecId],
}

impl FormatSpec {
    /// True if the format can hold a stream of this media type (it holds at least one codec of it).
    pub fn holds(&self, media: MediaType) -> bool {
        self.codecs.iter().any(|c| c.media_type() == media)
    }
}

pub static FORMATS: &[FormatSpec] = &[
    FormatSpec {
        name: "wav",
        extensions: &["wav", "wave"],
        description: "RIFF/WAVE uncompressed PCM audio",
        media: MediaType::Audio,
        demux: true,
        mux: true,
        codecs: &[CodecId::PcmU8, CodecId::PcmS16Le, CodecId::PcmS24Le, CodecId::PcmS32Le, CodecId::PcmF32Le, CodecId::PcmF64Le],
    },
    FormatSpec {
        name: "jpeg",
        extensions: &["jpg", "jpeg"],
        description: "One JPEG image (baseline, 8-bit; gray, 4:2:0, 4:2:2 or 4:4:4): read as a single Motion-JPEG frame, written from one",
        media: MediaType::Video,
        demux: true,
        mux: true,
        codecs: &[CodecId::Mjpeg],
    },
    FormatSpec {
        name: "y4m",
        extensions: &["y4m"],
        description: "YUV4MPEG2 uncompressed planar video",
        media: MediaType::Video,
        demux: true,
        mux: true,
        codecs: &[CodecId::RawVideo],
    },
    FormatSpec {
        name: "flac",
        extensions: &["flac"],
        description: "Native FLAC lossless audio",
        media: MediaType::Audio,
        demux: true,
        mux: true,
        codecs: &[CodecId::Flac],
    },
    FormatSpec {
        name: "mp4",
        extensions: &["mp4", "m4a", "m4v"],
        description: "MP4 (ISO BMFF) with H.264 or HEVC video (or VP9 or AV1 copied from another file) and optionally AAC, FLAC or ALAC audio (AAC copied or encoded). Read as well as written; written with the index first (fast-start), or fragmented (moov, then a moof and mdat per fragment) when a fragment length is given; fragmented files are read too",
        media: MediaType::Video,
        demux: true,
        mux: true,
        codecs: &[CodecId::H264, CodecId::Hevc, CodecId::Vp9, CodecId::Av1, CodecId::Aac, CodecId::Flac, CodecId::Alac],
    },
    FormatSpec {
        name: "mov",
        extensions: &["mov", "qt"],
        description: "QuickTime MOV (ISO BMFF with QuickTime sound entries): read as MP4 is, with H.264, HEVC, ProRes, Motion-JPEG or AAC video and audio, and PCM audio; written with a 'qt  ' brand and the MP4 index layout (fast-start)",
        media: MediaType::Video,
        demux: true,
        mux: true,
        codecs: &[CodecId::H264, CodecId::Hevc, CodecId::Vp9, CodecId::Av1, CodecId::Mjpeg, CodecId::ProRes, CodecId::Aac, CodecId::Flac, CodecId::Alac, CodecId::PcmU8, CodecId::PcmS16Le, CodecId::PcmS24Le, CodecId::PcmS32Le, CodecId::PcmF32Le, CodecId::PcmF64Le],
    },
    FormatSpec {
        name: "mkv",
        extensions: &["mkv", "mka"],
        description: "Matroska: read (also WebM, with any codec the file holds) and written with H.264, HEVC, VP8, VP9, AV1 or Motion-JPEG video (copied; VP8 and MJPEG also encoded by IronAV where it has the codec) and AAC, FLAC, MP3, Opus or Vorbis audio copied or encoded; one stream of each kind; no SeekHead",
        media: MediaType::Video,
        demux: true,
        mux: true,
        codecs: &[CodecId::H264, CodecId::Hevc, CodecId::Vp8, CodecId::Vp9, CodecId::Av1, CodecId::Mjpeg, CodecId::Aac, CodecId::Mp3, CodecId::Opus, CodecId::Vorbis, CodecId::Flac],
    },
    FormatSpec {
        name: "mp3",
        extensions: &["mp3"],
        description: "MP3 (MPEG-1/2/2.5 Layer III) files; ID3 tags are skipped and a Xing/Info frame is not played; written with an Info frame (frame count, gapless delay and padding)",
        media: MediaType::Audio,
        demux: true,
        mux: true,
        codecs: &[CodecId::Mp3],
    },
    FormatSpec {
        name: "webm",
        extensions: &["webm"],
        description: "WebM (written with VP8, VP9 or AV1 video copied from another file, and Opus or Vorbis audio); WebM files with any codec are read as Matroska",
        media: MediaType::Video,
        demux: true,
        mux: true,
        codecs: &[CodecId::Vp8, CodecId::Vp9, CodecId::Av1, CodecId::Opus, CodecId::Vorbis],
    },
    FormatSpec {
        name: "aac",
        extensions: &["aac", "adts"],
        description: "Raw AAC in ADTS; ID3 tags are skipped when reading",
        media: MediaType::Audio,
        demux: true,
        mux: true,
        codecs: &[CodecId::Aac],
    },
    FormatSpec {
        name: "ogg",
        extensions: &["ogg", "opus", "oga"],
        description: "Ogg: read Opus, Vorbis and FLAC audio (Speex and Theora are refused); write Opus, Vorbis (copied) and FLAC",
        media: MediaType::Audio,
        demux: true,
        mux: true,
        codecs: &[CodecId::Opus, CodecId::Vorbis, CodecId::Flac],
    },
    FormatSpec {
        name: "h264",
        extensions: &["h264", "264"],
        description: "Raw H.264 Annex B elementary stream (write-only)",
        media: MediaType::Video,
        demux: false,
        mux: true,
        codecs: &[CodecId::H264],
    },
    FormatSpec {
        name: "hevc",
        extensions: &["hevc", "h265", "265"],
        description: "Raw H.265/HEVC Annex B elementary stream (write-only)",
        media: MediaType::Video,
        demux: false,
        mux: true,
        codecs: &[CodecId::Hevc],
    },
    FormatSpec {
        name: "mpegts",
        extensions: &["ts", "m2ts", "mts"],
        description: "MPEG transport stream (broadcast, IPTV, HLS segments); 192-byte M2TS packets are read too",
        media: MediaType::Video,
        demux: true,
        mux: true,
        codecs: &[CodecId::H264, CodecId::Hevc, CodecId::Aac, CodecId::Mp3],
    },
];

pub fn find_format(name: &str) -> Result<&'static FormatSpec> {
    FORMATS.iter().find(|f| f.name == name || f.extensions.contains(&name)).ok_or_else(|| {
        Error::new(ErrorCode::UnsupportedFormat, format!("unknown format '{name}'"))
            .hint("see `ironav capabilities` for implemented formats; others appear in the ontology as planned")
            .alternatives(FORMATS.iter().map(|f| f.name))
    })
}

pub fn format_for_path(path: &Path) -> Result<&'static FormatSpec> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    find_format(&ext).map_err(|e| {
        Error::new(ErrorCode::UnsupportedFormat, format!("cannot infer an output format from extension '.{ext}'"))
            .hint("set the job's \"format\" field explicitly")
            .alternatives(e.alternatives)
    })
}

/// Open `path` and pick a demuxer from the file's magic bytes (extension is ignored).
pub fn open_demuxer(path: &Path) -> Result<Box<dyn Demuxer>> {
    let mut f = BufReader::with_capacity(1 << 20, File::open(path)?);
    // a transport stream shows itself by sync bytes 188 (or 192) bytes apart
    let mut magic = [0u8; 400];
    let n = f.read(&mut magic)?;
    f.seek(SeekFrom::Start(0))?;
    let magic = &magic[..n];
    // an ID3v2 tag fronts FLAC files as well as MP3 files: look at what follows it
    let behind_id3 = if magic.starts_with(b"ID3") && magic.len() >= 10 {
        let size = magic[6..10].iter().fold(0u64, |a, &b| (a << 7) | (b & 0x7F) as u64);
        let mut next = [0u8; 4];
        f.seek(SeekFrom::Start(10 + size + if magic[5] & 0x10 != 0 { 10 } else { 0 }))?;
        let got = f.read(&mut next)?;
        f.seek(SeekFrom::Start(0))?;
        Some(next[..got].to_vec())
    } else {
        None
    };
    if ts::packet_size(magic).is_some() {
        Ok(Box::new(ts::TsDemuxer::open(f)?))
    } else if magic.len() >= 12 && &magic[..4] == b"RIFF" && &magic[8..12] == b"WAVE" {
        Ok(Box::new(wav::WavDemuxer::open(f)?))
    } else if flac::looks_like_flac(magic) && behind_id3.as_ref().is_none_or(|b| b.starts_with(b"fLaC")) {
        Ok(Box::new(flac::FlacDemuxer::open(f)?))
    } else if mp4demux::looks_like_mp4(magic) {
        Ok(Box::new(mp4demux::Mp4Demuxer::open(f)?))
    } else if mkv::looks_like_mkv(magic) {
        // Matroska does its own buffering: it seeks over every frame while indexing.
        Ok(Box::new(mkv::open(f.into_inner())?))
    } else if ogg::looks_like_ogg(magic) {
        Ok(Box::new(ogg::open(f)?))
    } else if adts::looks_like_adts(magic) || behind_id3.as_ref().is_some_and(|b| b.len() >= 2 && b[0] == 0xFF && b[1] & 0xF6 == 0xF0) {
        Ok(Box::new(adts::open(f)?))
    } else if mp3::looks_like_mp3(magic) {
        Ok(Box::new(mp3::open(f)?))
    } else if jpeg::looks_like_jpeg(magic) {
        Ok(Box::new(jpeg::JpegDemuxer::open(f)?))
    } else if magic.starts_with(b"YUV4MPEG2") {
        Ok(Box::new(y4m::Y4mDemuxer::open(f)?))
    } else {
        Err(Error::new(ErrorCode::UnsupportedFormat, format!("unrecognised input format: {}", path.display()))
            .hint("only formats listed by `ironav capabilities` can be read so far")
            .alternatives(FORMATS.iter().filter(|f| f.demux).map(|f| f.name)))
    }
}

/// Anything a muxer can write to.
pub trait WriteSeek: Write + Seek + Send {}
impl<T: Write + Seek + Send> WriteSeek for T {}

pub fn create_muxer(path: &Path, format: &str) -> Result<Box<dyn Muxer>> {
    create_muxer_with(path, format, None)
}

/// [`create_muxer`], where `fragment_seconds` makes an MP4 or MOV writer produce a fragmented file, with a
/// fragment at a key frame once that many seconds have gone by. Other formats refuse it.
pub fn create_muxer_with(path: &Path, format: &str, fragment_seconds: Option<f64>) -> Result<Box<dyn Muxer>> {
    let spec = find_format(format)?;
    if fragment_seconds.is_some() && !matches!(spec.name, "mp4" | "mov") {
        return Err(Error::invalid_arg(format!("fragmented output is written to mp4 and mov files, not {}", spec.name)));
    }
    let out: Box<dyn WriteSeek> = Box::new(BufWriter::with_capacity(1 << 20, File::create(path)?));
    match spec.name {
        "wav" => Ok(Box::new(wav::WavMuxer::new(out))),
        "y4m" => Ok(Box::new(y4m::Y4mMuxer::new(out))),
        "jpeg" => Ok(Box::new(jpeg::JpegMuxer::new(out))),
        "flac" => Ok(Box::new(flac::FlacMuxer::new(out))),
        "mp4" => Ok(Box::new(match fragment_seconds {
            Some(s) => mp4::Mp4Muxer::fragmented(out, false, s),
            None => mp4::Mp4Muxer::with_faststart(out, path),
        })),
        "mov" => Ok(Box::new(match fragment_seconds {
            Some(s) => mp4::Mp4Muxer::fragmented(out, true, s),
            None => mp4::Mp4Muxer::quicktime(out, Some(path)),
        })),
        "aac" => Ok(Box::new(adts::AdtsMuxer::new(out))),
        "mp3" => Ok(Box::new(mp3::Mp3Muxer::new(out))),
        "mkv" => Ok(Box::new(mkvwrite::MatroskaWriter::new(out))),
        "ogg" => Ok(Box::new(oggwrite::OggWriter::new(out))),
        "webm" => Ok(Box::new(mkvwrite::MatroskaWriter::webm(out))),
        "h264" => Ok(Box::new(annexb::AnnexBMuxer::new(out, CodecId::H264))),
        "hevc" => Ok(Box::new(annexb::AnnexBMuxer::new(out, CodecId::Hevc))),
        "mpegts" => Ok(Box::new(ts::TsMuxer::new(out))),
        _ => Err(Error::new(ErrorCode::UnsupportedFormat, format!("format '{format}' cannot be written"))),
    }
}

#[cfg(test)]
mod seek_tests {
    use super::*;
    use std::io::Cursor;

    fn wav_bytes(frames: u32, rate: u32) -> Vec<u8> {
        let data: Vec<u8> = (0..frames).flat_map(|i| (i as i16).to_le_bytes()).collect();
        let mut f = b"RIFF".to_vec();
        f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        f.extend_from_slice(b"WAVEfmt ");
        f.extend_from_slice(&[16, 0, 0, 0, 1, 0, 1, 0]);
        f.extend_from_slice(&rate.to_le_bytes());
        f.extend_from_slice(&(rate * 2).to_le_bytes());
        f.extend_from_slice(&[2, 0, 16, 0]);
        f.extend_from_slice(b"data");
        f.extend_from_slice(&(data.len() as u32).to_le_bytes());
        f.extend_from_slice(&data);
        f
    }

    #[test]
    fn wav_seeks_to_the_exact_sample_and_back_to_the_start() {
        let mut d = wav::WavDemuxer::open(Cursor::new(wav_bytes(20_000, 8000))).unwrap();
        assert_eq!(d.seek(1.0).unwrap(), 1.0);
        let p = d.read_packet().unwrap().unwrap();
        assert_eq!(p.pts, 8000);
        assert_eq!(i16::from_le_bytes([p.data[0], p.data[1]]), 8000, "the sample at one second");
        assert_eq!(d.seek(99.0).unwrap(), 2.5, "past the end clamps to the end");
        assert!(d.read_packet().unwrap().is_none());
        d.seek(0.0).unwrap();
        assert_eq!(d.read_packet().unwrap().unwrap().pts, 0);
    }

    #[test]
    fn y4m_reports_its_length_and_seeks_by_frame() {
        let (w, h, n) = (4usize, 2usize, 10usize);
        let mut f = format!("YUV4MPEG2 W{w} H{h} F10:1 C420jpeg\n").into_bytes();
        for i in 0..n {
            f.extend_from_slice(b"FRAME\n");
            f.extend(std::iter::repeat_n(i as u8, w * h * 3 / 2));
        }
        let mut d = y4m::Y4mDemuxer::open(Cursor::new(f)).unwrap();
        assert_eq!(d.streams()[0].duration, Some(10));
        assert!((d.seek(0.45).unwrap() - 0.4).abs() < 1e-9, "frame 4 starts at 0.4 s");
        let p = d.read_packet().unwrap().unwrap();
        assert_eq!((p.pts, p.data[0]), (4, 4));
        d.seek(0.0).unwrap();
        assert_eq!(d.read_packet().unwrap().unwrap().pts, 0);
    }

    #[test]
    fn flac_seek_lands_on_the_frame_containing_the_time() {
        use ironav_codec::flac::FlacEncoder;
        use ironav_core::{AudioFrame, Encoder, Frame};
        let rate = 44_100u32;
        let samples: Vec<f64> = (0..rate as usize * 3).map(|i| ((i as f64) * 0.02).sin() * 0.5).collect();
        let mut enc = FlacEncoder::new(rate, 1, Some(16)).unwrap();
        let mut pkts = enc.encode(&Frame::Audio(AudioFrame { sample_rate: rate, channels: 1, pts: 0, samples })).unwrap();
        pkts.extend(enc.flush().unwrap());
        let mut file = b"fLaC".to_vec();
        file.extend_from_slice(&[0x80, 0, 0, 34]);
        let mut info = [0u8; 34];
        info[10..18].copy_from_slice(&(((rate as u64) << 44) | (15u64 << 36) | (rate as u64 * 3)).to_be_bytes());
        file.extend_from_slice(&info);
        for p in &pkts {
            file.extend_from_slice(&p.data);
        }
        let mut d = flac::FlacDemuxer::open(Cursor::new(file)).unwrap();
        let want = 1.5;
        let landed = d.seek(want).unwrap();
        let p = d.read_packet().unwrap().unwrap();
        assert_eq!(p.pts as f64 / rate as f64, landed);
        assert!(landed <= want && want < landed + p.duration as f64 / rate as f64, "frame [{landed}, +{}] holds {want}", p.duration);
        assert_eq!(p.data, pkts.iter().find(|q| q.pts == p.pts).unwrap().data);
        d.seek(0.0).unwrap();
        assert_eq!(d.read_packet().unwrap().unwrap().pts, 0);
    }
}
