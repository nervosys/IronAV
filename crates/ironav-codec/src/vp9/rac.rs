//! The VP8/VP9 boolean (range) decoder.

pub struct Rac {
    buf: Vec<u8>,
    pos: usize,
    value: u64,
    count: i32,
    range: u32,
}

const LOTS_OF_BITS: i32 = 0x4000_0000;

impl Rac {
    /// Starts decoding `buf`; the caller reads the marker bit.
    pub fn new(buf: &[u8]) -> Option<Self> {
        if buf.is_empty() {
            return None;
        }
        let mut r = Rac { buf: buf.to_vec(), pos: 0, value: 0, count: -8, range: 255 };
        r.fill();
        Some(r)
    }

    fn fill(&mut self) {
        let mut shift = 64 - 8 - (self.count + 8);
        while shift >= 0 {
            if self.pos >= self.buf.len() {
                self.count += LOTS_OF_BITS;
                break;
            }
            self.count += 8;
            self.value |= (self.buf[self.pos] as u64) << shift;
            self.pos += 1;
            shift -= 8;
        }
    }

    #[inline]
    pub fn get(&mut self, prob: u8) -> bool {
        let split = (self.range * prob as u32 + (256 - prob as u32)) >> 8;
        if self.count < 0 {
            self.fill();
        }
        let big = (split as u64) << 56;
        let bit = if self.value >= big {
            self.range -= split;
            self.value -= big;
            true
        } else {
            self.range = split;
            false
        };
        let shift = self.range.leading_zeros() - 24;
        self.range <<= shift;
        self.value <<= shift;
        self.count -= shift as i32;
        bit
    }

    #[inline]
    pub fn bit(&mut self) -> bool {
        self.get(128)
    }

    pub fn uint(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit() as u32;
        }
        v
    }

    /// Walks a tree stored as `[child; 2]` pairs where non-positive entries are negated leaves.
    #[inline]
    pub fn tree(&mut self, tree: &[[i8; 2]], probs: &[u8]) -> usize {
        let mut i = 0usize;
        loop {
            let n = tree[i][self.get(probs[i]) as usize];
            if n <= 0 {
                return (-n) as usize;
            }
            i = n as usize;
        }
    }
}
