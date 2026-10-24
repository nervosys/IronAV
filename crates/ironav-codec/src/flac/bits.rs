use ironav_core::{Error, Result};

fn eof() -> Error {
    Error::invalid_input("FLAC frame ends unexpectedly")
}

/// MSB-first bit reader over a byte slice.
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, end: data.len() * 8 }
    }

    /// Read `n` (0..=32) bits.
    #[inline]
    pub fn read(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        let n = n as usize;
        if self.pos + n > self.end {
            return Err(eof());
        }
        let byte = self.pos >> 3;
        let off = self.pos & 7;
        let nbytes = (off + n + 7) >> 3;
        let mut acc = 0u64;
        for i in 0..nbytes {
            acc = (acc << 8) | self.data[byte + i] as u64;
        }
        acc >>= nbytes * 8 - off - n;
        self.pos += n;
        Ok((acc & ((1u64 << n) - 1)) as u32)
    }

    /// Read up to 33 bits as an unsigned value.
    pub fn read_u64(&mut self, n: u32) -> Result<u64> {
        if n <= 32 {
            Ok(self.read(n)? as u64)
        } else {
            let hi = self.read(n - 32)? as u64;
            Ok((hi << 32) | self.read(32)? as u64)
        }
    }

    /// Read an `n`-bit two's-complement value.
    pub fn read_signed(&mut self, n: u32) -> Result<i64> {
        if n == 0 {
            return Ok(0);
        }
        let v = self.read_u64(n)?;
        let shift = 64 - n;
        Ok(((v << shift) as i64) >> shift)
    }

    /// Count zero bits up to and including the next one bit; returns the zero count.
    #[inline]
    pub fn read_unary(&mut self) -> Result<u32> {
        let mut count = 0u32;
        loop {
            if self.pos >= self.end {
                return Err(eof());
            }
            let off = (self.pos & 7) as u32;
            let b = (self.data[self.pos >> 3] << off) as u32 & 0xFF;
            if b == 0 {
                let avail = 8 - off;
                count += avail;
                self.pos += avail as usize;
                continue;
            }
            let lz = (b as u8).leading_zeros();
            count += lz;
            self.pos += lz as usize + 1;
            return Ok(count);
        }
    }

    pub fn bit_pos(&self) -> usize {
        self.pos
    }
}

/// MSB-first bit writer.
#[derive(Default)]
pub struct BitWriter {
    pub bytes: Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Write the low `n` (0..=32) bits of `v`.
    #[inline]
    pub fn write(&mut self, v: u64, n: u32) {
        debug_assert!(n <= 32);
        if n == 0 {
            return;
        }
        self.acc = (self.acc << n) | (v & ((1u64 << n) - 1));
        self.nbits += n;
        // Hand finished bytes over four at a time; at most 7 bits stay behind.
        if self.nbits >= 32 {
            self.nbits -= 32;
            self.bytes.extend_from_slice(&((self.acc >> self.nbits) as u32).to_be_bytes());
            self.acc &= (1u64 << self.nbits) - 1;
        }
    }

    pub fn write_signed(&mut self, v: i64, n: u32) {
        self.write(v as u64, n);
    }

    /// `q` zero bits followed by a one bit.
    #[inline]
    pub fn write_unary(&mut self, mut q: u64) {
        while q >= 31 {
            self.write(0, 31);
            q -= 31;
        }
        self.write(1, q as u32 + 1);
    }

    /// Pad with zero bits to a byte boundary.
    pub fn align(&mut self) {
        self.write(0, (8 - self.nbits % 8) % 8);
        while self.nbits > 0 {
            self.nbits -= 8;
            self.bytes.push((self.acc >> self.nbits) as u8);
            self.acc &= (1u64 << self.nbits) - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_roundtrip() {
        let mut w = BitWriter::new();
        w.write(0b101, 3);
        w.write_signed(-5, 7);
        w.write_unary(70);
        w.write(0xABCDE, 20);
        w.write_signed(-1_000_000_000, 32);
        w.align();
        let mut r = BitReader::new(&w.bytes);
        assert_eq!(r.read(3).unwrap(), 0b101);
        assert_eq!(r.read_signed(7).unwrap(), -5);
        assert_eq!(r.read_unary().unwrap(), 70);
        assert_eq!(r.read(20).unwrap(), 0xABCDE);
        assert_eq!(r.read_signed(32).unwrap(), -1_000_000_000);
        assert!(r.read(32).is_err());
    }
}
