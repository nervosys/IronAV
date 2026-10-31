//! Prediction and residual reconstruction of one block.

use super::dec::*;
use super::intrapred;
use super::itx;
use super::tables::*;

const VERT_PRED: u8 = 0;
const HOR_PRED: u8 = 1;
const DC_PRED_: u8 = 2;
const DIAG_DOWN_LEFT_PRED: u8 = 3;
const DIAG_DOWN_RIGHT_PRED: u8 = 4;
const VERT_RIGHT_PRED: u8 = 5;
const HOR_DOWN_PRED: u8 = 6;
const VERT_LEFT_PRED: u8 = 7;
const HOR_UP_PRED: u8 = 8;
const TM_VP8_PRED: u8 = 9;
const LEFT_DC_PRED: u8 = 10;
const TOP_DC_PRED: u8 = 11;

/// (needs_left, needs_top, needs_topleft, needs_topright) per prediction mode (0..=14).
fn edge_needs(mode: u8) -> (bool, bool, bool, bool) {
    match mode {
        VERT_PRED => (false, true, false, false),
        HOR_PRED => (true, false, false, false),
        DC_PRED_ => (true, true, false, false),
        DIAG_DOWN_LEFT_PRED => (false, true, false, true),
        DIAG_DOWN_RIGHT_PRED | VERT_RIGHT_PRED | HOR_DOWN_PRED | TM_VP8_PRED => (true, true, true, false),
        VERT_LEFT_PRED => (false, true, false, true),
        HOR_UP_PRED => (true, false, false, false),
        LEFT_DC_PRED => (true, false, false, false),
        TOP_DC_PRED => (false, true, false, false),
        _ => (false, false, false, false),
    }
}

fn rdiv(a: i32, b: i32) -> i32 {
    (if a >= 0 { a + (b >> 1) } else { a - (b >> 1) }) / b
}

fn mv2(a: Mv, b: Mv) -> Mv {
    Mv { x: rdiv(a.x as i32 + b.x as i32, 2) as i16, y: rdiv(a.y as i32 + b.y as i32, 2) as i16 }
}

fn mv4(a: Mv, b: Mv, c: Mv, d: Mv) -> Mv {
    Mv {
        x: rdiv(a.x as i32 + b.x as i32 + c.x as i32 + d.x as i32, 4) as i16,
        y: rdiv(a.y as i32 + b.y as i32 + c.y as i32 + d.y as i32, 4) as i16,
    }
}

impl Dec {
    /// Builds the prediction edges for a transform block at 4x4 unit (x4, y4) of the block and
    /// returns the mode to use after fallbacks for missing neighbours.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn intra_edges(&self, pl: usize, mode: u8, px: usize, py: usize, tx: usize, x4: usize, y4: usize, w4: usize, top: &mut [u16; 65], left: &mut [u16; 32]) -> u8 {
        let (ss_h, ss_v) = if pl == 0 { (0, 0) } else { (self.ss_h, self.ss_v) };
        let have_top = self.row > 0 || y4 > 0;
        let have_left = self.col > self.tile_col_start || x4 > 0;
        let have_right = x4 + 1 < w4;
        let bpp = self.bpp;
        let mode = MODE_CONV[mode as usize][have_left as usize][have_top as usize];
        let (needs_left, needs_top, needs_topleft, needs_topright) = edge_needs(mode);
        let n = 4usize << tx;
        let plane = &self.cur.planes[pl];
        let stride = self.cur.stride[pl];
        let base = 128u16 << (bpp - 8);

        if needs_top {
            let n_have = (((self.cols - self.col) << (1 - ss_h)) - x4) * 4;
            let need_tr = if tx == 0 && needs_topright && have_right { 4 } else { 0 };
            if have_top {
                let o = (py - 1) * stride + px;
                if n <= n_have {
                    top[1..=n].copy_from_slice(&plane[o..o + n]);
                } else {
                    top[1..=n_have].copy_from_slice(&plane[o..o + n_have]);
                    let v = plane[o + n_have - 1];
                    for t in &mut top[n_have + 1..=n] {
                        *t = v;
                    }
                }
            } else {
                for t in &mut top[1..=n] {
                    *t = base - 1;
                }
            }
            if needs_topleft {
                top[0] = if have_left && have_top { plane[(py - 1) * stride + px - 1] } else if have_top { base + 1 } else { base - 1 };
            }
            if tx == 0 && needs_topright {
                if have_top && have_right && n + need_tr <= n_have {
                    let o = (py - 1) * stride + px;
                    top[5..9].copy_from_slice(&plane[o + 4..o + 8]);
                } else {
                    let v = top[4];
                    for t in &mut top[5..9] {
                        *t = v;
                    }
                }
            } else if needs_topright {
                let v = top[n];
                for t in &mut top[n + 1..=2 * n] {
                    *t = v;
                }
            }
        }
        if needs_left {
            if have_left {
                let n_have = (((self.rows - self.row) << (1 - ss_v)) - y4) * 4;
                let cnt = n.min(n_have);
                for i in 0..cnt {
                    left[i] = plane[(py + i) * stride + px - 1];
                }
                let v = left[cnt - 1];
                for l in &mut left[cnt..n] {
                    *l = v;
                }
            } else {
                for l in &mut left[..n] {
                    *l = base + 1;
                }
            }
        }
        mode
    }

    fn add_residual(&mut self, pl: usize, tx: usize, txtp: usize, n: usize, eob: usize, px: usize, py: usize) {
        let stride = self.cur.stride[pl];
        let off = py * stride + px;
        let lossless = self.h.lossless;
        let bpp = self.bpp;
        let coefs = if pl == 0 { &mut self.block[16 * n..] } else { &mut self.uvblock[pl - 1][16 * n..] };
        let dst = &mut self.cur.planes[pl][off..];
        if lossless {
            itx::iwht_add(coefs, dst, stride, eob, bpp);
        } else {
            itx::inverse_add(tx, txtp, coefs, dst, stride, eob, bpp);
        }
    }

    pub fn intra_recon(&mut self) {
        let (row, col) = (self.row, self.col);
        let b = self.b;
        let bs = b.bs;
        let mut w4 = (BWH_TAB[1][bs][0] as usize) << 1;
        let h4 = (BWH_TAB[1][bs][1] as usize) << 1;
        let step1d = 1usize << b.tx;
        let step = 1usize << (b.tx * 2);
        let mut end_x = (2 * (self.cols - col)).min(w4);
        let mut end_y = (2 * (self.rows - row)).min(h4);
        let mut top = [0u16; 65];
        let mut left = [0u16; 32];

        let mut n = 0;
        let mut y = 0;
        while y < end_y {
            let mut x = 0;
            while x < end_x {
                let mode0 = b.mode[if bs > BS_8X8 && b.tx == 0 { y * 2 + x } else { 0 }];
                let txtp = INTRA_TXFM_TYPE[mode0 as usize] as usize;
                let eob = if b.skip { 0 } else { self.eob[n] as usize };
                let (px, py) = (col * 8 + x * 4, row * 8 + y * 4);
                let mode = self.intra_edges(0, mode0, px, py, b.tx, x, y, w4, &mut top, &mut left);
                let stride = self.cur.stride[0];
                let bpp = self.bpp;
                intrapred::predict(mode, b.tx, &mut self.cur.planes[0][py * stride + px..], stride, &top, &left, bpp);
                if eob > 0 {
                    self.add_residual(0, b.tx, txtp, n, eob, px, py);
                }
                x += step1d;
                n += step;
            }
            y += step1d;
        }

        w4 >>= self.ss_h;
        end_x >>= self.ss_h;
        end_y >>= self.ss_v;
        let uvstep1d = 1usize << b.uvtx;
        let step = 1usize << (b.uvtx * 2);
        for p in 0..2 {
            let mut n = 0;
            let mut y = 0;
            while y < end_y {
                let mut x = 0;
                while x < end_x {
                    let eob = if b.skip { 0 } else { self.uveob[p][n] as usize };
                    let px = ((col * 8) >> self.ss_h) + x * 4;
                    let py = ((row * 8) >> self.ss_v) + y * 4;
                    let mode = self.intra_edges(1 + p, b.uvmode, px, py, b.uvtx, x, y, w4, &mut top, &mut left);
                    let stride = self.cur.stride[1 + p];
                    let bpp = self.bpp;
                    intrapred::predict(mode, b.uvtx, &mut self.cur.planes[1 + p][py * stride + px..], stride, &top, &left, bpp);
                    if eob > 0 {
                        self.add_residual(1 + p, b.uvtx, 0, n, eob, px, py);
                    }
                    x += uvstep1d;
                    n += step;
                }
                y += uvstep1d;
            }
        }
    }

    /// Motion compensated prediction of a bw x bh block at (x, y) of plane `pl`, with the motion
    /// vector given in 1/16 sample units of that plane. `w`/`h` are the reference plane's size.
    #[allow(clippy::too_many_arguments)]
    fn mc_block(&mut self, pl: usize, r: &Pic, x: usize, y: usize, mvx: i32, mvy: i32, bw: usize, bh: usize, w: i32, h: i32, filt: usize, avg: bool) {
        let fx = x as i32 * 16 + mvx;
        let fy = y as i32 * 16 + mvy;
        let (ix, iy) = (fx >> 4, fy >> 4);
        let (mx, my) = ((fx & 15) as usize, (fy & 15) as usize);
        let src = &r.planes[pl];
        let ss = r.stride[pl];
        let tw = bw + 7;
        let th = bh + 7;
        let mut tmp = [0u16; 71 * 71];
        let inside = ix >= 3 && iy >= 3 && ix + bw as i32 + 4 <= w && iy + bh as i32 + 4 <= h;
        for j in 0..th {
            let yy = (iy - 3 + j as i32).clamp(0, h - 1) as usize;
            let row = &src[yy * ss..];
            let t = &mut tmp[j * tw..j * tw + tw];
            if inside {
                let o = (ix - 3) as usize;
                t.copy_from_slice(&row[o..o + tw]);
            } else {
                for (i, v) in t.iter_mut().enumerate() {
                    *v = row[(ix - 3 + i as i32).clamp(0, w - 1) as usize];
                }
            }
        }
        let maxv = (1i32 << self.bpp) - 1;
        let taps = |m: usize| -> [i32; 8] {
            if filt == 3 {
                let mut t = [0i32; 8];
                t[3] = 128 - 8 * m as i32;
                t[4] = 8 * m as i32;
                t
            } else {
                let mut t = [0i32; 8];
                for k in 0..8 {
                    t[k] = SUBPEL_FILTERS[filt][m][k] as i32;
                }
                t
            }
        };
        // horizontal pass
        let mut mid = [0u16; 71 * 64];
        let (r0, r1) = if my != 0 { (0, th) } else { (3, 3 + bh) };
        if mx != 0 {
            let f = taps(mx);
            for j in r0..r1 {
                for i in 0..bw {
                    let t = &tmp[j * tw + i..j * tw + i + 8];
                    let mut s = 64;
                    for k in 0..8 {
                        s += f[k] * t[k] as i32;
                    }
                    mid[j * 64 + i] = (s >> 7).clamp(0, maxv) as u16;
                }
            }
        } else {
            for j in r0..r1 {
                for i in 0..bw {
                    mid[j * 64 + i] = tmp[j * tw + i + 3];
                }
            }
        }
        let stride = self.cur.stride[pl];
        let dst = &mut self.cur.planes[pl];
        let f = taps(my);
        for j in 0..bh {
            for i in 0..bw {
                let v = if my != 0 {
                    let mut s = 64;
                    for k in 0..8 {
                        s += f[k] * mid[(j + k) * 64 + i] as i32;
                    }
                    (s >> 7).clamp(0, maxv)
                } else {
                    mid[(j + 3) * 64 + i] as i32
                };
                let d = &mut dst[(y + j) * stride + x + i];
                *d = if avg { ((*d as i32 + v + 1) >> 1) as u16 } else { v as u16 };
            }
        }
    }

    pub(super) fn mc_l(&mut self, r: &Pic, avg: bool, x: usize, y: usize, mv: Mv, bw: usize, bh: usize) {
        let f = self.b.filter as usize;
        self.mc_block(0, r, x, y, mv.x as i32 * 2, mv.y as i32 * 2, bw, bh, r.w as i32, r.h as i32, f, avg);
    }

    fn mc_c(&mut self, r: &Pic, avg: bool, x: usize, y: usize, mv: Mv, bw: usize, bh: usize) {
        let f = self.b.filter as usize;
        let (sh, sv) = (self.ss_h, self.ss_v);
        let w = ((r.w + sh) >> sh) as i32;
        let h = ((r.h + sv) >> sv) as i32;
        let mx = (mv.x as i32) << (1 - sh);
        let my = (mv.y as i32) << (1 - sv);
        for pl in 1..3 {
            self.mc_block(pl, r, x, y, mx, my, bw, bh, w, h, f, avg);
        }
    }

    pub(super) fn inter_pred(&mut self) {
        let (row, col) = (self.row, self.col);
        let b = self.b;
        let bs = b.bs;
        let (sh, sv) = (self.ss_h, self.ss_v);
        let nref = if b.comp { 2 } else { 1 };
        for k in 0..nref {
            let r = self.refs[b.refs[k]].clone().unwrap();
            let r = &*r;
            let avg = k == 1;
            let mv = |i: usize| b.mv[i][k];
            let (lx, ly) = (col * 8, row * 8);
            if bs > BS_8X8 {
                if bs == BS_8X4 {
                    self.mc_l(r, avg, lx, ly, mv(0), 8, 4);
                    self.mc_l(r, avg, lx, ly + 4, mv(2), 8, 4);
                    let (cx, cw) = (lx >> sh, 8 >> sh);
                    if sv != 0 {
                        self.mc_c(r, avg, cx, row << 2, mv2(mv(0), mv(2)), cw, 4);
                    } else {
                        self.mc_c(r, avg, cx, ly, mv(0), cw, 4);
                        let m = if sh == 0 { mv(2) } else { mv2(mv(0), mv(2)) };
                        self.mc_c(r, avg, cx, ly + 4, m, cw, 4);
                    }
                } else if bs == BS_4X8 {
                    self.mc_l(r, avg, lx, ly, mv(0), 4, 8);
                    self.mc_l(r, avg, lx + 4, ly, mv(1), 4, 8);
                    let (cy, ch) = (ly >> sv, 8 >> sv);
                    if sh != 0 {
                        self.mc_c(r, avg, col << 2, cy, mv2(mv(0), mv(1)), 4, ch);
                    } else {
                        self.mc_c(r, avg, lx, cy, mv(0), 4, ch);
                        self.mc_c(r, avg, lx + 4, cy, mv(1), 4, ch);
                    }
                } else {
                    self.mc_l(r, avg, lx, ly, mv(0), 4, 4);
                    self.mc_l(r, avg, lx + 4, ly, mv(1), 4, 4);
                    self.mc_l(r, avg, lx, ly + 4, mv(2), 4, 4);
                    self.mc_l(r, avg, lx + 4, ly + 4, mv(3), 4, 4);
                    if sv != 0 {
                        if sh != 0 {
                            self.mc_c(r, avg, col << 2, row << 2, mv4(mv(0), mv(1), mv(2), mv(3)), 4, 4);
                        } else {
                            self.mc_c(r, avg, lx, row << 2, mv2(mv(0), mv(2)), 4, 4);
                            self.mc_c(r, avg, lx + 4, row << 2, mv2(mv(1), mv(3)), 4, 4);
                        }
                    } else if sh != 0 {
                        self.mc_c(r, avg, col << 2, ly, mv2(mv(0), mv(1)), 4, 4);
                        self.mc_c(r, avg, col << 2, ly + 4, mv2(mv(1), mv(2)), 4, 4);
                    } else {
                        self.mc_c(r, avg, lx, ly, mv(0), 4, 4);
                        self.mc_c(r, avg, lx + 4, ly, mv(1), 4, 4);
                        self.mc_c(r, avg, lx, ly + 4, mv(2), 4, 4);
                        self.mc_c(r, avg, lx + 4, ly + 4, mv(3), 4, 4);
                    }
                }
            } else {
                let bw = BWH_TAB[0][bs][0] as usize * 4;
                let bh = BWH_TAB[0][bs][1] as usize * 4;
                let uvbw = BWH_TAB[sh][bs][0] as usize * 4;
                let uvbh = BWH_TAB[sv][bs][1] as usize * 4;
                self.mc_l(r, avg, lx, ly, mv(0), bw, bh);
                self.mc_c(r, avg, lx >> sh, ly >> sv, mv(0), uvbw, uvbh);
            }
        }
    }

    pub fn inter_recon(&mut self) -> Result<(), &'static str> {
        let b = self.b;
        for k in 0..(1 + b.comp as usize) {
            match &self.refs[b.refs[k]] {
                Some(r) if r.w == self.h.width && r.h == self.h.height => {}
                Some(_) => return Err("scaled reference frames are not supported"),
                None => return Err("missing reference frame"),
            }
        }
        self.inter_pred();

        if !b.skip {
            let (row, col) = (self.row, self.col);
            let bs = b.bs;
            let w4 = (BWH_TAB[1][bs][0] as usize) << 1;
            let h4 = (BWH_TAB[1][bs][1] as usize) << 1;
            let step1d = 1usize << b.tx;
            let step = 1usize << (b.tx * 2);
            let end_x = (2 * (self.cols - col)).min(w4);
            let end_y = (2 * (self.rows - row)).min(h4);
            let mut n = 0;
            let mut y = 0;
            while y < end_y {
                let mut x = 0;
                while x < end_x {
                    let eob = self.eob[n] as usize;
                    if eob > 0 {
                        self.add_residual(0, b.tx, 0, n, eob, col * 8 + x * 4, row * 8 + y * 4);
                    }
                    x += step1d;
                    n += step;
                }
                y += step1d;
            }
            let end_x = end_x >> self.ss_h;
            let end_y = end_y >> self.ss_v;
            let uvstep1d = 1usize << b.uvtx;
            let step = 1usize << (b.uvtx * 2);
            for p in 0..2 {
                let mut n = 0;
                let mut y = 0;
                while y < end_y {
                    let mut x = 0;
                    while x < end_x {
                        let eob = self.uveob[p][n] as usize;
                        if eob > 0 {
                            let px = ((col * 8) >> self.ss_h) + x * 4;
                            let py = ((row * 8) >> self.ss_v) + y * 4;
                            self.add_residual(1 + p, b.uvtx, 0, n, eob, px, py);
                        }
                        x += uvstep1d;
                        n += step;
                    }
                    y += uvstep1d;
                }
            }
        }
        Ok(())
    }
}
