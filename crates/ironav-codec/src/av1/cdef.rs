//! The constrained directional enhancement filter (spec 7.15), applied after deblocking.

use super::block::FrameDec;
use super::ipred::Plane;

const DIV_TABLE: [i64; 9] = [0, 840, 420, 280, 210, 168, 140, 120, 105];
/// Cdef_Uv_Dir by [subsampling_x][subsampling_y] and the luma direction.
const UV_DIR: [[[usize; 8]; 2]; 2] = [[[0, 1, 2, 3, 4, 5, 6, 7], [1, 2, 2, 2, 3, 4, 6, 0]], [[7, 0, 2, 4, 5, 6, 6, 6], [0, 1, 2, 3, 4, 5, 6, 7]]];
const DIRS: [[[i32; 2]; 2]; 8] = [
    [[-1, 1], [-2, 2]],
    [[0, 1], [-1, 2]],
    [[0, 1], [0, 2]],
    [[0, 1], [1, 2]],
    [[1, 1], [2, 2]],
    [[1, 0], [2, 1]],
    [[1, 0], [2, 0]],
    [[1, 0], [2, -1]],
];
const PRI_TAPS: [[i32; 2]; 2] = [[4, 2], [3, 3]];
const SEC_TAPS: [i32; 2] = [2, 1];

fn floor_log2(x: u32) -> i32 {
    31 - x.leading_zeros() as i32
}

fn constrain(diff: i32, thr: i32, damping: i32) -> i32 {
    if thr == 0 {
        return 0;
    }
    let adj = (damping - floor_log2(thr as u32)).max(0);
    let v = diff.abs().min((thr - (diff.abs() >> adj)).max(0));
    if diff < 0 {
        -v
    } else {
        v
    }
}

fn copy(p: &Plane) -> Plane {
    Plane { data: p.data.clone(), stride: p.stride }
}

/// cdef_direction: the dominant edge direction of the 8x8 luma block at mi (`r`, `c`) and its
/// variance.
fn direction(p: &Plane, bd: u32, r: usize, c: usize) -> (usize, i32) {
    let mut cost = [0i64; 8];
    let mut part = [[0i64; 15]; 8];
    let (x0, y0) = (c * 4, r * 4);
    for i in 0..8 {
        for j in 0..8 {
            let x = (p.at(x0 + j, y0 + i) >> (bd - 8)) as i64 - 128;
            part[0][i + j] += x;
            part[1][i + j / 2] += x;
            part[2][i] += x;
            part[3][3 + i - j / 2] += x;
            part[4][7 + i - j] += x;
            part[5][3 - i / 2 + j] += x;
            part[6][j] += x;
            part[7][i / 2 + j] += x;
        }
    }
    for i in 0..8 {
        cost[2] += part[2][i] * part[2][i];
        cost[6] += part[6][i] * part[6][i];
    }
    cost[2] *= DIV_TABLE[8];
    cost[6] *= DIV_TABLE[8];
    for i in 0..7 {
        cost[0] += (part[0][i] * part[0][i] + part[0][14 - i] * part[0][14 - i]) * DIV_TABLE[i + 1];
        cost[4] += (part[4][i] * part[4][i] + part[4][14 - i] * part[4][14 - i]) * DIV_TABLE[i + 1];
    }
    cost[0] += part[0][7] * part[0][7] * DIV_TABLE[8];
    cost[4] += part[4][7] * part[4][7] * DIV_TABLE[8];
    for i in [1, 3, 5, 7] {
        for j in 0..5 {
            cost[i] += part[i][3 + j] * part[i][3 + j];
        }
        cost[i] *= DIV_TABLE[8];
        for j in 0..3 {
            cost[i] += (part[i][j] * part[i][j] + part[i][10 - j] * part[i][10 - j]) * DIV_TABLE[2 * j + 2];
        }
    }
    let (mut best, mut dir) = (0i64, 0usize);
    for (i, &c) in cost.iter().enumerate() {
        if c > best {
            best = c;
            dir = i;
        }
    }
    (dir, ((best - cost[(dir + 4) & 7]) >> 10) as i32)
}

/// Filters one plane's 8x8 (or 4x4 chroma) block at mi (`r`, `c`) from `src` into `dst`.
#[allow(clippy::too_many_arguments)]
fn filter(src: &Plane, dst: &mut Plane, geo: (usize, usize, usize, usize, u32), r: usize, c: usize, str_: (i32, i32, i32, usize)) {
    let (sx, sy, mi_rows, mi_cols, bd) = geo;
    let (pri, sec, damping, dir) = str_;
    let shift = bd - 8;
    let (x0, y0) = ((c * 4) >> sx, (r * 4) >> sy);
    let (w, h) = (8 >> sx, 8 >> sy);
    let (mr, mc) = (mi_rows as isize, mi_cols as isize);
    let avail = |y: isize, x: isize| -> Option<i32> {
        let (cr, cc) = ((y << sy) >> 2, (x << sx) >> 2);
        (cr >= 0 && cr < mr && cc >= 0 && cc < mc).then(|| src.at(x as usize, y as usize))
    };
    let pt = PRI_TAPS[((pri >> shift) & 1) as usize];
    let dirs = [dir, (dir + 6) & 7, (dir + 2) & 7];
    for i in 0..h {
        for j in 0..w {
            let (yy, xx) = ((y0 + i) as isize, (x0 + j) as isize);
            let x = src.at(x0 + j, y0 + i);
            let (mut sum, mut mx, mut mn) = (0i32, x, x);
            for k in 0..2 {
                for sign in [-1i32, 1] {
                    let d = DIRS[dir][k];
                    if let Some(p) = avail(yy + (sign * d[0]) as isize, xx + (sign * d[1]) as isize) {
                        sum += pt[k] * constrain(p - x, pri, damping);
                        mx = mx.max(p);
                        mn = mn.min(p);
                    }
                    for &dd in &dirs[1..] {
                        let d = DIRS[dd][k];
                        if let Some(s) = avail(yy + (sign * d[0]) as isize, xx + (sign * d[1]) as isize) {
                            sum += SEC_TAPS[k] * constrain(s - x, sec, damping);
                            mx = mx.max(s);
                            mn = mn.min(s);
                        }
                    }
                }
            }
            let v = x + ((8 + sum - (sum < 0) as i32) >> 4);
            dst.data[(y0 + i) * dst.stride + x0 + j] = v.clamp(mn, mx) as u16;
        }
    }
}

/// Applies CDEF to the frame's planes (the 64x64 blocks whose cdef_idx was read).
pub fn cdef(f: &mut FrameDec) {
    if f.cdef_idx.iter().all(|&i| i < 0) {
        return;
    }
    let src: Vec<Plane> = f.planes.iter().map(copy).collect();
    let (mi_rows, mi_cols, bd) = (f.h.mi_rows, f.h.mi_cols, f.bd);
    let shift = bd - 8;
    let ss = f.ss;
    let skip = |r: usize, c: usize| f.mi[r * f.mi_stride + c].skip;
    for r in (0..mi_rows).step_by(2) {
        for c in (0..mi_cols).step_by(2) {
            let idx = f.cdef_idx[(r >> 4) * f.cdef_stride + (c >> 4)];
            if idx < 0 || (skip(r, c) && skip(r + 1, c) && skip(r, c + 1) && skip(r + 1, c + 1)) {
                continue;
            }
            let idx = idx as usize;
            let (ydir, var) = direction(&src[0], bd, r, c);
            let h = &f.h;
            let pri = (h.cdef_y_pri[idx] as i32) << shift;
            let sec = (h.cdef_y_sec[idx] as i32) << shift;
            let dir = if pri == 0 { 0 } else { ydir };
            let vs = if var >> 6 != 0 { floor_log2((var >> 6) as u32).min(12) } else { 0 };
            let pri_adj = if var != 0 { (pri * (4 + vs) + 8) >> 4 } else { 0 };
            let damping = h.cdef_damping as i32 + shift as i32;
            filter(&src[0], &mut f.planes[0], (0, 0, mi_rows, mi_cols, bd), r, c, (pri_adj, sec, damping, dir));
            if f.num_planes == 1 {
                continue;
            }
            let h = &f.h;
            let pri = (h.cdef_uv_pri[idx] as i32) << shift;
            let sec = (h.cdef_uv_sec[idx] as i32) << shift;
            let dir = if pri == 0 { 0 } else { UV_DIR[ss[0]][ss[1]][ydir] };
            let geo_c = (ss[0], ss[1], mi_rows, mi_cols, bd);
            for pl in 1..3 {
                filter(&src[pl], &mut f.planes[pl], geo_c, r, c, (pri, sec, damping - 1, dir));
            }
        }
    }
}
