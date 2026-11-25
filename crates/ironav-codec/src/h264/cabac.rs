//! The CABAC arithmetic decoding engine (ITU-T H.264 9.3.1.2 and 9.3.3.2).

use super::tables::{CABAC_INIT_I, CABAC_INIT_PB, LPS_RANGE};

/// State transition after coding the least probable symbol.
pub(crate) const TRANS_LPS: [u8; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12, 13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21, 21, 22, 22, 23, 24, 24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33, 33, 33,
    34, 34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

/// The next state (pStateIdx * 2 + valMPS) after an MPS (`.0`) or an LPS (`.1`), for each state.
const NEXT: [[u8; 2]; 128] = {
    let mut t = [[0u8; 2]; 128];
    let mut s = 0;
    while s < 128 {
        let (p, mps) = (s >> 1, s & 1);
        let np = if p < 62 { p + 1 } else { p };
        let nmps = if p == 0 { 1 - mps } else { mps };
        t[s] = [((np << 1) | mps) as u8, ((TRANS_LPS[p] as usize) << 1 | nmps) as u8];
        s += 1;
    }
    t
};

/// The arithmetic decoder keeps the offset register scaled up by `k` bits: `value` holds the
/// 9-bit offset of the standard followed by `k` look-ahead bits of the stream, so comparing with
/// the range means comparing with `range << k`, and renormalising is just `k -= n`. Bytes are
/// pulled in four at a time when fewer than 8 look-ahead bits are left.
pub struct Cabac<'a> {
    d: &'a [u8],
    /// Index of the next byte to load into `value` (it may run past the end: zeros are read).
    next: usize,
    range: u32,
    value: u64,
    k: u32,
    /// Per context: pStateIdx * 2 + valMPS.
    state: [u8; 1024],
}

impl<'a> Cabac<'a> {
    /// Starts decoding at byte `start` of `d` with the contexts initialised for a slice.
    pub fn new(d: &'a [u8], start: usize, intra: bool, init_idc: usize, slice_qp: i32) -> Self {
        let mut c = Cabac { d, next: start, range: 0, value: 0, k: 0, state: [0; 1024] };
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
        self.value = 0;
        self.k = 0;
        self.refill();
        // the first 9 bits are the offset; the rest of the 32 loaded are look-ahead
        self.k -= 9;
    }

    /// Loads four more bytes below the bits held (zeros past the end of the data).
    #[inline(never)]
    fn refill(&mut self) {
        let w = match self.d.get(self.next..self.next + 4) {
            Some(b) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            None => {
                let mut b = [0u8; 4];
                for (i, v) in b.iter_mut().enumerate() {
                    *v = self.d.get(self.next + i).copied().unwrap_or(0);
                }
                u32::from_be_bytes(b)
            }
        };
        self.next += 4;
        self.value = (self.value << 32) | w as u64;
        self.k += 32;
    }

    #[inline(always)]
    pub fn decision(&mut self, ctx: usize) -> u32 {
        if self.k < 8 {
            self.refill();
        }
        let s = self.state[ctx] as usize;
        let lps = LPS_RANGE[s >> 1][((self.range >> 6) & 3) as usize] as u32;
        let rmps = self.range - lps;
        let scaled = (rmps as u64) << self.k;
        if self.value < scaled {
            self.state[ctx] = NEXT[s][0];
            self.range = rmps;
            if rmps < 256 {
                self.range = rmps << 1;
                self.k -= 1;
            }
            (s & 1) as u32
        } else {
            self.value -= scaled;
            self.state[ctx] = NEXT[s][1];
            let n = lps.leading_zeros() - 23;
            self.range = lps << n;
            self.k -= n;
            ((s & 1) ^ 1) as u32
        }
    }

    #[inline(always)]
    pub fn bypass(&mut self) -> u32 {
        if self.k < 8 {
            self.refill();
        }
        self.k -= 1;
        let scaled = (self.range as u64) << self.k;
        if self.value >= scaled {
            self.value -= scaled;
            1
        } else {
            0
        }
    }

    pub fn terminate(&mut self) -> u32 {
        if self.k < 8 {
            self.refill();
        }
        self.range -= 2;
        if self.value >= (self.range as u64) << self.k {
            1
        } else {
            if self.range < 256 {
                let n = self.range.leading_zeros() - 23;
                self.range <<= n;
                self.k -= n;
            }
            0
        }
    }

    /// After a terminate bin of 1 (I_PCM or the end of the slice): the byte where raw data resumes.
    /// The decoder has read exactly as many bits as the encoder's flush wrote, the last of them
    /// being the final 1 bit, so the next byte boundary at or after the read position is it.
    pub fn pcm_byte_pos(&self) -> usize {
        (self.next * 8 - self.k as usize).div_ceil(8)
    }

    pub fn data(&self) -> &'a [u8] {
        self.d
    }

    pub fn restart_at(&mut self, byte: usize) {
        self.next = byte;
        self.init_engine();
    }

    #[allow(dead_code)]
    pub fn bit_pos(&self) -> usize {
        self.next * 8 - self.k as usize
    }
}
