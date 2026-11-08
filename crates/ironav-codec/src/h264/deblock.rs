//! The in-loop deblocking filter (ITU-T H.264 8.7).
//!
//! MBAFF frames are filtered as FFmpeg's h264_loopfilter.c (LGPL-2.1-or-later) does, which this
//! follows: the left edge between a frame and a field macroblock pair has a strength per line pair
//! of the current macroblock, and when its first strength is 4 every line of the group is filtered
//! strongly while otherwise lines of strength 4 are left as they are.

use super::mb::{chroma_qp, Kind, MbInfo, Picture};
use super::tables::{ALPHA, BETA, TC0};
use std::sync::atomic::{AtomicUsize, Ordering};

fn clip1(v: i32, bd: u32) -> u16 {
    v.clamp(0, (1 << bd) - 1) as u16
}

/// Filters the sample lines across one edge: four segments of `per_bs` lines, each with its own
/// boundary strength in `bs`. `pos` is the first q0 sample, `xs` the step across the edge and `ys`
/// the step along it.
#[allow(clippy::too_many_arguments)]
fn filter_edge(pl: &mut [u16], pos: usize, xs: usize, ys: usize, per_bs: usize, bs: &[u8; 4], alpha: i32, beta: i32, index_a: usize, chroma: bool, bd: u32, keep: (bool, bool)) {
    if alpha == 0 || beta == 0 {
        return;
    }
    let n = if chroma { 1 } else { 3 };
    // I_PCM and lossless samples are put back after filtering; only then are they read out
    let restore = keep.0 || keep.1;
    for (s, &b) in bs.iter().enumerate() {
        if b == 0 {
            continue;
        }
        let tc0 = if b < 4 { (TC0[index_a][b as usize].max(0) as i32) << (bd - 8) } else { 0 };
        for k in s * per_bs..(s + 1) * per_bs {
            let q = pos + k * ys;
            let (mut sp, mut sq) = ([0u16; 3], [0u16; 3]);
            if restore {
                for i in 0..n {
                    sp[i] = pl[q - (i + 1) * xs];
                    sq[i] = pl[q + i * xs];
                }
            }
            filter_line(pl, q, xs, b, tc0, alpha, beta, chroma, bd);
            if restore {
                for i in 0..n {
                    if keep.0 {
                        pl[q - (i + 1) * xs] = sp[i];
                    }
                    if keep.1 {
                        pl[q + i * xs] = sq[i];
                    }
                }
            }
        }
    }
}

/// Filters the sample line through `q`, the first sample past the edge, with boundary strength `b`;
/// `tc0` is the clipping threshold for `b` below 4.
#[allow(clippy::too_many_arguments)]
fn filter_line(pl: &mut [u16], q: usize, xs: usize, b: u8, tc0: i32, alpha: i32, beta: i32, chroma: bool, bd: u32) {
    let p0 = pl[q - xs] as i32;
    let q0 = pl[q] as i32;
    let p1 = pl[q - 2 * xs] as i32;
    let q1 = pl[q + xs] as i32;
    if (p0 - q0).abs() >= alpha || (p1 - p0).abs() >= beta || (q1 - q0).abs() >= beta {
        return;
    }
    if chroma {
        if b < 4 {
            let tc = tc0 + 1;
            let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
            pl[q - xs] = clip1(p0 + delta, bd);
            pl[q] = clip1(q0 - delta, bd);
        } else {
            pl[q - xs] = ((2 * p1 + p0 + q1 + 2) >> 2) as u16;
            pl[q] = ((2 * q1 + q0 + p1 + 2) >> 2) as u16;
        }
        return;
    }
    let p2 = pl[q - 3 * xs] as i32;
    let q2 = pl[q + 2 * xs] as i32;
    let ap = (p2 - p0).abs();
    let aq = (q2 - q0).abs();
    if b < 4 {
        let tc = tc0 + (ap < beta) as i32 + (aq < beta) as i32;
        let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
        pl[q - xs] = clip1(p0 + delta, bd);
        pl[q] = clip1(q0 - delta, bd);
        if ap < beta {
            pl[q - 2 * xs] = (p1 + ((p2 + ((p0 + q0 + 1) >> 1) - (p1 << 1)) >> 1).clamp(-tc0, tc0)) as u16;
        }
        if aq < beta {
            pl[q + xs] = (q1 + ((q2 + ((p0 + q0 + 1) >> 1) - (q1 << 1)) >> 1).clamp(-tc0, tc0)) as u16;
        }
    } else {
        let small = (p0 - q0).abs() < (alpha >> 2) + 2;
        if ap < beta && small {
            let p3 = pl[q - 4 * xs] as i32;
            pl[q - xs] = ((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3) as u16;
            pl[q - 2 * xs] = ((p2 + p1 + p0 + q0 + 2) >> 2) as u16;
            pl[q - 3 * xs] = ((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3) as u16;
        } else {
            pl[q - xs] = ((2 * p1 + p0 + q1 + 2) >> 2) as u16;
        }
        if aq < beta && small {
            let q3 = pl[q + 3 * xs] as i32;
            pl[q] = ((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3) as u16;
            pl[q + xs] = ((p0 + q0 + q1 + q2 + 2) >> 2) as u16;
            pl[q + 2 * xs] = ((2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3) as u16;
        } else {
            pl[q] = ((2 * q1 + q0 + p1 + 2) >> 2) as u16;
        }
    }
}

/// Whether the transform block holding 4x4 block (bx, by) has non-zero coefficients.
fn coded(m: &MbInfo, bx: usize, by: usize) -> bool {
    if m.t8x8 {
        let (x0, y0) = (bx & 2, by & 2);
        (0..2).any(|dy| (0..2).any(|dx| m.nnz[(y0 + dy) * 4 + x0 + dx] > 0))
    } else {
        m.nnz[by * 4 + bx] > 0
    }
}

/// Vectors a whole sample apart; `mvy` is the vertical limit in quarter samples (2 between field
/// macroblocks, whose rows are frame rows apart).
fn far(a: [i16; 2], b: [i16; 2], mvy: i32) -> bool {
    (a[0] as i32 - b[0] as i32).abs() >= 4 || (a[1] as i32 - b[1] as i32).abs() >= mvy
}

/// Different reference pictures, a different number of vectors or vectors a whole sample apart.
fn motion_differs(p: &MbInfo, pb: (usize, usize), q: &MbInfo, qb: (usize, usize), mvy: i32) -> bool {
    let far = |a, b| far(a, b, mvy);
    let (pi, qi) = (pb.1 * 4 + pb.0, qb.1 * 4 + qb.0);
    let (p8, q8) = ((pb.1 >> 1) * 2 + (pb.0 >> 1), (qb.1 >> 1) * 2 + (qb.0 >> 1));
    let pr = [p.ref_id[0][p8], p.ref_id[1][p8]];
    let qr = [q.ref_id[0][q8], q.ref_id[1][q8]];
    let pm = [p.mv[0][pi], p.mv[1][pi]];
    let qm = [q.mv[0][qi], q.mv[1][qi]];
    let np = pr.iter().filter(|&&r| r >= 0).count();
    let nq = qr.iter().filter(|&&r| r >= 0).count();
    if np != nq {
        return true;
    }
    if np == 1 {
        let (lp, lq) = (if pr[0] >= 0 { 0 } else { 1 }, if qr[0] >= 0 { 0 } else { 1 });
        return pr[lp] != qr[lq] || far(pm[lp], qm[lq]);
    }
    if !((pr[0] == qr[0] && pr[1] == qr[1]) || (pr[0] == qr[1] && pr[1] == qr[0])) {
        return true;
    }
    if pr[0] != pr[1] {
        if pr[0] == qr[0] {
            far(pm[0], qm[0]) || far(pm[1], qm[1])
        } else {
            far(pm[0], qm[1]) || far(pm[1], qm[0])
        }
    } else {
        (far(pm[0], qm[0]) || far(pm[1], qm[1])) && (far(pm[0], qm[1]) || far(pm[1], qm[0]))
    }
}

/// Boundary strength of each 4-sample segment along edge `e` of the macroblock `m`; `other` is the
/// macroblock on the far side of a macroblock edge (and `m` itself for inner edges).
fn strength(m: &MbInfo, other: &MbInfo, dir: usize, e: usize, mvy: i32) -> [u8; 4] {
    let mb_edge = e == 0;
    if m.kind != Kind::Inter || other.kind != Kind::Inter {
        return [if mb_edge { 4 } else { 3 }; 4];
    }
    let mut out = [0u8; 4];
    for (i, o) in out.iter_mut().enumerate() {
        let (q, p) = if dir == 0 { ((e, i), if mb_edge { (3, i) } else { (e - 1, i) }) } else { ((i, e), if mb_edge { (i, 3) } else { (i, e - 1) }) };
        *o = if coded(m, q.0, q.1) || coded(other, p.0, p.1) {
            2
        } else if motion_differs(other, p, m, q, mvy) {
            1
        } else {
            0
        };
    }
    out
}

pub fn deblock_picture(pic: &mut Picture, bd: u32) {
    deblock_rows(pic, bd, 0..pic.mb_h);
}

/// Filters the macroblock rows `rows`; in raster order with the rows above, this filters the picture.
/// Filtering a row changes up to three luma rows at the bottom of the row above.
pub fn deblock_rows(pic: &mut Picture, bd: u32, rows: std::ops::Range<usize>) {
    if pic.mbaff {
        deblock_mbaff(pic, bd, rows.start / 2..rows.end.div_ceil(2));
        return;
    }
    for mby in rows {
        for mbx in 0..pic.mb_w {
            deblock_mb(pic, bd, mbx, mby);
        }
    }
}

/// `deblock_picture` for live coding: each macroblock row is a task, and a macroblock waits only
/// for the row above to have finished the macroblock after it, so the rows are filtered in a
/// staggered wave, to the very samples the raster order gives (the filtering of a macroblock reads
/// and changes only samples of itself, of the macroblocks left of and above it, and of the one
/// above and to the right, through that one's left edge).
pub fn deblock_picture_par(pic: &mut Picture, bd: u32) {
    if pic.mbaff || pic.mb_h < 2 {
        return deblock_picture(pic, bd);
    }
    struct Shared(*mut Picture);
    // SAFETY: see below.
    unsafe impl Sync for Shared {}
    let (w, h) = (pic.mb_w, pic.mb_h);
    let done: Vec<AtomicUsize> = (0..h).map(|_| AtomicUsize::new(0)).collect();
    let shared = Shared(pic);
    let shared = &shared;
    crate::pool::run(h, |mby| {
        for mbx in 0..w {
            if mby > 0 {
                let need = (mbx + 2).min(w);
                let mut spins = 0u32;
                while done[mby - 1].load(Ordering::Acquire) < need {
                    spins += 1;
                    if spins < 200 {
                        std::hint::spin_loop();
                    } else {
                        std::thread::yield_now();
                    }
                }
            }
            // SAFETY: tasks are taken in row order and each waits as above, so no two macroblocks being
            // filtered at once are neighbours: they touch different samples, and none writes the
            // macroblock records or the slice parameters, which are only read.
            deblock_mb(unsafe { &mut *shared.0 }, bd, mbx, mby);
            done[mby].store(mbx + 1, Ordering::Release);
        }
    });
}

/// Filters macroblock (`mbx`, `mby`) of a picture that is not MBAFF.
fn deblock_mb(pic: &mut Picture, bd: u32, mbx: usize, mby: usize) {
    let w = pic.mb_w;
    let ls = w * 16;
    let cs = w * 8;
    let idx = mby * w + mbx;
    let m = &pic.mbs[idx];
    if m.slice == 0 {
        return;
    }
    let sp = pic.slices[m.slice as usize - 1];
    if sp.disable_deblocking == 1 {
        return;
    }
    let usable = |o: &MbInfo| o.slice != 0 && (sp.disable_deblocking == 0 || o.slice == m.slice);
    let left_ok = mbx > 0 && usable(&pic.mbs[idx - 1]);
    let top_ok = mby > 0 && usable(&pic.mbs[idx - w]);
    // the macroblocks on the far side of the left and top edges (`m` itself at the picture edge)
    let edge_mb = [if mbx > 0 { &pic.mbs[idx - 1] } else { m }, if mby > 0 { &pic.mbs[idx - w] } else { m }];
    // boundary strengths depend on the macroblocks only, so all planes and chroma share them
    let mut bs = [[[0u8; 4]; 4]; 2];
    for dir in 0..2 {
        for e in 0..4usize {
            let other = if e == 0 { edge_mb[dir] } else { m };
            bs[dir][e] = strength(m, other, dir, e, 4);
        }
    }
    // luma, and Cb and Cr filtered the same way in 4:4:4
    for pl in 0..if pic.cf == 3 { 3 } else { 1 } {
        let off = if pl > 0 { sp.chroma_qp_offset[pl - 1] } else { 0 };
        for dir in 0..2 {
            for e in 0..4usize {
                if e == 0 && !(if dir == 0 { left_ok } else { top_ok }) {
                    continue;
                }
                if m.t8x8 && e & 1 == 1 {
                    continue;
                }
                let other = if e == 0 { edge_mb[dir] } else { m };
                let qpav = if pl == 0 { (other.qp as i32 + m.qp as i32 + 1) >> 1 } else { (chroma_qp(other.qp as i32, off, bd) + chroma_qp(m.qp as i32, off, bd) + 1) >> 1 };
                let ia = (qpav + sp.alpha_offset).clamp(0, 51) as usize;
                let ib = (qpav + sp.beta_offset).clamp(0, 51) as usize;
                let (alpha, beta) = ((ALPHA[ia] as i32) << (bd - 8), (BETA[ib] as i32) << (bd - 8));
                let (x0, y0) = (mbx * 16, mby * 16);
                if dir == 0 {
                    filter_edge(&mut pic.planes[pl], y0 * ls + x0 + e * 4, 1, ls, 4, &bs[dir][e], alpha, beta, ia, false, bd, (other.keep, m.keep));
                } else {
                    filter_edge(&mut pic.planes[pl], (y0 + e * 4) * ls + x0, ls, 1, 4, &bs[dir][e], alpha, beta, ia, false, bd, (other.keep, m.keep));
                }
            }
        }
    }
    // chroma: vertical edges at 0 and 4, horizontal edges every 4 rows (every 4 chroma rows of
    // 4:2:2, every 8 luma rows of 4:2:0); each takes the strength of the luma edge it lines up with
    if matches!(pic.cf, 0 | 3) {
        return;
    }
    let chh = pic.ch;
    for comp in 0..2usize {
        let off = sp.chroma_qp_offset[comp];
        for dir in 0..2 {
            let edges: &[usize] = if dir == 1 && chh == 16 { &[0, 1, 2, 3] } else { &[0, 2] };
            for &e in edges {
                if e == 0 && !(if dir == 0 { left_ok } else { top_ok }) {
                    continue;
                }
                let other = if e == 0 { edge_mb[dir] } else { m };
                let qpav = (chroma_qp(other.qp as i32, off, bd) + chroma_qp(m.qp as i32, off, bd) + 1) >> 1;
                let ia = (qpav + sp.alpha_offset).clamp(0, 51) as usize;
                let ib = (qpav + sp.beta_offset).clamp(0, 51) as usize;
                let (alpha, beta) = ((ALPHA[ia] as i32) << (bd - 8), (BETA[ib] as i32) << (bd - 8));
                let (x0, y0) = (mbx * 8, mby * chh);
                if dir == 0 {
                    filter_edge(&mut pic.planes[1 + comp], y0 * cs + x0 + e * 2, 1, cs, chh / 4, &bs[dir][e], alpha, beta, ia, true, bd, (other.keep, m.keep));
                } else if chh == 16 {
                    filter_edge(&mut pic.planes[1 + comp], (y0 + e * 4) * cs + x0, cs, 1, 2, &bs[dir][e], alpha, beta, ia, true, bd, (other.keep, m.keep));
                } else {
                    filter_edge(&mut pic.planes[1 + comp], (y0 + e * 2) * cs + x0, cs, 1, 2, &bs[dir][e], alpha, beta, ia, true, bd, (other.keep, m.keep));
                }
            }
        }
    }
}

/// Filters one sample line across an edge, putting back I_PCM and lossless samples after.
#[allow(clippy::too_many_arguments)]
fn filter_one(pl: &mut [u16], q: usize, xs: usize, b: u8, index_a: usize, alpha: i32, beta: i32, chroma: bool, bd: u32, keep: (bool, bool)) {
    if b == 0 || alpha == 0 || beta == 0 {
        return;
    }
    let tc0 = if b < 4 { (TC0[index_a][b as usize].max(0) as i32) << (bd - 8) } else { 0 };
    let n = if chroma { 1 } else { 3 };
    let (mut sp, mut sq) = ([0u16; 3], [0u16; 3]);
    for i in 0..n {
        sp[i] = pl[q - (i + 1) * xs];
        sq[i] = pl[q + i * xs];
    }
    filter_line(pl, q, xs, b, tc0, alpha, beta, chroma, bd);
    for i in 0..n {
        if keep.0 {
            pl[q - (i + 1) * xs] = sp[i];
        }
        if keep.1 {
            pl[q + i * xs] = sq[i];
        }
    }
}

/// alpha, beta and indexA for the average QP `qpav`.
fn thresholds(qpav: i32, sp: &super::mb::SliceParams, bd: u32) -> (i32, i32, usize) {
    let ia = (qpav + sp.alpha_offset).clamp(0, 51) as usize;
    let ib = (qpav + sp.beta_offset).clamp(0, 51) as usize;
    ((ALPHA[ia] as i32) << (bd - 8), (BETA[ib] as i32) << (bd - 8), ia)
}

/// Filters the macroblock pair rows `pairs` of an MBAFF frame: pair by pair, the top macroblock
/// and then the bottom one.
fn deblock_mbaff(pic: &mut Picture, bd: u32, pairs: std::ops::Range<usize>) {
    let w = pic.mb_w;
    let (cw, chh) = (pic.cw, pic.ch);
    let (ls, cs) = (w * 16, w * cw);
    let ncomp = if pic.cf == 3 { 3 } else { 1 };
    let chroma = matches!(pic.cf, 1 | 2);
    let Picture { planes, mbs, slices, .. } = pic;
    for py in pairs {
        for mbx in 0..w {
            for bot in 0..2usize {
                let mby = 2 * py + bot;
                let idx = mby * w + mbx;
                let m = &mbs[idx];
                if m.slice == 0 {
                    continue;
                }
                let sp = slices[m.slice as usize - 1];
                if sp.disable_deblocking == 1 {
                    continue;
                }
                let usable = |o: &MbInfo| o.slice != 0 && (sp.disable_deblocking == 0 || o.slice == m.slice);
                let fld = m.field;
                let mvy = if fld { 2 } else { 4 };
                // the macroblock's first sample and the distance between its rows, luma and chroma
                let (lo, lrs, co, crs) = if fld {
                    (2 * py * 16 * ls + bot * ls + mbx * 16, 2 * ls, 2 * py * chh * cs + bot * cs + mbx * cw, 2 * cs)
                } else {
                    (mby * 16 * ls + mbx * 16, ls, mby * chh * cs + mbx * cw, cs)
                };
                let intra = |o: &MbInfo| o.kind != Kind::Inter;
                let cqp = |q: i8, c: usize| chroma_qp(q as i32, sp.chroma_qp_offset[c], bd);
                // QP average with `o` for luma-style component `pl`
                let qpav = |o: &MbInfo, pl: usize| if pl == 0 { (o.qp as i32 + m.qp as i32 + 1) >> 1 } else { (cqp(o.qp, pl - 1) + cqp(m.qp, pl - 1) + 1) >> 1 };

                // ---- left edge
                let lt = 2 * py * w + mbx;
                let left_ok = mbx > 0 && usable(&mbs[lt - 1]);
                let mut first_done = false;
                if left_ok && mbs[lt - 1].field != fld {
                    // the left pair is coded the other way: a strength per two lines of the
                    // current macroblock (one line of 4:2:0 chroma), from the left macroblock
                    // covering them
                    first_done = true;
                    let nb = |i: usize| &mbs[lt - 1 + if fld { i >> 2 } else { i & 1 } * w];
                    let mut bs8 = [0u8; 8];
                    for (i, b) in bs8.iter_mut().enumerate() {
                        let n = nb(i);
                        let nrow = if fld { i & 3 } else { bot * 2 + (i >> 2) };
                        *b = if intra(m) || intra(n) { 4 } else { 1 + (coded(m, 0, i >> 1) || coded(n, 3, nrow)) as u8 };
                    }
                    // the strength of line pair i, filtered as FFmpeg's groups of lines do
                    let eff = |i: usize| {
                        let first = if fld { bs8[i & 4] } else { bs8[i & 1] };
                        match (first, bs8[i]) {
                            (4, _) => 4,
                            (_, 4) => 0,
                            (_, b) => b,
                        }
                    };
                    // line t of a component whose macroblock has `n` lines: its strength index
                    let bsi = |t: usize, n: usize| if n == 8 { t } else if fld { t >> 1 } else { 2 * (t >> 2) + (t & 1) };
                    for pl in 0..ncomp {
                        for t in 0..16 {
                            let i = bsi(t, 16);
                            let n = nb(i);
                            let (alpha, beta, ia) = thresholds(qpav(n, pl), &sp, bd);
                            filter_one(&mut planes[pl], lo + t * lrs, 1, eff(i), ia, alpha, beta, false, bd, (n.keep, m.keep));
                        }
                    }
                    if chroma {
                        for comp in 0..2 {
                            for t in 0..chh {
                                let i = bsi(t, chh);
                                let n = nb(i);
                                let (alpha, beta, ia) = thresholds((cqp(n.qp, comp) + cqp(m.qp, comp) + 1) >> 1, &sp, bd);
                                filter_one(&mut planes[1 + comp], co + t * crs, 1, eff(i), ia, alpha, beta, true, bd, (n.keep, m.keep));
                            }
                        }
                    }
                }

                // ---- top edge: the macroblock above in the same field, or the frame row above
                let top = if fld {
                    if py == 0 {
                        None
                    } else if bot == 1 || mbs[idx - 2 * w].field {
                        Some(idx - 2 * w)
                    } else {
                        Some(idx - w)
                    }
                } else if bot == 1 || py > 0 {
                    Some(idx - w)
                } else {
                    None
                };
                let top = top.filter(|&t| usable(&mbs[t]));
                // a frame macroblock under a field pair: its top edge is filtered once for each field
                let split = !fld && bot == 0 && top.is_some_and(|t| mbs[t].field);

                // ---- strengths of the other edges
                let mut bs = [[[0u8; 4]; 4]; 2];
                for e in 0..4usize {
                    if e > 0 || (left_ok && !first_done) {
                        bs[0][e] = strength(m, if e == 0 { &mbs[idx - 1] } else { m }, 0, e, mvy);
                    }
                    if e > 0 {
                        bs[1][e] = strength(m, m, 1, e, mvy);
                    }
                }
                if let (Some(t), false) = (top, split) {
                    let n = &mbs[t];
                    bs[1][0] = if intra(m) || intra(n) {
                        [if fld || n.field { 3 } else { 4 }; 4]
                    } else if fld != n.field {
                        std::array::from_fn(|i| 1 + (coded(m, i, 0) || coded(n, i, 3)) as u8)
                    } else {
                        strength(m, n, 1, 0, mvy)
                    };
                }
                let edge_mb = [if left_ok { Some(&mbs[idx - 1]) } else { None }, if split { None } else { top.map(|t| &mbs[t]) }];
                let other = |dir: usize, e: usize| if e == 0 { edge_mb[dir] } else { Some(m) };
                // the two top edges of a frame macroblock under a field pair, after the vertical edges
                let split_b = |j: usize| -> (&MbInfo, [u8; 4]) {
                    let n = &mbs[idx - 2 * w + j * w];
                    (n, if intra(m) || intra(n) { [3; 4] } else { std::array::from_fn(|i| 1 + (coded(m, i, 0) || coded(n, i, 3)) as u8) })
                };

                // ---- filtering
                for pl in 0..ncomp {
                    for dir in 0..2 {
                        if dir == 1 && split {
                            for j in 0..2usize {
                                let (n, b) = split_b(j);
                                let (alpha, beta, ia) = thresholds(qpav(n, pl), &sp, bd);
                                filter_edge(&mut planes[pl], lo + j * ls, 2 * ls, 1, 4, &b, alpha, beta, ia, false, bd, (n.keep, m.keep));
                            }
                        }
                        for e in 0..4usize {
                            if (dir == 0 && e == 0 && first_done) || (m.t8x8 && e & 1 == 1) {
                                continue;
                            }
                            let Some(o) = other(dir, e) else { continue };
                            let (alpha, beta, ia) = thresholds(qpav(o, pl), &sp, bd);
                            if dir == 0 {
                                filter_edge(&mut planes[pl], lo + e * 4, 1, lrs, 4, &bs[dir][e], alpha, beta, ia, false, bd, (o.keep, m.keep));
                            } else {
                                filter_edge(&mut planes[pl], lo + e * 4 * lrs, lrs, 1, 4, &bs[dir][e], alpha, beta, ia, false, bd, (o.keep, m.keep));
                            }
                        }
                    }
                }
                if !chroma {
                    continue;
                }
                for comp in 0..2usize {
                    for dir in 0..2 {
                        if dir == 1 && split {
                            for j in 0..2usize {
                                let (n, b) = split_b(j);
                                let (alpha, beta, ia) = thresholds((cqp(n.qp, comp) + cqp(m.qp, comp) + 1) >> 1, &sp, bd);
                                filter_edge(&mut planes[1 + comp], co + j * cs, 2 * cs, 1, 2, &b, alpha, beta, ia, true, bd, (n.keep, m.keep));
                            }
                        }
                        let edges: &[usize] = if dir == 1 && chh == 16 { &[0, 1, 2, 3] } else { &[0, 2] };
                        for &e in edges {
                            if dir == 0 && e == 0 && first_done {
                                continue;
                            }
                            let Some(o) = other(dir, e) else { continue };
                            let (alpha, beta, ia) = thresholds((cqp(o.qp, comp) + cqp(m.qp, comp) + 1) >> 1, &sp, bd);
                            if dir == 0 {
                                filter_edge(&mut planes[1 + comp], co + e * 2, 1, crs, chh / 4, &bs[dir][e], alpha, beta, ia, true, bd, (o.keep, m.keep));
                            } else if chh == 16 {
                                filter_edge(&mut planes[1 + comp], co + e * 4 * crs, crs, 1, 2, &bs[dir][e], alpha, beta, ia, true, bd, (o.keep, m.keep));
                            } else {
                                filter_edge(&mut planes[1 + comp], co + e * 2 * crs, crs, 1, 2, &bs[dir][e], alpha, beta, ia, true, bd, (o.keep, m.keep));
                            }
                        }
                    }
                }
            }
        }
    }
}
