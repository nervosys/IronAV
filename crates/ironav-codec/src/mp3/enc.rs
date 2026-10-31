//! An MPEG-1 Layer III encoder: 32, 44.1 and 48 kHz, mono or stereo, constant bit rate. The chain is
//! the standard's: a 32-band polyphase analysis, a 36-point MDCT per subband (three 12-point ones in a
//! short block), the aliasing butterflies (long blocks), then a quantiser whose step follows each scalefactor band's level only
//! partway (`SHAPING`, as in the AAC and Vorbis encoders: noise is shaped, not hidden, since there is
//! no psychoacoustic model). The Huffman table of each region, the region split and the count1 table
//! are chosen for the fewest bits, and a bisection on the step holds the frame to its bit budget.
//!
//! Stereo is joint: a frame is coded as left/right or as mid/side, whichever looks cheaper. The bit
//! reservoir is used the way the standard intends: a frame's data may start in the unused tail of the
//! frames before it, so a packet is only complete (and emitted) once later frames have filled it.
//! The reservoir is steered toward a fixed level, so frames spend what they have; there is no
//! look-ahead for hard frames.
//!
//! Attacks switch a granule to short blocks, with start and stop windows around it (a granule of
//! look-ahead tells the one before). Both channels share the block type, as mid/side needs.
//!
//! Not done: mixed blocks, intensity stereo, scalefactor reuse between granules, subblock gain,
//! MPEG-2/2.5 sampling rates.

use super::tables::*;
use super::{statics, Header};
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, Result};
use std::collections::VecDeque;
use std::f64::consts::PI;
use std::sync::OnceLock;

const GRANULE: usize = 576;
const FRAME: usize = 1152;
const SHAPING: f64 = 0.6;
/// The decoder's own delay (polyphase synthesis) in samples; the LAME tag counts the encoder's part.
pub const DECODER_DELAY: i64 = 529;
/// Samples the decoder outputs ahead of the first input sample: the encoder's analysis, the MDCT's
/// one-granule overlap, and the decoder's synthesis (the filterbank pair alone is 481).
pub const LEAD_IN: i64 = 1057;
const BITRATES: [u32; 14] = [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
const RATES: [u32; 3] = [44100, 48000, 32000];
/// Part 2 and 3 of a granule are counted in 12 bits.
const MAX_PART23: usize = 4095;
/// Largest value a pair can carry: 15 plus 13 bits of escape.
const MAX_Q: i32 = 8206;
const INF: u32 = 1 << 20;
/// The reservoir can be reached back into by at most 9 bits of `main_data_begin`.
const MAX_RESERVOIR: usize = 511;

// ---- tables ------------------------------------------------------------------------------------

struct Code {
    len: [u8; 256],
    code: [u32; 256],
}

struct Tables {
    /// Big-value tables 1..=15 (index 0 unused): index `x * 16 + y`.
    big: Vec<Code>,
    /// cos(π/72 (2i+19)(2k+1)), indexed `[k][i]`.
    mdct: Vec<[f64; 36]>,
    /// cos(π/24 (2i+7)(2k+1)), indexed `[k][i]`: the short transform.
    mdct12: Vec<[f64; 12]>,
    /// (2k+1)(i-16)π/64 matrixing of the analysis filterbank, `[k][i]`.
    matrix: Vec<[f64; 64]>,
    /// The analysis window: the synthesis window over 32.
    window: [f64; 512],
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut big = vec![Code { len: [0; 256], code: [0; 256] }];
        let (mut at_len, mut at_sym) = (0usize, 0usize);
        for t in 0..15 {
            let n = HUFF_SIZES_MINUS_ONE[t] as usize + 1;
            let mut c = Code { len: [0; 256], code: [0; 256] };
            let mut code = 0u64;
            for i in 0..n {
                let len = HUFF_LENS[at_len + i] as u32;
                let sym = HUFF_SYMBOLS[at_sym + i] as usize;
                c.len[sym] = len as u8;
                c.code[sym] = (code >> (32 - len)) as u32;
                code += 1u64 << (32 - len);
            }
            at_len += n;
            at_sym += n;
            big.push(c);
        }
        let mdct = (0..18).map(|k| std::array::from_fn(|i| (PI / 72.0 * (2 * i + 19) as f64 * (2 * k + 1) as f64).cos())).collect();
        let mdct12 = (0..6).map(|k| std::array::from_fn(|i| (PI / 24.0 * (2 * i + 7) as f64 * (2 * k + 1) as f64).cos())).collect();
        let matrix = (0..32).map(|k| std::array::from_fn(|i| ((2 * k + 1) as f64 * (i as f64 - 16.0) * PI / 64.0).cos())).collect();
        let s = statics();
        let window = std::array::from_fn(|i| s.window[i] as f64 / 32.0);
        Tables { big, mdct, mdct12, matrix, window }
    })
}

/// Largest value (plus one is the side of the square) each of the non-escape tables 1..=13 codes.
const BASE_MAX: [i32; 13] = [1, 2, 2, 3, 3, 5, 5, 5, 7, 7, 7, 15, 15];
/// The `table_select` value of each of those tables.
const BASE_SEL: [usize; 13] = [1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15];
/// The two escape tables (14 and 15 here) with their `table_select` values and linbits.
const ESC_SEL: [[usize; 8]; 2] = [[16, 17, 18, 19, 20, 21, 22, 23], [24, 25, 26, 27, 28, 29, 30, 31]];
const ESC_LINBITS: [[u32; 8]; 2] = [[1, 2, 3, 4, 6, 8, 10, 13], [4, 5, 6, 7, 8, 9, 11, 13]];

// ---- bit writing -------------------------------------------------------------------------------

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

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Long,
    Start,
    Short,
    Stop,
}

impl Kind {
    fn block_type(self) -> u32 {
        match self {
            Kind::Long => 0,
            Kind::Start => 1,
            Kind::Short => 2,
            Kind::Stop => 3,
        }
    }
}

// ---- analysis ----------------------------------------------------------------------------------

/// The polyphase analysis filterbank of one channel.
struct Polyphase {
    x: [f64; 512],
}

impl Polyphase {
    /// 32 samples in, one sample for each of the 32 subbands out.
    fn push(&mut self, input: &[f32]) -> [f64; 32] {
        let t = tables();
        self.x.copy_within(0..480, 32);
        for i in 0..32 {
            self.x[31 - i] = input[i] as f64;
        }
        let mut y = [0.0f64; 64];
        for (i, y) in y.iter_mut().enumerate() {
            *y = (0..8).map(|j| t.window[i + 64 * j] * self.x[i + 64 * j]).sum();
        }
        std::array::from_fn(|k| t.matrix[k].iter().zip(&y).map(|(a, b)| a * b).sum())
    }
}

/// One channel's analysis: granules of 576 samples to 576 spectral lines.
struct Analysis {
    poly: Polyphase,
    /// The previous granule's subband samples, `[subband][slot]`, with the odd-slot inversion applied.
    prev: Vec<[f64; 18]>,
}

/// What the MDCT must multiply by for the decoder to give back what went in (the decoder's inverse
/// transform has no scale of its own).
const MDCT_SCALE: f64 = 1.0 / 9.0;
/// The same for the 12-point transform (4 / n).
const MDCT_SCALE_SHORT: f64 = 1.0 / 3.0;

impl Analysis {
    fn new() -> Self {
        Analysis { poly: Polyphase { x: [0.0; 512] }, prev: vec![[0.0; 18]; 32] }
    }

    /// The subband samples of a granule of PCM, `[subband][slot]`, with the odd-slot inversion applied.
    fn subbands(&mut self, pcm: &[f32]) -> Vec<[f64; 18]> {
        let mut cur = vec![[0.0f64; 18]; 32];
        for slot in 0..18 {
            let sb = self.poly.push(&pcm[slot * 32..slot * 32 + 32]);
            for k in 0..32 {
                // frequency inversion: the decoder negates the odd samples of odd subbands
                cur[k][slot] = if k & 1 == 1 && slot & 1 == 1 { -sb[k] } else { sb[k] };
            }
        }
        cur
    }

    /// The 576 lines of a granule's subband samples (with the previous granule's, called in order) for
    /// a block of the given kind. A short block's lines are left in the decoder's order after its
    /// reordering: `[subband * 18 + 3 * line + window]`.
    fn granule(&mut self, cur: Vec<[f64; 18]>, kind: Kind) -> [f32; GRANULE] {
        let t = tables();
        let s = statics();
        let mut xr = [0.0f64; GRANULE];
        for sb in 0..32 {
            if cur[sb].iter().chain(&self.prev[sb]).all(|&v| v == 0.0) {
                continue;
            }
            if kind == Kind::Short {
                let mut z = [0.0f64; 36];
                z[..18].copy_from_slice(&self.prev[sb]);
                z[18..].copy_from_slice(&cur[sb]);
                for w in 0..3 {
                    for k in 0..6 {
                        let acc: f64 = (0..12).map(|i| z[6 + 6 * w + i] * s.win_short[i] as f64 * t.mdct12[k][i]).sum();
                        xr[18 * sb + 3 * k + w] = acc * MDCT_SCALE_SHORT;
                    }
                }
            } else {
                let win = &s.win[kind.block_type() as usize];
                let mut z = [0.0f64; 36];
                for i in 0..18 {
                    z[i] = self.prev[sb][i] * win[i] as f64;
                    z[18 + i] = cur[sb][i] * win[18 + i] as f64;
                }
                for k in 0..18 {
                    xr[18 * sb + k] = t.mdct[k].iter().zip(&z).map(|(a, b)| a * b).sum::<f64>() * MDCT_SCALE;
                }
            }
        }
        self.prev = cur;
        if kind != Kind::Short {
            // the aliasing butterflies: the decoder rotates (a, b) by (cs, ca); this is the inverse
            for sb in 1..32 {
                let base = 18 * sb;
                for i in 0..8 {
                    let (a, b) = (xr[base - 1 - i], xr[base + i]);
                    xr[base - 1 - i] = a * s.cs[i] + b * s.ca[i];
                    xr[base + i] = b * s.cs[i] - a * s.ca[i];
                }
            }
        }
        xr.map(|v| v as f32)
    }
}

/// Where in a granule's subband samples (summed over channels, `slots` of them) an attack starts:
/// a slot whose energy above the lowest subbands is much more than the few before it. `history` holds
/// those few and is updated.
fn find_attack(chans: &[Vec<[f64; 18]>], history: &mut [f64; 4]) -> Option<usize> {
    let mut found = None;
    for slot in 0..18 {
        let e: f64 = chans.iter().map(|c| c[3..].iter().map(|sb| sb[slot] * sb[slot]).sum::<f64>()).sum();
        let mean = history.iter().sum::<f64>() / 4.0;
        if found.is_none() && e > ATTACK_FLOOR && e > ATTACK_RATIO * mean + 1e-9 {
            found = Some(slot);
        }
        history.copy_within(1..4, 0);
        history[3] = e;
    }
    found
}

const ATTACK_FLOOR: f64 = 1e-3;
const ATTACK_RATIO: f64 = 8.0;

// ---- coding one granule of one channel -----------------------------------------------------------

struct Coded {
    kind: Kind,
    q: [i32; GRANULE],
    /// Scalefactors: 21 bands, or for a short block 12 bands of three windows each.
    sf: [u8; 36],
    global_gain: i32,
    compress: usize,
    /// Pairs in the big-value region and quadruples in the count1 region.
    big: usize,
    quads: usize,
    r0: usize,
    r1: usize,
    sel: [usize; 3],
    count1_table: usize,
    part2: usize,
    part3: usize,
}

impl Coded {
    fn bits(&self) -> usize {
        self.part2 + self.part3
    }
}

/// Per scalefactor band: what each Huffman table would cost.
struct BandCosts {
    max: Vec<i32>,
    /// Prefix sums over bands of the cost with each base table (signs included).
    base: Vec<[u32; 13]>,
    /// Prefix sums of the cost with the escape tables' codes (linbits not included) and of the number of escapes.
    esc: Vec<[u32; 2]>,
    n_esc: Vec<u32>,
}

impl BandCosts {
    fn new(q: &[i32; GRANULE], bounds: &[usize], pairs: usize) -> Self {
        let t = tables();
        let nb = bounds.len() - 1;
        let mut c = BandCosts { max: vec![0; nb], base: vec![[0; 13]; nb + 1], esc: vec![[0; 2]; nb + 1], n_esc: vec![0; nb + 1] };
        for b in 0..nb {
            let (lo, hi) = (bounds[b], bounds[b + 1].min(2 * pairs));
            let mut max = 0;
            let mut cost = [0u32; 13];
            let mut esc = [0u32; 2];
            let (mut n_esc, mut signs) = (0u32, 0u32);
            let mut i = lo;
            while i < hi {
                let (x, y) = (q[i].abs(), q[i + 1].abs());
                max = max.max(x).max(y);
                signs += (x != 0) as u32 + (y != 0) as u32;
                n_esc += (x >= 15) as u32 + (y >= 15) as u32;
                let sym = (x.min(15) * 16 + y.min(15)) as usize;
                for (j, cj) in cost.iter_mut().enumerate() {
                    *cj += t.big[j + 1].len[sym] as u32;
                }
                esc[0] += t.big[14].len[sym] as u32;
                esc[1] += t.big[15].len[sym] as u32;
                i += 2;
            }
            c.max[b] = max;
            for j in 0..13 {
                c.base[b + 1][j] = c.base[b][j] + if max > BASE_MAX[j] { INF } else { cost[j] + signs };
            }
            for f in 0..2 {
                c.esc[b + 1][f] = c.esc[b][f] + esc[f] + signs;
            }
            c.n_esc[b + 1] = c.n_esc[b] + n_esc;
        }
        c
    }

    /// The cheapest table for bands `a..b`: (bits, `table_select`).
    fn best(&self, a: usize, b: usize) -> (u32, usize) {
        if a >= b {
            return (0, 0);
        }
        let max = self.max[a..b].iter().copied().max().unwrap_or(0);
        if max == 0 {
            return (0, 0);
        }
        let mut best = (u32::MAX, 0usize);
        if max <= 15 {
            for j in 0..13 {
                let cost = self.base[b][j] - self.base[a][j];
                if max <= BASE_MAX[j] && cost < best.0 {
                    best = (cost, BASE_SEL[j]);
                }
            }
        }
        for f in 0..2 {
            let n = self.n_esc[b] - self.n_esc[a];
            let need = (max - 14).max(1) as u32; // 2^linbits must exceed max - 15
            if let Some(l) = (0..8).find(|&l| (1u32 << ESC_LINBITS[f][l]) >= need) {
                let cost = self.esc[b][f] - self.esc[a][f] + ESC_LINBITS[f][l] * n;
                if cost < best.0 {
                    best = (cost, ESC_SEL[f][l]);
                }
            }
        }
        best
    }
}

/// Offsets of the long scalefactor bands, as coefficient indices (23 values).
fn band_bounds(rate_index: usize) -> Vec<usize> {
    let mut v = vec![0usize];
    for &s in &BAND_SIZE_LONG[rate_index] {
        v.push(v.last().unwrap() + s as usize);
    }
    v
}

/// Offsets of the short bands in the order they are coded: for each of the 13 bands, its three
/// windows one after another (40 values).
fn short_bounds(rate_index: usize) -> Vec<usize> {
    let mut v = vec![0usize];
    let mut start = 0;
    for &len in &BAND_SIZE_SHORT[rate_index] {
        let len = len as usize;
        for w in 0..3 {
            v.push(3 * start + (w + 1) * len);
        }
        start += len;
    }
    v
}

fn pow34(v: f64) -> f64 {
    (v * v.sqrt()).sqrt()
}

/// Quantise a spectrum with the desired step of each band, and find the cheapest way to write it.
/// `bounds` are the band offsets in coded order; `steps` has one entry per band.
fn code_unit(xr: &[f32; GRANULE], kind: Kind, bounds: &[usize], steps: &[f64]) -> Coded {
    let short = kind == Kind::Short;
    let nb = bounds.len() - 1;
    // bands with a scalefactor, and how many of them use the first (wider) length field
    let (nsf, split) = if short { (36, 18) } else { (21, 11) };
    // exponent of each band's step in quarter octaves
    let want: Vec<i32> = steps.iter().map(|&s| (s.max(1e-30).log2() * 4.0).round() as i32).collect();
    let mut top = want[..nsf].iter().copied().max().unwrap_or(0);
    if xr[bounds[nsf]..].iter().any(|&v| v != 0.0) {
        // the last band has no scalefactor of its own: it takes the global step
        top = top.min(want[nsf..nb].iter().copied().min().unwrap_or(0) + 12);
    }
    let global_gain = (top + 210).clamp(0, 255);
    let base = global_gain - 210;
    let mut q = [0i32; GRANULE];
    let mut sf = [0u8; 36];
    for b in 0..nb {
        let max_sf = if b < split { 15 } else { 7 };
        let s = if b < nsf { ((base - want[b] + 1).div_euclid(2)).clamp(0, max_sf) } else { 0 };
        let e = base - 2 * s;
        let inv = (-(e as f64) / 4.0).exp2();
        let mut any = false;
        for k in bounds[b]..bounds[b + 1] {
            let m = (pow34(xr[k].abs() as f64 * inv) + 0.4054).floor() as i32;
            let m = m.min(MAX_Q);
            q[k] = if xr[k] < 0.0 { -m } else { m };
            any |= m != 0;
        }
        if b < nsf {
            // a band that codes as zeros may as well take the cheapest scalefactor
            sf[b] = if any { s as u8 } else { 0 };
        }
    }
    // the cheapest scalefactor_compress that can hold them
    let (m1, m2) = (sf[..split].iter().copied().max().unwrap_or(0) as u32, sf[split..nsf].iter().copied().max().unwrap_or(0) as u32);
    let compress = (0..16)
        .filter(|&c| (1u32 << SLEN[0][c]) > m1 && (1u32 << SLEN[1][c]) > m2)
        .min_by_key(|&c| split * SLEN[0][c] as usize + (nsf - split) * SLEN[1][c] as usize)
        .unwrap_or(15);
    let part2 = split * SLEN[0][compress] as usize + (nsf - split) * SLEN[1][compress] as usize;

    // the big-value / count1 / zero partition
    let nz_end = q.iter().rposition(|&v| v != 0).map_or(0, |p| p + 1);
    let big_end = q.iter().rposition(|&v| v.abs() > 1).map_or(0, |p| p + 1);
    let mut big = big_end.div_ceil(2);
    let mut quads;
    loop {
        quads = nz_end.saturating_sub(2 * big).div_ceil(4);
        if 2 * big + 4 * quads <= GRANULE {
            break;
        }
        big += 1;
    }

    // region split and tables
    let (mut best, mut r0, mut r1, mut sel) = (u32::MAX, 0, 0, [0usize; 3]);
    if kind == Kind::Long {
        let costs = BandCosts::new(&q, bounds, big);
        let nbands = bounds[1..].iter().position(|&b| b >= 2 * big).map_or(22, |p| p + 1).min(22);
        let nbands = if big == 0 { 0 } else { nbands };
        for a in 0..16 {
            let e0 = (a + 1).min(nbands);
            let (c0, s0) = costs.best(0, e0);
            for b in 0..8 {
                let e1 = (a + b + 2).min(nbands);
                let (c1, s1) = costs.best(e0, e1);
                let (c2, s2) = costs.best(e1, nbands);
                let c = c0 + c1 + c2;
                if c < best {
                    (best, r0, r1, sel) = (c, a, b, [s0, s1, s2]);
                }
            }
        }
    } else {
        // blocks with a window-switching flag have two regions, split after 36 lines
        let costs = BandCosts::new(&q, &[0, REGION_SPLIT, GRANULE], big);
        let (c0, s0) = costs.best(0, 1);
        let (c1, s1) = costs.best(1, 2);
        (best, sel) = (c0 + c1, [s0, s1, 0]);
    }
    if best == u32::MAX {
        best = 0;
    }
    // count1: four values of -1, 0 or 1 per code
    let (mut ca, mut nzq) = (0u32, 0u32);
    for g in 0..quads {
        let at = 2 * big + 4 * g;
        let mut sym = 0usize;
        for j in 0..4 {
            let nz = (q[at + j] != 0) as usize;
            sym = sym << 1 | nz;
            nzq += nz as u32;
        }
        ca += QUAD_BITS[0][sym] as u32;
    }
    let cb = 4 * quads as u32;
    let count1_table = (cb < ca) as usize;
    let part3 = best as usize + nzq as usize + if count1_table == 1 { cb } else { ca } as usize;
    Coded { kind, q, sf, global_gain, compress, big, quads, r0, r1, sel, count1_table, part2, part3 }
}

/// Where the second Huffman region starts in a block with the window-switching flag.
const REGION_SPLIT: usize = 36;

fn write_unit(w: &mut BitWriter, c: &Coded, bounds: &[usize]) {
    let t = tables();
    let (nsf, split) = if c.kind == Kind::Short { (36, 18) } else { (21, 11) };
    for (b, &s) in c.sf[..nsf].iter().enumerate() {
        w.put(s as u32, SLEN[(b >= split) as usize][c.compress] as u32);
    }
    let edges = if c.kind == Kind::Long {
        let end = |bands: usize| bounds[bands.min(22)].min(2 * c.big);
        [0, end(c.r0 + 1), end(c.r0 + c.r1 + 2), 2 * c.big]
    } else {
        [0, REGION_SPLIT.min(2 * c.big), 2 * c.big, 2 * c.big]
    };
    for r in 0..3 {
        let [table, linbits] = HUFF_DATA[c.sel[r]];
        let (table, linbits) = (table as usize, linbits as u32);
        if table == 0 {
            continue;
        }
        let tab = &t.big[table];
        for i in (edges[r]..edges[r + 1]).step_by(2) {
            let (x, y) = (c.q[i].abs(), c.q[i + 1].abs());
            let sym = (x.min(15) * 16 + y.min(15)) as usize;
            w.put(tab.code[sym], tab.len[sym] as u32);
            for v in [c.q[i], c.q[i + 1]] {
                let m = v.abs();
                if m >= 15 && linbits > 0 {
                    w.put((m - 15) as u32, linbits);
                }
                if m != 0 {
                    w.put((v < 0) as u32, 1);
                }
            }
        }
    }
    let table = c.count1_table;
    for g in 0..c.quads {
        let at = 2 * c.big + 4 * g;
        let sym = (0..4).fold(0usize, |a, j| a << 1 | (c.q[at + j] != 0) as usize);
        w.put(QUAD_CODES[table][sym] as u32, QUAD_BITS[table][sym] as u32);
        for j in 0..4 {
            if c.q[at + j] != 0 {
                w.put((c.q[at + j] < 0) as u32, 1);
            }
        }
    }
}

// ---- the encoder ---------------------------------------------------------------------------------

struct Pending {
    /// Header and side information.
    head: Vec<u8>,
    /// Bytes of main data this frame's slot holds.
    len: usize,
    /// End of the slot in the main-data stream.
    end: u64,
}

pub struct Mp3Encoder {
    channels: usize,
    rate_index: usize,
    sample_rate: u32,
    bitrate: u32,
    bitrate_index: usize,
    bounds: Vec<usize>,
    bounds_short: Vec<usize>,
    /// First coefficient beyond the low-pass.
    cutoff: usize,
    ana: Vec<Analysis>,
    buf: Vec<Vec<f32>>,
    /// Spectra of the granules waiting for their frame's second granule: `[granule][channel]`.
    queue: Vec<Vec<[f32; GRANULE]>>,
    queue_kinds: Vec<Kind>,
    /// The last granule's subband samples per channel and whether its block is short, waiting for the
    /// next granule to say whether a start window is needed.
    held: Option<(Vec<Vec<[f64; 18]>>, bool)>,
    /// The next granule must be short (an attack late in the last one).
    carry_short: bool,
    prev_short: bool,
    history: [f64; 4],
    switching: bool,
    short_granules: u64,
    gain_db: f64,
    /// Fractional bytes carried to decide which frames take a padding byte.
    slot_acc: u32,
    /// Main data placed so far, from the start of the oldest frame not yet emitted.
    stream: Vec<u8>,
    /// Absolute end of the slots of all frames so far and of the data placed.
    slots_end: u64,
    placed: u64,
    pending: VecDeque<Pending>,
    out: VecDeque<Packet>,
    frames_in: u64,
    frames_out: u64,
    input: u64,
}

impl Mp3Encoder {
    pub fn new(sample_rate: u32, channels: usize, bitrate: u32) -> Result<Self> {
        if !(1..=2).contains(&channels) {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the MP3 encoder takes mono or stereo audio, not {channels} channels")));
        }
        let Some(rate_index) = RATES.iter().position(|&r| r == sample_rate) else {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the MP3 encoder writes MPEG-1 only, which has no {sample_rate} Hz"))
                .hint("resample to 32000, 44100 or 48000 Hz"));
        };
        let kbps = Self::snap(bitrate / 1000);
        let bitrate_index = BITRATES.iter().position(|&b| b == kbps).unwrap() + 1;
        let per_channel = kbps as f64 / channels as f64;
        let cutoff_hz = (4000.0 + per_channel * 190.0).min(20000.0).min(sample_rate as f64 * 0.45);
        let cutoff = ((cutoff_hz / (sample_rate as f64 / 2.0)) * GRANULE as f64).round() as usize;
        Ok(Self {
            channels,
            rate_index,
            sample_rate,
            bitrate: kbps,
            bitrate_index,
            bounds: band_bounds(rate_index),
            bounds_short: short_bounds(rate_index),
            cutoff: cutoff.min(GRANULE),
            ana: (0..channels).map(|_| Analysis::new()).collect(),
            buf: vec![Vec::new(); channels],
            queue: Vec::new(),
            queue_kinds: Vec::new(),
            held: None,
            carry_short: false,
            prev_short: false,
            history: [0.0; 4],
            switching: true,
            short_granules: 0,
            gain_db: 6.0,
            slot_acc: 0,
            stream: Vec::new(),
            slots_end: 0,
            placed: 0,
            pending: VecDeque::new(),
            out: VecDeque::new(),
            frames_in: 0,
            frames_out: 0,
            input: 0,
        })
    }

    /// The nearest bit rate MPEG-1 Layer III has, in kbit/s.
    pub fn snap(kbps: u32) -> u32 {
        *BITRATES.iter().min_by_key(|&&b| b.abs_diff(kbps)).unwrap()
    }

    pub fn default_bitrate(channels: usize) -> u32 {
        64_000 * channels as u32
    }

    /// The desired quantiser step of each band, from the band's level and the spectrum's overall level.
    fn steps(&self, spec: &[f32; GRANULE], bounds: &[usize], common: f64, gain_db: f64) -> Vec<f64> {
        let g = 10f64.powf(gain_db / 20.0);
        (0..bounds.len() - 1)
            .map(|b| {
                let band = &spec[bounds[b]..bounds[b + 1]];
                let rms = (band.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / band.len() as f64).sqrt();
                rms.max(1e-9).powf(SHAPING) * common.powf(1.0 - SHAPING) * g
            })
            .collect()
    }

    fn bounds_of(&self, kind: Kind) -> &[usize] {
        if kind == Kind::Short { &self.bounds_short } else { &self.bounds }
    }

    /// Put a spectrum from the analysis into the order it is coded in, with the low-pass applied. A
    /// short block's lines go band by band, window by window.
    fn prepare(&self, xr: &[f32; GRANULE], kind: Kind) -> [f32; GRANULE] {
        let mut x = *xr;
        x[self.cutoff..].fill(0.0);
        if kind != Kind::Short {
            return x;
        }
        let mut out = [0.0f32; GRANULE];
        let mut start = 0;
        for &len in &BAND_SIZE_SHORT[self.rate_index] {
            let len = len as usize;
            for w in 0..3 {
                for j in 0..len {
                    let f = start + j;
                    out[3 * start + w * len + j] = x[18 * (f / 6) + 3 * (f % 6) + w];
                }
            }
            start += len;
        }
        out
    }

    fn level(spec: &[f32; GRANULE]) -> f64 {
        (spec.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / GRANULE as f64).sqrt().max(1e-9)
    }

    /// Code every granule and channel of a frame with one step setting.
    fn code_all(&self, spec: &[Vec<[f32; GRANULE]>], kinds: &[Kind], commons: &[f64], gain_db: f64) -> Vec<Coded> {
        let mut out = Vec::new();
        for (gr, chans) in spec.iter().enumerate() {
            for (ch, s) in chans.iter().enumerate() {
                let mut gain = gain_db;
                let bounds = self.bounds_of(kinds[gr]);
                loop {
                    let c = code_unit(s, kinds[gr], bounds, &self.steps(s, bounds, commons[gr], gain));
                    if c.bits() <= MAX_PART23 || gain > gain_db + 60.0 {
                        out.push(c);
                        break;
                    }
                    gain += 1.0;
                }
                let _ = ch;
            }
        }
        out
    }

    fn encode_frame(&mut self, mut spec: Vec<Vec<[f32; GRANULE]>>, kinds: Vec<Kind>) {
        let nch = self.channels;
        // the common level of each granule, before any mid/side
        let mut commons: Vec<f64> = spec.iter().map(|c| (c.iter().map(|s| Self::level(s).powi(2)).sum::<f64>() / nch as f64).sqrt()).collect();
        let mut ms = false;
        if nch == 2 {
            let cost = |x: &[f32], step: f64| x.iter().map(|&v| (1.0 + (v.abs() as f64) / step).log2()).sum::<f64>();
            let (mut lr, mut m_s) = (0.0, 0.0);
            let r = std::f32::consts::FRAC_1_SQRT_2;
            for (gr, c) in spec.iter().enumerate() {
                let mid: [f32; GRANULE] = std::array::from_fn(|i| (c[0][i] + c[1][i]) * r);
                let side: [f32; GRANULE] = std::array::from_fn(|i| (c[0][i] - c[1][i]) * r);
                for (a, b) in [(&c[0], 0), (&c[1], 1), (&mid, 2), (&side, 3)].map(|(s, k)| (s, k)) {
                    let bounds = self.bounds_of(kinds[gr]);
                    let st = self.steps(a, bounds, commons[gr], 0.0);
                    let total: f64 = (0..bounds.len() - 1).map(|k| cost(&a[bounds[k]..bounds[k + 1]], st[k])).sum();
                    if b < 2 { lr += total } else { m_s += total }
                }
            }
            if m_s < lr * 0.9 {
                ms = true;
                for c in spec.iter_mut() {
                    let (l, rr) = (c[0], c[1]);
                    c[0] = std::array::from_fn(|i| (l[i] + rr[i]) * r);
                    c[1] = std::array::from_fn(|i| (l[i] - rr[i]) * r);
                }
            }
        }
        let _ = &mut commons;

        // this frame's slot and what the reservoir can add
        let h_len = self.next_frame_len();
        let side_len = if nch == 1 { 17 } else { 32 };
        let slot = h_len.0 - 4 - side_len;
        let reservoir = (self.slots_end - self.placed.min(self.slots_end)) as usize;
        let reservoir = reservoir.min(MAX_RESERVOIR);
        let mean = slot * 8;
        let target = (slot * 3 / 4).min(300);
        let budget = ((mean as f64 + (reservoir as f64 - target as f64) * 4.0) as usize).clamp(mean * 2 / 5, (reservoir + slot) * 8);

        let fits = |units: &[Coded]| units.iter().map(|c| c.bits()).sum::<usize>() <= budget;
        let (mut lo, mut hi) = (self.gain_db - 8.0, self.gain_db + 8.0);
        let mut best = self.code_all(&spec, &kinds, &commons, hi);
        while !fits(&best) && hi < 140.0 {
            lo = hi;
            hi += 12.0;
            best = self.code_all(&spec, &kinds, &commons, hi);
        }
        if fits(&best) {
            let first = self.code_all(&spec, &kinds, &commons, lo);
            if fits(&first) {
                best = first;
                hi = lo;
            } else {
                for _ in 0..6 {
                    let mid = (lo + hi) / 2.0;
                    let c = self.code_all(&spec, &kinds, &commons, mid);
                    if fits(&c) {
                        hi = mid;
                        best = c;
                    } else {
                        lo = mid;
                    }
                }
            }
        }
        self.gain_db = hi.clamp(-60.0, 120.0);

        // assemble the frame
        let mut main = BitWriter::default();
        for c in &best {
            write_unit(&mut main, c, self.bounds_of(c.kind));
        }
        let main = main.finish();
        let data_len = main.len();
        let mut side = BitWriter::default();
        side.put(reservoir as u32, 9);
        side.put(0, if nch == 2 { 3 } else { 5 });
        for _ in 0..nch {
            side.put(0, 4); // scfsi
        }
        for gr in 0..2 {
            for ch in 0..nch {
                let c = &best[gr * nch + ch];
                side.put(c.bits() as u32, 12);
                side.put(c.big as u32, 9);
                side.put(c.global_gain as u32, 8);
                side.put(c.compress as u32, 4);
                if c.kind == Kind::Long {
                    side.put(0, 1); // no window switching
                    for s in c.sel {
                        side.put(s as u32, 5);
                    }
                    side.put(c.r0 as u32, 4);
                    side.put(c.r1 as u32, 3);
                } else {
                    side.put(1, 1);
                    side.put(c.kind.block_type(), 2);
                    side.put(0, 1); // not mixed
                    side.put(c.sel[0] as u32, 5);
                    side.put(c.sel[1] as u32, 5);
                    side.put(0, 9); // no subblock gain
                }
                side.put(0, 1); // preflag
                side.put(0, 1); // scalefac_scale
                side.put(c.count1_table as u32, 1);
            }
        }
        let side = side.finish();
        debug_assert_eq!(side.len(), side_len);
        let mut head = vec![0xFF, 0xFB, (self.bitrate_index as u8) << 4 | (self.rate_index as u8) << 2 | (h_len.1 as u8) << 1];
        head.push(match (nch, ms) {
            (1, _) => 3 << 6,
            (_, true) => 1 << 6 | 2 << 4,
            (_, false) => 1 << 6,
        } | 0x04);
        head.extend(side);

        self.slots_end += slot as u64;
        self.pending.push_back(Pending { head, len: slot, end: self.slots_end });
        // the data starts at the oldest free byte, which is `reservoir` bytes before this slot
        debug_assert_eq!(self.placed, self.slots_end - slot as u64 - reservoir as u64);
        self.stream.extend_from_slice(&main);
        self.placed += data_len as u64;
        // more reservoir than the field can reach is lost to padding
        let free = self.slots_end.saturating_sub(self.placed);
        if free > MAX_RESERVOIR as u64 {
            let waste = (free - MAX_RESERVOIR as u64) as usize;
            self.stream.extend(std::iter::repeat_n(0u8, waste));
            self.placed += waste as u64;
        }
        self.release();
        self.frames_in += 1;
    }

    /// Frame length and padding bit of the next frame.
    fn next_frame_len(&mut self) -> (usize, bool) {
        let num = 144_000 * self.bitrate;
        let mut len = (num / self.sample_rate) as usize;
        self.slot_acc += num % self.sample_rate;
        let pad = self.slot_acc >= self.sample_rate;
        if pad {
            self.slot_acc -= self.sample_rate;
            len += 1;
        }
        (len, pad)
    }

    /// Emit the frames whose slots are filled.
    fn release(&mut self) {
        while let Some(p) = self.pending.front() {
            if p.end > self.placed {
                break;
            }
            let p = self.pending.pop_front().unwrap();
            let mut data = p.head;
            data.extend(self.stream.drain(..p.len));
            self.out.push_back(Packet { stream: 0, pts: self.frames_out as i64 * FRAME as i64, duration: FRAME as i64, data });
            self.frames_out += 1;
        }
    }

    /// Take a granule of PCM. Its predecessor's block is transformed now, since whether that one ends
    /// in a start window depends on this one.
    fn granule(&mut self, pcm: Vec<Vec<f32>>) {
        let chans: Vec<Vec<[f64; 18]>> = pcm.iter().enumerate().map(|(c, p)| self.ana[c].subbands(p)).collect();
        let attack = if self.switching { find_attack(&chans, &mut self.history) } else { None };
        // an attack in the first half of the granule belongs to its block, a later one to the next
        let short_now = std::mem::replace(&mut self.carry_short, attack.is_some_and(|s| s >= 9)) || attack.is_some_and(|s| s < 9);
        if let Some((prev, prev_short)) = self.held.replace((chans, short_now)) {
            let short = prev_short || (self.prev_short && short_now);
            let kind = if short {
                Kind::Short
            } else if short_now {
                Kind::Start
            } else if self.prev_short {
                Kind::Stop
            } else {
                Kind::Long
            };
            self.prev_short = short;
            self.short_granules += short as u64;
            let spectra: Vec<[f32; GRANULE]> = prev
                .into_iter()
                .enumerate()
                .map(|(c, sb)| {
                    let xr = self.ana[c].granule(sb, kind);
                    self.prepare(&xr, kind)
                })
                .collect();
            self.queue.push(spectra);
            self.queue_kinds.push(kind);
            if self.queue.len() == 2 {
                let spec = std::mem::take(&mut self.queue);
                let kinds = std::mem::take(&mut self.queue_kinds);
                self.encode_frame(spec, kinds);
            }
        }
    }
}

impl Encoder for Mp3Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid_input("the MP3 encoder received a video frame"));
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
        while self.buf[0].len() >= GRANULE {
            let pcm = self.buf.iter_mut().map(|b| b.drain(..GRANULE).collect()).collect();
            self.granule(pcm);
        }
        Ok(self.out.drain(..).collect())
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        if self.input == 0 {
            return Ok(Vec::new());
        }
        // frames until the decoder has output every input sample
        let wanted = (self.input + LEAD_IN as u64).div_ceil(FRAME as u64);
        while self.frames_in < wanted {
            let pcm = self
                .buf
                .iter_mut()
                .map(|b| {
                    let mut c: Vec<f32> = std::mem::take(b);
                    c.resize(GRANULE, 0.0);
                    c
                })
                .collect();
            self.granule(pcm);
        }
        // the slots still waiting for data are padded out
        if let Some(last) = self.pending.back() {
            let missing = last.end.saturating_sub(self.placed) as usize;
            self.stream.extend(std::iter::repeat_n(0u8, missing));
            self.placed += missing as u64;
        }
        self.release();
        // the last packet's duration ends where the audio does
        let valid = (self.input as i64 + LEAD_IN) - (self.frames_out as i64 - 1) * FRAME as i64;
        if let Some(p) = self.out.back_mut() {
            p.duration = valid.clamp(1, FRAME as i64);
        }
        Ok(self.out.drain(..).collect())
    }
}

/// Whether `h` is a frame this encoder could have written (for tests).
#[allow(dead_code)]
fn is_ours(h: &Header) -> bool {
    !h.lsf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp3::{granule_pcm, Granule, Mp3Decoder};
    use ironav_core::{AudioFrame, Decoder, StreamParams};

    #[test]
    fn analysis_and_the_decoders_synthesis_give_back_the_input() {
        let s = statics();
        let n = 12;
        // long, then an attack's start, short, short and stop windows, then long again
        let kinds = [Kind::Long, Kind::Long, Kind::Start, Kind::Short, Kind::Short, Kind::Stop, Kind::Long, Kind::Long, Kind::Start, Kind::Short, Kind::Stop, Kind::Long];
        let input: Vec<f32> = (0..n * GRANULE).map(|i| ((i * 7919) % 2000) as f32 / 2000.0 - 0.5 + 0.3 * (i as f32 * 0.05).sin()).collect();
        let mut ana = Analysis::new();
        let (mut overlap, mut v, mut off) = ([[0.0f32; 18]; 32], [0.0f32; 1024], 0usize);
        let mut out = Vec::new();
        for g in 0..n {
            let sb = ana.subbands(&input[g * GRANULE..(g + 1) * GRANULE]);
            let xr = ana.granule(sb, kinds[g]);
            let mut gr = Granule {
                xr: xr.iter().map(|&v| v as f64).collect(),
                block_type: kinds[g].block_type() as u8,
                ..Default::default()
            };
            super::super::antialias(s, &mut gr);
            let mut pcm = [0.0f32; GRANULE];
            granule_pcm(s, &gr, &mut overlap, &mut v, &mut off, &mut pcm);
            out.extend_from_slice(&pcm);
        }
        // find the delay and the gain
        let mut best = (f64::MAX, 0usize, 0.0f64);
        for d in 1000..1200 {
            let (mut num, mut den) = (0.0f64, 0.0f64);
            for i in 0..(n * GRANULE - d - 100) {
                num += out[i + d] as f64 * input[i] as f64;
                den += (input[i] as f64).powi(2);
            }
            let g = num / den;
            let err: f64 = (0..n * GRANULE - d - 100).map(|i| (out[i + d] as f64 - g * input[i] as f64).powi(2)).sum();
            if err < best.0 {
                best = (err, d, g);
            }
        }
        eprintln!("delay {} gain {} error {}", best.1, best.2, best.0);
        assert_eq!(best.1 as i64, LEAD_IN);
        assert!((best.2 - 1.0).abs() < 1e-3, "gain {}", best.2);
        assert!(best.0 < 1e-3);
    }

    fn run(rate: u32, channels: usize, kbps: u32, n: usize, make: impl Fn(usize, usize) -> f64) -> (Vec<f64>, Vec<f64>, Vec<Packet>) {
        let input: Vec<f64> = (0..n * channels).map(|i| make(i / channels, i % channels)).collect();
        let mut enc = Mp3Encoder::new(rate, channels, kbps * 1000).unwrap();
        let mut packets = Vec::new();
        for chunk in input.chunks(1000 * channels) {
            packets.extend(enc.encode(&Frame::Audio(AudioFrame { sample_rate: rate, channels: channels as u16, pts: 0, samples: chunk.to_vec() })).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        let mut dec = Mp3Decoder::new(&StreamParams::Audio { sample_rate: rate, channels: channels as u16 }).unwrap();
        let mut out = Vec::new();
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                if let Frame::Audio(a) = f {
                    out.extend(a.samples);
                }
            }
        }
        (input, out, packets)
    }

    fn snr(input: &[f64], out: &[f64], channels: usize) -> f64 {
        let skip = LEAD_IN as usize * channels;
        let n = input.len().min(out.len().saturating_sub(skip));
        let sig: f64 = input[..n].iter().map(|v| v * v).sum();
        let err: f64 = (0..n).map(|i| (out[i + skip] - input[i]).powi(2)).sum();
        10.0 * (sig / err.max(1e-30)).log10()
    }

    fn music(i: usize, c: usize) -> f64 {
        let t = i as f64 / 44100.0;
        let a = 0.3 * (2.0 * PI * 440.0 * t).sin() + 0.1 * (2.0 * PI * 3000.0 * t).sin() + 0.05 * (2.0 * PI * 9000.0 * t).sin();
        let noise = (((i as u64 * 2654435761) >> 7) % 1000) as f64 / 1000.0 - 0.5;
        a * if c == 0 { 1.0 } else { 0.8 } + 0.02 * noise
    }

    #[test]
    fn frames_are_the_size_the_header_says_and_decode_back() {
        let (input, out, packets) = run(44100, 2, 128, 44100, music);
        for p in &packets {
            let h = super::super::parse_header(&p.data).unwrap();
            assert_eq!(h.frame_len, p.data.len());
            assert_eq!((h.bitrate, h.sample_rate, h.mode), (128, 44100, 1));
        }
        assert_eq!(packets.len(), (44100 + LEAD_IN as usize).div_ceil(FRAME));
        let s = snr(&input, &out, 2);
        assert!(s > 14.0, "SNR {s}");
    }

    #[test]
    fn mono_and_other_rates_work() {
        for (rate, ch, kbps) in [(48000, 1, 64), (32000, 2, 96), (44100, 1, 32)] {
            let (input, out, packets) = run(rate, ch, kbps, rate as usize / 2, music);
            let s = snr(&input, &out, ch);
            assert!(s > 8.0, "{rate} Hz x{ch} at {kbps}k: SNR {s}");
            let bytes: usize = packets.iter().map(|p| p.data.len()).sum();
            let secs = packets.len() as f64 * 1152.0 / rate as f64;
            let rate_got = bytes as f64 * 8.0 / secs / 1000.0;
            assert!((rate_got - kbps as f64).abs() < 1.0, "{rate_got} kb/s for {kbps}");
        }
    }

    #[test]
    fn silence_and_full_scale_noise_do_not_break_the_stream() {
        let (_, out, _) = run(44100, 2, 128, 20000, |_, _| 0.0);
        assert!(out.iter().all(|v| v.abs() < 1e-6));
        let (input, out, _) = run(44100, 2, 192, 30000, |i, c| ((((i * 2 + c) as u64).wrapping_mul(6364136223846793005) >> 33) % 2001) as f64 / 1000.0 - 1.0);
        assert!(out.iter().all(|v| v.is_finite()));
        let _ = input;
    }

    #[test]
    fn rates_the_standard_lacks_are_refused() {
        assert!(Mp3Encoder::new(22050, 2, 128_000).is_err());
        assert!(Mp3Encoder::new(44100, 6, 128_000).is_err());
        assert_eq!(Mp3Encoder::snap(130), 128);
    }

    /// Error in the samples before an attack, with block switching on or off, and how many granules
    /// it made short.
    fn pre_echo(switching: bool, channels: usize) -> (f64, u64, f64) {
        let rate = 44100u32;
        let n = 40_000usize;
        let attack = 20_000usize;
        let make = |t: usize, c: usize| {
            let tone = 0.05 * (2.0 * PI * 440.0 * t as f64 / 44100.0 + c as f64).sin();
            let burst = if (attack..attack + 300).contains(&t) { 0.8 * ((((t * 2654435761 + c) % 65536) as f64 / 32768.0) - 1.0) } else { 0.0 };
            tone + burst
        };
        let input: Vec<f64> = (0..n * channels).map(|i| make(i / channels, i % channels)).collect();
        let mut enc = Mp3Encoder::new(rate, channels, 128_000).unwrap();
        enc.switching = switching;
        let mut packets = Vec::new();
        for chunk in input.chunks(1000 * channels) {
            packets.extend(enc.encode(&Frame::Audio(AudioFrame { sample_rate: rate, channels: channels as u16, pts: 0, samples: chunk.to_vec() })).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        let mut dec = Mp3Decoder::new(&StreamParams::Audio { sample_rate: rate, channels: channels as u16 }).unwrap();
        let mut out = Vec::new();
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                if let Frame::Audio(a) = f {
                    out.extend(a.samples);
                }
            }
        }
        let skip = LEAD_IN as usize * channels;
        // the last 150 samples are the filter bank's own spread, which no block type removes
        let err: f64 = (attack - 900..attack - 150).flat_map(|t| (0..channels).map(move |c| (t, c))).map(|(t, c)| (input[t * channels + c] - out[skip + t * channels + c]).powi(2)).sum();
        (err, enc.short_granules, snr(&input, &out, channels))
    }

    #[test]
    fn an_attack_gets_short_blocks_and_less_pre_echo() {
        for channels in [1usize, 2] {
            let (long_err, long_short, _) = pre_echo(false, channels);
            let (sw_err, sw_short, sw_snr) = pre_echo(true, channels);
            eprintln!("{channels} ch: pre-echo error {long_err:.4} (long only) vs {sw_err:.4} ({sw_short} short granules, {sw_snr:.1} dB)");
            assert_eq!(long_short, 0);
            assert!(sw_short >= 1);
            assert!(sw_err < long_err * 0.3, "{channels} ch: {sw_err} vs {long_err}");
        }
    }

    #[test]
    fn steady_music_stays_in_long_blocks() {
        let mut enc = Mp3Encoder::new(44100, 2, 128_000).unwrap();
        for chunk in (0..44100 * 2).map(|i| music(i / 2, i % 2)).collect::<Vec<_>>().chunks(2000) {
            enc.encode(&Frame::Audio(AudioFrame { sample_rate: 44100, channels: 2, pts: 0, samples: chunk.to_vec() })).unwrap();
        }
        // the onset from silence is the one attack
        assert!(enc.short_granules <= 1, "{} short granules", enc.short_granules);
    }
}
