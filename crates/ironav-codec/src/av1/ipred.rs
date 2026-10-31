//! Intra prediction (spec 7.11.2): edge preparation, the DC, smooth, Paeth, directional and filter
//! intra predictors, the intra edge filter and upsampling, and chroma from luma (7.11.5).
//! The smooth weights and derivative table follow rav1e 0.8.1 (src/predict.rs), BSD-2-Clause.

use super::quant_tables::SM_WEIGHTS;
use super::tables::*;

/// A plane of decoded samples, `stride` apart; the allocation covers whole superblocks.
#[derive(Clone)]
pub struct Plane {
    pub data: Vec<u16>,
    pub stride: usize,
}

impl Plane {
    pub fn new(w: usize, h: usize) -> Plane {
        Plane { data: vec![0; w * h], stride: w }
    }
    #[inline]
    pub fn at(&self, x: usize, y: usize) -> i32 {
        self.data[y * self.stride + x] as i32
    }
}

/// What the predictor knows about a block's surroundings.
pub struct Edges {
    pub have_left: bool,
    pub have_above: bool,
    pub have_above_right: bool,
    pub have_below_left: bool,
    /// The last sample column and row of the plane that the frame's decoded area covers.
    pub max_x: i32,
    pub max_y: i32,
}

const OFF: usize = 16;

fn round2(x: i32, n: u32) -> i32 {
    if n == 0 {
        x
    } else {
        (x + (1 << (n - 1))) >> n
    }
}

fn round2_signed(x: i32, n: u32) -> i32 {
    if x >= 0 {
        round2(x, n)
    } else {
        -round2(-x, n)
    }
}

fn strength(w: usize, h: usize, smooth: bool, delta: i32) -> u8 {
    let wh = w + h;
    let d = delta.unsigned_abs();
    if smooth {
        if wh <= 8 {
            if d >= 64 {
                return 2;
            }
            if d >= 40 {
                return 1;
            }
        } else if wh <= 16 {
            if d >= 48 {
                return 2;
            }
            if d >= 20 {
                return 1;
            }
        } else if wh <= 24 {
            if d >= 4 {
                return 3;
            }
        } else {
            return 3;
        }
    } else if wh <= 8 {
        if d >= 56 {
            return 1;
        }
    } else if wh <= 16 {
        if d >= 40 {
            return 1;
        }
    } else if wh <= 24 {
        if d >= 32 {
            return 3;
        }
        if d >= 16 {
            return 2;
        }
        if d >= 8 {
            return 1;
        }
    } else if wh <= 32 {
        if d >= 32 {
            return 3;
        }
        if d >= 4 {
            return 2;
        }
        return 1;
    } else {
        return 3;
    }
    0
}

fn use_upsample(w: usize, h: usize, smooth: bool, delta: i32) -> bool {
    let d = delta.unsigned_abs();
    if d == 0 || d >= 40 {
        false
    } else if smooth {
        w + h <= 8
    } else {
        w + h <= 16
    }
}

/// intra_edge_filter over `buf` (which holds the edge from index -1 at `OFF - 1`).
fn filter_edge(buf: &mut [i32], sz: usize, strength: u8) {
    if strength == 0 {
        return;
    }
    let mut edge = [0i32; 300];
    for i in 0..sz {
        edge[i] = buf[OFF - 1 + i];
    }
    for i in 1..sz {
        let mut s = 0;
        for j in 0..5 {
            let k = (i + j).saturating_sub(2).min(sz - 1);
            s += INTRA_EDGE_KERNEL[strength as usize - 1][j] * edge[k];
        }
        buf[OFF - 1 + i] = (s + 8) >> 4;
    }
}

/// intra edge upsampling over `buf` (index -1 at `OFF - 1`), doubling the edge resolution.
fn upsample_edge(buf: &mut [i32], num_px: usize, bd: u32) {
    let mut dup = [0i32; 300];
    dup[0] = buf[OFF - 1];
    for i in -1..num_px as i32 {
        dup[(i + 2) as usize] = buf[(OFF as i32 + i) as usize];
    }
    dup[num_px + 2] = buf[OFF + num_px - 1];
    buf[OFF - 2] = dup[0];
    let max = (1 << bd) - 1;
    for i in 0..num_px {
        let s = -dup[i] + 9 * dup[i + 1] + 9 * dup[i + 2] - dup[i + 3];
        buf[OFF + 2 * i - 1] = round2(s, 4).clamp(0, max);
        buf[OFF + 2 * i] = dup[i + 2];
    }
}

/// Predicts a `1 << log2w` by `1 << log2h` block at (x, y) of `p`, writing the prediction into the
/// plane. `filter` is the filter intra mode when filter intra is used (luma only); `smooth` is
/// get_filter_type's answer for the block's neighbours.
#[allow(clippy::too_many_arguments)]
pub fn predict_intra(p: &mut Plane, x: usize, y: usize, log2w: u32, log2h: u32, mode: u8, angle_delta: i32, filter: Option<u8>, e: &Edges, smooth: bool, edge_filter: bool, bd: u32) {
    let (w, h) = (1usize << log2w, 1usize << log2h);
    let base = 1i32 << (bd - 1);
    let mut above = [0i32; 288];
    let mut left = [0i32; 288];
    let (xi, yi) = (x as i32, y as i32);
    let n = w + h;
    if !e.have_above && e.have_left {
        let v = p.at(x - 1, y);
        above[OFF..OFF + n].fill(v);
    } else if !e.have_above {
        above[OFF..OFF + n].fill(base - 1);
    } else {
        let limit = e.max_x.min(xi + if e.have_above_right { 2 * w as i32 } else { w as i32 } - 1);
        let row = (y - 1) * p.stride;
        for i in 0..n {
            above[OFF + i] = p.data[row + limit.min(xi + i as i32) as usize] as i32;
        }
    }
    if !e.have_left && e.have_above {
        let v = p.at(x, y - 1);
        left[OFF..OFF + n].fill(v);
    } else if !e.have_left {
        left[OFF..OFF + n].fill(base + 1);
    } else {
        let limit = e.max_y.min(yi + if e.have_below_left { 2 * h as i32 } else { h as i32 } - 1);
        for i in 0..n {
            left[OFF + i] = p.at(x - 1, limit.min(yi + i as i32) as usize);
        }
    }
    let corner = if e.have_above && e.have_left {
        p.at(x - 1, y - 1)
    } else if e.have_above {
        p.at(x, y - 1)
    } else if e.have_left {
        p.at(x - 1, y)
    } else {
        base
    };
    above[OFF - 1] = corner;
    left[OFF - 1] = corner;

    let mut pred = vec![0i32; w * h];
    if let (0, Some(fmode)) = (0, filter) {
        let _ = fmode;
    }
    if let Some(fmode) = filter {
        filter_intra(&mut pred, &mut above, &mut left, w, h, fmode, bd);
    } else if (1..=8).contains(&mode) {
        let p_angle = mode_to_angle(mode) + angle_delta * 3;
        directional(&mut pred, &mut above, &mut left, w, h, p_angle, x, y, e, smooth, edge_filter, bd);
    } else {
        match mode {
            0 => {
                let v = if e.have_above && e.have_left {
                    let s: i32 = above[OFF..OFF + w].iter().sum::<i32>() + left[OFF..OFF + h].iter().sum::<i32>();
                    (s + ((w + h) >> 1) as i32) / (w + h) as i32
                } else if e.have_above {
                    let s: i32 = above[OFF..OFF + w].iter().sum();
                    (s + (w >> 1) as i32) >> log2w
                } else if e.have_left {
                    let s: i32 = left[OFF..OFF + h].iter().sum();
                    (s + (h >> 1) as i32) >> log2h
                } else {
                    base
                };
                pred.fill(v);
            }
            9 => {
                let wy = &SM_WEIGHTS[h..2 * h];
                let wx = &SM_WEIGHTS[w..2 * w];
                for i in 0..h {
                    for j in 0..w {
                        let s = wy[i] as i32 * above[OFF + j]
                            + (256 - wy[i] as i32) * left[OFF + h - 1]
                            + wx[j] as i32 * left[OFF + i]
                            + (256 - wx[j] as i32) * above[OFF + w - 1];
                        pred[i * w + j] = round2(s, 9);
                    }
                }
            }
            10 => {
                let wy = &SM_WEIGHTS[h..2 * h];
                for i in 0..h {
                    for j in 0..w {
                        let s = wy[i] as i32 * above[OFF + j] + (256 - wy[i] as i32) * left[OFF + h - 1];
                        pred[i * w + j] = round2(s, 8);
                    }
                }
            }
            11 => {
                let wx = &SM_WEIGHTS[w..2 * w];
                for i in 0..h {
                    for j in 0..w {
                        let s = wx[j] as i32 * left[OFF + i] + (256 - wx[j] as i32) * above[OFF + w - 1];
                        pred[i * w + j] = round2(s, 8);
                    }
                }
            }
            _ => {
                // Paeth
                let tl = above[OFF - 1];
                for i in 0..h {
                    for j in 0..w {
                        let t = above[OFF + j];
                        let l = left[OFF + i];
                        let b = t + l - tl;
                        let pl = (b - l).abs();
                        let pt = (b - t).abs();
                        let ptl = (b - tl).abs();
                        pred[i * w + j] = if pl <= pt && pl <= ptl {
                            l
                        } else if pt <= ptl {
                            t
                        } else {
                            tl
                        };
                    }
                }
            }
        }
    }
    for i in 0..h {
        let row = (y + i) * p.stride + x;
        for j in 0..w {
            p.data[row + j] = pred[i * w + j] as u16;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn directional(pred: &mut [i32], above: &mut [i32; 288], left: &mut [i32; 288], w: usize, h: usize, p_angle: i32, x: usize, y: usize, e: &Edges, smooth: bool, edge_filter: bool, bd: u32) {
    let mut up_above = false;
    let mut up_left = false;
    if edge_filter {
        if p_angle != 90 && p_angle != 180 {
            if p_angle > 90 && p_angle < 180 && w + h >= 24 {
                let v = round2(left[OFF] * 5 + above[OFF - 1] * 6 + above[OFF] * 5, 4);
                left[OFF - 1] = v;
                above[OFF - 1] = v;
            }
            if e.have_above {
                let s = strength(w, h, smooth, p_angle - 90);
                let num_px = w.min((e.max_x - x as i32 + 1) as usize) + if p_angle < 90 { h } else { 0 } + 1;
                filter_edge(above, num_px, s);
            }
            if e.have_left {
                let s = strength(w, h, smooth, p_angle - 180);
                let num_px = h.min((e.max_y - y as i32 + 1) as usize) + if p_angle > 180 { w } else { 0 } + 1;
                filter_edge(left, num_px, s);
            }
        }
        up_above = use_upsample(w, h, smooth, p_angle - 90);
        if up_above {
            let num_px = w + if p_angle < 90 { h } else { 0 };
            upsample_edge(above, num_px, bd);
        }
        up_left = use_upsample(w, h, smooth, p_angle - 180);
        if up_left {
            let num_px = h + if p_angle > 180 { w } else { 0 };
            upsample_edge(left, num_px, bd);
        }
    }
    let (ua, ul) = (up_above as i32, up_left as i32);
    let at = |buf: &[i32; 288], i: i32| buf[(OFF as i32 + i) as usize];
    if p_angle < 90 {
        let dx = dr(p_angle);
        let max_base_x = ((w + h - 1) as i32) << ua;
        for i in 0..h {
            for j in 0..w {
                let idx = (i as i32 + 1) * dx;
                let base = (idx >> (6 - ua)) + ((j as i32) << ua);
                let shift = ((idx << ua) >> 1) & 0x1F;
                pred[i * w + j] = if base < max_base_x {
                    round2(at(above, base) * (32 - shift) + at(above, base + 1) * shift, 5)
                } else {
                    at(above, max_base_x)
                };
            }
        }
    } else if p_angle > 90 && p_angle < 180 {
        let dx = dr(180 - p_angle);
        let dy = dr(p_angle - 90);
        for i in 0..h {
            for j in 0..w {
                let idx = ((j as i32) << 6) - (i as i32 + 1) * dx;
                let base = idx >> (6 - ua);
                if base >= -(1 << ua) {
                    let shift = ((idx << ua) >> 1) & 0x1F;
                    pred[i * w + j] = round2(at(above, base) * (32 - shift) + at(above, base + 1) * shift, 5);
                } else {
                    let idx = ((i as i32) << 6) - (j as i32 + 1) * dy;
                    let base = idx >> (6 - ul);
                    let shift = ((idx << ul) >> 1) & 0x1F;
                    pred[i * w + j] = round2(at(left, base) * (32 - shift) + at(left, base + 1) * shift, 5);
                }
            }
        }
    } else if p_angle > 180 {
        let dy = dr(270 - p_angle);
        for i in 0..h {
            for j in 0..w {
                let idx = (j as i32 + 1) * dy;
                let base = (idx >> (6 - ul)) + ((i as i32) << ul);
                let shift = ((idx << ul) >> 1) & 0x1F;
                pred[i * w + j] = round2(at(left, base) * (32 - shift) + at(left, base + 1) * shift, 5);
            }
        }
    } else if p_angle == 90 {
        for i in 0..h {
            for j in 0..w {
                pred[i * w + j] = above[OFF + j];
            }
        }
    } else {
        for i in 0..h {
            for j in 0..w {
                pred[i * w + j] = left[OFF + i];
            }
        }
    }
}

fn dr(angle: i32) -> i32 {
    dr_intra_derivative(angle)
}

/// The recursive filter intra predictor (7.11.2.3), four-by-two cells at a time.
fn filter_intra(pred: &mut [i32], above: &mut [i32; 288], left: &mut [i32; 288], w: usize, h: usize, mode: u8, bd: u32) {
    let max = (1 << bd) - 1;
    let taps = &INTRA_FILTER_TAPS[mode as usize];
    for i2 in 0..h / 2 {
        for j4 in 0..w / 4 {
            let mut pv = [0i32; 7];
            for (i, v) in pv.iter_mut().enumerate() {
                *v = if i < 5 {
                    if i2 == 0 {
                        above[(OFF as i32 + (j4 * 4) as i32 + i as i32 - 1) as usize]
                    } else if j4 == 0 && i == 0 {
                        left[OFF + (i2 * 2) - 1]
                    } else {
                        pred[(i2 * 2 - 1) * w + (j4 * 4 + i - 1)]
                    }
                } else if j4 == 0 {
                    left[OFF + (i2 * 2) + i - 5]
                } else {
                    pred[((i2 * 2) + i - 5) * w + (j4 * 4 - 1)]
                };
            }
            for i in 0..2 {
                for j in 0..4 {
                    let t = &taps[(i << 2) + j];
                    let pr: i32 = (0..7).map(|k| t[k] as i32 * pv[k]).sum();
                    pred[(i2 * 2 + i) * w + j4 * 4 + j] = round2_signed(pr, 4).clamp(0, max);
                }
            }
        }
    }
}

/// Chroma from luma (7.11.5): `dc` is the chroma prediction in place at (x, y) of size `w` by `h`;
/// `luma` is the reconstructed luma, read up to `max_w` by `max_h` samples (the coded luma extent).
#[allow(clippy::too_many_arguments)]
pub fn predict_cfl(c: &mut Plane, x: usize, y: usize, log2w: u32, log2h: u32, sub_x: u32, sub_y: u32, alpha: i32, luma: &Plane, max_w: usize, max_h: usize, bd: u32) {
    let (w, h) = (1usize << log2w, 1usize << log2h);
    let mut l = vec![0i32; w * h];
    let mut sum = 0i32;
    for i in 0..h {
        for j in 0..w {
            let lx = ((x + j) << sub_x).min(max_w - (1 << sub_x));
            let ly = ((y + i) << sub_y).min(max_h - (1 << sub_y));
            let mut t = 0;
            for dy in 0..=sub_y as usize {
                for dx in 0..=sub_x as usize {
                    t += luma.at(lx + dx, ly + dy);
                }
            }
            let v = t << (3 - sub_x - sub_y);
            l[i * w + j] = v;
            sum += v;
        }
    }
    let avg = round2(sum, log2w + log2h);
    let max = (1 << bd) - 1;
    for i in 0..h {
        for j in 0..w {
            let idx = (y + i) * c.stride + x + j;
            let dc = c.data[idx] as i32;
            let scaled = round2_signed(alpha * (l[i * w + j] - avg), 6);
            c.data[idx] = (dc + scaled).clamp(0, max) as u16;
        }
    }
}
