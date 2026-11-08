//! The reference picture's luma at every half-sample position, worked out once per picture so motion
//! search and prediction only average two of these planes (8.4.2.2.1: each quarter sample is the
//! rounded mean of the two nearest integer or half samples).

/// Samples of border kept around each plane, enough for a block 24 samples outside the picture plus
/// the interpolation taps.
const PAD: usize = 48;

pub struct HalfPel {
    stride: usize,
    /// The picture's size.
    pw: usize,
    ph: usize,
    /// Integer samples (with the picture's edges repeated), and the half samples to the right (b),
    /// below (h) and diagonally between (j), each at the integer sample's index.
    planes: [Vec<u16>; 4],
    /// The unrounded horizontal sums, kept between pictures when a picture's buffers are reused.
    scratch: Vec<i32>,
}

fn tap6(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32) -> i32 {
    a - 5 * b + 20 * c + 20 * d - 5 * e + f
}

/// Which planes, at which sample offsets, a quarter-sample fraction averages (one plane when both are equal).
const MIX: [[(usize, usize, usize, usize, usize, usize); 4]; 4] = {
    const G: usize = 0;
    const B: usize = 1;
    const H: usize = 2;
    const J: usize = 3;
    // indexed [fy][fx]: (plane, dx, dy, plane, dx, dy)
    [
        [(G, 0, 0, G, 0, 0), (G, 0, 0, B, 0, 0), (B, 0, 0, B, 0, 0), (G, 1, 0, B, 0, 0)],
        [(G, 0, 0, H, 0, 0), (B, 0, 0, H, 0, 0), (B, 0, 0, J, 0, 0), (B, 0, 0, H, 1, 0)],
        [(H, 0, 0, H, 0, 0), (H, 0, 0, J, 0, 0), (J, 0, 0, J, 0, 0), (J, 0, 0, H, 1, 0)],
        [(G, 0, 1, H, 0, 0), (H, 0, 0, B, 0, 1), (J, 0, 0, B, 0, 1), (H, 1, 0, B, 0, 1)],
    ]
};

impl HalfPel {
    /// Builds the planes from a `pw` x `ph` luma plane of `bd`-bit samples.
    pub fn new(luma: &[u16], pw: usize, ph: usize, bd: u32) -> Self {
        Self::with_threads(luma, pw, ph, bd, 1)
    }

    /// `new`, with the planes' rows worked out on up to `threads` threads at once (the same planes).
    pub fn with_threads(luma: &[u16], pw: usize, ph: usize, bd: u32, threads: usize) -> Self {
        Self::build(luma, pw, ph, bd, threads, None, false)
    }

    /// `with_threads`, in the buffers of `spare` (a picture of the same size that is no longer needed) when
    /// it has them, and keeping its scratch space for the next picture.
    pub fn rebuild(spare: Option<HalfPel>, luma: &[u16], pw: usize, ph: usize, bd: u32, threads: usize) -> Self {
        Self::build(luma, pw, ph, bd, threads, spare, true)
    }

    fn build(luma: &[u16], pw: usize, ph: usize, bd: u32, threads: usize, spare: Option<HalfPel>, keep: bool) -> Self {
        let maxv = (1i32 << bd) - 1;
        let (sw, sh) = (pw + 2 * PAD, ph + 2 * PAD);
        let n = sw * sh;
        // the planes of a picture of the same size, when given, are written over
        let (mut planes, mut b1) = match spare {
            Some(s) if s.stride == sw && s.ph == ph && s.scratch.len() == n => (s.planes, s.scratch),
            _ => ([vec![0u16; n], vec![0u16; n], vec![0u16; n], vec![0u16; n]], vec![0i32; n]),
        };
        let [g, b, h, j] = &mut planes;
        // unrounded horizontal sums, which the diagonal half samples filter again vertically
        let t = threads.clamp(1, (sh / 16).max(1));
        let per = sh.div_ceil(t);
        // the integer samples with the picture's edges repeated, and the horizontal half samples, row by row
        let round1 = |k: usize, g: &mut [u16], b: &mut [u16], b1: &mut [i32]| {
            for r in 0..g.len() / sw {
                let y = k * per + r;
                let sy = y.saturating_sub(PAD).min(ph - 1);
                let (gr, br, b1r) = (&mut g[r * sw..(r + 1) * sw], &mut b[r * sw..(r + 1) * sw], &mut b1[r * sw..(r + 1) * sw]);
                for (x, v) in gr.iter_mut().enumerate() {
                    *v = luma[sy * pw + x.saturating_sub(PAD).min(pw - 1)];
                }
                for x in 2..sw - 3 {
                    let v = tap6(gr[x - 2] as i32, gr[x - 1] as i32, gr[x] as i32, gr[x + 1] as i32, gr[x + 2] as i32, gr[x + 3] as i32);
                    b1r[x] = v;
                    br[x] = ((v + 16) >> 5).clamp(0, maxv) as u16;
                }
            }
        };
        if t == 1 {
            round1(0, g, b, &mut b1);
        } else {
            crate::pool::each(g.chunks_mut(per * sw).zip(b.chunks_mut(per * sw)).zip(b1.chunks_mut(per * sw)).collect(), |k, ((g, b), b1)| round1(k, g, b, b1));
        }
        // the vertical half samples and the diagonal ones, which need the rows above and below
        let (gref, b1ref) = (&*g, &b1);
        let round2 = |k: usize, h: &mut [u16], j: &mut [u16]| {
            for r in 0..h.len() / sw {
                let y = k * per + r;
                if !(2..sh - 3).contains(&y) {
                    continue;
                }
                for x in 0..sw {
                    let at = |yy: usize| gref[yy * sw + x] as i32;
                    let v = tap6(at(y - 2), at(y - 1), at(y), at(y + 1), at(y + 2), at(y + 3));
                    h[r * sw + x] = ((v + 16) >> 5).clamp(0, maxv) as u16;
                    let c = |yy: usize| b1ref[yy * sw + x];
                    let v = tap6(c(y - 2), c(y - 1), c(y), c(y + 1), c(y + 2), c(y + 3));
                    j[r * sw + x] = ((v + 512) >> 10).clamp(0, maxv) as u16;
                }
            }
        };
        if t == 1 {
            round2(0, h, j);
        } else {
            crate::pool::each(h.chunks_mut(per * sw).zip(j.chunks_mut(per * sw)).collect(), |k, (h, j)| round2(k, h, j));
        }
        HalfPel { stride: sw, pw, ph, planes, scratch: if keep { b1 } else { Vec::new() } }
    }

    fn offset(&self, x: i32, y: i32) -> usize {
        (y + PAD as i32) as usize * self.stride + (x + PAD as i32) as usize
    }

    /// The `w` x `h` block at (x, y) displaced by `mv` (quarter samples), as the decoder predicts it.
    pub fn predict(&self, x: i32, y: i32, mv: [i16; 2], w: usize, h: usize, out: &mut [u16]) {
        let (fx, fy) = ((mv[0] & 3) as usize, (mv[1] & 3) as usize);
        // a block wholly beyond an edge (taps included) sees only the edge's samples, however far
        // beyond it is, so any vector a decoder accepts can be predicted from the border kept
        let ix = (x + (mv[0] >> 2) as i32).clamp(-(w as i32) - 3, self.pw as i32 + 1);
        let iy = (y + (mv[1] >> 2) as i32).clamp(-(h as i32) - 3, self.ph as i32 + 1);
        let (pa, ax, ay, pb, bx, by) = MIX[fy][fx];
        let s = self.stride;
        let a = &self.planes[pa][self.offset(ix + ax as i32, iy + ay as i32)..];
        if pa == pb && (ax, ay) == (bx, by) {
            for r in 0..h {
                for c in 0..w {
                    out[r * w + c] = a[r * s + c];
                }
            }
        } else {
            let b = &self.planes[pb][self.offset(ix + bx as i32, iy + by as i32)..];
            for r in 0..h {
                for c in 0..w {
                    out[r * w + c] = (a[r * s + c] as u32 + b[r * s + c] as u32).div_ceil(2) as u16;
                }
            }
        }
    }

    /// The integer-sample `w` x `h` block at (x, y), row by row.
    pub fn rows(&self, x: i32, y: i32, w: usize, h: usize) -> impl Iterator<Item = &[u16]> {
        let o = self.offset(x, y);
        (0..h).map(move |r| &self.planes[0][o + r * self.stride..o + r * self.stride + w])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::inter::luma_mc;

    #[test]
    fn predictions_equal_the_decoders_interpolation() {
        for bd in [8u32, 10] {
            let (pw, ph) = (48usize, 32usize);
            let mut seed = 99u32;
            let luma: Vec<u16> = (0..pw * ph)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    ((seed >> 16) % (1 << bd)) as u16
                })
                .collect();
            let maxv = (1 << bd) - 1;
            let hp = HalfPel::new(&luma, pw, ph, bd);
            for (x, y) in [(0, 0), (16, 16), (32, 16)] {
                for my in -100i16..100 {
                    for mx in [-97i16, -40, -3, -2, -1, 0, 1, 2, 3, 5, 6, 7, 30, 97] {
                        let (mut a, mut b) = ([0u16; 256], [0u16; 256]);
                        hp.predict(x, y, [mx, my], 16, 16, &mut a);
                        luma_mc(&luma, pw, ph, x, y, mx as i32, my as i32, 16, 16, &mut b, maxv);
                        assert_eq!(a, b, "{bd}-bit block ({x}, {y}) vector ({mx}, {my})");
                    }
                }
            }
            // vectors far outside the picture, as temporal direct prediction can give
            for (x, y) in [(0, 0), (40, 24), (32, 16)] {
                for my in [-2049i16, -1203, -300, -1, 2, 299, 1205, 2047] {
                    for mx in [-8191i16, -2001, -401, 3, 398, 1999, 8190] {
                        let (mut a, mut b) = ([0u16; 64], [0u16; 64]);
                        hp.predict(x, y, [mx, my], 8, 8, &mut a);
                        luma_mc(&luma, pw, ph, x, y, mx as i32, my as i32, 8, 8, &mut b, maxv);
                        assert_eq!(a, b, "{bd}-bit 8x8 block ({x}, {y}) vector ({mx}, {my})");
                    }
                }
            }
        }
    }
}
