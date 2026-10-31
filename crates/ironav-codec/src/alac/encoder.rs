//! ALAC encoder. A block of up to 4096 samples is one frame. A stereo pair is coded as left and
//! right, left and side, or mid and side, whichever has the smallest first differences. Each channel
//! runs through a second-order adaptive predictor, and its residuals through the adaptive Rice
//! coder. A frame that would not come out smaller than its samples is written uncompressed.

use super::{lpc_adapt, lpc_base, rice_k, run_k, sign_extend, update_history, zigzag, CHANNEL_OFFSETS, FRAME_LENGTH, INITIAL_HISTORY, LIMIT, MULT};
use crate::flac::bits::BitWriter;
use ironav_core::{Encoder, Error, Frame, Packet, Result};

/// The predictor: second order, with the coefficients of 2x[i-1] - x[i-2] at quantizer 2^4.
const ORDER: usize = 2;
const QUANT: u32 = 4;
const INITIAL: [i16; ORDER] = [-16, 32];
/// Multiplier field of each channel's header: 4 times the configured 40, over 4, is the Rice multiplier.
const FRAME_MULT: u32 = 4;
const END: u64 = 7;
const SCE: u64 = 0;
const CPE: u64 = 1;
/// The elements of a frame by channel count, as FFmpeg writes them: the LFE is a single channel element.
const CHANNEL_ELEMENTS: [&[u64]; 8] = [&[SCE], &[CPE], &[SCE, CPE], &[SCE, CPE, SCE], &[SCE, CPE, CPE], &[SCE, CPE, CPE, SCE], &[SCE, CPE, CPE, SCE, SCE], &[SCE, CPE, CPE, CPE, SCE]];

/// Rice-codes one value: `x` as a unary prefix of ones and a zero, then `k` bits of remainder; a
/// prefix of 9 ones is an escape, followed by `bps` raw bits.
fn write_scalar(w: &mut BitWriter, x: u32, k: u32, bps: u32) {
    let div = (1u32 << k) - 1;
    let (q, r) = (x / div, x % div);
    if q > 8 {
        w.write(0x1ff, 9);
        w.write(x as u64, bps);
        return;
    }
    w.write((1u64 << q) - 1, q);
    w.write(0, 1);
    if k != 1 {
        if r > 0 {
            w.write((r + 1) as u64, k);
        } else {
            w.write(0, k - 1);
        }
    }
}

/// The residuals of one channel in Rice codes. After a low-history sample, the zero run that
/// follows is coded (possibly empty), and the sample after it is coded one lower, since it cannot be zero.
fn write_residuals(w: &mut BitWriter, res: &[i32], bps: u32) {
    let n = res.len();
    let mut history = INITIAL_HISTORY;
    let mut bump = 0u32;
    let mut i = 0;
    while i < n {
        let u = zigzag(res[i]);
        write_scalar(w, u.wrapping_sub(bump), rice_k(history, LIMIT), bps);
        history = update_history(history, u, MULT);
        bump = 0;
        if history < 128 && i + 1 < n {
            let run = res[i + 1..].iter().take_while(|&&v| v == 0).count();
            write_scalar(w, run as u32, run_k(history, LIMIT), 16);
            i += run;
            bump = 1;
            history = 0;
        }
        i += 1;
    }
}

/// The prediction residuals of one channel, as the decoder's predictor inverts them.
fn residuals(x: &[i32], bps: u32) -> Vec<i32> {
    let n = x.len();
    let mut e = vec![0i32; n];
    let mut coefs = INITIAL;
    if n == 0 {
        return e;
    }
    e[0] = x[0];
    for i in 1..n.min(ORDER + 1) {
        e[i] = sign_extend(x[i].wrapping_sub(x[i - 1]), bps);
    }
    for i in ORDER + 1..n {
        e[i] = sign_extend(x[i].wrapping_sub(lpc_base(x, i, &coefs, QUANT)), bps);
        lpc_adapt(x, i, &mut coefs, QUANT, e[i]);
    }
    e
}

/// Mid-side and the other stereo pairs: the two coded channels and the decorrelation (shift, weight)
/// the decoder must undo. Chosen by the sum of first differences, which tracks the Rice cost.
fn stereo(l: &[i32], r: &[i32]) -> ((u32, u32), [Vec<i32>; 2]) {
    let side: Vec<i32> = l.iter().zip(r).map(|(a, b)| a - b).collect();
    let mid: Vec<i32> = r.iter().zip(&side).map(|(b, s)| b + (s >> 1)).collect();
    let cost = |c: &[Vec<i32>; 2]| -> i64 { c.iter().map(|ch| ch.windows(2).map(|p| (p[1] as i64 - p[0] as i64).abs()).sum::<i64>()).sum() };
    let candidates = [((0, 0), [l.to_vec(), r.to_vec()]), ((0, 1), [l.to_vec(), side.clone()]), ((1, 1), [mid, side])];
    candidates.into_iter().min_by_key(|(_, c)| cost(c)).unwrap()
}

fn header(w: &mut BitWriter, tag: u64, instance: u64, n: usize, compressed: bool, low: u32) {
    w.write(tag, 3);
    w.write(instance, 4); // element instance tag
    w.write(0, 12); // unused
    let has_size = n as u32 != FRAME_LENGTH;
    w.write(has_size as u64, 1);
    w.write((low / 8) as u64, 2); // bytes of each sample coded raw after the predictor's table
    w.write(!compressed as u64, 1);
    if has_size {
        w.write(n as u64, 32);
    }
}

/// One element of a compressed frame: its stereo mode, the predictor and the Rice coded residuals.
fn compressed_element(w: &mut BitWriter, group: &[&Vec<i32>], tag: u64, instance: u64, bits: u32, low: u32) {
    let n = group[0].len();
    let high: Vec<Vec<i32>> = group.iter().map(|c| c.iter().map(|&s| s >> low).collect()).collect();
    let bps = bits - low + group.len() as u32 - 1;
    let ((shift, weight), coded): ((u32, u32), Vec<Vec<i32>>) = match high.as_slice() {
        [l, r] => {
            let ((s, w), c) = stereo(l, r);
            ((s, w), c.to_vec())
        }
        [m] => ((0, 0), vec![m.clone()]),
        _ => unreachable!("an ALAC element holds one or two channels"),
    };

    header(w, tag, instance, n, true, low);
    w.write(shift as u64, 8);
    w.write(weight as u64, 8);
    for _ in &coded {
        w.write(0, 4); // prediction type
        w.write(QUANT as u64, 4);
        w.write(FRAME_MULT as u64, 3);
        w.write(ORDER as u64, 5);
        for &c in INITIAL.iter().rev() {
            w.write_signed(c as i64, 16);
        }
    }
    if low > 0 {
        let mask = (1u32 << low) - 1;
        for i in 0..n {
            for ch in group {
                w.write((ch[i] as u32 & mask) as u64, low);
            }
        }
    }
    for c in &coded {
        write_residuals(w, &residuals(c, bps), bps);
    }
}

/// One element of an uncompressed frame: its samples as they are.
fn raw_element(w: &mut BitWriter, group: &[&Vec<i32>], tag: u64, instance: u64, bits: u32) {
    let n = group[0].len();
    header(w, tag, instance, n, false, 0);
    for i in 0..n {
        for ch in group {
            w.write_signed(ch[i] as i64, bits);
        }
    }
}

/// One frame's bytes: compressed when that is smaller, uncompressed otherwise.
fn encode_frame(chans: &[Vec<i32>], bits: u32) -> Vec<u8> {
    // Above 16 bits the low byte goes raw and the predictor codes the rest: the Rice history saturates
    // at 16 bits, so full-width residuals of a 24-bit signal would mostly escape. FFmpeg's encoder does the same.
    let low = bits.saturating_sub(16);
    // the channels in element order, as the layout's elements take them
    let order: Vec<&Vec<i32>> = CHANNEL_OFFSETS[chans.len() - 1].iter().map(|&o| &chans[o]).collect();
    let (mut compressed, mut raw) = (BitWriter::new(), BitWriter::new());
    let (mut at, mut sce, mut cpe): (usize, u64, u64) = (0, 0, 0);
    for &el in CHANNEL_ELEMENTS[chans.len() - 1] {
        let width = if el == CPE { 2 } else { 1 };
        let group = &order[at..at + width];
        at += width;
        let instance = if el == CPE {
            cpe += 1;
            cpe - 1
        } else {
            sce += 1;
            sce - 1
        };
        compressed_element(&mut compressed, group, el, instance, bits, low);
        raw_element(&mut raw, group, el, instance, bits);
    }
    compressed.write(END, 3);
    compressed.align();
    raw.write(END, 3);
    raw.align();

    if compressed.bytes.len() < raw.bytes.len() {
        compressed.bytes
    } else {
        raw.bytes
    }
}

/// Packet-level encoder: each packet is one ALAC frame.
pub struct AlacEncoder {
    channels: usize,
    bits: u32,
    pending: Vec<Vec<i32>>,
    pts: i64,
}

impl AlacEncoder {
    pub fn new(sample_rate: u32, channels: u16, bits: Option<u8>) -> Result<Self> {
        let bits = bits.unwrap_or(16) as u32;
        if bits != 16 && bits != 24 {
            return Err(Error::invalid_arg(format!("the ALAC encoder supports 16 and 24 bits per sample, not {bits}")));
        }
        if !(1..=8).contains(&channels) {
            return Err(Error::invalid_arg("the ALAC encoder writes one to eight channels"));
        }
        if sample_rate == 0 {
            return Err(Error::invalid_arg("ALAC needs a sample rate"));
        }
        Ok(Self { channels: channels as usize, bits, pending: vec![Vec::new(); channels as usize], pts: 0 })
    }

    fn block(&mut self, n: usize) -> Packet {
        let chans: Vec<Vec<i32>> = self.pending.iter_mut().map(|c| c.drain(..n).collect()).collect();
        let data = encode_frame(&chans, self.bits);
        let pts = self.pts;
        self.pts += n as i64;
        Packet { stream: 0, pts, duration: n as i64, data }
    }
}

impl Encoder for AlacEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid_input("ALAC encoder received a video frame"));
        };
        if a.channels as usize != self.channels {
            return Err(Error::invalid_input("audio frame channel count changed mid-stream"));
        }
        let scale = (1i64 << (self.bits - 1)) as f64;
        for (c, pending) in self.pending.iter_mut().enumerate() {
            pending.extend(a.samples.iter().skip(c).step_by(self.channels).map(|&s| (s * scale).round().clamp(-scale, scale - 1.0) as i32));
        }
        let mut out = Vec::new();
        while self.pending[0].len() >= FRAME_LENGTH as usize {
            out.push(self.block(FRAME_LENGTH as usize));
        }
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        let n = self.pending[0].len();
        Ok(if n > 0 { vec![self.block(n)] } else { Vec::new() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alac::decoder::decode_frame;
    use crate::alac::{parse_config, specific_config, Config};
    use ironav_core::{AudioFrame, Decoder, StreamParams};

    fn cfg(channels: u8, bits: u8) -> Config {
        parse_config(&specific_config(44_100, channels, bits)).unwrap()
    }

    /// Samples in [-1, 1) at `bits`, a tone with noise of about `noise` (fraction of full scale).
    fn signal(n: usize, ch: usize, bits: u32, noise: f64, seed: u64) -> Vec<f64> {
        let scale = (1i64 << (bits - 1)) as f64;
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut out = Vec::with_capacity(n * ch);
        for i in 0..n {
            for c in 0..ch {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let r = ((x >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 2.0 * noise;
                let tone = (i as f64 * (0.021 + 0.007 * c as f64)).sin() * 0.7 + (i as f64 * 0.13).sin() * 0.2;
                out.push(((tone + r) * scale).round().clamp(-scale, scale - 1.0) / scale);
            }
        }
        out
    }

    /// Encode `samples` in frames and decode every frame again: (decoded samples, packet bytes, frame sizes).
    fn roundtrip(samples: &[f64], ch: u16, bits: u8) -> (Vec<f64>, usize, Vec<usize>) {
        let mut enc = AlacEncoder::new(44_100, ch, Some(bits)).unwrap();
        let mut pkts = enc.encode(&Frame::Audio(AudioFrame { sample_rate: 44_100, channels: ch, pts: 0, samples: samples.to_vec() })).unwrap();
        pkts.extend(enc.flush().unwrap());
        let c = cfg(ch as u8, bits);
        let mut got = Vec::new();
        let mut bytes = 0;
        let mut sizes = Vec::new();
        for p in &pkts {
            bytes += p.data.len();
            sizes.push(p.data.len());
            let chans = decode_frame(&p.data, &c).unwrap();
            assert_eq!(chans.len(), ch as usize);
            let scale = (1i64 << (bits - 1)) as f64;
            for i in 0..chans[0].len() {
                for chan in &chans {
                    got.push(chan[i] as f64 / scale);
                }
            }
        }
        (got, bytes, sizes)
    }

    #[test]
    fn lossless_across_layouts_depths_and_lengths() {
        for &(n, ch, bits) in &[(10_000, 1, 16), (10_000, 2, 16), (9_999, 2, 24), (4096, 1, 24), (4097, 2, 24), (1, 1, 16), (1, 2, 24), (2, 2, 16), (37, 2, 24), (8192, 2, 16)] {
            let samples = signal(n, ch as usize, bits as u32, 0.01, n as u64 + ch as u64);
            let (got, _, _) = roundtrip(&samples, ch, bits);
            assert_eq!(got.len(), samples.len(), "sample count for n={n} ch={ch} bits={bits}");
            assert!(got == samples, "lossless mismatch for n={n} ch={ch} bits={bits}");
        }
    }

    #[test]
    fn up_to_eight_channels_are_lossless_in_the_layout_elements() {
        for ch in 3..=8u16 {
            for bits in [16u8, 24] {
                let samples = signal(5000, ch as usize, bits as u32, 0.01, ch as u64);
                let (got, _, _) = roundtrip(&samples, ch, bits);
                assert!(got == samples, "lossless mismatch for ch={ch} bits={bits}");
            }
        }
        // full-scale noise: the raw fallback holds every element
        let noise = signal(4096, 6, 16, 1.0, 3);
        assert!(roundtrip(&noise, 6, 16).0 == noise);
    }

    #[test]
    fn compressible_signals_shrink_and_noise_falls_back_to_uncompressed() {
        let tone = signal(8192, 2, 16, 0.0, 1);
        let (got, bytes, _) = roundtrip(&tone, 2, 16);
        assert!(got == tone);
        assert!(bytes < 8192 * 2 * 2 / 2, "tone should halve at least: {bytes}");

        // white noise at full scale does not compress; each frame is then the raw samples plus a header
        let noise: Vec<f64> = signal(4096, 2, 16, 1.0, 9);
        let (got, bytes, sizes) = roundtrip(&noise, 2, 16);
        assert!(got == noise);
        assert!(sizes.iter().all(|&s| s <= 4096 * 2 * 2 + 8), "{sizes:?}");
        assert!(bytes >= 4096 * 2 * 2, "{bytes}");
    }

    #[test]
    fn edge_values_and_long_zero_runs_are_lossless() {
        // full-scale square waves exercise the escape codes; silence, the zero runs
        let square: Vec<f64> = (0..5000).map(|i| if (i / 3) % 2 == 0 { 32767.0 / 32768.0 } else { -1.0 }).collect();
        let (got, _, _) = roundtrip(&square, 1, 16);
        assert!(got == square);
        let mut sparse = vec![0.0; 9000];
        for k in (0..9000).step_by(997) {
            sparse[k] = 0.25 * ((k % 7) as f64 - 3.0) / 8.0;
        }
        let (got, _, _) = roundtrip(&sparse, 1, 24);
        assert!(got == sparse);
        let (got, _, _) = roundtrip(&vec![0.0; 12000], 2, 16);
        assert!(got.iter().all(|&v| v == 0.0) && got.len() == 12000);
    }

    #[test]
    fn stereo_pairs_pick_a_mode_that_decodes_exactly() {
        // correlated (near mono), anti-correlated, and independent channels each pick a different mode
        let mut x = vec![0.0; 2 * 4096];
        let base = signal(4096, 1, 16, 0.001, 3);
        for i in 0..4096 {
            x[2 * i] = base[i];
            x[2 * i + 1] = (base[i] * 0.999 * 32768.0).round() / 32768.0;
        }
        let (got, _, _) = roundtrip(&x, 2, 16);
        assert!(got == x);
        let other = signal(4096, 1, 16, 0.05, 5);
        for i in 0..4096 {
            x[2 * i + 1] = ((-base[i] + other[i] * 0.1) * 32768.0).round() / 32768.0;
        }
        let (got, _, _) = roundtrip(&x, 2, 16);
        assert!(got == x);
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_a_panic() {
        let s = signal(4096, 2, 16, 0.01, 2);
        let mut enc = AlacEncoder::new(44_100, 2, Some(16)).unwrap();
        let mut pk = enc.encode(&Frame::Audio(AudioFrame { sample_rate: 44_100, channels: 2, pts: 0, samples: s })).unwrap();
        let data = pk.remove(0).data;
        for cut in [0, 1, 5, data.len() / 2, data.len() - 1] {
            assert!(decode_frame(&data[..cut], &cfg(2, 16)).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn the_decoder_path_matches_the_stream_api() {
        let s = signal(5000, 2, 24, 0.01, 4);
        let mut enc = AlacEncoder::new(48_000, 2, Some(24)).unwrap();
        let mut pk = enc.encode(&Frame::Audio(AudioFrame { sample_rate: 48_000, channels: 2, pts: 0, samples: s.clone() })).unwrap();
        pk.extend(enc.flush().unwrap());
        let params = StreamParams::Audio { sample_rate: 48_000, channels: 2 };
        let mut dec = crate::alac::AlacDecoder::new(&specific_config(48_000, 2, 24), &params).unwrap();
        let mut got = Vec::new();
        for p in &pk {
            let Frame::Audio(a) = dec.decode(p).unwrap().remove(0) else { panic!() };
            got.extend(a.samples);
        }
        assert!(got == s);
        assert_eq!(pk.iter().map(|p| p.duration).sum::<i64>(), 5000);
    }

    #[test]
    fn the_frame_multiplier_gives_the_configured_rice_multiplier() {
        assert_eq!(crate::alac::frame_mult(FRAME_MULT, 40), MULT);
    }
}
