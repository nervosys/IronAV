use crate::{Error, ErrorCode, Frame, Packet, Result, StreamInfo, StreamParams};

// All stage traits are `Send` so the engine can run each stage on its own thread and
// move packets/frames between stages without copying.

pub trait Demuxer: Send {
    fn format_name(&self) -> &'static str;
    fn streams(&self) -> &[StreamInfo];
    /// `Ok(None)` signals end of input.
    fn read_packet(&mut self) -> Result<Option<Packet>>;
    /// Tell the demuxer this stream's packets will not be used, so it need not read them.
    /// Optional: a demuxer that ignores this still works, the engine drops the packets itself.
    fn discard(&mut self, _stream: usize) {}
    /// Reposition so the next packets start at `seconds`. Returns the time actually landed on:
    /// formats with keyframes land on the last keyframe at or before the request, so a decoder
    /// can start cleanly and the caller drops frames until the wanted time.
    fn seek(&mut self, _seconds: f64) -> Result<f64> {
        Err(Error::new(ErrorCode::UnsupportedFormat, format!("{} files cannot seek", self.format_name())))
    }
}

pub trait Muxer: Send {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()>;
    fn write_packet(&mut self, packet: &Packet) -> Result<()>;
    fn finish(&mut self) -> Result<()>;
}

pub trait Decoder: Send {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>>;
    fn flush(&mut self) -> Result<Vec<Frame>> {
        Ok(Vec::new())
    }
}

pub trait Encoder: Send {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>>;
    fn flush(&mut self) -> Result<Vec<Packet>> {
        Ok(Vec::new())
    }
}

pub trait Filter: Send {
    /// Stream parameters produced when fed a stream with `input` parameters.
    fn output_params(&self, input: &StreamParams) -> StreamParams {
        input.clone()
    }
    fn process(&mut self, frame: Frame) -> Result<Vec<Frame>>;
    /// Frames still held back when the stream ends (a filter with look-ahead).
    fn flush(&mut self) -> Result<Vec<Frame>> {
        Ok(Vec::new())
    }
}
