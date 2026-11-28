//! Scaling (dequantisation) and the inverse transforms of HEVC (ITU-T H.265 8.6).

use super::ps::ScalingList;
use super::tables::{DIAG_SCAN4X4_X, DIAG_SCAN4X4_Y, DIAG_SCAN8X8_X, DIAG_SCAN8X8_Y, TRANSFORM};

pub const LEVEL_SCALE: [i32; 6] = [40, 45, 51, 57, 64, 72];

pub(super) const DST4: [[i32; 4]; 4] = [[29, 55, 74, 84], [74, 74, 0, -74], [84, -29, -74, 55], [55, -84, 74, -29]];

/// Expanded scaling factors: `factor[size_id][matrix_id]` is a row-major `n * n` array.
pub struct ScalingFactors {
    factor: [[Vec<u8>; 6]; 4],
}

impl ScalingFactors {
    pub fn new(list: &ScalingList) -> Self {
        let mk = |size_id: usize, m: usize| -> Vec<u8> {
            let n = 4usize << size_id;
            let mut f = vec![0u8; n * n];
            let l = &list.lists[size_id][m];
            if size_id == 0 {
                for i in 0..16 {
                    f[DIAG_SCAN4X4_Y[i] as usize * 4 + DIAG_SCAN4X4_X[i] as usize] = l[i];
                }
            } else {
                let rep = n / 8;
                for i in 0..64 {
                    let (sx, sy) = (DIAG_SCAN8X8_X[i] as usize, DIAG_SCAN8X8_Y[i] as usize);
                    for dy in 0..rep {
                        for dx in 0..rep {
                            f[(sy * rep + dy) * n + sx * rep + dx] = l[i];
                        }
                    }
                }
                if size_id >= 2 {
                    f[0] = list.dc[size_id - 2][m];
                }
            }
            f
        };
        let factor = std::array::from_fn(|s| std::array::from_fn(|m| mk(s, m)));
        ScalingFactors { factor }
    }

    /// The factors for a transform block of `1 << log2` samples; `matrix_id` is 3 * inter + colour component.
    pub fn get(&self, log2: u32, matrix_id: usize) -> &[u8] {
        &self.factor[log2 as usize - 2][matrix_id]
    }
}

/// Scales the coefficient levels in `c` (row-major, `n * n`) in place (8.6.4.2). `m` holds the
/// scaling factors, or `None` for the flat 16.
pub fn dequantise(c: &mut [i32], log2: u32, qp: i32, bd: u32, m: Option<&[u8]>, max_x: usize, max_y: usize) {
    let n = 1usize << log2;
    let shift = bd + log2 + 10 - 15;
    let scale = LEVEL_SCALE[(qp % 6) as usize];
    let up = (qp / 6) as u32;
    let rnd = 1i64 << (shift - 1);
    for y in 0..=max_y {
        for x in 0..=max_x {
            let v = c[y * n + x];
            if v != 0 {
                let f = m.map_or(16, |m| m[y * n + x] as i64);
                let s = ((v as i64 * f * ((scale as i64) << up)) + rnd) >> shift;
                c[y * n + x] = s.clamp(-32768, 32767) as i32;
            }
        }
    }
}

/// The inverse transform of an `n` x `n` block in place (8.6.4.2): a column pass, a row pass and the
/// final rounding. `max_x` and `max_y` bound the non-zero coefficients.
pub fn inverse(c: &mut [i32], log2: u32, dst: bool, bd: u32, max_x: usize, max_y: usize) {
    if dst {
        return inverse_matrix(c, log2, true, bd, max_x, max_y);
    }
    let n = 1usize << log2;
    let shift = 20 - bd;
    let rnd = 1i32 << (shift - 1);
    if max_x == 0 && max_y == 0 {
        // only the DC coefficient: every sample gets the same value
        let t = ((64 * c[0] + 64) >> 7).clamp(-32768, 32767);
        c[..n * n].fill((64 * t + rnd) >> shift);
        return;
    }
    match log2 {
        2 => inverse_fast::<16>(c, 2, shift, max_x, max_y),
        3 => inverse_fast::<64>(c, 3, shift, max_x, max_y),
        4 => inverse_fast::<256>(c, 4, shift, max_x, max_y),
        _ => inverse_fast::<1024>(c, 5, shift, max_x, max_y),
    }
}

/// One dimension of the inverse DCT of `n` points: `out` is the transform of `s`, whose entries from `k` on
/// are zero. It splits into even and odd parts, as the matrix is symmetric: the even part is the transform
/// of half the size on the even inputs.
#[inline(always)]
fn idct_1d(s: &[i32; 32], k: usize, n: usize, out: &mut [i32; 32]) {
    // the 2-point transform of s[0] and s[n / 2]
    let (a, b) = (s[0], s[n / 2]);
    out[0] = 64 * (a + b);
    out[1] = 64 * (a - b);
    let mut len = 2;
    while len < n {
        let l = len * 2;
        let (stride, rowmul, h) = (n / l, 32 / l, len);
        let mut o = [0i32; 16];
        // odd inputs of this level are s[(2m + 1) * stride]
        let mut j = stride;
        while j < k {
            let row = &TRANSFORM[(j / stride) * rowmul];
            let v = s[j];
            if v != 0 {
                for i in 0..h {
                    o[i] += row[i] as i32 * v;
                }
            }
            j += 2 * stride;
        }
        let mut next = [0i32; 32];
        for i in 0..h {
            next[i] = out[i] + o[i];
            next[l - 1 - i] = out[i] - o[i];
        }
        out[..l].copy_from_slice(&next[..l]);
        len = l;
    }
}

fn inverse_fast<const NN: usize>(c: &mut [i32], log2: u32, shift: u32, max_x: usize, max_y: usize) {
    let n = 1usize << log2;
    let rnd = 1i32 << (shift - 1);
    let mut tmp = [0i32; NN];
    let mut s = [0i32; 32];
    let mut o = [0i32; 32];
    for x in 0..=max_x {
        for j in 0..=max_y {
            s[j] = c[j * n + x];
        }
        idct_1d(&s, max_y + 1, n, &mut o);
        for i in 0..n {
            tmp[i * n + x] = ((o[i] + 64) >> 7).clamp(-32768, 32767);
        }
    }
    s = [0; 32];
    for y in 0..n {
        s[..=max_x].copy_from_slice(&tmp[y * n..y * n + max_x + 1]);
        idct_1d(&s, max_x + 1, n, &mut o);
        for i in 0..n {
            c[y * n + i] = (o[i] + rnd) >> shift;
        }
    }
}

/// The inverse transform as plain matrix products (also the reference the fast one is tested against).
fn inverse_matrix(c: &mut [i32], log2: u32, dst: bool, bd: u32, max_x: usize, max_y: usize) {
    let n = 1usize << log2;
    let step = 32 >> log2;
    let m = |j: usize, i: usize| -> i32 {
        if dst {
            DST4[j][i]
        } else {
            TRANSFORM[j * step][i] as i32
        }
    };
    let mut tmp = [0i32; 32 * 32];
    for x in 0..=max_x {
        for i in 0..n {
            let mut s = 0i32;
            for j in 0..=max_y {
                s += m(j, i) * c[j * n + x];
            }
            tmp[i * n + x] = ((s + 64) >> 7).clamp(-32768, 32767);
        }
    }
    let shift = 20 - bd;
    let rnd = 1i32 << (shift - 1);
    for y in 0..n {
        for i in 0..n {
            let mut s = 0i32;
            for x in 0..=max_x {
                s += m(x, i) * tmp[y * n + x];
            }
            c[y * n + i] = (s + rnd) >> shift;
        }
    }
}

/// Transform skip (8.6.4.2): the coefficients are the residual, scaled up and rounded.
pub fn skip(c: &mut [i32], log2: u32, bd: u32) {
    let n = 1usize << log2;
    let shift = 20 - bd;
    let ts = 5 + log2;
    let rnd = 1i32 << (shift - 1);
    for v in c[..n * n].iter_mut() {
        *v = ((*v << ts) + rnd) >> shift;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dc_only_block_is_flat() {
        for log2 in 2..=5u32 {
            let n = 1usize << log2;
            let mut c = vec![0i32; n * n];
            c[0] = 64;
            inverse(&mut c, log2, false, 8, 0, 0);
            assert!(c.iter().all(|&v| v == c[0]), "size {n}");
        }
    }

    #[test]
    fn transform_matrix_rows_match_the_spec() {
        assert_eq!(&TRANSFORM[8][..4], &[83, 36, -36, -83]);
        assert_eq!(&TRANSFORM[1][..4], &[90, 90, 88, 85]);
        assert_eq!(&TRANSFORM[16][..4], &[64, -64, -64, 64]);
    }

    #[test]
    fn flat_scaling_with_qp4_leaves_levels_scaled_by_one() {
        // qp 4: levelScale 64 << 0, factor 16: level * 1024 >> (8 + 2 + 10 - 15) = level * 32
        let mut c = vec![0i32; 16];
        c[0] = 3;
        dequantise(&mut c, 2, 4, 8, None, 0, 0);
        assert_eq!(c[0], 3 * 1024 / 32);
    }

    #[test]
    fn fast_inverse_matches_the_matrix_products() {
        let mut seed = 12345u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed >> 8
        };
        for log2 in 2..=5u32 {
            let n = 1usize << log2;
            for bd in [8u32, 10, 12] {
                for case in 0..200 {
                    let (max_x, max_y) = if case % 4 == 0 { (n - 1, n - 1) } else { (rnd() as usize % n, rnd() as usize % n) };
                    let mut a = vec![0i32; n * n];
                    for y in 0..=max_y {
                        for x in 0..=max_x {
                            let big = case % 3 == 0;
                            a[y * n + x] = if rnd() % 3 == 0 { 0 } else { (rnd() % if big { 65536 } else { 2000 }) as i32 - if big { 32768 } else { 1000 } };
                        }
                    }
                    a[0] = (rnd() % 4000) as i32 - 2000;
                    let mut b = a.clone();
                    inverse(&mut a, log2, false, bd, max_x, max_y);
                    inverse_matrix(&mut b, log2, false, bd, max_x, max_y);
                    assert_eq!(a, b, "size {n}, {bd} bits, case {case}");
                }
            }
        }
    }
}
