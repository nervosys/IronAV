//! AC-3 (ATSC A/52) encoder: 32, 44.1 and 48 kHz, any of the channel modes 1/0 to 3/2 with or
//! without the LFE channel, constant bit rate, at FFmpeg's default settings: dialnorm -31, copyright
//! off, original on, stereo rematrixing, channel coupling where FFmpeg enables it, the exponent
//! strategies FFmpeg chooses, and the bit allocation parameters FFmpeg codes.
//!
//! A translation of FFmpeg's `libavcodec/ac3enc.c`, `ac3enc_float.c`, `ac3enc_template.c` and the
//! encoder helpers of `ac3dsp.c` and `ac3tab.c` (LGPL-2.1 or later; Copyright (c) 2000 Fabrice
//! Bellard, 2006-2011 Justin Ruggles, 2006-2010 Prakash Punnoor). The float encoder is the one
//! ported: the input is float samples and the transform is float. The bit allocation (`alloc.rs`),
//! the KBD window (`synth.rs`), the DCT-IV (`imdct.rs`) and every table are shared with the
//! decoder. The forward MDCT is the decoder's DCT-IV behind the TDAC fold, scaled as FFmpeg's
//! `av_tx` MDCT is (-2 / 512); FFmpeg's own transform is an FFT with its own rounding, so the
//! coefficients, and so the frames, are close to FFmpeg's but not guaranteed to match its bits.

use super::alloc::{self, AllocParams, Dba};
use super::synth::kbd_window;
use super::tables::{BAP_TAB, DB_PER_BIT, FAST_DECAY, FAST_GAIN, FLOOR, FRAME_SIZE, REMATRIX_BAND, SLOW_DECAY, SLOW_GAIN};
use crate::imdct::Imdct;
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, Result};
use std::collections::VecDeque;

/// Samples per block and blocks per frame; a frame is 1536 samples per channel.
const BLOCK: usize = 256;
const NUM_BLOCKS: usize = 6;
pub const FRAME_SAMPLES: usize = BLOCK * NUM_BLOCKS;
/// Samples the decoder outputs before the first input sample: the first block has no history, so
/// the signal is delayed by one block (FFmpeg's `initial_padding`).
pub const LEAD_IN: i64 = BLOCK as i64;
/// Channel slots: slot 0 is the coupling channel, slots 1 to 6 the channels in AC-3 order (LFE last).
const SLOTS: usize = 7;
const CPL: usize = 0;
/// Exponent strategies (EXP_NEW is only a marker while the strategies are chosen).
const EXP_REUSE: u8 = 0;
const EXP_NEW: u8 = 1;
const EXP_D15: u8 = 1;
const EXP_D25: u8 = 2;
const EXP_D45: u8 = 3;
/// The strategy of a run of blocks by its length, for six blocks per frame (`exp_strategy_reuse_tab[3]`).
const EXP_STRATEGY_RUN: [u8; 6] = [EXP_D45, EXP_D25, EXP_D25, EXP_D15, EXP_D15, EXP_D15];
const EXP_DIFF_THRESHOLD: i32 = 500;

const SAMPLE_RATES: [u32; 3] = [48000, 44100, 32000];
/// Full-bandwidth channels of each channel mode (acmod).
const CHANNELS: [usize; 8] = [2, 1, 2, 3, 3, 4, 4, 5];
/// The input channel (in FFmpeg's native order for the layout) that each AC-3 channel takes, for
/// [acmod][lfe]: `ac3_enc_channel_map` of FFmpeg's `ac3enc.c`.
const CHANNEL_MAP: [[&[usize]; 2]; 8] = [
    [&[0, 1], &[0, 1, 2]],
    [&[0], &[0, 1]],
    [&[0, 1], &[0, 1, 2]],
    [&[0, 2, 1], &[0, 2, 1, 3]],
    [&[0, 1, 2], &[0, 1, 3, 2]],
    [&[0, 2, 1, 3], &[0, 2, 1, 4, 3]],
    [&[0, 1, 2, 3], &[0, 1, 3, 4, 2]],
    [&[0, 2, 1, 3, 4], &[0, 2, 1, 4, 5, 3]],
];
/// The bit rates in kbit/s, by bit rate code.
const BITRATE_TAB: [u32; 19] = [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640];
/// The bandwidth code by [fbw channels - 1][sample rate code][bit rate code] (`ac3_bandwidth_tab`).
const BANDWIDTH_TAB: [[[u8; 19]; 3]; 5] = [
    [
        [0, 0, 0, 12, 16, 32, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48],
        [0, 0, 0, 16, 20, 36, 56, 56, 56, 56, 56, 56, 56, 56, 56, 56, 56, 56, 56],
        [0, 0, 0, 32, 40, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60],
    ],
    [
        [0, 0, 0, 0, 0, 0, 0, 20, 24, 32, 48, 48, 48, 48, 48, 48, 48, 48, 48],
        [0, 0, 0, 0, 0, 0, 4, 24, 28, 36, 56, 56, 56, 56, 56, 56, 56, 56, 56],
        [0, 0, 0, 0, 0, 0, 20, 44, 52, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60],
    ],
    [
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 16, 24, 32, 40, 48, 48, 48, 48, 48, 48],
        [0, 0, 0, 0, 0, 0, 0, 0, 4, 20, 28, 36, 44, 56, 56, 56, 56, 56, 56],
        [0, 0, 0, 0, 0, 0, 0, 0, 20, 40, 48, 60, 60, 60, 60, 60, 60, 60, 60],
    ],
    [
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 12, 24, 32, 48, 48, 48, 48, 48, 48],
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 16, 28, 36, 56, 56, 56, 56, 56, 56],
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 48, 60, 60, 60, 60, 60, 60, 60],
    ],
    [
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 8, 20, 32, 40, 48, 48, 48, 48],
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 12, 24, 36, 44, 56, 56, 56, 56],
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 28, 44, 60, 60, 60, 60, 60, 60],
    ],
];
/// The coupling start band by [acmod - 2][sample rate code][bit rate code], -1 for no coupling
/// (`ac3_coupling_start_tab`).
const COUPLING_START_TAB: [[[i8; 19]; 3]; 6] = [
    [
        [0, 0, 0, 0, 0, 0, 0, 1, 1, 7, 8, 11, 12, -1, -1, -1, -1, -1, -1],
        [0, 0, 0, 0, 0, 0, 1, 3, 5, 7, 10, 12, 13, -1, -1, -1, -1, -1, -1],
        [0, 0, 0, 0, 1, 2, 2, 9, 13, 15, -1, -1, -1, -1, -1, -1, -1, -1, -1],
    ],
    [
        [0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 6, 9, 11, 12, 13, -1, -1, -1, -1],
        [0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 6, 9, 11, 12, 13, -1, -1, -1, -1],
        [-1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1],
    ],
    [
        [0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 6, 9, 11, 12, 13, -1, -1, -1, -1],
        [0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 6, 9, 11, 12, 13, -1, -1, -1, -1],
        [-1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1],
    ],
    [
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 2, 10, 11, 11, 12, 12, 14, -1],
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 2, 10, 11, 11, 12, 12, 14, -1],
        [-1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1],
    ],
    [
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 2, 10, 11, 11, 12, 12, 14, -1],
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 2, 10, 11, 11, 12, 12, 14, -1],
        [-1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1],
    ],
    [
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 6, 8, 11, 12, 12, -1, -1],
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 6, 8, 11, 12, 12, -1, -1],
        [-1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1],
    ],
];
/// Which coupling bands are merged into the previous one (`ff_eac3_default_cpl_band_struct`).
const CPL_BAND_STRUCT: [u8; 18] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 0, 1, 1, 1, 1, 1];
/// Bits per mantissa for each bit allocation pointer (`ff_ac3_bap_bits`).
const BAP_BITS: [usize; 16] = [0, 0, 0, 3, 0, 4, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16];
/// Bits of the optional header fields that depend on the channel mode (`frame_bits_inc`).
const FRAME_BITS_INC: [usize; 8] = [0, 0, 2, 2, 2, 4, 2, 4];
const CRC16_POLY: u32 = (1 << 0) | (1 << 2) | (1 << 15) | (1 << 16);
const COEF_MAX: f32 = 16777215.0 / 16777216.0;
const COEF_MIN: f32 = -16777215.0 / 16777216.0;
const NEW_CPL_COORD_THRESHOLD: f32 = 0.03;
const NO_DBA: [u8; 8] = [0; 8];

/// The channel mode and LFE flag for a channel count, as IronAV's AAC encoder reads the counts:
/// 1 mono, 2 stereo, 3 a centre and a pair, 4 with a back centre (3/1), 5 3/2, 6 3/2 with LFE.
/// Other counts have no AC-3 mode here (2.1, quad and the rest are chosen with `with_mode`).
pub fn mode_for_channels(channels: usize) -> Option<(usize, bool)> {
    Some(match channels {
        1 => (1, false),
        2 => (2, false),
        3 => (3, false),
        4 => (5, false),
        5 => (7, false),
        6 => (7, true),
        _ => return None,
    })
}

/// The bit rate FFmpeg's encoder uses when none is given, by full-bandwidth channel count.
pub fn default_bitrate(fbw: usize) -> u32 {
    match fbw {
        1 => 96_000,
        2 => 192_000,
        3 => 320_000,
        4 => 384_000,
        _ => 448_000,
    }
}

/// The Layer II-style snap: the AC-3 bit rate (kbit/s) nearest to `kbps`, on the table's values.
pub fn snap_bitrate(kbps: u32) -> u32 {
    let mut best = BITRATE_TAB[0];
    for &b in &BITRATE_TAB {
        if b.abs_diff(kbps) < best.abs_diff(kbps) {
            best = b;
        }
    }
    best
}

/// Side information of one block, as FFmpeg's `AC3Block` holds it.
#[derive(Clone, Copy, Default)]
struct BlockState {
    cpl_in_use: bool,
    new_cpl_strategy: bool,
    new_snr_offsets: bool,
    new_cpl_leak: bool,
    channel_in_cpl: [bool; SLOTS],
    end_freq: [usize; SLOTS],
    new_cpl_coords: [bool; SLOTS],
    cpl_master_exp: [u8; SLOTS],
    cpl_coord_exp: [[u8; 16]; SLOTS],
    cpl_coord_mant: [[u8; 16]; SLOTS],
    new_rem: bool,
    num_rem_bands: usize,
    rem_flags: [bool; 4],
}

/// The mantissa grouping state of one block: the pointers to the first value of each group of
/// bap 1, 2 and 4 mantissas, and how many of the group have been seen.
#[derive(Default)]
struct Mant {
    p1: (usize, usize),
    c1: u8,
    p2: (usize, usize),
    c2: u8,
    p4: (usize, usize),
    c4: u8,
}

/// The AC-3 encoder. Input is interleaved float samples in FFmpeg's channel order for the layout;
/// output is one AC-3 frame per 1536 samples per channel.
pub struct Ac3Encoder {
    sample_rate: u32,
    sr_code: usize,
    acmod: usize,
    lfe_on: bool,
    channels: usize,
    fbw: usize,
    channel_map: Vec<usize>,
    has_center: bool,
    has_surround: bool,
    bit_rate: u32,
    frame_size_code: usize,
    frame_size_min: usize,
    frame_size: usize,
    bits_written: i64,
    samples_written: i64,
    bandwidth_code: usize,
    start_freq: [usize; SLOTS],
    cpl_end_freq: usize,
    cpl_enabled: bool,
    cpl_on: bool,
    phase_flags_in_use: bool,
    phase_flags: [bool; 18],
    num_cpl_subbands: usize,
    num_cpl_bands: usize,
    cpl_band_sizes: [usize; 18],
    rematrixing_enabled: bool,
    alloc: AllocParams,
    fast_gain_code: usize,
    coarse_snr: i32,
    fine_snr: [i32; SLOTS],
    frame_bits_fixed: usize,
    frame_bits: usize,
    exponent_bits: usize,
    crc_inv: [u32; 2],
    blocks: [BlockState; NUM_BLOCKS],
    exp_strategy: [[u8; NUM_BLOCKS]; SLOTS],
    exp_ref_block: [[usize; NUM_BLOCKS]; SLOTS],
    mdct: Box<[[[f32; BLOCK]; NUM_BLOCKS]; SLOTS]>,
    fixed: Box<[[[i32; BLOCK]; NUM_BLOCKS]; SLOTS]>,
    exp: Box<[[[u8; BLOCK]; NUM_BLOCKS]; SLOTS]>,
    grouped: Box<[[[u8; 128]; NUM_BLOCKS]; SLOTS]>,
    psd: Box<[[[i16; BLOCK]; NUM_BLOCKS]; SLOTS]>,
    band_psd: Box<[[[i16; 50]; NUM_BLOCKS]; SLOTS]>,
    mask: Box<[[[i16; 50]; NUM_BLOCKS]; SLOTS]>,
    bap: Box<[[[u8; BLOCK]; NUM_BLOCKS]; SLOTS]>,
    qmant: Box<[[[i32; BLOCK]; NUM_BLOCKS]; SLOTS]>,
    /// The last 256 input samples of each channel, the first half of the next frame's first block.
    history: Vec<[f32; BLOCK]>,
    pending: Vec<Vec<f32>>,
    window: [f32; 2 * BLOCK],
    imdct: Imdct,
    frames_out: u64,
    input: u64,
    out: VecDeque<Packet>,
}

impl Ac3Encoder {
    /// An encoder for `channels` at `sample_rate` (32000, 44100 or 48000) and `bitrate` bits per
    /// second (0 for FFmpeg's default for the channel count), in the channel mode
    /// `mode_for_channels` gives that count.
    pub fn new(sample_rate: u32, channels: usize, bitrate: u32) -> Result<Self> {
        let Some((acmod, lfe)) = mode_for_channels(channels) else {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the AC-3 encoder has no channel mode for {channels} channels"))
                .hint("use 1, 2, 3, 4, 5 or 6 channels"));
        };
        Self::with_mode(sample_rate, acmod, lfe, bitrate)
    }

    /// An encoder in AC-3 channel mode `acmod` (1 to 7: mono, stereo, 3/0, 2/1, 3/1, 2/2, 3/2) with
    /// or without the LFE channel. The input order is FFmpeg's for the layout.
    pub fn with_mode(sample_rate: u32, acmod: usize, lfe: bool, bitrate: u32) -> Result<Self> {
        if !(1..8).contains(&acmod) {
            return Err(Error::invalid_arg(format!("AC-3 channel mode {acmod} is not written (modes 1 to 7 are)")));
        }
        let Some(sr_code) = SAMPLE_RATES.iter().position(|&r| r == sample_rate) else {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the AC-3 encoder writes 48, 44.1 and 32 kHz, not {sample_rate} Hz"))
                .hint("resample to 48000, 44100 or 32000 Hz"));
        };
        let fbw = CHANNELS[acmod];
        let channels = fbw + lfe as usize;
        let requested = if bitrate == 0 { default_bitrate(fbw) } else { bitrate };
        let mut best = 0;
        let mut best_diff = u64::MAX;
        for (i, &kb) in BITRATE_TAB.iter().enumerate() {
            let diff = (kb as i64 * 1000 - requested as i64).unsigned_abs();
            if diff < best_diff {
                best_diff = diff;
                best = i;
            }
            if diff == 0 {
                break;
            }
        }
        let bit_rate = BITRATE_TAB[best] * 1000;
        let frame_size_code = best << 1;
        let frame_size_min = 2 * FRAME_SIZE[frame_size_code][sr_code] as usize;
        let fs58 = |fs: usize| ((fs >> 2) + (fs >> 4)) << 1;
        let mut crc_inv = [0u32; 2];
        crc_inv[0] = pow_poly(CRC16_POLY >> 1, 8 * fs58(frame_size_min) as u32 - 16, CRC16_POLY);
        if sr_code == 1 {
            crc_inv[1] = pow_poly(CRC16_POLY >> 1, 8 * fs58(frame_size_min + 2) as u32 - 16, CRC16_POLY);
        }
        let bandwidth_code = BANDWIDTH_TAB[fbw - 1][sr_code][best] as usize;

        let mut blocks = [BlockState::default(); NUM_BLOCKS];
        let mut start_freq = [0usize; SLOTS];
        for b in blocks.iter_mut() {
            for ch in 1..=fbw {
                b.end_freq[ch] = bandwidth_code * 3 + 73;
            }
            if lfe {
                b.end_freq[channels] = 7;
            }
        }

        // channel coupling is on by default for two or more channels, from the coupling start band
        // the table gives for the bit rate; where the table says no, it is off
        let mut cpl_enabled = acmod >= 2;
        let mut cpl_start = 0i32;
        if cpl_enabled {
            cpl_start = COUPLING_START_TAB[acmod - 2][sr_code][best] as i32;
            if cpl_start < 0 {
                cpl_enabled = false;
            }
        }
        let mut cpl_end_freq = 0;
        let mut num_cpl_subbands = 0;
        let mut num_cpl_bands = 0;
        let mut cpl_band_sizes = [0usize; 18];
        if cpl_enabled {
            let cpl_end_band = bandwidth_code / 4 + 3;
            let cpl_start_band = cpl_start.clamp(0, (cpl_end_band as i32 - 1).min(15)) as usize;
            num_cpl_subbands = cpl_end_band - cpl_start_band;
            num_cpl_bands = 1;
            cpl_band_sizes[0] = 12;
            let mut idx = 0;
            for &merged in &CPL_BAND_STRUCT[cpl_start_band + 1..cpl_end_band] {
                if merged == 1 {
                    cpl_band_sizes[idx] += 12;
                } else {
                    num_cpl_bands += 1;
                    idx += 1;
                    cpl_band_sizes[idx] = 12;
                }
            }
            start_freq[CPL] = cpl_start_band * 12 + 37;
            cpl_end_freq = cpl_end_band * 12 + 37;
            for b in blocks.iter_mut() {
                b.end_freq[CPL] = cpl_end_freq;
            }
        }
        if lfe {
            start_freq[channels] = 0;
        }

        let alloc = AllocParams {
            sr_code,
            sr_shift: 0,
            // FFmpeg's defaults: slow decay code 2, fast decay code 1, slow gain code 1, dB per bit
            // code 3 and floor code 7
            slow_gain: SLOW_GAIN[1] as i32,
            slow_decay: SLOW_DECAY[2] as i32,
            fast_decay: FAST_DECAY[1] as i32,
            db_per_bit: DB_PER_BIT[3] as i32,
            floor: FLOOR[7] as i32,
            cpl_fast_leak: 0,
            cpl_slow_leak: 0,
        };
        let channel_map = CHANNEL_MAP[acmod][lfe as usize].to_vec();
        let mut enc = Ac3Encoder {
            sample_rate,
            sr_code,
            acmod,
            lfe_on: lfe,
            channels,
            fbw,
            channel_map,
            has_center: acmod & 1 != 0 && acmod != 1,
            has_surround: acmod & 4 != 0,
            bit_rate,
            frame_size_code,
            frame_size_min,
            frame_size: frame_size_min,
            bits_written: 0,
            samples_written: 0,
            bandwidth_code,
            start_freq,
            cpl_end_freq,
            cpl_enabled,
            cpl_on: false,
            phase_flags_in_use: false,
            phase_flags: [false; 18],
            num_cpl_subbands,
            num_cpl_bands,
            cpl_band_sizes,
            rematrixing_enabled: acmod == 2,
            alloc,
            fast_gain_code: 4,
            coarse_snr: 40,
            fine_snr: [0; SLOTS],
            frame_bits_fixed: 0,
            frame_bits: 0,
            exponent_bits: 0,
            crc_inv,
            blocks,
            exp_strategy: [[0; NUM_BLOCKS]; SLOTS],
            exp_ref_block: [[0; NUM_BLOCKS]; SLOTS],
            mdct: Box::new([[[0.0; BLOCK]; NUM_BLOCKS]; SLOTS]),
            fixed: Box::new([[[0; BLOCK]; NUM_BLOCKS]; SLOTS]),
            exp: Box::new([[[0; BLOCK]; NUM_BLOCKS]; SLOTS]),
            grouped: Box::new([[[0; 128]; NUM_BLOCKS]; SLOTS]),
            psd: Box::new([[[0; BLOCK]; NUM_BLOCKS]; SLOTS]),
            band_psd: Box::new([[[0; 50]; NUM_BLOCKS]; SLOTS]),
            mask: Box::new([[[0; 50]; NUM_BLOCKS]; SLOTS]),
            bap: Box::new([[[0; BLOCK]; NUM_BLOCKS]; SLOTS]),
            qmant: Box::new([[[0; BLOCK]; NUM_BLOCKS]; SLOTS]),
            history: vec![[0.0; BLOCK]; channels],
            pending: vec![Vec::new(); channels],
            window: full_window(),
            imdct: Imdct::new(BLOCK),
            frames_out: 0,
            input: 0,
            out: VecDeque::new(),
        };
        enc.frame_bits_fixed = enc.count_frame_bits_fixed();
        Ok(enc)
    }

    /// The bit rate in bit/s the encoder writes (the table's value nearest the one asked for).
    pub fn bit_rate(&self) -> u32 {
        self.bit_rate
    }

    /// The sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The `acmod` and LFE flag of the frames.
    pub fn mode(&self) -> (usize, bool) {
        (self.acmod, self.lfe_on)
    }

    /// The Matroska/MP4 fields the frames' `dac3` box needs: `(fscod, bit_rate_code, acmod, lfe)`.
    pub fn dac3_fields(&self) -> (u8, u8, u8, bool) {
        (self.sr_code as u8, (self.frame_size_code >> 1) as u8, self.acmod as u8, self.lfe_on)
    }

    fn push_frame(&mut self, pcm: &[Vec<f32>]) -> Result<()> {
        let data = self.encode_frame(pcm)?;
        self.out.push_back(Packet { stream: 0, pts: self.frames_out as i64 * FRAME_SAMPLES as i64, duration: FRAME_SAMPLES as i64, data });
        self.frames_out += 1;
        Ok(())
    }

    /// One frame from `pcm`, one vector of 1536 samples per AC-3 channel: the whole encoder.
    fn encode_frame(&mut self, pcm: &[Vec<f32>]) -> Result<Vec<u8>> {
        if self.sr_code == 1 {
            self.adjust_frame_size();
        }
        self.apply_mdct(pcm);
        self.cpl_on = self.cpl_enabled;
        self.compute_coupling_strategy();
        if self.cpl_on {
            self.apply_channel_coupling();
        }
        self.compute_rematrixing_strategy();
        self.scale_coefficients();
        self.apply_rematrixing();
        self.extract_exponents();
        self.compute_exp_strategy();
        self.encode_exponents();
        self.compute_bit_allocation()?;
        self.group_exponents();
        self.quantize_mantissas();
        Ok(self.output_frame())
    }

    /// At 44.1 kHz the frame size alternates, so the average bit rate is the one asked for.
    fn adjust_frame_size(&mut self) {
        let (br, sr) = (self.bit_rate as i64, self.sample_rate as i64);
        while self.bits_written >= br && self.samples_written >= sr {
            self.bits_written -= br;
            self.samples_written -= sr;
        }
        self.frame_size = self.frame_size_min + 2 * (self.bits_written * sr < self.samples_written * br) as usize;
        self.bits_written += self.frame_size as i64 * 8;
        self.samples_written += FRAME_SAMPLES as i64;
    }

    /// Windows each channel's blocks and transforms them into the coefficient slots.
    fn apply_mdct(&mut self, pcm: &[Vec<f32>]) {
        for ch in 0..self.channels {
            let mut prev = self.history[ch];
            for blk in 0..NUM_BLOCKS {
                let cur = &pcm[ch][blk * BLOCK..(blk + 1) * BLOCK];
                let mut x = [0f32; 2 * BLOCK];
                x[..BLOCK].copy_from_slice(&prev);
                x[BLOCK..].copy_from_slice(cur);
                self.mdct[ch + 1][blk] = self.mdct_block(&x);
                prev.copy_from_slice(cur);
            }
            self.history[ch] = prev;
        }
    }

    /// The MDCT of 512 samples into 256 coefficients: the window, the TDAC fold, and the DCT-IV.
    fn mdct_block(&mut self, x: &[f32; 2 * BLOCK]) -> [f32; BLOCK] {
        let w = &self.window;
        let mut u = [0f32; BLOCK];
        for i in 0..BLOCK / 2 {
            u[BLOCK / 2 + i] += x[i] * w[i];
        }
        for i in BLOCK / 2..3 * BLOCK / 2 {
            u[3 * BLOCK / 2 - 1 - i] -= x[i] * w[i];
        }
        for i in 3 * BLOCK / 2..2 * BLOCK {
            u[i - 3 * BLOCK / 2] -= x[i] * w[i];
        }
        let mut out = [0f32; BLOCK];
        self.imdct.dct4_into(&u, &mut out);
        // FFmpeg's scale for its float MDCT: -2 / AC3_WINDOW_SIZE
        for v in out.iter_mut() {
            *v *= -2.0 / (2 * BLOCK) as f32;
        }
        out
    }

    /// The coupling strategy of every block before the coupling analysis: coupling is in use for
    /// a block when at least two channels are in it.
    fn compute_coupling_strategy(&mut self) {
        for b in self.blocks.iter_mut() {
            for ch in 1..=self.fbw {
                b.channel_in_cpl[ch] = self.cpl_on;
            }
        }
        let mut got_cpl_snr = false;
        let mut num_cpl_blocks = 0;
        for blk in 0..NUM_BLOCKS {
            let n = (1..=self.fbw).filter(|&ch| self.blocks[blk].channel_in_cpl[ch]).count();
            self.blocks[blk].cpl_in_use = n > 1;
            num_cpl_blocks += self.blocks[blk].cpl_in_use as usize;
            if !self.blocks[blk].cpl_in_use {
                for ch in 1..=self.fbw {
                    self.blocks[blk].channel_in_cpl[ch] = false;
                }
            }
            self.blocks[blk].new_cpl_strategy = blk == 0 || (1..=self.fbw).any(|ch| self.blocks[blk].channel_in_cpl[ch] != self.blocks[blk - 1].channel_in_cpl[ch]);
            self.blocks[blk].new_cpl_leak = self.blocks[blk].new_cpl_strategy;
            let in_use = self.blocks[blk].cpl_in_use;
            if blk == 0 || (in_use && !got_cpl_snr) {
                self.blocks[blk].new_snr_offsets = true;
                if in_use {
                    got_cpl_snr = true;
                }
            } else {
                self.blocks[blk].new_snr_offsets = false;
            }
        }
        if num_cpl_blocks == 0 {
            self.cpl_on = false;
        }
        for blk in 0..NUM_BLOCKS {
            for ch in 1..=self.fbw {
                self.blocks[blk].end_freq[ch] = if self.blocks[blk].channel_in_cpl[ch] { self.start_freq[CPL] } else { self.bandwidth_code * 3 + 73 };
            }
        }
    }

    /// The coupling channel and the coupling coordinates (FFmpeg's `apply_channel_coupling`): the
    /// phase flags of stereo, the coupling channel's coefficients, and each channel's coordinate.
    fn apply_channel_coupling(&mut self) {
        let mut cpl_coords = [[[0f32; 16]; SLOTS]; NUM_BLOCKS];
        let mut energy = [[[0f32; 16]; SLOTS]; NUM_BLOCKS];
        self.phase_flags_in_use = false;
        // align the start to a 16-byte boundary and the length to 32: the start bin's residue is 1
        let num_cpl_coefs = (self.num_cpl_subbands * 12 + 1 + 31) & !31;
        let cpl_start = (self.start_freq[CPL] - 1 + num_cpl_coefs).min(256) - num_cpl_coefs;

        if self.acmod == 2 {
            let mut phase_flags = [true; 18];
            let mut cpl_blocks = 0;
            for blk in 0..NUM_BLOCKS {
                if !self.blocks[blk].cpl_in_use {
                    continue;
                }
                cpl_blocks += 1;
                let mut block_energy = butterfly(&self.mdct[1][blk][..self.start_freq[CPL]], &self.mdct[2][blk][..self.start_freq[CPL]]);
                let mut sum = [[0f32; 4]; 18];
                let mut i = self.start_freq[CPL];
                for bnd in 0..self.num_cpl_bands {
                    let size = self.cpl_band_sizes[bnd];
                    sum[bnd] = butterfly(&self.mdct[1][blk][i..i + size], &self.mdct[2][blk][i..i + size]);
                    energy[blk][1][bnd] = sum[bnd][0];
                    energy[blk][2][bnd] = sum[bnd][1];
                    block_energy[0] += sum[bnd][0];
                    block_energy[1] += sum[bnd][1];
                    i += size;
                }
                let max_energy = block_energy[0].max(block_energy[1]);
                for bnd in 0..self.num_cpl_bands {
                    let band_energy = sum[bnd][0].max(sum[bnd][1]);
                    let significant = band_energy > max_energy / 16384.0;
                    phase_flags[bnd] &= significant && sum[bnd][3] / 16.0 > sum[bnd][2];
                }
            }
            for bnd in 0..self.num_cpl_bands {
                let phase = phase_flags[bnd] && cpl_blocks > 0;
                self.phase_flags[bnd] = phase;
                self.phase_flags_in_use |= phase;
            }
        }

        // the coupling channel: the sum of the coupled channels, with the difference in the phase bands
        for blk in 0..NUM_BLOCKS {
            if !self.blocks[blk].cpl_in_use {
                continue;
            }
            for i in 0..num_cpl_coefs {
                self.mdct[CPL][blk][cpl_start + i] = 0.0;
            }
            for ch in 1..=self.fbw {
                if !self.blocks[blk].channel_in_cpl[ch] {
                    continue;
                }
                for i in 0..num_cpl_coefs {
                    self.mdct[CPL][blk][cpl_start + i] += self.mdct[ch][blk][cpl_start + i];
                }
            }
            if self.acmod == 2 {
                let mut i = self.start_freq[CPL];
                for bnd in 0..self.num_cpl_bands {
                    if self.phase_flags[bnd] {
                        for j in 0..self.cpl_band_sizes[bnd] {
                            self.mdct[CPL][blk][i + j] = self.mdct[1][blk][i + j] - self.mdct[2][blk][i + j];
                        }
                    }
                    i += self.cpl_band_sizes[bnd];
                }
            }
            for v in &mut self.mdct[CPL][blk][cpl_start..cpl_start + num_cpl_coefs] {
                *v = v.clamp(COEF_MIN, COEF_MAX);
            }
        }

        // the energy of each band in the coupling channel and in each channel
        let mut bnd = 0;
        let mut i = self.start_freq[CPL];
        while i < self.cpl_end_freq {
            let band_size = self.cpl_band_sizes[bnd];
            let last_ch = if self.acmod == 2 { CPL } else { self.fbw };
            for ch in CPL..=last_ch {
                for blk in 0..NUM_BLOCKS {
                    if !self.blocks[blk].cpl_in_use || (ch > CPL && !self.blocks[blk].channel_in_cpl[ch]) {
                        continue;
                    }
                    for j in 0..band_size {
                        let v = self.mdct[ch][blk][i + j];
                        energy[blk][ch][bnd] += v * v;
                    }
                }
            }
            i += band_size;
            bnd += 1;
        }

        for blk in 0..NUM_BLOCKS {
            if !self.blocks[blk].cpl_in_use {
                continue;
            }
            for ch in 1..=self.fbw {
                if !self.blocks[blk].channel_in_cpl[ch] {
                    continue;
                }
                for bnd in 0..self.num_cpl_bands {
                    cpl_coords[blk][ch][bnd] = calc_cpl_coord(energy[blk][ch][bnd], energy[blk][CPL][bnd]);
                }
            }
        }

        // which blocks send new coordinates
        for blk in 0..NUM_BLOCKS {
            self.blocks[blk].new_cpl_coords = [false; SLOTS];
            if !self.blocks[blk].cpl_in_use {
                continue;
            }
            if blk == 0 || !self.blocks[blk - 1].cpl_in_use {
                for ch in 1..=self.fbw {
                    self.blocks[blk].new_cpl_coords[ch] = true;
                }
            } else {
                for ch in 1..=self.fbw {
                    if !self.blocks[blk].channel_in_cpl[ch] {
                        continue;
                    }
                    if !self.blocks[blk - 1].channel_in_cpl[ch] {
                        self.blocks[blk].new_cpl_coords[ch] = true;
                    } else {
                        let mut coord_diff = 0f32;
                        for bnd in 0..self.num_cpl_bands {
                            coord_diff += (cpl_coords[blk - 1][ch][bnd] - cpl_coords[blk][ch][bnd]).abs();
                        }
                        coord_diff /= self.num_cpl_bands as f32;
                        if coord_diff > NEW_CPL_COORD_THRESHOLD {
                            self.blocks[blk].new_cpl_coords[ch] = true;
                        }
                    }
                }
            }
        }

        // the coordinates of a block that reuses the previous one are the energies summed over the reuse run
        for bnd in 0..self.num_cpl_bands {
            let mut blk = 0;
            while blk < NUM_BLOCKS {
                if !self.blocks[blk].cpl_in_use {
                    blk += 1;
                    continue;
                }
                let mut blk1 = blk + 1;
                for ch in 1..=self.fbw {
                    if !self.blocks[blk].channel_in_cpl[ch] {
                        continue;
                    }
                    let mut energy_cpl = energy[blk][CPL][bnd];
                    let mut energy_ch = energy[blk][ch][bnd];
                    blk1 = blk + 1;
                    while blk1 < NUM_BLOCKS && !self.blocks[blk1].new_cpl_coords[ch] {
                        if self.blocks[blk1].cpl_in_use {
                            energy_cpl += energy[blk1][CPL][bnd];
                            energy_ch += energy[blk1][ch][bnd];
                        }
                        blk1 += 1;
                    }
                    cpl_coords[blk][ch][bnd] = calc_cpl_coord(energy_ch, energy_cpl);
                }
                blk = blk1;
            }
        }

        // the exponents and mantissas of the coordinates
        for blk in 0..NUM_BLOCKS {
            if !self.blocks[blk].cpl_in_use {
                continue;
            }
            let mut fixed_cpl = [[0i32; 16]; SLOTS];
            for ch in 1..=self.fbw {
                for bnd in 0..16 {
                    fixed_cpl[ch][bnd] = lrint(cpl_coords[blk][ch][bnd] * 16777216.0);
                    self.blocks[blk].cpl_coord_exp[ch][bnd] = exp_of(fixed_cpl[ch][bnd]);
                }
            }
            for ch in 1..=self.fbw {
                if !self.blocks[blk].new_cpl_coords[ch] {
                    continue;
                }
                let exps = self.blocks[blk].cpl_coord_exp[ch];
                let mut min_exp = exps[0];
                let mut max_exp = exps[0];
                for &e in &exps[1..self.num_cpl_bands] {
                    min_exp = min_exp.min(e);
                    max_exp = max_exp.max(e);
                }
                let mut master_exp = ((max_exp as i32 - 15) + 2) / 3;
                master_exp = master_exp.max(0);
                while (min_exp as i32) < master_exp * 3 {
                    master_exp -= 1;
                }
                for bnd in 0..self.num_cpl_bands {
                    self.blocks[blk].cpl_coord_exp[ch][bnd] = (self.blocks[blk].cpl_coord_exp[ch][bnd] as i32 - master_exp * 3).clamp(0, 15) as u8;
                }
                self.blocks[blk].cpl_master_exp[ch] = master_exp as u8;
                for bnd in 0..self.num_cpl_bands {
                    let cpl_exp = self.blocks[blk].cpl_coord_exp[ch][bnd] as i32;
                    let mut cpl_mant = fixed_cpl[ch][bnd].wrapping_shl((5 + cpl_exp + master_exp * 3) as u32) >> 24;
                    if cpl_exp == 15 {
                        cpl_mant >>= 1;
                    } else {
                        cpl_mant -= 16;
                    }
                    self.blocks[blk].cpl_coord_mant[ch][bnd] = cpl_mant as u8;
                }
            }
        }
    }

    /// The stereo rematrixing flags of each block and band, and whether a block must send new ones.
    fn compute_rematrixing_strategy(&mut self) {
        if self.acmod != 2 {
            return;
        }
        for blk in 0..NUM_BLOCKS {
            self.blocks[blk].new_rem = blk == 0;
            self.blocks[blk].num_rem_bands = 4;
            if self.blocks[blk].cpl_in_use {
                self.blocks[blk].num_rem_bands -= (self.start_freq[CPL] <= 61) as usize;
                self.blocks[blk].num_rem_bands -= (self.start_freq[CPL] == 37) as usize;
                if blk > 0 && self.blocks[blk].num_rem_bands != self.blocks[blk - 1].num_rem_bands {
                    self.blocks[blk].new_rem = true;
                }
            }
            if !self.rematrixing_enabled {
                continue;
            }
            let nb_coefs = self.blocks[blk].end_freq[1].min(self.blocks[blk].end_freq[2]);
            for bnd in 0..self.blocks[blk].num_rem_bands {
                let start = REMATRIX_BAND[bnd] as usize;
                let end = nb_coefs.min(REMATRIX_BAND[bnd + 1] as usize);
                let sum = butterfly(&self.mdct[1][blk][start..end], &self.mdct[2][blk][start..end]);
                let flag = sum[2].min(sum[3]) < sum[0].min(sum[1]);
                self.blocks[blk].rem_flags[bnd] = flag;
                if blk > 0 && flag != self.blocks[blk - 1].rem_flags[bnd] {
                    self.blocks[blk].new_rem = true;
                }
            }
        }
    }

    /// The coefficients as 24-bit fixed point, for the exponents and mantissas.
    fn scale_coefficients(&mut self) {
        let first = if self.cpl_on { CPL } else { 1 };
        for slot in first..=self.channels {
            for blk in 0..NUM_BLOCKS {
                for i in 0..BLOCK {
                    self.fixed[slot][blk][i] = lrint(self.mdct[slot][blk][i] * 16777216.0);
                }
            }
        }
    }

    /// Rematrixing on the fixed-point coefficients: each flagged band becomes the sum and difference
    /// of the two channels, halved. The flags of a block that sends none are those of the last one that did.
    fn apply_rematrixing(&mut self) {
        if !self.rematrixing_enabled {
            return;
        }
        let mut flags = [false; 4];
        for blk in 0..NUM_BLOCKS {
            if self.blocks[blk].new_rem {
                flags = self.blocks[blk].rem_flags;
            }
            let nb_coefs = self.blocks[blk].end_freq[1].min(self.blocks[blk].end_freq[2]);
            for bnd in 0..self.blocks[blk].num_rem_bands {
                if !flags[bnd] {
                    continue;
                }
                let start = REMATRIX_BAND[bnd] as usize;
                let end = nb_coefs.min(REMATRIX_BAND[bnd + 1] as usize);
                for i in start..end {
                    let lt = self.fixed[1][blk][i];
                    let rt = self.fixed[2][blk][i];
                    self.fixed[1][blk][i] = (lt.wrapping_add(rt)) >> 1;
                    self.fixed[2][blk][i] = (lt.wrapping_sub(rt)) >> 1;
                }
            }
        }
    }

    /// The exponents of every coefficient: the power of two of each fixed-point coefficient's leading bit.
    fn extract_exponents(&mut self) {
        let first = if self.cpl_on { CPL } else { 1 };
        for slot in first..=self.channels {
            for blk in 0..NUM_BLOCKS {
                for i in 0..BLOCK {
                    self.exp[slot][blk][i] = exp_of(self.fixed[slot][blk][i]);
                }
            }
        }
    }

    /// The exponent strategy of each channel and block: a block whose exponents differ from the
    /// previous one's by more than the threshold gets new exponents; runs of reused blocks take the
    /// coarsest grouping that covers them.
    fn compute_exp_strategy(&mut self) {
        let first = if self.cpl_on { CPL } else { 1 };
        for ch in first..=self.fbw {
            self.exp_strategy[ch][0] = EXP_NEW;
            for blk in 1..NUM_BLOCKS {
                if ch == CPL {
                    if !self.blocks[blk - 1].cpl_in_use {
                        self.exp_strategy[ch][blk] = EXP_NEW;
                        continue;
                    } else if !self.blocks[blk].cpl_in_use {
                        self.exp_strategy[ch][blk] = EXP_REUSE;
                        continue;
                    }
                } else if self.blocks[blk].channel_in_cpl[ch] != self.blocks[blk - 1].channel_in_cpl[ch] {
                    self.exp_strategy[ch][blk] = EXP_NEW;
                    continue;
                }
                let exp_diff: i32 = self.exp[ch][blk].iter().zip(self.exp[ch][blk - 1].iter()).map(|(&a, &b)| (a as i32 - b as i32).abs()).sum();
                self.exp_strategy[ch][blk] = EXP_REUSE;
                if ch == CPL {
                    let width = (self.blocks[blk].end_freq[ch] - self.start_freq[ch]) as i32;
                    if exp_diff > EXP_DIFF_THRESHOLD * width / BLOCK as i32 {
                        self.exp_strategy[ch][blk] = EXP_NEW;
                    }
                } else if exp_diff > EXP_DIFF_THRESHOLD {
                    self.exp_strategy[ch][blk] = EXP_NEW;
                }
            }
            let mut blk = 0;
            while blk < NUM_BLOCKS {
                let mut blk1 = blk + 1;
                while blk1 < NUM_BLOCKS && self.exp_strategy[ch][blk1] == EXP_REUSE {
                    blk1 += 1;
                }
                self.exp_strategy[ch][blk] = EXP_STRATEGY_RUN[blk1 - blk - 1];
                blk = blk1;
            }
        }
        if self.lfe_on {
            let ch = self.channels;
            self.exp_strategy[ch][0] = EXP_D15;
            for blk in 1..NUM_BLOCKS {
                self.exp_strategy[ch][blk] = EXP_REUSE;
            }
        }
    }

    /// The exponents the decoder will see: each run's minimum, grouped by the strategy and with
    /// neighbouring exponents at most 2 apart.
    fn encode_exponents(&mut self) {
        let first = if self.cpl_on { CPL } else { 1 };
        for ch in first..=self.channels {
            let cpl = ch == CPL;
            let mut blk = 0;
            while blk < NUM_BLOCKS {
                if cpl && !self.blocks[blk].cpl_in_use {
                    blk += 1;
                    continue;
                }
                let nb_coefs = self.blocks[blk].end_freq[ch] - self.start_freq[ch];
                let mut blk1 = blk + 1;
                self.exp_ref_block[ch][blk] = blk;
                while blk1 < NUM_BLOCKS && self.exp_strategy[ch][blk1] == EXP_REUSE {
                    self.exp_ref_block[ch][blk1] = blk;
                    blk1 += 1;
                }
                let num_reuse = blk1 - blk - 1;
                for i in 0..BLOCK {
                    let mut m = self.exp[ch][blk][i];
                    for b in 1..=num_reuse {
                        m = m.min(self.exp[ch][blk + b][i]);
                    }
                    self.exp[ch][blk][i] = m;
                }
                encode_exp_blk_ch(&mut self.exp[ch][blk], self.start_freq[ch], nb_coefs, self.exp_strategy[ch][blk], cpl);
                blk = blk1;
            }
        }
    }

    /// The bits in the frame apart from the exponents and mantissas, for the parameters that change
    /// during encoding (`count_frame_bits`).
    fn count_frame_bits(&mut self) {
        let mut bits = 0usize;
        // audio production info and the alternate bit stream syntax are not written by default
        for blk in 0..NUM_BLOCKS {
            let b = self.blocks[blk];
            if b.new_cpl_strategy {
                bits += 1;
                if b.cpl_in_use {
                    bits += self.fbw;
                    if self.acmod == 2 {
                        bits += 1;
                    }
                    bits += 4 + 4;
                    bits += self.num_cpl_subbands - 1;
                }
            }
            if b.cpl_in_use {
                let mut cpl_coords_exist = false;
                for ch in 1..=self.fbw {
                    if b.channel_in_cpl[ch] {
                        bits += 1;
                        if b.new_cpl_coords[ch] {
                            cpl_coords_exist = true;
                            bits += 2;
                            bits += (4 + 4) * self.num_cpl_bands;
                        }
                    }
                }
                if self.acmod == 2 && self.phase_flags_in_use && cpl_coords_exist {
                    bits += self.num_cpl_bands;
                }
            }
            if self.acmod == 2 {
                bits += 1;
                if b.new_rem {
                    bits += b.num_rem_bands;
                }
            }
            for ch in 1..=self.fbw {
                if self.exp_strategy[ch][blk] != EXP_REUSE {
                    if !b.channel_in_cpl[ch] {
                        bits += 6;
                    }
                    bits += 2;
                }
            }
            if b.cpl_in_use {
                bits += 2;
            }
            if b.new_snr_offsets {
                bits += 6 + (self.channels + b.cpl_in_use as usize) * (4 + 3);
            }
            if b.cpl_in_use {
                bits += 1;
                if b.new_cpl_leak {
                    bits += 3 + 3;
                }
            }
        }
        self.frame_bits = self.frame_bits_fixed + bits;
    }

    /// The bits the frame's header and fixed block fields take (`count_frame_bits_fixed`).
    fn count_frame_bits_fixed(&self) -> usize {
        let mut bits = 16 + 49 + FRAME_BITS_INC[self.acmod];
        for _ in 0..NUM_BLOCKS {
            bits += self.fbw; // block switch flags
            bits += self.fbw; // dither flags
            bits += 1; // dynamic range
            bits += 1; // coupling strategy exists
            bits += 2 * self.fbw; // exponent strategy
            if self.lfe_on {
                bits += 1;
            }
            bits += 1; // bit allocation parameters
            bits += 1; // snr offset
            bits += 1; // delta bit allocation
            bits += 1; // skipped data
        }
        bits += 2 + 2 + 2 + 2 + 3; // bit allocation parameters of the first block
        bits += 1; // auxiliary data
        bits += 1 + 16; // CRC
        bits
    }

    /// The exponent bits: each exponent set's 4-bit DC plus 7 bits per group of three.
    fn count_exponent_bits(&self) -> usize {
        let mut bits = 0;
        for blk in 0..NUM_BLOCKS {
            let first = if self.blocks[blk].cpl_in_use { CPL } else { 1 };
            for ch in first..=self.channels {
                let strategy = self.exp_strategy[ch][blk];
                if strategy == EXP_REUSE {
                    continue;
                }
                let nb = exp_groups(ch == CPL, strategy, self.blocks[blk].end_freq[ch] - self.start_freq[ch]);
                bits += 4 + nb * 7;
            }
        }
        bits
    }

    /// Bits used by the PSD and the masking curve, for the bit allocation.
    fn bit_alloc_masking(&mut self) {
        for blk in 0..NUM_BLOCKS {
            let first = if self.blocks[blk].cpl_in_use { CPL } else { 1 };
            for ch in first..=self.channels {
                if self.exp_strategy[ch][blk] == EXP_REUSE {
                    continue;
                }
                let start = self.start_freq[ch];
                let end = self.blocks[blk].end_freq[ch];
                let exps: [i8; BLOCK] = self.exp[ch][blk].map(|e| e as i8);
                alloc::psd(&exps, start, end, &mut self.psd[ch][blk], &mut self.band_psd[ch][blk]);
                let dba = Dba { mode: 2, nsegs: 0, offsets: &NO_DBA, lengths: &NO_DBA, values: &NO_DBA };
                let is_lfe = self.lfe_on && ch == self.channels;
                alloc::mask(&self.alloc, &self.band_psd[ch][blk], start, end, FAST_GAIN[self.fast_gain_code] as i32, is_lfe, &dba, &mut self.mask[ch][blk]);
            }
        }
    }

    /// The bap of every coefficient at an snr offset (the offset in the bit stream's units, 0 to 1023).
    /// Returns the mantissa bits that takes.
    fn bit_alloc(&mut self, snr: i32) -> usize {
        let snr = (snr - 240) * 4;
        for blk in 0..NUM_BLOCKS {
            let first = if self.blocks[blk].cpl_in_use { CPL } else { 1 };
            for ch in first..=self.channels {
                if self.exp_strategy[ch][blk] != EXP_REUSE {
                    let end = self.blocks[blk].end_freq[ch];
                    let start = self.start_freq[ch];
                    alloc::bap(&self.mask[ch][blk], &self.psd[ch][blk], start, end, snr, self.alloc.floor, &BAP_TAB, &mut self.bap[ch][blk]);
                }
            }
        }
        self.count_mantissa_bits()
    }

    /// The mantissa bits at the current bap (`count_mantissa_bits` with the padding of each group).
    fn count_mantissa_bits(&self) -> usize {
        let mut mant = [[0usize; 16]; NUM_BLOCKS];
        for m in mant.iter_mut() {
            m[1] = 2;
            m[2] = 2;
            m[4] = 1;
        }
        let max_end = self.bandwidth_code * 3 + 73;
        let first = if self.cpl_enabled { CPL } else { 1 };
        for ch in first..=self.channels {
            for blk in 0..NUM_BLOCKS {
                if ch == CPL && !self.blocks[blk].cpl_in_use {
                    continue;
                }
                let start = self.start_freq[ch];
                let len = max_end.min(self.blocks[blk].end_freq[ch]) - start;
                for &b in &self.bap[ch][self.exp_ref_block[ch][blk]][start..start + len] {
                    mant[blk][b as usize] += 1;
                }
            }
        }
        let mut bits = 0;
        for m in &mant {
            bits += (m[1] / 3) * 5;
            bits += ((m[2] / 3) + (m[4] >> 1)) * 7;
            bits += m[3] * 3;
            for b in 5..16 {
                bits += m[b] * BAP_BITS[b];
            }
        }
        bits
    }

    /// The largest snr offset whose frame fits the bits left (`cbr_bit_allocation`).
    fn cbr_bit_allocation(&mut self) -> Result<()> {
        let bits_left = 8 * self.frame_size as i64 - (self.frame_bits + self.exponent_bits) as i64;
        if bits_left < 0 {
            return Err(bit_alloc_error());
        }
        let bits_left = bits_left as usize;
        let mut snr = self.coarse_snr << 4;
        if (snr | self.fine_snr[1]) == 1023 && self.bit_alloc(1023) <= bits_left {
            return Ok(());
        }
        while snr >= 0 && self.bit_alloc(snr) > bits_left {
            snr -= 64;
        }
        if snr < 0 {
            return Err(bit_alloc_error());
        }
        let mut incr = 64;
        while incr > 0 {
            while snr + incr <= 1023 && self.bit_alloc(snr + incr) <= bits_left {
                snr += incr;
            }
            incr >>= 2;
        }
        self.bit_alloc(snr);
        self.coarse_snr = snr >> 4;
        let first = if self.cpl_on { CPL } else { 1 };
        for ch in first..=self.channels {
            self.fine_snr[ch] = snr & 0xF;
        }
        Ok(())
    }

    fn compute_bit_allocation(&mut self) -> Result<()> {
        self.count_frame_bits();
        self.exponent_bits = self.count_exponent_bits();
        self.bit_alloc_masking();
        self.cbr_bit_allocation()
    }

    /// The exponents as the decoder reads them: each group's three deltas in one 7-bit code.
    fn group_exponents(&mut self) {
        for blk in 0..NUM_BLOCKS {
            let first = if self.blocks[blk].cpl_in_use { CPL } else { 1 };
            for ch in first..=self.channels {
                let strategy = self.exp_strategy[ch][blk];
                if strategy == EXP_REUSE {
                    continue;
                }
                let cpl = (ch == CPL) as usize;
                let group_size = strategy as usize + (strategy == EXP_D45) as usize;
                let nb = exp_groups(ch == CPL, strategy, self.blocks[blk].end_freq[ch] - self.start_freq[ch]);
                let e = &self.exp[ch][blk];
                let mut p = self.start_freq[ch] - cpl;
                let mut exp1 = e[p] as i32;
                p += 1;
                self.grouped[ch][blk][0] = exp1 as u8;
                for i in 1..=nb {
                    let mut exp0 = exp1;
                    exp1 = e[p] as i32;
                    p += group_size;
                    let delta0 = exp1 - exp0 + 2;
                    exp0 = exp1;
                    exp1 = e[p] as i32;
                    p += group_size;
                    let delta1 = exp1 - exp0 + 2;
                    exp0 = exp1;
                    exp1 = e[p] as i32;
                    p += group_size;
                    let delta2 = exp1 - exp0 + 2;
                    self.grouped[ch][blk][i] = (((delta0 * 5 + delta1) * 5) + delta2) as u8;
                }
            }
        }
    }

    /// The mantissas of every coefficient: each value is quantised to its bap's levels, and the
    /// bap 1, 2 and 4 values are grouped three or two to a code, as the bit stream has them.
    fn quantize_mantissas(&mut self) {
        for blk in 0..NUM_BLOCKS {
            let mut m = Mant::default();
            let mut got_cpl = !self.blocks[blk].cpl_in_use;
            let mut ch = 1;
            while ch <= self.channels {
                let mut q_ch = ch;
                let mut ch0 = 0;
                if !got_cpl && ch > 1 && self.blocks[blk].channel_in_cpl[ch - 1] {
                    ch0 = ch - 1;
                    q_ch = CPL;
                    got_cpl = true;
                }
                let eb = self.exp_ref_block[q_ch][blk];
                let (start, end) = (self.start_freq[q_ch], self.blocks[blk].end_freq[q_ch]);
                for i in start..end {
                    self.quantize_one(&mut m, q_ch, blk, eb, i);
                }
                if q_ch == CPL {
                    ch = ch0;
                }
                ch += 1;
            }
        }
    }

    fn quantize_one(&mut self, m: &mut Mant, ch: usize, blk: usize, eb: usize, i: usize) {
        let c = self.fixed[ch][blk][i];
        let e = self.exp[ch][eb][i] as i32;
        let mut v = self.bap[ch][eb][i] as i32;
        match v {
            0 => {}
            1 => {
                v = sym_quant(c, e, 3);
                match m.c1 {
                    0 => {
                        m.p1 = (ch, i);
                        v *= 9;
                        m.c1 = 1;
                    }
                    1 => {
                        self.qmant[m.p1.0][blk][m.p1.1] += 3 * v;
                        m.c1 = 2;
                        v = 128;
                    }
                    _ => {
                        self.qmant[m.p1.0][blk][m.p1.1] += v;
                        m.c1 = 0;
                        v = 128;
                    }
                }
            }
            2 => {
                v = sym_quant(c, e, 5);
                match m.c2 {
                    0 => {
                        m.p2 = (ch, i);
                        v *= 25;
                        m.c2 = 1;
                    }
                    1 => {
                        self.qmant[m.p2.0][blk][m.p2.1] += 5 * v;
                        m.c2 = 2;
                        v = 128;
                    }
                    _ => {
                        self.qmant[m.p2.0][blk][m.p2.1] += v;
                        m.c2 = 0;
                        v = 128;
                    }
                }
            }
            3 => v = sym_quant(c, e, 7),
            4 => {
                v = sym_quant(c, e, 11);
                match m.c4 {
                    0 => {
                        m.p4 = (ch, i);
                        v *= 11;
                        m.c4 = 1;
                    }
                    _ => {
                        self.qmant[m.p4.0][blk][m.p4.1] += v;
                        m.c4 = 0;
                        v = 128;
                    }
                }
            }
            5 => v = sym_quant(c, e, 15),
            14 => v = asym_quant(c, e, 14),
            15 => v = asym_quant(c, e, 16),
            _ => v = asym_quant(c, e, v - 1),
        }
        self.qmant[ch][blk][i] = v;
    }

    /// The frame's bytes: the header, the six blocks, then the padding and the two CRCs.
    fn output_frame(&self) -> Vec<u8> {
        let fs = self.frame_size;
        let mut frame = vec![0u8; fs];
        let mut w = BitWriter { buf: &mut frame, pos: 0 };
        self.write_header(&mut w);
        for blk in 0..NUM_BLOCKS {
            self.write_block(&mut w, blk);
        }
        let used = w.pos.div_ceil(8);
        let fs58 = ((fs >> 2) + (fs >> 4)) << 1;
        debug_assert!(fs - used >= 2, "the frame has room for its CRCs");
        // CRC 1 covers the first 5/8 of the frame, and is fixed up so that CRC 2 is independent of it
        let crc1 = crc16(&frame[4..fs58]);
        let crc_inv = self.crc_inv[(fs > self.frame_size_min) as usize];
        let crc1 = mul_poly(crc_inv, crc1, CRC16_POLY);
        frame[2..4].copy_from_slice(&(crc1 as u16).to_be_bytes());
        let mut crc2 = crc16(&frame[fs58..fs - 2]);
        if crc2 == 0x0B77 {
            // the CRC must not look like a sync word: flip a bit the CRC polynomial does not see
            frame[fs - 3] ^= 0x1;
            crc2 ^= 0x8005;
        }
        frame[fs - 2..].copy_from_slice(&(crc2 as u16).to_be_bytes());
        frame
    }

    fn write_header(&self, w: &mut BitWriter) {
        w.put(16, 0x0B77);
        w.put(16, 0); // CRC 1, filled in later
        w.put(2, self.sr_code as u32);
        w.put(6, (self.frame_size_code + (self.frame_size - self.frame_size_min) / 2) as u32);
        w.put(5, 8); // bsid
        w.put(3, 0); // bsmod: main audio service
        w.put(3, self.acmod as u32);
        if self.has_center {
            w.put(2, 1); // center mix level -4.5 dB
        }
        if self.has_surround {
            w.put(2, 1); // surround mix level -6 dB
        }
        if self.acmod == 2 {
            w.put(2, 0); // dolby surround mode not indicated
        }
        w.put(1, self.lfe_on as u32);
        w.put(5, 31); // dialogue level: -31 dB
        w.put(1, 0); // no compression word
        w.put(1, 0); // no language code
        w.put(1, 0); // no audio production info
        w.put(1, 0); // copyright off
        w.put(1, 1); // original
        w.put(1, 0); // no time code 1
        w.put(1, 0); // no time code 2
        w.put(1, 0); // no additional bit stream info
    }

    fn write_block(&self, w: &mut BitWriter, blk: usize) {
        let b = &self.blocks[blk];
        for _ in 0..self.fbw {
            w.put(1, 0); // block switch
        }
        for _ in 0..self.fbw {
            w.put(1, 1); // dither
        }
        w.put(1, 0); // dynamic range
        w.put(1, b.new_cpl_strategy as u32);
        if b.new_cpl_strategy {
            w.put(1, b.cpl_in_use as u32);
            if b.cpl_in_use {
                for ch in 1..=self.fbw {
                    w.put(1, b.channel_in_cpl[ch] as u32);
                }
                if self.acmod == 2 {
                    w.put(1, self.phase_flags_in_use as u32);
                }
                let start_sub = (self.start_freq[CPL] - 37) / 12;
                let end_sub = (self.cpl_end_freq - 37) / 12;
                w.put(4, start_sub as u32);
                w.put(4, (end_sub - 3) as u32);
                for bnd in start_sub + 1..end_sub {
                    w.put(1, CPL_BAND_STRUCT[bnd] as u32);
                }
            }
        }
        if b.cpl_in_use {
            let mut cpl_coords_exist = false;
            for ch in 1..=self.fbw {
                if b.channel_in_cpl[ch] {
                    w.put(1, b.new_cpl_coords[ch] as u32);
                    if b.new_cpl_coords[ch] {
                        cpl_coords_exist = true;
                        w.put(2, b.cpl_master_exp[ch] as u32);
                        for bnd in 0..self.num_cpl_bands {
                            w.put(4, b.cpl_coord_exp[ch][bnd] as u32);
                            w.put(4, b.cpl_coord_mant[ch][bnd] as u32);
                        }
                    }
                }
            }
            if self.acmod == 2 && self.phase_flags_in_use && cpl_coords_exist {
                for bnd in 0..self.num_cpl_bands {
                    w.put(1, self.phase_flags[bnd] as u32);
                }
            }
        }
        if self.acmod == 2 {
            w.put(1, b.new_rem as u32);
            if b.new_rem {
                for bnd in 0..b.num_rem_bands {
                    w.put(1, b.rem_flags[bnd] as u32);
                }
            }
        }
        let first = if b.cpl_in_use { CPL } else { 1 };
        for ch in first..=self.fbw {
            w.put(2, self.exp_strategy[ch][blk] as u32);
        }
        if self.lfe_on {
            w.put(1, self.exp_strategy[self.channels][blk] as u32);
        }
        for ch in 1..=self.fbw {
            if self.exp_strategy[ch][blk] != EXP_REUSE && !b.channel_in_cpl[ch] {
                w.put(6, self.bandwidth_code as u32);
            }
        }
        for ch in first..=self.channels {
            if self.exp_strategy[ch][blk] == EXP_REUSE {
                continue;
            }
            let cpl = (ch == CPL) as usize;
            w.put(4, (self.grouped[ch][blk][0] >> cpl) as u32);
            let nb = exp_groups(ch == CPL, self.exp_strategy[ch][blk], b.end_freq[ch] - self.start_freq[ch]);
            for i in 1..=nb {
                w.put(7, self.grouped[ch][blk][i] as u32);
            }
            if !(self.lfe_on && ch == self.channels) && cpl == 0 {
                w.put(2, 0); // gain range
            }
        }
        let baie = blk == 0;
        w.put(1, baie as u32);
        if baie {
            w.put(2, 2); // slow decay
            w.put(2, 1); // fast decay
            w.put(2, 1); // slow gain
            w.put(2, 3); // dB per bit
            w.put(3, 7); // floor
        }
        w.put(1, b.new_snr_offsets as u32);
        if b.new_snr_offsets {
            w.put(6, self.coarse_snr as u32);
            for ch in first..=self.channels {
                w.put(4, self.fine_snr[ch] as u32);
                w.put(3, self.fast_gain_code as u32);
            }
        }
        if b.cpl_in_use {
            w.put(1, b.new_cpl_leak as u32);
            if b.new_cpl_leak {
                w.put(3, 0);
                w.put(3, 0);
            }
        }
        w.put(1, 0); // no delta bit allocation
        w.put(1, 0); // no data to skip
        self.write_mantissas(w, blk);
    }

    fn write_mantissas(&self, w: &mut BitWriter, blk: usize) {
        let mut got_cpl = !self.blocks[blk].cpl_in_use;
        let mut ch = 1;
        while ch <= self.channels {
            let mut q_ch = ch;
            let mut ch0 = 0;
            if !got_cpl && ch > 1 && self.blocks[blk].channel_in_cpl[ch - 1] {
                ch0 = ch - 1;
                q_ch = CPL;
                got_cpl = true;
            }
            let eb = self.exp_ref_block[q_ch][blk];
            for i in self.start_freq[q_ch]..self.blocks[blk].end_freq[q_ch] {
                let q = self.qmant[q_ch][blk][i];
                match self.bap[q_ch][eb][i] {
                    0 => {}
                    1 => {
                        if q != 128 {
                            w.put(5, q as u32);
                        }
                    }
                    2 | 4 => {
                        if q != 128 {
                            w.put(7, q as u32);
                        }
                    }
                    b => w.put_signed(BAP_BITS[b as usize] as u32, q),
                }
            }
            if q_ch == CPL {
                ch = ch0;
            }
            ch += 1;
        }
    }
}

/// The bit allocation's failure, as FFmpeg reports it.
fn bit_alloc_error() -> Error {
    Error::invalid_input("AC-3: bit allocation failed; the bit rate is too low for this input").hint("use a higher bit rate")
}

/// The 512-point window of the MDCT: the KBD window's rising half, mirrored for the falling half.
fn full_window() -> [f32; 2 * BLOCK] {
    let half = kbd_window(5.0, BLOCK);
    let mut w = [0f32; 2 * BLOCK];
    for i in 0..BLOCK {
        w[i] = half[i];
        w[2 * BLOCK - 1 - i] = half[i];
    }
    w
}

/// Sums of squares of `a`, `b`, `a + b` and `a - b`, in FFmpeg's float order.
fn butterfly(a: &[f32], b: &[f32]) -> [f32; 4] {
    let mut sum = [0f32; 4];
    for (&lt, &rt) in a.iter().zip(b) {
        let md = lt + rt;
        let sd = lt - rt;
        sum[0] += lt * lt;
        sum[1] += rt * rt;
        sum[2] += md * md;
        sum[3] += sd * sd;
    }
    sum
}

/// A coupling coordinate: the square root of the channel's energy over the coupling channel's, times 1/8.
fn calc_cpl_coord(energy_ch: f32, energy_cpl: f32) -> f32 {
    let mut coord = 0.125f32;
    if energy_cpl > 0.0 {
        coord *= (energy_ch / energy_cpl).sqrt();
    }
    coord.min(COEF_MAX)
}

/// The exponent of a fixed-point coefficient: the shift that brings its leading bit to bit 23.
fn exp_of(v: i32) -> u8 {
    if v == 0 {
        return 24;
    }
    let log2 = 31 - v.unsigned_abs().leading_zeros() as i32;
    (23 - log2).clamp(0, 24) as u8
}

/// The exponent groups of a channel's `nb_coefs` bins (`exponent_group_tab`): groups of 3, 6 or 12
/// bins for the D15, D25 and D45 strategies, counted from the second bin for the non-coupling channels.
fn exp_groups(cpl: bool, strategy: u8, nb_coefs: usize) -> usize {
    let g = 3usize << (strategy as usize - 1);
    if cpl {
        nb_coefs / g
    } else {
        (nb_coefs + g - 4) / g
    }
}

/// FFmpeg's `encode_exponents_blk_ch`: the exponents of one block and channel, from the bin `base`,
/// made into the grouped form the decoder reads. The coupling channel's exponents start one bin
/// below `base`, which holds its DC exponent.
fn encode_exp_blk_ch(a: &mut [u8; BLOCK], base: usize, nb_exps: usize, strategy: u8, cpl: bool) {
    let c = cpl as isize;
    let nb3 = (exp_groups(cpl, strategy, nb_exps) * 3) as isize;
    let at = |j: isize| (base as isize + j) as usize;
    match strategy {
        EXP_D25 => {
            let mut k = 1 - c;
            for i in 1..=nb3 {
                let m = a[at(k)].min(a[at(k + 1)]);
                a[at(i - c)] = m;
                k += 2;
            }
        }
        EXP_D45 => {
            let mut k = 1 - c;
            for i in 1..=nb3 {
                let m = a[at(k)].min(a[at(k + 1)]).min(a[at(k + 2)]).min(a[at(k + 3)]);
                a[at(i - c)] = m;
                k += 4;
            }
        }
        _ => {}
    }
    if !cpl && a[at(0)] > 15 {
        a[at(0)] = 15;
    }
    for i in 1..=nb3 {
        a[at(i)] = a[at(i)].min(a[at(i - 1)] + 2);
    }
    for i in (0..nb3).rev() {
        a[at(i)] = a[at(i)].min(a[at(i + 1)] + 2);
    }
    if cpl {
        a[at(-1)] = a[at(0)] & !1;
    }
    match strategy {
        EXP_D25 => {
            let mut k = nb3 * 2 - c;
            for i in (1..=nb3).rev() {
                let e = a[at(i - c)];
                a[at(k)] = e;
                k -= 1;
                a[at(k)] = e;
                k -= 1;
            }
        }
        EXP_D45 => {
            let mut k = nb3 * 4 - c;
            for i in (1..=nb3).rev() {
                let e = a[at(i - c)];
                a[at(k)] = e;
                a[at(k - 1)] = e;
                a[at(k - 2)] = e;
                a[at(k - 3)] = e;
                k -= 4;
            }
        }
        _ => {}
    }
}

/// Round to the nearest integer, ties to even, as `lrintf` does.
fn lrint(x: f32) -> i32 {
    x.round_ties_even() as i32
}

/// Symmetric quantisation to `levels` levels (FFmpeg's `sym_quant`).
fn sym_quant(c: i32, e: i32, levels: i32) -> i32 {
    (((levels as i64 * c as i64) >> (24 - e)) as i32 + levels) >> 1
}

/// Asymmetric quantisation to `qbits` bits (FFmpeg's `asym_quant`).
fn asym_quant(c: i32, e: i32, qbits: i32) -> i32 {
    let c = ((((c as i64) * (1i64 << e)) >> (24 - qbits)) as i32 + 1) >> 1;
    let m = 1 << (qbits - 1);
    c.min(m - 1)
}

/// The CRC-16 of FFmpeg's `AV_CRC_16_ANSI` (polynomial 0x8005, most significant bit first, from 0).
fn crc16(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        crc ^= (b as u32) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { ((crc << 1) ^ 0x8005) & 0xFFFF } else { (crc << 1) & 0xFFFF };
        }
    }
    crc
}

/// Multiplication of two polynomials modulo `poly` (FFmpeg's `mul_poly`).
fn mul_poly(mut a: u32, mut b: u32, poly: u32) -> u32 {
    let mut c = 0;
    while a != 0 {
        if a & 1 != 0 {
            c ^= b;
        }
        a >>= 1;
        b <<= 1;
        if b & (1 << 16) != 0 {
            b ^= poly;
        }
    }
    c
}

/// `a` to the power `n` modulo `poly` (FFmpeg's `pow_poly`).
fn pow_poly(mut a: u32, mut n: u32, poly: u32) -> u32 {
    let mut r = 1;
    while n != 0 {
        if n & 1 != 0 {
            r = mul_poly(r, a, poly);
        }
        a = mul_poly(a, a, poly);
        n >>= 1;
    }
    r
}

/// Writes bits, most significant first, into a zeroed buffer.
struct BitWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl BitWriter<'_> {
    fn put(&mut self, n: u32, v: u32) {
        for k in (0..n).rev() {
            if (v >> k) & 1 != 0 {
                self.buf[self.pos >> 3] |= 0x80 >> (self.pos & 7);
            }
            self.pos += 1;
        }
    }

    /// A two's complement value of `n` bits.
    fn put_signed(&mut self, n: u32, v: i32) {
        self.put(n, (v as u32) & ((1u32 << n) - 1));
    }
}

impl Encoder for Ac3Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid_input("the AC-3 encoder received a video frame"));
        };
        if a.channels as usize != self.channels {
            return Err(Error::invalid_input("the audio changed channel count mid-stream"));
        }
        let n = a.samples.len() / self.channels;
        for i in 0..n {
            for ch in 0..self.channels {
                self.pending[ch].push(a.samples[i * self.channels + self.channel_map[ch]] as f32);
            }
        }
        self.input += n as u64;
        while self.pending[0].len() >= FRAME_SAMPLES {
            let pcm: Vec<Vec<f32>> = self.pending.iter_mut().map(|p| p.drain(..FRAME_SAMPLES).collect()).collect();
            self.push_frame(&pcm)?;
        }
        Ok(self.out.drain(..).collect())
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        if self.input == 0 {
            return Ok(Vec::new());
        }
        // the last samples are padded out to a frame, as FFmpeg pads them
        if !self.pending[0].is_empty() {
            let pcm: Vec<Vec<f32>> = self
                .pending
                .iter_mut()
                .map(|p| {
                    let mut c = std::mem::take(p);
                    c.resize(FRAME_SAMPLES, 0.0);
                    c
                })
                .collect();
            self.push_frame(&pcm)?;
        }
        Ok(self.out.drain(..).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_is_the_ansi_crc() {
        // CRC-16/UMTS-like check value for "123456789" with poly 0x8005, init 0, no reflection: 0xFEE8
        assert_eq!(crc16(b"123456789"), 0xFEE8);
    }

    #[test]
    fn the_default_bit_rates_and_snap_match_ffmpeg() {
        assert_eq!([1, 2, 3, 4, 5].map(default_bitrate), [96000, 192000, 320000, 384000, 448000]);
        assert_eq!(snap_bitrate(100), 96);
        assert_eq!(snap_bitrate(640), 640);
        assert_eq!(snap_bitrate(10), 32);
    }

    #[test]
    fn exponent_groups_follow_the_strategies() {
        assert_eq!(exp_groups(false, EXP_D15, 73), 24);
        assert_eq!(exp_groups(false, EXP_D25, 73), 12);
        assert_eq!(exp_groups(false, EXP_D45, 73), 6);
        assert_eq!(exp_groups(false, EXP_D15, 7), 2);
    }

    #[test]
    fn exponent_of_a_coefficient_is_its_leading_bit_distance() {
        assert_eq!(exp_of(0), 24);
        assert_eq!(exp_of(1 << 23), 0);
        assert_eq!(exp_of(1), 23);
        assert_eq!(exp_of(-(1 << 20)), 3);
    }
}
