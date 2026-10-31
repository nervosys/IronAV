//! The picture and macroblock layers of MPEG-4 Part 2 (progressive, rectangular): the video packet
//! headers and resync markers, the macroblock types, motion vectors and their prediction, direct
//! mode, coefficients with DC and AC prediction, dequantisation, and reconstruction. The syntax and
//! the prediction rules follow FFmpeg's `mpeg4videodec.c`, `h263.c` and `mpegvideo_unquantize.c`
//! (Fabrice Bellard, Michael Niedermayer and the FFmpeg developers, LGPL-2.1-or-later).

use super::bits::Bits;
use super::idct;
use super::mc::{self, Plane};
use super::tables as T;
use super::{Pic, Tables, Vol, FOUR_MV, INTRA, NOT_CODED};
use ironav_core::{Error, ErrorCode, Result};
use std::sync::Arc;

const QUANT_TAB: [i32; 4] = [-1, -2, 1, 2];
const CHROMA_ROUND: [i32; 16] = [0, 0, 0, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 1, 1];

fn bad(msg: &str) -> Error {
    Error::new(ErrorCode::InvalidInput, format!("MPEG-4: {msg}"))
}

/// The motion of a macroblock: the directions it predicts from, and each direction's vector per
/// 8x8 block (all four equal unless `four` is set, which also selects the 8x8 filter).
#[derive(Clone, Copy, Default)]
struct Plan {
    fwd: bool,
    bwd: bool,
    four: bool,
    mv: [[[i32; 2]; 4]; 2],
}

/// The grid (0 luma, 1 and 2 chroma) and the block coordinates in it of block `n` of a macroblock.
fn block_pos(mbx: usize, mby: usize, n: usize) -> (usize, usize, usize) {
    if n < 4 {
        (0, 2 * mbx + (n & 1), 2 * mby + (n >> 1))
    } else {
        (n - 3, mbx, mby)
    }
}

/// The median of three values.
fn median(a: i32, b: i32, c: i32) -> i32 {
    a.max(b).min(a.min(b).max(c))
}

/// A neighbour's AC coefficient brought to the quantiser of the current macroblock.
fn scale_ac(v: i32, nq: i32, q: i32) -> i32 {
    let p = v * nq;
    let half = q / 2;
    (if p >= 0 { p + half } else { p - half }) / q
}

/// One picture being decoded: its header state, the reference pictures, and the predictors.
pub(crate) struct Vop<'a> {
    t: &'a Tables,
    vol: &'a Vol,
    br: Bits<'a>,
    pub pict: u8,
    pub qscale: i32,
    pub fcode: [i32; 2],
    /// The rounding control of a P-VOP: no rounding when set.
    pub nr: bool,
    pub dc_thr: i32,
    pub cur: Pic,
    pub fwd_ref: Option<Arc<Pic>>,
    pub bwd_ref: Option<Arc<Pic>>,
    pub pp_time: i64,
    pub pb_time: i64,
    /// The predicted DC (full scale) and AC levels of each block, per grid.
    dc: [Vec<i32>; 3],
    ac: [Vec<[i32; 16]>; 3],
    mb_qs: Vec<i32>,
    last_mv: [[i32; 2]; 2],
    /// The first macroblock of the current video packet: nothing before it is predicted from.
    packet_start: usize,
    coef: [[i32; 64]; 6],
}

impl<'a> Vop<'a> {
    pub fn new(t: &'a Tables, vol: &'a Vol, br: Bits<'a>, cur: Pic) -> Self {
        let n = vol.mbw * vol.mbh;
        Vop {
            t,
            vol,
            br,
            pict: 0,
            qscale: 1,
            fcode: [1, 1],
            nr: false,
            dc_thr: 0,
            cur,
            fwd_ref: None,
            bwd_ref: None,
            pp_time: 1,
            pb_time: 0,
            dc: [vec![1024; 4 * n], vec![1024; n], vec![1024; n]],
            ac: [vec![[0; 16]; 4 * n], vec![[0; 16]; n], vec![[0; 16]; n]],
            mb_qs: vec![1; n],
            last_mv: [[0; 2]; 2],
            packet_start: 0,
            coef: [[0; 64]; 6],
        }
    }

    fn gdims(&self, g: usize) -> (i32, i32) {
        let (w, h) = (self.vol.mbw as i32, self.vol.mbh as i32);
        if g == 0 {
            (2 * w, 2 * h)
        } else {
            (w, h)
        }
    }

    /// The macroblock index of block (bx, by) of grid `g`.
    fn mb_of(&self, g: usize, bx: i32, by: i32) -> usize {
        let w = self.vol.mbw as i32;
        if g == 0 {
            ((by / 2) * w + bx / 2) as usize
        } else {
            (by * w + bx) as usize
        }
    }

    /// Whether block (bx, by) of grid `g` is inside the picture and the current packet.
    fn avail(&self, g: usize, bx: i32, by: i32) -> bool {
        let (gw, gh) = self.gdims(g);
        bx >= 0 && by >= 0 && bx < gw && by < gh && self.mb_of(g, bx, by) >= self.packet_start
    }

    fn idx(&self, g: usize, bx: i32, by: i32) -> usize {
        (by * self.gdims(g).0 + bx) as usize
    }

    fn reset_pred(&mut self, mbx: usize, mby: usize) {
        for n in 0..6 {
            let (g, bx, by) = block_pos(mbx, mby, n);
            let i = self.idx(g, bx as i32, by as i32);
            self.dc[g][i] = 1024;
            self.ac[g][i] = [0; 16];
        }
    }

    fn vlc_err(&self) -> Error {
        bad("damaged macroblock data")
    }

    fn dquant(&mut self) {
        let d = QUANT_TAB[self.br.read(2) as usize];
        self.qscale = (self.qscale + d).clamp(1, 31);
    }

    fn dc_scale(&self, n: usize) -> i32 {
        let q = self.qscale.clamp(0, 31) as usize;
        if n < 4 {
            T::Y_DC_SCALE[q] as i32
        } else {
            T::C_DC_SCALE[q] as i32
        }
    }

    /// Decodes the picture: its macroblocks, with a video packet header wherever a resync marker is.
    pub fn run(&mut self) -> Result<()> {
        let n = self.vol.mbw * self.vol.mbh;
        let mut mb = 0usize;
        while mb < n {
            if mb > 0 && self.vol.resync && self.try_resync() {
                let from = mb;
                mb = self.packet_header(n)?;
                for m in from..mb {
                    self.skipped_mb(m)?;
                }
            }
            self.macroblock(mb)?;
            mb += 1;
            if self.br.overrun() {
                return Err(bad("truncated picture"));
            }
        }
        Ok(())
    }

    /// Whether a resync marker starts at the current position (a stuffing bit, alignment, then at
    /// least 16 + f_code - 1 zero bits and a one). If so, the marker is consumed.
    fn try_resync(&mut self) -> bool {
        let k = self.br.pos & 7;
        let prefix = [0x7F00u32, 0x7E00, 0x7C00, 0x7800, 0x7000, 0x6000, 0x4000, 0x0000][k];
        if self.br.peek(16) != prefix {
            return false;
        }
        let save = self.br.pos;
        self.br.skip(1);
        self.br.align();
        let mut zeros = 0usize;
        while zeros < 32 && !self.br.bit() {
            zeros += 1;
        }
        let need = match self.pict {
            0 => 16,
            1 => 15 + self.fcode[0] as usize,
            _ => 15 + self.fcode[0].max(self.fcode[1]).max(2) as usize,
        };
        if zeros >= need && !self.br.overrun() {
            true
        } else {
            self.br.pos = save;
            false
        }
    }

    /// The video packet header after a resync marker: the first macroblock of the packet, the
    /// quantiser, and the header extension when it is present. Returns the first macroblock.
    fn packet_header(&mut self, n: usize) -> Result<usize> {
        let bits = ((usize::BITS - (n - 1).leading_zeros()) as usize).max(1);
        let mb = self.br.read(bits as u32) as usize;
        if mb == 0 || mb >= n {
            return Err(bad("illegal macroblock number in a video packet"));
        }
        let q = self.br.read(self.vol.quant_precision) as i32;
        if q != 0 {
            self.qscale = q.clamp(1, 31);
        }
        if self.br.bit() {
            while self.br.bit() {}
            self.br.skip(1);
            self.br.skip(self.vol.time_bits as usize);
            self.br.skip(1);
            self.br.skip(2);
            if self.pict != 0 {
                self.fcode[0] = self.br.read(3) as i32;
            }
            if self.pict == 2 {
                self.fcode[1] = self.br.read(3) as i32;
            }
        }
        self.packet_start = mb;
        Ok(mb)
    }

    fn macroblock(&mut self, mb: usize) -> Result<()> {
        let w = self.vol.mbw;
        let (mbx, mby) = (mb % w, mb / w);
        self.mb_qs[mb] = self.qscale;
        match self.pict {
            0 => {
                let mut cbpc;
                loop {
                    cbpc = self.t.intra_mcbpc.read(&mut self.br).ok_or_else(|| self.vlc_err())?;
                    if cbpc != 8 {
                        break;
                    }
                }
                self.intra_mb(mb, mbx, mby, cbpc as u32 & 3, cbpc & 4 != 0)
            }
            1 => self.p_mb(mb, mbx, mby),
            _ => self.b_mb(mb, mbx, mby),
        }
    }

    fn set_block_mv(&mut self, mbx: usize, mby: usize, i: usize, mv: [i32; 2]) {
        let gw = 2 * self.vol.mbw;
        let (bx, by) = (2 * mbx + (i & 1), 2 * mby + (i >> 1));
        self.cur.mv[by * gw + bx] = [mv[0] as i16, mv[1] as i16];
    }

    fn set_mb_mv(&mut self, mbx: usize, mby: usize, mv: [i32; 2]) {
        for i in 0..4 {
            self.set_block_mv(mbx, mby, i, mv);
        }
    }

    /// The motion vector of a block of the luma grid, zero outside the packet and picture.
    fn mv_at(&self, bx: i32, by: i32) -> [i32; 2] {
        if self.avail(0, bx, by) {
            let gw = 2 * self.vol.mbw as i32;
            let m = self.cur.mv[(by * gw + bx) as usize];
            [m[0] as i32, m[1] as i32]
        } else {
            [0, 0]
        }
    }

    /// The predicted vector of block `i` of a macroblock: the median of the left, above and
    /// above-right vectors. Where the block is in the top row of a packet or picture the left
    /// vector is used alone.
    fn pred_motion(&self, mbx: usize, mby: usize, i: usize) -> (i32, i32) {
        let (bx, by) = ((2 * mbx + (i & 1)) as i32, (2 * mby + (i >> 1)) as i32);
        let a = self.mv_at(bx - 1, by);
        if !self.avail(0, bx, by - 1) {
            return (a[0], a[1]);
        }
        let b = self.mv_at(bx, by - 1);
        let c = if i == 3 { self.mv_at(bx - 1, by - 1) } else { self.mv_at(bx + 1, by - 1) };
        (median(a[0], b[0], c[0]), median(a[1], b[1], c[1]))
    }

    /// A motion vector component: the difference decoded with `f_code`, added to `pred`.
    fn decode_motion(&mut self, pred: i32, f_code: i32) -> Result<i32> {
        let code = self.t.mv.read(&mut self.br).ok_or_else(|| self.vlc_err())? as i32;
        if code == 0 {
            return Ok(pred);
        }
        let sign = self.br.bit();
        let shift = f_code - 1;
        let mut val = code;
        if shift > 0 {
            val = (((val - 1) << shift) | self.br.read(shift as u32) as i32) + 1;
        }
        if sign {
            val = -val;
        }
        val += pred;
        let bits = 5 + f_code as u32;
        Ok((val << (32 - bits)) >> (32 - bits))
    }

    /// A macroblock that a video packet header passes over. In a B-VOP it is one the future anchor
    /// did not code, so it has no bits here and is a skip; anywhere else it is missing data.
    fn skipped_mb(&mut self, mb: usize) -> Result<()> {
        let skip = self.pict == 2 && self.bwd_ref.as_ref().is_some_and(|f| f.mbf[mb] & NOT_CODED != 0);
        if !skip {
            return Err(bad("video packet skips a coded macroblock"));
        }
        let w = self.vol.mbw;
        self.skip_mb(mb, mb % w, mb / w);
        Ok(())
    }

    fn skip_mb(&mut self, mb: usize, mbx: usize, mby: usize) {
        self.cur.mbf[mb] = NOT_CODED;
        self.reset_pred(mbx, mby);
        self.set_mb_mv(mbx, mby, [0, 0]);
        let plan = Plan { fwd: true, ..Plan::default() };
        self.mc_mb(mbx, mby, &plan);
    }

    fn p_mb(&mut self, mb: usize, mbx: usize, mby: usize) -> Result<()> {
        let cbpc = loop {
            if self.br.bit() {
                self.skip_mb(mb, mbx, mby);
                return Ok(());
            }
            let c = self.t.inter_mcbpc.read(&mut self.br).ok_or_else(|| self.vlc_err())?;
            if c != 20 {
                break c as u32;
            }
        };
        if cbpc & 4 != 0 {
            return self.intra_mb(mb, mbx, mby, cbpc & 3, cbpc & 8 != 0);
        }
        let cbpy = self.t.cbpy.read(&mut self.br).ok_or_else(|| self.vlc_err())? as u32 ^ 0xF;
        let cbp = (cbpc & 3) | (cbpy << 2);
        if cbpc & 8 != 0 {
            self.dquant();
        }
        self.mb_qs[mb] = self.qscale;
        self.reset_pred(mbx, mby);
        let mut plan = Plan { fwd: true, ..Plan::default() };
        if cbpc & 16 == 0 {
            self.cur.mbf[mb] = 0;
            let (px, py) = self.pred_motion(mbx, mby, 0);
            let mx = self.decode_motion(px, self.fcode[0])?;
            let my = self.decode_motion(py, self.fcode[0])?;
            self.set_mb_mv(mbx, mby, [mx, my]);
            plan.mv[0] = [[mx, my]; 4];
        } else {
            self.cur.mbf[mb] = FOUR_MV;
            plan.four = true;
            for i in 0..4 {
                let (px, py) = self.pred_motion(mbx, mby, i);
                let mx = self.decode_motion(px, self.fcode[0])?;
                let my = self.decode_motion(py, self.fcode[0])?;
                self.set_block_mv(mbx, mby, i, [mx, my]);
                plan.mv[0][i] = [mx, my];
            }
        }
        self.residual(cbp)?;
        self.mc_mb(mbx, mby, &plan);
        self.add_residual(mbx, mby, cbp);
        Ok(())
    }

    fn intra_mb(&mut self, mb: usize, mbx: usize, mby: usize, cbpc: u32, dquant: bool) -> Result<()> {
        let ac_pred = self.br.bit();
        let cbpy = self.t.cbpy.read(&mut self.br).ok_or_else(|| self.vlc_err())? as u32;
        let cbp = cbpc | (cbpy << 2);
        let dc_vlc = self.qscale < self.dc_thr;
        if dquant {
            self.dquant();
        }
        self.mb_qs[mb] = self.qscale;
        self.cur.mbf[mb] = INTRA;
        self.set_mb_mv(mbx, mby, [0, 0]);
        for n in 0..6 {
            let coded = cbp >> (5 - n) & 1 != 0;
            self.intra_block(mbx, mby, n, coded, dc_vlc, ac_pred)?;
        }
        for n in 0..6 {
            self.dequant(n, true);
            self.put_block(mbx, mby, n);
        }
        Ok(())
    }

    /// The motion of a B-VOP direct macroblock: each block's forward vector is the co-located
    /// vector of the future anchor scaled by the time distances, plus the coded delta (mx, my).
    fn direct_plan(&self, mb: usize, mbx: usize, mby: usize, mx: i32, my: i32) -> Plan {
        let fut = self.bwd_ref.as_ref().expect("checked by the caller");
        let gw = 2 * self.vol.mbw;
        let (pp, pb) = (self.pp_time, self.pb_time);
        let four = fut.mbf[mb] & FOUR_MV != 0;
        let mut plan = Plan { fwd: true, bwd: true, four: four || self.vol.quarter, ..Plan::default() };
        let scale = |p: i32, num: i64| (p as i64 * num / pp) as i32;
        for i in 0..4 {
            // a macroblock that is not four-vector takes its first block's vector for all four
            let src = if four { i } else { 0 };
            let (bx, by) = (2 * mbx + (src & 1), 2 * mby + (src >> 1));
            let p = fut.mv[by * gw + bx];
            let (px, py) = (p[0] as i32, p[1] as i32);
            let f0 = [scale(px, pb) + mx, scale(py, pb) + my];
            let b1 = [
                if mx != 0 { f0[0] - px } else { scale(px, pb - pp) },
                if my != 0 { f0[1] - py } else { scale(py, pb - pp) },
            ];
            plan.mv[0][i] = f0;
            plan.mv[1][i] = b1;
        }
        plan
    }

    fn b_mb(&mut self, mb: usize, mbx: usize, mby: usize) -> Result<()> {
        if mbx == 0 {
            self.last_mv = [[0; 2]; 2];
        }
        // a macroblock the future anchor skipped is a skip here too
        if self.bwd_ref.as_ref().is_some_and(|f| f.mbf[mb] & NOT_CODED != 0) {
            self.skip_mb(mb, mbx, mby);
            return Ok(());
        }
        let modb1 = self.br.bit();
        let (mt, cbp) = if modb1 {
            (0, 0)
        } else {
            let modb2 = self.br.bit();
            let mt = self.t.mb_type_b.read(&mut self.br).ok_or_else(|| self.vlc_err())?;
            let cbp = if modb2 { 0 } else { self.br.read(6) };
            if mt != 0 && cbp != 0 && self.br.bit() {
                let d = self.br.read(1) as i32 * 4 - 2;
                self.qscale = (self.qscale + d).clamp(1, 31);
            }
            (mt, cbp)
        };
        self.mb_qs[mb] = self.qscale;
        self.cur.mbf[mb] = 0;
        self.reset_pred(mbx, mby);
        let plan = if mt == 0 {
            let (mx, my) = if modb1 { (0, 0) } else { (self.decode_motion(0, 1)?, self.decode_motion(0, 1)?) };
            self.direct_plan(mb, mbx, mby, mx, my)
        } else {
            let mut plan = Plan { fwd: mt == 1 || mt == 3, bwd: mt == 1 || mt == 2, ..Plan::default() };
            if plan.fwd {
                let mx = self.decode_motion(self.last_mv[0][0], self.fcode[0])?;
                let my = self.decode_motion(self.last_mv[0][1], self.fcode[0])?;
                self.last_mv[0] = [mx, my];
                plan.mv[0] = [[mx, my]; 4];
            }
            if plan.bwd {
                let mx = self.decode_motion(self.last_mv[1][0], self.fcode[1])?;
                let my = self.decode_motion(self.last_mv[1][1], self.fcode[1])?;
                self.last_mv[1] = [mx, my];
                plan.mv[1] = [[mx, my]; 4];
            }
            plan
        };
        self.residual(cbp)?;
        self.mc_mb(mbx, mby, &plan);
        self.add_residual(mbx, mby, cbp);
        Ok(())
    }

    /// The coefficients of the coded inter blocks, in block order (cbp bit 5 is block 0).
    fn residual(&mut self, cbp: u32) -> Result<()> {
        for n in 0..6 {
            self.coef[n] = [0; 64];
            if cbp >> (5 - n) & 1 != 0 {
                self.run_levels(n, false, -1, &T::ZIGZAG)?;
            }
        }
        Ok(())
    }

    /// Adds the inverse transform of each coded inter block to the prediction.
    fn add_residual(&mut self, mbx: usize, mby: usize, cbp: u32) {
        for n in 0..6 {
            if cbp >> (5 - n) & 1 != 0 {
                self.dequant(n, false);
                let mut blk = [0i16; 64];
                for i in 0..64 {
                    blk[i] = self.coef[n][i] as i16;
                }
                let (plane, off, stride) = self.block_dst(mbx, mby, n);
                idct::add(&mut blk, &mut self.cur.planes[plane][off..], stride);
            }
        }
    }

    /// Decodes an intra block: the DC (by the DC VLC or as the first run-level coefficient), its
    /// prediction, the AC, and the AC prediction.
    fn intra_block(&mut self, mbx: usize, mby: usize, n: usize, coded: bool, dc_vlc: bool, ac_pred: bool) -> Result<()> {
        self.coef[n] = [0; 64];
        let (pred, top) = self.pred_dc(mbx, mby, n);
        let mut start = -1;
        if dc_vlc {
            let vlc = if n < 4 { &self.t.dc_lum } else { &self.t.dc_chrom };
            let size = vlc.read(&mut self.br).ok_or_else(|| self.vlc_err())? as u32;
            let mut level = 0;
            if size != 0 {
                level = self.br.xbits(size);
                if size > 8 {
                    self.br.skip(1);
                }
            }
            self.coef[n][0] = self.level_dc(mbx, mby, n, pred, level);
            start = 0;
        }
        // with AC prediction the scan follows the direction the DC was predicted in
        let scan: &[u8; 64] = match (ac_pred, top) {
            (false, _) => &T::ZIGZAG,
            (true, false) => &T::ALT_VERTICAL_SCAN,
            (true, true) => &T::ALT_HORIZONTAL_SCAN,
        };
        if coded {
            self.run_levels(n, true, start, scan)?;
        }
        if !dc_vlc {
            let raw = self.coef[n][0];
            self.coef[n][0] = self.level_dc(mbx, mby, n, pred, raw);
        }
        self.predict_ac(mbx, mby, n, top, ac_pred);
        Ok(())
    }

    /// The run-level symbols of a block, each written at its scan position (raster index).
    fn run_levels(&mut self, n: usize, intra: bool, start: i32, scan: &[u8; 64]) -> Result<()> {
        let t = self.t;
        let rl = if intra { &t.rl_intra } else { &t.rl_inter };
        let mut pos = start;
        loop {
            let sym = rl.vlc.read(&mut self.br).ok_or_else(|| self.vlc_err())?;
            // a symbol's level is followed by its sign bit; escape 3 carries a signed level instead
            let (run, level, last);
            if sym == rl.escape {
                if !self.br.bit() {
                    // escape 1: the level is raised by the largest level the table has for the run
                    let s = rl.vlc.read(&mut self.br).ok_or_else(|| self.vlc_err())?;
                    let l = (s >= rl.last_from) as usize;
                    run = rl.run[s] as i32;
                    let v = rl.level[s] as i32 + rl.max_level[l][run as usize] as i32;
                    level = if self.br.bit() { -v } else { v };
                    last = l == 1;
                } else if !self.br.bit() {
                    // escape 2: the run is raised by the largest run the table has for the level
                    let s = rl.vlc.read(&mut self.br).ok_or_else(|| self.vlc_err())?;
                    let l = (s >= rl.last_from) as usize;
                    let lv = rl.level[s] as usize;
                    run = rl.run[s] as i32 + rl.max_run[l][lv] as i32 + 1;
                    level = if self.br.bit() { -(lv as i32) } else { lv as i32 };
                    last = l == 1;
                } else {
                    // escape 3: the last flag, the run and the level are written out
                    last = self.br.bit();
                    run = self.br.read(6) as i32;
                    self.br.skip(1);
                    level = (self.br.read(12) as i32) << 20 >> 20;
                    self.br.skip(1);
                }
            } else {
                run = rl.run[sym] as i32;
                let v = rl.level[sym] as i32;
                level = if self.br.bit() { -v } else { v };
                last = sym >= rl.last_from;
            }
            pos += run + 1;
            if !(0..64).contains(&pos) {
                return Err(bad("coefficient out of range"));
            }
            self.coef[n][scan[pos as usize] as usize] = level;
            if last {
                break;
            }
        }
        Ok(())
    }

    /// The DC prediction of block `n` (full scale), and whether it comes from the block above.
    fn pred_dc(&self, mbx: usize, mby: usize, n: usize) -> (i32, bool) {
        let (g, bx, by) = block_pos(mbx, mby, n);
        let (bx, by) = (bx as i32, by as i32);
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

    /// The quantised DC level of a block (the coded level plus the prediction, in steps of the DC
    /// scale), with its full-scale value stored for the prediction of the blocks after it.
    fn level_dc(&mut self, mbx: usize, mby: usize, n: usize, pred: i32, level: i32) -> i32 {
        let (g, bx, by) = block_pos(mbx, mby, n);
        let scale = self.dc_scale(n);
        let q = level + (pred + (scale >> 1)) / scale;
        let mut full = q * scale;
        if full & !2047 != 0 {
            full = if full < 0 { 0 } else { 2047 };
        }
        let i = self.idx(g, bx as i32, by as i32);
        self.dc[g][i] = full;
        q
    }

    /// Adds the AC prediction of an intra block (from the left, or the block above when the DC is
    /// predicted from it), then stores this block's first row and column for the blocks after it.
    fn predict_ac(&mut self, mbx: usize, mby: usize, n: usize, top: bool, ac_pred: bool) {
        let (g, bx, by) = block_pos(mbx, mby, n);
        let (bx, by) = (bx as i32, by as i32);
        let q = self.qscale;
        if ac_pred {
            if !top && self.avail(g, bx - 1, by) {
                let nb = self.ac[g][self.idx(g, bx - 1, by)];
                let nq = self.mb_qs[self.mb_of(g, bx - 1, by)];
                for i in 1..8 {
                    self.coef[n][i * 8] += scale_ac(nb[i], nq, q);
                }
            } else if top && self.avail(g, bx, by - 1) {
                let nb = self.ac[g][self.idx(g, bx, by - 1)];
                let nq = self.mb_qs[self.mb_of(g, bx, by - 1)];
                for i in 1..8 {
                    self.coef[n][i] += scale_ac(nb[8 + i], nq, q);
                }
            }
        }
        let mut slots = [0i32; 16];
        for i in 1..8 {
            slots[i] = self.coef[n][i * 8];
            slots[8 + i] = self.coef[n][i];
        }
        let i = self.idx(g, bx, by);
        self.ac[g][i] = slots;
    }

    /// Dequantises block `n` in place, with the DC scaled, and the mismatch control of the MPEG
    /// quantiser. Inter blocks have no separate DC: their first coefficient is an AC level.
    fn dequant(&mut self, n: usize, intra: bool) {
        let q = self.qscale;
        let dcs = self.dc_scale(n);
        let first = if intra { 1 } else { 0 };
        let mpeg = self.vol.mpeg_quant;
        let mat = if intra { self.vol.intra_matrix } else { self.vol.inter_matrix };
        let blk = &mut self.coef[n];
        if intra {
            blk[0] *= dcs;
        }
        if !mpeg {
            let qadd = (q - 1) | 1;
            for c in blk.iter_mut().skip(first) {
                if *c != 0 {
                    let m = c.abs() * 2 * q + qadd;
                    *c = (if *c < 0 { -m } else { m }).clamp(-2048, 2047);
                }
            }
            return;
        }
        let mut sum: i32 = -1 + if intra { blk[0] } else { 0 };
        for i in first..64 {
            let c = blk[i];
            if c != 0 {
                let w = mat[i] as i32;
                let a = c.abs();
                let m = if intra { (a * 2 * q * w) >> 4 } else { (((a << 1) + 1) * 2 * q * w) >> 5 };
                let v = if c < 0 { -m } else { m };
                blk[i] = v;
                sum += v;
            }
        }
        blk[63] ^= sum & 1;
    }

    /// The block's destination: the plane, the offset of its top left sample, and the stride.
    fn block_dst(&self, mbx: usize, mby: usize, n: usize) -> (usize, usize, usize) {
        let (lw, cw) = (self.vol.mbw * 16, self.vol.mbw * 8);
        if n < 4 {
            (0, (mby * 16 + (n >> 1) * 8) * lw + mbx * 16 + (n & 1) * 8, lw)
        } else {
            (n - 3, mby * 8 * cw + mbx * 8, cw)
        }
    }

    /// Writes the inverse transform of intra block `n` (no prediction) to the picture.
    fn put_block(&mut self, mbx: usize, mby: usize, n: usize) {
        let mut blk = [0i16; 64];
        for i in 0..64 {
            blk[i] = self.coef[n][i] as i16;
        }
        let (plane, off, stride) = self.block_dst(mbx, mby, n);
        idct::put(&mut blk, &mut self.cur.planes[plane][off..], stride);
    }

    /// Motion compensation of a macroblock from its references into the picture. The second
    /// direction of a B-VOP is averaged into the first, and always rounds.
    fn mc_mb(&mut self, mbx: usize, mby: usize, plan: &Plan) {
        let quarter = self.vol.quarter;
        let (lw, lh) = (self.vol.mbw * 16, self.vol.mbh * 16);
        let (cw, ch) = (self.vol.mbw * 8, self.vol.mbh * 8);
        let (w, h) = (self.vol.width as i32, self.vol.height as i32);
        let (mx0, my0) = (mbx as i32 * 16, mby as i32 * 16);
        for d in 0..2 {
            if (d == 0 && !plan.fwd) || (d == 1 && !plan.bwd) {
                continue;
            }
            let refp = if d == 0 { self.fwd_ref.clone() } else { self.bwd_ref.clone() };
            let Some(refp) = refp else { continue };
            let avg = d == 1 && plan.fwd;
            let nr = self.nr && !avg;
            let lp = Plane { d: &refp.planes[0], stride: lw, w: lw as i32, h: lh as i32 };
            let mv = plan.mv[d];
            if plan.four {
                for i in 0..4 {
                    let (mvx, mvy) = (mv[i][0], mv[i][1]);
                    let (bx, by) = (mx0 + (i as i32 & 1) * 8, my0 + (i as i32 >> 1) * 8);
                    let dst = &mut self.cur.planes[0][by as usize * lw + bx as usize..];
                    if quarter {
                        let mut dxy = (((mvy & 3) << 2) | (mvx & 3)) as u32;
                        let sx = (bx + (mvx >> 2)).clamp(-16, w);
                        if sx == w {
                            dxy &= !3;
                        }
                        let sy = (by + (mvy >> 2)).clamp(-16, h);
                        if sy == h {
                            dxy &= !12;
                        }
                        mc::qpel(dst, lw, 8, &lp, sx, sy, dxy, nr, avg);
                    } else {
                        let mut dxy = 0u32;
                        let sx = (bx + (mvx >> 1)).clamp(-16, w);
                        if sx != w {
                            dxy |= (mvx & 1) as u32;
                        }
                        let sy = (by + (mvy >> 1)).clamp(-16, h);
                        if sy != h {
                            dxy |= ((mvy & 1) << 1) as u32;
                        }
                        mc::hpel(dst, lw, 8, 8, &lp, sx, sy, dxy, nr, avg);
                    }
                }
                // the chroma vector of a four-vector macroblock: the rounded sum of the four
                let (mut sx, mut sy) = (0i32, 0i32);
                for m in mv {
                    let (a, b) = if quarter { (m[0] / 2, m[1] / 2) } else { (m[0], m[1]) };
                    sx += a;
                    sy += b;
                }
                let (mx, my) = (round_chroma(sx), round_chroma(sy));
                let mut dxy = (((my & 1) << 1) | (mx & 1)) as u32;
                let (cx, cy) = (mx >> 1, my >> 1);
                let px = (mbx as i32 * 8 + cx).clamp(-8, w >> 1);
                if px == w >> 1 {
                    dxy &= !1;
                }
                let py = (mby as i32 * 8 + cy).clamp(-8, h >> 1);
                if py == h >> 1 {
                    dxy &= !2;
                }
                for p in 1..3 {
                    let cp = Plane { d: &refp.planes[p], stride: cw, w: cw as i32, h: ch as i32 };
                    let dst = &mut self.cur.planes[p][mby * 8 * cw + mbx * 8..];
                    mc::hpel(dst, cw, 8, 8, &cp, px, py, dxy, nr, avg);
                }
            } else if quarter {
                let (mvx, mvy) = (mv[0][0], mv[0][1]);
                let (sx, sy) = (mx0 + (mvx >> 2), my0 + (mvy >> 2));
                let dxy = (((mvy & 3) << 2) | (mvx & 3)) as u32;
                let dst = &mut self.cur.planes[0][my0 as usize * lw + mx0 as usize..];
                mc::qpel(dst, lw, 16, &lp, sx, sy, dxy, nr, avg);
                let cx = mvx / 2;
                let cy = mvy / 2;
                let cx = (cx >> 1) | (cx & 1);
                let cy = (cy >> 1) | (cy & 1);
                let uvdxy = ((cx & 1) | ((cy & 1) << 1)) as u32;
                for p in 1..3 {
                    let cp = Plane { d: &refp.planes[p], stride: cw, w: cw as i32, h: ch as i32 };
                    let dst = &mut self.cur.planes[p][mby * 8 * cw + mbx * 8..];
                    mc::hpel(dst, cw, 8, 8, &cp, mbx as i32 * 8 + (cx >> 1), mby as i32 * 8 + (cy >> 1), uvdxy, nr, avg);
                }
            } else {
                let (mvx, mvy) = (mv[0][0], mv[0][1]);
                let (sx, sy) = (mx0 + (mvx >> 1), my0 + (mvy >> 1));
                let dxy = (((mvy & 1) << 1) | (mvx & 1)) as u32;
                let dst = &mut self.cur.planes[0][my0 as usize * lw + mx0 as usize..];
                mc::hpel(dst, lw, 16, 16, &lp, sx, sy, dxy, nr, avg);
                let uvdxy = dxy | (mvy & 2) as u32 | ((mvx & 2) >> 1) as u32;
                for p in 1..3 {
                    let cp = Plane { d: &refp.planes[p], stride: cw, w: cw as i32, h: ch as i32 };
                    let dst = &mut self.cur.planes[p][mby * 8 * cw + mbx * 8..];
                    mc::hpel(dst, cw, 8, 8, &cp, sx >> 1, sy >> 1, uvdxy, nr, avg);
                }
            }
        }
    }
}

fn round_chroma(x: i32) -> i32 {
    CHROMA_ROUND[(x & 15) as usize] + (x >> 3)
}
