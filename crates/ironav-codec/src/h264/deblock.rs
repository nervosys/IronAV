//! The in-loop deblocking filter (ITU-T H.264 8.7).

use super::mb::{chroma_qp, Kind, MbInfo, Picture};
use super::tables::{ALPHA, BETA, TC0};

fn clip1(v: i32, bd: u32) -> u16 {
    v.clamp(0, (1 << bd) - 1) as u16
}

/// Filters `lines` sample lines across one edge. `pos` is the first q0 sample, `xs` the step
/// across the edge and `ys` the step along it.
#[allow(clippy::too_many_arguments)]
fn filter_edge(pl: &mut [u16], pos: usize, xs: usize, ys: usize, lines: usize, bs: &[u8; 4], per_bs: usize, alpha: i32, beta: i32, index_a: usize, chroma: bool, bd: u32, keep: (bool, bool)) {
    if alpha == 0 || beta == 0 {
        return;
    }
    for k in 0..lines {
        let b = bs[k / per_bs];
        if b == 0 {
            continue;
        }
        let q = pos + k * ys;
        let n = if chroma { 1 } else { 3 };
        let (mut sp, mut sq) = ([0u16; 3], [0u16; 3]);
        if keep.0 || keep.1 {
            for i in 0..n {
                sp[i] = pl[q - (i + 1) * xs];
                sq[i] = pl[q + i * xs];
            }
        }
        filter_line(pl, q, xs, b, alpha, beta, index_a, chroma, bd);
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

/// Filters the sample line through `q`, the first sample past the edge, with boundary strength `b`.
#[allow(clippy::too_many_arguments)]
fn filter_line(pl: &mut [u16], q: usize, xs: usize, b: u8, alpha: i32, beta: i32, index_a: usize, chroma: bool, bd: u32) {
    let p0 = pl[q - xs] as i32;
    let q0 = pl[q] as i32;
    let p1 = pl[q - 2 * xs] as i32;
    let q1 = pl[q + xs] as i32;
    if (p0 - q0).abs() >= alpha || (p1 - p0).abs() >= beta || (q1 - q0).abs() >= beta {
        return;
    }
    if chroma {
        if b < 4 {
            let tc = ((TC0[index_a][b as usize].max(0) as i32) << (bd - 8)) + 1;
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
        let tc0 = (TC0[index_a][b as usize].max(0) as i32) << (bd - 8);
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

fn far(a: [i16; 2], b: [i16; 2]) -> bool {
    (a[0] as i32 - b[0] as i32).abs() >= 4 || (a[1] as i32 - b[1] as i32).abs() >= 4
}

/// Different reference pictures, a different number of vectors or vectors a whole sample apart.
fn motion_differs(p: &MbInfo, pb: (usize, usize), q: &MbInfo, qb: (usize, usize)) -> bool {
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
fn strength(m: &MbInfo, other: &MbInfo, dir: usize, e: usize) -> [u8; 4] {
    let mb_edge = e == 0;
    if m.kind != Kind::Inter || other.kind != Kind::Inter {
        return [if mb_edge { 4 } else { 3 }; 4];
    }
    let mut out = [0u8; 4];
    for (i, o) in out.iter_mut().enumerate() {
        let (q, p) = if dir == 0 { ((e, i), if mb_edge { (3, i) } else { (e - 1, i) }) } else { ((i, e), if mb_edge { (i, 3) } else { (i, e - 1) }) };
        *o = if coded(m, q.0, q.1) || coded(other, p.0, p.1) {
            2
        } else if motion_differs(other, p, m, q) {
            1
        } else {
            0
        };
    }
    out
}

pub fn deblock_picture(pic: &mut Picture, bd: u32) {
    let (w, h) = (pic.mb_w, pic.mb_h);
    let ls = w * 16;
    let cs = w * 8;
    for mby in 0..h {
        for mbx in 0..w {
            let idx = mby * w + mbx;
            let m = pic.mbs[idx];
            if m.slice == 0 {
                continue;
            }
            let sp = pic.slices[m.slice as usize - 1];
            if sp.disable_deblocking == 1 {
                continue;
            }
            let usable = |o: &MbInfo| o.slice != 0 && (sp.disable_deblocking == 0 || o.slice == m.slice);
            let left_ok = mbx > 0 && usable(&pic.mbs[idx - 1]);
            let top_ok = mby > 0 && usable(&pic.mbs[idx - w]);
            let left = if mbx > 0 { pic.mbs[idx - 1] } else { m };
            let top = if mby > 0 { pic.mbs[idx - w] } else { m };
            // luma, and Cb and Cr filtered the same way in 4:4:4
            for pl in 0..if pic.cf == 3 { 3 } else { 1 } {
            let off = if pl > 0 { pic.slices[m.slice as usize - 1].chroma_qp_offset[pl - 1] } else { 0 };
            for dir in 0..2 {
                for e in 0..4usize {
                    if e == 0 && !(if dir == 0 { left_ok } else { top_ok }) {
                        continue;
                    }
                    if m.t8x8 && e & 1 == 1 {
                        continue;
                    }
                    let other = if e == 0 {
                        if dir == 0 {
                            &left
                        } else {
                            &top
                        }
                    } else {
                        &m
                    };
                    let bs = strength(&m, other, dir, e);
                    let qpav = if pl == 0 { (other.qp as i32 + m.qp as i32 + 1) >> 1 } else { (chroma_qp(other.qp as i32, off, bd) + chroma_qp(m.qp as i32, off, bd) + 1) >> 1 };
                    let ia = (qpav + sp.alpha_offset).clamp(0, 51) as usize;
                    let ib = (qpav + sp.beta_offset).clamp(0, 51) as usize;
                    let (alpha, beta) = ((ALPHA[ia] as i32) << (bd - 8), (BETA[ib] as i32) << (bd - 8));
                    let (x0, y0) = (mbx * 16, mby * 16);
                    if dir == 0 {
                        filter_edge(&mut pic.planes[pl], y0 * ls + x0 + e * 4, 1, ls, 16, &bs, 4, alpha, beta, ia, false, bd, (other.keep, m.keep));
                    } else {
                        filter_edge(&mut pic.planes[pl], (y0 + e * 4) * ls + x0, ls, 1, 16, &bs, 4, alpha, beta, ia, false, bd, (other.keep, m.keep));
                    }
                }
            }
            }
            // chroma: vertical edges at 0 and 4, horizontal edges every 4 rows (every 4 chroma rows of
            // 4:2:2, every 8 luma rows of 4:2:0); each takes the strength of the luma edge it lines up with
            if matches!(pic.cf, 0 | 3) {
                continue;
            }
            let chh = pic.ch;
            for comp in 0..2usize {
                let off = pic.slices[m.slice as usize - 1].chroma_qp_offset[comp];
                for dir in 0..2 {
                    let edges: &[usize] = if dir == 1 && chh == 16 { &[0, 1, 2, 3] } else { &[0, 2] };
                    for &e in edges {
                        if e == 0 && !(if dir == 0 { left_ok } else { top_ok }) {
                            continue;
                        }
                        let other = if e == 0 {
                            if dir == 0 {
                                &left
                            } else {
                                &top
                            }
                        } else {
                            &m
                        };
                        let bs = strength(&m, other, dir, e);
                        let qpav = (chroma_qp(other.qp as i32, off, bd) + chroma_qp(m.qp as i32, off, bd) + 1) >> 1;
                        let ia = (qpav + sp.alpha_offset).clamp(0, 51) as usize;
                        let ib = (qpav + sp.beta_offset).clamp(0, 51) as usize;
                        let (alpha, beta) = ((ALPHA[ia] as i32) << (bd - 8), (BETA[ib] as i32) << (bd - 8));
                        let (x0, y0) = (mbx * 8, mby * chh);
                        if dir == 0 {
                            filter_edge(&mut pic.planes[1 + comp], y0 * cs + x0 + e * 2, 1, cs, chh, &bs, chh / 4, alpha, beta, ia, true, bd, (other.keep, m.keep));
                        } else if chh == 16 {
                            filter_edge(&mut pic.planes[1 + comp], (y0 + e * 4) * cs + x0, cs, 1, 8, &bs, 2, alpha, beta, ia, true, bd, (other.keep, m.keep));
                        } else {
                            filter_edge(&mut pic.planes[1 + comp], (y0 + e * 2) * cs + x0, cs, 1, 8, &bs, 2, alpha, beta, ia, true, bd, (other.keep, m.keep));
                        }
                    }
                }
            }
        }
    }
}
