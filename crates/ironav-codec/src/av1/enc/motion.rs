//! Open-loop motion on the source: a full-pel motion vector per 8x8 block against the previous
//! picture's source. The partition plan reads it, and the closed-loop search starts from it.

use crate::av1::ipred::Plane;

/// The range of the open-loop search, in whole samples either way.
const RANGE: i32 = 4;

/// The sum of absolute differences of the `w` x `h` block of `cur` at (`x`, `y`) and the block of
/// `prev` displaced by (`dx`, `dy`). Displaced samples are clamped to the plane.
#[allow(clippy::too_many_arguments)]
pub fn sad(cur: &Plane, prev: &Plane, x: usize, y: usize, w: usize, h: usize, dx: i32, dy: i32) -> u64 {
    let (pw, ph) = (prev.stride as i64, (prev.data.len() / prev.stride) as i64);
    let mut s = 0u64;
    for j in 0..h {
        let py = (y as i64 + j as i64 + dy as i64).clamp(0, ph - 1) as usize;
        for i in 0..w {
            let px = (x as i64 + i as i64 + dx as i64).clamp(0, pw - 1) as usize;
            let a = cur.data[(y + j) * cur.stride + x + i] as i64;
            s += (a - prev.data[py * prev.stride + px] as i64).unsigned_abs();
        }
    }
    s
}

/// The full-pel motion (dy, dx) of each 8x8 block of the picture, `rows` x `cols` of them in
/// raster order, found by a search of `RANGE` around zero and around the blocks already searched
/// to the left and above, with a small penalty on length.
pub fn field(cur: &Plane, prev: &Plane, rows: usize, cols: usize) -> Vec<(i32, i32)> {
    let mut out = vec![(0i32, 0i32); rows * cols];
    let pen = |d: (i32, i32)| 2 * (d.0.unsigned_abs() + d.1.unsigned_abs()) as u64;
    for by in 0..rows {
        for bx in 0..cols {
            let (x, y) = (bx * 8, by * 8);
            let mut best = (u64::MAX, (0, 0));
            let try_mv = |d: (i32, i32), best: &mut (u64, (i32, i32))| {
                let c = sad(cur, prev, x, y, 8, 8, d.1, d.0) + pen(d);
                if c < best.0 {
                    *best = (c, d);
                }
            };
            try_mv((0, 0), &mut best);
            if bx > 0 {
                try_mv(out[by * cols + bx - 1], &mut best);
            }
            if by > 0 {
                try_mv(out[(by - 1) * cols + bx], &mut best);
            }
            for dy in -RANGE..=RANGE {
                for dx in -RANGE..=RANGE {
                    try_mv((dy, dx), &mut best);
                }
            }
            out[by * cols + bx] = best.1;
        }
    }
    out
}
