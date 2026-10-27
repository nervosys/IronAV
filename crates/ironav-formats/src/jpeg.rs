//! A `.jpg` file: one image, read as a single Motion-JPEG packet and written from one.

use crate::WriteSeek;
use ironav_core::{CodecId, Demuxer, Error, Muxer, Packet, Rational, Result, StreamInfo, StreamParams};
use std::io::Read;

pub fn looks_like_jpeg(magic: &[u8]) -> bool {
    magic.starts_with(&[0xFF, 0xD8, 0xFF])
}

pub struct JpegDemuxer {
    streams: Vec<StreamInfo>,
    data: Option<Vec<u8>>,
}

impl JpegDemuxer {
    pub fn open<R: Read>(mut r: R) -> Result<Self> {
        let mut data = Vec::new();
        r.read_to_end(&mut data)?;
        let (width, height, pix_fmt) = ironav_codec::jpeg::probe(&data)?;
        let frame_rate = Rational::new(25, 1);
        let stream = StreamInfo {
            index: 0,
            codec: CodecId::Mjpeg,
            time_base: frame_rate.invert(),
            duration: Some(1),
            bits: None,
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Video { width, height, pix_fmt, frame_rate },
        };
        Ok(Self { streams: vec![stream], data: Some(data) })
    }
}

impl Demuxer for JpegDemuxer {
    fn format_name(&self) -> &'static str {
        "jpeg"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn read_packet(&mut self) -> Result<Option<Packet>> {
        Ok(self.data.take().map(|data| Packet { stream: 0, pts: 0, duration: 1, data }))
    }
    fn seek(&mut self, _seconds: f64) -> Result<f64> {
        Ok(0.0)
    }
}

pub struct JpegMuxer {
    out: Box<dyn WriteSeek>,
    written: bool,
}

impl JpegMuxer {
    pub fn new(out: Box<dyn WriteSeek>) -> Self {
        Self { out, written: false }
    }
}

impl Muxer for JpegMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        match streams {
            [s] if s.codec == CodecId::Mjpeg => Ok(()),
            _ => Err(Error::invalid_arg("a .jpg file holds exactly one mjpeg video stream")),
        }
    }
    fn write_packet(&mut self, p: &Packet) -> Result<()> {
        if self.written {
            return Err(Error::invalid_arg("a .jpg file holds one image, but the input has more frames").hint("keep a single frame, or write several files"));
        }
        self.written = true;
        self.out.write_all(&p.data)?;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}
