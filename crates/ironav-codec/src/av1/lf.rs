//! The deblocking loop filter (spec 7.14), for intra frames.

use super::block::FrameDec;
use super::ipred::Plane;
use super::tables::{TX_H_LOG2, TX_W_LOG2};

pub fn deblock(f: &mut FrameDec) {
    for plane in 0..f.num_planes {
        if plane > 0 && f.h.lf_level[plane + 1] == 0 {
            continue;
        }
        let (sx, sy) = if plane == 0 { (0, 0) } else { (f.ss[0], f.ss[1]) };
        for pass in 0..2 {
            for row in (0..f.h.mi_rows).step_by(1 << sy) {
                for col in (0..f.h.mi_cols).step_by(1 << sx) {
                    edge(f, plane, pass, row, col, sx, sy);
                }
            }
        }
    }
}

/// edge_loop_filter: the edge on the left (pass 0) or top (pass 1) of the 4x4 at (`row`, `col`).
fn edge(f: &mut FrameDec, plane: usize, pass: usize, row: usize, col: usize, sx: usize, sy: usize) {
    let (x, y) = (col * 4, row * 4);
    if x >= f.h.width as usize || y >= f.h.height as usize || (pass == 0 && x == 0) || (pass == 1 && y == 0) {
        return;
    }
    let (xp, yp) = (x >> sx, y >> sy);
    let (row, col) = (row | sy, col | sx);
    let (prow, pcol) = if pass == 0 { (row, col - (1 << sx)) } else { (row - (1 << sy), col) };
    let ls = f.lf_stride[plane];
    let tx = f.lf_tx[plane][(row >> sy) * ls + (col >> sx)] as usize;
    let ptx = f.lf_tx[plane][(prow >> sy) * ls + (pcol >> sx)] as usize;
    // Every block is intra, so every transform edge is filtered (skip only matters for inter blocks).
    let (tl, ptl) = if pass == 0 { (TX_W_LOG2[tx], TX_W_LOG2[ptx]) } else { (TX_H_LOG2[tx], TX_H_LOG2[ptx]) };
    if (if pass == 0 { xp } else { yp }) & ((1 << tl) - 1) != 0 {
        return;
    }
    let size = (1usize << tl.min(ptl)).min(if plane == 0 { 16 } else { 8 });
    let mut lvl = level(f, row, col, plane, pass);
    if lvl == 0 {
        lvl = level(f, prow, pcol, plane, pass);
    }
    if lvl == 0 {
        return;
    }
    let sh = f.h.lf_sharpness as i32;
    let shift = if sh > 4 { 2 } else if sh > 0 { 1 } else { 0 };
    let limit = if sh > 0 { (lvl >> shift).clamp(1, 9 - sh) } else { (lvl >> shift).max(1) };
    let bd = f.bd;
    let lim = [limit << (bd - 8), (2 * (lvl + 2) + limit) << (bd - 8), (lvl >> 4) << (bd - 8)];
    let p = &mut f.planes[plane];
    for i in 0..4 {
        let (px, py) = if pass == 0 { (xp, yp + i) } else { (xp + i, yp) };
        filter(p, px, py, pass == 0, size, plane > 0, lim, bd);
    }
}

/// adaptive_filter_strength_selection for an intra block.
fn level(f: &FrameDec, row: usize, col: usize, plane: usize, pass: usize) -> i32 {
    let h = &f.h;
    let at = row * f.mi_stride + col;
    let i = if plane == 0 { pass } else { plane + 1 };
    let dlf = f.delta_lf.get(at).map_or(0, |d| if h.delta_lf_multi { d[i] } else { d[0] } as i32);
    let mut lvl = (dlf + h.lf_level[i] as i32).clamp(0, 63);
    let seg = f.mi[at].seg as usize;
    if h.seg_enabled && h.feature_enabled[seg][1 + i] {
        lvl = (lvl + h.feature_data[seg][1 + i]).clamp(0, 63);
    }
    if h.lf_delta_enabled {
        lvl = (lvl + h.lf_ref_deltas[0] * (1 << (lvl >> 5))).clamp(0, 63);
    }
    lvl
}

/// sample_filtering across the edge just before (`x`, `y`); `lim` is limit, blimit and thresh.
#[allow(clippy::too_many_arguments)]
fn filter(p: &mut Plane, x: usize, y: usize, vertical: bool, size: usize, chroma: bool, lim: [i32; 3], bd: u32) {
    let step = if vertical { 1 } else { p.stride as isize };
    let base = (y * p.stride + x) as isize;
    let len = if size == 4 { 4 } else if chroma { 6 } else if size == 8 { 8 } else { 16 };
    let n = match len {
        4 => 2,
        6 => 3,
        8 => 4,
        _ => 7,
    };
    // s[8 + k] is the sample k across the edge: q_k for k >= 0, p_(-k-1) for k < 0.
    let mut s = [0i32; 16];
    for k in -n..n {
        s[(8 + k) as usize] = p.data[(base + k * step) as usize] as i32;
    }
    let ad = |a: usize, b: usize| (s[a] - s[b]).abs();
    let [limit, blimit, thresh] = lim;
    let hev = ad(6, 7) > thresh || ad(9, 8) > thresh;
    let mut mask = ad(6, 7) > limit || ad(9, 8) > limit || ad(7, 8) * 2 + ad(6, 9) / 2 > blimit;
    if len >= 6 {
        mask |= ad(5, 6) > limit || ad(10, 9) > limit;
    }
    if len >= 8 {
        mask |= ad(4, 5) > limit || ad(11, 10) > limit;
    }
    if mask {
        return;
    }
    let one = 1 << (bd - 8);
    let flat_to = |k: usize| ad(7 - k, 7) <= one && ad(8 + k, 8) <= one;
    let flat = size >= 8 && (1..if len == 6 { 3 } else { 4 }).all(flat_to);
    let flat2 = size >= 16 && flat && (4..7).all(flat_to);
    let mut put = |k: isize, v: i32| p.data[(base + k * step) as usize] = v as u16;
    if size == 4 || !flat {
        let off = 0x80 << (bd - 8);
        let c = |v: i32| v.clamp(-(1 << (bd - 1)), (1 << (bd - 1)) - 1);
        let (ps1, ps0, qs0, qs1) = (s[6] - off, s[7] - off, s[8] - off, s[9] - off);
        let mut fl = if hev { c(ps1 - qs1) } else { 0 };
        fl = c(fl + 3 * (qs0 - ps0));
        let f1 = c(fl + 4) >> 3;
        let f2 = c(fl + 3) >> 3;
        put(0, c(qs0 - f1) + off);
        put(-1, c(ps0 + f2) + off);
        if !hev {
            let fl = (f1 + 1) >> 1;
            put(1, c(qs1 - fl) + off);
            put(-2, c(ps1 + fl) + off);
        }
        return;
    }
    let log2 = if size == 8 || !flat2 { 3 } else { 4 };
    let n: isize = if log2 == 4 { 6 } else if chroma { 2 } else { 3 };
    let n2 = if log2 == 3 && !chroma { 0 } else { 1 };
    let mut out = [0i32; 12];
    for i in -n..n {
        let mut t = 0;
        for j in -n..=n {
            let k = (i + j).clamp(-(n + 1), n);
            t += s[(8 + k) as usize] * if j.abs() <= n2 { 2 } else { 1 };
        }
        out[(i + n) as usize] = (t + (1 << (log2 - 1))) >> log2;
    }
    for i in -n..n {
        put(i, out[(i + n) as usize]);
    }
}
