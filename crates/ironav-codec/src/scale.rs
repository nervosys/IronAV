//! The `scale` video filter: resize with a separable triangle (bilinear) filter whose width grows
//! with the reduction ratio, so shrinking averages the source pixels it covers instead of
//! skipping them. Works on every pixel format, plane by plane, in 14-bit fixed point.

use ironav_core::{Error, Filter, Frame, Result, StreamParams, VideoFrame};

const BITS: u32 = 14;
const ONE: i32 = 1 << BITS;
const MAX_DIM: u32 = 16384;

/// Target size from the filter's parameters; a missing side keeps the aspect ratio (rounded up
/// to an even number so 4:2:0 output stays encodable).
pub(crate) fn target_size(width: Option<u32>, height: Option<u32>, src_w: u32, src_h: u32) -> (u32, u32) {
    let even = |v: u64| ((v + 1) & !1).max(2) as u32;
    match (width, height) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, even((w as u64 * src_h as u64 + src_w as u64 / 2) / src_w.max(1) as u64)),
        (None, Some(h)) => (even((h as u64 * src_w as u64 + src_h as u64 / 2) / src_h.max(1) as u64), h),
        (None, None) => (src_w, src_h),
    }
}

pub(crate) fn validate(width: Option<u32>, height: Option<u32>) -> Result<()> {
    if width.is_none() && height.is_none() {
        return Err(Error::invalid_arg("scale needs width, height, or both").hint("for example scale:width=1280 keeps the aspect ratio"));
    }
    for (name, v) in [("width", width), ("height", height)] {
        if let Some(v) = v {
            if v == 0 || v > MAX_DIM {
                return Err(Error::invalid_arg(format!("scale.{name} must be between 1 and {MAX_DIM}, got {v}")));
            }
        }
    }
    Ok(())
}

pub struct Scale {
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
}

impl Filter for Scale {
    fn output_params(&self, input: &StreamParams) -> StreamParams {
        match input {
            StreamParams::Video { width, height, pix_fmt, frame_rate } => {
                let (w, h) = target_size(self.width, self.height, *width, *height);
                StreamParams::Video { width: w, height: h, pix_fmt: *pix_fmt, frame_rate: *frame_rate }
            }
            other => other.clone(),
        }
    }

    fn process(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let Frame::Video(v) = frame else { return Ok(vec![frame]) };
        let (w, h) = target_size(self.width, self.height, v.width, v.height);
        if (w, h) == (v.width, v.height) {
            return Ok(vec![Frame::Video(v)]);
        }
        let src_layout = v.pix_fmt.planes(v.width as usize, v.height as usize);
        let dst_layout = v.pix_fmt.planes(w as usize, h as usize);
        let planes = v
            .planes
            .iter()
            .zip(src_layout.iter().zip(&dst_layout))
            .map(|(p, (&(sw, sh, bpp), &(dw, dh, _)))| resize_plane(p, sw, sh, dw, dh, bpp))
            .collect();
        Ok(vec![Frame::Video(VideoFrame { width: w, height: h, pix_fmt: v.pix_fmt, pts: v.pts, planes })])
    }
}

/// For each output coordinate: the first source index and the fixed-point weights of the taps.
struct Taps {
    start: Vec<usize>,
    weights: Vec<Vec<i32>>,
}

fn taps(src: usize, dst: usize) -> Taps {
    let ratio = src as f64 / dst as f64;
    // Enlarging uses the plain triangle; shrinking widens it to cover the source pixels.
    let support = ratio.max(1.0);
    let (mut start, mut weights) = (Vec::with_capacity(dst), Vec::with_capacity(dst));
    for i in 0..dst {
        let centre = (i as f64 + 0.5) * ratio - 0.5;
        let lo = ((centre - support).ceil().max(0.0)) as usize;
        let hi = (((centre + support).floor()) as isize).clamp(0, src as isize - 1) as usize;
        let mut w: Vec<f64> = (lo..=hi.max(lo)).map(|x| (1.0 - (x as f64 - centre).abs() / support).max(0.0)).collect();
        let sum: f64 = w.iter().sum();
        if sum <= 0.0 {
            // Cannot happen for in-range centres, but never divide by zero: take the nearest pixel.
            w = vec![1.0];
        }
        let sum: f64 = w.iter().sum();
        let mut q: Vec<i32> = w.iter().map(|x| (x / sum * ONE as f64).round() as i32).collect();
        // Make the taps add up to exactly one, so flat areas stay exactly flat.
        let diff = ONE - q.iter().sum::<i32>();
        let big = (0..q.len()).max_by_key(|&k| q[k]).unwrap_or(0);
        q[big] += diff;
        start.push(lo.min(src - 1));
        weights.push(q);
    }
    Taps { start, weights }
}

/// Resize one plane of `bpp`-byte pixels (interleaved channels are treated independently).
pub(crate) fn resize_plane(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize, bpp: usize) -> Vec<u8> {
    let (hx, vy) = (taps(sw, dw), taps(sh, dh));
    let src_stride = sw * bpp;
    // Horizontal pass into a plane that is dw wide and still sh tall.
    let mut mid = vec![0u8; dw * bpp * sh];
    for y in 0..sh {
        let row = &src[y * src_stride..(y + 1) * src_stride];
        let out = &mut mid[y * dw * bpp..(y + 1) * dw * bpp];
        for x in 0..dw {
            let (s, w) = (hx.start[x], &hx.weights[x]);
            for c in 0..bpp {
                let mut acc = 0i32;
                for (k, &wk) in w.iter().enumerate() {
                    acc += wk * row[(s + k).min(sw - 1) * bpp + c] as i32;
                }
                out[x * bpp + c] = ((acc + ONE / 2) >> BITS).clamp(0, 255) as u8;
            }
        }
    }
    // Vertical pass, accumulating whole rows so the inner loop is contiguous.
    let row_len = dw * bpp;
    let mut out = vec![0u8; row_len * dh];
    let mut acc = vec![0i32; row_len];
    for y in 0..dh {
        acc.fill(0);
        let (s, w) = (vy.start[y], &vy.weights[y]);
        for (k, &wk) in w.iter().enumerate() {
            let r = &mid[(s + k).min(sh - 1) * row_len..][..row_len];
            for (a, &p) in acc.iter_mut().zip(r) {
                *a += wk * p as i32;
            }
        }
        for (o, &a) in out[y * row_len..(y + 1) * row_len].iter_mut().zip(&acc) {
            *o = ((a + ONE / 2) >> BITS).clamp(0, 255) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::PixelFormat;

    fn frame(fmt: PixelFormat, w: u32, h: u32, f: impl Fn(usize, usize, usize) -> u8) -> VideoFrame {
        let planes = fmt
            .planes(w as usize, h as usize)
            .iter()
            .enumerate()
            .map(|(i, &(pw, ph, bpp))| (0..ph).flat_map(|y| (0..pw * bpp).map(move |x| (x, y))).map(|(x, y)| f(i, x, y)).collect())
            .collect();
        VideoFrame { width: w, height: h, pix_fmt: fmt, pts: 7, planes }
    }

    fn scaled(f: VideoFrame, w: Option<u32>, h: Option<u32>) -> VideoFrame {
        let Frame::Video(v) = Scale { width: w, height: h }.process(Frame::Video(f)).unwrap().remove(0) else { panic!() };
        v
    }

    #[test]
    fn flat_images_stay_exactly_flat_at_any_ratio() {
        for (sw, sh, dw, dh) in [(64, 48, 32, 24), (64, 48, 100, 70), (50, 30, 17, 11), (7, 5, 64, 48)] {
            let o = scaled(frame(PixelFormat::Yuv420p, sw, sh, |p, _, _| [90, 128, 200][p]), Some(dw), Some(dh));
            assert_eq!((o.width, o.height), (dw, dh));
            for (p, want) in o.planes.iter().zip([90u8, 128, 200]) {
                assert!(p.iter().all(|&x| x == want), "{sw}x{sh} -> {dw}x{dh}");
            }
        }
    }

    #[test]
    fn halving_a_checkerboard_averages_it() {
        let o = scaled(frame(PixelFormat::Gray8, 64, 64, |_, x, y| if (x + y) % 2 == 0 { 0 } else { 255 }), Some(32), Some(32));
        // Interior pixels average a 2x2 block of 0/255, which is 127.5.
        let mid = &o.planes[0][8 * 32 + 8..8 * 32 + 24];
        assert!(mid.iter().all(|&x| (126..=129).contains(&x)), "{mid:?}");
    }

    #[test]
    fn a_gradient_stays_a_gradient() {
        let o = scaled(frame(PixelFormat::Gray8, 256, 16, |_, x, _| x as u8), Some(64), Some(16));
        let row = &o.planes[0][..64];
        assert!(row.windows(2).all(|w| w[1] >= w[0]), "monotonic");
        assert!((row[32] as i32 - 130).abs() <= 4, "centre sample, got {}", row[32]);
    }

    #[test]
    fn one_side_keeps_the_aspect_ratio_with_even_sizes() {
        assert_eq!(target_size(Some(640), None, 1920, 1080), (640, 360));
        assert_eq!(target_size(None, Some(480), 1280, 720), (854, 480));
        assert_eq!(target_size(Some(100), None, 640, 480), (100, 76));
    }

    #[test]
    fn every_pixel_format_keeps_its_plane_layout() {
        for fmt in PixelFormat::ALL {
            let o = scaled(frame(fmt, 40, 30, |_, x, y| (x * 3 + y) as u8), Some(20), Some(14));
            let want: Vec<usize> = fmt.planes(20, 14).iter().map(|&(w, h, b)| w * h * b).collect();
            assert_eq!(o.planes.iter().map(Vec::len).collect::<Vec<_>>(), want, "{}", fmt.name());
            assert_eq!(o.pts, 7);
        }
    }

    #[test]
    fn bad_sizes_are_rejected_with_a_reason() {
        assert!(validate(None, None).is_err());
        assert!(validate(Some(0), None).is_err());
        assert!(validate(Some(100_000), None).is_err());
        assert!(validate(Some(640), None).is_ok());
    }
}
