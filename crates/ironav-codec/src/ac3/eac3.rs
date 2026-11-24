//! The E-AC-3 (Dolby Digital Plus, ETSI TS 102 366 annex E) parts of the decoder that AC-3 does not
//! have: the audio frame element (the frame-level side information), spectral extension, and the
//! adaptive hybrid transform with its gain adaptive quantisation and vector quantisation. The block
//! decoding itself (`block.rs`) is shared with AC-3. Translated from FFmpeg's `libavcodec/eac3dec.c`
//! and `ac3dec.c`, which are licensed under the LGPL-2.1 or later (Copyright (c) 2007 Bartlomiej
//! Wolowiec, 2008 Justin Ruggles, and the other FFmpeg developers).
//!
//! Transient pre-noise processing data is parsed and ignored, as in FFmpeg. Enhanced coupling is
//! refused (`block.rs`), as in FFmpeg.

use super::bits::BitReader;
use super::block::{band_structure, Res, State, CPL, NCH};
use super::eac3_tables::*;
use super::header::Header;
use super::tables::{DB_PER_BIT, FAST_DECAY, FLOOR, SLOW_DECAY, SLOW_GAIN};

const MODE_MONO: usize = 1;
const EXP_REUSE: u8 = 0;

/// `lrint(M_SQRT2*cos(2*M_PI/12)*(1<<23))`
const COEFF_0: i64 = 10273905;
/// `lrint(M_SQRT2*(1<<23))`
const COEFF_1: i64 = 11863283;
/// `lrint(M_SQRT2*cos(5*M_PI/12)*(1<<23))`
const COEFF_2: i64 = 3070444;

/// The 6-point inverse DCT of the pre-mantissas, in 24-bit fixed point.
pub(super) fn idct6(m: &mut [i32; 6]) {
    let p: [i64; 6] = [m[0] as i64, m[1] as i64, m[2] as i64, m[3] as i64, m[4] as i64, m[5] as i64];
    let odd1 = p[1] - p[3] - p[5];
    let even2 = (p[2] * COEFF_0) >> 23;
    let tmp = (p[4] * COEFF_1) >> 23;
    let odd0 = ((p[1] + p[5]) * COEFF_2) >> 23;
    let even0 = p[0] + (tmp >> 1);
    let even1 = p[0] - tmp;
    let t = even0;
    let even0 = t + even2;
    let even2 = t - even2;
    let t = odd0;
    let odd0 = t + p[1] + p[3];
    let odd2 = t + p[5] - p[3];
    m[0] = (even0 + odd0) as i32;
    m[1] = (even1 + odd1) as i32;
    m[2] = (even2 + odd2) as i32;
    m[3] = (even2 - odd2) as i32;
    m[4] = (even1 - odd1) as i32;
    m[5] = (even0 - odd0) as i32;
}

impl State {
    /// Sets the frame-level syntax flags of a plain AC-3 frame.
    pub(super) fn begin_ac3_frame(&mut self) {
        self.eac3 = false;
        self.frame_type = 2;
        self.snr_offset_strategy = 2;
        self.block_switch_syntax = true;
        self.dither_flag_syntax = true;
        self.bit_allocation_syntax = true;
        self.fast_gain_syntax = false;
        self.first_cpl_leak = false;
        self.dba_syntax = true;
        self.skip_syntax = true;
        self.channel_uses_aht = [false; NCH];
    }

    /// Parses the audio frame element of an E-AC-3 frame (E.1.3.2), which follows the bit stream
    /// information and holds the side information of the frame's blocks.
    pub(super) fn parse_audio_frame(&mut self, r: &mut BitReader, hdr: &Header) -> Res<()> {
        if hdr.sr_code == 3 {
            // FFmpeg does not decode these either: the standard does not say how the bit allocation
            // works at half rates
            return Err("E-AC-3 reduced sample rates (fscod 3) are not supported");
        }
        let num_blocks = self.num_blocks;
        let fbw = self.fbw;
        self.eac3 = true;
        self.frame_type = hdr.frame_type;

        let (ac3_exponent_strategy, parse_aht_info) = if num_blocks == 6 { (r.bit(), r.bit()) } else { (true, false) };
        self.snr_offset_strategy = r.bits(2) as u8;
        let parse_transient_proc_info = r.bit();
        self.block_switch_syntax = r.bit();
        if !self.block_switch_syntax {
            self.block_switch = [false; NCH];
        }
        self.dither_flag_syntax = r.bit();
        if !self.dither_flag_syntax {
            for ch in 1..=fbw {
                self.dither_flag[ch] = true;
            }
        }
        self.dither_flag[CPL] = false;
        self.dither_flag[self.lfe_ch] = false;
        self.bit_allocation_syntax = r.bit();
        if !self.bit_allocation_syntax {
            self.ba.slow_decay = SLOW_DECAY[2] as i32;
            self.ba.fast_decay = FAST_DECAY[1] as i32;
            self.ba.slow_gain = SLOW_GAIN[1] as i32;
            self.ba.db_per_bit = DB_PER_BIT[2] as i32;
            self.ba.floor = FLOOR[7] as i32;
        }
        self.fast_gain_syntax = r.bit();
        self.dba_syntax = r.bit();
        self.skip_syntax = r.bit();
        let parse_spx_atten_data = r.bit();

        // coupling strategy occurrence and coupling use per block
        let mut num_cpl_blocks = 0;
        if self.channel_mode > 1 {
            for blk in 0..num_blocks {
                self.cpl_strategy_exists[blk] = blk == 0 || r.bit();
                if self.cpl_strategy_exists[blk] {
                    self.cpl_in_use[blk] = r.bit();
                } else {
                    self.cpl_in_use[blk] = self.cpl_in_use[blk - 1];
                }
                num_cpl_blocks += self.cpl_in_use[blk] as usize;
            }
        } else {
            self.cpl_in_use = [false; 6];
        }

        // exponent strategies
        if ac3_exponent_strategy {
            for blk in 0..num_blocks {
                for ch in (!self.cpl_in_use[blk]) as usize..=fbw {
                    self.exp_strategy[blk][ch] = r.bits(2) as u8;
                }
            }
        } else {
            let first = !(self.channel_mode > 1 && num_cpl_blocks > 0) as usize;
            for ch in first..=fbw {
                let code = r.bits(5) as usize;
                for blk in 0..6 {
                    self.exp_strategy[blk][ch] = FRM_EXPSTR[code][blk];
                }
            }
        }
        if self.lfe_on {
            for blk in 0..num_blocks {
                self.exp_strategy[blk][self.lfe_ch] = r.bits(1) as u8;
            }
        }
        // the exponent strategies the stream had before it was converted from AC-3
        if self.frame_type == 0 && (num_blocks == 6 || r.bit()) {
            r.skip(5 * fbw);
        }

        // which channels use the adaptive hybrid transform: only those that keep their exponents
        // from the first block (and, for the coupling channel, all blocks use one coupling strategy)
        if parse_aht_info {
            self.channel_uses_aht[CPL] = false;
            for ch in (num_cpl_blocks != 6) as usize..=self.channels {
                let mut use_aht = true;
                for blk in 1..6 {
                    if self.exp_strategy[blk][ch] != EXP_REUSE || (ch == 0 && self.cpl_strategy_exists[blk]) {
                        use_aht = false;
                        break;
                    }
                }
                self.channel_uses_aht[ch] = use_aht && r.bit();
            }
        } else {
            self.channel_uses_aht = [false; NCH];
        }

        // the SNR offset of the whole frame
        if self.snr_offset_strategy == 0 {
            let csnr = (r.bits(6) as i32 - 15) << 4;
            let snr = (csnr + r.bits(4) as i32) << 2;
            for ch in 0..=self.channels {
                self.snr_offset[ch] = snr;
            }
        }

        // transient pre-noise processing: parsed and ignored
        if parse_transient_proc_info {
            for _ in 1..=fbw {
                if r.bit() {
                    r.skip(10 + 8);
                }
            }
        }

        // spectral extension attenuation
        for ch in 1..=fbw {
            self.spx_atten_code[ch] = if parse_spx_atten_data && r.bit() { r.bits(5) as i32 } else { -1 };
        }

        // block start information, which is not used
        if num_blocks > 1 && r.bit() {
            let log2 = (usize::BITS - 1 - (hdr.frame_size - 2).max(1).leading_zeros()) as usize;
            r.skip((num_blocks - 1) * (4 + log2));
        }

        for ch in 1..=fbw {
            self.first_spx_coords[ch] = true;
            self.first_cpl_coords[ch] = true;
        }
        self.first_cpl_leak = true;
        Ok(())
    }

    /// The spectral extension strategy of a block (E.1.2.3, spxstre onward).
    pub(super) fn spx_strategy(&mut self, r: &mut BitReader, blk: usize) -> Res<()> {
        let fbw = self.fbw;
        if self.channel_mode == MODE_MONO {
            self.channel_uses_spx[1] = true;
        } else {
            let mut bits = r.bits(fbw as u32);
            for ch in (1..=fbw).rev() {
                self.channel_uses_spx[ch] = bits & 1 != 0;
                bits >>= 1;
            }
        }
        let dst_start = r.bits(2) as usize;
        let mut start_subband = r.bits(3) as usize + 2;
        if start_subband > 7 {
            start_subband += start_subband - 7;
        }
        let mut end_subband = r.bits(3) as usize + 5;
        if end_subband > 7 {
            end_subband += end_subband - 7;
        }
        let dst_start_freq = dst_start * 12 + 25;
        let src_start_freq = start_subband * 12 + 25;
        let dst_end_freq = end_subband * 12 + 25;
        if start_subband >= end_subband {
            return Err("invalid spectral extension range");
        }
        if dst_start_freq >= src_start_freq {
            return Err("invalid spectral extension copy start bin");
        }
        self.spx_dst_start_freq = dst_start_freq;
        self.spx_src_start_freq = src_start_freq;
        self.spx_dst_end_freq = dst_end_freq;
        let mut sizes = [0u8; 17];
        let mut st = self.spx_band_struct;
        self.num_spx_bands = band_structure(r, blk, true, start_subband, end_subband, &DEFAULT_SPX_BAND_STRUCT, &mut st, &mut sizes);
        self.spx_band_struct = st;
        self.spx_band_sizes = sizes;
        Ok(())
    }

    /// The spectral extension coordinates of a block: each band's noise and signal blending factors,
    /// scaled by the coordinate.
    pub(super) fn spx_coordinates(&mut self, r: &mut BitReader) {
        for ch in 1..=self.fbw {
            if !self.channel_uses_spx[ch] {
                self.first_spx_coords[ch] = true;
                continue;
            }
            if self.first_spx_coords[ch] || r.bit() {
                self.first_spx_coords[ch] = false;
                let spx_blend = r.bits(5) as f32 * (1.0f32 / 32.0);
                let master = r.bits(2) as i32 * 3;
                let mut bin = self.spx_src_start_freq;
                for bnd in 0..self.num_spx_bands {
                    let bandsize = self.spx_band_sizes[bnd] as usize;
                    let nratio = (((bin + (bandsize >> 1)) as f32) / self.spx_dst_end_freq as f32 - spx_blend).clamp(0.0, 1.0);
                    // the noise is scaled by sqrt(3) to give it unit variance
                    let nblend = (3.0f32 * nratio).sqrt();
                    let sblend = (1.0f32 - nratio).sqrt();
                    bin += bandsize;
                    let exp = r.bits(4) as i32;
                    let mut mant = r.bits(2) as i32;
                    if exp == 15 {
                        mant <<= 1;
                    } else {
                        mant += 4;
                    }
                    mant <<= 25 - exp - master;
                    let coord = mant as f32 * (1.0f32 / (1 << 23) as f32);
                    self.spx_noise_blend[ch][bnd] = nblend * coord;
                    self.spx_signal_blend[ch][bnd] = sblend * coord;
                }
            }
        }
    }

    /// Spectral extension of the high bins of each channel that uses it: the lower bins are copied
    /// up, notched at the wrap points, and scaled and blended with noise band by band.
    pub(super) fn apply_spx(&mut self) -> Res<()> {
        let (dst_start, src_start) = (self.spx_dst_start_freq, self.spx_src_start_freq);
        let mut wrapflag = [false; 17];
        wrapflag[0] = true;
        let mut copy_sizes: Vec<usize> = Vec::with_capacity(40);
        let mut bin = dst_start;
        for bnd in 0..self.num_spx_bands {
            let bandsize = self.spx_band_sizes[bnd] as usize;
            if bin + bandsize > src_start {
                copy_sizes.push(bin - dst_start);
                bin = dst_start;
                wrapflag[bnd] = true;
            }
            let mut i = 0;
            while i < bandsize {
                if bin == src_start {
                    copy_sizes.push(bin - dst_start);
                    bin = dst_start;
                }
                let copysize = (bandsize - i).min(src_start - bin);
                bin += copysize;
                i += copysize;
            }
        }
        copy_sizes.push(bin - dst_start);
        let total: usize = self.spx_band_sizes[..self.num_spx_bands].iter().map(|&s| s as usize).sum();
        if src_start + total > 256 || copy_sizes.iter().sum::<usize>() != total {
            return Err("spectral extension runs past the end of the spectrum");
        }

        for ch in 1..=self.fbw {
            if !self.channel_uses_spx[ch] {
                continue;
            }
            // copy the coefficients of the normal bands into the extension bands
            let mut bin = src_start;
            for &size in &copy_sizes {
                self.transform_coeffs[ch].copy_within(dst_start..dst_start + size, bin);
                bin += size;
            }

            // the RMS energy of each band
            let mut rms = [0f32; 17];
            let mut bin = src_start;
            for bnd in 0..self.num_spx_bands {
                let bandsize = self.spx_band_sizes[bnd] as usize;
                let mut accum = 0.0f32;
                for _ in 0..bandsize {
                    let c = self.transform_coeffs[ch][bin];
                    accum += c * c;
                    bin += 1;
                }
                rms[bnd] = (accum / bandsize as f32).sqrt();
            }

            // a notch filter where the normal bands meet the extension and at all wrap points
            if self.spx_atten_code[ch] >= 0 {
                let atten = &SPX_ATTEN[self.spx_atten_code[ch] as usize];
                let mut bin = src_start - 2;
                for bnd in 0..self.num_spx_bands {
                    if wrapflag[bnd] {
                        let c = &mut self.transform_coeffs[ch][bin..bin + 5];
                        c[0] *= atten[0];
                        c[1] *= atten[1];
                        c[2] *= atten[2];
                        c[3] *= atten[1];
                        c[4] *= atten[0];
                    }
                    bin += self.spx_band_sizes[bnd] as usize;
                }
            }

            // scale by the signal blend and add the noise blend
            let mut bin = src_start;
            for bnd in 0..self.num_spx_bands {
                let nscale = self.spx_noise_blend[ch][bnd] * rms[bnd] * (-1.0f32 / 2147483648.0f32);
                let sscale = self.spx_signal_blend[ch][bnd];
                for _ in 0..self.spx_band_sizes[bnd] {
                    let noise = nscale * (self.lfg.get() as i32 as f32);
                    self.transform_coeffs[ch][bin] *= sscale;
                    self.transform_coeffs[ch][bin] += noise;
                    bin += 1;
                }
            }
        }
        Ok(())
    }

    /// The mantissas of an adaptive hybrid transform channel: the six blocks' coefficients of each
    /// bin, vector quantised or gain adaptively quantised, decoded together in the frame's first block
    /// and transformed back from the 6-point DCT into `pre_mantissa`.
    pub(super) fn decode_aht_ch(&mut self, r: &mut BitReader, ch: usize) {
        let gaq_mode = r.bits(2) as usize;
        let end_bap: u8 = if gaq_mode < 2 { 12 } else { 17 };
        let (start, end) = (self.start_freq[ch], self.end_freq[ch]);

        // the gain codes of the bins with a hebap from 8 to end_bap
        let mut gaq_gain = [0u8; 260];
        let mut gs = 0;
        if gaq_mode == 1 || gaq_mode == 2 {
            for bin in start..end {
                let b = self.bap[ch][bin];
                if b > 7 && b < end_bap {
                    gaq_gain[gs] = (r.bit() as u8) << (gaq_mode - 1);
                    gs += 1;
                }
            }
        } else if gaq_mode == 3 {
            // three codes in 5 bits
            let mut gc = 2;
            for bin in start..end {
                let b = self.bap[ch][bin];
                if b > 7 && b < 17 {
                    if gc == 2 {
                        let code = (r.bits(5) as usize).min(26);
                        gaq_gain[gs] = (code / 9) as u8;
                        gaq_gain[gs + 1] = ((code % 9) / 3) as u8;
                        gaq_gain[gs + 2] = ((code % 9) % 3) as u8;
                        gs += 3;
                        gc = 0;
                    } else {
                        gc += 1;
                    }
                }
            }
        }

        let mut gs = 0;
        for bin in start..end {
            let hebap = self.bap[ch][bin] as usize;
            let bits = BITS_VS_HEBAP[hebap.min(19)] as u32;
            let mut pre = [0i32; 6];
            if hebap == 0 {
                // zero-mantissa dithering
                for p in pre.iter_mut() {
                    *p = (self.lfg.get() & 0x7FFFFF) as i32 - 0x400000;
                }
            } else if hebap < 8 {
                // vector quantisation
                let v = r.bits(bits) as usize;
                let table = MANTISSA_VQ[hebap];
                let row = table.get(v).copied().unwrap_or([0; 6]);
                for (p, &m) in pre.iter_mut().zip(row.iter()) {
                    *p = (m as i32) << 8;
                }
            } else {
                // gain adaptive quantisation
                let log_gain = if gaq_mode != 0 && hebap < end_bap as usize {
                    gs += 1;
                    gaq_gain[gs - 1] as u32
                } else {
                    0
                };
                let gbits = bits - log_gain;
                for p in pre.iter_mut() {
                    let mut mant = r.sbits(gbits);
                    if log_gain != 0 && mant == -(1 << (gbits - 1)) {
                        // a large mantissa
                        let mbits = bits - (2 - log_gain);
                        mant = r.sbits(mbits);
                        mant = ((mant as u32) << (23 - (mbits - 1))) as i32;
                        // remap the value to correct for the asymmetric quantisation
                        let b = if mant >= 0 { 1i32 << (23 - log_gain) } else { (GAQ_REMAP_2_4_B[hebap - 8][log_gain as usize - 1] as i32) << 8 };
                        let a = GAQ_REMAP_2_4_A[hebap - 8][log_gain as usize - 1] as i64;
                        mant = mant.wrapping_add(((a * mant as i64) >> 15) as i32).wrapping_add(b);
                    } else {
                        // a small mantissa, no GAQ, or Gk=1
                        mant = mant.wrapping_mul(1 << (24 - bits));
                        if log_gain == 0 {
                            // remap the value for no GAQ or Gk=1
                            mant = mant.wrapping_add(((GAQ_REMAP_1[hebap - 8] as i64 * mant as i64) >> 15) as i32);
                        }
                    }
                    *p = mant;
                }
            }
            idct6(&mut pre);
            self.pre_mantissa[ch][bin] = pre;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idct6_of_a_dc_value_is_flat() {
        let mut m = [1 << 20, 0, 0, 0, 0, 0];
        idct6(&mut m);
        assert_eq!(m, [1 << 20; 6]);
    }

    #[test]
    fn idct6_inverts_the_cosine_basis() {
        // a lone first-order term is an odd function over the six outputs
        let mut m = [0, 1 << 20, 0, 0, 0, 0];
        idct6(&mut m);
        for i in 0..3 {
            assert_eq!(m[i], -m[5 - i], "{m:?}");
        }
        assert!(m[0] > m[1] && m[1] > m[2] && m[2] > 0, "{m:?}");
    }

    #[test]
    fn tables_have_the_sizes_of_the_standard() {
        assert_eq!(MANTISSA_VQ.map(|t| t.len()), [0, 4, 8, 16, 32, 128, 256, 512]);
        assert_eq!(FRM_EXPSTR[0], [1, 0, 0, 0, 0, 0]);
        assert_eq!(FRM_EXPSTR[31], [3; 6]);
        assert_eq!(HEBAP_TAB[63], 19);
        assert!((SPX_ATTEN[0][0] - 2f32.powf(-1.0 / 15.0)).abs() < 1e-6);
        assert!((SPX_ATTEN[31][2] - 2f32.powf(-96.0 / 15.0)).abs() < 1e-6);
    }
}
