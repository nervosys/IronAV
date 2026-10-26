//! A Vorbis I encoder: one long block size (2048), floor 1 with 24 posts, residue type 1 coded with
//! a two-dimensional lattice codebook, square-polar channel coupling for stereo, mono or stereo only.
//!
//! The headers are written by hand and then read back with the decoder's own parser, so the
//! decoder's idea of every table (codebooks, floor curve) is the one the encoder uses: the floor
//! curve a block's coefficients are divided by is the curve the decoder will multiply them by,
//! because the encoder decodes its own floor bits to get it.
//!
//! Rate control is a feedback loop on the floor level: a higher floor shrinks the residue, which
//! costs fewer bits and leaves more noise. There is no block switching, so an attack spreads its
//! noise over a 46 ms window, and no psychoacoustic model: the noise follows the signal's own
//! spectral envelope.

use super::{ilog, join_headers, slope, Bits, Setup};
use crate::opus::mdct::Dct4;
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, Result};

const N: usize = 2048;
const HALF: usize = N / 2;
/// Floor 1: values run 0..RANGE, each step is MULT table steps (about 1.1 dB).
const RANGE: i32 = 128;
const MULT: usize = 2;
const RANGE_BITS: u32 = 10;
/// Residue partition size and the largest lattice step either side of zero.
const PSIZE: usize = 16;
/// The lattice codebooks (books 2, 3, 4) reach this far either side of zero; a partition uses the
/// smallest that holds its largest value. The classes are 1 to 4 for these, and 0 for all zeros.
const LATTICES: [i32; 4] = [31, 7, 3, 1];
const LEVELS: i32 = 31;
const CLASSES: usize = 5;
/// Class words cover this many partitions each.
const CLASS_DIM: usize = 4;
/// How closely the quantiser's step follows each band's level: 1 keeps the same signal-to-noise
/// ratio everywhere, 0 the same noise level everywhere (the best for plain squared error).
const SHAPING: f64 = 0.6;

fn side(l: i32) -> usize {
    (2 * l + 1) as usize
}
const POSTS: [i32; 24] = [3, 6, 10, 15, 21, 28, 37, 48, 62, 80, 102, 130, 165, 210, 265, 330, 410, 500, 600, 700, 800, 900, 970, 1005];

// ---- bit writing and prefix codes ----------------------------------------------------------------

#[derive(Default)]
struct BitWriter {
    buf: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    /// The low `bits` (at most 32) of `v`, first bit first.
    fn put(&mut self, v: u32, bits: u32) {
        self.acc |= (v as u64 & ((1u64 << bits) - 1)) << self.n;
        self.n += bits;
        while self.n >= 8 {
            self.buf.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.buf.push(self.acc as u8);
        }
        self.buf
    }
}

/// Huffman code lengths for the weights (every entry gets at least one bit, the code is complete).
fn huffman_lengths(weights: &[f64]) -> Vec<u8> {
    let top = weights.iter().cloned().fold(0.0, f64::max);
    // (weight, members): merge the two lightest until one group is left, deepening its members
    let mut groups: Vec<(u64, Vec<usize>)> = weights.iter().enumerate().map(|(i, &w)| (((w / top) * 1e9).max(1.0) as u64, vec![i])).collect();
    let mut lens = vec![0u8; weights.len()];
    while groups.len() > 1 {
        groups.sort_by(|a, b| b.0.cmp(&a.0));
        let (a, b) = (groups.pop().unwrap(), groups.pop().unwrap());
        for &i in a.1.iter().chain(&b.1) {
            lens[i] += 1;
        }
        groups.push((a.0 + b.0, a.1.into_iter().chain(b.1).collect()));
    }
    lens
}

/// The decoder's code assignment: the first entry takes the all-zero code, each later one the
/// lowest free code of its length.
fn assign_codes(lens: &[u8]) -> Vec<u32> {
    let mut codes = vec![0u32; lens.len()];
    let mut exit = [0u32; 34];
    for (p, &l) in lens.iter().enumerate() {
        let len = l as usize;
        if p == 0 {
            for i in 0..len {
                exit[i + 1] = 1u32 << i;
            }
            continue;
        }
        let at = (1..=len).rev().find(|&i| exit[i] != 0).expect("a complete code leaves room");
        let code = exit[at];
        exit[at] = 0;
        for j in at + 1..=len {
            exit[j] = code + (1u32 << (j - 1));
        }
        codes[p] = code;
    }
    codes
}

struct Huff {
    lens: Vec<u8>,
    codes: Vec<u32>,
}

impl Huff {
    fn new(lens: Vec<u8>) -> Self {
        let codes = assign_codes(&lens);
        Huff { lens, codes }
    }
    fn put(&self, w: &mut BitWriter, entry: usize) {
        w.put(self.codes[entry], self.lens[entry] as u32);
    }
}

/// The code lengths of the five codebooks: floor values, residue classes, then the three lattices.
fn code_lengths() -> Vec<Vec<u8>> {
    let y: Vec<f64> = (0..RANGE).map(|v| (-(v as f64) / 2.5).exp() + 1e-4).collect();
    // class words: each partition's class is drawn from a fixed mix, and runs of one class are likely
    let mix = [0.3f64, 0.15, 0.15, 0.2, 0.2];
    let cls: Vec<f64> = (0..CLASSES.pow(CLASS_DIM as u32))
        .map(|e| {
            let digits: Vec<usize> = (0..CLASS_DIM).map(|i| (e / CLASSES.pow(i as u32)) % CLASSES).collect();
            let run = digits.iter().all(|&d| d == digits[0]);
            digits.iter().map(|&d| mix[d]).product::<f64>() * if run { 4.0 } else { 1.0 }
        })
        .collect();
    let mut out = vec![huffman_lengths(&y), huffman_lengths(&cls)];
    for (&l, scale) in LATTICES.iter().zip([6.0, 2.5, 1.6, 0.9]) {
        let sd = side(l);
        let w: Vec<f64> = (0..sd * sd)
            .map(|e| {
                let (a, b) = ((e % sd) as i32 - l, (e / sd) as i32 - l);
                (-((a.abs() + b.abs()) as f64) / scale).exp() + 1e-5
            })
            .collect();
        out.push(huffman_lengths(&w));
    }
    out
}

/// Vorbis's 32-bit float packing.
fn pack_float(v: f64) -> u32 {
    if v == 0.0 {
        return 0;
    }
    let e = v.abs().log2().floor() as i32;
    let mant = (v.abs() * 2f64.powi(20 - e)).round() as u32;
    (if v < 0.0 { 1 << 31 } else { 0 }) | (((e - 20 + 788) as u32) << 21) | mant
}

// ---- headers -------------------------------------------------------------------------------------

/// The three header packets, Xiph-laced the way containers carry them.
pub fn headers(rate: u32, channels: usize) -> Vec<u8> {
    let mut id = b"\x01vorbis".to_vec();
    id.extend(0u32.to_le_bytes());
    id.push(channels as u8);
    id.extend(rate.to_le_bytes());
    id.extend([0u8; 12]); // maximum, nominal and minimum bit rate: not stated
    id.push(8 | (11 << 4));
    id.push(1);

    let mut comment = b"\x03vorbis".to_vec();
    let vendor = b"IronAV";
    comment.extend((vendor.len() as u32).to_le_bytes());
    comment.extend(vendor);
    comment.extend(0u32.to_le_bytes());
    comment.push(1);

    let mut setup = b"\x05vorbis".to_vec();
    let mut w = BitWriter::default();
    let lens = code_lengths();
    w.put(lens.len() as u32 - 1, 8);
    for (i, l) in lens.iter().enumerate() {
        w.put(0x56_4342, 24);
        // dimensions: a floor value, CLASS_DIM partitions' classes, a pair of residue values
        w.put(if i == 0 { 1 } else if i == 1 { CLASS_DIM as u32 } else { 2 }, 16);
        w.put(l.len() as u32, 24);
        w.put(0, 2); // not ordered, not sparse
        for &len in l {
            w.put(len as u32 - 1, 5);
        }
        if i >= 2 {
            let lat = LATTICES[i - 2];
            let bits = ilog(2 * lat as u32);
            w.put(1, 4); // a lattice
            w.put(pack_float(-lat as f64), 32);
            w.put(pack_float(1.0), 32);
            w.put(bits - 1, 4);
            w.put(0, 1);
            for v in 0..side(lat) as u32 {
                w.put(v, bits);
            }
        } else {
            w.put(0, 4);
        }
    }
    w.put(0, 6); // one time-domain transform, the only one
    w.put(0, 16);
    w.put(0, 6); // one floor
    w.put(1, 16);
    w.put(POSTS.len() as u32, 5);
    for _ in POSTS {
        w.put(0, 4); // every partition is class 0
    }
    w.put(0, 3); // class 0 has dimension 1 …
    w.put(0, 2); // … no subclasses …
    w.put(1, 8); // … and its values come from codebook 0
    w.put((MULT - 1) as u32, 2);
    w.put(RANGE_BITS, 4);
    for x in bisection_order() {
        w.put(x as u32, RANGE_BITS);
    }
    w.put(0, 6); // one residue
    w.put(1, 16);
    w.put(0, 24);
    w.put(HALF as u32, 24);
    w.put(PSIZE as u32 - 1, 24);
    w.put(CLASSES as u32 - 1, 6);
    w.put(1, 8); // class words from codebook 1
    w.put(0, 3); // class 0: nothing coded
    w.put(0, 1);
    for k in 0..LATTICES.len() {
        w.put(1, 3); // classes 1 to 4: one pass each …
        w.put(0, 1);
        let _ = k;
    }
    for k in 0..LATTICES.len() {
        w.put(2 + k as u32, 8); // … with a lattice codebook
    }
    w.put(0, 6); // one mapping
    w.put(0, 16);
    w.put(0, 1); // one submap
    if channels == 2 {
        w.put(1, 1);
        w.put(0, 8); // one coupling step: channel 0 is the magnitude, 1 the angle
        w.put(0, 1);
        w.put(1, 1);
    } else {
        w.put(0, 1);
    }
    w.put(0, 2);
    w.put(0, 8);
    w.put(0, 8);
    w.put(0, 8);
    w.put(0, 6); // one mode: long blocks
    w.put(1, 1);
    w.put(0, 16);
    w.put(0, 16);
    w.put(0, 8);
    w.put(1, 1); // framing
    setup.extend(w.finish());
    join_headers([&id, &comment, &setup])
}

/// The posts in the order they are coded: each one's neighbours are already known and near, so the
/// prediction from them is good.
fn bisection_order() -> Vec<i32> {
    let mut todo: std::collections::VecDeque<(i32, i32)> = [(0, RANGE_X)].into();
    let mut out = Vec::new();
    while let Some((lo, hi)) = todo.pop_front() {
        let inside: Vec<i32> = POSTS.iter().copied().filter(|&p| p > lo && p < hi).collect();
        if inside.is_empty() {
            continue;
        }
        let mid = (lo + hi) / 2;
        let pick = *inside.iter().min_by_key(|&&p| (p - mid).abs()).unwrap();
        out.push(pick);
        todo.push_back((lo, pick));
        todo.push_back((pick, hi));
    }
    out
}
const RANGE_X: i32 = 1 << RANGE_BITS;

// ---- the encoder ---------------------------------------------------------------------------------

pub struct VorbisEncoder {
    channels: usize,
    setup: Setup,
    dct: Dct4,
    window: Vec<f32>,
    ybook: Huff,
    classbook: Huff,
    vq: Vec<Huff>,
    /// Posts as (x, index in the floor's own list), ascending by x, plus the coded order's list.
    xs: Vec<i32>,
    /// Bits a packet should take, and the floor level (dB above the signal's own envelope) that is steering to it.
    target_bits: f64,
    gain_db: f64,
    /// Unconsumed samples per channel, starting at the next block.
    buf: Vec<Vec<f32>>,
    packets: u64,
    input: u64,
}

impl VorbisEncoder {
    /// An encoder for `channels` (1 or 2) at about `bitrate` bits per second.
    pub fn new(sample_rate: u32, channels: usize, bitrate: u32) -> Result<Self> {
        if !(1..=2).contains(&channels) {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the Vorbis encoder takes mono or stereo audio, not {channels} channels")));
        }
        if !(2000..=200_000).contains(&sample_rate) {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the Vorbis encoder takes 2000 to 200000 Hz audio, not {sample_rate} Hz")));
        }
        let setup = Setup::parse(&headers(sample_rate, channels))?;
        let lens = code_lengths();
        let mut lens = lens.into_iter();
        let (y, c) = (lens.next().unwrap(), lens.next().unwrap());
        let mut window = vec![0f32; N];
        for (i, s) in slope(HALF).into_iter().enumerate() {
            window[i] = s;
            window[N - 1 - i] = s;
        }
        let xs = setup.floors[0].xs.clone();
        Ok(Self {
            channels,
            setup,
            dct: Dct4::new(HALF),
            window,
            ybook: Huff::new(y),
            classbook: Huff::new(c),
            vq: lens.map(Huff::new).collect(),
            xs,
            target_bits: bitrate as f64 * HALF as f64 / sample_rate as f64,
            gain_db: 6.0,
            buf: vec![vec![0.0; HALF]; channels],
            packets: 0,
            input: 0,
        })
    }

    pub fn default_bitrate(channels: usize) -> u32 {
        64_000 * channels as u32
    }

    /// The MDCT of one block (`N` samples): window, fold, DCT-IV.
    fn forward(&mut self, x: &[f32], out: &mut [f32]) {
        let q = HALF / 2;
        let y: Vec<f32> = x.iter().zip(&self.window).map(|(a, w)| a * w).collect();
        let mut c = vec![0f32; HALF];
        for i in 0..q {
            c[q + i] += y[i];
            c[i] -= y[q + HALF + i];
        }
        for i in 0..HALF {
            c[HALF - 1 - i] -= y[q + i];
        }
        self.dct.run(&c, 1, out);
        for v in out.iter_mut() {
            *v *= FORWARD_SCALE;
        }
    }

    /// Floor values for one channel's spectrum: the level of the local envelope, raised by the gain.
    fn floor_targets(&self, c: &[f32]) -> Vec<i32> {
        let g = 10f64.powf(self.gain_db / 20.0);
        // the step follows the local level only partway (see SHAPING): the rest is a level common to the whole block
        let alpha = SHAPING;
        let common = (c.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / c.len() as f64).sqrt().max(1e-9);
        let mut sorted: Vec<(i32, usize)> = self.xs.iter().copied().enumerate().map(|(i, x)| (x, i)).collect();
        sorted.sort();
        let mut t = vec![0i32; self.xs.len()];
        for (k, &(x, idx)) in sorted.iter().enumerate() {
            let lo = if k == 0 { 0 } else { (sorted[k - 1].0 + x) / 2 };
            let hi = if k + 1 == sorted.len() { HALF as i32 } else { (x + sorted[k + 1].0 + 1) / 2 };
            let band = &c[lo as usize..(hi as usize).clamp(lo as usize + 1, HALF)];
            let rms = (band.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / band.len() as f64).sqrt();
            // a lone peak must still fit the lattice: the level is never under peak / LEVELS
            let peak = band.iter().fold(0f32, |m, v| m.max(v.abs())) as f64;
            let step = rms.max(1e-12).powf(alpha) * common.powf(1.0 - alpha);
            let want = (step * g).max(peak / (LEVELS as f64 - 0.5)) as f32;
            // the lowest value whose gain reaches it, or the one just below if that is nearer
            let table = &super::FLOOR1_INVERSE_DB;
            let mut y = 0usize;
            while y + 1 < RANGE as usize && table[y * MULT] < want {
                y += 1;
            }
            if y > 0 && (want / table[(y - 1) * MULT]) < (table[y * MULT] / want) {
                y -= 1;
            }
            t[idx] = y as i32;
        }
        t
    }

    /// The (value, bits) pairs that code `targets` as a floor, and the values the decoder will end up with.
    fn floor_symbols(&self, targets: &[i32]) -> Vec<(u32, u32)> {
        let f = &self.setup.floors[0];
        let n = f.xs.len();
        let rb = ilog(RANGE as u32 - 1);
        let mut fin = vec![0i32; n];
        let mut sym = vec![(1u32, 1u32)];
        fin[0] = targets[0].clamp(0, RANGE - 1);
        fin[1] = targets[1].clamp(0, RANGE - 1);
        sym.push((fin[0] as u32, rb));
        sym.push((fin[1] as u32, rb));
        for i in 2..n {
            let (lo, hi) = (f.low[i], f.high[i]);
            let dy = fin[hi] - fin[lo];
            let adx = f.xs[hi] - f.xs[lo];
            let off = dy.abs() * (f.xs[i] - f.xs[lo]) / adx;
            let predicted = if dy < 0 { fin[lo] - off } else { fin[lo] + off };
            let t = targets[i].clamp(0, RANGE - 1);
            let (highroom, lowroom) = (RANGE - predicted, predicted);
            let room = highroom.min(lowroom) * 2;
            let diff = t - predicted;
            let val = if diff == 0 {
                0
            } else if diff > 0 {
                if 2 * diff < room { 2 * diff } else { diff + lowroom }
            } else if 2 * -diff - 1 < room {
                2 * -diff - 1
            } else {
                -diff + highroom - 1
            };
            fin[i] = t;
            sym.push((self.ybook.codes[val as usize], self.ybook.lens[val as usize] as u32));
        }
        sym
    }

    fn encode_block(&mut self, blocks: &[Vec<f32>], adapt: bool) -> Vec<u8> {
        let ch = self.channels;
        let mut w = BitWriter::default();
        w.put(0, 1); // an audio packet
        w.put(3, 2); // a long block between long blocks (the mode needs no bits: there is one)

        let mut spec = vec![vec![0f32; HALF]; ch];
        for (c, b) in blocks.iter().enumerate() {
            let mut out = vec![0f32; HALF];
            self.forward(b, &mut out);
            spec[c] = out;
        }
        let mut res = vec![vec![0f32; HALF]; ch];
        for c in 0..ch {
            let mut targets = self.floor_targets(&spec[c]);
            let mut sym;
            let mut curve = vec![0f32; HALF];
            // the curve runs straight between posts, so it can dip under a peak that sits between two:
            // raise the posts around any bin the lattice could not reach, and look again
            for pass in 0..6 {
                sym = self.floor_symbols(&targets);
                let mut fw = BitWriter::default();
                for &(v, b) in &sym {
                    fw.put(v, b);
                }
                let bytes = fw.finish();
                self.setup.floors[0].decode(&self.setup.books, &mut Bits::new(&bytes), &mut curve);
                let mut raised = false;
                for k in 0..HALF {
                    let ratio = spec[c][k].abs() / curve[k];
                    if ratio > LEVELS as f32 - 0.5 {
                        let steps = ((ratio / (LEVELS as f32 - 0.5)).log10() * 20.0 / 1.1).ceil() as i32 + 1;
                        let hi = self.xs.iter().enumerate().filter(|&(_, &x)| x > k as i32).min_by_key(|&(_, &x)| x).map(|(i, _)| i);
                        let lo = self.xs.iter().enumerate().filter(|&(_, &x)| x <= k as i32).max_by_key(|&(_, &x)| x).map(|(i, _)| i);
                        for i in [lo, hi].into_iter().flatten() {
                            targets[i] = (targets[i] + steps).min(RANGE - 1);
                        }
                        raised = true;
                    }
                }
                if !raised || pass == 5 {
                    for &(v, b) in &sym {
                        w.put(v, b);
                    }
                    break;
                }
            }
            for k in 0..HALF {
                res[c][k] = spec[c][k] / curve[k];
            }
        }
        if ch == 2 {
            let (a, b) = res.split_at_mut(1);
            for (l, r) in a[0].iter_mut().zip(b[0].iter_mut()) {
                let (lv, rv) = (*l, *r);
                // magnitude and angle: the larger channel, and the difference signed to match
                let (m, ang) = if lv.abs() > rv.abs() {
                    (lv, if lv > 0.0 { lv - rv } else { rv - lv })
                } else {
                    (rv, if rv > 0.0 { lv - rv } else { rv - lv })
                };
                *l = m;
                *r = ang;
            }
        }
        // lattice points, and which partitions have anything in them
        let q: Vec<Vec<i32>> = res
            .iter()
            .map(|r| r.iter().map(|&v| ((v.abs() + 0.45).floor().min(LEVELS as f32) as i32) * if v < 0.0 { -1 } else { 1 }).collect())
            .collect();
        let mut q = q;
        if ch == 2 {
            // Decoders disagree about a coupled pair whose magnitude is exactly zero and whose angle
            // is not (the specification says "not positive", some FFmpeg builds said "positive"), so
            // never send one: drop the angle, a step of at most about one lattice unit
            let (m, a) = q.split_at_mut(1);
            for (mv, av) in m[0].iter().zip(a[0].iter_mut()) {
                if *mv == 0 {
                    *av = 0;
                }
            }
        }
        let parts = HALF / PSIZE;
        let class: Vec<Vec<usize>> = q
            .iter()
            .map(|v| {
                (0..parts)
                    .map(|p| {
                        let m = v[p * PSIZE..(p + 1) * PSIZE].iter().map(|x| x.abs()).max().unwrap_or(0);
                        if m == 0 { 0 } else { 1 + LATTICES.iter().rposition(|&l| m <= l).unwrap() }
                    })
                    .collect()
            })
            .collect();
        for g in 0..parts / CLASS_DIM {
            for cl in class.iter() {
                let word = (0..CLASS_DIM).fold(0, |acc, i| acc * CLASSES + cl[g * CLASS_DIM + i]);
                self.classbook.put(&mut w, word);
            }
            for p in g * CLASS_DIM..(g + 1) * CLASS_DIM {
                for c in 0..ch {
                    if class[c][p] == 0 {
                        continue;
                    }
                    let lat = LATTICES[class[c][p] - 1];
                    for pair in q[c][p * PSIZE..(p + 1) * PSIZE].chunks(2) {
                        let entry = (pair[0] + lat) as usize + side(lat) * (pair[1] + lat) as usize;
                        self.vq[class[c][p] - 1].put(&mut w, entry);
                    }
                }
            }
        }
        let bytes = w.finish();
        if !adapt {
            return bytes;
        }
        // steer the floor level toward the bit budget
        let err = (bytes.len() as f64 * 8.0 / self.target_bits).ln();
        self.gain_db = (self.gain_db + (err * 4.0).clamp(-1.5, 1.5)).clamp(-24.0, 40.0);
        bytes
    }

    fn emit(&mut self, out: &mut Vec<Packet>, duration: i64) {
        let blocks: Vec<Vec<f32>> = self.buf.iter().map(|b| b[..N].to_vec()).collect();
        if self.packets < 2 && blocks.iter().any(|b| b.iter().any(|&v| v != 0.0)) {
            // the start has no history for the rate loop to follow: find the gain that meets the budget
            let (mut lo, mut hi) = (-24.0f64, 40.0f64);
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
        // the first packet decodes to nothing; the second to the first HALF samples
        let pts = self.packets.saturating_sub(1) as i64 * HALF as i64;
        out.push(Packet { stream: 0, pts, duration, data });
        self.packets += 1;
        for b in self.buf.iter_mut() {
            b.drain(..HALF);
        }
    }
}

/// Maps the spectrum to the scale the floor table expects, and is the inverse of the decoder's transform.
const FORWARD_SCALE: f32 = 1.0 / 512.0;

impl Encoder for VorbisEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid_input("the Vorbis encoder received a video frame"));
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
            let d = if self.packets == 0 { 0 } else { HALF as i64 };
            self.emit(&mut out, d);
        }
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        let mut out = Vec::new();
        if self.input == 0 {
            return Ok(out);
        }
        // one packet more than the blocks of HALF samples the input fills: the first decodes to nothing
        let wanted = (self.input as usize).div_ceil(HALF) as u64 + 1;
        while self.packets < wanted {
            while self.buf[0].len() < N {
                for b in self.buf.iter_mut() {
                    b.push(0.0);
                }
            }
            let last = self.packets + 1 == wanted;
            let d = if self.packets == 0 {
                0
            } else if last {
                (self.input as usize - (wanted as usize - 2) * HALF) as i64
            } else {
                HALF as i64
            };
            self.emit(&mut out, d);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vorbis::VorbisDecoder;
    use ironav_core::{AudioFrame, Decoder, StreamParams};

    #[test]
    fn huffman_codes_are_complete_and_prefix_free() {
        for lens in code_lengths() {
            let kraft: f64 = lens.iter().map(|&l| 0.5f64.powi(l as i32)).sum();
            assert!((kraft - 1.0).abs() < 1e-9, "{kraft}");
            assert!(lens.iter().all(|&l| (1..=32).contains(&l)));
        }
    }

    #[test]
    fn dct4_twice_scales_by_a_constant() {
        let mut d = Dct4::new(HALF);
        let v: Vec<f32> = (0..HALF).map(|i| ((i * 37) % 11) as f32 - 5.0).collect();
        let (mut a, mut b) = (vec![0f32; HALF], vec![0f32; HALF]);
        d.run(&v, 1, &mut a);
        d.run(&a, 1, &mut b);
        eprintln!("k = {}", b[3] / v[3]);
    }

    #[test]
    fn our_headers_parse() {
        for ch in [1, 2] {
            Setup::parse(&headers(44_100, ch)).unwrap();
        }
    }

    #[test]
    fn float_packing_inverts_unpacking() {
        for v in [1.0, -7.0, 0.5, 123.25] {
            assert_eq!(super::super::unpack_float(pack_float(v)) as f64, v);
        }
    }

    fn run(channels: usize, kbps: u32, n: usize, make: impl Fn(usize, usize) -> f64) -> (Vec<f64>, Vec<f64>, usize) {
        let input: Vec<f64> = (0..n * channels).map(|i| make(i / channels, i % channels)).collect();
        let mut enc = VorbisEncoder::new(44_100, channels, kbps * 1000).unwrap();
        let mut packets = Vec::new();
        for chunk in input.chunks(1000 * channels) {
            packets.extend(enc.encode(&Frame::Audio(AudioFrame { sample_rate: 44_100, channels: channels as u16, pts: 0, samples: chunk.to_vec() })).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        let bytes: usize = packets.iter().map(|p| p.data.len()).sum();
        let params = StreamParams::Audio { sample_rate: 44_100, channels: channels as u16 };
        let mut dec = VorbisDecoder::new(&headers(44_100, channels), &params).unwrap();
        let mut out = Vec::new();
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                let Frame::Audio(a) = f else { panic!() };
                out.extend(a.samples);
            }
        }
        let total: i64 = packets.iter().map(|p| p.duration).sum();
        assert_eq!(total as usize, n, "durations add up to the input");
        (input, out, bytes)
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
    fn every_entry_reads_back_through_the_decoders_codebook() {
        let setup = Setup::parse(&headers(44_100, 1)).unwrap();
        let lens = code_lengths();
        for (b, l) in lens.iter().enumerate() {
            let h = Huff::new(l.clone());
            let mut w = BitWriter::default();
            for e in 0..l.len() {
                h.put(&mut w, e);
            }
            let bytes = w.finish();
            let mut bits = Bits::new(&bytes);
            for e in 0..l.len() {
                assert_eq!(setup.books[b].read(&mut bits), Some(e), "book {b} entry {e}");
            }
        }
        // the lattice books: entry 0 is the far corner, and (a, b) sits at a + l + side * (b + l)
        for (k, &l) in LATTICES.iter().enumerate() {
            let v = &setup.books[2 + k].vectors;
            let sd = side(l);
            assert_eq!(&v[0..2], &[-l as f32, -l as f32]);
            let at = 2 * ((l - 1) as usize + sd * (l + 1) as usize);
            assert_eq!(&v[at..at + 2], &[-1.0, 1.0]);
        }
    }

    #[test]
    fn the_forward_transform_is_inverted_by_the_decoders() {
        let mut e = VorbisEncoder::new(44_100, 1, 64_000).unwrap();
        let x: Vec<f32> = (0..HALF * 6).map(|i| ((i * 7919) % 1000) as f32 / 500.0 - 1.0 + (i as f32 * 0.05).sin()).collect();
        let mut d = Dct4::new(HALF);
        let mut rec = vec![0f32; x.len() + N];
        for j in 0..5 {
            let mut c = vec![0f32; HALF];
            e.forward(&x[j * HALF..j * HALF + N], &mut c);
            let mut t = vec![0f32; HALF];
            d.run(&c, 1, &mut t);
            let q = HALF / 2;
            let mut y = vec![0f32; N];
            y[..q].copy_from_slice(&t[q..HALF]);
            for (o, &v) in y[q..q + HALF].iter_mut().zip(t.iter().rev()) {
                *o = -v;
            }
            for (o, &v) in y[q + HALF..].iter_mut().zip(&t[..q]) {
                *o = -v;
            }
            for i in 0..N {
                rec[j * HALF + i] += y[i] * e.window[i];
            }
        }
        for i in HALF..HALF * 5 {
            assert!((rec[i] - x[i]).abs() < 1e-3, "sample {i}: {} vs {}", rec[i], x[i]);
        }
    }

    #[test]
    #[ignore]
    fn bitrate_sweep() {
        for (name, f) in [("sine", (|t: usize, _| 0.5 * (2.0 * std::f64::consts::PI * 440.0 * t as f64 / 44_100.0).sin()) as fn(usize, usize) -> f64), ("music", music)] {
            for kbps in [24u32, 48, 96, 192, 320] {
                let (input, out, bytes) = run(1, kbps, 44_100 * 3, f);
                eprintln!("{name} {kbps}: {:.1} dB at {} kb/s", snr(&input, &out[..input.len()]), bytes * 8 / 3000);
            }
        }
    }

    #[test]
    #[ignore]
    fn noise_efficiency() {
        let mut x = 12345u64;
        let mut gauss = move || {
            let mut u = || {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((x >> 11) as f64 + 1.0) / (1u64 << 53) as f64
            };
            0.1 * (-2.0 * u().ln()).sqrt() * (2.0 * std::f64::consts::PI * u()).cos()
        };
        let noise: Vec<f64> = (0..44_100 * 3).map(|_| gauss()).collect();
        for kbps in [64u32, 96, 128, 192, 256, 384] {
            let (input, out, bytes) = run(1, kbps, noise.len(), |t, _| noise[t]);
            let bits = bytes as f64 * 8.0 / noise.len() as f64;
            eprintln!("noise {kbps}: {:.1} dB at {:.2} bits/sample (Gaussian bound {:.1} dB)", snr(&input, &out[..input.len()]), bits, 6.02 * bits);
        }
    }

    #[test]
    fn a_tone_comes_back_close() {
        for (ch, kbps) in [(1usize, 64u32), (2, 128)] {
            let n = 44_100 * 2;
            let (input, out, bytes) = run(ch, kbps, n, music);
            assert!(out.len() >= n * ch, "{} < {}", out.len(), n * ch);
            let db = snr(&input, &out[..n * ch]);
            eprintln!("{ch} ch {kbps} kb/s: {db:.1} dB, {} kb/s actual", bytes * 8 / 2000);
            assert!(db > 12.0, "{db}");
        }
    }

    /// Extreme signals and tiny inputs: every one decodes, the lengths add up, nothing is non-finite.
    #[test]
    fn stress() {
        let sizes = [1usize, 1025, 5000];
        let makers: Vec<(&str, Box<dyn Fn(usize, usize) -> f64>)> = vec![
            ("silence", Box::new(|_, _| 0.0)),
            ("full-scale square", Box::new(|t, _| if (t / 40) % 2 == 0 { 1.0 } else { -1.0 })),
            ("clicks", Box::new(|t, c| if t % 3001 == 0 { 1.0 - c as f64 * 2.0 } else { 0.0 })),
            ("noise", Box::new(|t, c| (((t * 2654435761 + c * 97) % 65536) as f64 / 32768.0 - 1.0) * 0.9)),
            ("dc", Box::new(|_, _| 0.7)),
        ];
        for (name, f) in &makers {
            for &n in &sizes {
                for (ch, kbps) in [(1usize, 16u32), (2, 320)] {
                    {
                        let (_, out, _) = run(ch, kbps, n, f);
                        assert!(out.len() >= n * ch, "{name} {n} {ch}ch {kbps}k: {} < {}", out.len(), n * ch);
                        assert!(out.iter().all(|v| v.is_finite() && v.abs() <= 1.0), "{name}");
                    }
                }
            }
        }
        // no input, no packets
        let mut e = VorbisEncoder::new(44_100, 2, 128_000).unwrap();
        assert!(e.flush().unwrap().is_empty());
    }
}
