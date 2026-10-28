//! Loop filter: walks the per-superblock edge masks built while parsing blocks.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::dec::*;
use super::lpf_dsp::{filter_edge, Samples};

struct Luts {
    lim: [u8; 64],
    mblim: [u8; 64],
}

/// A plane that several loop-filter threads write at once.
///
/// Superblock rows are filtered as a wavefront: superblock (r, c) starts only after row r - 1 has finished
/// superblock c + 1, the last one there whose edges reach the samples that (r, c) reads or writes (an edge
/// reaches eight samples into the superblocks above and to the left). Superblocks filtered at the same time
/// therefore never touch the same sample, and the release/acquire progress counters make every sample a
/// thread reads one that was finished before it.
#[derive(Clone, Copy)]
struct Shared<'a> {
    ptr: *mut u16,
    len: usize,
    _plane: PhantomData<&'a mut [u16]>,
}

// SAFETY: concurrent use is limited to the disjoint superblocks described above.
unsafe impl Send for Shared<'_> {}
unsafe impl Sync for Shared<'_> {}

impl<'a> Shared<'a> {
    fn new(plane: &'a mut [u16]) -> Self {
        Shared { ptr: plane.as_mut_ptr(), len: plane.len(), _plane: PhantomData }
    }
}

impl Samples for Shared<'_> {
    #[inline]
    fn get(&self, i: usize) -> u16 {
        assert!(i < self.len);
        // SAFETY: in bounds, and no other thread is writing this sample (see `Shared`).
        unsafe { *self.ptr.add(i) }
    }

    #[inline]
    fn set(&mut self, i: usize, v: u16) {
        assert!(i < self.len);
        // SAFETY: in bounds, and no other thread is using this sample (see `Shared`).
        unsafe { *self.ptr.add(i) = v }
    }
}

/// Marks a superblock row finished when dropped, also when filtering it panicked, so that the
/// threads waiting on it do not wait for ever.
struct RowDone<'a>(&'a AtomicUsize);

impl Drop for RowDone<'_> {
    fn drop(&mut self) {
        self.0.store(usize::MAX, Ordering::Release);
    }
}

struct Plane<'a> {
    buf: Shared<'a>,
    ls: usize,
    bpp: u32,
}

impl Plane<'_> {
    /// `dir` 0 filters the edges between columns, 1 the edges between rows.
    #[allow(clippy::too_many_arguments)]
    fn f(&mut self, dir: usize, pos: usize, count: usize, wd: usize, e: i32, i: i32, h: i32) {
        if dir == 0 {
            filter_edge(&mut self.buf, pos, 1, self.ls, count, wd, e, i, h, self.bpp);
        } else {
            filter_edge(&mut self.buf, pos, self.ls, 1, count, wd, e, i, h, self.bpp);
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
        let (sb_rows, sb_cols) = (self.sb_rows, self.sb_cols);
        let lflvl = &self.lflvl;
        let [y, u, v] = &mut self.cur.planes;
        let planes = [Shared::new(y), Shared::new(u), Shared::new(v)];
        let filter_sb = |sbr: usize, sbc: usize| {
            let f = &lflvl[sbr * sb_cols + sbc];
            let (row, col) = (sbr * 8, sbc * 8);
            {
                let mut p = Plane { buf: planes[0], ls: strides[0], bpp };
                let dst = row * 8 * strides[0] + col * 8;
                plane_cols(&mut p, &luts, col, 0, 0, &f.level, &f.mask[0][0], dst);
                plane_rows(&mut p, &luts, row, 0, 0, &f.level, &f.mask[0][1], dst);
            }
            for pl in 1..3 {
                let mut p = Plane { buf: planes[pl], ls: strides[pl], bpp };
                let dst = ((row * 8) >> ss_v) * strides[pl] + ((col * 8) >> ss_h);
                plane_cols(&mut p, &luts, col, ss_h, ss_v, &f.level, &f.mask[uvm][0], dst);
                plane_rows(&mut p, &luts, row, ss_h, ss_v, &f.level, &f.mask[uvm][1], dst);
            }
        };

        let threads = thread_count().min(sb_rows);
        if threads <= 1 {
            for sbr in 0..sb_rows {
                for sbc in 0..sb_cols {
                    filter_sb(sbr, sbc);
                }
            }
            return;
        }
        // Each thread takes the next unfiltered superblock row and follows two superblocks behind the
        // row above it (see `Shared`).
        let done: Vec<AtomicUsize> = (0..sb_rows).map(|_| AtomicUsize::new(0)).collect();
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| loop {
                    let sbr = next.fetch_add(1, Ordering::Relaxed);
                    if sbr >= sb_rows {
                        break;
                    }
                    let _done = RowDone(&done[sbr]);
                    for sbc in 0..sb_cols {
                        if sbr > 0 {
                            let need = (sbc + 2).min(sb_cols);
                            let mut spins = 0u32;
                            while done[sbr - 1].load(Ordering::Acquire) < need {
                                if spins < 100 {
                                    std::hint::spin_loop();
                                    spins += 1;
                                } else {
                                    std::thread::yield_now();
                                }
                            }
                        }
                        filter_sb(sbr, sbc);
                        done[sbr].store(sbc + 1, Ordering::Release);
                    }
                });
            }
        });
    }
}
