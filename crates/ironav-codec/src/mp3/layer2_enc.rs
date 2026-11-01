//! An MPEG-1 Audio Layer II (MP2) encoder: 32, 44.1 and 48 kHz, mono or stereo, constant bit rate.
//!
//! A translation of FFmpeg's `mpegaudioenc.c` (the `mp2` encoder, which takes 16-bit samples and
//! quantises with floats; the `mp2fixed` variant is not ported). FFmpeg is LGPL-2.1+. The analysis
//! filterbank is FFmpeg's integer one: a 512-tap window at 14 fractional bits, a 32-point integer
//! IDCT per slot, and a history of 512 samples per channel. The psychoacoustic model is FFmpeg's
//! fixed table of signal-to-mask ratios, so the bit allocation is the same greedy one FFmpeg runs:
//! the subband with the largest ratio gets the next allocation step until the frame's bits are spent.
//! Scale factors are shared among the three 12-slot groups of a subband where they are close, as
//! the standard allows. Joint stereo is not used.
//!
//! The encoder emits the frames FFmpeg's would (byte for byte, for the same 16-bit input), and at the
//! end of the stream enough further frames for the decoder to output every input sample: the
//! analysis delays the signal by `LEAD_IN` samples.

use super::layer2::{select_table_for, ALLOC, QUANT_BITS, QUANT_STEPS, SBLIMIT};
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, Result};
use std::collections::VecDeque;

const FRAME: usize = 1152;
const SLOTS: usize = 36;
const SUBBANDS: usize = 32;
/// Samples the decoder outputs before the first input sample: the analysis filterbank's delay.
pub const LEAD_IN: i64 = 481;
/// The sample rates MPEG-1 Layer II has, in the header's frequency index order.
const RATES: [u32; 3] = [44100, 48000, 32000];
/// The MPEG-1 Layer II bit rates in kbit/s, by bit rate index 1 to 14.
const BITRATES: [u32; 14] = [32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384];
/// Signal-to-mask ratios per subband, in dB (FFmpeg's `fixed_smr`, from the 'toolame' project).
const FIXED_SMR: [f32; 32] = [30.0, 17.0, 16.0, 10.0, 3.0, 12.0, 8.0, 2.5, 5.0, 5.0, 6.0, 6.0, 5.0, 6.0, 10.0, 6.0, -4.0, -10.0, -21.0, -30.0, -42.0, -55.0, -68.0, -75.0, -75.0, -75.0, -75.0, -75.0, -91.0, -107.0, -110.0, -108.0];
/// Signal-to-noise ratio of each quantiser class, in tenths of a dB.
const QUANT_SNR: [i32; 17] = [70, 110, 160, 208, 253, 316, 378, 439, 499, 559, 620, 680, 740, 800, 861, 920, 980];
/// Scale factor transmission: the number of scale factors each of the four codes carries.
const NB_SCALE_FACTORS: [i32; 4] = [3, 2, 1, 2];
/// The subband order of the IDCT's output (FFmpeg's `bitinv32`).
const BITINV32: [usize; 32] = [0, 16, 8, 24, 4, 20, 12, 28, 2, 18, 10, 26, 6, 22, 14, 30, 1, 17, 9, 25, 5, 21, 13, 29, 3, 19, 11, 27, 7, 23, 15, 31];
/// The cosines of the 32-point IDCT (FFmpeg's `costab32`), before conversion to fixed point.
const COSTAB32: [f64; 30] = [
    0.54119610014619701222,
    1.3065629648763763537,
    0.50979557910415917998,
    2.5629154477415054814,
    0.89997622313641556513,
    0.60134488693504528634,
    0.5024192861881556782,
    5.1011486186891552563,
    0.78815462345125020249,
    0.64682178335999007679,
    0.56694403481635768927,
    1.0606776859903470633,
    1.7224470982383341955,
    0.52249861493968885462,
    10.19000812354803287,
    0.674808341455005678,
    1.1694399334328846596,
    0.53104259108978413284,
    2.0577810099534108446,
    0.58293496820613388554,
    0.83934964541552681272,
    0.50547095989754364798,
    3.4076084184687189804,
    0.62250412303566482475,
    0.97256823786196078263,
    0.51544730992262455249,
    1.4841646163141661852,
    0.5531038960344445421,
    0.74453627100229857749,
    0.5006029982351962726,
];
/// The analysis window's first half (FFmpeg's `ff_mpa_enwindow`, 16 fractional bits).
const ENWINDOW: [i32; 257] = [
         0,    -1,    -1,    -1,    -1,    -1,    -1,    -2,
        -2,    -2,    -2,    -3,    -3,    -4,    -4,    -5,
        -5,    -6,    -7,    -7,    -8,    -9,   -10,   -11,
       -13,   -14,   -16,   -17,   -19,   -21,   -24,   -26,
       -29,   -31,   -35,   -38,   -41,   -45,   -49,   -53,
       -58,   -63,   -68,   -73,   -79,   -85,   -91,   -97,
      -104,  -111,  -117,  -125,  -132,  -139,  -147,  -154,
      -161,  -169,  -176,  -183,  -190,  -196,  -202,  -208,
       213,   218,   222,   225,   227,   228,   228,   227,
       224,   221,   215,   208,   200,   189,   177,   163,
       146,   127,   106,    83,    57,    29,    -2,   -36,
       -72,  -111,  -153,  -197,  -244,  -294,  -347,  -401,
      -459,  -519,  -581,  -645,  -711,  -779,  -848,  -919,
      -991, -1064, -1137, -1210, -1283, -1356, -1428, -1498,
     -1567, -1634, -1698, -1759, -1817, -1870, -1919, -1962,
     -2001, -2032, -2057, -2075, -2085, -2087, -2080, -2063,
      2037,  2000,  1952,  1893,  1822,  1739,  1644,  1535,
      1414,  1280,  1131,   970,   794,   605,   402,   185,
       -45,  -288,  -545,  -814, -1095, -1388, -1692, -2006,
     -2330, -2663, -3004, -3351, -3705, -4063, -4425, -4788,
     -5153, -5517, -5879, -6237, -6589, -6935, -7271, -7597,
     -7910, -8209, -8491, -8755, -8998, -9219, -9416, -9585,
     -9727, -9838, -9916, -9959, -9966, -9935, -9863, -9750,
     -9592, -9389, -9139, -8840, -8492, -8092, -7640, -7134,
      6574,  5959,  5288,  4561,  3776,  2935,  2037,  1082,
        70,  -998, -2122, -3300, -4533, -5818, -7154, -8540,
     -9975,-11455,-12980,-14548,-16155,-17799,-19478,-21189,
    -22929,-24694,-26482,-28289,-30112,-31947,-33791,-35640,
    -37489,-39336,-41176,-43006,-44821,-46617,-48390,-50137,
    -51853,-53534,-55178,-56778,-58333,-59838,-61289,-62684,
    -64019,-65290,-66494,-67629,-68692,-69679,-70590,-71420,
    -72169,-72835,-73415,-73908,-74313,-74630,-74856,-74992,
     75038,
];

/// Fixed point for the IDCT and the window: 15 fractional bits.
const FRAC_BITS: u32 = 15;
/// The window's coefficients are kept at 14 fractional bits, and the filter's sums shifted down by
/// the same amount: the result is in the IDCT's 15-bit fixed point.
const WSHIFT: u32 = 14;

/// Fixed-point multiply, as FFmpeg's `MUL`.
#[inline]
fn mul(a: i32, b: i32) -> i32 {
    ((a as i64 * b as i64) >> FRAC_BITS) as i32
}

/// The 32-point IDCT of FFmpeg's `idct32`, in place on `tab`; `out` gets the 32 subband samples.
fn idct32(costab: &[i32; 30], fix_sqrt: i32, tab: &mut [i32; 32], out: &mut [i32]) {
    for j in (3..=31).rev().step_by(2) {
        tab[j] = tab[j].wrapping_add(tab[j - 2]);
    }
    let mut t = 30;
    while t != 2 {
        tab[t] = tab[t].wrapping_add(tab[t - 4]);
        tab[t + 1] = tab[t + 1].wrapping_add(tab[t - 3]);
        t -= 4;
    }
    let mut t = 28;
    while t != 4 {
        for k in 0..4 {
            tab[t + k] = tab[t + k].wrapping_add(tab[t + k - 8]);
        }
        t -= 8;
    }
    for base in [0, 16] {
        for o in [3, 6, 11, 12, 13, 15] {
            tab[base + o] = tab[base + o].wrapping_neg();
        }
    }
    for t in 0..8 {
        let mut x3 = mul(tab[t + 16], fix_sqrt);
        let x4 = tab[t].wrapping_sub(x3);
        x3 = tab[t].wrapping_add(x3);
        let x2 = mul(-(tab[t + 24].wrapping_add(tab[t + 8])), fix_sqrt);
        let x1 = mul(tab[t + 8].wrapping_sub(x2), costab[0]);
        let x2 = mul(tab[t + 8].wrapping_add(x2), costab[1]);
        tab[t] = x3.wrapping_add(x1);
        tab[t + 8] = x4.wrapping_sub(x2);
        tab[t + 16] = x4.wrapping_add(x2);
        tab[t + 24] = x3.wrapping_sub(x1);
    }
    for t in 0..4 {
        let xr = mul(tab[t + 28], costab[2]);
        tab[t + 28] = tab[t].wrapping_sub(xr);
        tab[t] = tab[t].wrapping_add(xr);

        let xr = mul(tab[t + 4], costab[3]);
        tab[t + 4] = tab[t + 24].wrapping_sub(xr);
        tab[t + 24] = tab[t + 24].wrapping_add(xr);

        let xr = mul(tab[t + 20], costab[4]);
        tab[t + 20] = tab[t + 8].wrapping_sub(xr);
        tab[t + 8] = tab[t + 8].wrapping_add(xr);

        let xr = mul(tab[t + 12], costab[5]);
        tab[t + 12] = tab[t + 16].wrapping_sub(xr);
        tab[t + 16] = tab[t + 16].wrapping_add(xr);
    }
    let mut xp = 6;
    for i in 0..4 {
        let xr = mul(tab[30 - i * 4], costab[xp]);
        tab[30 - i * 4] = tab[i * 4].wrapping_sub(xr);
        tab[i * 4] = tab[i * 4].wrapping_add(xr);

        let xr = mul(tab[2 + i * 4], costab[xp + 1]);
        tab[2 + i * 4] = tab[28 - i * 4].wrapping_sub(xr);
        tab[28 - i * 4] = tab[28 - i * 4].wrapping_add(xr);

        let xr = mul(tab[31 - i * 4], costab[xp]);
        tab[31 - i * 4] = tab[1 + i * 4].wrapping_sub(xr);
        tab[1 + i * 4] = tab[1 + i * 4].wrapping_add(xr);

        let xr = mul(tab[3 + i * 4], costab[xp + 1]);
        tab[3 + i * 4] = tab[29 - i * 4].wrapping_sub(xr);
        tab[29 - i * 4] = tab[29 - i * 4].wrapping_add(xr);
        xp += 2;
    }
    for k in 0..16 {
        let (t, t1) = (30 - 2 * k, 1 + 2 * k);
        let xr = mul(tab[t1], costab[xp]);
        tab[t1] = tab[t].wrapping_sub(xr);
        tab[t] = tab[t].wrapping_add(xr);
        xp += 1;
    }
    for i in 0..32 {
        out[i] = tab[BITINV32[i]];
    }
}

/// Packs bits, most significant first, into a buffer of zeros.
struct Bits {
    buf: Vec<u8>,
    pos: usize,
}

impl Bits {
    fn put(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            if (v >> i) & 1 != 0 {
                self.buf[self.pos >> 3] |= 0x80 >> (self.pos & 7);
            }
            self.pos += 1;
        }
    }
}

/// Quantises a sample to a class with `steps` levels, as FFmpeg's float path does.
#[inline]
fn quantise(sample: i32, inv: f32, steps: i32) -> i32 {
    let a = sample as f32 * inv;
    let q = ((a as f64 + 1.0) * steps as f64 * 0.5) as i32;
    q.clamp(0, steps - 1)
}

/// An MPEG-1 Layer II encoder. Each packet is one frame of 1152 samples per channel.
pub struct Mp2Encoder {
    channels: usize,
    bitrate_index: u32,
    freq_index: u32,
    sblimit: usize,
    /// The allocation table's entries (see `layer2::ALLOC`), and where each subband's starts in it.
    alloc: &'static [u8],
    offsets: [usize; SUBBANDS],
    /// Frame size in bits (without the padding byte), and the fraction of a bit per frame that
    /// makes a padding byte due.
    frame_bits: i32,
    frame_frac: i32,
    frame_frac_incr: i32,
    costab: [i32; 30],
    fix_sqrt: i32,
    filter_bank: [i32; 512],
    /// 2^((3 - i) / 3) in fixed point: the scale factor table.
    scale_table: [i32; 64],
    /// 2^(-(3 - i) / 3) / 2^20: each scale factor's inverse, to divide by.
    scale_inv: [f32; 64],
    /// Scale factor differences, by `d + 64`, mapped to the five classes the codes distinguish.
    diff_table: [u8; 128],
    /// Bits of each quantiser class for its 12 groups of three values.
    total_bits: [i32; 17],
    /// Per channel, the last 512 input samples, newest first.
    history: Vec<[i32; 512]>,
    pending: Vec<Vec<i16>>,
    input: u64,
    frames_out: u64,
    out: VecDeque<Packet>,
}

/// One frame's subband samples: [slot][subband].
type Slots = [[i32; SUBBANDS]; SLOTS];

impl Mp2Encoder {
    /// An encoder for `channels` (1 or 2) at `sample_rate` (32000, 44100 or 48000) and `bitrate`
    /// bits per second, which is snapped to the nearest Layer II rate.
    pub fn new(sample_rate: u32, channels: usize, bitrate: u32) -> Result<Self> {
        if !(1..=2).contains(&channels) {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the MP2 encoder takes mono or stereo audio, not {channels} channels")));
        }
        let Some(freq_index) = RATES.iter().position(|&r| r == sample_rate) else {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the MP2 encoder writes MPEG-1 only, which has no {sample_rate} Hz"))
                .hint("resample to 32000, 44100 or 48000 Hz"));
        };
        let kbps = Self::snap(bitrate / 1000);
        let bitrate_index = BITRATES.iter().position(|&b| b == kbps).unwrap() as u32 + 1;

        // a frame is 1152 samples: its size in bits is the bit rate times that, over the rate
        let a = ((kbps * 1000 * 1152) as f32 as f64 / (sample_rate as f64 * 8.0)) as f32;
        let frame_bits = (a as i32) * 8;
        let frame_frac_incr = ((a as f64 - (a as f64).floor()) * 65536.0) as i32;

        let table = select_table_for(kbps, channels as u32, sample_rate, false);
        let sblimit = SBLIMIT[table];
        let alloc = ALLOC[table];
        let mut offsets = [0usize; SUBBANDS];
        let mut j = 0;
        for o in offsets.iter_mut().take(sblimit) {
            *o = j;
            j += 1 << alloc[j];
        }

        let costab = COSTAB32.map(|c| (c * (1 << FRAC_BITS) as f64) as i32);
        let fix_sqrt = (std::f64::consts::SQRT_2 * 0.5 * (1 << FRAC_BITS) as f64) as i32;

        let mut filter_bank = [0i32; 512];
        for i in 0..257 {
            // 16 to 14 fractional bits
            let v = (ENWINDOW[i] + 2) >> 2;
            filter_bank[i] = v;
            let w = if i & 63 != 0 { -v } else { v };
            if i != 0 {
                filter_bank[512 - i] = w;
            }
        }

        let mut scale_table = [0i32; 64];
        let mut scale_inv = [0f32; 64];
        for i in 0..64 {
            scale_table[i] = (((3 - i as i32) as f64 / 3.0).exp2() * (1 << 20) as f64) as i32;
            if scale_table[i] <= 0 {
                scale_table[i] = 1;
            }
            scale_inv[i] = (((-(3 - i as i32)) as f64 / 3.0).exp2() / (1 << 20) as f32 as f64) as f32;
        }

        let mut diff_table = [0u8; 128];
        for (i, d) in diff_table.iter_mut().enumerate() {
            let v = i as i32 - 64;
            *d = if v <= -3 {
                0
            } else if v < 0 {
                1
            } else if v == 0 {
                2
            } else if v < 3 {
                3
            } else {
                4
            };
        }

        let mut total_bits = [0i32; 17];
        for (i, t) in total_bits.iter_mut().enumerate() {
            let q = QUANT_BITS[i];
            *t = 12 * if q < 0 { -q } else { q * 3 };
        }

        Ok(Self {
            channels,
            bitrate_index,
            freq_index: freq_index as u32,
            sblimit,
            alloc,
            offsets,
            frame_bits,
            frame_frac: 0,
            frame_frac_incr,
            costab,
            fix_sqrt,
            filter_bank,
            scale_table,
            scale_inv,
            diff_table,
            total_bits,
            history: vec![[0; 512]; channels],
            pending: vec![Vec::new(); channels],
            input: 0,
            frames_out: 0,
            out: VecDeque::new(),
        })
    }

    /// The bit rate a stream gets when none is asked for: 96 kbit/s mono, 192 kbit/s stereo.
    pub fn default_bitrate(channels: usize) -> u32 {
        96_000 * channels as u32
    }

    /// The Layer II bit rate nearest to `kbps`.
    pub fn snap(kbps: u32) -> u32 {
        *BITRATES.iter().min_by_key(|&&b| b.abs_diff(kbps)).unwrap()
    }

    /// The analysis filterbank over one frame of one channel: 36 slots of 32 subband samples.
    fn analyse(&self, history: &mut [i32; 512], pcm: &[i16]) -> Slots {
        let mut out = [[0i32; SUBBANDS]; SLOTS];
        for (j, slot) in out.iter_mut().enumerate() {
            history.copy_within(0..512 - 32, 32);
            for i in 0..32 {
                history[31 - i] = pcm[j * 32 + i] as i32;
            }
            let mut tmp = [0i32; 64];
            for (i, t) in tmp.iter_mut().enumerate() {
                let mut sum = 0i32;
                for k in 0..8 {
                    sum = sum.wrapping_add(history[i + 64 * k].wrapping_mul(self.filter_bank[i + 64 * k]));
                }
                *t = sum;
            }
            let mut tmp1 = [0i32; 32];
            tmp1[0] = tmp[16] >> WSHIFT;
            for i in 1..=16 {
                tmp1[i] = tmp[i + 16].wrapping_add(tmp[16 - i]) >> WSHIFT;
            }
            for i in 17..=31 {
                tmp1[i] = tmp[i + 16].wrapping_sub(tmp[80 - i]) >> WSHIFT;
            }
            idct32(&self.costab, self.fix_sqrt, &mut tmp1, slot);
        }
        out
    }

    /// Each subband's scale factors (one per 12-slot group) and the code that says which repeat.
    fn scale_factors(&self, slots: &Slots) -> ([[u8; 3]; SUBBANDS], [u8; SUBBANDS]) {
        let mut sfs = [[0u8; 3]; SUBBANDS];
        let mut codes = [0u8; SUBBANDS];
        for j in 0..self.sblimit {
            let mut sf = [0u8; 3];
            for (i, s) in sf.iter_mut().enumerate() {
                let vmax = (0..12).map(|k| slots[i * 12 + k][j].abs()).max().unwrap();
                let index = if vmax > 1 {
                    let n = 31 - (vmax as u32).leading_zeros() as i32;
                    let mut index = (21 - n) * 3 - 3;
                    if index >= 0 {
                        while vmax <= self.scale_table[index as usize + 1] {
                            index += 1;
                        }
                        index
                    } else {
                        0
                    }
                } else {
                    62
                };
                *s = index as u8;
            }
            let d1 = self.diff_table[(sf[0] as i32 - sf[1] as i32 + 64) as usize] as usize;
            let d2 = self.diff_table[(sf[1] as i32 - sf[2] as i32 + 64) as usize] as usize;
            // the 25 cases: which factors may repeat, and the code that says so
            let code = match d1 * 5 + d2 {
                0 | 4 | 19 | 20 | 24 => 0,
                1 | 2 | 21 | 22 => {
                    sf[2] = sf[1];
                    3
                }
                3 | 23 => {
                    sf[1] = sf[2];
                    3
                }
                5 | 9 | 14 => {
                    sf[1] = sf[0];
                    1
                }
                6 | 7 | 10 | 11 | 12 => {
                    sf[1] = sf[0];
                    sf[2] = sf[0];
                    2
                }
                13 | 18 => {
                    // FFmpeg's chained assignment: every factor takes the last one
                    sf[1] = sf[2];
                    sf[0] = sf[1];
                    2
                }
                15..=17 => {
                    sf[2] = sf[1];
                    sf[0] = sf[2];
                    2
                }
                _ => {
                    // 8: the factors meet at the smaller of the outer two
                    if sf[0] > sf[2] {
                        sf[0] = sf[2];
                    }
                    sf[1] = sf[0];
                    sf[2] = sf[0];
                    2
                }
            };
            sfs[j] = sf;
            codes[j] = code;
        }
        (sfs, codes)
    }

    /// Decides how many bits each subband gets, greedily by signal-to-mask ratio (FFmpeg's
    /// `compute_bit_allocation`). Returns the allocation, the frame's size in bytes and whether it has
    /// a padding byte.
    fn allocate(&mut self, smr1: &[[i32; SUBBANDS]; 2], codes: &[[u8; SUBBANDS]; 2]) -> ([[u8; SUBBANDS]; 2], usize, bool) {
        let nch = self.channels;
        // the frame's padding: a byte in every frame where the fraction of a bit adds up to one
        self.frame_frac += self.frame_frac_incr;
        let padded = self.frame_frac >= 65536;
        if padded {
            self.frame_frac -= 65536;
        }
        let max_bits = self.frame_bits + if padded { 8 } else { 0 };

        // the header and the allocation fields, before any subband is allocated
        let mut current = 32i32;
        for i in 0..self.sblimit {
            current += self.alloc[self.offsets[i]] as i32 * nch as i32;
        }

        let mut smr = *smr1;
        // 0: nothing coded yet, 1: some bits, 2: no more
        let mut status = [[0u8; SUBBANDS]; 2];
        let mut bit_alloc = [[0u8; SUBBANDS]; 2];
        loop {
            let mut best: Option<(usize, usize)> = None;
            let mut best_smr = i32::MIN;
            for ch in 0..nch {
                for i in 0..self.sblimit {
                    if smr[ch][i] > best_smr && status[ch][i] != 2 {
                        best_smr = smr[ch][i];
                        best = Some((ch, i));
                    }
                }
            }
            let Some((ch, sb)) = best else { break };

            let o = self.offsets[sb];
            let alloc = self.alloc;
            let incr = if status[ch][sb] == 0 {
                2 + NB_SCALE_FACTORS[codes[ch][sb] as usize] * 6 + self.total_bits[alloc[o + 1] as usize]
            } else {
                let b = bit_alloc[ch][sb] as usize;
                self.total_bits[alloc[o + b + 1] as usize] - self.total_bits[alloc[o + b] as usize]
            };
            if current + incr <= max_bits {
                bit_alloc[ch][sb] += 1;
                let b = bit_alloc[ch][sb] as usize;
                current += incr;
                smr[ch][sb] = smr1[ch][sb] - QUANT_SNR[alloc[o + b] as usize];
                status[ch][sb] = if b == (1 << alloc[o]) - 1 { 2 } else { 1 };
            } else {
                status[ch][sb] = 2;
            }
        }
        debug_assert!(current <= max_bits);
        (bit_alloc, (max_bits / 8) as usize, padded)
    }

    /// Encodes one frame of 1152 samples per channel.
    fn encode_frame(&mut self, pcm: &[Vec<i16>]) -> Vec<u8> {
        let nch = self.channels;
        let mut slots = Vec::with_capacity(nch);
        for ch in 0..nch {
            let mut history = self.history[ch];
            slots.push(self.analyse(&mut history, &pcm[ch]));
            self.history[ch] = history;
        }
        let mut sfs = [[[0u8; 3]; SUBBANDS]; 2];
        let mut codes = [[0u8; SUBBANDS]; 2];
        for ch in 0..nch {
            (sfs[ch], codes[ch]) = self.scale_factors(&slots[ch]);
        }
        // the fixed psychoacoustic model: the same ratio for every frame
        let mut smr = [[0i32; SUBBANDS]; 2];
        for row in smr.iter_mut() {
            for (i, v) in row.iter_mut().enumerate().take(self.sblimit) {
                *v = (FIXED_SMR[i] * 10.0) as i32;
            }
        }
        let (bit_alloc, frame_bytes, padded) = self.allocate(&smr, &codes);

        let mut w = Bits { buf: vec![0u8; frame_bytes], pos: 0 };
        // header: MPEG-1, Layer II, no CRC; mono or joint-free stereo, original, no emphasis
        w.put(12, 0xFFF);
        w.put(1, 1);
        w.put(2, 4 - 2);
        w.put(1, 1);
        w.put(4, self.bitrate_index);
        w.put(2, self.freq_index);
        w.put(1, padded as u32);
        w.put(1, 0);
        w.put(2, if nch == 2 { 0 } else { 3 });
        w.put(2, 0);
        w.put(1, 0);
        w.put(1, 1);
        w.put(2, 0);

        // bit allocation, then the scale factor codes, then the scale factors themselves
        for i in 0..self.sblimit {
            let nb = self.alloc[self.offsets[i]] as u32;
            for ch in 0..nch {
                w.put(nb, bit_alloc[ch][i] as u32);
            }
        }
        for i in 0..self.sblimit {
            for ch in 0..nch {
                if bit_alloc[ch][i] != 0 {
                    w.put(2, codes[ch][i] as u32);
                }
            }
        }
        for i in 0..self.sblimit {
            for ch in 0..nch {
                if bit_alloc[ch][i] == 0 {
                    continue;
                }
                let sf = sfs[ch][i];
                match codes[ch][i] {
                    0 => w.put(18, (sf[0] as u32) << 12 | (sf[1] as u32) << 6 | sf[2] as u32),
                    1 | 3 => w.put(12, (sf[0] as u32) << 6 | sf[2] as u32),
                    2 => w.put(6, sf[0] as u32),
                    _ => {}
                }
            }
        }

        // the samples: for each 12-slot group, 3 slots of each subband at a time, grouped where the
        // class allows
        for k in 0..3 {
            for l in (0..12).step_by(3) {
                for i in 0..self.sblimit {
                    let o = self.offsets[i];
                    for ch in 0..nch {
                        let b = bit_alloc[ch][i] as usize;
                        if b == 0 {
                            continue;
                        }
                        let q = self.alloc[o + b] as usize;
                        let steps = QUANT_STEPS[q] as i32;
                        let inv = self.scale_inv[sfs[ch][i][k] as usize];
                        let mut v = [0i32; 3];
                        for (m, x) in v.iter_mut().enumerate() {
                            *x = quantise(slots[ch][k * 12 + l + m][i], inv, steps);
                        }
                        let bits = QUANT_BITS[q];
                        if bits < 0 {
                            w.put((-bits) as u32, (v[0] + steps * (v[1] + steps * v[2])) as u32);
                        } else {
                            for &x in &v {
                                w.put(bits as u32, x as u32);
                            }
                        }
                    }
                }
            }
        }
        w.buf
    }

    /// Encodes one frame from `pcm` (1152 samples per channel) and queues it.
    fn push_frame(&mut self, pcm: &[Vec<i16>]) {
        let data = self.encode_frame(pcm);
        self.out.push_back(Packet { stream: 0, pts: self.frames_out as i64 * FRAME as i64, duration: FRAME as i64, data });
        self.frames_out += 1;
    }
}

impl Encoder for Mp2Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid_input("the MP2 encoder received a video frame"));
        };
        if a.channels as usize != self.channels {
            return Err(Error::invalid_input("the audio changed channel count mid-stream"));
        }
        let n = a.samples.len() / self.channels;
        for i in 0..n {
            for c in 0..self.channels {
                // the encoder works on 16-bit samples, as FFmpeg's does
                let x = (a.samples[i * self.channels + c] * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
                self.pending[c].push(x);
            }
        }
        self.input += n as u64;
        while self.pending[0].len() >= FRAME {
            let pcm: Vec<Vec<i16>> = self.pending.iter_mut().map(|b| b.drain(..FRAME).collect()).collect();
            self.push_frame(&pcm);
        }
        Ok(self.out.drain(..).collect())
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        if self.input == 0 {
            return Ok(Vec::new());
        }
        // the last samples are padded out to a frame, then frames of silence follow until the decoder
        // has output every input sample
        if !self.pending[0].is_empty() {
            let pcm: Vec<Vec<i16>> = self.pending.iter_mut().map(|b| {
                let mut c: Vec<i16> = std::mem::take(b);
                c.resize(FRAME, 0);
                c
            }).collect();
            self.push_frame(&pcm);
        }
        let wanted = (self.input + LEAD_IN as u64).div_ceil(FRAME as u64);
        let silence = vec![vec![0i16; FRAME]; self.channels];
        while self.frames_out < wanted {
            self.push_frame(&silence);
        }
        Ok(self.out.drain(..).collect())
    }
}
