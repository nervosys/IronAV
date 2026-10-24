use ironav_core::{Error, ErrorCode, Filter, Frame, MediaType, PixelFormat, Result, StreamParams, VideoFrame};
use serde::Serialize;
use serde_json::{Map, Value};

#[derive(Debug, Serialize)]
pub struct ParamSpec {
    pub name: &'static str,
    #[serde(rename = "type")]
    pub ty: &'static str,
    pub default: Option<f64>,
    pub description: &'static str,
}

#[derive(Debug, Serialize)]
pub struct FilterSpec {
    pub name: &'static str,
    pub media: MediaType,
    pub description: &'static str,
    pub params: &'static [ParamSpec],
    /// Pixel formats the filter accepts (video filters only; empty = any).
    pub input_pix_fmts: &'static [PixelFormat],
    /// Pixel format produced, when it differs from the input.
    pub output_pix_fmt: Option<PixelFormat>,
}

pub static FILTERS: &[FilterSpec] = &[
    FilterSpec {
        name: "volume",
        media: MediaType::Audio,
        description: "Scale audio amplitude by a linear gain factor (1.0 = unchanged, 0.5 = -6 dB). Output is clipped to full scale.",
        params: &[ParamSpec { name: "gain", ty: "number", default: Some(1.0), description: "linear gain, must be >= 0" }],
        input_pix_fmts: &[],
        output_pix_fmt: None,
    },
    FilterSpec {
        name: "gray",
        media: MediaType::Video,
        description: "Convert video to single-plane 8-bit grayscale (luma only).",
        params: &[],
        input_pix_fmts: &[PixelFormat::Gray8, PixelFormat::Yuv420p, PixelFormat::Yuv422p, PixelFormat::Yuv444p, PixelFormat::Rgb24],
        output_pix_fmt: Some(PixelFormat::Gray8),
    },
    FilterSpec {
        name: "hflip",
        media: MediaType::Video,
        description: "Mirror each video frame horizontally.",
        params: &[],
        input_pix_fmts: &[],
        output_pix_fmt: None,
    },
];

pub fn build_filter(name: &str, args: &Map<String, Value>) -> Result<Box<dyn Filter>> {
    let spec = FILTERS.iter().find(|f| f.name == name).ok_or_else(|| {
        Error::new(ErrorCode::UnsupportedFilter, format!("unknown filter '{name}'"))
            .hint("see `ironav ontology search --kind filter`")
            .alternatives(FILTERS.iter().map(|f| f.name))
    })?;
    for k in args.keys() {
        if !spec.params.iter().any(|p| p.name == k) {
            return Err(Error::invalid_arg(format!("filter '{name}' has no parameter '{k}'"))
                .alternatives(spec.params.iter().map(|p| p.name)));
        }
    }
    let num = |key: &str| -> Result<Option<f64>> {
        match args.get(key) {
            None => Ok(None),
            Some(v) => v.as_f64().map(Some).ok_or_else(|| Error::invalid_arg(format!("parameter '{key}' of '{name}' must be a number"))),
        }
    };
    Ok(match name {
        "volume" => {
            let gain = num("gain")?.unwrap_or(1.0);
            if !gain.is_finite() || gain < 0.0 {
                return Err(Error::invalid_arg("volume.gain must be a finite number >= 0"));
            }
            Box::new(Volume { gain })
        }
        "gray" => Box::new(Gray),
        "hflip" => Box::new(HFlip),
        _ => unreachable!("spec table and match are kept in sync"),
    })
}

struct Volume {
    gain: f64,
}

impl Filter for Volume {
    fn process(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        match frame {
            Frame::Audio(mut a) => {
                let g = self.gain;
                for s in &mut a.samples {
                    *s = (*s * g).clamp(-1.0, 1.0);
                }
                Ok(vec![Frame::Audio(a)])
            }
            v => Ok(vec![v]),
        }
    }
}

struct Gray;

impl Filter for Gray {
    fn output_params(&self, input: &StreamParams) -> StreamParams {
        match input {
            StreamParams::Video { width, height, frame_rate, .. } => {
                StreamParams::Video { width: *width, height: *height, pix_fmt: PixelFormat::Gray8, frame_rate: *frame_rate }
            }
            other => other.clone(),
        }
    }
    fn process(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let Frame::Video(v) = frame else { return Ok(vec![frame]) };
        let plane = match v.pix_fmt {
            PixelFormat::Rgb24 => v
                .planes[0]
                .chunks_exact(3)
                .map(|p| ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114 + 500) / 1000) as u8)
                .collect(),
            _ => v.planes.into_iter().next().unwrap_or_default(),
        };
        Ok(vec![Frame::Video(VideoFrame { pix_fmt: PixelFormat::Gray8, planes: vec![plane], ..v })])
    }
}

struct HFlip;

impl Filter for HFlip {
    fn process(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let Frame::Video(mut v) = frame else { return Ok(vec![frame]) };
        let layout = v.pix_fmt.planes(v.width as usize, v.height as usize);
        for (plane, &(w, _h, bpp)) in v.planes.iter_mut().zip(&layout) {
            let stride = w * bpp;
            for row in plane.chunks_exact_mut(stride) {
                if bpp == 1 {
                    row.reverse();
                } else {
                    for x in 0..w / 2 {
                        for k in 0..bpp {
                            row.swap(x * bpp + k, (w - 1 - x) * bpp + k);
                        }
                    }
                }
            }
        }
        Ok(vec![Frame::Video(v)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hflip_reverses_rows() {
        let f = Frame::Video(VideoFrame { width: 3, height: 2, pix_fmt: PixelFormat::Gray8, pts: 0, planes: vec![vec![1, 2, 3, 4, 5, 6]] });
        let Frame::Video(out) = HFlip.process(f).unwrap().remove(0) else { panic!() };
        assert_eq!(out.planes[0], vec![3, 2, 1, 6, 5, 4]);
    }

    #[test]
    fn unknown_param_lists_valid_ones() {
        let mut m = Map::new();
        m.insert("db".into(), Value::from(3));
        let e = build_filter("volume", &m).err().unwrap();
        assert_eq!(e.alternatives, vec!["gain"]);
    }
}
