//! The 32-band QMF synthesis filter bank and the LFE interpolation, as FFmpeg's float decoder runs
//! them (`dcadsp.c` `sub_qmf32_float_c`, `lfe_fir_float_c`, and `synth_filter.c`
//! `synth_filter_float`). FFmpeg's inverse MDCT of 32 inputs (which gives the 32 samples in the
//! middle of the full inverse transform) is done here as a 32 by 32 matrix product.

use std::f64::consts::PI;
use std::sync::OnceLock;

/// The synthesis state of one channel: the last 512 values of the inverse transforms, where the next
/// one goes, and the half of the windowed sum carried to the next block.
pub(super) struct ChannelSynth {
    hist1: [f32; 512],
    offset: usize,
    hist2: [f32; 32],
}

impl ChannelSynth {
    pub(super) fn new() -> Self {
        ChannelSynth { hist1: [0.0; 512], offset: 0, hist2: [0.0; 32] }
    }
}

/// `-cos(pi / 32 * (i + 32 + 1/2) * (k + 1/2))`, the inverse MDCT of 32 coefficients: the samples
/// 16 to 47 of the 64 it makes (with the sign FFmpeg's transform gives it).
fn cos_matrix() -> &'static [[f32; 32]; 32] {
    static M: OnceLock<[[f32; 32]; 32]> = OnceLock::new();
    M.get_or_init(|| {
        let mut m = [[0f32; 32]; 32];
        for (i, row) in m.iter_mut().enumerate() {
            for (k, v) in row.iter_mut().enumerate() {
                *v = -(PI / 32.0 * (i as f64 + 32.0 + 0.5) * (k as f64 + 0.5)).cos() as f32;
            }
        }
        m
    })
}

/// One synthesis step: 32 subband samples in, 32 PCM samples out.
fn synth_filter(st: &mut ChannelSynth, window: &[f32; 512], out: &mut [f32], input: &[f32; 32], scale: f32) {
    let m = cos_matrix();
    let off = st.offset;
    for (i, row) in m.iter().enumerate() {
        let mut acc = 0f32;
        for k in 0..32 {
            acc += row[k] * input[k];
        }
        st.hist1[off + i] = acc;
    }
    for i in 0..16 {
        let mut a = st.hist2[i];
        let mut b = st.hist2[i + 16];
        let mut c = 0f32;
        let mut d = 0f32;
        for j in (0..512).step_by(64) {
            let p = off + j;
            a += window[i + j] * -st.hist1[(p + 15 - i) & 511];
            b += window[i + j + 16] * st.hist1[(p + i) & 511];
            c += window[i + j + 32] * st.hist1[(p + 16 + i) & 511];
            d += window[i + j + 48] * st.hist1[(p + 31 - i) & 511];
        }
        out[i] = a * scale;
        out[i + 16] = b * scale;
        st.hist2[i] = c;
        st.hist2[i + 16] = d;
    }
    st.offset = (off + 480) & 511;
}

/// Synthesises `nblocks` blocks of 32 PCM samples from one channel's subband samples (`bands[b]`
/// holds band `b`'s samples, a slice of at least `nblocks`).
pub(super) fn qmf_synth(st: &mut ChannelSynth, window: &[f32; 512], bands: &[&[i32]], nblocks: usize, out: &mut [f32]) {
    let mut input = [0f32; 32];
    for j in 0..nblocks {
        for (i, band) in bands.iter().enumerate().take(32) {
            let v = band[j] as f32;
            input[i] = if (i as i32 - 1) & 2 != 0 { -v } else { v };
        }
        synth_filter(st, window, &mut out[j * 32..j * 32 + 32], &input, 1.0 / (1 << 17) as f32);
    }
}

/// Interpolates the decimated LFE samples to PCM: `lfe` points at the first new sample with the 7
/// before it (or 3, at the 128 times factor) being history; `dec_select` is 0 for the 64 times
/// factor and 1 for 128 times. `pcm` gets `32 * npcmblocks` samples.
pub(super) fn lfe_fir(pcm: &mut [f32], lfe: &[i32], base: usize, filter: &[f32; 256], npcmblocks: usize, dec_select: usize) {
    let factor = 64 << dec_select;
    let ncoeffs = 8 >> dec_select;
    let n = npcmblocks >> (dec_select + 1);
    for i in 0..n {
        let cur = base + i;
        for j in 0..factor / 2 {
            let mut a = 0f32;
            let mut b = 0f32;
            for k in 0..ncoeffs {
                let s = lfe[cur - k] as f32;
                a += filter[j * ncoeffs + k] * s;
                b += filter[255 - j * ncoeffs - k] * s;
            }
            pcm[i * factor + j] = a;
            pcm[i * factor + factor / 2 + j] = b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dts::tables::FIR_32_PERFECT;

    /// A subband signal of a constant in band 0 gives a steady output once the filter has filled
    /// (the window's overlap is 16 blocks), and silence in gives silence out.
    #[test]
    fn silence_gives_silence_and_dc_gives_dc() {
        let mut st = ChannelSynth::new();
        let zeros = vec![0i32; 64];
        let bands: Vec<&[i32]> = (0..32).map(|_| &zeros[..]).collect();
        let mut out = vec![1f32; 64 * 32];
        qmf_synth(&mut st, &FIR_32_PERFECT, &bands, 64, &mut out);
        assert!(out.iter().all(|&x| x == 0.0));

        let dc = vec![1 << 20; 64];
        let bands: Vec<&[i32]> = (0..32).map(|b| if b == 0 { &dc[..] } else { &zeros[..] }).collect();
        qmf_synth(&mut st, &FIR_32_PERFECT, &bands, 64, &mut out);
        let tail = &out[40 * 32..];
        let mean = tail.iter().sum::<f32>() / tail.len() as f32;
        let dev = tail.iter().map(|&x| (x - mean).abs()).fold(0f32, f32::max);
        assert!(mean.abs() > 1e-3, "no DC came out: {mean}");
        assert!(dev < mean.abs() * 0.02, "ripple {dev} on {mean}");
    }

    /// One sample in one band is a finite burst: the response is not silent, and it has died away
    /// when the filter's 16 blocks of overlap have passed.
    #[test]
    fn an_impulse_has_a_finite_response() {
        for band in [0usize, 5, 31] {
            let mut st = ChannelSynth::new();
            let mut one = [0i32; 40];
            one[0] = 1 << 22;
            let zeros = [0i32; 40];
            let bands: Vec<&[i32]> = (0..32).map(|b| if b == band { &one[..] } else { &zeros[..] }).collect();
            let mut out = vec![0f32; 40 * 32];
            qmf_synth(&mut st, &FIR_32_PERFECT, &bands, 40, &mut out);
            let head: f64 = out[..17 * 32].iter().map(|&x| (x as f64).powi(2)).sum();
            assert!(head > 1e-6, "band {band}: no response ({head})");
            assert!(out[17 * 32..].iter().all(|&x| x == 0.0), "band {band}: the response does not end");
        }
    }
}
