//! Inter prediction: prediction unit syntax, motion vector derivation (merge, AMVP and temporal
//! candidates) and motion compensation with weighted prediction (ITU-T H.265 8.5.3).

use super::frame::*;
use super::ps::SliceType;
use super::slice::{bad, Dec, PartMode};
use super::tables::*;
use super::RefPic;
use ironav_core::Result;

const COMB: [(usize, usize); 12] = [(0, 1), (1, 0), (0, 2), (2, 0), (1, 2), (2, 1), (0, 3), (3, 0), (1, 3), (3, 1), (2, 3), (3, 2)];

fn same_motion(a: &MvField, b: &MvField) -> bool {
    a.pred & 3 == b.pred & 3 && (!a.uses(0) || (a.mv[0] == b.mv[0] && a.ref_idx[0] == b.ref_idx[0])) && (!a.uses(1) || (a.mv[1] == b.mv[1] && a.ref_idx[1] == b.ref_idx[1]))
}

fn scale_mv(mv: [i16; 2], td: i32, tb: i32) -> [i16; 2] {
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

    fn col_mv(&self, col: &Frame, x: usize, y: usize, ref_idx: usize, list: usize) -> Option<[i16; 2]> {
        let m = *col.mv.get((y >> 2) * col.w4 + (x >> 2))?;
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
        let ncomp = if sps.chroma_format_idc == 0 { 1 } else { 3 };
        let weighted = match self.hdr.slice_type {
            SliceType::P => self.pps.weighted_pred,
            _ => self.pps.weighted_bipred,
        };
        for c in 0..ncomp {
            let (sx, sy) = if c == 0 { (0, 0) } else { sps.chroma_shift() };
            let (bx, by, bw, bh) = (x >> sx, y >> sy, w >> sx, h >> sy);
            let bd = if c == 0 { self.bd_y } else { self.bd_c };
            let mut pred: [Vec<i16>; 2] = [Vec::new(), Vec::new()];
            for l in 0..2 {
                if !m.uses(l) {
                    continue;
                }
                let r = self.refs[l].get(m.ref_idx[l] as usize).ok_or_else(|| bad("reference picture index is out of range"))?;
                let f = &r.frame;
                let (pw, ph) = if c == 0 { (f.width, f.height) } else { (f.cwidth, f.cheight) };
                let mut out = vec![0i16; bw * bh];
                if c == 0 {
                    luma_mc(&f.planes[0], pw, ph, bx as i32, by as i32, m.mv[l], bw, bh, bd, &mut out);
                } else {
                    chroma_mc(&f.planes[c], pw, ph, bx as i32, by as i32, m.mv[l], (sx, sy), bw, bh, bd, &mut out);
                }
                pred[l] = out;
            }
            let cw = if c == 0 { self.pic.frame.width } else { self.pic.frame.cwidth };
            let maxv = (1i32 << bd) - 1;
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
            let bi = m.uses(0) && m.uses(1);
            let dst = &mut self.pic.frame.planes[c];
            for yy in 0..bh {
                for xx in 0..bw {
                    let i = yy * bw + xx;
                    let v = if bi {
                        let (a, b) = (pred[0][i] as i32, pred[1][i] as i32);
                        if wt.is_some() {
                            let ((w0, o0), (w1, o1)) = (params(0), params(1));
                            (a * w0 + b * w1 + ((o0 + o1 + 1) << log2wd)) >> (log2wd + 1)
                        } else {
                            (a + b + (1 << (14 - bd as i32))) >> (15 - bd as i32)
                        }
                    } else {
                        let l = if m.uses(0) { 0 } else { 1 };
                        let a = pred[l][i] as i32;
                        if wt.is_some() {
                            let (w0, o0) = params(l);
                            if log2wd >= 1 {
                                ((a * w0 + (1 << (log2wd - 1))) >> log2wd) + o0
                            } else {
                                a * w0 + o0
                            }
                        } else if shift1 > 0 {
                            (a + (1 << (shift1 - 1))) >> shift1
                        } else {
                            a
                        }
                    };
                    dst[(by + yy) * cw + bx + xx] = v.clamp(0, maxv) as u16;
                }
            }
        }
        Ok(())
    }
}

/// Separable interpolation shared by luma (8 taps) and chroma (4 taps): 14-bit intermediate samples for
/// a `bw` x `bh` block whose integer position is (`ix`, `iy`). The reference block with its filter margins
/// is copied once (clamping at the picture edges), so the filters index without bounds logic.
#[allow(clippy::too_many_arguments)]
fn interpolate<const T: usize>(src: &[u16], pw: usize, ph: usize, ix: i32, iy: i32, cx: Option<&[i8; T]>, cy: Option<&[i8; T]>, bw: usize, bh: usize, bd: u32, out: &mut [i16]) {
    let before = (T / 2 - 1) as i32;
    let (pwid, phgt) = (bw + T - 1, bh + T - 1);
    let mut pad = [0u16; 71 * 71];
    let (x0, y0) = (ix - before, iy - before);
    let inside_x = x0 >= 0 && x0 as usize + pwid <= pw;
    for r in 0..phgt {
        let sy = (y0 + r as i32).clamp(0, ph as i32 - 1) as usize;
        let row = &src[sy * pw..sy * pw + pw];
        let dst = &mut pad[r * pwid..r * pwid + pwid];
        if inside_x {
            dst.copy_from_slice(&row[x0 as usize..x0 as usize + pwid]);
        } else {
            for (c, d) in dst.iter_mut().enumerate() {
                *d = row[(x0 + c as i32).clamp(0, pw as i32 - 1) as usize];
            }
        }
    }
    let b = before as usize;
    let shift1 = (bd as i32 - 8).min(4);
    let shift3 = 14 - bd as i32;
    match (cx, cy) {
        (None, None) => {
            for y in 0..bh {
                for x in 0..bw {
                    out[y * bw + x] = ((pad[(y + b) * pwid + x + b] as i32) << shift3) as i16;
                }
            }
        }
        (Some(cx), None) => {
            for y in 0..bh {
                let row = &pad[(y + b) * pwid..(y + b + 1) * pwid];
                for x in 0..bw {
                    let mut s = 0;
                    for k in 0..T {
                        s += cx[k] as i32 * row[x + k] as i32;
                    }
                    out[y * bw + x] = (s >> shift1) as i16;
                }
            }
        }
        (None, Some(cy)) => {
            for y in 0..bh {
                for x in 0..bw {
                    let mut s = 0;
                    for k in 0..T {
                        s += cy[k] as i32 * pad[(y + k) * pwid + x + b] as i32;
                    }
                    out[y * bw + x] = (s >> shift1) as i16;
                }
            }
        }
        (Some(cx), Some(cy)) => {
            let mut tmp = [0i32; 71 * 64];
            for y in 0..phgt {
                let row = &pad[y * pwid..(y + 1) * pwid];
                for x in 0..bw {
                    let mut s = 0;
                    for k in 0..T {
                        s += cx[k] as i32 * row[x + k] as i32;
                    }
                    tmp[y * bw + x] = s >> shift1;
                }
            }
            for y in 0..bh {
                for x in 0..bw {
                    let mut s = 0;
                    for k in 0..T {
                        s += cy[k] as i32 * tmp[(y + k) * bw + x];
                    }
                    out[y * bw + x] = (s >> 6) as i16;
                }
            }
        }
    }
}

/// Luma sample interpolation (8.5.3.3.3.1).
#[allow(clippy::too_many_arguments)]
fn luma_mc(src: &[u16], pw: usize, ph: usize, bx: i32, by: i32, mv: [i16; 2], bw: usize, bh: usize, bd: u32, out: &mut [i16]) {
    let (fx, fy) = ((mv[0] & 3) as usize, (mv[1] & 3) as usize);
    let (ix, iy) = (bx + (mv[0] >> 2) as i32, by + (mv[1] >> 2) as i32);
    interpolate::<8>(src, pw, ph, ix, iy, if fx != 0 { Some(&QPEL_FILTERS[fx]) } else { None }, if fy != 0 { Some(&QPEL_FILTERS[fy]) } else { None }, bw, bh, bd, out);
}

/// Chroma sample interpolation (8.5.3.3.3.2). The luma vector is in quarter samples, so it is in
/// eighths of a chroma sample along a subsampled axis and in quarters (taken as even eighths) along a full one.
#[allow(clippy::too_many_arguments)]
fn chroma_mc(src: &[u16], pw: usize, ph: usize, bx: i32, by: i32, mv: [i16; 2], (sx, sy): (usize, usize), bw: usize, bh: usize, bd: u32, out: &mut [i16]) {
    let split = |v: i16, s: usize| -> (i32, usize) {
        if s == 1 {
            ((v >> 3) as i32, (v & 7) as usize)
        } else {
            ((v >> 2) as i32, ((v & 3) << 1) as usize)
        }
    };
    let ((dx, fx), (dy, fy)) = (split(mv[0], sx), split(mv[1], sy));
    let (ix, iy) = (bx + dx, by + dy);
    interpolate::<4>(src, pw, ph, ix, iy, if fx != 0 { Some(&EPEL_FILTERS[fx]) } else { None }, if fy != 0 { Some(&EPEL_FILTERS[fy]) } else { None }, bw, bh, bd, out);
}
