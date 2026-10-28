//! The reference picture's luma at every half-sample position, worked out once per picture so motion
//! search and prediction only average two of these planes (8.4.2.2.1: each quarter sample is the
//! rounded mean of the two nearest integer or half samples).

/// Samples of border kept around each plane, enough for a block 24 samples outside the picture plus
/// the interpolation taps.
const PAD: usize = 48;

pub struct HalfPel {
    stride: usize,
    /// Integer samples (with the picture's edges repeated), and the half samples to the right (b),
    /// below (h) and diagonally between (j), each at the integer sample's index.
    planes: [Vec<u8>; 4],
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
    /// Builds the planes from a `pw` x `ph` luma plane.
    pub fn new(luma: &[u16], pw: usize, ph: usize) -> Self {
        let (sw, sh) = (pw + 2 * PAD, ph + 2 * PAD);
        let mut g = vec![0u8; sw * sh];
        for y in 0..sh {
            let sy = y.saturating_sub(PAD).min(ph - 1);
            for x in 0..sw {
                g[y * sw + x] = luma[sy * pw + x.saturating_sub(PAD).min(pw - 1)] as u8;
            }
        }
        let mut b = vec![0u8; sw * sh];
        let mut h = vec![0u8; sw * sh];
        let mut j = vec![0u8; sw * sh];
        // unrounded horizontal sums, which the diagonal half samples filter again vertically
        let mut b1 = vec![0i32; sw * sh];
        let at = |x: usize, y: usize| g[y * sw + x] as i32;
        for y in 0..sh {
            for x in 2..sw - 3 {
                let v = tap6(at(x - 2, y), at(x - 1, y), at(x, y), at(x + 1, y), at(x + 2, y), at(x + 3, y));
                b1[y * sw + x] = v;
                b[y * sw + x] = ((v + 16) >> 5).clamp(0, 255) as u8;
            }
        }
        for y in 2..sh - 3 {
            for x in 0..sw {
                let v = tap6(at(x, y - 2), at(x, y - 1), at(x, y), at(x, y + 1), at(x, y + 2), at(x, y + 3));
                h[y * sw + x] = ((v + 16) >> 5).clamp(0, 255) as u8;
                let c = |yy: usize| b1[yy * sw + x];
                let v = tap6(c(y - 2), c(y - 1), c(y), c(y + 1), c(y + 2), c(y + 3));
                j[y * sw + x] = ((v + 512) >> 10).clamp(0, 255) as u8;
            }
        }
        HalfPel { stride: sw, planes: [g, b, h, j] }
    }

    fn offset(&self, x: i32, y: i32) -> usize {
        (y + PAD as i32) as usize * self.stride + (x + PAD as i32) as usize
    }

    /// The `w` x `h` block at (x, y) displaced by `mv` (quarter samples), as the decoder predicts it.
    pub fn predict(&self, x: i32, y: i32, mv: [i16; 2], w: usize, h: usize, out: &mut [u16]) {
        let (fx, fy) = ((mv[0] & 3) as usize, (mv[1] & 3) as usize);
        let (ix, iy) = (x + (mv[0] >> 2) as i32, y + (mv[1] >> 2) as i32);
        let (pa, ax, ay, pb, bx, by) = MIX[fy][fx];
        let s = self.stride;
        let a = &self.planes[pa][self.offset(ix + ax as i32, iy + ay as i32)..];
        if pa == pb && (ax, ay) == (bx, by) {
            for r in 0..h {
                for c in 0..w {
                    out[r * w + c] = a[r * s + c] as u16;
                }
            }
        } else {
            let b = &self.planes[pb][self.offset(ix + bx as i32, iy + by as i32)..];
            for r in 0..h {
                for c in 0..w {
                    out[r * w + c] = ((a[r * s + c] as u16 + b[r * s + c] as u16 + 1) >> 1) as u16;
                }
            }
        }
    }

    /// The integer-sample 16x16 block at (x, y), row by row.
    pub fn rows16(&self, x: i32, y: i32) -> impl Iterator<Item = &[u8]> {
        let o = self.offset(x, y);
        (0..16).map(move |r| &self.planes[0][o + r * self.stride..o + r * self.stride + 16])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::inter::luma_mc;

    #[test]
    fn predictions_equal_the_decoders_interpolation() {
        let (pw, ph) = (48usize, 32usize);
        let mut seed = 99u32;
        let luma: Vec<u16> = (0..pw * ph)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                ((seed >> 16) % 256) as u16
            })
            .collect();
        let hp = HalfPel::new(&luma, pw, ph);
        for (x, y) in [(0, 0), (16, 16), (32, 16)] {
            for my in -100i16..100 {
                for mx in [-97i16, -40, -3, -2, -1, 0, 1, 2, 3, 5, 6, 7, 30, 97] {
                    let (mut a, mut b) = ([0u16; 256], [0u16; 256]);
                    hp.predict(x, y, [mx, my], 16, 16, &mut a);
                    luma_mc(&luma, pw, ph, x, y, mx as i32, my as i32, 16, 16, &mut b, 255);
                    assert_eq!(a, b, "block ({x}, {y}) vector ({mx}, {my})");
                }
            }
        }
    }
}
