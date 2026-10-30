//! In-loop filters: the deblocking filter (ITU-T H.265 8.7.2) and sample adaptive offset (8.7.3).

use super::frame::*;
use super::ps::{Pps, Sps};
use super::tables::{BETA_TABLE, QP_C_TABLE, TC_TABLE};

pub fn run(pic: &mut CurPic, sps: &Sps, pps: &Pps, geo: &Geometry) {
    if pic.slices.iter().any(|s| !s.deblocking_disabled) {
        deblock(pic, sps, pps, geo);
    }
    if sps.sao && pic.sao.iter().any(|s| s.kind != [0; 3]) {
        sao(pic, sps, pps, geo);
    }
}

fn mv_far(a: [i16; 2], b: [i16; 2]) -> bool {
    (a[0] as i32 - b[0] as i32).abs() >= 4 || (a[1] as i32 - b[1] as i32).abs() >= 4
}

/// Whether the motion on the two sides of an edge calls for filtering (8.7.2.4).
fn motion_differs(a: &MvField, b: &MvField) -> bool {
    let (na, nb) = ((a.pred & 3).count_ones(), (b.pred & 3).count_ones());
    if na != nb {
        return true;
    }
    if na == 1 {
        let (la, lb) = (if a.uses(0) { 0 } else { 1 }, if b.uses(0) { 0 } else { 1 });
        return a.ref_poc[la] != b.ref_poc[lb] || mv_far(a.mv[la], b.mv[lb]);
    }
    if na == 0 {
        return false;
    }
    let (a0, a1, b0, b1) = (a.ref_poc[0], a.ref_poc[1], b.ref_poc[0], b.ref_poc[1]);
    if !((a0 == b0 && a1 == b1) || (a0 == b1 && a1 == b0)) {
        return true;
    }
    if a0 != a1 {
        if a0 == b0 {
            mv_far(a.mv[0], b.mv[0]) || mv_far(a.mv[1], b.mv[1])
        } else {
            mv_far(a.mv[0], b.mv[1]) || mv_far(a.mv[1], b.mv[0])
        }
    } else {
        (mv_far(a.mv[0], b.mv[0]) || mv_far(a.mv[1], b.mv[1])) && (mv_far(a.mv[0], b.mv[1]) || mv_far(a.mv[1], b.mv[0]))
    }
}

fn chroma_qp(qpi: i32, chroma_format: u32) -> i32 {
    if chroma_format == 1 {
        if qpi < 30 {
            qpi
        } else if qpi >= 44 {
            qpi - 6
        } else {
            QP_C_TABLE[(qpi - 30) as usize] as i32
        }
    } else {
        qpi.min(51)
    }
}

/// Filters one 4-line luma edge segment. `pos` is the index of the first q0 sample, `across` the
/// step from p0 to q0 and `along` the step between lines.
#[allow(clippy::too_many_arguments)]
fn luma_segment(p: &mut [u16], pos: usize, across: isize, along: isize, bs: u8, qpl: i32, beta_off: i32, tc_off: i32, bd: u32, no_p: bool, no_q: bool) {
    let q = (qpl + (beta_off << 1)).clamp(0, 51);
    let beta = (BETA_TABLE[q as usize] as i32) << (bd - 8);
    let q = (qpl + 2 * (bs as i32 - 1) + (tc_off << 1)).clamp(0, 53);
    let tc = (TC_TABLE[q as usize] as i32) << (bd - 8);
    let at = |p: &[u16], line: isize, k: isize| p[(pos as isize + line * along + k * across) as usize] as i32;
    let dp = |p: &[u16], l: isize| (at(p, l, -3) - 2 * at(p, l, -2) + at(p, l, -1)).abs();
    let dq = |p: &[u16], l: isize| (at(p, l, 2) - 2 * at(p, l, 1) + at(p, l, 0)).abs();
    let (dp0, dp3, dq0, dq3) = (dp(p, 0), dp(p, 3), dq(p, 0), dq(p, 3));
    let (dpq0, dpq3) = (dp0 + dq0, dp3 + dq3);
    let (dpt, dqt) = (dp0 + dp3, dq0 + dq3);
    if dpq0 + dpq3 >= beta {
        return;
    }
    let strong_line = |p: &[u16], l: isize, dpq: i32| -> bool {
        2 * dpq < (beta >> 2) && (at(p, l, -4) - at(p, l, -1)).abs() + (at(p, l, 0) - at(p, l, 3)).abs() < (beta >> 3) && (at(p, l, -1) - at(p, l, 0)).abs() < ((5 * tc + 1) >> 1)
    };
    let strong = strong_line(p, 0, dpq0) && strong_line(p, 3, dpq3);
    let side = (beta + (beta >> 1)) >> 3;
    let (de_p, de_q) = (dpt < side, dqt < side);
    let maxv = (1i32 << bd) - 1;
    for l in 0..4isize {
        let base = pos as isize + l * along;
        let ix = |k: isize| (base + k * across) as usize;
        let (p0, p1, p2, p3) = (p[ix(-1)] as i32, p[ix(-2)] as i32, p[ix(-3)] as i32, p[ix(-4)] as i32);
        let (q0, q1, q2, q3) = (p[ix(0)] as i32, p[ix(1)] as i32, p[ix(2)] as i32, p[ix(3)] as i32);
        if strong {
            if !no_p {
                p[ix(-1)] = ((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3).clamp(p0 - 2 * tc, p0 + 2 * tc) as u16;
                p[ix(-2)] = ((p2 + p1 + p0 + q0 + 2) >> 2).clamp(p1 - 2 * tc, p1 + 2 * tc) as u16;
                p[ix(-3)] = ((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3).clamp(p2 - 2 * tc, p2 + 2 * tc) as u16;
            }
            if !no_q {
                p[ix(0)] = ((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3).clamp(q0 - 2 * tc, q0 + 2 * tc) as u16;
                p[ix(1)] = ((p0 + q0 + q1 + q2 + 2) >> 2).clamp(q1 - 2 * tc, q1 + 2 * tc) as u16;
                p[ix(2)] = ((p0 + q0 + q1 + 3 * q2 + 2 * q3 + 4) >> 3).clamp(q2 - 2 * tc, q2 + 2 * tc) as u16;
            }
        } else {
            let mut delta = (9 * (q0 - p0) - 3 * (q1 - p1) + 8) >> 4;
            if delta.abs() >= tc * 10 {
                continue;
            }
            delta = delta.clamp(-tc, tc);
            if !no_p {
                p[ix(-1)] = (p0 + delta).clamp(0, maxv) as u16;
                if de_p {
                    let dp = ((((p2 + p0 + 1) >> 1) - p1 + delta) >> 1).clamp(-(tc >> 1), tc >> 1);
                    p[ix(-2)] = (p1 + dp).clamp(0, maxv) as u16;
                }
            }
            if !no_q {
                p[ix(0)] = (q0 - delta).clamp(0, maxv) as u16;
                if de_q {
                    let dq = ((((q2 + q0 + 1) >> 1) - q1 - delta) >> 1).clamp(-(tc >> 1), tc >> 1);
                    p[ix(1)] = (q1 + dq).clamp(0, maxv) as u16;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn chroma_segment(p: &mut [u16], pos: usize, across: isize, along: isize, lines: isize, qpc: i32, tc_off: i32, bd: u32, no_p: bool, no_q: bool) {
    let q = (qpc + 2 + (tc_off << 1)).clamp(0, 53);
    let tc = (TC_TABLE[q as usize] as i32) << (bd - 8);
    let maxv = (1i32 << bd) - 1;
    for l in 0..lines {
        let base = pos as isize + l * along;
        let ix = |k: isize| (base + k * across) as usize;
        let (p0, p1, q0, q1) = (p[ix(-1)] as i32, p[ix(-2)] as i32, p[ix(0)] as i32, p[ix(1)] as i32);
        let delta = ((((q0 - p0) << 2) + p1 - q1 + 4) >> 3).clamp(-tc, tc);
        if !no_p {
            p[ix(-1)] = (p0 + delta).clamp(0, maxv) as u16;
        }
        if !no_q {
            p[ix(0)] = (q0 - delta).clamp(0, maxv) as u16;
        }
    }
}

fn deblock(pic: &mut CurPic, sps: &Sps, pps: &Pps, geo: &Geometry) {
    let (w4, h4) = (pic.w4, pic.h4);
    let l = sps.log2_ctb;
    let (bd_y, bd_c) = (sps.bit_depth_luma, sps.bit_depth_chroma);
    let chroma = sps.chroma_format_idc != 0;
    let (sx, sy) = sps.chroma_shift();
    let (width, cwidth) = (pic.frame.width, pic.frame.cwidth);
    for vertical in [true, false] {
        for y4 in 0..h4 {
            for x4 in 0..w4 {
                let (x, y) = (x4 * 4, y4 * 4);
                let on_grid = if vertical { x % 8 == 0 && x > 0 } else { y % 8 == 0 && y > 0 };
                if !on_grid {
                    continue;
                }
                let iq = y4 * w4 + x4;
                let flags = pic.edges[iq];
                let (tu_flag, pu_flag) = if vertical { (EDGE_TU_V, EDGE_PU_V) } else { (EDGE_TU_H, EDGE_PU_H) };
                if flags & (tu_flag | pu_flag) == 0 {
                    continue;
                }
                let ip = if vertical { iq - 1 } else { iq - w4 };
                let (xp, yp) = if vertical { (x - 1, y) } else { (x, y - 1) };
                let crs_q = (y >> l) * sps.ctb_w + (x >> l);
                let crs_p = (yp >> l) * sps.ctb_w + (xp >> l);
                let sq = pic.slices[pic.ctb_slice_idx[crs_q] as usize].clone();
                if sq.deblocking_disabled {
                    continue;
                }
                if crs_p != crs_q {
                    if pic.ctb_slice_addr[crs_p] != pic.ctb_slice_addr[crs_q] && !sq.loop_filter_across_slices {
                        continue;
                    }
                    if !pps.loop_filter_across_tiles && geo.tile_id[geo.rs_to_ts[crs_p]] != geo.tile_id[geo.rs_to_ts[crs_q]] {
                        continue;
                    }
                }
                let bs = if pic.pred_mode[ip] == PRED_INTRA || pic.pred_mode[iq] == PRED_INTRA {
                    2
                } else if flags & tu_flag != 0 && (pic.nz[ip] != 0 || pic.nz[iq] != 0) {
                    1
                } else if motion_differs(&pic.frame.mv[ip], &pic.frame.mv[iq]) {
                    1
                } else {
                    0
                };
                if bs == 0 {
                    continue;
                }
                let (no_p, no_q) = (pic.nofilter[ip] != 0, pic.nofilter[iq] != 0);
                let qpl = (pic.qp[ip] as i32 + pic.qp[iq] as i32 + 1) >> 1;
                let (across, along) = if vertical { (1isize, width as isize) } else { (width as isize, 1isize) };
                luma_segment(&mut pic.frame.planes[0], y * width + x, across, along, bs, qpl, sq.beta_offset_div2, sq.tc_offset_div2, bd_y, no_p, no_q);
                if bs == 2 && chroma {
                    // chroma edges lie on an 8-sample chroma grid; this luma segment covers 4 luma lines of it
                    let on_chroma_grid = if vertical { x % (8 << sx) == 0 } else { y % (8 << sy) == 0 };
                    if on_chroma_grid {
                        let (across, along) = if vertical { (1isize, cwidth as isize) } else { (cwidth as isize, 1isize) };
                        let lines = 4 >> if vertical { sy } else { sx };
                        for c in 1..3 {
                            let off = if c == 1 { pps.cb_qp_offset } else { pps.cr_qp_offset };
                            let qpc = chroma_qp(qpl + off, sps.chroma_format_idc);
                            chroma_segment(&mut pic.frame.planes[c], (y >> sy) * cwidth + (x >> sx), across, along, lines, qpc, sq.tc_offset_div2, bd_c, no_p, no_q);
                        }
                    }
                }
            }
        }
    }
}

fn sao(pic: &mut CurPic, sps: &Sps, pps: &Pps, geo: &Geometry) {
    let ncomp = if sps.chroma_format_idc == 0 { 1 } else { 3 };
    let l = sps.log2_ctb;
    for c in 0..ncomp {
        if pic.sao.iter().all(|s| s.kind[c] == 0) {
            continue;
        }
        let (sx, sy) = if c == 0 { (0, 0) } else { sps.chroma_shift() };
        let (pw, ph) = if c == 0 { (pic.frame.width, pic.frame.height) } else { (pic.frame.cwidth, pic.frame.cheight) };
        let bd = if c == 0 { sps.bit_depth_luma } else { sps.bit_depth_chroma };
        let src = pic.frame.planes[c].clone();
        let maxv = (1i32 << bd) - 1;
        // log2_sao_offset_scale: 0 unless the PPS range extension sets it, which is refused. (The first
        // edition's `bitDepth - Min(bitDepth, 10)` predates the profiles that allow 12 bits.)
        let scale = 0;
        let (cw_ctb, ch_ctb) = ((1usize << l) >> sx, (1usize << l) >> sy);
        for ry in 0..sps.ctb_h {
            for rx in 0..sps.ctb_w {
                let rs = ry * sps.ctb_w + rx;
                let s = pic.sao[rs];
                if s.kind[c] == 0 {
                    continue;
                }
                let (x0, y0) = (rx * cw_ctb, ry * ch_ctb);
                let (x1, y1) = ((x0 + cw_ctb).min(pw), (y0 + ch_ctb).min(ph));
                let offs: [i32; 4] = std::array::from_fn(|i| (s.offset[c][i] as i32) << scale);
                if s.kind[c] == 1 {
                    let mut table = [0i32; 32];
                    for k in 0..4 {
                        table[(k + s.band_pos[c] as usize) & 31] = offs[k];
                    }
                    let shift = bd - 5;
                    for y in y0..y1 {
                        for x in x0..x1 {
                            if pic.nofilter[((y << sy) >> 2) * pic.w4 + ((x << sx) >> 2)] != 0 {
                                continue;
                            }
                            let v = src[y * pw + x] as i32;
                            pic.frame.planes[c][y * pw + x] = (v + table[(v >> shift) as usize]).clamp(0, maxv) as u16;
                        }
                    }
                } else {
                    let (dx, dy): ((isize, isize), (isize, isize)) = match s.eo_class[c] {
                        0 => ((-1, 0), (1, 0)),
                        1 => ((0, -1), (0, 1)),
                        2 => ((-1, -1), (1, 1)),
                        _ => ((1, -1), (-1, 1)),
                    };
                    let cur_ts = geo.rs_to_ts[rs];
                    let cur_slice = pic.ctb_slice_addr[rs];
                    let cur_across = pic.slices[pic.ctb_slice_idx[rs] as usize].loop_filter_across_slices;
                    for y in y0..y1 {
                        for x in x0..x1 {
                            if pic.nofilter[((y << sy) >> 2) * pic.w4 + ((x << sx) >> 2)] != 0 {
                                continue;
                            }
                            let v = src[y * pw + x] as i32;
                            let mut sum = 2;
                            let mut ok = true;
                            for (ox, oy) in [dx, dy] {
                                let (nx, ny) = (x as isize + ox, y as isize + oy);
                                if nx < 0 || ny < 0 || nx as usize >= pw || ny as usize >= ph {
                                    ok = false;
                                    break;
                                }
                                let (nx, ny) = (nx as usize, ny as usize);
                                let nrs = (((ny << sy) >> l) * sps.ctb_w) + ((nx << sx) >> l);
                                if nrs != rs {
                                    let n_ts = geo.rs_to_ts[nrs];
                                    if pic.ctb_slice_addr[nrs] != cur_slice {
                                        let n_across = pic.slices[pic.ctb_slice_idx[nrs] as usize].loop_filter_across_slices;
                                        if (n_ts < cur_ts && !cur_across) || (cur_ts < n_ts && !n_across) {
                                            ok = false;
                                            break;
                                        }
                                    }
                                    if !pps.loop_filter_across_tiles && geo.tile_id[n_ts] != geo.tile_id[cur_ts] {
                                        ok = false;
                                        break;
                                    }
                                }
                                let nv = src[ny * pw + nx] as i32;
                                sum += (v - nv).signum();
                            }
                            if !ok {
                                continue;
                            }
                            let o = match sum {
                                0 => offs[0],
                                1 => offs[1],
                                3 => offs[2],
                                4 => offs[3],
                                _ => 0,
                            };
                            pic.frame.planes[c][y * pw + x] = (v + o).clamp(0, maxv) as u16;
                        }
                    }
                }
            }
        }
    }
}
