//! The CABAC encoder (ITU-T H.265 9.3.5): the inverse of the decoder's arithmetic engine. The
//! probability tables and the context initialisation are the decoder's.

use super::super::cabac::{Cabac, MAX_CONTEXTS};
use crate::h264::cabac::TRANS_LPS;
use crate::h264::enc::bitw::BitWriter;
use crate::h264::tables::LPS_RANGE;

/// The context states for a slice of `qp` with the initial values `init` (the decoder's own set-up).
pub fn init_state(init: &[u8], qp: i32) -> [u8; MAX_CONTEXTS] {
    Cabac::new(&[], 0, init, qp).contexts()
}

pub struct CabacEnc {
    pub w: BitWriter,
    low: u32,
    range: u32,
    outstanding: u32,
    first: bool,
    pub state: [u8; MAX_CONTEXTS],
}

impl CabacEnc {
    pub fn new(w: BitWriter, init: &[u8], qp: i32) -> Self {
        CabacEnc { w, low: 0, range: 510, outstanding: 0, first: true, state: init_state(init, qp) }
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

    pub fn decision(&mut self, ctx: usize, bin: u32) {
        let s = self.state[ctx];
        let (p, mps) = ((s >> 1) as usize, (s & 1) as u32);
        let lps = LPS_RANGE[p][((self.range >> 6) & 3) as usize] as u32;
        self.range -= lps;
        if bin != mps {
            self.low += self.range;
            self.range = lps;
            let nmps = if p == 0 { 1 - mps } else { mps };
            self.state[ctx] = (TRANS_LPS[p] << 1) | nmps as u8;
        } else {
            let np = if p < 62 { p + 1 } else { p };
            self.state[ctx] = ((np as u8) << 1) | mps as u8;
        }
        self.renorm();
    }

    pub fn bypass(&mut self, bin: u32) {
        self.low <<= 1;
        if bin != 0 {
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

    /// The low `n` bits of `v`, most significant first, as bypass bins.
    pub fn bypass_bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bypass((v >> i) & 1);
        }
    }

    /// A terminating bin; a 1 flushes the engine, and its last bit is the RBSP stop bit.
    pub fn terminate(&mut self, bin: u32) {
        self.range -= 2;
        if bin != 0 {
            self.low += self.range;
            self.range = 2;
            self.renorm();
            self.put_bit((self.low >> 9) & 1);
            self.w.put(2, ((self.low >> 7) & 3) | 1);
        } else {
            self.renorm();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::enc::bitw::BitWriter;
    use crate::hevc::tables::INIT_VALUES;

    /// Bins written with the encoder read back through the decoder's engine.
    #[test]
    fn bins_read_back_through_the_decoder() {
        let bins: Vec<(usize, u32)> = (0..3000u32).map(|i| ((i % 7) as usize, ((i * i + i / 3) % 5 == 0) as u32)).collect();
        let mut e = CabacEnc::new(BitWriter::new(), &INIT_VALUES[1], 30);
        for (k, &(ctx, b)) in bins.iter().enumerate() {
            if k % 5 == 0 {
                e.bypass(b);
            } else {
                e.decision(ctx, b);
            }
        }
        e.terminate(1);
        let data = e.w.into_bytes();
        let mut d = Cabac::new(&data, 0, &INIT_VALUES[1], 30);
        for (k, &(ctx, b)) in bins.iter().enumerate() {
            let got = if k % 5 == 0 { d.bypass() } else { d.decision(ctx) };
            assert_eq!(got, b, "bin {k}");
        }
        assert_eq!(d.terminate(), 1);
    }
}
