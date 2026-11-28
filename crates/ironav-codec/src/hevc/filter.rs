//! In-loop filters: the deblocking filter (ITU-T H.265 8.7.2) and sample adaptive offset (8.7.3).

use super::frame::*;
use super::pool::{Pool, ScopedJob};
use super::ps::{Pps, Sps};
use super::tables::{BETA_TABLE, QP_C_TABLE, TC_TABLE};

/// Runs the loop filters over a decoded picture, on the threads of `pool` if there is one.
pub(super) fn run(pic: &mut CurPic, sps: &Sps, pps: &Pps, geo: &Geometry, pool: Option<&Pool>) {
    if pic.slices.iter().any(|s| !s.deblocking_disabled) {
        deblock(pic, sps, pps, geo, pool);
    }
    if sps.sao && pic.sao.iter().any(|s| s.kind != [0; 3]) {
        sao(pic, sps, pps, geo, pool);
    }
}

/// How many bands of a multiple of `unit` rows to cut `rows` rows into for the threads of `pool`, and how
/// many rows each has.
fn bands(rows: usize, unit: usize, pool: Option<&Pool>) -> (usize, usize) {
    // a band is at least two units, or handing it to a thread costs more than it saves
    let per = rows.div_ceil(pool.map_or(1, |p| p.width())).max(unit * 2).div_ceil(unit) * unit;
    (rows.div_ceil(per), per)
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
    // the eight samples p3 p2 p1 p0 q0 q1 q2 q3 of each of the four lines
    let mut s = [[0i32; 8]; 4];
    for (l, line) in s.iter_mut().enumerate() {
        let base = pos as isize + l as isize * along - 4 * across;
        for (k, v) in line.iter_mut().enumerate() {
            *v = p[(base + k as isize * across) as usize] as i32;
        }
    }
    let dp = |l: &[i32; 8]| (l[1] - 2 * l[2] + l[3]).abs();
    let dq = |l: &[i32; 8]| (l[6] - 2 * l[5] + l[4]).abs();
    let (dp0, dp3, dq0, dq3) = (dp(&s[0]), dp(&s[3]), dq(&s[0]), dq(&s[3]));
    let (dpq0, dpq3) = (dp0 + dq0, dp3 + dq3);
    let (dpt, dqt) = (dp0 + dp3, dq0 + dq3);
    if dpq0 + dpq3 >= beta {
        return;
    }
    let strong_line = |l: &[i32; 8], dpq: i32| -> bool { 2 * dpq < (beta >> 2) && (l[0] - l[3]).abs() + (l[4] - l[7]).abs() < (beta >> 3) && (l[3] - l[4]).abs() < ((5 * tc + 1) >> 1) };
    let strong = strong_line(&s[0], dpq0) && strong_line(&s[3], dpq3);
    let side = (beta + (beta >> 1)) >> 3;
    let (de_p, de_q) = (dpt < side, dqt < side);
    let maxv = (1i32 << bd) - 1;
    for (l, line) in s.iter().enumerate() {
        let base = pos as isize + l as isize * along;
        let mut put = |k: isize, v: i32| p[(base + k * across) as usize] = v as u16;
        let [p3, p2, p1, p0, q0, q1, q2, q3] = *line;
        if strong {
            if !no_p {
                put(-1, ((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3).clamp(p0 - 2 * tc, p0 + 2 * tc));
                put(-2, ((p2 + p1 + p0 + q0 + 2) >> 2).clamp(p1 - 2 * tc, p1 + 2 * tc));
                put(-3, ((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3).clamp(p2 - 2 * tc, p2 + 2 * tc));
            }
            if !no_q {
                put(0, ((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3).clamp(q0 - 2 * tc, q0 + 2 * tc));
                put(1, ((p0 + q0 + q1 + q2 + 2) >> 2).clamp(q1 - 2 * tc, q1 + 2 * tc));
                put(2, ((p0 + q0 + q1 + 3 * q2 + 2 * q3 + 4) >> 3).clamp(q2 - 2 * tc, q2 + 2 * tc));
            }
        } else {
            let mut delta = (9 * (q0 - p0) - 3 * (q1 - p1) + 8) >> 4;
            if delta.abs() >= tc * 10 {
                continue;
            }
            delta = delta.clamp(-tc, tc);
            if !no_p {
                put(-1, (p0 + delta).clamp(0, maxv));
                if de_p {
                    let dp = ((((p2 + p0 + 1) >> 1) - p1 + delta) >> 1).clamp(-(tc >> 1), tc >> 1);
                    put(-2, (p1 + dp).clamp(0, maxv));
                }
            }
            if !no_q {
                put(0, (q0 - delta).clamp(0, maxv));
                if de_q {
                    let dq = ((((q2 + q0 + 1) >> 1) - q1 - delta) >> 1).clamp(-(tc >> 1), tc >> 1);
                    put(1, (q1 + dq).clamp(0, maxv));
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

/// What the deblocking filter reads about a picture besides its samples.
struct Deblock<'a> {
    sps: &'a Sps,
    pps: &'a Pps,
    geo: &'a Geometry,
    edges: &'a [u8],
    pred_mode: &'a [u8],
    nz: &'a [u8],
    nofilter: &'a [u8],
    qp: &'a [i8],
    mv: &'a [MvField],
    slices: &'a [SliceInfo],
    ctb_slice_idx: &'a [u16],
    ctb_slice_addr: &'a [i32],
    w4: usize,
    width: usize,
    cwidth: usize,
}

impl Deblock<'_> {
    /// Filters the edges of one direction whose second block lies in rows `y4s` of the 4x4 grid. `planes`
    /// hold the samples from luma row `row0` and chroma row `crow0` on.
    fn pass(&self, vertical: bool, y4s: std::ops::Range<usize>, planes: &mut [&mut [u16]; 3], row0: usize, crow0: usize) {
        let (sps, pps, geo) = (self.sps, self.pps, self.geo);
        let (w4, l) = (self.w4, sps.log2_ctb);
        let (bd_y, bd_c) = (sps.bit_depth_luma, sps.bit_depth_chroma);
        let chroma = sps.chroma_format_idc != 0;
        let (sx, sy) = sps.chroma_shift();
        let (width, cwidth) = (self.width, self.cwidth);
        for y4 in y4s {
            for x4 in 0..w4 {
                let (x, y) = (x4 * 4, y4 * 4);
                let on_grid = if vertical { x % 8 == 0 && x > 0 } else { y % 8 == 0 && y > 0 };
                if !on_grid {
                    continue;
                }
                let iq = y4 * w4 + x4;
                let flags = self.edges[iq];
                let (tu_flag, pu_flag) = if vertical { (EDGE_TU_V, EDGE_PU_V) } else { (EDGE_TU_H, EDGE_PU_H) };
                if flags & (tu_flag | pu_flag) == 0 {
                    continue;
                }
                let ip = if vertical { iq - 1 } else { iq - w4 };
                let (xp, yp) = if vertical { (x - 1, y) } else { (x, y - 1) };
                let crs_q = (y >> l) * sps.ctb_w + (x >> l);
                let crs_p = (yp >> l) * sps.ctb_w + (xp >> l);
                let sq = &self.slices[self.ctb_slice_idx[crs_q] as usize];
                if sq.deblocking_disabled {
                    continue;
                }
                if crs_p != crs_q {
                    if self.ctb_slice_addr[crs_p] != self.ctb_slice_addr[crs_q] && !sq.loop_filter_across_slices {
                        continue;
                    }
                    if !pps.loop_filter_across_tiles && geo.tile_id[geo.rs_to_ts[crs_p]] != geo.tile_id[geo.rs_to_ts[crs_q]] {
                        continue;
                    }
                }
                let bs = if self.pred_mode[ip] == PRED_INTRA || self.pred_mode[iq] == PRED_INTRA {
                    2
                } else if (flags & tu_flag != 0 && (self.nz[ip] != 0 || self.nz[iq] != 0)) || motion_differs(&self.mv[ip], &self.mv[iq]) {
                    1
                } else {
                    0
                };
                if bs == 0 {
                    continue;
                }
                let (no_p, no_q) = (self.nofilter[ip] != 0, self.nofilter[iq] != 0);
                let qpl = (self.qp[ip] as i32 + self.qp[iq] as i32 + 1) >> 1;
                let (across, along) = if vertical { (1isize, width as isize) } else { (width as isize, 1isize) };
                luma_segment(planes[0], (y - row0) * width + x, across, along, bs, qpl, sq.beta_offset_div2, sq.tc_offset_div2, bd_y, no_p, no_q);
                if bs == 2 && chroma {
                    // chroma edges lie on an 8-sample chroma grid; this luma segment covers 4 luma lines of it
                    let on_chroma_grid = if vertical { x % (8 << sx) == 0 } else { y % (8 << sy) == 0 };
                    if on_chroma_grid {
                        let (across, along) = if vertical { (1isize, cwidth as isize) } else { (cwidth as isize, 1isize) };
                        let lines = 4 >> if vertical { sy } else { sx };
                        for c in 1..3 {
                            let off = if c == 1 { pps.cb_qp_offset } else { pps.cr_qp_offset };
                            let qpc = chroma_qp(qpl + off, sps.chroma_format_idc);
                            chroma_segment(planes[c], ((y >> sy) - crow0) * cwidth + (x >> sx), across, along, lines, qpc, sq.tc_offset_div2, bd_c, no_p, no_q);
                        }
                    }
                }
            }
        }
    }
}

/// Cuts a plane into consecutive chunks, chunk k starting at sample `starts[k]` (`starts[0]` is 0).
fn split_chunks<'p>(mut plane: &'p mut [u16], starts: &[usize]) -> Vec<&'p mut [u16]> {
    let mut out = Vec::with_capacity(starts.len());
    let mut taken = 0;
    for k in 0..starts.len() {
        let end = if k + 1 < starts.len() { starts[k + 1].min(taken + plane.len()) } else { taken + plane.len() };
        let (a, b) = std::mem::take(&mut plane).split_at_mut(end - taken);
        taken = end;
        plane = b;
        out.push(a);
    }
    out
}

pub(super) fn deblock(pic: &mut CurPic, sps: &Sps, pps: &Pps, geo: &Geometry, pool: Option<&Pool>) {
    let (w4, h4) = (pic.w4, pic.h4);
    let (_, sy) = sps.chroma_shift();
    let CurPic { frame, edges, pred_mode, nz, nofilter, qp, slices, ctb_slice_idx, ctb_slice_addr, .. } = pic;
    let [pl_y, pl_cb, pl_cr] = &mut frame.planes;
    let (width, cwidth) = (frame.width, frame.cwidth);
    let db = Deblock { sps, pps, geo, edges, pred_mode, nz, nofilter, qp, mv: &frame.mv, slices, ctb_slice_idx, ctb_slice_addr, w4, width, cwidth };
    // Bands of `per` luma rows (a multiple of 16, so of 8 chroma rows). The vertical edges of a band touch
    // only its own rows. A horizontal edge touches the 4 rows above it and the 4 below, but changes 3 at
    // most on each side and they are 8 rows apart, so a band that starts 4 rows above its first edge
    // does not overlap its neighbours.
    let (nb, per) = bands(h4 * 4, 16, pool);
    for vertical in [true, false] {
        if nb <= 1 {
            db.pass(vertical, 0..h4, &mut [&mut pl_y[..], &mut pl_cb[..], &mut pl_cr[..]], 0, 0);
            continue;
        }
        let first_row = |k: usize, shift: usize| -> usize {
            let r = (k * per) >> shift;
            if k == 0 || vertical {
                r
            } else {
                r - 4
            }
        };
        let starts = |shift: usize, stride: usize| -> Vec<usize> { (0..nb).map(|k| first_row(k, shift) * stride).collect() };
        let (cy, ccb, ccr) = (split_chunks(pl_y, &starts(0, width)), split_chunks(pl_cb, &starts(sy, cwidth)), split_chunks(pl_cr, &starts(sy, cwidth)));
        let db = &db;
        let jobs: Vec<ScopedJob> = cy
            .into_iter()
            .zip(ccb)
            .zip(ccr)
            .enumerate()
            .map(|(k, ((a, b), c))| -> ScopedJob {
                let (row0, crow0) = (first_row(k, 0), first_row(k, sy));
                let rows = k * per / 4..((k + 1) * per / 4).min(h4);
                Box::new(move || db.pass(vertical, rows, &mut [a, b, c], row0, crow0))
            })
            .collect();
        pool.expect("more than one band means a pool").run(jobs);
    }
}

thread_local! {
    static SAO_SRC: std::cell::RefCell<Vec<u16>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub(super) fn sao(pic: &mut CurPic, sps: &Sps, pps: &Pps, geo: &Geometry, pool: Option<&Pool>) {
    let ncomp = if sps.chroma_format_idc == 0 { 1 } else { 3 };
    let l = sps.log2_ctb;
    for c in 0..ncomp {
        if pic.sao.iter().all(|s| s.kind[c] == 0) {
            continue;
        }
        let (sx, sy) = if c == 0 { (0, 0) } else { sps.chroma_shift() };
        let (pw, ph) = if c == 0 { (pic.frame.width, pic.frame.height) } else { (pic.frame.cwidth, pic.frame.cheight) };
        let bd = if c == 0 { sps.bit_depth_luma } else { sps.bit_depth_chroma };
        // the plane before SAO, in a buffer kept between pictures (a fresh 4 MB allocation costs page faults)
        let mut src = SAO_SRC.with(|b| std::mem::take(&mut *b.borrow_mut()));
        src.clear();
        src.extend_from_slice(&pic.frame.planes[c]);
        // without PCM or transquant bypass blocks no sample is exempt, and whole rows can be filtered at once
        let no_bypass = !pic.nofilter.iter().any(|&v| v != 0);
        let maxv = (1i32 << bd) - 1;
        if no_bypass {
            sao_plane(pic, sps, pps, geo, c, &src, pool);
            SAO_SRC.with(|b| *b.borrow_mut() = src);
            continue;
        }
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
        SAO_SRC.with(|b| *b.borrow_mut() = src);
    }
}

/// Applies the band offset to a row of samples: `dst[i]` is `cur[i]` plus the offset of its band.
#[inline(always)]
fn sao_band_row(dst: &mut [u16], cur: &[u16], offs: [i32; 4], band_pos: i32, shift: u32, maxv: i32) {
    for (d, &c) in dst.iter_mut().zip(cur) {
        let v = c as i32;
        let k = ((v >> shift) - band_pos) & 31;
        // at most one of the masks is set, so the or picks that band's offset
        let o = (-((k == 0) as i32) & offs[0]) | (-((k == 1) as i32) & offs[1]) | (-((k == 2) as i32) & offs[2]) | (-((k == 3) as i32) & offs[3]);
        *d = (v + o).clamp(0, maxv) as u16;
    }
}

/// Applies the edge offset to a row of samples, comparing each with its neighbours `a` and `b`.
#[inline(always)]
fn sao_edge_row(dst: &mut [u16], cur: &[u16], a: &[u16], b: &[u16], offs: [i32; 4], maxv: i32) {
    for (((d, &c), &p), &q) in dst.iter_mut().zip(cur).zip(a).zip(b) {
        let v = c as i32;
        let sum = (v - p as i32).signum() + (v - q as i32).signum();
        let o = (-((sum == -2) as i32) & offs[0]) | (-((sum == -1) as i32) & offs[1]) | (-((sum == 1) as i32) & offs[2]) | (-((sum == 2) as i32) & offs[3]);
        *d = (v + o).clamp(0, maxv) as u16;
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    /// SAFETY: callers check that AVX2 is available.
    #[target_feature(enable = "avx2")]
    pub unsafe fn band_row(dst: &mut [u16], cur: &[u16], offs: [i32; 4], band_pos: i32, shift: u32, maxv: i32) {
        super::sao_band_row(dst, cur, offs, band_pos, shift, maxv)
    }

    /// SAFETY: callers check that AVX2 is available.
    #[target_feature(enable = "avx2")]
    pub unsafe fn edge_row(dst: &mut [u16], cur: &[u16], a: &[u16], b: &[u16], offs: [i32; 4], maxv: i32) {
        super::sao_edge_row(dst, cur, a, b, offs, maxv)
    }
}

fn band_row(dst: &mut [u16], cur: &[u16], offs: [i32; 4], band_pos: i32, shift: u32, maxv: i32) {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 was detected just above.
        return unsafe { x86::band_row(dst, cur, offs, band_pos, shift, maxv) };
    }
    sao_band_row(dst, cur, offs, band_pos, shift, maxv)
}

fn edge_row(dst: &mut [u16], cur: &[u16], a: &[u16], b: &[u16], offs: [i32; 4], maxv: i32) {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 was detected just above.
        return unsafe { x86::edge_row(dst, cur, a, b, offs, maxv) };
    }
    sao_edge_row(dst, cur, a, b, offs, maxv)
}

/// What SAO reads about a picture's CTBs besides the samples.
struct SaoMeta<'a> {
    sao: &'a [Sao],
    slices: &'a [SliceInfo],
    ctb_slice_addr: &'a [i32],
    ctb_slice_idx: &'a [u16],
}

/// SAO of one component for a picture with no exempt samples, with the CTB rows split over threads. `src`
/// is the plane before SAO.
fn sao_plane(pic: &mut CurPic, sps: &Sps, pps: &Pps, geo: &Geometry, c: usize, src: &[u16], pool: Option<&Pool>) {
    let (sx, sy) = if c == 0 { (0, 0) } else { sps.chroma_shift() };
    let (pw, ph) = if c == 0 { (pic.frame.width, pic.frame.height) } else { (pic.frame.cwidth, pic.frame.cheight) };
    let bd = if c == 0 { sps.bit_depth_luma } else { sps.bit_depth_chroma };
    let maxv = (1i32 << bd) - 1;
    let ch_ctb = (1usize << sps.log2_ctb) >> sy;
    let CurPic { frame, sao, slices, ctb_slice_addr, ctb_slice_idx, .. } = pic;
    let meta = SaoMeta { sao, slices, ctb_slice_addr, ctb_slice_idx };
    let (nb, per) = bands(sps.ctb_h, 1, pool);
    let starts: Vec<usize> = (0..nb).map(|k| k * per * ch_ctb * pw).collect();
    let chunks = split_chunks(&mut frame.planes[c], &starts);
    let rows = |dst: &mut [u16], k: usize| {
        let (r0, r1) = (k * per, ((k + 1) * per).min(sps.ctb_h));
        let row0 = r0 * ch_ctb;
        for ry in r0..r1 {
            for rx in 0..sps.ctb_w {
                let s = meta.sao[ry * sps.ctb_w + rx];
                if s.kind[c] == 0 {
                    continue;
                }
                let cw_ctb = (1usize << sps.log2_ctb) >> sx;
                let (x0, y0) = (rx * cw_ctb, ry * ch_ctb);
                let (x1, y1) = ((x0 + cw_ctb).min(pw), (y0 + ch_ctb).min(ph));
                // log2_sao_offset_scale is 0: the range extension that sets it is refused
                let offs: [i32; 4] = std::array::from_fn(|i| s.offset[c][i] as i32);
                sao_ctb(&meta, sps, pps, geo, c, rx, ry, (x0, y0, x1, y1), pw, src, dst, row0, offs, maxv, bd);
            }
        }
    };
    if nb <= 1 {
        for (k, dst) in chunks.into_iter().enumerate() {
            rows(dst, k);
        }
        return;
    }
    let rows = &rows;
    let jobs: Vec<ScopedJob> = chunks.into_iter().enumerate().map(|(k, dst)| -> ScopedJob { Box::new(move || rows(dst, k)) }).collect();
    pool.expect("more than one band means a pool").run(jobs);
}

/// Sample adaptive offset of one CTB of component `c`, for a picture with no exempt samples. `region` is
/// the CTB's (x0, y0, x1, y1) in samples of the plane and `src` the plane before SAO.
#[allow(clippy::too_many_arguments)]
fn sao_ctb(m: &SaoMeta, sps: &Sps, pps: &Pps, geo: &Geometry, c: usize, rx: usize, ry: usize, region: (usize, usize, usize, usize), pw: usize, src: &[u16], dst: &mut [u16], row0: usize, offs: [i32; 4], maxv: i32, bd: u32) {
    let (x0, y0, x1, y1) = region;
    let rs = ry * sps.ctb_w + rx;
    let s = m.sao[rs];
    if s.kind[c] == 1 {
        let shift = bd - 5;
        for y in y0..y1 {
            band_row(&mut dst[(y - row0) * pw + x0..(y - row0) * pw + x1], &src[y * pw + x0..y * pw + x1], offs, s.band_pos[c] as i32, shift, maxv);
        }
        return;
    }
    // the two neighbours of a sample, as (dx, dy) offsets
    let (na, nb): ((isize, isize), (isize, isize)) = match s.eo_class[c] {
        0 => ((-1, 0), (1, 0)),
        1 => ((0, -1), (0, 1)),
        2 => ((-1, -1), (1, 1)),
        _ => ((1, -1), (-1, 1)),
    };
    // whether the CTBs around this one (and the picture) let its samples use them: avail[row][column]
    let l = sps.log2_ctb;
    let (sx, sy) = if c == 0 { (0, 0) } else { sps.chroma_shift() };
    let (cw_ctb, ch_ctb) = ((1usize << l) >> sx, (1usize << l) >> sy);
    let cur_ts = geo.rs_to_ts[rs];
    let cur_slice = m.ctb_slice_addr[rs];
    let cur_across = m.slices[m.ctb_slice_idx[rs] as usize].loop_filter_across_slices;
    let mut avail = [[false; 3]; 3];
    for (j, row) in avail.iter_mut().enumerate() {
        for (i, a) in row.iter_mut().enumerate() {
            let (nx, ny) = (rx as isize + i as isize - 1, ry as isize + j as isize - 1);
            if nx < 0 || ny < 0 || nx as usize >= sps.ctb_w || ny as usize >= sps.ctb_h {
                continue;
            }
            // a CTB cut short by the picture edge has no neighbour to its right or below
            if (i == 2 && x1 != x0 + cw_ctb) || (j == 2 && y1 != y0 + ch_ctb) {
                continue;
            }
            let nrs = ny as usize * sps.ctb_w + nx as usize;
            let mut ok = true;
            if nrs != rs {
                let n_ts = geo.rs_to_ts[nrs];
                if m.ctb_slice_addr[nrs] != cur_slice {
                    let n_across = m.slices[m.ctb_slice_idx[nrs] as usize].loop_filter_across_slices;
                    if (n_ts < cur_ts && !cur_across) || (cur_ts < n_ts && !n_across) {
                        ok = false;
                    }
                }
                if !pps.loop_filter_across_tiles && geo.tile_id[n_ts] != geo.tile_id[cur_ts] {
                    ok = false;
                }
            }
            *a = ok;
        }
    }
    // 0 before the CTB, 1 inside, 2 after
    let cat = |v: isize, lo: usize, hi: usize| -> usize {
        if v < lo as isize {
            0
        } else if v >= hi as isize {
            2
        } else {
            1
        }
    };
    let off_of = |sum: i32| -> i32 {
        match sum {
            -2 => offs[0],
            -1 => offs[1],
            1 => offs[2],
            2 => offs[3],
            _ => 0,
        }
    };
    for y in y0..y1 {
        let (ay, by) = (y as isize + na.1, y as isize + nb.1);
        let (cya, cyb) = (cat(ay, y0, y1), cat(by, y0, y1));
        let one = |x: usize, dst: &mut [u16]| {
            let (ax, bx) = (x as isize + na.0, x as isize + nb.0);
            if avail[cya][cat(ax, x0, x1)] && avail[cyb][cat(bx, x0, x1)] {
                let v = src[y * pw + x] as i32;
                let sum = (v - src[ay as usize * pw + ax as usize] as i32).signum() + (v - src[by as usize * pw + bx as usize] as i32).signum();
                dst[(y - row0) * pw + x] = (v + off_of(sum)).clamp(0, maxv) as u16;
            }
        };
        // when no column needs a CTB that is out of bounds, the whole row goes at once
        let usable = |x: usize| avail[cya][cat(x as isize + na.0, x0, x1)] && avail[cyb][cat(x as isize + nb.0, x0, x1)];
        if usable(x0) && usable(x1 - 1) && avail[cya][1] && avail[cyb][1] {
            let n = x1 - x0;
            let a0 = ((ay as usize * pw) as isize + na.0 + x0 as isize) as usize;
            let b0 = ((by as usize * pw) as isize + nb.0 + x0 as isize) as usize;
            edge_row(&mut dst[(y - row0) * pw + x0..(y - row0) * pw + x1], &src[y * pw + x0..y * pw + x1], &src[a0..a0 + n], &src[b0..b0 + n], offs, maxv);
            continue;
        }
        // the first and last columns may reach into the CTBs beside this one; the columns between cannot
        let (mx0, mx1) = if x1 - x0 > 2 { (x0 + 1, x1 - 1) } else { (x1, x1) };
        for x in x0..mx0.min(x1) {
            one(x, dst);
        }
        if mx0 < mx1 {
            if avail[cya][1] && avail[cyb][1] {
                let n = mx1 - mx0;
                let a0 = (ay as usize * pw) as isize + na.0 + mx0 as isize;
                let b0 = (by as usize * pw) as isize + nb.0 + mx0 as isize;
                let (a0, b0) = (a0 as usize, b0 as usize);
                edge_row(&mut dst[(y - row0) * pw + mx0..(y - row0) * pw + mx1], &src[y * pw + mx0..y * pw + mx1], &src[a0..a0 + n], &src[b0..b0 + n], offs, maxv);
            }
            for x in mx1..x1 {
                one(x, dst);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dispatching row kernels (AVX2 where the processor has it) against the portable ones.
    #[test]
    fn sao_rows_match_the_scalar_code() {
        let mut seed = 7u32;
        let mut rng = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed >> 8
        };
        for bd in [8u32, 10, 12] {
            let maxv = (1i32 << bd) - 1;
            for n in [1usize, 3, 8, 15, 16, 17, 31, 62, 64] {
                for _ in 0..50 {
                    // flat areas make equal neighbours likely, wide ranges make the extremes
                    let m = if rng() % 2 == 0 { 4 } else { 1 << bd };
                    let mut px = || -> Vec<u16> { (0..n).map(|_| (rng() % m) as u16).collect() };
                    let (cur, a, b) = (px(), px(), px());
                    let offs = [(rng() % 8) as i32, (rng() % 8) as i32, -((rng() % 8) as i32), -((rng() % 8) as i32)];
                    let (mut d1, mut d2) = (vec![0u16; n], vec![0u16; n]);
                    edge_row(&mut d1, &cur, &a, &b, offs, maxv);
                    sao_edge_row(&mut d2, &cur, &a, &b, offs, maxv);
                    assert_eq!(d1, d2, "edge, {n} samples, {bd} bits");
                    let (band_pos, shift) = ((rng() % 32) as i32, bd - 5);
                    band_row(&mut d1, &cur, offs, band_pos, shift, maxv);
                    sao_band_row(&mut d2, &cur, offs, band_pos, shift, maxv);
                    assert_eq!(d1, d2, "band, {n} samples, {bd} bits");
                }
            }
        }
    }
}
