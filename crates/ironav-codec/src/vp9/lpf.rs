//! Loop filter: walks the per-superblock edge masks built while parsing blocks.

use super::dec::*;
use super::lpf_dsp::filter_edge;

struct Luts {
    lim: [u8; 64],
    mblim: [u8; 64],
}

struct Plane<'a> {
    buf: &'a mut [u16],
    ls: usize,
    bpp: u32,
}

impl Plane<'_> {
    /// `dir` 0 filters the edges between columns, 1 the edges between rows.
    #[allow(clippy::too_many_arguments)]
    fn f(&mut self, dir: usize, pos: usize, count: usize, wd: usize, e: i32, i: i32, h: i32) {
        if dir == 0 {
            filter_edge(self.buf, pos, 1, self.ls, count, wd, e, i, h, self.bpp);
        } else {
            filter_edge(self.buf, pos, self.ls, 1, count, wd, e, i, h, self.bpp);
        }
    }

    /// Two 8 sample runs along the edge with their own thresholds.
    #[allow(clippy::too_many_arguments)]
    fn mix2(&mut self, dir: usize, pos: usize, wd1: usize, wd2: usize, e: (i32, i32), i: (i32, i32), h: (i32, i32)) {
        let second = if dir == 0 { 8 * self.ls } else { 8 };
        self.f(dir, pos, 8, wd1, e.0, i.0, h.0);
        self.f(dir, pos + second, 8, wd2, e.1, i.1, h.1);
    }
}

fn thr(l: &Luts, lv: u8) -> (i32, i32, i32) {
    (l.mblim[lv as usize] as i32, l.lim[lv as usize] as i32, (lv >> 4) as i32)
}

#[allow(clippy::too_many_arguments)]
fn plane_cols(p: &mut Plane, luts: &Luts, col: usize, ss_h: usize, ss_v: usize, level: &[u8; 64], mask: &[[u8; 4]; 8], dst: usize) {
    let ls = p.ls;
    let mut dst = dst;
    let mut lvl = 0usize;
    let mut y = 0;
    while y < 8 {
        let mut ptr = dst;
        let mut l = lvl;
        let hmask1 = mask[y];
        let hmask2 = mask[y + 1 + ss_v];
        let hm1 = (hmask1[0] | hmask1[1] | hmask1[2]) as u32;
        let hm13 = hmask1[3] as u32;
        let hm2 = (hmask2[1] | hmask2[2]) as u32;
        let hm23 = hmask2[3] as u32;
        let hm = hm1 | hm2 | hm13 | hm23;
        let l2 = 8usize << ss_v;
        let mut x = 1u32;
        while hm & !(x - 1) != 0 {
            if col != 0 || x > 1 {
                if hm1 & x != 0 {
                    let lv = level[l];
                    let (e, i, h) = thr(luts, lv);
                    if hmask1[0] as u32 & x != 0 {
                        if hmask2[0] as u32 & x != 0 {
                            p.f(0, ptr, 16, 16, e, i, h);
                        } else {
                            p.f(0, ptr, 8, 16, e, i, h);
                        }
                    } else if hm2 & x != 0 {
                        let (e2, i2, h2) = thr(luts, level[l + l2]);
                        let a = if hmask1[1] as u32 & x != 0 { 8 } else { 4 };
                        let b = if hmask2[1] as u32 & x != 0 { 8 } else { 4 };
                        p.mix2(0, ptr, a, b, (e, e2), (i, i2), (h, h2));
                    } else {
                        let wd = if hmask1[1] as u32 & x != 0 { 8 } else { 4 };
                        p.f(0, ptr, 8, wd, e, i, h);
                    }
                } else if hm2 & x != 0 {
                    let (e, i, h) = thr(luts, level[l + l2]);
                    let wd = if hmask2[1] as u32 & x != 0 { 8 } else { 4 };
                    p.f(0, ptr + 8 * ls, 8, wd, e, i, h);
                }
            }
            if ss_h != 0 {
                if x & 0xAA != 0 {
                    l += 2;
                }
            } else {
                if hm13 & x != 0 {
                    let (e, i, h) = thr(luts, level[l]);
                    if hm23 & x != 0 {
                        let (e2, i2, h2) = thr(luts, level[l + l2]);
                        p.mix2(0, ptr + 4, 4, 4, (e, e2), (i, i2), (h, h2));
                    } else {
                        p.f(0, ptr + 4, 8, 4, e, i, h);
                    }
                } else if hm23 & x != 0 {
                    let (e, i, h) = thr(luts, level[l + l2]);
                    p.f(0, ptr + 8 * ls + 4, 8, 4, e, i, h);
                }
                l += 1;
            }
            x <<= 1;
            ptr += 8 >> ss_h;
        }
        y += 2 << ss_v;
        dst += 16 * ls;
        lvl += 16 << ss_v;
    }
}

#[allow(clippy::too_many_arguments)]
fn plane_rows(p: &mut Plane, luts: &Luts, row: usize, ss_h: usize, ss_v: usize, level: &[u8; 64], mask: &[[u8; 4]; 8], dst: usize) {
    let ls = p.ls;
    let mut dst = dst;
    let mut lvl = 0usize;
    for y in 0..8 {
        let mut ptr = dst;
        let mut l = lvl;
        let vmask = mask[y];
        let vm = (vmask[0] | vmask[1] | vmask[2]) as u32;
        let vm3 = vmask[3] as u32;
        let nx = 1u32 << (1 + ss_h);
        let mut x = 1u32;
        while vm & !(x - 1) != 0 {
            if row != 0 || y != 0 {
                if vm & x != 0 {
                    let (e, i, h) = thr(luts, level[l]);
                    if vmask[0] as u32 & x != 0 {
                        if vmask[0] as u32 & (x << (1 + ss_h)) != 0 {
                            p.f(1, ptr, 16, 16, e, i, h);
                        } else {
                            p.f(1, ptr, 8, 16, e, i, h);
                        }
                    } else if vm & (x << (1 + ss_h)) != 0 {
                        let (e2, i2, h2) = thr(luts, level[l + 1 + ss_h]);
                        let a = if vmask[1] as u32 & x != 0 { 8 } else { 4 };
                        let b = if vmask[1] as u32 & (x << (1 + ss_h)) != 0 { 8 } else { 4 };
                        p.mix2(1, ptr, a, b, (e, e2), (i, i2), (h, h2));
                    } else {
                        let wd = if vmask[1] as u32 & x != 0 { 8 } else { 4 };
                        p.f(1, ptr, 8, wd, e, i, h);
                    }
                } else if vm & (x << (1 + ss_h)) != 0 {
                    let (e, i, h) = thr(luts, level[l + 1 + ss_h]);
                    let wd = if vmask[1] as u32 & (x << (1 + ss_h)) != 0 { 8 } else { 4 };
                    p.f(1, ptr + 8, 8, wd, e, i, h);
                }
            }
            if ss_v == 0 {
                if vm3 & x != 0 {
                    let (e, i, h) = thr(luts, level[l]);
                    if vm3 & (x << (1 + ss_h)) != 0 {
                        let (e2, i2, h2) = thr(luts, level[l + 1 + ss_h]);
                        p.mix2(1, ptr + ls * 4, 4, 4, (e, e2), (i, i2), (h, h2));
                    } else {
                        p.f(1, ptr + ls * 4, 8, 4, e, i, h);
                    }
                } else if vm3 & (x << (1 + ss_h)) != 0 {
                    let (e, i, h) = thr(luts, level[l + 1 + ss_h]);
                    p.f(1, ptr + ls * 4 + 8, 8, 4, e, i, h);
                }
            }
            x <<= 2 << ss_h;
            ptr += 16;
            l += 2 << ss_h;
            let _ = nx;
        }
        if ss_v != 0 {
            if y & 1 != 0 {
                lvl += 16;
            }
        } else {
            lvl += 8;
        }
        dst += (8 * ls) >> ss_v;
    }
}

impl Dec {
    pub fn loopfilter(&mut self, sharpness: u8) {
        if self.h.lf_level == 0 {
            return;
        }
        let mut luts = Luts { lim: [0; 64], mblim: [0; 64] };
        let sharp = sharpness as i32;
        for i in 1..64i32 {
            let mut limit = i;
            if sharp > 0 {
                limit >>= (sharp + 3) >> 2;
                limit = limit.min(9 - sharp);
            }
            limit = limit.max(1);
            luts.lim[i as usize] = limit as u8;
            luts.mblim[i as usize] = (2 * (i + 2) + limit) as u8;
        }
        let (ss_h, ss_v) = (self.ss_h, self.ss_v);
        let bpp = self.bpp;
        let strides = self.cur.stride;
        let uvm = if ss_h | ss_v != 0 { 1 } else { 0 };
        for sbr in 0..self.sb_rows {
            for sbc in 0..self.sb_cols {
                let f = self.lflvl[sbr * self.sb_cols + sbc];
                let (row, col) = (sbr * 8, sbc * 8);
                {
                    let mut p = Plane { buf: &mut self.cur.planes[0], ls: strides[0], bpp };
                    let dst = row * 8 * strides[0] + col * 8;
                    plane_cols(&mut p, &luts, col, 0, 0, &f.level, &f.mask[0][0], dst);
                    plane_rows(&mut p, &luts, row, 0, 0, &f.level, &f.mask[0][1], dst);
                }
                for pl in 1..3 {
                    let mut p = Plane { buf: &mut self.cur.planes[pl], ls: strides[pl], bpp };
                    let dst = ((row * 8) >> ss_v) * strides[pl] + ((col * 8) >> ss_h);
                    plane_cols(&mut p, &luts, col, ss_h, ss_v, &f.level, &f.mask[uvm][0], dst);
                    plane_rows(&mut p, &luts, row, ss_h, ss_v, &f.level, &f.mask[uvm][1], dst);
                }
            }
        }
    }
}
