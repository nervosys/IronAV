use crate::{Attachment, Chapter, ColorInfo, Error, ErrorCode, FieldOrder, Frame, HdrMetadata, Packet, Result, StreamInfo, StreamParams};

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
    /// The streams of the file that were left out because IronAV does not decode them (a codec it lacks,
    /// a kind of stream it does not play), each as a short description. Empty for most formats.
    fn unsupported_streams(&self) -> Vec<String> {
        Vec::new()
    }
    /// The file's chapters, in order, when the format has them (Matroska). Their titles come from the file.
    fn chapters(&self) -> Vec<Chapter> {
        Vec::new()
    }
    /// The files attached to the container (Matroska): names and types, not the data.
    fn attachments(&self) -> Vec<Attachment> {
        Vec::new()
    }
    /// The colour description the container records for a video stream (Matroska `Colour`, MP4
    /// `colr`), or `None`. A fallback for streams whose bitstream does not say (VP9 cannot signal PQ).
    fn color_info(&self, _stream: usize) -> Option<ColorInfo> {
        None
    }
    /// The HDR mastering metadata the container records for a video stream, or `None`.
    fn hdr_metadata(&self, _stream: usize) -> Option<HdrMetadata> {
        None
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
    /// Forget the pictures and references held so far, as before a seek, so the next packet (a
    /// keyframe) starts cleanly, while keeping what was set up at open (parameter sets, driver
    /// state). Returns false if the decoder cannot do that, and the caller must build a new one.
    fn reset(&mut self) -> bool {
        false
    }

    /// The field order of each frame the last `decode` or `flush` call returned, in the same order.
    /// A decoder that does not track interlacing returns an empty list (every frame is then `Unknown`).
    fn field_orders(&self) -> Vec<FieldOrder> {
        Vec::new()
    }

    /// The colour description the stream signals for its pictures (from the sequence header or
    /// parameter set in use), or `None` if the decoder does not read it or the stream says nothing.
    /// Valid after the first `decode` that returned a frame.
    fn color_info(&self) -> Option<ColorInfo> {
        None
    }

    /// The HDR mastering metadata the stream carries (SEI messages or metadata OBUs), or `None`.
    /// Valid after the first `decode` that returned a frame.
    fn hdr_metadata(&self) -> Option<HdrMetadata> {
        None
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
    /// [`Filter::process`] with the field order the decoder reported for `frame`. Filters that do not
    /// care about interlacing keep this default.
    fn process_interlaced(&mut self, frame: Frame, _order: FieldOrder) -> Result<Vec<Frame>> {
        self.process(frame)
    }
    /// Whether the frames this filter returns have the field order of the one it took (a filter that
    /// touches each pixel alone, or flips along the lines, does; one that resizes vertically does not).
    fn keeps_field_order(&self) -> bool {
        false
    }
    /// The factor by which this filter multiplies the timestamps of the frames it returns (a filter that
    /// doubles the frame rate stamps its frames in half units, so that the new ones fall exactly between
    /// the old). The stream's time base is divided by it.
    fn time_scale(&self) -> i64 {
        1
    }
    /// Frames still held back when the stream ends (a filter with look-ahead).
    fn flush(&mut self) -> Result<Vec<Frame>> {
        Ok(Vec::new())
    }
}
