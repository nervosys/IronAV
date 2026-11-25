//! Parametric stereo (ISO/IEC 14496-3 subpart 8), the tool that makes HE-AAC v2: a mono SBR signal
//! plus a few parameters per band (inter-channel intensity and phase differences, coherence) is turned
//! into stereo in the QMF domain, before the synthesis filterbank.
//!
//! What is decoded: the `ps_data()` bitstream with its header, envelope borders, IID (coarse and fine
//! quantisation, 10, 20 and 34 bands), ICC (10, 20 and 34 bands) and the IPD / OPD extension (11 or 17
//! bands); the hybrid analysis filterbank (the lowest QMF bands split into 6+2+2 subbands, or, with
//! 34 parameter bands, 12+8+4+4+4); the decorrelator (three fractional-delay all-pass links per band
//! with the transient-aware ducking, and the plain delays above them); the stereo mixing with the
//! H11/H12/H21/H22 matrices (mixing rotation A below ICC mode 3 and rotation B above), interpolated
//! across each envelope; and the hybrid synthesis. IPD and OPD are applied (the full profile, not just
//! the baseline one). The structure follows FFmpeg's floating-point decoder (`aacps.c`). The
//! Huffman codewords and hybrid-band mappings come directly from 3GPP SP-040428. The other lookup
//! tables are computed from the codec formulas.
//!
//! Nothing of the tool is skipped. Parameters that arrive before the first header are ignored (the
//! stereo signal stays a copy of the mono one), and a frame that carries no `ps_data` reuses the last
//! one, as FFmpeg does. One difference from FFmpeg: whether IPD/OPD are in use is read per frame (a
//! frame without the extension has none), where FFmpeg keeps the last frame's flag; the two agree on
//! every stream whose encoder writes the extension in each frame or in none.

use super::ps_tables::*;
use super::Bits;
use std::collections::HashMap;
use std::sync::OnceLock;

const MAX_ENV: usize = 5;
const MAX_PAR: usize = 34;
/// QMF time slots per frame.
const SLOTS: usize = 32;
/// Longest delay line of the plain delay bands, in slots.
const MAX_DELAY: usize = 14;
const AP_LINKS: usize = 3;
const MAX_AP_DELAY: usize = 5;
const BANDS: [usize; 2] = [71, 91];
const PAR_BANDS: [usize; 2] = [20, 34];
const IPDOPD_BANDS: [usize; 2] = [11, 17];
const ALLPASS_BANDS: [usize; 2] = [30, 50];
const DECAY_CUTOFF: [usize; 2] = [10, 32];
const SHORT_DELAY_BAND: [usize; 2] = [42, 62];

const NUM_ENV_TAB: [[usize; 4]; 2] = [[0, 1, 2, 4], [1, 2, 3, 4]];
const NR_IIDICC_PAR: [usize; 6] = [10, 20, 34, 10, 20, 34];
const NR_IPDOPD_PAR: [usize; 6] = [5, 11, 17, 5, 11, 17];

type Par = [[i8; MAX_PAR]; MAX_ENV];
type Cx = [f32; 2];

// ---- tables -------------------------------------------------------------------------------------

struct Tabs {
    /// `(length << 24 | code)` to symbol, per Huffman table.
    huff: Vec<HashMap<u32, i32>>,
    pd_re: Vec<f32>,
    pd_im: Vec<f32>,
    /// Mixing matrices `[iid][icc]` as `[h11, h12, h21, h22]`, rotation A and B.
    ha: Vec<[[f32; 4]; 8]>,
    hb: Vec<[[f32; 4]; 8]>,
    f20_0_8: Vec<[Cx; 8]>,
    f34_0_12: Vec<[Cx; 8]>,
    f34_1_8: Vec<[Cx; 8]>,
    f34_2_4: Vec<[Cx; 8]>,
    q_fract: [Vec<[Cx; 3]>; 2],
    phi_fract: [Vec<Cx>; 2],
}

fn filters_from_proto(proto: &[f64; 7], bands: usize) -> Vec<[Cx; 8]> {
    (0..bands)
        .map(|q| {
            let mut f = [[0.0f32; 2]; 8];
            for n in 0..7 {
                let theta = 2.0 * std::f64::consts::PI * (q as f64 + 0.5) * (n as f64 - 6.0) / bands as f64;
                f[n] = [(proto[n] * theta.cos()) as f32, (proto[n] * -theta.sin()) as f32];
            }
            f
        })
        .collect()
}

fn tabs() -> &'static Tabs {
    static T: OnceLock<Tabs> = OnceLock::new();
    T.get_or_init(|| {
        let huff = HUFFMAN.iter().map(|&(first_delta, words)| {
            words.iter().enumerate().map(|(index, &(len, code))| {
                (u32::from(len) << 24 | code, first_delta + index as i32)
            }).collect()
        }).collect();
        // the phase of IPD / OPD index i is i * pi / 4; the smoothed phasor of the last three values
        // (weights 1/4, 1/2, 1) is normalised
        let (sin8, cos8): (Vec<f64>, Vec<f64>) = (0..8).map(|i| ((i as f64 * std::f64::consts::FRAC_PI_4).sin(), (i as f64 * std::f64::consts::FRAC_PI_4).cos())).unzip();
        let (mut pd_re, mut pd_im) = (vec![0f32; 512], vec![0f32; 512]);
        for a in 0..8 {
            for b in 0..8 {
                for c in 0..8 {
                    let re = 0.25 * cos8[a] + 0.5 * cos8[b] + cos8[c];
                    let im = 0.25 * sin8[a] + 0.5 * sin8[b] + sin8[c];
                    let mag = 1.0 / re.hypot(im);
                    pd_re[a * 64 + b * 8 + c] = (re * mag) as f32;
                    pd_im[a * 64 + b * 8 + c] = (im * mag) as f32;
                }
            }
        }
        // IID quantiser reconstruction levels (linear), coarse then fine
        const IID_DEQUANT: [f64; 46] = [
            0.05623413251903, 0.12589254117942, 0.19952623149689, 0.31622776601684, 0.44668359215096, 0.63095734448019, 0.79432823472428, 1.0,
            1.25892541179417, 1.58489319246111, 2.23872113856834, 3.16227766016838, 5.01187233627272, 7.94328234724282, 17.7827941003892,
            0.00316227766017, 0.00562341325190, 0.01, 0.01778279410039, 0.03162277660168, 0.05623413251903, 0.07943282347243, 0.11220184543020,
            0.15848931924611, 0.22387211385683, 0.31622776601684, 0.39810717055350, 0.50118723362727, 0.63095734448019, 0.79432823472428, 1.0,
            1.25892541179417, 1.58489319246111, 1.99526231496888, 2.51188643150958, 3.16227766016838, 4.46683592150963, 6.30957344480193, 8.91250938133745,
            12.5892541179417, 17.7827941003892, 31.6227766016838, 56.2341325190349, 100.0, 177.827941003892, 316.227766016837,
        ];
        const ICC_INVQ: [f64; 8] = [1.0, 0.937, 0.84118, 0.60092, 0.36764, 0.0, -0.589, -1.0];
        const ACOS_ICC_INVQ: [f64; 8] = [0.0, 0.35685527, 0.57133466, 0.92614472, 1.1943263, std::f64::consts::FRAC_PI_2, 2.2006171, std::f64::consts::PI];
        let mut ha = vec![[[0f32; 4]; 8]; 46];
        let mut hb = vec![[[0f32; 4]; 8]; 46];
        for iid in 0..46 {
            let c = IID_DEQUANT[iid];
            let c1 = std::f64::consts::SQRT_2 / (1.0 + c * c).sqrt();
            let c2 = c * c1;
            for icc in 0..8 {
                // rotation A
                let alpha = 0.5 * ACOS_ICC_INVQ[icc];
                let beta = alpha * (c1 - c2) * std::f64::consts::FRAC_1_SQRT_2;
                ha[iid][icc] = [(c2 * (beta + alpha).cos()) as f32, (c1 * (beta - alpha).cos()) as f32, (c2 * (beta + alpha).sin()) as f32, (c1 * (beta - alpha).sin()) as f32];
                // rotation B
                let rho = ICC_INVQ[icc].max(0.05);
                let mut alpha = 0.5 * (2.0 * c * rho).atan2(c * c - 1.0);
                let mu = c + 1.0 / c;
                let mu = (1.0 + (4.0 * rho * rho - 4.0) / (mu * mu)).sqrt();
                let gamma = ((1.0 - mu) / (1.0 + mu)).max(0.0).sqrt().atan();
                if alpha < 0.0 {
                    alpha += std::f64::consts::FRAC_PI_2;
                }
                let (ac, as_, gc, gs) = (alpha.cos(), alpha.sin(), gamma.cos(), gamma.sin());
                let s2 = std::f64::consts::SQRT_2;
                hb[iid][icc] = [(s2 * ac * gc) as f32, (s2 * as_ * gc) as f32, (-s2 * as_ * gs) as f32, (s2 * ac * gs) as f32];
            }
        }
        // all-pass links and the phase rotation of the fractional delay, by sub-subband centre frequency
        const F_CENTER_20: [f64; 10] = [-3.0, -1.0, 1.0, 3.0, 5.0, 7.0, 10.0, 14.0, 18.0, 22.0];
        const F_CENTER_34: [f64; 32] = [
            2.0, 6.0, 10.0, 14.0, 18.0, 22.0, 26.0, 30.0, 34.0, -10.0, -6.0, -2.0, 51.0, 57.0, 15.0, 21.0, 27.0, 33.0, 39.0, 45.0, 54.0, 66.0, 78.0, 42.0, 102.0, 66.0, 78.0, 90.0,
            102.0, 114.0, 126.0, 90.0,
        ];
        const LINK_DELAY: [f64; 3] = [0.43, 0.75, 0.347];
        const GAIN_DELAY: f64 = 0.39;
        let mut q_fract = [Vec::new(), Vec::new()];
        let mut phi_fract = [Vec::new(), Vec::new()];
        for (mode, n) in [(0usize, 30usize), (1, 50)] {
            for k in 0..n {
                let f_center = if mode == 0 {
                    if k < 10 { F_CENTER_20[k] * 0.125 } else { k as f64 - 6.5 }
                } else if k < 32 {
                    F_CENTER_34[k] / 24.0
                } else {
                    k as f64 - 26.5
                };
                let mut links = [[0f32; 2]; 3];
                for m in 0..3 {
                    let theta = -std::f64::consts::PI * LINK_DELAY[m] * f_center;
                    links[m] = [theta.cos() as f32, theta.sin() as f32];
                }
                q_fract[mode].push(links);
                let theta = -std::f64::consts::PI * GAIN_DELAY * f_center;
                phi_fract[mode].push([theta.cos() as f32, theta.sin() as f32]);
            }
        }
        // prototype filters of the hybrid analysis
        const G0_Q8: [f64; 7] = [0.00746082949812, 0.02270420949825, 0.04546865930473, 0.07266113929591, 0.09885108575264, 0.11793710567217, 0.125];
        const G0_Q12: [f64; 7] = [0.04081179924692, 0.03812810994926, 0.05144908135699, 0.06399831151592, 0.07428313801106, 0.08100347892914, 0.08333333333333];
        const G1_Q8: [f64; 7] = [0.01565675600122, 0.03752716391991, 0.05417891378782, 0.08417044116767, 0.10307344158036, 0.12222452249753, 0.125];
        const G2_Q4: [f64; 7] = [-0.05908211155639, -0.04871498374946, 0.0, 0.07778723915851, 0.16486303567403, 0.23279856662996, 0.25];
        Tabs {
            huff,
            pd_re,
            pd_im,
            ha,
            hb,
            f20_0_8: filters_from_proto(&G0_Q8, 8),
            f34_0_12: filters_from_proto(&G0_Q12, 12),
            f34_1_8: filters_from_proto(&G1_Q8, 8),
            f34_2_4: filters_from_proto(&G2_Q4, 4),
            q_fract,
            phi_fract,
        }
    })
}

/// One Huffman symbol, or `None` for a bit pattern no code uses.
fn read_huff(br: &mut Bits, table: usize) -> Option<i32> {
    let map = &tabs().huff[table];
    let mut code = 0u32;
    for len in 1..=20u32 {
        code = code << 1 | br.read(1);
        if let Some(&v) = map.get(&(len << 24 | code)) {
            return Some(v);
        }
    }
    None
}

// Indices of the Huffman tables in `HUFFMAN`.
const HUFF_IID_DF1: usize = 0;
const HUFF_IID_DT1: usize = 1;
const HUFF_IID_DF0: usize = 2;
const HUFF_IID_DT0: usize = 3;
const HUFF_ICC_DF: usize = 4;
const HUFF_ICC_DT: usize = 5;
const HUFF_IPD_DF: usize = 6;
const HUFF_IPD_DT: usize = 7;
const HUFF_OPD_DF: usize = 8;
const HUFF_OPD_DT: usize = 9;

// ---- parameter mapping between band configurations (table 8.46 and its inverses) -------------------

fn map_idx_10_to_20(out: &mut [i8; MAX_PAR], par: &[i8; MAX_PAR], full: bool) {
    let top = if full { 9 } else { 4 };
    if !full {
        out[10] = 0;
    }
    for b in (0..=top).rev() {
        out[2 * b] = par[b];
        out[2 * b + 1] = par[b];
    }
}

fn map_idx_34_to_20(out: &mut [i8; MAX_PAR], par: &[i8; MAX_PAR], full: bool) {
    let p = |i: usize| par[i] as i32;
    out[0] = ((2 * p(0) + p(1)) / 3) as i8;
    out[1] = ((p(1) + 2 * p(2)) / 3) as i8;
    out[2] = ((2 * p(3) + p(4)) / 3) as i8;
    out[3] = ((p(4) + 2 * p(5)) / 3) as i8;
    out[4] = ((p(6) + p(7)) / 2) as i8;
    out[5] = ((p(8) + p(9)) / 2) as i8;
    out[6] = par[10];
    out[7] = par[11];
    out[8] = ((p(12) + p(13)) / 2) as i8;
    out[9] = ((p(14) + p(15)) / 2) as i8;
    out[10] = par[16];
    if full {
        out[11] = par[17];
        out[12] = par[18];
        out[13] = par[19];
        out[14] = ((p(20) + p(21)) / 2) as i8;
        out[15] = ((p(22) + p(23)) / 2) as i8;
        out[16] = ((p(24) + p(25)) / 2) as i8;
        out[17] = ((p(26) + p(27)) / 2) as i8;
        out[18] = ((p(28) + p(29) + p(30) + p(31)) / 4) as i8;
        out[19] = ((p(32) + p(33)) / 2) as i8;
    }
}

fn map_val_34_to_20(par: &mut [f32; MAX_PAR]) {
    par[0] = (2.0 * par[0] + par[1]) * 0.333_333_33;
    par[1] = (par[1] + 2.0 * par[2]) * 0.333_333_33;
    par[2] = (2.0 * par[3] + par[4]) * 0.333_333_33;
    par[3] = (par[4] + 2.0 * par[5]) * 0.333_333_33;
    par[4] = (par[6] + par[7]) * 0.5;
    par[5] = (par[8] + par[9]) * 0.5;
    par[6] = par[10];
    par[7] = par[11];
    par[8] = (par[12] + par[13]) * 0.5;
    par[9] = (par[14] + par[15]) * 0.5;
    par[10] = par[16];
    par[11] = par[17];
    par[12] = par[18];
    par[13] = par[19];
    par[14] = (par[20] + par[21]) * 0.5;
    par[15] = (par[22] + par[23]) * 0.5;
    par[16] = (par[24] + par[25]) * 0.5;
    par[17] = (par[26] + par[27]) * 0.5;
    par[18] = (par[28] + par[29] + par[30] + par[31]) * 0.25;
    par[19] = (par[32] + par[33]) * 0.5;
}

fn map_idx_10_to_34(out: &mut [i8; MAX_PAR], par: &[i8; MAX_PAR], full: bool) {
    if full {
        for (range, from) in [(28..34, 9), (24..28, 8), (20..24, 7), (18..20, 6), (16..18, 5)] {
            for i in range {
                out[i] = par[from];
            }
        }
    } else {
        out[16] = 0;
    }
    for (range, from) in [(12..16, 4), (10..12, 3), (6..10, 2), (3..6, 1), (0..3, 0)] {
        for i in range {
            out[i] = par[from];
        }
    }
}

fn map_idx_20_to_34(out: &mut [i8; MAX_PAR], par: &[i8; MAX_PAR], full: bool) {
    if full {
        out[33] = par[19];
        out[32] = par[19];
        out[31] = par[18];
        out[30] = par[18];
        out[29] = par[18];
        out[28] = par[18];
        out[27] = par[17];
        out[26] = par[17];
        out[25] = par[16];
        out[24] = par[16];
        out[23] = par[15];
        out[22] = par[15];
        out[21] = par[14];
        out[20] = par[14];
        out[19] = par[13];
        out[18] = par[12];
        out[17] = par[11];
    }
    out[16] = par[10];
    out[15] = par[9];
    out[14] = par[9];
    out[13] = par[8];
    out[12] = par[8];
    out[11] = par[7];
    out[10] = par[6];
    out[9] = par[5];
    out[8] = par[5];
    out[7] = par[4];
    out[6] = par[4];
    out[5] = par[3];
    out[4] = ((par[2] as i32 + par[3] as i32) / 2) as i8;
    out[3] = par[2];
    out[2] = par[1];
    out[1] = ((par[0] as i32 + par[1] as i32) / 2) as i8;
    out[0] = par[0];
}

fn map_val_20_to_34(par: &mut [f32; MAX_PAR]) {
    par[33] = par[19];
    par[32] = par[19];
    par[31] = par[18];
    par[30] = par[18];
    par[29] = par[18];
    par[28] = par[18];
    par[27] = par[17];
    par[26] = par[17];
    par[25] = par[16];
    par[24] = par[16];
    par[23] = par[15];
    par[22] = par[15];
    par[21] = par[14];
    par[20] = par[14];
    par[19] = par[13];
    par[18] = par[12];
    par[17] = par[11];
    par[16] = par[10];
    par[15] = par[9];
    par[14] = par[9];
    par[13] = par[8];
    par[12] = par[8];
    par[11] = par[7];
    par[10] = par[6];
    par[9] = par[5];
    par[8] = par[5];
    par[7] = par[4];
    par[6] = par[4];
    par[5] = par[3];
    par[4] = (par[2] + par[3]) * 0.5;
    par[3] = par[2];
    par[2] = par[1];
    par[1] = (par[0] + par[1]) * 0.5;
}

/// The parameters of every envelope mapped to the 20- or 34-band layout in use.
fn remap(par: &Par, num_par: usize, num_env: usize, is34: bool, full: bool) -> Par {
    let mut out = *par;
    for e in 0..num_env.min(MAX_ENV) {
        if is34 {
            match num_par {
                20 | 11 => map_idx_20_to_34(&mut out[e], &par[e], full),
                10 | 5 => map_idx_10_to_34(&mut out[e], &par[e], full),
                _ => {}
            }
        } else {
            match num_par {
                34 | 17 => map_idx_34_to_20(&mut out[e], &par[e], full),
                10 | 5 => map_idx_10_to_20(&mut out[e], &par[e], full),
                _ => {}
            }
        }
    }
    out
}

// ---- the decoder ----------------------------------------------------------------------------------

/// Mixing matrices of the envelope borders: `[real / imaginary][border][parameter band]`.
type Mat = [[[f32; MAX_PAR]; MAX_ENV + 1]; 2];

pub struct Ps {
    // bitstream state
    pub start: bool,
    enable_iid: bool,
    iid_quant: bool,
    nr_iid_par: usize,
    nr_ipdopd_par: usize,
    enable_icc: bool,
    icc_mode: usize,
    nr_icc_par: usize,
    enable_ext: bool,
    frame_class: bool,
    num_env_old: usize,
    num_env: usize,
    enable_ipdopd: bool,
    border: [i32; MAX_ENV + 1],
    iid_par: Par,
    icc_par: Par,
    ipd_par: Par,
    opd_par: Par,
    is34: bool,
    is34_old: bool,
    // signal state
    /// The five lowest QMF bands, `[band][6 + slots + 6]`.
    in_buf: [[Cx; 44]; 5],
    delay: Vec<Cx>,
    ap_delay: Vec<Cx>,
    peak_decay_nrg: [f32; MAX_PAR],
    power_smooth: [f32; MAX_PAR],
    peak_decay_diff_smooth: [f32; MAX_PAR],
    h11: Mat,
    h12: Mat,
    h21: Mat,
    h22: Mat,
    lbuf: Vec<Cx>,
    rbuf: Vec<Cx>,
    opd_hist: [i8; MAX_PAR],
    ipd_hist: [i8; MAX_PAR],
}

fn parameter_stream<'a>(br: &mut Bits<'a>) -> Bits<'a> {
    Bits { data: br.data, pos: br.pos }
}

impl Ps {
    pub fn new() -> Self {
        Ps {
            start: false,
            enable_iid: false,
            iid_quant: false,
            nr_iid_par: 0,
            nr_ipdopd_par: 0,
            enable_icc: false,
            icc_mode: 0,
            nr_icc_par: 0,
            enable_ext: false,
            frame_class: false,
            num_env_old: 0,
            num_env: 0,
            enable_ipdopd: false,
            border: [0; MAX_ENV + 1],
            iid_par: [[0; MAX_PAR]; MAX_ENV],
            icc_par: [[0; MAX_PAR]; MAX_ENV],
            ipd_par: [[0; MAX_PAR]; MAX_ENV],
            opd_par: [[0; MAX_PAR]; MAX_ENV],
            is34: false,
            is34_old: false,
            in_buf: [[[0.0; 2]; 44]; 5],
            delay: vec![[0.0; 2]; 91 * (SLOTS + MAX_DELAY)],
            ap_delay: vec![[0.0; 2]; 50 * AP_LINKS * (SLOTS + MAX_AP_DELAY)],
            peak_decay_nrg: [0.0; MAX_PAR],
            power_smooth: [0.0; MAX_PAR],
            peak_decay_diff_smooth: [0.0; MAX_PAR],
            h11: [[[0.0; MAX_PAR]; MAX_ENV + 1]; 2],
            h12: [[[0.0; MAX_PAR]; MAX_ENV + 1]; 2],
            h21: [[[0.0; MAX_PAR]; MAX_ENV + 1]; 2],
            h22: [[[0.0; MAX_PAR]; MAX_ENV + 1]; 2],
            lbuf: vec![[0.0; 2]; 91 * SLOTS],
            rbuf: vec![[0.0; 2]; 91 * SLOTS],
            opd_hist: [0; MAX_PAR],
            ipd_hist: [0; MAX_PAR],
        }
    }

    fn clear_params(&mut self) {
        self.iid_par = [[0; MAX_PAR]; MAX_ENV];
        self.icc_par = [[0; MAX_PAR]; MAX_ENV];
        self.ipd_par = [[0; MAX_PAR]; MAX_ENV];
        self.opd_par = [[0; MAX_PAR]; MAX_ENV];
    }

    /// Delta- or plain-coded IID, ICC, IPD or OPD values of envelope `e` (`kind`: 0 IID, 1 ICC, 2 IPD, 3 OPD).
    fn read_par(&mut self, br: &mut Bits, kind: usize, table: usize, e: usize, dt: bool) -> Result<(), ()> {
        let num = match kind {
            0 => self.nr_iid_par,
            1 => self.nr_icc_par,
            _ => self.nr_ipdopd_par,
        };
        let (max_iid, quant) = (7 + 8 * self.iid_quant as i32, self.iid_quant);
        let _ = quant;
        let e_prev = if e > 0 { e - 1 } else { self.num_env_old.saturating_sub(1) };
        let par = match kind {
            0 => &mut self.iid_par,
            1 => &mut self.icc_par,
            2 => &mut self.ipd_par,
            _ => &mut self.opd_par,
        };
        let mut run = 0i32;
        for b in 0..num.min(MAX_PAR) {
            let d = read_huff(br, table).ok_or(())?;
            let mut val = if dt { par[e_prev][b] as i32 + d } else { run + d };
            if kind >= 2 {
                val &= 7;
            }
            run = val;
            par[e][b] = val as i8;
            let bad = match kind {
                0 => val.abs() > max_iid,
                1 => !(0..=7).contains(&val),
                _ => false,
            };
            if bad {
                return Err(());
            }
        }
        Ok(())
    }

    /// The `ps_extension()` body of extension id 0 (IPD and OPD); the bits it used.
    fn read_extension(&mut self, br: &mut Bits, id: u32) -> Result<i32, ()> {
        if id != 0 {
            return Ok(0);
        }
        let start = br.pos;
        self.enable_ipdopd = br.flag();
        if self.enable_ipdopd {
            for e in 0..self.num_env {
                let dt = br.flag();
                self.read_par(br, 2, if dt { HUFF_IPD_DT } else { HUFF_IPD_DF }, e, dt)?;
                let dt = br.flag();
                self.read_par(br, 3, if dt { HUFF_OPD_DT } else { HUFF_OPD_DF }, e, dt)?;
            }
        }
        br.skip(1); // reserved_ps
        Ok((br.pos - start) as i32)
    }

    fn parse(&mut self, g: &mut Bits, bits_left: i32) -> Result<(), ()> {
        let header = g.flag();
        if header {
            self.enable_iid = g.flag();
            if self.enable_iid {
                let mode = g.read(3) as usize;
                if mode > 5 {
                    return Err(());
                }
                self.nr_iid_par = NR_IIDICC_PAR[mode];
                self.iid_quant = mode > 2;
                self.nr_ipdopd_par = NR_IPDOPD_PAR[mode];
            }
            self.enable_icc = g.flag();
            if self.enable_icc {
                self.icc_mode = g.read(3) as usize;
                if self.icc_mode > 5 {
                    return Err(());
                }
                self.nr_icc_par = NR_IIDICC_PAR[self.icc_mode];
            }
            self.enable_ext = g.flag();
        }
        self.frame_class = g.flag();
        self.num_env_old = self.num_env;
        self.num_env = NUM_ENV_TAB[self.frame_class as usize][g.read(2) as usize];
        self.border[0] = -1;
        if self.frame_class {
            for e in 1..=self.num_env {
                self.border[e] = g.read(5) as i32;
                if self.border[e] < self.border[e - 1] {
                    return Err(());
                }
            }
        } else {
            let log2 = [0, 0, 1, 0, 2];
            for e in 1..=self.num_env {
                self.border[e] = ((e as i32 * SLOTS as i32) >> log2[self.num_env]) - 1;
            }
        }
        // an IPD / OPD extension of an earlier frame does not carry over
        self.enable_ipdopd = false;
        if self.enable_iid {
            for e in 0..self.num_env {
                let dt = g.flag();
                let table = match (dt, self.iid_quant) {
                    (false, false) => HUFF_IID_DF0,
                    (false, true) => HUFF_IID_DF1,
                    (true, false) => HUFF_IID_DT0,
                    (true, true) => HUFF_IID_DT1,
                };
                self.read_par(g, 0, table, e, dt)?;
            }
        } else {
            self.iid_par = [[0; MAX_PAR]; MAX_ENV];
        }
        if self.enable_icc {
            for e in 0..self.num_env {
                let dt = g.flag();
                self.read_par(g, 1, if dt { HUFF_ICC_DT } else { HUFF_ICC_DF }, e, dt)?;
            }
        } else {
            self.icc_par = [[0; MAX_PAR]; MAX_ENV];
        }
        if self.enable_ext {
            let mut cnt = g.read(4) as i32;
            if cnt == 15 {
                cnt += g.read(8) as i32;
            }
            cnt *= 8;
            while cnt > 7 {
                let id = g.read(2);
                cnt -= 2 + self.read_extension(g, id)?;
                if g.pos as isize > g.data.len() as isize * 8 {
                    return Err(());
                }
            }
            if cnt < 0 {
                return Err(());
            }
            g.skip(cnt as u32);
        }
        // a frame whose last envelope ends before the frame does gets one more, a copy of the last
        if self.num_env == 0 || self.border[self.num_env] < SLOTS as i32 - 1 {
            let n = self.num_env;
            let source = if n > 0 { n as isize - 1 } else { self.num_env_old as isize - 1 };
            if n >= MAX_ENV {
                return Err(());
            }
            if source >= 0 && source as usize != n {
                let s = source as usize;
                if self.enable_iid {
                    self.iid_par[n] = self.iid_par[s];
                }
                if self.enable_icc {
                    self.icc_par[n] = self.icc_par[s];
                }
                if self.enable_ipdopd {
                    self.ipd_par[n] = self.ipd_par[s];
                    self.opd_par[n] = self.opd_par[s];
                }
            }
            if self.enable_iid && self.iid_par[n][..self.nr_iid_par].iter().any(|&v| (v as i32).abs() > 7 + 8 * self.iid_quant as i32) {
                return Err(());
            }
            if self.enable_icc && self.icc_par[n][..self.nr_iid_par].iter().any(|&v| !(0..=7).contains(&v)) {
                return Err(());
            }
            self.num_env += 1;
            self.border[self.num_env] = SLOTS as i32 - 1;
        }
        self.is34_old = self.is34;
        if self.enable_iid || self.enable_icc {
            self.is34 = (self.enable_iid && self.nr_iid_par == 34) || (self.enable_icc && self.nr_icc_par == 34);
        }
        if !self.enable_ipdopd {
            self.ipd_par = [[0; MAX_PAR]; MAX_ENV];
            self.opd_par = [[0; MAX_PAR]; MAX_ENV];
        }
        if header {
            self.start = true;
        }
        let _ = bits_left;
        Ok(())
    }

    /// Read one `ps_data()` from `br` (positioned at its first bit), which may use at most `bits_left`
    /// bits. Returns the bits it used; on a bad stream, parametric stereo is switched off until the
    /// next header, the bits are skipped, and `bits_left` is returned.
    pub fn read(&mut self, br: &mut Bits, bits_left: i32) -> i32 {
        let mut g = parameter_stream(br);
        let start = g.pos;
        let ok = self.parse(&mut g, bits_left).is_ok() && (g.pos - start) as i32 <= bits_left && g.left() >= 0;
        if ok {
            br.pos = g.pos;
            return (g.pos - start) as i32;
        }
        self.start = false;
        self.clear_params();
        br.pos += bits_left.max(0) as usize;
        bits_left
    }

    // ---- hybrid filterbank ----

    /// Apply the symmetric complex filters to 13 consecutive samples; the outputs of the first `n` filters.
    fn cx_filter(inp: &[Cx], filter: &[[Cx; 8]], n: usize) -> [Cx; 12] {
        let mut inre0 = [0f32; 6];
        let mut inre1 = [0f32; 6];
        let mut inim0 = [0f32; 6];
        let mut inim1 = [0f32; 6];
        for j in 0..6 {
            inre0[j] = inp[j][0] + inp[12 - j][0];
            inre1[j] = inp[j][1] - inp[12 - j][1];
            inim0[j] = inp[j][1] + inp[12 - j][1];
            inim1[j] = inp[j][0] - inp[12 - j][0];
        }
        let mut out = [[0f32; 2]; 12];
        for i in 0..n {
            let mut re = filter[i][6][0] * inp[6][0];
            let mut im = filter[i][6][0] * inp[6][1];
            for j in 0..6 {
                re += filter[i][j][0] * inre0[j] - filter[i][j][1] * inre1[j];
                im += filter[i][j][0] * inim0[j] + filter[i][j][1] * inim1[j];
            }
            out[i] = [re, im];
        }
        out
    }

    /// Split one QMF band into two subbands with a real symmetric filter.
    fn split2(inp: &[Cx], filter: &[f32; 7]) -> (Cx, Cx) {
        let re_in = filter[6] * inp[6][0];
        let im_in = filter[6] * inp[6][1];
        let (mut re_op, mut im_op) = (0f32, 0f32);
        for j in (0..6).step_by(2) {
            re_op += filter[j + 1] * (inp[j + 1][0] + inp[12 - j - 1][0]);
            im_op += filter[j + 1] * (inp[j + 1][1] + inp[12 - j - 1][1]);
        }
        ([re_in + re_op, im_in + im_op], [re_in - re_op, im_in - im_op])
    }

    fn hybrid_analysis(&mut self, x: &[[f32; 64]], is34: bool) {
        const G1_Q2: [f32; 7] = [0.0, 0.018_994_875, 0.0, -0.072_931_39, 0.0, 0.305_966_3, 0.5];
        let t = tabs();
        for i in 0..5 {
            for j in 0..38 {
                self.in_buf[i][j + 6] = [x[j][i], x[38 + j][i]];
            }
        }
        for n in 0..SLOTS {
            if is34 {
                let o = Self::cx_filter(&self.in_buf[0][n..n + 13], &t.f34_0_12, 12);
                for k in 0..12 {
                    self.lbuf[k * SLOTS + n] = o[k];
                }
                let o = Self::cx_filter(&self.in_buf[1][n..n + 13], &t.f34_1_8, 8);
                for k in 0..8 {
                    self.lbuf[(12 + k) * SLOTS + n] = o[k];
                }
                for (band, first) in [(2, 20), (3, 24), (4, 28)] {
                    let o = Self::cx_filter(&self.in_buf[band][n..n + 13], &t.f34_2_4, 4);
                    for k in 0..4 {
                        self.lbuf[(first + k) * SLOTS + n] = o[k];
                    }
                }
            } else {
                let o = Self::cx_filter(&self.in_buf[0][n..n + 13], &t.f20_0_8, 8);
                let src = [6, 7, 0, 1];
                for (k, &s) in src.iter().enumerate() {
                    self.lbuf[k * SLOTS + n] = o[s];
                }
                self.lbuf[4 * SLOTS + n] = [o[2][0] + o[5][0], o[2][1] + o[5][1]];
                self.lbuf[5 * SLOTS + n] = [o[3][0] + o[4][0], o[3][1] + o[4][1]];
                // the two real-filtered bands, the first with its outputs swapped
                let (a, b) = Self::split2(&self.in_buf[1][n..n + 13], &G1_Q2);
                self.lbuf[7 * SLOTS + n] = a;
                self.lbuf[6 * SLOTS + n] = b;
                let (a, b) = Self::split2(&self.in_buf[2][n..n + 13], &G1_Q2);
                self.lbuf[8 * SLOTS + n] = a;
                self.lbuf[9 * SLOTS + n] = b;
            }
        }
        // the QMF bands above the split ones pass through
        let (first, offset) = if is34 { (5, 27) } else { (3, 7) };
        for i in first..64 {
            for j in 0..SLOTS {
                self.lbuf[(offset + i) * SLOTS + j] = [x[j][i], x[38 + j][i]];
            }
        }
        for band in self.in_buf.iter_mut() {
            band.copy_within(32..38, 0);
        }
    }

    fn hybrid_synthesis(buf: &[Cx], out: &mut [[f32; 64]], is34: bool) {
        for n in 0..SLOTS {
            let sum = |range: std::ops::Range<usize>| -> Cx {
                let mut s = [0f32; 2];
                for k in range {
                    s[0] += buf[k * SLOTS + n][0];
                    s[1] += buf[k * SLOTS + n][1];
                }
                s
            };
            let groups: &[(usize, std::ops::Range<usize>)] = if is34 {
                &[(0, 0..12), (1, 12..20), (2, 20..24), (3, 24..28), (4, 28..32)]
            } else {
                &[(0, 0..6), (1, 6..8), (2, 8..10)]
            };
            for (band, range) in groups {
                let s = sum(range.clone());
                out[n][*band] = s[0];
                out[38 + n][*band] = s[1];
            }
            let (first, offset) = if is34 { (5, 27) } else { (3, 7) };
            for i in first..64 {
                out[n][i] = buf[(offset + i) * SLOTS + n][0];
                out[38 + n][i] = buf[(offset + i) * SLOTS + n][1];
            }
        }
    }

    // ---- decorrelation ----

    fn decorrelation(&mut self, is34: bool) {
        let t = tabs();
        let mode = is34 as usize;
        let k_to_i: &[u8] = if is34 { &K_TO_I_34 } else { &K_TO_I_20 };
        let mut power = [[0f32; SLOTS]; MAX_PAR];
        let mut transient_gain = [[0f32; SLOTS]; MAX_PAR];
        if is34 != self.is34_old {
            self.peak_decay_nrg = [0.0; MAX_PAR];
            self.power_smooth = [0.0; MAX_PAR];
            self.peak_decay_diff_smooth = [0.0; MAX_PAR];
            self.delay.fill([0.0; 2]);
            self.ap_delay.fill([0.0; 2]);
        }
        for k in 0..BANDS[mode] {
            let i = k_to_i[k] as usize;
            for n in 0..SLOTS {
                let s = self.lbuf[k * SLOTS + n];
                power[i][n] += s[0] * s[0] + s[1] * s[1];
            }
        }
        // transient detection
        const PEAK_DECAY_FACTOR: f32 = 0.765_928_3;
        const TRANSIENT_IMPACT: f32 = 1.5;
        const A_SMOOTH: f32 = 0.25;
        for i in 0..PAR_BANDS[mode] {
            for n in 0..SLOTS {
                let decayed = PEAK_DECAY_FACTOR * self.peak_decay_nrg[i];
                self.peak_decay_nrg[i] = decayed.max(power[i][n]);
                self.power_smooth[i] += A_SMOOTH * (power[i][n] - self.power_smooth[i]);
                self.peak_decay_diff_smooth[i] += A_SMOOTH * (self.peak_decay_nrg[i] - power[i][n] - self.peak_decay_diff_smooth[i]);
                let denom = TRANSIENT_IMPACT * self.peak_decay_diff_smooth[i];
                transient_gain[i][n] = if denom > self.power_smooth[i] { self.power_smooth[i] / denom } else { 1.0 };
            }
        }
        // decorrelation and transient reduction: the all-pass chain, then the plain delays above it
        const A: [f32; 3] = [0.651_439_07, 0.564_718_1, 0.489_541_66];
        const DECAY_SLOPE: f32 = 0.05;
        let dl = SLOTS + MAX_DELAY;
        let al = SLOTS + MAX_AP_DELAY;
        for k in 0..BANDS[mode] {
            let b = k_to_i[k] as usize;
            let delay = &mut self.delay[k * dl..(k + 1) * dl];
            delay.copy_within(SLOTS..SLOTS + MAX_DELAY, 0);
            delay[MAX_DELAY..].copy_from_slice(&self.lbuf[k * SLOTS..(k + 1) * SLOTS]);
            let out = &mut self.rbuf[k * SLOTS..(k + 1) * SLOTS];
            if k < ALLPASS_BANDS[mode] {
                let slope = (1.0 - DECAY_SLOPE * (k as f32 - DECAY_CUTOFF[mode] as f32)).clamp(0.0, 1.0);
                let ap = &mut self.ap_delay[k * AP_LINKS * al..(k + 1) * AP_LINKS * al];
                for m in 0..AP_LINKS {
                    ap.copy_within(m * al + SLOTS..m * al + SLOTS + 5, m * al);
                }
                let phi = t.phi_fract[mode][k];
                let q = &t.q_fract[mode][k];
                let ag = [A[0] * slope, A[1] * slope, A[2] * slope];
                for n in 0..SLOTS {
                    let d = delay[MAX_DELAY - 2 + n];
                    let mut in_re = d[0] * phi[0] - d[1] * phi[1];
                    let mut in_im = d[0] * phi[1] + d[1] * phi[0];
                    for m in 0..AP_LINKS {
                        let a_re = ag[m] * in_re;
                        let a_im = ag[m] * in_im;
                        let link = ap[m * al + n + 2 - m];
                        let (apd_re, apd_im) = (in_re, in_im);
                        in_re = link[0] * q[m][0] - link[1] * q[m][1] - a_re;
                        in_im = link[0] * q[m][1] + link[1] * q[m][0] - a_im;
                        ap[m * al + n + 5] = [apd_re + ag[m] * in_re, apd_im + ag[m] * in_im];
                    }
                    out[n] = [transient_gain[b][n] * in_re, transient_gain[b][n] * in_im];
                }
            } else {
                // a delay of 14 slots below the short-delay band, of one slot above it
                let back = if k < SHORT_DELAY_BAND[mode] { 14 } else { 1 };
                for n in 0..SLOTS {
                    let d = delay[MAX_DELAY - back + n];
                    out[n] = [d[0] * transient_gain[b][n], d[1] * transient_gain[b][n]];
                }
            }
        }
    }

    // ---- stereo processing ----

    fn stereo_processing(&mut self, is34: bool) {
        let t = tabs();
        let mode = is34 as usize;
        let k_to_i: &[u8] = if is34 { &K_TO_I_34 } else { &K_TO_I_20 };
        let ne = self.num_env;
        if self.num_env_old > 0 {
            let o = self.num_env_old.min(MAX_ENV);
            for m in [&mut self.h11, &mut self.h12, &mut self.h21, &mut self.h22] {
                for part in 0..2 {
                    m[part][0] = m[part][o];
                }
            }
        }
        // map the parameters onto the band layout in use, and the old matrices if it changed
        let iid = remap(&self.iid_par, self.nr_iid_par, ne, is34, true);
        let icc = remap(&self.icc_par, self.nr_icc_par, ne, is34, true);
        let (ipd, opd) = if self.enable_ipdopd {
            (remap(&self.ipd_par, self.nr_ipdopd_par, ne, is34, false), remap(&self.opd_par, self.nr_ipdopd_par, ne, is34, false))
        } else {
            (self.ipd_par, self.opd_par)
        };
        if is34 && !self.is34_old {
            for m in [&mut self.h11, &mut self.h12, &mut self.h21, &mut self.h22] {
                for part in 0..2 {
                    map_val_20_to_34(&mut m[part][0]);
                }
            }
            self.ipd_hist = [0; MAX_PAR];
            self.opd_hist = [0; MAX_PAR];
        } else if !is34 && self.is34_old {
            for m in [&mut self.h11, &mut self.h12, &mut self.h21, &mut self.h22] {
                for part in 0..2 {
                    map_val_34_to_20(&mut m[part][0]);
                }
            }
            self.ipd_hist = [0; MAX_PAR];
            self.opd_hist = [0; MAX_PAR];
        }
        let lut = if self.icc_mode < 3 { &t.ha } else { &t.hb };
        let iid_off = 7 + 23 * self.iid_quant as usize;
        for e in 0..ne {
            for b in 0..PAR_BANDS[mode] {
                let row = &lut[(iid[e][b] as i32 + iid_off as i32).clamp(0, 45) as usize][icc[e][b] as usize & 7];
                let (mut h11, mut h12, mut h21, mut h22) = (row[0], row[1], row[2], row[3]);
                if self.enable_ipdopd && b < IPDOPD_BANDS[mode] {
                    // the standard has this smoothing run only with IPD / OPD enabled; the reference decoder always runs it
                    let opd_idx = (self.opd_hist[b] as usize) * 8 + (opd[e][b] as usize & 7);
                    let ipd_idx = (self.ipd_hist[b] as usize) * 8 + (ipd[e][b] as usize & 7);
                    let (opd_re, opd_im) = (t.pd_re[opd_idx], t.pd_im[opd_idx]);
                    let (ipd_re, ipd_im) = (t.pd_re[ipd_idx], t.pd_im[ipd_idx]);
                    self.opd_hist[b] = (opd_idx & 0x3F) as i8;
                    self.ipd_hist[b] = (ipd_idx & 0x3F) as i8;
                    let ipd_adj_re = opd_re * ipd_re + opd_im * ipd_im;
                    let ipd_adj_im = opd_im * ipd_re - opd_re * ipd_im;
                    self.h11[1][e + 1][b] = h11 * opd_im;
                    self.h12[1][e + 1][b] = h12 * ipd_adj_im;
                    self.h21[1][e + 1][b] = h21 * opd_im;
                    self.h22[1][e + 1][b] = h22 * ipd_adj_im;
                    h11 *= opd_re;
                    h12 *= ipd_adj_re;
                    h21 *= opd_re;
                    h22 *= ipd_adj_re;
                }
                self.h11[0][e + 1][b] = h11;
                self.h12[0][e + 1][b] = h12;
                self.h21[0][e + 1][b] = h21;
                self.h22[0][e + 1][b] = h22;
            }
            let (start, stop) = (self.border[e], self.border[e + 1]);
            let width = 1.0 / if stop - start != 0 { (stop - start) as f32 } else { 1.0 };
            for k in 0..BANDS[mode] {
                let b = k_to_i[k] as usize;
                let mut h = [[self.h11[0][e][b], self.h12[0][e][b], self.h21[0][e][b], self.h22[0][e][b]], [0.0; 4]];
                let mut step = [[0f32; 4]; 2];
                for (i, m) in [&self.h11, &self.h12, &self.h21, &self.h22].iter().enumerate() {
                    step[0][i] = (m[0][e + 1][b] - h[0][i]) * width;
                }
                if self.enable_ipdopd {
                    let flip = if (is34 && (9..=13).contains(&k)) || (!is34 && k <= 1) { -1.0 } else { 1.0 };
                    for (i, m) in [&self.h11, &self.h12, &self.h21, &self.h22].iter().enumerate() {
                        h[1][i] = flip * m[1][e][b];
                        step[1][i] = (m[1][e + 1][b] - h[1][i]) * width;
                    }
                }
                if stop > start && start >= -1 && stop < SLOTS as i32 {
                    let from = (start + 1) as usize;
                    let len = (stop - start) as usize;
                    // `l` holds the mono signal and `r` its decorrelated copy until they are mixed
                    for n in 0..len {
                        let l = self.lbuf[k * SLOTS + from + n];
                        let r = self.rbuf[k * SLOTS + from + n];
                        for i in 0..4 {
                            h[0][i] += step[0][i];
                            h[1][i] += step[1][i];
                        }
                        let (lo, ro);
                        if self.enable_ipdopd {
                            lo = [h[0][0] * l[0] + h[0][2] * r[0] - h[1][0] * l[1] - h[1][2] * r[1], h[0][0] * l[1] + h[0][2] * r[1] + h[1][0] * l[0] + h[1][2] * r[0]];
                            ro = [h[0][1] * l[0] + h[0][3] * r[0] - h[1][1] * l[1] - h[1][3] * r[1], h[0][1] * l[1] + h[0][3] * r[1] + h[1][1] * l[0] + h[1][3] * r[0]];
                        } else {
                            lo = [h[0][0] * l[0] + h[0][2] * r[0], h[0][0] * l[1] + h[0][2] * r[1]];
                            ro = [h[0][1] * l[0] + h[0][3] * r[0], h[0][1] * l[1] + h[0][3] * r[1]];
                        }
                        self.lbuf[k * SLOTS + from + n] = lo;
                        self.rbuf[k * SLOTS + from + n] = ro;
                    }
                }
            }
        }
    }

    /// Turn the mono signal in `x[0]` (QMF samples, `[re | im][slot][band]` with 38 slots each) into
    /// stereo in `x[0]` and `x[1]`. `top` is the highest QMF band SBR fills (`kx + m`).
    pub fn apply(&mut self, x: &mut [Vec<[f32; 64]>], top: usize) {
        let is34 = self.is34;
        let mode = is34 as usize;
        // the sub-subbands above SBR's top carry nothing: their delay lines are empty
        let top = (top + BANDS[mode]).saturating_sub(64);
        let dl = SLOTS + MAX_DELAY;
        for k in top..BANDS[mode] {
            self.delay[k * dl..(k + 1) * dl].fill([0.0; 2]);
        }
        let al = AP_LINKS * (SLOTS + MAX_AP_DELAY);
        for k in top..ALLPASS_BANDS[mode] {
            self.ap_delay[k * al..(k + 1) * al].fill([0.0; 2]);
        }
        self.hybrid_analysis(&x[0], is34);
        self.decorrelation(is34);
        self.stereo_processing(is34);
        let (l, r) = x.split_at_mut(1);
        Self::hybrid_synthesis(&self.lbuf, &mut l[0], is34);
        Self::hybrid_synthesis(&self.rbuf, &mut r[0], is34);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hybrid analysis followed by the synthesis (a sum of the subbands) returns the QMF signal,
    /// delayed by the six slots of the filters' symmetric halves.
    fn hybrid_round_trip(is34: bool) -> f64 {
        let mut ps = Ps::new();
        let mut state = 12345u32;
        let mut noise = move || {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let frames = 6;
        let mut input = vec![vec![[0f32; 64]; 2 * 38]; frames];
        for f in 0..frames {
            // each frame's slots 32..38 are the first of the next
            for slot in 0..38 {
                for band in 0..64 {
                    let (re, im) = (noise(), noise());
                    if f > 0 && slot < 6 {
                        let prev = input[f - 1][32 + slot];
                        let prev_im = input[f - 1][38 + 32 + slot];
                        input[f][slot][band] = prev[band];
                        input[f][38 + slot][band] = prev_im[band];
                    } else {
                        input[f][slot][band] = re;
                        input[f][38 + slot][band] = im;
                    }
                }
            }
        }
        let mut worst = 0f64;
        for f in 0..frames {
            ps.hybrid_analysis(&input[f], is34);
            let mut out = vec![[0f32; 64]; 76];
            Ps::hybrid_synthesis(&ps.lbuf, &mut out, is34);
            if f == 0 {
                continue;
            }
            for n in 0..SLOTS {
                for band in 0..64 {
                    let got = out[n][band] as f64;
                    let want = input[f][n][band] as f64;
                    worst = worst.max((got - want).abs());
                }
            }
        }
        worst
    }

    #[test]
    fn huffman_tables_are_prefix_codes() {
        for (table, &(first_delta, words)) in HUFFMAN.iter().enumerate() {
            let sum: u32 = words.iter().map(|&(len, _)| 1 << (20 - len)).sum();
            assert_eq!(sum, 1 << 20, "incomplete Huffman table {table}");
            for (index, &(len, code)) in words.iter().enumerate() {
                let data = (code << (32 - u32::from(len))).to_be_bytes();
                let mut bits = Bits::new(&data);
                let delta = first_delta + index as i32;
                assert_eq!(read_huff(&mut bits, table), Some(delta), "table {table}, delta {delta}");
                assert_eq!(bits.pos, usize::from(len), "table {table}, delta {delta}");
            }
        }
    }

    /// `bits` ('0' and '1', spaces ignored) as bytes, zero-padded.
    fn bytes(bits: &str) -> Vec<u8> {
        let b: Vec<bool> = bits.chars().filter(|c| !c.is_whitespace()).map(|c| c == '1').collect();
        b.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |a, (i, &x)| a | (x as u8) << (7 - i))).collect()
    }

    #[test]
    fn a_small_ps_data_parses() {
        // header, IID (mode 0: 10 bands, coarse) on, ICC off, no extension; fixed class, one envelope
        // (code 1): border 31; IID frequency-coded: ten zero differences (the one-bit code 0)
        let data = bytes("1 1 000 0 0  0 01  0 0000000000");
        let mut ps = Ps::new();
        let mut br = Bits::new(&data);
        assert_eq!(ps.read(&mut br, 24), 21);
        assert_eq!(br.pos, 21);
        assert!(ps.start && !ps.is34);
        assert_eq!((ps.num_env, ps.border[1]), (1, 31));
        assert_eq!(ps.iid_par[0][..10], [0; 10]);
    }

    #[test]
    fn bad_or_cut_ps_data_switches_the_tool_off_and_skips_its_bits() {
        let good = bytes("1 1 000 0 0  0 01  0 0000000000");
        // a reserved IID mode
        let reserved = bytes("1 1 110 0 0 0 01 0 0000000000");
        // borders that go backwards (variable class, two envelopes: 20 then 3)
        let backwards = bytes("1 0 0 0  1 01 10100 00011");
        // the data runs past what the extension says is there
        for (name, data, left) in [("reserved", &reserved, 24), ("backwards", &backwards, 24), ("cut", &good, 12), ("empty", &vec![], 8)] {
            let mut ps = Ps::new();
            ps.start = true;
            let mut br = Bits::new(data);
            assert_eq!(ps.read(&mut br, left), left, "{name}");
            assert_eq!(br.pos, left as usize, "{name}: the bits are skipped");
            assert!(!ps.start, "{name}: switched off");
            assert!(ps.iid_par.iter().all(|e| e.iter().all(|&v| v == 0)), "{name}: parameters cleared");
        }
        // anything at all, of any length, is survived
        let mut x = 0x1234_5678u32;
        for len in 0..40 {
            let data: Vec<u8> = (0..len).map(|_| { x = x.wrapping_mul(1664525).wrapping_add(1013904223); (x >> 24) as u8 }).collect();
            let mut ps = Ps::new();
            let mut br = Bits::new(&data);
            ps.read(&mut br, len * 8);
        }
    }

    #[test]
    fn hybrid_bank_is_a_delay_in_both_layouts() {
        for is34 in [false, true] {
            let worst = hybrid_round_trip(is34);
            assert!(worst < 0.02, "is34 {is34}: worst error {worst}");
        }
    }
}
