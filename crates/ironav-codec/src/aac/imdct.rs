//! Inverse MDCT by way of a DCT-IV, which is computed with a complex FFT of a quarter of the size.

use std::f64::consts::PI;

pub struct Imdct {
    /// Number of spectral coefficients (the window is twice this long).
    n: usize,
    pre_c: Vec<f32>,
    pre_s: Vec<f32>,
    post_c: Vec<f32>,
    post_s: Vec<f32>,
    /// Twiddles of each FFT stage, `len / 2` entries for a stage of butterflies `len` apart.
    stages: Vec<(Vec<f32>, Vec<f32>)>,
    rev: Vec<u32>,
    re: Vec<f32>,
    im: Vec<f32>,
    v: Vec<f32>,
}

impl Imdct {
    pub fn new(n: usize) -> Self {
        assert!(n.is_power_of_two() && n >= 8);
        let m = n / 2;
        let cis = |a: f64| (a.cos() as f32, a.sin() as f32);
        let pre: Vec<_> = (0..m).map(|k| cis(-PI * (4 * k + 1) as f64 / (4 * n) as f64)).collect();
        let post: Vec<_> = (0..m).map(|k| cis(-PI * k as f64 / n as f64)).collect();
        let mut stages = Vec::new();
        let mut len = 2;
        while len <= m {
            let tw: Vec<_> = (0..len / 2).map(|k| cis(-2.0 * PI * k as f64 / len as f64)).collect();
            stages.push((tw.iter().map(|t| t.0).collect(), tw.iter().map(|t| t.1).collect()));
            len *= 2;
        }
        let bits = m.trailing_zeros();
        let rev = (0..m as u32).map(|i| if bits == 0 { 0 } else { i.reverse_bits() >> (32 - bits) }).collect();
        Self {
            n,
            pre_c: pre.iter().map(|t| t.0).collect(),
            pre_s: pre.iter().map(|t| t.1).collect(),
            post_c: post.iter().map(|t| t.0).collect(),
            post_s: post.iter().map(|t| t.1).collect(),
            stages,
            rev,
            re: vec![0.0; m],
            im: vec![0.0; m],
            v: vec![0.0; n],
        }
    }

    /// DCT-IV of `x` (`n` values) into `self.v`: `v[m] = sum_k x[k] cos(pi/n (m + 1/2)(k + 1/2))`.
    fn dct4(&mut self, x: &[f32]) {
        let (n, m) = (self.n, self.n / 2);
        for k in 0..m {
            let (a, b) = (x[2 * k], x[n - 1 - 2 * k]);
            let (c, s) = (self.pre_c[k], self.pre_s[k]);
            let j = self.rev[k] as usize;
            self.re[j] = a * c - b * s;
            self.im[j] = a * s + b * c;
        }
        for (stage, (tc, ts)) in self.stages.iter().enumerate() {
            let len = 2usize << stage;
            let half = len / 2;
            for (re, im) in self.re.chunks_exact_mut(len).zip(self.im.chunks_exact_mut(len)) {
                let (re_lo, re_hi) = re.split_at_mut(half);
                let (im_lo, im_hi) = im.split_at_mut(half);
                for k in 0..half {
                    let (c, s) = (tc[k], ts[k]);
                    let tr = re_hi[k] * c - im_hi[k] * s;
                    let ti = re_hi[k] * s + im_hi[k] * c;
                    re_hi[k] = re_lo[k] - tr;
                    im_hi[k] = im_lo[k] - ti;
                    re_lo[k] += tr;
                    im_lo[k] += ti;
                }
            }
        }
        for k in 0..m {
            let (c, s) = (self.post_c[k], self.post_s[k]);
            self.v[2 * k] = self.re[k] * c - self.im[k] * s;
            self.v[n - 1 - 2 * k] = -(self.re[k] * s + self.im[k] * c);
        }
    }

    /// Inverse MDCT of `n` coefficients into `2n` samples, scaled by `scale`:
    /// `y[i] = scale * sum_k x[k] cos(pi/n (i + n/2 + 1/2)(k + 1/2))`.
    pub fn run(&mut self, x: &[f32], y: &mut [f32], scale: f32) {
        let n = self.n;
        self.dct4(x);
        let v = &self.v;
        for i in 0..n / 2 {
            y[i] = scale * v[n / 2 + i];
        }
        for i in n / 2..3 * n / 2 {
            y[i] = -scale * v[3 * n / 2 - 1 - i];
        }
        for i in 3 * n / 2..2 * n {
            y[i] = -scale * v[i - 3 * n / 2];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_definition() {
        for n in [16usize, 128] {
            let x: Vec<f32> = (0..n).map(|i| ((i * 37 % 11) as f32 - 5.0) / 7.0).collect();
            let mut y = vec![0f32; 2 * n];
            Imdct::new(n).run(&x, &mut y, 1.0);
            for (i, &got) in y.iter().enumerate() {
                let want: f64 = (0..n).map(|k| x[k] as f64 * (PI / n as f64 * (i as f64 + n as f64 / 2.0 + 0.5) * (k as f64 + 0.5)).cos()).sum();
                assert!((got as f64 - want).abs() < 1e-3, "n={n} i={i}: {got} vs {want}");
            }
        }
    }
}
