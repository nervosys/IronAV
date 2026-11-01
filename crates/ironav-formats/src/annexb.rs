//! Raw Annex B elementary streams (`.h264`, `.hevc`): the encoder's NAL units with start
//! codes, written back to back. There is no header, index or timing, so these files are
//! write-only here: a plain container for an encoder's output when no MP4 or MKV is wanted.

use crate::WriteSeek;
use ironav_core::{CodecId, Error, ErrorCode, Muxer, Packet, Result, StreamInfo};

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
