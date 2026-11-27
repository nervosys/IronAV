//! An MSB-first bit reader for MLP and TrueHD syntax. Reads past the end give zeros and move the
//! position on, as FFmpeg's `get_bits` does on its padded buffer; the decoder compares the position
//! with the substream's size at the places FFmpeg does, so a short access unit is reported rather
//! than decoded from zeros.

pub(super) struct Bits<'a> {
    data: &'a [u8],
    pub(super) pos: usize,
}

impl<'a> Bits<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0 }
    }

    /// The next 32 bits, zeros past the end, without moving on.
    pub(super) fn peek32(&self) -> u32 {
        let i = self.pos >> 3;
        let mut x = 0u64;
        for k in 0..5 {
            x = (x << 8) | self.data.get(i + k).copied().unwrap_or(0) as u64;
        }
        (x >> (8 - (self.pos & 7))) as u32
    }

    /// `n` bits (at most 32).
    pub(super) fn bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek32() >> (32 - n);
        self.pos += n as usize;
        v
    }

    pub(super) fn bit(&mut self) -> bool {
        self.bits(1) != 0
    }

    /// `n` bits as a two's-complement number.
    pub(super) fn sbits(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        (self.bits(n) as i32) << (32 - n) >> (32 - n)
    }

    pub(super) fn skip(&mut self, n: usize) {
        self.pos += n;
    }
}
