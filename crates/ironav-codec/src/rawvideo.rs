use ironav_core::{Decoder, Encoder, Error, Frame, Packet, PixelFormat, Result, VideoFrame};

pub struct RawVideoDecoder {
    width: u32,
    height: u32,
    pix_fmt: PixelFormat,
}

impl RawVideoDecoder {
    pub fn new(width: u32, height: u32, pix_fmt: PixelFormat) -> Self {
        Self { width, height, pix_fmt }
    }
}

impl Decoder for RawVideoDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let layout = self.pix_fmt.planes(self.width as usize, self.height as usize);
        let expect: usize = layout.iter().map(|&(w, h, b)| w * h * b).sum();
        if packet.data.len() != expect {
            return Err(Error::invalid_input(format!(
                "rawvideo packet is {} bytes, expected {expect} for {}x{} {}",
                packet.data.len(),
                self.width,
                self.height,
                self.pix_fmt.name()
            )));
        }
        let mut off = 0;
        let planes = layout
            .iter()
            .map(|&(w, h, b)| {
                let n = w * h * b;
                let p = packet.data[off..off + n].to_vec();
                off += n;
                p
            })
            .collect();
        Ok(vec![Frame::Video(VideoFrame { width: self.width, height: self.height, pix_fmt: self.pix_fmt, pts: packet.pts, planes })])
    }
}

pub struct RawVideoEncoder;

impl Encoder for RawVideoEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(v) = frame else {
            return Err(Error::invalid_input("rawvideo encoder received an audio frame"));
        };
        let mut data = Vec::with_capacity(v.planes.iter().map(Vec::len).sum());
        for p in &v.planes {
            data.extend_from_slice(p);
        }
        Ok(vec![Packet { stream: 0, pts: v.pts, duration: 1, data }])
    }
}
