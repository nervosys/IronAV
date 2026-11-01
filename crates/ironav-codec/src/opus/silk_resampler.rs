//! The SILK decoder's resampler: 8, 12 or 16 kHz SILK output to 48 kHz, on 16-bit samples, as
//! libopus does it.
//!
//! Translated from libopus's `silk/resampler.c`, `silk/resampler_private_IIR_FIR.c`,
//! `silk/resampler_private_up2_HQ.c` and `silk/resampler_rom.c`. The constants were read from the
//! copy of libopus that FFmpeg's binary links (the interpolation table and the up2 allpass
//! coefficients are there as data or as immediates in the code). libopus is Copyright (c)
//! 2006-2011 Skype Limited, Copyright (c) 2012-2020 Xiph.Org Foundation and contributors, and is
//! distributed under the BSD 3-Clause licence: <https://opus-codec.org/license/>.
//!
//! Each call takes a run of SILK samples and gives 6, 4 or 3 times as many at 48 kHz. The input
//! is delayed by `input_delay` samples, then 2x upsampled by two chains of three first-order
//! allpass sections (the half-band `up2_HQ` filter), then interpolated to 48 kHz by an 8-tap
//! filter in 12 fractional positions. The arithmetic is integer, as in libopus. On libopus-coded
//! streams the output agrees with libopus's decode to the rounding of SILK's own synthesis (see
//! the Opus decoder's tests).

/// Samples of history kept by the interpolation filter (`RESAMPLER_ORDER_FIR_12`).
const ORDER_FIR: usize = 8;
/// Milliseconds of input per batch: libopus processes the input in runs of this length, each
/// starting its fractional position afresh (`RESAMPLER_MAX_BATCH_SIZE_MS`).
const BATCH_MS: usize = 10;

/// The 12 fractional positions of the interpolation filter, four of its eight taps each (the
/// other four are these in reverse order, from the row 11 - k).
const FRAC_FIR_12: [[i16; 4]; 12] = [
    [189, -600, 617, 30567],
    [117, -159, -1070, 29704],
    [52, 221, -2392, 28276],
    [-4, 529, -3350, 26341],
    [-48, 758, -3956, 23973],
    [-80, 905, -4235, 21254],
    [-99, 972, -4222, 18278],
    [-107, 967, -3957, 15143],
    [-103, 896, -3487, 11950],
    [-91, 773, -2865, 8798],
    [-71, 611, -2143, 5784],
    [-46, 425, -1375, 2996],
];

/// The allpass coefficients of the 2x upsampler, Q16, for the even and the odd output samples.
/// The third of each is stored as the libopus table has it, offset by -65536.
const UP2_HQ_0: [i32; 3] = [1746, 14986, 39083 - 65536];
const UP2_HQ_1: [i32; 3] = [6854, 25769, 55542 - 65536];

/// The input delay, in samples at the SILK rate, of the resampler to 48 kHz: 0, 4 and 7 samples
/// for 8, 12 and 16 kHz. These make the output line up with libopus's decoder on libopus-coded
/// streams at every rate (see the tests of the Opus decoder); they are not read from libopus's
/// `delay_matrix_dec`, whose values could not be recovered from the binary.
pub(super) const fn input_delay(fs_in_khz: usize) -> usize {
    match fs_in_khz {
        8 => 0,
        12 => 4,
        _ => 7,
    }
}

/// `(a * b) >> 16` with the product in 64 bits: libopus's `silk_SMULWB` on 16-bit `b`.
#[inline]
fn smulwb(a: i32, b: i32) -> i32 {
    ((a as i64 * b as i64) >> 16) as i32
}

#[inline]
fn sat16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// One 2x upsampler: three first-order allpass sections on the even output samples and three on
/// the odd ones (`silk_resampler_private_up2_HQ`). `s` is its state, six values.
fn up2_hq(s: &mut [i32; 6], out: &mut [i16], input: &[i16]) {
    for (k, &x) in input.iter().enumerate() {
        // libopus works in Q10 inside the filter
        let in32 = (x as i32) << 10;
        let even = allpass3(in32, &mut s[0..3], UP2_HQ_0);
        let odd = allpass3(in32, &mut s[3..6], UP2_HQ_1);
        out[2 * k] = sat16((even + 512) >> 10);
        out[2 * k + 1] = sat16((odd + 512) >> 10);
    }
}

/// The three allpass sections of one branch, applied in turn to `u`.
#[inline]
fn allpass3(mut u: i32, s: &mut [i32], c: [i32; 3]) -> i32 {
    for i in 0..3 {
        let y = u - s[i];
        // the third section is `SMLAWB(y, y, c)`, which is `y + ((y * c) >> 16)`
        let x = if i < 2 { smulwb(y, c[i]) } else { y + smulwb(y, c[i]) };
        let out = s[i] + x;
        s[i] = u + x;
        u = out;
    }
    u
}

/// The resampler state for one channel (`silk_resampler_state_struct`).
#[derive(Clone)]
pub struct SilkResampler {
    fs_in_khz: usize,
    input_delay: usize,
    /// Input samples per batch.
    batch: usize,
    /// Input position increment per output sample, Q16, in units of the 2x upsampled signal.
    inc_q16: i64,
    s_iir: [i32; 6],
    s_fir: [i16; ORDER_FIR],
    delay_buf: [i16; 16],
}

impl SilkResampler {
    /// A resampler from `fs_in_khz` (8, 12 or 16) to 48 kHz, with empty history.
    pub fn new(fs_in_khz: usize) -> Self {
        // one output sample is `fs_in / 48` input samples, that is `2 * fs_in / 48` samples of
        // the 2x upsampled signal; libopus rounds the Q16 value up
        let inc_q16 = (((fs_in_khz as u64) << 17).div_ceil(48)) as i64;
        Self {
            fs_in_khz,
            input_delay: input_delay(fs_in_khz),
            batch: fs_in_khz * BATCH_MS,
            inc_q16,
            s_iir: [0; 6],
            s_fir: [0; ORDER_FIR],
            delay_buf: [0; 16],
        }
    }

    /// Output samples per input sample: 6, 4 or 3.
    pub fn ratio(&self) -> usize {
        48 / self.fs_in_khz
    }

    /// Resamples `input` (a whole number of milliseconds at the SILK rate) into `out`, which is
    /// `input.len() * ratio()` long.
    pub fn process(&mut self, input: &[i16], out: &mut [i16]) {
        let fs = self.fs_in_khz;
        let d = self.input_delay;
        let r = self.ratio();
        let n = fs - d;
        // the first millisecond, with the delayed samples from the last call in front
        let mut first = [0i16; 16];
        first[..d].copy_from_slice(&self.delay_buf[..d]);
        first[d..fs].copy_from_slice(&input[..n]);
        self.iir_fir(&first[..fs], &mut out[..fs * r]);
        // the last `d` samples wait for the next call
        let len = input.len();
        self.delay_buf[..d].copy_from_slice(&input[len - d..]);
        if len > fs {
            self.iir_fir(&input[n..len - d], &mut out[fs * r..]);
        }
    }

    /// `silk_resampler_private_IIR_FIR`: 2x upsampling, then the 12-position interpolation, in
    /// batches of `batch` input samples.
    fn iir_fir(&mut self, input: &[i16], out: &mut [i16]) {
        let mut buf = [0i16; 8 + 2 * 160];
        buf[..ORDER_FIR].copy_from_slice(&self.s_fir);
        let mut rest = input;
        let mut o = 0;
        let mut last = 0;
        while !rest.is_empty() {
            let n = rest.len().min(self.batch);
            up2_hq(&mut self.s_iir, &mut buf[ORDER_FIR..ORDER_FIR + 2 * n], &rest[..n]);
            let max_index = (n as i64) << 17;
            let mut index = 0i64;
            while index < max_index {
                let p = (index >> 16) as usize;
                let t = (((index & 0xFFFF) * 12) >> 16) as usize;
                let b = &buf[p..p + ORDER_FIR];
                let (f, g) = (&FRAC_FIR_12[t], &FRAC_FIR_12[11 - t]);
                let res = b[0] as i32 * f[0] as i32
                    + b[1] as i32 * f[1] as i32
                    + b[2] as i32 * f[2] as i32
                    + b[3] as i32 * f[3] as i32
                    + b[4] as i32 * g[3] as i32
                    + b[5] as i32 * g[2] as i32
                    + b[6] as i32 * g[1] as i32
                    + b[7] as i32 * g[0] as i32;
                out[o] = sat16((res + (1 << 14)) >> 15);
                o += 1;
                index += self.inc_q16;
            }
            rest = &rest[n..];
            last = n;
            if !rest.is_empty() {
                buf.copy_within(2 * n..2 * n + ORDER_FIR, 0);
            }
        }
        self.s_fir.copy_from_slice(&buf[2 * last..2 * last + ORDER_FIR]);
    }
}
