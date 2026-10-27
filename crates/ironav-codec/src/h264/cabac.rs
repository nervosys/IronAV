//! The CABAC arithmetic decoding engine (ITU-T H.264 9.3.1.2 and 9.3.3.2).

use super::tables::{CABAC_INIT_I, CABAC_INIT_PB, LPS_RANGE};

/// State transition after coding the least probable symbol.
pub(crate) const TRANS_LPS: [u8; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12, 13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21, 21, 22, 22, 23, 24, 24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33, 33, 33,
    34, 34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

pub struct Cabac<'a> {
    d: &'a [u8],
    /// Bit position of the next unread bit.
    pos: usize,
    range: u32,
    offset: u32,
    /// Per context: pStateIdx * 2 + valMPS.
    state: [u8; 1024],
}

impl<'a> Cabac<'a> {
    /// Starts decoding at byte `start` of `d` with the contexts initialised for a slice.
    pub fn new(d: &'a [u8], start: usize, intra: bool, init_idc: usize, slice_qp: i32) -> Self {
        let mut c = Cabac { d, pos: start * 8, range: 0, offset: 0, state: [0; 1024] };
        let qp = slice_qp.clamp(0, 51);
        for i in 0..1024 {
            let [m, n] = if intra { CABAC_INIT_I[i] } else { CABAC_INIT_PB[init_idc][i] };
            let pre = (((m as i32 * qp) >> 4) + n as i32).clamp(1, 126);
            c.state[i] = if pre <= 63 { ((63 - pre) << 1) as u8 } else { (((pre - 64) << 1) | 1) as u8 };
        }
        c.init_engine();
        c
    }

    fn init_engine(&mut self) {
        self.range = 510;
        self.offset = self.read(9);
    }

    fn bit(&mut self) -> u32 {
        let b = self.d.get(self.pos >> 3).map_or(0, |&byte| (byte >> (7 - (self.pos & 7))) & 1);
        self.pos += 1;
        b as u32
    }

    fn read(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit();
        }
        v
    }

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
        while self.range < 256 {
            self.range <<= 1;
            self.offset = (self.offset << 1) | self.bit();
        }
        bin
    }

    pub fn bypass(&mut self) -> u32 {
        self.offset = (self.offset << 1) | self.bit();
        if self.offset >= self.range {
            self.offset -= self.range;
            1
        } else {
            0
        }
    }

    pub fn terminate(&mut self) -> u32 {
        self.range -= 2;
        if self.offset >= self.range {
            1
        } else {
            while self.range < 256 {
                self.range <<= 1;
                self.offset = (self.offset << 1) | self.bit();
            }
            0
        }
    }

    /// After a terminate bin of 1 (I_PCM or the end of the slice): the byte where raw data resumes.
    /// The decoder has read exactly as many bits as the encoder's flush wrote, the last of them
    /// being the final 1 bit, so the next byte boundary at or after the read position is it.
    pub fn pcm_byte_pos(&self) -> usize {
        self.pos.div_ceil(8)
    }

    pub fn data(&self) -> &'a [u8] {
        self.d
    }

    pub fn restart_at(&mut self, byte: usize) {
        self.pos = byte * 8;
        self.init_engine();
    }

    #[allow(dead_code)]
    pub fn bit_pos(&self) -> usize {
        self.pos
    }
}
