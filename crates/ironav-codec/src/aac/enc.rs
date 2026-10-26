//! An AAC-LC encoder: long windows only (sine shape), mono or stereo, per-band mid/side, Huffman
//! codebooks chosen per band for the fewest bits, and a feedback loop on the quantiser's step to hold
//! the bit rate. The tables are the decoder's own, so what is written is what its parser reads.
//! Attacks switch a block to eight short windows (with start and stop windows around it), one frame
//! of lookahead telling the previous block to prepare.
//!
//! The step in each scalefactor band follows the band's level only partway (`SHAPING`), the same
//! compromise as the Vorbis encoder: noise is shaped, not hidden, since there is no psychoacoustic
//! model. Short blocks put each of their eight windows in a group of its own. No TNS, no PNS, no
//! intensity stereo, no prediction.

use super::imdct::Imdct;
use super::tables::*;
use super::SAMPLE_RATES;
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, Result};
use std::collections::VecDeque;

const N: usize = 1024;
/// Output of the forward transform in the decoder's units (it multiplies by 1 / (32768 * 1024) and the
/// DCT-IV is its own inverse up to a factor n / 2).
const FORWARD_SCALE: f32 = 65536.0;
const SHAPING: f64 = 0.6;
/// Largest magnitude an escape sequence can carry.
const MAX_Q: i32 = 8191;
/// The AAC-LC object type.
const AOT_LC: u32 = 2;
const SHORT: usize = 128;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Long,
    Start,
    Short,
    Stop,
}

impl Kind {
    fn sequence(self) -> u32 {
        match self {
            Kind::Long => 0,
            Kind::Start => 1,
            Kind::Short => 2,
            Kind::Stop => 3,
        }
    }
}

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

    /// Last coded band plus one, over every group of `groups` equal runs of bands.
    fn max_sfb(&self, groups: usize) -> usize {
        let per = self.book.len() / groups;
        self.book.chunks(per).map(|g| g.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1)).max().unwrap_or(0)
    }

    fn global_gain(&self) -> u32 {
        self.book.iter().position(|&b| b != 0).map_or(100, |p| self.sf[p] as u32)
    }

    /// Everything after the `ics_info`: sections, scalefactors, the three tool flags, the spectrum.
    fn write(&self, w: &mut BitWriter, swb: &[u16], max_sfb: usize, groups: usize, with_info: Option<Kind>) {
        w.put(self.global_gain(), 8);
        if let Some(kind) = with_info {
            write_ics_info(w, max_sfb, kind);
        }
        let per = self.book.len() / groups;
        let (len_bits, esc) = if groups > 1 { (3, 7) } else { (5, 31) };
        // sections: runs of bands with the same codebook, per group
        for g in 0..groups {
            let book = &self.book[g * per..g * per + max_sfb];
            let mut b = 0;
            while b < max_sfb {
                let cb = book[b];
                let mut end = b + 1;
                while end < max_sfb && book[end] == cb {
                    end += 1;
                }
                w.put(cb as u32, 4);
                let mut len = end - b;
                while len >= esc {
                    w.put(esc as u32, len_bits);
                    len -= esc;
                }
                w.put(len as u32, len_bits);
                b = end;
            }
        }
        // scalefactors, as differences between coded bands
        let mut last = self.global_gain() as i32;
        for g in 0..groups {
            for b in g * per..g * per + max_sfb {
                if self.book[b] != 0 {
                    let d = (self.sf[b] - last + 60) as usize;
                    w.put(SCALEFACTOR_CODE[d], SCALEFACTOR_BITS[d] as u32);
                    last = self.sf[b];
                }
            }
        }
        w.put(0, 3); // no pulses, no TNS, no gain control
        for g in 0..groups {
            for b in g * per..g * per + max_sfb {
                if self.book[b] != 0 {
                    write_band(w, self.book[b], &self.q[swb[b] as usize..swb[b + 1] as usize]);
                }
            }
        }
    }
}

fn write_ics_info(w: &mut BitWriter, max_sfb: usize, kind: Kind) {
    w.put(0, 1); // reserved
    w.put(kind.sequence(), 2);
    w.put(0, 1); // sine window
    if kind == Kind::Short {
        w.put(max_sfb as u32, 4);
        w.put(0, 7); // every window is its own group
    } else {
        w.put(max_sfb as u32, 6);
        w.put(0, 1); // no prediction
    }
}

/// MDCT of `2 * n` windowed samples into `n` coefficients, in the decoder's units.
fn mdct(imdct: &mut Imdct, n: usize, window: &[f32], x: &[f32], out: &mut [f32]) {
    let mut u = vec![0f32; n];
    for i in 0..n / 2 {
        u[n / 2 + i] += x[i] * window[i];
    }
    for i in n / 2..3 * n / 2 {
        u[3 * n / 2 - 1 - i] -= x[i] * window[i];
    }
    for i in 3 * n / 2..2 * n {
        u[i - 3 * n / 2] -= x[i] * window[i];
    }
    imdct.dct4_into(&u, out);
    for v in out.iter_mut() {
        *v *= FORWARD_SCALE;
    }
}

/// Whether a block of `2 * N` samples holds an attack in the stretch eight short windows would cover:
/// a 128-sample piece of its first difference with much more energy than the pieces just before it.
fn has_attack(x: &[f32]) -> bool {
    let e: Vec<f64> = x.chunks(SHORT).map(|c| c.windows(2).map(|p| ((p[1] - p[0]) as f64).powi(2)).sum::<f64>() / c.len() as f64).collect();
    (4..12).any(|k| e[k] > 1e-5 && e[k] > 8.0 * e[k.saturating_sub(4)..k].iter().sum::<f64>() / 4.0 + 1e-7)
}

struct Pending {
    /// `2 * N` samples per channel.
    blocks: Vec<Vec<f32>>,
    attack: bool,
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
    /// The short scalefactor bands of all eight windows laid end to end, one group per window.
    swb_short: Vec<u16>,
    imdct: Imdct,
    imdct_short: Imdct,
    /// Long-block windows by kind, and the short window.
    win_long: Vec<f32>,
    win_start: Vec<f32>,
    win_stop: Vec<f32>,
    win_short: Vec<f32>,
    queue: VecDeque<Pending>,
    prev_short: bool,
    pushed: u64,
    switching: bool,
    short_blocks: u64,
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
        let sine = |i: usize, len: usize| (std::f64::consts::PI * (i as f64 + 0.5) / len as f64).sin() as f32;
        let win_long: Vec<f32> = (0..2 * N).map(|i| sine(i, 2 * N)).collect();
        let win_short: Vec<f32> = (0..2 * SHORT).map(|i| sine(i, 2 * SHORT)).collect();
        let mut win_start = win_long.clone();
        let mut win_stop = win_long.clone();
        // start: ones to 1472, the short fall to 1600, then zeros
        win_start[1024..1472].fill(1.0);
        win_start[1472..1600].copy_from_slice(&win_short[SHORT..]);
        win_start[1600..].fill(0.0);
        // stop: zeros to 448, the short rise to 576, then ones
        win_stop[..448].fill(0.0);
        win_stop[448..576].copy_from_slice(&win_short[..SHORT]);
        win_stop[576..1024].fill(1.0);
        let short_offsets = SWB_OFFSET_128[sfi];
        let mut swb_short = Vec::new();
        for g in 0..8 {
            swb_short.extend(short_offsets[..short_offsets.len() - 1].iter().map(|&o| (g * SHORT) as u16 + o));
        }
        swb_short.push(N as u16);
        Ok(Self {
            channels,
            swb: SWB_OFFSET_1024[sfi],
            swb_short,
            imdct: Imdct::new(N),
            imdct_short: Imdct::new(SHORT),
            win_long,
            win_start,
            win_stop,
            win_short,
            queue: VecDeque::new(),
            prev_short: false,
            pushed: 0,
            switching: true,
            short_blocks: 0,
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

    /// Spectrum of one block for the given window kind: 1024 coefficients, for short blocks the eight
    /// windows' 128 one after another.
    fn forward(&mut self, x: &[f32], kind: Kind) -> Vec<f32> {
        let mut out = vec![0f32; N];
        match kind {
            Kind::Short => {
                for w in 0..8 {
                    let at = 448 + SHORT * w;
                    mdct(&mut self.imdct_short, SHORT, &self.win_short, &x[at..at + 2 * SHORT], &mut out[w * SHORT..(w + 1) * SHORT]);
                }
            }
            _ => {
                let win = match kind {
                    Kind::Start => &self.win_start,
                    Kind::Stop => &self.win_stop,
                    _ => &self.win_long,
                };
                mdct(&mut self.imdct, N, win, x, &mut out);
            }
        }
        out
    }

    /// Quantiser steps per scalefactor band for a spectrum.
    fn steps(&self, swb: &[u16], spec: &[f32], common: f64) -> Vec<f64> {
        let g = 10f64.powf(self.gain_db / 20.0);
        (0..swb.len() - 1)
            .map(|b| {
                let band = &spec[swb[b] as usize..swb[b + 1] as usize];
                let rms = (band.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / band.len() as f64).sqrt();
                rms.max(1e-9).powf(SHAPING) * common.powf(1.0 - SHAPING) * g
            })
            .collect()
    }

    fn encode_block(&mut self, blocks: &[Vec<f32>], kind: Kind, adapt: bool) -> Vec<u8> {
        let spec: Vec<Vec<f32>> = blocks.iter().map(|b| self.forward(b, kind)).collect();
        let (swb, groups): (Vec<u16>, usize) = if kind == Kind::Short { (self.swb_short.clone(), 8) } else { (self.swb.to_vec(), 1) };
        let swb = &swb[..];
        let bands = swb.len() - 1;
        let level = |s: &[f32]| (s.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / s.len() as f64).sqrt().max(1e-9);

        let mut w = BitWriter::default();
        if self.channels == 1 {
            let common = level(&spec[0]);
            let steps = self.steps(swb, &spec[0], common);
            let qz = Quantised::new(&spec[0], swb, &steps);
            let max_sfb = qz.max_sfb(groups);
            w.put(0, 3); // a single channel element
            w.put(0, 4);
            qz.write(&mut w, swb, max_sfb, groups, Some(kind));
        } else {
            let (l, r) = (spec[0].clone(), spec[1].clone());
            // mid/side per band where it looks cheaper
            let mut ms = vec![false; bands];
            let (mut a, mut b) = (l.clone(), r.clone());
            let (cl, cr) = (level(&l), level(&r));
            let cost = |x: &[f32], step: f64| x.iter().map(|&v| (1.0 + (v.abs() as f64) / step).log2()).sum::<f64>();
            let (sl, sr) = (self.steps(swb, &l, cl), self.steps(swb, &r, cr));
            let mid: Vec<f32> = l.iter().zip(&r).map(|(x, y)| (x + y) * 0.5).collect();
            let side: Vec<f32> = l.iter().zip(&r).map(|(x, y)| (x - y) * 0.5).collect();
            let (sm, ss) = (self.steps(swb, &mid, level(&mid).max(cl.min(cr) * 0.5)), self.steps(swb, &side, level(&side).max(cl.min(cr) * 0.5)));
            for k in 0..bands {
                let (lo, hi) = (swb[k] as usize, swb[k + 1] as usize);
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
            let (steps_a, steps_b) = (self.steps(swb, &a, common), self.steps(swb, &b, common));
            let (qa, qb) = (Quantised::new(&a, swb, &steps_a), Quantised::new(&b, swb, &steps_b));
            let max_sfb = qa.max_sfb(groups).max(qb.max_sfb(groups));
            w.put(1, 3); // a channel pair element
            w.put(0, 4);
            w.put(1, 1); // one set of window info for both
            write_ics_info(&mut w, max_sfb, kind);
            let per = bands / groups;
            let used: Vec<bool> = (0..groups).flat_map(|g| ms[g * per..g * per + max_sfb].to_vec()).collect();
            let any_ms = used.iter().any(|&m| m);
            w.put(any_ms as u32, 2); // 1: a mask follows, 0: no mid/side
            if any_ms {
                for &m in &used {
                    w.put(m as u32, 1);
                }
            }
            qa.write(&mut w, swb, max_sfb, groups, None);
            qb.write(&mut w, swb, max_sfb, groups, None);
        }
        w.put(7, 3); // end of the block
        let bytes = w.finish();
        if adapt {
            let err = (bytes.len() as f64 * 8.0 / self.target_bits).ln();
            self.gain_db = (self.gain_db + (err * 4.0).clamp(-1.5, 1.5)).clamp(-30.0, 50.0);
        }
        bytes
    }

    /// Take the next frame of samples; a block is encoded once the one after it is known, because
    /// whether it ends in a start window depends on that one.
    fn emit(&mut self, cur: Vec<Vec<f32>>, out: &mut Vec<Packet>) {
        let blocks: Vec<Vec<f32>> = (0..self.channels).map(|c| self.prev[c].iter().chain(&cur[c]).copied().collect()).collect();
        let attack = self.switching && blocks.iter().any(|b| has_attack(b));
        self.queue.push_back(Pending { blocks, attack });
        self.prev = cur;
        self.pushed += 1;
        if self.queue.len() >= 2 {
            self.encode_front(self.queue[1].attack, out);
        }
    }

    fn encode_front(&mut self, next_attack: bool, out: &mut Vec<Packet>) {
        let Some(p) = self.queue.pop_front() else { return };
        let short = p.attack || (self.prev_short && next_attack);
        let kind = if short {
            Kind::Short
        } else if next_attack {
            Kind::Start
        } else if self.prev_short {
            Kind::Stop
        } else {
            Kind::Long
        };
        self.prev_short = short;
        self.short_blocks += short as u64;
        if self.frames < 2 && p.blocks.iter().any(|b| b.iter().any(|&v| v != 0.0)) {
            // no history for the rate loop yet: find the step that meets the budget
            let (mut lo, mut hi) = (-30.0f64, 50.0f64);
            for _ in 0..7 {
                self.gain_db = (lo + hi) / 2.0;
                if self.encode_block(&p.blocks, kind, false).len() as f64 * 8.0 > self.target_bits {
                    lo = self.gain_db;
                } else {
                    hi = self.gain_db;
                }
            }
            self.gain_db = hi;
        }
        let data = self.encode_block(&p.blocks, kind, true);
        out.push(Packet { stream: 0, pts: self.frames as i64 * N as i64, duration: N as i64, data });
        self.frames += 1;
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
        while self.pushed < wanted {
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
        while !self.queue.is_empty() {
            self.encode_front(false, &mut out);
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

    /// Decode with the encoder's switching on or off; returns the error before an attack and the
    /// number of short blocks.
    fn pre_echo(switching: bool, channels: usize) -> (f64, u64) {
        let rate = 44_100u32;
        let n = 40_000usize;
        let attack = 20_000usize;
        let make = |t: usize, c: usize| {
            let tone = 0.05 * (2.0 * std::f64::consts::PI * 440.0 * t as f64 / 44_100.0 + c as f64).sin();
            let burst = if (attack..attack + 300).contains(&t) { 0.8 * ((((t * 2654435761 + c) % 65536) as f64 / 32768.0) - 1.0) } else { 0.0 };
            tone + burst
        };
        let input: Vec<f64> = (0..n * channels).map(|i| make(i / channels, i % channels)).collect();
        let mut enc = AacEncoder::new(rate, channels, 96_000).unwrap();
        enc.switching = switching;
        let mut packets = Vec::new();
        for chunk in input.chunks(1000 * channels) {
            packets.extend(enc.encode(&Frame::Audio(AudioFrame { sample_rate: rate, channels: channels as u16, pts: 0, samples: chunk.to_vec() })).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        let mut dec = AacDecoder::new(&audio_specific_config(rate, channels), &StreamParams::Audio { sample_rate: rate, channels: channels as u16 }).unwrap();
        let mut out = Vec::new();
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                let Frame::Audio(a) = f else { panic!() };
                out.extend(a.samples);
            }
        }
        let out = &out[N * channels..];
        let err: f64 = (attack - 900..attack).flat_map(|t| (0..channels).map(move |c| (t, c))).map(|(t, c)| (input[t * channels + c] - out[t * channels + c]).powi(2)).sum();
        (err, enc.short_blocks)
    }

    #[test]
    fn an_attack_gets_short_windows_and_less_pre_echo() {
        for channels in [1usize, 2] {
            let (long_err, long_short) = pre_echo(false, channels);
            let (sw_err, sw_short) = pre_echo(true, channels);
            eprintln!("{channels} ch: pre-echo error {long_err:.4} (long only) vs {sw_err:.4} ({sw_short} short blocks)");
            assert_eq!(long_short, 0);
            assert!(sw_short >= 1);
            assert!(sw_err < long_err * 0.5, "{channels} ch: {sw_err} vs {long_err}");
        }
    }
}
