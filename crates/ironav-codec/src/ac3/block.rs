//! The state of an AC-3 decoder and the decoding of one audio block: its side information
//! (ATSC A/52 section 5.4.3), the exponents and bit allocation, the mantissas and the dequantised
//! coefficients (section 7), coupling, rematrixing and the inverse transform. Translated from
//! FFmpeg's `libavcodec/ac3dec.c` and `ac3dec_float.c`, with the spectral extension, enhanced
//! coupling and the E-AC-3 syntax left out (they are not part of AC-3) and no downmix.

use super::alloc::{self, AllocParams, Dba};
use super::bits::BitReader;
use super::synth::{fmul_window, Synth, BLOCK};
use super::tables::*;

/// The coupling channel's index in the per-channel arrays; the full-bandwidth channels are 1 to 5
/// and the LFE channel follows them.
pub(super) const CPL: usize = 0;
/// Per-channel arrays hold the coupling channel and up to six channels.
pub(super) const NCH: usize = 7;

const EXP_REUSE: u8 = 0;
const EXP_D45: u8 = 3;
const DBA_NEW: u8 = 1;
const DBA_NONE: u8 = 2;
const DBA_RESERVED: u8 = 3;

const MODE_DUALMONO: usize = 0;
const MODE_STEREO: usize = 2;
const MODE_3F: usize = 3;
const MODE_2F1R: usize = 4;
const MODE_3F1R: usize = 5;
const MODE_2F2R: usize = 6;
const MODE_3F2R: usize = 7;

/// The scale of each exponent: 2^-e.
const SCALE: [f32; 25] = [
    1.0, 0.5, 0.25, 0.125, 0.0625, 0.03125, 0.015625, 0.0078125, 0.00390625, 0.001953125, 0.0009765625, 0.00048828125,
    0.000244140625, 0.0001220703125, 0.00006103515625, 0.000030517578125, 0.0000152587890625, 0.00000762939453125,
    0.000003814697265625, 0.0000019073486328125, 0.00000095367431640625, 0.000000476837158203125,
    0.0000002384185791015625, 0.00000011920928955078125, 0.000000059604644775390625,
];

/// The decoding error of a block, as a message.
pub(super) type Res<T> = Result<T, &'static str>;

/// The decoder's state. Most of it carries from block to block and frame to frame, as in FFmpeg.
pub(super) struct State {
    // the frame's configuration
    pub sr_code: usize,
    pub channel_mode: usize,
    pub lfe_on: bool,
    pub fbw: usize,
    pub channels: usize,
    pub lfe_ch: usize,
    pub num_blocks: usize,

    pub ba: AllocParams,

    // side information
    pub dynamic_range: [f32; 2],
    pub block_switch: [bool; NCH],
    pub dither_flag: [bool; NCH],
    pub cpl_in_use: [bool; 6],
    pub channel_in_cpl: [bool; NCH],
    pub phase_flags_in_use: bool,
    pub phase_flags: [bool; 18],
    pub num_cpl_bands: usize,
    pub cpl_band_sizes: [u8; 18],
    pub first_cpl_coords: [bool; NCH],
    pub cpl_coords: [[i32; 18]; NCH],
    pub num_rematrixing_bands: usize,
    pub rematrixing_flags: [bool; 4],
    pub num_exp_groups: [usize; NCH],
    pub exp_strategy: [[u8; NCH]; 6],
    pub start_freq: [usize; NCH],
    pub end_freq: [usize; NCH],
    pub snr_offset: [i32; NCH],
    pub fast_gain: [i32; NCH],
    pub dba_mode: [u8; NCH],
    pub dba_nsegs: [usize; NCH],
    pub dba_offsets: [[u8; 8]; NCH],
    pub dba_lengths: [[u8; 8]; NCH],
    pub dba_values: [[u8; 8]; NCH],

    // per-channel working arrays
    pub dexps: Vec<[i8; 256]>,
    pub bap: Vec<[u8; 256]>,
    pub psd: Vec<[i16; 256]>,
    pub band_psd: Vec<[i16; 50]>,
    pub mask: Vec<[i16; 50]>,
    pub coeffs: Vec<[f32; 256]>,
    pub transform_coeffs: Vec<[f32; 256]>,
    /// The overlap each channel's next block adds to its output.
    pub delay: Vec<[f32; 256]>,
    /// Decoded samples of each channel for the frame, in channel order (not the output order).
    pub output: Vec<[f32; 6 * BLOCK]>,
    /// Whether the delay holds the downmixed (stereo) overlap, as FFmpeg tracks it. Without a
    /// downmix, FFmpeg clears this at the first block with mixed transforms, and so do we.
    pub downmixed: bool,

    pub lfg: Lfg,
    pub synth: Synth,
}

/// FFmpeg's lagged Fibonacci generator for the dither of zero-bit mantissas, seeded with 0.
pub(super) struct Lfg {
    state: [u32; 64],
    index: u32,
}

impl Lfg {
    pub(super) fn new() -> Self {
        let mut state = [0u32; 64];
        state[8..].copy_from_slice(&LFG_INIT_STATE);
        Lfg { state, index: 0 }
    }

    /// `av_lfg_get`: the next number, from the two lags 24 and 55 back.
    fn get(&mut self) -> u32 {
        let i = self.index;
        let v = self.state[(i.wrapping_sub(24) & 63) as usize].wrapping_add(self.state[(i.wrapping_sub(55) & 63) as usize]);
        self.state[(i & 63) as usize] = v;
        self.index = i.wrapping_add(1);
        v
    }
}

impl State {
    pub(super) fn new() -> Self {
        State {
            sr_code: 0,
            channel_mode: 0,
            lfe_on: false,
            fbw: 0,
            channels: 0,
            lfe_ch: 0,
            num_blocks: 6,
            ba: AllocParams::default(),
            dynamic_range: [1.0; 2],
            block_switch: [false; NCH],
            dither_flag: [false; NCH],
            cpl_in_use: [false; 6],
            channel_in_cpl: [false; NCH],
            phase_flags_in_use: false,
            phase_flags: [false; 18],
            num_cpl_bands: 0,
            cpl_band_sizes: [0; 18],
            first_cpl_coords: [true; NCH],
            cpl_coords: [[0; 18]; NCH],
            num_rematrixing_bands: 0,
            rematrixing_flags: [false; 4],
            num_exp_groups: [0; NCH],
            exp_strategy: [[EXP_REUSE; NCH]; 6],
            start_freq: [0; NCH],
            end_freq: [0; NCH],
            snr_offset: [0; NCH],
            fast_gain: [0; NCH],
            dba_mode: [DBA_NONE; NCH],
            dba_nsegs: [0; NCH],
            dba_offsets: [[0; 8]; NCH],
            dba_lengths: [[0; 8]; NCH],
            dba_values: [[0; 8]; NCH],
            dexps: vec![[0; 256]; NCH],
            bap: vec![[0; 256]; NCH],
            psd: vec![[0; 256]; NCH],
            band_psd: vec![[0; 50]; NCH],
            mask: vec![[0; 50]; NCH],
            coeffs: vec![[0.0; 256]; NCH],
            transform_coeffs: vec![[0.0; 256]; NCH],
            delay: vec![[0.0; 256]; NCH],
            output: vec![[0.0; 6 * BLOCK]; NCH],
            downmixed: true,
            lfg: Lfg::new(),
            synth: Synth::new(),
        }
    }

    /// Decodes block `blk` of the frame (its side information, mantissas and inverse transform).
    pub(super) fn decode_block(&mut self, r: &mut BitReader, blk: usize) -> Res<()> {
        let fbw = self.fbw;
        let channel_mode = self.channel_mode;
        let mut stages = [0u8; NCH];
        let mut different_transforms = false;

        // block switch flags
        for ch in 1..=fbw {
            self.block_switch[ch] = r.bit();
            if ch > 1 && self.block_switch[ch] != self.block_switch[1] {
                different_transforms = true;
            }
        }

        // dithering flags
        for ch in 1..=fbw {
            self.dither_flag[ch] = r.bit();
        }

        // dynamic range (two words for dual mono, which reads the second channel's first)
        let mut i = (channel_mode == MODE_DUALMONO) as usize;
        loop {
            if r.bit() {
                let range_bits = r.bits(8) as usize;
                self.dynamic_range[i] = dynamic_range_tab(range_bits);
            } else if blk == 0 {
                self.dynamic_range[i] = 1.0;
            }
            if i == 0 {
                break;
            }
            i -= 1;
        }

        // coupling strategy
        if r.bit() {
            stages = [3; NCH];
            self.coupling_strategy(r, blk)?;
        } else if blk == 0 {
            return Err("new coupling strategy must be present in block 0");
        } else {
            self.cpl_in_use[blk] = self.cpl_in_use[blk - 1];
        }
        let cpl_in_use = self.cpl_in_use[blk];

        // coupling coordinates
        if cpl_in_use {
            self.coupling_coordinates(r, blk)?;
        }

        // stereo rematrixing
        if channel_mode == MODE_STEREO {
            if r.bit() {
                self.num_rematrixing_bands = 4;
                if cpl_in_use && self.start_freq[CPL] <= 61 {
                    self.num_rematrixing_bands -= 1 + (self.start_freq[CPL] == 37) as usize;
                }
                for bnd in 0..self.num_rematrixing_bands {
                    self.rematrixing_flags[bnd] = r.bit();
                }
            } else if blk == 0 {
                // FFmpeg warns and carries on without rematrixing
                self.num_rematrixing_bands = 0;
            }
        }

        // exponent strategies of each channel
        let first = (!cpl_in_use) as usize;
        for ch in first..=self.channels {
            let width = if ch == self.lfe_ch { 1 } else { 2 };
            self.exp_strategy[blk][ch] = r.bits(width) as u8;
            if self.exp_strategy[blk][ch] != EXP_REUSE {
                stages[ch] = 3;
            }
        }

        // channel bandwidth
        for ch in 1..=fbw {
            self.start_freq[ch] = 0;
            let strategy = self.exp_strategy[blk][ch];
            if strategy != EXP_REUSE {
                let prev = self.end_freq[ch];
                if self.channel_in_cpl[ch] {
                    self.end_freq[ch] = self.start_freq[CPL];
                } else {
                    let code = r.bits(6);
                    if code > 60 {
                        return Err("bandwidth code above 60");
                    }
                    self.end_freq[ch] = code as usize * 3 + 73;
                }
                let group_size = 3usize << (strategy - 1);
                self.num_exp_groups[ch] = (self.end_freq[ch] + group_size - 4) / group_size;
                if blk > 0 && self.end_freq[ch] != prev {
                    stages = [3; NCH];
                }
            }
        }
        if cpl_in_use && self.exp_strategy[blk][CPL] != EXP_REUSE {
            let group_size = 3usize << (self.exp_strategy[blk][CPL] - 1);
            self.num_exp_groups[CPL] = (self.end_freq[CPL] - self.start_freq[CPL]) / group_size;
        }

        // exponents of each channel
        for ch in first..=self.channels {
            let strategy = self.exp_strategy[blk][ch];
            if strategy != EXP_REUSE {
                let shift = (ch == CPL) as u32;
                let absexp = (r.bits(4) << shift) as i32;
                self.dexps[ch][0] = absexp as i8;
                let base = self.start_freq[ch] + (ch != CPL) as usize;
                let ngrps = self.num_exp_groups[ch];
                if base + ngrps * 3 * group_width(strategy) > 256 {
                    return Err("exponents run past the end of the spectrum");
                }
                decode_exponents(r, strategy, ngrps, absexp, &mut self.dexps[ch][base..])?;
                if ch != CPL && ch != self.lfe_ch {
                    r.skip(2); // gainrng
                }
            }
        }

        // bit allocation information
        if r.bit() {
            self.ba.slow_decay = SLOW_DECAY[r.bits(2) as usize] as i32 >> self.ba.sr_shift;
            self.ba.fast_decay = FAST_DECAY[r.bits(2) as usize] as i32 >> self.ba.sr_shift;
            self.ba.slow_gain = SLOW_GAIN[r.bits(2) as usize] as i32;
            self.ba.db_per_bit = DB_PER_BIT[r.bits(2) as usize] as i32;
            self.ba.floor = FLOOR[r.bits(3) as usize] as i32;
            for s in stages.iter_mut().take(self.channels + 1).skip(first) {
                *s = (*s).max(2);
            }
        } else if blk == 0 {
            return Err("new bit allocation info must be present in block 0");
        }

        // snr offsets and fast gains (the signal-to-mask ratios)
        if r.bit() {
            let csnr = (r.bits(6) as i32 - 15) << 4;
            for ch in first..=self.channels {
                let snr = (csnr + r.bits(4) as i32) << 2;
                if blk > 0 && self.snr_offset[ch] != snr {
                    stages[ch] = stages[ch].max(1);
                }
                self.snr_offset[ch] = snr;
                let prev = self.fast_gain[ch];
                self.fast_gain[ch] = FAST_GAIN[r.bits(3) as usize] as i32;
                if blk > 0 && prev != self.fast_gain[ch] {
                    stages[ch] = stages[ch].max(2);
                }
            }
        } else if blk == 0 {
            return Err("new snr offsets must be present in block 0");
        }

        // coupling leak
        if cpl_in_use {
            if r.bit() {
                let fl = r.bits(3) as i32;
                let sl = r.bits(3) as i32;
                if blk > 0 && (fl != self.ba.cpl_fast_leak || sl != self.ba.cpl_slow_leak) {
                    stages[CPL] = stages[CPL].max(2);
                }
                self.ba.cpl_fast_leak = fl;
                self.ba.cpl_slow_leak = sl;
            } else if blk == 0 {
                return Err("new coupling leak info must be present in block 0");
            }
        }

        // delta bit allocation
        if r.bit() {
            for ch in first..=fbw {
                self.dba_mode[ch] = r.bits(2) as u8;
                if self.dba_mode[ch] == DBA_RESERVED {
                    return Err("delta bit allocation strategy reserved");
                }
                stages[ch] = stages[ch].max(2);
            }
            for ch in first..=fbw {
                if self.dba_mode[ch] == DBA_NEW {
                    let nsegs = r.bits(3) as usize + 1;
                    self.dba_nsegs[ch] = nsegs;
                    for seg in 0..nsegs {
                        self.dba_offsets[ch][seg] = r.bits(5) as u8;
                        self.dba_lengths[ch][seg] = r.bits(4) as u8;
                        self.dba_values[ch][seg] = r.bits(3) as u8;
                    }
                    stages[ch] = stages[ch].max(2);
                }
            }
        } else if blk == 0 {
            for ch in 0..=self.channels {
                self.dba_mode[ch] = DBA_NONE;
            }
        }

        // bit allocation of each channel
        for ch in first..=self.channels {
            if stages[ch] > 2 {
                alloc::psd(&self.dexps[ch], self.start_freq[ch], self.end_freq[ch], &mut self.psd[ch], &mut self.band_psd[ch]);
            }
            if stages[ch] > 1 {
                let dba = Dba {
                    mode: self.dba_mode[ch],
                    nsegs: self.dba_nsegs[ch],
                    offsets: &self.dba_offsets[ch],
                    lengths: &self.dba_lengths[ch],
                    values: &self.dba_values[ch],
                };
                let ok = alloc::mask(
                    &self.ba,
                    &self.band_psd[ch],
                    self.start_freq[ch],
                    self.end_freq[ch],
                    self.fast_gain[ch],
                    ch == self.lfe_ch,
                    &dba,
                    &mut self.mask[ch],
                );
                if !ok {
                    return Err("error in bit allocation");
                }
            }
            if stages[ch] > 0 {
                alloc::bap(
                    &self.mask[ch],
                    &self.psd[ch],
                    self.start_freq[ch],
                    self.end_freq[ch],
                    self.snr_offset[ch],
                    self.ba.floor,
                    &BAP_TAB,
                    &mut self.bap[ch],
                );
            }
        }

        // unused dummy data
        if r.bit() {
            let skipl = r.bits(9) as usize;
            r.skip(8 * skipl);
        }

        // the mantissas, which also uncouple the coupled channels
        self.decode_transform_coeffs(r);

        if channel_mode == MODE_STEREO {
            self.rematrix();
        }

        // headroom and dynamic range
        for ch in 1..=self.channels {
            let audio_channel = if channel_mode == MODE_DUALMONO && ch <= 2 { 2 - ch } else { 0 };
            let gain = self.dynamic_range[audio_channel] * (1.0f32 / 4194304.0f32);
            for (t, &c) in self.transform_coeffs[ch].iter_mut().zip(self.coeffs[ch].iter()) {
                *t = c * gain;
            }
        }

        // the inverse transform; with mixed block sizes FFmpeg first restores the upmixed delay
        if different_transforms && self.downmixed {
            self.downmixed = false;
            self.upmix_delay();
        }
        self.do_imdct(blk);
        Ok(())
    }

    fn coupling_strategy(&mut self, r: &mut BitReader, blk: usize) -> Res<()> {
        self.cpl_in_use[blk] = r.bit();
        if self.cpl_in_use[blk] {
            if self.channel_mode < MODE_STEREO {
                return Err("coupling not allowed in mono or dual-mono");
            }
            for ch in 1..=self.fbw {
                self.channel_in_cpl[ch] = r.bit();
            }
            if self.channel_mode == MODE_STEREO {
                self.phase_flags_in_use = r.bit();
            }
            let start_subband = r.bits(4) as usize;
            let end_subband = r.bits(4) as usize + 3;
            if start_subband >= end_subband {
                return Err("invalid coupling range");
            }
            self.start_freq[CPL] = start_subband * 12 + 37;
            self.end_freq[CPL] = end_subband * 12 + 37;
            let mut sizes = [0u8; 18];
            self.num_cpl_bands = band_structure(r, start_subband, end_subband, &mut sizes);
            self.cpl_band_sizes = sizes;
        } else {
            for ch in 1..=self.fbw {
                self.channel_in_cpl[ch] = false;
                self.first_cpl_coords[ch] = true;
            }
            self.phase_flags_in_use = false;
        }
        Ok(())
    }

    fn coupling_coordinates(&mut self, r: &mut BitReader, blk: usize) -> Res<()> {
        let mut cpl_coords_exist = false;
        for ch in 1..=self.fbw {
            if self.channel_in_cpl[ch] {
                if r.bit() {
                    self.first_cpl_coords[ch] = false;
                    cpl_coords_exist = true;
                    let master = 3 * r.bits(2) as i32;
                    for bnd in 0..self.num_cpl_bands {
                        let exp = r.bits(4) as i32;
                        let mant = r.bits(4) as i32;
                        let v = if exp == 15 { mant << 22 } else { (mant + 16) << 21 };
                        self.cpl_coords[ch][bnd] = v >> (exp + master);
                    }
                } else if blk == 0 {
                    return Err("new coupling coordinates must be present in block 0");
                }
            } else {
                self.first_cpl_coords[ch] = true;
            }
        }
        if self.channel_mode == MODE_STEREO && cpl_coords_exist {
            for bnd in 0..self.num_cpl_bands {
                self.phase_flags[bnd] = self.phase_flags_in_use && r.bit();
            }
        }
        Ok(())
    }

    fn decode_transform_coeffs(&mut self, r: &mut BitReader) {
        let mut m = MantGroups::default();
        let mut got_cpl = false;
        for ch in 1..=self.channels {
            self.decode_coeffs_ch(r, ch, &mut m);
            let end = if self.channel_in_cpl[ch] {
                if !got_cpl {
                    self.decode_coeffs_ch(r, CPL, &mut m);
                    self.calc_cpl_coeffs();
                    got_cpl = true;
                }
                self.end_freq[CPL]
            } else {
                self.end_freq[ch]
            };
            for v in &mut self.coeffs[ch][end..] {
                *v = 0.0;
            }
        }
        // zero-bit mantissas of coupled channels that do not dither
        for ch in 1..=self.fbw {
            if !self.dither_flag[ch] && self.channel_in_cpl[ch] {
                for i in self.start_freq[CPL]..self.end_freq[CPL] {
                    if self.bap[CPL][i] == 0 {
                        self.coeffs[ch][i] = 0.0;
                    }
                }
            }
        }
    }

    /// The mantissas of one channel, dequantised with its exponents.
    fn decode_coeffs_ch(&mut self, r: &mut BitReader, ch: usize, m: &mut MantGroups) {
        let dither = ch == CPL || self.dither_flag[ch];
        for freq in self.start_freq[ch]..self.end_freq[ch] {
            let bap = self.bap[ch][freq] as usize;
            let mantissa: i32 = match bap {
                0 => {
                    if dither {
                        ((((self.lfg.get() >> 8) * 181) >> 8) as i32).wrapping_sub(5931008)
                    } else {
                        0
                    }
                }
                1 => {
                    if m.b1 > 0 {
                        m.b1 -= 1;
                        m.b1_mant[m.b1]
                    } else {
                        let t = bap1(r.bits(5) as usize);
                        m.b1_mant[1] = t[1];
                        m.b1_mant[0] = t[2];
                        m.b1 = 2;
                        t[0]
                    }
                }
                2 => {
                    if m.b2 > 0 {
                        m.b2 -= 1;
                        m.b2_mant[m.b2]
                    } else {
                        let t = bap2(r.bits(7) as usize);
                        m.b2_mant[1] = t[1];
                        m.b2_mant[0] = t[2];
                        m.b2 = 2;
                        t[0]
                    }
                }
                3 => sym(r.bits(3) as i32, 7),
                4 => {
                    if m.b4 {
                        m.b4 = false;
                        m.b4_mant
                    } else {
                        let t = bap4(r.bits(7) as usize);
                        m.b4_mant = t[1];
                        m.b4 = true;
                        t[0]
                    }
                }
                5 => sym(r.bits(4) as i32, 15),
                _ => {
                    // 6 to 15: the sign-extended code, shifted up to the 24-bit scale
                    let bits = QUANTIZATION[bap.min(15)] as u32;
                    r.sbits(bits) << (24 - bits)
                }
            };
            let e = (self.dexps[ch][freq] as usize).min(24);
            self.coeffs[ch][freq] = mantissa as f32 * SCALE[e];
        }
    }

    fn calc_cpl_coeffs(&mut self) {
        let mut bin = self.start_freq[CPL];
        for band in 0..self.num_cpl_bands {
            let band_start = bin;
            let band_end = bin + self.cpl_band_sizes[band] as usize;
            for ch in 1..=self.fbw {
                if self.channel_in_cpl[ch] {
                    let cpl_coord = self.cpl_coords[ch][band] as f32 * (1.0f32 / (1 << 23) as f32);
                    for b in band_start..band_end {
                        self.coeffs[ch][b] = self.coeffs[CPL][b] * cpl_coord;
                    }
                    if ch == 2 && self.phase_flags[band] {
                        for b in band_start..band_end {
                            self.coeffs[2][b] = -self.coeffs[2][b];
                        }
                    }
                }
            }
            bin = band_end;
        }
    }

    /// Stereo rematrixing: sum and difference of the two channels in the flagged bands.
    fn rematrix(&mut self) {
        let end = self.end_freq[1].min(self.end_freq[2]);
        for bnd in 0..self.num_rematrixing_bands {
            if self.rematrixing_flags[bnd] {
                let bndend = end.min(REMATRIX_BAND[bnd + 1] as usize);
                for i in REMATRIX_BAND[bnd] as usize..bndend {
                    let tmp0 = self.coeffs[1][i];
                    self.coeffs[1][i] += self.coeffs[2][i];
                    self.coeffs[2][i] = tmp0 - self.coeffs[2][i];
                }
            }
        }
    }

    /// The inverse transform of each channel into the frame's output, for block `blk`.
    fn do_imdct(&mut self, blk: usize) {
        let at = blk * BLOCK;
        for ch in 1..=self.channels {
            let c = ch - 1;
            let mut tmp = [0f32; BLOCK];
            if self.block_switch[ch] {
                // two 128-point transforms (FFmpeg's do_imdct): the even coefficients give the window's
                // input, the odd ones the overlap for the next block. Each is folded as the 256-point
                // case is, at half the size.
                let mut x = [0f32; 128];
                for (i, v) in x.iter_mut().enumerate() {
                    *v = self.transform_coeffs[ch][2 * i];
                }
                let y = self.synth.imdct128(&x);
                for j in 0..64 {
                    tmp[j] = y[63 - j];
                }
                for t in 0..64 {
                    tmp[64 + t] = -y[128 + t];
                }
                fmul_window(&mut self.output[c][at..at + BLOCK], &self.delay[c], &tmp, &self.synth.window, 128);
                for (i, v) in x.iter_mut().enumerate() {
                    *v = self.transform_coeffs[ch][2 * i + 1];
                }
                let y = self.synth.imdct128(&x);
                for j in 0..64 {
                    self.delay[c][j] = y[63 - j];
                }
                for t in 0..64 {
                    self.delay[c][64 + t] = -y[128 + t];
                }
            } else {
                let y = self.synth.imdct256(&self.transform_coeffs[ch]);
                for j in 0..128 {
                    tmp[j] = y[127 - j];
                }
                for t in 0..128 {
                    tmp[128 + t] = -y[256 + t];
                }
                fmul_window(&mut self.output[c][at..at + BLOCK], &self.delay[c], &tmp, &self.synth.window, 128);
                self.delay[c][..128].copy_from_slice(&tmp[128..256]);
            }
        }
    }

    /// Restores the overlap of a stereo or multichannel delay from the mono or stereo one, as
    /// FFmpeg's `ac3_upmix_delay` does.
    fn upmix_delay(&mut self) {
        match self.channel_mode {
            MODE_DUALMONO | MODE_STEREO => self.delay[1] = self.delay[0],
            MODE_2F2R => {
                self.delay[3] = [0.0; 256];
                self.delay[2] = [0.0; 256];
            }
            MODE_2F1R => self.delay[2] = [0.0; 256],
            MODE_3F2R => {
                self.delay[4] = [0.0; 256];
                self.delay[3] = [0.0; 256];
                self.delay[2] = self.delay[1];
                self.delay[1] = [0.0; 256];
            }
            MODE_3F1R => {
                self.delay[3] = [0.0; 256];
                self.delay[2] = self.delay[1];
                self.delay[1] = [0.0; 256];
            }
            MODE_3F => {
                self.delay[2] = self.delay[1];
                self.delay[1] = [0.0; 256];
            }
            _ => {}
        }
    }
}

/// The mantissas still to be used from the last grouped code of each kind.
#[derive(Default)]
struct MantGroups {
    b1_mant: [i32; 2],
    b2_mant: [i32; 2],
    b4_mant: i32,
    b1: usize,
    b2: usize,
    b4: bool,
}

/// Symmetric dequantisation of a code with `levels` levels, to the 24-bit scale.
fn sym(code: i32, levels: i32) -> i32 {
    ((code - (levels >> 1)) * (1 << 24)) / levels
}

/// Three mantissas from a 5-bit grouped code (levels 3).
fn bap1(code: usize) -> [i32; 3] {
    [sym(code as i32 / 9, 3), sym((code as i32 % 9) / 3, 3), sym((code as i32 % 9) % 3, 3)]
}

/// Three mantissas from a 7-bit grouped code (levels 5).
fn bap2(code: usize) -> [i32; 3] {
    let c = code as i32;
    [sym(c / 25, 5), sym((c % 25) / 5, 5), sym((c % 25) % 5, 5)]
}

/// Two mantissas from a 7-bit grouped code (levels 11).
fn bap4(code: usize) -> [i32; 2] {
    let c = code as i32;
    [sym(c / 11, 11), sym(c % 11, 11)]
}

/// The exponents of a block's channel, from their groups, each group expanded to its bins.
fn decode_exponents(r: &mut BitReader, strategy: u8, ngrps: usize, absexp: i32, out: &mut [i8]) -> Res<()> {
    let group = group_width(strategy);
    let mut dexp = [0i32; 256];
    for grp in 0..ngrps {
        let expacc = r.bits(7) as usize;
        if expacc >= 125 {
            return Err("exponent group code out of range");
        }
        let c = expacc as i32;
        dexp[3 * grp] = c / 25;
        dexp[3 * grp + 1] = (c % 25) / 5;
        dexp[3 * grp + 2] = (c % 25) % 5;
    }
    let mut prevexp = absexp;
    let mut j = 0;
    for &d in dexp.iter().take(ngrps * 3) {
        prevexp += d - 2;
        if !(0..=24).contains(&prevexp) {
            return Err("exponent out of range");
        }
        for _ in 0..group {
            out[j] = prevexp as i8;
            j += 1;
        }
    }
    Ok(())
}

/// The number of bins one exponent covers: 1, 2 or 4 for the strategies D15, D25 and D45.
fn group_width(strategy: u8) -> usize {
    (strategy + (strategy == EXP_D45) as u8) as usize
}

/// The bands of a coupling range from its band structure (one bit per subband after the first,
/// 1 to join the previous band). Returns the number of bands; `sizes` gets their widths in bins.
fn band_structure(r: &mut BitReader, start_subband: usize, end_subband: usize, sizes: &mut [u8; 18]) -> usize {
    let n_sub = end_subband - start_subband;
    let mut join = [0u8; 18];
    for b in join.iter_mut().take(n_sub.saturating_sub(1)) {
        *b = r.bit() as u8;
    }
    let mut n_bands = n_sub;
    let mut sz = [0u8; 18];
    sz[0] = 12;
    let mut bnd = 0;
    for sub in 1..n_sub {
        if join[sub - 1] != 0 {
            n_bands -= 1;
            sz[bnd] += 12;
        } else {
            bnd += 1;
            sz[bnd] = 12;
        }
    }
    sizes[..n_bands].copy_from_slice(&sz[..n_bands]);
    n_bands
}

/// The dynamic range scale of a range code (ATSC A/52 section 7.7.1), as FFmpeg's table.
fn dynamic_range_tab(i: usize) -> f32 {
    let v = (i as i32 >> 5) - ((i as i32 >> 7) << 3) - 5;
    2f32.powi(v) * (((i & 0x1F) | 0x20) as f32)
}
