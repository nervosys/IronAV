//! Bit-level reading of H.264 NAL units: emulation-prevention removal and Exp-Golomb codes.

use ironav_core::{Error, Result};

/// One NAL unit's payload with the 0x000003 emulation-prevention bytes taken out.
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Splits an Annex B byte stream into NAL units (without start codes). Trailing zero bytes
/// before the next start code belong to the stream, not to the NAL unit, and are dropped.
pub fn split_annexb(d: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (n, &s) in starts.iter().enumerate() {
        let mut e = if n + 1 < starts.len() { starts[n + 1] - 3 } else { d.len() };
        while e > s && d[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&d[s..e]);
        }
    }
    out
}

pub struct BitReader<'a> {
    d: &'a [u8],
    pos: usize,
}

fn eod() -> Error {
    Error::invalid_input("H.264 data ends in the middle of a syntax element")
}

impl<'a> BitReader<'a> {
    pub fn new(d: &'a [u8]) -> Self {
        Self { d, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn bits_left(&self) -> usize {
        (self.d.len() * 8).saturating_sub(self.pos)
    }

    pub fn bit(&mut self) -> Result<u32> {
        let byte = *self.d.get(self.pos >> 3).ok_or_else(eod)?;
        let b = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Ok(b as u32)
    }

    pub fn flag(&mut self) -> Result<bool> {
        Ok(self.bit()? == 1)
    }

    pub fn bits(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        if self.bits_left() < n as usize {
            return Err(eod());
        }
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    /// The next `n` bits (at most 32) without consuming them; bits past the end read as zero.
    pub fn peek(&self, n: u32) -> u32 {
        let mut v = 0u32;
        for k in 0..n as usize {
            let p = self.pos + k;
            let b = self.d.get(p >> 3).map_or(0, |&byte| (byte >> (7 - (p & 7))) & 1);
            v = (v << 1) | b as u32;
        }
        v
    }

    pub fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    pub fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 32 {
                return Err(Error::invalid_input("H.264 Exp-Golomb code is too long"));
            }
        }
        if zeros == 32 {
            return Ok(u32::MAX);
        }
        Ok(((1u64 << zeros) - 1 + self.bits(zeros)? as u64) as u32)
    }

    pub fn se(&mut self) -> Result<i32> {
        let k = self.ue()? as i64;
        Ok(if k & 1 == 1 { ((k + 1) >> 1) as i32 } else { -((k >> 1) as i32) })
    }

    /// Truncated Exp-Golomb with range `max` (te(v)): one inverted bit when the range is 1.
    pub fn te(&mut self, max: u32) -> Result<u32> {
        if max > 1 {
            self.ue()
        } else {
            Ok(1 - self.bit()?)
        }
    }

    #[allow(dead_code)]
    pub fn byte_aligned(&self) -> bool {
        self.pos & 7 == 0
    }

    pub fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    /// Whether any payload remains before the trailing stop bit (more_rbsp_data()).
    pub fn more_rbsp_data(&self) -> bool {
        let mut last = self.d.len();
        while last > 0 && self.d[last - 1] == 0 {
            last -= 1;
        }
        if last == 0 {
            return false;
        }
        let b = self.d[last - 1];
        let stop = last * 8 - 1 - b.trailing_zeros() as usize;
        self.pos < stop
    }

    #[allow(dead_code)]
    pub fn data(&self) -> &'a [u8] {
        self.d
    }
}
