//! Backward probability adaptation after a frame has been decoded.

use super::dec::*;
use super::header::*;

fn adapt(p: &mut u8, ct0: u32, ct1: u32, max_count: u32, update_factor: u32) {
    let ct = ct0 + ct1;
    if ct == 0 {
        return;
    }
    let uf = (update_factor * ct.min(max_count) / max_count) as i32;
    let p1 = *p as i32;
    let p2 = ((((ct0 as u64) << 8) + (ct as u64 >> 1)) / ct as u64).clamp(1, 255) as i32;
    *p = (p1 + (((p2 - p1) * uf + 128) >> 8)) as u8;
}

fn a20(p: &mut u8, c0: u32, c1: u32) {
    adapt(p, c0, c1, 20, 128);
}

pub fn adapt_probs(d: &Dec, st: &mut HeaderState, last_keyframe: bool) {
    let h = &d.h;
    let cnt = &d.counts;
    let ctx = &mut st.prob_ctx[h.framectxid];
    let uf = if h.keyframe || h.intraonly || !last_keyframe { 112 } else { 128 };

    for i in 0..4 {
        for j in 0..2 {
            for k in 0..2 {
                for l in 0..6 {
                    for m in 0..6 {
                        if l == 0 && m >= 3 {
                            break;
                        }
                        let pp = &mut ctx.coef[i][j][k][l][m];
                        let e = cnt.eob[i][j][k][l][m];
                        let c = cnt.coef[i][j][k][l][m];
                        adapt(&mut pp[0], e[0], e[1], 24, uf);
                        adapt(&mut pp[1], c[0], c[1] + c[2], 24, uf);
                        adapt(&mut pp[2], c[1], c[2], 24, uf);
                    }
                }
            }
        }
    }

    let p = &mut ctx.p;
    if h.keyframe || h.intraonly {
        p.skip = d.p.skip;
        p.tx32p = d.p.tx32p;
        p.tx16p = d.p.tx16p;
        p.tx8p = d.p.tx8p;
        return;
    }

    for i in 0..3 {
        a20(&mut p.skip[i], cnt.skip[i][0], cnt.skip[i][1]);
    }
    for i in 0..4 {
        a20(&mut p.intra[i], cnt.intra[i][0], cnt.intra[i][1]);
    }
    if h.comppredmode == PRED_SWITCHABLE {
        for i in 0..5 {
            a20(&mut p.comp[i], cnt.comp[i][0], cnt.comp[i][1]);
        }
    }
    if h.comppredmode != PRED_SINGLEREF {
        for i in 0..5 {
            a20(&mut p.comp_ref[i], cnt.comp_ref[i][0], cnt.comp_ref[i][1]);
        }
    }
    if h.comppredmode != PRED_COMPREF {
        for i in 0..5 {
            let c = &cnt.single_ref[i];
            a20(&mut p.single_ref[i][0], c[0][0], c[0][1]);
            a20(&mut p.single_ref[i][1], c[1][0], c[1][1]);
        }
    }
    for i in 0..4 {
        for j in 0..4 {
            let c = &cnt.partition[i][j];
            let pp = &mut p.partition[i][j];
            a20(&mut pp[0], c[0], c[1] + c[2] + c[3]);
            a20(&mut pp[1], c[1], c[2] + c[3]);
            a20(&mut pp[2], c[2], c[3]);
        }
    }
    if h.txfmmode == TX_SWITCHABLE {
        for i in 0..2 {
            let c16 = &cnt.tx16p[i];
            let c32 = &cnt.tx32p[i];
            a20(&mut p.tx8p[i], cnt.tx8p[i][0], cnt.tx8p[i][1]);
            a20(&mut p.tx16p[i][0], c16[0], c16[1] + c16[2]);
            a20(&mut p.tx16p[i][1], c16[1], c16[2]);
            a20(&mut p.tx32p[i][0], c32[0], c32[1] + c32[2] + c32[3]);
            a20(&mut p.tx32p[i][1], c32[1], c32[2] + c32[3]);
            a20(&mut p.tx32p[i][2], c32[2], c32[3]);
        }
    }
    if h.filtermode == FILTER_SWITCHABLE {
        for i in 0..4 {
            let c = &cnt.filter[i];
            a20(&mut p.filter[i][0], c[0], c[1] + c[2]);
            a20(&mut p.filter[i][1], c[1], c[2]);
        }
    }
    for i in 0..7 {
        let c = &cnt.mv_mode[i];
        let pp = &mut p.mv_mode[i];
        a20(&mut pp[0], c[2], c[1] + c[0] + c[3]);
        a20(&mut pp[1], c[0], c[1] + c[3]);
        a20(&mut pp[2], c[1], c[3]);
    }
    {
        let c = &cnt.mv_joint;
        let pp = &mut p.mv_joint;
        a20(&mut pp[0], c[0], c[1] + c[2] + c[3]);
        a20(&mut pp[1], c[1], c[2] + c[3]);
        a20(&mut pp[2], c[2], c[3]);
    }
    for i in 0..2 {
        let mc = &cnt.mv_comp[i];
        let m = &mut p.mv_comp[i];
        a20(&mut m.sign, mc.sign[0], mc.sign[1]);
        let c = &mc.classes;
        let pp = &mut m.classes;
        let mut sum: u32 = c[1..=10].iter().sum();
        a20(&mut pp[0], c[0], sum);
        sum -= c[1];
        a20(&mut pp[1], c[1], sum);
        sum -= c[2] + c[3];
        a20(&mut pp[2], c[2] + c[3], sum);
        a20(&mut pp[3], c[2], c[3]);
        sum -= c[4] + c[5];
        a20(&mut pp[4], c[4] + c[5], sum);
        a20(&mut pp[5], c[4], c[5]);
        sum -= c[6];
        a20(&mut pp[6], c[6], sum);
        a20(&mut pp[7], c[7] + c[8], c[9] + c[10]);
        a20(&mut pp[8], c[7], c[8]);
        a20(&mut pp[9], c[9], c[10]);

        a20(&mut m.class0, mc.class0[0], mc.class0[1]);
        for j in 0..10 {
            a20(&mut m.bits[j], mc.bits[j][0], mc.bits[j][1]);
        }
        for j in 0..2 {
            let c = &mc.class0_fp[j];
            let pp = &mut m.class0_fp[j];
            a20(&mut pp[0], c[0], c[1] + c[2] + c[3]);
            a20(&mut pp[1], c[1], c[2] + c[3]);
            a20(&mut pp[2], c[2], c[3]);
        }
        {
            let c = &mc.fp;
            let pp = &mut m.fp;
            a20(&mut pp[0], c[0], c[1] + c[2] + c[3]);
            a20(&mut pp[1], c[1], c[2] + c[3]);
            a20(&mut pp[2], c[2], c[3]);
        }
        if h.highprecisionmvs {
            a20(&mut m.class0_hp, mc.class0_hp[0], mc.class0_hp[1]);
            a20(&mut m.hp, mc.hp[0], mc.hp[1]);
        }
    }

    // intra modes: DC=2, TM=9, V=0, H=1, DDR=4, VR=5, DDL=3, VL=7, HD=6, HU=8
    for i in 0..4 {
        mode_probs(&mut p.y_mode[i], &cnt.y_mode[i]);
    }
    for i in 0..10 {
        mode_probs(&mut p.uv_mode[i], &cnt.uv_mode[i]);
    }
}

fn mode_probs(pp: &mut [u8; 9], c: &[u32; 10]) {
    let mut sum = c[0] + c[1] + c[3] + c[4] + c[5] + c[6] + c[7] + c[8] + c[9];
    a20(&mut pp[0], c[2], sum);
    sum -= c[9];
    a20(&mut pp[1], c[9], sum);
    sum -= c[0];
    a20(&mut pp[2], c[0], sum);
    let mut s2 = c[1] + c[4] + c[5];
    sum -= s2;
    a20(&mut pp[3], s2, sum);
    s2 -= c[1];
    a20(&mut pp[4], c[1], s2);
    a20(&mut pp[5], c[4], c[5]);
    sum -= c[3];
    a20(&mut pp[6], c[3], sum);
    sum -= c[7];
    a20(&mut pp[7], c[7], sum);
    a20(&mut pp[8], c[6], c[8]);
}
