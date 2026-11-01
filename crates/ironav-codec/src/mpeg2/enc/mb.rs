//! The slice and macroblock layers of the MPEG-2 encoder: the motion search, the intra, forward,
//! backward and bidirectional decisions, the coefficient levels, the bits, and the reconstruction. The
//! prediction, the mismatch control and the inverse transform are the decoder's (`mpeg2/mc.rs`,
//! `mpeg2/mb.rs` and `mpeg4/idct.rs`), so each macroblock is reconstructed exactly as the decoder will
//! reconstruct it. The coefficient writer follows FFmpeg's `mpeg12enc.c` (Fabrice Bellard, Michael
//! Niedermayer and the FFmpeg developers, LGPL-2.1-or-later).

use super::{BitW, Source, KIND_I, KIND_P, SEARCH};
use crate::mpeg2::mc::{self, View};
use crate::mpeg2::tables as T;
use crate::mpeg2::{Frame as Pic, Geo};
use crate::mpeg4::idct;
use std::sync::{Arc, OnceLock};

/// The cost of a unit of motion vector difference, in summed absolute differences.
const MV_LAMBDA: u32 = 2;
/// Intra is chosen over inter only when its cost, plus this much per quantiser step, is lower.
const INTRA_PENALTY: u32 = 16;

/// The run-level codes of the table of the first kind (`MPEG1_VLC`, which the intra and the inter blocks
/// of MPEG-2 use with `intra_vlc_format` 0): the symbol of each (run, level) pair, plus one, for runs
/// below 32 and levels up to 40. The escape is the symbol after the last pair, and the end of block the one after that.
const RL_ESCAPE: usize = T::RL_SYMBOLS;
const RL_EOB: usize = T::RL_SYMBOLS + 1;

fn rl_lut() -> &'static [[u8; 41]; 32] {
    static L: OnceLock<[[u8; 41]; 32]> = OnceLock::new();
    L.get_or_init(|| {
        let mut l = [[0u8; 41]; 32];
        for s in 0..T::RL_SYMBOLS {
            let (r, a) = (T::RUN[s] as usize, T::LEVEL[s] as usize);
            if r < 32 && a <= 40 && l[r][a] == 0 {
                l[r][a] = s as u8 + 1;
            }
        }
        l
    })
}

/// The intra quantiser: the nearest level, where level `l` reconstructs to `l * step`.
fn q_intra(f: f32, step: f32) -> i32 {
    let l = (f.abs() / step + 0.5).floor() as i32;
    if f < 0.0 {
        -l
    } else {
        l
    }
}

/// The non-intra quantiser: level `l` reconstructs to `(l + 1/2) * step` (`(2l + 1) * step / 2`), and zero
/// is zero, so a residual below three quarters of a step is not coded.
fn q_inter(f: f32, step: f32) -> i32 {
    let x = f.abs() / step;
    let l = if x < 0.75 { 0 } else { (x.floor() as i32).max(1) };
    if f < 0.0 {
        -l
    } else {
        l
    }
}

/// The dequantised coefficients of an intra block as the decoder reconstructs them (`mpeg2/mb.rs`): the DC
/// is `dc * 8`, each AC level `l` is `l * q * m / 16`, and the mismatch control sets the last coefficient's
/// low bit to the parity of all of them.
fn recon_intra(lv: &[i32; 64], dc: i32, qs: i32, m: &[u8; 64]) -> [i16; 64] {
    let mut v = [0i32; 64];
    v[0] = dc * 8;
    for k in 1..64 {
        let mag = (lv[k].abs() * qs * m[k] as i32) >> 4;
        v[k] = if lv[k] < 0 { -mag } else { mag };
    }
    let mut x = v[0] ^ 1;
    for &c in &v[1..] {
        x ^= c;
    }
    finish(&v, x)
}

/// The dequantised coefficients of a non-intra block: level `l` is `(2|l| + 1) * q * m / 32`, signed.
fn recon_inter(lv: &[i32; 64], qs: i32, m: &[u8; 64]) -> [i16; 64] {
    let mut v = [0i32; 64];
    for k in 0..64 {
        if lv[k] != 0 {
            let mag = ((2 * lv[k].abs() + 1) * qs * m[k] as i32) >> 5;
            v[k] = if lv[k] < 0 { -mag } else { mag };
        }
    }
    let mut x = 1;
    for &c in &v {
        x ^= c;
    }
    finish(&v, x)
}

/// The coefficients as 16-bit values, with the mismatch control's bit in the last one.
fn finish(v: &[i32; 64], x: i32) -> [i16; 64] {
    let mut out = [0i16; 64];
    for k in 0..64 {
        out[k] = v[k] as i16;
    }
    out[63] ^= (x & 1) as i16;
    out
}

/// The picture being coded: its geometry, the source, the references, the predictors, and the bits.
pub(super) struct Coder<'a> {
    geo: Geo,
    src: &'a Source,
    /// The forward reference: a P picture's last anchor, a B picture's past anchor.
    refp: Option<Arc<Pic>>,
    /// A B picture's backward reference, the anchor after it.
    bref: Option<Arc<Pic>>,
    /// The last motion vectors coded in each direction (the predictors), and the last DC of each
    /// component (the luma, U and V of the slice).
    last_mv: [[i32; 2]; 2],
    last_dc: [i32; 3],
    cur: Pic,
    bw: BitW,
    /// The quantiser code (1 to 31) and the quantiser scale it stands for (linear: twice the code).
    code: i32,
    qs: i32,
    fcode: i32,
    /// The orthonormal 8-point DCT basis: `cos[k][x]` (k the frequency, x the sample).
    cos: [[f32; 8]; 8],
}

impl<'a> Coder<'a> {
    pub fn new(geo: Geo, src: &'a Source, refs: [Option<Arc<Pic>>; 2], code: i32, fcode: i32, bw: BitW, pts: i64) -> Self {
        let [refp, bref] = refs;
        let mut cos = [[0f32; 8]; 8];
        for (k, row) in cos.iter_mut().enumerate() {
            let ck = if k == 0 { 1.0 / 2f32.sqrt() } else { 1.0 };
            for (x, v) in row.iter_mut().enumerate() {
                *v = ck / 2.0 * (((2 * x + 1) * k) as f32 * std::f32::consts::PI / 16.0).cos();
            }
        }
        Coder { geo, src, refp, bref, last_mv: [[0; 2]; 2], last_dc: [128; 3], cur: Pic::new(&geo, pts, 0), bw, code, qs: 2 * code, fcode, cos }
    }

    /// The picture's bits and its reconstruction.
    pub fn finish(self) -> (Vec<u8>, Pic) {
        (self.bw.finish(), self.cur)
    }

    /// The slices of a picture: one per macroblock row, each starting at its row's start code, with the
    /// quantiser code and no extra information.
    pub fn slices(&mut self, kind: u8) {
        for mby in 0..self.geo.mbh {
            self.bw.bytes(&[0, 0, 1, mby as u8 + 1]);
            self.bw.put(self.code as u32, 5);
            self.bw.bit(false);
            self.last_mv = [[0; 2]; 2];
            self.last_dc = [128; 3];
            let mut skip = 0usize;
            for mbx in 0..self.geo.mbw {
                let last = mbx + 1 == self.geo.mbw;
                match kind {
                    KIND_I => {
                        self.put_incr(0);
                        self.bw.bit(true); // intra, no quantiser change
                        self.intra_blocks(mbx, mby);
                    }
                    KIND_P => self.p_mb(mbx, mby, last, &mut skip),
                    _ => self.b_mb(mbx, mby),
                }
            }
        }
    }

    /// The plane, offset and stride of block `n`'s top left sample in the picture.
    fn block_dst(&self, mbx: usize, mby: usize, n: usize) -> (usize, usize, usize) {
        let (lw, cw) = (self.geo.mbw * 16, self.geo.mbw * 8);
        if n < 4 {
            (0, (mby * 16 + (n >> 1) * 8) * lw + mbx * 16 + (n & 1) * 8, lw)
        } else {
            (n - 3, mby * 8 * cw + mbx * 8, cw)
        }
    }

    /// The source samples of block `n`, as integers.
    fn src_block(&self, mbx: usize, mby: usize, n: usize) -> [i32; 64] {
        let (lw, cw) = (self.geo.mbw * 16, self.geo.mbw * 8);
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

    /// The prediction of block `n` as it is in the picture now.
    fn pred_block(&self, mbx: usize, mby: usize, n: usize) -> [i32; 64] {
        let (p, off, stride) = self.block_dst(mbx, mby, n);
        let mut out = [0i32; 64];
        for y in 0..8 {
            for x in 0..8 {
                out[y * 8 + x] = self.cur.p[p].d[off + y * stride + x] as i32;
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

    /// The skip count before a coded macroblock: the address increment. Each escape adds 33; the
    /// code for `n` (0 to 32) is the symbol `n`.
    fn put_incr(&mut self, mut n: usize) {
        while n >= 33 {
            self.bw.put(T::MBINCR[2 * 33] as u32, T::MBINCR[2 * 33 + 1] as u32);
            n -= 33;
        }
        self.bw.put(T::MBINCR[2 * n] as u32, T::MBINCR[2 * n + 1] as u32);
    }

    fn put_ptype(&mut self, flags: u8) {
        let i = T::PTYPE_FLAGS.iter().position(|&f| f == flags).expect("a P macroblock type of these flags");
        self.bw.put(T::PTYPE_CODE[i].0, T::PTYPE_CODE[i].1);
    }

    fn put_btype(&mut self, flags: u8) {
        let i = T::BTYPE_FLAGS.iter().position(|&f| f == flags).expect("a B macroblock type of these flags");
        self.bw.put(T::BTYPE_CODE[i].0, T::BTYPE_CODE[i].1);
    }

    fn put_mbpat(&mut self, cbp: usize) {
        self.bw.put(T::MBPAT[2 * cbp] as u32, T::MBPAT[2 * cbp + 1] as u32);
    }

    /// A motion vector difference: the motion code with its sign and residual, for the picture's f_code.
    fn put_mv(&mut self, d: i32) {
        if d == 0 {
            self.bw.put(T::MV[0] as u32, T::MV[1] as u32);
            return;
        }
        let a = d.unsigned_abs() - 1;
        let shift = (self.fcode - 1) as u32;
        let code = ((a >> shift) + 1) as usize;
        self.bw.put(T::MV[2 * code] as u32, T::MV[2 * code + 1] as u32);
        self.bw.bit(d < 0);
        if shift > 0 {
            self.bw.put(a & ((1 << shift) - 1), shift);
        }
    }

    /// A DC difference: its size with the DC VLC, then its bits (for a negative one, the bits of
    /// `diff + 2^size - 1`).
    fn put_dc(&mut self, chroma: bool, diff: i32) {
        let size = (32 - diff.unsigned_abs().leading_zeros()) as usize;
        let (codes, bits) = if chroma { (&T::DC_CHROMA_CODE, &T::DC_CHROMA_BITS) } else { (&T::DC_LUM_CODE, &T::DC_LUM_BITS) };
        self.bw.put(codes[size] as u32, bits[size] as u32);
        if size > 0 {
            let v = if diff > 0 { diff as u32 } else { (diff + (1 << size) - 1) as u32 };
            self.bw.put(v, size as u32);
        }
    }

    /// One non-zero level after `run` zeros: its symbol and sign if the table has it, else an escape
    /// with the run in six bits and the level in twelve.
    fn put_rl(&mut self, run: usize, level: i32) {
        let a = level.unsigned_abs() as usize;
        let s = if run < 32 && a <= 40 { rl_lut()[run][a] } else { 0 };
        if s != 0 {
            let s = s as usize - 1;
            self.bw.put(T::MPEG1_VLC[2 * s] as u32, T::MPEG1_VLC[2 * s + 1] as u32);
            self.bw.bit(level < 0);
        } else {
            self.bw.put(T::MPEG1_VLC[2 * RL_ESCAPE] as u32, T::MPEG1_VLC[2 * RL_ESCAPE + 1] as u32);
            self.bw.put(run as u32, 6);
            self.bw.put((level as u32) & 0xFFF, 12);
        }
    }

    /// The levels of a block in scan order, then the end of block. An intra block's DC is coded apart, so
    /// its AC run starts after position 0. A non-intra block whose first level is +-1 codes it in two bits,
    /// the sign after a one.
    fn put_block(&mut self, lv: &[i32; 64], intra: bool) {
        let mut prev: i32 = if intra { 0 } else { -1 };
        for p in (if intra { 1 } else { 0 })..64 {
            let v = lv[T::ZIGZAG[p] as usize];
            if v == 0 {
                continue;
            }
            if !intra && p == 0 && v.abs() == 1 {
                self.bw.bit(true);
                self.bw.bit(v < 0);
            } else {
                self.put_rl((p as i32 - prev - 1) as usize, v);
            }
            prev = p as i32;
        }
        let eob = RL_EOB;
        self.bw.put(T::MPEG1_VLC[2 * eob] as u32, T::MPEG1_VLC[2 * eob + 1] as u32);
    }

    /// The prediction of the macroblock into the picture: the first part as it is, each later one
    /// averaged in (the second direction of a bidirectional macroblock). Luma as the decoder predicts it,
    /// and the chroma from the vector halved.
    fn predict(&mut self, mbx: usize, mby: usize, parts: &[(Arc<Pic>, [i32; 2])]) {
        let (x0, y0) = (16 * mbx, 16 * mby);
        let (cx0, cy0) = (8 * mbx, 8 * mby);
        for (i, (r, mv)) in parts.iter().enumerate() {
            let avg = i > 0;
            let (mx, my) = (mv[0], mv[1]);
            mc::block(&mut self.cur.p[0], View::FRAME, x0, y0, &r.p[0], View::FRAME, x0 as i32 + (mx >> 1), y0 as i32 + (my >> 1), (mx & 1) as usize, (my & 1) as usize, 16, 16, avg);
            let (cmx, cmy) = (mx / 2, my / 2);
            for p in 1..3 {
                mc::block(&mut self.cur.p[p], View::FRAME, cx0, cy0, &r.p[p], View::FRAME, cx0 as i32 + (cmx >> 1), cy0 as i32 + (cmy >> 1), (cmx & 1) as usize, (cmy & 1) as usize, 8, 8, avg);
            }
        }
    }

    /// The summed absolute luma difference of the macroblock's prediction (in the picture) from the source.
    fn cur_sad(&self, mbx: usize, mby: usize) -> u32 {
        let lw = self.geo.mbw * 16;
        let mut s = 0u32;
        for y in 0..16 {
            let o = (mby * 16 + y) * lw + mbx * 16;
            for x in 0..16 {
                s += (self.cur.p[0].d[o + x] as i32 - self.src.y[o + x] as i32).unsigned_abs();
            }
        }
        s
    }

    /// The cost of vector `mv` for the macroblock at (`mbx`, `mby`) predicted from `refp`: the summed
    /// absolute difference of its luma, and the vector's difference from `pred`. The prediction is written
    /// into the picture, which is then overwritten by the chosen one.
    fn mv_cost(&mut self, refp: &Pic, mbx: usize, mby: usize, pred: [i32; 2], mv: [i32; 2]) -> u32 {
        let (x0, y0) = (16 * mbx, 16 * mby);
        mc::block(&mut self.cur.p[0], View::FRAME, x0, y0, &refp.p[0], View::FRAME, x0 as i32 + (mv[0] >> 1), y0 as i32 + (mv[1] >> 1), (mv[0] & 1) as usize, (mv[1] & 1) as usize, 16, 16, false);
        let sad = self.cur_sad(mbx, mby);
        sad + MV_LAMBDA * ((mv[0] - pred[0]).unsigned_abs() + (mv[1] - pred[1]).unsigned_abs())
    }

    /// The motion search for the macroblock: a diamond search from the zero and predicted vectors, then
    /// the eight half-sample neighbours of the best. Returns the vector and its cost.
    fn search(&mut self, refp: &Pic, mbx: usize, mby: usize, pred: [i32; 2]) -> ([i32; 2], u32) {
        let clampv = |m: [i32; 2]| [m[0].clamp(-SEARCH, SEARCH), m[1].clamp(-SEARCH, SEARCH)];
        let mut best = clampv(pred);
        let mut bc = self.mv_cost(refp, mbx, mby, pred, best);
        let c = self.mv_cost(refp, mbx, mby, pred, [0, 0]);
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
                let c = self.mv_cost(refp, mbx, mby, pred, m);
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
                let c = self.mv_cost(refp, mbx, mby, pred, m);
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

    /// The coefficient levels of the six blocks of an inter macroblock: each block's source minus the
    /// prediction now in the picture, transformed and quantised.
    fn inter_levels(&self, mbx: usize, mby: usize) -> [[i32; 64]; 6] {
        let mut out = [[0i32; 64]; 6];
        let step = self.qs as f32 / 16.0;
        for (n, lv) in out.iter_mut().enumerate() {
            let s = self.src_block(mbx, mby, n);
            let p = self.pred_block(mbx, mby, n);
            let mut d = [0i32; 64];
            for k in 0..64 {
                d[k] = s[k] - p[k];
            }
            let f = self.fdct(&d);
            for (k, l) in lv.iter_mut().enumerate() {
                *l = q_inter(f[k], step * T::INTER_MATRIX[k] as f32);
            }
        }
        out
    }

    /// The coded blocks' bit pattern: block `n` is bit `5 - n`.
    fn cbp_of(lv: &[[i32; 64]; 6]) -> (usize, [bool; 6]) {
        let coded: [bool; 6] = std::array::from_fn(|n| lv[n].iter().any(|&v| v != 0));
        let cbp = (0..6).fold(0usize, |acc, n| acc | ((coded[n] as usize) << (5 - n)));
        (cbp, coded)
    }

    /// Adds the residual of the coded blocks to the prediction in the picture.
    fn add_residual(&mut self, mbx: usize, mby: usize, lv: &[[i32; 64]; 6], coded: &[bool; 6]) {
        for n in 0..6 {
            if coded[n] {
                let mut blk = recon_inter(&lv[n], self.qs, &T::INTER_MATRIX);
                let (p, off, stride) = self.block_dst(mbx, mby, n);
                idct::add(&mut blk, &mut self.cur.p[p].d[off..], stride);
            }
        }
    }

    /// The six blocks of an intra macroblock, after its type: each block's DC as a difference from the
    /// last one of its component, then its levels. The picture gets the reconstruction.
    fn intra_blocks(&mut self, mbx: usize, mby: usize) {
        let mut lv = [[0i32; 64]; 6];
        let mut dc = [0i32; 6];
        let step = self.qs as f32 / 16.0;
        for n in 0..6 {
            let f = self.fdct(&self.src_block(mbx, mby, n));
            dc[n] = ((f[0] / 8.0).round() as i32).clamp(0, 255);
            for k in 1..64 {
                lv[n][k] = q_intra(f[k], step * T::INTRA_MATRIX[k] as f32);
            }
        }
        for n in 0..6 {
            let comp = if n < 4 { 0 } else { n - 3 };
            let diff = dc[n] - self.last_dc[comp];
            self.put_dc(n >= 4, diff);
            self.last_dc[comp] = dc[n];
            self.put_block(&lv[n], true);
        }
        for n in 0..6 {
            let mut blk = recon_intra(&lv[n], dc[n], self.qs, &T::INTRA_MATRIX);
            let (p, off, stride) = self.block_dst(mbx, mby, n);
            idct::put(&mut blk, &mut self.cur.p[p].d[off..], stride);
        }
    }

    /// A P-picture macroblock: skipped when it has no residual and no motion (and is neither the first nor
    /// the last of its row: a slice starts and ends on coded macroblocks), intra when intra is cheaper than the motion
    /// search, else forward with its vector and coefficients.
    fn p_mb(&mut self, mbx: usize, mby: usize, last: bool, skip: &mut usize) {
        let refp = self.refp.clone().expect("a P picture has a reference");
        let (mv, c) = self.search(&refp, mbx, mby, self.last_mv[0]);
        self.predict(mbx, mby, &[(refp.clone(), mv)]);
        if self.intra_cost(mbx, mby) + INTRA_PENALTY * (self.qs as u32) < c {
            self.put_incr(*skip);
            *skip = 0;
            self.put_ptype(T::F_INTRA);
            self.intra_blocks(mbx, mby);
            self.last_mv = [[0; 2]; 2];
            return;
        }
        let lv = self.inter_levels(mbx, mby);
        let (cbp, coded) = Self::cbp_of(&lv);
        // the first and last macroblocks of a slice are never skipped
        if cbp == 0 && mv == [0, 0] && !last && mbx != 0 {
            // the zero-vector prediction is the reconstruction; the predictors reset as the decoder's do
            *skip += 1;
            self.last_mv[0] = [0, 0];
            self.last_dc = [128; 3];
            return;
        }
        self.put_incr(*skip);
        *skip = 0;
        self.put_ptype(if cbp != 0 { T::F_FWD | T::F_CBP } else { T::F_FWD });
        let pred = self.last_mv[0];
        self.put_mv(mv[0] - pred[0]);
        self.put_mv(mv[1] - pred[1]);
        if cbp != 0 {
            self.put_mbpat(cbp);
        }
        for n in 0..6 {
            if coded[n] {
                self.put_block(&lv[n], false);
            }
        }
        self.last_mv[0] = mv;
        self.last_dc = [128; 3];
        self.add_residual(mbx, mby, &lv, &coded);
    }

    /// A B-picture macroblock: forward, backward or bidirectional, whichever of the three predicts best,
    /// with its residual as a P picture's. B macroblocks are always coded (there is no skip and no
    /// intra), since MPEG-2 has no direct mode.
    fn b_mb(&mut self, mbx: usize, mby: usize) {
        let fwd = self.refp.clone().expect("a B picture has a past anchor");
        let bwd = self.bref.clone().expect("a B picture has a future anchor");
        let (last0, last1) = (self.last_mv[0], self.last_mv[1]);
        let (mf, cf) = self.search(&fwd, mbx, mby, last0);
        let (mb, cb) = self.search(&bwd, mbx, mby, last1);
        self.predict(mbx, mby, &[(fwd.clone(), mf), (bwd.clone(), mb)]);
        let vec_cost = |v: [i32; 2], p: [i32; 2]| MV_LAMBDA * ((v[0] - p[0]).unsigned_abs() + (v[1] - p[1]).unsigned_abs());
        let c_bi = self.cur_sad(mbx, mby) + vec_cost(mf, last0) + vec_cost(mb, last1);
        // the chosen direction: 0 forward, 1 backward, 2 bidirectional
        let mode = [(cf, 0usize), (cb, 1), (c_bi, 2)].into_iter().min_by_key(|c| c.0).map_or(0, |c| c.1);
        let parts: Vec<(Arc<Pic>, [i32; 2])> = match mode {
            0 => vec![(fwd.clone(), mf)],
            1 => vec![(bwd.clone(), mb)],
            _ => vec![(fwd.clone(), mf), (bwd.clone(), mb)],
        };
        self.predict(mbx, mby, &parts);
        let lv = self.inter_levels(mbx, mby);
        let (cbp, coded) = Self::cbp_of(&lv);
        let mut flags = match mode {
            0 => T::F_FWD,
            1 => T::F_BWD,
            _ => T::F_FWD | T::F_BWD,
        };
        if cbp != 0 {
            flags |= T::F_CBP;
        }
        self.put_incr(0);
        self.put_btype(flags);
        if mode != 1 {
            self.put_mv(mf[0] - last0[0]);
            self.put_mv(mf[1] - last0[1]);
            self.last_mv[0] = mf;
        }
        if mode != 0 {
            self.put_mv(mb[0] - last1[0]);
            self.put_mv(mb[1] - last1[1]);
            self.last_mv[1] = mb;
        }
        if cbp != 0 {
            self.put_mbpat(cbp);
        }
        for n in 0..6 {
            if coded[n] {
                self.put_block(&lv[n], false);
            }
        }
        self.last_dc = [128; 3];
        self.add_residual(mbx, mby, &lv, &coded);
    }
}
