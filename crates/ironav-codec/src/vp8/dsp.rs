//! VP8 inverse transforms, sub-pixel motion compensation and loop filters.

fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

fn mul20091(a: i32) -> i32 {
    ((a * 20091) >> 16) + a
}

fn mul35468(a: i32) -> i32 {
    (a * 35468) >> 16
}

/// Inverse Walsh-Hadamard transform of the Y2 block into the DC of the sixteen luma blocks.
pub fn luma_dc_wht(blocks: &mut [[i16; 16]], dc: &[i16; 16]) {
    let mut dc = *dc;
    for i in 0..4 {
        let t0 = dc[i] as i32 + dc[12 + i] as i32;
        let t1 = dc[4 + i] as i32 + dc[8 + i] as i32;
        let t2 = dc[4 + i] as i32 - dc[8 + i] as i32;
        let t3 = dc[i] as i32 - dc[12 + i] as i32;
        dc[i] = (t0 + t1) as i16;
        dc[4 + i] = (t3 + t2) as i16;
        dc[8 + i] = (t0 - t1) as i16;
        dc[12 + i] = (t3 - t2) as i16;
    }
    for i in 0..4 {
        let t0 = dc[i * 4] as i32 + dc[i * 4 + 3] as i32 + 3;
        let t1 = dc[i * 4 + 1] as i32 + dc[i * 4 + 2] as i32;
        let t2 = dc[i * 4 + 1] as i32 - dc[i * 4 + 2] as i32;
        let t3 = dc[i * 4] as i32 - dc[i * 4 + 3] as i32 + 3;
        blocks[i * 4][0] = ((t0 + t1) >> 3) as i16;
        blocks[i * 4 + 1][0] = ((t3 + t2) >> 3) as i16;
        blocks[i * 4 + 2][0] = ((t0 - t1) >> 3) as i16;
        blocks[i * 4 + 3][0] = ((t3 - t2) >> 3) as i16;
    }
}

/// Inverse DCT of one 4x4 block, added to the prediction already in `dst`.
pub fn idct_add(dst: &mut [u8], off: usize, stride: usize, b: &[i16; 16]) {
    let mut tmp = [0i16; 16];
    for i in 0..4 {
        let t0 = b[i] as i32 + b[8 + i] as i32;
        let t1 = b[i] as i32 - b[8 + i] as i32;
        let t2 = mul35468(b[4 + i] as i32) - mul20091(b[12 + i] as i32);
        let t3 = mul20091(b[4 + i] as i32) + mul35468(b[12 + i] as i32);
        tmp[i * 4] = (t0 + t3) as i16;
        tmp[i * 4 + 1] = (t1 + t2) as i16;
        tmp[i * 4 + 2] = (t1 - t2) as i16;
        tmp[i * 4 + 3] = (t0 - t3) as i16;
    }
    for i in 0..4 {
        let t0 = tmp[i] as i32 + tmp[8 + i] as i32;
        let t1 = tmp[i] as i32 - tmp[8 + i] as i32;
        let t2 = mul35468(tmp[4 + i] as i32) - mul20091(tmp[12 + i] as i32);
        let t3 = mul20091(tmp[4 + i] as i32) + mul35468(tmp[12 + i] as i32);
        let row = off + i * stride;
        dst[row] = clip_u8(dst[row] as i32 + ((t0 + t3 + 4) >> 3));
        dst[row + 1] = clip_u8(dst[row + 1] as i32 + ((t1 + t2 + 4) >> 3));
        dst[row + 2] = clip_u8(dst[row + 2] as i32 + ((t1 - t2 + 4) >> 3));
        dst[row + 3] = clip_u8(dst[row + 3] as i32 + ((t0 - t3 + 4) >> 3));
    }
}

const SUBPEL: [[i32; 6]; 7] = [
    [0, 6, 123, 12, 1, 0],
    [2, 11, 108, 36, 8, 1],
    [0, 9, 93, 50, 6, 0],
    [3, 16, 77, 77, 16, 3],
    [0, 6, 50, 93, 9, 0],
    [1, 8, 36, 108, 11, 2],
    [0, 1, 12, 123, 6, 0],
];

fn tap6(f: &[i32; 6], s: [i32; 6]) -> u8 {
    // taps apply to pixels -2..=3; the sign pattern is built into the filter definition
    clip_u8((f[2] * s[2] - f[1] * s[1] + f[0] * s[0] + f[3] * s[3] - f[4] * s[4] + f[5] * s[5] + 64) >> 7)
}

/// A reference plane for motion compensation. Reads outside it clamp to the nearest edge pixel.
pub struct RefPlane<'a> {
    pub data: &'a [u8],
    pub stride: usize,
    pub w: usize,
    pub h: usize,
}

impl RefPlane<'_> {
    fn at(&self, x: i64, y: i64) -> i32 {
        let x = x.clamp(0, self.w as i64 - 1) as usize;
        let y = y.clamp(0, self.h as i64 - 1) as usize;
        self.data[y * self.stride + x] as i32
    }
}

/// Predicts a `bw` x `bh` block whose top-left source pixel is (`x`, `y`) plus a sub-pixel offset of
/// `mx`/`my` eighths, into `dst` at `off`.
#[allow(clippy::too_many_arguments)]
pub fn mc(dst: &mut [u8], off: usize, dst_stride: usize, r: &RefPlane, x: i64, y: i64, mx: usize, my: usize, bw: usize, bh: usize, bilinear: bool) {
    if bilinear {
        let (a, b) = (8 - mx as i32, mx as i32);
        let (c, d) = (8 - my as i32, my as i32);
        // first pass over bh + 1 rows so the vertical pass has its lower neighbour
        let mut tmp = [0i32; 17 * 16];
        for j in 0..=bh {
            for i in 0..bw {
                let p0 = r.at(x + i as i64, y + j as i64);
                let p1 = if mx != 0 { r.at(x + i as i64 + 1, y + j as i64) } else { p0 };
                tmp[j * 16 + i] = (a * p0 + b * p1 + 4) >> 3;
            }
        }
        for j in 0..bh {
            for i in 0..bw {
                let v = if my != 0 { (c * tmp[j * 16 + i] + d * tmp[(j + 1) * 16 + i] + 4) >> 3 } else { tmp[j * 16 + i] };
                dst[off + j * dst_stride + i] = v as u8;
            }
        }
        return;
    }
    if mx == 0 && my == 0 {
        for j in 0..bh {
            for i in 0..bw {
                dst[off + j * dst_stride + i] = r.at(x + i as i64, y + j as i64) as u8;
            }
        }
        return;
    }
    // horizontal pass on bh + 5 rows (two above, three below), then the vertical pass
    let mut tmp = [0u8; 21 * 16];
    let (y0, rows) = if my != 0 { (-2i64, bh + 5) } else { (0, bh) };
    for j in 0..rows {
        let yy = y + y0 + j as i64;
        for i in 0..bw {
            let xx = x + i as i64;
            tmp[j * 16 + i] = if mx != 0 {
                let f = &SUBPEL[mx - 1];
                tap6(f, [r.at(xx - 2, yy), r.at(xx - 1, yy), r.at(xx, yy), r.at(xx + 1, yy), r.at(xx + 2, yy), r.at(xx + 3, yy)])
            } else {
                r.at(xx, yy) as u8
            };
        }
    }
    for j in 0..bh {
        for i in 0..bw {
            dst[off + j * dst_stride + i] = if my != 0 {
                let f = &SUBPEL[my - 1];
                let s = |k: usize| tmp[(j + k) * 16 + i] as i32;
                tap6(f, [s(0), s(1), s(2), s(3), s(4), s(5)])
            } else {
                tmp[j * 16 + i]
            };
        }
    }
}

fn clip_i8(n: i32) -> i32 {
    n.clamp(-128, 127)
}

/// Which pixel line of an edge is being filtered: `pos` is the first pixel after the edge and
/// `step` moves across it.
struct Line<'a> {
    buf: &'a mut [u8],
    pos: usize,
    step: usize,
}

impl Line<'_> {
    fn get(&self, k: i32) -> i32 {
        self.buf[(self.pos as isize + k as isize * self.step as isize) as usize] as i32
    }

    fn set(&mut self, k: i32, v: i32) {
        let i = (self.pos as isize + k as isize * self.step as isize) as usize;
        self.buf[i] = clip_u8(v);
    }

    fn simple_limit(&self, flim: i32) -> bool {
        2 * (self.get(-1) - self.get(0)).abs() + ((self.get(-2) - self.get(1)).abs() >> 1) <= flim
    }

    fn normal_limit(&self, e: i32, i: i32) -> bool {
        self.simple_limit(e)
            && (self.get(-4) - self.get(-3)).abs() <= i
            && (self.get(-3) - self.get(-2)).abs() <= i
            && (self.get(-2) - self.get(-1)).abs() <= i
            && (self.get(3) - self.get(2)).abs() <= i
            && (self.get(2) - self.get(1)).abs() <= i
            && (self.get(1) - self.get(0)).abs() <= i
    }

    fn hev(&self, thresh: i32) -> bool {
        (self.get(-2) - self.get(-1)).abs() > thresh || (self.get(1) - self.get(0)).abs() > thresh
    }

    fn filter_common(&mut self, is4tap: bool) {
        let (p1, p0, q0, q1) = (self.get(-2), self.get(-1), self.get(0), self.get(1));
        let mut a = 3 * (q0 - p0);
        if is4tap {
            a += clip_i8(p1 - q1);
        }
        let a = clip_i8(a);
        let f1 = (a + 4).min(127) >> 3;
        let f2 = (a + 3).min(127) >> 3;
        self.set(-1, p0 + f2);
        self.set(0, q0 - f1);
        if !is4tap {
            let a = (f1 + 1) >> 1;
            self.set(-2, p1 + a);
            self.set(1, q1 - a);
        }
    }

    fn filter_mbedge(&mut self) {
        let (p2, p1, p0, q0, q1, q2) = (self.get(-3), self.get(-2), self.get(-1), self.get(0), self.get(1), self.get(2));
        let w = clip_i8(clip_i8(p1 - q1) + 3 * (q0 - p0));
        let a0 = (27 * w + 63) >> 7;
        let a1 = (18 * w + 63) >> 7;
        let a2 = (9 * w + 63) >> 7;
        self.set(-3, p2 + a2);
        self.set(-2, p1 + a1);
        self.set(-1, p0 + a0);
        self.set(0, q0 - a0);
        self.set(1, q1 - a1);
        self.set(2, q2 - a2);
    }
}

/// Filters `n` lines along an edge. `across` is the step perpendicular to the edge and `along` the
/// step between lines. `mb_edge` selects the stronger macroblock-edge filter.
#[allow(clippy::too_many_arguments)]
pub fn loop_filter(buf: &mut [u8], pos: usize, across: usize, along: usize, n: usize, e: i32, i: i32, hev_thresh: i32, mb_edge: bool) {
    for k in 0..n {
        let mut l = Line { buf: &mut *buf, pos: pos + k * along, step: across };
        if !l.normal_limit(e, i) {
            continue;
        }
        if l.hev(hev_thresh) {
            l.filter_common(true);
        } else if mb_edge {
            l.filter_mbedge();
        } else {
            l.filter_common(false);
        }
    }
}

/// The simple loop filter touches luma only and has no interior or high-variance tests.
pub fn loop_filter_simple(buf: &mut [u8], pos: usize, across: usize, along: usize, flim: i32) {
    for k in 0..16 {
        let mut l = Line { buf: &mut *buf, pos: pos + k * along, step: across };
        if l.simple_limit(flim) {
            l.filter_common(true);
        }
    }
}
