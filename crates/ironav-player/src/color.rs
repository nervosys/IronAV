//! Colour conversion constants for displaying decoded YUV pictures.
//!
//! One place decides the matrix (BT.601, 709 or 2020), the range (limited or full) and the bit
//! depth normalisation. The CPU converter in [`crate::snapshot`] and the GPU shader in
//! `ironav-gui` both take their numbers from [`YuvToRgb`], so they cannot disagree; the shader
//! computes `rgb = m * (yuv - offset)` exactly like [`YuvToRgb::apply`].

use ironav_core::VideoFrame;

/// The YUV to RGB matrix a picture is coded with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

impl Matrix {
    /// The luma weights `(kr, kb)`.
    pub fn kr_kb(self) -> (f64, f64) {
        match self {
            Self::Bt601 => (0.299, 0.114),
            Self::Bt709 => (0.2126, 0.0722),
            Self::Bt2020 => (0.2627, 0.0593),
        }
    }
}

/// How a picture's samples map to colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colorimetry {
    pub matrix: Matrix,
    /// Full range (0..=max) instead of limited ("studio") range.
    pub full_range: bool,
    /// Bits per sample (8, 10 or 12).
    pub bits: u8,
}

impl Colorimetry {
    /// What to assume for `v`. A frame carries no colour tags, so the usual rule applies:
    /// BT.709 from 720 lines up, BT.601 below, limited range.
    pub fn of(v: &VideoFrame) -> Self {
        Self { matrix: if v.height >= 720 { Matrix::Bt709 } else { Matrix::Bt601 }, full_range: false, bits: v.pix_fmt.bit_depth() }
    }
}

/// `rgb = m * (yuv - offset)`, with `yuv` as each sample divided by `2^bits - 1`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct YuvToRgb {
    /// Rows are R, G, B; columns are Y, U (Cb), V (Cr).
    pub m: [[f32; 3]; 3],
    pub offset: [f32; 3],
}

impl YuvToRgb {
    pub fn new(c: Colorimetry) -> Self {
        let (kr, kb) = c.matrix.kr_kb();
        let kg = 1.0 - kr - kb;
        let shift = f64::from(c.bits.clamp(8, 16) - 8);
        let top = ((1u64 << c.bits.clamp(8, 16)) - 1) as f64;
        let lvl = |v: f64| v * (shift.exp2()) / top;
        // Black and the size of the luma and chroma ranges, as fractions of full scale.
        let (y_off, y_rng, c_rng) = if c.full_range { (0.0, 1.0, 1.0) } else { (lvl(16.0), lvl(219.0), lvl(224.0)) };
        let c_mid = lvl(128.0);
        let (ys, cs) = (1.0 / y_rng, 1.0 / c_rng);
        let rv = 2.0 * (1.0 - kr) * cs;
        let bu = 2.0 * (1.0 - kb) * cs;
        let gu = -(kb * 2.0 * (1.0 - kb) / kg) * cs;
        let gv = -(kr * 2.0 * (1.0 - kr) / kg) * cs;
        Self {
            m: [[ys as f32, 0.0, rv as f32], [ys as f32, gu as f32, gv as f32], [ys as f32, bu as f32, 0.0]],
            offset: [y_off as f32, c_mid as f32, c_mid as f32],
        }
    }

    /// Convert normalised samples to RGB in 0..=1 (not clamped). This is the shader's formula.
    pub fn apply(&self, y: f32, u: f32, v: f32) -> [f32; 3] {
        let d = [y - self.offset[0], u - self.offset[1], v - self.offset[2]];
        let row = |r: &[f32; 3]| r[0] * d[0] + r[1] * d[1] + r[2] * d[2];
        [row(&self.m[0]), row(&self.m[1]), row(&self.m[2])]
    }

    /// The same, from raw sample values of the given depth, giving 8-bit RGB.
    pub fn apply_raw(&self, bits: u8, y: u32, u: u32, v: u32) -> [u8; 3] {
        let top = ((1u64 << bits) - 1) as f32;
        let rgb = self.apply(y as f32 / top, u as f32 / top, v as f32 / top);
        rgb.map(|c| (c * 255.0).round().clamp(0.0, 255.0) as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rgb_frame_reference;
    use ironav_core::PixelFormat;

    fn yuv420(w: u32, h: u32, y: u8, u: u8, v: u8) -> VideoFrame {
        let (cw, ch) = (w.div_ceil(2) as usize, h.div_ceil(2) as usize);
        VideoFrame { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes: vec![vec![y; (w * h) as usize], vec![u; cw * ch], vec![v; cw * ch]] }
    }

    #[test]
    fn the_shader_formula_matches_the_cpu_converter_on_sample_pixels() {
        // SD (BT.601) and HD (BT.709), black, white, greys and saturated colours.
        let samples: [(u8, u8, u8); 10] = [(16, 128, 128), (235, 128, 128), (126, 128, 128), (81, 90, 240), (145, 54, 34), (41, 240, 110), (170, 166, 16), (106, 202, 222), (60, 100, 150), (200, 30, 90)];
        for (w, h) in [(64u32, 48u32), (64, 720)] {
            for &(y, u, v) in &samples {
                let f = yuv420(w, h, y, u, v);
                let want = rgb_frame_reference(&f, None, None).2;
                let got = YuvToRgb::new(Colorimetry::of(&f)).apply_raw(8, y.into(), u.into(), v.into());
                for c in 0..3 {
                    assert!((i32::from(want[c]) - i32::from(got[c])).abs() <= 1, "{w}x{h} yuv({y},{u},{v}) channel {c}: cpu {} gpu {}", want[c], got[c]);
                }
            }
        }
    }

    #[test]
    fn ten_bit_samples_give_the_eight_bit_colour() {
        let m = YuvToRgb::new(Colorimetry { matrix: Matrix::Bt709, full_range: false, bits: 10 });
        let m8 = YuvToRgb::new(Colorimetry { matrix: Matrix::Bt709, full_range: false, bits: 8 });
        for (y, u, v) in [(16u32, 128u32, 128u32), (235, 128, 128), (81, 90, 240), (145, 54, 34)] {
            let a = m.apply_raw(10, y << 2, u << 2, v << 2);
            let b = m8.apply_raw(8, y, u, v);
            for c in 0..3 {
                assert!((i32::from(a[c]) - i32::from(b[c])).abs() <= 1, "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn full_range_and_the_three_matrices_hit_the_known_corners() {
        for matrix in [Matrix::Bt601, Matrix::Bt709, Matrix::Bt2020] {
            let full = YuvToRgb::new(Colorimetry { matrix, full_range: true, bits: 8 });
            assert_eq!(full.apply_raw(8, 0, 128, 128), [0, 0, 0]);
            assert_eq!(full.apply_raw(8, 255, 128, 128), [255, 255, 255]);
            let lim = YuvToRgb::new(Colorimetry { matrix, full_range: false, bits: 8 });
            assert_eq!(lim.apply_raw(8, 16, 128, 128), [0, 0, 0]);
            assert_eq!(lim.apply_raw(8, 235, 128, 128), [255, 255, 255]);
            // Pure red in full range: Y = kr * 255, Cr at its top.
            let (kr, kb) = matrix.kr_kb();
            let (y, cb, cr) = ((kr * 255.0).round() as u32, (128.0 - 127.5 * kr / (1.0 - kb)).round() as u32, 255);
            let red = full.apply_raw(8, y, cb, cr);
            assert!(red[0] >= 253 && red[1] <= 3 && red[2] <= 3, "{matrix:?} {red:?}");
        }
    }
}
