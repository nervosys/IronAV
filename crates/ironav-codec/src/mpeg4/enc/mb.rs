//! The macroblock layer of the MPEG-4 encoder: motion search, the intra and inter decisions, the
//! coefficient levels, the bits, and the reconstruction. The prediction, dequantisation and inverse
//! transform are the decoder's (`mpeg4/mb.rs` and `mc.rs`), so each macroblock is reconstructed
//! exactly as the decoder will reconstruct it.

use super::{BitW, Source, SEARCH};
use crate::mpeg4::bits::Rl;
use crate::mpeg4::idct;
use crate::mpeg4::mc::{self, Plane};
use crate::mpeg4::tables as T;
use crate::mpeg4::{Pic, Vol, FOUR_MV, INTER_LAST, INTRA_LAST, NOT_CODED, INTRA};
use std::sync::{Arc, OnceLock};

/// The chroma vector rounding table of the decoder (`mb.rs`).
const CHROMA_ROUND: [i32; 16] = [0, 0, 0, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 1, 1];
/// The cost of a unit of motion vector difference, in summed absolute differences.
const MV_LAMBDA: u32 = 2;
/// What four vectors cost beyond one, in summed absolute differences.
const FOUR_MV_PENALTY: u32 = 40;
/// Intra is chosen over inter only when its cost, plus this much per quantiser step, is lower: an intra
/// macroblock codes a DC and six blocks, where a skipped one codes almost nothing.
const INTRA_PENALTY: u32 = 16;

/// The run-level codes of both tables, with a lookup from (last, run, level) to the symbol.
struct Rls {
    intra: Rl,
    inter: Rl,
    intra_lut: Vec<u16>,
    inter_lut: Vec<u16>,
}

fn lut(rl: &Rl) -> Vec<u16> {
    let mut v = vec![0u16; 2 * 64 * 64];
    for s in 0..rl.escape {
        let last = (s >= rl.last_from) as usize;
        let i = (last * 64 + rl.run[s] as usize) * 64 + rl.level[s] as usize;
        if v[i] == 0 {
            v[i] = s as u16 + 1;
        }
    }
    v
}

fn rls() -> &'static Rls {
    static R: OnceLock<Rls> = OnceLock::new();
    R.get_or_init(|| {
        let intra = Rl::new(&T::INTRA_VLC, &T::INTRA_RUN, &T::INTRA_LEVEL, INTRA_LAST);
        let inter = Rl::new(&T::INTER_VLC, &T::INTER_RUN, &T::INTER_LEVEL, INTER_LAST);
        let (intra_lut, inter_lut) = (lut(&intra), lut(&inter));
        Rls { intra, inter, intra_lut, inter_lut }
    })
}

/// The grid (0 luma, 1 and 2 chroma) and the block coordinates in it of block `n` of a macroblock.
fn block_pos(mbx: usize, mby: usize, n: usize) -> (usize, i32, i32) {
    if n < 4 {
        (0, (2 * mbx + (n & 1)) as i32, (2 * mby + (n >> 1)) as i32)
    } else {
        (n - 3, mbx as i32, mby as i32)
    }
}

fn median(a: i32, b: i32, c: i32) -> i32 {
    a.max(b).min(a.min(b).max(c))
}

/// The scan a block's coefficients are written in: zigzag, or the alternate scan of its AC
/// prediction's direction (`top` is the block's DC prediction from the block above).
fn scan_for(ac_pred: bool, top: bool) -> &'static [u8; 64] {
    match (ac_pred, top) {
        (false, _) => &T::ZIGZAG,
        (true, false) => &T::ALT_VERTICAL_SCAN,
        (true, true) => &T::ALT_HORIZONTAL_SCAN,
    }
}

/// The quantised level of a coefficient under the H.263 rule the decoder dequantises with: a level
/// `l` reconstructs to `2|l|q + ((q - 1) | 1)`, so the nearest level is the rounded excess over that.
fn quant(f: f32, q: i32) -> i32 {
    let qadd = ((q - 1) | 1) as f32;
    let l = ((f.abs() - qadd) / (2.0 * q as f32)).round().max(0.0) as i32;
    if f < 0.0 {
        -l
    } else {
        l
    }
}

/// The inter quantiser: a dead zone of half a quantiser step, so small residuals quantise to zero and the
/// macroblock can be skipped. Level `l` covers `[(2l + 0.5) q, (2l + 2.5) q)`.
fn quant_inter(f: f32, q: i32) -> i32 {
    let l = ((f.abs() - q as f32 / 2.0) / (2.0 * q as f32)).floor().max(0.0) as i32;
    if f < 0.0 {
        -l
    } else {
        l
    }
}

/// The coefficients of a block as the decoder dequantises them (`mb.rs`, H.263 quantisation):
/// the intra DC is the level times its scale, and the AC levels are reconstructed by the rule above.
fn dequant(l: &[i32; 64], intra: bool, q: i32, dcs: i32) -> [i16; 64] {
    let qadd = (q - 1) | 1;
    let mut out = [0i16; 64];
    for i in 0..64 {
        let c = l[i];
        out[i] = if intra && i == 0 {
            (c * dcs) as i16
        } else if c == 0 {
            0
        } else {
            let m = c.abs() * 2 * q + qadd;
            (if c < 0 { -m } else { m }).clamp(-2048, 2047) as i16
        };
    }
    out
}

/// Sum of the AC magnitudes of six blocks: what an AC prediction must beat to be used.
fn ac_cost(c: &[[i32; 64]; 6]) -> u32 {
    c.iter().map(|b| b[1..].iter().map(|v| v.unsigned_abs()).sum::<u32>()).sum()
}

/// The luma prediction of an `n` by `n` block at (`x0`, `y0`) from `refp`, with the decoder's
/// half-sample motion compensation. Four-vector blocks clamp their position as the decoder does.
#[allow(clippy::too_many_arguments)]
fn luma_pred(refp: &Pic, vol: &Vol, x0: i32, y0: i32, n: usize, mv: [i32; 2], four: bool, dst: &mut [u8], ds: usize) {
    let (lw, lh) = (vol.mbw * 16, vol.mbh * 16);
    let lp = Plane { d: &refp.planes[0], stride: lw, w: lw as i32, h: lh as i32 };
    let (w, h) = (vol.width as i32, vol.height as i32);
    let (mvx, mvy) = (mv[0], mv[1]);
    let (sx, sy, dxy) = if four {
        let (sx, sy) = ((x0 + (mvx >> 1)).clamp(-16, w), (y0 + (mvy >> 1)).clamp(-16, h));
        let mut dxy = 0u32;
        if sx != w {
            dxy |= (mvx & 1) as u32;
        }
        if sy != h {
            dxy |= ((mvy & 1) << 1) as u32;
        }
        (sx, sy, dxy)
    } else {
        (x0 + (mvx >> 1), y0 + (mvy >> 1), (((mvy & 1) << 1) | (mvx & 1)) as u32)
    };
    mc::hpel(dst, ds, n, n, &lp, sx, sy, dxy, false, false);
}

/// The chroma predictions (U and V, 8 by 8) of a macroblock with one vector.
fn chroma_1(refp: &Pic, vol: &Vol, mbx: usize, mby: usize, mv: [i32; 2], du: &mut [u8], dv: &mut [u8]) {
    let (cw, ch) = (vol.mbw * 8, vol.mbh * 8);
    let (mvx, mvy) = (mv[0], mv[1]);
    let (sx, sy) = (mbx as i32 * 16 + (mvx >> 1), mby as i32 * 16 + (mvy >> 1));
    let dxy = (((mvy & 1) << 1) | (mvx & 1)) as u32;
    let uvdxy = dxy | (mvy & 2) as u32 | ((mvx & 2) >> 1) as u32;
    for (p, dst) in [(1, du), (2, dv)] {
        let cp = Plane { d: &refp.planes[p], stride: cw, w: cw as i32, h: ch as i32 };
        mc::hpel(dst, 8, 8, 8, &cp, sx >> 1, sy >> 1, uvdxy, false, false);
    }
}

/// The chroma predictions of a four-vector macroblock: the chroma vector is the rounded sum of the four.
fn chroma_4(refp: &Pic, vol: &Vol, mbx: usize, mby: usize, mvs: &[[i32; 2]; 4], du: &mut [u8], dv: &mut [u8]) {
    let (cw, ch) = (vol.mbw * 8, vol.mbh * 8);
    let (w, h) = (vol.width as i32, vol.height as i32);
    let (mut sx, mut sy) = (0i32, 0i32);
    for m in mvs {
        sx += m[0];
        sy += m[1];
    }
    let round = |x: i32| CHROMA_ROUND[(x & 15) as usize] + (x >> 3);
    let (mx, my) = (round(sx), round(sy));
    let mut dxy = (((my & 1) << 1) | (mx & 1)) as u32;
    let px = (mbx as i32 * 8 + (mx >> 1)).clamp(-8, w >> 1);
    if px == w >> 1 {
        dxy &= !1;
    }
    let py = (mby as i32 * 8 + (my >> 1)).clamp(-8, h >> 1);
    if py == h >> 1 {
        dxy &= !2;
    }
    for (p, dst) in [(1, du), (2, dv)] {
        let cp = Plane { d: &refp.planes[p], stride: cw, w: cw as i32, h: ch as i32 };
        mc::hpel(dst, 8, 8, 8, &cp, px, py, dxy, false, false);
    }
}

/// The picture being coded: its state, the reference, the predictors, and the bits written so far.
pub(super) struct Coder<'a> {
    vol: &'a Vol,
    src: &'a Source,
    refp: Option<Arc<Pic>>,
    cur: Pic,
    bw: BitW,
    q: i32,
    fcode: i32,
    /// The orthonormal 8-point DCT basis: `cos[k][x]` (k the frequency, x the sample).
    cos: [[f32; 8]; 8],
    /// The predicted DC (full scale) and the AC levels of each block, per grid.
    dc: [Vec<i32>; 3],
    ac: [Vec<[i32; 16]>; 3],
}

impl<'a> Coder<'a> {
    pub fn new(vol: &'a Vol, src: &'a Source, refp: Option<Arc<Pic>>, q: i32, fcode: i32, bw: BitW) -> Self {
        let n = vol.mbw * vol.mbh;
        let mut cos = [[0f32; 8]; 8];
        for (k, row) in cos.iter_mut().enumerate() {
            let ck = if k == 0 { 1.0 / 2f32.sqrt() } else { 1.0 };
            for (x, v) in row.iter_mut().enumerate() {
                *v = ck / 2.0 * (((2 * x + 1) * k) as f32 * std::f32::consts::PI / 16.0).cos();
            }
        }
        Coder {
            vol,
            src,
            refp,
            cur: Pic::new(vol, 0),
            bw,
            q,
            fcode,
            cos,
            dc: [vec![1024; 4 * n], vec![1024; n], vec![1024; n]],
            ac: [vec![[0; 16]; 4 * n], vec![[0; 16]; n], vec![[0; 16]; n]],
        }
    }

    /// The picture's bits and its reconstruction.
    pub fn finish(self) -> (Vec<u8>, Pic) {
        (self.bw.finish(), self.cur)
    }

    fn gw(&self, g: usize) -> i32 {
        if g == 0 {
            2 * self.vol.mbw as i32
        } else {
            self.vol.mbw as i32
        }
    }

    fn gh(&self, g: usize) -> i32 {
        if g == 0 {
            2 * self.vol.mbh as i32
        } else {
            self.vol.mbh as i32
        }
    }

    fn avail(&self, g: usize, bx: i32, by: i32) -> bool {
        bx >= 0 && by >= 0 && bx < self.gw(g) && by < self.gh(g)
    }

    fn idx(&self, g: usize, bx: i32, by: i32) -> usize {
        (by * self.gw(g) + bx) as usize
    }

    fn dc_scale(&self, n: usize) -> i32 {
        let q = self.q as usize;
        if n < 4 {
            T::Y_DC_SCALE[q] as i32
        } else {
            T::C_DC_SCALE[q] as i32
        }
    }

    /// The plane, offset and stride of block `n`'s top left sample in the picture.
    fn block_dst(&self, mbx: usize, mby: usize, n: usize) -> (usize, usize, usize) {
        let (lw, cw) = (self.vol.mbw * 16, self.vol.mbw * 8);
        if n < 4 {
            (0, (mby * 16 + (n >> 1) * 8) * lw + mbx * 16 + (n & 1) * 8, lw)
        } else {
            (n - 3, mby * 8 * cw + mbx * 8, cw)
        }
    }

    /// The source samples of block `n`, as integers.
    fn src_block(&self, mbx: usize, mby: usize, n: usize) -> [i32; 64] {
        let (lw, cw) = (self.vol.mbw * 16, self.vol.mbw * 8);
        let mut out = [0i32; 64];
        for y in 0..8 {
            for x in 0..8 {
                out[y * 8 + x] = if n < 4 {
                    self.src.y[(mby * 16 + (n >> 1) * 8 + y) * lw + mbx * 16 + (n & 1) * 8 + x] as i32
                } else {
                    let p = if n == 4 { &self.src.u } else { &self.src.v };
                    p[(mby * 8 + y) * cw + mbx * 8 + x] as i32
                };
            }
        }
        out
    }

    /// The forward DCT of an 8 by 8 block, in the decoder's layout: `F[v * 8 + u]`, with DC = 8 * mean.
    fn fdct(&self, p: &[i32; 64]) -> [f32; 64] {
        let mut t = [0f32; 64];
        for y in 0..8 {
            for u in 0..8 {
                t[y * 8 + u] = (0..8).map(|x| p[y * 8 + x] as f32 * self.cos[u][x]).sum();
            }
        }
        let mut f = [0f32; 64];
        for v in 0..8 {
            for u in 0..8 {
                f[v * 8 + u] = (0..8).map(|y| t[y * 8 + u] * self.cos[v][y]).sum();
            }
        }
        f
    }

    fn reset_pred(&mut self, mbx: usize, mby: usize) {
        for n in 0..6 {
            let (g, bx, by) = block_pos(mbx, mby, n);
            let i = self.idx(g, bx, by);
            self.dc[g][i] = 1024;
            self.ac[g][i] = [0; 16];
        }
    }

    /// The DC prediction of block `n` (full scale), and whether it comes from the block above.
    fn pred_dc(&self, mbx: usize, mby: usize, n: usize) -> (i32, bool) {
        let (g, bx, by) = block_pos(mbx, mby, n);
        let get = |x: i32, y: i32| if self.avail(g, x, y) { self.dc[g][self.idx(g, x, y)] } else { 1024 };
        let a = get(bx - 1, by);
        let b = get(bx - 1, by - 1);
        let c = get(bx, by - 1);
        if (a - b).abs() < (b - c).abs() {
            (c, true)
        } else {
            (a, false)
        }
    }

    fn mv_at(&self, bx: i32, by: i32) -> [i32; 2] {
        if self.avail(0, bx, by) {
            let m = self.cur.mv[self.idx(0, bx, by)];
            [m[0] as i32, m[1] as i32]
        } else {
            [0, 0]
        }
    }

    /// The predicted vector of block `i` of a macroblock: the median of the left, above and
    /// above-right vectors, or the left one alone in the top row. As the decoder predicts it.
    fn pred_motion(&self, mbx: usize, mby: usize, i: usize) -> [i32; 2] {
        let (bx, by) = ((2 * mbx + (i & 1)) as i32, (2 * mby + (i >> 1)) as i32);
        let a = self.mv_at(bx - 1, by);
        if !self.avail(0, bx, by - 1) {
            return a;
        }
        let b = self.mv_at(bx, by - 1);
        // the above-right candidate: the above-right macroblock's bottom-left block for block 0 and 1
        let c = match i {
            3 => self.mv_at(bx - 1, by - 1),
            0 => self.mv_at(bx + 2, by - 1),
            _ => self.mv_at(bx + 1, by - 1),
        };
        [median(a[0], b[0], c[0]), median(a[1], b[1], c[1])]
    }

    fn set_block_mv(&mut self, mbx: usize, mby: usize, i: usize, mv: [i32; 2]) {
        let k = self.idx(0, (2 * mbx + (i & 1)) as i32, (2 * mby + (i >> 1)) as i32);
        self.cur.mv[k] = [mv[0] as i16, mv[1] as i16];
    }

    fn set_mb_mv(&mut self, mbx: usize, mby: usize, mv: [i32; 2]) {
        for i in 0..4 {
            self.set_block_mv(mbx, mby, i, mv);
        }
    }

    /// Writes a motion vector difference with the picture's f_code.
    fn put_mv(&mut self, diff: i32) {
        if diff == 0 {
            self.bw.put(T::MV_CODE[0] as u32, T::MV_BITS[0] as u32);
            return;
        }
        let a = diff.unsigned_abs() - 1;
        let shift = self.fcode as u32 - 1;
        let code = ((a >> shift) + 1) as usize;
        self.bw.put(T::MV_CODE[code] as u32, T::MV_BITS[code] as u32);
        self.bw.bit(diff < 0);
        if shift > 0 {
            self.bw.put(a & ((1 << shift) - 1), shift);
        }
    }

    /// Writes a DC level difference: its size with the DC VLC, then its bits (and a marker above 8).
    fn put_dc(&mut self, chroma: bool, diff: i32) {
        let size = if diff == 0 { 0 } else { 32 - diff.unsigned_abs().leading_zeros() } as usize;
        let codes = if chroma { &T::DC_CHROM } else { &T::DC_LUM };
        self.bw.put(codes[2 * size] as u32, codes[2 * size + 1] as u32);
        if size > 0 {
            let v = if diff > 0 { diff as u32 } else { (diff + (1 << size) - 1) as u32 };
            self.bw.put(v, size as u32);
            if size > 8 {
                self.bw.bit(true);
            }
        }
    }

    /// Writes one run-level pair: its VLC symbol and sign if the table has it, else an escape of the
    /// first kind that reaches it (escape 1 raises the level, escape 2 the run), else escape 3.
    fn put_rl(&mut self, inter: bool, run: usize, last: bool, level: i32) {
        let r = rls();
        let (rl, lut, codes) = if inter { (&r.inter, &r.inter_lut, &T::INTER_VLC) } else { (&r.intra, &r.intra_lut, &T::INTRA_VLC) };
        let a = level.unsigned_abs() as usize;
        let neg = (level < 0) as u32;
        let lf = last as usize;
        let esc = rl.escape;
        let (ec, el) = (codes[2 * esc] as u32, codes[2 * esc + 1] as u32);
        if run < 64 && a < 64 {
            let s = lut[(lf * 64 + run) * 64 + a];
            if s != 0 {
                let s = s as usize - 1;
                self.bw.put(codes[2 * s] as u32, codes[2 * s + 1] as u32);
                self.bw.bit(neg != 0);
                return;
            }
        }
        if run < 64 {
            for s in 0..esc {
                if rl.run[s] as usize == run && (s >= rl.last_from) == last && rl.level[s] as usize + rl.max_level[lf][run] as usize == a {
                    self.bw.put(ec, el);
                    self.bw.bit(false);
                    self.bw.put(codes[2 * s] as u32, codes[2 * s + 1] as u32);
                    self.bw.bit(neg != 0);
                    return;
                }
            }
        }
        if a < 64 {
            for s in 0..esc {
                if rl.level[s] as usize == a && (s >= rl.last_from) == last && rl.run[s] as usize + rl.max_run[lf][a] as usize + 1 == run {
                    self.bw.put(ec, el);
                    self.bw.put(0b10, 2);
                    self.bw.put(codes[2 * s] as u32, codes[2 * s + 1] as u32);
                    self.bw.bit(neg != 0);
                    return;
                }
            }
        }
        self.bw.put(ec, el);
        self.bw.put(0b11, 2);
        self.bw.bit(last);
        self.bw.put(run as u32, 6);
        self.bw.bit(true);
        self.bw.put((level as u32) & 0xFFF, 12);
        self.bw.bit(true);
    }

    /// Writes the AC levels of a block in scan order. `start` is the position before the first one
    /// (0 for an intra block whose DC is coded apart, -1 for an inter block).
    fn put_block(&mut self, inter: bool, scan: &[u8; 64], start: i32, lv: &[i32; 64]) {
        let items: Vec<(usize, i32)> = ((start + 1) as usize..64).filter(|&p| lv[scan[p] as usize] != 0).map(|p| (p, lv[scan[p] as usize])).collect();
        let mut prev = start;
        for (k, &(p, v)) in items.iter().enumerate() {
            let run = (p as i32 - prev - 1) as usize;
            self.put_rl(inter, run, k + 1 == items.len(), v);
            prev = p as i32;
        }
    }

    fn put_inter_mcbpc(&mut self, sym: usize) {
        self.bw.put(T::INTER_MCBPC_CODE[sym] as u32, T::INTER_MCBPC_BITS[sym] as u32);
    }

    fn put_cbpy(&mut self, sym: usize) {
        self.bw.put(T::CBPY_CODE[sym] as u32, T::CBPY_BITS[sym] as u32);
    }

    /// The four luma blocks and two chroma blocks of an intra macroblock: its DC prediction and its
    /// AC prediction, both from the neighbours already coded. Returns the macroblock's coefficients
    /// with the DC as the level it is coded with.
    pub fn intra_mb(&mut self, mbx: usize, mby: usize, in_p: bool) {
        let mb = mby * self.vol.mbw + mbx;
        let mut a = [[0i32; 64]; 6];
        let mut dcs = [0i32; 6];
        for n in 0..6 {
            dcs[n] = self.dc_scale(n);
            let f = self.fdct(&self.src_block(mbx, mby, n));
            a[n][0] = (f[0] / dcs[n] as f32).round() as i32;
            for k in 1..64 {
                a[n][k] = quant(f[k], self.q);
            }
        }
        // the DC predictions, block by block: each stores its reconstructed DC for the blocks after it
        let mut top = [false; 6];
        let mut diff = [0i32; 6];
        for n in 0..6 {
            let (p, t) = self.pred_dc(mbx, mby, n);
            top[n] = t;
            let s = dcs[n];
            diff[n] = a[n][0] - (p + (s >> 1)) / s;
            let (g, bx, by) = block_pos(mbx, mby, n);
            let i = self.idx(g, bx, by);
            self.dc[g][i] = (a[n][0] * s).clamp(0, 2047);
        }
        // the AC levels, as the decoder stores them: the first column and the first row
        for n in 0..6 {
            let (g, bx, by) = block_pos(mbx, mby, n);
            let i = self.idx(g, bx, by);
            let mut s = [0i32; 16];
            for k in 1..8 {
                s[k] = a[n][k * 8];
                s[8 + k] = a[n][k];
            }
            self.ac[g][i] = s;
        }
        let any_pred = (0..6).any(|n| {
            let (g, bx, by) = block_pos(mbx, mby, n);
            if top[n] {
                self.avail(g, bx, by - 1)
            } else {
                self.avail(g, bx - 1, by)
            }
        });
        let mut coded = a;
        let mut ac_pred = false;
        if any_pred {
            let mut pc = a;
            for n in 0..6 {
                let (g, bx, by) = block_pos(mbx, mby, n);
                if !top[n] {
                    if self.avail(g, bx - 1, by) {
                        let nb = self.ac[g][self.idx(g, bx - 1, by)];
                        for k in 1..8 {
                            pc[n][k * 8] -= nb[k];
                        }
                    }
                } else if self.avail(g, bx, by - 1) {
                    let nb = self.ac[g][self.idx(g, bx, by - 1)];
                    for k in 1..8 {
                        pc[n][k] -= nb[8 + k];
                    }
                }
            }
            if ac_cost(&pc) < ac_cost(&a) {
                ac_pred = true;
                coded = pc;
            }
        }
        let cod: [bool; 6] = std::array::from_fn(|n| coded[n][1..].iter().any(|&v| v != 0));
        let cbpc = ((cod[4] as usize) << 1) | cod[5] as usize;
        let cbpy = (0..4).fold(0usize, |acc, n| acc | ((cod[n] as usize) << (3 - n)));
        if in_p {
            self.bw.put(0, 1); // coded
            self.put_inter_mcbpc(cbpc | 4);
        } else {
            self.bw.put(T::INTRA_MCBPC_CODE[cbpc] as u32, T::INTRA_MCBPC_BITS[cbpc] as u32);
        }
        self.bw.bit(ac_pred);
        self.put_cbpy(cbpy);
        for n in 0..6 {
            self.put_dc(n >= 4, diff[n]);
            if cod[n] {
                let scan = scan_for(ac_pred, top[n]);
                self.put_block(false, scan, 0, &coded[n]);
            }
        }
        // the reconstruction: the coefficients the decoder reads back are the actual levels
        self.cur.mbf[mb] = INTRA;
        self.set_mb_mv(mbx, mby, [0, 0]);
        for n in 0..6 {
            let mut blk = dequant(&a[n], true, self.q, dcs[n]);
            let (plane, off, stride) = self.block_dst(mbx, mby, n);
            idct::put(&mut blk, &mut self.cur.planes[plane][off..], stride);
        }
    }

    /// The predictions of a macroblock from `refp` with its vectors, into the 16 by 16 luma and the
    /// two 8 by 8 chroma arrays.
    #[allow(clippy::too_many_arguments)]
    fn predict(&self, refp: &Pic, mbx: usize, mby: usize, mvs: &[[i32; 2]; 4], four: bool, py: &mut [u8], pu: &mut [u8], pv: &mut [u8]) {
        let (x0, y0) = (mbx as i32 * 16, mby as i32 * 16);
        if four {
            for (i, mv) in mvs.iter().enumerate() {
                let (bx, by) = (x0 + (i as i32 & 1) * 8, y0 + (i as i32 >> 1) * 8);
                let off = (i >> 1) * 8 * 16 + (i & 1) * 8;
                luma_pred(refp, self.vol, bx, by, 8, *mv, true, &mut py[off..], 16);
            }
            chroma_4(refp, self.vol, mbx, mby, mvs, pu, pv);
        } else {
            luma_pred(refp, self.vol, x0, y0, 16, mvs[0], false, py, 16);
            chroma_1(refp, self.vol, mbx, mby, mvs[0], pu, pv);
        }
    }

    /// The motion search for an `n` by `n` block at (`x0`, `y0`): a diamond search from the zero and
    /// predicted vectors, then the eight half-sample neighbours of the best. Returns the vector and
    /// its cost (summed absolute differences plus the vector's cost).
    fn search(&self, refp: &Pic, x0: i32, y0: i32, n: usize, pred: [i32; 2], four: bool) -> ([i32; 2], u32) {
        let lw = self.vol.mbw * 16;
        let cost = |mv: [i32; 2]| -> u32 {
            let mut buf = [0u8; 256];
            luma_pred(refp, self.vol, x0, y0, n, mv, four, &mut buf, n);
            let mut s = 0u32;
            for y in 0..n {
                for x in 0..n {
                    let o = (y0 as usize + y) * lw + x0 as usize + x;
                    s += (buf[y * n + x] as i32 - self.src.y[o] as i32).unsigned_abs();
                }
            }
            s + MV_LAMBDA * ((mv[0] - pred[0]).unsigned_abs() + (mv[1] - pred[1]).unsigned_abs())
        };
        let clampv = |m: [i32; 2]| [m[0].clamp(-SEARCH, SEARCH), m[1].clamp(-SEARCH, SEARCH)];
        let mut best = clampv(pred);
        let mut bc = cost(best);
        let c = cost([0, 0]);
        if c < bc {
            bc = c;
            best = [0, 0];
        }
        for _ in 0..32 {
            let mut moved = false;
            for d in [[2, 0], [-2, 0], [0, 2], [0, -2]] {
                let m = clampv([best[0] + d[0], best[1] + d[1]]);
                if m == best {
                    continue;
                }
                let c = cost(m);
                if c < bc {
                    bc = c;
                    best = m;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        let centre = best;
        for dy in -1..=1 {
            for dx in -1..=1 {
                let m = clampv([centre[0] + dx, centre[1] + dy]);
                if (dx == 0 && dy == 0) || m == centre {
                    continue;
                }
                let c = cost(m);
                if c < bc {
                    bc = c;
                    best = m;
                }
            }
        }
        (best, bc)
    }

    /// The summed absolute deviation of the luma blocks from their means: what an intra macroblock
    /// costs to code, roughly, against the inter cost of the motion search.
    fn intra_cost(&self, mbx: usize, mby: usize) -> u32 {
        (0..4)
            .map(|n| {
                let s = self.src_block(mbx, mby, n);
                let mean = s.iter().sum::<i32>() / 64;
                s.iter().map(|&v| (v - mean).unsigned_abs()).sum::<u32>()
            })
            .sum()
    }

    /// Stores a macroblock's predictions as its reconstruction, before its residual is added.
    fn store_pred(&mut self, mbx: usize, mby: usize, py: &[u8], pu: &[u8], pv: &[u8]) {
        let (lw, cw) = (self.vol.mbw * 16, self.vol.mbw * 8);
        for y in 0..16 {
            let o = (mby * 16 + y) * lw + mbx * 16;
            self.cur.planes[0][o..o + 16].copy_from_slice(&py[y * 16..y * 16 + 16]);
        }
        for y in 0..8 {
            let o = (mby * 8 + y) * cw + mbx * 8;
            self.cur.planes[1][o..o + 8].copy_from_slice(&pu[y * 8..y * 8 + 8]);
            self.cur.planes[2][o..o + 8].copy_from_slice(&pv[y * 8..y * 8 + 8]);
        }
    }

    /// The coefficient levels of the six blocks of an inter macroblock: each block's source minus
    /// its prediction, transformed and quantised.
    fn inter_levels(&self, mbx: usize, mby: usize, py: &[u8], pu: &[u8], pv: &[u8]) -> [[i32; 64]; 6] {
        let mut out = [[0i32; 64]; 6];
        for (n, lv) in out.iter_mut().enumerate() {
            let s = self.src_block(mbx, mby, n);
            let mut d = [0i32; 64];
            for y in 0..8 {
                for x in 0..8 {
                    let p = if n < 4 {
                        py[((n >> 1) * 8 + y) * 16 + (n & 1) * 8 + x]
                    } else if n == 4 {
                        pu[y * 8 + x]
                    } else {
                        pv[y * 8 + x]
                    };
                    d[y * 8 + x] = s[y * 8 + x] - p as i32;
                }
            }
            let f = self.fdct(&d);
            for k in 0..64 {
                lv[k] = quant_inter(f[k], self.q);
            }
        }
        out
    }

    /// A P-picture macroblock: skipped when it has no residual and no motion, intra when intra is
    /// cheaper than the motion search, else inter with its vectors and coefficients.
    pub fn p_mb(&mut self, mbx: usize, mby: usize) {
        let refp = self.refp.clone().expect("a P picture has a reference");
        let mb = mby * self.vol.mbw + mbx;
        let (x0, y0) = (mbx as i32 * 16, mby as i32 * 16);
        let pred16 = self.pred_motion(mbx, mby, 0);
        let (mv16, c16) = self.search(&refp, x0, y0, 16, pred16, false);
        let mut preds4 = [[0i32; 2]; 4];
        let mut mv4 = [[0i32; 2]; 4];
        let mut c4 = 0u32;
        for i in 0..4 {
            let (bx, by) = (x0 + (i as i32 & 1) * 8, y0 + (i as i32 >> 1) * 8);
            let p = self.pred_motion(mbx, mby, i);
            let (m, c) = self.search(&refp, bx, by, 8, p, true);
            preds4[i] = p;
            mv4[i] = m;
            c4 += c;
            self.set_block_mv(mbx, mby, i, m);
        }
        let four = c4 + FOUR_MV_PENALTY < c16;        let mvs = if four { mv4 } else { [mv16; 4] };
        if !four {
            self.set_mb_mv(mbx, mby, mv16);
        }
        if self.intra_cost(mbx, mby) + (INTRA_PENALTY * self.q as u32) < if four { c4 } else { c16 } {
            return self.intra_mb(mbx, mby, true);
        }
        let (mut py, mut pu, mut pv) = ([0u8; 256], [0u8; 64], [0u8; 64]);
        self.predict(&refp, mbx, mby, &mvs, four, &mut py, &mut pu, &mut pv);
        let lv = self.inter_levels(mbx, mby, &py, &pu, &pv);
        let cbp: [bool; 6] = std::array::from_fn(|n| lv[n].iter().any(|&v| v != 0));
        if !cbp.iter().any(|&c| c) && mvs.iter().all(|m| *m == [0, 0]) {
            self.bw.bit(true); // not coded
            self.cur.mbf[mb] = NOT_CODED;
            self.reset_pred(mbx, mby);
            self.set_mb_mv(mbx, mby, [0, 0]);
            self.store_pred(mbx, mby, &py, &pu, &pv);
            return;
        }
        self.bw.bit(false); // coded
        let cbpc = ((cbp[4] as usize) << 1) | cbp[5] as usize;
        let cbpy = (0..4).fold(0usize, |acc, n| acc | ((cbp[n] as usize) << (3 - n)));
        self.put_inter_mcbpc(cbpc | if four { 16 } else { 0 });
        self.put_cbpy(cbpy ^ 0xF);
        if four {
            for i in 0..4 {
                self.put_mv(mv4[i][0] - preds4[i][0]);
                self.put_mv(mv4[i][1] - preds4[i][1]);
            }
        } else {
            self.put_mv(mv16[0] - pred16[0]);
            self.put_mv(mv16[1] - pred16[1]);
        }
        for n in 0..6 {
            if cbp[n] {
                self.put_block(true, &T::ZIGZAG, -1, &lv[n]);
            }
        }
        self.reset_pred(mbx, mby);
        self.cur.mbf[mb] = if four { FOUR_MV } else { 0 };
        self.store_pred(mbx, mby, &py, &pu, &pv);
        for n in 0..6 {
            if cbp[n] {
                let mut blk = dequant(&lv[n], false, self.q, 0);
                let (plane, off, stride) = self.block_dst(mbx, mby, n);
                idct::add(&mut blk, &mut self.cur.planes[plane][off..], stride);
            }
        }
    }
}
