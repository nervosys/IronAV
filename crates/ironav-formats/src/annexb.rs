//! Raw Annex B elementary streams (`.h264`, `.hevc`): the encoder's NAL units with start codes,
//! written back to back. There is no header, index or timing. Reading uses the live reader's
//! parser ([`crate::stream`]): access units come out in decoding order, numbered at the frame rate
//! the sequence parameter set's VUI states, else 25. Writing puts each packet's access unit into the
//! file as it is.

use crate::stream::{AnnexBStream, StreamDemuxer, StreamPacket};
use crate::WriteSeek;
use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Muxer, Packet, Result, StreamInfo};
use std::collections::VecDeque;
use std::io::Read;

/// Bytes read from the file at a time.
const CHUNK: usize = 1 << 20;
/// How much of a file is read, at most, looking for its sequence parameter set.
const MAX_HEAD: usize = 16 << 20;

/// The codec of a file whose first bytes are an Annex B start code and a NAL unit that opens a
/// stream (a sequence parameter set, an access unit delimiter or, for HEVC, a VPS).
pub fn looks_like(magic: &[u8]) -> Option<CodecId> {
    match crate::stream::sniff(magic)? {
        "h264" => Some(CodecId::H264),
        "hevc" => Some(CodecId::Hevc),
        _ => None,
    }
}

pub struct AnnexBDemuxer<R> {
    r: R,
    codec: CodecId,
    parser: AnnexBStream,
    /// Packets the parser has made that are not yet handed out.
    held: VecDeque<StreamPacket>,
    /// Bytes fed to the parser so far.
    fed: usize,
    eof: bool,
    ended: bool,
}

impl<R: Read + Send> AnnexBDemuxer<R> {
    /// Reads the head of `r` until its sequence parameter set has been seen.
    pub fn open(r: R, codec: CodecId) -> Result<Self> {
        let mut d = AnnexBDemuxer { r, codec, parser: AnnexBStream::new(codec, None), held: VecDeque::new(), fed: 0, eof: false, ended: false };
        while d.parser.streams().is_empty() {
            if d.fed > MAX_HEAD {
                return Err(Error::invalid_input(format!("no sequence parameter set in the first {} MiB", MAX_HEAD >> 20)));
            }
            if !d.pump()? {
                break;
            }
        }
        if d.parser.streams().is_empty() {
            return Err(Error::invalid_input("the stream holds no sequence parameter set"));
        }
        Ok(d)
    }

    /// Hands the parser the next chunk of input, or ends it at the end of input, and moves what it
    /// has made into `held`. False once the parser has ended and has nothing more.
    fn pump(&mut self) -> Result<bool> {
        while let Some(p) = self.parser.next_packet()? {
            self.held.push_back(p);
        }
        if self.eof {
            if self.ended {
                return Ok(false);
            }
            self.parser.end();
            self.ended = true;
            return Ok(true);
        }
        let mut buf = vec![0u8; CHUNK];
        let n = self.r.read(&mut buf)?;
        if n == 0 {
            self.eof = true;
        } else {
            self.parser.feed(&buf[..n])?;
            self.fed += n;
        }
        Ok(true)
    }
}

impl<R: Read + Send> Demuxer for AnnexBDemuxer<R> {
    fn format_name(&self) -> &'static str {
        if self.codec == CodecId::Hevc {
            "hevc"
        } else {
            "h264"
        }
    }

    fn streams(&self) -> &[StreamInfo] {
        self.parser.streams()
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(p) = self.held.pop_front() {
                return Ok(Some(p.packet));
            }
            if !self.pump()? && self.held.is_empty() {
                return Ok(None);
            }
        }
    }
}

pub struct AnnexBMuxer {
    out: Box<dyn WriteSeek>,
    codec: CodecId,
    started: bool,
}

impl AnnexBMuxer {
    pub fn new(out: Box<dyn WriteSeek>, codec: CodecId) -> Self {
        Self { out, codec, started: false }
    }
}

impl Muxer for AnnexBMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [s] = streams else {
            return Err(Error::invalid_arg("an Annex B stream holds exactly one video stream"));
        };
        if s.codec != self.codec {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("this format holds {}, not {}", self.codec.name(), s.codec.name()))
                .alternatives([self.codec.name()]));
        }
        Ok(())
    }

    fn write_packet(&mut self, p: &Packet) -> Result<()> {
        // The first packet must open with a start code, or the file is not a valid stream.
        if !self.started {
            if !(p.data.starts_with(&[0, 0, 0, 1]) || p.data.starts_with(&[0, 0, 1])) {
                return Err(Error::new(ErrorCode::Internal, "encoder output is not in Annex B form (no start code)"));
            }
            self.started = true;
        }
        self.out.write_all(&p.data)?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn raw_streams_are_sniffed_by_their_first_nal_unit() {
        assert_eq!(looks_like(&[0, 0, 0, 1, 0x67, 0x64, 0, 0x1f]), Some(CodecId::H264));
        assert_eq!(looks_like(&[0, 0, 1, 0x09, 0xF0]), Some(CodecId::H264));
        assert_eq!(looks_like(&[0, 0, 0, 1, 0x40, 0x01, 0x0c, 0x01]), Some(CodecId::Hevc));
        assert_eq!(looks_like(b"\0\0\0\x18ftypmp42"), None);
        assert_eq!(looks_like(b"RIFF\0\0\0\0WAVEfmt "), None);
    }

    #[test]
    fn a_stream_without_a_sequence_parameter_set_is_refused() {
        // two slices and no parameter set: nothing to give the streams' parameters from
        let slices = vec![0, 0, 0, 1, 0x41, 0x9a, 0, 0, 0, 1, 0x41, 0x9a];
        assert!(AnnexBDemuxer::open(Cursor::new(slices), CodecId::H264).is_err());
    }
}
