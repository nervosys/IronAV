//! Spectral band replication (ISO/IEC 14496-3 subpart 4), the HE-AAC v1 tool: the core codec codes the
//! lower half of the band at half the output rate and a small amount of side data says how to rebuild
//! the upper half from it. The bitstream parsing, frequency-band tables, patching, envelope and gain
//! processing follow FFmpeg's `aacsbr.c` / `aacsbr_template.c` and `sbrdsp.c` (float version), with its
//! tables (`tools/gen_sbr_tables.py` reads them from FFmpeg's sources). The QMF filterbanks are written
//! from the standard's definitions rather than FFmpeg's fast transform with its shuffles.
//!
//! Samples are scaled to the 16-bit range inside, as the envelope data expects (its units are those of
//! 16-bit PCM).
//!
//! Not done: parametric stereo (HE-AAC v2), the USAC variant, 960-sample frames, and the downsampled
//! (low-power) synthesis for decoders that only want the core rate.

use super::sbr_tables::*;
use super::Bits;
use std::collections::HashMap;
use std::f64::consts::PI;
use std::sync::OnceLock;

/// Shift of the HF generator's window into the QMF slots (the "t_HFAdj" offset).
const OFFSET: usize = 2;
const NOISE_FLOOR_OFFSET: i32 = 6;
const SLOTS: usize = 16;
const FLT_MIN: f32 = 1.175_494_4e-38;
const FLT_EPSILON: f32 = 1.192_092_9e-7;

const FIXFIX: u32 = 0;
const FIXVAR: u32 = 1;
const VARFIX: u32 = 2;
#[allow(dead_code)]
const VARVAR: u32 = 3;

/// Element types, as FFmpeg numbers them.
pub const TYPE_SCE: u8 = 0;
pub const TYPE_CPE: u8 = 1;

// ---- tables -------------------------------------------------------------------------------------

struct Tabs {
    huff: Vec<HashMap<u32, i32>>,
    /// cos / sin of π/64 (k + ½)(2n - ½) for the analysis, `[k][n]`.
    ana: Vec<[(f32, f32); 64]>,
    /// cos / sin of π/128 (k + ½)(2n - 255) for the synthesis, `[n][k]`.
    syn: Vec<[(f32, f32); 64]>,
    /// The prototype filter with the signs the cosine modulation above expects (the table's taps at
    /// 384 and 512 carry the opposite sign), and its every-other-tap decimation for the analysis.
    win_us: Vec<f32>,
    win_ds: Vec<f32>,
}

fn tabs() -> &'static Tabs {
    static T: OnceLock<Tabs> = OnceLock::new();
    T.get_or_init(|| {
        let mut huff = Vec::new();
        let mut at = 0;
        for t in 0..10 {
            let mut map = HashMap::new();
            let mut code = 0u64;
            for i in 0..HUFF_COUNTS[t] {
                let len = HUFF_LENS[at + i] as u32;
                map.insert(len << 24 | (code >> (32 - len)) as u32, HUFF_SYMBOLS[at + i] as i32 + HUFF_OFFSETS[t]);
                code += 1u64 << (32 - len);
            }
            at += HUFF_COUNTS[t];
            huff.push(map);
        }
        let ana = (0..32).map(|k| std::array::from_fn(|n| {
            let a = PI / 64.0 * (k as f64 + 0.5) * (2.0 * n as f64 - 0.5);
            (a.cos() as f32, a.sin() as f32)
        })).collect();
        let syn = (0..128).map(|n| std::array::from_fn(|k| {
            let a = PI / 128.0 * (k as f64 + 0.5) * (2.0 * n as f64 - 255.0);
            (a.cos() as f32, a.sin() as f32)
        })).collect();
        let mut win_us = QMF_WINDOW_US.to_vec();
        win_us[384] = -win_us[384];
        win_us[512] = -win_us[512];
        let win_ds = win_us.iter().step_by(2).copied().collect();
        Tabs { huff, ana, syn, win_us, win_ds }
    })
}

const T_ENV_1_5DB: usize = 0;
const F_ENV_1_5DB: usize = 1;
const T_ENV_BAL_1_5DB: usize = 2;
const F_ENV_BAL_1_5DB: usize = 3;
const T_ENV_3_0DB: usize = 4;
const F_ENV_3_0DB: usize = 5;
const T_ENV_BAL_3_0DB: usize = 6;
const F_ENV_BAL_3_0DB: usize = 7;
const T_NOISE_3_0DB: usize = 8;
const T_NOISE_BAL_3_0DB: usize = 9;

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

type Res<T = ()> = std::result::Result<T, ()>;

fn lrint(x: f32) -> i32 {
    x.round_ties_even() as i32
}

fn exp2fi(x: i32) -> f32 {
    (x as f32).exp2()
}

// ---- state --------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
struct Spectrum {
    start_freq: u8,
    stop_freq: u8,
    xover_band: u8,
    freq_scale: u8,
    alter_scale: u8,
    noise_bands: u8,
}

const UNSET: Spectrum = Spectrum { start_freq: 255, stop_freq: 255, xover_band: 255, freq_scale: 255, alter_scale: 255, noise_bands: 255 };

#[derive(Clone)]
struct Chan {
    frame_class: u32,
    add_harmonic_flag: bool,
    num_env: usize,
    freq_res: [u8; 9],
    num_noise: usize,
    df_env: [u8; 9],
    df_noise: [u8; 2],
    invf_mode: [[u8; 5]; 2],
    add_harmonic: [u8; 48],
    amp_res: u32,
    /// The synthesis filterbank's history of V, newest first.
    synth: Vec<f32>,
    /// The analysis filterbank's input history: 288 old samples and 1024 new.
    analysis: Vec<f32>,
    e_a: [i32; 2],
    bw_array: [f32; 5],
    /// QMF values of the original signal, `[buffer * 32 + slot][band]`.
    w: Vec<[[f32; 2]; 32]>,
    ypos: usize,
    /// Output of the HF adjuster, `[buffer * 38 + slot][band]`.
    y: Vec<[[f32; 2]; 64]>,
    g_temp: Vec<[f32; 48]>,
    q_temp: Vec<[f32; 48]>,
    s_indexmapped: [[u8; 48]; 9],
    env_facs_q: [[i32; 48]; 9],
    env_facs: [[f32; 48]; 9],
    noise_facs_q: [[i32; 5]; 3],
    noise_facs: [[f32; 5]; 3],
    t_env: [i32; 9],
    t_env_num_env_old: i32,
    t_q: [i32; 3],
    f_indexnoise: u32,
    f_indexsine: u32,
}

impl Chan {
    fn new() -> Self {
        Chan {
            frame_class: 0,
            add_harmonic_flag: false,
            num_env: 0,
            freq_res: [0; 9],
            num_noise: 0,
            df_env: [0; 9],
            df_noise: [0; 2],
            invf_mode: [[0; 5]; 2],
            add_harmonic: [0; 48],
            amp_res: 0,
            synth: vec![0.0; 1280],
            analysis: vec![0.0; 1312],
            e_a: [0, -1],
            bw_array: [0.0; 5],
            w: vec![[[0.0; 2]; 32]; 64],
            ypos: 0,
            y: vec![[[0.0; 2]; 64]; 76],
            g_temp: vec![[0.0; 48]; 42],
            q_temp: vec![[0.0; 48]; 42],
            s_indexmapped: [[0; 48]; 9],
            env_facs_q: [[0; 48]; 9],
            env_facs: [[0.0; 48]; 9],
            noise_facs_q: [[0; 5]; 3],
            noise_facs: [[0.0; 5]; 3],
            t_env: [0; 9],
            t_env_num_env_old: 0,
            t_q: [0; 3],
            f_indexnoise: 0,
            f_indexsine: 0,
        }
    }
}

/// The SBR decoder of one channel element (a single or a pair).
pub struct Sbr {
    sample_rate: i32,
    start: bool,
    ready_for_dequant: bool,
    id_aac: u8,
    reset: bool,
    spectrum: Spectrum,
    amp_res_header: u32,
    limiter_bands: u32,
    limiter_gains: u32,
    interpol_freq: bool,
    smoothing_mode: u32,
    coupling: bool,
    k: [i32; 5],
    kx: [i32; 2],
    m: [i32; 2],
    kx_and_m_pushed: bool,
    n_master: i32,
    data: [Chan; 2],
    n: [i32; 2],
    n_q: i32,
    n_lim: i32,
    f_master: [i32; 49],
    f_tablelow: [i32; 25],
    f_tablehigh: [i32; 49],
    f_tablenoise: [i32; 6],
    f_tablelim: [i32; 30],
    num_patches: usize,
    patch_num_subbands: [i32; 6],
    patch_start_subband: [i32; 6],
    x_low: Vec<[[f32; 2]; 40]>,
    x_high: Vec<[[f32; 2]; 40]>,
    /// QMF values of the reconstructed signal, `[channel][re/im][slot][band]`.
    x: Vec<Vec<[f32; 64]>>,
    alpha0: [[f32; 2]; 64],
    alpha1: [[f32; 2]; 64],
    e_origmapped: [[f32; 48]; 8],
    q_mapped: [[f32; 48]; 8],
    s_mapped: [[u8; 48]; 8],
    e_curr: [[f32; 48]; 8],
    q_m: [[f32; 48]; 8],
    s_m: [[f32; 48]; 8],
    gain: [[f32; 48]; 8],
}

fn bit_vec(br: &mut Bits, v: &mut [u8], n: usize) {
    for x in v.iter_mut().take(n) {
        *x = br.read(1) as u8;
    }
}

impl Sbr {
    pub fn new(core_rate: u32) -> Self {
        let mut s = Sbr {
            sample_rate: 2 * core_rate as i32,
            start: false,
            ready_for_dequant: false,
            id_aac: 0,
            reset: false,
            spectrum: UNSET,
            amp_res_header: 0,
            limiter_bands: 0,
            limiter_gains: 0,
            interpol_freq: false,
            smoothing_mode: 0,
            coupling: false,
            k: [0; 5],
            kx: [0, 32],
            m: [0; 2],
            kx_and_m_pushed: false,
            n_master: 0,
            data: [Chan::new(), Chan::new()],
            n: [0; 2],
            n_q: 0,
            n_lim: 0,
            f_master: [0; 49],
            f_tablelow: [0; 25],
            f_tablehigh: [0; 49],
            f_tablenoise: [0; 6],
            f_tablelim: [0; 30],
            num_patches: 0,
            patch_num_subbands: [0; 6],
            patch_start_subband: [0; 6],
            x_low: vec![[[0.0; 2]; 40]; 32],
            x_high: vec![[[0.0; 2]; 40]; 64],
            x: vec![vec![[0.0; 64]; 76]; 2],
            alpha0: [[0.0; 2]; 64],
            alpha1: [[0.0; 2]; 64],
            e_origmapped: [[0.0; 48]; 8],
            q_mapped: [[0.0; 48]; 8],
            s_mapped: [[0; 48]; 8],
            e_curr: [[0.0; 48]; 8],
            q_m: [[0.0; 48]; 8],
            s_m: [[0.0; 48]; 8],
            gain: [[0.0; 48]; 8],
        };
        s.turnoff();
        s
    }

    /// Places SBR in pure upsampling mode.
    fn turnoff(&mut self) {
        self.start = false;
        self.ready_for_dequant = false;
        self.kx[1] = 32;
        self.m[1] = 0;
        self.data[0].e_a[1] = -1;
        self.data[1].e_a[1] = -1;
        self.spectrum = UNSET;
    }

    // ---- frequency tables ----

    fn make_f_tablelim(&mut self) {
        if self.limiter_bands > 0 {
            let bands_warped = [1.327_151_7f32, 1.185_092_8, 1.119_871_6];
            let lim = bands_warped[self.limiter_bands as usize - 1];
            let mut patch_borders = [0i32; 7];
            patch_borders[0] = self.kx[1];
            for k in 1..=self.num_patches {
                patch_borders[k] = patch_borders[k - 1] + self.patch_num_subbands[k - 1];
            }
            let n0 = self.n[0] as usize;
            self.f_tablelim[..=n0].copy_from_slice(&self.f_tablelow[..=n0]);
            if self.num_patches > 1 {
                self.f_tablelim[n0 + 1..n0 + self.num_patches].copy_from_slice(&patch_borders[1..self.num_patches]);
            }
            self.f_tablelim[..self.num_patches + n0].sort_unstable();
            self.n_lim = self.n[0] + self.num_patches as i32 - 1;
            let (mut i, mut o) = (1usize, 0usize);
            let np = self.num_patches;
            let in_borders = |v: i32| patch_borders[..=np].contains(&v);
            while (o as i32) < self.n_lim {
                let (vi, vo) = (self.f_tablelim[i], self.f_tablelim[o]);
                if vi as f32 >= vo as f32 * lim {
                    o += 1;
                    self.f_tablelim[o] = vi;
                    i += 1;
                } else if vi == vo || !in_borders(vi) {
                    i += 1;
                    self.n_lim -= 1;
                } else if !in_borders(vo) {
                    self.f_tablelim[o] = vi;
                    i += 1;
                    self.n_lim -= 1;
                } else {
                    o += 1;
                    self.f_tablelim[o] = vi;
                    i += 1;
                }
            }
        } else {
            self.f_tablelim[0] = self.f_tablelow[0];
            self.f_tablelim[1] = self.f_tablelow[self.n[0] as usize];
            self.n_lim = 1;
        }
    }

    fn read_header(&mut self, br: &mut Bits) {
        self.start = true;
        self.ready_for_dequant = false;
        let old = self.spectrum;
        let old_limiter = self.limiter_bands;
        self.amp_res_header = br.read(1);
        self.spectrum.start_freq = br.read(4) as u8;
        self.spectrum.stop_freq = br.read(4) as u8;
        self.spectrum.xover_band = br.read(3) as u8;
        br.skip(2);
        let extra1 = br.flag();
        let extra2 = br.flag();
        if extra1 {
            self.spectrum.freq_scale = br.read(2) as u8;
            self.spectrum.alter_scale = br.read(1) as u8;
            self.spectrum.noise_bands = br.read(2) as u8;
        } else {
            self.spectrum.freq_scale = 2;
            self.spectrum.alter_scale = 1;
            self.spectrum.noise_bands = 2;
        }
        if old != self.spectrum {
            self.reset = true;
        }
        if extra2 {
            self.limiter_bands = br.read(2);
            self.limiter_gains = br.read(2);
            self.interpol_freq = br.flag();
            self.smoothing_mode = br.read(1);
        } else {
            self.limiter_bands = 2;
            self.limiter_gains = 2;
            self.interpol_freq = true;
            self.smoothing_mode = 1;
        }
        if self.limiter_bands != old_limiter && !self.reset {
            self.make_f_tablelim();
        }
    }

    fn make_bands(bands: &mut [i32], start: i32, stop: i32, num_bands: i32) {
        let base = (stop as f32 / start as f32).powf(1.0 / num_bands as f32);
        let mut prod = start as f32;
        let mut previous = start;
        for k in 0..(num_bands - 1) as usize {
            prod *= base;
            let present = lrint(prod);
            bands[k] = present - previous;
            previous = present;
        }
        bands[num_bands as usize - 1] = stop - previous;
    }

    fn check_n_master(&self, n_master: i32, xover: i32) -> Res {
        if n_master <= 0 || xover >= n_master {
            Err(())
        } else {
            Ok(())
        }
    }

    /// Master Frequency Band Table.
    fn make_f_master(&mut self) -> Res {
        let sp = self.spectrum;
        let off = match self.sample_rate {
            16000 => 0,
            22050 => 1,
            24000 => 2,
            32000 => 3,
            44100 | 48000 | 64000 => 4,
            88200 | 96000 | 128000 | 176400 | 192000 => 5,
            _ => return Err(()),
        };
        let temp = if self.sample_rate < 32000 { 3000 } else if self.sample_rate < 64000 { 4000 } else { 5000 };
        let sr = self.sample_rate;
        let start_min = ((temp << 7) + (sr >> 1)) / sr;
        let stop_min = ((temp << 8) + (sr >> 1)) / sr;
        self.k[0] = start_min + SBR_OFFSET[off][sp.start_freq as usize] as i32;
        if sp.stop_freq < 14 {
            self.k[2] = stop_min;
            let mut stop_dk = [0i32; 13];
            Self::make_bands(&mut stop_dk, stop_min, 64, 13);
            stop_dk.sort_unstable();
            for k in 0..sp.stop_freq as usize {
                self.k[2] += stop_dk[k];
            }
        } else if sp.stop_freq == 14 {
            self.k[2] = 2 * self.k[0];
        } else {
            self.k[2] = 3 * self.k[0];
        }
        self.k[2] = self.k[2].min(64);
        let max_qmf = if sr <= 32000 { 48 } else if sr == 44100 { 35 } else { 32 };
        if self.k[2] - self.k[0] > max_qmf {
            return Err(());
        }
        if sp.freq_scale == 0 {
            let dk = sp.alter_scale as i32 + 1;
            self.n_master = ((self.k[2] - self.k[0] + (dk & 2)) >> dk) << 1;
            self.check_n_master(self.n_master, sp.xover_band as i32)?;
            for k in 1..=self.n_master as usize {
                self.f_master[k] = dk;
            }
            let k2diff = self.k[2] - self.k[0] - self.n_master * dk;
            if k2diff < 0 {
                self.f_master[1] -= 1;
                self.f_master[2] -= (k2diff < -1) as i32;
            } else if k2diff != 0 {
                self.f_master[self.n_master as usize] += 1;
            }
            self.f_master[0] = self.k[0];
            for k in 1..=self.n_master as usize {
                self.f_master[k] += self.f_master[k - 1];
            }
        } else {
            let half_bands = 7 - sp.freq_scale as i32;
            let two_regions;
            if 49 * self.k[2] > 110 * self.k[0] {
                two_regions = true;
                self.k[1] = 2 * self.k[0];
            } else {
                two_regions = false;
                self.k[1] = self.k[2];
            }
            let num_bands_0 = lrint(half_bands as f32 * (self.k[1] as f32 / self.k[0] as f32).log2()) * 2;
            if num_bands_0 <= 0 {
                return Err(());
            }
            let mut vk0 = [0i32; 49];
            Self::make_bands(&mut vk0[1..], self.k[0], self.k[1], num_bands_0);
            vk0[1..=num_bands_0 as usize].sort_unstable();
            let vdk0_max = vk0[num_bands_0 as usize];
            vk0[0] = self.k[0];
            for k in 1..=num_bands_0 as usize {
                if vk0[k] <= 0 {
                    return Err(());
                }
                vk0[k] += vk0[k - 1];
            }
            if two_regions {
                let mut vk1 = [0i32; 49];
                let invwarp = if sp.alter_scale != 0 { 0.769_230_8f32 } else { 1.0 };
                let num_bands_1 = lrint(half_bands as f32 * invwarp * (self.k[2] as f32 / self.k[1] as f32).log2()) * 2;
                if num_bands_1 <= 0 || num_bands_0 + num_bands_1 > 48 {
                    return Err(());
                }
                Self::make_bands(&mut vk1[1..], self.k[1], self.k[2], num_bands_1);
                let nb1 = num_bands_1 as usize;
                let vdk1_min = *vk1[1..=nb1].iter().min().unwrap();
                if vdk1_min < vdk0_max {
                    vk1[1..=nb1].sort_unstable();
                    let change = (vdk0_max - vk1[1]).min((vk1[nb1] - vk1[1]) >> 1);
                    vk1[1] += change;
                    vk1[nb1] -= change;
                }
                vk1[1..=nb1].sort_unstable();
                vk1[0] = self.k[1];
                for k in 1..=nb1 {
                    if vk1[k] <= 0 {
                        return Err(());
                    }
                    vk1[k] += vk1[k - 1];
                }
                self.n_master = num_bands_0 + num_bands_1;
                self.check_n_master(self.n_master, sp.xover_band as i32)?;
                let nb0 = num_bands_0 as usize;
                self.f_master[..=nb0].copy_from_slice(&vk0[..=nb0]);
                self.f_master[nb0 + 1..nb0 + 1 + nb1].copy_from_slice(&vk1[1..=nb1]);
            } else {
                self.n_master = num_bands_0;
                self.check_n_master(self.n_master, sp.xover_band as i32)?;
                self.f_master[..=num_bands_0 as usize].copy_from_slice(&vk0[..=num_bands_0 as usize]);
            }
        }
        Ok(())
    }

    /// High Frequency Generation - Patch Construction.
    fn hf_calc_npatches(&mut self) -> Res {
        let (mut last_k, mut last_msb) = (-1i32, -1i32);
        let mut sb;
        let mut msb = self.k[0];
        let mut usb = self.kx[1];
        let goal_sb = ((1000 << 11) + (self.sample_rate >> 1)) / self.sample_rate;
        self.num_patches = 0;
        let mut k: i32 = if goal_sb < self.kx[1] + self.m[1] {
            let mut k = 0;
            while self.f_master[k as usize] < goal_sb {
                k += 1;
            }
            k
        } else {
            self.n_master
        };
        loop {
            let mut odd;
            if k == last_k && msb == last_msb {
                return Err(());
            }
            last_k = k;
            last_msb = msb;
            let mut i = k;
            loop {
                sb = self.f_master[i as usize];
                odd = (sb + self.k[0]) & 1;
                if sb > self.k[0] - 1 + msb - odd && i > 0 {
                    i -= 1;
                } else {
                    break;
                }
            }
            if self.num_patches > 5 {
                return Err(());
            }
            let np = self.num_patches;
            self.patch_num_subbands[np] = (sb - usb).max(0);
            self.patch_start_subband[np] = self.k[0] - odd - self.patch_num_subbands[np];
            if self.patch_num_subbands[np] > 0 {
                usb = sb;
                msb = sb;
                self.num_patches += 1;
            } else {
                msb = self.kx[1];
            }
            if self.f_master[k as usize] - sb < 3 {
                k = self.n_master;
            }
            if sb == self.kx[1] + self.m[1] {
                break;
            }
        }
        if self.num_patches > 1 && self.patch_num_subbands[self.num_patches - 1] < 3 {
            self.num_patches -= 1;
        }
        Ok(())
    }

    /// Derived Frequency Band Tables.
    fn make_f_derived(&mut self) -> Res {
        let xover = self.spectrum.xover_band as usize;
        self.n[1] = self.n_master - xover as i32;
        self.n[0] = (self.n[1] + 1) >> 1;
        let n1 = self.n[1] as usize;
        self.f_tablehigh[..=n1].copy_from_slice(&self.f_master[xover..=xover + n1]);
        self.m[1] = self.f_tablehigh[n1] - self.f_tablehigh[0];
        self.kx[1] = self.f_tablehigh[0];
        if self.kx[1] + self.m[1] > 64 || self.kx[1] > 32 {
            return Err(());
        }
        self.f_tablelow[0] = self.f_tablehigh[0];
        let temp = (self.n[1] & 1) as usize;
        for k in 1..=self.n[0] as usize {
            self.f_tablelow[k] = self.f_tablehigh[2 * k - temp];
        }
        self.n_q = lrint(self.spectrum.noise_bands as f32 * (self.k[2] as f32 / self.kx[1] as f32).log2()).max(1);
        if self.n_q > 5 {
            self.n_q = 1;
            return Err(());
        }
        self.f_tablenoise[0] = self.f_tablelow[0];
        let mut temp = 0;
        for k in 1..=self.n_q {
            temp += (self.n[0] - temp) / (self.n_q + 1 - k);
            self.f_tablenoise[k as usize] = self.f_tablelow[temp as usize];
        }
        self.hf_calc_npatches()?;
        self.make_f_tablelim();
        self.data[0].f_indexnoise = 0;
        self.data[1].f_indexnoise = 0;
        Ok(())
    }

    // ---- bitstream ----

    fn read_grid(&mut self, br: &mut Bits, ch: usize) -> Res {
        const CEIL_LOG2: [u32; 6] = [0, 1, 2, 2, 3, 3];
        let amp_res_header = self.amp_res_header;
        let d = &mut self.data[ch];
        let mut bs_pointer = 0u32;
        let mut abs_bord_trail = SLOTS as i32;
        let bs_num_env_old = d.num_env;
        d.freq_res[0] = d.freq_res[d.num_env];
        d.amp_res = amp_res_header;
        d.t_env_num_env_old = d.t_env[bs_num_env_old];
        let class = br.read(2);
        match class {
            FIXFIX => {
                let num_env = 1usize << br.read(2);
                if num_env > 5 {
                    return Err(());
                }
                d.num_env = num_env;
                let num_rel_lead = num_env - 1;
                if num_env == 1 {
                    d.amp_res = 0;
                }
                d.t_env[0] = 0;
                d.t_env[num_env] = abs_bord_trail;
                abs_bord_trail = (abs_bord_trail + (num_env as i32 >> 1)) / num_env as i32;
                for i in 0..num_rel_lead {
                    d.t_env[i + 1] = d.t_env[i] + abs_bord_trail;
                }
                d.freq_res[1] = br.read(1) as u8;
                for i in 1..num_env {
                    d.freq_res[i + 1] = d.freq_res[1];
                }
            }
            FIXVAR => {
                abs_bord_trail += br.read(2) as i32;
                let num_rel_trail = br.read(2) as usize;
                d.num_env = num_rel_trail + 1;
                d.t_env[0] = 0;
                d.t_env[d.num_env] = abs_bord_trail;
                for i in 0..num_rel_trail {
                    d.t_env[d.num_env - 1 - i] = d.t_env[d.num_env - i] - 2 * br.read(2) as i32 - 2;
                }
                bs_pointer = br.read(CEIL_LOG2[d.num_env]);
                for i in 0..d.num_env {
                    d.freq_res[d.num_env - i] = br.read(1) as u8;
                }
            }
            VARFIX => {
                d.t_env[0] = br.read(2) as i32;
                let num_rel_lead = br.read(2) as usize;
                d.num_env = num_rel_lead + 1;
                d.t_env[d.num_env] = abs_bord_trail;
                for i in 0..num_rel_lead {
                    d.t_env[i + 1] = d.t_env[i] + 2 * br.read(2) as i32 + 2;
                }
                bs_pointer = br.read(CEIL_LOG2[d.num_env]);
                let n = d.num_env;
                bit_vec(br, &mut d.freq_res[1..], n);
            }
            _ => {
                d.t_env[0] = br.read(2) as i32;
                abs_bord_trail += br.read(2) as i32;
                let num_rel_lead = br.read(2) as usize;
                let num_rel_trail = br.read(2) as usize;
                let num_env = num_rel_lead + num_rel_trail + 1;
                if num_env > 5 {
                    return Err(());
                }
                d.num_env = num_env;
                d.t_env[num_env] = abs_bord_trail;
                for i in 0..num_rel_lead {
                    d.t_env[i + 1] = d.t_env[i] + 2 * br.read(2) as i32 + 2;
                }
                for i in 0..num_rel_trail {
                    d.t_env[num_env - 1 - i] = d.t_env[num_env - i] - 2 * br.read(2) as i32 - 2;
                }
                bs_pointer = br.read(CEIL_LOG2[num_env]);
                bit_vec(br, &mut d.freq_res[1..], num_env);
            }
        }
        d.frame_class = class;
        if bs_pointer as usize > d.num_env + 1 {
            return Err(());
        }
        for i in 1..=d.num_env {
            if d.t_env[i - 1] >= d.t_env[i] {
                return Err(());
            }
        }
        d.num_noise = (d.num_env > 1) as usize + 1;
        d.t_q[0] = d.t_env[0];
        d.t_q[d.num_noise] = d.t_env[d.num_env];
        if d.num_noise > 1 {
            let idx = if d.frame_class == FIXFIX {
                d.num_env >> 1
            } else if d.frame_class & 1 == 1 {
                d.num_env - (bs_pointer as usize).saturating_sub(1).max(1)
            } else if bs_pointer == 0 {
                1
            } else if bs_pointer == 1 {
                d.num_env - 1
            } else {
                bs_pointer as usize - 1
            };
            d.t_q[1] = d.t_env[idx];
        }
        d.e_a[0] = -((d.e_a[1] != bs_num_env_old as i32) as i32);
        d.e_a[1] = -1;
        if d.frame_class & 1 == 1 && bs_pointer != 0 {
            d.e_a[1] = d.num_env as i32 + 1 - bs_pointer as i32;
        } else if d.frame_class == 2 && bs_pointer > 1 {
            d.e_a[1] = bs_pointer as i32 - 1;
        }
        Ok(())
    }

    fn copy_grid(&mut self) {
        let src = self.data[0].clone();
        let dst = &mut self.data[1];
        dst.freq_res[0] = dst.freq_res[dst.num_env];
        dst.t_env_num_env_old = dst.t_env[dst.num_env];
        dst.e_a[0] = -((dst.e_a[1] != dst.num_env as i32) as i32);
        dst.freq_res[1..].copy_from_slice(&src.freq_res[1..]);
        dst.t_env = src.t_env;
        dst.t_q = src.t_q;
        dst.num_env = src.num_env;
        dst.amp_res = src.amp_res;
        dst.num_noise = src.num_noise;
        dst.frame_class = src.frame_class;
        dst.e_a[1] = src.e_a[1];
    }

    fn read_dtdf(&mut self, br: &mut Bits, ch: usize) {
        let d = &mut self.data[ch];
        let (ne, nn) = (d.num_env, d.num_noise);
        bit_vec(br, &mut d.df_env, ne);
        bit_vec(br, &mut d.df_noise, nn);
    }

    fn read_invf(&mut self, br: &mut Bits, ch: usize) {
        let n_q = self.n_q as usize;
        let d = &mut self.data[ch];
        d.invf_mode[1] = d.invf_mode[0];
        for i in 0..n_q {
            d.invf_mode[0][i] = br.read(2) as u8;
        }
    }

    fn read_envelope(&mut self, br: &mut Bits, ch: usize) -> Res {
        let delta = (ch == 1 && self.coupling) as i32 + 1;
        let odd = (self.n[1] & 1) as usize;
        let (n, coupling) = (self.n, self.coupling);
        let d = &mut self.data[ch];
        let (bits, t_huff, f_huff) = if coupling && ch == 1 {
            if d.amp_res != 0 { (5, T_ENV_BAL_3_0DB, F_ENV_BAL_3_0DB) } else { (6, T_ENV_BAL_1_5DB, F_ENV_BAL_1_5DB) }
        } else if d.amp_res != 0 {
            (6, T_ENV_3_0DB, F_ENV_3_0DB)
        } else {
            (7, T_ENV_1_5DB, F_ENV_1_5DB)
        };
        let valid = |v: i32| (0..=127).contains(&v);
        for i in 0..d.num_env {
            let res = d.freq_res[i + 1] as usize;
            if d.df_env[i] != 0 {
                if d.freq_res[i + 1] == d.freq_res[i] {
                    for j in 0..n[res] as usize {
                        let v = d.env_facs_q[i][j] + delta * read_huff(br, t_huff).ok_or(())?;
                        d.env_facs_q[i + 1][j] = v;
                        if !valid(v) {
                            return Err(());
                        }
                    }
                } else if res == 1 {
                    for j in 0..n[1] as usize {
                        let k = (j + odd) >> 1;
                        let v = d.env_facs_q[i][k] + delta * read_huff(br, t_huff).ok_or(())?;
                        d.env_facs_q[i + 1][j] = v;
                        if !valid(v) {
                            return Err(());
                        }
                    }
                } else {
                    for j in 0..n[0] as usize {
                        let k = if j > 0 { 2 * j - odd } else { 0 };
                        let v = d.env_facs_q[i][k] + delta * read_huff(br, t_huff).ok_or(())?;
                        d.env_facs_q[i + 1][j] = v;
                        if !valid(v) {
                            return Err(());
                        }
                    }
                }
            } else {
                d.env_facs_q[i + 1][0] = delta * br.read(bits) as i32;
                for j in 1..n[res] as usize {
                    let v = d.env_facs_q[i + 1][j - 1] + delta * read_huff(br, f_huff).ok_or(())?;
                    d.env_facs_q[i + 1][j] = v;
                    if !valid(v) {
                        return Err(());
                    }
                }
            }
        }
        d.env_facs_q[0] = d.env_facs_q[d.num_env];
        Ok(())
    }

    fn read_noise(&mut self, br: &mut Bits, ch: usize) -> Res {
        let delta = (ch == 1 && self.coupling) as i32 + 1;
        let (t_huff, f_huff) = if self.coupling && ch == 1 { (T_NOISE_BAL_3_0DB, F_ENV_BAL_3_0DB) } else { (T_NOISE_3_0DB, F_ENV_3_0DB) };
        let n_q = self.n_q as usize;
        let d = &mut self.data[ch];
        let valid = |v: i32| (0..=30).contains(&v);
        for i in 0..d.num_noise {
            if d.df_noise[i] != 0 {
                for j in 0..n_q {
                    let v = d.noise_facs_q[i][j] + delta * read_huff(br, t_huff).ok_or(())?;
                    d.noise_facs_q[i + 1][j] = v;
                    if !valid(v) {
                        return Err(());
                    }
                }
            } else {
                d.noise_facs_q[i + 1][0] = delta * br.read(5) as i32;
                for j in 1..n_q {
                    let v = d.noise_facs_q[i + 1][j - 1] + delta * read_huff(br, f_huff).ok_or(())?;
                    d.noise_facs_q[i + 1][j] = v;
                    if !valid(v) {
                        return Err(());
                    }
                }
            }
        }
        d.noise_facs_q[0] = d.noise_facs_q[d.num_noise];
        Ok(())
    }

    fn read_harmonics(&mut self, br: &mut Bits, ch: usize) {
        let n1 = self.n[1] as usize;
        let d = &mut self.data[ch];
        d.add_harmonic_flag = br.flag();
        if d.add_harmonic_flag {
            bit_vec(br, &mut d.add_harmonic, n1);
        }
    }

    fn read_single(&mut self, br: &mut Bits) -> Res {
        if br.flag() {
            br.skip(4);
        }
        self.read_grid(br, 0)?;
        self.read_dtdf(br, 0);
        self.read_invf(br, 0);
        self.read_envelope(br, 0)?;
        self.read_noise(br, 0)?;
        self.read_harmonics(br, 0);
        Ok(())
    }

    fn read_pair(&mut self, br: &mut Bits) -> Res {
        if br.flag() {
            br.skip(8);
        }
        self.coupling = br.flag();
        if self.coupling {
            self.read_grid(br, 0)?;
            self.copy_grid();
            self.read_dtdf(br, 0);
            self.read_dtdf(br, 1);
            self.read_invf(br, 0);
            self.data[1].invf_mode[1] = self.data[1].invf_mode[0];
            self.data[1].invf_mode[0] = self.data[0].invf_mode[0];
            self.read_envelope(br, 0)?;
            self.read_noise(br, 0)?;
            self.read_envelope(br, 1)?;
            self.read_noise(br, 1)?;
        } else {
            self.read_grid(br, 0)?;
            self.read_grid(br, 1)?;
            self.read_dtdf(br, 0);
            self.read_dtdf(br, 1);
            self.read_invf(br, 0);
            self.read_invf(br, 1);
            self.read_envelope(br, 0)?;
            self.read_envelope(br, 1)?;
            self.read_noise(br, 0)?;
            self.read_noise(br, 1)?;
        }
        self.read_harmonics(br, 0);
        self.read_harmonics(br, 1);
        Ok(())
    }

    fn read_data(&mut self, br: &mut Bits, id_aac: u8) {
        self.id_aac = id_aac;
        self.ready_for_dequant = true;
        let ok = if id_aac == TYPE_SCE { self.read_single(br) } else if id_aac == TYPE_CPE { self.read_pair(br) } else { Err(()) };
        if ok.is_err() {
            self.turnoff();
            return;
        }
        if br.flag() {
            // bs_extended_data: extensions are parametric stereo (not decoded) or reserved; either is skipped
            let mut n = br.read(4) as i32;
            if n == 15 {
                n += br.read(8) as i32;
            }
            br.skip(n as u32 * 8);
        }
    }

    fn reset_tables(&mut self) {
        if self.make_f_master().and_then(|_| self.make_f_derived()).is_err() {
            self.turnoff();
        }
    }

    /// Decode one SBR extension. `br` is positioned just after the extension type; `cnt` is the length
    /// of the fill element in bytes.
    pub fn decode_extension(&mut self, br: &mut Bits, crc: bool, cnt: usize, id_aac: u8) {
        let start = br.pos;
        self.reset = false;
        if crc {
            br.skip(10);
        }
        self.kx[0] = self.kx[1];
        self.m[0] = self.m[1];
        self.kx_and_m_pushed = true;
        if br.flag() {
            self.read_header(br);
        }
        if self.reset {
            self.reset_tables();
        }
        if self.start {
            self.read_data(br, id_aac);
        }
        let used = br.pos - start;
        if (used + 4).div_ceil(8) > cnt {
            self.turnoff();
        }
    }

    // ---- processing ----

    fn dequant(&mut self, id_aac: u8) {
        let exp2_tab = [1.0f64, std::f64::consts::SQRT_2];
        let n = self.n;
        if id_aac == TYPE_CPE && self.coupling {
            let pan_offset = if self.data[0].amp_res != 0 { 12 } else { 24 };
            for e in 1..=self.data[0].num_env {
                for k in 0..n[self.data[0].freq_res[e] as usize] as usize {
                    let (q0, q1) = (self.data[0].env_facs_q[e][k], self.data[1].env_facs_q[e][k]);
                    let (mut temp1, temp2);
                    if self.data[0].amp_res != 0 {
                        temp1 = exp2fi(q0 + 7);
                        temp2 = exp2fi(pan_offset - q1);
                    } else {
                        temp1 = (exp2fi((q0 >> 1) + 7) as f64 * exp2_tab[(q0 & 1) as usize]) as f32;
                        temp2 = (exp2fi((pan_offset - q1) >> 1) as f64 * exp2_tab[((pan_offset - q1) & 1) as usize]) as f32;
                    }
                    if temp1 > 1e20 {
                        temp1 = 1.0;
                    }
                    let fac = temp1 / (1.0 + temp2);
                    self.data[0].env_facs[e][k] = fac;
                    self.data[1].env_facs[e][k] = fac * temp2;
                }
            }
            for e in 1..=self.data[0].num_noise {
                for k in 0..self.n_q as usize {
                    let temp1 = exp2fi(NOISE_FLOOR_OFFSET - self.data[0].noise_facs_q[e][k] + 1);
                    let temp2 = exp2fi(12 - self.data[1].noise_facs_q[e][k]);
                    let fac = temp1 / (1.0 + temp2);
                    self.data[0].noise_facs[e][k] = fac;
                    self.data[1].noise_facs[e][k] = fac * temp2;
                }
            }
        } else {
            for ch in 0..(id_aac == TYPE_CPE) as usize + 1 {
                let d = &mut self.data[ch];
                for e in 1..=d.num_env {
                    for k in 0..n[d.freq_res[e] as usize] as usize {
                        let q = d.env_facs_q[e][k];
                        let mut v = if d.amp_res != 0 {
                            exp2fi(q + 6)
                        } else {
                            (exp2fi((q >> 1) + 6) as f64 * exp2_tab[(q & 1) as usize]) as f32
                        };
                        if v > 1e20 {
                            v = 1.0;
                        }
                        d.env_facs[e][k] = v;
                    }
                }
                for e in 1..=d.num_noise {
                    for k in 0..self.n_q as usize {
                        d.noise_facs[e][k] = exp2fi(NOISE_FLOOR_OFFSET - d.noise_facs_q[e][k]);
                    }
                }
            }
        }
    }

    /// One analysis QMF pass over 1024 new samples (scaled to 16-bit units); fills `W[buf]`.
    fn qmf_analysis(&mut self, ch: usize, input: &[f32], buf: usize) {
        let t = tabs();
        let d = &mut self.data[ch];
        d.analysis.copy_within(1024..1312, 0);
        for (i, &s) in input.iter().enumerate() {
            d.analysis[288 + i] = s * 32768.0;
        }
        for slot in 0..2 * SLOTS {
            let x = &d.analysis[slot * 32..slot * 32 + 320];
            let mut u = [0.0f32; 64];
            for n in 0..64 {
                let mut s = 0.0;
                for j in 0..5 {
                    let i = n + 64 * j;
                    s += t.win_ds[i] * x[319 - i];
                }
                u[n] = s;
            }
            for k in 0..32 {
                let (mut re, mut im) = (0.0f32, 0.0f32);
                for n in 0..64 {
                    let (c, s) = t.ana[k][n];
                    re += u[n] * c;
                    im += u[n] * s;
                }
                d.w[buf * 32 + slot][k] = [re * ANALYSIS_SCALE, im * ANALYSIS_SCALE];
            }
        }
    }

    fn lf_gen(&mut self, ch: usize, buf: usize) {
        let t_hfgen = 8;
        let i_f = 2 * SLOTS;
        for row in self.x_low.iter_mut() {
            *row = [[0.0; 2]; 40];
        }
        let d = &self.data[ch];
        for k in 0..self.kx[1] as usize {
            for i in t_hfgen..i_f + t_hfgen {
                self.x_low[k][i] = d.w[buf * 32 + i - t_hfgen][k];
            }
        }
        let other = 1 - buf;
        for k in 0..self.kx[0] as usize {
            for i in 0..t_hfgen {
                self.x_low[k][i] = d.w[other * 32 + i + i_f - t_hfgen][k];
            }
        }
    }

    fn autocorrelate(x: &[[f32; 2]; 40]) -> [[[f32; 2]; 2]; 3] {
        let mut real_sum2 = x[0][0] * x[2][0] + x[0][1] * x[2][1];
        let mut imag_sum2 = x[0][0] * x[2][1] - x[0][1] * x[2][0];
        let (mut real_sum1, mut imag_sum1, mut real_sum0) = (0.0f32, 0.0f32, 0.0f32);
        for i in 1..38 {
            real_sum0 += x[i][0] * x[i][0] + x[i][1] * x[i][1];
            real_sum1 += x[i][0] * x[i + 1][0] + x[i][1] * x[i + 1][1];
            imag_sum1 += x[i][0] * x[i + 1][1] - x[i][1] * x[i + 1][0];
            real_sum2 += x[i][0] * x[i + 2][0] + x[i][1] * x[i + 2][1];
            imag_sum2 += x[i][0] * x[i + 2][1] - x[i][1] * x[i + 2][0];
        }
        let mut phi = [[[0.0f32; 2]; 2]; 3];
        phi[0][1][0] = real_sum2;
        phi[0][1][1] = imag_sum2;
        phi[2][1][0] = real_sum0 + x[0][0] * x[0][0] + x[0][1] * x[0][1];
        phi[1][0][0] = real_sum0 + x[38][0] * x[38][0] + x[38][1] * x[38][1];
        phi[1][1][0] = real_sum1 + x[0][0] * x[1][0] + x[0][1] * x[1][1];
        phi[1][1][1] = imag_sum1 + x[0][0] * x[1][1] - x[0][1] * x[1][0];
        phi[0][0][0] = real_sum1 + x[38][0] * x[39][0] + x[38][1] * x[39][1];
        phi[0][0][1] = imag_sum1 + x[38][0] * x[39][1] - x[38][1] * x[39][0];
        phi
    }

    fn hf_inverse_filter(&mut self) {
        for k in 0..self.k[0] as usize {
            let phi = Self::autocorrelate(&self.x_low[k]);
            let dk = phi[2][1][0] * phi[1][0][0] - (phi[1][1][0] * phi[1][1][0] + phi[1][1][1] * phi[1][1][1]) / 1.000_001;
            if dk == 0.0 {
                self.alpha1[k] = [0.0, 0.0];
            } else {
                let temp_real = phi[0][0][0] * phi[1][1][0] - phi[0][0][1] * phi[1][1][1] - phi[0][1][0] * phi[1][0][0];
                let temp_im = phi[0][0][0] * phi[1][1][1] + phi[0][0][1] * phi[1][1][0] - phi[0][1][1] * phi[1][0][0];
                self.alpha1[k] = [temp_real / dk, temp_im / dk];
            }
            if phi[1][0][0] == 0.0 {
                self.alpha0[k] = [0.0, 0.0];
            } else {
                let a1 = self.alpha1[k];
                let temp_real = phi[0][0][0] + a1[0] * phi[1][1][0] + a1[1] * phi[1][1][1];
                let temp_im = phi[0][0][1] + a1[1] * phi[1][1][0] - a1[0] * phi[1][1][1];
                self.alpha0[k] = [-temp_real / phi[1][0][0], -temp_im / phi[1][0][0]];
            }
            let (a0, a1) = (self.alpha0[k], self.alpha1[k]);
            if a1[0] * a1[0] + a1[1] * a1[1] >= 16.0 || a0[0] * a0[0] + a0[1] * a0[1] >= 16.0 {
                self.alpha0[k] = [0.0, 0.0];
                self.alpha1[k] = [0.0, 0.0];
            }
        }
    }

    fn chirp(&mut self, ch: usize) {
        const BW_TAB: [f32; 4] = [0.0, 0.75, 0.9, 0.98];
        let n_q = self.n_q as usize;
        let d = &mut self.data[ch];
        for i in 0..n_q {
            let mut new_bw = if d.invf_mode[0][i] + d.invf_mode[1][i] == 1 { 0.6 } else { BW_TAB[d.invf_mode[0][i] as usize] };
            if new_bw < d.bw_array[i] {
                new_bw = 0.75 * new_bw + 0.25 * d.bw_array[i];
            } else {
                new_bw = 0.906_25 * new_bw + 0.093_75 * d.bw_array[i];
            }
            d.bw_array[i] = if new_bw < 0.015_625 { 0.0 } else { new_bw };
        }
    }

    fn hf_gen(&mut self, ch: usize) -> Res {
        let d = &self.data[ch];
        let (start, end) = (2 * d.t_env[0] as usize, 2 * d.t_env[d.num_env] as usize);
        let mut g = 0usize;
        let mut k = self.kx[1] as usize;
        for j in 0..self.num_patches {
            for x in 0..self.patch_num_subbands[j] as usize {
                let p = self.patch_start_subband[j] as usize + x;
                while g as i32 <= self.n_q && k as i32 >= self.f_tablenoise[g] {
                    g += 1;
                }
                if g == 0 {
                    return Err(());
                }
                g -= 1;
                let bw = d.bw_array[g];
                let (a0, a1) = (self.alpha0[p], self.alpha1[p]);
                let alpha = [a1[0] * bw * bw, a1[1] * bw * bw, a0[0] * bw, a0[1] * bw];
                for i in start..end {
                    let (i0, i1, i2) = (i + OFFSET - 2, i + OFFSET - 1, i + OFFSET);
                    let xl = &self.x_low[p];
                    let re = xl[i0][0] * alpha[0] - xl[i0][1] * alpha[1] + xl[i1][0] * alpha[2] - xl[i1][1] * alpha[3] + xl[i2][0];
                    let im = xl[i0][1] * alpha[0] + xl[i0][0] * alpha[1] + xl[i1][1] * alpha[2] + xl[i1][0] * alpha[3] + xl[i2][1];
                    self.x_high[k][i + OFFSET] = [re, im];
                }
                k += 1;
            }
        }
        if (k as i32) < self.m[1] + self.kx[1] {
            for row in self.x_high[k..(self.m[1] + self.kx[1]) as usize].iter_mut() {
                *row = [[0.0; 2]; 40];
            }
        }
        Ok(())
    }

    fn mapping(&mut self, ch: usize) -> Res {
        let kx1 = self.kx[1];
        let e_a = self.data[ch].e_a;
        let d = &mut self.data[ch];
        d.s_indexmapped[1..8].iter_mut().for_each(|r| *r = [0; 48]);
        for e in 0..d.num_env {
            let high = d.freq_res[e + 1] != 0;
            let ilim = self.n[high as usize] as usize;
            let table: &[i32] = if high { &self.f_tablehigh } else { &self.f_tablelow };
            if kx1 != table[0] {
                return Err(());
            }
            for i in 0..ilim {
                for m in table[i]..table[i + 1] {
                    self.e_origmapped[e][(m - kx1) as usize] = d.env_facs[e + 1][i];
                }
            }
            let k = (d.num_noise > 1 && d.t_env[e] >= d.t_q[1]) as usize;
            for i in 0..self.n_q as usize {
                for m in self.f_tablenoise[i]..self.f_tablenoise[i + 1] {
                    self.q_mapped[e][(m - kx1) as usize] = d.noise_facs[k + 1][i];
                }
            }
            for i in 0..self.n[1] as usize {
                if d.add_harmonic_flag {
                    let mid = ((self.f_tablehigh[i] + self.f_tablehigh[i + 1]) >> 1) - kx1;
                    let prev = d.s_indexmapped[0][mid as usize] == 1;
                    d.s_indexmapped[e + 1][mid as usize] = d.add_harmonic[i] * (e as i32 >= e_a[1] || prev) as u8;
                }
            }
            for i in 0..ilim {
                let present = (table[i]..table[i + 1]).any(|m| d.s_indexmapped[e + 1][(m - kx1) as usize] != 0);
                for m in table[i]..table[i + 1] {
                    self.s_mapped[e][(m - kx1) as usize] = present as u8;
                }
            }
        }
        d.s_indexmapped[0] = d.s_indexmapped[d.num_env];
        Ok(())
    }

    fn env_estimate(&mut self, ch: usize) {
        let kx1 = self.kx[1] as usize;
        let d = &self.data[ch];
        let sum_square = |row: &[[f32; 2]; 40], lo: usize, hi: usize| -> f32 {
            let (mut s0, mut s1) = (0.0f32, 0.0f32);
            for v in &row[lo..hi] {
                s0 += v[0] * v[0];
                s1 += v[1] * v[1];
            }
            s0 + s1
        };
        if self.interpol_freq {
            for e in 0..d.num_env {
                let recip = 0.5 / (d.t_env[e + 1] - d.t_env[e]) as f32;
                let ilb = d.t_env[e] as usize * 2 + OFFSET;
                let iub = d.t_env[e + 1] as usize * 2 + OFFSET;
                if ilb >= 40 {
                    return;
                }
                for m in 0..self.m[1] as usize {
                    self.e_curr[e][m] = sum_square(&self.x_high[m + kx1], ilb, iub) * recip;
                }
            }
        } else {
            for e in 0..d.num_env {
                let env_size = 2 * (d.t_env[e + 1] - d.t_env[e]);
                let ilb = d.t_env[e] as usize * 2 + OFFSET;
                let iub = d.t_env[e + 1] as usize * 2 + OFFSET;
                let high = d.freq_res[e + 1] != 0;
                let table: &[i32] = if high { &self.f_tablehigh } else { &self.f_tablelow };
                if ilb >= 40 {
                    return;
                }
                for p in 0..self.n[high as usize] as usize {
                    let den = env_size * (table[p + 1] - table[p]);
                    let mut sum = 0.0f32;
                    for k in table[p]..table[p + 1] {
                        sum += sum_square(&self.x_high[k as usize], ilb, iub);
                    }
                    sum /= den as f32;
                    for k in table[p]..table[p + 1] {
                        self.e_curr[e][k as usize - kx1] = sum;
                    }
                }
            }
        }
    }

    fn gain_calc(&mut self, ch: usize) {
        const LIMGAIN: [f32; 4] = [0.70795, 1.0, 1.41254, 10_000_000_000.0];
        let kx1 = self.kx[1];
        let d = &self.data[ch];
        for e in 0..d.num_env {
            let delta = (!(e as i32 == d.e_a[1] || e as i32 == d.e_a[0])) as i32 as f32;
            for k in 0..self.n_lim as usize {
                let (lo, hi) = ((self.f_tablelim[k] - kx1) as usize, (self.f_tablelim[k + 1] - kx1) as usize);
                for m in lo..hi {
                    let temp = self.e_origmapped[e][m] / (1.0 + self.q_mapped[e][m]);
                    self.q_m[e][m] = (temp * self.q_mapped[e][m]).sqrt();
                    self.s_m[e][m] = (temp * d.s_indexmapped[e + 1][m] as f32).sqrt();
                    self.gain[e][m] = if self.s_mapped[e][m] == 0 {
                        (self.e_origmapped[e][m] / ((1.0 + self.e_curr[e][m]) * (1.0 + self.q_mapped[e][m] * delta))).sqrt()
                    } else {
                        (self.e_origmapped[e][m] * self.q_mapped[e][m] / ((1.0 + self.e_curr[e][m]) * (1.0 + self.q_mapped[e][m]))).sqrt()
                    };
                    self.gain[e][m] += FLT_MIN;
                }
                let (mut s0, mut s1) = (0.0f32, 0.0f32);
                for m in lo..hi {
                    s0 += self.e_origmapped[e][m];
                    s1 += self.e_curr[e][m];
                }
                let mut gain_max = LIMGAIN[self.limiter_gains as usize] * ((FLT_EPSILON + s0) / (FLT_EPSILON + s1)).sqrt();
                gain_max = gain_max.min(100000.0);
                for m in lo..hi {
                    let q_m_max = self.q_m[e][m] * gain_max / self.gain[e][m];
                    self.q_m[e][m] = self.q_m[e][m].min(q_m_max);
                    self.gain[e][m] = self.gain[e][m].min(gain_max);
                }
                let (mut s0, mut s1) = (0.0f32, 0.0f32);
                for m in lo..hi {
                    s0 += self.e_origmapped[e][m];
                    s1 += self.e_curr[e][m] * self.gain[e][m] * self.gain[e][m]
                        + self.s_m[e][m] * self.s_m[e][m]
                        + if delta != 0.0 && self.s_m[e][m] == 0.0 { self.q_m[e][m] * self.q_m[e][m] } else { 0.0 };
                }
                let mut gain_boost = ((FLT_EPSILON + s0) / (FLT_EPSILON + s1)).sqrt();
                gain_boost = gain_boost.min(1.584_893_2);
                for m in lo..hi {
                    self.gain[e][m] *= gain_boost;
                    self.q_m[e][m] *= gain_boost;
                    self.s_m[e][m] *= gain_boost;
                }
            }
        }
    }

    fn hf_assemble(&mut self, ch: usize) {
        const H_SMOOTH: [f32; 5] = [0.333_333_33, 0.301_502_83, 0.218_169_5, 0.115_163_83, 0.031_830_5];
        let h_sl = 4 * (self.smoothing_mode == 0) as usize;
        let kx = self.kx[1] as usize;
        let m_max = self.m[1] as usize;
        let reset = self.reset;
        let ypos = self.data[ch].ypos;
        let d = &mut self.data[ch];
        let mut indexnoise = d.f_indexnoise;
        let mut indexsine = d.f_indexsine;
        let t0 = 2 * d.t_env[0] as usize;
        if reset {
            for i in 0..h_sl {
                d.g_temp[i + t0][..m_max].copy_from_slice(&self.gain[0][..m_max]);
                d.q_temp[i + t0][..m_max].copy_from_slice(&self.q_m[0][..m_max]);
            }
        } else if h_sl != 0 {
            let old = 2 * d.t_env_num_env_old as usize;
            for i in 0..4 {
                d.g_temp[i + t0] = d.g_temp[i + old];
                d.q_temp[i + t0] = d.q_temp[i + old];
            }
        }
        for e in 0..d.num_env {
            for i in 2 * d.t_env[e] as usize..2 * d.t_env[e + 1] as usize {
                d.g_temp[h_sl + i][..m_max].copy_from_slice(&self.gain[e][..m_max]);
                d.q_temp[h_sl + i][..m_max].copy_from_slice(&self.q_m[e][..m_max]);
            }
        }
        for e in 0..d.num_env {
            for i in 2 * d.t_env[e] as usize..2 * d.t_env[e + 1] as usize {
                let mut g_filt = [0.0f32; 48];
                let mut q_filt = [0.0f32; 48];
                let special = e as i32 == d.e_a[0] || e as i32 == d.e_a[1];
                if h_sl != 0 && !special {
                    for m in 0..m_max {
                        let idx1 = i + h_sl;
                        for j in 0..=h_sl {
                            g_filt[m] += d.g_temp[idx1 - j][m] * H_SMOOTH[j];
                            q_filt[m] += d.q_temp[idx1 - j][m] * H_SMOOTH[j];
                        }
                    }
                } else {
                    g_filt = d.g_temp[i + h_sl];
                    q_filt = d.q_temp[i];
                }
                let y = &mut d.y[ypos * 38 + i];
                let ixh = i + OFFSET;
                for m in 0..m_max {
                    y[kx + m][0] = self.x_high[kx + m][ixh][0] * g_filt[m];
                    y[kx + m][1] = self.x_high[kx + m][ixh][1] * g_filt[m];
                }
                if !special {
                    let phi_sign = 1.0 - 2.0 * (kx & 1) as f32;
                    let (p0, mut p1) = match indexsine & 3 {
                        0 => (1.0, 0.0),
                        1 => (0.0, phi_sign),
                        2 => (-1.0, 0.0),
                        _ => (0.0, -phi_sign),
                    };
                    let mut noise = indexnoise as usize;
                    for m in 0..m_max {
                        noise = (noise + 1) & 0x1ff;
                        let s = self.s_m[e][m];
                        if s != 0.0 {
                            y[kx + m][0] += s * p0;
                            y[kx + m][1] += s * p1;
                        } else {
                            y[kx + m][0] += q_filt[m] * NOISE_TABLE[noise][0];
                            y[kx + m][1] += q_filt[m] * NOISE_TABLE[noise][1];
                        }
                        p1 = -p1;
                    }
                } else {
                    let idx = (indexsine & 1) as usize;
                    let a = 1 - ((indexsine as i32 + (kx & 1) as i32) & 2);
                    let b = if idx == 1 { -a } else { a };
                    let mut m = 0;
                    while m + 1 < m_max {
                        y[kx + m][idx] += self.s_m[e][m] * a as f32;
                        y[kx + m + 1][idx] += self.s_m[e][m + 1] * b as f32;
                        m += 2;
                    }
                    if m_max & 1 == 1 {
                        y[kx + m][idx] += self.s_m[e][m] * a as f32;
                    }
                }
                indexnoise = (indexnoise + m_max as u32) & 0x1ff;
                indexsine = (indexsine + 1) & 3;
            }
        }
        d.f_indexnoise = indexnoise;
        d.f_indexsine = indexsine;
    }

    fn x_gen(&mut self, ch: usize) {
        let i_f = 2 * SLOTS;
        let d = &self.data[ch];
        let i_temp = (2 * d.t_env_num_env_old - i_f as i32).max(0) as usize;
        for plane in self.x[ch].iter_mut() {
            *plane = [0.0; 64];
        }
        let ypos = d.ypos;
        let (y0, y1) = ((1 - ypos) * 38, ypos * 38);
        let (kx0, kx1, m0, m1) = (self.kx[0] as usize, self.kx[1] as usize, self.m[0] as usize, self.m[1] as usize);
        // x[ch][re * 38 + slot][band]
        for k in 0..kx0 {
            for i in 0..i_temp {
                self.x[ch][i][k] = self.x_low[k][i + OFFSET][0];
                self.x[ch][38 + i][k] = self.x_low[k][i + OFFSET][1];
            }
        }
        for k in kx0..kx0 + m0 {
            for i in 0..i_temp {
                self.x[ch][i][k] = d.y[y0 + i + i_f][k][0];
                self.x[ch][38 + i][k] = d.y[y0 + i + i_f][k][1];
            }
        }
        for k in 0..kx1 {
            for i in i_temp..38 {
                self.x[ch][i][k] = self.x_low[k][i + OFFSET][0];
                self.x[ch][38 + i][k] = self.x_low[k][i + OFFSET][1];
            }
        }
        for k in kx1..kx1 + m1 {
            for i in i_temp..i_f {
                self.x[ch][i][k] = d.y[y1 + i][k][0];
                self.x[ch][38 + i][k] = d.y[y1 + i][k][1];
            }
        }
    }

    /// The synthesis QMF bank: 32 slots of 64 bands to 2048 samples.
    fn qmf_synthesis(&mut self, ch: usize, out: &mut [f32]) {
        let t = tabs();
        for slot in 0..2 * SLOTS {
            let d = &mut self.data[ch];
            d.synth.copy_within(0..1152, 128);
            let (re, im) = (&self.x[ch][slot], &self.x[ch][38 + slot]);
            for n in 0..128 {
                let mut s = 0.0f32;
                for k in 0..64 {
                    let (c, sn) = t.syn[n][k];
                    s += re[k] * c - im[k] * sn;
                }
                d.synth[n] = s * SYNTHESIS_SCALE;
            }
            for k in 0..64 {
                let mut s = 0.0f32;
                for m in 0..5 {
                    s += t.win_us[128 * m + k] * d.synth[256 * m + k] + t.win_us[128 * m + 64 + k] * d.synth[256 * m + 192 + k];
                }
                out[slot * 64 + k] = s;
            }
        }
    }

    /// Rebuild the upper band of one channel element. `core` holds 1024 samples per channel (one or
    /// two channels); the result is 2048 per channel.
    pub fn apply(&mut self, id_aac: u8, core: &[&[f32]]) -> Vec<Vec<f32>> {
        let nch = core.len();
        if id_aac != self.id_aac {
            self.turnoff();
        }
        if self.start && !self.ready_for_dequant {
            self.turnoff();
        }
        if !self.kx_and_m_pushed {
            self.kx[0] = self.kx[1];
            self.m[0] = self.m[1];
        } else {
            self.kx_and_m_pushed = false;
        }
        if self.start {
            self.dequant(id_aac);
            self.ready_for_dequant = false;
        }
        for ch in 0..nch {
            let buf = self.data[ch].ypos;
            self.qmf_analysis(ch, core[ch], buf);
            self.lf_gen(ch, buf);
            self.data[ch].ypos ^= 1;
            if self.start {
                self.hf_inverse_filter();
                self.chirp(ch);
                let ok = self.hf_gen(ch).is_ok() && self.mapping(ch).is_ok();
                if !ok {
                    self.turnoff();
                } else {
                    self.env_estimate(ch);
                    self.gain_calc(ch);
                    self.hf_assemble(ch);
                }
            }
            self.x_gen(ch);
        }
        let mut out = vec![vec![0.0f32; 2048]; nch];
        for ch in 0..nch {
            self.qmf_synthesis(ch, &mut out[ch]);
        }
        out
    }
}

/// Analysis and synthesis scales: the pair reconstructs at unity, and the subband samples carry the
/// 16-bit units the envelope data is written in.
const ANALYSIS_SCALE: f32 = 2.0;
const SYNTHESIS_SCALE: f32 = 1.0 / (64.0 * 32768.0);

#[cfg(test)]
mod tests {
    use super::*;

    /// Without an SBR header the bank only upsamples: a tone at the core rate must come back at half
    /// its frequency, at unit gain, with the aliasing images well below it.
    #[test]
    fn the_filterbank_pair_upsamples_a_tone_cleanly() {
        const FREQ: f64 = 0.1;
        let mut sbr = Sbr::new(22050);
        let mut out: Vec<f32> = Vec::new();
        for f in 0..8 {
            let frame: Vec<f32> = (0..1024).map(|i| (0.4 * (2.0 * std::f64::consts::PI * FREQ * (f * 1024 + i) as f64).sin()) as f32).collect();
            out.extend(&sbr.apply(TYPE_SCE, &[&frame])[0]);
        }
        // least-squares fit of a sinusoid at the halved frequency over the steady state
        let w = std::f64::consts::PI * FREQ;
        let (mut ss, mut cc, mut sc, mut xs, mut xc, mut xx) = (0.0f64, 0.0, 0.0, 0.0, 0.0, 0.0);
        for i in 4000..12000 {
            let (sn, cs) = ((w * i as f64).sin(), (w * i as f64).cos());
            let x = out[i] as f64;
            ss += sn * sn;
            cc += cs * cs;
            sc += sn * cs;
            xs += x * sn;
            xc += x * cs;
            xx += x * x;
        }
        let det = ss * cc - sc * sc;
        let (a, b) = ((xs * cc - xc * sc) / det, (xc * ss - xs * sc) / det);
        let amplitude = (a * a + b * b).sqrt();
        let residual_db = 10.0 * ((xx - (a * xs + b * xc)) / xx).log10();
        assert!((amplitude - 0.4).abs() < 0.004, "gain {amplitude}");
        assert!(residual_db < -35.0, "residual {residual_db} dB");
    }
}
