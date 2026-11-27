//! An MSB-first bit reader for DTS syntax. Reads past the end give zeros and move the position on,
//! as FFmpeg's `get_bits` does on its padded buffer; the decoder checks `left` at the places FFmpeg
//! does, so a short frame is reported rather than decoded from zeros.

pub(super) struct Bits<'a> {
    data: &'a [u8],
    pub(super) pos: usize,
}

impl<'a> Bits<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0 }
    }

    /// Bits left before the end of the data; negative once a read has gone past it.
    pub(super) fn left(&self) -> i64 {
        self.data.len() as i64 * 8 - self.pos as i64
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first_and_zeros_past_the_end() {
        let data = [0b1010_0110, 0xFF, 0x80];
        let mut r = Bits::new(&data);
        assert!(r.bit());
        assert_eq!(r.bits(3), 0b010);
        assert_eq!(r.sbits(4), 0b0110);
        assert_eq!(r.sbits(4), -1);
        r.skip(4);
        assert_eq!(r.bits(2), 0b10);
        assert_eq!(r.left(), 6);
        assert_eq!(r.bits(20), 0);
        assert_eq!(r.left(), -14);
        assert_eq!(r.sbits(0), 0);
    }

    #[test]
    fn reads_32_bits_at_any_alignment() {
        let data = [0x7F, 0xFE, 0x80, 0x01, 0x12, 0x34];
        let mut r = Bits::new(&data);
        assert_eq!(r.bits(32), 0x7FFE8001);
        let mut r = Bits::new(&data);
        r.skip(4);
        assert_eq!(r.bits(32), 0xFFE80011);
        assert_eq!(Bits::new(&[0x80, 0, 0, 0]).sbits(16), -32768);
    }
}
