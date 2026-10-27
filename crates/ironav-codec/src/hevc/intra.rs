//! HEVC intra sample prediction (ITU-T H.265 8.4.4.2).
//!
//! The neighbouring samples arrive as one line `p` of `4n + 1` samples running from the bottom of the
//! left column up to the corner and then along the top row to the right:
//! `p[2n - 1 - y]` is the left neighbour of row `y`, `p[2n]` the corner and `p[2n + 1 + x]` the top
//! neighbour of column `x`.

const ANGLE: [i32; 35] = [0, 0, 32, 26, 21, 17, 13, 9, 5, 2, 0, -2, -5, -9, -13, -17, -21, -26, -32, -26, -21, -17, -13, -9, -5, -2, 0, 2, 5, 9, 13, 17, 21, 26, 32];
/// Inverse angles for modes 11..=25.
const INV_ANGLE: [i32; 15] = [-4096, -1638, -910, -630, -482, -390, -315, -256, -315, -390, -482, -630, -910, -1638, -4096];

/// Fills in the unavailable entries of `p` (8.4.4.2.2). `avail[i]` says whether `p[i]` was reconstructed.
pub fn substitute(p: &mut [u16], avail: &[bool], bd: u32) {
    let Some(first) = avail.iter().position(|&a| a) else {
        p.fill(1 << (bd - 1));
        return;
    };
    let v = p[first];
    p[..first].fill(v);
    for i in first + 1..p.len() {
        if !avail[i] {
            p[i] = p[i - 1];
        }
    }
}

/// Predicts an `n` x `n` block (`n = 1 << log2`) into `out` (row-major).
///
/// `filter` allows reference smoothing (luma, or chroma in 4:4:4), `edges` the DC and the pure
/// horizontal/vertical boundary smoothing (luma only), `strong` the strong 32x32 smoothing.
pub fn predict(p: &[u16], log2: u32, mode: u32, filter: bool, edges: bool, strong: bool, bd: u32, out: &mut [u16]) {
    let n = 1usize << log2;
    debug_assert_eq!(p.len(), 4 * n + 1);
    let mut f = [0i32; 129];
    for (d, &s) in f.iter_mut().zip(p) {
        *d = s as i32;
    }
    let len = 4 * n + 1;
    if filter && mode != 1 && n != 4 {
        let dist = (mode as i32 - 26).abs().min((mode as i32 - 10).abs());
        let thres = match n {
            8 => 7,
            16 => 1,
            _ => 0,
        };
        if dist > thres {
            let (corner, bl, tr) = (f[2 * n], f[0], f[4 * n]);
            if strong && n == 32 && (corner + tr - 2 * f[2 * n + n]).abs() < (1 << (bd - 5)) && (corner + bl - 2 * f[n]).abs() < (1 << (bd - 5)) {
                // bi-linear interpolation between the corners
                let mut g = [0i32; 129];
                g[0] = bl;
                g[2 * n] = corner;
                g[4 * n] = tr;
                for i in 1..64 {
                    // left column, from the bottom: p[-1][63 - i]... indexed so that i counts up from the corner
                    let y = i as i32;
                    g[2 * n - i] = ((64 - y) * corner + y * f[0] + 32) >> 6;
                    g[2 * n + i] = ((64 - y) * corner + y * tr + 32) >> 6;
                }
                f = g;
            } else {
                let mut g = f;
                for i in 1..len - 1 {
                    g[i] = (f[i - 1] + 2 * f[i] + f[i + 1] + 2) >> 2;
                }
                f = g;
            }
        }
    }
    let left = |y: i32| f[(2 * n as i32 - 1 - y) as usize];
    let top = |x: i32| f[(2 * n as i32 + 1 + x) as usize];
    let maxv = (1i32 << bd) - 1;
    let ni = n as i32;
    match mode {
        0 => {
            for y in 0..ni {
                for x in 0..ni {
                    let v = ((ni - 1 - x) * left(y) + (x + 1) * top(ni) + (ni - 1 - y) * top(x) + (y + 1) * left(ni) + ni) >> (log2 + 1);
                    out[(y * ni + x) as usize] = v as u16;
                }
            }
        }
        1 => {
            let mut sum = ni;
            for i in 0..ni {
                sum += left(i) + top(i);
            }
            let dc = sum >> (log2 + 1);
            out[..n * n].fill(dc as u16);
            if edges && n < 32 {
                out[0] = ((left(0) + 2 * dc + top(0) + 2) >> 2) as u16;
                for x in 1..ni {
                    out[x as usize] = ((top(x) + 3 * dc + 2) >> 2) as u16;
                }
                for y in 1..ni {
                    out[(y * ni) as usize] = ((left(y) + 3 * dc + 2) >> 2) as u16;
                }
            }
        }
        _ => {
            let angle = ANGLE[mode as usize];
            let vertical = mode >= 18;
            // main reference line indexed from -n..=2n, side line for the other direction
            let mut r = [0i32; 3 * 64 + 2];
            let o = 64usize; // r[o + k] is ref[k]
            let (main, side): (&dyn Fn(i32) -> i32, &dyn Fn(i32) -> i32) = if vertical { (&top, &left) } else { (&left, &top) };
            for k in 0..=ni {
                r[o + k as usize] = if k == 0 { f[2 * n] } else { main(k - 1) };
            }
            if angle < 0 {
                let last = (ni * angle) >> 5;
                if last < -1 {
                    let inv = INV_ANGLE[mode as usize - 11];
                    for k in last..=-1 {
                        // ref[k] = side[-1 + ((k * inv + 128) >> 8)]
                        let idx = -1 + ((k * inv + 128) >> 8);
                        r[(o as i32 + k) as usize] = if idx < 0 { f[2 * n] } else { side(idx) };
                    }
                }
            } else {
                for k in ni + 1..=2 * ni {
                    r[o + k as usize] = main(k - 1);
                }
            }
            for a in 0..ni {
                // a runs along the prediction direction (y for vertical modes), b across it
                let idx = ((a + 1) * angle) >> 5;
                let fact = ((a + 1) * angle) & 31;
                for b in 0..ni {
                    let base = (o as i32 + b + idx + 1) as usize;
                    let v = if fact != 0 { ((32 - fact) * r[base] + fact * r[base + 1] + 16) >> 5 } else { r[base] };
                    let (x, y) = if vertical { (b, a) } else { (a, b) };
                    out[(y * ni + x) as usize] = v as u16;
                }
            }
            if edges && n < 32 {
                if mode == 26 {
                    for y in 0..ni {
                        out[(y * ni) as usize] = (top(0) + ((left(y) - f[2 * n]) >> 1)).clamp(0, maxv) as u16;
                    }
                } else if mode == 10 {
                    for x in 0..ni {
                        out[x as usize] = (left(0) + ((top(x) - f[2 * n]) >> 1)).clamp(0, maxv) as u16;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(n: usize, f: impl Fn(usize) -> u16) -> Vec<u16> {
        (0..4 * n + 1).map(f).collect()
    }

    #[test]
    fn flat_neighbours_predict_flat_blocks_in_every_mode() {
        for log2 in 2..=5 {
            let n = 1 << log2;
            let p = line(n, |_| 77);
            for mode in 0..35 {
                let mut out = vec![0u16; n * n];
                predict(&p, log2, mode, true, true, true, 8, &mut out);
                assert!(out.iter().all(|&v| v == 77), "mode {mode} size {n}");
            }
        }
    }

    #[test]
    fn substitution_copies_the_nearest_available_sample() {
        let mut p = [0u16, 0, 5, 0, 9];
        substitute(&mut p, &[false, false, true, false, true], 8);
        assert_eq!(p, [5, 5, 5, 5, 9]);
        let mut q = [1u16; 5];
        substitute(&mut q, &[false; 5], 10);
        assert_eq!(q, [512; 5]);
    }

    #[test]
    fn vertical_mode_copies_the_row_above() {
        let n = 8;
        let p = line(n, |i| if i > 2 * n { (10 + i - 2 * n) as u16 } else { 50 });
        let mut out = vec![0u16; n * n];
        predict(&p, 3, 26, false, false, false, 8, &mut out);
        for y in 0..n {
            for x in 0..n {
                assert_eq!(out[y * n + x], (11 + x) as u16);
            }
        }
    }
}
