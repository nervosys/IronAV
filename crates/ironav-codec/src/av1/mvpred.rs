//! Motion vector prediction (spec 7.10.2): the reference MV stack of a block from its spatial and
//! temporal neighbours, the warp samples (7.10.4), and the motion field estimation (7.9) that the
//! temporal candidates come from.

use super::block::{FrameDec, Mi};
use super::inter_tables::DIV_MULT;
use super::obu::{rel_dist, GM_TRANSLATION, INTRA_ONLY_FRAME, KEY_FRAME};
use super::tables::*;

pub const NEARESTMV: u8 = 13;
pub const NEARMV: u8 = 14;
pub const GLOBALMV: u8 = 15;
pub const NEWMV: u8 = 16;
pub const NEAREST_NEARESTMV: u8 = 17;
pub const NEAR_NEARMV: u8 = 18;
pub const NEAREST_NEWMV: u8 = 19;
pub const NEW_NEARESTMV: u8 = 20;
pub const NEAR_NEWMV: u8 = 21;
pub const NEW_NEARMV: u8 = 22;
pub const GLOBAL_GLOBALMV: u8 = 23;
pub const NEW_NEWMV: u8 = 24;

pub const LAST_FRAME: i8 = 1;
pub const GOLDEN_FRAME: i8 = 4;
pub const BWDREF_FRAME: i8 = 5;
pub const ALTREF_FRAME: i8 = 7;

const REF_CAT_LEVEL: u32 = 640;
const MV_BORDER: i32 = 128;
const MAX_FRAME_DISTANCE: i32 = 31;
pub const INVALID_MV: i32 = i32::MIN;

/// A motion vector: row, column, in eighth samples.
pub type Mv = [i32; 2];

pub fn has_newmv(mode: u8) -> bool {
    matches!(mode, NEWMV | NEW_NEWMV | NEAR_NEWMV | NEW_NEARMV | NEAREST_NEWMV | NEW_NEARESTMV)
}

/// What a frame keeps of a block's motion for later frames' motion fields (MfRefFrames, MfMvs),
/// one per 8x8.
#[derive(Clone, Copy)]
pub struct SavedMv {
    pub rf: i8,
    pub mv: Mv,
}

/// A projected motion field entry: the motion vector and the distance it spans.
#[derive(Clone, Copy)]
pub struct Tpl {
    pub mv: Mv,
    pub offset: i32,
}

/// The reference MV stack of a block and the contexts derived from it.
#[derive(Default)]
pub struct MvStack {
    pub num: usize,
    pub mv: [[Mv; 2]; 9],
    pub weight: [u32; 9],
    pub global: [Mv; 2],
    pub new_ctx: usize,
    pub ref_ctx: usize,
    pub zero_ctx: usize,
    pub drl_ctx: [usize; 9],
    new_count: usize,
    found: bool,
}

fn round2signed(x: i64, n: u32) -> i64 {
    if x >= 0 {
        (x + (1 << (n - 1))) >> n
    } else {
        -((-x + (1 << (n - 1))) >> n)
    }
}

/// get_mv_projection.
pub fn project(mv: Mv, num: i32, den: i32) -> Mv {
    let den = den.min(MAX_FRAME_DISTANCE);
    let num = num.clamp(-MAX_FRAME_DISTANCE, MAX_FRAME_DISTANCE);
    mv.map(|v| (round2signed(v as i64 * num as i64 * DIV_MULT[den as usize] as i64, 14) as i32).clamp(-(1 << 14) + 1, (1 << 14) - 1))
}

impl FrameDec {
    /// lower_mv_precision.
    pub(crate) fn lower_precision(&self, mv: &mut Mv) {
        if self.h.allow_high_precision_mv {
            return;
        }
        for v in mv.iter_mut() {
            if self.h.force_integer_mv {
                let a = (v.abs() + 3) >> 3;
                *v = if *v > 0 { a << 3 } else { -(a << 3) };
            } else if *v & 1 != 0 {
                *v += if *v > 0 { -1 } else { 1 };
            }
        }
    }

    /// The global motion vector of the block for a reference frame (setup_global_mv).
    fn global_mv(&self, rf: i8) -> Mv {
        if rf <= 0 {
            return [0, 0];
        }
        let (typ, gm) = (self.h.gm_type[rf as usize], &self.h.gm_params[rf as usize]);
        let b = &self.b;
        let mut mv = if typ == 0 {
            [0, 0]
        } else if typ == GM_TRANSLATION {
            // row from the horizontal parameter, as the spec (and libaom) have it
            [gm[0] >> 13, gm[1] >> 13]
        } else {
            let x = (b.col * 4 + b.bw4 * 2) as i64 - 1;
            let y = (b.row * 4 + b.bh4 * 2) as i64 - 1;
            let xc = (gm[2] as i64 - 65536) * x + gm[3] as i64 * y + gm[0] as i64;
            let yc = gm[4] as i64 * x + (gm[5] as i64 - 65536) * y + gm[1] as i64;
            if self.h.allow_high_precision_mv {
                [round2signed(yc, 13) as i32, round2signed(xc, 13) as i32]
            } else {
                [round2signed(yc, 14) as i32 * 2, round2signed(xc, 14) as i32 * 2]
            }
        };
        self.lower_precision(&mut mv);
        mv
    }

    fn is_global_block(&self, m: &Mi, rf: i8) -> bool {
        (m.y_mode == GLOBALMV || m.y_mode == GLOBAL_GLOBALMV) && self.h.gm_type[rf as usize] > GM_TRANSLATION && MI_W_LOG2[m.size as usize] >= 1 && MI_H_LOG2[m.size as usize] >= 1
    }

    fn add_ref_mv_candidate(&self, st: &mut MvStack, m: &Mi, weight: u32) {
        if !m.is_inter {
            return;
        }
        let rf = self.b.ref_frame;
        if rf[1] <= 0 {
            for cl in 0..2 {
                if m.ref_frame[cl] == rf[0] {
                    let mut cand = if self.is_global_block(m, rf[0]) { st.global[0] } else { m.mv[cl] };
                    self.lower_precision(&mut cand);
                    if has_newmv(m.y_mode) {
                        st.new_count += 1;
                    }
                    st.found = true;
                    match (0..st.num).find(|&i| st.mv[i][0] == cand) {
                        Some(i) => st.weight[i] += weight,
                        None if st.num < 8 => {
                            st.mv[st.num][0] = cand;
                            st.weight[st.num] = weight;
                            st.num += 1;
                        }
                        None => {}
                    }
                }
            }
        } else if m.ref_frame == rf {
            let mut cand = [m.mv[0], m.mv[1]];
            for (i, c) in cand.iter_mut().enumerate() {
                if self.is_global_block(m, rf[i]) {
                    *c = st.global[i];
                }
                self.lower_precision(c);
            }
            if has_newmv(m.y_mode) {
                st.new_count += 1;
            }
            st.found = true;
            match (0..st.num).find(|&i| st.mv[i] == cand) {
                Some(i) => st.weight[i] += weight,
                None if st.num < 8 => {
                    st.mv[st.num] = cand;
                    st.weight[st.num] = weight;
                    st.num += 1;
                }
                None => {}
            }
        }
    }

    fn scan_row(&self, st: &mut MvStack, delta_row: isize) {
        let b = &self.b;
        let end4 = b.bw4.min(self.h.mi_cols - b.col).min(16);
        let (mut dr, mut dc) = (delta_row, 0isize);
        if dr.abs() > 1 {
            dr += (b.row & 1) as isize;
            dc = 1 - (b.col & 1) as isize;
        }
        let mut i = 0;
        while i < end4 {
            let (r, c) = (b.row as isize + dr, b.col as isize + dc + i as isize);
            if !self.inside(r, c) {
                break;
            }
            let m = *self.mi_at(r as usize, c as usize);
            let mut len = b.bw4.min(NUM_4X4_W[m.size as usize] as usize);
            if dr.abs() > 1 {
                len = len.max(2);
            }
            if b.bw4 >= 16 {
                len = len.max(4);
            }
            self.add_ref_mv_candidate(st, &m, 2 * len as u32);
            i += len;
        }
    }

    fn scan_col(&self, st: &mut MvStack, delta_col: isize) {
        let b = &self.b;
        let end4 = b.bh4.min(self.h.mi_rows - b.row).min(16);
        let (mut dr, mut dc) = (0isize, delta_col);
        if dc.abs() > 1 {
            dr = 1 - (b.row & 1) as isize;
            dc += (b.col & 1) as isize;
        }
        let mut i = 0;
        while i < end4 {
            let (r, c) = (b.row as isize + dr + i as isize, b.col as isize + dc);
            if !self.inside(r, c) {
                break;
            }
            let m = *self.mi_at(r as usize, c as usize);
            let mut len = b.bh4.min(NUM_4X4_H[m.size as usize] as usize);
            if dc.abs() > 1 {
                len = len.max(2);
            }
            if b.bh4 >= 16 {
                len = len.max(4);
            }
            self.add_ref_mv_candidate(st, &m, 2 * len as u32);
            i += len;
        }
    }

    fn scan_point(&self, st: &mut MvStack, dr: isize, dc: isize) {
        let (r, c) = (self.b.row as isize + dr, self.b.col as isize + dc);
        if self.inside(r, c) {
            let m = *self.mi_at(r as usize, c as usize);
            if m.coded {
                self.add_ref_mv_candidate(st, &m, 4);
            }
        }
    }

    /// add_tpl_ref_mv; returns whether the motion field had a vector there.
    fn add_tpl(&self, st: &mut MvStack, dr: isize, dc: isize) -> bool {
        let b = &self.b;
        let r = b.row as isize + if b.row & 1 == 1 { dr } else { dr + 1 };
        let c = b.col as isize + if b.col & 1 == 1 { dc } else { dc + 1 };
        if !self.inside(r, c) {
            return false;
        }
        let t = self.tpl[(r as usize >> 1) * self.tpl_stride + (c as usize >> 1)];
        if t.mv[0] == INVALID_MV {
            return false;
        }
        let rf = b.ref_frame;
        let mut cand = [[0; 2]; 2];
        for i in 0..1 + (rf[1] > 0) as usize {
            let off = rel_dist(&self.seq, self.h.order_hint, self.h.order_hints[rf[i] as usize]);
            cand[i] = project(t.mv, off, t.offset);
            self.lower_precision(&mut cand[i]);
        }
        let n = 1 + (rf[1] > 0) as usize;
        if dr == 0 && dc == 0 && (0..n).any(|i| (cand[i][0] - st.global[i][0]).abs() >= 16 || (cand[i][1] - st.global[i][1]).abs() >= 16) {
            st.zero_ctx = 1;
        }
        match (0..st.num).find(|&i| st.mv[i][0] == cand[0] && (n == 1 || st.mv[i][1] == cand[1])) {
            Some(i) => st.weight[i] += 2,
            None if st.num < 8 => {
                st.mv[st.num] = cand;
                st.weight[st.num] = 2;
                st.num += 1;
            }
            None => {}
        }
        true
    }

    fn temporal_scan(&self, st: &mut MvStack) {
        let (row, col, bw4, bh4) = (self.b.row, self.b.col, self.b.bw4, self.b.bh4);
        let step_h = if bh4 >= 16 { 4 } else { 2 };
        let step_w = if bw4 >= 16 { 4 } else { 2 };
        let mut avail = false;
        for dr in (0..bh4.min(16)).step_by(step_h) {
            for dc in (0..bw4.min(16)).step_by(step_w) {
                let ok = self.add_tpl(st, dr as isize, dc as isize);
                if dr == 0 && dc == 0 {
                    avail = ok;
                }
            }
        }
        if !avail {
            st.zero_ctx = 1;
        }
        if (2..16).contains(&bh4) && (2..16).contains(&bw4) {
            let (v, hz) = (bh4 as isize, bw4 as isize);
            for (dr, dc) in [(v, -2), (v, hz), (v - 2, hz)] {
                let (r, c) = ((row & 15) as isize + dr, (col & 15) as isize + dc);
                if (0..16).contains(&r) && (0..16).contains(&c) {
                    self.add_tpl(st, dr, dc);
                }
            }
        }
    }

    fn sort_stack(st: &mut MvStack, start: usize, end: usize) {
        let mut len = end;
        while len > start {
            let mut nr = start;
            for i in start + 1..len {
                if st.weight[i - 1] < st.weight[i] {
                    st.weight.swap(i - 1, i);
                    st.mv.swap(i - 1, i);
                    nr = i;
                }
            }
            len = nr;
        }
    }

    /// The candidate's vector for list `l` towards `rf`, negated when the sign biases differ.
    fn signed_mv(&self, m: &Mi, l: usize, rf: i8) -> Mv {
        let mv = m.mv[l];
        if self.h.sign_bias[m.ref_frame[l] as usize] != self.h.sign_bias[rf as usize] {
            [-mv[0], -mv[1]]
        } else {
            mv
        }
    }

    fn extra_search(&self, st: &mut MvStack) {
        let b = &self.b;
        let rf = b.ref_frame;
        let w4 = b.bw4.min(16).min(self.h.mi_cols - b.col);
        let h4 = b.bh4.min(16).min(self.h.mi_rows - b.row);
        let n4 = w4.min(h4);
        let mut neighbours = Vec::with_capacity(8);
        for pass in 0..2 {
            let mut idx = 0;
            while idx < n4 {
                let (r, c) = if pass == 0 { (b.row as isize - 1, (b.col + idx) as isize) } else { ((b.row + idx) as isize, b.col as isize - 1) };
                if !self.inside(r, c) {
                    break;
                }
                let m = *self.mi_at(r as usize, c as usize);
                neighbours.push(m);
                idx += if pass == 0 { NUM_4X4_W[m.size as usize] } else { NUM_4X4_H[m.size as usize] } as usize;
            }
        }
        if rf[1] > 0 {
            let mut id = [[[0; 2]; 2]; 2];
            let mut diff = [[[0; 2]; 2]; 2];
            let (mut nid, mut ndiff) = ([0usize; 2], [0usize; 2]);
            for m in &neighbours {
                for l in 0..2 {
                    let can = m.ref_frame[l];
                    for k in 0..2 {
                        if can == rf[k] && nid[k] < 2 {
                            id[k][nid[k]] = m.mv[l];
                            nid[k] += 1;
                        } else if can > 0 && ndiff[k] < 2 {
                            diff[k][ndiff[k]] = self.signed_mv(m, l, rf[k]);
                            ndiff[k] += 1;
                        }
                    }
                }
            }
            let mut comb = [[[0; 2]; 2]; 2];
            for k in 0..2 {
                let mut n = 0;
                for &v in id[k][..nid[k]].iter().chain(&diff[k][..ndiff[k]]) {
                    if n < 2 {
                        comb[n][k] = v;
                        n += 1;
                    }
                }
                while n < 2 {
                    comb[n][k] = st.global[k];
                    n += 1;
                }
            }
            if st.num == 1 {
                st.mv[1] = if comb[0] == st.mv[0] { comb[1] } else { comb[0] };
                st.weight[1] = 2;
                st.num = 2;
            } else {
                for c in comb {
                    st.mv[st.num] = c;
                    st.weight[st.num] = 2;
                    st.num += 1;
                }
            }
        } else {
            for m in &neighbours {
                if st.num >= 2 {
                    break;
                }
                for l in 0..2 {
                    if m.ref_frame[l] > 0 {
                        let v = self.signed_mv(m, l, rf[0]);
                        if !(0..st.num).any(|i| st.mv[i][0] == v) {
                            st.mv[st.num][0] = v;
                            st.weight[st.num] = 2;
                            st.num += 1;
                        }
                    }
                }
            }
            for i in st.num..2 {
                st.mv[i][0] = st.global[0];
            }
        }
    }

    /// find_mv_stack for the current block (whose reference frames are set).
    pub(crate) fn find_mv_stack(&self) -> MvStack {
        let b = &self.b;
        let (bw4, bh4) = (b.bw4, b.bh4);
        let compound = b.ref_frame[1] > 0;
        let mut st = MvStack::default();
        st.global[0] = self.global_mv(b.ref_frame[0]);
        if compound {
            st.global[1] = self.global_mv(b.ref_frame[1]);
        }
        self.scan_row(&mut st, -1);
        let mut above = st.found;
        st.found = false;
        self.scan_col(&mut st, -1);
        let mut left = st.found;
        st.found = false;
        if bw4.max(bh4) <= 16 {
            self.scan_point(&mut st, -1, bw4 as isize);
        }
        above |= st.found;
        let close = above as usize + left as usize;
        let (num_nearest, num_new) = (st.num, st.new_count);
        for w in st.weight[..num_nearest].iter_mut() {
            *w += REF_CAT_LEVEL;
        }
        st.zero_ctx = 0;
        if self.h.use_ref_frame_mvs {
            self.temporal_scan(&mut st);
        }
        st.found = false;
        self.scan_point(&mut st, -1, -1);
        above |= st.found;
        st.found = false;
        self.scan_row(&mut st, -3);
        above |= st.found;
        st.found = false;
        self.scan_col(&mut st, -3);
        left |= st.found;
        st.found = false;
        if bh4 > 1 {
            self.scan_row(&mut st, -5);
        }
        above |= st.found;
        st.found = false;
        if bw4 > 1 {
            self.scan_col(&mut st, -5);
        }
        left |= st.found;
        let total = above as usize + left as usize;
        let n = st.num;
        Self::sort_stack(&mut st, 0, num_nearest);
        Self::sort_stack(&mut st, num_nearest, n);
        if st.num < 2 {
            self.extra_search(&mut st);
        }
        for i in 0..st.num {
            st.drl_ctx[i] = if i + 1 < st.num {
                let (w0, w1) = (st.weight[i], st.weight[i + 1]);
                if w0 >= REF_CAT_LEVEL {
                    (w1 < REF_CAT_LEVEL) as usize
                } else {
                    2
                }
            } else {
                0
            };
        }
        let lists = 1 + compound as usize;
        for i in 0..st.num {
            for l in 0..lists {
                st.mv[i][l] = self.clamp_mv(st.mv[i][l]);
            }
        }
        (st.new_ctx, st.ref_ctx) = match close {
            0 => (total.min(1), total),
            1 => (3 - num_new.min(1), 2 + total),
            _ => (5 - num_new.min(1), 5),
        };
        st
    }

    /// clamp_mv_row and clamp_mv_col with the border of the MV stack.
    fn clamp_mv(&self, mv: Mv) -> Mv {
        let b = &self.b;
        let (bw4, bh4) = (b.bw4 as i32, b.bh4 as i32);
        let to_top = -((b.row as i32) * 32);
        let to_bottom = (self.h.mi_rows as i32 - bh4 - b.row as i32) * 32;
        let to_left = -((b.col as i32) * 32);
        let to_right = (self.h.mi_cols as i32 - bw4 - b.col as i32) * 32;
        let (br, bc) = (MV_BORDER + bh4 * 32, MV_BORDER + bw4 * 32);
        [mv[0].clamp(to_top - br, to_bottom + br), mv[1].clamp(to_left - bc, to_right + bc)]
    }

    /// has_overlappable_candidates.
    pub(crate) fn has_overlappable_candidates(&self) -> bool {
        let b = &self.b;
        if b.avail_u {
            for x4 in (b.col..(b.col + b.bw4).min(self.h.mi_cols)).step_by(2) {
                let x5 = (x4 | 1).min(self.h.mi_cols - 1);
                if self.mi_at(b.row - 1, x5).ref_frame[0] > 0 {
                    return true;
                }
            }
        }
        if b.avail_l {
            for y4 in (b.row..(b.row + b.bh4).min(self.h.mi_rows)).step_by(2) {
                let y5 = (y4 | 1).min(self.h.mi_rows - 1);
                if self.mi_at(y5, b.col - 1).ref_frame[0] > 0 {
                    return true;
                }
            }
        }
        false
    }

    fn add_sample(&self, s: &mut WarpSamples, dr: isize, dc: isize) {
        if s.scanned >= 8 {
            return;
        }
        let b = &self.b;
        let (r, c) = (b.row as isize + dr, b.col as isize + dc);
        if !self.inside(r, c) {
            return;
        }
        let m = *self.mi_at(r as usize, c as usize);
        if !m.coded || m.ref_frame[0] != b.ref_frame[0] || m.ref_frame[1] != -1 {
            return;
        }
        let (cw4, ch4) = (NUM_4X4_W[m.size as usize] as isize, NUM_4X4_H[m.size as usize] as isize);
        let (cr, cc) = (r & !(ch4 - 1), c & !(cw4 - 1));
        let mid_y = (cr * 4 + ch4 * 2 - 1) as i32;
        let mid_x = (cc * 4 + cw4 * 2 - 1) as i32;
        let thresh = ((b.bw4.max(b.bh4) * 4) as i32).clamp(16, 112);
        let diff = (m.mv[0][0] - b.mv[0][0]).abs() + (m.mv[0][1] - b.mv[0][1]).abs();
        let valid = diff <= thresh;
        let cand = [mid_y * 8, mid_x * 8, mid_y * 8 + m.mv[0][0], mid_x * 8 + m.mv[0][1]];
        s.scanned += 1;
        if !valid && s.scanned > 1 {
            return;
        }
        s.cand[s.num] = cand;
        if valid {
            s.num += 1;
        }
    }

    /// find_warp_samples.
    pub(crate) fn find_warp_samples(&self) -> WarpSamples {
        let b = &self.b;
        let mut s = WarpSamples::default();
        let (w4, h4) = (b.bw4, b.bh4);
        let (mut do_tl, mut do_tr) = (true, true);
        if b.avail_u {
            let src_w = NUM_4X4_W[self.mi_at(b.row - 1, b.col).size as usize] as usize;
            if w4 <= src_w {
                let col_off = -((b.col & (src_w - 1)) as isize);
                if col_off < 0 {
                    do_tl = false;
                }
                if col_off + src_w as isize > w4 as isize {
                    do_tr = false;
                }
                self.add_sample(&mut s, -1, 0);
            } else {
                let mut i = 0;
                while i < w4.min(self.h.mi_cols - b.col) {
                    let src_w = NUM_4X4_W[self.mi_at(b.row - 1, b.col + i).size as usize] as usize;
                    self.add_sample(&mut s, -1, i as isize);
                    i += src_w;
                }
            }
        }
        if b.avail_l {
            let src_h = NUM_4X4_H[self.mi_at(b.row, b.col - 1).size as usize] as usize;
            if h4 <= src_h {
                if b.row & (src_h - 1) != 0 {
                    do_tl = false;
                }
                self.add_sample(&mut s, 0, -1);
            } else {
                let mut i = 0;
                while i < h4.min(self.h.mi_rows - b.row) {
                    let src_h = NUM_4X4_H[self.mi_at(b.row + i, b.col - 1).size as usize] as usize;
                    self.add_sample(&mut s, i as isize, -1);
                    i += src_h;
                }
            }
        }
        if do_tl {
            self.add_sample(&mut s, -1, -1);
        }
        if do_tr && w4.max(h4) <= 16 {
            self.add_sample(&mut s, -1, w4 as isize);
        }
        if s.num == 0 && s.scanned > 0 {
            s.num = 1;
        }
        s
    }

    /// The motion field estimation (7.9): projects the saved motion vectors of the references onto
    /// this frame, as libaom's av1_setup_motion_field.
    pub(crate) fn setup_motion_field(&mut self) {
        self.tpl.iter_mut().for_each(|t| t.mv[0] = INVALID_MV);
        if !self.seq.enable_order_hint {
            return;
        }
        let cur = self.h.order_hint;
        let hint = |f: &FrameDec, rf: usize| f.h.order_hints[rf];
        let mut stamp = 2i32;
        // LAST, unless it is the overlay of the golden frame
        if let Some(last) = &self.refs[1] {
            if last.saved_hints[7] != hint(self, 4) {
                self.project_from(1, true);
            }
            stamp -= 1;
        }
        if rel_dist(&self.seq, hint(self, 5), cur) > 0 && self.project_from(5, false) {
            stamp -= 1;
        }
        if rel_dist(&self.seq, hint(self, 6), cur) > 0 && self.project_from(6, false) {
            stamp -= 1;
        }
        if rel_dist(&self.seq, hint(self, 7), cur) > 0 && stamp >= 0 && self.project_from(7, false) {
            stamp -= 1;
        }
        if stamp >= 0 {
            self.project_from(2, true);
        }
    }

    /// motion_field_projection from reference `src`; `backward` is libaom's dir == 2.
    fn project_from(&mut self, src: usize, backward: bool) -> bool {
        let Some(r) = self.refs[src].clone() else { return false };
        if r.frame_type == KEY_FRAME || r.frame_type == INTRA_ONLY_FRAME || r.mi_rows != self.h.mi_rows || r.mi_cols != self.h.mi_cols {
            return false;
        }
        let seq = &self.seq;
        let mut ref_off = [0i32; 8];
        for (rf, o) in ref_off.iter_mut().enumerate().skip(1) {
            *o = rel_dist(seq, r.order_hint, r.saved_hints[rf]);
        }
        let mut to_cur = rel_dist(seq, r.order_hint, self.h.order_hint);
        if backward {
            to_cur = -to_cur;
        }
        let (rows, cols) = ((self.h.mi_rows + 1) >> 1, (self.h.mi_cols + 1) >> 1);
        let (max_r, max_c) = ((self.h.mi_rows >> 1) as i32, (self.h.mi_cols >> 1) as i32);
        for br in 0..rows {
            for bc in 0..cols {
                let s = r.mvs[br * cols + bc];
                if s.rf <= 0 {
                    continue;
                }
                let off = ref_off[s.rf as usize];
                if !(off > 0 && off <= MAX_FRAME_DISTANCE && to_cur.abs() <= MAX_FRAME_DISTANCE) {
                    continue;
                }
                let p = project(s.mv, to_cur, off);
                let ro = if p[0] >= 0 { p[0] >> 6 } else { -((-p[0]) >> 6) };
                let co = if p[1] >= 0 { p[1] >> 6 } else { -((-p[1]) >> 6) };
                let (row, col) = if backward { (br as i32 - ro, bc as i32 - co) } else { (br as i32 + ro, bc as i32 + co) };
                let (base_r, base_c) = (((br >> 3) << 3) as i32, ((bc >> 3) << 3) as i32);
                if row < 0 || row >= max_r || col < 0 || col >= max_c || row < base_r || row >= base_r + 8 || col < base_c - 8 || col >= base_c + 16 {
                    continue;
                }
                let t = &mut self.tpl[row as usize * self.tpl_stride + col as usize];
                t.mv = s.mv;
                t.offset = off;
            }
        }
        true
    }

    /// Saves the block's motion for later frames (av1_copy_frame_mvs).
    pub(crate) fn save_mvs(&mut self) {
        let b = &self.b;
        let cols = (self.h.mi_cols + 1) >> 1;
        let (x_mis, y_mis) = (b.bw4.min(self.h.mi_cols - b.col), b.bh4.min(self.h.mi_rows - b.row));
        let mut s = SavedMv { rf: -1, mv: [0, 0] };
        for l in 0..2 {
            let rf = b.ref_frame[l];
            if rf > 0 {
                if rel_dist(&self.seq, self.h.order_hints[rf as usize], self.h.order_hint) >= 0 && self.seq.enable_order_hint {
                    continue;
                }
                if b.mv[l][0].abs() > 4095 || b.mv[l][1].abs() > 4095 {
                    continue;
                }
                s = SavedMv { rf, mv: b.mv[l] };
            }
        }
        for y in (b.row >> 1)..(b.row >> 1) + y_mis.div_ceil(2) {
            for x in (b.col >> 1)..(b.col >> 1) + x_mis.div_ceil(2) {
                self.saved_mvs[y * cols + x] = s;
            }
        }
    }
}

/// The warp samples of a block (CandList, NumSamples, NumSamplesScanned).
#[derive(Default)]
pub struct WarpSamples {
    pub num: usize,
    pub scanned: usize,
    pub cand: [[i32; 4]; 8],
}
