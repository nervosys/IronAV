//! Block level syntax: modes, references, coefficients, and the loop filter masks a block implies.

use super::dec::*;
use super::header::*;
use super::rac::Rac;
use super::tables::*;

const DECODE_ERR: &str = "corrupt VP9 block data";

/// Reads the tokens of one transform block. Returns the number of coefficients coded (the eob).
#[allow(clippy::too_many_arguments)]
fn coeffs_b(
    c: &mut Rac,
    coef: &mut [i32],
    n_coeffs: usize,
    is32: bool,
    bpp: u32,
    cnt: &mut [[[u32; 3]; 6]; 6],
    eob: &mut [[[u32; 2]; 6]; 6],
    p: &[[[u8; 11]; 6]; 6],
    mut nnz: usize,
    scan: &[i16],
    nb: &[[i16; 2]],
    bc: &[i16; 8],
    qmul: [i32; 2],
) -> usize {
    let mut i = 0usize;
    let mut band = 0usize;
    let mut band_left = bc[0] as i32;
    let mut tp = &p[0][nnz];
    let mut cache = [0u8; 1024];

    loop {
        let more = c.get(tp[0]);
        eob[band][nnz][more as usize] += 1;
        if !more {
            break;
        }
        loop {
            if c.get(tp[1]) {
                break;
            }
            cnt[band][nnz][0] += 1;
            band_left -= 1;
            if band_left == 0 {
                band += 1;
                band_left = bc[band] as i32;
            }
            cache[scan[i] as usize] = 0;
            nnz = (1 + cache[nb[i][0] as usize] as usize + cache[nb[i][1] as usize] as usize) >> 1;
            i += 1;
            if i == n_coeffs {
                return i;
            }
            tp = &p[band][nnz];
        }

        let rc = scan[i] as usize;
        let val: i32;
        if !c.get(tp[2]) {
            cnt[band][nnz][1] += 1;
            val = 1;
            cache[rc] = 1;
        } else {
            cnt[band][nnz][2] += 1;
            if !c.get(tp[3]) {
                if !c.get(tp[4]) {
                    val = 2;
                    cache[rc] = 2;
                } else {
                    val = 3 + c.get(tp[5]) as i32;
                    cache[rc] = 3;
                }
            } else if !c.get(tp[6]) {
                cache[rc] = 4;
                if !c.get(tp[7]) {
                    val = c.get(159) as i32 + 5;
                } else {
                    let mut v = ((c.get(165) as i32) << 1) + 7;
                    v += c.get(145) as i32;
                    val = v;
                }
            } else {
                cache[rc] = 5;
                if !c.get(tp[8]) {
                    if !c.get(tp[9]) {
                        let mut v = 11 + ((c.get(173) as i32) << 2);
                        v += (c.get(148) as i32) << 1;
                        v += c.get(140) as i32;
                        val = v;
                    } else {
                        let mut v = 19 + ((c.get(176) as i32) << 3);
                        v += (c.get(155) as i32) << 2;
                        v += (c.get(140) as i32) << 1;
                        v += c.get(135) as i32;
                        val = v;
                    }
                } else if !c.get(tp[10]) {
                    let mut v = ((c.get(180) as i32) << 4) + 35;
                    v += (c.get(157) as i32) << 3;
                    v += (c.get(141) as i32) << 2;
                    v += (c.get(134) as i32) << 1;
                    v += c.get(130) as i32;
                    val = v;
                } else {
                    let mut v = 67i32;
                    if bpp > 8 {
                        if bpp == 12 {
                            v += (c.get(255) as i32) << 17;
                            v += (c.get(255) as i32) << 16;
                        }
                        v += (c.get(255) as i32) << 15;
                        v += (c.get(255) as i32) << 14;
                    }
                    v += (c.get(254) as i32) << 13;
                    v += (c.get(254) as i32) << 12;
                    v += (c.get(254) as i32) << 11;
                    v += (c.get(252) as i32) << 10;
                    v += (c.get(249) as i32) << 9;
                    v += (c.get(243) as i32) << 8;
                    v += (c.get(230) as i32) << 7;
                    v += (c.get(196) as i32) << 6;
                    v += (c.get(177) as i32) << 5;
                    v += (c.get(153) as i32) << 4;
                    v += (c.get(140) as i32) << 3;
                    v += (c.get(133) as i32) << 2;
                    v += (c.get(130) as i32) << 1;
                    v += c.get(129) as i32;
                    val = v;
                }
            }
        }
        band_left -= 1;
        if band_left == 0 {
            band += 1;
            band_left = bc[band] as i32;
        }
        let signed = if c.bit() { -val } else { val };
        let q = qmul[(i != 0) as usize];
        let prod = (signed as u32).wrapping_mul(q as u32) as i32;
        coef[rc] = if is32 { prod / 2 } else { prod };
        nnz = (1 + cache[nb[i][0] as usize] as usize + cache[nb[i][1] as usize] as usize) >> 1;
        i += 1;
        if i >= n_coeffs {
            break;
        }
        tp = &p[band][nnz];
    }
    i
}

impl Dec {
    pub fn decode_mode(&mut self) {
        let (row, col) = (self.row, self.col);
        let row7 = row & 7;
        let bs = self.b.bs;
        let max_tx = MAX_TX_FOR_BL_BP[bs] as usize;
        let bw4 = BWH_TAB[1][bs][0] as usize;
        let w4 = bw4.min(self.cols - col);
        let bh4 = BWH_TAB[1][bs][1] as usize;
        let h4 = bh4.min(self.rows - row);
        let have_a = row > 0;
        let have_l = col > self.tile_col_start;
        let keyintra = self.keyintra();
        let fixcompref = self.h.fixcompref;

        // segment id
        let seg_en = self.h.seg.enabled;
        let seg_id: usize;
        if !seg_en {
            seg_id = 0;
        } else if keyintra {
            seg_id = if !self.h.seg.update_map { 0 } else { self.c.tree(&SEGMENTATION_TREE, &self.h.seg.prob) };
        } else if !self.h.seg.update_map
            || (self.h.seg.temporal && {
                let pc = self.h.seg.pred_prob[(self.above_segpred[col] + self.left_segpred[row7]) as usize];
                self.c.get(pc)
            })
        {
            let mut pred = 8u8;
            if !self.h.errorres {
                if let Some(m) = &self.segmap_ref {
                    for y in 0..h4 {
                        let base = (y + row) * 8 * self.sb_cols + col;
                        for x in 0..w4 {
                            pred = pred.min(m[base + x]);
                        }
                    }
                } else {
                    pred = 0;
                }
            } else {
                pred = 0;
            }
            seg_id = (pred as usize).min(7);
            splat(&mut self.above_segpred, col, w4, 1);
            splat(&mut self.left_segpred, row7, h4, 1);
        } else {
            seg_id = self.c.tree(&SEGMENTATION_TREE, &self.h.seg.prob);
            splat(&mut self.above_segpred, col, w4, 0);
            splat(&mut self.left_segpred, row7, h4, 0);
        }
        self.b.seg_id = seg_id;
        if seg_en && (self.h.seg.update_map || keyintra) {
            for y in 0..bh4 {
                let o = (row + y) * 8 * self.sb_cols + col;
                for x in 0..bw4 {
                    self.cur.segmap[o + x] = seg_id as u8;
                }
            }
        }

        let feat = self.h.seg.feat[seg_id];
        self.b.skip = seg_en && feat.skip_enabled;
        if !self.b.skip {
            let c = (self.left_skip[row7] + self.above_skip[col]) as usize;
            let bit = self.c.get(self.p.skip[c]);
            self.counts.skip[c][bit as usize] += 1;
            self.b.skip = bit;
        }

        if keyintra {
            self.b.intra = true;
        } else if seg_en && feat.ref_enabled {
            self.b.intra = feat.ref_val == 0;
        } else {
            let (ai, li) = (self.above_intra[col] as usize, self.left_intra[row7] as usize);
            let mut c;
            if have_a && have_l {
                c = ai + li;
                c += (c == 2) as usize;
            } else {
                c = if have_a { 2 * ai } else if have_l { 2 * li } else { 0 };
            }
            let bit = self.c.get(self.p.intra[c]);
            self.counts.intra[c][bit as usize] += 1;
            self.b.intra = !bit;
        }

        if (self.b.intra || !self.b.skip) && self.h.txfmmode == TX_SWITCHABLE {
            let (as_, ls) = (self.above_skip[col] != 0, self.left_skip[row7] != 0);
            let (at, lt) = (self.above_txfm[col] as usize, self.left_txfm[row7] as usize);
            let c: usize = if have_a {
                if have_l {
                    ((if as_ { max_tx } else { at }) + (if ls { max_tx } else { lt }) > max_tx) as usize
                } else if as_ {
                    1
                } else {
                    (at * 2 > max_tx) as usize
                }
            } else if have_l {
                if ls { 1 } else { (lt * 2 > max_tx) as usize }
            } else {
                1
            };
            match max_tx {
                3 => {
                    let mut t = self.c.get(self.p.tx32p[c][0]) as usize;
                    if t != 0 {
                        t += self.c.get(self.p.tx32p[c][1]) as usize;
                        if t == 2 {
                            t += self.c.get(self.p.tx32p[c][2]) as usize;
                        }
                    }
                    self.counts.tx32p[c][t] += 1;
                    self.b.tx = t;
                }
                2 => {
                    let mut t = self.c.get(self.p.tx16p[c][0]) as usize;
                    if t != 0 {
                        t += self.c.get(self.p.tx16p[c][1]) as usize;
                    }
                    self.counts.tx16p[c][t] += 1;
                    self.b.tx = t;
                }
                1 => {
                    let t = self.c.get(self.p.tx8p[c]) as usize;
                    self.counts.tx8p[c][t] += 1;
                    self.b.tx = t;
                }
                _ => self.b.tx = 0,
            }
        } else {
            self.b.tx = max_tx.min(self.h.txfmmode as usize);
        }

        let mut vref = 0usize;
        let mut filter_id = 0u8;
        if keyintra {
            self.b.comp = false;
            let a0 = col * 2;
            let l0 = row7 << 1;
            if bs > BS_8X8 {
                let m0 = self.c.tree(&INTRAMODE_TREE, &DEFAULT_KF_YMODE_PROBS[self.above_mode[a0] as usize][self.left_mode[l0] as usize]) as u8;
                self.b.mode[0] = m0;
                self.above_mode[a0] = m0;
                if bs != BS_8X4 {
                    let m1 = self.c.tree(&INTRAMODE_TREE, &DEFAULT_KF_YMODE_PROBS[self.above_mode[a0 + 1] as usize][m0 as usize]) as u8;
                    self.b.mode[1] = m1;
                    self.left_mode[l0] = m1;
                    self.above_mode[a0 + 1] = m1;
                } else {
                    self.left_mode[l0] = m0;
                    self.above_mode[a0 + 1] = m0;
                    self.b.mode[1] = m0;
                }
                if bs != BS_4X8 {
                    let m2 = self.c.tree(&INTRAMODE_TREE, &DEFAULT_KF_YMODE_PROBS[self.above_mode[a0] as usize][self.left_mode[l0 + 1] as usize]) as u8;
                    self.b.mode[2] = m2;
                    self.above_mode[a0] = m2;
                    if bs != BS_8X4 {
                        let m3 = self.c.tree(&INTRAMODE_TREE, &DEFAULT_KF_YMODE_PROBS[self.above_mode[a0 + 1] as usize][m2 as usize]) as u8;
                        self.b.mode[3] = m3;
                        self.left_mode[l0 + 1] = m3;
                        self.above_mode[a0 + 1] = m3;
                    } else {
                        self.left_mode[l0 + 1] = m2;
                        self.above_mode[a0 + 1] = m2;
                        self.b.mode[3] = m2;
                    }
                } else {
                    self.b.mode[2] = self.b.mode[0];
                    let m1 = self.b.mode[1];
                    self.left_mode[l0 + 1] = m1;
                    self.above_mode[a0 + 1] = m1;
                    self.b.mode[3] = m1;
                }
            } else {
                let m = self.c.tree(&INTRAMODE_TREE, &DEFAULT_KF_YMODE_PROBS[self.above_mode[a0] as usize][self.left_mode[l0] as usize]) as u8;
                self.b.mode = [m; 4];
                splat(&mut self.above_mode, a0, BWH_TAB[0][bs][0] as usize, m);
                splat_arr(&mut self.left_mode, l0, BWH_TAB[0][bs][1] as usize, m);
            }
            self.b.uvmode = self.c.tree(&INTRAMODE_TREE, &DEFAULT_KF_UVMODE_PROBS[self.b.mode[3] as usize]) as u8;
        } else if self.b.intra {
            self.b.comp = false;
            if bs > BS_8X8 {
                let m0 = self.c.tree(&INTRAMODE_TREE, &self.p.y_mode[0]);
                self.counts.y_mode[0][m0] += 1;
                self.b.mode[0] = m0 as u8;
                if bs != BS_8X4 {
                    let m1 = self.c.tree(&INTRAMODE_TREE, &self.p.y_mode[0]);
                    self.counts.y_mode[0][m1] += 1;
                    self.b.mode[1] = m1 as u8;
                } else {
                    self.b.mode[1] = self.b.mode[0];
                }
                if bs != BS_4X8 {
                    let m2 = self.c.tree(&INTRAMODE_TREE, &self.p.y_mode[0]);
                    self.counts.y_mode[0][m2] += 1;
                    self.b.mode[2] = m2 as u8;
                    if bs != BS_8X4 {
                        let m3 = self.c.tree(&INTRAMODE_TREE, &self.p.y_mode[0]);
                        self.counts.y_mode[0][m3] += 1;
                        self.b.mode[3] = m3 as u8;
                    } else {
                        self.b.mode[3] = self.b.mode[2];
                    }
                } else {
                    self.b.mode[2] = self.b.mode[0];
                    self.b.mode[3] = self.b.mode[1];
                }
            } else {
                let sz = SIZE_GROUP[bs] as usize;
                let m = self.c.tree(&INTRAMODE_TREE, &self.p.y_mode[sz]);
                self.b.mode = [m as u8; 4];
                self.counts.y_mode[sz][m] += 1;
            }
            let m3 = self.b.mode[3] as usize;
            let uv = self.c.tree(&INTRAMODE_TREE, &self.p.uv_mode[m3]);
            self.b.uvmode = uv as u8;
            self.counts.uv_mode[m3][uv] += 1;
        } else {
            let (ai, ac, ar) = (self.above_intra[col] != 0, self.above_comp[col] != 0, self.above_ref[col] as usize);
            let (li, lc, lr) = (self.left_intra[row7] != 0, self.left_comp[row7] != 0, self.left_ref[row7] as usize);
            let vc0 = self.h.varcompref[0];
            let vc1 = self.h.varcompref[1];
            if seg_en && feat.ref_enabled {
                self.b.comp = false;
                self.b.refs[0] = feat.ref_val as usize - 1;
            } else {
                if self.h.comppredmode != PRED_SWITCHABLE {
                    self.b.comp = self.h.comppredmode == PRED_COMPREF;
                } else {
                    let c: usize = if have_a {
                        if have_l {
                            if ac && lc {
                                4
                            } else if ac {
                                2 + (li || lr == fixcompref) as usize
                            } else if lc {
                                2 + (ai || ar == fixcompref) as usize
                            } else {
                                ((!ai && ar == fixcompref) ^ (!li && lr == fixcompref)) as usize
                            }
                        } else if ac {
                            3
                        } else {
                            (!ai && ar == fixcompref) as usize
                        }
                    } else if have_l {
                        if lc { 3 } else { (!li && lr == fixcompref) as usize }
                    } else {
                        1
                    };
                    let bit = self.c.get(self.p.comp[c]);
                    self.counts.comp[c][bit as usize] += 1;
                    self.b.comp = bit;
                }

                if self.b.comp {
                    let fix_idx = self.h.signbias[fixcompref] as usize;
                    let var_idx = 1 - fix_idx;
                    self.b.refs[fix_idx] = fixcompref;
                    let c: usize = if have_a {
                        if have_l {
                            if ai {
                                if li { 2 } else { 1 + 2 * (lr != vc1) as usize }
                            } else if li {
                                1 + 2 * (ar != vc1) as usize
                            } else {
                                let (refl, refa) = (lr, ar);
                                if refl == refa && refa == vc1 {
                                    0
                                } else if !lc && !ac {
                                    if (refa == fixcompref && refl == vc0) || (refl == fixcompref && refa == vc0) {
                                        4
                                    } else if refa == refl {
                                        3
                                    } else {
                                        1
                                    }
                                } else if !lc {
                                    if refa == vc1 && refl != vc1 {
                                        1
                                    } else if refl == vc1 && refa != vc1 {
                                        2
                                    } else {
                                        4
                                    }
                                } else if !ac {
                                    if refl == vc1 && refa != vc1 {
                                        1
                                    } else if refa == vc1 && refl != vc1 {
                                        2
                                    } else {
                                        4
                                    }
                                } else if refl == refa {
                                    4
                                } else {
                                    2
                                }
                            }
                        } else if ai {
                            2
                        } else if ac {
                            4 * (ar != vc1) as usize
                        } else {
                            3 * (ar != vc1) as usize
                        }
                    } else if have_l {
                        if li {
                            2
                        } else if lc {
                            4 * (lr != vc1) as usize
                        } else {
                            3 * (lr != vc1) as usize
                        }
                    } else {
                        2
                    };
                    let bit = self.c.get(self.p.comp_ref[c]);
                    self.b.refs[var_idx] = self.h.varcompref[bit as usize];
                    self.counts.comp_ref[c][bit as usize] += 1;
                } else {
                    let fc0 = fixcompref == 0;
                    let mut c: usize = if have_a && !ai {
                        if have_l && !li {
                            if lc {
                                if ac {
                                    1 + (fc0 || lr == 0 || ar == 0) as usize
                                } else {
                                    3 * (ar == 0) as usize + (fc0 || lr == 0) as usize
                                }
                            } else if ac {
                                3 * (lr == 0) as usize + (fc0 || ar == 0) as usize
                            } else {
                                2 * (lr == 0) as usize + 2 * (ar == 0) as usize
                            }
                        } else if ai {
                            2
                        } else if ac {
                            1 + (fc0 || ar == 0) as usize
                        } else {
                            4 * (ar == 0) as usize
                        }
                    } else if have_l && !li {
                        if li {
                            2
                        } else if lc {
                            1 + (fc0 || lr == 0) as usize
                        } else {
                            4 * (lr == 0) as usize
                        }
                    } else {
                        2
                    };
                    let bit = self.c.get(self.p.single_ref[c][0]);
                    self.counts.single_ref[c][0][bit as usize] += 1;
                    if !bit {
                        self.b.refs[0] = 0;
                    } else {
                        let fc1 = fixcompref == 1;
                        c = if have_a {
                            if have_l {
                                if li {
                                    if ai {
                                        2
                                    } else if ac {
                                        1 + 2 * (fc1 || ar == 1) as usize
                                    } else if ar == 0 {
                                        3
                                    } else {
                                        4 * (ar == 1) as usize
                                    }
                                } else if ai {
                                    if li {
                                        2
                                    } else if lc {
                                        1 + 2 * (fc1 || lr == 1) as usize
                                    } else if lr == 0 {
                                        3
                                    } else {
                                        4 * (lr == 1) as usize
                                    }
                                } else if ac {
                                    if lc {
                                        if lr == ar {
                                            3 * (fc1 || lr == 1) as usize
                                        } else {
                                            2
                                        }
                                    } else if lr == 0 {
                                        1 + 2 * (fc1 || ar == 1) as usize
                                    } else {
                                        3 * (lr == 1) as usize + (fc1 || ar == 1) as usize
                                    }
                                } else if lc {
                                    if ar == 0 {
                                        1 + 2 * (fc1 || lr == 1) as usize
                                    } else {
                                        3 * (ar == 1) as usize + (fc1 || lr == 1) as usize
                                    }
                                } else if ar == 0 {
                                    if lr == 0 { 3 } else { 4 * (lr == 1) as usize }
                                } else if lr == 0 {
                                    4 * (ar == 1) as usize
                                } else {
                                    2 * (lr == 1) as usize + 2 * (ar == 1) as usize
                                }
                            } else if ai || (!ac && ar == 0) {
                                2
                            } else if ac {
                                3 * (fc1 || ar == 1) as usize
                            } else {
                                4 * (ar == 1) as usize
                            }
                        } else if have_l {
                            if li || (!lc && lr == 0) {
                                2
                            } else if lc {
                                3 * (fc1 || lr == 1) as usize
                            } else {
                                4 * (lr == 1) as usize
                            }
                        } else {
                            2
                        };
                        let bit = self.c.get(self.p.single_ref[c][1]);
                        self.counts.single_ref[c][1][bit as usize] += 1;
                        self.b.refs[0] = 1 + bit as usize;
                    }
                }
            }

            let am = |s: &Dec, o: usize| s.above_mode[col + o] as usize;
            if bs <= BS_8X8 {
                if seg_en && feat.skip_enabled {
                    self.b.mode = [ZEROMV; 4];
                } else {
                    let off = ABOVE_MODE_CTX_OFFSET[bs] as usize;
                    let c = INTER_MODE_CTX_LUT[am(self, off)][self.left_mode[row7 + off] as usize] as usize;
                    let m = self.c.tree(&INTER_MODE_TREE, &self.p.mv_mode[c]) as u8;
                    self.b.mode = [m; 4];
                    self.counts.mv_mode[c][(m - 10) as usize] += 1;
                }
            }

            if self.h.filtermode == FILTER_SWITCHABLE {
                let am0 = self.above_mode[col] >= NEARESTMV;
                let lm0 = self.left_mode[row7] >= NEARESTMV;
                let c: usize = if have_a && am0 {
                    if have_l && lm0 {
                        if self.above_filter[col] == self.left_filter[row7] { self.left_filter[row7] as usize } else { 3 }
                    } else {
                        self.above_filter[col] as usize
                    }
                } else if have_l && lm0 {
                    self.left_filter[row7] as usize
                } else {
                    3
                };
                let fid = self.c.tree(&FILTER_TREE, &self.p.filter[c]);
                self.counts.filter[c][fid] += 1;
                filter_id = fid as u8;
                self.b.filter = FILTER_LUT[fid];
            } else {
                self.b.filter = self.h.filtermode;
            }

            if bs > BS_8X8 {
                let c = INTER_MODE_CTX_LUT[am(self, 0)][self.left_mode[row7] as usize] as usize;
                let m0 = self.c.tree(&INTER_MODE_TREE, &self.p.mv_mode[c]) as u8;
                self.b.mode[0] = m0;
                self.counts.mv_mode[c][(m0 - 10) as usize] += 1;
                self.b.mv[0] = self.fill_mv(m0, 0);
                if bs != BS_8X4 {
                    let m1 = self.c.tree(&INTER_MODE_TREE, &self.p.mv_mode[c]) as u8;
                    self.b.mode[1] = m1;
                    self.counts.mv_mode[c][(m1 - 10) as usize] += 1;
                    self.b.mv[1] = self.fill_mv(m1, 1);
                } else {
                    self.b.mode[1] = m0;
                    self.b.mv[1] = self.b.mv[0];
                }
                if bs != BS_4X8 {
                    let m2 = self.c.tree(&INTER_MODE_TREE, &self.p.mv_mode[c]) as u8;
                    self.b.mode[2] = m2;
                    self.counts.mv_mode[c][(m2 - 10) as usize] += 1;
                    self.b.mv[2] = self.fill_mv(m2, 2);
                    if bs != BS_8X4 {
                        let m3 = self.c.tree(&INTER_MODE_TREE, &self.p.mv_mode[c]) as u8;
                        self.b.mode[3] = m3;
                        self.counts.mv_mode[c][(m3 - 10) as usize] += 1;
                        self.b.mv[3] = self.fill_mv(m3, 3);
                    } else {
                        self.b.mode[3] = m2;
                        self.b.mv[3] = self.b.mv[2];
                    }
                } else {
                    self.b.mode[2] = self.b.mode[0];
                    self.b.mv[2] = self.b.mv[0];
                    self.b.mode[3] = self.b.mode[1];
                    self.b.mv[3] = self.b.mv[1];
                }
            } else {
                let m = self.b.mode[0];
                self.b.mv[0] = self.fill_mv(m, -1);
                self.b.mv[1] = self.b.mv[0];
                self.b.mv[2] = self.b.mv[0];
                self.b.mv[3] = self.b.mv[0];
            }

            vref = self.b.refs[if self.b.comp { self.h.signbias[self.h.varcompref[0]] as usize } else { 0 }];
        }

        // contexts
        let b = self.b;
        let nw = BWH_TAB[1][bs][0] as usize;
        let nh = BWH_TAB[1][bs][1] as usize;
        splat(&mut self.above_skip, col, nw, b.skip as u8);
        splat(&mut self.above_txfm, col, nw, b.tx as u8);
        splat(&mut self.above_partition, col, nw, ABOVE_CTX[bs]);
        splat(&mut self.left_skip, row7, nh, b.skip as u8);
        splat(&mut self.left_txfm, row7, nh, b.tx as u8);
        splat(&mut self.left_partition, row7, nh, LEFT_CTX[bs]);
        if !keyintra {
            splat(&mut self.above_intra, col, nw, b.intra as u8);
            splat(&mut self.above_comp, col, nw, b.comp as u8);
            splat(&mut self.above_mode, col, nw, b.mode[3]);
            splat(&mut self.left_intra, row7, nh, b.intra as u8);
            splat(&mut self.left_comp, row7, nh, b.comp as u8);
            splat_arr(&mut self.left_mode, row7, nh, b.mode[3]);
            if !b.intra {
                splat(&mut self.above_ref, col, nw, vref as u8);
                splat(&mut self.left_ref, row7, nh, vref as u8);
                if self.h.filtermode == FILTER_SWITCHABLE {
                    splat(&mut self.above_filter, col, nw, filter_id);
                    splat(&mut self.left_filter, row7, nh, filter_id);
                }
            }
        }

        if !keyintra {
            if bs > BS_8X8 {
                self.left_mv[row7 * 2] = b.mv[1];
                self.left_mv[row7 * 2 + 1] = b.mv[3];
                self.above_mv[col * 2] = b.mv[2];
                self.above_mv[col * 2 + 1] = b.mv[3];
            } else {
                for n in 0..w4 * 2 {
                    self.above_mv[col * 2 + n] = b.mv[3];
                }
                for n in 0..h4 * 2 {
                    self.left_mv[row7 * 2 + n] = b.mv[3];
                }
            }
        }

        for y in 0..h4 {
            let o = (row + y) * self.sb_cols * 8 + col;
            for x in 0..w4 {
                let m = &mut self.cur.mv[o + x];
                if b.intra {
                    m.r = [-1, -1];
                } else if b.comp {
                    m.r = [b.refs[0] as i8, b.refs[1] as i8];
                    m.mv = b.mv[3];
                } else {
                    m.r = [b.refs[0] as i8, -1];
                    m.mv[0] = b.mv[3][0];
                }
            }
        }
    }

    /// Reads the coefficients of the whole block. Returns whether any transform block had any.
    pub fn decode_coeffs(&mut self) -> bool {
        let (row, col) = (self.row, self.col);
        let b = self.b;
        let bs = b.bs;
        let mut w4 = (BWH_TAB[1][bs][0] as usize) << 1;
        let mut h4 = (BWH_TAB[1][bs][1] as usize) << 1;
        let mut end_x = (2 * (self.cols - col)).min(w4);
        let mut end_y = (2 * (self.rows - row)).min(h4);
        let qmul = self.h.seg.feat[b.seg_id].qmul;
        let tx = 4 * self.h.lossless as usize + b.tx;
        let ref_ = !b.intra as usize;
        let bpp = self.bpp;
        let mut total = false;

        let mut a = [0u8; 20];
        let mut l = [0u8; 20];
        let ly = (row & 7) * 2;
        a[..16].copy_from_slice(&self.above_y_nnz[col * 2..col * 2 + 16]);
        l[..16 - ly].copy_from_slice(&self.left_y_nnz[ly..16]);
        self.y_tokens(tx, b.tx, ref_, end_x, end_y, w4, h4, &mut a, &mut l, qmul[0], bpp, &mut total);
        self.above_y_nnz[col * 2..col * 2 + 16].copy_from_slice(&a[..16]);
        self.left_y_nnz[ly..16].copy_from_slice(&l[..16 - ly]);

        w4 >>= self.ss_h;
        end_x >>= self.ss_h;
        h4 >>= self.ss_v;
        end_y >>= self.ss_v;
        let ao = col << (1 - self.ss_h);
        let lo = (row & 7) << (1 - self.ss_v);
        for pl in 0..2 {
            let mut a = [0u8; 20];
            let mut l = [0u8; 20];
            a[..16].copy_from_slice(&self.above_uv_nnz[pl][ao..ao + 16]);
            l[..16 - lo].copy_from_slice(&self.left_uv_nnz[pl][lo..16]);
            self.uv_tokens(pl, b.uvtx, ref_, end_x, end_y, w4, h4, &mut a, &mut l, qmul[1], bpp, &mut total);
            self.above_uv_nnz[pl][ao..ao + 16].copy_from_slice(&a[..16]);
            self.left_uv_nnz[pl][lo..16].copy_from_slice(&l[..16 - lo]);
        }
        total
    }

    #[allow(clippy::too_many_arguments)]
    fn y_tokens(&mut self, tx: usize, btx: usize, ref_: usize, end_x: usize, end_y: usize, w4: usize, h4: usize, a: &mut [u8; 20], l: &mut [u8; 20], qmul: [i32; 2], bpp: u32, total: &mut bool) {
        let b = self.b;
        let step = 1usize << btx;
        if btx > 0 {
            merge(l, end_y, step);
            merge(a, end_x, step);
        }
        let mut n = 0usize;
        let mut y = 0;
        while y < end_y {
            let mut x = 0;
            while x < end_x {
                let mode_index = if btx == 0 && b.bs > BS_8X8 { n } else { 0 };
                let txtp = INTRA_TXFM_TYPE[b.mode[mode_index] as usize] as usize;
                let nnz = (a[x] + l[y]) as usize;
                let ret = coeffs_b(
                    &mut self.c,
                    &mut self.block[16 * n..],
                    16 * step * step,
                    btx == 3,
                    bpp,
                    &mut self.counts.coef[btx][0][ref_],
                    &mut self.counts.eob[btx][0][ref_],
                    &self.coef[btx][0][ref_],
                    nnz,
                    SCANS[tx][txtp],
                    SCANS_NB[tx][txtp],
                    &BAND_COUNTS_T[btx],
                    qmul,
                );
                a[x] = (ret != 0) as u8;
                l[y] = (ret != 0) as u8;
                *total |= ret != 0;
                self.eob[n] = ret as u16;
                x += step;
                n += step * step;
            }
            y += step;
        }
        if btx > 0 {
            splat_ctx(a, end_x, step, end_x == w4);
            splat_ctx(l, end_y, step, end_y == h4);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn uv_tokens(&mut self, pl: usize, uvtx: usize, ref_: usize, end_x: usize, end_y: usize, w4: usize, h4: usize, a: &mut [u8; 20], l: &mut [u8; 20], qmul: [i32; 2], bpp: u32, total: &mut bool) {
        let step = 1usize << uvtx;
        let sc = SCANS[uvtx][0];
        let nb = SCANS_NB[uvtx][0];
        if uvtx > 0 {
            merge(l, end_y, step);
            merge(a, end_x, step);
        }
        let mut n = 0usize;
        let mut y = 0;
        while y < end_y {
            let mut x = 0;
            while x < end_x {
                let nnz = (a[x] + l[y]) as usize;
                let ret = coeffs_b(
                    &mut self.c,
                    &mut self.uvblock[pl][16 * n..],
                    16 * step * step,
                    uvtx == 3,
                    bpp,
                    &mut self.counts.coef[uvtx][1][ref_],
                    &mut self.counts.eob[uvtx][1][ref_],
                    &self.coef[uvtx][1][ref_],
                    nnz,
                    sc,
                    nb,
                    &BAND_COUNTS_T[uvtx],
                    qmul,
                );
                a[x] = (ret != 0) as u8;
                l[y] = (ret != 0) as u8;
                *total |= ret != 0;
                self.uveob[pl][n] = ret as u16;
                x += step;
                n += step * step;
            }
            y += step;
        }
        if uvtx > 0 {
            splat_ctx(a, end_x, step, end_x == w4);
            splat_ctx(l, end_y, step, end_y == h4);
        }
    }

    pub fn decode_block(&mut self, row: usize, col: usize, bl: usize, bp: usize) -> Result<(), &'static str> {
        let bs = bl * 3 + bp;
        let w4 = BWH_TAB[1][bs][0] as usize;
        let h4 = BWH_TAB[1][bs][1] as usize;
        self.row = row;
        self.col = col;
        let row7 = row & 7;
        let col7 = col & 7;

        self.min_mv = Mv32 { x: -(128 + col as i32 * 64), y: -(128 + row as i32 * 64) };
        self.max_mv = Mv32 { x: 128 + (self.cols as i32 - col as i32 - w4 as i32) * 64, y: 128 + (self.rows as i32 - row as i32 - h4 as i32) * 64 };

        self.b.bs = bs;
        self.b.bl = bl;
        self.b.bp = bp;
        self.decode_mode();
        self.b.uvtx = self.b.tx - (((self.ss_h != 0 && w4 * 2 == (1 << self.b.tx)) || (self.ss_v != 0 && h4 * 2 == (1 << self.b.tx))) as usize);

        if !self.b.skip {
            let has = self.decode_coeffs();
            if !has && bs <= BS_8X8 && !self.b.intra {
                self.b.skip = true;
                splat(&mut self.above_skip, col, w4, 1);
                splat(&mut self.left_skip, row7, h4, 1);
            }
        } else {
            splat(&mut self.above_y_nnz, col * 2, w4 * 2, 0);
            splat_arr(&mut self.left_y_nnz, row7 * 2, h4 * 2, 0);
            for p in 0..2 {
                if self.ss_h != 0 {
                    splat(&mut self.above_uv_nnz[p], col, w4, 0);
                } else {
                    splat(&mut self.above_uv_nnz[p], col * 2, w4 * 2, 0);
                }
                if self.ss_v != 0 {
                    splat_arr(&mut self.left_uv_nnz[p], row7, h4, 0);
                } else {
                    splat_arr(&mut self.left_uv_nnz[p], row7 * 2, h4 * 2, 0);
                }
            }
        }

        if std::env::var_os("IRONAV_VP9_TRACE").is_some() {
            eprintln!("blk r{} c{} bs{} tx{} uvtx{} skip{} intra{} mode{:?} uv{} eob{:?}", row, col, bs, self.b.tx, self.b.uvtx, self.b.skip, self.b.intra, self.b.mode, self.b.uvmode, &self.eob[..4]);
        }
        if self.b.intra {
            self.intra_recon();
        } else {
            self.inter_recon()?;
        }

        let b = self.b;
        let lvl = if self.h.lf_level != 0 {
            self.h.seg.feat[b.seg_id].lflvl[if b.intra { 0 } else { b.refs[0] + 1 }][(b.mode[3] != ZEROMV) as usize]
        } else {
            0
        };
        if lvl > 0 {
            let x_end = w4.min(self.cols - col);
            let y_end = h4.min(self.rows - row);
            let skip_inter = !b.intra && b.skip;
            let sb = (row >> 3) * self.sb_cols + (col >> 3);
            let (ss_h, ss_v) = (self.ss_h, self.ss_v);
            let (cols, rows) = (self.cols, self.rows);
            let f = &mut self.lflvl[sb];
            for y in 0..h4 {
                for x in 0..w4 {
                    f.level[(row7 + y) * 8 + col7 + x] = lvl;
                }
            }
            mask_edges(&mut f.mask[0], 0, 0, row7, col7, x_end, y_end, 0, 0, b.tx, skip_inter);
            if ss_h != 0 || ss_v != 0 {
                mask_edges(
                    &mut f.mask[1],
                    ss_h,
                    ss_v,
                    row7,
                    col7,
                    x_end,
                    y_end,
                    if cols & 1 != 0 && col + w4 >= cols { cols & 7 } else { 0 },
                    if rows & 1 != 0 && row + h4 >= rows { rows & 7 } else { 0 },
                    b.uvtx,
                    skip_inter,
                );
            }
        }
        Ok(())
    }
}

const BAND_COUNTS_T: [[i16; 8]; 4] = BAND_COUNTS;

fn splat_arr<const N: usize>(a: &mut [u8; N], off: usize, n: usize, v: u8) {
    for x in &mut a[off..off + n] {
        *x = v;
    }
}

/// Folds the per-4x4 non-zero flags of each transform block into its first entry.
fn merge(la: &mut [u8; 20], end: usize, step: usize) {
    let mut n = 0;
    while n < end {
        let any = la[n..(n + step).min(20)].iter().any(|&v| v != 0);
        la[n] = any as u8;
        n += step;
    }
}

/// Spreads each transform block's flag over its 4x4 columns (or rows).
fn splat_ctx(la: &mut [u8; 20], end: usize, step: usize, full: bool) {
    if step == 2 {
        let mut n = 1;
        while n < end {
            la[n] = la[n - 1];
            n += 2;
        }
    } else if full {
        let mut n = 0;
        while n < end {
            let v = la[n];
            for k in 0..step {
                la[n + k] = v;
            }
            n += step;
        }
    } else {
        let mut n = 0;
        while n < end {
            let v = la[n];
            let cnt = (end - n - 1).min(step - 1);
            for k in 1..=cnt {
                la[n + k] = v;
            }
            n += step;
        }
    }
}

fn mask_edges(mask: &mut [[[u8; 4]; 8]; 2], ss_h: usize, ss_v: usize, row_and_7: usize, col_and_7: usize, mut w: usize, mut h: usize, col_end: usize, row_end: usize, tx: usize, skip_inter: bool) {
    let wide_col = WIDE_FILTER_COL_MASK;
    let wide_row = WIDE_FILTER_ROW_MASK;

    if tx == 0 && (ss_v | ss_h) != 0 {
        if h == ss_v {
            if row_and_7 & 1 != 0 {
                return;
            }
            if row_end == 0 {
                h += 1;
            }
        }
        if w == ss_h {
            if col_and_7 & 1 != 0 {
                return;
            }
            if col_end == 0 {
                w += 1;
            }
        }
    }

    if tx == 0 && !skip_inter {
        let t = 1u32 << col_and_7;
        let m_col = (t << w) - t;
        let m_row_8 = m_col & wide_col[ss_h];
        let m_row_4 = m_col - m_row_8;
        for y in row_and_7..h + row_and_7 {
            let col_mask_id = 2 - (y as u32 & wide_row[ss_v] == 0) as usize;
            mask[0][y][1] |= m_row_8 as u8;
            mask[0][y][2] |= m_row_4 as u8;
            if (ss_h & ss_v) != 0 && (col_end & 1) != 0 && (y & 1) != 0 {
                mask[1][y][col_mask_id] |= ((t << (w - 1)) - t) as u8;
            } else {
                mask[1][y][col_mask_id] |= m_col as u8;
            }
            if ss_h == 0 {
                mask[0][y][3] |= m_col as u8;
            }
            if ss_v == 0 {
                if ss_h != 0 && (col_end & 1) != 0 {
                    mask[1][y][3] |= ((t << (w - 1)) - t) as u8;
                } else {
                    mask[1][y][3] |= m_col as u8;
                }
            }
        }
    } else {
        let t = 1u32 << col_and_7;
        let m_col = (t << w) - t;
        if !skip_inter {
            let mask_id = (tx == 1) as usize;
            let mut l2 = (tx + ss_h) as i32 - 1;
            let masks = UV_EDGE_MASKS;
            let m_row = m_col & masks[l2 as usize];
            if ss_h != 0 && tx > 1 && (w ^ (w - 1)) == 1 {
                let m_row_16 = ((t << (w - 1)) - t) & masks[l2 as usize];
                let m_row_8 = m_row - m_row_16;
                for y in row_and_7..h + row_and_7 {
                    mask[0][y][0] |= m_row_16 as u8;
                    mask[0][y][1] |= m_row_8 as u8;
                }
            } else {
                for y in row_and_7..h + row_and_7 {
                    mask[0][y][mask_id] |= m_row as u8;
                }
            }
            l2 = (tx + ss_v) as i32 - 1;
            let step1d = 1usize << l2;
            if ss_v != 0 && tx > 1 && (h ^ (h - 1)) == 1 {
                let mut y = row_and_7;
                while y < h + row_and_7 - 1 {
                    mask[1][y][0] |= m_col as u8;
                    y += step1d;
                }
                if y - row_and_7 == h - 1 {
                    mask[1][y][1] |= m_col as u8;
                }
            } else {
                let mut y = row_and_7;
                while y < h + row_and_7 {
                    mask[1][y][mask_id] |= m_col as u8;
                    y += step1d;
                }
            }
        } else if tx != 0 {
            let mut mask_id = (tx == 1 || h == ss_v) as usize;
            mask[1][row_and_7][mask_id] |= m_col as u8;
            mask_id = (tx == 1 || w == ss_h) as usize;
            for y in row_and_7..h + row_and_7 {
                mask[0][y][mask_id] |= t as u8;
            }
        } else {
            let t8 = t & wide_col[ss_h];
            let t4 = t - t8;
            for y in row_and_7..h + row_and_7 {
                mask[0][y][2] |= t4 as u8;
                mask[0][y][1] |= t8 as u8;
            }
            let id = 2 - (row_and_7 as u32 & wide_row[ss_v] == 0) as usize;
            mask[1][row_and_7][id] |= m_col as u8;
        }
    }
}
