//! The synthesis filter bank of AC-3: the Kaiser-Bessel-derived window (alpha 5, 256 points), the
//! 256-coefficient and 128-coefficient inverse MDCTs, and the overlap with the previous block's
//! samples. The window and the overlap follow FFmpeg's `kbdwin.c` and `float_dsp.c`
//! (`vector_fmul_window`); the transforms are IronAV's own inverse MDCT.

use super::tables::{BESSEL_P1, BESSEL_P2, BESSEL_Q1, BESSEL_Q2};
use crate::aac::imdct::Imdct;
use std::f64::consts::PI;

pub(super) const BLOCK: usize = 256;

pub(super) struct Synth {
    imdct_long: Imdct,
    imdct_short: Imdct,
    y_long: Vec<f32>,
    y_short: Vec<f32>,
    /// The window: 256 points, rising over the first half and falling over the second.
    pub window: [f32; BLOCK],
}

impl Synth {
    pub(super) fn new() -> Self {
        Synth {
            imdct_long: Imdct::new(256),
            imdct_short: Imdct::new(128),
            y_long: vec![0.0; 512],
            y_short: vec![0.0; 256],
            window: kbd_window(5.0, BLOCK),
        }
    }

    /// The inverse MDCT of 256 coefficients: 512 samples, of which the overlap uses the
    /// quarters `[0, 128)` (odd-symmetric about 127.5) and `[256, 384)`.
    pub(super) fn imdct256(&mut self, x: &[f32]) -> &[f32] {
        self.imdct_long.run(x, &mut self.y_long, 1.0);
        &self.y_long
    }

    /// The inverse MDCT of 128 coefficients: 256 samples.
    pub(super) fn imdct128(&mut self, x: &[f32]) -> &[f32] {
        self.imdct_short.run(x, &mut self.y_short, 1.0);
        &self.y_short
    }
}

/// `vector_fmul_window` of FFmpeg: `dst` gets `2 * len` samples from the previous block's
/// overlap `src0`, the transform's first half `src1` (`len` values used) and the window.
pub(super) fn fmul_window(dst: &mut [f32], src0: &[f32], src1: &[f32], win: &[f32], len: usize) {
    for t in 0..len {
        let j = len - 1 - t;
        let (s0, s1) = (src0[t], src1[j]);
        let (wi, wj) = (win[t], win[len + j]);
        dst[t] = s0 * wj - s1 * wi;
        dst[len + j] = s0 * wi + s1 * wj;
    }
}

/// The Kaiser-Bessel-derived window of `n` points (FFmpeg's `kbd_window_init`).
pub(super) fn kbd_window(alpha: f64, n: usize) -> [f32; BLOCK] {
    assert_eq!(n, BLOCK);
    let alpha2 = 4.0 * (alpha * PI / n as f64) * (alpha * PI / n as f64);
    let mut temp = vec![0.0f64; n / 2 + 1];
    let mut scale = 0.0f64;
    for (i, t) in temp.iter_mut().enumerate() {
        let x = (i * (n - i)) as f64 * alpha2;
        *t = bessel_i0(x.sqrt());
        let weight = if i != 0 && i < n / 2 { 2.0 } else { 1.0 };
        scale += *t * weight;
    }
    scale = 1.0 / (scale + 1.0);
    let mut w = [0f32; BLOCK];
    let mut sum = 0.0f64;
    for i in 0..=n / 2 {
        sum += temp[i];
        w[i] = (sum * scale).sqrt() as f32;
    }
    for i in n / 2 + 1..n {
        sum += temp[n - i];
        w[i] = (sum * scale).sqrt() as f32;
    }
    w
}

/// The modified Bessel function of the first kind, order zero, as FFmpeg's `av_bessel_i0` computes
/// it: minimax rational approximations on two intervals (the window's values depend on it).
fn bessel_i0(x: f64) -> f64 {
    // Horner evaluation of a polynomial given by its coefficients, lowest order first
    fn eval_poly(coeff: &[f64], x: f64) -> f64 {
        let mut sum = coeff[coeff.len() - 1];
        for c in coeff[..coeff.len() - 1].iter().rev() {
            sum *= x;
            sum += c;
        }
        sum
    }
    let x = x.abs();
    if x == 0.0 {
        return 1.0;
    }
    if x <= 15.0 {
        let y = x * x;
        eval_poly(&BESSEL_P1, y) / eval_poly(&BESSEL_Q1, y)
    } else {
        let y = 1.0 / x - 1.0 / 15.0;
        let r = eval_poly(&BESSEL_P2, y) / eval_poly(&BESSEL_Q2, y);
        let factor = x.exp() / x.sqrt();
        factor * r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_is_power_complementary() {
        let w = kbd_window(5.0, BLOCK);
        // the window is symmetric in the sense of FFmpeg's kbd_window_init: index i pairs with 255 - i
        for i in 0..BLOCK / 2 {
            let s = w[i] * w[i] + w[BLOCK - 1 - i] * w[BLOCK - 1 - i];
            assert!((s - 1.0).abs() < 1e-5, "i={i}: {s}");
        }
    }

    #[test]
    fn bessel_matches_a_known_value() {
        // I0(1) = 1.2660658777520084
        assert!((bessel_i0(1.0) - 1.2660658777520084).abs() < 1e-12);
    }
}
