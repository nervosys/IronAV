//! An MSB-first bit reader for AC-3 syntax. Reads past the end give zeros, as FFmpeg's `get_bits`
//! does on its padded buffer, so a damaged frame is decoded to the end and then reported.

pub(super) struct BitReader<'a> {
    data: &'a [u8],
    pub(super) pos: usize,
}

impl<'a> BitReader<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    /// The bytes this reader was given.
    pub(super) fn data(&self) -> &'a [u8] {
        self.data
    }

    pub(super) fn bit(&mut self) -> bool {
        let byte = self.data.get(self.pos >> 3).copied().unwrap_or(0);
        let b = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        b != 0
    }

    pub(super) fn bits(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit() as u32;
        }
        v
    }

    /// `n` bits as a two's-complement number.
    pub(super) fn sbits(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        let v = self.bits(n) as i32;
        (v << (32 - n)) >> (32 - n)
    }

    pub(super) fn skip(&mut self, n: usize) {
        self.pos += n;
    }
}
