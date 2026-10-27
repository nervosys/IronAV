//! Scaling (dequantisation) and the inverse transforms of HEVC (ITU-T H.265 8.6).

use super::ps::ScalingList;
use super::tables::{DIAG_SCAN4X4_X, DIAG_SCAN4X4_Y, DIAG_SCAN8X8_X, DIAG_SCAN8X8_Y, TRANSFORM};

pub const LEVEL_SCALE: [i32; 6] = [40, 45, 51, 57, 64, 72];

const DST4: [[i32; 4]; 4] = [[29, 55, 74, 84], [74, 74, 0, -74], [84, -29, -74, 55], [55, -84, 74, -29]];

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
}
