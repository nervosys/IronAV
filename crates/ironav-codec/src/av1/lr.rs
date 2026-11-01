//! Loop restoration (spec 7.17): the Wiener filter and the self-guided filter, applied after CDEF.
//! The filter arithmetic follows rav1e 0.8.1 (src/lrf.rs), Copyright (c) 2018-2023 the rav1e
//! contributors, BSD-2-Clause, which reproduces libaom's output. The stripe rules are the spec's:
//! each 64-row stripe (offset by 8 luma rows) reads CDEF output inside it and the deblocked rows
//! of the frame, at most two rows, above and below it.

use super::block::FrameDec;
use super::ipred::Plane;

pub const RESTORE_NONE: u8 = 0;
pub const RESTORE_WIENER: u8 = 1;
pub const RESTORE_SGRPROJ: u8 = 2;
pub const RESTORE_SWITCHABLE: u8 = 3;

pub const WIENER_TAPS_MIN: [i32; 3] = [-5, -23, -17];
pub const WIENER_TAPS_MAX: [i32; 3] = [10, 8, 46];
pub const WIENER_TAPS_K: [u32; 3] = [1, 2, 3];
pub const WIENER_TAPS_MID: [i32; 3] = [3, -7, 15];
pub const SGRPROJ_XQD_MIN: [i32; 2] = [-96, -32];
pub const SGRPROJ_XQD_MAX: [i32; 2] = [31, 95];
pub const SGRPROJ_XQD_MID: [i32; 2] = [-32, 31];
pub const SGRPROJ_PARAMS_BITS: u32 = 4;
pub const SGRPROJ_PRJ_SUBEXP_K: u32 = 4;

/// Sgr_Params by set: the radius and the epsilon of the radius-2 filter, then of the radius-1 one.
/// A radius of 0 turns that filter off.
pub const SGR_PARAMS: [[u32; 4]; 16] = [
    [2, 140, 1, 3236],
    [2, 112, 1, 2158],
    [2, 93, 1, 1618],
    [2, 80, 1, 1438],
    [2, 70, 1, 1295],
    [2, 58, 1, 1177],
    [2, 47, 1, 1079],
    [2, 37, 1, 996],
    [2, 30, 1, 925],
    [2, 25, 1, 863],
    [0, 0, 1, 2589],
    [0, 0, 1, 1618],
    [0, 0, 1, 1177],
    [0, 0, 1, 925],
    [2, 56, 0, 0],
    [2, 22, 0, 0],
];

/// The restoration of one unit: its type (RESTORE_NONE, _WIENER or _SGRPROJ), the Wiener taps
/// (pass 0 vertical, pass 1 horizontal) and the self-guided set and weights.
#[derive(Clone, Copy, Default)]
pub struct Unit {
    pub typ: u8,
    pub wiener: [[i32; 3]; 2],
    pub set: usize,
    pub xqd: [i32; 2],
}

/// The units of one plane, `cols` by `rows`, in raster order.
#[derive(Clone, Default)]
pub struct Units {
    pub cols: usize,
    pub rows: usize,
    pub u: Vec<Unit>,
}

/// count_units_in_frame: the number of restoration units of `unit` samples across `size` samples.
pub fn count_units(unit: usize, size: usize) -> usize {
    ((size + (unit >> 1)) / unit).max(1)
}

/// Where the filter reads a sample from: the CDEF output inside the current stripe, and the
/// deblocked frame (before CDEF) for at most two rows above and below it.
struct Source<'a> {
    cdef: &'a Plane,
    pre: &'a Plane,
    pw: isize,
    ph: isize,
    /// StripeStartY and StripeEndY of the current stripe, in plane rows.
    ss: isize,
    se: isize,
}

impl Source<'_> {
    /// The plane and the row that get_source_sample (spec 7.17.6) reads for sample row `y`.
    fn row_of(&self, y: isize) -> (&Plane, usize) {
        let y = y.clamp(0, self.ph - 1);
        if y < self.ss {
            (self.pre, y.max(self.ss - 2) as usize)
        } else if y > self.se {
            (self.pre, y.min(self.se + 2) as usize)
        } else {
            (self.cdef, y as usize)
        }
    }
}

/// A rectangle of one plane: `w` by `h` samples from (`x0`, `y0`).
#[derive(Clone, Copy)]
struct Rect {
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
}

/// The source samples of a rectangle with a margin of 3 on every side, row-major, `w + 6` wide.
fn window(src: &Source, r: Rect) -> Vec<i32> {
    let ww = r.w + 6;
    let mut v = vec![0; ww * (r.h + 6)];
    let last = src.pw - 1;
    for (j, row) in v.chunks_exact_mut(ww).enumerate() {
        let (plane, y) = src.row_of((r.y0 + j) as isize - 3);
        let line = &plane.data[y * plane.stride..];
        for (i, s) in row.iter_mut().enumerate() {
            let x = (r.x0 as isize + i as isize - 3).clamp(0, last) as usize;
            *s = line[x] as i32;
        }
    }
    v
}

fn put(out: &mut Plane, x: usize, y: usize, v: i32) {
    out.data[y * out.stride + x] = v as u16;
}

/// The Wiener filter (spec 7.17.4) of one rectangle.
fn wiener(win: &[i32], r: Rect, coef: &[[i32; 3]; 2], bd: u32, out: &mut Plane) {
    let (round_h, round_v) = if bd == 12 { (5, 9) } else { (3, 11) };
    let offset = 1i32 << (bd + 7 - round_h - 1);
    let limit = (1i32 << (bd + 1 + 7 - round_h)) - 1;
    let taps = |c: &[i32; 3]| [c[0], c[1], c[2], 128 - 2 * (c[0] + c[1] + c[2]), c[2], c[1], c[0]];
    let (vf, hf) = (taps(&coef[0]), taps(&coef[1]));
    let ww = r.w + 6;
    // Horizontal pass over every window row: row j is the sample row y0 + j - 3.
    let mut mid = vec![0i32; (r.h + 6) * r.w];
    for j in 0..r.h + 6 {
        for c in 0..r.w {
            let acc: i32 = (0..7).map(|t| hf[t] * win[j * ww + c + t]).sum();
            mid[j * r.w + c] = ((acc + (1 << (round_h - 1))) >> round_h).clamp(-offset, limit - offset);
        }
    }
    // Vertical pass.
    let maxv = (1i32 << bd) - 1;
    for j in 0..r.h {
        for c in 0..r.w {
            let acc: i32 = (0..7).map(|t| vf[t] * mid[(j + t) * r.w + c]).sum();
            put(out, r.x0 + c, r.y0 + j, ((acc + (1 << (round_v - 1))) >> round_v).clamp(0, maxv));
        }
    }
}

/// Summed-area tables of a window and of its squares: entry (y, x) holds the sums over the rows
/// above y and the columns left of x, so any rectangle's sum takes four lookups.
struct Integral {
    w: usize,
    sum: Vec<i64>,
    ssq: Vec<i64>,
}

impl Integral {
    fn new(win: &[i32], ww: usize, hh: usize) -> Self {
        let w1 = ww + 1;
        let mut sum = vec![0i64; w1 * (hh + 1)];
        let mut ssq = vec![0i64; w1 * (hh + 1)];
        for y in 0..hh {
            let (mut rs, mut rq) = (0i64, 0i64);
            for x in 0..ww {
                let p = win[y * ww + x] as i64;
                rs += p;
                rq += p * p;
                sum[(y + 1) * w1 + x + 1] = sum[y * w1 + x + 1] + rs;
                ssq[(y + 1) * w1 + x + 1] = ssq[y * w1 + x + 1] + rq;
            }
        }
        Integral { w: w1, sum, ssq }
    }

    /// The sum and the sum of squares over window rows `y0..=y1` and columns `x0..=x1`.
    fn rect(&self, y0: usize, y1: usize, x0: usize, x1: usize) -> (i64, i64) {
        let (a, b, c, d) = ((y1 + 1) * self.w + x1 + 1, y0 * self.w + x1 + 1, (y1 + 1) * self.w + x0, y0 * self.w + x0);
        (self.sum[a] - self.sum[b] - self.sum[c] + self.sum[d], self.ssq[a] - self.ssq[b] - self.ssq[c] + self.ssq[d])
    }
}

/// The box sums of the self-guided filter (spec 7.17.3) for radius `rad` over the rows -1 to h
/// and columns -1 to w of the rectangle, as the A and B arrays (w + 2 wide). Only rows whose
/// absolute row is odd are computed when `rad` is 2: the even output rows use those and the odd
/// output rows use their own (rav1e's and libaom's radius-2 filter reads every other row).
fn box_ab(ig: &Integral, r: Rect, rad: usize, s: u32, bd: u32) -> (Vec<i64>, Vec<i64>) {
    let (aw, ah) = (r.w + 2, r.h + 2);
    let mut a = vec![0i64; aw * ah];
    let mut b = vec![0i64; aw * ah];
    let d = 2 * rad + 1;
    let n = (d * d) as i64;
    let one_over_n: i64 = if rad == 1 { 455 } else { 164 };
    let shift = 2 * (bd - 8);
    for i in 0..ah {
        let y = r.y0 as isize + i as isize - 1;
        if rad == 2 && y.rem_euclid(2) == 0 {
            continue;
        }
        // Window row and column of the centre sample (j - 1, i - 1) of the box is (i + 2, j + 2).
        let (yc0, yc1) = (i + 2 - rad, i + 2 + rad);
        for j in 0..aw {
            let (sum, ssq) = ig.rect(yc0, yc1, j + 2 - rad, j + 2 + rad);
            let scaled_ssq = (ssq + ((1 << shift) >> 1)) >> shift;
            let scaled_sum = (sum + ((1 << (bd - 8)) >> 1)) >> (bd - 8);
            let p = (scaled_ssq * n - scaled_sum * scaled_sum).max(0);
            let z = (p * s as i64 + (1 << 19)) >> 20;
            let av = if z >= 255 {
                256
            } else if z == 0 {
                1
            } else {
                ((z << 8) + z / 2) / (z + 1)
            };
            let bv = ((256 - av) * sum * one_over_n + (1 << 11)) >> 12;
            a[i * aw + j] = av;
            b[i * aw + j] = bv;
        }
    }
    (a, b)
}

/// The self-guided filter (spec 7.17.3) of one rectangle, with the weights of its unit.
fn self_guided(win: &[i32], r: Rect, u: &Unit, bd: u32, out: &mut Plane) {
    let set = SGR_PARAMS[u.set];
    let (r0, s0, r1, s1) = (set[0] as usize, set[1], set[2] as usize, set[3]);
    let aw = r.w + 2;
    let ww = r.w + 6;
    let ig = Integral::new(win, ww, r.h + 6);
    let (a1, b1) = if r1 != 0 { box_ab(&ig, r, 1, s1, bd) } else { (Vec::new(), Vec::new()) };
    let (a2, b2) = if r0 != 0 { box_ab(&ig, r, 2, s0, bd) } else { (Vec::new(), Vec::new()) };
    // Weights of the two filtered outputs (xq0 for the radius-2 one, xq1 for the radius-1 one).
    let (xq0, xq1) = if r0 == 0 {
        (0i64, (1 << 7) - u.xqd[1] as i64)
    } else if r1 == 0 {
        (u.xqd[0] as i64, 0)
    } else {
        (u.xqd[0] as i64, (1 << 7) - u.xqd[0] as i64 - u.xqd[1] as i64)
    };
    let maxv = (1i64 << bd) - 1;
    for j in 0..r.h {
        let yabs = r.y0 + j;
        for c in 0..r.w {
            let p = win[(j + 3) * ww + c + 3] as i64;
            let ui = (j + 1) * aw + c + 1;
            let mut v = p << (4 + 7);
            let ur = p << 4;
            if r0 != 0 {
                let f = if yabs.is_multiple_of(2) {
                    let up = (j) * aw + c + 1;
                    let dn = (j + 2) * aw + c + 1;
                    let (ac, bc) = (
                        5 * (a2[up - 1] + a2[up + 1] + a2[dn - 1] + a2[dn + 1]) + 6 * (a2[up] + a2[dn]),
                        5 * (b2[up - 1] + b2[up + 1] + b2[dn - 1] + b2[dn + 1]) + 6 * (b2[up] + b2[dn]),
                    );
                    (ac * p + bc + (1 << 8)) >> 9
                } else {
                    let (ac, bc) = (5 * (a2[ui - 1] + a2[ui + 1]) + 6 * a2[ui], 5 * (b2[ui - 1] + b2[ui + 1]) + 6 * b2[ui]);
                    (ac * p + bc + (1 << 7)) >> 8
                };
                v += xq0 * (f - ur);
            }
            if r1 != 0 {
                let up = j * aw + c + 1;
                let dn = (j + 2) * aw + c + 1;
                let ac = 3 * (a1[up - 1] + a1[up + 1] + a1[dn - 1] + a1[dn + 1]) + 4 * (a1[up] + a1[dn] + a1[ui - 1] + a1[ui + 1] + a1[ui]);
                let bc = 3 * (b1[up - 1] + b1[up + 1] + b1[dn - 1] + b1[dn + 1]) + 4 * (b1[up] + b1[dn] + b1[ui - 1] + b1[ui + 1] + b1[ui]);
                let f = (ac * p + bc + (1 << 8)) >> 9;
                v += xq1 * (f - ur);
            }
            let out_v = ((v + (1 << 10)) >> 11).clamp(0, maxv);
            put(out, r.x0 + c, yabs, out_v as i32);
        }
    }
}

/// Applies loop restoration to the planes of `f` (which hold the CDEF output); `pre` holds the
/// deblocked planes from before CDEF, which the stripe boundaries read.
pub fn loop_restore(f: &mut FrameDec, pre: &[Plane; 3]) {
    let bd = f.bd;
    let mut out = f.planes.clone();
    for plane in 0..f.num_planes {
        if f.h.lr_type[plane] == RESTORE_NONE {
            continue;
        }
        let (sx, sy) = if plane == 0 { (0, 0) } else { (f.ss[0], f.ss[1]) };
        let pw = (f.h.upscaled_width as usize + sx) >> sx;
        let ph = (f.h.height as usize + sy) >> sy;
        let unit = f.h.lr_size[plane] as usize;
        let lr = &f.lr[plane];
        let mut src = Source { cdef: &f.planes[plane], pre: &pre[plane], pw: pw as isize, ph: ph as isize, ss: 0, se: 0 };
        for m in 0.. {
            // StripeStartY is 64 luma rows (offset by 8) per stripe; the first stripe starts above the frame.
            let ss = (64 * m as isize - 8) >> sy;
            if ss >= ph as isize {
                break;
            }
            src.ss = ss;
            src.se = ss + (64 >> sy) as isize - 1;
            let y0 = ss.max(0) as usize;
            let y1 = (src.se.min(ph as isize - 1)) as usize;
            // Stripes never straddle a unit row (unit rows are offset by 8 luma rows too).
            let ur = (lr.rows - 1).min((y0 + (8 >> sy)) / unit);
            for uc in 0..lr.cols {
                let x0 = uc * unit;
                let x1 = if uc + 1 == lr.cols { pw } else { (uc + 1) * unit };
                if x0 >= x1 {
                    continue;
                }
                let u = lr.u[ur * lr.cols + uc];
                if u.typ == RESTORE_NONE {
                    continue;
                }
                let r = Rect { x0, y0, w: x1 - x0, h: y1 + 1 - y0 };
                let win = window(&src, r);
                match u.typ {
                    RESTORE_WIENER => wiener(&win, r, &u.wiener, bd, &mut out[plane]),
                    _ => self_guided(&win, r, &u, bd, &mut out[plane]),
                }
            }
        }
    }
    f.planes = out;
}
