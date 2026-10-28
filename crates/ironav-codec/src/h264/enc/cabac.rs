//! The CABAC arithmetic encoding engine (ITU-T H.264 9.3.4) and the residual block syntax coded with
//! it (7.3.5.3.3), the inverse of the decoder's.

use super::bitw::BitWriter;
use crate::h264::cabac::TRANS_LPS;
use crate::h264::tables::{ABS_OFF, CABAC_INIT_I, CABAC_INIT_PB, LAST_OFF, LPS_RANGE, SIG_OFF};

pub struct CabacWriter {
    w: BitWriter,
    low: u32,
    range: u32,
    outstanding: u32,
    first: bool,
    /// Per context: pStateIdx * 2 + valMPS.
    state: [u8; 1024],
}

impl CabacWriter {
    /// Continues `w` (already byte aligned after the slice header) with the contexts initialised for a slice.
    pub fn new(w: BitWriter, intra: bool, init_idc: usize, slice_qp: i32) -> Self {
        let mut state = [0u8; 1024];
        let qp = slice_qp.clamp(0, 51);
        for (i, s) in state.iter_mut().enumerate() {
            let [m, n] = if intra { CABAC_INIT_I[i] } else { CABAC_INIT_PB[init_idc][i] };
            let pre = (((m as i32 * qp) >> 4) + n as i32).clamp(1, 126);
            *s = if pre <= 63 { ((63 - pre) << 1) as u8 } else { (((pre - 64) << 1) | 1) as u8 };
        }
        CabacWriter { w, low: 0, range: 510, outstanding: 0, first: true, state }
    }

    fn put_bit(&mut self, b: u32) {
        if self.first {
            self.first = false;
        } else {
            self.w.put(1, b);
        }
        while self.outstanding > 0 {
            self.w.put(1, 1 - b);
            self.outstanding -= 1;
        }
    }

    fn renorm(&mut self) {
        while self.range < 256 {
            if self.low < 256 {
                self.put_bit(0);
            } else if self.low >= 512 {
                self.low -= 512;
                self.put_bit(1);
            } else {
                self.low -= 256;
                self.outstanding += 1;
            }
            self.range <<= 1;
            self.low <<= 1;
        }
    }

    pub fn decision(&mut self, ctx: usize, bin: bool) {
        let s = self.state[ctx];
        let (p, mps) = ((s >> 1) as usize, s & 1 == 1);
        let lps = LPS_RANGE[p][((self.range >> 6) & 3) as usize] as u32;
        self.range -= lps;
        if bin != mps {
            self.low += self.range;
            self.range = lps;
            let nmps = if p == 0 { !mps } else { mps };
            self.state[ctx] = (TRANS_LPS[p] << 1) | nmps as u8;
        } else {
            let np = if p < 62 { p + 1 } else { p };
            self.state[ctx] = ((np as u8) << 1) | mps as u8;
        }
        self.renorm();
    }

    pub fn bypass(&mut self, bin: bool) {
        self.low <<= 1;
        if bin {
            self.low += self.range;
        }
        if self.low >= 1024 {
            self.put_bit(1);
            self.low -= 1024;
        } else if self.low < 512 {
            self.put_bit(0);
        } else {
            self.low -= 512;
            self.outstanding += 1;
        }
    }

    /// Bypass-coded k-th order Exp-Golomb suffix (9.3.2.3).
    pub fn exp_golomb(&mut self, mut v: u32, mut k: u32) {
        while v >= 1 << k {
            self.bypass(true);
            v -= 1 << k;
            k += 1;
        }
        self.bypass(false);
        while k > 0 {
            k -= 1;
            self.bypass((v >> k) & 1 == 1);
        }
    }

    /// A terminating bin: end_of_slice_flag, or the I_PCM test of an intra mb_type.
    pub fn terminate(&mut self, bin: bool) {
        self.range -= 2;
        if bin {
            self.low += self.range;
            self.range = 2;
            self.renorm();
            self.put_bit((self.low >> 9) & 1);
            // the last of these two bits is the rbsp_stop_one_bit
            self.w.put(2, ((self.low >> 7) & 3) | 1);
        } else {
            self.renorm();
        }
    }

    /// The slice data, after a terminating bin of 1, padded with zeros to a byte.
    pub fn finish(self) -> Vec<u8> {
        self.w.into_bytes()
    }
}

/// Writes one residual block: coded_block_flag (with context `cbf_ctx`, unless `None` because the
/// block always has one) and the levels, in scan order, of category `cat`. Returns the number of
/// non-zero levels.
pub fn write_block(c: &mut CabacWriter, cat: usize, cbf_ctx: usize, levels: &[i32]) -> usize {
    let max = levels.len();
    let total = levels.iter().filter(|&&l| l != 0).count();
    c.decision(cbf_ctx, total > 0);
    if total == 0 {
        return 0;
    }
    let last = levels.iter().rposition(|&l| l != 0).unwrap();
    for (i, &l) in levels.iter().enumerate().take(max - 1) {
        // 4:2:0 chroma DC has three significance contexts, one per position
        let inc = if cat == 3 { i.min(2) } else { i };
        c.decision(SIG_OFF[0][cat] as usize + inc, l != 0);
        if l != 0 {
            c.decision(LAST_OFF[0][cat] as usize + inc, i == last);
            if i == last {
                break;
            }
        }
    }
    let base = ABS_OFF[cat] as usize;
    let (mut eq1, mut gt1) = (0usize, 0usize);
    for &l in levels[..=last].iter().rev().filter(|&&l| l != 0) {
        let abs = l.unsigned_abs();
        let inc0 = if gt1 != 0 { 0 } else { (1 + eq1).min(4) };
        if abs == 1 {
            c.decision(base + inc0, false);
            eq1 += 1;
        } else {
            c.decision(base + inc0, true);
            let incn = base + 5 + gt1.min(4 - (cat == 3) as usize);
            // abs - 2 ones and a zero, the ones capped at 13 with the rest in an Exp-Golomb suffix
            for _ in 0..(abs - 2).min(13) {
                c.decision(incn, true);
            }
            if abs < 15 {
                c.decision(incn, false);
            } else {
                c.exp_golomb(abs - 15, 0);
            }
            gt1 += 1;
        }
        c.bypass(l < 0);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::cabac::Cabac;

    #[test]
    fn bins_read_back_through_the_decoder() {
        let mut seed = 4321u32;
        let mut rnd = |m: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) % m
        };
        // (kind, context, value): 0 decision, 1 bypass, 2 terminate 0
        let ops: Vec<(u32, usize, bool)> = (0..20000)
            .map(|_| {
                let kind = [0, 0, 0, 1, 2][rnd(5) as usize];
                let ctx = rnd(8) as usize * 37;
                // skewed values so contexts adapt
                (kind, ctx, rnd(10) < if ctx % 2 == 0 { 2 } else { 8 })
            })
            .collect();
        let mut w = BitWriter::new();
        w.put(5, 0b10110);
        w.align_ones();
        let mut c = CabacWriter::new(w, false, 1, 30);
        for &(k, ctx, v) in &ops {
            match k {
                0 => c.decision(ctx, v),
                1 => c.bypass(v),
                _ => c.terminate(false),
            }
        }
        c.terminate(true);
        let data = c.finish();
        let mut d = Cabac::new(&data, 1, false, 1, 30);
        for (i, &(k, ctx, v)) in ops.iter().enumerate() {
            let got = match k {
                0 => d.decision(ctx) == 1,
                1 => d.bypass() == 1,
                _ => d.terminate() == 1,
            };
            assert_eq!(got, if k == 2 { false } else { v }, "bin {i}");
        }
        assert_eq!(d.terminate(), 1);
        assert_eq!(d.pcm_byte_pos(), data.len(), "the flush ends the data");
    }
}
