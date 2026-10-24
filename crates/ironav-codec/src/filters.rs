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
        name: "format",
        media: MediaType::Video,
        description: "Convert the pixel format (gray8, yuv420p, yuv422p, yuv444p, rgb24), for example to yuv420p for the GPU encoder. RGB uses limited-range BT.601 unless matrix=709.",
        params: &[
            ParamSpec { name: "pix_fmt", ty: "string", default: None, description: "target pixel format name" },
            ParamSpec { name: "matrix", ty: "integer", default: Some(601.0), description: "colour matrix for RGB conversions: 601 or 709" },
        ],
        input_pix_fmts: &[],
        output_pix_fmt: None,
    },
    FilterSpec {
        name: "scale",
        media: MediaType::Video,
        description: "Resize video. Give width, height, or both; a missing side keeps the aspect ratio (rounded to an even number). Shrinking averages the pixels it covers.",
        params: &[
            ParamSpec { name: "width", ty: "integer", default: None, description: "output width in pixels (0 or omitted = derive from height)" },
            ParamSpec { name: "height", ty: "integer", default: None, description: "output height in pixels (0 or omitted = derive from width)" },
        ],
        input_pix_fmts: &[],
        output_pix_fmt: None,
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
        "format" => {
            let name = match args.get("pix_fmt") {
                Some(Value::String(s)) => s.as_str(),
                Some(_) => return Err(Error::invalid_arg("format.pix_fmt must be a pixel format name")),
                None => return Err(Error::invalid_arg("format needs pix_fmt").hint("for example format:pix_fmt=yuv420p").alternatives(PixelFormat::ALL.iter().map(|p| p.name()))),
            };
            let to = PixelFormat::ALL.iter().copied().find(|p| p.name() == name).ok_or_else(|| {
                Error::invalid_arg(format!("unknown pixel format '{name}'")).alternatives(PixelFormat::ALL.iter().map(|p| p.name()))
            })?;
            let matrix = match num("matrix")? {
                None => crate::pixconv::Matrix::Bt601,
                Some(m) if m == 601.0 => crate::pixconv::Matrix::Bt601,
                Some(m) if m == 709.0 => crate::pixconv::Matrix::Bt709,
                Some(m) => return Err(Error::invalid_arg(format!("format.matrix must be 601 or 709, got {m}")).alternatives(["601", "709"])),
            };
            Box::new(crate::pixconv::Format { to, matrix })
        }
        "scale" => {
            let dim = |key: &str| -> Result<Option<u32>> {
                match num(key)? {
                    None => Ok(None),
                    Some(v) if v == 0.0 => Ok(None),
                    Some(v) if v.fract() == 0.0 && v > 0.0 && v <= u32::MAX as f64 => Ok(Some(v as u32)),
                    Some(v) => Err(Error::invalid_arg(format!("scale.{key} must be a whole number of pixels, got {v}"))),
                }
            };
            let (width, height) = (dim("width")?, dim("height")?);
            crate::scale::validate(width, height)?;
            Box::new(crate::scale::Scale { width, height })
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
