//! The HEVC CABAC engine (ITU-T H.265 9.3.4.3). It is the H.264 arithmetic coder with different
//! context initialisation, so the probability tables are shared.

use crate::h264::cabac::NEXT;
use crate::h264::tables::LPS_RANGE;

/// Room for every context of every syntax element.
pub const MAX_CONTEXTS: usize = 256;

/// The offset register is kept scaled up by `k` bits: `value` holds the 9-bit offset of the standard
/// followed by `k` look-ahead bits of the stream, so comparing with the range means comparing with
/// `range << k`, and renormalising is just `k -= n`. Bytes are pulled in four at a time.
pub struct Cabac<'a> {
    d: &'a [u8],
    /// Index of the next byte to load into `value` (it may run past the end: zeros are read).
    next: usize,
    range: u32,
    value: u64,
    k: u32,
    /// Per context: pStateIdx * 2 + valMPS.
    state: [u8; MAX_CONTEXTS],
}

impl<'a> Cabac<'a> {
    /// Starts decoding at byte `start` of `d`; contexts are initialised from `init` (one value per context).
    pub fn new(d: &'a [u8], start: usize, init: &[u8], slice_qp: i32) -> Self {
        let mut c = Cabac { d, next: start, range: 0, value: 0, k: 0, state: [0; MAX_CONTEXTS] };
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

    /// `n` bypass bins as an unsigned number, most significant first.
    pub fn bypass_bits(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bypass();
        }
        v
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
                self.range <<= 1;
                self.k -= 1;
            }
            0
        }
    }

    /// After a terminate bin of 1: the byte where raw data (PCM samples) resumes.
    pub fn pcm_byte_pos(&self) -> usize {
        (self.next * 8 - self.k as usize).div_ceil(8)
    }

    pub fn data(&self) -> &'a [u8] {
        self.d
    }

    /// Restarts the arithmetic decoder (not the contexts) at byte `byte`.
    pub fn restart_at(&mut self, byte: usize) {
        self.next = byte;
        self.init_engine();
    }
}
