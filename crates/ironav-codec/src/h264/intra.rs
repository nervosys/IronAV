//! Intra sample prediction (ITU-T H.264 8.3).

pub const V_PRED: u8 = 0;
pub const H_PRED: u8 = 1;
pub const DC_PRED: u8 = 2;
pub const DDL_PRED: u8 = 3;
pub const DDR_PRED: u8 = 4;
pub const VR_PRED: u8 = 5;
pub const HD_PRED: u8 = 6;
pub const VL_PRED: u8 = 7;
pub const HU_PRED: u8 = 8;

fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Neighbouring samples of an NxN block. `top` holds 2N samples (the second half is the
/// above-right row, already substituted when it is unavailable), `left` N samples.
pub struct Edges {
    pub top: [i32; 16],
    pub left: [i32; 8],
    pub topleft: i32,
    pub have_top: bool,
    pub have_left: bool,
    pub have_topleft: bool,
}

/// Predicts a 4x4 or 8x8 block (`n` is 4 or 8) in one of the nine directional modes. Samples the
/// mode needs but the stream did not provide read as zero; callers reject those modes first.
pub fn predict_nxn(n: usize, mode: u8, e: &Edges, out: &mut [u8]) {
    let t = |i: i32| -> i32 {
        if i < 0 {
            e.topleft
        } else {
            e.top[i as usize]
        }
    };
    let l = |j: i32| -> i32 {
        if j < 0 {
            e.topleft
        } else {
            e.left[j as usize]
        }
    };
    let ni = n as i32;
    let shift = n.trailing_zeros();
    for y in 0..ni {
        for x in 0..ni {
            let v = match mode {
                V_PRED => t(x),
                H_PRED => l(y),
                DC_PRED => {
                    let st: i32 = (0..ni).map(t).sum();
                    let sl: i32 = (0..ni).map(l).sum();
                    match (e.have_top, e.have_left) {
                        (true, true) => (st + sl + ni) >> (shift + 1),
                        (false, true) => (sl + ni / 2) >> shift,
                        (true, false) => (st + ni / 2) >> shift,
                        (false, false) => 128,
                    }
                }
                DDL_PRED => {
                    if x == ni - 1 && y == ni - 1 {
                        (t(2 * ni - 2) + 3 * t(2 * ni - 1) + 2) >> 2
                    } else {
                        (t(x + y) + 2 * t(x + y + 1) + t(x + y + 2) + 2) >> 2
                    }
                }
                DDR_PRED => {
                    if x > y {
                        (t(x - y - 2) + 2 * t(x - y - 1) + t(x - y) + 2) >> 2
                    } else if x < y {
                        (l(y - x - 2) + 2 * l(y - x - 1) + l(y - x) + 2) >> 2
                    } else {
                        (t(0) + 2 * e.topleft + l(0) + 2) >> 2
                    }
                }
                VR_PRED => {
                    let z = 2 * x - y;
                    if z >= 0 && z % 2 == 0 {
                        (t(x - (y >> 1) - 1) + t(x - (y >> 1)) + 1) >> 1
                    } else if z >= 0 {
                        (t(x - (y >> 1) - 2) + 2 * t(x - (y >> 1) - 1) + t(x - (y >> 1)) + 2) >> 2
                    } else if z == -1 {
                        (l(0) + 2 * e.topleft + t(0) + 2) >> 2
                    } else {
                        (l(y - 2 * x - 1) + 2 * l(y - 2 * x - 2) + l(y - 2 * x - 3) + 2) >> 2
                    }
                }
                HD_PRED => {
                    let z = 2 * y - x;
                    if z >= 0 && z % 2 == 0 {
                        (l(y - (x >> 1) - 1) + l(y - (x >> 1)) + 1) >> 1
                    } else if z >= 0 {
                        (l(y - (x >> 1) - 2) + 2 * l(y - (x >> 1) - 1) + l(y - (x >> 1)) + 2) >> 2
                    } else if z == -1 {
                        (l(0) + 2 * e.topleft + t(0) + 2) >> 2
                    } else {
                        (t(x - 2 * y - 1) + 2 * t(x - 2 * y - 2) + t(x - 2 * y - 3) + 2) >> 2
                    }
                }
                VL_PRED => {
                    if y % 2 == 0 {
                        (t(x + (y >> 1)) + t(x + (y >> 1) + 1) + 1) >> 1
                    } else {
                        (t(x + (y >> 1)) + 2 * t(x + (y >> 1) + 1) + t(x + (y >> 1) + 2) + 2) >> 2
                    }
                }
                _ => {
                    // HU_PRED
                    let z = x + 2 * y;
                    let edge = 2 * ni - 3;
                    if z > edge {
                        l(ni - 1)
                    } else if z == edge {
                        (l(ni - 2) + 3 * l(ni - 1) + 2) >> 2
                    } else if z % 2 == 0 {
                        (l(y + (x >> 1)) + l(y + (x >> 1) + 1) + 1) >> 1
                    } else {
                        (l(y + (x >> 1)) + 2 * l(y + (x >> 1) + 1) + l(y + (x >> 1) + 2) + 2) >> 2
                    }
                }
            };
            out[(y * ni + x) as usize] = v as u8;
        }
    }
}

/// The reference-sample smoothing that precedes Intra 8x8 prediction (8.3.2.2.1).
pub fn filter_8x8_edges(e: &mut Edges) {
    let p = Edges { top: e.top, left: e.left, topleft: e.topleft, have_top: e.have_top, have_left: e.have_left, have_topleft: e.have_topleft };
    if p.have_top {
        e.top[0] = if p.have_topleft { (p.topleft + 2 * p.top[0] + p.top[1] + 2) >> 2 } else { (3 * p.top[0] + p.top[1] + 2) >> 2 };
        for x in 1..15 {
            e.top[x] = (p.top[x - 1] + 2 * p.top[x] + p.top[x + 1] + 2) >> 2;
        }
        e.top[15] = (p.top[14] + 3 * p.top[15] + 2) >> 2;
    }
    if p.have_topleft {
        e.topleft = if p.have_top && p.have_left {
            (p.top[0] + 2 * p.topleft + p.left[0] + 2) >> 2
        } else if p.have_top {
            (3 * p.topleft + p.top[0] + 2) >> 2
        } else if p.have_left {
            (3 * p.topleft + p.left[0] + 2) >> 2
        } else {
            p.topleft
        };
    }
    if p.have_left {
        e.left[0] = if p.have_topleft { (p.topleft + 2 * p.left[0] + p.left[1] + 2) >> 2 } else { (3 * p.left[0] + p.left[1] + 2) >> 2 };
        for y in 1..7 {
            e.left[y] = (p.left[y - 1] + 2 * p.left[y] + p.left[y + 1] + 2) >> 2;
        }
        e.left[7] = (p.left[6] + 3 * p.left[7] + 2) >> 2;
    }
}

/// Intra 16x16 (`n` = 16, luma) or chroma 8x8 (`n` = 8) prediction for the non-DC modes and the
/// whole-block DC. `mode` follows the Intra16x16 numbering: 0 vertical, 1 horizontal, 2 DC, 3 plane.
pub fn predict_16x16(mode: u8, top: &[i32; 16], left: &[i32; 16], topleft: i32, have_top: bool, have_left: bool, out: &mut [u8]) {
    match mode {
        0 => {
            for y in 0..16 {
                for x in 0..16 {
                    out[y * 16 + x] = top[x] as u8;
                }
            }
        }
        1 => {
            for y in 0..16 {
                for x in 0..16 {
                    out[y * 16 + x] = left[y] as u8;
                }
            }
        }
        2 => {
            let st: i32 = top.iter().sum();
            let sl: i32 = left.iter().sum();
            let v = match (have_top, have_left) {
                (true, true) => (st + sl + 16) >> 5,
                (false, true) => (sl + 8) >> 4,
                (true, false) => (st + 8) >> 4,
                (false, false) => 128,
            };
            out[..256].fill(v as u8);
        }
        _ => {
            let tt = |i: i32| if i < 0 { topleft } else { top[i as usize] };
            let ll = |j: i32| if j < 0 { topleft } else { left[j as usize] };
            let mut h = 0;
            let mut v = 0;
            for k in 0..8 {
                h += (k + 1) * (tt(8 + k) - tt(6 - k));
                v += (k + 1) * (ll(8 + k) - ll(6 - k));
            }
            let a = 16 * (left[15] + top[15]);
            let b = (5 * h + 32) >> 6;
            let c = (5 * v + 32) >> 6;
            for y in 0..16 {
                for x in 0..16 {
                    out[y as usize * 16 + x as usize] = clip((a + b * (x - 7) + c * (y - 7) + 16) >> 5);
                }
            }
        }
    }
}

/// Chroma prediction for one 8x8 component. `mode` follows intra_chroma_pred_mode: 0 DC,
/// 1 horizontal, 2 vertical, 3 plane.
pub fn predict_chroma(mode: u8, top: &[i32; 8], left: &[i32; 8], topleft: i32, have_top: bool, have_left: bool, out: &mut [u8]) {
    match mode {
        0 => {
            for by in 0..2 {
                for bx in 0..2 {
                    let st: i32 = top[bx * 4..bx * 4 + 4].iter().sum();
                    let sl: i32 = left[by * 4..by * 4 + 4].iter().sum();
                    let v = if (bx == 0 && by == 0) || (bx > 0 && by > 0) {
                        match (have_top, have_left) {
                            (true, true) => (st + sl + 4) >> 3,
                            (false, true) => (sl + 2) >> 2,
                            (true, false) => (st + 2) >> 2,
                            (false, false) => 128,
                        }
                    } else if bx > 0 {
                        // top-right block prefers the row above
                        if have_top {
                            (st + 2) >> 2
                        } else if have_left {
                            (sl + 2) >> 2
                        } else {
                            128
                        }
                    } else if have_left {
                        // bottom-left block prefers the column to its left
                        (sl + 2) >> 2
                    } else if have_top {
                        (st + 2) >> 2
                    } else {
                        128
                    };
                    for y in 0..4 {
                        for x in 0..4 {
                            out[(by * 4 + y) * 8 + bx * 4 + x] = v as u8;
                        }
                    }
                }
            }
        }
        1 => {
            for y in 0..8 {
                for x in 0..8 {
                    out[y * 8 + x] = left[y] as u8;
                }
            }
        }
        2 => {
            for y in 0..8 {
                for x in 0..8 {
                    out[y * 8 + x] = top[x] as u8;
                }
            }
        }
        _ => {
            let tt = |i: i32| if i < 0 { topleft } else { top[i as usize] };
            let ll = |j: i32| if j < 0 { topleft } else { left[j as usize] };
            let mut h = 0;
            let mut v = 0;
            for k in 0..4 {
                h += (k + 1) * (tt(4 + k) - tt(2 - k));
                v += (k + 1) * (ll(4 + k) - ll(2 - k));
            }
            let a = 16 * (left[7] + top[7]);
            let b = (34 * h + 32) >> 6;
            let c = (34 * v + 32) >> 6;
            for y in 0..8 {
                for x in 0..8 {
                    out[y as usize * 8 + x as usize] = clip((a + b * (x - 3) + c * (y - 3) + 16) >> 5);
                }
            }
        }
    }
}
