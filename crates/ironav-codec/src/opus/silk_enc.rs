//! A SILK encoder: the layer `silk.rs` decodes, for wideband (16 kHz) packets of one frame of 10 or
//! 20 ms or of two or three 20 ms frames, mono or stereo (mid and side, with the side's prediction
//! from the mid taken out), with in-band redundancy (LBRR) when asked: a coarser copy of each
//! active frame in the next packet.
//!
//! The analysis is the classic one: LPC by autocorrelation over a window centred on the frame,
//! turned into NLSFs and quantized with the two-stage codebook; an open-loop pitch search on the
//! LPC residual with the lag contour chosen per subframe; five-tap long-term predictors from the
//! three codebooks; and subframe gains set from the prediction residual and a step size that the
//! rate control moves. The excitation is then quantized in closed loop: the encoder runs the
//! decoder's own synthesis (the same re-whitening, the same long- and short-term filters, in the
//! same order) and picks each pulse so that the decoder's output follows the input, so the two
//! stay in step. There is no NLSF interpolation between frames.

use super::rc::{Coder, RangeEncoder};
use super::silk::{lsf2lpc, stabilize_lsf, HISTORY};
use super::tables::*;
use std::f64::consts::PI;

const ORDER: usize = 16;
/// A subframe: 5 ms at 16 kHz.
const SFL: usize = 80;
const MIN_LAG: i32 = 32;
const MAX_LAG: i32 = 288;
/// Residual the long-term predictor reaches back to, as in the decoder.
const RES_PAST: usize = 288 + 2;
/// Input samples the encoder sees past the end of a frame.
pub const LOOKAHEAD: usize = 40;
/// Input samples kept before a frame, for the pitch search and the residual's filter memory.
pub const PAST: usize = 352;
/// The smallest step a pulse stands for (about -72 dBFS), so quiet input costs few bits.
const STEP_FLOOR: f64 = 2.5e-4;
/// How far the noise feedback follows the envelope: the coding noise takes the shape of
/// `1 / A(z / NOISE_SHAPE)`, under the formants where they mask it, and the residual the pulses
/// code stays the prediction residual instead of growing with the predictor's own gain.
const NOISE_SHAPE: f32 = 0.94;
/// The share of the long-term prediction taken from the input rather than the decoder's
/// output: the coding noise then follows the pitch harmonics instead of being coded away.
const HARMONIC_SHAPE: f32 = 0.7;
/// The largest pulse: sixteen of them in a block still fit ten extra bits.
const MAX_PULSE: i32 = 1023;

/// The decoder's state for the one channel, kept in step with it.
#[derive(Clone)]
struct State {
    coded: bool,
    log_gain: i32,
    nlsf: [i16; 16],
    output: [f32; 2 * HISTORY],
    lpc_history: [f32; 2 * HISTORY],
    /// The encoder's own: the last output errors, newest last, for the noise feedback.
    err: [f32; ORDER],
    /// Whether the last frame was voiced, and its pitch lag: a frame after it in the same packet
    /// codes its lag as a step from there.
    voiced: bool,
    lag: i32,
}

impl State {
    fn new() -> Self {
        let mut nlsf = [0i16; 16];
        for (i, v) in nlsf.iter_mut().enumerate() {
            *v = ((i + 1) * 32768 / 17) as i16;
        }
        Self { coded: false, log_gain: 0, nlsf, output: [0.0; 2 * HISTORY], lpc_history: [0.0; 2 * HISTORY], err: [0.0; ORDER], voiced: false, lag: 0 }
    }
}

/// What a frame's analysis decided, before the gains and the excitation.
#[derive(Clone, Default)]
struct Analysis {
    subframes: usize,
    active: bool,
    voiced: bool,
    qoffset_high: usize,
    i1: usize,
    i2: [i32; 16],
    lpc: [f32; 16],
    nlsf: [i16; 16],
    lag: i32,
    contour: usize,
    lags: [i32; 4],
    periodicity: usize,
    ltp_index: [usize; 4],
    taps: [[f32; 5]; 4],
    /// The prediction residual's level in each subframe.
    rms: [f64; 4],
}

pub struct SilkEncoder {
    st: State,
    /// log2 of the step size relative to the residual: what the rate control moves.
    lk: f64,
    frames: u64,
    scratch: RangeEncoder,
    /// In-band redundancy, when on: how much coarser (in log2 of the step) a frame's copy is.
    fec: Option<f64>,
    /// The copies of the last packet's frames, for this one, and that packet's input; and the
    /// state the last copy made leaves a decoder in.
    lbrr: [Option<Lbrr>; 3],
    lbrr_x: Vec<f32>,
    lbrr_st: State,
    /// The state after the last packet sent, for discontinuous transmission.
    sent: State,
}

/// The most of a frame's bits its copy may take: a coarser copy is tried, then none. Less
/// starves the copies of 20 ms frames, more the frames of 10 ms (whose side information is
/// most of a copy).
const LBRR_SHARE: f64 = 0.6;

/// A frame's redundant copy, waiting for the next packet: the frame's analysis, the decoder's
/// state before it, the step scale and the frame's number.
#[derive(Clone)]
struct Lbrr {
    an: Analysis,
    st: State,
    lk: f64,
    frame: u64,
}

/// The cost in bits of `s` under `cdf`.
fn bits(cdf: &[u16], s: usize) -> f64 {
    let low = if s > 0 { cdf[s] } else { 0 };
    let p = cdf[s + 1].saturating_sub(low);
    if p == 0 {
        return 99.0;
    }
    (cdf[0] as f64 / p as f64).log2()
}

/// A subframe gain as the decoder computes it from its index.
fn lin_gain(log_gain: i32) -> f32 {
    let lg = ((log_gain * 0x1D1C71) >> 16) + 2090;
    let ipart = lg >> 7;
    let fpart = lg & 127;
    let lingain = (1 << ipart) + (((-174 * fpart * (128 - fpart)) >> 16) + fpart) * ((1 << ipart) >> 7);
    lingain as f32 / 65536.0
}

/// LPC coefficients (`x[n] ~ sum a[k] x[n-1-k]`) of a windowed stretch of signal, or none for silence.
fn lpc_analysis(x: &[f32]) -> Option<[f64; ORDER]> {
    let n = x.len();
    let w: Vec<f64> = (0..n).map(|i| x[i] as f64 * (PI * (i as f64 + 0.5) / n as f64).sin()).collect();
    let mut r = [0f64; ORDER + 1];
    for (k, rk) in r.iter_mut().enumerate() {
        *rk = (k..n).map(|i| w[i] * w[i - k]).sum();
    }
    if r[0] < 1e-9 {
        return None;
    }
    // a white-noise floor 40 dB down, and a lag window that widens the formants a little
    r[0] *= 1.0 + 1e-4;
    for (k, rk) in r.iter_mut().enumerate().skip(1) {
        let t = 2.0 * PI * 60.0 / 16_000.0 * k as f64;
        *rk *= (-0.5 * t * t).exp();
    }
    let mut a = [0f64; ORDER];
    let mut err = r[0];
    for i in 0..ORDER {
        let mut acc = r[i + 1];
        for j in 0..i {
            acc -= a[j] * r[i - j];
        }
        let k = acc / err;
        let prev = a;
        a[i] = k;
        for j in 0..i {
            a[j] = prev[j] - k * prev[i - 1 - j];
        }
        err *= 1.0 - k * k;
        if err <= 0.0 {
            return None;
        }
    }
    let mut c = 1.0;
    for v in a.iter_mut() {
        c *= 0.996;
        *v *= c;
    }
    Some(a)
}

/// The NLSFs (Q15, 0 to pi) of LPC coefficients: the zeros, between 0 and pi, of the real and
/// imaginary parts of `A(e^jw) e^(jw(order+1)/2)`, which are those of the sum and difference
/// polynomials.
fn a2nlsf(a: &[f64; ORDER]) -> Option<[i16; ORDER]> {
    let m = (ORDER as f64 + 1.0) / 2.0;
    let eval = |w: f64| {
        let (mut re, mut im) = (0.0, 0.0);
        for n in 0..=ORDER {
            let c = if n == 0 { 1.0 } else { -a[n - 1] };
            let ph = w * (m - n as f64);
            re += c * ph.cos();
            im += c * ph.sin();
        }
        (re, im)
    };
    const N: usize = 1024;
    let mut roots = Vec::with_capacity(ORDER);
    let w_at = |i: usize| PI * (i as f64 + 0.5) / (N as f64 + 1.0);
    let mut prev = eval(w_at(0));
    for i in 1..=N {
        let cur = eval(w_at(i));
        for part in 0..2 {
            let (p, c) = if part == 0 { (prev.0, cur.0) } else { (prev.1, cur.1) };
            if (p < 0.0) != (c < 0.0) {
                let (mut lo, mut hi, mut flo) = (w_at(i - 1), w_at(i), p);
                for _ in 0..30 {
                    let mid = 0.5 * (lo + hi);
                    let v = eval(mid);
                    let f = if part == 0 { v.0 } else { v.1 };
                    if (f < 0.0) == (flo < 0.0) {
                        lo = mid;
                        flo = f;
                    } else {
                        hi = mid;
                    }
                }
                roots.push(0.5 * (lo + hi));
            }
        }
        prev = cur;
    }
    if roots.len() != ORDER {
        return None;
    }
    roots.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let mut out = [0i16; ORDER];
    for (o, w) in out.iter_mut().zip(&roots) {
        *o = (w / PI * 32768.0).round().clamp(1.0, 32767.0) as i16;
    }
    Some(out)
}

/// The decoder's stage-2 residual for index `v`, before the prediction is added.
fn nlsf_dequant(v: i32) -> i32 {
    let mut r = v * 1024;
    if v < 0 {
        r += 102;
    } else if v > 0 {
        r -= 102;
    }
    (r * 9830) >> 16
}

/// The cost of stage-2 index `v` of coefficient `i` after stage-1 vector `i1`.
fn nlsf_bits(i1: usize, i: usize, v: i32) -> f64 {
    let model = &SILK_MODEL_LSF_S2[SILK_LSF_S2_MODEL_SEL_WB[i1][i] as usize];
    let s = (v + 4).clamp(0, 8) as usize;
    let mut b = bits(model, s);
    if v <= -4 {
        b += bits(&SILK_MODEL_LSF_S2_EXT, (-4 - v) as usize);
    } else if v >= 4 {
        b += bits(&SILK_MODEL_LSF_S2_EXT, (v - 4) as usize);
    }
    b
}

/// Quantizes `target` with the two-stage codebook, as the decoder will rebuild it: the stage-1
/// vector, the stage-2 indices, and the NLSFs they give.
fn quantize_nlsf(target: &[i16; ORDER], voiced: bool) -> (usize, [i32; ORDER], [i16; ORDER]) {
    // weights: the inverse distances to the neighbours (Laroia), so close pairs, the formants, count most
    let mut w = [0f64; ORDER];
    for i in 0..ORDER {
        let lo = if i == 0 { 0 } else { target[i - 1] as i32 };
        let hi = if i + 1 == ORDER { 32768 } else { target[i + 1] as i32 };
        let d0 = (target[i] as i32 - lo).max(64) as f64;
        let d1 = (hi - target[i] as i32).max(64) as f64;
        w[i] = 1.0 / d0 + 1.0 / d1;
    }
    let mu = 0.4;
    let s1 = &SILK_MODEL_LSF_S1[1][voiced as usize];
    // the stage-1 candidates closest to the target
    let mut first: Vec<(f64, usize)> = (0..32)
        .map(|i1| {
            let cb = &SILK_LSF_CODEBOOK_WB[i1];
            let e: f64 = (0..ORDER).map(|i| w[i] * (target[i] as f64 - cb[i] as f64 * 128.0).powi(2)).sum();
            (e, i1)
        })
        .collect();
    first.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut best = (f64::MAX, 0, [0i32; ORDER], [0i16; ORDER]);
    for &(_, i1) in first.iter().take(8) {
        let cb = &SILK_LSF_CODEBOOK_WB[i1];
        let weight = &SILK_MODEL_LSF_WEIGHT_WB[i1];
        let mut i2 = [0i32; ORDER];
        let mut res = [0i32; ORDER];
        let mut nlsf = [0i16; ORDER];
        let mut cost = mu * bits(s1, i1);
        for i in (0..ORDER).rev() {
            let pred = if i + 1 < ORDER {
                let pw = SILK_LSF_PRED_WEIGHTS_WB[SILK_LSF_WEIGHT_SEL_WB[i1][i] as usize][i] as i32;
                (res[i + 1] * pw) >> 8
            } else {
                0
            };
            let want = (target[i] as f64 - cb[i] as f64 * 128.0) * weight[i] as f64 / 16384.0;
            let guess = ((want - pred as f64) / (9830.0 * 1024.0 / 65536.0)).round() as i32;
            let mut pick = (f64::MAX, 0, 0, 0i16);
            for v in (guess - 1..=guess + 1).map(|v| v.clamp(-10, 10)) {
                let r = (nlsf_dequant(v) + pred) as i16 as i32;
                let value = (cb[i] as i32 * 128 + (r * 16384) / weight[i] as i32).clamp(0, 32767);
                let c = w[i] * (value - target[i] as i32).pow(2) as f64 + mu * nlsf_bits(i1, i, v);
                if c < pick.0 {
                    pick = (c, v, r, value as i16);
                }
            }
            cost += pick.0;
            (i2[i], res[i], nlsf[i]) = (pick.1, pick.2, pick.3);
        }
        if cost < best.0 {
            best = (cost, i1, i2, nlsf);
        }
    }
    let (_, i1, i2, mut nlsf) = best;
    stabilize_lsf(&mut nlsf, ORDER, &SILK_LSF_MIN_SPACING_WB);
    (i1, i2, nlsf)
}

/// `x` filtered by `1 - sum lpc[k] z^-(k+1)`, from index `from` (which must be at least the order).
fn whiten(x: &[f32], lpc: &[f32; 16], from: usize) -> Vec<f64> {
    let mut r = vec![0f64; x.len()];
    for n in from..x.len() {
        let mut s = x[n] as f64;
        for k in 0..ORDER {
            s -= lpc[k] as f64 * x[n - 1 - k] as f64;
        }
        r[n] = s;
    }
    r
}

impl SilkEncoder {
    pub fn new() -> Self {
        Self {
            st: State::new(),
            lk: 0.0,
            frames: 0,
            scratch: RangeEncoder::new(1275),
            fec: None,
            lbrr: Default::default(),
            lbrr_x: Vec::with_capacity(PAST + 3 * 4 * SFL + LOOKAHEAD),
            lbrr_st: State::new(),
            sent: State::new(),
        }
    }

    /// Notes the state after a packet that is sent, which [`rewind`](Self::rewind) goes back to.
    pub fn mark_sent(&mut self) {
        self.sent.clone_from(&self.st);
    }

    /// Starts again as a decoder does after CELT frames: from no history, with no copies.
    pub fn restart(&mut self) {
        self.st = State::new();
        self.sent = State::new();
        self.lbrr = Default::default();
    }

    /// Goes back to the state after the last packet sent, as a decoder that was sent nothing
    /// since (it conceals, and keeps its state) has it; with no copies for the next packet.
    pub fn rewind(&mut self) {
        self.st.clone_from(&self.sent);
        self.lbrr = Default::default();
    }

    /// In-band redundancy for `loss` percent of packets lost, or none for 0: each active frame
    /// is coded again, coarser, into the next packet, where a decoder that lost the frame's own
    /// packet finds it. The copy is libopus's 7 gain steps (of 1.37 dB) coarser, a step less
    /// for each 5 % of loss, and never less than 3.
    pub fn set_fec(&mut self, loss: u32) {
        self.fec = (loss > 0).then(|| (7.0 - 0.2 * loss as f64).max(3.0) * 1.365 / 6.02);
        if self.fec.is_none() {
            self.lbrr = Default::default();
        }
    }

    /// Codes `n` (1 to 3) frames of `subframes` (2 or 4) subframes each, at `x[PAST..]` (with
    /// `PAST` samples before them and `LOOKAHEAD` after), as one mono packet into `rc`, after the
    /// copies of the last packet's frames; `keep` keeps this packet's for the next. Aims at
    /// `target_bits` on average and never more than `max_bits`. Returns the bits used.
    pub fn encode(&mut self, rc: &mut RangeEncoder, x: &[f32], n: usize, subframes: usize, target_bits: f64, max_bits: i32, keep: bool) -> i32 {
        let fl = subframes * SFL;
        debug_assert_eq!(x.len(), PAST + n * fl + LOOKAHEAD);
        let ans: [Analysis; 3] = std::array::from_fn(|i| if i < n { self.analyze(&x[i * fl..PAST + (i + 1) * fl + LOOKAHEAD], subframes) } else { Analysis::default() });
        // the header: each frame's VAD flag, the redundancy flag, and which frames have a copy
        let mask = self.lbrr_mask(n);
        Self::write_flags(rc, &ans[..n], mask);
        Self::write_mask(rc, n, mask);
        let start = rc.tell();
        let mut chain = State::new();
        for i in 0..n {
            if mask >> i & 1 != 0 {
                self.write_lbrr(rc, i, mask, &mut chain);
            }
        }
        self.lbrr = Default::default();
        let spent = (rc.tell() - start) as f64;
        let mut used = rc.tell();
        for (i, an) in ans[..n].iter().enumerate() {
            // room left for the frames after this one
            let room = max_bits - 40 * (n - 1 - i) as i32;
            used = self.code_frame(rc, an, &x[i * fl..PAST + (i + 1) * fl + LOOKAHEAD], (target_bits - spent) / n as f64, room, i, keep);
        }
        self.keep_input(x);
        used
    }

    /// Which of the last packet's `n` frames have a copy, a bit each.
    fn lbrr_mask(&self, n: usize) -> u32 {
        (0..n).filter(|&i| self.lbrr[i].is_some()).fold(0, |m, i| m | 1 << i)
    }

    /// A channel's flags in the packet's header: each frame's VAD flag and the redundancy flag.
    fn write_flags(rc: &mut RangeEncoder, ans: &[Analysis], mask: u32) {
        for an in ans {
            rc.log(1, an.active as u32);
        }
        rc.log(1, (mask != 0) as u32);
    }

    /// Which of a 40 or 60 ms packet's frames have a copy, after every channel's flags.
    fn write_mask(rc: &mut RangeEncoder, n: usize, mask: u32) {
        if mask != 0 && n > 1 {
            rc.cdf(if n == 2 { &SILK_MODEL_LBRR_FLAGS_40[..] } else { &SILK_MODEL_LBRR_FLAGS_60[..] }, mask as usize);
        }
    }

    /// Codes the copy of the last packet's frame `i` as a decoder that lost that packet decodes
    /// it, in place of the frame: after the copy of the frame before it if there is one
    /// (`chain` holds the state it leaves), else from the state before the frame.
    fn write_lbrr(&self, rc: &mut RangeEncoder, i: usize, mask: u32, chain: &mut State) {
        let l = self.lbrr[i].as_ref().expect("a copy of the frame");
        let cond = i > 0 && mask >> (i - 1) & 1 != 0;
        if !cond {
            chain.clone_from(&l.st);
        }
        let fl = l.an.subframes * SFL;
        code(&l.an, l.lk, &self.lbrr_x[i * fl..PAST + (i + 1) * fl + LOOKAHEAD], rc, chain, l.frame, false, cond, !cond);
    }

    /// Keeps the packet's input for the copies of its frames, if there are any.
    fn keep_input(&mut self, x: &[f32]) {
        if self.lbrr.iter().any(Option::is_some) {
            self.lbrr_x.clear();
            self.lbrr_x.extend_from_slice(x);
        }
    }

    /// Codes frame `i` of a packet, which `an` describes, after what `rc` holds, aiming at
    /// `target_bits` for the frame on average and never more than `max_bits` in all; with `keep`
    /// and redundancy on, keeps its copy for the next packet. A frame after the first codes its
    /// gain and lag from the frame before. Returns the bits the range coder has used in all.
    #[allow(clippy::too_many_arguments)]
    fn code_frame(&mut self, rc: &mut RangeEncoder, an: &Analysis, x: &[f32], target_bits: f64, max_bits: i32, i: usize, keep: bool) -> i32 {
        let subframes = an.subframes;
        let start = rc.tell();
        let (cond, scale) = (i > 0 && self.st.coded, i == 0);
        let mut lk = self.lk;
        // over the limit, coarser steps until it fits, and at the last no pulses at all
        let mut used = 0;
        let mut silent = false;
        for tries in 1.. {
            self.scratch.clone_from(rc);
            let mut st = self.st.clone();
            used = code(an, lk, x, &mut self.scratch, &mut st, self.frames, silent, cond, scale);
            if used <= max_bits || silent {
                break;
            }
            if tries == 8 {
                silent = true;
            } else {
                lk += 1.0;
            }
        }
        let mut st = self.st.clone();
        let bits = code(an, lk, x, rc, &mut st, self.frames, silent, cond, scale);
        debug_assert_eq!(bits, used);
        let before = std::mem::replace(&mut self.st, st);
        if let (Some(d), true, true) = (self.fec, keep, an.active) {
            // the copy as the decoder will decode it, coarser until it takes no more than its
            // share; none if it never does
            let cond = i > 0 && self.lbrr[i - 1].is_some();
            for extra in 0..3 {
                self.scratch.reset(1275);
                let mut st = if cond { self.lbrr_st.clone() } else { before.clone() };
                let lk = lk + d + extra as f64;
                if (code(an, lk, x, &mut self.scratch, &mut st, self.frames, false, cond, !cond) - 1) as f64 <= LBRR_SHARE * target_bits {
                    self.lbrr[i] = Some(Lbrr { an: an.clone(), st: before, lk, frame: self.frames });
                    self.lbrr_st = st;
                    break;
                }
            }
        }
        if an.active {
            // the step that would have hit the target, approached slowly
            let per_doubling = 0.4 * (subframes * SFL) as f64;
            self.lk = (self.lk + 0.3 * ((bits - start) as f64 - target_bits) / per_doubling).clamp(-6.0, 5.0);
        }
        self.frames += 1;
        bits
    }

    fn analyze(&self, x: &[f32], subframes: usize) -> Analysis {
        let fl = subframes * SFL;
        let frame = &x[PAST..PAST + fl];
        let energy = frame.iter().map(|&v| v as f64 * v as f64).sum::<f64>() / fl as f64;
        let active = energy.sqrt() > 1e-3;

        // the spectral envelope, from a window centred on the frame
        let a = lpc_analysis(&x[PAST - LOOKAHEAD..PAST + fl + LOOKAHEAD]);
        let target = a.as_ref().and_then(a2nlsf).unwrap_or(self.st.nlsf);

        // pitch, open loop, on the residual of the unquantized envelope
        let lpc0 = a.map_or([0f32; 16], |a| {
            let mut l = [0f32; 16];
            for (o, v) in l.iter_mut().zip(a) {
                *o = v as f32;
            }
            l
        });
        let r0 = whiten(&x[..PAST + fl], &lpc0, ORDER);
        let nlags = (MAX_LAG - MIN_LAG + 1) as usize;
        // per subframe: its energy, and per lag the cross term and the lagged energy
        let mut e = [0f64; 4];
        let mut num = vec![[0f64; 4]; nlags];
        let mut el = vec![[0f64; 4]; nlags];
        for s in 0..subframes {
            let n0 = PAST + s * SFL;
            e[s] = (n0..n0 + SFL).map(|n| r0[n] * r0[n]).sum();
            for (li, l) in (MIN_LAG..=MAX_LAG).enumerate() {
                let l = l as usize;
                let (mut c, mut q) = (0.0, 0.0);
                for n in n0..n0 + SFL {
                    c += r0[n] * r0[n - l];
                    q += r0[n - l] * r0[n - l];
                }
                num[li][s] = c;
                el[li][s] = q;
            }
        }
        let corr = |li: usize| {
            let c: f64 = (0..subframes).map(|s| num[li][s]).sum();
            let q: f64 = (0..subframes).map(|s| el[li][s]).sum();
            let et: f64 = e[..subframes].iter().sum();
            c / (et * q).sqrt().max(1e-12)
        };
        let mut best = (0..nlags).max_by(|&a, &b| corr(a).partial_cmp(&corr(b)).unwrap()).unwrap();
        // prefer a period to its multiples
        for d in [4usize, 3, 2] {
            let l = (best as i32 + MIN_LAG) as usize / d;
            if l < MIN_LAG as usize {
                continue;
            }
            let lo = (l - 1).max(MIN_LAG as usize) - MIN_LAG as usize;
            let hi = (l + 1 - MIN_LAG as usize).min(nlags - 1);
            if let Some(c) = (lo..=hi).max_by(|&a, &b| corr(a).partial_cmp(&corr(b)).unwrap()) {
                if corr(c) > 0.85 * corr(best) {
                    best = c;
                    break;
                }
            }
        }
        let voiced = active && corr(best) > 0.4;

        // the envelope, quantized; the residual of what the decoder will use
        let (i1, i2, nlsf) = quantize_nlsf(&target, voiced);
        let lpc = lsf2lpc(&nlsf, ORDER);
        let r = whiten(&x[..PAST + fl], &lpc, ORDER);

        let mut an = Analysis {
            subframes,
            active,
            voiced,
            qoffset_high: if voiced { 0 } else { 1 },
            i1,
            i2,
            lpc,
            nlsf,
            lag: 0,
            contour: 0,
            lags: [0; 4],
            periodicity: 0,
            ltp_index: [0; 4],
            taps: [[0.0; 5]; 4],
            rms: [0.0; 4],
        };
        if voiced {
            // the primary lag and the contour that best follow the subframes
            let offsets: &[[i8; 4]] = &SILK_PITCH_OFFSET_MBWB20MS;
            let offsets10: &[[i8; 2]] = &SILK_PITCH_OFFSET_MBWB10MS;
            let ncont = if subframes == 4 { offsets.len() } else { offsets10.len() };
            let centre = best as i32 + MIN_LAG;
            let mut pick = (f64::MIN, 0, 0);
            for p0 in (centre - 3).max(MIN_LAG)..=(centre + 3).min(MAX_LAG - 1) {
                for c in 0..ncont {
                    let mut score = 0.0;
                    for s in 0..subframes {
                        let off = if subframes == 4 { offsets[c][s] } else { offsets10[c][s] } as i32;
                        let li = ((p0 + off).clamp(MIN_LAG, MAX_LAG) - MIN_LAG) as usize;
                        score += num[li][s] / (e[s] * el[li][s]).sqrt().max(1e-12);
                    }
                    if score > pick.0 {
                        pick = (score, p0, c);
                    }
                }
            }
            an.lag = pick.1;
            an.contour = pick.2;
            for s in 0..subframes {
                let off = if subframes == 4 { offsets[an.contour][s] } else { offsets10[an.contour][s] } as i32;
                an.lags[s] = (an.lag + off).clamp(MIN_LAG, MAX_LAG);
            }

            // five-tap predictors: per codebook the best entry for each subframe, then the best codebook
            let books: [&[[i8; 5]]; 3] = [&SILK_LTP_FILTER0_TAPS, &SILK_LTP_FILTER1_TAPS, &SILK_LTP_FILTER2_TAPS];
            let sels: [&[u16]; 3] = [&SILK_MODEL_LTP_FILTER0_SEL, &SILK_MODEL_LTP_FILTER1_SEL, &SILK_MODEL_LTP_FILTER2_SEL];
            let mut stats = [([[0f64; 5]; 5], [0f64; 5], 0f64); 4];
            for s in 0..subframes {
                let n0 = PAST + s * SFL;
                let l = an.lags[s] as usize;
                let (rr, c, tt) = &mut stats[s];
                for n in n0..n0 + SFL {
                    let h: [f64; 5] = std::array::from_fn(|k| r[n + 2 - l - k]);
                    *tt += r[n] * r[n];
                    for i in 0..5 {
                        c[i] += r[n] * h[i];
                        for j in 0..5 {
                            rr[i][j] += h[i] * h[j];
                        }
                    }
                }
            }
            let mut choice = (f64::MAX, 0, [0usize; 4]);
            for (p, book) in books.iter().enumerate() {
                let mut total = bits(&SILK_MODEL_LTP_FILTER, p);
                let mut idx = [0usize; 4];
                for s in 0..subframes {
                    let (rr, c, tt) = &stats[s];
                    let mut sub = (f64::MAX, 0);
                    for (k, taps) in book.iter().enumerate() {
                        let b: [f64; 5] = std::array::from_fn(|i| taps[i] as f64 / 128.0);
                        let mut err = *tt;
                        for i in 0..5 {
                            err -= 2.0 * b[i] * c[i];
                            for j in 0..5 {
                                err += b[i] * rr[i][j] * b[j];
                            }
                        }
                        // the error in bits: half the log of the energy ratio per sample
                        let cost = 0.5 * SFL as f64 * ((err.max(1e-9 * tt.max(1e-12))) / tt.max(1e-12)).log2() + bits(sels[p], k);
                        if cost < sub.0 {
                            sub = (cost, k);
                        }
                    }
                    total += sub.0;
                    idx[s] = sub.1;
                }
                if total < choice.0 {
                    choice = (total, p, idx);
                }
            }
            an.periodicity = choice.1;
            an.ltp_index = choice.2;
            for s in 0..subframes {
                let taps = books[an.periodicity][an.ltp_index[s]];
                an.taps[s] = std::array::from_fn(|k| taps[k] as f32 / 128.0);
            }
        }

        // the residual's level per subframe, after the long-term prediction when there is one
        for s in 0..subframes {
            let n0 = PAST + s * SFL;
            let mut sum = 0.0;
            for n in n0..n0 + SFL {
                let mut v = r[n];
                if voiced {
                    let l = an.lags[s] as usize;
                    for k in 0..5 {
                        v -= an.taps[s][k] as f64 * r[n + 2 - l - k];
                    }
                }
                sum += v * v;
            }
            an.rms[s] = (sum / SFL as f64).sqrt();
        }
        an
    }
}

impl Default for SilkEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// The stereo weight nearest `w`: its interval of the table and step within it (the symbols),
/// and its value in Q13.
fn quantize_weight(w: f64) -> (usize, usize, i32) {
    let mut best = (0, 0, 0, f64::MAX);
    for wi in 0..15 {
        let (a, b) = (SILK_STEREO_WEIGHTS[wi] as i32, SILK_STEREO_WEIGHTS[wi + 1] as i32);
        for ws in 0..5 {
            let q = a + (((b - a) * 6554) >> 16) * (2 * ws as i32 + 1);
            let d = (q as f64 / 8192.0 - w).abs();
            if d < best.3 {
                best = (wi, ws, q, d);
            }
        }
    }
    (best.0, best.1, best.2)
}

/// The second channel of a stereo stream. Left and right are coded as mid (their mean) and side
/// (half their difference); the decoder predicts the side from the mid with two weights, on the
/// mid's low-passed part and on the mid itself, interpolated from the last frame's over the
/// first 8 ms, and this codes what is left. A side with nothing left is not coded at all.
pub struct SilkStereo {
    side: SilkEncoder,
    /// The last frame's weights as the decoder holds them: on the low-passed mid, on the mid.
    prev: [f32; 2],
    /// The side's residual before the next packet, `PAST` samples, and the side encoder's input.
    res: Vec<f32>,
    x: Vec<f32>,
    /// The weights' symbols of the last packet's frames, for their copies.
    lbrr_w: [Weights; 3],
    /// The weights after the last packet sent, for discontinuous transmission.
    sent: [f32; 2],
}

/// A frame's stereo weights: each as its interval of the table, step within it and value (Q13).
type Weights = [(usize, usize, i32); 2];

impl Default for SilkStereo {
    fn default() -> Self {
        Self::new()
    }
}

impl SilkStereo {
    pub fn new() -> Self {
        Self { side: SilkEncoder::new(), prev: [0.0; 2], res: vec![0.0; PAST], x: Vec::with_capacity(PAST + 3 * 4 * SFL + LOOKAHEAD), lbrr_w: [[(0, 0, 0); 2]; 3], sent: [0.0; 2] }
    }

    /// [`SilkEncoder::mark_sent`] for the side and the weights.
    pub fn mark_sent(&mut self) {
        self.side.mark_sent();
        self.sent = self.prev;
    }

    /// [`SilkEncoder::restart`] for the side and the weights.
    pub fn restart(&mut self) {
        self.side.restart();
        (self.prev, self.sent) = ([0.0; 2], [0.0; 2]);
        self.res.fill(0.0);
    }

    /// [`SilkEncoder::rewind`] for the side and the weights.
    pub fn rewind(&mut self) {
        self.side.rewind();
        self.prev = self.sent;
    }

    /// In-band redundancy for the side, as [`SilkEncoder::set_fec`].
    pub fn set_fec(&mut self, loss: u32) {
        self.side.set_fec(loss);
    }

    fn write_weights(rc: &mut RangeEncoder, q: &Weights) {
        rc.cdf(&SILK_MODEL_STEREO_S1, 5 * (q[0].0 / 3) + q[1].0 / 3);
        rc.cdf(&SILK_MODEL_STEREO_S2, q[0].0 % 3);
        rc.cdf(&SILK_MODEL_STEREO_S3, q[0].1);
        rc.cdf(&SILK_MODEL_STEREO_S2, q[1].0 % 3);
        rc.cdf(&SILK_MODEL_STEREO_S3, q[1].1);
    }

    /// Codes a stereo packet of `n` frames into `rc`, the mid with `mid_enc`: `m` and `s` hold the
    /// mid and the side as [`SilkEncoder::encode`] takes a mono packet. Returns the bits used.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(&mut self, mid_enc: &mut SilkEncoder, rc: &mut RangeEncoder, m: &[f32], s: &[f32], n: usize, subframes: usize, target_bits: f64, max_bits: i32, keep: bool) -> i32 {
        let fl = subframes * SFL;
        let len = n * fl + LOOKAHEAD;
        debug_assert!(m.len() == PAST + len && s.len() == PAST + len);
        // the low-passed signal around a sample, as the decoder takes it
        let lp = |x: &[f32], i: usize| 0.25 * (x[i - 1] as f64 + 2.0 * x[i] as f64 + x[(i + 1).min(x.len() - 1)] as f64);
        let interp = SILK_STEREO_INTERP_LEN[2] as usize;
        let as_decoded = |q: &Weights| [(q[0].2 - q[1].2) as f32 / 8192.0, q[1].2 as f32 / 8192.0];
        let mut q = [[(0, 0, 0); 2]; 3];
        let mut mid_only = [false; 3];
        let mut share = [0.0; 3];
        self.x.clear();
        self.x.extend_from_slice(&self.res);
        let mut prev = self.prev;
        for f in 0..n {
            let at = PAST + f * fl;
            // the weights, each band on its own: the side's low-passed part from the mid's, and
            // the high-passed rests (a fit of both at once runs away where the mid has little
            // treble)
            let (mut ll, mut hh, mut sl, mut sh, mut em) = (1e-9, 1e-9, 0.0, 0.0, 0.0);
            for i in at..at + fl {
                let (l, sl_) = (lp(m, i), lp(s, i));
                let (h, sh_) = (m[i] as f64 - l, s[i] as f64 - sl_);
                (ll, hh) = (ll + l * l, hh + h * h);
                (sl, sh) = (sl + sl_ * l, sh + sh_ * h);
                em += m[i] as f64 * m[i] as f64;
            }
            // what the weights leave of the side, as the decoder puts it back, from sample `k` of
            // the frame: it leaves the decoder as sample k + 1, where the weights are those
            let residual = |cur: [f32; 2], k: usize| {
                let j = k + 1;
                let (a, b) = if j < interp {
                    let t = j as f32 / interp as f32;
                    (prev[0] + t * (cur[0] - prev[0]), prev[1] + t * (cur[1] - prev[1]))
                } else {
                    (cur[0], cur[1])
                };
                s[at + k] - a * lp(m, at + k) as f32 - b * m[at + k]
            };
            let left = |cur: [f32; 2]| (0..fl).map(|k| (residual(cur, k) as f64).powi(2)).sum::<f64>();
            // no prediction when that leaves less
            let fit = [quantize_weight(sl / ll), quantize_weight(sh / hh)];
            let zero = [quantize_weight(0.0), quantize_weight(0.0)];
            let (ef, ez) = (left(as_decoded(&fit)), left(as_decoded(&zero)));
            let (w, er) = if ez < ef { (zero, ez) } else { (fit, ef) };
            let cur = as_decoded(&w);
            let last = if f + 1 == n { fl + LOOKAHEAD } else { fl };
            for k in 0..last {
                self.x.push(residual(cur, k));
            }
            // nothing left of the side (a mono or panned signal): the mid alone
            mid_only[f] = er < 1e-4 * em || er < 1e-7 * fl as f64;
            // the side's share of the bits by how much of it is left
            share[f] = if mid_only[f] { 0.0 } else { (0.08 + 0.42 * (er / em.max(1e-9)).sqrt()).min(0.5) };
            q[f] = w;
            prev = cur;
        }
        let frame = |f: usize| (f * fl, PAST + (f + 1) * fl + LOOKAHEAD);
        let man: [Analysis; 3] = std::array::from_fn(|f| if f < n { let (a, b) = frame(f); mid_enc.analyze(&m[a..b], subframes) } else { Analysis::default() });
        let san: [Analysis; 3] = std::array::from_fn(|f| if f < n { let (a, b) = frame(f); self.side.analyze(&self.x[a..b], subframes) } else { Analysis::default() });
        let side_vad: [bool; 3] = std::array::from_fn(|f| f < n && !mid_only[f] && san[f].active);

        // the header: each channel's flags, then which frames have copies
        let (mm, sm) = (mid_enc.lbrr_mask(n), self.side.lbrr_mask(n) & mid_enc.lbrr_mask(n));
        SilkEncoder::write_flags(rc, &man[..n], mm);
        for &v in &side_vad[..n] {
            rc.log(1, v as u32);
        }
        rc.log(1, (sm != 0) as u32);
        SilkEncoder::write_mask(rc, n, mm);
        SilkEncoder::write_mask(rc, n, sm);
        // the copies: the mid with its weights (and the side left out when it has none), then
        // the side
        let start = rc.tell();
        let (mut cm, mut cs) = (State::new(), State::new());
        for f in 0..n {
            if mm >> f & 1 != 0 {
                Self::write_weights(rc, &self.lbrr_w[f]);
                if sm >> f & 1 == 0 {
                    rc.cdf(&SILK_MODEL_MID_ONLY, 1);
                }
                mid_enc.write_lbrr(rc, f, mm, &mut cm);
                if sm >> f & 1 != 0 {
                    self.side.write_lbrr(rc, f, sm, &mut cs);
                }
            }
        }
        mid_enc.lbrr = Default::default();
        self.side.lbrr = Default::default();
        let spent = (rc.tell() - start) as f64;
        // the frames
        let per_frame = (target_bits - spent) / n as f64;
        let mut used = rc.tell();
        for f in 0..n {
            Self::write_weights(rc, &q[f]);
            if !side_vad[f] {
                rc.cdf(&SILK_MODEL_MID_ONLY, mid_only[f] as usize);
            }
            let room = max_bits - 60 * (n - 1 - f) as i32;
            let reserve = if mid_only[f] { 0 } else { (per_frame * share[f] * 0.5).max(24.0) as i32 };
            let (a, b) = frame(f);
            used = mid_enc.code_frame(rc, &man[f], &m[a..b], per_frame * (1.0 - share[f]), room - reserve, f, keep);
            if mid_only[f] {
                // the decoder resets its side channel
                self.side.st = State::new();
            } else {
                let (a, b) = frame(f);
                used = self.side.code_frame(rc, &san[f], &self.x[a..b], per_frame * share[f], room, f, keep);
            }
            // a copy of the side goes only with one of the mid
            if mid_enc.lbrr[f].is_none() {
                self.side.lbrr[f] = None;
            }
            self.lbrr_w[f] = q[f];
        }
        mid_enc.keep_input(m);
        let x = std::mem::take(&mut self.x);
        self.side.keep_input(&x);
        self.res.copy_from_slice(&x[n * fl..n * fl + PAST]);
        self.x = x;
        self.prev = prev;
        used
    }
}

/// Chooses the gains with step scale `2^lk`, quantizes the excitation (of the frame at
/// `x[PAST..]`) against the decoder's state
/// `st` (which it advances), and writes frame number `frames` to `rc`. Returns the bits used.
/// `silent` codes no pulses, which always fits.
///
/// `cond` codes the frame after the one before it in the packet (its first gain as a step, its lag
/// as a step when near), and `scale` writes the long-term predictor's scale (the first frame).
#[allow(clippy::too_many_arguments)]
fn code(an: &Analysis, lk: f64, x: &[f32], rc: &mut RangeEncoder, st: &mut State, frames: u64, silent: bool, cond: bool, scale: bool) -> i32 {
    let subframes = an.subframes;
    let fl = subframes * SFL;
    let (active, voiced) = (an.active, an.voiced);
    let (prev_voiced, prev_lag) = (st.voiced, st.lag);

    // the gains: subframe 0 absolute, the rest relative to the one before
    let mut gain_syms = [0usize; 4];
    let mut gains = [0f32; 4];
    let first_abs = !cond;
    for s in 0..subframes {
        let step = (2f64.powf(lk) * an.rms[s]).max(STEP_FLOOR);
        let want = step * 32768.0;
        let target = (0..64).min_by(|&a, &b| {
            let da = (lin_gain(a) as f64 / want).ln().abs();
            let db = (lin_gain(b) as f64 / want).ln().abs();
            da.partial_cmp(&db).unwrap()
        });
        let target = target.unwrap_or(0);
        let lg = if s == 0 && first_abs {
            gain_syms[0] = target as usize;
            if st.coded { target.max(st.log_gain - 16) } else { target }
        } else {
            let delta = (0..=40)
                .min_by_key(|&d| (((d << 1) - 16).max(st.log_gain + d - 4).clamp(0, 63) - target).abs())
                .unwrap();
            gain_syms[s] = delta as usize;
            ((delta << 1) - 16).max(st.log_gain + delta - 4).clamp(0, 63)
        };
        st.log_gain = lg;
        gains[s] = lin_gain(lg);
    }

    let ltpscale = if voiced { SILK_LTP_SCALE_FACTOR[0] as f32 / 16384.0 } else { 15565.0 / 16384.0 };
    let qoff = SILK_QUANT_OFFSET[voiced as usize][an.qoffset_high] as i32;
    let seed0 = (frames & 3) as u32;
    let mut seed = seed0;
    let mut pulses = [0i32; 320];
    let lpc = &an.lpc;
    let mut residual = [0f32; RES_PAST + 320];
    let mut shape = [0f32; ORDER];
    let mut c = 1.0;
    for (sh, a) in shape.iter_mut().zip(lpc) {
        c *= NOISE_SHAPE;
        *sh = a * c;
    }
    for i in 0..subframes {
        let dst = HISTORY + i * SFL;
        let res = RES_PAST + i * SFL;
        let lag = an.lags[i] as isize;
        if voiced {
            // the decoder's re-whitening of its own output, for the long-term predictor's history
            let out_end = -((i * SFL) as isize);
            let mut j = -lag - 2;
            while j < out_end {
                let at = |o: isize| (dst as isize + o) as usize;
                let mut sum = st.output[at(j)];
                for k in 0..ORDER {
                    sum -= lpc[k] * st.output[at(j - k as isize - 1)];
                }
                residual[(res as isize + j) as usize] = sum.clamp(-1.0, 1.0) * ltpscale / gains[i];
                j += 1;
            }
            if out_end != 0 {
                let rescale = gains[i - 1] / gains[i];
                for j in out_end..0 {
                    residual[(res as isize + j) as usize] *= rescale;
                }
            }
        }
        let g = gains[i];
        // the input's own residual in this subframe's units, for the open-loop share of the
        // long-term prediction
        let mut rt = [0f32; RES_PAST + SFL + 4];
        if voiced {
            let first = (i * SFL) as isize - lag - 4;
            let end = (i * SFL + SFL) as isize;
            for (m, v) in rt.iter_mut().enumerate().take((end - first) as usize) {
                let n = (PAST as isize + first + m as isize) as usize;
                let mut s = x[n];
                for k in 0..ORDER {
                    s -= lpc[k] * x[n - 1 - k];
                }
                *v = s / g;
            }
        }
        let rt_at = |rel: isize| rt[(rel - ((i * SFL) as isize - lag - 4)) as usize];
        for j in 0..SFL {
            let back = |k: usize| (res as isize + j as isize - lag + 2 - k as isize) as usize;
            let mut ltp = 0f32;
            if voiced {
                let rel = (i * SFL + j) as isize - lag + 2;
                let (mut closed, mut open) = (0f32, 0f32);
                for k in 0..5 {
                    closed += an.taps[i][k] * residual[back(k)];
                    open += an.taps[i][k] * rt_at(rel - k as isize);
                }
                ltp = (1.0 - HARMONIC_SHAPE) * closed + HARMONIC_SHAPE * open;
            }
            let mut lpcp = 0f32;
            for k in 1..=ORDER {
                lpcp += lpc[k - 1] * st.lpc_history[dst + j - k];
            }
            let mut fb = 0f32;
            for k in 0..ORDER {
                fb += shape[k] * st.err[ORDER - 1 - k];
            }
            let want = ((x[PAST + i * SFL + j] + fb - lpcp) / g - ltp) as f64 * 8_388_608.0;
            seed = seed.wrapping_mul(196314165).wrapping_add(907633515);
            let flip = seed & 0x8000_0000 != 0;
            let t = if flip { -want } else { want };
            let level = |v: i32| (v * 256 + qoff - 20 * v.signum()) as f64;
            let guess = ((t - qoff as f64) / 256.0).round().clamp(-(MAX_PULSE as f64), MAX_PULSE as f64) as i32;
            let mut v = guess;
            for c in [guess - 1, guess + 1] {
                if c.abs() <= MAX_PULSE && (level(c) - t).abs() < (level(v) - t).abs() {
                    v = c;
                }
            }
            if silent {
                v = 0;
            }
            pulses[i * SFL + j] = v;
            seed = seed.wrapping_add(v as u32);
            let mut e = (v * 256) | qoff;
            if v < 0 {
                e += 20;
            } else if v > 0 {
                e -= 20;
            }
            if flip {
                e = -e;
            }
            // the decoder's arithmetic, in its order
            let mut sum = e as f32 / 8_388_608.0;
            if voiced {
                for k in 0..5 {
                    sum += an.taps[i][k] * residual[back(k)];
                }
            }
            residual[res + j] = sum;
            let mut out = residual[res + j] * g;
            for k in 1..=ORDER {
                out += lpc[k - 1] * st.lpc_history[dst + j - k];
            }
            st.lpc_history[dst + j] = out;
            st.output[dst + j] = out.clamp(-1.0, 1.0);
            st.err.copy_within(1.., 0);
            st.err[ORDER - 1] = out - x[PAST + i * SFL + j];
        }
    }
    st.lpc_history.copy_within(fl..fl + HISTORY, 0);
    st.output.copy_within(fl..fl + HISTORY, 0);
    st.coded = true;
    st.nlsf = an.nlsf;
    st.voiced = voiced;
    if voiced {
        st.lag = an.lag;
    }

    // the bitstream, in the decoder's order (after the header the caller writes)
    if active {
        rc.cdf(&SILK_MODEL_FRAME_TYPE_ACTIVE, (voiced as usize) << 1 | an.qoffset_high);
    } else {
        rc.cdf(&SILK_MODEL_FRAME_TYPE_INACTIVE, an.qoffset_high);
    }
    for s in 0..subframes {
        if s == 0 && first_abs {
            let g = gain_syms[0];
            rc.cdf(&SILK_MODEL_GAIN_HIGHBITS[active as usize + voiced as usize], g >> 3);
            rc.cdf(&SILK_MODEL_GAIN_LOWBITS, g & 7);
        } else {
            rc.cdf(&SILK_MODEL_GAIN_DELTA, gain_syms[s]);
        }
    }
    rc.cdf(&SILK_MODEL_LSF_S1[1][voiced as usize], an.i1);
    for i in 0..ORDER {
        let v = an.i2[i];
        let model = &SILK_MODEL_LSF_S2[SILK_LSF_S2_MODEL_SEL_WB[an.i1][i] as usize];
        rc.cdf(model, (v + 4).clamp(0, 8) as usize);
        if v <= -4 {
            rc.cdf(&SILK_MODEL_LSF_S2_EXT, (-4 - v) as usize);
        } else if v >= 4 {
            rc.cdf(&SILK_MODEL_LSF_S2_EXT, (v - 4) as usize);
        }
    }
    if subframes == 4 {
        // no interpolation with the previous frame's envelope
        rc.cdf(&SILK_MODEL_LSF_INTERPOLATION_OFFSET, 4);
    }
    if voiced {
        // after a voiced frame of the packet, a step from its lag when near (0 is the escape)
        let step = an.lag - prev_lag + 9;
        let relative = cond && prev_voiced;
        if relative {
            rc.cdf(&SILK_MODEL_PITCH_DELTA, if (1..=20).contains(&step) { step as usize } else { 0 });
        }
        if !relative || !(1..=20).contains(&step) {
            let rel = an.lag - MIN_LAG;
            rc.cdf(&SILK_MODEL_PITCH_HIGHBITS, (rel / 8) as usize);
            rc.cdf(&SILK_MODEL_GAIN_LOWBITS, (rel % 8) as usize);
        }
        if subframes == 4 {
            rc.cdf(&SILK_MODEL_PITCH_CONTOUR_MBWB20MS, an.contour);
        } else {
            rc.cdf(&SILK_MODEL_PITCH_CONTOUR_MBWB10MS, an.contour);
        }
        rc.cdf(&SILK_MODEL_LTP_FILTER, an.periodicity);
        let sel: &[u16] = match an.periodicity {
            0 => &SILK_MODEL_LTP_FILTER0_SEL,
            1 => &SILK_MODEL_LTP_FILTER1_SEL,
            _ => &SILK_MODEL_LTP_FILTER2_SEL,
        };
        for s in 0..subframes {
            rc.cdf(sel, an.ltp_index[s]);
        }
        if scale {
            rc.cdf(&SILK_MODEL_LTP_SCALE_INDEX, 0);
        }
    }
    rc.cdf(&SILK_MODEL_LCG_SEED, seed0 as usize);
    write_excitation(rc, &pulses[..fl], active, voiced, an.qoffset_high);
    rc.tell()
}

/// Codes the pulses as the decoder reads them: a rate level, per block of 16 the pulse count
/// (with escapes for extra low bits), the positions by halving, the low bits, and the signs.
fn write_excitation(rc: &mut RangeEncoder, pulses: &[i32], active: bool, voiced: bool, qoffset_high: usize) {
    let blocks = pulses.len() / 16;
    let mut lsb = [0u32; 20];
    let mut count = [0usize; 20];
    for b in 0..blocks {
        let m = &pulses[16 * b..16 * b + 16];
        let mut l = 0;
        while l < 10 && m.iter().map(|v| (v.unsigned_abs() >> l) as usize).sum::<usize>() > 16 {
            l += 1;
        }
        lsb[b] = l;
        count[b] = m.iter().map(|v| (v.unsigned_abs() >> l) as usize).sum();
    }
    // the cheapest rate level for the counts (escapes go to the same model at every level)
    let level = (0..9)
        .min_by(|&a, &b| {
            let cost = |r: usize| bits(&SILK_MODEL_EXC_RATE[voiced as usize], r) + (0..blocks).map(|b| bits(&SILK_MODEL_PULSE_COUNT[r], if lsb[b] > 0 { 17 } else { count[b] })).sum::<f64>();
            cost(a).partial_cmp(&cost(b)).unwrap()
        })
        .unwrap();
    rc.cdf(&SILK_MODEL_EXC_RATE[voiced as usize], level);
    for b in 0..blocks {
        if lsb[b] == 0 {
            rc.cdf(&SILK_MODEL_PULSE_COUNT[level], count[b]);
        } else {
            rc.cdf(&SILK_MODEL_PULSE_COUNT[level], 17);
            for _ in 1..lsb[b].min(10) {
                rc.cdf(&SILK_MODEL_PULSE_COUNT[9], 17);
            }
            if lsb[b] == 10 {
                rc.cdf(&SILK_MODEL_PULSE_COUNT[9], 17);
                rc.cdf(&SILK_MODEL_PULSE_COUNT[10], count[b]);
            } else {
                rc.cdf(&SILK_MODEL_PULSE_COUNT[9], count[b]);
            }
        }
    }
    let split = |rc: &mut RangeEncoder, model: usize, left: usize, total: usize| {
        if total != 0 {
            let off = ((total - 1 + 5) * (total - 1)) >> 1;
            rc.cdf(&SILK_MODEL_PULSE_LOCATION[model][off..], left);
        }
    };
    for b in 0..blocks {
        if count[b] == 0 {
            continue;
        }
        let m: [usize; 16] = std::array::from_fn(|i| (pulses[16 * b + i].unsigned_abs() >> lsb[b]) as usize);
        let sum = |a: usize, n: usize| m[a..a + n].iter().sum::<usize>();
        split(rc, 0, sum(0, 8), count[b]);
        for h in 0..2 {
            split(rc, 1, sum(8 * h, 4), sum(8 * h, 8));
            for q in 0..2 {
                let at = 8 * h + 4 * q;
                split(rc, 2, sum(at, 2), sum(at, 4));
                for p in 0..2 {
                    split(rc, 3, m[at + 2 * p], sum(at + 2 * p, 2));
                }
            }
        }
    }
    for (i, v) in pulses.iter().enumerate() {
        let l = lsb[i >> 4];
        for k in (0..l).rev() {
            rc.cdf(&SILK_MODEL_EXCITATION_LSB, ((v.unsigned_abs() >> k) & 1) as usize);
        }
    }
    for (i, &v) in pulses.iter().enumerate() {
        if v != 0 {
            let model = &SILK_MODEL_EXCITATION_SIGN[active as usize + voiced as usize][qoffset_high][count[i >> 4].min(6)];
            rc.cdf(model, (v > 0) as usize);
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::rc::RangeDecoder;
    use super::super::silk::Silk;
    use super::*;

    /// Something like speech at 16 kHz: voiced stretches (a pulse train whose pitch glides,
    /// through two formants) between hissed ones and pauses.
    pub fn speechlike(n: usize, rate: f64) -> Vec<f32> {
        let mut out = vec![0f32; n];
        let (mut y1, mut y2, mut z1, mut z2) = (0f64, 0f64, 0f64, 0f64);
        let mut phase = 0.0;
        let mut s = 7u32;
        let res = |f: f64, bw: f64| {
            let r = (-PI * bw / rate).exp();
            (2.0 * r * (2.0 * PI * f / rate).cos(), -r * r)
        };
        let (a1, a2) = res(700.0, 80.0);
        let (b1, b2) = res(1800.0, 120.0);
        for (i, o) in out.iter_mut().enumerate() {
            let t = i as f64 / rate;
            let seg = (t * 2.5) as usize % 5;
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = (s >> 9) as f64 / (1 << 23) as f64 - 0.5;
            let src = match seg {
                0 | 1 | 3 => {
                    let f0 = 110.0 + 40.0 * (t * 1.3).sin();
                    phase += f0 / rate;
                    if phase >= 1.0 {
                        phase -= 1.0;
                        1.0
                    } else {
                        0.0
                    }
                }
                2 => 0.15 * noise,
                _ => 0.0,
            };
            let y = src + a1 * y1 + a2 * y2;
            (y2, y1) = (y1, y);
            let z = y + b1 * z1 + b2 * z2;
            (z2, z1) = (z1, z);
            *o = z as f32;
        }
        let peak = out.iter().fold(0f32, |m, v| m.max(v.abs()));
        for o in out.iter_mut() {
            *o *= 0.5 / peak;
        }
        out
    }

    /// Codes `x` (16 kHz) frame by frame and decodes it with the SILK decoder: the SNR in dB and
    /// the bit rate.
    pub fn silk_round_trip(x: &[f32], subframes: usize, bitrate: f64) -> (f64, f64) {
        let fl = subframes * SFL;
        let mut buf = vec![0f32; PAST];
        buf.extend_from_slice(x);
        buf.resize(PAST + x.len() + fl + LOOKAHEAD, 0.0);
        let mut enc = SilkEncoder::new();
        let mut dec = Silk::new(1);
        let mut out = [vec![0f32; 320], vec![0f32; 320]];
        let mut y = Vec::new();
        let mut total = 0usize;
        let frames = x.len() / fl;
        for f in 0..frames {
            let mut rc = RangeEncoder::new(1275);
            let target = bitrate * fl as f64 / 16_000.0;
            let used = enc.encode(&mut rc, &buf[f * fl..f * fl + PAST + fl + LOOKAHEAD], 1, subframes, target, 1275 * 8, false);
            rc.finish_in_place().unwrap();
            let nb = (used as usize).div_ceil(8);
            assert!(rc.bytes()[nb..].iter().all(|&b| b == 0), "frame {f}: bytes past the length");
            total += nb;
            let mut rd = RangeDecoder::new(&rc.bytes()[..nb]);
            let got = dec.decode_superframe(&mut rd, &mut out, 2, 1, subframes * 5);
            assert_eq!(got, fl);
            assert_eq!(rd.tell(), used, "frame {f}: the decoder read a different length");
            y.extend_from_slice(&out[0][..fl]);
        }
        // the decoder's output is a sample late
        let (mut sig, mut err) = (0f64, 0f64);
        for n in 0..frames * fl - 1 {
            let (a, b) = (x[n] as f64, y[n + 1] as f64);
            sig += a * a;
            err += (a - b) * (a - b);
        }
        (10.0 * (sig / err.max(1e-30)).log10(), total as f64 * 8.0 * 16_000.0 / (frames * fl) as f64)
    }

    #[test]
    fn speech_round_trips_through_the_decoder() {
        let x = speechlike(32_000, 16_000.0);
        for (sub, rate) in [(4, 12_000.0), (4, 24_000.0), (2, 16_000.0)] {
            let (snr, kbps) = silk_round_trip(&x, sub, rate);
            eprintln!("{} ms at {rate}: {snr:.1} dB, {:.1} kb/s", sub * 5, kbps / 1000.0);
            assert!(snr > 6.0, "{snr}");
        }
    }

    #[test]
    fn nlsf_round_trip_matches_the_lpc() {
        // a resonant envelope: its NLSFs, rebuilt by the decoder's conversion, give it back
        let mut x = vec![0f32; 640];
        let mut s = 1u32;
        for n in 2..x.len() {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = (s >> 9) as f32 / (1 << 23) as f32 - 0.5;
            x[n] = 1.6 * x[n - 1] - 0.9 * x[n - 2] + 0.1 * noise;
        }
        let a = lpc_analysis(&x).unwrap();
        let nlsf = a2nlsf(&a).unwrap();
        assert!(nlsf.windows(2).all(|w| w[0] < w[1]), "{nlsf:?}");
        let back = lsf2lpc(&nlsf, ORDER);
        for k in 0..ORDER {
            assert!((back[k] as f64 - a[k]).abs() < 0.01, "{k}: {} vs {}", back[k], a[k]);
        }
    }
}
