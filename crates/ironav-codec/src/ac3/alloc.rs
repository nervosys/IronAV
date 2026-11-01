//! Bit allocation: the power spectral density of a channel's exponents, the masking curve, and the
//! bit allocation pointer of each coefficient (ATSC A/52 section 7.2). Translated from FFmpeg's
//! `libavcodec/ac3.c` (`ff_ac3_bit_alloc_calc_psd`, `ff_ac3_bit_alloc_calc_mask`) and
//! `libavcodec/ac3dsp.c` (`ac3_bit_alloc_calc_bap_c`).

use super::tables::{BAND_START, BIN_TO_BAND, HEARING_THRESHOLD, LOG_ADD};

/// The bit allocation parameters of the bit stream (a channel's masking curve depends on them).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct AllocParams {
    pub sr_code: usize,
    pub sr_shift: u32,
    pub slow_gain: i32,
    pub slow_decay: i32,
    pub fast_decay: i32,
    pub db_per_bit: i32,
    pub floor: i32,
    pub cpl_fast_leak: i32,
    pub cpl_slow_leak: i32,
}

/// The delta bit allocation of one channel: its segments, from the bit stream.
pub(super) struct Dba<'a> {
    pub mode: u8,
    pub nsegs: usize,
    pub offsets: &'a [u8; 8],
    pub lengths: &'a [u8; 8],
    pub values: &'a [u8; 8],
}

const DBA_REUSE: u8 = 0;
const DBA_NEW: u8 = 1;

/// The power spectral density of bins `start..end` from their exponents, and the band PSD (the
/// integration of the PSD over each band).
pub(super) fn psd(exp: &[i8], start: usize, end: usize, psd: &mut [i16; 256], band_psd: &mut [i16; 50]) {
    for bin in start..end {
        psd[bin] = (3072 - ((exp[bin] as i32) << 7)) as i16;
    }
    // the first 28 bands have one coefficient each
    let mut bin = start;
    while bin < end.min(28) {
        band_psd[bin] = psd[bin];
        bin += 1;
    }
    if bin >= end {
        return;
    }
    let mut band = BIN_TO_BAND[bin] as usize;
    loop {
        let mut v = psd[bin] as i32;
        bin += 1;
        let band_end = (BAND_START[band + 1] as usize).min(end);
        while bin < band_end {
            let max = v.max(psd[bin] as i32);
            // log-add of the two PSD values
            let adr = (max - ((v + psd[bin] as i32 + 1) >> 1)).min(255);
            v = max + LOG_ADD[adr as usize] as i32;
            bin += 1;
        }
        band_psd[band] = v as i16;
        band += 1;
        if end <= BAND_START[band] as usize {
            break;
        }
    }
}

fn lowcomp1(a: i32, b0: i32, b1: i32, c: i32) -> i32 {
    if b0 + 256 == b1 {
        c
    } else if b0 > b1 {
        (a - 64).max(0)
    } else {
        a
    }
}

fn lowcomp(a: i32, b0: i32, b1: i32, bin: usize) -> i32 {
    if bin < 7 {
        lowcomp1(a, b0, b1, 384)
    } else if bin < 20 {
        lowcomp1(a, b0, b1, 320)
    } else {
        (a - 128).max(0)
    }
}

/// The masking curve of a channel into `mask`. Returns false when the delta bit allocation is
/// out of range (the frame is then damaged).
pub(super) fn mask(p: &AllocParams, band_psd: &[i16; 50], start: usize, end: usize, fast_gain: i32, is_lfe: bool, dba: &Dba, mask: &mut [i16; 50]) -> bool {
    if end == 0 {
        return false;
    }
    let mut excite = [0i32; 50];
    let band_start = BIN_TO_BAND[start] as usize;
    let band_end = BIN_TO_BAND[end - 1] as usize + 1;
    let bp = |b: usize| band_psd[b] as i32;
    let begin;
    let mut fastleak: i32 = 0;
    let mut slowleak: i32 = 0;

    if band_start == 0 {
        let mut lc = 0;
        lc = lowcomp1(lc, bp(0), bp(1), 384);
        excite[0] = bp(0) - fast_gain - lc;
        lc = lowcomp1(lc, bp(1), bp(2), 384);
        excite[1] = bp(1) - fast_gain - lc;
        let mut b0 = 7;
        for band in 2..7 {
            if !(is_lfe && band == 6) {
                lc = lowcomp1(lc, bp(band), bp(band + 1), 384);
            }
            fastleak = bp(band) - fast_gain;
            slowleak = bp(band) - p.slow_gain;
            excite[band] = fastleak - lc;
            if !(is_lfe && band == 6) && bp(band) <= bp(band + 1) {
                b0 = band + 1;
                break;
            }
        }
        let end1 = band_end.min(22);
        for band in b0..end1 {
            if !(is_lfe && band == 6) {
                lc = lowcomp(lc, bp(band), bp(band + 1), band);
            }
            fastleak = (fastleak - p.fast_decay).max(bp(band) - fast_gain);
            slowleak = (slowleak - p.slow_decay).max(bp(band) - p.slow_gain);
            excite[band] = (fastleak - lc).max(slowleak);
        }
        begin = 22;
    } else {
        // the coupling channel
        begin = band_start;
        fastleak = (p.cpl_fast_leak << 8) + 768;
        slowleak = (p.cpl_slow_leak << 8) + 768;
    }

    for band in begin..band_end {
        fastleak = (fastleak - p.fast_decay).max(bp(band) - fast_gain);
        slowleak = (slowleak - p.slow_decay).max(bp(band) - p.slow_gain);
        excite[band] = fastleak.max(slowleak);
    }

    // the masking curve
    for band in band_start..band_end {
        let tmp = p.db_per_bit - bp(band);
        if tmp > 0 {
            excite[band] += tmp >> 2;
        }
        let hearing = HEARING_THRESHOLD[band >> p.sr_shift][p.sr_code] as i32;
        mask[band] = hearing.max(excite[band]) as i16;
    }

    // delta bit allocation
    if dba.mode == DBA_REUSE || dba.mode == DBA_NEW {
        if dba.nsegs > 8 {
            return false;
        }
        let mut band = band_start;
        for seg in 0..dba.nsegs {
            band += dba.offsets[seg] as usize;
            if band >= 50 || dba.lengths[seg] as usize > 50 - band {
                return false;
            }
            let v = dba.values[seg] as i32;
            let delta = if v >= 4 { (v - 3) * 128 } else { (v - 4) * 128 };
            for _ in 0..dba.lengths[seg] {
                mask[band] = mask[band].wrapping_add(delta as i16);
                band += 1;
            }
        }
    }
    true
}

/// The bit allocation pointer of each coefficient of `start..end`, from the masking curve.
pub(super) fn bap(mask: &[i16; 50], psd: &[i16; 256], start: usize, end: usize, snr_offset: i32, floor: i32, bap_tab: &[u8; 64], bap: &mut [u8; 256]) {
    // an snr offset of -960 gives no bits to any coefficient
    if snr_offset == -960 {
        *bap = [0; 256];
        return;
    }
    let address = |bin: usize, m: i32| -> usize { (((psd[bin] as i32 - m) >> 5).clamp(0, 63)) as usize };
    let mut bin = start;
    // the first 28 bands have one coefficient each
    while bin < end.min(28) {
        let m = ((mask[bin] as i32 - snr_offset - floor).max(0) & 0x1FE0) + floor;
        bap[bin] = bap_tab[address(bin, m)];
        bin += 1;
    }
    if bin >= end {
        return;
    }
    let mut band = BIN_TO_BAND[bin] as usize;
    loop {
        let m = ((mask[band] as i32 - snr_offset - floor).max(0) & 0x1FE0) + floor;
        band += 1;
        let band_end = (BAND_START[band] as usize).min(end);
        while bin < band_end {
            bap[bin] = bap_tab[address(bin, m)];
            bin += 1;
        }
        if end <= band_end {
            break;
        }
    }
}
