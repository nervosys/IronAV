//! Turning a decoded picture into something a screen, a terminal or an agent can use.

use ironav_codec::resize_frame;
use ironav_core::{Error, ErrorCode, PixelFormat, Result, VideoFrame};

/// RGB24 pixels of `v`, resized to fit inside `max_w` x `max_h` (aspect kept) when given.
/// Returns `(width, height, rgb)`. Uses BT.709 for HD pictures and BT.601 below, limited range.
pub fn rgb_frame(v: &VideoFrame, max_w: Option<u32>, max_h: Option<u32>) -> (u32, u32, Vec<u8>) {
    let mut scaled;
    let mut src = v;
    if let Some((w, h)) = fit(v.width, v.height, max_w, max_h) {
        scaled = resize_frame(v, w, h);
        scaled.pts = v.pts;
        src = &scaled;
    }
    // High-bit-depth pictures are narrowed to 8 bits first; a screen shows 8 bits anyway.
    let narrowed;
    if src.pix_fmt.bit_depth() > 8 {
        narrowed = ironav_codec::convert_pixel_format(src.clone(), PixelFormat::Yuv420p, ironav_codec::Matrix::Bt709);
        src = &narrowed;
    }
    let (w, h) = (src.width as usize, src.height as usize);
    let mut out = vec![0u8; w * h * 3];
    let hd = v.height >= 720;
    let (kr, kb) = if hd { (0.2126, 0.0722) } else { (0.299, 0.114) };
    let kg = 1.0 - kr - kb;
    let layout = src.pix_fmt.planes(w, h);
    let at = |plane: usize, x: usize, y: usize, sub_x: usize, sub_y: usize| -> u8 {
        let pw = layout[plane].0;
        src.planes[plane][(y / sub_y) * pw + x / sub_x]
    };
    let (sx, sy) = match src.pix_fmt {
        PixelFormat::Yuv420p => (2, 2),
        PixelFormat::Yuv422p => (2, 1),
        PixelFormat::Yuv440p => (1, 2),
        _ => (1, 1),
    };
    for y in 0..h {
        for x in 0..w {
            let o = (y * w + x) * 3;
            match src.pix_fmt {
                PixelFormat::Rgb24 => out[o..o + 3].copy_from_slice(&src.planes[0][o..o + 3]),
                PixelFormat::Gray8 => {
                    let l = (at(0, x, y, 1, 1) as f32 - 16.0) * 255.0 / 219.0;
                    out[o..o + 3].fill(l.clamp(0.0, 255.0) as u8);
                }
                _ => {
                    let yy = (at(0, x, y, 1, 1) as f32 - 16.0) * (255.0 / 219.0);
                    let cb = (at(1, x, y, sx, sy) as f32 - 128.0) * (255.0 / 224.0);
                    let cr = (at(2, x, y, sx, sy) as f32 - 128.0) * (255.0 / 224.0);
                    let r = yy + 2.0 * (1.0 - kr) * cr;
                    let b = yy + 2.0 * (1.0 - kb) * cb;
                    let g = (yy - kr * r - kb * b) / kg;
                    out[o] = r.clamp(0.0, 255.0).round() as u8;
                    out[o + 1] = g.clamp(0.0, 255.0).round() as u8;
                    out[o + 2] = b.clamp(0.0, 255.0).round() as u8;
                }
            }
        }
    }
    (w as u32, h as u32, out)
}

/// The size that fits `w` x `h` in the given limits, or None if it already fits.
fn fit(w: u32, h: u32, max_w: Option<u32>, max_h: Option<u32>) -> Option<(u32, u32)> {
    let sw = max_w.map_or(1.0, |m| m as f64 / w as f64);
    let sh = max_h.map_or(1.0, |m| m as f64 / h as f64);
    let s = sw.min(sh);
    if s >= 1.0 {
        return None;
    }
    Some((((w as f64 * s).round() as u32).max(2) & !1, ((h as f64 * s).round() as u32).max(2) & !1))
}

pub fn to_png(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let fail = |e: png::EncodingError| Error::new(ErrorCode::Internal, format!("png encoding failed: {e}"));
    let mut enc = png::Encoder::new(&mut buf, width, height);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut w = enc.write_header().map_err(fail)?;
    w.write_image_data(rgb).map_err(fail)?;
    w.finish().map_err(fail)?;
    Ok(buf)
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn limited_range_black_white_and_grey_convert_correctly() {
        let mk = |y: u8| VideoFrame { width: 2, height: 2, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes: vec![vec![y; 4], vec![128], vec![128]] };
        assert_eq!(rgb_frame(&mk(16), None, None).2, vec![0; 12]);
        assert_eq!(rgb_frame(&mk(235), None, None).2, vec![255; 12]);
        let g = rgb_frame(&mk(126), None, None).2;
        assert!(g.iter().all(|&v| (v as i32 - 128).abs() <= 1), "{g:?}");
    }

    #[test]
    fn red_in_yuv_comes_out_red() {
        // BT.601 limited-range red is about Y=81 Cb=90 Cr=240.
        let f = VideoFrame { width: 2, height: 2, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes: vec![vec![81; 4], vec![90], vec![240]] };
        let (_, _, p) = rgb_frame(&f, None, None);
        assert!(p[0] > 250 && p[1] < 6 && p[2] < 6, "{:?}", &p[..3]);
    }

    #[test]
    fn large_pictures_are_fitted_and_png_has_a_signature() {
        let f = VideoFrame { width: 64, height: 32, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes: vec![vec![100; 2048], vec![128; 512], vec![128; 512]] };
        let (w, h, rgb) = rgb_frame(&f, Some(16), Some(16));
        assert_eq!((w, h), (16, 8));
        let png = to_png(w, h, &rgb).unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }

    #[test]
    fn ten_bit_pictures_become_the_same_rgb_as_their_eight_bit_version() {
        let words = |v: u16, n: usize| (0..n).flat_map(|_| v.to_le_bytes()).collect::<Vec<u8>>();
        // Studio-range red-ish: Y=81<<2, Cb=90<<2, Cr=240<<2 in 10-bit.
        let ten = VideoFrame { width: 2, height: 2, pix_fmt: PixelFormat::Yuv420p10, pts: 0, planes: vec![words(81 << 2, 4), words(90 << 2, 1), words(240 << 2, 1)] };
        let eight = VideoFrame { width: 2, height: 2, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes: vec![vec![81; 4], vec![90], vec![240]] };
        assert_eq!(rgb_frame(&ten, None, None), rgb_frame(&eight, None, None));
    }
}
