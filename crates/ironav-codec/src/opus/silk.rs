//! The SILK layer of Opus (RFC 6716 section 4.2): linear-prediction speech coding at 8, 12 and 16 kHz.
//!
//! Translated from the structure of FFmpeg's native Opus decoder (`libavcodec/opus/silk.c`). The side
//! information is decoded in integers, exactly as the specification defines it; the synthesis runs in
//! floating point.

use super::rc::{ilog, RangeDecoder};
use super::tables::*;

pub const HISTORY: usize = 322;
const MAX_LAG: usize = 288 + 2;
const LTP_ORDER: usize = 5;

#[derive(Clone)]
struct Frame {
    coded: bool,
    log_gain: i32,
    nlsf: [i16; 16],
    lpc: [f32; 16],
    output: [f32; 2 * HISTORY],
    lpc_history: [f32; 2 * HISTORY],
    primarylag: i32,
    prev_voiced: bool,
}

impl Frame {
    fn new() -> Self {
        Self {
            coded: false,
            log_gain: 0,
            nlsf: [0; 16],
            lpc: [0.0; 16],
            output: [0.0; 2 * HISTORY],
            lpc_history: [0.0; 2 * HISTORY],
            primarylag: 0,
            prev_voiced: false,
        }
    }

    fn flush(&mut self) {
        if self.coded {
            *self = Self::new();
        }
    }
}

pub struct Silk {
    output_channels: usize,
    midonly: bool,
    subframes: usize,
    sflength: usize,
    flength: usize,
    nlsf_interp_factor: i32,
    bandwidth: usize,
    wb: bool,
    frame: [Frame; 2],
    prev_stereo_weights: [f32; 2],
    stereo_weights: [f32; 2],
    prev_coded_channels: usize,
}

fn round_mull(a: i32, b: i32, s: u32) -> i32 {
    ((((a as i64 * b as i64) >> (s - 1)) + 1) >> 1) as i32
}

fn round_mull64(a: i32, b: i32, s: u32) -> i64 {
    (((a as i64 * b as i64) >> (s - 1)) + 1) >> 1
}

fn mulh(a: i32, b: i32) -> i32 {
    ((a as i64 * b as i64) >> 32) as i32
}

pub(super) fn stabilize_lsf(nlsf: &mut [i16; 16], order: usize, min_delta: &[u16]) {
    let md = |i: usize| min_delta[i] as i32;
    for _ in 0..20 {
        let mut min_diff = 0i32;
        let mut k = 0usize;
        for i in 0..=order {
            let low = if i != 0 { nlsf[i - 1] as i32 } else { 0 };
            let high = if i != order { nlsf[i] as i32 } else { 32768 };
            let diff = (high - low) - md(i);
            if diff < min_diff {
                min_diff = diff;
                k = i;
            }
        }
        if min_diff == 0 {
            return; // no issues; stabilized
        }
        // wiggle one or two LSFs
        if k == 0 {
            nlsf[0] = md(0) as i16;
        } else if k == order {
            nlsf[order - 1] = (32768 - md(order)) as i16;
        } else {
            let mut min_center = 0i32;
            let mut max_center = 32768i32;
            for i in 0..k {
                min_center += md(i);
            }
            min_center += md(k) >> 1;
            for i in (k + 1..=order).rev() {
                max_center -= md(i);
            }
            max_center -= md(k) >> 1;
            let mut center = nlsf[k - 1] as i32 + nlsf[k] as i32;
            center = (center >> 1) + (center & 1);
            center = max_center.min(min_center.max(center));
            nlsf[k - 1] = (center - (md(k) >> 1)) as i16;
            nlsf[k] = (nlsf[k - 1] as i32 + md(k)) as i16;
        }
    }

    // fall back to the standard method: sort, then push apart from both directions
    for i in 1..order {
        let value = nlsf[i];
        let mut j = i as isize - 1;
        while j >= 0 && nlsf[j as usize] > value {
            nlsf[j as usize + 1] = nlsf[j as usize];
            j -= 1;
        }
        nlsf[(j + 1) as usize] = value;
    }
    if (nlsf[0] as i32) < md(0) {
        nlsf[0] = md(0) as i16;
    }
    for i in 1..order {
        nlsf[i] = (nlsf[i] as i32).max((nlsf[i - 1] as i32 + md(i)).min(32767)) as i16;
    }
    if nlsf[order - 1] as i32 > 32768 - md(order) {
        nlsf[order - 1] = (32768 - md(order)) as i16;
    }
    for i in (0..order - 1).rev() {
        if nlsf[i] as i32 > nlsf[i + 1] as i32 - md(i + 1) {
            nlsf[i] = (nlsf[i + 1] as i32 - md(i + 1)) as i16;
        }
    }
}

fn is_lpc_stable(lpc: &[i16; 16], order: usize) -> bool {
    let mut lpc32 = [[0i32; 16]; 2];
    let mut total_inv_gain: i32 = 1 << 30;
    let mut dc_resp = 0i32;
    let mut cur = 0usize;
    for k in 0..order {
        dc_resp += lpc[k] as i32;
        lpc32[0][k] = lpc[k] as i32 * 4096;
    }
    if dc_resp >= 4096 {
        return false;
    }
    let mut k = order - 1;
    loop {
        if lpc32[cur][k].abs() > 16773022 {
            return false;
        }
        let rc = -(lpc32[cur][k] * 128);
        let gaindiv = (1 << 30) - mulh(rc, rc);
        total_inv_gain = mulh(total_inv_gain, gaindiv).wrapping_shl(2);
        if k == 0 {
            return total_inv_gain >= 107374;
        }
        // approximate 1.0 / gaindiv
        let fbits = ilog(gaindiv as u32) as i32;
        let mut gain = ((1i32 << 29) - 1) / (gaindiv >> (fbits + 1 - 16));
        let shifted = ((gaindiv as i64) << (15 + 16 - fbits)) as i32;
        let error = (1i32 << 29).wrapping_sub(((shifted as i64 * gain as i64) >> 16) as i32);
        gain = (gain << 16).wrapping_add(((error as i64 * gain as i64) >> 13) as i32);

        let prev = lpc32[cur];
        let next = k & 1;
        for j in 0..k {
            let sub = prev[j] as i64 - round_mull(prev[k - j - 1], rc, 31) as i64;
            let x = sub.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            let tmp = round_mull64(x, gain, fbits as u32);
            // per RFC 8251 section 6, an overflow here means the filter is unstable
            if tmp < i32::MIN as i64 || tmp > i32::MAX as i64 {
                return false;
            }
            lpc32[next][j] = tmp as i32;
        }
        cur = next;
        k -= 1;
    }
}

fn lsp2poly(lsp: &[i32], pol: &mut [i32], half_order: usize) {
    pol[0] = 65536;
    pol[1] = -lsp[0];
    for i in 1..half_order {
        pol[i + 1] = pol[i - 1].wrapping_mul(2).wrapping_sub(round_mull(lsp[2 * i], pol[i], 16));
        for j in (2..=i).rev() {
            pol[j] = pol[j].wrapping_add(pol[j - 2]).wrapping_sub(round_mull(lsp[2 * i], pol[j - 1], 16));
        }
        pol[1] -= lsp[2 * i];
    }
}

pub(super) fn lsf2lpc(nlsf: &[i16; 16], order: usize) -> [f32; 16] {
    let mut lsp = [0i32; 16];
    let (mut p, mut q) = ([0i32; 9], [0i32; 9]);
    let mut lpc32 = [0i32; 16];
    let mut lpc = [0i16; 16];

    for k in 0..order {
        let index = (nlsf[k] >> 8) as usize;
        let offset = (nlsf[k] & 255) as i32;
        let k2 = if order == 10 { SILK_LSF_ORDERING_NBMB[k] } else { SILK_LSF_ORDERING_WB[k] } as usize;
        let mut v = SILK_COSINE[index] as i32 * 256;
        v += (SILK_COSINE[index + 1] as i32 - SILK_COSINE[index] as i32) * offset;
        lsp[k2] = (v + 4) >> 3;
    }
    lsp2poly(&lsp, &mut p, order >> 1);
    lsp2poly(&lsp[1..], &mut q, order >> 1);

    for k in 0..order >> 1 {
        let p_tmp = p[k + 1].wrapping_add(p[k]);
        let q_tmp = q[k + 1].wrapping_sub(q[k]);
        lpc32[k] = (-q_tmp).wrapping_sub(p_tmp);
        lpc32[order - k - 1] = q_tmp.wrapping_sub(p_tmp);
    }

    // limit the range of the coefficients so each fits in an int16
    let mut i = 0;
    while i < 10 {
        let mut maxabs = 0u32;
        let mut k = 0usize;
        for j in 0..order {
            let x = lpc32[j].unsigned_abs();
            if x > maxabs {
                maxabs = x;
                k = j;
            }
        }
        maxabs = (maxabs + 16) >> 5; // to Q12
        if maxabs > 32767 {
            // bandwidth expansion
            maxabs = maxabs.min(163838);
            let chirp_base = 65470 - (((maxabs - 32767) << 14) / ((maxabs * (k as u32 + 1)) >> 2));
            let mut chirp = chirp_base;
            for c in lpc32.iter_mut().take(order) {
                *c = round_mull(*c, chirp as i32, 16);
                chirp = ((chirp_base as u64 * chirp as u64 + 32768) >> 16) as u32;
            }
        } else {
            break;
        }
        i += 1;
    }
    if i == 10 {
        for k in 0..order {
            let x = (lpc32[k] + 16) >> 5;
            lpc[k] = x.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            lpc32[k] = (lpc[k] as i32) << 5;
        }
    } else {
        for k in 0..order {
            lpc[k] = ((lpc32[k] + 16) >> 5) as i16;
        }
    }

    // if the prediction gain makes the filter unstable, expand further
    let mut i = 1;
    while i <= 16 && !is_lpc_stable(&lpc, order) {
        let chirp_base = 65536u32 - (1 << i);
        let mut chirp = chirp_base;
        for k in 0..order {
            lpc32[k] = round_mull(lpc32[k], chirp as i32, 16);
            lpc[k] = ((lpc32[k] + 16) >> 5) as i16;
            chirp = ((chirp_base as u64 * chirp as u64 + 32768) >> 16) as u32;
        }
        i += 1;
    }

    let mut out = [0f32; 16];
    for i in 0..order {
        out[i] = lpc[i] as f32 / 4096.0;
    }
    out
}

fn at(base: usize, off: isize) -> usize {
    (base as isize + off) as usize
}

impl Silk {
    pub fn new(output_channels: usize) -> Self {
        Self {
            output_channels,
            midonly: false,
            subframes: 0,
            sflength: 0,
            flength: 0,
            nlsf_interp_factor: 0,
            bandwidth: 0,
            wb: false,
            frame: [Frame::new(), Frame::new()],
            prev_stereo_weights: [0.0; 2],
            stereo_weights: [0.0; 2],
            prev_coded_channels: 0,
        }
    }

    pub fn flush(&mut self) {
        self.frame[0].flush();
        self.frame[1].flush();
        self.prev_stereo_weights = [0.0; 2];
    }

    #[allow(clippy::type_complexity)]
    fn decode_lpc(&mut self, ch: usize, rc: &mut RangeDecoder, voiced: bool) -> ([f32; 16], [f32; 16], usize, bool) {
        let wb = self.wb;
        let order = if wb { 16 } else { 10 };
        let mut lsf_i2 = [0i32; 16];
        let mut lsf_res = [0i32; 16];
        let mut nlsf = [0i16; 16];

        let i1 = rc.cdf(&SILK_MODEL_LSF_S1[wb as usize][voiced as usize]);
        for i in 0..order {
            let index = if wb { SILK_LSF_S2_MODEL_SEL_WB[i1][i] } else { SILK_LSF_S2_MODEL_SEL_NBMB[i1][i] } as usize;
            let mut v = rc.cdf(&SILK_MODEL_LSF_S2[index]) as i32 - 4;
            if v == -4 {
                v -= rc.cdf(&SILK_MODEL_LSF_S2_EXT) as i32;
            } else if v == 4 {
                v += rc.cdf(&SILK_MODEL_LSF_S2_EXT) as i32;
            }
            lsf_i2[i] = v;
        }

        // reverse the backwards-prediction step
        for i in (0..order).rev() {
            let qstep = if wb { 9830 } else { 11796 };
            let mut r = lsf_i2[i] * 1024;
            if lsf_i2[i] < 0 {
                r += 102;
            } else if lsf_i2[i] > 0 {
                r -= 102;
            }
            r = (r * qstep) >> 16;
            if i + 1 < order {
                let weight = if wb {
                    SILK_LSF_PRED_WEIGHTS_WB[SILK_LSF_WEIGHT_SEL_WB[i1][i] as usize][i]
                } else {
                    SILK_LSF_PRED_WEIGHTS_NBMB[SILK_LSF_WEIGHT_SEL_NBMB[i1][i] as usize][i]
                } as i32;
                r += (lsf_res[i + 1] * weight) >> 8;
            }
            lsf_res[i] = r as i16 as i32;
        }

        // reconstruct the NLSF coefficients
        for i in 0..order {
            let cur = if wb { SILK_LSF_CODEBOOK_WB[i1][i] } else { SILK_LSF_CODEBOOK_NBMB[i1][i] } as i32;
            let weight = if wb { SILK_MODEL_LSF_WEIGHT_WB[i1][i] } else { SILK_MODEL_LSF_WEIGHT_NBMB[i1][i] } as i32;
            let value = cur * 128 + (lsf_res[i] * 16384) / weight;
            nlsf[i] = value.clamp(0, 32767) as i16;
        }
        stabilize_lsf(&mut nlsf, order, if wb { &SILK_LSF_MIN_SPACING_WB } else { &SILK_LSF_MIN_SPACING_NBMB });

        // interpolate the first two subframes, then turn both sets of NLSFs into LPC coefficients
        let mut has_leadin = false;
        let mut leadin = [0f32; 16];
        let body;
        if self.subframes == 4 {
            let mut offset = rc.cdf(&SILK_MODEL_LSF_INTERPOLATION_OFFSET) as i32;
            let frame = &self.frame[ch];
            if offset != 4 && frame.coded {
                has_leadin = true;
                if offset != 0 {
                    let mut lead = [0i16; 16];
                    for i in 0..order {
                        lead[i] = (frame.nlsf[i] as i32 + (((nlsf[i] as i32 - frame.nlsf[i] as i32) * offset) >> 2)) as i16;
                    }
                    leadin = lsf2lpc(&lead, order);
                } else {
                    leadin = frame.lpc;
                }
            } else {
                offset = 4;
            }
            self.nlsf_interp_factor = offset;
            body = lsf2lpc(&nlsf, order);
        } else {
            self.nlsf_interp_factor = 4;
            body = lsf2lpc(&nlsf, order);
        }
        let frame = &mut self.frame[ch];
        frame.nlsf[..order].copy_from_slice(&nlsf[..order]);
        frame.lpc[..order].copy_from_slice(&body[..order]);
        (leadin, body, order, has_leadin)
    }

    fn count_children(rc: &mut RangeDecoder, model: usize, total: i32, child: &mut [i32]) {
        if total != 0 {
            let off = (((total - 1 + 5) * (total - 1)) >> 1) as usize;
            child[0] = rc.cdf(&SILK_MODEL_PULSE_LOCATION[model][off..]) as i32;
            child[1] = total - child[0];
        } else {
            child[0] = 0;
            child[1] = 0;
        }
    }

    fn decode_excitation(&self, rc: &mut RangeDecoder, out: &mut [f32], qoffset_high: usize, active: bool, voiced: bool) {
        let mut pulsecount = [0u8; 20];
        let mut lsbcount = [0u8; 20];
        let mut excitation = [0i32; 320];

        let mut seed = rc.cdf(&SILK_MODEL_LCG_SEED) as u32;
        let shellblocks = SILK_SHELL_BLOCKS[self.bandwidth][self.subframes >> 2] as usize;
        let ratelevel = rc.cdf(&SILK_MODEL_EXC_RATE[voiced as usize]);

        for i in 0..shellblocks {
            pulsecount[i] = rc.cdf(&SILK_MODEL_PULSE_COUNT[ratelevel]) as u8;
            if pulsecount[i] == 17 {
                loop {
                    if pulsecount[i] != 17 {
                        break;
                    }
                    lsbcount[i] += 1;
                    if lsbcount[i] == 10 {
                        break;
                    }
                    pulsecount[i] = rc.cdf(&SILK_MODEL_PULSE_COUNT[9]) as u8;
                }
                if lsbcount[i] == 10 {
                    pulsecount[i] = rc.cdf(&SILK_MODEL_PULSE_COUNT[10]) as u8;
                }
            }
        }

        // pulse locations, by recursively splitting the count in halves
        for i in 0..shellblocks {
            if pulsecount[i] != 0 {
                let loc = 16 * i;
                let mut n = loc;
                let mut b1 = [0i32; 2];
                Self::count_children(rc, 0, pulsecount[i] as i32, &mut b1);
                for b in 0..2 {
                    let mut b2 = [0i32; 2];
                    Self::count_children(rc, 1, b1[b], &mut b2);
                    for c in 0..2 {
                        let mut b3 = [0i32; 2];
                        Self::count_children(rc, 2, b2[c], &mut b3);
                        for d in 0..2 {
                            let mut leaf = [0i32; 2];
                            Self::count_children(rc, 3, b3[d], &mut leaf);
                            excitation[n] = leaf[0];
                            excitation[n + 1] = leaf[1];
                            n += 2;
                        }
                    }
                }
            } else {
                excitation[16 * i..16 * i + 16].fill(0);
            }
        }

        // least significant bits
        for i in 0..shellblocks << 4 {
            for _ in 0..lsbcount[i >> 4] {
                excitation[i] = (excitation[i] << 1) | rc.cdf(&SILK_MODEL_EXCITATION_LSB) as i32;
            }
        }

        // signs
        for i in 0..shellblocks << 4 {
            if excitation[i] != 0 {
                let model = &SILK_MODEL_EXCITATION_SIGN[active as usize + voiced as usize][qoffset_high][(pulsecount[i >> 4] as usize).min(6)];
                if rc.cdf(model) == 0 {
                    excitation[i] = -excitation[i];
                }
            }
        }

        // assemble
        for i in 0..shellblocks << 4 {
            let value = excitation[i];
            let mut e = value * 256 | SILK_QUANT_OFFSET[voiced as usize][qoffset_high] as i32;
            if value < 0 {
                e += 20;
            } else if value > 0 {
                e -= 20;
            }
            // invert samples pseudorandomly
            seed = seed.wrapping_mul(196314165).wrapping_add(907633515);
            if seed & 0x8000_0000 != 0 {
                e = -e;
            }
            seed = seed.wrapping_add(value as u32);
            out[i] = e as f32 / 8388608.0;
        }
    }

    fn decode_frame(&mut self, rc: &mut RangeDecoder, frame_num: usize, ch: usize, coded_channels: usize, active: bool, active1: bool, redundant: bool) {
        let subframes = self.subframes;
        let sflength = self.sflength;
        let bw = self.bandwidth;

        // stereo weights
        if coded_channels == 2 && ch == 0 {
            let n = rc.cdf(&SILK_MODEL_STEREO_S1) as i32;
            let wi0 = rc.cdf(&SILK_MODEL_STEREO_S2) as i32 + 3 * (n / 5);
            let ws0 = rc.cdf(&SILK_MODEL_STEREO_S3) as i32;
            let wi1 = rc.cdf(&SILK_MODEL_STEREO_S2) as i32 + 3 * (n % 5);
            let ws1 = rc.cdf(&SILK_MODEL_STEREO_S3) as i32;
            let mut w = [0i32; 2];
            for (i, (wi, ws)) in [(wi0, ws0), (wi1, ws1)].into_iter().enumerate() {
                let a = SILK_STEREO_WEIGHTS[wi as usize] as i32;
                let b = SILK_STEREO_WEIGHTS[wi as usize + 1] as i32;
                w[i] = a + (((b - a) * 6554) >> 16) * (ws * 2 + 1);
            }
            self.stereo_weights[0] = (w[0] - w[1]) as f32 / 8192.0;
            self.stereo_weights[1] = w[1] as f32 / 8192.0;
            self.midonly = if active1 { false } else { rc.cdf(&SILK_MODEL_MID_ONLY) != 0 };
        }

        // frame type
        let (qoffset_high, voiced);
        if !active {
            qoffset_high = rc.cdf(&SILK_MODEL_FRAME_TYPE_INACTIVE);
            voiced = false;
        } else {
            let t = rc.cdf(&SILK_MODEL_FRAME_TYPE_ACTIVE);
            qoffset_high = t & 1;
            voiced = (t >> 1) != 0;
        }

        // subframe gains
        let mut gains = [0f32; 4];
        for i in 0..subframes {
            let mut log_gain: i32;
            if i == 0 && (frame_num == 0 || !self.frame[ch].coded) {
                let x = rc.cdf(&SILK_MODEL_GAIN_HIGHBITS[active as usize + voiced as usize]) as i32;
                log_gain = (x << 3) | rc.cdf(&SILK_MODEL_GAIN_LOWBITS) as i32;
                if self.frame[ch].coded {
                    log_gain = log_gain.max(self.frame[ch].log_gain - 16);
                }
            } else {
                let delta = rc.cdf(&SILK_MODEL_GAIN_DELTA) as i32;
                log_gain = ((delta << 1) - 16).max(self.frame[ch].log_gain + delta - 4).clamp(0, 63);
            }
            self.frame[ch].log_gain = log_gain;

            // approximate 2**(x/128) with a Q7 input
            let lg = (log_gain * 0x1D1C71 >> 16) + 2090;
            let ipart = lg >> 7;
            let fpart = lg & 127;
            let lingain = (1 << ipart) + ((-174 * fpart * (128 - fpart) >> 16) + fpart) * ((1 << ipart) >> 7);
            gains[i] = lingain as f32 / 65536.0;
        }

        let (leadin, body, order, has_leadin) = self.decode_lpc(ch, rc, voiced);

        // pitch lags and LTP filters of a voiced frame
        let mut pitchlag = [0i32; 4];
        let mut ltptaps = [[0f32; 5]; 4];
        if voiced {
            let mut lag_absolute = frame_num == 0 || !self.frame[ch].prev_voiced;
            let mut primarylag = 0;
            if !lag_absolute {
                let delta = rc.cdf(&SILK_MODEL_PITCH_DELTA) as i32;
                if delta != 0 {
                    primarylag = self.frame[ch].primarylag + delta - 9;
                } else {
                    lag_absolute = true;
                }
            }
            if lag_absolute {
                let high = rc.cdf(&SILK_MODEL_PITCH_HIGHBITS) as i32;
                let model: &[u16] = match bw {
                    0 => &SILK_MODEL_LCG_SEED,
                    1 => &SILK_MODEL_PITCH_LOWBITS_MB,
                    _ => &SILK_MODEL_GAIN_LOWBITS,
                };
                let low = rc.cdf(model) as i32;
                primarylag = SILK_PITCH_MIN_LAG[bw] as i32 + high * SILK_PITCH_SCALE[bw] as i32 + low;
            }
            self.frame[ch].primarylag = primarylag;

            let offsets: &[i8] = if subframes == 2 {
                if bw == 0 {
                    &SILK_PITCH_OFFSET_NB10MS[rc.cdf(&SILK_MODEL_PITCH_CONTOUR_NB10MS)]
                } else {
                    &SILK_PITCH_OFFSET_MBWB10MS[rc.cdf(&SILK_MODEL_PITCH_CONTOUR_MBWB10MS)]
                }
            } else if bw == 0 {
                &SILK_PITCH_OFFSET_NB20MS[rc.cdf(&SILK_MODEL_PITCH_CONTOUR_NB20MS)]
            } else {
                &SILK_PITCH_OFFSET_MBWB20MS[rc.cdf(&SILK_MODEL_PITCH_CONTOUR_MBWB20MS)]
            };
            for i in 0..subframes {
                pitchlag[i] = (primarylag + offsets[i] as i32).clamp(SILK_PITCH_MIN_LAG[bw] as i32, SILK_PITCH_MAX_LAG[bw] as i32);
            }

            let ltpfilter = rc.cdf(&SILK_MODEL_LTP_FILTER);
            for i in 0..subframes {
                let (index, taps): (usize, &[[i8; 5]]) = match ltpfilter {
                    0 => (rc.cdf(&SILK_MODEL_LTP_FILTER0_SEL), &SILK_LTP_FILTER0_TAPS),
                    1 => (rc.cdf(&SILK_MODEL_LTP_FILTER1_SEL), &SILK_LTP_FILTER1_TAPS),
                    _ => (rc.cdf(&SILK_MODEL_LTP_FILTER2_SEL), &SILK_LTP_FILTER2_TAPS),
                };
                for j in 0..5 {
                    ltptaps[i][j] = taps[index][j] as f32 / 128.0;
                }
            }
        }

        let ltpscale = if voiced && frame_num == 0 {
            SILK_LTP_SCALE_FACTOR[rc.cdf(&SILK_MODEL_LTP_SCALE_INDEX)] as f32 / 16384.0
        } else {
            15565.0 / 16384.0
        };

        let mut residual = [0f32; MAX_LAG + HISTORY];
        self.decode_excitation(rc, &mut residual[MAX_LAG..], qoffset_high, active, voiced);

        if self.output_channels == ch || redundant {
            return;
        }

        let nlsf_interp = self.nlsf_interp_factor;
        let frame = &mut self.frame[ch];
        for i in 0..subframes {
            let lpc_coeff = if i < 2 && has_leadin { &leadin } else { &body };
            let dst = HISTORY + i * sflength;
            let res = MAX_LAG + i * sflength;
            let lpc = HISTORY + i * sflength;

            if voiced {
                let (out_end, scale) = if i < 2 || nlsf_interp == 4 {
                    (-((i * sflength) as isize), ltpscale)
                } else {
                    (-(((i - 2) * sflength) as isize), 1.0)
                };

                // re-whitening: the LPC coefficients may have changed since the history was produced
                let mut j = -(pitchlag[i] as isize) - (LTP_ORDER / 2) as isize;
                while j < out_end {
                    let mut sum = frame.output[at(dst, j)];
                    for k in 0..order {
                        sum -= lpc_coeff[k] * frame.output[at(dst, j - k as isize - 1)];
                    }
                    residual[at(res, j)] = sum.clamp(-1.0, 1.0) * scale / gains[i];
                    j += 1;
                }
                if out_end != 0 {
                    let rescale = gains[i - 1] / gains[i];
                    for j in out_end..0 {
                        residual[at(res, j)] *= rescale;
                    }
                }

                // long-term prediction
                for j in 0..sflength {
                    let mut sum = residual[res + j];
                    for k in 0..LTP_ORDER {
                        sum += ltptaps[i][k] * residual[at(res + j, -(pitchlag[i] as isize) + (LTP_ORDER / 2) as isize - k as isize)];
                    }
                    residual[res + j] = sum;
                }
            }

            // short-term prediction
            for j in 0..sflength {
                let mut sum = residual[res + j] * gains[i];
                for k in 1..=order {
                    sum += lpc_coeff[k - 1] * frame.lpc_history[lpc + j - k];
                }
                frame.lpc_history[lpc + j] = sum;
                frame.output[dst + j] = sum.clamp(-1.0, 1.0);
            }
        }

        frame.prev_voiced = voiced;
        let fl = self.flength;
        frame.lpc_history.copy_within(fl..fl + HISTORY, 0);
        frame.output.copy_within(fl..fl + HISTORY, 0);
        frame.coded = true;
    }

    fn unmix_ms(&mut self, l: &mut [f32], r: &mut [f32]) {
        let fl = self.flength;
        let mid = &self.frame[0].output;
        let side = &self.frame[1].output;
        let base = HISTORY - fl;
        let [w0_prev, w1_prev] = self.prev_stereo_weights;
        let [w0, w1] = self.stereo_weights;
        let n1 = SILK_STEREO_INTERP_LEN[self.bandwidth] as usize;
        let m = |i: isize| mid[at(base, i)];
        let s = |i: isize| side[at(base, i)];
        for i in 0..fl {
            let ii = i as isize;
            let (a, b) = if i < n1 {
                (w0_prev + i as f32 * (w0 - w0_prev) / n1 as f32, w1_prev + i as f32 * (w1 - w1_prev) / n1 as f32)
            } else {
                (w0, w1)
            };
            let p0 = 0.25 * (m(ii - 2) + 2.0 * m(ii - 1) + m(ii));
            l[i] = ((1.0 + b) * m(ii - 1) + s(ii - 1) + a * p0).clamp(-1.0, 1.0);
            r[i] = ((1.0 - b) * m(ii - 1) - s(ii - 1) - a * p0).clamp(-1.0, 1.0);
        }
        self.prev_stereo_weights = self.stereo_weights;
    }

    /// Decode the LP layer of one Opus frame (up to three SILK frames) into `output`, one slice per
    /// output channel, at the SILK rate. Returns the number of samples per channel.
    pub fn decode_superframe(&mut self, rc: &mut RangeDecoder, output: &mut [Vec<f32>; 2], bandwidth: usize, coded_channels: usize, duration_ms: usize) -> usize {
        let nb_frames = 1 + (duration_ms > 20) as usize + (duration_ms > 40) as usize;
        self.subframes = duration_ms / nb_frames / 5;
        self.sflength = 20 * (bandwidth + 2);
        self.flength = self.sflength * self.subframes;
        self.bandwidth = bandwidth;
        self.wb = bandwidth == 2;

        // flush the side channel when switching from mono to stereo
        if coded_channels > self.prev_coded_channels {
            self.frame[1].flush();
        }
        self.prev_coded_channels = coded_channels;

        let mut active = [[false; 6]; 2];
        let mut redundancy = [0u32; 2];
        for i in 0..coded_channels {
            for j in 0..nb_frames {
                active[i][j] = rc.log(1) != 0;
            }
            redundancy[i] = rc.log(1);
        }
        for i in 0..coded_channels {
            if redundancy[i] != 0 && duration_ms > 20 {
                redundancy[i] = rc.cdf(if duration_ms == 40 { &SILK_MODEL_LBRR_FLAGS_40[..] } else { &SILK_MODEL_LBRR_FLAGS_60[..] }) as u32;
            }
        }

        // the in-band redundancy frames are decoded to keep the range coder in step, then dropped
        for i in 0..nb_frames {
            for j in 0..coded_channels {
                if redundancy[j] & (1 << i) != 0 {
                    let active1 = !(j == 0 && redundancy[1] & (1 << i) == 0);
                    self.decode_frame(rc, i, j, coded_channels, true, active1, true);
                }
            }
            self.midonly = false;
        }

        let fl = self.flength;
        for i in 0..nb_frames {
            let mut j = 0;
            while j < coded_channels && !self.midonly {
                let active1 = if coded_channels > 1 { active[1][i] } else { false };
                self.decode_frame(rc, i, j, coded_channels, active[j][i], active1, false);
                j += 1;
            }
            // reset the side channel if it is not coded
            if self.midonly && self.frame[1].coded {
                self.frame[1].flush();
            }
            if coded_channels == 1 || self.output_channels == 1 {
                let src = HISTORY - fl - 2;
                for j in 0..self.output_channels {
                    output[j][i * fl..(i + 1) * fl].copy_from_slice(&self.frame[0].output[src..src + fl]);
                }
            } else {
                let [o0, o1] = output;
                self.unmix_ms(&mut o0[i * fl..(i + 1) * fl], &mut o1[i * fl..(i + 1) * fl]);
            }
            self.midonly = false;
        }
        nb_frames * fl
    }
}
