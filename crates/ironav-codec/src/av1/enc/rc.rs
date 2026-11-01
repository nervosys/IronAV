//! The multi-symbol range encoder of the tile data, the inverse of `av1::symbol::SymbolDecoder`.
//!
//! Adapted from rav1e's `src/ec.rs` (BSD-2-Clause, the rav1e contributors): the interval update
//! `lr_compute`, the carry-propagating `store` and the flush in `done` follow its encoder. CDFs
//! are stored the way the decoder stores them (`icdf`, with the adaptation counter last), so the
//! symbol probabilities are the decoder's by construction.

use crate::av1::symbol::update_cdf;

const EC_PROB_SHIFT: u64 = 6;
const EC_MIN_PROB: u64 = 4;

pub struct SymbolEncoder {
    low: u64,
    rng: u32,
    cnt: i32,
    precarry: Vec<u16>,
    /// Whether CDFs adapt after each symbol (disable_cdf_update is 0).
    adapt: bool,
}

impl SymbolEncoder {
    pub fn new(adapt: bool) -> Self {
        SymbolEncoder { low: 0, rng: 0x8000, cnt: -9, precarry: Vec::new(), adapt }
    }

    /// Codes symbol `s` of the CDF `icdf` (`icdf.len()` symbols; the last element is the counter).
    pub fn encode(&mut self, s: usize, icdf: &[u16]) {
        let n = icdf.len() as u64;
        let s = s as u64;
        let r = self.rng as u64;
        let nms = n - s;
        let fl = if s > 0 { icdf[s as usize - 1] as u64 } else { 32768 };
        let fh = if s + 1 < n { icdf[s as usize] as u64 } else { 0 };
        let mut u = (((r >> 8) * (fl >> EC_PROB_SHIFT)) >> (7 - EC_PROB_SHIFT)) + EC_MIN_PROB * nms;
        if fl >= 32768 {
            u = r;
        }
        let v = (((r >> 8) * (fh >> EC_PROB_SHIFT)) >> (7 - EC_PROB_SHIFT)) + EC_MIN_PROB * (nms - 1);
        self.store(r - u, (u - v) as u16);
    }

    /// Codes symbol `s` and adapts `cdf` as the decoder's `symbol` does.
    pub fn symbol(&mut self, s: usize, cdf: &mut [u16]) {
        self.encode(s, cdf);
        if self.adapt {
            update_cdf(cdf, s);
        }
    }

    /// A bool with probability 1/2 (the decoder's `bool`).
    pub fn bool(&mut self, b: bool) {
        self.encode(b as usize, &[1 << 14, 0]);
    }

    fn store(&mut self, l: u64, r: u16) {
        let mut low = l + self.low;
        let mut c = self.cnt;
        let d = r.leading_zeros() as i32;
        let mut s = c + d;
        if s >= 0 {
            c += 16;
            let mut m = (1u64 << c) - 1;
            if s >= 8 {
                self.precarry.push((low >> c) as u16);
                low &= m;
                c -= 8;
                m >>= 8;
            }
            self.precarry.push((low >> c) as u16);
            s = c + d - 24;
            low &= m;
        }
        self.low = low << d;
        self.rng = (r as u32) << d;
        self.cnt = s;
    }

    /// Flushes the coder and returns the tile's bytes.
    pub fn finish(mut self) -> Vec<u8> {
        let l = self.low;
        let mut c = self.cnt;
        let mut s = 10 + c;
        let m: u64 = 0x3FFF;
        let mut e = ((l + m) & !m) | (m + 1);
        if s > 0 {
            let mut n = (1u64 << (c + 16)) - 1;
            loop {
                self.precarry.push((e >> (c + 16)) as u16);
                e &= n;
                s -= 8;
                c -= 8;
                n >>= 8;
                if s <= 0 {
                    break;
                }
            }
        }
        let mut out = vec![0u8; self.precarry.len()];
        let mut carry = 0u32;
        for (i, &p) in self.precarry.iter().enumerate().rev() {
            carry += p as u32;
            out[i] = carry as u8;
            carry >>= 8;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::av1::symbol::SymbolDecoder;

    /// A small deterministic generator for the test symbols.
    fn lcg(state: &mut u32) -> u32 {
        *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        *state >> 8
    }

    #[test]
    fn decodes_what_it_encodes_with_adaptation() {
        let mut seed = 7;
        let mut enc = SymbolEncoder::new(true);
        let mut cdf_e = [[32768 - 8000, 32768 - 20000, 32768 - 30000, 0], [32768 - 1000, 32768 - 2000, 32768 - 4000, 0]];
        let mut syms = Vec::new();
        for i in 0..5000 {
            let ctx = i % 2;
            // skewed symbols, so the adaptation changes the probabilities
            let s = if lcg(&mut seed).is_multiple_of(4) { (lcg(&mut seed) % 4) as usize } else { 0 };
            enc.symbol(s, &mut cdf_e[ctx]);
            if i % 7 == 0 {
                enc.bool(i % 3 == 0);
            }
            syms.push(s);
        }
        let bytes = enc.finish();

        let mut cdf_d = [[32768 - 8000, 32768 - 20000, 32768 - 30000, 0], [32768 - 1000, 32768 - 2000, 32768 - 4000, 0]];
        let mut dec = SymbolDecoder::new(&bytes);
        for (i, &s) in syms.iter().enumerate() {
            let ctx = i % 2;
            assert_eq!(dec.symbol(&mut cdf_d[ctx]), s, "symbol {i}");
            if i % 7 == 0 {
                assert_eq!(dec.bool(), i % 3 == 0, "bool after symbol {i}");
            }
        }
        assert_eq!(cdf_d, cdf_e);
    }
}
