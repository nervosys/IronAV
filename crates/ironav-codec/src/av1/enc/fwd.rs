//! Forward transforms, measured from the decoder's inverse transform rather than written from the
//! formulas. A transform's synthesis basis is probed one coefficient at a time with
//! `av1::itx::inverse_transform`, so the encoder inverts exactly what the decoder computes, for
//! every size and type the encoder uses, and the rounding of the probes is the only error.
//!
//! The synthesis image of coefficient (v, u) is separable: `mu[v][u]` times the product of the
//! unit vectors `col[v]` (vertical) and `row[u]` (horizontal). A forward transform is then two
//! matrix products with those vectors, divided by the gain.

use std::collections::HashMap;

use crate::av1::itx::inverse_transform;
use crate::av1::tables::{TX_H_LOG2, TX_W_LOG2};

/// The coefficient the probes synthesise with: large enough that rounding is negligible.
const PROBE: i32 = 16384;

#[derive(Clone)]
pub struct Basis {
    w: usize,
    h: usize,
    tw: usize,
    th: usize,
    /// `th` unit vectors of `h` samples: the vertical basis of each row of coefficients.
    col: Vec<f64>,
    /// `tw` unit vectors of `w` samples: the horizontal basis of each column of coefficients.
    row: Vec<f64>,
    /// The gain of each coefficient, `th * tw` of them, row-major.
    mu: Vec<f64>,
}

/// The unit vector along the line of `img` (`rows` x `cols`, row-major) with the most energy, where
/// `vertical` means the line is a column of the image.
fn strongest_line(img: &[f64], rows: usize, cols: usize, vertical: bool) -> Vec<f64> {
    let energy = |i: usize| -> f64 {
        if vertical {
            (0..rows).map(|y| img[y * cols + i].powi(2)).sum()
        } else {
            (0..cols).map(|x| img[i * cols + x].powi(2)).sum()
        }
    };
    let best = (0..if vertical { cols } else { rows }).max_by(|&a, &b| energy(a).total_cmp(&energy(b))).unwrap_or(0);
    if vertical {
        (0..rows).map(|y| img[y * cols + best]).collect()
    } else {
        img[best * cols..(best + 1) * cols].to_vec()
    }
}

fn unit(mut v: Vec<f64>) -> Vec<f64> {
    let n = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

impl Basis {
    /// The basis of a transform of size `tx` and type `tt`, as the decoder inverts it.
    pub fn new(tx: usize, tt: u8) -> Basis {
        let (w, h) = (1usize << TX_W_LOG2[tx], 1usize << TX_H_LOG2[tx]);
        let (tw, th) = (w.min(32), h.min(32));
        let probe = |pos: usize| -> Vec<f64> {
            let mut dq = vec![0i32; tw * th];
            dq[pos] = PROBE;
            let mut res = vec![0i32; w * h];
            inverse_transform(&dq, tx, tt, 8, false, &mut res);
            res.into_iter().map(|v| v as f64).collect()
        };
        // Probes of the first column of coefficients (one per row frequency) and of the first row.
        let cols: Vec<Vec<f64>> = (0..th).map(|v| probe(v * tw)).collect();
        let rows: Vec<Vec<f64>> = (0..tw).map(probe).collect();

        let mut col = Vec::with_capacity(th * h);
        for img in &cols {
            col.extend(unit(strongest_line(img, h, w, true)));
        }
        let mut row = Vec::with_capacity(tw * w);
        for img in &rows {
            row.extend(unit(strongest_line(img, h, w, false)));
        }

        // The gain of (v, 0) and (0, u) against the unit vectors; (v, u) is their product over the gain of (0, 0).
        let gain = |img: &[f64], v: usize, u: usize| -> f64 {
            let (c, r) = (&col[v * h..(v + 1) * h], &row[u * w..(u + 1) * w]);
            let mut s = 0.0;
            for y in 0..h {
                for x in 0..w {
                    s += img[y * w + x] * c[y] * r[x];
                }
            }
            s / PROBE as f64
        };
        let mu00 = gain(&cols[0], 0, 0);
        let mut mu = vec![1.0; th * tw];
        if mu00.abs() > 1e-9 {
            for v in 0..th {
                for u in 0..tw {
                    mu[v * tw + u] = gain(&cols[v], v, 0) * gain(&rows[u], 0, u) / mu00;
                }
            }
        }
        Basis { w, h, tw, th, col, row, mu }
    }

    /// The coefficients (row-major, `min(w, 32)` wide) whose synthesis gives `res` (row-major, `w` wide).
    pub fn forward(&self, res: &[i32], out: &mut [f64]) {
        let (w, h, tw, th) = (self.w, self.h, self.tw, self.th);
        let mut t = vec![0.0f64; th * w];
        for v in 0..th {
            for y in 0..h {
                let c = self.col[v * h + y];
                if c == 0.0 {
                    continue;
                }
                for x in 0..w {
                    t[v * w + x] += c * res[y * w + x] as f64;
                }
            }
        }
        for v in 0..th {
            for u in 0..tw {
                let r = &self.row[u * w..(u + 1) * w];
                let s: f64 = (0..w).map(|x| t[v * w + x] * r[x]).sum();
                let mu = self.mu[v * tw + u];
                out[v * tw + u] = if mu.abs() > 1e-12 { s / mu } else { 0.0 };
            }
        }
    }
}

/// The bases of the transforms an encoder uses, built on first use.
#[derive(Default, Clone)]
pub struct Bases(HashMap<(usize, u8), Basis>);

impl Bases {
    /// Adds the bases `other` has built that this lacks.
    pub fn merge(&mut self, other: &Bases) {
        for (k, b) in &other.0 {
            self.0.entry(*k).or_insert_with(|| b.clone());
        }
    }

    pub fn get(&mut self, tx: usize, tt: u8) -> &Basis {
        self.0.entry((tx, tt)).or_insert_with(|| Basis::new(tx, tt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::av1::tables::{DCT_DCT, TX_16X16, TX_4X4, TX_8X8};

    /// A coefficient set through the decoder's inverse and back must come out (nearly) unchanged.
    #[test]
    fn forward_inverts_the_decoder_inverse() {
        for &(tx, tt) in &[(TX_4X4, DCT_DCT), (TX_8X8, DCT_DCT), (TX_16X16, DCT_DCT), (6, DCT_DCT), (TX_8X8, 1), (TX_4X4, 3), (TX_16X16, 2)] {
            let (w, h) = (1usize << TX_W_LOG2[tx], 1usize << TX_H_LOG2[tx]);
            let (tw, th) = (w.min(32), h.min(32));
            let mut coef = vec![0i32; tw * th];
            for (i, c) in coef.iter_mut().enumerate() {
                *c = ((i as i32 * 37 + 11) % 61 - 30) * 64;
            }
            let mut res = vec![0i32; w * h];
            inverse_transform(&coef, tx, tt, 8, false, &mut res);
            let mut back = vec![0.0; tw * th];
            Bases::default().get(tx, tt).forward(&res, &mut back);
            for (i, (&c, &b)) in coef.iter().zip(&back).enumerate() {
                assert!((c as f64 - b).abs() <= 0.03 * c.abs() as f64 + 16.0, "tx {tx} type {tt} coefficient {i}: {c} came back {b}");
            }
        }
    }
}
