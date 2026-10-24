//! FLAC encoder. Per block it tries constant / verbatim / fixed (orders 0-4) / LPC subframes,
//! picks the cheapest Rice partitioning for each, and for stereo chooses the best of
//! left-right, left-side, side-right and mid-side. Sizes are estimated with the standard
//! Rice cost model and the winner is then written exactly.

use super::bits::BitWriter;
use super::crc::{crc16, crc8};
use super::header::write_utf8_number;
use ironav_core::{Encoder, Error, Frame, Packet, Result};

const BLOCK_SIZE: usize = 4096;
const MAX_LPC_ORDER: usize = 12;
const LPC_ORDERS: [usize; 3] = [4, 8, 12];
const MAX_PARTITION_ORDER: u32 = 8;
const LPC_PRECISION: u32 = 15;

struct Plan {
    method: u32,
    porder: u32,
    params: Vec<u32>,
    bits: u64,
}

enum Kind {
    Constant(i64),
    Verbatim,
    Fixed(usize),
    Lpc { order: usize, shift: u32, coefs: Vec<i64> },
}

struct Sub {
    bits: u64,
    kind: Kind,
    residual: Vec<i64>,
    plan: Option<Plan>,
}

/// A signal together with its chosen subframe encoding and coded bit depth.
struct Coded {
    sig: Vec<i64>,
    sub: Sub,
    bps: u32,
}

/// Analyse each signal and keep it with its chosen encoding.
///
/// Deliberately sequential: spawning threads per 4096-sample block measured no better than
/// sequential at the median and was far less predictable on a loaded machine.
fn code_all(signals: Vec<(Vec<i64>, u32)>) -> Vec<Coded> {
    signals
        .into_iter()
        .map(|(sig, bps)| {
            let sub = analyze(&sig, bps);
            Coded { sig, sub, bps }
        })
        .collect()
}

#[inline]
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// Best Rice parameter and estimated cost for `n` values whose zigzag sum is `sum`.
fn best_k(sum: u64, n: usize) -> (u32, u64) {
    if n == 0 {
        return (0, 0);
    }
    let mut best = (0u32, u64::MAX);
    for k in 0..=30u32 {
        let cost = n as u64 * (k as u64 + 1) + (sum >> k);
        if cost < best.1 {
            best = (k, cost);
        }
    }
    best
}

/// Choose the partition order and Rice parameters for residuals `res` (which follow `order`
/// warm-up samples in a block of `n` samples).
fn plan_residual(res: &[i64], n: usize, order: usize) -> Plan {
    let mut max_p = 0u32;
    while max_p < MAX_PARTITION_ORDER && n % (1 << (max_p + 1)) == 0 && (n >> (max_p + 1)) >= 8.max(order) {
        max_p += 1;
    }
    // Sums of zigzag residuals at the finest partitioning, then merged pairwise for coarser ones.
    let parts = 1usize << max_p;
    let psize = n >> max_p;
    let mut level: Vec<(u64, usize)> = (0..parts)
        .map(|j| {
            let lo = (j * psize).saturating_sub(order);
            let hi = ((j + 1) * psize).saturating_sub(order);
            (res[lo..hi].iter().map(|&v| zigzag(v)).sum(), hi - lo)
        })
        .collect();
    let mut best: Option<Plan> = None;
    for p in (0..=max_p).rev() {
        let ks: Vec<(u32, u64)> = level.iter().map(|&(s, c)| best_k(s, c)).collect();
        let bits = 6 + ks.iter().map(|&(_, c)| 4 + c).sum::<u64>();
        if best.as_ref().is_none_or(|b| bits < b.bits) {
            let params: Vec<u32> = ks.iter().map(|&(k, _)| k).collect();
            let method = if params.iter().any(|&k| k > 14) { 1 } else { 0 };
            best = Some(Plan { method, porder: p, params, bits });
        }
        if p > 0 {
            level = level.chunks(2).map(|c| (c[0].0 + c[1].0, c[0].1 + c[1].1)).collect();
        }
    }
    best.expect("at least one partition order")
}

fn fixed_residual(s: &[i64], order: usize) -> Vec<i64> {
    let n = s.len();
    (order..n)
        .map(|i| {
            let pred = match order {
                0 => 0,
                1 => s[i - 1],
                2 => 2 * s[i - 1] - s[i - 2],
                3 => 3 * s[i - 1] - 3 * s[i - 2] + s[i - 3],
                _ => 4 * s[i - 1] - 6 * s[i - 2] + 4 * s[i - 3] - s[i - 4],
            };
            s[i] - pred
        })
        .collect()
}

/// Levinson-Durbin on a Tukey-windowed autocorrelation; returns coefficients for each order 1..=max.
fn lpc_coefficients(s: &[i64], max_order: usize) -> Option<Vec<Vec<f64>>> {
    let n = s.len();
    let x: Vec<f64> = s
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let t = i as f64 / (n - 1) as f64;
            let w = if t < 0.25 {
                0.5 - 0.5 * (4.0 * std::f64::consts::PI * t).cos()
            } else if t > 0.75 {
                0.5 - 0.5 * (4.0 * std::f64::consts::PI * (1.0 - t)).cos()
            } else {
                1.0
            };
            v as f64 * w
        })
        .collect();
    let mut r = vec![0f64; max_order + 1];
    for (lag, slot) in r.iter_mut().enumerate() {
        *slot = x[lag..].iter().zip(&x).map(|(a, b)| a * b).sum();
    }
    if r[0] <= 0.0 {
        return None;
    }
    r[0] *= 1.0 + 1e-9;
    let mut a = vec![0f64; max_order];
    let mut err = r[0];
    let mut all = Vec::with_capacity(max_order);
    for i in 0..max_order {
        let mut acc = r[i + 1];
        for j in 0..i {
            acc -= a[j] * r[i - j];
        }
        let k = acc / err;
        let prev = a.clone();
        for j in 0..i {
            a[j] = prev[j] - k * prev[i - 1 - j];
        }
        a[i] = k;
        err *= 1.0 - k * k;
        if !(err > 0.0) || !k.is_finite() {
            break;
        }
        all.push(a[..=i].to_vec());
    }
    (!all.is_empty()).then_some(all)
}

fn quantize(coefs: &[f64]) -> Option<(Vec<i64>, u32)> {
    let cmax = coefs.iter().fold(0f64, |m, c| m.max(c.abs()));
    if cmax <= 0.0 || !cmax.is_finite() {
        return None;
    }
    let shift = ((LPC_PRECISION as i32 - 1) - cmax.log2().ceil() as i32).clamp(0, 15) as u32;
    let limit = (1i64 << (LPC_PRECISION - 1)) - 1;
    let scale = (1i64 << shift) as f64;
    let mut err = 0f64;
    let q = coefs
        .iter()
        .map(|&c| {
            let v = c * scale + err;
            let q = (v.round() as i64).clamp(-limit - 1, limit);
            err = v - q as f64;
            q
        })
        .collect();
    Some((q, shift))
}

fn analyze(s: &[i64], bps: u32) -> Sub {
    let n = s.len();
    if s.iter().all(|&v| v == s[0]) {
        return Sub { bits: 8 + bps as u64, kind: Kind::Constant(s[0]), residual: Vec::new(), plan: None };
    }
    let mut best = Sub { bits: 8 + n as u64 * bps as u64, kind: Kind::Verbatim, residual: Vec::new(), plan: None };

    for order in 0..=4usize.min(n.saturating_sub(1)) {
        let res = fixed_residual(s, order);
        let plan = plan_residual(&res, n, order);
        let bits = 8 + order as u64 * bps as u64 + plan.bits;
        if bits < best.bits {
            best = Sub { bits, kind: Kind::Fixed(order), residual: res, plan: Some(plan) };
        }
    }

    if n >= 64 {
        if let Some(all) = lpc_coefficients(s, MAX_LPC_ORDER.min(n / 4)) {
            for &order in LPC_ORDERS.iter().filter(|&&o| o <= all.len()) {
                let Some((coefs, shift)) = quantize(&all[order - 1]) else { continue };
                let res: Vec<i64> = (order..n)
                    .map(|i| {
                        let mut sum = 0i64;
                        for (j, &c) in coefs.iter().enumerate() {
                            sum += c * s[i - 1 - j];
                        }
                        s[i] - (sum >> shift)
                    })
                    .collect();
                let plan = plan_residual(&res, n, order);
                let bits = 8 + order as u64 * bps as u64 + 4 + 5 + order as u64 * LPC_PRECISION as u64 + plan.bits;
                if bits < best.bits {
                    best = Sub { bits, kind: Kind::Lpc { order, shift, coefs }, residual: res, plan: Some(plan) };
                }
            }
        }
    }
    best
}

fn write_subframe(w: &mut BitWriter, s: &[i64], sub: &Sub, bps: u32) {
    w.write(0, 1);
    match &sub.kind {
        Kind::Constant(v) => {
            w.write(0, 7);
            w.write_signed(*v, bps);
        }
        Kind::Verbatim => {
            w.write(1 << 1, 7);
            for &v in s {
                w.write_signed(v, bps);
            }
        }
        Kind::Fixed(order) => {
            w.write((8 + *order as u64) << 1, 7);
            for &v in &s[..*order] {
                w.write_signed(v, bps);
            }
            write_residual(w, sub);
        }
        Kind::Lpc { order, shift, coefs } => {
            w.write((31 + *order as u64) << 1, 7);
            for &v in &s[..*order] {
                w.write_signed(v, bps);
            }
            w.write(LPC_PRECISION as u64 - 1, 4);
            w.write(*shift as u64, 5);
            for &c in coefs {
                w.write_signed(c, LPC_PRECISION);
            }
            write_residual(w, sub);
        }
    }
}

fn write_residual(w: &mut BitWriter, sub: &Sub) {
    let plan = sub.plan.as_ref().expect("predicted subframes have a plan");
    let param_bits = 4 + plan.method;
    w.write(plan.method as u64, 2);
    w.write(plan.porder as u64, 4);
    let parts = 1usize << plan.porder;
    let order = match &sub.kind {
        Kind::Fixed(o) => *o,
        Kind::Lpc { order, .. } => *order,
        _ => 0,
    };
    let n = sub.residual.len() + order;
    let psize = n >> plan.porder;
    let mut idx = 0;
    for p in 0..parts {
        let count = if p == 0 { psize - order } else { psize };
        let k = plan.params[p];
        w.write(k as u64, param_bits);
        for &v in &sub.residual[idx..idx + count] {
            let u = zigzag(v);
            w.write_unary(u >> k);
            if k > 0 {
                w.write(u & ((1u64 << k) - 1), k);
            }
        }
        idx += count;
    }
}

fn block_size_code(n: usize) -> (u8, Vec<u8>) {
    match n {
        192 => (1, vec![]),
        576 => (2, vec![]),
        1152 => (3, vec![]),
        2304 => (4, vec![]),
        4608 => (5, vec![]),
        256 => (8, vec![]),
        512 => (9, vec![]),
        1024 => (10, vec![]),
        2048 => (11, vec![]),
        4096 => (12, vec![]),
        8192 => (13, vec![]),
        16384 => (14, vec![]),
        32768 => (15, vec![]),
        n if n <= 256 => (6, vec![(n - 1) as u8]),
        n => (7, ((n - 1) as u16).to_be_bytes().to_vec()),
    }
}

fn sample_rate_code(rate: u32) -> (u8, Vec<u8>) {
    match rate {
        88_200 => (1, vec![]),
        176_400 => (2, vec![]),
        192_000 => (3, vec![]),
        8_000 => (4, vec![]),
        16_000 => (5, vec![]),
        22_050 => (6, vec![]),
        24_000 => (7, vec![]),
        32_000 => (8, vec![]),
        44_100 => (9, vec![]),
        48_000 => (10, vec![]),
        96_000 => (11, vec![]),
        r if r % 1000 == 0 && r / 1000 <= 255 => (12, vec![(r / 1000) as u8]),
        r if r <= 65_535 => (13, (r as u16).to_be_bytes().to_vec()),
        r if r % 10 == 0 && r / 10 <= 65_535 => (14, ((r / 10) as u16).to_be_bytes().to_vec()),
        _ => (0, vec![]), // defer to STREAMINFO
    }
}

fn sample_size_code(bits: u32) -> u8 {
    match bits {
        8 => 1,
        12 => 2,
        16 => 4,
        20 => 5,
        24 => 6,
        _ => 0,
    }
}

pub struct FlacEncoder {
    sample_rate: u32,
    channels: usize,
    bits: u32,
    pending: Vec<Vec<i64>>,
    frame_no: u64,
    emitted: i64,
}

impl FlacEncoder {
    pub fn new(sample_rate: u32, channels: u16, bits: Option<u8>) -> Result<Self> {
        let bits = bits.unwrap_or(16) as u32;
        if ![8, 12, 16, 20, 24].contains(&bits) {
            return Err(Error::invalid_arg(format!("the FLAC encoder supports 8, 12, 16, 20 or 24 bits per sample, not {bits}")));
        }
        if !(1..=8).contains(&channels) {
            return Err(Error::invalid_arg("FLAC holds 1 to 8 channels"));
        }
        if sample_rate == 0 || sample_rate > 655_350 {
            return Err(Error::invalid_arg("FLAC sample rate must be 1..=655350 Hz"));
        }
        Ok(Self { sample_rate, channels: channels as usize, bits, pending: vec![Vec::new(); channels as usize], frame_no: 0, emitted: 0 })
    }

    fn encode_block(&mut self, n: usize) -> Packet {
        let chans: Vec<Vec<i64>> = self.pending.iter_mut().map(|c| c.drain(..n).collect()).collect();
        let bps = self.bits;
        // Channel-assignment code and the signals each subframe codes.
        let (assignment, coded): (u8, Vec<Coded>) = if self.channels == 2 {
            let side: Vec<i64> = chans[0].iter().zip(&chans[1]).map(|(a, b)| a - b).collect();
            let mid: Vec<i64> = chans[0].iter().zip(&chans[1]).map(|(a, b)| (a + b) >> 1).collect();
            let mut it = chans.into_iter();
            let (l, r) = (it.next().unwrap(), it.next().unwrap());
            let mut c = code_all(vec![(l, bps), (r, bps), (side, bps + 1), (mid, bps)]).into_iter().map(Some).collect::<Vec<_>>();
            let c: &mut [Option<Coded>; 4] = c.as_mut_slice().try_into().expect("four signals");
            let cost = |c: &[Option<Coded>; 4], i: usize| c[i].as_ref().unwrap().sub.bits;
            let costs = [cost(c, 0) + cost(c, 1), cost(c, 0) + cost(c, 2), cost(c, 2) + cost(c, 1), cost(c, 3) + cost(c, 2)];
            let best = (0..4).min_by_key(|&i| costs[i]).unwrap();
            let (code_id, picks): (u8, [usize; 2]) = [(1, [0, 1]), (8, [0, 2]), (9, [2, 1]), (10, [3, 2])][best];
            (code_id, picks.iter().map(|&i| c[i].take().unwrap()).collect())
        } else {
            (self.channels as u8 - 1, code_all(chans.into_iter().map(|c| (c, bps)).collect()))
        };

        let mut frame = Vec::with_capacity(n * self.channels * 2);
        let (bs_code, bs_extra) = block_size_code(n);
        let (sr_code, sr_extra) = sample_rate_code(self.sample_rate);
        frame.extend_from_slice(&[0xFF, 0xF8, bs_code << 4 | sr_code, assignment << 4 | sample_size_code(bps) << 1]);
        write_utf8_number(&mut frame, self.frame_no);
        frame.extend_from_slice(&bs_extra);
        frame.extend_from_slice(&sr_extra);
        frame.push(crc8(&frame));

        let mut w = BitWriter::new();
        for c in &coded {
            write_subframe(&mut w, &c.sig, &c.sub, c.bps);
        }
        w.align();
        frame.extend_from_slice(&w.bytes);
        frame.extend_from_slice(&crc16(&frame).to_be_bytes());

        let pkt = Packet { stream: 0, pts: self.emitted, duration: n as i64, data: frame };
        self.emitted += n as i64;
        self.frame_no += 1;
        pkt
    }
}

impl Encoder for FlacEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid_input("FLAC encoder received a video frame"));
        };
        if a.channels as usize != self.channels {
            return Err(Error::invalid_input("audio frame channel count changed mid-stream"));
        }
        let scale = (1i64 << (self.bits - 1)) as f64;
        for (i, &s) in a.samples.iter().enumerate() {
            let v = (s * scale).round().clamp(-scale, scale - 1.0) as i64;
            self.pending[i % self.channels].push(v);
        }
        let mut out = Vec::new();
        while self.pending[0].len() >= BLOCK_SIZE {
            out.push(self.encode_block(BLOCK_SIZE));
        }
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        let n = self.pending[0].len();
        Ok(if n > 0 { vec![self.encode_block(n)] } else { Vec::new() })
    }
}
