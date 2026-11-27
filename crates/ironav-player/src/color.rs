//! Colour conversion constants for displaying decoded YUV pictures.
//!
//! One place decides the matrix (BT.601, 709 or 2020), the range (limited or full) and the bit
//! depth normalisation. The CPU converter in [`crate::snapshot`] and the GPU shader in
//! `ironav-gui` both take their numbers from [`YuvToRgb`], so they cannot disagree; the shader
//! computes `rgb = m * (yuv - offset)` exactly like [`YuvToRgb::apply`].
//!
//! HDR pictures (PQ or HLG) go one step further: [`ToneMap`] turns the R'G'B' that comes out of the
//! matrix into SDR BT.709 display code values. The shader runs the same maths, with its numbers
//! taken from [`ToneMap::new`].

use ironav_core::{ColorInfo, VideoFrame};

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

/// The transfer function of a picture: how code values relate to light.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transfer {
    /// Gamma-coded SDR (BT.709, BT.601, sRGB, or unknown). Shown as it is.
    Sdr,
    /// SMPTE ST 2084 perceptual quantizer.
    Pq,
    /// ARIB STD-B67 / BT.2100 hybrid log-gamma.
    Hlg,
}

/// The colour primaries of a picture. Only the two that need different treatment are told apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primaries {
    Bt709,
    Bt2020,
}

/// How a picture's samples map to colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colorimetry {
    pub matrix: Matrix,
    /// Full range (0..=max) instead of limited ("studio") range.
    pub full_range: bool,
    /// Bits per sample (8, 10 or 12).
    pub bits: u8,
    pub transfer: Transfer,
    pub primaries: Primaries,
}

impl Colorimetry {
    /// What to assume for `v`. A frame carries no colour tags, so the usual rule applies:
    /// BT.709 from 720 lines up, BT.601 below, limited range, SDR.
    pub fn of(v: &VideoFrame) -> Self {
        Self::with_info(v, None)
    }

    /// The decoder's colour description when it gave one, with [`Colorimetry::of`]'s guesses for
    /// whatever it leaves unspecified (the matrix, when it is not one of BT.601, 709 or 2020).
    pub fn with_info(v: &VideoFrame, info: Option<ColorInfo>) -> Self {
        let guess = if v.height >= 720 { Matrix::Bt709 } else { Matrix::Bt601 };
        let bits = v.pix_fmt.bit_depth();
        let Some(i) = info else {
            return Self { matrix: guess, full_range: false, bits, transfer: Transfer::Sdr, primaries: Primaries::Bt709 };
        };
        Self {
            matrix: match i.matrix {
                1 => Matrix::Bt709,
                5 | 6 => Matrix::Bt601,
                // 10 is constant-luminance BT.2020, which is treated like 9 (a close approximation).
                9 | 10 => Matrix::Bt2020,
                _ => guess,
            },
            full_range: i.full_range,
            bits,
            transfer: match i.transfer {
                ColorInfo::TRANSFER_PQ => Transfer::Pq,
                ColorInfo::TRANSFER_HLG => Transfer::Hlg,
                _ => Transfer::Sdr,
            },
            primaries: if i.primaries == 9 { Primaries::Bt2020 } else { Primaries::Bt709 },
        }
    }

    /// Does the picture need tone mapping to show on an SDR display?
    pub fn is_hdr(&self) -> bool {
        self.transfer != Transfer::Sdr
    }
}

/// SMPTE ST 2084 constants.
const PQ_M1: f32 = 0.159_301_76;
const PQ_M2: f32 = 78.84375;
const PQ_C1: f32 = 0.8359375;
const PQ_C2: f32 = 18.851_563;
const PQ_C3: f32 = 18.6875;
/// The light level PQ code value 1.0 stands for, in nits.
pub const PQ_PEAK: f32 = 10_000.0;
/// What the source is assumed to reach, in nits: the usual mastering level when nothing says (PQ),
/// and the nominal display peak (HLG).
pub const SOURCE_PEAK: f32 = 1000.0;
/// The SDR reference white (ITU-R BT.2408), in nits. After tone mapping it is the brightest
/// output, 1.0.
pub const TARGET_WHITE: f32 = 203.0;
/// BT.2100 HLG system gamma at a 1000 nit display.
const HLG_GAMMA: f32 = 1.2;

/// The PQ EOTF: a code value in 0..=1 to light in 0..=1 (1 is 10 000 nits).
pub fn pq_eotf(e: f32) -> f32 {
    let p = e.max(1e-7).powf(1.0 / PQ_M2);
    ((p - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * p)).powf(1.0 / PQ_M1)
}

/// The PQ inverse EOTF: light in 0..=1 (1 is 10 000 nits) to a code value.
pub fn pq_inverse(y: f32) -> f32 {
    let p = y.max(0.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * p) / (1.0 + PQ_C3 * p)).powf(PQ_M2)
}

/// The HLG inverse OETF: a code value to relative scene light, both in 0..=1.
pub fn hlg_inverse_oetf(e: f32) -> f32 {
    const A: f32 = 0.178_832_77;
    const B: f32 = 0.284_668_92;
    const C: f32 = 0.559_910_7;
    if e <= 0.5 {
        e * e / 3.0
    } else {
        (((e - C) / A).exp() + B) / 12.0
    }
}

/// BT.2020 to BT.709 linear-light primaries conversion (ITU-R BT.2087, as in FFmpeg's `csp.c`).
pub const BT2020_TO_BT709: [[f32; 3]; 3] =
    [[1.660_491, -0.587_641, -0.072_850], [-0.124_550, 1.132_900, -0.008_349], [-0.018_151, -0.100_579, 1.118_730]];
const IDENTITY: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// The sRGB / BT.709-display encoding of linear light in 0..=1.
pub fn display_encode(l: f32) -> f32 {
    if l <= 0.003_130_8 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// Tone mapping of PQ and HLG pictures to an SDR BT.709 display. Everything the shader needs is a
/// field here, and [`ToneMap::apply`] is the formula it evaluates.
///
/// Per pixel, from the matrix's R'G'B' (0..=1):
/// 1. linearize to nits: PQ EOTF, or HLG inverse OETF with the BT.2100 OOTF (system gamma 1.2 at
///    1000 nits);
/// 2. BT.2020 primaries to BT.709 with [`BT2020_TO_BT709`], negatives clipped;
/// 3. take the brightest channel `m` and scale all three by `eetf(m) / m` (this keeps hue and
///    cannot push a channel above 1);
/// 4. divide by the SDR reference white and encode with the sRGB curve.
///
/// `eetf` is the ITU-R BT.2390-3 (section 5.4.1) EETF: a PQ-domain Hermite spline that leaves
/// values below the knee `ks = 1.5 * max_lum - 0.5` alone and rolls the rest off so the source peak
/// lands on the target peak (the same curve libplacebo and mpv use for `bt.2390`). Source and
/// target black are 0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneMap {
    /// 0: none (SDR), 1: PQ, 2: HLG.
    pub mode: f32,
    /// Rows of the linear primaries matrix (identity unless the picture is BT.2020).
    pub gamut: [[f32; 3]; 3],
    /// PQ code of black (`pq_inverse(0)`).
    pub e_black: f32,
    /// PQ code of the source peak minus `e_black`.
    pub e_range: f32,
    /// The target peak as a fraction of the source range, `max_lum` in BT.2390.
    pub max_lum: f32,
    /// Nits that map to 1.0.
    pub target_white: f32,
}

impl ToneMap {
    pub fn new(c: Colorimetry) -> Self {
        let e_black = pq_inverse(0.0);
        let e_range = pq_inverse(SOURCE_PEAK / PQ_PEAK) - e_black;
        Self {
            mode: match c.transfer {
                Transfer::Sdr => 0.0,
                Transfer::Pq => 1.0,
                Transfer::Hlg => 2.0,
            },
            gamut: if c.primaries == Primaries::Bt2020 { BT2020_TO_BT709 } else { IDENTITY },
            e_black,
            e_range,
            max_lum: (pq_inverse(TARGET_WHITE / PQ_PEAK) - e_black) / e_range,
            target_white: TARGET_WHITE,
        }
    }

    /// The BT.2390 EETF on a light level in nits, for a source of [`SOURCE_PEAK`].
    pub fn eetf(&self, nits: f32) -> f32 {
        let e1 = ((pq_inverse(nits.max(1e-6) / PQ_PEAK) - self.e_black) / self.e_range).clamp(0.0, 1.0);
        let ks = 1.5 * self.max_lum - 0.5;
        let e2 = if e1 < ks {
            e1
        } else {
            let t = (e1 - ks) / (1.0 - ks);
            let (t2, t3) = (t * t, t * t * t);
            (2.0 * t3 - 3.0 * t2 + 1.0) * ks + (t3 - 2.0 * t2 + t) * (1.0 - ks) + (-2.0 * t3 + 3.0 * t2) * self.max_lum
        };
        PQ_PEAK * pq_eotf(e2 * self.e_range + self.e_black)
    }

    /// R'G'B' (0..=1) of an HDR picture to SDR display code values (0..=1). Not for `mode == 0`.
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let nits = if self.mode < 1.5 {
            rgb.map(|c| PQ_PEAK * pq_eotf(c))
        } else {
            let s = rgb.map(hlg_inverse_oetf);
            let ys = 0.2627 * s[0] + 0.678 * s[1] + 0.0593 * s[2];
            let k = SOURCE_PEAK * ys.max(1e-6).powf(HLG_GAMMA - 1.0);
            s.map(|c| c * k)
        };
        let row = |r: &[f32; 3]| (r[0] * nits[0] + r[1] * nits[1] + r[2] * nits[2]).max(0.0);
        let lin = [row(&self.gamut[0]), row(&self.gamut[1]), row(&self.gamut[2])];
        let m = lin[0].max(lin[1]).max(lin[2]);
        if m <= 0.0 {
            return [0.0; 3];
        }
        let k = self.eetf(m) / m / self.target_white;
        lin.map(|c| display_encode((c * k).clamp(0.0, 1.0)))
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

    fn sdr() -> Colorimetry {
        Colorimetry { matrix: Matrix::Bt709, full_range: false, bits: 8, transfer: Transfer::Sdr, primaries: Primaries::Bt709 }
    }

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
        let m = YuvToRgb::new(Colorimetry { matrix: Matrix::Bt709, full_range: false, bits: 10, ..sdr() });
        let m8 = YuvToRgb::new(Colorimetry { matrix: Matrix::Bt709, full_range: false, bits: 8, ..sdr() });
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
            let full = YuvToRgb::new(Colorimetry { matrix, full_range: true, bits: 8, ..sdr() });
            assert_eq!(full.apply_raw(8, 0, 128, 128), [0, 0, 0]);
            assert_eq!(full.apply_raw(8, 255, 128, 128), [255, 255, 255]);
            let lim = YuvToRgb::new(Colorimetry { matrix, full_range: false, bits: 8, ..sdr() });
            assert_eq!(lim.apply_raw(8, 16, 128, 128), [0, 0, 0]);
            assert_eq!(lim.apply_raw(8, 235, 128, 128), [255, 255, 255]);
            // Pure red in full range: Y = kr * 255, Cr at its top.
            let (kr, kb) = matrix.kr_kb();
            let (y, cb, cr) = ((kr * 255.0).round() as u32, (128.0 - 127.5 * kr / (1.0 - kb)).round() as u32, 255);
            let red = full.apply_raw(8, y, cb, cr);
            assert!(red[0] >= 253 && red[1] <= 3 && red[2] <= 3, "{matrix:?} {red:?}");
        }
    }

    #[test]
    fn pq_eotf_known_values() {
        // ST 2084 code values: 100 nits is 0.508, 1000 nits is 0.752, 10 000 nits is 1.0.
        assert!((pq_inverse(100.0 / PQ_PEAK) - 0.5081).abs() < 1e-3);
        assert!((pq_inverse(1000.0 / PQ_PEAK) - 0.7518).abs() < 1e-3);
        assert!((pq_inverse(1.0) - 1.0).abs() < 1e-5);
        assert!((pq_eotf(1.0) - 1.0).abs() < 1e-4);
        assert!(pq_eotf(0.0) < 1e-6);
        for nits in [0.5f32, 10.0, 100.0, 203.0, 1000.0, 4000.0] {
            let back = PQ_PEAK * pq_eotf(pq_inverse(nits / PQ_PEAK));
            assert!((back / nits - 1.0).abs() < 2e-3, "{nits} nits came back as {back}");
        }
    }

    #[test]
    fn hlg_known_values() {
        // BT.2100: signal 0.5 is 1/12 of peak scene light, 1.0 is full scale, 0 is 0.
        assert!((hlg_inverse_oetf(0.5) - 1.0 / 12.0).abs() < 1e-6);
        assert!((hlg_inverse_oetf(1.0) - 1.0).abs() < 1e-4);
        assert_eq!(hlg_inverse_oetf(0.0), 0.0);
        // Reference white (signal 0.75) is 0.265 of scene light.
        assert!((hlg_inverse_oetf(0.75) - 0.2650).abs() < 1e-3);
    }

    #[test]
    fn the_gamut_matrix_is_the_standard_one_and_keeps_white() {
        for row in BT2020_TO_BT709 {
            assert!((row.iter().sum::<f32>() - 1.0).abs() < 1e-4, "{row:?}");
        }
        assert_eq!(BT2020_TO_BT709[0][0], 1.660_491);
        // BT.2020 white (1,1,1) is BT.709 white; pure BT.2020 green is outside BT.709.
        let white = BT2020_TO_BT709.map(|r| r[0] + r[1] + r[2]);
        assert!(white.iter().all(|w| (w - 1.0).abs() < 1e-4));
        let green: Vec<f32> = BT2020_TO_BT709.iter().map(|r| r[1]).collect();
        assert!(green[1] > 1.0 && green[0] < 0.0, "{green:?}");
    }

    fn pq_map() -> ToneMap {
        ToneMap::new(Colorimetry { transfer: Transfer::Pq, primaries: Primaries::Bt2020, ..sdr() })
    }

    #[test]
    fn the_tone_curve_is_monotonic_and_hits_its_ends() {
        let t = pq_map();
        assert!(t.eetf(1e-6) < 1e-3);
        let mut last = 0.0;
        for i in 1..=2000 {
            let nits = i as f32 * 0.5;
            let out = t.eetf(nits);
            assert!(out >= last - 1e-3, "{nits} nits gave {out} after {last}");
            last = out;
        }
        // The source peak lands on the target peak, and brighter input does not exceed it.
        assert!((t.eetf(SOURCE_PEAK) - TARGET_WHITE).abs() < 0.5, "{}", t.eetf(SOURCE_PEAK));
        assert!(t.eetf(4000.0) <= TARGET_WHITE + 0.5);
        // Well below the knee (about 90 nits here) the curve is the identity.
        for nits in [0.1f32, 1.0, 10.0, 50.0] {
            assert!((t.eetf(nits) / nits - 1.0).abs() < 1e-2, "{nits} nits gave {}", t.eetf(nits));
        }
    }

    #[test]
    fn tone_mapped_output_is_in_range_and_grey_stays_grey() {
        for t in [pq_map(), ToneMap::new(Colorimetry { transfer: Transfer::Hlg, primaries: Primaries::Bt2020, ..sdr() })] {
            assert_eq!(t.apply([0.0; 3]), [0.0; 3]);
            let white = t.apply([1.0; 3]);
            assert!(white.iter().all(|c| (c - 1.0).abs() < 0.02), "{white:?}");
            let mut last = 0.0;
            for i in 0..=100 {
                let c = i as f32 / 100.0;
                let o = t.apply([c; 3]);
                assert!(o.iter().all(|v| (0.0..=1.0).contains(v)));
                assert!((o[0] - o[1]).abs() < 1e-3 && (o[1] - o[2]).abs() < 1e-3, "{o:?}");
                assert!(o[0] >= last - 1e-4);
                last = o[0];
            }
            // Saturated BT.2020 red, outside BT.709, still comes out in range.
            let red = t.apply([0.8, 0.0, 0.0]);
            assert!(red.iter().all(|v| (0.0..=1.0).contains(v)), "{red:?}");
            assert!(red[0] > red[1] && red[0] > red[2]);
        }
    }

    #[test]
    fn colour_info_selects_the_matrix_range_and_transfer() {
        let f = yuv420(64, 48, 100, 128, 128);
        assert_eq!(Colorimetry::with_info(&f, None), Colorimetry::of(&f));
        let info = |primaries, transfer, matrix, full_range| Some(ColorInfo { primaries, transfer, matrix, full_range });
        let pq = Colorimetry::with_info(&f, info(9, 16, 9, false));
        assert_eq!((pq.matrix, pq.transfer, pq.primaries, pq.full_range), (Matrix::Bt2020, Transfer::Pq, Primaries::Bt2020, false));
        let hlg = Colorimetry::with_info(&f, info(9, 18, 10, true));
        assert_eq!((hlg.matrix, hlg.transfer, hlg.full_range), (Matrix::Bt2020, Transfer::Hlg, true));
        let sd = Colorimetry::with_info(&f, info(5, 6, 6, false));
        assert_eq!((sd.matrix, sd.transfer, sd.primaries), (Matrix::Bt601, Transfer::Sdr, Primaries::Bt709));
        // Unspecified falls back to the size guess.
        assert_eq!(Colorimetry::with_info(&f, Some(ColorInfo::UNSPECIFIED)), Colorimetry::of(&f));
        assert_eq!(Colorimetry::with_info(&yuv420(64, 720, 1, 1, 1), Some(ColorInfo::UNSPECIFIED)).matrix, Matrix::Bt709);
        assert!(pq.is_hdr() && !sd.is_hdr());
    }
}
