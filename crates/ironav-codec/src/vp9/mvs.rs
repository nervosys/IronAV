//! Motion vector prediction and decoding.

use super::dec::*;
use super::tables::*;

impl Dec {
    fn clamp_mv(&self, m: Mv) -> Mv {
        Mv {
            x: (m.x as i32).clamp(self.min_mv.x, self.max_mv.x) as i16,
            y: (m.y as i32).clamp(self.min_mv.y, self.max_mv.y) as i16,
        }
    }

    /// Candidate list search: the `idx`'th distinct candidate (0 = nearest, 1 = near) for `ref_`.
    /// `sb` is the sub-block of an 8x8 block being predicted, or -1.
    // The last expansion of `ret_mv!` stores a candidate that nothing reads afterwards.
    #[allow(unused_assignments)]
    fn find_ref_mvs(&self, ref_: i8, z: usize, idx: bool, sb: i32) -> Mv {
        let (row, col) = (self.row, self.col);
        let row7 = row & 7;
        let p = &MV_REF_BLK_OFF[self.b.bs];
        let mut mem: Option<Mv> = None;
        let mut mem_sub8x8: Option<Mv> = None;
        let sbc = self.sb_cols;

        macro_rules! direct {
            ($mv:expr) => {{
                let m: Mv = $mv;
                if !idx {
                    return m;
                } else if mem.is_none() {
                    mem = Some(m);
                } else if Some(m) != mem {
                    return m;
                }
            }};
        }
        macro_rules! ret_mv {
            ($mv:expr) => {{
                let mv: Mv = $mv;
                if sb > 0 {
                    match mem_sub8x8 {
                        None => {
                            let t = self.clamp_mv(mv);
                            if Some(t) != mem {
                                return t;
                            }
                            mem_sub8x8 = Some(mv);
                        }
                        Some(ms) if ms != mv => {
                            let t = self.clamp_mv(mv);
                            return if Some(t) != mem { t } else { Mv::default() };
                        }
                        _ => {}
                    }
                } else if !idx {
                    return self.clamp_mv(mv);
                } else if mem.is_none() {
                    mem = Some(mv);
                } else if Some(mv) != mem {
                    return self.clamp_mv(mv);
                }
            }};
        }
        macro_rules! scaled {
            ($mv:expr, $flip:expr) => {{
                let m: Mv = $mv;
                if $flip {
                    ret_mv!(Mv { x: m.x.wrapping_neg(), y: m.y.wrapping_neg() });
                } else {
                    ret_mv!(m);
                }
            }};
        }

        let mut i;
        if sb >= 0 {
            let sbu = sb as usize;
            if sb == 2 || sb == 1 {
                direct!(self.b.mv[0][z]);
            } else if sb == 3 {
                direct!(self.b.mv[2][z]);
                direct!(self.b.mv[1][z]);
                direct!(self.b.mv[0][z]);
            }
            if row > 0 {
                let m = &self.cur.mv[(row - 1) * sbc * 8 + col];
                if m.r[0] == ref_ {
                    ret_mv!(self.above_mv[2 * col + (sbu & 1)][0]);
                } else if m.r[1] == ref_ {
                    ret_mv!(self.above_mv[2 * col + (sbu & 1)][1]);
                }
            }
            if col > self.tile_col_start {
                let m = &self.cur.mv[row * sbc * 8 + col - 1];
                if m.r[0] == ref_ {
                    ret_mv!(self.left_mv[2 * row7 + (sbu >> 1)][0]);
                } else if m.r[1] == ref_ {
                    ret_mv!(self.left_mv[2 * row7 + (sbu >> 1)][1]);
                }
            }
            i = 2;
        } else {
            i = 0;
        }

        while i < 8 {
            let c = p[i][0] as i32 + col as i32;
            let r = p[i][1] as i32 + row as i32;
            if c >= self.tile_col_start as i32 && c < self.cols as i32 && r >= 0 && r < self.rows as i32 {
                let m = &self.cur.mv[r as usize * sbc * 8 + c as usize];
                if m.r[0] == ref_ {
                    ret_mv!(m.mv[0]);
                } else if m.r[1] == ref_ {
                    ret_mv!(m.mv[1]);
                }
            }
            i += 1;
        }

        let prev = if self.h.use_last_frame_mvs { self.prev.as_ref() } else { None };
        if let Some(pv) = prev {
            let m = &pv.mv[row * sbc * 8 + col];
            if m.r[0] == ref_ {
                ret_mv!(m.mv[0]);
            } else if m.r[1] == ref_ {
                ret_mv!(m.mv[1]);
            }
        }

        let sbias = &self.h.signbias;
        for i in 0..8 {
            let c = p[i][0] as i32 + col as i32;
            let r = p[i][1] as i32 + row as i32;
            if c >= self.tile_col_start as i32 && c < self.cols as i32 && r >= 0 && r < self.rows as i32 {
                let m = &self.cur.mv[r as usize * sbc * 8 + c as usize];
                if m.r[0] != ref_ && m.r[0] >= 0 {
                    scaled!(m.mv[0], sbias[m.r[0] as usize] != sbias[ref_ as usize]);
                }
                if m.r[1] != ref_ && m.r[1] >= 0 && m.mv[0] != m.mv[1] {
                    scaled!(m.mv[1], sbias[m.r[1] as usize] != sbias[ref_ as usize]);
                }
            }
        }

        if let Some(pv) = prev {
            let m = &pv.mv[row * sbc * 8 + col];
            if m.r[0] != ref_ && m.r[0] >= 0 {
                scaled!(m.mv[0], sbias[m.r[0] as usize] != sbias[ref_ as usize]);
            }
            if m.r[1] != ref_ && m.r[1] >= 0 && m.mv[0] != m.mv[1] {
                scaled!(m.mv[1], sbias[m.r[1] as usize] != sbias[ref_ as usize]);
            }
        }

        self.clamp_mv(Mv::default())
    }

    fn read_mv_component(&mut self, idx: usize, hp: bool) -> i32 {
        let sign = self.c.get(self.p.mv_comp[idx].sign);
        let class = self.c.tree(&MV_CLASS_TREE, &self.p.mv_comp[idx].classes);
        self.counts.mv_comp[idx].sign[sign as usize] += 1;
        self.counts.mv_comp[idx].classes[class] += 1;
        let mut n: i32;
        if class != 0 {
            n = 0;
            for m in 0..class {
                let bit = self.c.get(self.p.mv_comp[idx].bits[m]);
                n |= (bit as i32) << m;
                self.counts.mv_comp[idx].bits[m][bit as usize] += 1;
            }
            n <<= 3;
            let fp = self.c.tree(&MV_FP_TREE, &self.p.mv_comp[idx].fp);
            n |= (fp as i32) << 1;
            self.counts.mv_comp[idx].fp[fp] += 1;
            if hp {
                let bit = self.c.get(self.p.mv_comp[idx].hp);
                self.counts.mv_comp[idx].hp[bit as usize] += 1;
                n |= bit as i32;
            } else {
                n |= 1;
                self.counts.mv_comp[idx].hp[1] += 1;
            }
            n += 8 << class;
        } else {
            let c0 = self.c.get(self.p.mv_comp[idx].class0) as usize;
            self.counts.mv_comp[idx].class0[c0] += 1;
            let fp = self.c.tree(&MV_FP_TREE, &self.p.mv_comp[idx].class0_fp[c0]);
            self.counts.mv_comp[idx].class0_fp[c0][fp] += 1;
            n = ((c0 as i32) << 3) | ((fp as i32) << 1);
            if hp {
                let bit = self.c.get(self.p.mv_comp[idx].class0_hp);
                self.counts.mv_comp[idx].class0_hp[bit as usize] += 1;
                n |= bit as i32;
            } else {
                n |= 1;
                self.counts.mv_comp[idx].class0_hp[1] += 1;
            }
        }
        if sign { -(n + 1) } else { n + 1 }
    }

    /// The candidate motion of `mode` (NEARESTMV, NEARMV or NEWMV) for reference `k`, and whether high
    /// precision applies: the base that a coded NEWMV difference is added to.
    pub(super) fn base_mv(&self, mode: u8, k: usize, sb: i32) -> (Mv, bool) {
        let mut mv = self.find_ref_mvs(self.b.refs[k] as i8, k, mode == NEARMV, if mode == NEWMV { -1 } else { sb });
        let mut hp = false;
        if mode == NEWMV || sb == -1 {
            hp = self.h.highprecisionmvs && mv.x.abs() < 64 && mv.y.abs() < 64;
            if !hp {
                if mv.y & 1 != 0 {
                    mv.y += if mv.y < 0 { 1 } else { -1 };
                }
                if mv.x & 1 != 0 {
                    mv.x += if mv.x < 0 { 1 } else { -1 };
                }
            }
        }
        (mv, hp)
    }

    /// Motion vector(s) of a block (or sub-block `sb`, or -1 for the whole block) coded as `mode`.
    pub fn fill_mv(&mut self, mode: u8, sb: i32) -> [Mv; 2] {
        let mut out = [Mv::default(); 2];
        if mode == ZEROMV {
            return out;
        }
        let nref = if self.b.comp { 2 } else { 1 };
        for k in 0..nref {
            let (mut mv, hp) = self.base_mv(mode, k, sb);
            if mode == NEWMV {
                let j = self.c.tree(&MV_JOINT_TREE, &self.p.mv_joint);
                self.counts.mv_joint[j] += 1;
                if j >= 2 {
                    mv.y = mv.y.wrapping_add(self.read_mv_component(0, hp) as i16);
                }
                if j & 1 != 0 {
                    mv.x = mv.x.wrapping_add(self.read_mv_component(1, hp) as i16);
                }
            }
            out[k] = mv;
        }
        out
    }
}
