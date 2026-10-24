use crate::WriteSeek;
use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Muxer, Packet, PixelFormat, Rational, Result, StreamInfo, StreamParams};
use std::io::BufRead;

pub struct Y4mDemuxer<R: BufRead + Send> {
    r: R,
    streams: Vec<StreamInfo>,
    frame_bytes: usize,
    index: i64,
}

impl<R: BufRead + Send> Y4mDemuxer<R> {
    pub fn open(mut r: R) -> Result<Self> {
        let mut line = Vec::new();
        r.read_until(b'\n', &mut line)?;
        let header = String::from_utf8_lossy(&line);
        let (mut w, mut h, mut fps, mut pix) = (0u32, 0u32, Rational::new(25, 1), PixelFormat::Yuv420p);
        for tok in header.trim_end().split(' ').skip(1) {
            let (k, v) = tok.split_at(tok.len().min(1));
            match k {
                "W" => w = v.parse().map_err(|_| Error::invalid_input("bad Y4M width"))?,
                "H" => h = v.parse().map_err(|_| Error::invalid_input("bad Y4M height"))?,
                "F" => {
                    if let Some((n, d)) = v.split_once(':') {
                        fps = Rational::new(n.parse().unwrap_or(25), d.parse().unwrap_or(1));
                    }
                }
                "C" => {
                    pix = match v {
                        v if v.starts_with("420") => PixelFormat::Yuv420p,
                        "422" => PixelFormat::Yuv422p,
                        "444" => PixelFormat::Yuv444p,
                        "mono" => PixelFormat::Gray8,
                        other => {
                            return Err(Error::new(ErrorCode::UnsupportedFormat, format!("Y4M colour space '{other}' is not supported"))
                                .alternatives(["420jpeg", "422", "444", "mono"]))
                        }
                    }
                }
                _ => {}
            }
        }
        if w == 0 || h == 0 || fps.num <= 0 || fps.den <= 0 {
            return Err(Error::invalid_input("Y4M header must give positive W, H and frame rate"));
        }
        let stream = StreamInfo {
            index: 0,
            codec: CodecId::RawVideo,
            time_base: fps.invert(),
            duration: None,
            bits: None,
            params: StreamParams::Video { width: w, height: h, pix_fmt: pix, frame_rate: fps },
        };
        Ok(Self { r, streams: vec![stream], frame_bytes: pix.frame_bytes(w as usize, h as usize), index: 0 })
    }
}

impl<R: BufRead + Send> Demuxer for Y4mDemuxer<R> {
    fn format_name(&self) -> &'static str {
        "y4m"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn read_packet(&mut self) -> Result<Option<Packet>> {
        let mut line = Vec::new();
        if self.r.read_until(b'\n', &mut line)? == 0 {
            return Ok(None);
        }
        if !line.starts_with(b"FRAME") {
            return Err(Error::invalid_input("expected 'FRAME' marker in Y4M stream"));
        }
        let mut data = vec![0u8; self.frame_bytes];
        self.r.read_exact(&mut data).map_err(|_| Error::invalid_input("truncated Y4M frame"))?;
        let pts = self.index;
        self.index += 1;
        Ok(Some(Packet { stream: 0, pts, duration: 1, data }))
    }
}

pub struct Y4mMuxer {
    out: Box<dyn WriteSeek>,
}

impl Y4mMuxer {
    pub fn new(out: Box<dyn WriteSeek>) -> Self {
        Self { out }
    }
}

impl Muxer for Y4mMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [s] = streams else {
            return Err(Error::invalid_arg("y4m holds exactly one video stream"));
        };
        let StreamParams::Video { width, height, pix_fmt, frame_rate } = s.params else {
            return Err(Error::invalid_arg("y4m can only hold video"));
        };
        let c = match pix_fmt {
            PixelFormat::Yuv420p => "420jpeg",
            PixelFormat::Yuv422p => "422",
            PixelFormat::Yuv444p => "444",
            PixelFormat::Gray8 => "mono",
            PixelFormat::Rgb24 => {
                return Err(Error::new(ErrorCode::UnsupportedFormat, "y4m cannot hold rgb24")
                    .hint("convert to a YUV or gray format first")
                    .alternatives(["yuv420p", "yuv422p", "yuv444p", "gray8"]))
            }
        };
        let line = format!("YUV4MPEG2 W{width} H{height} F{}:{} Ip A1:1 C{c}\n", frame_rate.num, frame_rate.den);
        self.out.write_all(line.as_bytes())?;
        Ok(())
    }
    fn write_packet(&mut self, p: &Packet) -> Result<()> {
        self.out.write_all(b"FRAME\n")?;
        self.out.write_all(&p.data)?;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}
