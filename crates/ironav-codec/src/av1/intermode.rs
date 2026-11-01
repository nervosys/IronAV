//! Inter mode info (spec 5.11.18 to 5.11.34 with libaom's contexts): skip mode, reference
//! frames, inter modes, motion vectors, inter-intra, motion modes, compound types, interpolation
//! filters and the variable transform sizes of inter blocks.

use super::block::*;
use super::mvpred::*;
use super::obu::{rel_dist, GM_TRANSLATION, TX_MODE_SELECT};
use super::tables::*;

const LAST2_FRAME: i8 = 2;
const LAST3_FRAME: i8 = 3;
const ALTREF2_FRAME: i8 = 6;
pub(crate) const COMPOUND_MODE_CTX_MAP: [[usize; 5]; 3] = [[0, 1, 1, 1, 1], [1, 2, 3, 4, 4], [4, 4, 5, 6, 7]];

/// Whether wedges exist for a block size (Wedge_Bits is not 0).
pub(crate) fn wedge_used(bs: usize) -> bool {
    matches!(bs, 3..=9 | 18 | 19)
}

fn count_ctx(a: u32, b: u32) -> usize {
    (a >= b) as usize + (a > b) as usize
}

fn backward(rf: i8) -> bool {
    rf >= BWDREF_FRAME
}

fn uni_comp(m: &Mi) -> bool {
    m.ref_frame[1] > 0 && backward(m.ref_frame[0]) == backward(m.ref_frame[1])
}

impl FrameDec {
    fn above(&self) -> Option<&Mi> {
        self.b.avail_u.then(|| self.mi_at(self.b.row - 1, self.b.col))
    }

    fn left(&self) -> Option<&Mi> {
        self.b.avail_l.then(|| self.mi_at(self.b.row, self.b.col - 1))
    }

    pub(crate) fn read_skip_mode(&mut self, t: &mut Tile) {
        let (seg, bs) = (self.b.seg, self.b.size);
        if self.seg_feature(seg, SEG_LVL_SKIP) || self.seg_feature(seg, SEG_LVL_REF_FRAME) || self.seg_feature(seg, SEG_LVL_GLOBALMV) || !self.h.skip_mode_present || MI_W_LOG2[bs] == 0 || MI_H_LOG2[bs] == 0 {
            return;
        }
        let ctx = self.above().map_or(0, |m| m.skip_mode as usize) + self.left().map_or(0, |m| m.skip_mode as usize);
        self.b.skip_mode = t.sd.symbol(&mut t.cdf.skip_mode[ctx]) == 1;
    }

    pub(crate) fn read_is_inter(&mut self, t: &mut Tile) {
        let seg = self.b.seg;
        self.b.is_inter = if self.b.skip_mode {
            true
        } else if self.seg_feature(seg, SEG_LVL_REF_FRAME) {
            self.h.feature_data[seg][SEG_LVL_REF_FRAME] != 0
        } else if self.seg_feature(seg, SEG_LVL_GLOBALMV) {
            true
        } else {
            let a = self.above().map(|m| m.ref_frame[0] <= 0);
            let l = self.left().map(|m| m.ref_frame[0] <= 0);
            let ctx = match (a, l) {
                (Some(a), Some(l)) => {
                    if a && l {
                        3
                    } else {
                        (a || l) as usize
                    }
                }
                (Some(x), None) | (None, Some(x)) => 2 * x as usize,
                _ => 0,
            };
            t.sd.symbol(&mut t.cdf.intra_inter[ctx]) == 1
        };
    }

    /// The reference frames of the above and left blocks, counted.
    fn count_refs(&self) -> [u32; 8] {
        let mut n = [0; 8];
        for m in [self.above(), self.left()].into_iter().flatten() {
            for rf in m.ref_frame {
                if rf > 0 {
                    n[rf as usize] += 1;
                }
            }
        }
        n
    }

    fn comp_mode_ctx(&self) -> usize {
        let single = |m: &Mi| m.ref_frame[1] <= 0;
        match (self.above(), self.left()) {
            (Some(a), Some(l)) => {
                if single(a) && single(l) {
                    (backward(a.ref_frame[0]) ^ backward(l.ref_frame[0])) as usize
                } else if single(a) {
                    2 + (backward(a.ref_frame[0]) || a.ref_frame[0] <= 0) as usize
                } else if single(l) {
                    2 + (backward(l.ref_frame[0]) || l.ref_frame[0] <= 0) as usize
                } else {
                    4
                }
            }
            (Some(e), None) | (None, Some(e)) => {
                if single(e) {
                    backward(e.ref_frame[0]) as usize
                } else {
                    3
                }
            }
            _ => 1,
        }
    }

    fn comp_ref_type_ctx(&self) -> usize {
        let intra = |m: &Mi| m.ref_frame[0] <= 0;
        let single = |m: &Mi| m.ref_frame[1] <= 0;
        match (self.above(), self.left()) {
            (Some(a), Some(l)) => {
                if intra(a) && intra(l) {
                    2
                } else if intra(a) || intra(l) {
                    let e = if intra(a) { l } else { a };
                    if single(e) {
                        2
                    } else {
                        1 + 2 * uni_comp(e) as usize
                    }
                } else {
                    let same = (backward(a.ref_frame[0]) == backward(l.ref_frame[0])) as usize;
                    match (single(a), single(l)) {
                        (true, true) => 1 + 2 * same,
                        (true, false) | (false, true) => {
                            if !uni_comp(if single(a) { l } else { a }) {
                                1
                            } else {
                                3 + same
                            }
                        }
                        _ => match (uni_comp(a), uni_comp(l)) {
                            (false, false) => 0,
                            (true, true) => 3 + ((a.ref_frame[0] == BWDREF_FRAME) == (l.ref_frame[0] == BWDREF_FRAME)) as usize,
                            _ => 2,
                        },
                    }
                }
            }
            (Some(e), None) | (None, Some(e)) => {
                if intra(e) || single(e) {
                    2
                } else {
                    4 * uni_comp(e) as usize
                }
            }
            _ => 2,
        }
    }

    fn read_ref_frames(&mut self, t: &mut Tile) {
        let seg = self.b.seg;
        if self.b.skip_mode {
            self.b.ref_frame = self.h.skip_mode_frame;
            return;
        }
        if self.seg_feature(seg, SEG_LVL_REF_FRAME) {
            self.b.ref_frame = [self.h.feature_data[seg][SEG_LVL_REF_FRAME] as i8, -1];
            return;
        }
        if self.seg_feature(seg, SEG_LVL_SKIP) || self.seg_feature(seg, SEG_LVL_GLOBALMV) {
            self.b.ref_frame = [LAST_FRAME, -1];
            return;
        }
        let n = self.count_refs();
        let fwd_bwd = count_ctx(n[1] + n[2] + n[3] + n[4], n[5] + n[6] + n[7]);
        let compound = self.h.reference_select && self.b.bw4.min(self.b.bh4) >= 2 && {
            let ctx = self.comp_mode_ctx();
            t.sd.symbol(&mut t.cdf.comp_mode[ctx]) == 1
        };
        let mut s = |cdf: &mut [u16]| t.sd.symbol(cdf) == 1;
        let c = &mut t.cdf;
        self.b.ref_frame = if compound {
            let ctx = self.comp_ref_type_ctx();
            if !s(&mut c.comp_ref_type[ctx]) {
                if s(&mut c.uni_comp_ref[fwd_bwd][0]) {
                    [BWDREF_FRAME, ALTREF_FRAME]
                } else if s(&mut c.uni_comp_ref[count_ctx(n[2], n[3] + n[4])][1]) {
                    if s(&mut c.uni_comp_ref[count_ctx(n[3], n[4])][2]) {
                        [LAST_FRAME, GOLDEN_FRAME]
                    } else {
                        [LAST_FRAME, LAST3_FRAME]
                    }
                } else {
                    [LAST_FRAME, LAST2_FRAME]
                }
            } else {
                let r0 = if !s(&mut c.comp_ref[count_ctx(n[1] + n[2], n[3] + n[4])][0]) {
                    if s(&mut c.comp_ref[count_ctx(n[1], n[2])][1]) {
                        LAST2_FRAME
                    } else {
                        LAST_FRAME
                    }
                } else if s(&mut c.comp_ref[count_ctx(n[3], n[4])][2]) {
                    GOLDEN_FRAME
                } else {
                    LAST3_FRAME
                };
                let r1 = if !s(&mut c.comp_bwdref[count_ctx(n[5] + n[6], n[7])][0]) {
                    if s(&mut c.comp_bwdref[count_ctx(n[5], n[6])][1]) {
                        ALTREF2_FRAME
                    } else {
                        BWDREF_FRAME
                    }
                } else {
                    ALTREF_FRAME
                };
                [r0, r1]
            }
        } else {
            let r = if s(&mut c.single_ref[fwd_bwd][0]) {
                if !s(&mut c.single_ref[count_ctx(n[5] + n[6], n[7])][1]) {
                    if s(&mut c.single_ref[count_ctx(n[5], n[6])][5]) {
                        ALTREF2_FRAME
                    } else {
                        BWDREF_FRAME
                    }
                } else {
                    ALTREF_FRAME
                }
            } else if s(&mut c.single_ref[count_ctx(n[1] + n[2], n[3] + n[4])][2]) {
                if s(&mut c.single_ref[count_ctx(n[3], n[4])][4]) {
                    GOLDEN_FRAME
                } else {
                    LAST3_FRAME
                }
            } else if s(&mut c.single_ref[count_ctx(n[1], n[2])][3]) {
                LAST2_FRAME
            } else {
                LAST_FRAME
            };
            [r, -1]
        };
    }

    pub(crate) fn inter_block_mode_info(&mut self, t: &mut Tile) {
        self.read_ref_frames(t);
        let compound = self.b.ref_frame[1] > 0;
        let st = self.find_mv_stack();
        let (seg, bs) = (self.b.seg, self.b.size);
        let c = &mut t.cdf;
        let sd = &mut t.sd;
        let mode = if self.b.skip_mode {
            NEAREST_NEARESTMV
        } else if self.seg_feature(seg, SEG_LVL_SKIP) || self.seg_feature(seg, SEG_LVL_GLOBALMV) {
            GLOBALMV
        } else if compound {
            let ctx = COMPOUND_MODE_CTX_MAP[st.ref_ctx >> 1][st.new_ctx.min(4)];
            NEAREST_NEARESTMV + sd.symbol(&mut c.compound_mode[ctx]) as u8
        } else if sd.symbol(&mut c.newmv[st.new_ctx]) == 0 {
            NEWMV
        } else if sd.symbol(&mut c.zeromv[st.zero_ctx]) == 0 {
            GLOBALMV
        } else if sd.symbol(&mut c.refmv[st.ref_ctx]) == 0 {
            NEARESTMV
        } else {
            NEARMV
        };
        self.b.y_mode = mode;
        let mut idx = 0;
        let start = if mode == NEWMV || mode == NEW_NEWMV {
            Some(0)
        } else if matches!(mode, NEARMV | NEAR_NEARMV | NEAR_NEWMV | NEW_NEARMV) {
            Some(1)
        } else {
            None
        };
        if let Some(s) = start {
            idx = s;
            for i in s..s + 2 {
                if st.num > i + 1 {
                    if sd.symbol(&mut c.drl[st.drl_ctx[i]]) == 0 {
                        idx = i;
                        break;
                    }
                    idx = i + 1;
                }
            }
        }
        // assign_mv
        for l in 0..1 + compound as usize {
            let cm = list_mode(mode, l);
            let pred = if cm == GLOBALMV {
                st.global[l]
            } else {
                let pos = if cm == NEARESTMV || (cm == NEWMV && st.num <= 1) { 0 } else { idx };
                st.mv[pos][l]
            };
            self.b.mv[l] = if cm == NEWMV { self.read_mv(t, pred, 0) } else { pred };
        }
        let c = &mut t.cdf;
        let sd = &mut t.sd;
        // read_interintra_mode
        if !self.b.skip_mode && self.seq.enable_interintra_compound && !compound && (BLOCK_8X8..=9).contains(&bs) {
            let g = SIZE_GROUP[bs] as usize;
            if sd.symbol(&mut c.interintra[g]) == 1 {
                self.b.interintra = true;
                self.b.ii_mode = sd.symbol(&mut c.interintra_mode[g]) as u8;
                self.b.ref_frame[1] = 0;
                self.b.wedge_ii = sd.symbol(&mut c.wedge_interintra[bs]) == 1;
                if self.b.wedge_ii {
                    self.b.wedge_index = sd.symbol(&mut c.wedge_idx[bs]);
                }
            }
        }
        // read_motion_mode
        let rf0 = self.b.ref_frame[0];
        let gm = self.h.gm_type;
        let global = !self.h.force_integer_mv && (mode == GLOBALMV || mode == GLOBAL_GLOBALMV) && gm[rf0 as usize] > GM_TRANSLATION;
        if !self.b.skip_mode && self.h.is_motion_mode_switchable && self.b.bw4.min(self.b.bh4) >= 2 && !global && !compound && self.b.ref_frame[1] != 0 && self.has_overlappable_candidates() {
            let ws = self.find_warp_samples();
            self.b.motion_mode = if self.h.force_integer_mv || ws.num == 0 || !self.h.allow_warped_motion || self.is_scaled(rf0) {
                if t.sd.symbol(&mut t.cdf.obmc[bs]) == 1 {
                    MOTION_OBMC
                } else {
                    0
                }
            } else {
                t.sd.symbol(&mut t.cdf.motion_mode[bs]) as u8
            };
            if self.b.motion_mode == MOTION_LOCALWARP {
                let p = super::inter::warp_estimation(&ws, self.b.mv[0], self.b.size, self.b.row, self.b.col);
                self.b.local_valid = p.is_some_and(|p| super::inter::setup_shear(&p).is_some());
                self.b.local_warp = p.unwrap_or_default();
            }
        }
        // read_compound_type
        let c = &mut t.cdf;
        let sd = &mut t.sd;
        if self.b.skip_mode {
            self.b.compound_type = COMPOUND_AVERAGE;
        } else if compound {
            if self.seq.enable_masked_compound {
                let ctx = self.comp_group_ctx();
                self.b.comp_group_idx = sd.symbol(&mut c.comp_group_idx[ctx]) as u8;
            }
            self.b.compound_type = if self.b.comp_group_idx == 0 {
                if self.seq.enable_jnt_comp {
                    let ctx = self.compound_idx_ctx();
                    self.b.compound_idx = sd.symbol(&mut c.compound_idx[ctx]) as u8;
                    if self.b.compound_idx == 1 {
                        COMPOUND_AVERAGE
                    } else {
                        COMPOUND_DISTANCE
                    }
                } else {
                    COMPOUND_AVERAGE
                }
            } else if wedge_used(bs) {
                sd.symbol(&mut c.compound_type[bs]) as u8
            } else {
                COMPOUND_DIFFWTD
            };
            if self.b.compound_type == COMPOUND_WEDGE {
                self.b.wedge_index = sd.symbol(&mut c.wedge_idx[bs]);
                self.b.wedge_sign = sd.literal(1) as usize;
            } else if self.b.compound_type == COMPOUND_DIFFWTD {
                self.b.mask_type = sd.literal(1) as usize;
            }
        } else if self.b.interintra {
            self.b.compound_type = if self.b.wedge_ii { COMPOUND_WEDGE } else { COMPOUND_INTRA };
        } else {
            self.b.compound_type = COMPOUND_AVERAGE;
        }
        // interpolation filters
        if self.h.interp_filter == 4 {
            let large = self.b.bw4.min(self.b.bh4) >= 2;
            let need = if self.b.skip_mode || self.b.motion_mode == MOTION_LOCALWARP {
                false
            } else if large && mode == GLOBALMV {
                gm[rf0 as usize] == GM_TRANSLATION
            } else if large && mode == GLOBAL_GLOBALMV {
                gm[rf0 as usize] == GM_TRANSLATION || gm[self.b.ref_frame[1] as usize] == GM_TRANSLATION
            } else {
                true
            };
            let dual = self.seq.enable_dual_filter;
            for dir in 0..1 + dual as usize {
                self.b.interp[dir] = if need {
                    let ctx = self.interp_ctx(dir);
                    t.sd.symbol(&mut t.cdf.interp_filter[ctx]) as u8
                } else {
                    0
                };
            }
            if !dual {
                self.b.interp[1] = self.b.interp[0];
            }
        } else {
            self.b.interp = [self.h.interp_filter; 2];
        }
    }

    fn interp_ctx(&self, dir: usize) -> usize {
        let rf = self.b.ref_frame[0];
        let ty = |m: Option<&Mi>| match m {
            Some(m) if m.ref_frame[0] == rf || m.ref_frame[1] == rf => m.interp[dir] as usize,
            _ => 3,
        };
        let (l, a) = (ty(self.left()), ty(self.above()));
        ((dir & 1) * 2 + (self.b.ref_frame[1] > 0) as usize) * 4
            + if l == a {
                l
            } else if l == 3 {
                a
            } else if a == 3 {
                l
            } else {
                3
            }
    }

    fn comp_group_ctx(&self) -> usize {
        let f = |m: Option<&Mi>| match m {
            Some(m) if m.ref_frame[1] > 0 => m.comp_group_idx as usize,
            Some(m) if m.ref_frame[0] == ALTREF_FRAME => 3,
            _ => 0,
        };
        (f(self.above()) + f(self.left())).min(5)
    }

    fn compound_idx_ctx(&self) -> usize {
        let h = &self.h;
        let fwd = rel_dist(&self.seq, h.order_hints[self.b.ref_frame[0] as usize], h.order_hint).abs();
        let bck = rel_dist(&self.seq, h.order_hints[self.b.ref_frame[1] as usize], h.order_hint).abs();
        let f = |m: Option<&Mi>| match m {
            Some(m) if m.ref_frame[1] > 0 => m.compound_idx as usize,
            Some(m) if m.ref_frame[0] == ALTREF_FRAME => 1,
            _ => 0,
        };
        3 * (fwd == bck) as usize + f(self.above()) + f(self.left())
    }

    /// Whether a reference frame has a different size than the frame (is_scaled): the reference's
    /// upscaled width against this frame's FrameWidth, which superres makes smaller than it.
    pub(crate) fn is_scaled(&self, rf: i8) -> bool {
        self.refs[rf as usize].as_ref().is_some_and(|p| p.width != self.h.width as usize || p.height != self.h.height as usize)
    }

    /// read_mv: the difference from `pred`, with the MV CDFs of MvCtx `ctx` (1 for intra block copy).
    pub(crate) fn read_mv(&mut self, t: &mut Tile, pred: Mv, ctx: usize) -> Mv {
        let joint = t.sd.symbol(&mut t.cdf.mv[ctx].joint);
        let mut d = [0; 2];
        if joint == 2 || joint == 3 {
            d[0] = self.read_mv_component(t, ctx, 0);
        }
        if joint == 1 || joint == 3 {
            d[1] = self.read_mv_component(t, ctx, 1);
        }
        [pred[0] + d[0], pred[1] + d[1]]
    }

    fn read_mv_component(&self, t: &mut Tile, ctx: usize, comp: usize) -> i32 {
        let (hp, int) = (self.h.allow_high_precision_mv, self.h.force_integer_mv);
        let c = &mut t.cdf.mv[ctx].comp[comp];
        let sd = &mut t.sd;
        let sign = sd.symbol(&mut c.sign) == 1;
        let class = sd.symbol(&mut c.class);
        let mag = if class == 0 {
            let b0 = sd.symbol(&mut c.class0);
            let fr = if int { 3 } else { sd.symbol(&mut c.class0_fr[b0]) };
            let h = if hp { sd.symbol(&mut c.class0_hp) } else { 1 };
            ((b0 << 3) | (fr << 1) | h) + 1
        } else {
            let mut d = 0;
            for i in 0..class {
                d |= sd.symbol(&mut c.bits[i]) << i;
            }
            let fr = if int { 3 } else { sd.symbol(&mut c.fr) };
            let h = if hp { sd.symbol(&mut c.hp) } else { 1 };
            (2 << (class + 2)) + ((d << 3) | (fr << 1) | h) + 1
        } as i32;
        if sign {
            -mag
        } else {
            mag
        }
    }

    /// read_block_tx_size.
    pub(crate) fn read_block_tx_size(&mut self, t: &mut Tile) {
        let bs = self.b.size;
        if self.h.tx_mode == TX_MODE_SELECT && bs > BLOCK_4X4 && self.b.is_inter && !self.b.skip && !self.b.lossless {
            self.b.var_tx = true;
            let max = MAX_TX_SIZE_RECT[bs];
            let (w4, h4) = (1usize << (TX_W_LOG2[max] - 2), 1usize << (TX_H_LOG2[max] - 2));
            let (r, c) = (self.b.row, self.b.col);
            for row in (r..r + self.b.bh4).step_by(h4) {
                for col in (c..c + self.b.bw4).step_by(w4) {
                    self.read_var_tx_size(t, row, col, max, 0);
                }
            }
        } else {
            self.read_tx_size(t, !self.b.skip || !self.b.is_inter);
        }
    }

    fn above_tx_w(&self, row: usize, col: usize) -> usize {
        if row == self.b.row {
            if !self.b.avail_u {
                return 64;
            }
            let m = self.mi_at(row - 1, col);
            if m.skip && m.is_inter {
                return 4 << MI_W_LOG2[m.size as usize];
            }
        }
        1 << TX_W_LOG2[self.mi_at(row - 1, col).tx as usize]
    }

    fn left_tx_h(&self, row: usize, col: usize) -> usize {
        if col == self.b.col {
            if !self.b.avail_l {
                return 64;
            }
            let m = self.mi_at(row, col - 1);
            if m.skip && m.is_inter {
                return 4 << MI_H_LOG2[m.size as usize];
            }
        }
        1 << TX_H_LOG2[self.mi_at(row, col - 1).tx as usize]
    }

    fn read_var_tx_size(&mut self, t: &mut Tile, row: usize, col: usize, tx: usize, depth: usize) {
        if row >= self.h.mi_rows || col >= self.h.mi_cols {
            return;
        }
        let split = tx != TX_4X4
            && depth != 2
            && {
                let above = (self.above_tx_w(row, col) < 1 << TX_W_LOG2[tx]) as usize;
                let left = (self.left_tx_h(row, col) < 1 << TX_H_LOG2[tx]) as usize;
                let bs = self.b.size;
                let size = (4usize << MI_W_LOG2[bs].max(MI_H_LOG2[bs])).min(64);
                let max = size.trailing_zeros() as usize - 2;
                let ctx = (TX_SIZE_SQR_UP[tx] != max) as usize * 3 + (4 - max) * 6 + above + left;
                t.sd.symbol(&mut t.cdf.txfm_partition[ctx]) == 1
            };
        let (w4, h4) = (1usize << (TX_W_LOG2[tx] - 2), 1usize << (TX_H_LOG2[tx] - 2));
        if split {
            let sub = SPLIT_TX_SIZE[tx];
            let (sw, sh) = (1usize << (TX_W_LOG2[sub] - 2), 1usize << (TX_H_LOG2[sub] - 2));
            for i in (0..h4).step_by(sh) {
                for j in (0..w4).step_by(sw) {
                    self.read_var_tx_size(t, row + i, col + j, sub, depth + 1);
                }
            }
        } else {
            for y in row..(row + h4).min(self.h.mi_rows) {
                for x in col..(col + w4).min(self.h.mi_cols) {
                    let i = self.mi_idx(y, x);
                    self.mi[i].tx = tx as u8;
                }
            }
            self.b.tx = tx;
        }
    }
}

/// get_mode: the single mode of one list of a (compound) mode.
fn list_mode(mode: u8, l: usize) -> u8 {
    if l == 0 {
        match mode {
            m if m < NEAREST_NEARESTMV => m,
            NEW_NEWMV | NEW_NEARESTMV | NEW_NEARMV => NEWMV,
            NEAREST_NEARESTMV | NEAREST_NEWMV => NEARESTMV,
            NEAR_NEARMV | NEAR_NEWMV => NEARMV,
            _ => GLOBALMV,
        }
    } else {
        match mode {
            NEW_NEWMV | NEAREST_NEWMV | NEAR_NEWMV => NEWMV,
            NEAREST_NEARESTMV | NEW_NEARESTMV => NEARESTMV,
            NEAR_NEARMV | NEW_NEARMV => NEARMV,
            _ => GLOBALMV,
        }
    }
}
