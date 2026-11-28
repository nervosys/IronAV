//! Inter prediction: prediction unit syntax, motion vector derivation (merge, AMVP and temporal
//! candidates) and motion compensation with weighted prediction (ITU-T H.265 8.5.3).

use super::frame::*;
use super::ps::SliceType;
use super::shared::SharedFrame;
use super::slice::{bad, Dec, PartMode};
use super::tables::*;
use super::RefPic;
use ironav_core::Result;

const COMB: [(usize, usize); 12] = [(0, 1), (1, 0), (0, 2), (2, 0), (1, 2), (2, 1), (0, 3), (3, 0), (1, 3), (3, 1), (2, 3), (3, 2)];

fn same_motion(a: &MvField, b: &MvField) -> bool {
    a.pred & 3 == b.pred & 3 && (!a.uses(0) || (a.mv[0] == b.mv[0] && a.ref_idx[0] == b.ref_idx[0])) && (!a.uses(1) || (a.mv[1] == b.mv[1] && a.ref_idx[1] == b.ref_idx[1]))
}

pub(super) fn scale_mv(mv: [i16; 2], td: i32, tb: i32) -> [i16; 2] {
    let td = td.clamp(-128, 127);
    let tb = tb.clamp(-128, 127);
    if td == 0 {
        return mv;
    }
    let tx = (16384 + (td.abs() >> 1)) / td;
    let f = ((tb * tx + 32) >> 6).clamp(-4096, 4095);
    let s = |v: i16| -> i16 {
        let p = f * v as i32;
        (p.signum() * ((p.abs() + 127) >> 8)).clamp(-32768, 32767) as i16
    };
    [s(mv[0]), s(mv[1])]
}

impl<'a> Dec<'a> {
    fn ref_of(&self, l: usize, idx: i8) -> Option<&RefPic> {
        self.refs[l].get(idx as usize)
    }

    /// Fills in the reference POCs and long-term bits of a motion field from its indices.
    fn complete(&self, m: &mut MvField) {
        for l in 0..2 {
            if m.uses(l) {
                match self.ref_of(l, m.ref_idx[l]) {
                    Some(r) => {
                        m.ref_poc[l] = r.poc;
                        if r.long_term {
                            m.pred |= 4 << l;
                        }
                    }
                    None => m.pred &= !(1 << l),
                }
            }
        }
    }

    fn mv_at(&self, x: usize, y: usize) -> MvField {
        self.pic.frame.mv[(y >> 2) * self.pic.w4 + (x >> 2)]
    }

    /// Availability of the prediction block covering (`xn`, `yn`) for the one at (`xp`, `yp`) (6.4.2).
    #[allow(clippy::too_many_arguments)]
    fn pb_avail(&self, xcb: usize, ycb: usize, ncbs: usize, xp: usize, yp: usize, w: usize, h: usize, part_idx: usize, xn: i32, yn: i32) -> bool {
        let same_cb = xn >= xcb as i32 && yn >= ycb as i32 && xn < (xcb + ncbs) as i32 && yn < (ycb + ncbs) as i32;
        let a = if !same_cb {
            self.avail(xp, yp, xn, yn)
        } else {
            !((w << 1) == ncbs && (h << 1) == ncbs && part_idx == 1 && ycb + h <= yn as usize && xcb + w > xn as usize)
        };
        a && self.pic.pred_mode[self.i4(xn as usize, yn as usize)] == PRED_INTER
    }

    // ------------------------------------------------------------ syntax

    pub(super) fn parse_inter_pus(&mut self, x0: usize, y0: usize, size: usize) -> Result<bool> {
        use PartMode::*;
        let s = size;
        let parts: &[(usize, usize, usize, usize)] = match self.part_mode {
            P2Nx2N => &[(0, 0, s, s)],
            P2NxN => &[(0, 0, s, s / 2), (0, s / 2, s, s / 2)],
            PNx2N => &[(0, 0, s / 2, s), (s / 2, 0, s / 2, s)],
            PNxN => &[(0, 0, s / 2, s / 2), (s / 2, 0, s / 2, s / 2), (0, s / 2, s / 2, s / 2), (s / 2, s / 2, s / 2, s / 2)],
            P2NxnU => &[(0, 0, s, s / 4), (0, s / 4, s, s * 3 / 4)],
            P2NxnD => &[(0, 0, s, s * 3 / 4), (0, s * 3 / 4, s, s / 4)],
            PnLx2N => &[(0, 0, s / 4, s), (s / 4, 0, s * 3 / 4, s)],
            PnRx2N => &[(0, 0, s * 3 / 4, s), (s * 3 / 4, 0, s / 4, s)],
        };
        let mut merge0 = false;
        for (i, &(dx, dy, w, h)) in parts.iter().enumerate() {
            let merged = self.pu(x0 + dx, y0 + dy, w, h, x0, y0, size, i, false)?;
            if i == 0 {
                merge0 = merged;
            }
        }
        Ok(merge0)
    }

    /// A skipped coding unit: one merged 2Nx2N prediction unit.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn prediction_unit(&mut self, x0: usize, y0: usize, w: usize, h: usize, xcb: usize, ycb: usize, ncbs: usize, part_idx: usize, skip: bool) -> Result<()> {
        self.pu(x0, y0, w, h, xcb, ycb, ncbs, part_idx, skip).map(|_| ())
    }

    fn parse_ref_idx(&mut self, n: usize) -> usize {
        let max = n - 1;
        let mut i = 0;
        while i < 2 && i < max && self.cab.decision(REF_IDX_L0_OFFSET + i) == 1 {
            i += 1;
        }
        if i == 2 {
            while i < max && self.cab.bypass() == 1 {
                i += 1;
            }
        }
        i
    }

    fn parse_mvd(&mut self) -> Result<[i32; 2]> {
        let g0 = [self.cab.decision(ABS_MVD_GREATER0_FLAG_OFFSET) == 1, self.cab.decision(ABS_MVD_GREATER0_FLAG_OFFSET) == 1];
        let mut g1 = [false; 2];
        for c in 0..2 {
            if g0[c] {
                g1[c] = self.cab.decision(ABS_MVD_GREATER1_FLAG_OFFSET + 1) == 1;
            }
        }
        let mut out = [0i32; 2];
        for c in 0..2 {
            if !g0[c] {
                continue;
            }
            let mut abs = 1i32;
            if g1[c] {
                let mut k = 1u32;
                let mut v = 0i32;
                while self.cab.bypass() == 1 {
                    v += 1 << k;
                    k += 1;
                    if k > 20 {
                        return Err(bad("motion vector difference is too large"));
                    }
                }
                v += self.cab.bypass_bits(k) as i32;
                abs = v + 2;
            }
            out[c] = if self.cab.bypass() == 1 { -abs } else { abs };
        }
        Ok(out)
    }

    /// One prediction unit: syntax, motion derivation and motion compensation. Returns the merge flag.
    #[allow(clippy::too_many_arguments)]
    fn pu(&mut self, x: usize, y: usize, w: usize, h: usize, xcb: usize, ycb: usize, ncbs: usize, part_idx: usize, skip: bool) -> Result<bool> {
        let hdr = self.hdr;
        let merge = skip || self.cab.decision(MERGE_FLAG_OFFSET) == 1;
        let mut m;
        if merge {
            let mut idx = 0;
            if hdr.max_num_merge_cand > 1 && self.cab.decision(MERGE_IDX_OFFSET) == 1 {
                idx = 1;
                while idx < hdr.max_num_merge_cand - 1 && self.cab.bypass() == 1 {
                    idx += 1;
                }
            }
            m = self.merge_candidate(x, y, w, h, xcb, ycb, ncbs, part_idx, idx);
            if m.pred & 3 == 3 && w + h == 12 {
                m.pred &= !2;
                m.ref_idx[1] = -1;
                m.mv[1] = [0, 0];
            }
        } else {
            let mut idc = 0; // 0 = L0, 1 = L1, 2 = bi
            if hdr.slice_type == SliceType::B {
                let depth = self.pic.ct_depth[self.i4(xcb, ycb)] as usize;
                idc = if w + h != 12 && self.cab.decision(INTER_PRED_IDC_OFFSET + depth) == 1 { 2 } else { self.cab.decision(INTER_PRED_IDC_OFFSET + 4) as usize };
            }
            m = MvField { ref_idx: [-1, -1], ..Default::default() };
            let mut mvd = [[0i32; 2]; 2];
            let mut mvp_flag = [0usize; 2];
            for l in 0..2 {
                if !(idc == 2 || idc == l) {
                    continue;
                }
                let n = hdr.num_ref_idx[l];
                if n == 0 {
                    return Err(bad("prediction from an empty reference list"));
                }
                m.ref_idx[l] = self.parse_ref_idx(n) as i8;
                m.pred |= 1 << l;
                mvd[l] = if l == 1 && hdr.mvd_l1_zero && idc == 2 { [0, 0] } else { self.parse_mvd()? };
                mvp_flag[l] = self.cab.decision(MVP_LX_FLAG_OFFSET) as usize;
            }
            self.complete(&mut m);
            for l in 0..2 {
                if m.uses(l) {
                    let p = self.amvp(x, y, w, h, xcb, ycb, ncbs, part_idx, l, m.ref_idx[l] as usize, mvp_flag[l]);
                    m.mv[l] = [(p[0] as i32 + mvd[l][0]) as i16, (p[1] as i32 + mvd[l][1]) as i16];
                }
            }
        }
        self.complete(&mut m);
        if m.pred & 3 == 0 {
            return Err(bad("prediction unit without a usable reference"));
        }
        // store the motion and mark the prediction block edges for the deblocking filter
        let w4 = self.pic.w4;
        for yy in (y >> 2)..((y + h) >> 2) {
            for xx in (x >> 2)..((x + w) >> 2) {
                self.pic.frame.mv[yy * w4 + xx] = m;
            }
        }
        self.mark_edges(x, y, w, h, EDGE_PU_V | EDGE_PU_H);
        self.motion_compensate(x, y, w, h, &m)?;
        Ok(merge)
    }

    // ------------------------------------------------------------ merge mode

    #[allow(clippy::too_many_arguments)]
    fn merge_candidate(&mut self, x: usize, y: usize, w: usize, h: usize, xcb: usize, ycb: usize, ncbs: usize, part_idx: usize, merge_idx: usize) -> MvField {
        let pps = self.pps;
        let (mut x, mut y, mut w, mut h, mut part_idx) = (x, y, w, h, part_idx);
        let mut part_mode = self.part_mode;
        // with a large parallel merge level the prediction units of an 8x8 CU share one candidate list
        if pps.log2_parallel_merge_level > 2 && ncbs == 8 {
            (x, y, w, h, part_idx) = (xcb, ycb, ncbs, ncbs, 0);
            part_mode = PartMode::P2Nx2N;
        }
        let pml = pps.log2_parallel_merge_level;
        let max = self.hdr.max_num_merge_cand;
        let mut list: Vec<MvField> = Vec::with_capacity(5);
        let (xi, yi, wi, hi) = (x as i32, y as i32, w as i32, h as i32);
        let get = |s: &Self, xn: i32, yn: i32| -> Option<MvField> {
            if (xi >> pml) == (xn >> pml) && (yi >> pml) == (yn >> pml) {
                return None;
            }
            if !s.pb_avail(xcb, ycb, ncbs, x, y, w, h, part_idx, xn, yn) {
                return None;
            }
            Some(s.mv_at(xn as usize, yn as usize))
        };
        let eq = |a: &Option<MvField>, b: &MvField| a.as_ref().is_some_and(|a| same_motion(a, b));
        // spatial candidates; the availability used for pruning ignores earlier pruning
        let a1 = if matches!(part_mode, PartMode::PNx2N | PartMode::PnLx2N | PartMode::PnRx2N) && part_idx == 1 { None } else { get(self, xi - 1, yi + hi - 1) };
        let b1 = if matches!(part_mode, PartMode::P2NxN | PartMode::P2NxnU | PartMode::P2NxnD) && part_idx == 1 { None } else { get(self, xi + wi - 1, yi - 1) };
        let b0 = get(self, xi + wi, yi - 1);
        let a0 = get(self, xi - 1, yi + hi);
        let b2 = get(self, xi - 1, yi - 1);
        if let Some(c) = a1 {
            list.push(c);
        }
        if let Some(c) = b1 {
            if !eq(&a1, &c) {
                list.push(c);
            }
        }
        if let Some(c) = b0 {
            if !eq(&b1, &c) {
                list.push(c);
            }
        }
        if let Some(c) = a0 {
            if !eq(&a1, &c) {
                list.push(c);
            }
        }
        if let Some(c) = b2 {
            if list.len() < 4 && !eq(&a1, &c) && !eq(&b1, &c) {
                list.push(c);
            }
        }
        if list.len() > merge_idx {
            return Self::strip(list[merge_idx]);
        }
        // temporal candidate
        if self.hdr.temporal_mvp {
            let mut c = MvField { ref_idx: [-1, -1], ..Default::default() };
            if let Some(mv) = self.temporal_mv(x, y, w, h, 0, 0) {
                c.mv[0] = mv;
                c.ref_idx[0] = 0;
                c.pred |= 1;
            }
            if self.hdr.slice_type == SliceType::B {
                if let Some(mv) = self.temporal_mv(x, y, w, h, 0, 1) {
                    c.mv[1] = mv;
                    c.ref_idx[1] = 0;
                    c.pred |= 2;
                }
            }
            if c.pred & 3 != 0 {
                list.push(c);
            }
        }
        list.truncate(max);
        if list.len() > merge_idx {
            return Self::strip(list[merge_idx]);
        }
        // combined bi-predictive candidates
        if self.hdr.slice_type == SliceType::B && list.len() > 1 && list.len() < max {
            let orig = list.len();
            for &(i0, i1) in COMB.iter().take(orig * (orig - 1)) {
                let (l0, l1) = (list[i0], list[i1]);
                if l0.uses(0) && l1.uses(1) {
                    let p0 = self.refs[0].get(l0.ref_idx[0] as usize).map(|r| r.poc);
                    let p1 = self.refs[1].get(l1.ref_idx[1] as usize).map(|r| r.poc);
                    if p0 != p1 || l0.mv[0] != l1.mv[1] {
                        list.push(MvField { mv: [l0.mv[0], l1.mv[1]], ref_idx: [l0.ref_idx[0], l1.ref_idx[1]], pred: 3, ..Default::default() });
                        if list.len() == max {
                            break;
                        }
                    }
                }
            }
        }
        if list.len() > merge_idx {
            return Self::strip(list[merge_idx]);
        }
        // zero candidates
        let nref = if self.hdr.slice_type == SliceType::P { self.hdr.num_ref_idx[0] } else { self.hdr.num_ref_idx[0].min(self.hdr.num_ref_idx[1]) };
        let mut zero = 0usize;
        while list.len() <= merge_idx {
            let r = if zero < nref { zero as i8 } else { 0 };
            let b = self.hdr.slice_type == SliceType::B;
            list.push(MvField { ref_idx: [r, if b { r } else { -1 }], pred: if b { 3 } else { 1 }, ..Default::default() });
            zero += 1;
        }
        Self::strip(list[merge_idx])
    }

    /// Candidates carry their reference POCs only after `complete`; start clean.
    fn strip(mut m: MvField) -> MvField {
        m.ref_poc = [0; 2];
        m.pred &= 3;
        if m.pred & 1 == 0 {
            m.ref_idx[0] = -1;
            m.mv[0] = [0, 0];
        }
        if m.pred & 2 == 0 {
            m.ref_idx[1] = -1;
            m.mv[1] = [0, 0];
        }
        m
    }

    // ------------------------------------------------------------ temporal prediction

    fn no_backward_pred(&self) -> bool {
        self.refs.iter().all(|l| l.iter().all(|r| r.poc <= self.poc))
    }

    fn temporal_mv(&self, x: usize, y: usize, w: usize, h: usize, ref_idx: usize, list: usize) -> Option<[i16; 2]> {
        let hdr = self.hdr;
        let col_list = if hdr.slice_type == SliceType::B && !hdr.collocated_from_l0 { 1 } else { 0 };
        let col = &self.refs[col_list].get(hdr.collocated_ref_idx)?.frame;
        let (xbr, ybr) = (x + w, y + h);
        if (y >> self.sps.log2_ctb) == (ybr >> self.sps.log2_ctb) && ybr < self.sps.height && xbr < self.sps.width {
            if let Some(mv) = self.col_mv(col, (xbr >> 4) << 4, (ybr >> 4) << 4, ref_idx, list) {
                return Some(mv);
            }
        }
        let (xc, yc) = (x + w / 2, y + h / 2);
        self.col_mv(col, (xc >> 4) << 4, (yc >> 4) << 4, ref_idx, list)
    }

    fn col_mv(&self, col: &SharedFrame, x: usize, y: usize, ref_idx: usize, list: usize) -> Option<[i16; 2]> {
        // waits for the collocated picture to have decoded and filtered that far
        let m = col.mv((y >> 2) * col.w4 + (x >> 2))?;
        if m.pred & 3 == 0 {
            return None;
        }
        let l = if !m.uses(0) {
            1
        } else if !m.uses(1) {
            0
        } else if self.no_backward_pred() {
            list
        } else {
            self.hdr.collocated_from_l0 as usize
        };
        let cur = self.refs[list].get(ref_idx)?;
        if cur.long_term != m.long_term(l) {
            return None;
        }
        let col_diff = col.poc - m.ref_poc[l];
        let cur_diff = self.poc - cur.poc;
        if cur.long_term || col_diff == cur_diff || col_diff == 0 {
            Some(m.mv[l])
        } else {
            Some(scale_mv(m.mv[l], col_diff, cur_diff))
        }
    }

    // ------------------------------------------------------------ AMVP

    #[allow(clippy::too_many_arguments)]
    fn amvp(&self, x: usize, y: usize, w: usize, h: usize, xcb: usize, ycb: usize, ncbs: usize, part_idx: usize, l: usize, ref_idx: usize, mvp_flag: usize) -> [i16; 2] {
        let (xi, yi, wi, hi) = (x as i32, y as i32, w as i32, h as i32);
        let cur = &self.refs[l][ref_idx];
        let (cur_poc, cur_lt) = (cur.poc, cur.long_term);
        let nb = |xn: i32, yn: i32| -> Option<MvField> {
            if self.pb_avail(xcb, ycb, ncbs, x, y, w, h, part_idx, xn, yn) {
                Some(self.mv_at(xn as usize, yn as usize))
            } else {
                None
            }
        };
        let a = [nb(xi - 1, yi + hi), nb(xi - 1, yi + hi - 1)];
        let b = [nb(xi + wi, yi - 1), nb(xi + wi - 1, yi - 1), nb(xi - 1, yi - 1)];
        let is_scaled = a[0].is_some() || a[1].is_some();
        // a candidate pointing at the very same picture, from list l first and then the other list
        let direct = |m: &MvField| -> Option<[i16; 2]> {
            [l, 1 - l].into_iter().find(|&ll| m.uses(ll) && m.ref_poc[ll] == cur_poc).map(|ll| m.mv[ll])
        };
        // a candidate with the same long-term status, scaled by the POC distances when both are short-term
        let scaled = |m: &MvField| -> Option<[i16; 2]> {
            for ll in [l, 1 - l] {
                if m.uses(ll) && m.long_term(ll) == cur_lt {
                    let mv = m.mv[ll];
                    if !cur_lt {
                        return Some(scale_mv(mv, self.poc - m.ref_poc[ll], self.poc - cur_poc));
                    }
                    return Some(mv);
                }
            }
            None
        };
        let mut mva = a.iter().flatten().find_map(direct);
        if mva.is_none() {
            mva = a.iter().flatten().find_map(scaled);
        }
        let mut mvb = b.iter().flatten().find_map(direct);
        if !is_scaled {
            if mvb.is_some() {
                mva = mvb;
            }
            mvb = b.iter().flatten().find_map(scaled);
        }
        let mut cands: Vec<[i16; 2]> = Vec::with_capacity(3);
        if let Some(v) = mva {
            cands.push(v);
        }
        if let Some(v) = mvb {
            if mva != Some(v) {
                cands.push(v);
            }
        }
        if cands.len() < 2 && self.hdr.temporal_mvp {
            if let Some(v) = self.temporal_mv(x, y, w, h, ref_idx, l) {
                cands.push(v);
            }
        }
        cands.get(mvp_flag).copied().unwrap_or([0, 0])
    }

    // ------------------------------------------------------------ motion compensation

    fn motion_compensate(&mut self, x: usize, y: usize, w: usize, h: usize, m: &MvField) -> Result<()> {
        let sps = self.sps;
        let refs = self.refs;
        let ncomp = if sps.chroma_format_idc == 0 { 1 } else { 3 };
        let weighted = match self.hdr.slice_type {
            SliceType::P => self.pps.weighted_pred,
            _ => self.pps.weighted_bipred,
        };
        let bi = m.uses(0) && m.uses(1);
        for c in 0..ncomp {
            let (sx, sy) = if c == 0 { (0, 0) } else { sps.chroma_shift() };
            let (bx, by, bw, bh) = (x >> sx, y >> sy, w >> sx, h >> sy);
            let bd = if c == 0 { self.bd_y } else { self.bd_c };
            let McScratch { pred, pad, tmp } = &mut self.mc;
            for l in 0..2 {
                if !m.uses(l) {
                    continue;
                }
                let r = refs[l].get(m.ref_idx[l] as usize).ok_or_else(|| bad("reference picture index is out of range"))?;
                let f = &*r.frame;
                let (pw, ph) = if c == 0 { (f.width, f.height) } else { (f.cwidth, f.cheight) };
                let out = &mut pred[l][..bw * bh];
                // The last row of this plane the interpolation reads (8 taps reach 3 rows above and 4 below,
                // 4 taps 1 above and 2 below), as a number of luma rows. The reference picture may still be
                // decoding: its rows become readable as they are final.
                let dy = if c == 0 { (m.mv[l][1] >> 2) as i32 } else if sy == 1 { (m.mv[l][1] >> 3) as i32 } else { (m.mv[l][1] >> 2) as i32 };
                let last = by as i32 + dy + bh as i32 + if c == 0 { 3 } else { 2 };
                let rows = ((last + 1).max(0) as usize).min(ph) << if c == 0 { 0 } else { sy };
                let plane = f.plane(c, rows);
                if c == 0 {
                    luma_mc_with(plane, pw, ph, bx as i32, by as i32, m.mv[l], bw, bh, bd, out, pad, tmp);
                } else {
                    chroma_mc_with(plane, pw, ph, bx as i32, by as i32, m.mv[l], (sx, sy), bw, bh, bd, out, pad, tmp);
                }
            }
            let cw = if c == 0 { self.pic.frame.width } else { self.pic.frame.cwidth };
            let shift1 = 14 - bd as i32;
            let wt = if weighted { self.hdr.pwt.as_ref() } else { None };
            let log2wd = match wt {
                Some(t) => (if c == 0 { t.luma_log2_denom } else { t.chroma_log2_denom }) as i32 + shift1,
                None => 0,
            };
            let params = |l: usize| -> (i32, i32) {
                let wp = wt.unwrap().l[l].get(m.ref_idx[l] as usize).copied().unwrap_or_default();
                (wp.weight[c], wp.offset[c] << (bd - 8))
            };
            let l0 = if m.uses(0) { 0 } else { 1 };
            let fin = Finish { bw, bh, stride: cw, maxv: (1i32 << bd) - 1, bd, bi, wt: wt.is_some(), log2wd, p: [params_or_zero(wt.is_some(), &params, l0), params_or_zero(wt.is_some() && bi, &params, 1)], l0 };
            let dst = &mut self.pic.frame.planes[c][by * cw + bx..];
            finish(dst, &fin, &pred[0][..bw * bh], &pred[1][..bw * bh]);
        }
        Ok(())
    }
}

fn params_or_zero(on: bool, params: &impl Fn(usize) -> (i32, i32), l: usize) -> (i32, i32) {
    if on {
        params(l)
    } else {
        (0, 0)
    }
}

/// Scratch space for motion compensation, kept from block to block.
pub(super) struct McScratch {
    pred: [Vec<i16>; 2],
    pad: Vec<u16>,
    tmp: Vec<i16>,
}

impl McScratch {
    pub(super) fn new() -> Self {
        McScratch { pred: [vec![0; 64 * 64], vec![0; 64 * 64]], pad: vec![0; 71 * 71], tmp: vec![0; 71 * 64] }
    }
}

/// What turns the interpolated samples of a block into picture samples.
struct Finish {
    bw: usize,
    bh: usize,
    /// Distance between rows of the picture plane.
    stride: usize,
    maxv: i32,
    bd: u32,
    bi: bool,
    /// Explicit weighted prediction: `p` holds (weight, offset) of the list used (or of both for bi).
    wt: bool,
    log2wd: i32,
    p: [(i32, i32); 2],
    /// The list a single-list prediction comes from.
    l0: usize,
}

/// Writes the prediction of a block into the picture at `dst` (the block's top left sample), from the 14-bit
/// samples of list 0 (`p0`) and list 1 (`p1`), with default or explicit weighting (8.5.3.3.4).
fn finish(dst: &mut [u16], f: &Finish, p0: &[i16], p1: &[i16]) {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 was detected just above.
        return unsafe { finish_avx2(dst, f, p0, p1) };
    }
    finish_impl(dst, f, p0, p1)
}

/// `finish_impl` compiled for AVX2, so that its loops are vectorised with it.
///
/// SAFETY: callers check that AVX2 is available.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn finish_avx2(dst: &mut [u16], f: &Finish, p0: &[i16], p1: &[i16]) {
    finish_impl(dst, f, p0, p1)
}

#[inline(always)]
fn finish_impl(dst: &mut [u16], f: &Finish, p0: &[i16], p1: &[i16]) {
    let (bw, maxv) = (f.bw, f.maxv);
    let shift1 = 14 - f.bd as i32;
    for yy in 0..f.bh {
        let d = &mut dst[yy * f.stride..yy * f.stride + bw];
        let a = &p0[yy * bw..yy * bw + bw];
        let b = &p1[yy * bw..yy * bw + bw];
        let one = if f.l0 == 0 { a } else { b };
        if f.bi {
            if f.wt {
                let ((w0, o0), (w1, o1)) = (f.p[0], f.p[1]);
                let (rnd, sh) = ((o0 + o1 + 1) << f.log2wd, f.log2wd + 1);
                for ((d, &a), &b) in d.iter_mut().zip(a).zip(b) {
                    *d = ((a as i32 * w0 + b as i32 * w1 + rnd) >> sh).clamp(0, maxv) as u16;
                }
            } else {
                let (rnd, sh) = (1 << (14 - f.bd as i32), 15 - f.bd as i32);
                for ((d, &a), &b) in d.iter_mut().zip(a).zip(b) {
                    *d = ((a as i32 + b as i32 + rnd) >> sh).clamp(0, maxv) as u16;
                }
            }
        } else if f.wt {
            let (w0, o0) = f.p[0];
            if f.log2wd >= 1 {
                let (rnd, sh) = (1 << (f.log2wd - 1), f.log2wd);
                for (d, &a) in d.iter_mut().zip(one) {
                    *d = (((a as i32 * w0 + rnd) >> sh) + o0).clamp(0, maxv) as u16;
                }
            } else {
                for (d, &a) in d.iter_mut().zip(one) {
                    *d = (a as i32 * w0 + o0).clamp(0, maxv) as u16;
                }
            }
        } else {
            let rnd = 1 << (shift1 - 1);
            for (d, &a) in d.iter_mut().zip(one) {
                *d = ((a as i32 + rnd) >> shift1).clamp(0, maxv) as u16;
            }
        }
    }
}

/// Horizontal filter of `rows` rows of `s` (rows `ss` apart) into `out` (rows `bw` apart), from column `xs` on:
/// output sample x is the sum of `c[k] * s[x + k]`, shifted right by `shift`.
#[inline(always)]
fn filter_h<const T: usize>(s: &[u16], ss: usize, c: &[i8; T], bw: usize, rows: usize, shift: i32, out: &mut [i16], xs: usize) {
    for y in 0..rows {
        let row = &s[y * ss..y * ss + bw + T - 1];
        let mut acc = [0i32; 64];
        let acc = &mut acc[..bw - xs];
        for k in 0..T {
            let ck = c[k] as i32;
            for (a, &v) in acc.iter_mut().zip(&row[k + xs..k + bw]) {
                *a += ck * v as i32;
            }
        }
        for (o, &a) in out[y * bw + xs..y * bw + bw].iter_mut().zip(acc.iter()) {
            *o = (a >> shift) as i16;
        }
    }
}

/// Vertical filter from column `xs` on: output (x, y) is the sum of `c[k] * s[(y + k) * ss + x]`, shifted
/// right by `shift`.
#[inline(always)]
fn filter_v<const T: usize, S: Copy + Into<i32>>(s: &[S], ss: usize, c: &[i8; T], bw: usize, bh: usize, shift: i32, out: &mut [i16], xs: usize) {
    for y in 0..bh {
        let mut acc = [0i32; 64];
        let acc = &mut acc[..bw - xs];
        for k in 0..T {
            let ck = c[k] as i32;
            for (a, &v) in acc.iter_mut().zip(&s[(y + k) * ss + xs..(y + k) * ss + bw]) {
                *a += ck * v.into();
            }
        }
        for (o, &a) in out[y * bw + xs..y * bw + bw].iter_mut().zip(acc.iter()) {
            *o = (a >> shift) as i16;
        }
    }
}

/// The horizontal filter, by AVX2 where `SIMD` says the caller runs with it.
#[inline(always)]
fn h_pass<const T: usize, const SIMD: bool>(s: &[u16], ss: usize, c: &[i8; T], bw: usize, rows: usize, shift: i32, out: &mut [i16]) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: SIMD is only set by the AVX2 entry points, which run after detecting AVX2
    let xs = if SIMD { unsafe { x86::filter_h::<T>(s, ss, c, bw, rows, shift, out) } } else { 0 };
    #[cfg(not(target_arch = "x86_64"))]
    let xs = 0;
    if xs < bw {
        filter_h::<T>(s, ss, c, bw, rows, shift, out, xs);
    }
}

/// The vertical filter, by AVX2 where `SIMD` says the caller runs with it.
#[inline(always)]
fn v_pass<const T: usize, const SIMD: bool, S: Copy + Into<i32>>(s: &[S], ss: usize, c: &[i8; T], bw: usize, bh: usize, shift: i32, out: &mut [i16]) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: SIMD is only set by the AVX2 entry points, which run after detecting AVX2
    let xs = if SIMD { unsafe { x86::filter_v::<T, S>(s, ss, c, bw, bh, shift, out) } } else { 0 };
    #[cfg(not(target_arch = "x86_64"))]
    let xs = 0;
    if xs < bw {
        filter_v::<T, S>(s, ss, c, bw, bh, shift, out, xs);
    }
}

/// Separable interpolation shared by luma (8 taps) and chroma (4 taps): 14-bit intermediate samples for
/// a `bw` x `bh` block whose integer position is (`ix`, `iy`). A reference block that reaches past the
/// picture edge is first copied, with its filter margins, into `pad` (clamping at the edges).
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn interpolate_impl<const T: usize, const SIMD: bool>(src: &[u16], pw: usize, ph: usize, ix: i32, iy: i32, cx: Option<&[i8; T]>, cy: Option<&[i8; T]>, bw: usize, bh: usize, bd: u32, out: &mut [i16], pad: &mut [u16], tmp: &mut [i16]) {
    let before = (T / 2 - 1) as i32;
    let (pwid, phgt) = (bw + T - 1, bh + T - 1);
    let (x0, y0) = (ix - before, iy - before);
    let inside = x0 >= 0 && y0 >= 0 && x0 as usize + pwid <= pw && y0 as usize + phgt <= ph;
    let (s, ss): (&[u16], usize) = if inside {
        (&src[y0 as usize * pw + x0 as usize..], pw)
    } else {
        for r in 0..phgt {
            let sy = (y0 + r as i32).clamp(0, ph as i32 - 1) as usize;
            let row = &src[sy * pw..sy * pw + pw];
            for (c, d) in pad[r * pwid..r * pwid + pwid].iter_mut().enumerate() {
                *d = row[(x0 + c as i32).clamp(0, pw as i32 - 1) as usize];
            }
        }
        (&pad[..pwid * phgt], pwid)
    };
    let b = before as usize;
    let shift1 = (bd as i32 - 8).min(4);
    let shift3 = 14 - bd as i32;
    match (cx, cy) {
        (None, None) => {
            for y in 0..bh {
                let row = &s[(y + b) * ss + b..(y + b) * ss + b + bw];
                for (o, &v) in out[y * bw..y * bw + bw].iter_mut().zip(row) {
                    *o = ((v as i32) << shift3) as i16;
                }
            }
        }
        (Some(cx), None) => h_pass::<T, SIMD>(&s[b * ss..], ss, cx, bw, bh, shift1, out),
        (None, Some(cy)) => v_pass::<T, SIMD, u16>(&s[b..], ss, cy, bw, bh, shift1, out),
        (Some(cx), Some(cy)) => {
            h_pass::<T, SIMD>(s, ss, cx, bw, phgt, shift1, tmp);
            v_pass::<T, SIMD, i16>(tmp, bw, cy, bw, bh, 6, out);
        }
    }
}

macro_rules! interpolate_fn {
    ($name:ident, $avx2:ident, $t:literal) => {
        /// `interpolate_impl` for this tap count, compiled for AVX2 when the processor has it.
        #[allow(clippy::too_many_arguments)]
        fn $name(src: &[u16], pw: usize, ph: usize, ix: i32, iy: i32, cx: Option<&[i8; $t]>, cy: Option<&[i8; $t]>, bw: usize, bh: usize, bd: u32, out: &mut [i16], pad: &mut [u16], tmp: &mut [i16]) {
            #[cfg(target_arch = "x86_64")]
            if is_x86_feature_detected!("avx2") {
                // SAFETY: AVX2 was detected just above.
                return unsafe { $avx2(src, pw, ph, ix, iy, cx, cy, bw, bh, bd, out, pad, tmp) };
            }
            interpolate_impl::<$t, false>(src, pw, ph, ix, iy, cx, cy, bw, bh, bd, out, pad, tmp)
        }

        /// SAFETY: callers check that AVX2 is available.
        #[cfg(target_arch = "x86_64")]
        #[target_feature(enable = "avx2")]
        #[allow(clippy::too_many_arguments)]
        unsafe fn $avx2(src: &[u16], pw: usize, ph: usize, ix: i32, iy: i32, cx: Option<&[i8; $t]>, cy: Option<&[i8; $t]>, bw: usize, bh: usize, bd: u32, out: &mut [i16], pad: &mut [u16], tmp: &mut [i16]) {
            interpolate_impl::<$t, true>(src, pw, ph, ix, iy, cx, cy, bw, bh, bd, out, pad, tmp)
        }
    };
}

interpolate_fn!(interpolate8, interpolate8_avx2, 8);
interpolate_fn!(interpolate4, interpolate4_avx2, 4);

/// Luma sample interpolation (8.5.3.3.3.1).
#[allow(clippy::too_many_arguments)]
fn luma_mc_with(src: &[u16], pw: usize, ph: usize, bx: i32, by: i32, mv: [i16; 2], bw: usize, bh: usize, bd: u32, out: &mut [i16], pad: &mut [u16], tmp: &mut [i16]) {
    let (fx, fy) = ((mv[0] & 3) as usize, (mv[1] & 3) as usize);
    let (ix, iy) = (bx + (mv[0] >> 2) as i32, by + (mv[1] >> 2) as i32);
    interpolate8(src, pw, ph, ix, iy, if fx != 0 { Some(&QPEL_FILTERS[fx]) } else { None }, if fy != 0 { Some(&QPEL_FILTERS[fy]) } else { None }, bw, bh, bd, out, pad, tmp);
}

/// Chroma sample interpolation (8.5.3.3.3.2). The luma vector is in quarter samples, so it is in
/// eighths of a chroma sample along a subsampled axis and in quarters (taken as even eighths) along a full one.
#[allow(clippy::too_many_arguments)]
fn chroma_mc_with(src: &[u16], pw: usize, ph: usize, bx: i32, by: i32, mv: [i16; 2], (sx, sy): (usize, usize), bw: usize, bh: usize, bd: u32, out: &mut [i16], pad: &mut [u16], tmp: &mut [i16]) {
    let split = |v: i16, s: usize| -> (i32, usize) {
        if s == 1 {
            ((v >> 3) as i32, (v & 7) as usize)
        } else {
            ((v >> 2) as i32, ((v & 3) << 1) as usize)
        }
    };
    let ((dx, fx), (dy, fy)) = (split(mv[0], sx), split(mv[1], sy));
    let (ix, iy) = (bx + dx, by + dy);
    interpolate4(src, pw, ph, ix, iy, if fx != 0 { Some(&EPEL_FILTERS[fx]) } else { None }, if fy != 0 { Some(&EPEL_FILTERS[fy]) } else { None }, bw, bh, bd, out, pad, tmp);
}

/// Luma sample interpolation for callers without scratch space (the encoder).
#[allow(clippy::too_many_arguments)]
pub(super) fn luma_mc(src: &[u16], pw: usize, ph: usize, bx: i32, by: i32, mv: [i16; 2], bw: usize, bh: usize, bd: u32, out: &mut [i16]) {
    let mut s = McScratch::new();
    luma_mc_with(src, pw, ph, bx, by, mv, bw, bh, bd, out, &mut s.pad, &mut s.tmp);
}

/// Chroma sample interpolation for callers without scratch space (the encoder).
#[allow(clippy::too_many_arguments)]
pub(super) fn chroma_mc(src: &[u16], pw: usize, ph: usize, bx: i32, by: i32, mv: [i16; 2], sub: (usize, usize), bw: usize, bh: usize, bd: u32, out: &mut [i16]) {
    let mut s = McScratch::new();
    chroma_mc_with(src, pw, ph, bx, by, mv, sub, bw, bh, bd, out, &mut s.pad, &mut s.tmp);
}

/// AVX2 interpolation filters. Each handles the columns in groups of eight or sixteen and returns how
/// many columns it did; the scalar filters finish the rest.
#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    /// The taps `c[2p]` and `c[2p + 1]` as the pair of 16-bit lanes that `madd` multiplies with two interleaved samples.
    #[inline(always)]
    fn pair<const T: usize>(c: &[i8; T], p: usize) -> i32 {
        (c[2 * p] as i16 as u16 as i32) | ((c[2 * p + 1] as i16 as i32) << 16)
    }

    /// `out[y * bw + x]` is the sum over k of `c[k] * s[y * ss + x + k]`, shifted right by `shift`, for
    /// `rows` rows and the first `bw & !7` columns. Samples must be below 2^15.
    ///
    /// SAFETY: AVX2 must be available; `s` must hold `bw + T - 1` samples in each of `rows` rows `ss` apart
    /// and `out` `bw` samples per row (checked by the slicing below).
    #[target_feature(enable = "avx2")]
    pub unsafe fn filter_h<const T: usize>(s: &[u16], ss: usize, c: &[i8; T], bw: usize, rows: usize, shift: i32, out: &mut [i16]) -> usize {
        let done = bw & !7;
        let sh = _mm_cvtsi32_si128(shift);
        let cp: [__m256i; 4] = std::array::from_fn(|p| if p < T / 2 { _mm256_set1_epi32(pair(c, p)) } else { _mm256_setzero_si256() });
        for y in 0..rows {
            let row = &s[y * ss..y * ss + bw + T - 1];
            let o = &mut out[y * bw..y * bw + bw];
            let mut x = 0;
            while x + 16 <= done {
                let (mut lo, mut hi) = (_mm256_setzero_si256(), _mm256_setzero_si256());
                for p in 0..T / 2 {
                    // SAFETY: x + 16 <= bw, so these reads end at or before x + 2p + 1 + 16 <= bw + T - 1
                    let a = _mm256_loadu_si256(row.as_ptr().add(x + 2 * p) as *const __m256i);
                    let b = _mm256_loadu_si256(row.as_ptr().add(x + 2 * p + 1) as *const __m256i);
                    lo = _mm256_add_epi32(lo, _mm256_madd_epi16(_mm256_unpacklo_epi16(a, b), cp[p]));
                    hi = _mm256_add_epi32(hi, _mm256_madd_epi16(_mm256_unpackhi_epi16(a, b), cp[p]));
                }
                let r = _mm256_packs_epi32(_mm256_sra_epi32(lo, sh), _mm256_sra_epi32(hi, sh));
                _mm256_storeu_si256(o.as_mut_ptr().add(x) as *mut __m256i, r);
                x += 16;
            }
            if x < done {
                let (mut lo, mut hi) = (_mm_setzero_si128(), _mm_setzero_si128());
                for p in 0..T / 2 {
                    let cq = _mm256_castsi256_si128(cp[p]);
                    // SAFETY: x + 8 <= bw, so these reads end at or before x + 2p + 1 + 8 <= bw + T - 1
                    let a = _mm_loadu_si128(row.as_ptr().add(x + 2 * p) as *const __m128i);
                    let b = _mm_loadu_si128(row.as_ptr().add(x + 2 * p + 1) as *const __m128i);
                    lo = _mm_add_epi32(lo, _mm_madd_epi16(_mm_unpacklo_epi16(a, b), cq));
                    hi = _mm_add_epi32(hi, _mm_madd_epi16(_mm_unpackhi_epi16(a, b), cq));
                }
                let r = _mm_packs_epi32(_mm_sra_epi32(lo, sh), _mm_sra_epi32(hi, sh));
                _mm_storeu_si128(o.as_mut_ptr().add(x) as *mut __m128i, r);
            }
        }
        done
    }

    /// `out[y * bw + x]` is the sum over k of `c[k] * s[(y + k) * ss + x]`, shifted right by `shift`, for
    /// `bh` rows and the first `bw & !7` columns. `s` is read as 16-bit samples below 2^15 (`u16` or `i16`).
    ///
    /// SAFETY: AVX2 must be available; `s` must hold `bw` samples in each of the rows 0 to `bh + T - 2`,
    /// `ss` apart, and `out` `bw` samples per row (checked by the assertions below).
    #[target_feature(enable = "avx2")]
    pub unsafe fn filter_v<const T: usize, S: Copy>(s: &[S], ss: usize, c: &[i8; T], bw: usize, bh: usize, shift: i32, out: &mut [i16]) -> usize {
        assert_eq!(std::mem::size_of::<S>(), 2);
        let done = bw & !7;
        let sh = _mm_cvtsi32_si128(shift);
        let cp: [__m256i; 4] = std::array::from_fn(|p| if p < T / 2 { _mm256_set1_epi32(pair(c, p)) } else { _mm256_setzero_si256() });
        // every read below is inside this many samples of `s`, and every write inside `out`
        assert!(bh == 0 || s.len() >= (bh + T - 2) * ss + bw);
        assert!(out.len() >= bw * bh);
        let (sp, op) = (s.as_ptr() as *const i16, out.as_mut_ptr());
        for y in 0..bh {
            let mut x = 0;
            while x + 16 <= done {
                let (mut lo, mut hi) = (_mm256_setzero_si256(), _mm256_setzero_si256());
                for p in 0..T / 2 {
                    // SAFETY: rows y + 2p and y + 2p + 1 are at most row bh + T - 2, and x + 16 <= bw
                    let a = _mm256_loadu_si256(sp.add((y + 2 * p) * ss + x) as *const __m256i);
                    let b = _mm256_loadu_si256(sp.add((y + 2 * p + 1) * ss + x) as *const __m256i);
                    lo = _mm256_add_epi32(lo, _mm256_madd_epi16(_mm256_unpacklo_epi16(a, b), cp[p]));
                    hi = _mm256_add_epi32(hi, _mm256_madd_epi16(_mm256_unpackhi_epi16(a, b), cp[p]));
                }
                let r = _mm256_packs_epi32(_mm256_sra_epi32(lo, sh), _mm256_sra_epi32(hi, sh));
                _mm256_storeu_si256(op.add(y * bw + x) as *mut __m256i, r);
                x += 16;
            }
            if x < done {
                let (mut lo, mut hi) = (_mm_setzero_si128(), _mm_setzero_si128());
                for p in 0..T / 2 {
                    let cq = _mm256_castsi256_si128(cp[p]);
                    // SAFETY: as above, with x + 8 <= bw
                    let a = _mm_loadu_si128(sp.add((y + 2 * p) * ss + x) as *const __m128i);
                    let b = _mm_loadu_si128(sp.add((y + 2 * p + 1) * ss + x) as *const __m128i);
                    lo = _mm_add_epi32(lo, _mm_madd_epi16(_mm_unpacklo_epi16(a, b), cq));
                    hi = _mm_add_epi32(hi, _mm_madd_epi16(_mm_unpackhi_epi16(a, b), cq));
                }
                let r = _mm_packs_epi32(_mm_sra_epi32(lo, sh), _mm_sra_epi32(hi, sh));
                _mm_storeu_si128(op.add(y * bw + x) as *mut __m128i, r);
            }
        }
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: &mut u32) -> u32 {
        *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        *seed >> 8
    }

    /// The dispatching interpolation (AVX2 where the processor has it) against the portable code.
    #[test]
    fn interpolation_matches_the_scalar_code() {
        let mut seed = 99u32;
        let (pw, ph) = (80usize, 72usize);
        for bd in [8u32, 10, 12] {
            let src: Vec<u16> = (0..pw * ph).map(|_| (rng(&mut seed) % (1 << bd)) as u16).collect();
            for _ in 0..600 {
                let bw = [2usize, 4, 6, 8, 12, 16, 24, 32, 48, 64][rng(&mut seed) as usize % 10];
                let bh = [2usize, 4, 8, 12, 16, 32, 64][rng(&mut seed) as usize % 7];
                // positions near and beyond the edges as well as inside
                let (ix, iy) = (rng(&mut seed) as i32 % 110 - 20, rng(&mut seed) as i32 % 100 - 20);
                let (fx, fy) = (rng(&mut seed) as usize % 4, rng(&mut seed) as usize % 4);
                let (ex, ey) = (rng(&mut seed) as usize % 8, rng(&mut seed) as usize % 8);
                let (mut pad, mut tmp) = (vec![0u16; 71 * 71], vec![0i16; 71 * 64]);
                let (mut a, mut b) = (vec![0i16; bw * bh], vec![0i16; bw * bh]);
                let q = |f: usize| if f != 0 { Some(&QPEL_FILTERS[f]) } else { None };
                interpolate8(&src, pw, ph, ix, iy, q(fx), q(fy), bw, bh, bd, &mut a, &mut pad, &mut tmp);
                interpolate_impl::<8, false>(&src, pw, ph, ix, iy, q(fx), q(fy), bw, bh, bd, &mut b, &mut pad, &mut tmp);
                assert_eq!(a, b, "luma {bw}x{bh} at {ix},{iy} frac {fx},{fy}, {bd} bits");
                let e = |f: usize| if f != 0 { Some(&EPEL_FILTERS[f]) } else { None };
                interpolate4(&src, pw, ph, ix, iy, e(ex), e(ey), bw, bh, bd, &mut a, &mut pad, &mut tmp);
                interpolate_impl::<4, false>(&src, pw, ph, ix, iy, e(ex), e(ey), bw, bh, bd, &mut b, &mut pad, &mut tmp);
                assert_eq!(a, b, "chroma {bw}x{bh} at {ix},{iy} frac {ex},{ey}, {bd} bits");
            }
        }
    }
}
