use ironav_core::{AudioFrame, Decoder, Encoder, Error, Frame, Packet, Result, SampleFormat};

pub struct PcmDecoder {
    fmt: SampleFormat,
    sample_rate: u32,
    channels: u16,
}

impl PcmDecoder {
    pub fn new(fmt: SampleFormat, sample_rate: u32, channels: u16) -> Self {
        Self { fmt, sample_rate, channels }
    }
}

impl Decoder for PcmDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let d = &packet.data;
        let width = self.fmt.bytes() * self.channels as usize;
        if width == 0 || d.len() % width != 0 {
            return Err(Error::invalid_input(format!(
                "PCM packet of {} bytes is not a whole number of {}-channel {} frames",
                d.len(),
                self.channels,
                self.fmt.name()
            )));
        }
        let samples: Vec<f64> = match self.fmt {
            SampleFormat::U8 => d.iter().map(|&b| (b as f64 - 128.0) / 128.0).collect(),
            SampleFormat::S16 => d.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f64 / 32768.0).collect(),
            SampleFormat::S24 => d
                .chunks_exact(3)
                .map(|c| (((i32::from_le_bytes([0, c[0], c[1], c[2]])) >> 8) as f64) / 8388608.0)
                .collect(),
            SampleFormat::S32 => {
                d.chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64 / 2147483648.0).collect()
            }
            SampleFormat::F32 => d.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64).collect(),
            SampleFormat::F64 => d
                .chunks_exact(8)
                .map(|c| f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
                .collect(),
        };
        Ok(vec![Frame::Audio(AudioFrame { sample_rate: self.sample_rate, channels: self.channels, pts: packet.pts, samples })])
    }
}

pub struct PcmEncoder {
    fmt: SampleFormat,
}

impl PcmEncoder {
    pub fn new(fmt: SampleFormat) -> Self {
        Self { fmt }
    }
}

impl Encoder for PcmEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid_input("PCM encoder received a video frame"));
        };
        let mut out = Vec::with_capacity(a.samples.len() * self.fmt.bytes());
        match self.fmt {
            SampleFormat::U8 => out.extend(a.samples.iter().map(|&s| (s * 128.0 + 128.0).round().clamp(0.0, 255.0) as u8)),
            SampleFormat::S16 => {
                for &s in &a.samples {
                    out.extend_from_slice(&((s * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes());
                }
            }
            SampleFormat::S24 => {
                for &s in &a.samples {
                    let v = (s * 8388608.0).round().clamp(-8388608.0, 8388607.0) as i32;
                    out.extend_from_slice(&v.to_le_bytes()[..3]);
                }
            }
            SampleFormat::S32 => {
                for &s in &a.samples {
                    out.extend_from_slice(&((s * 2147483648.0).round().clamp(-2147483648.0, 2147483647.0) as i32).to_le_bytes());
                }
            }
            SampleFormat::F32 => {
                for &s in &a.samples {
                    out.extend_from_slice(&(s as f32).to_le_bytes());
                }
            }
            SampleFormat::F64 => {
                for &s in &a.samples {
                    out.extend_from_slice(&s.to_le_bytes());
                }
            }
        }
        Ok(vec![Packet { stream: 0, pts: a.pts, duration: a.frames() as i64, data: out }])
    }
}
