//! Container demuxers and muxers, and the format registry.

mod wav;
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

pub static FORMATS: &[FormatSpec] = &[
    FormatSpec {
        name: "wav",
        extensions: &["wav", "wave"],
        description: "RIFF/WAVE uncompressed PCM audio",
        media: MediaType::Audio,
        demux: true,
        mux: true,
        codecs: &[CodecId::PcmU8, CodecId::PcmS16Le, CodecId::PcmS32Le, CodecId::PcmF32Le, CodecId::PcmF64Le],
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
    let mut magic = [0u8; 12];
    let n = f.read(&mut magic)?;
    f.seek(SeekFrom::Start(0))?;
    let magic = &magic[..n];
    if magic.len() >= 12 && &magic[..4] == b"RIFF" && &magic[8..12] == b"WAVE" {
        Ok(Box::new(wav::WavDemuxer::open(f)?))
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
    let spec = find_format(format)?;
    let out: Box<dyn WriteSeek> = Box::new(BufWriter::with_capacity(1 << 20, File::create(path)?));
    match spec.name {
        "wav" => Ok(Box::new(wav::WavMuxer::new(out))),
        "y4m" => Ok(Box::new(y4m::Y4mMuxer::new(out))),
        _ => Err(Error::new(ErrorCode::UnsupportedFormat, format!("format '{format}' cannot be written"))),
    }
}
