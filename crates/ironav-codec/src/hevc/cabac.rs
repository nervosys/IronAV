//! The HEVC CABAC engine (ITU-T H.265 9.3.4.3). It is the H.264 arithmetic coder with different
//! context initialisation, so the probability tables are shared.

use crate::h264::cabac::TRANS_LPS;
use crate::h264::tables::LPS_RANGE;

/// Room for every context of every syntax element.
pub const MAX_CONTEXTS: usize = 256;

pub struct Cabac<'a> {
    d: &'a [u8],
    /// Bit position of the next unread bit.
    pos: usize,
    range: u32,
    offset: u32,
    /// Per context: pStateIdx * 2 + valMPS.
    state: [u8; MAX_CONTEXTS],
}

impl<'a> Cabac<'a> {
    /// Starts decoding at byte `start` of `d`; contexts are initialised from `init` (one value per context).
    pub fn new(d: &'a [u8], start: usize, init: &[u8], slice_qp: i32) -> Self {
        let mut c = Cabac { d, pos: start * 8, range: 0, offset: 0, state: [0; MAX_CONTEXTS] };
        c.init_contexts(init, slice_qp);
        c.init_engine();
        c
    }

    pub fn init_contexts(&mut self, init: &[u8], slice_qp: i32) {
        let qp = slice_qp.clamp(0, 51);
        for (i, &v) in init.iter().enumerate() {
            let m = (v >> 4) as i32 * 5 - 45;
            let n = ((v & 15) as i32) * 8 - 16;
            let pre = (((m * qp) >> 4) + n).clamp(1, 126);
            self.state[i] = if pre <= 63 { ((63 - pre) << 1) as u8 } else { (((pre - 64) << 1) | 1) as u8 };
        }
    }

    pub fn contexts(&self) -> [u8; MAX_CONTEXTS] {
        self.state
    }

    pub fn set_contexts(&mut self, s: &[u8; MAX_CONTEXTS]) {
        self.state = *s;
    }

    fn init_engine(&mut self) {
        self.range = 510;
        self.offset = self.read(9);
    }

    /// Reads `n` (at most 9) bits.
    #[inline]
    fn read(&mut self, n: u32) -> u32 {
        let byte = self.pos >> 3;
        let sh = (self.pos & 7) as u32;
        let hi = *self.d.get(byte).unwrap_or(&0) as u32;
        let lo = *self.d.get(byte + 1).unwrap_or(&0) as u32;
        let mid = *self.d.get(byte + 2).unwrap_or(&0) as u32;
        let w = (hi << 16) | (lo << 8) | mid;
        self.pos += n as usize;
        (w >> (24 - sh - n)) & ((1 << n) - 1)
    }

    #[inline]
    pub fn decision(&mut self, ctx: usize) -> u32 {
        let s = self.state[ctx];
        let (p, mps) = ((s >> 1) as usize, (s & 1) as u32);
        let lps = LPS_RANGE[p][((self.range >> 6) & 3) as usize] as u32;
        self.range -= lps;
        let bin;
        if self.offset >= self.range {
            bin = 1 - mps;
            self.offset -= self.range;
            self.range = lps;
            let nmps = if p == 0 { 1 - mps } else { mps };
            self.state[ctx] = (TRANS_LPS[p] << 1) | nmps as u8;
        } else {
            bin = mps;
            let np = if p < 62 { p + 1 } else { p };
            self.state[ctx] = ((np as u8) << 1) | mps as u8;
        }
        if self.range < 256 {
            let sh = self.range.leading_zeros() - 23;
            self.range <<= sh;
            self.offset = (self.offset << sh) | self.read(sh);
        }
        bin
    }

    #[inline]
    pub fn bypass(&mut self) -> u32 {
        self.offset = (self.offset << 1) | self.read(1);
        if self.offset >= self.range {
            self.offset -= self.range;
            1
        } else {
            0
        }
    }

    /// `n` bypass bins as an unsigned number, most significant first.
    pub fn bypass_bits(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bypass();
        }
        v
    }

    pub fn terminate(&mut self) -> u32 {
        self.range -= 2;
        if self.offset >= self.range {
            1
        } else {
            if self.range < 256 {
                self.range <<= 1;
                self.offset = (self.offset << 1) | self.read(1);
            }
            0
        }
    }

    /// After a terminate bin of 1: the byte where raw data (PCM samples) resumes.
    pub fn pcm_byte_pos(&self) -> usize {
        self.pos.div_ceil(8)
    }

    pub fn data(&self) -> &'a [u8] {
        self.d
    }

    /// Restarts the arithmetic decoder (not the contexts) at byte `byte`.
    pub fn restart_at(&mut self, byte: usize) {
        self.pos = byte * 8;
        self.init_engine();
    }
}
