//! The inverse MDCT of CELT, for sizes of the form `15 * 2^k`, as a DCT-IV computed by a mixed-radix
//! (2, 3, 5) complex FFT of half the size.

use std::f64::consts::PI;

type C = [f32; 2];

fn mul(a: C, b: C) -> C {
    [a[0] * b[0] - a[1] * b[1], a[0] * b[1] + a[1] * b[0]]
}

struct Fft {
    m: usize,
    factors: Vec<usize>,
    /// `exp(-2 pi i t / m)` for `t` in `0..m`.
    tw: Vec<C>,
}

impl Fft {
    fn new(m: usize) -> Self {
        let mut factors = Vec::new();
        let mut rest = m;
        for p in [5, 3, 2] {
            while rest % p == 0 {
                factors.push(p);
                rest /= p;
            }
        }
        assert_eq!(rest, 1, "FFT length {m} has a prime factor above 5");
        // Larger radices last: they run on the smallest sub-transforms.
        factors.sort_unstable();
        let tw = (0..m).map(|t| {
            let a = -2.0 * PI * t as f64 / m as f64;
            [a.cos() as f32, a.sin() as f32]
        });
        Self { m, factors, tw: tw.collect() }
    }

    fn run(&self, input: &[C], out: &mut [C]) {
        self.rec(input, 1, self.m, out, 0);
    }

    fn rec(&self, input: &[C], stride: usize, n: usize, out: &mut [C], f: usize) {
        if n == 1 {
            out[0] = input[0];
            return;
        }
        let p = self.factors[f];
        let m = n / p;
        for r in 0..p {
            self.rec(&input[r * stride..], stride * p, m, &mut out[r * m..(r + 1) * m], f + 1);
        }
        let step = self.m / n; // W_n^t = tw[t * step]
        let pstep = self.m / p; // W_p^t = tw[t * pstep]
        let mut y = [[0f32; 2]; 5];
        for k in 0..m {
            for r in 0..p {
                y[r] = if r == 0 { out[k] } else { mul(out[r * m + k], self.tw[r * k * step]) };
            }
            for q in 0..p {
                let mut acc = y[0];
                for r in 1..p {
                    let w = self.tw[(r * q % p) * pstep];
                    let t = mul(y[r], w);
                    acc[0] += t[0];
                    acc[1] += t[1];
                }
                out[k + q * m] = acc;
            }
        }
    }
}

pub struct Dct4 {
    n: usize,
    pre: Vec<C>,
    post: Vec<C>,
    fft: Fft,
    a: Vec<C>,
    b: Vec<C>,
}

impl Dct4 {
    pub fn new(n: usize) -> Self {
        let m = n / 2;
        let cis = |a: f64| [a.cos() as f32, a.sin() as f32];
        Self {
            n,
            pre: (0..m).map(|k| cis(-PI * (4 * k + 1) as f64 / (4 * n) as f64)).collect(),
            post: (0..m).map(|k| cis(-PI * k as f64 / n as f64)).collect(),
            fft: Fft::new(m),
            a: vec![[0.0; 2]; m],
            b: vec![[0.0; 2]; m],
        }
    }

    /// `v[m] = sum_k x[k * stride] cos(pi/n (m + 1/2)(k + 1/2))` for `m` in `0..n`.
    pub fn run(&mut self, x: &[f32], stride: usize, v: &mut [f32]) {
        let (n, m) = (self.n, self.n / 2);
        for k in 0..m {
            let (a, b) = (x[2 * k * stride], x[(n - 1 - 2 * k) * stride]);
            self.a[k] = mul([a, b], self.pre[k]);
        }
        self.fft.run(&self.a, &mut self.b);
        for k in 0..m {
            let t = mul(self.b[k], self.post[k]);
            v[2 * k] = t[0];
            v[n - 1 - 2 * k] = -t[1];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dct4_matches_the_definition_for_every_celt_size() {
        for n in [120usize, 240, 480, 960] {
            let x: Vec<f32> = (0..n).map(|i| ((i * 37 % 13) as f32 - 6.0) / 9.0).collect();
            let mut v = vec![0f32; n];
            Dct4::new(n).run(&x, 1, &mut v);
            for m in (0..n).step_by(7) {
                let want: f64 = (0..n).map(|k| x[k] as f64 * (PI / n as f64 * (m as f64 + 0.5) * (k as f64 + 0.5)).cos()).sum();
                assert!((v[m] as f64 - want).abs() < 2e-3, "n={n} m={m}: {} vs {want}", v[m]);
            }
        }
    }

    #[test]
    fn a_stride_picks_every_nth_coefficient() {
        let n = 120;
        let x: Vec<f32> = (0..n * 3).map(|i| (i % 17) as f32 - 8.0).collect();
        let every: Vec<f32> = x.iter().step_by(3).copied().collect();
        let (mut a, mut b) = (vec![0f32; n], vec![0f32; n]);
        let mut d = Dct4::new(n);
        d.run(&x, 3, &mut a);
        d.run(&every, 1, &mut b);
        assert_eq!(a, b);
    }
}
