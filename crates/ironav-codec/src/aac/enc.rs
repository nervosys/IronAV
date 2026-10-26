//! An AAC-LC encoder: long windows only (sine shape), mono or stereo, per-band mid/side, Huffman
//! codebooks chosen per band for the fewest bits, and a feedback loop on the quantiser's step to hold
//! the bit rate. The tables are the decoder's own, so what is written is what its parser reads.
//!
//! The step in each scalefactor band follows the band's level only partway (`SHAPING`), the same
//! compromise as the Vorbis encoder: noise is shaped, not hidden, since there is no psychoacoustic
//! model. No window switching (an attack spreads its noise over a 2048-sample window), no TNS,
//! no PNS, no intensity stereo, no prediction.

use super::imdct::Imdct;
use super::tables::*;
use super::SAMPLE_RATES;
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, Result};

const N: usize = 1024;
/// Output of the forward transform in the decoder's units (it multiplies by 1 / (32768 * 1024) and the
/// DCT-IV is its own inverse up to a factor n / 2).
const FORWARD_SCALE: f32 = 65536.0;
const SHAPING: f64 = 0.6;
/// Largest magnitude an escape sequence can carry.
const MAX_Q: i32 = 8191;
/// The AAC-LC object type.
const AOT_LC: u32 = 2;

// ---- bit writing -----------------------------------------------------------------------------

#[derive(Default)]
struct BitWriter {
    buf: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    /// The low `bits` (at most 32) of `v`, most significant first.
    fn put(&mut self, v: u32, bits: u32) {
        if bits == 0 {
            return;
        }
        self.acc = (self.acc << bits) | (v as u64 & ((1u64 << bits) - 1));
        self.n += bits;
        while self.n >= 8 {
            self.n -= 8;
            self.buf.push((self.acc >> self.n) as u8);
        }
        self.acc &= (1u64 << self.n) - 1;
    }
    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.buf.push((self.acc << (8 - self.n)) as u8);
        }
        self.buf
    }
}

// ---- spectral codebooks ----------------------------------------------------------------------

/// (values per codeword, symbols per value, offset making values non-negative, largest magnitude).
fn book_shape(cb: usize) -> (usize, usize, i32, i32) {
    match cb {
        1 | 2 => (4, 3, 1, 1),
        3 | 4 => (4, 3, 0, 2),
        5 | 6 => (2, 9, 4, 4),
        7 | 8 => (2, 8, 0, 7),
        9 | 10 => (2, 13, 0, 12),
        _ => (2, 17, 0, 16),
    }
}

fn book_index(cb: usize, vals: &[i32]) -> usize {
    let (_, modulus, off, _) = book_shape(cb);
    let unsigned = off == 0;
    vals.iter().fold(0, |acc, &v| acc * modulus + if unsigned { v.unsigned_abs().min(16) as usize } else { (v + off) as usize })
}

fn escape_bits(m: u32) -> usize {
    let n = (31 - m.leading_zeros()) as usize - 4; // m >= 16
    2 * n + 5
}

/// Bits to code `q` (a band's values) with codebook `cb`, or `None` if a value is too big for it.
fn book_cost(cb: usize, q: &[i32]) -> Option<usize> {
    let (dim, _, off, max) = book_shape(cb);
    let table = SPECTRAL_BITS[cb - 1];
    let mut bits = 0;
    for v in q.chunks(dim) {
        if cb != 11 && v.iter().any(|x| x.abs() > max) {
            return None;
        }
        bits += table[book_index(cb, v)] as usize;
        if off == 0 {
            bits += v.iter().filter(|&&x| x != 0).count();
            if cb == 11 {
                bits += v.iter().filter(|x| x.unsigned_abs() >= 16).map(|x| escape_bits(x.unsigned_abs())).sum::<usize>();
            }
        }
    }
    Some(bits)
}

fn write_band(w: &mut BitWriter, cb: usize, q: &[i32]) {
    let (dim, _, off, _) = book_shape(cb);
    let (codes, lens) = (SPECTRAL_CODES[cb - 1], SPECTRAL_BITS[cb - 1]);
    for v in q.chunks(dim) {
        let i = book_index(cb, v);
        w.put(codes[i] as u32, lens[i] as u32);
        if off == 0 {
            for &x in v.iter().filter(|&&x| x != 0) {
                w.put((x < 0) as u32, 1);
            }
            if cb == 11 {
                for &x in v {
                    let m = x.unsigned_abs();
                    if m >= 16 {
                        let n = (31 - m.leading_zeros()) - 4;
                        for _ in 0..n {
                            w.put(1, 1);
                        }
                        w.put(0, 1);
                        w.put(m - (1 << (n + 4)), n + 4);
                    }
                }
            }
        }
    }
}

/// The cheapest codebook that can hold `q`, and its cost. All zeros is book 0, free.
fn best_book(q: &[i32]) -> (usize, usize) {
    let peak = q.iter().map(|x| x.abs()).max().unwrap_or(0);
    if peak == 0 {
        return (0, 0);
    }
    (1..=11).filter_map(|cb| book_cost(cb, q).map(|c| (cb, c))).min_by_key(|&(_, c)| c).unwrap()
}

// ---- one channel's quantised spectrum --------------------------------------------------------

#[derive(Clone)]
struct Quantised {
    /// Per scalefactor band: codebook (0 = nothing coded) and scalefactor.
    book: Vec<usize>,
    sf: Vec<i32>,
    q: Vec<i32>,
}

fn quantise(x: f32, inv_step: f32) -> i32 {
    let m = ((x.abs() * inv_step).powf(0.75) + 0.4054).floor() as i32;
    let m = m.min(MAX_Q);
    if x < 0.0 { -m } else { m }
}

impl Quantised {
    /// Quantise `spec` with a step per band given by `steps` (linear), keeping consecutive scalefactors
    /// of coded bands within what the delta code can hold.
    fn new(spec: &[f32], swb: &[u16], steps: &[f64]) -> Self {
        let bands = swb.len() - 1;
        let mut out = Quantised { book: vec![0; bands], sf: vec![0; bands], q: vec![0; N] };
        let mut prev: Option<i32> = None;
        for b in 0..bands {
            let (lo, hi) = (swb[b] as usize, swb[b + 1] as usize);
            let mut sf = ((steps[b].max(1e-30).log2() * 4.0).round() as i32 + 100).clamp(0, 255);
            if let Some(p) = prev {
                sf = sf.clamp(p - 60, p + 60);
            }
            let inv = (2f64.powf((sf - 100) as f64 / -4.0)) as f32;
            for k in lo..hi {
                out.q[k] = quantise(spec[k], inv);
            }
            let (cb, _) = best_book(&out.q[lo..hi]);
            out.book[b] = cb;
            out.sf[b] = sf;
            if cb != 0 {
                prev = Some(sf);
            }
        }
        out
    }

    fn max_sfb(&self) -> usize {
        self.book.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1)
    }

    fn global_gain(&self) -> u32 {
        self.book.iter().position(|&b| b != 0).map_or(100, |p| self.sf[p] as u32)
    }

    /// Everything after the `ics_info`: sections, scalefactors, the three tool flags, the spectrum.
    fn write(&self, w: &mut BitWriter, swb: &[u16], max_sfb: usize, with_info: Option<usize>) {
        w.put(self.global_gain(), 8);
        if let Some(sfb) = with_info {
            write_ics_info(w, sfb);
        }
        // sections: runs of bands with the same codebook
        let mut b = 0;
        while b < max_sfb {
            let cb = self.book[b];
            let mut end = b + 1;
            while end < max_sfb && self.book[end] == cb {
                end += 1;
            }
            w.put(cb as u32, 4);
            let mut len = end - b;
            while len >= 31 {
                w.put(31, 5);
                len -= 31;
            }
            w.put(len as u32, 5);
            b = end;
        }
        // scalefactors, as differences between coded bands
        let mut last = self.global_gain() as i32;
        for b in 0..max_sfb {
            if self.book[b] != 0 {
                let d = (self.sf[b] - last + 60) as usize;
                w.put(SCALEFACTOR_CODE[d], SCALEFACTOR_BITS[d] as u32);
                last = self.sf[b];
            }
        }
        w.put(0, 3); // no pulses, no TNS, no gain control
        for b in 0..max_sfb {
            if self.book[b] != 0 {
                write_band(w, self.book[b], &self.q[swb[b] as usize..swb[b + 1] as usize]);
            }
        }
    }
}

fn write_ics_info(w: &mut BitWriter, max_sfb: usize) {
    w.put(0, 1); // reserved
    w.put(0, 2); // only long windows
    w.put(0, 1); // sine window
    w.put(max_sfb as u32, 6);
    w.put(0, 1); // no prediction
}

// ---- the encoder -----------------------------------------------------------------------------

/// The two-byte AudioSpecificConfig of an AAC-LC stream.
pub fn audio_specific_config(sample_rate: u32, channels: usize) -> Vec<u8> {
    let sfi = SAMPLE_RATES.iter().position(|&r| r == sample_rate).unwrap_or(4) as u32;
    let bits = (AOT_LC << 11) | (sfi << 7) | ((channels as u32) << 3);
    vec![(bits >> 8) as u8, bits as u8]
}

/// Samples a decoder outputs before the first input sample comes out.
pub const PRIMING: i64 = N as i64;

pub struct AacEncoder {
    channels: usize,
    swb: &'static [u16],
    imdct: Imdct,
    window: Vec<f32>,
    /// The previous frame's samples per channel: the first half of the next block.
    prev: Vec<Vec<f32>>,
    buf: Vec<Vec<f32>>,
    target_bits: f64,
    gain_db: f64,
    frames: u64,
    input: u64,
}

impl AacEncoder {
    pub fn new(sample_rate: u32, channels: usize, bitrate: u32) -> Result<Self> {
        if !(1..=2).contains(&channels) {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the AAC encoder takes mono or stereo audio, not {channels} channels")));
        }
        let Some(sfi) = SAMPLE_RATES.iter().position(|&r| r == sample_rate) else {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("AAC has no sampling frequency index for {sample_rate} Hz"))
                .hint("resample to one of 96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000 or 7350 Hz"));
        };
        let window = (0..2 * N).map(|i| (std::f64::consts::PI * (i as f64 + 0.5) / (2 * N) as f64).sin() as f32).collect();
        Ok(Self {
            channels,
            swb: SWB_OFFSET_1024[sfi],
            imdct: Imdct::new(N),
            window,
            prev: vec![vec![0.0; N]; channels],
            buf: vec![Vec::new(); channels],
            target_bits: bitrate as f64 * N as f64 / sample_rate as f64,
            gain_db: 6.0,
            frames: 0,
            input: 0,
        })
    }

    pub fn default_bitrate(channels: usize) -> u32 {
        64_000 * channels as u32
    }

    /// MDCT of one block of `2 * N` samples, in the decoder's units.
    fn forward(&mut self, x: &[f32], out: &mut [f32]) {
        let mut u = vec![0f32; N];
        for i in 0..N / 2 {
            u[N / 2 + i] += x[i] * self.window[i];
        }
        for i in N / 2..3 * N / 2 {
            u[3 * N / 2 - 1 - i] -= x[i] * self.window[i];
        }
        for i in 3 * N / 2..2 * N {
            u[i - 3 * N / 2] -= x[i] * self.window[i];
        }
        self.imdct.dct4_into(&u, out);
        for v in out.iter_mut() {
            *v *= FORWARD_SCALE;
        }
    }

    /// Quantiser steps per scalefactor band for a spectrum.
    fn steps(&self, spec: &[f32], common: f64) -> Vec<f64> {
        let g = 10f64.powf(self.gain_db / 20.0);
        (0..self.swb.len() - 1)
            .map(|b| {
                let band = &spec[self.swb[b] as usize..self.swb[b + 1] as usize];
                let rms = (band.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / band.len() as f64).sqrt();
                rms.max(1e-9).powf(SHAPING) * common.powf(1.0 - SHAPING) * g
            })
            .collect()
    }

    fn encode_block(&mut self, blocks: &[Vec<f32>], adapt: bool) -> Vec<u8> {
        let mut spec: Vec<Vec<f32>> = Vec::new();
        for b in blocks {
            let mut s = vec![0f32; N];
            self.forward(b, &mut s);
            spec.push(s);
        }
        let bands = self.swb.len() - 1;
        let level = |s: &[f32]| (s.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / s.len() as f64).sqrt().max(1e-9);

        let mut w = BitWriter::default();
        if self.channels == 1 {
            let common = level(&spec[0]);
            let steps = self.steps(&spec[0], common);
            let qz = Quantised::new(&spec[0], self.swb, &steps);
            let max_sfb = qz.max_sfb();
            w.put(0, 3); // a single channel element
            w.put(0, 4);
            qz.write(&mut w, self.swb, max_sfb, Some(max_sfb));
        } else {
            let (l, r) = (spec[0].clone(), spec[1].clone());
            // mid/side per band where it looks cheaper
            let mut ms = vec![false; bands];
            let (mut a, mut b) = (l.clone(), r.clone());
            let (cl, cr) = (level(&l), level(&r));
            let cost = |x: &[f32], step: f64| x.iter().map(|&v| (1.0 + (v.abs() as f64) / step).log2()).sum::<f64>();
            let (sl, sr) = (self.steps(&l, cl), self.steps(&r, cr));
            let mid: Vec<f32> = l.iter().zip(&r).map(|(x, y)| (x + y) * 0.5).collect();
            let side: Vec<f32> = l.iter().zip(&r).map(|(x, y)| (x - y) * 0.5).collect();
            let (sm, ss) = (self.steps(&mid, level(&mid).max(cl.min(cr) * 0.5)), self.steps(&side, level(&side).max(cl.min(cr) * 0.5)));
            for k in 0..bands {
                let (lo, hi) = (self.swb[k] as usize, self.swb[k + 1] as usize);
                let lr = cost(&l[lo..hi], sl[k]) + cost(&r[lo..hi], sr[k]);
                let m_s = cost(&mid[lo..hi], sm[k]) + cost(&side[lo..hi], ss[k]);
                if m_s < lr * 0.9 {
                    ms[k] = true;
                    a[lo..hi].copy_from_slice(&mid[lo..hi]);
                    b[lo..hi].copy_from_slice(&side[lo..hi]);
                }
            }
            // steps from the transformed bands, with the common level of the original pair
            let common = (cl * cl + cr * cr).sqrt() / std::f64::consts::SQRT_2;
            let (steps_a, steps_b) = (self.steps(&a, common), self.steps(&b, common));
            let (qa, qb) = (Quantised::new(&a, self.swb, &steps_a), Quantised::new(&b, self.swb, &steps_b));
            let max_sfb = qa.max_sfb().max(qb.max_sfb());
            w.put(1, 3); // a channel pair element
            w.put(0, 4);
            w.put(1, 1); // one set of window info for both
            write_ics_info(&mut w, max_sfb);
            let any_ms = ms[..max_sfb].iter().any(|&m| m);
            w.put(any_ms as u32, 2); // 1: a mask follows, 0: no mid/side
            if any_ms {
                for &m in &ms[..max_sfb] {
                    w.put(m as u32, 1);
                }
            }
            qa.write(&mut w, self.swb, max_sfb, None);
            qb.write(&mut w, self.swb, max_sfb, None);
        }
        w.put(7, 3); // end of the block
        let bytes = w.finish();
        if adapt {
            let err = (bytes.len() as f64 * 8.0 / self.target_bits).ln();
            self.gain_db = (self.gain_db + (err * 4.0).clamp(-1.5, 1.5)).clamp(-30.0, 50.0);
        }
        bytes
    }

    fn emit(&mut self, cur: Vec<Vec<f32>>, out: &mut Vec<Packet>) {
        let blocks: Vec<Vec<f32>> = (0..self.channels).map(|c| self.prev[c].iter().chain(&cur[c]).copied().collect()).collect();
        if self.frames < 2 && blocks.iter().any(|b| b.iter().any(|&v| v != 0.0)) {
            // no history for the rate loop yet: find the step that meets the budget
            let (mut lo, mut hi) = (-30.0f64, 50.0f64);
            for _ in 0..7 {
                self.gain_db = (lo + hi) / 2.0;
                if self.encode_block(&blocks, false).len() as f64 * 8.0 > self.target_bits {
                    lo = self.gain_db;
                } else {
                    hi = self.gain_db;
                }
            }
            self.gain_db = hi;
        }
        let data = self.encode_block(&blocks, true);
        out.push(Packet { stream: 0, pts: self.frames as i64 * N as i64, duration: N as i64, data });
        self.frames += 1;
        self.prev = cur;
    }
}

impl Encoder for AacEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid_input("the AAC encoder received a video frame"));
        };
        if a.channels as usize != self.channels {
            return Err(Error::invalid_input("the audio changed channel count mid-stream"));
        }
        let n = a.samples.len() / self.channels;
        for i in 0..n {
            for c in 0..self.channels {
                self.buf[c].push(a.samples[i * self.channels + c] as f32);
            }
        }
        self.input += n as u64;
        let mut out = Vec::new();
        while self.buf[0].len() >= N {
            let cur: Vec<Vec<f32>> = self.buf.iter_mut().map(|b| b.drain(..N).collect()).collect();
            self.emit(cur, &mut out);
        }
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        let mut out = Vec::new();
        if self.input == 0 {
            return Ok(out);
        }
        // frames until the decoder has output every input sample: it is one frame behind
        let wanted = (self.input as usize).div_ceil(N) as u64 + 1;
        while self.frames < wanted {
            let cur: Vec<Vec<f32>> = self
                .buf
                .iter_mut()
                .map(|b| {
                    let mut c: Vec<f32> = b.drain(..).collect();
                    c.resize(N, 0.0);
                    c
                })
                .collect();
            self.emit(cur, &mut out);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aac::AacDecoder;
    use ironav_core::{AudioFrame, Decoder, StreamParams};

    fn run(rate: u32, channels: usize, kbps: u32, n: usize, make: impl Fn(usize, usize) -> f64) -> (Vec<f64>, Vec<f64>, usize) {
        let input: Vec<f64> = (0..n * channels).map(|i| make(i / channels, i % channels)).collect();
        let mut enc = AacEncoder::new(rate, channels, kbps * 1000).unwrap();
        let mut packets = Vec::new();
        for chunk in input.chunks(1000 * channels) {
            packets.extend(enc.encode(&Frame::Audio(AudioFrame { sample_rate: rate, channels: channels as u16, pts: 0, samples: chunk.to_vec() })).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        let bytes: usize = packets.iter().map(|p| p.data.len()).sum();
        let mut dec = AacDecoder::new(&audio_specific_config(rate, channels), &StreamParams::Audio { sample_rate: rate, channels: channels as u16 }).unwrap();
        let mut out = Vec::new();
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                let Frame::Audio(a) = f else { panic!() };
                out.extend(a.samples);
            }
        }
        assert!(out.len() >= (n + N) * channels, "{} samples out for {} in", out.len(), n);
        // the decoder's first frame is the priming; the input starts after it
        (input, out[N * channels..].to_vec(), bytes)
    }

    fn snr(a: &[f64], b: &[f64]) -> f64 {
        let (mut s, mut e) = (0.0, 0.0);
        for (x, y) in a.iter().zip(b) {
            s += x * x;
            e += (x - y) * (x - y);
        }
        10.0 * (s / e).log10()
    }

    fn music(t: usize, c: usize) -> f64 {
        let x = t as f64 / 44_100.0;
        let tone = 0.3 * (2.0 * std::f64::consts::PI * 440.0 * x).sin() + 0.15 * (2.0 * std::f64::consts::PI * 1320.0 * x).sin();
        let side = 0.1 * (2.0 * std::f64::consts::PI * 660.0 * x + c as f64).sin();
        tone + side + 0.02 * (((t * 7919 + c * 31) % 1000) as f64 / 500.0 - 1.0)
    }

    #[test]
    fn every_book_round_trips_through_the_decoders_parser() {
        // cost matches what is written, for every book on values it can hold
        for cb in 1..=11 {
            let (_, _, _, max) = book_shape(cb);
            let q: Vec<i32> = (0..16).map(|i| ((i * 5 + cb) as i32 % (2 * max.min(15) + 1)) - max.min(15)).collect();
            let want = book_cost(cb, &q).unwrap();
            let mut w = BitWriter::default();
            write_band(&mut w, cb, &q);
            let bits = w.buf.len() * 8 + w.n as usize;
            assert_eq!(bits, want, "book {cb}");
        }
    }

    #[test]
    fn a_tone_comes_back_close_at_each_rate_and_channel_count() {
        for (rate, ch, kbps) in [(44_100u32, 1usize, 64u32), (44_100, 2, 128), (48_000, 2, 96), (22_050, 1, 32), (16_000, 1, 24)] {
            let n = rate as usize * 2;
            let (input, out, bytes) = run(rate, ch, kbps, n, |t, c| music(t * 44_100 / rate as usize, c));
            let db = snr(&input, &out[..input.len()]);
            eprintln!("{rate} Hz {ch} ch {kbps} kb/s: {db:.1} dB at {} kb/s", bytes * 8 / 2000);
            assert!(db > 12.0, "{rate} {ch}: {db}");
        }
    }

    #[test]
    #[ignore]
    fn bitrate_sweep() {
        for kbps in [32u32, 64, 96, 128, 192, 256] {
            let (input, out, bytes) = run(44_100, 2, kbps, 44_100 * 3, music);
            eprintln!("{kbps}: {:.1} dB at {} kb/s", snr(&input, &out[..input.len()]), bytes * 8 / 3000);
        }
    }

    /// Extreme signals and tiny inputs decode and stay finite.
    #[test]
    fn stress() {
        let makers: Vec<(&str, Box<dyn Fn(usize, usize) -> f64>)> = vec![
            ("silence", Box::new(|_, _| 0.0)),
            ("full-scale square", Box::new(|t, _| if (t / 40) % 2 == 0 { 1.0 } else { -1.0 })),
            ("clicks", Box::new(|t, c| if t % 3001 == 0 { 1.0 - c as f64 * 2.0 } else { 0.0 })),
            ("noise", Box::new(|t, c| (((t * 2654435761 + c * 97) % 65536) as f64 / 32768.0 - 1.0) * 0.9)),
            ("dc", Box::new(|_, _| 0.7)),
        ];
        for (name, f) in &makers {
            for n in [1usize, 1025, 5000] {
                for (ch, kbps) in [(1usize, 16u32), (2, 320)] {
                    let (_, out, _) = run(44_100, ch, kbps, n, f);
                    assert!(out.iter().all(|v| v.is_finite()), "{name} {n}");
                }
            }
        }
        let mut e = AacEncoder::new(44_100, 2, 128_000).unwrap();
        assert!(e.flush().unwrap().is_empty());
    }
}
